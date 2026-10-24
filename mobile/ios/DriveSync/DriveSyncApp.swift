//
//  DriveSyncApp.swift
//  drive-sync iOS
//
//  App 入口。SwiftUI 生命周期；scenePhase 驱动「前台连接 / 后台断开」，
//  因为 iOS 不允许后台常驻 WebSocket（详见 docs/mobile-ios.md）。
//
//  UI 统一为 PolyBot 深色主题（深色背景 + 薄荷绿主色），
//  这里额外配置 UIKit 的标签栏/导航栏外观，保证系统控件也是深色风格。
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
                .preferredColorScheme(.dark)
                .task {
                    await model.bootstrap()
                }
                .onChange(of: scenePhase) { newPhase in
                    model.scenePhaseChanged(newPhase)
                }
        }
    }

    /// 深色标签栏 / 导航栏外观（PolyBot 主题：背景 #0B0F14、边框 #1E2A33、主色薄荷绿）。
    private static func configureGlobalAppearance() {
        let tabAppearance = UITabBarAppearance()
        tabAppearance.configureWithOpaqueBackground()
        tabAppearance.backgroundColor = UIColor(Theme.background)
        tabAppearance.shadowColor = UIColor(Theme.border)
        UITabBar.appearance().standardAppearance = tabAppearance
        UITabBar.appearance().scrollEdgeAppearance = tabAppearance
        UITabBar.appearance().tintColor = UIColor(Theme.mint)
        UITabBar.appearance().unselectedItemTintColor = UIColor(Theme.textTertiary)

        let navigationAppearance = UINavigationBarAppearance()
        navigationAppearance.configureWithOpaqueBackground()
        navigationAppearance.backgroundColor = UIColor(Theme.background)
        navigationAppearance.shadowColor = UIColor(Theme.border)
        navigationAppearance.titleTextAttributes = [.foregroundColor: UIColor(Theme.textPrimary)]
        navigationAppearance.largeTitleTextAttributes = [.foregroundColor: UIColor(Theme.textPrimary)]
        UINavigationBar.appearance().standardAppearance = navigationAppearance
        UINavigationBar.appearance().scrollEdgeAppearance = navigationAppearance
        UINavigationBar.appearance().compactAppearance = navigationAppearance
    }
}
