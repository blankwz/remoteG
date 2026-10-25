# Sunshine/Moonlight 编码管线原理卡片

- 来源：docs.lizardbyte.dev（Sunshine 官方文档）、moonlight-common-c（SdpGenerator.c）、NVIDIA 低延迟编码指南、社区 NVENC 解析
- 对应模块：`sb.codec.sunshine-presets`（即规范 `h264.ultralowlat` 参数档来源）

## 一句话原理
低延迟 = 捕获零拷贝 + 硬编极限档 + 无 B 帧 + CBR + 按需关键帧；画质靠码率与 preset 平衡，不靠排队。

## 关键机制
1. **捕获**：Windows 用 DXGI Desktop Duplication（全屏级，NVIDIA 卡可 NvFBC 历史），帧直接进 GPU 编码器，避免 CPU 读回。
2. **NVENC 参数档（P1-P7）**：P1-P3 = 编码耗时最短（P1 ~几毫秒级），是低延迟首选；P6/P7 追求压缩率但延迟翻倍。
3. **低延迟四件套**：CBR（恒定码率平滑带宽）、0 B 帧（无重排序延迟）、关 lookahead、单参考帧。
4. **关键帧策略**：不做周期性 I 帧（码率尖峰 + 卡顿）；客户端加入/丢帧才请求。
5. **intra-refresh**（可选）：用逐列刷新替代 I 帧，消除码率尖峰（NVIDIA 论坛/文档有示例，Sunshine 有开关）。
6. **FEC/传输**不在编码层——由 Moonlight 协议层负责（见 moonlight-gamestream.md），编码只管出低延迟码流。

## 已知缺陷与教训
- preset 过高（P6+）是"画面发糊/顿挫"的常见根因（编码排队）；
- 4:4:4 硬编受驱动限制（rd 实测 NVIDIA 驱动 <610 阻塞 444 探针），Sunshine 同样受此约束。

## 对规范的启示
- `h264.ultralowlat` 参数档（01 §9）：P1-P3 + CBR + 0 B 帧 + no lookahead + on-demand keyframe；
- `h264.lan444` 档：LAN 高带宽模式（Pyrowave 定位的低成本近似，rd 用 QOI 瓦片实现）；
- 编码层与传输/媒体适配解耦——Sunshine 是"只做编码"的阶段纯粹性范本（01 §6 MUST NOT 反例的对立面）。
