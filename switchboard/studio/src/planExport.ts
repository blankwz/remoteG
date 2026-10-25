// 画布图 → Plan TOML（spec/02）导出
import type { ModuleManifest, Stage } from './types';

export interface SWNode {
  id: string;
  stage: Stage;
  moduleId: string;
}

export interface SWEdge {
  source: string;
  target: string;
}

// 允许的阶段衔接（02 §2 路径语义）
export const ALLOWED: Record<Stage, Stage[]> = {
  discovery: ['punch', 'relay', 'transport'],
  punch: ['transport'],
  relay: ['transport'],
  transport: ['media'],
  media: ['codec'],
  codec: [],
};

// media.profile=fec-shards 对 transport 的 caps 门控（02 §2.3）
export function capsCompat(
  from: ModuleManifest | undefined,
  to: ModuleManifest | undefined
): { ok: boolean; reason: string } {
  if (!from || !to) return { ok: true, reason: '' };
  const needDatagram =
    to.provides && (to.provides as { profile?: string }).profile === 'fec-shards';
  if (needDatagram) {
    const d = from.provides?.datagram as { lossy_ok?: boolean; max_size?: number } | null | undefined;
    if (!d || d.lossy_ok !== true) {
      return { ok: false, reason: `${from.id} 无 lossy datagram，无法承接 fec-shards` };
    }
  }
  return { ok: true, reason: '' };
}

interface GraphLike {
  nodes: SWNode[];
  edges: SWEdge[];
}

// 图 → TOML：每条 transport 链生成一个 [[path]]
export function exportPlanTOML(g: GraphLike, _mods: Map<string, ModuleManifest>): string {
  const byId = new Map(g.nodes.map((n) => [n.id, n]));
  const lines: string[] = [];
  lines.push('spec = "switchboard/0.1"');
  lines.push(`name = "studio-export-${new Date().toISOString().slice(0, 10)}"`);
  lines.push('');
  const transports = g.nodes.filter((n) => n.stage === 'transport');
  for (const t of transports) {
    const chain = upstreamOf(t.id, g);
    const disc = chain.map((id) => byId.get(id)!).find((n) => n.stage === 'discovery');
    const punch = chain.map((id) => byId.get(id)!).find((n) => n.stage === 'punch');
    const relayNode = chain.map((id) => byId.get(id)!).find((n) => n.stage === 'relay');
    lines.push('[[path]]');
    lines.push(`name = "${t.moduleId.replace(/\./g, '-')}"`);
    if (disc) lines.push(`discovery = "${disc.moduleId}"`);
    if (punch) lines.push(`punch = "${punch.moduleId}"`);
    if (relayNode) lines.push(`fallback_relay = "${relayNode.moduleId}"`);
    lines.push(`transport = "${t.moduleId}"`);
    lines.push('');
  }
  const media = g.nodes.find((n) => n.stage === 'media');
  if (media) {
    lines.push('[media]');
    lines.push('profile = "fec-shards"');
    lines.push(`adapter = "${media.moduleId}"`);
    lines.push('');
  }
  const codec = g.nodes.find((n) => n.stage === 'codec');
  if (codec) {
    lines.push('[media.codec]');
    lines.push(`preset = "${codec.moduleId}"`);
    lines.push('');
  }
  return lines.join('\n');
}

// 沿边反向收集到 transport 的上游链（不含 transport 自身）
function upstreamOf(tid: string, g: GraphLike): string[] {
  const chain: string[] = [];
  let cur = tid;
  for (;;) {
    const inc = g.edges.find((e) => e.target === cur);
    if (!inc) break;
    // 端点节点（本机/远程机）不属于链路模块，跳过
    if (inc.source.startsWith('end-')) break;
    chain.unshift(inc.source);
    cur = inc.source;
    const node = g.nodes.find((n) => n.id === cur);
    if (!node || node.stage === 'discovery') break;
  }
  return chain;
}
