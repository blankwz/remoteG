//! rd-proto — 协议消息（prost 生成，源 proto 见仓库根 `proto/rd.proto`）。

pub mod v0 {
    include!(concat!(env!("OUT_DIR"), "/rd.v0.rs"));
}
