//! 阶段 2 端到端：目录快照/增量/墓碑 + have_list + 文件传输 BLAKE3 校验。
//!
//! 流程：
//! 1. A 先上线并在 B 离线时 `catalog add`（模拟本地已有目录）；
//! 2. B 上线 → 会话建立 → A 发 `catalog_snapshot`（快照路径）；
//! 3. A 再加一条 → `catalog_delta`（增量路径）；
//! 4. B `request_fetch` → A 流式发送 → B 收齐后校验 BLAKE3；
//! 5. A `catalog remove` → 墓碑经 delta 同步到 B（B 本地条目 deleted=true）。

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};
use derive_client::{CatalogAddParams, RuntimeEvent};
use derive_e2e::client_harness::{
    event_summaries, file_blake3, pair_nodes, write_random_file, ClientNode, DEFAULT_EVENT_TIMEOUT,
    DEFAULT_TOKEN,
};
use derive_e2e::harness::{relay_config, start_relay};
use tempfile::TempDir;

const TRANSFER_SIZE: usize = 2 * 1024 * 1024; // 2 MiB（256KiB 块 → 8 帧，远低于默认限流）

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_snapshot_delta_tombstone_and_fetch_with_blake3() -> Result<()> {
    let relay = start_relay(relay_config(None)).await?;
    let url = relay.ws_url();
    let root = TempDir::new()?;
    let dir_a = root.path().join("device-a");
    let dir_b = root.path().join("device-b");
    let files = root.path().join("files");

    let (pair_a, pair_b) = pair_nodes(&dir_a, &dir_b, "目录A", "目录B", &url, DEFAULT_TOKEN).await?;
    let file_snapshot = write_random_file(&files, "installer-snapshot.bin", TRANSFER_SIZE)?;
    let snapshot_hash = file_blake3(&file_snapshot)?;

    // 1) 只启动 A，B 离线时添加目录条目（只持久化，不会广播）。
    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    let entry_snapshot = a
        .handle()
        .catalog_add(CatalogAddParams::new(&file_snapshot))
        .await?;
    a.wait_event(DEFAULT_EVENT_TIMEOUT, |event| {
        matches!(event, RuntimeEvent::CatalogUpdated { .. })
    })
    .await?;

    // 2) B 上线 → 会话建立 → 收到 A 的目录快照。
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    b.wait_authenticated().await?;
    let a_id = a.device_id().to_string();
    let b_id = b.device_id().to_string();
    assert_eq!(a_id, pair_b.record.peer.device_id);
    assert_eq!(b_id, pair_a.record.peer.device_id);
    a.wait_session(&b_id).await?;
    b.wait_session(&a_id).await?;
    b.wait_event(DEFAULT_EVENT_TIMEOUT, |event| {
        matches!(event, RuntimeEvent::CatalogMerged { .. })
    })
    .await?;

    wait_catalog_entry(&b, &entry_snapshot, Duration::from_secs(10)).await?;
    let merged = b
        .catalog_entry(&entry_snapshot)?
        .expect("B 应通过目录快照合并到 A 的条目");
    assert!(!merged.deleted);
    assert_eq!(merged.content_hash, snapshot_hash);
    assert_eq!(merged.size, TRANSFER_SIZE as u64);

    // 3) A 新增第二条 → delta 增量同步到 B。
    let file_delta = write_random_file(&files, "installer-delta.bin", 4096)?;
    let entry_delta = a
        .handle()
        .catalog_add(CatalogAddParams::new(&file_delta))
        .await?;
    wait_catalog_entry(&b, &entry_delta, Duration::from_secs(10)).await?;

    // 4) B 看到 A 的 have_list → 主动 fetch → 传输完成 + BLAKE3 校验。
    b.wait_event(DEFAULT_EVENT_TIMEOUT, |event| {
        matches!(event, RuntimeEvent::PeerHaveList { entry_ids, .. } if entry_ids.contains(&entry_snapshot))
    })
    .await?;
    b.handle().request_fetch(&a_id, &entry_snapshot).await?;
    let transfer_event = b
        .wait_event(Duration::from_secs(30), |event| {
            matches!(event, RuntimeEvent::TransferReceived { entry_id: Some(id), .. } if id == &entry_snapshot)
        })
        .await?;
    let (received_path, received_size) = match transfer_event {
        RuntimeEvent::TransferReceived { path, size, .. } => (path, size),
        other => bail!("期待 TransferReceived，收到: {other:?}"),
    };
    assert_eq!(received_size, TRANSFER_SIZE as u64);
    assert_file_equals(&received_path, &file_snapshot, &snapshot_hash)?;

    // 5) A 删除第一条 → 墓碑同步到 B（deleted=true，不再出现在 visible 列表）。
    a.handle().catalog_remove(&entry_snapshot).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let tombstone = b.catalog_entry(&entry_snapshot)?;
        if tombstone.as_ref().map(|entry| entry.deleted).unwrap_or(false) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let events = b.drain_events();
            bail!(
                "等待墓碑同步超时；B 最近事件: {:?}",
                event_summaries(&events)
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let visible = b.visible_entries()?;
    assert!(
        !visible.iter().any(|entry| entry.id == entry_snapshot),
        "墓碑条目不应出现在 visible 列表"
    );
    // 增量条目仍在。
    assert!(
        visible.iter().any(|entry| entry.id == entry_delta),
        "未删除的增量条目应仍可见"
    );

    a.shutdown().await?;
    b.shutdown().await?;
    relay.shutdown().await;
    Ok(())
}

/// 轮询等待对端节点目录中出现某条目（50ms 间隔）。
async fn wait_catalog_entry(node: &ClientNode, entry_id: &str, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if node.catalog_entry(entry_id)?.is_some() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("等待目录条目 {entry_id} 同步超时（{timeout:?}）");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn assert_file_equals(received: &Path, original: &Path, expected_hash: &str) -> Result<()> {    let actual_hash = file_blake3(received)?;
    if actual_hash != expected_hash {
        bail!(
            "接收文件 BLAKE3 不匹配: {}（{}） vs {}（{}）",
            actual_hash,
            received.display(),
            expected_hash,
            original.display()
        );
    }
    let received_bytes = std::fs::read(received)?;
    let original_bytes = std::fs::read(original)?;
    if received_bytes != original_bytes {
        bail!("接收文件内容与源文件不一致");
    }
    Ok(())
}
