//
//  PairingView.swift
//  drive-sync iOS
//
//  配对：A 端发起显示 8 位码（+ dsync-pair-v1: 二维码），B 端输入码/粘贴扫码串加入。
//  C ABI：dm_pair_start / dm_pair_join（阻塞，最多 ~130s，调用在后台线程）。
//

import SwiftUI

struct PairingView: View {
    @EnvironmentObject private var model: AppModel
    @State private var joinCode = ""

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    if !model.core.isRunning {
                        Text("先到「连接」页连接中继，才能配对。")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    }

                    if let pairCode = model.pairCode {
                        VStack(alignment: .leading, spacing: 10) {
                            Text("把这 8 位配对码告诉对方设备，或让对方扫描下面的二维码")
                                .font(.footnote)
                                .foregroundStyle(.secondary)
                            Text(pairCode.formattedCode)
                                .font(.system(size: 40, weight: .bold, design: .monospaced))
                                .textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .center)
                            Text("约 120 秒内有效，一次性使用。")
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                            if !pairCode.qr.isEmpty {
                                Button {
                                    model.copyToPasteboard(text: pairCode.qr)
                                } label: {
                                    Label("复制 dsync-pair-v1: 配对串", systemImage: "doc.on.doc")
                                        .font(.caption)
                                }
                                QRCodeView(text: pairCode.qr)
                                    .frame(width: 160, height: 160)
                                    .frame(maxWidth: .infinity, alignment: .center)
                            }
                        }
                        .padding(.vertical, 4)
                    } else {
                        Text("点下面的按钮发起配对，配对码会显示在这里。")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    }

                    Button {
                        Task { await model.pairStart() }
                    } label: {
                        if model.pairingBusy {
                            HStack(spacing: 8) {
                                ProgressView()
                                Text("正在生成配对码…")
                            }
                        } else {
                            Label(
                                model.pairCode == nil ? "发起配对（生成 8 位码）" : "重新生成配对码",
                                systemImage: "qrcode"
                            )
                        }
                    }
                    .disabled(!model.core.isRunning || model.pairingBusy)
                } header: {
                    Text("发起配对（A 端）")
                } footer: {
                    Text("生成配对码后配对在后台继续，成功后自动出现在设备列表，并恢复同步会话。")
                }

                Section {
                    TextField("8 位配对码或 dsync-pair-v1: 配对串", text: $joinCode)
                        .keyboardType(.numbersAndPunctuation)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()

                    Button {
                        let code = joinCode
                        joinCode = ""
                        Task { await model.pairJoin(code: code) }
                    } label: {
                        if model.pairingBusy {
                            HStack(spacing: 8) {
                                ProgressView()
                                Text("正在配对（最多约 130 秒）…")
                            }
                        } else {
                            Label("加入配对（B 端）", systemImage: "arrow.right.circle")
                        }
                    }
                    .disabled(joinCode.isBlank || !model.core.isRunning || model.pairingBusy)

                    if let join = model.lastPairJoin {
                        LabeledContent("最近配对") {
                            Text("\(join.peerName)（SAS \(join.sas)）")
                                .font(.caption)
                        }
                    }
                } header: {
                    Text("加入配对（B 端）")
                } footer: {
                    Text("用 iPhone 相机扫描对方屏幕上的二维码可得到 dsync-pair-v1: 串，粘贴到输入框即可。SAS 在原型阶段自动确认，无需手动比对。")
                }

                Section {
                    if model.peers.isEmpty {
                        Text("暂无已配对设备。配对成功后，对方会出现在这里。")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    } else {
                        ForEach(model.peers) { peer in
                            PeerRow(peer: peer)
                        }
                    }
                } header: {
                    HStack {
                        Text("已配对设备（\(model.peers.count)）")
                        Spacer()
                        Button("刷新") {
                            Task { await model.refreshLiveData() }
                        }
                        .font(.caption)
                        .disabled(!model.core.isRunning)
                    }
                } footer: {
                    Text("配对永久有效（无 TTL），除非任一方显式删除本地配对记录。本轮 iOS App 暂未提供删除入口，可用桌面端 `drive-sync peers remove`；删除后对端会收到撤销通知。")
                }
            }
            .navigationTitle("配对")
            .scrollDismissesKeyboard(.interactively)
        }
    }
}
