# EasyTier 去中心化 mesh 原理卡片

- 来源：本地源码 G:\AICODE\0NiceProject\EasyTier-main（v2.7，Apache-2.0）easytier/src/{core.rs,easytier-cli.rs}、easytier-core/src/{config,gateway,peers}、官方 README
- 对应模块：`sb.easytier.mesh`（process 绑定，SDK `switchboard-mod-easytier`）

## 一句话原理
去中心化 mesh VPN：节点互为 peer（无中心协调者），UDP/TCP 打洞 + 多协议隧道回退 + 公网/自建节点中继全部发生在网内；上层应用只见一张「底层 IP 网」。

## 关键机制（源码级）
1. **组网即打洞**：节点经引导 peer（`[[peer]] uri`，公共节点 `tcp://public.easytier.top:11010` 或自建）交换路由（proto `Route` 表），随后 peer 间并发 UDP/TCP 打洞（`instance/udp_hole_punch.rs`），失败自动回落中继转发。
2. **多协议隧道**：同端口多协议复用（UDP/TCP/QUIC/WSS，`tunnel/`），对 UDP QoS 劣化的运营商可走 TCP 伪装——rd 自研栈没有的能力。
3. **虚拟网两层用法**：
   - TUN 模式（管理员）：虚拟网卡直拨对端虚拟 IP（DHCP 或 `ipv4` 静态，如 `10.126.126.x/24`）；
   - **no_tun + port-forward**（免管理员）：`[[port_forward]] bind_addr/dst_addr/proto` 把本机端口映射到 mesh 内 `虚拟IP:端口`（gateway/port_forward.rs，TCP+UDP 均支持）。
4. **查询面**：`easytier-core --rpc-portal <addr>` 开管理 RPC；`easytier-cli --rpc-portal <addr> --output json route` 出 `{node_info, peer_routes:[{route:{ipv4Addr,...}}]}`（pbjson camelCase）。
5. **身份**：network_name + network_secret（同网才能互见）；与 rd 的 QUIC 指纹钉扎正交——mesh 只管可达，端到端认证仍是 rd 证书对。

## 已知边界与教训
- no_tun 模式下虚拟 IP 不可直拨（无网卡路由），必须经 port-forward——这就是 SDK 默认产出 `127.0.0.1:port` 候选（cost=1）的原因。
- route JSON 是 pbjson camelCase；SDK 解析对 camel/snake 双兼容并 fixture 锁定（防上游序列化切换）。
- 官方公共节点（public.easytier.top）仅作引导/兜底，生产应自建中继节点（与 spec §2「兜底必须近处」结论一致）。

## 对规范的启示
- 「组网工具 = 候选来源（S1）+ 底层网」：mesh 内部的 punch/relay 对编排器透明，`PathKind` 按候选入口如实标注（port-forward=Direct 入口，经 mesh 中继时链路语义是 Relay——遥测 extra 记 `mesh=easytier`）。
- process 绑定第二例（继 tailscaled 后）：托管职责 = 配置生成 + spawn + rpc 就绪 + Drop 句柄终止，不引上游依赖（edition 2024/rust 1.95 不可行，且跨 workspace 继承是已踩坑）。
- `SourceKind::Mesh` 新候选来源：选路成本先验 1（转发入口）/2（虚拟 IP 直连），与 LAN(1)/WAN(2) 对齐。
