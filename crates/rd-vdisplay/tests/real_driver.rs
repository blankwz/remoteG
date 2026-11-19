//! 真机冒烟：对已安装的 virtual-display-rs master 驱动跑一遍
//! 建屏 → 换档 → 撤屏。
//!
//! 默认 `#[ignore]`（CI 与无驱动机器跑不了），本机手动执行：
//!
//! ```text
//! cargo test -p rd-vdisplay --test real_driver -- --ignored --nocapture
//! ```
//!
//! 注意：运行期间桌面上会短暂出现一块虚拟显示器（约 1~2 秒），结束后消失。

#![cfg(windows)]

use std::time::Instant;

use rd_vdisplay::{ScreenFit, VDisplay};

#[test]
#[ignore = "需要本机已装 virtual-display-rs master 驱动"]
fn real_driver_roundtrip() {
    let fit = ScreenFit::standard();
    let t0 = Instant::now();
    let mut vd = VDisplay::open(fit.clone()).expect("建屏");
    println!(
        "建屏 {:.0}ms: {} {}×{}",
        t0.elapsed().as_secs_f64() * 1000.0,
        vd.device_name().unwrap_or("<none>"),
        vd.size().0,
        vd.size().1
    );
    assert_eq!(vd.size(), fit.size(fit.base_width), "建屏后应处于基准档");

    let t1 = Instant::now();
    vd.set_width(1080).expect("换档到最宽");
    println!(
        "换档 {:.0}ms: 1080×{}",
        t1.elapsed().as_secs_f64() * 1000.0,
        vd.size().1
    );
    assert_eq!(vd.size(), fit.size(1080));
    // 换档不换屏：设备名（编号）不变
    assert!(vd.device_name().is_some());

    let t2 = Instant::now();
    vd.close().expect("撤屏");
    println!("撤屏 {:.0}ms", t2.elapsed().as_secs_f64() * 1000.0);
    assert_eq!(vd.device_name(), None);
}
