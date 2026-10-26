// 方案测试页——极简版：大卡片 + 大按钮 + 大分数，零学习成本
import { useCallback, useEffect, useRef, useState } from "react";
import { PRESETS, type Preset } from "./presets";

interface Score { total: number; smoothness: number; delivery: number; latency: number; robustness: number; avg_fps: number; frames_sent: number; frames_recv: number }
interface Result { presetName: string; score: Score }

const LETTER = /方案 ([A-D])/;

function sc(v: number) { return v >= 80 ? "#4ec994" : v >= 50 ? "#d4a72c" : "#f47067"; }

function ScoreHero({ r }: { r: Result }) {
  const c = sc(r.score.total);
  const circ = 2 * Math.PI * 36;
  return (
    <div style={{ display: "flex", alignItems: "center", gap: 32, padding: "20px 0" }}>
      <div style={{ position: "relative", width: 88, height: 88 }}>
        <svg viewBox="0 0 80 80" style={{ width: "100%", height: "100%", transform: "rotate(-90deg)" }}>
          <circle cx="40" cy="40" r="36" fill="none" stroke="var(--bg-input)" strokeWidth="5" />
          <circle cx="40" cy="40" r="36" fill="none" stroke={c} strokeWidth="5" strokeLinecap="round"
            strokeDasharray={circ} strokeDashoffset={circ * (1 - r.score.total / 100)}
            style={{ transition: "stroke-dashoffset .8s cubic-bezier(.4,0,.2,1)" }} />
        </svg>
        <div style={{ position: "absolute", inset: 0, display: "flex", alignItems: "center", justifyContent: "center" }}>
          <span style={{ fontSize: 28, fontWeight: 700, color: c }}>{r.score.total.toFixed(0)}</span>
        </div>
      </div>
      <div>
        <div style={{ fontSize: 16, fontWeight: 600 }}>{r.presetName.replace(/^方案 [A-D] · /, "")}</div>
        <div style={{ fontSize: 12, color: "var(--text-secondary)", marginTop: 4 }}>
          {r.score.avg_fps.toFixed(0)} 帧/秒 · 送达 {r.score.frames_recv}/{r.score.frames_sent}
        </div>
      </div>
    </div>
  );
}

export default function RuntimeView() {
  const [selected, setSelected] = useState<Preset>(PRESETS[0]);
  const [running, setRunning] = useState(false);
  const [runName, setRunName] = useState("");
  const [elapsed, setElapsed] = useState(0);
  const [fpsData, setFpsData] = useState<number[]>([]);
  const [mbpsData, setMbpsData] = useState<number[]>([]);
  const [events, setEvents] = useState<string[]>([]);
  const [results, setResults] = useState<Result[]>([]);
  const [error, setError] = useState("");
  const lock = useRef(false);

  // 测试单个方案
  const test = useCallback(async (p: Preset) => {
    if (lock.current) return;
    lock.current = true;
    setError(""); setFpsData([]); setMbpsData([]); setEvents([]);
    setSelected(p); setRunning(true); setRunName(p.name); setElapsed(0);

    try {
      const r = await fetch("/api/session", {
        method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify(p.params),
      });
      if (!r.ok) throw new Error(`服务器返回 ${r.status}`);
      const { id } = await r.json();

      // 一直听到 ended 才算跑完；分数随 ended 帧一起返回
      const score: Score | null = await new Promise((resolve, reject) => {
        const ws = new WebSocket(`ws://${location.host}/api/live/${id}`);
        let done = false;
        const timer = setTimeout(() => {
          if (!done) { ws.close(); reject(new Error("超时：30 秒内没有跑完")); }
        }, 30000);
        ws.onmessage = (ev) => {
          if (ev.data === "__end__") { ws.close(); return; }
          try {
            const m = JSON.parse(ev.data);
            if (m.type === "stats") {
              setFpsData(d => [...d, m.fps].slice(-100));
              setMbpsData(d => [...d, m.send_mbps].slice(-100));
            }
            if (m.type === "event" && m.event === "fec_recovered")
              setEvents(e => ["🔧 网络丢了几包，已自动修复", ...e].slice(0, 8));
            if (m.type === "event" && (m.event === "frame_lost" || m.event === "frame_drop"))
              setEvents(e => ["💔 有一帧没送到（已跳过，不影响后面的画面）", ...e].slice(0, 8));
            if (m.type === "ended") {
              done = true; clearTimeout(timer); ws.close();
              resolve(m.score ?? null);
            }
          } catch {}
        };
        ws.onerror = () => {
          if (!done) { clearTimeout(timer); reject(new Error("连接断开")); }
        };
      });

      if (score) setResults(rs => [...rs, { presetName: p.name, score }]);
      else setError("这次没有出分（会话被提前停掉了），再试一次吧");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
    lock.current = false;
    setRunning(false);
  }, []);

  // 依次测全部
  const testAll = useCallback(async () => {
    for (const p of PRESETS) {
      if (lock.current) return;
      setSelected(p);
      await test(p);
    }
  }, [test]);

  // 计时
  useEffect(() => {
    if (!running) return;
    const iv = setInterval(() => setElapsed(e => e + 1), 1000);
    return () => clearInterval(iv);
  }, [running]);

  // 最近一次的结果
  const lastResult = results.length > 0 ? results[results.length - 1] : null;

  return (
    <div style={{ padding: "24px 32px", maxWidth: 720, margin: "0 auto" }}>
      {/* ── 标题 ── */}
      <h2 style={{ fontSize: 20, fontWeight: 700, margin: "0 0 4px" }}>
        选一个场景，点一下就好
      </h2>
      <p style={{ color: "var(--text-secondary)", fontSize: 13, margin: "0 0 20px" }}>
        每张卡片是一种使用场景。点「测试」跑 10 秒看分数，分数越高越适合你。
      </p>

      {/* ── 方案卡片 ── */}
      <div style={{ display: "flex", flexDirection: "column", gap: 10, marginBottom: 20 }}>
        {PRESETS.map((p) => {
          const r = results.find(x => x.presetName === p.name);
          const isSel = selected.id === p.id;
          return (
            <div
              key={p.id}
              onClick={() => !running && setSelected(p)}
              style={{
                display: "flex", alignItems: "center", gap: 14,
                padding: "14px 18px",
                background: isSel ? "var(--bg-raised)" : "var(--bg-surface)",
                border: `1.5px solid ${isSel ? "var(--success)" : "var(--border)"}`,
                borderRadius: 12, cursor: running ? "wait" : "pointer",
                transition: "border-color .15s",
              }}
            >
              {/* 大字母 */}
              <div style={{
                width: 42, height: 42, borderRadius: 10, flexShrink: 0,
                display: "flex", alignItems: "center", justifyContent: "center",
                fontSize: 20, fontWeight: 700,
                background: isSel ? "var(--success-bg)" : "var(--bg-elevated)",
                color: isSel ? "var(--success)" : "var(--text-secondary)",
              }}>
                {p.name.match(LETTER)?.[1] ?? "?"}
              </div>
              <div style={{ flex: 1 }}>
                <div style={{ fontWeight: 600, fontSize: 14 }}>{p.name.replace(/^方案 [A-D] · /, "")}</div>
                <div style={{ fontSize: 12, color: "var(--text-secondary)", marginTop: 2 }}>{p.scene}</div>
              </div>
              {/* 已有分数 */}
              {r && (
                <div style={{ textAlign: "right" }}>
                  <div style={{ fontSize: 22, fontWeight: 700, color: sc(r.score.total) }}>
                    {r.score.total.toFixed(0)}
                  </div>
                  <div style={{ fontSize: 10, color: "var(--text-muted)" }}>分</div>
                </div>
              )}
            </div>
          );
        })}
      </div>

      {/* ── 测试按钮 ── */}
      <div style={{ display: "flex", gap: 10, marginBottom: 16 }}>
        <button
          onClick={() => void test(selected)}
          disabled={running}
          style={{
            flex: 1, padding: "12px 0",
            background: running ? "var(--bg-elevated)" : "#238636",
            color: running ? "var(--text-secondary)" : "#fff",
            border: "none", borderRadius: 10,
            fontSize: 15, fontWeight: 600, cursor: running ? "wait" : "pointer",
          }}
        >
          {running ? `⏳ 正在测试「${runName}」… ${elapsed}s` : `▶ 测试「${selected.name.replace(/^方案 [A-D] · /, "")}」`}
        </button>
        {PRESETS.length > 1 && !running && (
          <button
            onClick={() => void testAll()}
            style={{
              padding: "12px 20px",
              background: "var(--bg-elevated)", color: "var(--text-primary)",
              border: "1px solid var(--border-strong)", borderRadius: 10,
              fontSize: 14, cursor: "pointer",
            }}
          >
            全部测
          </button>
        )}
      </div>

      {/* ── 出分卡 ── */}
      {lastResult && !running && (
        <ScoreHero r={lastResult} />
      )}

      {/* ── 实时曲线（测试中） ── */}
      {running && fpsData.length > 2 && (
        <div className="chart-grid">
          <div className="chart-panel">
            <div className="chart-title"><b>画面流畅度</b><span><span className="chart-current">{fpsData[fpsData.length - 1]?.toFixed(0)}</span><span className="chart-unit"> 帧/秒</span></span></div>
            <Sparkline data={fpsData} color="#4ec994" />
          </div>
          <div className="chart-panel">
            <div className="chart-title"><b>占用网速</b><span><span className="chart-current">{(mbpsData[mbpsData.length - 1] ?? 0).toFixed(1)}</span><span className="chart-unit"> Mbps</span></span></div>
            <Sparkline data={mbpsData} color="#58a6ff" />
          </div>
        </div>
      )}

      {/* ── 事件流 ── */}
      {events.length > 0 && (
        <div className="event-feed">
          <b>刚才发生了什么</b>
          <div style={{ display: "flex", flexDirection: "column", gap: 4, marginTop: 8 }}>
            {events.map((e, i) => (
              <div key={i} style={{ fontSize: 12, color: "var(--text-secondary)" }}><span>{e}</span></div>
            ))}
          </div>
        </div>
      )}

      {/* ── 错误 ── */}
      {error && (
        <div className="err-banner">{error}</div>
      )}

      {/* ── 部署指引（默认收起） ── */}
      <details style={{ marginTop: 24, fontSize: 12, color: "var(--text-secondary)" }}>
        <summary style={{ cursor: "pointer", color: "var(--accent)" }}>跨网使用需要额外部署？看这里 →</summary>
        <div style={{ marginTop: 8, lineHeight: 1.8 }}>
          <p>如果你和对方在<b>不同城市/网络</b>，需要一个中转服务器来保证连接稳定。</p>
          <p>最简单的方案：租一台<b>香港 VPS</b>（约 ¥30/月），按 <code>switchboard/deploy/derper/README.md</code> 的步骤操作，30 分钟搞定。</p>
          <p>部署完成后，跨网延迟从 1.2 秒降到 0.1 秒。</p>
        </div>
      </details>
    </div>
  );
}

// ── 迷你折线图 ──
function Sparkline({ data, color }: { data: number[]; color: string }) {
  const w = 280, h = 48;
  if (data.length < 2) return <svg width="100%" height={h} />;
  const max = Math.max(...data, 1);
  const min = Math.min(...data, 0);
  const range = max - min || 1;
  const pts = data.map((v, i) =>
    `${(i / (data.length - 1)) * w},${h - 4 - ((v - min) / range) * (h - 8)}`
  ).join(" ");
  return (
    <svg width="100%" height={h} viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none">
      <polyline points={pts} fill="none" stroke={color} strokeWidth="1.5" />
    </svg>
  );
}
