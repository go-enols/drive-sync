//! drive-sync 客户端同步引擎。
//!
//! 分层：
//! - [`store`]：本地数据目录持久化（identity/peers/config/catalog/local_index/vault_keys/spool/incoming），
//!   JSON 原子写；多进程安全（先写临时文件再 rename）。
//! - [`engine`]：同步状态机（纯同步逻辑、可单测）。连接认证后处理 welcome/presence、
//!   E2E 会话握手、剪切板去重与回环抑制、目录 CRDT 同步、文件中转（流控/断点续传）、
//!   离线 spool、vault 密钥共享。
//! - [`net`]：tokio-tungstenite WebSocket 客户端（Hello 认证、WS Ping 心跳）。
//! - [`runtime`]：异步封装（连接循环 + 自动重连 + Engine + 传输驱动），供 CLI 与 e2e 复用。
//! - [`ops`]：一次性连接辅助（配对 `pair_initiate/pair_respond`、vault put/list/get、
//!   目录下载 `catalog_fetch`）。
//!
//! 安全约束：所有 PeerMessage 必须先经 E2E 会话加密再交给中继；未建立会话时进入本地 spool，
//! 绝不发送明文；日志只记录类型/大小/设备 id，不打印内容。
//!
//! # 快速上手（CLI daemon / e2e 测试复用）
//!
//! ```no_run
//! use std::path::PathBuf;
//! use std::time::Duration;
//! use drive_client::{pair_initiate, AutoPairUi, Runtime, RuntimeConfig, RuntimeEvent};
//!
//! # async fn demo() -> anyhow::Result<()> {
//! let data_dir = PathBuf::from("./deviceA-data");
//! let relay = "ws://127.0.0.1:8787/ws".to_string();
//! let token = "shared-access-token".to_string();
//!
//! // 1) 配对（A 端会打印二维码/配对码，B 端用 pair_respond(qr_or_code) 加入）
//! let config = RuntimeConfig::new(&data_dir, &relay, &token);
//! let outcome = pair_initiate(&config, &AutoPairUi::default()).await?;
//! println!("配对成功，对端 = {}", outcome.record.peer.device_id);
//!
//! // 2) 常驻同步：事件通道用于打印摘要/e2e 断言，远端剪切板经 std mpsc 交给 UI/后端线程
//! let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<RuntimeEvent>();
//! let (clip_apply_tx, clip_apply_rx) = std::sync::mpsc::channel();
//! let mut config = RuntimeConfig::new(&data_dir, &relay, &token);
//! config.event_tx = Some(event_tx);
//! config.remote_clipboard_tx = Some(clip_apply_tx);
//! let (runtime, handle) = Runtime::open(config)?;
//! let handle2 = handle.clone();
//! tokio::spawn(async move { runtime.run().await });
//!
//! // 3) 注入本机剪切板变化 / 目录操作 / 主动拉取文件
//! handle.set_local_clipboard(drive_core::clipboard::ClipboardPayload::text("hello")?).await?;
//! let entry_id = handle.catalog_add(drive_client::CatalogAddParams::new("demo.zip")).await?;
//! handle.request_fetch("dev-peer", &entry_id).await?;
//! # let _ = (&mut event_rx, clip_apply_rx, handle2);
//! # Ok(())
//! # }
//! ```
#![forbid(unsafe_code)]

pub mod dedupe;
pub mod engine;
pub mod net;
pub mod ops;
pub mod pairing_joiner;
pub mod runtime;
pub mod spool;
pub mod store;
pub mod transfer_rx;
pub mod util;

pub use dedupe::{RecentApplied, TimedLru};
pub use engine::{
    default_platform, CatalogAddParams, ConnectionInfo, Engine, EngineOutput, RuntimeCommand,
    RuntimeEvent,
};
pub use net::{normalize_relay_url, Connection};
pub use ops::{
    catalog_fetch, pair_initiate, pair_pairwise, pair_respond, vault_get, vault_list, vault_put,
    AutoPairUi, PairOutcome, PairUi, VaultPutOutcome,
};
pub use runtime::{Runtime, RuntimeConfig, RuntimeHandle};
pub use spool::{Spool, SpoolItem};
pub use store::{
    default_data_dir, ClientConfig, ClipboardSettings, LocalIndexEntry, SpoolSettings, Store,
    VaultKey,
};

/// 协议版本（与 drive-core 保持一致）。
pub use drive_core::PROTOCOL_VERSION;

/// 单条剪切板载荷上限（超过应由调用方走文件中转，不阻塞剪切板链路）。
pub const MAX_CLIPBOARD_BYTES: u64 = drive_core::clipboard::MAX_CLIPBOARD_BYTES;

/// 客户端库版本。
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
