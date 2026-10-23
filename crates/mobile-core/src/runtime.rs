//! 移动端运行时封装：tokio 运行时 + drive-client `Runtime` + 事件/剪切板分发。
//!
//! 设计：
//! - 进程级单例（移动端只同步一个设备身份），`Inner` 由 `Arc` 共享给 FFI 句柄；
//! - 同步会话 = 一个多线程 tokio runtime + `drive_client::Runtime::run()` 任务；
//! - 事件统一走一个 `drive-sync-ui` 分发线程调用 C 回调，避免在 tokio worker 上直接
//!   做 JNI attach / 用户回调；
//! - 配对会临时暂停同步会话（中继对同一 device_id 的新连接会踢旧连接），配对结束后
//!   自动恢复（若配对前处于连接状态）。

use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_void, CString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use drive_client::{
    pair_initiate, pair_respond, PairOutcome, PairUi, Runtime, RuntimeConfig, RuntimeEvent,
    RuntimeHandle, Store,
};
use drive_core::clipboard::{ClipboardKind, ClipboardPayload};
use drive_core::identity::PublicIdentity;
use drive_core::pairing::QR_PREFIX;
use serde_json::{json, Value};

/// 移动端 ABI 版本（与 include/drive_mobile.h 的 DM_ABI_VERSION 一致）。
pub const DM_ABI_VERSION: i32 = 1;
/// 移动端核心版本。
pub const DM_VERSION: &str = env!("CARGO_PKG_VERSION");

/// C ABI：远端剪切板文本回调。
pub type ClipboardCallback = unsafe extern "C" fn(*const c_char, *const c_char, *mut c_void);
/// C ABI：事件回调（JSON）。
pub type EventCallback = unsafe extern "C" fn(*const c_char, *mut c_void);

// ---------------------------------------------------------------------------
// 设备上下文（进程级单例，dm_init/dm_load 设置）
// ---------------------------------------------------------------------------

/// 当前设备上下文：数据目录 + 身份信息。
#[derive(Clone, Debug)]
pub struct DeviceContext {
    pub data_dir: PathBuf,
    pub device_id: String,
    pub name: String,
}

static DEVICE: Mutex<Option<DeviceContext>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 设置进程级设备上下文（dm_init / dm_load）。
pub fn set_device(ctx: DeviceContext) {
    *lock(&DEVICE) = Some(ctx);
}

/// 读取当前设备上下文。
pub fn device() -> Option<DeviceContext> {
    lock(&DEVICE).clone()
}

/// 要求已初始化设备（否则返回可读错误）。
pub fn require_device() -> Result<DeviceContext> {
    device().ok_or_else(|| anyhow!("尚未初始化设备上下文，请先调用 dm_init(data_dir, device_name)"))
}

// ---------------------------------------------------------------------------
// C 回调注册
// ---------------------------------------------------------------------------

/// 注册给 C ABI 的回调集合（裸函数指针 + user_data，回调实现方保证线程安全）。
#[derive(Clone, Copy)]
pub struct Callbacks {
    pub clipboard: Option<ClipboardCallback>,
    pub event: Option<EventCallback>,
    pub user_data: *mut c_void,
}

// SAFETY: 函数指针与 user_data 由调用方（JNI/Swift 桥）保证线程安全；本库只在 UI
// 分发线程按顺序调用，不并发触达同一份回调上下文，具体同步由调用方负责。
unsafe impl Send for Callbacks {}
unsafe impl Sync for Callbacks {}

/// UI 分发消息。
enum UiMsg {
    Event(String),
    Clipboard { text: String, from: String },
}

fn call_event_cb(callbacks: Callbacks, json_text: &str) {
    let Some(func) = callbacks.event else {
        return;
    };
    // serde_json 会把 NUL 转义成 \u0000，正常不会出现真实的 0 字节。
    let Ok(text) = CString::new(json_text) else {
        return;
    };
    unsafe { func(text.as_ptr(), callbacks.user_data) };
}

fn call_clipboard_cb(callbacks: Callbacks, text: &str, from: &str) {
    let Some(func) = callbacks.clipboard else {
        return;
    };
    // 远端文本可能包含 NUL（合法 UTF-8 字符），C 字符串无法承载：替换为 U+FFFD。
    let sanitized = text.replace('\0', "\u{FFFD}");
    let Ok(c_text) = CString::new(sanitized) else {
        return;
    };
    let Ok(c_from) = CString::new(from) else {
        return;
    };
    unsafe { func(c_text.as_ptr(), c_from.as_ptr(), callbacks.user_data) };
}

// ---------------------------------------------------------------------------
// 会话状态
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct RelaySettings {
    relay_url: String,
    access_token: String,
}

/// 同步会话（一个 tokio runtime + 一个 Runtime 任务 + 一个剪切板泵线程）。
struct Session {
    rt: Option<tokio::runtime::Runtime>,
    handle: RuntimeHandle,
    clip_thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct RuntimeState {
    session: Option<Session>,
    /// presence 在线设备。
    online: HashSet<String>,
    /// 已建立 E2E 会话的设备。
    sessions: HashSet<String>,
    /// 设备 id → 展示名。
    peer_names: HashMap<String, String>,
    /// 设备 id → 对端持有条目 id 列表（PeerHaveList）。
    have_lists: HashMap<String, Vec<String>>,
    last_clipboard_from: String,
    last_clipboard_len: usize,
    pairing: bool,
}

/// 配对码信息。
#[derive(Clone, Debug)]
pub struct PairCodeInfo {
    pub code: String,
    pub qr: String,
}

/// 配对完成信息。
#[derive(Clone, Debug)]
pub struct PairPeerInfo {
    pub peer_id: String,
    pub name: String,
    pub sas: String,
}

// ---------------------------------------------------------------------------
// Inner：单例运行时
// ---------------------------------------------------------------------------

/// 进程级运行时单例的内部状态。
pub struct Inner {
    data_dir: PathBuf,
    device_id: String,
    device_name: Mutex<String>,
    callbacks: Callbacks,
    settings: Mutex<RelaySettings>,
    state: Mutex<RuntimeState>,
    session_lock: Mutex<()>,
    pair_lock: Mutex<()>,
    /// UI 分发线程的发送端（dm_start 时设置，dm_stop 时置 None 关闭线程）。
    ui_tx: Mutex<Option<mpsc::Sender<UiMsg>>>,
    dispatcher: Mutex<Option<std::thread::JoinHandle<()>>>,
    active: AtomicBool,
    /// 配对前是否处于连接状态（配对结束后自动恢复）。
    resume_after_pair: AtomicBool,
    paused: AtomicBool,
}

impl Inner {
    fn new(device: DeviceContext, settings: RelaySettings, callbacks: Callbacks) -> Arc<Self> {
        Arc::new(Inner {
            data_dir: device.data_dir,
            device_id: device.device_id,
            device_name: Mutex::new(device.name),
            callbacks,
            settings: Mutex::new(settings),
            state: Mutex::new(RuntimeState::default()),
            session_lock: Mutex::new(()),
            pair_lock: Mutex::new(()),
            ui_tx: Mutex::new(None),
            dispatcher: Mutex::new(None),
            active: AtomicBool::new(true),
            resume_after_pair: AtomicBool::new(false),
            paused: AtomicBool::new(false),
        })
    }

    /// 创建单例并启动 UI 分发线程 + 同步会话。
    pub fn start(
        device: DeviceContext,
        relay_override: Option<&str>,
        token_override: Option<&str>,
        callbacks: Callbacks,
    ) -> Result<Arc<Self>> {
        let settings = resolve_settings(&device.data_dir, relay_override, token_override)?;
        let inner = Inner::new(device, settings, callbacks);
        inner.spawn_dispatcher()?;
        if let Err(error) = inner.start_session() {
            inner.shutdown_dispatcher();
            return Err(error);
        }
        let name = inner.name();
        inner.emit_event(json!({
            "type": "started",
            "device_id": inner.device_id,
            "name": name,
        }));
        Ok(inner)
    }

    fn name(&self) -> String {
        lock(&self.device_name).clone()
    }

    /// 会话是否处于活动状态（dm_stop 后为 false）。
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// 停止会话并关闭 UI 分发线程（dm_stop 调用）。
    pub(crate) fn shutdown(self: &Arc<Self>) {
        self.active.store(false, Ordering::SeqCst);
        self.stop_session();
        self.shutdown_dispatcher();
    }

    // -----------------------------------------------------------------
    // UI 分发线程
    // -----------------------------------------------------------------

    fn spawn_dispatcher(self: &Arc<Self>) -> Result<()> {
        let (tx, rx) = mpsc::channel::<UiMsg>();
        *lock(&self.ui_tx) = Some(tx);
        let callbacks = self.callbacks;
        let handle = std::thread::Builder::new()
            .name("drive-sync-ui".into())
            .spawn(move || {
                while let Ok(msg) = rx.recv() {
                    match msg {
                        UiMsg::Event(text) => call_event_cb(callbacks, &text),
                        UiMsg::Clipboard { text, from } => {
                            call_clipboard_cb(callbacks, &text, &from)
                        }
                    }
                }
            })
            .context("创建 UI 分发线程失败")?;
        *lock(&self.dispatcher) = Some(handle);
        Ok(())
    }

    fn shutdown_dispatcher(&self) {
        *lock(&self.ui_tx) = None;
        if let Some(handle) = lock(&self.dispatcher).take() {
            let _ = handle.join();
        }
    }

    fn send_ui(&self, msg: UiMsg) {
        let guard = lock(&self.ui_tx);
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(msg);
        }
    }

    /// 发送一个 JSON 事件（非阻塞；UI 分发线程按序回调）。
    pub fn emit_event(&self, value: Value) {
        self.send_ui(UiMsg::Event(value.to_string()));
    }

    // -----------------------------------------------------------------
    // 同步会话生命周期
    // -----------------------------------------------------------------

    fn start_session(self: &Arc<Self>) -> Result<()> {
        let _guard = lock(&self.session_lock);
        if lock(&self.state).session.is_some() {
            return Ok(());
        }
        if !self.active.load(Ordering::SeqCst) {
            bail!("同步会话已停止");
        }
        let settings = lock(&self.settings).clone();
        if settings.relay_url.trim().is_empty() {
            bail!("未配置中继地址（relay_url）");
        }
        let mut config = RuntimeConfig::new(
            &self.data_dir,
            settings.relay_url.clone(),
            settings.access_token.clone(),
        );
        config.device_name = None;
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<RuntimeEvent>();
        let (clip_tx, clip_rx) = std::sync::mpsc::channel::<ClipboardPayload>();
        config.event_tx = Some(event_tx);
        config.remote_clipboard_tx = Some(clip_tx);

        let (runtime, handle) = Runtime::open(config).context("打开同步运行时失败")?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("drive-sync-net")
            .build()
            .context("创建 tokio 运行时失败")?;

        let inner_ev = self.clone();
        rt.spawn(async move {
            while let Some(event) = event_rx.recv().await {
                inner_ev.on_runtime_event(event);
            }
        });
        let inner_clip = self.clone();
        let clip_thread = std::thread::Builder::new()
            .name("drive-sync-clip".into())
            .spawn(move || {
                while let Ok(payload) = clip_rx.recv() {
                    inner_clip.on_remote_clipboard(payload);
                }
            })
            .context("创建剪切板线程失败")?;
        rt.spawn(runtime.run());

        let mut state = lock(&self.state);
        state.session = Some(Session {
            rt: Some(rt),
            handle,
            clip_thread: Some(clip_thread),
        });
        state.online.clear();
        state.sessions.clear();
        state.have_lists.clear();
        drop(state);
        Ok(())
    }

    fn stop_session(&self) {
        let _guard = lock(&self.session_lock);
        let session = {
            let mut state = lock(&self.state);
            state.session.take()
        };
        let Some(mut session) = session else {
            return;
        };
        session.handle.shutdown();
        if let Some(rt) = session.rt.take() {
            rt.shutdown_timeout(Duration::from_secs(3));
        }
        if let Some(thread) = session.clip_thread.take() {
            let _ = thread.join();
        }
        {
            let mut state = lock(&self.state);
            state.online.clear();
            state.sessions.clear();
            state.have_lists.clear();
            state.last_clipboard_from.clear();
        }
        self.emit_event(json!({"type": "stopped"}));
    }

    /// 暂停同步会话（配对用），返回此前是否在运行。
    fn pause_for_pairing(&self) -> bool {
        let running = {
            let state = lock(&self.state);
            state.session.is_some()
        };
        if running {
            self.stop_session();
        }
        self.resume_after_pair.store(running, Ordering::SeqCst);
        running
    }

    /// 配对结束：更新状态并按需恢复同步会话。
    fn finish_pairing(self: &Arc<Self>) {
        {
            let mut state = lock(&self.state);
            state.pairing = false;
        }
        let active = self.active.load(Ordering::SeqCst);
        let resume = self.resume_after_pair.swap(false, Ordering::SeqCst);
        if !active || !resume {
            return;
        }
        // 给中继一点时间清理配对连接，避免与恢复的连接互相踢下线。
        std::thread::sleep(Duration::from_millis(600));
        if !self.active.load(Ordering::SeqCst) {
            return;
        }
        if let Err(error) = self.start_session() {
            self.emit_event(json!({
                "type": "error",
                "message": format!("配对后恢复连接失败: {error}"),
            }));
        }
    }

    // -----------------------------------------------------------------
    // 事件处理
    // -----------------------------------------------------------------

    fn on_runtime_event(&self, event: RuntimeEvent) {
        {
            let mut state = lock(&self.state);
            match &event {
                RuntimeEvent::Authenticated { online, .. } => {
                    state.online = online.iter().cloned().collect();
                    state.sessions.clear();
                }
                RuntimeEvent::Presence { online } => {
                    state.online = online.iter().cloned().collect();
                }
                RuntimeEvent::PeerOnline { peer_id, name } => {
                    state.online.insert(peer_id.clone());
                    state.peer_names.insert(peer_id.clone(), name.clone());
                }
                RuntimeEvent::PeerOffline { peer_id } => {
                    state.online.remove(peer_id);
                    state.sessions.remove(peer_id);
                }
                RuntimeEvent::SessionEstablished { peer_id, name } => {
                    state.sessions.insert(peer_id.clone());
                    state.online.insert(peer_id.clone());
                    state.peer_names.insert(peer_id.clone(), name.clone());
                }
                RuntimeEvent::SessionClosed { peer_id } => {
                    state.sessions.remove(peer_id);
                }
                RuntimeEvent::PeerHaveList { peer_id, entry_ids } => {
                    state.have_lists.insert(peer_id.clone(), entry_ids.clone());
                }
                RuntimeEvent::ClipboardApplied { peer_id, size, .. } => {
                    state.last_clipboard_from = peer_id.clone();
                    state.last_clipboard_len = *size as usize;
                }
                RuntimeEvent::PeerRevoked { device_id, .. }
                | RuntimeEvent::PeerUnpaired { device_id, .. } => {
                    state.online.remove(device_id);
                    state.sessions.remove(device_id);
                    state.have_lists.remove(device_id);
                }
                _ => {}
            }
        }
        if let Some(value) = runtime_event_json(&event) {
            self.emit_event(value);
        }
    }

    fn on_remote_clipboard(&self, payload: ClipboardPayload) {
        if self.paused.load(Ordering::SeqCst) {
            self.emit_event(json!({
                "type": "clipboard_applied",
                "peer_id": "",
                "kind": "paused",
                "size": payload.raw_size,
            }));
            return;
        }
        let text = match payload.kind {
            ClipboardKind::Text | ClipboardKind::RichText => match payload.as_text() {
                Ok(text) => text,
                Err(error) => {
                    self.emit_event(json!({
                        "type": "error",
                        "message": format!("解码远端剪切板文本失败: {error}"),
                    }));
                    return;
                }
            },
            // 图片/文件列表：原型阶段不写系统剪贴板（事件已由引擎发出）。
            _ => return,
        };
        let from = {
            let state = lock(&self.state);
            state.last_clipboard_from.clone()
        };
        self.send_ui(UiMsg::Clipboard { text, from });
    }

    // -----------------------------------------------------------------
    // 对外操作
    // -----------------------------------------------------------------

    pub(crate) fn send_clipboard(&self, payload: ClipboardPayload) -> i32 {
        if self.paused.load(Ordering::SeqCst) {
            return -3;
        }
        let state = lock(&self.state);
        let Some(session) = state.session.as_ref() else {
            return -3;
        };
        let Some(rt) = session.rt.as_ref() else {
            return -3;
        };
        let handle = session.handle.clone();
        rt.spawn(async move {
            let _ = handle.set_local_clipboard(payload).await;
        });
        0
    }

    fn pair_config(&self) -> Result<RuntimeConfig> {
        let settings = lock(&self.settings).clone();
        if settings.relay_url.trim().is_empty() {
            bail!("未配置中继地址（relay_url）");
        }
        let mut config =
            RuntimeConfig::new(&self.data_dir, settings.relay_url, settings.access_token);
        config.device_name = None;
        Ok(config)
    }

    /// 发起配对：阻塞到配对码生成后返回，配对过程在后台线程继续。
    pub(crate) fn pair_start(self: &Arc<Self>) -> Result<PairCodeInfo> {
        if !self.active.load(Ordering::SeqCst) {
            bail!("同步会话已停止");
        }
        let _guard = lock(&self.pair_lock);
        let config = self.pair_config()?;
        self.pause_for_pairing();
        {
            let mut state = lock(&self.state);
            state.pairing = true;
        }

        let (code_tx, code_rx) = mpsc::channel::<(String, String)>();
        let inner = self.clone();
        let ui_inner = self.clone();
        std::thread::Builder::new()
            .name("drive-sync-pair".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(error) => {
                        inner.emit_event(json!({
                            "type": "pair_failed",
                            "message": format!("创建配对运行时失败: {error}"),
                        }));
                        inner.finish_pairing();
                        return;
                    }
                };
                let ui = PairUiBridge::new(ui_inner, Some(code_tx));
                let result = rt.block_on(pair_initiate(&config, &ui));
                match result {
                    Ok(outcome) => emit_pair_succeeded(&inner, &outcome),
                    Err(error) => {
                        inner.emit_event(json!({
                            "type": "pair_failed",
                            "message": error.to_string(),
                        }));
                    }
                }
                inner.finish_pairing();
            })
            .map_err(|error| anyhow!("创建配对线程失败: {error}"))?;

        match code_rx.recv_timeout(Duration::from_secs(30)) {
            Ok((code, qr)) => Ok(PairCodeInfo { code, qr }),
            Err(_) => bail!("等待配对码超时（中继不可达或 token 错误？）"),
        }
    }

    /// 加入配对：阻塞到配对完成并返回结果。
    pub(crate) fn pair_join(self: &Arc<Self>, input: &str) -> Result<PairPeerInfo> {
        if !self.active.load(Ordering::SeqCst) {
            bail!("同步会话已停止");
        }
        let input = input.trim().to_string();
        if input.is_empty() {
            bail!("配对码/扫码串不能为空");
        }
        let _guard = lock(&self.pair_lock);
        let config = self.pair_config()?;
        self.pause_for_pairing();
        {
            let mut state = lock(&self.state);
            state.pairing = true;
        }

        let (done_tx, done_rx) = mpsc::channel::<std::result::Result<PairPeerInfo, String>>();
        let inner = self.clone();
        let ui_inner = self.clone();
        std::thread::Builder::new()
            .name("drive-sync-pair".into())
            .spawn(move || {
                let outcome: std::result::Result<PairPeerInfo, String> =
                    match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => {
                            let ui = PairUiBridge::new(ui_inner, None);
                            let result = rt.block_on(async {
                                if input.starts_with(QR_PREFIX) {
                                    pair_respond(&config, Some(&input), None, &ui).await
                                } else {
                                    pair_respond(&config, None, Some(&input), &ui).await
                                }
                            });
                            match result {
                                Ok(outcome) => {
                                    emit_pair_succeeded(&inner, &outcome);
                                    Ok(PairPeerInfo {
                                        peer_id: outcome.record.peer.device_id.clone(),
                                        name: outcome.record.peer.name.clone(),
                                        sas: outcome.sas.clone(),
                                    })
                                }
                                Err(error) => {
                                    let message = error.to_string();
                                    inner.emit_event(json!({
                                        "type": "pair_failed",
                                        "message": message,
                                    }));
                                    Err(message)
                                }
                            }
                        }
                        Err(error) => {
                            let message = format!("创建配对运行时失败: {error}");
                            inner.emit_event(json!({
                                "type": "pair_failed",
                                "message": message,
                            }));
                            Err(message)
                        }
                    };
                let _ = done_tx.send(outcome);
                inner.finish_pairing();
            })
            .map_err(|error| anyhow!("创建配对线程失败: {error}"))?;

        match done_rx.recv_timeout(Duration::from_secs(140)) {
            Ok(Ok(info)) => Ok(info),
            Ok(Err(message)) => bail!("{message}"),
            Err(_) => bail!("等待配对结果超时"),
        }
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
        self.emit_event(json!({"type": "paused", "paused": paused}));
    }

    // -----------------------------------------------------------------
    // 查询（读取磁盘状态，供 UI 展示）
    // -----------------------------------------------------------------

    pub(crate) fn peers_value(&self) -> Result<Value> {
        let store = Store::open(&self.data_dir)?;
        let peers = store.load_peers()?;
        let state = lock(&self.state);
        let list: Vec<Value> = peers
            .iter()
            .map(|record| {
                let peer_id = record.peer.device_id.clone();
                json!({
                    "device_id": peer_id,
                    "name": record.peer.name,
                    "label": record.display_name(),
                    "relay_url": record.relay_url,
                    "paired_at_ms": record.paired_at_ms,
                    "last_seen_ms": record.last_seen_or_paired(),
                    "online": state.online.contains(&record.peer.device_id),
                    "session": state.sessions.contains(&record.peer.device_id),
                })
            })
            .collect();
        Ok(json!({
            "ok": true,
            "self": {"device_id": self.device_id, "name": self.name()},
            "online_count": state.online.len(),
            "peers": list,
        }))
    }

    pub(crate) fn catalog_value(&self) -> Result<Value> {
        let store = Store::open(&self.data_dir)?;
        let (catalog, revision) = store.load_catalog()?;
        let local_index = store.load_local_index()?;
        let state = lock(&self.state);
        let mut entries: Vec<Value> = catalog
            .visible()
            .map(|entry| {
                let mut holders: Vec<String> = state
                    .have_lists
                    .iter()
                    .filter(|(_, ids)| ids.iter().any(|id| id == &entry.id))
                    .map(|(peer_id, _)| peer_id.clone())
                    .collect();
                holders.sort();
                json!({
                    "id": entry.id,
                    "name": entry.name,
                    "version": entry.version,
                    "platform": entry.platform.as_str(),
                    "kind": entry.kind,
                    "size": entry.size,
                    "tags": entry.tags,
                    "notes": entry.notes,
                    "updated_at_ms": entry.updated_at_ms,
                    "updated_by": entry.updated_by,
                    "content_hash": entry.content_hash,
                    "local": local_index.contains_key(&entry.id),
                    "holders": holders,
                })
            })
            .collect();
        entries.sort_by(|left, right| {
            left.get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .cmp(
                    right
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )
        });
        Ok(json!({
            "ok": true,
            "revision": revision,
            "count": entries.len(),
            "entries": entries,
        }))
    }

    pub(crate) fn status_value(&self) -> Value {
        let state = lock(&self.state);
        let settings = lock(&self.settings).clone();
        json!({
            "ok": true,
            "running": state.session.is_some(),
            "active": self.active.load(Ordering::SeqCst),
            "paused": self.paused.load(Ordering::SeqCst),
            "device_id": self.device_id,
            "name": self.name(),
            "relay_url": settings.relay_url,
            "has_token": !settings.access_token.is_empty(),
            "data_dir": self.data_dir.display().to_string(),
            "peers_online": state.online.len(),
            "sessions": state.sessions.len(),
            "pairing": state.pairing,
            "last_clipboard_from": state.last_clipboard_from,
            "last_clipboard_len": state.last_clipboard_len,
        })
    }
}

// ---------------------------------------------------------------------------
// 配对 UI 桥
// ---------------------------------------------------------------------------

/// 把 drive-client `PairUi` 回调桥接成事件 + 配对码通道。
struct PairUiBridge {
    inner: Arc<Inner>,
    code: Mutex<Option<String>>,
    qr: Mutex<Option<String>>,
    sas: Mutex<Option<String>>,
    peer: Mutex<Option<(String, String)>>,
    code_tx: Mutex<Option<mpsc::Sender<(String, String)>>>,
}

impl PairUiBridge {
    fn new(inner: Arc<Inner>, code_tx: Option<mpsc::Sender<(String, String)>>) -> Self {
        Self {
            inner,
            code: Mutex::new(None),
            qr: Mutex::new(None),
            sas: Mutex::new(None),
            peer: Mutex::new(None),
            code_tx: Mutex::new(code_tx),
        }
    }

    fn publish_code(&self) {
        let code = lock(&self.code).clone().unwrap_or_default();
        let qr = lock(&self.qr).clone().unwrap_or_default();
        self.inner.emit_event(json!({
            "type": "pair_code",
            "code": code,
            "qr": qr,
        }));
        let sender = lock(&self.code_tx).clone();
        if let Some(tx) = sender {
            let _ = tx.send((code, qr));
        }
    }
}

impl PairUi for PairUiBridge {
    fn on_qr(&self, qr: &str) {
        *lock(&self.qr) = Some(qr.to_string());
    }

    fn on_code(&self, code: &str) {
        *lock(&self.code) = Some(code.to_string());
        self.publish_code();
    }

    fn on_peer(&self, peer: &PublicIdentity) {
        *lock(&self.peer) = Some((peer.device_id.clone(), peer.name.clone()));
        self.inner.emit_event(json!({
            "type": "pair_peer",
            "peer_id": peer.device_id,
            "name": peer.name,
        }));
    }

    fn on_sas(&self, sas: &str) {
        *lock(&self.sas) = Some(sas.to_string());
    }

    fn confirm_sas(&self, _sas: &str) -> bool {
        // 原型阶段：移动端自动确认；SAS 通过 pair_succeeded 事件展示，用户可肉眼核对。
        true
    }
}

fn emit_pair_succeeded(inner: &Arc<Inner>, outcome: &PairOutcome) {
    let peer = &outcome.record.peer;
    inner.emit_event(json!({
        "type": "pair_succeeded",
        "peer_id": peer.device_id,
        "name": peer.name,
        "sas": outcome.sas,
        "relay_url": outcome.record.relay_url,
    }));
}

// ---------------------------------------------------------------------------
// 事件 → JSON
// ---------------------------------------------------------------------------

#[allow(unreachable_patterns)]
fn runtime_event_json(event: &RuntimeEvent) -> Option<Value> {
    use RuntimeEvent as E;
    let value = match event {
        E::Authenticated { device_id, online } => {
            json!({"type": "connected", "device_id": device_id, "online": online})
        }
        E::Presence { online } => json!({"type": "presence", "online": online}),
        E::PeerOnline { peer_id, name } => {
            json!({"type": "peer_online", "peer_id": peer_id, "name": name})
        }
        E::PeerOffline { peer_id } => json!({"type": "peer_offline", "peer_id": peer_id}),
        E::SessionEstablished { peer_id, name } => {
            json!({"type": "session_established", "peer_id": peer_id, "name": name})
        }
        E::SessionClosed { peer_id } => json!({"type": "session_closed", "peer_id": peer_id}),
        E::ClipboardCaptured { kind, size } => {
            json!({"type": "clipboard_captured", "kind": kind, "size": size})
        }
        E::ClipboardSent { peers, kind, size } => {
            json!({"type": "clipboard_sent", "peers": peers, "kind": kind, "size": size})
        }
        E::ClipboardApplied {
            peer_id,
            kind,
            size,
        } => {
            json!({"type": "clipboard_applied", "peer_id": peer_id, "kind": kind, "size": size})
        }
        E::ClipboardSpooled { peers } => json!({"type": "clipboard_spooled", "peers": peers}),
        E::SpoolReplayed { peer_id, count } => {
            json!({"type": "spool_replayed", "peer_id": peer_id, "count": count})
        }
        E::CatalogUpdated { entry_id, ops } => {
            json!({"type": "catalog_updated", "entry_id": entry_id, "ops": ops})
        }
        E::CatalogMerged {
            from,
            applied,
            ignored,
            conflicts,
        } => json!({
            "type": "catalog_merged",
            "from": from,
            "applied": applied,
            "ignored": ignored,
            "conflicts": conflicts,
        }),
        E::CatalogReloaded { entries } => json!({"type": "catalog_reloaded", "entries": entries}),
        E::PeerHaveList { peer_id, entry_ids } => {
            json!({"type": "peer_have_list", "peer_id": peer_id, "entry_ids": entry_ids})
        }
        E::TransferOffered {
            peer_id,
            entry_id,
            size,
            transfer_id,
        } => json!({
            "type": "transfer_offered",
            "peer_id": peer_id,
            "entry_id": entry_id,
            "size": size,
            "transfer_id": transfer_id,
        }),
        E::TransferStarted {
            transfer_id,
            entry_id,
            from_offset,
            total,
        } => json!({
            "type": "transfer_started",
            "transfer_id": transfer_id,
            "entry_id": entry_id,
            "from_offset": from_offset,
            "total": total,
        }),
        E::TransferProgress {
            transfer_id,
            received,
            total,
        } => json!({
            "type": "transfer_progress",
            "transfer_id": transfer_id,
            "received": received,
            "total": total,
        }),
        E::TransferReceived {
            transfer_id,
            entry_id,
            path,
            size,
        } => json!({
            "type": "transfer_received",
            "transfer_id": transfer_id,
            "entry_id": entry_id,
            "path": path.display().to_string(),
            "size": size,
        }),
        E::TransferFailed {
            transfer_id,
            message,
        } => json!({
            "type": "transfer_failed",
            "transfer_id": transfer_id,
            "message": message,
        }),
        E::VaultShared { peer_id, object_id } => {
            json!({"type": "vault_shared", "peer_id": peer_id, "object_id": object_id})
        }
        E::VaultKeyReceived { from, object_id } => {
            json!({"type": "vault_key_received", "from": from, "object_id": object_id})
        }
        E::PeersReloaded { count } => json!({"type": "peers_reloaded", "count": count}),
        E::PeerRevoked {
            device_id,
            name,
            reason,
            notified,
        } => json!({
            "type": "peer_revoked",
            "device_id": device_id,
            "name": name,
            "reason": reason,
            "notified": notified,
        }),
        E::PeerUnpaired {
            device_id,
            name,
            reason,
        } => json!({
            "type": "peer_unpaired",
            "device_id": device_id,
            "name": name,
            "reason": reason,
        }),
        E::RelayError { code, message } => {
            json!({"type": "relay_error", "code": code, "message": message})
        }
        E::Error { message } => json!({"type": "error", "message": message}),
        E::Stopped => json!({"type": "stopped"}),
        // 未来 client-core 新增事件：降级成 summary 字符串，避免移动端崩溃。
        _ => json!({"type": "event", "summary": event.summary()}),
    };
    Some(value)
}

// ---------------------------------------------------------------------------
// 配置解析
// ---------------------------------------------------------------------------

fn resolve_settings(
    data_dir: &std::path::Path,
    relay_override: Option<&str>,
    token_override: Option<&str>,
) -> Result<RelaySettings> {
    let store = Store::open(data_dir).context("打开数据目录失败")?;
    let mut config = store.load_config().unwrap_or_default();
    let mut changed = false;
    if let Some(relay) = relay_override {
        let relay = relay.trim();
        if !relay.is_empty() && relay != config.relay_url {
            config.relay_url = relay.to_string();
            changed = true;
        }
    }
    if let Some(token) = token_override {
        if !token.is_empty() && token != config.access_token {
            config.access_token = token.to_string();
            changed = true;
        }
    }
    if config.relay_url.trim().is_empty() {
        bail!("未配置中继地址：请先调用 dm_config_set_relay 或启动时传入 relay_url");
    }
    if changed {
        store.save_config(&config).context("保存中继配置失败")?;
    }
    Ok(RelaySettings {
        relay_url: config.relay_url,
        access_token: config.access_token,
    })
}
