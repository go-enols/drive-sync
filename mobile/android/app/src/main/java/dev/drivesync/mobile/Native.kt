package dev.drivesync.mobile

/**
 * JNI 绑定：与 `crates/mobile-core/src/jni.rs` 的导出符号一一对应。
 *
 * 符号规则：`Java_dev_drivesync_mobile_Native_<方法名>`（Kotlin `object` 的 native
 * 实例方法）。所有返回 JSON 的接口失败时也返回 JSON：`{"ok":false,"error":"..."}`。
 */
object Native {
    init {
        System.loadLibrary("drive_mobile")
    }

    /** 原生事件/剪切板回调（实现方需线程安全：回调来自 Rust 后台线程）。 */
    interface Callback {
        fun onEvent(json: String)
        fun onClipboard(text: String, fromDeviceId: String)
    }

    external fun dmVersion(): String
    external fun dmAbiVersion(): Int
    external fun dmLastError(): String?

    external fun dmInit(dataDir: String, deviceName: String): String
    external fun dmLoad(dataDir: String): String
    external fun dmConfigSetRelay(configJson: String): String

    external fun dmStart(relayUrl: String, token: String, callback: Callback): Long
    external fun dmStop(handle: Long)
    external fun dmSendClipboardText(handle: Long, text: String): Int
    external fun dmFetchCatalog(handle: Long, entryId: String): String
    external fun dmSetPaused(handle: Long, paused: Boolean): Int

    external fun dmPairStart(handle: Long): String
    external fun dmPairJoin(handle: Long, codeOrQr: String): String

    external fun dmPeersJson(handle: Long): String
    external fun dmCatalogJson(handle: Long): String
    external fun dmStatusJson(handle: Long): String
}
