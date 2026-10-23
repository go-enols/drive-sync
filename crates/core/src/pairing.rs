//! 设备配对：一次性二维码 + 6 位短认证串（SAS），防中继服务器 MITM。
//!
//! 流程：
//! 1. A 生成一次性配对码（8 位数字）与临时 X25519 密钥，把邀请信息经中继挂起（只挂 code_hash）；
//! 2. B 输入配对码（或扫 A 的二维码）后发出自己的临时公钥与身份公钥；
//! 3. 双方用临时密钥做 X25519，结合配对码与双方身份公钥派生配对密钥与 6 位 SAS；
//! 4. 用户比对两端 SAS 一致后确认 → 保存长期 PairRecord；
//! 5. 之后重连默认自动认证，无需再次配对。
//!
//! 若中继服务器替换公钥做 MITM，两端算出的 SAS 必然不同，会被用户发现。

use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};

use crate::crypto::hkdf_expand;
use crate::identity::{Identity, PublicIdentity};
use crate::util::{b64url_d, b64url_e, blake3_hex, canonical_json, now_ms, random_bytes};
use crate::{CoreError, Result};

pub const PAIRING_VERSION: u32 = 1;
pub const PAIR_CODE_LEN: usize = 8;
pub const QR_PREFIX: &str = "dsync-pair-v1:";

/// A 端广播的配对邀请（不含配对码明文）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairInvite {
    pub v: u32,
    pub relay_url: String,
    pub device: PublicIdentity,
    #[serde(with = "crate::util::hex32")]
    pub ephemeral_pub: [u8; 32],
    pub created_at_ms: u64,
}

impl PairInvite {
    pub fn validate(&self) -> Result<()> {
        if self.v != PAIRING_VERSION {
            return Err(CoreError::Invalid(format!(
                "不支持的配对协议版本: {}",
                self.v
            )));
        }
        if !self.device.is_consistent() {
            return Err(CoreError::Verification("配对邀请的设备身份不一致".into()));
        }
        if self.ephemeral_pub.iter().all(|b| *b == 0) {
            return Err(CoreError::Invalid("配对邀请的临时公钥非法".into()));
        }
        if self.relay_url.trim().is_empty() {
            return Err(CoreError::Invalid("配对邀请缺少中继地址".into()));
        }
        Ok(())
    }
}

/// B 端返回的加入信息。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairJoinPayload {
    pub v: u32,
    pub device: PublicIdentity,
    #[serde(with = "crate::util::hex32")]
    pub ephemeral_pub: [u8; 32],
    pub created_at_ms: u64,
}

impl PairJoinPayload {
    pub fn validate(&self) -> Result<()> {
        if self.v != PAIRING_VERSION {
            return Err(CoreError::Invalid(format!(
                "不支持的配对协议版本: {}",
                self.v
            )));
        }
        if !self.device.is_consistent() {
            return Err(CoreError::Verification("加入方设备身份不一致".into()));
        }
        if self.ephemeral_pub.iter().all(|b| *b == 0) {
            return Err(CoreError::Invalid("加入方临时公钥非法".into()));
        }
        Ok(())
    }
}

/// 二维码内容：邀请 + 配对码（二维码本身是带外认证通道）。
#[derive(Serialize, Deserialize)]
struct PairQrPayload {
    invite: PairInvite,
    code: String,
}

pub fn encode_qr(invite: &PairInvite, code: &str) -> Result<String> {
    let payload = PairQrPayload {
        invite: invite.clone(),
        code: code.to_string(),
    };
    Ok(format!(
        "{QR_PREFIX}{}",
        b64url_e(serde_json::to_string(&payload)?.as_bytes())
    ))
}

/// 解析二维码字符串，返回 `(邀请, 配对码)`。
pub fn decode_qr(s: &str) -> Result<(PairInvite, String)> {
    let body = s
        .strip_prefix(QR_PREFIX)
        .ok_or_else(|| CoreError::Invalid("不是 drive-sync 配对二维码".into()))?;
    let raw = b64url_d(body)?;
    let payload: PairQrPayload = serde_json::from_slice(&raw)?;
    payload.invite.validate()?;
    if payload.code.len() != PAIR_CODE_LEN {
        return Err(CoreError::Invalid("配对码长度非法".into()));
    }
    Ok((payload.invite, payload.code))
}

/// 配对成功后的长期记录（存于本地 peers.json）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairRecord {
    pub v: u32,
    pub peer: PublicIdentity,
    pub relay_url: String,
    pub paired_at_ms: u64,
    pub label: String,
    /// 配对密钥指纹（用于两端核对/审计，不保存配对密钥本身）。
    pub pairing_fingerprint: String,
}

impl PairRecord {
    pub fn display_name(&self) -> String {
        if self.label.trim().is_empty() {
            self.peer.name.clone()
        } else {
            self.label.clone()
        }
    }
}

/// 生成本地配对会话。
pub struct PairingInitiator {
    pub invite: PairInvite,
    pub code: String,
    eph: StaticSecret,
    pub peer_device: Option<PublicIdentity>,
    pub peer_ephemeral: Option<[u8; 32]>,
    shared: Option<[u8; 32]>,
}

impl PairingInitiator {
    pub fn start(identity: &Identity, relay_url: &str) -> Self {
        let mut seed = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut seed);
        let eph = StaticSecret::from(seed);
        let ephemeral_pub = XPublicKey::from(&eph).to_bytes();
        let code = generate_pair_code();
        Self {
            invite: PairInvite {
                v: PAIRING_VERSION,
                relay_url: relay_url.to_string(),
                device: identity.public(),
                ephemeral_pub,
                created_at_ms: now_ms(),
            },
            code,
            eph,
            peer_device: None,
            peer_ephemeral: None,
            shared: None,
        }
    }

    /// 服务端用于匹配的配对码哈希（不泄漏配对码明文）。
    pub fn code_hash(&self) -> String {
        blake3_hex(self.code.as_bytes())[..16].to_string()
    }

    /// 把邀请 + 配对码编码为二维码字符串（供 B 扫描，走带外信道）。
    pub fn qr_string(&self) -> Result<String> {
        encode_qr(&self.invite, &self.code)
    }

    /// A 端收到 B 的加入信息后调用。
    pub fn handle_join(&mut self, join: &PairJoinPayload) -> Result<()> {
        join.validate()?;
        if join.device.device_id == self.invite.device.device_id {
            return Err(CoreError::Verification("不能与自身配对".into()));
        }
        let transcript = pairing_transcript(
            &self.invite.device,
            &self.invite.ephemeral_pub,
            &join.device,
            &join.ephemeral_pub,
            &self.code,
        )?;
        let shared = pairing_shared(&self.eph, &join.ephemeral_pub, &transcript)?;
        self.peer_device = Some(join.device.clone());
        self.peer_ephemeral = Some(join.ephemeral_pub);
        self.shared = Some(shared);
        Ok(())
    }

    /// 6 位短认证串，形如 `123 456`；必须由用户与对端显示的 SAS 比对。
    pub fn sas(&self) -> Option<String> {
        self.shared.map(|k| sas_from_key(&k))
    }

    pub fn finish(&self) -> Result<PairRecord> {
        let peer = self
            .peer_device
            .clone()
            .ok_or_else(|| CoreError::Protocol("尚未收到对端加入信息".into()))?;
        let shared = self
            .shared
            .ok_or_else(|| CoreError::Protocol("配对密钥尚未派生".into()))?;
        Ok(PairRecord {
            v: PAIRING_VERSION,
            label: peer.name.clone(),
            peer,
            relay_url: self.invite.relay_url.clone(),
            paired_at_ms: now_ms(),
            pairing_fingerprint: blake3_hex(&shared)[..16].to_string(),
        })
    }
}

/// B 端配对会话。
pub struct PairingResponder {
    pub invite: PairInvite,
    pub code: String,
    pub join: PairJoinPayload,
    shared: [u8; 32],
    sas: String,
}

impl PairingResponder {
    /// 由邀请（扫码获得）与配对码创建。
    pub fn accept(identity: &Identity, invite: &PairInvite, code: &str) -> Result<Self> {
        invite.validate()?;
        if code.len() != PAIR_CODE_LEN || !code.chars().all(|c| c.is_ascii_digit()) {
            return Err(CoreError::Invalid("配对码必须是 8 位数字".into()));
        }
        if identity.device_id() == invite.device.device_id {
            return Err(CoreError::Verification("不能与自身配对".into()));
        }
        let mut seed = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut seed);
        let eph = StaticSecret::from(seed);
        let join = PairJoinPayload {
            v: PAIRING_VERSION,
            device: identity.public(),
            ephemeral_pub: XPublicKey::from(&eph).to_bytes(),
            created_at_ms: now_ms(),
        };
        let transcript = pairing_transcript(
            &invite.device,
            &invite.ephemeral_pub,
            &join.device,
            &join.ephemeral_pub,
            code,
        )?;
        let shared = pairing_shared(&eph, &invite.ephemeral_pub, &transcript)?;
        let sas = sas_from_key(&shared);
        Ok(Self {
            invite: invite.clone(),
            code: code.to_string(),
            join,
            shared,
            sas,
        })
    }

    pub fn code_hash(&self) -> String {
        blake3_hex(self.code.as_bytes())[..16].to_string()
    }

    pub fn sas(&self) -> &str {
        &self.sas
    }

    pub fn finish(&self) -> Result<PairRecord> {
        Ok(PairRecord {
            v: PAIRING_VERSION,
            label: self.invite.device.name.clone(),
            peer: self.invite.device.clone(),
            relay_url: self.invite.relay_url.clone(),
            paired_at_ms: now_ms(),
            pairing_fingerprint: blake3_hex(&self.shared)[..16].to_string(),
        })
    }
}

/// 生成 8 位数字配对码。
pub fn generate_pair_code() -> String {
    let bytes = random_bytes(4);
    let n = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) % 100_000_000;
    format!("{n:08}")
}

// ---------------------------------------------------------------------------
// 内部：transcript / 密钥派生 / SAS
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PairSide {
    device_id: String,
    name: String,
    ed_public_hex: String,
    x_public_hex: String,
    ephemeral_pub_hex: String,
}

impl PairSide {
    fn new(device: &PublicIdentity, eph: &[u8; 32]) -> Self {
        Self {
            device_id: device.device_id.clone(),
            name: device.name.clone(),
            ed_public_hex: hex::encode(device.ed_public),
            x_public_hex: hex::encode(device.x_public),
            ephemeral_pub_hex: hex::encode(eph),
        }
    }
}

#[derive(Serialize)]
struct PairTranscript {
    v: u32,
    code_hash: String,
    sides: Vec<PairSide>,
}

fn pairing_transcript(
    a: &PublicIdentity,
    a_eph: &[u8; 32],
    b: &PublicIdentity,
    b_eph: &[u8; 32],
    code: &str,
) -> Result<Vec<u8>> {
    let mut sides = vec![PairSide::new(a, a_eph), PairSide::new(b, b_eph)];
    // 按 device_id 排序，保证双方构造的 transcript 字节完全一致。
    sides.sort_by(|x, y| x.device_id.cmp(&y.device_id));
    let transcript = PairTranscript {
        v: PAIRING_VERSION,
        code_hash: blake3_hex(code.as_bytes()),
        sides,
    };
    Ok(canonical_json(&transcript)?.into_bytes())
}

fn pairing_shared(
    eph: &StaticSecret,
    peer_eph: &[u8; 32],
    transcript: &[u8],
) -> Result<[u8; 32]> {
    let dh = eph
        .diffie_hellman(&XPublicKey::from(*peer_eph))
        .to_bytes();
    let salt = blake3::hash(transcript);
    let okm = hkdf_expand(
        &dh,
        salt.as_bytes(),
        b"drive-sync-pairing-key-v1",
        crate::crypto::KEY_LEN,
    )?;
    let mut key = [0u8; 32];
    key.copy_from_slice(&okm);
    Ok(key)
}

fn sas_from_key(key: &[u8; 32]) -> String {
    let okm = hkdf_expand(key, b"drive-sync-sas-v1", b"sas", 4).unwrap_or_else(|_| vec![0u8; 4]);
    let n = u32::from_be_bytes([okm[0], okm[1], okm[2], okm[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identities() -> (Identity, Identity) {
        (Identity::generate("笔记本"), Identity::generate("手机"))
    }

    #[test]
    fn pairing_produces_same_sas_and_record() {
        let (a_id, b_id) = identities();
        let mut initiator = PairingInitiator::start(&a_id, "ws://127.0.0.1:8787");
        let qr = initiator.qr_string().unwrap();
        let (invite, code) = decode_qr(&qr).unwrap();
        assert_eq!(code, initiator.code);

        let responder = PairingResponder::accept(&b_id, &invite, &code).unwrap();
        initiator.handle_join(&responder.join).unwrap();

        assert_eq!(
            initiator.sas().unwrap(),
            responder.sas(),
            "两端 SAS 必须一致"
        );
        assert_eq!(initiator.code_hash(), responder.code_hash());

        let ra = initiator.finish().unwrap();
        let rb = responder.finish().unwrap();
        assert_eq!(ra.pairing_fingerprint, rb.pairing_fingerprint);
        assert_eq!(ra.peer.device_id, b_id.device_id());
        assert_eq!(rb.peer.device_id, a_id.device_id());
    }

    #[test]
    fn wrong_code_breaks_sas_agreement() {
        let (a_id, b_id) = identities();
        let mut initiator = PairingInitiator::start(&a_id, "ws://127.0.0.1:8787");
        let (invite, code) = decode_qr(&initiator.qr_string().unwrap()).unwrap();
        let wrong = if code == "00000000" { "00000001" } else { "00000000" };
        let responder = PairingResponder::accept(&b_id, &invite, wrong).unwrap();
        initiator.handle_join(&responder.join).unwrap();
        assert_ne!(initiator.sas().unwrap(), responder.sas());
        assert_ne!(
            initiator.finish().unwrap().pairing_fingerprint,
            responder.finish().unwrap().pairing_fingerprint
        );
    }

    #[test]
    fn cannot_pair_with_self() {
        let a = Identity::generate("a");
        let invite = PairInvite {
            v: PAIRING_VERSION,
            relay_url: "ws://x".into(),
            device: a.public(),
            ephemeral_pub: [7u8; 32],
            created_at_ms: 1,
        };
        assert!(PairingResponder::accept(&a, &invite, "12345678").is_err());
    }

    #[test]
    fn qr_roundtrip() {
        let a = Identity::generate("a");
        let inv = PairingInitiator::start(&a, "ws://127.0.0.1:1");
        let qr = inv.qr_string().unwrap();
        let (invite, code) = decode_qr(&qr).unwrap();
        assert_eq!(invite.device.device_id, a.device_id());
        assert_eq!(code, inv.code);
        assert!(decode_qr("garbage").is_err());
    }
}
