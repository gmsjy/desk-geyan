//! 托盘菜单的**属主窗口**。
//!
//! # 为什么 `TrackPopupMenu` 需要一个「专门的」属主
//!
//! 菜单收起靠的是「属主失去激活」：菜单是属主的从属弹出物，用户点了别处 →
//! 属主被别的窗口激活 → 菜单自己收掉。这套机制要求属主**能被激活**，而本进程
//! 两个现成的顶层窗口都不合格：
//!
//! | 候选 | 为什么不行 |
//! | --- | --- |
//! | 底板 `a.panel` | 带 `WS_EX_NOACTIVATE`，永远不会被激活，也就永远不会「失去激活」；且用户只用灵动岛时它是隐藏的（`visible=0`），**隐藏窗口连成为前台的资格都没有** |
//! | 文字层 `a.text` | 同样 `NOACTIVATE`，而且 `WS_EX_TRANSPARENT` |
//! | 设置面板 | 用完即毁；开着的时候拿它当属主，收菜单会把面板一起牵连进激活链 |
//!
//! 症状实测过两次：菜单弹得出来、点别处没反应、`TrackPopupMenu` 迟迟不返回，
//! 之后的托盘点击全排队 —— 用户看到的还是「点了没反应」。
//!
//! # 这个窗口必须同时满足四条
//!
//! 1. **可见**：不可见的窗口没有前台资格（`SetForegroundWindow` 直接返回 0）；
//! 2. **可激活**：不能带 `WS_EX_NOACTIVATE`；
//! 3. **用户看不见**：1×1 + 整窗 `alpha = 0`（`WS_EX_LAYERED` + `LWA_ALPHA`），
//!    不画任何东西，也不闪一下；
//! 4. **不进任务栏 / Alt-Tab**：`WS_EX_TOOLWINDOW`。
//!
//! 位置跟着光标走 —— 菜单就开在光标处，属主落在同一块屏上，多显示器与
//! 每屏 DPI 都不需要额外处理。

use super::*;

use std::mem::zeroed;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW, IsWindow, RegisterClassW,
    SetForegroundWindow, SetLayeredWindowAttributes, SetWindowPos, ShowWindow, WNDCLASSW,
};
use crate::consts::*;
use crate::util::{trace, wide};

pub(crate) const MENU_HOST_CLASS: &str = "DeskGeyanMenuHost";

/// 属主窗口的不透明度（0–255）。
///
/// **不用 0。** 分层窗口把整窗 alpha 设成 0 之后，命中测试也一并透明了
/// （`WindowFromPoint` 直接跳过它）—— 功能上不影响「能不能被激活」，但那意味着
/// 我们**没有任何手段证明它真的在屏幕上**：`IsWindowVisible` 对「被系统 cloaked
/// 的窗口」同样返回 true。取 1/255 在屏幕上不可见（0.4% 的黑，肉眼与测量都当它
/// 不存在），却保住了「点得到」这条可验证性。
const HOST_ALPHA: u8 = 1;

/// 属主窗口的边长（物理像素）。1px 就够 —— 它的全部意义是「存在且可激活」。
const HOST_SIZE: i32 = 1;

/// `RegisterClassW` 只该成功一次。
///
/// 和 `config_ui` 里同一个坑：重复注册返回 0（`ERROR_CLASS_ALREADY_EXISTS`），
/// 若把它当失败处理就会「每开一次菜单多一次无意义的系统调用 + 一次误报」。
static REGISTERED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // 没有任何自绘内容：alpha=0 的 1px 窗口，全部交回默认处理。
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// 取属主窗口，没有就建。
///
/// 懒建（第一次弹菜单时）而不是启动时建：不用托盘菜单的用户一个多余窗口都不该有。
/// 建成之后**长期保留**（收起时 Hide 而不是 Destroy）—— 建/毁窗口要注册类、
/// 挂 DWM 链路，而每次弹菜单都重建纯属浪费；隐藏窗口不占可见资源。
pub(crate) fn ensure(a: &mut App) -> HWND {
    unsafe {
        if !a.menu_host.is_null() {
            if IsWindow(a.menu_host) != 0 {
                return a.menu_host;
            }
            // 句柄失效（被外部 Destroy / 进程重启过）：重新建
            a.menu_host = null_mut();
        }

        if !REGISTERED.swap(true, Ordering::Relaxed) {
            let cls = wide(MENU_HOST_CLASS);
            let mut wc: WNDCLASSW = zeroed();
            wc.lpfnWndProc = Some(host_proc);
            wc.hInstance = a.hinst;
            // 无背景刷：本来就不显示，填色只会白费一次 GDI 操作
            wc.hbrBackground = null_mut();
            wc.lpszClassName = cls.as_ptr();
            RegisterClassW(&wc);
        }

        let cls = wide(MENU_HOST_CLASS);
        let title = wide("DeskGeyanMenuHost");
        let h = CreateWindowExW(
            // TOOLWINDOW：不进任务栏、不进 Alt-Tab；LAYERED：整窗 alpha=0 的前提。
            // 刻意**不加** NOACTIVATE —— 能被激活正是它存在的理由。
            //
            // TOPMOST：光标可能正好落在挂件自己身上（挂件是置顶的），此时若属主留在
            // 普通 z 序带里，它会被压在下面 —— 激活不受影响，但「点得到」这条要打折。
            // 1px、全透明、只在菜单期间显示，置顶没有任何可见代价。
            WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TOPMOST,
            cls.as_ptr(),
            title.as_ptr(),
            // 不带 WS_VISIBLE：什么时候显示由 `show_at` 决定
            WS_POPUP,
            -HOST_SIZE,
            -HOST_SIZE,
            HOST_SIZE,
            HOST_SIZE,
            null_mut(),
            null_mut(),
            a.hinst,
            null(),
        );
        if h.is_null() {
            trace(
                "menu",
                &format!("属主窗口创建失败 err={}", GetLastError()),
            );
            return null_mut();
        }
        // 整窗近乎全透明（见 `HOST_ALPHA` 为什么不是 0）。用 `LWA_ALPHA` 而不是
        // 逐像素 alpha：后者会让透明区域**点击穿透**，属主就失去了可验证的落点。
        SetLayeredWindowAttributes(h, 0, HOST_ALPHA, LWA_ALPHA);
        a.menu_host = h;
        h
    }
}

/// 把属主挪到光标处并显示。
///
/// 只做「放好 + 显示」，**不激活** —— 激活由 `activate` 单独做，两者分开是为了
/// 让 `--probe` 能验「属主是可见、点得到的合格窗口」而不用把前台从用户手里抢走。
pub(crate) fn show_at(h: HWND, pt: POINT) {
    if h.is_null() {
        return;
    }
    unsafe {
        SetWindowPos(
            h,
            HWND_TOP as HWND,
            pt.x,
            pt.y,
            HOST_SIZE,
            HOST_SIZE,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        ShowWindow(h, SW_SHOWNOACTIVATE);
    }
}

/// 让属主成为前台窗口 —— 菜单能收起的关键一步。
///
/// 属主成为前台之后，用户点别处 → 前台换人 → 属主失去激活 → 菜单自动收起。
/// 少了它，菜单会一直挂着把后续鼠标点击全吃掉（实测过的症状）。
///
/// 返回 `SetForegroundWindow` 的结果（调用方记进追踪）。**失败不阻断**：
/// 菜单照样弹得出来、菜单项也点得动，只是「点别处收起」没了保证 —— 这比
/// 「菜单根本不出现」好，而且失败必须能被说出来，不能静默。
///
/// # 这一步做不成时不是代码问题
///
/// 系统只把前台资格给「刚接收过用户输入」的进程。点托盘图标是 shell 收的输入，
/// 它转手发消息给我们；真正的用户点击会附带这份资格，**注入的消息不会**。
/// 实测：同一条代码，注入路径下 `SetForegroundWindow` 恒为 0（`SetActiveWindow`、
/// TOPMOST 闪烁、`ShowWindow` 也都无效），而进程已经持有前台窗口时它立刻成功。
/// 所以这里不叠加任何「硬抢」手法 —— 那些在无资格时必然失败，只会掩盖真相。
pub(crate) fn activate(h: HWND) -> bool {
    if h.is_null() {
        return false;
    }
    unsafe { SetForegroundWindow(h) != 0 }
}

/// 菜单收起后立刻藏起来。属主长时间可见没有意义（它是 1px 透明窗口）。
pub(crate) fn hide(h: HWND) {
    if h.is_null() {
        return;
    }
    unsafe { ShowWindow(h, SW_HIDE) };
}

/// 退出时销毁。隐藏窗口本来就不可见，销毁是为了让「窗口计数」类诊断
/// 在退出路径上也干净（进程随后就 exit，但 `--probe` 会数一遍）。
pub(crate) fn destroy(h: HWND) {
    if h.is_null() {
        return;
    }
    unsafe { DestroyWindow(h) };
}

/// 属主窗口是否处于「可被激活」的合格形态（样式位口径）。
///
/// 给 `--probe` 用：`NOACTIVATE` 是这条链路上唯一会**静默**把功能弄坏的样式位
/// （菜单照样弹、就是收不起来），样式里没有它才是对的。
pub(crate) fn activatable(h: HWND) -> bool {
    if h.is_null() {
        return false;
    }
    let ex = unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } as u32;
    ex & WS_EX_NOACTIVATE == 0
}
