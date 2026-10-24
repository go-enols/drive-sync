package dev.drivesync.mobile

import android.content.Context
import org.json.JSONObject
import java.io.File

/**
 * 统一启动/停止原生同步运行时（Activity、前台服务、开机自启、透明同步页共用）。
 *
 * 配置来自 SharedPreferences（relay / token / device_name），首次由 [MainActivity] 写入；
 * 因此即使 Activity 不在、进程被重建，前台服务也能从本地配置恢复连接。
 */
object SyncLauncher {
    const val PREFS = "drive_sync"
    const val KEY_RELAY = "relay"
    const val KEY_TOKEN = "token"
    const val KEY_DEVICE_NAME = "device_name"
    const val KEY_FLOATING_BALL = "floating_ball"
    const val KEY_BOOT_AUTOSTART = "boot_autostart"
    const val KEY_ONBOARDING_DONE = "onboarding_done"

    /** 确保原生运行时已启动；返回 "ok" 表示成功，否则返回可展示的错误文案。 */
    @Synchronized
    fun ensureRuntime(context: Context): String {
        if (AppState.handle != 0L) {
            return "ok"
        }
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val relay = prefs.getString(KEY_RELAY, "").orEmpty().trim()
        val token = prefs.getString(KEY_TOKEN, "").orEmpty().trim()
        val deviceName = prefs.getString(KEY_DEVICE_NAME, "Android 手机").orEmpty().trim()
        if (relay.isEmpty() || token.isEmpty()) {
            return "尚未配置中继地址或访问令牌"
        }
        val dataDir = File(context.filesDir, "drive-sync").absolutePath
        val init = parseJson(Native.dmInit(dataDir, deviceName))
        if (!init.optBoolean("ok")) {
            return init.optString("error")
        }
        val config = JSONObject()
            .put("relay_url", relay)
            .put("access_token", token)
            .put("device_name", deviceName)
        val saved = parseJson(Native.dmConfigSetRelay(config.toString()))
        if (!saved.optBoolean("ok")) {
            return saved.optString("error")
        }
        val handle = Native.dmStart(relay, token, AppState.callback())
        if (handle == 0L) {
            return Native.dmLastError() ?: "启动同步运行时失败"
        }
        AppState.handle = handle
        return "ok"
    }

    /** 停止原生运行时（后台线程执行，避免阻塞主线程）。 */
    fun stopRuntime() {
        val handle = AppState.handle
        AppState.handle = 0L
        if (handle != 0L) {
            Thread { Native.dmStop(handle) }.start()
        }
    }

    fun parseJson(text: String?): JSONObject {
        return try {
            JSONObject(text ?: "{}")
        } catch (_: Exception) {
            JSONObject().put("ok", false).put("error", "原生返回的 JSON 解析失败")
        }
    }
}
