package dev.drivesync.mobile

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent

/** 通知：前台服务常驻状态 + 收到远端剪切板提醒（不展示内容明文）。 */
object NotificationHelper {
    const val CHANNEL_STATUS = "drive_sync_status"
    const val CHANNEL_CLIPBOARD = "drive_sync_clipboard"
    const val SERVICE_NOTIFICATION_ID = 1001
    const val CLIPBOARD_NOTIFICATION_ID = 1002

    fun ensureChannels(context: Context) {
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        val status = NotificationChannel(
            CHANNEL_STATUS,
            "同步状态",
            NotificationManager.IMPORTANCE_LOW,
        )
        status.description = "drive-sync 前台同步服务状态"
        manager.createNotificationChannel(status)

        val clipboard = NotificationChannel(
            CHANNEL_CLIPBOARD,
            "剪切板同步",
            NotificationManager.IMPORTANCE_DEFAULT,
        )
        clipboard.description = "收到远端剪切板时提醒（不展示内容）"
        manager.createNotificationChannel(clipboard)
    }

    fun serviceNotification(context: Context, text: String): Notification {
        ensureChannels(context)
        val intent = Intent(context, MainActivity::class.java)
        val pending = PendingIntent.getActivity(
            context,
            0,
            intent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return Notification.Builder(context, CHANNEL_STATUS)
            .setSmallIcon(android.R.drawable.stat_notify_sync)
            .setContentTitle("drive-sync 正在同步")
            .setContentText(text)
            .setOngoing(true)
            .setContentIntent(pending)
            .build()
    }

    fun notifyClipboard(context: Context, fromDeviceId: String) {
        ensureChannels(context)
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        val from = if (fromDeviceId.isBlank()) "远端设备" else fromDeviceId
        val notification = Notification.Builder(context, CHANNEL_CLIPBOARD)
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentTitle("收到远端剪切板")
            .setContentText("已自动写入本机剪贴板（来自 $from）")
            .setAutoCancel(true)
            .build()
        try {
            manager.notify(CLIPBOARD_NOTIFICATION_ID, notification)
        } catch (_: SecurityException) {
            // 未授予 POST_NOTIFICATIONS 时静默失败
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
}
