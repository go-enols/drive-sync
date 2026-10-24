package dev.drivesync.mobile

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log

/**
 * 可选“开机自启”：Android 13+ 需要通知权限才可见提示；Android 14/15 对
 * BOOT_COMPLETED 启动 dataSync 前台服务有限制，失败时发通知提示用户手动打开 App。
 */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Intent.ACTION_BOOT_COMPLETED) {
            return
        }
        val prefs = context.getSharedPreferences(SyncLauncher.PREFS, Context.MODE_PRIVATE)
        if (!prefs.getBoolean(SyncLauncher.KEY_BOOT_AUTOSTART, false)) {
            return
        }
        AppState.init(context)
        try {
            SyncService.start(context)
        } catch (error: Exception) {
            Log.w("drive-sync", "开机启动前台服务被系统拒绝: ${error.message}")
            NotificationHelper.notifyBootFallback(context)
        }
    }
}
