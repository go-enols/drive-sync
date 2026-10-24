package dev.drivesync.mobile

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.widget.Toast
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL

/**
 * 更新检查：读取自建服务器上的 Android 更新清单（不再依赖 GitHub Release）。
 *
 * 清单示例（`https://sync.likecloud.top/android/latest.json`）：
 * {
 *   "versionCode": 7,
 *   "versionName": "0.3.2",
 *   "url": "https://sync.likecloud.top/android/DriveSync-...apk",
 *   "size": 25590895,
 *   "sha256": "...",
 *   "notes": "更新说明"
 * }
 *
 * 说明：Android 侧载应用不能静默安装；这里只做“检查 + 打开 APK 下载”，安装由系统完成。
 * 自动检查每天最多一次（打开 App 时触发）。
 */
object UpdateChecker {
    private const val MANIFEST_URL = "https://sync.likecloud.top/android/latest.json"
    private const val PREFS = "drive_sync_update"
    private const val KEY_LAST_CHECK = "last_check_ms"
    private const val KEY_AUTO_CHECK = "auto_check"
    private const val CHECK_INTERVAL_MS = 24L * 60 * 60 * 1000

    private val main = Handler(Looper.getMainLooper())

    data class Info(
        val latestVersion: String,
        val latestBuild: Long,
        val currentVersion: String,
        val currentBuild: Long,
        val notes: String,
        val downloadUrl: String,
        val size: Long,
        val sha256: String,
    )

    data class Result(
        val available: Boolean,
        val info: Info?,
        val message: String,
    )

    fun currentVersion(context: Context): String {
        return try {
            context.packageManager
                .getPackageInfo(context.packageName, 0)
                .versionName
                ?.trim()
                .orEmpty()
                .ifEmpty { "0.0.0" }
        } catch (_: Throwable) {
            "0.0.0"
        }
    }

    fun currentBuild(context: Context): Long {
        return try {
            context.packageManager
                .getPackageInfo(context.packageName, 0)
                .longVersionCode
        } catch (_: Throwable) {
            0L
        }
    }

    fun isAutoCheckEnabled(context: Context): Boolean =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getBoolean(KEY_AUTO_CHECK, true)

    fun setAutoCheckEnabled(context: Context, enabled: Boolean) {
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putBoolean(KEY_AUTO_CHECK, enabled)
            .apply()
    }

    /** 手动检查；回调切回主线程。 */
    fun check(context: Context, callback: (Result) -> Unit) {
        Thread {
            val result = try {
                fetch(context)
            } catch (error: Throwable) {
                Result(
                    available = false,
                    info = null,
                    message = "检查更新失败：${error.message ?: error.javaClass.simpleName}",
                )
            }
            main.post { callback(result) }
        }.start()
    }

    /** 打开 App 时自动检查：每天最多一次，有更新时回调（主线程）。 */
    fun maybeAutoCheck(context: Context, onAvailable: (Info) -> Unit) {
        if (!isAutoCheckEnabled(context)) return
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val now = System.currentTimeMillis()
        if (now - prefs.getLong(KEY_LAST_CHECK, 0L) < CHECK_INTERVAL_MS) return
        prefs.edit().putLong(KEY_LAST_CHECK, now).apply()
        check(context) { result ->
            val info = result.info
            if (result.available && info != null) {
                onAvailable(info)
            }
        }
    }

    private fun fetch(context: Context): Result {
        val connection = (URL(MANIFEST_URL).openConnection() as HttpURLConnection).apply {
            connectTimeout = 10_000
            readTimeout = 15_000
            instanceFollowRedirects = true
            setRequestProperty("Accept", "application/json")
            setRequestProperty("User-Agent", "drive-sync-android")
        }
        try {
            val code = connection.responseCode
            if (code !in 200..299) {
                throw IllegalStateException("更新清单 HTTP $code")
            }
            val body = connection.inputStream.bufferedReader(Charsets.UTF_8).use { it.readText() }
            val json = JSONObject(body)
            val latestBuild = json.optLong("versionCode", 0L)
            val latestVersion = json.optString("versionName").trim().ifEmpty { "unknown" }
            val downloadUrl = json.optString("url").trim()
            val size = json.optLong("size", 0L)
            val sha256 = json.optString("sha256").trim()
            val notes = json.optString("notes").trim()
            if (downloadUrl.isBlank()) {
                throw IllegalStateException("更新清单没有 APK 下载地址")
            }
            val currentVersion = currentVersion(context)
            val currentBuild = currentBuild(context)
            val available = latestBuild > currentBuild
            return Result(
                available = available,
                info = Info(
                    latestVersion = latestVersion,
                    latestBuild = latestBuild,
                    currentVersion = currentVersion,
                    currentBuild = currentBuild,
                    notes = notes,
                    downloadUrl = downloadUrl,
                    size = size,
                    sha256 = sha256,
                ),
                message = if (available) {
                    "发现新版本 v$latestVersion (build $latestBuild，当前 build $currentBuild)"
                } else {
                    "已是最新版本 v$currentVersion (build $currentBuild)"
                },
            )
        } finally {
            connection.disconnect()
        }
    }

    /** 打开 APK 下载地址（浏览器/系统下载器负责下载与安装）。 */
    fun openDownload(context: Context, info: Info) {
        if (info.downloadUrl.isBlank()) {
            Toast.makeText(context, "没有可用的下载地址", Toast.LENGTH_SHORT).show()
            return
        }
        try {
            context.startActivity(
                Intent(Intent.ACTION_VIEW, Uri.parse(info.downloadUrl))
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        } catch (error: Throwable) {
            Toast.makeText(context, "打开下载页失败：${error.message}", Toast.LENGTH_SHORT).show()
        }
    }

    fun showDialog(activity: Activity, info: Info) {
        val message = buildString {
            append("当前 v").append(info.currentVersion)
            append("（build ").append(info.currentBuild).append("）")
            append(" → 最新 v").append(info.latestVersion)
            append("（build ").append(info.latestBuild).append("）")
            if (info.size > 0) {
                append("\n大小：").append(String.format("%.1f MiB", info.size / 1024.0 / 1024.0))
            }
            if (info.notes.isNotBlank()) {
                append("\n\n").append(info.notes.take(1200))
            }
        }
        MaterialAlertDialogBuilder(activity)
            .setTitle("发现新版本")
            .setMessage(message.trim())
            .setPositiveButton("去下载") { _, _ -> openDownload(activity, info) }
            .setNegativeButton("稍后", null)
            .show()
    }
}
