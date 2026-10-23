//! `status` 子命令：只读展示本地状态（不需要网络）。

use anyhow::{Context, Result};
use derive_client::Spool;

use crate::common::{format_ms, human_bytes, App};

pub fn run(app: &App) -> Result<()> {
    let store = app.store()?;
    let config = store.load_config()?;
    println!("数据目录: {}", app.data_dir.display());

    if !store.identity_exists() {
        println!("设备: 未初始化（运行 `derive-sync init --name <设备名>`）");
        let relay = app.relay.clone().unwrap_or(config.relay_url);
        println!(
            "中继: {}",
            if relay.trim().is_empty() {
                "未配置".to_string()
            } else {
                relay
            }
        );
        return Ok(());
    }

    let identity = store.load_identity()?;
    println!("设备: {} ({})", identity.name, identity.device_id());
    let relay = app
        .relay
        .clone()
        .unwrap_or_else(|| config.relay_url.clone());
    println!(
        "中继: {}",
        if relay.trim().is_empty() {
            "未配置（使用 --relay 或修改 config.json）".to_string()
        } else {
            relay
        }
    );
    let token_configured =
        app.token.is_some() || !config.access_token.trim().is_empty();
    println!("令牌: {}", if token_configured { "已配置" } else { "未配置" });

    let peers = store.load_peers()?;
    println!("已配对设备: {}", peers.len());
    for peer in &peers {
        println!(
            "  - {} ({})  配对于 {}",
            peer.display_name(),
            peer.peer.device_id,
            format_ms(peer.paired_at_ms)
        );
    }

    let (catalog, revision) = store.load_catalog()?;
    let index = store.load_local_index()?;
    println!(
        "目录: {} 条可见（revision {}），本机持有 {} 个软件本体",
        catalog.visible_count(),
        revision,
        index.len()
    );
    if catalog.visible_count() > 0 {
        let total_size: u64 = catalog.visible().map(|entry| entry.size).sum();
        println!("目录总大小: {}", human_bytes(total_size));
    }

    let vault_keys = store.load_vault_keys()?;
    println!("云端暂存密钥: {} 个对象", vault_keys.len());

    let spool = Spool::new(store.spool_dir(), &config.spool)?;
    println!("待发队列（离线 spool）: {} 条", spool.count()?);

    // 接收目录里未完成的 .part（断点续传文件）。
    let partials = std::fs::read_dir(store.incoming_dir())
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .ends_with(".part")
                })
                .count()
        })
        .context("读取 incoming 目录失败")?;
    if partials > 0 {
        println!("未完成的接收任务: {partials} 个（重连后可续传）");
    }
    Ok(())
}
