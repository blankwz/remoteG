# T-027 视频切 fec-shards（协议 v0.2 媒体路径）
- 所属 Epic/Story：M2 / 05_api §6.4 候选「视频 fec-shards」
- 负责 Agent：A1 基础设施与协议
- 优先级：P1
- 预估：2~3d
- 前置依赖：T-004；switchboard v0.1.3（`sb.moonlight.fec-rs` 已端到端验证：switchboard-mod-quinn 集成测试 3/3）
- 可并行组：与 T-026 手测解耦（**不得在 M1 手测窗口合入**——会作废预构建产物与手测基线）

## 目标
视频帧从「quinn 可靠流（`send_video`）」切换到「quinn datagram + RS FEC + 丢帧」（switchboard `fec-shards` profile），消除丢包/中继路径上的「可靠流重传等待 + 整帧 IDR」放大效应。文字清晰路径（QOI 瓦片/终端）不受影响。

## 上下文（只读）
- switchboard/spec/01 §6（Channel 契约）、§8（fec-shards profile）
- switchboard/docs/cards/moonlight-gamestream.md（RS + 动态比例 + LTR/RFI 恢复）
- switchboard/sdk/rust/switchboard-mod-quinn/src/lib.rs 集成测试（fec-rs × quinn × 弱网 3/3 绿）
- crates/rd-switch（RdChannel：rd Conn 的 sb Channel 视图，datagram 已适配）
- 05_api/协议契约.md §3（当前视频流帧格式）、§6.4（v0.2 候选）

## 修改范围（可写）
- `proto/rd.proto`：`Hello` 增 `bool media_fec = 5;`、`HelloAck` 增 `bool media_fec = 6;`（附加字段，线上兼容）
- `apps/rd-agent/src/session.rs`：握手期协商 `media_fec = hello.media_fec && ack.media_fec`，写入会话配置
- `apps/rd-agent/src/window_session.rs`：send 路径分支——`media_fec=true` 时 AU（含 `VideoHeader` 前缀）经 `FecRs` FrameSink 走 `Conn::send_datagram`；保留现路径为回退
- `apps/rd-controller/src-tauri/src/conn.rs`：`media_fec=true` 时视频泵改为「datagram 循环 → `FecRs` FrameSource → `encode_video_frame` → 前端」；**datagram 需 1 字节端口前缀**（`0x01`=fec-shards 分片、`0x02`=输入回执）——fec 协商成立时双侧加前缀，旧版本混跑不触发
- `crates/rd-transport`：不动（`Conn::send_datagram/read_datagram` 已有；发送缓冲 64KB 已验证）
- `apps/rd-agent/tests/e2e.rs`：新增 `t09_media_fec_video`（协商 media_fec → 收帧断言同 t02），并将 `drop_to_idr` 断言适配为「丢帧后 LTR/IDR 恢复」

## 实现步骤
1. proto 字段 + 重新生成（build.rs prost），`PROTO_VER` 不变（附加字段向后兼容）。
2. agent：`window_session` 增加 `media_fec: bool`；true 时发送循环改 `FrameSink::submit`（帧号全局单调；队满弃帧事件入日志）。
3. controller：视频泵双路分支（协商标志决定）；datagram 循环必须容错 `RecvError::Closed`（对端旧版本不发 datagram → 优雅降级回退提示）。
4. 输入回执前缀：仅 media_fec 会话启用；controller 发输入 datagram 加 `0x02` 前缀，agent 收侧按前缀分流（无前缀=旧版对端，走原逻辑）。
5. e2e：t09 + 全套串行回归（`--test-threads=1`，套件内编码器冷启动容差 15s 已于 2026-10-24 调整）。

## 接口
```rust
// 发送侧（agent，media_fec=true 时）
let (sink, _src) = fec_rs.bind(ch_tx.clone(), ch_tx.clone(), bus).await?;   // 收发同 Conn
sink.submit(EncodedFrame { frame_id, data: Bytes::from([VideoHeader][AU]) }).await?;
// 接收侧（controller）
match frame { FrameIn::Complete { data, .. } => 前端投递(encode_video_frame(parse_header(&data), &au)),
              FrameIn::Lost { .. } => request_idr() /* 或 LTR（NVENC 驱动 ≥610 后） */ }
```

## 验收
1. `cargo test --workspace` 全绿（e2e 串行）；t09 覆盖「2% 丢包仿真下视频可恢复」（净流 + TOS 上限仿真）。
2. 真机手测：弱网（热点限速）下视频不「顿成幻灯片」；丢帧恢复 ≤1s（对比现路径）。
3. 码控联动：`FrameSink::submit` 的 QueueDepth 接入 `rate.rs`（v0.2 baseline-adaptive 已就位）。

## 已知风险
- fec 分片使单帧网络开销 ×1.2（默认 ratio 0.2）——弱网换恢复，LAN 由 AIMD 上限兜底；
- `FecRs` 接收乱序窗仅 1 帧，QUIC datagram 乱序概率低但非零——e2e 观察后再放宽；
- NVENC LTR 依赖驱动 ≥610（当前 572.70 阻塞，见未决事项 3）——先落 IDR 兜底。
