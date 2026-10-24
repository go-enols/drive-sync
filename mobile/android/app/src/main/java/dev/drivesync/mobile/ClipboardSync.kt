package dev.drivesync.mobile

import android.content.ClipboardManager
import android.content.Context
import android.os.Handler
import android.os.Looper

/**
 * 剪切板自动同步（轮询 + 哈希去重 + 回环抑制）。
 *
 * Android 10+ 只允许“有前台焦点的应用”读取剪贴板，因此：
 * - App 在前台/有可见界面时：自动轮询并发送（真正无感）；
 * - App 在后台时：系统会返回空剪贴板（普通权限无法绕过）——
 *   发送走“一键”通道：通知栏 Action 或悬浮同步球 → [ClipboardQuickActivity]
 *   取得焦点后读取并发送；接收方向仍然完全自动（写剪贴板 + 通知）。
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

    /** 开始轮询（幂等；连接成功/服务启动后调用）。 */
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
        // 手动一键同步过的内容（后台时）不再重复发送一次。
        if (AppState.isSameAsLastSent(hash)) return

        val result = Native.dmSendClipboardText(handle, text)
        if (result == 0) {
            AppState.markSent(text)
        } else if (result != -3) {
            AppState.lastError = Native.dmLastError() ?: "发送剪切板失败（code=$result）"
        }
    }
}
