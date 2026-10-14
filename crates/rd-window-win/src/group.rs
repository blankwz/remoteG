//! 窗口组：以目标 HWND 为根，收集 owned 对话框与同线程弹出菜单（US-04）。
//!
//! 成员条件（任一）：
//! 1. 根窗口本身（结果首位，偏移 (0,0)）；
//! 2. 同 PID、可见、且 `GW_OWNER` 链可达根（模态/非模态对话框）；
//! 3. 类名 `#32768`（菜单）且与根同线程同 PID（`TrackPopupMenu` 在调用线程
//!    创建菜单窗口，与根窗口通常同线程）。
//!
//! 每个成员输出相对根**客户区**原点的偏移与客户区尺寸，供控制端按相对坐标
//! 合成（架构 §3.3 M1）。

use std::ffi::c_void;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindow, GetWindowThreadProcessId, IsWindow, IsWindowVisible,
    GW_OWNER,
};

use crate::enumerate::{client_rect_screen, DpiGuard};
use crate::Rect;

/// 窗口组的一个成员。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMember {
    /// 成员窗口句柄。
    pub hwnd: isize,
    /// 客户区原点相对根窗口客户区原点的 X 偏移（物理像素）。
    pub offset_x: i32,
    /// 客户区原点相对根窗口客户区原点的 Y 偏移（物理像素）。
    pub offset_y: i32,
    /// 客户区宽。
    pub w: u32,
    /// 客户区高。
    pub h: u32,
}

/// 菜单弹出窗口类名（#32768）。
const MENU_CLASS: &str = "#32768";
/// owner 链遍历上限（防环；合法链深最多几层）。
const OWNER_CHAIN_LIMIT: usize = 32;

/// 求根窗口的窗口组（根 + owned 对话框 + 同线程弹出菜单）。
///
/// 根无效时返回空表；成员按 Z 序（根固定在首位）。最小化成员不纳入
/// （菜单/对话框不会最小化，最小化即不可见）。
pub fn window_group(root: isize) -> Vec<GroupMember> {
    let _dpi = DpiGuard::new();
    let root_h = HWND(root as *mut c_void);

    unsafe {
        if !IsWindow(Some(root_h)).as_bool() {
            return Vec::new();
        }
        let root_rect = match client_rect_screen(root_h) {
            Ok(rect) => rect,
            Err(err) => {
                tracing::debug!("根窗口客户区查询失败: {err}");
                return Vec::new();
            }
        };
        let mut root_pid = 0u32;
        let root_thread = GetWindowThreadProcessId(root_h, Some(&mut root_pid));

        let mut candidates: Vec<isize> = Vec::new();

        // EnumWindows 回调不能捕获环境，用裸指针搭桥。
        unsafe extern "system" fn collect_fn(
            hwnd: HWND,
            lparam: windows::Win32::Foundation::LPARAM,
        ) -> windows::core::BOOL {
            let out = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
            out.push(hwnd.0 as isize);
            true.into()
        }
        if EnumWindows(
            Some(collect_fn),
            windows::Win32::Foundation::LPARAM(&mut candidates as *mut _ as isize),
        )
        .is_err()
        {
            tracing::debug!("EnumWindows 失败，窗口组退化为仅根窗口");
        }

        let mut members = vec![GroupMember {
            hwnd: root,
            offset_x: 0,
            offset_y: 0,
            w: root_rect.w,
            h: root_rect.h,
        }];
        for hwnd in candidates {
            if hwnd == root {
                continue;
            }
            let h = HWND(hwnd as *mut c_void);
            if !is_group_member(h, root_h, root_pid, root_thread) {
                continue;
            }
            let rect: Rect = match client_rect_screen(h) {
                Ok(rect) => rect,
                Err(_) => continue, // 窗口恰在销毁
            };
            members.push(GroupMember {
                hwnd,
                offset_x: rect.x - root_rect.x,
                offset_y: rect.y - root_rect.y,
                w: rect.w,
                h: rect.h,
            });
        }
        members
    }
}

/// 成员判定：同 PID 且（owner 链可达根，或 `#32768` 菜单且同线程）且可见。
fn is_group_member(h: HWND, root: HWND, root_pid: u32, root_thread: u32) -> bool {
    unsafe {
        if !IsWindow(Some(h)).as_bool() || !IsWindowVisible(h).as_bool() {
            return false;
        }
        let mut pid = 0u32;
        let thread = GetWindowThreadProcessId(h, Some(&mut pid));
        if pid != root_pid {
            return false;
        }
        if owned_by(h, root) {
            return true;
        }
        class_name_of(h) == MENU_CLASS && thread == root_thread
    }
}

/// `GW_OWNER` 链是否可达 `root`。
fn owned_by(h: HWND, root: HWND) -> bool {
    let mut cur = h;
    for _ in 0..OWNER_CHAIN_LIMIT {
        let owner = match unsafe { GetWindow(cur, GW_OWNER) } {
            Ok(owner) => owner,
            Err(_) => return false,
        };
        if owner.0.is_null() {
            return false;
        }
        if owner == root {
            return true;
        }
        cur = owner;
    }
    false
}

fn class_name_of(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}
