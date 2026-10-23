# drive-sync Android 端（可侧载 APK · 无感剪切板同步原型）

v0.2.0 的 Android 端是**可侧载 APK**：单 Activity 控制台 + 前台服务保持同步。
本机 Windows 无 Android SDK/NDK，因此 APK 由 GitHub Actions 构建；Rust 端（`crates/mobile-core`）
只要求 `cargo check -p drive-mobile` 通过。

- applicationId：`dev.drivesync.mobile`
- minSdk 29（Android 10）· targetSdk 36 · compileSdk 36 · 版本 `0.2.0`
- 原生库：`libdrive_mobile.so`（Rust cdylib + JNI 壳，ABI：arm64-v8a / armeabi-v7a / x86_64）

## 1. 获取 APK

### GitHub Actions 构建（推荐）

1. 打开仓库 Actions → **Android APK** 工作流；
2. `Run workflow`（workflow_dispatch）或在 `main` 分支 push 触发；
3. 构建完成后下载 artifact **`drive-sync-android-debug.apk`**（debug 签名，可直接侧载）。

工作流做了什么：安装 Rust Android targets + Android SDK 36 + NDK r27c + `cargo-ndk`，
编译 `cargo ndk ... build -p drive-mobile --release` 产出三种 ABI 的 `.so` 到
`mobile/android/app/src/main/jniLibs/<abi>/`，再 `./gradlew assembleDebug`，最后上传 APK。

### 本地构建（有 Android SDK/NDK 的机器）

```bash
# 1) Rust .so（在仓库根目录）
cargo install cargo-ndk
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 \
  -o mobile/android/app/src/main/jniLibs \
  build -p drive-mobile --release

# 2) APK
cd mobile/android
gradle wrapper --gradle-version 8.13   # 仓库未提交 gradle-wrapper.jar 时先生成
./gradlew assembleDebug
# 产物：mobile/android/app/build/outputs/apk/debug/app-debug.apk
```

> 仓库只提交了 `gradle/wrapper/gradle-wrapper.properties`，没有二进制 `gradle-wrapper.jar`
> （避免提交二进制）。CI 用 `gradle wrapper` 生成；本地同样处理。

## 2. 安装（侧载）

1. 手机 `设置 → 关于手机` 连点版本号开启开发者选项，允许「安装未知来源应用」（针对文件管理器/浏览器）；
2. 把 `drive-sync-android-debug.apk` 传到手机（`adb install -r drive-sync-android-debug.apk` 也可）；
3. 打开 **drive-sync**，允许通知权限（Android 13+ 用于「收到远端剪切板」提醒和前台服务通知）。

## 3. 连自建 relay

在电脑/VPS 上启动中继（见仓库 README / `docs/deployment.md`）：

```bash
./drive-relay --bind 0.0.0.0:8787 --token "<强随机令牌>" --data-dir ./relay-state
```

手机 App 内填写：

| 字段 | 示例 | 说明 |
|---|---|---|
| 中继地址 | `ws://192.168.1.10:8787/ws` | 手机与中继需网络互通；公网部署建议 `wss://`（反代 TLS，客户端见 `docs/deployment.md`） |
| 访问令牌 | `<强随机令牌>` | 与 `drive-relay --token` 一致；端到端加密，中继只转发密文 |
| 本机设备名 | `我的 Android` | 用于在桌面端识别设备 |

点「保存配置并初始化设备」→「连接」。连接成功后状态栏显示已连接，前台服务启动。

> 桌面端同样用 `drive-sync init --name ...` / `drive-sync pair|daemon` 连接同一 relay 与 token。

## 4. 配对（一次性 8 位码 + SAS）

- **手机发起**：点「发起配对（生成 8 位码）」，把 8 位码发给桌面端：
  `drive-sync pair --relay ws://... --token ... --code 12345678`；
- **手机加入**：桌面端 `drive-sync pair` 会打印 8 位码，在 App 输入框填入后点「加入配对」；
- 配对过程中同步会话会**临时暂停**（中继对同一设备的新连接会踢旧连接），配对完成后自动恢复；
- 原型阶段移动端自动确认 SAS（结果里会显示 6 位 SAS，可与桌面端核对）。配对长期有效，
  90 天不活跃自动失效；任一方删除后立即撤销。

## 5. 剪切板：无感自动同步

目标与桌面端一致：**检测到本机剪贴板新内容就自动加密发送；收到远端内容自动写回系统剪贴板**
（不用每次点按钮）。按钮「立即发送当前剪切板」仅作手动兜底。

实现方式：

- App 进程内每 **900ms** 轮询 `ClipboardManager`，SHA-256 去重；
- 收到远端文本 → 自动 `setPrimaryClip` 写回（Android 10+ **后台写剪贴板不受限**）+ 系统通知（不显示内容明文）；
- **回环抑制**：远端写入后 5 秒内同哈希内容不再自动发回；Rust 侧引擎另有 BLAKE3 去重/回环抑制；
- 暂停开关：只暂停剪切板，连接与目录同步保持。

### 前台自动（默认可用）

App 有可见界面（进程处于前台）时，Android 允许读取剪贴板，自动发送完整可用。

### 后台自动（需额外授权，侧载测试推荐）

Android 10+ 限制**后台应用读取剪贴板**：退到后台后普通 App 读不到剪贴板内容。
本 App 已启动 `dataSync` 前台服务保持进程与连接，但要后台读取剪贴板仍需授权：

```bash
# 侧载/测试场景：允许后台读剪贴板（先安装 App）
adb shell appops set dev.drivesync.mobile READ_CLIPBOARD allow
# 撤销：
adb shell appops set dev.drivesync.mobile READ_CLIPBOARD default
```

授权后**重新打开 App 并连接**（前台服务会持续轮询），即可接近桌面端的后台无感同步。
未授权时自动降级为「仅前台自动」，后台仅继续接收（写剪贴板不受限），不会报错。

> 注意：`READ_CLIPBOARD` appop 不是常规运行时权限，无法在 UI 内申请；仅适合侧载/自测/企业内部分发，
> 上架 Google Play 的正式版不能依赖该授权。

## 6. 已知限制 / 风险

| 限制 | 说明 |
|---|---|
| 后台读剪贴板 | 见 §5，需要 `adb shell appops ... READ_CLIPBOARD allow`；未授权只会前台自动 |
| 前台服务时长 | Android 15+（API 35）对 `dataSync` 类型前台服务有 **24 小时内累计 6 小时**限制，超时系统回调 `onTimeout`，本 App 会停服务并提示；重新打开 App 可继续 |
| 进程被杀 | 国产 ROM 可能清理后台进程，导致连接/轮询中断（原型未做引导/白名单跳转） |
| 通知权限 | Android 13+ 未授予 `POST_NOTIFICATIONS` 时收不到「收到剪切板」提醒（同步仍工作） |
| 图片/富文本 | 仅文本剪切板自动写回；图片/文件列表只同步事件，原型不写入系统剪贴板 |
| 电池优化 | 通知栏/设置里关闭本 App 的电池优化可提升后台存活率（原型未做系统跳转） |
| 配对并发 | 配对会临时暂停同步会话（约 1~2 秒），期间剪切板发送返回 -3 |
| 无法本机验证 | Windows 无 Android SDK/NDK，APK 真机行为以 CI 构建产物 + 手机实测为准 |

## 7. 架构与代码位置

```
crates/mobile-core/            Rust：C ABI + JNI 壳（cdylib + staticlib）
├── include/drive_mobile.h     C ABI 契约（iOS 共用）
└── src/{ffi,runtime,jni}.rs   FFI / tokio 运行时封装 / JNI 导出
mobile/android/                Gradle 工程（Kotlin）
├── app/src/main/java/dev/drivesync/mobile/
│   ├── Native.kt              JNI 绑定（object Native 的 external fun）
│   ├── MainActivity.kt        单 Activity：配置/连接/配对/剪切板/目录
│   ├── AppState.kt            进程级状态、远端剪贴板写回、回环抑制
│   ├── ClipboardSync.kt       900ms 轮询自动发送
│   ├── SyncService.kt         dataSync 前台服务
│   └── NotificationHelper.kt  通知
.github/workflows/android.yml  CI：Rust .so → assembleDebug → APK artifact
```

JNI 符号与 Kotlin `Native` 方法一一对应：`Java_dev_drivesync_mobile_Native_dmInit` 等；
事件/剪切板回调从 Rust 后台线程触发（JNI 每次都 attach/detach），Kotlin 侧切主线程后更新 UI。
