//! 对称加密原语：XChaCha20-Poly1305 AEAD 与 HKDF-SHA256。

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    Key, XChaCha20Poly1305, XNonce,
};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;

use crate::{CoreError, Result};

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 24;
pub const TAG_LEN: usize = 16;

/// 生成 32 字节密钥材料。
pub fn random_key() -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    rand::rngs::OsRng.fill_bytes(&mut key);
    key
}

/// XChaCha20-Poly1305 加密。输出布局：`nonce(24) || ciphertext || tag(16)`。
pub fn seal(key: &[u8; KEY_LEN], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CoreError::Crypto("AEAD 加密失败".into()))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// XChaCha20-Poly1305 解密，输入布局同 [`seal`]。
pub fn open(key: &[u8; KEY_LEN], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return Err(CoreError::Crypto("密文长度不足".into()));
    }
    let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| CoreError::Crypto("AEAD 解密失败（密钥错误、AAD 不匹配或数据损坏）".into()))
}

/// HKDF-SHA256 派生任意长度密钥材料。
pub fn hkdf_expand(ikm: &[u8], salt: &[u8], info: &[u8], len: usize) -> Result<Vec<u8>> {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut okm = vec![0u8; len];
    hk.expand(info, &mut okm)
        .map_err(|_| CoreError::Crypto(format!("HKDF 派生失败（长度 {len} 非法）")))?;
    Ok(okm)
}

pub fn hkdf32(ikm: &[u8], salt: &[u8], info: &[u8]) -> Result<[u8; KEY_LEN]> {
    let okm = hkdf_expand(ikm, salt, info, KEY_LEN)?;
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&okm);
    Ok(key)
}

pub fn hkdf64(ikm: &[u8], salt: &[u8], info: &[u8]) -> Result<[u8; 64]> {
    let okm = hkdf_expand(ikm, salt, info, 64)?;
    let mut key = [0u8; 64];
    key.copy_from_slice(&okm);
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let key = random_key();
        let sealed = seal(&key, b"aad", b"hello drive-sync").unwrap();
        assert_eq!(open(&key, b"aad", &sealed).unwrap(), b"hello drive-sync");
        assert!(open(&key, b"other", &sealed).is_err());
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(open(&key, b"aad", &bad).is_err());
    }

    #[test]
    fn hkdf_is_deterministic_and_salt_sensitive() {
        let a = hkdf32(b"ikm", b"salt", b"info").unwrap();
        let b = hkdf32(b"ikm", b"salt", b"info").unwrap();
        let c = hkdf32(b"ikm", b"salt2", b"info").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
