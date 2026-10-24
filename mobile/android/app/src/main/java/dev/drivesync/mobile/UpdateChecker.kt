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
 * 更新检查：读取 GitHub Releases 最新版本，比较当前 App 版本。
 *
 * 说明：
 * - Android 侧载应用无法静默安装（需要系统权限/应用商店），因此这里只做“检查 +
 *   打开 APK/Release 下载页”，安装交给系统包安装器；
 * - 自动检查每天最多一次（打开 App 时触发），发现新版本弹窗提示；
 * - 网络/API 失败只提示，不影响同步功能。
 */
object UpdateChecker {
    private const val API_URL = "https://api.github.com/repos/go-enols/drive-sync/releases/latest"
    private const val PREFS = "drive_sync_update"
    private const val KEY_LAST_CHECK = "last_check_ms"
    private const val KEY_AUTO_CHECK = "auto_check"
    private const val CHECK_INTERVAL_MS = 24L * 60 * 60 * 1000

    private val main = Handler(Looper.getMainLooper())

    data class Info(
        val latest: String,
        val current: String,
        val notes: String,
        val downloadUrl: String?,
        val releaseUrl: String,
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
        val connection = (URL(API_URL).openConnection() as HttpURLConnection).apply {
            connectTimeout = 10_000
            readTimeout = 15_000
            instanceFollowRedirects = true
            setRequestProperty("Accept", "application/vnd.github+json")
            setRequestProperty("User-Agent", "drive-sync-android")
        }
        try {
            val code = connection.responseCode
            if (code !in 200..299) {
                val detail = connection.errorStream
                    ?.bufferedReader(Charsets.UTF_8)
                    ?.use { it.readText() }
                    ?.take(200)
                    .orEmpty()
                throw IllegalStateException("GitHub API HTTP $code${if (detail.isBlank()) "" else "：$detail"}")
            }
            val body = connection.inputStream.bufferedReader(Charsets.UTF_8).use { it.readText() }
            val json = JSONObject(body)
            val latest = json.optString("tag_name").trim().removePrefix("v")
            val current = currentVersion(context)
            val releaseUrl = json.optString("html_url")
            val notes = json.optString("body").trim()
            var apkUrl: String? = null
            val assets = json.optJSONArray("assets")
            if (assets != null) {
                for (index in 0 until assets.length()) {
                    val asset = assets.optJSONObject(index) ?: continue
                    val name = asset.optString("name")
                    if (name.endsWith(".apk", ignoreCase = true)) {
                        apkUrl = asset.optString("browser_download_url")
                        break
                    }
                }
            }
            val available = latest.isNotEmpty() && compareVersions(latest, current) > 0
            val message = if (available) {
                "发现新版本 v$latest（当前 v$current）"
            } else {
                "已是最新版本 v$current"
            }
            return Result(
                available = available,
                info = Info(
                    latest = latest.ifEmpty { current },
                    current = current,
                    notes = notes,
                    downloadUrl = apkUrl,
                    releaseUrl = releaseUrl,
                ),
                message = message,
            )
        } finally {
            connection.disconnect()
        }
    }

    /** 打开 APK 直链；没有 APK 产物时退回 Release 页面。 */
    fun openDownload(context: Context, info: Info) {
        val url = info.downloadUrl?.takeIf { it.isNotBlank() }
            ?: info.releaseUrl.takeIf { it.isNotBlank() }
        if (url.isNullOrBlank()) {
            Toast.makeText(context, "没有可用的下载地址", Toast.LENGTH_SHORT).show()
            return
        }
        try {
            context.startActivity(
                Intent(Intent.ACTION_VIEW, Uri.parse(url))
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        } catch (error: Throwable) {
            Toast.makeText(context, "打开下载页失败：${error.message}", Toast.LENGTH_SHORT).show()
        }
    }

    fun showDialog(activity: Activity, info: Info) {
        val message = buildString {
            append("当前版本 v").append(info.current)
            append(" → 最新版本 v").append(info.latest)
            if (info.notes.isNotBlank()) {
                append("\n\n").append(info.notes.take(1200))
            }
            if (info.downloadUrl == null) {
                append("\n\n（Release 中没有找到 APK 产物，将打开 Release 页面）")
            }
        }
        MaterialAlertDialogBuilder(activity)
            .setTitle("发现新版本")
            .setMessage(message.trim())
            .setPositiveButton("去下载") { _, _ -> openDownload(activity, info) }
            .setNegativeButton("稍后", null)
            .show()
    }

    private fun compareVersions(left: String, right: String): Int {
        val a = versionParts(left)
        val b = versionParts(right)
        val count = maxOf(a.size, b.size)
        for (index in 0 until count) {
            val x = a.getOrElse(index) { 0 }
            val y = b.getOrElse(index) { 0 }
            if (x != y) return x.compareTo(y)
        }
        return 0
    }

    private fun versionParts(version: String): List<Int> {
        val cleaned = version.trim().removePrefix("v")
        if (cleaned.isEmpty()) return emptyList()
        return cleaned
            .split('.', '-', '+', '_')
            .mapNotNull { part -> part.takeWhile { it.isDigit() }.toIntOrNull() }
    }
}
