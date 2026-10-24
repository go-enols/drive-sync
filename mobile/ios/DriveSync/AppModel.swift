//
//  AppModel.swift
//  drive-sync iOS
//
//  界面状态机：持有 RustMobileCore，把所有阻塞 C ABI 调用放到后台线程，
//  把 Rust 回调切回主线程后再更新 @Published 状态。
//
//  iOS 平台现实（产品行为，详见 docs/mobile-ios.md §9）：
//    - 前台无感同步：轮询剪贴板自动发送、收到内容自动写剪贴板；
//    - 只有前台保持 WebSocket，后台保留约 25 秒接收窗口后断开（无 APNs 推送）；
//    - iOS 16+ 首次读取剪贴板需系统「允许粘贴」授权，无法预授权/查询。
//

import Combine
import Foundation
import SwiftUI
import UIKit
import UserNotifications

@MainActor
final class AppModel: ObservableObject {
    // MARK: - 类型

    enum Tab: Hashable {
        case connect
        case pairing
        case sync
        case catalog
        case settings
    }

    /// 外观主题：跟随系统（默认）/ 深色 / 浅色；持久化到 UserDefaults。
    enum AppearanceMode: String, CaseIterable, Equatable {
        case system
        case light
        case dark

        var title: String {
            switch self {
            case .system: return "跟随系统"
            case .light: return "浅色"
            case .dark: return "深色"
            }
        }

        var detail: String {
            switch self {
            case .system: return "随 iOS 系统外观自动切换"
            case .light: return "浅色背景（#F5F7FA）"
            case .dark: return "深色背景（#0B0F14）"
            }
        }

        var icon: String {
            switch self {
            case .system: return "circle.lefthalf.filled"
            case .light: return "sun.max.fill"
            case .dark: return "moon.fill"
            }
        }

        /// 传给 `.preferredColorScheme`：nil = 跟随系统。
        var colorScheme: ColorScheme? {
            switch self {
            case .system: return nil
            case .light: return .light
            case .dark: return .dark
            }
        }
    }

    /// 前台连接状态机：未初始化 → 未连接 → 连接中 → 已连接；后台暂停。
    enum Phase: Equatable {
        case uninitialized
        case offline
        case connecting
        case online
        case paused
        case failed

        var title: String {
            switch self {
            case .uninitialized: return "未初始化"
            case .offline: return "未连接"
            case .connecting: return "连接中…"
            case .online: return "已连接"
            case .paused: return "后台已暂停"
            case .failed: return "连接失败"
            }
        }

        var detail: String {
            switch self {
            case .uninitialized: return "先初始化本机设备身份"
            case .offline: return "仅前台保持连接，切到后台会自动断开"
            case .connecting: return "正在连接自建中继并恢复加密会话…"
            case .online: return "剪贴板可以收发（需用户手势）"
            case .paused: return "后台接收窗口已结束/已断开，回到前台后自动重连"
            case .failed: return "请检查中继地址、访问令牌与网络"
            }
        }

        var isOnline: Bool { self == .online }

        var color: Color {
            switch self {
            case .uninitialized: return Theme.textTertiary
            case .offline: return Theme.warning
            case .connecting: return Theme.mint
            case .online: return Theme.mint
            case .paused: return Theme.textSecondary
            case .failed: return Theme.danger
            }
        }
    }

    /// 通知权限状态（Onboarding / 权限页展示）。
    enum NotificationPermissionState: Equatable {
        case notDetermined
        case denied
        case authorized
        case provisional
        case unknown

        var title: String {
            switch self {
            case .notDetermined: return "未请求"
            case .denied: return "已拒绝"
            case .authorized: return "已开启"
            case .provisional: return "安静通知"
            case .unknown: return "未知"
            }
        }

        var color: Color {
            switch self {
            case .notDetermined: return Theme.textSecondary
            case .denied: return Theme.danger
            case .authorized: return Theme.mint
            case .provisional: return Theme.mint
            case .unknown: return Theme.textTertiary
            }
        }

        /// 需要展示的操作按钮文字（nil 表示无需操作）。
        var actionTitle: String? {
            switch self {
            case .notDetermined: return "请求通知权限"
            case .denied: return "去系统设置"
            default: return nil
            }
        }
    }

    /// 顶部浮层提示。
    struct Toast: Identifiable, Equatable {
        let id = UUID()
        let text: String
        let level: LogLine.Level
    }

    // MARK: - 界面状态

    @Published var selectedTab: Tab = .connect
    /// 外观主题（跟随系统 / 深色 / 浅色），默认跟随系统。
    @Published private(set) var appearanceMode: AppearanceMode = .system

    /// 根视图 `.preferredColorScheme` 的取值：nil 表示跟随系统。
    var preferredColorScheme: ColorScheme? { appearanceMode.colorScheme }

    /// 切换外观主题（立即生效并持久化）。
    func setAppearanceMode(_ mode: AppearanceMode) {
        guard appearanceMode != mode else { return }
        appearanceMode = mode
        UserDefaults.standard.set(mode.rawValue, forKey: DefaultsKey.appearanceMode)
        appendLog("外观主题已切换为「\(mode.title)」", level: .info)
    }

    /// 打开应用时是否自动连接（默认开启，可在设置页关闭）。
    @Published private(set) var autoConnectOnLaunch: Bool = true

    /// 中继配置是否完整：设备身份 + 中继地址 + 访问令牌。
    var isConfigurationComplete: Bool {
        identity != nil && !relayURL.trimmed.isEmpty && !accessToken.trimmed.isEmpty
    }

    /// 启动/引导完成后是否应该自动连接。
    var shouldAutoConnectOnLaunch: Bool {
        onboardingCompleted && autoConnectOnLaunch && isConfigurationComplete && !core.isRunning && !connecting
    }

    /// 设置「打开应用时自动连接」。
    func setAutoConnectOnLaunch(_ enabled: Bool) {
        autoConnectOnLaunch = enabled
        UserDefaults.standard.set(enabled, forKey: DefaultsKey.autoConnectOnLaunch)
        appendLog(
            enabled ? "已开启：打开应用时自动连接中继" : "已关闭：打开应用时不再自动连接，可手动连接",
            level: .info
        )
    }

    @Published var relayURL: String = ""
    @Published var accessToken: String = ""
    @Published var deviceName: String = UIDevice.current.name
    @Published private(set) var phase: Phase = .uninitialized
    @Published private(set) var identity: DeviceIdentity?
    @Published private(set) var dataDirectory: String = ""
    @Published private(set) var peers: [PeerInfo] = []
    @Published private(set) var status: RuntimeStatus = .empty
    @Published private(set) var catalog: CatalogSnapshot = .empty
    @Published private(set) var pairCode: PairCodeInfo?
    @Published private(set) var lastPairJoin: PairJoinInfo?
    @Published private(set) var pairingBusy = false
    @Published private(set) var connecting = false
    @Published private(set) var receivedItems: [ReceivedItem] = []
    @Published private(set) var logs: [LogLine] = []
    @Published private(set) var clipboardPreview: String = ""
    @Published private(set) var lastSentAt: Date?
    @Published private(set) var lastAppliedAt: Date?
    @Published private(set) var abiInfo: String = ""
    /// 前台自动同步剪贴板（发送方向）开关，默认开启。
    @Published private(set) var autoSyncEnabled: Bool = true
    /// 前台收到远端内容后自动写入系统剪贴板，默认开启。
    @Published private(set) var autoApplyReceivedEnabled: Bool = true
    /// 自动读取被系统粘贴授权拦截（需用户手动兜底一次）。
    @Published private(set) var autoReadBlocked = false
    @Published var toast: Toast?
    @Published var errorText: String?

    // MARK: - 首次引导 / 权限

    /// 是否展示首次引导（Onboarding）；也用于「权限与平台说明」入口。
    @Published var showOnboarding = false
    /// 是否已完成首次引导（持久化到 UserDefaults，之后不再自动弹出）。
    @Published private(set) var onboardingCompleted = false
    /// 通知权限状态（Onboarding / 权限页展示）。
    @Published private(set) var notificationPermission: NotificationPermissionState = .unknown

    // MARK: - 内部状态

    /// Rust 门面（网络/加密/配对/存储都在 Rust 侧）。
    let core: RustMobileCore

    private var wishConnected = false
    private var bootstrapped = false
    private var pollTask: Task<Void, Never>?
    private var pollTick = 0
    private var isAppActive = true
    private var backgroundGraceTask: Task<Void, Never>?
    private var backgroundTaskID: UIBackgroundTaskIdentifier = .invalid

    /// 剪贴板回环抑制（远端写入 → 不再发回）。
    private let clipboardGuard = ClipboardLoopGuard()
    /// UIPasteboard.changeCount：iOS 无变更通知，靠它检测剪贴板变化。
    private var lastObservedChangeCount = -1
    /// 自动读取被系统拦截后的退避时间，避免反复弹授权框。
    private var lastPromptFailureAt: Date?
    /// App 不在前台时收到、等回到前台自动写入的内容。
    private var pendingClipboard: RemoteClipboard?

    // MARK: - 生命周期

    init() {
        let directory = AppPaths.dataDirectory
        self.dataDirectory = directory
        self.core = RustMobileCore(dataDirectory: directory)
        self.abiInfo = RustMobileCore.coreDescription
        loadPersistedSettings()
        core.setCallbacks(
            onEvent: { [weak self] event -> Void in
                Task { @MainActor in
                    self?.handle(event: event)
                }
            },
            onClipboard: { [weak self] clipboard -> Void in
                Task { @MainActor in
                    self?.handleRemoteClipboard(clipboard)
                }
            }
        )
    }

    /// App 启动后调用一次：加载或创建设备身份、读取持久化配置。
    func bootstrap() async {
        guard !bootstrapped else { return }
        bootstrapped = true

        let core = self.core
        let loaded = await runOffMain { core.loadIdentity() }
        switch loaded {
        case .success(let loadedIdentity):
            identity = loadedIdentity
            if deviceName.trimmed.isEmpty {
                deviceName = loadedIdentity.name
            }
            phase = .offline
            appendLog("已加载本机身份 \(loadedIdentity.deviceId)（\(loadedIdentity.name)）", level: .info)
        case .failure(_):
            let name = effectiveDeviceName
            let created = await runOffMain { core.loadOrCreateIdentity(deviceName: name) }
            switch created {
            case .success(let newIdentity):
                identity = newIdentity
                deviceName = newIdentity.name
                phase = .offline
                appendLog("首次启动，已生成设备身份 \(newIdentity.deviceId)", level: .success)
            case .failure(let error):
                phase = .failed
                errorText = error.localizedDescription
                appendLog("初始化设备身份失败：\(error.localizedDescription)", level: .failure)
            }
        }

        isAppActive = true
        lastObservedChangeCount = -1
        appendLog("前台自动同步已启用：剪贴板变化自动发送、收到内容自动写入剪贴板（iOS 首次读取需系统粘贴授权）。", level: .info)
        await refreshNotificationPermission()

        // 首次启动（或未完成引导）→ Onboarding；引导完成后由 completeOnboarding 触发自动连接。
        if !onboardingCompleted {
            showOnboarding = true
            appendLog("首次启动：请先完成引导，随后将自动连接中继", level: .info)
        } else if shouldAutoConnectOnLaunch {
            // 打开即连接：已完成引导 + 配置完整 + 开关开启时，无需用户点按钮。
            appendLog("配置完整，正在自动连接中继…", level: .info)
            await connect()
        } else if !autoConnectOnLaunch {
            appendLog("「打开应用时自动连接」已关闭：可到「连接」页手动连接。", level: .info)
        } else if !isConfigurationComplete {
            appendLog("中继配置不完整（需中继地址 + 访问令牌）：请到「连接」页完善。", level: .info)
        }
    }

    /// 手动初始化设备身份（bootstrap 通常已自动完成，保留入口便于排障）。
    func initializeIdentity() async {
        let core = self.core
        let name = effectiveDeviceName
        let result = await runOffMain { core.loadOrCreateIdentity(deviceName: name) }
        switch result {
        case .success(let newIdentity):
            identity = newIdentity
            deviceName = newIdentity.name
            phase = .offline
            appendLog("设备身份已就绪：\(newIdentity.deviceId)", level: .success)
        case .failure(let error):
            errorText = error.localizedDescription
            appendLog("初始化设备身份失败：\(error.localizedDescription)", level: .failure)
        }
    }

    // MARK: - 首次引导 / 权限

    /// 打开首次引导 / 权限说明页。
    func presentOnboarding() {
        showOnboarding = true
    }

    /// 完成首次引导（持久化，之后不再自动弹出）。
    func completeOnboarding() {
        onboardingCompleted = true
        UserDefaults.standard.set(true, forKey: DefaultsKey.onboardingCompleted)
        showOnboarding = false
        appendLog("已完成首次引导与权限说明", level: .info)

        // 引导完成后若配置完整且「打开应用时自动连接」开启，立即自动连接（打开即启动）。
        if shouldAutoConnectOnLaunch {
            appendLog("引导完成且配置完整，正在自动连接中继…", level: .info)
            Task { await connect() }
        }
    }

    /// 刷新通知权限状态（系统设置可能被用户修改）。
    func refreshNotificationPermission() async {
        let status = await withCheckedContinuation { (continuation: CheckedContinuation<UNAuthorizationStatus, Never>) in
            ClipboardNotifier.authorizationStatus { current in
                continuation.resume(returning: current)
            }
        }
        notificationPermission = Self.mapNotificationPermission(status)
    }

    /// 请求通知权限。
    func requestNotificationPermission() async {
        let granted = await withCheckedContinuation { (continuation: CheckedContinuation<Bool, Never>) in
            ClipboardNotifier.requestAuthorization { result in
                continuation.resume(returning: result)
            }
        }
        await refreshNotificationPermission()
        appendLog(
            granted
                ? "通知权限已开启：后台接收窗口内收到内容会弹本地通知"
                : "通知权限未开启：前台自动同步不受影响，可随时到系统设置开启",
            level: granted ? .success : .warning
        )
    }

    /// 权限页操作按钮：未请求 → 请求权限；已拒绝 → 去系统设置。
    func handleNotificationPermissionAction() async {
        switch notificationPermission {
        case .notDetermined:
            await requestNotificationPermission()
        case .denied:
            openSystemSettings()
        default:
            break
        }
    }

    /// 打开本 App 的系统设置页（粘贴授权 / 通知权限都在这里调整）。
    func openSystemSettings() {
        guard let url = URL(string: UIApplication.openSettingsURLString) else { return }
        UIApplication.shared.open(url)
    }

    private static func mapNotificationPermission(_ status: UNAuthorizationStatus) -> NotificationPermissionState {
        switch status {
        case .notDetermined: return .notDetermined
        case .denied: return .denied
        case .authorized: return .authorized
        case .provisional: return .provisional
        case .ephemeral: return .authorized
        @unknown default: return .unknown
        }
    }

    // MARK: - 场景切换（前台连接 / 后台断开）

    /// 场景切换：
    /// - 前台：恢复轮询、立即检查一次剪贴板、必要时重连；
    /// - 后台：保留约 25 秒接收窗口（beginBackgroundTask），期间收到内容发本地通知；
    ///   窗口结束后主动 dm_stop（iOS 不允许后台常驻连接，也不保证后台执行）。
    func scenePhaseChanged(_ scenePhase: ScenePhase) {
        switch scenePhase {
        case .active:
            isAppActive = true
            guard bootstrapped else { return }
            cancelBackgroundGrace()
            applyPendingClipboardIfNeeded()
            if wishConnected && !core.isRunning && !connecting {
                appendLog("回到前台，重新连接中继…", level: .info)
                Task { await connect() }
            } else if core.isRunning {
                lastObservedChangeCount = -1 // 回前台先检查一次当前剪贴板
                startPolling()
            }
        case .background:
            isAppActive = false
            stopPolling()
            beginBackgroundGrace()
        case .inactive:
            break
        @unknown default:
            break
        }
    }

    /// 后台接收宽限期：iOS 约给 30 秒；期间保持连接，可接收一条内容并发本地通知。
    private func beginBackgroundGrace() {
        guard core.isRunning else { return }
        cancelBackgroundGrace()
        backgroundTaskID = UIApplication.shared.beginBackgroundTask(withName: "drive-sync.clipboard-receive") { [weak self] in
            guard let self else { return }
            Task { @MainActor in
                self.finishBackgroundGrace(stopRuntime: true)
            }
        }
        appendLog("进入后台：保留约 25 秒接收窗口（iOS 限制，无后台常驻/APNs 推送）。", level: .info)
        backgroundGraceTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: ClipboardWatch.backgroundGraceSeconds)
            guard let self else { return }
            if !self.isAppActive {
                self.finishBackgroundGrace(stopRuntime: true)
            }
        }
    }

    private func cancelBackgroundGrace() {
        backgroundGraceTask?.cancel()
        backgroundGraceTask = nil
        endBackgroundTaskIfNeeded()
    }

    private func endBackgroundTaskIfNeeded() {
        guard backgroundTaskID != .invalid else { return }
        UIApplication.shared.endBackgroundTask(backgroundTaskID)
        backgroundTaskID = .invalid
    }

    /// 宽限期结束：仍在后台则断开连接（保留 wishConnected，回前台自动重连）。
    private func finishBackgroundGrace(stopRuntime: Bool) {
        cancelBackgroundGrace()
        guard stopRuntime, !isAppActive, core.isRunning else { return }
        phase = .paused
        let core = self.core
        Task {
            await runOffMain { core.stop() }
            if !self.isAppActive {
                self.phase = .paused
            }
            self.appendLog("后台接收窗口结束，已断开连接；回到前台会自动重连。", level: .info)
        }
    }

    // MARK: - 连接管理

    /// 保存配置并启动 Rust 同步运行时（连接 + 自动重连）。
    func connect() async {
        guard !connecting else { return }
        guard identity != nil else {
            errorText = "请先初始化设备身份"
            return
        }
        let relay = relayURL.trimmed
        guard !relay.isEmpty else {
            errorText = "请填写中继地址，例如 ws://192.168.1.10:8787/ws"
            return
        }
        guard relay.hasPrefix("ws://") || relay.hasPrefix("wss://") else {
            errorText = "中继地址必须以 ws:// 或 wss:// 开头"
            return
        }

        persistSettings()
        wishConnected = true
        connecting = true
        phase = .connecting

        let core = self.core
        let token = accessToken
        let name = effectiveDeviceName

        let configResult = await runOffMain { core.saveConfig(relayURL: relay, token: token, deviceName: name) }
        if case .failure(let error) = configResult {
            connecting = false
            phase = .failed
            errorText = error.localizedDescription
            appendLog("保存中继配置失败：\(error.localizedDescription)", level: .failure)
            return
        }

        let startResult = await runOffMain { core.start(relayURL: relay, token: token) }
        connecting = false
        if case .failure(let error) = startResult {
            phase = .failed
            errorText = error.localizedDescription
            appendLog("启动同步运行时失败：\(error.localizedDescription)", level: .failure)
        } else {
            appendLog("已启动同步运行时，正在连接 \(relay)…", level: .info)
            lastObservedChangeCount = -1
            startPolling()
        }
    }

    /// 用户主动断开（同时停止后台轮询）。
    func disconnect() async {
        wishConnected = false
        await stopRuntime(reason: nil)
    }

    private func stopRuntime(reason: String?) async {
        stopPolling()
        let core = self.core
        await runOffMain { core.stop() }
        status = .empty
        phase = identity == nil ? .uninitialized : .offline
        if let reason {
            appendLog(reason, level: .warning)
        }
    }

    // MARK: - 发送 / 接收剪贴板

    /// 手动兜底：用户点按「发送当前剪贴板」（iOS 16+ 首次读取会弹系统授权框）。
    func sendCurrentClipboard() {
        autoReadBlocked = false
        lastPromptFailureAt = nil
        let pasteboard = UIPasteboard.general
        lastObservedChangeCount = pasteboard.changeCount
        guard pasteboard.hasStrings else {
            errorText = "系统剪贴板中没有文本内容。请先复制一段文本，再点发送。"
            return
        }
        guard let text = pasteboard.string, !text.isEmpty else {
            autoReadBlocked = true
            errorText = "没有读取到剪贴板文本（可能拒绝了系统「允许粘贴」提示，或内容不是纯文本）。可到 设置 → DriveSync → 从其他 App 粘贴 改为允许后重试。"
            return
        }
        send(text: text)
    }

    /// 直接发送给定文本（手动兜底/调试；自动同步走 automaticClipboardCheck）。
    func send(text: String) {
        guard core.isRunning else {
            errorText = "同步未启动：请先在「连接」页保存配置并连接中继。"
            return
        }
        clipboardGuard.markSent(text: text)
        lastObservedChangeCount = UIPasteboard.general.changeCount
        let result = core.sendClipboard(text: text)
        if result.isSuccess {
            clipboardPreview = Self.preview(of: text)
            lastSentAt = Date()
            appendLog("已发送当前剪贴板：\(result.message)", level: .success)
            showToast(result.message)
        } else {
            appendLog(result.message, level: .failure)
            errorText = result.message
        }
    }

    /// 前台自动同步（发送方向）：按 `UIPasteboard.changeCount` 检测变化后自动发送。
    ///
    /// iOS 限制：没有剪贴板变更通知 API，只能轮询；没有后台读取 API；
    /// iOS 16+ 首次读取会触发系统粘贴授权框（拒绝后会退避，避免反复弹窗）。
    private func automaticClipboardCheck() {
        guard autoSyncEnabled, isAppActive, core.isRunning else { return }
        if let failedAt = lastPromptFailureAt, Date().timeIntervalSince(failedAt) < ClipboardWatch.promptBackoff {
            return
        }
        let pasteboard = UIPasteboard.general
        let changeCount = pasteboard.changeCount
        guard changeCount != lastObservedChangeCount else { return }
        lastObservedChangeCount = changeCount
        guard pasteboard.hasStrings else { return }
        guard let text = pasteboard.string, !text.isEmpty else {
            lastPromptFailureAt = Date()
            if !autoReadBlocked {
                autoReadBlocked = true
                appendLog("自动读取剪贴板被系统拦截（iOS 16+ 需粘贴授权）。可点「发送当前剪贴板」手动兜底，或到 设置 → DriveSync → 从其他 App 粘贴 改为允许。", level: .warning)
            }
            return
        }
        autoReadBlocked = false
        if clipboardGuard.shouldSuppress(text: text) {
            appendLog("检测到剪贴板是本 App 刚写入的远端内容，已抑制回环发送。", level: .info)
            return
        }
        if clipboardGuard.isSameAsLastSent(text: text) {
            return
        }
        sendAutomatically(text: text)
    }

    /// 自动发送（不弹 Toast，避免高频打扰；日志/预览可查）。
    private func sendAutomatically(text: String) {
        clipboardGuard.markSent(text: text)
        let result = core.sendClipboard(text: text)
        if result.isSuccess {
            clipboardPreview = Self.preview(of: text)
            lastSentAt = Date()
            appendLog("自动检测到剪贴板变化 → \(result.message)", level: .success)
        } else {
            appendLog("自动发送剪贴板失败：\(result.message)", level: .failure)
        }
    }

    /// 把文本写入系统剪贴板（远端自动写入 / 手动「再次复制」共用），并记录回环抑制。
    private func applyTextToPasteboard(_ text: String) {
        clipboardGuard.markApplied(text: text)
        UIPasteboard.general.string = text
        lastObservedChangeCount = UIPasteboard.general.changeCount
        lastAppliedAt = Date()
    }

    /// 手动「复制到剪贴板」（接收记录里的兜底按钮）。
    func copyToPasteboard(text: String) {
        applyTextToPasteboard(text)
        appendLog("已写入系统剪贴板（\(text.count) 字符），切换到目标 App 粘贴即可。", level: .success)
        showToast("已复制到剪贴板")
        UIImpactFeedbackGenerator(style: .medium).impactOccurred()
    }

    // MARK: - 自动同步开关

    func setAutoSyncEnabled(_ enabled: Bool) {
        autoSyncEnabled = enabled
        UserDefaults.standard.set(enabled, forKey: DefaultsKey.autoSyncEnabled)
        if enabled {
            lastObservedChangeCount = -1
            lastPromptFailureAt = nil
            autoReadBlocked = false
            appendLog("已开启前台自动同步剪贴板", level: .info)
        } else {
            appendLog("已暂停前台自动同步（仍可手动发送；收到内容仍会自动写入剪贴板）", level: .warning)
        }
    }

    func setAutoApplyReceivedEnabled(_ enabled: Bool) {
        autoApplyReceivedEnabled = enabled
        UserDefaults.standard.set(enabled, forKey: DefaultsKey.autoApplyReceived)
        appendLog(enabled ? "已开启：收到内容后自动写入剪贴板" : "已关闭自动写入：收到内容需在「接收记录」手动复制", level: .info)
    }

    func clearReceivedItems() {
        receivedItems.removeAll()
    }

    // MARK: - 配对

    func pairStart() async {
        guard core.isRunning else {
            errorText = "请先在「连接」页连接中继，再发起配对"
            return
        }
        guard !pairingBusy else { return }
        pairingBusy = true
        pairCode = nil
        appendLog("正在发起配对（生成 8 位配对码，通常 <2 秒）…", level: .info)

        let core = self.core
        let result = await runOffMain { core.pairStart() }
        pairingBusy = false
        switch result {
        case .success(let info):
            pairCode = info
            appendLog("配对码 \(info.formattedCode)，约 120 秒内有效；配对结果会自动出现在设备列表。", level: .success)
        case .failure(let error):
            errorText = error.localizedDescription
            appendLog("发起配对失败：\(error.localizedDescription)", level: .failure)
        }
    }

    func pairJoin(code rawCode: String) async {
        let code = rawCode.trimmed
        guard !code.isEmpty else {
            errorText = "请输入 8 位配对码，或粘贴 dsync-pair-v1: 开头的配对串"
            return
        }
        guard core.isRunning else {
            errorText = "请先在「连接」页连接中继，再加入配对"
            return
        }
        guard !pairingBusy else { return }
        pairingBusy = true
        appendLog("正在加入配对（最多等待约 130 秒）…", level: .info)

        let core = self.core
        let result = await runOffMain { core.pairJoin(codeOrQR: code) }
        pairingBusy = false
        switch result {
        case .success(let info):
            lastPairJoin = info
            appendLog("配对成功：\(info.peerName)（SAS \(info.sas)）", level: .success)
            showToast("已与 \(info.peerName) 配对")
            await refreshLiveData()
        case .failure(let error):
            errorText = error.localizedDescription
            appendLog("加入配对失败：\(error.localizedDescription)", level: .failure)
        }
    }

    // MARK: - 数据刷新

    func refreshLiveData() async {
        guard core.isRunning else { return }
        let core = self.core

        let peersResult = await runOffMain { core.peers() }
        if case .success(let snapshot) = peersResult {
            if peers != snapshot.peers {
                peers = snapshot.peers
            }
            if let selfIdentity = snapshot.selfIdentity, selfIdentity != identity {
                identity = selfIdentity
            }
        }

        let statusResult = await runOffMain { core.status() }
        guard case .success(let runtime) = statusResult else { return }
        status = runtime
        if runtime.running && runtime.active {
            if phase != .online {
                phase = .online
            }
        } else if !runtime.running {
            if phase == .online || phase == .connecting {
                phase = .offline
            }
        }
    }

    func refreshCatalog() async {
        guard core.isRunning else { return }
        let core = self.core
        let result = await runOffMain { core.catalog() }
        if case .success(let snapshot) = result {
            catalog = snapshot
        }
    }

    // MARK: - 事件处理（Rust 后台线程 → 主线程）

    private func handle(event: MobileEvent) {
        switch event.type {
        case "started":
            appendLog("同步运行时已启动（\(event.string("name") ?? "本机")）", level: .info)
            phase = .connecting
        case "stopped":
            appendLog("同步运行时已停止", level: .info)
            if phase != .paused {
                phase = identity == nil ? .uninitialized : .offline
            }
        case "connected":
            phase = .online
            appendLog("已连接中继，当前在线设备 \(event.onlineCount ?? 0) 台", level: .success)
            Task { await refreshLiveData() }
        case "presence":
            break
        case "peer_online":
            appendLog("设备上线：\(event.peerName ?? event.peerId ?? "未知设备")", level: .info)
            Task { await refreshLiveData() }
        case "peer_offline":
            appendLog("设备离线：\(event.peerId ?? "未知设备")", level: .warning)
            Task { await refreshLiveData() }
        case "session_established":
            appendLog("与 \(event.peerName ?? event.peerId ?? "对端设备") 建立端到端加密会话", level: .success)
            Task { await refreshLiveData() }
        case "session_closed":
            appendLog("与 \(event.peerId ?? "对端设备") 的加密会话已关闭", level: .warning)
        case "clipboard_captured":
            appendLog("已捕获本机剪贴板内容（\(event.int("size") ?? 0) 字节）", level: .info)
        case "clipboard_sent":
            appendLog("剪贴板已加密发送给 \(event.int("peers") ?? 0) 台设备", level: .success)
        case "clipboard_applied":
            appendLog("收到来自 \(event.peerId ?? "对端设备") 的剪贴板内容（\(event.int("size") ?? 0) 字节）", level: .info)
            Task { await refreshLiveData() }
        case "clipboard_spooled":
            appendLog("对端离线：内容已进入离线队列，设备上线后自动补发", level: .warning)
        case "catalog_updated", "catalog_merged", "catalog_reloaded":
            Task { await refreshCatalog() }
        case "peers_reloaded":
            Task { await refreshLiveData() }
        case "pair_code":
            if pairCode == nil, let code = event.code, !code.isEmpty {
                pairCode = PairCodeInfo(code: code, qr: event.qr ?? "")
            }
            appendLog("配对码已生成：\(pairCode?.formattedCode ?? "-")", level: .info)
        case "pair_peer":
            appendLog("配对中发现对端设备：\(event.peerName ?? event.peerId ?? "未知设备")", level: .info)
        case "pair_succeeded":
            appendLog("配对成功：\(event.peerName ?? "对端设备")（SAS \(event.sas ?? "-")）", level: .success)
            showToast("配对成功，已加入设备列表")
            Task { await refreshLiveData() }
        case "pair_failed":
            let message = event.message ?? "未知原因"
            appendLog("配对失败：\(message)", level: .failure)
            errorText = message
        case "paused":
            let paused = event.bool("paused") ?? false
            appendLog(
                paused ? "剪切板同步已暂停（连接与目录同步不受影响）" : "剪切板同步已恢复",
                level: paused ? .warning : .info
            )
        case "peer_revoked", "peer_unpaired":
            appendLog("对端已解除配对：\(event.peerName ?? event.peerId ?? "未知设备")，正在刷新设备列表", level: .warning)
            showToast("对端已解除配对", level: .warning)
            Task { await refreshLiveData() }
        case "spool_replayed":
            appendLog("离线队列已补发给对端", level: .info)
        case "peer_have_list":
            appendLog("对端可用软件列表已更新", level: .info)
        case "vault_shared", "vault_key_received":
            appendLog("收到云端暂存对象密钥（本轮 iOS 未提供暂存 UI）", level: .info)
        case "relay_error":
            appendLog("中继错误：\(event.message ?? event.code ?? "未知")", level: .warning)
        case "error":
            appendLog("错误：\(event.message ?? "未知")", level: .failure)
        case "transfer_received":
            appendLog("收到文件：\(event.string("path") ?? "-")", level: .success)
        case "transfer_failed":
            appendLog("文件传输失败：\(event.message ?? "未知")", level: .failure)
        case "transfer_offered", "transfer_started":
            appendLog("对端发起文件传输（本轮 iOS 未提供文件接收 UI）", level: .info)
        case "transfer_progress":
            break
        case "event":
            appendLog(event.message ?? event.raw, level: .info)
        default:
            appendLog("[\(event.type)] \(event.raw)", level: .info)
        }
    }

    private func handleRemoteClipboard(_ clipboard: RemoteClipboard) {
        let item = ReceivedItem(
            text: clipboard.text,
            peerId: clipboard.fromDeviceId,
            peerName: peerDisplayName(for: clipboard.fromDeviceId),
            kind: "text",
            receivedAt: clipboard.receivedAt
        )
        receivedItems.insert(item, at: 0)
        if receivedItems.count > 50 {
            receivedItems.removeLast(receivedItems.count - 50)
        }

        if isAppActive && autoApplyReceivedEnabled {
            // 无感接收：前台收到即自动写入系统剪贴板（iOS 写入剪贴板不需要额外授权）。
            applyTextToPasteboard(clipboard.text)
            appendLog("收到来自 \(item.peerName) 的内容（\(item.characterCount) 字符），已自动写入系统剪贴板，可直接在目标 App 粘贴。", level: .success)
            showToast("已自动同步 \(item.peerName) 的剪贴板内容")
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
        } else if isAppActive {
            appendLog("收到来自 \(item.peerName) 的内容（\(item.characterCount) 字符），自动写入已关闭，请点「复制到剪贴板」。", level: .info)
            showToast("收到 \(item.peerName) 的剪贴板内容", level: .warning)
        } else {
            // App 不在前台（后台接收窗口内）：本地通知展示摘要，回前台自动写入。
            pendingClipboard = clipboard
            ClipboardNotifier.notifyReceived(
                summary: ClipboardNotifier.summary(for: clipboard.text),
                peerName: item.peerName
            )
            appendLog("App 不在前台时收到 \(item.peerName) 的内容，已发本地通知；回到 App 后自动写入剪贴板。", level: .warning)
        }
    }

    /// 回到前台时把后台接收窗口内收到的内容写入剪贴板。
    private func applyPendingClipboardIfNeeded() {
        guard let pending = pendingClipboard else { return }
        pendingClipboard = nil
        if autoApplyReceivedEnabled {
            applyTextToPasteboard(pending.text)
            appendLog("已把后台窗口收到的内容写入系统剪贴板。", level: .success)
            showToast("已自动写入后台收到的剪贴板内容")
        } else {
            appendLog("有回到前台前收到的剪贴板内容，自动写入已关闭，请在「接收记录」手动复制。", level: .warning)
        }
    }

    // MARK: - 日志 / 提示

    func appendLog(_ text: String, level: LogLine.Level = .info) {
        logs.append(LogLine(date: Date(), text: text, level: level))
        if logs.count > 200 {
            logs.removeFirst(logs.count - 200)
        }
    }

    func clearLogs() {
        logs.removeAll()
    }

    func showToast(_ text: String, level: LogLine.Level = .success) {
        let newToast = Toast(text: text, level: level)
        toast = newToast
        Task { [weak self] in
            try? await Task.sleep(nanoseconds: 2_800_000_000)
            guard let self else { return }
            if self.toast?.id == newToast.id {
                self.toast = nil
            }
        }
    }

    // MARK: - 辅助

    var effectiveDeviceName: String {
        let trimmed = deviceName.trimmed
        if !trimmed.isEmpty {
            return trimmed
        }
        let systemName = UIDevice.current.name
        return systemName.isEmpty ? "iPhone" : systemName
    }

    private func peerDisplayName(for deviceId: String) -> String {
        if let peer = peers.first(where: { $0.deviceId == deviceId }) {
            return peer.displayName
        }
        return deviceId.isEmpty ? "未知设备" : deviceId
    }

    private static func preview(of text: String) -> String {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        let limit = 400
        if trimmed.count <= limit {
            return trimmed
        }
        return String(trimmed.prefix(limit)) + "…"
    }

    // MARK: - 轮询（对齐 dm_status_json / dm_peers_json）

    /// 前台轮询：每 ~1.5s 检查剪贴板变化（自动发送），每 2 拍刷新状态/设备，每 8 拍刷新目录。
    private func startPolling() {
        stopPolling()
        pollTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: ClipboardWatch.pollIntervalNanoseconds)
                guard let self else { return }
                if Task.isCancelled || !self.isAppActive || !self.core.isRunning { return }
                self.automaticClipboardCheck()
                self.pollTick += 1
                if self.pollTick % 2 == 0 {
                    await self.refreshLiveData()
                }
                if self.pollTick % 8 == 0 {
                    await self.refreshCatalog()
                }
            }
        }
    }

    private func stopPolling() {
        pollTask?.cancel()
        pollTask = nil
    }

    // MARK: - 本地配置持久化（relay/设备名 → UserDefaults；token → Keychain）

    private func loadPersistedSettings() {
        let defaults = UserDefaults.standard
        relayURL = defaults.string(forKey: DefaultsKey.relayURL) ?? ""
        let savedName = defaults.string(forKey: DefaultsKey.deviceName) ?? ""
        if !savedName.isEmpty {
            deviceName = savedName
        }
        accessToken = KeychainStore.loadToken() ?? ""
        autoSyncEnabled = defaults.object(forKey: DefaultsKey.autoSyncEnabled) as? Bool ?? true
        autoApplyReceivedEnabled = defaults.object(forKey: DefaultsKey.autoApplyReceived) as? Bool ?? true
        onboardingCompleted = defaults.bool(forKey: DefaultsKey.onboardingCompleted)
        autoConnectOnLaunch = defaults.object(forKey: DefaultsKey.autoConnectOnLaunch) as? Bool ?? true
        if let rawAppearance = defaults.string(forKey: DefaultsKey.appearanceMode),
           let savedAppearance = AppearanceMode(rawValue: rawAppearance) {
            appearanceMode = savedAppearance
        }
    }

    private func persistSettings() {
        let defaults = UserDefaults.standard
        defaults.set(relayURL.trimmed, forKey: DefaultsKey.relayURL)
        defaults.set(deviceName.trimmed, forKey: DefaultsKey.deviceName)
        defaults.set(autoSyncEnabled, forKey: DefaultsKey.autoSyncEnabled)
        defaults.set(autoApplyReceivedEnabled, forKey: DefaultsKey.autoApplyReceived)
        KeychainStore.saveToken(accessToken)
    }
}

// MARK: - 后台执行辅助

/// 把阻塞的 C ABI 调用放到后台线程，避免阻塞主线程（头文件要求）。
private func runOffMain<T>(_ work: @escaping @Sendable () -> T) async -> T {
    await Task.detached(priority: .userInitiated) { work() }.value
}

// MARK: - 路径与常量

enum AppPaths {
    /// `<Application Support>/drive-sync`：identity.json / peers.json / spool 等。
    static var dataDirectory: String {
        let manager = FileManager.default
        let base = manager.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSHomeDirectory())
        return base.appendingPathComponent("drive-sync", isDirectory: true).path
    }
}

private enum DefaultsKey {
    static let relayURL = "dev.drivesync.mobile.relayURL"
    static let deviceName = "dev.drivesync.mobile.deviceName"
    static let autoSyncEnabled = "dev.drivesync.mobile.autoSyncEnabled"
    static let autoApplyReceived = "dev.drivesync.mobile.autoApplyReceived"
    static let onboardingCompleted = "dev.drivesync.mobile.onboardingCompleted"
    static let appearanceMode = "dev.drivesync.mobile.appearanceMode"
    static let autoConnectOnLaunch = "dev.drivesync.mobile.autoConnectOnLaunch"
}

extension String {
    /// 去掉首尾空白（配置输入常用）。
    var trimmed: String {
        trimmingCharacters(in: .whitespacesAndNewlines)
    }
}
