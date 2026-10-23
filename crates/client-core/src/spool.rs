//! 离线 spool：对端不在线时暂存待发 PeerMessage，上限 100 条 / 16 MiB / TTL 24h
//! （协议 §7.1、§8.3）。
//!
//! 每个待发消息一个 JSON 文件（`spool/{created:016x}-{seq:016x}-{id}.json`），
//! 原子写；会话建立后按时间序（同源再按 origin_seq）补发并删除。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use drive_core::peer::PeerMessage;
use drive_core::util::now_ms;
use serde::{Deserialize, Serialize};

use crate::store::SpoolSettings;
use crate::util::{random_hex, write_text_atomic};

pub const SPOOL_ITEM_VERSION: u32 = 1;

/// 一条待发消息。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpoolItem {
    #[serde(default = "default_v")]
    pub v: u32,
    pub id: String,
    pub peer_id: String,
    pub created_at_ms: u64,
    /// 相对创建时间的有效期（毫秒）。
    pub ttl_ms: u64,
    /// 源事件序号（剪切板 origin_seq），用于同源顺序补发。
    #[serde(default)]
    pub origin_seq: u64,
    pub message: PeerMessage,
}

fn default_v() -> u32 {
    SPOOL_ITEM_VERSION
}

impl SpoolItem {
    pub fn new(
        peer_id: impl Into<String>,
        message: PeerMessage,
        origin_seq: u64,
        ttl_ms: u64,
    ) -> Self {
        Self {
            v: SPOOL_ITEM_VERSION,
            id: random_hex(8),
            peer_id: peer_id.into(),
            created_at_ms: now_ms(),
            ttl_ms,
            origin_seq,
            message,
        }
    }

    pub fn is_expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.created_at_ms) > self.ttl_ms
    }

    fn file_name(&self) -> String {
        format!(
            "{:016x}-{:016x}-{}.json",
            self.created_at_ms, self.origin_seq, self.id
        )
    }
}

/// spool 目录管理器。
#[derive(Debug)]
pub struct Spool {
    dir: PathBuf,
    max_items: usize,
    max_bytes: u64,
    ttl_ms: u64,
}

impl Spool {
    pub fn new(dir: impl Into<PathBuf>, settings: &SpoolSettings) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("创建 spool 目录失败: {}", dir.display()))?;
        Ok(Self {
            dir,
            max_items: settings.max_items.max(1),
            max_bytes: settings.max_bytes.max(1024),
            ttl_ms: settings.ttl_ms.max(1000),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn ttl_ms(&self) -> u64 {
        self.ttl_ms
    }

    pub fn update_settings(&mut self, settings: &SpoolSettings) {
        self.max_items = settings.max_items.max(1);
        self.max_bytes = settings.max_bytes.max(1024);
        self.ttl_ms = settings.ttl_ms.max(1000);
    }

    /// 追加一条待发消息并执行容量限制（超限时丢弃最旧的）。
    pub fn push(&self, mut item: SpoolItem) -> Result<()> {
        if item.ttl_ms == 0 {
            item.ttl_ms = self.ttl_ms;
        }
        item.v = SPOOL_ITEM_VERSION;
        let path = self.dir.join(item.file_name());
        let bytes = serde_json::to_vec_pretty(&item).context("序列化 spool 条目失败")?;
        write_text_atomic(&path, &bytes)?;
        self.enforce_limits()?;
        Ok(())
    }

    /// 读取全部条目，按 (created_at_ms, origin_seq) 升序。
    pub fn list(&self) -> Result<Vec<SpoolItem>> {
        let mut items = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(items),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "读取 spool 目录失败 {}: {}",
                    self.dir.display(),
                    e
                ))
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read(&path) {
                Ok(bytes) => match serde_json::from_slice::<SpoolItem>(&bytes) {
                    Ok(item) => items.push(item),
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "spool 条目解析失败，忽略"
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "spool 条目读取失败，忽略");
                }
            }
        }
        items.sort_by(|a, b| {
            (a.created_at_ms, a.origin_seq).cmp(&(b.created_at_ms, b.origin_seq))
        });
        Ok(items)
    }

    /// 指定对端的待发条目（保持顺序）。
    pub fn for_peer(&self, peer_id: &str) -> Result<Vec<SpoolItem>> {
        let mut items: Vec<SpoolItem> = self
            .list()?
            .into_iter()
            .filter(|item| item.peer_id == peer_id)
            .collect();
        items.sort_by(|a, b| {
            (a.created_at_ms, a.origin_seq).cmp(&(b.created_at_ms, b.origin_seq))
        });
        Ok(items)
    }

    pub fn count(&self) -> Result<usize> {
        Ok(self.list()?.len())
    }

    /// 删除指定 peer 的全部待发消息，返回删除数量（解除配对时调用）。
    pub fn remove_for_peer(&self, peer_id: &str) -> Result<usize> {
        let mut removed = 0;
        for item in self.list()? {
            if item.peer_id == peer_id {
                self.remove(&item.id)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    pub fn total_bytes(&self) -> Result<u64> {
        let mut total = 0u64;
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    total += meta.len();
                }
            }
        }
        Ok(total)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    if name.contains(id) {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
        Ok(())
    }

    /// 删除过期条目，返回删除数量。
    pub fn prune_expired(&self, now: u64) -> Result<usize> {
        let mut removed = 0;
        for item in self.list()? {
            if item.is_expired(now) {
                self.remove(&item.id)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// 清空全部待发消息，返回删除数量。
    pub fn clear(&self) -> Result<usize> {
        let mut removed = 0;
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("json")
                    && std::fs::remove_file(&path).is_ok()
                {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    fn enforce_limits(&self) -> Result<()> {
        let mut items = self.list()?;
        let mut total: u64 = items
            .iter()
            .map(|item| {
                serde_json::to_vec(item)
                    .map(|b| b.len() as u64)
                    .unwrap_or(0)
            })
            .sum();
        let mut removed = 0;
        while !items.is_empty() && (items.len() > self.max_items || total > self.max_bytes) {
            let oldest = items.remove(0);
            let size = serde_json::to_vec(&oldest)
                .map(|b| b.len() as u64)
                .unwrap_or(0);
            self.remove(&oldest.id)?;
            total = total.saturating_sub(size);
            removed += 1;
        }
        if removed > 0 {
            tracing::debug!(removed, "spool 超出限额，已丢弃最旧条目");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drive_core::clipboard::ClipboardPayload;
    use drive_core::peer::PeerMessage;
    use tempfile::TempDir;

    fn clip_msg(text: &str) -> PeerMessage {
        let event = drive_core::clipboard::ClipboardEvent {
            v: 1,
            event_id: format!("e-{}", text.len()),
            origin_device: "dev-a".into(),
            origin_seq: text.len() as u64,
            created_at_ms: 1,
            payload: ClipboardPayload::text(text).unwrap(),
        };
        PeerMessage::Clipboard { event }
    }

    #[test]
    fn push_list_and_remove() {
        let tmp = TempDir::new().unwrap();
        let spool = Spool::new(tmp.path().join("spool"), &SpoolSettings::default()).unwrap();
        assert_eq!(spool.count().unwrap(), 0);
        spool
            .push(SpoolItem::new("dev-b", clip_msg("hello"), 1, 60_000))
            .unwrap();
        spool
            .push(SpoolItem::new("dev-b", clip_msg("world!!"), 2, 60_000))
            .unwrap();
        spool
            .push(SpoolItem::new("dev-c", clip_msg("other"), 1, 60_000))
            .unwrap();
        let for_b = spool.for_peer("dev-b").unwrap();
        assert_eq!(for_b.len(), 2);
        assert!(for_b[0].origin_seq < for_b[1].origin_seq);
        assert_eq!(spool.count().unwrap(), 3);
        spool.remove(&for_b[0].id).unwrap();
        assert_eq!(spool.for_peer("dev-b").unwrap().len(), 1);
    }

    #[test]
    fn limits_drop_oldest() {
        let tmp = TempDir::new().unwrap();
        let settings = SpoolSettings {
            max_items: 2,
            max_bytes: 1024 * 1024,
            ttl_ms: 60_000,
        };
        let spool = Spool::new(tmp.path().join("spool"), &settings).unwrap();
        for i in 0..4u64 {
            let mut msg = clip_msg("data");
            if let PeerMessage::Clipboard { event } = &mut msg {
                event.origin_seq = i;
            }
            spool
                .push(SpoolItem::new("dev-b", msg, i, 60_000))
                .unwrap();
        }
        assert_eq!(spool.count().unwrap(), 2);
    }

    #[test]
    fn prune_expired() {
        let tmp = TempDir::new().unwrap();
        let spool = Spool::new(tmp.path().join("spool"), &SpoolSettings::default()).unwrap();
        let mut item = SpoolItem::new("dev-b", clip_msg("x"), 1, 1000);
        item.created_at_ms = 100;
        spool.push(item).unwrap();
        assert_eq!(spool.prune_expired(500).unwrap(), 0);
        assert_eq!(spool.prune_expired(5000).unwrap(), 1);
        assert_eq!(spool.count().unwrap(), 0);
    }
}
