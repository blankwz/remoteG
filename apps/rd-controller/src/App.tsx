/**
 * rd-controller 前端入口（T-005）。
 *
 * hash 路由：#/spike（视频链路 spike）/ #/devices / #/windows/:id / #/session/:id。
 * /windows 由 T-020 挂载真实页面；/devices、/session 仍为占位（T-019 遗留 / T-021 实现）。
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { probeDecodeCaps, setDecodeCaps } from "./api/devices";
import { spikeConnect } from "./api/spike";
import { Hud } from "./hud/Hud";
import { Windows } from "./pages/Windows";
import type { DecodeStats } from "./video/decoder";
import { H264Decoder } from "./video/decoder";
import { VideoCanvas } from "./video/VideoCanvas";
import styles from "./App.module.css";

type Route =
  | { page: "spike" }
  | { page: "devices" }
  | { page: "windows"; deviceId: string }
  | { page: "session"; deviceId: string };

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

function Placeholder({ title, task }: { title: string; task: string }) {
  return (
    <main className={styles.placeholder}>
      <h1>{title}</h1>
      <p>占位页，由 {task} 实现。</p>
    </main>
  );
}

export default function App() {
  const [route, setRoute] = useState<Route>(() => parseRoute(window.location.hash));

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
      <nav className={styles.nav}>
        <a href="#/spike">Spike</a>
        <a href="#/devices">Devices</a>
        <a href="#/windows/demo">Windows</a>
        <a href="#/session/demo">Session</a>
      </nav>
      {route.page === "spike" && <SpikePage />}
      {route.page === "devices" && (
        <Placeholder title="Devices" task="T-019" />
      )}
      {route.page === "windows" && <Windows deviceId={route.deviceId} />}
      {route.page === "session" && (
        <Placeholder title={`Session (${route.deviceId})`} task="T-021" />
      )}
    </div>
  );
}
