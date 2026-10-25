// 方案测试页（默认）：三步引导 → 选方案 → 一键测试 → 自动出分排名
import { useCallback, useEffect, useRef, useState } from 'react';
import { PRESETS, type Preset } from './presets';

// ── 类型 ──
interface Stat { ts_ms: number; fps: number; recv_mbps: number; rtt_ms: number; loss_pct: number; }
interface Score { total: number; smoothness: number; delivery: number; latency: number; robustness: number; avg_fps: number; frames_sent: number; frames_recv: number; }
interface Result { presetName: string; score: Score; ts: number; }
type Phase = 'idle' | 'testing' | 'done';

// ── 常量 ──
const MAX_PTS = 120;
const SCORE_DIMS: [keyof Score, string, string][] = [
  ['smoothness', '画面流畅', '40%'],
  ['delivery', '画面送达', '30%'],
  ['latency', '延迟', '20%'],
  ['robustness', '抗丢包', '10%'],
];
const EVENT_ICON: Record<string, string> = {
  fec_recovered: '🔧', frame_lost: '💔', frame_drop: '⏱',
};
const EVENT_TEXT: Record<string, string> = {
  fec_recovered: '自动修复了丢失的数据片',
  frame_lost: '丢了一帧（坏得太厉害救不回来）',
  frame_drop: '来不及发送，扔掉一帧',
};

// ── 工具 ──
function scoreColor(v: number) { return v >= 80 ? '#3fb950' : v >= 60 ? '#d29922' : '#e5484d'; }

// ── 迷你折线图 ──
function Spark({ data, color, label, unit }: { data: number[]; color: string; label: string; unit: string }) {
  const w = 280, h = 60;
  const max = Math.max(...data, 0.001);
  const pts = data.map((v, i) => `${(i / Math.max(data.length - 1, 1)) * w},${h - (v / max) * (h - 6) - 3}`).join(' ');
  const cur = data.length ? data[data.length - 1] : 0;
  const avgV = data.length ? data.reduce((a, b) => a + b, 0) / data.length : 0;
  return (
    <div className="spark">
      <div className="spark-head">
        <b>{label}</b>
        <span>
          <span style={{ color: props_color(color) }}>{cur.toFixed(1)}</span>
          <span className="hint"> / 均{avgV.toFixed(1)} {unit}</span>
        </span>
      </div>
      <svg width="100%" height={h} viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none">
        <polyline points={pts} fill="none" stroke={color} strokeWidth="1.6" />
      </svg>
    </div>
  );
}
function props_color(c: string) { return c; }

// ── 评分卡（动画出分） ──
function ScoreReveal({ s, name }: { s: Score; name: string }) {
  return (
    <div className="score-reveal">
      <div className="score-reveal-name">{name}</div>
      <div className="score-big" style={{ color: scoreColor(s.total) }}>
        {s.total.toFixed(0)}<span className="score-unit">分</span>
      </div>
      <div className="score-dims">
        {SCORE_DIMS.map(([k, label]) => (
          <div key={k} className="score-dim">
            <span className="score-dim-label">{label}</span>
            <div className="score-dim-track">
              <div className="score-dim-fill" style={{ width: `${s[k] as number}%` }} />
            </div>
            <span className="score-dim-val">{(s[k] as number).toFixed(0)}</span>
          </div>
        ))}
      </div>
      <div className="hint">实测平均 {s.avg_fps.toFixed(1)} 帧/秒 · 送达 {s.frames_recv}/{s.frames_sent} 帧</div>
    </div>
  );
}

// ── 主组件 ──
export default function RuntimeView() {
  const [selected, setSelected] = useState<Preset>(PRESETS[0]);
  const [phase, setPhase] = useState<Phase>('idle');
  const [testPreset, setTestPreset] = useState('');
  const [elapsed, setElapsed] = useState(0);
  const [stats, setStats] = useState<Stat[]>([]);
  const [events, setEvents] = useState<{ icon: string; text: string }[]>([]);
  const [results, setResults] = useState<Result[]>([]);
  const [lastScore, setLastScore] = useState<{ s: Score; name: string } | null>(null);
  const [errMsg, setErrMsg] = useState('');

  const wsRef = useRef<WebSocket | null>(null);
  const stopRef = useRef(false);
  const runningRef = useRef(false);
  const sessionIdRef = useRef<number | null>(null);

  // 卸载清理
  useEffect(() => () => { wsRef.current?.close(); }, []);

  // ── 停止 ──
  const doStop = useCallback(() => {
    stopRef.current = true;
    wsRef.current?.close();
    if (sessionIdRef.current !== null) {
      void fetch(`/api/session/${sessionIdRef.current}`, { method: 'DELETE' });
    }
    setPhase('idle');
  }, []);

  // ── 跑一个方案 ──
  const runPreset = useCallback((preset: Preset): Promise<Score> => {
    return new Promise((resolve, reject) => {
      setTestPreset(preset.name);
      setPhase('testing');
      setStats([]);
      setEvents([]);
      setElapsed(0);

      fetch('/api/session', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(preset.params),
      })
        .then((r) => {
          if (!r.ok) throw new Error(`daemon 返回 ${r.status}`);
          return r.json();
        })
        .then(({ id }) => {
          sessionIdRef.current = id;
          const ws = new WebSocket(`ws://${location.host}/api/live/${id}`);
          wsRef.current = ws;
          let settled = false;
          const done = (fn: () => void) => { if (!settled) { settled = true; fn(); } };
          ws.onmessage = (ev) => {
            if (ev.data === '__end__') { ws.close(); return; }
            const msg = JSON.parse(ev.data);
            if (msg.type === 'stats') {
              setStats((old) => [...old, msg as Stat].slice(-MAX_PTS));
            } else if (msg.type === 'event') {
              const icon = EVENT_ICON[msg.event] ?? '❓';
              const text = EVENT_TEXT[msg.event] ?? msg.event;
              setEvents((e) => [{ icon, text }, ...e].slice(0, 12));
            } else if (msg.type === 'ended') {
              done(() => {
                setPhase('done');
                if (msg.score) resolve(msg.score as Score);
                else reject(new Error('无评分'));
              });
            }
          };
          ws.onerror = () => done(() => reject(new Error('连接 daemon 失败')));
          ws.onclose = () => done(() => { if (!settled) reject(new Error('连接已断开')); });
        })
        .catch((e) => { runningRef.current = false; reject(e); });
    });
  }, []);

  // 计时器
  useEffect(() => {
    if (phase !== 'testing') return;
    const iv = setInterval(() => setElapsed((e) => e + 1), 1000);
    return () => clearInterval(iv);
  }, [phase]);

  // ── 测试一个 ──
  const testOne = useCallback(async (preset: Preset) => {
    if (runningRef.current) return;
    runningRef.current = true;
    stopRef.current = false;
    setErrMsg('');
    try {
      const score = await runPreset(preset);
      setResults((rs) => [...rs, { presetName: preset.name, score, ts: Date.now() }]);
      setLastScore({ s: score, name: preset.name });
    } catch (e) {
      setErrMsg(e instanceof Error ? e.message : String(e));
      setPhase('idle');
    } finally {
      runningRef.current = false;
    }
  }, [runPreset]);

  // ── 依次测全部 ──
  const testAll = useCallback(async () => {
    if (runningRef.current) return;
    runningRef.current = true;
    stopRef.current = false;
    setErrMsg('');
    const ready = PRESETS.filter((p) => p.badge === '开箱即用');
    for (const p of ready) {
      if (stopRef.current) break;
      setSelected(p);
      try {
        const score = await runPreset(p);
        setResults((rs) => [...rs, { presetName: p.name, score, ts: Date.now() }]);
      } catch (e) {
        setErrMsg(e instanceof Error ? e.message : String(e));
        break;
      }
    }
    runningRef.current = false;
    setPhase('idle');
  }, [runPreset]);

  // ── 渲染 ──
  const fpsData = stats.map((s) => s.fps);
  const mbpsData = stats.map((s) => s.recv_mbps);
  const lossData = stats.map((s) => s.loss_pct);
  const progressPct = phase === 'testing' && testPreset
    ? Math.min(100, (elapsed / (selected.params.duration_s || 10)) * 100)
    : 0;

  const ranked = [...results].sort((a, b) => b.score.total - a.score.total);

  return (
    <div className="plan-page">
      {/* ── 标题 + 步骤指示 ── */}
      <div className="plan-head">
        <h2>选择使用场景，一键测出哪个方案最适合你</h2>
        <div className="steps-ind">
          <span className={`step ${phase === 'idle' ? 'step-on' : ''}`}>① 选场景</span>
          <span className="step-arrow">→</span>
          <span className={`step ${phase === 'testing' ? 'step-on' : ''}`}>② 点测试</span>
          <span className="step-arrow">→</span>
          <span className={`step ${results.length > 0 ? 'step-on' : ''}`}>③ 看分数</span>
        </div>
      </div>

      {/* ── 方案卡片 ── */}
      <div className="preset-grid">
        {PRESETS.map((p) => {
          const best = ranked.find((r) => r.presetName === p.name);
          return (
            <div
              key={p.id}
              className={`preset-card ${selected.id === p.id ? 'preset-on' : ''}`}
              onClick={() => setSelected(p)}
            >
              <div className="preset-head">
                <b>{p.name}</b>
                <span className={`badge ${p.badge === '开箱即用' ? 'badge-lc' : 'badge-warn'}`}>{p.badge}</span>
              </div>
              <div className="preset-scene">{p.scene}</div>
              <div className="preset-chain">{p.chain.join(' → ')}</div>
              {best && (
                <div className="preset-score" style={{ color: scoreColor(best.score.total) }}>
                  上次：{best.score.total.toFixed(0)} 分
                </div>
              )}
            </div>
          );
        })}
      </div>

      {/* ── 操作按钮 ── */}
      <div className="plan-actions">
        <button className="btn btn-primary" disabled={phase === 'testing'} onClick={() => void testOne(selected)}>
          ▶ 测试「{selected.name.replace(/^方案 [A-D] · /, '')}」
        </button>
        <button className="btn btn-outline" disabled={phase === 'testing'} onClick={() => void testAll()}>
          ▶ 依次测试全部
        </button>
        {phase === 'testing' && (
          <button className="btn btn-danger" onClick={doStop}>■ 停止</button>
        )}
      </div>

      {/* ── 进度条 ── */}
      {phase === 'testing' && (
        <div className="test-progress">
          <div className="test-progress-label">
            正在测试 <b>{testPreset}</b>（{elapsed}s）
          </div>
          <div className="test-progress-bar">
            <div className="test-progress-fill" style={{ width: `${progressPct}%` }} />
          </div>
        </div>
      )}

      {/* ── 实时曲线 ── */}
      {stats.length > 0 && (
        <div className="rt-charts">
          <Spark data={fpsData} color="#3fb950" label="画面流畅度" unit="帧/秒" />
          <Spark data={mbpsData} color="#58a6ff" label="占用网速" unit="Mbps" />
          <Spark data={lossData} color="#e5484d" label="丢帧比例" unit="%" />
        </div>
      )}

      {/* ── 事件流 ── */}
      {events.length > 0 && (
        <div className="rt-events">
          <b>刚才发生了什么</b>
          <div className="rt-event-list">
            {events.map((e, i) => (
              <div key={i} className="rt-event">{e.icon} {e.text}</div>
            ))}
          </div>
        </div>
      )}

      {/* ── 最近评分卡 ── */}
      {lastScore && phase !== 'testing' && (
        <ScoreReveal s={lastScore.s} name={lastScore.name} />
      )}

      {/* ── 排行榜 ── */}
      {results.length > 0 && (
        <div className="results">
          <div className="results-head">
            <h3>测试排名（满分 100）</h3>
            <button className="btn-export" onClick={() => exportResults(results)}>
              导出结果
            </button>
          </div>
          {ranked.map((r, i) => (
            <div key={i} className="result-row">
              <span className={`rank rank-${i + 1}`}>{i + 1}</span>
              <span className="rname">{r.presetName}</span>
              <div className="bar-track">
                <div className="bar-fill" style={{ width: `${r.score.total}%`, background: scoreColor(r.score.total) }} />
                <span className="bar-val">{r.score.total.toFixed(0)} 分</span>
              </div>
            </div>
          ))}
        </div>
      )}

      {/* ── 错误 ── */}
      {errMsg && (
        <div className="err-banner">{errMsg}</div>
      )}

      {/* ── 测试依据 ── */}
      <details className="basis">
        <summary>这个测试是怎么进行的？（测试依据）</summary>
        <div className="basis-body">
          <p><b>真实的部分：</b>纠错编码、分片打包、丢帧恢复这些模块代码是真实运行的——同样的代码将来跑在真实连接里。</p>
          <p><b>模拟的部分：</b>网络是参数化模拟的，分数用于横向对比不同方案在同一网络下的表现。</p>
          <p><b>评分依据：</b>Switchboard 规范 04 篇（流畅 40% + 送达 30% + 延迟 20% + 抗丢包 10%）。</p>
          <p><b>边界：</b>模拟 ≠ 真实公网。真实成绩需要两台真机 + 部署中转（见下方指引）。</p>
        </div>
      </details>

      {/* ── 部署指引 ── */}
      <details className="deploy-guide">
        <summary>真实环境部署指引（中转服务器 / 穿墙打洞 / 组网）</summary>
        <div className="deploy-body">
          {PRESETS.filter((p) => p.deploy).map((p) => (
            <div key={p.id} className="deploy-item">
              <b>{p.deploy!.title}</b>
              <ol>{p.deploy!.steps.map((st, i) => <li key={i}>{st}</li>)}</ol>
            </div>
          ))}
        </div>
      </details>
    </div>
  );
}

// ── 导出结果 ──
function exportResults(results: Result[]) {
  const data = results.map((r) => ({
    方案: r.presetName,
    总分: r.score.total.toFixed(1),
    流畅: r.score.smoothness.toFixed(1),
    送达: r.score.delivery.toFixed(1),
    延迟: r.score.latency.toFixed(1),
    抗丢包: r.score.robustness.toFixed(1),
    测试时间: new Date(r.ts).toLocaleString(),
  }));
  const csv = [
    Object.keys(data[0]).join(','),
    ...data.map((d) => Object.values(d).join(',')),
  ].join('\n');
  const blob = new Blob(['﻿' + csv], { type: 'text/csv' });
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = '方案测试结果.csv';
  a.click();
}

