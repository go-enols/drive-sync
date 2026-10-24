//! GUI 主应用：首次运行向导 + PolyBot 风格主界面（状态/设备/剪切板/目录/暂存/设置/日志）。
//!
//! UI 线程只做渲染与轻量文件读写；所有网络与同步逻辑通过 [`backend::Backend`] 的后台线程完成。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use drive_client::{LocalIndexEntry, RuntimeEvent, Store, VaultKey};
use drive_core::catalog::{Catalog, CatalogEntry};
use drive_core::clipboard::{ClipboardKind, ClipboardPayload};
use drive_core::pairing::PairRecord;

use crate::backend::{
    Backend, ClipboardControls, Cmd, PairInput, PairMode, PairProgress, UiMsg, UpdateInfo,
    VaultItem,
};
use crate::logbuf;
use crate::platform;
use crate::screenshots;
use crate::theme;
use crate::theme::{GuiSettings, ThemeMode};

/// 向导预填中继地址（示例/默认值；token 需用户自行填写）。
pub const RELAY_PLACEHOLDER: &str = "wss://sync.likecloud.top/ws";
const EVENT_LIMIT: usize = 400;
const CLIPBOARD_RECORD_LIMIT: usize = 80;
const TRANSFER_LIMIT: usize = 80;

// ---------------------------------------------------------------------------
// 页面 / 屏幕
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Status,
    Devices,
    Clipboard,
    Catalog,
    Vault,
    Settings,
    Logs,
}

impl Page {
    const ALL: [Page; 7] = [
        Page::Status,
        Page::Devices,
        Page::Clipboard,
        Page::Catalog,
        Page::Vault,
        Page::Settings,
        Page::Logs,
    ];

    fn label(self) -> &'static str {
        match self {
            Page::Status => "状态",
            Page::Devices => "设备",
            Page::Clipboard => "剪切板",
            Page::Catalog => "软件目录",
            Page::Vault => "云端暂存",
            Page::Settings => "设置",
            Page::Logs => "日志",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Page::Status => "连接状态、在线设备与最近事件",
            Page::Devices => "长期有效的配对设备；90 天不活跃自动失效",
            Page::Clipboard => "文本 / 图片自动同步，远端内容只显示类型与大小",
            Page::Catalog => "只同步元数据，本体留在持有设备本地并按需加密传输",
            Page::Vault => "客户端随机密钥加密上传，服务端只存密文，TTL ≤ 30 天",
            Page::Settings => "中继、设备、自启、更新与数据目录",
            Page::Logs => "运行日志（不含剪切板/文件明文）",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SyncState {
    Stopped,
    Starting,
    Running { device_id: String },
    Error { message: String },
}

impl SyncState {
    fn is_running(&self) -> bool {
        matches!(self, SyncState::Running { .. } | SyncState::Starting)
    }

    fn label(&self) -> &str {
        match self {
            SyncState::Stopped => "未同步",
            SyncState::Starting => "正在连接…",
            SyncState::Running { .. } => "同步中",
            SyncState::Error { .. } => "连接异常",
        }
    }

    fn color(&self) -> egui::Color32 {
        match self {
            SyncState::Running { .. } => theme::mint(),
            SyncState::Starting => theme::warn(),
            SyncState::Stopped => theme::muted_color(),
            SyncState::Error { .. } => theme::danger(),
        }
    }
}

// ---------------------------------------------------------------------------
// 配对面板 / 向导
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct PairPanel {
    pub mode: Option<PairMode>,
    pub started: bool,
    pub progress: PairProgress,
    pub join_code: String,
    pub join_qr: String,
    pub finished: Option<(bool, String)>,
    pub visible_modal: bool,
}

pub struct WizardState {
    pub step: usize,
    pub name: String,
    pub relay: String,
    pub token: String,
    pub test: Option<(bool, String)>,
    pub test_pending: bool,
    pub pair: PairPanel,
    pub skip_pair: bool,
    pub error: Option<String>,
}

struct SettingsState {
    name: String,
    relay: String,
    token: String,
    poll_ms: u64,
    share_text: bool,
    share_images: bool,
    autostart: bool,
    update: Option<UpdateInfo>,
    update_message: Option<String>,
    checking_update: bool,
    loaded: bool,
}

impl Default for SettingsState {
    fn default() -> Self {
        Self {
            name: String::new(),
            relay: String::new(),
            token: String::new(),
            poll_ms: 300,
            share_text: true,
            share_images: true,
            autostart: false,
            update: None,
            update_message: None,
            checking_update: false,
            loaded: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Toast {
    text: String,
    ok: bool,
}

enum Dialog {
    None,
    RemovePeer { peer_id: String, name: String },
    RemoveCatalog { id: String, name: String },
    About,
}

// ---------------------------------------------------------------------------
// 截图计划（仅本机验收用）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum ShotAction {
    WizardStep(usize),
    Page(Page),
}

#[derive(Clone, Debug)]
pub struct ShotStep {
    pub label: String,
    pub action: ShotAction,
}

pub struct ShotPlan {
    pub dir: PathBuf,
    pub steps: Vec<ShotStep>,
    pub index: usize,
    action_applied: bool,
    shot_sent: bool,
    frames: u32,
}

impl ShotPlan {
    pub fn new(dir: PathBuf, steps: Vec<ShotStep>) -> Self {
        Self {
            dir,
            steps,
            index: 0,
            action_applied: false,
            shot_sent: false,
            frames: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// 主应用
// ---------------------------------------------------------------------------

/// 长耗时操作的顶部常驻指示（spinner/进度条），避免用户以为界面卡死。
struct BusyState {
    label: String,
    started: Instant,
    done: u64,
    total: Option<u64>,
}

pub struct GuiApp {
    data_dir: PathBuf,
    _instance_guard: Option<platform::InstanceGuard>,
    cmd_tx: Sender<Cmd>,
    backend: Option<Backend>,
    controls: ClipboardControls,
    remote_clip_rx: Receiver<ClipboardPayload>,
    remote_clip_tx: Sender<ClipboardPayload>,
    clipboard: Option<arboard::Clipboard>,

    screen_wizard: bool,
    wizard: WizardState,
    page: Page,
    settings: SettingsState,

    sync_state: SyncState,
    events: VecDeque<String>,
    online: BTreeSet<String>,
    sessions: BTreeSet<String>,
    clipboard_records: VecDeque<String>,
    transfer_lines: VecDeque<String>,
    peer_have: BTreeMap<String, Vec<String>>,
    vault_items: Vec<VaultItem>,

    peers: Vec<PairRecord>,
    catalog: Catalog,
    catalog_revision: u64,
    local_index: BTreeMap<String, LocalIndexEntry>,
    vault_keys: Vec<VaultKey>,
    last_refresh: Instant,

    pair: PairPanel,
    dialog: Dialog,
    toast: Option<Toast>,
    toast_at: Instant,
    busy: Option<BusyState>,
    shots: Option<ShotPlan>,
    catalog_path_input: String,
    vault_upload_path: String,
    vault_ttl_days: u64,
    vault_download_dir: String,
    auto_pair_started: bool,
    pair_auto_confirmed_for: Option<String>,

    /// 外观：跟随系统 / 深色 / 浅色（持久化到 `<data-dir>/gui.json`）。
    theme_mode: ThemeMode,
    system_light: Option<bool>,
    applied_light: bool,
    last_system_probe: Instant,

    /// 启动与窗口行为（gui.json）。
    auto_start_sync: bool,
    hide_to_tray: bool,
    tray: Option<crate::tray::Tray>,
    tray_hwnd: std::sync::Arc<std::sync::atomic::AtomicIsize>,
    quitting: bool,
    was_minimized: bool,
}

impl GuiApp {
    /// 参数较多：窗口/数据目录/后台句柄/剪切板通道/截图计划/单实例锁。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        data_dir: PathBuf,
        backend: Backend,
        remote_clip_rx: Receiver<ClipboardPayload>,
        remote_clip_tx: Sender<ClipboardPayload>,
        shots: Option<ShotPlan>,
        shot_mode: Option<String>,
        instance_guard: Option<platform::InstanceGuard>,
    ) -> Self {
        install_fonts_and_style(&cc.egui_ctx);

        let controls = backend.controls.clone();
        let cmd_tx = backend.cmd_tx.clone();
        let store = Store::open(&data_dir).ok();
        let mut config = store
            .as_ref()
            .and_then(|store| store.load_config().ok())
            .unwrap_or_default();
        let identity = store.as_ref().and_then(|store| store.load_identity().ok());
        let device_name = identity
            .as_ref()
            .map(|identity| identity.name.clone())
            .unwrap_or_else(|| "我的设备".to_string());

        let force_wizard = shot_mode.as_deref() == Some("wizard");
        let force_main = shot_mode.as_deref() == Some("main");
        let needs_wizard = !force_main
            && (identity.is_none()
                || config.relay_url.trim().is_empty()
                || config.access_token.trim().is_empty());

        if config.relay_url.trim().is_empty() {
            config.relay_url = RELAY_PLACEHOLDER.to_string();
        }

        // 外观主题：环境变量（验收截图用）> gui.json > 跟随系统。
        let gui_settings = theme::load_gui_settings(&data_dir);
        let mut theme_mode = gui_settings.theme;
        if let Ok(forced) = std::env::var("DRIVE_GUI_SHOT_THEME") {
            match forced.to_lowercase().as_str() {
                "dark" => theme_mode = ThemeMode::Dark,
                "light" => theme_mode = ThemeMode::Light,
                _ => {}
            }
        }
        let system_light = platform::system_prefers_light();
        // 自动开始同步：环境变量测试覆盖 > gui.json（默认开）。
        let mut auto_start_sync = gui_settings.auto_start_sync;
        if let Ok(forced) = std::env::var("DRIVE_GUI_AUTOSTART_SYNC") {
            match forced.trim() {
                "0" | "false" => auto_start_sync = false,
                "1" | "true" => auto_start_sync = true,
                _ => {}
            }
        }
        let hide_to_tray = gui_settings.hide_to_tray && crate::tray::supported();

        let wizard = WizardState {
            step: 0,
            name: device_name,
            relay: config.relay_url.clone(),
            token: config.access_token.clone(),
            test: None,
            test_pending: false,
            pair: PairPanel::default(),
            skip_pair: false,
            error: None,
        };

        let mut app = Self {
            data_dir: data_dir.clone(),
            _instance_guard: instance_guard,
            cmd_tx,
            backend: Some(backend),
            controls,
            remote_clip_rx,
            remote_clip_tx,
            clipboard: None,
            screen_wizard: force_wizard || (!force_main && needs_wizard),
            wizard,
            page: Page::Status,
            settings: SettingsState::default(),
            sync_state: SyncState::Stopped,
            events: VecDeque::new(),
            online: BTreeSet::new(),
            sessions: BTreeSet::new(),
            clipboard_records: VecDeque::new(),
            transfer_lines: VecDeque::new(),
            peer_have: BTreeMap::new(),
            vault_items: Vec::new(),
            peers: Vec::new(),
            catalog: Catalog::default(),
            catalog_revision: 0,
            local_index: BTreeMap::new(),
            vault_keys: Vec::new(),
            last_refresh: Instant::now() - Duration::from_secs(10),
            pair: PairPanel::default(),
            dialog: Dialog::None,
            toast: None,
            toast_at: Instant::now(),
            busy: None,
            shots,
            catalog_path_input: String::new(),
            vault_upload_path: String::new(),
            vault_ttl_days: 7,
            vault_download_dir: data_dir.join("incoming").display().to_string(),
            auto_pair_started: false,
            pair_auto_confirmed_for: None,
            theme_mode,
            system_light,
            applied_light: !theme_mode.palette(system_light).light,
            last_system_probe: Instant::now(),
            auto_start_sync,
            hide_to_tray,
            tray: None,
            tray_hwnd: std::sync::Arc::new(std::sync::atomic::AtomicIsize::new(0)),
            quitting: false,
            was_minimized: false,
        };
        app.refresh_store();
        app.apply_theme(&cc.egui_ctx);
        let tray_bridge = crate::tray::TrayBridge {
            ctx: cc.egui_ctx.clone(),
            cmd_tx: app.cmd_tx.clone(),
            remote_clipboard: app.remote_clip_tx.clone(),
            data_dir: data_dir.clone(),
            hwnd: app.tray_hwnd.clone(),
        };
        app.tray = crate::tray::Tray::new(
            &cc.egui_ctx,
            &format!("drive-sync · {}", app.sync_state.label()),
            tray_bridge.clone(),
        );
        if app.tray.is_some() {
            logbuf::log("系统托盘图标已创建（左键显示主界面，右键菜单：显示/开始/停止/退出）");
        } else {
            logbuf::log("未创建系统托盘图标（当前平台不支持或创建失败，回退任务栏）");
        }
        // 验收测试钩子：2s 隐藏 → 4s 从托盘恢复 → 6s 退出（仅调试用，独立于 UI 重绘）。
        if std::env::var("DRIVE_GUI_TEST_TRAY")
            .map(|value| value == "1")
            .unwrap_or(false)
        {
            let bridge = tray_bridge.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                bridge.hide();
                std::thread::sleep(Duration::from_secs(2));
                bridge.show();
                std::thread::sleep(Duration::from_secs(2));
                bridge.quit();
            });
        }
        if force_wizard {
            app.prefill_wizard_for_screenshots();
        }
        // 打开即同步：已完成向导（identity/relay/token 完整）且设置开启时才自动开始。
        let configured = !app.screen_wizard;
        if configured && app.auto_start_sync {
            logbuf::log("打开即同步：自动开始连接中继（可在设置中关闭）");
            app.start_sync();
        }
        if !app.screen_wizard {
            app.maybe_auto_pair();
        }
        app
    }

    fn save_gui_prefs(&self) {
        let settings = GuiSettings {
            theme: self.theme_mode,
            auto_start_sync: self.auto_start_sync,
            hide_to_tray: self.hide_to_tray,
        };
        if let Err(error) = theme::save_gui_settings(&self.data_dir, &settings) {
            logbuf::log(format!("保存 gui.json 失败: {error:#}"));
        }
    }

    fn update_tray_tooltip(&mut self) {
        if let Some(tray) = self.tray.as_mut() {
            tray.set_tooltip(&format!("drive-sync · {}", self.sync_state.label()));
        }
    }

    /// 最小化 → 隐藏到托盘；关闭 X → 默认隐藏到托盘（设置可关）。
    fn handle_window_events(&mut self, ctx: &egui::Context) {
        if crate::tray::supported() && self.tray.is_some() {
            let minimized = ctx
                .input(|input| input.viewport().minimized)
                .unwrap_or(false);
            if minimized && self.hide_to_tray && !self.was_minimized {
                self.was_minimized = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                logbuf::log("窗口已最小化到系统托盘（同步继续运行）");
            } else if !minimized && self.was_minimized {
                self.was_minimized = false;
            }
            if ctx.input(|input| input.viewport().close_requested())
                && self.hide_to_tray
                && !self.quitting
                && !crate::tray::quit_requested()
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                logbuf::log("关闭窗口已改为隐藏到系统托盘（同步继续运行；托盘菜单可退出）");
            }
        }
    }

    /// 应用当前主题（palette 切换立即生效）。
    fn apply_theme(&mut self, ctx: &egui::Context) {
        let palette = self.theme_mode.palette(self.system_light);
        ctx.set_theme(if palette.light {
            egui::ThemePreference::Light
        } else {
            egui::ThemePreference::Dark
        });
        theme::apply(ctx, palette);
        self.applied_light = palette.light;
    }

    /// 「跟随系统」时检测系统外观变化并即时跟随（读取失败保持当前/回退深色）。
    fn maybe_follow_system_theme(&mut self, ctx: &egui::Context) {
        if self.theme_mode != ThemeMode::System {
            return;
        }
        let raw = ctx.input(|input| input.raw.system_theme);
        match raw {
            Some(egui::Theme::Light) => self.system_light = Some(true),
            Some(egui::Theme::Dark) => self.system_light = Some(false),
            None => {
                if self.last_system_probe.elapsed() >= Duration::from_secs(5) {
                    self.last_system_probe = Instant::now();
                    if let Some(light) = platform::system_prefers_light() {
                        self.system_light = Some(light);
                    }
                }
            }
        }
        let effective_light = self.system_light.unwrap_or(false);
        if effective_light != self.applied_light {
            self.apply_theme(ctx);
            ctx.request_repaint();
        }
    }

    fn set_theme_mode(&mut self, ctx: &egui::Context, mode: ThemeMode) {
        self.theme_mode = mode;
        self.save_gui_prefs();
        self.toast(true, format!("外观主题：{}", mode.label()));
        self.apply_theme(ctx);
        ctx.request_repaint();
    }

    fn begin_busy(&mut self, label: impl Into<String>) {
        self.busy = Some(BusyState {
            label: label.into(),
            started: Instant::now(),
            done: 0,
            total: None,
        });
    }

    fn set_busy_progress(&mut self, done: u64, total: u64) {
        if let Some(busy) = self.busy.as_mut() {
            busy.done = done;
            busy.total = Some(total);
        } else {
            self.busy = Some(BusyState {
                label: "处理中…".to_string(),
                started: Instant::now(),
                done,
                total: Some(total),
            });
        }
    }

    fn end_busy(&mut self) {
        self.busy = None;
    }

    fn toast(&mut self, ok: bool, text: impl Into<String>) {
        self.toast = Some(Toast {
            text: text.into(),
            ok,
        });
        self.toast_at = Instant::now();
    }

    fn refresh_store(&mut self) {
        let Ok(store) = Store::open(&self.data_dir) else {
            return;
        };
        if let Ok(mut peers) = store.load_peers() {
            peers.sort_by(|a, b| a.peer.device_id.cmp(&b.peer.device_id));
            self.peers = peers;
        }
        if let Ok((catalog, revision)) = store.load_catalog() {
            self.catalog = catalog;
            self.catalog_revision = revision;
        }
        if let Ok(index) = store.load_local_index() {
            self.local_index = index;
        }
        if let Ok(keys) = store.load_vault_keys() {
            self.vault_keys = keys;
        }
    }

    fn prefill_wizard_for_screenshots(&mut self) {
        self.wizard.step = 0;
        self.wizard.name = "演示笔记本".to_string();
        self.wizard.relay = RELAY_PLACEHOLDER.to_string();
        self.wizard.token = String::new();
        self.wizard.test = Some((
            true,
            "连接成功：中继会话 abcdef0123456789，当前在线 2 台设备".to_string(),
        ));
        self.wizard.pair.mode = Some(PairMode::Initiate);
        self.wizard.pair.started = true;
        self.wizard.pair.progress = PairProgress {
            phase: "请核对 SAS 后确认".to_string(),
            code: Some("48213 907".to_string()),
            qr: Some("dsync-pair-v1:eyJpbnZpdGUiOnsidjox... (演示)".to_string()),
            peer_name: Some("公司笔记本".to_string()),
            peer_id: Some("dev-0123456789abcdef".to_string()),
            sas: Some("482 197".to_string()),
            waiting_confirm: true,
        };
    }

    /// 本机联调/自动化：`DRIVE_GUI_AUTO_PAIR=initiate` 或 `join:<配对码>`。
    fn maybe_auto_pair(&mut self) {
        if self.auto_pair_started {
            return;
        }
        let Ok(spec) = std::env::var("DRIVE_GUI_AUTO_PAIR") else {
            return;
        };
        let Ok(store) = Store::open(&self.data_dir) else {
            return;
        };
        let config = store.load_config().unwrap_or_default();
        if config.relay_url.trim().is_empty() {
            return;
        }
        self.auto_pair_started = true;
        let (code, qr) = match spec.strip_prefix("join:") {
            Some(value) if !value.trim().is_empty() => (value.trim().to_string(), String::new()),
            _ => (String::new(), String::new()),
        };
        let mode = if spec.starts_with("join:") {
            PairMode::Join
        } else {
            PairMode::Initiate
        };
        self.pair = PairPanel {
            mode: Some(mode),
            started: true,
            visible_modal: false,
            join_code: code.clone(),
            join_qr: qr.clone(),
            ..Default::default()
        };
        let _ = self.cmd_tx.send(Cmd::Pair(PairInput {
            mode,
            relay_url: config.relay_url,
            access_token: config.access_token,
            code: if code.is_empty() { None } else { Some(code) },
            qr: if qr.is_empty() { None } else { Some(qr) },
        }));
        logbuf::log(format!("自动配对已启动（{mode:?}）"));
    }

    // ------------------------------------------------------------------
    // 消息处理
    // ------------------------------------------------------------------

    fn poll_messages(&mut self) {
        let mut messages = Vec::new();
        if let Some(backend) = self.backend.as_ref() {
            while let Some(message) = backend.try_recv() {
                messages.push(message);
                if messages.len() >= 500 {
                    break;
                }
            }
        }
        for message in messages {
            self.handle_message(message);
        }
        while let Ok(payload) = self.remote_clip_rx.try_recv() {
            self.apply_remote_clipboard(payload);
        }
        if self.toast.is_some() && self.toast_at.elapsed() > Duration::from_secs(6) {
            self.toast = None;
        }
    }

    fn handle_message(&mut self, message: UiMsg) {
        match message {
            UiMsg::SyncStarted { device_id } => {
                self.sync_state = SyncState::Running { device_id };
            }
            UiMsg::SyncStopped { reason } => {
                let running = self.sync_state.is_running();
                self.sync_state = if reason.contains("失败") || reason.contains("错误") {
                    SyncState::Error {
                        message: reason.clone(),
                    }
                } else {
                    SyncState::Stopped
                };
                if running {
                    self.toast(false, reason);
                }
            }
            UiMsg::Event(event) => self.handle_runtime_event(event),
            UiMsg::Progress { label, done, total } => {
                if self.busy.is_none() {
                    self.begin_busy(label);
                }
                self.set_busy_progress(done, total);
            }
            UiMsg::PairState(progress) => {
                logbuf::log(format!(
                    "配对进度: {} code={:?} peer={:?} sas={:?}",
                    progress.phase, progress.code, progress.peer_name, progress.sas
                ));
                self.pair.progress = progress.clone();
                if self.screen_wizard {
                    self.wizard.pair.progress = progress.clone();
                    self.wizard.pair.started = true;
                } else {
                    self.pair.started = true;
                }
                let auto_confirm = std::env::var("DRIVE_GUI_AUTO_CONFIRM_SAS")
                    .map(|value| value == "1")
                    .unwrap_or(false);
                if auto_confirm && progress.waiting_confirm {
                    if let Some(sas) = progress.sas {
                        if self.pair_auto_confirmed_for.as_deref() != Some(sas.as_str()) {
                            self.pair_auto_confirmed_for = Some(sas);
                            let _ = self.cmd_tx.send(Cmd::PairDecision(true));
                        }
                    }
                }
            }
            UiMsg::PairFinished { ok, message } => {
                logbuf::log(format!("配对结束: ok={ok} {message}"));
                self.pair.finished = Some((ok, message.clone()));
                if self.screen_wizard {
                    self.wizard.pair.finished = Some((ok, message.clone()));
                    self.wizard.pair.started = false;
                } else {
                    self.pair.started = false;
                }
                self.toast(ok, message);
                self.refresh_store();
            }
            UiMsg::TestResult { ok, message } => {
                if self.screen_wizard {
                    self.wizard.test = Some((ok, message.clone()));
                    self.wizard.test_pending = false;
                } else {
                    self.settings.update_message = Some(message.clone());
                }
                self.toast(ok, message);
            }
            UiMsg::CatalogAddResult { ok, message } => {
                self.end_busy();
                self.toast(ok, message);
                self.refresh_store();
            }
            UiMsg::CatalogRemoveResult { ok, message } => {
                self.end_busy();
                self.toast(ok, message);
                self.refresh_store();
            }
            UiMsg::FetchFinished { ok, message, path } => {
                self.end_busy();
                let line = if let Some(path) = &path {
                    format!("{} → {}", message, path.display())
                } else {
                    message.clone()
                };
                push_limited(&mut self.transfer_lines, line, TRANSFER_LIMIT);
                self.toast(ok, message);
                self.refresh_store();
            }
            UiMsg::VaultList(items) => {
                self.vault_items = items;
            }
            UiMsg::VaultOpDone { ok, message } => {
                self.end_busy();
                self.toast(ok, message);
                self.refresh_store();
            }
            UiMsg::UpdateCheck(info) => {
                self.settings.checking_update = false;
                self.settings.update_message = Some(format!("（{}）", info.reason));
                self.settings.update = Some(info);
            }
            UiMsg::UpdateApplied { ok, message } => {
                self.settings.checking_update = false;
                self.settings.update_message = Some(message.clone());
                self.toast(ok, message);
            }
            UiMsg::Log(line) => {
                push_limited(&mut self.events, line, EVENT_LIMIT);
            }
        }
    }

    fn handle_runtime_event(&mut self, event: RuntimeEvent) {
        let summary = event.summary();
        logbuf::log(format!("事件: {summary}"));
        push_limited(&mut self.events, summary.clone(), EVENT_LIMIT);
        match &event {
            RuntimeEvent::Authenticated { online, .. } => {
                self.online = online.iter().cloned().collect();
            }
            RuntimeEvent::Presence { online } => {
                self.online = online.iter().cloned().collect();
            }
            RuntimeEvent::PeerOnline { peer_id, .. } => {
                self.online.insert(peer_id.clone());
                self.refresh_store();
            }
            RuntimeEvent::PeerOffline { peer_id } => {
                self.online.remove(peer_id);
                self.sessions.remove(peer_id);
                self.refresh_store();
            }
            RuntimeEvent::SessionEstablished { peer_id, .. } => {
                self.sessions.insert(peer_id.clone());
                self.refresh_store();
            }
            RuntimeEvent::SessionClosed { peer_id } => {
                self.sessions.remove(peer_id);
            }
            RuntimeEvent::ClipboardCaptured { kind, size } => {
                push_limited(
                    &mut self.clipboard_records,
                    format!("↑ 本机复制 {kind} {size} 字节"),
                    CLIPBOARD_RECORD_LIMIT,
                );
            }
            RuntimeEvent::ClipboardApplied {
                peer_id,
                kind,
                size,
            } => {
                push_limited(
                    &mut self.clipboard_records,
                    format!("↓ 收到 {kind} {size} 字节（来自 {peer_id}）"),
                    CLIPBOARD_RECORD_LIMIT,
                );
            }
            RuntimeEvent::ClipboardSent { peers, kind, size } => {
                push_limited(
                    &mut self.clipboard_records,
                    format!("↑ 发送 {kind} {size} 字节 → {peers} 台设备"),
                    CLIPBOARD_RECORD_LIMIT,
                );
            }
            RuntimeEvent::ClipboardSpooled { peers } => {
                push_limited(
                    &mut self.clipboard_records,
                    format!("… 对端离线，{peers} 条进入待发队列"),
                    CLIPBOARD_RECORD_LIMIT,
                );
            }
            RuntimeEvent::PeerHaveList { peer_id, entry_ids } => {
                self.peer_have.insert(peer_id.clone(), entry_ids.clone());
            }
            RuntimeEvent::CatalogUpdated { .. }
            | RuntimeEvent::CatalogMerged { .. }
            | RuntimeEvent::CatalogReloaded { .. } => {
                self.refresh_store();
            }
            RuntimeEvent::TransferProgress {
                received, total, ..
            } => {
                if self.busy.is_none() {
                    self.begin_busy("正在传输文件…");
                }
                self.set_busy_progress(*received, *total);
                push_limited(&mut self.transfer_lines, summary.clone(), TRANSFER_LIMIT);
            }
            RuntimeEvent::TransferReceived { .. } | RuntimeEvent::TransferFailed { .. } => {
                self.end_busy();
                push_limited(&mut self.transfer_lines, summary.clone(), TRANSFER_LIMIT);
            }
            RuntimeEvent::TransferOffered { .. } | RuntimeEvent::TransferStarted { .. } => {
                push_limited(&mut self.transfer_lines, summary.clone(), TRANSFER_LIMIT);
            }
            RuntimeEvent::PeerRevoked { .. }
            | RuntimeEvent::PeerUnpaired { .. }
            | RuntimeEvent::PeerRevocationNotice { .. }
            | RuntimeEvent::PeerExpired { .. }
            | RuntimeEvent::PeersReloaded { .. } => {
                self.refresh_store();
            }
            RuntimeEvent::Error { message } => {
                self.sync_state = SyncState::Error {
                    message: message.clone(),
                };
            }
            _ => {}
        }
    }

    fn apply_remote_clipboard(&mut self, payload: ClipboardPayload) {
        let mut clipboard = match self.clipboard.take() {
            Some(clipboard) => clipboard,
            None => match arboard::Clipboard::new() {
                Ok(clipboard) => clipboard,
                Err(error) => {
                    self.toast(false, format!("初始化系统剪切板失败: {error}"));
                    return;
                }
            },
        };
        let result: Result<(), String> = match payload.kind {
            ClipboardKind::Text | ClipboardKind::RichText => payload
                .as_text()
                .map_err(|error| format!("解析文本失败: {error}"))
                .and_then(|text| {
                    clipboard
                        .set_text(text)
                        .map_err(|error| format!("写入剪切板失败: {error}"))
                }),
            ClipboardKind::Image => payload
                .decompressed()
                .map_err(|error| format!("解压图片失败: {error}"))
                .and_then(|png| {
                    let image = image::load_from_memory(&png)
                        .map_err(|error| format!("解码 PNG 失败: {error}"))?;
                    let rgba = image.to_rgba8();
                    let (width, height) = rgba.dimensions();
                    clipboard
                        .set_image(arboard::ImageData {
                            width: width as usize,
                            height: height as usize,
                            bytes: std::borrow::Cow::Owned(rgba.into_raw()),
                        })
                        .map_err(|error| format!("写入剪切板失败: {error}"))
                }),
            ClipboardKind::Files => Ok(()),
        };
        self.clipboard = Some(clipboard);
        match result {
            Ok(()) => push_limited(
                &mut self.clipboard_records,
                format!(
                    "↓ 已应用到本机剪切板（{} {} 字节）",
                    clipboard_kind_text(payload.kind),
                    payload.raw_size
                ),
                CLIPBOARD_RECORD_LIMIT,
            ),
            Err(error) => self.toast(false, error),
        }
    }

    // ------------------------------------------------------------------
    // 操作
    // ------------------------------------------------------------------

    fn start_sync(&mut self) {
        let Ok(store) = Store::open(&self.data_dir) else {
            self.toast(false, "打开数据目录失败");
            return;
        };
        let Ok(config) = store.load_config() else {
            self.toast(false, "读取配置失败");
            return;
        };
        if config.relay_url.trim().is_empty() {
            self.toast(false, "请先在设置中填写中继地址");
            return;
        }
        if !store.identity_exists() {
            self.toast(false, "请先完成首次运行向导（创建设备身份）");
            return;
        }
        let _ = self.cmd_tx.send(Cmd::StartSync {
            relay_url: config.relay_url,
            access_token: config.access_token,
            remote_clipboard: self.remote_clip_tx.clone(),
        });
        self.sync_state = SyncState::Starting;
    }

    fn stop_sync(&mut self) {
        let _ = self.cmd_tx.send(Cmd::StopSync);
        self.sync_state = SyncState::Stopped;
    }

    fn save_config(&mut self, relay: &str, token: &str, poll_ms: u64) {
        let Ok(store) = Store::open(&self.data_dir) else {
            self.toast(false, "打开数据目录失败");
            return;
        };
        let mut config = store.load_config().unwrap_or_default();
        config.relay_url = relay.trim().to_string();
        config.access_token = token.trim().to_string();
        config.poll_ms = poll_ms.clamp(200, 5000);
        config.clipboard.share_text = self.controls.share_text.load(Ordering::SeqCst);
        config.clipboard.share_images = self.controls.share_images.load(Ordering::SeqCst);
        match store.save_config(&config) {
            Ok(()) => self.toast(true, "配置已保存（修改中继/令牌后需重启同步生效）"),
            Err(error) => self.toast(false, format!("保存配置失败: {error:#}")),
        }
        self.controls
            .poll_ms
            .store(config.poll_ms, Ordering::SeqCst);
    }

    fn save_device_name(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.toast(false, "设备名不能为空");
            return;
        }
        let Ok(store) = Store::open(&self.data_dir) else {
            return;
        };
        let Ok(mut identity) = store.load_identity() else {
            self.toast(false, "设备尚未初始化");
            return;
        };
        identity.name = name.to_string();
        match store.save_identity(&identity) {
            Ok(()) => {
                self.wizard.name = name.to_string();
                self.toast(true, "设备名已更新");
            }
            Err(error) => self.toast(false, format!("保存设备名失败: {error:#}")),
        }
    }

    fn create_identity_if_needed(&mut self, name: &str) -> bool {
        let Ok(store) = Store::open(&self.data_dir) else {
            self.toast(false, "打开数据目录失败");
            return false;
        };
        if store.identity_exists() {
            return true;
        }
        match store.load_or_create_identity(name.trim()) {
            Ok(_) => {
                self.refresh_store();
                true
            }
            Err(error) => {
                self.toast(false, format!("创建身份失败: {error:#}"));
                false
            }
        }
    }

    fn enter_main_after_wizard(&mut self) {
        let relay = self.wizard.relay.clone();
        let token = self.wizard.token.clone();
        self.save_config(&relay, &token, self.controls.poll_ms.load(Ordering::SeqCst));
        self.screen_wizard = false;
        self.page = Page::Status;
        self.refresh_store();
        self.start_sync();
    }
}

// ---------------------------------------------------------------------------
// eframe App
// ---------------------------------------------------------------------------

impl eframe::App for GuiApp {
    #[cfg_attr(
        not(any(target_os = "windows", target_os = "macos")),
        allow(unused_variables)
    )]
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // 记录原生窗口句柄，供托盘线程隐藏/恢复窗口（Windows）。
        #[cfg(target_os = "windows")]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(handle) = frame.window_handle() {
                if let RawWindowHandle::Win32(win32) = handle.as_raw() {
                    self.tray_hwnd
                        .store(win32.hwnd.get(), std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
        self.poll_messages();
        if self.last_refresh.elapsed() >= Duration::from_secs(1) {
            self.last_refresh = Instant::now();
            self.refresh_store();
        }
        self.maybe_follow_system_theme(ctx);
        self.handle_window_events(ctx);
        self.update_tray_tooltip();
        self.handle_screenshot_events(ctx);

        if self.screen_wizard {
            draw_wizard(self, ctx);
        } else {
            draw_main(self, ctx);
        }
        draw_pair_modal(self, ctx);
        draw_dialogs(self, ctx);
        draw_toast(self, ctx);
        self.run_shot_plan(ctx);

        ctx.request_repaint_after(Duration::from_millis(200));
    }
}

impl Drop for GuiApp {
    fn drop(&mut self) {
        if let Some(backend) = self.backend.take() {
            backend.shutdown();
        }
    }
}

// ---------------------------------------------------------------------------
// 字体 / 主题
// ---------------------------------------------------------------------------

pub fn install_fonts_and_style(ctx: &egui::Context) -> bool {
    let candidates = [
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/msyh.ttf",
        "C:/Windows/Fonts/simhei.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/System/Library/Fonts/PingFang.ttc",
    ];
    let mut loaded_font = false;
    for path in candidates {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "cjk".to_string(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes)),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "cjk".to_string());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("cjk".to_string());
        ctx.set_fonts(fonts);
        logbuf::log(format!("已加载中文字体: {path}"));
        loaded_font = true;
        break;
    }
    // 固定初始主题；GuiApp::new 会按 gui.json / 系统设置重新应用 palette。
    ctx.set_theme(egui::ThemePreference::Dark);
    theme::apply(ctx, &theme::DARK);
    loaded_font
}

// ---------------------------------------------------------------------------
// 向导（PolyBot 风格：居中卡片 + 品牌头 + 步骤条）
// ---------------------------------------------------------------------------

const WIZARD_STEPS: &[&str] = &["欢迎", "设备名", "中继", "配对", "完成"];

fn draw_wizard(app: &mut GuiApp, ctx: &egui::Context) {
    egui::CentralPanel::default()
        .frame(
            egui::Frame::NONE
                .fill(theme::bg())
                .inner_margin(egui::Margin::symmetric(0, 16)),
        )
        .show(ctx, |ui| {
            theme::grid(ui);
            let width = 880.0_f32.min(ui.available_width() - 60.0);
            let margin = ((ui.available_width() - width) / 2.0).max(0.0);
            // 整个卡片放进可滚动区域：小窗口下内容超出时可滚动查看，不会裁剪。
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add_space(margin);
                        ui.vertical(|ui| {
                            ui.set_width(width);
                            egui::Frame::new()
                                .fill(theme::panel())
                                .stroke(egui::Stroke::new(1.0, theme::border()))
                                .corner_radius(egui::CornerRadius::same(theme::CARD_RADIUS))
                                .inner_margin(egui::Margin::same(18))
                                .show(ui, |ui| {
                                    ui.set_width(width - 36.0);
                                    theme::brand_row(
                                        ui,
                                        "端到端加密 · 中继只转发密文",
                                        &format!("v{}", env!("CARGO_PKG_VERSION")),
                                    );
                                    ui.add_space(8.0);
                                    theme::rule(ui);
                                    ui.add_space(6.0);
                                    wizard_stepper(app, ui);
                                    ui.add_space(8.0);
                                    match app.wizard.step {
                                        0 => wizard_welcome(app, ui),
                                        1 => wizard_device_name(app, ui),
                                        2 => wizard_relay(app, ui),
                                        3 => wizard_pairing(app, ui),
                                        _ => wizard_finish(app, ui),
                                    }
                                });
                        });
                    });
                });
        });
}

fn wizard_stepper(app: &GuiApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        for (index, label) in WIZARD_STEPS.iter().enumerate() {
            let active = index == app.wizard.step;
            let done = index < app.wizard.step;
            let color = if active {
                theme::mint()
            } else if done {
                theme::muted_color()
            } else {
                theme::border()
            };
            let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 8.0, color);
            if active {
                ui.painter().circle_filled(rect.center(), 3.5, theme::bg());
            }
            ui.label(
                egui::RichText::new(format!("{}. {}", index + 1, label))
                    .size(13.0)
                    .color(if active {
                        theme::text()
                    } else {
                        theme::muted_color()
                    })
                    .strong(),
            );
            if index + 1 < WIZARD_STEPS.len() {
                ui.label(egui::RichText::new("─").color(theme::border()));
            }
        }
    });
}

fn wizard_welcome(app: &mut GuiApp, ui: &mut egui::Ui) {
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new("欢迎使用 drive-sync 桌面客户端")
            .size(20.0)
            .strong()
            .color(theme::text()),
    );
    ui.add_space(8.0);
    for line in [
        "剪切板、软件目录与暂存内容端到端加密，中继只能看到密文与路由信息。",
        "配对一次后长期有效；显式删除或超过 90 天不活跃才会失效。",
        "软件本体留在持有设备本地，中继只中转加密分块。",
        "数据目录与 CLI（drive-sync）完全共用：identity.json / config.json / peers.json。",
    ] {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
            ui.painter()
                .circle_filled(rect.center(), 3.0, theme::mint());
            ui.label(egui::RichText::new(line).color(theme::muted_color()));
        });
    }
    ui.add_space(10.0);
    egui::Frame::new()
        .fill(theme::panel_dark())
        .stroke(egui::Stroke::new(1.0, theme::border()))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.label(theme::muted(
                "注意：GUI 与命令行客户端不要同时运行（同一设备在中继上只允许一条连接，会互踢；本应用启动即为常驻同步）。",
            ));
        });
    ui.add_space(14.0);
    if theme::primary_button(ui, "开始设置 →") {
        app.wizard.step = 1;
    }
}

fn wizard_device_name(app: &mut GuiApp, ui: &mut egui::Ui) {
    ui.label(
        egui::RichText::new("给这台设备起个名字")
            .size(20.0)
            .strong()
            .color(theme::text()),
    );
    ui.label(theme::muted("用于在其他设备的配对列表中识别本机。"));
    ui.add_space(10.0);
    let mut name = app.wizard.name.clone();
    theme::input(ui, &mut name, "例如：我的笔记本", 380.0);
    app.wizard.name = name;
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        if theme::secondary_button(ui, "上一步") {
            app.wizard.step = 0;
        }
        if theme::primary_button(ui, "下一步") {
            if app.wizard.name.trim().is_empty() {
                app.wizard.error = Some("请填写设备名称".to_string());
            } else if app.create_identity_if_needed(&app.wizard.name.clone()) {
                app.wizard.error = None;
                app.wizard.step = 2;
            }
        }
    });
    wizard_error(ui, &app.wizard.error);
}

fn wizard_relay(app: &mut GuiApp, ui: &mut egui::Ui) {
    ui.label(
        egui::RichText::new("连接中继服务器")
            .size(20.0)
            .strong()
            .color(theme::text()),
    );
    ui.label(theme::muted(
        "自建中继地址（wss:// 需要 TLS）与共享访问令牌；令牌可在服务器配置中查看。",
    ));
    ui.add_space(10.0);
    egui::Grid::new("wizard-relay-grid")
        .num_columns(2)
        .spacing([14.0, 12.0])
        .show(ui, |ui| {
            ui.label(theme::muted("中继地址"));
            theme::input(ui, &mut app.wizard.relay, RELAY_PLACEHOLDER, 430.0);
            ui.end_row();
            ui.label(theme::muted("访问令牌"));
            egui::Frame::NONE.show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.wizard.token)
                        .password(true)
                        .hint_text(
                            egui::RichText::new("服务器共享令牌").color(theme::muted_color()),
                        )
                        .desired_width(430.0)
                        .margin(egui::vec2(10.0, 8.0)),
                );
            });
            ui.end_row();
        });
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        if theme::secondary_button(ui, "测试连接") && !app.wizard.test_pending {
            app.wizard.test_pending = true;
            app.wizard.test = None;
            let _ = app.cmd_tx.send(Cmd::TestConnection {
                relay_url: app.wizard.relay.clone(),
                access_token: app.wizard.token.clone(),
            });
        }
        if app.wizard.test_pending {
            ui.spinner();
            ui.label(theme::muted("正在连接…"));
        }
        if let Some((ok, message)) = &app.wizard.test {
            theme::hint(ui, Some(*ok), message);
        }
    });
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        if theme::secondary_button(ui, "上一步") {
            app.wizard.step = 1;
        }
        if theme::primary_button(ui, "下一步") {
            if app.wizard.relay.trim().is_empty() {
                app.wizard.error = Some("请填写中继地址".to_string());
            } else if app.wizard.token.trim().is_empty() {
                app.wizard.error = Some("请填写访问令牌".to_string());
            } else {
                let relay = app.wizard.relay.clone();
                let token = app.wizard.token.clone();
                app.save_config(&relay, &token, app.controls.poll_ms.load(Ordering::SeqCst));
                app.wizard.error = None;
                app.wizard.step = 3;
            }
        }
    });
    wizard_error(ui, &app.wizard.error);
}

fn wizard_pairing(app: &mut GuiApp, ui: &mut egui::Ui) {
    ui.label(
        egui::RichText::new("与其他设备配对")
            .size(19.0)
            .strong()
            .color(theme::text()),
    );
    ui.label(theme::muted(
        "双方核对 6 位 SAS 校验码一致后才会保存配对记录；可跳过，稍后在「设备」页完成。",
    ));
    ui.add_space(6.0);
    let relay = app.wizard.relay.clone();
    let token = app.wizard.token.clone();
    let mut panel = std::mem::take(&mut app.wizard.pair);
    pair_panel_ui(ui, &mut panel, &app.cmd_tx, &relay, &token);
    app.wizard.pair = panel;
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        if theme::secondary_button(ui, "上一步") {
            app.wizard.step = 2;
        }
        if app.wizard.pair.started {
            ui.label(theme::muted("配对进行中…"));
        } else if theme::primary_button(ui, "下一步") {
            app.wizard.step = 4;
        }
        if theme::secondary_button(ui, "跳过配对") {
            app.wizard.skip_pair = true;
            app.wizard.step = 4;
        }
    });
}

fn wizard_finish(app: &mut GuiApp, ui: &mut egui::Ui) {
    ui.label(
        egui::RichText::new("准备就绪")
            .size(20.0)
            .strong()
            .color(theme::text()),
    );
    app.refresh_store();
    ui.add_space(8.0);
    theme::kv(ui, "本机设备名", app.wizard.name.clone());
    theme::kv(ui, "已配对设备", format!("{} 台", app.peers.len()));
    if app.peers.is_empty() {
        ui.label(theme::muted(
            "当前没有配对设备，可稍后在「设备」页发起或加入配对。",
        ));
    } else {
        for peer in &app.peers {
            ui.label(theme::muted(format!(
                "• {}（{}）",
                peer.display_name(),
                peer.peer.device_id
            )));
        }
    }
    ui.add_space(10.0);
    ui.label(theme::muted(
        "点击下方按钮保存配置并进入主界面，首次启动会尝试连接中继。",
    ));
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        if theme::secondary_button(ui, "上一步") {
            app.wizard.step = 3;
        }
        if theme::primary_button(ui, "完成并启动同步") {
            app.enter_main_after_wizard();
        }
        if theme::secondary_button(ui, "仅进入主界面") {
            let relay = app.wizard.relay.clone();
            let token = app.wizard.token.clone();
            app.save_config(&relay, &token, app.controls.poll_ms.load(Ordering::SeqCst));
            app.screen_wizard = false;
            app.page = Page::Status;
            app.refresh_store();
        }
    });
}

fn wizard_error(ui: &mut egui::Ui, error: &Option<String>) {
    if let Some(error) = error {
        ui.add_space(10.0);
        theme::hint(ui, Some(false), error);
    }
}

// ---------------------------------------------------------------------------
// 配对面板（SAS 确认）
// ---------------------------------------------------------------------------

fn pair_panel_ui(
    ui: &mut egui::Ui,
    panel: &mut PairPanel,
    cmd_tx: &Sender<Cmd>,
    relay: &str,
    token: &str,
) {
    // 入口已经带了模式：发起配对直接开始，加入配对直接显示配对码输入，
    // 不再在弹窗里让用户二次选择「发起/加入」。
    if !panel.started && panel.mode == Some(PairMode::Initiate) {
        start_pair_from_panel(panel, cmd_tx, relay, token, PairMode::Initiate);
    }
    if !panel.started && panel.mode.is_none() {
        ui.horizontal(|ui| {
            if theme::primary_button(ui, "发起配对") {
                start_pair_from_panel(panel, cmd_tx, relay, token, PairMode::Initiate);
            }
            if theme::secondary_button(ui, "加入配对") {
                panel.mode = Some(PairMode::Join);
            }
        });
    }
    if panel.mode == Some(PairMode::Join) && !panel.started {
        ui.add_space(8.0);
        ui.label(theme::muted("配对码（8 位数字）或扫码串（二选一）："));
        theme::input(ui, &mut panel.join_code, "例如 12345678", 240.0);
        theme::input(
            ui,
            &mut panel.join_qr,
            "或粘贴 dsync-pair-v1:… 扫码串",
            540.0,
        );
        if theme::primary_button(ui, "开始加入") {
            start_pair_from_panel(panel, cmd_tx, relay, token, PairMode::Join);
        }
    }
    if panel.started {
        ui.add_space(6.0);
        egui::Frame::new()
            .fill(theme::panel_dark())
            .stroke(egui::Stroke::new(1.0, theme::border()))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new(&panel.progress.phase).color(theme::text()));
                });
                if let Some(code) = &panel.progress.code {
                    ui.add_space(4.0);
                    ui.label(theme::muted("配对码（在另一台设备输入）"));
                    ui.label(
                        egui::RichText::new(code)
                            .size(26.0)
                            .strong()
                            .monospace()
                            .color(theme::mint()),
                    );
                }
                if let Some(qr) = &panel.progress.qr {
                    ui.add_space(4.0);
                    ui.label(theme::muted("二维码串（另一台设备可粘贴）："));
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(qr)
                                .monospace()
                                .size(11.0)
                                .color(theme::muted_color()),
                        )
                        .wrap(),
                    );
                }
                if let Some(peer) = &panel.progress.peer_name {
                    let peer_id = panel.progress.peer_id.clone().unwrap_or_default();
                    theme::kv(ui, "对端设备", format!("{peer}（{peer_id}）"));
                }
                if let Some(sas) = &panel.progress.sas {
                    ui.add_space(8.0);
                    ui.label(theme::muted("SAS 校验码："));
                    theme::big_text(ui, sas, theme::mint());
                    ui.label(theme::muted("与另一台设备屏幕显示的 SAS 核对一致后确认。"));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if panel.progress.waiting_confirm
                            && theme::primary_button(ui, "SAS 一致，确认配对")
                        {
                            let _ = cmd_tx.send(Cmd::PairDecision(true));
                        }
                        if panel.progress.waiting_confirm && theme::secondary_button(ui, "取消配对")
                        {
                            let _ = cmd_tx.send(Cmd::PairDecision(false));
                        }
                    });
                }
            });
    }
    if let Some((ok, message)) = &panel.finished {
        ui.add_space(6.0);
        theme::hint(ui, Some(*ok), message);
    }
}

fn start_pair_from_panel(
    panel: &mut PairPanel,
    cmd_tx: &Sender<Cmd>,
    relay: &str,
    token: &str,
    mode: PairMode,
) {
    panel.mode = Some(mode);
    panel.started = true;
    panel.finished = None;
    panel.progress = PairProgress {
        phase: "正在连接中继…".to_string(),
        ..Default::default()
    };
    let code = panel.join_code.trim().to_string();
    let qr = panel.join_qr.trim().to_string();
    let _ = cmd_tx.send(Cmd::Pair(PairInput {
        mode,
        relay_url: relay.to_string(),
        access_token: token.to_string(),
        code: if code.is_empty() { None } else { Some(code) },
        qr: if qr.is_empty() { None } else { Some(qr) },
    }));
}

fn draw_pair_modal(app: &mut GuiApp, ctx: &egui::Context) {
    if app.screen_wizard || !app.pair.visible_modal {
        return;
    }
    let Ok(store) = Store::open(&app.data_dir) else {
        return;
    };
    let config = store.load_config().unwrap_or_default();
    let mut visible = app.pair.visible_modal;
    let mut panel = std::mem::take(&mut app.pair);
    let mut close = false;
    egui::Window::new("配对设备")
        .collapsible(false)
        .resizable(false)
        .default_width(640.0)
        .show(ctx, |ui| {
            pair_panel_ui(
                ui,
                &mut panel,
                &app.cmd_tx,
                &config.relay_url,
                &config.access_token,
            );
            if theme::secondary_button(ui, "关闭") {
                close = true;
            }
        });
    if close {
        visible = false;
    }
    panel.visible_modal = visible;
    app.pair = panel;
}

// ---------------------------------------------------------------------------
// 主界面：顶部品牌栏 + 左侧导航 + 卡片内容
// ---------------------------------------------------------------------------

fn draw_main(app: &mut GuiApp, ctx: &egui::Context) {
    egui::TopBottomPanel::top("brand-bar")
        .frame(
            egui::Frame::new()
                .fill(theme::panel())
                .stroke(egui::Stroke::new(1.0, theme::border()))
                .inner_margin(egui::Margin::symmetric(20, 12)),
        )
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                theme::logo(ui, 36.0);
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new("drive-sync")
                            .size(21.0)
                            .strong()
                            .color(theme::text()),
                    );
                    ui.label(
                        egui::RichText::new("桌面同步控制台")
                            .size(12.0)
                            .color(theme::muted_color()),
                    );
                });
                theme::right_aligned(ui, |ui| {
                    if let Some(busy) = app.busy.as_ref() {
                        ui.add(egui::Spinner::new().size(14.0).color(theme::mint()));
                        ui.label(
                            egui::RichText::new(format!(
                                "{} {:.1}s",
                                busy.label,
                                busy.started.elapsed().as_secs_f32()
                            ))
                            .size(11.5)
                            .color(theme::muted_color()),
                        );
                        if let Some(total) = busy.total.filter(|total| *total > 0) {
                            let fraction = (busy.done as f32 / total as f32).clamp(0.0, 1.0);
                            ui.add(
                                egui::ProgressBar::new(fraction)
                                    .desired_width(120.0)
                                    .text(format!("{}/{}", busy.done, total)),
                            );
                        }
                        ui.ctx().request_repaint();
                        ui.add_space(4.0);
                    }
                    if app.sync_state.is_running() {
                        if theme::secondary_button(ui, "停止同步") {
                            app.stop_sync();
                        }
                    } else if theme::primary_button(ui, "开始同步") {
                        app.start_sync();
                    }
                    theme::status_dot(ui, app.sync_state.color(), app.sync_state.label());
                    ui.label(
                        egui::RichText::new(format!(
                            "在线 {} · 会话 {}",
                            app.online.len(),
                            app.sessions.len()
                        ))
                        .color(theme::muted_color()),
                    );
                    theme::pill(ui, &format!("v{}", env!("CARGO_PKG_VERSION")));
                });
            });
        });

    egui::SidePanel::left("side-nav")
        .resizable(false)
        .exact_width(196.0)
        .frame(
            egui::Frame::new()
                .fill(theme::panel_dark())
                .stroke(egui::Stroke::new(1.0, theme::border()))
                .inner_margin(egui::Margin::symmetric(12, 16)),
        )
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new("工作台")
                    .size(11.0)
                    .color(theme::muted_color())
                    .strong(),
            );
            ui.add_space(8.0);
            for page in Page::ALL {
                if nav_item(ui, page.label(), app.page == page) {
                    app.page = page;
                    if page == Page::Settings && !app.settings.loaded {
                        load_settings(app);
                    }
                    if page == Page::Vault && app.vault_items.is_empty() {
                        let _ = app.cmd_tx.send(Cmd::VaultList);
                    }
                }
            }
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                ui.add_space(10.0);
                if theme::secondary_button(ui, "退出") {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!(
                        "v{} · 协议 v{}",
                        env!("CARGO_PKG_VERSION"),
                        drive_client::PROTOCOL_VERSION
                    ))
                    .size(11.0)
                    .color(theme::muted_color()),
                );
                let device = Store::open(&app.data_dir)
                    .ok()
                    .and_then(|store| store.load_identity().ok())
                    .map(|identity| identity.name);
                ui.label(
                    egui::RichText::new(device.unwrap_or_else(|| "未初始化".into()))
                        .size(12.0)
                        .color(theme::muted_color()),
                );
            });
        });

    egui::CentralPanel::default()
        .frame(
            egui::Frame::NONE
                .fill(theme::bg())
                .inner_margin(egui::Margin::same(20)),
        )
        .show(ctx, |ui| {
            theme::grid(ui);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match app.page {
                    Page::Status => page_status(app, ui),
                    Page::Devices => page_devices(app, ui),
                    Page::Clipboard => page_clipboard(app, ui),
                    Page::Catalog => page_catalog(app, ui),
                    Page::Vault => page_vault(app, ui),
                    Page::Settings => page_settings(app, ui),
                    Page::Logs => page_logs(app, ui),
                });
        });
}

fn nav_item(ui: &mut egui::Ui, label: &str, selected: bool) -> bool {
    let (fill, color) = if selected {
        (theme::mint(), theme::mint_dark())
    } else {
        (egui::Color32::TRANSPARENT, theme::muted_color())
    };
    let button = egui::Button::new(egui::RichText::new(label).color(color).strong())
        .fill(fill)
        .corner_radius(egui::CornerRadius::same(8))
        .min_size(egui::vec2(ui.available_width(), 36.0));
    ui.add(button).clicked()
}

// ---------------------------------------------------------------------------
// 页面：状态
// ---------------------------------------------------------------------------

fn page_status(app: &mut GuiApp, ui: &mut egui::Ui) {
    theme::page_header(ui, "状态", Page::Status.subtitle());
    let device = Store::open(&app.data_dir)
        .ok()
        .and_then(|store| store.load_identity().ok())
        .map(|identity| (identity.name.clone(), identity.device_id()));
    egui::Grid::new("status-top-grid")
        .num_columns(2)
        .spacing([14.0, 0.0])
        .show(ui, |ui| {
            ui.vertical(|ui| {
                theme::card_titled(ui, "本机设备", |ui| {
                    if let Some((name, device_id)) = &device {
                        theme::kv(ui, "设备名", name.clone());
                        ui.horizontal(|ui| {
                            ui.label(theme::muted("设备 ID"));
                            ui.label(
                                egui::RichText::new(device_id)
                                    .monospace()
                                    .color(theme::text()),
                            );
                        });
                        ui.add_space(8.0);
                        if theme::secondary_button(ui, "复制设备 ID") {
                            ui.ctx().copy_text(device_id.clone());
                        }
                    } else {
                        theme::hint(ui, Some(false), "尚未初始化设备身份");
                    }
                    ui.add_space(6.0);
                    egui::Frame::new()
                        .fill(theme::panel_dark())
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::same(10))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                theme::status_dot(
                                    ui,
                                    app.sync_state.color(),
                                    app.sync_state.label(),
                                );
                                ui.label(theme::muted("连接状态"));
                            });
                        });
                });
            });
            ui.vertical(|ui| {
                theme::card_titled(ui, "在线设备", |ui| {
                    if app.online.is_empty() {
                        theme::empty_state(ui, "暂无在线设备");
                    }
                    for peer_id in &app.online {
                        let name = peer_name(app, peer_id);
                        let session = app.sessions.contains(peer_id);
                        ui.horizontal(|ui| {
                            theme::status_dot(
                                ui,
                                if session {
                                    theme::mint()
                                } else {
                                    theme::warn()
                                },
                                &name,
                            );
                            ui.label(theme::muted(peer_id.clone()));
                            if session {
                                ui.label(
                                    egui::RichText::new("E2E 会话已建立").color(theme::mint()),
                                );
                            }
                        });
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if app.sync_state.is_running() {
                            if theme::secondary_button(ui, "停止同步") {
                                app.stop_sync();
                            }
                        } else if theme::primary_button(ui, "开始同步") {
                            app.start_sync();
                        }
                        if theme::secondary_button(ui, "查看设备") {
                            app.page = Page::Devices;
                        }
                    });
                });
            });
            ui.end_row();
        });
    if let SyncState::Error { message } = &app.sync_state {
        let message = message.clone();
        theme::card_titled(ui, "连接失败", |ui| {
            theme::hint(ui, Some(false), &message);
            ui.add_space(6.0);
            ui.label(theme::muted(
                "中继不可达 / 令牌错误 / 网络问题都可能导致连接失败；窗口不会阻塞，可修改设置后重试。",
            ));
            ui.add_space(6.0);
            if theme::primary_button(ui, "重试连接") {
                app.start_sync();
            }
            if theme::secondary_button(ui, "检查中继设置") {
                app.page = Page::Settings;
            }
        });
    }
    theme::card_titled(ui, "最近事件", |ui| {
        if app.events.is_empty() {
            theme::empty_state(ui, "暂无事件");
        }
        for line in app.events.iter().rev().take(12) {
            ui.label(
                egui::RichText::new(line)
                    .color(theme::muted_color())
                    .size(12.5),
            );
        }
    });
}

// ---------------------------------------------------------------------------
// 页面：设备
// ---------------------------------------------------------------------------

fn page_devices(app: &mut GuiApp, ui: &mut egui::Ui) {
    theme::page_header(ui, "已配对设备", Page::Devices.subtitle());
    ui.horizontal(|ui| {
        if theme::primary_button(ui, "发起配对") {
            app.pair = PairPanel::default();
            app.pair.visible_modal = true;
            app.pair.mode = Some(PairMode::Initiate);
        }
        if theme::secondary_button(ui, "加入配对") {
            app.pair = PairPanel::default();
            app.pair.visible_modal = true;
            app.pair.mode = Some(PairMode::Join);
        }
        if theme::secondary_button(ui, "刷新") {
            app.refresh_store();
        }
    });
    ui.add_space(12.0);
    if app.peers.is_empty() {
        theme::card_titled(ui, "设备列表", |ui| {
            theme::empty_state(
                ui,
                "暂无已配对设备。点击上方「发起配对」或「加入配对」开始。",
            );
        });
        return;
    }
    theme::card_titled(ui, "设备列表", |ui| {
        egui::Grid::new("peers-grid")
            .num_columns(5)
            .spacing([24.0, 12.0])
            .show(ui, |ui| {
                for header in ["设备", "设备 ID", "最近活跃", "90 天到期", "操作"] {
                    ui.label(
                        egui::RichText::new(header)
                            .size(12.0)
                            .color(theme::muted_color())
                            .strong(),
                    );
                }
                ui.end_row();
                for peer in app.peers.clone() {
                    ui.label(
                        egui::RichText::new(peer.display_name())
                            .color(theme::text())
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new(&peer.peer.device_id)
                            .monospace()
                            .size(11.5)
                            .color(theme::muted_color()),
                    );
                    ui.label(theme::muted(platform::format_ms(
                        peer.last_seen_or_paired(),
                    )));
                    ui.label(theme::muted(platform::format_ms(peer.expires_at_ms())));
                    if theme::danger_button(ui, "删除配对") {
                        app.dialog = Dialog::RemovePeer {
                            peer_id: peer.peer.device_id.clone(),
                            name: peer.display_name(),
                        };
                    }
                    ui.end_row();
                }
            });
    });
}

// ---------------------------------------------------------------------------
// 页面：剪切板
// ---------------------------------------------------------------------------

fn page_clipboard(app: &mut GuiApp, ui: &mut egui::Ui) {
    theme::page_header(ui, "剪切板同步", Page::Clipboard.subtitle());
    let mut share_text = app.controls.share_text.load(Ordering::SeqCst);
    let mut share_images = app.controls.share_images.load(Ordering::SeqCst);
    let mut poll_ms = app.controls.poll_ms.load(Ordering::SeqCst);
    let mut changed = false;
    theme::card_titled(ui, "同步设置", |ui| {
        changed |= ui
            .checkbox(&mut share_text, "同步文本（含富文本）")
            .changed();
        changed |= ui.checkbox(&mut share_images, "同步图片（PNG）").changed();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(theme::muted("本机轮询间隔（毫秒）"));
            changed |= ui
                .add(egui::Slider::new(&mut poll_ms, 200..=2000).step_by(100.0))
                .changed();
            ui.label(theme::muted(format!("当前 {poll_ms}ms")));
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if app.sync_state.is_running() {
                if theme::secondary_button(ui, "暂停自动同步") {
                    app.stop_sync();
                }
            } else if theme::primary_button(ui, "恢复自动同步") {
                app.start_sync();
            }
            ui.label(theme::muted(if app.sync_state.is_running() {
                "剪切板同步运行中"
            } else {
                "剪切板同步已暂停"
            }));
        });
    });
    if changed {
        app.controls.share_text.store(share_text, Ordering::SeqCst);
        app.controls
            .share_images
            .store(share_images, Ordering::SeqCst);
        app.controls.poll_ms.store(poll_ms, Ordering::SeqCst);
        if let Ok(store) = Store::open(&app.data_dir) {
            let mut config = store.load_config().unwrap_or_default();
            config.clipboard.share_text = share_text;
            config.clipboard.share_images = share_images;
            config.poll_ms = poll_ms;
            if let Err(error) = store.save_config(&config) {
                app.toast(false, format!("保存剪切板设置失败: {error:#}"));
            }
        }
    }
    theme::card_titled(ui, "最近同步记录（不含内容）", |ui| {
        if app.clipboard_records.is_empty() {
            theme::empty_state(ui, "暂无记录");
        }
        for line in app.clipboard_records.iter().rev().take(16) {
            ui.label(
                egui::RichText::new(line)
                    .color(theme::muted_color())
                    .size(12.5),
            );
        }
        ui.add_space(8.0);
        ui.collapsing("回环抑制说明", |ui| {
            ui.label(theme::muted(
                "接收远端内容后记录 BLAKE3 哈希到 recent_applied（LRU 256 / 5 分钟），本地监听读到相同内容不再上报；",
            ));
            ui.label(theme::muted("同时按 event_id 幂等去重，避免来回回环。"));
        });
    });
}

// ---------------------------------------------------------------------------
// 页面：软件目录
// ---------------------------------------------------------------------------

fn page_catalog(app: &mut GuiApp, ui: &mut egui::Ui) {
    theme::page_header(ui, "软件目录", Page::Catalog.subtitle());
    theme::card_titled(ui, "共享文件 / 文件夹", |ui| {
        ui.horizontal(|ui| {
            ui.label(theme::muted("文件路径"));
            theme::input(
                ui,
                &mut app.catalog_path_input,
                "例如 D:\\Downloads\\tool-1.2.3.zip",
                360.0,
            );
            if platform::browse_supported() && theme::secondary_button(ui, "浏览…") {
                if let Some(path) = platform::pick_file("选择要添加的软件文件") {
                    app.catalog_path_input = path.display().to_string();
                }
            }
            if theme::primary_button(ui, "添加文件") {
                let path = PathBuf::from(app.catalog_path_input.trim());
                if path.as_os_str().is_empty() {
                    app.toast(false, "请先填写文件路径");
                } else {
                    app.begin_busy("正在计算文件哈希并登记目录…");
                    let _ = app.cmd_tx.send(Cmd::CatalogAdd { path });
                }
            }
            if theme::secondary_button(ui, "打开下载目录") {
                let dir = app.data_dir.join("incoming");
                let _ = std::fs::create_dir_all(&dir);
                if let Err(error) = platform::open_path(&dir) {
                    app.toast(false, format!("{error:#}"));
                }
            }
        });
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if theme::primary_button(ui, "共享文件夹…") {
                if let Some(dir) = platform::pick_folder("选择要共享的文件夹") {
                    app.begin_busy(format!("正在扫描共享文件夹：{}", dir.display()));
                    let _ = app.cmd_tx.send(Cmd::ShareFolder { path: dir });
                } else if !platform::browse_supported() {
                    app.toast(false, "当前平台暂未启用文件夹选择");
                }
            }
            ui.label(theme::muted(
                "文件夹按目录树逐文件登记并监听变更；已配对设备可浏览目录、逐个下载，源文件不会被移动。",
            ));
        });
    });
    app.refresh_store();
    let shared: Vec<CatalogEntry> = app
        .catalog
        .visible()
        .filter(|entry| is_shared_entry(entry))
        .cloned()
        .collect();
    if !shared.is_empty() {
        theme::card_titled(
            ui,
            "共享文件夹（对端可浏览并逐个下载）",
            |ui| {
                let mut groups: BTreeMap<String, (String, Vec<CatalogEntry>)> = BTreeMap::new();
                for entry in shared {
                    let id = tag_value(&entry, "share-id:")
                        .unwrap_or("unknown")
                        .to_string();
                    let name = tag_value(&entry, "share-name:")
                        .unwrap_or("共享文件夹")
                        .to_string();
                    groups
                        .entry(id)
                        .or_insert_with(|| (name, Vec::new()))
                        .1
                        .push(entry);
                }
                for (_, (name, files)) in groups {
                    let count = files.len();
                    let tree = build_share_tree(files);
                    egui::CollapsingHeader::new(format!("{name}（{count} 个文件）"))
                        .default_open(false)
                        .show(ui, |ui| {
                            render_share_tree(ui, app, &tree, 0);
                        });
                }
            },
        );
    }
    theme::card_titled(ui, "目录条目", |ui| {
        if app.catalog.visible_count() == 0 {
            theme::empty_state(ui, "目录为空，添加文件后自动登记元数据并同步给已配对设备。");
            return;
        }
        egui::Grid::new("catalog-grid")
            .num_columns(6)
            .spacing([18.0, 12.0])
            .show(ui, |ui| {
                for header in ["名称", "版本", "平台", "大小", "持有设备", "操作"] {
                    ui.label(
                        egui::RichText::new(header)
                            .size(12.0)
                            .color(theme::muted_color())
                            .strong(),
                    );
                }
                ui.end_row();
                for entry in app
                    .catalog
                    .visible()
                    .filter(|entry| !is_shared_entry(entry))
                    .cloned()
                    .collect::<Vec<_>>()
                {
                    let local = app.local_index.contains_key(&entry.id);
                    let holders: Vec<String> = app
                        .peer_have
                        .iter()
                        .filter(|(_, entries)| entries.iter().any(|id| id == &entry.id))
                        .map(|(peer_id, _)| peer_name(app, peer_id))
                        .collect();
                    ui.label(
                        egui::RichText::new(&entry.name)
                            .color(theme::text())
                            .strong(),
                    );
                    ui.label(theme::muted(if entry.version.is_empty() {
                        "-".to_string()
                    } else {
                        entry.version.clone()
                    }));
                    ui.label(theme::muted(entry.platform.as_str()));
                    ui.label(theme::muted(platform::human_bytes(entry.size)));
                    ui.label(theme::muted(if local {
                        "本机".to_string()
                    } else if holders.is_empty() {
                        "无".to_string()
                    } else {
                        holders.join("、")
                    }));
                    ui.horizontal(|ui| {
                        if local {
                            if theme::danger_button(ui, "删除") {
                                app.dialog = Dialog::RemoveCatalog {
                                    id: entry.id.clone(),
                                    name: entry.name.clone(),
                                };
                            }
                        } else if app
                            .peer_have
                            .values()
                            .any(|entries| entries.iter().any(|id| id == &entry.id))
                            && theme::secondary_button(ui, "从对端获取")
                        {
                            app.begin_busy(format!("正在获取 {}…", entry.name));
                            let _ = app.cmd_tx.send(Cmd::FetchCatalog {
                                entry_id: entry.id.clone(),
                                out_dir: app.data_dir.join("incoming"),
                            });
                        }
                    });
                    ui.end_row();
                }
            });
    });
    theme::card_titled(ui, "传输记录", |ui| {
        if app.transfer_lines.is_empty() {
            theme::empty_state(ui, "暂无传输记录");
        }
        for line in app.transfer_lines.iter().rev().take(12) {
            ui.label(
                egui::RichText::new(line)
                    .color(theme::muted_color())
                    .size(12.5),
            );
        }
    });
}

// ---------------------------------------------------------------------------
// 页面：云端暂存
// ---------------------------------------------------------------------------

fn page_vault(app: &mut GuiApp, ui: &mut egui::Ui) {
    theme::page_header(ui, "云端暂存", Page::Vault.subtitle());
    theme::card_titled(ui, "加密上传", |ui| {
        ui.horizontal(|ui| {
            ui.label(theme::muted("文件路径"));
            theme::input(ui, &mut app.vault_upload_path, "要加密上传的文件", 320.0);
            if platform::browse_supported() && theme::secondary_button(ui, "浏览…") {
                if let Some(path) = platform::pick_file("选择要加密上传的文件") {
                    app.vault_upload_path = path.display().to_string();
                }
            }
            ui.label(theme::muted("保留天数"));
            ui.add(egui::DragValue::new(&mut app.vault_ttl_days).range(1..=30));
            if theme::primary_button(ui, "加密上传") {
                let path = PathBuf::from(app.vault_upload_path.trim());
                if path.as_os_str().is_empty() {
                    app.toast(false, "请先填写上传文件路径");
                } else {
                    app.begin_busy("正在加密并上传…");
                    let _ = app.cmd_tx.send(Cmd::VaultPut {
                        path,
                        ttl_days: app.vault_ttl_days,
                    });
                }
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(theme::muted("下载目录"));
            theme::input(ui, &mut app.vault_download_dir, "解密输出目录", 340.0);
            if platform::browse_supported() && theme::secondary_button(ui, "浏览…") {
                if let Some(dir) = platform::pick_folder("选择解密输出文件夹") {
                    app.vault_download_dir = dir.display().to_string();
                }
            }
            if theme::secondary_button(ui, "打开目录") {
                let _ = std::fs::create_dir_all(&app.vault_download_dir);
                if let Err(error) =
                    platform::open_path(PathBuf::from(&app.vault_download_dir).as_path())
                {
                    app.toast(false, format!("{error:#}"));
                }
            }
            if theme::secondary_button(ui, "刷新列表") {
                let _ = app.cmd_tx.send(Cmd::VaultList);
            }
        });
    });
    theme::card_titled(ui, "暂存对象", |ui| {
        if app.vault_items.is_empty() {
            theme::empty_state(ui, "暂无暂存对象（点击「刷新列表」从服务器拉取）。");
            return;
        }
        egui::Grid::new("vault-grid")
            .num_columns(5)
            .spacing([20.0, 12.0])
            .show(ui, |ui| {
                for header in ["对象 ID", "名称", "大小", "到期时间", "操作"] {
                    ui.label(
                        egui::RichText::new(header)
                            .size(12.0)
                            .color(theme::muted_color())
                            .strong(),
                    );
                }
                ui.end_row();
                for item in app.vault_items.clone() {
                    ui.label(
                        egui::RichText::new(&item.object_id)
                            .monospace()
                            .size(11.5)
                            .color(theme::muted_color()),
                    );
                    ui.label(egui::RichText::new(&item.name).color(theme::text()));
                    ui.label(theme::muted(platform::human_bytes(item.size)));
                    ui.label(theme::muted(platform::format_ms(item.expires_at_ms)));
                    if theme::secondary_button(ui, "下载解密") {
                        app.begin_busy(format!("正在下载并解密 {}…", item.name));
                        let _ = app.cmd_tx.send(Cmd::VaultGet {
                            object_id: item.object_id.clone(),
                            out_dir: PathBuf::from(&app.vault_download_dir),
                        });
                    }
                    ui.end_row();
                }
            });
    });
}

// ---------------------------------------------------------------------------
// 页面：设置
// ---------------------------------------------------------------------------

fn load_settings(app: &mut GuiApp) {
    let Ok(store) = Store::open(&app.data_dir) else {
        return;
    };
    let config = store.load_config().unwrap_or_default();
    let identity = store.load_identity().ok();
    let settings = SettingsState {
        name: identity
            .as_ref()
            .map(|identity| identity.name.clone())
            .unwrap_or_default(),
        relay: config.relay_url.clone(),
        token: config.access_token.clone(),
        poll_ms: config.poll_ms,
        share_text: config.clipboard.share_text,
        share_images: config.clipboard.share_images,
        autostart: platform::autostart_enabled(),
        update: None,
        update_message: None,
        checking_update: false,
        loaded: true,
    };
    app.controls
        .share_text
        .store(settings.share_text, Ordering::SeqCst);
    app.controls
        .share_images
        .store(settings.share_images, Ordering::SeqCst);
    app.controls
        .poll_ms
        .store(settings.poll_ms.clamp(200, 5000), Ordering::SeqCst);
    app.settings = settings;
}

fn page_settings(app: &mut GuiApp, ui: &mut egui::Ui) {
    if !app.settings.loaded {
        load_settings(app);
    }
    theme::page_header(ui, "设置", Page::Settings.subtitle());

    theme::card_titled(ui, "外观主题", |ui| {
        ui.label(theme::muted(
            "「跟随系统」会读取操作系统外观设置并自动切换；修改立即生效，保存在 <数据目录>/gui.json（与 CLI config.json 分离）。",
        ));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            for mode in ThemeMode::ALL {
                let selected = app.theme_mode == mode;
                if theme::check_mark(ui, selected, mode.label()) && !selected {
                    app.set_theme_mode(ui.ctx(), mode);
                }
            }
            ui.label(theme::muted(format!(
                "当前：{}（{}）",
                app.theme_mode.label(),
                if theme::is_light() {
                    "浅色"
                } else {
                    "深色"
                }
            )));
        });
        ui.add_space(4.0);
        // 注意：不要在这里调用 platform::system_prefers_light()。Windows 下它会
        // 每帧 spawn 一次 reg.exe，设置页会因此严重卡顿/假死；使用启动时缓存的
        // 系统外观即可（跟随系统模式下由 maybe_follow_system_theme 节流刷新）。
        ui.label(theme::muted(match app.system_light {
            Some(true) => "系统外观：浅色",
            Some(false) => "系统外观：深色",
            None => "系统外观：未知（跟随系统时回退深色）",
        }));
    });

    theme::card_titled(ui, "启动与窗口行为", |ui| {
        let mut auto_start = app.auto_start_sync;
        if ui
            .checkbox(&mut auto_start, "打开应用时自动开始同步（打开即同步）")
            .changed()
        {
            app.auto_start_sync = auto_start;
            app.save_gui_prefs();
            app.toast(
                true,
                if auto_start {
                    "已启用：打开应用自动连接中继并开始同步"
                } else {
                    "已关闭：打开应用后需手动点「开始同步」"
                },
            );
        }
        ui.label(theme::muted(
            "默认开启：已完成向导后打开应用即自动连接中继/剪切板/目录，无需点击按钮。",
        ));
        ui.add_space(6.0);
        let mut hide_to_tray = app.hide_to_tray;
        let tray_supported = crate::tray::supported();
        if ui
            .add_enabled(
                tray_supported,
                egui::Checkbox::new(&mut hide_to_tray, "最小化/关闭窗口时隐藏到系统托盘"),
            )
            .changed()
        {
            app.hide_to_tray = hide_to_tray;
            app.save_gui_prefs();
            app.toast(
                true,
                if hide_to_tray {
                    "已启用：最小化或点 X 隐藏到系统托盘（同步继续）"
                } else {
                    "已关闭：最小化保留在任务栏；关闭窗口将退出"
                },
            );
        }
        if tray_supported {
            ui.label(theme::muted(
                "最小化或点窗口 X 默认隐藏到系统托盘，同步不中断；托盘左键显示主界面，右键菜单可开始/停止同步或退出。",
            ));
        } else {
            ui.label(theme::muted(
                "当前平台未启用系统托盘（Linux 回退任务栏窗口）；关闭窗口将直接退出。",
            ));
        }
    });

    theme::card_titled(ui, "中继与设备", |ui| {
        egui::Grid::new("settings-conn")
            .num_columns(2)
            .spacing([16.0, 12.0])
            .show(ui, |ui| {
                ui.label(theme::muted("设备名称"));
                theme::input(ui, &mut app.settings.name, "例如：我的笔记本", 380.0);
                ui.end_row();
                ui.label(theme::muted("中继地址"));
                theme::input(ui, &mut app.settings.relay, RELAY_PLACEHOLDER, 380.0);
                ui.end_row();
                ui.label(theme::muted("访问令牌"));
                ui.add(
                    egui::TextEdit::singleline(&mut app.settings.token)
                        .password(true)
                        .hint_text(
                            egui::RichText::new("服务器共享令牌").color(theme::muted_color()),
                        )
                        .desired_width(380.0)
                        .margin(egui::vec2(10.0, 8.0)),
                );
                ui.end_row();
            });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if theme::primary_button(ui, "保存设置") {
                app.save_device_name(&app.settings.name.clone());
                let relay = app.settings.relay.clone();
                let token = app.settings.token.clone();
                let poll = app.settings.poll_ms;
                app.save_config(&relay, &token, poll);
            }
            if theme::secondary_button(ui, "测试连接") {
                let _ = app.cmd_tx.send(Cmd::TestConnection {
                    relay_url: app.settings.relay.clone(),
                    access_token: app.settings.token.clone(),
                });
            }
            if let Some(message) = &app.settings.update_message {
                ui.label(theme::muted(message.clone()));
            }
        });
    });
    theme::card_titled(ui, "开机自启", |ui| {
        let mut autostart = app.settings.autostart;
        if ui
            .checkbox(&mut autostart, "登录系统后自动启动 drive-sync GUI")
            .changed()
        {
            match platform::set_autostart(autostart) {
                Ok(()) => {
                    app.settings.autostart = autostart;
                    app.toast(
                        true,
                        if autostart {
                            "已启用开机自启"
                        } else {
                            "已取消开机自启"
                        },
                    );
                }
                Err(error) => {
                    app.settings.autostart = platform::autostart_enabled();
                    app.toast(false, format!("{error:#}"));
                }
            }
        }
        #[cfg(target_os = "windows")]
        ui.label(theme::muted(
            "Windows：写入 HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
        ));
        #[cfg(target_os = "linux")]
        ui.label(theme::muted(
            "Linux：写入 ~/.config/autostart/drive-sync-gui.desktop",
        ));
        #[cfg(target_os = "macos")]
        ui.label(theme::muted(
            "macOS：写入 ~/Library/LaunchAgents/dev.drivesync.gui.plist",
        ));
        #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
        ui.label(theme::muted("当前平台暂不支持开机自启。"));
    });
    theme::card_titled(ui, "自动更新", |ui| {
        ui.horizontal(|ui| {
            theme::kv(ui, "当前版本", format!("v{}", env!("CARGO_PKG_VERSION")));
            if theme::secondary_button(ui, "检查更新") && !app.settings.checking_update {
                app.settings.checking_update = true;
                app.settings.update_message = Some("正在检查更新…".to_string());
                let _ = app.cmd_tx.send(Cmd::CheckUpdate);
            }
            if app.settings.checking_update {
                ui.spinner();
            }
        });
        if let Some(info) = &app.settings.update {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                theme::kv(
                    ui,
                    "最新版本",
                    format!("v{}（当前 v{}）", info.latest, info.current),
                );
                theme::hint(
                    ui,
                    Some(info.available),
                    if info.available {
                        "有新版本可用"
                    } else {
                        "已是最新版本"
                    },
                );
                if info.available && theme::primary_button(ui, "立即更新") {
                    let _ = app.cmd_tx.send(Cmd::ApplyUpdate);
                }
            });
            if !info.notes.trim().is_empty() {
                ui.add_space(4.0);
                ui.collapsing("更新说明", |ui| {
                    ui.label(theme::muted(info.notes.clone()));
                });
            }
        }
        if let Some(message) = &app.settings.update_message {
            ui.label(theme::muted(message.clone()));
        }
    });
    theme::card_titled(ui, "数据目录与关于", |ui| {
        ui.horizontal(|ui| {
            ui.label(theme::muted("数据目录"));
            ui.label(
                egui::RichText::new(app.data_dir.display().to_string())
                    .monospace()
                    .size(12.0)
                    .color(theme::text()),
            );
            if theme::secondary_button(ui, "打开数据目录")
                && platform::open_path(&app.data_dir).is_err()
            {
                app.toast(false, "打开数据目录失败");
            }
        });
        ui.add_space(8.0);
        ui.label(theme::muted(
            "drive-sync 桌面客户端：共享剪切板 + 软件目录 + 可选加密暂存；中继只转发密文。",
        ));
        ui.label(theme::muted(format!(
            "客户端 v{} · 协议 v{}",
            env!("CARGO_PKG_VERSION"),
            drive_client::PROTOCOL_VERSION
        )));
        ui.horizontal(|ui| {
            ui.hyperlink_to("GitHub 项目主页", "https://github.com/go-enols/drive-sync");
            if theme::secondary_button(ui, "关于详情") {
                app.dialog = Dialog::About;
            }
            if theme::secondary_button(ui, "退出 GUI") {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    });
}

// ---------------------------------------------------------------------------
// 页面：日志
// ---------------------------------------------------------------------------

fn page_logs(app: &mut GuiApp, ui: &mut egui::Ui) {
    theme::page_header(ui, "日志", Page::Logs.subtitle());
    ui.horizontal(|ui| {
        if theme::secondary_button(ui, "清空") {
            logbuf::clear();
            app.toast(true, "日志已清空");
        }
        if theme::secondary_button(ui, "复制全部") {
            ui.ctx().copy_text(logbuf::full_text());
            app.toast(true, "日志已复制到剪贴板");
        }
        if theme::secondary_button(ui, "打开日志目录") {
            let dir = app.data_dir.join("logs");
            let _ = std::fs::create_dir_all(&dir);
            if let Err(error) = platform::open_path(&dir) {
                app.toast(false, format!("{error:#}"));
            }
        }
        if theme::secondary_button(ui, "刷新") {
            app.toast(true, "日志已刷新");
        }
        if let Some(path) = logbuf::log_path() {
            ui.label(
                egui::RichText::new(path.display().to_string())
                    .monospace()
                    .size(11.0)
                    .color(theme::muted_color()),
            );
        }
    });
    ui.add_space(10.0);
    let lines = logbuf::snapshot(600);
    theme::card(ui, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(540.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if lines.is_empty() {
                    ui.label(theme::muted("（暂无日志）"));
                }
                for line in &lines {
                    ui.label(theme::mono(line));
                }
            });
    });
}

// ---------------------------------------------------------------------------
// 对话框 / Toast
// ---------------------------------------------------------------------------

fn draw_dialogs(app: &mut GuiApp, ctx: &egui::Context) {
    let dialog = std::mem::replace(&mut app.dialog, Dialog::None);
    let mut keep_open = false;
    match &dialog {
        Dialog::None => {}
        Dialog::RemovePeer { peer_id, name } => {
            let mut close = false;
            egui::Window::new("删除配对记录")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(egui::RichText::new(format!("确认删除与 {name}（{peer_id}）的配对记录？")).color(theme::text()));
                    ui.label(theme::muted(
                        "对端在线会立即收到撤销通知；离线时会在其上线后自动清理。删除后需要重新配对才能恢复同步。",
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if theme::secondary_button(ui, "取消") {
                            close = true;
                        }
                        if theme::danger_button(ui, "确认删除") {
                            match Store::open(&app.data_dir)
                                .and_then(|store| store.remove_peer(peer_id))
                            {
                                Ok(_) => {
                                    app.toast(true, "已删除配对记录（同步引擎在 2 秒内检测并通知对端，离线对端上线后自动清理）");
                                    app.refresh_store();
                                }
                                Err(error) => {
                                    app.toast(false, format!("删除失败: {error:#}"));
                                }
                            }
                            close = true;
                        }
                    });
                });
            keep_open = !close;
        }
        Dialog::RemoveCatalog { id, name } => {
            let mut close = false;
            egui::Window::new("删除目录条目")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(
                        egui::RichText::new(format!("确认删除目录条目「{name}」？"))
                            .color(theme::text()),
                    );
                    ui.label(theme::muted(
                        "将写入墓碑并同步给其他设备，不会删除本机文件。",
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if theme::secondary_button(ui, "取消") {
                            close = true;
                        }
                        if theme::danger_button(ui, "确认删除") {
                            let _ = app.cmd_tx.send(Cmd::CatalogRemove { id: id.clone() });
                            close = true;
                        }
                    });
                });
            keep_open = !close;
        }
        Dialog::About => {
            let mut close = false;
            egui::Window::new("关于 drive-sync")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(
                        egui::RichText::new("drive-sync 桌面客户端")
                            .size(20.0)
                            .strong()
                            .color(theme::text()),
                    );
                    ui.add_space(6.0);
                    theme::kv(ui, "客户端版本", format!("v{}", env!("CARGO_PKG_VERSION")));
                    theme::kv(
                        ui,
                        "协议版本",
                        format!("v{}", drive_client::PROTOCOL_VERSION),
                    );
                    ui.add_space(6.0);
                    ui.label(theme::muted(
                        "中继不可信：剪切板 / 目录元数据 / 文件内容都端到端加密。",
                    ));
                    ui.label(theme::muted(
                        "软件本体留在持有设备本地，中继只中转加密分块。",
                    ));
                    ui.label(theme::muted("仓库：https://github.com/go-enols/drive-sync"));
                    ui.add_space(8.0);
                    if theme::secondary_button(ui, "关闭") {
                        close = true;
                    }
                });
            keep_open = !close;
        }
    }
    if keep_open {
        app.dialog = dialog;
    }
}

fn draw_toast(app: &mut GuiApp, ctx: &egui::Context) {
    let Some(toast) = &app.toast else {
        return;
    };
    let color = if toast.ok {
        theme::mint()
    } else {
        theme::danger()
    };
    egui::TopBottomPanel::bottom("toast")
        .frame(
            egui::Frame::new()
                .fill(theme::panel_dark())
                .stroke(egui::Stroke::new(1.0, theme::border()))
                .inner_margin(egui::Margin::symmetric(18, 10)),
        )
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&toast.text).color(color).strong());
            });
        });
}

// ---------------------------------------------------------------------------
// 截图模式
// ---------------------------------------------------------------------------

impl GuiApp {
    fn apply_shot_action(&mut self, action: &ShotAction) {
        match action {
            ShotAction::WizardStep(step) => {
                self.screen_wizard = true;
                self.wizard.step = *step;
            }
            ShotAction::Page(page) => {
                self.screen_wizard = false;
                self.page = *page;
                if *page == Page::Settings {
                    load_settings(self);
                }
                if *page == Page::Vault && self.vault_items.is_empty() {
                    self.vault_items = vec![
                        VaultItem {
                            object_id: "obj-1a2b3c4d5e6f7a8b".to_string(),
                            name: "季度备份.zip".to_string(),
                            size: 24 * 1024 * 1024,
                            expires_at_ms: drive_core::util::now_ms() + 5 * 24 * 60 * 60 * 1000,
                        },
                        VaultItem {
                            object_id: "obj-9f8e7d6c5b4a3928".to_string(),
                            name: "设计稿.psd".to_string(),
                            size: 8 * 1024 * 1024,
                            expires_at_ms: drive_core::util::now_ms() + 22 * 24 * 60 * 60 * 1000,
                        },
                    ];
                }
            }
        }
    }

    fn handle_screenshot_events(&mut self, ctx: &egui::Context) {
        let Some(plan) = self.shots.as_ref() else {
            return;
        };
        let dir = plan.dir.clone();
        let labels: Vec<String> = plan.steps.iter().map(|step| step.label.clone()).collect();
        let events = ctx.input(|input| input.events.clone());
        for event in events {
            if let egui::Event::Screenshot {
                user_data, image, ..
            } = event
            {
                let id = user_data
                    .data
                    .as_ref()
                    .and_then(|data| data.downcast_ref::<u64>())
                    .copied()
                    .unwrap_or(0);
                if id == 0 || id as usize > labels.len() {
                    continue;
                }
                let label = labels[id as usize - 1].as_str();
                let path = dir.join(format!("{label}.png"));
                match screenshots::save_color_image(&path, &image) {
                    Ok(()) => logbuf::log(format!("截图已保存: {}", path.display())),
                    Err(error) => logbuf::log(format!("截图保存失败: {error:#}")),
                }
            }
        }
    }

    fn run_shot_plan(&mut self, ctx: &egui::Context) {
        let Some(plan) = self.shots.as_mut() else {
            return;
        };
        if plan.index >= plan.steps.len() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if plan.frames > 0 {
            plan.frames -= 1;
            ctx.request_repaint();
            return;
        }
        if !plan.action_applied {
            let action = plan.steps[plan.index].action.clone();
            plan.action_applied = true;
            plan.frames = 18;
            self.apply_shot_action(&action);
            ctx.request_repaint();
            return;
        }
        if !plan.shot_sent {
            let id = (plan.index + 1) as u64;
            plan.shot_sent = true;
            plan.frames = 12;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(id)));
            ctx.request_repaint();
            return;
        }
        plan.index += 1;
        plan.action_applied = false;
        plan.shot_sent = false;
        plan.frames = 4;
        ctx.request_repaint();
    }
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

fn push_limited(buffer: &mut VecDeque<String>, line: String, limit: usize) {
    buffer.push_back(line);
    while buffer.len() > limit {
        buffer.pop_front();
    }
}

#[derive(Default)]
struct ShareTreeNode {
    dirs: BTreeMap<String, ShareTreeNode>,
    files: Vec<(String, CatalogEntry)>,
}

fn build_share_tree(entries: Vec<CatalogEntry>) -> ShareTreeNode {
    let mut root = ShareTreeNode::default();
    for entry in entries {
        let mut parts: Vec<&str> = entry
            .name
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        let Some(file) = parts.pop() else {
            continue;
        };
        let mut node = &mut root;
        for dir in parts {
            node = node.dirs.entry(dir.to_string()).or_default();
        }
        node.files.push((file.to_string(), entry));
    }
    root
}

/// 按目录树渲染共享文件夹；子目录默认折叠，展开时才构建下一层（懒加载 UI）。
fn render_share_tree(ui: &mut egui::Ui, app: &mut GuiApp, node: &ShareTreeNode, depth: usize) {
    for (dir, child) in &node.dirs {
        egui::CollapsingHeader::new(format!("{dir}/"))
            .default_open(false)
            .show(ui, |ui| render_share_tree(ui, app, child, depth + 1));
    }
    for (file, entry) in &node.files {
        let local = app.local_index.contains_key(&entry.id);
        let holders: Vec<String> = app
            .peer_have
            .iter()
            .filter(|(_, entries)| entries.iter().any(|id| id == &entry.id))
            .map(|(peer_id, _)| peer_name(app, peer_id))
            .collect();
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(file)
                    .monospace()
                    .size(12.5)
                    .color(theme::text()),
            )
            .on_hover_text(&entry.name);
            ui.label(theme::muted(platform::human_bytes(entry.size)));
            if local {
                ui.label(theme::muted("本机"));
            } else {
                if holders.is_empty() {
                    ui.label(theme::muted("等待持有设备上线"));
                } else {
                    ui.label(theme::muted(format!("来自 {}", holders.join("、"))));
                }
                // 即使当前没有在线持有设备也显示下载入口，后台会等待/报错，
                // 避免用户只“看得见”却没有任何操作。
                if theme::secondary_button(ui, "下载") {
                    app.begin_busy(format!("正在下载 {file}…"));
                    let _ = app.cmd_tx.send(Cmd::FetchCatalog {
                        entry_id: entry.id.clone(),
                        out_dir: app.data_dir.join("incoming"),
                    });
                }
            }
        });
    }
}

/// 目录条目是否来自「共享文件夹」（对端可浏览并逐个下载的文件）。
fn is_shared_entry(entry: &CatalogEntry) -> bool {
    entry.tags.iter().any(|tag| tag == "shared-folder")
}

fn tag_value<'a>(entry: &'a CatalogEntry, prefix: &str) -> Option<&'a str> {
    entry
        .tags
        .iter()
        .find_map(|tag| tag.strip_prefix(prefix))
        .filter(|value| !value.is_empty())
}

fn peer_name(app: &GuiApp, peer_id: &str) -> String {
    app.peers
        .iter()
        .find(|peer| peer.peer.device_id == peer_id)
        .map(|peer| peer.display_name())
        .unwrap_or_else(|| peer_id.to_string())
}

fn clipboard_kind_text(kind: ClipboardKind) -> &'static str {
    match kind {
        ClipboardKind::Text => "文本",
        ClipboardKind::RichText => "富文本",
        ClipboardKind::Image => "图片",
        ClipboardKind::Files => "文件",
    }
}
