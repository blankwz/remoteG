#!/usr/bin/env python3
"""tailscale 遥测桥（spec 05 §2 process 绑定）：tailscaled → Switchboard StageStats v1。

以 1Hz 采样 `tailscale status --json` 与 `tailscale netcheck --format json`，
对每个在线 peer 输出一行 StageStats（JSON Lines，stdout），path.kind 如实标注
（06 §1：谎报 = L-C 一票否决，对 process 绑定模块同样适用）。

用法：python tailscale-bridge.py [--peer <主机名或IP过滤>] [--interval 1]
消费方：rd HUD / SB-Bench 的 process 模块采集器。
"""

import argparse
import json
import subprocess
import time
import uuid

CONN = f"ts-{uuid.uuid4().hex[:8]}"


def sh(*args: str) -> dict:
    out = subprocess.run(args, capture_output=True, text=True, timeout=15)
    return json.loads(out.stdout or "{}")


def emit(stats: dict) -> None:
    print(json.dumps(stats, ensure_ascii=False), flush=True)


def peer_stats(peer: dict, netcheck: dict, now_ms: int) -> dict | None:
    """把一个 tailscale peer 映射为 StageStats；离线/无连接返回 None。"""
    online = peer.get("Online")
    relay = peer.get("Relay")
    cur_addr = peer.get("CurAddr") or ""
    if not online and not cur_addr:
        return None
    direct = bool(cur_addr) and not relay
    kind = "direct" if direct else "relay"
    # tailscale status 不直接给 RTT；用 netcheck 的 preferred DERP 延迟或最近 ping 值
    latency = -1.0  # 负哨兵=不可用（spec/03：消费方见到负值跳过，不当 0ms 聚合）
    if direct:
        # 真实 RTT 需 tailscale ping 采样注入；此处留负哨兵避免假 0ms 进 SB-Bench
        pass
    else:
        derp_lat = (netcheck.get("Latency") or {})
        if derp_lat:
            latency = min(float(v) for v in derp_lat.values()) * 1000.0
    return {
        "v": 1,
        "ts_ms": now_ms,
        "stage": "transport",
        "module": "sb.ext.tailscale",
        "conn": CONN,
        "path": {"kind": kind, "local": None, "remote": cur_addr or relay},
        "rtt_ewma_ms": latency,
        "loss_ratio": 0.0,        # tailscaled 不外发丢包；留空并如实为 0（SHOULD 字段）
        "loss_rx": 0.0,
        "loss_tx": 0.0,
        "drop_app": 0.0,
        "send_mbps": 0.0,
        "recv_mbps": 0.0,
        "queue_depth_bytes": 0,
        "jitter_ms": 0.0,
        "extra": {
            "peerHostName": peer.get("HostName"),
            "relay": relay or "",
            "rxBytes": peer.get("RxBytes", 0),
            "txBytes": peer.get("TxBytes", 0),
            "note": "RTT 需 tailscale ping 采样注入（见 README）；loss 字段如实为零",
        },
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--peer", default="", help="只输出匹配主机名/IP 的 peer")
    ap.add_argument("--interval", type=float, default=1.0)
    args = ap.parse_args()

    while True:
        try:
            status = sh("tailscale", "status", "--json")
            netcheck = sh("tailscale", "netcheck", "--format", "json")
            now = int(time.time() * 1000)
            for name, peer in (status.get("Peer") or {}).items():
                if args.peer and args.peer not in (peer.get("HostName") or "", peer.get("DNSName") or "", peer.get("TailscaleIPs", [None] or [""])[0] if peer.get("TailscaleIPs") else ""):
                    continue
                s = peer_stats(peer, netcheck, now)
                if s:
                    emit(s)
        except Exception as e:  # noqa: BLE001 —— 采集器不许崩，错误进事件流
            emit({
                "v": 1, "ts_ms": int(time.time() * 1000), "type": "event",
                "event": "bridge_error", "detail": str(e), "conn": CONN,
            })
        time.sleep(args.interval)


if __name__ == "__main__":
    main()
