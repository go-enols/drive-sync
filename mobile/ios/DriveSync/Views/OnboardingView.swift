//
//  OnboardingView.swift
//  drive-sync iOS
//
//  首次启动引导 + 权限状态页（PolyBot 深色主题）。
//  - firstRun：品牌欢迎、三步上手、通知权限请求、iOS 粘贴授权说明、后台限制说明；
//  - permissions：从「连接」页进入的权限与平台说明（可重复查看）。
//
//  iOS 现实（必须如实告知用户）：
//    - 通知权限用于后台约 25 秒接收窗口内的本地通知；
//    - iOS 16+ 首次自动读取剪贴板会弹「允许粘贴？」；授权无法预先申请/查询；
//    - iOS 不允许后台常驻连接或静默读写剪贴板，未接入 APNs 推送。
//

import SwiftUI

struct OnboardingView: View {
    enum Mode: Equatable {
        case firstRun
        case permissions
    }

    let mode: Mode

    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        ZStack {
            Theme.backgroundGradient.ignoresSafeArea()

            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    header

                    if mode == .firstRun {
                        stepsCard
                    }

                    permissionsCard
                    securityCard
                    actions
                }
                .padding(.horizontal, 20)
                .padding(.top, 24)
                .padding(.bottom, 34)
            }
            .scrollIndicators(.hidden)
        }
        .preferredColorScheme(.dark)
        .interactiveDismissDisabled(mode == .firstRun)
        .task {
            await model.refreshNotificationPermission()
        }
    }

    // MARK: - 顶部

    @ViewBuilder
    private var header: some View {
        if mode == .firstRun {
            VStack(alignment: .leading, spacing: 14) {
                BrandLogo(size: 64)

                HStack(alignment: .center, spacing: 10) {
                    Text("drive-sync")
                        .font(.system(size: 30, weight: .bold))
                        .foregroundStyle(Theme.textPrimary)
                    VersionBadge()
                }

                Text("跨设备剪贴板 · 无感同步")
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(Theme.mint)

                Text("自托管中继 + 端到端加密：复制即同步到已配对设备，前台无感收发，不经过第三方云。")
                    .font(.system(size: 13))
                    .foregroundStyle(Theme.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        } else {
            SheetHeader(title: "权限与平台说明", subtitle: "drive-sync \(AppInfo.versionBadge)") {
                dismiss()
            }
        }
    }

    // MARK: - 三步上手

    private var stepsCard: some View {
        PolyCard {
            VStack(alignment: .leading, spacing: 14) {
                CardTitle(icon: "sparkles", title: "三步开始使用")

                stepRow(
                    number: "1",
                    title: "连接中继",
                    detail: "在「连接」页填写自建 relay 地址与访问令牌；已保存配置时启动自动连接。"
                )
                PolyDivider()
                stepRow(
                    number: "2",
                    title: "配对设备",
                    detail: "一台设备「发起配对」显示 8 位码/二维码，另一台输入配对码加入。"
                )
                PolyDivider()
                stepRow(
                    number: "3",
                    title: "前台无感同步",
                    detail: "检测到剪贴板变化自动发送；收到内容自动写入剪贴板。后台限制见下方说明。"
                )
            }
        }
    }

    private func stepRow(number: String, title: String, detail: String) -> some View {
        HStack(alignment: .top, spacing: 12) {
            Text(number)
                .font(.system(size: 13, weight: .bold, design: .monospaced))
                .foregroundStyle(Theme.mint)
                .frame(width: 26, height: 26)
                .background(Circle().fill(Theme.mint.opacity(0.12)))
                .overlay(Circle().strokeBorder(Theme.mint.opacity(0.35), lineWidth: 1))

            VStack(alignment: .leading, spacing: 3) {
                Text(title)
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Theme.textPrimary)
                Text(detail)
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
        }
    }

    // MARK: - 权限与平台限制

    private var permissionsCard: some View {
        PolyCard {
            VStack(alignment: .leading, spacing: 14) {
                CardTitle(icon: "checkmark.shield", title: "权限与系统限制")

                notificationRow
                PolyDivider()
                infoBlock(
                    icon: "doc.on.clipboard",
                    title: "系统粘贴授权",
                    detail: "iOS 16+ 首次自动读取剪贴板会弹「允许粘贴？」。选择允许后前台无感发送；该授权无法预先申请或查询，可在 设置 → drive-sync → 从其他 App 粘贴 调整。"
                )
                PolyDivider()
                infoBlock(
                    icon: "moon.zzz",
                    title: "后台限制",
                    detail: "iOS 不允许后台常驻连接或静默读写剪贴板：进入后台保留约 25 秒接收窗口并发本地通知，随后断开；回到前台自动重连。本轮未接入 APNs 推送。"
                )
                PolyDivider()

                HStack {
                    Button {
                        model.openSystemSettings()
                    } label: {
                        Label("打开系统设置", systemImage: "gear")
                    }
                    .buttonStyle(PolyLinkButtonStyle())

                    Spacer()

                    Text("drive-sync \(AppInfo.versionBadge)")
                        .font(.system(size: 10, design: .monospaced))
                        .foregroundStyle(Theme.textTertiary)
                }
            }
        }
    }

    private var notificationRow: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: "bell.badge")
                .font(.system(size: 15))
                .foregroundStyle(Theme.mint)
                .frame(width: 22)

            VStack(alignment: .leading, spacing: 3) {
                Text("通知权限")
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Theme.textPrimary)
                Text("后台接收窗口内收到内容时发一条本地通知（只含内容摘要）")
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            Spacer(minLength: 8)

            VStack(alignment: .trailing, spacing: 6) {
                StatusPill(text: model.notificationPermission.title, color: model.notificationPermission.color)
                if let actionTitle = model.notificationPermission.actionTitle {
                    Button(actionTitle) {
                        Task { await model.handleNotificationPermissionAction() }
                    }
                    .buttonStyle(PolyLinkButtonStyle())
                }
            }
        }
    }

    private func infoBlock(icon: String, title: String, detail: String) -> some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: icon)
                .font(.system(size: 15))
                .foregroundStyle(Theme.mint)
                .frame(width: 22)

            VStack(alignment: .leading, spacing: 3) {
                Text(title)
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Theme.textPrimary)
                Text(detail)
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
        }
    }

    // MARK: - 隐私与安全

    private var securityCard: some View {
        PolyCard {
            VStack(alignment: .leading, spacing: 10) {
                CardTitle(icon: "lock.shield", title: "隐私与安全")
                EmptyHint(
                    icon: "checkmark.seal",
                    text: "剪贴板内容端到端加密后才发往中继，中继只见密文；access token 存 Keychain；App 日志不含剪贴板明文。"
                )
            }
        }
    }

    // MARK: - 底部操作

    @ViewBuilder
    private var actions: some View {
        if mode == .firstRun {
            VStack(spacing: 10) {
                Button("开始使用") {
                    model.selectedTab = .connect
                    model.completeOnboarding()
                }
                .buttonStyle(PolyPrimaryButtonStyle())

                Button("稍后设置") {
                    model.completeOnboarding()
                }
                .buttonStyle(PolySecondaryButtonStyle())
            }
        } else {
            Button("完成") {
                dismiss()
            }
            .buttonStyle(PolyPrimaryButtonStyle())
        }
    }
}
