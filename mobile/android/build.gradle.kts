// drive-sync Android 工程（Kotlin DSL）。
// Rust 原生库由 CI 用 cargo-ndk 产出到 app/src/main/jniLibs/<abi>/libdrive_mobile.so。
plugins {
    id("com.android.application") version "8.11.1" apply false
    id("org.jetbrains.kotlin.android") version "2.1.21" apply false
}
