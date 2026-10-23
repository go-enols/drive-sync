//! 剪切板后端：统一 trait + 系统剪切板（arboard，文本/PNG 图片）+ file 模式。
//!
//! - **auto**：优先同步文本；没有文本（或文本未变）时尝试图片；
//! - **text**：只同步文本；
//! - **image**：只同步图片（RGBA → PNG 跨端交换）；
//! - **file:<path>**：把文本内容读写到文件，供同机多实例测试；
//!   PNG 图片同样以原始字节写入文件。
//!
//! 轮询 + 哈希去重（Windows 上不需要事件窗口，300ms 默认足够；协议文档建议生产端
//! 使用 WM_CLIPBOARDUPDATE / XFixes 事件驱动，这里按任务范围实现轮询版本）。
//!
//! Linux 说明：arboard 在 Linux 走 X11（x11rb）。纯 Wayland 会话（无 XWayland）下无法
//! 读取系统剪切板，请使用 `--clipboard file:<path>`；KDE/wlroots 的 Wayland 会话通常
//! 带 XWayland 可正常工作。

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use drive_core::clipboard::{ClipboardKind, ClipboardPayload};

/// 剪切板读取/写入后端。
pub trait ClipboardBackend: Send {
    /// 轮询一次；仅当内容变化时返回载荷（内部按哈希去重）。
    fn read_change(&mut self) -> Option<ClipboardPayload>;
    /// 把远端内容写入本机剪切板。
    fn write_payload(&mut self, payload: &ClipboardPayload) -> Result<()>;
    fn describe(&self) -> String;
}

/// 剪切板工作模式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardMode {
    Auto,
    Text,
    Image,
    File(PathBuf),
}

impl ClipboardMode {
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        match spec {
            "auto" => Ok(Self::Auto),
            "text" => Ok(Self::Text),
            "image" => Ok(Self::Image),
            other if other.starts_with("file:") => {
                let path = PathBuf::from(&other["file:".len()..]);
                if path.as_os_str().is_empty() {
                    bail!("file: 模式需要文件路径，例如 --clipboard file:C:\\temp\\drive-sync-clip.txt");
                }
                Ok(Self::File(path))
            }
            other => bail!("未知剪切板模式: {other}（可用 auto|text|image|file:<path>）"),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Auto => "auto（文本优先，其次图片）".to_string(),
            Self::Text => "text（仅文本）".to_string(),
            Self::Image => "image（仅 PNG 图片）".to_string(),
            Self::File(path) => format!("file({})", path.display()),
        }
    }

    fn allows_text(&self) -> bool {
        matches!(self, Self::Auto | Self::Text)
    }

    fn allows_image(&self) -> bool {
        matches!(self, Self::Auto | Self::Image)
    }
}

/// 创建后端。
pub fn build_backend(spec: &str) -> Result<Box<dyn ClipboardBackend>> {
    let mode = ClipboardMode::parse(spec)?;
    match &mode {
        ClipboardMode::File(path) => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("创建剪切板文件目录失败: {}", parent.display())
                    })?;
                }
            }
            println!("剪切板后端: {}", mode.describe());
            Ok(Box::new(FileBackend::new(path.clone())))
        }
        ClipboardMode::Auto | ClipboardMode::Text | ClipboardMode::Image => {
            #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
            {
                let backend = ArboardBackend::new(mode.clone())?;
                println!("剪切板后端: {}", mode.describe());
                #[cfg(target_os = "linux")]
                {
                    if std::env::var("WAYLAND_DISPLAY").is_ok()
                        && std::env::var("DISPLAY").is_err()
                    {
                        println!(
                            "提示: 当前为纯 Wayland 会话（无 XWayland），arboard 依赖 X11；\
                             若读写失败请改用 --clipboard file:<path>。"
                        );
                    }
                }
                Ok(Box::new(backend))
            }
            #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
            {
                bail!("当前平台不支持系统剪切板后端，请使用 --clipboard file:<path>")
            }
        }
    }
}

/// 剪切板监听线程：把本机变化送进 `clip_tx`，并把远端应用内容写入后端。
pub fn run_watcher(
    mut backend: Box<dyn ClipboardBackend>,
    poll_ms: u64,
    apply_rx: std::sync::mpsc::Receiver<ClipboardPayload>,
    clip_tx: tokio::sync::mpsc::Sender<ClipboardPayload>,
    shutdown: Arc<AtomicBool>,
) {
    let poll = Duration::from_millis(poll_ms.max(50));
    while !shutdown.load(Ordering::SeqCst) {
        // 1) 先应用远端内容（立即生效，降低延迟）。
        while let Ok(payload) = apply_rx.try_recv() {
            if let Err(error) = backend.write_payload(&payload) {
                tracing::warn!(error = %error, "应用远端剪切板失败");
            }
        }
        // 2) 轮询本机变化。
        if let Some(payload) = backend.read_change() {
            if clip_tx.blocking_send(payload).is_err() {
                break;
            }
        }
        // 3) 等待下一轮：期间收到远端应用则立即写入。
        match apply_rx.recv_timeout(poll) {
            Ok(payload) => {
                if let Err(error) = backend.write_payload(&payload) {
                    tracing::warn!(error = %error, "应用远端剪切板失败");
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    tracing::debug!(backend = %backend.describe(), "剪切板监听线程退出");
}

// ---------------------------------------------------------------------------
// 系统剪切板（arboard）
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
struct ArboardBackend {
    clipboard: arboard::Clipboard,
    mode: ClipboardMode,
    last_text_hash: Option<String>,
    last_image_hash: Option<String>,
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
impl ArboardBackend {
    fn new(mode: ClipboardMode) -> Result<Self> {
        let clipboard = arboard::Clipboard::new().context(
            "初始化系统剪切板失败（Linux 需要 X11/XWayland；无图形会话请用 --clipboard file:<path>）",
        )?;
        let mut backend = Self {
            clipboard,
            mode,
            last_text_hash: None,
            last_image_hash: None,
        };
        // 启动时先记录当前内容，避免把"启动前就存在的旧内容"当成变化广播。
        let _ = backend.read_change();
        Ok(backend)
    }
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
impl ClipboardBackend for ArboardBackend {
    fn read_change(&mut self) -> Option<ClipboardPayload> {
        if self.mode.allows_text() {
            match self.clipboard.get_text() {
                Ok(text) => {
                    let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
                    if self.last_text_hash.as_deref() != Some(hash.as_str()) {
                        self.last_text_hash = Some(hash);
                        match ClipboardPayload::text(&text) {
                            Ok(payload) => return Some(payload),
                            Err(error) => {
                                tracing::debug!(error = %error, "文本超出剪切板同步上限，跳过")
                            }
                        }
                    }
                }
                Err(error) => tracing::trace!(error = %error, "读取文本剪切板失败（可能无文本）"),
            }
        }
        if self.mode.allows_image() {
            match self.clipboard.get_image() {
                Ok(image) => {
                    let key = image_key(image.width, image.height, &image.bytes);
                    if self.last_image_hash.as_deref() != Some(key.as_str()) {
                        self.last_image_hash = Some(key);
                        match encode_png(image.width, image.height, &image.bytes) {
                            Ok(png) => match ClipboardPayload::png(png) {
                                Ok(payload) => return Some(payload),
                                Err(error) => {
                                    tracing::debug!(error = %error, "图片超出剪切板同步上限，跳过")
                                }
                            },
                            Err(error) => tracing::warn!(error = %error, "PNG 编码失败"),
                        }
                    }
                }
                Err(error) => tracing::trace!(error = %error, "读取图片剪切板失败（可能无图片）"),
            }
        }
        None
    }

    fn write_payload(&mut self, payload: &ClipboardPayload) -> Result<()> {
        match payload.kind {
            ClipboardKind::Text | ClipboardKind::RichText => {
                let text = payload.as_text().context("解析文本载荷失败")?;
                self.clipboard
                    .set_text(text)
                    .context("写入系统剪切板失败")?;
                self.last_text_hash = Some(payload.hash.clone());
                Ok(())
            }
            ClipboardKind::Image => {
                let png = payload.decompressed().context("解压图片载荷失败")?;
                let image = decode_png(&png)?;
                let key = image_key(image.width, image.height, &image.bytes);
                self.clipboard
                    .set_image(image)
                    .context("写入系统剪切板失败")?;
                self.last_image_hash = Some(key);
                Ok(())
            }
            ClipboardKind::Files => {
                tracing::warn!("文件列表类型剪切板暂不应用（只同步占位元数据）");
                Ok(())
            }
        }
    }

    fn describe(&self) -> String {
        format!("系统剪切板（{}）", self.mode.describe())
    }
}

// ---------------------------------------------------------------------------
// file 模式
// ---------------------------------------------------------------------------

/// 用文件模拟剪切板（同机多实例联调/无图形环境测试）。
struct FileBackend {
    path: PathBuf,
    last_hash: Option<String>,
}

impl FileBackend {
    fn new(path: PathBuf) -> Self {
        let mut backend = Self {
            path,
            last_hash: None,
        };
        // 启动时先记录文件当前内容，避免把"启动前就存在的旧内容"当成变化广播。
        if let Ok(bytes) = std::fs::read(&backend.path) {
            if !bytes.is_empty() {
                backend.last_hash = Some(blake3::hash(&bytes).to_hex().to_string());
            }
        }
        backend
    }
}

impl ClipboardBackend for FileBackend {
    fn read_change(&mut self) -> Option<ClipboardPayload> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!(path = %self.path.display(), error = %error, "读取剪切板文件失败");
                }
                return None;
            }
        };
        if bytes.is_empty() {
            return None;
        }
        let hash = blake3::hash(&bytes).to_hex().to_string();
        if self.last_hash.as_deref() == Some(hash.as_str()) {
            return None;
        }
        self.last_hash = Some(hash);
        match String::from_utf8(bytes.clone()) {
            Ok(text) => ClipboardPayload::text(&text).ok(),
            // 非 UTF-8 内容按 PNG 图片处理（同机测试可写入 PNG 文件）。
            Err(_) => ClipboardPayload::png(bytes).ok(),
        }
    }

    fn write_payload(&mut self, payload: &ClipboardPayload) -> Result<()> {
        let bytes = payload.decompressed().context("解压剪切板载荷失败")?;
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建目录失败: {}", parent.display()))?;
            }
        }
        std::fs::write(&self.path, &bytes)
            .with_context(|| format!("写入剪切板文件失败: {}", self.path.display()))?;
        self.last_hash = Some(blake3::hash(&bytes).to_hex().to_string());
        Ok(())
    }

    fn describe(&self) -> String {
        format!("file({})", self.path.display())
    }
}

// ---------------------------------------------------------------------------
// PNG 编解码（图片跨端交换格式）
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
fn image_key(width: usize, height: usize, rgba: &[u8]) -> String {
    format!(
        "{width}x{height}:{}",
        blake3::hash(rgba).to_hex().to_string()
    )
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>> {
    use image::{ExtendedColorType, ImageEncoder};
    let expected = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("图片尺寸非法: {width}x{height}"))?;
    if rgba.len() != expected {
        bail!(
            "图片 RGBA 数据长度非法: {}，期望 {}",
            rgba.len(),
            expected
        );
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            rgba,
            width as u32,
            height as u32,
            ExtendedColorType::Rgba8,
        )
        .context("PNG 编码失败")?;
    Ok(out)
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
fn decode_png(png: &[u8]) -> Result<arboard::ImageData<'static>> {
    let image = image::load_from_memory(png).context("PNG 解码失败")?;
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(arboard::ImageData {
        width: width as usize,
        height: height as usize,
        bytes: Cow::Owned(rgba.into_raw()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_clipboard_modes() {
        assert_eq!(ClipboardMode::parse("auto").unwrap(), ClipboardMode::Auto);
        assert_eq!(ClipboardMode::parse(" text ").unwrap(), ClipboardMode::Text);
        assert_eq!(ClipboardMode::parse("image").unwrap(), ClipboardMode::Image);
        match ClipboardMode::parse("file:/tmp/clip.txt").unwrap() {
            ClipboardMode::File(path) => assert_eq!(path, PathBuf::from("/tmp/clip.txt")),
            other => panic!("模式解析错误: {other:?}"),
        }
        assert!(ClipboardMode::parse("").is_err());
        assert!(ClipboardMode::parse("files").is_err());
        assert!(ClipboardMode::parse("file:").is_err());
        assert!(ClipboardMode::parse("unknown").is_err());
    }

    #[test]
    fn file_backend_dedup_and_loopback_suppression() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("clip.txt");
        std::fs::write(&path, "seed").unwrap();

        let mut backend = FileBackend::new(path.clone());
        // 启动时已有内容不应当作"变化"上报。
        assert!(backend.read_change().is_none());

        std::fs::write(&path, "changed content").unwrap();
        let payload = backend.read_change().expect("应检测到外部变化");
        assert_eq!(payload.as_text().unwrap(), "changed content");
        assert!(backend.read_change().is_none(), "同样内容不重复上报");

        // 应用远端内容后，本地监听器不应再上报（回环抑制的最后一环）。
        backend
            .write_payload(&ClipboardPayload::text("remote applied").unwrap())
            .unwrap();
        assert!(backend.read_change().is_none());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "remote applied"
        );
    }

    #[test]
    fn file_backend_image_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("clip.bin");
        let mut backend = FileBackend::new(path.clone());
        // 非 UTF-8 内容按 PNG 图片处理。
        let png_like = vec![0x89u8, b'P', b'N', b'G', 0, 1, 2, 3];
        std::fs::write(&path, &png_like).unwrap();
        let payload = backend.read_change().expect("应检测到图片变化");
        assert_eq!(payload.kind, ClipboardKind::Image);
        backend.write_payload(&payload).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), png_like);
        assert!(backend.read_change().is_none());
    }
}

