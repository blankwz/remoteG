// rd 原生中继 · Node 版（零依赖；Docker/反代场景，T-033）
//
// 用法：node relay.mjs [port]        （默认 8787；前置 Caddy/nginx 做 HTTPS）
// 协议与 cloudflare/worker.mjs 完全一致：
//   GET /healthz                → {"ok":true,"edition":"node"}
//   GET /agent?th=<tag8hex>     → WS 升级，首帧 text register 后回 {"t":"registered"}
//   GET /c/<tag8hex>            → WS 升级，二进制直通
//   二进制帧 = [8B tag LE][QUIC 原始包]
import crypto from "node:crypto";
import http from "node:http";

const PORT = Number(process.argv[2] ?? process.env.PORT ?? 8787);
const MAGIC_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/** tag → { agent, client } 会话表（内存态；进程重启即清，客户端会自动重连） */
const sessions = new Map();

// ── 极简 RFC6455 服务端实现（足够本协议：小帧、文本+二进制、ping/pong/close）──

function acceptKey(key) {
  return crypto.createHash("sha1").update(key + MAGIC_GUID).digest("base64");
}

/** 解析一个或多个完整帧；返回 {frames, rest}。分片(continuation)按累计处理。 */
function decodeFrames(buf, state) {
  const frames = [];
  let off = 0;
  while (buf.length - off >= 2) {
    const b0 = buf[off];
    const b1 = buf[off + 1];
    const fin = (b0 & 0x80) !== 0;
    const opcode = b0 & 0x0f;
    const masked = (b1 & 0x80) !== 0;
    let len = b1 & 0x7f;
    let pos = off + 2;
    if (len === 126) {
      if (buf.length - pos < 2) break;
      len = buf.readUInt16BE(pos); pos += 2;
    } else if (len === 127) {
      if (buf.length - pos < 8) break;
      const big = buf.readBigUInt64BE(pos); pos += 8;
      len = Number(big);
    }
    const maskLen = masked ? 4 : 0;
    if (buf.length - pos < maskLen + len) break;
    let payload = buf.subarray(pos + maskLen, pos + maskLen + len);
    if (masked) {
      const mask = buf.subarray(pos, pos + 4);
      payload = Buffer.from(payload);
      for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i & 3];
    }
    off = pos + maskLen + len;
    // 分片：0=continuation 1=text 2=binary 8=close 9=ping 10=pong
    if (opcode === 0 && state.frag) {
      state.frag.data = Buffer.concat([state.frag.data, payload]);
      if (fin) { frames.push({ opcode: state.frag.opcode, data: state.frag.data }); state.frag = null; }
    } else if (!fin && (opcode === 1 || opcode === 2)) {
      state.frag = { opcode, data: Buffer.from(payload) };
    } else {
      frames.push({ opcode, data: payload });
    }
  }
  return { frames, rest: buf.subarray(off) };
}

function encodeFrame(opcode, data) {
  const len = data.length;
  let head;
  if (len < 126) {
    head = Buffer.from([0x80 | opcode, len]);
  } else if (len < 65536) {
    head = Buffer.alloc(4); head[0] = 0x80 | opcode; head.writeUInt16BE(len, 2);
  } else {
    head = Buffer.alloc(10); head[0] = 0x80 | opcode; head.writeBigUInt64BE(BigInt(len), 2);
  }
  return Buffer.concat([head, data]);
}

function wsSend(ws, opcode, data) {
  if (ws.destroyed || ws.readyState !== "open") return;
  ws.write(encodeFrame(opcode, data));
}
const wsText = (ws, str) => wsSend(ws, 0x1, Buffer.from(str, "utf8"));
const wsBinary = (ws, buf) => wsSend(ws, 0x2, buf);
const wsClose = (ws, code = 1000) => {
  if (ws.destroyed || ws.readyState !== "open") return;
  const body = Buffer.alloc(2); body.writeUInt16BE(code);
  ws.write(encodeFrame(0x8, body));
  ws.end();
};

// ── 会话桥接 ────────────────────────────────────────────────

function getSession(tag) {
  let s = sessions.get(tag);
  if (!s) { s = { agent: null, client: null }; sessions.set(tag, s); }
  return s;
}

function bridge(from, to, label) {
  from.on("message", ({ opcode, data }) => {
    if (opcode !== 0x1 && opcode !== 0x2) return;
    const peer = sessions.get(from.tag)?.[to];
    if (peer) wsBinary(peer, data); // 原样转发（含 tag 头，密文完整性）
  });
  from.on("close", () => {
    const s = sessions.get(from.tag);
    if (s && s[from.label] === from) {
      // 对端掉线后本侧 QUIC idle timeout（20s）自行收尾
    }
  });
}

const server = http.createServer((req, res) => {
  const url = new URL(req.url, "http://x");
  if (url.pathname === "/healthz") {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ ok: true, edition: "node", agents: sessions.size }));
    return;
  }
  res.writeHead(404, { "content-type": "application/json" });
  res.end(JSON.stringify({ error: "未知路径（/healthz /agent?th= /c/<tag>）" }));
});

server.on("upgrade", (req, socket) => {
  const url = new URL(req.url, "http://x");
  const key = req.headers["sec-websocket-key"];
  if (!key) { socket.destroy(); return; }

  let tag = null, role = null;
  if (url.pathname === "/agent") {
    const th = url.searchParams.get("th") ?? "";
    if (!/^[0-9a-f]{16}$/i.test(th)) {
      socket.write("HTTP/1.1 400 Bad Request\r\n\r\n"); socket.destroy(); return;
    }
    tag = th.toLowerCase(); role = "agent";
  } else {
    const m = url.pathname.match(/^\/c\/([0-9a-f]{16})$/i);
    if (!m) { socket.write("HTTP/1.1 404 Not Found\r\n\r\n"); socket.destroy(); return; }
    tag = m[1].toLowerCase(); role = "client";
  }

  socket.write(
    "HTTP/1.1 101 Switching Protocols\r\n" +
    "Upgrade: websocket\r\nConnection: Upgrade\r\n" +
    `Sec-WebSocket-Accept: ${acceptKey(key)}\r\n\r\n`,
  );

  const ws = socket; // 直接读写原始 socket
  const state = { frag: null };
  ws.tag = tag; ws.label = role;
  ws.readyState = "open";

  const s = getSession(tag);
  if (role === "agent") {
    if (s.agent) wsClose(s.agent, 4000); // 同 tag 重复注册：踢旧（最新地址为准）
    s.agent = ws;
    wsText(ws, JSON.stringify({ t: "registered", th: tag }));
  } else {
    if (s.client) wsClose(s.client, 4000);
    s.client = ws;
  }

  let buf = Buffer.alloc(0);
  socket.on("data", (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    const { frames, rest } = decodeFrames(buf, state);
    buf = rest;
    for (const f of frames) {
      if (f.opcode === 0x8) { wsClose(socket, 1000); return; }
      if (f.opcode === 0x9) { wsSend(socket, 0xA, f.data); continue; } // ping → pong
      if (f.opcode === 0xA) continue; // pong
      const peer = role === "agent" ? s.client : s.agent;
      if (peer && peer.readyState === "open" && (f.opcode === 0x2 || f.opcode === 0x1)) {
        wsBinary(peer, Buffer.from(f.data)); // 二进制/文本原样转发
      }
      // 对端不在线：静默丢（QUIC 重传 + 客户端 DIAL 15s 超时兜底）
    }
  });
  socket.on("close", () => {
    ws.readyState = "closed";
    const cur = sessions.get(tag);
    if (cur && cur[role] === ws) cur[role] = null;
  });
  socket.on("error", () => { /* 网络错：close 事件统一清理 */ });
});

server.listen(PORT, "0.0.0.0", () => {
  console.log(`rd-relay (node) 监听 0.0.0.0:${PORT} —— /healthz /agent?th= /c/<tag>`);
});
