# drive-sync

自托管的多设备同步系统：**共享剪切板** + **软件目录表**（软件本体留在本地，云端只中转不存储）+ 可选 **30 天加密云端暂存**。支持 Windows / Linux / Android / iOS / macOS，中继服务器端到端加密、零内容存储，个人 / 小团队自建。

> 当前状态：**v0.1 原型已完成并验证**。Rust 协议核心、中继服务器、桌面客户端（Windows/Linux CLI）、剪切板同步、软件目录 CRDT、软件本体点对点中转、30 天加密暂存、配对、断线重连与端到端测试均已实现；Android/iOS 为方案级支持（见 [移动端平台调研](docs/platform-mobile.md)）。实测指标见 [基准报告](docs/benchmark.md)。

## 为什么不用现成方案

- 现有剪切板同步工具大多把内容存进云端，或要求登录第三方账号；
- 软件/安装包分发通常要上传网盘或服务器，隐私和带宽都不划算；
- 跨平台（尤其 iOS/Android 后台限制）几乎没有"无感双向同步"的通用解。

drive-sync 的做法：**中继只是一台不存内容的交换机**；所有剪切板/目录/文件内容在设备之间端到端加密直传。软件目录只同步元数据，点"获取"时由持有该软件的设备把本体加密传给你。移动端用系统推送唤醒 + 拉取 + 用户确认，遵守平台后台限制。

## 核心特性

| 能力 | 说明 |
|---|---|
| 剪切板同步 | 文本 / 富文本 / PNG 图片；zstd 压缩、BLAKE3 去重、回环抑制、离线补发；每设备/每类型可开关 |
| 软件目录 | 仅同步元数据（名称/版本/平台/大小/BLAKE3）；HLC LWW CRDT + 墓碑，离线可合并 |
| 软件本体中转 | 设备间 E2E 分块流式传输（64KiB 块、流控、断点续传、整体 BLAKE3 校验）；中继零落盘 |
| 首次配对 | 一次性 8 位码/二维码 + 6 位 SAS 防 MITM；**配对永久有效（无 TTL）**，离线多久/重启/换网络/中继重启都无需重新配对；只有任一方显式 `peers remove` 才失效 |
| 可选云端暂存 | 客户端随机密钥加密后上传；服务端 TTL 强制 ≤ 30 天并定时删除；密钥只在设备端 |
| 安全 | Ed25519 身份、X25519 连接级前向保密、XChaCha20-Poly1305、零信任中继 |
| 跨平台 | Rust 核心复用；桌面 CLI 已实现，移动端方案见文档 |

## 架构一览

```
设备 A ──wss──┐
设备 B ──wss──┼──► 自建中继（只转发密文、不落盘） ──► 设备 C
设备 D ──wss──┘        └─ 可选：加密暂存 30 天 TTL
```

详细设计见 [docs/architecture.md](docs/architecture.md)，协议规范见 [docs/protocol.md](docs/protocol.md)。

## 快速开始（Windows / Linux）

前置：Rust 1.80+（本项目在 1.96 验证）。默认构建使用 `ws://` 便于本机/内网测试；公网部署请用反代终止 TLS 后客户端连 `wss://`（需 `cargo build --release --features tls` 或对应 crate feature，见下）。仓库内 `.cargo/config.toml` 只是本机镜像失效时的临时覆盖，网络正常时可删除。

```bash
# 1. 编译（只编中继与客户端）
cargo build --release -p drive-relay -p drive-cli

# 2. 启动中继（自建 VPS 或本机；token 也可用环境变量 DRIVE_RELAY_TOKEN）
./target/release/drive-relay --bind 127.0.0.1:8787 --token <强随机令牌>

# 3. 每台设备初始化身份（数据目录默认在用户目录；--data-dir 可指定）
./target/release/drive-sync init --name "我的笔记本"

# 4. 配对：A 端发起（打印二维码串 + 8 位配对码 + 等待加入），B 端输入配对码或扫二维码
#    A 端：
./target/release/drive-sync pair --relay ws://127.0.0.1:8787/ws --token <令牌>
#    B 端（二选一）：
./target/release/drive-sync pair --relay ws://127.0.0.1:8787/ws --token <令牌> --code 12345678
./target/release/drive-sync pair --relay ws://127.0.0.1:8787/ws --token <令牌> --qr "dsync-pair-v1:..."
#    两端比对 6 位 SAS 一致后确认（脚本/测试可加 --yes 自动确认）

# 5. 常驻同步（Ctrl-C 退出；服务化见部署文档）
./target/release/drive-sync daemon --relay ws://127.0.0.1:8787/ws --token <令牌>
#    也可只用文件模拟剪切板做同机双实例测试：--clipboard file:/tmp/clip-a.txt

# 6. 软件目录（add 只登记元数据+本地路径；本体不上传）
./target/release/drive-sync catalog add --path ./setup.exe --name "某工具" --version 1.2.3 --platform windows --kind installer
./target/release/drive-sync catalog list
#    在另一台设备（其 daemon 需在线）按 entry_id 拉取本体：
./target/release/drive-sync catalog fetch <entry_id> --out ./downloads

# 7. 可选：加密云端暂存（默认 24h，最长 30 天；daemon 在线时会自动把密钥分享给已配对设备）
./target/release/drive-sync vault put --path ./big.iso --ttl-days 7
./target/release/drive-sync vault list
./target/release/drive-sync vault get <object_id> --out ./big.iso

# 8. 配对管理（配对永久有效；只有显式删除才会失效）
./target/release/drive-sync peers list
./target/release/drive-sync peers remove <device_id>   # 撤销信任：对方无法再与本机同步
```

注意事项：

- **配对永久有效**：配对记录保存在本地 `peers.json`，没有过期时间；离线多久、重启、换网络、中继重启、升级应用（保留数据目录）都不需要重新配对。唯一失效方式是任一方执行 `peers remove`；删除 `identity.json` 等于换新设备身份，所有配对都需要重来。
- 撤销是**单向即时生效**的：删除方立刻拒绝对方；若对方在线会尽力收到 `PairRevoked` 并自动清理记录；若对方离线，其后续同步会失败（安全兜底），需要双方删除后重新配对。
- `peers list / remove` 是纯本地操作，daemon 运行时也可用（daemon 会在 2s 内检测到并实时断开对应会话）。
- **中继默认同一设备只允许一个连接**：daemon 运行时执行 `pair`/`catalog fetch`/`vault` 等一次性命令会互踢连接。要么先停 daemon，要么给中继加 `--allow-multi-conn`（原型下不真正支持并行多连接，仅不踢旧连接）。
- `catalog fetch` 需要**持有该软件且 daemon 在线**的已配对设备；若对方离线会明确失败。
- `wss://` 需启用 TLS feature（`drive-client`/`drive-cli` 的 `tls` feature，依赖 rustls-webpki-roots）；本仓库默认构建为 `ws://`。
- 数据目录优先级：`--data-dir` > `DRIVE_SYNC_DATA_DIR` > 平台默认（Windows `%APPDATA%\drive-sync`，Linux `~/.local/share/drive-sync`）。
- 命令细节以 `--help` 为准。

## 仓库结构

```
crates/
  core/          # 协议核心：身份、配对、E2E 会话、wire 类型、目录 CRDT、传输、暂存类型
  relay/         # 中继服务器（lib + bin）
  client-core/   # 客户端同步引擎：Store / Engine / WS 连接 / 传输收发 / spool
  cli/           # drive-sync 命令行（含 Windows/Linux 剪切板后端）
  e2e-tests/     # 端到端测试与 drive-bench 延迟基准
docs/
  architecture.md        # 架构方案
  protocol.md            # 协议规范 v0.1
  platform-desktop.md    # Windows/Linux 平台调研（剪切板/保活/功耗/打包）
  platform-mobile.md     # Android/iOS 平台调研（剪切板/推送/后台限制/合规）
deploy/                  # Caddyfile / systemd unit / Dockerfile
scripts/                 # 演示与工具脚本
```

## 测试与基准

```bash
cargo test --workspace        # 全部单测 + 集成测试（含 e2e）
# 延迟/吞吐基准（release，脚本自动选 pwsh/powershell）
powershell -NoProfile -File scripts/bench.ps1
# 一键演示：同机双设备（file 模式剪切板）+ 中继 + 配对 + 同步
powershell -NoProfile -File scripts/demo.ps1
```

实测（Windows 11 / i9-12900HX / 本机回环，详见 [docs/benchmark.md](docs/benchmark.md)）：

| 指标 | 目标 | 实测 |
|---|---|---|
| 1KiB 剪切板 E2E RTT p95 | < 30ms | **0.27ms** |
| 64KiB 分块 50MiB 吞吐（默认限流） | > 20MB/s | **~90-110MB/s** |
| 空载 100 连接内存（含中继+客户端进程内） | < 100MB | **~29MiB** |

## 安全模型（摘要）

- 中继不可信：内容全部 E2E 加密，中继只看到路由元数据；
- 配对带外认证（二维码/SAS），防止中继 MITM；
- 连接级前向保密，重连即换新临时密钥；
- 云端暂存只存密文，密钥只在设备；TTL 到期物理删除；
- 已知限制：v0.1 无消息级前向保密/Double Ratchet（v1.0 计划）；移动端无法无感后台同步（平台限制）。

## 文档

- [架构方案](docs/architecture.md)
- [协议规范](docs/protocol.md)
- [桌面平台实现调研](docs/platform-desktop.md)
- [移动端实现调研](docs/platform-mobile.md)
- [部署与运维](docs/deployment.md)
- [性能基准报告](docs/benchmark.md)

## 路线图

- v0.1（当前）：Rust 核心 + 中继 + 桌面 CLI + 剪切板/目录/文件传输/暂存 + e2e 测试
- v0.2：局域网 mDNS + QUIC 直连、二进制帧、托盘 GUI、systemd/自启动、Docker
- v0.3：Android/iOS App（推送→拉取→确认链路）、国内厂商推送
- v1.0：多用户、设备撤销/密钥轮换、Double Ratchet/后量子、自动更新

## License

MIT
