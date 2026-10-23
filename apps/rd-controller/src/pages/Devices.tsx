/**
 * 设备页（T-019）：设备卡片 + 「添加设备」对话框 + 局域网扫描（D-013 包1：
 * 免手填 IP）+ 指纹不匹配一键重配对 + Tailscale 节点共享引导。
 */
import { useCallback, useEffect, useState, type FormEvent } from "react";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  clearLastDeviceId,
  connectDevice,
  disconnectDevice,
  discoverDevices,
  getLastDeviceId,
  listDevices,
  pairDevice,
  probeDevice,
  rememberLastDeviceId,
  removeDevice,
  type DeviceDto,
  type DiscoveredDeviceDto,
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
  /** 局域网扫描结果（发现 ≠ 信任，点击仅预填添加对话框）。 */
  const [discovered, setDiscovered] = useState<DiscoveredDeviceDto[] | null>(null);
  const [scanning, setScanning] = useState(false);

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
        // 卡片显示在线时跳过 connect：后端 connect_device 对在线设备做连接
        // 替换（关旧连新），旧连接的断线监听会把新连接误清出连接表（conn.rs
        // put_device 竞态），导致随后的窗口列表报“设备未连接”
        if (!d.online) {
          await connectDevice(d.id);
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
          setFormError("对端证书已变化（可能已重装）。请让对方在托盘点「生成配对码」，输入新配对码完成重新配对。");
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
        <div className={styles.headerActions}>
          <button
            type="button"
            className={styles.actionBtn}
            onClick={() => void onScan()}
            disabled={scanning || busyId !== null}
          >
            {scanning ? "扫描中…" : "扫描局域网"}
          </button>
          <button type="button" className={styles.addBtn} onClick={() => setShowAdd(true)}>
            添加设备
          </button>
        </div>
      </header>

      {error !== null && <p className={styles.error}>{error}</p>}
      {notice !== null && <p className={styles.notice}>{notice}</p>}

      {devices.length === 0 ? (
        <p className={styles.empty}>
          还没有设备。点「扫描局域网」自动发现同网段被控电脑，或点「添加设备」手动输入地址与托盘显示的 6 位配对码。
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
                title={`连接 ${d.name}（指纹 ${d.fingerprintHex.slice(0, 16)}…）`}
              >
                <span className={styles.dot} data-online={d.online} aria-hidden="true" />
                <span className={styles.info}>
                  <span className={styles.name}>{d.name}</span>
                  <span className={styles.addr}>{d.addr}</span>
                </span>
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
          {discovered !== null && discovered.length > 0 && (
            <>
              <li className={styles.scanHeading}>
                局域网发现（发现≠信任，添加后仍需托盘配对码）
              </li>
              {discovered.map((d) => (
                <li key={d.addr} className={styles.card}>
                  <button
                    type="button"
                    className={styles.mainBtn}
                    onClick={() => onPickDiscovered(d)}
                    title="添加该设备（需再输托盘配对码）"
                  >
                    <span className={`${styles.dot} ${styles.dotScan}`} aria-hidden="true" />
                    <span className={styles.info}>
                      <span className={styles.name}>{d.name}</span>
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
            </>
          )}
        </ul>
      )}

      <footer className={styles.hint}>
        对方没装 Tailscale？同一 WiFi 可直接用局域网 IP 连接（先「扫描局域网」）。跨网络用
        Tailscale：
        <button
          type="button"
          className={styles.linkBtn}
          onClick={() => void openUrl("https://login.tailscale.com/admin/users").catch(() => undefined)}
        >
          管理后台邀请
        </button>
        （登录后在 Machines 里选你这台 → Share → 生成邀请链接发给对方，对方点开用自己的账号
        接受，无需登你的账号）。注意：两台电脑必须在<strong>同一个 tailnet</strong> 里才能互通。
      </footer>

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
                placeholder="托盘弹窗里的地址，如 100.x.y.z:21118 或 192.168.1.10"
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
