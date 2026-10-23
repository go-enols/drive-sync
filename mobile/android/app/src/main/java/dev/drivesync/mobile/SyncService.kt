package dev.drivesync.mobile

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.IBinder
import androidx.core.content.ContextCompat
import org.json.JSONObject

/**
 * 前台服务：保持进程存活并驱动剪切板轮询，实现“退到后台仍自动同步”。
 *
 * - targetSdk 36 声明 `foregroundServiceType="dataSync"`；
 * - Android 15（API 35）+ 对 dataSync 类型前台服务有 24 小时内累计 6 小时限制，
 *   超时回调 [onTimeout]，此时只能重新打开 App 恢复（原型限制，见 docs/mobile-android.md）；
 * - 后台读取剪贴板仍受系统限制：Android 10+ 需要用户在系统/侧载场景显式授权
 *   （`adb shell appops set dev.drivesync.mobile READ_CLIPBOARD allow`），未授权时降级为仅前台自动。
 */
class SyncService : Service() {
    private val eventListener: (JSONObject) -> Unit = { event ->
        when (event.optString("type")) {
            "connected" -> NotificationHelper.updateServiceNotification(
                this,
                "已连接中继 · 剪切板自动同步中",
            )
            "presence" -> {
                val online = event.optJSONArray("online")?.length() ?: 0
                NotificationHelper.updateServiceNotification(this, "已连接中继 · 在线设备 $online")
            }
            "stopped" -> NotificationHelper.updateServiceNotification(
                this,
                "连接已断开（服务保持运行，可重新打开 App 连接）",
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
        startForeground(
            NotificationHelper.SERVICE_NOTIFICATION_ID,
            NotificationHelper.serviceNotification(this, "正在同步（剪切板自动同步）"),
            ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
        )
        AppState.addEventListener(eventListener)
        ClipboardSync.ensureStarted()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        ClipboardSync.ensureStarted()
        return START_STICKY
    }

    override fun onDestroy() {
        AppState.removeEventListener(eventListener)
        ClipboardSync.stop()
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
