//! GUI 后台：同步 Runtime 生命周期、事件转发、剪切板线程、配对、目录/暂存/更新等异步操作。
//!
//! 线程模型：
//! - UI 线程（eframe）通过 [`Cmd`] 发送命令，通过 [`UiMsg`] 接收结果（std mpsc，非阻塞 try_recv）；
//! - 本模块的后台线程持有 tokio Runtime（全局唯一），负责 `Runtime::run()`、配对、一次性网络操作；
//! - 另一个独立线程轮询本机剪切板（arboard，默认 800ms，哈希去重）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use drive_client::{
    catalog_fetch, pair_initiate, pair_respond, vault_get, vault_list, vault_put, CatalogAddParams,
    LocalIndexEntry, PairUi, Runtime, RuntimeConfig, RuntimeEvent, RuntimeHandle, Store,
};
use drive_core::catalog::{CatalogEntry, TargetPlatform};
use drive_core::clipboard::ClipboardPayload;
use drive_core::identity::{Identity, PublicIdentity};
use drive_core::util::{blake3_hex, now_ms};

use crate::logbuf;
use crate::platform;

// ---------------------------------------------------------------------------
// 数据结构（UI ↔ 后台）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct PairProgress {
    pub phase: String,
    pub qr: Option<String>,
    pub code: Option<String>,
    pub peer_name: Option<String>,
    pub peer_id: Option<String>,
    pub sas: Option<String>,
    pub waiting_confirm: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairMode {
    Initiate,
    Join,
}

#[derive(Clone, Debug)]
pub struct PairInput {
    pub mode: PairMode,
    pub relay_url: String,
    pub access_token: String,
    pub code: Option<String>,
    pub qr: Option<String>,
}

#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub notes: String,
    pub available: bool,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct VaultItem {
    pub object_id: String,
    pub name: String,
    pub size: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug)]
pub enum UiMsg {
    SyncStarted {
        device_id: String,
    },
    SyncStopped {
        reason: String,
    },
    Event(RuntimeEvent),
    PairState(PairProgress),
    PairFinished {
        ok: bool,
        message: String,
    },
    TestResult {
        ok: bool,
        message: String,
    },
    CatalogAddResult {
        ok: bool,
        message: String,
    },
    CatalogRemoveResult {
        ok: bool,
        message: String,
    },
    FetchFinished {
        ok: bool,
        message: String,
        path: Option<PathBuf>,
    },
    VaultList(Vec<VaultItem>),
    VaultOpDone {
        ok: bool,
        message: String,
    },
    UpdateCheck(UpdateInfo),
    UpdateApplied {
        ok: bool,
        message: String,
    },
    Log(String),
}

#[derive(Clone)]
pub struct ClipboardControls {
    pub poll_ms: Arc<AtomicU64>,
    pub share_text: Arc<AtomicBool>,
    pub share_images: Arc<AtomicBool>,
}

impl ClipboardControls {
    pub fn from_store(store: &Store) -> Self {
        let config = store.load_config().unwrap_or_default();
        Self {
            poll_ms: Arc::new(AtomicU64::new(config.poll_ms.clamp(200, 5000))),
            share_text: Arc::new(AtomicBool::new(config.clipboard.share_text)),
            share_images: Arc::new(AtomicBool::new(config.clipboard.share_images)),
        }
    }
}

/// UI → 后台命令。
pub enum Cmd {
    StartSync {
        relay_url: String,
        access_token: String,
        remote_clipboard: Sender<ClipboardPayload>,
    },
    StopSync,
    Pair(PairInput),
    PairDecision(bool),
    TestConnection {
        relay_url: String,
        access_token: String,
    },
    CatalogAdd {
        path: PathBuf,
    },
    CatalogRemove {
        id: String,
    },
    FetchCatalog {
        entry_id: String,
        out_dir: PathBuf,
    },
    VaultList,
    VaultPut {
        path: PathBuf,
        ttl_days: u64,
    },
    VaultGet {
        object_id: String,
        out_dir: PathBuf,
    },
    CheckUpdate,
    ApplyUpdate,
    LocalClipboard(ClipboardPayload),
    Shutdown,
    /// 内部：配对任务完成。
    PairDone {
        ok: bool,
        message: String,
    },
    /// 内部：一次性操作完成（用于恢复此前暂停的同步）。
    OpDone,
}

/// GUI 后台句柄。
pub struct Backend {
    pub cmd_tx: Sender<Cmd>,
    pub controls: ClipboardControls,
    msg_rx: Receiver<UiMsg>,
    join: Option<JoinHandle<()>>,
}

impl Backend {
    pub fn spawn(
        tokio_rt: &'static tokio::runtime::Runtime,
        data_dir: PathBuf,
        controls: ClipboardControls,
    ) -> Backend {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (msg_tx, msg_rx) = mpsc::channel::<UiMsg>();
        let controls_clone = controls.clone();
        let cmd_tx_for_state = cmd_tx.clone();
        let join = std::thread::Builder::new()
            .name("drive-gui-backend".to_string())
            .spawn(move || {
                let state = BackendState {
                    tokio: tokio_rt,
                    data_dir,
                    msg_tx,
                    cmd_tx: cmd_tx_for_state,
                    controls: controls_clone,
                    sync: None,
                    watcher: None,
                    pair_shared: None,
                    pair_was_running: false,
                    last_sync: None,
                    remote_clipboard: None,
                    pending_restart: false,
                };
                backend_loop(state, cmd_rx);
            })
            .expect("启动后台线程失败");
        Backend {
            cmd_tx,
            controls,
            msg_rx,
            join: Some(join),
        }
    }

    pub fn try_recv(&self) -> Option<UiMsg> {
        self.msg_rx.try_recv().ok()
    }

    pub fn shutdown(mut self) {
        let _ = self.cmd_tx.send(Cmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

// ---------------------------------------------------------------------------
// 后台状态
// ---------------------------------------------------------------------------

struct SyncState {
    handle: RuntimeHandle,
    task: tokio::task::JoinHandle<()>,
    events: tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>,
}

struct Watcher {
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

struct PairShared {
    progress: Mutex<PairProgress>,
    decision: (Mutex<Option<bool>>, Condvar),
}

struct BackendState {
    tokio: &'static tokio::runtime::Runtime,
    data_dir: PathBuf,
    msg_tx: Sender<UiMsg>,
    cmd_tx: Sender<Cmd>,
    controls: ClipboardControls,
    sync: Option<SyncState>,
    watcher: Option<Watcher>,
    pair_shared: Option<Arc<PairShared>>,
    pair_was_running: bool,
    last_sync: Option<(String, String)>,
    remote_clipboard: Option<Sender<ClipboardPayload>>,
    pending_restart: bool,
}

impl BackendState {
    fn send(&self, message: UiMsg) {
        let _ = self.msg_tx.send(message);
    }

    fn log(&self, message: impl Into<String>) {
        let message = message.into();
        logbuf::log(message.clone());
        let _ = self.msg_tx.send(UiMsg::Log(message));
    }

    fn store(&self) -> Result<Store> {
        Store::open(&self.data_dir)
    }

    fn stop_sync(&mut self) -> Option<String> {
        let sync = self.sync.take()?;
        sync.handle.shutdown();
        let _ = self
            .tokio
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), sync.task).await });
        self.stop_watcher();
        Some(sync.handle.device_id().to_string())
    }

    fn stop_watcher(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.stop.store(true, Ordering::SeqCst);
            let _ = watcher.join.join();
        }
    }

    fn start_sync(&mut self, relay_url: String, access_token: String) -> Result<()> {
        self.stop_sync();
        let store = self.store()?;
        if !store.identity_exists() {
            anyhow::bail!("设备尚未初始化（缺少 identity.json）");
        }
        self.controls.poll_ms.store(
            store
                .load_config()
                .map(|c| c.poll_ms)
                .unwrap_or(300)
                .clamp(200, 5000),
            Ordering::SeqCst,
        );
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<RuntimeEvent>();
        let mut config = RuntimeConfig::new(&self.data_dir, &relay_url, &access_token);
        config.event_tx = Some(event_tx);
        config.remote_clipboard_tx = self.remote_clipboard.clone();
        let (runtime, handle) = Runtime::open(config)?;
        let device_id = handle.device_id().to_string();
        let msg_tx = self.msg_tx.clone();
        let task = self.tokio.spawn(async move {
            if let Err(error) = runtime.run().await {
                let text = format!("{error:#}");
                logbuf::log(format!("同步运行时退出: {text}"));
                // 连接失败/运行时退出必须回传 UI，让用户看到错误并可重试（不阻塞窗口）。
                let _ = msg_tx.send(UiMsg::SyncStopped {
                    reason: format!("连接中继/同步失败: {text}"),
                });
            }
        });
        let watcher =
            spawn_clipboard_watcher(self.tokio, self.controls.clone(), self.cmd_tx.clone())?;
        self.sync = Some(SyncState {
            handle,
            task,
            events: event_rx,
        });
        self.watcher = Some(watcher);
        self.last_sync = Some((relay_url, access_token));
        self.send(UiMsg::SyncStarted {
            device_id: device_id.clone(),
        });
        self.log(format!("同步已启动（{device_id}）"));
        Ok(())
    }

    fn drain_events(&mut self) {
        let mut events = Vec::new();
        if let Some(sync) = self.sync.as_mut() {
            while let Ok(event) = sync.events.try_recv() {
                events.push(event);
                if events.len() >= 500 {
                    break;
                }
            }
        }
        for event in events {
            self.send(UiMsg::Event(event));
        }
    }

    fn resume_after_op(&mut self) {
        if !self.pending_restart {
            return;
        }
        self.pending_restart = false;
        if let Some((relay, token)) = self.last_sync.clone() {
            if let Err(error) = self.start_sync(relay, token) {
                self.send(UiMsg::SyncStopped {
                    reason: format!("恢复同步失败: {error:#}"),
                });
            }
        }
    }

    fn pause_for_op(&mut self) {
        if self.sync.is_some() {
            self.pending_restart = true;
            self.stop_sync();
        }
    }
}

// ---------------------------------------------------------------------------
// 主循环
// ---------------------------------------------------------------------------

fn backend_loop(mut state: BackendState, cmd_rx: Receiver<Cmd>) {
    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Cmd::Shutdown) => break,
            Ok(command) => {
                if !handle_command(&mut state, command) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        state.drain_events();
    }
    state.stop_sync();
    state.stop_watcher();
    state.log("GUI 后台线程已退出");
}

fn handle_command(state: &mut BackendState, command: Cmd) -> bool {
    match command {
        Cmd::Shutdown => return false,
        Cmd::StartSync {
            relay_url,
            access_token,
            remote_clipboard,
        } => {
            state.remote_clipboard = Some(remote_clipboard);
            if let Err(error) = state.start_sync(relay_url, access_token) {
                state.send(UiMsg::SyncStopped {
                    reason: format!("{error:#}"),
                });
            }
        }
        Cmd::StopSync => {
            state.pending_restart = false;
            let device = state.stop_sync();
            state.send(UiMsg::SyncStopped {
                reason: if device.is_some() {
                    "已停止同步".to_string()
                } else {
                    "同步未在运行".to_string()
                },
            });
            state.log("同步已停止");
        }
        Cmd::Pair(input) => {
            state.pair_was_running = state.sync.is_some();
            state.pending_restart = false;
            state.stop_sync();
            let shared = Arc::new(PairShared {
                progress: Mutex::new(PairProgress {
                    phase: "正在连接中继…".to_string(),
                    ..Default::default()
                }),
                decision: (Mutex::new(None), Condvar::new()),
            });
            state.pair_shared = Some(shared.clone());
            state.send(UiMsg::PairState(
                shared
                    .progress
                    .lock()
                    .map(|guard| guard.clone())
                    .unwrap_or_default(),
            ));
            let ui = GuiPairUi {
                shared: shared.clone(),
                msg_tx: state.msg_tx.clone(),
            };
            let cmd_tx = state.cmd_tx.clone();
            let data_dir = state.data_dir.clone();
            state.tokio.spawn(async move {
                let config = RuntimeConfig::new(&data_dir, &input.relay_url, &input.access_token);
                let result = match input.mode {
                    PairMode::Initiate => pair_initiate(&config, &ui).await.map(|outcome| {
                        (
                            outcome.record.peer.name.clone(),
                            outcome.record.peer.device_id.clone(),
                            outcome.sas.clone(),
                        )
                    }),
                    PairMode::Join => {
                        pair_respond(&config, input.qr.as_deref(), input.code.as_deref(), &ui)
                            .await
                            .map(|outcome| {
                                (
                                    outcome.record.peer.name.clone(),
                                    outcome.record.peer.device_id.clone(),
                                    outcome.sas.clone(),
                                )
                            })
                    }
                };
                let (ok, message) = match result {
                    Ok((name, device_id, sas)) => {
                        (true, format!("配对成功：{name}（{device_id}），SAS {sas}"))
                    }
                    Err(error) => (false, format!("配对失败: {error:#}")),
                };
                let _ = cmd_tx.send(Cmd::PairDone { ok, message });
            });
        }
        Cmd::PairDecision(accept) => {
            if let Some(shared) = state.pair_shared.as_ref() {
                let mut decision = shared
                    .decision
                    .0
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                *decision = Some(accept);
                shared.decision.1.notify_all();
            }
        }
        Cmd::PairDone { ok, message } => {
            state.pair_shared = None;
            state.send(UiMsg::PairFinished {
                ok,
                message: message.clone(),
            });
            state.log(message);
            if ok && state.pair_was_running {
                if let Some((relay, token)) = state.last_sync.clone() {
                    if let Err(error) = state.start_sync(relay, token) {
                        state.send(UiMsg::SyncStopped {
                            reason: format!("配对后恢复同步失败: {error:#}"),
                        });
                    }
                }
            }
            state.pair_was_running = false;
        }
        Cmd::TestConnection {
            relay_url,
            access_token,
        } => {
            let msg_tx = state.msg_tx.clone();
            state.tokio.spawn(async move {
                let result = test_connection(&relay_url, &access_token).await;
                let _ = msg_tx.send(result);
            });
        }
        Cmd::CatalogAdd { path } => {
            if let Some(handle) = state.sync.as_ref().map(|sync| sync.handle.clone()) {
                let msg_tx = state.msg_tx.clone();
                let params = CatalogAddParams::new(&path);
                state.tokio.spawn(async move {
                    let (ok, message) = match handle.catalog_add(params).await {
                        Ok(entry_id) => (true, format!("已添加目录条目: {entry_id}")),
                        Err(error) => (false, format!("添加目录条目失败: {error:#}")),
                    };
                    let _ = msg_tx.send(UiMsg::CatalogAddResult { ok, message });
                });
            } else {
                let result = catalog_add_offline(&state.data_dir, &path);
                let (ok, message) = match result {
                    Ok(entry_id) => (true, format!("已添加目录条目（离线）: {entry_id}")),
                    Err(error) => (false, format!("添加目录条目失败: {error:#}")),
                };
                state.send(UiMsg::CatalogAddResult { ok, message });
            }
        }
        Cmd::CatalogRemove { id } => {
            if let Some(handle) = state.sync.as_ref().map(|sync| sync.handle.clone()) {
                let msg_tx = state.msg_tx.clone();
                state.tokio.spawn(async move {
                    let (ok, message) = match handle.catalog_remove(id.clone()).await {
                        Ok(()) => (true, format!("已删除目录条目: {id}")),
                        Err(error) => (false, format!("删除目录条目失败: {error:#}")),
                    };
                    let _ = msg_tx.send(UiMsg::CatalogRemoveResult { ok, message });
                });
            } else {
                let result = catalog_remove_offline(&state.data_dir, &id);
                let (ok, message) = match result {
                    Ok(()) => (true, format!("已删除目录条目（离线）: {id}")),
                    Err(error) => (false, format!("删除目录条目失败: {error:#}")),
                };
                state.send(UiMsg::CatalogRemoveResult { ok, message });
            }
        }
        Cmd::FetchCatalog { entry_id, out_dir } => {
            let (relay, token) = match state.sync_config() {
                Some(value) => value,
                None => {
                    state.send(UiMsg::FetchFinished {
                        ok: false,
                        message: "尚未配置中继/令牌".to_string(),
                        path: None,
                    });
                    return true;
                }
            };
            state.pause_for_op();
            let msg_tx = state.msg_tx.clone();
            let cmd_tx = state.cmd_tx.clone();
            let data_dir = state.data_dir.clone();
            state.tokio.spawn(async move {
                let config = RuntimeConfig::new(&data_dir, relay, token);
                let result =
                    catalog_fetch(&config, &entry_id, Some(out_dir), Duration::from_secs(300))
                        .await;
                let message = match result {
                    Ok(path) => UiMsg::FetchFinished {
                        ok: true,
                        message: format!("下载完成: {}", path.display()),
                        path: Some(path),
                    },
                    Err(error) => UiMsg::FetchFinished {
                        ok: false,
                        message: format!("下载失败: {error:#}"),
                        path: None,
                    },
                };
                let _ = msg_tx.send(message);
                let _ = cmd_tx.send(Cmd::OpDone);
            });
        }
        Cmd::VaultList => {
            let Some((relay, token)) = state.sync_config() else {
                state.send(UiMsg::VaultOpDone {
                    ok: false,
                    message: "尚未配置中继/令牌".to_string(),
                });
                return true;
            };
            state.pause_for_op();
            let msg_tx = state.msg_tx.clone();
            let cmd_tx = state.cmd_tx.clone();
            let data_dir = state.data_dir.clone();
            state.tokio.spawn(async move {
                let config = RuntimeConfig::new(&data_dir, relay, token);
                match vault_list(&config).await {
                    Ok(objects) => {
                        let items = objects
                            .into_iter()
                            .map(|object| VaultItem {
                                object_id: object.object_id,
                                name: object.meta.name,
                                size: object.meta.size,
                                expires_at_ms: object.expires_at_ms,
                            })
                            .collect();
                        let _ = msg_tx.send(UiMsg::VaultList(items));
                    }
                    Err(error) => {
                        let _ = msg_tx.send(UiMsg::VaultOpDone {
                            ok: false,
                            message: format!("获取暂存列表失败: {error:#}"),
                        });
                    }
                }
                let _ = cmd_tx.send(Cmd::OpDone);
            });
        }
        Cmd::VaultPut { path, ttl_days } => {
            let Some((relay, token)) = state.sync_config() else {
                state.send(UiMsg::VaultOpDone {
                    ok: false,
                    message: "尚未配置中继/令牌".to_string(),
                });
                return true;
            };
            state.pause_for_op();
            let msg_tx = state.msg_tx.clone();
            let cmd_tx = state.cmd_tx.clone();
            let data_dir = state.data_dir.clone();
            state.tokio.spawn(async move {
                let config = RuntimeConfig::new(&data_dir, relay, token);
                let message = match vault_put(&config, &path, ttl_days).await {
                    Ok(outcome) => UiMsg::VaultOpDone {
                        ok: true,
                        message: format!(
                            "上传成功: {}（保留至 {}）",
                            outcome.object_id,
                            platform::format_ms(outcome.expires_at_ms)
                        ),
                    },
                    Err(error) => UiMsg::VaultOpDone {
                        ok: false,
                        message: format!("上传失败: {error:#}"),
                    },
                };
                let _ = msg_tx.send(message);
                let _ = cmd_tx.send(Cmd::OpDone);
            });
        }
        Cmd::VaultGet { object_id, out_dir } => {
            let Some((relay, token)) = state.sync_config() else {
                state.send(UiMsg::VaultOpDone {
                    ok: false,
                    message: "尚未配置中继/令牌".to_string(),
                });
                return true;
            };
            state.pause_for_op();
            let msg_tx = state.msg_tx.clone();
            let cmd_tx = state.cmd_tx.clone();
            let data_dir = state.data_dir.clone();
            state.tokio.spawn(async move {
                let config = RuntimeConfig::new(&data_dir, relay, token);
                let out_path = out_dir.join(format!("{object_id}.bin"));
                let message = match vault_get(&config, &object_id, &out_path).await {
                    Ok(path) => UiMsg::VaultOpDone {
                        ok: true,
                        message: format!("已解密保存到: {}", path.display()),
                    },
                    Err(error) => UiMsg::VaultOpDone {
                        ok: false,
                        message: format!("下载解密失败: {error:#}"),
                    },
                };
                let _ = msg_tx.send(message);
                let _ = cmd_tx.send(Cmd::OpDone);
            });
        }
        Cmd::CheckUpdate => {
            let msg_tx = state.msg_tx.clone();
            let data_dir = state.data_dir.clone();
            state.tokio.spawn(async move {
                let result =
                    tokio::task::spawn_blocking(move || check_update_blocking(&data_dir)).await;
                match result {
                    Ok(Ok(info)) => {
                        let _ = msg_tx.send(UiMsg::UpdateCheck(info));
                    }
                    Ok(Err(error)) => {
                        let _ = msg_tx.send(UiMsg::UpdateApplied {
                            ok: false,
                            message: format!("检查更新失败: {error:#}"),
                        });
                    }
                    Err(error) => {
                        let _ = msg_tx.send(UiMsg::UpdateApplied {
                            ok: false,
                            message: format!("检查更新任务异常: {error}"),
                        });
                    }
                }
            });
        }
        Cmd::ApplyUpdate => {
            state.pause_for_op();
            let msg_tx = state.msg_tx.clone();
            let cmd_tx = state.cmd_tx.clone();
            let data_dir = state.data_dir.clone();
            state.tokio.spawn(async move {
                let result =
                    tokio::task::spawn_blocking(move || apply_update_blocking(&data_dir)).await;
                let message = match result {
                    Ok(Ok(text)) => UiMsg::UpdateApplied {
                        ok: true,
                        message: text,
                    },
                    Ok(Err(error)) => UiMsg::UpdateApplied {
                        ok: false,
                        message: format!("更新失败: {error:#}"),
                    },
                    Err(error) => UiMsg::UpdateApplied {
                        ok: false,
                        message: format!("更新任务异常: {error}"),
                    },
                };
                let _ = msg_tx.send(message);
                let _ = cmd_tx.send(Cmd::OpDone);
            });
        }
        Cmd::LocalClipboard(payload) => {
            if let Some(handle) = state.sync.as_ref().map(|sync| sync.handle.clone()) {
                state.tokio.spawn(async move {
                    let _ = handle.set_local_clipboard(payload).await;
                });
            }
        }
        Cmd::OpDone => {
            state.resume_after_op();
        }
    }
    true
}

impl BackendState {
    fn sync_config(&self) -> Option<(String, String)> {
        if let Some(value) = self.last_sync.clone() {
            return Some(value);
        }
        let store = self.store().ok()?;
        let config = store.load_config().ok()?;
        let relay = config.relay_url.trim().to_string();
        if relay.is_empty() {
            return None;
        }
        Some((relay, config.access_token.clone()))
    }
}

// ---------------------------------------------------------------------------
// 配对 UI（把进度回传 UI 线程，SAS 阻塞等待用户确认）
// ---------------------------------------------------------------------------

struct GuiPairUi {
    shared: Arc<PairShared>,
    msg_tx: Sender<UiMsg>,
}

impl GuiPairUi {
    fn update(&self, mutate: impl FnOnce(&mut PairProgress)) {
        {
            let mut progress = self
                .shared
                .progress
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            mutate(&mut progress);
        }
        self.emit();
    }

    fn emit(&self) {
        let snapshot = self
            .shared
            .progress
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let _ = self.msg_tx.send(UiMsg::PairState(snapshot));
    }
}

impl PairUi for GuiPairUi {
    fn on_qr(&self, qr: &str) {
        self.update(|progress| progress.qr = Some(qr.to_string()));
    }

    fn on_code(&self, code: &str) {
        self.update(|progress| {
            progress.code = Some(code.to_string());
            progress.phase = "等待对端加入配对…".to_string();
        });
    }

    fn on_peer(&self, peer: &PublicIdentity) {
        self.update(|progress| {
            progress.peer_name = Some(peer.name.clone());
            progress.peer_id = Some(peer.device_id.clone());
            progress.phase = "对端已加入，等待确认 SAS".to_string();
        });
    }

    fn on_sas(&self, sas: &str) {
        self.update(|progress| {
            progress.sas = Some(sas.to_string());
            progress.waiting_confirm = true;
            progress.phase = "请核对 SAS 后确认".to_string();
        });
    }

    fn confirm_sas(&self, sas: &str) -> bool {
        self.on_sas(sas);
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            if let Some(shared) = Some(self.shared.as_ref()) {
                let decision = shared
                    .decision
                    .0
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if let Some(value) = *decision {
                    return value;
                }
            }
            let now = Instant::now();
            if now >= deadline {
                self.update(|progress| {
                    progress.phase = "SAS 确认超时，配对已取消".to_string();
                    progress.waiting_confirm = false;
                });
                return false;
            }
            let wait = (deadline - now).min(Duration::from_millis(200));
            let _ = self
                .shared
                .decision
                .1
                .wait_timeout_while(
                    self.shared
                        .decision
                        .0
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner()),
                    wait,
                    |decision| decision.is_none(),
                )
                .ok();
        }
    }
}

// ---------------------------------------------------------------------------
// 剪切板监听线程
// ---------------------------------------------------------------------------

fn spawn_clipboard_watcher(
    _tokio: &'static tokio::runtime::Runtime,
    controls: ClipboardControls,
    cmd_tx: Sender<Cmd>,
) -> Result<Watcher> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    let join = std::thread::Builder::new()
        .name("drive-gui-clipboard".to_string())
        .spawn(move || clipboard_watcher_loop(controls, cmd_tx, stop_clone))
        .context("启动剪切板监听线程失败")?;
    Ok(Watcher { stop, join })
}

fn clipboard_watcher_loop(controls: ClipboardControls, cmd_tx: Sender<Cmd>, stop: Arc<AtomicBool>) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(error) => {
            logbuf::log(format!("初始化系统剪切板失败（剪切板同步不可用）: {error}"));
            return;
        }
    };
    let mut last_text_hash: Option<String> = clipboard
        .get_text()
        .ok()
        .map(|text| blake3_hex(text.as_bytes()));
    let mut last_image_hash: Option<String> = None;
    while !stop.load(Ordering::SeqCst) {
        let poll_ms = controls.poll_ms.load(Ordering::SeqCst).clamp(200, 5000);
        std::thread::sleep(Duration::from_millis(poll_ms));
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if controls.share_text.load(Ordering::SeqCst) {
            if let Ok(text) = clipboard.get_text() {
                let hash = blake3_hex(text.as_bytes());
                if last_text_hash.as_ref() != Some(&hash) {
                    last_text_hash = Some(hash);
                    if let Ok(payload) = ClipboardPayload::text(&text) {
                        let _ = cmd_tx.send(Cmd::LocalClipboard(payload));
                    }
                }
            }
        }
        if controls.share_images.load(Ordering::SeqCst) {
            if let Ok(image) = clipboard.get_image() {
                let dims_key = format!(
                    "{}x{}:{}",
                    image.width,
                    image.height,
                    blake3_hex(&image.bytes)
                );
                if last_image_hash.as_ref() != Some(&dims_key) {
                    last_image_hash = Some(dims_key);
                    match encode_png(image.width, image.height, &image.bytes) {
                        Ok(png) => {
                            if let Ok(payload) = ClipboardPayload::png(png) {
                                let _ = cmd_tx.send(Cmd::LocalClipboard(payload));
                            }
                        }
                        Err(error) => logbuf::log(format!("剪切板图片编码失败: {error:#}")),
                    }
                }
            }
        }
    }
}

fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>> {
    let image = image::RgbaImage::from_raw(width as u32, height as u32, rgba.to_vec())
        .ok_or_else(|| anyhow::anyhow!("图片尺寸与 RGBA 数据长度不匹配"))?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .context("PNG 编码失败")?;
    Ok(png)
}

// ---------------------------------------------------------------------------
// 一次性操作
// ---------------------------------------------------------------------------

async fn test_connection(relay_url: &str, access_token: &str) -> UiMsg {
    let identity = Identity::generate("gui-测试");
    match drive_client::Connection::connect(relay_url, &identity, access_token).await {
        Ok(connection) => {
            let info = connection.connection_info();
            connection.close().await;
            UiMsg::TestResult {
                ok: true,
                message: format!(
                    "连接成功：中继会话 {}，当前在线 {} 台设备",
                    &info.session_id,
                    info.online.len()
                ),
            }
        }
        Err(error) => UiMsg::TestResult {
            ok: false,
            message: format!("连接失败: {error:#}"),
        },
    }
}

fn catalog_add_offline(data_dir: &Path, path: &Path) -> Result<String> {
    let store = Store::open(data_dir)?;
    let identity = store.load_identity()?;
    let node = identity.device_id();
    let (size, hash) = drive_client::util::hash_file(path)?;
    let (mut catalog, mut revision) = store.load_catalog()?;
    if catalog.node.trim().is_empty() {
        catalog.node = node.clone();
    }
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "未命名".to_string());
    let entry = CatalogEntry::new(
        name,
        "",
        TargetPlatform::Any,
        "file",
        size,
        hash.clone(),
        Vec::new(),
        "",
        &node,
    );
    let now = now_ms();
    catalog.upsert_local(entry.clone(), &node, now);
    revision = revision.saturating_add(1);
    let mut index = store.load_local_index()?;
    index.insert(
        entry.id.clone(),
        LocalIndexEntry {
            path: path.to_path_buf(),
            content_hash: hash,
            size,
            updated_at_ms: now,
        },
    );
    store.save_catalog(&catalog, revision)?;
    store.save_local_index(&index)?;
    Ok(entry.id)
}

fn catalog_remove_offline(data_dir: &Path, id: &str) -> Result<()> {
    let store = Store::open(data_dir)?;
    let identity = store.load_identity()?;
    let node = identity.device_id();
    let (mut catalog, mut revision) = store.load_catalog()?;
    let mut index = store.load_local_index()?;
    if catalog.delete_local(id, &node, now_ms()).is_none() {
        anyhow::bail!("目录条目不存在或已删除");
    }
    revision = revision.saturating_add(1);
    index.remove(id);
    store.save_catalog(&catalog, revision)?;
    store.save_local_index(&index)?;
    Ok(())
}

fn update_sources(data_dir: &Path) -> (String, String) {
    let mut url = drive_updater::DEFAULT_UPDATE_URL.to_string();
    let mut pubkey = drive_updater::DEFAULT_UPDATE_PUBKEY_HEX.to_string();
    if let Ok(raw) = std::fs::read_to_string(data_dir.join("config.json")) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(candidate) = value
                .get("update_manifest_url")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|text| !text.is_empty())
            {
                url = candidate.to_string();
            }
            if let Some(candidate) = value
                .get("update_pubkey")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|text| !text.is_empty())
            {
                pubkey = candidate.to_string();
            }
        }
    }
    (url, pubkey)
}

fn check_update_blocking(data_dir: &Path) -> Result<UpdateInfo> {
    let (url, pubkey) = update_sources(data_dir);
    let manifest_json = drive_updater::fetch_text(&url, drive_updater::MAX_MANIFEST_FETCH_BYTES)?;
    let manifest = drive_updater::verify_manifest(&manifest_json, &pubkey)?;
    let current = env!("CARGO_PKG_VERSION").to_string();
    let ordering = drive_updater::compare_versions(&current, &manifest.version)?;
    let target = drive_updater::current_target();
    let has_asset = drive_updater::select_asset(&manifest, target).is_some();
    let available = ordering == std::cmp::Ordering::Less && has_asset;
    let reason = if ordering != std::cmp::Ordering::Less {
        "已是最新版本".to_string()
    } else if !has_asset {
        format!(
            "最新版本 {} 没有适用于当前平台（{target}）的产物",
            manifest.version
        )
    } else {
        "有新版本可用".to_string()
    };
    Ok(UpdateInfo {
        current,
        latest: manifest.version,
        notes: manifest.notes,
        available,
        reason,
    })
}

fn apply_update_blocking(data_dir: &Path) -> Result<String> {
    let (url, pubkey) = update_sources(data_dir);
    let manifest_json = drive_updater::fetch_text(&url, drive_updater::MAX_MANIFEST_FETCH_BYTES)?;
    let manifest = drive_updater::verify_manifest(&manifest_json, &pubkey)?;
    let target = drive_updater::current_target();
    let asset = drive_updater::select_asset(&manifest, target)
        .ok_or_else(|| anyhow::anyhow!("清单中没有适用于当前平台的产物（{target}）"))?;
    let cache_dir = drive_updater::update_cache_dir(data_dir);
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("创建更新缓存目录失败: {}", cache_dir.display()))?;
    let file_name = format!("drive-sync-gui-{}-{target}.bin", manifest.version);
    let download_path = cache_dir.join(file_name);
    drive_updater::download_to_file(
        &asset.url,
        &download_path,
        drive_updater::MAX_DOWNLOAD_BYTES,
    )?;
    drive_updater::verify_file(&download_path, &asset.blake3, asset.size)?;
    let current_exe = platform::current_exe()?;
    let report = drive_updater::install_binary(&download_path, &current_exe)?;
    Ok(format!(
        "已更新到 v{}（替换 {}，备份 {:?}）。{}",
        manifest.version,
        report.target_exe.display(),
        report.backup,
        drive_updater::restart_hint(&manifest.version)
    ))
}
