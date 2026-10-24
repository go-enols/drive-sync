# drive-sync Android 端（v0.3.2：打开即同步 · App 内权限引导 · PolyBot 深色/浅色 UI）

Android 端是**可侧载 APK**：单 Activity 控制台 + 前台服务保持同步，UI 采用 PolyBot 风格，
**同时支持深色与浅色两套配色**（默认跟随系统，也可在「设置 → 外观主题」手动选择）。
**打开 App 即自动连接并开始同步**（首次启动仍走引导；可在设置关闭自动开始）。
所有需要的权限与操作都在 **App 内完成**，无需命令行或额外工具。

Rust 端（`crates/mobile-core`）本版本无改动；APK 由 GitHub Actions 构建，
本地也可用 Android SDK/NDK 等价构建（见 §10）。

- applicationId：`dev.drivesync.mobile`
- minSdk 29（Android 10）· targetSdk 36 · compileSdk 36 · 版本 `0.3.2`（versionCode 5）
- 原生库：`libdrive_mobile.so`（Rust cdylib + JNI 壳，ABI：arm64-v8a / armeabi-v7a / x86_64）

## 1. 获取 APK

### GitHub Actions 构建（推荐）

1. 打开仓库 Actions → **Android APK** 工作流；
2. `Run workflow`（workflow_dispatch）或在 `main` 分支 push 触发；
3. 构建完成后下载 artifact **`drive-sync-android-debug.apk`**
   （artifact 内含 `drive-sync-android-debug.apk` 与版本化文件 `drive-sync-v0.3.2-android-debug.apk`，
   debug 签名，可直接侧载）。

工作流：安装 Rust Android targets + Android SDK 36 + NDK r27c + `cargo-ndk`，
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

> 仓库只提交 `gradle/wrapper/gradle-wrapper.properties`，不提交二进制 `gradle-wrapper.jar`；
> CI 用 `gradle wrapper` 生成。

## 2. 安装（侧载）

1. 手机 `设置 → 关于手机` 连点版本号开启开发者选项，允许「安装未知来源应用」（文件管理器/浏览器）；
2. 把 APK 传到手机安装（或用文件管理器打开）；
3. 打开 **drive-sync**：首次启动会弹出引导，建议点「去开启」进入权限页逐项授权。

### 打开即用（自动开始同步）

- 首次启动：完成引导与 relay/token/设备名配置（点一次「保存配置并初始化设备」→「连接」即可）。
- **之后每次打开 App**：只要配置完整且「设置 → 启动与同步 → 打开应用时自动开始同步」开启（默认开启），
  App 会自动连接中继并启动前台服务，**无需再点“连接”**；界面直接显示连接/在线状态。
- 手动「断开」后本次运行保持断开；下次重新打开 App 仍按开关自动开始。
- 关闭该开关后：打开 App 不自动连接，需要手动点「连接」；手动连接/断开始终可用。

## 3. 连自建 relay

在电脑/VPS 上启动中继（见仓库 README / `docs/deployment.md`）：

```bash
./drive-relay --bind 0.0.0.0:8787 --token "<强随机令牌>" --data-dir ./relay-state
```

手机 App 内填写：

| 字段 | 示例 | 说明 |
|---|---|---|
| 中继地址 | `ws://192.168.1.10:8787/ws` | 手机与中继需网络互通；公网部署建议 `wss://`（反代 TLS） |
| 访问令牌 | `<强随机令牌>` | 与 `drive-relay --token` 一致；端到端加密，中继只转发密文 |
| 本机设备名 | `我的 Android` | 用于在桌面端识别设备 |

点「保存配置并初始化设备」→「连接」。连接成功后通知栏出现 drive-sync 常驻状态。

## 4. 权限引导（全部在 App 内完成）

入口：主页「权限与后台同步」按钮（首次启动引导也会进入）。权限状态页每项显示
**已授权/未授权 + “去开启”按钮**，未授权时在「当前能力状态」里给出降级提示。

| 权限 | 是否必须 | 用途 | App 内申请方式 |
|---|---|---|---|
| 通知权限（Android 13+） | 建议 | 远端剪切板提醒、通知栏「同步当前剪贴板」一键按钮、前台服务状态 | 页面内点「去开启」→ 系统运行时弹窗 |
| 电池优化豁免 | 可选 | 保持后台服务存活，减少被系统冻结 | 点「去开启」→ 系统对话框「允许/不允许」 |
| 悬浮窗权限 | 可选 | 显示「悬浮同步球」，后台发送一键完成 | 点「去开启」→ 跳转系统「显示在其他应用上层」开关 |
| 开机自启 | 可选 | 重启后尝试自动恢复后台同步 | 页面内开关（系统级广播权限，普通权限） |

说明：

- 这些操作都是系统标准 UI；卸载重装或清除数据后需要重新授权。
- **Android 14/15 限制**：开机时用 `BOOT_COMPLETED` 启动 `dataSync` 前台服务可能被系统拒绝；
  失败时 App 会发通知「后台同步未自动恢复」，点按打开 App 即可继续（不会崩溃）。
- 电池优化豁免在部分国产 ROM 上仍需在系统「后台运行/自启动」里额外允许，属于厂商策略。

## 5. 剪切板同步与「Android 能力边界」

目标与桌面端一致：检测到本机剪贴板新内容就自动加密发送；收到远端内容自动写回系统剪贴板。
但 Android 10+ 对**后台读取剪贴板**有系统级限制（任何普通权限都绕不过），因此能力边界如下：

| 场景 | 能力 | 说明 |
|---|---|---|
| App 在前台 | **双向全自动** | 进程内 900ms 轮询剪贴板自动发送；收到远端自动写回 + 通知 |
| App 在后台（接收方向） | **全自动** | `dataSync` 前台服务保持连接；收到远端文本自动写入系统剪贴板（后台写不受限）+ 通知 |
| App 在后台（发送方向） | **一键完成**（非静默） | 系统禁止后台读剪贴板；点通知栏「同步当前剪贴板」或悬浮同步球，透明页面取得焦点后读取并发送，随后自动关闭 |
| 完全后台静默发送 | 不支持 | 需要系统级特殊权限（如 root 或默认输入法），本项目**不要求也不提供**这类步骤 |

### 一键发送的实现

- **通知 Action**：常驻通知上的「同步当前剪贴板」按钮 → 打开透明 Activity
  （`ClipboardQuickActivity`）→ 取得前台焦点 → 读取剪贴板 → `dm_send_clipboard_text` → 自动关闭；
- **悬浮同步球**：授权悬浮窗后在权限页开启；单击小球走同一条透明页流程，长按关闭小球；
- **去重**：通知/悬浮球对同一内容有 10 秒去重窗口；收到远端写入的内容还有 5 秒回环抑制
  （不会“刚收到又发回去”）；前台轮询也跳过手动一键发过的内容。
- 两种方式都只用普通权限。

### 其他同步细节

- 暂停开关：只暂停剪切板，连接与目录同步保持；
- 收到通知提示不展示剪切板内容明文（敏感信息）；
- 图片/富文本：本版本仅文本自动写回，其他类型只同步事件。

## 6. 操作入口审计（全部在 App 内完成）

结论：**没有任何功能需要命令行或额外工具**；安装 APK 本身只需在系统 UI 里允许「未知来源」。
自动连接、权限申请、配对、剪切板、主题、开机自启、断开/重连全部在 App 内完成：

| 功能 | 入口（App 内） | 是否需要命令行 |
|---|---|---|
| 首次配置（relay/token/设备名） | 主页「中继与设备」→「保存配置并初始化设备」 | 否 |
| 连接 / 断开 / 自动开始 | 自动（默认）或主页「连接 / 断开」；设置页开关 | 否 |
| 配对（发起/加入、SAS） | 主页「配对」卡片 | 否 |
| 剪切板同步（自动/一键/暂停） | 主页剪切板卡片、通知 Action、悬浮同步球 | 否 |
| 权限申请（通知/电池/悬浮窗/开机自启） | 主页「权限与后台同步」页 | 否 |
| 外观主题（跟随系统/深色/浅色） | 主页「设置与外观主题」→ 设置页 | 否 |
| 在线设备 / 软件目录查看 | 主页「在线设备」「软件目录」卡片 + 刷新 | 否 |
| 退出同步/停止后台 | 主页「断开」；系统设置中停止 App | 否 |

移动端降级范围（非命令行缺失，而是产品裁剪）：软件目录**只读**（添加/删除/获取文件在桌面端完成）；
云端暂存与文件传输暂未做手机 UI；手机端定位为“剪切板 + 配对 + 目录查看”。

## 7. 已知限制 / 风险

| 限制 | 说明 |
|---|---|
| 后台读剪贴板 | Android 10+ 系统限制；发送需通知/悬浮球一键，接收始终自动 |
| 前台服务时长 | Android 15+（API 35）对 `dataSync` 前台服务有 **24 小时内累计 6 小时**限制，超时回调 `onTimeout` 停服务并提示；重新打开 App 可继续 |
| 开机自启 | Android 14/15 对 `BOOT_COMPLETED` 启动 dataSync 前台服务有限制，失败会发通知提示手动打开 |
| 进程被杀 | 国产 ROM 可能清理后台进程；可在系统设置允许自启动/后台运行，或开启电池优化豁免 |
| 通知权限 | Android 13+ 未授权时收不到提醒与一键按钮（同步本身仍工作） |
| 悬浮球 | 需要悬浮窗权限；部分 ROM 会限制悬浮窗显示 |
| 电池优化 | 未豁免时后台服务可能被更快冻结；页面内有引导 |
| 配对并发 | 配对会临时暂停同步会话（约 1~2 秒），期间剪切板发送返回 -3 |
| 无法真机验证 | 构建/资源/JNI 已在本机等价验证，但 UI、权限弹窗、通知/悬浮球交互需真机实测 |

## 8. PolyBot 风格 UI（深色 / 浅色）

入口：主页「设置与外观主题」→ 设置页「外观主题」：**跟随系统（默认）/ 深色 / 浅色**。
选择写入 SharedPreferences（键 `theme_mode`）并立即生效：
`AppCompatDelegate.setDefaultNightMode` 会自动重建已打开页面，资源目录
`values/colors.xml`（浅色）与 `values-night/colors.xml`（深色）随之切换，
状态栏/导航栏图标颜色也由 `Theme.DriveSync`（values / values-night）跟随。

| 元素 | 深色 | 浅色 |
|---|---|---|
| 背景 | `#0B0F14` | `#F5F7FA` |
| 卡片 | `#111820`（边框 `#1E2A33`） | `#FFFFFF`（边框 `#E2E8F0`） |
| 主文字 / 次要文字 | `#E6EDF3` / `#9CA3AF` | `#0B1220` / `#64748B` |
| 主色（按钮/高亮/选中） | 薄荷绿 `#22E3B0`（按钮文字 `#06121A`） | 薄荷绿 `#0EA37F`（按钮文字 `#FFFFFF`） |
| 危险色 | `#F87171` | `#DC2626` |
| 输入框 | `#0B1016`，聚焦薄荷绿边框 | `#F8FAFC`，聚焦薄荷绿边框 |
| 顶部品牌栏 | 圆角薄荷绿 logo + `drive-sync` + `v0.3.2` 徽章 | 同左 |
| 通知色 | 薄荷绿 `#22E3B0` 强调 | 薄荷绿 `#0EA37F` 强调 |
| 状态栏 / 导航栏 | 背景色 + 浅色图标 | 背景色 + 深色图标 |

覆盖页面：主页（连接/配对/剪切板/设备/目录）、设置页（外观主题/启动与同步/权限入口/关于）、
权限与后台同步页、透明一键同步页、悬浮同步球与通知样式。

## 9. 架构与代码位置

```
crates/mobile-core/            Rust：C ABI + JNI 壳（cdylib + staticlib；本版本未改）
├── include/drive_mobile.h     C ABI 契约（iOS 共用）
└── src/{ffi,runtime,jni}.rs   FFI / tokio 运行时封装 / JNI 导出
mobile/android/                Gradle 工程（Kotlin）
├── app/src/main/java/dev/drivesync/mobile/
│   ├── Native.kt              JNI 绑定（object Native 的 external fun）
│   ├── MainActivity.kt        主页：打开即自动连接/配对/剪切板/设备/目录 + 首次引导
│   ├── PermissionsActivity.kt 权限状态页：通知/电池/悬浮窗/开机自启 + 能力边界
│   ├── SettingsActivity.kt    设置页：外观主题 + 启动与同步开关 + 权限入口 + 关于
│   ├── ThemePrefs.kt          主题偏好持久化 + AppCompat night mode + 非 Activity 组件配色判断
│   ├── ClipboardQuickActivity.kt 透明一键同步页（通知 Action / 悬浮球）
│   ├── FloatingBallManager.kt 悬浮同步球（授权悬浮窗后可用）
│   ├── SyncService.kt         dataSync 前台服务（常驻通知 + 一键 Action）
│   ├── SyncLauncher.kt        统一启动原生运行时 + 自动开始开关/配置完整性判断
│   ├── BootReceiver.kt        可选开机自启（失败时通知提示）
│   ├── AppState.kt            进程级状态、远端剪贴板写回、去重/回环抑制
│   ├── ClipboardSync.kt       900ms 前台自动轮询发送
│   └── NotificationHelper.kt  深色/薄荷绿通知、一键同步 Action
└── app/src/main/res/          PolyBot 深色/浅色主题（values + values-night）、卡片/徽章/logo/悬浮球 drawable
.github/workflows/android.yml  CI：Rust .so → assembleDebug → APK artifact
```

JNI 符号与 Kotlin `Native` 方法一一对应：`Java_dev_drivesync_mobile_Native_dmInit` 等；
事件/剪切板回调从 Rust 后台线程触发（JNI 每次 attach/detach），Kotlin 侧切主线程后更新 UI。

## 10. 构建与验收

- CI：`.github/workflows/android.yml`（workflow_dispatch + push），产物 artifact
  `drive-sync-android-debug.apk`（含 `drive-sync-v0.3.2-android-debug.apk`）；
- Rust：`cargo check -p drive-mobile` 必须通过（本版本 Android 侧改动不涉及 Rust；已验证 exit 0）；
- 真机验收清单：安装 → 首次引导 → 权限页逐项授权 → 配置 relay（保存+连接一次）→
  **杀掉 App 后重新打开：应自动连接并启动前台服务（无需点连接）** → 桌面端配对 →
  复制文本（前台自动同步）→ 退到后台在桌面端复制（手机通知出现并自动写剪贴板）→
  点通知「同步当前剪贴板」/悬浮球（后台发送成功）→ 目录列表展示 →
  设置页切换深色/浅色/跟随系统（立即生效）→ 设置页关闭自动开始开关后重开 App（应停在未连接，可手动连接）。

### 本版本（v0.3.2）本地等价构建验证

已在 Windows 上使用 **Android SDK 36 / build-tools 36 / NDK 27 + Gradle 8.14.3 + JDK 21**
跑通与 CI 等价的完整构建（CI 使用 Gradle 8.13 + JDK 17 + NDK r27c，命令一致）：

- `cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 --platform 29 -o ... build -p drive-mobile --release`
  → 3 个 ABI 的 `libdrive_mobile.so` 构建成功；
- `gradle assembleDebug` → **BUILD SUCCESSFUL**，APK 约 22.7 MB；
- `aapt2 dump badging` 校验：`dev.drivesync.mobile` / `versionCode 5` / `versionName 0.3.2` /
  minSdk 29 / targetSdk 36 / MainActivity 可启动，权限清单包含通知、前台服务(dataSync)、
  开机自启、悬浮窗、电池优化豁免；
- APK 内含 3 个 ABI 的 `libdrive_mobile.so`，导出 15 个
  `Java_dev_drivesync_mobile_Native_*` JNI 符号与 16 个 `dm_*` C ABI 符号（已用 llvm-nm 核对）。
- 本机构建产出的临时 APK（不入库，供真机安装验收）：
  `target/android-apk/drive-sync-android-debug.apk`（另有 `drive-sync-v0.3.2-android-debug.apk`）；
  CI push 后会重新构建并上传 artifact。

