# drive-sync 基准测试报告（v0.1 原型）

> 工具：`crates/e2e-tests/src/bin/bench.rs`（`drive-bench`）
> 端到端测试：`crates/e2e-tests/tests/*.rs`（`cargo test -p drive-e2e`）
> 本报告包含：延迟/吞吐/内存基准（中继默认限流与关闭限流两组口径）+ 阶段 2 双/多设备
> 端到端测试覆盖清单 + 与架构文档验收指标的对照。
>
> 原始 JSON：`target/bench/bench-default-limits.json`（默认限流）、
> `target/bench/bench-limits-off.json`（关闭限流上限），每次运行生成、不入库。

## 0. 结论摘要

| 指标 | 架构文档目标 | 默认限流（生产默认） | 关闭限流（路径上限） | 结论 |
|---|---|---|---|---|
| 中继转发延迟（1KiB 文本，E2E RTT） | p95 < 30 ms | **p95 = 0.218 ms**（p50 0.139 / p99 0.257 / max 0.380） | **p95 = 0.209 ms**（p99 0.270） | **PASS**（约 137 倍余量） |
| 大文件吞吐（本机回环 50MB，64KiB 块） | > 20 MB/s | **108.08 MB/s**（windowed，端到端含 ack；wire 192.69 MB/s） | **110.01 MB/s** windowed / **195.78 MB/s** blast（无 ack） | **PASS**（约 5.4 倍余量） |
| 中继 RSS（空载，100 连接） | < 100 MB | **27.5 MiB**（进程合计口径；见 §4.3） | 31.8 MiB（同口径） | **PASS**（口径受限，见 §5.2） |
| E2E 握手（session_init/session_ack 经中继） | 无明确指标 | 0.496 ms | 0.561 ms | 参考 |
| 端到端测试 | 全流程自动测试通过 | `cargo test -p drive-e2e`：**26 passed / 0 failed**（含撤销信箱 5、客户端离线撤销 1、90 天过期 2、配对永久性/边界 2） | — | **PASS** |

**关键结论（限流口径）**：中继默认限流已从"200 帧/s + 超限丢帧"调整为
**2000 帧/s、burst 4000、令牌不足先背压等待（最多 2s）不丢帧**（协议 §4.5 同步）。
默认配置下 64KiB 块的理论持续容量 ≈ `2000 × 64KiB = 125 MiB/s`，高于本机实测路径
上限 ~108 MB/s，**默认限流不再是大文件吞吐瓶颈**；瓶颈是 JSON+base64 的 CPU 开销
（wire/payload ≈ 1.78x）。默认配置组 windowed 50MiB 全程 0 丢帧、0 `rate_limited`。

## 1. 测试环境

| 项 | 值 |
|---|---|
| OS | Microsoft Windows 11 专业版中文版，10.0.26200 |
| CPU | 12th Gen Intel(R) Core(TM) i9-12900HX（16 物理核 / 24 逻辑核） |
| 内存 | 31.7 GB |
| 磁盘 | NTFS（仓库所在 D:） |
| 工具链 | rustc 1.96.0 (ac68faa20 2026-05-25)，cargo 同版本 |
| 构建 profile | release（workspace：`lto = "thin"`、`codegen-units = 1`） |
| 运行时 | Tokio 1.x multi-thread；中继 axum 0.7；WS 客户端 tokio-tungstenite 0.24 |
| 网络 | 全部走 `127.0.0.1` 回环（中继监听 `127.0.0.1:0`，端口随机） |
| 时间 | 2026-10-23/24（Asia/Shanghai）；单次完整运行，非多轮中位数 |

构建使用项目本地 `CARGO_HOME=.cargo-home`（离线），见 `scripts/cargo.ps1` /
`scripts/bench.ps1`。注意本机只有 Windows PowerShell 5.1（无 `pwsh`），脚本保持
ASCII-only 以兼容 PS5.1（UTF-8 无 BOM 的中文字面量会被误读）。

## 2. 测试方法

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
  （`hello_sign_input` = `b"drive-sync-hello-v1" || nonce || device_id` + Ed25519 签名）。
- `PairRecord` 直接构造（跳过配对码/SAS，密码学正确性由 `drive-core` 单测覆盖）；
  E2E 会话走真实 `session_init`/`session_ack` 握手（经中继转发），后续业务消息为
  XChaCha20-Poly1305 `E2eEnvelope`（AAD 绑定 v/session/seq/kind/ts）。

### 2.2 1KiB 剪切板 E2E RTT

每次测量（N=1000，前 50 次 warmup 不计入）：A 构造 `PeerMessage::Clipboard`
（`ClipboardPayload::text` 填充 1024 字节 ASCII，低于 2KiB 压缩阈值）→ 加密 →
中继转发 → B 解密并校验 BLAKE3/内容 → B 加密回显 `Pong` → 中继 → A 解密校验。
计时覆盖**完整双向往返**（含两次加解密与两次中继转发）。百分位用 nearest-rank。

### 2.3 64KiB 分块吞吐（50 MiB）

50 MiB 随机字节等分 64 KiB 块（800 块），每块 `PeerMessage::TransferChunk` 后 E2E
加密经中继转发，B 解密并校验 offset 连续。两种口径：

| 口径 | 说明 |
|---|---|
| `windowed`（端到端） | 协议式流控：A 最多 32 块（2 MiB）在途，B 每 8 块（512 KiB）回 `TransferAck`；耗时 = 开始发送 → 收到覆盖全部 50 MiB 的 ack。这是**真实传输协议**的吞吐（含 ack 往返），两组限流口径都测。 |
| `blast_no_ack`（无 ack） | A 不读 ack、把所有块尽量快写入 WS；B 并发解密统计。报告发送侧/接收侧两个速率，明确标注**无应用层确认**。仅在关闭限流组测量：默认配置下 800 帧会超出中继默认出站队列（512），属非协议用法，不代表生产行为。 |

吞吐单位 MB/s 按 1 MiB = 1024×1024 字节、原始 payload 计；`wire_mbps` 为 WS 文本
帧字节（量化 JSON+base64 开销）。

### 2.4 连接数 / 内存

中继在**同一进程**内，无法单独读取"纯中继"RSS；采样 bench 进程 WorkingSet64 +
PrivateMemorySize64（PowerShell `Get-Process`）。采样点：中继启动前、2 连接基线、
吞吐后、100 条额外空闲已认证连接后；通过 `presence` 确认中继侧真实在线连接数
（观测 **102** = 2 基准连接 + 100 空闲），验证连接确实建立。

### 2.5 中继配置（两组口径）

| 配置 | 协议默认 / 默认限流组 | 关闭限流组 | 说明 |
|---|---|---|---|
| `rate_limit_per_sec` | **2000**（默认） | 0（关闭） | 默认限流组即生产默认值 |
| `rate_limit_burst` | **4000**（默认） | 0 | burst 4000 ≈ 2s 帧流量 |
| `rate_limit_max_wait_ms` | **2000**（默认） | — | 令牌不足先背压等待（TCP 窗口自然减速发送端），超过才丢帧 + `rate_limited` |
| `send_queue_capacity` | 512（默认） | 1024 | 仅关闭限流组的 blast 需要；避免出站队列满导致的非协议丢帧 |
| `idle_timeout_secs` | 90（默认） | 0 | 长基准避免空闲误断 |
| 其余（单帧 8 MiB、配对 TTL 120s、暂存限额/TTL） | 默认 | 默认 | 未调整 |

启动方式：
- 默认限流组：`drive-bench --default-limits`（直接使用 `RelayConfig::default()` 限流参数）；
- 关闭限流组：`drive-bench`（关闭限流测路径上限，历史口径，便于对照）。

## 3. 结果

### 3.1 1KiB 剪切板消息 E2E RTT（N=1000，warmup=50）

| 指标 | 默认限流组 | 关闭限流组 |
|---|---|---|
| min | 0.102 ms | 0.093 ms |
| **p50** | **0.139 ms** | 0.133 ms |
| **p95** | **0.218 ms**（目标 <30ms，PASS） | 0.209 ms |
| **p99** | 0.257 ms | 0.270 ms |
| max | 0.380 ms | 0.341 ms |
| mean | 0.148 ms | 0.142 ms |
| E2E 握手（一次性） | 0.496 ms | 0.561 ms |

解读：一次往返含两次 WS 读写、两跳中继转发、两次 XChaCha20-Poly1305 加解密、两次
JSON 序列化/反序列化与 BLAKE3 校验。亚毫秒 p95 说明回环下**协议/加密/中继转发不是
延迟瓶颈**；真实场景 p95 由物理 RTT 与中继部署位置决定（架构文档：同城 <150ms、
跨洲 <400ms）。

### 3.2 64KiB 分块吞吐（50 MiB = 800 块）

| 口径 | 应用层吞吐 | wire 吞吐 | 耗时 | 校验 |
|---|---|---|---|---|
| **默认限流** windowed（32 块窗口+每 8 块 ack） | **108.08 MB/s** | 192.69 MB/s | 0.463 s | 800 块 offset 连续、BLAKE3 通过，0 丢帧、0 `rate_limited` |
| 关闭限流 windowed | **110.01 MB/s** | 196.12 MB/s | 0.455 s | 同上 |
| 关闭限流 blast（无 ack，发送侧） | **195.78 MB/s** | 309 MB/s（同前次运行） | 0.29 s | 800/800 帧，0 丢帧，`transport_errors: []` |
| 关闭限流 blast（接收侧观测） | 196.01 MB/s | — | — | 首次→最后一次收帧 |

- 默认限流与关闭限流两种口径下 windowed 吞吐几乎一致（108 vs 110 MB/s），说明
  **默认 2000 帧/s + 背压已不再限制本机路径上限**；差异在测量抖动范围内。
- wire/payload ≈ 1.78x，与"E2E 密文 base64（×1.33）+ JSON 信封 base64（再 ×1.33）"
  理论值一致——这是架构 §6.2 计划用二进制帧/CBOR 的量化依据。
- 与 client-core 侧独立冒烟数据一致：release 下 6MiB 传输约 124~150ms（≈40-48MiB/s，
  256KiB 块）、64MiB 约 694ms（≈92MiB/s），均无 `rate_limited`（client 有 32 块窗口流控）。

### 3.3 内存 / 连接数（bench 进程合计口径）

| 阶段 | 默认限流组 WorkingSet / Private | 关闭限流组 |
|---|---|---|
| 中继启动前（仅 bench 二进制） | 6.3 / 2.4 MiB | 6.3 / 2.4 MiB |
| 2 条基准连接（hello+E2E 会话） | 8.5 / 2.8 MiB | 8.4 / 2.7 MiB |
| 吞吐后 | 9.7 / 3.9 MiB | 10.0 / 4.4 MiB |
| **102 条空闲已认证连接**（presence 观测 102） | **27.5 / 24.7 MiB** | 31.8 / 29.7 MiB |

- 空闲 100+ 连接时进程合计 WorkingSet 27.5 MiB < 100 MB 目标（口径注明）。
- 每连接增量约 (27.5−8.5)/100 ≈ 0.19 MiB（含 WS 缓冲、Task、mpsc 队列与 bench 客户端
  自身结构）；纯中继侧应更低。
- 50 MiB windowed 传输期间 WorkingSet 仅 ~10 MiB，说明转发路径无整包缓冲放大。

## 4. 分析

### 4.1 延迟

p95 0.22ms、p99 0.26ms，均在亚毫秒级；v0.1 的"JSON + base64 + 内存中继"在回环
网络下完全不是延迟瓶颈。真实使用中 p95 由物理 RTT 与中继位置决定。

### 4.2 吞吐

单条 WS 连接 64KiB 分块流水线的带 ack 真实协议吞吐 ~108 MB/s（wire ~193 MB/s），
远高于 20MB/s 目标；限制因素是 JSON/base64 的 CPU 开销而非 TCP/中继。默认限流
（2000 帧/s ≈ 125 MiB/s 容量）已高于该路径上限，因此默认配置不再需要为吞吐调参；
生产 WAN 场景仍受 VPS 带宽与物理 RTT 限制（架构文档：WAN ≥5MB/s）。

### 4.3 内存口径说明

中继与两个客户端在同进程，`Get-Process` 只能读进程合计内存：27.5 MiB 含中继 +
100 个 bench 客户端连接 + Tokio 运行时 + bench 框架，是"纯中继 RSS <100MB"的
**上限近似**；真实独立中继进程应低于此值。精确值需独立进程部署测量（见 §6）。

### 4.4 限流口径（已解决）

阶段 1 曾发现"默认 200 帧/s 使 64KiB 分块持续吞吐上限 ≈12.5 MiB/s，与 20MB/s
目标冲突，且超限丢帧"。经 lead 决策与 server-impl 实现，中继默认已改为
**2000 帧/s、burst 4000、令牌不足先背压等待（≤2s）不丢帧，超时才 `rate_limited`**
（协议 §4.5 同步）；client-core 出站默认 1900 帧/s + 32 块窗口流控。结果：
- 默认配置下理论容量 `2000×64KiB = 125 MiB/s`，高于本机路径上限 ~108MB/s；
- 默认限流组 windowed 50MiB 实测 108.08 MB/s、0 丢帧/0 `rate_limited`；
- 结论：**默认配置已不与架构吞吐目标冲突**，无需为吞吐关闭限流。

## 5. 端到端测试覆盖（`cargo test -p drive-e2e`：26 passed / 0 failed）

| 测试文件 | 用例 | 覆盖 |
|---|---|---|
| `e2e_handshake.rs` | `two_clients_handshake_and_clipboard_roundtrip` | 真实 WS + hello 认证 + 经中继 session 握手 + 1KiB 剪切板双向 10 轮（底层协议冒烟） |
| `e2e_pairing_clipboard.rs` | `pairing_clipboard_sync_dedupe_and_loopback` | 非交互配对（SAS 自动确认）→ 双 Runtime → 剪切板 A→B/B→A、**回环抑制**（B 重复注入同内容不回传 A）、去重不误伤新内容 |
| `e2e_catalog_transfer.rs` | `catalog_snapshot_delta_tombstone_and_fetch_with_blake3` | B 离线时 A catalog add → B 上线收**快照**；A 再加条目走**增量**；B fetch 2MiB 文件校验 BLAKE3/字节一致；A 删除→**墓碑**同步、visible 列表剔除 |
| `e2e_multi_device.rs` | `three_devices_clipboard_and_concurrent_fetch` | 3 设备（A-B、A-C 配对，B-C 未配对不建会话）；剪切板 A→B/C；**B、C 并发 fetch 同一 entry**（client 并发 bug 回归），两份文件 BLAKE3 均一致 |
| `e2e_reconnect_spool.rs` | `spool_replay_after_peer_online` | B 离线时 A 写剪切板→spool；B 上线后 `SpoolReplayed` + 内容补发 |
| 同上 | `relay_restart_transparent_reconnect` | 中继 `shutdown()`（向在线连接发 Close 1001）→ 同地址重启 → **客户端不重启自动重连**、重建 E2E 会话、双向剪切板继续同步 |
| `e2e_vault_share.rs` | `vault_put_share_and_get_decrypt_on_peer` | A `vault_put`（TTL clamp 断言）→ 会话建立后 E2E `vault_share` 共享密钥（B 收到 `VaultKeyReceived`）→ B 拉取解密 BLAKE3 一致；`vault_list` 仅本设备可见 |
| `e2e_pairing_permanence.rs` | 2 个用例 | 配对长期有效（`paired_at` 10 年前 + `last_seen` 近期仍有效）；重复配对幂等 |
| `e2e_inactive_expiry.rs` | 2 个用例 | `last_seen` 91 天前 → 启动自动生成 inactive_90d 通知并删除记录、需重新配对；89 天 23 小时边界仍有效且会话刷新活跃时间 |
| `e2e_revocation_mailbox.rs` | 5 个用例 | 撤销通知离线信箱：B 离线 put → B 上线 pending → ack 删除；在线立即投递/未 ack 重投；非目标 ack 不删；伪造/过期/超大拒绝；中继重启 pending 保留 |
| `e2e_client_offline_revoke.rs` | 1 个用例 | 客户端视角：A 删除离线 B → 通知投递 relay → B 上线验签删除记录并 ack → 双方 peers 清空、重新配对恢复 |
| `e2e_pair_revoke.rs` | 3 个用例 | 在线撤销（A 外部删除 peers.json → ≤2s 会话销毁 + PairRevoked → B 自动删记录；撤销后双向同步阻断）；离线撤销（A 停机删除 → B 握手被拒）；重新配对后恢复同步 |
| `relay_vault_ttl.rs` | 4 个用例 | 见 §5.1 |

说明：同 device_id 新连接踢旧连接导致的透明重连路径由 `relay_restart_transparent_reconnect`
（Close 1001 断线 + 自动重连）与 client-core 单测共同覆盖；中继重启测试要求
`RelayHandle::shutdown()` 关闭在线连接（server-impl 已修复并加 relay 回归测试）。

### 5.1 暂存 TTL 的 60s 下限（任务书 vs 协议）

协议 §4.5/§7.4 规定服务端把 TTL **强制 clamp 到 60s~30d**，任务书"put ttl=1s →
sleep 2s → 过期"无法经协议实现。`relay_vault_ttl.rs` 用等价语义覆盖（全部走真实 WS）：
1. `ttl=1` 上传 → 断言 `expires_at_ms ≥ now+60s`（clamp）→ get 成功 → 落盘只有密文
   （逐字节等于客户端密文，无明文）→ 显式删除后文件消失、`vault_not_found`；
2. 停机后白盒把 index 的 `expires_at_ms` 拨到过去 → 重启验证**启动清理**删除对象文件；
3. 到期时间设 2.5s 后、1s 清理间隔重启 → 验证**后台 reaper** 自动删除（轮询覆盖
   "sleep 后过期"语义）；
4. B 凭 object_id 可跨设备 get（密钥经 E2E 共享），只有上传者能 delete（`protocol_error`）；
   未启用暂存时返回 `vault_disabled`。

## 6. 复现

```powershell
# 全部 e2e 测试
powershell -NoProfile -File scripts/bench.ps1 -SkipTests        # 或直接 cargo test -p drive-e2e

# 默认限流（生产默认）release 基准
$env:CARGO_HOME="D:\Code\drive-sync\.cargo-home"
cargo run --release -p drive-e2e --bin drive-bench --offline -- `
    --default-limits --json-out target/bench/bench-default-limits.json

# 关闭限流（路径上限）release 基准
cargo run --release -p drive-e2e --bin drive-bench --offline -- `
    --json-out target/bench/bench-limits-off.json

# 快速冒烟（N=100，5MiB，20 连接）
cargo run --release -p drive-e2e --bin drive-bench --offline -- --quick

# 一键演示（relay + 双设备 + 配对 + 剪切板 + 目录/文件传输 + vault；CLI 串行）
powershell -NoProfile -File scripts/demo.ps1
```

`drive-bench --help` 查看全部参数（`--default-limits`、`--quick`、`--skip-rss`、
`--print-json` 等）。JSON 字段：`rtt_1kb`、`throughput.{windowed,blast_no_ack}`、
`memory.idle_connections`、`relay.config`、`verdict`。

## 7. 已知限制与剩余风险

1. **内存口径**：中继与客户端同进程，RSS 为进程合计（上限近似）；无独立中继进程 RSS。
2. **单次运行**：未做多轮取中位数；CPU 调度/后台负载可带来 10%~20% 波动。debug 构建
   （`cargo run` 默认）RTT p95 ≈2.1ms、吞吐 ≈3.4MB/s，仅用于功能验证。
3. **blast 口径**：无应用层 ack，发送侧吞吐不代表端到端；接收侧已并发解密校验并报告
   丢帧（本次 0 丢帧）。默认限流组不测 blast（超出默认出站队列，非协议用法）。
4. **中继关闭语义**：`RelayHandle::shutdown()` 依赖 server-impl 修复（Close 1001 +
   等待在线连接注销）；`shutdown_signal()` 仅停止 accept、不关闭已有 WS（rustdoc 已注明）。
5. **e2e 时序**：多设备事件顺序依赖调度，`ClientNode::wait_event` 已改为"未匹配事件
   入 pending 缓冲"避免丢事件；仍建议 CI 上重复跑以观察偶发时序问题。
6. `scripts/demo.ps1` 与 README/deployment 的命令差异核对（task-5 阶段 3）中，
   demo 脚本由 lead 收尾；本文件只覆盖基准与 e2e。

## 8. 变更记录

- 2026-10-24：加入默认限流（2000 帧/s + 背压）与关闭限流两组口径；更新为最新
  release 数字；补充阶段 2 端到端测试覆盖（13 passed）；记录限流口径冲突已解决。
- 2026-10-23：初版（阶段 1：RTT/吞吐/内存基准 + vault TTL 测试）。
