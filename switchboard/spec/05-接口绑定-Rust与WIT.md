# 05 接口绑定：Rust（规范性）与 WIT（跨语言）

- 状态：规范性（Rust trait 为准）+ 信息性（WIT 镜像）
- 版本：0.1

## 1. 双面绑定策略

| 绑定 | 地位 | 适用 |
|---|---|---|
| `native-rust` | **规范性**：trait 定义即规范 | 进程内模块（quinn/KCP/合成模块） |
| `wasm` | 草案：WIT 镜像，异步等 WASI 0.3 | 跨语言插件（Go/JS/Python 编写 discovery/策略类模块） |
| `process` | 规范性：由 Plan 声明，SDK 内置适配器 | 巨型既有栈（Tailscale/tailscaled、SRT、libwebrtc）——**整栈即模块** |

> 论证（P5）：Tailscale 的 DISCO+DERP、Valve SDR 这类栈不可能移植进 SDK；把它们当外部进程托管（健康检查 + 本地 socket 暴露 + 遥测采集脚本）是唯一现实路径。rd 对 Tailscale 的现有用法即此模式的先例。

## 2. process 绑定契约

模块 manifest 声明：

```toml
[binding.process]
run = "tailscaled"                     # 或容器/服务名
health = "http://127.0.0.1:9/metrics"  # 健康与指标端点
handoff = "sock"                       # socket 交接方式：sock | fd | vsock
telemetry_bridge = "deploy/tailscale-bridge.py"   # 已交付：deploy/ 目录   # 把栈私有指标映射为 StageStats
```

MUST：`telemetry_bridge` 产出符合 03 篇的 StageStats，`path.kind` 如实——否则 L-C 否决（与原生模块同一杆秤）。

## 3. WIT 镜像（信息性，0.1 草案）

```wit
package switchboard:core@0.1.0;

interface channel {
  record datagram-caps { max-size: u16, lossy-ok: bool }
  record stream-caps { reliable-ordered: bool, bidi: bool, max-concurrent: u32 }
  variant payload { dgram(list<u8>), frame(list<u8>) }
  resource channel {
    caps: func() -> channel-caps;
    send-dgram: func(b: list<u8>) -> result<_, send-error>;
    open-stream: func(kind: stream-kind) -> result<stream, stream-error>;
    stats: func() -> stage-stats;
  }
  resource stream { write: func(b: list<u8>) -> result<u64, stream-error>; finish: func(); }
}

interface discovery {
  record endpoint-candidate { addr: string, proto: proto, source: source-kind, cost: u8 }
  poll-candidates: func() -> list<endpoint-candidate>;
}

world module {
  export discovery;
  // punch/relay/transport/media 同构；异步轮询风格（WASI 0.3 async 落地后改 async）
}
```

规则：WIT 与 Rust trait 由 CI 生成对照表强制同步；语义以 Rust 为准。

## 4. SDK 布局

```
sdk/rust/
  switchboard-core/       # 类型 + caps 协商 + Plan 解析 + TelemetryBus + 执行器
  switchboard-bench/      # 04 篇实现：负载发生器 + 指标聚合 + 评分 + bench.json
  switchboard-mods/       # 参考模块：loopback / lossy-sim / quinn适配(rd) / lan-broadcast(rd) / rustdesk-punch(研究级) / moonlight-fec(研究级)
```

参考模块中标注「研究级」者实现完整 trait 与遥测，但未过 L-C 认证——它们的当前价值是把各家原理带进规范，避免规范闭门造车。
