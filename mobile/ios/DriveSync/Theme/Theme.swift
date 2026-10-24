//
//  Theme.swift
//  drive-sync iOS
//
//  PolyBot 风格主题（深色 + 薄荷绿）。
//  颜色/圆角/渐变/按钮样式集中在这里，页面只引用 Theme 与 Poly* 组件，
//  不允许散落硬编码颜色（便于后续统一换肤）。
//
//  规范：
//    背景 #0B0F14  卡片 #111820  边框 #1E2A33
//    主文字 #E6EDF3  次要 #9CA3AF
//    主色薄荷绿 #22E3B0  危险 #F87171
//    圆角 10–12pt；主按钮薄荷绿填充深色文字；输入框深色 + 薄荷绿聚焦描边。
//

import SwiftUI
import UIKit

enum Theme {
    // MARK: 颜色

    static let background = Color(hex: 0x0B0F14)
    static let card = Color(hex: 0x111820)
    static let cardElevated = Color(hex: 0x151E27)
    static let border = Color(hex: 0x1E2A33)
    static let textPrimary = Color(hex: 0xE6EDF3)
    static let textSecondary = Color(hex: 0x9CA3AF)
    static let textTertiary = Color(hex: 0x6B7280)
    static let mint = Color(hex: 0x22E3B0)
    static let mintDeep = Color(hex: 0x0FB98A)
    static let danger = Color(hex: 0xF87171)
    static let warning = Color(hex: 0xFBBF24)

    // MARK: 圆角

    static let corner: CGFloat = 12
    static let cornerSmall: CGFloat = 10
    static let inputCorner: CGFloat = 10

    // MARK: 渐变

    static let backgroundGradient = LinearGradient(
        colors: [Color(hex: 0x0B0F14), Color(hex: 0x0C131A), Color(hex: 0x0B0F14)],
        startPoint: .top,
        endPoint: .bottom
    )

    static let mintGradient = LinearGradient(
        colors: [mint, Color(hex: 0x1FD9A8)],
        startPoint: .topLeading,
        endPoint: .bottomTrailing
    )

    static let logoGradient = LinearGradient(
        colors: [Color(hex: 0x143D33), Color(hex: 0x0C211C)],
        startPoint: .topLeading,
        endPoint: .bottomTrailing
    )
}

// MARK: - 十六进制颜色

extension Color {
    /// `Color(hex: 0x0B0F14)` / `Color(hex: 0x22E3B0, opacity: 0.12)`。
    init(hex: UInt32, opacity: Double = 1.0) {
        let red = Double((hex >> 16) & 0xFF) / 255.0
        let green = Double((hex >> 8) & 0xFF) / 255.0
        let blue = Double(hex & 0xFF) / 255.0
        self.init(.sRGB, red: red, green: green, blue: blue, opacity: opacity)
    }
}

// MARK: - App 信息（品牌栏版本徽章用）

enum AppInfo {
    /// 来自 Info.plist 的 `CFBundleShortVersionString`（project.yml 的 MARKETING_VERSION）。
    static let version: String = {
        let shortVersion = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String
        if let shortVersion, !shortVersion.isEmpty {
            return shortVersion
        }
        return "0.3.0"
    }()

    static var versionBadge: String { "v\(version)" }
}

// MARK: - 按钮样式

/// 主按钮：薄荷绿渐变填充 + 深色文字（PolyBot 主 CTA）。
struct PolyPrimaryButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 15, weight: .semibold))
            .foregroundStyle(Theme.background)
            .frame(maxWidth: .infinity)
            .padding(.vertical, 13)
            .background(
                RoundedRectangle(cornerRadius: Theme.cornerSmall, style: .continuous)
                    .fill(Theme.mintGradient)
            )
            .opacity(configuration.isPressed ? 0.82 : 1.0)
            .contentShape(RoundedRectangle(cornerRadius: Theme.cornerSmall, style: .continuous))
    }
}

/// 次按钮：深色底 + 细边框 + 主文字色。
struct PolySecondaryButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 14, weight: .semibold))
            .foregroundStyle(Theme.textPrimary)
            .frame(maxWidth: .infinity)
            .padding(.vertical, 12)
            .background(
                RoundedRectangle(cornerRadius: Theme.cornerSmall, style: .continuous)
                    .fill(Theme.cardElevated)
            )
            .overlay(
                RoundedRectangle(cornerRadius: Theme.cornerSmall, style: .continuous)
                    .strokeBorder(Theme.border, lineWidth: 1)
            )
            .opacity(configuration.isPressed ? 0.75 : 1.0)
            .contentShape(RoundedRectangle(cornerRadius: Theme.cornerSmall, style: .continuous))
    }
}

/// 危险操作按钮：红色低透明底 + 红字。
struct PolyDangerButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 14, weight: .semibold))
            .foregroundStyle(Theme.danger)
            .frame(maxWidth: .infinity)
            .padding(.vertical, 12)
            .background(
                RoundedRectangle(cornerRadius: Theme.cornerSmall, style: .continuous)
                    .fill(Theme.danger.opacity(0.10))
            )
            .overlay(
                RoundedRectangle(cornerRadius: Theme.cornerSmall, style: .continuous)
                    .strokeBorder(Theme.danger.opacity(0.35), lineWidth: 1)
            )
            .opacity(configuration.isPressed ? 0.75 : 1.0)
            .contentShape(RoundedRectangle(cornerRadius: Theme.cornerSmall, style: .continuous))
    }
}

/// 文字按钮（薄荷绿）：用于卡片内轻量操作（刷新/清空/复制）。
struct PolyLinkButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 13, weight: .semibold))
            .foregroundStyle(Theme.mint)
            .opacity(configuration.isPressed ? 0.65 : 1.0)
            .contentShape(Rectangle())
    }
}

// MARK: - 常用修饰

extension View {
    /// 全屏深色背景（含安全区）。
    func polyBackground() -> some View {
        background(Theme.backgroundGradient.ignoresSafeArea())
    }

    /// 细分割线颜色（用于自定义内容）。
    func polyDividerStyle() -> some View {
        overlay(alignment: .bottom) {
            Rectangle()
                .fill(Theme.border)
                .frame(height: 1)
        }
    }
}

/// 水平细分割线。
struct PolyDivider: View {
    var body: some View {
        Rectangle()
            .fill(Theme.border)
            .frame(height: 1)
    }
}
