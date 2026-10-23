//
//  ClipboardSync.swift
//  drive-sync iOS
//
//  剪贴板“无感同步”的支撑件：
//   1. ClipboardLoopGuard：回环抑制（本 App 写入的远端内容不再发回对端）；
//   2. ClipboardNotifier：App 不在前台时收到内容 → 本地通知展示摘要。
//
//  iOS 现实：没有剪贴板变更通知 API（只能轮询 changeCount）、没有后台读取 API。
//  因此“自动同步”只可能发生在 App 前台激活时，详见 docs/mobile-ios.md。
//

import Foundation
import UIKit
import UserNotifications

// MARK: - 回环抑制

/// 记录「最近发送」与「App 写入的远端内容」的文本哈希（FNV-1a 64，进程内稳定）。
///
/// 场景：App 收到远端内容 → 自动写入 UIPasteboard → 下一轮 changeCount 轮询会读到它；
/// 若不做抑制，就会把刚收到的内容再发回对端，形成 A→B→A 回环。
final class ClipboardLoopGuard {
    private var appliedHashes: [UInt64: Date] = [:]
    private var lastSentHash: UInt64?
    private let suppressionWindow: TimeInterval = 120

    /// FNV-1a 64 bit；只用于进程内去重，不做安全用途。
    static func hash(_ text: String) -> UInt64 {
        var value: UInt64 = 0xCBF2_9CE4_8422_2325
        for byte in text.utf8 {
            value ^= UInt64(byte)
            value = value &* 0x0000_0100_0000_01B3
        }
        return value
    }

    /// 记录「这段文本是本 App 从远端收到的，已写入剪贴板」。
    func markApplied(text: String) {
        appliedHashes[Self.hash(text)] = Date()
        prune()
    }

    /// 记录「这段文本已经发送过」（手动或自动），避免重复发送。
    func markSent(text: String) {
        lastSentHash = Self.hash(text)
    }

    /// 是否是本 App 刚刚写入的远端内容（命中则抑制自动发送）。
    func shouldSuppress(text: String) -> Bool {
        prune()
        return appliedHashes[Self.hash(text)] != nil
    }

    /// 与上一次发送内容相同（未变化不重复发送）。
    func isSameAsLastSent(text: String) -> Bool {
        lastSentHash == Self.hash(text)
    }

    func reset() {
        appliedHashes.removeAll()
        lastSentHash = nil
    }

    private func prune() {
        let cutoff = Date().addingTimeInterval(-suppressionWindow)
        appliedHashes = appliedHashes.filter { $0.value > cutoff }
    }
}

// MARK: - 本地通知

/// App 不在前台时收到远端剪贴板内容 → 发一条本地通知（只放摘要，不放全文）。
enum ClipboardNotifier {
    /// 启动时请求通知权限（被拒绝也不影响前台自动同步）。
    static func requestAuthorization() {
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge]) { _, _ in
            // 权限结果不影响主流程；前台自动写入不依赖通知。
        }
    }

    /// 收到远端内容但 App 不在前台时调用：标题带来源设备名，正文是内容摘要。
    static func notifyReceived(summary: String, peerName: String) {
        let content = UNMutableNotificationContent()
        content.title = "收到来自 \(peerName) 的剪贴板内容"
        content.body = "打开 drive-sync 后将自动写入系统剪贴板。内容摘要：\(summary)"
        content.sound = .default
        content.userInfo = ["kind": "clipboard_received"]

        let request = UNNotificationRequest(
            identifier: "drive-sync-clipboard-\(UUID().uuidString)",
            content: content,
            trigger: nil
        )
        UNUserNotificationCenter.current().add(request, withCompletionHandler: nil)
    }

    /// 内容摘要：单行、最多 100 字符（通知里不展示完整内容）。
    static func summary(for text: String, limit: Int = 100) -> String {
        let singleLine = text
            .replacingOccurrences(of: "\r\n", with: " ")
            .replacingOccurrences(of: "\n", with: " ")
            .replacingOccurrences(of: "\r", with: " ")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if singleLine.count <= limit {
            return singleLine
        }
        return String(singleLine.prefix(limit)) + "…"
    }
}

// MARK: - 前台轮询参数

enum ClipboardWatch {
    /// 前台轮询剪贴板 changeCount 的间隔（iOS 没有变更通知 API）。
    static let pollIntervalNanoseconds: UInt64 = 1_500_000_000
    /// 进入后台后的接收宽限期（beginBackgroundTask 通常给约 30 秒）。
    static let backgroundGraceSeconds: UInt64 = 25_000_000_000
    /// 自动读取被系统粘贴授权拦截后的退避时间。
    static let promptBackoff: TimeInterval = 60
}
