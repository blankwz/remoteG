//! `easytier-cli --output json route` 输出解析。
//!
//! 上游 route JSON（easytier/src/easytier-cli.rs `handle_route_list`）：
//! `{ "node_info": {...}, "peer_routes": [ PeerRoutePair... ] }`，其中
//! `PeerRoutePair = { route: Route, peer: PeerInfo }`。proto 结构经 pbjson
//! 序列化为 **camelCase**（`ipv4Addr`）；本解析对 camelCase / snake_case
//! 双兼容（键名归一化），地址值接受 `"ip/prefix"` 字符串或 pbjson 对象
//! `{ "address": "ip", "prefixLength": n }` 两种形态。fixture 单测锁定。

use anyhow::Context;
use serde_json::Value;

/// 一条 mesh 路由（对端虚拟地址）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteEntry {
    /// 对端 IPv4 虚拟地址（不含前缀，如 `10.126.126.3`）。
    pub ipv4: Option<String>,
    /// 对端 IPv6 虚拟地址。
    pub ipv6: Option<String>,
    pub hostname: Option<String>,
}

/// camelCase / snake_case 键名归一化：`ipv4Addr` → `ipv4_addr`。
fn normalize_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 4);
    for (i, ch) in key.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// 从「字符串 `"ip/prefix"` 或对象 `{"address": ip, ...}`」提取纯 IP。
fn extract_addr(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.split('/').next().unwrap_or(s).to_string()),
        Value::Object(o) => o
            .get("address")
            .and_then(Value::as_str)
            .map(|s| s.split('/').next().unwrap_or(s).to_string()),
        _ => None,
    }
}

/// 从对象按归一化键取值（先精确匹配，再归一化匹配）。
fn get_normalized<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
    let obj = obj.as_object()?;
    if let Some(v) = obj.get(key) {
        return Some(v);
    }
    let want = normalize_key(key);
    obj.iter()
        .find(|(k, _)| normalize_key(k) == want)
        .map(|(_, v)| v)
}

/// 解析 route JSON（兼容整包 `RouteListData` 与裸 `peer_routes` 两种壳）。
pub fn parse_route_json(raw: &str) -> anyhow::Result<Vec<RouteEntry>> {
    let v: Value = serde_json::from_str(raw)
        .with_context(|| format!("easytier-cli 输出非 JSON: {:.120}", raw))?;

    // 壳：RouteListData { node_info, peer_routes }（单实例时 cli 直接打印它）
    let pairs = get_normalized(&v, "peer_routes")
        .ok_or_else(|| anyhow::anyhow!("route JSON 缺少 peer_routes: {:.120}", raw))?;
    let arr = pairs
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("peer_routes 非数组"))?;

    let mut out = Vec::with_capacity(arr.len());
    for pair in arr {
        // PeerRoutePair { route: Route, peer: PeerInfo }
        let Some(route) = get_normalized(pair, "route") else {
            continue;
        };
        let ipv4 = get_normalized(route, "ipv4_addr").and_then(extract_addr);
        let ipv6 = get_normalized(route, "ipv6_addr").and_then(extract_addr);
        let hostname = get_normalized(route, "hostname")
            .and_then(Value::as_str)
            .map(str::to_string);
        if ipv4.is_some() || ipv6.is_some() {
            out.push(RouteEntry {
                ipv4,
                ipv6,
                hostname,
            });
        }
    }
    Ok(out)
}

/// 解析本节点自身的虚拟 IPv4（route JSON 的 `node_info.ipv4_addr`）。
/// DHCP 分配的自查来源；无分配/非 JSON 返回 None（best-effort）。
pub fn parse_self_ipv4(raw: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let node = get_normalized(&v, "node_info")?;
    let ip = get_normalized(node, "ipv4_addr")?;
    extract_addr(ip).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 自身虚拟 IP：node_info.ipv4Addr（DHCP 分配结果的自查来源）。
    #[test]
    fn 解析自身虚拟ip_camel_case() {
        let raw = r#"{
          "node_info": { "peerId": 1, "ipv4Addr": "10.126.126.2/24", "hostname": "ctrl" },
          "peer_routes": []
        }"#;
        assert_eq!(parse_self_ipv4(raw).as_deref(), Some("10.126.126.2"));
        assert_eq!(parse_self_ipv4(r#"{"peer_routes": []}"#), None);
        assert_eq!(parse_self_ipv4("not json"), None);
    }

    /// pbjson camelCase 形态（上游实际输出）。
    #[test]
    fn 解析_camel_case_fixture() {
        let raw = r#"{
          "node_info": { "peerId": 1, "ipv4Addr": "10.126.126.2/24", "hostname": "ctrl" },
          "peer_routes": [
            { "route": {
                "peerId": 2,
                "ipv4Addr": "10.126.126.3/24",
                "hostname": "agent-pc",
                "cost": 1
              },
              "peer": {} },
            { "route": { "peerId": 3, "ipv6Addr": { "address": "fd::3", "prefixLength": 64 } } }
          ]
        }"#;
        let entries = parse_route_json(raw).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].ipv4.as_deref(), Some("10.126.126.3"));
        assert_eq!(entries[0].hostname.as_deref(), Some("agent-pc"));
        assert_eq!(entries[1].ipv6.as_deref(), Some("fd::3"));
    }

    /// snake_case 形态（防御上游序列化切换）。
    #[test]
    fn 解析下划线命名_fixture() {
        let raw = r#"{
          "node_info": { "ipv4_addr": "10.126.126.2/24" },
          "peer_routes": [
            { "route": { "peer_id": 2, "ipv4_addr": "10.126.126.3/24" } }
          ]
        }"#;
        let entries = parse_route_json(raw).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].ipv4.as_deref(), Some("10.126.126.3"));
    }

    /// 无虚拟地址的对端（未分配/离线中）不产出条目。
    #[test]
    fn 无地址路由_被过滤() {
        let raw = r#"{ "peer_routes": [ { "route": { "peer_id": 9 } } ] }"#;
        assert!(parse_route_json(raw).unwrap().is_empty());
    }

    /// 非 JSON / 缺 peer_routes：报错不 panic。
    #[test]
    fn 非法输入_报错() {
        assert!(parse_route_json("not json").is_err());
        assert!(parse_route_json(r#"{"foo": 1}"#).is_err());
    }
}
