//
//  SettingsView.swift
//  drive-sync iOS
//
//  设置页（PolyBot 深/浅双主题）：外观主题（跟随系统/深色/浅色）、通知权限、
//  首次引导与权限说明入口、关于（版本 / Rust 核心 / 设备 ID / 数据目录）。
//

import SwiftUI

struct SettingsView: View {
    @EnvironmentObject private var model: AppModel
    @State private var showPermissions = false

    var body: some View {
        PolyScreen {
            BrandHeader(subtitle: "外观 / 权限 / 关于")

            // MARK: 外观主题
            PolyCard {
                VStack(alignment: .leading, spacing: 12) {
                    CardTitle(icon: "circle.lefthalf.filled", title: "外观主题")
                    Text("切换后立即生效并持久化；默认跟随系统。")
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.textSecondary)
                        .fixedSize(horizontal: false, vertical: true)

                    ForEach(Array(AppModel.AppearanceMode.allCases.indices), id: \.self) { index in
                        if index > 0 {
                            PolyDivider()
                        }
                        appearanceRow(AppModel.AppearanceMode.allCases[index])
                    }
                }
            }

            // MARK: 通知权限
            PolyCard {
                VStack(alignment: .leading, spacing: 12) {
                    CardTitle(icon: "bell.badge", title: "通知权限")

                    HStack(alignment: .top, spacing: 12) {
                        VStack(alignment: .leading, spacing: 3) {
                            Text("后台接收窗口内的本地通知")
                                .font(.system(size: 13, weight: .medium))
                                .foregroundStyle(Theme.textPrimary)
                            Text("App 进入后台约 25 秒内收到内容时，用本地通知展示摘要；前台自动同步不依赖通知。")
                                .font(.system(size: 12))
                                .foregroundStyle(Theme.textSecondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        Spacer(minLength: 8)
                        StatusPill(
                            text: model.notificationPermission.title,
                            color: model.notificationPermission.color
                        )
                    }

                    if let actionTitle = model.notificationPermission.actionTitle {
                        Button(actionTitle) {
                            Task { await model.handleNotificationPermissionAction() }
                        }
                        .buttonStyle(PolySecondaryButtonStyle())
                    } else {
                        Button("打开系统设置") {
                            model.openSystemSettings()
                        }
                        .buttonStyle(PolySecondaryButtonStyle())
                    }
                }
            }

            // MARK: 首次引导 / 权限说明
            PolyCard {
                VStack(alignment: .leading, spacing: 12) {
                    CardTitle(icon: "checkmark.shield", title: "指引与说明")
                    Text("iOS 16+ 首次自动读取剪贴板需系统「允许粘贴」授权；后台不允许常驻连接/静默同步。")
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.textSecondary)
                        .fixedSize(horizontal: false, vertical: true)

                    Button {
                        showPermissions = true
                    } label: {
                        Label("权限与平台说明", systemImage: "hand.raised")
                    }
                    .buttonStyle(PolySecondaryButtonStyle())

                    Button {
                        model.presentOnboarding()
                    } label: {
                        Label("重新查看首次引导", systemImage: "sparkles")
                    }
                    .buttonStyle(PolySecondaryButtonStyle())
                }
            }

            // MARK: 关于
            PolyCard {
                VStack(alignment: .leading, spacing: 12) {
                    HStack {
                        CardTitle(icon: "info.circle", title: "关于")
                        Spacer(minLength: 8)
                        VersionBadge()
                    }
                    InfoRow(label: "版本", value: AppInfo.versionBadge)
                    PolyDivider()
                    InfoColumn(label: "Rust 核心", value: model.abiInfo, mono: true)
                    PolyDivider()
                    InfoColumn(
                        label: "设备 ID",
                        value: model.identity?.deviceId ?? "未初始化",
                        mono: true
                    )
                    PolyDivider()
                    InfoColumn(label: "数据目录", value: model.dataDirectory, mono: true)
                }
            }

            Text("drive-sync · MIT License · 数据仅在本机与已配对设备间端到端加密传输")
                .font(.system(size: 11))
                .foregroundStyle(Theme.textTertiary)
                .frame(maxWidth: .infinity, alignment: .center)
                .padding(.top, 4)
        }
        .sheet(isPresented: $showPermissions) {
            OnboardingView(mode: .permissions)
                .environmentObject(model)
                .preferredColorScheme(model.preferredColorScheme)
        }
    }

    // MARK: - 外观主题行

    private func appearanceRow(_ mode: AppModel.AppearanceMode) -> some View {
        Button {
            model.setAppearanceMode(mode)
        } label: {
            HStack(spacing: 12) {
                Image(systemName: mode.icon)
                    .font(.system(size: 15))
                    .foregroundStyle(model.appearanceMode == mode ? Theme.mint : Theme.textSecondary)
                    .frame(width: 22)

                VStack(alignment: .leading, spacing: 3) {
                    Text(mode.title)
                        .font(.system(size: 14, weight: .medium))
                        .foregroundStyle(Theme.textPrimary)
                    Text(mode.detail)
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.textSecondary)
                }

                Spacer(minLength: 8)

                Image(systemName: model.appearanceMode == mode ? "checkmark.circle.fill" : "circle")
                    .font(.system(size: 17))
                    .foregroundStyle(model.appearanceMode == mode ? Theme.mint : Theme.textTertiary)
            }
            .padding(.vertical, 6)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}
