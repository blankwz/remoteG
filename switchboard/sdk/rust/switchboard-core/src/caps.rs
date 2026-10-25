//! caps 能力系统（规范 01 §1）：提供方 offering 与消费方 requirement 的交集协商。

use serde::{Deserialize, Serialize};

/// 不可靠数据报能力。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatagramCaps {
    /// 单个数据报最大字节数（载荷，不含模块内部封装头）。
    pub max_size: u16,
    /// true = 发送方可接受队列超限丢弃（fec-shards profile 必需）。
    pub lossy_ok: bool,
}

/// 可靠有序流能力。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamCaps {
    pub reliable_ordered: bool,
    pub bidi: bool,
    pub max_concurrent: u32,
}

/// 拥塞控制算法（quinn/Cubic/BBR 对齐）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CongestionKind {
    Cubic,
    Bbr,
}

/// Channel 能力（Transport 的 offering = Channel 的 caps；
/// requirement 语义：字段为 `None` 表示不限制，`Some` 表示必须满足且不劣于）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCaps {
    pub datagram: Option<DatagramCaps>,
    pub streams: Option<StreamCaps>,
    #[serde(default)]
    pub congestion: Vec<CongestionKind>,
    /// 实测路径 MTU。requirement 语义：`0` 表示不限制。
    #[serde(default)]
    pub mtu: u16,
    #[serde(default)]
    pub multipath: bool,
}

impl ChannelCaps {
    /// 协商（01 §1.1）：offering `self` 是否满足 requirement `req`。
    ///
    /// - `req.datagram = Some(d)`：offering 必须有 datagram 且 `max_size >= d.max_size`、
    ///   `lossy_ok >= d.lossy_ok`；
    /// - `req.streams = Some(s)`：offering 必须有流且能力不劣；
    /// - `req.congestion`：offering 必须包含 req 的全部算法；
    /// - `req.mtu > 0`：offering `mtu >= req.mtu`；
    /// - `req.multipath = true`：offering 必须为 true。
    pub fn satisfies(&self, req: &ChannelCaps) -> bool {
        if let Some(rd) = &req.datagram {
            match &self.datagram {
                None => return false,
                Some(od) => {
                    if od.max_size < rd.max_size || od.lossy_ok < rd.lossy_ok {
                        return false;
                    }
                }
            }
        }
        if let Some(rs) = &req.streams {
            match &self.streams {
                None => return false,
                Some(os) => {
                    if os.reliable_ordered < rs.reliable_ordered
                        || os.bidi < rs.bidi
                        || os.max_concurrent < rs.max_concurrent
                    {
                        return false;
                    }
                }
            }
        }
        if !req.congestion.iter().all(|c| self.congestion.contains(c)) {
            return false;
        }
        if req.mtu > 0 && self.mtu < req.mtu {
            return false;
        }
        if req.multipath && !self.multipath {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quic_like() -> ChannelCaps {
        ChannelCaps {
            datagram: Some(DatagramCaps { max_size: 1200, lossy_ok: true }),
            streams: Some(StreamCaps { reliable_ordered: true, bidi: true, max_concurrent: 100 }),
            congestion: vec![CongestionKind::Cubic],
            mtu: 1200,
            multipath: false,
        }
    }

    #[test]
    fn 可靠流媒体档_要求_datagram_满足() {
        // fec-shards profile：需要 datagram（lossy_ok）+ mtu≥1200
        let req = ChannelCaps {
            datagram: Some(DatagramCaps { max_size: 1024, lossy_ok: true }),
            mtu: 1200,
            ..Default::default()
        };
        assert!(quic_like().satisfies(&req));
    }

    #[test]
    fn 要求_bb_而只有_cubic_失败() {
        let req = ChannelCaps { congestion: vec![CongestionKind::Bbr], ..Default::default() };
        assert!(!quic_like().satisfies(&req));
    }

    #[test]
    fn mtu_不足_失败() {
        let req = ChannelCaps { mtu: 1400, ..Default::default() };
        assert!(!quic_like().satisfies(&req));
    }

    #[test]
    fn 无约束_总是满足() {
        assert!(quic_like().satisfies(&ChannelCaps::default()));
    }
}
