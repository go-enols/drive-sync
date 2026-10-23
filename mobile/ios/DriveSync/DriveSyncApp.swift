//
//  DriveSyncApp.swift
//  drive-sync iOS
//
//  App 入口。SwiftUI 生命周期；scenePhase 驱动「前台连接 / 后台断开」，
//  因为 iOS 不允许后台常驻 WebSocket（详见 docs/mobile-ios.md）。
//

import SwiftUI

@main
struct DriveSyncApp: App {
    @StateObject private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

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
}
