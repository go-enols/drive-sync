//! 阶段 2 端到端：非交互配对 → 双向 E2E 会话 → 剪切板同步（去重/回环抑制）。
//!
//! 使用 `drive-client` 的 Runtime（库 API，非 CLI 子进程），经进程内真实中继通信。

use std::time::Duration;

use anyhow::Result;
use drive_client::RuntimeEvent;
use drive_e2e::client_harness::{
    event_summaries, pair_nodes, ClientNode, DEFAULT_EVENT_TIMEOUT, DEFAULT_TOKEN,
};
use drive_e2e::harness::{relay_config, start_relay};
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_clipboard_sync_dedupe_and_loopback() -> Result<()> {
    let relay = start_relay(relay_config(None)).await?;
    let url = relay.ws_url();
    let root = TempDir::new()?;
    let dir_a = root.path().join("device-a");
    let dir_b = root.path().join("device-b");

    // 1) 非交互配对（自动确认 SAS）；两端指纹一致、互相记录 peer。
    let (pair_a, pair_b) = pair_nodes(&dir_a, &dir_b, "设备A", "设备B", &url, DEFAULT_TOKEN).await?;
    assert_eq!(pair_a.record.pairing_fingerprint, pair_b.record.pairing_fingerprint);
    assert_eq!(pair_a.record.peer.name, "设备B");
    assert_eq!(pair_b.record.peer.name, "设备A");
    assert!(!pair_a.sas.is_empty(), "配对应产生 SAS");

    // 2) 同时启动两条 Runtime（真实 WS + E2E 会话）。
    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    b.wait_authenticated().await?;
    let a_id = a.device_id().to_string();
    let b_id = b.device_id().to_string();
    assert_eq!(a_id, pair_b.record.peer.device_id);
    assert_eq!(b_id, pair_a.record.peer.device_id);
    a.wait_session(&b_id).await?;
    b.wait_session(&a_id).await?;

    // 3) A 注入剪切板 → B 应用内容（含 ClipboardApplied 事件）。
    let text_ab = "hello drive-sync e2e";
    a.set_clipboard_text(text_ab).await?;
    let applied = b.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?;
    assert_eq!(applied.as_text()?, text_ab);
    b.wait_event(DEFAULT_EVENT_TIMEOUT, |event| {
        matches!(event, RuntimeEvent::ClipboardApplied { .. })
    })
    .await?;

    // 4) 回环抑制：B 的本地监听器读到"刚由对端应用"的同一内容 → 不应回传给 A。
    b.set_clipboard_text(text_ab).await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let events_a = a.drain_events();
    assert!(
        !events_a
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ClipboardApplied { .. })),
        "B 应抑制回环（不应把刚应用的内容再发回 A）；A 收到的事件: {:?}",
        event_summaries(&events_a)
    );

    // 5) 链路仍正常：B → A 的新内容可以同步（去重不会误伤新内容）。
    b.set_clipboard_text("message from B").await?;
    let applied = a.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?;
    assert_eq!(applied.as_text()?, "message from B");

    // 6) A 的新内容再次同步到 B。
    a.set_clipboard_text("second message from A").await?;
    let applied = b.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?;
    assert_eq!(applied.as_text()?, "second message from A");

    a.shutdown().await?;
    b.shutdown().await?;
    relay.shutdown().await;
    Ok(())
}
