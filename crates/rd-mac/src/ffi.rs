//! macOS 系统框架 C FFI 绑定（CoreGraphics / CoreFoundation / ApplicationServices）。
//!
//! 零外部 ObjC/Swift 构建依赖，直接链接 macOS 系统自带 framework。

#![allow(non_camel_case_types, non_upper_case_globals, dead_code)]

use std::ffi::{c_char, c_void};

pub type CFTypeRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFNumberRef = *const c_void;
pub type CFDataRef = *const c_void;
pub type CFIndex = isize;
pub type CGImageRef = *const c_void;
pub type CGDataProviderRef = *const c_void;
pub type CGEventRef = *const c_void;
pub type CGEventSourceRef = *const c_void;
pub type CGWindowID = u32;
pub type CGKeyCode = u16;
pub type CGFloat = f64;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CGPoint {
    pub x: CGFloat,
    pub y: CGFloat,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CGSize {
    pub width: CGFloat,
    pub height: CGFloat,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;
pub const kCFNumberSInt32Type: i32 = 3;
pub const kCFNumberSInt64Type: i32 = 4;

pub const kCGNullWindowID: CGWindowID = 0;
pub const kCGWindowListOptionOnScreenOnly: u32 = 1 << 0;
pub const kCGWindowListOptionIncludingWindow: u32 = 1 << 3;
pub const kCGWindowListExcludeDesktopElements: u32 = 1 << 4;

pub const kCGWindowImageBoundsIgnoreFraming: u32 = 1 << 0;
pub const kCGWindowImageNominalResolution: u32 = 1 << 4;

pub const kCGHIDEventTap: u32 = 0;

pub const kCGEventLeftMouseDown: u32 = 1;
pub const kCGEventLeftMouseUp: u32 = 2;
pub const kCGEventRightMouseDown: u32 = 3;
pub const kCGEventRightMouseUp: u32 = 4;
pub const kCGEventMouseMoved: u32 = 5;
pub const kCGEventLeftMouseDragged: u32 = 6;
pub const kCGEventRightMouseDragged: u32 = 7;
pub const kCGEventOtherMouseDown: u32 = 25;
pub const kCGEventOtherMouseUp: u32 = 26;
pub const kCGEventOtherMouseDragged: u32 = 27;

pub const kCGMouseButtonLeft: u32 = 0;
pub const kCGMouseButtonRight: u32 = 1;
pub const kCGMouseButtonCenter: u32 = 2;

pub const kCGScrollEventUnitPixel: u32 = 0;
pub const kCGScrollEventUnitLine: u32 = 1;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFArrayGetCount(theArray: CFArrayRef) -> CFIndex;
    pub fn CFArrayGetValueAtIndex(theArray: CFArrayRef, idx: CFIndex) -> *const c_void;
    pub fn CFDictionaryGetValue(theDict: CFDictionaryRef, key: *const c_void) -> *const c_void;
    pub fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        numBytes: CFIndex,
        encoding: u32,
        isExternalRepresentation: bool,
    ) -> CFStringRef;
    pub fn CFStringGetCString(
        theString: CFStringRef,
        buffer: *mut c_char,
        bufferSize: CFIndex,
        encoding: u32,
    ) -> bool;
    pub fn CFNumberGetValue(number: CFNumberRef, theType: i32, valuePtr: *mut c_void) -> bool;
    pub fn CFDataGetLength(theData: CFDataRef) -> CFIndex;
    pub fn CFDataGetBytePtr(theData: CFDataRef) -> *const u8;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    pub static CGRectNull: CGRect;
    pub fn CGPreflightScreenCaptureAccess() -> bool;
    pub fn CGRequestScreenCaptureAccess() -> bool;
    pub fn CGWindowListCopyWindowInfo(option: u32, relativeToWindow: CGWindowID) -> CFArrayRef;
    pub fn CGRectMakeWithDictionaryRepresentation(dict: CFDictionaryRef, rect: *mut CGRect)
        -> bool;
    pub fn CGWindowListCreateImage(
        screenBounds: CGRect,
        listOption: u32,
        windowID: CGWindowID,
        imageOption: u32,
    ) -> CGImageRef;
    pub fn CGImageGetWidth(image: CGImageRef) -> usize;
    pub fn CGImageGetHeight(image: CGImageRef) -> usize;
    pub fn CGImageGetBytesPerRow(image: CGImageRef) -> usize;
    pub fn CGImageGetBitsPerPixel(image: CGImageRef) -> usize;
    pub fn CGImageGetDataProvider(image: CGImageRef) -> CGDataProviderRef;
    pub fn CGDataProviderCopyData(provider: CGDataProviderRef) -> CFDataRef;
    pub fn CGImageRelease(image: CGImageRef);

    pub fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        mouseType: u32,
        mouseCursorPosition: CGPoint,
        mouseButton: u32,
    ) -> CGEventRef;
    pub fn CGEventCreateScrollWheelEvent(
        source: CGEventSourceRef,
        units: u32,
        wheelCount: u32,
        wheel1: i32,
        wheel2: i32,
    ) -> CGEventRef;
    pub fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtualKey: CGKeyCode,
        keyDown: bool,
    ) -> CGEventRef;
    pub fn CGEventKeyboardSetUnicodeString(
        event: CGEventRef,
        stringLength: usize,
        unicodeString: *const u16,
    );
    pub fn CGEventPost(tap: u32, event: CGEventRef);
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    pub fn AXIsProcessTrusted() -> bool;
}

/// 构造临时 CFString（调用方须 `CFRelease`）。
pub unsafe fn cf_string(s: &str) -> CFStringRef {
    unsafe {
        CFStringCreateWithBytes(
            std::ptr::null(),
            s.as_ptr(),
            s.len() as CFIndex,
            kCFStringEncodingUTF8,
            false,
        )
    }
}

/// 从 CFDictionary 读取字符串键的值（转为 Rust `String`）。
pub unsafe fn dict_get_string(dict: CFDictionaryRef, key: &str) -> Option<String> {
    unsafe {
        let cf_key = cf_string(key);
        if cf_key.is_null() {
            return None;
        }
        let val = CFDictionaryGetValue(dict, cf_key);
        CFRelease(cf_key);
        if val.is_null() {
            return None;
        }
        let mut buf = vec![0u8; 1024];
        if CFStringGetCString(
            val as CFStringRef,
            buf.as_mut_ptr() as *mut c_char,
            buf.len() as CFIndex,
            kCFStringEncodingUTF8,
        ) {
            let cstr = std::ffi::CStr::from_ptr(buf.as_ptr() as *const c_char);
            Some(cstr.to_string_lossy().into_owned())
        } else {
            None
        }
    }
}

/// 从 CFDictionary 读取 i64 键的值。
pub unsafe fn dict_get_i64(dict: CFDictionaryRef, key: &str) -> Option<i64> {
    unsafe {
        let cf_key = cf_string(key);
        if cf_key.is_null() {
            return None;
        }
        let val = CFDictionaryGetValue(dict, cf_key);
        CFRelease(cf_key);
        if val.is_null() {
            return None;
        }
        let mut out: i64 = 0;
        if CFNumberGetValue(
            val as CFNumberRef,
            kCFNumberSInt64Type,
            &mut out as *mut i64 as *mut c_void,
        ) {
            Some(out)
        } else {
            None
        }
    }
}

/// 从 CFDictionary 读取 `kCGWindowBounds` CGRect。
pub unsafe fn dict_get_rect(dict: CFDictionaryRef, key: &str) -> Option<CGRect> {
    unsafe {
        let cf_key = cf_string(key);
        if cf_key.is_null() {
            return None;
        }
        let val = CFDictionaryGetValue(dict, cf_key);
        CFRelease(cf_key);
        if val.is_null() {
            return None;
        }
        let mut rect = CGRect::default();
        if CGRectMakeWithDictionaryRepresentation(val as CFDictionaryRef, &mut rect) {
            Some(rect)
        } else {
            None
        }
    }
}
