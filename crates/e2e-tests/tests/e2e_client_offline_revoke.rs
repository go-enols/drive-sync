//! v0.2 客户端侧离线撤销 e2e（task-13）：A 删除离线 B → relay 离线信箱 → B 上线自动清理。
//!
//! 断言链路：
//! 1. A 运行时发现外部 `peers.json` 删除 B（模拟 `peers remove`）→ 生成签名 `inactive/user`
//!    撤销通知并 `revocation_put` 到 relay 信箱（B 离线）→ emit `RevocationDelivered`；
//! 2. B 上线连接 relay → 收到 `pending_revocations` → 用本地 PairRecord 中 A 的公钥验签 →
//!    删除本地 A 记录 + emit `PeerRevocationNotice` → 发 `revocation_ack`；
//! 3. relay 索引条目被删除；双方 peers 为空、撤销后剪切板不同步；
//! 4. 重新配对后会话/同步恢复。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Result};
use drive_client::{RuntimeEvent, Store};
use drive_e2e::client_harness::{pair_nodes, ClientNode, DEFAULT_EVENT_TIMEOUT, DEFAULT_TOKEN};
use drive_e2e::harness::{relay_config_with_data_dir, start_relay};
use tempfile::TempDir;

const LONG: Duration = Duration::from_secs(20);

fn index_path(state: &Path) -> PathBuf {
    state.join("revocations").join("index.json")
}

/// 读取 relay 撤销索引里所有 (notice_id, to)。
fn index_entries(state: &Path) -> Result<Vec<(String, String)>> {
    let path = index_path(state);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let stored: Vec<serde_json::Value> = serde_json::from_str(&raw)?;
    Ok(stored
        .iter()
        .filter_map(|item| {
            let id = item["notice"]["content"]["notice_id"].as_str()?.to_string();
            let to = item["notice"]["content"]["to"].as_str()?.to_string();
            Some((id, to))
        })
        .collect())
}

async fn wait_relay_notice_for(state: &Path, target: &str, timeout: Duration) -> Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let entries = index_entries(state)?;
        if let Some((id, _)) = entries.iter().find(|(_, to)| to == target) {
            return Ok(id.clone());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("等待 relay 信箱出现目标 {target} 的撤销通知超时，当前: {entries:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_relay_index_empty(state: &Path, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if index_entries(state)?.is_empty() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("等待 relay 撤销索引清空超时: {:?}", index_entries(state)?);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn peers(dir: &Path) -> Result<Vec<drive_core::pairing::PairRecord>> {
    Store::open(dir)?.load_peers()
}

async fn assert_clipboard_not_synced(a: &mut ClientNode, b: &mut ClientNode) -> Result<()> {
    a.set_clipboard_text("revoked offline: A->B must fail")
        .await?;
    if b.recv_clipboard(Duration::from_secs(2)).await.is_ok() {
        bail!("离线撤销后 A→B 剪切板仍生效（不应发生）");
    }
    b.set_clipboard_text("revoked offline: B->A must fail")
        .await?;
    if a.recv_clipboard(Duration::from_secs(2)).await.is_ok() {
        bail!("离线撤销后 B→A 剪切板仍生效（不应发生）");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_revoke_cleans_peer_via_relay_mailbox_and_repairs() -> Result<()> {
    let root = TempDir::new()?;
    let dir_a = root.path().join("device-a");
    let dir_b = root.path().join("device-b");
    let state = root.path().join("relay-state");
    let relay = start_relay(relay_config_with_data_dir(None, state.clone())).await?;
    let url = relay.ws_url();

    pair_nodes(
        &dir_a,
        &dir_b,
        "离线撤销A",
        "离线撤销B",
        &url,
        DEFAULT_TOKEN,
    )
    .await?;

    // 1) 只启动 A（B 离线）。外部删除 A 的 B 配对记录（模拟另一进程 peers remove）。
    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    let a_id = a.device_id().to_string();
    let b_id = Store::open(&dir_b)?.load_identity()?.device_id();
    assert!(
        Store::open(&dir_a)?.remove_peer(&b_id)?,
        "A 应存在 B 的配对记录"
    );

    // A 检测到外部删除 → emit PeerRevoked（B 离线 notified=false）+ 生成离线通知。
    a.wait_event(
        LONG,
        |event| matches!(event, RuntimeEvent::PeerRevoked { device_id, .. } if *device_id == b_id),
    )
    .await?;
    a.wait_event(LONG, |event| {
        matches!(event, RuntimeEvent::RevocationQueued { device_id, .. } if *device_id == b_id)
    })
    .await?;
    // 通知投递到 relay 离线信箱（A 在线即可投递，B 离线不影响入信箱）。
    a.wait_event(LONG, |event| {
        matches!(event, RuntimeEvent::RevocationDelivered { device_id, .. } if *device_id == b_id)
    })
    .await?;
    let queued_notice_id = wait_relay_notice_for(&state, &b_id, LONG).await?;

    // 2) B 上线：收到 pending_revocations → 验签通过 → 删除 A 记录并 ack。
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    b.wait_authenticated().await?;
    b.wait_event(LONG, |event| {
        matches!(event, RuntimeEvent::PeerRevocationNotice { device_id, .. } if *device_id == a_id)
    })
    .await?;
    assert!(
        peers(&dir_b)?.is_empty(),
        "B 收到离线撤销通知后应删除 A 的配对记录"
    );
    // ack 后 relay 索引删除该通知。
    wait_relay_index_empty(&state, LONG).await?;
    let _ = queued_notice_id;

    // 3) 双方 peers 为空，撤销后双向不同步。
    assert!(peers(&dir_a)?.is_empty(), "A 的 peers 应为空");
    assert_clipboard_not_synced(&mut a, &mut b).await?;

    // 4) 重新配对 → 恢复会话与同步（证明离线撤销后必须重新配对）。
    a.shutdown().await?;
    b.shutdown().await?;
    pair_nodes(
        &dir_a,
        &dir_b,
        "离线撤销A",
        "离线撤销B",
        &url,
        DEFAULT_TOKEN,
    )
    .await?;
    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    b.wait_authenticated().await?;
    let b_id = b.device_id().to_string();
    a.wait_session(&b_id).await?;
    b.wait_session(&a_id).await?;
    a.set_clipboard_text("re-paired after offline revoke")
        .await?;
    assert_eq!(
        b.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?.as_text()?,
        "re-paired after offline revoke"
    );

    a.shutdown().await?;
    b.shutdown().await?;
    relay.shutdown().await;
    Ok(())
}
