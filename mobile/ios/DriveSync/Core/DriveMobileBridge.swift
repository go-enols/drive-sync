//
//  DriveMobileBridge.swift
//  drive-sync iOS
//
//  drive-mobile C ABI 的 Swift 薄封装（crates/mobile-core/include/drive_mobile.h）。
//  这里只做三件事：
//    1. C 指针/字符串的所有权转换（Rust 分配的字符串必须 dm_string_free）；
//    2. C 回调（可能来自 Rust 后台线程）→ Swift 对象；
//    3. 供 RustMobileCore 调用的强类型入口。
//
//  业务模型与 JSON 解析在 MobileCoreModels.swift / RustMobileCore.swift。
//

import Foundation

/// `drive_mobile.h` 的静态 C 入口。
enum DriveMobileBridge {
    /// 库版本（静态字符串，不需要释放）。
    static var libraryVersion: String {
        guard let pointer = dm_version() else { return "unknown" }
        return String(cString: pointer)
    }

    /// C ABI 版本（DM_ABI_VERSION）。
    static var abiVersion: Int32 {
        dm_abi_version()
    }

    /// 线程本地最近一次错误（静态字符串，不要释放）。
    static func lastError() -> String? {
        guard let pointer = dm_last_error() else { return nil }
        return String(cString: pointer)
    }

    // MARK: - 身份 / 配置

    /// `dm_init`：初始化（或加载）设备身份，返回 `{"ok":true,...}` JSON。
    static func initializeIdentity(dataDirectory: String, deviceName: String) -> String? {
        dataDirectory.withCString { directoryPointer in
            deviceName.withCString { namePointer in
                consume(dm_init(directoryPointer, namePointer))
            }
        }
    }

    /// `dm_load`：只加载已存在的身份。
    static func loadIdentity(dataDirectory: String) -> String? {
        dataDirectory.withCString { directoryPointer in
            consume(dm_load(directoryPointer))
        }
    }

    /// `dm_config_set_relay`：保存 relay/token/设备名。
    static func saveConfiguration(json: String) -> String? {
        json.withCString { jsonPointer in
            consume(dm_config_set_relay(jsonPointer))
        }
    }

    // MARK: - 运行时

    /// `dm_start`：启动 Rust 后台运行时（连接 + 自动重连），立即返回句柄。
    static func start(relayURL: String, token: String, userData: UnsafeMutableRawPointer) -> OpaquePointer? {
        relayURL.withCString { relayPointer in
            token.withCString { tokenPointer in
                dm_start(relayPointer, tokenPointer, dmClipboardCallback, dmEventCallback, userData)
            }
        }
    }

    /// `dm_stop`：停止运行时（最多等待约 3 秒）。
    static func stop(_ handle: OpaquePointer?) {
        dm_stop(handle)
    }

    /// `dm_send_clipboard_text`：0 成功，-1 句柄无效，-2 文本非法，-3 未连接（进离线队列）。
    static func sendClipboardText(_ handle: OpaquePointer?, text: String) -> Int32 {
        text.withCString { textPointer in
            dm_send_clipboard_text(handle, textPointer)
        }
    }

    /// `dm_set_paused`：暂停/恢复 Rust 侧剪切板同步（不影响连接与目录同步）。
    /// iOS 侧暂不调用：iOS 的剪贴板读写由 Swift 完成，该暂停语义主要服务 Android JNI 自动剪贴板。
    static func setPaused(_ handle: OpaquePointer?, paused: Bool) -> Int32 {
        dm_set_paused(handle, paused ? 1 : 0)
    }

    // MARK: - 配对 / 查询（阻塞，必须后台线程调用）

    static func pairStart(_ handle: OpaquePointer?) -> String? {
        consume(dm_pair_start(handle))
    }

    static func pairJoin(_ handle: OpaquePointer?, codeOrQR: String) -> String? {
        codeOrQR.withCString { pointer in
            consume(dm_pair_join(handle, pointer))
        }
    }

    static func peers(_ handle: OpaquePointer?) -> String? {
        consume(dm_peers_json(handle))
    }

    static func status(_ handle: OpaquePointer?) -> String? {
        consume(dm_status_json(handle))
    }

    static func catalog(_ handle: OpaquePointer?) -> String? {
        consume(dm_catalog_json(handle))
    }

    // MARK: - 字符串所有权

    /// 消费 Rust 返回的 `char *`：拷贝成 Swift String 后立即 `dm_string_free`。
    /// Rust 分配的字符串归调用方所有，禁止 free()/delete。
    @discardableResult
    static func consume(_ pointer: UnsafeMutablePointer<CChar>?) -> String? {
        guard let pointer else { return nil }
        defer { dm_string_free(pointer) }
        return String(cString: pointer)
    }
}

// MARK: - C 回调入口（Rust 后台线程调用）

/// 事件回调：`{"type":"...",...}` JSON。
private let dmEventCallback: dm_event_cb = { eventJSON, userData in
    guard let eventJSON, let userData else { return }
    let context = Unmanaged<CallbackContext>.fromOpaque(userData).takeUnretainedValue()
    context.owner?.receive(eventJSON: String(cString: eventJSON))
}

/// 远端剪贴板文本回调。参数仅在回调期间有效，所以要立刻转成 Swift String。
private let dmClipboardCallback: dm_clipboard_cb = { textPointer, fromPointer, userData in
    guard let textPointer, let userData else { return }
    let context = Unmanaged<CallbackContext>.fromOpaque(userData).takeUnretainedValue()
    let text = String(cString: textPointer)
    let from = fromPointer.map { String(cString: $0) } ?? ""
    context.owner?.receive(clipboardText: text, fromDeviceId: from)
}
