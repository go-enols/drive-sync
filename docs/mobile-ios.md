# drive-sync iOS 客户端（SwiftUI）

> 版本：v0.2.0（2026-10 原型） · Bundle ID：`dev.drivesync.mobile` · Deployment Target：iOS 16.0
> 代码：`mobile/ios/**` · CI：`.github/workflows/ios.yml` · 共享 C ABI：`crates/mobile-core/include/drive_mobile.h`
> 协议基准：[protocol.md](protocol.md)；移动端平台调研：[platform-mobile.md](platform-mobile.md)

本轮交付一个**可编译、可安装到模拟器、可用个人 Apple ID 签安装到 iPhone** 的原型 App：
功能与 Android 端对齐（配置 / 配对 / 剪贴板收发 / 状态 / 设备列表 / 只读软件目录），
剪贴板采用**无感自动同步**（前台自动发、收到自动写），但受 iOS 平台限制：
**无后台静默能力（只有前台 + 后台约 25 秒接收窗口）、iOS 16+ 首次读取需系统粘贴授权、无 APNs 推送**。
能力边界见 [§9](#9-无感同步能力边界与-ios-平台限制必读)。

---

## 1. 架构

```
┌────────────────────────────── App 进程（iOS） ──────────────────────────────┐
│  SwiftUI Views (Connect / Pairing / Sync / Catalog)                         │
│        │ @EnvironmentObject                                                 │
│  AppModel（@MainActor 状态机：回调切主线程、1.5s 轮询剪贴板变化、3s 轮询状态） │
│        │ 后台线程执行阻塞调用（dm_pair_* / dm_*_json）                        │
│  RustMobileCore（Swift 门面：JSON 编解码、句柄、回调 user_data）              │
│        │ 桥接头 DriveSync-Bridging-Header.h → drive_mobile.h                │
│  libdrive_mobile.a（Rust staticlib：drive-core + drive-client runtime）      │
│        │ tokio runtime / WebSocket 客户端 / E2E 加密 / 自动重连 / spool       │
└────────────────────────────────────┼───────────────────────────────────────┘
                                     │ ws:// 或 wss://（E2E 加密，中继只见密文）
                              自建 relay（crates/relay）
```

要点：

- **网络、加密、配对、持久化全部在 Rust 侧**（`crates/mobile-core` 复用 `drive-core` / `drive-client`），Swift 只做 UI 与系统剪贴板；
- **无感同步**：前台每 ~1.5s 轮询 `UIPasteboard.changeCount` 检测变化 → 自动 `dm_send_clipboard_text`；收到远端内容 → 自动写 `UIPasteboard`。回环抑制用文本哈希 + 120s 时间窗，避免 A→B→A 回环（见 [§9](#9-无感同步能力边界与-ios-平台限制必读)）；
- Swift ↔ Rust 之间只有 **JSON 字符串进/出 + 两个 C 回调**；Rust 分配的字符串必须用 `dm_string_free` 释放；
- 回调来自 Rust 后台线程，`AppModel` 统一 `Task { @MainActor in … }` 切回主线程更新 `@Published`；
- 阻塞接口（`dm_pair_start` 最多 30s、`dm_pair_join` 最多 130s、`dm_*_json` 读磁盘）**禁止在主线程调用**，统一 `Task.detached` 执行。

## 2. 目录结构

```
mobile/ios/
├── project.yml                          # XcodeGen 工程定义（不提交 .xcodeproj）
├── DriveSync/
│   ├── DriveSyncApp.swift               # @main，scenePhase → 前台连接/后台断开
│   ├── AppModel.swift                   # 状态机、FFI 调用编排、事件日志、配置持久化
│   ├── Core/
│   │   ├── DriveMobileBridge.swift      # C ABI 薄封装（指针/字符串所有权 + 回调入口）
│   │   ├── RustMobileCore.swift         # Rust 门面（JSON → Swift 模型）
│   │   ├── MobileCoreModels.swift       # 身份/配对/设备/目录/事件模型
│   │   ├── ClipboardSync.swift          # 回环抑制 + 本地通知 + 轮询参数
│   │   └── KeychainStore.swift          # access token → Keychain
│   ├── Views/
│   │   ├── RootView.swift               # 四 Tab 框架 + Toast/Alert
│   │   ├── ConnectView.swift            # relay/token/设备名 + 连接按钮 + 身份/状态
│   │   ├── PairingView.swift            # 发起配对（8 位码 + QR）/ 加入配对 / 设备列表
│   │   ├── SyncView.swift               # 自动同步开关 / 手动兜底 / 接收记录 / 运行状态
│   │   ├── CatalogView.swift            # 只读软件目录
│   │   ├── EventLogView.swift           # 最近 200 条事件日志
│   │   └── Components.swift             # 状态卡/设备行/Toast/二维码等
│   ├── Resources/Assets.xcassets/       # AppIcon / AccentColor
│   ├── Info.plist
│   └── DriveSync-Bridging-Header.h      # #import "drive_mobile.h"
├── ffi/
│   ├── README.md                        # 静态库放置说明（.a 不提交）
│   ├── iphoneos/                        # 真机 arm64 libdrive_mobile.a
│   └── iphonesimulator/                 # 模拟器 arm64+x86_64 通用 libdrive_mobile.a
└── scripts/
    ├── build-ffi.sh                     # 构建 3 个 Rust target 并 lipo 成两个 .a
    └── gen-app-icon.py                  # 重新生成 1024×1024 App 图标（纯 Python）
```

## 3. 环境要求

| 用途 | 要求 |
|---|---|
| 本地构建 | macOS 13+、Xcode 15+（含 Command Line Tools）、`brew install xcodegen`、Rust stable + `rustup target add` |
| CI | GitHub Actions `macos-15`（自带 Xcode 15.x），无需签名证书 |
| 最低系统 | iOS 16.0 |

> 本仓库开发机是 Windows，**无法本地编译 Swift/Xcode**；Swift 代码由 CI 的 `xcodebuild` 验证。

## 4. 构建（macOS 本地）

```bash
# 0) 仓库根目录
cd drive-sync

# 1) 构建 Rust 静态库（真机 + 模拟器）→ mobile/ios/ffi/
bash mobile/ios/scripts/build-ffi.sh

# 2) 生成 Xcode 工程（project.yml → DriveSync.xcodeproj）
cd mobile/ios
xcodegen generate

# 3) 打开工程，选择 iPhone 模拟器，⌘R 运行
open DriveSync.xcodeproj
```

纯命令行构建模拟器 App（CI 同款）：

```bash
xcodebuild \
  -project mobile/ios/DriveSync.xcodeproj \
  -scheme DriveSync \
  -configuration Debug \
  -sdk iphonesimulator \
  -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath mobile/ios/build \
  CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO \
  build
# 产物：mobile/ios/build/Build/Products/Debug-iphonesimulator/DriveSync.app
```

## 5. 安装到模拟器

### 方式 A：下载 CI 产物（无 Mac 开发环境也可）

1. GitHub → Actions → **iOS** → 选一次成功运行 → 下载 artifact `drive-sync-ios-simulator`；
2. 解压得到 `DriveSync.app`；
3. 在 macOS 上安装并启动：

```bash
unzip drive-sync-ios-simulator.zip
xcrun simctl boot "iPhone 15" || true
open -a Simulator
xcrun simctl install booted DriveSync.app
xcrun simctl launch booted dev.drivesync.mobile
```

### 方式 B：Xcode 直接运行

`xcodegen generate` 后打开工程，选择模拟器（如 iPhone 15 / iOS 17），⌘R。

> 模拟器包是 **x86_64 + arm64 模拟器 slice**（不含真机 arm64），因此**不能直接安装到 iPhone**。

## 6. 安装到 iPhone（真机签名）

CI 产物只用于模拟器。真机安装需要 Apple 签名，两种路径：

### 6.1 免费 Apple ID（推荐个人测试）

1. macOS 上按第 4 节构建（`build-ffi.sh` 会生成真机 arm64 的 `ffi/iphoneos/libdrive_mobile.a`）；
2. `xcodegen generate` 后打开 `DriveSync.xcodeproj`；
3. 选中 **DriveSync** target → **Signing & Capabilities**：
   - 勾选 **Automatically manage signing**；
   - **Team** 选择你的 **Personal Team**（登录 Apple ID 后出现，免费）；
   - **Bundle Identifier** 改成全局唯一，例如 `dev.drivesync.mobile.<你的名字>`（免费账号不能用已被占用的 bundle id）；
4. 用数据线连接 iPhone，在 Xcode 工具栏选择你的设备 → ⌘R；
5. iPhone 上首次启动会提示“不受信任的开发者”：**设置 → 通用 → VPN 与设备管理 → 开发者 App → 信任**；
6. 免费签名限制：**7 天有效期**（到期重新 ⌘R）、同一 Apple ID 最多 3 个自签名 App、部分系统能力不可用（本 App 未用推送/后台能力，不受影响）。

### 6.2 TestFlight / App Store（需付费账号）

需要 Apple Developer Program（99 USD/年）与 App Store Connect 配置；本轮未提供导出存档/上传流程，且 App 使用自定义 E2E 加密，上架前需评估出口合规（`ITSAppUsesNonExemptEncryption`）与隐私标签（剪贴板内容、设备标识）。原型阶段建议只做个人/内测签名。

## 7. CI（`.github/workflows/ios.yml`）

**触发方式**

- 手动：GitHub → Actions → iOS → **Run workflow**（`workflow_dispatch`）；
- push 到 `main` 且改动命中：`mobile/ios/**`、`crates/mobile-core/**`、`crates/core/**`、`crates/client-core/**`、`Cargo.toml`、`Cargo.lock`、workflow 自身；
- PR 命中上述路径。

**流程**：checkout → 校验头文件 → 安装 Rust iOS targets（`aarch64-apple-ios` / `aarch64-apple-ios-sim` / `x86_64-apple-ios`）→ 缓存 Cargo → `scripts/build-ffi.sh` 构建 3 个 target 并 lipo → `brew install xcodegen` → `xcodegen generate` → `xcodebuild -sdk iphonesimulator -configuration Debug`（`CODE_SIGNING_ALLOWED=NO`）→ 打包 zip → 上传 artifact。

**产物**

| artifact | 内容 | 用途 |
|---|---|---|
| `drive-sync-ios-simulator` | `drive-sync-ios-simulator.zip`（含 `DriveSync.app`） | 模拟器安装/演示，**不能直装真机** |
| `drive-mobile-ios-staticlibs` | 真机 + 模拟器 `libdrive_mobile.a` | 无 Rust 环境的 Mac 可直接放入 `mobile/ios/ffi/` 用 Xcode 签名真机 |
| `ios-build-log`（仅失败时） | `mobile/ios/build.log` | 排查 CI 编译错误 |

## 8. C ABI 契约（与 Android 共用）

头文件：`crates/mobile-core/include/drive_mobile.h`（纯 C，`DM_ABI_VERSION = 1`）。
Swift 通过桥接头 `#import "drive_mobile.h"`，`HEADER_SEARCH_PATHS` 指向 `crates/mobile-core/include`。

| 函数 | 说明 | 线程 |
|---|---|---|
| `dm_version()` / `dm_abi_version()` | 版本/ABI 常量 | 任意 |
| `dm_init(data_dir, device_name)` | 初始化（或加载）设备身份，返回 JSON | 启动时一次 |
| `dm_load(data_dir)` | 只加载已有身份 | 启动时一次 |
| `dm_config_set_relay(json)` | 保存 relay/token/设备名（字段可选） | 启动前 |
| `dm_start(relay, token, clip_cb, event_cb, user_data)` | 启动 Rust 运行时，立即返回句柄；内部 tokio + 自动重连 | 任意，立即返回 |
| `dm_stop(handle)` | 停止运行时（最多约 3s） | 任意 |
| `dm_send_clipboard_text(handle, text)` | 0 成功 / -1 句柄失效 / -2 文本非法 / -3 未连接（进离线队列） | 任意，快速 |
| `dm_pair_start(handle)` | 生成 8 位码 + `dsync-pair-v1:` 二维码串（阻塞 <30s） | **后台线程** |
| `dm_pair_join(handle, code_or_qr)` | 加入配对（阻塞 ≤130s） | **后台线程** |
| `dm_peers_json` / `dm_status_json` / `dm_catalog_json` | 设备列表 / 运行状态 / 软件目录 | **后台线程** |
| `dm_string_free(s)` / `dm_last_error()` | 释放 Rust 字符串 / 取最近错误 | 任意 |

事件：`{"type":"..."}`，列表见头文件末尾（`started`/`connected`/`presence`/`peer_online`/`peer_offline`/`session_established`/`session_closed`/`clipboard_captured`/`clipboard_sent`/`clipboard_applied`/`clipboard_spooled`/`catalog_*`/`peers_reloaded`/`pair_code`/`pair_succeeded`/`pair_failed`/`transfer_*`/`relay_error`/`error`）。
> 事件集合**只增不改**（v0.2.0 新增 `pair_peer` / `paused` / `peer_revoked` / `peer_unpaired` / `catalog_reloaded` / `spool_replayed` / `transfer_*` 等）；iOS 对未知 `type` 忽略并记一条日志，对配对/暂停/撤销/目录刷新等已知新事件已做展示或数据刷新处理。
> C ABI 另有 `dm_set_paused(handle, paused)`（暂停剪贴板同步、不影响连接与目录）——iOS 已封装但未调用：iOS 的剪贴板读写由 Swift 侧控制，暂停语义主要服务 Android JNI。

远端剪贴板文本另走 `dm_clipboard_cb(text, from_device_id, user_data)`；回调参数只在回调期间有效，Swift 侧立即拷贝。

> 头文件是 Android/iOS **共用契约**：如需改签名，须先与 Android 端同步并在 CI 两侧同时验证。

## 9. 无感同步能力边界与 iOS 平台限制（必读）

目标：**桌面/Android 上“复制即同步”的无感体验，在 iOS 上只能做到“前台自动 + 首次授权”**。这是平台约束，不是实现取舍。

### 9.1 能力矩阵

| 能力 | 桌面（Windows/Linux） | Android（本仓库 App / adb appops） | iOS（本 App） |
|---|---|---|---|
| 前台自动读取剪贴板并发送 | ✅ 监听/轮询 | ✅ 前台 Activity 有焦点 | ✅ 前台每 ~1.5s 轮询 `changeCount`，首次需系统粘贴授权 |
| 后台自动读取并发送 | ✅ 常驻进程 | ⚠️ 需 adb 授权/特殊路径，默认不可用 | ❌ 无 API（进程挂起；无后台读取能力） |
| 前台自动写入收到的内容 | ✅ | ✅ | ✅ 写入 `UIPasteboard`，无需授权 |
| 后台静默写入 | ✅ | ✅（系统限制逐渐收紧） | ❌ 不承诺；仅后台约 25 秒接收窗口内可能写入/通知 |
| 推送唤醒 | — | FCM（本轮未接） | ❌ APNs 未实现 |
| 系统提示 | 无 | Android 12+ 读取提示 | iOS 16+ 首次读取弹「允许粘贴？」 |

### 9.2 发送方向（本机复制 → 其他设备）

1. App 前台激活且已连接时，`AppModel` 每 ~1.5s 读取 `UIPasteboard.general.changeCount`（不触发授权的轻量属性）；
2. 变化后读取 `hasStrings` 与文本内容；**首次可能弹系统「允许粘贴？」**——用户选择「允许」后继续，选择「不允许」则退避 60 秒并提示手动兜底；
3. 文本哈希（FNV-1a 64）与「最近发送」比较，未变化不重复发送；
4. 命中「本 App 刚写入的远端内容」（120 秒时间窗）则抑制，**防止 A→B→A 回环**；
5. 调用 `dm_send_clipboard_text` → Rust 侧 E2E 加密后发给已配对在线设备（未连接时进离线队列重试）。

App 启动时若已有保存的 relay 配置，会自动连接（无需手动点「保存配置并连接」）；从后台回到前台时也会自动重连并立即检查一次剪贴板。用户仍可随时点「发送当前剪贴板（手动兜底）」，在自动读取被拦截（如用户在设置里选了“拒绝”）时恢复同步。

### 9.3 接收方向（其他设备 → 本机）

| App 状态 | 行为 |
|---|---|
| 前台激活 | 收到内容**立即自动写入** `UIPasteboard.general`，并 Toast/日志提示；同时记录哈希用于抑制回发 |
| 后台（约 25 秒接收窗口，`beginBackgroundTask`） | 保持连接接收 → 发**本地通知**（标题=来源设备，正文=内容摘要），内容挂起；回到 App 后自动写入 |
| 后台超过窗口 / App 被挂起或杀死 | 连接断开，无 APNs 推送 → **收不到**；待回到前台重连后由 Rust spool/会话补齐能补的部分 |
| 关闭「收到内容后自动写入」开关 | 只进「接收记录」，用户点「复制到剪贴板」才写入（适合敏感内容场景） |

通知权限在首次启动时请求；拒绝通知权限不影响前台自动写入，只影响后台窗口内的通知提醒。

### 9.4 仍不能做的事（如实写明）

1. **后台常驻连接 / 静默同步**：iOS 不允许，App 后台约 25 秒后主动 `dm_stop`（保留“希望连接”状态，回前台自动重连）。与 Android 的 `adb appops` 方案不同，iOS 没有对应授权路径。
2. **iOS 16+ 读取剪贴板的系统授权**：无法绕过；`UIPasteControl` 可做到“无弹窗粘贴”，但它只能作为按钮粘贴控件（需要用户点按该控件），不能用于自动轮询读取，后续可作为手动兜底的无弹窗版本。
3. **后台静默写剪贴板**：没有官方保证；本 App 只在前台（或极短后台窗口内）写入。
4. **无 APNs 推送**：本轮未实现，因此 App 不在前台时无法被远端内容唤醒。

### 9.5 其它平台限制

5. **只支持文本**：图片/文件不在本轮范围（Rust 侧图片类型只发 `clipboard_applied` 事件，UI 不处理）。
6. **配对撤销**：C ABI 本轮无 `dm_peers_remove`，App 内未提供删除配对入口；可用桌面端 `drive-sync peers remove <device_id>`（对端在线会收到撤销通知，离线则经由中继撤销信箱补达）。
7. **局域网中继**：填 `ws://192.168.x.x:...` 时 iOS 会弹「本地网络」权限（Info.plist 已配 `NSLocalNetworkUsageDescription`）；拒绝后无法连接局域网地址。
8. **TLS**：连接走 Rust 的 WebSocket 栈（不经过 URLSession，因此不受 ATS 约束）。`ws://` 可用于开发；`wss://` 建议使用受信任证书（如 Caddy/Let's Encrypt 自动签发），自签证书默认校验失败（当前未提供“忽略证书”开关）。
9. **剪贴板历史**：系统剪贴板可能被其他 App 覆盖，接收记录只在 App 内（最多 50 条，内存态，重启清空）。

## 10. 数据与隐私

- 数据目录：`<App 沙盒>/Library/Application Support/drive-sync/`（`identity.json`、`peers.json`、`config.json`、`spool/` 等，由 Rust 侧管理）；
- `access_token` 存 **Keychain**（`kSecAttrAccessibleAfterFirstUnlock`），relay 地址/设备名存 UserDefaults；
- 内容端到端加密后才发给中继，中继只见密文；日志不打印剪贴板明文（Rust 侧保证），App 事件日志只记设备 id、大小、错误；
- App 不在前台时收到内容的本地通知只展示**摘要**（单行、最多 100 字符），完整内容需打开 App 后自动写入剪贴板；可在系统设置中关闭通知；
- 删除 App 会删除身份与配对记录，需重新配对。

## 11. 故障排查

| 现象 | 原因/解决 |
|---|---|
| CI/本地报 `'drive_mobile.h' file not found` | 头文件未生成：确认 `crates/mobile-core/include/drive_mobile.h` 存在；`project.yml` 的 `HEADER_SEARCH_PATHS` 是否被改动 |
| 链接报 `Undefined symbols: _dm_start` | 没构建/没放静态库：运行 `scripts/build-ffi.sh`，确认 `ffi/iphonesimulator/libdrive_mobile.a`（或 `iphoneos/`）存在且架构匹配（`lipo -info`） |
| `building for iOS Simulator, but linking in object file built for iOS` | 用了真机 `.a`：`ffi/iphoneos` 与 `ffi/iphonesimulator` 放反了 |
| App 启动即崩溃（dyld） | 静态库 ABI 与头文件不一致：重新构建 `libdrive_mobile.a`，确认 `DM_ABI_VERSION` |
| 一直“连接中…” | relay 地址/端口/令牌错误；中继未启动（`GET /healthz` 应返回 200）；`ws://` 与 `wss://` 不匹配 |
| 发剪贴板无反应 | 未连接中继（按钮置灰）、剪贴板为空、或拒绝了系统粘贴授权。自动同步被拦截时界面会显示橙色提示，可点「发送当前剪贴板（手动兜底）」 |
| 自动同步不生效 | 确认「自动同步剪贴板（前台）」开关开启、App 在前台；iOS 16+ 需在系统弹窗允许粘贴，或到 设置 → DriveSync → 从其他 App 粘贴 改为允许 |
| 收到内容没有自动写入剪贴板 | 「收到内容后自动写入剪贴板」被关闭；或 App 不在前台（看通知/回前台后自动写入） |
| 通知没有出现 | 未授予通知权限（设置 → DriveSync → 通知）；前台自动写入不依赖通知 |
| 沙盒里没有 `drive-sync` 目录 | 首次启动 `dm_init` 才创建；检查 `AppModel.bootstrap()` 日志 |
| Xcode 签名报 bundle id 冲突 | 免费账号必须用自己的唯一 bundle id |

## 12. 后续工作（v0.3+）

- APNs（alert/background push）+ 拉取 → 解密 → 本地通知 → 用户点通知打开 App 复制（对应 [platform-mobile.md](platform-mobile.md) §3.2/§4.1）；
- `UIPasteControl` 免弹窗粘贴按钮；Share Extension「分享到 drive-sync」；
- `BGAppRefreshTask` 兜底刷新（系统调度，不承诺时延）；
- 配合 Rust 侧补充 `dm_peers_remove` / 文件传输 API，对齐 Android 能力；
- 图片/文件剪贴板、二维码扫码（`AVCaptureSession`）视为加分项。
