// 方案测试页（默认）：选场景 → 一键测试 → 自动出分排名
import { useCallback, useEffect, useRef, useState } from 'react';
import { PRESETS, type Preset } from './presets';

interface Stat {
  ts_ms: number;
  fps: number;
  recv_mbps: number;
  rtt_ms: number;
  loss_pct: number;
}
interface Score {
  total: number;
  smoothness: number;
  delivery: number;
  latency: number;
  robustness: number;
  avg_fps: number;
  frames_sent: number;
  frames_recv: number;
}
interface Result {
  presetName: string;
  score: Score;
}

const SCORE_LABEL: [keyof Score, string, string][] = [
  ['smoothness', '画面流畅', '40%'],
  ['delivery', '画面送达率', '30%'],
  ['latency', '延迟表现', '20%'],
  ['robustness', '抗丢包表现', '10%'],
];

function Spark(props: { data: number[]; color: string; label: string; unit: string }) {
  const w = 260;
  const h = 56;
  const max = Math.max(...props.data, 0.0001);
  const pts = props.data
    .map((v, i) => `${(i / Math.max(props.data.length - 1, 1)) * w},${h - (v / max) * (h - 6) - 3}`)
    .join(' ');
  const last = props.data.length ? props.data[props.data.length - 1] : 0;
  return (
    <div className="spark">
      <div className="spark-head">
        <b>{props.label}</b>
        <span style={{ color: props.color }}>
          {last.toFixed(1)} {props.unit}
        </span>
      </div>
      <svg width={w} height={h}>
        <polyline points={pts} fill="none" stroke={props.color} strokeWidth="1.6" />
      </svg>
    </div>
  );
}

const DEPLOY_ITEMS = PRESETS.filter((p) => p.deploy).map((p) => ({
  title: p.deploy!.title,
  steps: p.deploy!.steps,
}));

function ScoreCard({ s }: { s: Score }) {
  return (
    <div className="score-card">
      <div className="score-total">
        <span className="score-num">{s.total.toFixed(0)}</span>
        <span className="score-unit">分</span>
      </div>
      <div className="score-bars">
        {SCORE_LABEL.map(([k, label, w]) => (
          <div key={k} className="bar-row">
            <span className="bar-label">{label}（{w}）</span>
            <div className="bar-track">
              <div className="bar-fill" style={{ width: `${s[k] as number}%`, background: '#3fb950' }} />
              <span className="bar-val">{(s[k] as number).toFixed(0)}</span>
            </div>
          </div>
        ))}
      </div>
      <div className="score-sub hint">
        实测平均 {s.avg_fps.toFixed(1)} 帧/秒 · 送达 {s.frames_recv}/{s.frames_sent} 帧
      </div>
    </div>
  );
}

interface LiveState {
  running: boolean;
  presetName: string;
  stats: Stat[];
  events: string[];
}

export default function RuntimeView() {
  const [selected, setSelected] = useState<Preset>(PRESETS[0]);
  const [live, setLive] = useState<LiveState>({ running: false, presetName: '', stats: [], events: [] });
  const [results, setResults] = useState<Result[]>([]);
  const [busy, setBusy] = useState<string>('');
  const [detail, setDetail] = useState<{ r: Result } | null>(null);   // 正在跑的提示（含依次测试进度）
  const wsRef = useRef<WebSocket | null>(null);
  const stopRef = useRef(false);
  const runningRef = useRef(false);
  // 审查 #8：组件卸载时关闭 WS（会话由时长兜底结束）
  useEffect(
    () => () => {
      wsRef.current?.close();
    },
    []
  );

  const stop = useCallback(() => {
    stopRef.current = true;
    wsRef.current?.close();
    wsRef.current = null;
    setLive((l) => ({ ...l, running: false }));
  }, []);

  /** 跑一个方案直到出分；resolve 出 Score */
  const runOne = useCallback((preset: Preset): Promise<Score> => {
    return new Promise((resolve, reject) => {
      if (runningRef.current) return; // 竞态锁
    runningRef.current = true;
    setLive({ running: true, presetName: preset.name, stats: [], events: [] });
      fetch('/api/session', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(preset.params),
      })
        .then((r) => r.json())
        .then(({ id }) => {
          const ws = new WebSocket(`ws://${location.host}/api/live/${id}`);
          wsRef.current = ws;
          let settled = false;
          const settle = (fn: (v: Score) => void, v: Score) => {
            if (!settled) {
              settled = true;
              fn(v);
            }
          };
          ws.onmessage = (ev) => {
            if (ev.data === '__end__') {
              ws.close();
              return;
            }
            const msg = JSON.parse(ev.data);
            if (msg.type === 'stats') {
              setLive((l) => ({
                ...l,
                stats: [...l.stats, msg as Stat].slice(-120),
              }));
            } else if (msg.type === 'event') {
              const human: Record<string, string> = {
                fec_recovered: '🔧 自动修复丢包',
                frame_lost: '💔 丢了一帧',
                frame_drop: '⏱ 来不及发送，扔帧',
              };
              const text = (human[msg.event] ?? msg.event) + ' 帧#' + (msg.detail?.frame_id ?? '');
              setLive((l) => ({ ...l, events: [text, ...l.events].slice(0, 12) }));
            } else if (msg.type === 'ended') {
              runningRef.current = false;
              ws.close();
              // 审查 #9：score 缺失（如手动停止无评分）也要 settle，不挂死
              if (msg.score) {
                settle(resolve, msg.score as Score);
              } else {
                settle(reject as unknown as (v: Score) => void, {
                  total: 0, smoothness: 0, delivery: 0, latency: 0, robustness: 0,
                  avg_fps: 0, frames_sent: 0, frames_recv: 0,
                } as Score);
              }
            }
          };
          ws.onerror = () => {
            if (!settled) reject(new Error('连接 daemon 失败'));
          };
          // 审查 #2：点「停止」→ ws.close() → 服务端杀会话；前端必须 settle，不挂死
          ws.onclose = () => {
            if (!settled) {
              settled = true;
              reject(new Error('已停止'));
            }
          };
        })
        .catch(reject);
    });
  }, []);

  /** 测试当前选中的方案 */
  const testSelected = useCallback(async () => {
    stopRef.current = false;
    const preset = selected;
    setBusy(`正在测试 ${preset.name}…`);
    try {
      const score = await runOne(preset);
      setResults((rs) => [...rs, { presetName: preset.name, score }]);
    } catch (e) {
      setBusy('测试中断: ' + String(e));
      return;
    }
    setBusy('');
    setLive((l) => ({ ...l, running: false })); // 审查 #1：出分后复位，UI 不砖化
  }, [selected, runOne]);

  /** 依次测试所有「开箱即用」方案 */
  const testAll = useCallback(async () => {
    stopRef.current = false;
    const ready = PRESETS.filter((p) => p.badge === '开箱即用');
    setBusy(`准备依次测试 ${ready.length} 套方案…`);
    for (const p of ready) {
      if (stopRef.current) break;
      setSelected(p);
      setBusy(`正在测试 ${p.name}…`);
      try {
        const score = await runOne(p);
        setResults((rs) => [...rs, { presetName: p.name, score }]);
      } catch (e) {
        setBusy('测试中断: ' + String(e));
        setLive((l) => ({ ...l, running: false }));
        return;
      }
      setLive((l) => ({ ...l, running: false }));
    }
    setBusy('');
  }, [runOne]);


  // 实时曲线数据（最近 120 个点）
  const fpsData = live.stats.map((s) => s.fps);
  const mbpsData = live.stats.map((s) => s.recv_mbps);
  const lossData = live.stats.map((s) => s.loss_pct);

  return (
    <div className="plan-page">
      <div className="plan-head">
        <h2>三步测出哪个方案适合你</h2>
        <p className="hint">
          第 1 步选场景 → 第 2 步点「测试这个方案」→ 第 3 步看分数。所有方案都在本机演示网络里跑，绿色=流畅。
        </p>
      </div>

      <details className="basis">
        <summary>这个测试是怎么进行的？（测试依据）</summary>
        <div className="basis-body">
          <p><b>真实的部分：</b>纠错编码、分片打包、丢帧恢复这些<b>模块代码是真实运行的</b>——同样的代码将来跑在真实连接里。</p>
          <p><b>模拟的部分：</b>网络是参数化模拟的（延迟/丢包/限速由上方参数决定），所以分数用于<b>横向对比不同方案在同一网络下的表现</b>。</p>
          <p><b>评分依据：</b>《Switchboard 规范》04 篇的锚点口径（流畅 40% + 送达 30% + 延迟 20% + 抗丢包 10%）。</p>
          <p><b>边界：</b>模拟网络 ≠ 真实公网。要拿真实成绩，需要两台真机 + 部署中转（见下方部署指引），走实网 T3/T4 剖面。</p>
        </div>
      </details>

      {/* 第 1 步：方案卡片 */}
      <div className="preset-grid">
        {PRESETS.map((p) => (
          <div
            key={p.id}
            className={`preset-card ${selected.id === p.id ? 'preset-on' : ''}`}
            onClick={() => setSelected(p)}
          >
            <div className="preset-head">
              <b>{p.name}</b>
              <span className={p.badge === '开箱即用' ? 'badge badge-lc' : 'badge badge-warn'}>
                {p.badge}
              </span>
            </div>
            <div className="preset-scene">{p.scene}</div>
            <div className="preset-chain">{p.chain.join(' → ')}</div>
          </div>
        ))}
      </div>

      {/* 第 2 步：测试 */}
      <div className="plan-actions">
        <button className="btn" disabled={live.running} onClick={() => void testSelected()}>
          ▶ 测试「{selected.name.replace(/^方案 [A-D] · /, '')}」
        </button>
        <button className="btn ghost" disabled={live.running} onClick={() => void testAll()}>
          ▶ 依次测试全部（约 1 分钟）
        </button>
        {live.running && <button className="btn ghost" onClick={stop}>停止</button>}
        {busy && <span className="hint">{busy}</span>}
      </div>

      {/* 实时区：曲线 + 事件 */}
      {live.running && (
        <div className="rt-charts">
          <Spark data={fpsData} color="#3fb950" label="画面流畅度" unit="帧/秒" />
          <Spark data={mbpsData} color="#58a6ff" label="占用网速" unit="Mbps" />
          <Spark data={lossData} color="#e5484d" label="丢帧比例" unit="%" />
        </div>
      )}
      {live.running && (
        <div className="rt-events">
          <b>刚才发生了什么</b>
          <div className="rt-event-list">
            {live.events.length === 0 && <span className="hint">网络正常时这里很安静</span>}
            {live.events.map((e, i) => (
              <div key={i} className="rt-event">{e}</div>
            ))}
          </div>
        </div>
      )}

      {/* 第 3 步：分数排行榜 */}
      {results.length > 0 && (
        <div className="results">
          <h3>本轮测试排名（满分 100）</h3>
          {[...results]
            .sort((a, b) => b.score.total - a.score.total)
            .map((r, i) => (
              <div key={i} className="result-row">
                <span className="rank">{i + 1}</span>
                <span className="rname">{r.presetName}</span>
                <div className="bar-track">
                  <div className="bar-fill" style={{ width: `${r.score.total}%`, background: '#3fb950' }} />
                  <span className="bar-val">{r.score.total.toFixed(0)} 分</span>
                </div>
                <button className="btn ghost" onClick={() => setDetail({ r })}>详情</button>
              </div>
            ))}
        </div>
      )}

      {detail && (
        <div className="modal-mask" onClick={() => setDetail(null)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h3>{detail.r.presetName} 的评分明细</h3>
            <ScoreCard s={detail.r.score} />
            <div className="modal-actions">
              <button className="btn ghost" onClick={() => setDetail(null)}>关闭</button>
            </div>
          </div>
        </div>
      )}

      {/* 部署指引：真实环境需要额外部署的说明 */}
      <details className="deploy-guide">
        <summary>真实环境部署指引（中转服务器 / 穿墙打洞 / 组网）</summary>
        <div className="deploy-body">
          {DEPLOY_ITEMS.map((d) => (
            <div key={d.title} className="deploy-item">
              <b>{d.title}</b>
              <ol>
                {d.steps.map((st, i) => (
                  <li key={i}>{st}</li>
                ))}
              </ol>
            </div>
          ))}
        </div>
      </details>
    </div>
  );
}

// 供上方 JSX 使用的常量与工具（文件底部集中定义）
