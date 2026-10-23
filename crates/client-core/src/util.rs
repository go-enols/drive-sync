//! 客户端通用工具：文件哈希、时间、路径处理。

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

pub use drive_core::util::{b64d, b64e, blake3_hex, now_ms, random_hex};

/// 计算文件大小与 BLAKE3 摘要（流式，避免大文件占内存）。
pub fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut file = File::open(path).with_context(|| format!("打开文件失败: {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("读取文件失败: {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, hasher.finalize().to_hex().to_string()))
}

/// 文件 mtime（Unix 毫秒），取不到返回 `None`。
pub fn file_mtime_ms(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
}

/// 当前 Unix 毫秒（`SystemTime` 版本，便于比较文件时间）。
pub fn system_time_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// 原子写文本文件（同目录临时文件 + rename，Windows 上 rename 覆盖已存在文件）。
pub fn write_text_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let tmp: PathBuf = path.with_file_name(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        random_hex(4)
    ));
    std::fs::write(&tmp, data).with_context(|| format!("写临时文件失败: {}", tmp.display()))?;
    let mut last_err = None;
    for attempt in 0..5u64 {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(10 * (attempt + 1)));
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Err(anyhow::anyhow!(
        "原子写入 {} 失败: {}",
        path.display(),
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "未知错误".into())
    ))
}

/// 简单指数退避 + ±20% 抖动：1s, 2s, 4s, ... 上限 30s。
pub fn backoff_delay(attempt: u32) -> std::time::Duration {
    let base_ms = 1000u64.saturating_mul(1u64 << attempt.min(5)).min(30_000);
    let jitter_permille = 800 + (now_ms() % 401); // 800..=1200
    let ms = (base_ms * jitter_permille / 1000).clamp(500, 36_000);
    std::time::Duration::from_millis(ms)
}
