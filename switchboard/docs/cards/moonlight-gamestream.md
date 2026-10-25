# Moonlight / GameStream 视频传输原理卡片

> 依据 moonlight-stream/moonlight-common-c 真实源码（master, 2026-10），所有论断带 文件:行号。

## 一句话原理

视频走"裸 RTP + 逐帧 Reed-Solomon 前向纠错"的纯 UDP 单向流，**没有任何包级重传（无 ARQ）**；丢包先靠 FEC 恢复，救不回来就整帧丢弃并通知主机做参考帧失效（RFI）或请求 IDR 重编码；控制与输入另走一条 ENet 可靠 UDP 通道。

## 分包与传输

- 一帧切为固定大小分片：非末片 payload 恒等于 `packetSize`，末片可短（RtpVideoQueue.c:738）。`packetSize` 由客户端声明、向下取 16 的倍数（Connection.c:293-295，"FEC 只按 16 字节块工作"）；远程 IPv4 强制 1024、IPv6 1184（Connection.c:412-424）；开视频加密再减 32 字节头（SdpGenerator.c:324-328）。
- 包结构 = RTP 头(12，带扩展则 16)（Video.h:37-40）+ NV_VIDEO_PACKET 头(16：streamPacketIndex/frameIndex/flags/fecInfo 等，Video.h:27-35) + 数据。flags 标 SOF/EOF/含图像数据（Video.h:21-23）。
- 帧首片带 8 或 44 字节帧头（首字节 0x01/0x81 区分，随版本漂移），byte3 为帧类型：1=普通 P、2=IDR、4=帧内刷新、5=参考帧已失效的 P（VideoDepacketizer.c:858-887, 915-966）。
- **RTP 序列号是"帧内连续块"语义**：帧首包 seq 减 fecInfo 中的 fecIndex 得本帧最低 seq；数据分片在前、parity 分片紧随其后（RtpVideoQueue.c:584, 701, 712-713）；帧边界靠 32 位 frameIndex。RTP timestamp 为 90kHz（RtpVideoQueue.c:17-18）。

## FEC 机制（含公式）

- 比例由**主机逐帧**下放在 fecInfo：`data=(fecInfo>>22)&0x3FF`、`pct=(fecInfo>>4)&0xFF`、`parity=⌈data×pct/100⌉`（RtpVideoQueue.c:709-711）。
- 纠错**不是 XOR 而是 Reed-Solomon**：`reed_solomon_new(data, parity)` + `decode`，可恢复任意 ≤parity 个丢包（RtpVideoQueue.c:269, 332）；Gen5 以下不支持恢复（:254-259）。
- **"最多 200%"**：SDP 协商 `minRequiredFecPackets=2`，小帧至少 2 个 parity 分片——单数据分片的帧比例抬到 200%（SdpGenerator.c:207-211）。大帧反而可能 pct=0 整帧关 FEC（RtpVideoQueue.c:204-205）。
- 动态 FEC 是主机行为：GFE 3.26 的 BLL-FEC 随丢包/码率自调，被客户端显式关闭退回静态比例（SdpGenerator.c:213-218）；Sunshine 收 `x-ml-video.configuredBitrateKbps` 后自行调（SdpGenerator.c:369-373）。
- 客户端预算：发送码率 = 用户码率×0.80，留 20% 给 FEC/音频（SdpGenerator.c:337-338）。一帧可拆多个 FEC block（multi-FEC，主机≥7.1.431，RtpVideoQueue.c:25, 585-590）。

## 丢帧与恢复

- 无 ARQ。恢复链 = FEC → 整帧丢弃 → 通知主机。
- **RFI**：`SS_RFI_REQUEST(0x0301)` 带 [startFrame,endFrame] 区间，走 reliable+URGENT 通道，多个丢失区间聚合成一个（ControlStream.c:1536-1558, 1595-1617；Video.h:80-86）。主机令参考帧失效，后续 P 帧改引未失效帧（帧头 type=5 标记），无需 I 帧。
- 前提：DESCRIBE 发现 `x-nv-video[0].refPicInvalidation` 且解码器声明 CAPABILITY_REFERENCE_FRAME_INVALIDATION_AVC/HEVC（RtspConnection.c:1138-1142；Limelight.h:252-256）；4K+GFE 直接禁 RFI（Connection.c:325-330）。
- **IDR 兜底**：`LiRequestIdrFrame`→0x0302；不支持 IDR 消息的老主机用覆盖最近 0x20 帧的 RFI 逼出 IDR（ControlStream.c:1488-1534）。防拥塞：丢包后先等"下一个完整帧"证明网络恢复才发 IDR（VideoDepacketizer.c:1082-1089）；连续丢 120 帧强制 IDR 重置（VideoDepacketizer.c:30, 120-129）。
- **投机 RFI**：从未见过乱序包时，可在确认不可恢复前预判丢失、提前发 RFI 省 1 个 RTT；预测错了冷却 5 分钟（RtpVideoQueue.c:15, 215-226, 236-241）。
- **LTR 提案（issue #120）**：用长期参考帧取代 RFI——主机周期编码 LTR 并在 extraFlags 标记（Video.h:25），客户端 FEC 校验通过后回 `LTR ACK(0x0350)`（ControlStream.c:1560-1578；VideoDepacketizer.c:1125），主机才把该帧钉进 DPB 当恢复锚点；丢包时 RFI 请求被当 NACK，主机编码引用 LTR 的恢复帧，免 IDR。PR #122 已实现 ACK，issue 仍开放。

## 控制与输入通道

- Gen5+ 全走一条 **ENet reliable-UDP**（端口来自 RTSP SETUP，默认 47999），Gen3/4 为 TCP 47995（ControlStream.c:1751-1848）。按语义分 0x30 个 channel：0=generic、1=urgent(IDR/RFI/LTR-ACK)、2/3=键盘/鼠标、0x10+=手柄（Limelight-internal.h:57-65）。
- 输入大多 reliable；可批量的触摸/鼠标移动/体感发不可靠包、丢旧留新（InputStream.c:554-558, 1433）。
- 主机≥7.1.431 整条控制流 AES-GCM 加密：单调 seq 作 IV，v2 用 12 字节 IV 加方向标签防碰撞（ControlStream.c:26-32, 548-591）；视频包同密钥同算法（ENC_VIDEO_HEADER：12B IV+16B tag，Video.h:15-19；VideoStream.c:209-222）。
- **统计**：每 100ms 发 reliable ping——目的就是逼 ENet 刷新 RTT（ControlStream.c:1424-1440），经 `LiGetEstimatedRttInfo` 暴露（:1725-1745）；老版本每 50ms 发 0x0201 Loss Stats 带 lastGoodFrame（:1456-1481）；Sunshine 额外收逐帧 FEC 状态 0x5502 供主机调 FEC（Video.h:57-70；ControlStream.c:1404-1419）。客户端不做自适应码率（SDP 锁死，SdpGenerator.c:357-367），只向 UI 报质量：3 秒采样，丢帧 ≥30% 或连续两周期 ≥15% 判 Poor、≤5% 恢复 Okay（ControlStream.c:125-128, 488-511）。

## 会话协商（RTSP）

序列：OPTIONS → DESCRIBE → SETUP(audio/video/control 各一次，返回三个 UDP 端口，默认 48000/47998/47999) → ANNOUNCE(客户端 SDP) → PLAY（RtspConnection.c:1047-1413）。DESCRIBE 响应夹带主机能力：AV1/HEVC 探测（:1090-1123）、RFI 支持（:1139）、加密能力 `x-ss-general.encryptionSupported`（:1150）。ANNOUNCE 的 SDP 承载全部参数：分辨率/fps/packetSize（SdpGenerator.c:315-330）、码率、`fec.enable=1`+repairPercent（:387, 226-231）、编码器/HDR/参考帧数（:454-479）、Opus 声道与包时长、加密开关（:303-304）；`useReliableUdp=13` 选加密控制协议，drc/recovery 显式关闭（:205-251）。

## 对"模块化规范"的启示

**值得成为标准契约**：① 逐帧自描述的 FEC（比例随帧下发 + RS(n,k) 数据/校验同块）；② 帧级三态恢复语义（FEC 恢复 / RFI 降级 / IDR 兜底）与"等网络稳定再要 IDR"的拥塞礼貌；③ 控制通道按语义分 channel、可靠性按消息正交选择；④ 统计回程闭环（fecInfo → 0x5502 → 主机调参）；⑤ 传输与纠错参数随网络场景（本地/远程、IPv4/IPv6）显式协商。
**属其私有实现，不宜照抄**：NV 的 x-nv-* SDP 魔法属性族、随版本漂移的 8/44 字节帧头（VideoDepacketizer.c:915-966）、ENet 协议绑定、名为 remoteInputAesKey 却兼护视频音频的密钥复用。
