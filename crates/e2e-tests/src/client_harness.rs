//! `drive_client::Runtime` 节点封装（阶段 2 双/多设备端到端测试用）。
//!
//! 目标：让测试用**库 API**（而不是 CLI 子进程）驱动真实客户端运行时：
//! - [`ClientNode::start`]：打开数据目录 + 异步 Runtime（自动重连、会话、心跳）；
//! - [`ClientNode::wait_event`] / [`ClientNode::recv_clipboard`]：等待 `RuntimeEvent`
//!   与远端剪切板内容（带超时，失败信息带客户端名）；
//! - [`ClientNode::catalog`] / [`ClientNode::shutdown`]：本地状态查询与优雅退出；
//! - [`pair_nodes`]：非交互配对（`pair_pairwise`，自动确认 SAS）；
//! - [`write_random_file`] / [`file_blake3`]：文件传输测试素材与校验。
//!
//! 注意：同一 `device_id` 在中继上只允许一条连接（新连接踢旧），因此测试里
//! **运行 Runtime 期间不要再用 `ops::*` 一次性连接**；需要 `vault_put/get`、
//! `catalog_fetch` 时先把 Runtime 关掉。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use drive_client::{PairOutcome, Runtime, RuntimeConfig, RuntimeEvent, RuntimeHandle};
use drive_core::catalog::{Catalog, CatalogEntry};
use drive_core::clipboard::ClipboardPayload;
use drive_core::util::{blake3_hex, random_bytes};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub use crate::harness::DEFAULT_TOKEN;

/// 事件等待默认上限（足够覆盖配对/握手/100ms 去抖/重连退避）。
pub const DEFAULT_EVENT_TIMEOUT: Duration = Duration::from_secs(20);

/// 一条已启动的客户端运行时（真实 WS 连接 + Engine + Store）。
pub struct ClientNode {
    name: String,
    device_id: String,
    data_dir: PathBuf,
    handle: RuntimeHandle,
    events: mpsc::UnboundedReceiver<RuntimeEvent>,
    /// 被 `wait_event` 过滤掉的非目标事件（保留给后续 `wait_event`/`drain_events`，
    /// 避免"连续 wait_session 丢失先到的会话事件"这类时序 flaky）。
    pending_events: std::collections::VecDeque<RuntimeEvent>,
    /// 远端剪切板应用出口（std mpsc，由 Engine 直接投递）。
    clipboard_rx: std::sync::mpsc::Receiver<ClipboardPayload>,
    run_task: Option<JoinHandle<Result<()>>>,
    relay_url: String,
    access_token: String,
}

impl ClientNode {
    /// 打开数据目录并启动运行时。身份必须已存在（先 `pair_nodes` 或自行生成）。
    pub fn start(
        data_dir: impl Into<PathBuf>,
        relay_url: impl Into<String>,
        access_token: impl Into<String>,
    ) -> Result<Self> {
        let data_dir = data_dir.into();
        let relay_url = relay_url.into();
        let access_token = access_token.into();

        let (event_tx, events) = mpsc::unbounded_channel::<RuntimeEvent>();
        let (clip_tx, clipboard_rx) = std::sync::mpsc::channel::<ClipboardPayload>();
        let mut config = RuntimeConfig::new(&data_dir, &relay_url, &access_token);
        config.event_tx = Some(event_tx);
        config.remote_clipboard_tx = Some(clip_tx);

        let (runtime, handle) = Runtime::open(config)
            .with_context(|| format!("打开客户端数据目录失败: {}", data_dir.display()))?;
        let identity = handle
            .store()
            .load_identity()
            .context("读取客户端身份失败")?;
        let name = identity.name.clone();
        let device_id = handle.device_id().to_string();
        let run_task = tokio::spawn(async move { runtime.run().await });

        Ok(Self {
            name,
            device_id,
            data_dir,
            handle,
            events,
            pending_events: std::collections::VecDeque::new(),
            clipboard_rx,
            run_task: Some(run_task),
            relay_url,
            access_token,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn relay_url(&self) -> &str {
        &self.relay_url
    }

    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn handle(&self) -> RuntimeHandle {
        self.handle.clone()
    }

    pub fn store(&self) -> &std::sync::Arc<drive_client::Store> {
        self.handle.store()
    }

    /// 等待满足条件的事件；不匹配的事件**缓存起来**供后续调用使用（不丢弃）。
    pub async fn wait_event<F>(
        &mut self,
        timeout: Duration,
        mut predicate: F,
    ) -> Result<RuntimeEvent>
    where
        F: FnMut(&RuntimeEvent) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // 先检查缓存中是否有满足条件的事件。
            if let Some(index) = self.pending_events.iter().position(&mut predicate) {
                if let Some(event) = self.pending_events.remove(index) {
                    return Ok(event);
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!(
                    "客户端 {} 等待事件超时（{timeout:?}）；已缓存未匹配事件 {} 条",
                    self.name,
                    self.pending_events.len()
                );
            }
            match tokio::time::timeout(remaining, self.events.recv()).await {
                Err(_) => bail!(
                    "客户端 {} 等待事件超时（{timeout:?}）；已缓存未匹配事件 {} 条",
                    self.name,
                    self.pending_events.len()
                ),
                Ok(None) => bail!("客户端 {} 事件通道已关闭", self.name),
                Ok(Some(event)) => {
                    if predicate(&event) {
                        return Ok(event);
                    }
                    self.pending_events.push_back(event);
                }
            }
        }
    }

    /// 等待连接中继成功。
    pub async fn wait_authenticated(&mut self) -> Result<()> {
        self.wait_event(DEFAULT_EVENT_TIMEOUT, |event| {
            matches!(event, RuntimeEvent::Authenticated { .. })
        })
        .await
        .map(|_| ())
    }

    /// 等待与指定对端建立 E2E 会话。
    pub async fn wait_session(&mut self, peer_id: &str) -> Result<()> {
        let peer_id = peer_id.to_string();
        self.wait_event(DEFAULT_EVENT_TIMEOUT, move |event| {
            matches!(event, RuntimeEvent::SessionEstablished { peer_id: id, .. } if *id == peer_id)
        })
        .await
        .map(|_| ())
    }

    /// 注入本机剪切板变化（模拟剪切板监听器）。
    pub async fn set_clipboard_text(&self, text: &str) -> Result<()> {
        let payload = ClipboardPayload::text(text).context("构造剪切板文本失败")?;
        self.handle
            .set_local_clipboard(payload)
            .await
            .map_err(|error| anyhow::anyhow!("{} 注入剪切板失败: {error}", self.name))?;
        Ok(())
    }

    /// 等待远端剪切板内容（Engine 应用后投递）。
    pub async fn recv_clipboard(&mut self, timeout: Duration) -> Result<ClipboardPayload> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.clipboard_rx.try_recv() {
                Ok(payload) => return Ok(payload),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    bail!("客户端 {} 剪切板通道已关闭", self.name)
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if tokio::time::Instant::now() >= deadline {
                        bail!("客户端 {} 等待远端剪切板超时（{timeout:?}）", self.name);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    }

    /// 取走当前已排队的事件（含之前被过滤缓存的），用于"不应发生某事"的断言。
    pub fn drain_events(&mut self) -> Vec<RuntimeEvent> {
        let mut drained: Vec<RuntimeEvent> = self.pending_events.drain(..).collect();
        while let Ok(event) = self.events.try_recv() {
            drained.push(event);
        }
        drained
    }

    /// 读取持久化目录（引擎每次变更都会原子落盘，故可反映最新状态）。
    pub fn catalog(&self) -> Result<(Catalog, u64)> {
        self.store().load_catalog()
    }

    /// 当前可见（未删除）的目录条目。
    pub fn visible_entries(&self) -> Result<Vec<CatalogEntry>> {
        Ok(self.catalog()?.0.visible().cloned().collect())
    }

    /// 目录中某条目（含墓碑，若存在）。
    pub fn catalog_entry(&self, entry_id: &str) -> Result<Option<CatalogEntry>> {
        Ok(self.catalog()?.0.get_any(entry_id).cloned())
    }

    /// 优雅关闭：通知 Runtime 停止并等待任务退出（最多 5s）。
    pub async fn shutdown(mut self) -> Result<()> {
        self.handle.shutdown();
        self.join_run_task().await
    }

    async fn join_run_task(&mut self) -> Result<()> {
        let Some(mut task) = self.run_task.take() else {
            return Ok(());
        };
        match tokio::time::timeout(Duration::from_secs(5), &mut task).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => bail!("客户端 {} 运行时退出错误: {error}", self.name),
            Ok(Err(join_error)) => bail!("客户端 {} 运行时任务异常: {join_error}", self.name),
            Err(_) => {
                task.abort();
                bail!("客户端 {} 运行时未在 5s 内退出", self.name)
            }
        }
    }
}

impl Drop for ClientNode {
    fn drop(&mut self) {
        // 测试提前失败时也尽量停止运行时，避免后台任务泄漏。
        self.handle.shutdown();
        if let Some(task) = self.run_task.take() {
            task.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// 配对 / 文件 / 等待辅助
// ---------------------------------------------------------------------------

/// 用 `pair_pairwise` 完成两端非交互配对（自动确认 SAS），返回双方结果。
///
/// 会产生一次性的中继连接；调用时两端不应有正在运行的 Runtime（中继单连接会互踢）。
pub async fn pair_nodes(
    dir_a: &Path,
    dir_b: &Path,
    name_a: &str,
    name_b: &str,
    relay_url: &str,
    access_token: &str,
) -> Result<(PairOutcome, PairOutcome)> {
    let config_a = RuntimeConfig::new(dir_a, relay_url, access_token).with_device_name(name_a);
    let config_b = RuntimeConfig::new(dir_b, relay_url, access_token).with_device_name(name_b);
    let (outcome_a, outcome_b) =
        drive_client::pair_pairwise(&config_a, &config_b, Duration::from_secs(30))
            .await
            .context("非交互配对失败")?;
    if outcome_a.record.pairing_fingerprint != outcome_b.record.pairing_fingerprint {
        bail!(
            "两端配对指纹不一致: {} vs {}",
            outcome_a.record.pairing_fingerprint,
            outcome_b.record.pairing_fingerprint
        );
    }
    Ok((outcome_a, outcome_b))
}

/// 在 `dir` 下生成随机文件并返回路径（用于目录/文件传输测试）。
pub fn write_random_file(dir: &Path, name: &str, size: usize) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("创建目录失败: {}", dir.display()))?;
    let path = dir.join(name);
    let data = random_bytes(size);
    std::fs::write(&path, &data).with_context(|| format!("写测试文件失败: {}", path.display()))?;
    Ok(path)
}

/// 文件 BLAKE3（hex），用于传输后校验。
pub fn file_blake3(path: &Path) -> Result<String> {
    let data = std::fs::read(path).with_context(|| format!("读取文件失败: {}", path.display()))?;
    Ok(blake3_hex(&data))
}

/// 轮询等待异步条件成立（50ms 间隔），超时报错。
pub async fn wait_until<F, Fut>(timeout: Duration, mut condition: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition().await {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("等待条件成立超时（{timeout:?}）");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 事件列表的一行摘要（断言失败时便于排查；不含内容明文）。
pub fn event_summaries(events: &[RuntimeEvent]) -> Vec<String> {
    events.iter().map(|event| event.summary()).collect()
}
