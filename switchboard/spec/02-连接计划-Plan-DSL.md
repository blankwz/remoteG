# 02 连接计划（Plan DSL）

- 状态：规范性（Normative）
- 版本：0.1

Plan 是 Studio 的编辑对象、执行器的输入、模块组合的**可移植载体**。换实现不换计划（P2）。

## 1. 语法（TOML）

```toml
spec = "switchboard/0.1"
name = "family-help-v1"

[session]
role = "dial"                                  # dial（控制端）| listen（被控端）——评审修订 #3
identity = "file:%APPDATA%/rd/identity.pem"    # 身份密钥；PeerId = 身份公钥指纹（fallback_relay.alloc 的 peer 来源）
trust = "fingerprint-pinning"                  # fingerprint-pinning | psk
pairing = "code6"                              # code6 | pre_shared_fp
connect_confirm = true                         # 被控端连接确认框（rd D-013 包3；仅 listen 角色生效）

# ── 候选路径：按序执行，首个成功者胜出；支持事后升级 ──
[[path]]
name = "lan-direct"
discovery = "sb.lan.udp-broadcast"
transport = "sb.quinn.quic"
budget_ms = 1500                               # 本路径总预算：按 §5 规则切分到阶段

[[path]]
name = "tailnet"                               # process 绑定：整栈即模块
transport = "sb.ext.tailscale"                 # 内含 DISCO 打洞 + DERP 中继（stages 为数组，见 06 §2）

[[path]]
name = "stun-punch"
punch = "sb.rustdesk.punch-v2"
rendezvous = "hbbs://self-host.example"        # needs_rendezvous 模块必须给出会合点
transport = "sb.quinn.quic"
fallback_relay = "sb.derp.self-hk"             # punch 失败 → 该中继，同 transport

# ── 升级策略：已建 Channel 的后验优化（评审修订 #4：不参与初始路径选择） ──
[upgrade]
mode = "relay-first"                           # relay-first | direct-only
probe.punch = "sb.rustdesk.punch-v2"           # 直连探测显式绑定模块（候选复用已有 discovery）
probe_interval_ms = 5000                       # 直连探测周期
settle_ms = 800                                # 直连稳定多久后才切换（防抖）

# ── 重连（评审修订 #3）：断线后的行为 ──
[retry]
mode = "replan"                                # replan（重跑整个路径序）| same-path（原路径退避重试）
backoff_ms = 1000                              # 指数退避起点
backoff_max_ms = 30000
max_attempts = 0                               # 0 = 无限（帮家人场景）

[media]
profile = "fec-shards"                         # 01 §8 三选一
codec = "h264.ultralowlat"
adapter = "sb.core.fec-xor"                    # 示例必须引用注册表真实 id（评审修订 #6）
[media.control]                                # 码控：消费遥测的规则集
rules = "sb.qos.baseline-adaptive"             # RustDesk 1.5.0 QoS 规则
limits = { min_kbps = 500, max_kbps = 50000, relay_kbps_cap = 8000 }
```

## 2. 语义

### 2.1 路径选择（path）

1. 按 `[[path]]` 声明序执行；任一路径产出**指纹互认后的 Channel** 即成功。
2. `fallback_relay`：punch 预算耗尽 → 用该中继继续（同 transport），路径仍算成功但 `Channel.stats.path.kind = relay`。
3. 全部失败 → `PlanError::NoPath`，执行器 MUST 输出各路径失败原因（可观测，不许静默）。

### 2.2 升级（upgrade）

- `relay-first`（默认，Tailscale/Steam SDR 同款体验）：先用中继保通，后台按 `probe_interval_ms` 尝试直连；直连 RTT 连续 `settle_ms` 优于中继 → 无缝迁移 Channel（迁移语义见 §4）。
- `direct-only`：禁用中继回退（强隐私场景）。

### 2.3 caps 门控

Plan 可声明全局要求，执行器据此**预筛**模块：

```toml
[requirements]
min_mtu = 1200          # rd 经验：tailscale MTU 1280 → QUIC 钳 1200
datagram = true         # media.profile = fec-shards 时执行器自动附加
```

要求与模块 offering 无交集的路径在**加载期**即被剔除并在 Studio 标红。

## 3. 幂等与复现

- Plan 文件 MUST 自包含：模块 id 全部可在注册表解析；不允许引用本机隐式状态（身份/信任除外，且路径参数化）。
- 同一 Plan + 同一注册表版本 + 同一网络剖面 → SB-Bench 结果可复现（04 篇）。

## 4. Channel 迁移语义（升级直连时）

1. 新 PathHandle → 原 Transport `establish` 出新 Channel（同密钥会话续用）；
2. 应用面无感：S5 只看到 `Channel::replace(new)`；在途数据报不保证迁移（语义 = 丢帧），流 MUST 保证已提交字节不丢（可靠流语义跨迁移保持）；
3. 旧 Channel 在 `grace_ms`（默认 2s）后关闭。

## 5. 预算（评审修订 #5：切分规则）

`path.budget_ms` 是**路径总预算**，执行器按剩余阶段均分剩余时间；阶段可用 `[path.overrides]` 单独覆盖。未声明 budget_ms 时用下表阶段默认：

| 阶段 | 默认 | 出处 |
|---|---|---|
| discovery | 2000 ms | LAN 广播 3 轮 |
| punch | 3000 ms | ICE 常规 |
| relay.alloc | 5000 ms | DERP 冷启动实测 |
| transport.establish | 5000 ms；中继路径 15000 ms | rd 教训：DERP 链路 RTT 1.2s+ 时 5s 必超时 |
| 整路径 | 15000 ms | — |

约束：`budget_ms` MUST ≥ 该路径所含阶段默认之和；否则 Plan 加载期报错（防示例自证非法）。
