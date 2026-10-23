//! 中继可选加密暂存（vault）的端到端语义测试。
//!
//! 全部通过**真实 WebSocket 客户端 → 进程内中继**验证协议行为，不直接调用中继内部 API。
//!
//! 关于 TTL 的说明（实现与任务的差异，已记录在 docs/benchmark.md / 汇报中）：
//! 协议 §4.5 / §7.4 规定服务端把 `ttl_secs` 强制收敛到 **60s ~ 30d**，因此**无法**通过
//! 协议在 1~2 秒内构造"自然过期"。本测试用两种方式覆盖到期语义：
//! 1. `ttl=1` 上传 → 验证服务端把它 clamp 到 60s，且对象可读、落盘只有密文；
//! 2. 停机后直接修改 `<vault_dir>/index.json` 的 `expires_at_ms`（白盒构造"已到期"），
//!    重启中继验证**启动清理**删除对象文件 + 查询返回 not found；
//! 3. 把到期时间设为 2.5s 后、以 1s 清理间隔重启，验证**后台清理任务**删除文件，
//!    并轮询等待（覆盖任务要求的"sleep 2s 后过期/not found"语义）。
//!
//! 为什么可以改 index.json：TTL 下限 60s 是为了防误删的生产约束，e2e 测试不能等 60s；
//! 启动清理/后台清理是真实产品路径，修改索引只是把时间拨快，未绕过任何逻辑。

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use drive_core::identity::Identity;
use drive_core::util::{blake3_hex, now_ms};
use drive_core::vault::{VaultMeta, MIN_VAULT_TTL_SECS};
use drive_core::wire::{ClientToServer, ServerToClient};
use drive_e2e::harness::{relay_config, start_relay, RelayClient, DEFAULT_TIMEOUT, DEFAULT_TOKEN};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn vault_config(dir: &Path) -> drive_relay::RelayConfig {
    // 每设备/全局限额与清理间隔沿用默认值（暂存目录仍会做启动清理）。
    relay_config(Some(dir.to_path_buf()))
}

fn reaping_vault_config(dir: &Path) -> drive_relay::RelayConfig {
    let mut config = vault_config(dir);
    // 背景清理间隔 1s（协议默认 60s），便于在秒级测试中观察"到期自动删除"。
    config.vault_reap_interval_secs = 1;
    config
}

async fn connect(url: &str, name: &str) -> Result<RelayClient> {
    RelayClient::connect(url, Identity::generate(name), DEFAULT_TOKEN).await
}

fn meta_for(object_id: &str, plaintext_len: usize) -> VaultMeta {
    VaultMeta {
        name: format!("{object_id}.bin"),
        kind: "archive".to_string(),
        size: plaintext_len as u64,
        content_hash: blake3_hex(format!("plaintext-of-{object_id}").as_bytes()),
        entry_id: None,
        note: String::new(),
    }
}

/// 模拟客户端已做 E2E 加密：中继看到的只是不透明密文。
fn ciphertext_for(object_id: &str) -> Vec<u8> {
    let mut bytes = drive_core::util::random_bytes(256);
    bytes.extend_from_slice(object_id.as_bytes());
    bytes
}

struct PutResult {
    ok: bool,
    object_id: String,
    expires_at_ms: u64,
    message: String,
}

async fn put(
    client: &mut RelayClient,
    object_id: &str,
    ciphertext: &[u8],
    ttl_secs: u64,
) -> Result<PutResult> {
    client
        .send(&ClientToServer::VaultPut {
            object_id: object_id.to_string(),
            meta: meta_for(object_id, 1024),
            ciphertext: ciphertext.to_vec(),
            ttl_secs,
        })
        .await?;
    loop {
        match client.recv(DEFAULT_TIMEOUT).await? {
            ServerToClient::VaultPutResult {
                ok,
                object_id,
                expires_at_ms,
                message,
            } => {
                return Ok(PutResult {
                    ok,
                    object_id,
                    expires_at_ms,
                    message,
                })
            }
            ServerToClient::Presence { .. }
            | ServerToClient::Welcome { .. }
            | ServerToClient::Pong { .. } => continue,
            other => bail!("期待 vault_put_result，收到: {other:?}"),
        }
    }
}

struct GetResult {
    ok: bool,
    object_id: String,
    meta: Option<VaultMeta>,
    ciphertext: Option<Vec<u8>>,
    message: String,
}

async fn get(client: &mut RelayClient, object_id: &str) -> Result<GetResult> {
    client
        .send(&ClientToServer::VaultGet {
            object_id: object_id.to_string(),
        })
        .await?;
    loop {
        match client.recv(DEFAULT_TIMEOUT).await? {
            ServerToClient::VaultGetResult {
                ok,
                object_id,
                meta,
                ciphertext,
                message,
            } => {
                return Ok(GetResult {
                    ok,
                    object_id,
                    meta,
                    ciphertext,
                    message,
                })
            }
            ServerToClient::Presence { .. }
            | ServerToClient::Welcome { .. }
            | ServerToClient::Pong { .. } => continue,
            other => bail!("期待 vault_get_result，收到: {other:?}"),
        }
    }
}

struct ListResult {
    ok: bool,
    objects: Vec<drive_core::vault::VaultObjectInfo>,
    message: String,
}

async fn list(client: &mut RelayClient) -> Result<ListResult> {
    client.send(&ClientToServer::VaultList).await?;
    loop {
        match client.recv(DEFAULT_TIMEOUT).await? {
            ServerToClient::VaultListResult {
                ok,
                objects,
                message,
            } => {
                return Ok(ListResult {
                    ok,
                    objects,
                    message,
                })
            }
            ServerToClient::Presence { .. }
            | ServerToClient::Welcome { .. }
            | ServerToClient::Pong { .. } => continue,
            other => bail!("期待 vault_list_result，收到: {other:?}"),
        }
    }
}

struct DeleteResult {
    ok: bool,
    message: String,
}

async fn delete(client: &mut RelayClient, object_id: &str) -> Result<DeleteResult> {
    client
        .send(&ClientToServer::VaultDelete {
            object_id: object_id.to_string(),
        })
        .await?;
    loop {
        match client.recv(DEFAULT_TIMEOUT).await? {
            ServerToClient::VaultDeleteResult {
                ok,
                object_id: _,
                message,
            } => return Ok(DeleteResult { ok, message }),
            ServerToClient::Presence { .. }
            | ServerToClient::Welcome { .. }
            | ServerToClient::Pong { .. } => continue,
            other => bail!("期待 vault_delete_result，收到: {other:?}"),
        }
    }
}

fn object_file(dir: &Path, object_id: &str) -> std::path::PathBuf {
    dir.join(format!("{object_id}.bin"))
}

fn index_file(dir: &Path) -> std::path::PathBuf {
    dir.join("index.json")
}

/// 白盒构造"已到期"：把索引中的 `expires_at_ms` 改成过去/未来时间。
///
/// 仅在**中继停机**时调用；重启后中继会按真实清理路径处理该对象。
fn patch_expiry(dir: &Path, object_id: &str, expires_at_ms: u64) -> Result<()> {
    let path = index_file(dir);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("读取暂存索引失败: {}", path.display()))?;
    let mut value: serde_json::Value = serde_json::from_str(&raw).context("解析暂存索引失败")?;
    let objects = value
        .get_mut("objects")
        .and_then(|objects| objects.as_object_mut())
        .context("暂存索引缺少 objects 字段")?;
    let record = objects
        .get_mut(object_id)
        .with_context(|| format!("暂存索引中没有对象 {object_id}"))?;
    record["expires_at_ms"] = serde_json::json!(expires_at_ms);
    std::fs::write(&path, serde_json::to_vec_pretty(&value)?)
        .with_context(|| format!("写回暂存索引失败: {}", path.display()))?;
    Ok(())
}

fn index_object_ids(dir: &Path) -> Result<Vec<String>> {
    let raw = std::fs::read_to_string(index_file(dir)).context("读取暂存索引失败")?;
    let value: serde_json::Value = serde_json::from_str(&raw).context("解析暂存索引失败")?;
    let mut ids: Vec<String> = value["objects"]
        .as_object()
        .map(|objects| objects.keys().cloned().collect())
        .unwrap_or_default();
    ids.sort();
    Ok(ids)
}

// ---------------------------------------------------------------------------
// 测试 1：ttl=1 上传（服务端 clamp 到 60s）→ 可读 → 删除 → 文件消失
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vault_put_get_delete_and_ttl_is_clamped() -> Result<()> {
    let dir: TempDir = tempfile::tempdir().context("创建临时暂存目录失败")?;
    let relay = start_relay(vault_config(dir.path())).await?;
    let url = relay.ws_url();
    let mut client = connect(&url, "vault-client").await?;

    let object_id = "obj-ttl-1";
    let ciphertext = ciphertext_for(object_id);
    let before = now_ms();
    let put_result = put(&mut client, object_id, &ciphertext, 1).await?;
    assert!(put_result.ok, "上传应成功: {}", put_result.message);
    assert_eq!(put_result.object_id, object_id);

    // 协议强制 TTL 下限 60s：请求 1s 必须被 clamp（无法在秒级测试里自然过期）。
    let min_expiry = before + MIN_VAULT_TTL_SECS * 1000;
    assert!(
        put_result.expires_at_ms >= min_expiry,
        "TTL 应在服务端被 clamp 到 >= {MIN_VAULT_TTL_SECS}s（expires={}，期望>= {min_expiry}）",
        put_result.expires_at_ms
    );

    // 立即可读，且密文逐字节一致。
    let get_result = get(&mut client, object_id).await?;
    assert!(get_result.ok, "上传后应能立即读取: {}", get_result.message);
    assert_eq!(get_result.object_id, object_id);
    assert_eq!(
        get_result.ciphertext.as_deref(),
        Some(ciphertext.as_slice()),
        "取回的密文应与上传时一致"
    );
    assert_eq!(
        get_result.meta.as_ref().map(|meta| meta.name.as_str()),
        Some(format!("{object_id}.bin").as_str())
    );

    // 落盘检查：对象文件 + 索引存在，且磁盘上只有密文（无明文落盘）。
    let file = object_file(dir.path(), object_id);
    assert!(file.exists(), "对象文件应存在: {}", file.display());
    assert!(
        index_file(dir.path()).exists(),
        "index.json 应存在: {}",
        index_file(dir.path()).display()
    );
    let on_disk = std::fs::read(&file)?;
    assert_eq!(on_disk, ciphertext, "磁盘上的对象文件应等于客户端密文");
    assert!(
        !on_disk
            .windows(b"plaintext-of-".len())
            .any(|window| window == b"plaintext-of-"),
        "中继磁盘上不得出现明文内容"
    );

    let listed = list(&mut client).await?;
    assert!(listed.ok, "list 应成功: {}", listed.message);
    assert_eq!(listed.objects.len(), 1);
    assert_eq!(listed.objects[0].object_id, object_id);
    assert_eq!(listed.objects[0].ciphertext_len, ciphertext.len() as u64);

    // 显式删除 → 立即不可读且文件被删除。
    let delete_result = delete(&mut client, object_id).await?;
    assert!(delete_result.ok, "删除应成功: {}", delete_result.message);
    assert!(
        !object_file(dir.path(), object_id).exists(),
        "删除后文件应消失"
    );
    let get_after_delete = get(&mut client, object_id).await?;
    assert!(!get_after_delete.ok, "删除后读取必须失败");
    assert!(
        get_after_delete.message.contains("vault_not_found"),
        "删除后读取应返回 vault_not_found，实际: {}",
        get_after_delete.message
    );
    let listed = list(&mut client).await?;
    assert!(listed.objects.is_empty(), "删除后列表应为空");

    client.close().await;
    relay.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// 测试 2：到期对象在重启时被启动清理删除；后台 reaper 在到期后删除文件
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vault_expired_objects_are_deleted_on_restart_and_by_reaper() -> Result<()> {
    let dir: TempDir = tempfile::tempdir().context("创建临时暂存目录失败")?;

    // 第一阶段：正常上传两个对象（TTL 下限 60s，只能白盒改索引模拟到期）。
    let expired_id = "obj-expired";
    let reaped_id = "obj-reaped";
    {
        let relay = start_relay(vault_config(dir.path())).await?;
        let mut client = connect(&relay.ws_url(), "vault-client").await?;
        for object_id in [expired_id, reaped_id] {
            let result = put(&mut client, object_id, &ciphertext_for(object_id), 60).await?;
            assert!(result.ok, "上传 {object_id} 失败: {}", result.message);
        }
        assert!(object_file(dir.path(), expired_id).exists());
        assert!(object_file(dir.path(), reaped_id).exists());
        client.close().await;
        relay.shutdown().await;
    }

    // 停机后修改索引：expired 已经过期；reaped 2.5s 后过期（交给后台清理）。
    patch_expiry(dir.path(), expired_id, now_ms().saturating_sub(1_000))?;
    patch_expiry(dir.path(), reaped_id, now_ms() + 2_500)?;

    // 第二阶段：以 1s 清理间隔重启；启动清理应删除 expired 对象。
    let relay = start_relay(reaping_vault_config(dir.path())).await?;
    let mut client = connect(&relay.ws_url(), "vault-client").await?;

    assert!(
        !object_file(dir.path(), expired_id).exists(),
        "启动清理应删除已过期对象文件"
    );
    let ids = index_object_ids(dir.path())?;
    assert!(
        !ids.contains(&expired_id.to_string()),
        "启动清理后索引不应再包含过期对象，当前: {ids:?}"
    );
    let get_expired = get(&mut client, expired_id).await?;
    assert!(!get_expired.ok, "已过期对象必须不可读");
    assert!(
        get_expired.message.contains("vault_not_found")
            || get_expired.message.contains("vault_expired"),
        "过期对象读取应返回 not_found/expired，实际: {}",
        get_expired.message
    );

    // reaped 对象尚未到期，此时仍可读。
    let still_there = get(&mut client, reaped_id).await?;
    assert!(
        still_there.ok,
        "未到期对象在重启后应仍可读: {}",
        still_there.message
    );

    // 等待后台清理任务（1s 间隔）在到期后删除 reaped 对象，覆盖"过期后自动删除"语义。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && object_file(dir.path(), reaped_id).exists() {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        !object_file(dir.path(), reaped_id).exists(),
        "后台清理任务应在到期后删除对象文件（等待 10s 超时）"
    );
    let get_reaped = get(&mut client, reaped_id).await?;
    assert!(!get_reaped.ok, "被清理对象必须不可读");
    let ids = index_object_ids(dir.path())?;
    assert!(ids.is_empty(), "所有对象过期后索引应为空，当前: {ids:?}");

    client.close().await;
    relay.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// 测试 3：object_id 可被任一已认证设备拉取（密钥经 E2E 共享），但只有上传者可删
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vault_get_is_shared_by_object_id_but_delete_is_owner_only() -> Result<()> {
    let dir: TempDir = tempfile::tempdir().context("创建临时暂存目录失败")?;
    let relay = start_relay(vault_config(dir.path())).await?;
    let url = relay.ws_url();
    let mut alice = connect(&url, "alice").await?;
    let mut bob = connect(&url, "bob").await?;

    let object_id = "obj-shared";
    let ciphertext = ciphertext_for(object_id);
    let put_result = put(&mut alice, object_id, &ciphertext, 3600).await?;
    assert!(put_result.ok, "A 上传失败: {}", put_result.message);

    // B 凭 object_id 拉取密文（真实客户端会通过 E2E vault_share 收到解密密钥）。
    let bob_get = get(&mut bob, object_id).await?;
    assert!(
        bob_get.ok,
        "B 应能凭 object_id 拉取密文: {}",
        bob_get.message
    );
    assert_eq!(bob_get.ciphertext.as_deref(), Some(ciphertext.as_slice()));

    // 但只有上传者能删除。
    let bob_delete = delete(&mut bob, object_id).await?;
    assert!(!bob_delete.ok, "非上传者删除必须失败");
    assert!(
        bob_delete.message.contains("protocol_error"),
        "非上传者删除应返回 protocol_error，实际: {}",
        bob_delete.message
    );
    assert!(object_file(dir.path(), object_id).exists());

    let alice_delete = delete(&mut alice, object_id).await?;
    assert!(
        alice_delete.ok,
        "上传者删除应成功: {}",
        alice_delete.message
    );
    assert!(!object_file(dir.path(), object_id).exists());
    let alice_get = get(&mut alice, object_id).await?;
    assert!(!alice_get.ok, "删除后不可读");

    alice.close().await;
    bob.close().await;
    relay.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// 测试 4：未启用暂存时协议返回明确错误
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vault_disabled_reports_clear_error() -> Result<()> {
    let relay = start_relay(relay_config(None)).await?;
    let mut client = connect(&relay.ws_url(), "vault-client").await?;

    let put_result = put(&mut client, "obj-none", b"cipher", 60).await?;
    assert!(!put_result.ok);
    assert!(
        put_result.message.contains("vault_disabled"),
        "未启用暂存应返回 vault_disabled，实际: {}",
        put_result.message
    );

    let get_result = get(&mut client, "obj-none").await?;
    assert!(!get_result.ok);
    assert!(get_result.message.contains("vault_disabled"));
    let list_result = list(&mut client).await?;
    assert!(!list_result.ok);
    assert!(list_result.message.contains("vault_disabled"));

    client.close().await;
    relay.shutdown().await;
    Ok(())
}
