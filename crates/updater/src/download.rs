//! 下载（HTTP/HTTPS/file://）与文件完整性校验。
//!
//! - 网络下载使用 reqwest blocking + rustls（不依赖系统 OpenSSL）；
//! - 支持 `file://` 与裸本地路径，便于离线/CI 自测；
//! - 所有下载都受 `max_bytes` 上限保护，先写 `.part` 临时文件，成功后 rename，
//!   失败不会留下半截目标文件。

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{Result, UpdateError};

/// 下载结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadReport {
    /// 最终文件路径。
    pub path: PathBuf,
    /// 下载字节数（与文件大小一致）。
    pub bytes: u64,
}

/// 下载 `url` 到 `dest`，最多允许 `max_bytes` 字节。
///
/// 支持 `https://`、`http://`、`file://` 与裸本地路径。成功返回写入的字节数。
pub fn download_to_file(url: &str, dest: &Path, max_bytes: u64) -> Result<DownloadReport> {
    let url = url.trim();
    if url.is_empty() {
        return Err(UpdateError::Http("下载地址为空".into()));
    }
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // 先写同目录 `.part` 临时文件，成功后再 rename，避免中断留下半截文件。
    let mut part = dest.as_os_str().to_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    let bytes = (|| -> Result<u64> {
        let mut file = File::create(&part)?;
        let written = if is_http_url(url) {
            let response = http_get(url)?;
            let mut response = response;
            copy_limited(&mut response, &mut file, max_bytes)?
        } else {
            let path = local_path(url)?;
            let metadata = std::fs::metadata(&path).map_err(|error| {
                UpdateError::Http(format!("打开本地文件 {} 失败: {error}", path.display()))
            })?;
            if metadata.len() > max_bytes {
                return Err(UpdateError::TooLarge { max: max_bytes });
            }
            let mut source = File::open(&path)?;
            copy_limited(&mut source, &mut file, max_bytes)?
        };
        file.flush()?;
        file.sync_all()?;
        if written == 0 {
            return Err(UpdateError::EmptyDownload);
        }
        Ok(written)
    })();
    match bytes {
        Ok(bytes) => {
            // Windows 上 rename 不允许覆盖已存在文件，先删掉旧缓存。
            if dest.exists() {
                std::fs::remove_file(dest)?;
            }
            std::fs::rename(&part, dest).inspect_err(|_| {
                let _ = std::fs::remove_file(&part);
            })?;
            Ok(DownloadReport {
                path: dest.to_path_buf(),
                bytes,
            })
        }
        Err(error) => {
            let _ = std::fs::remove_file(&part);
            Err(error)
        }
    }
}

/// 下载/读取一个小文本（清单），最多 `max_bytes` 字节。
pub fn fetch_text(url: &str, max_bytes: u64) -> Result<String> {
    let url = url.trim();
    if url.is_empty() {
        return Err(UpdateError::Http("清单地址为空".into()));
    }
    let mut buffer = Vec::new();
    if is_http_url(url) {
        let mut response = http_get(url)?;
        copy_limited(&mut response, &mut buffer, max_bytes)?;
    } else {
        let path = local_path(url)?;
        let metadata = std::fs::metadata(&path).map_err(|error| {
            UpdateError::Http(format!("打开本地清单 {} 失败: {error}", path.display()))
        })?;
        if metadata.len() > max_bytes {
            return Err(UpdateError::TooLarge { max: max_bytes });
        }
        let mut source = File::open(&path)?;
        copy_limited(&mut source, &mut buffer, max_bytes)?;
    }
    if buffer.is_empty() {
        return Err(UpdateError::EmptyDownload);
    }
    String::from_utf8(buffer)
        .map_err(|error| UpdateError::Http(format!("清单不是 UTF-8 文本: {error}")))
}

/// 校验文件大小与 BLAKE3。
pub fn verify_file(path: &Path, blake3_hex: &str, size: u64) -> Result<()> {
    let metadata = std::fs::metadata(path)?;
    let actual_size = metadata.len();
    if actual_size != size {
        return Err(UpdateError::SizeMismatch {
            expected: size,
            actual: actual_size,
        });
    }
    let (hashed_size, actual_hash) = hash_file(path)?;
    if hashed_size != size {
        return Err(UpdateError::SizeMismatch {
            expected: size,
            actual: hashed_size,
        });
    }
    let expected = blake3_hex.trim().to_ascii_lowercase();
    if expected != actual_hash {
        return Err(UpdateError::HashMismatch {
            expected,
            actual: actual_hash,
        });
    }
    Ok(())
}

/// 计算文件大小与 BLAKE3（流式，不整文件读入内存）。
pub fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total = total.saturating_add(read as u64);
    }
    Ok((total, hasher.finalize().to_hex().to_string()))
}

// ---------------------------------------------------------------------------
// 内部工具
// ---------------------------------------------------------------------------

fn is_http_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

fn local_path(url: &str) -> Result<PathBuf> {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("file://") {
        let text = percent_decode(rest);
        // `file:///D:/path` → `/D:/path`：Windows 盘符前的前导 `/` 要去掉。
        #[cfg(windows)]
        {
            let bytes = text.as_bytes();
            if text.starts_with('/')
                && bytes.len() >= 3
                && bytes[1].is_ascii_alphabetic()
                && bytes[2] == b':'
            {
                text.remove(0);
            }
        }
        if text.is_empty() {
            return Err(UpdateError::Http(format!("file:// 地址缺少路径: {url}")));
        }
        return Ok(PathBuf::from(text));
    }
    if url.contains("://") {
        return Err(UpdateError::Http(format!(
            "不支持的下载协议（仅 https/http/file）: {url}"
        )));
    }
    Ok(PathBuf::from(url))
}

fn http_get(url: &str) -> Result<reqwest::blocking::Response> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("drive-sync-updater/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        // 大文件在慢速网络下可能需要几分钟；上限由 size/max_bytes 控制。
        .timeout(Duration::from_secs(30 * 60))
        .build()
        .map_err(|error| UpdateError::Http(format!("初始化 HTTP 客户端失败: {error}")))?;
    let response = client
        .get(url)
        .send()
        .map_err(|error| UpdateError::Http(format!("请求 {url} 失败: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(UpdateError::Http(format!(
            "请求 {url} 返回 HTTP {}",
            status.as_u16()
        )));
    }
    Ok(response)
}

fn copy_limited<R: Read, W: Write>(reader: &mut R, writer: &mut W, max_bytes: u64) -> Result<u64> {
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > max_bytes {
            return Err(UpdateError::TooLarge { max: max_bytes });
        }
        writer.write_all(&buffer[..read])?;
    }
    Ok(total)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_url(path: &Path) -> String {
        format!("file://{}", path.display())
    }

    #[test]
    fn hash_and_verify_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asset.bin");
        let payload = b"drive-sync updater test payload";
        std::fs::write(&path, payload).unwrap();
        let (size, hash) = hash_file(&path).unwrap();
        assert_eq!(size, payload.len() as u64);
        assert_eq!(hash, blake3::hash(payload).to_hex().to_string());
        verify_file(&path, &hash, size).unwrap();
        // 大小写不敏感。
        verify_file(&path, &hash.to_uppercase(), size).unwrap();
        // 内容损坏。
        std::fs::write(&path, b"tampered").unwrap();
        assert!(matches!(
            verify_file(&path, &hash, size),
            Err(UpdateError::SizeMismatch { .. })
        ));
        std::fs::write(&path, payload).unwrap();
        let other_hash = blake3::hash(b"other").to_hex().to_string();
        assert!(matches!(
            verify_file(&path, &other_hash, size),
            Err(UpdateError::HashMismatch { .. })
        ));
    }

    #[test]
    fn download_file_url_and_fetch_text() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.bin");
        std::fs::write(&source, b"hello update").unwrap();
        let dest = dir.path().join("nested").join("dest.bin");
        let report = download_to_file(&local_url(&source), &dest, 1024).unwrap();
        assert_eq!(report.bytes, 12);
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello update");
        assert!(!dest.with_file_name("dest.bin.part").exists());

        let text = fetch_text(&local_url(&source), 1024).unwrap();
        assert_eq!(text, "hello update");
    }

    #[test]
    fn download_supports_bare_paths() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.bin");
        std::fs::write(&source, b"bare").unwrap();
        let dest = dir.path().join("dest.bin");
        download_to_file(source.to_str().unwrap(), &dest, 100).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"bare");
    }

    #[test]
    fn download_respects_max_bytes_and_leaves_no_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("big.bin");
        std::fs::write(&source, vec![7u8; 4096]).unwrap();
        let dest = dir.path().join("dest.bin");
        assert!(matches!(
            download_to_file(&local_url(&source), &dest, 1024),
            Err(UpdateError::TooLarge { .. })
        ));
        assert!(!dest.exists());
        let mut part = dest.as_os_str().to_os_string();
        part.push(".part");
        assert!(!PathBuf::from(part).exists());
        assert!(matches!(
            fetch_text(&local_url(&source), 1024),
            Err(UpdateError::TooLarge { .. })
        ));
    }

    #[test]
    fn download_reports_missing_local_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest.bin");
        assert!(
            download_to_file(&local_url(&dir.path().join("missing.bin")), &dest, 1024).is_err()
        );
        assert!(!dest.exists());
    }

    #[test]
    fn rejects_unknown_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest.bin");
        assert!(download_to_file("ftp://example.invalid/x", &dest, 1024).is_err());
    }

    #[test]
    fn percent_decodes_file_urls() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("with space.bin");
        std::fs::write(&source, b"spaced").unwrap();
        let encoded = local_url(&source).replace(' ', "%20");
        let dest = dir.path().join("dest.bin");
        download_to_file(&encoded, &dest, 1024).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"spaced");
    }
}
