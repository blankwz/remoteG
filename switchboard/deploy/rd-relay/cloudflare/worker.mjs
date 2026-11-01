// rd 原生中继 · Cloudflare Workers 版（T-033）
//
// 协议（与 Node 版/UDP 版语义一致，传输换成 WebSocket）：
//   被控端:  WSS GET /agent?th=<tag8hex>          → 首帧 text {"t":"register","th":"..."}
//   控制端:  WSS GET /c/<tag8hex>                 → 直接开始二进制帧
//   二进制帧 = [8B tag LE][QUIC 原始包]           —— 中继只搬运，永不解密
//   /healthz → {"ok":true,"edition":"cloudflare"}
//
// 每个 tag 一个 Durable Object（SQLite 存储，Free 计划可用）；
// WebSocket Hibernation 让空闲会话近乎零开销。

export default {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);
    if (url.pathname === "/healthz") {
      return json({ ok: true, edition: "cloudflare" });
    }
    if (url.pathname === "/agent") {
      const th = url.searchParams.get("th");
      if (!th || !/^[0-9a-f]{16}$/i.test(th)) {
        return json({ error: "th 必须是 16 位 hex（指纹前 8 字节）" }, 400);
      }
      return forwardToDO(request, env, th, "agent");
    }
    const cMatch = url.pathname.match(/^\/c\/([0-9a-f]{16})$/i);
    if (cMatch) {
      return forwardToDO(request, env, cMatch[1].toLowerCase(), "client");
    }
    return json({ error: "未知路径（/healthz /agent?th= /c/<tag>）" }, 404);
  },
};

function json(obj, status = 200) {
  return new Response(JSON.stringify(obj), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function forwardToDO(request, env, tag, role) {
  const id = env.RELAY.idFromName(tag);
  const stub = env.RELAY.get(id);
  return stub.fetch(new Request(`https://do.internal/${role}?tag=${tag}`, request));
}

// ─────────────────────────────────────────────────────────────
// 每个 tag 一个 DO：桥接 agent WS ↔ client WS
// ─────────────────────────────────────────────────────────────

export class RelayDO {
  constructor(state, env) {
    this.state = state;
    this.agent = null; // WebSocket
    this.client = null;
  }

  async fetch(request) {
    const url = new URL(request.url);
    const role = url.pathname === "/agent" ? "agent" : "client";
    const tag = url.searchParams.get("tag") ?? "";
    if (request.headers.get("upgrade") !== "websocket") {
      return json({ error: "需要 WebSocket 升级" }, 426);
    }
    const pair = new WebSocketPair();
    this.state.acceptWebSocket(pair[0], [role]);
    const ws = pair[0];

    if (role === "agent") {
      if (this.agent) {
        // 同 tag 重复注册：踢掉旧的（与 UDP 版「最新地址为准」语义一致）
        try { this.agent.close(4000, "被新注册替换"); } catch { /* 已关 */ }
        this.agent = null;
      }
      this.agent = ws;
      // 首帧 register 确认
      ws.send(JSON.stringify({ t: "registered", th: tag }));
    } else {
      if (this.client) {
        try { this.client.close(4000, "被新连接替换"); } catch { /* 已关 */ }
      }
      this.client = ws;
      // agent 不在线时客户端先等：注册帧不必发（二进制直通由下面桥接处理）
    }

    ws.addEventListener("message", (event) => this.onMessage(ws, event));
    ws.addEventListener("close", () => this.onClose(ws));
    ws.addEventListener("error", () => this.onClose(ws));

    return new Response(null, { status: 101, webSocket: pair[1] });
  }

  onMessage(ws, event) {
    // 只做二进制直通（密文转发）；文本仅 register/registered/ping 控制帧
    if (typeof event.data === "string") {
      // agent 的 register 首帧已在 fetch 里确认；其余文本帧忽略
      if (ws === this.agent && !this.registered) {
        this.registered = true;
      }
      return;
    }
    const peer = ws === this.agent ? this.client : this.agent;
    if (peer && peer.readyState === 1) {
      peer.send(event.data);
    }
    // 对端不在线：静默丢弃（QUIC 会重传；客户端 DIAL 侧有 15s 超时）
  }

  onClose(ws) {
    if (ws === this.agent) this.agent = null;
    if (ws === this.client) this.client = null;
    // 对端掉线后本侧 QUIC 的 idle timeout（20s）自行收尾，无需主动关闭
  }
}
