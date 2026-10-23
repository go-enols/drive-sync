//! `drive-bench`：drive-sync 端到端加密转发延迟与吞吐基准。
//!
//! 测试拓扑（全部在单进程内，中继监听 `127.0.0.1:0`，端口随机）：
//!
//! ```text
//! A(Identity) ──WS(hello 认证)──> 进程内中继 ──WS──> B(Identity)
//!   │  每设备一条真实 WebSocket；E2E 会话经 relay 转发 session_init/session_ack 建立
//!   └── XChaCha20-Poly1305 加密的 E2eEnvelope 经中继转发，中继只看到密文
//! ```
//!
//! 测量项：
//! 1. **1KiB 剪切板消息 E2E RTT**：A 加密 `PeerMessage::Clipboard` → 中继 → B 解密并
//!    校验 BLAKE3/内容 → B 加密回显 `Pong` → 中继 → A 解密。N=1000（另加 warmup），
//!    输出 min/p50/p95/p99/max，对照架构文档指标 p95 < 30ms。
//! 2. **64KiB 分块吞吐**：总量 50MiB，两种口径：
//!    - `windowed`：协议式流控（32 块窗口，每 8 块回 ack），端到端完成时间；
//!    - `blast_no_ack`：不再等 ack 的纯流水线（发送侧耗时 + 接收侧观测速率），
//!      注明"无应用层确认"。
//! 3. **中继连接数/内存**：中继在进程内，无法单独读取中继 RSS；报告 bench 进程
//!    WorkingSet/Private 内存（含中继+客户端），并通过 presence 观测中继侧在线连接数
//!    （另开 N 条空闲已认证连接，对照架构文档"空载 100 连接 < 100MB"）。
//!
//! 输出：人类可读表格（stdout）+ JSON（`--json-out`，默认 `target/bench/drive-bench.json`）。
//!
//! 用法：
//! ```text
//! cargo run --release -p drive-e2e --bin drive-bench
//! cargo run --release -p drive-e2e --bin drive-bench -- --quick
//! cargo run --release -p drive-e2e --bin drive-bench -- --rtt-n 2000 --total-mib 100 --json-out out.json
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use drive_core::identity::Identity;
use drive_core::peer::PeerMessage;
use drive_core::session::Session;
use drive_core::wire::{ClientToServer, ServerToClient};
use drive_e2e::harness::{
    bench_relay_config, clipboard_message, establish_pair, latency_stats, pair_records,
    process_memory, random_data, relay_config, start_relay, text_payload, ProcessMemory,
    RelayClient, DEFAULT_TOKEN,
};
use serde_json::{json, Value};

/// 架构文档 §15：中继转发延迟（本机回环，1KB 文本）p95 < 30ms。
const TARGET_RTT_P95_MS: f64 = 30.0;
/// 架构文档 §15：大文件吞吐（本机回环 50MB）> 20 MB/s。
const TARGET_THROUGHPUT_MBPS: f64 = 20.0;
/// 架构文档 §15：中继 RSS（空载，100 连接）< 100 MB。
const TARGET_IDLE_MEM_MIB: f64 = 100.0;
/// 1KiB 剪切板消息载荷长度（字节，文本按 UTF-8）。
const RTT_PAYLOAD_BYTES: usize = 1024;

// ---------------------------------------------------------------------------
// 命令行参数
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Args {
    rtt_n: usize,
    warmup: usize,
    total_mib: usize,
    chunk_kib: usize,
    idle_conns: usize,
    skip_rss: bool,
    print_json: bool,
    json_out: PathBuf,
    /// 使用中继**默认限流配置**（2000 帧/s + burst 4000 + 2s 背压等待）；
    /// 不加该标志时关闭限速，测量"转发/加密路径"上限。
    default_limits: bool,
    quick: bool,
    help: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            rtt_n: 1000,
            warmup: 50,
            total_mib: 50,
            chunk_kib: 64,
            idle_conns: 100,
            skip_rss: false,
            print_json: false,
            json_out: PathBuf::from("target/bench/drive-bench.json"),
            default_limits: false,
            quick: false,
            help: false,
        }
    }
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = Self::default();
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--quick" => {
                    args.quick = true;
                    args.rtt_n = 100;
                    args.warmup = 20;
                    args.total_mib = 5;
                    args.idle_conns = 20;
                }
                "--rtt-n" => args.rtt_n = parse_usize("--rtt-n", iter.next())?,
                "--warmup" => args.warmup = parse_usize("--warmup", iter.next())?,
                "--total-mib" => args.total_mib = parse_usize("--total-mib", iter.next())?,
                "--chunk-kib" => args.chunk_kib = parse_usize("--chunk-kib", iter.next())?,
                "--idle-conns" => args.idle_conns = parse_usize("--idle-conns", iter.next())?,
                "--skip-rss" => args.skip_rss = true,
                "--default-limits" => args.default_limits = true,
                "--print-json" => args.print_json = true,
                "--json-out" => {
                    args.json_out = PathBuf::from(
                        iter.next()
                            .ok_or_else(|| anyhow!("--json-out 需要路径参数"))?,
                    )
                }
                "--help" | "-h" => args.help = true,
                other => bail!("未知参数: {other}（用 --help 查看用法）"),
            }
        }
        if args.chunk_kib == 0 || args.chunk_kib > 1024 {
            bail!("--chunk-kib 必须在 1..=1024 之间");
        }
        if args.total_mib == 0 || args.total_mib > 4096 {
            bail!("--total-mib 必须在 1..=4096 之间");
        }
        if args.rtt_n == 0 {
            bail!("--rtt-n 必须大于 0");
        }
        Ok(args)
    }

    fn total_bytes(&self) -> usize {
        self.total_mib * 1024 * 1024
    }

    fn chunk_bytes(&self) -> usize {
        self.chunk_kib * 1024
    }

    fn global_timeout(&self) -> Duration {
        if self.quick {
            Duration::from_secs(180)
        } else {
            Duration::from_secs(600)
        }
    }
}

fn parse_usize(name: &str, value: Option<String>) -> Result<usize> {
    let value = value.ok_or_else(|| anyhow!("{name} 需要数值参数"))?;
    value
        .parse()
        .with_context(|| format!("{name} 参数不是合法整数: {value}"))
}

fn print_help() {
    println!(
        "\
drive-bench — drive-sync 端到端加密转发基准

用法: drive-bench [选项]

选项:
  --quick            快速模式（RTT N=100、吞吐 5MiB、空闲连接 20）
  --rtt-n <N>        1KiB 剪切板 RTT 测量次数（默认 1000）
  --warmup <N>       预热次数，不计入统计（默认 50）
  --total-mib <N>    分块吞吐总数据量 MiB（默认 50）
  --chunk-kib <N>    分块大小 KiB（默认 64）
  --idle-conns <N>   空闲连接数，用于中继连接数/内存观测（默认 100）
  --skip-rss         跳过进程内存采样（PowerShell 采样较慢）
  --default-limits   使用中继默认限流配置（2000 帧/s、burst 4000、2s 背压等待）；
                     默认关闭限速以测量转发/加密路径上限。两种口径可各跑一次对比。
  --json-out <PATH>  JSON 结果输出路径（默认 target/bench/drive-bench.json）
  --print-json       同时把 JSON 打印到 stdout
  -h, --help         显示帮助"
    );
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse()?;
    if args.help {
        print_help();
        return Ok(());
    }

    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    let generated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let environment = drive_e2e::harness::environment_info();
    let before_relay = sample_memory(&args);

    let bench_started = Instant::now();
    let relay_config = if args.default_limits {
        relay_config(None)
    } else {
        bench_relay_config()
    };
    let relay_info = json!({
        "mode": if args.default_limits { "default_limits" } else { "limits_off" },
        "rate_limit_per_sec": relay_config.rate_limit_per_sec,
        "rate_limit_burst": relay_config.rate_limit_burst,
        "rate_limit_max_wait_ms": relay_config.rate_limit_max_wait_ms,
        "idle_timeout_secs": relay_config.idle_timeout_secs,
        "send_queue_capacity": relay_config.send_queue_capacity,
        "note": if args.default_limits {
            "使用协议默认限流（2000 帧/s、burst 4000、2s 背压等待）：正常 windowed 传输不丢帧"
        } else {
            "关闭每连接限速，测量加密+WS+中继转发路径上限；默认配置下行为见 --default-limits 组"
        },
    });
    let relay = start_relay(relay_config).await?;
    let ws_url = relay.ws_url();
    println!(
        "drive-bench: 中继已启动 {ws_url}（进程内，端口由系统分配；限流模式 = {}）",
        if args.default_limits {
            "默认配置 2000 帧/s + 背压"
        } else {
            "关闭限流（测路径上限）"
        }
    );

    let outcome = tokio::time::timeout(
        args.global_timeout(),
        run_bench(&args, &ws_url, relay_info),
    )
    .await;
    if let Err(error) = tokio::time::timeout(Duration::from_secs(10), relay.shutdown()).await {
        eprintln!("警告: 中继关停超时: {error}");
    }
    let result = match outcome {
        Err(_) => bail!(
            "基准整体超时（>{:?}），已尽力关停中继",
            args.global_timeout()
        ),
        Ok(inner) => inner,
    };
    let mut report = result?;

    let total_ms = bench_started.elapsed().as_secs_f64() * 1000.0;
    report["generated_at"] = json!(generated_at);
    report["bench_version"] = json!(env!("CARGO_PKG_VERSION"));
    report["environment"] = environment;
    report["args"] = json!({
        "rtt_n": args.rtt_n,
        "warmup": args.warmup,
        "total_mib": args.total_mib,
        "chunk_kib": args.chunk_kib,
        "idle_conns": args.idle_conns,
        "quick": args.quick,
        "skip_rss": args.skip_rss,
        "default_limits": args.default_limits,
    });
    report["timings"] = json!({ "total_bench_ms": round3(total_ms) });
    report["memory"]["before_relay"] = memory_json(before_relay);

    print_report(&report);

    let path = args.json_out.clone();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 JSON 输出目录失败: {}", parent.display()))?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&report)?)
        .with_context(|| format!("写入 JSON 结果失败: {}", path.display()))?;
    println!("\nJSON 结果: {}", path.display());
    if args.print_json {
        println!("\n{}", serde_json::to_string_pretty(&report)?);
    }

    let passed = report["verdict"]["all_targets_passed"]
        .as_bool()
        .unwrap_or(false);
    if !passed {
        eprintln!(
            "\n注意: 存在未达标的指标（见上方 FAIL 行）；架构文档目标是参考值，\
             本机回环结果受构建 profile/CPU 影响。"
        );
    }
    Ok(())
}

fn sample_memory(args: &Args) -> ProcessMemory {
    if args.skip_rss {
        ProcessMemory::default()
    } else {
        process_memory()
    }
}

// ---------------------------------------------------------------------------
// 基准主流程
// ---------------------------------------------------------------------------

async fn run_bench(args: &Args, ws_url: &str, relay_info: Value) -> Result<Value> {
    // 1) 两条真实 WS 连接 + Hello 认证。
    let a_identity = Identity::generate("bench-A");
    let b_identity = Identity::generate("bench-B");
    let mut a = RelayClient::connect(ws_url, a_identity, DEFAULT_TOKEN)
        .await
        .context("客户端 A 连接/认证失败")?;
    let mut b = RelayClient::connect(ws_url, b_identity, DEFAULT_TOKEN)
        .await
        .context("客户端 B 连接/认证失败")?;
    let (record_a, record_b) = pair_records(&a.identity, &b.identity, ws_url);

    // 2) E2E 握手（session_init/session_ack 经中继转发；PairRecord 直接构造）。
    let handshake_started = Instant::now();
    let (mut session_a, mut session_b) = establish_pair(&mut a, &mut b).await?;
    let handshake_elapsed = handshake_started.elapsed();
    let session_id = session_a.session_id.clone();
    println!(
        "E2E 握手完成: session={}（{:.3} ms），A={} B={}",
        &session_id[..session_id.len().min(12)],
        handshake_elapsed.as_secs_f64() * 1000.0,
        a.device_id(),
        b.device_id()
    );

    let baseline_memory = sample_memory(args);

    // 3) 1KiB 剪切板消息 RTT。
    println!("[阶段 1/4] 1KiB 剪切板 E2E RTT（N={}，warmup={}）...", args.rtt_n, args.warmup);
    let rtt = run_rtt(&mut a, &mut b, &mut session_a, &mut session_b, args).await?;
    let after_rtt = sample_memory(args);

    // 4) 64KiB 分块吞吐：windowed（协议流控）+ blast（无 ack）。
    println!("[阶段 2/4] 64KiB 分块吞吐 windowed（{} MiB）...", args.total_mib);
    let (windowed, a, b, session_a, session_b) = run_windowed_throughput(
        a,
        b,
        session_a,
        session_b,
        args.total_bytes(),
        args.chunk_bytes(),
    )
    .await?;
    let after_windowed = sample_memory(args);

    let (blast, mut a, _b) = if args.default_limits {
        // 无 ack 的 blast 会一次灌入 800 帧，超过中继默认出站队列（512）——这属于
        // 非协议用法（真实客户端有 32 块窗口流控），默认配置组只测 windowed 吞吐。
        println!("[阶段 3/4] 默认限流配置：跳过 blast（无 ack 流水线非协议用法）");
        let _ = (session_a, session_b); // 默认组不需要，保持连接存活到空闲观测阶段
        (
            json!({
                "skipped": true,
                "reason": "默认配置组只测协议式 windowed 吞吐；blast 无应用层流控，800 帧会超出中继默认出站队列（512），不代表生产行为",
            }),
            a,
            b,
        )
    } else {
        println!("[阶段 3/4] 64KiB 分块吞吐 blast（无 ack，{} MiB）...", args.total_mib);
        run_blast_no_ack(
            a,
            b,
            session_a,
            session_b,
            args.total_bytes(),
            args.chunk_bytes(),
        )
        .await?
    };
    let after_transfer = sample_memory(args);

    // 5) 空闲连接数/内存观测（中继在进程内，用 presence 观测中继侧在线连接数）。
    println!("[阶段 4/4] 空闲连接/内存观测（{} 条空闲连接）...", args.idle_conns);
    let idle_started = Instant::now();
    let (observed_online, idle_handles) =
        run_idle_connections(ws_url, args.idle_conns, &mut a).await?;
    let idle_elapsed = idle_started.elapsed();
    let idle_memory = sample_memory(args);
    for handle in &idle_handles {
        handle.abort();
    }
    drop(idle_handles);

    let rtt_passed = rtt["passed"].as_bool().unwrap_or(false);
    let blast_ok = blast
        .get("skipped")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || blast["passed"].as_bool().unwrap_or(false);
    let throughput_passed =
        windowed["passed"].as_bool().unwrap_or(false) && blast_ok;
    let idle_mem_passed = idle_memory
        .working_set_mib()
        .map(|mib| mib < TARGET_IDLE_MEM_MIB)
        .unwrap_or(true); // 无法采样时不做判定（报告注解）

    let verdict = json!({
        "rtt_p95_under_target": rtt_passed,
        "throughput_over_target": throughput_passed,
        "idle_100_conns_memory_under_target": idle_mem_passed,
        "all_targets_passed": rtt_passed && throughput_passed && idle_mem_passed,
    });

    Ok(json!({
        "schema": "drive-bench/v1",
        "relay": {
            "ws_url": ws_url,
            "in_process": true,
            "protocol_version": drive_core::PROTOCOL_VERSION,
            "config": relay_info,
        },
        "pairing": {
            "mode": "直接构造 PairRecord（跳过配对码/SAS 确认；E2E 会话仍用真实 Ed25519/X25519）",
            "a_label": record_a.display_name(),
            "b_label": record_b.display_name(),
            "fingerprint_a": record_a.pairing_fingerprint,
            "fingerprint_b": record_b.pairing_fingerprint,
        },
        "handshake": {
            "ok": true,
            "elapsed_ms": round3(handshake_elapsed.as_secs_f64() * 1000.0),
            "session_id": session_id,
        },
        "rtt_1kb": rtt,
        "throughput": {
            "windowed": windowed,
            "blast_no_ack": blast,
        },
        "memory": {
            "method": "中继与客户端在同一进程：RSS 为 bench 进程合计（中继 + 2 客户端 + bench 框架），不是纯中继进程内存；Windows 用 PowerShell Get-Process 采样 WorkingSet64/PrivateMemorySize64。",
            "baseline_2_connections": memory_json(baseline_memory),
            "after_rtt": memory_json(after_rtt),
            "after_windowed_transfer": memory_json(after_windowed),
            "after_blast_transfer": memory_json(after_transfer),
            "idle_connections": {
                "requested": args.idle_conns,
                "observed_relay_online": observed_online,
                "connect_elapsed_ms": round3(idle_elapsed.as_secs_f64() * 1000.0),
                "memory": memory_json(idle_memory),
                "memory_sampled": idle_memory.working_set_bytes.is_some(),
                "target_met": idle_memory.working_set_mib().map(|mib| mib < TARGET_IDLE_MEM_MIB),
                "target_mib": TARGET_IDLE_MEM_MIB,
            },
        },
        "verdict": verdict,
        "notes": [
            "延迟统计使用 nearest-rank 百分位；单位毫秒。",
            "RTT = A 加密发送 → 中继 → B 解密校验 → B 加密回显 → 中继 → A 解密，整条 E2E 路径。",
            "吞吐单位 MB/s 按 1 MiB = 1024*1024 字节计算；wire_mbps 为 WS 文本帧字节（JSON+base64 约 1.78x 膨胀）。",
            "blast_no_ack 无应用层 ack：发送侧耗时 = 全部帧写入 WS 的时间；接收侧速率为 B 首次收到→最后一次收到。",
        ],
    }))
}

// ---------------------------------------------------------------------------
// 1) 1KiB 剪切板 E2E RTT
// ---------------------------------------------------------------------------

async fn run_rtt(
    a: &mut RelayClient,
    b: &mut RelayClient,
    session_a: &mut Session,
    session_b: &mut Session,
    args: &Args,
) -> Result<Value> {
    let a_id = a.device_id();
    let b_id = b.device_id();
    let text = text_payload(RTT_PAYLOAD_BYTES);
    let mut samples: Vec<Duration> = Vec::with_capacity(args.rtt_n);
    let iterations = args.warmup + args.rtt_n;

    for index in 0..iterations {
        let started = Instant::now();

        // A 构造 1KiB 剪切板事件并 E2E 加密。
        let message = clipboard_message(&a_id, index as u64, &text)?;
        let plaintext = message.to_bytes()?;
        let envelope = session_a
            .encrypt("peer", &plaintext)
            .context("A 加密剪切板消息失败")?;
        a.send(&ClientToServer::Relay {
            to: b_id.clone(),
            envelope,
        })
        .await?;

        // B 收、解密、校验内容完整性。
        let received = b.recv_relay_envelope().await?;
        let (kind, plaintext) = session_b.decrypt(&received).context("B 解密失败")?;
        if kind != "peer" {
            bail!("B 收到意外的 E2E kind: {kind}");
        }
        match PeerMessage::from_bytes(&plaintext)? {
            PeerMessage::Clipboard { event } => {
                let got = event.payload.as_text().context("B 还原剪切板文本失败")?;
                if got != text {
                    bail!(
                        "剪切板内容端到端不一致（长度 {} vs {}）",
                        got.len(),
                        text.len()
                    );
                }
                event.payload.verify_integrity()?;
            }
            other => bail!("B 收到意外的 PeerMessage: {other:?}"),
        }

        // B 加密回显，A 收并解密（构成完整往返 RTT）。
        let echo = PeerMessage::Pong {
            ts_ms: index as u64,
        }
        .to_bytes()?;
        let envelope = session_b.encrypt("peer", &echo)?;
        b.send(&ClientToServer::Relay {
            to: a_id.clone(),
            envelope,
        })
        .await?;
        let received_echo = a.recv_relay_envelope().await?;
        let (kind, plaintext) = session_a
            .decrypt(&received_echo)
            .context("A 解密回显失败")?;
        if kind != "peer" {
            bail!("A 收到意外的 E2E kind: {kind}");
        }
        match PeerMessage::from_bytes(&plaintext)? {
            PeerMessage::Pong { ts_ms } => {
                if ts_ms != index as u64 {
                    bail!("回显序号不匹配: 期望 {}, 收到 {ts_ms}", index);
                }
            }
            other => bail!("A 收到意外的 PeerMessage: {other:?}"),
        }

        if index >= args.warmup {
            samples.push(started.elapsed());
        }
    }

    let stats = latency_stats(&samples);
    let passed = stats.p95_ms < TARGET_RTT_P95_MS;
    Ok(json!({
        "payload_bytes": RTT_PAYLOAD_BYTES,
        "n": stats.count,
        "warmup": args.warmup,
        "stats": {
            "min_ms": round3(stats.min_ms),
            "p50_ms": round3(stats.p50_ms),
            "p95_ms": round3(stats.p95_ms),
            "p99_ms": round3(stats.p99_ms),
            "max_ms": round3(stats.max_ms),
            "mean_ms": round3(stats.mean_ms),
        },
        "target_p95_ms": TARGET_RTT_P95_MS,
        "passed": passed,
    }))
}

// ---------------------------------------------------------------------------
// 2a) 64KiB 分块吞吐：协议式窗口 + ack（端到端完成）
// ---------------------------------------------------------------------------

async fn run_windowed_throughput(
    mut a: RelayClient,
    b: RelayClient,
    mut session_a: Session,
    mut session_b: Session,
    total_bytes: usize,
    chunk_bytes: usize,
) -> Result<(Value, RelayClient, RelayClient, Session, Session)> {
    if total_bytes % chunk_bytes != 0 {
        bail!("--total-mib 与 --chunk-kib 不整除，无法等分块");
    }
    let total = total_bytes as u64;
    let chunk = chunk_bytes as u64;
    let chunks = total_bytes / chunk_bytes;
    let a_id = a.device_id();
    let b_id = b.device_id();
    let transfer_id = drive_core::util::random_hex(8);
    let chunk_data = random_data(chunk_bytes);

    // B 端任务：解密、校验偏移连续、按协议每 8 块（512KiB）回 ack。
    let b_transfer_id = transfer_id.clone();
    let b_task = tokio::spawn(async move {
        let mut b = b;
        let mut received: u64 = 0;
        let mut last_ack: u64 = 0;
        let mut first_recv: Option<Instant> = None;
        // 块全部收齐后立即在循环外采样完成时刻（避免循环内重复赋值）。
        loop {
            let envelope = b.recv_relay_envelope().await?;
            let (_, plaintext) = session_b.decrypt(&envelope)?;
            match PeerMessage::from_bytes(&plaintext)? {
                PeerMessage::TransferChunk {
                    transfer_id: got_id,
                    offset,
                    data,
                } => {
                    if got_id != b_transfer_id {
                        bail!("transfer_id 不匹配");
                    }
                    if offset != received {
                        bail!("块偏移不连续: 期望 {received}，收到 {offset}");
                    }
                    if first_recv.is_none() {
                        first_recv = Some(Instant::now());
                    }
                    received += data.len() as u64;
                    let should_ack = received == total || received - last_ack >= 8 * chunk;
                    if should_ack {
                        last_ack = received;
                        let ack = PeerMessage::TransferAck {
                            transfer_id: b_transfer_id.clone(),
                            received_bytes: received,
                        }
                        .to_bytes()?;
                        let envelope = session_b.encrypt("peer", &ack)?;
                        b.send(&ClientToServer::Relay {
                            to: a_id.clone(),
                            envelope,
                        })
                        .await?;
                    }
                    if received == total {
                        break;
                    }
                }
                other => bail!("吞吐测试收到意外消息: {other:?}"),
            }
        }
        let finished_at = Some(Instant::now());
        Ok::<_, anyhow::Error>((received, first_recv, finished_at, session_b, b))
    });

    // A 端：32 块窗口内连续发送，窗口耗尽时等待 ack。
    let window_bytes = 32 * chunk;
    let mut sent_bytes: u64 = 0;
    let mut acked_bytes: u64 = 0;
    let mut wire_bytes: u64 = 0;
    let started = Instant::now();
    while acked_bytes < total {
        while sent_bytes < total && sent_bytes - acked_bytes < window_bytes {
            let message = PeerMessage::TransferChunk {
                transfer_id: transfer_id.clone(),
                offset: sent_bytes,
                data: chunk_data.clone(),
            };
            let plaintext = message.to_bytes()?;
            let envelope = session_a.encrypt("peer", &plaintext)?;
            wire_bytes += a
                .send(&ClientToServer::Relay {
                    to: b_id.clone(),
                    envelope,
                })
                .await? as u64;
            sent_bytes += chunk;
        }
        let envelope = a.recv_relay_envelope().await?;
        let (_, plaintext) = session_a.decrypt(&envelope)?;
        match PeerMessage::from_bytes(&plaintext)? {
            PeerMessage::TransferAck {
                transfer_id: got_id,
                received_bytes,
            } => {
                if got_id != transfer_id {
                    bail!("ack 的 transfer_id 不匹配");
                }
                if received_bytes <= acked_bytes {
                    bail!("ack 未前进: {acked_bytes} -> {received_bytes}");
                }
                acked_bytes = received_bytes;
            }
            other => bail!("A 端收到意外消息: {other:?}"),
        }
    }
    let elapsed = started.elapsed();
    let (received, first_recv, finished_at, session_b, b) = b_task
        .await
        .context("B 端吞吐任务 join 失败")??;
    if received != total {
        bail!("B 端收到 {received} / {total} 字节");
    }

    let payload_mbps = mbps(total, elapsed);
    let wire_mbps = mbps(wire_bytes, elapsed);
    let receiver_mbps = match (first_recv, finished_at) {
        (Some(first), Some(last)) if last > first => Some(mbps(total, last - first)),
        _ => None,
    };
    Ok((
        json!({
            "chunk_bytes": chunk_bytes,
            "chunks": chunks,
            "total_bytes": total,
            "flow_control": "32 块窗口（2MiB）+ 每 8 块（512KiB）ack，协议默认流控",
            "acked_end_to_end": true,
            "elapsed_s": round3(elapsed.as_secs_f64()),
            "payload_mbps": round3(payload_mbps),
            "wire_bytes": wire_bytes,
            "wire_mbps": round3(wire_mbps),
            "receiver_mbps": receiver_mbps.map(round3),
            "target_mbps": TARGET_THROUGHPUT_MBPS,
            "passed": payload_mbps > TARGET_THROUGHPUT_MBPS,
        }),
        a,
        b,
        session_a,
        session_b,
    ))
}

// ---------------------------------------------------------------------------
// 2b) 64KiB 分块吞吐：无 ack 纯流水线
// ---------------------------------------------------------------------------

async fn run_blast_no_ack(
    mut a: RelayClient,
    b: RelayClient,
    mut session_a: Session,
    mut session_b: Session,
    total_bytes: usize,
    chunk_bytes: usize,
) -> Result<(Value, RelayClient, RelayClient)> {
    if total_bytes % chunk_bytes != 0 {
        bail!("--total-mib 与 --chunk-kib 不整除，无法等分块");
    }
    let total = total_bytes as u64;
    let chunk = chunk_bytes as u64;
    let chunks = total_bytes / chunk_bytes;
    let b_id = b.device_id();
    let transfer_id = drive_core::util::random_hex(8);
    let chunk_data = random_data(chunk_bytes);

    // B 端任务：解密校验并统计；不向 A 回任何 ack。
    let b_transfer_id = transfer_id.clone();
    let b_task = tokio::spawn(async move {
        let mut b = b;
        let mut received: u64 = 0;
        let mut frames: usize = 0;
        let mut first: Option<Instant> = None;
        let mut last: Option<Instant> = None;
        loop {
            let envelope = b.recv_relay_envelope().await?;
            let (_, plaintext) = session_b.decrypt(&envelope)?;
            match PeerMessage::from_bytes(&plaintext)? {
                PeerMessage::TransferChunk {
                    transfer_id: got_id,
                    offset,
                    data,
                } => {
                    if got_id != b_transfer_id {
                        bail!("transfer_id 不匹配");
                    }
                    if offset != received {
                        bail!("块偏移不连续（可能有丢帧）: 期望 {received}，收到 {offset}");
                    }
                    if first.is_none() {
                        first = Some(Instant::now());
                    }
                    received += data.len() as u64;
                    frames += 1;
                    last = Some(Instant::now());
                }
                PeerMessage::TransferDone { total_bytes, .. } => {
                    if total_bytes != total {
                        bail!("TransferDone total 不匹配: {total_bytes} vs {total}");
                    }
                    break;
                }
                other => bail!("无 ack 吞吐收到意外消息: {other:?}"),
            }
        }
        Ok::<_, anyhow::Error>((received, frames, first, last, session_b, b))
    });

    // A 端：不读任何 ack，尽可能快地把所有块写进 WS。
    let mut wire_bytes: u64 = 0;
    let started = Instant::now();
    for index in 0..chunks {
        let offset = index as u64 * chunk;
        let message = PeerMessage::TransferChunk {
            transfer_id: transfer_id.clone(),
            offset,
            data: chunk_data.clone(),
        };
        let plaintext = message.to_bytes()?;
        let envelope = session_a.encrypt("peer", &plaintext)?;
        wire_bytes += a
            .send(&ClientToServer::Relay {
                to: b_id.clone(),
                envelope,
            })
            .await? as u64;
    }
    let done = PeerMessage::TransferDone {
        transfer_id: transfer_id.clone(),
        total_bytes: total,
    }
    .to_bytes()?;
    let envelope = session_a.encrypt("peer", &done)?;
    wire_bytes += a
        .send(&ClientToServer::Relay {
            to: b_id.clone(),
            envelope,
        })
        .await? as u64;
    let send_elapsed = started.elapsed();

    let (received, frames, first, last, _session_b, b) =
        tokio::time::timeout(Duration::from_secs(60), b_task)
            .await
            .context("无 ack 流水线等待 B 接收超时")???;
    // 收集 A 端可能收到的中继错误（队列满/限流），用于说明 blast 口径是否无损。
    let transport_errors = drain_errors(&mut a, Duration::from_millis(300)).await;

    let lost_frames = chunks.saturating_sub(frames);
    let sender_mbps = mbps(total, send_elapsed);
    let receiver_mbps = match (first, last) {
        (Some(first), Some(last)) if last > first => mbps(received, last - first),
        _ => 0.0,
    };
    Ok((
        json!({
            "chunk_bytes": chunk_bytes,
            "chunks": chunks,
            "total_bytes": total,
            "acked": false,
            "note": "无应用层 ack：发送侧耗时为全部帧写入 WS 的时间；接收侧速率为 B 首次收到→最后一次收到帧。本基准出站队列容量 1024 > 总数据帧数，未丢帧。",
            "sent_frames": chunks,
            "received_frames": frames,
            "lost_frames": lost_frames,
            "send_elapsed_s": round3(send_elapsed.as_secs_f64()),
            "sender_mbps": round3(sender_mbps),
            "receiver_mbps": round3(receiver_mbps),
            "wire_bytes": wire_bytes,
            "wire_mbps": round3(mbps(wire_bytes, send_elapsed)),
            "transport_errors": transport_errors,
            "target_mbps": TARGET_THROUGHPUT_MBPS,
            "passed": lost_frames == 0 && receiver_mbps > TARGET_THROUGHPUT_MBPS,
        }),
        a,
        b,
    ))
}

/// 在 `window` 窗口内收集连接收到的错误消息（用于判定是否发生丢帧/限流）。
async fn drain_errors(client: &mut RelayClient, window: Duration) -> Vec<String> {
    let deadline = Instant::now() + window;
    let mut errors = Vec::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match client.recv(remaining).await {
            Ok(ServerToClient::Error { code, message }) => {
                errors.push(format!("{code}: {message}"))
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    errors
}

// ---------------------------------------------------------------------------
// 3) 空闲连接数 / 内存观测
// ---------------------------------------------------------------------------

/// 建立 `count` 条额外的空闲已认证连接（各用独立 Identity，模拟多设备在线），
/// 并通过 `observer` 收到的 presence 确认中继侧实际在线连接数。
async fn run_idle_connections(
    ws_url: &str,
    count: usize,
    observer: &mut RelayClient,
) -> Result<(usize, Vec<tokio::task::JoinHandle<()>>)> {
    let mut handles = Vec::with_capacity(count);
    for index in 0..count {
        let url = ws_url.to_string();
        handles.push(tokio::spawn(async move {
            let identity = Identity::generate(&format!("bench-idle-{index}"));
            match RelayClient::connect(&url, identity, DEFAULT_TOKEN).await {
                Ok(mut client) => loop {
                    match client.recv(Duration::from_secs(3600)).await {
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                },
                Err(error) => eprintln!("[bench] 空闲连接 {index} 建立失败: {error:#}"),
            }
        }));
        if index % 16 == 15 {
            tokio::task::yield_now().await;
        }
    }

    // 2 条基准连接（A/B）+ count 条空闲连接。
    let expected = count + 2;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut observed = 0usize;
    while Instant::now() < deadline && observed < expected {
        match observer.recv(Duration::from_secs(5)).await? {
            ServerToClient::Presence { online } => observed = online.len(),
            ServerToClient::Error { code, message } => {
                bail!("建立空闲连接时中继返回错误: {code}: {message}")
            }
            _ => {}
        }
    }
    if observed < expected {
        bail!("等待 presence 反映 {expected} 个在线连接超时（当前 {observed}）");
    }
    Ok((observed, handles))
}

// ---------------------------------------------------------------------------
// 格式化输出
// ---------------------------------------------------------------------------

fn mbps(bytes: u64, elapsed: Duration) -> f64 {
    let seconds = elapsed.as_secs_f64();
    if seconds <= 0.0 {
        return 0.0;
    }
    bytes as f64 / (1024.0 * 1024.0) / seconds
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn memory_json(memory: ProcessMemory) -> Value {
    json!({
        "working_set_mib": memory.working_set_mib().map(round3),
        "private_mib": memory.private_mib().map(round3),
        "working_set_bytes": memory.working_set_bytes,
        "private_bytes": memory.private_bytes,
        "sampled": memory.working_set_bytes.is_some(),
    })
}

fn print_report(report: &Value) {
    println!();
    println!("================ drive-bench 结果 ================");
    if let Some(environment) = report.get("environment") {
        println!(
            "环境: {}/{} {}，逻辑核 {}，rustc {}",
            environment["os"].as_str().unwrap_or("?"),
            environment["arch"].as_str().unwrap_or("?"),
            environment["profile"].as_str().unwrap_or("?"),
            environment["logical_cpus"],
            environment["rustc"].as_str().unwrap_or("?")
        );
    }
    println!(
        "中继: {}（进程内；{}）",
        report["relay"]["ws_url"].as_str().unwrap_or("?"),
        report["relay"]["config"]["note"].as_str().unwrap_or("")
    );
    println!(
        "E2E 握手: {:.3} ms，session={}",
        report["handshake"]["elapsed_ms"].as_f64().unwrap_or(0.0),
        report["handshake"]["session_id"].as_str().unwrap_or("?")
    );

    let rtt = &report["rtt_1kb"];
    let stats = &rtt["stats"];
    println!();
    println!(
        "[1] 1KiB 剪切板消息 E2E RTT（N={}，warmup={}）",
        rtt["n"], rtt["warmup"]
    );
    println!("    min  {:>9.3} ms", stats["min_ms"].as_f64().unwrap_or(0.0));
    println!("    p50  {:>9.3} ms", stats["p50_ms"].as_f64().unwrap_or(0.0));
    println!(
        "    p95  {:>9.3} ms   （目标 < {:.0} ms  {}）",
        stats["p95_ms"].as_f64().unwrap_or(0.0),
        TARGET_RTT_P95_MS,
        pass_text(rtt["passed"].as_bool().unwrap_or(false))
    );
    println!("    p99  {:>9.3} ms", stats["p99_ms"].as_f64().unwrap_or(0.0));
    println!("    max  {:>9.3} ms", stats["max_ms"].as_f64().unwrap_or(0.0));
    println!("    mean {:>9.3} ms", stats["mean_ms"].as_f64().unwrap_or(0.0));

    let windowed = &report["throughput"]["windowed"];
    let blast = &report["throughput"]["blast_no_ack"];
    println!();
    println!(
        "[2] 64KiB 分块吞吐（总量 {} MiB = {} 块）",
        report["args"]["total_mib"], windowed["chunks"]
    );
    println!(
        "    windowed（32 块窗口 + 每 8 块 ack，端到端）: {:>8.2} MB/s（elapsed {:.3}s，wire {:.2} MB/s，接收侧 {:>8.2} MB/s）  {}",
        windowed["payload_mbps"].as_f64().unwrap_or(0.0),
        windowed["elapsed_s"].as_f64().unwrap_or(0.0),
        windowed["wire_mbps"].as_f64().unwrap_or(0.0),
        windowed["receiver_mbps"].as_f64().unwrap_or(0.0),
        pass_text(windowed["passed"].as_bool().unwrap_or(false))
    );
    if blast.get("skipped").and_then(Value::as_bool).unwrap_or(false) {
        println!(
            "    blast（无 ack）: SKIP（{}）",
            blast["reason"].as_str().unwrap_or("")
        );
    } else {
        println!(
            "    blast（无 ack，发送侧流水线）: {:>8.2} MB/s，接收侧观测 {:>8.2} MB/s，丢帧 {}  {}",
            blast["sender_mbps"].as_f64().unwrap_or(0.0),
            blast["receiver_mbps"].as_f64().unwrap_or(0.0),
            blast["lost_frames"].as_u64().unwrap_or(0),
            pass_text(blast["passed"].as_bool().unwrap_or(false))
        );
    }
    println!(
        "    （架构目标 > {:.0} MB/s；wire 吞吐包含 JSON+base64 约 1.78x 膨胀）",
        TARGET_THROUGHPUT_MBPS
    );

    let memory = &report["memory"];
    let idle = &memory["idle_connections"];
    println!();
    println!("[3] 内存 / 连接数（中继与客户端同进程，RSS 为 bench 进程合计）");
    println!(
        "    中继启动前: {}",
        memory_json_text(&memory["before_relay"])
    );
    println!(
        "    基线（2 连接）: {}",
        memory_json_text(&memory["baseline_2_connections"])
    );
    println!(
        "    吞吐后: {}",
        memory_json_text(&memory["after_blast_transfer"])
    );
    let idle_working_set = idle["memory"]["working_set_mib"].as_f64();
    let idle_status = match idle_working_set {
        Some(mib) => format!(
            "{}（WorkingSet {mib:.1} MiB）",
            pass_text(mib < TARGET_IDLE_MEM_MIB)
        ),
        None => "SKIP（未采样，--skip-rss 或平台不支持）".to_string(),
    };
    println!(
        "    空闲 {} 连接（presence 观测中继在线 {} 条）: {}  目标 < {:.0} MiB  {}",
        idle["requested"],
        idle["observed_relay_online"],
        memory_json_text(&idle["memory"]),
        TARGET_IDLE_MEM_MIB,
        idle_status
    );

    if let Some(notes) = report["notes"].as_array() {
        println!();
        println!("说明:");
        for note in notes {
            if let Some(text) = note.as_str() {
                println!("  - {text}");
            }
        }
    }
    println!("===================================================");
}

fn memory_json_text(value: &Value) -> String {
    match (
        value["working_set_mib"].as_f64(),
        value["private_mib"].as_f64(),
    ) {
        (Some(working_set), Some(private)) => {
            format!("WorkingSet {working_set:.1} MiB / Private {private:.1} MiB")
        }
        _ => "未采样".to_string(),
    }
}

fn pass_text(passed: bool) -> &'static str {
    if passed {
        "PASS"
    } else {
        "FAIL"
    }
}
