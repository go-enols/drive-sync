//
//  ConnectView.swift
//  drive-sync iOS
//
//  连接页（PolyBot 深色主题）：中继配置 / 本机身份 / 运行状态 / 权限与引导入口。
//

import SwiftUI

struct ConnectView: View {
    @EnvironmentObject private var model: AppModel

    @State private var showPermissions = false

    var body: some View {
        PolyScreen {
            BrandHeader()

            ConnectionStatusCard()

            // MARK: 中继与身份
            PolyCard {
                VStack(alignment: .leading, spacing: 14) {
                    CardTitle(icon: "server.rack", title: "中继与身份")

                    PolyTextField(
                        title: "中继地址",
                        placeholder: "ws://192.168.1.10:8787/ws",
                        text: $model.relayURL,
                        icon: "link",
                        keyboard: .URL
                    )

                    PolySecureField(
                        title: "访问令牌（access token）",
                        placeholder: "自建 relay 的共享令牌",
                        text: $model.accessToken,
                        icon: "key.fill"
                    )

                    PolyTextField(
                        title: "设备名称",
                        placeholder: "例如：我的 iPhone",
                        text: $model.deviceName,
                        icon: "iphone"
                    )

                    if model.identity == nil {
                        Button {
                            Task { await model.initializeIdentity() }
                        } label: {
                            Label("初始化设备身份", systemImage: "person.badge.key")
                        }
                        .buttonStyle(PolyPrimaryButtonStyle())
                    } else if model.core.isRunning {
                        Button(role: .destructive) {
                            Task { await model.disconnect() }
                        } label: {
                            Label("断开连接", systemImage: "stop.circle")
                        }
                        .buttonStyle(PolyDangerButtonStyle())
                    } else {
                        Button {
                            Task { await model.connect() }
                        } label: {
                            HStack(spacing: 8) {
                                if model.connecting {
                                    ProgressView()
                                        .tint(Theme.buttonText)
                                }
                                Text(model.connecting ? "连接中…" : "保存配置并连接")
                            }
                        }
                        .buttonStyle(PolyPrimaryButtonStyle())
                        .disabled(model.connecting || model.relayURL.isBlank)
                        .opacity(model.connecting || model.relayURL.isBlank ? 0.5 : 1.0)
                    }

                    Text("iOS 仅前台保持连接：后台保留约 25 秒接收窗口后断开，回到前台自动重连；未接入 APNs 推送。")
                        .font(.system(size: 11))
                        .foregroundStyle(Theme.textTertiary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }

            // MARK: 本机身份
            PolyCard {
                VStack(alignment: .leading, spacing: 12) {
                    CardTitle(icon: "person.crop.circle", title: "本机身份")
                    InfoColumn(
                        label: "设备 ID",
                        value: model.identity?.deviceId ?? "未初始化",
                        mono: true
                    )
                    PolyDivider()
                    InfoRow(label: "设备名", value: model.identity?.name ?? "-")
                    PolyDivider()
                    InfoColumn(label: "Rust 核心", value: model.abiInfo, mono: true)
                    PolyDivider()
                    InfoColumn(label: "数据目录", value: model.dataDirectory, mono: true)
                }
            }

            // MARK: 运行状态
            PolyCard {
                VStack(alignment: .leading, spacing: 12) {
                    CardTitle(icon: "waveform.path.ecg", title: "运行状态")
                    InfoRow(label: "中继", value: relayDisplay)
                    PolyDivider()
                    InfoRow(label: "在线设备", value: "\(model.status.peersOnline)")
                    PolyDivider()
                    InfoRow(label: "已配对设备", value: "\(model.peers.count)")
                    PolyDivider()
                    InfoRow(label: "配对进行中", value: model.status.pairing ? "是" : "否")
                    PolyDivider()
                    InfoRow(
                        label: "最近发送",
                        value: model.lastSentAt.map { TimeFormat.dateTime.string(from: $0) } ?? "-"
                    )
                }
            }

            // MARK: 权限与引导
            PolyCard {
                VStack(alignment: .leading, spacing: 12) {
                    CardTitle(icon: "checkmark.shield", title: "权限与平台说明")
                    Text("通知权限、iOS 16+ 粘贴授权、后台限制与首次引导都在「权限与平台说明」页查看。")
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.textSecondary)
                        .fixedSize(horizontal: false, vertical: true)

                    Button {
                        showPermissions = true
                    } label: {
                        Label("查看权限与平台说明", systemImage: "hand.raised")
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

            Text("数据仅在本机与已配对设备间经端到端加密传输，中继只见密文。")
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

    private var relayDisplay: String {
        if !model.status.relayURL.isEmpty {
            return model.status.relayURL
        }
        return model.relayURL.isBlank ? "-" : model.relayURL
    }
}
