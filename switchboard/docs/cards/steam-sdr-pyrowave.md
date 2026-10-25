# Steam Remote Play（SDR + Pyrowave）原理卡片

- 来源：Steamworks SDR 文档、Steam 官方 FAQ、Steam Remote Play 社区发布帖（2026-09 Pyrowave beta）、Valve wiki
- 对应：`sb.derp.self-hk`/`sb.relay.*` 的架构参照；`sb.codec.lan444` 参照

## SDR（Steam Datagram Relay）
1. **直连优先**：会合后尝试 P2P；失败/路由差 → 走 SDR 中继（社区见 "stuck in relay mode"）。
2. **SDR = 自有骨干中继网**：全球 PoP（自有机房 + 云厂商），玩家就近接入，流量走 Valve 骨干路由而非拥塞公网；顺带隐藏 IP、抗 DoS。
3. **洞察**：SDR = 「自建 DERP」的完全体——打洞失败的连接只要中继质量够高，跨网体验依然成立。个人项目的等价物 = 一台放在近处的优质中继节点。

## Pyrowave（2026-09 beta）
1. **是什么**：GPU 上跑的纯 intra 编解码器，编+解合计 <0.1ms，原生 4:4:4 + HDR。
2. **代价**：~400-480 Mbps（5-10× 带宽），千兆 LAN 专属；跨网继续用 H.264/HEVC。
3. **定位**：Steam 版的"文字清晰"终极答案——与我们 QOI 无损瓦片 + L2 终端语义层解决同一问题；我们带宽花在脏区上，成本更低且跨网可用。

## 对规范的启示
- 中继质量分档进 caps（RelayCaps.quality_hint，01 §5）；
- `h264.lan444` 档（01 §9）对齐 Pyrowave 场景，但不追其带宽档位；
- relay-first + 优质中继的组合是「帮家人」场景的工业级验证。
