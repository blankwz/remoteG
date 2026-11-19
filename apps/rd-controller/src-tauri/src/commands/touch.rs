//! 触控手势接线（改造项 C 控制端落点）：前端采集原始 touch（窗口相对物理
//! 坐标），Rust 侧 rd-touch-translate 翻译成鼠标/滚动命令走既有链路。
//!
//! 架构（第一性）：
//! - 翻译在控制端 Rust（改造计划决策「翻译放控制端」）：手势状态机是有状态
//!   时序逻辑，前端只做事件采集；被控端零翻译（协议分工，惯性物理永远在
//!   发送侧，被控端 `rd-input-win::scroll_pixel` 无状态消费）。
//! - 坐标系：事件已由前端换算为窗口客户区相对物理像素（与鼠标路径同源的
//!   toPhys/hitAt 换算），`TouchRegion.frame` 即整个窗口客户区；slop 等阈值
//!   的「物理手指距离」语义由 `region.scale`（远程宽 / 画面 css 宽）保持——
//!   画面缩得越小阈值越大防误判（snug 语义）。
//! - CloseWindow 命令控制端不触发：核对 is_frontmost 需要「屏上窗口从前到
//!   后」列表（snug 的 macOS 窗口服务器语义，Windows 端无对应物），触屏
//!   关窗走普通点击。防御性忽略该命令。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rd_proto::v0::{ContinuousScroll, MouseButton, envelope};
use rd_touch_translate::scrolling::ScrollStep;
use rd_touch_translate::translator::{
    Point, Rect, TouchCommand, TouchInput, TouchKind, TouchRegion, TouchTranslator,
};
use serde::Deserialize;

use super::input::{encode_mouse_move, next_seq};
use crate::conn::{ConnManager, DeviceConn};

/// 前端送来的单个触控事件（坐标=窗口客户区相对物理像素，t=单调秒）。
#[derive(Debug, Deserialize)]
pub struct TouchEventDto {
    pub kind: String,
    pub x: f64,
    pub y: f64,
    pub t: f64,
}

/// 接管范围参数：窗口客户区尺寸（物理像素）与显示缩放（远程宽/画面 css 宽，
/// 事件越缩越小时阈值按它放大）。
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct TouchRegionDto {
    pub w: f64,
    pub h: f64,
    pub scale: f64,
}

impl From<&TouchRegionDto> for TouchRegion {
    fn from(r: &TouchRegionDto) -> Self {
        Self {
            frame: Rect::new(0.0, 0.0, r.w, r.h),
            scale: r.scale,
            close_button: None,
        }
    }
}

/// DTO kind 字符串 → 状态机输入种类（未知值按拖动处理，不炸整批）。
fn kind_of(s: &str) -> TouchKind {
    match s {
        "press" => TouchKind::Press,
        "release" => TouchKind::Release,
        _ => TouchKind::Drag,
    }
}

/// 翻译产物：可靠控制流消息，或走 datagram 的鼠标移动（与鼠标路径一致，
/// 低延迟）。
#[derive(Debug, Clone, PartialEq)]
pub enum TouchOutMsg {
    Reliable(envelope::Msg),
    Move { x: i32, y: i32 },
}

/// 单窗口手势翻译状态。一个远程窗口一个实例：窗口切换后 region 变化，
/// 状态机不跨窗口延续。
pub struct TouchSession {
    translator: TouchTranslator,
    /// 最近一次按下的位置（窗口相对物理像素）——translator 吞掉的按下不随
    /// 命令回传，ReplayPress/点击类补发位置由此提供。
    press_at: Point,
}

impl Default for TouchSession {
    fn default() -> Self {
        Self::new()
    }
}

impl TouchSession {
    pub fn new() -> Self {
        Self {
            translator: TouchTranslator::new(),
            press_at: Point::default(),
        }
    }

    /// 惯性是否还在走（决定是否起驱动任务）。
    pub fn is_coasting(&self) -> bool {
        self.translator.is_coasting()
    }

    /// 喂一批事件，按序产出待发送消息。翻译为纯逻辑（锁内完成，发送在锁外）。
    pub fn feed(
        &mut self,
        region: &TouchRegionDto,
        window_id: u64,
        evs: &[TouchEventDto],
    ) -> Vec<TouchOutMsg> {
        let region = TouchRegion::from(region);
        let mut out = Vec::new();
        for ev in evs {
            let loc = Point::new(ev.x, ev.y);
            let kind = kind_of(&ev.kind);
            let input = TouchInput {
                kind,
                location: loc,
                time: ev.t,
            };
            if kind == TouchKind::Press {
                self.press_at = loc;
            }
            let resp = self.translator.handle(input, Some(&region));
            if resp.pass_original {
                out.extend(self.base_mouse(window_id, kind, loc));
            }
            for cmd in resp.commands {
                out.extend(self.expand(window_id, cmd, kind, loc));
            }
        }
        out
    }

    /// 惯性下一帧；结束返回 None（调用方停驱动任务）。
    pub fn momentum_frame(&mut self, window_id: u64) -> Option<TouchOutMsg> {
        let step = self.translator.momentum_frame()?;
        Some(scroll_msg(window_id, step))
    }

    /// 放行的原始触控转基础鼠标——没触发手势时手指也要有反应（点击/拖动
    /// 不因走了触控路径而失效）。
    fn base_mouse(&self, window_id: u64, kind: TouchKind, loc: Point) -> Vec<TouchOutMsg> {
        match kind {
            TouchKind::Press => vec![btn_msg(window_id, loc, 0, true)],
            TouchKind::Release => vec![btn_msg(window_id, loc, 0, false)],
            TouchKind::Drag => vec![TouchOutMsg::Move {
                x: loc.x.round() as i32,
                y: loc.y.round() as i32,
            }],
        }
    }

    /// 状态机命令 → 协议消息。`kind`/`loc` 为触发本命令的事件上下文
    /// （ReplayCurrent 的展开依赖它）。
    fn expand(
        &self,
        window_id: u64,
        cmd: TouchCommand,
        kind: TouchKind,
        loc: Point,
    ) -> Vec<TouchOutMsg> {
        match cmd {
            // 补发扣住的按下（位置=记下的 origin）
            TouchCommand::ReplayPress => vec![btn_msg(window_id, self.press_at, 0, true)],
            // 补发当前事件：点击补发的抬起用按下位置（严格点击语义）；
            // 选择态的拖动用当前位置
            TouchCommand::ReplayCurrent => match kind {
                TouchKind::Release => vec![btn_msg(window_id, self.press_at, 0, false)],
                _ => vec![TouchOutMsg::Move {
                    x: loc.x.round() as i32,
                    y: loc.y.round() as i32,
                }],
            },
            TouchCommand::RightClick(p) => vec![
                btn_msg(window_id, p, 1, true),
                btn_msg(window_id, p, 1, false),
            ],
            TouchCommand::Scroll(step) => vec![scroll_msg(window_id, step)],
            // 控制端不喂 close_button，不会产生；防御性忽略
            TouchCommand::CloseWindow(_) => Vec::new(),
        }
    }
}

fn btn_msg(window_id: u64, p: Point, button: u32, down: bool) -> TouchOutMsg {
    TouchOutMsg::Reliable(envelope::Msg::MouseBtn(MouseButton {
        window_id,
        x: p.x.round() as i32,
        y: p.y.round() as i32,
        button,
        down,
    }))
}

fn scroll_msg(window_id: u64, step: ScrollStep) -> TouchOutMsg {
    TouchOutMsg::Reliable(envelope::Msg::ContinuousScroll(ContinuousScroll {
        window_id,
        x: step.location.x,
        y: step.location.y,
        pixels_x: step.pixels_x,
        pixels_y: step.pixels_y,
        delta_x: step.delta_x,
        delta_y: step.delta_y,
        phase: step.phase as i32,
        momentum: step.momentum as i32,
    }))
}

// ---------------------------------------------------------------------------
// 会话表与 Tauri 命令
// ---------------------------------------------------------------------------

/// 手势状态表：key = `device_id/window_id`。断开不显式清理——单状态几百
/// 字节，窗口重开时新按下自然 reset；惯性任务随 conn 失败自停。
static SESSIONS: OnceLock<Mutex<HashMap<String, TouchSession>>> = OnceLock::new();

fn sessions() -> &'static Mutex<HashMap<String, TouchSession>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 触控手势批量喂入（前端 pointer 触屏路径）：翻译在锁内纯逻辑完成，
/// 产出的消息锁外发送。
#[tauri::command]
pub async fn touch_gesture(
    device_id: String,
    window_id: String,
    region: TouchRegionDto,
    events: Vec<TouchEventDto>,
    state: tauri::State<'_, ConnManager>,
) -> Result<(), String> {
    if events.is_empty() {
        return Ok(());
    }
    let win = super::session::parse_window_id(&window_id)?;
    let conn: Arc<DeviceConn> = state
        .get_device(&device_id)
        .ok_or_else(|| "设备未连接".to_string())?;
    let key = format!("{device_id}/{win}");

    let msgs = {
        let mut map = sessions()
            .lock()
            .map_err(|_| "手势状态锁中毒".to_string())?;
        let session = map.entry(key.clone()).or_default();
        session.feed(&region, win, &events)
    };
    let coasting = {
        let map = sessions()
            .lock()
            .map_err(|_| "手势状态锁中毒".to_string())?;
        map.get(&key).is_some_and(TouchSession::is_coasting)
    };
    send_out(&conn, &device_id, &key, win, msgs).await?;
    if coasting {
        spawn_coast(conn, key, win, device_id);
    }
    Ok(())
}

/// 发送一批产物：可靠消息走控制流；datagram 移动失败静默（低延迟路径与
/// 鼠标一致，下一帧立即覆盖）。
async fn send_out(
    conn: &Arc<DeviceConn>,
    device_id: &str,
    _key: &str,
    win: u64,
    msgs: Vec<TouchOutMsg>,
) -> Result<(), String> {
    for m in msgs {
        match m {
            TouchOutMsg::Reliable(r) => conn
                .send(r)
                .await
                .map_err(|e| format!("发送手势消息失败：{e:#}"))?,
            TouchOutMsg::Move { x, y } => {
                let seq = next_seq(device_id);
                let _ = conn.send_datagram(encode_mouse_move(win, x, y, seq).into());
            }
        }
    }
    Ok(())
}

/// 惯性驱动任务（60fps）：每 tick 从状态表取下一帧滚动并发送，结束/连接
/// 失败/状态丢失即停。重复 spawn 安全——旧任务在同一 translator 上取不到
/// 帧时自然退出（momentum 已被新按下 stop）。
fn spawn_coast(conn: Arc<DeviceConn>, key: String, win: u64, device_id: String) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(16));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let msg = {
                let Ok(mut map) = sessions().lock() else {
                    break;
                };
                let Some(session) = map.get_mut(&key) else {
                    break;
                };
                session.momentum_frame(win)
            };
            match msg {
                Some(TouchOutMsg::Reliable(r)) => {
                    if conn.send(r).await.is_err() {
                        break;
                    }
                }
                Some(TouchOutMsg::Move { x, y }) => {
                    let seq = next_seq(&device_id);
                    let _ = conn.send_datagram(encode_mouse_move(win, x, y, seq).into());
                }
                None => break,
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 测试：TouchSession 纯逻辑（tap/拖动滚动/长按右键/放行/惯性/scale 阈值）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rd_touch_translate::scrolling::{MomentumPhase, ScrollPhase};

    fn ev(kind: &str, x: f64, y: f64, t: f64) -> TouchEventDto {
        TouchEventDto {
            kind: kind.to_string(),
            x,
            y,
            t,
        }
    }

    fn region(w: f64, h: f64, scale: f64) -> TouchRegionDto {
        TouchRegionDto { w, h, scale }
    }

    fn btn_of(m: &TouchOutMsg) -> Option<(i32, i32, u32, bool)> {
        match m {
            TouchOutMsg::Reliable(envelope::Msg::MouseBtn(b)) => Some((b.x, b.y, b.button, b.down)),
            _ => None,
        }
    }

    fn scrolls_of(msgs: &[TouchOutMsg]) -> Vec<(i32, i32, i32, i32)> {
        msgs.iter()
            .filter_map(|m| match m {
                TouchOutMsg::Reliable(envelope::Msg::ContinuousScroll(s)) => {
                    Some((s.pixels_x, s.pixels_y, s.phase, s.momentum))
                }
                _ => None,
            })
            .collect()
    }

    /// 轻点=补发按下+抬起（点击落到按下位置）。
    #[test]
    fn tap_replays_click_at_press_position() {
        let mut s = TouchSession::new();
        let msgs = s.feed(
            &region(800.0, 600.0, 1.0),
            7,
            &[
                ev("press", 10.0, 20.0, 0.0),
                ev("release", 11.0, 21.0, 0.05),
            ],
        );
        let btns: Vec<_> = msgs.iter().filter_map(btn_of).collect();
        assert_eq!(
            btns,
            vec![(10, 20, 0, true), (10, 20, 0, false)],
            "轻点应补发按下+抬起于按下位置"
        );
    }

    /// 拖动超 slop → 连续滚动（Began→Changed），松手 → Ended + 起惯性。
    #[test]
    fn drag_scrolls_then_fling() {
        let mut s = TouchSession::new();
        let mut evs = vec![ev("press", 100.0, 100.0, 0.0)];
        // 30ms 一步拖 20px，10 步（超 slop 且每步速度样本充足）
        for i in 1..=10 {
            evs.push(ev(
                "drag",
                100.0,
                100.0 + 20.0 * f64::from(i),
                0.03 * f64::from(i),
            ));
        }
        evs.push(ev("release", 100.0, 300.0, 0.32));
        let msgs = s.feed(&region(800.0, 1200.0, 1.0), 7, &evs);
        let sc = scrolls_of(&msgs);
        assert!(
            sc.len() >= 10,
            "应有 Began + Changed×N + Ended 序列，实际 {} 帧",
            sc.len()
        );
        assert_eq!(sc[0].2, ScrollPhase::Began as i32);
        assert!(sc.iter().any(|f| f.2 == ScrollPhase::Changed as i32));
        assert!(sc.iter().any(|f| f.2 == ScrollPhase::Ended as i32));
        assert!(s.is_coasting(), "松手有甩出速度 → 惯性应起");
        // 惯性帧持续产出直到结束
        let mut frames = 0;
        while let Some(m) = s.momentum_frame(7) {
            let _ = m;
            frames += 1;
            assert!(frames < 200, "惯性应自然收敛");
        }
        assert!(frames > 0, "至少产出一帧惯性滚动");
    }

    /// 长按松手 = 右键（按下+抬起于按下位置）。
    #[test]
    fn long_press_right_clicks() {
        let mut s = TouchSession::new();
        let msgs = s.feed(
            &region(800.0, 600.0, 1.0),
            7,
            &[ev("press", 50.0, 60.0, 0.0), ev("release", 50.0, 60.0, 0.6)],
        );
        let btns: Vec<_> = msgs.iter().filter_map(btn_of).collect();
        assert_eq!(
            btns,
            vec![(50, 60, 1, true), (50, 60, 1, false)],
            "长按应补发右键按下+抬起"
        );
    }

    /// 区域外按下原样放行 → 基础鼠标（触屏在接管区外也有反应）。
    #[test]
    fn outside_region_passes_as_basic_mouse() {
        let mut s = TouchSession::new();
        let msgs = s.feed(
            &region(800.0, 600.0, 1.0),
            7,
            &[
                ev("press", 900.0, 20.0, 0.0),
                ev("release", 900.0, 20.0, 0.05),
            ],
        );
        let btns: Vec<_> = msgs.iter().filter_map(btn_of).collect();
        assert_eq!(btns, vec![(900, 20, 0, true), (900, 20, 0, false)]);
    }

    /// scale=2 时 slop 阈值放大：10pt 的位移不再触发滚动（仍是待定吞按）。
    #[test]
    fn scale_enlarges_slop_threshold() {
        let mut s = TouchSession::new();
        let msgs = s.feed(
            &region(1600.0, 1200.0, 2.0),
            7,
            &[
                ev("press", 100.0, 100.0, 0.0),
                ev("drag", 105.0, 100.0, 0.03),
            ],
        );
        assert!(
            scrolls_of(&msgs).is_empty(),
            "5pt 位移 < slop×2=20pt，不应进入滚动"
        );
    }

    /// 惯性相位递进到 End 后驱动停止。
    #[test]
    fn momentum_frames_end_phase_then_none() {
        let mut s = TouchSession::new();
        let mut evs = vec![ev("press", 100.0, 100.0, 0.0)];
        for i in 1..=10 {
            evs.push(ev(
                "drag",
                100.0,
                100.0 + 20.0 * f64::from(i),
                0.03 * f64::from(i),
            ));
        }
        evs.push(ev("release", 100.0, 300.0, 0.32));
        let _ = s.feed(&region(800.0, 1200.0, 1.0), 7, &evs);
        let mut saw_end = false;
        while let Some(m) = s.momentum_frame(7) {
            if let TouchOutMsg::Reliable(envelope::Msg::ContinuousScroll(sc)) = &m
                && sc.momentum == MomentumPhase::End as i32
            {
                saw_end = true;
            }
        }
        assert!(saw_end, "惯性帧序列应以 End 相位收尾");
        assert!(s.momentum_frame(7).is_none(), "End 后驱动应自然停止");
    }
}
