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
    const val KEY_AUTO_START = "auto_start_sync"

    /** 是否允许“打开应用时自动开始同步”（默认开启）。 */
    fun isAutoStartEnabled(context: Context): Boolean =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getBoolean(KEY_AUTO_START, true)

    fun setAutoStartEnabled(context: Context, enabled: Boolean) {
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putBoolean(KEY_AUTO_START, enabled)
            .apply()
    }

    /**
     * 公网中继自动升级到 wss：明文 ws:// 打到 Cloudflare/nginx 的 443 域名会被 301 拒绝。
     * 局域网/本机地址保留 ws://（没有证书也方便自建）。
     */
    fun normalizeRelayUrl(raw: String): String {
        val trimmed = raw.trim()
        if (!trimmed.startsWith("ws://", ignoreCase = true)) return trimmed
        val rest = trimmed.substring(5)
        val authority = rest.substringBefore('/')
        val host = if (authority.startsWith("[")) {
            authority.substringAfter('[').substringBefore(']')
        } else {
            authority.substringBefore(':')
        }.lowercase()
        return if (isPrivateHost(host)) trimmed else "wss://$rest"
    }

    private fun isPrivateHost(host: String): Boolean {
        if (host == "localhost" || host == "::1" || host.endsWith(".local")) return true
        if (host.startsWith("127.") || host.startsWith("10.") || host.startsWith("192.168.") ||
            host.startsWith("169.254.")
        ) {
            return true
        }
        if (host.startsWith("172.")) {
            val second = host.split('.').getOrNull(1)?.toIntOrNull()
            if (second != null && second in 16..31) return true
        }
        // IPv6 ULA fc00::/7 与链路本地 fe80::/10
        if (host.startsWith("fc") || host.startsWith("fd") || host.startsWith("fe80")) return true
        return false
    }

    /** relay/token 是否已保存且格式可用（用于判断“已完成首次配置，可自动连接”）。 */
    fun hasCompleteConfig(context: Context): Boolean {
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val relay = normalizeRelayUrl(prefs.getString(KEY_RELAY, "").orEmpty())
        val token = prefs.getString(KEY_TOKEN, "").orEmpty().trim()
        return token.isNotEmpty() &&
            (relay.startsWith("ws://") || relay.startsWith("wss://"))
    }

    /** 确保原生运行时已启动；返回 "ok" 表示成功，否则返回可展示的错误文案。 */
    @Synchronized
    fun ensureRuntime(context: Context): String {
        if (AppState.handle != 0L) {
            return "ok"
        }
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val relay = normalizeRelayUrl(prefs.getString(KEY_RELAY, "").orEmpty())
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
