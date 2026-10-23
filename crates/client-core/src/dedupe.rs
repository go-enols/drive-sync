//! 去重与回环抑制用的带 TTL 的 LRU 结构。
//!
//! - 剪切板接收端用 `event_id` 去重（协议 §7.1：256 条 / 5 分钟）；
//! - 应用远端内容前记录 `(payload.hash, origin_device, event_id)` 到 `recent_applied`，
//!   本地监听器读到同样内容时据此忽略，避免"应用→监听→再发送"的回环；
//! - 发送端维护 `last_local_hash`（本模块不负责，Engine 持有）。

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

/// 带 TTL 与容量上限的 LRU 集合（FIFO 淘汰 + 惰性过期）。
#[derive(Debug)]
pub struct TimedLru<T: Eq + Hash + Clone> {
    cap: usize,
    ttl_ms: u64,
    order: VecDeque<(T, u64)>,
    seen: HashMap<T, u64>,
}

impl<T: Eq + Hash + Clone> TimedLru<T> {
    pub fn new(cap: usize, ttl_ms: u64) -> Self {
        Self {
            cap: cap.max(1),
            ttl_ms,
            order: VecDeque::new(),
            seen: HashMap::new(),
        }
    }

    /// 插入并返回是否为新元素（已存在且未过期返回 false）。
    pub fn insert(&mut self, key: T, now_ms: u64) -> bool {
        self.prune(now_ms);
        if let Some(ts) = self.seen.get(&key) {
            if now_ms.saturating_sub(*ts) <= self.ttl_ms {
                return false;
            }
        }
        self.seen.insert(key.clone(), now_ms);
        self.order.push_back((key, now_ms));
        while self.order.len() > self.cap {
            if let Some((old, _)) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }

    /// 是否存在且未过期。
    pub fn contains_fresh(&mut self, key: &T, now_ms: u64) -> bool {
        self.prune(now_ms);
        match self.seen.get(key) {
            Some(ts) => now_ms.saturating_sub(*ts) <= self.ttl_ms,
            None => false,
        }
    }

    /// 清理过期项。
    pub fn prune(&mut self, now_ms: u64) {
        loop {
            let Some((key, ts)) = self.order.front().cloned() else {
                break;
            };
            if now_ms.saturating_sub(ts) <= self.ttl_ms {
                break;
            }
            self.order.pop_front();
            // 只有当 map 中记录的时间戳与队首一致时才删除（同 key 可能被重复插入过）。
            if self.seen.get(&key) == Some(&ts) {
                self.seen.remove(&key);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// 已应用的远端剪切板记录（回环抑制核心）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedRecord {
    pub hash: String,
    pub origin_device: String,
    pub event_id: String,
    pub applied_at_ms: u64,
}

/// `recent_applied`：hash → 应用记录，LRU 256 / 5 分钟。
#[derive(Debug)]
pub struct RecentApplied {
    cap: usize,
    ttl_ms: u64,
    order: VecDeque<(String, u64)>,
    map: HashMap<String, AppliedRecord>,
}

impl Default for RecentApplied {
    fn default() -> Self {
        Self::new(256, 5 * 60 * 1000)
    }
}

impl RecentApplied {
    pub fn new(cap: usize, ttl_ms: u64) -> Self {
        Self {
            cap: cap.max(1),
            ttl_ms,
            order: VecDeque::new(),
            map: HashMap::new(),
        }
    }

    pub fn insert(&mut self, record: AppliedRecord) {
        let now = record.applied_at_ms;
        if self.map.remove(&record.hash).is_some() {
            self.order.retain(|(h, _)| h != &record.hash);
        }
        self.order.push_back((record.hash.clone(), now));
        self.map.insert(record.hash.clone(), record);
        while self.order.len() > self.cap {
            if let Some((hash, _)) = self.order.pop_front() {
                self.map.remove(&hash);
            }
        }
    }

    /// 该内容是否在最近（TTL 内）被远端应用过 → 本地监听器应忽略，防回环。
    pub fn contains_fresh(&mut self, hash: &str, now_ms: u64) -> bool {
        self.prune(now_ms);
        self.map
            .get(hash)
            .map(|r| now_ms.saturating_sub(r.applied_at_ms) <= self.ttl_ms)
            .unwrap_or(false)
    }

    pub fn get(&self, hash: &str) -> Option<&AppliedRecord> {
        self.map.get(hash)
    }

    pub fn prune(&mut self, now_ms: u64) {
        while let Some((hash, ts)) = self.order.front().cloned() {
            if now_ms.saturating_sub(ts) <= self.ttl_ms {
                break;
            }
            self.order.pop_front();
            if self.map.get(&hash).map(|r| r.applied_at_ms) == Some(ts) {
                self.map.remove(&hash);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timed_lru_dedups_and_expires() {
        let mut lru = TimedLru::<String>::new(3, 1000);
        assert!(lru.insert("a".into(), 10));
        assert!(!lru.insert("a".into(), 20));
        assert!(lru.contains_fresh(&"a".into(), 900));
        assert!(!lru.contains_fresh(&"a".into(), 1200));
        // 过期后可以重新插入
        assert!(lru.insert("a".into(), 1300));
    }

    #[test]
    fn timed_lru_evicts_oldest() {
        let mut lru = TimedLru::<String>::new(2, 60_000);
        lru.insert("a".into(), 1);
        lru.insert("b".into(), 2);
        lru.insert("c".into(), 3);
        assert!(!lru.contains_fresh(&"a".into(), 4));
        assert!(lru.contains_fresh(&"b".into(), 4));
        assert!(lru.contains_fresh(&"c".into(), 4));
    }

    #[test]
    fn recent_applied_ttl_and_cap() {
        let mut recent = RecentApplied::new(2, 1000);
        recent.insert(AppliedRecord {
            hash: "h1".into(),
            origin_device: "b".into(),
            event_id: "e1".into(),
            applied_at_ms: 100,
        });
        assert!(recent.contains_fresh("h1", 500));
        assert!(!recent.contains_fresh("h1", 2000));
        recent.insert(AppliedRecord {
            hash: "h2".into(),
            origin_device: "b".into(),
            event_id: "e2".into(),
            applied_at_ms: 3000,
        });
        recent.insert(AppliedRecord {
            hash: "h3".into(),
            origin_device: "b".into(),
            event_id: "e3".into(),
            applied_at_ms: 3001,
        });
        recent.insert(AppliedRecord {
            hash: "h4".into(),
            origin_device: "b".into(),
            event_id: "e4".into(),
            applied_at_ms: 3002,
        });
        assert_eq!(recent.len(), 2);
        assert!(!recent.contains_fresh("h2", 3003));
        assert!(recent.contains_fresh("h3", 3003));
        assert!(recent.contains_fresh("h4", 3003));
    }
}
