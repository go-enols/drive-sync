//! WebSocket 客户端：连接中继、Hello 认证、消息收发、WS Ping 心跳。
//!
//! 只处理"一条连接"的生命周期；自动重连/退避在 [`crate::runtime`] 里。
//! wss:// 需要启用本 crate 的 `tls` feature（映射到 tokio-tungstenite 的
//! `rustls-tls-webpki-roots`）；未启用时连接 wss:// 会返回明确错误。

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use drive_core::identity::Identity;
use drive_core::util::random_hex;
use drive_core::wire::{hello_sign_input, ClientToServer, ServerToClient, WIRE_VERSION};
use drive_core::MAX_WIRE_FRAME_BYTES;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_with_config, MaybeTlsStream, WebSocketStream};

use crate::engine::ConnectionInfo;

/// 底层 WS 流类型。
pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 规范化中继地址：补 `ws://` 前缀、`http(s)` 转 `ws(s)`、补 `/ws` 路径。
pub fn normalize_relay_url(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut url = if trimmed.starts_with("ws://")
        || trimmed.starts_with("wss://")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
    {
        trimmed.to_string()
    } else {
        format!("ws://{trimmed}")
    };
    if let Some(rest) = url.strip_prefix("http://") {
        url = format!("ws://{rest}");
    } else if let Some(rest) = url.strip_prefix("https://") {
        url = format!("wss://{rest}");
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return url;
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let path = if path.is_empty() || path == "/" {
        "/ws"
    } else {
        path
    };
    format!("{scheme}://{authority}{path}")
}

fn tls_hint(url: &str) -> &'static str {
    if url.starts_with("wss://") {
        #[cfg(not(feature = "tls"))]
        {
            "（当前构建未启用 tls feature，无法连接 wss://；请用 `--features tls` 重新编译）"
        }
        #[cfg(feature = "tls")]
        {
            ""
        }
    } else {
        ""
    }
}

/// 反向代理常见错误提示：明文 `ws://` 打到只开 443 的域名会被 301/308 重定向。
fn redirect_hint(url: &str, error: &str) -> &'static str {
    let redirected = error.contains("301")
        || error.contains("302")
        || error.contains("307")
        || error.contains("308");
    if !redirected {
        return "";
    }
    if url.starts_with("ws://") {
        "（服务器要求 HTTPS：请把中继地址改成 wss:// 再试）"
    } else {
        "（反代重定向异常：请检查域名、端口与证书）"
    }
}

/// 一条已认证的中继连接。
pub struct Connection {
    ws: WsStream,
    welcome: ServerToClient,
    queued: Vec<ServerToClient>,
    device_id: String,
}

/// rustls 0.23 要求连接前显式安装 crypto provider。桌面构建通常由 reqwest 等
/// 依赖合并出 provider；移动端（Android/iOS）没有这些依赖，必须在首次 wss 连接前
/// 安装，否则 rustls 会 panic（Could not automatically determine ... CryptoProvider）。
#[cfg(feature = "tls")]
fn ensure_crypto_provider() {
    use std::sync::Once;
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

impl Connection {
    /// 连接并完成 Hello 认证；返回时 `welcome` 已到。认证阶段限时 15 秒。
    pub async fn connect(relay_url: &str, identity: &Identity, access_token: &str) -> Result<Self> {
        #[cfg(feature = "tls")]
        ensure_crypto_provider();
        let url = normalize_relay_url(relay_url);
        if url.is_empty() {
            bail!("中继地址为空（请用 --relay 指定，例如 ws://127.0.0.1:8787/ws）");
        }
        // 关闭 Nagle（TCP_NODELAY）：同步消息/传输 ack 都是小帧，开启 Nagle 时与对端
        // delayed-ACK 叠加会出现几十~几百毫秒的停顿，实测会显著拖慢文件传输吞吐。
        let connect = async {
            connect_async_with_config(url.as_str(), None, true)
                .await
                .map_err(|e| {
                    let text = e.to_string();
                    anyhow!(
                        "连接中继失败（{url}）: {e}{}{}",
                        tls_hint(&url),
                        redirect_hint(&url, &text)
                    )
                })
        };
        let (mut ws, _response) = tokio::time::timeout(Duration::from_secs(15), connect)
            .await
            .map_err(|_| anyhow!("连接中继超时（{url}）"))??;

        let device = identity.public();
        let nonce = random_hex(16);
        let signature = identity.sign(&hello_sign_input(&nonce, &device.device_id));
        let hello = ClientToServer::Hello {
            v: WIRE_VERSION,
            device,
            access_token: access_token.to_string(),
            nonce,
            signature,
        };
        ws.send(Message::Text(hello.to_json()?))
            .await
            .context("发送 hello 失败")?;

        let mut queued = Vec::new();
        let welcome = loop {
            match tokio::time::timeout(Duration::from_secs(15), ws.next()).await {
                Err(_) => bail!("等待中继 welcome 超时"),
                Ok(None) => bail!("中继在认证阶段关闭了连接"),
                Ok(Some(Err(e))) => bail!("中继连接错误（认证阶段）: {e}"),
                Ok(Some(Ok(Message::Text(text)))) => {
                    let message = ServerToClient::from_json(&text)
                        .context("解析中继 welcome 前的消息失败")?;
                    match message {
                        ServerToClient::Welcome { .. } => break message,
                        other => queued.push(other),
                    }
                }
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    let _ = ws.send(Message::Pong(payload)).await;
                }
                Ok(Some(Ok(Message::Close(frame)))) => bail!(
                    "中继在认证阶段关闭连接（code={:?}）",
                    frame.map(|f| u16::from(f.code))
                ),
                Ok(Some(Ok(_))) => {}
            }
        };
        Ok(Self {
            ws,
            welcome,
            queued,
            device_id: identity.device_id(),
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// 认证成功返回的 `welcome`（原样保留）。
    pub fn welcome(&self) -> &ServerToClient {
        &self.welcome
    }

    /// 提取 Engine 需要的连接信息。
    pub fn connection_info(&self) -> ConnectionInfo {
        match &self.welcome {
            ServerToClient::Welcome {
                device_id,
                session_id,
                server_time_ms,
                online,
                ..
            } => ConnectionInfo {
                device_id: device_id.clone(),
                session_id: session_id.clone(),
                online: online.clone(),
                server_time_ms: *server_time_ms,
            },
            _ => ConnectionInfo::default(),
        }
    }

    /// 取走 welcome 之前收到的消息（正常顺序下为空或 presence）。
    pub fn take_queued(&mut self) -> Vec<ServerToClient> {
        std::mem::take(&mut self.queued)
    }

    /// 发送一条客户端消息（JSON 文本帧）。
    pub async fn send(&mut self, message: &ClientToServer) -> Result<()> {
        let text = message.to_json().context("序列化消息失败")?;
        if text.len() > MAX_WIRE_FRAME_BYTES {
            bail!(
                "待发送帧 {} 字节超过上限 {} 字节",
                text.len(),
                MAX_WIRE_FRAME_BYTES
            );
        }
        self.ws
            .send(Message::Text(text))
            .await
            .context("WebSocket 发送失败")
    }

    /// 读取下一条服务器消息；自动回复 Ping。连接关闭返回 None。
    pub async fn next(&mut self) -> Option<Result<ServerToClient>> {
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    return Some(ServerToClient::from_json(&text).context("解析中继消息失败"));
                }
                Some(Ok(Message::Ping(payload))) => {
                    let _ = self.ws.send(Message::Pong(payload)).await;
                }
                Some(Ok(Message::Close(frame))) => {
                    return Some(Err(anyhow!(
                        "中继关闭连接（code={:?}）",
                        frame.map(|f| u16::from(f.code))
                    )));
                }
                Some(Err(e)) => return Some(Err(anyhow!("WebSocket 连接错误: {e}"))),
                // Binary/Frame/Pong：本协议不使用，忽略。
                Some(Ok(_)) => continue,
                None => return None,
            }
        }
    }

    /// 发送 WS Ping（心跳；中继 90s 未收到任何帧会断开）。
    pub async fn send_ping(&mut self) -> Result<()> {
        self.ws
            .send(Message::Ping(Vec::new()))
            .await
            .context("发送 WS Ping 失败")
    }

    /// 优雅关闭（最多等 1 秒）。
    pub async fn close(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(1), self.ws.close(None)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_relay_url_variants() {
        assert_eq!(
            normalize_relay_url("127.0.0.1:8787"),
            "ws://127.0.0.1:8787/ws"
        );
        assert_eq!(
            normalize_relay_url("ws://example.com:8787"),
            "ws://example.com:8787/ws"
        );
        assert_eq!(
            normalize_relay_url("ws://example.com:8787/"),
            "ws://example.com:8787/ws"
        );
        assert_eq!(
            normalize_relay_url("ws://example.com:8787/ws"),
            "ws://example.com:8787/ws"
        );
        assert_eq!(
            normalize_relay_url("https://example.com"),
            "wss://example.com/ws"
        );
        assert_eq!(normalize_relay_url(""), "");
        assert_eq!(
            normalize_relay_url("  ws://127.0.0.1:1/custom  "),
            "ws://127.0.0.1:1/custom"
        );
    }
}
