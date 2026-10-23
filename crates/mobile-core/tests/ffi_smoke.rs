//! C ABI 冒烟测试：设备身份 → 配置 → 启动/停止 → 查询 JSON。
//!
//! 不依赖 Android/网络：中继地址指向本机未监听端口，运行时只会在退避重连循环里，
//! `dm_stop` 必须在数秒内优雅退出（与 client-core runtime 的 shutdown 行为一致）。

use std::ffi::{c_char, CStr, CString};

use drive_mobile::ffi;

fn take_string(pointer: *mut c_char) -> String {
    assert!(!pointer.is_null(), "原生函数返回了 NULL");
    let text = unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .unwrap()
        .to_string();
    unsafe { ffi::dm_string_free(pointer) };
    text
}

#[test]
fn ffi_device_lifecycle() {
    let dir = tempfile::TempDir::new().unwrap();
    let data_dir = CString::new(dir.path().to_str().unwrap()).unwrap();
    let device_name = CString::new("测试安卓设备").unwrap();

    let init = take_string(unsafe { ffi::dm_init(data_dir.as_ptr(), device_name.as_ptr()) });
    assert!(init.contains("\"ok\":true"), "dm_init 失败: {init}");
    assert!(init.contains("dev-"), "dm_init 应返回设备 id: {init}");

    let config = CString::new(
        r#"{"relay_url":"ws://127.0.0.1:59998/ws","access_token":"smoke-token","device_name":"测试安卓设备"}"#,
    )
    .unwrap();
    let configured = take_string(unsafe { ffi::dm_config_set_relay(config.as_ptr()) });
    assert!(
        configured.contains("\"ok\":true"),
        "dm_config_set_relay 失败: {configured}"
    );

    let handle = unsafe {
        ffi::dm_start(
            std::ptr::null(),
            std::ptr::null(),
            None,
            None,
            std::ptr::null_mut(),
        )
    };
    let last_error_pointer = ffi::dm_last_error();
    let last_error = if last_error_pointer.is_null() {
        "unknown".to_string()
    } else {
        unsafe { CStr::from_ptr(last_error_pointer) }
            .to_string_lossy()
            .into_owned()
    };
    assert!(!handle.is_null(), "dm_start 失败: {last_error}");

    // 查询接口在连接失败/重连阶段也必须返回合法 JSON。
    let peers = take_string(unsafe { ffi::dm_peers_json(handle) });
    assert!(peers.contains("\"ok\":true"), "dm_peers_json: {peers}");
    let status = take_string(unsafe { ffi::dm_status_json(handle) });
    assert!(
        status.contains("\"running\":true"),
        "dm_status_json: {status}"
    );
    let catalog = take_string(unsafe { ffi::dm_catalog_json(handle) });
    assert!(
        catalog.contains("\"ok\":true"),
        "dm_catalog_json: {catalog}"
    );

    // 版本/ABI
    assert_eq!(
        unsafe { CStr::from_ptr(ffi::dm_version()) }
            .to_str()
            .unwrap(),
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(ffi::dm_abi_version(), 1);

    unsafe { ffi::dm_stop(handle) };
    // 重复 stop / stop(NULL) 必须安全（幂等）。
    unsafe { ffi::dm_stop(handle) };
    unsafe { ffi::dm_stop(std::ptr::null_mut()) };
}
