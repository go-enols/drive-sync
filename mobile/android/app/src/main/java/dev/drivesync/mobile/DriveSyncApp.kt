package dev.drivesync.mobile

import android.app.Application
import androidx.appcompat.app.AppCompatDelegate

/**
 * 应用入口：强制 PolyBot 深色主题（不随系统浅色模式切换），并初始化进程级状态。
 */
class DriveSyncApp : Application() {
    override fun onCreate() {
        super.onCreate()
        AppCompatDelegate.setDefaultNightMode(AppCompatDelegate.MODE_NIGHT_YES)
        AppState.init(this)
    }
}
