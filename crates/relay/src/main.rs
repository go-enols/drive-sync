//! drive-relay 可执行程序：解析参数、初始化日志、启动中继并等待退出信号。

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;
use drive_relay::{start, RelayConfig};

#[derive(Debug, Parser)]
#[command(
    name = "drive-relay",
    version,
    about = "drive-sync 中继服务器：只做连接认证/配对路由/密文转发（可选 30 天加密暂存）"
)]
struct Args {
    /// 监听地址，例如 0.0.0.0:8787（生产建议前置 Caddy/Nginx 终止 TLS）
    #[arg(long, default_value = "0.0.0.0:8787")]
    bind: String,

    /// 共享访问令牌（也可用 --token-file 或环境变量 DRIVE_RELAY_TOKEN）
    #[arg(long)]
    token: Option<String>,

    /// 从文件读取共享访问令牌（首尾空白会被去除）
    #[arg(long)]
    token_file: Option<PathBuf>,

    /// 加密暂存目录；缺省不启用暂存
    #[arg(long)]
    vault_dir: Option<PathBuf>,

    /// 暂存每设备总配额（MiB）
    #[arg(long, default_value_t = 1024)]
    vault_quota_mib: u64,

    /// 暂存单对象上限（MiB）
    #[arg(long, default_value_t = 8)]
    vault_max_object_mib: u64,

    /// 暂存每设备对象数上限
    #[arg(long, default_value_t = 1000)]
    vault_max_objects: usize,

    /// 每连接入帧速率上限（帧/秒，0 表示关闭限速；默认 2000 ≈ 64KiB 块 125 MiB/s）
    #[arg(long, default_value_t = 2000)]
    rate_limit: u32,

    /// 令牌桶突发上限
    #[arg(long, default_value_t = 4000)]
    rate_burst: u32,

    /// 令牌不足时的单帧最长背压等待（毫秒；0 表示不等待、超限立即丢弃）
    #[arg(long, default_value_t = 2000)]
    rate_limit_max_wait_ms: u64,

    /// 空闲超时秒数（0 表示关闭）
    #[arg(long, default_value_t = 90)]
    idle_timeout: u64,

    /// 配对登记 TTL 秒（协议默认 120）
    #[arg(long, default_value_t = 120)]
    pair_ttl: u64,

    /// 允许同一设备多连接（默认 false：新连接踢掉旧连接）
    #[arg(long, default_value_t = false)]
    allow_multi_conn: bool,
}

fn resolve_token(args: &Args) -> Result<String> {
    if let Some(token) = args.token.as_deref() {
        let token = token.trim();
        if token.is_empty() {
            bail!("--token 不能为空");
        }
        return Ok(token.to_string());
    }
    if let Some(path) = &args.token_file {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("读取令牌文件失败: {}", path.display()))?;
        let token = raw.trim();
        if token.is_empty() {
            bail!("令牌文件内容为空: {}", path.display());
        }
        return Ok(token.to_string());
    }
    if let Ok(token) = std::env::var("DRIVE_RELAY_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(token.to_string());
        }
    }
    bail!("必须提供共享访问令牌：--token、--token-file 或环境变量 DRIVE_RELAY_TOKEN")
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    let bind: SocketAddr = args
        .bind
        .parse()
        .with_context(|| format!("监听地址非法: {}", args.bind))?;
    let token = resolve_token(&args)?;
    if token.len() < 16 {
        tracing::warn!("access_token 长度较短（建议至少 16 字符的随机串）");
    }

    let config = RelayConfig {
        bind,
        access_token: token,
        vault_dir: args.vault_dir.clone(),
        allow_multi_conn: args.allow_multi_conn,
        pair_ttl_secs: args.pair_ttl.max(1),
        rate_limit_per_sec: args.rate_limit,
        rate_limit_burst: args.rate_burst,
        rate_limit_max_wait_ms: args.rate_limit_max_wait_ms,
        idle_timeout_secs: args.idle_timeout,
        vault_quota_bytes: args.vault_quota_mib.saturating_mul(1024 * 1024),
        vault_max_object_bytes: args.vault_max_object_mib.saturating_mul(1024 * 1024),
        vault_max_objects: args.vault_max_objects,
        ..RelayConfig::default()
    };

    let handle = start(config).await?;
    tracing::info!(
        "drive-relay 已启动：ws://{}/ws（健康检查 http://{}/healthz，暂存：{}）",
        handle.addr,
        handle.addr,
        if args.vault_dir.is_some() {
            "启用"
        } else {
            "禁用"
        }
    );

    wait_for_shutdown_signal().await;
    tracing::info!("收到退出信号，正在关闭中继服务...");
    handle.shutdown().await;
    tracing::info!("drive-relay 已停止");
    Ok(())
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = terminate.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
