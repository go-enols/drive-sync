# 自动更新与发布流程

> 适用于 drive-sync v0.2+：桌面端（Windows / Linux / macOS）可执行文件的自动更新，以及
> GitHub Actions 多平台构建 + 签名更新清单的发布流水线。
>
> 相关代码：
> - [`crates/updater`](../crates/updater)：清单格式、Ed25519 验签、下载、BLAKE3 校验、安全替换 exe；
> - [`crates/cli/src/update_cmd.rs`](../crates/cli/src/update_cmd.rs)：`drive-sync update check|apply` 与 daemon 自动更新；
> - [`.github/workflows/ci.yml`](../.github/workflows/ci.yml)、[`.github/workflows/release.yml`](../.github/workflows/release.yml)；
> - [`scripts/update-e2e.ps1`](../scripts/update-e2e.ps1)：离线端到端自测。

## 1. 设计目标与安全模型

| 目标 | 做法 |
|---|---|
| 中继/网络不可信 | 清单用 **Ed25519 签名**，公钥编译进客户端；验签失败一律拒绝 |
| 下载内容被篡改 | 清单里带每个产物的 **BLAKE3 + 字节数**，安装前校验 |
| 不破坏用户环境 | 只替换当前可执行文件；保留 `.old-*` 备份；Windows 复制失败自动回滚 |
| 未签名不静默放行 | 默认拒绝未签名清单；只有显式 `--allow-unsigned` 才继续（本地测试用） |
| 私钥不入库 | 私钥只存在于 GitHub Actions Secret `UPDATE_SIGNING_KEY`；仓库只有公钥 |

信任链：**客户端内置公钥 → 验证 latest.json 签名 → 按清单中的 BLAKE3/size 验证二进制**。
因此即使 GitHub Release 被替换、或下载走的是不可信镜像，只要签名私钥不泄漏，就无法让客户端
安装任意文件。

## 2. 更新清单格式（`latest.json`）

```json
{
  "schema": 1,
  "product": "drive-sync",
  "version": "0.3.0",
  "published_at_ms": 1760000000000,
  "notes": "修复……",
  "assets": [
    {
      "target": "x86_64-pc-windows-msvc",
      "url": "https://github.com/go-enols/drive-sync/releases/download/v0.3.0/drive-sync-v0.3.0-x86_64-pc-windows-msvc.exe",
      "size": 12345678,
      "blake3": "0123…（64 字符 hex）"
    },
    {
      "target": "x86_64-unknown-linux-gnu",
      "url": "https://github.com/go-enols/drive-sync/releases/download/v0.3.0/drive-sync-v0.3.0-x86_64-unknown-linux-gnu",
      "size": 12000000,
      "blake3": "89ab…"
    }
  ],
  "signature": "…（128 字符 hex = 64 字节 Ed25519 签名；未签名为 null）"
}
```

字段说明：

| 字段 | 类型 | 说明 |
|---|---|---|
| `schema` | u32 | 清单结构版本，当前固定 `1`，不兼容变更时递增 |
| `product` | string | 固定 `"drive-sync"`（防止把别的项目的清单喂给客户端） |
| `version` | string | 新版本号（semver，允许 `v` 前缀；客户端做简化 semver 比较） |
| `published_at_ms` | u64 | 发布时间（Unix 毫秒，仅展示/排序用，不参与信任判断） |
| `notes` | string | 更新说明（纯文本，`update check` 会展示） |
| `assets[]` | array | 每个平台一个产物 |
| `assets[].target` | string | Rust target triple，客户端只选与自身编译目标精确匹配的产物 |
| `assets[].url` | string | 产物地址，支持 `https://` / `http://` / `file://`（本地测试） |
| `assets[].size` | u64 | 字节数（与 BLAKE3 一起校验） |
| `assets[].blake3` | string | BLAKE3 摘要（64 字符 hex） |
| `signature` | string? | Ed25519 签名（128 字符 hex）；未签名清单为 `null`/缺失 |

补充约定：

- **`assets[].url` 指向裸可执行文件**（不是 zip/tar.gz）。客户端下载后直接替换当前 exe；
  zip/tar.gz 只是给人工下载的便携包，不参与自动更新。
- **签名对象**：去掉整个 `signature` 字段后的 **canonical JSON**（对象 key 递归字典序排序，
  复用 `drive_core::util::canonical_json`）。因此 JSON 排版、字段顺序不影响验签。
- 发布流水线额外写入 `"signed": true|false` 标记。它**不在签名覆盖范围内**，只用于人和脚本
  快速识别；权威判断始终是“`signature` 能否验签通过”。
- 清单大小上限 256 KiB（防御异常大清单）。

客户端 target triple 映射（编译期决定）：

| 平台 | target |
|---|---|
| Windows x86_64 | `x86_64-pc-windows-msvc` |
| Linux x86_64 | `x86_64-unknown-linux-gnu`（musl 构建为 `x86_64-unknown-linux-musl`） |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |

## 3. 签名密钥管理

### 3.1 生成密钥对

```bash
cargo run -p drive-updater --bin sign-manifest -- --keygen
# 输出：
# pubkey (64 hex, 编译进客户端): 99e9923b…
# seed   (64 hex, CI Secret UPDATE_SIGNING_KEY): <随机种子>
```

- **公钥**（64 hex）是公开的，当前发布公钥为
  `99e9923bd30dea30a7d9b419016c714629729649083622ed4dc95051e1f7e9b5`：
  提交在 [`crates/updater/update-pubkey.hex`](../crates/updater/update-pubkey.hex)，
  并作为 [`crates/updater/src/lib.rs`](../crates/updater/src/lib.rs) 中 `DEFAULT_UPDATE_PUBKEY_HEX`
  的 fallback（发布构建还会通过 `DRIVE_UPDATE_PUBKEY` 环境变量注入同一把公钥）。
- **私钥种子**（64 hex）**绝不能提交仓库**。正式发布私钥只存在于 GitHub Actions Secret
  `UPDATE_SIGNING_KEY`；本地开发/自测用 `sign-manifest --keygen` 临时生成一对，并通过
  `DRIVE_UPDATE_PUBKEY` 环境变量覆盖客户端内置公钥即可。

### 3.2 配置 GitHub Secret

仓库 → Settings → Secrets and variables → Actions → New repository secret：

| Secret | 值 | 用途 |
|---|---|---|
| `UPDATE_SIGNING_KEY` | 64 字符 hex 种子 | release workflow 给 `latest.json` 签名 |

没有该 secret 时 release workflow **仍能跑通**：会产出 `signed: false` 的未签名清单，并在
Actions 日志里打 `::warning::`。此时客户端 `update apply` 默认拒绝安装（需 `--allow-unsigned`）。

### 3.3 密钥轮换

1. `sign-manifest --keygen` 生成新密钥对；
2. 把新 pubkey 写入 `crates/updater/update-pubkey.hex` 与源码 fallback，提交并发布新版本
   （旧客户端只信任旧公钥，必须先用旧私钥发布这次“换钥”版本）；
3. 更新 GitHub Secret `UPDATE_SIGNING_KEY` 为新种子；
4. 旧客户端更新到该版本后即信任新公钥；之后可停用旧私钥。

## 4. 发布流程

### 4.1 正常发版（tag 触发，自动发布）

```bash
# 1) 修改 workspace.package.version（Cargo.toml），同步更新 README/文档中的版本说明
# 2) 提交
git commit -am "chore: release v0.3.0"
git push origin main

# 3) 打 tag（tag 必须与 Cargo 版本完全一致，否则 release workflow 直接失败）
git tag v0.3.0
git push origin v0.3.0
```

release workflow 会：

1. `meta`：从 `cargo metadata` 读取版本、校验 tag 一致性、读取公钥；
2. `build`（4 平台矩阵）：构建 `drive-cli` + `drive-relay` + updater 工具，打包便携包，
   上传裸可执行文件、压缩包与每平台 asset JSON；
3. `manifest`：合并 4 个平台的 asset JSON 生成 `latest.json`；有 `UPDATE_SIGNING_KEY` 则签名并
   自检验签；
4. `publish`：创建/更新 GitHub Release，上传全部产物 + `latest.json`（标记 latest）。

产物命名（`v0.3.0` 示例）：

| 文件 | 说明 |
|---|---|
| `drive-sync-v0.3.0-x86_64-pc-windows-msvc.zip` | Windows 便携包（exe + README + 示例配置） |
| `drive-sync-v0.3.0-x86_64-unknown-linux-gnu.tar.gz` | Linux 便携包 |
| `drive-sync-v0.3.0-aarch64-apple-darwin.tar.gz` | macOS ARM 便携包 |
| `drive-sync-v0.3.0-x86_64-apple-darwin.tar.gz` | macOS Intel 便携包 |
| `drive-sync-v0.3.0-<target>[.exe]` | **裸可执行文件**（自动更新下载/安装用） |
| `latest.json` | 更新清单（客户端默认从 `releases/latest/download/latest.json` 读取） |

### 4.2 手动触发（只产出 artifact，便于测试）

Actions → Release → Run workflow：

- `publish` 保持 `false`：只构建并上传 Actions Artifact（`build-<target>`、`update-manifest`），
  不创建 Release；
- `tag` 留空时使用 `v<Cargo 版本>`；
- 勾选 `publish=true` 并填 `tag`：走与 tag push 相同的发布逻辑。

### 4.3 手动构建/本地发布清单

```bash
# 单平台：计算产物哈希并写 asset JSON
cargo run -p drive-updater --bin gen-manifest -- asset \
  --target x86_64-pc-windows-msvc \
  --url "https://example.com/drive-sync.exe" \
  --file target/release/drive-sync.exe \
  --out dist/assets/asset-windows.json

# 合并为未签名清单
cargo run -p drive-updater --bin gen-manifest -- merge \
  --version 0.3.0 --notes "test" --assets dist/assets --out dist/latest.json

# 签名（种子从环境变量读取，不落盘）
UPDATE_SIGNING_KEY=<64 hex> \
  cargo run -p drive-updater --bin sign-manifest -- --in dist/latest.json --out dist/latest.json

# 验签
cargo run -p drive-updater --bin gen-manifest -- verify \
  --manifest dist/latest.json --pubkey-hex <64 hex>
```

## 5. 客户端使用

### 5.1 检查更新

```bash
drive-sync update check                 # 人读输出
drive-sync update check --json          # 脚本用（含 current_version/latest/update_available/asset）
drive-sync update check --url https://mirror.example.com/latest.json
```

`check` 只下载小清单（≤256 KiB）并验签，不下载产物，可随时运行。

### 5.2 安装更新

```bash
drive-sync update apply                 # 交互确认后下载/校验/替换 exe
drive-sync update apply --yes           # 脚本/服务用，跳过确认
drive-sync update apply --json --yes    # 机器可读结果
drive-sync update apply --allow-unsigned --yes   # 仅本地测试！跳过验签（会打印警告）
```

安装成功后提示：**已更新到 vX，重启后生效**（运行中的进程仍是旧版本，重新运行 `drive-sync`
即可）。旧版本保留为 `<exe>.old-<时间戳>`，状态写入 `<data_dir>/update-state.json`。

### 5.3 daemon 自动更新

```bash
# 启动时检查一次，之后每 6 小时检查；发现新版本自动下载/验签/安装
drive-sync daemon --auto-update

# 安装后自动拉起新进程并退出（适合作为系统服务运行）
drive-sync daemon --auto-update --update-restart
```

自动更新默认**关闭**；只接受已签名清单（没有 `--allow-unsigned` 入口），并读取
`update-state.json` 避免每 6 小时重复安装同一版本。安装后需重启才生效；`--update-restart`
会用相同命令行参数拉起新进程。

### 5.4 配置覆盖（优先级从高到低）

| 项 | 来源 |
|---|---|
| 清单地址 | `--url` → 环境变量 `DRIVE_UPDATE_URL` → `config.json` 的 `update_manifest_url` → 编译期 `DRIVE_UPDATE_URL`（发布构建默认指向 `https://github.com/go-enols/drive-sync/releases/latest/download/latest.json`） |
| 验签公钥 | `config.json` 的 `update_pubkey` → 环境变量 `DRIVE_UPDATE_PUBKEY` → 编译期 `DRIVE_UPDATE_PUBKEY` → 源码 fallback 公钥 |

`config.json` 示例（`<data_dir>/config.json`，与中继配置同一个文件）：

```json
{
  "relay_url": "wss://sync.example.com/ws",
  "access_token": "…",
  "update_manifest_url": "https://mirror.example.com/drive-sync/latest.json",
  "update_pubkey": "99e9923bd30dea30a7d9b419016c714629729649083622ed4dc95051e1f7e9b5"
}
```

> 说明：当前 CLI 以“原始 JSON 读取”方式支持这两个键（不修改 client-core 的 `ClientConfig`）。
> 其它命令保存 `config.json` 时可能丢弃未知键；如需长期固定，建议用编译期默认值或环境变量。

## 6. 本地测试（无需联网）

[`scripts/update-e2e.ps1`](../scripts/update-e2e.ps1) 在 `tmp/update-e2e` 下用 exe 副本完整走一遍
“生成清单 → 未签名被拒 → 签名 → check → apply → 校验备份/状态 → 篡改被拒 → config 覆盖”：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/update-e2e.ps1
# 保留临时目录排查：
powershell -ExecutionPolicy Bypass -File scripts/update-e2e.ps1 -KeepWork
```

手工方式（任意平台，产物换成对应文件）：

```bash
# 1) 启动一个本地 HTTP 服务（也可直接用 file:// 地址）
python -m http.server 8000 -d dist

# 2) 生成清单（未签名）
cargo run -p drive-updater --bin gen-manifest -- merge --version 0.9.9 \
  --notes "local test" --assets dist/assets --out dist/latest.json

# 3) 签名（本地临时密钥）
cargo run -p drive-updater --bin sign-manifest -- --keygen
UPDATE_SIGNING_KEY=<seed> \
  cargo run -p drive-updater --bin sign-manifest -- --in dist/latest.json --out dist/latest.json

# 4) 客户端指向本地清单（公钥用 --keygen 输出的 pubkey）
DRIVE_UPDATE_PUBKEY=<pubkey> \
  drive-sync update check --url http://127.0.0.1:8000/latest.json
DRIVE_UPDATE_PUBKEY=<pubkey> \
  drive-sync update apply --url http://127.0.0.1:8000/latest.json --yes
```

## 7. 手动下载与完整性校验

```bash
# 从 GitHub Release 下载最新清单与产物
gh release download --repo go-enols/drive-sync --pattern latest.json
gh release download --repo go-enols/drive-sync --pattern 'drive-sync-v*'

# 校验下载文件（输出 JSON：size + blake3；与清单里的值比对）
cargo run -p drive-updater --bin sign-manifest -- --hash drive-sync-v0.3.0-x86_64-pc-windows-msvc.exe

# 校验清单签名（公钥来自仓库 update-pubkey.hex）
cargo run -p drive-updater --bin gen-manifest -- verify \
  --manifest latest.json --pubkey-hex "$(tr -d '[:space:]' < crates/updater/update-pubkey.hex)"
```

## 8. 回滚说明

自动更新只替换可执行文件，**数据目录/配置/身份密钥不受影响**。

### 8.1 使用备份回滚

安装时旧文件保留为 `<exe>.old-<tag>`（`tag` 默认是毫秒时间戳，CLI 也可用版本号）：

```powershell
# Windows（PowerShell）
Copy-Item "$env:LOCALAPPDATA\drive-sync\drive-sync.exe.old-1760000000000" `
          "$env:LOCALAPPDATA\drive-sync\drive-sync.exe" -Force
```

```bash
# Linux / macOS
cp ~/.local/share/drive-sync/drive-sync.old-1760000000000 ~/.local/share/drive-sync/drive-sync
chmod 755 ~/.local/share/drive-sync/drive-sync
```

实际 exe 路径以 `update apply` 输出或 `Get-Command drive-sync` / `which drive-sync` 为准；
`<data_dir>/update-state.json` 的 `last_backup` 记录了最近一次备份路径。

### 8.2 失败回滚（自动）

- **Windows**：先把运行中的 exe 重命名为 `.old-*`，再复制新 exe；复制失败会删除半成品并把
  旧 exe 改回原名。
- **Unix**：新文件写到同目录临时文件、设 0755，再原子 `rename` 覆盖；`rename` 失败时旧文件
  保持不变，临时文件被清理。
- 下载/校验失败不会触碰现有 exe（`verify_file` 在 `install_binary` 之前执行）。

### 8.3 降级/禁用自动更新

- 去掉 `daemon --auto-update`（或修改 systemd unit / 服务配置）即可关闭自动更新；
- 把清单里的 `version` 改回旧版本即可让客户端“降级”（客户端只比较版本号；签名清单仍必须
  由发布私钥签出）。

## 9. CI / Release workflow 说明

### 9.1 `ci.yml`（push main / PR / 手动）

| job | 内容 |
|---|---|
| `rustfmt` | `cargo fmt --all -- --check` |
| `clippy` | `cargo clippy --workspace --all-targets -- -D warnings`（Linux 需先装 xcb/wayland 依赖） |
| `test` | Ubuntu + Windows：`cargo test --workspace --exclude drive-e2e`；e2e 单独 `--test-threads=1` 串行跑 |
| `build` | Ubuntu + Windows：release 构建 `drive-cli` / `drive-relay` / updater 工具，并冒烟检查 `update check/apply --help` |

### 9.2 `release.yml`（tag `v*` / 手动）

- 触发：push tag `v*`（自动发布）、`workflow_dispatch`（默认只产 artifact）；
- 矩阵：`windows-latest`(x86_64-pc-windows-msvc)、`ubuntu-latest`(x86_64-unknown-linux-gnu)、
  `macos-14`(aarch64-apple-darwin)、`macos-13`(x86_64-apple-darwin)；
- Linux runner 安装：`libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
  libxkbcommon-dev libwayland-dev`（arboard 依赖）；
- 构建环境：`DRIVE_UPDATE_URL=<repo>/releases/latest/download/latest.json`、
  `DRIVE_UPDATE_PUBKEY=<仓库公钥>` 注入二进制；
- 签名：`secrets.UPDATE_SIGNING_KEY` 存在 → Ed25519 签名 `latest.json`；不存在 → 未签名
  （`signed:false`，日志 warning，客户端默认拒绝）；
- 权限：仅 `publish` job 需要 `permissions: contents: write`。

## 10. 故障排查

| 现象 | 原因 / 处理 |
|---|---|
| `未配置更新验签公钥` | 编译时没注入公钥且源码 fallback 为空：检查 `crates/updater/update-pubkey.hex` 与 `DEFAULT_UPDATE_PUBKEY_HEX` |
| `清单验签失败: 清单未签名` | 发布时没配 `UPDATE_SIGNING_KEY`，清单是 unsigned；正式发布请配置 secret |
| `清单签名校验失败` | 公钥与签名私钥不匹配（换钥后旧客户端）、清单被改动、或下载到错误文件 |
| `清单中没有适用于 <target> 的产物` | 该平台未参与 release 矩阵；用 `update check --json` 查看 `assets` 里的 target |
| `下载超过大小上限` | 产物异常大或清单被恶意篡改；上限 512 MiB |
| `BLAKE3 校验失败` | 下载不完整/被中间人替换；删除 `<data_dir>/updates/` 后重试 |
| Windows `Access is denied` | exe 被安全软件/服务管理器占用；用管理员权限或先停止服务 |
| daemon 自动更新无日志 | 默认只 debug 日志；`--log-level debug` 查看“已是最新版本/无产物”等跳过原因 |

## 11. 已知限制与风险

- **未签名清单**只在本地/测试使用；生产发布务必配置 `UPDATE_SIGNING_KEY`。
- 自动更新只替换 CLI 可执行文件本身，不处理 systemd unit/桌面快捷方式等安装项；服务化部署
  的 `--update-restart` 依赖服务管理器能接受子进程重启（默认 systemd `Restart=always` 亦可）。
- `macos-13`（Intel）runner 属于 GitHub 较老镜像，若被下线需在 `release.yml` 改为
  `macos-15-intel` 或 `macos-14` + 交叉编译；发布前请确认 runner 可用性。
- 发布产物默认启用 `tls` feature（可连 `wss://`）；自建中继若只走内网 `ws://` 也可正常工作。
- 更新清单地址默认指向 GitHub；完全离线/内网部署需自建清单镜像，并通过 `--url` /
  `DRIVE_UPDATE_URL` / `config.json` 覆盖，同时把公钥一起分发。
- 清单/产物缓存目录 `<data_dir>/updates/` 会在安装成功后清理下载文件；`update check` 不缓存清单。
