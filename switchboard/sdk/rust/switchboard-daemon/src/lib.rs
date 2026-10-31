//! Switchboard Daemon：合成会话引擎的 HTTP/WS 门面。
//!
//! 端点：
//! - `POST /api/session`   body = SimParams(JSON) → {id}
//! - `DELETE /api/session/{id}` → 停止
//! - `GET  /api/sessions`  → 在跑会话列表
//! - `GET  /api/live/{id}` → WebSocket：Telem JSON 流
//! - `GET  /healthz`       → ok
//!
//! 用法：`switchboard-daemon [--port 7500]`

use anyhow::Context as _;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get_service;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::SinkExt;
use sim::{SessionHandle, SimParams, Telem};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tower_http::services::{ServeDir, ServeFile};

pub mod sim;

type Shared = Arc<Mutex<HashMap<u64, Arc<SessionHandle>>>>;

#[derive(Clone)]
pub struct AppState {
    pub sessions: Shared,
    pub bench_dir: Arc<Option<String>>,
}

/// Studio 界面产物目录探测：相对路径随启动目录漂移（cwd 可能是 exe 目录、
/// 安装后的 Program Files 甚至 system32），按候选顺序找第一个含 index.html
/// 的。外部宿主（rd-controller 内嵌）也用它。
pub fn default_static_dir() -> Option<String> {
    // exe 相对候选（安装版：资源随安装包落在 exe 旁；review R1 修复）
    let exe_rel: Vec<std::path::PathBuf> = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .map(|dir| {
            ["studio/dist", "../../studio/dist"]
                .iter()
                .map(|c| dir.join(c))
                .collect()
        })
        .unwrap_or_default();
    let cwd_rel = [
        "switchboard/studio/dist",
        "../studio/dist",
        "../../studio/dist",
        "../../../studio/dist",
        // 双击 target/release/switchboard-daemon.exe 时 cwd = release 目录
        "../../../../studio/dist",
    ];
    let candidates: Vec<std::path::PathBuf> = exe_rel
        .into_iter()
        .chain(cwd_rel.iter().map(std::path::PathBuf::from))
        .collect();
    candidates
        .iter()
        .find(|c| c.join("index.html").exists())
        .map(|c| c.to_string_lossy().to_string())
}

/// 启动 HTTP/WS 服务（阻塞当前任务直到出错；宿主自行 spawn）。
/// `static_dir` 传 `None` 时自动探测 Studio 产物（找不到则仅提供 API）。
pub async fn serve(
    port: u16,
    bench_dir: Option<String>,
    static_dir: Option<String>,
) -> anyhow::Result<()> {
    let static_dir = match static_dir {
        Some(d) => Some(d),
        None => default_static_dir(),
    };
    let bench_dir = Arc::new(bench_dir);
    let state = AppState {
        sessions: Arc::new(Mutex::new(HashMap::new())),
        bench_dir,
    };
    let mut app = build_router(state);

    // 静态托管 Studio 界面（存在 dist 时）：一个端口 = 界面 + 数据
    if let Some(dir) = &static_dir {
        if std::path::Path::new(dir).join("index.html").exists() {
            let serve = ServeDir::new(dir)
                .append_index_html_on_directories(true)
                .fallback(ServeFile::new(std::path::Path::new(dir).join("index.html")));
            app = app.fallback_service(get_service(serve));
        } else {
            eprintln!(
                "[提示] 未找到界面产物 {dir}/index.html（先执行 pnpm -C switchboard/studio build）；本次仅提供 API"
            );
        }
    }

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("绑定 127.0.0.1:{port}"))?;
    println!("switchboard-daemon listening on http://127.0.0.1:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// API 路由（main 与安全测试共用；静态托管在 main 里按需追加）。
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/session", post(start_session))
        .route("/api/session/{id}", delete(stop_session))
        .route("/api/sessions", get(list_sessions))
        .route("/api/live/{id}", get(live_ws))
        .route("/api/bench/list", get(list_bench))
        .route("/api/bench/{name}", get(get_bench))
        .with_state(state)
}

fn validate_params(p: &SimParams) -> Result<(), String> {
    // QA #1/#6 + 审查 #6：入口钳制，杜绝无限会话与越界参数
    if !p.duration_s.is_finite() || !(0.1..=3600.0).contains(&p.duration_s) {
        return Err("duration_s 需在 0.1~3600 秒".into());
    }
    if !p.rtt_ms.is_finite() || !(0.0..=2000.0).contains(&p.rtt_ms) {
        return Err("rtt_ms 需在 0~2000".into());
    }
    if !p.loss.is_finite() || !(0.0..=1.0).contains(&p.loss) {
        return Err("loss 需在 0~1".into());
    }
    if !p.mbps.is_finite() || !(0.1..=10000.0).contains(&p.mbps) {
        return Err("mbps 需在 0.1~10000".into());
    }
    if p.burst_len > 1000 {
        return Err("burst_len 需 ≤1000".into());
    }
    Ok(())
}

async fn start_session(
    State(state): State<AppState>,
    Json(params): Json<SimParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if let Err(msg) = validate_params(&params) {
        return Err((StatusCode::BAD_REQUEST, msg));
    }
    let handle = Arc::new(sim::start(params));
    let id = handle.id;
    state.sessions.lock().unwrap().insert(id, handle);
    Ok(Json(serde_json::json!({ "id": id })))
}

async fn stop_session(State(state): State<AppState>, Path(id): Path<u64>) -> impl IntoResponse {
    let removed = state.sessions.lock().unwrap().remove(&id);
    if let Some(h) = removed {
        h.stop();
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn list_sessions(State(state): State<AppState>) -> Json<serde_json::Value> {
    let map = state.sessions.lock().unwrap();
    // QA #4：只列出未结束的会话（引擎结束置 done）
    let items: Vec<serde_json::Value> = map
        .values()
        .filter(|h| !h.done.load(std::sync::atomic::Ordering::SeqCst))
        .map(|h| serde_json::json!({ "id": h.id, "params": h.params }))
        .collect();
    Json(serde_json::json!({ "sessions": items }))
}

/// 列出 bench 目录下的 bench.json 文件（--bench-dir 启用时）。
async fn list_bench(State(state): State<AppState>) -> impl IntoResponse {
    match state.bench_dir.as_ref() {
        Some(dir) => {
            let mut files = Vec::new();
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    if e.path().extension().is_some_and(|x| x == "json") {
                        files.push(e.file_name().to_string_lossy().to_string());
                    }
                }
            }
            files.sort();
            Json(serde_json::json!({ "files": files }))
        }
        None => Json(serde_json::json!({ "files": [] })),
    }
}

/// 读取单个 bench.json。
async fn get_bench(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // 防目录穿越：只允许文件名
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err((StatusCode::BAD_REQUEST, "非法文件名".into()));
    }
    let Some(dir) = state.bench_dir.as_ref() else {
        return Err((StatusCode::NOT_FOUND, "未启用 --bench-dir".into()));
    };
    // 审查 #10：文件名白名单（防目录穿越与盘符相对路径），404 不透传 OS 原文
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        || name.contains("..")
    {
        return Err((StatusCode::BAD_REQUEST, "非法文件名".into()));
    }
    let raw = std::fs::read_to_string(std::path::Path::new(dir).join(&name))
        .map_err(|_| (StatusCode::NOT_FOUND, "报告不存在".to_string()))?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(v))
}

async fn live_ws(
    State(state): State<AppState>,
    Path(id): Path<u64>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    let handle = state.sessions.lock().unwrap().get(&id).cloned();
    upgrade.on_upgrade(move |socket| live_stream(state, id, socket, handle))
}

async fn live_stream(
    state: AppState,
    id: u64,
    mut socket: WebSocket,
    handle: Option<Arc<SessionHandle>>,
) {
    let Some(handle) = handle else {
        let _ = socket.send(Message::Text("no such session".into())).await;
        return;
    };
    let mut rx = handle.telemetry_rx.resubscribe();
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(telem) => {
                        let json = match &telem {
                            Telem::Stats { .. } | Telem::Event { .. } | Telem::Ended { .. } => {
                                serde_json::to_string(&telem).unwrap_or_default()
                            }
                        };
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                        if matches!(telem, Telem::Ended { .. }) {
                            let _ = socket.send(Message::Text("__end__".into())).await;
                            let _ = socket.close().await; // QA #7：补 CLOSE 帧（1000）
                            // 审查 #4/#5：会话结束即从列表清理（观察者断开不影响会话）
                            state.sessions.lock().unwrap().remove(&id);
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let payload = serde_json::json!({ "type": "lagged", "n": n }).to_string();
                        let _ = socket.send(Message::Text(payload.into())).await;
                    }
                    Err(_) => break,
                }
            }
            // 审查 #4：观察者断开只退订，不停会话（停止唯一入口 = DELETE）
            _ = socket.recv() => {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 安全测试（09_test Phase 4：4.7 目录穿越 / 4.8 参数注入，接口层自动化）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod security_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // oneshot

    fn app() -> Router {
        build_router(AppState {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            bench_dir: Arc::new(None),
        })
    }

    async fn get(uri: &str) -> axum::response::Response {
        app()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    /// 4.7 目录穿越：`..`、路径分隔、盘符相对路径一律 400，绝不落盘读取。
    /// axum 的 Path 提取器会对 %2F 解码——穿越载荷按解码后形态到达 handler。
    #[tokio::test]
    async fn bench_name_traversal_rejected() {
        for payload in [
            "/api/bench/..%2F..%2Fetc%2Fpasswd", // ../ → 含 / 与 ..
            "/api/bench/..%5C..%5Cwin.ini",      // ..\ → 含 \ 与 ..
            "/api/bench/..",                     // 裸 ..
            "/api/bench/a..b",                   // 内嵌 ..（白名单也拦）
        ] {
            let resp = get(payload).await;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "穿越载荷 {payload} 必须 400"
            );
        }
        // 合法文件名但未启用 bench-dir → 404（不透传 OS 错误原文）
        let resp = get("/api/bench/report.json").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// 4.8 参数注入：越界/NaN 参数一律 400 拒绝；非法 JSON 由 axum 422 拒绝；
    /// 全部优雅返回，不崩进程、不创建会话。
    #[tokio::test]
    async fn session_params_injection_rejected() {
        // 越界但合法 JSON：loss>1 / duration 超限 / burst 超限
        for body in [
            r#"{"rtt_ms":40,"loss":5.0,"mbps":20,"burst_len":0,"fec_rs":true,"duration_s":10}"#,
            r#"{"rtt_ms":40,"loss":0.01,"mbps":20,"burst_len":0,"fec_rs":true,"duration_s":99999}"#,
            r#"{"rtt_ms":40,"loss":0.01,"mbps":20,"burst_len":99999,"fec_rs":true,"duration_s":10}"#,
        ] {
            let resp = app()
                .oneshot(
                    Request::post("/api/session")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "越界参数必须 400: {body}"
            );
        }
        // 非法 JSON（NaN 字面量）：axum Json 提取 422，进程不崩
        let resp = app()
            .oneshot(
                Request::post("/api/session")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"loss":NaN}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "非法 JSON 必须优雅拒绝"
        );
        // 会话表保持为空（拒绝路径不得留下半成品会话）
        let resp = get("/api/sessions").await;
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        assert!(
            bytes.windows(3).any(|w| w == b"[]")
                || serde_json::from_slice::<serde_json::Value>(&bytes)
                    .map(|v| v["sessions"]
                        .as_array()
                        .map(|a| a.is_empty())
                        .unwrap_or(true))
                    .unwrap_or(true),
            "拒绝后不得残留会话: {bytes:?}"
        );
    }

    /// 4.8 未知资源：stop/live 打不存在的 id → 404/错误消息，不 panic。
    #[tokio::test]
    async fn unknown_session_handled() {
        let _resp = get("/api/session/999").await;
        // DELETE /api/session/{id}：不存在 → 404
        let resp = app()
            .oneshot(
                Request::delete("/api/session/999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
