//
//  EventLogView.swift
//  drive-sync iOS
//
//  事件日志弹层（PolyBot 深色主题）：最近 200 条事件（started/connected/clipboard/pair/relay_error 等）。
//  日志只含设备 id、大小、错误信息，不含剪贴板明文（Rust 侧已保证）。
//

import SwiftUI

struct EventLogView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss

    private var reversedLogs: [LogLine] {
        Array(model.logs.reversed())
    }

    var body: some View {
        ZStack {
            Theme.backgroundGradient.ignoresSafeArea()

            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    SheetHeader(
                        title: "事件日志",
                        subtitle: "最近 \(model.logs.count) 条 · 不含剪贴板明文"
                    ) {
                        dismiss()
                    }

                    PolyCard {
                        VStack(alignment: .leading, spacing: 0) {
                            if model.logs.isEmpty {
                                EmptyHint(icon: "list.bullet.rectangle", text: "暂无日志。连接中继、配对或收到内容后这里会有记录。")
                            } else {
                                ForEach(reversedLogs) { line in
                                    logRow(line)
                                    if line.id != reversedLogs.last?.id {
                                        PolyDivider()
                                            .padding(.vertical, 10)
                                    }
                                }
                            }
                        }
                    }

                    Button("清空日志") {
                        model.clearLogs()
                    }
                    .buttonStyle(PolySecondaryButtonStyle())
                    .disabled(model.logs.isEmpty)
                }
                .padding(.horizontal, 16)
                .padding(.top, 14)
                .padding(.bottom, 32)
            }
            .scrollIndicators(.hidden)
        }
        .preferredColorScheme(.dark)
    }

    private func logRow(_ line: LogLine) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: line.level.icon)
                .font(.system(size: 13))
                .foregroundStyle(line.level.color)
                .padding(.top, 1)
            VStack(alignment: .leading, spacing: 3) {
                Text(line.text)
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.textPrimary)
                    .fixedSize(horizontal: false, vertical: true)
                Text(TimeFormat.dateTime.string(from: line.date))
                    .font(.system(size: 10, design: .monospaced))
                    .foregroundStyle(Theme.textTertiary)
            }
            Spacer(minLength: 0)
        }
    }
}
