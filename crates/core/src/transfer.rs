//! 文件中转（软件本体/大文件）：分块清单与接收端落盘校验。
//!
//! 数据只在设备间端到端加密传输，中继服务器只转发密文分块，不落盘。
//! 原型采用"有序流 + 断点偏移"：接收端只接受连续块，中断后从已收字节数续传；
//! 生产可升级为 Merkle 块树（乱序/多路径/损坏块精确重传）。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::util::{now_ms, random_hex};
use crate::{CoreError, Result};

/// 默认块大小 64 KiB：兼顾吞吐（大文件）与延迟（小文件/信令通道）。
pub const DEFAULT_CHUNK_SIZE: u32 = 64 * 1024;
pub const MIN_CHUNK_SIZE: u32 = 4 * 1024;
pub const MAX_CHUNK_SIZE: u32 = 1024 * 1024;

/// 传输清单：接收端据此重建文件并校验。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransferManifest {
    pub v: u32,
    pub transfer_id: String,
    #[serde(default)]
    pub entry_id: Option<String>,
    pub file_name: String,
    pub size: u64,
    /// 整个文件的 BLAKE3 hex。
    pub content_hash: String,
    pub chunk_size: u32,
    pub created_at_ms: u64,
}

impl TransferManifest {
    pub fn new(
        file_name: impl Into<String>,
        size: u64,
        content_hash: impl Into<String>,
        entry_id: Option<String>,
    ) -> Self {
        Self {
            v: 1,
            transfer_id: format!("tx-{}", random_hex(8)),
            entry_id,
            file_name: file_name.into(),
            size,
            content_hash: content_hash.into(),
            chunk_size: DEFAULT_CHUNK_SIZE,
            created_at_ms: now_ms(),
        }
    }

    pub fn with_chunk_size(mut self, chunk_size: u32) -> Self {
        self.chunk_size = chunk_size.clamp(MIN_CHUNK_SIZE, MAX_CHUNK_SIZE);
        self
    }

    pub fn chunk_count(&self) -> u64 {
        if self.size == 0 {
            0
        } else {
            self.size.div_ceil(self.chunk_size as u64)
        }
    }

    /// 第 index 块的字节偏移。
    pub fn chunk_offset(&self, index: u64) -> Option<u64> {
        let offset = index.checked_mul(self.chunk_size as u64)?;
        (offset < self.size).then_some(offset)
    }

    /// 第 index 块的实际长度（最后一块可能更短）。
    pub fn chunk_len(&self, index: u64) -> Option<u64> {
        let offset = self.chunk_offset(index)?;
        Some((self.size - offset).min(self.chunk_size as u64))
    }

    /// 校验清单自身是否合法。
    pub fn validate(&self) -> Result<()> {
        if self.v != 1 {
            return Err(CoreError::Invalid(format!("不支持的传输版本: {}", self.v)));
        }
        if self.chunk_size < MIN_CHUNK_SIZE || self.chunk_size > MAX_CHUNK_SIZE {
            return Err(CoreError::Invalid(format!(
                "非法块大小: {}",
                self.chunk_size
            )));
        }
        if self.file_name.is_empty() || self.file_name.contains(['/', '\\']) {
            return Err(CoreError::Invalid("文件名非法（不允许路径分隔符）".into()));
        }
        if self.content_hash.len() != 64
            || !self.content_hash.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err(CoreError::Invalid("content_hash 必须是 64 字符 hex".into()));
        }
        Ok(())
    }
}

/// 生成上传计划：[(index, offset, len)]。
pub fn chunk_plan(size: u64, chunk_size: u32) -> Vec<(u64, u64, u64)> {
    let mut out = Vec::new();
    let mut offset = 0u64;
    let mut index = 0u64;
    while offset < size {
        let len = (size - offset).min(chunk_size as u64);
        out.push((index, offset, len));
        offset += len;
        index += 1;
    }
    out
}

/// 接收端：按顺序写入临时文件，完成后校验哈希并原子重命名。
pub struct TransferReceiver {
    manifest: TransferManifest,
    temp_path: PathBuf,
    final_path: PathBuf,
    file: Option<File>,
    hasher: blake3::Hasher,
    received: u64,
}

impl TransferReceiver {
    /// 在 `dir` 下创建临时接收文件（`<dir>/.incoming-<transfer_id>.part`）。
    pub fn new(manifest: TransferManifest, dir: &Path) -> Result<Self> {
        manifest.validate()?;
        std::fs::create_dir_all(dir)?;
        let safe_name = sanitize_file_name(&manifest.file_name);
        let temp_path = dir.join(format!(".incoming-{}.part", manifest.transfer_id));
        let final_path = dir.join(&safe_name);
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_path)?;
        Ok(Self {
            manifest,
            temp_path,
            final_path,
            file: Some(file),
            hasher: blake3::Hasher::new(),
            received: 0,
        })
    }

    /// 从已有 `.part` 文件续传（崩溃恢复）：返回已接收字节数并重建 hasher。
    ///
    /// 只保留**块边界对齐**的前缀（最后一块可能只写了一半，发送端必须从块边界
    /// 重新发送）。`received_bytes()` 即续传偏移。
    pub fn resume_from_disk(manifest: TransferManifest, dir: &Path) -> Result<Self> {
        manifest.validate()?;
        std::fs::create_dir_all(dir)?;
        let safe_name = sanitize_file_name(&manifest.file_name);
        let temp_path = dir.join(format!(".incoming-{}.part", manifest.transfer_id));
        let final_path = dir.join(&safe_name);

        let mut existing = Vec::new();
        if temp_path.exists() {
            File::open(&temp_path)?.read_to_end(&mut existing)?;
        }
        let size = manifest.size as usize;
        let chunk = manifest.chunk_size as usize;
        let mut keep = existing.len().min(size);
        if keep < size && keep % chunk != 0 {
            keep -= keep % chunk;
        }
        existing.truncate(keep);

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_path)?;
        file.write_all(&existing)?;
        file.seek(SeekFrom::End(0))?;

        let mut hasher = blake3::Hasher::new();
        hasher.update(&existing);
        Ok(Self {
            manifest,
            temp_path,
            final_path,
            file: Some(file),
            hasher,
            received: keep as u64,
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

    /// 写入一块。要求 `offset == received`（有序流），否则返回可续传错误。
    pub fn push_chunk(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if offset != self.received {
            return Err(CoreError::Protocol(format!(
                "块偏移不连续: 期望 {}, 实际 {}（应从 {} 续传）",
                self.received, offset, self.received
            )));
        }
        if self.received + data.len() as u64 > self.manifest.size {
            return Err(CoreError::Protocol("写入超过清单声明的大小".into()));
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| CoreError::Protocol("接收器已关闭".into()))?;
        file.write_all(data)?;
        self.hasher.update(data);
        self.received += data.len() as u64;
        Ok(())
    }

    /// 写完并校验；成功后返回最终文件路径。
    pub fn finish(mut self) -> Result<PathBuf> {
        if self.received != self.manifest.size {
            return Err(CoreError::Protocol(format!(
                "文件不完整: 已收 {}, 期望 {}",
                self.received, self.manifest.size
            )));
        }
        let digest = self.hasher.finalize().to_hex().to_string();
        if digest != self.manifest.content_hash {
            self.cleanup_partial();
            return Err(CoreError::Verification(format!(
                "文件 BLAKE3 校验失败: 期望 {}, 实际 {}",
                self.manifest.content_hash, digest
            )));
        }
        if let Some(mut file) = self.file.take() {
            file.flush()?;
            file.sync_all()?;
        }
        if self.final_path.exists() {
            std::fs::remove_file(&self.final_path)?;
        }
        std::fs::rename(&self.temp_path, &self.final_path)?;
        Ok(self.final_path)
    }

    pub fn abort(mut self) {
        self.cleanup_partial();
    }

    fn cleanup_partial(&mut self) {
        self.file.take();
        let _ = std::fs::remove_file(&self.temp_path);
    }
}

fn sanitize_file_name(name: &str) -> String {
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

/// 发送端辅助：读取文件并逐块产出。
pub struct FileChunker {
    pub file: File,
    pub manifest: TransferManifest,
    pub next_index: u64,
}

impl FileChunker {
    pub fn open(path: &Path, entry_id: Option<String>, chunk_size: u32) -> Result<Self> {
        let meta = std::fs::metadata(path)?;
        if !meta.is_file() {
            return Err(CoreError::Invalid(format!(
                "不是普通文件: {}",
                path.display()
            )));
        }
        let mut hasher = blake3::Hasher::new();
        let mut file = File::open(path)?;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let content_hash = hasher.finalize().to_hex().to_string();
        let file_name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file.bin".to_string());
        let manifest = TransferManifest::new(file_name, meta.len(), content_hash, entry_id)
            .with_chunk_size(chunk_size);
        Ok(Self {
            file: File::open(path)?,
            manifest,
            next_index: 0,
        })
    }

    /// 读取下一块；返回 `(index, offset, data)`，读完后返回 None。
    pub fn next_chunk(&mut self) -> Result<Option<(u64, u64, Vec<u8>)>> {
        let Some(len) = self.manifest.chunk_len(self.next_index) else {
            return Ok(None);
        };
        let offset = self.manifest.chunk_offset(self.next_index).unwrap();
        self.file.seek(SeekFrom::Start(offset))?;
        let mut data = vec![0u8; len as usize];
        self.file.read_exact(&mut data)?;
        let index = self.next_index;
        self.next_index += 1;
        Ok(Some((index, offset, data)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::blake3_hex;

    #[test]
    fn manifest_chunk_math() {
        let mut m = TransferManifest::new("a.bin", 10 * 1024 + 5, "0".repeat(64), None);
        m.chunk_size = 4096;
        assert_eq!(m.chunk_count(), 3);
        assert_eq!(m.chunk_offset(0), Some(0));
        assert_eq!(m.chunk_len(0), Some(4096));
        assert_eq!(m.chunk_offset(2), Some(8192));
        assert_eq!(m.chunk_len(2), Some(2048 + 5));
        assert_eq!(m.chunk_offset(3), None);
        assert!(m.validate().is_ok());
    }

    #[test]
    fn receiver_writes_and_verifies() {
        let dir = std::env::temp_dir().join(format!("dsync-rx-{}", random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let data = b"drive-sync chunked transfer test payload".repeat(1000);
        let hash = blake3_hex(&data);
        let manifest =
            TransferManifest::new("out.bin", data.len() as u64, hash, None).with_chunk_size(4096);
        let mut rx = TransferReceiver::new(manifest.clone(), &dir).unwrap();
        for (_, offset, len) in chunk_plan(data.len() as u64, manifest.chunk_size) {
            rx.push_chunk(offset, &data[offset as usize..(offset + len) as usize])
                .unwrap();
        }
        let path = rx.finish().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), data);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn receiver_rejects_bad_hash() {
        let dir = std::env::temp_dir().join(format!("dsync-rx2-{}", random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let data = b"payload";
        let manifest = TransferManifest::new("x.bin", data.len() as u64, "0".repeat(64), None);
        let mut rx = TransferReceiver::new(manifest, &dir).unwrap();
        rx.push_chunk(0, data).unwrap();
        assert!(rx.finish().is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resume_from_disk_keeps_aligned_prefix_and_completes() {
        let dir = std::env::temp_dir().join(format!("dsync-rx3-{}", random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let data = b"drive-sync resume test payload ".repeat(500); // 16000 字节
        let hash = blake3_hex(&data);
        let manifest = TransferManifest::new("resume.bin", data.len() as u64, hash, None)
            .with_chunk_size(4096);
        let temp_path = dir.join(format!(".incoming-{}.part", manifest.transfer_id));

        // 模拟崩溃：已写入 2 个完整块 + 半块（应丢弃半块）
        std::fs::write(&temp_path, &data[..4096 * 2 + 1000]).unwrap();

        let mut rx = TransferReceiver::resume_from_disk(manifest.clone(), &dir).unwrap();
        assert_eq!(rx.received_bytes(), 4096 * 2, "应只保留块边界对齐前缀");

        let plan = chunk_plan(data.len() as u64, manifest.chunk_size);
        for (_, offset, len) in plan.into_iter().filter(|(_, off, _)| *off >= 4096 * 2) {
            rx.push_chunk(offset, &data[offset as usize..(offset + len) as usize])
                .unwrap();
        }
        let path = rx.finish().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), data);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_chunker_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dsync-tx-{}", random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        std::fs::write(&src, vec![7u8; 10_000]).unwrap();
        let mut chunker = FileChunker::open(&src, None, 4096).unwrap();
        let manifest = chunker.manifest.clone();
        let mut total = 0u64;
        while let Some((_, _, data)) = chunker.next_chunk().unwrap() {
            total += data.len() as u64;
        }
        assert_eq!(total, 10_000);
        assert_eq!(manifest.chunk_count(), 3);
        std::fs::remove_dir_all(&dir).ok();
    }
}
