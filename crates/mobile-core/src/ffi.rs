//! 稳定 C ABI（`include/drive_mobile.h`）。
//!
//! Android JNI 壳（[`crate::jni`]）与 iOS Swift 桥都只依赖本模块导出的 `dm_*` 符号，
//! 或（JNI 壳）直接调用这里的 `pub(crate)` Rust 函数。
//!
//! 约定：
//! - 返回 `char *` 的函数一律返回 Rust 分配的 UTF-8 JSON，调用方用 `dm_string_free` 释放；
//!   NULL 仅表示内存分配失败；业务错误放在 `{"ok":false,"error":"..."}`。
//! - 进程级单例：`dm_init`/`dm_load` 设置设备上下文，`dm_start` 创建唯一活动会话。

use std::cell::RefCell;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use drive_core::clipboard::ClipboardPayload;
use serde_json::{json, Value};

use crate::runtime::{
    require_device, set_device, Callbacks, ClipboardCallback, DeviceContext, EventCallback, Inner,
    DM_ABI_VERSION,
};

/// 当前活动会话句柄（`Arc::into_raw` 的指针，作为 C ABI 不透明句柄）。
static ACTIVE_PTR: AtomicUsize = AtomicUsize::new(0);
/// 当前活动会话（保持强引用；dm_stop 时与 ACTIVE_PTR 一起释放）。
static ACTIVE: Mutex<Option<Arc<Inner>>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// 线程本地错误
// ---------------------------------------------------------------------------

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// 记录线程本地错误（供 `dm_last_error` 与日志使用）。
pub fn set_last_error(message: impl Into<String>) {
    let text = message.into().replace('\0', " ");
    let c_string = CString::new(text).unwrap_or_default();
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(c_string));
}

/// 取线程本地错误指针（静态生命周期直到本线程下一次 set_last_error）。
pub fn last_error_ptr() -> *const c_char {
    LAST_ERROR.with(|slot| match slot.borrow().as_ref() {
        Some(text) => text.as_ptr(),
        None => std::ptr::null(),
    })
}

// ---------------------------------------------------------------------------
// JSON 返回辅助
// ---------------------------------------------------------------------------

fn error_json(message: impl Into<String>) -> String {
    let message = message.into();
    set_last_error(message.clone());
    json!({"ok": false, "error": message}).to_string()
}

fn result_json(result: Result<Value>) -> String {
    match result {
        Ok(value) => value.to_string(),
        Err(error) => error_json(error.to_string()),
    }
}

fn into_c_string(text: String) -> *mut c_char {
    match CString::new(text) {
        Ok(text) => text.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
// 句柄辅助
// ---------------------------------------------------------------------------

/// 从 C ABI 句柄借用会话（递增 Arc 引用计数，调用期间对象不会释放）。
fn borrow_inner(handle: *mut c_void) -> Option<Arc<Inner>> {
    if handle.is_null() {
        return None;
    }
    let active = lock(&ACTIVE);
    if ACTIVE_PTR.load(Ordering::SeqCst) != handle as usize {
        return None;
    }
    active.as_ref().cloned()
}

fn active_ptr() -> *mut c_void {
    ACTIVE_PTR.load(Ordering::SeqCst) as *mut c_void
}

// ---------------------------------------------------------------------------
// 内部 Rust API（JNI 壳复用；全部为 post-ffi 的安全封装）
// ---------------------------------------------------------------------------

/// 初始化设备身份（不存在则创建），设置进程级设备上下文。返回 JSON。
pub(crate) fn init_device(data_dir: &str, device_name: &str) -> String {
    result_json((|| {
        let data_dir = data_dir.trim();
        if data_dir.is_empty() {
            bail!("data_dir 不能为空");
        }
        if ACTIVE_PTR.load(Ordering::SeqCst) != 0 {
            bail!("已有活动同步会话，请先调用 dm_stop");
        }
        let store = drive_client::Store::open(data_dir).context("打开数据目录失败")?;
        let name = if device_name.trim().is_empty() {
            "移动设备"
        } else {
            device_name.trim()
        };
        let (identity, created) = store
            .load_or_create_identity(name)
            .context("加载/创建设备身份失败")?;
        let device_id = identity.device_id();
        let name = identity.name.clone();
        set_device(DeviceContext {
            data_dir: PathBuf::from(data_dir),
            device_id: device_id.clone(),
            name: name.clone(),
        });
        Ok(json!({
            "ok": true,
            "created": created,
            "device_id": device_id,
            "name": name,
            "data_dir": data_dir,
        }))
    })())
}

/// 仅加载已存在的设备身份（不创建）。返回 JSON。
pub(crate) fn load_device(data_dir: &str) -> String {
    result_json((|| {
        let data_dir = data_dir.trim();
        if data_dir.is_empty() {
            bail!("data_dir 不能为空");
        }
        if ACTIVE_PTR.load(Ordering::SeqCst) != 0 {
            bail!("已有活动同步会话，请先调用 dm_stop");
        }
        let store = drive_client::Store::open(data_dir).context("打开数据目录失败")?;
        let identity = match store.load_identity() {
            Ok(identity) => identity,
            Err(_) => bail!(
                "数据目录 {} 中尚未初始化设备身份，请先调用 dm_init(data_dir, device_name)",
                data_dir
            ),
        };
        let device_id = identity.device_id();
        let name = identity.name.clone();
        set_device(DeviceContext {
            data_dir: PathBuf::from(data_dir),
            device_id: device_id.clone(),
            name: name.clone(),
        });
        Ok(json!({
            "ok": true,
            "created": false,
            "device_id": device_id,
            "name": name,
            "data_dir": data_dir,
        }))
    })())
}

/// 保存 relay/token/设备名到 `<data_dir>/config.json`。返回 JSON。
pub(crate) fn config_set_relay(json_text: &str) -> String {
    result_json((|| {
        let context = require_device()?;
        let parsed: Value = serde_json::from_str(json_text).context("配置 JSON 解析失败")?;
        let object = parsed
            .as_object()
            .ok_or_else(|| anyhow!("配置 JSON 必须是对象"))?;
        let store = drive_client::Store::open(&context.data_dir)?;
        let mut config = store.load_config().unwrap_or_default();
        if let Some(value) = object.get("relay_url").and_then(Value::as_str) {
            config.relay_url = value.trim().to_string();
        }
        if let Some(value) = object.get("access_token").and_then(Value::as_str) {
            config.access_token = value.trim().to_string();
        }
        let mut name = context.name.clone();
        if let Some(value) = object.get("device_name").and_then(Value::as_str) {
            let new_name = value.trim();
            if !new_name.is_empty() && new_name != name {
                let mut identity = store
                    .load_identity()
                    .context("加载设备身份失败（请先 dm_init）")?;
                identity.name = new_name.to_string();
                store.save_identity(&identity).context("保存设备名失败")?;
                name = new_name.to_string();
            }
        }
        store.save_config(&config).context("保存配置失败")?;
        let mut updated = context.clone();
        updated.name = name.clone();
        set_device(updated);
        Ok(json!({
            "ok": true,
            "relay_url": config.relay_url,
            "has_token": !config.access_token.is_empty(),
            "device_name": name,
            "data_dir": context.data_dir.display().to_string(),
        }))
    })())
}

/// 启动同步会话（幂等：已有活动会话时返回同一句柄）。
pub(crate) fn start_runtime(
    relay_url: Option<&str>,
    token: Option<&str>,
    clipboard_cb: Option<ClipboardCallback>,
    event_cb: Option<EventCallback>,
    user_data: *mut c_void,
) -> Result<*mut c_void> {
    let existing = active_ptr();
    if !existing.is_null() {
        return Ok(existing);
    }
    let context = require_device()?;
    let callbacks = Callbacks {
        clipboard: clipboard_cb,
        event: event_cb,
        user_data,
    };
    let inner = Inner::start(context, relay_url, token, callbacks)?;
    // Arc::into_raw 的引用作为 C 句柄；ACTIVE 持有另一个强引用。
    let raw = Arc::into_raw(inner.clone()) as *mut c_void;
    ACTIVE_PTR.store(raw as usize, Ordering::SeqCst);
    *lock(&ACTIVE) = Some(inner);
    Ok(raw)
}

/// 停止同步会话并释放句柄。可重复调用。
pub(crate) fn stop_runtime(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    let raw = handle as usize;
    let inner = {
        let mut active = lock(&ACTIVE);
        if ACTIVE_PTR.load(Ordering::SeqCst) != raw {
            return;
        }
        ACTIVE_PTR.store(0, Ordering::SeqCst);
        active.take()
    };
    if let Some(inner) = inner {
        inner.shutdown();
    }
    // 释放 dm_start 时 Arc::into_raw 的强引用。
    // SAFETY: 指针由 start_runtime 中 Arc::into_raw 产生，且仅在此处消费一次
    //（ACTIVE_PTR 已清零，重复 dm_stop 会提前返回）。
    unsafe {
        drop(Arc::from_raw(raw as *const Inner));
    }
}

/// 发送本机剪切板文本。返回值与头文件一致（0 成功，负数错误）。
pub(crate) fn send_clipboard_text(handle: *mut c_void, text: &str) -> i32 {
    let Some(inner) = borrow_inner(handle) else {
        set_last_error("句柄无效或会话已停止");
        return -1;
    };
    if !inner.is_active() {
        set_last_error("同步会话已停止");
        return -1;
    }
    let payload = match ClipboardPayload::text(text) {
        Ok(payload) => payload,
        Err(error) => {
            set_last_error(error.to_string());
            return -2;
        }
    };
    inner.send_clipboard(payload)
}

/// 暂停/恢复剪切板同步。0 成功，-1 句柄无效。
pub(crate) fn set_paused(handle: *mut c_void, paused: bool) -> i32 {
    let Some(inner) = borrow_inner(handle) else {
        set_last_error("句柄无效或会话已停止");
        return -1;
    };
    inner.set_paused(paused);
    0
}

/// 发起配对 → `{"ok":true,"code":"12345678","qr":"..."}`。
pub(crate) fn pair_start_json(handle: *mut c_void) -> String {
    result_json((|| {
        let inner = borrow_inner(handle).ok_or_else(|| anyhow!("句柄无效或会话已停止"))?;
        let info = inner.pair_start()?;
        Ok(json!({"ok": true, "code": info.code, "qr": info.qr}))
    })())
}

/// 加入配对 → `{"ok":true,"peer_id","name","sas"}`。
pub(crate) fn pair_join_json(handle: *mut c_void, code_or_qr: &str) -> String {
    result_json((|| {
        let inner = borrow_inner(handle).ok_or_else(|| anyhow!("句柄无效或会话已停止"))?;
        let info = inner.pair_join(code_or_qr)?;
        Ok(json!({
            "ok": true,
            "peer_id": info.peer_id,
            "name": info.name,
            "sas": info.sas,
        }))
    })())
}

/// 已配对设备列表。
pub(crate) fn peers_json(handle: *mut c_void) -> String {
    result_json((|| {
        let inner = borrow_inner(handle).ok_or_else(|| anyhow!("句柄无效或会话已停止"))?;
        inner.peers_value()
    })())
}

/// 软件目录（只读展示）。
pub(crate) fn catalog_json(handle: *mut c_void) -> String {
    result_json((|| {
        let inner = borrow_inner(handle).ok_or_else(|| anyhow!("句柄无效或会话已停止"))?;
        inner.catalog_value()
    })())
}

/// 会话状态。
pub(crate) fn status_json(handle: *mut c_void) -> String {
    match borrow_inner(handle) {
        Some(inner) => inner.status_value().to_string(),
        None => error_json("句柄无效或会话已停止"),
    }
}

/// 库版本（静态字符串）。
pub(crate) fn version_ptr() -> *const c_char {
    static VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
    VERSION.as_ptr() as *const c_char
}

// ---------------------------------------------------------------------------
// C ABI 导出
// ---------------------------------------------------------------------------

unsafe fn read_cstr(pointer: *const c_char) -> Result<String> {
    if pointer.is_null() {
        return Ok(String::new());
    }
    CStr::from_ptr(pointer)
        .to_str()
        .map(str::to_string)
        .map_err(|_| anyhow!("参数不是合法 UTF-8"))
}

unsafe fn read_cstr_required(pointer: *const c_char) -> Result<String> {
    if pointer.is_null() {
        bail!("参数为空指针");
    }
    read_cstr(pointer)
}

/// 库版本，例如 "0.2.0"。静态字符串，无需释放。
#[no_mangle]
pub extern "C" fn dm_version() -> *const c_char {
    version_ptr()
}

/// C ABI 版本号（与头文件 DM_ABI_VERSION 对应）。
#[no_mangle]
pub extern "C" fn dm_abi_version() -> i32 {
    DM_ABI_VERSION
}

/// 初始化设备身份并设置数据目录。返回 JSON（需 dm_string_free）。
#[no_mangle]
pub unsafe extern "C" fn dm_init(
    data_dir: *const c_char,
    device_name: *const c_char,
) -> *mut c_char {
    let result = (|| -> Result<String> {
        let data_dir = read_cstr_required(data_dir)?;
        let device_name = read_cstr(device_name)?;
        Ok(init_device(&data_dir, &device_name))
    })();
    match result {
        Ok(json_text) => into_c_string(json_text),
        Err(error) => into_c_string(error_json(error.to_string())),
    }
}

/// 仅加载已存在的设备身份。返回 JSON（需 dm_string_free）。
#[no_mangle]
pub unsafe extern "C" fn dm_load(data_dir: *const c_char) -> *mut c_char {
    let result = (|| -> Result<String> {
        let data_dir = read_cstr_required(data_dir)?;
        Ok(load_device(&data_dir))
    })();
    match result {
        Ok(json_text) => into_c_string(json_text),
        Err(error) => into_c_string(error_json(error.to_string())),
    }
}

/// 保存 relay/token/设备名配置。返回 JSON（需 dm_string_free）。
#[no_mangle]
pub unsafe extern "C" fn dm_config_set_relay(config_json: *const c_char) -> *mut c_char {
    let result = (|| -> Result<String> {
        let config_json = if config_json.is_null() {
            "{}".to_string()
        } else {
            read_cstr(config_json)?
        };
        Ok(config_set_relay(&config_json))
    })();
    match result {
        Ok(json_text) => into_c_string(json_text),
        Err(error) => into_c_string(error_json(error.to_string())),
    }
}

/// 启动后台同步运行时；失败返回 NULL（错误见 dm_last_error）。
#[no_mangle]
pub unsafe extern "C" fn dm_start(
    relay_url: *const c_char,
    token: *const c_char,
    clipboard_cb: Option<ClipboardCallback>,
    event_cb: Option<EventCallback>,
    user_data: *mut c_void,
) -> *mut c_void {
    let result = (|| -> Result<*mut c_void> {
        let relay = if relay_url.is_null() {
            None
        } else {
            Some(read_cstr(relay_url)?)
        };
        let token = if token.is_null() {
            None
        } else {
            Some(read_cstr(token)?)
        };
        start_runtime(
            relay.as_deref(),
            token.as_deref(),
            clipboard_cb,
            event_cb,
            user_data,
        )
    })();
    match result {
        Ok(handle) => handle,
        Err(error) => {
            set_last_error(error.to_string());
            std::ptr::null_mut()
        }
    }
}

/// 停止同步运行时并释放句柄。
#[no_mangle]
pub unsafe extern "C" fn dm_stop(handle: *mut c_void) {
    stop_runtime(handle);
}

/// 发送本机剪切板文本（UTF-8）。0 成功，负数错误。
#[no_mangle]
pub unsafe extern "C" fn dm_send_clipboard_text(handle: *mut c_void, text: *const c_char) -> i32 {
    let text = match read_cstr(text) {
        Ok(text) => text,
        Err(error) => {
            set_last_error(error.to_string());
            return -2;
        }
    };
    send_clipboard_text(handle, &text)
}

/// 暂停（1）/恢复（0）剪切板同步。0 成功，负数错误。
#[no_mangle]
pub unsafe extern "C" fn dm_set_paused(handle: *mut c_void, paused: i32) -> i32 {
    set_paused(handle, paused != 0)
}

/// 发起配对；返回 JSON（需 dm_string_free）。
#[no_mangle]
pub unsafe extern "C" fn dm_pair_start(handle: *mut c_void) -> *mut c_char {
    into_c_string(pair_start_json(handle))
}

/// 加入配对；返回 JSON（需 dm_string_free）。
#[no_mangle]
pub unsafe extern "C" fn dm_pair_join(
    handle: *mut c_void,
    code_or_qr: *const c_char,
) -> *mut c_char {
    let result = (|| -> Result<String> {
        let input = read_cstr_required(code_or_qr)?;
        Ok(pair_join_json(handle, &input))
    })();
    match result {
        Ok(json_text) => into_c_string(json_text),
        Err(error) => into_c_string(error_json(error.to_string())),
    }
}

/// 已配对设备列表；返回 JSON（需 dm_string_free）。
#[no_mangle]
pub unsafe extern "C" fn dm_peers_json(handle: *mut c_void) -> *mut c_char {
    into_c_string(peers_json(handle))
}

/// 软件目录；返回 JSON（需 dm_string_free）。
#[no_mangle]
pub unsafe extern "C" fn dm_catalog_json(handle: *mut c_void) -> *mut c_char {
    into_c_string(catalog_json(handle))
}

/// 会话状态；返回 JSON（需 dm_string_free）。
#[no_mangle]
pub unsafe extern "C" fn dm_status_json(handle: *mut c_void) -> *mut c_char {
    into_c_string(status_json(handle))
}

/// 释放本库返回的字符串（NULL 安全）。
#[no_mangle]
pub unsafe extern "C" fn dm_string_free(text: *mut c_char) {
    if text.is_null() {
        return;
    }
    drop(CString::from_raw(text));
}

/// 线程本地最近一次错误（静态字符串，勿释放）。
#[no_mangle]
pub extern "C" fn dm_last_error() -> *const c_char {
    last_error_ptr()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_helpers_roundtrip() {
        let text = result_json(Ok(json!({"ok": true})));
        assert_eq!(text, "{\"ok\":true}");
        let text = result_json(Err(anyhow!("boom")));
        assert!(text.contains("\"ok\":false"));
        assert!(text.contains("boom"));
    }
}
