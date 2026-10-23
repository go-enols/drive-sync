//! 设备身份：Ed25519 签名密钥 + X25519 密钥交换密钥。

use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};

use crate::util::{blake3_hex, now_ms};
use crate::{CoreError, Result};

pub const IDENTITY_VERSION: u32 = 1;

/// 磁盘上的身份文件。**这是最敏感的文件，应设为 0600 权限并考虑 OS 密钥链保护。**
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IdentityFile {
    pub v: u32,
    pub name: String,
    pub ed_secret_hex: String,
    pub x_secret_hex: String,
    pub created_at_ms: u64,
}

#[derive(Clone)]
pub struct Identity {
    pub name: String,
    ed: SigningKey,
    x: StaticSecret,
    pub created_at_ms: u64,
}

impl Identity {
    pub fn generate(name: &str) -> Self {
        let mut ed_seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut ed_seed);
        let mut x_seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut x_seed);
        Self {
            name: name.to_string(),
            ed: SigningKey::from_bytes(&ed_seed),
            x: StaticSecret::from(x_seed),
            created_at_ms: now_ms(),
        }
    }

    pub fn from_file(file: IdentityFile) -> Result<Self> {
        if file.v != IDENTITY_VERSION {
            return Err(CoreError::Invalid(format!(
                "不支持的身份文件版本: {}",
                file.v
            )));
        }
        let ed_bytes = hex::decode(&file.ed_secret_hex)
            .map_err(|e| CoreError::Codec(format!("身份文件 ed25519 种子非法: {e}")))?;
        let x_bytes = hex::decode(&file.x_secret_hex)
            .map_err(|e| CoreError::Codec(format!("身份文件 x25519 种子非法: {e}")))?;
        let ed_seed: [u8; 32] = ed_bytes
            .try_into()
            .map_err(|_| CoreError::Invalid("ed25519 种子长度应为 32 字节".into()))?;
        let x_seed: [u8; 32] = x_bytes
            .try_into()
            .map_err(|_| CoreError::Invalid("x25519 种子长度应为 32 字节".into()))?;
        Ok(Self {
            name: file.name,
            ed: SigningKey::from_bytes(&ed_seed),
            x: StaticSecret::from(x_seed),
            created_at_ms: file.created_at_ms,
        })
    }

    /// 加载或创建身份文件。返回 `(身份, 是否新建)`。
    pub fn load_or_create(path: &Path, name: &str) -> Result<(Self, bool)> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)?;
            let file: IdentityFile = serde_json::from_str(&raw)?;
            return Ok((Self::from_file(file)?, false));
        }
        let identity = Self::generate(name);
        identity.save(path)?;
        Ok((identity, true))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = IdentityFile {
            v: IDENTITY_VERSION,
            name: self.name.clone(),
            ed_secret_hex: hex::encode(self.ed.to_bytes()),
            x_secret_hex: hex::encode(self.x.to_bytes()),
            created_at_ms: self.created_at_ms,
        };
        let raw = serde_json::to_string_pretty(&file)?;
        std::fs::write(path, raw)?;
        restrict_permissions(path)?;
        Ok(())
    }

    pub fn device_id(&self) -> String {
        device_id_from_ed_bytes(&self.ed.verifying_key().to_bytes())
    }

    pub fn public(&self) -> PublicIdentity {
        let ed_public = self.ed.verifying_key().to_bytes();
        PublicIdentity {
            device_id: device_id_from_ed_bytes(&ed_public),
            name: self.name.clone(),
            ed_public,
            x_public: XPublicKey::from(&self.x).to_bytes(),
            created_at_ms: self.created_at_ms,
        }
    }

    pub fn ed_public_bytes(&self) -> [u8; 32] {
        self.ed.verifying_key().to_bytes()
    }

    pub fn x_public_bytes(&self) -> [u8; 32] {
        XPublicKey::from(&self.x).to_bytes()
    }

    /// 用 Ed25519 身份密钥签名。
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.ed.sign(msg).to_bytes()
    }

    /// 与对端做 X25519 密钥交换（返回 32 字节共享秘密）。
    pub fn diffie_hellman(&self, peer_x_public: &[u8; 32]) -> [u8; 32] {
        self.x.diffie_hellman(&XPublicKey::from(*peer_x_public)).to_bytes()
    }
}

/// 可公开分享的设备身份信息（配对时交换、写入 pair 记录）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicIdentity {
    pub device_id: String,
    pub name: String,
    #[serde(with = "crate::util::hex32")]
    pub ed_public: [u8; 32],
    #[serde(with = "crate::util::hex32")]
    pub x_public: [u8; 32],
    pub created_at_ms: u64,
}

impl PublicIdentity {
    pub fn verify(&self, msg: &[u8], sig: &[u8; 64]) -> bool {
        let Ok(vk) = VerifyingKey::from_bytes(&self.ed_public) else {
            return false;
        };
        vk.verify(msg, &Signature::from_bytes(sig)).is_ok()
    }

    /// 校验 device_id 确实由 ed_public 派生（防止身份字段被篡改）。
    pub fn is_consistent(&self) -> bool {
        device_id_from_ed_bytes(&self.ed_public) == self.device_id
    }

    pub fn short_fingerprint(&self) -> String {
        blake3_hex(&self.ed_public)[..16].to_string()
    }
}

/// device_id = "dev-" + BLAKE3(ed25519 公钥) 的前 16 个 hex 字符。
pub fn device_id_from_ed_bytes(ed_public: &[u8; 32]) -> String {
    format!("dev-{}", &blake3_hex(ed_public)[..16])
}

/// 为敏感文件设置 0600 权限（Windows 上由用户目录 ACL 保护，跳过）。
fn restrict_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// 默认身份文件路径：`<data_dir>/identity.json`。
pub fn identity_path(data_dir: &Path) -> PathBuf {
    data_dir.join("identity.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::random_hex;

    #[test]
    fn identity_roundtrip_and_sign() {
        let dir = std::env::temp_dir().join(format!("dsync-id-test-{}", random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = identity_path(&dir);
        let (id, created) = Identity::load_or_create(&path, "test-device").unwrap();
        assert!(created);
        let pub_id = id.public();
        assert!(pub_id.is_consistent());
        assert_eq!(pub_id.device_id, id.device_id());

        let sig = id.sign(b"hello");
        assert!(pub_id.verify(b"hello", &sig));
        assert!(!pub_id.verify(b"hell0", &sig));

        let (loaded, created2) = Identity::load_or_create(&path, "ignored").unwrap();
        assert!(!created2);
        assert_eq!(loaded.device_id(), id.device_id());
        std::fs::remove_dir_all(&dir).ok();
    }
}
