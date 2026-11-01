# rd 一键中继（路线 A：最简单，推荐小白）

> **一句话**：租一台云服务器，跑两个脚本，异地远控就能用了。
> 不用 Docker、不用域名、不用证书、不用 Tailscale——rd 的加密和配对照常生效，中继只是根"哑管道"。

---

## 什么时候需要我？

两台电脑**不在同一个 WiFi**（比如你在公司、家里电脑在老家），而且直连打洞失败时，
就需要一台放在中间的"传话服务器"。它就是本方案。

## 准备清单（共 3 样）

| # | 东西 | 去哪弄 | 花多少钱 |
|---|---|---|---|
| 1 | 一台云服务器（VPS），Linux 或 Windows 都行 | 阿里云/腾讯云/Vultr 等，最低配即可 | 约 ¥10-30/月 |
| 2 | 被控的那台电脑（要被远控的） | 已装 rd，托盘图标在跑 | 已有 |
| 3 | 控制的电脑（你手里的） | 已装 rd 控制端 | 已有 |

---

## 三步走

### 第 1 步：部署中继（在 VPS 上装"传话服务"，约 3 分钟）

**VPS 是 Linux**（最常见）——在**你自己的电脑**上跑一条命令：

```bash
./deploy-vps.sh root@你的VPS的IP
```

按提示输一次 VPS 的 SSH 密码，然后看着屏幕跑完。最后会显示：
- ✅ frp 服务端装好了、开机自启了、防火墙开了、端口验证过了
- 一个 **中继令牌**（自动生成并保存到本目录 `.relay-info`，下一步自动用，不用抄）

**VPS 是 Windows**——远程桌面进 VPS，把本目录的 `deploy-vps-windows.ps1` 拷过去，
右键 PowerShell 以管理员运行：

```powershell
powershell -ExecutionPolicy Bypass -File .\deploy-vps-windows.ps1
```

同样全自动，最后屏幕会显示 **VPS 地址** 和 **中继令牌**（记下来）。

> ⚠ **唯一可能要手动做的一件事**：云服务商控制台的「安全组」里放行
> **UDP 21118** 和 **TCP 7000**（脚本会提醒你；不开这个，前面全白搭）。

### 第 2 步：接线被控端（在被控电脑上跑，约 2 分钟）

把本目录的 `connect-home.ps1` 拷到被控电脑，双击 PowerShell 运行：

```powershell
powershell -ExecutionPolicy Bypass -File .\connect-home.ps1 -Server VPS的IP -Token 第1步的令牌
```

脚本会自动：检查 rd 在跑 → 下载 frp 客户端 → 配置并注册开机自启 → 验证连上 VPS。
Linux VPS 部署生成的 `.relay-info` 和脚本放一起时，`-Server`/`-Token` 都能自动读取。

### 第 3 步：控制端添加设备（10 秒）

rd 控制端 → 设备 → 添加设备 → 地址填：

```
VPS的IP:21118
```

配对码照常在被控端托盘生成。连上后远控就通了——**走中继会多几十毫秒延迟，正常**。

---

## 常见问题

| 现象 | 原因与解法 |
|---|---|
| 第 1 步下载 frp 失败 | VPS 访问 GitHub 被墙。脚本自动试了 3 个源都不行的话，手动去 [frp releases](https://github.com/fatedier/frp/releases) 下载压缩包传到 VPS 的 `/opt/rd-relay`（Windows 是 `C:\rd-relay`）后重跑 |
| 控制端连不上 | 九成是安全组没放行：**UDP 21118 + TCP 7000**。到云控制台补上 |
| 速度慢 | 中继速度 = VPS 带宽。低配 VPS（1-3 Mbps）看高清会卡，换带宽大的 |
| 想删掉中继 | VPS 上 `systemctl disable --now rd-relay && rm -rf /opt/rd-relay`（Windows：`schtasks /delete /tn rd-relay /f` + 删 `C:\rd-relay`） |
| 安全吗 | 中继只是转发加密流量，**看不到内容**；rd 的配对码、指纹校验、连接确认全部照常生效 |

---

## 和路线 B（自建 DERP）怎么选？

| | 路线 A：frp 一键中继（本目录） | 路线 B：自建 DERP（../derper/） |
|---|---|---|
| 部署难度 | ★☆☆ 两个脚本 | ★★★ Docker+证书+Tailscale 全家桶 |
| 需要域名 | 不需要 | 需要（HTTPS 证书） |
| 被控端要装的东西 | 一个小客户端（脚本自动装） | Tailscale 客户端 + 登录组网 |
| 延迟 | 多一跳 frp 转发，够用 | 略优（UDP 原生中继） |
| 附加能力 | 仅中继 | 整个 tailnet 组网（手机也能进） |

**结论**：只想"异地能远控"→ 路线 A；想要整套虚拟局域网（多设备、手机接入）且不怕折腾 → 路线 B。
