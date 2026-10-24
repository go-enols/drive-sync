#![allow(dead_code)] // 托盘实现按平台 cfg 使用；Linux 回退任务栏时部分类型仅保留接口

//! 系统托盘（Windows / macOS）。Linux 不编译 tray-icon，回退为普通任务栏窗口。
//!
//! 托盘事件回调运行在系统事件线程，不能直接访问 `GuiApp`；因此这里用 [`TrayBridge`]
//! （`egui::Context` + 后台命令通道均可跨线程使用）直接执行「显示/开始/停止/退出」。
//! 窗口隐藏后 eframe 会暂停 UI 重绘，所以恢复必须由托盘回调直接发送 Viewport 命令。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use egui::Context;

use crate::backend::Cmd;
use crate::logbuf;

/// 托盘「退出」标记：主循环看到后不再把关闭请求改为隐藏。
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn quit_requested() -> bool {
    QUIT_REQUESTED.load(Ordering::SeqCst)
}

/// 托盘 → 主程序动作桥（可在任意线程调用）。
#[derive(Clone)]
pub struct TrayBridge {
    pub ctx: Context,
    pub cmd_tx: Sender<Cmd>,
    pub remote_clipboard: Sender<drive_core::clipboard::ClipboardPayload>,
    pub data_dir: PathBuf,
    /// Windows HWND（UI 线程每帧更新），用于隐藏/显示原生窗口。
    pub hwnd: Arc<AtomicIsize>,
}

impl TrayBridge {
    fn native_visible(&self, visible: bool) {
        #[cfg(target_os = "windows")]
        {
            let hwnd = self.hwnd.load(Ordering::SeqCst);
            if hwnd == 0 {
                return;
            }
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                SetForegroundWindow, ShowWindow, SW_HIDE, SW_SHOW,
            };
            let handle = hwnd as *mut core::ffi::c_void;
            unsafe {
                ShowWindow(handle, if visible { SW_SHOW } else { SW_HIDE });
                if visible {
                    SetForegroundWindow(handle);
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = visible;
        }
    }

    pub fn show(&self) {
        self.native_visible(true);
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::Visible(true));
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.ctx.request_repaint();
        logbuf::log("托盘：显示主界面");
    }

    pub fn hide(&self) {
        self.native_visible(false);
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::Visible(false));
        logbuf::log("托盘：窗口已隐藏（同步继续运行）");
    }

    pub fn start_sync(&self) {
        let Ok(store) = drive_client::Store::open(&self.data_dir) else {
            logbuf::log("托盘：读取数据目录失败，无法开始同步");
            return;
        };
        let Ok(config) = store.load_config() else {
            logbuf::log("托盘：读取配置失败，无法开始同步");
            return;
        };
        if config.relay_url.trim().is_empty() {
            logbuf::log("托盘：尚未配置中继，无法开始同步");
            return;
        }
        let _ = self.cmd_tx.send(Cmd::StartSync {
            relay_url: config.relay_url,
            access_token: config.access_token,
            remote_clipboard: self.remote_clipboard.clone(),
        });
        logbuf::log("托盘：开始同步");
    }

    pub fn stop_sync(&self) {
        let _ = self.cmd_tx.send(Cmd::StopSync);
        logbuf::log("托盘：停止同步");
    }

    pub fn quit(&self) {
        QUIT_REQUESTED.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(Cmd::StopSync);
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        self.ctx.request_repaint();
        logbuf::log("托盘：退出");
        // 隐藏状态下事件循环可能暂停；兜底确保进程退出。
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(1200));
            std::process::exit(0);
        });
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod imp {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    };

    use super::TrayBridge;

    pub struct Tray {
        icon: TrayIcon,
        tooltip: String,
    }

    impl Tray {
        /// 创建托盘图标；失败返回 None（UI 仍可正常使用，仅无托盘）。
        pub fn new(ctx: &egui::Context, tooltip: &str, bridge: TrayBridge) -> Option<Tray> {
            let menu = Menu::new();
            let show = MenuItem::with_id("show", "显示主界面", true, None);
            let start = MenuItem::with_id("start", "开始同步", true, None);
            let stop = MenuItem::with_id("stop", "停止同步", true, None);
            let quit = MenuItem::with_id("quit", "退出", true, None);
            let sep1 = PredefinedMenuItem::separator();
            let sep2 = PredefinedMenuItem::separator();
            menu.append_items(&[&show, &sep1, &start, &stop, &sep2, &quit])
                .ok()?;
            let icon = Icon::from_rgba(mint_icon_rgba(64), 64, 64).ok()?;
            let tray = TrayIconBuilder::new()
                .with_icon(icon)
                .with_tooltip(tooltip)
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .build()
                .ok()?;

            let tray_bridge = bridge.clone();
            TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    tray_bridge.show();
                }
            }));
            let menu_bridge = bridge;
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| match event.id.0.as_str() {
                "show" => menu_bridge.show(),
                "start" => menu_bridge.start_sync(),
                "stop" => menu_bridge.stop_sync(),
                "quit" => menu_bridge.quit(),
                _ => {}
            }));
            let _ = ctx;
            Some(Tray {
                icon: tray,
                tooltip: tooltip.to_string(),
            })
        }

        /// 更新 tooltip（随同步状态变化）。
        pub fn set_tooltip(&mut self, text: &str) {
            if self.tooltip == text {
                return;
            }
            if self.icon.set_tooltip(Some(text)).is_ok() {
                self.tooltip = text.to_string();
            }
        }
    }

    /// 64x64 薄荷绿圆角方块 + 深色折线 logo。
    pub fn mint_icon_rgba(size: u32) -> Vec<u8> {
        let s = size as f32;
        let radius = s * 0.28;
        let mut rgba = Vec::with_capacity((size * size * 4) as usize);
        let points = [
            (0.32 * s, 0.52 * s),
            (0.48 * s, 0.28 * s),
            (0.56 * s, 0.48 * s),
            (0.70 * s, 0.24 * s),
        ];
        let in_bolt = |x: f32, y: f32| -> bool {
            let mut inside = false;
            let mut j = points.len() - 1;
            for i in 0..points.len() {
                let (xi, yi) = points[i];
                let (xj, yj) = points[j];
                if ((yi > y) != (yj > y)) && (x < (xj - xi) * (y - yi) / (yj - yi) + xi) {
                    inside = !inside;
                }
                j = i;
            }
            inside
        };
        for y in 0..size {
            for x in 0..size {
                let fx = x as f32 + 0.5;
                let fy = y as f32 + 0.5;
                let cx = fx.clamp(radius, s - radius);
                let cy = fy.clamp(radius, s - radius);
                let dist = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                if dist > radius {
                    rgba.extend_from_slice(&[0, 0, 0, 0]);
                } else if in_bolt(fx, fy) {
                    rgba.extend_from_slice(&[8, 17, 12, 255]);
                } else {
                    rgba.extend_from_slice(&[14, 163, 127, 255]);
                }
            }
        }
        rgba
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod imp {
    use super::TrayBridge;

    /// Linux 等平台不启用 tray-icon（避免 libappindicator 系统依赖），回退任务栏。
    pub struct Tray;

    impl Tray {
        pub fn new(_ctx: &egui::Context, _tooltip: &str, _bridge: TrayBridge) -> Option<Tray> {
            None
        }
        pub fn set_tooltip(&mut self, _text: &str) {}
    }
}

pub use imp::Tray;

/// 当前平台是否支持系统托盘。
pub const fn supported() -> bool {
    cfg!(any(target_os = "windows", target_os = "macos"))
}
