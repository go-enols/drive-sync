//! 端到端测试/基准共享基础设施。
//!
//! 设计目标：用**真实 WebSocket 客户端 + 真实中继**验证协议，而不是 mock：
//! - [`start_relay`] / [`bench_relay_config`]：在 `127.0.0.1:0` 起进程内中继（随机端口）；
//! - [`RelayClient`]：tokio-tungstenite WS 客户端，完成 Hello 认证（Ed25519 签名）；
//! - [`establish_pair`]：直接构造 `PairRecord` 后经中继转发 `session_init`/`session_ack`，
//!   用 `drive_core::session` 完成 E2E 握手（等价于客户端看到对端上线后的握手流程）；
//! - [`latency_stats`]：p50/p95/p99 等延迟统计；
//! - [`process_memory`]：进程内存采样（中继在进程内，用于近似"中继 + 客户端"占用）。
//!
//! 测试/基准可以直接使用平台 crate 的公开 API，不绕过任何认证/加密路径。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use drive_core::identity::{Identity, PublicIdentity};
use drive_core::pairing::{PairRecord, PAIRING_VERSION};
use drive_core::peer::PeerMessage;
use drive_core::session::{
    accept_session_init, complete_session, create_session_init, E2eEnvelope, Session,
    SessionAckMsg, SessionInitMsg,
};
use drive_core::util::{blake3_hex, now_ms, random_hex};
use drive_core::wire::{hello_sign_input, ClientToServer, ServerToClient, WIRE_VERSION};
use drive_core::MAX_WIRE_FRAME_BYTES;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

/// 基准/测试中继统一使用的共享 access_token（与 `RelayConfig` 一致）。
pub const DEFAULT_TOKEN: &str = "drive-e2e-token";

/// 单次期望消息的默认等待上限。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// 底层 WS 流类型（`ws://`，无 TLS feature）。
pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

// ---------------------------------------------------------------------------
// 中继启动
// ---------------------------------------------------------------------------

/// 测试/基准通用中继配置：绑定 `127.0.0.1:0`、启用可选暂存（若给目录）。
///
/// 注意：v0.2 起 `RelayConfig.data_dir` 是撤销通知离线信箱（`<data_dir>/revocations/`）
/// 的落盘位置。每个测试**必须**使用独立目录，否则会跨测试共享离线通知。本函数
/// 默认分配一个唯一的系统临时目录；需要"中继重启后通知仍存在"的测试请用
/// [`relay_config_with_data_dir`] 显式指定目录。
pub fn relay_config(vault_dir: Option<PathBuf>) -> drive_relay::RelayConfig {
    let data_dir = std::env::temp_dir().join(format!("drive-e2e-relay-state-{}", random_hex(8)));
    relay_config_with_data_dir(vault_dir, data_dir)
}

/// 与 [`relay_config`] 相同，但显式指定中继状态目录（撤销通知持久化 / 重启复用）。
pub fn relay_config_with_data_dir(
    vault_dir: Option<PathBuf>,
    data_dir: PathBuf,
) -> drive_relay::RelayConfig {
    let mut config = drive_relay::RelayConfig::new(DEFAULT_TOKEN);
    config.bind = "127.0.0.1:0".parse().expect("127.0.0.1:0 是合法监听地址");
    config.vault_dir = vault_dir;
    config.data_dir = data_dir;
    config
}

/// 基准专用中继配置：关闭每连接帧率限速，避免令牌桶排队污染延迟/吞吐。
///
/// 协议默认 200 帧/秒（突发 400）会限制**持续**发送速率，测延迟时逐消息排队会
/// 放大 p95；基准关心的是"加密 → WS → 中继转发 → WS → 解密"这条路径本身。
/// 默认配置下的限流行为由中继单测覆盖，不在此重复。
pub fn bench_relay_config() -> drive_relay::RelayConfig {
    let mut config = relay_config(None);
    config.rate_limit_per_sec = 0; // 0 = 关闭令牌桶
    config.rate_limit_burst = 0;
    config.idle_timeout_secs = 0; // 长基准期间不因空闲断开
    config.send_queue_capacity = 1024; // 无 ack 流水线时避免出站队列满丢帧
    config
}

/// 启动进程内中继；返回时监听端口已就绪。
pub async fn start_relay(config: drive_relay::RelayConfig) -> Result<drive_relay::RelayHandle> {
    drive_relay::start(config)
        .await
        .context("启动进程内中继失败")
}

/// 在**指定地址**重启中继（断线重连测试用）；状态目录使用新的临时目录。
///
/// 旧中继 `shutdown().await` 后端口可能还没完全释放，这里带 50ms 间隔重试。
/// 返回值与原中继相同的监听地址，客户端按固定 URL 自动重连。
pub async fn restart_relay_on(addr: std::net::SocketAddr) -> Result<drive_relay::RelayHandle> {
    let data_dir = std::env::temp_dir().join(format!("drive-e2e-relay-state-{}", random_hex(8)));
    restart_relay_on_with_data_dir(addr, data_dir).await
}

/// 在指定地址重启中继，并复用指定状态目录（撤销通知离线信箱持久化测试用）。
pub async fn restart_relay_on_with_data_dir(
    addr: std::net::SocketAddr,
    data_dir: PathBuf,
) -> Result<drive_relay::RelayHandle> {
    let mut last_error = None;
    for _ in 0..120 {
        let mut config = relay_config_with_data_dir(None, data_dir.clone());
        config.bind = addr;
        match drive_relay::start(config).await {
            Ok(handle) => return Ok(handle),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("在 {addr} 重启中继失败")))
}

// ---------------------------------------------------------------------------
// WebSocket 客户端
// ---------------------------------------------------------------------------

/// 一条已认证的中继连接；`identity` 保留长期密钥用于 E2E 握手签名。
pub struct RelayClient {
    pub ws: WsStream,
    pub identity: Identity,
    /// `welcome` 之前收到的消息（正常为空）。
    pub queued: Vec<ServerToClient>,
    /// 中继返回的 welcome（含 device_id / session_id / online）。
    pub welcome: ServerToClient,
}

impl RelayClient {
    /// 连接中继并完成 Hello 认证（返回时已收到 `welcome`）。
    pub async fn connect(url: &str, identity: Identity, access_token: &str) -> Result<Self> {
        let connect = tokio::time::timeout(DEFAULT_TIMEOUT, connect_async(url))
            .await
            .map_err(|_| anyhow!("连接中继超时（{url}）"))?;
        let (mut ws, _response) = connect.with_context(|| format!("连接中继失败（{url}）"))?;

        let device = identity.public();
        let nonce = random_hex(16);
        let signature = identity.sign(&hello_sign_input(&nonce, &device.device_id));
        let hello = ClientToServer::Hello {
            v: WIRE_VERSION,
            device,
            access_token: access_token.to_string(),
            nonce,
            signature,
        };
        ws.send(Message::Text(hello.to_json()?))
            .await
            .context("发送 hello 失败")?;

        let mut queued = Vec::new();
        let welcome = loop {
            match tokio::time::timeout(DEFAULT_TIMEOUT, ws.next()).await {
                Err(_) => bail!("等待中继 welcome 超时"),
                Ok(None) => bail!("中继在认证阶段关闭了连接"),
                Ok(Some(Err(error))) => bail!("认证阶段 WebSocket 错误: {error}"),
                Ok(Some(Ok(Message::Text(text)))) => {
                    let message =
                        ServerToClient::from_json(&text).context("解析认证阶段消息失败")?;
                    match message {
                        ServerToClient::Welcome { .. } => break message,
                        other => queued.push(other),
                    }
                }
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    let _ = ws.send(Message::Pong(payload)).await;
                }
                Ok(Some(Ok(Message::Close(frame)))) => bail!(
                    "中继在认证阶段关闭连接（code={:?}）",
                    frame.map(|f| u16::from(f.code))
                ),
                Ok(Some(Ok(_))) => {}
            }
        };

        Ok(Self {
            ws,
            identity,
            queued,
            welcome,
        })
    }

    pub fn device_id(&self) -> String {
        self.identity.device_id()
    }

    pub fn public(&self) -> PublicIdentity {
        self.identity.public()
    }

    /// welcome 中的在线设备列表（协议侧看到的连接数快照）。
    pub fn online(&self) -> Vec<String> {
        match &self.welcome {
            ServerToClient::Welcome { online, .. } => online.clone(),
            _ => Vec::new(),
        }
    }

    /// 发送一条客户端消息，返回实际写出的 JSON 文本字节数（用于吞吐统计）。
    pub async fn send(&mut self, message: &ClientToServer) -> Result<usize> {
        let text = message.to_json().context("序列化客户端消息失败")?;
        if text.len() > MAX_WIRE_FRAME_BYTES {
            bail!(
                "待发送帧 {} 字节超过上限 {} 字节",
                text.len(),
                MAX_WIRE_FRAME_BYTES
            );
        }
        let len = text.len();
        self.ws
            .send(Message::Text(text))
            .await
            .context("WebSocket 发送失败")?;
        Ok(len)
    }

    /// 读取下一条服务器消息（自动回复 WS Ping）；超过 `timeout` 返回错误。
    pub async fn recv(&mut self, timeout: Duration) -> Result<ServerToClient> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("等待中继消息超时（{timeout:?}）");
            }
            match tokio::time::timeout(remaining, self.ws.next()).await {
                Err(_) => bail!("等待中继消息超时（{timeout:?}）"),
                Ok(None) => bail!("中继连接已关闭"),
                Ok(Some(Err(error))) => bail!("WebSocket 连接错误: {error}"),
                Ok(Some(Ok(Message::Text(text)))) => {
                    return ServerToClient::from_json(&text).context("解析中继消息失败");
                }
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    let _ = self.ws.send(Message::Pong(payload)).await;
                }
                Ok(Some(Ok(Message::Close(frame)))) => bail!(
                    "中继关闭连接（code={:?}）",
                    frame.map(|f| u16::from(f.code))
                ),
                Ok(Some(Ok(_))) => continue,
            }
        }
    }

    /// 等待下一条 `relay` 信封；跳过 presence/welcome/pong 等无关消息。
    pub async fn recv_relay_envelope(&mut self) -> Result<E2eEnvelope> {
        loop {
            match self.recv(DEFAULT_TIMEOUT).await? {
                ServerToClient::Relay { envelope, .. } => return Ok(envelope),
                ServerToClient::Presence { .. }
                | ServerToClient::Welcome { .. }
                | ServerToClient::Pong { .. } => continue,
                ServerToClient::Error { code, message } => {
                    bail!("中继返回错误: {code}: {message}")
                }
                other => bail!("等待 E2E 信封时收到意外消息: {other:?}"),
            }
        }
    }

    /// 等待 `session_init` 转发。
    pub async fn recv_session_init(&mut self) -> Result<SessionInitMsg> {
        loop {
            match self.recv(DEFAULT_TIMEOUT).await? {
                ServerToClient::SessionInit { msg, .. } => return Ok(msg),
                ServerToClient::Presence { .. }
                | ServerToClient::Welcome { .. }
                | ServerToClient::Pong { .. } => continue,
                ServerToClient::Error { code, message } => {
                    bail!("中继返回错误: {code}: {message}")
                }
                other => bail!("等待 session_init 时收到意外消息: {other:?}"),
            }
        }
    }

    /// 等待 `session_ack` 转发。
    pub async fn recv_session_ack(&mut self) -> Result<SessionAckMsg> {
        loop {
            match self.recv(DEFAULT_TIMEOUT).await? {
                ServerToClient::SessionAck { msg, .. } => return Ok(msg),
                ServerToClient::Presence { .. }
                | ServerToClient::Welcome { .. }
                | ServerToClient::Pong { .. } => continue,
                ServerToClient::Error { code, message } => {
                    bail!("中继返回错误: {code}: {message}")
                }
                other => bail!("等待 session_ack 时收到意外消息: {other:?}"),
            }
        }
    }

    /// 关闭连接（最多等 1 秒）。
    pub async fn close(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(1), self.ws.close(None)).await;
    }
}

// ---------------------------------------------------------------------------
// 配对记录与 E2E 握手
// ---------------------------------------------------------------------------

/// 直接构造双方 `PairRecord`（模拟"配对已经完成"，跳过配对码/SAS 环节）。
///
/// 握手/转发只依赖 `PairRecord.peer` 的 Ed25519 公钥，因此基准/测试不需要走完整
/// 配对流程；真实配对流程由中继单测与后续 client-core 端到端测试覆盖。
pub fn pair_record(self_identity: &Identity, peer: &PublicIdentity, relay_url: &str) -> PairRecord {
    // 指纹按 device_id 字典序拼接，保证双方算出的值一致（真实配对里是 blake3(shared)）。
    let (first, second) = if self_identity.device_id() < peer.device_id {
        (self_identity.device_id(), peer.device_id.clone())
    } else {
        (peer.device_id.clone(), self_identity.device_id())
    };
    let fingerprint = blake3_hex(format!("{first}/{second}").as_bytes());
    let now = now_ms();
    PairRecord {
        v: PAIRING_VERSION,
        peer: peer.clone(),
        relay_url: relay_url.to_string(),
        paired_at_ms: now,
        label: peer.name.clone(),
        pairing_fingerprint: fingerprint[..16].to_string(),
        last_seen_ms: now,
    }
}

/// 生成 A/B 双方各持一份的 `PairRecord`。
pub fn pair_records(a: &Identity, b: &Identity, relay_url: &str) -> (PairRecord, PairRecord) {
    (
        pair_record(a, &b.public(), relay_url),
        pair_record(b, &a.public(), relay_url),
    )
}

/// 经中继完成一次 E2E 握手：A 发 `session_init` → B 收并 `accept` → B 回 `session_ack`
/// → A `complete`。返回双方会话（发送/接收密钥由 device_id 字典序决定方向）。
pub async fn establish_pair(
    a: &mut RelayClient,
    b: &mut RelayClient,
) -> Result<(Session, Session)> {
    let a_public = a.public();
    let b_public = b.public();
    let a_id = a_public.device_id.clone();
    let b_id = b_public.device_id.clone();

    let init = create_session_init(&a.identity, &b_public).context("创建 session_init 失败")?;
    a.send(&ClientToServer::SessionInit {
        to: b_id,
        msg: init.msg.clone(),
    })
    .await?;

    let received_init = b.recv_session_init().await?;
    let (ack, session_b) = accept_session_init(&b.identity, &received_init, &a_public)
        .context("接受 session_init 失败")?;
    b.send(&ClientToServer::SessionAck {
        to: a_id,
        msg: ack.clone(),
    })
    .await?;

    let received_ack = a.recv_session_ack().await?;
    let session_a = complete_session(&a.identity, &init, &received_ack, &b_public)
        .context("完成会话握手失败")?;
    Ok((session_a, session_b))
}

// ---------------------------------------------------------------------------
// 消息构造
// ---------------------------------------------------------------------------

/// 构造一条剪切板 `PeerMessage`（文本经 `ClipboardPayload` 规范化）。
pub fn clipboard_message(origin: &str, seq: u64, text: &str) -> Result<PeerMessage> {
    use drive_core::clipboard::{ClipboardEvent, ClipboardPayload};

    let payload = ClipboardPayload::text(text).context("构造剪切板载荷失败")?;
    Ok(PeerMessage::Clipboard {
        event: ClipboardEvent {
            v: 1,
            event_id: random_hex(8),
            origin_device: origin.to_string(),
            origin_seq: seq,
            created_at_ms: now_ms(),
            payload,
        },
    })
}

/// 生成指定长度的确定性文本（ASCII，长度=字节数）。
///
/// 用非重复模式，避免压缩/去重逻辑意外介入；1KiB 文本低于压缩阈值。
pub fn text_payload(len: usize) -> String {
    let mut out = String::with_capacity(len);
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..len {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((b'!' + (seed >> 33) as u8 % 90) as char);
    }
    out
}

/// 生成随机二进制数据（用于分块吞吐，避免 zstd/重复模式影响）。
pub fn random_data(len: usize) -> Vec<u8> {
    drive_core::util::random_bytes(len)
}

// ---------------------------------------------------------------------------
// 统计
// ---------------------------------------------------------------------------

/// 延迟统计（毫秒，nearest-rank 百分位）。
#[derive(Clone, Debug, Default)]
pub struct LatencyStats {
    pub count: usize,
    pub min_ms: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

/// 计算延迟统计（会排序输入副本）。
pub fn latency_stats(samples: &[Duration]) -> LatencyStats {
    if samples.is_empty() {
        return LatencyStats::default();
    }
    let mut sorted: Vec<Duration> = samples.to_vec();
    sorted.sort();
    let count = sorted.len();
    let percentile = |p: f64| -> f64 {
        let rank = ((p / 100.0) * count as f64).ceil() as usize;
        let index = rank.clamp(1, count) - 1;
        sorted[index].as_secs_f64() * 1000.0
    };
    let sum: f64 = sorted.iter().map(|d| d.as_secs_f64() * 1000.0).sum();
    LatencyStats {
        count,
        min_ms: sorted[0].as_secs_f64() * 1000.0,
        mean_ms: sum / count as f64,
        p50_ms: percentile(50.0),
        p95_ms: percentile(95.0),
        p99_ms: percentile(99.0),
        max_ms: sorted[count - 1].as_secs_f64() * 1000.0,
    }
}

// ---------------------------------------------------------------------------
// 进程内存采样
// ---------------------------------------------------------------------------

/// 进程内存快照（字节）。中继与基准跑在同一进程，因此这是"中继 + 客户端 + bench
/// 框架"的合计占用；报告时须注明该口径。
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessMemory {
    /// 工作集（Windows WorkingSet64 / Linux VmRSS）。
    pub working_set_bytes: Option<u64>,
    /// 私有内存（Windows PrivateMemorySize64 / Linux VmData）。
    pub private_bytes: Option<u64>,
}

impl ProcessMemory {
    pub fn working_set_mib(&self) -> Option<f64> {
        self.working_set_bytes.map(|v| v as f64 / (1024.0 * 1024.0))
    }

    pub fn private_mib(&self) -> Option<f64> {
        self.private_bytes.map(|v| v as f64 / (1024.0 * 1024.0))
    }
}

/// 采样当前进程内存；平台不支持时返回 `default()`（全 `None`）。
pub fn process_memory() -> ProcessMemory {
    #[cfg(target_os = "windows")]
    {
        match windows_process_memory() {
            Some((working_set, private)) => ProcessMemory {
                working_set_bytes: Some(working_set),
                private_bytes: Some(private),
            },
            None => ProcessMemory::default(),
        }
    }
    #[cfg(target_os = "linux")]
    {
        match linux_process_memory() {
            Some((working_set, private)) => ProcessMemory {
                working_set_bytes: Some(working_set),
                private_bytes: Some(private),
            },
            None => ProcessMemory::default(),
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        ProcessMemory::default()
    }
}

/// Windows：通过 PowerShell `Get-Process` 读取工作集与私有内存。
///
/// 不引入 `windows-sys` 依赖，避免为一次基准采样增加平台 crate；PowerShell 启动
/// 有约 0.5~1.5s 开销，因此只在阶段边界采样（不在测量窗口内）。
#[cfg(target_os = "windows")]
fn windows_process_memory() -> Option<(u64, u64)> {
    use std::process::Command;

    let pid = std::process::id();
    let script = format!(
        "$p = Get-Process -Id {pid} -ErrorAction Stop; Write-Output \"$($p.WorkingSet64) $($p.PrivateMemorySize64)\""
    );
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace();
    let working_set: u64 = parts.next()?.parse().ok()?;
    let private: u64 = parts.next()?.parse().ok()?;
    Some((working_set, private))
}

/// Linux：解析 `/proc/self/status` 的 `VmRSS` / `VmData`（kB）。
#[cfg(target_os = "linux")]
fn linux_process_memory() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let read = |key: &str| -> Option<u64> {
        let line = status.lines().find(|line| line.starts_with(key))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    };
    Some((read("VmRSS:")?, read("VmData:")?))
}

// ---------------------------------------------------------------------------
// 环境信息
// ---------------------------------------------------------------------------

/// 采集基准环境信息（OS/架构/CPU/CPU 核数/Rust 版本）。
pub fn environment_info() -> serde_json::Value {
    let rustc = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "logical_cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "rustc": rustc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        let samples: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        let stats = latency_stats(&samples);
        assert_eq!(stats.count, 100);
        assert_eq!(stats.min_ms, 1.0);
        assert_eq!(stats.max_ms, 100.0);
        assert_eq!(stats.p50_ms, 50.0);
        assert_eq!(stats.p95_ms, 95.0);
        assert_eq!(stats.p99_ms, 99.0);
        assert!((stats.mean_ms - 50.5).abs() < 1e-9);
    }

    #[test]
    fn text_payload_length_and_distinct() {
        let a = text_payload(1024);
        let mut b = text_payload(1024);
        assert_eq!(a.len(), 1024);
        assert_eq!(a, b);
        b.push('x');
        assert_ne!(a, b);
    }
}
