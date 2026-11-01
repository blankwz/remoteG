# 自建 DERP 中继部署套件（路线 B：体验更优，但需要折腾）

> **先看这里**：如果你只想"异地能远控、越简单越好"，请用**路线 A（[../relay-frp/](../relay-frp/README.md)，两个脚本搞定）**。
> 本方案（路线 B）适合：你想要**整套虚拟局域网**（多台设备互通、手机也能接入）、不介意准备域名和 Docker。

## 这套东西是干什么的（人话）

两台电脑打洞失败时，需要一个中间服务器帮忙转发流量。Tailscale 官方的中转站在国内很慢
（实测 RTT 1.2 秒+），所以我们在近处（香港/新加坡）放一个自己的中转站，延迟降到 50~150 毫秒。
这套套件包含两个程序：
- **headscale**：花名册管理员——记录哪台设备是我们家的（官方 Tailscale 账号不支持自建中转站，这是用它的根本原因）
- **derper**：中转站本尊——实际转发加密流量

## 准备清单（比路线 A 多两样）

| # | 东西 | 说明 |
|---|---|---|
| 1 | 一台 VPS（香港/新加坡 CN2 最好） | 要求：国内连它延迟 <100ms（买之前先 ping 一下） |
| 2 | **一个域名**，并把某条 DNS 记录指向 VPS 的 IP | 路线 A 不需要，这个必须要（证书要用） |
| 3 | VPS 上装好 Docker | `curl -fsSL https://get.docker.com \| sh` 一条命令 |

## 部署步骤（约 30 分钟，每步都说了"你会看到什么"）

1. **把本目录整个拷到 VPS**（比如 `scp -r deploy/derper root@VPS的IP:/root/`），SSH 登录 VPS。
2. **改两个模板里的"你的域名"**：
   - `headscale-config.template.yaml` 里所有 `server_url` 改成 `https://你的域名`
   - `derp-map.template.json` 里的域名和 RegionID（随便起个数，比如 901）
3. **起服务**：`docker compose up -d`
   - 看到两个容器 Created/Started 就是成了；`docker ps` 能看到 headscale 和 derper
4. **创建家庭用户和入门钥匙**（一串一次性密钥，新设备用它入网）：
   ```bash
   docker exec headscale headscale users create family
   docker exec headscale headscale preauthkeys create --user family --expire 8760h
   ```
   最后那串输出复制保存（一年有效）。
5. **两台电脑入网**：装 [Tailscale 客户端](https://tailscale.com/download) 后，管理员命令行跑：
   `tailscale up --login-server https://你的域名 --hostname 名字`
   浏览器会打开登录页，粘贴第 4 步的钥匙 → 显示 Success 即入网。
   （两台都做；配对码照常在 rd 托盘生成，这一步只管"组网"不管 rd。）
6. **验证**：
   - `tailscale netcheck` 输出里能看到你的自建 Region → 中转站注册成功
   - `tailscale ping 对端的tailscale名` 显示 via DERP(你的Region) → 走的你的中转站
   - `tailscale ping` 若显示 direct → 已打洞直连，中转站自动待命（这正是理想状态）
7. **rd 接入**：rd 连接层无需任何改动——Tailscale 网卡起来后，设备地址填 `tailscale IP:21118`（`tailscale ip -4` 查看）即可；需要 Switchboard 遥测时运行 `python ../tailscale-bridge.py`。

## 已知注意事项（技术备忘）

- derper 默认要求 TLS（`-a :443 --hostname`）；Caddy 自动签发证书含在 compose 里，域名 DNS 没生效前会一直重试签发——DNS 先配好。
- 内网试验可 `--verify-clients` + `DERP_ADDR` 裸 HTTP，**仅限试验**。
- 国内 VPS 若域名没备案，443 会被运营商拦：换境外 VPS，或用非标端口 + IP 直连证书（企业内网可接受）。
- 官方 Tailscale 账号不支持自定义 DERP map——这就是必须用 headscale 的原因。
- 预期效果：中继 RTT 从 1.2s → 50~150ms；打洞成功率由 Tailscale 保持（>90%）。
- 遥测（可选）：`../tailscale-bridge.py` 把 tailscaled 指标映射为 StageStats v1（05 §2 process 绑定；`path.kind` 如实标注，06 §1 谎报一票否决同样适用）。
