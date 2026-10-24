package dev.drivesync.mobile

import android.content.Intent
import android.os.Bundle
import android.widget.RadioGroup
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import com.google.android.material.button.MaterialButton
import com.google.android.material.switchmaterial.SwitchMaterial

/**
 * 设置页：外观主题（跟随系统 / 深色 / 浅色）+ 启动与同步 + 权限入口 + 关于。
 *
 * 主题切换立即生效：AppCompatDelegate.setDefaultNightMode 会让已打开的
 * AppCompatActivity 自动 recreate，资源目录 values/ 与 values-night/ 随之切换；
 * 通知与悬浮球在下一次刷新时使用对应配色。
 */
class SettingsActivity : AppCompatActivity() {
    private lateinit var rgTheme: RadioGroup
    private lateinit var swAutoStartSync: SwitchMaterial
    private lateinit var swAutoUpdate: SwitchMaterial
    private lateinit var btnCheckUpdate: MaterialButton
    private lateinit var tvUpdateCurrent: TextView
    private lateinit var tvUpdateStatus: TextView
    private var refreshing = false
    private var checkingUpdate = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_settings)
        rgTheme = findViewById(R.id.rgTheme)
        swAutoStartSync = findViewById(R.id.swAutoStartSync)
        swAutoUpdate = findViewById(R.id.swAutoUpdate)
        btnCheckUpdate = findViewById(R.id.btnCheckUpdate)
        tvUpdateCurrent = findViewById(R.id.tvUpdateCurrent)
        tvUpdateStatus = findViewById(R.id.tvUpdateStatus)
        tvUpdateCurrent.text =
            "当前版本 v${UpdateChecker.currentVersion(this)} (build ${UpdateChecker.currentBuild(this)})"
        swAutoUpdate.isChecked = UpdateChecker.isAutoCheckEnabled(this)
        swAutoUpdate.setOnCheckedChangeListener { _, checked ->
            if (refreshing) return@setOnCheckedChangeListener
            UpdateChecker.setAutoCheckEnabled(this, checked)
            Toast.makeText(
                this,
                if (checked) "已开启：每天最多自动检查一次更新" else "已关闭自动检查更新",
                Toast.LENGTH_SHORT,
            ).show()
        }
        btnCheckUpdate.setOnClickListener { checkForUpdate() }
        findViewById<MaterialButton>(R.id.btnBack).setOnClickListener { finish() }
        findViewById<MaterialButton>(R.id.btnOpenPermissions).setOnClickListener {
            startActivity(Intent(this, PermissionsActivity::class.java))
        }
        swAutoStartSync.isChecked = SyncLauncher.isAutoStartEnabled(this)
        swAutoStartSync.setOnCheckedChangeListener { _, checked ->
            if (refreshing) return@setOnCheckedChangeListener
            SyncLauncher.setAutoStartEnabled(this, checked)
            Toast.makeText(
                this,
                if (checked) {
                    "已开启：下次打开 App 会自动开始同步"
                } else {
                    "已关闭：打开 App 后需手动点「连接」"
                },
                Toast.LENGTH_SHORT,
            ).show()
        }
        rgTheme.setOnCheckedChangeListener { _, checkedId ->
            if (refreshing) return@setOnCheckedChangeListener
            val mode = when (checkedId) {
                R.id.rbDark -> ThemePrefs.MODE_DARK
                R.id.rbLight -> ThemePrefs.MODE_LIGHT
                else -> ThemePrefs.MODE_SYSTEM
            }
            if (mode == ThemePrefs.mode(this)) return@setOnCheckedChangeListener
            ThemePrefs.setMode(this, mode)
            Toast.makeText(this, "外观主题已切换", Toast.LENGTH_SHORT).show()
        }
    }

    override fun onResume() {
        super.onResume()
        refreshSelection()
    }

    private fun checkForUpdate() {
        if (checkingUpdate) return
        checkingUpdate = true
        btnCheckUpdate.isEnabled = false
        tvUpdateStatus.text = "正在检查更新…"
        UpdateChecker.check(this) { result ->
            checkingUpdate = false
            btnCheckUpdate.isEnabled = true
            tvUpdateCurrent.text =
            "当前版本 v${UpdateChecker.currentVersion(this)} (build ${UpdateChecker.currentBuild(this)})"
            tvUpdateStatus.text = result.message
            val info = result.info
            if (result.available && info != null) {
                UpdateChecker.showDialog(this, info)
            } else if (result.message.startsWith("检查更新失败")) {
                Toast.makeText(this, result.message, Toast.LENGTH_LONG).show()
            }
        }
    }

    private fun refreshSelection() {
        refreshing = true
        try {
            rgTheme.check(
                when (ThemePrefs.mode(this)) {
                    ThemePrefs.MODE_DARK -> R.id.rbDark
                    ThemePrefs.MODE_LIGHT -> R.id.rbLight
                    else -> R.id.rbSystem
                },
            )
            swAutoStartSync.isChecked = SyncLauncher.isAutoStartEnabled(this)
            swAutoUpdate.isChecked = UpdateChecker.isAutoCheckEnabled(this)
            tvUpdateCurrent.text =
            "当前版本 v${UpdateChecker.currentVersion(this)} (build ${UpdateChecker.currentBuild(this)})"
        } finally {
            refreshing = false
        }
    }
}
