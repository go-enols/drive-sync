//! 可选加密暂存（协议 §7.4 / §4.5）。
//!
//! 服务端只保存客户端已加密的密文：
//! - 落盘布局：`<vault_dir>/index.json` + `<vault_dir>/<object_id>.bin`；
//! - 索引与对象文件均通过 "写临时文件 + rename" 原子替换；
//! - TTL 由服务端强制 `clamp(ttl_secs, 60s, 30d)`，到期对象不可恢复；
//! - 限额：单对象 ≤ 8 MiB、每设备总量 ≤ 1 GiB、每设备对象数 ≤ 1000（均可配置）；
//! - 后台每 60s 清理过期对象，启动时也做一次全量清理（含孤儿文件）。
//!
//! 中继不持有任何内容密钥、不解析密文；日志中不出现 `meta.name` / 文件名 / 密文。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use derive_core::util::{now_ms, random_hex};
use derive_core::vault::{effective_ttl_secs, VaultMeta, VaultObjectInfo};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, warn};

const INDEX_VERSION: u32 = 1;
const INDEX_FILE: &str = "index.json";
const MAX_OBJECT_ID_LEN: usize = 128;

/// 暂存操作错误：`code` 对应协议错误码，会以 `"<code>: <message>"` 放入结果 message。
#[derive(Debug, Clone)]
pub(crate) struct VaultError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl VaultError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn display_message(&self) -> String {
        format!("{}: {}", self.code, self.message)
    }
}

/// 暂存限额（来自 `RelayConfig`）。
#[derive(Clone, Debug)]
pub(crate) struct VaultLimits {
    pub(crate) max_object_bytes: u64,
    pub(crate) quota_bytes: u64,
    pub(crate) max_objects: usize,
}

/// 索引中的一条对象记录（服务端可见的元数据）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct VaultObjectRecord {
    pub(crate) owner: String,
    pub(crate) meta: VaultMeta,
    pub(crate) ciphertext_len: u64,
    pub(crate) created_at_ms: u64,
    pub(crate) expires_at_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct VaultIndexFile {
    v: u32,
    #[serde(default)]
    objects: HashMap<String, VaultObjectRecord>,
}

#[derive(Clone, Copy, Default)]
struct OwnerUsage {
    bytes: u64,
    count: usize,
}

struct VaultInner {
    objects: HashMap<String, VaultObjectRecord>,
    owners: HashMap<String, OwnerUsage>,
}

impl VaultInner {
    /// 移除对象记录、更新用量、删除落盘文件。
    async fn remove_object(&mut self, dir: &Path, object_id: &str) -> bool {
        let Some(record) = self.objects.remove(object_id) else {
            return false;
        };
        let mut remove_owner = false;
        if let Some(usage) = self.owners.get_mut(&record.owner) {
            usage.bytes = usage.bytes.saturating_sub(record.ciphertext_len);
            usage.count = usage.count.saturating_sub(1);
            remove_owner = usage.count == 0 && usage.bytes == 0;
        }
        if remove_owner {
            self.owners.remove(&record.owner);
        }
        if let Err(error) = tokio::fs::remove_file(object_path(dir, object_id)).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                warn!(%error, "删除暂存对象文件失败");
            }
        }
        true
    }

    fn rebuild_owners(&mut self) {
        self.owners.clear();
        for record in self.objects.values() {
            let usage = self.owners.entry(record.owner.clone()).or_default();
            usage.bytes = usage.bytes.saturating_add(record.ciphertext_len);
            usage.count += 1;
        }
    }
}

/// `vault_get` 的返回内容。
#[derive(Debug)]
pub(crate) struct VaultObjectData {
    pub(crate) meta: VaultMeta,
    pub(crate) ciphertext: Vec<u8>,
}

pub(crate) struct VaultStore {
    dir: PathBuf,
    limits: VaultLimits,
    inner: Mutex<VaultInner>,
}

impl VaultStore {
    /// 打开/初始化暂存目录：加载索引并做一次启动清理（过期对象、孤儿文件、残留临时文件）。
    pub(crate) async fn open(dir: PathBuf, limits: VaultLimits) -> anyhow::Result<Self> {
        use anyhow::Context as _;

        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("无法创建暂存目录 {}", dir.display()))?;

        let index_path = dir.join(INDEX_FILE);
        let index_file = match tokio::fs::read(&index_path).await {
            Ok(bytes) => {
                let file: VaultIndexFile = serde_json::from_slice(&bytes)
                    .with_context(|| "暂存索引 index.json 解析失败（请检查是否损坏）")?;
                if file.v != INDEX_VERSION {
                    anyhow::bail!("不支持的暂存索引版本: {}", file.v);
                }
                file
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => VaultIndexFile {
                v: INDEX_VERSION,
                objects: HashMap::new(),
            },
            Err(error) => {
                return Err(error).with_context(|| "读取暂存索引失败");
            }
        };

        let mut inner = VaultInner {
            objects: index_file.objects,
            owners: HashMap::new(),
        };
        let mut dirty = false;
        let now = now_ms();

        // 过期对象 / 索引存在但文件缺失。
        let known: Vec<String> = inner.objects.keys().cloned().collect();
        for object_id in known {
            let expired = inner
                .objects
                .get(&object_id)
                .map(|record| record.expires_at_ms <= now)
                .unwrap_or(false);
            if expired {
                inner.remove_object(&dir, &object_id).await;
                dirty = true;
                continue;
            }
            let exists = tokio::fs::try_exists(object_path(&dir, &object_id))
                .await
                .unwrap_or(false);
            if !exists {
                warn!("暂存索引中的对象文件缺失，已清理索引项");
                inner.remove_object(&dir, &object_id).await;
                dirty = true;
            }
        }

        // 孤儿对象文件 / 残留临时文件。
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .with_context(|| "读取暂存目录失败")?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == INDEX_FILE {
                continue;
            }
            if name.starts_with('.') && name.contains(".tmp-") {
                let _ = tokio::fs::remove_file(entry.path()).await;
                dirty = true;
                continue;
            }
            if let Some(object_id) = name.strip_suffix(".bin") {
                if !inner.objects.contains_key(object_id) {
                    let _ = tokio::fs::remove_file(entry.path()).await;
                    dirty = true;
                }
            }
        }

        inner.rebuild_owners();
        let store = Self {
            dir,
            limits,
            inner: Mutex::new(inner),
        };
        if dirty {
            let inner = store.inner.lock().await;
            persist_index(&store.dir, &inner)
                .await
                .with_context(|| "写入暂存索引失败")?;
        }
        Ok(store)
    }

    /// 上传（覆盖同 owner 的同 object_id 对象）。
    pub(crate) async fn put(
        &self,
        owner: &str,
        object_id: &str,
        meta: VaultMeta,
        ciphertext: Vec<u8>,
        ttl_secs: u64,
    ) -> Result<VaultObjectRecord, VaultError> {
        validate_object_id(object_id)?;
        let ciphertext_len = ciphertext.len() as u64;
        if self.limits.max_object_bytes > 0 && ciphertext_len > self.limits.max_object_bytes {
            return Err(VaultError::new(
                "payload_too_large",
                format!(
                    "单对象密文 {} 字节，超过上限 {} 字节",
                    ciphertext_len, self.limits.max_object_bytes
                ),
            ));
        }
        let ttl = effective_ttl_secs(ttl_secs);

        let mut inner = self.inner.lock().await;
        let now = now_ms();

        // 同 id 的过期对象先清理。
        let expired = inner
            .objects
            .get(object_id)
            .map(|record| record.expires_at_ms <= now)
            .unwrap_or(false);
        if expired {
            inner.remove_object(&self.dir, object_id).await;
        }

        if let Some(existing) = inner.objects.get(object_id) {
            if existing.owner != owner {
                return Err(VaultError::new(
                    "protocol_error",
                    "object_id 已被其他设备占用",
                ));
            }
        }
        let replacing = inner.objects.contains_key(object_id);
        let existing_bytes = if replacing {
            inner
                .objects
                .get(object_id)
                .map(|record| record.ciphertext_len)
                .unwrap_or(0)
        } else {
            0
        };

        let current = inner.owners.get(owner).copied().unwrap_or_default();
        let new_bytes = current
            .bytes
            .saturating_sub(existing_bytes)
            .saturating_add(ciphertext_len);
        let new_count = current.count + usize::from(!replacing);
        if self.limits.quota_bytes > 0 && new_bytes > self.limits.quota_bytes {
            return Err(VaultError::new(
                "vault_quota_exceeded",
                format!(
                    "单设备暂存配额不足（已用 {} 字节，上限 {} 字节）",
                    current.bytes, self.limits.quota_bytes
                ),
            ));
        }
        if self.limits.max_objects > 0 && new_count > self.limits.max_objects {
            return Err(VaultError::new(
                "vault_quota_exceeded",
                format!(
                    "单设备暂存对象数超过上限（{} 个）",
                    self.limits.max_objects
                ),
            ));
        }

        let record = VaultObjectRecord {
            owner: owner.to_string(),
            meta,
            ciphertext_len,
            created_at_ms: now,
            expires_at_ms: now.saturating_add(ttl.saturating_mul(1000)),
        };

        // 先写对象文件（原子替换），再更新内存索引与落盘索引。
        write_atomic(&object_path(&self.dir, object_id), &ciphertext)
            .await
            .map_err(|error| {
                warn!(%error, "写入暂存对象文件失败");
                VaultError::new("internal_error", "服务端写入暂存对象失败")
            })?;

        inner.objects.insert(object_id.to_string(), record.clone());
        inner.owners.insert(
            owner.to_string(),
            OwnerUsage {
                bytes: new_bytes,
                count: new_count,
            },
        );

        if let Err(error) = persist_index(&self.dir, &inner).await {
            warn!(%error, "写入暂存索引失败");
            // 尽力回滚内存状态；磁盘上的对象文件也尝试删除（启动清理会兜底孤儿文件）。
            inner.objects.remove(object_id);
            if current.count == 0 {
                inner.owners.remove(owner);
            } else {
                inner.owners.insert(owner.to_string(), current);
            }
            let _ = tokio::fs::remove_file(object_path(&self.dir, object_id)).await;
            return Err(VaultError::new("internal_error", "服务端写入暂存索引失败"));
        }

        Ok(record)
    }

    /// 下载密文；过期对象立即删除并返回 `vault_expired`。
    pub(crate) async fn get(&self, object_id: &str) -> Result<VaultObjectData, VaultError> {
        validate_object_id(object_id)?;
        let mut inner = self.inner.lock().await;
        let now = now_ms();

        let Some(record) = inner.objects.get(object_id) else {
            return Err(VaultError::new("vault_not_found", "对象不存在"));
        };
        let expired = record.expires_at_ms <= now;
        if expired {
            inner.remove_object(&self.dir, object_id).await;
            let _ = persist_index(&self.dir, &inner).await;
            return Err(VaultError::new("vault_expired", "对象已过期并被删除"));
        }
        let meta = record.meta.clone();

        match tokio::fs::read(object_path(&self.dir, object_id)).await {
            Ok(ciphertext) => Ok(VaultObjectData { meta, ciphertext }),
            Err(error) => {
                warn!(%error, "读取暂存对象文件失败，已清理索引项");
                inner.remove_object(&self.dir, object_id).await;
                let _ = persist_index(&self.dir, &inner).await;
                Err(VaultError::new("vault_not_found", "对象数据缺失"))
            }
        }
    }

    /// 列出本设备上传的对象（过期对象一并清理）。
    pub(crate) async fn list(&self, owner: &str) -> Vec<VaultObjectInfo> {
        let mut inner = self.inner.lock().await;
        let now = now_ms();
        let expired: Vec<String> = inner
            .objects
            .iter()
            .filter(|(_, record)| record.expires_at_ms <= now)
            .map(|(object_id, _)| object_id.clone())
            .collect();
        let dirty = !expired.is_empty();
        for object_id in expired {
            inner.remove_object(&self.dir, &object_id).await;
        }

        let mut infos: Vec<VaultObjectInfo> = inner
            .objects
            .iter()
            .filter(|(_, record)| record.owner == owner)
            .map(|(object_id, record)| VaultObjectInfo {
                object_id: object_id.clone(),
                meta: record.meta.clone(),
                ciphertext_len: record.ciphertext_len,
                created_at_ms: record.created_at_ms,
                expires_at_ms: record.expires_at_ms,
            })
            .collect();
        infos.sort_by(|left, right| left.object_id.cmp(&right.object_id));

        if dirty {
            if let Err(error) = persist_index(&self.dir, &inner).await {
                warn!(%error, "写入暂存索引失败");
            }
        }
        infos
    }

    /// 立即删除（仅允许上传者删除自己的对象）。
    pub(crate) async fn delete(&self, owner: &str, object_id: &str) -> Result<(), VaultError> {
        validate_object_id(object_id)?;
        let mut inner = self.inner.lock().await;
        let Some(record) = inner.objects.get(object_id) else {
            return Err(VaultError::new("vault_not_found", "对象不存在"));
        };
        if record.owner != owner {
            return Err(VaultError::new(
                "protocol_error",
                "只能删除本设备上传的暂存对象",
            ));
        }
        let expired = record.expires_at_ms <= now_ms();
        inner.remove_object(&self.dir, object_id).await;
        if let Err(error) = persist_index(&self.dir, &inner).await {
            warn!(%error, "写入暂存索引失败");
            return Err(VaultError::new("internal_error", "服务端写入暂存索引失败"));
        }
        if expired {
            Err(VaultError::new("vault_expired", "对象已过期并被删除"))
        } else {
            Ok(())
        }
    }

    /// 清理过期对象，返回删除数量（后台任务每 60s 调用一次）。
    pub(crate) async fn reap(&self) -> anyhow::Result<usize> {
        let mut inner = self.inner.lock().await;
        let now = now_ms();
        let expired: Vec<String> = inner
            .objects
            .iter()
            .filter(|(_, record)| record.expires_at_ms <= now)
            .map(|(object_id, _)| object_id.clone())
            .collect();
        let count = expired.len();
        for object_id in expired {
            inner.remove_object(&self.dir, &object_id).await;
        }
        if count > 0 {
            persist_index(&self.dir, &inner).await?;
            debug!(count, "清理过期暂存对象");
        }
        Ok(count)
    }
}

fn validate_object_id(object_id: &str) -> Result<(), VaultError> {
    if object_id.is_empty() || object_id.len() > MAX_OBJECT_ID_LEN {
        return Err(VaultError::new("protocol_error", "object_id 长度非法"));
    }
    if !object_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(VaultError::new(
            "protocol_error",
            "object_id 只允许字母、数字、'-'、'_'",
        ));
    }
    Ok(())
}

fn object_path(dir: &Path, object_id: &str) -> PathBuf {
    dir.join(format!("{object_id}.bin"))
}

/// `index.json` 原子写入。
async fn persist_index(dir: &Path, inner: &VaultInner) -> std::io::Result<()> {
    let file = VaultIndexFile {
        v: INDEX_VERSION,
        objects: inner.objects.clone(),
    };
    let json = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
    write_atomic(&dir.join(INDEX_FILE), &json).await
}

/// 原子写：同目录临时文件 + rename；Windows 上 rename 不能覆盖已存在文件，
/// 退化为「删除目标后重命名」（窗口极小，原型可接受）。
async fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("目标路径没有父目录"))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("object");
    let tmp_path = dir.join(format!(".{file_name}.tmp-{}", random_hex(4)));
    tokio::fs::write(&tmp_path, data).await?;
    match tokio::fs::rename(&tmp_path, path).await {
        Ok(()) => Ok(()),
        Err(first_error) => {
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    return Err(first_error);
                }
            }
            match tokio::fs::rename(&tmp_path, path).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    Err(error)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use derive_core::vault::{MAX_VAULT_TTL_SECS, MIN_VAULT_TTL_SECS};

    fn limits() -> VaultLimits {
        VaultLimits {
            max_object_bytes: 1024,
            quota_bytes: 4096,
            max_objects: 8,
        }
    }

    fn meta(name: &str) -> VaultMeta {
        VaultMeta {
            name: name.to_string(),
            kind: "clipboard".to_string(),
            size: 5,
            content_hash: "hash".to_string(),
            entry_id: None,
            note: String::new(),
        }
    }

    async fn temp_store() -> (tempfile::TempDir, VaultStore) {
        temp_store_with(limits()).await
    }

    async fn temp_store_with(limits: VaultLimits) -> (tempfile::TempDir, VaultStore) {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = VaultStore::open(dir.path().to_path_buf(), limits)
            .await
            .expect("打开暂存");
        (dir, store)
    }

    #[tokio::test]
    async fn put_get_delete_roundtrip() {
        let (dir, store) = temp_store().await;
        let record = store
            .put("dev-a", "obj-1", meta("a.bin"), b"cipher".to_vec(), 3600)
            .await
            .expect("上传成功");
        assert_eq!(record.ciphertext_len, 6);
        assert!(dir.path().join("obj-1.bin").exists());
        assert!(dir.path().join("index.json").exists());

        let data = store.get("obj-1").await.expect("下载成功");
        assert_eq!(data.ciphertext, b"cipher");
        assert_eq!(data.meta.name, "a.bin");

        store.delete("dev-a", "obj-1").await.expect("删除成功");
        assert!(store.get("obj-1").await.is_err());
        assert!(!dir.path().join("obj-1.bin").exists());
    }

    #[tokio::test]
    async fn ttl_is_clamped_to_protocol_range() {
        let (_dir, store) = temp_store().await;
        let before = now_ms();
        let record = store
            .put("dev-a", "obj-ttl", meta("x"), b"data".to_vec(), 1)
            .await
            .expect("上传成功");
        let min_expiry = before + MIN_VAULT_TTL_SECS * 1000;
        assert!(
            record.expires_at_ms >= min_expiry,
            "TTL 下限必须强制为 60s"
        );
        let record = store
            .put("dev-a", "obj-ttl2", meta("y"), b"data".to_vec(), u64::MAX)
            .await
            .expect("上传成功");
        let max_expiry = now_ms() + MAX_VAULT_TTL_SECS * 1000;
        assert!(
            record.expires_at_ms <= max_expiry + 5_000,
            "TTL 上限必须强制为 30 天"
        );
    }

    #[tokio::test]
    async fn expired_object_is_deleted_on_get() {
        let (dir, store) = temp_store().await;
        store
            .put("dev-a", "obj-exp", meta("x"), b"data".to_vec(), 60)
            .await
            .expect("上传成功");
        {
            let mut inner = store.inner.lock().await;
            let record = inner.objects.get_mut("obj-exp").expect("记录存在");
            record.expires_at_ms = now_ms().saturating_sub(1);
        }
        let error = store.get("obj-exp").await.expect_err("过期对象必须不可读");
        assert_eq!(error.code, "vault_expired");
        {
            let inner = store.inner.lock().await;
            assert!(!inner.objects.contains_key("obj-exp"));
        }
        assert!(!dir.path().join("obj-exp.bin").exists());
    }

    #[tokio::test]
    async fn reap_removes_expired_objects() {
        let (dir, store) = temp_store().await;
        store
            .put("dev-a", "obj-1", meta("x"), b"data".to_vec(), 60)
            .await
            .expect("上传成功");
        store
            .put("dev-a", "obj-2", meta("y"), b"data".to_vec(), 60)
            .await
            .expect("上传成功");
        {
            let mut inner = store.inner.lock().await;
            let record = inner.objects.get_mut("obj-1").expect("记录存在");
            record.expires_at_ms = now_ms().saturating_sub(1);
        }
        assert_eq!(store.reap().await.expect("清理成功"), 1);
        assert!(store.get("obj-1").await.is_err());
        assert!(store.get("obj-2").await.is_ok());
        assert!(!dir.path().join("obj-1.bin").exists());
    }

    #[tokio::test]
    async fn object_too_large_rejected() {
        let (_dir, store) = temp_store().await;
        let error = store
            .put(
                "dev-a",
                "obj-big",
                meta("x"),
                vec![0u8; 1025],
                60,
            )
            .await
            .expect_err("超过单对象上限必须拒绝");
        assert_eq!(error.code, "payload_too_large");
    }

    #[tokio::test]
    async fn per_device_quota_enforced() {
        let (_dir, store) = temp_store().await;
        for index in 0..4 {
            store
                .put(
                    "dev-a",
                    &format!("obj-{index}"),
                    meta("x"),
                    vec![0u8; 1024],
                    60,
                )
                .await
                .expect("配额内上传成功");
        }
        let error = store
            .put("dev-a", "obj-over", meta("x"), vec![0u8; 16], 60)
            .await
            .expect_err("超过配额必须拒绝");
        assert_eq!(error.code, "vault_quota_exceeded");

        // 其他设备配额独立。
        assert!(store
            .put("dev-b", "obj-b", meta("x"), vec![0u8; 16], 60)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn object_count_limit_enforced() {
        let (_dir, store) = temp_store_with(VaultLimits {
            max_object_bytes: 1024,
            quota_bytes: 1024 * 1024,
            max_objects: 8,
        })
        .await;
        for index in 0..8 {
            store
                .put(
                    "dev-a",
                    &format!("obj-{index}"),
                    meta("x"),
                    vec![0u8; 16],
                    60,
                )
                .await
                .expect("对象数上限内上传成功");
        }
        let error = store
            .put("dev-a", "obj-many", meta("x"), vec![0u8; 16], 60)
            .await
            .expect_err("超过对象数上限必须拒绝");
        assert_eq!(error.code, "vault_quota_exceeded");
    }

    #[tokio::test]
    async fn invalid_object_id_rejected() {
        let (_dir, store) = temp_store().await;
        for object_id in ["", "../evil", "a/b", "a b", "中文"] {
            let error = store
                .put("dev-a", object_id, meta("x"), b"data".to_vec(), 60)
                .await
                .expect_err("非法 object_id 必须拒绝");
            assert_eq!(error.code, "protocol_error");
        }
    }

    #[tokio::test]
    async fn replace_does_not_double_count_quota() {
        let (_dir, store) = temp_store().await;
        store
            .put("dev-a", "obj-1", meta("x"), vec![0u8; 1000], 60)
            .await
            .expect("上传成功");
        store
            .put("dev-a", "obj-1", meta("y"), vec![0u8; 1000], 60)
            .await
            .expect("覆盖上传成功");
        let inner = store.inner.lock().await;
        let usage = inner.owners.get("dev-a").expect("用量存在");
        assert_eq!(usage.bytes, 1000);
        assert_eq!(usage.count, 1);
    }

    #[tokio::test]
    async fn delete_only_by_owner() {
        let (_dir, store) = temp_store().await;
        store
            .put("dev-a", "obj-1", meta("x"), b"data".to_vec(), 60)
            .await
            .expect("上传成功");
        let error = store
            .delete("dev-b", "obj-1")
            .await
            .expect_err("其他设备不能删除");
        assert_eq!(error.code, "protocol_error");
        assert!(store.get("obj-1").await.is_ok());
    }

    #[tokio::test]
    async fn index_survives_reopen_and_startup_cleanup() {
        let dir = tempfile::tempdir().expect("临时目录");
        {
            let store = VaultStore::open(dir.path().to_path_buf(), limits())
                .await
                .expect("打开暂存");
            store
                .put("dev-a", "obj-keep", meta("x"), b"keep".to_vec(), 3600)
                .await
                .expect("上传成功");
            store
                .put("dev-a", "obj-drop", meta("y"), b"drop".to_vec(), 3600)
                .await
                .expect("上传成功");
        }
        // 直接改索引文件，模拟"重启前被外部标记过期"的对象（TTL 最小值 60s 无法用 API 构造）。
        let index_path = dir.path().join("index.json");
        let mut index: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&index_path).expect("读取索引"))
                .expect("解析索引");
        index["objects"]["obj-drop"]["expires_at_ms"] =
            serde_json::json!(now_ms().saturating_sub(1));
        std::fs::write(&index_path, serde_json::to_vec_pretty(&index).expect("序列化索引"))
            .expect("写回索引");
        // 孤儿文件：不在索引里，启动清理应删除。
        tokio::fs::write(dir.path().join("orphan.bin"), b"orphan")
            .await
            .expect("写孤儿文件");

        let store = VaultStore::open(dir.path().to_path_buf(), limits())
            .await
            .expect("重新打开暂存");
        assert!(store.get("obj-keep").await.is_ok());
        assert!(store.get("obj-drop").await.is_err());
        assert!(!dir.path().join("orphan.bin").exists());
        assert!(!dir.path().join("obj-drop.bin").exists());
    }

    #[tokio::test]
    async fn list_filters_by_owner_and_cleans_expired() {
        let (_dir, store) = temp_store().await;
        store
            .put("dev-a", "obj-a", meta("x"), b"data".to_vec(), 60)
            .await
            .expect("上传成功");
        store
            .put("dev-b", "obj-b", meta("y"), b"data".to_vec(), 60)
            .await
            .expect("上传成功");
        {
            let mut inner = store.inner.lock().await;
            let record = inner.objects.get_mut("obj-a").expect("记录存在");
            record.expires_at_ms = now_ms().saturating_sub(1);
        }
        let listed = store.list("dev-a").await;
        assert!(listed.is_empty(), "过期对象不应出现在列表");
        let listed = store.list("dev-b").await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].object_id, "obj-b");
    }
}
