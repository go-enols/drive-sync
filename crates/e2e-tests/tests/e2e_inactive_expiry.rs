//! v0.2 配对生命周期 e2e：90 天不活跃自动过期 + 边界（task-13）。
//!
//! 规则（core `PairRecord::inactivity_expired` / `INACTIVE_PAIR_EXPIRY_MS`）：
//! - `last_seen_ms`（成功 E2E 会话/收发消息/心跳）超过 **90 天** → 引擎自动删除配对记录、
//!   生成 `inactive_90d` 离线撤销通知、提示重新配对；
//! - 89 天 23 小时仍有效（边界），且上线/会话会刷新 last_seen；
//! - `paired_at_ms` 本身没有 TTL（配合 `e2e_pairing_permanence.rs` 验证）。
//!
//! 构造方式：直接改 `peers.json` 的 `last_seen_ms` 模拟"很久没活动"（协议不暴露时间注入）。

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};
use drive_client::{RuntimeEvent, Store};
use drive_e2e::client_harness::{
    pair_nodes, ClientNode, DEFAULT_EVENT_TIMEOUT, DEFAULT_TOKEN,
};
use drive_e2e::harness::{relay_config, start_relay};
use tempfile::TempDir;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const INACTIVE_90D_MS: u64 = 90 * DAY_MS;

/// 把该数据目录下所有配对记录的 last_seen_ms（同时把 paired_at 留在过去，模拟长期配对）
/// 改成指定时间。
fn set_last_seen(dir: &Path, last_seen_ms: u64) -> Result<()> {
    let store = Store::open(dir)?;
    let mut peers = store.load_peers()?;
    if peers.is_empty() {
        bail!("{} 的 peers.json 为空", dir.display());
    }
    for peer in &mut peers {
        peer.last_seen_ms = last_seen_ms;
    }
    store.save_peers(&peers)?;
    Ok(())
}

fn peer_count(dir: &Path) -> Result<usize> {
    Ok(Store::open(dir)?.load_peers()?.len())
}

async fn assert_no_session_established(a: &mut ClientNode, b_id: &str) -> Result<()> {
    let events = a.drain_events();
    if events.iter().any(
        |event| matches!(event, RuntimeEvent::SessionEstablished { peer_id, .. } if peer_id == b_id),
    ) {
        bail!("过期配对应禁止建立会话，但出现了 SessionEstablished({b_id})");
    }
    Ok(())
}

async fn assert_clipboard_not_synced(a: &mut ClientNode, b: &mut ClientNode) -> Result<()> {
    a.set_clipboard_text("expired pair: must not sync").await?;
    if b.recv_clipboard(Duration::from_secs(2)).await.is_ok() {
        bail!("过期配对后 A→B 剪切板仍然同步（不应发生）");
    }
    b.set_clipboard_text("expired pair: must not sync back").await?;
    if a.recv_clipboard(Duration::from_secs(2)).await.is_ok() {
        bail!("过期配对后 B→A 剪切板仍然同步（不应发生）");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 90 天不活跃：自动删除配对 + PeerExpired；重新配对后恢复
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inactive_pair_expires_after_90_days_and_requires_re_pairing() -> Result<()> {
    let root = TempDir::new()?;
    let dir_a = root.path().join("device-a");
    let dir_b = root.path().join("device-b");
    let relay = start_relay(relay_config(None)).await?;
    let url = relay.ws_url();

    pair_nodes(&dir_a, &dir_b, "过期A", "过期B", &url, DEFAULT_TOKEN).await?;
    // 两端 last_seen 都拨到 91 天前（超过 90 天不活跃窗口）。
    let last_seen = drive_core::util::now_ms().saturating_sub(INACTIVE_90D_MS + DAY_MS);
    set_last_seen(&dir_a, last_seen)?;
    set_last_seen(&dir_b, last_seen)?;

    // 新 Runtime 启动：on_authenticated 先处理 90 天过期（不建立会话）。
    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    b.wait_authenticated().await?;
    let a_id = a.device_id().to_string();
    let b_id = b.device_id().to_string();

    // A 应 emit PeerExpired（并删除本地记录）。
    let expired_event = a
        .wait_event(Duration::from_secs(15), |event| {
            matches!(event, RuntimeEvent::PeerExpired { device_id, .. } if *device_id == b_id)
        })
        .await?;
    if let RuntimeEvent::PeerExpired {
        last_seen_ms,
        expires_at_ms,
        ..
    } = expired_event
    {
        assert_eq!(last_seen_ms, last_seen);
        assert!(
            expires_at_ms <= drive_core::util::now_ms(),
            "expires_at_ms 应已过期（expires={expires_at_ms}）"
        );
    }
    // B 侧也应独立过期。
    b.wait_event(Duration::from_secs(15), |event| {
        matches!(event, RuntimeEvent::PeerExpired { device_id, .. } if *device_id == a_id)
    })
    .await?;

    // 双方 peers 记录被自动删除；没有建立会话；同步不生效。
    assert_eq!(peer_count(&dir_a)?, 0, "过期后 A 的 peers 应为空");
    assert_eq!(peer_count(&dir_b)?, 0, "过期后 B 的 peers 应为空");
    assert_no_session_established(&mut a, &b_id).await?;
    assert_clipboard_not_synced(&mut a, &mut b).await?;

    // 重新配对 → 恢复同步（证明过期后必须重新配对）。
    a.shutdown().await?;
    b.shutdown().await?;
    pair_nodes(&dir_a, &dir_b, "过期A", "过期B", &url, DEFAULT_TOKEN).await?;
    assert_eq!(peer_count(&dir_a)?, 1);
    assert_eq!(peer_count(&dir_b)?, 1);
    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    b.wait_authenticated().await?;
    a.wait_session(&b_id).await?;
    b.wait_session(&a_id).await?;
    a.set_clipboard_text("re-paired after expiry").await?;
    assert_eq!(
        b.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?.as_text()?,
        "re-paired after expiry"
    );

    a.shutdown().await?;
    b.shutdown().await?;
    relay.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// 89 天 23 小时边界：仍有效，且会话/同步会刷新 last_seen
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inactive_89d23h_boundary_still_valid_and_touched() -> Result<()> {
    let root = TempDir::new()?;
    let dir_a = root.path().join("device-a");
    let dir_b = root.path().join("device-b");
    let relay = start_relay(relay_config(None)).await?;
    let url = relay.ws_url();

    pair_nodes(&dir_a, &dir_b, "边界A", "边界B", &url, DEFAULT_TOKEN).await?;
    // 89 天 23 小时（< 90 天）：不应过期。
    let last_seen = drive_core::util::now_ms().saturating_sub(INACTIVE_90D_MS - 60 * 60 * 1000);
    set_last_seen(&dir_a, last_seen)?;
    set_last_seen(&dir_b, last_seen)?;

    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    b.wait_authenticated().await?;
    let a_id = a.device_id().to_string();
    let b_id = b.device_id().to_string();

    // 未过期：会话建立、双向同步正常。
    a.wait_session(&b_id).await?;
    b.wait_session(&a_id).await?;
    a.set_clipboard_text("89d boundary works").await?;
    assert_eq!(
        b.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?.as_text()?,
        "89d boundary works"
    );

    // 没有过期事件（89d23h 不应触发 PeerExpired）。
    let events = a.drain_events();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::PeerExpired { .. })),
        "89 天边界不应出现 PeerExpired: {events:?}"
    );

    // last_seen 采用"内存更新 + 每 60s/断开时 flush"策略：优雅 shutdown 触发 flush 后再读盘，
    // 避免依赖尚未落盘的瞬时值（也不在会话建立后立即读盘）。
    a.shutdown().await?;
    b.shutdown().await?;
    let peers_a = Store::open(&dir_a)?.load_peers()?;
    assert_eq!(peers_a.len(), 1, "89 天边界不应删除配对记录");
    assert!(
        peers_a[0].last_seen_ms > last_seen,
        "会话建立/同步后 last_seen 应在 flush 后刷新（旧={last_seen}，新={}）",
        peers_a[0].last_seen_ms
    );
    let now = drive_core::util::now_ms();
    assert!(
        !peers_a[0].inactivity_expired(now),
        "刷新后不应处于过期状态"
    );

    relay.shutdown().await;
    Ok(())
}
