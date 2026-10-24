//
//  DriveSyncApp.swift
//  drive-sync iOS
//
//  App 入口。SwiftUI 生命周期；scenePhase 驱动「前台连接 / 后台断开」，
//  因为 iOS 不允许后台常驻 WebSocket（详见 docs/mobile-ios.md）。
//
//  UI 为 PolyBot 双主题（深色/浅色 + 薄荷绿主色）：外观由 RootView 的
//  `.preferredColorScheme` 控制（跟随系统 / 深色 / 浅色），这里配置的
//  UIKit 标签栏/导航栏外观全部使用动态颜色，会随外观自动解析。
//

import SwiftUI
import UIKit

@main
struct DriveSyncApp: App {
    @StateObject private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    init() {
        Self.configureGlobalAppearance()
    }

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(model)
                .task {
                    await model.bootstrap()
                }
                .onChange(of: scenePhase) { newPhase in
                    model.scenePhaseChanged(newPhase)
                }
        }
    }

    /// 动态标签栏 / 导航栏外观：颜色来自 Theme 的动态调色板，
    /// 深浅模式切换时由系统按 trait 自动重解析（无需重建 appearance）。
    private static func configureGlobalAppearance() {
        let tabAppearance = UITabBarAppearance()
        tabAppearance.configureWithOpaqueBackground()
        tabAppearance.backgroundColor = Theme.uiBackground
        tabAppearance.shadowColor = Theme.uiBorder
        UITabBar.appearance().standardAppearance = tabAppearance
        UITabBar.appearance().scrollEdgeAppearance = tabAppearance
        UITabBar.appearance().tintColor = Theme.uiMint
        UITabBar.appearance().unselectedItemTintColor = Theme.uiTextSecondary

        let navigationAppearance = UINavigationBarAppearance()
        navigationAppearance.configureWithOpaqueBackground()
        navigationAppearance.backgroundColor = Theme.uiBackground
        navigationAppearance.shadowColor = Theme.uiBorder
        navigationAppearance.titleTextAttributes = [.foregroundColor: Theme.uiTextPrimary]
        navigationAppearance.largeTitleTextAttributes = [.foregroundColor: Theme.uiTextPrimary]
        UINavigationBar.appearance().standardAppearance = navigationAppearance
        UINavigationBar.appearance().scrollEdgeAppearance = navigationAppearance
        UINavigationBar.appearance().compactAppearance = navigationAppearance
    }
}
