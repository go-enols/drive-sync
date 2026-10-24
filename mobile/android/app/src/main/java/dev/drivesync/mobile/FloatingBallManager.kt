package dev.drivesync.mobile

import android.content.Context
import android.content.Intent
import android.graphics.PixelFormat
import android.graphics.drawable.GradientDrawable
import android.os.SystemClock
import android.provider.Settings
import android.view.Gravity
import android.view.MotionEvent
import android.view.ViewConfiguration
import android.view.WindowManager
import android.widget.TextView
import android.widget.Toast
import kotlin.math.abs

/**
 * 可选“悬浮同步球”：用户授权悬浮窗后显示一个薄荷绿小球。
 *
 * - 单击：打开 [ClipboardQuickActivity]（透明页面取得焦点 → 读剪贴板 → 发送 → 自动关闭）；
 * - 拖动：移动位置；
 * - 长按：关闭悬浮球（也可在“权限与后台同步”页开关）。
 *
 * 悬浮球让“后台发送”只需一次点击，替代 adb/appops 方案。
 */
object FloatingBallManager {
    private var ball: TextView? = null
    private var params: WindowManager.LayoutParams? = null
    private var windowManager: WindowManager? = null

    fun isEnabled(context: Context): Boolean {
        return context.getSharedPreferences(SyncLauncher.PREFS, Context.MODE_PRIVATE)
            .getBoolean(SyncLauncher.KEY_FLOATING_BALL, false)
    }

    fun setEnabled(context: Context, enabled: Boolean) {
        context.getSharedPreferences(SyncLauncher.PREFS, Context.MODE_PRIVATE)
            .edit()
            .putBoolean(SyncLauncher.KEY_FLOATING_BALL, enabled)
            .apply()
    }

    /** 根据偏好与系统权限同步显示状态（幂等）。 */
    fun syncFromPrefs(context: Context) {
        if (isEnabled(context) && Settings.canDrawOverlays(context)) {
            show(context)
        } else {
            hide()
        }
    }

    fun show(context: Context) {
        if (ball != null) return
        val app = context.applicationContext
        if (!Settings.canDrawOverlays(app)) return
        val manager = app.getSystemService(WindowManager::class.java) ?: return
        val density = app.resources.displayMetrics.density
        val size = (56 * density).toInt()

        val view = TextView(app).apply {
            text = "⇄"
            textSize = 26f
            gravity = Gravity.CENTER
            setTextColor(0xFF06121A.toInt())
            contentDescription = "drive-sync 同步当前剪贴板"
            elevation = 10f * density
            background = GradientDrawable().apply {
                shape = GradientDrawable.OVAL
                setColor(0xFF22E3B0.toInt())
                setStroke((1 * density).toInt(), 0xFF0E5A47.toInt())
            }
        }
        val layoutParams = WindowManager.LayoutParams(
            size,
            size,
            WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY,
            WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE,
            PixelFormat.TRANSLUCENT,
        ).apply {
            gravity = Gravity.TOP or Gravity.START
            x = app.resources.displayMetrics.widthPixels - size - (16 * density).toInt()
            y = (app.resources.displayMetrics.heightPixels / 4)
        }

        val touchSlop = ViewConfiguration.get(app).scaledTouchSlop
        var downRawX = 0f
        var downRawY = 0f
        var startX = 0
        var startY = 0
        var dragging = false
        var downAtMs = 0L
        view.setOnTouchListener { _, event ->
            when (event.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    downRawX = event.rawX
                    downRawY = event.rawY
                    startX = layoutParams.x
                    startY = layoutParams.y
                    dragging = false
                    downAtMs = SystemClock.uptimeMillis()
                    true
                }
                MotionEvent.ACTION_MOVE -> {
                    val deltaX = (event.rawX - downRawX).toInt()
                    val deltaY = (event.rawY - downRawY).toInt()
                    if (abs(deltaX) > touchSlop || abs(deltaY) > touchSlop) {
                        dragging = true
                    }
                    if (dragging) {
                        layoutParams.x = startX + deltaX
                        layoutParams.y = startY + deltaY
                        try {
                            manager.updateViewLayout(view, layoutParams)
                        } catch (_: Exception) {
                            // 忽略
                        }
                    }
                    true
                }
                MotionEvent.ACTION_UP -> {
                    val heldMs = SystemClock.uptimeMillis() - downAtMs
                    if (dragging) {
                        // 拖动结束，不触发同步
                    } else if (heldMs > 600L) {
                        // 长按：关闭悬浮球（撤销入口）
                        setEnabled(app, false)
                        hide()
                        Toast.makeText(app, "悬浮同步球已关闭", Toast.LENGTH_SHORT).show()
                    } else {
                        launchQuickSync(app)
                    }
                    true
                }
                MotionEvent.ACTION_CANCEL -> {
                    dragging = false
                    true
                }
                else -> false
            }
        }

        try {
            manager.addView(view, layoutParams)
            ball = view
            params = layoutParams
            windowManager = manager
        } catch (_: Exception) {
            ball = null
            params = null
            windowManager = null
        }
    }

    fun hide() {
        val view = ball
        val manager = windowManager
        ball = null
        params = null
        windowManager = null
        if (view != null && manager != null) {
            try {
                manager.removeView(view)
            } catch (_: Exception) {
                // 已经被系统移除
            }
        }
    }

    private fun launchQuickSync(context: Context) {
        try {
            context.startActivity(
                Intent(context, ClipboardQuickActivity::class.java)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        } catch (_: Exception) {
            Toast.makeText(context, "无法打开同步页面，请从通知栏操作", Toast.LENGTH_SHORT).show()
        }
    }
}
