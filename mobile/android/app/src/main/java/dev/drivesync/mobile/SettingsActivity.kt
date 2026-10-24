package dev.drivesync.mobile

import android.content.Intent
import android.os.Bundle
import android.widget.RadioGroup
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import com.google.android.material.button.MaterialButton

/**
 * 设置页：外观主题（跟随系统 / 深色 / 浅色）+ 权限入口 + 关于。
 *
 * 主题切换立即生效：AppCompatDelegate.setDefaultNightMode 会让已打开的
 * AppCompatActivity 自动 recreate，资源目录 values/ 与 values-night/ 随之切换；
 * 通知与悬浮球在下一次刷新时使用对应配色。
 */
class SettingsActivity : AppCompatActivity() {
    private lateinit var rgTheme: RadioGroup
    private var refreshing = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_settings)
        rgTheme = findViewById(R.id.rgTheme)
        findViewById<MaterialButton>(R.id.btnBack).setOnClickListener { finish() }
        findViewById<MaterialButton>(R.id.btnOpenPermissions).setOnClickListener {
            startActivity(Intent(this, PermissionsActivity::class.java))
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
        } finally {
            refreshing = false
        }
    }
}
