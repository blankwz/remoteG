//! 参考模块集（规范 05 §4 / 06 §2）。
//!
//! | 模块 id | 阶段 | 合规 | 说明 |
//! |---|---|---|---|
//! | `sb.core.loopback` | transport | L-B | 内存双端管道（T0 基准面） |
//! | `sb.core.lossy-sim` | transport | L-B | 弱网仿真装饰器（04 §1 T0/T3） |
//! | `sb.core.fec-xor` | media | L-B | fec-shards profile 最小实现（Moonlight 简化版） |
//! | `sb.lan.udp-broadcast` | discovery | L-B | LAN 广播发现（rd 移植） |
//! | `sb.rustdesk.punch-v2` | punch | L-A | 研究桩（铁律来源） |
//! | `sb.derp.self-hk` | relay | L-A | 研究桩（process 绑定路线） |

pub mod fec;
pub mod lanbcast;
pub mod loopback;
pub mod lossy;
pub mod punch_stub;
