//! 可选云端暂存（30 天 TTL）相关类型。
//!
//! 设计约束：
//! - 服务端只保存**客户端加密后的密文**，无法解密内容；
//! - 每个对象带 TTL，服务端强制上限 30 天，到期清理任务删除；
//! - 这是"可选中转仓"，不是备份服务：删除后不可恢复，客户端应保留本地副本。

use serde::{Deserialize, Serialize};

/// 云端对象元数据（服务端可见，因此不应包含敏感内容）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VaultMeta {
    /// 展示名（如 "xxx-1.2.3.zip"）。
    pub name: String,
    /// 内容分类：installer / archive / document / clipboard 等。
    pub kind: String,
    /// 明文大小（字节）。
    pub size: u64,
    /// 明文 BLAKE3（客户端校验用）。
    pub content_hash: String,
    /// 关联的软件目录条目 id（可选）。
    #[serde(default)]
    pub entry_id: Option<String>,
    /// 备注（不要写敏感信息）。
    #[serde(default)]
    pub note: String,
}

/// 服务端返回的对象列表项。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VaultObjectInfo {
    pub object_id: String,
    pub meta: VaultMeta,
    pub ciphertext_len: u64,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
}

/// 云端保留上限：30 天。
pub const MAX_VAULT_TTL_SECS: u64 = 30 * 24 * 60 * 60;
/// 最小 TTL（便于测试；生产可按小时下限）。
pub const MIN_VAULT_TTL_SECS: u64 = 60;

/// 把请求的 TTL 收敛到允许范围。
pub fn effective_ttl_secs(requested: u64) -> u64 {
    requested.clamp(MIN_VAULT_TTL_SECS, MAX_VAULT_TTL_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_is_clamped() {
        assert_eq!(effective_ttl_secs(0), MIN_VAULT_TTL_SECS);
        assert_eq!(effective_ttl_secs(3600), 3600);
        assert_eq!(effective_ttl_secs(u64::MAX), MAX_VAULT_TTL_SECS);
    }
}
