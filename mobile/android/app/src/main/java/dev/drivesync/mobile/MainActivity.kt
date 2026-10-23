package dev.drivesync.mobile

import android.Manifest
import android.content.ClipboardManager
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import androidx.lifecycle.lifecycleScope
import com.google.android.material.button.MaterialButton
import com.google.android.material.switchmaterial.SwitchMaterial
import com.google.android.material.textfield.TextInputEditText
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

/**
 * 单 Activity 控制台：配置中继 → 连接 → 配对 → 剪切板自动同步 → 软件目录只读展示。
 *
 * 剪切板同步是“无感自动”的：前台由 [ClipboardSync] 轮询自动发送，收到远端内容自动写回系统
 * 剪贴板并通知；“立即发送当前剪切板”仅作为手动兜底。后台无感依赖 [SyncService] 前台服务
 * + 可选 READ_CLIPBOARD appops 授权（详见 docs/mobile-android.md）。
 */
class MainActivity : AppCompatActivity() {
    private lateinit var etRelay: TextInputEditText
    private lateinit var etToken: TextInputEditText
    private lateinit var etDeviceName: TextInputEditText
    private lateinit var etJoinCode: TextInputEditText
    private lateinit var tvConnStatus: TextView
    private lateinit var tvSyncStatus: TextView
    private lateinit var tvPeers: TextView
    private lateinit var tvPairCode: TextView
    private lateinit var tvPairStatus: TextView
    private lateinit var tvClipboard: TextView
    private lateinit var tvCatalog: TextView
    private lateinit var btnSaveConfig: MaterialButton
    private lateinit var btnConnect: MaterialButton
    private lateinit var btnDisconnect: MaterialButton
    private lateinit var btnPairStart: MaterialButton
    private lateinit var btnPairJoin: MaterialButton
    private lateinit var btnSendClipboard: MaterialButton
    private lateinit var btnRefresh: MaterialButton
    private lateinit var swPause: SwitchMaterial

    private val prefs by lazy { getSharedPreferences("drive_sync", Context.MODE_PRIVATE) }

    private val dataDir: String
        get() = File(filesDir, "drive-sync").absolutePath

    private val eventListener: (JSONObject) -> Unit = { event -> onNativeEvent(event) }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        AppState.init(this)
        bindViews()
        loadPrefs()
        AppState.addEventListener(eventListener)
        wireButtons()
        updateConnectedUi()
        requestNotificationPermissionIfNeeded()
        if (AppState.handle != 0L) {
            updateConnectedUi()
            refreshAll()
        }
    }

    override fun onDestroy() {
        AppState.removeEventListener(eventListener)
        super.onDestroy()
    }

    private fun bindViews() {
        etRelay = findViewById(R.id.etRelay)
        etToken = findViewById(R.id.etToken)
        etDeviceName = findViewById(R.id.etDeviceName)
        etJoinCode = findViewById(R.id.etJoinCode)
        tvConnStatus = findViewById(R.id.tvConnStatus)
        tvSyncStatus = findViewById(R.id.tvSyncStatus)
        tvPeers = findViewById(R.id.tvPeers)
        tvPairCode = findViewById(R.id.tvPairCode)
        tvPairStatus = findViewById(R.id.tvPairStatus)
        tvClipboard = findViewById(R.id.tvClipboard)
        tvCatalog = findViewById(R.id.tvCatalog)
        btnSaveConfig = findViewById(R.id.btnSaveConfig)
        btnConnect = findViewById(R.id.btnConnect)
        btnDisconnect = findViewById(R.id.btnDisconnect)
        btnPairStart = findViewById(R.id.btnPairStart)
        btnPairJoin = findViewById(R.id.btnPairJoin)
        btnSendClipboard = findViewById(R.id.btnSendClipboard)
        btnRefresh = findViewById(R.id.btnRefresh)
        swPause = findViewById(R.id.swPause)
    }

    private fun wireButtons() {
        btnSaveConfig.setOnClickListener { saveConfigOnly() }
        btnConnect.setOnClickListener { connect() }
        btnDisconnect.setOnClickListener { disconnect() }
        btnPairStart.setOnClickListener { pairStart() }
        btnPairJoin.setOnClickListener { pairJoin() }
        btnSendClipboard.setOnClickListener { sendCurrentClipboard() }
        btnRefresh.setOnClickListener { refreshAll() }
        swPause.setOnCheckedChangeListener { _, checked ->
            AppState.paused = checked
            val handle = AppState.handle
            if (handle != 0L) {
                Thread { Native.dmSetPaused(handle, checked) }.start()
            }
            tvSyncStatus.text = if (checked) "剪切板同步已暂停（连接保持）" else "剪切板同步已恢复"
        }
    }

    private fun loadPrefs() {
        etRelay.setText(prefs.getString("relay", ""))
        etToken.setText(prefs.getString("token", ""))
        etDeviceName.setText(prefs.getString("device_name", "Android 手机"))
    }

    private fun savePrefs(relay: String, token: String, deviceName: String) {
        prefs.edit()
            .putString("relay", relay)
            .putString("token", token)
            .putString("device_name", deviceName)
            .apply()
    }

    /** 读取并校验输入；返回 null 表示校验失败（已 toast）。 */
    private fun readConfig(): Triple<String, String, String>? {
        val relay = etRelay.text?.toString()?.trim().orEmpty()
        val token = etToken.text?.toString()?.trim().orEmpty()
        val deviceName = etDeviceName.text?.toString()?.trim().orEmpty()
        if (relay.isEmpty() || token.isEmpty() || deviceName.isEmpty()) {
            toast("请填写中继地址、访问令牌与设备名")
            return null
        }
        if (!relay.startsWith("ws://") && !relay.startsWith("wss://")) {
            toast("中继地址需以 ws:// 或 wss:// 开头")
            return null
        }
        savePrefs(relay, token, deviceName)
        return Triple(relay, token, deviceName)
    }

    // ------------------------------------------------------------------
    // 配置 / 连接
    // ------------------------------------------------------------------

    private fun saveConfigOnly() {
        val config = readConfig() ?: return
        lifecycleScope.launch {
            val message = withContext(Dispatchers.IO) {
                val init = parseJson(Native.dmInit(dataDir, config.third))
                if (!init.optBoolean("ok")) {
                    return@withContext "初始化失败：${init.optString("error")}"
                }
                val relayJson = JSONObject()
                    .put("relay_url", config.first)
                    .put("access_token", config.second)
                    .put("device_name", config.third)
                val saved = parseJson(Native.dmConfigSetRelay(relayJson.toString()))
                if (!saved.optBoolean("ok")) {
                    return@withContext "保存配置失败：${saved.optString("error")}"
                }
                "配置已保存 · 设备 ${init.optString("name")} (${init.optString("device_id")})"
            }
            tvConnStatus.text = message
            toast(message)
        }
    }

    private fun connect() {
        if (AppState.handle != 0L) {
            toast("已连接")
            return
        }
        val config = readConfig() ?: return
        btnConnect.isEnabled = false
        lifecycleScope.launch {
            val message = withContext(Dispatchers.IO) {
                val init = parseJson(Native.dmInit(dataDir, config.third))
                if (!init.optBoolean("ok")) {
                    return@withContext "初始化失败：${init.optString("error")}"
                }
                val relayJson = JSONObject()
                    .put("relay_url", config.first)
                    .put("access_token", config.second)
                    .put("device_name", config.third)
                val saved = parseJson(Native.dmConfigSetRelay(relayJson.toString()))
                if (!saved.optBoolean("ok")) {
                    return@withContext "保存配置失败：${saved.optString("error")}"
                }
                val handle = Native.dmStart(config.first, config.second, AppState.callback())
                if (handle == 0L) {
                    return@withContext "启动失败：${Native.dmLastError() ?: "未知错误"}"
                }
                AppState.handle = handle
                "连接已启动 · 设备 ${init.optString("name")} (${init.optString("device_id")})"
            }
            btnConnect.isEnabled = true
            if (AppState.handle != 0L) {
                updateConnectedUi()
                SyncService.start(this@MainActivity)
                ClipboardSync.ensureStarted()
                toast("已启动连接，正在连接中继…")
                refreshAll()
            } else {
                tvConnStatus.text = message
                toast(message)
            }
        }
    }

    private fun disconnect() {
        val handle = AppState.handle
        AppState.handle = 0L
        AppState.paused = false
        swPause.isChecked = false
        ClipboardSync.stop()
        SyncService.stop(this)
        if (handle != 0L) {
            Thread { Native.dmStop(handle) }.start()
        }
        updateDisconnectedUi()
        toast("已断开")
    }

    // ------------------------------------------------------------------
    // 配对
    // ------------------------------------------------------------------

    private fun pairStart() {
        val handle = AppState.handle
        if (handle == 0L) {
            toast("请先连接")
            return
        }
        btnPairStart.isEnabled = false
        tvPairStatus.text = "正在生成配对码（会临时暂停同步，完成后自动恢复）…"
        lifecycleScope.launch {
            val result = withContext(Dispatchers.IO) { parseJson(Native.dmPairStart(handle)) }
            btnPairStart.isEnabled = true
            if (result.optBoolean("ok")) {
                tvPairCode.text = formatPairCode(result.optString("code"))
                tvPairStatus.text = "请在对端输入该配对码；等待对端加入…"
            } else {
                tvPairStatus.text = "发起配对失败：${result.optString("error")}"
            }
        }
    }

    private fun pairJoin() {
        val handle = AppState.handle
        if (handle == 0L) {
            toast("请先连接")
            return
        }
        val input = etJoinCode.text?.toString()?.trim().orEmpty()
        if (input.isEmpty()) {
            toast("请输入对端 8 位配对码或扫码串")
            return
        }
        btnPairJoin.isEnabled = false
        tvPairStatus.text = "正在加入配对（最长约 130 秒）…"
        lifecycleScope.launch {
            val result = withContext(Dispatchers.IO) {
                parseJson(Native.dmPairJoin(handle, input))
            }
            btnPairJoin.isEnabled = true
            if (result.optBoolean("ok")) {
                tvPairStatus.text =
                    "配对成功：${result.optString("name")} (${result.optString("peer_id")})，SAS=${result.optString("sas")}"
                etJoinCode.setText("")
                refreshAll()
            } else {
                tvPairStatus.text = "加入配对失败：${result.optString("error")}"
            }
        }
    }

    // ------------------------------------------------------------------
    // 剪切板
    // ------------------------------------------------------------------

    private fun sendCurrentClipboard() {
        val handle = AppState.handle
        if (handle == 0L) {
            toast("请先连接")
            return
        }
        val clipboard = getSystemService(ClipboardManager::class.java)
        val text = try {
            clipboard?.primaryClip
                ?.takeIf { it.itemCount > 0 }
                ?.getItemAt(0)
                ?.coerceToText(this)
                ?.toString()
        } catch (_: Throwable) {
            null
        }
        if (text.isNullOrBlank()) {
            toast("系统剪贴板没有文本")
            return
        }
        lifecycleScope.launch {
            val result = withContext(Dispatchers.IO) { Native.dmSendClipboardText(handle, text) }
            if (result == 0) {
                AppState.markLocalSent(text)
                tvClipboard.text = text.take(500)
                toast("已发送当前剪切板")
            } else {
                toast("发送失败（code=$result）：${Native.dmLastError() ?: ""}")
            }
        }
    }

    // ------------------------------------------------------------------
    // 刷新/渲染
    // ------------------------------------------------------------------

    private fun refreshAll() {
        val handle = AppState.handle
        if (handle == 0L) return
        lifecycleScope.launch {
            val snapshot = withContext(Dispatchers.IO) {
                Triple(
                    Native.dmPeersJson(handle),
                    Native.dmCatalogJson(handle),
                    Native.dmStatusJson(handle),
                )
            }
            renderPeers(parseJson(snapshot.first))
            renderCatalog(parseJson(snapshot.second))
            renderStatus(parseJson(snapshot.third))
        }
    }

    private fun refreshPeersOnly() {
        val handle = AppState.handle
        if (handle == 0L) return
        lifecycleScope.launch {
            val json = withContext(Dispatchers.IO) { Native.dmPeersJson(handle) }
            renderPeers(parseJson(json))
        }
    }

    private fun refreshCatalogOnly() {
        val handle = AppState.handle
        if (handle == 0L) return
        lifecycleScope.launch {
            val json = withContext(Dispatchers.IO) { Native.dmCatalogJson(handle) }
            renderCatalog(parseJson(json))
        }
    }

    private fun renderPeers(json: JSONObject) {
        if (!json.optBoolean("ok")) {
            tvPeers.text = "设备列表加载失败：${json.optString("error")}"
            return
        }
        val builder = StringBuilder()
        val self = json.optJSONObject("self")
        if (self != null) {
            builder.append("本机：")
                .append(self.optString("name"))
                .append(" (")
                .append(self.optString("device_id"))
                .append(")\n")
        }
        val peers = json.optJSONArray("peers")
        if (peers == null || peers.length() == 0) {
            builder.append("（暂无已配对设备，请先配对）")
        } else {
            for (index in 0 until peers.length()) {
                val peer = peers.optJSONObject(index) ?: continue
                builder.append(peer.optString("label"))
                    .append(" · ")
                    .append(if (peer.optBoolean("online")) "在线" else "离线")
                    .append(if (peer.optBoolean("session")) " · 会话已建立" else "")
                    .append("\n")
            }
        }
        tvPeers.text = builder.toString().trimEnd()
    }

    private fun renderCatalog(json: JSONObject) {
        if (!json.optBoolean("ok")) {
            tvCatalog.text = "目录加载失败：${json.optString("error")}"
            return
        }
        val entries = json.optJSONArray("entries")
        if (entries == null || entries.length() == 0) {
            tvCatalog.text = "目录为空（在桌面端添加软件后会同步到这里）"
            return
        }
        val builder = StringBuilder()
        builder.append("共 ").append(json.optInt("count")).append(" 项\n")
        for (index in 0 until entries.length()) {
            val entry = entries.optJSONObject(index) ?: continue
            builder.append("• ").append(entry.optString("name"))
                .append(" v").append(entry.optString("version"))
                .append(" [").append(entry.optString("platform"))
                .append("/").append(entry.optString("kind")).append("] ")
                .append(formatSize(entry.optLong("size")))
            val holders = entry.optJSONArray("holders")
            if (holders != null && holders.length() > 0) {
                builder.append(" · 持有设备 ").append(holders.length())
            }
            if (entry.optBoolean("local")) {
                builder.append(" · 本机已有")
            }
            builder.append("\n")
        }
        tvCatalog.text = builder.toString().trimEnd()
    }

    private fun renderStatus(json: JSONObject) {
        if (!json.optBoolean("ok")) {
            tvSyncStatus.text = "状态加载失败：${json.optString("error")}"
            return
        }
        tvSyncStatus.text = buildString {
            append("运行中=").append(json.optBoolean("running"))
            append(" · 在线=").append(json.optInt("peers_online"))
            append(" · 暂停=").append(json.optBoolean("paused"))
            append(" · 配对中=").append(json.optBoolean("pairing"))
            append(" · 中继=").append(json.optString("relay_url"))
        }
    }

    // ------------------------------------------------------------------
    // 原生事件
    // ------------------------------------------------------------------

    private fun onNativeEvent(event: JSONObject) {
        if (isFinishing || isDestroyed) return
        when (event.optString("type")) {
            "started" -> tvConnStatus.text =
                "已启动 · 设备 ${event.optString("name")} (${event.optString("device_id")})"
            "connected" -> {
                tvConnStatus.text = "已连接中继（设备 ${event.optString("device_id")}）"
                refreshPeersOnly()
            }
            "presence" -> {
                val online = event.optJSONArray("online")?.length() ?: 0
                tvConnStatus.text = "已连接中继 · 在线设备 $online 台"
            }
            "peer_online", "peer_offline", "session_established", "session_closed",
            "peers_reloaded", "peer_revoked", "peer_unpaired" -> {
                tvConnStatus.text = "同步状态更新"
                refreshPeersOnly()
            }
            "clipboard_sent" -> tvSyncStatus.text =
                "已自动发送剪切板 ${event.optLong("size")} 字节 → ${event.optInt("peers")} 台设备"
            "clipboard_applied" -> tvSyncStatus.text =
                "收到远端剪切板 ${event.optLong("size")} 字节（来自 ${event.optString("peer_id")}）"
            "remote_clipboard_applied" -> tvClipboard.text = event.optString("preview")
            "pair_code" -> tvPairCode.text = formatPairCode(event.optString("code"))
            "pair_succeeded" -> {
                tvPairStatus.text =
                    "配对成功：${event.optString("name")} (${event.optString("peer_id")})，SAS=${event.optString("sas")}"
                refreshAll()
            }
            "pair_failed" -> tvPairStatus.text = "配对失败：${event.optString("message")}"
            "catalog_updated", "catalog_merged", "catalog_reloaded" -> refreshCatalogOnly()
            "relay_error", "error" -> {
                val message = event.optString("message")
                AppState.lastError = message
                tvSyncStatus.text = "错误：$message"
            }
            "paused" -> tvSyncStatus.text =
                if (event.optBoolean("paused")) "剪切板同步已暂停" else "剪切板同步已恢复"
            "stopped" -> tvConnStatus.text = "连接已断开"
        }
    }

    // ------------------------------------------------------------------
    // 工具
    // ------------------------------------------------------------------

    private fun updateConnectedUi() {
        val connected = AppState.handle != 0L
        btnConnect.isEnabled = !connected
        btnDisconnect.isEnabled = connected
        tvConnStatus.text = if (connected) "已连接（后台自动同步中）" else "未连接"
    }

    private fun updateDisconnectedUi() {
        btnConnect.isEnabled = true
        btnDisconnect.isEnabled = false
        tvConnStatus.text = "未连接"
        tvSyncStatus.text = "—"
    }

    private fun requestNotificationPermissionIfNeeded() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1001)
        }
    }

    private fun parseJson(text: String?): JSONObject {
        return try {
            JSONObject(text ?: "{}")
        } catch (_: Exception) {
            JSONObject().put("ok", false).put("error", "原生返回的 JSON 解析失败")
        }
    }

    private fun formatPairCode(code: String): String {
        if (code.length != 8) return code.ifEmpty { "—" }
        return "${code.substring(0, 4)} ${code.substring(4)}"
    }

    private fun formatSize(bytes: Long): String {
        if (bytes < 1024) return "$bytes B"
        val kb = bytes / 1024.0
        if (kb < 1024) return String.format("%.1f KiB", kb)
        val mb = kb / 1024.0
        if (mb < 1024) return String.format("%.1f MiB", mb)
        return String.format("%.1f GiB", mb / 1024.0)
    }

    private fun toast(message: String) {
        Toast.makeText(this, message, Toast.LENGTH_LONG).show()
    }
}
