# RustDesk NAT 打洞与传输原理卡片

> 依据 rustdesk master（HEAD `4d37f00a`, 2026-10-23）+ hbb_common 子模块（`defa08b`）源码逐行核实；无 QUIC/quinn 依赖，打洞主力仍是 hbbs/hbbr + KCP，1.5.x 起新增 webrtc-rs 路径。

## 一句话原理

双方先对 hbbs 各自完成一次"专用 UDP socket"的端口测绘并经 hbbs 交换（`PunchHoleRequest.udp_port`），然后向对方的公网 IP:UDP端口 互发 PROBE 探针穿透 NAT，洞一开就在这条 UDP 上跑 KCP（turbo 配置）伪 TCP 流；任何一端为对称 NAT 或打洞超时则回退 hbbr 的 TCP 中继，端到端加密与传输方式在直连/中继上完全同构。

## 打洞流程（时序）

1. A 并行起 UDP 测绘：独立 UDP socket 发 `TestNatRequest` 给 hbbs，hbbs 回 `TestNatResponse.port`（它观察到的 A 的 UDP 公网端口），**该 socket 即后续打洞 socket，端口不换**（`src/client.rs:496-515, 5414-5511`；注释明言禁止用别的 socket 的 STUN 结果，`src/client.rs:5420-5423`）。
2. A→hbbs（TCP 21116，可 `secure_tcp`）：`PunchHoleRequest{id, nat_type, udp_port, force_relay, socket_addr_v6, webrtc_sdp_offer}`（`src/client.rs:953-969`）。
3. hbbs 经 B 的注册心跳通道下发 `PunchHole{socket_addr(=A 的 TCP 公网地址), udp_port(=A 的 UDP 端口), nat_type, webrtc_sdp_offer}`（`rendezvous.proto:73-84`；`src/rendezvous_mediator.rs:539-545`）。
4. B 决策（`src/rendezvous_mediator.rs:1091-1149`）：任一方 SYMMETRIC / force_relay / 代理 → 直接 `RequestRelay`；否则 `ph.udp_port>0` 走 UDP 打洞，`peer_addr.set_port(ph.udp_port)`（`:1145-1146`）；无 UDP 端口则走 TCP punch。
5. UDP punch：B 用新 socket 向 hbbs 发 UDP 版 `PunchHoleSent`（先把 B 的 NAT 映射打开）→ `udp_nat_listen`：`connect(A的公网:udp_port)` + `punch_udp(listen=true)`（`src/rendezvous_mediator.rs:1198-1227, 1464-1495`）。A 收到 `PunchHoleResponse{is_udp, pk}` 后把测绘 socket `connect` 到 B 并 `punch_udp`（`src/client.rs:1030-1036, 5514-5532`）。
6. 探针协议（`src/common.rs:2884-2960`）：64bit 随机 tid 的 PROBE，20ms 起 1.5 倍退避至 200ms，3s 超时；收到对方 PROBE 立即回同 tid ACK，双向 PROBE/ACK 交叉即洞成。
7. 失败回退：`RelayResponse` 后直连与中继 `select_ok` 赛跑，直连赢（`src/client.rs:1051-1195, 1522-1535`）。TCP punch 是同源地址 simultaaneous open（B 复用中介 socket 的 local_addr 反复连 A，0.15s→2s 退避，`src/rendezvous_mediator.rs:1166-1182, 1499-1520`）。

## 直连传输（KCP 参数）

- UDP 洞上跑 KCP：`kcp-sys`（rustdesk-org fork）`new_turbo` 配置；可选开启拥塞窗口 `nc=0`（默认 turbo nc=1 无拥塞控制），由 `get_kcp_cc_enabled` 开关控制，收发端无需协商（`src/kcp_stream.rs:29-43`）。rustdesk 不显式设 mtu/窗口，全用 kcp-sys 默认。
- KCP 流外面套与 TCP 完全相同的 `FramedStream(BytesCodec)`，上层无感知（`src/kcp_stream.rs:53-60`）。
- 分帧是自定义变长前缀：长度左移 2 位带 2bit 标记，按大小用 1/2/3/4 字节 LE 编码（≤0x3F/0x3FFF/0x3FFFFF/16MB）（`hbb_common/src/bytes_codec.rs:123-144`），大消息可选 zstd（`hbb_common/src/compress.rs`）。
- 视频：编码后的 `EncodedVideoFrames` 作为 protobuf `Message::video_frame` union 与输入/剪贴板共挤一条流（`libs/base/protos/message.proto:4-37`）——单连接单流复用，无独立视频通道。

## 中继与加密

- hbbr 中继是**纯 TCP 长连接**（默认 21117，`hbb_common/src/config.rs:121`）：双方各连一次，首条消息带同一 uuid，hbbr 按 uuid 配对后原样转发字节（`src/client.rs:1862-1876`；B 侧 `src/rendezvous_mediator.rs:1117-1131`）。hbbr 本身不做应用层加密。
- 加密端到端在双方之间协商：hbbs 用其 Ed25519 私钥签 B 的 `id+公钥` 下发，A 验签后双方 `crypto_box` ECDH 出对称密钥对后续 `Message` 流加解密；密钥缺失/验签失败则明文降级（`src/client.rs:1653-1785, 5743+`）。因此中继链路上跑的也是密文。

## NAT 检测

`test_nat_type_`（`src/common.rs:705-753`）：同一本地地址连 hbbs 的两个端口（21116 与 21115，`increase_port(-1)`），两次 `TestNatResponse.port` 相等 → `ASYMMETRIC`（锥形，可打洞）；不等 → `SYMMETRIC`。判定是**二值**的，没有完整 STUN RFC 分类。决策用法：任一方 SYMMETRIC → 直接放弃 v4 直连转中继（`src/rendezvous_mediator.rs:1091-1094`）；对端 SYMMETRIC 时打洞超时压到 1s（`src/client.rs:1432-1433`）；socks/ws 代理一律按 SYMMETRIC 处理（`src/common.rs:668-671`）。UDP 端口测绘与 TCP 的 NAT 类型检测是两套独立机制。

## WebRTC 新路径

- 实现：webrtc-rs 的 rustdesk-org fork（pinned rev，`Cargo.toml:254-256`），ICE/DTLS/SCTP 全套。
- 携带内容：**单个有序且完全可靠**的 DataChannel（label "bootstrap"，收到乱序/部分可靠通道直接拒收，因分片重组依赖有序性，`hbb_common/src/webrtc.rs:738-751`），上面仍跑同一套 protobuf `Message` 流（`Stream::WebRTC`，`src/client.rs:1515`）——不是 media 通道。
- 信令：SDP offer 塞进 `PunchHoleRequest.webrtc_sdp_offer`、answer 走 `PunchHoleSent.webrtc_sdp_answer`，信令通道强制加密否则放弃（`src/client.rs:873-885`；`src/rendezvous_mediator.rs:1151-1163`）；trickle ICE 经 `IceCandidate{session_key}` 定向。
- 防 MITM：要求对方 Ed25519 签名其 DTLS 指纹并与实际指纹绑定，不符即断（`src/client.rs:1709-1719`）。SCTP 拥塞控制默认关闭（webrtc-sctp 的 1s RTO 地板太亏，`Cargo.toml:239-242`）。

## 已知缺陷与教训

- **#16488 PoC 结论**：旧版把"TCP 汇合连接观察到的地址"直接当 UDP 打洞目标，CGNAT/4G 下 TCP 与 UDP 映射不一致导致打洞必败；作者改用专用 UDP socket + STUN + 双向同时打洞后全部组合可直连。维护者回应 "We focus on webrtc now"，未合入修复。
- **当前 master 状态**：该缺陷在 v4 UDP 路径上已实质修复——打洞端口只来自打洞 socket 本身的 UDP `TestNatResponse`（`src/client.rs:5420-5423, 5479`），且目标端口由 `PunchHole.udp_port` 单独传递（`src/rendezvous_mediator.rs:1145`）。但两点残留：① 对称 NAT 仍按设计直接转中继（不做端口预测），CGNAT 若被分类为 symmetric 则永远享受不到直连；② UDP 打洞的**公网 IP** 仍取自 TCP 汇合观察（`ph.socket_addr` 的 IP + `udp_port` 换端口），TCP/UDP 出口 IP 不同的罕见多出口 NAT 下仍会指错。
- 教训：端口映射不可跨协议/跨 socket 推断；打洞 socket、测绘 socket、信令通道必须分离且映射可复用；"对称→中继"的降级闸门换来了确定性，也放弃了端口预测的成功率。

## 对"模块化规范"的启示

1. **传输与业务解耦到极致**：直连(KCP/TCP)/中继/WebRTC 全部收敛为一个 `Stream` 枚举 + 同一 `Message` 协议，加密握手（`secure_connection`）对所有传输复用——新传输只需实现 Stream trait。
2. **决策用小而足够的信息**：NAT 检测只有 3 个值、1 次双端口探测，配合 `direct_failures` 历史动态调整打洞预算，而非完备的 STUN 分类。
3. **每条路径独立可赛跑**：直连/中继/WebRTC 作为并行 future `select_ok`，先成者赢、P2P 优先窗口兜底；失败路径自动降级，永不阻塞用户。
4. **端到端加密不信任任何中转**：hbbs 只做签名身份分发，hbbr 只搬字节，密钥协商在两端完成——中继被攻破也只有密文。
