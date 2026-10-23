//! 本地数据目录持久化。
//!
//! 目录布局（见 docs/protocol.md §8.1）：
//!
//! ```text
//! <data_dir>/
//!   identity.json     # 身份密钥（0600，最敏感）
//!   peers.json        # PairRecord 列表
//!   config.json       # relay_url / access_token / clipboard / spool 配置
//!   catalog.json      # 目录 CRDT 全量状态 + revision
//!   local_index.json  # entry_id -> 本地文件路径/哈希
//!   vault_keys.json   # 云端暂存对象密钥（只存客户端）
//!   spool/            # 待发消息（离线补发）
//!   incoming/         # 接收中的 .part 与已收文件
//!   logs/
//! ```
//!
//! 所有 JSON 写入都是原子的（同目录临时文件 + rename），避免进程崩溃留下半截文件；
//! 同一进程内的写入用互斥锁串行化。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};
use drive_core::catalog::Catalog;
use drive_core::identity::{Identity, IdentityFile, PublicIdentity};
use drive_core::pairing::PairRecord;
use drive_core::revocation::RevocationNotice;
use drive_core::vault::VaultMeta;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::util::{file_mtime_ms, random_hex};

pub const STORE_VERSION: u32 = 1;

/// 剪切板同步策略。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipboardSettings {
    #[serde(default = "default_true")]
    pub share_text: bool,
    #[serde(default = "default_true")]
    pub share_images: bool,
    /// 手动暂停开关（暂停后仍接收，但不上报本机变化）。
    #[serde(default)]
    pub paused: bool,
}

fn default_true() -> bool {
    true
}
fn default_poll_ms() -> u64 {
    300
}

impl Default for ClipboardSettings {
    fn default() -> Self {
        Self {
            share_text: true,
            share_images: true,
            paused: false,
        }
    }
}

/// 离线 spool 限额（见协议 §7.1：最多 100 条、总 16 MiB、TTL 24h）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpoolSettings {
    #[serde(default = "default_spool_max_items")]
    pub max_items: usize,
    #[serde(default = "default_spool_max_bytes")]
    pub max_bytes: u64,
    #[serde(default = "default_spool_ttl_ms")]
    pub ttl_ms: u64,
}

fn default_spool_max_items() -> usize {
    100
}
fn default_spool_max_bytes() -> u64 {
    16 * 1024 * 1024
}
fn default_spool_ttl_ms() -> u64 {
    24 * 60 * 60 * 1000
}

impl Default for SpoolSettings {
    fn default() -> Self {
        Self {
            max_items: default_spool_max_items(),
            max_bytes: default_spool_max_bytes(),
            ttl_ms: default_spool_ttl_ms(),
        }
    }
}

/// `<data_dir>/config.json`。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientConfig {
    #[serde(default = "default_store_version")]
    pub v: u32,
    #[serde(default)]
    pub relay_url: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub clipboard: ClipboardSettings,
    #[serde(default)]
    pub spool: SpoolSettings,
    /// 剪切板变更轮询间隔（毫秒）。
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
}

fn default_store_version() -> u32 {
    STORE_VERSION
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            v: STORE_VERSION,
            relay_url: String::new(),
            access_token: String::new(),
            clipboard: ClipboardSettings::default(),
            spool: SpoolSettings::default(),
            poll_ms: default_poll_ms(),
        }
    }
}

/// 本机持有软件本体的索引项：条目 id → 本地路径 + 内容哈希。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalIndexEntry {
    pub path: PathBuf,
    pub content_hash: String,
    pub size: u64,
    pub updated_at_ms: u64,
}

/// 云端暂存对象密钥（服务端不可见；跨设备经 E2E 通道 `vault_share` 共享）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VaultKey {
    pub object_id: String,
    pub key_hex: String,
    pub meta: VaultMeta,
    pub created_at_ms: u64,
    /// 上传时请求的 TTL（秒）。
    #[serde(default)]
    pub ttl_secs: u64,
    /// 已把密钥共享给的设备 id，避免重复发送。
    #[serde(default)]
    pub shared_with: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct PeersFile {
    #[serde(default = "default_store_version")]
    v: u32,
    #[serde(default)]
    peers: Vec<PairRecord>,
}

#[derive(Serialize, Deserialize)]
struct CatalogFile {
    #[serde(default = "default_store_version")]
    v: u32,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    catalog: Catalog,
}

#[derive(Serialize, Deserialize)]
struct LocalIndexFile {
    #[serde(default = "default_store_version")]
    v: u32,
    #[serde(default)]
    entries: BTreeMap<String, LocalIndexEntry>,
}

#[derive(Serialize, Deserialize)]
struct VaultKeysFile {
    #[serde(default = "default_store_version")]
    v: u32,
    #[serde(default)]
    keys: Vec<VaultKey>,
}

/// 本地待投递的解除配对通知（离线撤销信箱）。
#[derive(Serialize, Deserialize, Default)]
struct PendingRevocationsFile {
    #[serde(default = "default_store_version")]
    v: u32,
    #[serde(default)]
    notices: Vec<RevocationNotice>,
}

/// 本地存储句柄（可在线程间共享；内部写锁串行化）。
pub struct Store {
    data_dir: PathBuf,
    write_lock: Mutex<()>,
    tmp_counter: AtomicU64,
}

impl Store {
    /// 打开（并创建）数据目录及子目录。
    pub fn open(data_dir: impl Into<PathBuf>) -> Result<Self> {
        let data_dir = data_dir.into();
        fs::create_dir_all(&data_dir)
            .with_context(|| format!("创建数据目录失败: {}", data_dir.display()))?;
        let store = Self {
            data_dir,
            write_lock: Mutex::new(()),
            tmp_counter: AtomicU64::new(0),
        };
        for dir in [store.spool_dir(), store.incoming_dir(), store.logs_dir()] {
            fs::create_dir_all(&dir).with_context(|| format!("创建目录失败: {}", dir.display()))?;
        }
        Ok(store)
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn identity_path(&self) -> PathBuf {
        self.data_dir.join("identity.json")
    }
    pub fn peers_path(&self) -> PathBuf {
        self.data_dir.join("peers.json")
    }
    pub fn config_path(&self) -> PathBuf {
        self.data_dir.join("config.json")
    }
    pub fn catalog_path(&self) -> PathBuf {
        self.data_dir.join("catalog.json")
    }
    pub fn local_index_path(&self) -> PathBuf {
        self.data_dir.join("local_index.json")
    }
    pub fn vault_keys_path(&self) -> PathBuf {
        self.data_dir.join("vault_keys.json")
    }
    /// 待投递的解除配对通知队列（离线撤销用）。
    pub fn pending_revocations_path(&self) -> PathBuf {
        self.data_dir.join("pending_revocations.json")
    }
    pub fn spool_dir(&self) -> PathBuf {
        self.data_dir.join("spool")
    }
    pub fn incoming_dir(&self) -> PathBuf {
        self.data_dir.join("incoming")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn identity_exists(&self) -> bool {
        self.identity_path().exists()
    }

    /// 文件 mtime（Unix 毫秒）。用于检测其他进程对配置类文件的写入。
    pub fn file_mtime_ms(&self, path: &Path) -> Option<u64> {
        file_mtime_ms(path)
    }

    // ------------------------------------------------------------------
    // identity
    // ------------------------------------------------------------------

    /// 加载或创建身份。返回 `(identity, 是否新建)`。
    pub fn load_or_create_identity(&self, name: &str) -> Result<(Identity, bool)> {
        let path = self.identity_path();
        if path.exists() {
            let raw =
                fs::read(&path).with_context(|| format!("读取身份文件失败: {}", path.display()))?;
            let file: IdentityFile = serde_json::from_slice(&raw)
                .with_context(|| format!("解析身份文件失败: {}", path.display()))?;
            let identity = Identity::from_file(file).context("身份文件内容非法")?;
            Ok((identity, false))
        } else {
            let identity = Identity::generate(name);
            self.save_identity(&identity)?;
            Ok((identity, true))
        }
    }

    /// 加载已存在的身份；不存在时报友好错误（提示先运行 `init`）。
    pub fn load_identity(&self) -> Result<Identity> {
        let path = self.identity_path();
        if !path.exists() {
            bail!(
                "尚未初始化设备身份（{} 不存在）。请先运行 `drive-sync init --name <设备名>`",
                path.display()
            );
        }
        let raw =
            fs::read(&path).with_context(|| format!("读取身份文件失败: {}", path.display()))?;
        let file: IdentityFile = serde_json::from_slice(&raw)
            .with_context(|| format!("解析身份文件失败: {}", path.display()))?;
        Identity::from_file(file).context("身份文件内容非法")
    }

    /// 原子保存身份（临时文件写入后 rename；Unix 下保持 0600 权限）。
    pub fn save_identity(&self, identity: &Identity) -> Result<()> {
        let _guard = self.lock();
        let path = self.identity_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.temp_path(&path);
        identity
            .save(&tmp)
            .with_context(|| format!("写入身份文件失败: {}", tmp.display()))?;
        self.replace_file(&tmp, &path)
    }

    // ------------------------------------------------------------------
    // config
    // ------------------------------------------------------------------

    pub fn load_config(&self) -> Result<ClientConfig> {
        Ok(self
            .read_json_opt::<ClientConfig>(&self.config_path())?
            .unwrap_or_default())
    }

    pub fn save_config(&self, config: &ClientConfig) -> Result<()> {
        self.write_json_atomic(&self.config_path(), config)
    }

    // ------------------------------------------------------------------
    // peers
    // ------------------------------------------------------------------

    pub fn load_peers(&self) -> Result<Vec<PairRecord>> {
        let file = self.read_json_opt::<PeersFile>(&self.peers_path())?;
        Ok(file.map(|f| f.peers).unwrap_or_default())
    }

    pub fn save_peers(&self, peers: &[PairRecord]) -> Result<()> {
        let file = PeersFile {
            v: STORE_VERSION,
            peers: peers.to_vec(),
        };
        self.write_json_atomic(&self.peers_path(), &file)
    }

    /// 新增或更新对端记录（按 peer.device_id 去重）。
    pub fn upsert_peer(&self, record: PairRecord) -> Result<()> {
        let mut peers = self.load_peers()?;
        match peers
            .iter_mut()
            .find(|p| p.peer.device_id == record.peer.device_id)
        {
            Some(slot) => *slot = record,
            None => peers.push(record),
        }
        self.save_peers(&peers)
    }

    /// 删除对端记录（解除信任）。返回是否确实删除。
    pub fn remove_peer(&self, device_id: &str) -> Result<bool> {
        let mut peers = self.load_peers()?;
        let before = peers.len();
        peers.retain(|p| p.peer.device_id != device_id);
        if peers.len() != before {
            self.save_peers(&peers)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn find_peer(&self, device_id: &str) -> Result<Option<PairRecord>> {
        Ok(self
            .load_peers()?
            .into_iter()
            .find(|p| p.peer.device_id == device_id))
    }

    // ------------------------------------------------------------------
    // 待投递的解除配对通知（离线撤销信箱）
    // ------------------------------------------------------------------

    /// 读取本地待投递通知（非破坏性；投递成功后再用 [`Store::ack_revocation`] 删除）。
    ///
    /// 命名沿用接口约定 `take_pending_revocations`：调用方拿到列表后负责逐条确认。
    pub fn take_pending_revocations(&self) -> Result<Vec<RevocationNotice>> {
        let file =
            self.read_json_opt::<PendingRevocationsFile>(&self.pending_revocations_path())?;
        Ok(file.map(|f| f.notices).unwrap_or_default())
    }

    /// 把一条签名通知加入本地待发队列（按 `notice_id` 去重）。
    ///
    /// 重复投递同一目标/原因的通知由上层去重；这里只保证 notice_id 不重复写入。
    pub fn queue_revocation(&self, notice: &RevocationNotice) -> Result<()> {
        let mut notices = self.take_pending_revocations()?;
        let notice_id = notice.notice_id();
        match notices
            .iter_mut()
            .find(|existing| existing.notice_id() == notice_id)
        {
            Some(slot) => *slot = notice.clone(),
            None => notices.push(notice.clone()),
        }
        let file = PendingRevocationsFile {
            v: STORE_VERSION,
            notices,
        };
        self.write_json_atomic(&self.pending_revocations_path(), &file)
    }

    /// 通知已投递/已过期：从队列删除。返回是否确实删除。
    pub fn ack_revocation(&self, notice_id: &str) -> Result<bool> {
        let mut notices = self.take_pending_revocations()?;
        let before = notices.len();
        notices.retain(|notice| notice.notice_id() != notice_id);
        if notices.len() != before {
            let file = PendingRevocationsFile {
                v: STORE_VERSION,
                notices,
            };
            self.write_json_atomic(&self.pending_revocations_path(), &file)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    // ------------------------------------------------------------------
    // catalog / local index
    // ------------------------------------------------------------------

    /// 返回 `(catalog, revision)`。
    pub fn load_catalog(&self) -> Result<(Catalog, u64)> {
        let file = self.read_json_opt::<CatalogFile>(&self.catalog_path())?;
        Ok(file
            .map(|f| (f.catalog, f.revision))
            .unwrap_or_else(|| (Catalog::default(), 0)))
    }

    pub fn save_catalog(&self, catalog: &Catalog, revision: u64) -> Result<()> {
        let file = CatalogFile {
            v: STORE_VERSION,
            revision,
            catalog: catalog.clone(),
        };
        self.write_json_atomic(&self.catalog_path(), &file)
    }

    pub fn load_local_index(&self) -> Result<BTreeMap<String, LocalIndexEntry>> {
        let file = self.read_json_opt::<LocalIndexFile>(&self.local_index_path())?;
        Ok(file.map(|f| f.entries).unwrap_or_default())
    }

    pub fn save_local_index(&self, entries: &BTreeMap<String, LocalIndexEntry>) -> Result<()> {
        let file = LocalIndexFile {
            v: STORE_VERSION,
            entries: entries.clone(),
        };
        self.write_json_atomic(&self.local_index_path(), &file)
    }

    // ------------------------------------------------------------------
    // vault keys
    // ------------------------------------------------------------------

    pub fn load_vault_keys(&self) -> Result<Vec<VaultKey>> {
        let file = self.read_json_opt::<VaultKeysFile>(&self.vault_keys_path())?;
        Ok(file.map(|f| f.keys).unwrap_or_default())
    }

    pub fn save_vault_keys(&self, keys: &[VaultKey]) -> Result<()> {
        let file = VaultKeysFile {
            v: STORE_VERSION,
            keys: keys.to_vec(),
        };
        self.write_json_atomic(&self.vault_keys_path(), &file)
    }

    pub fn find_vault_key(&self, object_id: &str) -> Result<Option<VaultKey>> {
        Ok(self
            .load_vault_keys()?
            .into_iter()
            .find(|k| k.object_id == object_id))
    }

    /// 写入（或覆盖）一个 vault 密钥。
    pub fn upsert_vault_key(&self, key: VaultKey) -> Result<()> {
        let mut keys = self.load_vault_keys()?;
        match keys.iter_mut().find(|k| k.object_id == key.object_id) {
            Some(slot) => *slot = key,
            None => keys.push(key),
        }
        self.save_vault_keys(&keys)
    }

    /// 标记某对象密钥已共享给指定设备。
    pub fn mark_vault_key_shared(&self, object_id: &str, peer_id: &str) -> Result<()> {
        let mut keys = self.load_vault_keys()?;
        if let Some(k) = keys.iter_mut().find(|k| k.object_id == object_id) {
            if !k.shared_with.iter().any(|p| p == peer_id) {
                k.shared_with.push(peer_id.to_string());
                self.save_vault_keys(&keys)?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // 内部：原子 JSON 读写
    // ------------------------------------------------------------------

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.write_lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn temp_path(&self, path: &Path) -> PathBuf {
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let file_name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "data".to_string());
        path.with_file_name(format!(".{file_name}.tmp-{}-{n}", std::process::id()))
    }

    fn replace_file(&self, tmp: &Path, path: &Path) -> Result<()> {
        let mut last_err = None;
        for attempt in 0..5u64 {
            match fs::rename(tmp, path) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(10 * (attempt + 1)));
                }
            }
        }
        let _ = fs::remove_file(tmp);
        Err(anyhow!(
            "写入 {} 失败: {}",
            path.display(),
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "未知错误".into())
        ))
    }

    fn write_json_atomic<T: Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value)
            .with_context(|| format!("序列化 {} 失败", path.display()))?;
        let _guard = self.lock();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.temp_path(path);
        fs::write(&tmp, &bytes).with_context(|| format!("写临时文件失败: {}", tmp.display()))?;
        self.replace_file(&tmp, path)
    }

    fn read_json_opt<T: DeserializeOwned>(&self, path: &Path) -> Result<Option<T>> {
        match fs::read(path) {
            Ok(bytes) => {
                // 容忍外部编辑器写入的 UTF-8 BOM。
                let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
                if bytes.iter().all(|b| b.is_ascii_whitespace()) {
                    return Ok(None);
                }
                let value = serde_json::from_slice(bytes)
                    .with_context(|| format!("解析 {} 失败", path.display()))?;
                Ok(Some(value))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow!("读取 {} 失败: {}", path.display(), e)),
        }
    }
}

/// 平台默认数据目录：
/// - Windows：`%APPDATA%\drive-sync`
/// - Linux：`$XDG_DATA_HOME/drive-sync` 或 `~/.local/share/drive-sync`
/// - macOS：`~/Library/Application Support/drive-sync`
pub fn default_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("DRIVE_SYNC_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.trim().is_empty() {
                return PathBuf::from(appdata).join(drive_core::APP_NAME);
            }
        }
        PathBuf::from(".").join(".drive-sync")
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join(drive_core::APP_NAME);
        }
        PathBuf::from(".").join(".drive-sync")
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            if !xdg.trim().is_empty() {
                return PathBuf::from(xdg).join(drive_core::APP_NAME);
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                return PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join(drive_core::APP_NAME);
            }
        }
        PathBuf::from(".drive-sync")
    }
}

/// 由公开身份生成简短展示字符串（不含敏感信息）。
pub fn short_peer_label(peer: &PublicIdentity) -> String {
    format!("{} ({})", peer.name, peer.device_id)
}

/// 生成随机短 id（用于 spool 文件名等）。
pub fn short_id() -> String {
    random_hex(8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use drive_core::identity::Identity;
    use drive_core::pairing::{PairRecord, PAIRING_VERSION};
    use tempfile::TempDir;

    fn record(identity: &Identity, label: &str) -> PairRecord {
        PairRecord {
            v: PAIRING_VERSION,
            peer: identity.public(),
            relay_url: "ws://127.0.0.1:8787/ws".into(),
            paired_at_ms: 1_700_000_000_000,
            label: label.to_string(),
            pairing_fingerprint: "0123456789abcdef".into(),
            last_seen_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn peers_persist_across_reopen_without_ttl() {
        let dir = TempDir::new().expect("临时目录");
        let identity_a = Identity::generate("设备A");
        let identity_b = Identity::generate("设备B");
        {
            let store = Store::open(dir.path()).expect("打开 store");
            store
                .save_peers(&[record(&identity_a, "peer-a"), record(&identity_b, "peer-b")])
                .expect("写入 peers");
        }
        // 模拟进程重启 / 长时间之后重新打开：配对记录必须原样保留（无 TTL）。
        let store = Store::open(dir.path()).expect("重开 store");
        let peers = store.load_peers().expect("读取 peers");
        assert_eq!(peers.len(), 2);
        assert!(peers
            .iter()
            .any(|peer| peer.peer.device_id == identity_a.device_id()));
        assert!(peers
            .iter()
            .any(|peer| peer.peer.device_id == identity_b.device_id()));
        assert_eq!(peers[0].paired_at_ms, 1_700_000_000_000);
        assert_eq!(peers[0].pairing_fingerprint, "0123456789abcdef");

        // peers.json 仍是合法 JSON（原子写不应留下半截文件）。
        let raw = std::fs::read_to_string(store.peers_path()).expect("读文件");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("解析 JSON");
        assert_eq!(parsed["v"], STORE_VERSION);
        assert_eq!(parsed["peers"].as_array().expect("数组").len(), 2);
    }

    #[test]
    fn upsert_peer_dedups_and_keeps_latest_fields() {
        let dir = TempDir::new().expect("临时目录");
        let store = Store::open(dir.path()).expect("打开 store");
        let identity_b = Identity::generate("设备B");

        let mut first = record(&identity_b, "旧标签");
        first.label = "旧标签".into();
        store.upsert_peer(first).expect("首次 upsert");

        let mut second = record(&identity_b, "新标签");
        second.label = "新标签".into();
        second.paired_at_ms = 1_800_000_000_000;
        store.upsert_peer(second).expect("再次 upsert");

        let peers = store.load_peers().expect("读取 peers");
        assert_eq!(peers.len(), 1, "同一 device_id 不应产生重复记录");
        assert_eq!(peers[0].label, "新标签");
        assert_eq!(peers[0].paired_at_ms, 1_800_000_000_000);
    }

    #[test]
    fn remove_peer_is_idempotent() {
        let dir = TempDir::new().expect("临时目录");
        let store = Store::open(dir.path()).expect("打开 store");
        let identity_a = Identity::generate("设备A");
        let identity_b = Identity::generate("设备B");
        store
            .save_peers(&[record(&identity_a, "a"), record(&identity_b, "b")])
            .expect("写入 peers");

        assert!(store
            .remove_peer(&identity_a.device_id())
            .expect("第一次删除"));
        let peers = store.load_peers().expect("读取 peers");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].peer.device_id, identity_b.device_id());

        // 重复删除：幂等（返回 false，不报错、不影响其它记录）。
        assert!(!store
            .remove_peer(&identity_a.device_id())
            .expect("第二次删除"));
        assert_eq!(store.load_peers().expect("读取 peers").len(), 1);
    }

    #[test]
    fn pending_revocation_queue_persists_dedups_and_acks() {
        use drive_core::revocation::reason;
        let dir = TempDir::new().expect("临时目录");
        let identity_a = Identity::generate("设备A");
        let identity_b = Identity::generate("设备B");
        let notice = RevocationNotice::create(
            &identity_a,
            &identity_b.device_id(),
            reason::USER_REMOVED,
            1_000,
        )
        .expect("创建撤销通知");
        let notice_id = notice.notice_id().to_string();

        {
            let store = Store::open(dir.path()).expect("打开 store");
            store.queue_revocation(&notice).expect("入队");
            // 重复入队同一 notice_id 不应产生重复条目。
            store.queue_revocation(&notice).expect("重复入队");
        }

        // 进程重启后队列仍在（离线撤销可跨重启重试）。
        let store = Store::open(dir.path()).expect("重开 store");
        let pending = store.take_pending_revocations().expect("读取队列");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].notice_id(), notice_id);
        assert_eq!(pending[0].content.to, identity_b.device_id());

        assert!(store.ack_revocation(&notice_id).expect("ack"));
        assert!(store
            .take_pending_revocations()
            .expect("读取队列")
            .is_empty());
        // ack 幂等。
        assert!(!store.ack_revocation(&notice_id).expect("重复 ack"));
    }
}
