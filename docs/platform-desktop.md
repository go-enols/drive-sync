# 桌面端平台实现调研（Windows / Linux）

> 适用范围：drive-sync 桌面客户端（共享剪切板 + 软件目录表 + 可选加密云端暂存）在 Windows 10/11 与 Linux（X11 / Wayland）上的**平台层实现决策**。
> 目标：让一个最小原型能直接照着做，同时标出"原型可简化"与"生产级必须做"的边界。
> 调研时间：2026-10（2026-10-23 完成一轮补充核验）。调研方式：`web_fetch` 直抓官方原文（微软文档、freedesktop/X11/Wayland 规范与协议源码、KDE/GNOME 文档、主流开源项目源码/README）+ `web_search` 交叉核验（2026-10-23，重点复核 12 条关键结论与 §10 未决项）。搜索无法定论的条目在文中明确标注"需实测"；核验记录见文末 §12。
> 与代码的对应关系：本文对齐 `crates/core/src/clipboard.rs`（`ClipboardKind` / `ClipboardPayload` / `ClipboardEvent`）、`crates/core/src/peer.rs`（`PeerMessage::Ping/Pong`）与 `crates/core/src/session.rs`（E2E 信封按连接 seq 严格有序）。

---

## 0. 结论速览

### 0.1 一句话结论

| 领域 | 结论 |
| --- | --- |
| Windows 剪切板 | **事件驱动**：隐藏窗口 + `AddClipboardFormatListener` → `WM_CLIPBOARDUPDATE`，读前读后校验 `GetClipboardSequenceNumber`；不用轮询 |
| Windows 常驻 | **托盘程序（用户会话内）**，不要服务：服务在 Session 0，剪贴板按 Window Station 隔离，读不到用户剪贴板 |
| Linux X11 | 监控用 **XFixes `SelectSelectionInput`**（`SelectionNotify` 事件）；读数据走 `TARGETS` + `UTF8_STRING`/`text/html`/`image/png`；**绝不按定时器读剪贴板**（Qt 文档明确警告） |
| Linux Wayland | 无窗口进程只能靠 **`ext-data-control-v1`**（`wlr-data-control` 已废弃）；KWin / wlroots 系（Sway、Hyprland）支持，**GNOME 不支持**，需 GNOME Shell 扩展或 XWayland 回退 |
| 保活 | 应用层 WebSocket/Ping 心跳 **20–30s**（pong 超时 10s，2 次失败重连），TCP keepalive 作为兜底（必须显式设置，系统默认 Win 2h / Linux 7200s）；NAT 场景 UDP 打洞心跳 15–25s |
| 休眠恢复 | 监听系统挂起/恢复事件（Win `WM_POWERBROADCAST`；Linux logind `PrepareForSleep`），恢复后**立即重连 + 全量对齐 seq/revision**；定时器用单调时钟 |
| 功耗 | 事件驱动 + 定时器合并（Win `SetCoalescableTimer`，Linux timerfd + timer slack），禁止 <1s 的常规定时器；空闲退避；用 `powercfg /energy`、`sleepstudy`、`powertop` 量化 |
| 局域网发现 | **mDNS/DNS-SD**（`mdns-sd` crate，服务类型 `_derivesync._tcp.local.`）+ 手动 IP/配对兜底；SSDP 不推荐；Windows 端必须处理防火墙入站默认拦截 |
| 局域网直连 | mDNS 发现后 **TCP + TLS（rustls，证书指纹校验）** 直连承载现有 E2E 信封；跨网段再用 STUN 打洞，失败回退中继；不引入新的应用层协议 |
| 打包 | Windows：**MSI（WiX）+ HKCU Run 自启**（可选便携 zip，MSIX 后置）；Linux：**AppImage + deb**（原生 systemd user unit / XDG autostart），Flatpak 后置 |
| 现有方案可借鉴 | ClipCascade（Wayland 轮询兜底 3s、文件只传路径）、KDE Connect（TLS + JSON packet + 插件架构）、LocalSend（双向 register 发现 + 自签证书指纹）、Syncthing（设备身份=证书哈希、发现/中继分层） |

### 0.2 推荐技术选型总表（桌面端）

| 能力 | Windows 推荐 | Linux 推荐 | Rust crate / API |
| --- | --- | --- | --- |
| 剪切板事件 | `AddClipboardFormatListener` + 隐藏消息窗口 | X11：XFixes；Wayland：`ext-data-control-v1` | `windows`、`x11rb`（含 `protocol::xfixes`）、`wl-clipboard-rs` |
| 剪切板读写 | Win32 剪贴板 API（自己封装） | X11：x11rb；Wayland：wl-clipboard-rs | `windows` / `x11rb` / `wl-clipboard-rs`；`arboard` 仅用于"有窗口应用"的简单读写 |
| 托盘/自启动 | `Shell_NotifyIcon` + HKCU Run | XDG autostart 或 systemd user unit | `tray-icon`、`global-hotkey`、`auto-launch`（亦可自己写注册表/desktop 文件） |
| 网络 | tokio + WebSocket / TCP | 同左 | `tokio`、`tokio-tungstenite`/`axum`、`rustls`、`quinn`（可选 QUIC） |
| 局域网发现 | mDNS 自带 responder | mDNS（avahi 可选） | `mdns-sd` |
| 休眠/功耗 | `WM_POWERBROADCAST`、`RegisterSuspendResumeNotification`、`SetCoalescableTimer` | logind D-Bus `PrepareForSleep`、timerfd/timer slack | `windows`、`zbus` + `zvariant` |
| 打包更新 | MSI + 自签更新清单（后期 MSIX/winget） | AppImage + deb；更新清单签名 | WiX / `cargo-deb` / Tauri updater（可选） |

### 0.3 五条必须记住的硬约束

1. **剪贴板是会话级/窗口站级资源**。Windows 服务（Session 0）拥有自己的 Window Station 与剪贴板，任何"服务里读用户剪贴板"的设计都不可行（[Interactive Services](https://learn.microsoft.com/en-us/windows/win32/services/interactive-services)、[Window Stations](https://learn.microsoft.com/en-us/windows/win32/winstation/window-stations)）。
2. **X11 不是"复制"，是"所有权引用"**。数据由所有者进程提供；所有者退出则数据消失，除非有剪贴板管理器接管（[ArchWiki: Clipboard](https://wiki.archlinux.org/title/Clipboard)）。Qt 文档也明确：X11 只会通知所有者与前一所有者，不会通知所有应用（[QClipboard](https://doc.qt.io/qt-6/qclipboard.html)）。
3. **Wayland 核心协议没有"全局剪贴板监听"能力**。只有拿到焦点的客户端才会收到 selection 事件；后台常驻程序必须依赖 data-control 协议或桌面环境私有通道。`wlr-data-control-unstable-v1` 的协议文件已写明自身 **deprecated、not intended for production use，要求改用 `ext-data-control-v1`**（[wlr-data-control](https://wayland.app/protocols/wlr-data-control-unstable-v1)）。
4. **桌面 Linux 的 Wayland 生态是分裂的**：KWin 已实现 `ext_data_control_manager_v1`（[KWin 源码](https://invent.kde.org/plasma/kwin/-/blob/master/src/wayland/datacontroldevicemanager_v1.cpp)），Sway/Hyprland 等 wlroots 系可用；**GNOME 不支持，需要 GNOME Shell 扩展**，且扩展在 Flatpak/AppImage 里无法注册（[CopyQ Known Issues](https://copyq.readthedocs.io/en/latest/known-issues.html)）。
5. **Windows 防火墙默认拦截入站**（[Windows Firewall overview](https://learn.microsoft.com/en-us/windows/security/operating-system-security/network-security/windows-firewall/)）。任何"局域网监听端口"的直连方案都必须处理首次监听弹窗/静默失败/安装器加规则三件事，否则用户会看到"设备发现得到但连不上"。

---

## 1. Windows 剪切板

### 1.1 事件模型：监听而非轮询

- 把窗口加入系统维护的剪贴板监听列表后，剪贴板内容变化会收到 `WM_CLIPBOARDUPDATE`（Vista+，桌面应用）：`AddClipboardFormatListener(HWND)`（[官方文档](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-addclipboardformatlistener)）。
- 该窗口**不需要可见**：用一个隐藏窗口（`HWND_MESSAGE` 的 message-only window 或普通隐藏窗口）挂消息循环即可。Rust 侧可以让 `winit` 的事件循环承载，也可以在专用线程里跑一个极小的 Win32 消息泵；托盘程序本来就有消息循环，复用之。
- `GetClipboardSequenceNumber()` 返回**当前 Window Station** 的剪贴板序列号，内容变化或清空都会 +1；若进程没有 `WINSTA_ACCESSCLIPBOARD` 权限则返回 0；**延迟渲染（delayed rendering）时，序列号要等渲染发生后才增加**（[官方文档](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getclipboardsequencenumber)）。因此它是"去重/一致性校验"的辅助，不是事件来源。
- 实践写法（防 TOCTOU）：收到事件后 `seq0 = GetClipboardSequenceNumber()` → `OpenClipboard` 读数据 → 关闭 → `seq1 = GetClipboardSequenceNumber()`；若 `seq0 != seq1` 丢弃本次结果，等下一条 `WM_CLIPBOARDUPDATE`。这能避免读到"半新半旧"的内容。
- 清空剪贴板同样会触发事件；读取时 `EnumClipboardFormats` 为空即视为"清空事件"，向对端发送显式的 clear 语义而不是空文本。
- **原型可简化**：只监听文本；**生产级**：必须处理 `OpenClipboard` 失败重试（其它程序短暂占用剪贴板是常态，建议 10–50ms 退避、总预算 ≤200ms，超时丢弃该次事件而不是阻塞消息循环）、空剪贴板、自己写入导致的回环。

### 1.2 格式：先定内部模型，再做平台映射

Windows 标准格式（[Standard Clipboard Formats](https://learn.microsoft.com/en-us/windows/win32/dataxchg/standard-clipboard-formats)）：

| 格式 | 值 | 内容 | 用途/注意 |
| --- | --- | --- | --- |
| `CF_UNICODETEXT` | 13 | UTF-16LE，CRLF 换行，NUL 结尾 | 纯文本主通道；接收端统一转 UTF-8 + LF |
| `CF_TEXT` | 1 | ANSI（代码页） | 不要读；系统可自动由 Unicode 合成 |
| `HTML Format` | 注册名 | UTF-8 的 HTML 片段，头部含 `Version`/`StartHTML`/`EndHTML`/`StartFragment`/`EndFragment` 字节偏移 | 富文本；Win10 20H2 起 `Version:1.0`；偏移量要按字节解析（[HTML Clipboard Format](https://learn.microsoft.com/en-us/windows/win32/dataxchg/html-clipboard-format)） |
| `CF_DIB` | 8 | `BITMAPINFO` + 位图位 | 兼容性最好，但无色彩空间 |
| `CF_DIBV5` | 17 | `BITMAPV5HEADER` + 色彩空间 + 位图位 | 推荐读取的位图格式；系统会在请求时做色彩空间转换（[Clipboard Formats](https://learn.microsoft.com/en-us/windows/win32/dataxchg/clipboard-formats)） |
| `CF_HDROP` | 15 | `HDROP`，文件路径列表 | 文件复制；用 `DragQueryFile` 枚举；跨机同步只能同步"路径/清单"，实体文件必须走软件目录/文件通道 |
| `PNG`（注册名） | — | PNG 字节流 | **跨平台图片交换的事实标准**（Chromium 等均提供）；建议"优先 PNG，回落 DIBV5" |

格式选型建议（与 `ClipboardPayload` 的四类映射）：

- `ClipboardKind::Text` ↔ `CF_UNICODETEXT` / Linux `UTF8_STRING` / Wayland `text/plain;charset=utf-8`；统一 UTF-8 + `\n`，写回 Windows 时再转 CRLF。
- `ClipboardKind::RichText` ↔ `CF_HTML`；用 `mime = "text/html; charset=utf-8"`。归一化时只取 `StartFragment..EndFragment` 的片段，**不要把整段上下文 HTML 同步出去**（体量大、且可能带来源站点信息）。
- `ClipboardKind::Image` ↔ 优先 `PNG` 注册格式，缺失时读 `CF_DIBV5` 转 PNG（`image` crate）。写回时同时提供 `PNG` 和 `CF_DIBV5` 更兼容（原型只写 PNG 即可）。
- `ClipboardKind::Files` ↔ `CF_HDROP`。**只同步文件清单（名称、大小、哈希）**，实体传输交给软件目录/文件传输通道——这一条与 ClipCascade 的教训一致：剪贴板本身不存储文件，只保留路径（[ClipCascade README](https://github.com/Sathvik-Rao/ClipCascade)）。

写剪贴板时注意：

- 用 `SetClipboardData` 一次性写入完整数据（**不要**用延迟渲染），否则进程退出/崩溃后数据丢失，系统还会向所有者窗口发 `WM_RENDERFORMAT`；我们是常驻程序，但延迟渲染会让"复制后立刻断电/重启"的时间窗变长。
- 写入前 `EmptyClipboard()`，写完 `CloseClipboard()`；每类数据用 `GlobalAlloc(GMEM_MOVEABLE)` 分配。
- 收到自己写入触发的 `WM_CLIPBOARDUPDATE` 时，用 **BLAKE3 哈希 + 最近写入时间窗（如 2s）** 双重判定抑制回环（`ClipboardPayload::hash` 已经算好未压缩内容的 BLAKE3，可直接复用）。

### 1.3 敏感内容：Windows 的三种"排除"注册格式

Windows 在 `Clipboard Formats` 文档中定义了 3 个**注册格式名**，用于阻止剪贴板内容进入系统剪贴板历史 / 云同步（[Cloud Clipboard and Clipboard History Formats](https://learn.microsoft.com/en-us/windows/win32/dataxchg/clipboard-formats)）：

| 注册格式名 | 数据类型 | 效果 |
| --- | --- | --- |
| `ExcludeClipboardContentFromMonitorProcessing` | 任意数据（存在即可） | 阻止**所有**格式进入剪贴板历史与云同步 |
| `CanIncludeInClipboardHistory` | 序列化 `DWORD`：0=排除、1=显式允许 | 只影响本地剪贴板历史，不影响云同步 |
| `CanUploadToCloudClipboard` | 序列化 `DWORD`：0=排除、1=显式允许 | 只影响云同步，不影响本地历史 |

drive-sync 的支持策略（重要，涉及"敏感内容排除"）：

1. **读取侧**：检测到上述任一"排除"标记时，默认**不上行同步**（本地仍可记录一条"已拦截"的元事件用于提示）；这是密码管理器（KeePass、1Password、Bitwarden 等）与 Windows 剪贴板历史交互的既有约定，尊重它比自创规则更可靠。
2. **写入侧**：当我们把远端内容放到本机剪贴板时，按内容敏感级别写入 `CanIncludeInClipboardHistory = 0`（例如来自"私密剪切板"或一次性验证码），避免二次泄漏到系统历史/云剪贴板；对普通内容写入 `= 1`。
3. 另外维护一层 `drive-sync` 自己的敏感规则（正则 + 用户排除的应用/标题），但这是**附加**，不能替代平台约定。
4. 注意：这些是"我们希望别人尊重"的标记，Windows 上无法强制所有程序不读取——不要宣称"密码不会被其它剪贴板工具看到"。

Linux 侧对应约定是 KDE 的 `x-kde-passwordManagerHint`（MIME 类型），GPaste 等管理器据此把内容当密码处理（[GPaste README](https://github.com/Keruspe/GPaste)）；Wayland 下通过 data-control 的 MIME 列表可见，看到该类型即视为敏感内容。

### 1.4 Rust 生态与局限

| 方案 | 能做什么 | 局限 |
| --- | --- | --- |
| **`windows`（windows-rs）** | 完整 Win32：`AddClipboardFormatListener`、`WM_CLIPBOARDUPDATE`、`GetClipboardSequenceNumber`、`RegisterClipboardFormatW`、`Get/SetClipboardData`、`GlobalAlloc/Lock/Unlock`、`Shell_NotifyIcon`、`WM_POWERBROADCAST`、`SetCoalescableTimer` | 需要自己管理 `HGLOBAL` 生命周期、消息循环、错误码；跨 bitness 句柄语义要小心（32 位进程读写 64 位程序写入的句柄型格式时不可假设可解引用，**统一走"系统合成格式"如 `CF_UNICODETEXT`/`PNG` 最稳**） |
| `clipboard-win` | 封装 Win32 剪贴板读写句柄 | 仍是"读写"抽象，事件监听、敏感格式、`CF_HDROP` 精细控制要自己补 |
| `arboard` | 跨平台文本/图片读写，API 简单；Windows 后端基于 `clipboard-win` | **没有变更通知**（要找平台 API）、无敏感标记、无 `CF_HTML`/`CF_HDROP` 级别的控制；只适合"有窗口的简单场景"或作为降级路径（[arboard](https://docs.rs/arboard/latest/arboard/)） |
| `tray-icon` | 托盘图标 + 菜单（跨平台，Windows 上封装 `Shell_NotifyIcon`） | 需与事件循环/消息泵配合；无"开机自启"能力 |
| `global-hotkey` | 全局快捷键 | 仅快捷键 |

**推荐**：平台层自建 `ClipboardBackend` trait（`start_watch(cb)` / `read()` / `write(payload)` / `supports(fmt)`），Windows 实现直接写 `windows-rs`，不要为了省事用 `arboard` 把事件能力弄丢。

### 1.5 原型 vs 生产（Windows 剪切板）

| 项 | 原型可简化 | 生产级必须做 |
| --- | --- | --- |
| 监听 | 只收 `WM_CLIPBOARDUPDATE`，只读文本 | 加 seq 校验 + `OpenClipboard` 重试 + 自写回环抑制 + 清空事件 |
| 格式 | 文本 + PNG | HTML 片段解析、DIBV5 转换、CF_HDROP 清单、格式能力协商 |
| 敏感 | 不处理 | 三个排除格式的读写语义 + 自有规则 + 用户可见提示 |
| 生命周期 | 单进程托盘 | 单实例互斥、崩溃恢复、系统重启后自启动 |

---

## 2. Linux 剪切板

### 2.1 X11：选区所有权模型

- X11 的 CLIPBOARD/PRIMARY 是**选区（selection）**：内容由当前所有者进程提供服务，粘贴方通过 `SelectionRequest` 向所有者要数据。所有者退出且无剪贴板管理器接管时，内容消失（[ArchWiki: Clipboard](https://wiki.archlinux.org/title/Clipboard)）。
- 只改选区**不会通知所有应用**：Qt 文档写得很直白——"if you change the selection within a window, X11 will only notify the owner and the previous owner of the change, i.e. it will not notify all applications"；并且"X11 clipboard is event driven... You should not store or retrieve the clipboard contents in response to timer or non-user-input events"（[QClipboard — Notes for X11 Users](https://doc.qt.io/qt-6/qclipboard.html)）。这直接否定了"定时轮询 X11 剪贴板"的工程做法（也不符合 X11 的按需服务语义，轮询会不断向所有者发请求、唤醒对端进程）。
- 正确做法：**XFixes** 扩展的选区通知。`x11rb` 的 `protocol::xfixes` 提供 `select_selection_input()` 与 `SelectionNotifyEvent` / `SelectionEvent` / `SelectionEventMask`（[x11rb::protocol::xfixes](https://docs.rs/x11rb/latest/x11rb/protocol/xfixes/index.html)）。流程：
  1. 打开 X 连接，`xfixes::query_version()`；
  2. `select_selection_input(root, atom("CLIPBOARD"), SetSelectionOwner | SelectionWindowDestroy | SelectionClientClose)`；
  3. 事件循环里收到 `SelectionNotify`（owner 变化）→ 读取；
  4. 读取：`ConvertSelection` 请求 `TARGETS`，再按优先级请求 `UTF8_STRING` → `text/html` → `image/png`；读取是异步的（`SelectionNotify` 回来才拿数据），要用状态机而不是阻塞等待。
- 常见 X11 目标格式：`TARGETS`、`UTF8_STRING`、`TEXT`、`text/plain;charset=utf-8`、`text/html`、`image/png`、`text/uri-list`、`x-special/gnome-copied-files`（Qt 文档提到 Nautilus 的文件复制约定）。**PRIMARY selection 默认不同步**（它随鼠标选区不断变化，同步会非常吵）；如要支持，做成显式开关。
- 剪贴板管理器（Klipper/GPaste/cliphist/CopyQ 等）会抢所有权保存历史；我们的监听器与它们共存，注意不要形成"两个管理器互相抢所有权"的循环。

### 2.2 Wayland：核心协议的限制与 data-control

- Wayland 核心的 `wl_data_device` 是"焦点驱动"的：普通客户端只在拥有键盘焦点时收到 selection 事件，**后台无窗口进程无法监听全局剪贴板**。这是设计使然（安全模型：剪贴板内容只对用户当前交互的应用可见）。
- 解决方案是**特权客户端协议** `data-control`，让剪贴板管理器类工具可以管理/监听所有 seat 的选区：
  - `wlr-data-control-unstable-v1`：wlroots 系事实标准，**协议文件已标注 deprecated、not intended for production use，并要求改用 `ext-data-control-v1`**（[原文](https://wayland.app/protocols/wlr-data-control-unstable-v1)）。
  - `ext-data-control-v1`：已进入 wayland-protocols **staging**（`staging/ext-data-control/ext-data-control-v1.xml`），接口与 wlr 版同构（manager → device → offer/source），事件 `selection`/`primary_selection` 在绑定后立即发一次，之后每次变化都通知（[协议文档](https://wayland.app/protocols/ext-data-control-v1)）。
- 关键差异（相对核心协议）：
  - 绑定 `ext_data_control_manager_v1` 后立刻拿到当前 selection（首事件），不需要焦点；
  - `set_selection` 可主动写入剪贴板（作为 source 提供 MIME 数据）；
  - 数据通过 fd 传输（`send(mime_type, fd)`），天然支持大对象流式读写；
  - **MIME 类型是唯一格式协商机制**，没有 Windows 的 `CF_*` 概念。

### 2.3 桌面环境支持矩阵（决定产品体验）

| 合成器/桌面 | data-control 支持 | 依据/备注 |
| --- | --- | --- |
| KWin / KDE Plasma | ✅ **Plasma 6.4+**（2025-04 起）实现 `ext_data_control_manager_v1`；更早版本只提供 `wlr-data-control` | 移植 MR !6606 于 2025-04-12 合并、目标里程碑 Plasma 6.4（[MR](https://invent.kde.org/plasma/kwin/-/merge_requests/6606)）；master 源码 `src/wayland/datacontroldevicemanager_v1.cpp` 绑定 `qwayland-server-ext-data-control-v1.h`（[源码](https://invent.kde.org/plasma/kwin/-/blob/master/src/wayland/datacontroldevicemanager_v1.cpp)）；CopyQ 文档确认 KDE Plasma 可用 |
| Sway / Hyprland / wlroots 系 | ✅（wlr-data-control，新版可能已支持 ext） | CopyQ 文档："works on KDE Plasma, Sway, Hyprland and other wlroots-based compositors" |
| GNOME（Mutter） | ❌ 不支持 data-control（**截至 2026-10 复核仍如此**） | CopyQ 文档："On GNOME, this protocol is not supported, but CopyQ ships a GNOME Shell extension"；Mutter 的跟踪 issue「Support wlr-data-control」长期未落地（[mutter#524](https://gitlab.gnome.org/GNOME/mutter/-/issues/524)）；2026-10 仍在维护的 GNOME 45–51 剪贴板管理器（ClipFlow Pro）以 **GNOME Shell 扩展（GJS，Shell 进程内）** 实现（[ClipFlow Pro](https://dev.to/nickotmazgin/clipflow-pro-156-a-free-open-source-clipboard-history-manager-for-gnome-45-51-wayland-x11-2o0l)） |
| XWayland 下的 X11 程序 | 部分（取决于桥接质量） | CopyQ 提供 `QT_QPA_PLATFORM=xcb` 回退，但同时警告可能引发监控失败、X11 连接错误等问题 |

产品策略建议：

1. **KDE/wlroots**：主路径，`ext-data-control-v1` 优先、`wlr-data-control` 兜底（两个都试，选可用的）。
2. **GNOME**：MVP 阶段明确"不支持自动监听"，提供两种降级：
   - 文档指引安装 GNOME Shell 扩展（我们如果做，必须考虑扩展分发与 GNOME 版本兼容，成本高）；
   - XWayland 回退（要求用户以 X11 会话登录，或对二进制设置 `QT_QPA_PLATFORM` 类环境并不适用 Rust 应用——Rust 应用可显式用 X11 后端连接 `DISPLAY`，但 Wayland 会话下 XWayland 的剪贴板桥接有坑，**仅作实验性选项**）。
3. **未知合成器**：探测不到 data-control 时，降级到"轮询 `wl-paste -l`"（ClipCascade 实测做法，默认 3s，见 §7.1），并在 UI 明示"降级模式，可能有延迟/功耗上升"。

### 2.4 GTK / Qt / 无窗口守护进程的差异

| 角色 | 可用能力 | 结论 |
| --- | --- | --- |
| GTK4 应用（有窗口） | `GdkClipboard` 的 `changed` 信号在剪贴板所有权变化时发出；通过 `read_async`/`read_texture_async` 读（[GdkClipboard](https://docs.gtk.org/gdk4/class.Clipboard.html)） | 适用于"我们的设置 UI 复制粘贴"，**不适用于后台监听**：Wayland 下无焦点时收不到 selection 事件 |
| Qt 应用（有窗口） | `QClipboard::changed`/`dataChanged`；X11 下有 XFixes 通知；Wayland 下同样受焦点限制（[QClipboard](https://doc.qt.io/qt-6/qclipboard.html)） | 同上；Qt 文档的 X11 注意事项值得作为"不要轮询"的依据 |
| 无窗口守护进程/CLI | X11：XFixes 可直接监听；Wayland：只能 data-control | 我们的形态属于这一类 → 走 §2.1 / §2.2 的原生协议 |

**不要**为了省事在守护进程里嵌入 GTK/Qt 来"借用"剪贴板能力：一是依赖重（GTK/Qt 运行时），二是 Wayland 下同样拿不到后台监听，三是事件循环冲突。

### 2.5 Rust 生态与局限

| crate | 能力 | 局限 |
| --- | --- | --- |
| **`x11rb`**（+ `x11rb-protocol`） | 纯 Rust X11 绑定，含 XFixes、选区请求、事件循环；无需 libX11 | 需要自己写选区状态机（TARGETS 协商、异步 SelectionNotify、INCORRECT 重试）；**这是 X11 侧的正确选择** |
| **`wl-clipboard-rs`** | Wayland 剪贴板读写 + **`watch::Watcher` 变更监听**；内部使用 `ext-data-control` 或 `wlr-data-control`（[docs](https://docs.rs/wl-clipboard-rs/latest/wl_clipboard_rs/)） | 仅 Wayland；依赖合成器支持 data-control；primary selection 需要 ext-data-control 或 wlr v2；`watch` 阻塞式 API，需放进专用线程/tokio `spawn_blocking` |
| `smithay-clipboard` | 面向**有窗口**应用的 Wayland 剪贴板（走 `wl_data_device`） | 不适合后台守护进程（无焦点就监听不到） |
| `arboard` | 跨平台简单读写（Linux 下 X11 用 x11rb，Wayland 可选 `wl-clipboard-rs` feature） | 无事件监听、无 MIME 精细控制、无敏感标记；只适合简单场景 |
| `copypasta` | 轻量跨平台读写（Alacritty 系） | 同上，且维护活跃度一般，不建议作为主方案 |

**推荐**：Linux 侧实现两个独立 backend：
- `X11Backend`：`x11rb` + XFixes（事件驱动）；
- `WaylandBackend`：`wl-clipboard-rs`（先探测 `ext-data-control` / `wlr-data-control`，失败则进入"轮询降级"状态）；
- 统一 trait 与 Windows 相同：`start_watch(cb)` / `read()` / `write(payload)` / `capabilities()`。

### 2.6 回退与轮询的代价（必读）

- `wl-paste --watch` 本身依赖 data-control；**不支持 data-control 的合成器上 `--watch` 会直接退出/不可用**，只能轮询 `wl-paste -l`（列 MIME）或 `wl-paste`（取内容）。ClipCascade 的实测策略是：native Wayland/Hyprland 先试 `wl-paste --watch`，失败则每 3 秒轮询一次（[ClipCascade README 的剪贴板监控表](https://github.com/Sathvik-Rao/ClipCascade)）。
- 轮询代价：每次轮询都会唤醒 `wl-paste` 子进程、向合成器发请求；3s 间隔在"笔记本电池模式"下不可忽视。建议：**活跃时 1–2s、空闲 5–10s 自适应**，并在设置中允许用户关闭（关闭即无法自动同步）。
- 另一个可选兜底：让用户装 `wl-clip-persist`（保持剪贴板内容）或剪贴板管理器，但它们解决的是"内容持久化"，不解决"变更通知"。

### 2.7 原型 vs 生产（Linux 剪切板）

| 项 | 原型可简化 | 生产级必须做 |
| --- | --- | --- |
| X11 | XFixes 监听 + UTF8_STRING 读取 | 完整 TARGETS 协商、HTML/PNG、非 UTF8 编码回落、异步错误恢复 |
| Wayland | 只支持 ext-data-control（KDE/wlroots） | wlr 兜底、协议探测、GNOME 明确降级、XWayland 实验选项 |
| 回退 | 轮询 3s | 自适应轮询 + 功耗提示 + 用户开关 |
| 降级提示 | 无 | 首次进入降级模式时一次性提示，避免用户以为"同步坏了" |

---

## 3. 常驻架构与自启动

### 3.1 Windows：托盘程序，不要服务

- **服务方案不可行**：服务运行在 **Session 0**，微软文档明确 "Services cannot directly interact with a user as of Windows Vista"（[Interactive Services](https://learn.microsoft.com/en-us/windows/win32/services/interactive-services)）；而剪贴板属于 **Window Station**，交互用户使用 `WinSta0`，服务在非交互 Window Station 上，有自己的剪贴板（[Window Stations](https://learn.microsoft.com/en-us/windows/win32/winstation/window-stations)）。也就是说：服务读不到、也写不了用户剪贴板。
- 正确形态：**用户会话内的托盘程序**。
  - 托盘：`Shell_NotifyIcon`（`NIM_ADD`/`NIM_MODIFY`/`NIM_DELETE`），Rust 用 `tray-icon`（[Shell_NotifyIcon](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shell_notifyicona)）。
  - 隐藏消息窗口承接 `WM_CLIPBOARDUPDATE` / `WM_POWERBROADCAST` / 托盘回调消息；与 `winit` 事件循环合用，或独立线程消息泵。
  - 单实例：`CreateMutexW` 命名互斥（`Local\drive-sync`），第二个实例把激活请求通过本地 socket/`WM_COPYDATA` 转给已运行实例后退出。
- 自启动（原型推荐）：**HKCU\Software\Microsoft\Windows\CurrentVersion\Run**，写入命令行（≤260 字符），无需管理员、无 UAC。注意微软文档说明系统可能**故意延迟** Run 键程序的启动（"does not provide guarantees about how promptly the programs in the Run key are run"），以及 Run/RunOnce 的语义（[Run and RunOnce Registry Keys](https://learn.microsoft.com/en-us/windows/win32/setupapi/run-and-runonce-registry-keys)）。
  - 生产可选：任务计划（Logon 触发器，可"最高权限"运行、启动更可控），但创建计划任务需要额外权限且用户体验更重。
- **无头模式（Windows）**：可以做（`--headless` 不进托盘），但仍然必须运行在交互用户会话内（否则没有剪贴板）。真正"无会话"的 Windows 场景只适合中继/文件同步，不适合剪贴板。

### 3.2 Linux：systemd user unit 与 XDG autostart

- **XDG autostart**：把 `.desktop` 放到 `~/.config/autostart/`（或 `/etc/xdg/autostart/`），桌面环境登录时启动；`Hidden=true` 可禁用同名系统项（[ArchWiki: XDG Autostart](https://wiki.archlinux.org/title/XDG_Autostart)、[规范](https://specifications.freedesktop.org/autostart-spec/latest/)）。优点：由桌面环境启动，**继承完整的图形会话环境变量**（`DISPLAY`/`WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`/DBus），对剪贴板访问最友好。这是**默认推荐**。
- **systemd user unit**：适合需要 `Restart=`、日志统一、依赖顺序的场景。写 `~/.config/systemd/user/drive-sync.service`，`WantedBy=graphical-session.target`（或 `default.target`）。坑：
  - user 实例默认在**首次登录**时才启动、最后一个会话结束时被杀；要脱离登录常驻需要 `loginctl enable-linger`，但 linger 的服务**运行在会话之外**，ArchWiki 明确提示"programs that need to be run inside a session will probably break in user services"（[ArchWiki: systemd/User](https://wiki.archlinux.org/title/Systemd/User)）。剪贴板正是"需要会话"的资源，**linger 的方案不要用于剪贴板主功能**。
  - 环境变量不会从 `.bashrc` 继承；`DISPLAY`/`XAUTHORITY` 在 X11 下由 `/etc/X11/xinit/xinitrc.d/50-systemd-user.sh` 导入；Wayland 需要自己 `systemctl --user import-environment WAYLAND_DISPLAY` 或走 `graphical-session.target` 配套机制（同上）。
  - 结论：**systemd unit 也要 `PartOf=graphical-session.target`，并确保图形会话启动时导入环境**；如果只交付 XDG autostart，MVP 更稳。
- 无头模式（Linux）：只有"不需要剪贴板"的功能（中继/目录同步）才有意义；有 Wayland/X11 socket 但没有图形会话时，data-control/选区都不可用。

### 3.3 建议的进程模型

```
┌──────────────────────── drive-sync (单进程, 默认 tray 模式) ────────────────────────┐
│  tokio runtime (网络/会话/加密/目录)        platform thread(s)                        │
│  ├─ relay/直连连接管理                       ├─ Win32 消息泵 (剪贴板/电源/托盘)        │
│  ├─ PeerMessage 分发                         ├─ X11 事件循环 (XFixes)                 │
│  ├─ ClipboardPayload 编解码/压缩/去重         ├─ Wayland 事件循环 (data-control)       │
│  └─ 目录/传输/暂存                           └─ 托盘 UI (tray-icon)                   │
└──────────────────────────────────────────────────────────────────────────────────────┘
```

- MVP：**单进程**，平台线程通过 `tokio::sync::mpsc`/`flume` 把事件送进异步侧；避免为了"架构漂亮"先拆 daemon+UI（IPC、生命周期、权限都会翻倍）。
- 生产再考虑拆分：`drive-sync-daemon`（无 UI，供 headless/服务器）+ `drive-sync-tray`（UI）通过本地 socket 通信；拆分的前提是**剪贴板后端必须在有会话的进程里**。

---

## 4. 保活、断线重连与功耗

### 4.1 心跳参数（建议值）

| 层 | 参数 | 建议值 | 依据 |
| --- | --- | --- | --- |
| 应用层 `PeerMessage::Ping/Pong` | 间隔 | 桌面 **25–30s**（当前 `peer.rs` 注释 30–60s 可接受，但 60s 偏高） | 自建中继常经 Nginx/Caddy/Cloudflare；nginx `proxy_read_timeout` 默认 **60s**（[nginx 文档](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_read_timeout)），WebSocket 链路 60s 级空闲断开也很常见（[websocket.org 排障指南](https://websocket.org/guides/troubleshooting/timeout/)）；应用层心跳必须小于最严一跳 |
| | pong 超时 | 10s | 单次心跳 RTT 通常 <1s；10s 足够覆盖弱网 |
| | 失败判定 | 连续 2 次无 pong → 断开重连 | 避免单次丢包误判断线 |
| WebSocket 协议层 ping | — | 由库自动处理（`tungstenite` 对收到的 ping 自动回 pong），不要与应用层心跳叠加过密 | RFC 6455 §5.5.2/§5.5.3 定义了 ping/pong 与心跳用法（[MDN: Pings and Pongs](https://developer.mozilla.org/en-US/docs/Web/API/WebSockets_API/Writing_WebSocket_servers#pings_and_pongs_the_heartbeat_of_websockets)） |
| TCP keepalive | 空闲/间隔/次数 | 60s / 10s / 3（显式设置） | Windows 默认空闲 **2 小时**、间隔 1s、探测次数固定 10（[SIO_KEEPALIVE_VALS](https://learn.microsoft.com/en-us/windows/win32/winsock/sio-keepalive-vals)）；Linux 默认 `tcp_keepalive_time=7200`、`intvl=75`、`probes=9`，即约 **2h11m** 才发现死连接（[tcp(7)](https://man7.org/linux/man-pages/man7/tcp.7.html)）。两者都太长，必须应用层覆盖 |
| UDP 打洞保活 | 间隔 | 15–25s（WireGuard `PersistentKeepalive` 默认 25s 是成熟先例，[wg(8)](https://man7.org/linux/man-pages/man8/wg.8.html)） | UDP NAT 映射超时通常 ≥2 分钟（RFC 4787 要求），25s 留足余量 |
| 重连退避 | 策略 | 指数退避 + full jitter：1s、2s、4s…上限 60s；连接稳定 60s 后重置 | 避免中继重启时惊群 |

补充：NAT/防火墙对"死连接"的判定远快于 TCP keepalive 默认值；**移动网络/企业代理下 UDP 可能直接被封**，所以业务主通道保持 TCP/WebSocket，QUIC/UDP 只作为可选加速。

### 4.2 休眠 / 唤醒

- **Windows**：窗口/服务收到 `WM_POWERBROADCAST`，事件：
  - `PBT_APMSUSPEND`（4）：系统即将挂起；
  - `PBT_APMRESUMEAUTOMATIC`（18）：**每次**恢复都会发；
  - `PBT_APMRESUMESUSPEND`（7）：用户输入触发的恢复，在 18 之后再发（[WM_POWERBROADCAST](https://learn.microsoft.com/en-us/windows/win32/power/wm-powerbroadcast)）。
  - 无窗口进程可用 `RegisterSuspendResumeNotification` / `PowerSettingRegisterNotification`（[RegisterSuspendResumeNotification](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-registersuspendresumenotification)）。
  - **Modern Standby** 下系统"保持联网"但应用会被静默/挂起，网络设备在协议卸载与低功耗间切换；应用不能假设连接一定活着，恢复后必须重连并对齐（[Modern Standby](https://learn.microsoft.com/en-us/windows-hardware/design/device-experiences/modern-standby)）。
- **Linux**：systemd-logind 的 D-Bus 信号 `PrepareForSleep(b start)`：`true` = 即将睡眠，`false` = 已恢复（[org.freedesktop.login1](https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html)）。Rust 用 `zbus` 连接 **system bus** 订阅（`zbus`/`zvariant` 正是为此场景；任务里的 `zvariant` 属于 zbus 生态）。注意：不要注册 sleep inhibitor 来"保命"——常驻同步工具不应阻止系统睡眠，只应响应。
- **恢复后的动作清单（两个平台一致）**：
  1. 立即发起一次重连（不等退避定时器；此时退避应被重置）；
  2. 重新走 E2E 握手（新 `session_id`，因为旧连接已死；`session.rs` 的"每连接重新协商"设计正好支持）；
  3. 发送/请求一次**全量对齐**：`CatalogRequest`（拿对端 revision）+ 本机剪贴板 seq/哈希对比；
  4. 丢弃所有"过期"的定时器与 pending 心跳，重新计时；
  5. 如果睡眠期间发生过本地剪贴板变化（系统休眠前用户复制），恢复后按离线队列补发（注意：离线队列要有 TTL，避免恢复后把很久以前的内容突然塞给对端）。

### 4.3 功耗：事件驱动 + 定时器合并

- **禁止**常规 <1s 的定时器（UI 动画除外）；剪贴板/网络/目录同步都应是事件驱动。
- **Windows**：用 `SetCoalescableTimer` 设置容差（`uToleranceDelay`，0 表示系统默认合并；文档建议"use the system default or the largest value possible"；[SetCoalescableTimer](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setcoalescabletimer)）。对"非实时"任务（重连、清理、心跳）设置 50–500ms 容差即可显著减少唤醒。
- **Linux**：定时器用 `timerfd`（可被 epoll 统一收编），并通过 `prctl(PR_SET_TIMERSLACK, ...)` 增大 slack（默认 50µs），让内核合并唤醒；tokio 的定时器精度高但**不要开一堆亚秒定时器**，把周期任务对齐到整分钟/整 5 分钟。
- **不调用** `SetThreadExecutionState` 阻止息屏/睡眠；需要"传输中不休眠"时，只在**主动传输**期间短暂申请，并在完成后释放（这是用户可感知的行为，需提示）。
- 空闲策略：无操作 N 分钟后把心跳从 25–30s 拉到 60s、停掉轮询类任务、目录同步改为低频；用户回归（鼠标/键盘/网络事件）时恢复。
- **量化工具**：Windows `powercfg /energy`、`/sleepstudy`、`/requests`、`/waketimers`（[powercfg](https://learn.microsoft.com/en-us/windows-hardware/design/device-experiences/powercfg-command-line-options)）；Linux `powertop`、`perf sched`/`turbostat`、`/proc/timer_list`（需权限）。验收标准建议：**空闲时 CPU 占用 <0.5%、无持续每秒唤醒**。

### 4.4 原型 vs 生产（保活与功耗）

| 项 | 原型可简化 | 生产级必须做 |
| --- | --- | --- |
| 心跳 | 固定 30s 应用层 Ping | 心跳可配置 + pong 超时 + 连续失败判定 + 代理空闲超时适配 |
| 断线 | 固定 5s 重连 | 指数退避 + jitter + 网络切换事件（Wi-Fi/有线/休眠恢复）触发立即重连 |
| 休眠 | 不处理（靠心跳超时自然重连） | 监听挂起/恢复信号，恢复即重连 + 全量对齐；单调时钟 |
| 功耗 | 不做优化 | 定时器合并、空闲退避、无 <1s 常规定时器、可量化验收 |

---

## 5. 局域网发现与直连

### 5.1 方案对比

| 方案 | 原理 | 优点 | 缺点 / 风险 | 结论 |
| --- | --- | --- | --- | --- |
| **mDNS / DNS-SD** | UDP 5353 组播，`_service._tcp.local.` + TXT 记录 | 跨平台、无需中心服务器、可自描述（TXT 携带 id/版本/公钥指纹）、主流桌面原生支持 | 组播可能被 AP isolation/防火墙拦截；Windows 防火墙需要放行；多网卡/多 VLAN 需逐接口处理 | ✅ **首选** |
| SSDP / UPnP | UDP 1900 组播 + HTTP 描述 | Windows 生态存在 | 规范庞大、攻击面大（UPnP 历史漏洞多）、多数设备不需要；不适合作为自有协议 | ❌ 不采用 |
| UDP 广播 | 向 `255.255.255.255` 或子网广播 | 实现最简单 | 不跨网段；部分 Android 受限（LocalSend 因此固定 `224.0.0.167`/`224.0.0.0/24`）；IPv6 无广播 | 🟡 仅作 mDNS 失败时的兜底 |
| 手动 IP / 配对码 / 二维码 | 用户输入 | 100% 可靠 | 体验差 | ✅ **必备兜底**（mDNS 失败时提示） |
| STUN + UDP 打洞 | 借助 STUN 服务器发现公网映射，双方互发探测 | 可跨 NAT 直连，省中继 | 对称 NAT/企业防火墙下失败；需要额外基础设施 | 🟡 作为"跨网段直连"增强，失败回退中继 |
| 组网工具（Tailscale/ZeroTier/WireGuard） | 虚拟局域网 | 免运维、NAT 穿透成熟 | 引入第三方依赖；对"自托管"目标是可选项 | 🟡 文档说明可搭配使用，不做强制 |

### 5.2 mDNS 设计（建议照抄的字段）

- 服务类型：`_derivesync._tcp.local.`（如果将来加 QUIC：`_derivesync._udp.local.`，或同一 TXT 里声明 `proto=quic`）。
- 实例名：`drive-sync-<device_short_id>`（避免重名；`mdns-sd` 实现了 RFC 6762 §9 的冲突解决并在冲突时改名，见 [mdns-sd](https://docs.rs/mdns-sd/latest/mdns_sd/)）。
- TXT 记录建议：

| key | 示例 | 说明 |
| --- | --- | --- |
| `v` | `1` | 协议版本 |
| `id` | `d3f1a9c2` | 设备短 ID（与配对身份绑定） |
| `pk` | `ed25519:<base64 前 16B>` | 公钥指纹前缀（用于 UI 展示/快速比对；完整校验在 TLS/E2E 握手） |
| `proto` | `tcp` / `quic` | 传输类型 |
| `port` | `47820` | 直连端口 |
| `caps` | `clip,catalog,relay` | 能力位 |
| `relay` | `1` | 是否允许经由中继回退 |

- Rust 选型 `mdns-sd`：纯 Rust、自带 responder + browser、无 async runtime 依赖（线程 + channel），满足"自托管、零外部依赖（不要求用户装 Avahi/Bonjour）"的诉求；局限是**只支持组播、不支持单播**（[docs.rs](https://docs.rs/mdns-sd/latest/mdns_sd/)）。
- Windows 注意：Windows 自带 mDNS 解析能力有限且与第三方 responder 可能抢 5353 端口，建议**只用 `mdns-sd` 自己的 responder**，并在绑定失败时降级到"手动 IP + 中继"；不要依赖 Bonjour SDK（安装包体/授权都麻烦）。
- Linux 注意：若系统装有 Avahi，5353 端口可能已被占用；`mdns-sd` 绑定失败要有清晰日志与降级路径（或用 Avahi 的 D-Bus API 作为备选实现，但会增加对 Avahi 的依赖）。

### 5.3 Windows 防火墙与网络位置

- 默认策略：**阻止所有入站，除非是 solicited 或命中规则；允许所有出站**（[Windows Firewall overview](https://learn.microsoft.com/en-us/windows/security/operating-system-security/network-security/windows-firewall/)）。
- 现实表现：程序第一次 `listen` 时 Windows 会弹"是否允许访问网络"；用户点"取消"或该提示被策略禁用 → 入站被静默丢弃，局域网直连表现为"发现得到、连不上"。
- 应对（按优先级）：
  1. **安装器加规则**：MSI 自定义动作或 `netsh advfirewall firewall add rule`（需管理员），为 TCP 直连端口和 UDP 5353 建"仅私有网络"规则；
  2. **运行时检测/引导**：直连失败时提示"检查 Windows 防火墙/网络是否为专用网络"，并提供一键打开防火墙设置的按钮；
  3. **纯出站兜底**：如果我们是"客户端连中继"模式，则完全不需要入站；局域网直连只是可选优化——这一点决定了 MVP 可以先不做入站。
- 网络位置：公共网络配置文件更严格（LocalSend 的排障文档同样要求"把网络设为专用网络"，[LocalSend README](https://github.com/localsend/localsend)）；我们的 UI 应解释"为什么专用网络更顺"。
- 打包注意（2026-10 修正）：MSIX **full-trust** 应用与普通 Win32 应用权限相同，容器化只做文件/注册表虚拟化（[MSIX containerization](https://learn.microsoft.com/en-us/windows/msix/msix-containerization-overview)），**不存在**"MSIX 默认阻塞入站监听"这回事；网络隔离与 loopback 限制属于 **AppContainer/UWP**（需显式选择）。因此入站监听的真实风险仍是 Windows 防火墙与"首次监听弹窗被用户拒绝"，而不是 MSIX 容器本身。

### 5.4 直连与安全衔接（与现有 E2E 会话的关系）

现有设计是"中继转发 E2E 信封"（`session.rs`：握手经中继，X25519 每连接协商，`E2eEnvelope` 带 `session_id`/`seq` 严格有序）。局域网直连**不应**绕过或复用这套安全层，而应：

1. mDNS 发现对端 → 用 TXT 里的 `id`/指纹做**快速匹配**，向用户确认（或已配对记录自动匹配）；
2. 建立 **TCP + TLS**（`rustls`，自签证书，证书指纹 = 设备身份之一；LocalSend 用"HTTPS + 自签证书 + SHA-256 指纹"就是这个模式，[LocalSend protocol](https://github.com/localsend/protocol)）；
3. 在 TLS 之上**重新走现有 E2E 握手**（`SessionInit`/`SessionAck`），得到新的 `session_id` 与 seq 起点；
4. 切换传输后，**不能**沿用旧连接的 `seq`（`session.rs` 严格连续序号会拒绝）；通过 `CatalogRequest`/全量剪贴板哈希做一次对齐；
5. 如果直连在 5s 内建立失败或中途断开 → 自动回退中继，用户无感（只记录路径切换日志）。

跨 NAT 打洞：如果将来要做，标准路径是 STUN（RFC 8489）发现公网映射 + 双方互发 UDP 探测 + 失败回退中继；NAT 行为差异见 RFC 4787（UDP）、RFC 5128（P2P 状态综述）；ClipCascade 的 P2P 模式就是"服务端只做信令 + STUN 发现"（`CC_P2P_STUN_URL` 默认 `stun:stun.l.google.com:19302`，[README](https://github.com/Sathvik-Rao/ClipCascade)），可作为参考实现。Tailscale 的 [NAT 穿透长文](https://tailscale.com/blog/how-nat-traversal-works) 对端到端流程有完整描述。

**安全底线**：无论 LAN 还是中继，内容一律走 E2E 加密；直连 TLS 只是"传输层保护 + 身份速查"，**不能**替代 E2E（否则中继路径与直连路径的安全等级不一致）。

---

## 6. 打包分发与自动更新

### 6.1 Windows

| 方案 | 优点 | 缺点 | 适用 |
| --- | --- | --- | --- |
| **MSI（WiX Toolset）** | 标准安装/卸载、可写注册表 Run 键、可加防火墙规则、企业可分发 | 需要签名；升级逻辑要自己写（MajorUpgrade）；UAC 提权 | ✅ **MVP/主推** |
| 便携 zip | 零安装、无残留 | 无自启动、无卸载、用户需自己解压 | 🟡 备选 |
| MSIX | 包身份、干净卸载、"99.96% 安装成功率"、差分更新（仅下载变化的 64KB 块）、容器化（[MSIX overview](https://learn.microsoft.com/en-us/windows/msix/overview)） | 必须签名；容器化=文件/注册表虚拟化，full-trust 应用**无网络隔离**（网络隔离属 AppContainer/UWP，[containerization](https://learn.microsoft.com/en-us/windows/msix/msix-containerization-overview)）；自启动用 `startupTask` 扩展而非 Run 键；调试/日志路径与常规 exe 不同 | 🟡 后期（若上 Microsoft Store 或需要差分更新） |
| winget / Chocolatey | 用户安装方便 | 需要维护清单；更新由包管理器驱动 | 🟡 后期 |

- **签名是硬要求**：未签名安装包会触发 SmartScreen 警告（ClipCascade 的 README 就专门解释了"未在微软注册所以有警告"以及证书费用问题，[README](https://github.com/Sathvik-Rao/ClipCascade)）。自托管工具至少应提供**校验和 + 更新清单签名**，正式分发建议 OV/EV 代码签名证书。
- 自启动：MSI 安装时写 `HKCU\...\Run`（当前用户，免提权）；若 per-machine 安装则写 `HKLM\...\Run` 或计划任务。

### 6.2 Linux

| 格式 | 优点 | 缺点 | 适用 |
| --- | --- | --- | --- |
| **AppImage** | 单文件、免安装、免 root、几乎所有发行版可跑（[appimage.org](https://appimage.org/)） | 需要 FUSE（无 FUSE 环境要 `--appimage-extract-and-run`）；不会自动装 systemd unit/desktop autostart；更新需 AppImageUpdate/zsync 或自建 | ✅ **MVP/主推**（与 deb 并行） |
| **deb** | Debian/Ubuntu 生态标准；可随包安装 `~/.config/autostart` 模板、systemd user unit | 需要维护打包脚本；跨发行版依赖差异 | ✅ **主推** |
| rpm | Fedora/openSUSE 生态 | 同上 | ✅ 有余力就做 |
| Flatpak | 沙箱、集中分发、权限模型清晰 | 默认无网络/无 X11，需 `--share=network`、`--socket=wayland`（[Sandbox Permissions](https://docs.flatpak.org/en/latest/sandbox-permissions.html)）；后台常驻需 Background portal，GNOME 剪贴板管理器仍要 Shell 扩展而沙箱内无法注册（CopyQ 文档）；Flatpak 1.15/1.16 引入 Wayland security-context 集成（供合成器识别应用身份，1.16 前有已知回归 [issue #6019](https://github.com/flatpak/flatpak/issues/6019)，[PR #6018](https://github.com/flatpak/flatpak/pull/6018) 修复），但它**不定义协议白名单**，data-control 是否可用仍取决于合成器 | 🟡 后期，需专项验证 |
| Snap | 类似 Flatpak | 同上；AppArmor 不中介剪贴板协议内容，但 Snap 的后台/自启动与 GNOME Shell 扩展注册同样受限；未见 snapd 过滤 Wayland 协议的公开证据，需实测 | 🟡 后期 |

- 打包时要把 **XDG autostart 文件**（`~/.config/autostart/drive-sync.desktop` 的模板）或 **systemd user unit** 作为安装内容/首次运行生成内容；注意 `Exec=` 用绝对路径，`X-GNOME-Autostart-Delay` 可加延迟。
- 不要用 system service 跑剪贴板（§3.2）。

### 6.3 自动更新

| 方案 | 说明 |
| --- | --- |
| **自建签名清单（推荐起步）** | HTTPS 拉 `latest.json`（版本、平台、URL、SHA-256、Ed25519 签名）→ 校验签名 → 下载到临时目录 → 校验哈希 → 替换（Windows 用"退出后替换/重启"，Linux AppImage 直接替换文件）。签名密钥可复用项目的发布密钥，但**必须与设备身份密钥隔离**。 |
| Tauri updater | 若将来用 Tauri 做 UI：官方 updater 支持 Windows/Linux/macOS（含 AppImage），使用签名更新与静态 JSON/动态服务器（[Tauri Updater](https://v2.tauri.app/plugin/updater/)）；Windows 有 `installMode`（per-user/per-machine）配置 |
| 包管理器 | deb/rpm 交给 apt/dnf；winget/Chocolatey 交给包管理器；**不要**在包管理器安装的版本里再做自更新（会打架） |

策略建议：**启动时检查 + 每天最多一次 + 用户确认后下载**；更新失败要能回滚（保留上一版本二进制/AppImage），并在更新后首次启动做一次健康检查。

### 6.4 原型 vs 生产（打包）

| 项 | 原型可简化 | 生产级必须做 |
| --- | --- | --- |
| Windows | 便携 zip / 未签名 MSI | 代码签名、MSI 升级/卸载、防火墙规则、自启动、更新签名校验 |
| Linux | AppImage（手动替换更新） | deb/rpm + 仓库、AppImageUpdate、systemd/autostart、FUSE 缺失提示 |
| 更新 | 手动下载 | 签名清单、增量/回滚、灰度、失败恢复 |

---

## 7. 现有方案点评（可借鉴与坑）

### 7.1 ClipCascade（最接近的竞品）

架构：Java 服务端（WebSocket，`/clipsocket`；P2P 模式 `/p2psignaling` + STUN）+ 各平台客户端（桌面端 Python/PyInstaller）；E2E 加密（口令派生 + salt/rounds，服务端不存密钥）；支持文本/图片/文件，**文件只同步路径**（[README](https://github.com/Sathvik-Rao/ClipCascade)）。

可直接借鉴：

- **剪贴板监控实现矩阵**（README 的表格）非常有价值：Windows `win32gui` 事件驱动；macOS 0.3s 轮询"计数器"；Linux X11 `Gtk.Clipboard` 事件驱动；XWayland 走 X11 栈或 `xclip` 0.3s 轮询；**native Wayland 先试 `wl-paste --watch`，失败则 3s 轮询 `wl-paste -l`**；这验证了本文 §2.6 的降级策略。
- **P2P 失败回退**：服务端仅做信令，STUN 发现后直连；这与 §5.4 的设计一致。
- 1 MiB 默认消息上限 + 客户端可配本地上限（Android 文本约 1 MiB 限制、Pixel 6a 建议 900KB）——提醒我们**跨端必须有 per-device 上限协商**，`MAX_CLIPBOARD_BYTES=32MiB` 只适合桌面之间。

要避免的坑：

- 未签名安装包（SmartScreen/杀软告警）；
- Android 后台剪贴板需要 `READ_LOGS` + 悬浮窗这种"非常规"手段（移动端任务会展开，本文不涉及）；
- 大文件直接走剪贴板通道会导致客户端卡顿（其 README 自己承认）。

### 7.2 KDE Connect

架构（[README](https://invent.kde.org/network/kdeconnect-kde)）：UI 无关的核心库 + D-Bus 接口 + 插件式 `NetworkPacket`（JSON：`id`/`type`/`body`/`version`；二进制 payload 用 `payloadSize` + `payloadTransferInfo` 描述，由具体 Link 建立连接传输）；设备发现由 **LinkProvider** 负责，链路使用 **TLS**。

可借鉴：

- **插件化 + 自包含 JSON 包**：消息体由插件自己解释，核心只负责路由/加密/连接——与我们 `PeerMessage` 的 serde tag 分发是同一思路；
- **二进制走独立传输通道**（`payloadTransferInfo` 里给 IP/端口），而不是塞在 JSON 里——对应我们"剪贴板只传清单、文件走传输通道"的约定；
- TLS 链路 + 配对信任的模型。

差异：KDE Connect 深度绑定 KDE/D-Bus 生态，跨桌面（GNOME/Windows）体验一般（其 README 明确说 Linux 之外的平台"not officially supported"）；我们不能照搬其 D-Bus 架构。

### 7.3 LocalSend

协议要点（[protocol](https://github.com/localsend/protocol)）：默认组播 `224.0.0.167:53317`（并说明部分 Android 设备只接受 `224.0.0.0/24`，所以默认用这个段）；HTTP(S) 53317；**双向注册发现**：收到 announce 后向对方 `POST /api/localsend/v2/register` 回自己的信息，UDP 组播回复作为 fallback；HTTPS 模式下 **fingerprint = 证书 SHA-256**，HTTP 模式为随机串；支持 PIN；设备类型枚举里专门有 `headless`/`server`。

可借鉴：

- mDNS 之外用"组播 announce + HTTP register 回连"提高发现可靠性（尤其某设备不能发组播时）；
- 自签证书 + 指纹校验，不依赖 CA；
- 明确的排障文档（AP isolation、Windows 专用网络、VPN 拦截本地流量）——这些都是我们 LAN 直连会遇到的问题，值得写进我们自己的排障页；
- 文件传输用 session/token/分片（prepare-upload → upload → cancel），可作为我们文件通道的参考。

注意：LocalSend 明确"app does not have an auto-update"（README），说明桌面 Linux/Windows 的自动更新在开源项目里是普遍短板——我们需要自己设计（§6.3）。

### 7.4 Syncthing

定位是**文件同步**，不提供剪贴板同步，但工程架构值得借鉴（[官方文档](https://docs.syncthing.net/)、[源码](https://github.com/syncthing/syncthing)）：

- **设备身份 = 证书哈希**：设备 ID 由证书派生，天然绑定身份与传输层；
- **发现分层**：本地发现（局域网广播）+ 全局发现（HTTPS 服务，可自建）+ 中继（可自建），任何一层失败都能降级；
- **可中继、可直连**：连接建立后协商走直连还是中继，业务层不感知；
- **块级传输 + 续传**：文件按块索引、断点续传、完整性校验。

对我们的映射：设备身份（Ed25519 公钥）已是配对基础；`peer.rs` 的 `TransferChunk`/`from_offset` 已是续传模型；缺的是"发现层"和"中继/直连路径选择"——正好是 §5 的内容。

---

## 8. 最小原型（MVP）实施清单

按优先级排列，预计可在一个迭代内跑通"Windows↔Linux 文本剪贴板互同步"：

1. **统一裁剪板抽象**（`ClipboardBackend` trait + `ClipboardEvent` 映射到现有 `ClipboardPayload`），先只支持 `Text`。
2. **Windows 后端**：`windows-rs` + 隐藏窗口 + `AddClipboardFormatListener` + seq 校验；写回用 `CF_UNICODETEXT`。
3. **Linux 后端**：
   - X11：`x11rb` + XFixes 监听 + `UTF8_STRING` 读写；
   - Wayland：`wl-clipboard-rs` 探测 data-control + `Watcher`；KDE/wlroots 验证；GNOME 先明确"不支持"。
4. **回环抑制**：BLAKE3 哈希 + 2s 时间窗（复用 `ClipboardPayload.hash`）。
5. **常驻形态**：单进程托盘（Windows `tray-icon`；Linux 先跑 CLI/无托盘，用 XDG autostart 启动）。
6. **自启动**：Windows 写 HKCU Run；Linux 写 `~/.config/autostart/drive-sync.desktop`。
7. **保活**：应用层 Ping 30s + pong 10s 超时 + 指数退避重连（当前 `PeerMessage` 已有 Ping/Pong，补计时与失败判定）。
8. **休眠恢复**：Windows `WM_POWERBROADCAST` / Linux logind `PrepareForSleep` → 重连 + 对齐。
9. **mDNS 发现**：`mdns-sd` 注册 `_derivesync._tcp.local.` + TXT（先只做发现，不做直连）。
10. **打包**：Windows 便携 zip → MSI；Linux AppImage（先不做自动更新）。

**明确不做（原型阶段）**：图片/HTML/文件剪贴板、敏感内容排除、LAN 直连数据通道、GNOME 支持、自动更新、MSIX/Flatpak。

---

## 9. 生产级差距清单

| # | 能力 | 差距 | 优先级 |
| --- | --- | --- | --- |
| 1 | 富格式 | HTML（片段解析）、PNG/DIBV5、文件清单（CF_HDROP / text/uri-list） | P1 |
| 2 | 敏感内容 | Windows 三格式读写 + Linux `x-kde-passwordManagerHint` + 自有规则 + UI 提示 | P0（隐私红线） |
| 3 | 大内容 | per-device 上限协商、图片转码/尺寸限制、超限自动转文件通道 | P1 |
| 4 | 稳定性 | `OpenClipboard` 重试、X11 异步错误恢复、Wayland 协议探测与降级、看门狗 | P0 |
| 5 | 功耗 | 定时器合并、空闲退避、量化验收（powercfg/powertop） | P1 |
| 6 | 休眠 | 挂起/恢复事件、恢复即重连、单调时钟、离线队列 TTL | P0 |
| 7 | 局域网 | mDNS + 防火墙规则 + 手动 IP 兜底 + 直连 TLS + 回退中继 | P2 |
| 8 | 打包 | 签名、MSI 升级、deb/rpm、更新清单签名与回滚、防火墙规则 | P1 |
| 9 | GNOME | 明确降级策略或 GNOME Shell 扩展（成本高，建议先文档化） | P2 |
| 10 | 多会话 | Windows 快速用户切换/RDP 多会话下的剪贴板归属（每会话一个进程，天然隔离） | P2 |

---

## 10. 未决问题（需要原型实测/跨文档对齐）

> 2026-10-23 复核后：原第 1、3 项已定论/修正，第 2 项部分定论，其余维持。

1. **GNOME 的 data-control（2026-10 已定论：仍不支持）**：CopyQ 文档仍标注 GNOME 不支持、需 GNOME Shell 扩展；Mutter 的「Support wlr-data-control」issue（[mutter#524](https://gitlab.gnome.org/GNOME/mutter/-/issues/524)）长期未落地；2026-10 仍在维护的 GNOME 45–51 剪贴板管理器（ClipFlow Pro）以 Shell 扩展（GJS，Shell 进程内）实现。→ 产品维持"GNOME 明确降级（扩展/实验性 XWayland/轮询）"；若 Mutter 未来合入 `ext-data-control-v1`，需重新复核 §2.2/§2.3。
2. **Flatpak 沙箱内的剪贴板能力（部分定论，仍需实测）**：Flatpak 官方沙箱权限文档未定义"Wayland 协议白名单"，默认无网络/无 X11，需 `--share=network`、`--socket=wayland`；后台常驻需 Background portal；GNOME 剪贴板管理器必须 Shell 扩展，而 Flatpak 内无法注册（CopyQ 明确）。Flatpak 1.15/1.16 的 Wayland security-context 集成只用于让合成器识别应用身份，不决定协议是否可用。→ **data-control 可用性取决于合成器**，需分别实测「KDE/wlroots + Flatpak」与「GNOME + Flatpak」。
3. **Snap 沙箱（未定论，需实测）**：未检索到 snapd 过滤 Wayland 协议的公开证据（AppArmor 不中介剪贴板协议内容），但 Snap 的后台/自启动与 GNOME Shell 扩展注册同样受限。
4. **MSIX 网络限制（2026-10 已修正）**：MSIX 容器化仅涉及文件/注册表虚拟化，full-trust 应用无网络隔离；"MSIX 限制入站监听"不成立，真实风险是 Windows 防火墙与首次监听弹窗。若改用 AppContainer 打包，才需评估 loopback 豁免。→ 实测目标改为"首次监听/防火墙规则行为"，而非"MSIX 网络隔离"。
5. **自建中继/反代的实际 WebSocket 空闲超时**：Caddy/Nginx/Cloudflare/Traefik 默认值不一（nginx `proxy_read_timeout` 默认 60s），心跳参数必须可配置，不能写死 30s 就以为安全。
6. **图片保真策略**：PNG vs DIBV5、HDR/广色域（`CF_DIBV5` 带色彩空间）、超大截图（>32MiB）的处理阈值。
7. **剪贴板历史与 30 天加密暂存的边界**：本地剪贴板历史（是否保存、保存多久）与云端加密暂存的对象生命周期需要在架构文档中统一定义；桌面端只提供策略开关，不做最终裁决。
8. **Windows 多会话/RDP**：每个交互会话一个进程，剪贴板天然隔离；但"同一账号多会话"下用户会看到两份托盘图标，需要产品决策（禁止多实例或按会话隔离配置）。

---

## 11. 参考资料

**Windows（微软官方）**

1. [AddClipboardFormatListener](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-addclipboardformatlistener)
2. [GetClipboardSequenceNumber](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getclipboardsequencenumber)
3. [Standard Clipboard Formats](https://learn.microsoft.com/en-us/windows/win32/dataxchg/standard-clipboard-formats)
4. [Clipboard Formats（含 Cloud Clipboard / Clipboard History 排除格式）](https://learn.microsoft.com/en-us/windows/win32/dataxchg/clipboard-formats)
5. [HTML Clipboard Format](https://learn.microsoft.com/en-us/windows/win32/dataxchg/html-clipboard-format)
6. [Interactive Services（服务不可直接与用户交互）](https://learn.microsoft.com/en-us/windows/win32/services/interactive-services)
7. [Window Stations（剪贴板属于 Window Station）](https://learn.microsoft.com/en-us/windows/win32/winstation/window-stations)
8. [Shell_NotifyIcon](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shell_notifyicona)
9. [Run and RunOnce Registry Keys](https://learn.microsoft.com/en-us/windows/win32/setupapi/run-and-runonce-registry-keys)
10. [WM_POWERBROADCAST](https://learn.microsoft.com/en-us/windows/win32/power/wm-powerbroadcast) / [RegisterSuspendResumeNotification](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-registersuspendresumenotification)
11. [Modern Standby](https://learn.microsoft.com/en-us/windows-hardware/design/device-experiences/modern-standby)
12. [SIO_KEEPALIVE_VALS（TCP keepalive 2h 默认值）](https://learn.microsoft.com/en-us/windows/win32/winsock/sio-keepalive-vals)
13. [SetCoalescableTimer（定时器合并）](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setcoalescabletimer)
14. [Powercfg command-line options（/energy /sleepstudy /requests /waketimers）](https://learn.microsoft.com/en-us/windows-hardware/design/device-experiences/powercfg-command-line-options)
15. [What is MSIX?](https://learn.microsoft.com/en-us/windows/msix/overview) / [MSIX Containerization](https://learn.microsoft.com/en-us/windows/msix/msix-containerization-overview)
16. [Windows Firewall overview（默认阻止入站、网络配置文件）](https://learn.microsoft.com/en-us/windows/security/operating-system-security/network-security/windows-firewall/)

**Linux / X11 / Wayland**

17. [wlr-data-control-unstable-v1（已废弃，指向 ext-data-control）](https://wayland.app/protocols/wlr-data-control-unstable-v1)
18. [ext-data-control-v1（wayland-protocols staging）](https://wayland.app/protocols/ext-data-control-v1) / [协议 XML](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/staging/ext-data-control/ext-data-control-v1.xml)
19. [KWin ext-data-control 实现源码](https://invent.kde.org/plasma/kwin/-/blob/master/src/wayland/datacontroldevicemanager_v1.cpp)
20. [CopyQ Known Issues（GNOME/Wayland/Flatpak 的剪贴板限制）](https://copyq.readthedocs.io/en/latest/known-issues.html)
21. [wl-clipboard（wl-copy/wl-paste，--watch 用法）](https://github.com/bugaevc/wl-clipboard)
22. [wl-clipboard-rs（Watcher API、ext/wlr-data-control）](https://docs.rs/wl-clipboard-rs/latest/wl_clipboard_rs/)
23. [x11rb::protocol::xfixes（SelectSelectionInput / SelectionNotify）](https://docs.rs/x11rb/latest/x11rb/protocol/xfixes/index.html)
24. [Qt QClipboard（X11 所有权与"不要按定时器读"）](https://doc.qt.io/qt-6/qclipboard.html)
25. [GTK4 GdkClipboard（changed 信号/异步读取）](https://docs.gtk.org/gdk4/class.Clipboard.html)
26. [ArchWiki: Clipboard（选区模型、无管理器则内容丢失）](https://wiki.archlinux.org/title/Clipboard)
27. [GPaste README（x-kde-passwordManagerHint 敏感标记）](https://github.com/Keruspe/GPaste)
28. [systemd-logind D-Bus 接口（PrepareForSleep / Inhibit）](https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html)
29. [ArchWiki: systemd/User（环境变量、linger、会话外限制）](https://wiki.archlinux.org/title/Systemd/User)
30. [XDG Autostart 规范](https://specifications.freedesktop.org/autostart-spec/latest/) / [ArchWiki: XDG Autostart](https://wiki.archlinux.org/title/XDG_Autostart)
31. [tcp(7)（Linux keepalive 默认 7200s/75s/9）](https://man7.org/linux/man-pages/man7/tcp.7.html)
32. [ICCCM / 选区规范](https://www.x.org/releases/current/doc/xorg-docs/icccm/icccm.html) / [freedesktop clipboard spec](https://specifications.freedesktop.org/clipboard-spec/latest/)

**网络 / 发现 / 协议**

33. [RFC 6455 §5.5.2 Ping/Pong](https://www.rfc-editor.org/rfc/rfc6455#section-5.5.2) / [MDN: Pings and Pongs](https://developer.mozilla.org/en-US/docs/Web/API/WebSockets_API/Writing_WebSocket_servers#pings_and_pongs_the_heartbeat_of_websockets)
34. [RFC 6762 mDNS](https://www.rfc-editor.org/rfc/rfc6762) / [RFC 6763 DNS-SD](https://www.rfc-editor.org/rfc/rfc6763)
35. [mdns-sd crate（纯 Rust mDNS responder/browser，仅组播）](https://docs.rs/mdns-sd/latest/mdns_sd/)
36. [RFC 8489 STUN](https://www.rfc-editor.org/rfc/rfc8489) / [RFC 4787 UDP NAT 行为](https://www.rfc-editor.org/rfc/rfc4787) / [RFC 5128 P2P 状态综述](https://www.rfc-editor.org/rfc/rfc5128)
37. [Tailscale: How NAT traversal works](https://tailscale.com/blog/how-nat-traversal-works)
38. [wg(8)（PersistentKeepalive 建议 25s）](https://man7.org/linux/man-pages/man8/wg.8.html)

**现有项目**

39. [ClipCascade README（各平台剪贴板监控实现、P2P/STUN、E2E、限制）](https://github.com/Sathvik-Rao/ClipCascade)
40. [KDE Connect README（TLS、NetworkPacket、LinkProvider、插件）](https://invent.kde.org/network/kdeconnect-kde)
41. [LocalSend README](https://github.com/localsend/localsend) / [LocalSend Protocol v2.2](https://github.com/localsend/protocol)
42. [Syncthing 文档](https://docs.syncthing.net/) / [Syncthing 源码](https://github.com/syncthing/syncthing)

**打包 / 更新**

43. [AppImage 官网](https://appimage.org/)
44. [Tauri Updater（签名更新、static JSON/动态服务器）](https://v2.tauri.app/plugin/updater/)
45. [Flatpak 文档（沙箱与权限，供 §6.2 评估）](https://docs.flatpak.org/)

**Rust 库（选型依据）**

46. [windows-rs（windows crate）](https://docs.rs/windows) / [clipboard-win](https://docs.rs/clipboard-win)
47. [arboard（简单读写，无事件监听）](https://docs.rs/arboard/latest/arboard/)
48. [tray-icon](https://docs.rs/tray-icon) / [global-hotkey](https://docs.rs/global-hotkey)
49. [tokio-tungstenite](https://docs.rs/tokio-tungstenite) / [rustls](https://docs.rs/rustls) / [quinn（可选 QUIC）](https://docs.rs/quinn)
50. [zbus](https://docs.rs/zbus) / [zvariant（logind D-Bus 信号与 inhibit，任务指定）](https://docs.rs/zvariant)

**2026-10-23 复核新增来源**

51. [wl-clipboard #242：ext-data-control 进入 wayland-protocols v1.39](https://github.com/bugaevc/wl-clipboard/issues/242)
52. [KWin MR !6606：wayland/datacontrol 移植到 ext-data-control（2025-04-12 合并，Plasma 6.4）](https://invent.kde.org/plasma/kwin/-/merge_requests/6606)
53. [Flatpak Sandbox Permissions（沙箱默认权限/端口/wayland socket）](https://docs.flatpak.org/en/latest/sandbox-permissions.html)
54. [Flatpak #6019：Wayland security-context 集成回归讨论](https://github.com/flatpak/flatpak/issues/6019) / [Flatpak PR #6018：WAYLAND_DISPLAY 处理修复](https://github.com/flatpak/flatpak/pull/6018)
55. [GNOME/mutter #524：Support wlr-data-control（长期跟踪 issue）](https://gitlab.gnome.org/GNOME/mutter/-/issues/524)
56. [ClipFlow Pro（GNOME 45–51 剪贴板管理器，2026-10，Shell 扩展形态）](https://dev.to/nickotmazgin/clipflow-pro-156-a-free-open-source-clipboard-history-manager-for-gnome-45-51-wayland-x11-2o0l)
57. [nginx `proxy_read_timeout`（默认 60s）](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_read_timeout)
58. [websocket.org：WebSocket 超时与空闲断开排障](https://websocket.org/guides/troubleshooting/timeout/)

> 注：首轮调研（2026-10）时本机 `web_search` 返回 HTTP 402（余额不足），故全部结论由 `web_fetch` 直抓官方原文得出；2026-10-23 搜索通道恢复后，已用 `web_search` 对 12 条关键结论与 §10 未决项做交叉核验（记录见 §12）。原有 URL 全部保留，新增核验来源见上方 51–58。

---

## 12. 复核记录（2026-10-23）

复核方式：`web_search` 抽查关键结论 → 对命中的官方/上游页面用 `web_fetch` 二次确认。共抽查 12 条，其中 **2 条修正/精确化、10 条确认**。

| # | 被核验的结论/数字 | 复核结果 | 依据 |
| --- | --- | --- | --- |
| 1 | `AddClipboardFormatListener` 后收到 `WM_CLIPBOARDUPDATE`（Vista+） | ✅ 确认 | [MS Learn](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-addclipboardformatlistener) |
| 2 | `GetClipboardSequenceNumber` 按 Window Station 计数、无权限返回 0、延迟渲染不立即 +1 | ✅ 确认 | [MS Learn](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getclipboardsequencenumber) |
| 3 | 剪贴板属于 Window Station；服务在 Session 0，无法访问交互用户剪贴板 | ✅ 确认 | [Interactive Services](https://learn.microsoft.com/en-us/windows/win32/services/interactive-services) / [Window Stations](https://learn.microsoft.com/en-us/windows/win32/winstation/window-stations) |
| 4 | `ExcludeClipboardContentFromMonitorProcessing` / `CanIncludeInClipboardHistory` / `CanUploadToCloudClipboard` 的排除语义 | ✅ 确认 | [MS Learn](https://learn.microsoft.com/en-us/windows/win32/dataxchg/clipboard-formats) |
| 5 | Windows 防火墙默认阻止入站、允许出站，私有/公共配置文件 | ✅ 确认 | [MS Learn](https://learn.microsoft.com/en-us/windows/security/operating-system-security/network-security/windows-firewall/) |
| 6 | MSIX 容器化/网络隔离是否限制入站监听 | ⚠️ **修正**：full-trust 与普通 Win32 同权限；容器化仅文件/注册表虚拟化，网络隔离属 AppContainer/UWP | [MSIX containerization](https://learn.microsoft.com/en-us/windows/msix/msix-containerization-overview) |
| 7 | XFixes `SelectSelectionInput` + `SelectionNotify` 用于剪贴板变更监听 | ✅ 确认 | [x11rb::xfixes](https://docs.rs/x11rb/latest/x11rb/protocol/xfixes/index.html) |
| 8 | `wlr-data-control` 已废弃、推荐 `ext-data-control-v1` | ✅ 确认并补充：ext-data-control 随 **wayland-protocols v1.39** 进入 staging | [wayland.app](https://wayland.app/protocols/wlr-data-control-unstable-v1) / [wl-clipboard #242](https://github.com/bugaevc/wl-clipboard/issues/242) |
| 9 | KWin 实现 `ext_data_control_manager_v1` | ⚠️ **精确化**：MR !6606 于 2025-04-12 合并（Plasma 6.4）；此前 KWin 只提供 wlr-data-control | [KWin MR !6606](https://invent.kde.org/plasma/kwin/-/merge_requests/6606) / [源码](https://invent.kde.org/plasma/kwin/-/blob/master/src/wayland/datacontroldevicemanager_v1.cpp) |
| 10 | GNOME 不支持 data-control | ✅ 维持：2026-10 仍无 Mutter 实现记录，GNOME 45–51 剪贴板管理器仍以 Shell 扩展实现 | [CopyQ](https://copyq.readthedocs.io/en/latest/known-issues.html) / [mutter#524](https://gitlab.gnome.org/GNOME/mutter/-/issues/524) / [ClipFlow Pro](https://dev.to/nickotmazgin/clipflow-pro-156-a-free-open-source-clipboard-history-manager-for-gnome-45-51-wayland-x11-2o0l) |
| 11 | TCP keepalive 默认：Windows 2h/1s/10 次；Linux 7200s/75s/9 次 | ✅ 确认 | [SIO_KEEPALIVE_VALS](https://learn.microsoft.com/en-us/windows/win32/winsock/sio-keepalive-vals) / [tcp(7)](https://man7.org/linux/man-pages/man7/tcp.7.html) |
| 12 | 反代 WebSocket 空闲超时常见 60s；ClipCascade Wayland 先 `wl-paste --watch`、失败回退 3s 轮询 | ✅ 确认（nginx 默认 60s；ClipCascade README 原文） | [nginx](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_read_timeout) / [ClipCascade](https://github.com/Sathvik-Rao/ClipCascade) |

**本轮对正文的其他改动**：§2.3 支持矩阵（KWin 版本、GNOME 证据）、§4.1 心跳依据（补 nginx 60s）、§5.3 与 §6.1（MSIX 修正）、§6.2（Flatpak/Snap 现状）、§10（未决项定论/修正）。

**复核期间的访问限制（供后续参考）**：GitHub REST API 在复核后期触发限流（HTTP 403）；`gitlab.gnome.org` 页面被反爬拦截（HTTP 406），因此 mutter#524 只能通过搜索命中标题 + CopyQ/ClipFlow 生态证据交叉确认，未能读到原帖正文。若后续要核实「Mutter 是否已合入 ext-data-control」，建议在能访问 GitLab 的网络环境直接查 mutter 的 MR/NEWS。
