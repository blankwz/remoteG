import React from "react";
import ReactDOM from "react-dom/client";
import { applyUiScale, loadUiScale } from "./ui/uiScale";

// 全局 UI 缩放（P1-4）：首屏渲染前应用已保存档位，避免闪一帧默认字号
applyUiScale(loadUiScale());

interface BoundaryState {
  err: string | null;
}

class RootErrorBoundary extends React.Component<{ children: React.ReactNode }, BoundaryState> {
  state: BoundaryState = { err: null };

  static getDerivedStateFromError(error: unknown): BoundaryState {
    return {
      err: error instanceof Error ? `${error.name}: ${error.message}\n${error.stack ?? ""}` : String(error),
    };
  }

  render() {
    if (this.state.err !== null) {
      return (
        <pre style={{ padding: "20px", color: "#ef6f6f", whiteSpace: "pre-wrap" }}>
          {this.state.err}
        </pre>
      );
    }
    return this.props.children;
  }
}

const rootEl = document.getElementById("root") as HTMLElement;

void import("./App")
  .then(({ default: App }) => {
    ReactDOM.createRoot(rootEl).render(
      <React.StrictMode>
        <RootErrorBoundary>
          <App />
        </RootErrorBoundary>
      </React.StrictMode>,
    );
  })
  .catch((err: unknown) => {
    const msg = err instanceof Error ? `${err.name}: ${err.message}\n${err.stack ?? ""}` : String(err);
    rootEl.innerHTML = `<pre style="padding:20px;color:#ef6f6f;white-space:pre-wrap">${msg}</pre>`;
  });
