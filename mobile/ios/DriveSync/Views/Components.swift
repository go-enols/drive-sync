//
//  Components.swift
//  drive-sync iOS
//
//  PolyBot 主题下的业务组件：连接状态卡、设备行、接收记录行、Toast、二维码、目录行。
//  基础样式（颜色/卡片/输入框）在 Theme/ 下，这里只组合业务展示。
//

import CoreImage
import SwiftUI
import UIKit

// MARK: - 日志级别样式

extension LogLine.Level {
    var color: Color {
        switch self {
        case .info: return Theme.textSecondary
        case .success: return Theme.mint
        case .warning: return Theme.warning
        case .failure: return Theme.danger
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
        PolyCard {
            VStack(alignment: .leading, spacing: 12) {
                HStack {
                    CardTitle(icon: "antenna.radiowaves.left.and.right", title: "连接状态")
                    Spacer(minLength: 8)
                    StatusPill(text: model.phase.title, color: model.phase.color)
                }
                Text(model.phase.detail)
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
                if model.phase.isOnline {
                    HStack(spacing: 10) {
                        MetricTile(value: "\(model.status.peersOnline)", label: "在线设备")
                        MetricTile(value: "\(model.peers.count)", label: "已配对")
                    }
                }
            }
        }
    }
}

// MARK: - 已配对设备行

struct PeerRow: View {
    let peer: PeerInfo

    var body: some View {
        HStack(spacing: 12) {
            ZStack {
                Circle()
                    .fill(Theme.mint.opacity(peer.isOnline ? 0.16 : 0.06))
                    .frame(width: 36, height: 36)
                Image(systemName: "iphone")
                    .font(.system(size: 14, weight: .medium))
                    .foregroundStyle(peer.isOnline ? Theme.mint : Theme.textTertiary)
            }
            VStack(alignment: .leading, spacing: 3) {
                Text(peer.displayName)
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(Theme.textPrimary)
                Text(peer.deviceId)
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(Theme.textTertiary)
                    .lineLimit(1)
            }
            Spacer(minLength: 8)
            VStack(alignment: .trailing, spacing: 4) {
                StatusPill(
                    text: peer.sessionActive ? "加密会话" : (peer.isOnline ? "在线" : "离线"),
                    color: peer.isOnline ? Theme.mint : Theme.textTertiary
                )
                Text("配对于 \(peer.pairedAtText)")
                    .font(.system(size: 10))
                    .foregroundStyle(Theme.textTertiary)
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
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Image(systemName: "arrow.down.left.circle.fill")
                    .font(.system(size: 13))
                    .foregroundStyle(Theme.mint)
                Text(item.peerName)
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Theme.textPrimary)
                Spacer(minLength: 8)
                Text(TimeFormat.time.string(from: item.receivedAt))
                    .font(.system(size: 10, design: .monospaced))
                    .foregroundStyle(Theme.textTertiary)
            }

            Text(item.text)
                .font(.system(size: 12, design: .monospaced))
                .foregroundStyle(Theme.textPrimary)
                .lineLimit(6)
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

            HStack {
                Button {
                    model.copyToPasteboard(text: item.text)
                } label: {
                    Label("复制到剪贴板", systemImage: "doc.on.doc")
                }
                .buttonStyle(PolyLinkButtonStyle())

                Spacer()

                Text("\(item.characterCount) 字符")
                    .font(.system(size: 10))
                    .foregroundStyle(Theme.textTertiary)
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
                HStack(spacing: 10) {
                    Image(systemName: toast.level.icon)
                        .font(.system(size: 14, weight: .semibold))
                        .foregroundStyle(toast.level.color)
                    Text(toast.text)
                        .font(.system(size: 13, weight: .medium))
                        .foregroundStyle(Theme.textPrimary)
                        .multilineTextAlignment(.leading)
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 11)
                .background(
                    RoundedRectangle(cornerRadius: Theme.cornerSmall, style: .continuous)
                        .fill(Theme.cardElevated)
                )
                .overlay(
                    RoundedRectangle(cornerRadius: Theme.cornerSmall, style: .continuous)
                        .strokeBorder(Theme.border, lineWidth: 1)
                )
                .shadow(color: Color.black.opacity(0.35), radius: 12, x: 0, y: 4)
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
                    .padding(10)
                    .background(
                        RoundedRectangle(cornerRadius: 10, style: .continuous)
                            .fill(Color.white)
                    )
                    .accessibilityLabel("配对二维码")
            } else {
                Text("二维码生成失败")
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.textTertiary)
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
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Text(entry.displayName)
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(Theme.textPrimary)
                Spacer(minLength: 6)
                if entry.isLocal {
                    StatusPill(text: "本机持有", color: Theme.mint)
                }
            }
            HStack(spacing: 10) {
                if let version = entry.version, !version.isEmpty {
                    Text("v\(version)")
                }
                Text(entry.platformText)
                Text(entry.displaySize)
                if entry.holderCount > 0 {
                    Text("\(entry.holderCount) 台持有")
                }
            }
            .font(.system(size: 11))
            .foregroundStyle(Theme.textSecondary)

            if let notes = entry.notes, !notes.isEmpty {
                Text(notes)
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.textTertiary)
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
