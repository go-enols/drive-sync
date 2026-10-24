package dev.drivesync.mobile

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.IBinder
import androidx.core.content.ContextCompat
import org.json.JSONObject

/**
 * 前台服务：保持连接与后台接收（收到远端内容自动写剪贴板 + 通知）。
 *
 * - targetSdk 36 声明 `foregroundServiceType="dataSync"`；
 * - 通知常驻“同步当前剪贴板”Action：后台无法自动读剪贴板时的官方一键方案（无需 adb）；
 * - Android 15（API 35）+ 对 dataSync 类型前台服务有 24 小时内累计 6 小时限制，
 *   超时回调 [onTimeout]；重新打开 App 可继续；
 * - 若进程被系统重建，服务会用本地保存的配置自动恢复原生运行时。
 */
class SyncService : Service() {
    private val eventListener: (JSONObject) -> Unit = { event ->
        when (event.optString("type")) {
            "connected" -> NotificationHelper.updateServiceNotification(
                this,
                "已连接中继 · 后台接收自动同步中（发送可点下方按钮）",
            )
            "presence" -> {
                val online = event.optJSONArray("online")?.length() ?: 0
                NotificationHelper.updateServiceNotification(
                    this,
                    "已连接中继 · 在线设备 $online · 点“同步当前剪贴板”一键发送",
                )
            }
            "stopped" -> NotificationHelper.updateServiceNotification(
                this,
                "连接已断开（服务保持运行，重新打开 App 可重连）",
            )
            "relay_error", "error" -> NotificationHelper.updateServiceNotification(
                this,
                "同步异常：" + event.optString("message").take(80),
            )
        }
    }

    override fun onCreate() {
        super.onCreate()
        AppState.init(this)
        NotificationHelper.ensureChannels(this)
        try {
            startForeground(
                NotificationHelper.SERVICE_NOTIFICATION_ID,
                NotificationHelper.serviceNotification(this, "正在连接中继…"),
                ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
            )
        } catch (error: Exception) {
            // Android 14/15 对后台/开机启动 dataSync 前台服务有限制：降级为通知提示。
            NotificationHelper.notifyBootFallback(this)
            stopSelf()
            return
        }
        AppState.addEventListener(eventListener)
        val result = SyncLauncher.ensureRuntime(this)
        NotificationHelper.updateServiceNotification(
            this,
            if (result == "ok") {
                "已启动 · 前台自动同步中；后台可点“同步当前剪贴板”"
            } else {
                "等待配置：$result"
            },
        )
        ClipboardSync.ensureStarted()
        FloatingBallManager.syncFromPrefs(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        ClipboardSync.ensureStarted()
        FloatingBallManager.syncFromPrefs(this)
        return START_STICKY
    }

    override fun onDestroy() {
        AppState.removeEventListener(eventListener)
        super.onDestroy()
    }

    /** Android 15+：dataSync 前台服务累计时长超限（6h/24h）时回调。 */
    override fun onTimeout(startId: Int) {
        NotificationHelper.notifyServiceTimeout(this)
        stopSelf()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        fun start(context: Context) {
            ContextCompat.startForegroundService(context, Intent(context, SyncService::class.java))
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, SyncService::class.java))
        }
    }
}
