//
//  Theme.swift
//  drive-sync iOS
//
//  PolyBot 风格主题（同时支持深色 / 浅色）。
//  颜色令牌全部为动态 Color（UIColor dynamicProvider），随系统外观或用户在
//  「设置 → 外观主题」中的选择即时切换；页面只引用 Theme 与 Poly* 组件。
//
//  调色板：
//                深色                          浅色
//    背景        #0B0F14                       #F5F7FA
//    卡片        #111820                       #FFFFFF
//    边框        #1E2A33                       #E2E8F0
//    主文字      #E6EDF3                       #0B1220
//    次要文字    #9CA3AF                       #64748B
//    品牌薄荷绿  #22E3B0                       #0EA37F / #10B981
//    危险        #F87171                       #DC2626
//
//  圆角 10–12pt；主按钮薄荷绿填充深色文字；输入框浅/深底 + 薄荷绿聚焦描边。
//

import SwiftUI
import UIKit

enum Theme {
    // MARK: - 动态颜色工具

    /// 生成随深浅外观自动切换的 Color（SwiftUI 在渲染时按 colorScheme 解析）。
    static func adaptive(light: UInt32, dark: UInt32) -> Color {
        Color(uiColor: UIColor.adaptive(light: light, dark: dark))
    }

    // MARK: - 颜色令牌（深 / 浅两套）

    static let background = adaptive(light: 0xF5F7FA, dark: 0x0B0F14)
    static let card = adaptive(light: 0xFFFFFF, dark: 0x111820)
    static let cardElevated = adaptive(light: 0xF1F5F9, dark: 0x151E27)
    static let border = adaptive(light: 0xE2E8F0, dark: 0x1E2A33)
    static let textPrimary = adaptive(light: 0x0B1220, dark: 0xE6EDF3)
    static let textSecondary = adaptive(light: 0x64748B, dark: 0x9CA3AF)
    static let textTertiary = adaptive(light: 0x64748B, dark: 0x6B7280)
    static let mint = adaptive(light: 0x0EA37F, dark: 0x22E3B0)
    static let mintDeep = adaptive(light: 0x0B8A6B, dark: 0x0FB98A)
    static let danger = adaptive(light: 0xDC2626, dark: 0xF87171)
    static let warning = adaptive(light: 0xB45309, dark: 0xFBBF24)

    /// 薄荷绿主按钮上的文字：两套主题都用深色文字（对比度最佳）。
    static let buttonText = Color(hex: 0x07120E)

    // MARK: - 圆角

    static let corner: CGFloat = 12
    static let cornerSmall: CGFloat = 10
    static let inputCorner: CGFloat = 10

    // MARK: - 渐变（两套主题）

    static let backgroundGradient = LinearGradient(
        colors: [
            adaptive(light: 0xF5F7FA, dark: 0x0B0F14),
            adaptive(light: 0xEEF2F7, dark: 0x0C131A),
            adaptive(light: 0xF5F7FA, dark: 0x0B0F14)
        ],
        startPoint: .top,
        endPoint: .bottom
    )

    static let mintGradient = LinearGradient(
        colors: [
            adaptive(light: 0x0EA37F, dark: 0x22E3B0),
            adaptive(light: 0x10B981, dark: 0x1FD9A8)
        ],
        startPoint: .topLeading,
        endPoint: .bottomTrailing
    )

    static let logoGradient = LinearGradient(
        colors: [
            adaptive(light: 0xDCFCE7, dark: 0x143D33),
            adaptive(light: 0xBBF7D0, dark: 0x0C211C)
        ],
        startPoint: .topLeading,
        endPoint: .bottomTrailing
    )

    // MARK: - UIKit 外观（标签栏 / 导航栏，动态解析）

    static let uiBackground = UIColor.adaptive(light: 0xF5F7FA, dark: 0x0B0F14)
    static let uiSurface = UIColor.adaptive(light: 0xFFFFFF, dark: 0x111820)
    static let uiBorder = UIColor.adaptive(light: 0xE2E8F0, dark: 0x1E2A33)
    static let uiMint = UIColor.adaptive(light: 0x0EA37F, dark: 0x22E3B0)
    static let uiTextPrimary = UIColor.adaptive(light: 0x0B1220, dark: 0xE6EDF3)
    static let uiTextSecondary = UIColor.adaptive(light: 0x64748B, dark: 0x9CA3AF)
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

extension UIColor {
    convenience init(hex: UInt32, alpha: CGFloat = 1.0) {
        let red = CGFloat((hex >> 16) & 0xFF) / 255.0
        let green = CGFloat((hex >> 8) & 0xFF) / 255.0
        let blue = CGFloat(hex & 0xFF) / 255.0
        self.init(red: red, green: green, blue: blue, alpha: alpha)
    }

    /// 随深浅外观自动切换的 UIColor（用于 UITabBarAppearance 等 UIKit 外观）。
    static func adaptive(light: UInt32, dark: UInt32) -> UIColor {
        UIColor { traits in
            traits.userInterfaceStyle == .dark ? UIColor(hex: dark) : UIColor(hex: light)
        }
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
        return "0.3.1"
    }()

    static var versionBadge: String { "v\(version)" }
}

// MARK: - 按钮样式

/// 主按钮：薄荷绿渐变填充 + 深色文字（PolyBot 主 CTA）。
struct PolyPrimaryButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 15, weight: .semibold))
            .foregroundStyle(Theme.buttonText)
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

/// 次按钮：卡片浮层底 + 细边框 + 主文字色。
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
    /// 全屏背景（深/浅主题渐变，含安全区）。
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
