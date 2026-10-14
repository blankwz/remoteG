//! 输入处理:协议输入消息 → rd-input-win 注入(T-017 实现)。
//!
//! 本任务(T-015)只定义共享类型与桩签名;注入逻辑(含坐标映射、置前、
//! 断开 `release_all` 防卡键)由 T-017 填充。

use crate::session::SessionCtx;
use rd_proto::v0::{KeyEvent, MouseButton, MouseWheel, TextInput};

/// 键鼠注入器:Windows 下为 rd-input-win 实现(记录按下状态,断开时全抬);
/// 其他平台为空桩,保持整个 crate 可跨平台 check。
#[cfg(windows)]
pub use rd_input_win::Injector;

/// 非 Windows 平台注入桩:仅满足类型签名,`new()` 不做任何事。
#[cfg(not(windows))]
#[derive(Debug, Default)]
pub struct Injector;

#[cfg(not(windows))]
impl Injector {
    /// 与 Windows 版签名对齐的空构造。
    pub fn new() -> Self {
        Self
    }
}

/// 控制流承载的输入消息(点击/按键需可靠送达,05_api 摘要)。
/// 载荷字段由 T-017 的注入实现消费,桩阶段无读者。
#[derive(Debug)]
#[allow(dead_code)]
pub enum InputMsg {
    Key(KeyEvent),
    MouseButton(MouseButton),
    MouseWheel(MouseWheel),
    TextInput(TextInput),
}

/// 处理一条控制流输入消息(T-017 实现;桩不注入、不回包)。
pub fn handle(ctx: &SessionCtx, msg: InputMsg) -> anyhow::Result<()> {
    let _ = (ctx, msg);
    Ok(())
}

/// 处理一条 QUIC datagram(鼠标移动:`u8 type=1, u64 window_id, i32 x, i32 y,
/// u32 seq`,05_api §3)。T-017 实现;桩直接忽略。
pub fn on_datagram(ctx: &SessionCtx, bytes: &[u8]) -> anyhow::Result<()> {
    let _ = (ctx, bytes);
    Ok(())
}

/// 会话断开:抬起全部已按下键/鼠标键,防目标窗口卡键(T-017 实现;桩为空)。
pub fn on_disconnect(ctx: &SessionCtx) {
    let _ = ctx;
}
