/**
 * rd-controller 前端入口（T-005）。
 *
 * hash 路由：#/spike（M0 视频链路 spike，调试用：已从导航下架，直达路由保留）/
 * #/devices / #/windows/:id / #/session/:id。
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { getLastDeviceId, probeDecodeCaps, setDecodeCaps } from "./api/devices";
import { spikeConnect } from "./api/spike";
import { Hud } from "./hud/Hud";
import { Devices } from "./pages/Devices";
import { Session } from "./pages/Session";
import { Windows } from "./pages/Windows";
import { Settings } from "./pages/Settings";
import type { DecodeStats } from "./video/decoder";
import { H264Decoder } from "./video/decoder";
import { VideoCanvas } from "./video/VideoCanvas";
import { IconGear } from "./ui/Icon";
import { UpdateBanner } from "./ui/UpdateBanner";
import styles from "./App.module.css";

type Route =
  | { page: "spike" }
  | { page: "devices" }
  | { page: "windows"; deviceId: string }
  | { page: "session"; deviceId: string }
  | { page: "settings" };

function parseRoute(hash: string): Route {
  const parts = hash
    .replace(/^#/, "")
    .split("/")
    // 路由段剥离查询串（如 #/session/:id?open=<windowId>），查询参数由页面自取
    .map((s) => s.split("?")[0] ?? "")
    .filter((s) => s.length > 0);
  switch (parts[0]) {
    case "spike":
      return { page: "spike" };
    case "settings":
      return { page: "settings" };
    case "windows":
      if (parts[1]) return { page: "windows", deviceId: parts[1] };
      break;
    case "session":
      if (parts[1]) return { page: "session", deviceId: parts[1] };
      break;
    default:
      break;
  }
  return { page: "devices" };
}

const DEFAULT_SPIKE_ADDR = "127.0.0.1:21118";
const EMPTY_STATS: DecodeStats = { fps: 0, decodeMs: 0, width: 0, height: 0 };
/** pts 差刷新周期。 */
const PTS_DRIFT_POLL_MS = 500;

type SpikeStatus = "idle" | "connecting" | "connected" | "error";

/** /spike：地址输入 + 连接 + VideoCanvas + Hud。 */
function SpikePage() {
  const decoderRef = useRef<H264Decoder | null>(null);
  const [stats, setStats] = useState<DecodeStats>(EMPTY_STATS);
  const [addr, setAddr] = useState(DEFAULT_SPIKE_ADDR);
  const [status, setStatus] = useState<SpikeStatus>("idle");
  const [error, setError] = useState<string | null>(null);
  const [ptsDriftUs, setPtsDriftUs] = useState<number | null>(null);

  // 定时取最近帧头部，估算 pts 与本地时钟差（两端未同步，仅参考）
  useEffect(() => {
    const timer = setInterval(() => {
      const header = decoderRef.current?.lastHeader();
      setPtsDriftUs(header ? Date.now() * 1000 - Number(header.ptsUs) : null);
    }, PTS_DRIFT_POLL_MS);
    return () => clearInterval(timer);
  }, []);

  const onStats = useCallback((s: DecodeStats) => setStats(s), []);

  const connect = useCallback(() => {
    setStatus("connecting");
    setError(null);
    spikeConnect(addr, (frame) => decoderRef.current?.push(frame))
      .then(() => {
        setStatus("connected");
      })
      .catch((e: unknown) => {
        setStatus("error");
        setError(e instanceof Error ? e.message : String(e));
      });
  }, [addr]);

  return (
    <main className={styles.spike}>
      <form
        className={styles.connectBar}
        onSubmit={(e) => {
          e.preventDefault();
          connect();
        }}
      >
        <input
          className={styles.addrInput}
          value={addr}
          onChange={(e) => setAddr(e.currentTarget.value)}
          placeholder="agent addr (ip:port)"
          spellCheck={false}
        />
        <button
          className={styles.connectBtn}
          type="submit"
          disabled={status === "connecting" || addr.trim().length === 0}
        >
          {status === "connecting" ? "connecting..." : "connect"}
        </button>
        <span className={styles.statusText} data-status={status}>
          {status}
        </span>
      </form>
      {error !== null && <p className={styles.error}>{error}</p>}
      <div className={styles.stage}>
        <VideoCanvas decoderRef={decoderRef} onStats={onStats} />
      </div>
      <Hud stats={stats} ptsDriftUs={ptsDriftUs} />
    </main>
  );
}

export default function App() {
  const [route, setRoute] = useState<Route>(() =>
    parseRoute(window.location.hash),
  );

  useEffect(() => {
    const onHashChange = () => setRoute(parseRoute(window.location.hash));
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  // T-019 接线：启动时探测 WebCodecs 解码能力并上报后端（连接握手时填入 Hello）
  useEffect(() => {
    probeDecodeCaps()
      .then((caps) => setDecodeCaps(caps).catch(() => undefined))
      .catch(() => undefined);
  }, []);

  return (
    <div className={styles.app}>
      <Nav active={route.page} />
      <UpdateBanner />
      {route.page === "spike" && <SpikePage />}
      {route.page === "devices" && <Devices />}
      {route.page === "windows" && <Windows deviceId={route.deviceId} />}
      {route.page === "session" && <Session deviceId={route.deviceId} />}
      {route.page === "settings" && <Settings />}
    </div>
  );
}

/** 导航上「窗口列表 / 会话」指向的最后连接设备 id；没有时链接禁用。 */
function Nav({ active }: { active: Route["page"] }) {
  const lastDeviceId = getLastDeviceId();
  const linkClass = (page: Route["page"]): string =>
    active === page
      ? `${styles.navLink} ${styles.navLinkActive}`
      : styles.navLink;
  return (
    <nav className={styles.nav}>
      <a className={styles.brand} href="#/devices" title="返回设备页">
        <span className={styles.brandMark} aria-hidden="true" />
        rd-controller
      </a>
      <div className={styles.navLinks}>
        <a className={linkClass("devices")} href="#/devices">
          设备
        </a>
        <a
          className={linkClass("settings")}
          href="#/settings"
          title="设置"
          aria-label="设置"
        >
          <IconGear size={14} />
          设置
        </a>
        {active === "devices" && lastDeviceId !== null && (
          <>
            <a
              className={linkClass("windows")}
              href={`#/windows/${lastDeviceId}`}
            >
              窗口列表
            </a>
            <a
              className={linkClass("session")}
              href={`#/session/${lastDeviceId}`}
            >
              会话
            </a>
          </>
        )}
      </div>
    </nav>
  );
}
