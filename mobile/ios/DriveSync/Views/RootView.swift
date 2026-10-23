//
//  RootView.swift
//  drive-sync iOS
//
//  四个 Tab：连接（配置+状态）/ 配对 / 同步（收发剪贴板）/ 目录。
//  UI 与 Android 端功能对齐（Android App 的配置、配对、运行三块 + 目录只读）。
//

import SwiftUI

struct RootView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        TabView(selection: $model.selectedTab) {
            ConnectView()
                .tag(AppModel.Tab.connect)
                .tabItem {
                    Label("连接", systemImage: "link")
                }

            PairingView()
                .tag(AppModel.Tab.pairing)
                .tabItem {
                    Label("配对", systemImage: "person.2")
                }

            SyncView()
                .tag(AppModel.Tab.sync)
                .tabItem {
                    Label("同步", systemImage: "arrow.left.arrow.right")
                }

            CatalogView()
                .tag(AppModel.Tab.catalog)
                .tabItem {
                    Label("目录", systemImage: "square.stack.3d.up")
                }
        }
        .overlay(alignment: .top) {
            ToastView()
        }
        .alert(
            "提示",
            isPresented: Binding(
                get: { model.errorText != nil },
                set: { newValue in
                    if !newValue {
                        model.errorText = nil
                    }
                }
            )
        ) {
            Button("好", role: .cancel) {
                model.errorText = nil
            }
        } message: {
            Text(model.errorText ?? "")
        }
    }
}
