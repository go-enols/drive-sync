//
//  SyncView.swift
//  drive-sync iOS
//
//  运行页：自动同步开关 / 手动兜底发送 / 接收记录 / 运行状态。
//  iOS 平台现实（详见 docs/mobile-ios.md §9）：
//   - 读剪贴板无法后台进行：仅前台自动轮询 + iOS 16+ 首次系统粘贴授权；
//   - 收到内容在前台自动写入剪贴板；后台约 25 秒窗口内收到会发本地通知；
//   - 无 APNs 推送，App 长时间不在前台时收不到内容。
//

import SwiftUI

struct SyncView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    ConnectionStatusCard()

                    if !model.core.isRunning {
                        Text("未启动同步：到「连接」页保存配置并连接中继。")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    }

                    Toggle(
                        "自动同步剪贴板（前台）",
                        isOn: Binding(
                            get: { model.autoSyncEnabled },
                            set: { model.setAutoSyncEnabled($0) }
                        )
                    )
                    Toggle(
                        "收到内容后自动写入剪贴板",
                        isOn: Binding(
                            get: { model.autoApplyReceivedEnabled },
                            set: { model.setAutoApplyReceivedEnabled($0) }
                        )
                    )

                    if model.autoReadBlocked {
                        Label(
                            "自动读取被系统粘贴授权拦截：可点下面按钮手动发送一次，或到 设置 → DriveSync → 从其他 App 粘贴 改为允许。",
                            systemImage: "exclamationmark.triangle"
                        )
                        .font(.footnote)
                        .foregroundStyle(.orange)
                    }

                    Button {
                        model.sendCurrentClipboard()
                    } label: {
                        Label("发送当前剪贴板（手动兜底）", systemImage: "paperclip")
                            .frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(!model.core.isRunning)

                    if !model.clipboardPreview.isEmpty {
                        VStack(alignment: .leading, spacing: 4) {
                            Text("最近发送的预览")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            Text(model.clipboardPreview)
                                .font(.caption.monospaced())
                                .lineLimit(4)
                                .textSelection(.enabled)
                        }
                    }
                } header: {
                    Text("自动同步 / 发送（本机 → 其他设备）")
                } footer: {
                    Text("前台时每 1.5 秒检测一次剪贴板变化并自动发送。iOS 16+ 首次读取需在系统弹窗里允许粘贴（可到 设置 → DriveSync → 从其他 App 粘贴 改为始终允许）；App 在后台/未运行时无法读取剪贴板，这是 iOS 系统限制。仅支持文本。")
                }

                Section {
                    if model.receivedItems.isEmpty {
                        Text("暂无接收记录。前台收到其他设备的剪贴板文本会自动写入系统剪贴板，并在这里保留一条记录。")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    } else {
                        ForEach(model.receivedItems) { item in
                            ReceivedRow(item: item)
                        }
                    }
                } header: {
                    HStack {
                        Text("接收（其他设备 → 本机）")
                        Spacer()
                        if !model.receivedItems.isEmpty {
                            Button("清空") {
                                model.clearReceivedItems()
                            }
                            .font(.caption)
                        }
                    }
                } footer: {
                    Text("前台收到内容会立刻自动写入系统剪贴板，直接切换目标 App 粘贴即可；关闭自动写入时请点「复制到剪贴板」。后台接收窗口（约 25 秒）内收到会发本地通知，打开 App 后自动写入；没有 APNs 推送，长时间不在前台的内容不会被接收。")
                }

                Section {
                    LabeledContent("中继") {
                        Text(model.status.relayURL.isEmpty ? (model.relayURL.isBlank ? "-" : model.relayURL) : model.status.relayURL)
                            .font(.caption.monospaced())
                    }
                    LabeledContent("在线设备") {
                        Text("\(model.status.peersOnline)")
                    }
                    LabeledContent("已配对设备") {
                        Text("\(model.peers.count)")
                    }
                    LabeledContent("最近写入剪贴板") {
                        Text(model.lastAppliedAt.map { TimeFormat.dateTime.string(from: $0) } ?? "-")
                    }
                    LabeledContent("设备 ID") {
                        Text(model.identity?.deviceId ?? "-")
                            .font(.caption.monospaced())
                            .textSelection(.enabled)
                    }
                    NavigationLink {
                        EventLogView()
                    } label: {
                        HStack {
                            Text("事件日志")
                            Spacer()
                            Text("\(model.logs.count) 条")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    }
                } header: {
                    Text("运行状态")
                }

                Section {
                    Button {
                        Task { await model.refreshLiveData() }
                    } label: {
                        Label("刷新状态与设备列表", systemImage: "arrow.clockwise")
                    }
                    .disabled(!model.core.isRunning)
                }
            }
            .navigationTitle("同步")
        }
    }
}
