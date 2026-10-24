//! drive-sync 桌面图形客户端入口。
//!
//! - 纯 Rust（eframe/egui），不依赖 Node/WebView；
//! - 首次运行自动进入向导，之后进入左侧导航主界面；
//! - 数据目录与 CLI 共用（identity.json/config.json/peers.json），支持 `--data-dir`；
//! - 单实例运行；release 构建隐藏控制台窗口。
//!
//! 本机验收截图模式（不提交产物）：
//! ```text
//! DRIVE_GUI_SHOT_DIR=target/gui-screenshots DRIVE_GUI_SHOT_MODE=wizard \
//!   drive-sync-gui --data-dir target/gui-demo-wizard
//! DRIVE_GUI_SHOT_DIR=target/gui-screenshots DRIVE_GUI_SHOT_MODE=main \
//!   drive-sync-gui --data-dir target/gui-demo-main
//! ```

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backend;
mod logbuf;
mod platform;
mod screenshots;
mod theme;
mod tray;

use std::path::PathBuf;
use std::sync::mpsc;

use app::{GuiApp, Page, ShotAction, ShotPlan, ShotStep};
use backend::{Backend, ClipboardControls};

struct Args {
    data_dir: Option<PathBuf>,
}

impl Args {
    fn parse() -> Self {
        let mut data_dir = None;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--data-dir" => data_dir = args.next().map(PathBuf::from),
                "-h" | "--help" => {
                    println!("drive-sync-gui [--data-dir <DIR>]");
                    println!("环境变量:");
                    println!("  DRIVE_SYNC_DATA_DIR   数据目录（与 CLI 共用）");
                    println!("  DRIVE_GUI_SHOT_DIR    截图输出目录（验收用，不入库）");
                    println!("  DRIVE_GUI_SHOT_MODE   wizard | main（截图模式）");
                    std::process::exit(0);
                }
                other if other.starts_with("--data-dir=") => {
                    data_dir = Some(PathBuf::from(&other["--data-dir=".len()..]));
                }
                other => {
                    eprintln!("忽略未知参数: {other}");
                }
            }
        }
        Self { data_dir }
    }
}

fn main() -> eframe::Result<()> {
    let args = Args::parse();
    let data_dir = args
        .data_dir
        .or_else(|| {
            std::env::var_os("DRIVE_SYNC_DATA_DIR")
                .map(PathBuf::from)
                .filter(|path| !path.as_os_str().is_empty())
        })
        .unwrap_or_else(platform::default_data_dir);
    let _ = std::fs::create_dir_all(&data_dir);
    let _ = logbuf::init(&data_dir);
    logbuf::log(format!("GUI 启动，数据目录: {}", data_dir.display()));

    let shot_dir = std::env::var("DRIVE_GUI_SHOT_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from);
    let shot_mode = std::env::var("DRIVE_GUI_SHOT_MODE")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if let Some(dir) = shot_dir.as_ref() {
        let _ = std::fs::create_dir_all(dir);
    }
    if shot_mode.as_deref() == Some("main") {
        if let Err(error) = screenshots::prepare_demo_data(&data_dir) {
            logbuf::log(format!("准备演示数据失败: {error:#}"));
        }
    }

    // 单实例：已有实例时用一个独立小窗口提示后退出。
    let instance_guard = match platform::acquire_single_instance(&data_dir) {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => {
            logbuf::log("检测到已有 GUI 实例在运行");
            return show_already_running();
        }
        Err(error) => {
            logbuf::log(format!("单实例检测失败（继续运行）: {error:#}"));
            None
        }
    };

    // 全局 tokio runtime（后台线程与异步任务共用）。
    let tokio_rt: &'static tokio::runtime::Runtime = Box::leak(Box::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("创建 tokio runtime 失败"),
    ));

    let store = drive_client::Store::open(&data_dir).expect("打开数据目录失败");
    let controls = ClipboardControls::from_store(&store);
    let (remote_tx, remote_rx) = mpsc::channel();
    let backend = Backend::spawn(tokio_rt, data_dir.clone(), controls);

    let shot_plan = shot_dir.map(|dir| ShotPlan::new(dir, shot_steps(shot_mode.as_deref())));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 700.0])
            .with_min_inner_size([860.0, 600.0])
            .with_title("drive-sync"),
        ..Default::default()
    };
    let shot_mode_for_app = shot_mode.clone();
    eframe::run_native(
        "drive-sync",
        options,
        Box::new(move |cc| {
            let app = GuiApp::new(
                cc,
                data_dir.clone(),
                backend,
                remote_rx,
                remote_tx,
                shot_plan,
                shot_mode_for_app.clone(),
                instance_guard,
            );
            Ok(Box::new(app))
        }),
    )
}

fn shot_steps(mode: Option<&str>) -> Vec<ShotStep> {
    let prefix = std::env::var("DRIVE_GUI_SHOT_PREFIX").unwrap_or_default();
    let step = |name: &str, action: ShotAction| ShotStep {
        label: format!("{prefix}{name}"),
        action,
    };
    match mode {
        Some("wizard") => vec![
            step("01-wizard-welcome", ShotAction::WizardStep(0)),
            step("02-wizard-device-name", ShotAction::WizardStep(1)),
            step("03-wizard-relay", ShotAction::WizardStep(2)),
            step("04-wizard-pairing", ShotAction::WizardStep(3)),
            step("05-wizard-finish", ShotAction::WizardStep(4)),
        ],
        _ => vec![
            step("06-main-status", ShotAction::Page(Page::Status)),
            step("07-devices", ShotAction::Page(Page::Devices)),
            step("08-clipboard", ShotAction::Page(Page::Clipboard)),
            step("09-catalog", ShotAction::Page(Page::Catalog)),
            step("10-vault", ShotAction::Page(Page::Vault)),
            step("11-settings", ShotAction::Page(Page::Settings)),
            step("12-logs", ShotAction::Page(Page::Logs)),
        ],
    }
}

/// 第二个实例：小窗口提示已运行，点击关闭退出。
fn show_already_running() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([440.0, 190.0])
            .with_title("drive-sync 已在运行"),
        ..Default::default()
    };
    eframe::run_native(
        "drive-sync",
        options,
        Box::new(|cc| {
            app::install_fonts_and_style(&cc.egui_ctx);
            Ok(Box::new(AlreadyRunningApp))
        }),
    )
}

struct AlreadyRunningApp;

impl eframe::App for AlreadyRunningApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(16.0);
            ui.heading("drive-sync 已在运行");
            ui.label("检测到另一个 drive-sync GUI 实例正在使用同一数据目录。");
            ui.label("如果任务栏里没有窗口，请先退出旧进程后再试。");
            ui.add_space(14.0);
            if ui.button("退出").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}
