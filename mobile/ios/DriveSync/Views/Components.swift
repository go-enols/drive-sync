//
//  Components.swift
//  drive-sync iOS
//
//  跨页面复用的小组件：状态卡、设备行、接收记录行、Toast、二维码、目录行。
//

import CoreImage
import SwiftUI
import UIKit

// MARK: - 日志级别样式

extension LogLine.Level {
    var color: Color {
        switch self {
        case .info: return .secondary
        case .success: return .green
        case .warning: return .orange
        case .failure: return .red
        }
    }

    var icon: String {
        switch self {
        case .info: return "info.circle"
        case .success: return "checkmark.circle"
        case .warning: return "exclamationmark.triangle"
        case .failure: return "xmark.octagon"
        }
    }
}

// MARK: - 时间格式

enum TimeFormat {
    static let time: DateFormatter = {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "zh_CN")
        formatter.dateFormat = "HH:mm:ss"
        return formatter
    }()

    static let dateTime: DateFormatter = {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "zh_CN")
        formatter.dateFormat = "MM-dd HH:mm"
        return formatter
    }()
}

// MARK: - 连接状态卡

struct ConnectionStatusCard: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Circle()
                .fill(model.phase.color)
                .frame(width: 12, height: 12)
                .padding(.top, 6)
            VStack(alignment: .leading, spacing: 3) {
                Text(model.phase.title)
                    .font(.headline)
                Text(model.phase.detail)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                if model.phase.isOnline {
                    Text("在线设备 \(model.status.peersOnline) · 已配对 \(model.peers.count)")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
            Spacer()
            if model.connecting || model.pairingBusy {
                ProgressView()
            }
        }
        .padding(.vertical, 2)
    }
}

// MARK: - 已配对设备行

struct PeerRow: View {
    let peer: PeerInfo

    var body: some View {
        HStack(spacing: 10) {
            Circle()
                .fill(peer.isOnline ? Color.green : Color.secondary.opacity(0.35))
                .frame(width: 8, height: 8)
            VStack(alignment: .leading, spacing: 2) {
                Text(peer.displayName)
                    .font(.body.weight(.medium))
                Text(peer.deviceId)
                    .font(.caption2.monospaced())
                    .foregroundStyle(.secondary)
            }
            Spacer()
            VStack(alignment: .trailing, spacing: 2) {
                Text(peer.sessionActive ? "加密会话" : (peer.isOnline ? "在线" : "离线"))
                    .font(.caption2)
                    .foregroundStyle(peer.sessionActive ? Color.green : Color.secondary)
                Text("配对于 \(peer.pairedAtText)")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
        }
        .padding(.vertical, 2)
    }
}

// MARK: - 接收记录行

struct ReceivedRow: View {
    @EnvironmentObject private var model: AppModel
    let item: ReceivedItem

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(item.peerName)
                    .font(.body.weight(.medium))
                Spacer()
                Text(TimeFormat.time.string(from: item.receivedAt))
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            Text(item.text)
                .font(.caption.monospaced())
                .lineLimit(6)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
            HStack {
                Button {
                    model.copyToPasteboard(text: item.text)
                } label: {
                    Label("复制到剪贴板", systemImage: "doc.on.doc")
                        .font(.caption.weight(.medium))
                }
                .buttonStyle(.bordered)
                .controlSize(.small)

                Spacer()

                Text("\(item.characterCount) 字符")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 2)
    }
}

// MARK: - Toast 浮层

struct ToastView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        VStack {
            if let toast = model.toast {
                HStack(spacing: 8) {
                    Image(systemName: toast.level.icon)
                        .foregroundStyle(toast.level.color)
                    Text(toast.text)
                        .font(.footnote.weight(.medium))
                        .multilineTextAlignment(.leading)
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
                .background(.regularMaterial, in: Capsule())
                .overlay(Capsule().strokeBorder(Color.primary.opacity(0.08)))
                .padding(.horizontal, 16)
                .padding(.top, 8)
                .onTapGesture {
                    model.toast = nil
                }
                .transition(.move(edge: .top).combined(with: .opacity))
            }
            Spacer()
        }
        .animation(.spring(response: 0.3, dampingFraction: 0.8), value: model.toast)
    }
}

// MARK: - 二维码

struct QRCodeView: View {
    let text: String

    var body: some View {
        Group {
            if let image = QRCodeGenerator.image(for: text) {
                Image(uiImage: image)
                    .interpolation(.none)
                    .resizable()
                    .scaledToFit()
                    .accessibilityLabel("配对二维码")
            } else {
                Text("二维码生成失败")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
    }
}

enum QRCodeGenerator {
    private static let context = CIContext()

    /// 把 `dsync-pair-v1:...` 配对串渲染成二维码图片（CoreImage，无第三方依赖）。
    static func image(for text: String) -> UIImage? {
        guard let data = text.data(using: .utf8),
              let filter = CIFilter(name: "CIQRCodeGenerator") else {
            return nil
        }
        filter.setValue(data, forKey: "inputMessage")
        filter.setValue("M", forKey: "inputCorrectionLevel")
        guard let output = filter.outputImage else { return nil }

        let scale: CGFloat = 8
        let scaled = output.transformed(by: CGAffineTransform(scaleX: scale, y: scale))
        guard let cgImage = context.createCGImage(scaled, from: scaled.extent) else { return nil }
        return UIImage(cgImage: cgImage)
    }
}

// MARK: - 目录行

struct CatalogRow: View {
    let entry: CatalogItem

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                Text(entry.displayName)
                    .font(.body.weight(.medium))
                Spacer()
                if entry.isLocal {
                    Text("本机持有")
                        .font(.caption2)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .background(Color.green.opacity(0.15), in: Capsule())
                        .foregroundStyle(.green)
                }
            }
            HStack(spacing: 10) {
                if let version = entry.version, !version.isEmpty {
                    Text("v\(version)")
                }
                Text(entry.platformText)
                Text(entry.displaySize)
                if entry.holderCount > 0 {
                    Text("\(entry.holderCount) 台设备持有")
                }
            }
            .font(.caption2)
            .foregroundStyle(.secondary)

            if let notes = entry.notes, !notes.isEmpty {
                Text(notes)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }
        }
        .padding(.vertical, 2)
    }
}

// MARK: - 小工具

extension String {
    /// 配置输入常用：去掉首尾空白，并判断是否为空。
    var isBlank: Bool {
        trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }
}
