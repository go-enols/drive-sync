# 部署与运维

> 适用于 v0.1 原型：单台 VPS / NAS，自建中继 + N 台设备。目标是"够用、可维护、内容零存储"。

## 1. 最小拓扑

```
Internet
   │ 443/tcp (TLS)
   ▼
Caddy / Nginx  ──►  127.0.0.1:8787  drive-relay
                        ├─ 内存：连接/配对登记/转发
                        └─ /data/vault：仅密文（可选）
```

- 中继**不需要**数据库、对象存储、备份（因为不存内容）。
- 唯一需要备份的是 `access_token`（丢失需重新分发配置）与中继配置。
- 建议放在离常用设备近的区域；跨洲延迟主要是光速，无法优化掉。

## 2. 中继二进制

```bash
# 编译（在构建机）
cargo build --release -p drive-relay

# 直接运行（开发/本机）
./drive-relay --bind 127.0.0.1:8787 --token "$(openssl rand -hex 32)"

# 常用参数（以 --help 为准）
--bind 127.0.0.1:8787        # 监听地址（生产建议只监听 127.0.0.1，由反代暴露）
--token <token>              # 设备连接令牌（32B 随机；也可 --token-file 或环境变量 DRIVE_RELAY_TOKEN）
--vault-dir /data/vault      # 可选云端暂存的密文目录（不配置则禁用暂存）
--vault-quota-mib 1024       # 每设备暂存配额（MiB，默认 1024）
--vault-max-object-mib 8     # 单对象上限（MiB，默认 8）
--rate-limit 2000            # 每连接入帧速率（帧/秒，默认 2000；令牌不足先背压等待）
--rate-burst 4000            # 令牌桶突发（默认 4000）
--rate-limit-max-wait-ms 2000  # 背压等待上限（毫秒，超时才回 rate_limited 并丢帧）
--pair-ttl 120               # 配对登记 TTL 秒
--idle-timeout 90            # 空闲断开秒数
--log-level info             # 日志级别（RUST_LOG 亦可）
```

环境变量：`DRIVE_RELAY_TOKEN` 优先于命令行（避免 token 出现在 `ps` 里）。

## 3. TLS 反向代理

### Caddy（推荐，自动证书）

```caddyfile
sync.example.com {
    encode zstd gzip
    @ws {
        path /ws
        header Connection *Upgrade*
        header Upgrade websocket
    }
    reverse_proxy @ws 127.0.0.1:8787 {
        transport http {
            read_timeout 10m
            write_timeout 10m
        }
    }
    reverse_proxy /healthz 127.0.0.1:8787
    # 关闭缓存与请求体限制（大文件块走 WS 帧，单帧 ≤ 8MiB）
    request_body {
        max_size 16MB
    }
}
```

### Nginx

```nginx
server {
    listen 443 ssl http2;
    server_name sync.example.com;

    client_max_body_size 16m;
    proxy_read_timeout 600s;
    proxy_send_timeout 600s;

    location /ws {
        proxy_pass http://127.0.0.1:8787;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_buffering off;
    }
    location /healthz {
        proxy_pass http://127.0.0.1:8787;
    }
}
```

客户端配置：`--relay wss://sync.example.com`，`--token <token>`。

## 4. systemd（Linux 服务器）

```ini
# /etc/systemd/system/drive-relay.service
[Unit]
Description=drive-sync relay
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=drive
Group=drive
Environment=DRIVE_RELAY_TOKEN=<32B随机hex>
ExecStart=/usr/local/bin/drive-relay --bind 127.0.0.1:8787 --vault-dir /var/lib/drive/vault
Restart=on-failure
RestartSec=3
LimitNOFILE=65536
# 收紧权限：只允许写 vault 目录
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/drive
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now drive-relay
curl -fsS http://127.0.0.1:8787/healthz
```

## 5. Docker

```dockerfile
# 多阶段构建见 deploy/Dockerfile
FROM rust:1.96-slim AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p drive-relay

FROM debian:bookworm-slim
RUN useradd -r -u 10001 drive && mkdir -p /data && chown drive /data
COPY --from=builder /src/target/release/drive-relay /usr/local/bin/drive-relay
USER drive
VOLUME ["/data"]
EXPOSE 8787
ENTRYPOINT ["drive-relay"]
CMD ["--bind", "0.0.0.0:8787", "--vault-dir", "/data/vault"]
```

```bash
docker build -f deploy/Dockerfile -t drive-relay:0.1 .
docker run -d --name drive-relay --restart unless-stopped \
  -p 127.0.0.1:8787:8787 -v /srv/drive:/data \
  -e DRIVE_RELAY_TOKEN=<token> drive-relay:0.1
```

## 6. 设备端常驻（桌面）

### Windows

- 原型：`drive-sync daemon` 前台运行，或注册为启动项：
  ```powershell
  $exe = "C:\path\drive-sync.exe"
  $cmd = "`"$exe`" daemon --relay wss://sync.example.com --token <token>"
  schtasks /Create /TN "drive-sync" /TR $cmd /SC ONLOGON /RL LIMITED /F
  ```
- 生产建议：系统托盘 GUI + 单实例守卫 + 随用户登录启动（不要用 Windows 服务，Session 0 无法访问用户剪贴板）。

### Linux

```ini
# ~/.config/systemd/user/drive-sync.service
[Unit]
Description=drive-sync daemon
After=graphical-session.target

[Service]
ExecStart=/usr/local/bin/drive-sync daemon --relay wss://sync.example.com --token <token>
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now drive-sync
# 允许用户级服务在未登录时运行（可选）
sudo loginctl enable-linger $USER
```

Wayland 下需要 `wl-clipboard`（`wl-paste`）及 compositor 支持 data-control 协议；不支持的会话降级为不可用并提示。

## 7. 监控与告警

- 健康检查：`GET /healthz`（200 即存活）。
- 关键指标（建议在日志中定期输出，或接 Prometheus exporter）：
  - 在线设备数、连接数、重连次数
  - 转发帧数/字节、丢弃帧数、错误码计数
  - 暂存对象数/字节、过期删除数、配额拒绝数
- 日志脱敏：不得打印 access_token、ciphertext、内容、文件名。
- 资源：空载 100 连接目标 RSS < 100 MB；大文件转发主要消耗带宽与 socket buffer。

## 8. 备份与升级

- 无需备份内容（中继不存内容）；备份 `access_token` 与部署配置。
- 可选暂存目录按需备份（但到期会被删除，备份没有长期意义；如需长期保存请在设备端保留原始文件）。
- 升级流程：停服 → 替换二进制 → 启动；协议兼容窗口 ≥ 1 个大版本。
- token 轮换：生成新 token → 更新所有设备配置与中继 → 旧连接自然失效。

## 9. 安全加固清单

- [ ] 中继只监听 `127.0.0.1`，公网入口仅 443（反代）
- [ ] `access_token` 32B 随机，不写入 shell history（用环境变量/密钥管理）
- [ ] TLS 强制（wss），禁用弱协议，开启 HSTS
- [ ] 反代限制请求体（≤16MB）与连接数
- [ ] 中继以非 root 用户运行，收紧文件权限（ProtectSystem/PrivateTmp）
- [ ] 暂存目录单独限额，磁盘写满不影响系统
- [ ] 日志脱敏审计（内容/token/ciphertext 均不得出现）
- [ ] 定期检查依赖安全公告并升级
