//! 安全替换当前可执行文件（自动更新安装步骤）。
//!
//! 原则：
//! - 只替换 **当前 exe 本身**，不碰数据目录/配置；
//! - 替换前保留旧文件备份（`.old-<tag>`），Windows 复制失败时自动回滚；
//! - Unix 用同目录临时文件 + `rename`（原子替换，运行中的进程继续用旧 inode，重启生效），
//!   并保留 0755 权限；
//! - 本模块的函数都接收显式路径，测试用临时目录模拟 exe，不会碰真实进程。

use std::fs;
use std::path::{Path, PathBuf};

use crate::{Result, UpdateError};

/// 安装结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallReport {
    /// 被替换（或创建）的可执行文件路径。
    pub target_exe: PathBuf,
    /// 旧文件备份路径（首次安装、目标不存在时为 `None`）。
    pub backup: Option<PathBuf>,
    /// 新文件字节数。
    pub installed_bytes: u64,
}

impl InstallReport {
    /// 备份的简短展示（无备份时返回 `"-"`）。
    pub fn backup_display(&self) -> String {
        match &self.backup {
            Some(path) => path.display().to_string(),
            None => "-".to_string(),
        }
    }
}

/// 替换 `current_exe` 为 `new_path`，备份标签自动取当前毫秒时间戳。
pub fn install_binary(new_path: &Path, current_exe: &Path) -> Result<InstallReport> {
    let tag = drive_core::util::now_ms().to_string();
    install_binary_with_tag(new_path, current_exe, &tag)
}

/// 替换 `current_exe` 为 `new_path`，备份后缀使用给定标签（如版本号）。
pub fn install_binary_with_tag(
    new_path: &Path,
    current_exe: &Path,
    tag: &str,
) -> Result<InstallReport> {
    let metadata = fs::metadata(new_path).map_err(|error| {
        UpdateError::Install(format!("新文件不可用 {}: {error}", new_path.display()))
    })?;
    if !metadata.is_file() {
        return Err(UpdateError::Install(format!(
            "新文件不是普通文件: {}",
            new_path.display()
        )));
    }
    if metadata.len() == 0 {
        return Err(UpdateError::Install(format!(
            "新文件为空: {}",
            new_path.display()
        )));
    }
    if current_exe.as_os_str().is_empty() {
        return Err(UpdateError::Install("当前 exe 路径为空".into()));
    }
    let tag = sanitize_tag(tag);
    let installed_bytes = metadata.len();

    #[cfg(windows)]
    {
        replace_windows(new_path, current_exe, &tag, installed_bytes)
    }
    #[cfg(unix)]
    {
        replace_unix(new_path, current_exe, &tag, installed_bytes)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (&tag, installed_bytes);
        Err(UpdateError::Install(
            "当前平台不支持原地替换可执行文件".into(),
        ))
    }
}

/// 备份文件路径：`<exe>.old-<tag>`（保留原扩展名，方便人工回滚）。
pub fn backup_path(current_exe: &Path, tag: &str) -> PathBuf {
    let mut text = current_exe.as_os_str().to_os_string();
    text.push(format!(".old-{}", sanitize_tag(tag)));
    PathBuf::from(text)
}

fn sanitize_tag(tag: &str) -> String {
    let mut out: String = tag
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if out.trim_matches('_').is_empty() {
        out = "manual".to_string();
    }
    out
}

#[cfg(windows)]
fn replace_windows(
    new_path: &Path,
    current_exe: &Path,
    tag: &str,
    installed_bytes: u64,
) -> Result<InstallReport> {
    let backup = if current_exe.exists() {
        let backup = backup_path(current_exe, tag);
        if backup.exists() {
            fs::remove_file(&backup).map_err(|error| {
                UpdateError::Install(format!("清理旧备份 {} 失败: {error}", backup.display()))
            })?;
        }
        // 运行中的 exe 可以被 rename（只是不能删除/覆盖），这是 Windows 自更新的常用做法。
        fs::rename(current_exe, &backup).map_err(|error| {
            UpdateError::Install(format!(
                "重命名当前 exe {} → {} 失败: {error}",
                current_exe.display(),
                backup.display()
            ))
        })?;
        Some(backup)
    } else {
        None
    };

    match fs::copy(new_path, current_exe) {
        Ok(_) => {
            // 确保复制完整：copy 的返回值就是字节数，与源文件元数据一致。
            Ok(InstallReport {
                target_exe: current_exe.to_path_buf(),
                backup,
                installed_bytes,
            })
        }
        Err(error) => {
            // 回滚：删掉可能写了一半的新文件，把旧 exe 放回去。
            let _ = fs::remove_file(current_exe);
            let mut rollback_error = None;
            if let Some(backup) = &backup {
                if let Err(restore) = fs::rename(backup, current_exe) {
                    rollback_error = Some(format!(
                        "回滚失败，旧版本仍保存在 {}，请手动恢复: {restore}",
                        backup.display()
                    ));
                }
            }
            Err(UpdateError::Install(format!(
                "复制新 exe 到 {} 失败: {error}{}",
                current_exe.display(),
                rollback_error
                    .map(|message| format!("；{message}"))
                    .unwrap_or_default()
            )))
        }
    }
}

#[cfg(unix)]
fn replace_unix(
    new_path: &Path,
    current_exe: &Path,
    tag: &str,
    installed_bytes: u64,
) -> Result<InstallReport> {
    use std::os::unix::fs::PermissionsExt;

    let parent = current_exe
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            UpdateError::Install(format!(
                "无法确定当前 exe 的目录: {}",
                current_exe.display()
            ))
        })?;
    let file_name = current_exe
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| {
            UpdateError::Install(format!(
                "当前 exe 路径缺少文件名: {}",
                current_exe.display()
            ))
        })?;
    // 同目录临时文件：保证 rename 在同一文件系统内原子完成。
    let temp_path = parent.join(format!(".{file_name}.update-{tag}.tmp"));
    if temp_path.exists() {
        fs::remove_file(&temp_path)?;
    }
    let prepared = (|| -> Result<()> {
        fs::copy(new_path, &temp_path)?;
        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o755))?;
        Ok(())
    })();
    if let Err(error) = prepared {
        let _ = fs::remove_file(&temp_path);
        return Err(UpdateError::Install(format!(
            "准备新 exe 失败（{}）: {error}",
            temp_path.display()
        )));
    }

    // 先复制一份旧文件备份（原子 rename 失败时原文件仍在，备份用于人工回滚）。
    let backup = if current_exe.exists() {
        let backup = backup_path(current_exe, tag);
        if backup.exists() {
            let _ = fs::remove_file(&backup);
        }
        fs::copy(current_exe, &backup).map_err(|error| {
            let _ = fs::remove_file(&temp_path);
            UpdateError::Install(format!("备份旧 exe 失败: {error}"))
        })?;
        Some(backup)
    } else {
        None
    };

    match fs::rename(&temp_path, current_exe) {
        Ok(()) => Ok(InstallReport {
            target_exe: current_exe.to_path_buf(),
            backup,
            installed_bytes,
        }),
        Err(error) => {
            let _ = fs::remove_file(&temp_path);
            if let Some(backup) = &backup {
                let _ = fs::remove_file(backup);
            }
            Err(UpdateError::Install(format!(
                "原子替换 {} 失败: {error}",
                current_exe.display()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_binary_and_keeps_backup() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("drive-sync.exe");
        let new_file = dir.path().join("downloaded.exe");
        std::fs::write(&current, b"old-binary").unwrap();
        std::fs::write(&new_file, b"new-binary").unwrap();

        let report = install_binary_with_tag(&new_file, &current, "0.3.0").unwrap();
        assert_eq!(report.target_exe, current);
        assert_eq!(report.installed_bytes, 10);
        assert_eq!(std::fs::read(&current).unwrap(), b"new-binary");
        let backup = report.backup.expect("应保留旧版本备份");
        assert_eq!(std::fs::read(&backup).unwrap(), b"old-binary");
        assert!(backup.to_string_lossy().contains(".old-0.3.0"));
    }

    #[test]
    fn installs_when_target_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("fresh.exe");
        let new_file = dir.path().join("downloaded.exe");
        std::fs::write(&new_file, b"fresh-binary").unwrap();
        let report = install_binary(&new_file, &current).unwrap();
        assert!(report.backup.is_none());
        assert_eq!(std::fs::read(&current).unwrap(), b"fresh-binary");
    }

    #[test]
    fn refuses_missing_or_empty_new_file_without_touching_target() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("drive-sync.exe");
        std::fs::write(&current, b"old").unwrap();
        let missing = dir.path().join("missing.exe");
        assert!(install_binary_with_tag(&missing, &current, "x").is_err());
        assert_eq!(std::fs::read(&current).unwrap(), b"old");

        let empty = dir.path().join("empty.exe");
        std::fs::write(&empty, b"").unwrap();
        assert!(install_binary_with_tag(&empty, &current, "x").is_err());
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
    }

    #[test]
    fn tag_is_sanitized_for_backup_file_name() {
        assert_eq!(sanitize_tag("v0.3.0"), "v0.3.0");
        assert_eq!(sanitize_tag("../../evil"), ".._.._evil");
        assert_eq!(sanitize_tag(""), "manual");
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("exe");
        std::fs::write(&current, b"old").unwrap();
        let new_file = dir.path().join("new");
        std::fs::write(&new_file, b"new").unwrap();
        let report = install_binary_with_tag(&new_file, &current, "../evil/tag").unwrap();
        let backup = report.backup.unwrap();
        assert_eq!(backup.parent(), current.parent());
        assert!(backup
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".old-.._evil_tag"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_install_keeps_executable_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("drive-sync");
        std::fs::write(&current, b"old").unwrap();
        let new_file = dir.path().join("new");
        std::fs::write(&new_file, b"#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(&new_file, fs::Permissions::from_mode(0o644)).unwrap();
        install_binary_with_tag(&new_file, &current, "0.3.0").unwrap();
        let mode = std::fs::metadata(&current).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }
}
