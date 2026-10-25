// 基准页：加载 bench.json（daemon 或本地文件），分组柱状对比
import { useCallback, useEffect, useState } from 'react';

interface BenchFile {
  name: string;
  module: string;
  profile: string;
  composite: number;
  w: Record<string, number>;
}

const WORKLOADS: [string, string][] = [
  ['W1', '文字清不清楚'],
  ['W2', '鼠标跟不跟手'],
  ['W3', '视频流不畅'],
  ['W6', '网差时恢复多快'],
  ['W7', '打开多快'],
];
const COLORS = ['#3fb950', '#58a6ff', '#d29922', '#e5484d', '#bc8cff', '#39c5cf'];

function parseBench(name: string, v: any): BenchFile {
  const w: Record<string, number> = {};
  const cnt: Record<string, number> = {};
  const add = (key: string, score: number) => {
    w[key] = (w[key] ?? 0) + score;
    cnt[key] = (cnt[key] ?? 0) + 1;
  };
  // 新格式：passes[].runs[].{w1..w7}.score（T3/T4 多口径多 run 取均值）
  for (const p of v.passes ?? []) {
    for (const r of p.runs ?? []) {
      for (const [wk, sk] of [['W1', 'w1'], ['W2', 'w2'], ['W3', 'w3'], ['W6', 'w6'], ['W7', 'w7']]) {
        const sc = Number(r[sk]?.score ?? 0);
        if (sc > 0) add(wk, sc);
      }
    }
  }
  // 旧格式：workloads.{W1..W7}.score
  for (const key of Object.keys(v.workloads ?? {})) {
    const sc = Number(v.workloads[key]?.score ?? 0);
    if (sc > 0) add(key, sc);
  }
  for (const k of Object.keys(w)) {
    w[k] = w[k] / (cnt[k] || 1);
  }
  return {
    name,
    module: String(v.module?.id ?? name),
    profile: String(v.profile ?? 'T0'),
    composite: Number(v.composite ?? v.composite_available ?? 0),
    w,
  };
}

export default function BenchView() {
  const [files, setFiles] = useState<BenchFile[]>([]);
  const [avail, setAvail] = useState<string[]>([]);
  const [err, setErr] = useState('');

  useEffect(() => {
    fetch('/api/bench/list')
      .then((r) => r.json())
      .then((r) => setAvail(r.files ?? []))
      .catch(() => setAvail([]));
  }, []);

  const load = useCallback(async (name: string) => {
    try {
      const r = await fetch(`/api/bench/${encodeURIComponent(name)}`);
      if (!r.ok) throw new Error(await r.text());
      const v = await r.json();
      setFiles((fs) => [...fs.filter((f) => f.name !== name), parseBench(name, v)]);
    } catch (e) {
      setErr(String(e));
    }
  }, []);

  const onUpload = useCallback(async (fl: FileList | null) => {
    if (!fl) return;
    for (const f of Array.from(fl)) {
      try {
        const v = parseBench(f.name, JSON.parse(await f.text()));
        setFiles((fs) => [...fs.filter((x) => x.name !== f.name), v]);
      } catch (e) {
        setErr(`解析 ${f.name} 失败: ${String(e)}`);
      }
    }
  }, []);

  const maxW = 100;
  return (
    <div className="bench">
      <div className="bench-actions">
        <select
          defaultValue=""
          onChange={(e) => e.target.value && void load(e.target.value)}
        >
          <option value="">— 从 registry/bench 加载 —</option>
          {avail.map((f) => (
            <option key={f} value={f}>{f}</option>
          ))}
        </select>
        <label className="btn ghost filebtn">
          上传 bench.json
          <input type="file" multiple accept=".json" hidden onChange={(e) => void onUpload(e.target.files)} />
        </label>
        {err && <span className="err">{err}</span>}
      </div>

      {files.length === 0 && <p className="hint">加载两份以上的跑分报告（比如同一个弱网场景下的两个方案），就能并排比较谁更强。绿色/蓝色的条就是各自的得分。</p>}

      {files.length > 0 && (
        <div className="bench-composite">
          <h3>总分（满分 100，越高越好）</h3>
          {files
            .slice()
            .sort((a, b) => b.composite - a.composite)
            .map((f, i) => (
              <div key={f.name} className="bar-row">
                <span className="bar-label">{f.module}（{f.profile}）</span>
                <div className="bar-track">
                  <div className="bar-fill" style={{ width: `${f.composite}%`, background: COLORS[i % COLORS.length] }} />
                  <span className="bar-val">{f.composite.toFixed(1)}</span>
                </div>
              </div>
            ))}
        </div>
      )}

      {files.length > 0 && (
        <div className="bench-workloads">
          <h3>分项得分</h3>
          {WORKLOADS.map(([wk, label]) => (
            <div key={wk} className="bar-group">
              <b>{wk} {label}</b>
              {files.map((f, i) => (
                <div key={f.name} className="bar-row">
                  <span className="bar-label">{f.module}</span>
                  <div className="bar-track">
                    <div
                      className="bar-fill"
                      style={{ width: `${Math.min((f.w[wk] ?? 0), 100)}%`, background: COLORS[i % COLORS.length] }}
                    />
                    <span className="bar-val">{(f.w[wk] ?? 0).toFixed(1)}</span>
                  </div>
                </div>
              ))}
            </div>
          ))}
        </div>
      )}
      <div className="bench-note hint">最大刻度 = 100 分（{maxW}）；评分口径见 spec/04。</div>
    </div>
  );
}
