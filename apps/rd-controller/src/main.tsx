import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { applyUiScale, loadUiScale } from "./ui/uiScale";

// 全局 UI 缩放（P1-4）：首屏渲染前应用已保存档位，避免闪一帧默认字号
applyUiScale(loadUiScale());

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
