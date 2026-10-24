//
//  RootView.swift
//  drive-sync iOS
//
//  全局框架：深色背景 + 四个 Tab（连接 / 配对 / 同步 / 目录）+ Toast + 首次引导。
//  主题规范见 Theme/Theme.swift（PolyBot：深色 + 薄荷绿）。
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
            }
            .tint(Theme.mint)
        }
        .overlay(alignment: .top) {
            ToastView()
        }
        .fullScreenCover(isPresented: $model.showOnboarding) {
            OnboardingView(mode: .firstRun)
                .environmentObject(model)
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
        .preferredColorScheme(.dark)
    }
}
