# drive-sync 桌面 GUI 客户端（drive-sync-gui）

纯 Rust 桌面客户端（[eframe/egui](https://github.com/emilk/egui)），不依赖 Node.js / WebView，
Windows / Linux / macOS 同一套源码。数据目录与 CLI（`drive-sync`）完全共用，可随时切换。

> 适用版本：v0.3.2+（协议 v2）。GUI 与 CLI 使用同一 `identity.json` / `config.json` /
> `peers.json` / `catalog.json` / `vault_keys.json`，不存在“GUI 账号”与“CLI 账号”之分。

## 1. 功能

| 页面 | 能力 |
| --- | --- |
| 首次运行向导 | 欢迎说明 → 设备名 → 中继 URL + token（含“测试连接”）→ 配对（发起显示 8 位码 + SAS / 加入输入码或扫码串 + SAS 确认）→ 完成并启动同步；可返回上一步 |
| 状态 | 连接状态、本机设备名/ID、在线设备、E2E 会话、最近事件、开始/停止同步、复制设备 ID |
| 设备 | 已配对设备列表（最近活跃 / 90 天到期）、发起配对、加入配对、删除配对（二次确认；对端在线立即通知，离线由中继离线信箱在其上线后补达） |
| 剪切板 | 自动同步开关、文本/图片共享开关、轮询间隔、回环抑制说明、最近记录（只显示类型/大小/方向，不含内容） |
| 软件目录 | 列表（名称/版本/平台/大小/持有设备）、添加文件（哈希后登记）、删除（墓碑）、从在线持有设备加密获取、打开下载目录、传输记录 |
| 云端暂存 | 本设备暂存对象列表（对象 ID/名称/大小/到期时间）、加密上传（TTL 1–30 天）、下载解密到目录 |
| 设置 | 外观主题（跟随系统/深色/浅色）、打开即同步开关、最小化到托盘开关、中继地址/token/设备名、开机自启、检查更新/立即更新、数据目录（显示/打开）、关于（版本/协议/GitHub）、退出 |
| 日志 | 滚动日志（`<数据目录>/logs/gui.log`）、清空、复制全部、打开日志目录 |

### 外观主题（深色 / 浅色 / 跟随系统）

- 设置页「外观主题」三选一，**修改立即生效**（无需重启），持久化到
  `<数据目录>/gui.json`（GUI 专用，与 CLI 的 `config.json` 完全分离，不影响 CLI 兼容性）。
- 默认「跟随系统」：Windows 读取 `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize`
  的 `AppsUseLightTheme`；Linux 读取 `gsettings get org.gnome.desktop.interface color-scheme` /
  `gtk-theme` 以及 `GTK_THEME` / `COLORSCHEME` 环境变量；macOS 读取
  `defaults read -g AppleInterfaceStyle`。**读取失败回退深色，不 panic**；系统外观变化时自动跟随。
- 两套 PolyBot 令牌（全部页面共用）：
  - 深色：bg `#0B0F14`、card `#111820`、border `#1E2A33`、文字 `#E6EDF3/#9CA3AF`、
    主色 `#22E3B0`、危险 `#F87171`；
  - 浅色：bg `#F5F7FA`、card `#FFFFFF`、border `#E2E8F0`、主文字 `#0B1220`、次要 `#64748B`、
    主色 `#0EA37F`（白底对比度）、危险 `#DC2626`；网格纹更淡，卡片阴影更收敛。

说明：

- 所有网络请求与同步都在后台线程（tokio）执行，UI 不会卡网络。
- 本机剪切板由独立线程轮询（默认 800ms，可用设置页/CLI 配置调整），哈希去重后才上报。
- 远端内容只显示类型与大小；文本/图片写回系统剪切板，文件列表仅记录不自动展开。
- 中继仍只看到密文：剪切板、目录元数据、暂存内容均端到端加密；软件本体留在持有设备本地。

## 2. 构建与运行

```bash
# 工作区根目录；GUI 需要 TLS（连接 wss:// 中继）
cargo build --release -p drive-gui
./target/release/drive-sync-gui            # Windows: target\release\drive-sync-gui.exe
```

- 可用 `--data-dir <DIR>` 指定数据目录（与 CLI 的 `--data-dir` 一致）。
- 环境变量 `DRIVE_SYNC_DATA_DIR` 同样可指定数据目录。
- debug 构建保留控制台方便排错；release 构建在 Windows 上隐藏控制台窗口
  （`cfg_attr(not(debug_assertions), windows_subsystem = "windows")`）。
- 窗口默认 1000×700，最小 860×600；中文界面通过系统字体加载。

### 数据目录默认位置（与 CLI 共享）

| 平台 | 默认路径 |
| --- | --- |
| Windows | `%APPDATA%\drive-sync` |
| Linux | `$XDG_DATA_HOME/drive-sync` 或 `~/.local/share/drive-sync` |
| macOS | `~/Library/Application Support/drive-sync` |

可与 CLI `drive-sync` 直接互换使用：CLI 初始化过的目录，GUI 启动会沿用身份与配置；
GUI 完成的配对/目录/暂存操作，CLI 也能读取。

## 3. 首次运行

1. 若数据目录没有 `identity.json`，或 `config.json` 缺少中继地址/token，GUI 会自动进入向导；
2. 填写设备名 → 中继地址（示例占位符 `wss://sync.likecloud.top/ws`，请替换为你的中继）→ 访问令牌；
   可点“测试连接”验证 token 与网络（使用临时身份握手，不影响本机配对）；
3. 配对（可跳过，稍后在“设备”页完成）：
   - 发起：另一台设备选择“加入”，输入显示的 8 位配对码（或粘贴 `dsync-pair-v1:…` 扫码串）；
   - 双方屏幕出现 6 位 SAS，核对一致后点“SAS 一致，确认配对”；超时或点取消即中止；
4. 完成后进入主界面并自动尝试启动同步。

### 打开即同步（默认）

- 已完成首次运行向导（`identity.json` + `config.json` 的中继/token 完整）后，**打开 GUI 即自动开始同步**：
  连接中继、监听/应用剪切板、目录与暂存事件，无需点击「开始同步」，也无需运行任何命令行。
- 设置页「启动与窗口行为」提供「打开应用时自动开始同步」开关（默认开启）；关闭后启动只显示界面，
  需手动点「开始同步」。设置保存在 `<数据目录>/gui.json`（与 CLI `config.json` 分离）。
- 自动连接失败（中继不可达 / token 错误 / 网络异常）时窗口不会阻塞：状态页显示「连接失败」卡片与
  「重试连接」按钮，顶栏状态同步显示错误；修改设置后可立即重试。
- Windows 安装包内可执行文件名为 `DriveSync.exe`，安装后双击即打开并自动同步。

### 系统托盘（Windows / macOS）

- 点最小化或关闭窗口（X）默认**隐藏到系统托盘**（不占任务栏），同步保持运行；设置页「启动与窗口行为」
  可关闭该行为（关闭后最小化保留任务栏、X 退出）。
- 托盘左键单击 → 显示主界面；右键菜单：显示主界面 / 开始同步 / 停止同步 / 退出；
  tooltip 显示同步状态（drive-sync · 同步中/未同步/连接异常）。
- 托盘图标由程序绘制的薄荷绿圆角 logo 生成（RGBA，无图片资源）；从托盘恢复使用原生窗口显示
  （Windows `ShowWindow`），隐藏期间 eframe 暂停重绘也不会丢失同步。
- Linux 构建不启用 tray-icon（避免 libappindicator 系统依赖），回退为普通任务栏窗口、关闭窗口退出；
  设置页会禁用该开关并说明平台差异。

## 4. 与 CLI / 多端协作

- **GUI 启动同步 ≈ CLI `daemon`**：同一设备在中继上只允许一条连接，GUI 与 CLI daemon 同时运行会互踢；
  请二选一，切换前在 GUI 点“停止同步”或在 CLI 停掉 daemon。
- 多台设备可以 GUI + CLI 混搭：只要 `peers.json` 里有对方记录即可建立 E2E 会话。
- 90 天不活跃的配对会由客户端自动失效；GUI 设备列表显示“最近活跃 / 90 天到期”。

## 5. 开机自启

设置页勾选即可（也可手动管理）：

| 平台 | 实现 |
| --- | --- |
| Windows | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 的 `DriveSyncGui` = 当前 exe 路径 |
| Linux | `~/.config/autostart/drive-sync-gui.desktop`（XDG autostart） |
| macOS | `~/Library/LaunchAgents/dev.drivesync.gui.plist`（RunAtLoad） |

## 6. 自动更新

设置页“检查更新”会下载并校验 Ed25519 签名的 `latest.json` 清单（与 CLI `update` 相同来源：
`drive_updater::DEFAULT_UPDATE_URL`，可被 `config.json` 的 `update_manifest_url` / `update_pubkey`
或环境变量 `DRIVE_UPDATE_URL` / `DRIVE_UPDATE_PUBKEY` 覆盖）。“立即更新”会下载目标平台产物、
校验大小 + BLAKE3、替换当前 exe，**重启 GUI 后生效**。

## 7. 打包

- **Windows**：单文件 `drive-sync-gui.exe`（release 构建，无控制台）。
- **Linux**：`tar.gz` 内放 `drive-sync-gui` + `README`/`docs`；运行依赖系统图形环境
  （X11/Wayland + OpenGL 3.3；无显示器的 CI 只做编译验证）。
- **macOS**：`.app` 包结构：
  ```
  drive-sync.app/
    Contents/
      Info.plist          # 用 crates/gui/packaging/Info.plist 模板，替换 __VERSION__
      MacOS/
        drive-sync-gui    # release 二进制
  ```
  模板里的 `CFBundleIdentifier=dev.drivesync.gui`、`CFBundleExecutable=drive-sync-gui`；
  发布时 `sed -i '' "s/__VERSION__/0.3.2/g" Info.plist` 即可（版本以实际为准）。

## 8. 验收截图（不入库）

`target/gui-screenshots/` 保存本机验收截图（`target/` 已在 `.gitignore` 中，不会提交）：
**深色 `dark-*.png` 与浅色 `light-*.png` 各 12 张**（首次向导 5 张 + 主界面 7 页各 1 张），
共 24 张 PNG（约 1500×1050，系统 DPI 缩放；每个页面已检查在 1000×700 下无裁剪，
超长内容使用滚动区域，中文渲染与对比度正常）。

生成方式（本机 Windows 已实测）：

```bash
# 深色 / 浅色各跑一遍向导 + 主界面（DRIVE_GUI_SHOT_THEME 强制主题并配合前缀命名）
for theme in dark light; do
  DRIVE_GUI_SHOT_DIR=target/gui-screenshots DRIVE_GUI_SHOT_PREFIX="$theme-" \
  DRIVE_GUI_SHOT_THEME="$theme" DRIVE_GUI_SHOT_MODE=wizard \
    drive-sync-gui --data-dir "target/theme-demo-wizard-$theme"
  DRIVE_GUI_SHOT_DIR=target/gui-screenshots DRIVE_GUI_SHOT_PREFIX="$theme-" \
  DRIVE_GUI_SHOT_THEME="$theme" DRIVE_GUI_SHOT_MODE=main \
    drive-sync-gui --data-dir "target/theme-demo-main-$theme"
done
```

调试用自动化环境变量（仅供本机联调，不写入文档示例的常规用法）：

| 变量 | 作用 |
| --- | --- |
| `DRIVE_GUI_SHOT_DIR` | 指定截图输出目录 |
| `DRIVE_GUI_SHOT_PREFIX` | 截图文件名前缀（如 `dark-` / `light-`） |
| `DRIVE_GUI_SHOT_THEME=dark\|light` | 强制截图使用深色/浅色（覆盖 gui.json，不写入 gui.json） |
| `DRIVE_GUI_SHOT_MODE=wizard\|main` | 截图流程（向导 / 主界面） |
| `DRIVE_GUI_AUTOSTART_SYNC=1` | 启动后自动开始同步（验证/演示） |
| `DRIVE_GUI_AUTO_PAIR=initiate` 或 `join:<8 位码>` | 自动发起/加入配对（配 `DRIVE_GUI_AUTO_CONFIRM_SAS=1` 自动确认 SAS） |

## 9. 故障排查

| 现象 | 处理 |
| --- | --- |
| 连接失败 / `wss://` 握手错误 | 确认 GUI 是带 TLS feature 构建（本仓库默认启用 `drive-client/tls`）；自签证书不信任请换受信任证书或使用中继提供的 wss 地址 |
| 中文显示为方块 | 检查系统字体：Windows `C:\Windows\Fonts\msyh.ttc`/`simhei.ttf`、macOS `/System/Library/Fonts/PingFang.ttc`、Linux `Noto Sans CJK`/`wqy-microhei`；缺失时界面回退英文标签并记录日志 |
| 启动后提示“已在运行” | 单实例锁：关闭任务栏/托盘里的旧窗口或结束旧 `drive-sync-gui` 进程 |
| GUI 与 CLI 同时运行互相踢下线 | 同一设备只允许一条中继连接；停止其中一方的同步/daemon |
| 主题切换不生效 / 跟随系统仍是深色 | 检查 `<数据目录>/gui.json` 的 `theme`（system/dark/light）；「跟随系统」在读取失败时按规范回退深色；可显式选「深色/浅色」覆盖 |
| 想恢复默认主题 | 删除 `<数据目录>/gui.json` 后重启 GUI（默认跟随系统） |
| 配对一直等待 | 双方都需在线并连接同一中继；确认 token 相同；SAS 超时会自动取消，可重试 |
| 日志排错 | 设置页/日志页“打开日志目录”，文件 `<数据目录>/logs/gui.log`；报告问题时附最后 50 行（日志不含明文内容） |
| Linux 无图形环境（CI） | 只保证编译通过；`cargo build -p drive-gui` 不需要显示器，运行需要 X11/Wayland + OpenGL |
