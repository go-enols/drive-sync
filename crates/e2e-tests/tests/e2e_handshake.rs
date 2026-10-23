//! E2E 握手 + 剪切板消息经进程内中继往返的冒烟测试。
//!
//! 这是 `derive-bench` 的 CI 快速版本：验证两条真实 WebSocket 客户端经 hello 认证、
//! 经中继转发 `session_init`/`session_ack` 完成 E2E 握手，并以加密信封双向收发
//! 剪切板消息（1KiB）。`bench` 的 1000 次 RTT 不放在 `cargo test` 里（耗时且在慢机器
//! 上有抖动），这里只做正确性覆盖。

use anyhow::{bail, Context, Result};
use derive_core::identity::Identity;
use derive_core::peer::PeerMessage;
use derive_core::wire::ClientToServer;
use derive_e2e::harness::{
    clipboard_message, establish_pair, pair_records, relay_config, start_relay, text_payload,
    RelayClient, DEFAULT_TOKEN,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_handshake_and_clipboard_roundtrip() -> Result<()> {
    let relay = start_relay(relay_config(None)).await?;
    let url = relay.ws_url();

    // 两条真实 WS 连接：hello 认证由 RelayClient::connect 完成。
    let a_identity = Identity::generate("smoke-A");
    let b_identity = Identity::generate("smoke-B");
    let mut a = RelayClient::connect(&url, a_identity, DEFAULT_TOKEN)
        .await
        .context("A 连接失败")?;
    let mut b = RelayClient::connect(&url, b_identity, DEFAULT_TOKEN)
        .await
        .context("B 连接失败")?;

    // 直接构造 PairRecord（配对码/SAS 流程不在本测试范围）。
    let (record_a, record_b) = pair_records(&a.identity, &b.identity, &url);
    assert_eq!(record_a.peer.device_id, b.device_id());
    assert_eq!(record_b.peer.device_id, a.device_id());
    assert!(record_a.peer.is_consistent());
    assert_eq!(record_a.pairing_fingerprint, record_b.pairing_fingerprint);

    // 经中继完成 E2E 握手（session_init / session_ack 均由中继转发）。
    let (mut session_a, mut session_b) = establish_pair(&mut a, &mut b).await?;
    assert_eq!(session_a.session_id, session_b.session_id);
    assert_eq!(session_a.peer_id(), b.device_id().as_str());
    assert_eq!(session_b.peer_id(), a.device_id().as_str());

    let a_id = a.device_id();
    let b_id = b.device_id();
    let text = text_payload(1024);
    let rounds = 10;

    for index in 0..rounds {
        // A → 中继 → B
        let message = clipboard_message(&a_id, index as u64, &text)?;
        let envelope = session_a.encrypt("peer", &message.to_bytes()?)?;
        a.send(&ClientToServer::Relay {
            to: b_id.clone(),
            envelope,
        })
        .await?;

        let envelope = b.recv_relay_envelope().await?;
        let (kind, plaintext) = session_b.decrypt(&envelope)?;
        if kind != "peer" {
            bail!("B 收到意外 kind: {kind}");
        }
        match PeerMessage::from_bytes(&plaintext)? {
            PeerMessage::Clipboard { event } => {
                assert_eq!(event.origin_seq, index);
                assert_eq!(event.origin_device, a_id);
                assert_eq!(event.payload.as_text()?, text, "第 {index} 轮内容不一致");
                event.payload.verify_integrity()?;
            }
            other => bail!("B 收到意外消息: {other:?}"),
        }

        // B → 中继 → A（回显）
        let echo = PeerMessage::Pong {
            ts_ms: index as u64,
        }
        .to_bytes()?;
        let envelope = session_b.encrypt("peer", &echo)?;
        b.send(&ClientToServer::Relay {
            to: a_id.clone(),
            envelope,
        })
        .await?;

        let envelope = a.recv_relay_envelope().await?;
        let (kind, plaintext) = session_a.decrypt(&envelope)?;
        if kind != "peer" {
            bail!("A 收到意外 kind: {kind}");
        }
        match PeerMessage::from_bytes(&plaintext)? {
            PeerMessage::Pong { ts_ms } => assert_eq!(ts_ms, index as u64),
            other => bail!("A 收到意外消息: {other:?}"),
        }
    }

    // 序号严格递增、无丢失。
    assert_eq!(session_a.send_seq(), rounds as u64);
    assert_eq!(session_b.recv_seq(), rounds as u64);
    assert_eq!(session_b.send_seq(), rounds as u64);
    assert_eq!(session_a.recv_seq(), rounds as u64);

    a.close().await;
    b.close().await;
    relay.shutdown().await;
    Ok(())
}
