//! Hello 认证（协议 §4.1）。
//!
//! 校验顺序（任一失败即关闭连接）：
//! 1. 协议版本 `v == PROTOCOL_VERSION`；
//! 2. `access_token` 常量时间比较；
//! 3. `PublicIdentity::is_consistent()`（device_id 必须由 ed_public 派生）；
//! 4. Ed25519 签名：`b"derive-sync-hello-v1" || nonce || device_id`。
//!
//! 认证材料（token / nonce / 签名）绝不写入日志。

use derive_core::identity::PublicIdentity;
use derive_core::util::ct_eq;
use derive_core::wire::{hello_sign_input, ClientToServer, WIRE_VERSION};

use crate::RelayConfig;

/// 认证失败：`code` 用于 `ServerToClient::Error`，`message` 为不含敏感信息的提示。
#[derive(Debug)]
pub(crate) struct AuthFailure {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl AuthFailure {
    fn new(code: &'static str, message: &str) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }
}

/// 校验首帧 `hello`，成功返回已认证的设备公开身份。
pub(crate) fn authenticate(
    config: &RelayConfig,
    first: ClientToServer,
) -> Result<PublicIdentity, AuthFailure> {
    let ClientToServer::Hello {
        v,
        device,
        access_token,
        nonce,
        signature,
    } = first
    else {
        return Err(AuthFailure::new("auth_failed", "首帧必须是 hello 认证消息"));
    };

    if v != WIRE_VERSION {
        return Err(AuthFailure::new(
            "bad_version",
            "协议版本不匹配，请升级客户端/服务端",
        ));
    }

    if !ct_eq(access_token.as_bytes(), config.access_token.as_bytes()) {
        return Err(AuthFailure::new("auth_failed", "access_token 无效"));
    }

    if !device.is_consistent() {
        return Err(AuthFailure::new(
            "auth_failed",
            "device_id 与 Ed25519 公钥不一致",
        ));
    }

    // 协议要求 nonce 为 32 hex 随机串；这里只做长度防御（不约束具体编码，避免跨实现不兼容）。
    if nonce.is_empty() || nonce.len() > 128 {
        return Err(AuthFailure::new("auth_failed", "nonce 长度非法"));
    }

    let input = hello_sign_input(&nonce, &device.device_id);
    if !device.verify(&input, &signature) {
        return Err(AuthFailure::new("auth_failed", "hello 签名无效"));
    }

    Ok(device)
}

#[cfg(test)]
mod tests {
    use super::*;
    use derive_core::identity::Identity;

    fn config() -> RelayConfig {
        RelayConfig::new("test-token")
    }

    fn valid_hello(identity: &Identity) -> ClientToServer {
        let device = identity.public();
        let nonce = "00112233445566778899aabbccddeeff".to_string();
        let signature = identity.sign(&hello_sign_input(&nonce, &device.device_id));
        ClientToServer::Hello {
            v: WIRE_VERSION,
            device,
            access_token: "test-token".to_string(),
            nonce,
            signature,
        }
    }

    #[test]
    fn accepts_valid_hello() {
        let identity = Identity::generate("dev");
        let device = authenticate(&config(), valid_hello(&identity)).expect("认证应通过");
        assert_eq!(device.device_id, identity.device_id());
    }

    #[test]
    fn rejects_wrong_token() {
        let identity = Identity::generate("dev");
        let mut hello = valid_hello(&identity);
        if let ClientToServer::Hello { access_token, .. } = &mut hello {
            *access_token = "wrong-token".to_string();
        }
        let failure = authenticate(&config(), hello).expect_err("错误 token 必须拒绝");
        assert_eq!(failure.code, "auth_failed");
    }

    #[test]
    fn rejects_bad_version() {
        let identity = Identity::generate("dev");
        let mut hello = valid_hello(&identity);
        if let ClientToServer::Hello { v, .. } = &mut hello {
            *v = WIRE_VERSION + 1;
        }
        let failure = authenticate(&config(), hello).expect_err("版本不匹配必须拒绝");
        assert_eq!(failure.code, "bad_version");
    }

    #[test]
    fn rejects_bad_signature() {
        let identity = Identity::generate("dev");
        let mut hello = valid_hello(&identity);
        if let ClientToServer::Hello { signature, .. } = &mut hello {
            *signature = [0u8; 64];
        }
        let failure = authenticate(&config(), hello).expect_err("错误签名必须拒绝");
        assert_eq!(failure.code, "auth_failed");
    }

    #[test]
    fn rejects_inconsistent_device_id() {
        let identity = Identity::generate("dev");
        let mut hello = valid_hello(&identity);
        if let ClientToServer::Hello { device, .. } = &mut hello {
            device.device_id = "dev-0000000000000000".to_string();
        }
        let failure = authenticate(&config(), hello).expect_err("身份不一致必须拒绝");
        assert_eq!(failure.code, "auth_failed");
    }

    #[test]
    fn rejects_non_hello_first_frame() {
        let failure = authenticate(&config(), ClientToServer::Ping { ts_ms: 0 })
            .expect_err("首帧不是 hello 必须拒绝");
        assert_eq!(failure.code, "auth_failed");
    }
}
