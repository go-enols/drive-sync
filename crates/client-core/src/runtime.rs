//! 异步运行时：连接循环（自动重连退避 1s→30s ±20% 抖动）+ Engine + WS 心跳（30s）。
//!
//! 对外提供 [`Runtime`]（消费式 `run()`）与可克隆的 [`RuntimeHandle`]（注入本机剪切板变化、
//! 目录操作、主动拉取、优雅关闭）。CLI daemon 与 e2e 测试都复用它。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use drive_core::clipboard::ClipboardPayload;
use drive_core::identity::Identity;
use tokio::sync::{mpsc, watch};

use crate::engine::{CatalogAddParams, Engine, EngineOutput, RuntimeCommand, RuntimeEvent};
use crate::net::Connection;
use crate::store::Store;
use crate::util::{backoff_delay, now_ms};

/// 默认出站帧速率：中继默认 2000 帧/s（突发 4000），客户端留 ~5% 余量避免触发限流。
pub const DEFAULT_OUTBOUND_FRAMES_PER_SEC: u32 = 1900;
/// 默认出站突发容量（中继为 4000）。
pub const DEFAULT_OUTBOUND_BURST: u32 = 3800;

/// 出站令牌桶：客户端主动背压，避免把中继的"先等待、超时才 rate_limited"路径打满。
struct OutboundPacer {
    rate_per_sec: f64,
    capacity: f64,
    tokens: f64,
    last_refill: tokio::time::Instant,
    cooldown_until: Option<tokio::time::Instant>,
    cooldown: Duration,
}

impl OutboundPacer {
    fn new(frames_per_sec: u32, burst: u32, cooldown: Duration) -> Self {
        let unlimited = frames_per_sec == 0;
        let capacity = if unlimited { 0.0 } else { burst.max(1) as f64 };
        Self {
            rate_per_sec: frames_per_sec as f64,
            capacity,
            tokens: capacity,
            last_refill: tokio::time::Instant::now(),
            cooldown_until: None,
            cooldown,
        }
    }

    fn unlimited(&self) -> bool {
        self.rate_per_sec <= 0.0
    }

    fn refill(&mut self) {
        if self.unlimited() {
            return;
        }
        let now = tokio::time::Instant::now();
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * self.rate_per_sec).min(self.capacity);
    }

    /// 取一个令牌；必要时等待（背压在这里生效，不会把帧丢给中继限流器）。
    async fn acquire(&mut self) {
        if self.unlimited() {
            return;
        }
        loop {
            if let Some(until) = self.cooldown_until {
                let now = tokio::time::Instant::now();
                if now < until {
                    tokio::time::sleep(until - now).await;
                    continue;
                }
                self.cooldown_until = None;
            }
            self.refill();
            if self.tokens >= 1.0 {
                self.tokens -= 1.0;
                return;
            }
            let wait_secs = (1.0 - self.tokens) / self.rate_per_sec;
            tokio::time::sleep(Duration::from_secs_f64(wait_secs.clamp(0.0005, 1.0))).await;
        }
    }

    /// 收到中继 `rate_limited` 回执：清空令牌并短暂停发（背压）。
    fn backoff(&mut self) {
        if self.unlimited() {
            return;
        }
        self.tokens = 0.0;
        self.last_refill = tokio::time::Instant::now();
        self.cooldown_until = Some(tokio::time::Instant::now() + self.cooldown);
    }
}

/// Runtime 配置。
#[derive(Clone)]
pub struct RuntimeConfig {
    pub data_dir: PathBuf,
    pub relay_url: String,
    pub access_token: String,
    /// 身份不存在时允许自动创建（e2e/测试用）；CLI daemon 不设置，要求先 `init`。
    pub device_name: Option<String>,
    /// 运行期事件出口（CLI 打印摘要 / e2e 断言）。
    pub event_tx: Option<mpsc::UnboundedSender<RuntimeEvent>>,
    /// 收到远端剪切板后应用内容的出口（CLI 交给剪切板后端线程；e2e 可直接消费）。
    pub remote_clipboard_tx: Option<std::sync::mpsc::Sender<ClipboardPayload>>,
    pub connect_timeout: Duration,
    /// 出站帧速率上限（帧/秒）。中继默认限流 2000 帧/s、突发 4000；令牌不足时中继会
    /// **先背压等待（默认最多 2s）而非丢帧**，超时才回 `error{rate_limited}`。
    /// 客户端默认压到 1900 帧/s 留出余量；0 表示不限速（自行调大中继 `--rate-limit`
    /// 的 e2e/bench 场景可设 0）。
    pub outbound_frames_per_sec: u32,
    /// 出站令牌桶突发容量（帧）。
    pub outbound_burst: u32,
    /// 收到 `rate_limited` 后的暂停发送时长（背压）。
    pub rate_limit_cooldown: Duration,
}

impl RuntimeConfig {
    pub fn new(
        data_dir: impl Into<PathBuf>,
        relay_url: impl Into<String>,
        access_token: impl Into<String>,
    ) -> Self {
        Self {
            data_dir: data_dir.into(),
            relay_url: relay_url.into(),
            access_token: access_token.into(),
            device_name: None,
            event_tx: None,
            remote_clipboard_tx: None,
            connect_timeout: Duration::from_secs(15),
            outbound_frames_per_sec: DEFAULT_OUTBOUND_FRAMES_PER_SEC,
            outbound_burst: DEFAULT_OUTBOUND_BURST,
            rate_limit_cooldown: Duration::from_millis(250),
        }
    }

    /// 关闭出站限速（仅建议在自行调大中继限流的测试/基准场景使用）。
    pub fn without_outbound_pacing(mut self) -> Self {
        self.outbound_frames_per_sec = 0;
        self
    }

    /// 自定义出站限速（帧/秒、突发容量）。
    pub fn with_outbound_pacing(mut self, frames_per_sec: u32, burst: u32) -> Self {
        self.outbound_frames_per_sec = frames_per_sec;
        self.outbound_burst = burst;
        self
    }

    pub fn with_device_name(mut self, name: impl Into<String>) -> Self {
        self.device_name = Some(name.into());
        self
    }

    pub fn with_event_sender(mut self, sender: mpsc::UnboundedSender<RuntimeEvent>) -> Self {
        self.event_tx = Some(sender);
        self
    }
}

impl std::fmt::Debug for RuntimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeConfig")
            .field("data_dir", &self.data_dir)
            .field("relay_url", &self.relay_url)
            .field(
                "access_token",
                &if self.access_token.is_empty() {
                    "（空）"
                } else {
                    "已配置"
                },
            )
            .field("device_name", &self.device_name)
            .finish()
    }
}

/// 可克隆的运行时句柄。
#[derive(Clone)]
pub struct RuntimeHandle {
    clip_tx: mpsc::Sender<ClipboardPayload>,
    cmd_tx: mpsc::Sender<RuntimeCommand>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_flag: Arc<AtomicBool>,
    store: Arc<Store>,
    device_id: String,
}

impl RuntimeHandle {
    /// 注入本机剪切板变化（异步）。
    pub async fn set_local_clipboard(&self, payload: ClipboardPayload) -> Result<()> {
        self.clip_tx
            .send(payload)
            .await
            .map_err(|_| anyhow!("运行时已退出，无法投递剪切板变化"))
    }

    /// 拿到原始发送端（供剪切板监听线程用 `blocking_send`）。
    pub fn local_clipboard_sender(&self) -> mpsc::Sender<ClipboardPayload> {
        self.clip_tx.clone()
    }

    /// 本地添加目录条目，返回新条目 id。
    pub async fn catalog_add(&self, params: CatalogAddParams) -> Result<String> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(RuntimeCommand::CatalogAdd {
                params,
                reply: Some(reply_tx),
            })
            .await
            .map_err(|_| anyhow!("运行时已退出"))?;
        match reply_rx.await.map_err(|_| anyhow!("运行时已退出"))? {
            Ok(entry_id) => Ok(entry_id),
            Err(message) => Err(anyhow!(message)),
        }
    }

    /// 本地删除目录条目（写墓碑）。
    pub async fn catalog_remove(&self, id: impl Into<String>) -> Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(RuntimeCommand::CatalogRemove {
                id: id.into(),
                reply: Some(reply_tx),
            })
            .await
            .map_err(|_| anyhow!("运行时已退出"))?;
        match reply_rx.await.map_err(|_| anyhow!("运行时已退出"))? {
            Ok(()) => Ok(()),
            Err(message) => Err(anyhow!(message)),
        }
    }

    /// 请求某对端传输指定目录条目的软件本体。
    pub async fn request_fetch(
        &self,
        peer_id: impl Into<String>,
        entry_id: impl Into<String>,
    ) -> Result<()> {
        self.cmd_tx
            .send(RuntimeCommand::Fetch {
                peer_id: peer_id.into(),
                entry_id: entry_id.into(),
            })
            .await
            .map_err(|_| anyhow!("运行时已退出"))
    }

    /// 优雅关闭。
    pub fn shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        let _ = self.shutdown_tx.send(true);
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown_flag.load(Ordering::SeqCst)
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn data_dir(&self) -> &std::path::Path {
        self.store.data_dir()
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }
}

/// 同步运行时（由 CLI daemon / e2e 驱动）。
pub struct Runtime {
    config: RuntimeConfig,
    engine: Engine,
    clip_rx: mpsc::Receiver<ClipboardPayload>,
    cmd_rx: mpsc::Receiver<RuntimeCommand>,
    shutdown_rx: watch::Receiver<bool>,
}

impl Runtime {
    /// 打开本地数据目录、加载身份与本地状态；返回运行时与句柄。
    pub fn open(config: RuntimeConfig) -> Result<(Runtime, RuntimeHandle)> {
        let store = Arc::new(
            Store::open(&config.data_dir)
                .with_context(|| format!("打开数据目录失败: {}", config.data_dir.display()))?,
        );
        let identity = load_identity(&store, config.device_name.as_deref())?;
        let mut effective = config;
        let stored = store.load_config().unwrap_or_default();
        if effective.relay_url.trim().is_empty() {
            effective.relay_url = stored.relay_url.clone();
        }
        if effective.access_token.is_empty() {
            effective.access_token = stored.access_token.clone();
        }
        let engine = Engine::new(store.clone(), identity)?;
        let device_id = engine.self_id().to_string();
        let (clip_tx, clip_rx) = mpsc::channel(128);
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let handle = RuntimeHandle {
            clip_tx,
            cmd_tx,
            shutdown_tx,
            shutdown_flag,
            store: store.clone(),
            device_id,
        };
        let runtime = Runtime {
            config: effective,
            engine,
            clip_rx,
            cmd_rx,
            shutdown_rx,
        };
        Ok((runtime, handle))
    }

    /// 常驻运行直到收到 shutdown（或不可恢复错误）。连接断开自动退避重连。
    pub async fn run(self) -> Result<()> {
        let Runtime {
            config,
            mut engine,
            mut clip_rx,
            mut cmd_rx,
            mut shutdown_rx,
        } = self;
        let identity = engine.identity().clone();
        let mut attempt: u32 = 0;
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            let connect = Connection::connect(&config.relay_url, &identity, &config.access_token);
            let connect_result = tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut shutdown_rx) => {
                    emit(&config, RuntimeEvent::Stopped);
                    return Ok(());
                }
                result = tokio::time::timeout(config.connect_timeout, connect) => {
                    match result {
                        Ok(result) => result,
                        Err(_) => Err(anyhow!("连接中继超时（{}）", config.relay_url)),
                    }
                }
            };
            let mut conn = match connect_result {
                Ok(conn) => conn,
                Err(error) => {
                    attempt = attempt.saturating_add(1);
                    let delay = backoff_delay(attempt.saturating_sub(1));
                    tracing::warn!(
                        relay = %config.relay_url,
                        error = %error,
                        retry_in_ms = delay.as_millis() as u64,
                        "连接中继失败，退避重试"
                    );
                    emit(
                        &config,
                        RuntimeEvent::Error {
                            message: format!("{error}（{:.1}s 后重试）", delay.as_secs_f32()),
                        },
                    );
                    if sleep_or_shutdown(&mut shutdown_rx, delay).await {
                        break;
                    }
                    continue;
                }
            };
            attempt = 0;
            let connected_at = std::time::Instant::now();
            let outcome = run_connected(
                &mut conn,
                &mut engine,
                &config,
                &mut clip_rx,
                &mut cmd_rx,
                &mut shutdown_rx,
            )
            .await;
            // 连接断开：清理会话状态（保留 spool 与 .part）。
            let cleanup = engine.on_disconnect();
            if let Err(error) = dispatch_events_only(&config, cleanup) {
                tracing::debug!(error = %error, "清理阶段发送失败");
            }
            match outcome {
                SessionOutcome::Shutdown => {
                    conn.close().await;
                    break;
                }
                SessionOutcome::Disconnected(reason) => {
                    tracing::warn!(reason = %reason, "连接断开");
                    conn.close().await;
                }
            }
            if connected_at.elapsed() < Duration::from_secs(10) {
                attempt = attempt.saturating_add(1);
            } else {
                attempt = 0;
            }
            let delay = backoff_delay(attempt);
            tracing::info!(retry_in_ms = delay.as_millis() as u64, "准备重连");
            if sleep_or_shutdown(&mut shutdown_rx, delay).await {
                break;
            }
        }
        engine.on_disconnect();
        emit(&config, RuntimeEvent::Stopped);
        Ok(())
    }
}

enum SessionOutcome {
    Shutdown,
    Disconnected(String),
}

async fn run_connected(
    conn: &mut Connection,
    engine: &mut Engine,
    config: &RuntimeConfig,
    clip_rx: &mut mpsc::Receiver<ClipboardPayload>,
    cmd_rx: &mut mpsc::Receiver<RuntimeCommand>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> SessionOutcome {
    let mut pacer = OutboundPacer::new(
        config.outbound_frames_per_sec,
        config.outbound_burst,
        config.rate_limit_cooldown,
    );
    let info = conn.connection_info();
    let initial = engine.on_authenticated(&info);
    if let Err(error) = dispatch(conn, config, &mut pacer, initial).await {
        return SessionOutcome::Disconnected(error.to_string());
    }
    for queued in conn.take_queued() {
        let output = engine.handle_server(queued);
        if let Err(error) = dispatch(conn, config, &mut pacer, output).await {
            return SessionOutcome::Disconnected(error.to_string());
        }
    }

    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_ping = std::time::Instant::now();

    loop {
        tokio::select! {
            biased;
            _ = wait_for_shutdown(shutdown_rx) => {
                return SessionOutcome::Shutdown;
            }
            clip = clip_rx.recv() => {
                match clip {
                    Some(payload) => {
                        let output = engine.on_local_clipboard(payload);
                        if let Err(error) = dispatch(conn, config, &mut pacer, output).await {
                            return SessionOutcome::Disconnected(error.to_string());
                        }
                    }
                    None => return SessionOutcome::Disconnected("剪切板输入通道已关闭".into()),
                }
            }
            command = cmd_rx.recv() => {
                match command {
                    Some(command) => {
                        let output = engine.handle_command(command);
                        if let Err(error) = dispatch(conn, config, &mut pacer, output).await {
                            return SessionOutcome::Disconnected(error.to_string());
                        }
                    }
                    None => return SessionOutcome::Disconnected("命令通道已关闭".into()),
                }
            }
            message = conn.next() => {
                match message {
                    Some(Ok(server_message)) => {
                        let output = engine.handle_server(server_message);
                        if let Err(error) = dispatch(conn, config, &mut pacer, output).await {
                            return SessionOutcome::Disconnected(error.to_string());
                        }
                    }
                    Some(Err(error)) => return SessionOutcome::Disconnected(error.to_string()),
                    None => return SessionOutcome::Disconnected("中继连接已关闭".into()),
                }
            }
            _ = ticker.tick() => {
                let output = engine.on_tick(now_ms());
                if let Err(error) = dispatch(conn, config, &mut pacer, output).await {
                    return SessionOutcome::Disconnected(error.to_string());
                }
                if last_ping.elapsed() >= Duration::from_secs(30) {
                    if let Err(error) = conn.send_ping().await {
                        return SessionOutcome::Disconnected(format!("WS 心跳失败: {error}"));
                    }
                    last_ping = std::time::Instant::now();
                }
            }
        }
    }
}

/// 发送 outbound、投递应用剪切板内容、上报事件。
///
/// 所有出站帧先取出站令牌桶令牌：中继默认 2000 帧/s，令牌不足时中继会先背压等待、
/// 超时才回 `error{rate_limited}`；客户端主动限速（默认 1900 帧/s、突发 3800）留出余量，
/// 收到 rate_limited 时仍短暂停发（兜底背压）。
async fn dispatch(
    conn: &mut Connection,
    config: &RuntimeConfig,
    pacer: &mut OutboundPacer,
    output: EngineOutput,
) -> Result<()> {
    let EngineOutput {
        outbound,
        applied_clipboard,
        events,
    } = output;
    for event in events {
        if let RuntimeEvent::RelayError { code, .. } = &event {
            if code == "rate_limited" {
                pacer.backoff();
                tracing::warn!(
                    cooldown_ms = config.rate_limit_cooldown.as_millis() as u64,
                    "中继返回 rate_limited，暂停发送以避免触发中继侧超时（背压）"
                );
            }
        }
        emit(config, event);
    }
    for payload in applied_clipboard {
        match &config.remote_clipboard_tx {
            Some(sender) => {
                // std::sync::mpsc：接收端是阻塞线程，send 不会阻塞（无界队列）。
                let _ = sender.send(payload);
            }
            None => {
                tracing::debug!(
                    size = payload.raw_size,
                    "收到远端剪切板但未注册应用回调（忽略）"
                );
            }
        }
    }
    let batch_size = outbound.len();
    let batch_start = std::time::Instant::now();
    for message in outbound {
        let frame_start = std::time::Instant::now();
        pacer.acquire().await;
        let acquire_elapsed = frame_start.elapsed();
        conn.send(&message)
            .await
            .map_err(|error| anyhow!("发送到中继失败: {error}"))?;
        let total_elapsed = frame_start.elapsed();
        if total_elapsed > Duration::from_millis(5) {
            tracing::trace!(
                batch_size,
                acquire_ms = acquire_elapsed.as_millis() as u64,
                send_ms = (total_elapsed - acquire_elapsed).as_millis() as u64,
                "单帧发送耗时较长"
            );
        }
    }
    if batch_size > 1 {
        tracing::trace!(
            batch_size,
            elapsed_ms = batch_start.elapsed().as_millis() as u64,
            "批量发送完成"
        );
    }
    Ok(())
}

fn dispatch_events_only(config: &RuntimeConfig, output: EngineOutput) -> Result<()> {
    for event in output.events {
        emit(config, event);
    }
    Ok(())
}

fn emit(config: &RuntimeConfig, event: RuntimeEvent) {
    tracing::debug!(event = %event.summary(), "运行时事件");
    if let Some(sender) = &config.event_tx {
        let _ = sender.send(event);
    }
}

async fn wait_for_shutdown(rx: &mut watch::Receiver<bool>) {
    if *rx.borrow() {
        return;
    }
    loop {
        if rx.changed().await.is_err() {
            return;
        }
        if *rx.borrow() {
            return;
        }
    }
}

async fn sleep_or_shutdown(rx: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = wait_for_shutdown(rx) => true,
    }
}

/// 加载身份：存在则读取；不存在且允许自动创建时生成（e2e 用）。
pub fn load_identity(store: &Store, device_name: Option<&str>) -> Result<Identity> {
    if store.identity_exists() {
        return store.load_identity();
    }
    match device_name {
        Some(name) if !name.trim().is_empty() => Ok(store.load_or_create_identity(name)?.0),
        _ => bail!(
            "尚未初始化设备身份（{}）。请先运行 `drive-sync init --name <设备名>`",
            store.identity_path().display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn pacer_unlimited_never_waits() {
        let mut pacer = OutboundPacer::new(0, 0, Duration::from_millis(50));
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            pacer.acquire().await;
        }
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "不限速模式不应等待"
        );
    }

    #[tokio::test]
    async fn pacer_backoff_pauses_sending() {
        // 突发 2 个令牌：两次 acquire 立即通过；backoff 之后必须等冷却时间。
        let mut pacer = OutboundPacer::new(1900, 2, Duration::from_millis(120));
        pacer.acquire().await;
        pacer.acquire().await;
        pacer.backoff();
        let start = std::time::Instant::now();
        pacer.acquire().await;
        assert!(
            start.elapsed() >= Duration::from_millis(100),
            "收到 rate_limited 后应至少暂停冷却时间，实际 {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn shutdown_stops_retry_loop_quickly() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.load_or_create_identity("测试设备").unwrap();
        // 指向一个没有服务监听的端口，运行时会在退避重连循环里。
        let config = RuntimeConfig::new(dir.path(), "ws://127.0.0.1:59999/ws", "");
        let (runtime, handle) = Runtime::open(config).unwrap();
        let task = tokio::spawn(runtime.run());
        tokio::time::sleep(Duration::from_millis(400)).await;
        handle.shutdown();
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("shutdown 后运行时未在 5 秒内退出")
            .expect("运行时任务 panic");
        assert!(result.is_ok(), "优雅关闭应返回 Ok: {result:?}");
    }

    #[tokio::test]
    async fn open_requires_identity_unless_device_name_given() {
        let dir = TempDir::new().unwrap();
        let config = RuntimeConfig::new(dir.path(), "ws://127.0.0.1:1/ws", "");
        assert!(Runtime::open(config).is_err(), "无身份且未允许创建时应报错");

        let dir2 = TempDir::new().unwrap();
        let config = RuntimeConfig::new(dir2.path(), "ws://127.0.0.1:1/ws", "")
            .with_device_name("自动创建设备");
        let (_runtime, handle) = Runtime::open(config).unwrap();
        assert!(handle.device_id().starts_with("dev-"));
    }
}
