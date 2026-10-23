//! CLI 公共部分：数据目录解析、配置解析、日志、时间格式化。

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use drive_client::{default_data_dir, RuntimeConfig, Store};

/// 全局参数解析后的应用上下文。
#[derive(Clone, Debug)]
pub struct App {
    pub data_dir: PathBuf,
    pub relay: Option<String>,
    pub token: Option<String>,
}

impl App {
    pub fn new(data_dir: Option<PathBuf>, relay: Option<String>, token: Option<String>) -> Self {
        let data_dir = data_dir
            .or_else(|| {
                std::env::var_os("DRIVE_SYNC_DATA_DIR")
                    .map(PathBuf::from)
                    .filter(|path| !path.as_os_str().is_empty())
            })
            .unwrap_or_else(default_data_dir);
        Self {
            data_dir,
            relay,
            token,
        }
    }

    pub fn store(&self) -> Result<Store> {
        Store::open(&self.data_dir)
    }

    /// 合并命令行参数与 config.json，得到中继地址与令牌。
    pub fn resolve_relay_token(&self, store: &Store) -> Result<(String, String)> {
        let stored = store.load_config()?;
        let relay = self
            .relay
            .clone()
            .or_else(|| {
                let value = stored.relay_url.trim();
                (!value.is_empty()).then(|| stored.relay_url.clone())
            })
            .ok_or_else(|| {
                anyhow!(
                    "未配置中继地址：请用 --relay 指定（例如 ws://127.0.0.1:8787/ws），或在 config.json 中配置 relay_url"
                )
            })?;
        let token = self
            .token
            .clone()
            .unwrap_or_else(|| stored.access_token.clone());
        Ok((relay, token))
    }

    /// 需要联网的命令使用的运行时配置。
    pub fn runtime_config(&self, store: &Store) -> Result<RuntimeConfig> {
        let (relay, token) = self.resolve_relay_token(store)?;
        Ok(RuntimeConfig::new(self.data_dir.clone(), relay, token))
    }
}

/// 初始化 tracing 日志（输出到 stderr，避免污染 CLI 的 stdout 结果）。
pub fn init_logging(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

/// Unix 毫秒 → 本地时间 `YYYY-MM-DD HH:MM`。
pub fn format_ms(ms: u64) -> String {
    match chrono::DateTime::from_timestamp_millis(ms as i64) {
        Some(time) => time
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
        None => "-".to_string(),
    }
}

/// 当前本地时间 `HH:MM:SS`（daemon 事件行首）。
pub fn format_now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// 截断展示用：超大数字友好格式（例如 12.3 MiB）。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_formats() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(6 * 1024 * 1024), "6.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.00 GiB");
    }

    #[test]
    fn data_dir_env_fallback() {
        // 显式参数优先。
        let app = App::new(Some(PathBuf::from("D:/explicit")), None, None);
        assert_eq!(app.data_dir, PathBuf::from("D:/explicit"));
        // 不设置参数时使用默认目录（平台目录或 DRIVE_SYNC_DATA_DIR），非空。
        std::env::remove_var("DRIVE_SYNC_DATA_DIR");
        let app = App::new(None, None, None);
        assert!(!app.data_dir.as_os_str().is_empty());
    }
}
