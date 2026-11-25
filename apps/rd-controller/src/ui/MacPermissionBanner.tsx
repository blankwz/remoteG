/**
 * macOS 首次启动《隐私与安全性》权限引导横幅。
 *
 * 仅在 macOS 且缺少「屏幕录制」或「辅助功能」任一权限时显示；
 * Windows / 已全部授权的 Mac 渲染 null，零视觉干扰。
 */
import { useCallback, useEffect, useState } from "react";
import { call } from "../api/bridge";

export interface MacPermissionsDto {
  isMac: boolean;
  screenRecording: boolean;
  accessibility: boolean;
}

export function MacPermissionBanner() {
  const [perms, setPerms] = useState<MacPermissionsDto | null>(null);
  const [dismissed, setDismissed] = useState(false);

  const check = useCallback(() => {
    void call<MacPermissionsDto>("get_mac_permissions")
      .then((res) => {
        if (res && typeof res.isMac === "boolean") {
          setPerms(res);
        }
      })
      .catch(() => undefined);
  }, []);

  useEffect(() => {
    check();
  }, [check]);

  if (dismissed || perms === null || !perms.isMac) {
    return null;
  }
  if (perms.screenRecording && perms.accessibility) {
    return null;
  }

  const openPane = (pane: "screen" | "accessibility") => {
    void call<void>("open_mac_privacy_settings", { pane }).catch(() => undefined);
  };

  return (
    <div
      role="region"
      aria-label="macOS 隐私与安全性权限引导"
      style={{
        display: "flex",
        flexWrap: "wrap",
        alignItems: "center",
        justifyContent: "space-between",
        gap: "10px",
        padding: "10px 16px",
        background: "rgba(245, 158, 11, 0.14)",
        borderBottom: "1px solid rgba(245, 158, 11, 0.35)",
        fontSize: "13px",
      }}
    >
      <div style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
        <strong>
          macOS 首次使用提示：请前往「系统设置 → 隐私与安全性」开启权限（勾选后重启应用生效）
        </strong>
        <span>
          屏幕录制：{perms.screenRecording ? "✅ 已授权" : "⚠️ 未授权（共享窗口画面必需）"} · 辅助功能：
          {perms.accessibility ? "✅ 已授权" : "⚠️ 未授权（远程键鼠操控必需）"}
        </span>
      </div>
      <div style={{ display: "flex", flexWrap: "wrap", gap: "6px" }}>
        {!perms.screenRecording && (
          <button
            type="button"
            onClick={() => openPane("screen")}
            style={{
              padding: "5px 10px",
              borderRadius: "6px",
              border: "1px solid rgba(245, 158, 11, 0.5)",
              background: "rgba(245, 158, 11, 0.22)",
              color: "inherit",
              cursor: "pointer",
            }}
          >
            打开「屏幕录制」设置
          </button>
        )}
        {!perms.accessibility && (
          <button
            type="button"
            onClick={() => openPane("accessibility")}
            style={{
              padding: "5px 10px",
              borderRadius: "6px",
              border: "1px solid rgba(245, 158, 11, 0.5)",
              background: "rgba(245, 158, 11, 0.22)",
              color: "inherit",
              cursor: "pointer",
            }}
          >
            打开「辅助功能」设置
          </button>
        )}
        <button
          type="button"
          onClick={check}
          style={{
            padding: "5px 10px",
            borderRadius: "6px",
            border: "1px solid rgba(255, 255, 255, 0.2)",
            background: "transparent",
            color: "inherit",
            cursor: "pointer",
          }}
        >
          重新检测
        </button>
        <button
          type="button"
          onClick={() => setDismissed(true)}
          style={{
            padding: "5px 10px",
            borderRadius: "6px",
            border: "1px solid rgba(255, 255, 255, 0.14)",
            background: "transparent",
            color: "inherit",
            cursor: "pointer",
          }}
        >
          暂不提示
        </button>
      </div>
    </div>
  );
}
