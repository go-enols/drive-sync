//! drive-sync 自动更新库：Ed25519 签名清单、下载、BLAKE3 校验与安全替换二进制。
//!
//! # 安全模型
//!
//! - **清单签名**：清单以 canonical JSON（对象 key 递归排序、去掉 `signature` 字段）用
//!   Ed25519 签名，公钥编译进客户端（`DRIVE_UPDATE_PUBKEY` 环境变量或本文件 fallback）；
//! - **产物校验**：下载后先按清单里的 `size` + `blake3` 校验，验签/校验任一失败都拒绝安装；
//! - **安装**：只替换当前可执行文件本身，不动其它文件。Windows 先 rename 正在运行的
//!   `.exe` 为 `.old-*` 再写入新文件（失败回滚）；Unix 用同目录临时文件 + `rename`
//!   （运行中的进程不受影响，重启后生效），保留 0755；
//! - **未签名清单默认拒绝**：只有显式 `--allow-unsigned` 才继续（用于本地开发/CI 无
//!   secret 时的自测，**不应在生产使用**）。
//!
//! 本 crate 是同步（blocking）实现，CLI/daemon 自行决定是否放到 blocking 线程池执行。
//!
//! # 最小用法
//!
//! ```
//! use drive_updater::{select_asset, verify_manifest, DEFAULT_UPDATE_PUBKEY_HEX};
//!
//! # fn demo(manifest_json: &str) -> Result<(), Box<dyn std::error::Error>> {
//! let manifest = verify_manifest(manifest_json, DEFAULT_UPDATE_PUBKEY_HEX)?;
//! if let Some(asset) = select_asset(&manifest, "x86_64-pc-windows-msvc") {
//!     println!("下载 {}", asset.url);
//! }
//! # Ok(())
//! # }
//! ```
#![forbid(unsafe_code)]

pub mod download;
pub mod install;
pub mod manifest;
pub mod version;

use std::path::PathBuf;

pub use download::{download_to_file, fetch_text, hash_file, verify_file, DownloadReport};
pub use install::{install_binary, install_binary_with_tag, InstallReport};
pub use manifest::{
    current_target, parse_manifest_unverified, select_asset, sign_manifest, verify_manifest,
    UpdateAsset, UpdateManifest, DEFAULT_PRODUCT, MANIFEST_SCHEMA, MAX_MANIFEST_BYTES,
};
pub use version::{compare_versions, parse_version, Version, VersionError};

/// 默认清单地址：编译期 `DRIVE_UPDATE_URL` 优先，否则指向 GitHub Releases 的 `latest.json`。
pub const DEFAULT_UPDATE_URL: &str = match option_env!("DRIVE_UPDATE_URL") {
    Some(url) => url,
    None => "https://github.com/go-enols/drive-sync/releases/latest/download/latest.json",
};

/// 默认清单验签公钥（64 字符 hex，Ed25519）。
///
/// 编译期 `DRIVE_UPDATE_PUBKEY` 优先（CI 发布构建使用），否则使用仓库内固定的测试/发布
/// 公钥 fallback。私钥（种子）只存在于 CI Secret `UPDATE_SIGNING_KEY`，绝不入库。
pub const DEFAULT_UPDATE_PUBKEY_HEX: &str = match option_env!("DRIVE_UPDATE_PUBKEY") {
    Some(key) => key,
    None => "99e9923bd30dea30a7d9b419016c714629729649083622ed4dc95051e1f7e9b5",
};

/// 单次更新产物下载上限（512 MiB）：防御恶意清单/服务器拖垮磁盘。
pub const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// 清单 JSON 的大小上限（256 KiB）：清单很小，超过即视为异常。
pub const MAX_MANIFEST_FETCH_BYTES: u64 = MAX_MANIFEST_BYTES;

/// 统一错误类型。
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("JSON 编解码失败: {0}")]
    Json(#[from] serde_json::Error),

    #[error("IO 失败: {0}")]
    Io(#[from] std::io::Error),

    #[error("网络请求失败: {0}")]
    Http(String),

    #[error("清单格式非法: {0}")]
    InvalidManifest(String),

    #[error("清单未签名（默认拒绝；仅本地测试可显式 --allow-unsigned）")]
    UnsignedManifest,

    #[error("清单签名校验失败: {0}")]
    BadSignature(String),

    #[error("公钥非法: {0}")]
    BadPubkey(String),

    #[error("签名编码非法: {0}")]
    BadSignatureEncoding(String),

    #[error("版本号非法: {0}")]
    Version(String),

    #[error("下载超过大小上限 {max} 字节，已中止")]
    TooLarge { max: u64 },

    #[error("下载内容为空")]
    EmptyDownload,

    #[error("文件大小不符: 期望 {expected} 字节，实际 {actual} 字节")]
    SizeMismatch { expected: u64, actual: u64 },

    #[error("BLAKE3 校验失败: 期望 {expected}，实际 {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("无法识别当前平台 triple，自动更新不支持该平台")]
    UnsupportedTarget,

    #[error("清单中没有适用于 {target} 的产物（可用: {available}）")]
    NoAssetForTarget { target: String, available: String },

    #[error("安装失败: {0}")]
    Install(String),

    #[error("密钥非法: {0}")]
    BadKey(String),

    #[error("参数错误: {0}")]
    Args(String),
}

pub type Result<T> = std::result::Result<T, UpdateError>;

/// 更新安装后用于“重启生效”的提示文案。
pub fn restart_hint(version: &str) -> String {
    format!("已更新到 v{version}，重启后生效")
}

/// 更新状态文件路径约定：`<data_dir>/update-state.json`。
pub fn update_state_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("update-state.json")
}

/// 更新下载缓存目录约定：`<data_dir>/updates/`。
pub fn update_cache_dir(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("updates")
}
