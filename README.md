# drive-sync

自托管的多设备同步系统：**无感共享剪切板** + **软件目录表**（软件本体留在本地，云端只中转不存储）+ 可选 **30 天加密云端暂存**。支持 Windows / Linux / macOS / Android / iOS，中继服务器端到端加密、零内容存储，个人 / 小团队自建。

> 当前版本：**v0.2.0**。桌面端（Windows/Linux/macOS）为可常驻的命令行程序；Android 为可侧载 APK；iOS 为前台连接的原型 App（真机安装需 Apple 签名）。发布产物见 [GitHub Releases](https://github.com/go-enols/drive-sync/releases)，CI 预构建产物见 [Actions](https://github.com/go-enols/drive-sync/actions)。

## 下载与安装

| 平台 | 产物 | 说明 |
|---|---|---|
| Windows x64 | `drive-sync-v0.2.0-x86_64-pc-windows-msvc.zip` | 解压即用 `drive-sync.exe` / `drive-relay.exe` |
| Linux x64 | `drive-sync-v0.2.0-x86_64-unknown-linux-gnu.tar.gz` | 需 X11 依赖（arboard）；Wayland 见平台文档 |
| macOS Apple Silicon / Intel | `...-aarch64-apple-darwin.tar.gz` / `...-x86_64-apple-darwin.tar.gz` | 解压即用 |
| Android 8.0+（API 29+） | `drive-sync-v0.2.0-android-debug.apk`（Actions artifact） | 允许未知来源侧载；后台无感同步需 `adb appops` 授权（见下） |
| iOS 16+ | `drive-sync-ios-simulator.zip`（Actions artifact） | CI 产物仅模拟器可运行；真机需用 Xcode + Apple ID 签名，详见 [iOS 文档](docs/mobile-ios.md) |

> 没有 GitHub Release 时，可在 Actions 页面选择 `workflow_dispatch` 手动触发 `Release`（只产 artifact）或 `Android` / `iOS` / `CI` 工作流下载构建产物。

## 核心特性

| 能力 | 说明 |
|---|---|
| 无感剪切板同步 | 检测到本机剪切板变化即自动加密发送，对端自动写入剪贴板；文本/富文本/PNG；zstd 压缩、BLAKE3 去重、回环抑制、离线补发、按设备/类型开关 |
| 软件目录 | 仅同步元数据（名称/版本/平台/大小/BLAKE3）；HLC LWW CRDT + 墓碑，离线可合并；本体永不上传服务器 |
| 软件本体中转 | 设备间 E2E 分块流式传输（64KiB 块、流控、断点续传、整体 BLAKE3 校验）；中继零落盘 |
| 配对 | 一次性 8 位码/二维码 + 6 位 SAS 防 MITM；**配对长期有效（无 TTL），但 90 天不活跃自动失效**；任一方显式删除立即撤销 |
| 离线撤销 | 删除方生成**签名撤销通知**投递到中继离线信箱，对端即使长期离线，上线后也会验签并自动清理配对记录 |
| 自动更新 | GitHub Releases + Ed25519 签名清单 + BLAKE3 校验，下载后自替换；`drive-sync update check/apply` 手动或 daemon `--auto-update` |
| 可选云端暂存 | 客户端随机密钥加密后上传；服务端 TTL 强制 ≤ 30 天并定时删除；密钥只在设备端 |
| 安全 | Ed25519 身份、X25519 连接级前向保密、XChaCha20-Poly1305、零信任中继、发布清单签名 |
| 跨平台 | Rust 核心复用；桌面 CLI、Android APK、iOS 原型 App；局域网直连为 v0.3 路线 |

## 架构一览

```
设备 A ──wss──┐
设备 B ──wss──┼──► 自建中继（只转发密文，不落盘） ──► 设备 C
设备 D ──wss──┘        ├─ 可选：加密暂存 30 天 TTL
                       └─ 撤销通知离线信箱（签名元数据 ≤90 天）
```

详细设计见 [docs/architecture.md](docs/architecture.md)，协议规范见 [docs/protocol.md](docs/protocol.md)。

## 快速开始（桌面端）

前置：Rust 1.80+（本项目在 1.96 验证）。默认构建 `ws://` 便于内网测试；公网部署用反代终止 TLS 后连 `wss://`（需 `--features tls`）。仓库内 `.cargo/config.toml` 只是本机镜像失效时的临时覆盖，网络正常时可删除。

```bash
# 1. 编译（只编中继与客户端）
cargo build --release -p drive-relay -p drive-cli

# 2. 启动中继（自建 VPS 或本机；token 也可用环境变量 DRIVE_RELAY_TOKEN）
./target/release/drive-relay --bind 127.0.0.1:8787 --token <强随机令牌> --data-dir ./relay-state

# 3. 每台设备初始化身份（数据目录默认在用户目录；--data-dir 可指定）
./target/release/drive-sync init --name "我的笔记本"

# 4. 配对：A 端发起（打印二维码串 + 8 位配对码），B 端输入配对码或扫码
./target/release/drive-sync pair --relay ws://127.0.0.1:8787/ws --token <令牌>
./target/release/drive-sync pair --relay ws://127.0.0.1:8787/ws --token <令牌> --code 12345678
#    两端比对 6 位 SAS 一致后确认（脚本/测试可加 --yes）

# 5. 常驻同步（自动监听/应用剪切板；Ctrl-C 退出；服务化见部署文档）
./target/release/drive-sync daemon --relay ws://127.0.0.1:8787/ws --token <令牌>
#    同机测试可用文件模拟剪切板：--clipboard file:/tmp/clip-a.txt

# 6. 软件目录（add 只登记元数据+本地路径；本体不上传）
./target/release/drive-sync catalog add --path ./setup.exe --name "某工具" --version 1.2.3 --platform windows --kind installer
./target/release/drive-sync catalog fetch <entry_id> --out ./downloads

# 7. 可选：加密云端暂存（最长 30 天）
./target/release/drive-sync vault put --path ./big.iso --ttl-days 7
./target/release/drive-sync vault get <object_id> --out ./big.iso

# 8. 配对管理（长期有效；显式删除会通知对端，离线也能在下次上线自动清理）
./target/release/drive-sync peers list
./target/release/drive-sync peers remove <device_id>

# 9. 自动更新
./target/release/drive-sync update check
./target/release/drive-sync update apply --yes
#    daemon 常驻自动更新（可选）：daemon ... --auto-update
```

注意事项：

- **配对规则**：长期有效（无 TTL），但**任意一台设备连续 90 天没有成功同步/心跳**时，双方配对自动失效，再次使用需重新配对；`peers remove` 显式删除立即生效，并通过中继签发通知（离线对端上线后自动清理）。
- **撤销通知**：中继离线信箱保存的是删除方**签名**的控制面元数据（≤90 天，不含内容），对端验签通过才删除记录；中继无法伪造撤销。
- **无感剪切板**：桌面端默认自动监听/应用；Android 前台自动，后台无感需一次性 `adb shell appops set dev.drivesync.mobile READ_CLIPBOARD allow`；iOS 受系统限制只能前台自动 + 首次粘贴授权（详见移动端文档）。
- **中继默认同一设备只允许一个连接**：daemon 运行时执行 `pair`/`catalog fetch`/`vault` 等一次性命令会互踢；先停 daemon 或中继加 `--allow-multi-conn`（原型仅不踢旧连接，不真正并行）。
- `catalog fetch` 需要持有该软件且 daemon 在线的已配对设备。
- 数据目录优先级：`--data-dir` > `DRIVE_SYNC_DATA_DIR` > 平台默认（Windows `%APPDATA%\drive-sync`，Linux `~/.local/share/drive-sync`）。
- 命令细节以 `--help` 为准；自动更新签名/发布流程见 [docs/updates.md](docs/updates.md)。

## 移动端（原型）

- Android：安装 APK → 填自建 relay 地址/token → 配对 → 前台自动同步；后台无感需 adb appops 授权（测试用，见 [Android 文档](docs/mobile-android.md)）。Android 10+ 后台读剪贴板系统限制决定了必须这一步授权。
- iOS：Xcode 打开 `mobile/ios` 工程，用 Apple ID 自动签名安装到真机；或使用 CI 的模拟器包。iOS 16+ 读剪贴板需用户授权、无后台同步 API，见 [iOS 文档](docs/mobile-ios.md)。
- 移动端当前为原型：无系统推送（FCM/APNs）集成，App 需在前台保持连接（Android 可选前台服务）。

## 仓库结构

```
crates/
  core/          # 协议核心：身份、配对、E2E 会话、wire 类型、目录 CRDT、传输、暂存、撤销通知
  relay/         # 中继服务器（lib + bin，含撤销通知离线信箱）
  client-core/   # 客户端同步引擎：Store / Engine / WS 连接 / 传输收发 / spool / 活跃期
  cli/           # drive-sync 命令（桌面剪切板后端 + update 子命令）
  updater/       # 自动更新：签名清单、下载校验、自替换、sign-manifest
  mobile-core/   # 移动端 Rust FFI（C ABI + JNI）
  e2e-tests/     # 端到端测试与 drive-bench 延迟基准
mobile/android/  # Android 原型 App（Gradle/Kotlin）
mobile/ios/      # iOS 原型 App（SwiftUI + XcodeGen）
.github/workflows/  # CI / Release / Android / iOS 构建
docs/            # 架构、协议、平台调研、部署、更新、移动端、基准
deploy/          # Caddyfile / systemd unit / Dockerfile
scripts/         # 演示、基准、工具脚本
```

## 测试与基准

```bash
cargo test --workspace        # 全部单测 + 集成测试（含 e2e）
powershell -NoProfile -File scripts/bench.ps1   # 延迟/吞吐基准
powershell -NoProfile -File scripts/demo.ps1    # 一键演示（同机双设备）
```

实测（Windows 11 / i9-12900HX / 本机回环，详见 [docs/benchmark.md](docs/benchmark.md)）：1KiB 剪切板 E2E RTT p95 **0.218ms**、50MiB 分块吞吐 **108MB/s**（默认限流）、空载 102 连接内存 **27.5MiB**。

## 安全模型（摘要）

- 中继不可信：内容全部 E2E 加密，中继只看到路由元数据；撤销信箱仅存签名通知；
- 配对带外认证（二维码/SAS），防止中继 MITM；90 天不活跃自动过期降低僵尸配对风险；
- 连接级前向保密，重连即换新临时密钥；
- 云端暂存只存密文，密钥只在设备；TTL 到期物理删除；
- 自动更新清单 Ed25519 签名 + BLAKE3 校验，默认拒绝未签名更新；
- 已知限制：无消息级前向保密/Double Ratchet（v1.0 计划）；iOS 无法后台静默同步；Android 后台无感需一次性 adb 授权。

## 文档

- [架构方案](docs/architecture.md)
- [协议规范 v2](docs/protocol.md)
- [部署与运维](docs/deployment.md)
- [自动更新与发布流程](docs/updates.md)
- [桌面平台调研](docs/platform-desktop.md) / [移动端平台调研](docs/platform-mobile.md)
- [Android 安装与使用](docs/mobile-android.md) / [iOS 构建与安装](docs/mobile-ios.md)
- [性能基准报告](docs/benchmark.md)

## 路线图

- v0.1：Rust 核心 + 中继 + 桌面 CLI + 剪切板/目录/文件传输/暂存 + e2e（已完成）
- v0.2（当前）：项目更名；离线撤销通知信箱；90 天活跃期；自动更新；GitHub Actions 多平台产物；Android/iOS 原型 App
- v0.3：局域网 mDNS + QUIC 直连、二进制帧/CBOR、桌面托盘 GUI、systemd/自启动、移动端系统推送（FCM/APNs）
- v1.0：多用户、设备撤销广播/密钥轮换、Double Ratchet/后量子、自动更新灰度回滚

## License

MIT（见 [LICENSE](LICENSE)）
