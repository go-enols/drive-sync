//! Android JNI 壳：`dev.drivesync.mobile.Native`（Kotlin `object`）的 native 方法。
//!
//! 符号名规则：`Java_dev_drivesync_mobile_Native_<方法名>`。Kotlin `object` 的
//! `external fun` 编译为实例 native 方法，第二个参数是 `jobject`（本实现忽略）。
//!
//! 线程说明：
//! - 事件/剪切板回调由 Rust 的 `drive-sync-ui` 分发线程触发，回调里每次
//!   `attach_current_thread` 取得 `JNIEnv` 后调用 Kotlin，调用结束自动 detach；
//! - 回调对象用 `GlobalRef` 保住，避免 GC 回收；`dmStop` 时释放。
//!
//! 仅非 iOS 目标编译（`lib.rs` 中按 `cfg(not(target_os = "ios"))` 引入）。

use std::ffi::{c_char, c_void, CStr};
use std::sync::{Arc, Mutex, OnceLock};

use jni::objects::{GlobalRef, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong, jstring};
use jni::{JNIEnv, JavaVM};

use crate::ffi;

static JAVA_VM: OnceLock<Arc<JavaVM>> = OnceLock::new();
static CALLBACK: Mutex<Option<GlobalRef>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 首次 native 调用时缓存 JavaVM（后续后台线程 attach 用）。
fn ensure_vm(env: &mut JNIEnv) {
    if JAVA_VM.get().is_some() {
        return;
    }
    if let Ok(vm) = env.get_java_vm() {
        let _ = JAVA_VM.set(Arc::new(vm));
    }
}

fn jstring_to_string(env: &mut JNIEnv, value: &JString) -> Option<String> {
    env.get_string(value).ok().map(|text| text.into())
}

fn new_jstring(env: &mut JNIEnv, text: &str) -> jstring {
    match env.new_string(text) {
        Ok(value) => value.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn new_jstring_json(env: &mut JNIEnv, text: String) -> jstring {
    new_jstring(env, &text)
}

fn read_c_char(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// 在后台线程 attach JVM 并调用 Kotlin 回调对象的 `onEvent(String)`。
fn emit_event_to_java(json: &str) {
    let Some(vm) = JAVA_VM.get() else {
        return;
    };
    let callback = lock(&CALLBACK).clone();
    let Some(callback) = callback else {
        return;
    };
    let Ok(mut env) = vm.attach_current_thread() else {
        return;
    };
    let Ok(argument) = env.new_string(json) else {
        return;
    };
    let args = [JValue::Object(&argument)];
    let _ = env.call_method(&callback, "onEvent", "(Ljava/lang/String;)V", &args);
}

/// 调用 Kotlin 回调对象的 `onClipboard(String, String)`（文本 + 来源设备 id）。
fn emit_clipboard_to_java(text: &str, from: &str) {
    let Some(vm) = JAVA_VM.get() else {
        return;
    };
    let callback = lock(&CALLBACK).clone();
    let Some(callback) = callback else {
        return;
    };
    let Ok(mut env) = vm.attach_current_thread() else {
        return;
    };
    let Ok(text_arg) = env.new_string(text) else {
        return;
    };
    let Ok(from_arg) = env.new_string(from) else {
        return;
    };
    let args = [JValue::Object(&text_arg), JValue::Object(&from_arg)];
    let _ = env.call_method(
        &callback,
        "onClipboard",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        &args,
    );
}

unsafe extern "C" fn event_trampoline(json: *const c_char, _user_data: *mut c_void) {
    if let Some(text) = read_c_char(json) {
        emit_event_to_java(&text);
    }
}

unsafe extern "C" fn clipboard_trampoline(
    text: *const c_char,
    from: *const c_char,
    _user_data: *mut c_void,
) {
    let Some(text) = read_c_char(text) else {
        return;
    };
    let from = read_c_char(from).unwrap_or_default();
    emit_clipboard_to_java(&text, &from);
}

// ---------------------------------------------------------------------------
// 基础信息
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmVersion<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
) -> jstring {
    ensure_vm(&mut env);
    new_jstring(&mut env, env!("CARGO_PKG_VERSION"))
}

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmAbiVersion<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
) -> jint {
    ensure_vm(&mut env);
    crate::runtime::DM_ABI_VERSION
}

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmLastError<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
) -> jstring {
    ensure_vm(&mut env);
    let pointer = ffi::last_error_ptr();
    if pointer.is_null() {
        return std::ptr::null_mut();
    }
    match read_c_char(pointer) {
        Some(text) => new_jstring(&mut env, &text),
        None => std::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
// 设备身份 / 配置
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmInit<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    data_dir: JString<'local>,
    device_name: JString<'local>,
) -> jstring {
    ensure_vm(&mut env);
    let Some(data_dir) = jstring_to_string(&mut env, &data_dir) else {
        return new_jstring(&mut env, "{\"ok\":false,\"error\":\"无效的 data_dir\"}");
    };
    let device_name = jstring_to_string(&mut env, &device_name).unwrap_or_default();
    new_jstring_json(&mut env, ffi::init_device(&data_dir, &device_name))
}

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmLoad<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    data_dir: JString<'local>,
) -> jstring {
    ensure_vm(&mut env);
    let Some(data_dir) = jstring_to_string(&mut env, &data_dir) else {
        return new_jstring(&mut env, "{\"ok\":false,\"error\":\"无效的 data_dir\"}");
    };
    new_jstring_json(&mut env, ffi::load_device(&data_dir))
}

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmConfigSetRelay<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    config_json: JString<'local>,
) -> jstring {
    ensure_vm(&mut env);
    let Some(config_json) = jstring_to_string(&mut env, &config_json) else {
        return new_jstring(&mut env, "{\"ok\":false,\"error\":\"无效的配置 JSON\"}");
    };
    new_jstring_json(&mut env, ffi::config_set_relay(&config_json))
}

// ---------------------------------------------------------------------------
// 同步会话
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmStart<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    relay_url: JString<'local>,
    token: JString<'local>,
    callback: JObject<'local>,
) -> jlong {
    ensure_vm(&mut env);
    let relay_url = jstring_to_string(&mut env, &relay_url).unwrap_or_default();
    let token = jstring_to_string(&mut env, &token).unwrap_or_default();
    if !callback.as_raw().is_null() {
        if let Ok(global) = env.new_global_ref(&callback) {
            *lock(&CALLBACK) = Some(global);
        }
    }
    match ffi::start_runtime(
        Some(&relay_url),
        Some(&token),
        Some(clipboard_trampoline),
        Some(event_trampoline),
        std::ptr::null_mut(),
    ) {
        Ok(handle) => handle as jlong,
        Err(error) => {
            ffi::set_last_error(error.to_string());
            0
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmStop<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    handle: jlong,
) {
    ensure_vm(&mut env);
    ffi::stop_runtime(handle as *mut c_void);
    *lock(&CALLBACK) = None;
}

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmSendClipboardText<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    handle: jlong,
    text: JString<'local>,
) -> jint {
    ensure_vm(&mut env);
    let Some(text) = jstring_to_string(&mut env, &text) else {
        return -2;
    };
    ffi::send_clipboard_text(handle as *mut c_void, &text)
}

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmFetchCatalog<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    handle: jlong,
    entry_id: JString<'local>,
) -> jstring {
    ensure_vm(&mut env);
    let Some(entry_id) = jstring_to_string(&mut env, &entry_id) else {
        return new_jstring(&mut env, "{\"ok\":false,\"error\":\"无效的 entry_id\"}");
    };
    new_jstring_json(
        &mut env,
        ffi::fetch_catalog_json(handle as *mut c_void, &entry_id),
    )
}

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmSetPaused<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    handle: jlong,
    paused: jboolean,
) -> jint {
    ensure_vm(&mut env);
    ffi::set_paused(handle as *mut c_void, paused != 0)
}

// ---------------------------------------------------------------------------
// 配对
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmPairStart<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> jstring {
    ensure_vm(&mut env);
    new_jstring_json(&mut env, ffi::pair_start_json(handle as *mut c_void))
}

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmPairJoin<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    handle: jlong,
    code_or_qr: JString<'local>,
) -> jstring {
    ensure_vm(&mut env);
    let Some(input) = jstring_to_string(&mut env, &code_or_qr) else {
        return new_jstring(&mut env, "{\"ok\":false,\"error\":\"无效的配对码\"}");
    };
    new_jstring_json(&mut env, ffi::pair_join_json(handle as *mut c_void, &input))
}

// ---------------------------------------------------------------------------
// 只读查询
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmPeersJson<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> jstring {
    ensure_vm(&mut env);
    new_jstring_json(&mut env, ffi::peers_json(handle as *mut c_void))
}

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmCatalogJson<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> jstring {
    ensure_vm(&mut env);
    new_jstring_json(&mut env, ffi::catalog_json(handle as *mut c_void))
}

#[no_mangle]
pub extern "system" fn Java_dev_drivesync_mobile_Native_dmStatusJson<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> jstring {
    ensure_vm(&mut env);
    new_jstring_json(&mut env, ffi::status_json(handle as *mut c_void))
}
