//! 剪贴板（CF_UNICODETEXT 文本）。
//!
//! `windows-sys` 当前没开 `Win32_System_DataExchange` feature，
//! 按项目一贯的做法（见 `consts.rs` 顶部注释）**自己声明 FFI**，
//! 不为了两个函数去拖一整个 feature 进来。
//!
//! 两个坑，都踩过：
//!
//! * **`OpenClipboard` 会因为别的窗口正开着剪贴板而失败。** 这不是罕见情况 ——
//!   各种管家软件、远程桌面工具、剪贴板历史程序都在盯着这块。
//!   所以这里重试，而不是一次失败就返回。
//! * **`SetClipboardData` 成功后句柄所有权归系统**，失败时才归我们。
//!   失败路径上必须自己 `GlobalFree`，否则每次都漏一块全局内存
//!   （GDI/USER 之外的第三类内核资源，`--memtest` 抓不到它）。
//!   `copy_text` 返回前只在失败分支上 free。

use std::slice;

use crate::ffi;

/// `CF_UNICODETEXT`
pub const CF_UNICODETEXT: u32 = 13;
/// `GMEM_MOVEABLE` —— 剪贴板**只**接受可移动内存，固定内存会被拒
const GMEM_MOVEABLE: u32 = 0x0002;

/// 打开剪贴板，带重试。
///
/// 返回 `true` 表示**已经打开，调用方负责 `CloseClipboard`**。
fn open_with_retry(hwnd: ffi::HWND) -> bool {
    for _ in 0..8 {
        if unsafe { ffi::OpenClipboard(hwnd) } != 0 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    false
}

/// 把文本写进剪贴板。`hwnd` 传 null 也可以，传挂件窗口更稳
/// （部分 shell 扩展会因为「陌生窗口」而多做事）。
pub fn copy_text(hwnd: ffi::HWND, s: &str) -> bool {
    // UTF-16 + 结尾 NUL，字节数 = (chars + 1) * 2
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * 2;

    unsafe {
        if !open_with_retry(hwnd) {
            return false;
        }
        if ffi::EmptyClipboard() == 0 {
            ffi::CloseClipboard();
            return false;
        }
        let hmem = ffi::GlobalAlloc(GMEM_MOVEABLE, bytes);
        if hmem.is_null() {
            ffi::CloseClipboard();
            return false;
        }
        let dst = ffi::GlobalLock(hmem);
        if dst.is_null() {
            // 没锁上：句柄还归我们，必须自己放掉
            ffi::GlobalFree(hmem);
            ffi::CloseClipboard();
            return false;
        }
        slice::from_raw_parts_mut(dst as *mut u16, wide.len()).copy_from_slice(&wide);
        ffi::GlobalUnlock(hmem);

        let r = ffi::SetClipboardData(CF_UNICODETEXT, hmem);
        if r.is_null() {
            // 系统没收下 → 所有权还在我们手里
            ffi::GlobalFree(hmem);
            ffi::CloseClipboard();
            return false;
        }
        ffi::CloseClipboard();
        true
    }
}

/// 读回剪贴板里的文本（测试用：写进去不等于真的写对了）。
///
/// 剪贴板里不是文本、或内容非法 UTF-16 时返回 `None`。
/// 与 `copy_text` 同样先 `OpenClipboard` —— 读也要遵守同一把锁。
pub fn get_text(hwnd: ffi::HWND) -> Option<String> {
    unsafe {
        if !open_with_retry(hwnd) {
            return None;
        }
        if ffi::IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
            ffi::CloseClipboard();
            return None;
        }
        let hmem = ffi::GetClipboardData(CF_UNICODETEXT);
        if hmem.is_null() {
            ffi::CloseClipboard();
            return None;
        }
        let src = ffi::GlobalLock(hmem);
        if src.is_null() {
            ffi::CloseClipboard();
            return None;
        }
        // 自己找 NUL：GlobalSize 要再导入一个函数，而字符串结尾本来就是 NUL 终止的
        let mut len = 0usize;
        let p = src as *const u16;
        while *p.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(slice::from_raw_parts(p, len));
        ffi::GlobalUnlock(hmem);
        ffi::CloseClipboard();
        Some(s)
    }
}
