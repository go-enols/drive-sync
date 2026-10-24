//
//  RootView.swift
//  drive-sync iOS
//
//  全局框架：主题背景 + 五个 Tab（连接 / 配对 / 同步 / 目录 / 设置）+ Toast + 首次引导。
//  主题规范见 Theme/Theme.swift（PolyBot：深色/浅色双主题，薄荷绿主色）。
//  外观主题由 AppModel.preferredColorScheme 驱动（跟随系统 / 深色 / 浅色）。
//

import SwiftUI

struct RootView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        ZStack {
            Theme.backgroundGradient.ignoresSafeArea()

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

                SettingsView()
                    .tag(AppModel.Tab.settings)
                    .tabItem {
                        Label("设置", systemImage: "gearshape")
                    }
            }
            .tint(Theme.mint)
            // 让系统管理的标签栏跟随当前外观（nil = 跟随系统）。
            .toolbarColorScheme(model.preferredColorScheme, for: .tabBar)
        }
        .overlay(alignment: .top) {
            ToastView()
        }
        .fullScreenCover(isPresented: $model.showOnboarding) {
            OnboardingView(mode: .firstRun)
                .environmentObject(model)
                .preferredColorScheme(model.preferredColorScheme)
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
        // nil = 跟随系统；.light / .dark = 用户在设置页手动指定，立即生效。
        .preferredColorScheme(model.preferredColorScheme)
    }
}
