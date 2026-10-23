//! 离线解除配对通知信箱（中继侧撤销通知存储/投递/确认/清理）。
//!
//! 场景：A 删除与 B 的配对时 B 离线。中继保存一条 A 用身份密钥签名的
//! [`RevocationNotice`]（**只存控制面元数据，≤8 KiB/条**），B 下次上线时投递；
//! B 处理完回 `revocation_ack` 后删除，未确认的通知保留到 TTL 过期（≤90 天）。
//!
//! 安全要点：
//! - 中继无法伪造：`revocation_put` 用 hello 认证得到的发送方公钥 [`verify`] 验签，
//!   要求 `content.from == 连接设备`、目标不是发送方自己、未过期、TTL 合法；
//! - 每个发送方/全局都有未处理通知数量上限，防止把中继当免费存储滥用；
//! - 同一 `notice_id` 重复 put 是幂等操作（返回已存的那条），不重复占额度；
//! - 落盘：`<data_dir>/revocations/index.json`（`Vec<StoredNotice>`，原子写 tmp+rename）。
//!
//! 日志脱敏：只打印 `notice_id` 前 8 个字符与大小，不打印 `reason`/签名/正文。
//!
//! [`verify`]: drive_core::revocation::RevocationNotice::verify

use std::path::{Path, PathBuf};

use drive_core::identity::PublicIdentity;
use drive_core::revocation::{RevocationNotice, MAX_REVOCATION_JSON_BYTES};
use drive_core::util::now_ms;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, warn};

const INDEX_DIR: &str = "revocations";
const INDEX_FILE: &str = "index.json";
const MAX_NOTICE_ID_LEN: usize = 128;

/// 索引里的一条待投递通知。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StoredNotice {
    /// 完整签名通知（内联存储，单条 ≤ `MAX_REVOCATION_JSON_BYTES`）。
    pub(crate) notice: RevocationNotice,
    /// 入信箱时间（毫秒）。
    pub(crate) stored_at_ms: u64,
    /// 冗余的发送方/接收方 device_id（索引排障/限额统计用，和 notice.content 一致）。
    pub(crate) from: String,
    pub(crate) to: String,
}

/// 撤销通知保留限额（0 表示该维度不限制）。
#[derive(Clone, Debug)]
pub(crate) struct RevocationLimits {
    /// 每个发送方最多保留的未处理通知数（默认 20）。
    pub(crate) max_per_sender: usize,
    /// 中继最多保留的未处理通知总数（默认 500）。
    pub(crate) max_total: usize,
}

/// 撤销通知存储错误：`code` 对应协议错误码，结果消息格式为 `"<code>: <detail>"`。
#[derive(Debug)]
pub(crate) struct RevocationError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl RevocationError {
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

/// `revocation_put` 成功结果。
#[derive(Debug)]
pub(crate) struct PutOutcome {
    pub(crate) notice: RevocationNotice,
    /// 同一 `notice_id` 重复投递（幂等，不新增存储）。
    pub(crate) duplicate: bool,
}

/// `revocation_ack` 结果（无线上响应，仅用于日志/测试断言）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AckOutcome {
    Removed,
    NotFound,
    NotOwner,
}

/// 撤销通知离线信箱。
pub(crate) struct RevocationStore {
    /// `<data_dir>/revocations/`（首次写入时才创建）。
    dir: PathBuf,
    limits: RevocationLimits,
    notices: Mutex<Vec<StoredNotice>>,
}

impl RevocationStore {
    /// 加载索引并做启动清理（过期/字段不一致的条目丢弃；索引损坏时按空信箱启动）。
    ///
    /// 启动失败（目录暂不存在/损坏）不影响中继启动：撤销信箱是尽力而为的异步清理通道。
    pub(crate) async fn open(data_dir: &Path, limits: RevocationLimits) -> Self {
        let dir = data_dir.join(INDEX_DIR);
        let index_path = dir.join(INDEX_FILE);
        let mut notices = match tokio::fs::read(&index_path).await {
            Ok(bytes) => match serde_json::from_slice::<Vec<StoredNotice>>(&bytes) {
                Ok(notices) => notices,
                Err(error) => {
                    warn!(%error, "撤销通知索引损坏，按空信箱启动");
                    // 保留现场便于人工排查，同时避免下次启动重复报错。
                    let corrupt_path = dir.join(format!("index.json.corrupt-{}", now_ms()));
                    let _ = tokio::fs::rename(&index_path, &corrupt_path).await;
                    Vec::new()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                warn!(%error, "读取撤销通知索引失败，按空信箱启动");
                Vec::new()
            }
        };

        let now = now_ms();
        let original = notices.len();
        notices.retain(|stored| {
            let consistent =
                stored.from == stored.notice.content.from && stored.to == stored.notice.content.to;
            if !consistent {
                debug!(
                    notice_id = %short_id(stored.notice.notice_id()),
                    "丢弃字段不一致的撤销通知"
                );
                return false;
            }
            if stored.notice.is_expired(now) {
                debug!(
                    notice_id = %short_id(stored.notice.notice_id()),
                    "启动清理过期撤销通知"
                );
                return false;
            }
            true
        });
        let cleaned = original - notices.len();

        let store = Self {
            dir,
            limits,
            notices: Mutex::new(notices),
        };
        if cleaned > 0 {
            let queued = store.notices.lock().await;
            if let Err(error) = persist(&store.dir, &queued).await {
                warn!(%error, "写入撤销通知索引失败");
            }
        }
        store
    }

    /// 接收一条已认证发送方的撤销通知（验签 + 限额 + 落盘）。
    ///
    /// 成功返回通知本体；调用方负责在目标在线时立即推送（存储保留到 ack/TTL）。
    pub(crate) async fn put(
        &self,
        sender: &PublicIdentity,
        notice: RevocationNotice,
    ) -> Result<PutOutcome, RevocationError> {
        let now = now_ms();
        let content = &notice.content;

        // 1) 发送方必须是通知的签名者本人，且不能向自己发撤销。
        if content.from != sender.device_id {
            return Err(RevocationError::new(
                "revocation_invalid",
                "撤销通知的 from 与当前认证设备不一致",
            ));
        }
        if content.to == sender.device_id {
            return Err(RevocationError::new(
                "revocation_invalid",
                "不能向自己发送解除配对通知",
            ));
        }
        if content.to.is_empty() || content.notice_id.is_empty() {
            return Err(RevocationError::new(
                "revocation_invalid",
                "撤销通知缺少 to/notice_id",
            ));
        }

        // 2) 版本/过期/TTL/签名校验（core 权威实现；中继只做存储前把关）。
        notice.verify(sender, now).map_err(|error| {
            RevocationError::new("revocation_invalid", format!("撤销通知校验失败: {error}"))
        })?;

        // 3) 单条大小上限（避免把中继当对象存储）。
        let json = serde_json::to_vec(&notice).map_err(|error| {
            RevocationError::new("protocol_error", format!("撤销通知序列化失败: {error}"))
        })?;
        if json.len() > MAX_REVOCATION_JSON_BYTES {
            return Err(RevocationError::new(
                "payload_too_large",
                format!(
                    "单条撤销通知 {} 字节超过上限 {} 字节",
                    json.len(),
                    MAX_REVOCATION_JSON_BYTES
                ),
            ));
        }

        let mut queued = self.notices.lock().await;

        // 4) 同 id 幂等：内容一致视为重复投递，不一致视为冲突。
        if let Some(existing) = queued
            .iter()
            .find(|stored| stored.notice.notice_id() == content.notice_id)
        {
            if existing.notice.content == *content {
                return Ok(PutOutcome {
                    notice: existing.notice.clone(),
                    duplicate: true,
                });
            }
            return Err(RevocationError::new(
                "revocation_invalid",
                "notice_id 冲突（相同 id 但通知内容不同）",
            ));
        }

        // 5) 限额（拒绝而非淘汰，避免静默丢通知；超限由发送方稍后重试）。
        if self.limits.max_per_sender > 0 {
            let per_sender = queued
                .iter()
                .filter(|stored| stored.from == content.from)
                .count();
            if per_sender >= self.limits.max_per_sender {
                return Err(RevocationError::new(
                    "revocation_quota_exceeded",
                    format!(
                        "每个设备最多保留 {} 条未处理撤销通知，请等待对端上线确认",
                        self.limits.max_per_sender
                    ),
                ));
            }
        }
        if self.limits.max_total > 0 && queued.len() >= self.limits.max_total {
            return Err(RevocationError::new(
                "revocation_quota_exceeded",
                format!("中继最多保留 {} 条未处理撤销通知", self.limits.max_total),
            ));
        }

        // 6) 入内存 + 原子落盘（失败回滚）。
        queued.push(StoredNotice {
            notice: notice.clone(),
            stored_at_ms: now,
            from: content.from.clone(),
            to: content.to.clone(),
        });
        if let Err(error) = persist(&self.dir, &queued).await {
            queued.pop();
            warn!(%error, "写入撤销通知索引失败");
            return Err(RevocationError::new(
                "data_dir_unavailable",
                "中继控制面状态目录不可用（写入撤销通知索引失败）",
            ));
        }
        debug!(
            notice_id = %short_id(&notice.content.notice_id),
            bytes = json.len(),
            "撤销通知已入离线信箱"
        );
        Ok(PutOutcome {
            notice,
            duplicate: false,
        })
    }

    /// 目标确认已处理：只允许删除 `to == device_id` 的通知；重复 ack 幂等。
    pub(crate) async fn ack(&self, device_id: &str, notice_id: &str) -> AckOutcome {
        if notice_id.is_empty() || notice_id.len() > MAX_NOTICE_ID_LEN {
            return AckOutcome::NotFound;
        }
        let mut queued = self.notices.lock().await;
        let Some(index) = queued
            .iter()
            .position(|stored| stored.notice.notice_id() == notice_id)
        else {
            return AckOutcome::NotFound;
        };
        if queued[index].to != device_id {
            return AckOutcome::NotOwner;
        }
        let removed = queued.remove(index);
        if let Err(error) = persist(&self.dir, &queued).await {
            warn!(%error, "写入撤销通知索引失败");
            queued.insert(index, removed);
            return AckOutcome::NotFound;
        }
        debug!(
            notice_id = %short_id(notice_id),
            "撤销通知已确认删除"
        );
        AckOutcome::Removed
    }

    /// 登录后取出该设备所有待处理通知（顺带清理过期项；不改动投递状态以便重试）。
    pub(crate) async fn pending_for(&self, device_id: &str) -> Vec<RevocationNotice> {
        let mut queued = self.notices.lock().await;
        let now = now_ms();
        let before = queued.len();
        queued.retain(|stored| !stored.notice.is_expired(now));
        if queued.len() != before {
            if let Err(error) = persist(&self.dir, &queued).await {
                warn!(%error, "写入撤销通知索引失败");
            }
        }
        queued
            .iter()
            .filter(|stored| stored.to == device_id)
            .map(|stored| stored.notice.clone())
            .collect()
    }

    /// 后台清理过期通知，返回删除数量。
    pub(crate) async fn reap(&self) -> usize {
        let mut queued = self.notices.lock().await;
        let now = now_ms();
        let before = queued.len();
        queued.retain(|stored| !stored.notice.is_expired(now));
        let removed = before - queued.len();
        if removed > 0 {
            if let Err(error) = persist(&self.dir, &queued).await {
                warn!(%error, "写入撤销通知索引失败");
            }
            debug!(count = removed, "清理过期撤销通知");
        }
        removed
    }
}

/// 日志脱敏：返回 `notice_id` 的前 8 个字符（按字符边界安全截断）。
pub(crate) fn short_id(notice_id: &str) -> String {
    let end = notice_id
        .char_indices()
        .nth(8)
        .map(|(index, _)| index)
        .unwrap_or(notice_id.len());
    notice_id[..end].to_string()
}

async fn persist(dir: &Path, notices: &[StoredNotice]) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    // 索引即 `Vec<StoredNotice>` 的 JSON 数组（通知内联）。
    let json = serde_json::to_vec_pretty(notices).map_err(std::io::Error::other)?;
    crate::fsutil::write_atomic(&dir.join(INDEX_FILE), &json).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use drive_core::identity::Identity;
    use drive_core::revocation::{create_now, reason, REVOCATION_TTL_SECS};

    fn limits() -> RevocationLimits {
        RevocationLimits {
            max_per_sender: 20,
            max_total: 500,
        }
    }

    async fn temp_store(limits: RevocationLimits) -> (tempfile::TempDir, RevocationStore) {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = RevocationStore::open(dir.path(), limits).await;
        (dir, store)
    }

    fn notice(from: &Identity, to: &str) -> RevocationNotice {
        create_now(from, to, reason::USER_REMOVED).expect("创建撤销通知")
    }

    #[tokio::test]
    async fn put_pending_ack_roundtrip_and_persistence() {
        let dir = tempfile::tempdir().expect("临时目录");
        let alice = Identity::generate("alice");
        let bob = Identity::generate("bob");
        let target = bob.device_id();
        {
            let store = RevocationStore::open(dir.path(), limits()).await;
            let expected = notice(&alice, &target);
            let notice_id = expected.notice_id().to_string();
            let outcome = store
                .put(&alice.public(), expected.clone())
                .await
                .expect("入信箱");
            assert!(!outcome.duplicate);

            let pending = store.pending_for(&target).await;
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].notice_id(), notice_id);
            assert!(store.pending_for(&alice.device_id()).await.is_empty());

            // 其他设备不能 ack 删除。
            assert_eq!(
                store.ack(&alice.device_id(), &notice_id).await,
                AckOutcome::NotOwner
            );
            assert_eq!(store.pending_for(&target).await.len(), 1);

            // 目标 ack 后删除；重复 ack 幂等。
            assert_eq!(store.ack(&target, &notice_id).await, AckOutcome::Removed);
            assert!(store.pending_for(&target).await.is_empty());
            assert_eq!(store.ack(&target, &notice_id).await, AckOutcome::NotFound);
        }
        // 重启后仍为空（索引已落盘）。
        let store = RevocationStore::open(dir.path(), limits()).await;
        assert!(store.pending_for(&target).await.is_empty());
    }

    #[tokio::test]
    async fn pending_survives_restart() {
        let dir = tempfile::tempdir().expect("临时目录");
        let alice = Identity::generate("alice");
        let bob = Identity::generate("bob");
        let target = bob.device_id();
        let expected = notice(&alice, &target);
        let notice_id = expected.notice_id().to_string();
        {
            let store = RevocationStore::open(dir.path(), limits()).await;
            store.put(&alice.public(), expected).await.expect("入信箱");
            let raw = tokio::fs::read(dir.path().join(INDEX_DIR).join(INDEX_FILE))
                .await
                .expect("索引存在");
            assert!(String::from_utf8_lossy(&raw).contains(&notice_id));
        }
        let store = RevocationStore::open(dir.path(), limits()).await;
        let pending = store.pending_for(&target).await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].notice_id(), notice_id);
    }

    #[tokio::test]
    async fn duplicate_put_is_idempotent_and_conflict_rejected() {
        let (_dir, store) = temp_store(limits()).await;
        let alice = Identity::generate("alice");
        let bob = Identity::generate("bob");
        let expected = notice(&alice, &bob.device_id());
        let first = store
            .put(&alice.public(), expected.clone())
            .await
            .expect("首次");
        assert!(!first.duplicate);
        let second = store
            .put(&alice.public(), expected.clone())
            .await
            .expect("重复");
        assert!(second.duplicate);
        assert_eq!(store.pending_for(&bob.device_id()).await.len(), 1);

        // 相同 notice_id、不同内容 → 冲突。
        let mut conflict = expected;
        conflict.content.reason = reason::INACTIVE_90D.to_string();
        let error = store
            .put(&alice.public(), conflict)
            .await
            .expect_err("冲突应拒绝");
        assert_eq!(error.code, "revocation_invalid");
    }

    #[tokio::test]
    async fn rejects_wrong_sender_and_forged_signature() {
        let (_dir, store) = temp_store(limits()).await;
        let alice = Identity::generate("alice");
        let bob = Identity::generate("bob");
        let mallory = Identity::generate("mallory");

        // 通知的 from 不是发送方 → 拒绝。
        let forged_from = notice(&alice, &bob.device_id());
        let error = store
            .put(&mallory.public(), forged_from)
            .await
            .expect_err("from 不匹配应拒绝");
        assert_eq!(error.code, "revocation_invalid");

        // 篡改签名 → 拒绝。
        let mut tampered = notice(&alice, &bob.device_id());
        tampered.signature[0] ^= 1;
        let error = store
            .put(&alice.public(), tampered)
            .await
            .expect_err("签名错误应拒绝");
        assert_eq!(error.code, "revocation_invalid");

        // 向自己发 → 拒绝（构造 from/to 相同、签名已失效的通知也应在结构校验被拒）。
        let mut self_target = notice(&alice, &bob.device_id());
        self_target.content.to = alice.device_id();
        let error = store
            .put(&alice.public(), self_target)
            .await
            .expect_err("自撤自身应拒绝");
        assert_eq!(error.code, "revocation_invalid");
    }

    #[tokio::test]
    async fn rejects_expired_and_oversized_notices() {
        let (_dir, store) = temp_store(limits()).await;
        let alice = Identity::generate("alice");
        let bob = Identity::generate("bob");

        let expired_at = now_ms().saturating_sub(REVOCATION_TTL_SECS * 1000 + 1000);
        let expired =
            RevocationNotice::create(&alice, &bob.device_id(), reason::USER_REMOVED, expired_at)
                .expect("创建过期通知");
        let error = store
            .put(&alice.public(), expired)
            .await
            .expect_err("过期通知应拒绝");
        assert_eq!(error.code, "revocation_invalid");

        let huge_reason = "x".repeat(MAX_REVOCATION_JSON_BYTES + 1024);
        let oversized = RevocationNotice::create(&alice, &bob.device_id(), &huge_reason, now_ms())
            .expect("创建超大通知");
        let error = store
            .put(&alice.public(), oversized)
            .await
            .expect_err("超大通知应拒绝");
        assert_eq!(error.code, "payload_too_large");
    }

    #[tokio::test]
    async fn per_sender_and_total_quota_enforced() {
        let (_dir, store) = temp_store(RevocationLimits {
            max_per_sender: 2,
            max_total: 3,
        })
        .await;
        let alice = Identity::generate("alice");
        let bob = Identity::generate("bob");
        let carol = Identity::generate("carol");
        let dave = Identity::generate("dave");
        let eve = Identity::generate("eve");

        store
            .put(&alice.public(), notice(&alice, &bob.device_id()))
            .await
            .expect("第 1 条");
        store
            .put(&alice.public(), notice(&alice, &carol.device_id()))
            .await
            .expect("第 2 条");
        let error = store
            .put(&alice.public(), notice(&alice, &dave.device_id()))
            .await
            .expect_err("超过单发送方上限应拒绝");
        assert_eq!(error.code, "revocation_quota_exceeded");

        store
            .put(&dave.public(), notice(&dave, &bob.device_id()))
            .await
            .expect("全局第 3 条");
        let error = store
            .put(&eve.public(), notice(&eve, &bob.device_id()))
            .await
            .expect_err("超过全局上限应拒绝");
        assert_eq!(error.code, "revocation_quota_exceeded");
    }

    #[tokio::test]
    async fn reap_removes_expired_and_startup_cleanup_drops_them() {
        let dir = tempfile::tempdir().expect("临时目录");
        let alice = Identity::generate("alice");
        let bob = Identity::generate("bob");
        let target = bob.device_id();
        {
            let store = RevocationStore::open(dir.path(), limits()).await;
            store
                .put(&alice.public(), notice(&alice, &target))
                .await
                .expect("入信箱");
            // 白盒模拟时间推进：直接把内存中的过期时间拨到过去。
            {
                let mut queued = store.notices.lock().await;
                queued[0].notice.content.expires_at_ms = now_ms().saturating_sub(1);
            }
            assert_eq!(store.reap().await, 1);
            assert!(store.pending_for(&target).await.is_empty());
        }
        // 构造索引里已过期的条目：启动时应清理。
        {
            let store = RevocationStore::open(dir.path(), limits()).await;
            store
                .put(&alice.public(), notice(&alice, &target))
                .await
                .expect("入信箱");
        }
        let index_path = dir.path().join(INDEX_DIR).join(INDEX_FILE);
        let mut index: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(&index_path)
                .await
                .expect("读索引"),
        )
        .expect("解析索引");
        index[0]["notice"]["content"]["expires_at_ms"] =
            serde_json::json!(now_ms().saturating_sub(1));
        tokio::fs::write(
            &index_path,
            serde_json::to_vec_pretty(&index).expect("序列化"),
        )
        .await
        .expect("写索引");

        let store = RevocationStore::open(dir.path(), limits()).await;
        assert!(store.pending_for(&target).await.is_empty());
        let raw = tokio::fs::read_to_string(&index_path)
            .await
            .expect("索引存在");
        assert!(!raw.contains("rev-"), "过期通知应已从索引删除");
    }

    #[tokio::test]
    async fn corrupt_index_tolerated_at_startup() {
        let dir = tempfile::tempdir().expect("临时目录");
        let index_dir = dir.path().join(INDEX_DIR);
        tokio::fs::create_dir_all(&index_dir)
            .await
            .expect("创建目录");
        tokio::fs::write(index_dir.join(INDEX_FILE), b"{not valid json")
            .await
            .expect("写坏索引");
        let alice = Identity::generate("alice");
        let bob = Identity::generate("bob");

        let store = RevocationStore::open(dir.path(), limits()).await;
        assert!(store.pending_for(&bob.device_id()).await.is_empty());
        // 坏索引被改名保留，不影响后续写入。
        store
            .put(&alice.public(), notice(&alice, &bob.device_id()))
            .await
            .expect("坏索引不阻塞写入");
        assert_eq!(store.pending_for(&bob.device_id()).await.len(), 1);
    }

    #[test]
    fn short_id_is_utf8_safe_and_bounded() {
        assert_eq!(short_id("rev-0123456789abcdef"), "rev-0123");
        assert_eq!(short_id("short"), "short");
        assert_eq!(short_id("解除配对通知撤销标识"), "解除配对通知撤销");
        assert_eq!(short_id(""), "");
    }
}
