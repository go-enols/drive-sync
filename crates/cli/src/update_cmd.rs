//! `update` 子命令与 daemon 自动更新：检查清单、验签、下载、校验、安装。
//!
//! 默认值来源（优先级从高到低）：
//! - 清单地址：`--url` → 环境变量 `DRIVE_UPDATE_URL` → `config.json` 的
//!   `update_manifest_url` → 编译期 `option_env!("DRIVE_UPDATE_URL")`（本仓库默认指向
//!   GitHub Releases 的 `latest.json`）；
//! - 验签公钥：`config.json` 的 `update_pubkey` → 环境变量 `DRIVE_UPDATE_PUBKEY` →
//!   编译期 `option_env!("DRIVE_UPDATE_PUBKEY")` → 源码内 fallback（正式发布公钥）。
//!
//! 安全默认：清单必须通过 Ed25519 验签；`--allow-unsigned` 只为本地开发/无 secret 的
//! 自测保留，会在 stderr 打印醒目警告。

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use drive_updater::{
    current_target, download_to_file, fetch_text, install_binary, parse_manifest_unverified,
    select_asset, update_cache_dir, update_state_path, verify_file, verify_manifest,
    UpdateManifest, DEFAULT_UPDATE_PUBKEY_HEX, DEFAULT_UPDATE_URL, MAX_DOWNLOAD_BYTES,
    MAX_MANIFEST_FETCH_BYTES,
};
use serde::{Deserialize, Serialize};

use crate::common::{format_ms, human_bytes, App};

/// 当前客户端版本（编译期）。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// daemon 自动更新检查间隔：6 小时。
pub const AUTO_UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Subcommand)]
pub enum UpdateCommand {
    /// 检查是否有新版本（只下载小清单，不下载/安装产物）。
    Check {
        /// 更新清单地址（覆盖编译期默认值 / config.json）
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// 输出 JSON（便于脚本解析）
        #[arg(long)]
        json: bool,
    },
    /// 下载、验签、BLAKE3 校验并安装新版本（重启后生效）。
    Apply {
        /// 更新清单地址（覆盖编译期默认值 / config.json）
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// 跳过交互确认（脚本/服务使用）
        #[arg(long)]
        yes: bool,
        /// 允许未签名清单（仅本地测试；生产请配置公钥/Secret）
        #[arg(long)]
        allow_unsigned: bool,
        /// 已是最新版本时也强制重新下载安装
        #[arg(long)]
        force: bool,
        /// 输出 JSON（便于脚本解析）
        #[arg(long)]
        json: bool,
    },
}

/// `drive-sync update ...` 入口（同步阻塞实现，命令本身很快/一次性）。
pub fn run(app: &App, command: UpdateCommand) -> Result<()> {
    match command {
        UpdateCommand::Check { url, json } => cmd_check(app, url, json),
        UpdateCommand::Apply {
            url,
            yes,
            allow_unsigned,
            force,
            json,
        } => cmd_apply(app, url, yes, allow_unsigned, force, json),
    }
}

// ---------------------------------------------------------------------------
// 配置解析
// ---------------------------------------------------------------------------

#[derive(Default)]
struct UpdateSources {
    url: Option<String>,
    pubkey: Option<String>,
}

fn resolve_sources(app: &App, url_override: Option<String>) -> Result<UpdateSources> {
    let config = read_config_overrides(app);
    let url = url_override
        .or_else(|| env_non_empty("DRIVE_UPDATE_URL"))
        .or(config.url)
        .or_else(|| {
            let default = DEFAULT_UPDATE_URL.trim();
            (!default.is_empty()).then(|| default.to_string())
        });
    let pubkey = config
        .pubkey
        .or_else(|| env_non_empty("DRIVE_UPDATE_PUBKEY"))
        .or_else(|| {
            let default = DEFAULT_UPDATE_PUBKEY_HEX.trim();
            (!default.is_empty()).then(|| default.to_string())
        });
    Ok(UpdateSources { url, pubkey })
}

/// 从 `config.json` 原始 JSON 里读更新相关覆盖项。
///
/// 注意：`ClientConfig` 目前不包含这两个字段（本 crate 只做最小 CLI 集成），因此这里
/// 直接按 JSON 读；写入 config.json 的这两个键是非破坏性的（客户端加载时忽略未知字段）。
fn read_config_overrides(app: &App) -> UpdateSources {
    let path = app.data_dir.join("config.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return UpdateSources::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return UpdateSources::default();
    };
    let get = |key: &str| {
        value
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    UpdateSources {
        url: get("update_manifest_url"),
        pubkey: get("update_pubkey"),
    }
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn require_url(sources: &UpdateSources) -> Result<String> {
    sources.url.clone().ok_or_else(|| {
        anyhow!(
            "未配置更新清单地址：请用 --url 指定，或设置环境变量 DRIVE_UPDATE_URL / \
             config.json 的 update_manifest_url（默认值应由发布构建编译进 DRIVE_UPDATE_URL）"
        )
    })
}

fn require_pubkey(sources: &UpdateSources) -> Result<String> {
    let pubkey = sources.pubkey.clone().ok_or_else(|| {
        anyhow!(
            "未配置更新验签公钥：请设置 config.json 的 update_pubkey 或环境变量 \
             DRIVE_UPDATE_PUBKEY（正式客户端在编译期内置发布公钥）"
        )
    })?;
    let trimmed = pubkey.trim();
    if trimmed.len() != 64 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("更新公钥格式非法：应为 64 字符 hex（32 字节 Ed25519 公钥）");
    }
    Ok(trimmed.to_string())
}

/// 拉取清单：优先验签；`allow_unsigned` 下验签失败时降级为“结构合法但未签名”。
fn load_manifest(
    text: &str,
    pubkey: Option<&str>,
    allow_unsigned: bool,
) -> Result<(UpdateManifest, bool)> {
    if allow_unsigned {
        if let Some(pubkey) = pubkey {
            if let Ok(manifest) = verify_manifest(text, pubkey) {
                return Ok((manifest, true));
            }
        }
        let manifest = parse_manifest_unverified(text).context("清单结构非法")?;
        return Ok((manifest, false));
    }
    let pubkey = pubkey.ok_or_else(|| {
        anyhow!("未配置更新验签公钥，无法校验清单签名（本地测试可加 --allow-unsigned）")
    })?;
    let manifest = verify_manifest(text, pubkey).map_err(|error| {
        anyhow!("清单验签失败: {error}\n提示: 正式版本必须使用官方签名的清单；本地自测可加 --allow-unsigned")
    })?;
    Ok((manifest, true))
}

fn fetch_manifest(url: &str) -> Result<String> {
    fetch_text(url, MAX_MANIFEST_FETCH_BYTES).with_context(|| format!("下载更新清单失败: {url}"))
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

fn cmd_check(app: &App, url_override: Option<String>, json: bool) -> Result<()> {
    let sources = resolve_sources(app, url_override)?;
    let url = require_url(&sources)?;
    let pubkey = require_pubkey(&sources)?;
    let text = fetch_manifest(&url)?;
    let (manifest, signed) = load_manifest(&text, Some(&pubkey), false)?;
    if !signed {
        bail!("清单未签名，拒绝检查结果（生产客户端必须验签）");
    }
    let target = current_target();
    let newer = manifest.is_newer_than(CURRENT_VERSION)?;
    let asset = select_asset(&manifest, target);

    if json {
        let value = serde_json::json!({
            "current_version": CURRENT_VERSION,
            "target": target,
            "manifest_url": url,
            "signed": true,
            "latest": {
                "version": manifest.version,
                "published_at_ms": manifest.published_at_ms,
                "notes": manifest.notes,
                "assets": manifest.assets,
            },
            "update_available": newer,
            "asset": asset,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    println!("当前版本: v{CURRENT_VERSION}（{target}）");
    println!(
        "清单版本: v{}（发布于 {}，签名有效）",
        manifest.version,
        format_ms(manifest.published_at_ms)
    );
    if newer {
        println!("发现新版本: v{}", manifest.version);
    } else {
        println!("已是最新版本。");
    }
    match asset {
        Some(asset) => {
            println!(
                "产物: {}（{}，BLAKE3 {}）",
                asset.url,
                human_bytes(asset.size),
                &asset.blake3[..asset.blake3.len().min(16)]
            );
        }
        None => {
            println!(
                "提示: 清单没有当前平台 {target} 的产物（可用: {}）。",
                if manifest.assets.is_empty() {
                    "-".to_string()
                } else {
                    manifest.asset_targets().join(", ")
                }
            );
        }
    }
    if !manifest.notes.trim().is_empty() {
        println!("更新说明:\n{}", manifest.notes.trim());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// apply
// ---------------------------------------------------------------------------

fn cmd_apply(
    app: &App,
    url_override: Option<String>,
    yes: bool,
    allow_unsigned: bool,
    force: bool,
    json: bool,
) -> Result<()> {
    let sources = resolve_sources(app, url_override)?;
    let url = require_url(&sources)?;
    // allow_unsigned 时公钥可以缺失；否则必须有。
    let pubkey = if allow_unsigned {
        sources.pubkey.clone()
    } else {
        Some(require_pubkey(&sources)?)
    };
    let text = fetch_manifest(&url)?;
    let (manifest, signed) = load_manifest(&text, pubkey.as_deref(), allow_unsigned)?;
    if !signed {
        eprintln!("警告: 清单未签名（--allow-unsigned），仅适用于本地测试，切勿用于生产！");
    }

    let target = current_target();
    if target == "unknown" {
        bail!(
            "当前平台（{}-{}）不支持自动更新",
            std::env::consts::ARCH,
            std::env::consts::OS
        );
    }
    let newer = manifest.is_newer_than(CURRENT_VERSION)?;
    if !newer && !force {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "current_version": CURRENT_VERSION,
                    "target": target,
                    "update_available": false,
                    "installed": false,
                }))?
            );
        } else {
            println!(
                "已是最新版本 v{CURRENT_VERSION}（清单 v{}），无需更新。",
                manifest.version
            );
        }
        return Ok(());
    }
    let asset = select_asset(&manifest, target).cloned().ok_or_else(|| {
        anyhow!(
            "清单中没有适用于 {target} 的产物（可用: {}）",
            manifest.asset_targets().join(", ")
        )
    })?;

    if !yes {
        confirm_install(&manifest, &asset, target)?;
    }

    let cache_dir = update_cache_dir(&app.data_dir);
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("创建更新缓存目录失败: {}", cache_dir.display()))?;
    let file_name = asset_file_name(&manifest.version, target, &asset.url);
    let downloaded = cache_dir.join(&file_name);
    eprintln!(
        "正在下载 v{}（{}）…",
        manifest.version,
        human_bytes(asset.size)
    );
    let report = download_to_file(&asset.url, &downloaded, MAX_DOWNLOAD_BYTES)?;
    verify_file(&downloaded, &asset.blake3, asset.size).with_context(|| {
        format!(
            "产物校验失败（{} 字节，BLAKE3 不符）：已中止安装，缓存文件 {}",
            report.bytes,
            downloaded.display()
        )
    })?;
    eprintln!("下载完成并校验通过: {}", downloaded.display());

    let current_exe = std::env::current_exe().context("无法定位当前可执行文件")?;
    let install = install_binary(&downloaded, &current_exe)
        .with_context(|| format!("替换可执行文件失败（当前 exe: {}）", current_exe.display()))?;

    let mut state = load_state(&app.data_dir);
    state.last_checked_ms = Some(drive_core::util::now_ms());
    state.last_installed_version = Some(manifest.version.clone());
    state.last_installed_at_ms = Some(drive_core::util::now_ms());
    state.last_installed_target = Some(target.to_string());
    state.last_backup = install
        .backup
        .as_ref()
        .map(|path| path.display().to_string());
    state.last_error = None;
    save_state(&app.data_dir, &state)?;
    // 安装成功后清掉缓存里的安装包，避免占用磁盘（清单本身不缓存）。
    let _ = std::fs::remove_file(&downloaded);

    let hint = drive_updater::restart_hint(&manifest.version);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "current_version": CURRENT_VERSION,
                "target": target,
                "update_available": true,
                "installed": true,
                "installed_version": manifest.version,
                "signed": signed,
                "target_exe": install.target_exe,
                "backup": install.backup,
                "restart_required": true,
                "message": hint,
            }))?
        );
    } else {
        println!("{hint}");
        println!("  新版本: v{}", manifest.version);
        println!("  当前 exe: {}", install.target_exe.display());
        println!("  旧版本备份: {}", install.backup_display());
        println!("  提示: 直接重新运行 `drive-sync` 即可使用新版本。");
    }
    Ok(())
}

fn confirm_install(
    manifest: &UpdateManifest,
    asset: &drive_updater::UpdateAsset,
    target: &str,
) -> Result<()> {
    println!("将安装 v{}（{target}）", manifest.version);
    println!("  产物: {}", asset.url);
    println!("  大小: {}", human_bytes(asset.size));
    println!("  BLAKE3: {}", asset.blake3);
    if !manifest.notes.trim().is_empty() {
        println!("  更新说明:\n{}", manifest.notes.trim());
    }
    if !std::io::stdin().is_terminal() {
        bail!("非交互环境（stdin 不是终端）：请加 --yes 确认安装");
    }
    eprint!("确认下载并安装？[y/N] ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line)?;
    if read == 0 || !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        bail!("已取消");
    }
    Ok(())
}

fn asset_file_name(version: &str, target: &str, url: &str) -> String {
    let fallback = || format!("drive-sync-v{version}-{target}");
    let Some(name) = url
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty() && !name.contains('?'))
    else {
        return fallback();
    };
    let cleaned = sanitize_file_name(name);
    // 防御：URL 末段是 "." / ".." 时不能作为缓存文件名（会逃出缓存目录）。
    if cleaned == "." || cleaned == ".." || cleaned.is_empty() {
        fallback()
    } else {
        cleaned
    }
}

fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "drive-sync-update.bin".to_string()
    } else {
        cleaned
    }
}

// ---------------------------------------------------------------------------
// daemon 自动更新
// ---------------------------------------------------------------------------

/// 一次自动更新检查的结果。
#[derive(Clone, Debug, Default)]
pub struct AutoUpdateReport {
    /// 是否有新版本被安装。
    pub installed: Option<String>,
    /// 跳过原因（已是最新/无当前平台产物等）。
    pub skipped: Option<String>,
}

/// 执行一次自动更新：检查 → 验签 → 下载 → 校验 → 安装。
///
/// 与 CLI `update apply` 的区别：永不接受未签名清单、不交互、不强制重装同版本，
/// 并用 `<data_dir>/update-state.json` 记住已安装版本，避免 daemon 每轮重复安装。
pub fn auto_update_once(app: &App) -> Result<AutoUpdateReport> {
    let sources = resolve_sources(app, None)?;
    let url = require_url(&sources)?;
    let pubkey = require_pubkey(&sources)?;
    let text = fetch_manifest(&url)?;
    let (manifest, signed) = load_manifest(&text, Some(&pubkey), false)?;
    if !signed {
        bail!("拒绝安装未签名清单");
    }
    let target = current_target();
    let asset = match select_asset(&manifest, target) {
        Some(asset) => asset.clone(),
        None => {
            return Ok(AutoUpdateReport {
                installed: None,
                skipped: Some(format!("清单里没有 {target} 的产物")),
            })
        }
    };

    let mut state = load_state(&app.data_dir);
    let installed_base = state
        .last_installed_version
        .clone()
        .unwrap_or_else(|| CURRENT_VERSION.to_string());
    if !manifest.is_newer_than(&installed_base)? {
        state.last_checked_ms = Some(drive_core::util::now_ms());
        save_state(&app.data_dir, &state)?;
        return Ok(AutoUpdateReport {
            installed: None,
            skipped: Some(format!(
                "已是最新版本（当前 v{CURRENT_VERSION}，清单 v{}）",
                manifest.version
            )),
        });
    }

    let cache_dir = update_cache_dir(&app.data_dir);
    std::fs::create_dir_all(&cache_dir)?;
    let downloaded = cache_dir.join(asset_file_name(&manifest.version, target, &asset.url));
    let report = download_to_file(&asset.url, &downloaded, MAX_DOWNLOAD_BYTES)?;
    if let Err(error) = verify_file(&downloaded, &asset.blake3, asset.size) {
        let _ = std::fs::remove_file(&downloaded);
        state.last_error = Some(format!("产物校验失败: {error}"));
        save_state(&app.data_dir, &state)?;
        bail!("产物校验失败: {error}（已中止安装）");
    }
    let current_exe = std::env::current_exe().context("无法定位当前可执行文件")?;
    let install = install_binary(&downloaded, &current_exe)?;
    let _ = std::fs::remove_file(&downloaded);

    state.last_checked_ms = Some(drive_core::util::now_ms());
    state.last_installed_version = Some(manifest.version.clone());
    state.last_installed_at_ms = Some(drive_core::util::now_ms());
    state.last_installed_target = Some(target.to_string());
    state.last_backup = install
        .backup
        .as_ref()
        .map(|path| path.display().to_string());
    state.last_error = None;
    save_state(&app.data_dir, &state)?;
    eprintln!(
        "自动更新: 已下载并安装 v{}（{} 字节，备份 {}）",
        manifest.version,
        report.bytes,
        install.backup_display()
    );
    Ok(AutoUpdateReport {
        installed: Some(manifest.version),
        skipped: None,
    })
}

/// daemon 的后台自动更新循环：启动时立即检查一次，之后每 6 小时检查。
///
/// `restart` 为真时，安装成功后自动拉起新进程并退出当前进程。
pub async fn auto_update_loop(app: App, restart: bool, interval: Duration) {
    let app = Arc::new(app);
    loop {
        let app_for_task = app.clone();
        let result = tokio::task::spawn_blocking(move || auto_update_once(&app_for_task)).await;
        match result {
            Ok(Ok(report)) => {
                if let Some(version) = report.installed {
                    println!(
                        "[{}] {}",
                        crate::common::format_now(),
                        drive_updater::restart_hint(&version)
                    );
                    if restart {
                        spawn_current_and_exit();
                    }
                } else if let Some(reason) = report.skipped {
                    tracing::debug!(reason = %reason, "自动更新检查完成");
                }
            }
            Ok(Err(error)) => {
                eprintln!(
                    "[{}] 自动更新检查失败: {error:#}",
                    crate::common::format_now()
                );
            }
            Err(join_error) => {
                eprintln!(
                    "[{}] 自动更新任务异常: {join_error}",
                    crate::common::format_now()
                );
            }
        }
        tokio::time::sleep(interval).await;
    }
}

/// 用当前命令行参数拉起新进程，然后退出当前进程（`--update-restart`）。
pub fn spawn_current_and_exit() -> ! {
    let exit_code = match std::env::current_exe() {
        Ok(exe) => {
            let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
            match std::process::Command::new(&exe).args(&args).spawn() {
                Ok(_) => {
                    println!("已拉起新进程（{}），当前进程退出。", exe.display());
                    0
                }
                Err(error) => {
                    eprintln!("拉起新进程失败: {error}；请手动重启 drive-sync。");
                    1
                }
            }
        }
        Err(error) => {
            eprintln!("无法定位当前可执行文件: {error}；请手动重启 drive-sync。");
            1
        }
    };
    // 给 stdout/stderr 一点刷新时间（println 已按行缓冲）。
    std::process::exit(exit_code);
}

// ---------------------------------------------------------------------------
// update-state.json
// ---------------------------------------------------------------------------

/// `<data_dir>/update-state.json`：记录最近一次检查/安装，供 daemon 去重。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UpdateState {
    #[serde(default)]
    pub last_checked_ms: Option<u64>,
    #[serde(default)]
    pub last_installed_version: Option<String>,
    #[serde(default)]
    pub last_installed_at_ms: Option<u64>,
    #[serde(default)]
    pub last_installed_target: Option<String>,
    #[serde(default)]
    pub last_backup: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
}

fn load_state(data_dir: &std::path::Path) -> UpdateState {
    let path = update_state_path(data_dir);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_state(data_dir: &std::path::Path, state: &UpdateState) -> Result<()> {
    let path = update_state_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(state)?;
    std::fs::write(&path, format!("{text}\n"))
        .with_context(|| format!("写入更新状态失败: {}", path.display()))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use drive_updater::{sign_manifest, UpdateAsset, UpdateManifest};

    fn manifest_json(signed: bool, version: &str) -> (String, String) {
        let manifest = UpdateManifest::new(
            version,
            1_760_000_000_000,
            "测试",
            vec![UpdateAsset {
                target: current_target().to_string(),
                url: "file:///tmp/drive-sync.exe".into(),
                size: 10,
                blake3: "a".repeat(64),
            }],
        )
        .unwrap();
        if signed {
            let (pubkey, seed) = drive_updater::manifest::keygen();
            let signed = sign_manifest(&manifest, &seed).unwrap();
            (serde_json::to_string(&signed).unwrap(), pubkey)
        } else {
            (serde_json::to_string(&manifest).unwrap(), String::new())
        }
    }

    #[test]
    fn signed_manifest_loads_strictly() {
        let (json, pubkey) = manifest_json(true, "0.3.0");
        let (manifest, signed) = load_manifest(&json, Some(&pubkey), false).unwrap();
        assert!(signed);
        assert_eq!(manifest.version, "0.3.0");
        assert!(manifest.is_newer_than("0.2.0").unwrap());
    }

    #[test]
    fn unsigned_manifest_rejected_unless_allowed() {
        let (json, _pubkey) = manifest_json(false, "0.3.0");
        assert!(load_manifest(&json, None, false).is_err());
        let (manifest, signed) = load_manifest(&json, None, true).unwrap();
        assert!(!signed);
        assert_eq!(manifest.version, "0.3.0");
    }

    #[test]
    fn tampered_manifest_rejected() {
        let (json, pubkey) = manifest_json(true, "0.3.0");
        let tampered = json.replace("0.3.0", "9.9.9");
        assert!(load_manifest(&tampered, Some(&pubkey), false).is_err());
        // 即使 allow_unsigned 也不应接受坏签名清单（会降级为 unsigned，但仍然提示）。
        let (manifest, signed) = load_manifest(&tampered, Some(&pubkey), true).unwrap();
        assert!(!signed);
        assert_eq!(manifest.version, "9.9.9");
    }

    #[test]
    fn asset_file_name_is_sanitized() {
        assert_eq!(
            asset_file_name(
                "0.3.0",
                "x86_64-pc-windows-msvc",
                "https://x/y/drive-sync.exe"
            ),
            "drive-sync.exe"
        );
        assert_eq!(
            asset_file_name("0.3.0", "x86_64-unknown-linux-gnu", "https://x/y/"),
            "drive-sync-v0.3.0-x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            asset_file_name("0.3.0", "t", "https://x/evil name?.exe"),
            "drive-sync-v0.3.0-t"
        );
        assert_eq!(
            asset_file_name("0.3.0", "t", "https://x/.."),
            "drive-sync-v0.3.0-t"
        );
    }

    #[test]
    fn state_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = load_state(dir.path());
        assert!(state.last_installed_version.is_none());
        state.last_installed_version = Some("0.3.0".into());
        state.last_installed_target = Some("x86_64-pc-windows-msvc".into());
        save_state(dir.path(), &state).unwrap();
        let loaded = load_state(dir.path());
        assert_eq!(loaded.last_installed_version.as_deref(), Some("0.3.0"));
        assert_eq!(
            loaded.last_installed_target.as_deref(),
            Some("x86_64-pc-windows-msvc")
        );
    }

    #[test]
    fn invalid_pubkey_is_reported() {
        let sources = UpdateSources {
            url: Some("https://example.invalid/latest.json".into()),
            pubkey: Some("zz".into()),
        };
        assert!(require_pubkey(&sources).is_err());
        // 未配置 --url / config / env 时回退到编译期默认值（发布构建一定有）。
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(Some(dir.path().to_path_buf()), None, None);
        let resolved = resolve_sources(&app, None).unwrap();
        assert!(resolved.url.is_some());
        assert!(resolved.pubkey.is_some());
    }
}
