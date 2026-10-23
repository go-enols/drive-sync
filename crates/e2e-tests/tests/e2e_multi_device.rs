//! 阶段 2 端到端：2~3 设备并发（A 同时与 B、C 配对；B/C 未互相配对）。
//!
//! 覆盖：
//! 1. A↔B、A↔C 各建立 E2E 会话（B/C 之间不应建会话，见架构 §14 第 8 条）；
//! 2. A 的剪切板同时同步到 B、C；
//! 3. A 的目录条目同步到 B、C，两个对端**并发** fetch 同一文件均成功并通过 BLAKE3；
//! 4. 设备 Set 的 presence/session 视图符合预期。

use std::time::Duration;

use anyhow::{bail, Result};
use drive_client::{CatalogAddParams, RuntimeEvent};
use drive_e2e::client_harness::{
    event_summaries, file_blake3, pair_nodes, write_random_file, ClientNode, DEFAULT_EVENT_TIMEOUT,
    DEFAULT_TOKEN,
};
use drive_e2e::harness::{relay_config, start_relay};
use tempfile::TempDir;

const FILE_SIZE: usize = 1024 * 1024; // 1 MiB

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_devices_clipboard_and_concurrent_fetch() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let relay = start_relay(relay_config(None)).await?;
    let url = relay.ws_url();
    let root = TempDir::new()?;
    let dir_a = root.path().join("device-a");
    let dir_b = root.path().join("device-b");
    let dir_c = root.path().join("device-c");
    let files = root.path().join("files");

    // 配对：A-B、A-C（B-C 不配对，v0.1 不做群组转发，见架构 §14）。
    pair_nodes(&dir_a, &dir_b, "三端A", "三端B", &url, DEFAULT_TOKEN).await?;
    pair_nodes(&dir_a, &dir_c, "三端A", "三端C", &url, DEFAULT_TOKEN).await?;

    let src = write_random_file(&files, "shared-tool.bin", FILE_SIZE)?;
    let src_hash = file_blake3(&src)?;

    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    let mut c = ClientNode::start(&dir_c, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    b.wait_authenticated().await?;
    c.wait_authenticated().await?;
    let a_id = a.device_id().to_string();
    let b_id = b.device_id().to_string();
    let c_id = c.device_id().to_string();
    a.wait_session(&b_id).await?;
    a.wait_session(&c_id).await?;
    b.wait_session(&a_id).await?;
    c.wait_session(&a_id).await?;

    // B/C 未配对：不应建立 B↔C 会话。
    tokio::time::sleep(Duration::from_millis(800)).await;
    let b_events = b.drain_events();
    assert!(
        !b_events.iter().any(|event| {
            matches!(event, RuntimeEvent::SessionEstablished { peer_id, .. } if *peer_id == c_id)
        }),
        "B/C 未配对不应建立会话；B 的事件: {:?}",
        event_summaries(&b_events)
    );

    // 1) 剪切板从 A 并发同步到 B、C。
    a.set_clipboard_text("three-device clipboard").await?;
    let applied_b = b.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?;
    let applied_c = c.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?;
    assert_eq!(applied_b.as_text()?, "three-device clipboard");
    assert_eq!(applied_c.as_text()?, "three-device clipboard");

    // 2) 目录：A 添加一个条目 → B、C 都应看到 have_list / 合并目录。
    let entry = a.handle().catalog_add(CatalogAddParams::new(&src)).await?;
    b.wait_event(DEFAULT_EVENT_TIMEOUT, |event| {
        matches!(event, RuntimeEvent::PeerHaveList { entry_ids, .. } if entry_ids.contains(&entry))
    })
    .await?;
    c.wait_event(DEFAULT_EVENT_TIMEOUT, |event| {
        matches!(event, RuntimeEvent::PeerHaveList { entry_ids, .. } if entry_ids.contains(&entry))
    })
    .await?;

    // 3) B、C **并发**向 A 请求**同一**文件（A 并发上限 2，恰好覆盖）。
    //    回归 client-impl 修复的 bug：offer_cache 原按 entry_id 复用含 transfer_id 的
    //    manifest，导致同一 entry 的两次并发 fetch 共用 transfer_id、A 端 outgoing 互相覆盖；
    //    修复后按 (peer_id, entry_id) 缓存，两个对端各自独立 transfer_id。
    b.handle().request_fetch(&a_id, &entry).await?;
    c.handle().request_fetch(&a_id, &entry).await?;
    let path_b = match wait_transfer(&mut b, &entry).await {
        Ok(path) => path,
        Err(error) => {
            eprintln!("B 事件: {:?}", event_summaries(&b.drain_events()));
            return Err(error);
        }
    };
    let path_c = match wait_transfer(&mut c, &entry).await {
        Ok(path) => path,
        Err(error) => {
            eprintln!("C 事件: {:?}", event_summaries(&c.drain_events()));
            return Err(error);
        }
    };
    assert_eq!(
        file_blake3(&path_b)?,
        src_hash,
        "B 收到的文件 BLAKE3 不一致"
    );
    assert_eq!(
        file_blake3(&path_c)?,
        src_hash,
        "C 收到的文件 BLAKE3 不一致"
    );

    a.shutdown().await?;
    b.shutdown().await?;
    c.shutdown().await?;
    relay.shutdown().await;
    Ok(())
}

async fn wait_transfer(node: &mut ClientNode, entry_id: &str) -> Result<std::path::PathBuf> {
    let entry_id = entry_id.to_string();
    let event = node
        .wait_event(Duration::from_secs(30), move |event| {
            matches!(event, RuntimeEvent::TransferReceived { entry_id: Some(id), .. } if *id == entry_id)
        })
        .await?;
    match event {
        RuntimeEvent::TransferReceived { path, .. } => Ok(path),
        other => bail!("期待 TransferReceived，收到: {other:?}"),
    }
}
