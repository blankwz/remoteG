# UU 对标与中继落地总结（T-031 / T-032，2026-10-31 ~ 11-01）

> 给谁看：项目所有者与后续接手的 AI/工程师。
> 一句话：对照网易 UU 远程的差距分析结论，**全部代码缺口已实现完毕**——文件传输、
> WOL 远程开机、rd 原生中继（rd-relay）三件大事 + 两条备用中继路线的部署套件。

---

## 一、起点：UU 对标结论（2026-10-31 分析）

对标网易 UU 远程（免费 4K/144、加速器节点网络、游戏场景钩子），rd 的差距收敛为三类：

| 类别 | 项 | 结论 |
|---|---|---|
| 硬伤 | 打洞失败后无中继兜底（官方 DERP 国内 1.2s+，自建 DERP 步骤多） | **本次已解决**（三条路线，见 §4） |
| 刚需缺口 | 文件传输 / 隐私屏 / WOL 远程开机 | 隐私屏已有（T-028）；**文件传输 + WOL 本次落地**（T-031） |
| 明确不追 | 4K/144 高刷、移动端、账号体系、键位映射 | 与「个人窗口级工具」定位冲突；移动端语音输入已记忆为远期需求 |

UU 受欢迎的根因（免费 × 节点基建 × 游戏场景 × 零门槛）均不可复制也不必复制；
rd 的独有牌是**窗口级控制 + 终端自绘**。操作侧借鉴（连接前零决策/功能藏会话工具条/
默认即最优）已在此前 UX 轮次（R1~R15）落地。

## 二、T-031 文件传输 + WOL（提交 93dcc24）

### 文件传输（协议 v0.2）
- **协议**：`FileListReq/Ack`(45/46)、`FileXferOffer/Chunk/Ack/Cancel`(47~50)，
  分片全部 `req_id=0` 按 `xfer_id` 路由；`HelloAck.mac_addrs=7`。
- **被控端** `file_handler.rs`：接收固定 `Downloads\rd接收` + 文件名消毒（防穿越/
  非法字符/Windows 保留名）+ 重名 `(n)` 去重；拉取后台任务 + 取消令牌 + RAII 摘表；
  断开清理半成品；审计 `file_send_done`/`file_recv_done`（只记名字与字节数）。
- **控制端** `commands/files.rs` + 前端 `FilesPanel` 浮层：发送（对话框多选）、
  远端浏览（以回传绝对路径为准）、拉取到本机下载目录、进度节流（≥1MiB 才发一条
  IPC）、30s 分片空闲超时、`create_new` 原子占位防并发同名截断。
- **验证**：e2e t10 推送+拉取逐字节比对；双端单测（消毒/去重/列目录排序/接收目录
  重定向 RD_RECV_DIR）。

### WOL 远程开机
- 被控端 `netinfo.rs`（GetAdaptersAddresses 只收以太网/Wi-Fi 物理网卡 MAC）→
  `HelloAck.mac_addrs` → 控制端存 devices.json → `wake_device` 命令（魔术包
  广播 5×双端口 9/7，3s 去抖）→ 设备卡片「远程开机」菜单。
- 局限（诚实）：仅同网段有效；跨网需路由器辅助转发（按钮 tooltip 已说明）。

### code-review 修复轮
2 HIGH（面板首页主流程坏 / 对齐 UB）+ 6 MED + 6 LOW 全闭环，backlog 清零（唤醒
去抖已顺手实现）。

## 三、T-032 rd 原生中继 rd-relay（本次新增）

### 设计（第一性原理）
中继的本质 = 帮两条 NAT 内的 UDP 流互相找到对方。**不走流拼接**（那要终结两端
QUIC、中继可见明文，破坏「中转无法窥屏」承诺），做 **UDP 打标转发**：

```
数据报:  [u64 tag][QUIC 原始包]     tag = 对端证书指纹前 8 字节（无需协商）
控制报:  [u64 0][REG/DIAL][32B 指纹] → 应答 [status][tag]
被控端启动时 REG 注册；控制端连接时 DIAL 按指纹呼叫；随后原样转发密文。
```

- **rd 双端零协议改动**：`rd-transport/relay.rs` 把 tokio UDP socket 包成
  `quinn::AsyncUdpSocket`（发=加 tag 发中继；收=验 tag 剥头），quinn 全程以为
  在和中继说 QUIC——配对码、双向证书认证、指纹钉扎**与直连完全一致**。
- NAT 保活：quinn keep-alive 5s 持续出包；会话空闲 60s 回收；数据报 ≤1500B。
- 单指纹单会话（与免费版 1 并发一致）；中继不鉴权注册，但"连上"≠"能控制"
  （QUIC 层安全不变）。

### 实现
- `crates/rd-relay`：中继服务端（单文件 exe，零外部依赖；REG/DIAL/转发/GC；
  2 单测含双向转发与 NOT_FOUND）。
- `crates/rd-transport/relay.rs`：`connect_via_relay` / `bind_via_relay` /
  `RelayUdpSocket`；conn.rs 提取 `server_config`/`client_config` 并新增
  `bind_over`/`connect_over`。
- **agent**：`agent.toml relay_addr` 或环境变量 `RD_RELAY` → 启动时异步注册，
  失败仅降级（直连不受影响）。
- **controller**：devices.json 增 `relay` 字段 → `DeviceConn::connect` 直连失败
  **自动回退中继**（首配对无指纹不回退——DIAL 按指纹寻人）；设备编辑对话框新增
  「原生中继」字段；超时策略：配中继 30s / 纯直连 3s。
- **验证**：rd-transport e2e「经中继的完整握手与控制流回环」（真实 rd_relay 实例
  + QUIC 双向认证 + 指纹钉扎断言 + Envelope 往返）。

## 四、三条中继路线（部署套件齐备）

| 路线 | 目录 | 适用 | 部署难度 |
|---|---|---|---|
| **C：rd-relay 原生（推荐）** | `switchboard/deploy/rd-relay/` | 默认选择；被控端零安装 | ★ Workers 免费 / Docker 一条命令 / SSH 一条命令 |
| A：frp 一键 | `switchboard/deploy/relay-frp/` | 不想动 rd 配置时的替代 | ★ 一条 SSH 命令 |
| B：自建 DERP | `switchboard/deploy/derper/` | 要整套 tailnet 组网 | ★★★ Docker+域名 |

路线 C 参考 alma-relay 的部署体验，提供**两种中继实现**（协议一致，rd 客户端按
地址格式自动选择：`https://…` → WS 版、`ip:port` → UDP 版）：
- **UDP 版**（`deploy-vps.sh`）：打标 UDP 转发，VPS 直跑，延迟最优；
- **WS 版**（`cloudflare/worker.mjs` + `node/relay.mjs`）：WebSocket 二进制帧桥接，
  可上 **Cloudflare Workers 免费版**（免 VPS、自带域名与 TLS）或 Docker+Caddy
  一条命令自动 HTTPS——对标 DERP「HTTPS 中继穿墙」的设计哲学。
- **设置页 UI**（设置 → 远程访问 → 使用自建中继）：被控端中继地址不再需要手改
  agent.toml（UI 写入 relay_addr，重启生效）。

## 五、验收状态

| 项 | 结果 |
|---|---|
| Rust workspace 全量 | 308+ 用例 0 失败（双跑；T-031 时点） |
| rd-relay 单测 | 2/2（REG/DIAL 双向转发、NOT_FOUND） |
| rd-transport 经中继 e2e | QUIC 双向认证 + 指纹钉扎 + Envelope 往返 ✅ |
| 前端 | 140 用例 + tsc 干净（T-031 时点；本轮 UI 改动后复验通过） |
| clippy `-D warnings` / fmt | 绿（T-031 时点基线） |
| 脚本语法 | bash -n / PS Parser 全过 |

## 五b、T-033 rd-relay WS 版 + 设置页（2026-11-01，参考 alma-relay）

- **Cloudflare Workers 版**：`deploy/rd-relay/cloudflare/`（Durable Object 按 tag
  配对、WS 二进制直通、/healthz；Free 计划 + Hibernation）。
- **Node 版**（零依赖，含极简 RFC6455 实现）：`deploy/rd-relay/node/`；
  `docker-compose.yml` + `Caddyfile` 一条命令自动 HTTPS。
- **Rust 客户端**：`rd-transport/relay_ws.rs`——`RelayEndpoint`（UDP|WS 按地址
  自动选择）、`RelayUdpSocket` 的 WS 变体（QUIC 包过 WS 二进制帧）、
  tokio-tungstenite（rustls）。
- **设置页**：设置 → 远程访问 → 使用自建中继（get/set_relay_addr 命令，
  写被控端 agent.toml relay_addr，重启生效）。
- **验证**：rd-transport 假 WS 中继 e2e（QUIC 双向认证+指纹钉扎+Envelope 往返）
  与 UDP 版对称；前端 140 用例 + tsc；两个 JS 版 node --check。

## 六、剩余事项（全部需要外部资源或用户操作）

1. **真机手测**：`09_test/手测清单.md` 1.14（文件传输）/ 1.15（WOL）；
   中继路线建议补一轮跨网实测（两台真机 + VPS）。
2. **VPS**：三条路线任选其一部署（推荐路线 C，脚本已就绪）。
3. **上线前换正式授权签名密钥**（当前为公开开发种子）。
4. 远期：中继多会话（Pro 多控）、文件流拆独立 QUIC 流（消除大文件传输期输入
   延迟）、移动端 + 语音输入（协议 TextInput 通道是前置）。

## 七、提交索引

- `93dcc24` feat: T-031 文件传输 + WOL 远程开机
- `5f011cc` chore: T-031 收尾（去抖 + 手测清单 + release 重建）
- `05ed701` feat: 中继部署小白化（relay-frp 路线 A + derper README 重写）
- 本次：T-032 rd-relay（协议无关的传输层新能力）+ 路线 C 部署套件 + 本文档
