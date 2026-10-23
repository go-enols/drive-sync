# drive-sync 基准与端到端验收报告（v0.2）

> 基准工具：`crates/e2e-tests/src/bin/bench.rs`（`drive-bench`）
> 端到端测试：`crates/e2e-tests/tests/*.rs`（`cargo test -p drive-e2e`）
> 本报告覆盖：延迟/吞吐/内存基准（中继默认限流与关闭限流两组口径）、v0.2 新增能力
> （90 天不活跃自动过期、离线撤销通知信箱、自动更新）的端到端验收、与架构文档指标的对照。
>
> 原始 JSON：`target/bench/bench-v02-default-limits.json`（默认限流）、
> `target/bench/bench-v02-limits-off.json`（关闭限流上限），每次运行生成、不入库。

## 0. 结论摘要

| 指标 | 架构文档目标 | 默认限流（生产默认） | 关闭限流（路径上限） | 结论 |
|---|---|---|---|---|
| 中继转发延迟（1KiB 文本，E2E RTT） | p95 < 30 ms | **p95 = 0.182 ms**（p50 0.126 / p99 0.210 / max 0.259） | p95 = 0.179 ms（p99 0.205） | **PASS**（约 165 倍余量） |
| 大文件吞吐（本机回环 50MB，64KiB 块） | > 20 MB/s | **113.68 MB/s**（windowed，端到端含 ack；wire 202.67 MB/s） | 116.71 MB/s windowed / 192.75 MB/s blast（无 ack） | **PASS**（约 5.7 倍余量） |
| 中继 RSS（空载，100 连接） | < 100 MB | **29.1 MiB**（进程合计口径；见 §4.3） | 26.6 MiB（同口径） | **PASS**（口径受限，见 §6.2） |
| 端到端测试（`cargo test -p drive-e2e`） | 全流程自动测试通过 | **26 passed / 0 failed**（连跑 2 次稳定） | — | **PASS** |
| 全仓回归（`cargo test --workspace`） | 全绿 | **222 passed / 0 failed**（31 个测试二进制） | — | **PASS** |
| 自动更新自测（`scripts/update-e2e.ps1`） | 端到端通过 | 未签名默认拒绝、签名 check/apply、BLAKE3/备份/篡改检测全部通过 | — | **PASS** |
| 一键演示（`scripts/demo.ps1`） | 端到端 0 失败 | **8 步全部通过（failures=0）** | — | **PASS** |

**关键结论**：
1. 中继默认限流（2000 帧/s、burst 4000、令牌不足先背压 ≤2s 不丢帧）下，50MiB windowed
   吞吐 113.68 MB/s、0 丢帧/0 `rate_limited`，**默认配置不是吞吐瓶颈**；瓶颈是
   JSON+base64 的 CPU 开销（wire/payload ≈ 1.78x）。
2. v0.2 协议（PROTOCOL_VERSION=2）新增 **90 天不活跃配对自动过期** 与 **签名撤销通知
   离线信箱**，e2e 已覆盖在线/离线/双向撤销、长时间离线补投递、启动清理与重新配对恢复。

## 1. 测试环境

| 项 | 值 |
|---|---|
| OS | Microsoft Windows 11 专业版中文版，10.0.26200 |
| CPU | 12th Gen Intel(R) Core(TM) i9-12900HX（16 物理核 / 24 逻辑核） |
| 内存 | 31.7 GB |
| 磁盘 | NTFS（仓库所在 D:） |
| 工具链 | rustc 1.96.0 (ac68faa20 2026-05-25) |
| 构建 profile | release（workspace：`lto = "thin"`、`codegen-units = 1`） |
| 运行时 | Tokio 1.x multi-thread；中继 axum 0.7；WS 客户端 tokio-tungstenite 0.24 |
| 网络 | 全部走 `127.0.0.1` 回环（中继监听 `127.0.0.1:0`，端口随机） |
| 时间 | 2026-10-24（Asia/Shanghai）；单次完整运行，非多轮中位数 |

构建使用项目本地 `CARGO_HOME=.cargo-home`（离线），见 `scripts/cargo.ps1` /
`scripts/bench.ps1`。本机只有 Windows PowerShell 5.1（无 `pwsh`），脚本保持 ASCII-only
以兼容 PS5.1（UTF-8 无 BOM 的中文字面量会被误读为 ANSI）。

## 2. 基准方法

### 2.1 拓扑

```text
┌──────────────┐   WS /ws（hello 认证）   ┌────────────────────────────┐   WS /ws   ┌──────────────┐
│ A（Identity）│ ───────────────────────> │ 进程内中继（内存转发，       │ <───────── │ B（Identity）│
│  bench 客户端 │ <─────────────────────── │ 不落盘、不解析 ciphertext）  │ ─────────> │  bench 客户端 │
└──────────────┘   relay{session_init,    └────────────────────────────┘   relay{…}  └──────────────┘
                     session_ack, relay}        同一进程内（bench 二进制）
```

- 中继用公开 API `drive_relay::{RelayConfig, start}` 在 `127.0.0.1:0` 启动；
  `RelayHandle.ws_url()` 得到 `ws://127.0.0.1:<port>/ws`。
- 两个客户端是**真实 WebSocket 连接**（tokio-tungstenite），各自用
  `Identity::generate` 生成 Ed25519+X25519 长期身份；Hello 认证与生产一致
  （`hello_sign_input` + Ed25519 签名，PROTOCOL_VERSION=2）。
- `PairRecord` 直接构造（跳过配对码/SAS，密码学正确性由 `drive-core` 单测覆盖）；
  E2E 会话走真实 `session_init`/`session_ack` 握手（经中继转发），后续业务消息为
  XChaCha20-Poly1305 `E2eEnvelope`。

### 2.2 1KiB 剪切板 E2E RTT

N=1000（前 50 次 warmup 不计入）：A 构造 `PeerMessage::Clipboard`（1024 字节文本，
低于压缩阈值）→ 加密 → 中继转发 → B 解密并校验 BLAKE3/内容 → B 加密回显 `Pong` →
中继 → A 解密校验。计时覆盖**完整双向往返**（两次加解密 + 两次中继转发）。
百分位用 nearest-rank。

### 2.3 64KiB 分块吞吐（50 MiB）

50 MiB 随机字节等分 64 KiB 块（800 块），每块 `PeerMessage::TransferChunk` 后 E2E
加密经中继转发，B 解密并校验 offset 连续。

| 口径 | 说明 |
|---|---|
| `windowed`（端到端） | 协议式流控：A 最多 32 块（2 MiB）在途，B 每 8 块（512 KiB）回 ack；耗时 = 开始发送 → 收到覆盖全部 50 MiB 的 ack（含 ack 往返）。两组限流口径都测。 |
| `blast_no_ack`（无 ack） | A 不读 ack、尽量快写入 WS；B 并发解密统计。明确标注**无应用层确认**，仅在关闭限流组测（默认配置下 800 帧会超出中继默认出站队列 512，属非协议用法）。 |

吞吐单位 MB/s 按 1 MiB = 1024×1024 字节、原始 payload 计；`wire_mbps` 为 WS 文本帧
字节（量化 JSON+base64 开销）。

### 2.4 连接数 / 内存

中继在**同一进程**内，无法单独读取"纯中继"RSS；采样 bench 进程 WorkingSet64 +
PrivateMemorySize64（PowerShell `Get-Process`）。采样点：中继启动前、2 连接基线、
吞吐后、100 条额外空闲已认证连接后；通过 `presence` 确认中继侧真实在线连接数
（观测 **102** = 2 基准连接 + 100 空闲）。

### 2.5 中继配置

| 配置 | 默认限流组（生产默认） | 关闭限流组 | 说明 |
|---|---|---|---|
| `rate_limit_per_sec` | **2000** | 0（关闭） | v0.2 默认 |
| `rate_limit_burst` | **4000** | 0 | ≈2s 帧流量 |
| `rate_limit_max_wait_ms` | **2000** | — | 令牌不足先背压等待，超时才丢帧 + `rate_limited` |
| `send_queue_capacity` | 512（默认） | 1024 | 仅关闭限流组的 blast 需要 |
| `idle_timeout_secs` | 90（默认） | 0 | 长基准避免空闲误断 |
| `data_dir` | **每个测试/基准唯一临时目录** | 同左 | v0.2 撤销通知离线信箱落盘位置；默认 `./drive-relay-state` 会污染仓库，e2e harness 已改为临时目录 |
| 其余（单帧 8 MiB、配对 TTL 120s、暂存限额/TTL、撤销通知限额 20/500） | 默认 | 默认 | 未调整 |

## 3. 基准结果

### 3.1 1KiB 剪切板消息 E2E RTT（N=1000，warmup=50）

| 指标 | 默认限流组 | 关闭限流组 |
|---|---|---|
| min | 0.100 ms | 0.095 ms |
| **p50** | **0.126 ms** | 0.124 ms |
| **p95** | **0.182 ms**（目标 <30ms，PASS） | 0.179 ms |
| **p99** | 0.210 ms | 0.205 ms |
| max | 0.259 ms | 0.277 ms |
| mean | 0.133 ms | 0.130 ms |
| E2E 握手（一次性） | 0.536 ms | 0.509 ms |

解读：一次往返含两次 WS 读写、两跳中继转发、两次 XChaCha20-Poly1305 加解密、两次
JSON 序列化/反序列化与 BLAKE3 校验。亚毫秒 p95 说明回环下协议/加密/中继转发不是
延迟瓶颈；真实场景 p95 由物理 RTT 与中继部署位置决定。

### 3.2 64KiB 分块吞吐（50 MiB = 800 块）

| 口径 | 应用层吞吐 | wire 吞吐 | 耗时 | 校验 |
|---|---|---|---|---|
| **默认限流** windowed | **113.68 MB/s** | 202.67 MB/s | 0.440 s | 800 块 offset 连续、BLAKE3 通过，0 丢帧/0 `rate_limited` |
| 关闭限流 windowed | **116.71 MB/s** | 208.08 MB/s | 0.428 s | 同上 |
| 关闭限流 blast（发送侧） | 192.75 MB/s | — | — | 0 丢帧 |
| 关闭限流 blast（接收侧观测） | 193.01 MB/s | — | — | 首次→最后一次收帧 |

- 默认限流与关闭限流两种口径几乎一致（113.68 vs 116.71 MB/s），**默认 2000 帧/s +
  背压已不再限制本机路径上限**；差异在测量抖动范围内。
- wire/payload ≈ 1.78x，与"密文 base64（×1.33）+ JSON 信封 base64（再 ×1.33）"
  理论值一致——这是架构 §6.2 计划用二进制帧/CBOR 的量化依据。
- 与 client-core 侧独立数据一致：release 下 6MiB 传输约 124~150ms（≈40-48MiB/s，
  256KiB 块）、64MiB 约 694ms（≈92MiB/s），均无 `rate_limited`。

### 3.3 内存 / 连接数（bench 进程合计口径）

| 阶段 | 默认限流组 WorkingSet / Private | 关闭限流组 |
|---|---|---|
| 中继启动前（仅 bench 二进制） | 6.7 / 2.5 MiB | 6.3 / 2.4 MiB |
| 2 条基准连接（hello+E2E 会话） | 11.4 / 3.0 MiB | 8.6 / 2.8 MiB |
| 吞吐后 | 13.2 / 5.1 MiB | 11.3 / 6.1 MiB |
| **102 条空闲已认证连接**（presence 观测 102） | **29.1 / 22.3 MiB** | 26.6 / 22.2 MiB |

- 空闲 100+ 连接时进程合计 WorkingSet 29.1 MiB < 100 MB 目标（口径注明）。
- 50 MiB windowed 传输期间 WorkingSet 仅 ~13 MiB，说明转发路径无整包缓冲放大。

## 4. v0.2 新增能力与端到端验收

### 4.1 90 天不活跃自动过期（`PairRecord.last_seen_ms`）

- `last_seen_ms` 在会话建立/收发任意 PeerMessage/心跳 Pong 时刷新（内存更新 + 每 60s
  或断开时 flush 到 peers.json）；`inactivity_expired(now)` 为 `now - last_seen > 90d`。
- e2e（`tests/e2e_inactive_expiry.rs`）：
  - 91 天：新 Runtime 启动即自动删除配对记录、emit `PeerExpired`（含 last_seen/expires_at）、
    不建立会话、剪切板不同步；重新配对后恢复；
  - 89 天 23 小时边界：仍有效、会话/同步正常、shutdown flush 后 last_seen 已刷新且不再临近过期；
  - `tests/e2e_pairing_permanence.rs`：`paired_at_ms` 拨到 10 年前但 last_seen 近期
    → 仍有效（证明配对时间本身没有 TTL，只有不活跃窗口）。
- 心跳（空闲 >6h 发 `PeerMessage::Ping`、Pong 刷新 last_seen）为 6h 级间隔，e2e 不便制造
  长窗口，由 client-core 单测覆盖；e2e 覆盖了 touch + flush 落盘路径。

### 4.2 离线撤销通知信箱（PROTOCOL_VERSION=2）

中继：`<data_dir>/revocations/index.json` 持久化签名通知（TTL ≤90d、单条 ≤8KiB、
每发送方 20 条/总 500 条限额），登录顺序 `welcome → pending_revocations → presence`，
`revocation_ack` 只允许目标设备删除，启动/后台清理过期。

- 协议级 e2e（`tests/e2e_revocation_mailbox.rs`，5 用例）：B 离线投递→落盘→登录收到→
  验签→ack→索引删除→再登录为空；B 在线立即推送、未 ack 前重复登录重复投递（幂等）；
  非目标设备收不到且其 ack 不删除；篡改签名/第三方转发/过期/超大被拒；中继同 data_dir
  重启后 pending 仍在；启动清理过期通知。
- 客户端级 e2e（`tests/e2e_client_offline_revoke.rs`）：A 运行时检测外部删除 B →
  `RevocationQueued`→`RevocationDelivered`（入 relay 信箱）→ B 上线收到
  `PeerRevocationNotice`、删除本地记录并 ack → relay 索引清空 → 撤销后剪切板不同步 →
  重新配对恢复。
- 在线撤销/双向撤销/30 天之外等见 `tests/e2e_pair_revoke.rs`（3 用例，≤5s 生效断言）。

### 4.3 自动更新

- `scripts/update-e2e.ps1`：临时密钥 + `file://` 清单，验证未签名默认拒绝、
  `sign-manifest` 签名后 `update check` 通过、`update apply` 下载/校验/BLAKE3/替换/
  `.old-*` 备份、篡改清单拒绝、`--allow-unsigned` 仅本地可用。实测全部通过。
- 自动更新库单测 27 个在 `cargo test --workspace` 内全绿。

## 5. 端到端测试覆盖（`cargo test -p drive-e2e`：26 passed / 0 failed）

| 测试文件 | 用例数 | 覆盖 |
|---|---|---|
| `e2e_handshake.rs` | 1 | 真实 WS + hello 认证 + 经中继 E2E 握手 + 1KiB 剪切板双向 10 轮 |
| `e2e_pairing_clipboard.rs` | 1 | 非交互配对 → 双 Runtime → 剪切板双向 + 回环抑制/去重 |
| `e2e_pairing_permanence.rs` | 2 | 配对时间无 TTL（paired_at 10 年前仍有效）；重复配对幂等 + remove 幂等 |
| `e2e_catalog_transfer.rs` | 1 | 目录快照/增量/墓碑 + fetch 2MiB + BLAKE3 |
| `e2e_multi_device.rs` | 1 | 3 设备（A-B、A-C 配对）；B/C **并发 fetch 同一 entry** + BLAKE3 |
| `e2e_reconnect_spool.rs` | 2 | 离线 spool 补发；中继 shutdown(Close 1001)→同地址重启→客户端不重启自动重连 |
| `e2e_vault_share.rs` | 1 | vault put → E2E vault_share → 对端解密 BLAKE3；list 仅本设备可见 |
| `e2e_pair_revoke.rs` | 3 | 在线撤销（≤5s、PairRevoked 传播、双向同步停）；离线撤销后需重新配对；双向撤销 + 幂等 |
| `e2e_inactive_expiry.rs` | 2 | 90 天不活跃自动过期/事件/需重新配对；89d23h 边界仍有效且刷新 last_seen |
| `e2e_revocation_mailbox.rs` | 5 | 协议级离线信箱：投递/登录接收/ack 删除/在线推送/重启持久化/启动清理/伪造拒绝 |
| `e2e_client_offline_revoke.rs` | 1 | 客户端级离线撤销端到端：入信箱 → 对端上线自动清理并 ack → 重新配对恢复 |
| `relay_vault_ttl.rs` | 4 | vault TTL clamp、密文落盘、删除、启动清理、后台 reaper、跨设备 get/owner delete |
| `src/lib.rs` 单测 | 2 | 延迟统计/文本载荷辅助 |

说明：临时文件/中继状态目录全部使用 `tempfile` + harness 唯一 `RelayConfig.data_dir`，
测试不污染仓库（`drive-relay-state/` 已在 .gitignore）。e2e 使用库 API/Runtime，
不依赖 CLI 子进程 daemon。

### 5.1 暂存 TTL 的 60s 下限（协议 vs 任务描述）

协议规定服务端把 TTL 强制 clamp 到 60s~30d，任务书"put ttl=1s → sleep 2s → 过期"无法
经协议实现；`relay_vault_ttl.rs` 用等价语义覆盖：clamp 断言、密文落盘、显式删除、
停机后白盒拨快 index 的 `expires_at_ms` 验证启动清理/后台 reaper。

## 6. 已知限制与剩余风险

1. **内存口径**：中继与客户端同进程，RSS 为进程合计（上限近似）；无独立中继进程 RSS。
2. **单次运行**：基准未做多轮中位数；CPU 调度/后台负载可带来 10%~20% 波动。debug 构建
   （`cargo run` 默认）RTT p95 ≈2.1ms、吞吐 ≈3.4MB/s，仅用于功能验证。
3. **blast 口径**：无应用层 ack，发送侧吞吐不代表端到端；默认限流组不测 blast（超出
   默认出站队列，非协议用法）。
4. **时序敏感测试**：撤销生效断言"≤5s"（检测周期 2s + 余量）在极度过载 CI 上可能偶发；
   本地连跑 2 次全绿。90 天过期用例通过改 `last_seen_ms` 白盒构造，心跳 6h 间隔由单测覆盖。
5. **跨平台**：测试断言未使用 Windows 专有行为（路径/大小写/文件锁），但 Windows 文件
   锁/CI 调度差异可能导致撤销类用例需要放宽等待窗口；建议 CI 串行跑 e2e、不与构建并行。
6. **`RelayHandle::shutdown()`** 会向在线连接发 Close(1001) 并等待注销；`shutdown_signal()`
   只停止 accept（rustdoc 已注明）。
7. 自动更新在真实 GitHub Release 上的签名清单由 CI `UPDATE_SIGNING_KEY` 生成，本机只做
   `file://` 端到端自测。

## 7. 复现

```powershell
# 全部 e2e 测试（可连跑多次验证稳定性）
powershell -NoProfile -File scripts/bench.ps1 -SkipTests   # 或 cargo test -p drive-e2e --offline
cargo test --workspace --offline

# 默认限流（生产默认）release 基准
$env:CARGO_HOME="D:\Code\derive-sync\.cargo-home"
cargo run --release -p drive-e2e --bin drive-bench --offline -- `
    --default-limits --json-out target/bench/bench-v02-default-limits.json

# 关闭限流（路径上限）release 基准
cargo run --release -p drive-e2e --bin drive-bench --offline -- `
    --json-out target/bench/bench-v02-limits-off.json

# 快速冒烟（N=100，5MiB，20 连接） / 自动更新自测 / 一键演示
cargo run --release -p drive-e2e --bin drive-bench --offline -- --quick
powershell -NoProfile -File scripts/update-e2e.ps1
powershell -NoProfile -File scripts/demo.ps1
```

`drive-bench --help` 查看全部参数（`--default-limits`、`--quick`、`--skip-rss`、
`--print-json` 等）。JSON 字段：`rtt_1kb`、`throughput.{windowed,blast_no_ack}`、
`memory.idle_connections`、`relay.config`、`verdict`。

## 8. 变更记录

- 2026-10-24（v0.2）：项目改名 drive-*；协议 v2（90 天不活跃过期 + 离线撤销通知信箱）；
  更新 e2e 覆盖到 26 用例（含信箱 5、不活跃 2、客户端离线撤销 1），workspace 222 测试全绿；
  重新跑默认限流/关闭限流两组基准并更新数字；补充自动更新与 demo 验收结果。
- 2026-10-23（v0.1）：初版（RTT/吞吐/内存基准 + vault TTL + 阶段 2 端到端）。
