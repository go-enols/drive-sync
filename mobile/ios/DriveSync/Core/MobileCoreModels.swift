//
//  MobileCoreModels.swift
//  drive-sync iOS
//
//  drive-mobile C ABI（crates/mobile-core/include/drive_mobile.h）返回 JSON 对应的模型。
//  所有字段名与 Rust 侧 serde 输出一致（snake_case），见头文件注释。
//

import Foundation

// MARK: - 错误

/// C ABI 调用失败。`api` 是 Rust 返回的 `{"ok":false,"error":"..."}` 里的业务错误。
enum MobileCoreError: LocalizedError, Equatable, Sendable {
    case notInitialized
    case notRunning
    case api(String)
    case decoding(String)

    var errorDescription: String? {
        switch self {
        case .notInitialized:
            return "尚未初始化设备身份"
        case .notRunning:
            return "同步尚未启动，请先在「连接」页启动"
        case .api(let message):
            return message
        case .decoding(let message):
            return "解析 Rust 返回值失败：\(message)"
        }
    }
}

// MARK: - 身份与配置

/// 本机（或对端）公开身份：`device_id = "dev-" + hex(BLAKE3(ed_pub))[..16]`。
struct DeviceIdentity: Codable, Equatable, Sendable {
    let deviceId: String
    let name: String

    enum CodingKeys: String, CodingKey {
        case deviceId = "device_id"
        case name
    }
}

/// `dm_config_set_relay` 成功后的配置回显。
struct RelayConfigState: Equatable, Sendable {
    let relayURL: String
    let deviceName: String
    let hasToken: Bool
}

// MARK: - 配对

/// `dm_pair_start` 返回的 8 位配对码 + 二维码载荷（`dsync-pair-v1:...`）。
struct PairCodeInfo: Equatable, Sendable {
    let code: String
    let qr: String

    /// 按 4+4 分组显示，便于口头转述与两端核对（如 `1234 5678`）。
    var formattedCode: String {
        let digits = code.filter { $0.isNumber }
        guard digits.count == 8 else { return code }
        let middle = digits.index(digits.startIndex, offsetBy: 4)
        return String(digits[..<middle]) + " " + String(digits[middle...])
    }
}

/// `dm_pair_join` 成功结果；SAS 在原型阶段由库自动确认，仅用于展示。
struct PairJoinInfo: Equatable, Sendable {
    let peerId: String
    let peerName: String
    let sas: String
}

// MARK: - 已配对设备 / 运行状态

/// `dm_peers_json` 中 `peers[]` 的一项。
struct PeerInfo: Decodable, Identifiable, Equatable, Sendable {
    let deviceId: String
    let name: String
    let label: String?
    let relayURL: String?
    let pairedAtMs: Int64?
    let online: Bool?
    let session: Bool?

    var id: String { deviceId }
    var displayName: String { name.isEmpty ? deviceId : name }
    var isOnline: Bool { online ?? false }
    var sessionActive: Bool { session ?? false }

    var pairedAtText: String {
        guard let pairedAtMs, pairedAtMs > 0 else { return "-" }
        let date = Date(timeIntervalSince1970: TimeInterval(pairedAtMs) / 1000.0)
        return PeerInfo.pairedAtFormatter.string(from: date)
    }

    private static let pairedAtFormatter: DateFormatter = {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "zh_CN")
        formatter.dateFormat = "yyyy-MM-dd HH:mm"
        return formatter
    }()

    enum CodingKeys: String, CodingKey {
        case deviceId = "device_id"
        case name
        case label
        case relayURL = "relay_url"
        case pairedAtMs = "paired_at_ms"
        case online
        case session
    }
}

/// `dm_peers_json` 整体结果。
struct PeersSnapshot: Equatable, Sendable {
    let selfIdentity: DeviceIdentity?
    let peers: [PeerInfo]

    static let empty = PeersSnapshot(selfIdentity: nil, peers: [])
}

/// `dm_status_json` 的 Swift 镜像。
struct RuntimeStatus: Equatable, Sendable {
    let running: Bool
    let active: Bool
    let deviceId: String
    let name: String
    let relayURL: String
    let dataDir: String
    let peersOnline: Int
    let pairing: Bool
    let lastClipboardLength: Int

    static let empty = RuntimeStatus(
        running: false,
        active: false,
        deviceId: "",
        name: "",
        relayURL: "",
        dataDir: "",
        peersOnline: 0,
        pairing: false,
        lastClipboardLength: 0
    )
}

// MARK: - 软件目录

/// `dm_catalog_json` 中 `entries[]` 的一项（只读展示）。
struct CatalogItem: Decodable, Identifiable, Equatable, Sendable {
    let id: String
    let name: String
    let version: String?
    let platform: String?
    let kind: String?
    let size: Int64?
    let notes: String?
    let updatedAtMs: Int64?
    let local: Bool?
    let holders: [String]?

    var displayName: String { name.isEmpty ? id : name }
    var isLocal: Bool { local ?? false }
    var holderCount: Int { holders?.count ?? 0 }
    var displaySize: String {
        guard let size, size > 0 else { return "-" }
        return ByteCountFormatter.string(fromByteCount: size, countStyle: .file)
    }
    var platformText: String { platform?.isEmpty == false ? platform! : "-" }

    enum CodingKeys: String, CodingKey {
        case id
        case name
        case version
        case platform
        case kind
        case size
        case notes
        case local
        case holders
        case updatedAtMs = "updated_at_ms"
    }
}

/// `dm_catalog_json` 整体结果。
struct CatalogSnapshot: Equatable, Sendable {
    let revision: Int
    let entries: [CatalogItem]

    static let empty = CatalogSnapshot(revision: 0, entries: [])
}

// MARK: - 剪贴板

/// `dm_clipboard_cb` 回调给 Swift 的远端文本（内容由库解密后传入）。
struct RemoteClipboard: Equatable, Sendable {
    let text: String
    let fromDeviceId: String
    let receivedAt: Date
}

/// 界面「接收记录」列表项。
struct ReceivedItem: Identifiable, Equatable, Sendable {
    let id = UUID()
    let text: String
    let peerId: String
    let peerName: String
    let kind: String
    let receivedAt: Date

    var characterCount: Int { text.count }
}

/// `dm_send_clipboard_text` 返回值（0 = 已投递，-3 = 进离线队列待重试）。
enum ClipboardSendResult: Equatable, Sendable {
    case sent
    case queuedOffline
    case invalidHandle
    case invalidText
    case other(Int32)

    init(code: Int32) {
        switch code {
        case 0: self = .sent
        case -1: self = .invalidHandle
        case -2: self = .invalidText
        case -3: self = .queuedOffline
        default: self = .other(code)
        }
    }

    var isSuccess: Bool {
        switch self {
        case .sent, .queuedOffline: return true
        default: return false
        }
    }

    var message: String {
        switch self {
        case .sent:
            return "剪贴板已加密发送给在线设备"
        case .queuedOffline:
            return "暂未连接/对端离线，内容已进入离线队列，稍后自动重试"
        case .invalidHandle:
            return "同步运行时未启动或已停止"
        case .invalidText:
            return "文本为空或超过单条上限（32 MiB）"
        case .other(let code):
            return "发送剪贴板失败（错误码 \(code)）"
        }
    }
}

// MARK: - 事件

/// `dm_event_cb` 回调的事件：`{"type":"...", ...}`。
///
/// 事件字段是可信的本机 Rust 核心输出（非网络数据），这里只做轻量字典解析，
/// 避免为每个事件定义结构体；新增字段不会导致解码失败。
struct MobileEvent: Equatable, Sendable {
    let type: String
    let raw: String
    let fields: [String: String]

    static func parse(_ raw: String) -> MobileEvent? {
        guard let data = raw.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data),
              let dictionary = object as? [String: Any],
              let type = dictionary["type"] as? String else {
            return nil
        }
        var fields: [String: String] = [:]
        for (key, value) in dictionary where key != "type" {
            if let string = value as? String {
                fields[key] = string
            } else if let number = value as? NSNumber {
                fields[key] = number.stringValue
            } else if let array = value as? [Any] {
                fields[key] = array.map { String(describing: $0) }.joined(separator: ",")
            }
        }
        return MobileEvent(type: type.lowercased(), raw: raw, fields: fields)
    }

    func string(_ key: String) -> String? {
        guard let value = fields[key], !value.isEmpty, value != "null" else { return nil }
        return value
    }

    func int(_ key: String) -> Int? {
        guard let value = fields[key] else { return nil }
        return Int(value)
    }

    func bool(_ key: String) -> Bool? {
        guard let value = fields[key] else { return nil }
        if value == "true" || value == "1" {
            return true
        }
        if value == "false" || value == "0" {
            return false
        }
        return nil
    }

    var peerId: String? { string("peer_id") ?? string("from") ?? string("from_device_id") }
    var peerName: String? { string("name") }
    var message: String? { string("message") ?? string("error") }
    var code: String? { string("code") }
    var qr: String? { string("qr") }
    var sas: String? { string("sas") }

    /// `connected` / `presence` 事件的在线设备数（`online` 是 device_id 数组）。
    var onlineCount: Int? {
        guard let value = fields["online"] else { return nil }
        if let number = Int(value) { return number }
        let parts = value.split(separator: ",").filter { !$0.isEmpty }
        return parts.count
    }
}

// MARK: - Rust 返回 JSON 的解码 DTO（字段缺失容忍）

struct InitResponse: Decodable {
    let ok: Bool?
    let created: Bool?
    let deviceId: String?
    let name: String?
    let error: String?

    enum CodingKeys: String, CodingKey {
        case ok, created, name, error
        case deviceId = "device_id"
    }
}

struct ConfigResponse: Decodable {
    let ok: Bool?
    let relayURL: String?
    let deviceName: String?
    let hasToken: Bool?
    let error: String?

    enum CodingKeys: String, CodingKey {
        case ok, error
        case relayURL = "relay_url"
        case deviceName = "device_name"
        case hasToken = "has_token"
    }
}

struct PairStartResponse: Decodable {
    let ok: Bool?
    let code: String?
    let qr: String?
    let error: String?
}

struct PairJoinResponse: Decodable {
    let ok: Bool?
    let peerId: String?
    let name: String?
    let sas: String?
    let error: String?

    enum CodingKeys: String, CodingKey {
        case ok, name, sas, error
        case peerId = "peer_id"
    }
}

struct PeersResponse: Decodable {
    let ok: Bool?
    let selfInfo: IdentityDTO?
    let peers: [PeerInfo]?
    let error: String?

    enum CodingKeys: String, CodingKey {
        case ok, peers, error
        case selfInfo = "self"
    }
}

struct IdentityDTO: Decodable {
    let deviceId: String?
    let name: String?

    enum CodingKeys: String, CodingKey {
        case deviceId = "device_id"
        case name
    }

    var identity: DeviceIdentity? {
        guard let deviceId, let name else { return nil }
        return DeviceIdentity(deviceId: deviceId, name: name)
    }
}

struct StatusResponse: Decodable {
    let ok: Bool?
    let running: Bool?
    let active: Bool?
    let deviceId: String?
    let name: String?
    let relayURL: String?
    let dataDir: String?
    let peersOnline: Int?
    let pairing: Bool?
    let lastClipboardLen: Int?
    let error: String?

    enum CodingKeys: String, CodingKey {
        case ok, running, active, name, pairing, error
        case deviceId = "device_id"
        case relayURL = "relay_url"
        case dataDir = "data_dir"
        case peersOnline = "peers_online"
        case lastClipboardLen = "last_clipboard_len"
    }

    var runtimeStatus: RuntimeStatus {
        RuntimeStatus(
            running: running ?? false,
            active: active ?? false,
            deviceId: deviceId ?? "",
            name: name ?? "",
            relayURL: relayURL ?? "",
            dataDir: dataDir ?? "",
            peersOnline: peersOnline ?? 0,
            pairing: pairing ?? false,
            lastClipboardLength: lastClipboardLen ?? 0
        )
    }
}

struct CatalogResponse: Decodable {
    let ok: Bool?
    let revision: Int?
    let count: Int?
    let entries: [CatalogItem]?
    let error: String?
}

// MARK: - 日志

/// 「事件日志」一行。
struct LogLine: Identifiable, Equatable, Sendable {
    enum Level: String, Sendable {
        case info
        case success
        case warning
        case failure
    }

    let id = UUID()
    let date: Date
    let text: String
    let level: Level
}
