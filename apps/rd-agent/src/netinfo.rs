//! 本机网卡信息（T-031 WOL 前置）：枚举物理网卡 MAC。
//!
//! 用途：`HelloAck.mac_addrs` 上报给控制端，供「远程开机（WOL）」构造魔术包。
//! 安全考量：MAC 属于局域网发现同级公开信息（同网段 arp 广播本来可见），
//! 不含可识别个人内容；只取**已启用、非回环、物理地址长度 6** 的适配器，
//! 排除虚拟/隧道接口可经 IfType 过滤（回环 24、软件回环除外）。

/// 物理网卡 MAC 列表（大写冒号格式，如 `"AA:BB:CC:DD:EE:FF"`；多网卡顺序
/// 为系统枚举序）。非 Windows 或枚举失败返回空——控制端据此置灰 WOL 按钮。
#[cfg(windows)]
pub fn mac_addrs() -> Vec<String> {
    use windows::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;

    const AF_UNSPEC: u32 = 0;
    // 结构体按其自身对齐分配（API 要求缓冲区按 IP_ADAPTER_ADDRESSES_LH 对齐；
    // Vec<u8> 对齐 1 强转即 UB，review H2）
    let unit = std::mem::size_of::<IP_ADAPTER_ADDRESSES_LH>();
    // 预估缓冲:典型机器数 KiB;不足时按系统提示的 size 重试
    let mut size: u32 = 16 * 1024;
    for _ in 0..3 {
        let count = (size as usize).div_ceil(unit).max(1);
        let mut buf = vec![IP_ADAPTER_ADDRESSES_LH::default(); count];
        let adapters = buf.as_mut_ptr();
        let ret = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC,
                GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                None,
                Some(adapters),
                &mut size,
            )
        };
        if ret == ERROR_BUFFER_OVERFLOW.0 {
            continue; // buf 按 size 重试
        }
        if ret != 0 {
            tracing::warn!("GetAdaptersAddresses 失败: win32 error {ret}");
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut cur = adapters;
        while !cur.is_null() {
            let a = unsafe { &*cur };
            let up = a.OperStatus == IfOperStatusUp;
            let physical = a.PhysicalAddressLength == 6;
            // 只收真实物理网卡（L3）：6=ethernetCsmacd、71=IEEE80211(wifi)；
            // 回环(24)与 Hyper-V vSwitch 等虚拟交换机排除，免得 WOL 发冗余包
            let physical_kind = matches!(a.IfType, 6 | 71);
            if up && physical && physical_kind && a.IfType != IF_TYPE_SOFTWARE_LOOPBACK {
                let bytes = unsafe { std::slice::from_raw_parts(a.PhysicalAddress.as_ptr(), 6) };
                out.push(
                    bytes
                        .iter()
                        .map(|b| format!("{b:02X}"))
                        .collect::<Vec<_>>()
                        .join(":"),
                );
            }
            cur = a.Next;
        }
        return out;
    }
    tracing::warn!("GetAdaptersAddresses 缓冲重试耗尽");
    Vec::new()
}

#[cfg(not(windows))]
pub fn mac_addrs() -> Vec<String> {
    Vec::new()
}

// ── 本机单播地址（T-035 直连候选前置）──

/// 本机单播 IPv4/IPv6 地址（非回环、非 link-local；适配器过滤与
/// [`mac_addrs`] 同口径：已启用 + 物理网卡）。直连候选的上游输入。
#[cfg(windows)]
pub fn local_ip_addrs() -> Vec<std::net::IpAddr> {
    use std::net::IpAddr;

    use windows::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};

    let unit = std::mem::size_of::<IP_ADAPTER_ADDRESSES_LH>();
    let mut size: u32 = 16 * 1024;
    for _ in 0..3 {
        let count = (size as usize).div_ceil(unit).max(1);
        let mut buf = vec![IP_ADAPTER_ADDRESSES_LH::default(); count];
        let adapters = buf.as_mut_ptr();
        let ret = unsafe {
            GetAdaptersAddresses(
                0, // AF_UNSPEC：v4/v6 一并返回
                GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                None,
                Some(adapters),
                &mut size,
            )
        };
        if ret == ERROR_BUFFER_OVERFLOW.0 {
            continue;
        }
        if ret != 0 {
            tracing::warn!("GetAdaptersAddresses 失败: win32 error {ret}");
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut cur = adapters;
        while !cur.is_null() {
            let a = unsafe { &*cur };
            let up = a.OperStatus == IfOperStatusUp;
            // 与 mac_addrs 同口径：真实物理网卡（6=以太网 / 71=Wi-Fi）
            let physical_kind = matches!(a.IfType, 6 | 71);
            if !(up && physical_kind) {
                cur = a.Next;
                continue;
            }
            let mut ua = a.FirstUnicastAddress;
            while !ua.is_null() {
                let u = unsafe { &*ua };
                let sa = u.Address.lpSockaddr;
                if !sa.is_null() {
                    let sa = unsafe { &*sa };
                    // IPv4 走 sockaddr_in 的 sa_data；IPv6 地址 22 字节放不进
                    // sa_data[14]，必须按 SOCKADDR_IN6 读 sin6_addr（否则越界
                    // panic——安全审计连带修复：panic 会杀掉中继注册任务）
                    let ip = match sa.sa_family {
                        AF_INET => {
                            let o: Option<[u8; 4]> = sa.sa_data[2..6]
                                .iter()
                                .map(|b| *b as u8)
                                .collect::<Vec<_>>()
                                .try_into()
                                .ok();
                            o.map(IpAddr::from)
                        }
                        AF_INET6 => {
                            use windows::Win32::Networking::WinSock::SOCKADDR_IN6;
                            let sa6 =
                                unsafe { &*(sa as *const _ as *const SOCKADDR_IN6) };
                            let bytes: [u8; 16] =
                                unsafe { sa6.sin6_addr.u.Byte };
                            Some(IpAddr::from(bytes))
                        }
                        _ => None,
                    };
                    // 排除回环与 link-local（169.254/16、fe80::/10）——候选必须
                    // 是「对端可能可达」的地址
                    if let Some(ip) = ip {
                        let usable = match &ip {
                            IpAddr::V4(v4) => {
                                !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified()
                            }
                            IpAddr::V6(v6) => {
                                !v6.is_loopback()
                                    && !v6.is_unicast_link_local()
                                    && !v6.is_unspecified()
                            }
                        };
                        if usable && !out.contains(&ip) {
                            out.push(ip);
                        }
                    }
                }
                ua = u.Next;
            }
            cur = a.Next;
        }
        return out;
    }
    tracing::warn!("GetAdaptersAddresses 缓冲重试耗尽");
    Vec::new()
}

#[cfg(not(windows))]
pub fn local_ip_addrs() -> Vec<std::net::IpAddr> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    /// 冒烟：本机枚举应至少返回一个可用单播地址（开发机必有物理网卡）；
    /// 不强断言数量（CI/容器环境可能为空，非空时必须全部可用）。
    #[test]
    fn local_ip_addrs_全为可用单播() {
        for ip in super::local_ip_addrs() {
            let usable = match ip {
                std::net::IpAddr::V4(v4) => {
                    !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified()
                }
                std::net::IpAddr::V6(v6) => {
                    !v6.is_loopback() && !v6.is_unspecified() && v6.segments()[0] & 0xffc0 != 0xfe80
                }
            };
            assert!(usable, "{ip} 不应出现在候选里");
        }
    }
}
