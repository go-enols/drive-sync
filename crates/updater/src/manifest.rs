//! 更新清单（`latest.json`）：数据结构、规范化签名/验签、产物选择。

use std::cmp::Ordering;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::version::Version;
use crate::{Result, UpdateError};

/// 清单 schema 版本：不兼容变更时递增。
pub const MANIFEST_SCHEMA: u32 = 1;
/// 产品标识（防止把别的项目的清单喂给客户端）。
pub const DEFAULT_PRODUCT: &str = "drive-sync";
/// 清单 JSON 大小上限：清单本身很小，超过即视为异常。
pub const MAX_MANIFEST_BYTES: u64 = 256 * 1024;

/// 单个平台产物。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateAsset {
    /// Rust target triple，例如 `x86_64-pc-windows-msvc`。
    pub target: String,
    /// 产物下载地址（一般指向 GitHub Release 里的**裸可执行文件**，而非压缩包）。
    pub url: String,
    /// 文件字节数。
    pub size: u64,
    /// 文件 BLAKE3（64 字符 hex）。
    pub blake3: String,
}

/// 更新清单。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateManifest {
    pub schema: u32,
    pub product: String,
    pub version: String,
    pub published_at_ms: u64,
    #[serde(default)]
    pub notes: String,
    pub assets: Vec<UpdateAsset>,
    /// Ed25519 签名（128 字符 hex = 64 字节）；未签名清单为 `None`/`null`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl UpdateManifest {
    /// 构造一个未签名清单（发布脚本用）。
    pub fn new(
        version: impl Into<String>,
        published_at_ms: u64,
        notes: impl Into<String>,
        assets: Vec<UpdateAsset>,
    ) -> Result<Self> {
        let manifest = Self {
            schema: MANIFEST_SCHEMA,
            product: DEFAULT_PRODUCT.to_string(),
            version: version.into(),
            published_at_ms,
            notes: notes.into(),
            assets,
            signature: None,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// 需要签名的字节串：canonical JSON，且**去掉整个 `signature` 字段**。
    ///
    /// 客户端验签时用同样的规则重算，因此签名与 JSON 排版/字段顺序无关。
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let mut unsigned = self.clone();
        unsigned.signature = None;
        let mut value = serde_json::to_value(&unsigned)?;
        let serde_json::Value::Object(map) = &mut value else {
            return Err(UpdateError::InvalidManifest("清单不是 JSON 对象".into()));
        };
        map.remove("signature");
        let canonical = drive_core::util::canonical_json(&value).map_err(|error| {
            UpdateError::InvalidManifest(format!("canonical JSON 失败: {error}"))
        })?;
        Ok(canonical.into_bytes())
    }

    /// 结构合法性校验（不含验签）。验签前/后都应调用。
    pub fn validate(&self) -> Result<()> {
        if self.schema != MANIFEST_SCHEMA {
            return Err(UpdateError::InvalidManifest(format!(
                "不支持的清单 schema {}（期望 {MANIFEST_SCHEMA}）",
                self.schema
            )));
        }
        if self.product != DEFAULT_PRODUCT {
            return Err(UpdateError::InvalidManifest(format!(
                "清单 product 不是 {DEFAULT_PRODUCT}: {:?}",
                self.product
            )));
        }
        Version::parse(&self.version)
            .map_err(|error| UpdateError::InvalidManifest(format!("清单 version 非法: {error}")))?;
        if self.assets.is_empty() {
            return Err(UpdateError::InvalidManifest("清单 assets 为空".into()));
        }
        let mut seen = std::collections::BTreeSet::new();
        for asset in &self.assets {
            asset.validate()?;
            if !seen.insert(asset.target.as_str()) {
                return Err(UpdateError::InvalidManifest(format!(
                    "清单里 target 重复: {}",
                    asset.target
                )));
            }
        }
        Ok(())
    }

    /// 选择精确匹配 target 的产物。
    pub fn find_asset(&self, target: &str) -> Option<&UpdateAsset> {
        self.assets.iter().find(|asset| asset.target == target)
    }

    /// [`find_asset`](Self::find_asset) 的别名，语义与自由函数 [`select_asset`] 一致。
    pub fn select_asset(&self, target: &str) -> Option<&UpdateAsset> {
        self.find_asset(target)
    }

    /// 全部可用 target（错误提示用）。
    pub fn asset_targets(&self) -> Vec<String> {
        self.assets
            .iter()
            .map(|asset| asset.target.clone())
            .collect()
    }

    /// 清单版本是否比 `current_version` 新。
    pub fn is_newer_than(&self, current_version: &str) -> Result<bool> {
        let latest = Version::parse(&self.version)?;
        let current = Version::parse(current_version)?;
        Ok(latest.cmp(&current) == Ordering::Greater)
    }

    /// 是否已签名（只检查字段存在且格式像签名，权威判断仍以验签结果为准）。
    pub fn has_signature(&self) -> bool {
        self.signature
            .as_deref()
            .is_some_and(|sig| !sig.trim().is_empty())
    }
}

impl UpdateAsset {
    /// 单个产物字段校验（target/url/size/blake3）。
    pub fn validate(&self) -> Result<()> {
        if self.target.trim().is_empty()
            || self.target.contains('/')
            || self.target.contains('\\')
            || self.target.contains(char::is_whitespace)
        {
            return Err(UpdateError::InvalidManifest(format!(
                "产物 target 非法: {:?}",
                self.target
            )));
        }
        let url = self.url.trim();
        if url.is_empty() {
            return Err(UpdateError::InvalidManifest(format!(
                "产物 {} 的 url 为空",
                self.target
            )));
        }
        const ALLOWED_SCHEMES: [&str; 3] = ["https://", "http://", "file://"];
        if !ALLOWED_SCHEMES.iter().any(|scheme| url.starts_with(scheme)) {
            return Err(UpdateError::InvalidManifest(format!(
                "产物 {} 的 url 协议不受支持（仅 https/http/file）: {url}",
                self.target
            )));
        }
        if self.size == 0 {
            return Err(UpdateError::InvalidManifest(format!(
                "产物 {} 的 size 为 0",
                self.target
            )));
        }
        let hash = self.blake3.trim();
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(UpdateError::InvalidManifest(format!(
                "产物 {} 的 blake3 不是 64 字符 hex",
                self.target
            )));
        }
        Ok(())
    }
}

/// 解析并校验清单结构，**不验签**（供 `--allow-unsigned` 的本地测试路径使用）。
pub fn parse_manifest_unverified(manifest_json: &str) -> Result<UpdateManifest> {
    let manifest: UpdateManifest = serde_json::from_str(manifest_json)?;
    manifest.validate()?;
    Ok(manifest)
}

/// 解析 + 校验结构 + 用 Ed25519 公钥验签。
///
/// 公钥为 64 字符 hex（32 字节）；签名必须是 128 字符 hex（64 字节）。
/// 未签名清单返回 [`UpdateError::UnsignedManifest`]，绝不静默放行。
pub fn verify_manifest(manifest_json: &str, pubkey_hex: &str) -> Result<UpdateManifest> {
    let manifest: UpdateManifest = serde_json::from_str(manifest_json)?;
    manifest.validate()?;
    let signature_hex = manifest
        .signature
        .as_deref()
        .map(str::trim)
        .filter(|sig| !sig.is_empty())
        .ok_or(UpdateError::UnsignedManifest)?;
    let pubkey = decode_hex_fixed::<32>(pubkey_hex)
        .map_err(|error| UpdateError::BadPubkey(format!("{error}（期望 64 字符 hex）")))?;
    let verifying_key = VerifyingKey::from_bytes(&pubkey)
        .map_err(|error| UpdateError::BadPubkey(format!("Ed25519 公钥非法: {error}")))?;
    let signature = decode_hex_fixed::<64>(signature_hex).map_err(|error| {
        UpdateError::BadSignatureEncoding(format!("{error}（期望 128 字符 hex）"))
    })?;
    let signature = Signature::from_bytes(&signature);
    let message = manifest.canonical_bytes()?;
    verifying_key
        .verify_strict(&message, &signature)
        .map_err(|error| UpdateError::BadSignature(format!("清单签名无效: {error}")))?;
    Ok(manifest)
}

/// 用 Ed25519 种子（64 字符 hex）给清单签名，返回带 `signature` 的新清单。
pub fn sign_manifest(manifest: &UpdateManifest, seed_hex: &str) -> Result<UpdateManifest> {
    manifest.validate()?;
    let signing_key = signing_key_from_seed(seed_hex)?;
    let message = manifest.canonical_bytes()?;
    let signature = signing_key.sign(&message).to_bytes();
    let mut signed = manifest.clone();
    signed.signature = Some(hex::encode(signature));
    Ok(signed)
}

/// 由 64 字符 hex 种子派生 Ed25519 签名密钥。
pub fn signing_key_from_seed(seed_hex: &str) -> Result<SigningKey> {
    let seed = decode_hex_fixed::<32>(seed_hex).map_err(|error| {
        UpdateError::BadKey(format!(
            "UPDATE_SIGNING_KEY 非法: {error}（期望 64 字符 hex 种子）"
        ))
    })?;
    Ok(SigningKey::from_bytes(&seed))
}

/// 由种子导出公钥（64 字符 hex）。
pub fn pubkey_hex_from_seed(seed_hex: &str) -> Result<String> {
    Ok(hex::encode(
        signing_key_from_seed(seed_hex)?.verifying_key().to_bytes(),
    ))
}

/// 生成一对新的签名密钥：返回 `(公钥 hex, 种子 hex)`。
pub fn keygen() -> (String, String) {
    let seed = drive_core::util::random_bytes(32);
    let seed_hex = hex::encode(&seed);
    let seed_array: [u8; 32] = seed.try_into().expect("32 字节种子");
    let signing_key = SigningKey::from_bytes(&seed_array);
    let pubkey = hex::encode(signing_key.verifying_key().to_bytes());
    (pubkey, seed_hex)
}

/// 选择 `target` 对应的产物；没有时返回 `None`。
pub fn select_asset<'a>(manifest: &'a UpdateManifest, target: &str) -> Option<&'a UpdateAsset> {
    manifest.find_asset(target)
}

/// 当前编译目标对应的 Rust target triple。
///
/// 只覆盖发布流水线产出的平台；其它平台返回 `"unknown"`（调用方应报错而不是猜测）。
pub fn current_target() -> &'static str {
    #[cfg(all(target_arch = "x86_64", target_os = "windows", target_env = "msvc"))]
    const TARGET: &str = "x86_64-pc-windows-msvc";
    #[cfg(all(target_arch = "x86_64", target_os = "windows", target_env = "gnu"))]
    const TARGET: &str = "x86_64-pc-windows-gnu";
    #[cfg(all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"))]
    const TARGET: &str = "x86_64-unknown-linux-gnu";
    #[cfg(all(target_arch = "x86_64", target_os = "linux", target_env = "musl"))]
    const TARGET: &str = "x86_64-unknown-linux-musl";
    #[cfg(all(target_arch = "aarch64", target_os = "linux", target_env = "gnu"))]
    const TARGET: &str = "aarch64-unknown-linux-gnu";
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    const TARGET: &str = "aarch64-apple-darwin";
    #[cfg(all(target_arch = "x86_64", target_os = "macos"))]
    const TARGET: &str = "x86_64-apple-darwin";
    #[cfg(not(any(
        all(target_arch = "x86_64", target_os = "windows", target_env = "msvc"),
        all(target_arch = "x86_64", target_os = "windows", target_env = "gnu"),
        all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"),
        all(target_arch = "x86_64", target_os = "linux", target_env = "musl"),
        all(target_arch = "aarch64", target_os = "linux", target_env = "gnu"),
        all(target_arch = "aarch64", target_os = "macos"),
        all(target_arch = "x86_64", target_os = "macos"),
    )))]
    const TARGET: &str = "unknown";
    TARGET
}

/// 解析固定长度的 hex 字符串。
fn decode_hex_fixed<const N: usize>(text: &str) -> std::result::Result<[u8; N], String> {
    let bytes = hex::decode(text.trim()).map_err(|error| format!("hex 解码失败: {error}"))?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| format!("长度应为 {N} 字节，实际 {}", bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest(version: &str) -> UpdateManifest {
        UpdateManifest::new(
            version,
            1_760_000_000_000,
            "测试版本",
            vec![
                UpdateAsset {
                    target: "x86_64-pc-windows-msvc".into(),
                    url: "https://example.invalid/drive-sync.exe".into(),
                    size: 1024,
                    blake3: "a".repeat(64),
                },
                UpdateAsset {
                    target: "x86_64-unknown-linux-gnu".into(),
                    url: "https://example.invalid/drive-sync".into(),
                    size: 2048,
                    blake3: "b".repeat(64),
                },
            ],
        )
        .unwrap()
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let (pubkey, seed) = keygen();
        let manifest = sample_manifest("0.2.0");
        let signed = sign_manifest(&manifest, &seed).unwrap();
        assert!(signed.has_signature());

        let json = serde_json::to_string(&signed).unwrap();
        let verified = verify_manifest(&json, &pubkey).unwrap();
        assert_eq!(verified.version, "0.2.0");
        assert_eq!(verified.assets.len(), 2);
    }

    #[test]
    fn verify_accepts_reordered_and_pretty_printed_json() {
        let (pubkey, seed) = keygen();
        let signed = sign_manifest(&sample_manifest("0.2.0"), &seed).unwrap();
        let pretty = serde_json::to_string_pretty(&signed).unwrap();
        assert!(verify_manifest(&pretty, &pubkey).is_ok());
    }

    #[test]
    fn tampered_manifest_fails_verification() {
        let (pubkey, seed) = keygen();
        let signed = sign_manifest(&sample_manifest("0.2.0"), &seed).unwrap();

        let mut tampered = signed.clone();
        tampered.version = "9.9.9".into();
        let json = serde_json::to_string(&tampered).unwrap();
        assert!(matches!(
            verify_manifest(&json, &pubkey),
            Err(UpdateError::BadSignature(_))
        ));

        // 篡改产物 url 同样会失败（签名覆盖 assets）。
        let mut tampered = signed.clone();
        tampered.assets[0].url = "https://evil.invalid/x.exe".into();
        let json = serde_json::to_string(&tampered).unwrap();
        assert!(verify_manifest(&json, &pubkey).is_err());
    }

    #[test]
    fn unsigned_manifest_is_rejected() {
        let (pubkey, _seed) = keygen();
        let json = serde_json::to_string(&sample_manifest("0.2.0")).unwrap();
        assert!(matches!(
            verify_manifest(&json, &pubkey),
            Err(UpdateError::UnsignedManifest)
        ));
        // --allow-unsigned 路径仍能解析结构。
        assert!(parse_manifest_unverified(&json).is_ok());
    }

    #[test]
    fn wrong_pubkey_is_rejected() {
        let (_pubkey, seed) = keygen();
        let (other_pubkey, _) = keygen();
        let signed = sign_manifest(&sample_manifest("0.2.0"), &seed).unwrap();
        let json = serde_json::to_string(&signed).unwrap();
        assert!(matches!(
            verify_manifest(&json, &other_pubkey),
            Err(UpdateError::BadSignature(_))
        ));
        assert!(matches!(
            verify_manifest(&json, "zz"),
            Err(UpdateError::BadPubkey(_))
        ));
    }

    #[test]
    fn canonical_bytes_exclude_signature_and_sort_keys() {
        let signed = sign_manifest(&sample_manifest("0.2.0"), &keygen().1).unwrap();
        let canonical = String::from_utf8(signed.canonical_bytes().unwrap()).unwrap();
        assert!(!canonical.contains("signature"));
        // canonical JSON 的 key 是排序的：assets 出现且 schema 在 version 之前…
        assert!(canonical.starts_with("{\"assets\":"));
        assert!(canonical.contains("\"product\":\"drive-sync\""));
    }

    #[test]
    fn select_asset_matches_exact_target() {
        let manifest = sample_manifest("0.2.0");
        let asset = select_asset(&manifest, "x86_64-pc-windows-msvc").unwrap();
        assert_eq!(asset.size, 1024);
        assert!(select_asset(&manifest, "aarch64-apple-darwin").is_none());
        assert_eq!(manifest.asset_targets().len(), 2);
    }

    #[test]
    fn version_comparison() {
        let manifest = sample_manifest("0.3.0");
        assert!(manifest.is_newer_than("0.2.0").unwrap());
        assert!(!manifest.is_newer_than("0.3.0").unwrap());
        assert!(!manifest.is_newer_than("0.4.0").unwrap());
        assert!(manifest.is_newer_than("v0.2.9").unwrap());
    }

    #[test]
    fn validates_schema_product_and_assets() {
        let mut manifest = sample_manifest("0.2.0");
        manifest.schema = 2;
        assert!(manifest.validate().is_err());

        let mut manifest = sample_manifest("0.2.0");
        manifest.product = "other".into();
        assert!(manifest.validate().is_err());

        let mut manifest = sample_manifest("0.2.0");
        manifest.assets[0].blake3 = "not-hex".into();
        assert!(manifest.validate().is_err());

        let mut manifest = sample_manifest("0.2.0");
        manifest.assets[0].url = "ftp://example.invalid/x".into();
        assert!(manifest.validate().is_err());

        let mut manifest = sample_manifest("0.2.0");
        manifest.assets.push(manifest.assets[0].clone());
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn current_target_is_known_on_supported_platforms() {
        let target = current_target();
        // 本仓库 CI 覆盖的 4 个平台，本机开发机（Windows）必须能识别。
        #[cfg(windows)]
        assert_eq!(target, "x86_64-pc-windows-msvc");
        assert_ne!(target, "unknown");
    }

    #[test]
    fn keygen_derives_consistent_pubkey() {
        let (pubkey, seed) = keygen();
        assert_eq!(pubkey, pubkey_hex_from_seed(&seed).unwrap());
        assert_eq!(pubkey.len(), 64);
        assert_eq!(seed.len(), 64);
    }

    #[test]
    fn committed_pubkey_file_matches_compile_time_fallback() {
        // 发布流水线从 update-pubkey.hex 注入 DRIVE_UPDATE_PUBKEY；两者必须一致，
        // 否则 CI 签名的清单会被客户端内置公钥拒绝。
        let committed = include_str!("../update-pubkey.hex").trim().to_string();
        assert_eq!(committed.len(), 64, "update-pubkey.hex 应为 64 字符 hex");
        assert!(
            committed.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "update-pubkey.hex 含非 hex 字符"
        );
        assert!(!crate::DEFAULT_UPDATE_PUBKEY_HEX.trim().is_empty());
        if option_env!("DRIVE_UPDATE_PUBKEY").is_none() {
            assert_eq!(
                crate::DEFAULT_UPDATE_PUBKEY_HEX.trim(),
                committed,
                "源码 fallback 公钥与 update-pubkey.hex 不一致"
            );
        }
    }
}
