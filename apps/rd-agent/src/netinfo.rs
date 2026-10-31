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
