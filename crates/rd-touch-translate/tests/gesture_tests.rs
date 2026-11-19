//! 手势状态机全迁移路径与阈值边界测试。
//!
//! 阈值边界（10pt、0.5s、速度门槛）逐个卡临界值；假速度防护用「事件扎堆
//! 5ms」样例（snug 实测网络转发会事件挤在几毫秒一起到）。

use rd_touch_translate::{
    CloseButton, Momentum, MomentumPhase, Point, Rect, ScrollPhase, ScrollStep, TouchCommand,
    TouchInput, TouchKind, TouchRegion, TouchTranslator,
};

/// 构造输入的便签函数。
fn ev(kind: TouchKind, x: f64, y: f64, t: f64) -> TouchInput {
    TouchInput {
        kind,
        location: Point::new(x, y),
        time: t,
    }
}

/// 标准接管区域（原点 100,100 起 800×600，scale 1，无关闭按钮）。
fn region() -> TouchRegion {
    TouchRegion {
        frame: Rect::new(100.0, 100.0, 800.0, 600.0),
        scale: 1.0,
        close_button: None,
    }
}

#[test]
fn tap_replays_press_and_current() {
    let mut tr = TouchTranslator::new();
    let resp = tr.handle(ev(TouchKind::Press, 300.0, 300.0, 1.0), Some(&region()));
    assert!(!resp.pass_original, "接管范围内的按下被扣住");
    let resp = tr.handle(ev(TouchKind::Release, 300.0, 300.0, 1.05), Some(&region()));
    assert_eq!(
        resp.commands,
        vec![TouchCommand::ReplayPress, TouchCommand::ReplayCurrent]
    );
}

#[test]
fn hold_exactly_half_second_right_clicks() {
    // 0.5s 恰好临界：>= HOLD_DURATION → 右键
    let mut tr = TouchTranslator::new();
    tr.handle(ev(TouchKind::Press, 300.0, 300.0, 1.0), Some(&region()));
    let resp = tr.handle(ev(TouchKind::Release, 300.0, 300.0, 1.5), Some(&region()));
    assert_eq!(
        resp.commands,
        vec![TouchCommand::RightClick(Point::new(300.0, 300.0))]
    );
    // 差一点（0.499s）→ 不是长按，走补发点击
    let mut tr = TouchTranslator::new();
    tr.handle(ev(TouchKind::Press, 300.0, 300.0, 1.0), Some(&region()));
    let resp = tr.handle(ev(TouchKind::Release, 300.0, 300.0, 1.499), Some(&region()));
    assert_eq!(
        resp.commands,
        vec![TouchCommand::ReplayPress, TouchCommand::ReplayCurrent]
    );
}

#[test]
fn drag_before_hold_scrolls_with_fling() {
    let mut tr = TouchTranslator::new();
    tr.handle(ev(TouchKind::Press, 300.0, 300.0, 1.0), Some(&region()));
    // 0.1s 时拖动 20pt（> slop 10）→ 进入 scrolling
    let resp = tr.handle(ev(TouchKind::Drag, 300.0, 280.0, 1.1), Some(&region()));
    assert_eq!(resp.commands.len(), 2, "began + changed 两帧");
    assert_eq!(
        resp.commands[0],
        TouchCommand::Scroll(ScrollStep {
            pixels_y: 0,
            pixels_x: 0,
            delta_x: 0.0,
            delta_y: 0.0,
            phase: ScrollPhase::Began,
            momentum: MomentumPhase::None,
            location: Point::new(300.0, 300.0),
        })
    );
    let rd_touch_translate::TouchCommand::Scroll(step) = &resp.commands[1] else {
        panic!("第二帧必须是滚动");
    };
    assert_eq!(step.phase, ScrollPhase::Changed);
    assert_eq!(step.pixels_y, -20, "向上拖 20pt 整数像素");

    // 持续拖到 1.5s（拖动 0.4s），末尾速度约 50pt/0.1s = 500pt/s（> 200 门槛）
    for (i, t) in (1..5).map(|i| (i, 1.2 + i as f64 * 0.1)) {
        let _ = i;
        tr.handle(
            ev(
                TouchKind::Drag,
                300.0,
                280.0 - t * 0.0,
                1.2 + (t - 1.2) * 0.0 + 0.0,
            ),
            Some(&region()),
        );
    }
    // 直接以稳定速度收尾：最后 0.1s 内拖 50pt
    tr.handle(ev(TouchKind::Drag, 300.0, 210.0, 1.45), Some(&region()));
    let resp = tr.handle(ev(TouchKind::Release, 300.0, 210.0, 1.5), Some(&region()));
    assert!(matches!(
        resp.commands.last(),
        Some(TouchCommand::Scroll(s)) if s.phase == ScrollPhase::Ended
    ));
    assert!(tr.is_coasting(), "500pt/s 的甩出应带惯性");

    // 惯性帧序列：begin → continue → …
    let first = tr.momentum_frame().unwrap();
    assert_eq!(first.momentum, MomentumPhase::Begin);
    let second = tr.momentum_frame().unwrap();
    assert_eq!(second.momentum, MomentumPhase::Continue);
}

#[test]
fn slop_exactly_ten_points_triggers_scroll() {
    // 恰好 10pt：>= slop → 触发滚动
    let mut tr = TouchTranslator::new();
    tr.handle(ev(TouchKind::Press, 300.0, 300.0, 1.0), Some(&region()));
    let resp = tr.handle(ev(TouchKind::Drag, 300.0, 290.0, 1.05), Some(&region()));
    assert!(
        resp.commands
            .iter()
            .any(|c| matches!(c, TouchCommand::Scroll(_))),
        "恰好 10pt 应触发滚动"
    );

    // 9.99pt：被吞（还在 pending）
    let mut tr = TouchTranslator::new();
    tr.handle(ev(TouchKind::Press, 300.0, 300.0, 1.0), Some(&region()));
    let resp = tr.handle(ev(TouchKind::Drag, 300.0, 290.01, 1.05), Some(&region()));
    assert!(resp.commands.is_empty(), "不足 slop 应被吞");
    assert!(!resp.pass_original, "pending 期间事件被扣");
}

#[test]
fn hold_then_drag_selects() {
    let mut tr = TouchTranslator::new();
    tr.handle(ev(TouchKind::Press, 300.0, 300.0, 1.0), Some(&region()));
    // 按住 0.6s 后拖 >slop → selecting（补发按下+当前事件）
    let resp = tr.handle(ev(TouchKind::Drag, 320.0, 300.0, 1.6), Some(&region()));
    assert_eq!(
        resp.commands,
        vec![TouchCommand::ReplayPress, TouchCommand::ReplayCurrent]
    );
    // selecting 中后续拖动原样放行（调用方直接发鼠标拖拽）
    let resp = tr.handle(ev(TouchKind::Drag, 340.0, 300.0, 1.7), Some(&region()));
    assert!(resp.pass_original);
}

#[test]
fn fling_fake_speed_is_guarded() {
    // 事件扎堆：5ms 内挤了 5 个样本（网络转发场景），跨度 < minFlingSpan
    // → 往前多取；总样本跨度也短 → 速度按真实跨度算，不炸
    let mut tr = TouchTranslator::new();
    tr.handle(ev(TouchKind::Press, 300.0, 300.0, 1.0), Some(&region()));
    for i in 0..5 {
        tr.handle(
            ev(
                TouchKind::Drag,
                300.0,
                300.0 - 100.0 * (i as f64 + 1.0),
                1.0 + 0.005 * (i as f64 + 1.0),
            ),
            Some(&region()),
        );
    }
    let resp = tr.handle(ev(TouchKind::Release, 300.0, 200.0, 1.03), Some(&region()));
    let TouchCommand::Scroll(ended) = resp.commands.last().unwrap() else {
        panic!("最后必须是 ended");
    };
    let _ = ended;
    // 若惯性产生了，逐帧速度必须 ≤ cap/60（cap 4000pt/s → 每帧 ~66.7pt）
    while let Some(frame) = tr.momentum_frame() {
        let per_frame = frame.delta_y.abs().max(frame.delta_x.abs());
        assert!(
            per_frame <= 4000.0 / 60.0 + 1e-9,
            "惯性帧速度超上限: {per_frame}"
        );
    }
}

#[test]
fn tap_while_coasting_only_stops() {
    let mut tr = TouchTranslator::new();
    // 制造惯性：快滑+快松
    tr.handle(ev(TouchKind::Press, 300.0, 300.0, 1.0), Some(&region()));
    tr.handle(ev(TouchKind::Drag, 300.0, 250.0, 1.05), Some(&region()));
    tr.handle(ev(TouchKind::Drag, 300.0, 200.0, 1.10), Some(&region()));
    tr.handle(ev(TouchKind::Release, 300.0, 200.0, 1.12), Some(&region()));
    assert!(tr.is_coasting());

    // 惯性中点一下：只停，不补发点击
    tr.handle(ev(TouchKind::Press, 300.0, 300.0, 2.0), Some(&region()));
    let resp = tr.handle(ev(TouchKind::Release, 300.0, 300.0, 2.05), Some(&region()));
    assert!(resp.commands.is_empty(), "停惯性后快速松手不补发点击");
    assert!(!tr.is_coasting(), "惯性已停");
}

#[test]
fn outside_region_passes_through() {
    let mut tr = TouchTranslator::new();
    let resp = tr.handle(ev(TouchKind::Press, 50.0, 50.0, 1.0), Some(&region()));
    assert!(resp.pass_original, "区域外按下原样放行");
    let resp = tr.handle(ev(TouchKind::Release, 50.0, 50.0, 1.05), Some(&region()));
    assert!(resp.pass_original);
}

#[test]
fn close_button_intercepts_tap() {
    let mut tr = TouchTranslator::new();
    let mut r = region();
    r.close_button = Some(CloseButton {
        frame: Rect::new(290.0, 290.0, 20.0, 20.0),
        window: 42,
        window_frame: Rect::new(280.0, 280.0, 400.0, 300.0),
    });
    tr.handle(ev(TouchKind::Press, 300.0, 300.0, 1.0), Some(&r));
    let resp = tr.handle(ev(TouchKind::Release, 300.0, 300.0, 1.05), Some(&r));
    assert_eq!(
        resp.commands,
        vec![TouchCommand::CloseWindow(Point::new(300.0, 300.0))]
    );
}

#[test]
fn close_button_mismatch_check() {
    // is_frontmost：位置被拖走 → 核对不过 → false（调用方照常补发点击）
    let btn = CloseButton {
        frame: Rect::new(290.0, 290.0, 20.0, 20.0),
        window: 42,
        window_frame: Rect::new(280.0, 280.0, 400.0, 300.0),
    };
    assert!(btn.is_frontmost(
        Point::new(300.0, 300.0),
        &[(42, Rect::new(280.0, 280.0, 400.0, 300.0))]
    ));
    assert!(
        !btn.is_frontmost(
            Point::new(300.0, 300.0),
            &[(42, Rect::new(350.0, 350.0, 400.0, 300.0))]
        ),
        "窗口挪走 → 核对失败"
    );
    assert!(
        !btn.is_frontmost(Point::new(300.0, 300.0), &[]),
        "点下无窗口 → 核对失败"
    );
}

#[test]
fn region_scale_enlarges_thresholds() {
    // scale=2：slop 10 → 20；速度门槛 200 → 400
    let mut tr = TouchTranslator::new();
    let r = TouchRegion {
        frame: Rect::new(100.0, 100.0, 1600.0, 1200.0),
        scale: 2.0,
        close_button: None,
    };
    tr.handle(ev(TouchKind::Press, 300.0, 300.0, 1.0), Some(&r));
    // 15pt 位移（> 10 但 < 20）→ 应被吞
    let resp = tr.handle(ev(TouchKind::Drag, 315.0, 300.0, 1.05), Some(&r));
    assert!(resp.commands.is_empty(), "scale=2 下 15pt 不足 20pt 阈值");
}

#[test]
fn decimal_scroll_accumulates() {
    // 过 slop 后每帧 0.5px 慢拖：小数余量累计，每两帧出一像素
    let mut tr = TouchTranslator::new();
    tr.handle(ev(TouchKind::Press, 300.0, 300.0, 1.0), Some(&region()));
    // 先拖 20pt 过 slop 进入 scrolling
    tr.handle(ev(TouchKind::Drag, 300.0, 280.0, 1.05), Some(&region()));
    let px_of = |r: &rd_touch_translate::TouchResponse| -> Vec<i32> {
        r.commands
            .iter()
            .filter_map(|c| match c {
                TouchCommand::Scroll(s) => Some(s.pixels_y),
                _ => None,
            })
            .collect()
    };
    // 之后每帧只挪 0.5px
    let r1 = tr.handle(ev(TouchKind::Drag, 300.0, 279.5, 1.10), Some(&region()));
    let r2 = tr.handle(ev(TouchKind::Drag, 300.0, 279.0, 1.15), Some(&region()));
    let mut frames = px_of(&r1);
    frames.extend(px_of(&r2));
    assert_eq!(frames, vec![0, -1], "0.5px/帧累计：首帧余量、次帧出一像素");
}

#[test]
fn momentum_thresholds_are_exact() {
    // 恰好 200pt/s：<= 门槛 → 不带惯性
    assert!(Momentum::new(200.0, 0.0, 1.0).is_none());
    // 201pt/s：带惯性，逐帧衰减到 stopSpeed 以下停
    let mut m = Momentum::new(201.0, 0.0, 1.0).expect("201 应起振");
    let mut frames = 0;
    while m.advance_frame().is_some() {
        frames += 1;
        assert!(frames < 200, "衰减应收敛");
    }
    assert!(frames > 10, "0.95 衰减下应有十几到几十帧");
    // scale=2：门槛 400
    assert!(Momentum::new(399.0, 0.0, 2.0).is_none());
    assert!(Momentum::new(401.0, 0.0, 2.0).is_some());
}
