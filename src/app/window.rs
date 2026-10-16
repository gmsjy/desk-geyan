//! Win32 窗口层：创建、Acrylic 毛玻璃、穿透、几何与可见性。
//!
//! 这一层只回答「窗口在屏幕上是什么形状、能不能被点到」，不回答「画什么」。
//! 与绘制的边界是 `mark_dirty` / `render_and_present` / `apply_geometry`。

use super::*;

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, InvalidateRect, MONITORINFO, MonitorFromPoint};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, GetWindowLongPtrW, LoadCursorW, RegisterClassW, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, ShowWindow, SystemParametersInfoW, WNDCLASSW};
use crate::config::Config;
use crate::ffi::{AccentPolicy, WinCompAttrData, set_window_composition_attribute};
use crate::consts::*;
use crate::util::*;

/// 矩形逐字段相等。
///
/// `windows-sys` 的 `RECT` 没 derive `PartialEq`，而多显示器那条判据要求的正是
/// 「两块屏的工作区**逐字段**相同」—— 按面积或宽高比相等都不算，
/// 副屏的工作区完全可能与主屏等宽等高但整体偏移。
pub(crate) fn rect_eq(a: &RECT, b: &RECT) -> bool {
    a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
}

/// 取「挂件所在那块屏幕」的工作区与全屏区域。
///
/// **以前这里用 `SystemParametersInfoW(SPI_GETWORKAREA)`，而它返回的是主屏工作区**
/// —— 多显示器下整个基准就是错的：
///
/// * 挂件拖到副屏后，`clamp_keep_visible` 仍按主屏夹取，副屏上的窗口会被夹到主屏的
///   坐标范围内，视觉上就是「窗口自己跳走了」；
/// * 任务栏在副屏时，副屏的可用高度被主屏的值顶替，落位与缩放夹取全错；
/// * 副屏 DPI 与主屏不同时，前面做的 `PER_MONITOR_AWARE_V2` 等于白做 ——
///   DPI 感知是对的，但「按哪块屏」错了。
///
/// 现在按挂件当前位置取所在显示器：`MonitorFromPoint` + `MONITOR_DEFAULTTONEAREST`
/// 表示「离这个点最近的显示器」，点落在所有屏幕之外时也能落到正确的那块
/// （比 `MONITOR_DEFAULTTOPRIMARY` 更适合「被拖到屏外后要按哪块屏夹回去」），
/// 再用 `GetMonitorInfoW` 读它的 `rcWork`（工作区）与 `rcMonitor`（全屏）。
///
/// `--probe` 有一条断言守「夹取所用屏 == 挂件所在屏」，单屏机上它恒真，
/// 所以要接上副屏才是真的在验（见 probe 里的说明）。
pub(crate) fn work_area_at(x: i32, y: i32, w: i32) -> (RECT, RECT) {
    unsafe {
        let hmon = MonitorFromPoint(
            POINT {
                x: x + w / 2,
                y: y + 8,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        if !hmon.is_null() && GetMonitorInfoW(hmon, &mut mi) != 0 {
            (mi.rcWork, mi.rcMonitor)
        } else {
            // 兜底：取不到显示器信息就退回主屏工作区（老系统 / 异常路径）
            let mut work: RECT = zeroed();
            SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                &mut work as *mut _ as *mut c_void,
                0,
            );
            let full = RECT {
                left: 0,
                top: 0,
                right: work.right.max(1280),
                bottom: work.bottom.max(720),
            };
            (work, full)
        }
    }
}

pub(crate) fn clamp_to_work_area(cfg: &mut Config) {
    let (wa, _) = work_area_at(cfg.x, cfg.y, cfg.w);
    cfg.w = cfg.w.clamp(MIN_W, wa.right.max(320));
    cfg.h = cfg.h.clamp(MIN_H, wa.bottom.max(160));
    // 至少留 80px 在屏幕内，避免用户把窗口拖到找不回来的地方
    let (x, y) = clamp_keep_visible(cfg.x, cfg.y, cfg.w);
    cfg.x = x;
    cfg.y = y;
}

// ---------------------------------------------------------------- 建窗

pub(crate) unsafe fn create_panel(a: &App) -> Option<HWND> {
    let cls = wide(PANEL_CLASS);
    let mut wc: WNDCLASSW = zeroed();
    wc.style = CS_HREDRAW | CS_VREDRAW;
    wc.lpfnWndProc = Some(panel_proc);
    wc.hInstance = a.hinst;
    wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW_RES);
    // 关键：不给背景刷。底板窗口必须「什么都不画」，
    // 否则 GDI 会把 DWM 的毛玻璃糊掉。
    wc.hbrBackground = null_mut();
    wc.lpszClassName = cls.as_ptr();
    RegisterClassW(&wc);

    let title = wide("DeskGeyan");
    let h = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TRANSPARENT,
        cls.as_ptr(),
        title.as_ptr(),
        WS_POPUP | WS_VISIBLE,
        a.cfg.x,
        a.cfg.y,
        a.cfg.w,
        a.cfg.h,
        null_mut(),
        null_mut(),
        a.hinst,
        null(),
    );
    if h.is_null() {
        return None;
    }
    // Win11 圆角。失败也无所谓（Win10 没有这个属性）
    let round = DWMWCP_ROUND;
    DwmSetWindowAttribute(
        h,
        DWMWA_WINDOW_CORNER_PREFERENCE as u32,
        &round as *const _ as *const c_void,
        std::mem::size_of::<i32>() as u32,
    );
    Some(h)
}

pub(crate) unsafe fn create_text(a: &App) -> Option<HWND> {
    let cls = wide(TEXT_CLASS);
    let mut wc: WNDCLASSW = zeroed();
    wc.style = 0;
    wc.lpfnWndProc = Some(text_proc);
    wc.hInstance = a.hinst;
    wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW_RES);
    wc.hbrBackground = null_mut();
    wc.lpszClassName = cls.as_ptr();
    RegisterClassW(&wc);

    let title = wide("DeskGeyanText");
    let h = CreateWindowExW(
        // WS_EX_TRANSPARENT 在文字窗口上是**恒定**的：它永远只负责显示，
        // 鼠标一律交给下面的底板窗口处理，绝无可能挡住用户的点击。
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TRANSPARENT,
        cls.as_ptr(),
        title.as_ptr(),
        WS_POPUP | WS_VISIBLE,
        a.cfg.x,
        a.cfg.y,
        a.cfg.w,
        a.cfg.h,
        null_mut(),
        null_mut(),
        a.hinst,
        null(),
    );
    if h.is_null() {
        None
    } else {
        Some(h)
    }
}

/// 底板窗口在文字窗口**下面**紧贴一层
pub(crate) fn raise_pair(a: &App) {
    unsafe {
        let after = if a.cfg.always_on_top {
            HWND_TOPMOST as *mut c_void
        } else {
            HWND_NOTOPMOST as *mut c_void
        };
        SetWindowPos(
            a.panel,
            after,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );
        SetWindowPos(
            a.text,
            after,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );
        // 把底板压到文字下面（hWndInsertAfter 之后的窗口在 z 序里更靠下）
        SetWindowPos(
            a.panel,
            a.text,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

pub(crate) fn apply_topmost(a: &App) {
    raise_pair(a);
}

// ---------------------------------------------------------------- 毛玻璃

pub(crate) fn set_accent(hwnd: HWND, state: i32, alpha: u8) -> bool {
    let policy = AccentPolicy {
        accent_state: state,
        accent_flags: 0,
        // 0xAABBGGRR：最高字节就是毛玻璃浓度
        gradient_color: ((alpha as u32) << 24)
            | (PANEL_B << 16)
            | (PANEL_G << 8)
            | PANEL_R,
        animation_id: 0,
    };
    let data = WinCompAttrData {
        attribute: WCA_ACCENT_POLICY,
        data: &policy as *const _ as *const c_void,
        size_of_data: std::mem::size_of::<AccentPolicy>(),
    };
    set_window_composition_attribute(hwnd, &data)
}

/// 底板可见性 + 毛玻璃浓度。同时决定底板是否需要「自绘实心」（降级路径）。
pub(crate) fn apply_glass(a: &mut App) {
    let show = a.should_show_panel();
    if !show {
        if a.glass == Glass::Acrylic {
            set_accent(a.panel, ACCENT_DISABLED, 0);
        }
        unsafe { ShowWindow(a.panel, SW_HIDE) };
        return;
    }

    let alpha = a.panel_alpha();
    if a.glass == Glass::Acrylic {
        // 短路求值：Acrylic(4) 不行才退 BlurBehind(3)
        let ok = set_accent(a.panel, ACCENT_ENABLE_ACRYLICBLURBEHIND, alpha)
            || set_accent(a.panel, ACCENT_ENABLE_BLURBEHIND, alpha);
        if ok {
            a.accent_ok = true;
        } else {
            a.accent_ok = false;
            // 老系统：退化为整窗 alpha + 自绘实心底板
            a.glass = Glass::Layered;
            let ex = unsafe { GetWindowLongPtrW(a.panel, GWL_EXSTYLE) } as u32;
            unsafe {
                SetWindowLongPtrW(a.panel, GWL_EXSTYLE, (ex | WS_EX_LAYERED) as isize);
                SetWindowPos(
                    a.panel,
                    null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
                windows_sys::Win32::UI::WindowsAndMessaging::SetLayeredWindowAttributes(
                    a.panel,
                    0,
                    alpha,
                    LWA_ALPHA,
                );
            }
        }
    } else {
        unsafe { SetLayeredWindowAttributes(a.panel, 0, alpha, LWA_ALPHA) };
    }
    unsafe {
        ShowWindow(a.panel, SW_SHOWNOACTIVATE);
        InvalidateRect(a.panel, null(), 0);
    }
}

// ---------------------------------------------------------------- 模式 / 穿透

/// 切换「鼠标能否落到挂件上」。**操作的是底板窗口自己的 `WS_EX_TRANSPARENT`。**
///
/// 这一位与 `panel_proc` 里 `WM_NCHITTEST` 返回 `HTTRANSPARENT` 是**两条独立的机制**，
/// 本项目两件事都做：样式位让命中测试阶段就跳过窗口，`HTTRANSPARENT` 兜住
/// 「消息已经派发到我们头上」的情况。`--clicktest` 用真实光标验的是**行为**
/// （数我们自己的窗口收到几条鼠标移动），不是这两个属性写没写。
fn set_panel_click_through(a: &App, on: bool) {
    unsafe {
        let ex = GetWindowLongPtrW(a.panel, GWL_EXSTYLE) as u32;
        let nw = if on {
            ex | WS_EX_TRANSPARENT
        } else {
            ex & !WS_EX_TRANSPARENT
        };
        if nw != ex {
            SetWindowLongPtrW(a.panel, GWL_EXSTYLE, nw as isize);
            SetWindowPos(
                a.panel,
                null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
}

pub(crate) fn apply_mode(a: &mut App) {
    // 布局模式必须能点得到：临时关掉穿透，底板当拖拽把手
    set_panel_click_through(a, !a.cfg.layout_mode);
    // 窗口尺寸可能变了，把时间日期块重新夹一次，别让它跑出可视区
    normalize_clock_offset(a);
    apply_glass(a);
    raise_pair(a);
    render_and_present(a);
    a.cfg.save();
}

pub(crate) fn set_visible(a: &mut App, on: bool) {
    a.cfg.visible = on;
    // 手动切换 = 用户意图，永远压过「全屏自动隐藏」：清零记账位，
    // 否则退出全屏时会弹回用户刚刚亲手关掉的窗口（或重复 ShowWindow）。
    a.fs_hidden = false;
    unsafe {
        if on {
            if a.should_show_panel() {
                ShowWindow(a.panel, SW_SHOWNOACTIVATE);
            }
            ShowWindow(a.text, SW_SHOWNOACTIVATE);
            render_and_present(a);
        } else {
            ShowWindow(a.text, SW_HIDE);
            ShowWindow(a.panel, SW_HIDE);
        }
    }
    a.cfg.save();
}

/// 记下「窗口该去哪」，并按节拍把它落到窗口上 —— 移动布局的最优路径。
///
/// 位置变化不影响任何一个像素：掩码、合成、预乘结果都和窗口在哪无关。
/// 所以「挪」这件事的全部工作就是**改窗口位置**，一次内容提交都不该发生。
/// 旧实现在每个 `WM_MOVE` 里跑一遍整帧重绘（~800µs）＋上屏，
/// 这些工作对「位置变了」这件事毫无贡献。
pub(crate) fn request_move(a: &mut App, x: i32, y: i32) {
    a.cfg.x = x;
    a.cfg.y = y;
    apply_geometry(a, false, false);
}

/// 把 `cfg` 里的几何落到窗口上。**拖动路径上唯二的两处窗口调用就在这里面**，
/// 务必别在这儿加东西。
///
/// * `resize`：要不要连尺寸一起落。缩放走它，移动不走（`SWP_NOSIZE`）。
/// * `force`：无视节拍立刻落。松手收尾必须带上 —— 那一下的位置就是最终位置。
///
/// 底板一律 `SetWindowPos`（毛玻璃由 DWM 按新位置重算，没别的做法）。
/// 文字层分两种：
/// * **只挪位置** → `UpdateLayeredWindow`。实测 ~91µs，比 `SetWindowPos` 的
///   ~206µs 快一倍多（理由见 `MoveWay`）。
/// * **要改尺寸** → 退回 `SetWindowPos`。ULW 会把窗口尺寸一起定成帧缓冲的尺寸，
///   而这时候帧缓冲还没按新尺寸重建，两边会打架；等下一帧重绘再把新尺寸提交上去。
pub(crate) fn apply_geometry(a: &mut App, resize: bool, force: bool) {
    if !force && a.last_move.elapsed().as_millis() < a.move_throttle {
        return;
    }
    let (x, y, w, h) = (a.cfg.x, a.cfg.y, a.cfg.w, a.cfg.h);
    let (dw, dh) = if resize { (w, h) } else { (0, 0) };
    let mut flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
    if !resize {
        flags |= SWP_NOSIZE;
    }
    unsafe {
        // 纯文字模式下底板是藏着的，但**仍然要挪**：一进布局模式它就要显形，
        // 位置这时候不跟上，它就会从上一处「飞」过来。
        SetWindowPos(a.panel, null_mut(), x, y, dw, dh, flags);
        match a.move_way {
            MoveWay::Swp => {
                SetWindowPos(a.text, null_mut(), x, y, dw, dh, flags);
            }
            MoveWay::Ulw => {
                if resize {
                    SetWindowPos(a.text, null_mut(), x, y, dw, dh, flags);
                } else {
                    let hwnd = a.text;
                    a.frame.present(hwnd, x, y);
                }
            }
        }
    }
    a.last_move = std::time::Instant::now();
}

/// 应用一次缩放：位置和尺寸都按节拍落到窗口上（边框一样跟手，只是落在 8ms 的
/// 格子上），内容重绘另走重绘节流 —— 尺寸变了必须重画，但没必要每条鼠标消息都画。
///
/// # 为什么 `normalize_clock_offset` 留到出帧那一步
///
/// 它要 `layout_clock`（两次文字度量 + 两次 `SelectObject`，约 700µs），
/// 而 1000Hz 鼠标一条消息的预算总共才 1000µs —— 每条都夹一次就直接超支，
/// `--perf` 实测缩放拖动 1156µs/条就是这么来的。
///
/// 夹取是**幂等且不必即时**的：它只保证「时间日期块不会留在被裁掉的区域里」，
/// 而用户看到这个结果是在下一帧（`render_if_due` 已经按 8ms 节流）。
/// 所以这里只标脏，把夹取并进出帧路径：拖动期间时钟块允许暂时越界一帧，
/// 松手时 `render_if_due(force=true)` 一定夹到最终位置。
pub(crate) fn apply_resize(a: &mut App, x: i32, y: i32, w: i32, h: i32) {
    let resized = w != a.cfg.w || h != a.cfg.h;
    a.cfg.x = x;
    a.cfg.y = y;
    a.cfg.w = w;
    a.cfg.h = h;
    if resized {
        // 跑马灯一条要走多远由宽度决定，宽度变了得重新夹一次偏移，
        // 否则时间日期块会留在已经被裁掉的区域里。
        // 夹取本身并进 `render_and_present`（见那儿的第一行），这里只标脏。
        mark_dirty(a);
    }
    apply_geometry(a, true, false);
    render_if_due(a, false);
}
