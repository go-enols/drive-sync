//
//  DriveSync-Bridging-Header.h
//  drive-sync iOS
//
//  把 drive-mobile 的 C ABI 暴露给 Swift。
//  头文件契约来自 crates/mobile-core/include/drive_mobile.h（Android/iOS 共用）。
//  HEADER_SEARCH_PATHS 已在 project.yml 里指向该目录（见 docs/mobile-ios.md）。
//
//  纯 C 接口：JSON 字符串进/出 + 事件回调；网络/加密/存储全部在 Rust 侧完成，
//  Swift 只负责 UI 与系统剪贴板。
//

#import "drive_mobile.h"
