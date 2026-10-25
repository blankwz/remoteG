# 自建 DERP 中继部署套件（sb.derp.self-hk 的落地物）

> **为什么**：rd 实测两台国内机器打洞失败时走官方 DERP，RTT 1.2s+（QUIC over WireGuard over HTTPS-TCP 双层重传放大）。自建一台放在近处的 derper 是唯一可靠修法（spec 07 / 调研结论第一名）。

## 组成

| 文件 | 用途 |
|---|---|
| `docker-compose.yml` | headscale（控制面）+ derper（中继面）一键起 |
| `headscale-config.template.yaml` | headscale 配置模板（改 server_url / DERP map 路径） |
| `derp-map.template.json` | 自定义 DERP 区域模板（改 RegionID / 域名） |
| `../tailscale-bridge.py` | process 绑定遥测桥（05 §2）：tailscaled 指标 → StageStats v1 |

## 步骤（约 30 分钟）

1. **VPS**：选国内可达位置（香港/新加坡 CN2；实测延迟国内 <100ms 为合格）。
2. `docker compose up -d` 起 headscale + derper（本目录）。
3. 创建用户与接入密钥：`docker exec headscale headscale users create family && docker exec headscale headscale preauthkeys create --user family --expire 8760h`。
4. **两台机器**：`tailscale up --login-server https://<你的域名> --hostname <名字>`（需先把 headscale 的 HTTPS 证书配好，模板内含 Caddy 自动签发）。
5. 验证：`tailscale netcheck`（应看到自建 Region），`tailscale ping <对端>` 观察是否 still 经自建 derper。
6. **客户端接入 Switchboard**：Plan 的 `[[path]] name="tailnet"` 使用 `sb.ext.tailscale`（process 绑定），运行 `python ../tailscale-bridge.py` 把 tailscaled 状态映射为 StageStats——`path.kind` 如实标注（06 §1 谎报一票否决同样适用于 process 模块）。

## 已知注意事项

- derper 默认要求 TLS（-a :443 --hostname）；内网测试可用 `--verify-clients` + `DERP_ADDR` 裸 HTTP（仅限试验）。
- 官方客户端指向自建 DERP 需要 headscale 的自定义 DERP map（模板已含），官方 Tailscale 账号不支持自定义 DERP——**这是用 headscale 的根本原因**。
- 国内 VPS 需备案的域名走 443；否则用非标端口 + IP 直连证书（企业内网可接受）。
- 预期效果：中继 RTT 从 1.2s → 50~150ms；打洞成功率由 Tailscale 自身保持（>90%）。
