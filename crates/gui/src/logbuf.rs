//! GUI 日志：tracing 输出同时写入 `logs/gui.log` 与内存环形缓冲，供日志页展示。

use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};

const CAPACITY: usize = 2000;

struct Inner {
    lines: VecDeque<String>,
    file: Option<std::fs::File>,
    path: Option<PathBuf>,
}

static STORE: OnceLock<Arc<Mutex<Inner>>> = OnceLock::new();

fn store() -> &'static Arc<Mutex<Inner>> {
    STORE.get_or_init(|| {
        Arc::new(Mutex::new(Inner {
            lines: VecDeque::new(),
            file: None,
            path: None,
        }))
    })
}

fn push_line(line: impl Into<String>) {
    let line = line.into();
    let store = store();
    let mut inner = store.lock().unwrap_or_else(|poison| poison.into_inner());
    for part in line.lines() {
        if part.trim().is_empty() {
            continue;
        }
        inner.lines.push_back(part.to_string());
        while inner.lines.len() > CAPACITY {
            inner.lines.pop_front();
        }
        if let Some(file) = inner.file.as_mut() {
            let _ = writeln!(file, "{part}");
            let _ = file.flush();
        }
    }
}

/// 直接追加一行应用日志（不经过 tracing，便于 UI 内调用）。
pub fn log(message: impl Into<String>) {
    let stamp = chrono::Local::now().format("%H:%M:%S");
    push_line(format!("[{stamp}] {}", message.into()));
}

/// 初始化 tracing：控制台（debug）+ 内存/文件。
pub fn init(data_dir: &Path) -> Result<PathBuf> {
    let logs_dir = data_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)
        .with_context(|| format!("创建日志目录失败: {}", logs_dir.display()))?;
    let path = logs_dir.join("gui.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("打开日志文件失败: {}", path.display()))?;

    {
        let store = store();
        let mut inner = store.lock().unwrap_or_else(|poison| poison.into_inner());
        inner.file = Some(file);
        inner.path = Some(path.clone());
    }

    let writer_store = store().clone();
    let make_writer = move || MemoryWriter {
        store: writer_store.clone(),
    };
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(make_writer)
        .with_target(false)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
    Ok(path)
}

pub fn log_path() -> Option<PathBuf> {
    let store = store();
    let inner = store.lock().unwrap_or_else(|poison| poison.into_inner());
    inner.path.clone()
}

/// 最近的日志行（从旧到新）。
pub fn snapshot(limit: usize) -> Vec<String> {
    let store = store();
    let inner = store.lock().unwrap_or_else(|poison| poison.into_inner());
    let skip = inner.lines.len().saturating_sub(limit);
    inner.lines.iter().skip(skip).cloned().collect()
}

pub fn full_text() -> String {
    let store = store();
    let inner = store.lock().unwrap_or_else(|poison| poison.into_inner());
    inner.lines.iter().cloned().collect::<Vec<_>>().join("\n")
}

pub fn clear() {
    let store = store();
    let mut inner = store.lock().unwrap_or_else(|poison| poison.into_inner());
    inner.lines.clear();
    if let Some(path) = inner.path.as_ref() {
        if let Ok(file) = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
        {
            drop(file);
        }
    }
}

struct MemoryWriter {
    store: Arc<Mutex<Inner>>,
}

impl Write for MemoryWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf).to_string();
        push_line_into(&self.store, &text);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn push_line_into(store: &Arc<Mutex<Inner>>, text: &str) {
    let mut inner = store.lock().unwrap_or_else(|poison| poison.into_inner());
    for part in text.lines() {
        if part.trim().is_empty() {
            continue;
        }
        inner.lines.push_back(part.to_string());
        while inner.lines.len() > CAPACITY {
            inner.lines.pop_front();
        }
        if let Some(file) = inner.file.as_mut() {
            let _ = writeln!(file, "{part}");
            let _ = file.flush();
        }
    }
}
