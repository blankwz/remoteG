// 预设方案：每个都是「一句话场景 + 开箱即用的模块组合 + 推荐参数」
import type { Stage } from './types';

export interface Params {
  rtt_ms: number;
  loss: number;
  mbps: number;
  burst_len: number;
  fec_rs: boolean;
  duration_s: number;
}

export interface DeployGuide {
  title: string;
  steps: string[];
}

export interface Preset {
  id: string;
  name: string;
  scene: string;          // 一句话：什么时候选它
  chain: string[];        // 模块链（大白话，按顺序）
  badge: '开箱即用' | '需要部署';
  params: Params;
  deploy?: DeployGuide;   // 真实环境需要的额外部署
}

export const PRESETS: Preset[] = [
  {
    id: 'lan',
    name: '方案 A · 家里局域网直连',
    scene: '两台电脑连同一个 WiFi / 路由器（帮家人修电脑最常见）',
    chain: ['找到对方（局域网广播）', 'QUIC 数据通道', 'XOR 轻纠错'],
    badge: '开箱即用',
    params: { rtt_ms: 2, loss: 0, mbps: 500, burst_len: 0, fec_rs: false, duration_s: 10 },
  },
  {
    id: 'wan',
    name: '方案 B · 跨网帮家人（异地）',
    scene: '两地在不同城市，网络都正常（你在公司，爸妈在家）',
    chain: ['STUN 穿墙直连', 'QUIC 数据通道', 'RS 纠错（Moonlight 同款）', '备用中转（DERP）'],
    badge: '开箱即用',
    params: { rtt_ms: 80, loss: 0.01, mbps: 20, burst_len: 0, fec_rs: true, duration_s: 10 },
    deploy: {
      title: '真实跨网需要部署中转服务器（DERP）',
      steps: [
        '准备一台香港/新加坡 VPS（国内可达，实测延迟 <100ms 为合格）',
        '按 switchboard/deploy/derper/README.md 执行（约 30 分钟）：docker compose 起headscale + derper',
        '两台电脑执行 tailscale up --login-server https://<你的域名> 接入',
        '验证：tailscale netcheck 能看到自建节点；跨网延迟从 1.2 秒降到 0.1 秒左右',
      ],
    },
  },
  {
    id: 'weak',
    name: '方案 C · 弱网救急',
    scene: '酒店 WiFi / 信号差的房间：网络拥挤、经常丢包',
    chain: ['STUN 穿墙直连', 'QUIC 数据通道', 'RS 纠错（高保护档）'],
    badge: '开箱即用',
    params: { rtt_ms: 120, loss: 0.05, mbps: 6, burst_len: 3, fec_rs: true, duration_s: 10 },
  },
  {
    id: 'extreme',
    name: '方案 D · 极限弱网',
    scene: 'UDP 被限速的恶劣网络：看这套方案能撑到什么程度',
    chain: ['STUN 穿墙直连', 'QUIC 数据通道', 'RS 纠错（高保护档）'],
    badge: '开箱即用',
    params: { rtt_ms: 200, loss: 0.08, mbps: 3, burst_len: 5, fec_rs: true, duration_s: 12 },
  },
];

/// 高级编排下拉框的人话标签（按模块 id）
export const ID_LABEL: Record<string, string> = {
  'sb.core.loopback': '内存模拟（本机演示用）',
  'sb.core.lossy-sim': '弱网模拟器（调丢包/延迟）',
  'sb.lan.udp-broadcast': '局域网广播找设备',
  'sb.rustdesk.punch-v2': 'RustDesk 打洞（需会合服务器）',
  'sb.stun.punch-v2': 'STUN 打洞（标准协议）',
  'sb.webrtc.ice': 'WebRTC ICE 打洞（未实现）',
  'sb.derp.self-hk': '自建 DERP 中转（需 VPS）',
  'sb.quinn.quic': 'QUIC 传输（推荐）',
  'sb.kcp.kcp': 'KCP 快速直连',
  'sb.srt.srt': 'SRT 广电级传输（需安装）',
  'sb.ext.tailscale': 'Tailscale 整套组网（需安装登录）',
  'sb.core.fec-xor': 'XOR 轻纠错（省带宽）',
  'sb.moonlight.fec-rs': 'RS 纠错（Moonlight 同款，抗丢包强）',
  'sb.qos.baseline-adaptive': '智能码率调节（未实现）',
};

/// 需要用户额外部署的模块 → 部署指引
export const DEPLOY_GUIDE: Record<string, { title: string; steps: string[] }> = {
  'sb.derp.self-hk': {
    title: '自建 DERP 中转服务器（需一台 VPS）',
    steps: [
      '准备一台香港/新加坡 VPS（国内可达，延迟 <100ms 为合格）',
      '把 switchboard/deploy/derper/ 目录传上 VPS，替换模板里的域名和 IP',
      '在 VPS 上执行 docker compose up -d',
      '在 headscale 里创建用户和接入密钥（README 有命令）',
      '两台电脑执行 tailscale up --login-server https://<你的域名>',
      '验证：tailscale netcheck 能看到自建节点',
    ],
  },
  'sb.rustdesk.punch-v2': {
    title: 'RustDesk 打洞（需要一台会合服务器 hbbs）',
    steps: [
      '会合服务器负责让两台电脑互相知道对方的地址',
      '可复用自建 DERP 套件里的 headscale 思路，或直接用 RustDesk 官方 hbbs',
      '没有会合服务器时，本模块会自动跳过，走备选中转',
    ],
  },
  'sb.ext.tailscale': {
    title: 'Tailscale 整套组网（需要安装并登录）',
    steps: [
      '两台电脑都安装 Tailscale 客户端（官网下载）',
      '登录同一账号（或自建 headscale 的账号）',
      '验证：tailscale status 能看到对方设备',
    ],
  },
  'sb.srt.srt': {
    title: 'SRT 传输（需要安装 libsrt）',
    steps: ['从 SRT Alliance 官网下载对应平台的库', '以外部进程方式启动 srt-listener 后填入地址'],
  },
};


/// 每个模块的「作用 / 影响 / 何时用」（高级编排详情与引导用）
export const MODULE_INFO: Record<
  string,
  { role: string; impact: string; when: string }
> = {
  'sb.lan.udp-broadcast': {
    role: '在局域网里喊一嗓子，自动发现要控制的电脑，免手填 IP',
    impact: '只在同一 WiFi/路由器下有效；对公网连接没有作用',
    when: '两台设备在同一 WiFi/路由器下时使用',
  },
  'sb.rustdesk.punch-v2': {
    role: '在两边路由器上打洞，建立点对点直连（不经过服务器）',
    impact: '打洞成功=延迟最低；失败自动转中转。需要一台会合服务器帮忙介绍',
    when: '跨网且两边网络都允许 UDP 时使用',
  },
  'sb.stun.punch-v2': {
    role: '标准协议版打洞（用标准 STUN 探测，兼容性最好）',
    impact: '同打洞；已实现五条打洞规则并有测试覆盖',
    when: '跨网直连首选；需要一台 STUN 服务器（公共的可用）',
  },
  'sb.webrtc.ice': {
    role: 'WebRTC 标准打洞（浏览器也能用）',
    impact: '未实现（L-A 研究桩）；执行时会自动跳过',
    when: '要做网页版控制端时再启用',
  },
  'sb.derp.self-hk': {
    role: '中转服务器：直连打不通时，数据经它转发',
    impact: '延迟取决于服务器位置——放香港约 50~150ms；需要一台 VPS 部署（有指引）',
    when: '跨网且打洞可能失败时，作为兜底必配',
  },
  'sb.quinn.quic': {
    role: 'QUIC 传输：加密、多路复用、抗丢包，综合最均衡',
    impact: '延迟低、可靠；带宽利用受拥塞算法影响（可切 BBR 实验）',
    when: '绝大多数场景的默认选择',
  },
  'sb.kcp.kcp': {
    role: 'KCP 快速直连：牺牲部分带宽换更低延迟（游戏同款）',
    impact: '延迟更低但更耗带宽；可靠语义与 QUIC 略不同',
    when: '对延迟极敏感、带宽充足的局域网/优质线路',
  },
  'sb.srt.srt': {
    role: 'SRT 广电级传输：抗丢包极强，延迟较高',
    impact: '未实现（L-A）；极差公网下可能比 QUIC 更稳',
    when: '公网极差环境时作为对比实验',
  },
  'sb.ext.tailscale': {
    role: 'Tailscale 整套组网：打洞+中转+加密全托管',
    impact: '最省心，但官方中转在国内可能劣化（可自建 DERP 解决）',
    when: '不想自己维护服务器时使用',
  },
  'sb.core.loopback': {
    role: '内存模拟通道：本机自己连自己，用于演示和测试',
    impact: '不经过任何真实网络，速度=内存速度',
    when: '演示评分、验证管线时使用',
  },
  'sb.core.lossy-sim': {
    role: '弱网模拟器：给通道加延迟/丢包/限速，模拟真实网络',
    impact: '参数越差分数越低——用于横向对比方案',
    when: '测试和演示时使用',
  },
  'sb.core.fec-xor': {
    role: 'XOR 轻纠错：每 8 片加 1 片校验，省带宽',
    impact: '省 12% 带宽；但连续丢 2 片以上就救不回（弱网偏弱）',
    when: '网络较好、想省带宽时',
  },
  'sb.moonlight.fec-rs': {
    role: 'RS 纠错（Moonlight 同款）：纠错能力随比例可调，最高 200%',
    impact: '多花最高 20% 带宽，换强抗丢包；弱网下明显更稳',
    when: '跨网/弱网场景推荐',
  },
  'sb.qos.baseline-adaptive': {
    role: '智能码率调节：根据延迟/丢包自动升降清晰度',
    impact: '未实现（L-A）；计划替换 rd 现有 AIMD',
    when: '与任意传输/媒体组合使用',
  },
};

/// 高级编排的方案模板（一键铺满画布）
export const TEMPLATES: {
  name: string;
  desc: string;
  chain: { stage: Stage; moduleId: string }[];
}[] = [
  {
    name: '模板 1 · 家里局域网直连（推荐新手）',
    desc: '同一 WiFi 下自动找到对方，QUIC 直连，XOR 轻纠错',
    chain: [
      { stage: 'discovery', moduleId: 'sb.lan.udp-broadcast' },
      { stage: 'transport', moduleId: 'sb.quinn.quic' },
      { stage: 'media', moduleId: 'sb.core.fec-xor' },
    ],
  },
  {
    name: '模板 2 · 跨网帮家人（打洞+中转兜底）',
    desc: '先尝试穿墙直连，失败自动走自建中转；RS 纠错保画质',
    chain: [
      { stage: 'punch', moduleId: 'sb.stun.punch-v2' },
      { stage: 'relay', moduleId: 'sb.derp.self-hk' },
      { stage: 'transport', moduleId: 'sb.quinn.quic' },
      { stage: 'media', moduleId: 'sb.moonlight.fec-rs' },
    ],
  },
  {
    name: '模板 3 · 弱网救急（RS 高保护）',
    desc: '酒店/信号差：RS 纠错扛丢包，码率自动收敛',
    chain: [
      { stage: 'punch', moduleId: 'sb.stun.punch-v2' },
      { stage: 'transport', moduleId: 'sb.quinn.quic' },
      { stage: 'media', moduleId: 'sb.moonlight.fec-rs' },
    ],
  },
  {
    name: '模板 4 · Tailscale 全托管（最省心）',
    desc: '安装 Tailscale 后整套组网由它负责，rd 只管画面',
    chain: [{ stage: 'transport', moduleId: 'sb.ext.tailscale' }],
  },
  {
    name: '模板 5 · KCP 电竞低延迟（局域网）',
    desc: '牺牲带宽换极低延迟，适合游戏级操作',
    chain: [
      { stage: 'discovery', moduleId: 'sb.lan.udp-broadcast' },
      { stage: 'transport', moduleId: 'sb.kcp.kcp' },
      { stage: 'media', moduleId: 'sb.core.fec-xor' },
    ],
  },
  {
    name: '模板 6 · 内网纯演示（无任何外部依赖）',
    desc: '全内存通道，用于演示打分，不碰真实网络',
    chain: [{ stage: 'transport', moduleId: 'sb.core.loopback' }],
  },
];
