//! 配对"加入方"实现（B 端）。
//!
//! 为什么不用 `drive_core::pairing::PairingResponder`：协议 §4.3 要求 B 先发
//! `pair_join`（中继据此把 A 的 invite 转给 B），而 `PairingResponder::accept`
//! 需要先拿到 invite 才能构造 join，存在先有鸡还是先有蛋的问题。因此 B 端先生成
//! 临时 X25519 密钥与 join 载荷，收到 invite 后再按协议 §5 的字节级一致派生规则
//! 计算配对密钥与 SAS。派生逻辑与 core 完全一致（`pairing_joiner_matches_core` 单测
//! 会与 core 的 `PairingInitiator` 对齐验证）。

use anyhow::{bail, Result};
use drive_core::crypto::hkdf_expand;
use drive_core::identity::Identity;
use drive_core::pairing::{
    PairInvite, PairJoinPayload, PairRecord, PAIRING_VERSION, PAIR_CODE_LEN,
};
use drive_core::util::{blake3_hex, canonical_json, now_ms, random_bytes};
use serde::Serialize;
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};

/// 配对加入方会话。
pub struct PairingJoiner {
    /// 待通过中继发给 A 的加入信息（含本端临时公钥）。
    pub join: PairJoinPayload,
    pub code: String,
    invite: Option<PairInvite>,
    eph: StaticSecret,
    shared: Option<[u8; 32]>,
}

impl PairingJoiner {
    /// 用 8 位配对码创建（`--code` 手工输入模式）。此时还没有 invite。
    pub fn start(identity: &Identity, code: &str) -> Result<Self> {
        if code.len() != PAIR_CODE_LEN || !code.chars().all(|c| c.is_ascii_digit()) {
            bail!("配对码必须是 8 位数字");
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&random_bytes(32));
        let eph = StaticSecret::from(seed);
        let join = PairJoinPayload {
            v: PAIRING_VERSION,
            device: identity.public(),
            ephemeral_pub: XPublicKey::from(&eph).to_bytes(),
            created_at_ms: now_ms(),
        };
        Ok(Self {
            join,
            code: code.to_string(),
            invite: None,
            eph,
            shared: None,
        })
    }

    /// 服务端用于匹配的配对码哈希（与 core 的 `PairingInitiator::code_hash` 一致）。
    pub fn code_hash(&self) -> String {
        blake3_hex(self.code.as_bytes())[..16].to_string()
    }

    pub fn invite(&self) -> Option<&PairInvite> {
        self.invite.as_ref()
    }

    /// 收到 A 的邀请（二维码或 `pair_event`）后计算 SAS 与配对指纹。
    pub fn accept_invite(&mut self, invite: &PairInvite) -> Result<()> {
        invite
            .validate()
            .map_err(|e| anyhow::anyhow!("配对邀请非法: {e}"))?;
        if invite.device.device_id == self.join.device.device_id {
            bail!("不能与自身配对");
        }
        let transcript = pairing_transcript(
            &invite.device,
            &invite.ephemeral_pub,
            &self.join.device,
            &self.join.ephemeral_pub,
            &self.code,
        )?;
        let shared = pairing_shared(&self.eph, &invite.ephemeral_pub, &transcript)?;
        self.invite = Some(invite.clone());
        self.shared = Some(shared);
        Ok(())
    }

    /// 6 位短认证串，形如 `123 456`；必须与 A 端显示的一致。
    pub fn sas(&self) -> Option<String> {
        self.shared.map(|key| sas_from_key(&key))
    }

    /// 保守起见不保存配对密钥，只保存指纹（与 core 行为一致）。
    pub fn finish(&self) -> Result<PairRecord> {
        let invite = self
            .invite
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("尚未收到对端配对邀请"))?;
        let shared = self
            .shared
            .ok_or_else(|| anyhow::anyhow!("配对密钥尚未派生"))?;
        Ok(PairRecord {
            v: PAIRING_VERSION,
            label: invite.device.name.clone(),
            peer: invite.device.clone(),
            relay_url: invite.relay_url.clone(),
            paired_at_ms: now_ms(),
            pairing_fingerprint: blake3_hex(&shared)[..16].to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// 与 core 字节级一致的 transcript / 密钥派生（协议 §5）
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
    fn new(device: &drive_core::identity::PublicIdentity, ephemeral: &[u8; 32]) -> Self {
        Self {
            device_id: device.device_id.clone(),
            name: device.name.clone(),
            ed_public_hex: hex::encode(device.ed_public),
            x_public_hex: hex::encode(device.x_public),
            ephemeral_pub_hex: hex::encode(ephemeral),
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
    a: &drive_core::identity::PublicIdentity,
    a_ephemeral: &[u8; 32],
    b: &drive_core::identity::PublicIdentity,
    b_ephemeral: &[u8; 32],
    code: &str,
) -> Result<Vec<u8>> {
    let mut sides = vec![PairSide::new(a, a_ephemeral), PairSide::new(b, b_ephemeral)];
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
    peer_ephemeral: &[u8; 32],
    transcript: &[u8],
) -> Result<[u8; 32]> {
    let dh = eph
        .diffie_hellman(&XPublicKey::from(*peer_ephemeral))
        .to_bytes();
    let salt = blake3::hash(transcript);
    let okm = hkdf_expand(
        &dh,
        salt.as_bytes(),
        b"drive-sync-pairing-key-v1",
        drive_core::crypto::KEY_LEN,
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
    use drive_core::pairing::{decode_qr, PairingInitiator};

    #[test]
    fn pairing_joiner_matches_core() {
        let a = Identity::generate("A");
        let b = Identity::generate("B");
        let mut initiator = PairingInitiator::start(&a, "ws://127.0.0.1:8787");
        let (invite, code) = decode_qr(&initiator.qr_string().unwrap()).unwrap();

        let mut joiner = PairingJoiner::start(&b, &code).unwrap();
        assert_eq!(joiner.code_hash(), initiator.code_hash());
        joiner.accept_invite(&invite).unwrap();
        initiator.handle_join(&joiner.join).unwrap();

        assert_eq!(
            joiner.sas().unwrap(),
            initiator.sas().unwrap(),
            "两端 SAS 必须一致"
        );
        let ra = initiator.finish().unwrap();
        let rb = joiner.finish().unwrap();
        assert_eq!(ra.pairing_fingerprint, rb.pairing_fingerprint);
        assert_eq!(ra.peer.device_id, b.device_id());
        assert_eq!(rb.peer.device_id, a.device_id());
    }

    #[test]
    fn wrong_code_breaks_sas() {
        let a = Identity::generate("A");
        let b = Identity::generate("B");
        let mut initiator = PairingInitiator::start(&a, "ws://x");
        let (invite, code) = decode_qr(&initiator.qr_string().unwrap()).unwrap();
        let wrong = if code == "00000000" {
            "00000001"
        } else {
            "00000000"
        };
        let mut joiner = PairingJoiner::start(&b, wrong).unwrap();
        joiner.accept_invite(&invite).unwrap();
        initiator.handle_join(&joiner.join).unwrap();
        assert_ne!(joiner.sas().unwrap(), initiator.sas().unwrap());
    }

    #[test]
    fn rejects_bad_code() {
        let b = Identity::generate("B");
        assert!(PairingJoiner::start(&b, "1234").is_err());
        assert!(PairingJoiner::start(&b, "abcdefgh").is_err());
    }
}
