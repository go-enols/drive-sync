//! `sign-manifest`：drive-sync 更新清单签名 / 验签 / 文件哈希工具。
//!
//! 用法：
//!
//! ```text
//! # 生成一对新的签名密钥（公钥编译进客户端；种子只放 CI Secret）
//! sign-manifest --keygen
//!
//! # 从环境变量 UPDATE_SIGNING_KEY 读取种子，签名 stdin 清单并输出到 stdout
//! UPDATE_SIGNING_KEY=<64 hex> sign-manifest < latest.json > latest.signed.json
//!
//! # 读写文件 + 展示 signed 标记
//! sign-manifest --in latest.json --out latest.signed.json
//!
//! # 验签
//! sign-manifest --verify --pubkey-hex <64 hex> --in latest.json
//!
//! # 计算产物哈希（发布脚本/人工核对）
//! sign-manifest --hash dist/drive-sync.exe
//! ```
//!
//! 私钥（64 字符 hex 种子）**永远不要提交仓库**：本地可放 `.env` 或系统环境变量，
//! CI 用 GitHub Actions Secret `UPDATE_SIGNING_KEY`。

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use clap::{ArgAction, Parser};
use drive_updater::manifest::{keygen, sign_manifest};
use drive_updater::{hash_file, verify_manifest, UpdateManifest, DEFAULT_UPDATE_PUBKEY_HEX};

#[derive(Parser, Debug)]
#[command(
    name = "sign-manifest",
    version,
    about = "drive-sync 更新清单签名工具（Ed25519 + canonical JSON）",
    long_about = None
)]
struct Args {
    /// 生成一对新的签名密钥（打印公钥与种子），不读写清单
    #[arg(long)]
    keygen: bool,

    /// 签名种子的环境变量名（64 字符 hex 种子）
    #[arg(long, default_value = "UPDATE_SIGNING_KEY", value_name = "ENV")]
    key_env: String,

    /// 直接指定签名种子（64 hex；仅本地测试，生产用环境变量/CI Secret）
    #[arg(long, value_name = "HEX")]
    key_hex: Option<String>,

    /// 输入清单 JSON（默认 stdin；`--hash` 时不需要）
    #[arg(long = "in", short = 'i', value_name = "FILE")]
    input: Option<PathBuf>,

    /// 输出文件（默认 stdout）
    #[arg(long = "out", short = 'o', value_name = "FILE")]
    output: Option<PathBuf>,

    /// 计算文件 BLAKE3 与大小（输出 JSON）
    #[arg(long, value_name = "FILE")]
    hash: Option<PathBuf>,

    /// 验签模式：校验输入清单签名
    #[arg(long, action = ArgAction::SetTrue)]
    verify: bool,

    /// 验签公钥（64 hex；默认取 --pubkey-env 或编译期默认公钥）
    #[arg(long, value_name = "HEX")]
    pubkey_hex: Option<String>,

    /// 验签公钥的环境变量名
    #[arg(long, default_value = "DRIVE_UPDATE_PUBKEY", value_name = "ENV")]
    pubkey_env: String,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.keygen {
        let (pubkey, seed) = keygen();
        println!("Ed25519 更新签名密钥对（请妥善保存种子，不要提交仓库）:");
        println!("pubkey (64 hex, 编译进客户端): {pubkey}");
        println!("seed   (64 hex, CI Secret UPDATE_SIGNING_KEY): {seed}");
        println!();
        println!("下一步：");
        println!(
            "  1) 把 pubkey 写入 crates/updater/src/lib.rs 的 DEFAULT_UPDATE_PUBKEY_HEX fallback"
        );
        println!("  2) 把 seed 配置为 GitHub Actions Secret: UPDATE_SIGNING_KEY");
        return Ok(());
    }

    if let Some(path) = &args.hash {
        let (size, blake3) = hash_file(path)?;
        println!(
            "{}",
            serde_json::json!({
                "file": path.display().to_string(),
                "size": size,
                "blake3": blake3,
            })
        );
        return Ok(());
    }

    let manifest_json = read_input(args.input.as_deref())?;

    if args.verify {
        let pubkey = resolve_pubkey(&args)?;
        let manifest = verify_manifest(&manifest_json, &pubkey)?;
        eprintln!(
            "签名有效: v{}（{} 个平台产物，发布于 {} ms）",
            manifest.version,
            manifest.assets.len(),
            manifest.published_at_ms
        );
        println!("{}", serde_json::to_string_pretty(&manifest)?);
        return Ok(());
    }

    // 默认动作：签名。
    let seed = resolve_seed(&args)?;
    let manifest: UpdateManifest = serde_json::from_str(&manifest_json)
        .context("输入不是合法的 UpdateManifest JSON（字段见 docs/updates.md）")?;
    let signed = sign_manifest(&manifest, &seed)?;
    let text = render_signed(&signed)?;
    write_output(args.output.as_deref(), &text)
}

/// 输出带 `signed: true` 标记的清单 JSON。
///
/// 标记只是给人和发布脚本看的提示；权威判断是 `signature` 字段能否验签通过。
fn render_signed(manifest: &UpdateManifest) -> anyhow::Result<String> {
    let mut value = serde_json::to_value(manifest)?;
    if let serde_json::Value::Object(map) = &mut value {
        map.insert("signed".to_string(), serde_json::Value::Bool(true));
    }
    Ok(serde_json::to_string_pretty(&value)?)
}

fn resolve_seed(args: &Args) -> anyhow::Result<String> {
    if let Some(seed) = &args.key_hex {
        return Ok(seed.trim().to_string());
    }
    let seed = std::env::var(&args.key_env).unwrap_or_default();
    if seed.trim().is_empty() {
        bail!(
            "未提供签名种子：请设置环境变量 {}（64 字符 hex），或用 --key-hex（仅本地测试）；\n\
             尚未生成密钥可先运行 `sign-manifest --keygen`。",
            args.key_env
        );
    }
    Ok(seed.trim().to_string())
}

fn resolve_pubkey(args: &Args) -> anyhow::Result<String> {
    if let Some(pubkey) = &args.pubkey_hex {
        return Ok(pubkey.trim().to_string());
    }
    if let Ok(pubkey) = std::env::var(&args.pubkey_env) {
        if !pubkey.trim().is_empty() {
            return Ok(pubkey.trim().to_string());
        }
    }
    if DEFAULT_UPDATE_PUBKEY_HEX.trim().is_empty() {
        bail!(
            "未提供公钥：请用 --pubkey-hex 或设置环境变量 {}",
            args.pubkey_env
        );
    }
    Ok(DEFAULT_UPDATE_PUBKEY_HEX.trim().to_string())
}

fn read_input(path: Option<&Path>) -> anyhow::Result<String> {
    let mut text = String::new();
    match path {
        Some(path) => {
            text = std::fs::read_to_string(path)
                .with_context(|| format!("读取清单失败: {}", path.display()))?;
        }
        None => {
            std::io::stdin()
                .read_to_string(&mut text)
                .context("从 stdin 读取清单失败（可用 --in <FILE>）")?;
        }
    }
    if text.trim().is_empty() {
        bail!("输入清单为空");
    }
    Ok(text)
}

fn write_output(path: Option<&Path>, text: &str) -> anyhow::Result<()> {
    match path {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, format!("{text}\n"))
                .with_context(|| format!("写入 {} 失败", path.display()))?;
            eprintln!("已写入 {}", path.display());
        }
        None => println!("{text}"),
    }
    Ok(())
}
