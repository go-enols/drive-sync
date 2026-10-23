//! `catalog` 子命令：add / list / remove / fetch（一次性连接下载）。

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use drive_client::{CatalogAddParams, LocalIndexEntry};
use drive_core::catalog::{CatalogEntry, TargetPlatform};
use drive_core::util::now_ms;

use crate::common::{format_ms, human_bytes, App};

#[derive(Subcommand)]
pub enum CatalogCommand {
    /// 添加软件目录条目（自动计算 BLAKE3 并记录本地路径）。
    Add {
        /// 软件本体文件路径
        #[arg(long)]
        path: PathBuf,
        /// 展示名（默认取文件名）
        #[arg(long)]
        name: Option<String>,
        /// 版本号（例如 1.2.3）
        #[arg(long)]
        version: Option<String>,
        /// 目标平台: windows/linux/android/ios/macos/any（默认当前平台）
        #[arg(long)]
        platform: Option<String>,
        /// 分类: installer/portable/package/iso/document/file 等（默认 file）
        #[arg(long)]
        kind: Option<String>,
        /// 标签，逗号分隔
        #[arg(long)]
        tags: Option<String>,
        /// 备注（不会同步敏感信息请勿填写）
        #[arg(long)]
        notes: Option<String>,
    },
    /// 列出目录条目。
    List,
    /// 删除目录条目（写墓碑；不会删除本机文件）。
    Remove { id: String },
    /// 从在线已配对设备下载软件本体到本地。
    Fetch {
        entry_id: String,
        /// 输出目录（默认写入数据目录的 incoming/）
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

pub async fn run(app: &App, command: CatalogCommand) -> Result<()> {
    match command {
        CatalogCommand::Add {
            path,
            name,
            version,
            platform,
            kind,
            tags,
            notes,
        } => add(app, path, name, version, platform, kind, tags, notes),
        CatalogCommand::List => list(app),
        CatalogCommand::Remove { id } => remove(app, &id),
        CatalogCommand::Fetch { entry_id, out } => fetch(app, &entry_id, out).await,
    }
}

#[allow(clippy::too_many_arguments)]
fn add(
    app: &App,
    path: PathBuf,
    name: Option<String>,
    version: Option<String>,
    platform: Option<String>,
    kind: Option<String>,
    tags: Option<String>,
    notes: Option<String>,
) -> Result<()> {
    let store = app.store()?;
    let identity = store.load_identity()?;
    let node = identity.device_id();
    let path = std::path::absolute(&path).unwrap_or(path);
    let (size, content_hash) = drive_client::util::hash_file(&path)
        .with_context(|| format!("读取/计算文件哈希失败: {}", path.display()))?;
    let platform = match platform {
        Some(text) => TargetPlatform::parse(&text).ok_or_else(|| {
            anyhow::anyhow!("未知平台: {text}（可用 windows/linux/android/ios/macos/any）")
        })?,
        None => drive_client::engine::default_platform(),
    };
    let name = name.unwrap_or_else(|| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_else(|| "未命名".to_string())
    });
    let tags: Vec<String> = tags
        .map(|raw| {
            raw.split(',')
                .map(|tag| tag.trim())
                .filter(|tag| !tag.is_empty())
                .map(|tag| tag.to_string())
                .collect()
        })
        .unwrap_or_default();

    let (mut catalog, mut revision) = store.load_catalog()?;
    if catalog.node.trim().is_empty() {
        catalog.node = node.clone();
    }
    let entry = CatalogEntry::new(
        name,
        version.unwrap_or_default(),
        platform,
        kind.unwrap_or_else(|| "file".to_string()),
        size,
        content_hash.clone(),
        tags,
        notes.unwrap_or_default(),
        &node,
    );
    let now = now_ms();
    let _op = catalog.upsert_local(entry.clone(), &node, now);
    revision = revision.saturating_add(1);

    let mut index = store.load_local_index()?;
    index.insert(
        entry.id.clone(),
        LocalIndexEntry {
            path: path.clone(),
            content_hash,
            size,
            updated_at_ms: now,
        },
    );
    store.save_catalog(&catalog, revision)?;
    store.save_local_index(&index)?;

    println!("已添加目录条目: {}", entry.id);
    println!(
        "  {} v{} ({})，{}，{}",
        entry.name,
        if entry.version.is_empty() {
            "-"
        } else {
            entry.version.as_str()
        },
        entry.platform.as_str(),
        human_bytes(entry.size),
        entry.kind
    );
    println!("  BLAKE3: {}", entry.content_hash);
    println!("  本地路径: {}", path.display());
    println!("提示: 若 daemon 正在运行，会在约 2 秒内自动加载并把快照同步给在线设备。");
    Ok(())
}

fn list(app: &App) -> Result<()> {
    let store = app.store()?;
    let (catalog, revision) = store.load_catalog()?;
    let index = store.load_local_index()?;
    if catalog.visible_count() == 0 {
        println!("目录为空。使用 `drive-sync catalog add --path <文件>` 添加条目。");
        return Ok(());
    }
    println!("目录条目（revision {}）:", revision);
    println!(
        "{:<20} {:<24} {:<10} {:<8} {:>12}  {:<4} 更新时间",
        "ID", "名称", "版本", "平台", "大小", "本地"
    );
    for entry in catalog.visible() {
        println!(
            "{:<20} {:<24} {:<10} {:<8} {:>12}  {:<4} {}",
            entry.id,
            truncate_chars(&entry.name, 24),
            truncate_chars(
                if entry.version.is_empty() {
                    "-"
                } else {
                    &entry.version
                },
                10
            ),
            entry.platform.as_str(),
            human_bytes(entry.size),
            if index.contains_key(&entry.id) {
                "是"
            } else {
                "-"
            },
            format_ms(entry.updated_at_ms)
        );
    }
    println!();
    println!(
        "共 {} 条可见条目，本机持有 {} 个软件本体。",
        catalog.visible_count(),
        index.len()
    );
    Ok(())
}

fn remove(app: &App, id: &str) -> Result<()> {
    let store = app.store()?;
    let identity = store.load_identity()?;
    let node = identity.device_id();
    let (mut catalog, mut revision) = store.load_catalog()?;
    match catalog.delete_local(id, &node, now_ms()) {
        Some(_op) => {
            revision = revision.saturating_add(1);
            let mut index = store.load_local_index()?;
            index.remove(id);
            store.save_catalog(&catalog, revision)?;
            store.save_local_index(&index)?;
            println!("已删除目录条目（写墓碑）: {id}");
            println!("提示: 本机文件不会被删除；墓碑会同步给其他设备避免条目复活。");
            Ok(())
        }
        None => bail!("目录条目不存在或已删除: {id}"),
    }
}

async fn fetch(app: &App, entry_id: &str, out: Option<PathBuf>) -> Result<()> {
    let store = app.store()?;
    let config = app.runtime_config(&store)?;
    println!("正在连接中继并等待持有该条目的设备响应…");
    let path =
        drive_client::ops::catalog_fetch(&config, entry_id, out, Duration::from_secs(180)).await?;
    println!("下载完成: {}", path.display());
    println!("文件已记录到本地索引（daemon 在线时会通过 have_list 广播）。");
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

/// 供 e2e/调试：构造 Add 参数（与 CLI 行为一致）。
#[allow(dead_code)]
pub fn build_add_params(path: PathBuf) -> CatalogAddParams {
    let mut params = CatalogAddParams::new(path);
    params.kind = "file".to_string();
    params
}
