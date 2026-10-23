//! `vault` 子命令：put / list / get（客户端加密的 30 天暂存）。
//!
//! 上传前用随机 32 字节对象密钥做 XChaCha20-Poly1305 加密，密钥只存本地
//! `vault_keys.json`；daemon 在线时通过 E2E 通道把密钥共享给已配对设备。

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::Subcommand;
use derive_core::vault::MAX_VAULT_TTL_SECS;

use crate::common::{format_ms, human_bytes, App};

#[derive(Subcommand)]
pub enum VaultCommand {
    /// 上传加密对象（服务端只保存密文，到期自动删除）。
    Put {
        #[arg(long)]
        path: PathBuf,
        /// 保留天数（1~30，默认 7；服务端强制上限 30 天）
        #[arg(long, default_value_t = 7)]
        ttl_days: u64,
    },
    /// 列出云端暂存对象。
    List,
    /// 下载并解密对象到本地文件。
    Get {
        object_id: String,
        #[arg(long)]
        out: PathBuf,
    },
}

pub async fn run(app: &App, command: VaultCommand) -> Result<()> {
    match command {
        VaultCommand::Put { path, ttl_days } => put(app, path, ttl_days).await,
        VaultCommand::List => list(app).await,
        VaultCommand::Get { object_id, out } => get(app, &object_id, out).await,
    }
}

async fn put(app: &App, path: PathBuf, ttl_days: u64) -> Result<()> {
    if ttl_days == 0 {
        bail!("--ttl-days 至少为 1（服务端最小 TTL 60 秒、最大 30 天）");
    }
    let store = app.store()?;
    let config = app.runtime_config(&store)?;
    println!("正在加密并上传（TTL {ttl_days} 天，上限 30 天）…");
    let outcome = derive_client::ops::vault_put(&config, &path, ttl_days).await?;
    println!("上传成功！");
    println!("  对象 ID: {}", outcome.object_id);
    println!(
        "  保留至: {}（{} 秒）",
        format_ms(outcome.expires_at_ms),
        outcome.ttl_secs
    );
    println!("  对象密钥已保存到本地 vault_keys.json（服务端不可见）。");
    println!("提示: daemon 在线时会在约 2 秒内把密钥共享给已配对设备。");
    let _ = MAX_VAULT_TTL_SECS;
    Ok(())
}

async fn list(app: &App) -> Result<()> {
    let store = app.store()?;
    let config = app.runtime_config(&store)?;
    let objects = derive_client::ops::vault_list(&config).await?;
    if objects.is_empty() {
        println!("云端暂无暂存对象。");
        return Ok(());
    }
    println!("云端暂存对象（{} 个）:", objects.len());
    println!(
        "{:<22} {:<24} {:>12} {:<16} {:<16}",
        "对象 ID", "名称", "大小", "上传时间", "过期时间"
    );
    for object in &objects {
        println!(
            "{:<22} {:<24} {:>12} {:<16} {:<16}",
            object.object_id,
            truncate_chars(&object.meta.name, 24),
            human_bytes(object.meta.size),
            format_ms(object.created_at_ms),
            format_ms(object.expires_at_ms)
        );
    }
    Ok(())
}

async fn get(app: &App, object_id: &str, out: PathBuf) -> Result<()> {
    let store = app.store()?;
    let config = app.runtime_config(&store)?;
    if store.find_vault_key(object_id)?.is_none() {
        bail!(
            "本地没有对象 {object_id} 的解密密钥（vault_keys.json）。\
             密钥只保存在上传方设备，并会通过 E2E 通道共享给已配对设备；\
             请确认上传方已在线同步，或在本机重新上传。"
        );
    }
    println!("正在下载并解密 {object_id} …");
    let path = derive_client::ops::vault_get(&config, object_id, &out).await?;
    println!("已保存到: {}", path.display());
    Ok(())
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}
