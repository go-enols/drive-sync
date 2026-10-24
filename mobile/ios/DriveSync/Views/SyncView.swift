//
//  SyncView.swift
//  drive-sync iOS
//
//  同步页（PolyBot 深色主题）：自动同步开关 / 手动兜底发送 / 接收记录 / 运行信息 / 事件日志。
//  iOS 平台现实（详见 docs/mobile-ios.md §9）：
//   - 读剪贴板仅前台自动（changeCount 轮询），iOS 16+ 首次需系统粘贴授权；
//   - 收到内容前台自动写入剪贴板；后台约 25 秒窗口内收到会发本地通知；
//   - 无 APNs 推送，长时间不在前台时收不到内容。
//

import SwiftUI

struct SyncView: View {
    @EnvironmentObject private var model: AppModel
    @State private var showLogs = false

    var body: some View {
        PolyScreen {
            BrandHeader()

            ConnectionStatusCard()

            // MARK: 自动同步
            PolyCard {
                VStack(alignment: .leading, spacing: 14) {
                    CardTitle(icon: "bolt.fill", title: "自动同步")

                    PolyToggleRow(
                        icon: "arrow.up.doc",
                        title: "自动同步剪贴板（前台）",
                        subtitle: "前台每 1.5 秒检测剪贴板变化并自动发送",
                        isOn: Binding(
                            get: { model.autoSyncEnabled },
                            set: { model.setAutoSyncEnabled($0) }
                        )
                    )

                    PolyDivider()

                    PolyToggleRow(
                        icon: "arrow.down.doc",
                        title: "收到内容后自动写入剪贴板",
                        subtitle: "收到其他设备内容后立即写入系统剪贴板",
                        isOn: Binding(
                            get: { model.autoApplyReceivedEnabled },
                            set: { model.setAutoApplyReceivedEnabled($0) }
                        )
                    )

                    if model.autoReadBlocked {
                        EmptyHint(
                            icon: "exclamationmark.triangle",
                            text: "自动读取被系统粘贴授权拦截：可点下方「发送当前剪贴板」手动兜底，或到 设置 → drive-sync → 从其他 App 粘贴 改为允许。"
                        )
                    }

                    Text("iOS 16+ 首次读取剪贴板会弹系统「允许粘贴？」；拒绝后自动同步会退避，可用手动按钮兜底。")
                        .font(.system(size: 11))
                        .foregroundStyle(Theme.textTertiary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }

            // MARK: 手动发送
            PolyCard {
                VStack(alignment: .leading, spacing: 12) {
                    CardTitle(icon: "paperplane.fill", title: "发送（本机 → 其他设备）")
                    Text("自动同步未生效或读取被拦截时，点按按钮显式读取当前剪贴板并发送。")
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.textSecondary)
                        .fixedSize(horizontal: false, vertical: true)

                    Button {
                        model.sendCurrentClipboard()
                    } label: {
                        Label("发送当前剪贴板", systemImage: "paperclip")
                    }
                    .buttonStyle(PolyPrimaryButtonStyle())
                    .disabled(!model.core.isRunning)
                    .opacity(model.core.isRunning ? 1.0 : 0.5)

                    if !model.clipboardPreview.isEmpty {
                        VStack(alignment: .leading, spacing: 6) {
                            Text("最近发送的预览")
                                .font(.system(size: 11))
                                .foregroundStyle(Theme.textTertiary)
                            Text(model.clipboardPreview)
                                .font(.system(size: 12, design: .monospaced))
                                .foregroundStyle(Theme.textPrimary)
                                .lineLimit(4)
                                .textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .padding(10)
                                .background(
                                    RoundedRectangle(cornerRadius: 8, style: .continuous)
                                        .fill(Theme.background)
                                )
                                .overlay(
                                    RoundedRectangle(cornerRadius: 8, style: .continuous)
                                        .strokeBorder(Theme.border, lineWidth: 1)
                                )
                        }
                    }
                }
            }

            // MARK: 接收记录
            PolyCard {
                VStack(alignment: .leading, spacing: 12) {
                    HStack {
                        CardTitle(icon: "tray.and.arrow.down", title: "接收记录（\(model.receivedItems.count)）")
                        Spacer(minLength: 8)
                        if !model.receivedItems.isEmpty {
                            Button("清空") {
                                model.clearReceivedItems()
                            }
                            .buttonStyle(PolyLinkButtonStyle())
                        }
                    }

                    if model.receivedItems.isEmpty {
                        EmptyHint(icon: "tray", text: "暂无接收记录。前台收到内容会自动写入剪贴板，并在这里保留一条记录。")
                    } else {
                        ForEach(model.receivedItems) { item in
                            ReceivedRow(item: item)
                            if item.id != model.receivedItems.last?.id {
                                PolyDivider()
                            }
                        }
                    }
                }
            }

            // MARK: 运行信息
            PolyCard {
                VStack(alignment: .leading, spacing: 12) {
                    CardTitle(icon: "waveform.path.ecg", title: "运行信息")
                    InfoRow(label: "中继", value: relayDisplay)
                    PolyDivider()
                    InfoRow(label: "在线设备", value: "\(model.status.peersOnline)")
                    PolyDivider()
                    InfoRow(label: "已配对设备", value: "\(model.peers.count)")
                    PolyDivider()
                    InfoRow(
                        label: "最近写入剪贴板",
                        value: model.lastAppliedAt.map { TimeFormat.dateTime.string(from: $0) } ?? "-"
                    )
                    PolyDivider()
                    InfoColumn(label: "设备 ID", value: model.identity?.deviceId ?? "-", mono: true)

                    Button {
                        showLogs = true
                    } label: {
                        Label("查看事件日志（\(model.logs.count)）", systemImage: "list.bullet.rectangle")
                    }
                    .buttonStyle(PolySecondaryButtonStyle())
                }
            }

            Text("没有 APNs 推送：App 长时间不在前台时收不到内容；回到前台会自动重连并恢复同步。")
                .font(.system(size: 11))
                .foregroundStyle(Theme.textTertiary)
                .frame(maxWidth: .infinity, alignment: .center)
                .padding(.top, 4)
        }
        .sheet(isPresented: $showLogs) {
            EventLogView()
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
