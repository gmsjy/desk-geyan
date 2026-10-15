//! 基准与端到端实测：`--perf` / `--dragbench` / `--dragtest` / `--memtest` /
//! `--clicktest` / `--favtest`。
//!
//! # 为什么单独一个模块
//!
//! 这些函数的共同点是：**给用户跑的体检**，而不是挂件自己要跑的逻辑。
//!
//! # 当前边界
//!
//! 已搬：消息夹具（`pump` / `send_mouse` / `press` / `pump_wait`）、
//! 配置守卫（`with_config_guard`）、真实光标探针（`mouse_moves_at`）。
//! 它们自足 —— 不碰 `App` 的任何字段，只碰窗口句柄和消息队列。
//!
//! `clicktest` / `favtest` / `dragtest` / `perf` / `dragbench` / `memtest`
//! 仍留在 `app`：它们要读 `App` 的私有字段（帧缓冲、窗口句柄、配置、过渡状态），
//! 逐个提权会让 `App` 的封装形同虚设。这些函数的共同点是「只读不改语义」，
//! 真要把它们搬过来，得先有一层明确的「体检视图」—— 那是独立的一次改动，
//! 不该和「把自足的消息夹具搬出来」混在一起做。

use std::mem::zeroed;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_MOUSE, MOUSEEVENTF_MOVE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetCursorPos, PeekMessageW, PostMessageW, SetCursorPos, TranslateMessage, MSG,
    PM_REMOVE,
};

use crate::config::Config;
use crate::consts::*;

// ---------------------------------------------------------------- 拖动链路端到端

/// 把配置快照起来，跑完原样写回。`--probe` / `--dragtest` 都必须零副作用，
/// 否则「测一下」就把用户摆好的位置带偏了。
pub(crate) fn with_config_guard<F: FnOnce() -> i32>(f: F) -> i32 {
    let path = Config::path();
    let backup = std::fs::read_to_string(&path).ok();
    let code = f();
    match &backup {
        Some(text) => {
            let _ = std::fs::write(&path, text);
        }
        None => {
            let _ = std::fs::remove_file(&path);
        }
    }
    code
}

/// 抽干消息队列。`--dragtest` 发的鼠标消息必须真的被派发到 wndproc，
/// 而不是停在队列里当作「测试通过」。
pub(crate) unsafe fn pump() {
    let mut msg: MSG = zeroed();
    while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// 按 `MAKELPARAM` 打包坐标后投递一条鼠标消息
pub(crate) unsafe fn send_mouse(hwnd: HWND, msg: u32, x: i32, y: i32) {
    let lp = ((x & 0xFFFF) | ((y & 0xFFFF) << 16)) as isize;
    PostMessageW(hwnd, msg, 0, lp);
}

/// 合成一次「按下」，**但先把消息队列排空**。
///
/// 为什么需要它（`--dragtest` 曾因此稳定红一条断言）：
/// 真实光标此刻可能正停在我们窗口上方，系统于是往队列里塞了带**真实坐标**的
/// `WM_MOUSEMOVE`。这条消息在 `grab == None`（没在拖）时会被窗口过程直接丢弃，
/// 完全无害；**但如果它恰好夹在「合成按下」与「合成的那一下移动」之间被泵出来**，
/// 窗口过程就会拿真实光标位置去重算锚点，把第一条移动消息的位移带偏。
///
/// 实测症状（本机真实光标停在窗口内时）：合成的第一条移动是 `+40/+25`，
/// 窗口却位移 `(40, 27)` —— 多出来的 `+2` 正是真实光标与合成坐标之差经一次
/// 边界夹取后剩下的零头。显式报 `整体移动：第一条消息就到位` 失败。
///
/// 先 `pump()` 把这类残留消息在「未开始拖动」的状态下消化掉，锚点才落在
/// 合成的坐标上。这是**测试夹具**的问题，不是产品的问题 —— 真实拖拽里
/// 按下的位置本来就是真实光标的位置，不存在这层错位。
pub(crate) unsafe fn press(panel: HWND, x: i32, y: i32) {
    pump();
    send_mouse(panel, WM_LBUTTONDOWN, x, y);
}

/// 抽一段时间队列。注入的系统级输入是**异步**送达的，
/// 只泵一次可能还什么都收不到 —— 那样「收到 0 条」会把失败读成成功。
pub(crate) unsafe fn pump_wait(ms: u64) {
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_millis() < ms as u128 {
        pump();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    pump();
}

// ---------------------------------------------------------------- 穿透行为实测

/// 把真实光标挪到 `(cx,cy)`，返回**我们自己的窗口**收到几条鼠标移动消息。
///
/// 为什么不直接 `SetCursorPos` 就算数：那只是摆位置，系统未必重新做一次命中测试。
/// 所以摆好之后再注入**一次真实的 1px 移动**（`SendInput`）—— 只有真实输入才会
/// 走完整条命中测试链路，而「命中测试有没有把我们自己的窗口判进去」正是要问的事。
pub(crate) unsafe fn mouse_moves_at(cx: i32, cy: i32) -> u32 {
    let mut old: POINT = zeroed();
    GetCursorPos(&mut old);

    SetCursorPos(cx - 2, cy);
    pump_wait(60);
    crate::app::widget_mouse_hits_reset();

    let mut inp: INPUT = zeroed();
    inp.r#type = INPUT_MOUSE;
    inp.Anonymous.mi.dx = 2;
    inp.Anonymous.mi.dy = 0;
    inp.Anonymous.mi.dwFlags = MOUSEEVENTF_MOVE;
    SendInput(1, &inp, std::mem::size_of::<INPUT>() as i32);
    pump_wait(120);
    let n = crate::app::widget_mouse_hits();

    SetCursorPos(old.x, old.y);
    pump_wait(40);
    n
}

// `clicktest` / `favtest` / `dragtest` / `perf` / `dragbench` / `memtest`
// 仍留在 `app`：它们要读 `App` 的私有字段（帧缓冲、窗口句柄、配置、过渡状态），
// 逐个提权会让 `App` 的封装形同虚设。这些函数的共同点是「只读不改语义」，
// 真要把它们搬过来，得先有一层明确的「体检视图」——那是独立的一次改动，
// 不该和「把自足的消息夹具搬出来」混在一起做。
