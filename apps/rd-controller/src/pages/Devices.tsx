/**
 * 设备页（T-019）：设备卡片（在线圆点/指纹前 8 字节）+ “添加设备”对话框
 * （地址可填 MagicDNS 名，默认端口 21118 由 Rust 侧补全）+ 连接跳转 /devices → /windows/:id。
 */
import { useCallback, useEffect, useState, type FormEvent } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  AGENT_DEFAULT_PORT,
  connectDevice,
  disconnectDevice,
  listDevices,
  pairDevice,
  probeDevice,
  removeDevice,
  type DeviceDto,
} from "../api/devices";
import styles from "./Devices.module.css";

interface DisconnectedPayload {
  deviceId: string;
}

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

  const refresh = useCallback(async () => {
    try {
      setDevices(await listDevices());
      setError(null);
    } catch (e) {
      setError(errText(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
    // ConnManager 断线（含主动断开）广播 → 刷新在线圆点
    const unlisten = listen<DisconnectedPayload>("device-disconnected", () => {
      void refresh();
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, [refresh]);

  const onConnect = useCallback(
    async (d: DeviceDto) => {
      setBusyId(d.id);
      setError(null);
      try {
        await connectDevice(d.id);
        window.location.hash = `#/windows/${d.id}`;
        setNotice(null);
      } catch (e) {
        setError(errText(e));
        await refresh();
      } finally {
        setBusyId(null);
      }
    },
    [refresh],
  );

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

  const onRemove = useCallback(
    async (d: DeviceDto) => {
      setBusyId(d.id);
      try {
        await removeDevice(d.id);
        setNotice(`已删除设备「${d.name}」`);
        await refresh();
      } catch (e) {
        setError(errText(e));
      } finally {
        setBusyId(null);
      }
    },
    [refresh],
  );

  const submitAdd = useCallback(
    async (e: FormEvent) => {
      e.preventDefault();
      setFormError(null);
      setPairing(true);
      try {
        await pairDevice(form.addr, form.code, form.name);
        setShowAdd(false);
        setForm({ addr: "127.0.0.1:21118", code: "", name: "" });
        await refresh();
      } catch (e) {
        setFormError(errText(e));
      } finally {
        setPairing(false);
      }
    },
    [form, refresh],
  );

  const closeAdd = useCallback(() => {
    setShowAdd(false);
    setFormError(null);
  }, []);

  const updateForm = useCallback(
    (field: "addr" | "code" | "name", value: string) => {
      setForm((prev) => ({ ...prev, [field]: value }));
    },
    [],
  );

  return (
    <main className={styles.page}>
      <header className={styles.header}>
        <h1 className={styles.title}>设备</h1>
        <button type="button" className={styles.addBtn} onClick={() => setShowAdd(true)}>
          添加设备
        </button>
      </header>

      {error !== null && <p className={styles.error}>{error}</p>}
      {notice !== null && <p className={styles.notice}>{notice}</p>}

      {devices.length === 0 ? (
        <p className={styles.empty}>
          还没有设备。点击右上角“添加设备”，输入被控端地址与托盘显示的 6 位配对码。
        </p>
      ) : (
        <ul className={styles.list}>
          {devices.map((d) => (
            <li key={d.id} className={styles.card}>
              <button
                type="button"
                className={styles.mainBtn}
                onClick={() => void onConnect(d)}
                disabled={busyId !== null}
                title="连接设备"
              >
                <span className={styles.dot} data-online={d.online} aria-hidden="true" />
                <span className={styles.info}>
                  <span className={styles.name}>{d.name}</span>
                  <span className={styles.addr}>{d.addr}</span>
                </span>
                <code className={styles.fp} title="对端证书指纹（前 8 字节）">
                  {d.fingerprintHex.slice(0, 16)}
                </code>
              </button>
              <div className={styles.actions}>
                <button
                  type="button"
                  className={styles.actionBtn}
                  onClick={() => void onProbe(d)}
                  disabled={busyId !== null}
                >
                  测试
                </button>
                <button
                  type="button"
                  className={styles.actionBtn}
                  onClick={() => void onDisconnect(d)}
                  disabled={busyId !== null}
                >
                  断开
                </button>
                <button
                  type="button"
                  className={`${styles.actionBtn} ${styles.danger}`}
                  onClick={() => void onRemove(d)}
                  disabled={busyId !== null}
                >
                  删除
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}

      {showAdd && (
        <div className={styles.overlay} onClick={closeAdd}>
          <form
            className={styles.dialog}
            onClick={(e) => e.stopPropagation()}
            onSubmit={(e) => void submitAdd(e)}
          >
            <h2 className={styles.dialogTitle}>添加设备</h2>
            <label className={styles.field}>
              <span>地址</span>
              <input
                className={styles.input}
                value={form.addr}
                onChange={(e) => updateForm("addr", e.currentTarget.value)}
                placeholder={`my-pc:${AGENT_DEFAULT_PORT} 或 192.168.1.10`}
                spellCheck={false}
                autoFocus
              />
            </label>
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
                {pairing ? "配对中…" : "配对"}
              </button>
            </div>
          </form>
        </div>
      )}
    </main>
  );
}
