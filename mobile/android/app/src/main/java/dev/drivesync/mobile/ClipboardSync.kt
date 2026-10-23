package dev.drivesync.mobile

import android.content.ClipboardManager
import android.content.Context
import android.os.Handler
import android.os.Looper

/**
 * 剪切板自动同步（轮询 + 哈希去重 + 回环抑制）。
 *
 * - App 在前台时（进程有可见 Activity）：Android 10+ 允许读取剪贴板，自动发送；
 * - App 退到后台：若侧载测试时用 `adb shell appops set dev.drivesync.mobile READ_CLIPBOARD allow`
 *   授权（见 docs/mobile-android.md），前台服务里的轮询仍能读到并自动发送，实现接近桌面端的无感；
 * - 未授权时后台读取返回空，自动降级为“仅前台自动”，不会报错。
 */
object ClipboardSync {
    private const val POLL_INTERVAL_MS = 900L
    private const val MAX_TEXT_BYTES = 512 * 1024

    private val handler = Handler(Looper.getMainLooper())

    @Volatile
    private var watching = false
    private var lastSeenHash = ""

    private val poller = object : Runnable {
        override fun run() {
            if (!watching) return
            try {
                pollOnce()
            } catch (_: Throwable) {
                // 轮询失败不打断主循环
            }
            if (watching) {
                handler.postDelayed(this, POLL_INTERVAL_MS)
            }
        }
    }

    /** 开始轮询（幂等；连接成功后调用）。 */
    fun ensureStarted() {
        if (watching) return
        watching = true
        handler.post(poller)
    }

    /** 停止轮询（断开连接时调用）。 */
    fun stop() {
        watching = false
        handler.removeCallbacks(poller)
    }

    private fun pollOnce() {
        val context: Context = AppState.appContext ?: return
        val handle = AppState.handle
        if (handle == 0L || AppState.paused) return
        val clipboard = context.getSystemService(ClipboardManager::class.java) ?: return
        val clip = try {
            clipboard.primaryClip
        } catch (_: Throwable) {
            null
        } ?: return
        if (clip.itemCount <= 0) return
        val text = try {
            clip.getItemAt(0).coerceToText(context)?.toString()
        } catch (_: Throwable) {
            null
        } ?: return
        if (text.isBlank()) return
        if (text.toByteArray(Charsets.UTF_8).size > MAX_TEXT_BYTES) return

        val hash = AppState.sha256(text)
        if (hash == lastSeenHash) return
        lastSeenHash = hash
        if (AppState.isRemoteEcho(hash)) return

        val result = Native.dmSendClipboardText(handle, text)
        if (result == 0) {
            AppState.markLocalSent(text)
        } else if (result != -3) {
            AppState.lastError = Native.dmLastError() ?: "发送剪切板失败（code=$result）"
        }
    }
}
