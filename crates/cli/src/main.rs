//! drive-sync 命令行客户端。
//!
//! 子命令：
//! - `init --name <设备名>`：初始化身份与配置
//! - `pair [--qr <扫码串> | --code <8位配对码>] [--yes]`：配对（不带参数为发起方）
//! - `daemon [--clipboard auto|text|image|file:<path>] [--poll-ms N]`：常驻同步
//! - `status`：本地状态
//! - `peers list|remove`：查看 / 删除已配对设备（配对记录无 TTL，只有显式删除才解除）
//! - `catalog add|list|remove|fetch`：软件目录（本体留在本地，云端只中转）
//! - `vault put|list|get`：客户端加密的云端暂存（TTL ≤ 30 天）
//! - `update check|apply`：检查 / 下载并安装新版本（Ed25519 签名清单 + BLAKE3 校验）
//!
//! 全局参数：`--data-dir`（或环境变量 `DRIVE_SYNC_DATA_DIR`）、`--log-level`、
//! `--relay`、`--token`。
//!
//! 日志一律写 stderr（只含类型/大小/设备 id，不打印内容明文）；stdout 只输出命令结果。

mod catalog_cmd;
mod clipboard;
mod common;
mod daemon;
mod pair_cmd;
mod peers_cmd;
mod status_cmd;
mod update_cmd;
mod vault_cmd;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use drive_client::Store;

use crate::common::App;

#[derive(Parser)]
#[command(
    name = "drive-sync",
    version,
    about = "drive-sync 桌面同步客户端：共享剪切板 + 软件目录 + 可选加密暂存",
    long_about = None,
    subcommand_required = true,
    arg_required_else_help = true
)]
struct Cli {
    /// 数据目录（默认使用平台数据目录；也可用 DRIVE_SYNC_DATA_DIR）
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// 日志级别: off / error / warn / info / debug / trace
    #[arg(long, global = true, default_value = "info", value_name = "LEVEL")]
    log_level: String,

    /// 中继地址，例如 ws://127.0.0.1:8787/ws（wss:// 需要构建时启用 tls feature）
    #[arg(long, global = true, value_name = "URL")]
    relay: Option<String>,

    /// 中继访问令牌（也可写入 config.json）
    #[arg(long, global = true, value_name = "TOKEN")]
    token: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 初始化设备身份与配置（生成 Ed25519/X25519 密钥，写入 data_dir）。
    Init {
        /// 设备显示名（例如 "我的笔记本"）
        #[arg(long)]
        name: String,
    },
    /// 配对：不带参数为发起方；带 --qr/--code 为加入方。
    Pair {
        /// 对方二维码串（dsync-pair-v1:...）
        #[arg(long, value_name = "SCAN")]
        qr: Option<String>,
        /// 对方显示的 8 位配对码
        #[arg(long, value_name = "CODE")]
        code: Option<String>,
        /// 自动确认 SAS（脚本/测试用；生产建议人工核对）
        #[arg(long)]
        yes: bool,
    },
    /// 常驻同步（剪切板 + 目录 + 文件传输；Ctrl-C 退出）。
    Daemon {
        /// 剪切板模式: auto | text | image | file:<path>
        #[arg(long, default_value = "auto", value_name = "MODE")]
        clipboard: String,
        /// 剪切板轮询间隔（毫秒）
        #[arg(long, default_value_t = 300, value_name = "MS")]
        poll_ms: u64,
        /// 启动时检查一次自动更新，之后每 6 小时检查（默认关闭）
        #[arg(long)]
        auto_update: bool,
        /// 自动更新安装成功后自动拉起新进程并退出（配合 --auto-update）
        #[arg(long)]
        update_restart: bool,
    },
    /// 查看本地状态（设备/配对/目录/暂存/spool）。
    Status,
    /// 查看 / 删除已配对设备（配对永久有效，除非显式删除）。
    Peers {
        #[command(subcommand)]
        command: peers_cmd::PeersCommand,
    },
    /// 软件目录管理。
    Catalog {
        #[command(subcommand)]
        command: catalog_cmd::CatalogCommand,
    },
    /// 云端加密暂存（30 天 TTL）。
    Vault {
        #[command(subcommand)]
        command: vault_cmd::VaultCommand,
    },
    /// 检查 / 下载并安装新版本（Ed25519 签名清单 + BLAKE3 校验）。
    Update {
        #[command(subcommand)]
        command: update_cmd::UpdateCommand,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    common::init_logging(&cli.log_level);
    if let Err(error) = run(cli).await {
        eprintln!("错误: {error:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let app = App::new(cli.data_dir, cli.relay, cli.token);
    match cli.command {
        Command::Init { name } => init(&app, &name),
        Command::Status => status_cmd::run(&app),
        Command::Pair { qr, code, yes } => {
            let store = app.store()?;
            let config = app.runtime_config(&store)?;
            pair_cmd::run(&config, qr, code, yes).await
        }
        Command::Daemon {
            clipboard,
            poll_ms,
            auto_update,
            update_restart,
        } => daemon::run(&app, &clipboard, poll_ms, auto_update, update_restart).await,
        Command::Peers { command } => peers_cmd::run(&app, command),
        Command::Catalog { command } => catalog_cmd::run(&app, command).await,
        Command::Vault { command } => vault_cmd::run(&app, command).await,
        Command::Update { command } => update_cmd::run(&app, command),
    }
}

fn init(app: &App, name: &str) -> Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("--name 不能为空");
    }
    let store = Store::open(&app.data_dir)?;
    let (identity, created) = store.load_or_create_identity(name)?;

    let mut config = store.load_config()?;
    let mut config_changed = false;
    if let Some(relay) = &app.relay {
        if config.relay_url != *relay {
            config.relay_url = relay.clone();
            config_changed = true;
        }
    }
    if let Some(token) = &app.token {
        if config.access_token != *token {
            config.access_token = token.clone();
            config_changed = true;
        }
    }
    if config_changed {
        store.save_config(&config)?;
    }

    println!("数据目录: {}", app.data_dir.display());
    println!("设备 ID: {}", identity.device_id());
    println!("设备名称: {}", identity.name);
    if created {
        println!("身份已创建。");
    } else {
        println!("设备已初始化（身份文件已存在，名称沿用旧值）。");
    }
    println!(
        "中继: {}",
        if config.relay_url.trim().is_empty() {
            "未配置（可用 --relay 指定，或下次运行 init/编辑 config.json）".to_string()
        } else {
            config.relay_url.clone()
        }
    );
    println!("下一步: 运行 `drive-sync pair` 与其他设备配对，然后 `drive-sync daemon` 常驻同步。");
    Ok(())
}
