package dev.drivesync.mobile

import android.app.Application

/**
 * 应用入口：按用户外观偏好应用 DayNight（跟随系统 / 深色 / 浅色），并初始化进程级状态。
 * 默认跟随系统；偏好与切换在「设置 → 外观主题」中调整。
 */
class DriveSyncApp : Application() {
    override fun onCreate() {
        super.onCreate()
        ThemePrefs.apply(this)
        AppState.init(this)
    }
}
