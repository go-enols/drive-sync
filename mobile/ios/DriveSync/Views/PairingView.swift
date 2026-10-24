//
//  PairingView.swift
//  drive-sync iOS
//
//  配对页（PolyBot 深色主题）：发起配对（8 位码 + dsync-pair-v1: 二维码）/ 加入配对 / 已配对设备。
//

import SwiftUI

struct PairingView: View {
    @EnvironmentObject private var model: AppModel
    @State private var joinCode = ""

    var body: some View {
        PolyScreen {
            BrandHeader()

            // MARK: 发起配对（A 端）
            PolyCard {
                VStack(alignment: .leading, spacing: 14) {
                    CardTitle(icon: "qrcode", title: "发起配对（A 端）")
                    Text("生成 8 位配对码并告诉对方设备；对方也可直接扫描下方二维码。")
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.textSecondary)
                        .fixedSize(horizontal: false, vertical: true)

                    if let pairCode = model.pairCode {
                        VStack(spacing: 12) {
                            Text(pairCode.formattedCode)
                                .font(.system(size: 38, weight: .bold, design: .monospaced))
                                .tracking(2)
                                .foregroundStyle(Theme.mint)
                                .frame(maxWidth: .infinity)
                                .padding(.vertical, 14)
                                .background(
                                    RoundedRectangle(cornerRadius: Theme.cornerSmall, style: .continuous)
                                        .fill(Theme.background)
                                )
                                .overlay(
                                    RoundedRectangle(cornerRadius: Theme.cornerSmall, style: .continuous)
                                        .strokeBorder(Theme.mint.opacity(0.35), lineWidth: 1)
                                )

                            Text("约 120 秒内有效，一次性使用")
                                .font(.system(size: 11))
                                .foregroundStyle(Theme.textTertiary)

                            Button {
                                model.copyToPasteboard(text: pairCode.qr)
                            } label: {
                                Label("复制 dsync-pair-v1: 配对串", systemImage: "doc.on.doc")
                            }
                            .buttonStyle(PolyLinkButtonStyle())

                            if !pairCode.qr.isEmpty {
                                QRCodeView(text: pairCode.qr)
                                    .frame(width: 180, height: 180)
                                    .frame(maxWidth: .infinity, alignment: .center)
                            }
                        }
                    } else {
                        EmptyHint(icon: "number", text: "点下面的按钮发起配对，配对码会显示在这里。")
                    }

                    Button {
                        Task { await model.pairStart() }
                    } label: {
                        HStack(spacing: 8) {
                            if model.pairingBusy {
                                ProgressView()
                                    .tint(Theme.buttonText)
                            }
                            Text(model.pairingBusy
                                 ? "正在生成配对码…"
                                 : (model.pairCode == nil ? "发起配对（生成 8 位码）" : "重新生成配对码"))
                        }
                    }
                    .buttonStyle(PolyPrimaryButtonStyle())
                    .disabled(!model.core.isRunning || model.pairingBusy)
                    .opacity(!model.core.isRunning || model.pairingBusy ? 0.5 : 1.0)

                    if !model.core.isRunning {
                        EmptyHint(icon: "link.badge.plus", text: "配对需要中继连接：先到「连接」页连接中继。")
                    }
                }
            }

            // MARK: 加入配对（B 端）
            PolyCard {
                VStack(alignment: .leading, spacing: 14) {
                    CardTitle(icon: "arrow.right.circle", title: "加入配对（B 端）")

                    PolyTextField(
                        title: "配对码或扫码串",
                        placeholder: "8 位数字或 dsync-pair-v1: 串",
                        text: $joinCode,
                        icon: "number",
                        keyboard: .numbersAndPunctuation
                    )

                    Button {
                        let code = joinCode
                        joinCode = ""
                        Task { await model.pairJoin(code: code) }
                    } label: {
                        HStack(spacing: 8) {
                            if model.pairingBusy {
                                ProgressView()
                                    .tint(Theme.buttonText)
                            }
                            Text(model.pairingBusy ? "正在配对（最多约 130 秒）…" : "加入配对")
                        }
                    }
                    .buttonStyle(PolyPrimaryButtonStyle())
                    .disabled(joinCode.isBlank || !model.core.isRunning || model.pairingBusy)
                    .opacity(joinCode.isBlank || !model.core.isRunning || model.pairingBusy ? 0.5 : 1.0)

                    if let join = model.lastPairJoin {
                        InfoRow(label: "最近配对", value: "\(join.peerName)（SAS \(join.sas)）")
                    }

                    Text("用 iPhone 相机扫描对方二维码可得到 dsync-pair-v1: 串，粘贴到输入框即可；SAS 在原型阶段自动确认。")
                        .font(.system(size: 11))
                        .foregroundStyle(Theme.textTertiary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }

            // MARK: 已配对设备
            PolyCard {
                VStack(alignment: .leading, spacing: 12) {
                    HStack {
                        CardTitle(icon: "person.2", title: "已配对设备（\(model.peers.count)）")
                        Spacer(minLength: 8)
                        Button("刷新") {
                            Task { await model.refreshLiveData() }
                        }
                        .buttonStyle(PolyLinkButtonStyle())
                        .disabled(!model.core.isRunning)
                    }

                    if model.peers.isEmpty {
                        EmptyHint(icon: "person.badge.plus", text: "暂无已配对设备。配对成功后，对方会出现在这里。")
                    } else {
                        ForEach(model.peers) { peer in
                            PeerRow(peer: peer)
                            if peer.id != model.peers.last?.id {
                                PolyDivider()
                            }
                        }
                    }
                }
            }

            Text("配对永久有效（无 TTL），除非任一方显式删除本地配对记录；iOS 当前版本暂未提供删除配对入口（移动端能力降级，后续版本补齐）。")
                .font(.system(size: 11))
                .foregroundStyle(Theme.textTertiary)
                .frame(maxWidth: .infinity, alignment: .center)
                .padding(.top, 4)
        }
    }
}
