//! drive-sync 移动端核心（Android / iOS 共用）。
//!
//! - [`ffi`]：稳定 C ABI（`include/drive_mobile.h`），Android JNI 壳与 iOS Swift 桥共用；
//! - [`runtime`]：后台同步运行时封装（tokio runtime + drive-client Runtime + 事件分发）；
//! - [`jni`]：Android JNI 导出符号（`dev.drivesync.mobile.Native`），仅非 iOS 目标编译。
//!
//! 设计约束：移动端只有一个设备身份、一个活动同步会话；所有网络/磁盘操作在后台线程，
//! C ABI 调用方不得在 UI 线程调用会阻塞的接口（`dm_pair_*` / `dm_*_json`）。
#![allow(clippy::missing_safety_doc)]

pub mod ffi;
#[cfg(not(target_os = "ios"))]
pub mod jni;
pub mod runtime;

/// 移动端核心版本（与 workspace 版本一致）。
pub const MOBILE_VERSION: &str = env!("CARGO_PKG_VERSION");
