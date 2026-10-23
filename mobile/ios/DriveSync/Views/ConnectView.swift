//
//  ConnectView.swift
//  drive-sync iOS
//
//  配置（relay / token / 设备名）+ 本机身份 + 中继状态。
//  「保存配置并连接」= dm_config_set_relay → dm_start（Rust 后台运行时）。
//

import SwiftUI

struct ConnectView: View {
    @EnvironmentObject private var model: AppModel
    @State private var showToken = false

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    ConnectionStatusCard()
                }

                Section {
                    TextField(
                        "中继地址",
                        text: $model.relayURL,
                        prompt: Text("ws://192.168.1.10:8787/ws")
                    )
                    .keyboardType(.URL)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()

                    if showToken {
                        TextField("访问令牌（access token）", text: $model.accessToken)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                    } else {
                        SecureField("访问令牌（access token）", text: $model.accessToken)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                    }

                    TextField("设备名称", text: $model.deviceName, prompt: Text("例如：我的 iPhone"))

                    Toggle("显示访问令牌", isOn: $showToken)
                } header: {
                    Text("中继与身份")
                } footer: {
                    Text("中继地址填自建 relay 的 WebSocket 地址（开发用 ws://，生产建议 wss:// 由 Caddy/Nginx 终止 TLS）。访问令牌保存在 Keychain，不写 UserDefaults。")
                }

                Section {
                    if model.identity == nil {
                        Button {
                            Task { await model.initializeIdentity() }
                        } label: {
                            Label("初始化设备身份", systemImage: "person.badge.key")
                        }
                    } else if model.core.isRunning {
                        Button(role: .destructive) {
                            Task { await model.disconnect() }
                        } label: {
                            Label("断开连接", systemImage: "stop.circle")
                        }
                    } else {
                        Button {
                            Task { await model.connect() }
                        } label: {
                            Label("保存配置并连接", systemImage: "play.circle")
                        }
                        .disabled(model.connecting || model.relayURL.isBlank)
                    }
                } footer: {
                    Text("iOS 只允许前台保持 WebSocket：切到后台后保留约 25 秒接收窗口（可收一条内容并发本地通知），随后自动断开，回到前台自动重连；后台常驻同步/APNs 推送本轮未实现。")
                }

                Section {
                    LabeledContent("设备 ID") {
                        Text(model.identity?.deviceId ?? "未初始化")
                            .font(.caption.monospaced())
                            .textSelection(.enabled)
                    }
                    LabeledContent("设备名") {
                        Text(model.identity?.name ?? "-")
                    }
                    LabeledContent("数据目录") {
                        Text(model.dataDirectory)
                            .font(.caption2)
                            .textSelection(.enabled)
                    }
                    LabeledContent("Rust 核心") {
                        Text(model.abiInfo)
                            .font(.caption2)
                    }
                } header: {
                    Text("本机身份")
                } footer: {
                    Text("身份密钥在 App 沙盒 Application Support/drive-sync；删除 App 会丢失身份，需要重新配对。")
                }

                Section("中继状态") {
                    LabeledContent("连接状态") {
                        Text(model.phase.title)
                    }
                    LabeledContent("中继地址") {
                        Text(model.status.relayURL.isEmpty ? (model.relayURL.isBlank ? "-" : model.relayURL) : model.status.relayURL)
                            .font(.caption.monospaced())
                    }
                    LabeledContent("在线设备") {
                        Text("\(model.status.peersOnline)")
                    }
                    LabeledContent("已配对设备") {
                        Text("\(model.peers.count)")
                    }
                    LabeledContent("配对进行中") {
                        Text(model.status.pairing ? "是" : "否")
                    }
                    LabeledContent("最近发送") {
                        Text(model.lastSentAt.map { TimeFormat.dateTime.string(from: $0) } ?? "-")
                    }
                }

                Section {
                    Button {
                        model.selectedTab = .pairing
                    } label: {
                        Label("去配对设备", systemImage: "person.2")
                    }
                } footer: {
                    Text("配对成功后设备会出现在「配对」页；已配对设备在线时自动建立端到端加密会话。")
                }
            }
            .navigationTitle("连接")
            .scrollDismissesKeyboard(.interactively)
        }
    }
}
