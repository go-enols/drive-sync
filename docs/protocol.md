# derive-sync 协议规范 v0.1

> 状态：原型实现基准（2026-10）。实现代码见 `crates/core`，本文档与 `derive-core` 中的类型一一对应。
> 变更协议必须同步修改：`crates/core` 类型、本文档、`crates/e2e-tests` 互操作测试。

## 0. 术语

| 术语 | 含义 |
|---|---|
| 设备（Device） | 一次安装/一台机器，持有唯一身份密钥对 |
| 中继（Relay） | 自建云服务器，只做连接注册、配对转发、密文转发、可选加密暂存 |
| 配对（Pair） | 两台设备一次性建立长期信任关系，之后默认自动认证 |
| 会话（Session） | 两台已配对设备在一次连接期间建立的 E2E 加密通道 |
| 目录（Catalog） | 软件元数据表；软件本体留在持有设备本地 |
| 暂存（Vault） | 可选的云端加密存储，服务端只保存密文，TTL 最长 30 天 |

信任模型：**中继不可信**（可能被入侵/运维窥探/篡改），所有内容端到端加密；中继能看到设备 id、在线状态、消息大小、时间与路由关系，看不到剪切板内容、目录元数据、文件内容、文件名（暂存文件名在 meta 中是明文，生产可加密）。

## 1. 角色与拓扑

- 个人/小团队自建：一个中继服务 + N 台设备（Windows/Linux/Android/iOS/macOS）。
- 信任关系是**两两配对**（pairwise mesh），没有群组密钥；N 台设备最多 N-1 条配对。
- 转发：任一台设备复制剪切板后，分别加密发给所有已配对且在线的设备。
- 同一局域网优先直连（v0.2：mDNS + QUIC/TCP 直连；v0.1 原型先走中继，接口预留 `Transport` 抽象）。
- 中继只做三件事：连接注册/在线状态、配对信息转发、E2E 密文转发（+ 可选密文暂存）。**不持久化任何内容数据**（暂存除外，且只存密文）。

## 2. 传输层

- 中继：`wss://<host>/ws`（生产必须 TLS，建议 Caddy/Nginx 终止 TLS）；`GET /healthz` 返回 `200 ok`。
- 开发/本机测试允许 `ws://`。
- 文本帧：UTF-8 JSON；单帧上限 8 MiB（`MAX_WIRE_FRAME_BYTES`）。超限由中继断开连接。
- 心跳：
  - WS Ping 帧：客户端每 30s 发一次（桌面）；中继 90s 未收到任何帧则断开。
  - 应用层 `Ping/Pong`：用于 PeerMessage 层（移动端/弱网按需）。
  - 重连退避：1s, 2s, 4s, 8s, ..., 上限 30s，±20% 抖动；休眠唤醒后立即重连一次。
- 顺序：同一连接内消息按序处理；中继保证同一发送者→同一接收者的 FIFO。

## 3. 身份

每台设备本地生成并保存（`<data_dir>/identity.json`，Unix 0600）：

- Ed25519 长期签名密钥（`ed_secret_hex`），用于 Hello 认证、会话握手签名、配对确认。
- X25519 长期密钥（`x_secret_hex`），保留用于未来静态密钥协商/联系人发现；v0.1 会话用临时 X25519。
- `device_id = "dev-" + hex(BLAKE3(ed25519_pub))[..16]`。

公开身份（`PublicIdentity`，JSON 字段顺序固定）：

```json
{
  "device_id": "dev-0123456789abcdef",
  "name": "我的笔记本",
  "ed_public": "<64 hex>",
  "x_public": "<64 hex>",
  "created_at_ms": 1750000000000
}
```

校验要求：`device_id` 必须等于 `BLAKE3(ed_public)` 派生值（`PublicIdentity::is_consistent`）。

## 4. 中继协议（JSON over WebSocket）

### 4.1 连接认证

1. 客户端连接 `/ws`。
2. 客户端发送：

```json
{"t":"hello","v":1,"device":{...PublicIdentity...},"access_token":"<自建服务器共享令牌>","nonce":"<32 hex 随机>","signature":"<128 hex>"}
```

`signature = Ed25519_sign(ed_secret, b"derive-sync-hello-v1" || nonce || device_id)`。

3. 中继校验：
   - `v` 版本匹配、token 常量时间比较、`PublicIdentity::is_consistent()`、签名有效（否则关闭连接）；
   - device_id 已在别处在线 → 踢掉旧连接（同设备重连）或拒绝（配置 `allow_multi_conn`，原型默认踢旧）。
4. 中继回复：

```json
{"t":"welcome","v":1,"device_id":"dev-...","session_id":"<中继连接 id>","server_time_ms":1750000000000,"online":["dev-...", "..."]}
```

5. 之后广播 `presence` 给所有在线客户端：`{"t":"presence","online":[...]}`。

### 4.2 消息一览

客户端 → 中继：

| t | 字段 | 说明 |
|---|---|---|
| `hello` | v, device, access_token, nonce, signature | 认证 |
| `pair_announce` | code_hash, invite, ttl_secs | 发起配对；中继只存 code_hash 与 invite，默认 TTL 120s |
| `pair_join` | code_hash, join | 用配对码加入 |
| `session_init` | to, msg | 转发 E2E 握手请求 |
| `session_ack` | to, msg | 转发 E2E 握手响应 |
| `relay` | to, envelope | 转发 E2E 密文信封 |
| `vault_put` | object_id, meta, ciphertext, ttl_secs | 可选加密暂存上传 |
| `vault_get` | object_id | 下载密文 |
| `vault_list` | — | 列出对象（含过期时间） |
| `vault_delete` | object_id | 立即删除 |
| `ping` | ts_ms | 应用层心跳 |

中继 → 客户端：

| t | 字段 | 说明 |
|---|---|---|
| `welcome` | v, device_id, session_id, server_time_ms, online | 认证成功 |
| `pair_event` | from, invite \| join | 配对信息互转 |
| `pair_result` | ok, message, peer | 配对路由结果（非最终配对成功；最终以 SAS 确认为准） |
| `session_init` / `session_ack` | from, msg | 握手转发 |
| `relay` | from, envelope | 密文转发 |
| `presence` | online | 在线设备 id 列表 |
| `vault_put_result` / `vault_get_result` / `vault_list_result` / `vault_delete_result` | 见类型定义 | 暂存操作结果 |
| `pong` | ts_ms | 心跳响应 |
| `error` | code, message | 协议/权限/限额错误 |

### 4.3 配对路由

- `code_hash = hex(BLAKE3(8 位配对码))[..16]`。
- A 发 `pair_announce {code_hash, invite}`，中继在 `code_hash → (device_id, invite, expires_at)` 表中登记（TTL 120s，一次性）。
- B 发 `pair_join {code_hash, join}`，中继找到登记：给 A 发 `pair_event{from:B, join}`，给 B 发 `pair_event{from:A, invite}`，删除该登记并回 `pair_result{ok:true, peer:A}`。
- 限流：每设备每分钟最多 5 次 join 尝试，失败即销毁登记（防暴力猜码）；配对码 8 位数字 + 120s TTL + 失败销毁，暴力破解概率可忽略。
- 中继不参与 SAS 计算，也看不到配对码明文。

### 4.4 在线状态与投递语义

- `presence.online` 是本中继上所有已认证设备；客户端只对**已配对**且在线的设备建会话。
- 中继**不缓存**发往离线设备的帧（回声/离线队列由客户端本地 spool 负责，见 §8）。
- 中继转发校验：
  - `relay.to` 必须是当前在线设备；
  - 只能转发给与自己已配对的设备？中继不感知配对关系（E2E 加密保证内容安全），但会拒绝 `to == self`。
  - 生产增强：中继可要求双方都提交过配对完成回执（`paired_with` 声明）再做转发过滤；v0.1 不做。

### 4.5 限额

| 项 | 默认值 |
|---|---|
| 单 WS 帧 | 8 MiB |
| 单设备发送速率 | 2000 帧/秒（令牌桶，突发 4000）；令牌不足时中继**背压等待**（默认最多 2s）而不是直接丢帧，等待超时才回 `rate_limited` 并丢弃该帧 |
| 单设备吞吐上限 | 约 125 MiB/s（2000 × 64KiB 块；WAN 实际受带宽限制） |
| 配对登记 TTL | 120s |
| 暂存单对象密文 | 8 MiB（原型，WS 单帧上限；生产改为 HTTP 流式） |
| 暂存总配额 | 1 GiB/设备，可配置 |
| 暂存 TTL | 60s ~ 30 天，服务端强制上限 30 天 |
| 连接空闲超时 | 90s（无任何帧） |

> 说明：限速按"帧"计而非按字节计，是原型阶段的简化；背压保证正常大文件传输不丢块。
> 生产建议改为按字节限速（数据帧 byte/s + 控制帧 frame/s），避免小帧刷接口与 8MiB 大帧
> 带来的不公平；客户端也应在本地做发送节流（默认 1900 帧/s 留余量）并把 `rate_limited`
> 视为可重试的背压信号。

## 5. 配对协议（一次性，SAS 防 MITM）

```
A(已运行)                             中继                         B(输入配对码/扫码)
  |-- pair_announce(code_hash,invite) -->|                              |
  |                                     |<-- pair_join(code_hash,join)--|
  |<----------- pair_event(join) -------|------- pair_event(invite) --->|
  |                                                                     |
  | 双方各自计算 SAS = f(X25519(a_eph,b_eph), code, 双方身份公钥)          |
  |---------------------------------------------------------------------|
  | 用户比对两端显示的 6 位 SAS 一致后确认（A 端确认/二维码扫码可带外确认）     |
  | 保存 PairRecord：peer PublicIdentity + relay_url + paired_at + 指纹   |
```

- `PairInvite`：`{v, relay_url, device: PublicIdentity, ephemeral_pub, created_at_ms}`，不含配对码。
- `PairJoinPayload`：`{v, device, ephemeral_pub, created_at_ms}`。
- 二维码字符串：`dsync-pair-v1:<base64url(JSON{invite, code})>`，即扫码本身完成带外码传递；手工输入时两端比对 SAS。
- 密钥派生（两端必须字节级一致）：
  1. `sides = [{device_id, name, ed_public_hex, x_public_hex, ephemeral_pub_hex} × 2]`，按 `device_id` 字典序排序；
  2. `transcript = canonical_json({v:1, code_hash: hex(BLAKE3(code)), sides})`；
  3. `shared = HKDF-SHA256(X25519(eph_self, eph_peer), salt=BLAKE3(transcript), info="derive-sync-pairing-key-v1", 32)`；
  4. `sas_bytes = HKDF-SHA256(shared, salt="derive-sync-sas-v1", info="sas", 4)`，`n = u32::from_be_bytes(sas_bytes) % 1_000_000`，显示为 `"%03d %03d"`（如 `042 917`）。
- 二维码/带外确认可以跳过人工比对（扫码即认证），但 CLI 仍打印 SAS 供核对。
- `pairing_fingerprint = hex(BLAKE3(shared))[..16]`，写入 PairRecord，用于两端审计；**不保存 shared/pairing key**。
- 配对完成即长期信任；后续连接用 §6 会话握手自动认证。
- 解除信任：删除本地 PairRecord（对端下次会话握手因找不到配对记录而拒绝）。生产增强：可发送撤销通知并轮换长期密钥。

## 6. E2E 会话握手（连接级前向保密）

触发：看到已配对设备上线后，由 **device_id 字典序较小的一方**发起（避免同时握手冲突）。

1. `session_init {to, msg}`：

```json
{"content":{"v":1,"from":"dev-a","to":"dev-b","session_id":"<24 hex>","ephemeral_pub":"<64 hex>","ed_public":"<64 hex>","created_at_ms":0},"signature":"<128 hex>"}
```

`signature = Ed25519_sign(ed_priv, b"derive-sync-session-init-v1" || canonical_json(content))`。

2. 对端校验：版本、`from == 期望配对设备`、`ed_public == PairRecord.peer.ed_public`、`to == 本机`、签名。通过后用自身临时 X25519 生成 `session_ack`：

```json
{"content":{"v":1,"session_id":"<同 init>","from":"dev-b","to":"dev-a","ephemeral_pub":"<64 hex>","created_at_ms":0},"signature":"<128 hex>"}
```

`signature = Ed25519_sign(ed_priv, b"derive-sync-session-ack-v1" || BLAKE3(canonical_json(init.content)) || canonical_json(ack.content))`。

3. 双方派生密钥：
   - `shared = X25519(eph_self, eph_peer)`；
   - `transcript = canonical_json({v:1, session_id, sides:[{device_id, ephemeral_pub_hex, ed_public_hex}] 按 device_id 排序})`；
   - `okm = HKDF-SHA256(shared, salt=BLAKE3(transcript), info="derive-sync-session-keys-v1", 64)`；
   - `device_id` 小的方向：`send_key = okm[0..32], recv_key = okm[32..64]`；另一方相反。
4. 会话建立后所有业务消息（PeerMessage）以 `E2eEnvelope` 加密并经 `relay` 转发：

```json
{"v":1,"session_id":"...","seq":0,"kind":"clipboard","ts_ms":0,"ciphertext":"<base64>"}
```

- `ciphertext = XChaCha20-Poly1305(key, nonce随机24B, plaintext=PeerMessage JSON, aad= "{v}|{session_id}|{seq}|{kind}|{ts_ms}")`，输出布局 `nonce||ct||tag`。
- `seq` 从 0 开始、每个方向独立、严格递增；接收方要求 `env.seq == 期望值`，否则视为重放/乱序错误并断开重握手。
- 会话仅存内存；连接断开即销毁。重连后重新握手（新临时密钥），因此具备连接级前向保密。
- 生产升级：Double Ratchet（消息级前向保密）+ X25519+ML-KEM 混合；v0.1 不做。

## 7. 设备间消息（PeerMessage）

外层为 E2E 信封，`kind = "peer"` 或按业务命名（实现统一用 `"peer"`，内层 `m` 区分）。JSON：`{"m":"clipboard", ...}`。

| m | 字段 | 语义 |
|---|---|---|
| `clipboard` | event | 剪切板变化 |
| `catalog_snapshot` | ops[], revision | 目录全量快照（会话建立后双方互发一次） |
| `catalog_delta` | ops[], revision | 目录增量 |
| `catalog_request` | — | 请求对端重发全量快照 |
| `have_list` | entry_ids[] | 本机持有软件本体（可用性广播） |
| `fetch_request` | entry_id | 请求对端把某软件本体传过来 |
| `transfer_offer` | manifest | 传输报价 |
| `transfer_accept` | transfer_id, from_offset | 接受并从偏移续传 |
| `transfer_reject` | transfer_id, reason | 拒绝 |
| `transfer_chunk` | transfer_id, offset, data(base64) | 数据块 |
| `transfer_ack` | transfer_id, received_bytes | 流控确认 |
| `transfer_done` | transfer_id, total_bytes | 发送完成 |
| `transfer_error` | transfer_id, message | 传输出错 |
| `vault_share` | object_id, key_hex, meta | 共享云端暂存对象密钥 |
| `ping` / `pong` | ts_ms | 应用层心跳 |

### 7.1 剪切板同步语义

- `ClipboardEvent`：`{v, event_id(16 hex), origin_device, origin_seq, created_at_ms, payload}`。
- `ClipboardPayload`：`{v, kind: text|rich_text|image|files, mime, compression: none|zstd, raw_size, hash(BLAKE3原始内容), data(base64)}`。
- 单条上限 32 MiB；超过提示走文件中转通道，不阻塞剪切板链路。
- 发送端本地去抖：检测到剪切板变化后等待 100ms 合并连续变化（避免复制大文件时多次触发）。
- 回环抑制（必须实现）：
  1. 维护 `last_local_hash`；本机内容未变化不发送；
  2. 应用远端内容前记录 `(payload.hash, origin_device, event_id)` 到 `recent_applied`（LRU 256 / 5 分钟）；
  3. 本地监听器读到内容后计算 hash，若命中 `recent_applied` 则忽略（不上报、不再转发）；
  4. 收到自己 origin 的 event 直接丢弃（防中继回环）。
- 幂等：`event_id` LRU 去重；重复收到同一 event 不重复应用。
- 离线设备：发送失败/离线时进入本地 spool（最多 100 条、总 16 MiB、TTL 24h），上线后按 origin_seq 顺序补发；接收端按 event 去重。
- 敏感内容：生产必须在 OS 层排除密码管理器内容（见平台文档）；v0.1 提供配置项 `clipboard.share_types` 与手动暂停开关。

### 7.2 软件目录同步（CRDT）

- 每条 `CatalogEntry`：`{id: "ent-"+16hex, name, version, platform, kind, size, content_hash(BLAKE3), tags[], notes, updated_at_ms, updated_by, hlc, deleted}`。
- `CatalogOp`：`upsert{entry}` / `delete{id, hlc, deleted_by, deleted_at_ms}`。
- `Hlc`：`{wall_ms, counter, node}`，编码 `"{wall:016x}-{counter:08x}-{node}"`；比较顺序 wall→counter→node。
- 本地写：`hlc = 当前目录最大 hlc.tick(now, node)`；删除写墓碑（`entry.deleted=true`），墓碑 30 天后可 GC。
- 远端合并：按 hlc 比较，新的覆盖旧的；相等视为幂等；同 hlc 不同内容记 conflict（保留本地）。
- 同步流程：
  1. 会话建立 → 双方各发 `catalog_snapshot`（含墓碑，几百~几千条 < 1MB）；
  2. 本地变更 → `catalog_delta`（可合并多个 op）；
  3. 收到 snapshot/delta → merge，若产生变化则持久化并更新本地 revision；不自动回发（避免风暴）。若发现对端 revision 落后则可补发 delta（v0.1 简化：靠全量 snapshot）。
- 可用性：`have_list` 是本机持有 `content_hash` 对应本体的 entry 集合；每次会话建立、软件增删后广播；收到 `fetch_request` 时查本地索引。
- 条目与本地文件解耦：`CatalogEntry` 不含路径；持有设备本地 `local_index: {entry_id → 本地路径}`。

### 7.3 文件中转（软件本体/大文件）

- 发起方 B 收到 `fetch_request`：查本地索引 → 打开文件 → 发 `transfer_offer{manifest}`。
- `TransferManifest`：`{v, transfer_id, entry_id?, file_name, size, content_hash(BLAKE3 hex), chunk_size(默认 64KiB), created_at_ms}`；`file_name` 不含路径分隔符。
- A 收到 offer：校验清单 → 创建接收器（`<data_dir>/incoming/.incoming-<id>.part`）→ 发 `transfer_accept{from_offset=0}`（断点续传时传已收字节数）。
- 发送方窗口：最多 32 块（2 MiB）未确认；接收方每收到 8 块（512 KiB）或最后一块后回 `transfer_ack{received_bytes}`；发送方收到 ack 推进窗口。
- A 收齐后：`transfer_done{total_bytes}` → 校验 BLAKE3 → 原子重命名为最终文件 → 更新本地索引与目录（若关联 entry）→ 结束。
- 任一方可发 `transfer_error/reject` 中止；接收端删除 `.part`，发送端停止。
- 重连续传：接收端保留 `.part`，重新 offer/accept（`from_offset = received_bytes`），发送方从该偏移继续（要求块边界对齐；原型按块偏移对齐，接收端丢弃未对齐尾部）。
- 每个设备并发传输上限 2 个。

### 7.4 云端暂存（可选，30 天 TTL）

- 密钥：每个对象随机 32 字节 `object_key`，**只存客户端** `<data_dir>/vault_keys.json`；上传前 `ciphertext = XChaCha20-Poly1305(object_key, nonce, plaintext, aad=object_id)`。
- 上传：`vault_put{object_id, meta, ciphertext, ttl_secs}`；服务端强制 `effective_ttl = clamp(ttl_secs, 60, 30d)`，记录 `created_at_ms/expires_at_ms`，落盘 `<vault_dir>/<object_id>.bin` + 索引。
- 服务端清理任务：每 60s 扫描过期对象删除；启动时清理；被查询到过期对象即删除并返回 not found。**到期删除不可恢复**。
- 跨设备使用：上传方通过 E2E `vault_share{object_id, key_hex, meta}` 把密钥发给已配对设备（离线则排队，上线补发）；接收方保存到自己的 `vault_keys.json` 后即可 `vault_get` 并用密钥解密。
- 服务端限额：单对象 ≤ 8 MiB（原型）、总 ≤ 1 GiB/设备、对象数 ≤ 1000；超限返回 `error`。
- 隐私：服务端可见 `meta.name/kind/size/content_hash` 与对象大小/时间；敏感场景应使用混淆名或后续版本把 meta 也加密进 ciphertext。

## 8. 客户端可靠性

### 8.1 本地数据目录

```
<data_dir>/
  identity.json        # 身份密钥（0600）
  peers.json           # PairRecord 列表
  config.json          # relay_url, access_token, clipboard 策略, spool TTL 等
  catalog.json         # 目录 CRDT 全量状态
  local_index.json     # 本机持有的 entry_id → 本地路径/内容哈希
  vault_keys.json      # 云端暂存对象密钥
  spool/               # 待发消息（离线补发）
  incoming/            # 接收中的 .part 与已收文件
  logs/
```

### 8.2 连接状态机

`Disconnected → Connecting → Authenticated → SessionsReady`；任何错误回到 `Disconnected` 并退避重连。重连成功后：`welcome` → 对每个在线已配对设备按规则建会话 → 会话建立后发 snapshot/have_list/spool。

### 8.3 投递与去重

- 所有待发 PeerMessage 先写 spool（`{peer_id, kind, payload, created_at, ttl, seq}`），收到对方 ack 或发送成功且对端在线后才删除？v0.1 简化：发送成功即删；重连后重发 snapshot/have_list 补齐。
- 剪切板事件使用 `event_id` LRU 去重（接收端 256 条/5 分钟）。
- 目录使用 CRDT 幂等合并，可安全重发。

### 8.4 多设备同时在线

- 每对设备独立会话；同一剪切板事件广播给所有在线已配对设备。
- 避免 A→B→C 中继式泛洪：只发送本机产生的变化，不转发从别的设备收到的变化（由各设备直接与 origin 配对？不保证全连接：如果 A 与 B、B 与 C 配对但 A 与 C 未配对，则 B 不转发 → A、C 不同步）。**v0.1 规定：设备只与自己配对的设备同步；需要全互通请两两配对。** 生产增强可做受信任群组转发（带 origin_device + TTL 跳数）。

## 9. 安全与威胁模型（摘要）

| 威胁 | 缓解 |
|---|---|
| 中继窃听内容 | E2E 加密（XChaCha20-Poly1305），中继只转发密文 |
| 中继 MITM 交换公钥 | 配对 SAS/二维码带外认证；会话签名由配对身份密钥验证 |
| 重放/乱序 | E2E `seq` 严格递增；剪切板 `event_id` LRU |
| 中继伪造设备上线 | Hello Ed25519 签名 + device_id 一致性校验 |
| 配对码暴力猜测 | 8 位数字 + 120s TTL + 失败即销毁登记 + 限流 |
| 设备丢失 | 删除本地 PairRecord；生产支持远程撤销 + 密钥轮换 |
| 云端暂存数据泄露 | 客户端加密，服务端无密钥；TTL 强制 30 天 |
| 剪切板敏感内容外泄 | OS 层排除密码管理器 + 按设备/类型开关 + 暂停同步 |

## 10. 版本与错误码

- `PROTOCOL_VERSION = 1`；`Welcome.v` 与 `Hello.v` 不一致时拒绝连接。
- 错误码（`error` 与 `pair_result`/`vault_*` 的 message 中）：

| code | 含义 |
|---|---|
| `auth_failed` | token 或签名无效 |
| `bad_version` | 协议版本不匹配 |
| `rate_limited` | 超出帧率限额且背压等待超时（客户端应退避重试，属可恢复错误） |
| `pair_not_found` | 配对码无效/过期/已使用 |
| `peer_offline` | 目标设备不在线 |
| `payload_too_large` | 帧或对象超限 |
| `vault_quota_exceeded` | 暂存配额不足 |
| `vault_expired` / `vault_not_found` | 对象不存在或已过期 |
| `protocol_error` | 非法消息 |

## 11. 实现契约（v0.1 原型）

| crate | 职责 | 负责人 |
|---|---|---|
| `derive-core` | 身份、密码学、配对、会话、wire 类型、目录 CRDT、传输清单、剪切板载荷（**已完成，视为冻结接口**） | Lead |
| `derive-relay` (lib + bin) | WS 中继：Hello 认证、presence、配对路由、转发、暂存 + TTL 清理、`/healthz`、指标日志 | server-impl 队友 |
| `derive-client` | `Store` 本地持久化、`Engine` 同步状态机（会话管理、剪切板去重、目录同步、传输收发、spool）、`net` WS 客户端 | client-impl 队友 |
| `derive-cli` | `derive-sync` 命令：`init/pair/daemon/status/catalog/relay/vault`；Windows/Linux 剪切板后端（arboard + 变更检测）；文件模式（测试用） | client-impl 队友 |
| `derive-e2e` | 端到端测试（进程内中继 + 双客户端）、延迟基准 `derive-bench`、演示脚本 | qa 队友 |

实现要求：

1. 不得修改 `derive-core` 公开类型/语义，除非同步更新本文档并由 Lead 确认（接口冻结，避免并行冲突）。
2. 中继不得落盘任何转发内容；暂存只存密文；日志禁止打印内容/文件名/access_token。
3. 客户端所有内容发送前必须经 E2E 会话加密；未建立会话时排队，不得明文发送。
4. 错误处理：协议错误记录日志并断开/重建会话，不得 panic。
5. 测试：中继/核心逻辑单测；端到端测试覆盖 配对→剪切板同步→目录同步→文件传输→暂存 TTL 删除→断线重连。

## 12. 待补充（v0.2+）

- 局域网直连：mDNS 发现、QUIC 传输、同 LAN 延迟目标 < 10ms、加密会话复用（同一 E2E 会话可跑在两个 transport 上，按 `event_id` 去重）。
- 移动端推送（FCM/APNs）触发的"拉取式"同步（见 docs/platform-mobile.md）。
- 群组/多用户、设备撤销、密钥轮换、后量子混合密钥交换。
- 二进制帧/CBOR 编码（替代 JSON+base64），吞吐提升约 30-40%。
- 大对象云端暂存改为 HTTP 流式上传/下载（预签名、分片、断点续传）。
