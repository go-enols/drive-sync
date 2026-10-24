//
//  PolyComponents.swift
//  drive-sync iOS
//
//  PolyBot 风格的通用组件：品牌栏、卡片、输入框、信息行、状态徽章、开关行等。
//  只依赖 SwiftUI/UIKit，不引入第三方 UI 库。
//

import SwiftUI
import UIKit

// MARK: - 屏幕容器

/// 统一页面容器：深色渐变背景 + 卡片纵向间距 + 隐藏滚动指示器。
struct PolyScreen<Content: View>: View {
    private let content: Content

    init(@ViewBuilder content: () -> Content) {
        self.content = content()
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                content
            }
            .padding(.horizontal, 16)
            .padding(.top, 10)
            .padding(.bottom, 32)
        }
        .scrollIndicators(.hidden)
        .background(Theme.backgroundGradient.ignoresSafeArea())
        .toolbarBackground(Theme.background, for: .tabBar)
        .toolbarBackground(.visible, for: .tabBar)
        .toolbarColorScheme(.dark, for: .tabBar)
    }
}

// MARK: - 品牌栏

/// 圆角 logo：深绿底 + 薄荷绿描边与图标（对应 PolyBot 顶部品牌标识）。
struct BrandLogo: View {
    var size: CGFloat = 42

    var body: some View {
        ZStack {
            RoundedRectangle(cornerRadius: size * 0.28, style: .continuous)
                .fill(Theme.logoGradient)
                .overlay(
                    RoundedRectangle(cornerRadius: size * 0.28, style: .continuous)
                        .strokeBorder(Theme.mint.opacity(0.45), lineWidth: 1)
                )
            Image(systemName: "arrow.left.arrow.right")
                .font(.system(size: size * 0.40, weight: .bold))
                .foregroundStyle(Theme.mint)
        }
        .frame(width: size, height: size)
        .shadow(color: Theme.mint.opacity(0.20), radius: 10, x: 0, y: 2)
    }
}

/// 版本徽章胶囊（如 `v0.3.0`）。
struct VersionBadge: View {
    var text: String = AppInfo.versionBadge

    var body: some View {
        Text(text)
            .font(.system(size: 11, weight: .medium, design: .monospaced))
            .foregroundStyle(Theme.textSecondary)
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .background(Capsule().fill(Theme.cardElevated))
            .overlay(Capsule().strokeBorder(Theme.border, lineWidth: 1))
    }
}

/// 顶部品牌栏：圆角 logo + drive-sync + 版本徽章 + 副标题。
struct BrandHeader: View {
    var subtitle: String = "跨设备剪贴板同步"

    var body: some View {
        HStack(spacing: 12) {
            BrandLogo(size: 42)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 8) {
                    Text("drive-sync")
                        .font(.system(size: 20, weight: .bold))
                        .foregroundStyle(Theme.textPrimary)
                    VersionBadge()
                }
                Text(subtitle)
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.textSecondary)
            }
            Spacer(minLength: 0)
        }
        .padding(.bottom, 2)
    }
}

// MARK: - 卡片

/// 卡片容器：`#111820` 底 + `#1E2A33` 细边框 + 12pt 圆角。
struct PolyCard<Content: View>: View {
    private let content: Content

    init(@ViewBuilder content: () -> Content) {
        self.content = content()
    }

    var body: some View {
        content
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(16)
            .background(
                RoundedRectangle(cornerRadius: Theme.corner, style: .continuous)
                    .fill(Theme.card)
            )
            .overlay(
                RoundedRectangle(cornerRadius: Theme.corner, style: .continuous)
                    .strokeBorder(Theme.border, lineWidth: 1)
            )
    }
}

/// 卡片标题（薄荷绿小图标 + 主文字）。
struct CardTitle: View {
    let icon: String
    let title: String

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: icon)
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(Theme.mint)
            Text(title)
                .font(.system(size: 14, weight: .semibold))
                .foregroundStyle(Theme.textPrimary)
        }
    }
}

// MARK: - 输入框

/// 深色输入框 + 薄荷绿聚焦描边（PolyBot 风格）。
struct PolyTextField: View {
    let title: String
    let placeholder: String
    @Binding var text: String
    var icon: String = "pencil"
    var keyboard: UIKeyboardType = .default
    var contentType: UITextContentType? = nil

    @FocusState private var isFocused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title)
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(Theme.textSecondary)

            HStack(spacing: 9) {
                Image(systemName: icon)
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(isFocused ? Theme.mint : Theme.textTertiary)
                    .frame(width: 18)

                TextField("", text: $text, prompt: Text(placeholder).foregroundColor(Theme.textTertiary))
                    .font(.system(size: 15))
                    .foregroundStyle(Theme.textPrimary)
                    .keyboardType(keyboard)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled(true)
                    .textContentType(contentType)
                    .focused($isFocused)
            }
            .padding(.horizontal, 12)
            .frame(height: 46)
            .background(
                RoundedRectangle(cornerRadius: Theme.inputCorner, style: .continuous)
                    .fill(Theme.background)
            )
            .overlay(
                RoundedRectangle(cornerRadius: Theme.inputCorner, style: .continuous)
                    .strokeBorder(isFocused ? Theme.mint : Theme.border, lineWidth: isFocused ? 1.5 : 1)
            )
            .animation(.easeOut(duration: 0.15), value: isFocused)
        }
        .onTapGesture {
            isFocused = true
        }
    }
}

/// 深色密码输入框 + 显示/隐藏切换 + 薄荷绿聚焦描边。
struct PolySecureField: View {
    let title: String
    let placeholder: String
    @Binding var text: String
    var icon: String = "key.fill"

    @State private var isRevealed = false
    @FocusState private var isFocused: Bool

    private var promptText: Text {
        Text(placeholder).foregroundColor(Theme.textTertiary)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title)
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(Theme.textSecondary)

            HStack(spacing: 9) {
                Image(systemName: icon)
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(isFocused ? Theme.mint : Theme.textTertiary)
                    .frame(width: 18)

                Group {
                    if isRevealed {
                        TextField("", text: $text, prompt: promptText)
                    } else {
                        SecureField("", text: $text, prompt: promptText)
                    }
                }
                .font(.system(size: 15))
                .foregroundStyle(Theme.textPrimary)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled(true)
                .focused($isFocused)

                Button {
                    isRevealed.toggle()
                } label: {
                    Image(systemName: isRevealed ? "eye.slash" : "eye")
                        .font(.system(size: 13))
                        .foregroundStyle(Theme.textTertiary)
                }
                .buttonStyle(.plain)
            }
            .padding(.horizontal, 12)
            .frame(height: 46)
            .background(
                RoundedRectangle(cornerRadius: Theme.inputCorner, style: .continuous)
                    .fill(Theme.background)
            )
            .overlay(
                RoundedRectangle(cornerRadius: Theme.inputCorner, style: .continuous)
                    .strokeBorder(isFocused ? Theme.mint : Theme.border, lineWidth: isFocused ? 1.5 : 1)
            )
            .animation(.easeOut(duration: 0.15), value: isFocused)
        }
        .onTapGesture {
            isFocused = true
        }
    }
}

// MARK: - 信息行 / 指标

/// 一行信息：左标签（次要色）+ 右值（主文字）。
struct InfoRow: View {
    let label: String
    let value: String
    var mono: Bool = false

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Text(label)
                .font(.system(size: 13))
                .foregroundStyle(Theme.textSecondary)
            Spacer(minLength: 8)
            Text(value)
                .font(mono ? .system(size: 12, design: .monospaced) : .system(size: 13, weight: .medium))
                .foregroundStyle(Theme.textPrimary)
                .multilineTextAlignment(.trailing)
                .textSelection(.enabled)
        }
    }
}

/// 多行信息（长路径/设备 ID 用）：标签在上、值在下，可选中。
struct InfoColumn: View {
    let label: String
    let value: String
    var mono: Bool = false

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(label)
                .font(.system(size: 11))
                .foregroundStyle(Theme.textTertiary)
            Text(value)
                .font(mono ? .system(size: 12, design: .monospaced) : .system(size: 13))
                .foregroundStyle(Theme.textPrimary)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// 指标块：大号薄荷绿数值 + 下方标签。
struct MetricTile: View {
    let value: String
    let label: String

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(value)
                .font(.system(size: 22, weight: .bold))
                .foregroundStyle(Theme.mint)
            Text(label)
                .font(.system(size: 11))
                .foregroundStyle(Theme.textTertiary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(12)
        .background(
            RoundedRectangle(cornerRadius: Theme.inputCorner, style: .continuous)
                .fill(Theme.cardElevated)
        )
    }
}

// MARK: - 状态 / 提示

/// 状态胶囊徽章（在线/离线/权限状态等）。
struct StatusPill: View {
    let text: String
    let color: Color

    var body: some View {
        Text(text)
            .font(.system(size: 11, weight: .semibold))
            .foregroundStyle(color)
            .padding(.horizontal, 9)
            .padding(.vertical, 4)
            .background(Capsule().fill(color.opacity(0.12)))
            .overlay(Capsule().strokeBorder(color.opacity(0.35), lineWidth: 1))
    }
}

/// 空状态/说明提示（左侧小图标 + 次要文字）。
struct EmptyHint: View {
    let icon: String
    let text: String

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: icon)
                .font(.system(size: 14))
                .foregroundStyle(Theme.textTertiary)
            Text(text)
                .font(.system(size: 13))
                .foregroundStyle(Theme.textSecondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

// MARK: - 开关行

/// 开关行：图标 + 标题/说明 + 薄荷绿 Toggle。
struct PolyToggleRow: View {
    let icon: String
    let title: String
    var subtitle: String? = nil
    @Binding var isOn: Bool

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: icon)
                .font(.system(size: 15))
                .foregroundStyle(Theme.mint)
                .frame(width: 22, height: 22)
            VStack(alignment: .leading, spacing: 3) {
                Text(title)
                    .font(.system(size: 14, weight: .medium))
                    .foregroundStyle(Theme.textPrimary)
                if let subtitle {
                    Text(subtitle)
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.textSecondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 8)
            Toggle("", isOn: $isOn)
                .labelsHidden()
                .tint(Theme.mint)
        }
    }
}

// MARK: - 弹层标题栏

/// 弹层（sheet）顶部标题栏：标题/副标题 + 圆形关闭按钮。
struct SheetHeader: View {
    let title: String
    var subtitle: String? = nil
    let onClose: () -> Void

    var body: some View {
        HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(.system(size: 18, weight: .bold))
                    .foregroundStyle(Theme.textPrimary)
                if let subtitle {
                    Text(subtitle)
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.textSecondary)
                }
            }
            Spacer(minLength: 8)
            Button(action: onClose) {
                Image(systemName: "xmark")
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Theme.textSecondary)
                    .frame(width: 32, height: 32)
                    .background(Circle().fill(Theme.cardElevated))
                    .overlay(Circle().strokeBorder(Theme.border, lineWidth: 1))
            }
            .buttonStyle(.plain)
        }
    }
}
