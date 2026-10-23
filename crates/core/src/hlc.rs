//! 混合逻辑时钟（HLC）：为软件目录的 LWW 合并提供全序时间戳。

use serde::{Deserialize, Serialize};

use crate::{CoreError, Result};

/// Hybrid Logical Clock。
///
/// 排序规则：`wall_ms` → `counter` → `node`（字典序）。node 通常取设备短 id，
/// 保证不同设备的同一毫秒事件也有确定全序。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hlc {
    pub wall_ms: u64,
    pub counter: u32,
    pub node: String,
}

impl Hlc {
    pub fn zero(node: &str) -> Self {
        Self {
            wall_ms: 0,
            counter: 0,
            node: node.to_string(),
        }
    }

    pub fn now(node: &str) -> Self {
        Self {
            wall_ms: crate::util::now_ms(),
            counter: 0,
            node: node.to_string(),
        }
    }

    /// 本地事件：时钟只前进不后退。
    pub fn tick(&self, now_ms: u64, node: &str) -> Self {
        if now_ms > self.wall_ms {
            Self {
                wall_ms: now_ms,
                counter: 0,
                node: node.to_string(),
            }
        } else {
            Self {
                wall_ms: self.wall_ms,
                counter: self.counter.saturating_add(1),
                node: node.to_string(),
            }
        }
    }

    /// 收到远端事件后的合并时钟（保证 local 因果上大于双方）。
    pub fn receive(&self, remote: &Hlc, now_ms: u64, node: &str) -> Self {
        let wall = now_ms.max(self.wall_ms).max(remote.wall_ms);
        let counter = if wall == self.wall_ms && wall == remote.wall_ms {
            self.counter.max(remote.counter).saturating_add(1)
        } else if wall == self.wall_ms {
            self.counter.saturating_add(1)
        } else if wall == remote.wall_ms {
            remote.counter.saturating_add(1)
        } else {
            0
        };
        Self {
            wall_ms: wall,
            counter,
            node: node.to_string(),
        }
    }

    /// 紧凑编码：`<wall:016x>-<counter:08x>-<node>`，可直接字符串比较排序。
    pub fn encode(&self) -> String {
        format!("{:016x}-{:08x}-{}", self.wall_ms, self.counter, self.node)
    }

    pub fn decode(s: &str) -> Result<Self> {
        let mut parts = s.splitn(3, '-');
        let wall = parts.next().unwrap_or_default();
        let counter = parts.next().unwrap_or_default();
        let node = parts.next().unwrap_or_default();
        if wall.is_empty() || counter.is_empty() || node.is_empty() {
            return Err(CoreError::Codec(format!("非法 HLC 编码: {s}")));
        }
        Ok(Self {
            wall_ms: u64::from_str_radix(wall, 16)
                .map_err(|e| CoreError::Codec(format!("HLC wall 非法: {e}")))?,
            counter: u32::from_str_radix(counter, 16)
                .map_err(|e| CoreError::Codec(format!("HLC counter 非法: {e}")))?,
            node: node.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_and_encoding() {
        let mut a = Hlc::zero("dev-a");
        a = a.tick(1_000, "dev-a");
        let b = a.tick(1_000, "dev-a");
        assert!(b > a);
        let c = Hlc::zero("dev-b").tick(1_000, "dev-b");
        assert!(c > a); // node 字典序更大
        assert_eq!(Hlc::decode(&a.encode()).unwrap(), a);
        // 编码后的字符串排序与 Ord 一致
        let mut list = vec![b.clone(), a.clone(), c.clone()];
        list.sort();
        let mut encoded: Vec<String> = list.iter().map(|h| h.encode()).collect();
        encoded.sort();
        let decoded: Vec<Hlc> = encoded.iter().map(|s| Hlc::decode(s).unwrap()).collect();
        assert_eq!(decoded, list);
    }

    #[test]
    fn receive_advances() {
        let local = Hlc {
            wall_ms: 100,
            counter: 2,
            node: "a".into(),
        };
        let remote = Hlc {
            wall_ms: 100,
            counter: 5,
            node: "b".into(),
        };
        let merged = local.receive(&remote, 90, "a");
        assert!(merged > local);
        assert!(merged > remote);
    }
}
