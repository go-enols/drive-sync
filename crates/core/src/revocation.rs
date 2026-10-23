//! 解除配对通知（离线撤销的持久化控制面消息）。
//!
//! 场景：A 删除与 B 的配对时 B 离线。中继作为"离线信箱"保存一条 A 签名的
//! [`RevocationNotice`]（**只存控制面元数据，不存内容**），B 下次上线时取回并
//! 用本地 PairRecord 中 A 的 Ed25519 公钥验签，验签通过才删除本地配对记录。
//!
//! 安全要点：
//! - 通知由删除方身份密钥签名，中继无法伪造（伪造会被验签拒绝）；
//! - 通知带过期时间（默认 90 天，与"90 天不活跃需重新配对"窗口对齐）；
//! - 中继只按 to 设备投递，不解析其他字段；不存在私钥泄露面。

use serde::{Deserialize, Serialize};

use crate::identity::{Identity, PublicIdentity};
use crate::util::{blake3_hex, canonical_json, now_ms, random_hex};
use crate::{CoreError, Result};

pub const REVOCATION_VERSION: u32 = 1;
/// 通知有效期（秒）：90 天。与不活跃配对过期窗口对齐。
pub const REVOCATION_TTL_SECS: u64 = 90 * 24 * 60 * 60;
/// 单条通知序列化后的上限（防止滥用中继存储）。
pub const MAX_REVOCATION_JSON_BYTES: usize = 8 * 1024;
/// 一条通知可接受的时钟误差（未来时间容忍）。
pub const REVOCATION_CLOCK_SKEW_MS: u64 = 10 * 60 * 1000;

/// 撤销原因（自由字符串，约定值见常量）。
pub mod reason {
    /// 用户主动删除配对记录。
    pub const USER_REMOVED: &str = "user_removed";
    /// 超过 90 天未与对端成功同步/心跳，配对按策略过期。
    pub const INACTIVE_90D: &str = "inactive_90d";
}

/// 签名覆盖的内容部分（`signature` 之外的全部字段）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevocationContent {
    pub v: u32,
    pub notice_id: String,
    pub from: String,
    pub to: String,
    pub reason: String,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
}

/// 一条待投递的解除配对通知。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevocationNotice {
    pub content: RevocationContent,
    #[serde(with = "crate::util::hex64")]
    pub signature: [u8; 64],
}

impl RevocationNotice {
    /// 由待删除方（删除配对的一方）生成并签名。
    pub fn create(identity: &Identity, target: &str, reason: &str, now_ms: u64) -> Result<Self> {
        if target == identity.device_id() {
            return Err(CoreError::Invalid("不能向自身发送解除配对通知".into()));
        }
        let content = RevocationContent {
            v: REVOCATION_VERSION,
            notice_id: format!("rev-{}", random_hex(12)),
            from: identity.device_id(),
            to: target.to_string(),
            reason: reason.to_string(),
            created_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(REVOCATION_TTL_SECS * 1000),
        };
        let signature = identity.sign(&signing_input(&content)?);
        Ok(Self { content, signature })
    }

    /// 接收方校验：版本、目标、过期、签名（必须用本地配对记录中的对端公钥校验）。
    pub fn verify(&self, expected_from: &PublicIdentity, now_ms: u64) -> Result<()> {
        let c = &self.content;
        if c.v != REVOCATION_VERSION {
            return Err(CoreError::Protocol(format!(
                "不支持的解除配对通知版本: {}",
                c.v
            )));
        }
        if c.from != expected_from.device_id {
            return Err(CoreError::Verification(
                "解除配对通知的发送方与本地配对记录不一致".into(),
            ));
        }
        if c.created_at_ms > now_ms.saturating_add(REVOCATION_CLOCK_SKEW_MS) {
            return Err(CoreError::Verification(
                "解除配对通知的时间在未来（时钟异常或伪造）".into(),
            ));
        }
        if c.expires_at_ms <= now_ms {
            return Err(CoreError::Verification("解除配对通知已过期".into()));
        }
        if c.expires_at_ms <= c.created_at_ms
            || c.expires_at_ms - c.created_at_ms > REVOCATION_TTL_SECS * 1000
        {
            return Err(CoreError::Verification("解除配对通知有效期非法".into()));
        }
        if !expected_from.verify(&signing_input(c)?, &self.signature) {
            return Err(CoreError::Verification("解除配对通知签名无效".into()));
        }
        Ok(())
    }

    /// 校验目标设备是接收方自己。
    pub fn verify_target(&self, self_device_id: &str) -> Result<()> {
        if self.content.to != self_device_id {
            return Err(CoreError::Verification("解除配对通知目标不是本机".into()));
        }
        Ok(())
    }

    pub fn notice_id(&self) -> &str {
        &self.content.notice_id
    }

    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.content.expires_at_ms <= now_ms
    }

    pub fn to_json(&self) -> Result<String> {
        let json = serde_json::to_string(self)?;
        if json.len() > MAX_REVOCATION_JSON_BYTES {
            return Err(CoreError::Invalid("解除配对通知过大".into()));
        }
        Ok(json)
    }

    /// 内容指纹（日志/去重审计用，不含签名）。
    pub fn fingerprint(&self) -> String {
        blake3_hex(canonical_json(&self.content).unwrap_or_default().as_bytes())[..16].to_string()
    }
}

fn signing_input(content: &RevocationContent) -> Result<Vec<u8>> {
    let mut buf = b"drive-sync-revocation-v1".to_vec();
    buf.extend_from_slice(canonical_json(content)?.as_bytes());
    Ok(buf)
}

/// 便捷：用当前时间生成通知。
pub fn create_now(identity: &Identity, target: &str, reason: &str) -> Result<RevocationNotice> {
    RevocationNotice::create(identity, target, reason, now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_roundtrip() {
        let a = Identity::generate("A");
        let b = Identity::generate("B");
        let notice =
            RevocationNotice::create(&a, &b.device_id(), reason::USER_REMOVED, 1000).unwrap();
        notice.verify_target(&b.device_id()).unwrap();
        notice.verify(&a.public(), 2000).unwrap();
        assert_eq!(notice.content.from, a.device_id());
        assert_eq!(notice.content.to, b.device_id());
        assert!(!notice.is_expired(2000));
        assert!(!notice.fingerprint().is_empty());
        assert!(notice.to_json().unwrap().len() < MAX_REVOCATION_JSON_BYTES);
    }

    #[test]
    fn rejects_wrong_signer_expired_and_self() {
        let a = Identity::generate("A");
        let b = Identity::generate("B");
        let evil = Identity::generate("evil");
        let notice =
            RevocationNotice::create(&a, &b.device_id(), reason::USER_REMOVED, 1000).unwrap();
        assert!(notice.verify(&evil.public(), 2000).is_err());
        assert!(notice
            .verify(&a.public(), 1000 + REVOCATION_TTL_SECS * 1000)
            .is_err());
        assert!(notice.verify_target(&a.device_id()).is_err());
        assert!(RevocationNotice::create(&a, &a.device_id(), reason::USER_REMOVED, 1000).is_err());
    }

    #[test]
    fn tamper_detected() {
        let a = Identity::generate("A");
        let b = Identity::generate("B");
        let mut notice =
            RevocationNotice::create(&a, &b.device_id(), reason::USER_REMOVED, 1000).unwrap();
        notice.content.reason = reason::INACTIVE_90D.into();
        assert!(notice.verify(&a.public(), 2000).is_err());
        let mut sig_bad =
            RevocationNotice::create(&a, &b.device_id(), reason::USER_REMOVED, 1000).unwrap();
        sig_bad.signature[0] ^= 1;
        assert!(sig_bad.verify(&a.public(), 2000).is_err());
    }
}
