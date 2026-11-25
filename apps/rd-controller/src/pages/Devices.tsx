/**
 * 设备页（T-019）：设备卡片 + 「添加设备」对话框 + 局域网扫描（D-013 包1：
 * 免手填 IP）+ 指纹不匹配一键重配对 + Tailscale 节点共享引导。
 */
import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  APP_VERSION,
  clearLastDeviceId,
  lastConnectedAt,
  relativeTime,
  connectDevice,
  disconnectDevice,
  discoverDevices,
  editDevice,
  generatePairCode,
  getHostVersion,
  getLastDeviceId,
  getRelayAddr,
  hostStatus,
  tailscalePing,
  type HostStatusDto,
  type PairCodeDto,
  listDevices,
  pairDevice,
  probeDevice,
  rememberLastDeviceId,
  rememberHostVersion,
  removeDevice,
  wakeDevice,
  type DeviceDto,
  type DiscoveredDeviceDto,
} from "../api/devices";
import { call, listenStub } from "../api/bridge";
import { useDismiss } from "../ui/useDismiss";

import { IconChevronDown, IconChevronUp, IconLab, IconMore, IconPlus, IconScan } from "../ui/Icon";
import styles from "./Devices.module.css";

/** 把 Error/unknown 统一成消息文本。 */
function errText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** 本机被控展开层记忆键（低频配置不占主线空间，默认收起）。 */
const HOST_DETAILS_KEY = "rd.hostDetailsOpen";

/** 读展开层记忆；存储不可用按收起处理。 */
function readHostDetailsOpen(): boolean {
  try {
    return window.localStorage.getItem(HOST_DETAILS_KEY) === "1";
  } catch {
    return false;
  }
}

/** 被控时长文案（秒 → 小时/分钟/秒，与原 peer 徽标口径一致）。 */
function hostDuration(secs: number): string {
  if (secs >= 3600) {
    return `${Math.floor(secs / 3600)} 小时`;
  }
  if (secs >= 60) {
    return `${Math.floor(secs / 60)} 分钟`;
  }
  return `${secs} 秒`;
}

/** 完整桌面（主显示器）虚拟 window_id：0xFFFF_0000_0000_0000，点击设备默认一步直入整屏。 */
const DESKTOP_PRIMARY_ID = "18446462598732840960";

/** 根据本机地址/指纹派生 RustDesk 风格 9 位数字设备 ID（开启中转后用于跨网 ID 直连）。 */
function deriveNineDigitId(seed: string): string {
  let h = 2166136261;
  for (let i = 0; i < seed.length; i += 1) {
    h ^= seed.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  const num = (Math.abs(h) % 900_000_000) + 100_000_000;
  const s = String(num);
  return `${s.slice(0, 3)} ${s.slice(3, 6)} ${s.slice(6, 9)}`;
}

/** 展示地址：默认端口 21118 自动隐藏，仅保留非默认自定义端口，减少视觉噪声。 */
function formatDisplayAddr(addr: string): string {
  return addr.endsWith(":21118") ? addr.slice(0, -":21118".length) : addr;
}

/**
 * 设备页（#/devices）。
 *
 * 点击设备卡片 → connect_device → 跳转 #/windows/:deviceId（T-020 页面）。
 */
export function Devices() {
  const [devices, setDevices] = useState<DeviceDto[]>([]);
  const [error, setError] = useState<string | null>(null);
  /** 信息性消息（探测结果、删除成功等），与错误区分展示。 */
  const [notice, setNotice] = useState<string | null>(null);
  /** 正在操作的设备 id（禁用按钮防重复点击）。 */
  const [busyId, setBusyId] = useState<string | null>(null);
  const [showAdd, setShowAdd] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [pairing, setPairing] = useState(false);
  const [form, setForm] = useState({ addr: "", code: "", name: "", relay: "" });
  /** 局域网扫描结果（发现 ≠ 信任，点击仅预填添加对话框）。 */
  const [discovered, setDiscovered] = useState<DiscoveredDeviceDto[] | null>(null);
  const [scanning, setScanning] = useState(false);
  /** fec 高清链路开关（settings.json 持久化；下次连接生效）。 */
  const [mediaFec, setMediaFec] = useState(false);
  /** 已配置的自建中转地址（非空时启用 RustDesk 风格 9 位 ID 连接模式）。 */
  const [relayAddr, setRelayAddr] = useState<string>("");
  /** 本机被控状态（融合版：本机也是被控机）。 */
  const [host, setHost] = useState<HostStatusDto | null>(null);
  /** 被控状态查询失败（P0-2②：整卡消失 ≠ 功能不存在，降级态要说明）。 */
  const [hostDown, setHostDown] = useState(false);
  /** 配对码卡（null = 未生成）。 */
  const [pairCard, setPairCard] = useState<PairCodeDto | null>(null);
  /** 配对码剩余秒数（5 分钟有效；归零置灰提示重新生成）。 */
  const [pairRemainSecs, setPairRemainSecs] = useState(0);
  const pairGeneratedAtRef = useRef(0);
  useEffect(() => {
    if (pairCard === null) {
      return undefined;
    }
    const tick = () => {
      const left = 300 - Math.floor((Date.now() - pairGeneratedAtRef.current) / 1000);
      setPairRemainSecs(Math.max(0, left));
    };
    tick();
    const iv = window.setInterval(tick, 1000);
    return () => window.clearInterval(iv);
  }, [pairCard]);
  /** 正在编辑的设备（null = 添加模式）。 */
  const [editing, setEditing] = useState<DeviceDto | null>(null);
  /** 链路测试进行中的设备 id 与结果文本。 */
  const [linkTest, setLinkTest] = useState<{ id: string; text: string } | null>(null);
  /** 待确认删除的设备 id（二次确认层：按钮原位变「确认删除」）。 */
  const [confirmRemoveId, setConfirmRemoveId] = useState<string | null>(null);
  /** 确认钮就绪时刻（+400ms 才可点，防双击穿透误删——Jenson R2）。 */
  const [confirmArmed, setConfirmArmed] = useState(false);
  useEffect(() => {
    if (confirmRemoveId === null) {
      setConfirmArmed(false);
      return undefined;
    }
    const t = window.setTimeout(() => setConfirmArmed(true), 400);
    return () => window.clearTimeout(t);
  }, [confirmRemoveId]);
  /** 连接帮助折叠显隐。 */
  const [helpOpen, setHelpOpen] = useState(false);
  /** 本机被控展开层（默认收起，localStorage 记忆——低频配置不占主线空间）。 */
  const [hostExpanded, setHostExpanded] = useState(readHostDetailsOpen);
  /** 展开/收起 aria-live 播报（空串 = 无待播报内容）。 */
  const [hostLiveMsg, setHostLiveMsg] = useState("");
  const toggleHostExpanded = useCallback(() => {
    const next = !hostExpanded;
    setHostExpanded(next);
    try {
      window.localStorage.setItem(HOST_DETAILS_KEY, next ? "1" : "0");
    } catch {
      // 存储不可用：仅失去展开记忆，不影响功能
    }
    setHostLiveMsg(next ? "已展开本机被控详情" : "已收起本机被控详情");
  }, [hostExpanded]);
  /** 设备搜索（>3 台时出现；IT 管理员 R4）。 */
  const [deviceQuery, setDeviceQuery] = useState("");
  // `/` 聚焦设备搜索（与窗口页对齐——快捷键体系一致性）
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      const typing =
        target !== null &&
        (target.tagName === "INPUT" || target.tagName === "TEXTAREA");
      if (e.key === "/" && !typing) {
        e.preventDefault();
        document.querySelector<HTMLInputElement>('input[type="search"]')?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  /** 上次连接的设备（「继续上次」快达卡；null = 无历史）。 */
  const [resume, setResume] = useState<{ id: string; name: string; online: boolean } | null>(null);

  /** 打开添加/编辑对话框（记录触发点：effect 里再取已被 autoFocus 抢走）。 */
  const addTriggerRef = useRef<HTMLElement | null>(null);
  const openAddDialog = useCallback(
    (prefill?: { addr: string; name: string; relay?: string } | null, edit?: DeviceDto | null) => {
      addTriggerRef.current = document.activeElement as HTMLElement | null;
      if (prefill !== undefined && prefill !== null) {
        setForm({ addr: prefill.addr, code: "", name: prefill.name, relay: prefill.relay ?? "" });
        setFormError(null);
      }
      setEditing(edit ?? null);
      setShowAdd(true);
    },
    [],
  );

  const refresh = useCallback(async () => {
    try {
      const list = await listDevices();
      setDevices(list);
      const lastId = getLastDeviceId();
      if (lastId !== null) {
        const d = list.find((x) => x.id === lastId);
        setResume(d !== undefined ? { id: d.id, name: d.name, online: d.online } : null);
      } else {
        setResume(null);
      }
      setError(null);
    } catch (e) {
      setError(errText(e));
    }
  }, []);

  /** 被控状态查询（2s 轮询 + 「重新检查」共用）；查不到 → 降级态。 */
  const pollHost = useCallback(async () => {
    try {
      const status = await hostStatus();
      setHost(status);
      setHostDown(false);
    } catch {
      // 查询没有成功 = 被控服务不可用：不弹错误（页面其余功能不受影响），
      // 用降级卡片说明功能存在与可能原因（P0-2②——曾整卡静默消失）
      setHost(null);
      setHostDown(true);
    }
  }, []);

  useEffect(() => {
    void refresh();
    // fec 开关回显（settings.json）；经 bridge call()（demo 模式给人话，R11 D06）
    void call<boolean>("get_media_fec")
      .then(setMediaFec)
      .catch(() => undefined);
    void getRelayAddr()
      .then((r) => setRelayAddr(typeof r === "string" ? r.trim() : ""))
      .catch(() => undefined);
    // ConnManager 断线（含主动断开）广播 → 刷新在线圆点
    const unlisten = listenStub("device-disconnected", () => {
      void refresh();
    });
    // 被控状态 2s 轮询（地址/被谁连接/暂停态）
    void pollHost();
    const hostTimer = window.setInterval(() => {
      void pollHost();
    }, 2000);
    // 能自动就自动：进入设备页时后台静默扫描一次局域网，仅发现新设备时展示，空结果不打扰
    let cancelled = false;
    void discoverDevices()
      .then((found) => {
        if (!cancelled && Array.isArray(found) && found.length > 0) {
          setDiscovered(found);
        }
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
      void unlisten.then((fn) => fn());
      window.clearInterval(hostTimer);
    };
  }, [refresh, pollHost]);

  /** 紧急切断：暂停本机被控共享（对方画面/输入/终端全部停止）。
   * 经 bridge call()（R11 D06——曾裸 invoke 绕过 mock 切换，demo 下报黑话）。 */
  const [cutting, setCutting] = useState(false);
  const onCutPeers = useCallback(async () => {
    setCutting(true);
    try {
      await call<void>("set_host_paused", { paused: true });
      setNotice("已切断控制连接（对方已无法操作本机；在托盘可恢复共享）");
      await refresh();
    } catch (e) {
      setError(`切断失败：${errText(e)}`);
    } finally {
      setCutting(false);
    }
  }, [refresh]);

  /** 生成配对码（UI 卡片展示 + 自动复制；5 分钟倒计时）。 */
  const onGenerateCode = useCallback(async () => {
    try {
      const card = await generatePairCode();
      pairGeneratedAtRef.current = Date.now();
      setPairCard(card);
      try {
        await navigator.clipboard.writeText(card.code);
      } catch {
        // 剪贴板不可用：卡片上仍显示明文
      }
    } catch (e) {
      setError(errText(e));
    }
  }, []);

  const onConnect = useCallback(
    async (d: DeviceDto) => {
      setBusyId(d.id);
      setError(null);
      try {
        // 卡片显示在线时跳过 connect：后端 connect_device 对在线设备做连接
        // 替换（关旧连新），旧连接的断线监听会把新连接误清出连接表（conn.rs
        // put_device 竞态），导致随后的窗口列表报“设备未连接”
        if (!d.online) {
          const ack = await connectDevice(d.id);
          rememberHostVersion(d.id, ack.hostVersion);
        }
        rememberLastDeviceId(d.id);
        window.location.hash = `#/session/${d.id}?open=${DESKTOP_PRIMARY_ID}`;
        setNotice(null);
      } catch (e) {
        const msg = errText(e);
        // 指纹不匹配（对端重装/换证书）→ 一键重配对：预填地址打开添加对话框
        if (msg.includes("指纹不匹配")) {
          openAddDialog({ addr: formatDisplayAddr(d.addr), name: d.name });
          setFormError(
            "对端证书已变化：可能是对方重装了系统/rd-agent，也可能是网络被劫持。" +
              "若对方确认刚重装过，请对方在托盘点「生成配对码」，你再输入新码完成重新配对；" +
              "若对方没有重装，请勿输入配对码，先通过其他渠道核实对方身份。",
          );
        } else if (msg.includes("尚未信任") || msg.includes("未鉴权")) {
          openAddDialog({ addr: formatDisplayAddr(d.addr), name: d.name });
          setFormError(
            "对方电脑尚未信任本机（可能是对方重装或清空了信任列表）。请让对方点「生成配对码」，输入 6 位码重新配对即可连接。",
          );
        } else {
          setError(msg);
        }
        await refresh();
      } finally {
        setBusyId(null);
      }
    },
    [refresh],
  );

  /** 删除设备（仅经二次确认调用；删除后清导航记忆）。 */
  const onRemove = useCallback(
    async (d: DeviceDto) => {
      setBusyId(d.id);
      try {
        await removeDevice(d.id);
        // 删除的是“最后连接的设备”时清除导航记忆，避免快捷入口指向空设备
        if (getLastDeviceId() === d.id) {
          clearLastDeviceId();
        }
        setNotice(`已删除设备「${d.name}」`);
        await refresh();
      } catch (e) {
        setError(errText(e));
      } finally {
        setBusyId(null);
        setConfirmRemoveId(null);
      }
    },
    [refresh],
  );

  /** 扫描局域网（约 1.5s）；结果点击仅预填添加对话框，不自动连接。 */
  const onScan = useCallback(async () => {
    setScanning(true);
    setNotice(null);
    setError(null);
    try {
      setDiscovered(await discoverDevices());
    } catch (e) {
      setError(`扫描失败：${errText(e)}`);
    } finally {
      setScanning(false);
    }
  }, []);

  /** 方案实验室：直接在主窗口内切换到 #/lab，避免再弹第二个带导航栏的嵌套窗口。 */
  const onOpenStudio = useCallback(() => {
    window.location.hash = "#/lab";
  }, []);

  /** fec 高清链路开关（写 settings.json，下次连接生效）。经 bridge call()（D06）。 */
  const onToggleMediaFec = useCallback(async () => {
    try {
      const next = !mediaFec;
      await call<void>("set_media_fec", { enabled: next });
      setMediaFec(next);
    } catch (e) {
      setError(`保存弱网纠错开关失败：${errText(e)}`);
    }
  }, [mediaFec]);

  /** 把发现的设备预填进添加对话框（需再输配对码完成配对）。 */
  const onPickDiscovered = useCallback((d: DiscoveredDeviceDto) => {
    openAddDialog({ addr: d.addr, name: d.name });
  }, [openAddDialog]);

  const onDisconnect = useCallback(
    async (d: DeviceDto) => {
      setBusyId(d.id);
      try {
        await disconnectDevice(d.id);
        setNotice(`已断开 ${d.name}`);
        await refresh();
      } catch (e) {
        setError(errText(e));
      } finally {
        setBusyId(null);
      }
    },
    [refresh],
  );

  const onProbe = useCallback(
    async (d: DeviceDto) => {
      setBusyId(d.id);
      setNotice(null);
      try {
        const ok = await probeDevice(d.id);
        setNotice(ok ? `设备「${d.name}」可达` : `设备「${d.name}」不可达（超时或指纹不匹配）`);
      } catch (e) {
        setError(errText(e));
      } finally {
        setBusyId(null);
      }
    },
    [],
  );

  const submitAdd = useCallback(
    async (e: FormEvent) => {
      e.preventDefault();
      setFormError(null);
      setPairing(true);
      try {
        if (editing !== null) {
          // 编辑模式：只改名称/地址，不需要配对码
          await editDevice(editing.id, form.name, form.addr, form.relay);
          setShowAdd(false);
          setEditing(null);
          setForm({ addr: "127.0.0.1:21118", code: "", name: "", relay: "" });
          await refresh();
        } else {
          const d = await pairDevice(form.addr, form.code, form.name);
          setShowAdd(false);
          setForm({ addr: "127.0.0.1:21118", code: "", name: "", relay: "" });
          await refresh();
          // 输码直连：配对成功即连接进入（省一击——竞品对照 R4 共识）
          setBusyId(d.id);
          try {
            const ack = await connectDevice(d.id);
            rememberHostVersion(d.id, ack.hostVersion);
            rememberLastDeviceId(d.id);
            window.location.hash = `#/session/${d.id}?open=${DESKTOP_PRIMARY_ID}`;
          } catch {
            // 直连失败不回滚配对：设备已入库，用户可手动再连
            setNotice(`设备「${d.name}」已添加；连接未成功，可在列表中重试`);
          } finally {
            setBusyId(null);
          }
        }
      } catch (e) {
        setFormError(errText(e));
      } finally {
        setPairing(false);
      }
    },
    [form, refresh, editing],
  );

  /** 进入编辑模式（复用添加对话框，隐藏配对码栏）。 */
  const onEdit = useCallback(
    (d: DeviceDto) => {
      openAddDialog({ addr: d.addr, name: d.name, relay: d.relay ?? undefined }, d);
    },
    [openAddDialog],
  );

  /** Tailscale 链路测试（直达/DERP 中继）。结果标注设备名而非 id。 */
  const onLinkTest = useCallback(async (d: DeviceDto) => {
    const ip = d.addr.split(":")[0] ?? "";
    if (!ip.startsWith("100.")) {
      return;
    }
    setBusyId(d.id);
    setLinkTest({ id: d.id, text: "测试中…" });
    try {
      const text = await tailscalePing(ip);
      setLinkTest({ id: d.id, text: `「${d.name}」链路：${text}` });
    } catch (e) {
      setLinkTest({ id: d.id, text: `「${d.name}」链路测试没有成功：${errText(e)}` });
    } finally {
      setBusyId(null);
    }
  }, []);

  /** 远程开机（T-031 WOL）：向本网段广播魔术包；数秒后可再点连通测试确认。 */
  const onWake = useCallback(async (d: DeviceDto) => {
    setBusyId(d.id);
    try {
      await wakeDevice(d.id);
      setNotice(`已向「${d.name}」发唤醒包：同网段时数秒内开机，稍后可用「连通测试」确认`);
    } catch (e) {
      setNotice(`唤醒失败：${errText(e)}`);
    } finally {
      setBusyId(null);
    }
  }, []);

  /** ⋯ 菜单动作分发（R12：菜单本体抽 DeviceRowMenu，协议收编 useDismiss）。 */
  const onMenuAction = useCallback(
    (
      d: DeviceDto,
      a:
        | "disconnect"
        | "wake"
        | "copyAddr"
        | "probe"
        | "linkTest"
        | "smartLab"
        | "copyFp"
        | "edit"
        | "remove",
    ) => {
      switch (a) {
        case "disconnect":
          void onDisconnect(d);
          break;
        case "wake":
          void onWake(d);
          break;
        case "copyAddr":
          void navigator.clipboard
            .writeText(d.addr)
            .then(() => setNotice(`已复制地址：${d.addr}`))
            .catch(() => undefined);
          break;
        case "probe":
          void onProbe(d);
          break;
        case "linkTest":
          void onLinkTest(d);
          break;
        case "smartLab":
          window.location.hash = `#/lab/${d.id}`;
          break;
        case "copyFp":
          void navigator.clipboard
            .writeText(d.fingerprintHex)
            .then(() =>
              setNotice(
                `「${d.name}」的设备指纹已复制（SHA-256 前 16 位：${d.fingerprintHex.slice(0, 16)}），可与对方核对`,
              ),
            )
            .catch(() => undefined);
          break;
        case "edit":
          onEdit(d);
          break;
        case "remove":
          setConfirmRemoveId(d.id);
          break;
      }
    },
    [onDisconnect, onProbe, onLinkTest, onEdit],
  );

  /** 搜索过滤后的设备列表（>3 台出现搜索框时使用）。 */
  const shownDevices =
    deviceQuery.trim().length === 0
      ? devices
      : devices.filter(
          (d) =>
            d.name.toLowerCase().includes(deviceQuery.trim().toLowerCase()) ||
            d.addr.toLowerCase().includes(deviceQuery.trim().toLowerCase()),
        );

  const closeAdd = useCallback(() => {
    setShowAdd(false);
    setEditing(null);
    setFormError(null);
  }, []);

  // 添加/编辑对话框焦点协议（R11 D07）：打开时记录触发点，Esc（文档级）
  // 关闭，Tab 困在对话框内循环，关闭后焦点还原到触发按钮——曾 aria-modal
  // 却无陷阱，Tab 穿透背景、关闭后焦点丢 BODY。
  const addDialogRef = useRef<HTMLFormElement | null>(null);
  useEffect(() => {
    if (!showAdd) {
      return undefined;
    }
    const trigger = addTriggerRef.current;
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === "Escape") {
        e.preventDefault();
        closeAdd();
        return;
      }
      if (e.key !== "Tab" || addDialogRef.current === null) {
        return;
      }
      const focusables = addDialogRef.current.querySelectorAll<HTMLElement>(
        'button:not([disabled]), input:not([disabled]), [tabindex]:not([tabindex="-1"])',
      );
      if (focusables.length === 0) {
        return;
      }
      const first = focusables[0] as HTMLElement;
      const last = focusables[focusables.length - 1] as HTMLElement;
      if (e.shiftKey && document.activeElement === first) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault();
        first.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
      trigger?.focus();
    };
  }, [showAdd, closeAdd]);

  /** RustDesk 式顶部快速直连栏（宪法 #5）：输入 IP/地址（可选 6 位配对码）一步直连或自动配对。 */
  const [quickAddr, setQuickAddr] = useState("");
  const [quickCode, setQuickCode] = useState("");
  const [quickBusy, setQuickBusy] = useState(false);

  const onQuickConnect = useCallback(
    async (e: FormEvent) => {
      e.preventDefault();
      const raw = quickAddr.trim();
      if (raw.length === 0) {
        return;
      }
      setError(null);
      const norm = raw.includes(":") ? raw : `${raw}:21118`;
      const existing = devices.find(
        (d) => d.addr === raw || d.addr === norm || d.name.toLowerCase() === raw.toLowerCase(),
      );
      if (existing !== undefined) {
        await onConnect(existing);
        return;
      }
      const code = quickCode.trim();
      if (code.length === 6) {
        setQuickBusy(true);
        try {
          const added = await pairDevice(raw, code, "");
          await refresh();
          setQuickAddr("");
          setQuickCode("");
          await onConnect(added);
        } catch (err) {
          setError(`快速配对失败：${errText(err)}`);
        } finally {
          setQuickBusy(false);
        }
        return;
      }
      // 未配对设备且未填 6 位码：预填地址直达配对框
      openAddDialog({ addr: raw, name: "" });
    },
    [devices, onConnect, openAddDialog, quickAddr, quickCode, refresh],
  );

  const updateForm = useCallback(
    (field: "addr" | "code" | "name" | "relay", value: string) => {
      setForm((prev) => ({ ...prev, [field]: value }));
    },
    [],
  );

  return (
    <main className={styles.page}>
      <div className={styles.rdShell}>
        {/* ── 左侧固定面板：RustDesk 风格「你的桌面（本机被控）」 ── */}
        <aside className={styles.yourDesktop} aria-label="你的桌面与本机被控">
          <span className={styles.eyebrow}>你的桌面 · THIS DESKTOP</span>

          {host === null && hostDown && (
            <div role="status" style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
              <div className={styles.desktopStatusRow}>
                <strong>本机被控</strong>
                <span className={styles.hostDownTag}>被控服务不可用</span>
              </div>
              <p className={styles.hostDownHint}>
                查不到本机被控状态，可能端口被占用或被控核心未启动（不影响控制别的电脑）。
              </p>
              <button
                type="button"
                className={styles.toolbarBtn}
                disabled={busyId !== null}
                onClick={() => void pollHost()}
              >
                重新检查
              </button>
            </div>
          )}

          {host !== null && (
            <section aria-label="本机被控状态" style={{ display: "flex", flexDirection: "column", gap: "12px" }}>
              <div className={styles.desktopStatusRow}>
                {host.peers.length === 0 ? (
                  <>
                    <span className={styles.hostStateIdle}>本机待命</span>
                    {host.paused && <span className={styles.hostPaused}>已暂停共享</span>}
                  </>
                ) : (
                  <>
                    <span className={styles.hostStateBusy}>正在被控制</span>
                    {host.peers.map((p) => (
                      <span key={p.name} className={styles.hostPeer} title="对方开始控制本机的时长">
                        「{p.name}」· {hostDuration(p.sinceSecs)}
                      </span>
                    ))}
                  </>
                )}
                <span className={styles.hostSpacer} />
                {host.peers.length > 0 && (
                  <button
                    type="button"
                    className={styles.cutBtn}
                    title="立即断开对方的控制连接"
                    disabled={cutting}
                    onClick={() => void onCutPeers()}
                  >
                    {cutting ? "切断中…" : "切断"}
                  </button>
                )}
                <button
                  type="button"
                  className={styles.expandBtn}
                  aria-expanded={hostExpanded}
                  aria-label="本机被控详情"
                  title="监听地址 · 弱网纠错 · 指纹核对"
                  onClick={toggleHostExpanded}
                >
                  {hostExpanded ? <IconChevronUp size={14} /> : <IconChevronDown size={14} />}
                </button>
              </div>

              <span className={styles.srOnly} role="status">{hostLiveMsg}</span>

              {host.listenAddrs.length > 0 && (
                <>
                  {relayAddr.length > 0 && (
                    <div className={styles.idBox} style={{ borderColor: "rgb(61 126 255 / 35%)" }}>
                      <span className={styles.idLabel}>本机设备 ID（已开启中转 · 跨网直连）</span>
                      <div className={styles.idValueRow}>
                        <code className={styles.idValue}>
                          {deriveNineDigitId(host.listenAddrs.join("|"))}
                        </code>
                        <button
                          type="button"
                          className={styles.copyMiniBtn}
                          onClick={() => {
                            const id = deriveNineDigitId(host.listenAddrs.join("|")).replace(/\s+/g, "");
                            void navigator.clipboard.writeText(id).catch(() => undefined);
                            setNotice(`已复制本机设备 ID：${id}`);
                          }}
                        >
                          复制
                        </button>
                      </div>
                    </div>
                  )}
                  <div className={styles.idBox}>
                    <span className={styles.idLabel}>
                      {relayAddr.length > 0
                        ? "局域网直连 IP（同网段 4K@144Hz）"
                        : "本机连接地址（发给对方即可被控）"}
                    </span>
                    <div className={styles.idValueRow}>
                      <code className={styles.idValue} title={formatDisplayAddr(host.listenAddrs[0] ?? "")}>
                        {formatDisplayAddr(host.listenAddrs[0] ?? "")}
                      </code>
                      <button
                        type="button"
                        className={styles.copyMiniBtn}
                        onClick={() => {
                          const ip = formatDisplayAddr(host.listenAddrs[0] ?? "");
                          void navigator.clipboard.writeText(ip).catch(() => undefined);
                          setNotice(`已复制本机地址：${ip}`);
                        }}
                      >
                        复制
                      </button>
                    </div>
                  </div>
                </>
              )}

              {host.peers.length === 0 && (
                <div className={styles.pairHeroBox}>
                  <span className={styles.idLabel}>一次性配对码（5 分钟有效）</span>
                  {pairCard !== null ? (
                    <>
                      <div className={styles.idValueRow}>
                        <code className={styles.pairHeroCode} data-expired={pairRemainSecs === 0}>
                          {pairCard.code}
                        </code>
                        <button
                          type="button"
                          className={styles.copyMiniBtn}
                          onClick={() => void onGenerateCode()}
                        >
                          {pairRemainSecs === 0 ? "重新生成" : "刷新"}
                        </button>
                      </div>
                      <span className={styles.hostLabel}>
                        {pairRemainSecs === 0
                          ? "配对码已过期，请重新生成"
                          : `还剩 ${Math.floor(pairRemainSecs / 60)}:${String(pairRemainSecs % 60).padStart(2, "0")} · 已复制到剪贴板`}
                      </span>
                    </>
                  ) : (
                    <button
                      type="button"
                      className={styles.genCodeBtn}
                      onClick={() => void onGenerateCode()}
                    >
                      生成配对码
                    </button>
                  )}
                </div>
              )}

              {hostExpanded && (
                <div className={styles.hostDetails}>
                  {host.listenAddrs.length > 0 && (
                    <div className={styles.hostRow}>
                      <span className={styles.hostLabel}>监听地址：</span>
                      {host.listenAddrs.map((a) => (
                        <code key={a} className={styles.hostAddr}>
                          <span className={styles.addrKind} data-tailscale={a.startsWith("100.")}>
                            {a.startsWith("100.") ? "Tailscale" : "局域网"}
                          </span>
                          {formatDisplayAddr(a)}
                        </code>
                      ))}
                    </div>
                  )}
                  <div className={styles.hostRow}>
                    <label className={styles.fecToggle} title="开启后信号差时画面仍流畅不花屏（带宽多耗约 20%）；下次连接生效">
                      <input
                        type="checkbox"
                        className={styles.switchInput}
                        checked={mediaFec}
                        onChange={() => void onToggleMediaFec()}
                      />
                      <span className={styles.switchTrack} data-on={mediaFec} aria-hidden="true">
                        <span className={styles.switchThumb} />
                      </span>
                      <span className={styles.fecText}>
                        <span className={styles.fecTitle}>弱网纠错</span>
                        <span className={styles.fecSub}>信号差时更稳 · 下次连接生效</span>
                      </span>
                    </label>
                  </div>
                  <div className={styles.hostRow}>
                    <span className={styles.hostLabel}>
                      安全核对：点设备卡右上角「⋯」的「查看指纹」可核对身份防中间人。
                    </span>
                  </div>
                </div>
              )}
            </section>
          )}

          <footer className={styles.leftFooter}>
            <div className={styles.footerRow}>
              <button
                type="button"
                className={styles.helpToggle}
                aria-expanded={helpOpen}
                onClick={() => setHelpOpen((v) => !v)}
              >
                连不上对方？
                <svg
                  width="12"
                  height="12"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="2"
                  aria-hidden="true"
                  style={{ transform: helpOpen ? "rotate(180deg)" : undefined }}
                >
                  <path d="M6 9.5l6 6 6-6" />
                </svg>
              </button>
              <button
                type="button"
                className={styles.linkBtn}
                onClick={() => void onOpenStudio()}
                title="画质方案对比与链路实测（高级）"
              >
                <IconLab size={12} />
                方案实验室
              </button>
            </div>
            {helpOpen && (
              <ul className={styles.helpList}>
                <li>
                  <b>同局域网：</b>点「扫描局域网」或直接填对方内网 IP（免填端口）。
                </li>
                <li>
                  <b>跨网络：</b>两台电脑登录同一 Tailscale 网络，填 100.x 地址即可直连。
                  <button
                    type="button"
                    className={styles.linkBtn}
                    onClick={() => void openUrl("https://login.tailscale.com/admin/users").catch(() => undefined)}
                  >
                    Tailscale 后台
                  </button>
                </li>
              </ul>
            )}
            <span className={styles.version}>rd-controller v{APP_VERSION}</span>
          </footer>
        </aside>

        {/* ── 右侧主工作区：控制远程桌面 + 多列设备卡片网格 ── */}
        <div className={styles.remoteWorkspace}>
          <form
            className={styles.connectHero}
            onSubmit={(e) => void onQuickConnect(e)}
            aria-label="快速连接远端设备"
          >
            <div className={styles.connectHeroHead}>
              <h1 className={styles.connectHeroTitle}>控制远程桌面</h1>
              <span className={styles.connectHeroBadge}>默认直入完整桌面 · 4K@144Hz</span>
            </div>
            <div className={styles.connectHeroRow}>
              <input
                type="text"
                className={styles.heroInput}
                value={quickAddr}
                onChange={(e) => setQuickAddr(e.currentTarget.value)}
                placeholder={
                  relayAddr.length > 0
                    ? "输入对方 9 位设备 ID 或 IP（如 482 916 305 或 192.168.1.10）"
                    : "输入对方 IP 或设备名（如 192.168.1.10 或 100.81.205.0，免填端口）"
                }
                aria-label="快速连接地址"
                spellCheck={false}
              />
              <input
                type="text"
                className={styles.heroCodeInput}
                value={quickCode}
                onChange={(e) => setQuickCode(e.currentTarget.value)}
                placeholder="6 位配对码 / 密码"
                aria-label="快速连接配对码"
                maxLength={16}
                spellCheck={false}
              />
              <button
                type="submit"
                className={styles.heroSubmitBtn}
                disabled={quickBusy || busyId !== null || quickAddr.trim().length === 0}
              >
                {quickBusy ? "连接中…" : "连接桌面"}
              </button>
            </div>
          </form>

          {resume !== null && (
            <button
              type="button"
              className={styles.resumeCard}
              onClick={() => {
                const target = devices.find((d) => d.id === resume.id);
                if (target !== undefined) {
                  void onConnect(target);
                }
              }}
              disabled={busyId !== null}
              title="一键回到上次使用的设备"
            >
              <span className={styles.resumeLabel}>继续上次</span>
              <span className={styles.resumeName}>
                <span className={styles.dot} data-online={resume.online} aria-hidden="true" />
                {resume.name}
              </span>
              <span className={styles.resumeGo}>进入桌面 →</span>
            </button>
          )}

          {error !== null && <p className={styles.error}>{error}</p>}
          {notice !== null && <p className={styles.notice}>{notice}</p>}

          <div className={styles.gridToolbar}>
            <div className={styles.gridToolbarLeft}>
              <h2 className={styles.title} style={{ fontSize: "15px" }}>
                我的设备
                {devices.length > 0 && (
                  <span className={styles.titleMeta}>
                    {devices.length} 台 · {devices.filter((d) => d.online).length} 在线
                    {devices.some((d) => !d.online) &&
                      ` · ${devices.filter((d) => !d.online).length} 离线`}
                  </span>
                )}
              </h2>
              {devices.length >= 3 && (
                <input
                  className={styles.toolbarSearch}
                  type="search"
                  placeholder={`搜索 ${devices.length} 台设备（按 / 聚焦）…`}
                  aria-label="搜索设备"
                  value={deviceQuery}
                  onChange={(e) => setDeviceQuery(e.currentTarget.value)}
                />
              )}
            </div>
            <div className={styles.gridToolbarRight}>
              <button
                type="button"
                className={styles.toolbarBtn}
                onClick={() => void onScan()}
                disabled={scanning || busyId !== null}
              >
                <IconScan size={13} />
                {scanning ? "扫描中…" : "扫描局域网"}
              </button>
              <button
                type="button"
                className={styles.toolbarBtnPrimary}
                onClick={() => openAddDialog()}
              >
                <IconPlus size={13} />
                添加设备
              </button>
            </div>
          </div>

          {deviceQuery.trim().length > 0 && shownDevices.length === 0 && (
            <p className={styles.empty}>没有匹配「{deviceQuery.trim()}」的设备。</p>
          )}

          {devices.length === 0 ? (
            <div className={styles.emptyState}>
              <div className={styles.emptyIcon} aria-hidden="true">
                <svg width="44" height="44" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round">
                  <rect x="2.5" y="4" width="19" height="13" rx="2" />
                  <path d="M9 21h6M12 17.5V21" />
                  <circle cx="12" cy="10.5" r="2.6" />
                  <path d="M12 7.9v2.6l1.8 1.2" />
                </svg>
              </div>
              <p className={styles.emptyTitle}>还没有已保存的设备</p>
              <p className={styles.emptyHint}>
                直接在上方输入对方 IP 与 6 位配对码即可连接，或点击「扫描局域网」自动发现附近电脑。
              </p>
              <div className={styles.emptyActions}>
                <button type="button" className={styles.toolbarBtnPrimary} onClick={() => openAddDialog()}>
                  手动添加设备
                </button>
                <button
                  type="button"
                  className={styles.toolbarBtn}
                  onClick={() => void onScan()}
                  disabled={scanning || busyId !== null}
                >
                  {scanning ? "扫描中…" : "扫描局域网发现"}
                </button>
              </div>
            </div>
          ) : (
            <ul className={styles.deviceGrid}>
              {shownDevices.map((d) => {
                const displayIp = formatDisplayAddr(d.addr);
                const isTs = displayIp.startsWith("100.");
                const isLoop = displayIp.startsWith("127.");
                return (
                  <li key={d.id} className={styles.deviceCard}>
                    <div className={styles.cardPreview} data-online={d.online}>
                      <div className={styles.cardBadges}>
                        <span className={styles.dot} data-online={d.online} aria-hidden="true" />
                        <span className={styles.onlineTag} data-online={d.online}>
                          {d.online ? "在线" : "离线"}
                        </span>
                        <span className={styles.netBadge} data-tailscale={isTs}>
                          {isTs ? "Tailscale" : isLoop ? "本机" : "局域网"}
                        </span>
                      </div>
                      <DeviceRowMenu d={d} onAction={(a) => onMenuAction(d, a)} />
                    </div>

                    <div className={styles.cardBody}>
                      <span className={styles.cardTitle} title={d.name}>
                        {d.name}
                      </span>
                      <span className={styles.cardAddr}>{displayIp}</span>
                      <span className={styles.cardMeta}>
                        {lastConnectedAt(d.id) !== null
                          ? `上次连接 ${relativeTime(lastConnectedAt(d.id) as number)}`
                          : "已配对信任"}
                        {getHostVersion(d.id) !== null ? ` · v${getHostVersion(d.id)}` : ""}
                      </span>
                    </div>

                    <div className={styles.cardFooter}>
                      {confirmRemoveId === d.id ? (
                        <>
                          <button
                            type="button"
                            className={`${styles.actionBtn} ${styles.confirmDelete}`}
                            style={{ flex: 1, height: "34px" }}
                            onClick={() => {
                              if (confirmArmed) {
                                void onRemove(d);
                              }
                            }}
                            disabled={busyId !== null || !confirmArmed}
                            data-armed={confirmArmed}
                            title={confirmArmed ? "此操作不可撤销" : "防误触：进度条走完即可点击"}
                          >
                            确认删除
                            {!confirmArmed && (
                              <span className={styles.confirmProgress} aria-hidden="true" />
                            )}
                          </button>
                          <button
                            type="button"
                            className={styles.toolbarBtn}
                            onClick={() => setConfirmRemoveId(null)}
                          >
                            取消
                          </button>
                        </>
                      ) : (
                        <button
                          type="button"
                          className={styles.cardConnectBtn}
                          data-online={d.online}
                          onClick={() => void onConnect(d)}
                          disabled={busyId !== null}
                          title={`连接 ${d.name} 完整桌面（指纹 ${d.fingerprintHex.slice(0, 16)}…）`}
                        >
                          {busyId === d.id
                            ? "连接中…"
                            : d.online
                              ? "连接桌面"
                              : "唤醒连接"}
                        </button>
                      )}
                    </div>

                    {linkTest !== null && linkTest.id === d.id && (
                      <div
                        className={styles.linkTestOut}
                        style={{ margin: "0 12px 12px" }}
                        data-failed={linkTest.text.includes("没有成功")}
                      >
                        {linkTest.text}
                      </div>
                    )}
                  </li>
                );
              })}
            </ul>
          )}

          {(scanning || discovered !== null) && (
            <p className={styles.scanStatus} role="status" aria-live="polite">
              {scanning
                ? "正在扫描局域网（约需 2 秒）…"
                : discovered !== null && discovered.length > 0
                  ? `扫描完成：发现 ${discovered.length} 台设备`
                  : "扫描完成：没有发现新设备"}
            </p>
          )}
          {discovered !== null && discovered.length === 0 && !scanning && (
            <div className={styles.scanEmpty} role="presentation">
              <div className={styles.scanEmptyIcon} aria-hidden="true">
                <IconScan size={26} />
              </div>
              <p className={styles.scanEmptyTitle}>没有发现新设备</p>
              <p className={styles.scanEmptyHint}>
                可能是两台电脑不在同一局域网/WiFi，或对方未打开 rd-controller。可直接输入对方 IP 连接。
              </p>
              <div className={styles.emptyActions}>
                <button
                  type="button"
                  className={styles.toolbarBtnPrimary}
                  onClick={() => openAddDialog()}
                >
                  手动添加设备
                </button>
                <button
                  type="button"
                  className={styles.toolbarBtn}
                  onClick={() => void onScan()}
                  disabled={busyId !== null}
                >
                  再扫一次
                </button>
              </div>
            </div>
          )}

          {discovered !== null && discovered.length > 0 && (
            <section className={styles.scanSection} aria-label="局域网发现的设备">
              <h2 className={styles.scanHeading}>
                局域网发现（未配对 · 点击输入对方 6 位配对码即可连接）
              </h2>
              <ul className={styles.deviceGrid}>
                {discovered.map((d) => (
                  <li key={d.addr} className={styles.deviceCard}>
                    <div className={styles.cardPreview}>
                      <div className={styles.cardBadges}>
                        <span className={`${styles.dot} ${styles.dotScan}`} aria-hidden="true" />
                        <span className={styles.unpairedTag}>未配对</span>
                        <span className={styles.netBadge}>局域网</span>
                      </div>
                    </div>
                    <div className={styles.cardBody}>
                      <span className={styles.cardTitle}>{d.name}</span>
                      <span className={styles.cardAddr}>{formatDisplayAddr(d.addr)}</span>
                    </div>
                    <div className={styles.cardFooter}>
                      <button
                        type="button"
                        className={styles.cardConnectBtn}
                        onClick={() => onPickDiscovered(d)}
                      >
                        配对并连接
                      </button>
                    </div>
                  </li>
                ))}
              </ul>
            </section>
          )}
        </div>
      </div>

      {showAdd && (
        <div
          className={styles.overlay}
          role="presentation"
          onClick={(e) => {
            if (e.target === e.currentTarget && editing === null) {
              closeAdd();
            }
          }}
        >
          <form
            ref={addDialogRef}
            className={styles.dialog}
            role="dialog"
            aria-modal="true"
            aria-label={editing !== null ? `编辑设备：${editing.name}` : "添加设备"}
            onClick={(e) => e.stopPropagation()}
            onSubmit={(e) => void submitAdd(e)}
          >
            <h2 className={styles.dialogTitle}>
              {editing !== null ? `编辑设备：${editing.name}` : "添加设备"}
            </h2>
            <label className={styles.field}>
              <span>对方 IP 地址</span>
              <input
                className={styles.input}
                value={form.addr}
                onChange={(e) => updateForm("addr", e.currentTarget.value)}
                placeholder="如 192.168.1.10 或 100.81.205.0（默认端口无需填写）"
                spellCheck={false}
                autoFocus
              />
            </label>
            {editing === null && (
              <label className={styles.field}>
                <span>配对码</span>
                <input
                  className={styles.input}
                  value={form.code}
                  onChange={(e) => updateForm("code", e.currentTarget.value)}
                  placeholder="对方首页左侧「你的桌面」显示的 6 位配对码"
                  inputMode="numeric"
                  maxLength={6}
                />
              </label>
            )}
            {editing !== null && (
              <label className={styles.field}>
                <span>原生中继（可选）</span>
                <input
                  className={styles.input}
                  value={form.relay}
                  onChange={(e) => updateForm("relay", e.currentTarget.value)}
                  placeholder="跨网打洞失败的兜底，如 vps.example.com:21118；直连失败才启用"
                  spellCheck={false}
                />
              </label>
            )}
            <label className={styles.field}>
              <span>名称（可选）</span>
              <input
                className={styles.input}
                value={form.name}
                onChange={(e) => updateForm("name", e.currentTarget.value)}
                placeholder={`默认取地址主机部分`}
              />
            </label>
            {formError !== null && <p className={styles.error}>{formError}</p>}
            <div className={styles.dialogActions}>
              <button type="button" className={styles.actionBtn} onClick={closeAdd}>
                取消
              </button>
              <button type="submit" className={styles.addBtn} disabled={pairing}>
                {editing !== null ? "保存" : pairing ? "配对中…" : "配对"}
              </button>
            </div>
          </form>
        </div>
      )}
    </main>
  );
}

/** 单行 ⋯ 菜单（R12 无障碍：菜单协议唯一出口 useDismiss——Esc/点外/焦点还原
 * + menu 键控 ↑↓/Home/End + 打开聚焦首项，`role="menu"` 不再是空承诺）。 */
function DeviceRowMenu({
  d,
  onAction,
}: {
  d: DeviceDto;
  onAction: (
    a:
      | "disconnect"
      | "wake"
      | "copyAddr"
      | "probe"
      | "linkTest"
      | "smartLab"
      | "copyFp"
      | "edit"
      | "remove",
  ) => void;
}) {
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);
  useDismiss(open, wrapRef, useCallback(() => setOpen(false), []), { menu: true });
  const act = (a: Parameters<typeof onAction>[0]): void => {
    setOpen(false);
    onAction(a);
  };
  return (
    <div className={styles.menuWrap} ref={wrapRef}>
      <button
        type="button"
        className={styles.moreBtn}
        aria-label={d.name + " 的更多操作"}
        title="更多操作"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
      >
        <IconMore size={16} />
      </button>
      {open && (
        <div className={styles.menu} role="menu" aria-label={`${d.name} 操作`}>
          {d.online && (
            <button type="button" className={styles.menuItem} role="menuitem" onClick={() => act("disconnect")}>
              断开连接
            </button>
          )}
          {!d.online && (d.macAddrs?.length ?? 0) > 0 && (
            <button
              type="button"
              className={styles.menuItem}
              role="menuitem"
              onClick={() => act("wake")}
              title="向本网段广播开机魔术包（设备需与这台电脑在同一局域网；需至少成功连接过一次）"
            >
              远程开机
            </button>
          )}
          <button type="button" className={styles.menuItem} role="menuitem" onClick={() => act("copyAddr")}>
            复制地址
          </button>
          <button type="button" className={styles.menuItem} role="menuitem" onClick={() => act("probe")}>
            连通测试
          </button>
          <button
            type="button"
            className={styles.menuItem}
            role="menuitem"
            onClick={() => act("smartLab")}
            title="进入方案实验室对该设备进行全方案自动寻优与自定义线路绑定"
          >
            智能寻优（实验室）
          </button>
          {d.addr.split(":")[0]?.startsWith("100.") && (
            <button
              type="button"
              className={styles.menuItem}
              role="menuitem"
              onClick={() => act("linkTest")}
              title="Tailscale 链路：直达或经中继"
            >
              线路测试
            </button>
          )}
          <button
            type="button"
            className={styles.menuItem}
            role="menuitem"
            onClick={() => act("copyFp")}
            title="设备指纹是本连接的唯一凭证标识，可与对方屏幕上显示的指纹比对以防中间人"
          >
            查看指纹
          </button>
          <button type="button" className={styles.menuItem} role="menuitem" onClick={() => act("edit")}>
            编辑
          </button>
          <button
            type="button"
            className={`${styles.menuItem} ${styles.menuItemDanger}`}
            role="menuitem"
            onClick={() => act("remove")}
          >
            删除设备…
          </button>
        </div>
      )}
    </div>
  );
}
