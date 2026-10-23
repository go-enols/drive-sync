//! drive-sync 中继服务器（库）。
//!
//! 实现 `docs/protocol.md` §4 / §5 / §11：
//! - `GET /ws`：WebSocket；首帧 `hello` 认证（共享 access_token 常量时间比较 + Ed25519 签名 +
//!   `PublicIdentity::is_consistent`），同 `device_id` 重连踢旧连接；
//! - `GET /healthz`：返回 `200 ok`；
//! - presence 广播（连接/断开）、配对邀请路由（`pair_announce`/`pair_join`，TTL/一次性/失败限流）；
//! - E2E 握手与密文信封转发（`session_init`/`session_ack`/`relay`），发往离线设备不缓存；
//! - 可选加密暂存（只存客户端密文；TTL 强约束 60s..30d；文件 + 索引落盘；后台清理）；
//! - 限额：单帧 8 MiB、空闲 90s 断开、每连接令牌桶默认 2000 帧/秒（突发 4000）。
//!   令牌不足时先背压等待（读循环暂停 → TCP 窗口自然限速发送端，不丢帧），
//!   只有等待超过 `rate_limit_max_wait_ms`（默认 2000ms）才丢弃该帧并回 `rate_limited`。
//!
//! 中继不持久化任何转发内容、不持有内容密钥；日志不打印 token / ciphertext / 文件名。
//!
//! # 可测试入口
//!
//! ```text
//! let mut config = RelayConfig::new("shared-token");
//! config.bind = "127.0.0.1:0".parse()?;
//! let handle = drive_relay::start(config).await?;
//! println!("ws://{}/ws", handle.addr);
//! handle.shutdown().await;
//! ```

#![forbid(unsafe_code)]

mod auth;
mod conn;
mod pair;
mod ratelimit;
mod state;
mod vault;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::{routing::get, Router};
use drive_core::{DEFAULT_RELAY_PORT, MAX_WIRE_FRAME_BYTES};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use state::RelayState;
use vault::{VaultLimits, VaultStore};

/// 中继运行配置。`RelayConfig::new(token)` 会填入协议默认值，测试可改绑 `127.0.0.1:0`。
#[derive(Clone, Debug)]
pub struct RelayConfig {
    /// 监听地址；测试用 `127.0.0.1:0` 让系统分配端口。
    pub bind: SocketAddr,
    /// 自建服务器共享令牌（必填；空 token 拒绝启动）。
    pub access_token: String,
    /// 可选加密暂存目录；`None` 表示禁用暂存。
    pub vault_dir: Option<PathBuf>,
    /// `true` 时同一设备的多连接会被拒绝；默认 `false`（新连接踢掉旧连接）。
    pub allow_multi_conn: bool,
    /// 配对登记最大 TTL（协议默认 120s）。
    pub pair_ttl_secs: u64,
    /// 配对登记后台清理间隔（秒）。
    pub pair_cleanup_interval_secs: u64,
    /// 每设备每分钟 join 失败上限；`0` 表示关闭限流。
    pub pair_join_limit_per_min: u32,
    /// 单 WS 帧上限（协议默认 8 MiB）。
    pub max_frame_bytes: usize,
    /// 空闲超时（秒，无任何入帧即断开）；`0` 表示关闭。
    pub idle_timeout_secs: u64,
    /// hello 认证超时（秒）。
    pub handshake_timeout_secs: u64,
    /// 每连接发送队列长度（出站消息缓冲）。
    pub send_queue_capacity: usize,
    /// 每连接入帧速率上限（帧/秒）；`0` 表示关闭限速。
    ///
    /// 默认 2000：按 64 KiB 块折算 ≈ 125 MiB/s，满足架构「本机回环 50MB > 20MB/s」
    /// 的大文件吞吐目标（协议 §4.5 原基线 200 帧/s 仅约 12.5 MiB/s）。
    pub rate_limit_per_sec: u32,
    /// 令牌桶突发上限（默认 4000 帧，约覆盖 2 秒的帧流量）。
    pub rate_limit_burst: u32,
    /// 令牌不足时单帧最长背压等待（毫秒，默认 2000）；`0` 表示不等待、超限立即丢弃。
    /// 等待期间只暂停本连接的读循环（对发送端形成 TCP 背压），不影响其他连接。
    pub rate_limit_max_wait_ms: u64,
    /// 暂存单对象密文上限（字节）。
    pub vault_max_object_bytes: u64,
    /// 暂存每设备总量上限（字节）。
    pub vault_quota_bytes: u64,
    /// 暂存每设备对象数上限。
    pub vault_max_objects: usize,
    /// 暂存过期清理间隔（秒，协议要求 60s）。
    pub vault_reap_interval_secs: u64,
}

impl RelayConfig {
    /// 以共享令牌创建默认配置。
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            ..Self::default()
        }
    }
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], DEFAULT_RELAY_PORT)),
            access_token: String::new(),
            vault_dir: None,
            allow_multi_conn: false,
            pair_ttl_secs: 120,
            pair_cleanup_interval_secs: 30,
            pair_join_limit_per_min: 5,
            max_frame_bytes: MAX_WIRE_FRAME_BYTES,
            idle_timeout_secs: 90,
            handshake_timeout_secs: 10,
            send_queue_capacity: 512,
            rate_limit_per_sec: 2000,
            rate_limit_burst: 4000,
            rate_limit_max_wait_ms: 2000,
            vault_max_object_bytes: MAX_WIRE_FRAME_BYTES as u64,
            vault_quota_bytes: 1024 * 1024 * 1024,
            vault_max_objects: 1000,
            vault_reap_interval_secs: 60,
        }
    }
}

/// 已启动中继的句柄：实际监听地址 + 关停信号 + join handle。
pub struct RelayHandle {
    /// 实际监听地址（支持 `127.0.0.1:0` 自动分配端口）。
    pub addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
    background: Vec<JoinHandle<()>>,
    /// 用于优雅停机时通知已建立的 WS 连接主动关闭（Close 1001）。
    state: Option<Arc<RelayState>>,
}

impl RelayHandle {
    /// `/ws` 的完整 URL（便于测试/客户端直接使用）。
    pub fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    /// 取出 axum 关停信号发送端（用于把关停能力交给其他任务）。
    ///
    /// 注意：只发送该信号会停止接受新连接，但**不会关闭已建立的 WS 连接**；
    /// 需要完整优雅停机（关闭在线连接并等待清理）请调用 [`RelayHandle::shutdown`]。
    pub fn shutdown_signal(&mut self) -> Option<oneshot::Sender<()>> {
        self.shutdown.take()
    }

    /// 取出服务主任务的 join handle。
    pub fn join_handle(&mut self) -> Option<&mut JoinHandle<()>> {
        self.join.as_mut()
    }

    /// 优雅关停：
    /// 1. 通知 axum 停止接受新连接；
    /// 2. 向所有已建立的 WS 连接广播 Close(1001 going away)，最多等待 1s 让其注销；
    /// 3. 等待服务主任务退出，并中止后台清理任务。
    ///
    /// 返回时在线连接已收到关闭帧（或已断开），客户端可据此感知断线并重连。
    pub async fn shutdown(mut self) {
        if let Some(signal) = self.shutdown.take() {
            let _ = signal.send(());
        }
        if let Some(state) = self.state.take() {
            state.begin_shutdown();
            let _ = state.wait_until_no_peers(Duration::from_secs(1)).await;
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
        for task in self.background.drain(..) {
            task.abort();
        }
    }
}

impl Drop for RelayHandle {
    fn drop(&mut self) {
        if let Some(signal) = self.shutdown.take() {
            let _ = signal.send(());
        }
        if let Some(state) = self.state.take() {
            // Drop 无法 await，只广播关闭信号；连接任务会自行发送 Close(1001) 并退出。
            state.begin_shutdown();
        }
        if let Some(join) = self.join.take() {
            join.abort();
        }
        for task in self.background.drain(..) {
            task.abort();
        }
    }
}

/// 启动中继服务；返回时监听端口已就绪（可用于测试）。
pub async fn start(config: RelayConfig) -> Result<RelayHandle> {
    validate_config(&config)?;

    // 可选暂存：启动时加载索引并清理过期/孤儿对象。
    let vault = match &config.vault_dir {
        Some(dir) => Some(
            VaultStore::open(
                dir.clone(),
                VaultLimits {
                    max_object_bytes: config.vault_max_object_bytes,
                    quota_bytes: config.vault_quota_bytes,
                    max_objects: config.vault_max_objects,
                },
            )
            .await
            .context("初始化加密暂存目录失败")?,
        ),
        None => None,
    };

    let vault_interval = Duration::from_secs(config.vault_reap_interval_secs.max(1));
    let pair_interval = Duration::from_secs(config.pair_cleanup_interval_secs.max(1));
    let bind = config.bind;
    let has_vault = vault.is_some();
    let state = RelayState::new(config, vault);

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("无法绑定监听地址 {bind}"))?;
    let addr = listener.local_addr().context("读取监听地址失败")?;

    let app = Router::new()
        .route("/ws", get(conn::ws_handler))
        .route("/healthz", get(healthz))
        .with_state(state.clone());

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let join = tokio::spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        });
        if let Err(error) = server.await {
            tracing::error!(%error, "中继 HTTP 服务异常退出");
        }
    });

    let mut background = Vec::new();
    if has_vault {
        background.push(tokio::spawn(vault_reaper(state.clone(), vault_interval)));
    }
    background.push(tokio::spawn(pair_reaper(state.clone(), pair_interval)));

    tracing::info!(
        %addr,
        vault = has_vault,
        "drive-relay 开始监听（ws://{addr}/ws，健康检查 /healthz）"
    );

    Ok(RelayHandle {
        addr,
        shutdown: Some(shutdown_tx),
        join: Some(join),
        background,
        state: Some(state),
    })
}

fn validate_config(config: &RelayConfig) -> Result<()> {
    if config.access_token.is_empty() {
        bail!("必须配置非空 access_token（中继不接受无认证连接）");
    }
    if config.max_frame_bytes < 64 || config.max_frame_bytes > 64 * 1024 * 1024 {
        bail!(
            "max_frame_bytes 必须在 64..=64 MiB 之间（当前 {}）",
            config.max_frame_bytes
        );
    }
    if config.vault_dir.is_some() && config.vault_quota_bytes == 0 {
        bail!("启用暂存时 vault_quota_bytes 不能为 0");
    }
    Ok(())
}

/// 健康检查：`200 ok`。
async fn healthz() -> &'static str {
    "ok"
}

/// 每 60s 清理过期暂存对象（启动时已在 `VaultStore::open` 中全量清理一次）。
async fn vault_reaper(state: Arc<RelayState>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // 跳过第一次立即触发
    loop {
        ticker.tick().await;
        let Some(vault) = &state.vault else {
            return;
        };
        match vault.reap().await {
            Ok(count) if count > 0 => {
                tracing::info!(count, "清理过期加密暂存对象");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "暂存清理任务失败"),
        }
    }
}

/// 后台清理过期配对登记与失败限流窗口。
async fn pair_reaper(state: Arc<RelayState>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let cleaned = {
            let mut pairs = state.pairs.lock().await;
            pairs.reap(std::time::Instant::now())
        };
        if cleaned > 0 {
            tracing::debug!(count = cleaned, "清理过期配对登记");
        }
        state.join_limiter.prune();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_protocol_limits() {
        let config = RelayConfig::default();
        assert_eq!(config.pair_ttl_secs, 120);
        assert_eq!(config.max_frame_bytes, MAX_WIRE_FRAME_BYTES);
        assert_eq!(config.idle_timeout_secs, 90);
        assert_eq!(config.rate_limit_per_sec, 2000);
        assert_eq!(config.rate_limit_burst, 4000);
        assert_eq!(config.rate_limit_max_wait_ms, 2000);
        assert_eq!(config.vault_quota_bytes, 1024 * 1024 * 1024);
        assert_eq!(config.vault_max_objects, 1000);
        assert_eq!(config.vault_reap_interval_secs, 60);
        assert_eq!(config.pair_join_limit_per_min, 5);
    }

    /// 大文件吞吐推导：默认 2000 帧/s × 64 KiB/块 = 125 MiB/s，满足架构 >20MB/s 目标。
    #[test]
    fn default_rate_limit_meets_large_file_throughput_target() {
        let config = RelayConfig::default();
        let chunk_bytes: u64 = 64 * 1024;
        let capacity_bytes_per_sec = config.rate_limit_per_sec as u64 * chunk_bytes;
        let target_bytes_per_sec: u64 = 20 * 1024 * 1024;
        assert!(
            capacity_bytes_per_sec >= target_bytes_per_sec,
            "默认限流容量 {capacity_bytes_per_sec} B/s 低于 20 MiB/s 目标"
        );
        // 突发容量至少覆盖 1 秒流量，避免正常窗口式传输触发背压。
        assert!(
            config.rate_limit_burst as u64 >= config.rate_limit_per_sec as u64,
            "burst 应至少覆盖 1 秒的帧流量"
        );
        // 背压等待须够长，确保正常限速下不丢帧。
        assert!(config.rate_limit_max_wait_ms >= 1000);
    }

    #[test]
    fn empty_token_rejected() {
        let config = RelayConfig::default();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn valid_config_accepted() {
        let mut config = RelayConfig::new("token");
        config.bind = "127.0.0.1:0".parse().unwrap();
        assert!(validate_config(&config).is_ok());
    }
}
