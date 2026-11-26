/**
 * demo 预览数据（仅浏览器环境）：模拟已配对多设备的使用场景，供 UX 评审
 * 截图使用；Tauri 桌面运行时不经过此文件。
 */

/** 生成一枚圆角字母图标（SVG data URI）。 */
function icon(color: string, letter: string): string {
  const svg =
    `<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32">` +
    `<defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1">` +
    `<stop offset="0" stop-color="${color}"/><stop offset="1" stop-color="${color}" stop-opacity="0.72"/>` +
    `</linearGradient></defs>` +
    `<rect width="32" height="32" rx="7" fill="url(#g)"/>` +
    `<rect x="0.5" y="0.5" width="31" height="31" rx="6.5" fill="none" stroke="#ffffff2e"/>` +
    `<text x="16" y="22" font-size="15" font-family="Segoe UI, sans-serif" fill="#fff" text-anchor="middle" font-weight="600">${letter}</text></svg>`;
  return `data:image/svg+xml,${encodeURIComponent(svg)}`;
}

/** 已配对设备（DeviceDto）。 */
const DEVICES = [
  {
    id: "dev-studio",
    name: "工作室工作站",
    addr: "100.101.43.7:21118",
    fingerprintHex: "a3f1c90b7e22d4f8c05a6b1d3e7f9a2b4c6d8e0f2a4b6c8d0e2f4a6",
    addedAt: 1761500000,
    online: true,
    macAddrs: ["AA:BB:CC:DD:EE:01"],
  },
  {
    id: "dev-htpc",
    name: "客厅 HTPC",
    addr: "127.0.0.1:21118",
    fingerprintHex: "b2e0d8c6a4f2b0d8c6a4f2b0d8c6a4f2b0d8c6a4f2b0d8c6a4f2b0d8c6",
    addedAt: 1761200000,
    online: true,
    macAddrs: ["AA:BB:CC:DD:EE:02"],
  },
  {
    id: "dev-office",
    name: "办公台式机",
    addr: "192.168.1.66:21118",
    fingerprintHex: "c1d3e5f7a9b1c3d5e7f9a1b3c5d7e9f1a3c5e7f9b1d3f5a7c9e1b3d5f7",
    addedAt: 1760900000,
    online: false,
    macAddrs: ["AA:BB:CC:DD:EE:03"],
  },
];

/** 可捕获窗口（WindowDto）：坐标模拟真实桌面错落布局（1920×1080 基准，
 * 桌面地图按几何摆放——曾全屏堆叠使地图退化）。 */
const WINDOWS = [
  { id: "3301", title: "main.ts - rd-controller - Visual Studio Code", process: "Code.exe", pid: 4021, icon: icon("#2f9e8f", "V"), x: 0, y: 0, w: 1280, h: 800, dpi: 1.0, resizable: true, minimized: false },
  { id: "3302", title: "rd - 窗口级远程控制 - 汇报材料.docx - Word", process: "WINWORD.EXE", pid: 8812, icon: icon("#2b579a", "W"), x: 1320, y: 0, w: 600, h: 900, dpi: 1.0, resizable: true, minimized: false },
  { id: "3303", title: "Q3 营收报表.xlsx - Excel", process: "EXCEL.EXE", pid: 7310, icon: icon("#217346", "X"), x: 0, y: 840, w: 1180, h: 240, dpi: 1.0, resizable: true, minimized: false },
  { id: "3304", title: "产品研发一群 - 企业微信", process: "WXWork.exe", pid: 9912, icon: icon("#3d7eff", "企"), x: 1960, y: 120, w: 480, h: 800, dpi: 1.0, resizable: true, minimized: false },
  { id: "3305", title: "rd-final.log - 记事本", process: "Notepad.exe", pid: 6230, icon: icon("#767676", "N"), x: 1320, y: 940, w: 560, h: 140, dpi: 1.0, resizable: true, minimized: false },
  { id: "3306", title: "Windows Terminal", process: "WindowsTerminal.exe", pid: 4402, icon: icon("#4d4d4d", ">"), x: 1220, y: 320, w: 700, h: 560, dpi: 1.0, resizable: true, minimized: false },
  { id: "3307", title: "New Tab - Chrome", process: "chrome.exe", pid: 2210, icon: icon("#4285f4", "C"), x: 2400, y: 500, w: 640, h: 520, dpi: 1.0, resizable: true, minimized: false },
  { id: "3308", title: "OBS 31.0 - 直播推流中", process: "obs64.exe", pid: 5511, icon: icon("#1f1f23", "O"), x: 120, y: 180, w: 900, h: 560, dpi: 1.0, resizable: true, minimized: false },
  { id: "3309", title: "Steam", process: "steam.exe", pid: 3310, icon: icon("#171a21", "S"), x: 1500, y: 200, w: 800, h: 500, dpi: 1.0, resizable: true, minimized: true },
  { id: "3310", title: "Postman", process: "Postman.exe", pid: 8899, icon: icon("#ff6c37", "P"), x: 300, y: 300, w: 760, h: 500, dpi: 1.0, resizable: true, minimized: true },
];

const HOST_STATUS = {
  listenAddrs: ["100.101.43.7:21118", "192.168.1.10:21118"],
  peers: [] as { name: string; sinceSecs: number }[],
  paused: false,
};

const PAIR_CARD = {
  code: "482916",
  addrs: ["100.101.43.7:21118", "192.168.1.10:21118"],
  paused: false,
};

/** 命令表：命令名 → 演示数据（80ms 延迟模拟 IPC 手感）。 */
export async function mockInvoke<T>(
  cmd: string,
  _args?: Record<string, unknown>,
): Promise<T> {
  // 订阅/单向类命令：demo 无推送源，直接 resolve null（前端 unlisten?.()/
  // .catch 均可安全消化）——不进 TABLE，避免「暂不支持」误伤正常预览路径。
  const ONE_WAY = new Set([
    "subscribe_vdisplay_state",
    "subscribe_window_events",
    "subscribe_session_state",
    "subscribe_group_events",
    "subscribe_session_events",
    "touch_gesture",
    "set_display_width",
    "send_clipboard",
  ]);
  if (ONE_WAY.has(cmd)) {
    await new Promise((r) => setTimeout(r, 80));
    return null as T;
  }
  const TABLE: Record<string, unknown> = {
    list_devices: DEVICES,
    discover_devices: [
      { name: "书房笔记本", addr: "192.168.1.34:21118", fingerprintShortHex: "9f3a" },
      { name: "小米电视助手 PC", addr: "192.168.1.77:21118", fingerprintShortHex: "2c81" },
    ],
    host_status: HOST_STATUS,
    generate_pair_code: PAIR_CARD,
    get_media_fec: false,
    set_media_fec: true,
    get_license_status: { plan: "free", name: "", devicesMax: 2 },
    set_host_paused: true,
    probe_device: true,
    connect_device: { protoVer: 3, hostName: "工作室工作站", encodeCaps: ["h264", "tiles"], hostVersion: "0.1.0" },
    pair_device: DEVICES[0],
    edit_device: DEVICES[0],
    list_windows: WINDOWS,
    // demo 无真实系统剪贴板通道：返回演示文本让正向流程走通（R12 排错#5：
    // 曾恒返空串，把「demo 无通道」错报成「你的剪贴板是空的」甩锅用户）
    local_clipboard_read: "（demo 演示文本：桌面端可粘贴真实剪贴板）",
    local_clipboard_write: true,
    // T-031：文件传输列目录演示（推送/拉取在 api 层模拟，不经此表）
    file_list: {
      path: "C:\\Users\\demo",
      entries: [
        { name: "Desktop", dir: true, size: 0 },
        { name: "Documents", dir: true, size: 0 },
        { name: "Downloads", dir: true, size: 0 },
        { name: "report.pdf", dir: false, size: 1024 * 1024 },
        { name: "setup.exe", dir: false, size: 48 * 1024 * 1024 },
      ],
    },
    lab_get_state: {
      activeDeviceId: "dev-studio",
      activeAddr: "100.101.43.7:21118",
      routeTag: "direct (192.168.1.10:21118)",
      congestion: "bbr",
      mediaFec: true,
      fecOverheadPct: 20,
      fecRecovered: 14,
      fecLost: 0,
      rttMs: 12,
      quicCwndKb: 256,
      quicSentPkts: 18420,
      quicLostPkts: 23,
      encoderName: "h264_nvenc",
      hardwareEnc: true,
      is444: false,
      targetKbps: 8000,
      videoKbps: 4200,
      tileKbps: 380,
      fpsCap: 60,
      tileStillMs: 300,
      tileBudgetPct: 50,
      tilesSent: 128,
      idrCount: 4,
      netem: {
        enabled: false,
        delayMs: 0,
        jitterMs: 0,
        lossPct: 0,
        burstLen: 1,
        rateKbps: 0,
      },
      netemDropped: 0,
      netemDelayed: 0,
      sandboxRunning: true,
      sandboxPsnrDb: 46.8,
      sandboxTextFidelityPct: 99.9,
    },
    lab_apply_tuning: null,
    lab_apply_transport: "已热切至 [DIRECT / BBR / RS-FEC +20%]",
    lab_set_netem: {
      enabled: true,
      delayMs: 50,
      jitterMs: 10,
      lossPct: 3,
      burstLen: 2,
      rateKbps: 12000,
    },
    lab_panic_reset: null,
    lab_start_sandbox: null,
    lab_stop_sandbox: null,
    lab_list_profiles: [
      {
        id: "geek-lan-444",
        name: "⚡ 极客 LAN · 1:1 像素零损",
        boundDeviceId: null,
        transport: { routeMode: "direct", congestion: "bbr", mediaFec: false, fecOverheadPct: 15 },
        codec: {
          manualLock: true,
          bitrateKbps: 20000,
          maxFps: 60,
          preferHw: true,
          request444: true,
          tileEnabled: true,
          tileStillMs: 120,
          tileBudgetPct: 65,
        },
      },
      {
        id: "cross-wan-rs",
        name: "🛡️ 跨网抗丢包 · BBR + RS-FEC 25%",
        boundDeviceId: null,
        transport: { routeMode: "punch", congestion: "bbr", mediaFec: true, fecOverheadPct: 25 },
        codec: {
          manualLock: false,
          bitrateKbps: 8000,
          maxFps: 60,
          preferHw: true,
          request444: false,
          tileEnabled: true,
          tileStillMs: 250,
          tileBudgetPct: 50,
        },
      },
    ],
    lab_save_profile: [],
    lab_delete_profile: [],
    lab_fetch_public_nodes: {
      nodes: [
        {
          name: "小米公共 STUN",
          kind: "stun",
          addr: "stun.miwifi.com:3478",
          region: "国内",
          source: "精选国内大厂",
          note: "国内低延迟 UDP 打洞（零流量经过）",
        },
        {
          name: "B站公共 STUN",
          kind: "stun",
          addr: "stun.chat.bilibili.com:3478",
          region: "国内",
          source: "精选国内大厂",
          note: "国内低延迟 UDP 打洞",
        },
        {
          name: "Cloudflare 全球 STUN",
          kind: "stun",
          addr: "stun.cloudflare.com:3478",
          region: "全球 Anycast",
          source: "Cloudflare",
          note: "跨境/出海网络高可用打洞",
        },
        {
          name: "EasyTier 社区共享节点",
          kind: "easytier",
          addr: "tcp://public.easytier.top:11010",
          region: "社区共享",
          source: "EasyTier 社区源",
          note: "公益组网/打洞引导节点（加密流量）",
        },
      ],
      fetchedOnlineCount: 2,
      logs: [
        "已载入 3 个内置国内/全球高速 STUN 节点",
        "✓ 预览模式示例：已同步社区公共打洞与 EasyTier 节点",
      ],
    },
    lab_probe_node: {
      addr: "stun.miwifi.com:3478",
      kind: "stun",
      ok: true,
      rttMs: 24.6,
      mappedAddr: "112.80.248.75:54321",
      detail: "STUN 可达 24.6ms · 公网映射 112.80.248.75:54321",
    },
    easytier_detect_status: {
      installed: true,
      binaryPath: "C:\\tools\\easytier-core.exe",
      enabled: false,
      networkName: "",
      virtualIp: "",
      peerUrls: [],
      inviteCode: null,
      peers: [],
    },
    easytier_generate_invite: {
      ok: true,
      inviteCode: "rd-et://eyJuIjoicmQtbWVzaC0xYTJiM2MiLCJzIjoiZGVtb3NlY3JldDEyMzQ1NiIsInAiOlsidGNwOi8vcHVibGljLmVhc3l0aWVyLnRvcDoxMTAxMCJdfQ",
      networkName: "rd-mesh-1a2b3c",
      peerUrls: ["tcp://public.easytier.top:11010"],
      binaryFound: true,
      binaryPath: "C:\\tools\\easytier-core.exe",
      hint: "已生成组网口令！复制发给对方粘贴加入，双方重启后即可通过 10.126.x.x 低延迟直连。",
    },
    easytier_join_invite: {
      ok: true,
      inviteCode: "rd-et://demo",
      networkName: "rd-mesh-1a2b3c",
      peerUrls: ["tcp://public.easytier.top:11010"],
      binaryFound: true,
      binaryPath: "C:\\tools\\easytier-core.exe",
      hint: "已加入组网「rd-mesh-1a2b3c」！重启后自动分配 10.126.x.x 内网 IP。",
    },
    easytier_auto_install: {
      ok: true,
      version: "v2.2.4",
      binaryPath: "C:\\tools\\easytier-core.exe",
      toolsDir: "C:\\tools",
      detail: "已自动下载并安装 EasyTier v2.2.4 到 C:\\tools！",
    },
    lab_diagnose_nat: {
      natType: "cone",
      label: "圆锥型 NAT（极易 P2P 打洞直连）",
      advice: "两次探测公网端口完全一致！使用「EasyTier 组网」或「自建中转 (方案 A · 9位ID)」均可直接打洞实现 20~50ms P2P 直连。",
      mappedA: "112.80.248.75:54321",
      mappedB: "112.80.248.75:54321",
      serverA: "stun.miwifi.com:3478",
      serverB: "stun.chat.bilibili.com:3478",
      rttAMs: 22.4,
      rttBMs: 26.1,
    },
    relay_id_status: {
      numericId: "482 916 305",
      rawId: 482916305,
      relayAddr: null,
      fingerprintShort: "710fedcefbe23d73",
    },
    relay_resolve_id: {
      ok: true,
      online: true,
      numericId: "482 916 305",
      rttMs: 28.5,
      peerFingerprintShort: "710fedcefbe23d73",
      detail: "对方 ID 在线！中转延迟 28.5ms · 指纹 710fedcefbe23d73（支持 P2P 打洞 + 中转兜底）",
    },
    connect_by_relay_id: {
      ok: true,
      needPairCode: false,
      device: {
        id: "dev-studio",
        name: "远程设备 482 916 305",
        addr: "198.51.100.10:21118",
        fingerprintHex: "710fedcefbe23d73".repeat(4),
        addedAt: 1700000000,
        online: true,
        macAddrs: [],
        relay: "198.51.100.10:21118",
      },
      routeTag: "P2P 打洞直连命中",
      detail: "已通过 P2P 打洞直连命中 连接成功！",
    },
    lab_connect_direct_or_mesh: {
      ok: true,
      needPairCode: false,
      device: {
        id: "dev-studio",
        name: "组网远程主机",
        addr: "10.126.126.2:21118",
        fingerprintHex: "710fedcefbe23d73".repeat(4),
        addedAt: 1700000000,
        online: true,
        macAddrs: [],
        relay: null,
      },
      routeTag: "direct (10.126.126.2:21118)",
      detail: "连接成功，正在进入远程桌面…",
    },
    lab_p2p_host_open: {
      ok: true,
      token: "rd-p2p://eyJtIjoiMTEyLjgwLjI0OC43NTo1NDMyMSIsImMiOlsiMTkyLjE2OC4xLjg6MjExMTgiXSwicCI6IjY1NDMyMSIsInMiOiJzdHVuLm1pd2lmaS5jb206MzQ3OCIsIm4iOiJTdHVkaW8tUEMifQ",
      mappedAddr: "112.80.248.75:54321",
      natType: "cone",
      natLabel: "圆锥型 NAT（支持 STUN 打洞直连）",
      pairCode: "654321",
      stunUsed: "stun.miwifi.com:3478",
      candidates: ["192.168.1.8:21118"],
      advice: "已锁定本机公网洞口并持续保活！把下方打洞连接码发给对方粘贴即可直连。",
    },
    lab_p2p_host_punch_peer:
      "已向控制端发射双向开洞脉冲（持续 6 秒）！现在立刻让对方点击「完成打洞并连接桌面」即可秒通！",
    lab_p2p_connect_token: {
      ok: true,
      needReversePunch: false,
      needPairCode: false,
      reverseCode: "rd-punch://112.80.248.99:51234",
      device: {
        id: "dev-studio",
        name: "Studio-PC",
        addr: "112.80.248.75:54321",
        fingerprintHex: "710fedcefbe23d73".repeat(4),
        addedAt: 1700000000,
        online: true,
        macAddrs: [],
        relay: null,
      },
      routeTag: "STUN P2P 打洞直连 (112.80.248.75:54321)",
      detail: "STUN P2P 打洞直连成功！正在进入完整远程桌面…",
      ctrlNatType: "cone",
      hostNatType: "cone",
      txPulses: 4,
      rxPulses: 1,
      attemptCount: 1,
    },
    lab_p2p_check_status: {
      active: false,
      reverseCode: "",
      ctrlNatType: "cone",
      txPulses: 0,
      rxPulses: 0,
      punchedPeer: null,
    },
    lab_export_diagnostic_log: "=== RD 远程控制 · 实验室排障诊断快照 (Web Demo) ===\n状态正常\n",
    lab_scan_mesh_peers: [
      {
        source: "EasyTier",
        name: "Office-Workstation",
        addr: "10.126.126.2",
        latencyMs: 18.4,
        cost: "P2P",
      },
      {
        source: "Tailscale",
        name: "MacBook-Pro",
        addr: "100.81.205.10",
        latencyMs: 25.0,
        cost: "Tailnet",
      },
    ],
    lab_run_ab_bench: {
      netemSummary: "Netem 开启：+60ms (±10ms) · 丢包 4.0% (Burst 3)",
      runA: {
        label: "方案 A · QUIC Stream",
        encoderName: "h264_nvenc",
        avgFps: 42.5,
        p99FrameMs: 148.0,
        videoKbps: 6200,
        tileKbps: 410,
        fecRecovered: 0,
        framesDropped: 5,
        psnrDb: 38.4,
        textFidelityPct: 91.2,
      },
      runB: {
        label: "方案 B · RS-FEC 25% + BBR",
        encoderName: "h264_nvenc",
        avgFps: 59.4,
        p99FrameMs: 68.0,
        videoKbps: 6400,
        tileKbps: 480,
        fecRecovered: 11,
        framesDropped: 0,
        psnrDb: 47.2,
        textFidelityPct: 99.8,
      },
      winner: "方案 B · RS-FEC 25% + BBR",
      verdict: "方案 B 胜出：文字保真度 99.8% (PSNR 47.2dB)，P99 帧延迟 68.0ms，救回 11 帧",
    },
  };
  await new Promise((r) => setTimeout(r, 80));
  if (!(cmd in TABLE)) {
    // 未收录命令给人话（R11 D06——曾抛 window.__TAURI_INTERNALS__ 黑话）
    throw new Error(`demo 预览模式暂不支持「${cmd}」，请在桌面版中使用`);
  }
  return structuredClone(TABLE[cmd]) as T;
}