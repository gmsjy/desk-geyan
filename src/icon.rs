//! 应用图标。
//!
//! 图标的源文件是 `assets/desk-geyan.ico`（10 档尺寸：16/20/24/32/40/48/64/96/128/256），
//! 由 `build.rs` 编进 PE 资源，这里只负责按需要的像素尺寸取出来。
//!
//! 为什么要按尺寸取、而不是取一个 32×32 让系统缩：
//! 托盘图标在 100% 屏是 16px、150% 屏是 24px、200% 屏是 32px。让系统把 256px
//! 缩到 16px 会糊成一团灰，所以 ICO 里小尺寸是单独手绘的（去掉文字细节、加粗轮廓），
//! `LoadImageW` 会自己挑最贴近的那一档，只有实在没有时才缩放。

use std::ffi::c_void;

use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, LoadImageW, LoadIconW, HICON, IMAGE_ICON, LR_DEFAULTCOLOR,
    SM_CXSMICON, SM_CYSMICON,
};

use crate::consts::IDI_APPLICATION_RES;

/// RT_GROUP_ICON 的资源 ID，与 `build.rs` 里写的一致。
const ICON_RES_ID: usize = 1;

/// 取应用图标。
///
/// `cx` / `cy` 是期望的像素尺寸；传 0 表示「按系统小图标尺寸」（托盘用这个）。
/// 资源没编进去时（例如别处 clone 后 `assets` 缺失）退回系统默认图标，
/// 不 panic —— 丢个图标而已，不值得让程序起不来。
pub(crate) fn app_icon(cx: i32, cy: i32) -> HICON {
    let (cx, cy) = if cx <= 0 || cy <= 0 {
        unsafe {
            let s = GetSystemMetrics(SM_CXSMICON).max(16);
            (s, GetSystemMetrics(SM_CYSMICON).max(16))
        }
    } else {
        (cx, cy)
    };

    unsafe {
        let h = LoadImageW(
            crate::app::module_handle(),
            ICON_RES_ID as *const u16, // 整数 ID ⇒ MAKEINTRESOURCE
            IMAGE_ICON,
            cx,
            cy,
            LR_DEFAULTCOLOR,
        );
        if !h.is_null() {
            return h as HICON;
        }
        // 资源缺失：退回系统默认应用图标（与改造前行为一致）
        LoadIconW(std::ptr::null_mut::<c_void>() as _, IDI_APPLICATION_RES)
    }
}

/// 设置窗口用的图标对：小图标给标题栏 / 任务栏缩略图，大图标给 Alt+Tab。
pub(crate) fn window_icons() -> (HICON, HICON) {
    unsafe {
        let small = app_icon(
            GetSystemMetrics(SM_CXSMICON).max(16),
            GetSystemMetrics(SM_CYSMICON).max(16),
        );
        // ICON_BIG 用 32px 档；高 DPI 下系统会自己挑更大的档，这里给个基准即可
        let big = app_icon(32, 32);
        (small, big)
    }
}
