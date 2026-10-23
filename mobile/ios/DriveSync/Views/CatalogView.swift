//
//  CatalogView.swift
//  drive-sync iOS
//
//  软件目录只读展示（dm_catalog_json）。目录由已配对设备同步而来，
//  文件本体传输不在本轮 iOS 范围。
//

import SwiftUI

struct CatalogView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        NavigationStack {
            List {
                Section {
                    LabeledContent("目录版本") {
                        Text("revision \(model.catalog.revision)")
                    }
                    LabeledContent("条目数量") {
                        Text("\(model.catalog.entries.count)")
                    }
                } footer: {
                    Text("目录来自已配对设备同步（只读展示）。软件本体/文件传输不在本轮 iOS 范围。")
                }

                if model.catalog.entries.isEmpty {
                    Section {
                        Text("暂无目录条目。先在桌面端或 Android 端添加目录并完成同步，再回来刷新。")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    }
                } else {
                    Section("条目") {
                        ForEach(model.catalog.entries) { entry in
                            CatalogRow(entry: entry)
                        }
                    }
                }
            }
            .navigationTitle("目录")
            .refreshable {
                await model.refreshCatalog()
            }
            .toolbar {
                ToolbarItem(placement: .navigationBarTrailing) {
                    Button {
                        Task { await model.refreshCatalog() }
                    } label: {
                        Image(systemName: "arrow.clockwise")
                    }
                    .disabled(!model.core.isRunning)
                }
            }
            .task {
                await model.refreshCatalog()
            }
        }
    }
}
