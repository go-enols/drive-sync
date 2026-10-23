//! drive-sync 协议核心库。
//!
//! 设计要点：
//! - **中继不可信**：所有内容端到端加密，中继只做信令与密文转发；可选暂存也只保存密文。
//! - **身份自持**：每设备一对 Ed25519（签名）+ X25519（密钥交换）密钥，device_id 由公钥派生。
//! - **配对一次性**：8 位配对码/二维码 + 6 位 SAS 防 MITM；配对后默认信任。
//! - **目录 CRDT**：软件目录只同步元数据（LWW + 墓碑），软件本体留在持有设备本地。
//! - **连接级前向保密**：每次重连重新做 X25519 握手。

#![forbid(unsafe_code)]

pub mod catalog;
pub mod clipboard;
pub mod crypto;
pub mod error;
pub mod hlc;
pub mod identity;
pub mod pairing;
pub mod peer;
pub mod revocation;
pub mod session;
pub mod transfer;
pub mod util;
pub mod vault;
pub mod wire;

pub use error::{CoreError, Result};

/// 协议版本：不兼容变更时递增。
///
/// v2：新增离线解除配对通知（`revocation_put`/`pending_revocations`/`revocation_ack`）
/// 与配对记录活跃时间（90 天不活跃过期）。与 v1 不互通，中继会拒绝旧版本客户端
/// （`bad_version`），需与中继一起升级。
pub const PROTOCOL_VERSION: u32 = 2;

/// 应用名（用于 data 目录、日志等）。
pub const APP_NAME: &str = "drive-sync";

/// 单条 WebSocket 文本帧上限（中继/客户端配置一致；块传输每块约 64 KiB）。
pub const MAX_WIRE_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// 默认中继端口。
pub const DEFAULT_RELAY_PORT: u16 = 8787;
