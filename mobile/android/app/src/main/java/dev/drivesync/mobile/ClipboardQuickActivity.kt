package dev.drivesync.mobile

import android.app.Activity
import android.content.ClipboardManager
import android.os.Bundle
import android.widget.Toast

/**
 * 透明“一键同步”页：由通知 Action 或悬浮同步球拉起。
 *
 * Android 10+ 禁止后台应用读剪贴板（普通权限无解）；本 Activity 取得前台焦点后
 * 读取剪贴板 → `dm_send_clipboard_text` → 自动关闭，用户只需点一次。
 */
class ClipboardQuickActivity : Activity() {
    private var handled = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_quick_sync)
        AppState.init(this)
        if (AppState.handle == 0L) {
            val message = SyncLauncher.ensureRuntime(this)
            if (message != "ok") {
                toast("同步未启动：$message")
                finishAfter(400)
                return
            }
        }
        // 焦点获取后读取；700ms 兜底（部分 ROM 焦点回调较慢）。
        window.decorView.postDelayed({ trySync() }, 700)
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) {
            window.decorView.postDelayed({ trySync() }, 150)
        }
    }

    private fun trySync() {
        if (handled || isFinishing) return
        handled = true
        if (AppState.paused) {
            toast("剪切板同步已暂停，请打开 App 恢复")
            finishAfter(400)
            return
        }
        val handle = AppState.handle
        if (handle == 0L) {
            toast("未连接中继，请打开 App 检查配置")
            finishAfter(400)
            return
        }
        val text = readClipboardText()
        if (text.isNullOrBlank()) {
            toast("剪贴板里没有文本")
            finishAfter(400)
            return
        }
        val hash = AppState.sha256(text)
        if (AppState.isRecentlySent(hash)) {
            toast("内容与上次同步相同，已跳过")
            finishAfter(400)
            return
        }
        val result = Native.dmSendClipboardText(handle, text)
        if (result == 0) {
            AppState.markSent(text)
            toast("已同步当前剪贴板")
        } else {
            toast("发送失败（code=$result）")
        }
        finishAfter(450)
    }

    private fun readClipboardText(): String? {
        val clipboard = getSystemService(ClipboardManager::class.java) ?: return null
        val clip = try {
            clipboard.primaryClip
        } catch (_: Throwable) {
            null
        } ?: return null
        if (clip.itemCount <= 0) return null
        return try {
            clip.getItemAt(0).coerceToText(this)?.toString()
        } catch (_: Throwable) {
            null
        }
    }

    private fun finishAfter(delayMs: Long) {
        window.decorView.postDelayed({
            if (!isFinishing) {
                finish()
            }
        }, delayMs)
    }

    private fun toast(message: String) {
        Toast.makeText(this, message, Toast.LENGTH_SHORT).show()
    }
}
