//! 离线撤销通知信箱的**协议级** e2e（不依赖 client Runtime，task-13 非 client 部分）。
//!
//! 覆盖（真实 WS 客户端 + 进程内中继，`RelayConfig.data_dir` 指向每个测试的临时目录）：
//! 1. B 离线：A `revocation_put` → 中继落盘（`<data_dir>/revocations/index.json`）→
//!    B 登录后 `welcome → pending_revocations → presence` 顺序收到 → 验签通过 →
//!    `revocation_ack` → 索引删除 → 再登录不再收到；
//! 2. B 在线：A 投递后 B **立即**收到 `pending_revocations`；未 ack 前重复登录会重复投递
//!    （幂等设计），ack 后消失；
//! 3. 安全边界：非目标设备收不到通知、其 ack 不会删除；篡改签名 / from 与认证设备不一致 /
//!    过期通知 / 超大通知均被拒（`revocation_invalid`、`payload_too_large`）；
//! 4. 中继用**同一 data_dir** 重启后 pending 仍在（持久化）；
//! 5. 启动清理：停机后把索引里的 `expires_at_ms` 拨到过去 → 重启中继后该通知被清理、不再投递。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use drive_core::identity::Identity;
use drive_core::revocation::{reason, RevocationNotice, REVOCATION_TTL_SECS};
use drive_core::util::now_ms;
use drive_core::wire::{ClientToServer, ServerToClient};
use drive_e2e::harness::{
    relay_config_with_data_dir, restart_relay_on_with_data_dir, start_relay, RelayClient,
    DEFAULT_TOKEN,
};
use tempfile::TempDir;

const RECV_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// 辅助：状态目录 / 索引 / wire 交互
// ---------------------------------------------------------------------------

fn state_dir(root: &TempDir) -> PathBuf {
    root.path().join("relay-state")
}

fn index_path(state: &Path) -> PathBuf {
    state.join("revocations").join("index.json")
}

/// 读取索引中所有 notice_id（文件不存在 = 空信箱）。
fn index_notice_ids(state: &Path) -> Result<Vec<String>> {
    let path = index_path(state);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("读取撤销索引失败: {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let stored: Vec<serde_json::Value> =
        serde_json::from_str(&raw).context("解析撤销索引 JSON 失败")?;
    Ok(stored
        .iter()
        .filter_map(|item| {
            item["notice"]["content"]["notice_id"]
                .as_str()
                .map(str::to_string)
        })
        .collect())
}

/// 轮询等待索引满足条件。
async fn wait_index<F>(state: &Path, timeout: Duration, mut predicate: F) -> Result<()>
where
    F: FnMut(&[String]) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let ids = index_notice_ids(state)?;
        if predicate(&ids) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("等待撤销索引状态超时（{timeout:?}），当前: {ids:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 投递一条撤销通知并等待 `revocation_put_result`。
async fn put_revocation(
    client: &mut RelayClient,
    notice: RevocationNotice,
) -> Result<(bool, String, String)> {
    client
        .send(&ClientToServer::RevocationPut { notice })
        .await?;
    loop {
        match client.recv(RECV_TIMEOUT).await? {
            ServerToClient::RevocationPutResult {
                ok,
                notice_id,
                message,
            } => return Ok((ok, notice_id, message)),
            ServerToClient::Presence { .. }
            | ServerToClient::Welcome { .. }
            | ServerToClient::Pong { .. }
            | ServerToClient::PendingRevocations { .. } => continue,
            other => bail!("等待 revocation_put_result 时收到意外消息: {other:?}"),
        }
    }
}

/// 登录后收取 pending 通知：按协议顺序应为 `welcome → pending_revocations? → presence`，
/// 本函数读到 `presence` 为止（因此同时验证了 pending 在 presence 之前到达）。
async fn collect_pending_on_login(client: &mut RelayClient) -> Result<Vec<RevocationNotice>> {
    let mut notices = Vec::new();
    loop {
        match client.recv(RECV_TIMEOUT).await? {
            ServerToClient::PendingRevocations { notices: batch } => notices.extend(batch),
            ServerToClient::Presence { .. } => return Ok(notices),
            ServerToClient::Welcome { .. } | ServerToClient::Pong { .. } => continue,
            other => bail!("登录后等待 pending/presence 时收到意外消息: {other:?}"),
        }
    }
}

/// 已在线设备等待一次 `pending_revocations` 推送。
async fn recv_pending_push(client: &mut RelayClient) -> Result<Vec<RevocationNotice>> {
    loop {
        match client.recv(RECV_TIMEOUT).await? {
            ServerToClient::PendingRevocations { notices } => return Ok(notices),
            ServerToClient::Presence { .. }
            | ServerToClient::Welcome { .. }
            | ServerToClient::Pong { .. } => continue,
            other => bail!("等待 pending_revocations 推送时收到意外消息: {other:?}"),
        }
    }
}

/// 断言在窗口内没有收到 `pending_revocations` 推送（presence 等其它消息忽略）。
async fn assert_no_pending_push(client: &mut RelayClient, window: Duration) -> Result<()> {
    match tokio::time::timeout(window, client.recv(window + Duration::from_secs(1))).await {
        Err(_) => Ok(()), // 窗口内无任何消息 = 没有推送
        Ok(Ok(ServerToClient::PendingRevocations { notices })) => {
            bail!("非目标设备不应收到 pending_revocations: {notices:?}")
        }
        Ok(Ok(_)) => Ok(()), // presence/pong 等非通知消息不构成"收到推送"
        Ok(Err(error)) => Err(error),
    }
}

async fn ack(client: &mut RelayClient, notice_id: &str) -> Result<()> {
    client
        .send(&ClientToServer::RevocationAck {
            notice_id: notice_id.to_string(),
        })
        .await?;
    Ok(())
}

fn assert_notice_verifies(
    notice: &RevocationNotice,
    from: &Identity,
    to_device_id: &str,
) -> Result<()> {
    notice
        .verify_target(to_device_id)
        .context("通知目标不是接收方")?;
    notice
        .verify(&from.public(), now_ms())
        .context("通知签名校验失败")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 1) B 离线：put → 落盘 → B 登录收到 → ack → 索引删除 → 再登录无
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_revocation_mailbox_deliver_ack_and_purge() -> Result<()> {
    let root = TempDir::new()?;
    let state = state_dir(&root);
    let relay = start_relay(relay_config_with_data_dir(None, state.clone())).await?;
    let url = relay.ws_url();

    let a_identity = Identity::generate("信箱A");
    let b_identity = Identity::generate("信箱B");
    let b_id = b_identity.device_id();

    // A 在线投递，B 离线。通知 90 天有效。
    let mut a = RelayClient::connect(&url, a_identity.clone(), DEFAULT_TOKEN).await?;
    let notice = RevocationNotice::create(&a_identity, &b_id, reason::USER_REMOVED, now_ms())?;
    let notice_id = notice.notice_id().to_string();
    let (ok, returned_id, message) = put_revocation(&mut a, notice.clone()).await?;
    assert!(ok, "离线目标的通知应入信箱成功: {message}");
    assert_eq!(returned_id, notice_id);
    assert_eq!(message, "queued", "首次投递应返回 queued");
    assert!(
        index_notice_ids(&state)?.contains(&notice_id),
        "通知应已落盘到 {}",
        index_path(&state).display()
    );

    // B 登录：welcome → pending_revocations → presence。
    let mut b = RelayClient::connect(&url, b_identity.clone(), DEFAULT_TOKEN).await?;
    let pending = collect_pending_on_login(&mut b).await?;
    assert_eq!(pending.len(), 1, "B 登录应收到 1 条 pending 通知");
    assert_eq!(pending[0].notice_id(), notice_id);
    assert_notice_verifies(&pending[0], &a_identity, &b_id)?;

    // ack → 中继删除索引条目。
    ack(&mut b, &notice_id).await?;
    wait_index(&state, Duration::from_secs(5), |ids| {
        !ids.contains(&notice_id)
    })
    .await?;

    // B 重新登录（同 device_id 踢旧连接）→ 无 pending。
    let mut b2 = RelayClient::connect(&url, b_identity.clone(), DEFAULT_TOKEN).await?;
    let pending = collect_pending_on_login(&mut b2).await?;
    assert!(pending.is_empty(), "ack 后不再投递，实际: {pending:?}");

    a.close().await;
    b.close().await;
    b2.close().await;
    relay.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// 2) B 在线 → 立即推送；未 ack 重复登录重复投递；ack 后消失
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn online_push_duplicate_until_ack() -> Result<()> {
    let root = TempDir::new()?;
    let state = state_dir(&root);
    let relay = start_relay(relay_config_with_data_dir(None, state.clone())).await?;
    let url = relay.ws_url();

    let a_identity = Identity::generate("在线A");
    let b_identity = Identity::generate("在线B");
    let b_id = b_identity.device_id();

    let mut a = RelayClient::connect(&url, a_identity.clone(), DEFAULT_TOKEN).await?;
    let mut b = RelayClient::connect(&url, b_identity.clone(), DEFAULT_TOKEN).await?;
    assert!(
        collect_pending_on_login(&mut b).await?.is_empty(),
        "投递前 B 无 pending"
    );

    let notice = RevocationNotice::create(&a_identity, &b_id, reason::USER_REMOVED, now_ms())?;
    let notice_id = notice.notice_id().to_string();
    let (ok, _, message) = put_revocation(&mut a, notice.clone()).await?;
    assert!(ok, "在线目标投递应成功: {message}");

    // 在线立即推送。
    let pushed = recv_pending_push(&mut b).await?;
    assert_eq!(pushed.len(), 1);
    assert_eq!(pushed[0].notice_id(), notice_id);

    // 未 ack：重复 put（同 notice_id/同内容）返回 duplicate，且不重复入信箱。
    let (ok_dup, _, message_dup) = put_revocation(&mut a, notice.clone()).await?;
    assert!(ok_dup, "重复投递应幂等成功");
    assert_eq!(message_dup, "duplicate");
    assert_eq!(
        index_notice_ids(&state)?
            .iter()
            .filter(|id| id.as_str() == notice_id.as_str())
            .count(),
        1,
        "重复投递不应产生重复条目"
    );

    // 未 ack 前重连，仍会收到（重复投递是幂等设计）。
    let mut b2 = RelayClient::connect(&url, b_identity.clone(), DEFAULT_TOKEN).await?;
    let pending = collect_pending_on_login(&mut b2).await?;
    assert_eq!(pending.len(), 1, "ack 前重连应重复收到通知");
    assert_eq!(pending[0].notice_id(), notice_id);

    // ack 后消失。
    ack(&mut b2, &notice_id).await?;
    wait_index(&state, Duration::from_secs(5), |ids| {
        !ids.contains(&notice_id)
    })
    .await?;
    let mut b3 = RelayClient::connect(&url, b_identity.clone(), DEFAULT_TOKEN).await?;
    assert!(
        collect_pending_on_login(&mut b3).await?.is_empty(),
        "ack 后重连不再收到"
    );

    a.close().await;
    b2.close().await;
    b3.close().await;
    relay.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// 3) 安全边界：非目标 ack 不删除；伪造/过期/超大被拒
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_target_ack_and_invalid_puts_rejected() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let root = TempDir::new()?;
    let state = state_dir(&root);
    let relay = start_relay(relay_config_with_data_dir(None, state.clone())).await?;
    let url = relay.ws_url();

    let a_identity = Identity::generate("安全A");
    let b_identity = Identity::generate("安全B");
    let c_identity = Identity::generate("安全C");
    let b_id = b_identity.device_id();

    let mut a = RelayClient::connect(&url, a_identity.clone(), DEFAULT_TOKEN).await?;
    let mut c = RelayClient::connect(&url, c_identity.clone(), DEFAULT_TOKEN).await?;
    assert!(collect_pending_on_login(&mut c).await?.is_empty());

    // A→B 合法通知（B 离线）。
    let notice = RevocationNotice::create(&a_identity, &b_id, reason::USER_REMOVED, now_ms())?;
    let notice_id = notice.notice_id().to_string();
    assert!(put_revocation(&mut a, notice.clone()).await?.0);

    // C 不是目标：登录阶段已确认收不到；后续也不会收到推送；C 的 ack 被忽略，不删除。
    assert_no_pending_push(&mut c, Duration::from_millis(800)).await?;
    ack(&mut c, &notice_id).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        index_notice_ids(&state)?.contains(&notice_id),
        "非目标设备的 ack 不得删除通知"
    );

    // 3a) 篡改签名 → revocation_invalid。
    let mut tampered = notice.clone();
    tampered.signature[0] ^= 1;
    let (ok, _, message) = put_revocation(&mut a, tampered).await?;
    assert!(!ok, "篡改签名必须被拒");
    assert!(
        message.starts_with("revocation_invalid"),
        "期望 revocation_invalid，实际: {message}"
    );

    // 3b) from 与当前认证设备不一致（用 A 的合法通知让 C 发送）→ 拒绝且不入信箱。
    let cross = RevocationNotice::create(&a_identity, &b_id, reason::USER_REMOVED, now_ms())?;
    let cross_id = cross.notice_id().to_string();
    let (ok, _, message) = put_revocation(&mut c, cross).await?;
    assert!(!ok, "第三方转发他人通知必须被拒");
    assert!(message.starts_with("revocation_invalid"), "实际: {message}");
    assert!(!index_notice_ids(&state)?.contains(&cross_id));

    // 3c) 过期通知（created/expires 均为 90 天+T 之前）→ 拒绝。
    let expired_at = now_ms().saturating_sub(REVOCATION_TTL_SECS * 1000 + 60_000);
    let expired = RevocationNotice::create(&a_identity, &b_id, reason::USER_REMOVED, expired_at)?;
    let (ok, _, message) = put_revocation(&mut a, expired).await?;
    assert!(!ok, "过期通知必须被拒");
    assert!(message.starts_with("revocation_invalid"), "实际: {message}");

    // 3d) 超过单条 8KiB（超长 reason）→ payload_too_large。
    let huge_reason = "x".repeat(9 * 1024);
    let oversize = RevocationNotice::create(&a_identity, &b_id, &huge_reason, now_ms())?;
    let (ok, _, message) = put_revocation(&mut a, oversize).await?;
    assert!(!ok, "超大通知必须被拒");
    assert!(
        message.starts_with("payload_too_large"),
        "期望 payload_too_large，实际: {message}"
    );

    // 合法通知仍在，B 上线 can ack 清理。
    let mut b = RelayClient::connect(&url, b_identity.clone(), DEFAULT_TOKEN).await?;
    let pending = collect_pending_on_login(&mut b).await?;
    assert!(
        pending.iter().any(|n| n.notice_id() == notice_id),
        "合法通知应仍可投递"
    );
    assert_notice_verifies(&pending[0], &a_identity, &b_id)?;
    ack(&mut b, &notice_id).await?;
    wait_index(&state, Duration::from_secs(5), |ids| {
        !ids.contains(&notice_id)
    })
    .await?;

    a.close().await;
    b.close().await;
    c.close().await;
    relay.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// 4) 中继用同一 data_dir 重启后 pending 仍在
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_restart_preserves_pending_mailbox() -> Result<()> {
    let root = TempDir::new()?;
    let state = state_dir(&root);
    let relay = start_relay(relay_config_with_data_dir(None, state.clone())).await?;
    let address = relay.addr;
    let url = relay.ws_url();

    let a_identity = Identity::generate("重启A");
    let b_identity = Identity::generate("重启B");
    let b_id = b_identity.device_id();

    let mut a = RelayClient::connect(&url, a_identity.clone(), DEFAULT_TOKEN).await?;
    let notice = RevocationNotice::create(&a_identity, &b_id, reason::USER_REMOVED, now_ms())?;
    let notice_id = notice.notice_id().to_string();
    assert!(put_revocation(&mut a, notice).await?.0);
    a.close().await;

    // 中继重启（同地址、同状态目录）。
    relay.shutdown().await;
    let relay = restart_relay_on_with_data_dir(address, state.clone()).await?;
    assert!(
        index_notice_ids(&state)?.contains(&notice_id),
        "重启后索引仍应包含 pending 通知"
    );

    let mut b = RelayClient::connect(&relay.ws_url(), b_identity.clone(), DEFAULT_TOKEN).await?;
    let pending = collect_pending_on_login(&mut b).await?;
    assert_eq!(pending.len(), 1, "重启后 B 登录仍应收到通知");
    assert_eq!(pending[0].notice_id(), notice_id);

    ack(&mut b, &notice_id).await?;
    wait_index(&state, Duration::from_secs(5), |ids| {
        !ids.contains(&notice_id)
    })
    .await?;

    b.close().await;
    relay.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// 5) 启动清理：过期通知在重启时被删除、不再投递
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_cleanup_removes_expired_notices() -> Result<()> {
    let root = TempDir::new()?;
    let state = state_dir(&root);
    let relay = start_relay(relay_config_with_data_dir(None, state.clone())).await?;
    let address = relay.addr;
    let url = relay.ws_url();

    let a_identity = Identity::generate("过期A");
    let b_identity = Identity::generate("过期B");
    let b_id = b_identity.device_id();

    let mut a = RelayClient::connect(&url, a_identity.clone(), DEFAULT_TOKEN).await?;
    let notice = RevocationNotice::create(&a_identity, &b_id, reason::USER_REMOVED, now_ms())?;
    let notice_id = notice.notice_id().to_string();
    assert!(put_revocation(&mut a, notice).await?.0);
    a.close().await;
    assert!(index_notice_ids(&state)?.contains(&notice_id));
    relay.shutdown().await;

    // 白盒"时间旅行"：协议无法构造短 TTL 通知（core 固定 90 天签名内容），
    // 停机后把索引里的 expires_at_ms 拨到过去，验证中继启动清理路径。
    let path = index_path(&state);
    let raw = std::fs::read_to_string(&path)?;
    let mut stored: Vec<serde_json::Value> = serde_json::from_str(&raw)?;
    for item in stored.iter_mut() {
        item["notice"]["content"]["expires_at_ms"] = serde_json::json!(now_ms() - 1000);
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&stored)?)?;

    let relay = restart_relay_on_with_data_dir(address, state.clone()).await?;
    assert!(
        !index_notice_ids(&state)?.contains(&notice_id),
        "启动清理应删除过期通知"
    );
    let mut b = RelayClient::connect(&relay.ws_url(), b_identity.clone(), DEFAULT_TOKEN).await?;
    assert!(
        collect_pending_on_login(&mut b).await?.is_empty(),
        "过期通知不应再投递"
    );

    b.close().await;
    relay.shutdown().await;
    Ok(())
}
