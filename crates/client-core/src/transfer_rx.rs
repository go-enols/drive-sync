//! 文件接收端：`.part` 临时文件、按块顺序写入、BLAKE3 校验、原子重命名、断点续传。
//!
//! 与 `derive_core::transfer::TransferReceiver` 的差异：本实现可真正从已有 `.part`
//! 恢复（读回前缀重建哈希、丢弃未对齐尾部），这是协议 §7.3 "重连续传" 需要的。
//! 块按 `offset == received` 顺序追加；发送端在 `transfer_accept.from_offset` 之后
//! 也必须从对齐到块边界的偏移继续。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use derive_core::transfer::TransferManifest;

/// 接收中的文件（`.part` 在未完成前保留，进程崩溃后可续传）。
pub struct PartialReceiver {
    manifest: TransferManifest,
    temp_path: PathBuf,
    final_path: PathBuf,
    file: File,
    hasher: blake3::Hasher,
    received: u64,
}

impl PartialReceiver {
    /// 打开（或恢复）接收器。已存在的 `.part` 会被读回并校验：
    /// 未对齐到块边界/超过声明大小的尾部会被截掉（接收端丢弃未对齐尾部）。
    pub fn open(manifest: &TransferManifest, dir: &Path) -> Result<Self> {
        manifest
            .validate()
            .map_err(|e| anyhow!("传输清单非法: {e}"))?;
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建接收目录失败: {}", dir.display()))?;
        let temp_path = dir.join(format!(".incoming-{}.part", manifest.transfer_id));
        let final_path = dir.join(sanitize_file_name(&manifest.file_name));
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&temp_path)
            .with_context(|| format!("打开临时文件失败: {}", temp_path.display()))?;

        let mut received = file.metadata()?.len().min(manifest.size);
        let chunk = manifest.chunk_size as u64;
        if received % chunk != 0 && received < manifest.size {
            // 丢弃未对齐尾部（正常不会发生：崩溃时最后一块可能写了一半）。
            received = (received / chunk) * chunk;
        }
        if received != file.metadata()?.len() {
            file.set_len(received)?;
        }
        file.seek(SeekFrom::Start(0))?;
        let mut hasher = blake3::Hasher::new();
        let mut remaining = received;
        let mut buf = vec![0u8; 256 * 1024];
        while remaining > 0 {
            let want = (buf.len() as u64).min(remaining) as usize;
            let n = file
                .read(&mut buf[..want])
                .with_context(|| format!("读取临时文件失败: {}", temp_path.display()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            remaining -= n as u64;
        }
        file.seek(SeekFrom::Start(received))?;
        Ok(Self {
            manifest: manifest.clone(),
            temp_path,
            final_path,
            file,
            hasher,
            received,
        })
    }

    pub fn manifest(&self) -> &TransferManifest {
        &self.manifest
    }

    pub fn received_bytes(&self) -> u64 {
        self.received
    }

    pub fn remaining(&self) -> u64 {
        self.manifest.size.saturating_sub(self.received)
    }

    pub fn is_complete(&self) -> bool {
        self.received >= self.manifest.size
    }

    pub fn temp_path(&self) -> &Path {
        &self.temp_path
    }

    /// 追加一块，要求 `offset == received`（有序流）。
    pub fn push_chunk(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if offset != self.received {
            bail!(
                "块偏移不连续: 期望 {}, 实际 {}（接收 {} 字节）",
                self.received,
                offset,
                self.received
            );
        }
        if self.received + data.len() as u64 > self.manifest.size {
            bail!("写入超过清单声明的大小");
        }
        self.file
            .write_all(data)
            .context("写入接收临时文件失败")?;
        self.hasher.update(data);
        self.received += data.len() as u64;
        Ok(())
    }

    /// 校验并原子重命名为最终文件；失败时删除 `.part`。
    pub fn finish(mut self) -> Result<PathBuf> {
        if self.received != self.manifest.size {
            bail!(
                "文件不完整: 已收 {}, 期望 {}",
                self.received,
                self.manifest.size
            );
        }
        let digest = self.hasher.finalize().to_hex().to_string();
        if digest != self.manifest.content_hash {
            let _ = std::fs::remove_file(&self.temp_path);
            bail!(
                "文件 BLAKE3 校验失败（期望 {}，实际 {}）",
                &self.manifest.content_hash[..16.min(self.manifest.content_hash.len())],
                &digest[..16.min(digest.len())]
            );
        }
        self.file.flush().context("刷新接收文件失败")?;
        self.file.sync_all().ok();
        drop(self.file);
        if self.final_path.exists() {
            std::fs::remove_file(&self.final_path).ok();
        }
        std::fs::rename(&self.temp_path, &self.final_path).with_context(|| {
            format!(
                "重命名接收文件失败: {} -> {}",
                self.temp_path.display(),
                self.final_path.display()
            )
        })?;
        Ok(self.final_path)
    }

    /// 放弃接收（删除 `.part`）。
    pub fn discard(self) {
        drop(self.file);
        let _ = std::fs::remove_file(&self.temp_path);
    }
}

/// 清洗文件名：不允许路径分隔符/控制字符（协议要求 `file_name` 不含路径）。
pub fn sanitize_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\' | '/') {
                '_'
            } else {
                c
            }
        })
        .collect();
    if cleaned.trim().is_empty() {
        "unnamed.bin".to_string()
    } else {
        cleaned
    }
}

/// 把逻辑偏移向上对齐到块边界（发送端断点续传用）。
pub fn align_up(offset: u64, chunk_size: u64) -> u64 {
    if chunk_size == 0 {
        return offset;
    }
    offset.div_ceil(chunk_size) * chunk_size
}

#[cfg(test)]
mod tests {
    use super::*;
    use derive_core::util::blake3_hex;
    use tempfile::TempDir;

    fn manifest(data: &[u8], chunk: u32) -> TransferManifest {
        TransferManifest::new("test.bin", data.len() as u64, blake3_hex(data), None)
            .with_chunk_size(chunk)
    }

    #[test]
    fn receive_and_verify() {
        let tmp = TempDir::new().unwrap();
        let data = b"derive-sync-partial-receiver-test".repeat(500);
        let m = manifest(&data, 4096);
        let mut rx = PartialReceiver::open(&m, tmp.path()).unwrap();
        assert_eq!(rx.received_bytes(), 0);
        let mut offset = 0u64;
        while offset < data.len() as u64 {
            let len = ((data.len() as u64) - offset).min(m.chunk_size as u64) as usize;
            rx.push_chunk(offset, &data[offset as usize..offset as usize + len])
                .unwrap();
            offset += len as u64;
        }
        let path = rx.finish().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), data);
    }

    #[test]
    fn resume_from_partial_file() {
        let tmp = TempDir::new().unwrap();
        let data = b"0123456789abcdef".repeat(1000);
        let m = manifest(&data, 4096);
        // 先写入前两块（跨越一次进程崩溃）
        {
            let mut rx = PartialReceiver::open(&m, tmp.path()).unwrap();
            rx.push_chunk(0, &data[..4096]).unwrap();
            rx.push_chunk(4096, &data[4096..8192]).unwrap();
            drop(rx); // 模拟崩溃：文件不 flush 也由 OS 保证写入
        }
        let mut rx = PartialReceiver::open(&m, tmp.path()).unwrap();
        assert_eq!(rx.received_bytes(), 8192);
        let mut offset = rx.received_bytes();
        while offset < data.len() as u64 {
            let len = ((data.len() as u64) - offset).min(m.chunk_size as u64) as usize;
            rx.push_chunk(offset, &data[offset as usize..offset as usize + len])
                .unwrap();
            offset += len as u64;
        }
        let path = rx.finish().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), data);
    }

    #[test]
    fn unaligned_tail_is_dropped() {
        let tmp = TempDir::new().unwrap();
        let data = b"abcdefgh".repeat(2000);
        let m = manifest(&data, 4096);
        {
            let mut rx = PartialReceiver::open(&m, tmp.path()).unwrap();
            rx.push_chunk(0, &data[..100]).unwrap();
            // 文件只有 100 字节（模拟写一半崩溃）
            drop(rx);
        }
        let rx = PartialReceiver::open(&m, tmp.path()).unwrap();
        assert_eq!(rx.received_bytes(), 0);
        rx.discard();
    }

    #[test]
    fn bad_hash_detected() {
        let tmp = TempDir::new().unwrap();
        let data = b"payload";
        let mut m = manifest(data, 4096);
        m.content_hash = "0".repeat(64);
        let mut rx = PartialReceiver::open(&m, tmp.path()).unwrap();
        rx.push_chunk(0, data).unwrap();
        assert!(rx.finish().is_err());
    }
}
