//! derive-relay 集成测试：真实 WebSocket 客户端覆盖认证、presence、配对、转发与暂存。
//!
//! 每个测试用 `127.0.0.1:0` 启动独立中继实例，互不干扰。

use std::net::SocketAddr;
use std::time::Duration;

use derive_core::identity::Identity;
use derive_core::pairing::{
    PairJoinPayload, PairingInitiator, PairingResponder, PAIRING_VERSION,
};
use derive_core::session::{accept_session_init, complete_session, create_session_init, E2eEnvelope};
use derive_core::util::{now_ms, random_hex};
use derive_core::vault::VaultMeta;
use derive_core::wire::{hello_sign_input, ClientToServer, ServerToClient, WIRE_VERSION};
use derive_relay::{start, RelayConfig, RelayHandle};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const TEST_TOKEN: &str = "test-token-0123456789abcdef";
const WAIT: Duration = Duration::from_secs(5);

fn test_config() -> RelayConfig {
    let mut config = RelayConfig::new(TEST_TOKEN);
    config.bind = SocketAddr::from(([127, 0, 0, 1], 0));
    config
}

async fn spawn_relay() -> RelayHandle {
    start(test_config()).await.expect("启动中继失败")
}

async fn spawn_relay_with(mutate: impl FnOnce(&mut RelayConfig)) -> RelayHandle {
    let mut config = test_config();
    mutate(&mut config);
    start(config).await.expect("启动中继失败")
}

async fn ws_connect(addr: SocketAddr) -> Ws {
    let url = format!("ws://{addr}/ws");
    let (ws, _response) = connect_async(&url).await.expect("连接中继失败");
    ws
}

async fn send_json(ws: &mut Ws, message: &ClientToServer) {
    let json = message.to_json().expect("序列化客户端消息失败");
    ws.send(Message::Text(json)).await.expect("发送失败");
}

fn hello_message(identity: &Identity, token: &str) -> ClientToServer {
    hello_message_with(identity, token, WIRE_VERSION)
}

fn hello_message_with(identity: &Identity, token: &str, version: u32) -> ClientToServer {
    let device = identity.public();
    let nonce = random_hex(16);
    let signature = identity.sign(&hello_sign_input(&nonce, &device.device_id));
    ClientToServer::Hello {
        v: version,
        device,
        access_token: token.to_string(),
        nonce,
        signature,
    }
}

fn dummy_join(identity: &Identity) -> PairJoinPayload {
    PairJoinPayload {
        v: PAIRING_VERSION,
        device: identity.public(),
        ephemeral_pub: [9u8; 32],
        created_at_ms: now_ms(),
    }
}

fn vault_meta(name: &str, size: u64) -> VaultMeta {
    VaultMeta {
        name: name.to_string(),
        kind: "clipboard".to_string(),
        size,
        content_hash: "hash".to_string(),
        entry_id: None,
        note: String::new(),
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum Event {
    Message(ServerToClient),
    Closed,
    Timeout,
}

async fn next_event(ws: &mut Ws, wait: Duration) -> Event {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Event::Timeout;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Err(_) => return Event::Timeout,
            Ok(None) => return Event::Closed,
            Ok(Some(Err(_))) => return Event::Closed,
            Ok(Some(Ok(Message::Text(text)))) => match ServerToClient::from_json(&text) {
                Ok(message) => return Event::Message(message),
                Err(_) => return Event::Closed,
            },
            Ok(Some(Ok(Message::Close(_)))) => return Event::Closed,
            Ok(Some(Ok(_))) => continue,
        }
    }
}

struct Client {
    ws: Ws,
    identity: Identity,
    device_id: String,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Self {
        Self::connect_with(addr, Identity::generate("测试设备")).await
    }

    async fn connect_with(addr: SocketAddr, identity: Identity) -> Self {
        let mut ws = ws_connect(addr).await;
        send_json(&mut ws, &hello_message(&identity, TEST_TOKEN)).await;
        let welcome = match next_event(&mut ws, WAIT).await {
            Event::Message(message @ ServerToClient::Welcome { .. }) => message,
            other => panic!("期望 welcome，收到 {other:?}"),
        };
        let device_id = identity.device_id();
        if let ServerToClient::Welcome {
            device_id: welcome_device,
            ..
        } = &welcome
        {
            assert_eq!(welcome_device, &device_id, "welcome.device_id 必须是当前设备");
        }
        Self {
            ws,
            identity,
            device_id,
        }
    }

    async fn send(&mut self, message: &ClientToServer) {
        send_json(&mut self.ws, message).await;
    }

    async fn next(&mut self) -> Event {
        next_event(&mut self.ws, WAIT).await
    }

    async fn recv(&mut self) -> ServerToClient {
        match self.next().await {
            Event::Message(message) => message,
            Event::Closed => panic!("连接意外关闭"),
            Event::Timeout => panic!("等待中继消息超时"),
        }
    }

    async fn recv_until(&mut self, predicate: impl Fn(&ServerToClient) -> bool) -> ServerToClient {
        for _ in 0..64 {
            let message = self.recv().await;
            if predicate(&message) {
                return message;
            }
        }
        panic!("未收到期望的中继消息");
    }

    async fn expect_closed(&mut self, wait: Duration) {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                panic!("连接未被关闭");
            }
            match next_event(&mut self.ws, remaining).await {
                Event::Closed => return,
                Event::Message(_) => continue,
                Event::Timeout => panic!("连接未被关闭"),
            }
        }
    }

    async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

async fn expect_auth_failure(mut ws: Ws, hello: ClientToServer, expected_code: &str) {
    send_json(&mut ws, &hello).await;
    match next_event(&mut ws, WAIT).await {
        Event::Message(ServerToClient::Error { code, .. }) => assert_eq!(code, expected_code),
        other => panic!("期望 {expected_code} 错误，收到 {other:?}"),
    }
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "认证失败后连接未被关闭");
        match next_event(&mut ws, remaining).await {
            Event::Closed => return,
            Event::Message(_) => continue,
            Event::Timeout => panic!("认证失败后连接未被关闭"),
        }
    }
}

async fn http_healthz(addr: SocketAddr) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("连接 HTTP 端口失败");
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("发送 HTTP 请求失败");
    let mut buffer = Vec::new();
    stream.read_to_end(&mut buffer).await.expect("读取 HTTP 响应失败");
    String::from_utf8_lossy(&buffer).to_string()
}

/// 等待并返回服务端 Close 帧的关闭码（跳过其他消息）；超时/直接断开返回 None。
async fn wait_for_close_code(ws: &mut Ws, wait: Duration) -> Option<u16> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Err(_) | Ok(None) | Ok(Some(Err(_))) => return None,
            Ok(Some(Ok(Message::Close(frame)))) => {
                return Some(frame.map(|frame| u16::from(frame.code)).unwrap_or(0))
            }
            Ok(Some(Ok(_))) => continue,
        }
    }
}

// ---------------------------------------------------------------------------
// 健康检查与认证
// ---------------------------------------------------------------------------

#[tokio::test]
async fn healthz_returns_ok() {
    let relay = spawn_relay().await;
    let response = http_healthz(relay.addr).await;
    assert!(response.contains("200"), "响应: {response}");
    assert!(response.to_ascii_lowercase().contains("ok"), "响应: {response}");
    relay.shutdown().await;
}

#[tokio::test]
async fn valid_hello_gets_welcome_with_online_list() {
    let relay = spawn_relay().await;
    let client = Client::connect(relay.addr).await;
    assert!(client.device_id.starts_with("dev-"));
    // 认证成功后应收到包含自己的 presence。
    let mut client = client;
    let presence = client
        .recv_until(|message| matches!(message, ServerToClient::Presence { .. }))
        .await;
    if let ServerToClient::Presence { online } = presence {
        assert_eq!(online, vec![client.device_id.clone()]);
    }
    relay.shutdown().await;
}

#[tokio::test]
async fn auth_rejects_wrong_token() {
    let relay = spawn_relay().await;
    let identity = Identity::generate("dev");
    let ws = ws_connect(relay.addr).await;
    let mut hello = hello_message(&identity, "wrong-token");
    if let ClientToServer::Hello { access_token, .. } = &mut hello {
        *access_token = "wrong-token".to_string();
    }
    expect_auth_failure(ws, hello, "auth_failed").await;
    relay.shutdown().await;
}

#[tokio::test]
async fn auth_rejects_bad_version() {
    let relay = spawn_relay().await;
    let identity = Identity::generate("dev");
    let ws = ws_connect(relay.addr).await;
    expect_auth_failure(
        ws,
        hello_message_with(&identity, TEST_TOKEN, WIRE_VERSION + 1),
        "bad_version",
    )
    .await;
    relay.shutdown().await;
}

#[tokio::test]
async fn auth_rejects_bad_signature() {
    let relay = spawn_relay().await;
    let identity = Identity::generate("dev");
    let ws = ws_connect(relay.addr).await;
    let mut hello = hello_message(&identity, TEST_TOKEN);
    if let ClientToServer::Hello { signature, .. } = &mut hello {
        *signature = [0u8; 64];
    }
    expect_auth_failure(ws, hello, "auth_failed").await;
    relay.shutdown().await;
}

#[tokio::test]
async fn auth_rejects_inconsistent_device_id() {
    let relay = spawn_relay().await;
    let identity = Identity::generate("dev");
    let ws = ws_connect(relay.addr).await;
    let mut hello = hello_message(&identity, TEST_TOKEN);
    if let ClientToServer::Hello { device, .. } = &mut hello {
        device.device_id = "dev-0000000000000000".to_string();
    }
    expect_auth_failure(ws, hello, "auth_failed").await;
    relay.shutdown().await;
}

#[tokio::test]
async fn auth_rejects_non_hello_first_frame() {
    let relay = spawn_relay().await;
    let mut ws = ws_connect(relay.addr).await;
    ws.send(Message::Text(
        ClientToServer::Ping { ts_ms: 1 }.to_json().expect("json"),
    ))
    .await
    .expect("发送 ping");
    match next_event(&mut ws, WAIT).await {
        Event::Message(ServerToClient::Error { code, .. }) => assert_eq!(code, "auth_failed"),
        other => panic!("期望 auth_failed，收到 {other:?}"),
    }
    relay.shutdown().await;
}

#[tokio::test]
async fn auth_rejects_malformed_json() {
    let relay = spawn_relay().await;
    let mut ws = ws_connect(relay.addr).await;
    ws.send(Message::Text("{not valid json".to_string()))
        .await
        .expect("发送非法 JSON");
    match next_event(&mut ws, WAIT).await {
        Event::Message(ServerToClient::Error { code, .. }) => assert_eq!(code, "auth_failed"),
        other => panic!("期望 auth_failed，收到 {other:?}"),
    }
    relay.shutdown().await;
}

#[tokio::test]
async fn same_device_new_connection_kicks_old_one() {
    let relay = spawn_relay().await;
    let identity = Identity::generate("重复设备");
    let mut first = Client::connect_with(relay.addr, identity.clone()).await;
    let _second = Client::connect_with(relay.addr, identity.clone()).await;
    first.expect_closed(Duration::from_secs(3)).await;
    relay.shutdown().await;
}

#[tokio::test]
async fn multi_conn_mode_rejects_duplicate_device() {
    let relay = spawn_relay_with(|config| config.allow_multi_conn = true).await;
    let identity = Identity::generate("重复设备");
    let _first = Client::connect_with(relay.addr, identity.clone()).await;
    let mut ws = ws_connect(relay.addr).await;
    send_json(&mut ws, &hello_message(&identity, TEST_TOKEN)).await;
    match next_event(&mut ws, WAIT).await {
        Event::Message(ServerToClient::Error { code, .. }) => assert_eq!(code, "auth_failed"),
        other => panic!("期望 auth_failed，收到 {other:?}"),
    }
    relay.shutdown().await;
}

// ---------------------------------------------------------------------------
// presence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presence_broadcasts_connect_and_disconnect() {
    let relay = spawn_relay().await;
    let mut alice = Client::connect(relay.addr).await;
    let alice_id = alice.device_id.clone();
    let presence = alice
        .recv_until(|message| matches!(message, ServerToClient::Presence { .. }))
        .await;
    if let ServerToClient::Presence { online } = presence {
        assert_eq!(online, vec![alice_id.clone()]);
    }

    let bob = Client::connect(relay.addr).await;
    let bob_id = bob.device_id.clone();

    // Alice 看到 Bob 上线。
    let presence = alice
        .recv_until(|message| {
            matches!(message, ServerToClient::Presence { online } if online.contains(&bob_id))
        })
        .await;
    if let ServerToClient::Presence { online } = presence {
        let mut expected = vec![alice_id.clone(), bob_id.clone()];
        expected.sort();
        assert_eq!(online, expected);
    }

    // Bob 下线，Alice 收到不含 Bob 的 presence。
    bob.close().await;
    let presence = alice
        .recv_until(|message| {
            matches!(
                message,
                ServerToClient::Presence { online }
                    if !online.contains(&bob_id) && online.contains(&alice_id)
            )
        })
        .await;
    if let ServerToClient::Presence { online } = presence {
        assert_eq!(online, vec![alice_id]);
    }
    relay.shutdown().await;
}

// ---------------------------------------------------------------------------
// 配对路由
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pair_announce_and_join_are_routed_and_consumed_once() {
    let relay = spawn_relay().await;
    let mut alice = Client::connect(relay.addr).await;
    let mut bob = Client::connect(relay.addr).await;
    let alice_id = alice.device_id.clone();
    let bob_id = bob.device_id.clone();

    let initiator = PairingInitiator::start(&alice.identity, "ws://relay.test");
    let code_hash = initiator.code_hash();
    let invite = initiator.invite.clone();
    alice
        .send(&ClientToServer::PairAnnounce {
            code_hash: code_hash.clone(),
            invite: invite.clone(),
            ttl_secs: 120,
        })
        .await;

    let responder = PairingResponder::accept(&bob.identity, &invite, &initiator.code)
        .expect("构造配对加入信息");
    bob.send(&ClientToServer::PairJoin {
        code_hash: code_hash.clone(),
        join: responder.join.clone(),
    })
    .await;

    // Alice（发起方）收到 pair_event{join}。
    let event = alice
        .recv_until(|message| matches!(message, ServerToClient::PairEvent { .. }))
        .await;
    if let ServerToClient::PairEvent { from, invite, join } = event {
        assert_eq!(from, bob_id);
        assert!(invite.is_none());
        let join = join.expect("pair_event 应携带 join");
        assert_eq!(join.device.device_id, bob_id);
    }

    // Bob（加入方）收到 pair_event{invite} + pair_result{ok:true, peer:A}。
    let mut got_invite = false;
    let mut got_result = false;
    for _ in 0..8 {
        match bob.recv().await {
            ServerToClient::PairEvent { from, invite, join } if !got_invite => {
                assert_eq!(from, alice_id);
                assert!(join.is_none());
                assert_eq!(invite.expect("应携带 invite").device.device_id, alice_id);
                got_invite = true;
            }
            ServerToClient::PairResult { ok, peer, .. } if !got_result => {
                assert!(ok);
                assert_eq!(peer.as_deref(), Some(alice_id.as_str()));
                got_result = true;
            }
            _ => {}
        }
        if got_invite && got_result {
            break;
        }
    }
    assert!(got_invite, "加入方未收到 invite");
    assert!(got_result, "加入方未收到 pair_result");

    // 一次性消费：重复 join 必须失败。
    bob.send(&ClientToServer::PairJoin {
        code_hash: code_hash.clone(),
        join: responder.join.clone(),
    })
    .await;
    let result = bob
        .recv_until(|message| matches!(message, ServerToClient::PairResult { .. }))
        .await;
    if let ServerToClient::PairResult { ok, message, .. } = result {
        assert!(!ok);
        assert!(
            message.contains("pair_not_found"),
            "message 应包含 pair_not_found: {message}"
        );
    }
    relay.shutdown().await;
}

#[tokio::test]
async fn pair_registration_expires_after_ttl() {
    let relay = spawn_relay_with(|config| config.pair_ttl_secs = 1).await;
    let mut alice = Client::connect(relay.addr).await;
    let mut bob = Client::connect(relay.addr).await;

    let initiator = PairingInitiator::start(&alice.identity, "ws://relay.test");
    let code_hash = initiator.code_hash();
    alice
        .send(&ClientToServer::PairAnnounce {
            code_hash: code_hash.clone(),
            invite: initiator.invite.clone(),
            ttl_secs: 120,
        })
        .await;

    tokio::time::sleep(Duration::from_millis(1300)).await;

    bob.send(&ClientToServer::PairJoin {
        code_hash: code_hash.clone(),
        join: dummy_join(&bob.identity),
    })
    .await;
    let result = bob
        .recv_until(|message| matches!(message, ServerToClient::PairResult { .. }))
        .await;
    if let ServerToClient::PairResult { ok, message, .. } = result {
        assert!(!ok);
        assert!(
            message.contains("pair_not_found"),
            "过期登记应返回 pair_not_found: {message}"
        );
    }
    relay.shutdown().await;
}

#[tokio::test]
async fn pair_join_failures_are_rate_limited() {
    let relay = spawn_relay_with(|config| config.pair_join_limit_per_min = 2).await;
    let _alice = Client::connect(relay.addr).await;
    let mut bob = Client::connect(relay.addr).await;

    for _ in 0..2 {
        bob.send(&ClientToServer::PairJoin {
            code_hash: random_hex(8),
            join: dummy_join(&bob.identity),
        })
        .await;
        let result = bob
            .recv_until(|message| matches!(message, ServerToClient::PairResult { .. }))
            .await;
        if let ServerToClient::PairResult { ok, message, .. } = result {
            assert!(!ok);
            assert!(message.contains("pair_not_found"), "message: {message}");
        }
    }

    bob.send(&ClientToServer::PairJoin {
        code_hash: random_hex(8),
        join: dummy_join(&bob.identity),
    })
    .await;
    let result = bob
        .recv_until(|message| matches!(message, ServerToClient::PairResult { .. }))
        .await;
    if let ServerToClient::PairResult { ok, message, .. } = result {
        assert!(!ok);
        assert!(
            message.contains("rate_limited"),
            "第三次失败应被限流: {message}"
        );
    }
    relay.shutdown().await;
}

#[tokio::test]
async fn pair_announce_rejects_mismatched_invite_device() {
    let relay = spawn_relay().await;
    let mut alice = Client::connect(relay.addr).await;
    let mallory = Identity::generate("mallory");
    let initiator = PairingInitiator::start(&mallory, "ws://relay.test");
    alice
        .send(&ClientToServer::PairAnnounce {
            code_hash: initiator.code_hash(),
            invite: initiator.invite.clone(),
            ttl_secs: 120,
        })
        .await;
    let error = alice
        .recv_until(|message| matches!(message, ServerToClient::Error { .. }))
        .await;
    if let ServerToClient::Error { code, .. } = error {
        assert_eq!(code, "protocol_error");
    }
    relay.shutdown().await;
}

// ---------------------------------------------------------------------------
// E2E 握手与密文转发
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forwards_session_init_ack_and_encrypted_relay() {
    let relay = spawn_relay().await;
    let mut alice = Client::connect(relay.addr).await;
    let mut bob = Client::connect(relay.addr).await;
    let alice_id = alice.device_id.clone();
    let bob_id = bob.device_id.clone();

    // A → 中继 → B：session_init
    let init = create_session_init(&alice.identity, &bob.identity.public()).expect("创建握手");
    let init_msg = init.msg.clone();
    alice
        .send(&ClientToServer::SessionInit {
            to: bob_id.clone(),
            msg: init_msg.clone(),
        })
        .await;
    let forwarded = bob
        .recv_until(|message| matches!(message, ServerToClient::SessionInit { .. }))
        .await;
    if let ServerToClient::SessionInit { from, msg } = forwarded {
        assert_eq!(from, alice_id);
        assert_eq!(msg.content.session_id, init_msg.content.session_id);
        assert_eq!(msg.content.from, alice_id);
        assert_eq!(msg.content.to, bob_id);
    }

    // B → 中继 → A：session_ack
    let (ack, mut session_b) =
        accept_session_init(&bob.identity, &init_msg, &alice.identity.public()).expect("接受握手");
    bob.send(&ClientToServer::SessionAck {
        to: alice_id.clone(),
        msg: ack.clone(),
    })
    .await;
    let forwarded = alice
        .recv_until(|message| matches!(message, ServerToClient::SessionAck { .. }))
        .await;
    if let ServerToClient::SessionAck { from, msg } = forwarded {
        assert_eq!(from, bob_id);
        assert_eq!(msg.content.session_id, init_msg.content.session_id);
    }

    // A → 中继 → B：relay 密文信封（中继不解析，端到端仍可解密）
    let mut session_a =
        complete_session(&alice.identity, &init, &ack, &bob.identity.public()).expect("完成握手");
    let envelope = session_a.encrypt("peer", b"hello-bob").expect("加密");
    alice
        .send(&ClientToServer::Relay {
            to: bob_id.clone(),
            envelope: envelope.clone(),
        })
        .await;
    let forwarded = bob
        .recv_until(|message| matches!(message, ServerToClient::Relay { .. }))
        .await;
    if let ServerToClient::Relay { from, envelope } = forwarded {
        assert_eq!(from, alice_id);
        let (kind, plaintext) = session_b.decrypt(&envelope).expect("B 端解密");
        assert_eq!(kind, "peer");
        assert_eq!(plaintext, b"hello-bob".to_vec());
    }

    // to == self 拒绝。
    alice
        .send(&ClientToServer::Relay {
            to: alice_id.clone(),
            envelope: envelope.clone(),
        })
        .await;
    let error = alice
        .recv_until(|message| matches!(message, ServerToClient::Error { .. }))
        .await;
    if let ServerToClient::Error { code, .. } = error {
        assert_eq!(code, "protocol_error");
    }

    // 目标离线：不缓存，直接报 peer_offline。
    let offline_envelope = E2eEnvelope {
        v: 1,
        session_id: "0123456789abcdef01234567".to_string(),
        seq: 0,
        kind: "peer".to_string(),
        ts_ms: now_ms(),
        ciphertext: vec![1, 2, 3, 4],
    };
    alice
        .send(&ClientToServer::Relay {
            to: "dev-0000000000000000".to_string(),
            envelope: offline_envelope,
        })
        .await;
    let error = alice
        .recv_until(|message| matches!(message, ServerToClient::Error { .. }))
        .await;
    if let ServerToClient::Error { code, .. } = error {
        assert_eq!(code, "peer_offline");
    }
    relay.shutdown().await;
}

// ---------------------------------------------------------------------------
// 限额：限速 / 空闲超时 / 帧上限
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rate_limit_backpressure_forwards_all_frames_without_dropping() {
    // 50 帧/s、burst 200、背压等待上限 2s：260 帧中后 60 帧各需等待约 20ms，
    // 应全部转发，绝不出现 rate_limited（不丢帧）。
    let relay = spawn_relay_with(|config| {
        config.rate_limit_per_sec = 50;
        config.rate_limit_burst = 200;
        config.rate_limit_max_wait_ms = 2000;
    })
    .await;
    let mut client = Client::connect(relay.addr).await;
    let frames = 260usize;
    for ts_ms in 0..frames as u64 {
        client.send(&ClientToServer::Ping { ts_ms }).await;
    }
    let started = std::time::Instant::now();
    let mut pongs = 0usize;
    while pongs < frames {
        match client.next().await {
            Event::Message(ServerToClient::Pong { .. }) => pongs += 1,
            Event::Message(ServerToClient::Error { code, .. }) if code == "rate_limited" => {
                panic!("背压路径不应丢弃帧或返回 rate_limited")
            }
            Event::Message(_) => {}
            Event::Closed => panic!("连接被关闭"),
            Event::Timeout => panic!("等待背压转发超时（已收到 {pongs}/{frames} 帧）"),
        }
    }
    assert_eq!(pongs, frames, "所有帧都必须经背压后转发");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "背压转发耗时异常: {elapsed:?}"
    );
    relay.shutdown().await;
}

#[tokio::test]
async fn rate_limit_backpressure_timeout_drops_frame_with_error() {
    // 1 帧/s、burst 1、等待上限 1ms：补令牌需 1s，必然超时。
    // 期望：仅第 1 帧通过（1 个 pong），后 2 帧被丢弃并各回 1 个 rate_limited。
    let relay = spawn_relay_with(|config| {
        config.rate_limit_per_sec = 1;
        config.rate_limit_burst = 1;
        config.rate_limit_max_wait_ms = 1;
    })
    .await;
    let mut client = Client::connect(relay.addr).await;
    for ts_ms in 0..3 {
        client.send(&ClientToServer::Ping { ts_ms }).await;
    }

    let mut pongs = 0usize;
    let mut rate_limited = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while pongs + rate_limited < 3 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "等待限流响应超时");
        match tokio::time::timeout(remaining, client.ws.next()).await {
            Err(_) => panic!("等待限流响应超时"),
            Ok(None) => panic!("连接提前关闭"),
            Ok(Some(Err(error))) => panic!("连接错误: {error}"),
            Ok(Some(Ok(Message::Text(text)))) => {
                let message = ServerToClient::from_json(&text).expect("解析中继消息");
                match message {
                    ServerToClient::Pong { .. } => pongs += 1,
                    ServerToClient::Error { code, .. } => {
                        assert_eq!(code, "rate_limited", "超时丢弃应返回 rate_limited");
                        rate_limited += 1;
                    }
                    _ => {}
                }
            }
            Ok(Some(Ok(_))) => {}
        }
    }
    assert_eq!(pongs, 1, "burst=1 时只允许 1 帧通过");
    assert_eq!(rate_limited, 2, "背压等待超时的 2 帧应被丢弃并回 rate_limited");
    relay.shutdown().await;
}

/// 默认配置下 64KiB 分块流应全部转发、不触发限流丢帧。
///
/// 默认 2000 帧/s × 64 KiB ≈ 125 MiB/s（容量推导见 lib.rs 单测），本测试验证
/// "默认路径能持续吸收 64KiB 帧"（release 绝对吞吐由 QA 的 derive-bench 测量）。
#[tokio::test]
async fn default_rate_limit_handles_64kib_chunk_stream() {
    let relay = spawn_relay().await;
    let mut alice = Client::connect(relay.addr).await;
    let bob = Client::connect(relay.addr).await;
    let bob_id = bob.device_id.clone();
    let frames = 300usize;
    let chunk = vec![7u8; 64 * 1024];

    let reader = tokio::spawn(async move {
        let mut bob = bob;
        let mut received = 0usize;
        while received < frames {
            let message = bob
                .recv_until(|message| matches!(message, ServerToClient::Relay { .. }))
                .await;
            if let ServerToClient::Relay { envelope, .. } = message {
                assert_eq!(envelope.ciphertext.len(), 64 * 1024);
            }
            received += 1;
        }
        received
    });

    let started = std::time::Instant::now();
    for seq in 0..frames as u64 {
        let envelope = E2eEnvelope {
            v: 1,
            session_id: "0123456789abcdef01234567".to_string(),
            seq,
            kind: "peer".to_string(),
            ts_ms: now_ms(),
            ciphertext: chunk.clone(),
        };
        alice
            .send(&ClientToServer::Relay {
                to: bob_id.clone(),
                envelope,
            })
            .await;
    }
    let received = reader.await.expect("读取任务不应 panic");
    let elapsed = started.elapsed();
    assert_eq!(received, frames, "默认配置下 64KiB 分块不应丢帧");
    let mib_per_sec = (frames as f64 * chunk.len() as f64) / 1024.0 / 1024.0
        / elapsed.as_secs_f64().max(0.001);
    // debug 构建下给宽松下限（release 实测远高于此）；绝对 >20MB/s 目标由
    // `default_rate_limit_meets_large_file_throughput_target` 单测按容量推导保证。
    assert!(
        mib_per_sec >= 5.0,
        "64KiB 分块转发吞吐过低: {mib_per_sec:.1} MiB/s（elapsed={elapsed:?}）"
    );
    relay.shutdown().await;
}

#[tokio::test]
async fn idle_timeout_closes_connection() {
    let relay = spawn_relay_with(|config| config.idle_timeout_secs = 1).await;
    let mut client = Client::connect(relay.addr).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    client.expect_closed(Duration::from_secs(3)).await;
    relay.shutdown().await;
}

/// 回归（qa 阶段 2 发现）：`RelayHandle::shutdown().await` 必须关闭已建立的 WS 连接，
/// 否则嵌入式/热重启场景下客户端无法感知断线、不会自动重连。
#[tokio::test]
async fn graceful_shutdown_closes_established_connections() {
    let relay = spawn_relay().await;
    let addr = relay.addr;
    let mut alice = Client::connect(addr).await;
    let mut bob = Client::connect(addr).await;

    let started = std::time::Instant::now();
    relay.shutdown().await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "关停不应被在线连接拖住（耗时 {:?}）",
        started.elapsed()
    );

    // 两个客户端都应收到 Close(1001 going away)。
    let alice_code = wait_for_close_code(&mut alice.ws, Duration::from_secs(2)).await;
    let bob_code = wait_for_close_code(&mut bob.ws, Duration::from_secs(2)).await;
    assert_eq!(alice_code, Some(1001), "客户端应收到 Close(1001)");
    assert_eq!(bob_code, Some(1001), "客户端应收到 Close(1001)");

    // 关停后不再接受新连接（同一端口重启由部署方/测试另起实例）。
    let url = format!("ws://{addr}/ws");
    assert!(
        connect_async(url.as_str()).await.is_err(),
        "关停后不应再接受新连接"
    );
}

/// 连接正处于限流背压等待（max_wait 10s）时，关停必须立即打断等待并关闭连接，
/// 不能被背压等待拖住（否则嵌入式热重启最坏要等满 max_wait）。
#[tokio::test]
async fn graceful_shutdown_interrupts_backpressure_wait() {
    let relay = spawn_relay_with(|config| {
        config.rate_limit_per_sec = 1;
        config.rate_limit_burst = 1;
        config.rate_limit_max_wait_ms = 10_000;
    })
    .await;
    let mut client = Client::connect(relay.addr).await;
    // 第 1 帧消耗唯一令牌；第 2 帧进入背压等待（上限 10s）。
    client.send(&ClientToServer::Ping { ts_ms: 0 }).await;
    client.send(&ClientToServer::Ping { ts_ms: 1 }).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let started = std::time::Instant::now();
    relay.shutdown().await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "关停被背压等待拖住（耗时 {:?}）",
        started.elapsed()
    );
    let code = wait_for_close_code(&mut client.ws, Duration::from_secs(2)).await;
    assert_eq!(code, Some(1001), "背压中的连接也应收到 Close(1001)");
}

#[tokio::test]
async fn oversized_frame_closes_connection() {
    let relay = spawn_relay_with(|config| config.max_frame_bytes = 1024).await;
    let mut client = Client::connect(relay.addr).await;
    let _ = client.ws.send(Message::Text("x".repeat(4096))).await;
    client.expect_closed(Duration::from_secs(3)).await;
    relay.shutdown().await;
}

// ---------------------------------------------------------------------------
// 可选加密暂存
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vault_put_get_list_delete_and_limits() {
    let dir = tempfile::tempdir().expect("临时目录");
    let relay = spawn_relay_with(|config| {
        config.vault_dir = Some(dir.path().to_path_buf());
        config.vault_max_object_bytes = 16;
        config.vault_quota_bytes = 24;
        config.vault_max_objects = 2;
    })
    .await;
    let mut alice = Client::connect(relay.addr).await;
    let mut bob = Client::connect(relay.addr).await;

    // 上传 12 字节密文。
    let ciphertext = vec![7u8; 12];
    alice
        .send(&ClientToServer::VaultPut {
            object_id: "obj-1".to_string(),
            meta: vault_meta("secret.bin", 12),
            ciphertext: ciphertext.clone(),
            ttl_secs: 3600,
        })
        .await;
    let result = alice
        .recv_until(|message| matches!(message, ServerToClient::VaultPutResult { .. }))
        .await;
    let expires_at_ms = match result {
        ServerToClient::VaultPutResult {
            ok,
            object_id,
            expires_at_ms,
            message,
        } => {
            assert!(ok, "上传失败: {message}");
            assert_eq!(object_id, "obj-1");
            assert!(expires_at_ms > now_ms());
            expires_at_ms
        }
        other => panic!("收到 {other:?}"),
    };
    assert!(
        expires_at_ms >= now_ms() + 3_500_000,
        "TTL 3600s 的过期时间不合理"
    );
    assert!(dir.path().join("obj-1.bin").exists(), "密文必须落盘");
    assert!(dir.path().join("index.json").exists(), "索引必须落盘");

    // 跨设备下载（配对设备共享密钥后可解密，中继仍只返回密文）。
    bob.send(&ClientToServer::VaultGet {
        object_id: "obj-1".to_string(),
    })
    .await;
    let result = bob
        .recv_until(|message| matches!(message, ServerToClient::VaultGetResult { .. }))
        .await;
    if let ServerToClient::VaultGetResult {
        ok,
        ciphertext: got,
        meta,
        message,
        ..
    } = result
    {
        assert!(ok, "下载失败: {message}");
        assert_eq!(got.expect("应返回密文"), ciphertext);
        assert_eq!(meta.expect("应返回 meta").name, "secret.bin");
    }

    // 列表只列本设备对象。
    alice.send(&ClientToServer::VaultList).await;
    let result = alice
        .recv_until(|message| matches!(message, ServerToClient::VaultListResult { .. }))
        .await;
    if let ServerToClient::VaultListResult { ok, objects, .. } = result {
        assert!(ok);
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].object_id, "obj-1");
        assert_eq!(objects[0].ciphertext_len, 12);
    }
    bob.send(&ClientToServer::VaultList).await;
    let result = bob
        .recv_until(|message| matches!(message, ServerToClient::VaultListResult { .. }))
        .await;
    if let ServerToClient::VaultListResult { ok, objects, .. } = result {
        assert!(ok);
        assert!(objects.is_empty(), "其他设备不应看到本设备对象列表");
    }

    // 配额：12 + 12 = 24 字节正好达上限，第三个对象超限。
    alice
        .send(&ClientToServer::VaultPut {
            object_id: "obj-2".to_string(),
            meta: vault_meta("two.bin", 12),
            ciphertext: vec![8u8; 12],
            ttl_secs: 3600,
        })
        .await;
    let result = alice
        .recv_until(|message| matches!(message, ServerToClient::VaultPutResult { .. }))
        .await;
    if let ServerToClient::VaultPutResult { ok, message, .. } = result {
        assert!(ok, "第二次上传失败: {message}");
    }

    alice
        .send(&ClientToServer::VaultPut {
            object_id: "obj-3".to_string(),
            meta: vault_meta("three.bin", 1),
            ciphertext: vec![9u8; 1],
            ttl_secs: 3600,
        })
        .await;
    let result = alice
        .recv_until(|message| matches!(message, ServerToClient::VaultPutResult { .. }))
        .await;
    if let ServerToClient::VaultPutResult { ok, message, .. } = result {
        assert!(!ok, "超过配额必须失败");
        assert!(
            message.contains("vault_quota_exceeded"),
            "message: {message}"
        );
    }

    // 单对象上限。
    alice
        .send(&ClientToServer::VaultPut {
            object_id: "obj-big".to_string(),
            meta: vault_meta("big.bin", 17),
            ciphertext: vec![0u8; 17],
            ttl_secs: 3600,
        })
        .await;
    let result = alice
        .recv_until(|message| matches!(message, ServerToClient::VaultPutResult { .. }))
        .await;
    if let ServerToClient::VaultPutResult { ok, message, .. } = result {
        assert!(!ok);
        assert!(message.contains("payload_too_large"), "message: {message}");
    }

    // 删除后不可再下载。
    alice
        .send(&ClientToServer::VaultDelete {
            object_id: "obj-1".to_string(),
        })
        .await;
    let result = alice
        .recv_until(|message| matches!(message, ServerToClient::VaultDeleteResult { .. }))
        .await;
    if let ServerToClient::VaultDeleteResult { ok, message, .. } = result {
        assert!(ok, "删除失败: {message}");
    }
    assert!(!dir.path().join("obj-1.bin").exists());
    bob.send(&ClientToServer::VaultGet {
        object_id: "obj-1".to_string(),
    })
    .await;
    let result = bob
        .recv_until(|message| matches!(message, ServerToClient::VaultGetResult { .. }))
        .await;
    if let ServerToClient::VaultGetResult { ok, message, .. } = result {
        assert!(!ok);
        assert!(message.contains("vault_not_found"), "message: {message}");
    }
    relay.shutdown().await;
}

#[tokio::test]
async fn vault_ttl_is_clamped_to_minimum() {
    let dir = tempfile::tempdir().expect("临时目录");
    let relay = spawn_relay_with(|config| {
        config.vault_dir = Some(dir.path().to_path_buf());
    })
    .await;
    let mut alice = Client::connect(relay.addr).await;
    alice
        .send(&ClientToServer::VaultPut {
            object_id: "obj-ttl".to_string(),
            meta: vault_meta("ttl.bin", 1),
            ciphertext: vec![1u8; 1],
            ttl_secs: 1,
        })
        .await;
    let result = alice
        .recv_until(|message| matches!(message, ServerToClient::VaultPutResult { .. }))
        .await;
    if let ServerToClient::VaultPutResult {
        ok,
        expires_at_ms,
        message,
        ..
    } = result
    {
        assert!(ok, "上传失败: {message}");
        assert!(
            expires_at_ms >= now_ms() + 55_000,
            "TTL 必须被 clamp 到至少 60s（expires_at_ms={expires_at_ms}）"
        );
    }
    relay.shutdown().await;
}

#[tokio::test]
async fn vault_survives_restart() {
    let dir = tempfile::tempdir().expect("临时目录");
    let vault_dir = dir.path().to_path_buf();

    let relay = spawn_relay_with(|config| {
        config.vault_dir = Some(vault_dir.clone());
    })
    .await;
    let mut alice = Client::connect(relay.addr).await;
    alice
        .send(&ClientToServer::VaultPut {
            object_id: "obj-persist".to_string(),
            meta: vault_meta("persist.bin", 4),
            ciphertext: b"data".to_vec(),
            ttl_secs: 3600,
        })
        .await;
    let result = alice
        .recv_until(|message| matches!(message, ServerToClient::VaultPutResult { .. }))
        .await;
    if let ServerToClient::VaultPutResult { ok, message, .. } = result {
        assert!(ok, "上传失败: {message}");
    }
    relay.shutdown().await;

    // 重启后仍可读取（索引持久化生效）。
    let relay = spawn_relay_with(|config| {
        config.vault_dir = Some(vault_dir.clone());
    })
    .await;
    let mut bob = Client::connect(relay.addr).await;
    bob.send(&ClientToServer::VaultGet {
        object_id: "obj-persist".to_string(),
    })
    .await;
    let result = bob
        .recv_until(|message| matches!(message, ServerToClient::VaultGetResult { .. }))
        .await;
    if let ServerToClient::VaultGetResult {
        ok,
        ciphertext,
        message,
        ..
    } = result
    {
        assert!(ok, "重启后下载失败: {message}");
        assert_eq!(ciphertext.expect("密文"), b"data".to_vec());
    }
    relay.shutdown().await;
}

#[tokio::test]
async fn vault_disabled_returns_vault_disabled_message() {
    let relay = spawn_relay().await;
    let mut alice = Client::connect(relay.addr).await;
    alice
        .send(&ClientToServer::VaultPut {
            object_id: "obj-1".to_string(),
            meta: vault_meta("x.bin", 1),
            ciphertext: vec![1u8; 1],
            ttl_secs: 60,
        })
        .await;
    let result = alice
        .recv_until(|message| matches!(message, ServerToClient::VaultPutResult { .. }))
        .await;
    if let ServerToClient::VaultPutResult { ok, message, .. } = result {
        assert!(!ok);
        assert!(message.contains("vault_disabled"), "message: {message}");
    }
    alice.send(&ClientToServer::VaultList).await;
    let result = alice
        .recv_until(|message| matches!(message, ServerToClient::VaultListResult { .. }))
        .await;
    if let ServerToClient::VaultListResult { ok, message, .. } = result {
        assert!(!ok);
        assert!(message.contains("vault_disabled"), "message: {message}");
    }
    relay.shutdown().await;
}

#[tokio::test]
async fn vault_rejects_path_traversal_object_id() {
    let dir = tempfile::tempdir().expect("临时目录");
    let relay = spawn_relay_with(|config| {
        config.vault_dir = Some(dir.path().to_path_buf());
    })
    .await;
    let mut alice = Client::connect(relay.addr).await;
    alice
        .send(&ClientToServer::VaultPut {
            object_id: "../escape".to_string(),
            meta: vault_meta("x.bin", 1),
            ciphertext: vec![1u8; 1],
            ttl_secs: 3600,
        })
        .await;
    let result = alice
        .recv_until(|message| matches!(message, ServerToClient::VaultPutResult { .. }))
        .await;
    if let ServerToClient::VaultPutResult { ok, message, .. } = result {
        assert!(!ok);
        assert!(message.contains("protocol_error"), "message: {message}");
    }
    relay.shutdown().await;
}
