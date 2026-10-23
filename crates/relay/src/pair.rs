//! 配对路由（协议 §4.3 / §5）。
//!
//! 中继只保存 `code_hash → (announcer, invite, expires_at)` 登记：
//! - TTL 默认 120s（可配置，且不超过配置上限）；
//! - 一次性消费：成功加入后立即删除；任何失败的 join 也会销毁对应登记（防暴力猜码）；
//! - 每设备 join 失败限流：默认 5 次/分钟，超限后拒绝并回 `pair_result{ok:false}`；
//! - 中继看不到配对码明文，也不参与 SAS 计算，只把 invite/join 在两端之间互转。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use drive_core::pairing::{PairInvite, PairJoinPayload};
use drive_core::wire::ServerToClient;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::state::{try_send_error, try_send_json, OutMsg, RelayState};

/// `code_hash = hex(BLAKE3(8 位配对码))[..16]`，即 16 个 hex 字符。
pub(crate) fn is_valid_code_hash(code_hash: &str) -> bool {
    code_hash.len() == 16 && code_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) struct PairRegistration {
    pub(crate) announcer: String,
    pub(crate) invite: PairInvite,
    pub(crate) expires_at: Instant,
}

/// 配对登记表（`code_hash` 索引 + 每设备至多一条未消费登记）。
pub(crate) struct PairRegistry {
    ttl: Duration,
    by_code: HashMap<String, PairRegistration>,
    by_device: HashMap<String, String>,
}

impl PairRegistry {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            by_code: HashMap::new(),
            by_device: HashMap::new(),
        }
    }

    fn remove_entry(&mut self, code_hash: &str) -> Option<PairRegistration> {
        let removed = self.by_code.remove(code_hash);
        if let Some(registration) = &removed {
            let owns_mapping = self
                .by_device
                .get(&registration.announcer)
                .map(|mapped| mapped == code_hash)
                .unwrap_or(false);
            if owns_mapping {
                self.by_device.remove(&registration.announcer);
            }
        }
        removed
    }

    /// 登记/覆盖一条邀请。同一设备重复 announce 会先移除旧登记；
    /// 不同设备使用相同 code_hash 时（概率极低的碰撞）以新登记为准。
    pub(crate) fn announce(
        &mut self,
        announcer: &str,
        code_hash: String,
        invite: PairInvite,
        ttl: Duration,
    ) {
        let ttl = ttl.min(self.ttl);
        if let Some(previous) = self.by_device.get(announcer).cloned() {
            if previous != code_hash {
                self.remove_entry(&previous);
            }
        }
        let conflicting_announcer = self
            .by_code
            .get(&code_hash)
            .map(|registration| registration.announcer.clone());
        if let Some(other) = conflicting_announcer {
            if other != announcer {
                self.by_device.remove(&other);
            }
        }
        self.by_device
            .insert(announcer.to_string(), code_hash.clone());
        self.by_code.insert(
            code_hash,
            PairRegistration {
                announcer: announcer.to_string(),
                invite,
                expires_at: Instant::now() + ttl,
            },
        );
    }

    /// 仅查看（不消费）有效登记；顺手清理过期条目。
    pub(crate) fn lookup(&mut self, code_hash: &str) -> Option<(String, PairInvite)> {
        let now = Instant::now();
        match self.by_code.get(code_hash) {
            Some(registration) if registration.expires_at > now => Some((
                registration.announcer.clone(),
                registration.invite.clone(),
            )),
            Some(_) => {
                self.remove_entry(code_hash);
                None
            }
            None => None,
        }
    }

    /// 取出并消费有效登记（一次性）。
    pub(crate) fn take_if_valid(&mut self, code_hash: &str) -> Option<PairRegistration> {
        let valid = self
            .by_code
            .get(code_hash)
            .map(|registration| registration.expires_at > Instant::now())
            .unwrap_or(false);
        if valid {
            self.remove_entry(code_hash)
        } else {
            None
        }
    }

    /// 销毁登记（失败的 join 尝试 / 发起方主动取消时调用）。
    pub(crate) fn destroy(&mut self, code_hash: &str) {
        self.remove_entry(code_hash);
    }

    /// 后台清理过期登记，返回清理数量。
    pub(crate) fn reap(&mut self, now: Instant) -> usize {
        let expired: Vec<String> = self
            .by_code
            .iter()
            .filter(|(_, registration)| registration.expires_at <= now)
            .map(|(code_hash, _)| code_hash.clone())
            .collect();
        let count = expired.len();
        for code_hash in expired {
            self.remove_entry(&code_hash);
        }
        count
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_code.len()
    }
}

fn send_pair_result(
    out_tx: &mpsc::Sender<OutMsg>,
    ok: bool,
    message: &str,
    peer: Option<String>,
) -> bool {
    try_send_json(
        out_tx,
        ServerToClient::PairResult {
            ok,
            message: message.to_string(),
            peer,
        },
    )
}

/// 处理 `pair_announce`：校验邀请、登记 code_hash（TTL 有上限）。
pub(crate) async fn handle_announce(
    state: &RelayState,
    device_id: &str,
    out_tx: &mpsc::Sender<OutMsg>,
    code_hash: String,
    invite: PairInvite,
    ttl_secs: u64,
) {
    if !is_valid_code_hash(&code_hash) {
        let _ = try_send_error(
            out_tx,
            "protocol_error",
            "code_hash 必须是 16 个 hex 字符（hex(BLAKE3(code)) 前 16 位）",
        );
        return;
    }
    if let Err(error) = invite.validate() {
        debug!(device = %device_id, %error, "pair_announce 邀请校验失败");
        let _ = try_send_error(out_tx, "protocol_error", "配对邀请（invite）非法");
        return;
    }
    if invite.device.device_id != device_id {
        let _ = try_send_error(
            out_tx,
            "protocol_error",
            "配对邀请中的 device 与当前认证设备不一致",
        );
        return;
    }

    let max_ttl = state.config.pair_ttl_secs.max(1);
    let effective_ttl = if ttl_secs == 0 {
        max_ttl
    } else {
        ttl_secs.clamp(1, max_ttl)
    };
    let mut pairs = state.pairs.lock().await;
    pairs.announce(
        device_id,
        code_hash,
        invite,
        Duration::from_secs(effective_ttl),
    );
    drop(pairs);
    debug!(device = %device_id, ttl_secs = effective_ttl, "配对邀请登记已创建");
}

/// 处理 `pair_join`：限流 → 校验 → 一次性消费 → 互转 invite/join → 回 pair_result。
pub(crate) async fn handle_join(
    state: &RelayState,
    device_id: &str,
    out_tx: &mpsc::Sender<OutMsg>,
    code_hash: String,
    join: PairJoinPayload,
) {
    // 1) 每设备失败限流（不记录被限流的尝试，避免无限延长封禁窗口）。
    if state.join_limiter.is_limited(device_id) {
        let _ = send_pair_result(
            out_tx,
            false,
            "rate_limited: 配对尝试过于频繁，请稍后再试",
            None,
        );
        return;
    }

    // 2) 基础校验：任何失败都计入限流，并销毁对应登记（失败即销毁）。
    let join_valid = join.validate().is_ok() && join.device.device_id == device_id;
    if !is_valid_code_hash(&code_hash) || !join_valid {
        state.join_limiter.record_failure(device_id);
        let mut pairs = state.pairs.lock().await;
        pairs.destroy(&code_hash);
        drop(pairs);
        let _ = send_pair_result(out_tx, false, "protocol_error: 配对加入信息非法", None);
        return;
    }

    // 3) 查有效登记（先不消费，发起方离线时允许加入方在 TTL 内重试）。
    let mut pairs = state.pairs.lock().await;
    let registration = pairs.lookup(&code_hash);
    drop(pairs);
    let Some((announcer, invite)) = registration else {
        state.join_limiter.record_failure(device_id);
        let _ = send_pair_result(
            out_tx,
            false,
            "pair_not_found: 配对码无效、已过期或已被使用",
            None,
        );
        return;
    };

    // 4) 不能与自己配对（失败即销毁）。
    if announcer == device_id {
        state.join_limiter.record_failure(device_id);
        let mut pairs = state.pairs.lock().await;
        pairs.destroy(&code_hash);
        drop(pairs);
        let _ = send_pair_result(out_tx, false, "protocol_error: 不能与自己配对", None);
        return;
    }

    // 5) 发起方必须在线，否则本次 join 不消费登记（不计失败）。
    let Some(announcer_conn) = state.peer(&announcer).await else {
        let _ = send_pair_result(
            out_tx,
            false,
            "peer_offline: 配对发起方当前不在线，请稍后重试",
            Some(announcer),
        );
        return;
    };

    // 6) 一次性消费登记。
    let mut pairs = state.pairs.lock().await;
    let consumed = pairs.take_if_valid(&code_hash);
    drop(pairs);
    if consumed.is_none() {
        state.join_limiter.record_failure(device_id);
        let _ = send_pair_result(
            out_tx,
            false,
            "pair_not_found: 配对码无效、已过期或已被使用",
            None,
        );
        return;
    }

    // 7) 互转：给发起方 join，给加入方 invite，最后给加入方 pair_result。
    let event_for_announcer = ServerToClient::PairEvent {
        from: device_id.to_string(),
        invite: None,
        join: Some(join),
    };
    if announcer_conn
        .tx
        .try_send(OutMsg::Json(event_for_announcer))
        .is_err()
    {
        state
            .unregister_peer(&announcer_conn.device_id, &announcer_conn.session_id)
            .await;
        let _ = send_pair_result(
            out_tx,
            false,
            "peer_offline: 配对发起方当前不在线，请稍后重试",
            Some(announcer),
        );
        return;
    }

    let _ = try_send_json(
        out_tx,
        ServerToClient::PairEvent {
            from: announcer.clone(),
            invite: Some(invite),
            join: None,
        },
    );
    let _ = send_pair_result(
        out_tx,
        true,
        "配对邀请已转发，请比对 6 位 SAS 后在两端确认",
        Some(announcer.clone()),
    );
    info!(device = %device_id, peer = %announcer, "配对邀请已路由");
}

#[cfg(test)]
mod tests {
    use super::*;
    use drive_core::identity::Identity;
    use drive_core::pairing::{PairInvite, PAIRING_VERSION};

    fn invite_for(identity: &Identity) -> PairInvite {
        PairInvite {
            v: PAIRING_VERSION,
            relay_url: "ws://127.0.0.1:8787".to_string(),
            device: identity.public(),
            ephemeral_pub: [7u8; 32],
            created_at_ms: 0,
        }
    }

    #[test]
    fn code_hash_validation() {
        assert!(is_valid_code_hash("0123456789abcdef"));
        assert!(!is_valid_code_hash("0123456789abcde"));
        assert!(!is_valid_code_hash("0123456789abcdeg"));
        assert!(!is_valid_code_hash(""));
    }

    #[test]
    fn announce_and_consume_once() {
        let alice = Identity::generate("a");
        let mut registry = PairRegistry::new(Duration::from_secs(120));
        registry.announce(
            &alice.device_id(),
            "0123456789abcdef".to_string(),
            invite_for(&alice),
            Duration::from_secs(120),
        );
        assert_eq!(registry.len(), 1);
        let looked_up = registry.lookup("0123456789abcdef").expect("应能查到登记");
        assert_eq!(looked_up.0, alice.device_id());
        assert!(registry.take_if_valid("0123456789abcdef").is_some());
        assert!(registry.lookup("0123456789abcdef").is_none());
        assert!(registry.take_if_valid("0123456789abcdef").is_none());
    }

    #[test]
    fn announce_replaces_previous_code_of_same_device() {
        let alice = Identity::generate("a");
        let mut registry = PairRegistry::new(Duration::from_secs(120));
        registry.announce(
            &alice.device_id(),
            "0123456789abcdef".to_string(),
            invite_for(&alice),
            Duration::from_secs(120),
        );
        registry.announce(
            &alice.device_id(),
            "fedcba9876543210".to_string(),
            invite_for(&alice),
            Duration::from_secs(120),
        );
        assert_eq!(registry.len(), 1);
        assert!(registry.lookup("0123456789abcdef").is_none());
        assert!(registry.lookup("fedcba9876543210").is_some());
    }

    #[test]
    fn expired_registration_not_returned() {
        let alice = Identity::generate("a");
        let mut registry = PairRegistry::new(Duration::from_secs(120));
        registry.announce(
            &alice.device_id(),
            "0123456789abcdef".to_string(),
            invite_for(&alice),
            Duration::ZERO,
        );
        assert!(registry.lookup("0123456789abcdef").is_none());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn reap_removes_expired() {
        let alice = Identity::generate("a");
        let bob = Identity::generate("b");
        let mut registry = PairRegistry::new(Duration::from_secs(120));
        registry.announce(
            &alice.device_id(),
            "0123456789abcdef".to_string(),
            invite_for(&alice),
            Duration::ZERO,
        );
        registry.announce(
            &bob.device_id(),
            "fedcba9876543210".to_string(),
            invite_for(&bob),
            Duration::from_secs(120),
        );
        assert_eq!(registry.reap(Instant::now()), 1);
        assert_eq!(registry.len(), 1);
    }
}
