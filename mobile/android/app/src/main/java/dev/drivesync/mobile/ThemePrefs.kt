package dev.drivesync.mobile

import android.content.Context
import android.content.res.Configuration
import androidx.appcompat.app.AppCompatDelegate

/**
 * 外观主题偏好：跟随系统（默认）/ 深色 / 浅色。
 *
 * - 持久化在 SharedPreferences；
 * - [setMode] 立即生效（AppCompatDelegate.setDefaultNightMode 会自动重建已打开的
 *   AppCompatActivity，资源目录 values/ 与 values-night/ 随之切换）；
 * - 非 Activity 组件（通知、悬浮球、前台服务）用 [isDark] 判断当前应使用的配色。
 */
object ThemePrefs {
    const val MODE_SYSTEM = 0
    const val MODE_DARK = 1
    const val MODE_LIGHT = 2
    const val KEY_THEME_MODE = "theme_mode"

    private fun prefs(context: Context) =
        context.getSharedPreferences(SyncLauncher.PREFS, Context.MODE_PRIVATE)

    fun mode(context: Context): Int = prefs(context).getInt(KEY_THEME_MODE, MODE_SYSTEM)

    fun appCompatMode(context: Context): Int = when (mode(context)) {
        MODE_DARK -> AppCompatDelegate.MODE_NIGHT_YES
        MODE_LIGHT -> AppCompatDelegate.MODE_NIGHT_NO
        else -> AppCompatDelegate.MODE_NIGHT_FOLLOW_SYSTEM
    }

    /** 在 Application.onCreate 里调用，保证首帧即按用户偏好渲染。 */
    fun apply(context: Context) {
        AppCompatDelegate.setDefaultNightMode(appCompatMode(context))
    }

    /** 保存并立即生效。 */
    fun setMode(context: Context, mode: Int) {
        prefs(context).edit().putInt(KEY_THEME_MODE, mode).apply()
        AppCompatDelegate.setDefaultNightMode(appCompatMode(context))
    }

    /** 当前是否应为深色（通知/悬浮球等非 Activity 组件使用）。 */
    fun isDark(context: Context): Boolean = when (mode(context)) {
        MODE_DARK -> true
        MODE_LIGHT -> false
        else -> (context.resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK) ==
            Configuration.UI_MODE_NIGHT_YES
    }
}
