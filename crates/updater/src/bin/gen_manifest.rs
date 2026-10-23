//! `gen-manifest`：drive-sync 发布流水线用的更新清单生成/合并工具。
//!
//! 拆成两步是为了配合多平台 CI：
//!
//! 1. 每个构建 job 用 `gen-manifest asset` 为**本平台产物**（裸可执行文件）计算
//!    size/BLAKE3，写出一份 asset JSON；
//! 2. 汇总 job 用 `gen-manifest merge` 合并所有 asset JSON，生成未签名的
//!    `latest.json`（`signature: null`、`signed: false`）；
//! 3. 有 `UPDATE_SIGNING_KEY` 时再用 `sign-manifest` 签名（输出 `signed: true`）。
//!
//! 输出版本号也可以用 `gen-manifest version`（来自 workspace.package.version）。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use drive_updater::manifest::MAX_MANIFEST_BYTES;
use drive_updater::{
    hash_file, parse_manifest_unverified, verify_manifest, UpdateAsset, UpdateManifest,
    DEFAULT_UPDATE_PUBKEY_HEX,
};

#[derive(Parser, Debug)]
#[command(
    name = "gen-manifest",
    version,
    about = "drive-sync 更新清单生成/合并工具（生成未签名清单，签名用 sign-manifest）",
    long_about = None
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 打印 workspace 版本号（供 CI 使用）
    Version,
    /// 计算单个平台产物并写出 asset JSON
    Asset {
        /// 目标平台 triple，例如 x86_64-unknown-linux-gnu
        #[arg(long)]
        target: String,
        /// 产物在 GitHub Release 中的下载地址
        #[arg(long)]
        url: String,
        /// 本地产物文件（裸可执行文件）
        #[arg(long, value_name = "FILE")]
        file: PathBuf,
        /// 输出 asset JSON 路径
        #[arg(long, short = 'o', value_name = "FILE")]
        out: PathBuf,
    },
    /// 合并 assets 目录下所有 *.json，输出未签名 latest.json
    Merge {
        /// 版本号（如 0.3.0，可带 v 前缀）
        #[arg(long)]
        version: String,
        /// 发布时间（Unix 毫秒，默认当前时间）
        #[arg(long, value_name = "MS")]
        published_at_ms: Option<u64>,
        /// 发布说明（纯文本）
        #[arg(long)]
        notes: Option<String>,
        /// 从文件读取发布说明（优先于 --notes）
        #[arg(long, value_name = "FILE")]
        notes_file: Option<PathBuf>,
        /// asset JSON 所在目录
        #[arg(long, value_name = "DIR", default_value = "assets")]
        assets: PathBuf,
        /// 输出 latest.json 路径
        #[arg(long, short = 'o', value_name = "FILE")]
        out: PathBuf,
    },
    /// 校验清单签名（CI 自检/人工核对）
    Verify {
        /// 清单 JSON 路径
        #[arg(long, short = 'i', value_name = "FILE")]
        manifest: PathBuf,
        /// 公钥（64 hex；默认用环境变量 DRIVE_UPDATE_PUBKEY 或编译期 fallback）
        #[arg(long, value_name = "HEX")]
        pubkey_hex: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    match Args::parse().command {
        Command::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
        }
        Command::Asset {
            target,
            url,
            file,
            out,
        } => cmd_asset(&target, &url, &file, &out)?,
        Command::Merge {
            version,
            published_at_ms,
            notes,
            notes_file,
            assets,
            out,
        } => cmd_merge(
            &version,
            published_at_ms,
            notes,
            notes_file.as_deref(),
            &assets,
            &out,
        )?,
        Command::Verify {
            manifest,
            pubkey_hex,
        } => cmd_verify(&manifest, pubkey_hex.as_deref())?,
    }
    Ok(())
}

fn cmd_asset(target: &str, url: &str, file: &Path, out: &Path) -> anyhow::Result<()> {
    let (size, blake3) =
        hash_file(file).with_context(|| format!("计算产物哈希失败: {}", file.display()))?;
    let asset = UpdateAsset {
        target: target.to_string(),
        url: url.to_string(),
        size,
        blake3,
    };
    asset
        .validate()
        .with_context(|| format!("产物 {target} 的 asset 字段非法"))?;
    write_json(out, &asset)?;
    eprintln!(
        "asset: target={target} size={size} blake3={} → {}",
        asset.blake3,
        out.display()
    );
    Ok(())
}

fn cmd_merge(
    version: &str,
    published_at_ms: Option<u64>,
    notes: Option<String>,
    notes_file: Option<&Path>,
    assets_dir: &Path,
    out: &Path,
) -> anyhow::Result<()> {
    let notes = match notes_file {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("读取发布说明失败: {}", path.display()))?,
        None => notes.unwrap_or_default(),
    };
    let published_at_ms = published_at_ms.unwrap_or_else(drive_core::util::now_ms);
    let mut assets = Vec::new();
    let entries = std::fs::read_dir(assets_dir)
        .with_context(|| format!("读取 assets 目录失败: {}", assets_dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("读取 asset JSON 失败: {}", path.display()))?;
        let asset: UpdateAsset = serde_json::from_str(&raw)
            .with_context(|| format!("解析 asset JSON 失败: {}", path.display()))?;
        assets.push(asset);
    }
    if assets.is_empty() {
        bail!(
            "assets 目录 {} 下没有 *.json（每个构建 job 需先运行 gen-manifest asset）",
            assets_dir.display()
        );
    }
    assets.sort_by(|a, b| a.target.cmp(&b.target));
    let manifest = UpdateManifest::new(version, published_at_ms, notes, assets)
        .with_context(|| format!("构建清单失败（version={version}）"))?;
    let mut value = serde_json::to_value(&manifest)?;
    if let serde_json::Value::Object(map) = &mut value {
        map.insert("signature".into(), serde_json::Value::Null);
        map.insert("signed".into(), serde_json::Value::Bool(false));
    }
    let text = serde_json::to_string_pretty(&value)?;
    let rendered = format!("{text}\n");
    if rendered.len() as u64 > MAX_MANIFEST_BYTES {
        bail!("清单超过 {MAX_MANIFEST_BYTES} 字节（notes 太长？）");
    }
    write_text(out, &rendered)?;
    eprintln!(
        "未签名清单已生成: {}（v{}，{} 个平台）",
        out.display(),
        manifest.version,
        manifest.assets.len()
    );
    for asset in &manifest.assets {
        eprintln!("  - {} {}", asset.target, asset.url);
    }
    Ok(())
}

fn cmd_verify(manifest: &Path, pubkey_hex: Option<&str>) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(manifest)
        .with_context(|| format!("读取清单失败: {}", manifest.display()))?;
    let pubkey = match pubkey_hex {
        Some(pubkey) => pubkey.trim().to_string(),
        None => match std::env::var("DRIVE_UPDATE_PUBKEY") {
            Ok(pubkey) if !pubkey.trim().is_empty() => pubkey.trim().to_string(),
            _ => DEFAULT_UPDATE_PUBKEY_HEX.trim().to_string(),
        },
    };
    match verify_manifest(&raw, &pubkey) {
        Ok(manifest) => {
            eprintln!(
                "签名有效: v{}（{} 个平台产物）",
                manifest.version,
                manifest.assets.len()
            );
            println!("{}", serde_json::to_string_pretty(&manifest)?);
            Ok(())
        }
        Err(error) => {
            // 未签名清单仍打印结构信息，便于排查 CI 没配上 secret 的情况。
            if let Ok(unsigned) = parse_manifest_unverified(&raw) {
                eprintln!(
                    "清单未通过验签: {error}\n版本 v{}，{} 个平台产物（signed={}）",
                    unsigned.version,
                    unsigned.assets.len(),
                    unsigned
                        .signature
                        .as_deref()
                        .map(|_| "true")
                        .unwrap_or("false")
                );
            }
            Err(error.into())
        }
    }
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    write_text(path, &format!("{text}\n"))
}

fn write_text(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, text).with_context(|| format!("写入 {} 失败", path.display()))
}
