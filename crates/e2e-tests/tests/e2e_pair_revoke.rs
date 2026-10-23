//! 配对撤销 e2e 回归（task-7 中依赖 task-6 的部分）。
//!
//! 需求：已配对设备除非任一方显式删除 `PairRecord`，否则永久有效；一旦删除：
//! - 在线：对端应在秒级内收到 `PairRevoked`、双方删除 peer 记录、会话销毁、双向同步停止；
//! - 离线：重启后仍无该 peer 记录，对端发起会话握手被拒绝；必须重新配对才能恢复；
//! - 双向删除、重复删除幂等。
//!
//! 构造方式：用 `Store` 直接读写 `peers.json` 模拟"另一个管理进程执行 peers remove"
//! （task-6 的 Engine 每 2s 检测外部 peers.json 变更）；不依赖 CLI 子进程。
//!
//! 断言尽量只用稳定 API（`Store::load_peers/remove_peer`、`RuntimeEvent::SessionEstablished`
//! 与"同步不生效"的负向断言），避免绑定 task-6 新增事件变体的具体名称。

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};
use drive_client::{RuntimeEvent, Store};
use drive_e2e::client_harness::{
    pair_nodes, ClientNode, DEFAULT_EVENT_TIMEOUT, DEFAULT_TOKEN,
};
use drive_e2e::harness::{relay_config, start_relay};
use tempfile::TempDir;

/// 外部管理进程删除本地 peer 记录（等价于 `drive-sync peers remove <id>`）。
fn external_remove_peer(dir: &Path, device_id: &str) -> Result<bool> {
    let store = Store::open(dir)?;
    let removed = store.remove_peer(device_id)?;
    Ok(removed)
}

fn peers(dir: &Path) -> Result<Vec<drive_core::pairing::PairRecord>> {
    Ok(Store::open(dir)?.load_peers()?)
}

fn has_peer(dir: &Path, device_id: &str) -> Result<bool> {
    Ok(peers(dir)?
        .iter()
        .any(|record| record.peer.device_id == device_id))
}

/// peer 记录数轮询等待（外部 peers.json 变更由 Engine 每 2s 检测并传播）。
async fn wait_peer_gone(dir: &Path, device_id: &str, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !has_peer(dir, device_id)? {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "等待 {} 的 peer 记录 {} 消失超时（{timeout:?}）",
                dir.display(),
                device_id
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 断言两个已撤销设备之间剪切板不再同步（双向各等 2s，期望超时=未应用）。
async fn assert_clipboard_not_synced(a: &mut ClientNode, b: &mut ClientNode) -> Result<()> {
    a.set_clipboard_text("revoked: should not reach B")
        .await?;
    if b.recv_clipboard(Duration::from_secs(2)).await.is_ok() {
        bail!("撤销后 A→B 剪切板仍然生效（不应发生）");
    }
    b.set_clipboard_text("revoked: should not reach A")
        .await?;
    if a.recv_clipboard(Duration::from_secs(2)).await.is_ok() {
        bail!("撤销后 B→A 剪切板仍然生效（不应发生）");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 3) 在线撤销：A（运行中）删除 B 记录 → PairRevoked 传播到 B，双向同步停止
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn online_revoke_propagates_and_stops_sync() -> Result<()> {
    let root = TempDir::new()?;
    let dir_a = root.path().join("device-a");
    let dir_b = root.path().join("device-b");
    let relay = start_relay(relay_config(None)).await?;
    let url = relay.ws_url();

    pair_nodes(&dir_a, &dir_b, "撤销A", "撤销B", &url, DEFAULT_TOKEN).await?;
    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    b.wait_authenticated().await?;
    let a_id = a.device_id().to_string();
    let b_id = b.device_id().to_string();
    a.wait_session(&b_id).await?;
    b.wait_session(&a_id).await?;

    // 撤销前同步正常（建立基线，避免"本来就不通"的假阳性）。
    a.set_clipboard_text("before revoke").await?;
    assert_eq!(
        b.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?.as_text()?,
        "before revoke"
    );

    // A 侧外部删除 B（模拟另一个管理进程执行 `drive-sync peers remove <B>`）。
    let revoke_started = std::time::Instant::now();
    assert!(
        external_remove_peer(&dir_a, &b_id)?,
        "A 应存在 B 的配对记录"
    );
    assert!(!has_peer(&dir_a, &b_id)?, "A 本地记录应已删除");

    // A 的 Engine 应在 2s 检测窗口内发现外部变更：emit PeerRevoked（并尽力通知对端）。
    let revoked_event = a
        .wait_event(Duration::from_secs(10), |event| {
            matches!(event, RuntimeEvent::PeerRevoked { device_id, .. } if *device_id == b_id)
        })
        .await?;
    assert!(
        revoke_started.elapsed() < Duration::from_secs(5),
        "撤销应在检测窗口（2s）+ 通知余量内生效，实际耗时 {:?}",
        revoke_started.elapsed()
    );
    if let RuntimeEvent::PeerRevoked { notified, .. } = &revoked_event {
        assert!(*notified, "B 在线时应通过 E2E 会话成功发送 PairRevoked");
    }

    // B 应在秒级内收到 PairRevoked → 删除 A 的本地 peer 记录，并 emit PeerUnpaired。
    wait_peer_gone(&dir_b, &a_id, Duration::from_secs(10)).await?;
    b.wait_event(Duration::from_secs(10), |event| {
        matches!(event, RuntimeEvent::PeerUnpaired { device_id, .. } if *device_id == a_id)
    })
    .await?;

    // 双方记录均不存在；会话销毁后剪切板双向都不再生效。
    assert!(!has_peer(&dir_a, &b_id)?);
    assert!(!has_peer(&dir_b, &a_id)?);
    assert_clipboard_not_synced(&mut a, &mut b).await?;

    a.shutdown().await?;
    b.shutdown().await?;
    relay.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// 4) 离线撤销：A 停机时删除 B → A 重启后 B 的握手被拒绝；必须重新配对才恢复
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_revoke_requires_re_pairing_to_restore() -> Result<()> {
    let root = TempDir::new()?;
    let dir_a = root.path().join("device-a");
    let dir_b = root.path().join("device-b");
    let relay = start_relay(relay_config(None)).await?;
    let url = relay.ws_url();

    pair_nodes(&dir_a, &dir_b, "离线撤销A", "离线撤销B", &url, DEFAULT_TOKEN).await?;

    // A 离线（未启动）时，外部管理进程删除 A 侧的 B 记录。
    assert!(external_remove_peer(&dir_a, &b_id_like(&dir_a)?)?);

    // 启动 A + B：A 已无 B 记录，B 仍持有 A 记录 → B 发起的握手应被 A 拒绝。
    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    b.wait_authenticated().await?;
    let a_id = a.device_id().to_string();
    let b_id = b.device_id().to_string();
    assert!(!has_peer(&dir_a, &b_id)?, "A 重启后仍不应有 B 记录");

    // 给握手留出时间；B 不应出现与 A 的 SessionEstablished。
    tokio::time::sleep(Duration::from_secs(5)).await;
    let b_events = b.drain_events();
    if b_events.iter().any(
        |event| matches!(event, RuntimeEvent::SessionEstablished { peer_id, .. } if *peer_id == a_id),
    ) {
        bail!("A 已撤销 B，但 B 仍与 A 建立了 E2E 会话（应被拒绝）");
    }
    assert_clipboard_not_synced(&mut a, &mut b).await?;

    // 重新配对：停掉运行时后用一次性连接完成配对，再启动即可恢复同步。
    a.shutdown().await?;
    b.shutdown().await?;
    pair_nodes(&dir_a, &dir_b, "离线撤销A", "离线撤销B", &url, DEFAULT_TOKEN).await?;
    assert!(has_peer(&dir_a, &b_id)?, "重新配对后 A 应有 B 记录");
    assert!(has_peer(&dir_b, &a_id)?, "重新配对后 B 应有 A 记录");

    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    b.wait_authenticated().await?;
    a.wait_session(&b_id).await?;
    b.wait_session(&a_id).await?;
    a.set_clipboard_text("re-paired sync works").await?;
    assert_eq!(
        b.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?.as_text()?,
        "re-paired sync works"
    );

    a.shutdown().await?;
    b.shutdown().await?;
    relay.shutdown().await;
    Ok(())
}

/// 从 A 的 peers.json 中读出唯一对端 device_id（A 尚未启动时使用）。
fn b_id_like(dir: &Path) -> Result<String> {
    let records = peers(dir)?;
    match records.as_slice() {
        [record] => Ok(record.peer.device_id.clone()),
        other => bail!("期望 {} 中恰好 1 条配对记录，实际 {}", dir.display(), other.len()),
    }
}

// ---------------------------------------------------------------------------
// 5) 双向撤销 + 删除幂等
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bilateral_revoke_and_idempotent_remove() -> Result<()> {
    let root = TempDir::new()?;
    let dir_a = root.path().join("device-a");
    let dir_b = root.path().join("device-b");
    let relay = start_relay(relay_config(None)).await?;
    let url = relay.ws_url();

    pair_nodes(&dir_a, &dir_b, "双向A", "双向B", &url, DEFAULT_TOKEN).await?;
    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    b.wait_authenticated().await?;
    let a_id = a.device_id().to_string();
    let b_id = b.device_id().to_string();
    a.wait_session(&b_id).await?;
    b.wait_session(&a_id).await?;

    // 双方各自删除对方（双向撤销）。
    assert!(external_remove_peer(&dir_a, &b_id)?);
    assert!(external_remove_peer(&dir_b, &a_id)?);

    // 等两端的 Engine 检测到外部 peers.json 变更并销毁会话（2s 检测窗口 + 通知）。
    a.wait_event(Duration::from_secs(10), |event| {
        matches!(
            event,
            RuntimeEvent::PeerRevoked { device_id, .. } if *device_id == b_id
        )
    })
    .await?;
    b.wait_event(Duration::from_secs(10), |event| {
        matches!(
            event,
            RuntimeEvent::PeerRevoked { device_id, .. } if *device_id == a_id
        ) || matches!(
            event,
            RuntimeEvent::PeerUnpaired { device_id, .. } if *device_id == a_id
        )
    })
    .await?;
    wait_peer_gone(&dir_a, &b_id, Duration::from_secs(10)).await?;
    wait_peer_gone(&dir_b, &a_id, Duration::from_secs(10)).await?;
    assert!(peers(&dir_a)?.is_empty());
    assert!(peers(&dir_b)?.is_empty());

    // 删除幂等：对已不存在的 device 再次 remove 返回 false 且不报错。
    assert!(!external_remove_peer(&dir_a, &b_id)?);
    assert!(!external_remove_peer(&dir_b, &a_id)?);

    assert_clipboard_not_synced(&mut a, &mut b).await?;

    a.shutdown().await?;
    b.shutdown().await?;
    relay.shutdown().await;
    Ok(())
}
