//! 配对永久有效 + 重复配对幂等（task-7 中**不依赖 task-6** 的部分）。
//!
//! 需求：已配对设备除非任一方显式删除 `PairRecord`，否则不论多久、重启/换网络都无需重新配对。
//! 本文件验证：
//! 1. [`paired_peers_never_expire_after_ten_years`]：配对后把两端 `peers.json` 的
//!    `paired_at_ms` 改成 10 年前 → 重启中继 + 用**新的 Runtime** 连接（等价于进程重启/长期
//!    未连接），不重新配对即可建立 E2E 会话，且剪切板双向同步、目录同步正常；断言 peers
//!    记录始终存在、`paired_at_ms` 未被刷新、无"未配对/需重新配对"类错误。
//! 2. [`repeated_pairing_is_idempotent`]：`Store::upsert_peer` 同一 device 两次 → peers
//!    仍 1 条且字段更新；真实非交互配对流程执行两遍 → 两端 peers 仍各 1 条，之后会话/同步正常。
//!
//! 撤销相关用例（在线/离线 PairRevoked、删除后重新配对、双向撤销）依赖 task-6 的
//! client-core 实现（peers 外部变更检测、PairRevoked 处理、peers 命令），见
//! `e2e_pair_revoke.rs`（task-6 合并后补充）。

use std::path::Path;
use anyhow::{bail, Context, Result};
use drive_client::{CatalogAddParams, RuntimeEvent, Store};
use drive_core::identity::Identity;
use drive_core::pairing::{PairRecord, PAIRING_VERSION};
use drive_core::util::now_ms;
use drive_e2e::client_harness::{
    pair_nodes, write_random_file, ClientNode, DEFAULT_EVENT_TIMEOUT, DEFAULT_TOKEN,
};
use drive_e2e::harness::{relay_config, restart_relay_on, start_relay};
use tempfile::TempDir;

/// 10 年（毫秒）。
const TEN_YEARS_MS: u64 = 10 * 365 * 24 * 60 * 60 * 1000;

fn open_store(dir: &Path) -> Result<Store> {
    Store::open(dir).with_context(|| format!("打开数据目录失败: {}", dir.display()))
}

fn peer_count(dir: &Path) -> Result<usize> {
    Ok(open_store(dir)?.load_peers()?.len())
}

/// 把该数据目录下所有配对记录的 `paired_at_ms` 改成指定旧时间（模拟"很久以前配对"）。
fn age_peers(dir: &Path, paired_at_ms: u64) -> Result<()> {
    let store = open_store(dir)?;
    let mut peers = store.load_peers()?;
    if peers.is_empty() {
        bail!("{} 的 peers.json 为空，无法模拟长期未连接", dir.display());
    }
    for peer in &mut peers {
        peer.paired_at_ms = paired_at_ms;
    }
    store.save_peers(&peers)?;
    Ok(())
}



// ---------------------------------------------------------------------------
// 1) 长期未连接 / 进程与中继重启后仍然有效
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paired_peers_never_expire_after_ten_years() -> Result<()> {
    let root = TempDir::new()?;
    let dir_a = root.path().join("device-a");
    let dir_b = root.path().join("device-b");
    let files = root.path().join("files");

    let relay = start_relay(relay_config(None)).await?;
    let address = relay.addr;
    let url = relay.ws_url();

    // 1) 正常配对，确认双方各有 1 条 PairRecord。
    let (pair_a, pair_b) = pair_nodes(&dir_a, &dir_b, "长期A", "长期B", &url, DEFAULT_TOKEN).await?;
    assert_eq!(pair_a.record.pairing_fingerprint, pair_b.record.pairing_fingerprint);
    assert_eq!(peer_count(&dir_a)?, 1);
    assert_eq!(peer_count(&dir_b)?, 1);

    // 2) 把 paired_at_ms 改成 10 年前：模拟"配对后很久没连接"（协议/实现不应有任何 TTL）。
    let ten_years_ago = now_ms().saturating_sub(TEN_YEARS_MS);
    age_peers(&dir_a, ten_years_ago)?;
    age_peers(&dir_b, ten_years_ago)?;

    // 3) 重启中继（同地址），并用**新的 Runtime** 连接（等价于客户端进程重启）。
    relay.shutdown().await;
    let relay = restart_relay_on(address).await?;
    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    b.wait_authenticated().await?;
    let a_id = a.device_id().to_string();
    let b_id = b.device_id().to_string();

    // 关键断言：不重新配对也能建立 E2E 会话（否则 wait_session 超时）。
    a.wait_session(&b_id).await?;
    b.wait_session(&a_id).await?;

    // 4) 剪切板双向同步。
    a.set_clipboard_text("ten years later from A").await?;
    let applied = b.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?;
    assert_eq!(applied.as_text()?, "ten years later from A");
    b.set_clipboard_text("ten years later from B").await?;
    let applied = a.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?;
    assert_eq!(applied.as_text()?, "ten years later from B");

    // 5) 目录同步也正常（证明完整会话能力，不只是能连上）。
    let src = write_random_file(&files, "legacy-software.bin", 4096)?;
    let entry = a
        .handle()
        .catalog_add(CatalogAddParams::new(&src))
        .await?;
    b.wait_event(DEFAULT_EVENT_TIMEOUT, |event| {
        matches!(event, RuntimeEvent::PeerHaveList { entry_ids, .. } if entry_ids.contains(&entry))
    })
    .await?;
    let b_entry = b.catalog_entry(&entry)?;
    assert!(
        b_entry.is_some(),
        "长期未连接的已配对设备仍应通过目录快照同步到新条目"
    );

    // 6) peers 记录始终存在，且 paired_at_ms 未被连接流程刷新（无隐式重配对）。
    let peers_a = open_store(&dir_a)?.load_peers()?;
    let peers_b = open_store(&dir_b)?.load_peers()?;
    assert_eq!(peers_a.len(), 1, "长期未连接后 peers 记录不应丢失");
    assert_eq!(peers_b.len(), 1, "长期未连接后 peers 记录不应丢失");
    assert_eq!(
        peers_a[0].paired_at_ms, ten_years_ago,
        "连接流程不应隐式刷新 paired_at_ms（配对永久有效，无隐式重配对）"
    );
    assert_eq!(peers_a[0].peer.device_id, b_id);
    assert_eq!(peers_b[0].peer.device_id, a_id);

    // 7) 没有任何"需要重新配对/未配对"类错误事件。
    for event in a.drain_events().iter().chain(b.drain_events().iter()) {
        match event {
            RuntimeEvent::Error { message } | RuntimeEvent::RelayError { message, .. } => {
                assert!(
                    !message.contains("重新配对") && !message.contains("未配对"),
                    "不应出现要求重新配对的错误: {message}"
                );
            }
            _ => {}
        }
    }

    a.shutdown().await?;
    b.shutdown().await?;
    relay.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// 2) 重复配对 / 重复 upsert 不产生重复记录
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_pairing_is_idempotent() -> Result<()> {
    // ---- 2a) Store 层：同一 device_id upsert 两次 → 1 条，字段取最新 ----
    let root = TempDir::new()?;
    let store_dir = root.path().join("upsert-store");
    let store = open_store(&store_dir)?;
    let (identity, _) = store.load_or_create_identity("重复配对-A")?;
    let peer_identity = Identity::generate("重复配对-B");
    let mut record = PairRecord {
        v: PAIRING_VERSION,
        peer: peer_identity.public(),
        relay_url: "ws://127.0.0.1:1/ws".to_string(),
        paired_at_ms: now_ms().saturating_sub(TEN_YEARS_MS),
        label: "旧标签".to_string(),
        pairing_fingerprint: "fingerprint-1".to_string(),
    };
    store.upsert_peer(record.clone())?;
    store.upsert_peer(record.clone())?;
    assert_eq!(store.load_peers()?.len(), 1, "同一 device upsert 两次不应重复");

    record.label = "新标签".to_string();
    record.pairing_fingerprint = "fingerprint-2".to_string();
    record.paired_at_ms = now_ms();
    store.upsert_peer(record.clone())?;
    let peers = store.load_peers()?;
    assert_eq!(peers.len(), 1, "更新同一 device 后仍应只有 1 条");
    assert_eq!(peers[0].label, "新标签", "重复配对应保留最新字段");
    assert_eq!(peers[0].pairing_fingerprint, "fingerprint-2");
    assert_eq!(peers[0].peer.device_id, peer_identity.device_id());
    assert_eq!(identity.device_id(), store.load_identity()?.device_id());

    // remove 幂等：删除不存在的 device 不报错（返回 false）。
    assert!(store.remove_peer(&peer_identity.device_id())?);
    assert!(store.load_peers()?.is_empty());
    assert!(!store.remove_peer(&peer_identity.device_id())?, "重复删除应幂等地返回 false");

    // ---- 2b) 真实配对流程跑两遍：两端 peers 仍各 1 条，之后会话/同步正常 ----
    let relay = start_relay(relay_config(None)).await?;
    let url = relay.ws_url();
    let dir_a = root.path().join("device-a");
    let dir_b = root.path().join("device-b");

    pair_nodes(&dir_a, &dir_b, "重复配对-A", "重复配对-B", &url, DEFAULT_TOKEN).await?;
    assert_eq!(peer_count(&dir_a)?, 1);
    assert_eq!(peer_count(&dir_b)?, 1);

    // 再配对一次：upsert 语义应替换而不是追加。
    pair_nodes(&dir_a, &dir_b, "重复配对-A", "重复配对-B", &url, DEFAULT_TOKEN).await?;
    assert_eq!(peer_count(&dir_a)?, 1, "第二次配对后 A 的 peers 仍应 1 条");
    assert_eq!(peer_count(&dir_b)?, 1, "第二次配对后 B 的 peers 仍应 1 条");

    // 二次配对后功能正常（会话建立 + 剪切板同步）。
    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    b.wait_authenticated().await?;
    let a_id = a.device_id().to_string();
    let b_id = b.device_id().to_string();
    a.wait_session(&b_id).await?;
    b.wait_session(&a_id).await?;
    a.set_clipboard_text("idempotent pairing works").await?;
    let applied = b.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?;
    assert_eq!(applied.as_text()?, "idempotent pairing works");

    a.shutdown().await?;
    b.shutdown().await?;
    relay.shutdown().await;
    Ok(())
}

