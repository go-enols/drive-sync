//! `peers` 子命令：查看 / 删除已配对设备。
//!
//! 配对记录没有 TTL：只有显式 `peers remove`（或对端发送撤销）才会解除。
//! 本命令是**纯本地文件操作**（不连中继），daemon 运行中也可执行；daemon 会每 2 秒
//! 检测 peers.json 变化，被删除的对端会尽力收到 `PairRevoked` 并立即销毁会话。

use std::io::{IsTerminal, Write};

use anyhow::{bail, Result};
use clap::Subcommand;
use derive_client::Store;
use derive_core::pairing::PairRecord;

use crate::common::{format_ms, App};

#[derive(Subcommand)]
pub enum PeersCommand {
    /// 列出已配对设备。
    List {
        /// 输出 JSON（便于脚本断言）。
        #[arg(long)]
        json: bool,
    },
    /// 删除配对记录（本地操作；daemon 运行时会实时通知对端撤销）。
    Remove {
        /// 设备 ID 或唯一前缀
        target: String,
        /// 跳过二次确认（非交互环境默认不提示）
        #[arg(long)]
        yes: bool,
    },
}

pub fn run(app: &App, command: PeersCommand) -> Result<()> {
    let store = app.store()?;
    match command {
        PeersCommand::List { json } => list(&store, json),
        PeersCommand::Remove { target, yes } => remove(&store, &target, yes),
    }
}

fn sorted_peers(store: &Store) -> Result<Vec<PairRecord>> {
    let mut peers = store.load_peers()?;
    peers.sort_by(|a, b| a.peer.device_id.cmp(&b.peer.device_id));
    Ok(peers)
}

fn list(store: &Store, json: bool) -> Result<()> {
    let peers = sorted_peers(store)?;
    if json {
        let items: Vec<serde_json::Value> = peers
            .iter()
            .map(|record| {
                serde_json::json!({
                    "device_id": record.peer.device_id,
                    "name": record.peer.name,
                    "label": record.display_name(),
                    "relay_url": record.relay_url,
                    "paired_at_ms": record.paired_at_ms,
                    "paired_at": format_ms(record.paired_at_ms),
                    "pairing_fingerprint": record.pairing_fingerprint,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }
    if peers.is_empty() {
        println!("尚无已配对设备。运行 `derive-sync pair` 与其他设备配对。");
        return Ok(());
    }
    println!(
        "已配对设备（{} 个；配对永久有效，只有显式 `peers remove` 才会解除）:",
        peers.len()
    );
    for (index, record) in peers.iter().enumerate() {
        println!();
        println!(
            "  [{}] {} ({})",
            index + 1,
            record.display_name(),
            record.peer.device_id
        );
        println!(
            "      配对时间: {} | 配对指纹: {}",
            format_ms(record.paired_at_ms),
            record.pairing_fingerprint
        );
        println!("      中继: {}", record.relay_url);
    }
    println!();
    println!("移除配对: derive-sync peers remove <DEVICE_ID|唯一前缀>");
    Ok(())
}

fn resolve_match<'a>(peers: &'a [PairRecord], target: &str) -> Result<Option<&'a PairRecord>> {
    if let Some(exact) = peers
        .iter()
        .find(|record| record.peer.device_id == target)
    {
        return Ok(Some(exact));
    }
    let matches: Vec<&PairRecord> = peers
        .iter()
        .filter(|record| record.peer.device_id.starts_with(target))
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        _ => {
            let candidates = matches
                .iter()
                .map(|record| {
                    format!("  {}  {}", record.peer.device_id, record.display_name())
                })
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "前缀 `{target}` 匹配到多个配对记录，请提供更长的前缀或完整 device_id:\n{candidates}"
            )
        }
    }
}

fn remove(store: &Store, target: &str, yes: bool) -> Result<()> {
    let peers = sorted_peers(store)?;
    let Some(record) = resolve_match(&peers, target)? else {
        bail!("未找到匹配的配对记录: {target}（可用 `derive-sync peers list` 查看）");
    };
    let device_id = record.peer.device_id.clone();
    let name = record.display_name();

    if !yes && std::io::stdin().is_terminal() {
        print!("确认删除与 {name} ({device_id}) 的配对记录？此后需要重新配对才能同步。[y/N]: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let confirmed = std::io::stdin()
            .read_line(&mut line)
            .map(|_| matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
            .unwrap_or(false);
        if !confirmed {
            bail!("已取消删除");
        }
    }

    store.remove_peer(&device_id)?;
    println!("已删除配对记录: {name} ({device_id})");
    println!("  对端将无法再与本机建立会话；对端在线且 daemon 运行时会在约 2 秒内收到 PairRevoked 通知，");
    println!("  若对端离线，它下次尝试握手时会被拒绝（需要重新配对）。");
    println!("  提示: 本机待发队列/进行中的传输由 daemon 自动清理；本机文件不会被删除。");
    Ok(())
}
