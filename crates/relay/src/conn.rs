//! 单条 WebSocket 连接：hello 认证、welcome/presence、消息路由、限速与空闲超时。
//!
//! 每条连接 = 一个读任务（本模块 `run_connection`）+ 一个写任务（writer）。
//! 业务消息只做转发：`session_init` / `session_ack` / `relay`；中继不解析、不记录 ciphertext。
//! 发往离线设备的帧不缓存，直接回 `error{code:"peer_offline"}`。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use drive_core::identity::PublicIdentity;
use drive_core::revocation::RevocationNotice;
use drive_core::util::{now_ms, random_hex};
use drive_core::vault::VaultMeta;
use drive_core::wire::{ClientToServer, ServerToClient, WIRE_VERSION};
use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::auth;
use crate::pair;
use crate::ratelimit::TokenBucket;
use crate::revocation::{self, AckOutcome};
use crate::state::{try_send_close, try_send_error, try_send_json, OutMsg, PeerConn, RelayState};

// 应用自定义关闭码（RFC 6455 4000-4999 区间）+ 标准 1001 关停码。
const CLOSE_AUTH_FAILED: u16 = 4001;
const CLOSE_SESSION_REPLACED: u16 = 4002;
const CLOSE_IDLE_TIMEOUT: u16 = 4003;
const CLOSE_PROTOCOL_ERROR: u16 = 4004;
/// 标准 1001 (going away)：中继优雅停机时通知在线客户端主动断开/重连。
const CLOSE_GOING_AWAY: u16 = 1001;

/// `GET /ws`：升级为 WebSocket，每条连接交给独立任务处理。
pub(crate) async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<RelayState>>,
) -> Response {
    let max_frame = state.config.max_frame_bytes;
    ws.max_message_size(max_frame)
        .max_frame_size(max_frame)
        .on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<RelayState>) {
    // ------------------------------------------------------------------
    // 1) 首帧 hello 认证
    // ------------------------------------------------------------------
    let first = match read_first_frame(&mut socket, &state).await {
        Ok(frame) => frame,
        Err(HandshakeError::Disconnected) => return,
        Err(HandshakeError::Timeout) => {
            send_direct_error(&mut socket, "auth_failed", "认证超时").await;
            close_direct(&mut socket, CLOSE_AUTH_FAILED, "auth timeout").await;
            return;
        }
        Err(HandshakeError::BadFrame) => {
            send_direct_error(&mut socket, "auth_failed", "首帧必须是合法的 hello 消息").await;
            close_direct(&mut socket, CLOSE_AUTH_FAILED, "bad hello").await;
            return;
        }
    };

    let device = match auth::authenticate(&state.config, first) {
        Ok(device) => device,
        Err(failure) => {
            debug!(code = failure.code, "连接认证失败");
            send_direct_error(&mut socket, failure.code, &failure.message).await;
            close_direct(&mut socket, CLOSE_AUTH_FAILED, "auth failed").await;
            return;
        }
    };
    let device_id = device.device_id.clone();
    let session_id = random_hex(8);

    // ------------------------------------------------------------------
    // 2) 注册在线表（同 device_id 踢旧连接；allow_multi_conn 时拒绝新连接）
    // ------------------------------------------------------------------
    let capacity = state.config.send_queue_capacity.max(16);
    let (out_tx, out_rx) = mpsc::channel::<OutMsg>(capacity);
    let (kick_tx, mut kick_rx) = watch::channel(false);
    // 中继关停广播：RelayHandle::shutdown 会置 true，所有连接发 Close(1001) 退出。
    let mut shutdown_rx = state.shutdown_receiver();
    let new_peer = PeerConn {
        device_id: device_id.clone(),
        session_id: session_id.clone(),
        tx: out_tx.clone(),
        kick: kick_tx,
    };

    let replaced = {
        let mut peers = state.peers.write().await;
        if state.config.allow_multi_conn && peers.contains_key(&device_id) {
            drop(peers);
            send_direct_error(&mut socket, "auth_failed", "该设备已在其他连接上在线").await;
            close_direct(&mut socket, CLOSE_AUTH_FAILED, "device already online").await;
            return;
        }
        peers.insert(device_id.clone(), new_peer)
    };
    if let Some(old) = replaced {
        info!(device = %device_id, "同设备新连接接入，断开旧连接");
        let _ = old.kick.send(true);
    }

    // ------------------------------------------------------------------
    // 3) welcome + presence 广播
    // ------------------------------------------------------------------
    let online = state.online_ids().await;
    let welcome = ServerToClient::Welcome {
        v: WIRE_VERSION,
        device_id: device_id.clone(),
        session_id: session_id.clone(),
        server_time_ms: now_ms(),
        online,
    };
    if send_direct(&mut socket, &welcome).await.is_err() {
        state.unregister_peer(&device_id, &session_id).await;
        return;
    }

    // 登录后推送离线期间积压的解除配对通知（welcome 之后的第一批业务消息；
    // 投递不改变存储状态，ack 前每次登录都会重推，保证幂等可达）。
    let pending_revocations = state.revocations.pending_for(&device_id).await;
    if !pending_revocations.is_empty() {
        debug!(
            device = %device_id,
            count = pending_revocations.len(),
            "推送待处理解除配对通知"
        );
        if !try_send_json(
            &out_tx,
            ServerToClient::PendingRevocations {
                notices: pending_revocations,
            },
        ) {
            warn!(device = %device_id, "推送解除配对通知失败，待下次登录重试");
        }
    }

    info!(device = %device_id, "设备已认证上线");
    state.broadcast_presence().await;

    // ------------------------------------------------------------------
    // 4) 拆分为读/写任务，进入消息循环
    // ------------------------------------------------------------------
    let (mut sink, mut stream) = socket.split();
    let writer = tokio::spawn(async move {
        let mut out_rx = out_rx;
        while let Some(out) = out_rx.recv().await {
            match out {
                OutMsg::Json(message) => match message.to_json() {
                    Ok(json) => {
                        if sink.send(Message::Text(json)).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => warn!(%error, "服务端消息序列化失败"),
                },
                OutMsg::Close { code, reason } => {
                    let _ = sink
                        .send(Message::Close(Some(close_frame(code, reason))))
                        .await;
                    break;
                }
            }
        }
        let _ = sink.close().await;
    });

    let close_reason = run_connection(
        &state,
        &device,
        &out_tx,
        &mut kick_rx,
        &mut shutdown_rx,
        &mut stream,
    )
    .await;

    // ------------------------------------------------------------------
    // 5) 清理：注销在线表（仅移除本次连接）+ 关闭写任务
    // ------------------------------------------------------------------
    drop(out_tx);
    state.unregister_peer(&device_id, &session_id).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    debug!(device = %device_id, reason = close_reason, "WebSocket 连接已结束");
    info!(device = %device_id, "设备离线");
}

// ---------------------------------------------------------------------------
// 握手
// ---------------------------------------------------------------------------

enum HandshakeError {
    Timeout,
    BadFrame,
    Disconnected,
}

async fn read_first_frame(
    socket: &mut WebSocket,
    state: &RelayState,
) -> Result<ClientToServer, HandshakeError> {
    let timeout_secs = state.config.handshake_timeout_secs.max(1);
    let collected = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        loop {
            match socket.recv().await {
                Some(Ok(Message::Text(text))) => return Some(text),
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
                Some(Ok(Message::Binary(_)))
                | Some(Ok(Message::Close(_)))
                | Some(Err(_))
                | None => return None,
            }
        }
    })
    .await;

    match collected {
        Err(_) => Err(HandshakeError::Timeout),
        Ok(None) => Err(HandshakeError::Disconnected),
        Ok(Some(text)) => {
            if text.len() > state.config.max_frame_bytes {
                return Err(HandshakeError::BadFrame);
            }
            ClientToServer::from_json(&text).map_err(|_| HandshakeError::BadFrame)
        }
    }
}

async fn send_direct(socket: &mut WebSocket, message: &ServerToClient) -> Result<(), ()> {
    let json = message.to_json().map_err(|_| ())?;
    socket.send(Message::Text(json)).await.map_err(|_| ())
}

async fn send_direct_error(socket: &mut WebSocket, code: &str, message: &str) {
    let error = ServerToClient::Error {
        code: code.to_string(),
        message: message.to_string(),
    };
    let _ = send_direct(socket, &error).await;
}

async fn close_direct(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(close_frame(code, reason))))
        .await;
}

fn close_frame(code: u16, reason: &'static str) -> CloseFrame<'static> {
    CloseFrame {
        code,
        reason: reason.into(),
    }
}

// ---------------------------------------------------------------------------
// 读循环
// ---------------------------------------------------------------------------

enum FrameEvent {
    Message(Result<Message, axum::Error>),
    Idle,
    End,
}

async fn next_frame(stream: &mut SplitStream<WebSocket>, idle: Option<Duration>) -> FrameEvent {
    match idle {
        Some(timeout) => match tokio::time::timeout(timeout, stream.next()).await {
            Err(_) => FrameEvent::Idle,
            Ok(Some(message)) => FrameEvent::Message(message),
            Ok(None) => FrameEvent::End,
        },
        None => match stream.next().await {
            Some(message) => FrameEvent::Message(message),
            None => FrameEvent::End,
        },
    }
}

async fn run_connection(
    state: &Arc<RelayState>,
    device: &PublicIdentity,
    out_tx: &mpsc::Sender<OutMsg>,
    kick_rx: &mut watch::Receiver<bool>,
    shutdown_rx: &mut watch::Receiver<bool>,
    stream: &mut SplitStream<WebSocket>,
) -> &'static str {
    let device_id = device.device_id.as_str();
    let idle = if state.config.idle_timeout_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(state.config.idle_timeout_secs))
    };
    let mut bucket = TokenBucket::new(
        state.config.rate_limit_per_sec,
        state.config.rate_limit_burst,
    );

    // 注册时中继可能已进入关停流程（watch 只在值变化时通知，故先检查当前值）。
    if *shutdown_rx.borrow() {
        let _ = try_send_close(out_tx, CLOSE_GOING_AWAY, "server shutdown");
        return "server_shutdown";
    }

    loop {
        let next = tokio::select! {
            biased;
            changed = kick_rx.changed() => {
                let _ = changed;
                let _ = try_send_close(out_tx, CLOSE_SESSION_REPLACED, "session replaced");
                return "session_replaced";
            }
            changed = shutdown_rx.changed() => {
                let _ = changed;
                let _ = try_send_close(out_tx, CLOSE_GOING_AWAY, "server shutdown");
                return "server_shutdown";
            }
            next = next_frame(stream, idle) => next,
        };

        let message = match next {
            FrameEvent::End => return "client_closed",
            FrameEvent::Idle => {
                let _ = try_send_close(out_tx, CLOSE_IDLE_TIMEOUT, "idle timeout");
                return "idle_timeout";
            }
            FrameEvent::Message(Err(error)) => {
                debug!(device = %device_id, %error, "WebSocket 读取错误");
                return "socket_error";
            }
            FrameEvent::Message(Ok(message)) => message,
        };

        match message {
            Message::Text(text) => {
                if text.len() > state.config.max_frame_bytes {
                    let _ = try_send_error(out_tx, "payload_too_large", "单帧超过服务端上限");
                    let _ = try_send_close(out_tx, CLOSE_PROTOCOL_ERROR, "frame too large");
                    return "frame_too_large";
                }

                // 每连接令牌桶限速（背压优先：不足时暂停本连接读循环等待补令牌，
                // 通过 TCP 窗口自然减速发送端，不丢帧；等待超时才回 rate_limited）。
                if let Some(bucket) = bucket.as_mut() {
                    let max_wait = Duration::from_millis(state.config.rate_limit_max_wait_ms);
                    // 背压等待期间也要能被“踢旧连接 / 中继关停”立即打断。
                    let acquired = tokio::select! {
                        biased;
                        changed = kick_rx.changed() => {
                            let _ = changed;
                            let _ = try_send_close(out_tx, CLOSE_SESSION_REPLACED, "session replaced");
                            return "session_replaced";
                        }
                        changed = shutdown_rx.changed() => {
                            let _ = changed;
                            let _ = try_send_close(out_tx, CLOSE_GOING_AWAY, "server shutdown");
                            return "server_shutdown";
                        }
                        acquired = bucket.acquire(max_wait) => acquired,
                    };
                    if !acquired {
                        // 最后防线：背压等待超时，丢弃该帧并让发送端降速/重试。
                        if !try_send_error(
                            out_tx,
                            "rate_limited",
                            "发送速率超过限制且背压等待超时，请降低发送频率",
                        ) {
                            return "send_queue_full";
                        }
                        continue;
                    }
                }

                let message = match ClientToServer::from_json(&text) {
                    Ok(message) => message,
                    Err(error) => {
                        debug!(device = %device_id, %error, "客户端消息解析失败");
                        let _ =
                            try_send_error(out_tx, "protocol_error", "消息不是合法的 JSON 协议帧");
                        let _ = try_send_close(out_tx, CLOSE_PROTOCOL_ERROR, "invalid json");
                        return "invalid_json";
                    }
                };

                match handle_client_message(state, device, out_tx, message).await {
                    MsgFlow::Continue => {}
                    MsgFlow::Close(reason) => {
                        let _ = try_send_close(out_tx, CLOSE_PROTOCOL_ERROR, reason);
                        return reason;
                    }
                }
            }
            Message::Binary(_) => {
                let _ = try_send_error(out_tx, "protocol_error", "只支持文本 JSON 帧");
                let _ = try_send_close(out_tx, CLOSE_PROTOCOL_ERROR, "binary frame");
                return "binary_frame";
            }
            Message::Close(frame) => {
                debug!(
                    device = %device_id,
                    code = ?frame.map(|frame| frame.code),
                    "客户端主动关闭连接"
                );
                return "client_closed";
            }
            Message::Ping(_) | Message::Pong(_) => {
                // 收到任何帧都算活跃（下一轮循环重新计时空闲超时）。
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 业务消息分发
// ---------------------------------------------------------------------------

enum MsgFlow {
    Continue,
    Close(&'static str),
}

async fn handle_client_message(
    state: &Arc<RelayState>,
    device: &PublicIdentity,
    out_tx: &mpsc::Sender<OutMsg>,
    message: ClientToServer,
) -> MsgFlow {
    let device_id = device.device_id.as_str();
    match message {
        ClientToServer::Hello { .. } => {
            let _ = try_send_error(
                out_tx,
                "protocol_error",
                "连接已完成认证，不能重复发送 hello",
            );
            MsgFlow::Close("duplicate hello")
        }
        ClientToServer::Ping { ts_ms } => {
            try_send_json(out_tx, ServerToClient::Pong { ts_ms });
            MsgFlow::Continue
        }
        ClientToServer::PairAnnounce {
            code_hash,
            invite,
            ttl_secs,
        } => {
            pair::handle_announce(state, device_id, out_tx, code_hash, invite, ttl_secs).await;
            MsgFlow::Continue
        }
        ClientToServer::PairJoin { code_hash, join } => {
            pair::handle_join(state, device_id, out_tx, code_hash, join).await;
            MsgFlow::Continue
        }
        ClientToServer::SessionInit { to, msg } => {
            deliver(
                state,
                out_tx,
                device_id,
                &to,
                ServerToClient::SessionInit {
                    from: device_id.to_string(),
                    msg,
                },
            )
            .await;
            MsgFlow::Continue
        }
        ClientToServer::SessionAck { to, msg } => {
            deliver(
                state,
                out_tx,
                device_id,
                &to,
                ServerToClient::SessionAck {
                    from: device_id.to_string(),
                    msg,
                },
            )
            .await;
            MsgFlow::Continue
        }
        ClientToServer::Relay { to, envelope } => {
            // 中继不解析 envelope.ciphertext，只按 to 转发。
            deliver(
                state,
                out_tx,
                device_id,
                &to,
                ServerToClient::Relay {
                    from: device_id.to_string(),
                    envelope,
                },
            )
            .await;
            MsgFlow::Continue
        }
        ClientToServer::VaultPut {
            object_id,
            meta,
            ciphertext,
            ttl_secs,
        } => {
            handle_vault_put(
                state, device_id, out_tx, object_id, meta, ciphertext, ttl_secs,
            )
            .await;
            MsgFlow::Continue
        }
        ClientToServer::VaultGet { object_id } => {
            handle_vault_get(state, out_tx, object_id).await;
            MsgFlow::Continue
        }
        ClientToServer::VaultList => {
            handle_vault_list(state, device_id, out_tx).await;
            MsgFlow::Continue
        }
        ClientToServer::VaultDelete { object_id } => {
            handle_vault_delete(state, device_id, out_tx, object_id).await;
            MsgFlow::Continue
        }
        ClientToServer::RevocationPut { notice } => {
            handle_revocation_put(state, device, out_tx, notice).await;
            MsgFlow::Continue
        }
        ClientToServer::RevocationAck { notice_id } => {
            handle_revocation_ack(state, device, notice_id).await;
            MsgFlow::Continue
        }
    }
}

/// 校验目标并把消息投递给在线对端（不缓存离线帧）。
async fn deliver(
    state: &Arc<RelayState>,
    out_tx: &mpsc::Sender<OutMsg>,
    me: &str,
    to: &str,
    message: ServerToClient,
) {
    if to.is_empty() {
        let _ = try_send_error(out_tx, "protocol_error", "缺少目标设备 id");
        return;
    }
    if to == me {
        let _ = try_send_error(out_tx, "protocol_error", "不能转发给自己");
        return;
    }
    let Some(peer) = state.peer(to).await else {
        let _ = try_send_error(out_tx, "peer_offline", "目标设备不在线");
        return;
    };
    match peer.tx.try_send(OutMsg::Json(message)) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            let _ = try_send_error(out_tx, "rate_limited", "目标设备发送队列已满，请稍后重试");
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            // 目标连接已失效：顺手清理在线表。
            state
                .unregister_peer(&peer.device_id, &peer.session_id)
                .await;
            let _ = try_send_error(out_tx, "peer_offline", "目标设备不在线");
        }
    }
}

// ---------------------------------------------------------------------------
// 可选加密暂存
// ---------------------------------------------------------------------------

fn vault_put_failure(object_id: String, error: &crate::vault::VaultError) -> ServerToClient {
    ServerToClient::VaultPutResult {
        ok: false,
        object_id,
        expires_at_ms: 0,
        message: error.display_message(),
    }
}

async fn handle_vault_put(
    state: &Arc<RelayState>,
    device_id: &str,
    out_tx: &mpsc::Sender<OutMsg>,
    object_id: String,
    meta: VaultMeta,
    ciphertext: Vec<u8>,
    ttl_secs: u64,
) {
    let Some(vault) = &state.vault else {
        let _ = try_send_json(
            out_tx,
            ServerToClient::VaultPutResult {
                ok: false,
                object_id,
                expires_at_ms: 0,
                message: "vault_disabled: 服务端未启用加密暂存".to_string(),
            },
        );
        return;
    };
    let bytes = ciphertext.len();
    match vault
        .put(device_id, &object_id, meta, ciphertext, ttl_secs)
        .await
    {
        Ok(record) => {
            debug!(device = %device_id, bytes, "暂存对象已保存");
            let _ = try_send_json(
                out_tx,
                ServerToClient::VaultPutResult {
                    ok: true,
                    object_id,
                    expires_at_ms: record.expires_at_ms,
                    message: "ok".to_string(),
                },
            );
        }
        Err(error) => {
            debug!(device = %device_id, bytes, code = error.code, "暂存上传失败");
            let _ = try_send_json(out_tx, vault_put_failure(object_id, &error));
        }
    }
}

async fn handle_vault_get(
    state: &Arc<RelayState>,
    out_tx: &mpsc::Sender<OutMsg>,
    object_id: String,
) {
    let Some(vault) = &state.vault else {
        let _ = try_send_json(
            out_tx,
            ServerToClient::VaultGetResult {
                ok: false,
                object_id,
                meta: None,
                ciphertext: None,
                message: "vault_disabled: 服务端未启用加密暂存".to_string(),
            },
        );
        return;
    };
    match vault.get(&object_id).await {
        Ok(data) => {
            let _ = try_send_json(
                out_tx,
                ServerToClient::VaultGetResult {
                    ok: true,
                    object_id,
                    meta: Some(data.meta),
                    ciphertext: Some(data.ciphertext),
                    message: "ok".to_string(),
                },
            );
        }
        Err(error) => {
            let _ = try_send_json(
                out_tx,
                ServerToClient::VaultGetResult {
                    ok: false,
                    object_id,
                    meta: None,
                    ciphertext: None,
                    message: error.display_message(),
                },
            );
        }
    }
}

async fn handle_vault_list(
    state: &Arc<RelayState>,
    device_id: &str,
    out_tx: &mpsc::Sender<OutMsg>,
) {
    let Some(vault) = &state.vault else {
        let _ = try_send_json(
            out_tx,
            ServerToClient::VaultListResult {
                ok: false,
                objects: Vec::new(),
                message: "vault_disabled: 服务端未启用加密暂存".to_string(),
            },
        );
        return;
    };
    let objects = vault.list(device_id).await;
    let _ = try_send_json(
        out_tx,
        ServerToClient::VaultListResult {
            ok: true,
            objects,
            message: "ok".to_string(),
        },
    );
}

async fn handle_vault_delete(
    state: &Arc<RelayState>,
    device_id: &str,
    out_tx: &mpsc::Sender<OutMsg>,
    object_id: String,
) {
    let Some(vault) = &state.vault else {
        let _ = try_send_json(
            out_tx,
            ServerToClient::VaultDeleteResult {
                ok: false,
                object_id,
                message: "vault_disabled: 服务端未启用加密暂存".to_string(),
            },
        );
        return;
    };
    let result = vault.delete(device_id, &object_id).await;
    let message = match &result {
        Ok(()) => "ok".to_string(),
        Err(error) => error.display_message(),
    };
    let _ = try_send_json(
        out_tx,
        ServerToClient::VaultDeleteResult {
            ok: result.is_ok(),
            object_id,
            message,
        },
    );
}

// ---------------------------------------------------------------------------
// 离线解除配对通知信箱
// ---------------------------------------------------------------------------

/// `revocation_put`：验签 + 限额 + 落盘；目标在线时立即推送（仍保留到 ack/TTL）。
async fn handle_revocation_put(
    state: &Arc<RelayState>,
    device: &PublicIdentity,
    out_tx: &mpsc::Sender<OutMsg>,
    notice: RevocationNotice,
) {
    let notice_id = notice.notice_id().to_string();
    let id_prefix = revocation::short_id(&notice_id);
    match state.revocations.put(device, notice).await {
        Ok(outcome) => {
            let target = outcome.notice.content.to.clone();
            if outcome.duplicate {
                debug!(
                    device = %device.device_id,
                    notice_id = %id_prefix,
                    "收到重复的撤销通知（幂等，不重复入信箱）"
                );
            } else {
                debug!(
                    device = %device.device_id,
                    target = %target,
                    notice_id = %id_prefix,
                    "撤销通知已入离线信箱"
                );
            }
            // 目标在线 → 立即推送；推送失败不影响存储（下次登录会重推）。
            if let Some(peer) = state.peer(&target).await {
                let pushed = ServerToClient::PendingRevocations {
                    notices: vec![outcome.notice.clone()],
                };
                match peer.tx.try_send(OutMsg::Json(pushed)) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        warn!(
                            notice_id = %id_prefix,
                            "目标设备发送队列已满，撤销通知待其下次登录重推"
                        );
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        state
                            .unregister_peer(&peer.device_id, &peer.session_id)
                            .await;
                    }
                }
            }
            let message = if outcome.duplicate {
                "duplicate"
            } else {
                "queued"
            };
            let _ = try_send_json(
                out_tx,
                ServerToClient::RevocationPutResult {
                    ok: true,
                    notice_id,
                    message: message.to_string(),
                },
            );
        }
        Err(error) => {
            debug!(
                device = %device.device_id,
                notice_id = %id_prefix,
                code = error.code,
                "撤销通知被拒绝"
            );
            let _ = try_send_json(
                out_tx,
                ServerToClient::RevocationPutResult {
                    ok: false,
                    notice_id,
                    message: error.display_message(),
                },
            );
        }
    }
}

/// `revocation_ack`：只允许删除 `to == 自己` 的通知（无线上响应，重复 ack 幂等）。
async fn handle_revocation_ack(
    state: &Arc<RelayState>,
    device: &PublicIdentity,
    notice_id: String,
) {
    let id_prefix = revocation::short_id(&notice_id);
    match state.revocations.ack(&device.device_id, &notice_id).await {
        AckOutcome::Removed => {}
        AckOutcome::NotFound => {
            debug!(device = %device.device_id, notice_id = %id_prefix, "撤销通知确认：不存在或已删除（幂等）");
        }
        AckOutcome::NotOwner => {
            warn!(
                device = %device.device_id,
                notice_id = %id_prefix,
                "忽略删除他人撤销通知的确认请求"
            );
        }
    }
}
