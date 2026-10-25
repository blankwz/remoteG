# rd（本项目）QUIC 栈原理卡片

- 来源：REQ-20261013-001 仓库 `crates/rd-transport/`、`crates/rd-encode/src/rate.rs`、`apps/rd-agent/src/discover.rs`、git log
- 对应模块：`sb.quinn.quic`、`sb.lan.udp-broadcast`、`sb.rd.reliable-idr`（profile）

## 一句话原理
quinn QUIC 承载一切（控制流/视频流/终端流 + 证书指纹钉扎），视频走可靠流 + 陈旧帧丢弃 + IDR 恢复，码控 AIMD。

## 关键机制
1. **传输**：ALPN `rd/0`；keepalive 5s / idle 20s；MTU 发现关闭、钳 1200（tailscale 接口 MTU 1280 的实测教训，conn.rs:125 附近注释）。
2. **视频路径**：QUIC 可靠流（conn.rs `DataTx::send_video` write_all）；发送侧通道积压 ≥2 帧时跳帧并置 `drop_to_idr`，编码循环下帧强制 IDR（window_session.rs D-013 包2）。
3. **码控**：AIMD 200ms 反馈——拥塞信号（积压>1帧 / RTT 超基线 50% / 丢包>0）→ ×0.7；连续 4 tick 无拥塞 → ×1.15；上限 50Mbps（LAN 带宽换画质）；积压信号需连续 2 tick 防误降（鼠标 hover 风暴实测教训）。
4. **发现**：UDP 21119 广播 `RD_DISCOVER`，应答 `RD_AGENT_v1|主机名|指纹短hex|端口`；只应答不主动发；发现 ≠ 信任。
5. **安全**：自签证书 + 指纹钉扎（TOFU+6 位码配对）。

## 已知缺陷与教训
- 可靠流在丢包/中继路径上是延迟放大器（丢 1 包整帧卡 RTT+）；恢复靠整帧 IDR，差链路恶性循环 → 规范 `fec-shards` profile 的反面对照组。
- DERP 中继 RTT 1.2s 时配对握手必超时 → RPC 5s→30s 是补丁不是根治（根治 = 自建近处中继）。

## 对规范的启示
- `reliable-idr` profile 登记在案（01 §8），作为 fec-shards 的对照基线；
- LAN 广播发现的安全基线（只应答/公开信息/不授信）写入 01 §3 MUST；
- caps 实测经验：隧道内 MTU 1280 → 应用层钳 1200 写入 02 §2.3 示例。
