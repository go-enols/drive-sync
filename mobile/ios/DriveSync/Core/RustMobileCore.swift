//
//  RustMobileCore.swift
//  drive-sync iOS
//
//  drive-mobile（Rust staticlib，libdrive_mobile.a）的 Swift 门面。
//  - 网络、加密、配对、存储、自动重连全部在 Rust 侧完成；
//  - 这里负责 JSON 编解码、回调桥接、句柄生命周期；
//  - 阻塞接口（dm_pair_* / dm_peers_json / dm_status_json / dm_catalog_json）
//    必须在后台线程调用，AppModel 统一用 Task.detached 调用它们。
//

import Foundation

/// 传给 C 回调的 `user_data`，回指 Swift 门面对象（弱引用避免循环）。
final class CallbackContext {
    weak var owner: RustMobileCore?

    init(owner: RustMobileCore) {
        self.owner = owner
    }
}

/// `drive_mobile.h` 的面向对象封装。线程安全：句柄只在主线程启动/停止；
/// 回调可来自 Rust 任意后台线程（`receive` 内部只做解析后转发）。
final class RustMobileCore: @unchecked Sendable {
    /// App 数据目录（identity.json / peers.json / spool 等都在这里）。
    let dataDirectory: String

    private(set) var handle: OpaquePointer?
    private var callbackContext: CallbackContext?
    private var onEvent: (@Sendable (MobileEvent) -> Void)?
    private var onClipboard: (@Sendable (RemoteClipboard) -> Void)?

    init(dataDirectory: String) {
        self.dataDirectory = dataDirectory
        try? FileManager.default.createDirectory(
            atPath: dataDirectory,
            withIntermediateDirectories: true,
            attributes: nil
        )
    }

    /// 同步运行时是否已启动（不保证已连接网络）。
    var isRunning: Bool {
        handle != nil
    }

    // MARK: - 回调注册

    func setCallbacks(
        onEvent: (@Sendable (MobileEvent) -> Void)?,
        onClipboard: (@Sendable (RemoteClipboard) -> Void)?
    ) {
        self.onEvent = onEvent
        self.onClipboard = onClipboard
    }

    /// Rust 事件回调入口（任意后台线程）。
    func receive(eventJSON: String) {
        guard let event = MobileEvent.parse(eventJSON) else { return }
        onEvent?(event)
    }

    /// Rust 剪贴板回调入口（任意后台线程）。
    func receive(clipboardText: String, fromDeviceId: String) {
        let clipboard = RemoteClipboard(
            text: clipboardText,
            fromDeviceId: fromDeviceId,
            receivedAt: Date()
        )
        onClipboard?(clipboard)
    }

    // MARK: - 身份 / 配置（dm_init / dm_load / dm_config_set_relay）

    /// 初始化（不存在则创建）设备身份并设置数据目录。
    func loadOrCreateIdentity(deviceName: String) -> Result<DeviceIdentity, MobileCoreError> {
        let json = DriveMobileBridge.initializeIdentity(
            dataDirectory: dataDirectory,
            deviceName: deviceName
        )
        guard let response = decode(InitResponse.self, fromJSON: json) else { return .failure(decodeError(json)) }
        if let error = businessError(ok: response.ok, error: response.error, fallback: "初始化设备身份失败") {
            return .failure(error)
        }
        guard let deviceId = response.deviceId, let name = response.name else {
            return .failure(.decoding("dm_init 返回值缺少 device_id / name"))
        }
        return .success(DeviceIdentity(deviceId: deviceId, name: name))
    }

    /// 只加载已有设备身份（不创建）。
    func loadIdentity() -> Result<DeviceIdentity, MobileCoreError> {
        let json = DriveMobileBridge.loadIdentity(dataDirectory: dataDirectory)
        guard let response = decode(InitResponse.self, fromJSON: json) else { return .failure(decodeError(json)) }
        if let error = businessError(ok: response.ok, error: response.error, fallback: "加载设备身份失败") {
            return .failure(error)
        }
        guard let deviceId = response.deviceId, let name = response.name else {
            return .failure(.decoding("dm_load 返回值缺少 device_id / name"))
        }
        return .success(DeviceIdentity(deviceId: deviceId, name: name))
    }

    /// 保存 relay 地址 / token / 设备名到 `<data_dir>/config.json`。
    func saveConfig(relayURL: String, token: String, deviceName: String) -> Result<RelayConfigState, MobileCoreError> {
        let payload: [String: String] = [
            "relay_url": relayURL,
            "access_token": token,
            "device_name": deviceName
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload, options: []),
              let json = String(data: data, encoding: .utf8) else {
            return .failure(.decoding("配置 JSON 编码失败"))
        }
        let responseJSON = DriveMobileBridge.saveConfiguration(json: json)
        guard let response = decode(ConfigResponse.self, fromJSON: responseJSON) else {
            return .failure(decodeError(responseJSON))
        }
        if let error = businessError(ok: response.ok, error: response.error, fallback: "保存中继配置失败") {
            return .failure(error)
        }
        return .success(
            RelayConfigState(
                relayURL: response.relayURL ?? relayURL,
                deviceName: response.deviceName ?? deviceName,
                hasToken: response.hasToken ?? !token.isEmpty
            )
        )
    }

    // MARK: - 运行时（dm_start / dm_stop / dm_send_clipboard_text）

    /// 启动 Rust 后台运行时（连接 + 自动重连 + 同步），立即返回。
    func start(relayURL: String, token: String) -> Result<Bool, MobileCoreError> {
        let context = CallbackContext(owner: self)
        callbackContext = context
        let userData = Unmanaged.passUnretained(context).toOpaque()
        guard let newHandle = DriveMobileBridge.start(relayURL: relayURL, token: token, userData: userData) else {
            callbackContext = nil
            let message = DriveMobileBridge.lastError() ?? "dm_start 返回 NULL"
            return .failure(.api(message))
        }
        handle = newHandle
        return .success(true)
    }

    /// 停止运行时并释放句柄（幂等；最多阻塞约 3 秒）。
    func stop() {
        DriveMobileBridge.stop(handle)
        handle = nil
        callbackContext = nil
    }

    /// 发送本机剪贴板文本（调用方必须已经由用户手势读取剪贴板）。
    func sendClipboard(text: String) -> ClipboardSendResult {
        let code = DriveMobileBridge.sendClipboardText(handle, text: text)
        return ClipboardSendResult(code: code)
    }

    /// 暂停/恢复 Rust 侧剪切板同步（C ABI 提供；iOS 目前由 AppModel/Swift 自行控制，保留封装供后续使用）。
    @discardableResult
    func setPaused(_ paused: Bool) -> Int32 {
        DriveMobileBridge.setPaused(handle, paused: paused)
    }

    // MARK: - 配对（阻塞，后台线程调用）

    func pairStart() -> Result<PairCodeInfo, MobileCoreError> {
        guard handle != nil else { return .failure(.notRunning) }
        let json = DriveMobileBridge.pairStart(handle)
        guard let response = decode(PairStartResponse.self, fromJSON: json) else { return .failure(decodeError(json)) }
        if let error = businessError(ok: response.ok, error: response.error, fallback: "发起配对失败") {
            return .failure(error)
        }
        guard let code = response.code, !code.isEmpty else {
            return .failure(.decoding("dm_pair_start 返回值缺少 code"))
        }
        return .success(PairCodeInfo(code: code, qr: response.qr ?? ""))
    }

    func pairJoin(codeOrQR: String) -> Result<PairJoinInfo, MobileCoreError> {
        guard handle != nil else { return .failure(.notRunning) }
        let json = DriveMobileBridge.pairJoin(handle, codeOrQR: codeOrQR)
        guard let response = decode(PairJoinResponse.self, fromJSON: json) else { return .failure(decodeError(json)) }
        if let error = businessError(ok: response.ok, error: response.error, fallback: "加入配对失败") {
            return .failure(error)
        }
        return .success(
            PairJoinInfo(
                peerId: response.peerId ?? "",
                peerName: response.name ?? "对端设备",
                sas: response.sas ?? ""
            )
        )
    }

    // MARK: - 查询（阻塞，后台线程调用）

    func peers() -> Result<PeersSnapshot, MobileCoreError> {
        guard handle != nil else { return .failure(.notRunning) }
        let json = DriveMobileBridge.peers(handle)
        guard let response = decode(PeersResponse.self, fromJSON: json) else { return .failure(decodeError(json)) }
        if let error = businessError(ok: response.ok, error: response.error, fallback: "读取已配对设备失败") {
            return .failure(error)
        }
        return .success(
            PeersSnapshot(
                selfIdentity: response.selfInfo?.identity,
                peers: response.peers ?? []
            )
        )
    }

    func status() -> Result<RuntimeStatus, MobileCoreError> {
        guard handle != nil else { return .failure(.notRunning) }
        let json = DriveMobileBridge.status(handle)
        guard let response = decode(StatusResponse.self, fromJSON: json) else { return .failure(decodeError(json)) }
        if let error = businessError(ok: response.ok, error: response.error, fallback: "读取运行状态失败") {
            return .failure(error)
        }
        return .success(response.runtimeStatus)
    }

    func catalog() -> Result<CatalogSnapshot, MobileCoreError> {
        guard handle != nil else { return .failure(.notRunning) }
        let json = DriveMobileBridge.catalog(handle)
        guard let response = decode(CatalogResponse.self, fromJSON: json) else { return .failure(decodeError(json)) }
        if let error = businessError(ok: response.ok, error: response.error, fallback: "读取软件目录失败") {
            return .failure(error)
        }
        return .success(
            CatalogSnapshot(
                revision: response.revision ?? 0,
                entries: response.entries ?? []
            )
        )
    }

    // MARK: - JSON 解码辅助

    private func decode<T: Decodable>(_ type: T.Type, fromJSON json: String?) -> T? {
        guard let json, let data = json.data(using: .utf8) else { return nil }
        return try? JSONDecoder().decode(type, from: data)
    }

    private func decodeError(_ json: String?) -> MobileCoreError {
        if let json, !json.isEmpty {
            return .decoding("无法解析：\(json.prefix(180))")
        }
        return .api(DriveMobileBridge.lastError() ?? "Rust 核心返回 NULL")
    }

    /// `{"ok":false,"error":"..."}` → 业务错误；ok 缺省视为成功（容忍旧版核心）。
    private func businessError(ok: Bool?, error: String?, fallback: String) -> MobileCoreError? {
        guard ok == false else { return nil }
        if let error, !error.isEmpty {
            return .api(error)
        }
        return .api(fallback)
    }
}

extension RustMobileCore {
    /// 仅供调试/关于页展示。
    static var coreDescription: String {
        "libdrive_mobile \(DriveMobileBridge.libraryVersion) · ABI \(DriveMobileBridge.abiVersion)"
    }
}
