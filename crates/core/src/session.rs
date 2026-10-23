//! E2E 会话：连接级前向保密（每次连接重新协商），身份由配对阶段的 Ed25519 公钥认证。
//!
//! 握手经中继转发（中继只看到公钥与签名，无法解密后续内容）：
//! - 发起方 → 对端：`SessionInitMsg`（临时 X25519 公钥 + Ed25519 签名）
//! - 对端 → 发起方：`SessionAckMsg`（临时 X25519 公钥 + 签名，覆盖 init 内容哈希）
//! - 双方用 X25519 ECDH 派生双向密钥；会话密钥只存在于内存。
//!
//! 生产升级点：加入 Double Ratchet/后量子混合（X25519+ML-KEM）、密钥轮换与丢弃策略。

use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};

use crate::crypto::{hkdf64, open, seal};
use crate::identity::{Identity, PublicIdentity};
use crate::util::{blake3_hex, canonical_json, now_ms, random_hex};
use crate::{CoreError, Result};

pub const SESSION_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInitContent {
    pub v: u32,
    pub from: String,
    pub to: String,
    pub session_id: String,
    #[serde(with = "crate::util::hex32")]
    pub ephemeral_pub: [u8; 32],
    #[serde(with = "crate::util::hex32")]
    pub ed_public: [u8; 32],
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInitMsg {
    pub content: SessionInitContent,
    #[serde(with = "crate::util::hex64")]
    pub signature: [u8; 64],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionAckContent {
    pub v: u32,
    pub session_id: String,
    pub from: String,
    pub to: String,
    #[serde(with = "crate::util::hex32")]
    pub ephemeral_pub: [u8; 32],
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionAckMsg {
    pub content: SessionAckContent,
    #[serde(with = "crate::util::hex64")]
    pub signature: [u8; 64],
}

/// 发起方握手状态（临时私钥只在内存）。
pub struct SessionInit {
    pub msg: SessionInitMsg,
    pub eph: StaticSecret,
}

/// 加密后的 E2E 信封：中继只按 `session_id`/`seq` 转发，不解析内容。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct E2eEnvelope {
    pub v: u32,
    pub session_id: String,
    pub seq: u64,
    pub kind: String,
    pub ts_ms: u64,
    #[serde(with = "crate::util::b64_vec")]
    pub ciphertext: Vec<u8>,
}

/// 已建立的 E2E 会话。
pub struct Session {
    pub peer_id: String,
    pub session_id: String,
    send_key: [u8; 32],
    recv_key: [u8; 32],
    send_seq: u64,
    recv_seq: u64,
}

impl Session {
    pub fn encrypt(&mut self, kind: &str, plaintext: &[u8]) -> Result<E2eEnvelope> {
        let seq = self.send_seq;
        self.send_seq += 1;
        let ts_ms = now_ms();
        let aad = envelope_aad(SESSION_VERSION, &self.session_id, seq, kind, ts_ms);
        let ciphertext = seal(&self.send_key, aad.as_bytes(), plaintext)?;
        Ok(E2eEnvelope {
            v: SESSION_VERSION,
            session_id: self.session_id.clone(),
            seq,
            kind: kind.to_string(),
            ts_ms,
            ciphertext,
        })
    }

    /// 解密并校验序号（严格有序，防重放）。返回 `(kind, plaintext)`。
    pub fn decrypt(&mut self, envelope: &E2eEnvelope) -> Result<(String, Vec<u8>)> {
        if envelope.v != SESSION_VERSION {
            return Err(CoreError::Protocol(format!(
                "不支持的会话信封版本: {}",
                envelope.v
            )));
        }
        if envelope.session_id != self.session_id {
            return Err(CoreError::Protocol("会话 id 不匹配".into()));
        }
        if envelope.seq != self.recv_seq {
            return Err(CoreError::Protocol(format!(
                "会话序号不连续/重放: 期望 {}, 收到 {}",
                self.recv_seq, envelope.seq
            )));
        }
        let aad = envelope_aad(
            envelope.v,
            &envelope.session_id,
            envelope.seq,
            &envelope.kind,
            envelope.ts_ms,
        );
        let plaintext = open(&self.recv_key, aad.as_bytes(), &envelope.ciphertext)?;
        self.recv_seq += 1;
        Ok((envelope.kind.clone(), plaintext))
    }

    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }

    pub fn send_seq(&self) -> u64 {
        self.send_seq
    }

    pub fn recv_seq(&self) -> u64 {
        self.recv_seq
    }
}

/// 发起方创建握手消息。
pub fn create_session_init(identity: &Identity, peer: &PublicIdentity) -> Result<SessionInit> {
    let mut seed = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut seed);
    let eph = StaticSecret::from(seed);
    let content = SessionInitContent {
        v: SESSION_VERSION,
        from: identity.device_id(),
        to: peer.device_id.clone(),
        session_id: random_hex(12),
        ephemeral_pub: XPublicKey::from(&eph).to_bytes(),
        ed_public: identity.ed_public_bytes(),
        created_at_ms: now_ms(),
    };
    let signature = identity.sign(&init_sign_input(&content)?);
    Ok(SessionInit {
        msg: SessionInitMsg { content, signature },
        eph,
    })
}

/// 接收方校验发起方握手消息（是否来自配对过的对端）。
pub fn verify_session_init(msg: &SessionInitMsg, expected_peer: &PublicIdentity) -> Result<()> {
    let c = &msg.content;
    if c.v != SESSION_VERSION {
        return Err(CoreError::Protocol("会话握手版本不匹配".into()));
    }
    if c.from != expected_peer.device_id {
        return Err(CoreError::Verification("会话发起方不是已配对设备".into()));
    }
    if c.ed_public != expected_peer.ed_public {
        return Err(CoreError::Verification("会话发起方 Ed25519 公钥不匹配".into()));
    }
    if !expected_peer.verify(&init_sign_input(c)?, &msg.signature) {
        return Err(CoreError::Verification("会话握手签名无效".into()));
    }
    Ok(())
}

/// 接收方处理握手：校验、生成响应并派生会话。
pub fn accept_session_init(
    identity: &Identity,
    init: &SessionInitMsg,
    peer: &PublicIdentity,
) -> Result<(SessionAckMsg, Session)> {
    verify_session_init(init, peer)?;
    let self_id = identity.device_id();
    if init.content.to != self_id {
        return Err(CoreError::Protocol("会话握手目标不是本机".into()));
    }
    if init.content.from == self_id {
        return Err(CoreError::Protocol("不能与自己建立会话".into()));
    }

    let mut seed = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut seed);
    let eph = StaticSecret::from(seed);
    let ack_content = SessionAckContent {
        v: SESSION_VERSION,
        session_id: init.content.session_id.clone(),
        from: self_id.clone(),
        to: init.content.from.clone(),
        ephemeral_pub: XPublicKey::from(&eph).to_bytes(),
        created_at_ms: now_ms(),
    };
    let signature = identity.sign(&ack_sign_input(&init.content, &ack_content)?);
    let ack = SessionAckMsg {
        content: ack_content,
        signature,
    };

    let shared = eph
        .diffie_hellman(&XPublicKey::from(init.content.ephemeral_pub))
        .to_bytes();
    let session = derive_session(
        &shared,
        &self_id,
        &peer.device_id,
        &transcript(&init.content, &ack.content)?,
        &init.content.session_id,
    )?;
    Ok((ack, session))
}

/// 发起方收到响应后完成握手。
pub fn complete_session(
    identity: &Identity,
    init: &SessionInit,
    ack: &SessionAckMsg,
    peer: &PublicIdentity,
) -> Result<Session> {
    let self_id = identity.device_id();
    if ack.content.v != SESSION_VERSION {
        return Err(CoreError::Protocol("会话响应版本不匹配".into()));
    }
    if ack.content.session_id != init.msg.content.session_id {
        return Err(CoreError::Protocol("会话响应 id 不匹配".into()));
    }
    if ack.content.from != peer.device_id || ack.content.to != self_id {
        return Err(CoreError::Verification("会话响应设备不匹配".into()));
    }
    if !peer.verify(
        &ack_sign_input(&init.msg.content, &ack.content)?,
        &ack.signature,
    ) {
        return Err(CoreError::Verification("会话响应签名无效".into()));
    }
    let shared = init
        .eph
        .diffie_hellman(&XPublicKey::from(ack.content.ephemeral_pub))
        .to_bytes();
    derive_session(
        &shared,
        &self_id,
        &peer.device_id,
        &transcript(&init.msg.content, &ack.content)?,
        &init.msg.content.session_id,
    )
}

// ---------------------------------------------------------------------------
// 内部
// ---------------------------------------------------------------------------

fn derive_session(
    shared: &[u8; 32],
    self_id: &str,
    peer_id: &str,
    transcript: &[u8],
    session_id: &str,
) -> Result<Session> {
    let salt = blake3::hash(transcript);
    let okm = hkdf64(shared, salt.as_bytes(), b"drive-sync-session-keys-v1")?;
    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    a.copy_from_slice(&okm[..32]);
    b.copy_from_slice(&okm[32..]);
    // 按 device_id 字典序决定发送/接收方向，双方推导一致。
    let (send_key, recv_key) = if self_id < peer_id { (a, b) } else { (b, a) };
    Ok(Session {
        peer_id: peer_id.to_string(),
        session_id: session_id.to_string(),
        send_key,
        recv_key,
        send_seq: 0,
        recv_seq: 0,
    })
}

#[derive(Serialize)]
struct TranscriptSide {
    device_id: String,
    ephemeral_pub_hex: String,
    ed_public_hex: String,
}

#[derive(Serialize)]
struct SessionTranscript {
    v: u32,
    session_id: String,
    sides: Vec<TranscriptSide>,
}

fn transcript(init: &SessionInitContent, ack: &SessionAckContent) -> Result<Vec<u8>> {
    let mut sides = vec![
        TranscriptSide {
            device_id: init.from.clone(),
            ephemeral_pub_hex: hex::encode(init.ephemeral_pub),
            ed_public_hex: hex::encode(init.ed_public),
        },
        TranscriptSide {
            device_id: ack.from.clone(),
            ephemeral_pub_hex: hex::encode(ack.ephemeral_pub),
            ed_public_hex: hex::encode(init.ed_public), // 由各自签名绑定，见 ack 签名校验
        },
    ];
    sides.sort_by(|x, y| x.device_id.cmp(&y.device_id));
    Ok(canonical_json(&SessionTranscript {
        v: SESSION_VERSION,
        session_id: init.session_id.clone(),
        sides,
    })?
    .into_bytes())
}

fn envelope_aad(v: u32, session_id: &str, seq: u64, kind: &str, ts_ms: u64) -> String {
    format!("{v}|{session_id}|{seq}|{kind}|{ts_ms}")
}

fn init_sign_input(content: &SessionInitContent) -> Result<Vec<u8>> {
    let mut buf = b"drive-sync-session-init-v1".to_vec();
    buf.extend_from_slice(canonical_json(content)?.as_bytes());
    Ok(buf)
}

fn ack_sign_input(init: &SessionInitContent, ack: &SessionAckContent) -> Result<Vec<u8>> {
    let mut buf = b"drive-sync-session-ack-v1".to_vec();
    buf.extend_from_slice(blake3::hash(canonical_json(init)?.as_bytes()).as_bytes());
    buf.extend_from_slice(canonical_json(ack)?.as_bytes());
    Ok(buf)
}

/// 会话握手消息的摘要（日志用）。
pub fn session_init_summary(msg: &SessionInitMsg) -> String {
    format!(
        "{} → {} ({})",
        msg.content.from,
        msg.content.to,
        &msg.content.session_id
    )
}

/// 生成仅供测试/调试用的会话指纹。
pub fn session_fingerprint(session: &Session) -> String {
    blake3_hex(format!("{}/{}", session.session_id, session.peer_id).as_bytes())[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::{PairRecord, PAIRING_VERSION};

    fn paired_identities() -> (Identity, Identity, PairRecord, PairRecord) {
        let a = Identity::generate("A");
        let b = Identity::generate("B");
        let rec_a = PairRecord {
            v: PAIRING_VERSION,
            peer: b.public(),
            relay_url: "ws://x".into(),
            paired_at_ms: 1,
            label: "B".into(),
            pairing_fingerprint: "f".into(),
        };
        let rec_b = PairRecord {
            v: PAIRING_VERSION,
            peer: a.public(),
            relay_url: "ws://x".into(),
            paired_at_ms: 1,
            label: "A".into(),
            pairing_fingerprint: "f".into(),
        };
        (a, b, rec_a, rec_b)
    }

    #[test]
    fn handshake_and_encrypted_roundtrip() {
        let (a, b, rec_a, rec_b) = paired_identities();
        let init = create_session_init(&a, &rec_a.peer).unwrap();
        let (ack, mut session_b) = accept_session_init(&b, &init.msg, &rec_b.peer).unwrap();
        let mut session_a = complete_session(&a, &init, &ack, &rec_a.peer).unwrap();

        assert_eq!(session_a.session_id, session_b.session_id);
        let env = session_a.encrypt("clipboard", br#"{"hello":1}"#).unwrap();
        let (kind, plain) = session_b.decrypt(&env).unwrap();
        assert_eq!(kind, "clipboard");
        assert_eq!(plain, br#"{"hello":1}"#);

        let env2 = session_b.encrypt("clipboard", b"pong").unwrap();
        let (_, plain2) = session_a.decrypt(&env2).unwrap();
        assert_eq!(plain2, b"pong");
    }

    #[test]
    fn replay_and_tamper_rejected() {
        let (a, b, rec_a, rec_b) = paired_identities();
        let init = create_session_init(&a, &rec_a.peer).unwrap();
        let (ack, mut session_b) = accept_session_init(&b, &init.msg, &rec_b.peer).unwrap();
        let mut session_a = complete_session(&a, &init, &ack, &rec_a.peer).unwrap();
        let env = session_a.encrypt("x", b"data").unwrap();
        let mut replay = env.clone();
        session_b.decrypt(&env).unwrap();
        replay.seq = 0;
        assert!(session_b.decrypt(&replay).is_err());

        let mut tampered = session_a.encrypt("x", b"data2").unwrap();
        tampered.ciphertext[0] ^= 1;
        assert!(session_b.decrypt(&tampered).is_err());
    }

    #[test]
    fn signed_with_wrong_identity_rejected() {
        let (a, b, _rec_a, rec_b) = paired_identities();
        let init = create_session_init(&a, &b.public()).unwrap();
        // b 期望的对端是 a，但用错误 Peer 校验应失败
        let imposter = Identity::generate("imposter");
        assert!(verify_session_init(&init.msg, &imposter.public()).is_err());
        assert!(verify_session_init(&init.msg, &rec_b.peer).is_ok());
    }
}
