package dev.drivesync.mobile

import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import androidx.lifecycle.lifecycleScope
import com.google.android.material.button.MaterialButton
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import com.google.android.material.switchmaterial.SwitchMaterial
import com.google.android.material.textfield.TextInputEditText
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.io.File

/**
 * PolyBot 风格控制台：打开即自动连接/同步（可关闭）→ 配对 → 剪切板同步 → 设备/目录 → 权限引导。
 *
 * - 打开即用：已完成首次配置且「打开应用时自动开始同步」开启（默认）时，
 *   MainActivity 启动后自动连接并拉起前台服务，无需点“连接”；
 * - 首次启动/未配置时仍走引导与手动配置；
 * - 剪切板能力（如实呈现 Android 平台边界）：前台自动轮询发送；后台接收自动写剪贴板；
 *   后台发送用通知 Action / 悬浮同步球一键完成。
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
    private lateinit var layoutCatalog: LinearLayout
    private lateinit var tvPermissionHint: TextView
    private lateinit var btnSaveConfig: MaterialButton
    private lateinit var btnConnect: MaterialButton
    private lateinit var btnDisconnect: MaterialButton
    private lateinit var btnPairStart: MaterialButton
    private lateinit var btnPairJoin: MaterialButton
    private lateinit var btnSendClipboard: MaterialButton
    private lateinit var btnRefresh: MaterialButton
    private lateinit var btnPermissions: MaterialButton
    private lateinit var btnSettings: MaterialButton
    private lateinit var swPause: SwitchMaterial

    /** 自动连接进行中（避免与手动“连接”重复触发）。 */
    private var autoConnecting = false

    private val prefs by lazy {
        getSharedPreferences(SyncLauncher.PREFS, Context.MODE_PRIVATE)
    }

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
        val onboarded = prefs.getBoolean(SyncLauncher.KEY_ONBOARDING_DONE, false)
        showOnboardingIfNeeded()
        if (AppState.handle != 0L) {
            updateConnectedUi()
            refreshAll()
        } else if (onboarded) {
            // 打开即同步：已过首次引导且配置完整时自动连接（首次启动不触发，仍走引导）。
            maybeAutoStart()
        }
        // 自动检查更新：每天最多一次；发现新版本时弹窗提示（Android 侧载无法静默安装）。
        UpdateChecker.maybeAutoCheck(this) { info ->
            if (!isFinishing && !isDestroyed) {
                UpdateChecker.showDialog(this, info)
            }
        }
    }

    override fun onResume() {
        super.onResume()
        FloatingBallManager.syncFromPrefs(this)
        updatePermissionHint()
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
        layoutCatalog = findViewById(R.id.layoutCatalog)
        tvPermissionHint = findViewById(R.id.tvPermissionHint)
        btnSaveConfig = findViewById(R.id.btnSaveConfig)
        btnConnect = findViewById(R.id.btnConnect)
        btnDisconnect = findViewById(R.id.btnDisconnect)
        btnPairStart = findViewById(R.id.btnPairStart)
        btnPairJoin = findViewById(R.id.btnPairJoin)
        btnSendClipboard = findViewById(R.id.btnSendClipboard)
        btnRefresh = findViewById(R.id.btnRefresh)
        btnPermissions = findViewById(R.id.btnPermissions)
        btnSettings = findViewById(R.id.btnSettings)
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
        btnPermissions.setOnClickListener {
            startActivity(Intent(this, PermissionsActivity::class.java))
        }
        btnSettings.setOnClickListener {
            startActivity(Intent(this, SettingsActivity::class.java))
        }
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
        etRelay.setText(prefs.getString(SyncLauncher.KEY_RELAY, ""))
        etToken.setText(prefs.getString(SyncLauncher.KEY_TOKEN, ""))
        etDeviceName.setText(
            prefs.getString(SyncLauncher.KEY_DEVICE_NAME, "Android 手机"),
        )
    }

    private fun savePrefs(relay: String, token: String, deviceName: String) {
        prefs.edit()
            .putString(SyncLauncher.KEY_RELAY, relay)
            .putString(SyncLauncher.KEY_TOKEN, token)
            .putString(SyncLauncher.KEY_DEVICE_NAME, deviceName)
            .apply()
    }

    /** 读取并校验输入；返回 null 表示校验失败（已 toast）。 */
    private fun readConfig(): Triple<String, String, String>? {
        val rawRelay = etRelay.text?.toString()?.trim().orEmpty()
        val relay = SyncLauncher.normalizeRelayUrl(rawRelay)
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
        if (relay != rawRelay) {
            etRelay.setText(relay)
            toast("公网中继已自动改用 wss://（明文 ws 会被服务器 301 拒绝）")
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
                val init = SyncLauncher.parseJson(Native.dmInit(dataDir, config.third))
                if (!init.optBoolean("ok")) {
                    return@withContext "初始化失败：${init.optString("error")}"
                }
                val relayJson = JSONObject()
                    .put("relay_url", config.first)
                    .put("access_token", config.second)
                    .put("device_name", config.third)
                val saved = SyncLauncher.parseJson(Native.dmConfigSetRelay(relayJson.toString()))
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
        if (autoConnecting) {
            toast("正在自动连接中…")
            return
        }
        readConfig() ?: return
        btnConnect.isEnabled = false
        lifecycleScope.launch {
            val result = withContext(Dispatchers.IO) { SyncLauncher.ensureRuntime(this@MainActivity) }
            btnConnect.isEnabled = true
            if (result != "ok") {
                tvConnStatus.text = "启动失败：$result"
                toast("启动失败：$result")
                return@launch
            }
            startSyncServices()
            updateConnectedUi()
            watchConnectionTimeout()
            toast("已启动连接，正在连接中继…")
            refreshAll()
        }
    }

    /**
     * 打开即同步：已保存 relay/token 且设置开关开启时，启动后自动连接并启动前台服务。
     * 未配置、首次引导、或用户在设置里关闭开关时保持手动模式。
     */
    private fun maybeAutoStart() {
        if (AppState.handle != 0L || autoConnecting) return
        if (!SyncLauncher.isAutoStartEnabled(this)) {
            tvConnStatus.text = "未连接（已关闭「打开应用时自动开始同步」，可手动点「连接」）"
            return
        }
        if (!SyncLauncher.hasCompleteConfig(this)) {
            tvConnStatus.text = "未配置中继：填写下方信息后点「连接」完成首次设置"
            return
        }
        autoConnecting = true
        btnConnect.isEnabled = false
        tvConnStatus.text = "正在自动连接中继…"
        lifecycleScope.launch {
            val result = withContext(Dispatchers.IO) { SyncLauncher.ensureRuntime(this@MainActivity) }
            autoConnecting = false
            if (result != "ok") {
                updateConnectedUi()
                tvConnStatus.text = "自动连接失败：$result（可手动点「连接」重试）"
                return@launch
            }
            startSyncServices()
            updateConnectedUi()
            watchConnectionTimeout()
            refreshAll()
        }
    }

    /** 连接看门狗：20 秒仍停在“正在连接”就给出明确提示，避免无限等待。 */
    private fun watchConnectionTimeout() {
        window.decorView.postDelayed({
            if (AppState.handle != 0L && tvConnStatus.text?.contains("正在连接") == true) {
                tvConnStatus.text = "连接超时：检查网络/VPN，或到设置「复制诊断日志」发给我"
            }
        }, 20_000)
    }

    /** 启动前台服务 + 剪切板轮询 + 悬浮球（手动/自动连接共用）。 */
    private fun startSyncServices() {
        try {
            SyncService.start(this)
        } catch (error: Exception) {
            tvSyncStatus.text = "前台服务启动失败：${error.message ?: "未知错误"}"
        }
        ClipboardSync.ensureStarted()
        FloatingBallManager.syncFromPrefs(this)
    }

    private fun disconnect() {
        SyncLauncher.stopRuntime()
        AppState.paused = false
        swPause.isChecked = false
        ClipboardSync.stop()
        SyncService.stop(this)
        FloatingBallManager.hide()
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
            val result = withContext(Dispatchers.IO) {
                SyncLauncher.parseJson(Native.dmPairStart(handle))
            }
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
                SyncLauncher.parseJson(Native.dmPairJoin(handle, input))
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
                AppState.markSent(text)
                tvClipboard.text = text.take(500)
                toast("已发送当前剪贴板")
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
            renderPeers(SyncLauncher.parseJson(snapshot.first))
            renderCatalog(SyncLauncher.parseJson(snapshot.second))
            renderStatus(SyncLauncher.parseJson(snapshot.third))
        }
    }

    private fun refreshPeersOnly() {
        val handle = AppState.handle
        if (handle == 0L) return
        lifecycleScope.launch {
            val json = withContext(Dispatchers.IO) { Native.dmPeersJson(handle) }
            renderPeers(SyncLauncher.parseJson(json))
        }
    }

    private fun refreshCatalogOnly() {
        val handle = AppState.handle
        if (handle == 0L) return
        lifecycleScope.launch {
            val json = withContext(Dispatchers.IO) { Native.dmCatalogJson(handle) }
            renderCatalog(SyncLauncher.parseJson(json))
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
        layoutCatalog.removeAllViews()
        if (!json.optBoolean("ok")) {
            tvCatalog.text = "目录加载失败：${json.optString("error")}"
            return
        }
        val entries = json.optJSONArray("entries")
        if (entries == null || entries.length() == 0) {
            tvCatalog.text = "目录为空（在桌面端添加软件/共享文件夹后会同步到这里）"
            return
        }
        tvCatalog.text = "共 ${json.optInt("count")} 项；非本机文件可点右侧「下载」"
        val density = resources.displayMetrics.density
        val rowPadding = (6 * density).toInt()
        val maxRows = 300
        val shown = minOf(entries.length(), maxRows)
        for (index in 0 until shown) {
            val entry = entries.optJSONObject(index) ?: continue
            val local = entry.optBoolean("local")
            val holders = entry.optJSONArray("holders")?.length() ?: 0
            val row = LinearLayout(this).apply {
                orientation = LinearLayout.HORIZONTAL
                setPadding(0, rowPadding, 0, rowPadding)
            }
            val label = TextView(this).apply {
                text = buildString {
                    append(entry.optString("name"))
                    append("  ")
                    append(formatSize(entry.optLong("size")))
                    if (local) {
                        append(" · 本机")
                    } else if (holders > 0) {
                        append(" · $holders 台持有")
                    } else {
                        append(" · 等待持有设备上线")
                    }
                }
                textSize = 12.5f
                setTextColor(getColor(R.color.text_primary))
                layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
            }
            row.addView(label)
            if (!local) {
                val entryId = entry.optString("id")
                val entryName = entry.optString("name")
                val button = MaterialButton(this).apply {
                    text = "下载"
                    isAllCaps = false
                    minimumWidth = 0
                    minWidth = 0
                    setOnClickListener { downloadEntry(entryId, entryName) }
                }
                row.addView(button)
            }
            layoutCatalog.addView(row)
        }
        if (entries.length() > maxRows) {
            val more = TextView(this).apply {
                text = "…仅显示前 $maxRows 项（请在桌面端缩小共享范围或分文件夹）"
                textSize = 12f
                setTextColor(getColor(R.color.text_secondary))
            }
            layoutCatalog.addView(more)
        }
    }

    private fun downloadEntry(entryId: String, name: String) {
        val handle = AppState.handle
        if (handle == 0L) {
            toast("请先连接中继")
            return
        }
        lifecycleScope.launch {
            val json = withContext(Dispatchers.IO) { Native.dmFetchCatalog(handle, entryId) }
            val result = SyncLauncher.parseJson(json)
            if (result.optBoolean("ok")) {
                toast("开始下载：$name")
                tvSyncStatus.text = "正在下载：$name（完成后保存到 Downloads/drive-sync）"
            } else {
                toast("下载失败：${result.optString("error")}")
            }
        }
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
        }
    }

    // ------------------------------------------------------------------
    // 原生事件
    // ------------------------------------------------------------------

    private fun onNativeEvent(event: JSONObject) {
        if (isFinishing || isDestroyed) return
        when (event.optString("type")) {
            "started" -> tvConnStatus.text = "正在连接中继…"
            "connected" -> {
                tvConnStatus.text = "已连接中继"
                refreshPeersOnly()
            }
            "presence" -> {
                val online = event.optJSONArray("online")?.length() ?: 0
                tvConnStatus.text = "已连接 · 在线 $online 台"
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
            "transfer_received" -> {
                tvSyncStatus.text =
                    "文件已收到（${formatSize(event.optLong("size"))}），正在保存到 Downloads/drive-sync…"
            }
            "transfer_failed" -> {
                tvSyncStatus.text = "文件下载失败：${event.optString("message")}"
            }
            "file_saved" -> {
                tvSyncStatus.text = if (event.optBoolean("ok")) {
                    "已保存：Downloads/drive-sync/${event.optString("name")}"
                } else {
                    "文件保存失败：${event.optString("name")}"
                }
            }
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
                tvConnStatus.text = "连接失败（自动重试中）"
                tvSyncStatus.text = "错误：$message"
            }
            "paused" -> tvSyncStatus.text =
                if (event.optBoolean("paused")) "剪切板同步已暂停" else "剪切板同步已恢复"
            "stopped" -> tvConnStatus.text = "连接已断开"
        }
    }

    // ------------------------------------------------------------------
    // 首次引导 / 权限
    // ------------------------------------------------------------------

    private fun showOnboardingIfNeeded() {
        if (prefs.getBoolean(SyncLauncher.KEY_ONBOARDING_DONE, false)) return
        prefs.edit().putBoolean(SyncLauncher.KEY_ONBOARDING_DONE, true).apply()
        MaterialAlertDialogBuilder(this)
            .setTitle("欢迎使用 drive-sync")
            .setMessage(
                "下一步建议开启（均在 App 内完成）：\n" +
                    "• 通知权限：接收远端剪切板提醒与“一键同步”按钮\n" +
                    "• 电池优化豁免：保持后台服务存活\n" +
                    "• 可选悬浮窗：开启悬浮同步球\n\n" +
                    "Android 限制：App 在后台时系统禁止读取剪贴板，发送请点通知栏" +
                    "「同步当前剪贴板」或悬浮球；接收方向始终自动。",
            )
            .setPositiveButton("去开启") { _, _ ->
                startActivity(Intent(this, PermissionsActivity::class.java))
            }
            .setNegativeButton("稍后", null)
            .show()
    }

    private fun updatePermissionHint() {
        val notificationReady = PermissionsActivity.isNotificationGranted(this)
        tvPermissionHint.text = if (notificationReady) {
            "后台发送：下拉通知栏点「同步当前剪贴板」，或开启悬浮同步球（一键完成）。"
        } else {
            "通知权限未开启：会影响远端提醒与一键同步按钮，请到「权限与后台同步」开启。"
        }
    }

    // ------------------------------------------------------------------
    // 工具
    // ------------------------------------------------------------------

    private fun updateConnectedUi() {
        val started = AppState.handle != 0L
        btnConnect.isEnabled = !started
        btnDisconnect.isEnabled = started
        // handle 只表示原生运行时已启动，真正连上中继由 connected/presence 事件更新。
        tvConnStatus.text = if (started) "正在连接中继…" else "未连接"
    }

    private fun updateDisconnectedUi() {
        btnConnect.isEnabled = true
        btnDisconnect.isEnabled = false
        tvConnStatus.text = "未连接"
        tvSyncStatus.text = "—"
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
