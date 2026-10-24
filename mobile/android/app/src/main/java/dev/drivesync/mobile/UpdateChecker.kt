package dev.drivesync.mobile

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import android.widget.Toast
import androidx.core.content.FileProvider
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import org.json.JSONObject
import java.io.File
import java.io.FileOutputStream
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest

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

    /**
     * 应用内下载 APK → 校验大小/SHA256 → 交给系统安装器。
     * 回调都在主线程；进度回调用于界面进度条。
     */
    fun downloadAndInstall(
        context: Context,
        info: Info,
        onProgress: (received: Long, total: Long) -> Unit,
        onResult: (ok: Boolean, message: String) -> Unit,
    ) {
        val appContext = context.applicationContext
        Thread {
            var failure: String? = null
            var apkFile: File? = null
            try {
                val dir = File(appContext.cacheDir, "updates")
                dir.mkdirs()
                val target = File(dir, "drive-sync-${info.latestBuild}.apk")
                val part = File(dir, "drive-sync-${info.latestBuild}.apk.part")
                part.delete()
                val connection = (URL(info.downloadUrl).openConnection() as HttpURLConnection).apply {
                    connectTimeout = 15_000
                    readTimeout = 60_000
                    instanceFollowRedirects = true
                    setRequestProperty("User-Agent", "drive-sync-android")
                }
                try {
                    val code = connection.responseCode
                    if (code !in 200..299) {
                        throw IllegalStateException("下载 APK HTTP $code")
                    }
                    val total = if (connection.contentLengthLong > 0) {
                        connection.contentLengthLong
                    } else {
                        info.size
                    }
                    val digest = MessageDigest.getInstance("SHA-256")
                    var received = 0L
                    var lastReported = 0L
                    connection.inputStream.use { input ->
                        FileOutputStream(part).use { output ->
                            val buffer = ByteArray(64 * 1024)
                            while (true) {
                                val read = input.read(buffer)
                                if (read < 0) break
                                output.write(buffer, 0, read)
                                digest.update(buffer, 0, read)
                                received += read
                                if (received - lastReported >= 256 * 1024) {
                                    lastReported = received
                                    val shownReceived = received
                                    main.post { onProgress(shownReceived, total) }
                                }
                            }
                        }
                    }
                    if (info.size > 0 && part.length() != info.size) {
                        throw IllegalStateException("APK 大小不匹配（${part.length()} != ${info.size}）")
                    }
                    val actual = digest.digest().joinToString("") { byte ->
                        (byte.toInt() and 0xff).toString(16).padStart(2, '0')
                    }
                    if (info.sha256.isNotBlank() &&
                        !actual.equals(info.sha256.trim(), ignoreCase = true)
                    ) {
                        throw IllegalStateException("APK SHA256 校验失败")
                    }
                    target.delete()
                    if (!part.renameTo(target)) {
                        throw IllegalStateException("保存 APK 失败")
                    }
                    apkFile = target
                } finally {
                    connection.disconnect()
                }
            } catch (error: Throwable) {
                failure = error.message ?: error.javaClass.simpleName
            }
            main.post {
                val file = apkFile
                if (failure != null || file == null) {
                    onResult(false, "下载失败：${failure ?: "未知错误"}")
                    return@post
                }
                try {
                    if (!appContext.packageManager.canRequestPackageInstalls()) {
                        val intent = Intent(
                            Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
                            Uri.parse("package:${appContext.packageName}"),
                        ).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                        appContext.startActivity(intent)
                        onResult(false, "请先允许 drive-sync 安装未知应用，然后重新点更新")
                        return@post
                    }
                    val uri = FileProvider.getUriForFile(
                        appContext,
                        "${appContext.packageName}.fileprovider",
                        file,
                    )
                    val intent = Intent(Intent.ACTION_VIEW)
                        .setDataAndType(uri, "application/vnd.android.package-archive")
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_GRANT_READ_URI_PERMISSION)
                    appContext.startActivity(intent)
                    onResult(true, "已打开系统安装器，按提示完成更新")
                } catch (error: Throwable) {
                    onResult(false, "拉起安装器失败：${error.message}")
                }
            }
        }.start()
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

    fun showDialog(
        activity: Activity,
        info: Info,
        onInstall: ((Info) -> Unit)? = null,
    ) {
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
            .setPositiveButton("去更新") { _, _ ->
                if (onInstall != null) onInstall(info) else openDownload(activity, info)
            }
            .setNegativeButton("稍后", null)
            .show()
    }
}
