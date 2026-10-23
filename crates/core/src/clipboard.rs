//! 剪切板载荷：跨平台内容规范化、压缩、去重哈希。

use serde::{Deserialize, Serialize};

use crate::util::blake3_hex;
use crate::{CoreError, Result};

/// 剪切板内容类型。不同平台格式不同，跨端同步时统一映射到以下四类。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardKind {
    /// 纯文本（UTF-8）。
    Text,
    /// 富文本（HTML/RTF 等，`mime` 区分）。
    RichText,
    /// 图片（PNG 为跨端交换格式）。
    Image,
    /// 文件列表（原型不传输实体，只传占位；生产建议走软件目录/文件中转）。
    Files,
}

/// 传输压缩算法。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    #[default]
    None,
    Zstd,
}

/// 剪切板内容上限（32 MiB）：超过走文件传输通道，避免阻塞同步链路。
pub const MAX_CLIPBOARD_BYTES: u64 = 32 * 1024 * 1024;
/// 超过该大小才尝试压缩。
pub const COMPRESS_THRESHOLD: usize = 2048;
const ZSTD_LEVEL: i32 = 3;

/// 跨端传递的剪切板载荷。
///
/// `hash` 是**未压缩原始内容**的 BLAKE3 十六进制摘要，用于去重、去回环与完整性校验。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipboardPayload {
    pub v: u32,
    pub kind: ClipboardKind,
    pub mime: String,
    #[serde(default)]
    pub compression: Compression,
    pub raw_size: u64,
    pub hash: String,
    #[serde(with = "crate::util::b64_vec")]
    pub data: Vec<u8>,
}

impl ClipboardPayload {
    pub fn new(kind: ClipboardKind, mime: impl Into<String>, raw: Vec<u8>) -> Result<Self> {
        if raw.len() as u64 > MAX_CLIPBOARD_BYTES {
            return Err(CoreError::Invalid(format!(
                "剪切板内容 {} 字节超过上限 {} 字节",
                raw.len(),
                MAX_CLIPBOARD_BYTES
            )));
        }
        let hash = blake3_hex(&raw);
        let raw_size = raw.len() as u64;
        let (data, compression) = maybe_compress(raw)?;
        Ok(Self {
            v: 1,
            kind,
            mime: mime.into(),
            compression,
            raw_size,
            hash,
            data,
        })
    }

    pub fn text(s: &str) -> Result<Self> {
        Self::new(
            ClipboardKind::Text,
            "text/plain; charset=utf-8",
            s.as_bytes().to_vec(),
        )
    }

    pub fn html(html: &str) -> Result<Self> {
        Self::new(
            ClipboardKind::RichText,
            "text/html; charset=utf-8",
            html.as_bytes().to_vec(),
        )
    }

    /// PNG 图片。生产实现建议限制尺寸/转码（如最长边 4096、PNG 重编码）。
    pub fn png(png: Vec<u8>) -> Result<Self> {
        Self::new(ClipboardKind::Image, "image/png", png)
    }

    pub fn decompressed(&self) -> Result<Vec<u8>> {
        match self.compression {
            Compression::None => Ok(self.data.clone()),
            Compression::Zstd => zstd::bulk::decompress(&self.data, MAX_CLIPBOARD_BYTES as usize)
                .map_err(|e| CoreError::Codec(format!("zstd 解压失败: {e}"))),
        }
    }

    pub fn as_text(&self) -> Result<String> {
        if self.kind != ClipboardKind::Text && self.kind != ClipboardKind::RichText {
            return Err(CoreError::Invalid(format!(
                "载荷类型 {:?} 不是文本",
                self.kind
            )));
        }
        String::from_utf8(self.decompressed()?)
            .map_err(|e| CoreError::Invalid(format!("文本不是合法 UTF-8: {e}")))
    }

    /// 校验解压后内容与 `hash`、`raw_size` 一致。
    pub fn verify_integrity(&self) -> Result<()> {
        let raw = self.decompressed()?;
        if raw.len() as u64 != self.raw_size {
            return Err(CoreError::Verification(format!(
                "剪切板载荷大小不匹配: 期望 {}, 实际 {}",
                self.raw_size,
                raw.len()
            )));
        }
        if blake3_hex(&raw) != self.hash {
            return Err(CoreError::Verification("剪切板载荷 BLAKE3 不匹配".into()));
        }
        Ok(())
    }

    /// 人类可读摘要，用于日志/通知（不泄漏内容）。
    pub fn summary(&self) -> String {
        format!(
            "{:?} {} {} bytes pk={}B",
            self.kind,
            self.mime,
            self.raw_size,
            self.data.len()
        )
    }
}

fn maybe_compress(raw: Vec<u8>) -> Result<(Vec<u8>, Compression)> {
    if raw.len() < COMPRESS_THRESHOLD {
        return Ok((raw, Compression::None));
    }
    let compressed = zstd::bulk::compress(&raw, ZSTD_LEVEL)
        .map_err(|e| CoreError::Codec(format!("zstd 压缩失败: {e}")))?;
    // 压缩收益不足 5% 时不压缩，避免小文本反而变大。
    if compressed.len() + 16 < raw.len() * 95 / 100 {
        Ok((compressed, Compression::Zstd))
    } else {
        Ok((raw, Compression::None))
    }
}

/// 一次剪切板变化事件（E2E 加密后经中继转发）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipboardEvent {
    pub v: u32,
    pub event_id: String,
    pub origin_device: String,
    pub origin_seq: u64,
    pub created_at_ms: u64,
    pub payload: ClipboardPayload,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_roundtrip_and_integrity() {
        let payload = ClipboardPayload::text("你好，drive-sync!").unwrap();
        assert_eq!(payload.kind, ClipboardKind::Text);
        assert_eq!(payload.as_text().unwrap(), "你好，drive-sync!");
        payload.verify_integrity().unwrap();
    }

    #[test]
    fn large_text_is_compressed() {
        let text = "abc123".repeat(20_000);
        let payload = ClipboardPayload::text(&text).unwrap();
        assert_eq!(payload.compression, Compression::Zstd);
        assert!(payload.data.len() < payload.raw_size as usize);
        assert_eq!(payload.as_text().unwrap(), text);
    }

    #[test]
    fn tamper_detected() {
        let mut payload = ClipboardPayload::text("hello").unwrap();
        payload.hash = blake3_hex(b"other");
        assert!(payload.verify_integrity().is_err());
    }

    #[test]
    fn oversize_rejected() {
        let raw = vec![0u8; (MAX_CLIPBOARD_BYTES + 1) as usize];
        assert!(ClipboardPayload::new(ClipboardKind::Image, "image/png", raw).is_err());
    }
}
