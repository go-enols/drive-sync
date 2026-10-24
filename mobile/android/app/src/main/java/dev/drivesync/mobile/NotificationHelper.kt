package dev.drivesync.mobile

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat

/**
 * 通知：前台服务常驻状态 + 收到远端剪切板提醒。
 *
 * PolyBot 风格：薄荷绿强调色 + 白色通知图标；深/浅色模式下使用对应对比度的薄荷绿
 * （深色 #22E3B0 / 浅色 #0EA37F）。常驻通知带“同步当前剪贴板”一键 Action
 * （后台无法自动读剪贴板时的一键替代方案）。
 */
object NotificationHelper {
    const val CHANNEL_STATUS = "drive_sync_status"
    const val CHANNEL_CLIPBOARD = "drive_sync_clipboard"
    const val SERVICE_NOTIFICATION_ID = 1001
    const val CLIPBOARD_NOTIFICATION_ID = 1002
    const val FILE_NOTIFICATION_ID = 1003

    private const val REQUEST_OPEN_APP = 1
    private const val REQUEST_QUICK_SYNC = 2

    fun ensureChannels(context: Context) {
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        val status = NotificationChannel(
            CHANNEL_STATUS,
            "同步状态",
            NotificationManager.IMPORTANCE_LOW,
        )
        status.description = "drive-sync 前台同步服务状态（含一键同步 Action）"
        manager.createNotificationChannel(status)

        val clipboard = NotificationChannel(
            CHANNEL_CLIPBOARD,
            "剪切板同步",
            NotificationManager.IMPORTANCE_DEFAULT,
        )
        clipboard.description = "收到远端剪切板时提醒（不展示内容）"
        manager.createNotificationChannel(clipboard)
    }

    /** 按当前主题选择薄荷绿（浅色模式用更深的 #0EA37F 保证对比度）。 */
    private fun accent(context: Context): Int =
        if (ThemePrefs.isDark(context)) 0xFF22E3B0.toInt() else 0xFF0EA37F.toInt()

    private fun openAppIntent(context: Context): PendingIntent {
        val intent = Intent(context, MainActivity::class.java)
        return PendingIntent.getActivity(
            context,
            REQUEST_OPEN_APP,
            intent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
    }

    private fun quickSyncIntent(context: Context): PendingIntent {
        val intent = Intent(context, ClipboardQuickActivity::class.java)
        return PendingIntent.getActivity(
            context,
            REQUEST_QUICK_SYNC,
            intent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
    }

    /** 前台服务常驻通知（含一键同步 Action）。 */
    fun serviceNotification(context: Context, text: String): Notification {
        ensureChannels(context)
        return NotificationCompat.Builder(context, CHANNEL_STATUS)
            .setSmallIcon(R.drawable.ic_notification)
            .setColor(accent(context))
            .setContentTitle("drive-sync")
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .setContentIntent(openAppIntent(context))
            .addAction(0, "同步当前剪贴板", quickSyncIntent(context))
            .build()
    }

    /** 收到远端剪切板提醒（自动写入系统剪贴板后）。 */
    fun notifyClipboard(context: Context, fromDeviceId: String) {
        ensureChannels(context)
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        val from = if (fromDeviceId.isBlank()) "远端设备" else fromDeviceId
        val notification = NotificationCompat.Builder(context, CHANNEL_CLIPBOARD)
            .setSmallIcon(R.drawable.ic_notification)
            .setColor(accent(context))
            .setContentTitle("收到远端剪切板")
            .setContentText("已自动写入本机剪贴板（来自 $from）")
            .setAutoCancel(true)
            .setPriority(NotificationCompat.PRIORITY_DEFAULT)
            .setContentIntent(openAppIntent(context))
            .addAction(0, "同步当前剪贴板", quickSyncIntent(context))
            .build()
        try {
            manager.notify(CLIPBOARD_NOTIFICATION_ID, notification)
        } catch (_: SecurityException) {
            // 未授予 POST_NOTIFICATIONS 时静默失败
        }
    }

    /** 设备间下载/接收文件完成。 */
    fun notifyFileSaved(context: Context, name: String, ok: Boolean) {
        ensureChannels(context)
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        val notification = NotificationCompat.Builder(context, CHANNEL_CLIPBOARD)
            .setSmallIcon(R.drawable.ic_notification)
            .setColor(accent(context))
            .setContentTitle(if (ok) "文件已下载" else "文件保存失败")
            .setContentText(
                if (ok) "$name 已保存到 Downloads/drive-sync" else "$name 无法写入 Downloads",
            )
            .setAutoCancel(true)
            .setPriority(NotificationCompat.PRIORITY_DEFAULT)
            .setContentIntent(openAppIntent(context))
            .build()
        try {
            manager.notify(FILE_NOTIFICATION_ID, notification)
        } catch (_: SecurityException) {
            // 忽略
        }
    }

    fun updateServiceNotification(context: Context, text: String) {
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        try {
            manager.notify(SERVICE_NOTIFICATION_ID, serviceNotification(context, text))
        } catch (_: SecurityException) {
            // 忽略
        }
    }

    fun notifyServiceTimeout(context: Context) {
        updateServiceNotification(context, "后台同步已达系统时长上限，重新打开 App 可继续")
    }

    /** 开机自启失败（Android 14/15 限制）时的提示。 */
    fun notifyBootFallback(context: Context) {
        ensureChannels(context)
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        val notification = NotificationCompat.Builder(context, CHANNEL_CLIPBOARD)
            .setSmallIcon(R.drawable.ic_notification)
            .setColor(accent(context))
            .setContentTitle("后台同步未自动恢复")
            .setContentText("系统限制了开机启动前台服务，点按打开 drive-sync 继续同步")
            .setAutoCancel(true)
            .setContentIntent(openAppIntent(context))
            .build()
        try {
            manager.notify(CLIPBOARD_NOTIFICATION_ID, notification)
        } catch (_: SecurityException) {
            // 忽略
        }
    }
}
