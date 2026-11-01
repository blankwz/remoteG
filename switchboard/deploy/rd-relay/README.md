# rd 原生中继（路线 C：rd 代码原生支持，被控端零安装）

> **一句话**：部署一个 rd-relay 中继（三种方式任选，最便宜的在 Cloudflare 免费跑），
> 被控端设置页填一次地址，控制端编辑设备填一次地址——之后连接直连失败时**自动**走中继。
> 中继只转发**加密后的** QUIC 包，永远看不到内容。参考 [alma-relay](https://github.com/yetone/alma-relay) 的部署体验设计。

## 与其他路线的关系

| | C：rd-relay（本目录） | A：frp（../relay-frp/） | B：DERP（../derper/） |
|---|---|---|---|
| 被控端要装什么 | **什么都不装**（rd 内置） | frp 客户端常驻 | Tailscale 全家桶 |
| 需要的设施 | Workers 免费 / VPS / Docker 任选 | VPS | VPS+域名+Docker |
| 打洞成功时 | 自动直连（中继仅兜底） | 永远走 frp 隧道 | 自动直连 |
| 端到端加密 | ✅（中继只见密文） | ✅（哑管道） | ✅ |

## 部署方式（三选一）

### 方式 1：Cloudflare Workers（免费，无需服务器）⭐ 小白首选

网页一键：在浏览器打开 [deploy.workers.cloudflare.com](https://deploy.workers.cloudflare.com)，
按提示导入本仓库即可；或命令行：

```bash
cd cloudflare
npm install -g wrangler
npx wrangler deploy    # 输出 https://rd-relay.<你的子域>.workers.dev
```

免费额度足够个人用；TLS 与 `*.workers.dev` 域名自带，无需购买任何东西。

### 方式 2：Docker 一条命令 + 自动 HTTPS（任意 VPS）

前置：VPS 开放 80/443，一个 DNS 记录指到它。

```bash
cp .env.example .env      # 填 RELAY_DOMAIN=relay.example.com
docker compose up -d
```

Caddy 自动签发/续期证书。验证：`curl https://relay.example.com/healthz`。

### 方式 3：VPS 直跑 UDP 版（延迟最低）

```bash
./deploy-vps.sh root@VPS的IP     # SSH 自动部署（systemd 自启 + 防火墙 + 验证）
```

二进制来源自动选择：本仓库已编译的 Linux 版 → Release 下载 → VPS 上 cargo 编译。

## 部署完的两步

1. **被控端**：rd 控制端 → **设置 → 远程访问（跨网中继）→ 使用自建中继**，
   填中继地址（`https://xxx.workers.dev` 或 `ip:21118`）→ 保存 → 重启 rd。
   （等价于编辑 `%LOCALAPPDATA%\rd\agent.toml` 加 `relay_addr = "..."`。）
2. **控制端**：设备 → 该设备「⋯」→ 编辑 → 「原生中继」填同一地址 → 保存。

完成。此后连接：直连能通就走直连；直连/打洞失败自动经中继（多几十毫秒延迟）。

## 工作原理（一段话）

被控端启动时向中继**注册**（上报证书指纹前 8 字节派生的会话标号），控制端连接时
**按标号呼叫**；之后双方所有数据报加 8 字节标号经中继转发。QUIC 包一个字节都不动
——rd 的配对码、双向证书认证、指纹钉扎、连接确认在经中继时全部照常生效，
中继无法窥屏。UDP 版转发 UDP 数据报；WS 版转发 WebSocket 二进制帧（协议一致）。

## 安全与限制（诚实清单）

- 中继按证书指纹派生标号路由：不知道对端指纹就无法借道（指纹配对时才交换）。
- 中继不鉴权注册——任何人可注册，但 QUIC 双向认证 + 配对码不变，
  "能连上"永远不等于"能控制"。个人使用通常足够；介意可加注册密钥（后续项）。
- WS 版每包多一层 WS+TLS 头，延迟略高于 UDP 版；UDP 版单指纹单会话（免费版 1 并发一致）；
  空闲 60s（WS 版跟随 WS 生命周期）回收。
- 手动卸载：VPS `systemctl disable --now rd-relay && rm -rf /opt/rd-relay`
  （Docker：`docker compose down`；Workers：`wrangler delete`）。
