//! 客户端 ↔ 中继服务器的在线协议（JSON over WebSocket）。
//!
//! 中继只做三件事：设备连接注册、配对信息转发、E2E 密文转发（以及可选的加密暂存）。
//! 它不持有任何明文内容，也不持久化剪切板/目录/文件数据。

use serde::{Deserialize, Serialize};

use crate::pairing::{PairInvite, PairJoinPayload};
use crate::session::{E2eEnvelope, SessionAckMsg, SessionInitMsg};
use crate::vault::{VaultMeta, VaultObjectInfo};
use crate::PROTOCOL_VERSION;

/// 客户端 → 中继。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientToServer {
    /// 连接认证：access_token + Ed25519 签名（证明持有 device 私钥）。
    Hello {
        v: u32,
        device: crate::identity::PublicIdentity,
        access_token: String,
        nonce: String,
        #[serde(with = "crate::util::hex64")]
        signature: [u8; 64],
    },
    /// 发起配对：中继只保存 code_hash 与邀请，TTL 到期自动清除。
    PairAnnounce {
        code_hash: String,
        invite: PairInvite,
        ttl_secs: u64,
    },
    /// 用配对码加入。
    PairJoin {
        code_hash: String,
        join: PairJoinPayload,
    },
    /// E2E 会话握手第 1 步：发起方交给中继转发给对端。
    SessionInit { to: String, msg: SessionInitMsg },
    /// E2E 会话握手第 2 步：响应方交给中继转发回发起方。
    SessionAck { to: String, msg: SessionAckMsg },
    /// 转发 E2E 信封（中继不解析 ciphertext）。
    Relay { to: String, envelope: E2eEnvelope },
    /// 可选：加密暂存上传（服务端只存密文，TTL ≤ 30 天）。
    VaultPut {
        object_id: String,
        meta: VaultMeta,
        #[serde(with = "crate::util::b64_vec")]
        ciphertext: Vec<u8>,
        ttl_secs: u64,
    },
    VaultGet { object_id: String },
    VaultList,
    VaultDelete { object_id: String },
    /// 应用层心跳（服务端立即回 Pong）。
    Ping { ts_ms: u64 },
}

/// 中继 → 客户端。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerToClient {
    Welcome {
        v: u32,
        device_id: String,
        session_id: String,
        server_time_ms: u64,
        online: Vec<String>,
    },
    /// 配对事件：invite 给加入方，join 给发起方。
    PairEvent {
        from: String,
        #[serde(default)]
        invite: Option<PairInvite>,
        #[serde(default)]
        join: Option<PairJoinPayload>,
    },
    PairResult {
        ok: bool,
        message: String,
        #[serde(default)]
        peer: Option<String>,
    },
    /// 对端发来的会话握手请求（设备间直接处理，中继只转发）。
    SessionInit { from: String, msg: SessionInitMsg },
    /// 对端返回的会话握手响应。
    SessionAck { from: String, msg: SessionAckMsg },
    Relay {
        from: String,
        envelope: E2eEnvelope,
    },
    /// 在线设备列表变化（仅本服务器上的设备；加密内容与本字段无关）。
    Presence { online: Vec<String> },
    VaultPutResult {
        ok: bool,
        object_id: String,
        expires_at_ms: u64,
        message: String,
    },
    VaultGetResult {
        ok: bool,
        object_id: String,
        #[serde(default)]
        meta: Option<VaultMeta>,
        #[serde(default, with = "crate::util::b64_opt_vec")]
        ciphertext: Option<Vec<u8>>,
        message: String,
    },
    VaultListResult {
        ok: bool,
        objects: Vec<VaultObjectInfo>,
        message: String,
    },
    VaultDeleteResult {
        ok: bool,
        object_id: String,
        message: String,
    },
    Pong { ts_ms: u64 },
    Error { code: String, message: String },
}

/// Hello 签名输入：`b"derive-sync-hello-v1" || nonce || device_id`。
pub fn hello_sign_input(nonce: &str, device_id: &str) -> Vec<u8> {
    let mut buf = b"derive-sync-hello-v1".to_vec();
    buf.extend_from_slice(nonce.as_bytes());
    buf.extend_from_slice(device_id.as_bytes());
    buf
}

/// 协议版本常量（与 `Welcome.v` 对比）。
pub const WIRE_VERSION: u32 = PROTOCOL_VERSION;

impl ClientToServer {
    pub fn to_json(&self) -> crate::Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    pub fn from_json(s: &str) -> crate::Result<Self> {
        Ok(serde_json::from_str(s)?)
    }
}

impl ServerToClient {
    pub fn to_json(&self) -> crate::Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    pub fn from_json(s: &str) -> crate::Result<Self> {
        Ok(serde_json::from_str(s)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::PublicIdentity;

    fn dummy_device() -> PublicIdentity {
        PublicIdentity {
            device_id: crate::identity::device_id_from_ed_bytes(&[1u8; 32]),
            name: "d".into(),
            ed_public: [1u8; 32],
            x_public: [2u8; 32],
            created_at_ms: 0,
        }
    }

    #[test]
    fn wire_roundtrip() {
        let hello = ClientToServer::Hello {
            v: WIRE_VERSION,
            device: dummy_device(),
            access_token: "token".into(),
            nonce: "abc".into(),
            signature: [9u8; 64],
        };
        let json = hello.to_json().unwrap();
        assert!(json.contains("\"t\":\"hello\""));
        let back = ClientToServer::from_json(&json).unwrap();
        matches!(back, ClientToServer::Hello { .. });

        let pong = ServerToClient::Pong { ts_ms: 42 };
        assert_eq!(
            ServerToClient::from_json(&pong.to_json().unwrap())
                .unwrap()
                .to_json()
                .unwrap(),
            pong.to_json().unwrap()
        );
    }

    #[test]
    fn hello_sign_input_stable() {
        let a = hello_sign_input("n", "dev-1");
        let b = hello_sign_input("n", "dev-1");
        assert_eq!(a, b);
        assert_ne!(a, hello_sign_input("n", "dev-2"));
    }
}
