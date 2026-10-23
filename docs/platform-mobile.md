# drive-sync 移动端平台调研：剪贴板、推送保活、后台限制与 30 天加密暂存合规

> 文档版本：v0.1（调研稿） · 调研/核验日期：2026-10-23 · 适用：drive-sync Android / iOS 客户端
> 调研方式：**web_fetch 直抓官方原文 + web_search 交叉核验（2026-10-23）**。抓取来源包括 developer.android.com、firebase.google.com、developer.apple.com、support.apple.com、support.google.com、source.android.com、AOSP 源码、中国人大网/网信办、欧盟委员会；关键数字以官方原文为准，平台文档会随版本更新，合入 PRD 或排期前请按“参考来源”复核一次。

---

## 0. 结论先行（TL;DR）

1. **移动端不存在与桌面等价的“常驻后台连接 + 静默剪贴板双向同步”。** Android 受 Android 10 起的“仅默认输入法/焦点应用可读剪贴板”限制、Doze/App Standby、Android 14+ 前台服务类型与 Android 15 的 `dataSync` 6 小时/24 小时配额约束；iOS 受“进程挂起 + 后台推送不保证送达 + iOS 16 起程序化读剪贴板会弹用户授权框”约束。两端都应把**推送唤醒 → 拉取密文 → 本地解密 → 通知 → 用户确认写入剪贴板**作为主链路，而不是降级方案。
2. **Android：后台“读”剪贴板做不到（官方豁免只有默认输入法和当前有输入焦点的应用）；后台“写”剪贴板技术上可行**（AOSP 实现中写操作不要求焦点），因此 Android 接收方向可以做得接近“无感”，发送方向必须由用户手势触发。
3. **iOS：后台“读写”都不可靠，且读剪贴板有用户可见成本。** 程序化读取在 iOS 16+ 会弹系统授权框；用 `UIPasteControl` 的“粘贴”按钮可以无弹窗读取。接收方向只能靠用户点通知 → 打开 App → 一键复制；没有任何 API 能像系统 Universal Clipboard 那样把内容自动放进其他 App 的剪贴板。
4. **推送是“尽力而为”，不是保证。** FCM 高优先级消息可能被降级/代理（基于 7 天行为），被降级后**不能**启动前台服务；APNs 后台推送明确“不保证送达”，建议不超过 2–3 条/小时，超过 3 条/小时会触发系统限流。
5. **不要设计 24/7 WebSocket/长连接。** iOS 后台会被挂起，连接必然断；Android 上长期前台服务既受 Android 15 的 `dataSync` 6 小时/24 小时限制，也受 Google Play 的 FGS 政策约束（`dataSync` 用例必须是用户发起，视频不得展示全自动后台备份）。前台长连接可做，后台只做“短促的推送唤醒 + 一次 HTTPS 拉取”。
6. **Android 目标 API 级别已是硬约束**：自 2026-08-31 起，Google Play 新应用/更新必须 target Android 16（API 36），因此 Android 14/15 的前台服务、剪贴板等行为全部适用，不能靠低 targetSdk 规避。
7. **FCM 正处于 v1 → v2 迁移期**：**Android 官方文档**（Doze / 应用待机分桶页）已注明 v1 弃用并建议迁移 v2（`RemoteNotification` / `BackgroundSync`），而 Firebase FCM 侧的 `AndroidConfigV2` 仍标注为**预览版**。实现上建议：先用 v1 高优先级 data message + WorkManager 兜底，同时预留 v2 消息类型抽象。
8. **30 天云端加密暂存的最大合规风险不是“加密”，而是“元数据 + 留存期限 + 敏感内容”。** 建议 E2EE（服务端只存密文）、默认 24h/最长 30d 的硬 TTL、剪贴板敏感内容默认不入云、推送载荷不含明文，并准备 GDPR/PIPL 告知、删除、泄露通知与商店隐私披露。

### 0.1 能力矩阵（工程师速查）

| 能力 | Android | iOS | 结论 |
|---|---|---|---|
| 后台读剪贴板（无用户手势） | ❌ 仅默认 IME 或“当前有输入焦点”的应用；锁屏时读取被拒 | ❌ 无受支持 API；读取需前台/用户交互 | **不能设计后台采集** |
| 前台读剪贴板 | ✅ 应用有焦点即可；Android 12+ 系统会提示“XX 已从剪贴板粘贴” | ✅ 但 iOS 16+ 程序化读会弹“允许粘贴？”；`UIPasteControl` 无弹窗 | 发送方向靠用户手势 + 前台读取 |
| 后台写剪贴板 | ✅ 技术可行（写不要求焦点，AOSP 实现） | ⚠️ 无文档保证，不建议；可行路径是用户点按后在前台写 | Android 可“自动粘贴”，iOS 必须用户确认 |
| 用户可见通知 | ✅ 需 `POST_NOTIFICATIONS`（Android 13+ 运行时权限） | ✅ 需 `UNUserNotificationCenter` 授权（可用 provisional） | 通知是主 UX，不是附属品 |
| 静默推送到达保证 | ❌ FCM 尽力而为，高优先级可被降级/代理 | ❌ 后台推送“不保证送达”，会限流 | 必须有拉取兜底 |
| 常驻长连接 | ❌ 政策+功耗不允许；FGS 有类型/时长限制 | ❌ 进程挂起，连接断 | 只在前台保持连接 |
| 定时后台任务 | ✅ WorkManager，周期最小 15 分钟，不精确 | ✅ BGTaskScheduler，系统决定运行时机，不保证 | 作为兜底，不作为实时通道 |
| 锁屏状态下读剪贴板 | ❌ AOSP 读取检查 `isDeviceLocked` | ❌（无后台读取路径） | 锁屏时只做“拉取+通知” |
| 跨设备自动放入其他 App 剪贴板 | ⚠️ 仅接收方向可尝试（推送后写剪贴板） | ❌ 系统 Universal Clipboard 是特例（同 Apple ID + Handoff） | 不要承诺“双向无感” |

---

## 1. 调研范围与口径

- **覆盖**：Android 10→16 的剪贴板、Doze/App Standby、前台服务、WorkManager、FCM v1/v2、Google Play 政策；iOS 16+ 的 UIPasteboard/`UIPasteControl`、APNs、BGTaskScheduler、background URLSession、App Store 审核；30 天加密暂存的 GDPR/PIPL 注意点。
- **口径**：
  - “官方文档”指平台厂商（Google/Apple）与监管机构（EUR-Lex/欧盟委员会、中国人大网/网信办）的原文；AOSP 源码用于补充官方文档未写明的实现细节，已单独标注。
  - 文中数字、版本、API 名均来自所引页面；未标注来源的数字均为 drive-sync 的产品目标值（会明确写“目标值”）。
- **时效性注意（2026-10）**：
  - Google Play 自 **2026-08-31** 起要求新应用/更新 target **Android 16（API 36）**（Wear OS/Automotive/TV/XR 另有要求），见 [Play 目标 API 级别要求](https://support.google.com/googleplay/android-developer/answer/11926878)。
  - FCM 处于 v1→v2 迁移期：**Android 官方文档**（Doze / 应用待机分桶页面）已标注 v1 弃用、建议迁移 v2；Firebase 的 `AndroidConfigV2` 页面标注为**预览版**，见 [FCM AndroidConfigV2](https://firebase.google.com/docs/cloud-messaging/send/v2-api)。
  - 因此 Android 侧必须按“Android 14+ 前台服务类型 + Android 15 `dataSync` 超时 + Android 16 target”的现实约束设计。
- **复核记录（2026-10-23，web_search 交叉核验）**：
  - 已抽查 12+ 条关键结论（Android 10 剪贴板限制、Android 13 浮层与敏感标记、AOSP 1 小时自动清除、Android 14/15 前台服务、FCM 高优先级配额与降级、APNs 后台推送、iOS 16 粘贴提示、WorkManager 15 分钟、Play target API 36、Doze/待机分桶、Play 无障碍政策、iOS BGTask/后台 URLSession），与官方原文（含 AOSP 源码）一致；
  - **修正 1 处来源归属**：FCM v1 弃用的表述来自 **Android 官方文档**（Doze / 应用待机分桶页），不是 Firebase FCM 文档；Firebase 侧 v2 仍为预览版（详见 1 与 2.2.1）；
  - **补充来源**：AOSP Security enhancements、Android 13 全应用行为变更（FCM 配额）、前台服务超时专页、FGS 声明页、`PeriodicWorkRequest` API 参考、FCM Android 客户端（Google Play services 检查）；
  - **新增小节 6.1**：国内无 GMS 设备的推送方案（官方文档入口 + 架构建议），原“待决策”项仍未闭环。

---

## 2. Android

先给一张“版本 → 对本项目的影响”速查表（详细依据见各小节与参考来源）：

| Android 版本（API） | 与本项目相关的变化 |
|---|---|
| 10（29） | 后台读剪贴板受限（仅默认输入法/焦点应用）；分区存储 |
| 12（31） | 应用在后台不能启动前台服务（少数例外，含高优先级 FCM）；AOSP 实现中读取剪贴板会出现系统“已从剪贴板粘贴”提示 |
| 13（33） | 复制内容时系统显示确认界面（含预览），敏感内容需标记 `EXTRA_IS_SENSITIVE`；`POST_NOTIFICATIONS` 变为运行时权限 |
| 14（34） | 前台服务必须声明类型与对应权限，Play 需声明；后台创建“使用时权限”类 FGS 会抛异常 |
| 15（35） | `dataSync` FGS 限 6 小时/24 小时（所有同类服务共享配额）；`BOOT_COMPLETED` 不能启动 `dataSync`；`SYSTEM_ALERT_WINDOW` 豁免收窄 |
| 16（36） | Google Play 自 2026-08-31 起强制新应用/更新 target 该级别；其余行为以官方行为变更页为准 |

### 2.1 剪贴板到底能做什么、不能做什么

#### 2.1.1 读取：只有两种官方豁免

Android 10 起，官方《隐私权变更》明确：**除非应用是默认输入法（IME）或当前有输入焦点的应用，否则无法访问剪贴板数据**，见 [Android 10 隐私权变更（限制剪贴板访问）](https://developer.android.com/about/versions/10/privacy/changes)。AOSP 的官方《安全增强》文档给出了同样表述（Clipboard data 一节），见 [AOSP Security enhancements](https://source.android.com/docs/security/enhancements)。

API 文档给出了可观察到的行为（这一点对代码防御很重要）：

- `ClipboardManager.getPrimaryClip()`：**如果应用不是默认 IME 或没有输入焦点，返回 `null`**，见 [`ClipboardManager` 参考文档](https://developer.android.com/reference/android/content/ClipboardManager)。
- `getPrimaryClipDescription()` 同样返回 `null`；`hasPrimaryClip()` 返回 `false`。
- AOSP 实现进一步补充（官方文档未写、但对排期很关键）：
  - 读取操作（`getPrimaryClip`/`getPrimaryClipDescription`/`hasPrimaryClip` 等）在 `isDeviceLocked(userId, deviceId)` 为真时直接返回 `null`/`false`，即**设备锁屏时读不到剪贴板**；
  - 读取豁免包括：默认 IME、当前获得焦点的 UID、SystemUI/内容捕获/自动填充等特权场景；
  - 写入操作（`setPrimaryClip`/`clearPrimaryClip`）的检查中**没有**锁屏判断，且源码注释明确 “Writing is allowed without focus”。
  - 来源：[AOSP `ClipboardService.java`（main 分支）](https://android.googlesource.com/platform/frameworks/base/+/refs/heads/main/services/core/java/com/android/server/clipboard/ClipboardService.java)。

> 工程含义：`getPrimaryClip()` 返回 `null` 既可能是“剪贴板为空”，也可能是“无权限/锁屏”，**不要用 null 判断“没内容”**；需要结合 `hasPrimaryClip()`（同样受限制）与业务上下文区分，且前台读取失败要静默降级。

#### 2.1.2 系统对“读剪贴板”的用户可见提示（不要忽略）

- **Android 12+**：非剪贴板内容所有者、非 IME 的应用读取剪贴板时，系统会弹出提示（实现为 Toast，文案类似“XX 已从剪贴板粘贴”）。AOSP 的 `showAccessNotificationLocked()` 会按 UID+Clip 去重，默认受设备配置控制；该提示属于系统实现行为（官方开发者文档未单列，不同 OEM/版本可能不同）。来源：[AOSP `ClipboardService.java`](https://android.googlesource.com/platform/frameworks/base/+/refs/heads/main/services/core/java/com/android/server/clipboard/ClipboardService.java)。
  - **含义**：用户在前台用 drive-sync “读取剪贴板并发送”时，会看到系统的“drive-sync 已从剪贴板粘贴”提示——这是系统隐私特性，**不能也不应绕过**，产品文案要与之配合（例如按钮旁说明“将读取你刚复制的文本并发送”）。
- **Android 13+**：内容进入剪贴板时系统会显示**标准视觉确认界面**（含内容预览），应用不应再自己弹 Toast/Snackbar 重复提示；如需隐藏预览，必须给 `ClipDescription` 加敏感标记，见 [Android 复制和粘贴指南](https://developer.android.com/develop/ui/views/touch-and-input/copy-paste)。

#### 2.1.3 写入：可以后台写，但要么让用户知道，要么别做

- 官方文档没有限制应用在后台调用 `setPrimaryClip()`；AOSP 实现中写入不需要焦点，也不检查锁屏（见 2.1.1 的源码链接）。
- Android 13+ 当应用把内容加入剪贴板时，系统会显示**标准视觉确认界面（含内容预览）**，应用不应再自建 Toast/Snackbar 重复提示；如果同步的是密码、验证码、银行卡等敏感文本，必须在 `ClipDescription` 上加敏感标记（`ClipDescription.EXTRA_IS_SENSITIVE`，低版本 SDK 用字符串键 `android.content.extra.IS_SENSITIVE`），否则预览会泄露内容；官方要求**无论 target API 级别都必须加**，见 [复制和粘贴指南「将敏感内容添加到剪贴板」](https://developer.android.com/develop/ui/views/touch-and-input/copy-paste)。后台写入时用户不一定能看到该确认界面，产品需要额外用通知告知“已写入剪贴板”（不要假设系统提示一定可见）。
- 另外两个容易踩坑的点（AOSP 实现，OEM 可能改）：
  - **系统会自动清除剪贴板**：`ClipboardService` 中 `DEFAULT_CLIPBOARD_TIMEOUT_MILLIS = 3600000`（1 小时），自动清除默认开启。即“推送写入剪贴板 → 用户几小时后才去粘贴”大概率已经空了。**注意**：是否启用与具体时长由设备 `DeviceConfig` 控制（OEM/版本可能不同），且官方开发者文档未把“1 小时”写成契约，因此产品不要承诺“剪贴板长期保留”，需要历史记录请落到 App 内列表。
  - 后台写入从用户视角是“静默改剪贴板”，是明显的隐私敏感行为：**必须做成显式开关**（例如“收到内容后自动写入剪贴板”），默认关闭或首次启用时强提示。

#### 2.1.4 路径对比：哪些技术路径真的可用

| 路径 | 是否能读剪贴板 | 是否能监听用户在其他 App 的复制 | 结论 |
|---|---|---|---|
| 前台 Activity（有焦点） | ✅ | 仅应用在前台时 | **发送方向主路径**：用户点按后读取 |
| 默认输入法（IME） | ✅（官方豁免） | 仅输入法进程存活/键盘活跃时可读；进程会被系统回收 | 可选“高级模式”，不建议默认：开发量大、隐私敏感、商店审核与用户信任成本高 |
| AccessibilityService | ❌ 不在官方豁免列表内（官方豁免只有默认 IME 与焦点应用）；Play 政策严格限制其用途 | ❌ | **反模式，不采用**。Play 对无障碍 API 的声明/审核要求见 [Play AccessibilityService 政策](https://support.google.com/googleplay/android-developer/answer/10964491) |
| 通知操作按钮 | 通知动作可打开 Activity/服务，但读取仍受焦点/锁屏限制 | ❌ | 适合“打开 App 复制”，不适合后台读 |
| 分享面板（Sharesheet，`ACTION_SEND`） | 不需要读剪贴板，用户选中的文本直接传入 | ❌（但不需要） | **发送方向最佳降级路径**：用户从任意 App 分享文本到 drive-sync，见 [Android 分享简单数据](https://developer.android.com/training/sharing/send) |
| 快捷设置磁贴（TileService） | 点击磁贴可拉起 UI/前台服务 | ❌ | 快捷入口：一键“发送剪贴板”（需应用可见/有焦点后再读） |
| WorkManager 定时任务 | ❌ | ❌ | 只用于拉取/重试，不能读剪贴板 |
| 前台服务（FGS） | 不改变读取权限模型（不是 IME/焦点就读不到） | ❌ | 不能用来“后台读剪贴板”；见 2.2 的类型与政策限制 |

> 结论：**Android 没有受支持的“后台剪贴板监听”方案**。产品上必须接受“发送靠用户手势，接收可自动化”的非对称设计。

### 2.2 推送与保活：FCM、Doze、前台服务

#### 2.2.1 FCM 消息类型与优先级

- **消息类型**（v1 口径）：通知消息（notification）由 FCM SDK 自动展示；数据消息（data message）由应用自行处理；两者载荷上限均为 **4096 字节**（控制台发送通知文本限制 1000 字符），见 [FCM 消息类型](https://firebase.google.com/docs/cloud-messaging/customize-messages/set-message-type)。
- **优先级**（v1 口径）：
  - `normal`：设备未休眠时立即投递；Doze 下可能被延迟到维护窗口。
  - `high`：FCM 立即尝试投递，**必要时唤醒休眠设备并允许非常有限的网络访问**；应只用于“用户可见、时效性强”的内容。
  - 见 [设置和管理 Android 消息优先级](https://firebase.google.com/docs/cloud-messaging/android/message-priority)。
- **高优先级消息的处理窗口只有几秒**：`onMessageReceived()` 中系统只给“几秒”处理载荷，高优先级略长；官方明确不建议在其中做额外异步网络请求，需要的后续工作交给 WorkManager/前台服务。见同上页面。
- **高优先级可能被降级或代理（这是最关键的隐性配额）**：如果 FCM 检测到“高优先级消息没有产生用户可见通知”的模式（基于 **7 天**消息行为、按应用实例判断），会**降级为普通优先级**，或改由 **Google Play 服务代理展示通知**（HTTP v1 发送默认启用代理；需要 `firebase-messaging` 24.0.0+ 才能上报代理通知分析）。见 [消息优先级页面「在 Android 上降低高优先级 FCM 的优先级」](https://firebase.google.com/docs/cloud-messaging/android/message-priority)。
  - 被降级后的直接后果：后台启动前台服务会被拒绝（`ForegroundServiceStartNotAllowedException`），见 [从后台启动前台服务的限制](https://developer.android.com/develop/background-work/services/fgs/restrictions-bg-start)。因此代码里启动 FGS 前必须检查 `RemoteMessage.getPriority() == PRIORITY_HIGH`。
- **Android 13 的高优先级 FCM 配额行为变更（官方补充）**：App Standby 分桶**不再**决定应用可用多少条高优先级 FCM；系统会在检测到应用“持续发送不产生通知的高优先级消息”时降级这些消息；**超出配额的高优先级消息会被降为普通优先级**。官方建议：如果应用不总是针对高优先级 FCM 发通知，应把这类消息改为 `normal`，以免真正需要通知的消息被降级；启动 FGS 前检查 `RemoteMessage.getPriority()` 并捕获 `ForegroundServiceStartNotAllowedException`。见 [Android 13 行为变更（所有应用）：高优先级 FCM 配额](https://developer.android.com/about/versions/13/behavior-changes-all#fcm-quotas)。
- **FCM v2（预览版）语义**——迁移期必须知道：
  - `RemoteNotification`：面向“紧急、时效性、用户可见”的通知；**绕过低电耗模式立即投递**；GMS Core 可直接展示通知；若 `mutable_content: true` 则回调 `onNotificationReceived()`，必须在 **20 秒**内返回 `NotificationResponse`，否则回退展示通知载荷里的备份通知；后台时每个应用最多抑制 **25%** 的 `RemoteNotification` 流量，超过则强制展示备份通知。
  - `BackgroundSync`：面向非紧急后台同步（状态同步、刷新小组件等），**系统可能批量或延迟投递**，不得用于用户可见通知。
  - 见 [FCM AndroidConfigV2](https://firebase.google.com/docs/cloud-messaging/send/v2-api)。旧客户端会自动降级为 v1 广播（`onMessageReceived`），便于服务端先迁移。

#### 2.2.2 FCM 配额/速率限制（官方数字）

来自 [FCM 节流和配额](https://firebase.google.com/docs/cloud-messaging/throttling-and-quotas)：

| 限制项 | 数值 | 说明 |
|---|---|---|
| HTTP v1 项目级下行配额 | 默认 **600,000 条/分钟** | 超限返回 `429 RESOURCE_EXHAUSTED`；可申请提升（通常最多 +25%） |
| 单设备发送速率（Android） | **240 条/分钟**、**5,000 条/小时** | “防止发送逻辑错误耗尽设备电量”；不要长期贴着上限发 |
| 可折叠消息（collapse） | 每设备每应用突发 **20 条**，之后每 **3 分钟**补充 1 条 | 适合“同步状态”类消息；不可折叠消息适合必须逐条送达的内容 |
| 消息载荷 | **4 KB（4096 bytes）** | FCM 与 APNs 一致 |
| 项目级消息有效期 | 见 `ttl` 配置 | 官方推荐按需设置 |

> 注意：**官方没有公布“每天 N 条高优先级消息”的固定数字**。高优先级真正的“配额”是上面的**行为性降级机制**（7 天窗口）与代理机制，而不是一个可查询的计数器。工程上应把“高优先级 = 用户可见提醒”当作硬规则，把“数据同步”放 `normal`/`BackgroundSync`。

#### 2.2.3 Doze 与 App Standby

- **Doze（低电耗模式）**：设备未充电、静止、熄屏一段时间后进入；期间**暂停网络访问、忽略唤醒锁、推迟标准闹钟、禁止 JobScheduler/WorkManager、禁止同步**，只在“维护窗口”放行；维护窗口会越来越稀疏。见 [针对低电耗模式和应用待机模式进行优化](https://developer.android.com/training/monitoring-device-state/doze-standby)。
  - 官方核查清单明确：**能用 FCM 就不要自建长连接**（FCM 是与云端共享的单一持久连接，省电）；`setAndAllowWhileIdle()`/`setExactAndAllowWhileIdle()` 每个应用 **9 分钟最多 1 次**。
  - 详细的“设备状态 × 资源”表格（含 FCM 高/普通优先级在 Doze 下的行为）见 [电源管理资源限制](https://developer.android.com/topic/performance/power/power-details)：熄屏且 Doze 生效时，**高优先级 FCM 无执行数限制；普通优先级推迟到维护窗口**。
- **App Standby（应用待机分桶）**：分 `active / working set / frequent / rare / restricted` 等桶，桶决定作业与闹钟频率、网络访问；设备充电时不施加上限。见 [应用待机分桶](https://developer.android.com/topic/performance/appstandby)。
  - 低优先级桶：`rare` 桶会**限制互联网连接**；`restricted` 桶更严格：作业每天只能在 **10 分钟批处理会话**内运行一次、闹钟每天一次、加急作业更少；Android 13+ 用户 **8 天**未与应用交互可能进入 restricted（Android 12/12L 为 45 天）。
  - 官方明确：**不要试图影响自己被分到哪个桶**，而是保证在任意桶都能工作；每个 OEM 的算法可能不同。
- **Play 政策约束**：官方文档注明 —— Google Play 政策**禁止应用直接请求忽略电池优化（Doze/App Standby）**，除非应用核心功能确实受到不利影响，见 [Doze 页面「部分豁免的应用」](https://developer.android.com/training/monitoring-device-state/doze-standby)。因此不要引导用户“关闭电池优化”作为主方案。

#### 2.2.4 前台服务（FGS）：类型、启动限制、时长配额、商店审核

- **Android 12+ 后台启动限制**：应用在后台时**不能**启动 FGS，否则抛 `ForegroundServiceStartNotAllowedException`；例外包括“应用收到 **高优先级 FCM 消息**”、用户交互、精确闹钟等。见 [从后台启动前台服务的限制](https://developer.android.com/develop/background-work/services/fgs/restrictions-bg-start)。
  - **高优先级被降级后不再享有豁免**，这是 FCM 降级机制的连锁后果（同上页面明确写了这一点）。
  - Android 14+ 对“需要 `while-in-use` 权限的服务类型（camera/location/microphone）”更严格：应用在后台创建这类 FGS 会直接 `SecurityException`。
- **Android 14+ FGS 类型必填 + 权限 + Play 声明**：
  - 每个 FGS 必须在清单声明 `android:foregroundServiceType`，并声明对应的 `FOREGROUND_SERVICE_*` 权限，否则 `startForeground()` 抛 `MissingForegroundServiceTypeException`/`SecurityException`，见 [Android 14 前台服务类型必填](https://developer.android.com/about/versions/14/changes/fgs-types-required) 与 [声明前台服务并请求权限](https://developer.android.com/develop/background-work/services/fgs/declare)。
  - 在 Google Play 管理中心还必须**声明使用的前台服务类型与用例**（Android 14+ target），见 [Play 前台服务和全屏 intent 要求](https://support.google.com/googleplay/android-developer/answer/13392821)。
- **`dataSync` 类型的用户预算（Android 15+）**：
  - 以 Android 15（API 35）及以上为目标时，`dataSync` FGS 在**任意 24 小时内累计最多运行 6 小时**（该应用所有 `dataSync` 服务共享配额）；超时回调 `Service.onTimeout(int, int)`，只有几秒调用 `stopSelf()`，否则系统抛异常；配额耗尽后再启动会抛 `ForegroundServiceStartNotAllowedException`；**用户把应用切到前台会重置计时器**。见 [Android 15 行为变更](https://developer.android.com/about/versions/15/behavior-changes-15) 与官方专页 [前台服务超时（Foreground service timeouts）](https://developer.android.com/develop/background-work/services/fgs/timeout)。
  - Android 15 起，`BOOT_COMPLETED` 接收器**不能**启动 `dataSync` 类型 FGS。
  - Android 15 还收窄了 `SYSTEM_ALERT_WINDOW` 的启动豁免（必须有可见悬浮窗）。
- **`dataSync` 的 Play 用例口径（非常关键）**：Play 政策要求 `dataSync` 用例是**用户发起**的传输/备份/导入导出；提交视频“不得展示全自动执行或在后台进行的备份/恢复任务”。见 [Play 前台服务要求中的 TYPE_DATA_SYNC 说明](https://support.google.com/googleplay/android-developer/answer/13392821)。
  - **结论**：drive-sync **不能**用 `dataSync` FGS 来维持“服务器触发的自动同步/常驻连接”；`dataSync` 只能用于用户明确发起的一次传输（例如“立即同步”“发送大文件”）。后台自动同步应走 FCM + WorkManager/短连接，不启动 FGS。
  - `remoteMessaging` 类型的官方用途是“把短信从一台设备转移到另一台设备，保障换机时消息连续性”，见 [前台服务类型](https://developer.android.com/develop/background-work/services/fg-service-types)；**不建议**用它包装剪贴板同步（用途不符 + 需要 Play 声明审核）。
- **Android 8.0+ 通用后台限制**：后台服务在应用进入空闲后会被停止；多数隐式广播不能用清单注册；应改用 JobScheduler/WorkManager。见 [后台执行限制](https://developer.android.com/about/versions/oreo/background)。

#### 2.2.5 WorkManager（兜底与重试）

- 周期任务最小间隔 **15 分钟**（与 JobScheduler 一致），支持 flex 窗口；Doze 下 WorkManager 依赖的 JobScheduler 会被推迟，不保证准时。见 [WorkManager 定期工作](https://developer.android.com/develop/background-work/background-tasks/persistent/getting-started/define-work)；API 常量见 [PeriodicWorkRequest.MIN_PERIODIC_INTERVAL_MILLIS](https://developer.android.com/reference/androidx/work/PeriodicWorkRequest)。
- 高优先级 FCM 的 `onMessageReceived()` 之后立即调度**加急作业（expedited job）**可短暂豁免加急作业配额，用于在“几秒窗口”外完成少量后续工作；普通优先级则用普通 `WorkRequest`。见 [FCM 消息优先级：高优先级/普通优先级消息的消息处理](https://firebase.google.com/docs/cloud-messaging/android/message-priority)。
- 不要在 WorkManager 里做“读剪贴板”的幻想——它没有前台/IME 身份，读不到。

#### 2.2.6 通知权限与通知交互

- Android 13+ `POST_NOTIFICATIONS` 是运行时权限；新安装应用默认**通知关闭**，用户授权前无法发通知。前台服务仍必须提供通知，但用户拒绝通知权限时，FGS 通知只在任务管理器可见、通知抽屉不可见。见 [通知运行时权限](https://developer.android.com/develop/ui/views/notifications/notification-permission)。
- 通知操作按钮（Action）可以在不打开主界面的情况下把用户操作回传给应用，见 [创建通知](https://developer.android.com/develop/ui/views/notifications/build-notification)。但**通知 Action 不能读取剪贴板**（权限模型不变），只能做“打开 App 复制”“标记已读”等。

#### 2.2.7 厂商杀后台（非官方，但必须纳入风险）

AOSP 的 Doze/Standby 是下限，国产 ROM（小米/华为/OPPO/vivo/荣耀等）通常有更激进的省电与自启动管理，可能导致：推送延迟/不投递、WorkManager 不执行、后台进程被杀。厂商策略没有统一官方文档，社区数据可参考 [dontkillmyapp.com](https://dontkillmyapp.com/)（**第三方社区，非官方**）。工程上：
- 不要依赖“能保活”；把“回前台全量拉取”作为最终一致性的保证；
- 申请电池优化白名单只在核心功能确实受影响时使用，并遵守 Play 政策（见 2.2.3）；
- 在设置页提供“自检/诊断”入口，展示最近一次同步时间、推送状态、厂商限制提示。

### 2.3 Android 推荐实现路径（分层）

**发送方向（把本机复制的内容同步出去）**

| 层级 | 交互 | 实现 | 说明 |
|---|---|---|---|
| L1 主路径 | 用户选中文本 → 系统分享面板 → drive-sync | `ACTION_SEND` + 分享目标 | 不需要剪贴板权限，无系统弹窗，见 [分享简单数据](https://developer.android.com/training/sharing/send) |
| L2 快捷路径 | 用户打开 drive-sync → 点「发送剪贴板」 | 前台 Activity 有焦点 → `getPrimaryClip()` | 会触发 Android 12+ 系统“已从剪贴板粘贴”提示，属预期 |
| L3 快捷入口 | 快捷设置磁贴 / 长按图标快捷方式 / 通知 | `TileService` 或 Shortcut 拉起 L2 | 见 [快捷设置磁贴](https://developer.android.com/develop/ui/views/quicksettings-tiles) |
| L4 定时兜底 | 回前台时自动补同步 / 15 分钟周期任务 | 前台读取 + WorkManager | 只处理“应用内历史”，不能采集其他 App 的复制 |
| 可选高级 | 用户显式启用“输入法模式” | 自定义默认 IME 在键盘活跃时读取剪贴板 | 开发/审核/隐私成本高，默认不做 |

**接收方向（把云端内容送达本机）**

| 层级 | 触发 | 行为 |
|---|---|---|
| R1 | 高优先级 FCM data message（或 v2 `RemoteNotification` + `mutable_content`） | 拉取密文 → 解密 → 若用户开启“自动写入剪贴板”则 `setPrimaryClip`（敏感内容加 `EXTRA_IS_SENSITIVE`）→ 发本地通知“已就绪，可粘贴” |
| R2 | 通知被降级/未送达 | 用户回到 App 时立即拉取未读；WorkManager 周期（≥15 分钟）兜底 |
| R3 | 用户拒绝通知权限 | App 内红点/角标 + 打开即拉取；不依赖通知 |
| R4 | 设备锁屏 | 可写剪贴板（AOSP 不检查锁屏写），但读取在锁屏时被拒；通知需解锁后可见 |

---

## 3. iOS

### 3.1 剪贴板：能力、隐私提示与 `UIPasteControl`

- **iOS 16+ 程序化读剪贴板会弹系统授权框**：Apple 在 `UIPasteControl` 文档中明确 —— iOS 16 起，程序化粘贴（例如直接读 `UIPasteboard.general.string`）会弹出用户授权框，应用在获得允许前拿不到内容；使用 `UIPasteControl` 按钮则**不会弹框**。见 [UIPasteControl](https://developer.apple.com/documentation/uikit/uipastecontrol)（iOS 16.0+）。
- **iOS 14+ 起系统会告知用户“跨应用读取”**：如果应用读取的剪贴板内容来自其他 App 且系统未判定“用户意图”，系统会通知用户；用户意图由系统根据交互判断（如点按系统提供的控件、按 Command-V）。可以用 `hasStrings` 等类型检查属性与 `detectPatterns` 模式检测**在不触发通知的情况下**判断内容类型。见 [UIPasteboard](https://developer.apple.com/documentation/uikit/uipasteboard)。
  - 工程含义：判断“要不要显示粘贴按钮”时用 `hasStrings`/`canLoadObject`，不要为了探测而读取内容。
- **没有受支持的后台读取路径**：Apple 的剪贴板文档只描述用户复制/粘贴交互与用户意图判定，未提供任何后台/静默读取 API；不要设计后台采集。读取应发生在 App 前台、由用户手势触发（`UIPasteControl` 视为用户意图）。Apple 开发者论坛也有同类讨论（社区证据，非官方）：[Can't get UIPasteboard content in the background](https://developer.apple.com/forums/thread/706761)。
- **写入可行，但必须在用户可感知的上下文中**：`UIPasteboard` 支持写入（`string`/`setItems`），但把“后台静默改写剪贴板”做成默认行为既无文档保证也会引发用户不信任；推荐“用户点按复制 → 前台写入 `UIPasteboard.general`”。
- **Universal Clipboard 是系统能力，第三方无法复制**：同一 Apple Account 的设备、彼此靠近、蓝牙与 Wi-Fi 开启、Handoff 开启时，复制内容会自动出现在附近设备剪贴板，并“短暂保留，直到被新复制内容替换”。见 [Apple 支持：Use Universal Clipboard](https://support.apple.com/en-us/102430)。
  - **含义**：Apple 生态内跨设备剪贴板已由系统覆盖（同账号）；drive-sync 的价值在**跨账号、跨平台、历史记录、离线暂存**，不要试图在 iOS 上复制系统级“复制后自动出现在其他 App”的体验。
- **`UIPasteboard` 的其他可用能力**：写入时可设置 `expirationDate`/本地化选项（`setItems(_:options:)`），可排除 Handoff（`localOnly`）。见 [UIPasteboard](https://developer.apple.com/documentation/uikit/uipasteboard)。同步内容如需“30 天暂存”之外的本机保留，也应设置合理过期时间。

### 3.2 APNs：alert push 与 background push

| 维度 | alert push（用户可见通知） | background push（静默） |
|---|---|---|
| 载荷 | `aps.alert/sound/badge` + 自定义键 | `aps.content-available: 1`，不得带 alert/sound/badge |
| 必需请求头 | `apns-push-type: alert`，`apns-priority` 10（即时）或 5（考虑功耗） | `apns-push-type: background`，`apns-priority` **必须 5**（用 10 是错误） |
| 送达保证 | 尽力而为；APNs 可能重排、分组、节流、甚至不投递；离线时按 `apns-expiration` 存储（最长 30 天），且**每个 bundle ID 只存一条**（通常是最新一条） | 系统**不保证送达**；可能被节流；建议**不超过 2–3 条/小时**，超过 3 条/小时会被限流 |
| 应用状态 | 应用未启动也能由系统展示 | 唤醒应用（后台）→ 应用有 **30 秒**执行任务并调用 completion handler |
| 被抑制 | 用户可关闭通知 | 系统只保留最新一条；若应用被强杀/force quit，**持有的静默推送会被丢弃**；用户重新打开 App 时立即投递 |

来源：[APNs 请求头与推送类型](https://developer.apple.com/documentation/usernotifications/sending-notification-requests-to-apns)、[向你的 App 推送后台更新](https://developer.apple.com/documentation/usernotifications/pushing-background-updates-to-your-app)、[选择后台策略](https://developer.apple.com/documentation/backgroundtasks/choosing-background-strategies-for-your-app)。

补充要点：

- **`apns-priority` 取值**：`10` 立即发送；`5` 依据设备功耗情况发送；`1` 把功耗置于最高优先级并**阻止唤醒设备**（适合极低优先级）。见 [APNs 请求头](https://developer.apple.com/documentation/usernotifications/sending-notification-requests-to-apns)。
- **后台推送必须用 priority 5**：官方原文明确 “Always use priority `5`. Using priority `10` is an error.”，见 [APNs 推送类型说明](https://developer.apple.com/documentation/usernotifications/sending-notification-requests-to-apns)。
- **载荷上限**：普通远程通知 **4 KB（4096 bytes）**，VoIP 通知 5 KB。见 [生成远程通知](https://developer.apple.com/documentation/usernotifications/generating-a-remote-notification)。
- **不要在载荷里放敏感明文**：Apple 明确建议敏感数据加密后再放入载荷，可用通知服务扩展（Notification Service Extension）在设备端解密。见 [生成远程通知](https://developer.apple.com/documentation/usernotifications/generating-a-remote-notification)。这与 drive-sync 的 E2EE 设计一致。
- **通知权限**：必须调用 `UNUserNotificationCenter.requestAuthorization` 请求（可带 `.provisional` 进入“安静通知/通知中心历史”模式，用户再决定是否保留）；所有通知类型（含声音/角标）都受授权控制。见 [请求通知权限](https://developer.apple.com/documentation/usernotifications/asking-permission-to-use-notifications)。
- **通知操作**：用户点按通知按钮时，系统会在后台启动应用并回调 `userNotificationCenter(_:didReceive:withCompletionHandler:)`，可让用户在通知上直接完成“复制/打开”等轻量操作。见 [处理通知与通知相关操作](https://developer.apple.com/documentation/usernotifications/handling-notifications-and-notification-related-actions)。但**后台写剪贴板没有文档保证**，通知按钮应设计为“打开并复制”，而不是承诺后台完成。
- **时间敏感通知**：可用 `UNNotificationInterruptionLevel.timeSensitive` 提升打扰等级（需相应能力/授权），见 [UNNotificationInterruptionLevel](https://developer.apple.com/documentation/usernotifications/unnotificationinterruptionlevel)。是否启用取决于产品定位，建议默认普通等级。

### 3.3 后台执行：BGTaskScheduler、background URLSession、NSE

- **BGTaskScheduler**：应用可在后台运行任务的调度框架；`BGAppRefreshTask` 用于短时刷新（系统给最多 **30 秒**），`BGProcessingTask` 用于重活（系统选择低活动时段，可要求充电/联网）。**系统决定何时运行，`earliestBeginDate` 只是“不早于”，不保证准时。** 见 [BGTaskScheduler](https://developer.apple.com/documentation/backgroundtasks/bgtaskscheduler)、[选择后台策略](https://developer.apple.com/documentation/backgroundtasks/choosing-background-strategies-for-your-app)。
- **background URLSession**：适合“长耗时、非紧急”的下载/上传；传输由系统独立进程执行，App 被挂起甚至被系统终止也能继续；完成后再唤醒 App（`handleEventsForBackgroundURLSession`）。限制：只支持 HTTP/HTTPS、跟随重定向、上传只能从文件上传；系统对“后台任务启动频率”有 rate limiter，**每次被唤醒后马上又发起新下载会拉大延迟**，应复用少量后台 session。见 [在后台下载文件](https://developer.apple.com/documentation/foundation/downloading-files-in-the-background)。
- **Notification Service Extension（NSE）**：收到推送后可在展示前修改通知内容（解密、丰富文案），但运行时间有限；适合处理“加密的推送载荷”，不适合作为剪贴板读写通道。见 [选择后台策略](https://developer.apple.com/documentation/backgroundtasks/choosing-background-strategies-for-your-app)。
- **结论**：iOS 后台唯一可靠的“唤醒源”是推送（且不保证），其次是用户打开 App。没有常驻连接、没有后台剪贴板监听、没有准确定时器。

### 3.4 App Store 审核相关条款

- **2.5.4**：多任务应用只能将后台服务用于其预期用途（VoIP、音频播放、定位、任务完成、本地通知等）。见 [App Store Review Guidelines](https://developer.apple.com/app-store/review/guidelines/)。
  - 含义：不能以“保活/保持同步”为由滥用 `UIBackgroundModes`；后台推送必须用于内容更新（`content-available`），不能用来做“隐形常驻”。
- **2.4.2**：应用需高效用电，不得快速耗电、过度发热或做无关后台进程（如挖矿）。见同上。
- **4.2 Minimum Functionality**：应用需提供足够功能，不能只是营销/网页壳。见同上。
- **5.1.1 Data Collection and Storage**：必须有隐私政策；收集需用户同意；数据最小化；支持账号删除（若支持创建账号）。见 [App Store Review Guidelines 5.1.1](https://developer.apple.com/app-store/review/guidelines/)。
- **5.1.2 Data Use and Sharing**：不得未经许可使用/传输/共享个人数据；不得以“必须开启推送通知”作为使用功能的前提。见同上。
- **隐私标签**：App Store 的 App 隐私详情（App Privacy）需要如实申报数据收集（含“剪贴板”这类用户内容）。见 [App privacy details on the App Store](https://developer.apple.com/app-store/app-privacy-details/)。

### 3.5 iOS 推荐实现路径（含“类 Universal Clipboard”UX）

**发送方向（把本机内容同步出去）**

| 层级 | 交互 | 实现 | 备注 |
|---|---|---|---|
| L1 主路径 | 打开 drive-sync → 点“粘贴并发送” | `UIPasteControl`（iOS 16+）绑定按钮，无系统弹窗；或普通“粘贴”按钮（会弹一次授权框） | 见 [UIPasteControl](https://developer.apple.com/documentation/uikit/uipastecontrol) |
| L2 分享扩展 | 在任意 App 选中文本 → 分享 → drive-sync | Share Extension / `UIActivityViewController` | 不需要剪贴板权限，最顺滑，见 [UIActivityViewController](https://developer.apple.com/documentation/uikit/uiactivityviewcontroller) |
| L3 快捷指令 / App Intents | 用户运行“发送当前剪贴板”快捷指令（可从快捷指令 App、Siri、Action Button、背面轻点触发） | 暴露 App Intent；由 Shortcuts 读取剪贴板并调用上传 | 见 [App Intents](https://developer.apple.com/documentation/appintents)、[快捷指令使用手册](https://support.apple.com/guide/shortcuts/welcome/ios) |
| L4 兜底 | 打开 App 时提示“检测到新复制内容？”（仅当 App 前台且有用户手势时读取） | 前台读取（触发系统提示） | 不做后台探测 |

**接收方向（把云端内容送达本机）**

| 层级 | 通知形态 | 用户路径 |
|---|---|---|
| R1 | **alert push**（priority 10，`apns-push-type: alert`） | 用户点通知 → App 打开 → 显示内容 → 点「复制」写入 `UIPasteboard` → 用户切换到目标 App 粘贴 |
| R2 | **静默 push**（`content-available: 1`）→ 拉取 → 本地通知（`UNUserNotificationCenter`） | 系统可能在后台唤醒 App，30 秒内拉取并调度本地通知；用户点开 → 复制。适合不希望每次推送都消耗 APNs alert 预算时 |
| R3 | 通知 Action「复制」 | 系统后台唤起 App 并回调；**建议按钮文案改为“打开并复制”**，因为后台写剪贴板无文档保证 |
| R4 | 前台/回到 App | 打开即拉取未读；`BGAppRefreshTask` 兜底（系统决定） |
| 不可实现 | — | 复制后自动出现在其他 App 的剪贴板（仅系统 Universal Clipboard 支持，需同 Apple ID + Handoff） |

**快捷指令的定位**：iOS 的“快捷指令”由系统进程执行，用户运行“复制到剪贴板”动作属于明确的用户意图；drive-sync 可以暴露 `App Intents` 让 Shortcuts 取到“最新一条”再写入剪贴板。它适合高频用户（绑定 Action Button/背面轻点），但**不能作为默认路径**（配置成本高）。

---

## 4. 两端共同设计

### 4.1 推荐主链路（Push → Pull → Notify → Confirm → Clipboard）

```
桌面端(来源)               自托管中继(仅密文)             移动端(目标)
 复制/选中文本
   │ 本地加密(E2EE)
   └──► 上传密文(HTTPS) ──► 存储(≤30d TTL) ──► 推送(仅信封: itemId/长度/无明文)
                                                   │
                                                   ▼
                               ┌─────────────────────────────────────────┐
                               │ 高优先级 FCM / APNs alert 或 silent push │
                               └─────────────────────────────────────────┘
                                                   │ 唤醒(几秒~30秒)
                                                   ▼
                                         HTTPS 拉取密文(短连接、单次)
                                                   │ 本地解密(设备密钥)
                                                   ▼
                            ┌────────────────────────────────────────────┐
                            │ Android: 可选自动写剪贴板 + 本地通知        │
                            │ iOS: 本地/远程通知 → 用户打开 → 点「复制」  │
                            └────────────────────────────────────────────┘
                                                   │
                                        用户粘贴到目标 App（系统行为）
                                                   │
                                        ACK 消费状态回传(可选)
```

要点：

1. **推送载荷只放“信封”**（条目 ID、长度、是否有附件、来源设备），不放明文；FCM 连接加密但不是端到端加密，APNs 官方也建议敏感数据加密后再入载荷，见 [FCM 消息类型（E2EE 提示）](https://firebase.google.com/docs/cloud-messaging/customize-messages/set-message-type) 与 [FCM 加密](https://firebase.google.com/docs/cloud-messaging/encryption)。
2. **拉取必须短促**：后台唤醒只有几秒（Android）/30 秒（iOS），拉取一次小 JSON 即可，图片/大文本走已签名的临时 URL + background URLSession（iOS）/WorkManager（Android）。
3. **通知是产品界面**：Android 通知带“复制”“打开”动作；iOS 通知带“打开并复制”。通知被关闭时，App 内提供“未读条目”列表与红点。
4. **确认写剪贴板**：Android 可自动（用户开关控制），iOS 必须用户点按后在前台写入；两端都要对敏感内容做标记（Android `EXTRA_IS_SENSITIVE`；iOS 不做预览）。
5. **ACK/去重**：同一 itemId 幂等；本地保存最近 N 条历史（不依赖系统剪贴板，因为 Android 可能 1 小时自动清除、iOS 的 Universal Clipboard 也会很快被覆盖）。

### 4.2 传输通道选择

| 场景 | 推荐 | 理由/限制 |
|---|---|---|
| App 前台 | WebSocket（WSS）或 HTTP/2/3 长请求 | 前台保活没问题；但进入后台必须主动关闭；参考 Android FGS 政策与 iOS 进程挂起 |
| App 后台（Android） | FCM 唤醒 + 单次 HTTPS GET；重活用 WorkManager（加急作业豁免） | 高优先级消息只有几秒处理窗口；被降级则无豁免 |
| App 后台（iOS） | 静默 push/alert push + 单次 HTTPS 拉取；大文件用 background URLSession | 30 秒运行窗口；推送不保证送达 |
| 同局域网直连 | 可选（mDNS/Bonjour + TLS 直连），仅前台 | iOS 本地网络权限会弹授权；Android 也受前台限制；不解决跨网场景，作为“服务器不在同一网络/离线”补充 |
| QUIC | 不推荐自研；优先 HTTPS（HTTP/3 由系统栈支持） | 自研 QUIC 在移动端收益小、后台同样受限；维护成本高 |

**明确否定的设计**：
- ❌ 移动端常驻 WebSocket/自建长连接“实时同步”（两端都不允许/不经济）。
- ❌ 用 Android `dataSync` 前台服务维持“服务器触发的自动同步”（Play 政策不允许，且 Android 15 有 6h/24h 上限）。
- ❌ 用 iOS `BGTaskScheduler` 做“准确定时同步”（系统决定运行时机，不保证）。
- ❌ 用 APNs/FCM 推明文剪贴板内容（合规与隐私风险）。

### 4.3 功耗预算（产品目标值，非平台文档）

| 指标 | 目标值 | 说明 |
|---|---|---|
| 推送频率上限 | ≤ **3 条/小时/设备**；文本合并窗口 1–2 秒 | 对齐 Apple 对后台推送的建议；Android 同样避免高优先级刷屏 |
| 后台唤醒 | ≤ 30 次/天/设备（前台主动同步不计） | 每次唤醒只做“拉取 + 解密 + 通知” |
| 单次后台流量 | 信封 ≤ 4 KB；文本条目 ≤ 64 KB | 大文件不在剪贴板链路内 |
| Android FGS 使用 | 仅用户发起的一次性传输；目标 ≤ 30 分钟/24 小时（远低于系统 6 小时配额） | 配额是“上限”，不是目标 |
| iOS 后台执行 | 常规只依赖推送与 `BGAppRefreshTask`；`BGProcessingTask` 仅用于历史清理 | 系统调度，产品不承诺时延 |
| 长连接 | 仅前台；切后台 5 秒内关闭 | 避免“后台耗电”口碑问题 |
| 重试 | 指数退避 + 抖动；后台失败不立即重试，等下一次推送/前台 | 避免触发 FCM/APNs 限流 |

> 这些是 drive-sync 的自定目标，平台没有对应的“标准值”；需要用真机功耗测试（Android Battery Historian / iOS Energy Log）校验。

### 4.4 权限清单

**Android（`AndroidManifest.xml`）**

| 权限/配置 | 必要性 | 说明 |
|---|---|---|
| `INTERNET` | 必须 | 网络 |
| `ACCESS_NETWORK_STATE` | 建议 | 网络状态/重试策略 |
| `POST_NOTIFICATIONS` | Android 13+ 必须（若要通知） | 运行时权限，见 [通知权限](https://developer.android.com/develop/ui/views/notifications/notification-permission) |
| `FOREGROUND_SERVICE` + `FOREGROUND_SERVICE_DATA_SYNC` | 仅当使用 FGS | Android 14+ 类型必填；Play 需声明，见 [FGS 类型](https://developer.android.com/about/versions/14/changes/fgs-types-required)、[Play FGS 要求](https://support.google.com/googleplay/android-developer/answer/13392821) |
| `RECEIVE_BOOT_COMPLETED` | 可选 | Android 15+ 不能从 BOOT_COMPLETED 启动 `dataSync` FGS |
| `WAKE_LOCK` | 可选 | 让 WorkManager 管理，不要自己长期持有 |
| `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` | **不建议** | Play 政策限制，见 2.2.3 |
| `SCHEDULE_EXACT_ALARM` / `USE_EXACT_ALARM` | 不需要 | 剪贴板同步不需要精确闹钟 |

> 注意：**Android 没有可申请的“剪贴板读取权限”**。`READ_CLIPBOARD_IN_BACKGROUND` 是签名/特权权限（AOSP 实现中仅授予 shell 等），普通应用无法获得，见 [AOSP ClipboardService](https://android.googlesource.com/platform/frameworks/base/+/refs/heads/main/services/core/java/com/android/server/clipboard/ClipboardService.java)。

**iOS（Info.plist / Capabilities）**

| 配置 | 必要性 | 说明 |
|---|---|---|
| Push Notifications capability + `aps-environment` | 必须（若要推送） | APNs |
| `UIBackgroundModes: remote-notification` | 必须（后台静默推送） | 见 [推送后台更新](https://developer.apple.com/documentation/usernotifications/pushing-background-updates-to-your-app) |
| `UIBackgroundModes: fetch` / `processing` | 可选 | BGAppRefresh/BGProcessing |
| 通知授权（运行时） | 必须（若要通知） | `UNUserNotificationCenter.requestAuthorization` |
| App Group（主 App 与 Share Extension） | 可选 | 分享扩展/密钥共享 |
| 本地网络权限（Bonjour 直连时） | 可选 | 局域网直连才会触发系统授权 |

> iOS 同样**没有**“剪贴板权限”可申请：粘贴授权由系统在读取时弹窗，或使用 `UIPasteControl` 规避弹窗；无法预先申请。

### 4.5 最低系统版本建议

| 平台 | 建议 | 理由 |
|---|---|---|
| Android `minSdk` | **29（Android 10）** | Android 10 起剪贴板限制生效，低版本行为不同（旧版本可能可后台读，但 Play target 要求 + 用户占比已无收益）；同时覆盖 WorkManager、Keystore、通知渠道等基线能力 |
| Android `targetSdk` | **36（Android 16）** | Google Play 自 2026-08-31 起强制，见 [Play 目标 API 要求](https://support.google.com/googleplay/android-developer/answer/11926878)；因此必须处理 Android 14/15/16 的 FGS/剪贴板/通知行为 |
| iOS Deployment Target | **16.0** | `UIPasteControl` 与 iOS 16 粘贴授权弹窗均从 iOS 16 开始；iOS 15 没有 `UIPasteControl`，且“粘贴隐私提示”行为不同，维护两套 UX 不划算；如需覆盖 iOS 15，建议只提供“分享扩展 + 手动粘贴”降级模式 |
| 测试基线 | Android 14/15/16 各一台 + 国产 ROM（小米/华为/OPPO/vivo）各一台（如有国内用户）；iOS 16/17/26 真机 | OEM 行为差异必须真机验证 |

---

## 5. “云端 30 天加密暂存”在移动端的合规/隐私注意点

> 本节是工程视角的合规检查表，**不构成法律意见**；涉及具体司法辖区、数据出境、员工/团队场景时请法务确认。

### 5.1 设计护栏（先做对，再谈合规）

1. **E2EE 是基础**：服务端只存密文与最小元数据；密钥在设备生成、放 Android Keystore / iOS Keychain（Secure Enclave 保护），不上传。推送载荷、日志、分析事件都不得含明文。
2. **TTL 硬限制**：默认 24 小时、最长 **30 天**；服务端定时任务强制删除（不是“依赖客户端删除”）；用户可在任意端立即删除（含服务端 + 本地缓存）。
3. **敏感内容默认不入云**：检测到密码/验证码/身份证/银行卡等模式时，默认仅本地保留或要求用户确认；Android 写入剪贴板必须带 `EXTRA_IS_SENSITIVE`；iOS 不展示预览。
4. **元数据最小化**：只保留必要字段（条目 ID、大小、时间戳、设备标识的假名化 ID）；IP 日志短周期保留并脱敏；不做内容分析/广告用途。
5. **日志与崩溃报告**：不记录剪贴板内容；崩溃栈采样前过滤 `UIPasteboard`/`ClipData` 变量；分析 SDK 默认关闭或仅上报聚合指标（如“同步成功率”）。
6. **密钥与账号生命周期**：退出登录/删除账号时销毁本地密钥并让服务端密文不可解密（密钥派生自设备密钥 + 账号材料）；换机需要显式恢复流程。
7. **团队/多人场景**：一旦为他人提供服务（团队共享空间），自建者就从“个人自用”变成“个人信息处理者”，义务显著增加（告知、同意、删除、泄露通知、跨境评估）。

### 5.2 GDPR / 《个人信息保护法》简表

| 合规点 | GDPR（参考条款） | 中国《个人信息保护法》（参考条款） | drive-sync 产品要求 |
|---|---|---|---|
| 目的限制/最小必要 | Art. 5(1)(b)(c) | 第 6 条：明确、合理目的，与处理目的直接相关，影响最小；限于最小范围 | 只同步用户主动选择的内容；默认不采集剪贴板历史；功能开关默认保守 |
| 存储期限限制 | Art. 5(1)(e) | 第 19 条：保存期限应为实现处理目的所必要的最短时间 | 默认 24h、最长 30d，硬 TTL + 到期删除；提供“立即删除” |
| 告知义务 | Art. 13/14 | 第 17 条：处理前以显著方式、清晰易懂语言告知处理者、目的、方式、种类、保存期限、行使权利方式等 | 首次使用隐私弹窗 + 隐私政策；说明“剪贴板内容会加密上传（如开启）”“保存期限 30 天” |
| 合法性基础/同意 | Art. 6 | 第 13 条（含同意等情形） | 同步/云端暂存必须明确同意、可撤回；撤回后停止并删除 |
| 敏感个人信息 | Art. 9（仅限特殊类别，如健康/生物识别等，需额外条件；金融账户、身份证件等通常属普通个人数据，但应按高风险处理） | 第 28 条定义（含金融账户等）、第 29 条单独同意、第 30 条告知必要性与影响 | 剪贴板可能含金融账户/身份证/密码等敏感信息 → 敏感内容默认本地、需单独同意才上云 |
| 删除权/被遗忘 | Art. 17 | 第 47 条：法定情形下应主动删除，个人有权请求删除 | 客户端“删除单条/全部/账号”一键完成；服务端同步删除密文与索引 |
| 安全措施 | Art. 32 | 第 51 条：加密、去标识化、权限管理、应急预案等 | E2EE + TLS + 最小权限 + 密钥轮换 + 访问审计（不含内容） |
| 数据泄露通知 | Art. 33/34 | 第 57 条：立即补救并通知监管部门和个人 | 即便只泄露密文/元数据也要有预案；提供安全事件公告渠道 |
| 数据出境 | Art. 44–49（第五章） | 第 38 条（出境条件）、第 40 条（CII/达到数量阈值须境内存储+安全评估） | 服务端部署位置要写清楚；如服务器在境外/团队跨区，需评估出境路径 |
| 影响评估 | Art. 35（DPIA） | 第 55 条（事前个人信息保护影响评估，含敏感信息、自动化决策、跨境等情形） | 处理敏感剪贴板内容或跨境时，自建者也应做简版影响评估并留档 |
| 未成年人 | Art. 8 | 第 31 条：不满 14 周岁需监护人同意 | 产品定位不做未成年人场景；如涉及需额外流程 |

**官方链接**：

- GDPR 全文（欧盟官方公报）：[EUR-Lex Regulation (EU) 2016/679](https://eur-lex.europa.eu/eli/reg/2016/679/oj)；欧盟委员会数据保护专题：[European Commission – Data protection](https://commission.europa.eu/law/law-topic/data-protection_en)；欧洲数据保护委员会：[EDPB](https://www.edpb.europa.eu/)。
- 《中华人民共和国个人信息保护法》全文（中国人大网转载/网信办）：[《个人信息保护法》](https://www.cac.gov.cn/2021-08/20/c_1631050028355286.htm)。

### 5.3 应用商店的隐私披露

- **Google Play Data Safety**：需申报“收集/共享的数据类型、是否加密传输、是否可删除”。剪贴板文本属于用户内容，必须如实申报；见 [Play Data safety](https://support.google.com/googleplay/android-developer/answer/10787469)。
- **Apple App Privacy**：需申报收集的数据类型与用途；剪贴板内容若上传服务器，属于“用户内容”，必须披露；见 [App privacy details](https://developer.apple.com/app-store/app-privacy-details/)。
- **账号删除**：App Store 5.1.1(v) 要求支持创建账号的应用提供 App 内删除账号；Play 也有对应要求，见 [App Store Review Guidelines](https://developer.apple.com/app-store/review/guidelines/)。

### 5.4 产品默认值建议

| 项 | 建议默认 | 理由 |
|---|---|---|
| 云端暂存 | 关闭（仅中转） | 最符合“云端只中转不存储”的产品定位 |
| 开启暂存后的 TTL | 24 小时，最大 30 天 | 存储期限最小化 |
| 敏感内容 | 默认不入云；本地保留 | PIPL 单独同意成本高，默认规避 |
| 自动写入剪贴板（Android） | 关闭 | 后台改剪贴板是强隐私行为，需显式开启 |
| 推送内容 | 仅信封，无明文 | FCM 非 E2EE、APNs 载荷可见 |
| 分析/崩溃 | 默认关闭内容采集 | 避免二次泄露 |

---

## 6. 风险清单与开放问题

| # | 风险/问题 | 影响 | 缓解 |
|---|---|---|---|
| 1 | **最大平台风险：iOS 后台不可达 + 剪贴板必须用户确认** | 无法做到“复制即同步、无感粘贴”；与 Apple 生态内 Universal Clipboard 的体验差距会被用户直接感知 | 产品明确“跨平台/跨账号/历史”定位；iOS 侧把“通知 → 打开 → 复制”做成 2 步内完成；提供快捷指令/分享扩展 |
| 2 | **推送不保证送达**（FCM 降级/代理、APNs 静默推送限流、用户关通知） | 同步延迟/漏同步 | 前台/回前台全量拉取 + WorkManager/BGTask 兜底 + 未读列表 |
| 3 | **Android 厂商杀后台/自启动限制** | 推送/任务不执行，用户投诉“不同步” | 设置页诊断、引导白名单（合规前提下）、回前台补同步；国内无 GMS 设备 FCM 不可用，厂商推送方案见 6.1（**仍待决策**） |
| 4 | **Android `dataSync` FGS 的 Play 审核** | 审核被拒/下架 | 不拿 FGS 做常驻同步；只在用户发起的一次性传输使用，并按 Play 要求录制声明视频 |
| 5 | **PIPL/GDPR 敏感信息与跨境** | 合规风险 | 默认关闭云暂存、敏感内容本地化、TTL、出境评估、隐私政策 |
| 6 | **剪贴板自动清除（Android 约 1 小时）** | 用户“推送后过一会粘贴”拿到空内容 | 不依赖系统剪贴板做历史；App 内保留条目并提示“剪贴板可能已被系统清除，请从历史复制” |
| 7 | **FCM v1 弃用 / v2 预览** | 未来需迁移，服务端/客户端接口变化 | 抽象“推送信封”协议，服务端可切换 v1/v2；关注 v2 GA 时间 |
| 8 | **iOS 静默推送被强杀后丢弃** | 用户 force quit 后收不到同步 | 打开 App 全量拉取；提示“请勿强制退出，否则后台同步可能停止” |
| 9 | **Android 12+ “已从剪贴板粘贴”提示、iOS 粘贴授权弹窗** | 用户误以为应用在偷看剪贴板 | 文案解释 + 不自建后台读取 + 只在用户点按时读取 |
| 10 | **未真机验证项** | 实现风险 | 需真机验证：OEM ROM 后台写剪贴板/锁屏写剪贴板、iOS 通知 Action 后台写剪贴板、Android 16 新限制、FCM v2 行为；国内厂商推送已做文档调研（见 6.1），配额/上架要求仍需控制台核对 |

### 6.1 国内无 GMS 设备的推送方案（2026-10-23 补充调研）

**为什么必须单独考虑**：FCM 的 Android 客户端依赖 Google Play services；官方文档要求应用在访问 FCM/Google Play services 功能前检查设备是否装有兼容的 Google Play services APK（无 GMS 时可引导用户安装，但国内多数设备不可行），见 [FCM Android 客户端设置（Check for Google Play services）](https://firebase.google.com/docs/cloud-messaging/android/client)。因此“国内无 GMS 设备上 FCM 不可用”是现实约束，不是配置问题。

**厂商推送现状（均有官方推送平台/文档入口）**：

| 厂商/通道 | 官方入口 | 覆盖范围（以厂商为准） | 关键事实（据官方文档/页面） |
|---|---|---|---|
| 小米 MiPush | [小米推送文档](https://dev.mi.com/xiaomihyperos/documentation/detail?pId=1533) | 小米/Redmi（MIUI/HyperOS） | 官方文档明确“通知栏消息通过小米系统通道下发，不需要应用驻留后台”；MIUI 上的长连接由系统维护；需在小米开放平台注册开发者、创建应用并启用推送；客户端集成 SDK，服务端用 HTTP API/Java SDK 发送；RegID 按设备+应用生成（30 天未与推送服务建立连接会判定失效） |
| 华为 Push Kit（HMS Core） | [Huawei Push Kit](https://developer.huawei.com/consumer/en/hms/huawei-pushkit) | 华为 HMS Core 设备；鸿蒙原生应用另有 [HarmonyOS Push Kit 指南](https://developer.huawei.com/consumer/cn/doc/harmonyos-guides/push-gettingstart) | 厂商级推送通道，需 HMS Core + AppGallery Connect 配置；服务端 REST API 发送 |
| OPPO PUSH | [OPPO 开放平台推送文档](https://open.oppomobile.com/documentation/page/info?id=11207) | OPPO/一加/realme 等 ColorOS 设备（具体以厂商为准） | 需在 OPPO 开放平台注册并集成 SDK；服务端 API 发送 |
| vivo PUSH | [vivo 开放平台推送文档](https://dev.vivo.com.cn/documentCenter/doc/541) | vivo/iQOO 设备（具体以厂商为准） | 需在 vivo 开放平台注册并集成 SDK；服务端 API 发送 |

> 说明：小米文档本次抓取到完整正文；华为/OPPO/vivo 的文档页多为 JS 渲染、部分内容需登录控制台查看。表中“需注册/集成/服务端 API 发送”为各平台通用接入模式，具体字段、配额、消息分类与审核要求需在各自控制台核对（本次未逐条核验）。

**架构建议**：

1. 客户端/服务端都抽象 `PushProvider`：以 `{ provider, token, deviceId }` 三元组持久化；客户端按 `Build.MANUFACTURER`/品牌选择 SDK 注册，服务端按 provider 路由到对应 REST API。FCM token 与厂商 token 不能互换，必须分开存储并做失效清理。
2. 厂商通道只负责“唤醒”，载荷仍只放信封（与 4.1 主链路一致）；拉取仍走 drive-sync 自建 HTTPS，保持 E2EE。
3. 覆盖策略：个人/小团队自建通常只有 1–2 个品牌设备，建议**先做目标用户最常用的 1–2 家 + 前台/WorkManager 兜底**，不要一开始铺满 4 家；产品说明中如实写“实时性取决于系统推送，后台可能延迟”。
4. 不建议引入第三方统一推送云（个推/极光等）：会引入新的第三方数据处理者，与“自托管、云端只中转”的定位冲突；如未来支持，应作为可选插件并做隐私披露。
5. 待确认（仍未闭环）：
   - 各厂商对“自建分发/侧载（非应用商店上架）应用”是否开放推送、是否需要企业开发者认证；
   - 各厂商推送的配额、消息分类（系统消息/营销消息）与限流规则；
   - 荣耀、鸿蒙 NEXT 等品牌/系统（HarmonyOS NEXT 不运行 Android APK）是否需要独立适配；
   - 是否为国内场景维护 4 套 SDK + 4 套服务端适配（成本 vs 收益）。

---

## 7. 参考来源（全部为官方或官方源码；按类别，含 2026-10-23 复核补充）

### Android / Google Play

1. [Android 10 隐私权变更（限制剪贴板访问）](https://developer.android.com/about/versions/10/privacy/changes)
2. [`ClipboardManager` API 参考](https://developer.android.com/reference/android/content/ClipboardManager)
3. [`ClipDescription` API 参考](https://developer.android.com/reference/android/content/ClipDescription)
4. [Android 复制和粘贴指南（含 Android 13 确认界面与敏感标记）](https://developer.android.com/develop/ui/views/touch-and-input/copy-paste)
5. [从后台启动前台服务的限制](https://developer.android.com/develop/background-work/services/fgs/restrictions-bg-start)
6. [前台服务类型](https://developer.android.com/develop/background-work/services/fg-service-types)
7. [Android 14 前台服务类型必填](https://developer.android.com/about/versions/14/changes/fgs-types-required)
8. [Android 15 行为变更（dataSync 6 小时超时等）](https://developer.android.com/about/versions/15/behavior-changes-15)
9. [Android 16 行为变更](https://developer.android.com/about/versions/16/behavior-changes-16)
10. [低电耗模式和应用待机模式优化](https://developer.android.com/training/monitoring-device-state/doze-standby)
11. [应用待机分桶](https://developer.android.com/topic/performance/appstandby)
12. [电源管理资源限制（设备状态 × FCM 行为表）](https://developer.android.com/topic/performance/power/power-details)
13. [WorkManager 入门](https://developer.android.com/develop/background-work/background-tasks/persistent/getting-started)
14. [WorkManager 定期工作（最小 15 分钟）](https://developer.android.com/develop/background-work/background-tasks/persistent/getting-started/define-work)
15. [Android 8.0 后台执行限制](https://developer.android.com/about/versions/oreo/background)
16. [通知运行时权限（Android 13+）](https://developer.android.com/develop/ui/views/notifications/notification-permission)
17. [创建通知（含 Action）](https://developer.android.com/develop/ui/views/notifications/build-notification)
18. [从其他应用接收简单数据（Sharesheet）](https://developer.android.com/training/sharing/send)
19. [快捷设置磁贴](https://developer.android.com/develop/ui/views/quicksettings-tiles)
20. [无障碍服务（AccessibilityService）开发指南](https://developer.android.com/guide/topics/ui/accessibility/service)
21. [Google Play：目标 API 级别要求（2026-08-31 起 API 36）](https://support.google.com/googleplay/android-developer/answer/11926878)
22. [Google Play：前台服务和全屏 intent 要求（FGS 声明、TYPE_DATA_SYNC 用例）](https://support.google.com/googleplay/android-developer/answer/13392821)
23. [Google Play：AccessibilityService API 政策](https://support.google.com/googleplay/android-developer/answer/10964491)
24. [Google Play：Data safety](https://support.google.com/googleplay/android-developer/answer/10787469)
25. [AOSP `ClipboardService.java`（锁屏拒绝读取、写不要求焦点、1 小时自动清除、粘贴提示）](https://android.googlesource.com/platform/frameworks/base/+/refs/heads/main/services/core/java/com/android/server/clipboard/ClipboardService.java)

### FCM

26. [设置和管理 Android 消息优先级（含降级/代理机制）](https://firebase.google.com/docs/cloud-messaging/android/message-priority)
27. [FCM 节流和配额（项目 60 万/分钟、单设备 240/分钟与 5000/小时、可折叠 20 条）](https://firebase.google.com/docs/cloud-messaging/throttling-and-quotas)
28. [FCM 消息类型（通知/数据，4 KB 上限，非端到端加密）](https://firebase.google.com/docs/cloud-messaging/customize-messages/set-message-type)
29. [FCM AndroidConfigV2（RemoteNotification / BackgroundSync，20 秒回调、25% 抑制上限；预览版）](https://firebase.google.com/docs/cloud-messaging/send/v2-api)
30. [FCM 接收消息](https://firebase.google.com/docs/cloud-messaging/receive-messages)
31. [FCM 端到端加密说明](https://firebase.google.com/docs/cloud-messaging/encryption)
32. [FCM 了解消息传送](https://firebase.google.com/docs/cloud-messaging/understand-delivery)

### iOS / Apple

33. [UIPasteboard（用户意图、模式检测、Handoff/过期）](https://developer.apple.com/documentation/uikit/uipasteboard)
34. [UIPasteControl（iOS 16 程序化粘贴弹窗；按钮无弹窗）](https://developer.apple.com/documentation/uikit/uipastecontrol)
35. [Apple 支持：Use Universal Clipboard（同 Apple Account/蓝牙/Wi-Fi/Handoff）](https://support.apple.com/en-us/102430)
36. [APNs 请求头与推送类型（priority 10/5/1、background 必须 5、存储 30 天、每 bundle 只存一条）](https://developer.apple.com/documentation/usernotifications/sending-notification-requests-to-apns)
37. [向 App 推送后台更新（不保证送达、2–3 条/小时、30 秒、强制退出丢弃）](https://developer.apple.com/documentation/usernotifications/pushing-background-updates-to-your-app)
38. [生成远程通知（4 KB / VoIP 5 KB、敏感载荷加密建议）](https://developer.apple.com/documentation/usernotifications/generating-a-remote-notification)
39. [处理通知与通知相关操作（Action 后台回调）](https://developer.apple.com/documentation/usernotifications/handling-notifications-and-notification-related-actions)
40. [请求通知权限（含 provisional）](https://developer.apple.com/documentation/usernotifications/asking-permission-to-use-notifications)
41. [UNNotificationInterruptionLevel](https://developer.apple.com/documentation/usernotifications/unnotificationinterruptionlevel)
42. [选择后台策略（BGTask/BGProcessing/background push/NSE）](https://developer.apple.com/documentation/backgroundtasks/choosing-background-strategies-for-your-app)
43. [BGTaskScheduler](https://developer.apple.com/documentation/backgroundtasks/bgtaskscheduler)
44. [在后台下载文件（background URLSession 限制与 rate limiter）](https://developer.apple.com/documentation/foundation/downloading-files-in-the-background)
45. [App Intents](https://developer.apple.com/documentation/appintents)
46. [UIActivityViewController](https://developer.apple.com/documentation/uikit/uiactivityviewcontroller)
47. [快捷指令使用手册](https://support.apple.com/guide/shortcuts/welcome/ios)
48. [App Store Review Guidelines（2.4.2 / 2.5.4 / 4.2 / 5.1.1 / 5.1.2）](https://developer.apple.com/app-store/review/guidelines/)
49. [App privacy details on the App Store](https://developer.apple.com/app-store/app-privacy-details/)

### 法规 / 合规

50. [GDPR 全文（EUR-Lex, Regulation (EU) 2016/679）](https://eur-lex.europa.eu/eli/reg/2016/679/oj)
51. [欧盟委员会：数据保护专题](https://commission.europa.eu/law/law-topic/data-protection_en)
52. [欧洲数据保护委员会（EDPB）](https://www.edpb.europa.eu/)
53. [《中华人民共和国个人信息保护法》全文（网信办转载）](https://www.cac.gov.cn/2021-08/20/c_1631050028355286.htm)

### 2026-10-23 复核补充来源

- [AOSP Security enhancements（Clipboard data：Android 10+ 仅默认 IME/焦点应用可访问）](https://source.android.com/docs/security/enhancements)
- [Android 13 行为变更（所有应用）：高优先级 FCM 配额与降级](https://developer.android.com/about/versions/13/behavior-changes-all#fcm-quotas)
- [前台服务超时（dataSync/mediaProcessing 6 小时/24 小时专页）](https://developer.android.com/develop/background-work/services/fgs/timeout)
- [声明前台服务并请求权限（foregroundServiceType / FOREGROUND_SERVICE_*）](https://developer.android.com/develop/background-work/services/fgs/declare)
- [PeriodicWorkRequest API（MIN_PERIODIC_INTERVAL_MILLIS）](https://developer.android.com/reference/androidx/work/PeriodicWorkRequest)
- [FCM Android 客户端设置（Check for Google Play services）](https://firebase.google.com/docs/cloud-messaging/android/client)
- [小米推送产品说明（MiPush）](https://dev.mi.com/xiaomihyperos/documentation/detail?pId=1533)
- [Huawei Push Kit](https://developer.huawei.com/consumer/en/hms/huawei-pushkit)
- [HarmonyOS Push Kit 接入指南](https://developer.huawei.com/consumer/cn/doc/harmonyos-guides/push-gettingstart)
- [OPPO 开放平台推送文档](https://open.oppomobile.com/documentation/page/info?id=11207)
- [vivo 开放平台推送文档](https://dev.vivo.com.cn/documentCenter/doc/541)

### 非官方但建议纳入社区数据

54. [dontkillmyapp.com（厂商后台限制社区统计，**非官方**）](https://dontkillmyapp.com/)
55. [Apple Developer Forums：Can't get UIPasteboard content in the background（社区讨论，**非官方**，佐证 iOS 无法后台读取剪贴板）](https://developer.apple.com/forums/thread/706761)

---

## 8. 附：调研未覆盖/待决策

1. **国内无 GMS 设备的推送方案**：已补充 6.1（FCM 依赖 Google Play services 的官方依据、小米/华为/OPPO/vivo 官方文档入口、`PushProvider` 架构建议）。仍待决策：覆盖哪些厂商、是否接受自建分发/侧载与商店上架要求、各厂商配额与审核规则。
2. **iOS 通知 Action 后台写剪贴板的可行性**：官方无保证，需真机实验；若不可行，统一改为“打开并复制”。
3. **Android 锁屏状态下后台写剪贴板的真机行为**：AOSP 代码未见限制，但 OEM 可能定制；需覆盖主流 ROM 测试。
4. **FCM v2 GA 时间与迁移计划**：目前预览版，服务端需保持 v1/v2 可切换。
5. **iOS 最低版本**：若产品要求覆盖 iOS 15，需要额外设计一套“无 `UIPasteControl`”的降级 UX（系统弹窗 + 分享扩展为主），本调研建议先做 iOS 16+。
