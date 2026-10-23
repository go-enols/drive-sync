//! 阶段 2 端到端：断线重连 + 离线 spool 补发 + 中继重启后的透明自动重连。
//!
//! 两个场景：
//! 1. [`spool_replay_after_peer_online`]：B 离线时 A 写入剪切板 → 事件进 A 本地 spool；
//!    B 上线后会话建立 → A 补发 → B 应用（中继不缓存离线帧，协议 §4.4）。
//! 2. [`reconnect_after_connection_replaced`]：用同 device_id 的新连接抢占（中继语义：
//!    踢掉旧连接，旧连接收 Close 4002）→ A 的 Runtime 自动退避重连、与 B 重建 E2E 会话，
//!    之后剪切板继续同步（验证透明断线重连路径）。
//! 3. [`relay_restart_transparent_reconnect`]：**中继在同一地址重启**（`RelayHandle::shutdown`
//!    现在会向在线连接发送 Close(1001 going away)）→ 两端 Runtime 不重启、自动重连到新中继
//!    并重建 E2E 会话 → 剪切板继续双向同步。

use std::time::Duration;

use anyhow::Result;
use drive_client::RuntimeEvent;
use drive_e2e::client_harness::{pair_nodes, ClientNode, DEFAULT_EVENT_TIMEOUT, DEFAULT_TOKEN};
use drive_e2e::harness::{relay_config, restart_relay_on, start_relay};
use tempfile::TempDir;

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spool_replay_after_peer_online() -> Result<()> {
    init_tracing();
    let relay = start_relay(relay_config(None)).await?;
    let url = relay.ws_url();
    let root = TempDir::new()?;
    let dir_a = root.path().join("device-a");
    let dir_b = root.path().join("device-b");

    pair_nodes(&dir_a, &dir_b, "补发A", "补发B", &url, DEFAULT_TOKEN).await?;

    // B 离线：A 的剪切板事件进入本地 spool（中继不缓存）。
    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    a.set_clipboard_text("offline clipboard").await?;
    a.wait_event(DEFAULT_EVENT_TIMEOUT, |event| {
        matches!(event, RuntimeEvent::ClipboardSpooled { .. })
    })
    .await?;

    // B 上线：会话建立后 A 补发 spool，B 应用内容。
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    b.wait_authenticated().await?;
    let a_id = a.device_id().to_string();
    let b_id = b.device_id().to_string();
    a.wait_session(&b_id).await?;
    b.wait_session(&a_id).await?;
    a.wait_event(
        DEFAULT_EVENT_TIMEOUT,
        |event| matches!(event, RuntimeEvent::SpoolReplayed { count, .. } if *count >= 1),
    )
    .await?;

    let applied = b.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?;
    assert_eq!(applied.as_text()?, "offline clipboard");

    a.shutdown().await?;
    b.shutdown().await?;
    relay.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_restart_transparent_reconnect() -> Result<()> {
    init_tracing();
    let relay = start_relay(relay_config(None)).await?;
    let address = relay.addr;
    let url = relay.ws_url();
    let root = TempDir::new()?;
    let dir_a = root.path().join("device-a");
    let dir_b = root.path().join("device-b");

    pair_nodes(&dir_a, &dir_b, "重启A", "重启B", &url, DEFAULT_TOKEN).await?;
    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    b.wait_authenticated().await?;
    let a_id = a.device_id().to_string();
    let b_id = b.device_id().to_string();
    a.wait_session(&b_id).await?;
    b.wait_session(&a_id).await?;

    // 中继关停（向在线连接发 Close 1001）→ 同地址重启 → 客户端 Runtime 自动重连。
    let shutdown_started = std::time::Instant::now();
    relay.shutdown().await;
    assert!(
        shutdown_started.elapsed() < Duration::from_secs(5),
        "中继关停应在 5s 内返回（含关闭在线连接）"
    );
    let relay = restart_relay_on(address).await?;

    // 不重启客户端：应自动重连 + 重建 E2E 会话。
    a.wait_event(Duration::from_secs(30), |event| {
        matches!(event, RuntimeEvent::Authenticated { .. })
    })
    .await?;
    b.wait_event(Duration::from_secs(30), |event| {
        matches!(event, RuntimeEvent::Authenticated { .. })
    })
    .await?;
    a.wait_session(&b_id).await?;
    b.wait_session(&a_id).await?;

    // 重启后剪切板双向同步仍正常。
    a.set_clipboard_text("A after relay restart").await?;
    let applied = b.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?;
    assert_eq!(applied.as_text()?, "A after relay restart");
    b.set_clipboard_text("B after relay restart").await?;
    let applied = a.recv_clipboard(DEFAULT_EVENT_TIMEOUT).await?;
    assert_eq!(applied.as_text()?, "B after relay restart");

    a.shutdown().await?;
    b.shutdown().await?;
    relay.shutdown().await;
    Ok(())
}
