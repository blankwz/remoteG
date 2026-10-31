/**
 * 设备页（T-019）：设备卡片 + 「添加设备」对话框 + 局域网扫描（D-013 包1：
 * 免手填 IP）+ 指纹不匹配一键重配对 + Tailscale 节点共享引导。
 */
import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  APP_VERSION,
  clearLastDeviceId,
  connectDevice,
  disconnectDevice,
  discoverDevices,
  editDevice,
  generatePairCode,
  getHostVersion,
  getLastDeviceId,
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
  type DeviceDto,
  type DiscoveredDeviceDto,
} from "../api/devices";
import { call, listenStub } from "../api/bridge";

import { IconLab, IconMore, IconPlus, IconScan } from "../ui/Icon";
import styles from "./Devices.module.css";

/** 把 Error/unknown 统一成消息文本。 */
function errText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
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
  const [form, setForm] = useState({ addr: "127.0.0.1:21118", code: "", name: "" });
  /** 局域网扫描结果（发现 ≠ 信任，点击仅预填添加对话框）。 */
  const [discovered, setDiscovered] = useState<DiscoveredDeviceDto[] | null>(null);
  const [scanning, setScanning] = useState(false);
  /** fec 高清链路开关（settings.json 持久化；下次连接生效）。 */
  const [mediaFec, setMediaFec] = useState(false);
  /** 本机被控状态（融合版：本机也是被控机）。 */
  const [host, setHost] = useState<HostStatusDto | null>(null);
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
  /** 展开次操作菜单的设备 id（⋯ 溢出）。 */
  const [menuId, setMenuId] = useState<string | null>(null);
  /** 连接帮助折叠显隐。 */
  const [helpOpen, setHelpOpen] = useState(false);
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

  // ⋯ 菜单点外关闭（Holmes R2：菜单协议统一）
  useEffect(() => {
    if (menuId === null) {
      return undefined;
    }
    const onDown = (e: MouseEvent): void => {
      const el = e.target as HTMLElement;
      if (!el.closest(`.${styles.menuWrap}`)) {
        setMenuId(null);
      }
    };
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === "Escape") {
        setMenuId(null);
      }
    };
    window.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [menuId]);
  /** 上次连接的设备（「继续上次」快达卡；null = 无历史）。 */
  const [resume, setResume] = useState<{ id: string; name: string; online: boolean } | null>(null);

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

  useEffect(() => {
    void refresh();
    // fec 开关回显（settings.json）；经 bridge call()（demo 模式给人话，R11 D06）
    void call<boolean>("get_media_fec")
      .then(setMediaFec)
      .catch(() => undefined);
    // ConnManager 断线（含主动断开）广播 → 刷新在线圆点
    const unlisten = listenStub("device-disconnected", () => {
      void refresh();
    });
    // 被控状态 2s 轮询（地址/被谁连接/暂停态）
    const hostTimer = window.setInterval(() => {
      void hostStatus()
        .then(setHost)
        .catch(() => undefined);
    }, 2000);
    void hostStatus()
      .then(setHost)
      .catch(() => undefined);
    return () => {
      void unlisten.then((fn) => fn());
      window.clearInterval(hostTimer);
    };
  }, [refresh]);

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
        window.location.hash = `#/windows/${d.id}`;
        setNotice(null);
      } catch (e) {
        const msg = errText(e);
        // 指纹不匹配（对端重装/换证书）→ 一键重配对：预填地址打开添加对话框
        if (msg.includes("指纹不匹配")) {
          setShowAdd(true);
          setForm({ addr: d.addr, code: "", name: d.name });
          setFormError(
            "对端证书已变化：可能是对方重装了系统/rd-agent，也可能是网络被劫持。" +
              "若对方确认刚重装过，请对方在托盘点「生成配对码」，你再输入新码完成重新配对；" +
              "若对方没有重装，请勿输入配对码，先通过其他渠道核实对方身份。",
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
        setMenuId(null);
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

  /** 方案实验室（Studio 内嵌窗口，2026-10-28 融合）。经 bridge call()（D06）。 */
  const onOpenStudio = useCallback(async () => {
    try {
      await call<void>("open_studio");
    } catch (e) {
      setError(`打开方案实验室没有成功：${errText(e)}。可稍后重试，或检查安装目录中的 Studio 组件`);
    }
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
    setShowAdd(true);
    setForm({ addr: d.addr, code: "", name: d.name });
    setFormError(null);
  }, []);

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
          await editDevice(editing.id, form.name, form.addr);
          setShowAdd(false);
          setEditing(null);
          setForm({ addr: "127.0.0.1:21118", code: "", name: "" });
          await refresh();
        } else {
          const d = await pairDevice(form.addr, form.code, form.name);
          setShowAdd(false);
          setForm({ addr: "127.0.0.1:21118", code: "", name: "" });
          await refresh();
          // 输码直连：配对成功即连接进入（省一击——竞品对照 R4 共识）
          setBusyId(d.id);
          try {
            const ack = await connectDevice(d.id);
            rememberHostVersion(d.id, ack.hostVersion);
            rememberLastDeviceId(d.id);
            window.location.hash = `#/windows/${d.id}`;
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
  const onEdit = useCallback((d: DeviceDto) => {
    setEditing(d);
    setShowAdd(true);
    setForm({ addr: d.addr, code: "", name: d.name });
    setFormError(null);
  }, []);

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
    const trigger = document.activeElement as HTMLElement | null;
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

  const updateForm = useCallback(
    (field: "addr" | "code" | "name", value: string) => {
      setForm((prev) => ({ ...prev, [field]: value }));
    },
    [],
  );

  return (
    <main className={styles.page}>
      <header className={styles.header}>
        <h1 className={styles.title}>
          设备
          {devices.length > 0 && (
            <span className={styles.titleMeta}>
              {devices.length} 台 · {devices.filter((d) => d.online).length} 在线
              {devices.some((d) => !d.online) &&
                ` · ${devices.filter((d) => !d.online).length} 离线`}
            </span>
          )}
        </h1>
        <div className={styles.headerActions}>
          <button
            type="button"
            className={styles.actionBtn}
            onClick={() => void onScan()}
            disabled={scanning || busyId !== null}
          >
            <IconScan size={13} />
            {scanning ? "扫描中…" : "扫描局域网"}
          </button>
          <button type="button" className={styles.addBtn} onClick={() => setShowAdd(true)}>
            <IconPlus size={13} />
            添加设备
          </button>
        </div>
      </header>

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
          <span className={styles.resumeGo}>进入 →</span>
        </button>
      )}

      {error !== null && <p className={styles.error}>{error}</p>}
      {notice !== null && <p className={styles.notice}>{notice}</p>}

      {devices.length >= 3 && (
        <input
          className={styles.deviceSearch}
          type="search"
          placeholder={`搜索 ${devices.length} 台设备（按 / 聚焦）…`}
          aria-label="搜索设备"
          value={deviceQuery}
          onChange={(e) => setDeviceQuery(e.currentTarget.value)}
        />
      )}

      {host !== null && (
        <div className={styles.hostCard}>
          <div className={styles.hostRow}>
            <strong>本机被控</strong>
          <span className={styles.hostSub}>这台电脑也可被其他设备远程控制</span>
            {host.paused && <span className={styles.hostPaused}>已暂停共享</span>}
            <span className={styles.hostSpacer} />
            <button type="button" className={styles.actionBtn} onClick={() => void onGenerateCode()}>
              生成配对码
            </button>
          </div>
          {host.listenAddrs.length > 0 && (
            <div className={styles.hostRow}>
              <span className={styles.hostLabel}>本机地址：</span>
              {host.listenAddrs.map((a) => (
                <code key={a} className={styles.hostAddr}>
                  <span className={styles.addrKind} data-tailscale={a.startsWith("100.")}>
                    {a.startsWith("100.") ? "Tailscale" : "局域网"}
                  </span>
                  {a}
                </code>
              ))}
            </div>
          )}
          {host.peers.length > 0 && (
            <div className={styles.hostRow}>
              <span className={styles.hostLabel}>正在被控制：</span>
              {host.peers.map((p) => (
                <span key={p.name} className={styles.hostPeer} title="对方开始控制本机的时长">
                  {p.name}
                  {p.sinceSecs > 0 &&
                    ` · ${p.sinceSecs >= 3600 ? `${Math.floor(p.sinceSecs / 3600)} 小时` : p.sinceSecs >= 60 ? `${Math.floor(p.sinceSecs / 60)} 分钟` : `${p.sinceSecs} 秒`}`}
                </span>
              ))}
              <button
                type="button"
                className={styles.cutBtn}
                title="立即断开对方的控制连接"
                disabled={cutting}
                onClick={() => void onCutPeers()}
              >
                {cutting ? "切断中…" : "切断"}
              </button>
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
          {pairCard !== null && (
            <div className={styles.pairBlock} data-expired={pairRemainSecs === 0}>
              <div className={styles.pairMain}>
                <span className={styles.hostLabel}>
                  {pairRemainSecs === 0
                    ? "配对码已过期，请重新生成"
                    : `对方添加设备时输入此配对码（还剩 ${Math.floor(pairRemainSecs / 60)}:${String(pairRemainSecs % 60).padStart(2, "0")}）`}
                </span>
                <code className={styles.pairCode} data-expired={pairRemainSecs === 0}>
                  {pairCard.code}
                </code>
                {pairRemainSecs === 0 && (
                  <button
                    type="button"
                    className={styles.actionBtn}
                    onClick={() => void onGenerateCode()}
                  >
                    重新生成
                  </button>
                )}
                {pairRemainSecs > 0 && (
                  <span className={styles.pairNote}>已复制到剪贴板</span>
                )}
              </div>
              <div className={styles.hostRow}>
                <span className={styles.hostLabel}>对方要连的地址：</span>
                {pairCard.addrs.map((a) => (
                  <code key={a} className={styles.hostAddr}>
                    <span className={styles.addrKind} data-tailscale={a.startsWith("100.")}>
                      {a.startsWith("100.") ? "Tailscale" : "局域网"}
                    </span>
                    {a}
                  </code>
                ))}
              </div>
            </div>
          )}
        </div>
      )}

      {linkTest !== null && (
        <pre className={styles.linkTestOut}>{linkTest.text}</pre>
      )}

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
          <p className={styles.emptyTitle}>还没有可控制的设备</p>
          <p className={styles.emptyHint}>
            两步开始：对方电脑装好 rd-agent → 在这里添加并输入对方托盘显示的 6 位配对码。
          </p>
          <div className={styles.emptyActions}>
            <button type="button" className={styles.addBtn} onClick={() => setShowAdd(true)}>
              手动添加设备
            </button>
            <button
              type="button"
              className={styles.actionBtn}
              onClick={() => void onScan()}
              disabled={scanning || busyId !== null}
            >
              {scanning ? "扫描中…" : "扫描局域网发现"}
            </button>
          </div>
        </div>
      ) : (
        <ul className={styles.list}>
          {shownDevices.map((d) => (
            <li key={d.id} className={styles.card}>
              <button
                type="button"
                className={styles.mainBtn}
                onClick={() => void onConnect(d)}
                disabled={busyId !== null}
                title={`连接 ${d.name}（指纹 ${d.fingerprintHex.slice(0, 16)}…）`}
              >
                <span className={styles.dot} data-online={d.online} aria-hidden="true" />
                <span className={styles.info}>
                  <span className={styles.name}>
                    {d.name}
                    <span className={styles.onlineTag} data-online={d.online}>
                      {d.online ? "在线" : "离线"}
                    </span>
                  </span>
                  <span className={styles.addr}>
                    {d.addr}
                    {getHostVersion(d.id) !== null && (
                      <span title="被控端 rd-agent 版本"> · v{getHostVersion(d.id)}</span>
                    )}
                  </span>
                </span>
                {busyId === d.id && <span className={styles.connecting}>连接中…</span>}
                {busyId !== d.id && (
                  <span className={styles.connectCta}>{d.online ? "连接" : "唤醒连接"}</span>
                )}
              </button>
              <div className={styles.actions}>
                {confirmRemoveId === d.id ? (
                  <>
                    <button
                      type="button"
                      className={`${styles.actionBtn} ${styles.confirmDelete}`}
                      onClick={() => {
                        if (confirmArmed) {
                          void onRemove(d);
                        }
                      }}
                      disabled={busyId !== null || !confirmArmed}
                      title={confirmArmed ? "此操作不可撤销" : "再按一次确认（防误触）"}
                    >
                      确认删除
                    </button>
                    <button
                      type="button"
                      className={styles.actionBtn}
                      onClick={() => setConfirmRemoveId(null)}
                    >
                      取消
                    </button>
                  </>
                ) : (
                  <>
                    <div className={styles.menuWrap}>
                      <button
                        type="button"
                        className={styles.moreBtn}
                        aria-label={d.name + " 的更多操作"}
                        title="更多操作"
                        aria-haspopup="menu"
                        aria-expanded={menuId === d.id}
                        onClick={() => setMenuId(menuId === d.id ? null : d.id)}
                      >
                        <IconMore size={16} />
                      </button>
                      {menuId === d.id && (
                        <div className={styles.menu} role="menu">
                          {d.online && (
                            <button
                              type="button"
                              className={styles.menuItem}
                              role="menuitem"
                              onClick={() => {
                                setMenuId(null);
                                void onDisconnect(d);
                              }}
                            >
                              断开连接
                            </button>
                          )}
                          <button
                            type="button"
                            className={styles.menuItem}
                            role="menuitem"
                            onClick={() => {
                              setMenuId(null);
                              void navigator.clipboard
                                .writeText(d.addr)
                                .then(() => setNotice(`已复制地址：${d.addr}`))
                                .catch(() => undefined);
                            }}
                          >
                            复制地址
                          </button>
                          <button
                            type="button"
                            className={styles.menuItem}
                            role="menuitem"
                            onClick={() => {
                              setMenuId(null);
                              void onProbe(d);
                            }}
                          >
                            连通测试
                          </button>
                          {d.addr.split(":")[0]?.startsWith("100.") && (
                            <button
                              type="button"
                              className={styles.menuItem}
                              role="menuitem"
                              onClick={() => {
                                setMenuId(null);
                                void onLinkTest(d);
                              }}
                              title="Tailscale 链路：直达或经中继"
                            >
                              线路测试
                            </button>
                          )}
                          <button
                            type="button"
                            className={styles.menuItem}
                            role="menuitem"
                            onClick={() => {
                              setMenuId(null);
                              void navigator.clipboard
                                .writeText(d.fingerprintHex)
                                .then(() =>
                                  setNotice(
                                    `「${d.name}」的设备指纹已复制（SHA-256 前 16 位：${d.fingerprintHex.slice(0, 16)}），可与对方核对`,
                                  ),
                                )
                                .catch(() => undefined);
                            }}
                            title="设备指纹是本连接的唯一凭证标识，可与对方屏幕上显示的指纹比对以防中间人"
                          >
                            查看指纹
                          </button>
                          <button
                            type="button"
                            className={styles.menuItem}
                            role="menuitem"
                            onClick={() => {
                              setMenuId(null);
                              onEdit(d);
                            }}
                          >
                            编辑
                          </button>
                          <button
                            type="button"
                            className={`${styles.menuItem} ${styles.menuItemDanger}`}
                            role="menuitem"
                            onClick={() => {
                              setMenuId(null);
                              setConfirmRemoveId(d.id);
                            }}
                          >
                            删除设备…
                          </button>
                        </div>
                      )}
                    </div>
                  </>
                )}
              </div>
            </li>
          ))}
        </ul>
      )}

      {discovered !== null && discovered.length > 0 && (
        <section className={styles.scanSection} aria-label="局域网发现的设备">
          <h2 className={styles.scanHeading}>
            局域网发现（未配对 · 添加后仍需对方托盘配对码）
          </h2>
          <ul className={styles.list}>
            {discovered.map((d) => (
              <li key={d.addr} className={`${styles.card} ${styles.scanCard}`}>
                <button
                  type="button"
                  className={styles.mainBtn}
                  onClick={() => onPickDiscovered(d)}
                  title="添加该设备（需再输托盘配对码）"
                >
                  <span className={`${styles.dot} ${styles.dotScan}`} aria-hidden="true" />
                  <span className={styles.info}>
                    <span className={styles.name}>
                      {d.name}
                      <span className={styles.unpairedTag}>未配对</span>
                    </span>
                    <span className={styles.addr}>{d.addr}</span>
                  </span>
                </button>
                <div className={styles.actions}>
                  <button
                    type="button"
                    className={styles.actionBtn}
                    onClick={() => onPickDiscovered(d)}
                  >
                    添加
                  </button>
                </div>
              </li>
            ))}
          </ul>
        </section>
      )}

      <footer className={styles.hint}>
        <span className={styles.footerRow}>
          <button
            type="button"
            className={styles.helpToggle}
            aria-expanded={helpOpen}
            onClick={() => setHelpOpen((v) => !v)}
          >
            连不上对方？查看连接指南
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
            title="画质方案对比调试工具（高级）"
          >
            <IconLab size={12} />
            方案实验室
          </button>
        </span>
        {helpOpen && (
          <ul className={styles.helpList}>
            <li>
              <b>同一 WiFi / 局域网：</b>
              点「扫描局域网」自动发现对方，或直接填对方内网 IP（如 192.168.1.23）。
            </li>
            <li>
              <b>跨网络（不在同一网络）：</b>
              两台电脑都装并登录 Tailscale，互相可见后再连接。
              <button
                type="button"
                className={styles.linkBtn}
                onClick={() => void openUrl("https://login.tailscale.com/admin/users").catch(() => undefined)}
              >
                打开 Tailscale 管理后台
              </button>
              （邀请对方：Machines → Share → 生成邀请链接发给对方。）
            </li>
            <li>
              <b>已用 Tailscale 还是连不上：</b>
              确认两台电脑登录的是<strong>同一个账号网络（tailnet）</strong>，且对方 rd-agent 正在运行（托盘图标）。
            </li>
          </ul>
        )}
        <span className={styles.version}>rd-controller v{APP_VERSION}</span>
      </footer>

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
              <span>地址</span>
              <input
                className={styles.input}
                value={form.addr}
                onChange={(e) => updateForm("addr", e.currentTarget.value)}
                placeholder="托盘弹窗里的地址，如 100.x.y.z:21118 或 192.168.1.10"
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
                  placeholder="被控端托盘显示的 6 位数字"
                  inputMode="numeric"
                  maxLength={6}
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
