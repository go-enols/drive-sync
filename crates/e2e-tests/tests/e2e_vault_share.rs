//! 阶段 2 端到端：vault put → 会话建立时经 E2E `vault_share` 共享密钥 → 对端拉取解密。
//!
//! 覆盖：
//! 1. A `vault_put`（客户端加密，服务端只存密文；TTL clamp 到 ≥60s）；
//! 2. A/B 建立 E2E 会话后，A 自动把对象密钥 `vault_share` 给 B（B 收到 `VaultKeyReceived`）；
//! 3. B 凭 object_id `vault_get` 下载并解密，BLAKE3/内容与原文一致；
//! 4. `vault_list` 只返回本设备对象（B 看不到 A 的对象），但 `get` 可跨设备；
//! 5. 两个方向都验证：A 自己也能解密取回。

use anyhow::{bail, Result};
use drive_client::{RuntimeConfig, RuntimeEvent};
use drive_core::util::now_ms;
use drive_e2e::client_harness::{
    file_blake3, pair_nodes, write_random_file, ClientNode, DEFAULT_EVENT_TIMEOUT, DEFAULT_TOKEN,
};
use drive_e2e::harness::{relay_config, start_relay};
use tempfile::TempDir;

use anyhow::Context as _;

const VAULT_FILE_SIZE: usize = 96 * 1024;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vault_put_share_and_get_decrypt_on_peer() -> Result<()> {
    let root = TempDir::new()?;
    let relay = start_relay(relay_config(Some(
        root.path().join("relay-vault"),
    )))
    .await?;
    let url = relay.ws_url();
    let dir_a = root.path().join("device-a");
    let dir_b = root.path().join("device-b");
    let files = root.path().join("files");

    pair_nodes(&dir_a, &dir_b, "暂存A", "暂存B", &url, DEFAULT_TOKEN).await?;

    // 1) A 用一次性连接上传（此时两端 Runtime 都未启动，避免同 device 互踢）。
    let src = write_random_file(&files, "vault-secret.bin", VAULT_FILE_SIZE)?;
    let src_hash = file_blake3(&src)?;
    let config_a = RuntimeConfig::new(&dir_a, &url, DEFAULT_TOKEN);
    let put = drive_client::vault_put(&config_a, &src, 1)
        .await
        .context("vault_put 失败")?;
    assert!(
        put.expires_at_ms > now_ms() + 59_000,
        "服务端 TTL 应 clamp 到 >= 60s（实际 expires_at_ms={}）",
        put.expires_at_ms
    );
    assert_eq!(put.ttl_secs, 24 * 60 * 60, "请求 1 天 TTL 应原样生效");

    // 2) 启动 A/B：会话建立后 A 自动共享 vault 密钥，B 应收到 VaultKeyReceived。
    let mut a = ClientNode::start(&dir_a, &url, DEFAULT_TOKEN)?;
    let mut b = ClientNode::start(&dir_b, &url, DEFAULT_TOKEN)?;
    a.wait_authenticated().await?;
    b.wait_authenticated().await?;
    let a_id = a.device_id().to_string();
    let b_id = b.device_id().to_string();
    a.wait_session(&b_id).await?;
    b.wait_session(&a_id).await?;

    let object_id = put.object_id.clone();
    b.wait_event(DEFAULT_EVENT_TIMEOUT, |event| {
        matches!(event, RuntimeEvent::VaultKeyReceived { object_id: id, .. } if *id == object_id)
    })
    .await?;
    a.wait_event(DEFAULT_EVENT_TIMEOUT, |event| {
        matches!(event, RuntimeEvent::VaultShared { object_id: id, .. } if *id == object_id)
    })
    .await?;

    // 3) 停掉 Runtime（同 device 只能一条连接），用一次性连接做 vault_get。
    a.shutdown().await?;
    b.shutdown().await?;

    let config_b = RuntimeConfig::new(&dir_b, &url, DEFAULT_TOKEN);
    let restored_b = drive_client::vault_get(
        &config_b,
        &put.object_id,
        &root.path().join("restored-by-b.bin"),
    )
    .await
    .context("B 解密下载 vault 对象失败")?;
    assert_eq!(file_blake3(&restored_b)?, src_hash, "B 解密内容 BLAKE3 不一致");
    assert_eq!(
        std::fs::read(&restored_b)?,
        std::fs::read(&src)?,
        "B 解密内容与原文不一致"
    );

    // A 自己也能取回（本地保留 object_key）。
    let restored_a = drive_client::vault_get(
        &config_a,
        &put.object_id,
        &root.path().join("restored-by-a.bin"),
    )
    .await
    .context("A 解密下载 vault 对象失败")?;
    assert_eq!(file_blake3(&restored_a)?, src_hash);

    // 4) vault_list 只列本设备对象：A 可见，B 为空（跨设备 get 由 object_id + 共享密钥完成）。
    let list_a = drive_client::vault_list(&config_a).await?;
    if !list_a.iter().any(|object| object.object_id == put.object_id) {
        bail!("A 的 vault_list 应包含刚上传的对象: {list_a:?}");
    }
    let list_b = drive_client::vault_list(&config_b).await?;
    assert!(
        list_b.is_empty(),
        "vault_list 只返回本设备对象，B 不应看到 A 的对象: {list_b:?}"
    );

    relay.shutdown().await;
    Ok(())
}
