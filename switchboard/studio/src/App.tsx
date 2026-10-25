// Switchboard Studio 主界面：拖拽编排各阶段模块，导出 Plan TOML
import { useCallback, useEffect, useRef, useState } from 'react';
import {
  ReactFlow,
  ReactFlowProvider,
  Background,
  Controls,
  useNodesState,
  useEdgesState,
  addEdge,
  Handle,
  Position,
  type Node,
  type Edge,
  type Connection,
  type NodeProps,
} from '@xyflow/react';
import '@xyflow/react/dist/style.css';
import type { ModuleManifest, Registry, Stage } from './types';
import RuntimeView from './RuntimeView';
import { DEPLOY_GUIDE, ID_LABEL, MODULE_INFO, TEMPLATES } from './presets';
import BenchView from './BenchView';
import { ALLOWED, capsCompat, exportPlanTOML, type SWNode } from './planExport';

interface SWData {
  stage: Stage;
  moduleId: string;
  options: ModuleManifest[];
  originText: string;
  compliance?: string;
  onDelete?: (nid: string) => void;
  onChange: (nid: string, mid: string) => void;
}

const STAGES: { key: Stage; label: string; desc: string }[] = [
  { key: 'discovery', label: '找到对方', desc: '在局域网里找到要控制的电脑（广播/会合服务器）' },
  { key: 'punch', label: '穿墙直连', desc: '在两边路由器上打个洞，实现点对点直连' },
  { key: 'relay', label: '备用中转', desc: '直连打不通时，走服务器帮忙转发（慢一点但能连）' },
  { key: 'transport', label: '数据通道', desc: '决定数据怎么送：QUIC / KCP / SRT，各有快慢取舍' },
  { key: 'media', label: '画质保障', desc: '画面分片打包、抗丢包、自动调清晰度' },
  { key: 'codec', label: '画面压缩', desc: '把画面压缩成视频流（省带宽的压缩档位）' },
];

function stageLabel(k: Stage): string {
  return STAGES.find((s) => s.key === k)?.label ?? k;
}

function badgeClass(c: string): string {
  if (c === 'L-C') return 'badge badge-lc';
  if (c === 'L-B') return 'badge badge-lb';
  return 'badge badge-la';
}

// ── 自定义节点 ──
/// 端点与逐段机制的大白话解释（「这条链是怎么通的」数据源）
const EXPLAIN: Record<string, string> = {
  __local: '本机 = 运行控制端的一方：键盘鼠标在这里发出，远程画面在这里显示',
  __remote: '远程机 = 运行被控端的一方：画面在这里被捕获压缩，你的指令在这里被注入',
  'sb.lan.udp-broadcast': '本机在局域网广播「有人吗」，远程机应答自己的 IP 和端口 → 拿到局域网直连地址',
  'sb.stun.punch-v2': '两边各自问 STUN 服务器「我的公网地址」，然后同时互发探测包，在两边路由器上打出洞 → 直连',
  'sb.derp.self-hk': '两台机器都先连上中转服务器；打洞失败时数据经它加密转发（慢一点但一定能通）',
  'sb.quinn.quic': '本机作为 QUIC 客户端，主动连远程机的 21118 端口；双方交换证书指纹并钉扎校验，防中间人',
  'sb.kcp.kcp': '在 UDP 上做快速重传（比 QUIC 更激进）：延迟更低、更耗带宽',
  'sb.ext.tailscale': 'Tailscale 先在两台机器间建好加密隧道（打洞+中转它全包），rd 流量走这条隧道',
  'sb.moonlight.fec-rs': '远程机把画面切片+纠错码发出；本机收够多数分片即可还原，丢少量片不用重传',
  'sb.core.fec-xor': '远程机把画面切片，每 8 片附 1 片异或校验；丢 1 片可异或重建',
  'sb.core.loopback': '数据不出本机内存——纯演示',
  'sb.core.lossy-sim': '在通道上人为加延迟/丢包/限速，模拟真实网络',
  'sb.core.relay-loopback': '演示用中转：数据绕一行再回来（path 如实标 relay）',
};

/// 端点节点（本机/远程机）：由模板自动连线，不可删除
function EndNodeView({ data }: NodeProps) {
  const d = data as { side: 'local' | 'remote' };
  const isLocal = d.side === 'local';
  return (
    <div className={`end-node ${isLocal ? 'end-local' : 'end-remote'}`}>
      {isLocal ? null : <Handle type="target" position={Position.Left} />}
      <div className="end-title">{isLocal ? '🖥️ 本机 · 控制端' : '💻 远程机 · 被控端'}</div>
      <div className="end-foot">键盘鼠标从这里发出 · 画面在这里显示</div>
      {isLocal ? <Handle type="source" position={Position.Right} /> : null}
    </div>
  );
}

function SWNodeView(props: NodeProps) {
  const { id, data } = props;
  const d = data as unknown as SWData;
  return (
    <div className="sw-node">
      <Handle type="target" position={Position.Left} />
      <div className="sw-node-head">
        <span className="sw-stage">{stageLabel(d.stage)}</span>
        <button
          className="sw-del"
          title="删除该节点（同时断开连线）"
          onClick={(e) => {
            e.stopPropagation();
            d.onDelete?.(id);
          }}
        >
          ✕
        </button>
        {d.compliance && <span className={badgeClass(String(d.compliance))}>{String(d.compliance)}</span>}
      </div>
      <select
        className="sw-select"
        value={d.moduleId}
        onChange={(e) => d.onChange(id, e.target.value)}
      >
        {d.options.map((m: ModuleManifest) => (
          <option key={m.id} value={m.id}>
            {ID_LABEL[m.id] ?? m.id}（{m.compliance}）
          </option>
        ))}
      </select>
      <div className="sw-node-sub">{d.originText}</div>
      <Handle type="source" position={Position.Right} />
    </div>
  );
}

// ── 默认示例图（lan 发现 → quinn 传输 → fec 媒体） ──
function chainGraph(
  mods: ModuleManifest[],
  chain: { stage: Stage; moduleId: string }[],
  onChange: (nid: string, mid: string) => void,
): { nodes: Node[]; edges: Edge[] } {
  const mk = (id: string, stage: Stage, mid: string, x: number, y: number): Node => {
    const m =
      mods.find((z) => z.id === mid && z.stage === stage) ??
      ({
        id: mid,
        stage,
        version: '0',
        binding: 'native-rust',
        telemetry: '',
        compliance: 'L-A',
      } as unknown as ModuleManifest);
    return {
      id,
      type: 'sw',
      position: { x, y },
      data: {
        stage,
        moduleId: mid,
        options: mods.filter((z) => z.stage === stage),
        originText: m.origin?.adapted_from ?? '',
        compliance: m.compliance,
        onChange,
      },
    };
  };
  const endLocal: Node = {
    id: 'end-local',
    type: 'end',
    position: { x: -300, y: 120 },
    data: { side: 'local' },
    deletable: false,
  };
  const endRemote: Node = {
    id: 'end-remote',
    type: 'end',
    position: { x: 40 + (chain.length + 1) * 300, y: 120 },
    data: { side: 'remote' },
    deletable: false,
  };
  const nodes: Node[] = [endLocal];
  const edges: Edge[] = [
    {
      id: 'ee0',
      source: 'end-local',
      target: 'c0',
      animated: true,
      label: '指令/连接',
      labelStyle: { fontSize: 10, fill: '#8b949e' },
    },
  ];
  chain.forEach((c, i) => {
    nodes.push(mk(`c${i}`, c.stage, c.moduleId, 40 + (i + 1) * 300, 120));
    edges.push({
      id: `ec${i}`,
      source: i === 0 ? 'end-local' : `c${i - 1}`,
      target: `c${i}`,
      animated: true,
    });
  });
  edges.push({
    id: 'er',
    source: `c${chain.length - 1}`,
    target: 'end-remote',
    animated: true,
    label: '画面回来',
    labelStyle: { fontSize: 10, fill: '#bc8cff' },
  });
  nodes.push(endRemote);
  return { nodes, edges };
}

// ── 主组件 ──
function Studio() {
  const [mods, setMods] = useState<ModuleManifest[]>([]);
  const [nodes, setNodes, onNodesChange] = useNodesState<Node>([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState<Edge>([]);
  const [sel, setSel] = useState<string | null>(null);
  const [toml, setToml] = useState<string | null>(null);
  const [err, setErr] = useState<string>('');
  const [tab, setTab] = useState<'plan' | 'adv' | 'bench'>('plan');

  const onChangeModule = useCallback(
    (nid: string, mid: string) => {
      setNodes((ns) =>
        ns.map((n) => {
          if (n.id !== nid) return n;
          const d = n.data as unknown as SWData;
          const m = d.options.find((x) => x.id === mid);
          return { ...n, data: { ...d, moduleId: mid, originText: m?.origin?.adapted_from ?? '', compliance: m?.compliance } };
        })
      );
    },
    [setNodes]
  );
  const onChangeModuleRef = useRef(onChangeModule);
  onChangeModuleRef.current = onChangeModule;

  const deleteNode = useCallback(
    (nid: string) => {
      setNodes((ns) => ns.filter((n) => n.id !== nid));
      setEdges((es) => es.filter((e) => e.source !== nid && e.target !== nid));
      setSel((s0) => (s0 === nid ? null : s0));
    },
    [setNodes, setEdges]
  );
  const deleteNodeRef = useRef(deleteNode);
  deleteNodeRef.current = deleteNode;

  const applyTemplate = useCallback(
    (ti: number) => {
      const tpl = TEMPLATES[ti];
      if (!tpl) return;
      const g = chainGraph(mods, tpl.chain, onChangeModuleRef.current);
      setNodes(g.nodes);
      setEdges(g.edges);
      setSel(null);
    },
    [mods, setNodes, setEdges]
  );

  useEffect(() => {
    fetch('/registry.json')
      .then((r) => r.json() as Promise<Registry>)
      .then((r) => {
        setMods(r.modules);
        const g = chainGraph(r.modules, TEMPLATES[0].chain, onChangeModuleRef.current);
        setNodes(g.nodes);
        setEdges(g.edges);
      })
      .catch((e) => setErr('注册表加载失败: ' + String(e)));
  }, []);

  const onConnect = useCallback(
    (c: Connection) => {
      const s = nodes.find((n) => n.id === c.source);
      const t = nodes.find((n) => n.id === c.target);
      if (!s || !t) return;
      if (s.type === 'end' || t.type === 'end') {
        setErr('本机 / 远程机节点由模板自动连线，无需手动连接');
        return;
      }
      const sd = s.data as unknown as SWData;
      const td = t.data as unknown as SWData;
      if (!ALLOWED[sd.stage]?.includes(td.stage)) {
        setErr(`阶段顺序不允许: ${stageLabel(sd.stage)} → ${stageLabel(td.stage)}`);
        return;
      }
      const sm = mods.find((m) => m.id === sd.moduleId);
      const tm = mods.find((m) => m.id === td.moduleId);
      const caps = capsCompat(sm, tm);
      setEdges((eds) =>
        addEdge(
          {
            ...c,
            animated: true,
            style: caps.ok ? {} : { stroke: '#e5484d', strokeWidth: 2 },
            label: caps.ok ? '' : 'caps 不兼容',
            labelStyle: { fill: '#e5484d', fontSize: 10 },
          },
          eds
        )
      );
      setErr('');
    },
    [nodes, mods, setEdges]
  );

  const onDrop = useCallback(
    (ev: React.DragEvent) => {
      ev.preventDefault();
      const stage = ev.dataTransfer.getData('application/sw-stage') as Stage;
      if (!stage) return;
      const options = mods.filter((m) => m.stage === stage);
      if (options.length === 0) {
        setErr(`注册表中暂无 ${stageLabel(stage)} 阶段模块`);
        return;
      }
      const position = { x: ev.clientX - 60, y: ev.clientY - 40 };
      const id = `n${Date.now()}`;
      setNodes((ns) =>
        ns.concat({
          id,
          type: 'sw',
          position,
          data: {
            stage,
            moduleId: options[0].id,
            options,
            originText: options[0].origin?.adapted_from ?? '',
            compliance: options[0].compliance,
          },
        })
      );
    },
    [mods, setNodes]
  );

  function exportTOML() {
    const g = {
      nodes: nodes
        .filter((n) => n.type !== 'end')
        .map((n) => {
          const d = n.data as unknown as SWData;
          return { id: n.id, stage: d.stage, moduleId: d.moduleId };
        }) as SWNode[],
      edges: edges.map((e) => ({ source: e.source, target: e.target })),
    };
    const modsMap = new Map(mods.map((m) => [m.id, m]));
    setToml(exportPlanTOML(g, modsMap));
  }

  const selNode = nodes.find((n) => n.id === sel);
  const selData = selNode?.data as unknown as (SWData & { compliance?: string }) | undefined;
  const selMod = selData ? mods.find((m) => m.id === selData.moduleId) : undefined;

  return (
    <ReactFlowProvider>
      <div className="tabs">
        {([['plan', '方案测试'], ['adv', '高级编排'], ['bench', '跑分对比']] as const).map(([k, label]) => (
          <button key={k} className={`tab ${tab === k ? 'tab-on' : ''}`} onClick={() => setTab(k)}>
            {label}
          </button>
        ))}
      </div>
      {tab === 'plan' && <RuntimeView />}
      {tab === 'bench' && <BenchView />}
      <div className="layout" style={{ display: tab === 'adv' ? 'flex' : 'none' }}>
        <aside className="palette">
          <h1 className="brand">Switchboard<br />Studio</h1>
          <p className="hint">像搭积木：把左边的卡片拖到画布，连成一条线就是一套「连接方案」。每个节点上的下拉框可以直接换品牌实现。</p>
          <div className="tpl-box">
            <b>方案模板（一键铺满画布）</b>
            {TEMPLATES.map((t, i) => (
              <button key={i} className="tpl-btn" onClick={() => applyTemplate(i)} title={t.desc}>
                {t.name}
              </button>
            ))}
          </div>
          <div className="howto">
            <b>怎么用</b>
            <ol>
              <li>点上方模板一键铺画布，或自己拖</li>
              <li>删除：节点右上角 ✕，或选中后按 Delete</li>
              <li>连线：从节点右侧拖到下一个节点左侧</li>
              <li>下拉框换实现；点节点看详解</li>
              <li>「导出 Plan」给 rd / bench 使用</li>
            </ol>
          </div>
          {STAGES.map((s) => (
            <div
              key={s.key}
              className="pal-item"
              draggable
              onDragStart={(e) => e.dataTransfer.setData('application/sw-stage', s.key)}
              title={s.desc}
            >
              <b>{s.label}</b>
              <span>{s.desc}</span>
            </div>
          ))}
          <div className="legend">
            <span className="badge badge-lc">L-C 认证</span>
            <span className="badge badge-lb">L-B 可用</span>
            <span className="badge badge-la">L-A 登记</span>
          </div>
        </aside>

        <div
          className="canvas"
          onDrop={onDrop}
          onDragOver={(e) => {
            e.preventDefault();
            e.dataTransfer.dropEffect = 'copy';
          }}
        >
          <ReactFlow
            nodes={nodes}
            edges={edges}
            nodeTypes={{ sw: SWNodeView, end: EndNodeView }}
            onNodesChange={onNodesChange}
            onEdgesChange={onEdgesChange}
            onConnect={onConnect}
            deleteKeyCode={['Backspace', 'Delete']}
            onNodeClick={(_, n) => setSel(n.id)}
            fitView
          >
            <Background />
            <Controls />
          </ReactFlow>
          <div className="hud">
            {err ? <span className="err">{err}</span> : <span>连成一条线 = 一套方案；红线表示两边能力对不上（运行时会自动换备选）</span>}
            <button className="btn" onClick={exportTOML}>导出 Plan TOML</button>
          </div>
        </div>

        <aside className="inspector">
          <h2>节点详情</h2>
          {!selNode && <p className="hint">点画布上的节点，看这个模块是谁家的、怎么工作的</p>}
          {selNode && selData && (
            <div>
              <p className="hint">阶段：{stageLabel(selData.stage)}</p>
              <p><b>{selData.moduleId}</b></p>
              <p className="hint">{selMod?.origin?.adapted_from}</p>
              <p className="hint">{selMod?.bench_note}</p>
              {selMod?.principles_card && (
                <p className="hint">原理卡片: {selMod.principles_card}</p>
              )}
              <p className="hint">上游: {selMod?.origin?.upstream_ref?.join(', ')}</p>
              {selData.moduleId && EXPLAIN[selData.moduleId] && (
                <div className="mod-info">
                  <p><b>怎么实现的：</b>{EXPLAIN[selData.moduleId]}</p>
                </div>
              )}
              {selData.moduleId && MODULE_INFO[selData.moduleId] && (
                <div className="mod-info">
                  <p><b>作用：</b>{MODULE_INFO[selData.moduleId].role}</p>
                  <p><b>影响：</b>{MODULE_INFO[selData.moduleId].impact}</p>
                  <p><b>何时用：</b>{MODULE_INFO[selData.moduleId].when}</p>
                </div>
              )}
              {selData.moduleId && DEPLOY_GUIDE[selData.moduleId] && (
                <div className="deploy-guide" style={{ marginTop: 10 }}>
                  <b>{DEPLOY_GUIDE[selData.moduleId].title}</b>
                  <ol style={{ margin: '6px 0 0 16px', padding: 0 }}>
                    {DEPLOY_GUIDE[selData.moduleId].steps.map((st, i) => (
                      <li key={i} className="hint">{st}</li>
                    ))}
                  </ol>
                </div>
              )}
            </div>
          )}
          <div className="bench-tip">
            <p><b>基准评分</b></p>
            <code>cargo run -p switchboard-bench -- --fast</code>
          </div>
        </aside>

        {toml !== null && (
          <div className="modal-mask" onClick={() => setToml(null)}>
            <div className="modal" onClick={(e) => e.stopPropagation()}>
              <h3>导出连接计划</h3>
              <pre>{toml}</pre>
              <div className="modal-actions">
                <button
                  className="btn"
                  onClick={() => {
                    const blob = new Blob([toml], { type: 'text/plain' });
                    const a = document.createElement('a');
                    a.href = URL.createObjectURL(blob);
                    a.download = 'plan.toml';
                    a.click();
                  }}
                >
                  下载 plan.toml
                </button>
                <button className="btn ghost" onClick={() => setToml(null)}>关闭</button>
              </div>
            </div>
          </div>
        )}
      </div>
    </ReactFlowProvider>
  );
}

export default Studio;
