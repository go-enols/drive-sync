package dev.drivesync.mobile

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import org.json.JSONObject
import java.security.MessageDigest
import java.util.concurrent.CopyOnWriteArrayList

/**
 * 进程级共享状态：原生句柄、暂停开关、事件监听、远端剪切板应用与去重。
 *
 * 前台 Activity、后台前台服务、透明同步页共用这一份状态（同一进程）。
 * 回调全部切到主线程后再分发给监听者，UI 可直接更新控件。
 */
object AppState {
    /** 本机自动发送后，在这段时间内忽略同内容（避免“远端写入→又发回去”的回环）。 */
    private const val ECHO_WINDOW_MS = 5_000L

    /** 通知 Action / 悬浮球 同一内容去重窗口（防止连点重复发送）。 */
    private const val DEDUPE_WINDOW_MS = 10_000L

    @Volatile
    var appContext: Context? = null
        private set

    /** dm_start 返回的原生句柄（0 = 未启动）。 */
    @Volatile
    var handle: Long = 0L

    /** 暂停剪切板同步（连接与目录同步保持）。 */
    @Volatile
    var paused: Boolean = false

    /** 最近一次剪切板文本预览（本机发送或远端写入）。 */
    @Volatile
    var lastClipboardPreview: String = ""

    /** 最近一次错误（供 UI 展示）。 */
    @Volatile
    var lastError: String = ""

    private val mainHandler = Handler(Looper.getMainLooper())
    private val listeners = CopyOnWriteArrayList<(JSONObject) -> Unit>()
    private var lastRemoteHash: String = ""
    private var lastRemoteAtMs: Long = 0L
    private var lastSentHash: String = ""
    private var lastSentAtMs: Long = 0L

    private val callbackImpl = object : Native.Callback {
        override fun onEvent(json: String) {
            val event = try {
                JSONObject(json)
            } catch (_: Exception) {
                return
            }
            mainHandler.post {
                for (listener in listeners) {
                    try {
                        listener(event)
                    } catch (_: Exception) {
                        // 忽略单个 UI 监听器异常
                    }
                }
            }
        }

        override fun onClipboard(text: String, fromDeviceId: String) {
            val context = appContext ?: return
            mainHandler.post { applyRemoteClipboard(context, text, fromDeviceId) }
        }
    }

    fun init(context: Context) {
        appContext = context.applicationContext
    }

    fun callback(): Native.Callback = callbackImpl

    fun addEventListener(listener: (JSONObject) -> Unit) {
        listeners.addIfAbsent(listener)
    }

    fun removeEventListener(listener: (JSONObject) -> Unit) {
        listeners.remove(listener)
    }

    fun sha256(text: String): String {
        val digest = MessageDigest.getInstance("SHA-256").digest(text.toByteArray(Charsets.UTF_8))
        val builder = StringBuilder(digest.size * 2)
        for (byte in digest) {
            builder.append(String.format("%02x", byte))
        }
        return builder.toString()
    }

    /** 判断是否是刚刚由远端写入剪贴板的内容（回环抑制）。 */
    @Synchronized
    fun isRemoteEcho(hash: String): Boolean {
        return hash == lastRemoteHash &&
            SystemClock.elapsedRealtime() - lastRemoteAtMs < ECHO_WINDOW_MS
    }

    /** 是否与上次发送内容相同（后台一键同步的连点去重）。 */
    @Synchronized
    fun isRecentlySent(hash: String): Boolean {
        return hash == lastSentHash &&
            SystemClock.elapsedRealtime() - lastSentAtMs < DEDUPE_WINDOW_MS
    }

    /** 是否已经发送过该内容（前台轮询用，避免手动同步后再次重复发送）。 */
    @Synchronized
    fun isSameAsLastSent(hash: String): Boolean = hash == lastSentHash

    /** 记录一次成功发送（同时更新预览与去重哈希）。 */
    @Synchronized
    fun markSent(text: String) {
        lastSentHash = sha256(text)
        lastSentAtMs = SystemClock.elapsedRealtime()
        lastClipboardPreview = text
    }

    /** 接收远端剪切板：写系统剪贴板 + 通知 + 更新预览（主线程调用）。 */
    @Synchronized
    private fun applyRemoteClipboard(context: Context, text: String, fromDeviceId: String) {
        val clipboard = context.getSystemService(ClipboardManager::class.java)
        if (clipboard != null) {
            try {
                clipboard.setPrimaryClip(ClipData.newPlainText("drive-sync", text))
            } catch (_: Throwable) {
                // 后台写剪贴板在 Android 10+ 是允许的；个别 ROM 可能限制，忽略即可。
            }
        }
        lastRemoteHash = sha256(text)
        lastRemoteAtMs = SystemClock.elapsedRealtime()
        lastClipboardPreview = text
        NotificationHelper.notifyClipboard(context, fromDeviceId)
        val event = JSONObject()
            .put("type", "remote_clipboard_applied")
            .put("preview", text.take(500))
            .put("from", fromDeviceId)
        for (listener in listeners) {
            try {
                listener(event)
            } catch (_: Exception) {
                // 忽略
            }
        }
    }
}
