//! Engine 单元测试：用内存"模拟中继"驱动两个真实 Engine 实例，
//! 覆盖剪切板回环抑制、事件去重、离线 spool 重发、目录合并/持久化、传输断点续传。

use std::collections::VecDeque;
use std::sync::Arc;

use derive_core::catalog::TargetPlatform;
use derive_core::clipboard::{ClipboardEvent, ClipboardPayload};
use derive_core::pairing::{PairRecord, PAIRING_VERSION};
use derive_core::peer::PeerMessage;
use derive_core::wire::{ClientToServer, ServerToClient};
use tempfile::TempDir;

use super::*;
use crate::store::Store;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    A,
    B,
}

impl Side {
    fn other(self) -> Side {
        match self {
            Side::A => Side::B,
            Side::B => Side::A,
        }
    }
}

#[derive(Default)]
struct Settled {
    applied_a: Vec<ClipboardPayload>,
    applied_b: Vec<ClipboardPayload>,
    events_a: Vec<RuntimeEvent>,
    events_b: Vec<RuntimeEvent>,
}

/// 两个已配对 Engine + 内存中继。
struct Harness {
    a: Engine,
    b: Engine,
    a_id: String,
    b_id: String,
    _dir_a: TempDir,
    _dir_b: TempDir,
}

impl Harness {
    fn new() -> Self {
        let dir_a = TempDir::new().expect("创建临时目录");
        let dir_b = TempDir::new().expect("创建临时目录");
        let store_a = Arc::new(Store::open(dir_a.path()).expect("打开 store A"));
        let store_b = Arc::new(Store::open(dir_b.path()).expect("打开 store B"));
        let (identity_a, _) = store_a.load_or_create_identity("设备A").expect("创建身份 A");
        let (identity_b, _) = store_b.load_or_create_identity("设备B").expect("创建身份 B");

        let record_for_a = PairRecord {
            v: PAIRING_VERSION,
            peer: identity_b.public(),
            relay_url: "ws://test/ws".into(),
            paired_at_ms: 1,
            label: "设备B".into(),
            pairing_fingerprint: "fp-test".into(),
        };
        let record_for_b = PairRecord {
            v: PAIRING_VERSION,
            peer: identity_a.public(),
            relay_url: "ws://test/ws".into(),
            paired_at_ms: 1,
            label: "设备A".into(),
            pairing_fingerprint: "fp-test".into(),
        };
        store_a.upsert_peer(record_for_a).expect("写入配对 A");
        store_b.upsert_peer(record_for_b).expect("写入配对 B");

        let a = Engine::new(store_a, identity_a).expect("创建 engine A");
        let b = Engine::new(store_b, identity_b).expect("创建 engine B");
        let a_id = a.self_id().to_string();
        let b_id = b.self_id().to_string();
        Self {
            a,
            b,
            a_id,
            b_id,
            _dir_a: dir_a,
            _dir_b: dir_b,
        }
    }

    fn connection_info(&self, side: Side) -> ConnectionInfo {
        match side {
            Side::A => ConnectionInfo {
                device_id: self.a_id.clone(),
                session_id: "relay-session".into(),
                online: vec![self.a_id.clone(), self.b_id.clone()],
                server_time_ms: 0,
            },
            Side::B => ConnectionInfo {
                device_id: self.b_id.clone(),
                session_id: "relay-session".into(),
                online: vec![self.a_id.clone(), self.b_id.clone()],
                server_time_ms: 0,
            },
        }
    }

    fn engine(&self, side: Side) -> &Engine {
        match side {
            Side::A => &self.a,
            Side::B => &self.b,
        }
    }

    fn engine_mut(&mut self, side: Side) -> &mut Engine {
        match side {
            Side::A => &mut self.a,
            Side::B => &mut self.b,
        }
    }

    /// 模拟一次完整连接：双方收到 welcome/presence → 握手 → 目录快照/have_list。
    fn handshake(&mut self) -> Settled {
        let info_a = self.connection_info(Side::A);
        let info_b = self.connection_info(Side::B);
        let out_a = self.a.on_authenticated(&info_a);
        let out_b = self.b.on_authenticated(&info_b);
        let mut queue = self.queue_from(Side::A, out_a);
        queue.extend(self.queue_from(Side::B, out_b));
        self.settle(queue)
    }

    fn queue_from(
        &self,
        from: Side,
        output: EngineOutput,
    ) -> VecDeque<(Side, ClientToServer)> {
        output
            .outbound
            .into_iter()
            .map(|message| (from.other(), message))
            .collect()
    }

    /// 把 `from` 的输出投递给对端，并递归处理由此产生的所有消息（模拟中继在两条连接间转发）。
    fn settle(&mut self, mut queue: VecDeque<(Side, ClientToServer)>) -> Settled {
        let mut settled = Settled::default();
        let mut guard = 0usize;
        while let Some((receiver, message)) = queue.pop_front() {
            guard += 1;
            assert!(guard < 100_000, "模拟中继消息循环疑似死循环");
            let from_id = match receiver {
                Side::A => self.b_id.clone(),
                Side::B => self.a_id.clone(),
            };
            let Some(server) = to_server_message(message, &from_id) else {
                continue;
            };
            let output = match receiver {
                Side::A => self.a.handle_server(server),
                Side::B => self.b.handle_server(server),
            };
            match receiver {
                Side::A => {
                    settled.applied_a.extend(output.applied_clipboard);
                    settled.events_a.extend(output.events);
                }
                Side::B => {
                    settled.applied_b.extend(output.applied_clipboard);
                    settled.events_b.extend(output.events);
                }
            }
            for reply in output.outbound {
                queue.push_back((receiver.other(), reply));
            }
        }
        settled
    }

    /// `from` 的输出交给对端，返回对端产生的输出（不做进一步转发）。
    fn deliver(&mut self, target: Side, messages: Vec<ClientToServer>, from: Side) -> EngineOutput {
        let from_id = match from {
            Side::A => self.a_id.clone(),
            Side::B => self.b_id.clone(),
        };
        let mut output = EngineOutput::default();
        for message in messages {
            if let Some(server) = to_server_message(message, &from_id) {
                let produced = match target {
                    Side::A => self.a.handle_server(server),
                    Side::B => self.b.handle_server(server),
                };
                output.merge(produced);
            }
        }
        output
    }

    /// 从 `from` 开始自动转发全部消息。
    fn pump(&mut self, from: Side, output: EngineOutput) -> Settled {
        let queue = self.queue_from(from, output);
        self.settle(queue)
    }

    /// 模拟连接断开（两端都收到连接关闭）。
    fn disconnect(&mut self) {
        let _ = self.a.on_disconnect();
        let _ = self.b.on_disconnect();
    }

    fn add_file(&mut self, file_name: &str, data: &[u8]) -> (CatalogEntry, TempDir) {
        let dir = TempDir::new().expect("创建临时文件目录");
        let path = dir.path().join(file_name);
        std::fs::write(&path, data).expect("写临时文件");
        let mut params = CatalogAddParams::new(&path);
        params.name = file_name.to_string();
        params.version = "1.0.0".into();
        params.kind = "file".into();
        params.platform = TargetPlatform::Any;
        let (entry, output) = self.a.catalog_add_local(params).expect("添加目录条目");
        assert!(
            output.events.iter().any(|event| matches!(
                event,
                RuntimeEvent::CatalogUpdated { entry_id, .. } if entry_id == &entry.id
            )),
            "应产生目录更新事件"
        );
        self.pump(Side::A, output);
        (entry, dir)
    }
}

fn to_server_message(message: ClientToServer, from: &str) -> Option<ServerToClient> {
    match message {
        ClientToServer::SessionInit { msg, .. } => Some(ServerToClient::SessionInit {
            from: from.to_string(),
            msg,
        }),
        ClientToServer::SessionAck { msg, .. } => Some(ServerToClient::SessionAck {
            from: from.to_string(),
            msg,
        }),
        ClientToServer::Relay { envelope, .. } => Some(ServerToClient::Relay {
            from: from.to_string(),
            envelope,
        }),
        _ => None,
    }
}

fn clipboard_event(event_id: &str, origin: &str, payload: &ClipboardPayload) -> ClipboardEvent {
    ClipboardEvent {
        v: 1,
        event_id: event_id.to_string(),
        origin_device: origin.to_string(),
        origin_seq: 1,
        created_at_ms: 1,
        payload: payload.clone(),
    }
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

#[test]
fn handshake_establishes_one_session_on_both_sides() {
    let mut harness = Harness::new();
    let settled = harness.handshake();
    assert_eq!(harness.a.sessions().len(), 1, "A 应有一个会话");
    assert_eq!(harness.b.sessions().len(), 1, "B 应有一个会话");
    assert!(settled
        .events_a
        .iter()
        .any(|event| matches!(event, RuntimeEvent::SessionEstablished { .. })));
    assert!(settled
        .events_b
        .iter()
        .any(|event| matches!(event, RuntimeEvent::SessionEstablished { .. })));
    // 双方互发 snapshot / have_list；空目录也应有 HaveList 事件。
    assert!(settled
        .events_a
        .iter()
        .any(|event| matches!(event, RuntimeEvent::PeerHaveList { .. })));
}

#[test]
fn clipboard_sync_and_loopback_suppressed() {
    let mut harness = Harness::new();
    harness.handshake();

    let payload = ClipboardPayload::text("你好，derive-sync!").expect("构造文本载荷");
    let hash = payload.hash.clone();
    let output = harness.a.on_local_clipboard(payload.clone());
    assert!(output.outbound.is_empty(), "去抖窗口内不应发送");
    assert!(harness.a.has_pending_local_clipboard());

    // 未到去抖截止时间不发送。
    let output = harness.a.on_tick(now_ms());
    assert!(output.outbound.is_empty());

    // 100ms 后广播到 B。
    let output = harness.a.on_tick(now_ms() + 200);
    assert!(!output.outbound.is_empty(), "去抖到期后应广播");
    let settled = harness.pump(Side::A, output);
    assert_eq!(settled.applied_b.len(), 1);
    assert_eq!(
        settled.applied_b[0].as_text().expect("文本"),
        "你好，derive-sync!"
    );
    assert!(
        harness.b.recent_applied_contains(&hash),
        "B 应记录已应用的哈希"
    );

    // B 的本地监听器读到刚应用的内容 → 回环抑制，不上报。
    let echo = harness.b.on_local_clipboard(settled.applied_b[0].clone());
    assert!(echo.outbound.is_empty(), "回环内容不应再上报");
    let echo_tick = harness.b.on_tick(now_ms() + 500);
    assert!(echo_tick.outbound.is_empty(), "回环内容不应再发送");
    assert_eq!(harness.b.spool_count(), 0);

    // B 再发一条不同内容 → A 正常接收。
    let payload2 = ClipboardPayload::text("second message").expect("构造文本载荷");
    let _ = harness.b.on_local_clipboard(payload2);
    let output = harness.b.on_tick(now_ms() + 700);
    let settled = harness.pump(Side::B, output);
    assert_eq!(settled.applied_a.len(), 1);
    assert_eq!(settled.applied_a[0].as_text().expect("文本"), "second message");
}

#[test]
fn clipboard_event_dedupe_and_origin_filter() {
    let mut harness = Harness::new();
    harness.handshake();
    let payload = ClipboardPayload::text("dedupe-test").expect("构造文本载荷");

    // 收到自己 origin 的事件 → 直接丢弃。
    let own_event = clipboard_event("e-self", &harness.b_id, &payload);
    let output = harness
        .b
        .handle_peer_message(&harness.a_id, PeerMessage::Clipboard { event: own_event });
    assert!(output.applied_clipboard.is_empty());

    // 正常事件应用一次。
    let event = clipboard_event("e1", &harness.a_id, &payload);
    let output = harness
        .b
        .handle_peer_message(&harness.a_id, PeerMessage::Clipboard { event: event.clone() });
    assert_eq!(output.applied_clipboard.len(), 1);

    // 同一 event_id 重复投递 → 幂等，不再应用。
    let output = harness
        .b
        .handle_peer_message(&harness.a_id, PeerMessage::Clipboard { event: event.clone() });
    assert!(output.applied_clipboard.is_empty());

    // 同样内容的新 event_id（回环场景）→ recent_applied 命中，不再应用。
    let duplicate_content = clipboard_event("e2", &harness.a_id, &payload);
    let output = harness.b.handle_peer_message(
        &harness.a_id,
        PeerMessage::Clipboard {
            event: duplicate_content,
        },
    );
    assert!(output.applied_clipboard.is_empty());
}

#[test]
fn clipboard_tampered_payload_rejected() {
    let mut harness = Harness::new();
    harness.handshake();
    let mut payload = ClipboardPayload::text("integrity").expect("构造文本载荷");
    payload.hash = blake3::hash(b"other").to_hex().to_string();
    let event = clipboard_event("bad", &harness.a_id, &payload);
    let output = harness
        .b
        .handle_peer_message(&harness.a_id, PeerMessage::Clipboard { event });
    assert!(output.applied_clipboard.is_empty());
    assert!(output.events.iter().any(|event| matches!(event, RuntimeEvent::Error { .. })));
}

#[test]
fn offline_clipboard_events_are_spooled_and_replayed_in_order() {
    let mut harness = Harness::new();
    // 不握手：A 认为 B 离线。
    for (index, text) in ["first", "second", "third"].iter().enumerate() {
        let payload = ClipboardPayload::text(text).expect("构造文本载荷");
        let _ = harness.a.on_local_clipboard(payload);
        let output = harness.a.on_tick(now_ms() + 200 * (index as u64 + 1));
        assert!(output.outbound.is_empty(), "无会话时不得明文发送");
    }
    assert_eq!(harness.a.spool_count(), 3, "离线事件应进入 spool");

    // B 上线 → 握手后自动补发。
    let settled = harness.handshake();
    assert_eq!(harness.a.spool_count(), 0, "补发成功后应清空 spool");
    assert_eq!(settled.applied_b.len(), 3);
    assert_eq!(settled.applied_b[0].as_text().expect("文本"), "first");
    assert_eq!(settled.applied_b[1].as_text().expect("文本"), "second");
    assert_eq!(settled.applied_b[2].as_text().expect("文本"), "third");
    assert!(settled.events_a.iter().any(|event| matches!(
        event,
        RuntimeEvent::SpoolReplayed { count: 3, .. }
    )));
}

#[test]
fn catalog_add_merges_persists_and_delete_propagates() {
    let mut harness = Harness::new();
    harness.handshake();

    let (entry, _file_dir) = harness.add_file("tool.zip", b"fake installer content");
    assert!(harness.b.catalog().get(&entry.id).is_some(), "B 应合并到条目");

    // B 的目录已持久化，可被新 Engine 重新加载。
    let store_b = harness.b.store().clone();
    let (catalog_on_disk, revision) = store_b.load_catalog().expect("读取目录");
    assert!(catalog_on_disk.get(&entry.id).is_some());
    assert!(revision >= 1);
    let identity_b = harness.b.identity().clone();
    let reloaded = Engine::new(store_b.clone(), identity_b).expect("重新加载 engine");
    assert!(reloaded.catalog().get(&entry.id).is_some());

    // have_list 广播：B 应记录 A 持有的条目本体。
    let has_entry = harness
        .b
        .peer_have()
        .get(&harness.a_id)
        .map(|ids| ids.iter().any(|id| id == &entry.id))
        .unwrap_or(false);
    assert!(has_entry, "B 应收到 A 的 have_list");
    assert!(harness.a.local_index().contains_key(&entry.id));

    // 删除写墓碑并传播。
    let output = harness.a.catalog_remove_local(&entry.id).expect("删除条目");
    let settled = harness.pump(Side::A, output);
    assert!(harness.b.catalog().get(&entry.id).is_none(), "B 应删除条目");
    assert!(
        harness
            .b
            .catalog()
            .get_any(&entry.id)
            .map(|entry| entry.deleted)
            .unwrap_or(false),
        "B 应保留墓碑"
    );
    assert!(settled
        .events_b
        .iter()
        .any(|event| matches!(event, RuntimeEvent::CatalogMerged { applied, .. } if *applied >= 1)));

    // 重新加载后墓碑仍在（离线设备重连不会复活条目）。
    let (catalog_on_disk, _) = store_b.load_catalog().expect("读取目录");
    assert!(catalog_on_disk
        .get_any(&entry.id)
        .map(|entry| entry.deleted)
        .unwrap_or(false));
}

#[test]
fn catalog_delta_idempotent_merge() {
    let mut harness = Harness::new();
    harness.handshake();
    let (entry, _file_dir) = harness.add_file("dup.zip", b"dup");
    let snapshot_ops = harness.a.catalog().to_sync_ops();
    // 再次收到全量快照（幂等）：不应产生 applied，也不应复活/重复。
    let output = harness.b.handle_peer_message(
        &harness.a_id,
        PeerMessage::CatalogSnapshot {
            ops: snapshot_ops,
            revision: harness.a.catalog_revision(),
        },
    );
    match output
        .events
        .iter()
        .find(|event| matches!(event, RuntimeEvent::CatalogMerged { .. }))
    {
        Some(RuntimeEvent::CatalogMerged { applied, .. }) => assert_eq!(*applied, 0),
        other => panic!("缺少目录合并事件: {other:?}"),
    }
    assert!(harness.b.catalog().get(&entry.id).is_some());
}

#[test]
fn transfer_resumes_from_partial_file() {
    let mut harness = Harness::new();
    harness.handshake();

    // 6 MiB / 64 KiB = 96 块，超过 32 块发送窗口，便于制造"传一半断线"。
    let data: Vec<u8> = (0..6 * 1024 * 1024).map(|index| (index % 251) as u8).collect();
    let (entry, _file_dir) = harness.add_file("big.bin", &data);

    // B 请求 → A 报价 → B 接受 → A 发送满窗口 32 块。
    let fetch = harness.b.request_fetch(&harness.a_id, &entry.id);
    let offer = harness.deliver(Side::A, fetch.outbound, Side::B);
    let accept = harness.deliver(Side::B, offer.outbound, Side::A);
    let chunks = harness.deliver(Side::A, accept.outbound, Side::B);
    assert_eq!(
        chunks.outbound.len(),
        TRANSFER_WINDOW_BLOCKS as usize,
        "接受后应先发送一个窗口（32 块）"
    );

    // 只把前 8 块交给 B（512 KiB），其余模拟网络中断丢弃；B 的 ack 也丢弃。
    let mut produced = EngineOutput::default();
    for message in chunks.outbound.into_iter().take(8) {
        if let ClientToServer::Relay { envelope, .. } = message {
            let server = ServerToClient::Relay {
                from: harness.a_id.clone(),
                envelope,
            };
            produced.merge(harness.b.handle_server(server));
        }
    }
    assert!(
        produced
            .events
            .iter()
            .all(|event| !matches!(event, RuntimeEvent::TransferFailed { .. })),
        "前 8 块应正常写入"
    );
    assert!(
        !produced.outbound.is_empty(),
        "B 收到 8 块后应回 ack（流控）"
    );
    // 丢弃 ack，随后连接断开（.part 保留）。
    harness.disconnect();
    assert!(harness.a.sessions().is_empty());
    assert!(harness.b.sessions().is_empty());

    // 重连并重新 fetch：A 复用缓存的传输清单（同一 transfer_id），B 从 .part 续传。
    harness.handshake();
    let fetch = harness.b.request_fetch(&harness.a_id, &entry.id);
    let settled = harness.pump(Side::B, fetch);

    assert!(
        settled.events_b.iter().any(|event| matches!(
            event,
            RuntimeEvent::TransferStarted { from_offset, .. } if *from_offset > 0
        )),
        "应发生断点续传（from_offset > 0）"
    );
    let (path, size) = settled
        .events_b
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::TransferReceived { path, size, .. } => Some((path.clone(), *size)),
            _ => None,
        })
        .expect("B 应完成接收");
    assert_eq!(size, data.len() as u64);
    assert_eq!(std::fs::read(&path).expect("读取接收文件"), data);
    // 接收完成后本地索引更新，可再次提供给其他设备。
    assert!(harness.b.local_index().contains_key(&entry.id));
}

#[test]
fn transfer_hash_mismatch_is_reported() {
    let mut harness = Harness::new();
    harness.handshake();
    let data = b"original-content".repeat(1000);
    let (entry, file_dir) = harness.add_file("bad.bin", &data);

    // 发送端本地文件被篡改（大小不变、mtime 变化 → 重新报价但沿用索引中的旧哈希）。
    let path = file_dir.path().join("bad.bin");
    let mut tampered = data.clone();
    tampered[0] ^= 0xFF;
    std::fs::write(&path, &tampered).expect("篡改文件");
    // 确保 mtime 变化（部分文件系统时间精度较粗）。
    std::thread::sleep(std::time::Duration::from_millis(20));

    let fetch = harness.b.request_fetch(&harness.a_id, &entry.id);
    let settled = harness.pump(Side::B, fetch);
    assert!(
        settled
            .events_b
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TransferFailed { .. })),
        "哈希不一致应报告传输失败"
    );
    assert!(
        !settled
            .events_b
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TransferReceived { .. })),
        "校验失败不应产生接收完成事件"
    );
    // 不应留下最终文件。
    let incoming = harness.b.store().incoming_dir();
    let bad_files: Vec<_> = std::fs::read_dir(&incoming)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("bad.bin"))
                .collect()
        })
        .unwrap_or_default();
    assert!(bad_files.is_empty(), "校验失败不应生成最终文件");
}

// ---------------------------------------------------------------------------
// 多设备（3 引擎）回归测试：B、C 并发 fetch 同一 entry（曾经的 transfer_id 覆盖 bug）
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MultiSettled {
    applied: Vec<Vec<ClipboardPayload>>,
    events: Vec<Vec<RuntimeEvent>>,
}

impl MultiSettled {
    fn new(engines: usize) -> Self {
        Self {
            applied: (0..engines).map(|_| Vec::new()).collect(),
            events: (0..engines).map(|_| Vec::new()).collect(),
        }
    }

    fn merge(&mut self, other: MultiSettled) {
        for (index, mut applied) in other.applied.into_iter().enumerate() {
            self.applied[index].append(&mut applied);
        }
        for (index, mut events) in other.events.into_iter().enumerate() {
            self.events[index].append(&mut events);
        }
    }
}

/// 三设备内存中继：A 与 B、C 配对（B、C 之间不配对）。
struct MultiHarness {
    engines: Vec<Engine>,
    ids: Vec<String>,
    _dirs: Vec<TempDir>,
}

impl MultiHarness {
    fn new() -> Self {
        let dirs: Vec<TempDir> = (0..3).map(|_| TempDir::new().expect("临时目录")).collect();
        let stores: Vec<Arc<Store>> = dirs
            .iter()
            .map(|dir| Arc::new(Store::open(dir.path()).expect("打开 store")))
            .collect();
        let names = ["设备A", "设备B", "设备C"];
        let mut identities = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let (identity, _) = stores[index]
                .load_or_create_identity(name)
                .expect("创建身份");
            identities.push(identity);
        }
        let make_record = |peer: derive_core::identity::PublicIdentity| PairRecord {
            v: PAIRING_VERSION,
            peer,
            relay_url: "ws://test/ws".into(),
            paired_at_ms: 1,
            label: "peer".into(),
            pairing_fingerprint: "fp-multi".into(),
        };
        // A(0) <-> B(1)、A(0) <-> C(2)
        for other in [1usize, 2] {
            stores[0]
                .upsert_peer(make_record(identities[other].public()))
                .expect("写入 A 的配对");
            stores[other]
                .upsert_peer(make_record(identities[0].public()))
                .expect("写入对端配对");
        }
        let engines: Vec<Engine> = identities
            .into_iter()
            .enumerate()
            .map(|(index, identity)| Engine::new(stores[index].clone(), identity).expect("创建 engine"))
            .collect();
        let ids: Vec<String> = engines.iter().map(|engine| engine.self_id().to_string()).collect();
        Self {
            engines,
            ids,
            _dirs: dirs,
        }
    }

    fn index_of(&self, device_id: &str) -> Option<usize> {
        self.ids.iter().position(|id| id == device_id)
    }

    fn convert(
        &self,
        from: usize,
        message: ClientToServer,
    ) -> Option<(usize, ServerToClient)> {
        let from_id = self.ids[from].clone();
        match message {
            ClientToServer::SessionInit { to, msg } => Some((
                self.index_of(&to)?,
                ServerToClient::SessionInit { from: from_id, msg },
            )),
            ClientToServer::SessionAck { to, msg } => Some((
                self.index_of(&to)?,
                ServerToClient::SessionAck { from: from_id, msg },
            )),
            ClientToServer::Relay { to, envelope } => Some((
                self.index_of(&to)?,
                ServerToClient::Relay {
                    from: from_id,
                    envelope,
                },
            )),
            _ => None,
        }
    }

    /// 投递队列直到没有新消息；返回各引擎产生的事件/应用内容。
    fn settle(&mut self, mut queue: VecDeque<(usize, ServerToClient)>) -> MultiSettled {
        let mut settled = MultiSettled::new(self.engines.len());
        let mut guard = 0usize;
        while let Some((receiver, message)) = queue.pop_front() {
            guard += 1;
            assert!(guard < 200_000, "模拟中继消息循环疑似死循环");
            let output = self.engines[receiver].handle_server(message);
            settled.applied[receiver].extend(output.applied_clipboard);
            settled.events[receiver].extend(output.events);
            for reply in output.outbound {
                if let Some((target, server)) = self.convert(receiver, reply) {
                    queue.push_back((target, server));
                }
            }
        }
        settled
    }

    fn enqueue(&mut self, from: usize, output: EngineOutput, queue: &mut VecDeque<(usize, ServerToClient)>) {
        for reply in output.outbound {
            if let Some((target, server)) = self.convert(from, reply) {
                queue.push_back((target, server));
            }
        }
    }

    fn handshake(&mut self) -> MultiSettled {
        let online: Vec<Vec<String>> = vec![
            vec![self.ids[1].clone(), self.ids[2].clone()],
            vec![self.ids[0].clone()],
            vec![self.ids[0].clone()],
        ];
        let mut settled = MultiSettled::new(self.engines.len());
        let mut queue = VecDeque::new();
        for index in 0..self.engines.len() {
            let info = ConnectionInfo {
                device_id: self.ids[index].clone(),
                session_id: format!("relay-{index}"),
                online: online[index].clone(),
                server_time_ms: 0,
            };
            let output = self.engines[index].on_authenticated(&info);
            let EngineOutput {
                outbound,
                applied_clipboard,
                events,
            } = output;
            settled.events[index].extend(events);
            settled.applied[index].extend(applied_clipboard);
            for reply in outbound {
                if let Some((target, server)) = self.convert(index, reply) {
                    queue.push_back((target, server));
                }
            }
        }
        let rest = self.settle(queue);
        settled.merge(rest);
        settled
    }
}

#[test]
fn concurrent_fetch_same_entry_from_two_peers() {
    let mut harness = MultiHarness::new();
    harness.handshake();
    assert_eq!(harness.engines[0].sessions().len(), 2, "A 应分别与 B、C 建会话");

    // A 本地持有 1 MiB 文件并加入目录。
    let file_dir = TempDir::new().expect("临时文件目录");
    let data: Vec<u8> = (0..1024 * 1024).map(|index| (index % 251) as u8).collect();
    let path = file_dir.path().join("shared.bin");
    std::fs::write(&path, &data).expect("写测试文件");
    let mut params = CatalogAddParams::new(&path);
    params.name = "shared".into();
    params.version = "1.0.0".into();
    params.kind = "archive".into();
    params.platform = TargetPlatform::Any;
    let (entry, add_output) = harness.engines[0]
        .catalog_add_local(params)
        .expect("添加目录条目");
    let mut queue = VecDeque::new();
    harness.enqueue(0, add_output, &mut queue);
    let _ = harness.settle(queue);

    // B、C **同时**请求同一 entry（两条 fetch_request 一起进入 A）。
    let fetch_b = harness.engines[1].request_fetch(&harness.ids[0].clone(), &entry.id);
    let fetch_c = harness.engines[2].request_fetch(&harness.ids[0].clone(), &entry.id);
    let mut queue = VecDeque::new();
    harness.enqueue(1, fetch_b, &mut queue);
    harness.enqueue(2, fetch_c, &mut queue);
    let settled = harness.settle(queue);

    // A 必须为两个对端各发一次 offer，且 transfer_id 不同。
    let offered: Vec<String> = settled.events[0]
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::TransferOffered { transfer_id, .. } => Some(transfer_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(offered.len(), 2, "A 应对 B、C 各发一次 TransferOffer");
    assert_ne!(
        offered[0], offered[1],
        "并发 fetch 同一 entry 必须使用不同 transfer_id（否则 outgoing 会被覆盖）"
    );

    // B、C 都应完整收到同一文件。
    for device in [1usize, 2] {
        let received = settled.events[device]
            .iter()
            .find_map(|event| match event {
                RuntimeEvent::TransferReceived { path, size, .. } => Some((path.clone(), *size)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("设备 {device} 应完成接收"));
        assert_eq!(received.1, data.len() as u64);
        assert_eq!(
            std::fs::read(&received.0).expect("读取接收文件"),
            data,
            "设备 {device} 收到的内容应与源文件一致"
        );
    }
    assert!(
        harness.engines[0].outgoing.is_empty(),
        "全部传输完成后发送端 outgoing 应清空"
    );
}

// ---------------------------------------------------------------------------
// 连接被"同 identity 新连接"抢占后的兜底重握手
// ---------------------------------------------------------------------------

#[test]
fn larger_side_fallback_rehandshake_after_connection_replaced() {
    let mut harness = Harness::new();
    harness.handshake();
    let larger = if harness.a_id > harness.b_id {
        Side::A
    } else {
        Side::B
    };
    let smaller = larger.other();
    let smaller_id = harness.engine(smaller).self_id().to_string();
    let base = now_ms();

    // 模拟较大侧连接被同 identity 新连接抢占后重连：本端重连并清空会话，
    // 但较小侧没有收到 offline presence，仍持有 stale session（不会主动重握手）。
    let _ = harness.engine_mut(larger).on_disconnect();
    let info = harness.connection_info(larger);
    let auth = harness.engine_mut(larger).on_authenticated(&info);
    assert!(auth.outbound.is_empty(), "较大侧重连后不应立即发起握手");

    let early = harness.engine_mut(larger).on_tick(base + 1_000);
    assert!(early.outbound.is_empty(), "兜底延迟内不应主动发起");

    let fallback = harness
        .engine_mut(larger)
        .on_tick(base + SESSION_FALLBACK_DELAY_MS + 1_500);
    assert!(
        !fallback.outbound.is_empty(),
        "超过兜底延迟后较大侧应主动发起重握手"
    );
    let settled = harness.pump(larger, fallback);
    assert_eq!(harness.engine(larger).sessions().len(), 1);
    assert_eq!(harness.engine(smaller).sessions().len(), 1);
    let events: &[RuntimeEvent] = match larger {
        Side::A => &settled.events_a,
        Side::B => &settled.events_b,
    };
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::SessionEstablished { peer_id, .. } if peer_id == &smaller_id
    )));

    // 兜底重建的会话可正常同步剪切板（较小侧 → 较大侧）。
    let tick = base + SESSION_FALLBACK_DELAY_MS + 3_000;
    let _ = harness
        .engine_mut(smaller)
        .on_local_clipboard(ClipboardPayload::text("after-fallback").expect("构造文本载荷"));
    let output = harness.engine_mut(smaller).on_tick(tick);
    let settled = harness.pump(smaller, output);
    let applied: &[ClipboardPayload] = match larger {
        Side::A => &settled.applied_a,
        Side::B => &settled.applied_b,
    };
    assert_eq!(applied.len(), 1, "兜底重建的会话应能同步剪切板");
    assert_eq!(applied[0].as_text().expect("文本"), "after-fallback");
}


