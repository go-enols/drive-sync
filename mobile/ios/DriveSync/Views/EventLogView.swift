//
//  EventLogView.swift
//  drive-sync iOS
//
//  最近 200 条事件日志（started/connected/clipboard/pair/relay_error 等）。
//  日志只含设备 id、大小、错误信息，不含剪贴板明文（Rust 侧已保证）。
//

import SwiftUI

struct EventLogView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        List {
            if model.logs.isEmpty {
                Text("暂无日志")
                    .foregroundStyle(.secondary)
            } else {
                ForEach(Array(model.logs.reversed())) { line in
                    HStack(alignment: .top, spacing: 8) {
                        Image(systemName: line.level.icon)
                            .foregroundStyle(line.level.color)
                            .font(.caption)
                            .padding(.top, 2)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(line.text)
                                .font(.caption)
                            Text(TimeFormat.dateTime.string(from: line.date))
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                        }
                    }
                    .padding(.vertical, 1)
                }
            }
        }
        .listStyle(.plain)
        .navigationTitle("事件日志")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .navigationBarTrailing) {
                Button("清空") {
                    model.clearLogs()
                }
                .disabled(model.logs.isEmpty)
            }
        }
    }
}
