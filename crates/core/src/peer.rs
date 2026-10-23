//! 设备间端到端消息（解密后由对端处理）。

use serde::{Deserialize, Serialize};

use crate::catalog::CatalogOp;
use crate::clipboard::ClipboardEvent;
use crate::transfer::TransferManifest;
use crate::vault::VaultMeta;

/// 设备间业务消息。外层再由 E2E 会话加密，中继不可见。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "m", rename_all = "snake_case")]
pub enum PeerMessage {
    /// 剪切板内容变化。
    Clipboard { event: ClipboardEvent },
    /// 目录全量快照（新配对/重连首次同步）。
    CatalogSnapshot { ops: Vec<CatalogOp>, revision: u64 },
    /// 目录增量操作。
    CatalogDelta { ops: Vec<CatalogOp>, revision: u64 },
    /// 请求对端发送目录全量快照。
    CatalogRequest,
    /// 本机持有的软件条目 id 列表（用于"谁有这份软件"的可用性）。
    HaveList { entry_ids: Vec<String> },
    /// 请求对端把某个软件传给本机。
    FetchRequest { entry_id: String },
    /// 文件传输报价。
    TransferOffer { manifest: TransferManifest },
    /// 接受传输，从 `from_offset` 续传。
    TransferAccept {
        transfer_id: String,
        from_offset: u64,
    },
    TransferReject {
        transfer_id: String,
        reason: String,
    },
    TransferChunk {
        transfer_id: String,
        offset: u64,
        #[serde(with = "crate::util::b64_vec")]
        data: Vec<u8>,
    },
    /// 接收端确认已连续收到的字节数（流控窗口，每 8 块或 512 KiB 回一次）。
    TransferAck { transfer_id: String, received_bytes: u64 },
    TransferDone {
        transfer_id: String,
        total_bytes: u64,
    },
    TransferError {
        transfer_id: String,
        message: String,
    },
    /// 把云端暂存对象的解密密钥共享给已配对设备（经 E2E 通道）。
    VaultShare {
        object_id: String,
        key_hex: String,
        meta: VaultMeta,
    },
    /// 应用层心跳（穿透 NAT/保持连接；桌面端 30~60s）。
    Ping { ts_ms: u64 },
    Pong { ts_ms: u64 },
    /// 对端已解除配对（撤销信任）。
    ///
    /// 仅在已有 E2E 会话时尽力发送；接收方应立即删除本地 PairRecord、销毁会话、
    /// 清理该对端的待发队列，此后不再与该设备同步。若对端离线收不到，
    /// 下次会话握手会因发起方已无配对记录而被拒绝（安全兜底）。
    PairRevoked { reason: String },
}

impl PeerMessage {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Clipboard { .. } => "clipboard",
            Self::CatalogSnapshot { .. } => "catalog_snapshot",
            Self::CatalogDelta { .. } => "catalog_delta",
            Self::CatalogRequest => "catalog_request",
            Self::HaveList { .. } => "have_list",
            Self::FetchRequest { .. } => "fetch_request",
            Self::TransferOffer { .. } => "transfer_offer",
            Self::TransferAccept { .. } => "transfer_accept",
            Self::TransferReject { .. } => "transfer_reject",
            Self::TransferChunk { .. } => "transfer_chunk",
            Self::TransferAck { .. } => "transfer_ack",
            Self::TransferDone { .. } => "transfer_done",
            Self::TransferError { .. } => "transfer_error",
            Self::VaultShare { .. } => "vault_share",
            Self::Ping { .. } => "ping",
            Self::Pong { .. } => "pong",
            Self::PairRevoked { .. } => "pair_revoked",
        }
    }

    pub fn to_bytes(&self) -> crate::Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn from_bytes(data: &[u8]) -> crate::Result<Self> {
        Ok(serde_json::from_slice(data)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::ClipboardPayload;

    #[test]
    fn roundtrip_clipboard_message() {
        let event = ClipboardEvent {
            v: 1,
            event_id: "e1".into(),
            origin_device: "dev-a".into(),
            origin_seq: 7,
            created_at_ms: 1,
            payload: ClipboardPayload::text("hi").unwrap(),
        };
        let msg = PeerMessage::Clipboard { event };
        let bytes = msg.to_bytes().unwrap();
        let decoded = PeerMessage::from_bytes(&bytes).unwrap();
        match decoded {
            PeerMessage::Clipboard { event } => {
                assert_eq!(event.payload.as_text().unwrap(), "hi");
                assert_eq!(event.origin_seq, 7);
            }
            _ => panic!("类型错误"),
        }
    }
}
