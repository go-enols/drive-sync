package dev.drivesync.mobile

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import android.provider.Settings
import android.widget.TextView
import android.widget.Toast
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import com.google.android.material.button.MaterialButton
import com.google.android.material.switchmaterial.SwitchMaterial

/**
 * 权限与后台同步页：所有权限都在 App 内引导申请（不需要 adb / root）。
 *
 * - 通知权限（Android 13+ 运行时弹窗）：远端剪切板提醒 + 一键同步按钮；
 * - 电池优化豁免（可选，系统对话框）：保持后台服务存活；
 * - 悬浮窗权限（可选，系统设置页）：开启“悬浮同步球”；
 * - 开机自启（可选）：Android 14/15 对开机启动 dataSync 前台服务有限制，失败会通知。
 */
class PermissionsActivity : AppCompatActivity() {
    private lateinit var tvNotifStatus: TextView
    private lateinit var btnNotifGrant: MaterialButton
    private lateinit var tvBatteryStatus: TextView
    private lateinit var btnBatteryGrant: MaterialButton
    private lateinit var tvOverlayStatus: TextView
    private lateinit var btnOverlayGrant: MaterialButton
    private lateinit var swFloatingBall: SwitchMaterial
    private lateinit var tvFloatingHint: TextView
    private lateinit var swBootStart: SwitchMaterial
    private lateinit var tvDegraded: TextView

    private var refreshing = false

    private val prefs by lazy {
        getSharedPreferences(SyncLauncher.PREFS, Context.MODE_PRIVATE)
    }

    private val notificationLauncher =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { refresh() }

    private val settingsLauncher =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { refresh() }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_permissions)
        bindViews()
        wireButtons()
        refresh()
    }

    override fun onResume() {
        super.onResume()
        refresh()
    }

    private fun bindViews() {
        tvNotifStatus = findViewById(R.id.tvNotifStatus)
        btnNotifGrant = findViewById(R.id.btnNotifGrant)
        tvBatteryStatus = findViewById(R.id.tvBatteryStatus)
        btnBatteryGrant = findViewById(R.id.btnBatteryGrant)
        tvOverlayStatus = findViewById(R.id.tvOverlayStatus)
        btnOverlayGrant = findViewById(R.id.btnOverlayGrant)
        swFloatingBall = findViewById(R.id.swFloatingBall)
        tvFloatingHint = findViewById(R.id.tvFloatingHint)
        swBootStart = findViewById(R.id.swBootStart)
        tvDegraded = findViewById(R.id.tvDegraded)
        findViewById<MaterialButton>(R.id.btnBack).setOnClickListener { finish() }
    }

    private fun wireButtons() {
        btnNotifGrant.setOnClickListener { requestNotificationPermission() }
        btnBatteryGrant.setOnClickListener { requestIgnoreBatteryOptimizations() }
        btnOverlayGrant.setOnClickListener { requestOverlayPermission() }

        swFloatingBall.setOnCheckedChangeListener { _, checked ->
            if (refreshing) return@setOnCheckedChangeListener
            if (checked) {
                if (!Settings.canDrawOverlays(this)) {
                    refreshing = true
                    swFloatingBall.isChecked = false
                    refreshing = false
                    toast("请先授予悬浮窗权限")
                    requestOverlayPermission()
                } else {
                    FloatingBallManager.setEnabled(this, true)
                    FloatingBallManager.syncFromPrefs(this)
                    toast("悬浮同步球已开启：单击小球即可同步剪贴板")
                }
            } else {
                FloatingBallManager.setEnabled(this, false)
                FloatingBallManager.hide()
                toast("悬浮同步球已关闭")
            }
            refresh()
        }

        swBootStart.setOnCheckedChangeListener { _, checked ->
            if (refreshing) return@setOnCheckedChangeListener
            prefs.edit().putBoolean(SyncLauncher.KEY_BOOT_AUTOSTART, checked).apply()
            toast(
                if (checked) {
                    "已开启：开机后尝试恢复后台同步（Android 15+ 可能被系统限制）"
                } else {
                    "已关闭开机自启"
                },
            )
        }
    }

    private fun refresh() {
        refreshing = true
        try {
            val notificationGranted = isNotificationGranted(this)
            setStatus(tvNotifStatus, notificationGranted)
            btnNotifGrant.isEnabled = !notificationGranted
            btnNotifGrant.text = if (notificationGranted) "已授权" else "去开启"

            val batteryIgnored =
                getSystemService(PowerManager::class.java)
                    ?.isIgnoringBatteryOptimizations(packageName) == true
            setStatus(tvBatteryStatus, batteryIgnored)
            btnBatteryGrant.isEnabled = !batteryIgnored
            btnBatteryGrant.text = if (batteryIgnored) "已开启" else "去开启"

            val overlayGranted = Settings.canDrawOverlays(this)
            setStatus(tvOverlayStatus, overlayGranted)
            btnOverlayGrant.isEnabled = !overlayGranted
            btnOverlayGrant.text = if (overlayGranted) "已授权" else "去开启"

            val ballEnabled = FloatingBallManager.isEnabled(this) && overlayGranted
            swFloatingBall.isChecked = ballEnabled
            tvFloatingHint.text = when {
                !overlayGranted -> "未授权悬浮窗：无法显示悬浮同步球（可用通知栏按钮代替）。"
                ballEnabled -> "已开启：单击小球一键同步；长按小球可关闭。"
                else -> "授权悬浮窗后可开启；后台发送只需点一下小球。"
            }

            swBootStart.isChecked = prefs.getBoolean(SyncLauncher.KEY_BOOT_AUTOSTART, false)

            val degraded = mutableListOf<String>()
            if (!notificationGranted) {
                degraded += "通知未开启：收不到远端剪切板提醒与「同步当前剪贴板」按钮"
            }
            if (!overlayGranted) {
                degraded += "悬浮窗未开启：后台发送只能用通知栏按钮"
            }
            if (!batteryIgnored) {
                degraded += "未豁免电池优化：后台服务可能被系统更快冻结"
            }
            tvDegraded.text = if (degraded.isEmpty()) {
                "全部就绪：前台自动同步；后台接收自动；后台发送点通知/悬浮球一键完成。"
            } else {
                degraded.joinToString("\n") { "• $it" }
            }
        } finally {
            refreshing = false
        }
    }

    private fun setStatus(view: TextView, granted: Boolean) {
        view.text = if (granted) "已授权" else "未授权"
        view.setTextColor(
            ContextCompat.getColor(this, if (granted) R.color.mint else R.color.danger),
        )
    }

    private fun requestNotificationPermission() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            if (isNotificationGranted(this)) {
                refresh()
                return
            }
            val askedBefore = prefs.getBoolean("notif_permission_asked", false)
            val canAskAgain =
                shouldShowRequestPermissionRationale(Manifest.permission.POST_NOTIFICATIONS)
            if (askedBefore && !canAskAgain) {
                // 用户已拒绝且不再弹窗：只能去系统设置开启。
                openNotificationSettings()
                return
            }
            prefs.edit().putBoolean("notif_permission_asked", true).apply()
            notificationLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
        } else {
            openNotificationSettings()
        }
    }

    private fun openNotificationSettings() {
        try {
            settingsLauncher.launch(
                Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
                    .putExtra(Settings.EXTRA_APP_PACKAGE, packageName),
            )
        } catch (_: Exception) {
            openAppDetailsSettings()
        }
    }

    private fun openAppDetailsSettings() {
        try {
            settingsLauncher.launch(
                Intent(
                    Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                    Uri.parse("package:$packageName"),
                ),
            )
        } catch (_: Exception) {
            toast("无法打开系统设置")
        }
    }

    private fun requestIgnoreBatteryOptimizations() {
        val powerManager = getSystemService(PowerManager::class.java)
        if (powerManager?.isIgnoringBatteryOptimizations(packageName) == true) {
            refresh()
            return
        }
        try {
            settingsLauncher.launch(
                Intent(
                    Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                    Uri.parse("package:$packageName"),
                ),
            )
        } catch (_: Exception) {
            try {
                settingsLauncher.launch(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
            } catch (_: Exception) {
                toast("无法打开电池优化设置，请到系统设置手动关闭")
            }
        }
    }

    private fun requestOverlayPermission() {
        try {
            settingsLauncher.launch(
                Intent(
                    Settings.ACTION_MANAGE_OVERLAY_PERMISSION,
                    Uri.parse("package:$packageName"),
                ),
            )
        } catch (_: Exception) {
            openAppDetailsSettings()
        }
    }

    private fun toast(message: String) {
        Toast.makeText(this, message, Toast.LENGTH_LONG).show()
    }

    companion object {
        /** 通知权限状态（Android 13 以下默认允许）。 */
        fun isNotificationGranted(context: Context): Boolean {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
                return true
            }
            return ContextCompat.checkSelfPermission(
                context,
                Manifest.permission.POST_NOTIFICATIONS,
            ) == PackageManager.PERMISSION_GRANTED
        }
    }
}
