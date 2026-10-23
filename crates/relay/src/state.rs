//! 中继共享状态：在线设备表、配对登记表、失败限流、可选暂存。
//!
//! 并发模型：每个连接一个读任务（+ 一个写任务），共享状态用 `Arc<RelayState>`，
//! 在线表用 `RwLock`、配对表用 `Mutex`，跨连接投递用每条连接的有界 `mpsc`。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use derive_core::wire::ServerToClient;
use tokio::sync::{mpsc, watch, Mutex, RwLock};

use crate::pair::PairRegistry;
use crate::ratelimit::JoinLimiter;
use crate::vault::VaultStore;
use crate::RelayConfig;

/// 出站消息：JSON 协议帧或关闭帧。
///
/// `Json` 变体较大（`ServerToClient` 含 base64 密文可选字段），但每条连接只有一个
/// 有界通道；为了发送路径简单不做 Box。
#[allow(clippy::large_enum_variant)]
pub(crate) enum OutMsg {
    Json(ServerToClient),
    Close {
        code: u16,
        reason: &'static str,
    },
}

/// 一条已认证连接的可投递句柄。
#[derive(Clone)]
pub(crate) struct PeerConn {
    pub(crate) device_id: String,
    pub(crate) session_id: String,
    pub(crate) tx: mpsc::Sender<OutMsg>,
    /// 置为 `true` 表示服务端要求该连接主动断开（同设备重连时踢旧连接）。
    pub(crate) kick: watch::Sender<bool>,
}

pub(crate) struct RelayState {
    pub(crate) config: Arc<RelayConfig>,
    pub(crate) peers: RwLock<HashMap<String, PeerConn>>,
    pub(crate) pairs: Mutex<PairRegistry>,
    pub(crate) join_limiter: JoinLimiter,
    pub(crate) vault: Option<VaultStore>,
    /// 关停广播：置为 `true` 后所有在线连接任务应发 Close(1001) 并退出。
    shutdown: watch::Sender<bool>,
}

impl RelayState {
    pub(crate) fn new(config: RelayConfig, vault: Option<VaultStore>) -> Arc<Self> {
        let pair_ttl = Duration::from_secs(config.pair_ttl_secs.max(1));
        let (shutdown, _) = watch::channel(false);
        Arc::new(Self {
            join_limiter: JoinLimiter::new(config.pair_join_limit_per_min),
            pairs: Mutex::new(PairRegistry::new(pair_ttl)),
            vault,
            peers: RwLock::new(HashMap::new()),
            shutdown,
            config: Arc::new(config),
        })
    }

    /// 通知所有在线连接「中继正在关停」：连接任务会发送 Close(1001) 并退出。
    pub(crate) fn begin_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// 每条连接持有的关停信号接收端。
    pub(crate) fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    /// 等待在线连接全部注销（关闭帧发送/任务退出），最多等待 `timeout`。
    /// 返回是否在超时前清空；用于 `RelayHandle::shutdown` 的优雅停机。
    pub(crate) async fn wait_until_no_peers(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.peers.read().await.is_empty() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// 当前在线设备 id（字典序，供 welcome / presence 使用）。
    pub(crate) async fn online_ids(&self) -> Vec<String> {
        let peers = self.peers.read().await;
        let mut ids: Vec<String> = peers.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// 查在线连接句柄。
    pub(crate) async fn peer(&self, device_id: &str) -> Option<PeerConn> {
        self.peers.read().await.get(device_id).cloned()
    }

    /// 向所有在线客户端广播 presence（通道满则丢弃该条，不阻塞其他连接）。
    pub(crate) async fn broadcast_presence(&self) {
        let (online, senders) = {
            let peers = self.peers.read().await;
            let mut online: Vec<String> = peers.keys().cloned().collect();
            online.sort();
            let senders: Vec<mpsc::Sender<OutMsg>> = peers.values().map(|p| p.tx.clone()).collect();
            (online, senders)
        };
        let message = ServerToClient::Presence { online };
        for tx in senders {
            let _ = tx.try_send(OutMsg::Json(message.clone()));
        }
    }

    /// 注销连接；仅当 `session_id` 匹配时移除（防止被踢的旧连接误删新连接）。
    /// 返回是否真的移除了连接（移除后广播 presence）。
    pub(crate) async fn unregister_peer(&self, device_id: &str, session_id: &str) -> bool {
        let removed = {
            let mut peers = self.peers.write().await;
            let matches = peers
                .get(device_id)
                .map(|peer| peer.session_id == session_id)
                .unwrap_or(false);
            if matches {
                peers.remove(device_id);
            }
            matches
        };
        if removed {
            self.broadcast_presence().await;
        }
        removed
    }
}

// ---------------------------------------------------------------------------
// 出站发送辅助（均为非阻塞 try_send；通道满表示对端消费过慢）
// ---------------------------------------------------------------------------

pub(crate) fn try_send_json(tx: &mpsc::Sender<OutMsg>, message: ServerToClient) -> bool {
    tx.try_send(OutMsg::Json(message)).is_ok()
}

pub(crate) fn try_send_error(tx: &mpsc::Sender<OutMsg>, code: &str, message: &str) -> bool {
    try_send_json(
        tx,
        ServerToClient::Error {
            code: code.to_string(),
            message: message.to_string(),
        },
    )
}

pub(crate) fn try_send_close(
    tx: &mpsc::Sender<OutMsg>,
    code: u16,
    reason: &'static str,
) -> bool {
    tx.try_send(OutMsg::Close { code, reason }).is_ok()
}
