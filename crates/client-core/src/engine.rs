//! 同步状态机（纯同步逻辑，无网络依赖，可单测）。
//!
//! Engine 负责：
//! - 连接认证后的 `welcome`/`presence` 处理，按"device_id 字典序小的一方发起"规则做 E2E 会话握手；
//! - `relay` 信封解密 → `PeerMessage` 分发；所有对端消息发送前都经会话加密（未建立会话时排队/进 spool）；
//! - 剪切板：本机变化去抖（100ms）→ 广播在线已配对设备 / 离线进 spool；收端校验 + `event_id` 去重 +
//!   `recent_applied` 回环抑制；
//! - 目录：会话建立后互发 snapshot，本地变更发 delta，合并 CRDT，have_list 广播，fetch_request 处理；
//! - 文件传输：offer/accept/chunk/ack（窗口 32 块）/done，BLAKE3 校验后原子重命名，支持断点续传；
//! - 云端暂存密钥共享：会话建立/检测到外部 vault_keys.json 变更时发 `vault_share`；
//! - 优雅关闭：`on_disconnect` 清理会话但保留 `.part` 与 spool（重连后续传/补发）。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use derive_core::catalog::{Catalog, CatalogEntry, CatalogOp, TargetPlatform};
use derive_core::clipboard::{ClipboardEvent, ClipboardKind, ClipboardPayload};
use derive_core::identity::Identity;
use derive_core::pairing::PairRecord;
use derive_core::peer::PeerMessage;
use derive_core::session::{
    accept_session_init, complete_session, create_session_init, Session, SessionInit,
};
use derive_core::transfer::TransferManifest;
use derive_core::util::{now_ms, random_hex};
use derive_core::vault::VaultMeta;
use derive_core::wire::{ClientToServer, ServerToClient};

use crate::dedupe::{AppliedRecord, RecentApplied, TimedLru};
use crate::spool::{Spool, SpoolItem};
use crate::store::{ClientConfig, ClipboardSettings, LocalIndexEntry, SpoolSettings, Store, VaultKey};
use crate::transfer_rx::{align_up, PartialReceiver};
use crate::util::hash_file;

/// 剪切板本地去抖窗口（协议 §7.1：100ms 合并连续变化）。
pub const CLIPBOARD_DEBOUNCE_MS: u64 = 100;
/// `recent_applied` 容量/TTL（协议 §7.1：LRU 256 / 5 分钟）。
pub const RECENT_APPLIED_CAP: usize = 256;
pub const RECENT_APPLIED_TTL_MS: u64 = 5 * 60 * 1000;
/// 剪切板 `event_id` 去重容量/TTL。
pub const EVENT_DEDUPE_CAP: usize = 256;
pub const EVENT_DEDUPE_TTL_MS: u64 = 5 * 60 * 1000;
/// 发送窗口：最多 32 块未确认（协议 §7.3）。
pub const TRANSFER_WINDOW_BLOCKS: u32 = 32;
/// 接收端每 8 块（默认 512 KiB）回一次 ack。
pub const TRANSFER_ACK_EVERY_CHUNKS: u32 = 8;
/// 每设备并发传输上限。
pub const MAX_CONCURRENT_TRANSFERS: usize = 2;
/// 会话握手重试间隔/上限。
pub const SESSION_RETRY_MS: u64 = 5_000;
pub const SESSION_MAX_ATTEMPTS: u32 = 8;
/// 兜底重握手延迟：对端在线超过该时长仍无会话时，较大 id 侧也会主动发起。
///
/// 用于"同 identity 新连接抢旧连接"场景：被抢占方重连了，但对端看不到 offline 事件、
/// 仍持有 stale session 不会重新握手；正常启动时小 id 侧毫秒级发起，不会触发兜底。
pub const SESSION_FALLBACK_DELAY_MS: u64 = 3_000;
/// 断点续传时复用的传输清单缓存有效期（同 entry 再次 fetch 时复用 transfer_id）。
pub const OFFER_CACHE_TTL_MS: u64 = 10 * 60 * 1000;
/// 传输进度事件间隔（字节）。
pub const TRANSFER_PROGRESS_STEP: u64 = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// 对外事件 / 命令 / 输出
// ---------------------------------------------------------------------------

/// 连接认证后引擎需要的信息（由 net 层从 `welcome` 提取）。
#[derive(Clone, Debug, Default)]
pub struct ConnectionInfo {
    pub device_id: String,
    pub session_id: String,
    pub online: Vec<String>,
    pub server_time_ms: u64,
}

/// 运行期事件（供 CLI 打印摘要 / e2e 断言；**不包含内容明文**）。
#[derive(Clone, Debug)]
pub enum RuntimeEvent {
    Authenticated {
        device_id: String,
        online: Vec<String>,
    },
    Presence {
        online: Vec<String>,
    },
    PeerOnline {
        peer_id: String,
        name: String,
    },
    PeerOffline {
        peer_id: String,
    },
    SessionEstablished {
        peer_id: String,
        name: String,
    },
    SessionClosed {
        peer_id: String,
    },
    ClipboardCaptured {
        kind: &'static str,
        size: u64,
    },
    ClipboardSent {
        peers: usize,
        kind: &'static str,
        size: u64,
    },
    ClipboardApplied {
        peer_id: String,
        kind: &'static str,
        size: u64,
    },
    ClipboardSpooled {
        peers: usize,
    },
    SpoolReplayed {
        peer_id: String,
        count: usize,
    },
    CatalogUpdated {
        entry_id: String,
        ops: usize,
    },
    CatalogMerged {
        from: String,
        applied: u32,
        ignored: u32,
        conflicts: u32,
    },
    /// 检测到其他进程写入了 catalog.json / local_index.json，已重新加载并广播。
    CatalogReloaded {
        entries: usize,
    },
    PeerHaveList {
        peer_id: String,
        entry_ids: Vec<String>,
    },
    TransferOffered {
        peer_id: String,
        entry_id: Option<String>,
        size: u64,
        transfer_id: String,
    },
    TransferStarted {
        transfer_id: String,
        entry_id: Option<String>,
        from_offset: u64,
        total: u64,
    },
    TransferProgress {
        transfer_id: String,
        received: u64,
        total: u64,
    },
    TransferReceived {
        transfer_id: String,
        entry_id: Option<String>,
        path: PathBuf,
        size: u64,
    },
    TransferFailed {
        transfer_id: String,
        message: String,
    },
    VaultShared {
        peer_id: String,
        object_id: String,
    },
    VaultKeyReceived {
        from: String,
        object_id: String,
    },
    /// 外部 peers.json 新增/更新了配对记录（如 CLI 完成配对），已重新加载。
    PeersReloaded {
        count: usize,
    },
    /// 本机显式删除了某个配对记录（外部 peers.json 变更或收到对端撤销后清理）。
    PeerRevoked {
        device_id: String,
        name: String,
        reason: String,
        /// 是否通过现有 E2E 会话成功把 `PairRevoked` 通知发给了对端。
        notified: bool,
    },
    /// 收到对端解除配对通知，本地 PairRecord 已删除。
    PeerUnpaired {
        device_id: String,
        name: String,
        reason: String,
    },
    RelayError {
        code: String,
        message: String,
    },
    Error {
        message: String,
    },
    /// 运行期已停止（Ctrl-C / 优雅关闭）。
    Stopped,
}

impl RuntimeEvent {
    /// 一行中文摘要（CLI 用；只含类型/大小/设备 id，不含内容）。
    pub fn summary(&self) -> String {
        match self {
            Self::Authenticated { device_id, online } => {
                format!("已连接中继（{device_id}，{online_count} 台设备在线）", online_count = online.len())
            }
            Self::Presence { online } => format!("在线状态更新（{} 台设备）", online.len()),
            Self::PeerOnline { peer_id, name } => format!("设备上线: {name} ({peer_id})"),
            Self::PeerOffline { peer_id } => format!("设备离线: {peer_id}"),
            Self::SessionEstablished { peer_id, name } => {
                format!("E2E 会话已建立: {name} ({peer_id})")
            }
            Self::SessionClosed { peer_id } => format!("会话已关闭: {peer_id}"),
            Self::ClipboardCaptured { kind, size } => {
                format!("本机剪切板变化: {kind} {size} 字节")
            }
            Self::ClipboardSent { peers, kind, size } => {
                format!("剪切板已发送: {kind} {size} 字节 → {peers} 台设备")
            }
            Self::ClipboardApplied { peer_id, kind, size } => {
                format!("收到剪切板: {kind} {size} 字节（来自 {peer_id}）")
            }
            Self::ClipboardSpooled { peers } => {
                format!("对端离线，剪切板事件已进入待发队列（{peers} 台设备）")
            }
            Self::SpoolReplayed { peer_id, count } => {
                format!("离线消息已补发: {count} 条 → {peer_id}")
            }
            Self::CatalogUpdated { entry_id, ops } => {
                format!("目录已更新: {entry_id}（{ops} 个操作）")
            }
            Self::CatalogMerged {
                from,
                applied,
                ignored,
                conflicts,
            } => format!(
                "目录已合并（来自 {from}）: 应用 {applied}，忽略 {ignored}，冲突 {conflicts}"
            ),
            Self::CatalogReloaded { entries } => {
                format!("检测到外部目录更新，已重新加载并广播（{entries} 条可见）")
            }
            Self::PeerHaveList { peer_id, entry_ids } => {
                format!("设备 {} 持有 {} 个软件本体", peer_id, entry_ids.len())
            }
            Self::TransferOffered {
                peer_id,
                entry_id,
                size,
                transfer_id,
            } => format!(
                "发送文件报价: {} {size} 字节 → {peer_id} ({transfer_id})",
                entry_id.as_deref().unwrap_or("-")
            ),
            Self::TransferStarted {
                transfer_id,
                entry_id,
                from_offset,
                total,
            } => format!(
                "开始接收: {} {total} 字节，从 {from_offset} 续传 ({transfer_id})",
                entry_id.as_deref().unwrap_or("-")
            ),
            Self::TransferProgress {
                transfer_id,
                received,
                total,
            } => format!("接收进度 {received}/{total} ({transfer_id})"),
            Self::TransferReceived {
                transfer_id,
                entry_id,
                path,
                size,
            } => format!(
                "文件接收完成: {} {size} 字节 → {} ({transfer_id})",
                entry_id.as_deref().unwrap_or("-"),
                path.display()
            ),
            Self::TransferFailed {
                transfer_id,
                message,
            } => format!("传输失败 ({transfer_id}): {message}"),
            Self::VaultShared { peer_id, object_id } => {
                format!("暂存密钥已共享: {object_id} → {peer_id}")
            }
            Self::VaultKeyReceived { from, object_id } => {
                format!("收到暂存密钥: {object_id}（来自 {from}）")
            }
            Self::PeersReloaded { count } => {
                format!("检测到外部配对变更，已重新加载（{count} 个已配对设备）")
            }
            Self::PeerRevoked {
                device_id,
                name,
                reason,
                notified,
            } => format!(
                "已删除配对记录: {name} ({device_id})，原因: {reason}；{}",
                if *notified {
                    "已通知对端解除配对"
                } else {
                    "对端离线，未能发送撤销通知（下次握手会被拒绝）"
                }
            ),
            Self::PeerUnpaired {
                device_id,
                name,
                reason,
            } => format!("对端已解除配对: {name} ({device_id})，本地配对记录已删除（原因: {reason}）"),
            Self::RelayError { code, message } => format!("中继错误 [{code}]: {message}"),
            Self::Error { message } => format!("内部错误: {message}"),
            Self::Stopped => "已停止".to_string(),
        }
    }
}

/// 引擎单次处理产生的输出。
#[derive(Default, Debug)]
pub struct EngineOutput {
    /// 需要发往中继的消息（顺序即发送顺序）。
    pub outbound: Vec<ClientToServer>,
    /// 需要应用到本机剪切板的内容（已通过完整性与去重校验）。
    pub applied_clipboard: Vec<ClipboardPayload>,
    /// 事件（供日志/CLI/e2e）。
    pub events: Vec<RuntimeEvent>,
}

impl EngineOutput {
    pub fn merge(&mut self, other: EngineOutput) {
        self.outbound.extend(other.outbound);
        self.applied_clipboard.extend(other.applied_clipboard);
        self.events.extend(other.events);
    }

    pub fn is_idle(&self) -> bool {
        self.outbound.is_empty() && self.applied_clipboard.is_empty() && self.events.is_empty()
    }
}

/// 运行时命令（RuntimeHandle → Engine）。
pub enum RuntimeCommand {
    SetClipboard(ClipboardPayload),
    CatalogAdd {
        params: CatalogAddParams,
        reply: Option<tokio::sync::oneshot::Sender<std::result::Result<String, String>>>,
    },
    CatalogRemove {
        id: String,
        reply: Option<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
    },
    Fetch {
        peer_id: String,
        entry_id: String,
    },
}

/// `catalog add` 参数（CLI 与 e2e 共用）。
#[derive(Clone, Debug)]
pub struct CatalogAddParams {
    pub path: PathBuf,
    pub name: String,
    pub version: String,
    pub platform: TargetPlatform,
    pub kind: String,
    pub tags: Vec<String>,
    pub notes: String,
}

impl CatalogAddParams {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            name: String::new(),
            version: String::new(),
            platform: default_platform(),
            kind: "file".to_string(),
            tags: Vec::new(),
            notes: String::new(),
        }
    }
}

/// 当前构建目标对应的平台。
pub fn default_platform() -> TargetPlatform {
    #[cfg(target_os = "windows")]
    {
        TargetPlatform::Windows
    }
    #[cfg(target_os = "linux")]
    {
        TargetPlatform::Linux
    }
    #[cfg(target_os = "macos")]
    {
        TargetPlatform::Macos
    }
    #[cfg(target_os = "android")]
    {
        TargetPlatform::Android
    }
    #[cfg(target_os = "ios")]
    {
        TargetPlatform::Ios
    }
    #[cfg(not(any(
        target_os = "windows",
        target_os = "linux",
        target_os = "macos",
        target_os = "android",
        target_os = "ios"
    )))]
    {
        TargetPlatform::Any
    }
}

// ---------------------------------------------------------------------------
// 内部状态
// ---------------------------------------------------------------------------

struct PendingInit {
    init: SessionInit,
    last_attempt_ms: u64,
    attempts: u32,
}

struct PendingLocal {
    payload: ClipboardPayload,
    due_at_ms: u64,
}

/// 发送端"报价模板"缓存：按 `(peer_id, entry_id)` 缓存，保证：
/// - 同一对端重连后重新 fetch 同一条目可复用 `transfer_id`（接收端 `.part` 续传）；
/// - 不同对端并发 fetch 同一 entry 各自拥有独立 `transfer_id`（避免 outgoing 覆盖）。
#[derive(Clone)]
struct CachedOffer {
    manifest: TransferManifest,
    size: u64,
    content_hash: String,
    file_mtime_ms: Option<u64>,
    expires_at_ms: u64,
}

struct OutgoingTransfer {
    peer_id: String,
    manifest: TransferManifest,
    file: File,
    /// 下一块发送偏移（块对齐）。
    next_offset: u64,
    /// 接收端已确认的连续字节数。
    acked_offset: u64,
    done_sent: bool,
    last_activity_ms: u64,
}

struct IncomingTransfer {
    peer_id: String,
    manifest: TransferManifest,
    receiver: PartialReceiver,
    chunks_since_ack: u32,
    next_progress_bytes: u64,
    created_at_ms: u64,
}

/// 同步引擎。
pub struct Engine {
    store: Arc<Store>,
    identity: Identity,
    self_id: String,
    config: ClientConfig,
    clipboard: ClipboardSettings,
    spool_settings: SpoolSettings,

    peers: Vec<PairRecord>,
    catalog: Catalog,
    catalog_revision: u64,
    local_index: BTreeMap<String, LocalIndexEntry>,
    vault_keys: Vec<VaultKey>,
    spool: Spool,

    online: BTreeSet<String>,
    /// 每个在线已配对设备首次被观测到在线的时刻（用于兜底重握手）。
    online_since: HashMap<String, u64>,
    sessions: HashMap<String, Session>,
    pending_inits: HashMap<String, PendingInit>,
    peer_have: HashMap<String, Vec<String>>,

    recent_applied: RecentApplied,
    seen_events: TimedLru<String>,
    last_local_hash: Option<String>,
    pending_local: Option<PendingLocal>,
    local_seq: u64,

    outgoing: HashMap<String, OutgoingTransfer>,
    incoming: HashMap<String, IncomingTransfer>,
    offer_cache: HashMap<(String, String), CachedOffer>,

    last_spool_prune_ms: u64,
    last_vault_check_ms: u64,
    vault_mtime: Option<u64>,
    catalog_mtime: Option<u64>,
    local_index_mtime: Option<u64>,
    /// peers.json 的 mtime（检测外部 `peers remove`/`pair` 写入）。
    peers_mtime: Option<u64>,
}

impl Engine {
    pub fn new(store: Arc<Store>, identity: Identity) -> Result<Self> {
        let self_id = identity.device_id();
        let config = store.load_config()?;
        let clipboard = config.clipboard.clone();
        let spool_settings = config.spool.clone();
        let mut peers = store.load_peers()?;
        peers.sort_by(|a, b| a.peer.device_id.cmp(&b.peer.device_id));
        let (mut catalog, catalog_revision) = store.load_catalog()?;
        if catalog.node.trim().is_empty() {
            catalog.node = self_id.clone();
        }
        let local_index = store.load_local_index()?;
        let vault_keys = store.load_vault_keys()?;
        let spool = Spool::new(store.spool_dir(), &spool_settings)?;
        let vault_mtime = store.file_mtime_ms(&store.vault_keys_path());
        let catalog_mtime = store.file_mtime_ms(&store.catalog_path());
        let local_index_mtime = store.file_mtime_ms(&store.local_index_path());
        let peers_mtime = store.file_mtime_ms(&store.peers_path());
        let _ = spool.prune_expired(now_ms());
        Ok(Self {
            store,
            identity,
            self_id,
            config,
            clipboard,
            spool_settings,
            peers,
            catalog,
            catalog_revision,
            local_index,
            vault_keys,
            spool,
            online: BTreeSet::new(),
            online_since: HashMap::new(),
            sessions: HashMap::new(),
            pending_inits: HashMap::new(),
            peer_have: HashMap::new(),
            recent_applied: RecentApplied::new(RECENT_APPLIED_CAP, RECENT_APPLIED_TTL_MS),
            seen_events: TimedLru::new(EVENT_DEDUPE_CAP, EVENT_DEDUPE_TTL_MS),
            last_local_hash: None,
            pending_local: None,
            local_seq: 0,
            outgoing: HashMap::new(),
            incoming: HashMap::new(),
            offer_cache: HashMap::new(),
            last_spool_prune_ms: now_ms(),
            last_vault_check_ms: now_ms(),
            vault_mtime,
            catalog_mtime,
            local_index_mtime,
            peers_mtime,
        })
    }

    // ------------------------------------------------------------------
    // 只读访问（CLI / 测试 / e2e）
    // ------------------------------------------------------------------

    pub fn self_id(&self) -> &str {
        &self.self_id
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn device_name(&self) -> &str {
        &self.identity.name
    }

    pub fn peers(&self) -> &[PairRecord] {
        &self.peers
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn catalog_revision(&self) -> u64 {
        self.catalog_revision
    }

    pub fn local_index(&self) -> &BTreeMap<String, LocalIndexEntry> {
        &self.local_index
    }

    pub fn spool(&self) -> &Spool {
        &self.spool
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn online_peers(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .peers
            .iter()
            .filter(|p| self.online.contains(&p.peer.device_id))
            .map(|p| p.peer.device_id.clone())
            .collect();
        out.sort();
        out
    }

    pub fn sessions(&self) -> Vec<String> {
        let mut out: Vec<String> = self.sessions.keys().cloned().collect();
        out.sort();
        out
    }

    pub fn peer_have(&self) -> &HashMap<String, Vec<String>> {
        &self.peer_have
    }

    pub fn clipboard_settings(&self) -> &ClipboardSettings {
        &self.clipboard
    }

    pub fn set_clipboard_paused(&mut self, paused: bool) {
        self.clipboard.paused = paused;
    }

    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    // ------------------------------------------------------------------
    // 连接生命周期
    // ------------------------------------------------------------------

    /// 连接认证成功后调用：重置连接级状态（会话只存在于内存，重连必须重新握手）。
    pub fn on_authenticated(&mut self, info: &ConnectionInfo) -> EngineOutput {
        let mut out = EngineOutput::default();
        self.reset_connection_state(&mut out);
        self.online = info
            .online
            .iter()
            .filter(|id| id.as_str() != self.self_id)
            .cloned()
            .collect();
        let now = now_ms();
        for peer_id in &self.online {
            self.online_since.insert(peer_id.clone(), now);
        }
        out.events.push(RuntimeEvent::Authenticated {
            device_id: self.self_id.clone(),
            online: self.online.iter().cloned().collect(),
        });
        self.announce_online_peers(&mut out);
        self.retry_sessions(now, &mut out);
        out
    }

    /// 连接断开：清理会话级状态（保留 spool 与 `.part` 文件以便重连后补发/续传）。
    pub fn on_disconnect(&mut self) -> EngineOutput {
        let mut out = EngineOutput::default();
        self.reset_connection_state(&mut out);
        out
    }

    fn reset_connection_state(&mut self, out: &mut EngineOutput) {
        for peer_id in self.sessions.keys() {
            out.events.push(RuntimeEvent::SessionClosed {
                peer_id: peer_id.clone(),
            });
        }
        self.sessions.clear();
        self.pending_inits.clear();
        self.outgoing.clear();
        // 丢弃接收器（不删除 .part），重连后可续传。
        self.incoming.clear();
        self.peer_have.clear();
        self.online.clear();
        self.online_since.clear();
    }

    fn announce_online_peers(&mut self, out: &mut EngineOutput) {
        let online = self.online.clone();
        for peer in &self.peers {
            let pid = peer.peer.device_id.clone();
            if online.contains(&pid) {
                out.events.push(RuntimeEvent::PeerOnline {
                    peer_id: pid,
                    name: peer.display_name(),
                });
            }
        }
    }

    // ------------------------------------------------------------------
    // 中继消息处理
    // ------------------------------------------------------------------

    pub fn handle_server(&mut self, msg: ServerToClient) -> EngineOutput {
        match msg {
            ServerToClient::Welcome {
                v: _,
                device_id,
                session_id,
                server_time_ms,
                online,
            } => {
                let info = ConnectionInfo {
                    device_id,
                    session_id,
                    online,
                    server_time_ms,
                };
                self.on_authenticated(&info)
            }
            ServerToClient::Presence { online } => self.on_presence(online),
            ServerToClient::SessionInit { from, msg } => self.handle_session_init(from, msg),
            ServerToClient::SessionAck { from, msg } => self.handle_session_ack(from, msg),
            ServerToClient::Relay { from, envelope } => self.handle_relay(from, envelope),
            ServerToClient::Error { code, message } => {
                let mut out = EngineOutput::default();
                out.events.push(RuntimeEvent::RelayError { code, message });
                out
            }
            // 配对与暂存的一次性响应不经过 Engine（由 ops 层直接处理）。
            _ => EngineOutput::default(),
        }
    }

    fn on_presence(&mut self, online: Vec<String>) -> EngineOutput {
        let mut out = EngineOutput::default();
        let now = now_ms();
        let new_set: BTreeSet<String> = online
            .into_iter()
            .filter(|id| id.as_str() != self.self_id)
            .collect();

        let removed: Vec<String> = self.online.difference(&new_set).cloned().collect();
        for peer_id in removed {
            if self.sessions.remove(&peer_id).is_some() {
                out.events.push(RuntimeEvent::SessionClosed {
                    peer_id: peer_id.clone(),
                });
            }
            self.pending_inits.remove(&peer_id);
            self.peer_have.remove(&peer_id);
            self.online_since.remove(&peer_id);
            self.outbound_transfers_for_peer_cleanup(&peer_id);
            self.incoming.retain(|_, t| t.peer_id != peer_id);
            if self.peers.iter().any(|p| p.peer.device_id == peer_id) {
                out.events.push(RuntimeEvent::PeerOffline { peer_id });
            }
        }

        let added: Vec<String> = new_set.difference(&self.online).cloned().collect();
        for peer_id in added {
            if let Some(peer) = self
                .peers
                .iter()
                .find(|p| p.peer.device_id == peer_id)
                .cloned()
            {
                out.events.push(RuntimeEvent::PeerOnline {
                    peer_id: peer_id.clone(),
                    name: peer.display_name(),
                });
                // 重新上线时重置握手重试计数与兜底计时。
                self.pending_inits.remove(&peer_id);
                self.online_since.insert(peer_id.clone(), now);
            }
        }
        self.online = new_set;
        out.events.push(RuntimeEvent::Presence {
            online: self.online.iter().cloned().collect(),
        });
        self.retry_sessions(now, &mut out);
        out
    }

    fn outbound_transfers_for_peer_cleanup(&mut self, peer_id: &str) {
        self.outgoing.retain(|_, t| t.peer_id != peer_id);
    }

    fn handle_session_init(
        &mut self,
        from: String,
        msg: derive_core::session::SessionInitMsg,
    ) -> EngineOutput {
        let mut out = EngineOutput::default();
        // 同时握手裁决（字典序）：较小 id 侧是协议规定的发起方，优先保留它的 init。
        if self.pending_inits.contains_key(&from) {
            if self.self_id < from {
                tracing::debug!(
                    peer = %from,
                    "忽略同时到达的兜底 session_init（本端为协议规定的发起方）"
                );
                return out;
            }
            // 本端是兜底发起方：放弃自己的 pending，接受较小侧的 init。
            self.pending_inits.remove(&from);
        }
        let Some(peer) = self
            .peers
            .iter()
            .find(|p| p.peer.device_id == from)
            .cloned()
        else {
            out.events.push(RuntimeEvent::Error {
                message: format!("拒绝未配对设备的会话握手: {from}"),
            });
            return out;
        };
        match accept_session_init(&self.identity, &msg, &peer.peer) {
            Ok((ack, session)) => {
                self.pending_inits.remove(&from);
                self.sessions.insert(from.clone(), session);
                out.outbound.push(ClientToServer::SessionAck {
                    to: from.clone(),
                    msg: ack,
                });
                self.on_session_established(&from, &mut out);
            }
            Err(e) => {
                out.events.push(RuntimeEvent::Error {
                    message: format!("会话握手校验失败（{from}）: {e}"),
                });
            }
        }
        out
    }

    fn handle_session_ack(
        &mut self,
        from: String,
        msg: derive_core::session::SessionAckMsg,
    ) -> EngineOutput {
        let mut out = EngineOutput::default();
        let Some(pending) = self.pending_inits.remove(&from) else {
            return out;
        };
        let Some(peer) = self
            .peers
            .iter()
            .find(|p| p.peer.device_id == from)
            .cloned()
        else {
            return out;
        };
        match complete_session(&self.identity, &pending.init, &msg, &peer.peer) {
            Ok(session) => {
                self.sessions.insert(from.clone(), session);
                self.on_session_established(&from, &mut out);
            }
            Err(e) => {
                out.events.push(RuntimeEvent::Error {
                    message: format!("会话握手响应校验失败（{from}）: {e}"),
                });
                if self.self_id < from {
                    self.initiate_session(&from, &mut out);
                }
            }
        }
        out
    }

    fn handle_relay(
        &mut self,
        from: String,
        envelope: derive_core::session::E2eEnvelope,
    ) -> EngineOutput {
        let mut out = EngineOutput::default();
        let decrypted = match self.sessions.get_mut(&from) {
            Some(session) => session.decrypt(&envelope).map_err(|e| e.to_string()),
            None => Err("会话不存在（可能是重连后的旧消息）".to_string()),
        };
        let (kind, plain) = match decrypted {
            Ok(v) => v,
            Err(e) => {
                let had_session = self.sessions.remove(&from).is_some();
                out.events.push(RuntimeEvent::Error {
                    message: format!("会话消息处理失败（{from}）: {e}"),
                });
                if had_session {
                    out.events.push(RuntimeEvent::SessionClosed {
                        peer_id: from.clone(),
                    });
                }
                if self.self_id < from {
                    self.initiate_session(&from, &mut out);
                }
                return out;
            }
        };
        if kind != "peer" {
            tracing::warn!(peer = %from, kind = %kind, "收到非 peer 类型的会话消息，忽略");
            return out;
        }
        match PeerMessage::from_bytes(&plain) {
            Ok(message) => self.handle_peer_message(&from, message),
            Err(e) => {
                out.events.push(RuntimeEvent::Error {
                    message: format!("PeerMessage 解码失败（{from}）: {e}"),
                });
                out
            }
        }
    }

    /// 处理解密后的对端业务消息。
    pub fn handle_peer_message(&mut self, from: &str, message: PeerMessage) -> EngineOutput {
        let mut out = EngineOutput::default();
        let paired = self.peers.iter().any(|p| p.peer.device_id == from);
        if !paired {
            out.events.push(RuntimeEvent::Error {
                message: format!("收到未配对设备的消息: {from}"),
            });
            return out;
        }
        match message {
            PeerMessage::Clipboard { event } => {
                self.handle_remote_clipboard(from, event, &mut out)
            }
            PeerMessage::CatalogSnapshot { ops, revision: _ } => {
                self.merge_catalog(from, ops, &mut out)
            }
            PeerMessage::CatalogDelta { ops, revision: _ } => {
                self.merge_catalog(from, ops, &mut out)
            }
            PeerMessage::CatalogRequest => {
                let ops = self.catalog.to_sync_ops();
                self.send_peer(
                    from,
                    PeerMessage::CatalogSnapshot {
                        ops,
                        revision: self.catalog_revision,
                    },
                    &mut out,
                );
            }
            PeerMessage::HaveList { entry_ids } => {
                self.peer_have.insert(from.to_string(), entry_ids.clone());
                out.events.push(RuntimeEvent::PeerHaveList {
                    peer_id: from.to_string(),
                    entry_ids,
                });
            }
            PeerMessage::FetchRequest { entry_id } => {
                self.handle_fetch_request(from, &entry_id, &mut out)
            }
            PeerMessage::TransferOffer { manifest } => {
                self.handle_transfer_offer(from, manifest, &mut out)
            }
            PeerMessage::TransferAccept {
                transfer_id,
                from_offset,
            } => self.handle_transfer_accept(from, &transfer_id, from_offset, &mut out),
            PeerMessage::TransferReject {
                transfer_id,
                reason,
            } => {
                self.outgoing.remove(&transfer_id);
                out.events.push(RuntimeEvent::TransferFailed {
                    transfer_id,
                    message: format!("对端拒绝传输: {reason}"),
                });
            }
            PeerMessage::TransferChunk {
                transfer_id,
                offset,
                data,
            } => self.handle_transfer_chunk(from, &transfer_id, offset, &data, &mut out),
            PeerMessage::TransferAck {
                transfer_id,
                received_bytes,
            } => self.handle_transfer_ack(from, &transfer_id, received_bytes, &mut out),
            PeerMessage::TransferDone {
                transfer_id,
                total_bytes,
            } => self.handle_transfer_done(from, &transfer_id, total_bytes, &mut out),
            PeerMessage::TransferError {
                transfer_id,
                message,
            } => {
                self.incoming.remove(&transfer_id);
                self.outgoing.remove(&transfer_id);
                out.events.push(RuntimeEvent::TransferFailed {
                    transfer_id,
                    message,
                });
            }
            PeerMessage::VaultShare {
                object_id,
                key_hex,
                meta,
            } => self.handle_vault_share(from, object_id, key_hex, meta, &mut out),
            PeerMessage::PairRevoked { reason } => {
                self.handle_pair_revoked(from, &reason, &mut out)
            }
            PeerMessage::Ping { ts_ms } => {
                self.send_peer(from, PeerMessage::Pong { ts_ms }, &mut out);
            }
            PeerMessage::Pong { .. } => {}
        }
        out
    }

    /// 本机（外部 CLI 或收到对端撤销）删除配对关系后的统一清理：
    /// 销毁会话/握手状态/传输/offer 缓存/spool/可用性缓存，并通知上层。
    fn cleanup_peer_relationship(&mut self, peer_id: &str, out: &mut EngineOutput) {
        if self.sessions.remove(peer_id).is_some() {
            out.events.push(RuntimeEvent::SessionClosed {
                peer_id: peer_id.to_string(),
            });
        }
        self.pending_inits.remove(peer_id);
        self.peer_have.remove(peer_id);
        self.online_since.remove(peer_id);
        // 进行中的传输：发送端丢弃；接收端删除 .part（关系已解除，不应再续传）。
        self.outgoing.retain(|_, transfer| transfer.peer_id != peer_id);
        let incoming_ids: Vec<String> = self
            .incoming
            .iter()
            .filter(|(_, transfer)| transfer.peer_id == peer_id)
            .map(|(id, _)| id.clone())
            .collect();
        for transfer_id in incoming_ids {
            if let Some(transfer) = self.incoming.remove(&transfer_id) {
                transfer.receiver.discard();
            }
        }
        self.offer_cache
            .retain(|(peer, _), _| peer.as_str() != peer_id);
        // spool 中该对端的待发消息全部丢弃。
        match self.spool.remove_for_peer(peer_id) {
            Ok(removed) if removed > 0 => {
                tracing::info!(peer = %peer_id, removed, "解除配对：已清空该对端的离线待发消息");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(peer = %peer_id, error = %e, "解除配对：清理 spool 失败"),
        }
    }

    /// 处理对端发来的 `PairRevoked`：删除本地 PairRecord 并清理关系，绝不自动重建。
    fn handle_pair_revoked(&mut self, from: &str, reason: &str, out: &mut EngineOutput) {
        let name = self
            .peers
            .iter()
            .find(|peer| peer.peer.device_id == from)
            .map(|peer| peer.display_name())
            .unwrap_or_else(|| from.to_string());
        if let Err(e) = self.store.remove_peer(from) {
            out.events.push(RuntimeEvent::Error {
                message: format!("删除被撤销的配对记录失败（{from}）: {e}"),
            });
        }
        self.peers.retain(|peer| peer.peer.device_id != from);
        self.peers_mtime = self.store.file_mtime_ms(&self.store.peers_path());
        self.cleanup_peer_relationship(from, out);
        out.events.push(RuntimeEvent::PeerUnpaired {
            device_id: from.to_string(),
            name,
            reason: reason.to_string(),
        });
    }

    // ------------------------------------------------------------------
    // 会话管理
    // ------------------------------------------------------------------

    fn on_session_established(&mut self, peer_id: &str, out: &mut EngineOutput) {
        let Some(peer) = self
            .peers
            .iter()
            .find(|p| p.peer.device_id == peer_id)
            .cloned()
        else {
            return;
        };
        out.events.push(RuntimeEvent::SessionEstablished {
            peer_id: peer_id.to_string(),
            name: peer.display_name(),
        });
        self.online_since.remove(peer_id);
        // 1) 目录全量快照（含墓碑）。
        let ops = self.catalog.to_sync_ops();
        self.send_peer(
            peer_id,
            PeerMessage::CatalogSnapshot {
                ops,
                revision: self.catalog_revision,
            },
            out,
        );
        // 2) 本机持有的软件本体可用性。
        self.send_have_list_to(peer_id, out);
        // 3) 待共享的 vault 对象密钥。
        self.distribute_vault_shares(peer_id, out);
        // 4) 离线 spool 补发。
        self.replay_spool(peer_id, out);
    }

    /// 会话建立后按 (created_at, origin_seq) 顺序补发该对端的离线消息，成功即删除。
    fn replay_spool(&mut self, peer_id: &str, out: &mut EngineOutput) {
        let items = match self.spool.for_peer(peer_id) {
            Ok(items) => items,
            Err(e) => {
                out.events.push(RuntimeEvent::Error {
                    message: format!("读取离线待发队列失败: {e}"),
                });
                return;
            }
        };
        let now = now_ms();
        let mut replayed = 0usize;
        for item in items {
            if item.is_expired(now) {
                let _ = self.spool.remove(&item.id);
                continue;
            }
            if self.send_peer(peer_id, item.message.clone(), out) {
                let _ = self.spool.remove(&item.id);
                replayed += 1;
            } else {
                break;
            }
        }
        if replayed > 0 {
            out.events.push(RuntimeEvent::SpoolReplayed {
                peer_id: peer_id.to_string(),
                count: replayed,
            });
        }
    }

    fn initiate_session(&mut self, peer_id: &str, out: &mut EngineOutput) {        let Some(peer) = self
            .peers
            .iter()
            .find(|p| p.peer.device_id == peer_id)
            .cloned()
        else {
            return;
        };
        match create_session_init(&self.identity, &peer.peer) {
            Ok(init) => {
                let now = now_ms();
                let msg = init.msg.clone();
                let attempts = self
                    .pending_inits
                    .get(peer_id)
                    .map(|p| p.attempts + 1)
                    .unwrap_or(1);
                self.pending_inits.insert(
                    peer_id.to_string(),
                    PendingInit {
                        init,
                        last_attempt_ms: now,
                        attempts,
                    },
                );
                out.outbound.push(ClientToServer::SessionInit {
                    to: peer_id.to_string(),
                    msg,
                });
            }
            Err(e) => out.events.push(RuntimeEvent::Error {
                message: format!("创建会话握手失败（{peer_id}）: {e}"),
            }),
        }
    }

    /// 会话发起规则：
    /// - 正常由 device_id 字典序较小的一方发起（协议 §6），超时重试避免丢包导致不建连；
    /// - 兜底：若对端在线超过 [`SESSION_FALLBACK_DELAY_MS`] 仍无会话，较大的一方也发起。
    ///   用于"同 identity 新连接抢旧连接"：被抢占方重连后，对端看不到 offline 事件、
    ///   仍持有 stale session 不会重新握手，需要本端主动打破僵局。
    fn retry_sessions(&mut self, now: u64, out: &mut EngineOutput) {
        let online: Vec<String> = self.online.iter().cloned().collect();
        for peer_id in online {
            if peer_id == self.self_id {
                continue;
            }
            if self.sessions.contains_key(&peer_id) {
                continue;
            }
            let paired = self.peers.iter().any(|p| p.peer.device_id == peer_id);
            if !paired {
                continue;
            }
            let designated_initiator = self.self_id < peer_id;
            let should_init = match self.pending_inits.get(&peer_id) {
                None => {
                    if designated_initiator {
                        true
                    } else {
                        self.online_since
                            .get(&peer_id)
                            .map(|since| {
                                now.saturating_sub(*since) >= SESSION_FALLBACK_DELAY_MS
                            })
                            .unwrap_or(false)
                    }
                }
                Some(pending) => {
                    now.saturating_sub(pending.last_attempt_ms) >= SESSION_RETRY_MS
                        && pending.attempts < SESSION_MAX_ATTEMPTS
                }
            };
            if should_init {
                self.initiate_session(&peer_id, out);
            }
        }
    }

    // ------------------------------------------------------------------
    // 剪切板
    // ------------------------------------------------------------------

    /// 本机剪切板监听器上报变化（先进入 100ms 去抖窗口）。
    pub fn on_local_clipboard(&mut self, payload: ClipboardPayload) -> EngineOutput {
        let mut out = EngineOutput::default();
        if self.clipboard.paused {
            return out;
        }
        if payload.raw_size > crate::MAX_CLIPBOARD_BYTES {
            out.events.push(RuntimeEvent::Error {
                message: format!(
                    "剪切板内容 {} 字节超过单条上限，已跳过（建议改用文件中转）",
                    payload.raw_size
                ),
            });
            return out;
        }
        let now = now_ms();
        if let Some(pending) = &self.pending_local {
            if pending.payload.hash == payload.hash {
                return out;
            }
        }
        // 本机内容未变化 / 刚应用过远端同样内容 → 不上报（回环抑制第 1/3 条）。
        if self.last_local_hash.as_deref() == Some(payload.hash.as_str()) {
            return out;
        }
        if self.recent_applied.contains_fresh(&payload.hash, now) {
            return out;
        }
        self.pending_local = Some(PendingLocal {
            payload,
            due_at_ms: now + CLIPBOARD_DEBOUNCE_MS,
        });
        out
    }

    fn emit_clipboard_event(&mut self, payload: ClipboardPayload, out: &mut EngineOutput) {
        let now = now_ms();
        self.local_seq = self.local_seq.wrapping_add(1);
        let event = ClipboardEvent {
            v: 1,
            event_id: random_hex(8),
            origin_device: self.self_id.clone(),
            origin_seq: self.local_seq,
            created_at_ms: now,
            payload: payload.clone(),
        };
        self.last_local_hash = Some(payload.hash.clone());
        let kind = clipboard_kind_name(payload.kind);
        out.events.push(RuntimeEvent::ClipboardCaptured {
            kind,
            size: payload.raw_size,
        });

        let message = PeerMessage::Clipboard {
            event: event.clone(),
        };
        let paired: Vec<String> = self
            .peers
            .iter()
            .map(|p| p.peer.device_id.clone())
            .collect();
        let mut sent = 0usize;
        let mut queued = 0usize;
        for peer_id in paired {
            if self.sessions.contains_key(&peer_id)
                && self.send_peer(&peer_id, message.clone(), out)
            {
                sent += 1;
            } else {
                self.spool_message(&peer_id, message.clone(), event.origin_seq);
                queued += 1;
            }
        }
        if sent > 0 {
            out.events.push(RuntimeEvent::ClipboardSent {
                peers: sent,
                kind,
                size: payload.raw_size,
            });
        }
        if queued > 0 {
            out.events.push(RuntimeEvent::ClipboardSpooled { peers: queued });
        }
    }

    fn spool_message(&mut self, peer_id: &str, message: PeerMessage, origin_seq: u64) {
        let item = SpoolItem::new(peer_id, message, origin_seq, self.spool_settings.ttl_ms);
        if let Err(e) = self.spool.push(item) {
            tracing::warn!(error = %e, "写入离线待发队列失败");
        }
    }

    fn handle_remote_clipboard(
        &mut self,
        from: &str,
        event: ClipboardEvent,
        out: &mut EngineOutput,
    ) {
        let now = now_ms();
        // 回环抑制第 4 条：收到自己 origin 的事件直接丢弃。
        if event.origin_device == self.self_id {
            return;
        }
        // 幂等：event_id LRU 去重。
        if !self.seen_events.insert(event.event_id.clone(), now) {
            return;
        }
        if let Err(e) = event.payload.verify_integrity() {
            out.events.push(RuntimeEvent::Error {
                message: format!("剪切板载荷校验失败（{from}）: {e}"),
            });
            return;
        }
        if event.payload.raw_size > crate::MAX_CLIPBOARD_BYTES {
            out.events.push(RuntimeEvent::Error {
                message: format!("剪切板载荷超过上限（{from}）"),
            });
            return;
        }
        let hash = event.payload.hash.clone();
        // 回环抑制第 3 条：最近应用过的内容再次出现 → 忽略。
        if self.recent_applied.contains_fresh(&hash, now) {
            return;
        }
        // 内容与本机当前一致 → 无需重复应用。
        if self.last_local_hash.as_deref() == Some(hash.as_str()) {
            return;
        }
        self.recent_applied.insert(AppliedRecord {
            hash: hash.clone(),
            origin_device: from.to_string(),
            event_id: event.event_id.clone(),
            applied_at_ms: now,
        });
        self.last_local_hash = Some(hash.clone());
        // 如果本机监听器在应用前刚捕获到同一内容（例如 file 后端启动竞态），丢弃待发送项，
        // 避免把刚应用的内容再原样发回去。
        if self
            .pending_local
            .as_ref()
            .map(|pending| pending.payload.hash == hash)
            .unwrap_or(false)
        {
            self.pending_local = None;
        }
        out.applied_clipboard.push(event.payload.clone());
        out.events.push(RuntimeEvent::ClipboardApplied {
            peer_id: from.to_string(),
            kind: clipboard_kind_name(event.payload.kind),
            size: event.payload.raw_size,
        });
    }

    // ------------------------------------------------------------------
    // 目录
    // ------------------------------------------------------------------

    fn merge_catalog(&mut self, from: &str, ops: Vec<CatalogOp>, out: &mut EngineOutput) {
        let report = self.catalog.apply_ops(&ops);
        out.events.push(RuntimeEvent::CatalogMerged {
            from: from.to_string(),
            applied: report.applied,
            ignored: report.ignored,
            conflicts: report.conflicts,
        });
        if report.applied > 0 {
            self.catalog_revision += 1;
            if let Err(e) = self.persist_catalog() {
                out.events.push(RuntimeEvent::Error {
                    message: format!("持久化目录失败: {e}"),
                });
            }
        }
    }

    /// 本地添加软件目录条目（计算 BLAKE3、写本地索引、广播 delta）。
    pub fn catalog_add_local(
        &mut self,
        params: CatalogAddParams,
    ) -> Result<(CatalogEntry, EngineOutput)> {
        let path = params.path;
        let (size, hash) = hash_file(&path)?;
        let name = if params.name.trim().is_empty() {
            path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "未命名".to_string())
        } else {
            params.name
        };
        let entry = CatalogEntry::new(
            name,
            params.version,
            params.platform,
            params.kind,
            size,
            hash.clone(),
            params.tags,
            params.notes,
            &self.self_id,
        );
        let now = now_ms();
        let op = self.catalog.upsert_local(entry.clone(), &self.self_id, now);
        self.catalog_revision += 1;
        self.local_index.insert(
            entry.id.clone(),
            LocalIndexEntry {
                path,
                content_hash: hash,
                size,
                updated_at_ms: now,
            },
        );
        self.persist_catalog()?;
        self.persist_local_index()?;
        let mut out = EngineOutput::default();
        self.broadcast_catalog_ops(vec![op], &mut out);
        self.broadcast_have_list(&mut out);
        out.events.push(RuntimeEvent::CatalogUpdated {
            entry_id: entry.id.clone(),
            ops: 1,
        });
        Ok((entry, out))
    }

    /// 本地删除目录条目（写墓碑），并从本地索引移除。
    pub fn catalog_remove_local(&mut self, id: &str) -> Result<EngineOutput> {
        let now = now_ms();
        let mut out = EngineOutput::default();
        match self.catalog.delete_local(id, &self.self_id, now) {
            Some(op) => {
                self.catalog_revision += 1;
                self.local_index.remove(id);
                self.persist_catalog()?;
                self.persist_local_index()?;
                self.broadcast_catalog_ops(vec![op], &mut out);
                self.broadcast_have_list(&mut out);
                out.events.push(RuntimeEvent::CatalogUpdated {
                    entry_id: id.to_string(),
                    ops: 1,
                });
                Ok(out)
            }
            None => bail!("目录条目不存在或已删除: {id}"),
        }
    }

    fn broadcast_catalog_ops(&mut self, ops: Vec<CatalogOp>, out: &mut EngineOutput) {
        let peer_ids: Vec<String> = self.sessions.keys().cloned().collect();
        for peer_id in peer_ids {
            self.send_peer(
                &peer_id,
                PeerMessage::CatalogDelta {
                    ops: ops.clone(),
                    revision: self.catalog_revision,
                },
                out,
            );
        }
    }

    fn send_have_list_to(&mut self, peer_id: &str, out: &mut EngineOutput) {
        let entry_ids: Vec<String> = self.local_index.keys().cloned().collect();
        self.send_peer(peer_id, PeerMessage::HaveList { entry_ids }, out);
    }

    fn broadcast_have_list(&mut self, out: &mut EngineOutput) {
        let entry_ids: Vec<String> = self.local_index.keys().cloned().collect();
        let peer_ids: Vec<String> = self.sessions.keys().cloned().collect();
        for peer_id in peer_ids {
            self.send_peer(
                &peer_id,
                PeerMessage::HaveList {
                    entry_ids: entry_ids.clone(),
                },
                out,
            );
        }
    }

    // ------------------------------------------------------------------
    // 文件传输
    // ------------------------------------------------------------------

    fn handle_fetch_request(&mut self, from: &str, entry_id: &str, out: &mut EngineOutput) {
        // 同一对端重复请求同一条目：复用进行中的传输并补发一次 offer，避免覆盖 outgoing。
        if let Some(existing) = self
            .outgoing
            .values()
            .find(|transfer| {
                transfer.peer_id == from
                    && transfer.manifest.entry_id.as_deref() == Some(entry_id)
            })
            .map(|transfer| transfer.manifest.clone())
        {
            out.events.push(RuntimeEvent::TransferOffered {
                peer_id: from.to_string(),
                entry_id: existing.entry_id.clone(),
                size: existing.size,
                transfer_id: existing.transfer_id.clone(),
            });
            self.send_peer(from, PeerMessage::TransferOffer { manifest: existing }, out);
            return;
        }
        if self.outgoing.len() >= MAX_CONCURRENT_TRANSFERS {
            out.events.push(RuntimeEvent::Error {
                message: "并发传输已达上限（2），拒绝新的 fetch_request".to_string(),
            });
            return;
        }
        let Some(index) = self.local_index.get(entry_id).cloned() else {
            out.events.push(RuntimeEvent::Error {
                message: format!("没有该条目的本地文件: {entry_id}"),
            });
            return;
        };
        let file_mtime = self.store.file_mtime_ms(&index.path);
        // 报价缓存按 (对端, 条目) 区分：不同对端并发拉取同一 entry 时各用独立 transfer_id；
        // 同一对端重连后再次 fetch 时复用原 transfer_id，接收端 .part 可续传。
        let cache_key = (from.to_string(), entry_id.to_string());
        let cached = self.offer_cache.get(&cache_key).cloned();
        let manifest = match cached {
            Some(cached)
                if cached.size == index.size
                    && cached.content_hash == index.content_hash
                    && cached.file_mtime_ms == file_mtime =>
            {
                cached.manifest
            }
            _ => {
                let file_name = index
                    .path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file.bin".to_string());
                let mut manifest = TransferManifest::new(
                    file_name,
                    index.size,
                    index.content_hash.clone(),
                    Some(entry_id.to_string()),
                );
                // 防御：正常随机 id 不会碰撞，但绝不能让 outgoing 的 key 被覆盖。
                for _ in 0..8 {
                    if !self.outgoing.contains_key(&manifest.transfer_id) {
                        break;
                    }
                    manifest.transfer_id = format!("tx-{}", random_hex(8));
                }
                self.offer_cache.insert(
                    cache_key,
                    CachedOffer {
                        manifest: manifest.clone(),
                        size: index.size,
                        content_hash: index.content_hash.clone(),
                        file_mtime_ms: file_mtime,
                        expires_at_ms: now_ms() + OFFER_CACHE_TTL_MS,
                    },
                );
                manifest
            }
        };
        let file = match File::open(&index.path) {
            Ok(file) => file,
            Err(e) => {
                out.events.push(RuntimeEvent::Error {
                    message: format!("打开待发送文件失败（{entry_id}）: {e}"),
                });
                return;
            }
        };
        let now = now_ms();
        let transfer_id = manifest.transfer_id.clone();
        self.outgoing.insert(
            transfer_id.clone(),
            OutgoingTransfer {
                peer_id: from.to_string(),
                manifest: manifest.clone(),
                file,
                next_offset: 0,
                acked_offset: 0,
                done_sent: false,
                last_activity_ms: now,
            },
        );
        out.events.push(RuntimeEvent::TransferOffered {
            peer_id: from.to_string(),
            entry_id: manifest.entry_id.clone(),
            size: manifest.size,
            transfer_id,
        });
        self.send_peer(from, PeerMessage::TransferOffer { manifest }, out);
    }

    fn handle_transfer_offer(
        &mut self,
        from: &str,
        manifest: TransferManifest,
        out: &mut EngineOutput,
    ) {
        let transfer_id = manifest.transfer_id.clone();
        if let Err(e) = manifest.validate() {
            out.events.push(RuntimeEvent::TransferFailed {
                transfer_id,
                message: format!("传输清单非法: {e}"),
            });
            return;
        }
        // 防御：同一 transfer_id 已属于另一个对端（不该发生，除非对端实现有 bug/恶意）；
        // 拒绝而不是覆盖，避免两个接收器互相破坏。
        if let Some(existing) = self.incoming.get(&transfer_id) {
            if existing.peer_id != from {
                self.send_peer(
                    from,
                    PeerMessage::TransferReject {
                        transfer_id: transfer_id.clone(),
                        reason: "transfer_id 冲突（不同对端使用了相同 id）".to_string(),
                    },
                    out,
                );
                return;
            }
        }
        if self.incoming.len() >= MAX_CONCURRENT_TRANSFERS {
            self.send_peer(
                from,
                PeerMessage::TransferReject {
                    transfer_id: transfer_id.clone(),
                    reason: "并发传输已达上限".to_string(),
                },
                out,
            );
            return;
        }
        match PartialReceiver::open(&manifest, &self.store.incoming_dir()) {
            Ok(receiver) => {
                let from_offset = receiver.received_bytes();
                if from_offset > manifest.size {
                    receiver.discard();
                    self.send_peer(
                        from,
                        PeerMessage::TransferError {
                            transfer_id: transfer_id.clone(),
                            message: "断点偏移超过文件大小".to_string(),
                        },
                        out,
                    );
                    return;
                }
                let now = now_ms();
                out.events.push(RuntimeEvent::TransferStarted {
                    transfer_id: transfer_id.clone(),
                    entry_id: manifest.entry_id.clone(),
                    from_offset,
                    total: manifest.size,
                });
                self.incoming.insert(
                    transfer_id.clone(),
                    IncomingTransfer {
                        peer_id: from.to_string(),
                        manifest,
                        receiver,
                        chunks_since_ack: 0,
                        next_progress_bytes: TRANSFER_PROGRESS_STEP,
                        created_at_ms: now,
                    },
                );
                self.send_peer(
                    from,
                    PeerMessage::TransferAccept {
                        transfer_id,
                        from_offset,
                    },
                    out,
                );
            }
            Err(e) => {
                out.events.push(RuntimeEvent::TransferFailed {
                    transfer_id,
                    message: format!("创建接收器失败: {e}"),
                });
            }
        }
    }

    fn handle_transfer_accept(
        &mut self,
        from: &str,
        transfer_id: &str,
        from_offset: u64,
        out: &mut EngineOutput,
    ) {
        {
            let Some(transfer) = self.outgoing.get_mut(transfer_id) else {
                return;
            };
            if transfer.peer_id != from {
                return;
            }
            if from_offset > transfer.manifest.size {
                self.outgoing.remove(transfer_id);
                out.events.push(RuntimeEvent::TransferFailed {
                    transfer_id: transfer_id.to_string(),
                    message: "对端续传偏移超过文件大小".to_string(),
                });
                return;
            }
            let chunk_size = transfer.manifest.chunk_size as u64;
            let start = align_up(from_offset, chunk_size).min(transfer.manifest.size);
            transfer.next_offset = transfer.next_offset.max(start);
            transfer.acked_offset = transfer.acked_offset.max(from_offset.min(transfer.manifest.size));
            transfer.last_activity_ms = now_ms();
        }
        self.pump_outgoing(transfer_id, out);
    }

    fn handle_transfer_ack(
        &mut self,
        from: &str,
        transfer_id: &str,
        received_bytes: u64,
        out: &mut EngineOutput,
    ) {
        {
            let Some(transfer) = self.outgoing.get_mut(transfer_id) else {
                return;
            };
            if transfer.peer_id != from {
                return;
            }
            transfer.acked_offset = transfer
                .acked_offset
                .max(received_bytes.min(transfer.manifest.size));
            transfer.last_activity_ms = now_ms();
            tracing::trace!(
                transfer_id,
                received_bytes,
                acked_offset = transfer.acked_offset,
                "收到传输 ack"
            );
        }
        self.pump_outgoing(transfer_id, out);
    }

    fn pump_outgoing(&mut self, transfer_id: &str, out: &mut EngineOutput) {
        let Some(peer_id) = self
            .outgoing
            .get(transfer_id)
            .map(|transfer| transfer.peer_id.clone())
        else {
            return;
        };
        let mut messages: Vec<PeerMessage> = Vec::new();
        let mut read_error: Option<String> = None;
        {
            let Some(transfer) = self.outgoing.get_mut(transfer_id) else {
                return;
            };
            let chunk_size = transfer.manifest.chunk_size as u64;
            let total = transfer.manifest.size;
            loop {
                if transfer.done_sent {
                    break;
                }
                if transfer.next_offset >= total {
                    transfer.done_sent = true;
                    messages.push(PeerMessage::TransferDone {
                        transfer_id: transfer_id.to_string(),
                        total_bytes: total,
                    });
                    break;
                }
                let unacked = transfer.next_offset.saturating_sub(transfer.acked_offset);
                if unacked.div_ceil(chunk_size) >= TRANSFER_WINDOW_BLOCKS as u64 {
                    break;
                }
                let offset = transfer.next_offset;
                let len = (total - offset).min(chunk_size) as usize;
                let mut buf = vec![0u8; len];
                let read_result = transfer
                    .file
                    .seek(SeekFrom::Start(offset))
                    .and_then(|_| transfer.file.read_exact(&mut buf));
                if let Err(e) = read_result {
                    read_error = Some(format!("读取本地文件失败: {e}"));
                    break;
                }
                transfer.next_offset += len as u64;
                transfer.last_activity_ms = now_ms();
                messages.push(PeerMessage::TransferChunk {
                    transfer_id: transfer_id.to_string(),
                    offset,
                    data: buf,
                });
            }
        }
        if !messages.is_empty() {
            tracing::trace!(
                transfer_id,
                chunks = messages.len(),
                next_offset = self
                    .outgoing
                    .get(transfer_id)
                    .map(|transfer| transfer.next_offset)
                    .unwrap_or(0),
                "发送传输消息"
            );
        }
        if let Some(message) = read_error {
            self.outgoing.remove(transfer_id);
            self.send_peer(
                &peer_id,
                PeerMessage::TransferError {
                    transfer_id: transfer_id.to_string(),
                    message,
                },
                out,
            );
            return;
        }
        let mut done = false;
        for message in messages {
            if matches!(message, PeerMessage::TransferDone { .. }) {
                done = true;
            }
            if !self.send_peer(&peer_id, message, out) {
                break;
            }
        }
        if done {
            self.outgoing.remove(transfer_id);
        }
    }

    fn handle_transfer_chunk(
        &mut self,
        from: &str,
        transfer_id: &str,
        offset: u64,
        data: &[u8],
        out: &mut EngineOutput,
    ) {
        let mut ack: Option<u64> = None;
        let mut error: Option<String> = None;
        let mut progress: Option<(u64, u64)> = None;
        {
            let Some(transfer) = self.incoming.get_mut(transfer_id) else {
                return;
            };
            if transfer.peer_id != from {
                return;
            }
            match transfer.receiver.push_chunk(offset, data) {
                Ok(()) => {
                    transfer.chunks_since_ack += 1;
                    let received = transfer.receiver.received_bytes();
                    tracing::trace!(transfer_id, offset, received, "接收传输块");
                    let complete = transfer.receiver.is_complete();
                    if received >= transfer.next_progress_bytes || complete {
                        progress = Some((received, transfer.manifest.size));
                        transfer.next_progress_bytes = received + TRANSFER_PROGRESS_STEP;
                    }
                    if transfer.chunks_since_ack >= TRANSFER_ACK_EVERY_CHUNKS || complete {
                        transfer.chunks_since_ack = 0;
                        ack = Some(received);
                    }
                }
                Err(e) => error = Some(e.to_string()),
            }
        }
        if let Some((received, total)) = progress {
            out.events.push(RuntimeEvent::TransferProgress {
                transfer_id: transfer_id.to_string(),
                received,
                total,
            });
        }
        if let Some(message) = error {
            if let Some(transfer) = self.incoming.remove(transfer_id) {
                transfer.receiver.discard();
            }
            self.send_peer(
                from,
                PeerMessage::TransferError {
                    transfer_id: transfer_id.to_string(),
                    message: message.clone(),
                },
                out,
            );
            out.events.push(RuntimeEvent::TransferFailed {
                transfer_id: transfer_id.to_string(),
                message,
            });
        } else if let Some(received_bytes) = ack {
            self.send_peer(
                from,
                PeerMessage::TransferAck {
                    transfer_id: transfer_id.to_string(),
                    received_bytes,
                },
                out,
            );
        }
    }

    fn handle_transfer_done(
        &mut self,
        from: &str,
        transfer_id: &str,
        total_bytes: u64,
        out: &mut EngineOutput,
    ) {
        let Some(transfer) = self.incoming.remove(transfer_id) else {
            return;
        };
        let IncomingTransfer {
            manifest,
            receiver,
            ..
        } = transfer;
        let complete = receiver.received_bytes() == manifest.size && total_bytes == manifest.size;
        if !complete {
            receiver.discard();
            self.send_peer(
                from,
                PeerMessage::TransferError {
                    transfer_id: transfer_id.to_string(),
                    message: "接收完成信号与文件大小不一致".to_string(),
                },
                out,
            );
            out.events.push(RuntimeEvent::TransferFailed {
                transfer_id: transfer_id.to_string(),
                message: "接收完成信号与文件大小不一致".to_string(),
            });
            return;
        }
        match receiver.finish() {
            Ok(path) => {
                let entry_id = manifest.entry_id.clone();
                if let Some(entry_id) = &entry_id {
                    self.local_index.insert(
                        entry_id.clone(),
                        LocalIndexEntry {
                            path: path.clone(),
                            content_hash: manifest.content_hash.clone(),
                            size: manifest.size,
                            updated_at_ms: now_ms(),
                        },
                    );
                    if let Err(e) = self.persist_local_index() {
                        out.events.push(RuntimeEvent::Error {
                            message: format!("持久化本地索引失败: {e}"),
                        });
                    }
                    self.broadcast_have_list(out);
                }
                out.events.push(RuntimeEvent::TransferReceived {
                    transfer_id: transfer_id.to_string(),
                    entry_id,
                    path,
                    size: manifest.size,
                });
            }
            Err(e) => {
                self.send_peer(
                    from,
                    PeerMessage::TransferError {
                        transfer_id: transfer_id.to_string(),
                        message: format!("文件校验失败: {e}"),
                    },
                    out,
                );
                out.events.push(RuntimeEvent::TransferFailed {
                    transfer_id: transfer_id.to_string(),
                    message: format!("文件校验失败: {e}"),
                });
            }
        }
    }

    // ------------------------------------------------------------------
    // 云端暂存密钥共享
    // ------------------------------------------------------------------

    fn handle_vault_share(
        &mut self,
        from: &str,
        object_id: String,
        key_hex: String,
        meta: VaultMeta,
        out: &mut EngineOutput,
    ) {
        if key_hex.len() != 64 || !key_hex.chars().all(|c| c.is_ascii_hexdigit()) {
            out.events.push(RuntimeEvent::Error {
                message: format!("非法 vault 密钥（来自 {from}）"),
            });
            return;
        }
        let key = VaultKey {
            object_id: object_id.clone(),
            key_hex,
            meta,
            created_at_ms: now_ms(),
            ttl_secs: 0,
            shared_with: vec![from.to_string()],
        };
        if let Err(e) = self.store.upsert_vault_key(key.clone()) {
            out.events.push(RuntimeEvent::Error {
                message: format!("保存 vault 密钥失败: {e}"),
            });
            return;
        }
        match self.vault_keys.iter_mut().find(|k| k.object_id == object_id) {
            Some(existing) => *existing = key,
            None => self.vault_keys.push(key),
        }
        self.vault_mtime = self.store.file_mtime_ms(&self.store.vault_keys_path());
        out.events.push(RuntimeEvent::VaultKeyReceived {
            from: from.to_string(),
            object_id,
        });
    }

    fn distribute_vault_shares(&mut self, peer_id: &str, out: &mut EngineOutput) {
        let pending: Vec<(String, String, VaultMeta)> = self
            .vault_keys
            .iter()
            .filter(|key| !key.shared_with.iter().any(|id| id == peer_id))
            .map(|key| (key.object_id.clone(), key.key_hex.clone(), key.meta.clone()))
            .collect();
        let mut changed = false;
        for (object_id, key_hex, meta) in pending {
            if self.send_peer(
                peer_id,
                PeerMessage::VaultShare {
                    object_id: object_id.clone(),
                    key_hex,
                    meta,
                },
                out,
            ) {
                if let Some(key) = self
                    .vault_keys
                    .iter_mut()
                    .find(|key| key.object_id == object_id)
                {
                    key.shared_with.push(peer_id.to_string());
                    changed = true;
                }
                out.events.push(RuntimeEvent::VaultShared {
                    peer_id: peer_id.to_string(),
                    object_id,
                });
            } else {
                break;
            }
        }
        if changed {
            if let Err(e) = self.store.save_vault_keys(&self.vault_keys) {
                out.events.push(RuntimeEvent::Error {
                    message: format!("持久化 vault 密钥失败: {e}"),
                });
            }
            self.vault_mtime = self.store.file_mtime_ms(&self.store.vault_keys_path());
        }
    }

    /// 检测其他进程（如 `derive-sync catalog add` / `vault put`）对本地状态文件的更新：
    /// 重新加载并广播给在线会话，避免"CLI 改了但 daemon 内存还是旧的"。
    fn detect_external_state_changes(&mut self, _now: u64, out: &mut EngineOutput) {
        // 目录（catalog.json）
        let path = self.store.catalog_path();
        let mtime = self.store.file_mtime_ms(&path);
        if mtime != self.catalog_mtime {
            match self.store.load_catalog() {
                Ok((catalog, revision)) => {
                    self.catalog = catalog;
                    self.catalog_revision = self.catalog_revision.max(revision);
                    self.catalog_mtime = mtime;
                    let ops = self.catalog.to_sync_ops();
                    let peer_ids: Vec<String> = self.sessions.keys().cloned().collect();
                    for peer_id in peer_ids {
                        self.send_peer(
                            &peer_id,
                            PeerMessage::CatalogSnapshot {
                                ops: ops.clone(),
                                revision: self.catalog_revision,
                            },
                            out,
                        );
                    }
                    out.events.push(RuntimeEvent::CatalogReloaded {
                        entries: self.catalog.visible_count(),
                    });
                }
                Err(e) => {
                    self.catalog_mtime = mtime;
                    tracing::warn!(error = %e, "重新加载 catalog.json 失败");
                }
            }
        }
        // 本地索引（local_index.json）
        let path = self.store.local_index_path();
        let mtime = self.store.file_mtime_ms(&path);
        if mtime != self.local_index_mtime {
            match self.store.load_local_index() {
                Ok(index) => {
                    self.local_index = index;
                    self.local_index_mtime = mtime;
                    self.broadcast_have_list(out);
                }
                Err(e) => {
                    self.local_index_mtime = mtime;
                    tracing::warn!(error = %e, "重新加载 local_index.json 失败");
                }
            }
        }
        // 云端暂存密钥（vault_keys.json）
        self.detect_external_vault_changes(_now, out);
    }

    /// 检测其他进程（如 `derive-sync vault put`）对 vault_keys.json 的更新，并补发 vault_share。
    fn detect_external_vault_changes(&mut self, _now: u64, out: &mut EngineOutput) {
        let path = self.store.vault_keys_path();
        let mtime = self.store.file_mtime_ms(&path);
        if mtime == self.vault_mtime {
            return;
        }
        match self.store.load_vault_keys() {
            Ok(keys) => {
                if keys.len() != self.vault_keys.len() {
                    tracing::info!(objects = keys.len(), "检测到 vault_keys.json 更新");
                }
                self.vault_keys = keys;
                self.vault_mtime = mtime;
                let peer_ids: Vec<String> = self.sessions.keys().cloned().collect();
                for peer_id in peer_ids {
                    self.distribute_vault_shares(&peer_id, out);
                }
            }
            Err(e) => {
                self.vault_mtime = mtime;
                tracing::warn!(error = %e, "重新加载 vault_keys.json 失败");
            }
        }
    }

    /// 检测其他进程对 `peers.json` 的写入（`derive-sync peers remove` / `pair`）：
    /// - 被删除的对端：先经现有会话尽力发送 `PairRevoked`，再销毁关系并 emit 事件；
    /// - 新增/更新的对端：重新加载，后续 presence/兜底逻辑会正常建立会话。
    fn detect_external_peer_changes(&mut self, now: u64, out: &mut EngineOutput) {
        let path = self.store.peers_path();
        let mtime = self.store.file_mtime_ms(&path);
        if mtime == self.peers_mtime {
            return;
        }
        let disk_peers = match self.store.load_peers() {
            Ok(peers) => peers,
            Err(error) => {
                self.peers_mtime = mtime;
                tracing::warn!(error = %error, "重新加载 peers.json 失败");
                return;
            }
        };
        self.peers_mtime = mtime;
        let disk_ids: BTreeSet<String> = disk_peers
            .iter()
            .map(|peer| peer.peer.device_id.clone())
            .collect();
        let removed_ids: Vec<String> = self
            .peers
            .iter()
            .map(|peer| peer.peer.device_id.clone())
            .filter(|id| !disk_ids.contains(id))
            .collect();

        for peer_id in removed_ids {
            let name = self
                .peers
                .iter()
                .find(|peer| peer.peer.device_id == peer_id)
                .map(|peer| peer.display_name())
                .unwrap_or_else(|| peer_id.clone());
            // 1) 先尽力经现有 E2E 会话通知对端（离线则下次握手会被拒绝，安全兜底）。
            let reason = "本机已删除配对记录".to_string();
            let notified = self.send_peer(
                &peer_id,
                PeerMessage::PairRevoked {
                    reason: reason.clone(),
                },
                out,
            );
            // 2) 销毁关系（会话/传输/spool/缓存）。
            self.cleanup_peer_relationship(&peer_id, out);
            out.events.push(RuntimeEvent::PeerRevoked {
                device_id: peer_id.clone(),
                name,
                reason,
                notified,
            });
        }

        // 新增或字段更新的配对记录。
        let mut changes = 0usize;
        for record in disk_peers {
            match self
                .peers
                .iter_mut()
                .find(|peer| peer.peer.device_id == record.peer.device_id)
            {
                Some(slot) => *slot = record,
                None => {
                    self.peers.push(record);
                    changes += 1;
                }
            }
        }
        self.peers
            .retain(|peer| disk_ids.contains(&peer.peer.device_id));
        self.peers
            .sort_by(|a, b| a.peer.device_id.cmp(&b.peer.device_id));
        if changes > 0 {
            out.events.push(RuntimeEvent::PeersReloaded {
                count: self.peers.len(),
            });
            // 新增的在线对端：按正常规则（小 id 立即 / 较大侧兜底延迟）建立会话。
            self.retry_sessions(now, out);
        }
    }

    // ------------------------------------------------------------------
    // 周期任务
    // ------------------------------------------------------------------

    /// 周期驱动（Runtime 每 100ms 调用一次）：去抖、TTL 清理、握手重试、外部文件检测。
    pub fn on_tick(&mut self, now: u64) -> EngineOutput {
        let mut out = EngineOutput::default();

        // 1) 剪切板去抖到期 → 生成事件并广播。
        if let Some(pending) = self.pending_local.take() {
            if now >= pending.due_at_ms {
                self.emit_clipboard_event(pending.payload, &mut out);
            } else {
                self.pending_local = Some(pending);
            }
        }

        // 2) LRU 清理。
        self.recent_applied.prune(now);
        self.seen_events.prune(now);

        // 3) spool 过期清理（每 60s）。
        if now.saturating_sub(self.last_spool_prune_ms) >= 60_000 {
            self.last_spool_prune_ms = now;
            if let Ok(removed) = self.spool.prune_expired(now) {
                if removed > 0 {
                    tracing::debug!(removed, "清理过期 spool 条目");
                }
            }
        }

        // 4) 外部状态文件变更检测（每 2s）。
        if now.saturating_sub(self.last_vault_check_ms) >= 2_000 {
            self.last_vault_check_ms = now;
            self.detect_external_state_changes(now, &mut out);
            self.detect_external_peer_changes(now, &mut out);
        }

        // 5) 会话握手重试。
        self.retry_sessions(now, &mut out);

        // 6) 清理过期传输缓存与僵死传输。
        self.offer_cache
            .retain(|_, cached| cached.expires_at_ms > now);
        let stale_before = now.saturating_sub(10 * 60 * 1000);
        self.outgoing
            .retain(|_, transfer| transfer.last_activity_ms >= stale_before);
        self.incoming
            .retain(|_, transfer| transfer.created_at_ms >= stale_before);

        out
    }

    // ------------------------------------------------------------------
    // 命令
    // ------------------------------------------------------------------

    pub fn handle_command(&mut self, command: RuntimeCommand) -> EngineOutput {
        match command {
            RuntimeCommand::SetClipboard(payload) => self.on_local_clipboard(payload),
            RuntimeCommand::CatalogAdd { params, reply } => {
                let result = self.catalog_add_local(params);
                match result {
                    Ok((entry, out)) => {
                        if let Some(reply) = reply {
                            let _ = reply.send(Ok(entry.id.clone()));
                        }
                        out
                    }
                    Err(e) => {
                        if let Some(reply) = reply {
                            let _ = reply.send(Err(e.to_string()));
                        }
                        let mut out = EngineOutput::default();
                        out.events.push(RuntimeEvent::Error {
                            message: format!("添加目录条目失败: {e}"),
                        });
                        out
                    }
                }
            }
            RuntimeCommand::CatalogRemove { id, reply } => {
                let result = self.catalog_remove_local(&id);
                match result {
                    Ok(out) => {
                        if let Some(reply) = reply {
                            let _ = reply.send(Ok(()));
                        }
                        out
                    }
                    Err(e) => {
                        if let Some(reply) = reply {
                            let _ = reply.send(Err(e.to_string()));
                        }
                        let mut out = EngineOutput::default();
                        out.events.push(RuntimeEvent::Error {
                            message: format!("删除目录条目失败: {e}"),
                        });
                        out
                    }
                }
            }
            RuntimeCommand::Fetch { peer_id, entry_id } => {
                self.request_fetch(&peer_id, &entry_id)
            }
        }
    }

    /// 向对端请求传输某个软件本体。
    pub fn request_fetch(&mut self, peer_id: &str, entry_id: &str) -> EngineOutput {
        let mut out = EngineOutput::default();
        self.send_peer(
            peer_id,
            PeerMessage::FetchRequest {
                entry_id: entry_id.to_string(),
            },
            &mut out,
        );
        out
    }

    // ------------------------------------------------------------------
    // 内部：发送 / 持久化 / 通用
    // ------------------------------------------------------------------

    /// 用会话密钥加密并加入待发送队列。返回是否成功（失败说明无会话或加密错误）。
    fn send_peer(&mut self, peer_id: &str, message: PeerMessage, out: &mut EngineOutput) -> bool {
        let Some(session) = self.sessions.get_mut(peer_id) else {
            return false;
        };
        let plaintext = match message.to_bytes() {
            Ok(bytes) => bytes,
            Err(e) => {
                out.events.push(RuntimeEvent::Error {
                    message: format!("PeerMessage 编码失败: {e}"),
                });
                return false;
            }
        };
        match session.encrypt("peer", &plaintext) {
            Ok(envelope) => {
                out.outbound.push(ClientToServer::Relay {
                    to: peer_id.to_string(),
                    envelope,
                });
                true
            }
            Err(e) => {
                out.events.push(RuntimeEvent::Error {
                    message: format!("会话加密失败（{peer_id}）: {e}"),
                });
                false
            }
        }
    }

    fn persist_catalog(&mut self) -> Result<()> {
        self.store
            .save_catalog(&self.catalog, self.catalog_revision)
            .map_err(|e| anyhow!("保存目录失败: {e}"))?;
        self.catalog_mtime = self.store.file_mtime_ms(&self.store.catalog_path());
        Ok(())
    }

    fn persist_local_index(&mut self) -> Result<()> {
        self.store
            .save_local_index(&self.local_index)
            .map_err(|e| anyhow!("保存本地索引失败: {e}"))?;
        self.local_index_mtime = self.store.file_mtime_ms(&self.store.local_index_path());
        Ok(())
    }

    /// 供 CLI/状态展示：当前待发送（离线）消息数量。
    pub fn spool_count(&self) -> usize {
        self.spool.count().unwrap_or(0)
    }

    /// 供测试/诊断：当前是否有待处理的本机剪切板变化。
    pub fn has_pending_local_clipboard(&self) -> bool {
        self.pending_local.is_some()
    }

    /// 供测试/诊断：`recent_applied` 中的内容哈希数量。
    pub fn recent_applied_len(&self) -> usize {
        self.recent_applied.len()
    }

    /// 供测试/诊断：`recent_applied` 是否包含某内容哈希（不清理 TTL）。
    pub fn recent_applied_contains(&self, hash: &str) -> bool {
        self.recent_applied.get(hash).is_some()
    }

    /// 供 e2e：请求对端重发全量目录快照。
    pub fn request_catalog_snapshot(&mut self, peer_id: &str) -> EngineOutput {
        let mut out = EngineOutput::default();
        self.send_peer(peer_id, PeerMessage::CatalogRequest, &mut out);
        out
    }
}

/// 剪切板类型名（日志/事件用，避免 Debug 输出）。
pub fn clipboard_kind_name(kind: ClipboardKind) -> &'static str {
    match kind {
        ClipboardKind::Text => "text",
        ClipboardKind::RichText => "rich_text",
        ClipboardKind::Image => "image",
        ClipboardKind::Files => "files",
    }
}

/// 内容哈希摘要（只取前 8 字符，供诊断日志）。
pub fn hash_short(hash: &str) -> &str {
    if hash.len() > 8 {
        &hash[..8]
    } else {
        hash
    }
}

#[cfg(test)]
mod tests;
