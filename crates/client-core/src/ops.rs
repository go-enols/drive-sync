//! 一次性连接辅助命令：配对（A 端发起 / B 端加入）、云端暂存 put/list/get、目录下载。
//!
//! 这些操作不需要常驻 daemon：各自建立一条中继连接、完成一次交互后关闭。
//! 配对助手支持非交互（`--yes` / e2e 自动确认 SAS），其余日志不打印内容明文。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use derive_core::crypto::seal;
use derive_core::identity::{Identity, PublicIdentity};
use derive_core::pairing::{decode_qr, PairInvite, PairingInitiator};
use derive_core::util::{blake3_hex, now_ms, random_hex};
use derive_core::vault::{effective_ttl_secs, VaultMeta, VaultObjectInfo, MAX_VAULT_TTL_SECS};
use derive_core::wire::{ClientToServer, ServerToClient};
use tokio::sync::mpsc;

use crate::engine::RuntimeEvent;
use crate::net::Connection;
use crate::pairing_joiner::PairingJoiner;
use crate::runtime::{Runtime, RuntimeConfig};
use crate::store::{Store, VaultKey};
use crate::util::write_text_atomic;

/// 配对等待超时（协议：配对码 TTL 120s）。
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(125);
/// 单对象暂存上限（原型：WS 单帧限制，见协议 §4.5）。
pub const MAX_VAULT_OBJECT_BYTES: u64 = 8 * 1024 * 1024;

/// 配对过程中的用户交互回调（CLI 打印/确认；e2e 自动确认）。
pub trait PairUi: Send + Sync {
    fn on_qr(&self, qr: &str) {
        let _ = qr;
    }
    fn on_code(&self, code: &str) {
        let _ = code;
    }
    fn on_peer(&self, peer: &PublicIdentity) {
        let _ = peer;
    }
    fn on_sas(&self, sas: &str) {
        let _ = sas;
    }
    /// 返回 true 表示用户确认 SAS 一致；默认自动确认（供 e2e/--yes）。
    fn confirm_sas(&self, sas: &str) -> bool {
        let _ = sas;
        true
    }
}

/// 非交互 UI（自动确认 SAS），同时捕获配对过程信息供 e2e 断言/自动化使用。
#[derive(Default)]
pub struct AutoPairUi {
    qr: std::sync::Mutex<Option<String>>,
    code: std::sync::Mutex<Option<String>>,
    peer: std::sync::Mutex<Option<PublicIdentity>>,
    sas: std::sync::Mutex<Option<String>>,
}

impl AutoPairUi {
    pub fn new() -> Self {
        Self::default()
    }

    /// 发起方生成的二维码串（`on_qr` 后可用）。
    pub fn qr(&self) -> Option<String> {
        self.qr.lock().ok().and_then(|guard| guard.clone())
    }

    /// 发起方生成的 8 位配对码（`on_code` 后可用）。
    pub fn code(&self) -> Option<String> {
        self.code.lock().ok().and_then(|guard| guard.clone())
    }

    /// 对端公开身份（收到 join/invite 后可用）。
    pub fn peer(&self) -> Option<PublicIdentity> {
        self.peer.lock().ok().and_then(|guard| guard.clone())
    }

    /// 计算出的 SAS（供 e2e 校验两端一致）。
    pub fn sas(&self) -> Option<String> {
        self.sas.lock().ok().and_then(|guard| guard.clone())
    }
}

impl PairUi for AutoPairUi {
    fn on_qr(&self, qr: &str) {
        if let Ok(mut guard) = self.qr.lock() {
            *guard = Some(qr.to_string());
        }
        tracing::debug!(qr_len = qr.len(), "配对二维码串已生成");
    }
    fn on_code(&self, code: &str) {
        if let Ok(mut guard) = self.code.lock() {
            *guard = Some(code.to_string());
        }
        tracing::info!(code, "配对码");
    }
    fn on_peer(&self, peer: &PublicIdentity) {
        if let Ok(mut guard) = self.peer.lock() {
            *guard = Some(peer.clone());
        }
        tracing::info!(peer = %peer.device_id, name = %peer.name, "配对对端");
    }
    fn on_sas(&self, sas: &str) {
        if let Ok(mut guard) = self.sas.lock() {
            *guard = Some(sas.to_string());
        }
        tracing::info!(sas, "SAS");
    }
}

/// 自动化配对两方（e2e 用）：并发启动 A 端发起与 B 端加入，双方自动确认 SAS。
///
/// 返回 `(发起方结果, 加入方结果)`；两端 `record.pairing_fingerprint` 必须相同。
pub async fn pair_pairwise(
    initiator: &RuntimeConfig,
    responder: &RuntimeConfig,
    timeout: Duration,
) -> Result<(PairOutcome, PairOutcome)> {
    let ui = std::sync::Arc::new(AutoPairUi::new());
    let initiator_config = initiator.clone();
    let initiator_ui = ui.clone();
    let initiator_task = tokio::spawn(async move {
        pair_initiate(&initiator_config, initiator_ui.as_ref()).await
    });

    // 等待发起方生成配对码。
    let ui_ref = ui.clone();
    let code = {
        let deadline = tokio::time::Instant::now() + timeout.min(Duration::from_secs(20));
        loop {
            if let Some(code) = ui_ref.code() {
                break code;
            }
            if initiator_task.is_finished() {
                let result = initiator_task.await;
                return match result {
                    Ok(Ok(outcome)) => Err(anyhow!(
                        "发起方在生成配对码前已结束（对端 {}）",
                        outcome.record.peer.device_id
                    )),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(anyhow!("发起方配对任务异常: {error}")),
                };
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("等待发起方生成配对码超时");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };

    let responder_outcome = pair_respond(responder, None, Some(&code), ui.as_ref()).await?;
    let initiator_outcome = tokio::time::timeout(timeout, initiator_task)
        .await
        .map_err(|_| anyhow!("等待发起方完成配对超时"))?
        .map_err(|error| anyhow!("发起方配对任务异常: {error}"))??;
    Ok((initiator_outcome, responder_outcome))
}

/// 配对结果。
#[derive(Clone, Debug)]
pub struct PairOutcome {
    pub record: derive_core::pairing::PairRecord,
    pub sas: String,
}

fn resolve_relay(config: &RuntimeConfig, store: &Store) -> Result<String> {
    if !config.relay_url.trim().is_empty() {
        return Ok(config.relay_url.clone());
    }
    let stored = store.load_config()?;
    if !stored.relay_url.trim().is_empty() {
        return Ok(stored.relay_url);
    }
    bail!("未配置中继地址（请用 --relay 指定，或在 config.json 中配置 relay_url）")
}

fn resolve_token(config: &RuntimeConfig, store: &Store) -> Result<String> {
    if !config.access_token.is_empty() {
        return Ok(config.access_token.clone());
    }
    Ok(store.load_config()?.access_token)
}

async fn connect(config: &RuntimeConfig, store: &Store, identity: &Identity) -> Result<Connection> {
    let relay = resolve_relay(config, store)?;
    let token = resolve_token(config, store)?;
    Ok(Connection::connect(&relay, identity, &token).await?)
}

/// A 端发起配对：打印二维码串/配对码，等待 B 加入，双方确认 SAS 后保存 PairRecord。
pub async fn pair_initiate(config: &RuntimeConfig, ui: &dyn PairUi) -> Result<PairOutcome> {
    let store = Store::open(&config.data_dir)?;
    let identity = crate::runtime::load_identity(&store, config.device_name.as_deref())?;
    let relay = resolve_relay(&config, &store)?;
    let mut conn = connect(config, &store, &identity).await?;

    let mut initiator = PairingInitiator::start(&identity, &relay);
    let qr = initiator.qr_string().context("生成配对二维码失败")?;
    ui.on_qr(&qr);
    ui.on_code(&initiator.code);
    conn.send(&ClientToServer::PairAnnounce {
        code_hash: initiator.code_hash(),
        invite: initiator.invite.clone(),
        ttl_secs: 120,
    })
    .await?;

    let deadline = tokio::time::Instant::now() + PAIR_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("等待对端加入配对超时（120 秒），请重新生成配对码");
        }
        let message = match tokio::time::timeout(remaining, conn.next()).await {
            Err(_) => bail!("等待对端加入配对超时（120 秒），请重新生成配对码"),
            Ok(None) => bail!("中继连接已关闭"),
            Ok(Some(Err(error))) => return Err(error),
            Ok(Some(Ok(message))) => message,
        };
        match message {
            ServerToClient::PairEvent {
                from,
                join: Some(join),
                ..
            } => {
                if join.device.device_id != from {
                    bail!("配对事件中的设备 id 不一致");
                }
                initiator
                    .handle_join(&join)
                    .map_err(|error| anyhow!("处理对端加入信息失败: {error}"))?;
                let sas = initiator
                    .sas()
                    .ok_or_else(|| anyhow!("未能派生配对 SAS"))?;
                ui.on_peer(&join.device);
                ui.on_sas(&sas);
                if !ui.confirm_sas(&sas) {
                    bail!("SAS 未确认，配对已取消");
                }
                let record = initiator
                    .finish()
                    .map_err(|error| anyhow!("生成配对记录失败: {error}"))?;
                store.upsert_peer(record.clone())?;
                conn.close().await;
                return Ok(PairOutcome { record, sas });
            }
            ServerToClient::Error { code, message } => {
                if matches!(
                    code.as_str(),
                    "auth_failed" | "bad_version" | "protocol_error"
                ) {
                    bail!("中继拒绝配对请求 [{code}]: {message}");
                }
                tracing::warn!(code = %code, message = %message, "配对期间中继返回错误");
            }
            ServerToClient::PairResult { ok: false, .. } => {
                tracing::debug!("配对路由尚未完成（继续等待 join）");
            }
            _ => {}
        }
    }
}

/// B 端加入配对：`qr`（扫码串）或 `code`（8 位数字）二选一。
///
/// 扫码串本身带外传递了配对码，仍会打印 SAS 供核对（`--yes` 可跳过人工确认）。
pub async fn pair_respond(
    config: &RuntimeConfig,
    qr: Option<&str>,
    code: Option<&str>,
    ui: &dyn PairUi,
) -> Result<PairOutcome> {
    let store = Store::open(&config.data_dir)?;
    let identity = crate::runtime::load_identity(&store, config.device_name.as_deref())?;
    let (known_invite, pair_code): (Option<PairInvite>, String) = match (qr, code) {
        (Some(qr_string), _) => {
            let (invite, code) = decode_qr(qr_string)
                .map_err(|error| anyhow!("解析配对二维码失败: {error}"))?;
            (Some(invite), code)
        }
        (None, Some(code)) => (None, code.to_string()),
        (None, None) => bail!("加入配对需要提供 --qr <扫码串> 或 --code <8 位配对码>"),
    };
    let mut joiner = PairingJoiner::start(&identity, &pair_code)?;
    if let Some(invite) = &known_invite {
        joiner.accept_invite(invite)?;
        ui.on_peer(&invite.device);
    }
    let mut conn = connect(config, &store, &identity).await?;
    conn.send(&ClientToServer::PairJoin {
        code_hash: joiner.code_hash(),
        join: joiner.join.clone(),
    })
    .await?;

    let deadline = tokio::time::Instant::now() + PAIR_TIMEOUT;
    let mut have_invite = known_invite.is_some();
    let mut routed = false;
    loop {
        if have_invite && routed {
            break;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("等待中继转发配对信息超时");
        }
        let message = match tokio::time::timeout(remaining, conn.next()).await {
            Err(_) => bail!("等待中继转发配对信息超时"),
            Ok(None) => bail!("中继连接已关闭（配对码可能无效或已过期）"),
            Ok(Some(Err(error))) => return Err(error),
            Ok(Some(Ok(message))) => message,
        };
        match message {
            ServerToClient::PairEvent {
                from,
                invite: Some(invite),
                ..
            } => {
                if from != invite.device.device_id {
                    bail!("配对事件中的设备 id 不一致");
                }
                if known_invite.is_none() {
                    joiner.accept_invite(&invite)?;
                    ui.on_peer(&invite.device);
                    have_invite = true;
                } else if let Some(known) = &known_invite {
                    if known.device.device_id != invite.device.device_id
                        || known.ephemeral_pub != invite.ephemeral_pub
                    {
                        bail!("中继返回的邀请与二维码不一致（可能的中间人攻击）");
                    }
                }
                routed = true;
            }
            ServerToClient::PairResult { ok, message, .. } => {
                if ok {
                    routed = true;
                } else {
                    bail!("配对失败: {message}");
                }
            }
            ServerToClient::Error { code, message } => {
                if matches!(
                    code.as_str(),
                    "auth_failed" | "bad_version" | "protocol_error" | "pair_not_found"
                ) {
                    bail!("配对失败 [{code}]: {message}");
                }
                tracing::warn!(code = %code, message = %message, "配对期间中继返回错误");
            }
            _ => {}
        }
    }

    let sas = joiner
        .sas()
        .ok_or_else(|| anyhow!("未能派生配对 SAS"))?;
    ui.on_sas(&sas);
    if !ui.confirm_sas(&sas) {
        bail!("SAS 未确认，配对已取消");
    }
    let record = joiner
        .finish()
        .map_err(|error| anyhow!("生成配对记录失败: {error}"))?;
    store.upsert_peer(record.clone())?;
    conn.close().await;
    Ok(PairOutcome { record, sas })
}

/// 等待指定类型的服务器响应（配对/vault 一次性连接用）。
async fn wait_response<F>(conn: &mut Connection, timeout: Duration, predicate: F) -> Result<ServerToClient>
where
    F: Fn(&ServerToClient) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("等待中继响应超时");
        }
        let message = match tokio::time::timeout(remaining, conn.next()).await {
            Err(_) => bail!("等待中继响应超时"),
            Ok(None) => bail!("中继连接已关闭"),
            Ok(Some(Err(error))) => return Err(error),
            Ok(Some(Ok(message))) => message,
        };
        if predicate(&message) {
            return Ok(message);
        }
    }
}

// ---------------------------------------------------------------------------
// 云端暂存（vault）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct VaultPutOutcome {
    pub object_id: String,
    pub expires_at_ms: u64,
    pub ttl_secs: u64,
}

/// 上传一个加密对象：客户端随机密钥 + XChaCha20-Poly1305，密钥只存本地 `vault_keys.json`。
pub async fn vault_put(
    config: &RuntimeConfig,
    path: &Path,
    ttl_days: u64,
) -> Result<VaultPutOutcome> {
    let store = Store::open(&config.data_dir)?;
    let identity = crate::runtime::load_identity(&store, config.device_name.as_deref())?;
    let data = std::fs::read(path).with_context(|| format!("读取文件失败: {}", path.display()))?;
    if data.len() as u64 > MAX_VAULT_OBJECT_BYTES {
        bail!(
            "文件 {} 字节超过单对象暂存上限 {} 字节",
            data.len(),
            MAX_VAULT_OBJECT_BYTES
        );
    }
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "object.bin".to_string());
    let object_id = format!("obj-{}", random_hex(8));
    let content_hash = blake3_hex(&data);
    let meta = VaultMeta {
        name: file_name,
        kind: "file".to_string(),
        size: data.len() as u64,
        content_hash,
        entry_id: None,
        note: String::new(),
    };
    let key = derive_core::crypto::random_key();
    let ciphertext = seal(&key, object_id.as_bytes(), &data)
        .map_err(|error| anyhow!("加密失败: {error}"))?;
    let requested_ttl = ttl_days.saturating_mul(24 * 60 * 60);
    let ttl_secs = effective_ttl_secs(requested_ttl);

    let mut conn = connect(config, &store, &identity).await?;
    conn.send(&ClientToServer::VaultPut {
        object_id: object_id.clone(),
        meta: meta.clone(),
        ciphertext,
        ttl_secs,
    })
    .await?;
    let response = wait_response(&mut conn, Duration::from_secs(60), |message| {
        matches!(message, ServerToClient::VaultPutResult { .. })
    })
    .await?;
    conn.close().await;
    match response {
        ServerToClient::VaultPutResult {
            ok,
            object_id,
            expires_at_ms,
            message,
        } => {
            if !ok {
                bail!("暂存上传失败: {message}");
            }
            store.upsert_vault_key(VaultKey {
                object_id: object_id.clone(),
                key_hex: hex::encode(key),
                meta,
                created_at_ms: now_ms(),
                ttl_secs,
                shared_with: Vec::new(),
            })?;
            Ok(VaultPutOutcome {
                object_id,
                expires_at_ms,
                ttl_secs,
            })
        }
        _ => bail!("中继返回了意外的响应"),
    }
}

/// 列出云端暂存对象（服务端只持有密文与元数据）。
pub async fn vault_list(config: &RuntimeConfig) -> Result<Vec<VaultObjectInfo>> {
    let store = Store::open(&config.data_dir)?;
    let identity = crate::runtime::load_identity(&store, config.device_name.as_deref())?;
    let mut conn = connect(config, &store, &identity).await?;
    conn.send(&ClientToServer::VaultList).await?;
    let response = wait_response(&mut conn, Duration::from_secs(60), |message| {
        matches!(message, ServerToClient::VaultListResult { .. })
    })
    .await?;
    conn.close().await;
    match response {
        ServerToClient::VaultListResult {
            ok,
            objects,
            message,
        } => {
            if !ok {
                bail!("列出暂存对象失败: {message}");
            }
            Ok(objects)
        }
        _ => bail!("中继返回了意外的响应"),
    }
}

/// 下载并解密云端口暂存对象，校验内容哈希后写盘。
pub async fn vault_get(
    config: &RuntimeConfig,
    object_id: &str,
    out_path: &Path,
) -> Result<PathBuf> {
    let store = Store::open(&config.data_dir)?;
    let identity = crate::runtime::load_identity(&store, config.device_name.as_deref())?;
    let key_record = store
        .find_vault_key(object_id)?
        .ok_or_else(|| anyhow!("本地没有对象 {object_id} 的解密密钥（vault_keys.json）"))?;
    let key_bytes = hex::decode(&key_record.key_hex)
        .map_err(|error| anyhow!("本地保存的 vault 密钥非法: {error}"))?;
    let key: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| anyhow!("本地保存的 vault 密钥长度应为 32 字节"))?;

    let mut conn = connect(config, &store, &identity).await?;
    conn.send(&ClientToServer::VaultGet {
        object_id: object_id.to_string(),
    })
    .await?;
    let response = wait_response(&mut conn, Duration::from_secs(60), |message| {
        matches!(message, ServerToClient::VaultGetResult { .. })
    })
    .await?;
    conn.close().await;
    match response {
        ServerToClient::VaultGetResult {
            ok,
            object_id: response_id,
            meta,
            ciphertext,
            message,
        } => {
            if !ok {
                bail!("下载暂存对象失败: {message}");
            }
            if response_id != object_id {
                bail!("中继返回的对象 id 不匹配");
            }
            let meta = meta.ok_or_else(|| anyhow!("中继响应缺少对象元数据"))?;
            let ciphertext = ciphertext.ok_or_else(|| anyhow!("中继响应缺少密文"))?;
            let plaintext = derive_core::crypto::open(&key, object_id.as_bytes(), &ciphertext)
                .map_err(|error| anyhow!("解密失败（密钥不匹配或数据损坏）: {error}"))?;
            if plaintext.len() as u64 != meta.size {
                bail!("解密后大小与元数据不一致");
            }
            if blake3_hex(&plaintext) != meta.content_hash {
                bail!("解密后内容 BLAKE3 校验失败");
            }
            if let Some(parent) = out_path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("创建输出目录失败: {}", parent.display()))?;
                }
            }
            write_text_atomic(out_path, &plaintext)
                .with_context(|| format!("写入输出文件失败: {}", out_path.display()))?;
            Ok(out_path.to_path_buf())
        }
        _ => bail!("中继返回了意外的响应"),
    }
}

// ---------------------------------------------------------------------------
// 目录下载（catalog fetch）：一次性连接，等待会话建立 + have_list，发起 fetch 并接收
// ---------------------------------------------------------------------------

/// 从在线已配对设备下载某个软件本体（一次性连接）。
///
/// 接收到的文件先落在 `<data_dir>/incoming/`；若提供 `out_dir` 则移动到该目录。
pub async fn catalog_fetch(
    config: &RuntimeConfig,
    entry_id: &str,
    out_dir: Option<PathBuf>,
    timeout: Duration,
) -> Result<PathBuf> {
    let mut cfg = config.clone();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    cfg.event_tx = Some(event_tx);
    cfg.remote_clipboard_tx = None;
    let (runtime, handle) = Runtime::open(cfg)?;
    let mut run_task = tokio::spawn(async move { runtime.run().await });
    let deadline = tokio::time::Instant::now() + timeout;
    let mut requested = false;
    let mut received_path: Option<PathBuf> = None;
    let mut failure: Option<String> = None;
    let mut saw_session = false;
    let mut first_session_at: Option<tokio::time::Instant> = None;
    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let event = tokio::select! {
            biased;
            _ = &mut run_task => {
                failure = Some("同步运行时提前退出".to_string());
                break;
            }
            _ = ticker.tick() => {
                // 会话已建立但一直没有设备报告持有该条目 → 尽早失败，避免死等超时。
                if let Some(started) = first_session_at {
                    if !requested && started.elapsed() > Duration::from_secs(8) {
                        failure = Some(format!(
                            "已与对端建立会话，但没有设备持有条目 {entry_id}（have_list 未出现）"
                        ));
                        break;
                    }
                }
                continue;
            }
            result = tokio::time::timeout_at(deadline, event_rx.recv()) => {
                match result {
                    Err(_) => {
                        failure = Some(format!("等待对端响应超时（{} 秒）", timeout.as_secs()));
                        break;
                    }
                    Ok(None) => {
                        failure = Some("运行期事件通道已关闭".to_string());
                        break;
                    }
                    Ok(Some(event)) => event,
                }
            }
        };
        match event {
            RuntimeEvent::SessionEstablished { peer_id, name } => {
                saw_session = true;
                first_session_at.get_or_insert_with(tokio::time::Instant::now);
                tracing::info!(peer = %peer_id, name = %name, "会话已建立，等待对端可用性列表");
            }
            RuntimeEvent::PeerHaveList {
                peer_id,
                entry_ids,
            } if !requested && entry_ids.iter().any(|id| id == entry_id) => {
                tracing::info!(peer = %peer_id, "对端持有目标文件，开始请求传输");
                if let Err(error) = handle.request_fetch(&peer_id, entry_id).await {
                    failure = Some(error.to_string());
                    break;
                }
                requested = true;
            }
            RuntimeEvent::TransferReceived {
                entry_id: Some(received_entry),
                path,
                size,
                ..
            } if received_entry == entry_id => {
                tracing::info!(size, "文件接收完成");
                received_path = Some(path);
                break;
            }
            RuntimeEvent::TransferFailed { message, .. } => {
                failure = Some(message);
                break;
            }
            _ => {}
        }
    }

    handle.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(3), &mut run_task).await;

    if let Some(message) = failure {
        bail!("{message}");
    }
    let path = received_path.ok_or_else(|| {
        if saw_session {
            anyhow!("没有设备提供该条目（have_list 中未出现）")
        } else {
            anyhow!("未能与任何已配对设备建立会话，请确认对端在线")
        }
    })?;
    match out_dir {
        Some(out_dir) => {
            std::fs::create_dir_all(&out_dir)
                .with_context(|| format!("创建输出目录失败: {}", out_dir.display()))?;
            let file_name = path
                .file_name()
                .map(|s| s.to_owned())
                .unwrap_or_else(|| std::ffi::OsString::from("received.bin"));
            let destination = out_dir.join(file_name);
            move_file(&path, &destination)?;
            Ok(destination)
        }
        None => Ok(path),
    }
}

fn move_file(source: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        std::fs::remove_file(destination).ok();
    }
    match std::fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(source, destination).with_context(|| {
                format!(
                    "移动文件失败: {} -> {}",
                    source.display(),
                    destination.display()
                )
            })?;
            std::fs::remove_file(source).ok();
            Ok(())
        }
    }
}

/// 计算 TTL 天数（供 CLI 展示/校验）。
pub fn ttl_days_to_secs(days: u64) -> u64 {
    effective_ttl_secs(days.saturating_mul(24 * 60 * 60)).min(MAX_VAULT_TTL_SECS)
}
