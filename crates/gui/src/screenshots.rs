//! 截图与演示数据辅助：仅用于本机验收截图（`target/gui-screenshots/` 不入库）。

use std::path::Path;

use anyhow::{Context, Result};
use drive_client::{LocalIndexEntry, Store, VaultKey};
use drive_core::catalog::{Catalog, CatalogEntry, TargetPlatform};
use drive_core::identity::Identity;
use drive_core::pairing::{PairRecord, PAIRING_VERSION};
use drive_core::util::{now_ms, random_hex};
use drive_core::vault::VaultMeta;

/// 把 egui 截图保存为 PNG。
pub fn save_color_image(path: &Path, image: &egui::ColorImage) -> Result<()> {
    let width = image.size[0] as u32;
    let height = image.size[1] as u32;
    if width == 0 || height == 0 {
        anyhow::bail!("截图为空");
    }
    let mut rgba = Vec::with_capacity(image.pixels.len() * 4);
    for pixel in &image.pixels {
        rgba.extend_from_slice(&pixel.to_array());
    }
    let rgba_image = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| anyhow::anyhow!("截图尺寸与像素数量不匹配"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建截图目录失败: {}", parent.display()))?;
    }
    rgba_image
        .save(path)
        .with_context(|| format!("保存截图失败: {}", path.display()))?;
    Ok(())
}

/// 为截图演示准备一个包含身份/配置/配对/目录/暂存的数据目录（不启动真实连接）。
pub fn prepare_demo_data(dir: &Path) -> Result<()> {
    let store = Store::open(dir)?;
    let identity = if store.identity_exists() {
        store.load_identity()?
    } else {
        store.load_or_create_identity("演示笔记本")?.0
    };

    let mut config = store.load_config()?;
    if config.relay_url.trim().is_empty() {
        config.relay_url = "wss://sync.example.com/ws".to_string();
    }
    store.save_config(&config)?;

    if store.load_peers()?.is_empty() {
        let now = now_ms();
        let desktop = Identity::generate("办公室台式机");
        let laptop = Identity::generate("公司笔记本");
        let record = |peer: &Identity, label: &str, last_seen_ms: u64| PairRecord {
            v: PAIRING_VERSION,
            peer: peer.public(),
            relay_url: "wss://sync.example.com/ws".to_string(),
            paired_at_ms: now.saturating_sub(46 * 24 * 60 * 60 * 1000),
            label: label.to_string(),
            pairing_fingerprint: random_hex(8),
            last_seen_ms,
        };
        store.save_peers(&[
            record(
                &desktop,
                "办公室台式机",
                now.saturating_sub(2 * 60 * 60 * 1000),
            ),
            record(
                &laptop,
                "公司笔记本",
                now.saturating_sub(3 * 24 * 60 * 60 * 1000),
            ),
        ])?;
    }

    let (mut catalog, mut revision) = store.load_catalog()?;
    if catalog.node.trim().is_empty() {
        catalog.node = identity.device_id();
    }
    if catalog.visible_count() == 0 {
        let samples = dir.join("samples");
        std::fs::create_dir_all(&samples)?;
        let sample_path = samples.join("demo-tool-1.2.3.zip");
        if !sample_path.exists() {
            std::fs::write(&sample_path, b"drive-sync demo payload")?;
        }
        let size = std::fs::metadata(&sample_path)?.len();
        let hash = drive_core::util::blake3_hex(b"drive-sync demo payload");
        let mut entry = CatalogEntry::new(
            "示例工具",
            "1.2.3",
            TargetPlatform::Windows,
            "installer",
            size,
            hash.clone(),
            vec!["demo".to_string()],
            "演示数据",
            &catalog.node,
        );
        entry.id = format!("ent-{}", random_hex(8));
        let now = now_ms();
        catalog.upsert_local(entry.clone(), &identity.device_id(), now);
        revision = revision.saturating_add(1);
        let mut index = store.load_local_index()?;
        index.insert(
            entry.id.clone(),
            LocalIndexEntry {
                path: sample_path,
                content_hash: hash,
                size,
                updated_at_ms: now,
            },
        );
        store.save_catalog(&catalog, revision)?;
        store.save_local_index(&index)?;
    }

    if store.load_vault_keys()?.is_empty() {
        let now = now_ms();
        let keys = vec![
            VaultKey {
                object_id: format!("obj-{}", random_hex(8)),
                key_hex: random_hex(32),
                meta: VaultMeta {
                    name: "季度备份.zip".to_string(),
                    kind: "archive".to_string(),
                    size: 24 * 1024 * 1024,
                    content_hash: random_hex(32),
                    entry_id: None,
                    note: String::new(),
                },
                created_at_ms: now.saturating_sub(2 * 24 * 60 * 60 * 1000),
                ttl_secs: 7 * 24 * 60 * 60,
                shared_with: Vec::new(),
            },
            VaultKey {
                object_id: format!("obj-{}", random_hex(8)),
                key_hex: random_hex(32),
                meta: VaultMeta {
                    name: "设计稿.psd".to_string(),
                    kind: "file".to_string(),
                    size: 8 * 1024 * 1024,
                    content_hash: random_hex(32),
                    entry_id: None,
                    note: String::new(),
                },
                created_at_ms: now.saturating_sub(5 * 24 * 60 * 60 * 1000),
                ttl_secs: 30 * 24 * 60 * 60,
                shared_with: Vec::new(),
            },
        ];
        store.save_vault_keys(&keys)?;
    }

    let _ = Catalog::new(&identity.device_id());
    Ok(())
}
