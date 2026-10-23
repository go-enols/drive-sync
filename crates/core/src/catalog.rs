//! 软件目录：元数据 CRDT（LWW-Map + 墓碑）。
//!
//! 软件本体永远留在持有设备本地，目录表只同步元数据（名称/版本/平台/内容哈希等）。
//! 设备间通过 HLC 做 last-writer-wins 合并；删除写墓碑，避免"离线设备复活"。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::hlc::Hlc;
use crate::util::{now_ms, random_hex};

/// 目标平台。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TargetPlatform {
    Windows,
    Linux,
    Android,
    Ios,
    Macos,
    /// 跨平台 / 与平台无关（文档、脚本、镜像等）。
    Any,
}

impl TargetPlatform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Linux => "linux",
            Self::Android => "android",
            Self::Ios => "ios",
            Self::Macos => "macos",
            Self::Any => "any",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "windows" | "win" => Some(Self::Windows),
            "linux" => Some(Self::Linux),
            "android" => Some(Self::Android),
            "ios" => Some(Self::Ios),
            "macos" | "mac" => Some(Self::Macos),
            "any" | "all" => Some(Self::Any),
            _ => None,
        }
    }
}

/// 一条软件目录记录（不含本机路径，路径只存于持有设备的本地索引）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub platform: TargetPlatform,
    /// installer / portable / package / iso / document 等自由分类。
    pub kind: String,
    pub size: u64,
    /// 软件本体内容的 BLAKE3 十六进制摘要（用于校验和跨设备去重）。
    pub content_hash: String,
    pub tags: Vec<String>,
    pub notes: String,
    pub updated_at_ms: u64,
    pub updated_by: String,
    pub hlc: Hlc,
    #[serde(default)]
    pub deleted: bool,
}

impl CatalogEntry {
    /// 构造一条新的本地条目（hlc 由 [`Catalog::upsert_local`] 填充）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        platform: TargetPlatform,
        kind: impl Into<String>,
        size: u64,
        content_hash: impl Into<String>,
        tags: Vec<String>,
        notes: impl Into<String>,
        node: &str,
    ) -> Self {
        Self {
            id: format!("ent-{}", random_hex(8)),
            name: name.into(),
            version: version.into(),
            platform,
            kind: kind.into(),
            size,
            content_hash: content_hash.into(),
            tags,
            notes: notes.into(),
            updated_at_ms: now_ms(),
            updated_by: node.to_string(),
            hlc: Hlc::zero(node),
            deleted: false,
        }
    }

    /// 展示名：`name v1.2.3 (windows)`。
    pub fn display_name(&self) -> String {
        format!(
            "{} v{} ({})",
            self.name,
            self.version,
            self.platform.as_str()
        )
    }
}

/// 目录变更操作（CRDT 复制单元）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CatalogOp {
    Upsert {
        entry: CatalogEntry,
    },
    Delete {
        id: String,
        hlc: Hlc,
        deleted_by: String,
        deleted_at_ms: u64,
    },
}

impl CatalogOp {
    pub fn hlc(&self) -> &Hlc {
        match self {
            Self::Upsert { entry } => &entry.hlc,
            Self::Delete { hlc, .. } => hlc,
        }
    }
}

/// 合并统计。
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeReport {
    pub applied: u32,
    pub ignored: u32,
    pub conflicts: u32,
}

impl MergeReport {
    pub fn merge(&mut self, other: MergeReport) {
        self.applied += other.applied;
        self.ignored += other.ignored;
        self.conflicts += other.conflicts;
    }
}

/// 全量目录（含墓碑）。小团队场景下全量同步足够（几百~几千条 < 1 MB）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub entries: BTreeMap<String, CatalogEntry>,
    #[serde(default)]
    pub node: String,
}

impl Catalog {
    pub fn new(node: &str) -> Self {
        Self {
            entries: BTreeMap::new(),
            node: node.to_string(),
        }
    }

    /// 本地新增/更新条目：分配新的 HLC 并生成同步 op。
    pub fn upsert_local(&mut self, mut entry: CatalogEntry, node: &str, now: u64) -> CatalogOp {
        entry.hlc = self.next_hlc(now, node);
        entry.updated_at_ms = now;
        entry.updated_by = node.to_string();
        entry.deleted = false;
        self.entries.insert(entry.id.clone(), entry.clone());
        CatalogOp::Upsert { entry }
    }

    /// 本地删除：写墓碑并生成同步 op。
    pub fn delete_local(&mut self, id: &str, node: &str, now: u64) -> Option<CatalogOp> {
        let mut seen = self.entries.get(id)?.clone();
        if seen.deleted {
            return None;
        }
        let hlc = self.next_hlc(now, node);
        seen.deleted = true;
        seen.hlc = hlc.clone();
        seen.updated_by = node.to_string();
        seen.updated_at_ms = now;
        self.entries.insert(id.to_string(), seen);
        Some(CatalogOp::Delete {
            id: id.to_string(),
            hlc,
            deleted_by: node.to_string(),
            deleted_at_ms: now,
        })
    }

    /// 合并远端操作。幂等：重复应用同一 op 不会改变状态。
    pub fn apply_ops(&mut self, ops: &[CatalogOp]) -> MergeReport {
        let mut report = MergeReport::default();
        for op in ops {
            self.apply_op(op, &mut report);
        }
        report
    }

    fn apply_op(&mut self, op: &CatalogOp, report: &mut MergeReport) {
        match op {
            CatalogOp::Upsert { entry } => match self.entries.get(&entry.id) {
                None => {
                    self.entries.insert(entry.id.clone(), entry.clone());
                    report.applied += 1;
                }
                Some(current) if entry.hlc > current.hlc => {
                    self.entries.insert(entry.id.clone(), entry.clone());
                    report.applied += 1;
                }
                Some(current) => {
                    if entry.hlc == current.hlc && entry != current {
                        // 同一 HLC 但内容不同：视为异常冲突，保留本地并记录。
                        report.conflicts += 1;
                    } else {
                        report.ignored += 1;
                    }
                }
            },
            CatalogOp::Delete {
                id,
                hlc,
                deleted_by,
                deleted_at_ms,
            } => {
                if let Some(current) = self.entries.get(id) {
                    if current.deleted || current.hlc >= *hlc {
                        report.ignored += 1;
                        return;
                    }
                }
                let mut tomb = self
                    .entries
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| CatalogEntry {
                        id: id.clone(),
                        name: String::new(),
                        version: String::new(),
                        platform: TargetPlatform::Any,
                        kind: String::new(),
                        size: 0,
                        content_hash: String::new(),
                        tags: Vec::new(),
                        notes: String::new(),
                        updated_at_ms: *deleted_at_ms,
                        updated_by: deleted_by.clone(),
                        hlc: hlc.clone(),
                        deleted: true,
                    });
                tomb.deleted = true;
                tomb.hlc = hlc.clone();
                tomb.updated_by = deleted_by.clone();
                tomb.updated_at_ms = *deleted_at_ms;
                self.entries.insert(id.clone(), tomb);
                report.applied += 1;
            }
        }
    }

    /// 全量快照（含墓碑），用于新配对设备首次目录同步。
    pub fn to_sync_ops(&self) -> Vec<CatalogOp> {
        self.entries
            .values()
            .map(|entry| CatalogOp::Upsert {
                entry: entry.clone(),
            })
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<&CatalogEntry> {
        self.entries.get(id).filter(|e| !e.deleted)
    }

    pub fn get_any(&self, id: &str) -> Option<&CatalogEntry> {
        self.entries.get(id)
    }

    pub fn visible(&self) -> impl Iterator<Item = &CatalogEntry> {
        self.entries.values().filter(|e| !e.deleted)
    }

    pub fn visible_count(&self) -> usize {
        self.visible().count()
    }

    pub fn is_empty(&self) -> bool {
        self.visible_count() == 0
    }

    /// 清理过期墓碑（例如 30 天后），返回清理数量。
    pub fn gc_tombstones(&mut self, older_than_ms: u64) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|_, e| !(e.deleted && e.updated_at_ms < older_than_ms));
        before - self.entries.len()
    }

    fn next_hlc(&self, now: u64, node: &str) -> Hlc {
        self.entries
            .values()
            .map(|e| &e.hlc)
            .max()
            .map(|h| h.tick(now, node))
            .unwrap_or_else(|| Hlc::now(node))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, node: &str) -> CatalogEntry {
        CatalogEntry::new(
            name,
            "1.0.0",
            TargetPlatform::Windows,
            "installer",
            1024,
            "hash",
            vec![],
            "",
            node,
        )
    }

    #[test]
    fn local_upsert_and_delete() {
        let mut cat = Catalog::new("dev-a");
        let op = cat.upsert_local(entry("git", "dev-a"), "dev-a", 1000);
        let id = match &op {
            CatalogOp::Upsert { entry } => entry.id.clone(),
            _ => unreachable!(),
        };
        assert_eq!(cat.visible_count(), 1);
        let del = cat.delete_local(&id, "dev-a", 2000).unwrap();
        assert_eq!(cat.visible_count(), 0);
        assert!(cat.get_any(&id).unwrap().deleted);
        assert!(cat.delete_local(&id, "dev-a", 3000).is_none());
        // 重复应用删除 op 幂等
        let mut cat2 = cat.clone();
        let report = cat2.apply_ops(std::slice::from_ref(&del));
        assert_eq!(report.applied, 0);
        assert_eq!(cat2.visible_count(), 0);
    }

    #[test]
    fn merge_uses_hlc_last_write_wins() {
        let mut a = Catalog::new("dev-a");
        let mut b = Catalog::new("dev-b");
        let mut e = entry("vim", "dev-a");
        e.id = "ent-1".into();
        let op_a = a.upsert_local(e.clone(), "dev-a", 1000);
        let mut newer = e.clone();
        newer.notes = "newer note".into();
        let op_b = b.upsert_local(newer, "dev-b", 2000);

        a.apply_ops(std::slice::from_ref(&op_b));
        b.apply_ops(std::slice::from_ref(&op_a));
        assert_eq!(
            a.get("ent-1").unwrap().notes,
            "newer note",
            "较新的写入应获胜"
        );
        assert_eq!(b.get("ent-1").unwrap().notes, "newer note");
    }

    #[test]
    fn delete_wins_over_older_upsert_and_survives_offline_rejoin() {
        let mut a = Catalog::new("dev-a");
        let mut e = entry("tool", "dev-a");
        e.id = "ent-x".into();
        let up = a.upsert_local(e.clone(), "dev-a", 1000);
        let del = a.delete_local("ent-x", "dev-a", 2000).unwrap();
        assert_eq!(a.visible_count(), 0);

        // 离线设备 b 还持有旧 upsert，重连后同步不应复活条目
        let mut b = Catalog::new("dev-b");
        b.apply_ops(std::slice::from_ref(&up));
        assert_eq!(b.visible_count(), 1);
        b.apply_ops(std::slice::from_ref(&del));
        assert_eq!(b.visible_count(), 0);
        // 从 a 全量同步到 c（含墓碑）也不应复活
        let mut c = Catalog::new("dev-c");
        c.apply_ops(&a.to_sync_ops());
        assert_eq!(c.visible_count(), 0);
    }

    #[test]
    fn tombstones_gc() {
        let mut cat = Catalog::new("dev-a");
        let op = cat.upsert_local(entry("x", "dev-a"), "dev-a", 1000);
        let id = match op {
            CatalogOp::Upsert { entry } => entry.id,
            _ => unreachable!(),
        };
        cat.delete_local(&id, "dev-a", 1000);
        assert_eq!(cat.gc_tombstones(500), 0);
        assert_eq!(cat.gc_tombstones(5000), 1);
    }
}
