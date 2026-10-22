//! 剪贴板同步(文本起步,v0.1/D-013 包4)。
//!
//! 控制端→Agent `ClipboardData{write:true}` → 写本机剪贴板;
//! `write:false` → 读本机剪贴板回发(控制端写入其本机剪贴板)。
//! 首版不含「Agent 剪贴板变化主动推送」(需要消息窗口线程,v0.2 候选)。
//!
//! 日志只记录操作与字节数,**绝不记录剪贴板内容**(DoD 同按键纪律)。

/// 读本机剪贴板文本(CF_UNICODETEXT;打开失败/无文本/非 UTF-16 均返回 None)。
#[cfg(windows)]
pub fn read_text() -> Option<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    unsafe {
        OpenClipboard(None).ok()?;
        let out = (|| {
            let h = GetClipboardData(u32::from(CF_UNICODETEXT.0)).ok()?;
            let p = GlobalLock(HGLOBAL(h.0)) as *const u16;
            if p.is_null() {
                return None;
            }
            // 以 NUL 截断(GLOBALAlloc 的剪贴板文本保证 NUL 结尾,但防御性扫描)
            let mut len = 0usize;
            while *p.add(len) != 0 {
                len += 1;
                if len > 8 * 1024 * 1024 {
                    return None; // 8 MiB 上限,防异常剪贴板内容
                }
            }
            let slice = std::slice::from_raw_parts(p, len);
            let text = String::from_utf16_lossy(slice);
            let _ = GlobalUnlock(HGLOBAL(h.0));
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        })();
        let _ = CloseClipboard();
        out
    }
}

/// 写本机剪贴板文本(失败返回 false;内容不落日志)。
#[cfg(windows)]
pub fn write_text(text: &str) -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let ok = (|| {
            let _ = EmptyClipboard();
            let mut utf16: Vec<u16> = text.encode_utf16().collect();
            utf16.push(0); // NUL 结尾
            let bytes = utf16.len() * 2;
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes).ok()?;
            let p = GlobalLock(h) as *mut u16;
            if p.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(utf16.as_ptr(), p, utf16.len());
            let _ = GlobalUnlock(h);
            SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(h.0))).ok()?;
            Some(())
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}

#[cfg(not(windows))]
pub fn read_text() -> Option<String> {
    None
}

#[cfg(not(windows))]
pub fn write_text(_text: &str) -> bool {
    false
}
