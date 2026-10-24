//
//  CatalogView.swift
//  drive-sync iOS
//
//  目录页（PolyBot 深色主题）：只读展示已配对设备同步来的软件目录（dm_catalog_json）。
//

import SwiftUI

struct CatalogView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        PolyScreen {
            BrandHeader(subtitle: "已配对设备同步的软件目录（只读）")

            PolyCard {
                VStack(alignment: .leading, spacing: 12) {
                    HStack {
                        CardTitle(icon: "square.stack.3d.up", title: "目录概览")
                        Spacer(minLength: 8)
                        Button("刷新") {
                            Task { await model.refreshCatalog() }
                        }
                        .buttonStyle(PolyLinkButtonStyle())
                        .disabled(!model.core.isRunning)
                    }

                    HStack(spacing: 10) {
                        MetricTile(value: "\(model.catalog.entries.count)", label: "条目数量")
                        MetricTile(value: "\(model.catalog.revision)", label: "目录版本")
                    }

                    EmptyHint(
                        icon: "info.circle",
                        text: "目录来自桌面端/Android 端的软件清单同步；文件本体传输不在本轮 iOS 范围。"
                    )
                }
            }

            PolyCard {
                VStack(alignment: .leading, spacing: 12) {
                    CardTitle(icon: "shippingbox", title: "条目")

                    if model.catalog.entries.isEmpty {
                        EmptyHint(
                            icon: "shippingbox",
                            text: "暂无目录条目。先在桌面端或 Android 端添加目录并完成同步，再回来刷新。"
                        )
                    } else {
                        ForEach(model.catalog.entries) { entry in
                            CatalogRow(entry: entry)
                            if entry.id != model.catalog.entries.last?.id {
                                PolyDivider()
                            }
                        }
                    }
                }
            }
        }
        .task {
            await model.refreshCatalog()
        }
    }
}
