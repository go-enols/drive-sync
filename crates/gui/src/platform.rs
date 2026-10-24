//! 平台相关：数据目录、单实例锁、开机自启（Windows HKCU Run）、打开路径。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// 与 CLI 共享的数据目录：`DRIVE_SYNC_DATA_DIR` > 平台默认。
pub fn default_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("DRIVE_SYNC_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.trim().is_empty() {
                return PathBuf::from(appdata).join(drive_core::APP_NAME);
            }
        }
        PathBuf::from(".").join(".drive-sync")
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join(drive_core::APP_NAME);
        }
        PathBuf::from(".").join(".drive-sync")
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            if !xdg.trim().is_empty() {
                return PathBuf::from(xdg).join(drive_core::APP_NAME);
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                return PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join(drive_core::APP_NAME);
            }
        }
        PathBuf::from(".drive-sync")
    }
}

/// 进程级单实例锁（Windows 用独占 handle，进程退出/崩溃时由系统释放）。
pub struct InstanceGuard {
    _file: std::fs::File,
    #[allow(dead_code)]
    path: PathBuf,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        #[cfg(not(target_os = "windows"))]
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub fn acquire_single_instance(data_dir: &Path) -> Result<Option<InstanceGuard>> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("创建数据目录失败: {}", data_dir.display()))?;
    let path = data_dir.join("gui.lock");
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // share_mode(0)：其他进程无法再打开该文件 → 第二个实例打开失败。
        match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                let _ = file.set_len(0);
                let _ = write!(file, "pid={}", std::process::id());
                Ok(Some(InstanceGuard { _file: file, path }))
            }
            Err(error) => {
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(32)
                {
                    Ok(None)
                } else {
                    Err(error).with_context(|| format!("创建单实例锁失败: {}", path.display()))
                }
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "pid={}", std::process::id());
                Ok(Some(InstanceGuard { _file: file, path }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("创建单实例锁失败: {}", path.display()))
            }
        }
    }
}

pub fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("获取当前可执行文件路径失败")
}

#[cfg(target_os = "windows")]
const AUTOSTART_VALUE: &str = "DriveSyncGui";
#[cfg(target_os = "windows")]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(target_os = "linux")]
fn linux_autostart_path() -> PathBuf {
    if let Ok(config) = std::env::var("XDG_CONFIG_HOME") {
        if !config.trim().is_empty() {
            return PathBuf::from(config)
                .join("autostart")
                .join("drive-sync-gui.desktop");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("autostart")
            .join("drive-sync-gui.desktop");
    }
    PathBuf::from(".config/autostart/drive-sync-gui.desktop")
}

#[cfg(target_os = "macos")]
fn macos_launch_agent_path() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("LaunchAgents")
            .join("dev.drivesync.gui.plist");
    }
    PathBuf::from("Library/LaunchAgents/dev.drivesync.gui.plist")
}

/// 是否已设置开机自启（Windows/Linux/macOS）。
pub fn autostart_enabled() -> bool {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("reg")
            .args(["query", RUN_KEY, "/v", AUTOSTART_VALUE])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(target_os = "linux")]
    {
        linux_autostart_path().is_file()
    }
    #[cfg(target_os = "macos")]
    {
        macos_launch_agent_path().is_file()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

/// 设置/取消开机自启。未实现的平台返回明确错误（UI 中该项禁用）。
pub fn set_autostart(enabled: bool) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        if enabled {
            let exe = current_exe()?;
            let command = format!("\"{}\"", exe.display());
            let status = std::process::Command::new("reg")
                .args([
                    "add",
                    RUN_KEY,
                    "/v",
                    AUTOSTART_VALUE,
                    "/t",
                    "REG_SZ",
                    "/d",
                    &command,
                    "/f",
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .context("调用 reg add 失败")?;
            if !status.success() {
                anyhow::bail!("写入开机自启注册表失败（reg add exit={status}）");
            }
        } else {
            let status = std::process::Command::new("reg")
                .args(["delete", RUN_KEY, "/v", AUTOSTART_VALUE, "/f"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .context("调用 reg delete 失败")?;
            // 项不存在时 reg 返回非 0，视为已取消。
            let _ = status;
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let path = linux_autostart_path();
        if enabled {
            let exe = current_exe()?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建自启目录失败: {}", parent.display()))?;
            }
            let desktop = format!(
                "[Desktop Entry]\nType=Application\nName=drive-sync\nComment=drive-sync 桌面客户端\nExec=\"{}\"\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
                exe.display()
            );
            std::fs::write(&path, desktop)
                .with_context(|| format!("写入自启文件失败: {}", path.display()))?;
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("删除自启文件失败: {}", path.display()));
                }
            }
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let path = macos_launch_agent_path();
        if enabled {
            let exe = current_exe()?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建 LaunchAgents 目录失败: {}", parent.display()))?;
            }
            let plist = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dev.drivesync.gui</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
                exe.display()
            );
            std::fs::write(&path, plist)
                .with_context(|| format!("写入 LaunchAgent 失败: {}", path.display()))?;
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("删除 LaunchAgent 失败: {}", path.display()));
                }
            }
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        let _ = enabled;
        anyhow::bail!("当前平台不支持开机自启")
    }
}

/// 用系统文件管理器打开目录/文件。
pub fn open_path(path: &Path) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("路径不存在: {}", path.display());
    }
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("explorer");
        command.arg(path);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(path);
        command
    };
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path);
        command
    };
    command
        .spawn()
        .with_context(|| format!("打开路径失败: {}", path.display()))?;
    Ok(())
}

/// 本地时间展示。
pub fn format_ms(ms: u64) -> String {
    if ms == 0 {
        return "-".to_string();
    }
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}

/// 人性化文件大小。
pub fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KIB {
        format!("{bytes} B")
    } else if value < KIB * KIB {
        format!("{:.1} KiB", value / KIB)
    } else if value < KIB * KIB * KIB {
        format!("{:.1} MiB", value / (KIB * KIB))
    } else {
        format!("{:.2} GiB", value / (KIB * KIB * KIB))
    }
}

/// 探测系统是否偏好浅色外观。
///
/// - Windows：`HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize` 的
///   `AppsUseLightTheme`（REG_DWORD，1=浅色）；
/// - Linux：`gsettings get org.gnome.desktop.interface color-scheme` / `gtk-theme`，
///   以及 `GTK_THEME` / `COLORSCHEME` 环境变量；
/// - macOS：`defaults read -g AppleInterfaceStyle`（存在且为 Dark 表示深色）。
///
/// 读取失败返回 `None`，调用方回退深色；不会 panic。
pub fn system_prefers_light() -> Option<bool> {
    #[cfg(target_os = "windows")]
    {
        let output = std::process::Command::new("reg")
            .args([
                "query",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
                "/v",
                "AppsUseLightTheme",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
        for token in text.split_whitespace() {
            if let Some(value) = token.strip_prefix("0x") {
                return Some(value != "0");
            }
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        fn run(command: &str, args: &[&str]) -> Option<String> {
            let output = std::process::Command::new(command)
                .args(args)
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            Some(String::from_utf8_lossy(&output.stdout).to_lowercase())
        }
        if let Some(scheme) = run(
            "gsettings",
            ["get", "org.gnome.desktop.interface", "color-scheme"].as_slice(),
        ) {
            if scheme.contains("light") {
                return Some(true);
            }
            if scheme.contains("dark") {
                return Some(false);
            }
        }
        if let Some(theme) = run(
            "gsettings",
            ["get", "org.gnome.desktop.interface", "gtk-theme"].as_slice(),
        ) {
            if theme.contains("dark") {
                return Some(false);
            }
            if theme.contains("light") {
                return Some(true);
            }
        }
        for variable in ["GTK_THEME", "COLORSCHEME"] {
            if let Ok(value) = std::env::var(variable) {
                let value = value.to_lowercase();
                if value.contains("light") {
                    return Some(true);
                }
                if value.contains("dark") {
                    return Some(false);
                }
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleInterfaceStyle"])
            .output()
            .ok()?;
        // 键不存在（标准浅色模式）时 defaults 返回非 0，按浅色处理。
        let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
        if output.status.success() {
            Some(!text.contains("dark"))
        } else if text.contains("does not exist") {
            Some(true)
        } else {
            None
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        None
    }
}
