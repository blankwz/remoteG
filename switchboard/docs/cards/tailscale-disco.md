# Tailscale DISCO/DERP 原理卡片

- 来源：tailscale.com/blog/how-nat-traversal-works、blog/nat-traversal-improvements-pt-1（2025-10）、官方文档
- 对应模块：`sb.ext.tailscale`（process 绑定）、`sb.derp.self-hk`

## 一句话原理
每个节点用 DISCO 协议在 STUN 测绘的端点上并发打洞，同时流量先走 DERP 中继保通，直连成功后无缝升级。

## 关键机制
1. **DERP 先行**：连接先经中继建立（用户无感），并行尝试 UDP 打洞；成功则迁移（与本规范 02 §2.2 `relay-first` 同构）。
2. **STUN 测绘 + 多路径竞速**：IPv4/IPv6、多网卡同时探测，择优。
3. **对称 NAT 生日攻击**：对端口预测目标并发发包提高命中（Tailscale 特有）。
4. **端口映射守护**：NAT-PMP/PCP 映射 epoch 监控，路由器重启自动重建；FreeBSD EIM-NAT 补丁使 pf 类防火墙从对称 NAT 变为端点无关映射。
5. **IPv6 优先**：双端原生 v6 直连，完全绕过 NAT。
6. 官方口径：典型条件下直连成功率 >90%，DERP 仅兜底。

## 已知缺陷与教训（rd 实测）
- 官方 DERP 节点在国内访问劣化：rd 两台机器打洞失败时中继 RTT 1.2s+，QUIC-over-WireGuard-over-HTTPS-TCP 双层重传放大延迟。
- 结论：**中继兜底的质量决定跨网体验下限**——自建 headscale+derper（放近处 VPS）是唯一可靠的修法。

## 对规范的启示
- 「先保通后变优」上升为 Plan 语义（02 §2.2 relay-first/upgrade）；
- `path.kind` 必须如实（direct/relay），码控有权按中继降码率；
- process 绑定（05 §2）：tailscaled 整栈即模块，遥测桥映射 StageStats。
