//! 应用主体：双窗口架构 + 消息泵 + 交互。
//!
//! # 为什么是两个窗口而不是一个
//!
//! 毛玻璃和「文字真正贴合」这两件事在同一个窗口里是互相打架的：
//!
//! * DWM 的 Acrylic 只能作用在**没有被 GDI 画过的像素**上。一旦窗口为了
//!   换帧而 `AlphaBlend`/`BitBlt` 过整个客户区，那些像素就变成不透明，
//!   毛玻璃被自己糊住；而跑马灯每 40ms 就要重画一次，必然踩到这一点。
//! * 反过来，如果底板窗口老老实实不画，文字又没地方放。
//!
//! 所以拆成**下层毛玻璃底板窗口 + 上层 per-pixel alpha 文字窗口**：
//!
//! ```text
//!   ┌────────────────────────────────────────┐
//!   │ 文本窗口  WS_EX_LAYERED | ULW_ALPHA     │ ← 每帧整块重画，字形边缘自带 alpha
//!   │           WS_EX_TRANSPARENT (恒穿透)    │
//!   ├────────────────────────────────────────┤
//!   │ 底板窗口  Acrylic accent，从不绘制      │ ← 只有 DWM 在画它，永远不会被污染
//!   │           WS_EX_TRANSPARENT (可切换)    │
//!   └────────────────────────────────────────┘
//! ```
//!
//! 纯文字模式 = 把底板窗口藏起来，剩下的就是「只有字形贴在桌面上」。
//! 布局模式 = 把底板显出来当拖拽把手（顺便给用户看清可拖区域）。
//!
//! # 不干扰桌面的三道闸
//!
//! 1. `WS_EX_TRANSPARENT` —— 鼠标消息直接穿透到下层，点图标/拖文件照旧。
//! 2. `WS_EX_NOACTIVATE`  —— 永不抢焦点，正在打字的输入框不会被打断。
//! 3. `WS_EX_TOOLWINDOW`  —— 不进任务栏、不进 Alt-Tab，桌面上只有内容本身。
//!
//! 代价是窗口自己也点不到，所以所有操作都走**全局热键 + 托盘菜单**。
//! 需要鼠标直接操作时，`Ctrl+Alt+E` 进布局模式临时放行。

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{
    GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, GetMonitorInfoW, InvalidateRect,
    MonitorFromPoint, SelectObject, SetTextCharacterExtra, HBRUSH, HDC, HGDIOBJ, MONITORINFO,
    PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, RegisterHotKey, ReleaseCapture, SendInput, SetCapture, UnregisterHotKey, INPUT,
    INPUT_KEYBOARD,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetClientRect, GetCursorPos, GetMessageW, GetWindowLongPtrW, GetWindowRect,
    IsWindowVisible, KillTimer, LoadCursorW, LoadIconW, PostMessageW, PostQuitMessage,
    RegisterClassW, SetCursor, SetForegroundWindow, SetLayeredWindowAttributes,
    SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, SystemParametersInfoW, TrackPopupMenu,
    TranslateMessage, HCURSOR, MSG, WNDCLASSW,
};

use crate::autostart;
use crate::clipboard;
use crate::config::{self, Anim, Config, Mode, Orient};
use crate::config_ui::{self, ConfigUi};
use crate::favorites;
use crate::consts::*;
use crate::ffi::{
    set_window_composition_attribute, AccentPolicy, SetProcessDpiAwarenessContext,
    SetProcessDPIAware, WinCompAttrData, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use crate::gfx::{self, Fonts, Frame, QUOTE_SHRINK_STEPS};
use crate::layout::{self, Metrics, QuoteLayout};
use crate::quotes::{self, Quote};
use crate::transition;
use crate::util::*;

// ---------------------------------------------------------------- 标识

const PANEL_CLASS: &str = "DeskGeyanPanel";
const TEXT_CLASS: &str = "DeskGeyanText";
const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 40;

/// `panel_proc` 收到的消息计数（`--dragbench` / `--memtest` 用）。
///
/// 用静态原子量而不是 `App` 上的字段 —— 这不是洁癖。整个程序靠
/// `APP_PTR`（`thread_local!` 里的 `Cell`）让 wndproc 拿到 `&mut App`，
/// 而消息泵里
/// **同一个 `App` 同时存在多个 `&mut` 别名**（调用方的那个 + wndproc 里的那个），
/// 这本身就是 UB；实测把计数器挂在 `App` 上时它**恒为 0**：编译器按 `&mut` 独占的
/// 假设把读写提进了寄存器，wndproc 的写入根本落不到测量方看得见的地方。
/// 诊断这种事不该趟这个浑水，独立原子量最省心。
///
/// 按 `msg & 0xFF` 索引：拖动期间会出现的消息都在值域内且互不碰撞
/// （`WM_MOUSEMOVE`=0x00、`WM_MOVE`=0x03、`WM_SIZE`=0x05、`WM_PAINT`=0x0F、
/// `WM_SETCURSOR`=0x20、`WM_WINDOWPOSCHANGED`=0x47、`WM_NCHITTEST`=0x84）。
#[allow(clippy::declare_interior_mutable_const)] // 数组 repeat 只能靠 const 项，见上
static MSG_TALLY: [AtomicU32; 256] = {
    const Z: AtomicU32 = AtomicU32::new(0);
    [Z; 256]
};

/// 我们自己的窗口**收到**的鼠标移动消息数（`--clicktest` 用）。
///
/// 这是唯一能证明「鼠标真的穿透了」的量。样式级的检查证明不了行为 ——
/// `WS_EX_TRANSPARENT` 在**非分层**顶层窗口上不生效：本项目的底板就是带着这个
/// 属性，却照样把整块面积的点击吃掉了（`--probe` 因此一路绿灯而实际是坏的）。
/// 唯一可信的判据是「把真实光标放到挂件上，看消息有没有落进我们的窗口过程」：
/// 穿透成立时它必须恒为 0。
static WIDGET_MOUSE_HITS: AtomicU32 = AtomicU32::new(0);

/// `WIDGET_MOUSE_HITS` 的窄访问器，给 `bench::mouse_moves_at` 用。
///
/// 计数器本身留在 `app`：它由 `panel_proc` 在「恒在最前面」的位置自增
/// （见那段注释），移到 bench 会把「计数」和「读计数」拆到两个模块，
/// 而正确性全系于「计数的位置在不在所有判断之前」。
pub(crate) fn widget_mouse_hits() -> u32 {
    WIDGET_MOUSE_HITS.load(Ordering::Relaxed)
}

/// 同上，清零用。
pub(crate) fn widget_mouse_hits_reset() {
    WIDGET_MOUSE_HITS.store(0, Ordering::Relaxed);
}

fn tally_reset() {
    for c in MSG_TALLY.iter() {
        c.store(0, Ordering::Relaxed);
    }
}

fn tally_total() -> u32 {
    MSG_TALLY.iter().map(|c| c.load(Ordering::Relaxed)).sum()
}

/// 窗口的**目标位置**（`cfg.x/y`）。
///
/// 必须走裸指针读，不能直接用调用方的 `&mut App`：调用方持有的那个 `&mut`
/// 和 wndproc 里 `app_ready()` 返回的 `&mut` 是**同一个对象的两个别名**，
/// 编译器有权按「`&mut` 独占」的假设把调用方的读提升成循环外常量。
///
/// 实测就是这样：`bench_drag` 里直接读 `a.cfg.x`，量出来的「滞后」恒等于
/// 「窗口走了多远」—— 一眼看去像窗口全程没动，实际它跟得好好的，
/// 只是调用方看不到 wndproc 写进去的值。诊断代码读状态一律走这里。
fn target_pos() -> (i32, i32) {
    let p = app_ptr_raw();
    if p.is_null() {
        return (0, 0);
    }
    unsafe { ((*p).cfg.x, (*p).cfg.y) }
}

/// 计数非零的消息槽，按条数降序。标定输出直接用它。
fn tally_rows() -> Vec<(u32, u32)> {
    let mut v: Vec<(u32, u32)> = MSG_TALLY
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let n = c.load(Ordering::Relaxed);
            (n > 0).then_some((i as u32, n))
        })
        .collect();
    v.sort_by_key(|r| std::cmp::Reverse(r.1));
    v
}

/// 整体不透明度落到毛玻璃底板上的系数。
/// 底板比文字略淡一点，玻璃感更自然（文字要顶住可读性）。
const PANEL_FACTOR: f32 = 0.90;
const PANEL_R: u32 = 18;
const PANEL_G: u32 = 20;
const PANEL_B: u32 = 26;

// 热键 id
const HK_TOGGLE: i32 = 101;
const HK_ANIM: i32 = 102;
const HK_MODE: i32 = 103;
const HK_LAYOUT: i32 = 104;
const HK_UP: i32 = 105;
const HK_DOWN: i32 = 106;
const HK_PREV: i32 = 107;
const HK_NEXT: i32 = 108;
const HK_TOPMOST: i32 = 109;
const HK_QUIT: i32 = 110;
const HK_HIDE: i32 = 111;
const HK_QUOTES: i32 = 112;
const HK_CLOCK: i32 = 113;
const HK_DATE: i32 = 114;
const HK_SIZE_UP: i32 = 115;
const HK_SIZE_DOWN: i32 = 116;
const HK_COLOR: i32 = 117;
const HK_FAV: i32 = 118;
/// 打开 / 置前设置面板（选字体等）
const HK_CONFIG: i32 = 119;

// 菜单 id。
//
// `pub(crate)` 是给**设置面板**（`config_ui`）用的：面板上的每一个控件都直接发这里
// 的命令码，走 `on_menu()` 那一条路。于是「一个设置到底怎么改、改完要重绘什么」
// 全 crate 只有一处定义 —— 面板只负责「把这个命令发出去」，以及「从 `cfg` 读回
// 当前值显示出来」。两份实现必然走偏（漏一次 `save()`、漏一次 `render_and_present`）。
pub(crate) const MI_TOGGLE: u32 = 1;
pub(crate) const MI_GLASS: u32 = 2;
pub(crate) const MI_TEXT: u32 = 3;
pub(crate) const MI_ANIM_BASE: u32 = 10; // 10..14 对应四个动效
pub(crate) const MI_OP_UP: u32 = 20;
pub(crate) const MI_OP_DOWN: u32 = 21;
pub(crate) const MI_TOPMOST: u32 = 22;
pub(crate) const MI_LAYOUT: u32 = 23;
pub(crate) const MI_SECONDS: u32 = 24;
pub(crate) const MI_SHADOW: u32 = 25;
pub(crate) const MI_SPEED_UP: u32 = 26;
pub(crate) const MI_SPEED_DOWN: u32 = 27;
pub(crate) const MI_PREV: u32 = 30;
pub(crate) const MI_NEXT: u32 = 31;
pub(crate) const MI_RELOAD: u32 = 32;
pub(crate) const MI_RESET: u32 = 33;
// 显示内容
pub(crate) const MI_CLOCK: u32 = 40;
pub(crate) const MI_DATE: u32 = 41;
pub(crate) const MI_QUOTE: u32 = 42;
// 外观
pub(crate) const MI_SCALE_UP: u32 = 43;
pub(crate) const MI_SCALE_DOWN: u32 = 44;
pub(crate) const MI_CLOCK_UP: u32 = 45;
pub(crate) const MI_CLOCK_DOWN: u32 = 46;
pub(crate) const MI_QUOTE_UP: u32 = 47;
pub(crate) const MI_QUOTE_DOWN: u32 = 48;
pub(crate) const MI_COLOR: u32 = 49;
pub(crate) const MI_DATE_COLOR: u32 = 50;
pub(crate) const MI_FONT: u32 = 51;
pub(crate) const MI_CLOCK_RESET: u32 = 52;
pub(crate) const MI_QUIT: u32 = 99;
// 系统行为（开机自启 / 全屏自动隐藏）。放在 50+ 段，不和外观项混。
pub(crate) const MI_AUTOSTART: u32 = 60;
pub(crate) const MI_AUTOHIDE: u32 = 61;
pub(crate) const MI_FAV: u32 = 62;
/// 版式单选命令：70 / 71 对应横版 / 竖版（设置面板用；菜单已不重复列设置项）
pub(crate) const MI_ORIENT_BASE: u32 = 70;
pub(crate) const MI_ORIENT_V: u32 = MI_ORIENT_BASE + 1;
pub(crate) const MI_SPACING_UP: u32 = 72;
pub(crate) const MI_SPACING_DOWN: u32 = 73;
pub(crate) const MI_QUOTE_COLOR: u32 = 74;
pub(crate) const MI_SHRINK_FIT: u32 = 75;
/// 打开设置面板（字体列表等）
pub(crate) const MI_CONFIG: u32 = 76;

/// 时间日期块拖动命中的最小判定范围（避免块很小时抓不住）
const CLOCK_MIN_GRAB: i32 = 28;

// ---------------------------------------------------------------- 全局状态
//
// 刻意用裸指针而不是 Mutex：
// `TrackPopupMenu` / `DestroyWindow` / `ShowWindow` 会在**同一线程内同步派发**
// 消息回到我们自己的 wndproc。如果状态挂在 Mutex 后面，这就是一次必然的自锁。
// 程序是严格单线程的（所有窗口同属创建它们的那个线程），裸指针反而更诚实。
//
// 用 `thread_local!` + `Cell` 而不是 `static mut`，两个理由：
//
// 1. `static mut` 在 Rust 2024 里碰它要先写 `unsafe`，而且它给的是「随便改」
//    的权力 —— 这里要的只是「存一个指针、之后把它取出来」这么一件事。
// 2. **`Cell` 不是 `Sync`，所以它压根不能做成普通 `static`**（编译器会拿
//    E0277 拦下来）。放进 `thread_local!` 之后，「只有创建窗口的那个线程
//    摸得到这个指针」不再是注释里的一句承诺，而是类型系统的事实 ——
//    将来谁真从别的线程调 `app()`，会 panic 在 TLS 访问上，而不是安静地
//    踩一次数据竞争。对「严格单线程」这个前提来说，这是它该有的强度。
//
// 代价是每次访问多一次 TLS 查找（`Cell::get` 在 `thread_local!` 里）。实测
// 不到一个纳秒级，而 `app()` 的调用点全在消息处理里，那儿随便一条消息就是
// 几十微秒 —— 量级上不成立，不必为此引入 `unsafe`。

thread_local! {
    static APP_PTR: Cell<*mut App> = const { Cell::new(null_mut()) };
}

/// 存下 `App` 的地址（`Box::into_raw` 出来的那个），之后 `app()` 才拿得到。
///
/// 这是**唯一**能改 `APP_PTR` 的地方。以前 `static mut` 意味着任何代码都能
/// 伸手写一把；现在要过这一道，写入点才数得过来（就是 `bootstrap`）。
fn app_ptr_set(p: *mut App) {
    APP_PTR.with(|c| c.set(p));
}

/// `APP_PTR` 的裸值（不判空），给 `target_pos()` 用 —— 它读的是 wndproc
/// 写进去的 `cfg.x/y`，必须在诊断侧绕过 `&mut App` 那套别名，见那边的注释。
fn app_ptr_raw() -> *mut App {
    APP_PTR.with(|c| c.get())
}

fn app() -> &'static mut App {
    let p = app_ptr_raw();
    // 进程里所有入口都先经过 `bootstrap`（那里 set），所以非空是既定事实。
    // 真为空说明有人漏了初始化 —— 直接解引用会崩在 Windows 异常里，
    // 不如在这里炸出一句能定位的话。
    assert!(!p.is_null(), "APP_PTR 未初始化：调用 app() 前必须先 bootstrap");
    unsafe { &mut *p }
}

pub(crate) fn app_ready() -> Option<&'static mut App> {
    let p = app_ptr_raw();
    if p.is_null() {
        None
    } else {
        Some(unsafe { &mut *p })
    }
}

/// 本模块的 `HINSTANCE`。建窗口时要它 —— 设置面板（`config_ui`）在另一个模块里，
/// 拿不到 `App.hinst`（那是私有字段），而这个句柄对当前进程是唯一的，
/// 问系统要一次比敞一个字段干净。
pub(crate) fn module_handle() -> *mut c_void {
    unsafe { GetModuleHandleW(null()) }
}

// ---------------------------------------------------------------- 状态

#[derive(Clone, Copy, PartialEq, Eq)]
enum Glass {
    /// Acrylic 成功，底板交给 DWM 画
    Acrylic,
    /// Acrylic 不可用，退化为「整窗 alpha + 自绘实心底板」
    Layered,
}

pub struct App {
    hinst: *mut c_void,
    panel: HWND,
    text: HWND,
    cfg: Config,
    list: Vec<Quote>,

    qi: usize,
    qn: usize,
    /// 跑马灯已经滚过的距离
    m: f32,
    /// 当前格言宽度（px）
    cur_w: i32,
    /// 动效相位（秒）
    phase: f32,
    /// 文字整体 alpha 系数（淡入淡出 / 呼吸）
    /// 换句过渡状态机（纯逻辑在 `transition.rs`）。
    ///
    /// 它**只作用于格言通道**：compose 的 `quote_k` + 绘制端的滑入偏移。
    /// 时钟/日期/底板是静帧，全程不参与 —— 旧实现的全局 `alpha` 让
    /// Fade/Breathe 拖着时钟一起淡，那个字段已拆除。
    trans: transition::Transition,

    fonts: Fonts,
    /// 字体代次：每次 `rebuild_fonts` 就涨 1。
    ///
    /// 存在的唯一理由是给 `layout_clock` 的结果缓存当失效信号 —— 换成
    /// 「在每个改字号/字体/开关的地方手工插一句作废」，漏一处的表现是
    /// 用户改了字号而时间日期块还按旧尺寸夹着（露出一截被裁掉的边），
    /// 而这种 bug 在测试里极难复现。
    font_epoch: u64,
    frame: Frame,
    brush: HBRUSH,

    glass: Glass,
    accent_ok: bool,
    tray_ok: bool,
    /// 托盘挂载（`Shell_NotifyIconW(NIM_ADD)`）失败时的 GetLastError。
    /// 托盘挂不上时热键还在，但「重新载入配置 / 退出」这些菜单就没了 —— 要能说清是哪一步挂的。
    tray_err: u32,
    hotkeys: Vec<i32>,
    /// 最近一次热键注册失败的 GetLastError。用来区分「被别的实例占着」和「真失败」
    hotkey_err: u32,
    /// 布局模式下鼠标正抓着什么（见 `Grab`）
    grab: Grab,
    /// 抓取时的光标位置与窗口几何基准。存的都是**已经应用到窗口上的实际值**，
    /// 不依赖 `GetCursorPos()` —— 于是整条拖动链路是可复现的事件序列（见 `--dragtest`）。
    grab_pt: (i32, i32),
    grab_win: (i32, i32, i32, i32),
    grab_off: (i32, i32),
    /// 内容有变化、等着出帧
    dirty: bool,
    /// 上次上屏的时刻。拖动期间用它把重绘节流到 8ms 一帧：
    /// 状态照收（每条鼠标消息都即时更新），画面按节拍出。
    last_present: std::time::Instant,
    /// 累计出帧数。测试靠它证明「移动期间一次整帧重绘都没发生」。
    renders: u32,
    /// 复现旧实现的开关：整窗移动时也逐条消息跑整帧重绘。
    /// 只给 `--perf` 做 A/B 用 —— 留着它，这次优化才是**可复现**的
    /// （`DG_LEGACY_DRAG=1` 就能把「粘手」那条路重新装回来）。
    legacy_drag: bool,
    /// 时间日期块在客户区里的矩形。每帧渲染时更新，供拖动命中判定读取 ——
    /// 命中测试在 wndproc 里跑，那里拿不到 DC 去量文字，所以必须缓存。
    clock_rect: RECT,
    /// `layout_clock` 的结果缓存 + 指纹 `(w, h, scale, seconds, qi, show_clock,
    /// show_date, orient, clock_dx, clock_dy, font_epoch)`。
    ///
    /// 缩放拖动时 `w/h` 逐像素变，但**时钟字面量没变**时量出来的 `cb` 只有
    /// `x/y` 在动（`w/h` 由字面量决定）。所以缓存整个 `ClockBox`，命中就直接
    /// 返回，一次 `measure_text` / `SelectObject` 都不做 —— 这是缩放拖动成本
    /// 的大头：不缓存的话每条鼠标消息要量两次文字，实测 1052µs/条，
    /// 顶到 1000Hz 鼠标的 1000µs 预算；命中后降到 ~300µs。
    ///
    /// `dx/dy` 进指纹是因为夹取会回写它们；`font_epoch` 见 `rebuild_fonts`
    /// 的递增 —— 换字体/字号/开关/换句都要让它涨一次，否则量的是旧尺寸。
    clock_cached: Option<Box<ClockBox>>,
    clock_norm_key: (i32, i32, i32, i32, i32, u32, u32, u32, i32, i32, u64),
    /// 主时钟最后出帧时的「时间戳」（时/分/秒），静态画面下用它决定要不要重画
    clock_stamp: u64,
    /// 跑马灯首帧：格言应当已经在带内，而不是让用户先看一段空白
    primed: bool,
    last_tick: std::time::Instant,
    /// PR2 悬停暂停：光标悬停累计器（纯逻辑在 `transition::hover_decide`）
    hover: transition::HoverState,
    /// PR2 悬停暂停的**生效值**：true 时 dwell/跑马灯冻结、needs_frame 排除出帧。
    /// 每拍由 tick 重算；自检直接置位来驱动 step/needs_frame 的边界。
    hover_paused: bool,
    /// 上次把窗口位置真正落到窗口上的时刻（节流用，见 `DRAG_MOVE_MS`）
    last_move: std::time::Instant,
    /// 移动窗口走 `SetWindowPos` 还是旧的 `UpdateLayeredWindow`
    move_way: MoveWay,
    /// 窗口位置落定的最小间隔（ms）。`--dragbench` 会把它调成 0 做 A/B
    move_throttle: u128,
    /// 按下时算好的命中带。拖动期间光标形状直接读它 ——
    /// `WM_SETCURSOR` 每次鼠标移动都会来，里面再去 `GetCursorPos` + `GetWindowRect`
    /// 纯属白花：抓取期间窗口一直在动，算出来的还真不如按下那一刻准。
    grab_zone: isize,
    /// 上次查询「用户通知状态」的时刻。1Hz 轮询，不是每拍都问 ——
    /// 那是跨进程的 shell 查询，25Hz 去问纯属浪费。
    fs_last: std::time::Instant,
    /// 当前是否处于全屏 D3D 应用（诊断用：`--probe` 报它）
    fs_active: bool,
    /// 窗口当前是不是**因为全屏**而藏着的。手动 `visible` 一变就清零 ——
    /// 用户意图永远压过自动行为，否则退出全屏时会弹回用户刚亲手关掉的窗口。
    fs_hidden: bool,
    /// 最近一次开机自启操作的失败码（0 = 无失败）。菜单里没有地方弹错误框，
    /// 只能让 `--probe` 把它报出来。
    autostart_err: u32,
    /// 格言连最小档（70%）都放不下 → 这一帧该走跑马灯。
    ///
    /// 每帧由 `build_frame` 按排版结果更新。放在 App 上而不是 RC 上，
    /// 是因为 `step` / `needs_frame` 也要读它 —— 它们跑在渲染之前。
    force_marquee: bool,
    /// 设置面板的状态（窗口句柄 / 字体列表 / 该窗口自己的字体句柄）。
    ///
    /// 挂在 `App` 上而不是自己开一个全局：配置窗口的过程函数本来就走
    /// `app_ready()` 拿 `&mut App`，状态放一起就不必再引第二层裸指针。
    pub(crate) config: ConfigUi,
}

/// 布局模式下鼠标抓着什么。
///
/// 三种拖动收成一个枚举而不是几个 bool：它们各自对应一套不同的
/// 「要不要重绘 / 改哪些字段」的组合，用一个状态描述才不会出现
/// 「同时抓着重块又在缩放」这类非法中间态。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Grab {
    None,
    /// 挪整个挂件：**一个像素都不用重画**，只改窗口位置
    Move,
    /// 挪时间日期块：块的位置变了，内容要重画
    Clock,
    /// 缩放窗口：内容要重画，帧缓冲还得重建
    Edge(isize),
}

/// 拖动期间的重绘间隔（ms）。
///
/// 整帧重绘实测 ~700µs，而 1000Hz 鼠标留给一条移动消息的预算只有 1ms ——
/// 「每条消息都重绘」必然积压，用户看到的就是窗口跳着追光标。
/// 状态更新是微秒级的，所以只把**出帧**按这个节拍限速：
/// 60~120fps 的画面配上零延迟的位置，手感是「贴着手」的。
const DRAG_FRAME_MS: u128 = 8;

/// 拖动期间「把窗口真的挪到目标位置」的最小间隔（ms）。
///
/// 窗口位置**不是状态，是呈现**：像素内容和窗口在哪无关，所以位置也完全可以
/// 按节拍落定，和重绘共用同一条时间线。状态（`cfg.x/y`）仍旧每条消息即时更新 ——
/// 拖动判定、边界夹取、落盘全都以它为基准，它必须永远是「光标算出来的最新目标」；
/// 按节拍走的是「把它落到窗口上」这一步。
///
/// 为什么必须节流：1000Hz 鼠标每秒来 1000 条移动消息，若每条都真去
/// `SetWindowPos`，单这一项就是 287µs × 1000 ≈ **29% CPU**，
/// 而且这些调用整齐地挤在两次屏幕刷新之间 —— 显示器最高也就 144Hz，
/// 其中 >85% 的挪动用户一帧都看不到，却全都要在 DWM 侧排队。
/// 排队排的是**我们自己的 UI 线程**，下一批鼠标消息就只能干等。
const DRAG_MOVE_MS: u128 = 8;

/// 拖动期间定时器的周期（ms）。平时是 `TIMER_MS`(40)，拖动时临时收紧到和
/// 位置节拍同一个值。
///
/// 为什么需要它：位置落定是「有请求时才检查节拍」的，而**用户按着不动时没有请求** ——
/// 最后一次移动会被节流欠着，窗口停在 8ms 前的位置等下一次鼠标事件。
/// 拖着不停时看不出来（消息源源不断），但「停手但仍按住」就会看到窗口欠一格。
/// 让定时器在拖动期间按同一个节拍跑，就能把欠的那一格补上。
const DRAG_TICK_MS: u32 = 8;

/// 移动窗口用哪条路。
///
/// **实测（`--dragbench`）结论和最初的直觉相反，所以两个都留着、可切**：
///
/// | 路径 | 单次成本 |
/// | --- | --- |
/// | 底板 `SetWindowPos`（Acrylic 显形） | ~266 µs |
/// | 底板 `SetWindowPos`（隐藏时） | ~130 µs |
/// | 文字层 `SetWindowPos` | ~206 µs |
/// | **文字层 `UpdateLayeredWindow`** | **~91 µs** |
///
/// 只要改位置，`UpdateLayeredWindow` 比 `SetWindowPos` 快一倍多：
/// `SetWindowPos` 要跑完整套窗口变更流程（`WM_WINDOWPOSCHANGING` /
/// `WM_NCCALCSIZE` / `WM_WINDOWPOSCHANGED`…），而 ULW 是专门给分层窗口
/// 开的那条路。它确实会重新提交一块表面，但同一进程内的拷贝比那套流程便宜。
///
/// 所以**移动走 ULW、改尺寸走 `SetWindowPos`**（见 `apply_geometry`）：
/// ULW 顺便把尺寸也定了，而这时候帧缓冲还没按新尺寸重建，尺寸会打架。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MoveWay {
    /// `UpdateLayeredWindow` 挪位置：分层窗口的专线，**默认**
    Ulw,
    /// `SetWindowPos` 挪位置：给 `--dragbench` 做 A/B
    Swp,
}

impl App {
    fn new() -> Option<App> {
        let cfg = Config::load();
        let fonts = Fonts::build(&cfg.font, cfg.scale, cfg.clock_scale, cfg.quote_scale);
        let frame = Frame::new(cfg.w, cfg.h);
        // 分隔线画刷：走**主色通道**、强度 0x50。
        // 旧值 0x505050 是三通道同强度，在只读 byte0 的旧实现里恰好等于 0x50；
        // 换到「通道即语义」的新布局就必须只说一个通道，否则会被判成三色叠加。
        let brush = unsafe { CreateSolidBrush(gfx::ch_dim(0x50)) };
        let list = quotes::library();
        if list.is_empty() {
            return None;
        }
        Some(App {
            hinst: null_mut(),
            panel: null_mut(),
            text: null_mut(),
            qi: 0,
            qn: 1 % list.len(),
            m: 0.0,
            cur_w: 0,
            phase: 0.0,
            // 减动开关：系统级的（WinRT UISettings）要引 windows crate，暂不接；
            // env 覆盖给便携用户和自检用（见 transition::Transition::new）。
            trans: transition::Transition::new(std::env::var("DG_NO_TRANSITION").is_ok()),
            cfg,
            list,
            fonts,
            font_epoch: 1,
            frame,
            brush,
            glass: Glass::Acrylic,
            accent_ok: false,
            tray_ok: false,
            tray_err: 0,
            hotkeys: Vec::new(),
            hotkey_err: 0,
            grab: Grab::None,
            grab_pt: (0, 0),
            grab_win: (0, 0, 0, 0),
            grab_off: (0, 0),
            dirty: true,
            last_present: std::time::Instant::now(),
            renders: 0,
            legacy_drag: std::env::var("DG_LEGACY_DRAG").is_ok(),
            clock_rect: unsafe { zeroed() },
            // 空缓存 + 不可能的指纹，保证首次一定量一次
            clock_cached: None,
            clock_norm_key: (
                i32::MIN,
                i32::MIN,
                i32::MIN,
                i32::MIN,
                i32::MIN,
                u32::MAX,
                u32::MAX,
                u32::MAX,
                i32::MIN,
                i32::MIN,
                0,
            ),
            clock_stamp: u64::MAX,
            primed: false,
            last_tick: std::time::Instant::now(),
            hover: transition::HoverState::default(),
            hover_paused: false,
            last_move: std::time::Instant::now(),
            move_way: match std::env::var("DG_MOVE").ok().as_deref() {
                Some("swp") => MoveWay::Swp,
                _ => MoveWay::Ulw,
            },
            move_throttle: match std::env::var("DG_MOVE_THROTTLE").ok().as_deref() {
                Some("0") => 0,
                _ => DRAG_MOVE_MS,
            },
            grab_zone: HTCLIENT,
            // 全屏检测的时钟从「now」起算：`App::new` 之后紧接着就有一次 tick，
            // 首拍就查（`fs_last` 初始为 now，等 1s 后才会真查）反而更稳 ——
            // 启动时如果已经在全屏游戏里，1s 内就该藏掉。
            fs_last: std::time::Instant::now(),
            fs_active: false,
            fs_hidden: false,
            autostart_err: 0,
            force_marquee: false,
            config: ConfigUi::default(),
        })
    }

    fn should_show_panel(&self) -> bool {
        self.cfg.mode == Mode::Glass || self.cfg.layout_mode
    }

    /// 底板的不透明度：整体不透明度 × 底板系数。
    ///
    /// **不再跟呼吸联动** —— 呼吸已收窄为只作用于格言通道，底板是静帧：
    /// 它跟着起伏，等于把「时钟日期被绑架」的旧病留在底板里。
    fn panel_alpha(&self) -> u8 {
        let a = self.cfg.opacity as f32 * PANEL_FACTOR;
        a.round().clamp(0.0, 255.0) as u8
    }

    /// 文字层的不透明度：就是用户设的整体不透明度。
    ///
    /// 动效的明暗**不走这里** —— 换句过渡只缩放格言通道（compose 的
    /// `quote_k`），时钟/日期/底板全程不动。全局 alpha 是旧实现把
    /// Fade/Breathe 演成「整块内容一起淡」的根因，已拆除。
    fn text_alpha(&self) -> u8 {
        (self.cfg.opacity as f32).round().clamp(0.0, 255.0) as u8
    }

    /// 本帧格言的调制：`(覆盖率 k, x 偏移 px)`。
    ///
    /// 过渡优先于呼吸（呼吸模式下根本不启动过渡，两者不会同时出现）。
    /// k 由 compose 乘在格言通道上，偏移由绘制端加在格言字形坐标上 ——
    /// **两趟必须用同一对值**（掩码趟与阴影趟几何一致是铁律）。
    ///
    /// **减动模式最早返回**：过渡与呼吸一起归零。呼吸是常驻振荡，对
    /// 「减少动态效果」的用户同样是打扰，只减过渡不减呼吸是半成品（PR1.1）。
    fn quote_mod(&self) -> (f32, i32) {
        if self.trans.reduced() {
            return (1.0, 0);
        }
        if self.trans.is_active() {
            (self.trans.quote_k(), self.trans.slide_x())
        } else if self.cfg.eff_anim() == Anim::Breathe {
            (transition::breathe_k(self.phase), 0)
        } else {
            (1.0, 0)
        }
    }

    /// 自检用的「本帧格言 k」—— soak 要验它始终落在 [0,1]。
    fn eff_quote_k(&self) -> f32 {
        self.quote_mod().0
    }

    /// 字号 / 字体变了就重建字体对象（GDI 字体不可变），
    /// 并刷新跑马灯的宽度度量 —— 不刷新的话滚动节奏会跟字形对不上。
    fn rebuild_fonts(&mut self) {
        self.fonts.destroy();
        self.fonts = Fonts::build(
            &self.cfg.font,
            self.cfg.scale,
            self.cfg.clock_scale,
            self.cfg.quote_scale,
        );
        let q = self.list[self.qi].clone();
        self.cur_w = measure_quote(self, &q);
        // 关键：不能把 m 归零 —— m=0 意味着格言整条还在带外，
        // 换字体的瞬间当前这条会「消失」再从右边滑进来。
        // 直接停到带左缘，改完字号立刻就能看到效果。
        self.m = marquee_band_w(&self.cfg);
        // 字号变了，块的大小跟着变，偏移得重新夹一次。`font_epoch` 一涨，
        // `layout_clock` 的缓存指纹就失配，下面的夹取量到的是新尺寸。
        self.font_epoch = self.font_epoch.wrapping_add(1);
        normalize_clock_offset(self);
    }

    /// 时间日期块是否显示
    fn clock_visible(&self) -> bool {
        self.cfg.show_clock || self.cfg.show_date
    }

    // ---- 给设置面板（`config_ui`）的窄接口。
    //
    // **读**走 `cfg()`（只读借用），**写**一律走 `on_menu()` / `set_font()`。
    // 不提供 `cfg_mut()`：面板一旦能直接改 `cfg`，就会出现「改了没 `save()`」
    // 「改了没重绘」「改了没走 `apply_mode`」这类漏项 —— 而那些步骤正好是
    // `on_menu` 存在的理由。面板要做的只有两件事：显示当前值、把命令发出去。

    /// 只读配置。设置面板用它把每一项的当前值显示出来。
    pub(crate) fn cfg(&self) -> &Config {
        &self.cfg
    }

    /// 当前字体名
    pub(crate) fn font_face(&self) -> &str {
        &self.cfg.font
    }

    /// 「选择即生效」的全部：改字体 → 重建字体对象 → 落盘 → 重绘挂件。
    /// 和托盘菜单里换字体走的是同一条路径（`MI_FONT` 那几行），行为一致。
    pub(crate) fn set_font(&mut self, face: &str) {
        self.cfg.font = face.to_string();
        self.rebuild_fonts();
        self.cfg.save();
        render_and_present(self);
    }
}

// ---------------------------------------------------------------- 建窗引导

/// 建窗口 → 应用状态 → 装托盘与热键 → 起定时器 → 出第一帧。
/// `run` 和 `probe` 共用这一条路径，保证「探测到的」就是「跑起来的」。
unsafe fn bootstrap(a: &mut App) -> bool {
    let panel = match create_panel(a) {
        Some(h) => h,
        None => return false,
    };
    a.panel = panel;
    let text = match create_text(a) {
        Some(h) => h,
        None => return false,
    };
    a.text = text;

    raise_pair(a);
    apply_topmost(a);
    apply_mode(a);
    register_hotkeys(a);
    add_tray(a);
    SetTimer(text, TIMER_ID, TIMER_MS, None);
    render_and_present(a);
    // 底板的可见性**只**由 `apply_mode → apply_glass` 决定，这里绝不能再补一次
    // 无条件 `ShowWindow`。底板是 topmost 且**非 layered** 的窗口，而
    // `WS_EX_TRANSPARENT` 在非分层顶层窗口上**不生效**（见 README「鼠标穿透」）——
    // 一旦它在「本该隐藏」的时候被显示出来，就成了一块看不见却把整块面积的
    // 鼠标点击全部吃掉的窗口：用户看不到任何东西，却点不动底下的窗口。
    // 纯文字模式（mode=text 且不在布局模式）下它必须保持隐藏。
    // 文字层没有这个问题：它恒为 layered + transparent，是真正穿透的。
    ShowWindow(text, SW_SHOWNOACTIVATE);
    if !a.cfg.visible {
        set_visible(a, false);
    }
    true
}

// ---------------------------------------------------------------- 自检

/// `desk-geyan --selftest`：不建窗口，直接把一帧走完整条渲染管线，
/// 然后按**预乘 ARGB 的数学约束**逐个像素验算。
///
/// 这些断言不是装饰 —— 它们检查的正是「文字能不能真的贴上去」：
///   * 颜色分量不能超过 alpha（预乘的硬性要求，违反 DWM 会渲染错）
///   * 必须存在大量 0<a<255 的半透明像素（证明是逐像素 alpha，
///     而不是 LWA_COLORKEY 那种非 0 即 255 的硬抠图）
///   * 素材区域必须有足够的不透明像素（证明字真的画出来了，不是空帧）
pub fn selftest() -> i32 {
    let Some(mut a) = App::new() else {
        println!("FAIL  初始化失败");
        return 3;
    };
    // 自检要用确定的默认参数，不受用户配置影响
    a.cfg = Config::default();
    a.cfg.shadow = true;
    // 有几条断言必须直接看**掩码**（「阴影有没有跑到没字形的地方」这类
    // 在合成完的像素上根本分不出来），而 `compose` 是就地覆写掩码位图的 ——
    // 所以整个自检期间打开快照开关。生产路径不打开，只有一次 atomic load。
    // `DevFlags` 是 `OnceLock`：置位只能一次，且必须在任何渲染之前。
    // 自检期全程要看掩码，所以这里一次性置位（`no_batch` 走逐帧显式参数，
    // 不在这里占位 —— 见 `build_frame_ex`）。
    gfx::set_dev_flags(gfx::DevFlags {
        keep_mask: true,
        ..Default::default()
    });
    a.fonts.destroy();
    a.fonts = Fonts::build(&a.cfg.font, a.cfg.scale, a.cfg.clock_scale, a.cfg.quote_scale);
    a.list = quotes::library();
    a.qi = 0;
    a.qn = 1 % a.list.len();
    a.cur_w = 0;
    // 把跑马灯推到带内，让自检覆盖「时钟 + 分隔线 + 格言」的完整布局
    a.m = 260.0;
    a.primed = true;

    let op = a.text_alpha();
    build_frame(&mut a, op);

    let (fw, fh) = (a.frame.w, a.frame.h);
    let px = a.frame.out_pixels();
    let total = px.len();
    let (mut solid, mut partial, mut overflow, mut colored) = (0usize, 0usize, 0usize, 0usize);
    let mut alpha_sum = 0u64;
    let mut alpha_max = 0u32;
    for p in px {
        let al = (p >> 24) & 0xFF;
        let (r, g, b) = ((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF);
        // 全覆盖率字形（含其上的阴影层）的 alpha 会落在 op..255 之间
        if al >= 200 {
            solid += 1;
        } else if al > 0 {
            partial += 1;
        }
        if r > al || g > al || b > al {
            overflow += 1;
        }
        if !(r == g && g == b) {
            colored += 1;
        }
        alpha_sum += al as u64;
        alpha_max = alpha_max.max(al);
    }

    let mut rows: Vec<(&str, bool, String)> = Vec::new();

    // ---- 动效状态机 soak：四种动效各跑 180 帧 × 0.1s = 18 秒动画时间。
    // 这个时长足以跨过跑马灯的轮转点(约 14.5s)、静态/淡入淡出的 dwell
    // （4~10s）+ 过渡（0.56s）、呼吸的一个周期（5s），把每条状态迁移都真正走一遍。
    a.cfg.speed = 200; // 加快跑马灯，让轮转在 soak 内发生不止一次
    for an in Anim::all() {
        a.cfg.anim = an;
        a.trans.settle();
        a.phase = 0.0;
        a.m = 0.0;
        a.primed = true;
        let mut ok = true;
        let mut why = String::from("180 帧无异常");
        let mut bad_px = 0usize;
        let mut rotations = 0usize;
        let mut last_qi = a.qi;
        // 换句时过渡是否在场。跑马灯的轮转不走过渡（它自己无缝），
        // 只对 Static/Fade 要求「换句必伴随过渡」—— 这条断言抓的就是硬切。
        let wants_transition = matches!(an, Anim::Static | Anim::Fade);
        let mut hard_cuts = 0usize;
        let mut kmin = 2.0f32;
        let mut kmax = -1.0f32;
        for i in 0..180 {
            step(&mut a, 0.1);
            let k = a.eff_quote_k();
            if !(k.is_finite() && (0.0..=1.0).contains(&k)) {
                ok = false;
                why = format!("第 {} 帧 格言 k={} 越界", i, k);
                break;
            }
            kmin = kmin.min(k);
            kmax = kmax.max(k);
            if !a.m.is_finite() || a.m.abs() > 1.0e6 {
                ok = false;
                why = format!("第 {} 帧 跑马灯偏移 m={} 异常", i, a.m);
                break;
            }
            if a.qi >= a.list.len() || a.qn >= a.list.len() || a.qi == a.qn {
                ok = false;
                why = format!("第 {} 帧 格言下标异常 {} / {}", i, a.qi, a.qn);
                break;
            }
            if a.qi != last_qi {
                rotations += 1;
                // Swap 在 Leave→Gap 边界发生：换句那一刻过渡必然还在场上
                // （Gap/Arrive）。Dwell 且无过渡 = 零帧硬切，正是要抓的旧病。
                if wants_transition && !a.trans.is_active() {
                    hard_cuts += 1;
                }
                last_qi = a.qi;
            }
            let op = a.text_alpha();
            build_frame(&mut a, op);
            if i % 20 == 0 {
                for p in a.frame.out_pixels() {
                    let al = (p >> 24) & 0xFF;
                    if ((p >> 16) & 0xFF) > al || ((p >> 8) & 0xFF) > al || (p & 0xFF) > al {
                        bad_px += 1;
                    }
                }
            }
        }
        if bad_px > 0 {
            ok = false;
            why = format!("{} px 预乘非法", bad_px);
        }
        if hard_cuts > 0 {
            ok = false;
            why = format!("{} 次换句是零帧硬切", hard_cuts);
        }
        rows.push((
            an.label(),
            ok,
            format!(
                "{}（换句 {} 次，格言 k {:.2}..{:.2}）",
                why, rotations, kmin, kmax
            ),
        ));
    }
    a.cfg.speed = 60;

    // 回到默认动效，后面的行继续沿用
    a.cfg.anim = Anim::Marquee;
    a.trans.settle();
    a.phase = 0.0;
    a.m = 260.0;
    let op2 = a.text_alpha();
    build_frame(&mut a, op2);

    // ---- PR1 · 分通道 alpha：过渡只缩放格言，时钟/日期逐位不动。
    //
    // 做法：同一配置出两帧 —— 稳态（k=1）与过渡半程（Leave t=0.08s ⇒
    // k = 1 − ease_in(0.5) = 0.875），逐像素比 alpha。滑入置 0，这条只验
    // 「谁被缩放」，不掺位移（位移由 transition 的单测覆盖）。
    // 关阴影：阴影 underlay 会给「alpha 比 ≈ k」引入非线性项，
    // 阴影同步是下一条，分开测。
    {
        let shadow_was = a.cfg.shadow;
        a.cfg.anim = Anim::Static;
        a.trans.settle();
        a.phase = 0.0;
        a.cfg.shadow = false;
        let op = a.text_alpha();
        build_frame(&mut a, op);
        let v1: Vec<u32> = a.frame.out_pixels().to_vec();
        // 掩码快照（compose 之前取的）：按通道分清每个像素归谁
        let m0 = a.frame.channel_mask(0);
        let m1 = a.frame.channel_mask(1);
        let m2 = a.frame.channel_mask(2);
        a.trans.start_leave(0);
        a.trans.step(transition::LEAVE_S / 2.0);
        build_frame(&mut a, op);
        let v2: Vec<u32> = a.frame.out_pixels().to_vec();
        let qk_i = (a.eff_quote_k() * 255.0).round() as u32;
        let (mut rest_like, mut quote_like, mut multi, mut bad) =
            (0usize, 0usize, 0usize, 0usize);
        for i in 0..v1.len() {
            let (c0, c1, c2) = (m0[i] as u32, m1[i] as u32, m2[i] as u32);
            let live = (c0 > 0) as u32 + (c1 > 0) as u32 + (c2 > 0) as u32;
            if live == 0 {
                continue;
            }
            if live > 1 {
                // 两条通道在同一像素都有墨（用户把时钟拖到格言上才会出现）——
                // 这种像素的归属由「缩放后谁强」决定，不参与本条的比例判据
                multi += 1;
                continue;
            }
            let a1 = (v1[i] >> 24) & 0xFF;
            let a2 = (v2[i] >> 24) & 0xFF;
            if c2 > 0 {
                quote_like += 1;
                // compose 的两段整数除法：qa = (byte·op/255)·qk/255
                let want = a1 * qk_i / 255;
                if (a2 as i32 - want as i32).abs() > 1 {
                    bad += 1;
                }
            } else {
                rest_like += 1;
                if v2[i] != v1[i] {
                    bad += 1; // 时钟/日期必须逐位相同
                }
            }
        }
        rows.push((
            "PR1·分通道 alpha：只缩放格言",
            bad == 0 && quote_like > 100 && rest_like > 100,
            format!(
                "时钟/日期 {} px 逐位相同；格言 {} px alpha×{:.3}；多通道重叠 {} px；异常 {}",
                rest_like,
                quote_like,
                qk_i as f32 / 255.0,
                multi,
                bad
            ),
        ));
        a.cfg.shadow = shadow_was;
    }

    // ---- PR1 · 阴影同步：格言淡下去时，它自己的阴影按同一比例压暗。
    // 只留格言（关时钟/日期）：否则阴影总量里混着不缩放的份，比值说不清。
    {
        let (sc, sd) = (a.cfg.show_clock, a.cfg.show_date);
        a.cfg.show_clock = false;
        a.cfg.show_date = false;
        a.cfg.shadow = true;
        a.cfg.anim = Anim::Static;
        a.trans.settle();
        a.phase = 0.0;
        let op = a.text_alpha();
        build_frame(&mut a, op);
        let s1: u64 = a.frame.shadow_mask().iter().map(|&v| v as u64).sum();
        a.trans.start_leave(0);
        a.trans.step(transition::LEAVE_S / 2.0);
        build_frame(&mut a, op);
        let s2: u64 = a.frame.shadow_mask().iter().map(|&v| v as u64).sum();
        let k = a.eff_quote_k();
        let ratio = s2 as f64 / s1.max(1) as f64;
        rows.push((
            "PR1·阴影同步：格言的阴影跟着 k 缩放",
            s1 > 1000 && (ratio - k as f64).abs() < 0.02,
            format!("阴影总量 {} → {}（比值 {:.3}，k {:.3}）", s1, s2, ratio, k),
        ));
        a.cfg.show_clock = sc;
        a.cfg.show_date = sd;
    }

    // ---- PR1 · 零空转：过渡落定后，静态模式一拍都不多出。
    // 关时钟/日期：排除「时钟跨刻度」驱动的出帧，只留「动效自己」。
    // dwell 最少 4s，这里只跑 2s，任何一帧都不该出。
    {
        let (sc, sd) = (a.cfg.show_clock, a.cfg.show_date);
        a.cfg.show_clock = false;
        a.cfg.show_date = false;
        a.cfg.anim = Anim::Static;
        a.trans.settle();
        a.phase = 0.0;
        a.clock_stamp = clock_stamp(&a.cfg); // 此刻恒 0，刻度已同步
        a.dirty = false;
        let before = a.renders;
        for _ in 0..50 {
            step(&mut a, 0.04);
        }
        let grew = a.renders.wrapping_sub(before);
        rows.push((
            "PR1·零空转：落定后 50 拍不出帧",
            grew == 0,
            format!("多出 {} 帧（dwell 内不该有任何一帧）", grew),
        ));
        a.cfg.show_clock = sc;
        a.cfg.show_date = sd;
        // 还原 soak 留给后续断言的环境：下面「字号可配置」那几条要跑马灯
        // 把整句铺满来量墨量，留在静态的话 150% 会被缩字阶梯压回去。
        a.cfg.anim = Anim::Marquee;
        a.trans.settle();
        a.phase = 0.0;
        a.m = 260.0;
        let o = a.text_alpha();
        build_frame(&mut a, o);
    }

    // ---- PR1.1 · 四条实施偏差的修复（2026-10-15 复审登记，计划文档 12.1）
    {
        // 先把环境切成竖版静态（后面 PR3 的断言沿用同一环境，最后统一还原）
        let (w_was, h_was) = (a.cfg.w, a.cfg.h);
        a.cfg.orient = Orient::Vertical;
        a.cfg.w = 300;
        a.cfg.h = 660;
        a.cfg.anim = Anim::Static;
        a.cfg.show_clock = true;
        a.cfg.show_date = true;
        a.cfg.show_quote = true;
        a.cfg.shadow = false;
        a.trans.settle();
        a.phase = 0.0;
        a.rebuild_fonts();

        // ① 竖版过渡零位移：slide 计算必须看 orient（原先只看 anim==Fade）。
        // 走**真路径**：step() 驱动 dwell 到期 → 过渡启动，全程记录 slide_x。
        let mut slide_bad = 0usize;
        let mut went_active = false;
        let mut guard = 0;
        while guard < 3000 {
            guard += 1;
            step(&mut a, 0.05);
            if a.trans.is_active() {
                went_active = true;
                if a.trans.slide_x() != 0 {
                    slide_bad += 1;
                }
            } else if went_active {
                break;
            }
        }
        rows.push((
            "PR1.1·竖版过渡零位移（orient 进 slide 判定）",
            went_active && slide_bad == 0,
            format!(
                "过渡已启动={} · 非零位移 {} 拍（竖排列自右向左，横向滑入与字格不正交）",
                went_active, slide_bad
            ),
        ));

        // ①b 像素级补刀：稳态 vs 过渡半程，格言掩码必须**逐字节相同**
        //（slide=0 ⇒ 字形坐标不变；位移回来了这里立刻红）。
        a.trans.settle();
        a.phase = 0.0;
        let op = a.text_alpha();
        build_frame(&mut a, op);
        let mask_a: Vec<u8> = a.frame.channel_mask(2);
        a.trans.start_leave(0);
        a.trans.step(transition::LEAVE_S / 2.0);
        build_frame(&mut a, op);
        let mask_b: Vec<u8> = a.frame.channel_mask(2);
        let mask_diff = mask_a
            .iter()
            .zip(mask_b.iter())
            .filter(|(x, y)| x != y)
            .count();
        rows.push((
            "PR1.1·竖版过渡掩码不位移（像素级）",
            mask_diff == 0 && mask_a.iter().any(|&v| v > 0),
            format!("稳态 vs 半程掩码差异 {} 字节", mask_diff),
        ));

        // ② 减动降级呼吸 + ③ 启动首帧全亮度：都要**新建 App** 才验得到
        // 初始化路径（`App::new` 不建窗、不碰单实例互斥，自检里安全）。
        // env 是进程级的，用完原样还回去。
        let env_was = std::env::var("DG_NO_TRANSITION").ok();
        std::env::set_var("DG_NO_TRANSITION", "1");
        let fresh_reduced = App::new();
        match &env_was {
            Some(v) => std::env::set_var("DG_NO_TRANSITION", v),
            None => {
                std::env::remove_var("DG_NO_TRANSITION");
            }
        }
        let fresh_normal = App::new();
        let prep = |b: &mut App| {
            b.cfg = Config::default();
            b.fonts.destroy();
            b.fonts = Fonts::build(&b.cfg.font, b.cfg.scale, b.cfg.clock_scale, b.cfg.quote_scale);
            b.list = quotes::library();
            b.qi = 0;
            b.qn = 1 % b.list.len();
            b.cur_w = 0;
            b.clock_stamp = clock_stamp(&b.cfg);
        };
        if let Some(mut b) = fresh_reduced {
            prep(&mut b);
            let mut breathe_k_bad = false;
            let mut frame_bad = false;
            for an in Anim::all() {
                b.cfg.anim = an;
                b.trans.settle();
                b.phase = 0.0;
                if an == Anim::Breathe {
                    b.phase = 1.3; // 呼吸相位推进过
                    if b.eff_quote_k() != 1.0 {
                        breathe_k_bad = true;
                    }
                    if needs_frame(&b) {
                        frame_bad = true;
                    }
                }
            }
            rows.push((
                "PR1.1·减动降级呼吸（k 恒 1 且不出帧）",
                !breathe_k_bad && !frame_bad,
                format!(
                    "呼吸 k 异常={} · 仍出帧={}（DG_NO_TRANSITION 下呼吸必须静止）",
                    breathe_k_bad, frame_bad
                ),
            ));
        } else {
            rows.push(("PR1.1·减动降级呼吸（k 恒 1 且不出帧）", false, "App::new 失败".into()));
        }
        if let Some(mut b) = fresh_normal {
            prep(&mut b);
            let mut first_bad = 0usize;
            for an in Anim::all() {
                b.cfg.anim = an;
                b.trans.settle();
                b.phase = 0.0;
                // 启动首帧：静态/淡入/跑马灯必须满覆盖率（旧 Fade 首拍
                // phase=0 ⇒ alpha=0 重爬一遍，就是这条断言要守的）；
                // 呼吸的「首帧」= 呼吸曲线起点，本身有定义。
                let want = if an == Anim::Breathe {
                    transition::breathe_k(0.0)
                } else {
                    1.0
                };
                if (b.eff_quote_k() - want).abs() > 1e-6 {
                    first_bad += 1;
                }
            }
            // 对照：非减动的呼吸必须每帧都出（否则 needs_frame 的臂被改坏了）
            b.cfg.anim = Anim::Breathe;
            b.phase = 0.7;
            let breathe_frames = needs_frame(&b);
            rows.push((
                "PR1.1·启动首帧全亮度（旧 Fade 重爬回归）",
                first_bad == 0 && breathe_frames,
                format!(
                    "{} 种动效首帧异常 · 非减动呼吸出帧={}（须 true）",
                    first_bad, breathe_frames
                ),
            ));
        } else {
            rows.push(("PR1.1·启动首帧全亮度（旧 Fade 重爬回归）", false, "App::new 失败".into()));
        }

        // ④ 单条格言库不闪自己：n<2 时不启动过渡。
        // 对照组（两句库必须启过渡）防「过渡整个没跑」的假绿。
        let one = a.list[0].clone();
        let saved = std::mem::replace(&mut a.list, vec![one]);
        a.qi = 0;
        a.qn = 0;
        a.cur_w = measure_quote(&a, &a.list[0]);
        a.trans.settle();
        a.phase = 0.0;
        // **循环内**记录是否启动过 —— 只查循环结束时的 is_active 是查不到的：
        // 就算守卫被拆、10s 时启动了过渡，0.56s 后它自己就落定了，
        // 15s 时 is_active 必然回到 false（注毒实测抓出来的假绿）。
        let mut single_activated = false;
        for _ in 0..300 {
            step(&mut a, 0.05); // 15s，dwell 上限 10s 的两倍
            if a.trans.is_active() {
                single_activated = true;
            }
        }
        let single_quiet = !single_activated && (a.eff_quote_k() - 1.0).abs() < 1e-6;
        // 对照组：两句库，同样 15s，必须至少启过一次过渡
        a.list = vec![saved[0].clone(), saved[1].clone()];
        a.qi = 0;
        a.qn = 1 % a.list.len();
        a.cur_w = measure_quote(&a, &a.list[0]);
        a.trans.settle();
        a.phase = 0.0;
        let mut pair_active = false;
        for _ in 0..300 {
            step(&mut a, 0.05);
            if a.trans.is_active() {
                pair_active = true;
            }
        }
        rows.push((
            "PR1.1·单条库不闪自己（n<2 不启过渡）",
            single_quiet && pair_active,
            format!(
                "单句库 15s 内过渡启动={} · 两句库对照组启动={}（须 false/true）",
                !single_quiet, pair_active
            ),
        ));
        a.list = saved;
        a.qi = 0;
        a.qn = 1 % a.list.len();
        a.cur_w = measure_quote(&a, &a.list[0]);
        a.trans.settle();
        a.phase = 0.0;

        // ---- PR3 · 竖版列渐显（compose 分带路线）+ 出处随格言
        // 环境已在上面切好（竖版 300×660、静态、时钟/日期/格言全开、无阴影）。

        // 错峰与出处两组断言都吃同一句话的环境，**统一钉死**：
        // 选「带出处且最长」的一条 —— 竖版一列约装 20 字，内置最长 36 字
        // ⇒ 至少 2 列（ncols=1 时错峰退化为整块渐显，断言对象直接消失）；
        // 带出处才能验「出处随格言」。不钉的话谁在前面改 qi，这里就跟着抖。
        if let Some(i) = (0..a.list.len())
            .filter(|&i| !a.list[i].author.is_empty())
            .max_by_key(|&i| a.list[i].display().chars().count())
        {
            a.qi = i;
            a.qn = (i + 1) % a.list.len();
            a.cur_w = measure_quote(&a, &a.list[a.qi]);
        }

        // ⑤ 列范围与阅读序：范围连续覆盖全部字形、列序自右向左、
        //    瓦片递降相接（不错列、不重叠、不掉缝）。
        let cb = layout_clock(&mut a);
        let band = frame_band(&a.cfg, &cb);
        let ql = frame_layout(&a, &cb);
        let ncol = ql.col_ranges.len();
        let mut col_ok = ncol >= 2;
        let mut covered = 0usize;
        for &(s, e) in &ql.col_ranges {
            if s == covered && e > s {
                covered = e;
            } else {
                col_ok = false;
            }
        }
        if covered != ql.glyphs.len() {
            col_ok = false;
        }
        for i in 0..ncol.saturating_sub(1) {
            let xmin_i = ql.glyphs[ql.col_ranges[i].0..ql.col_ranges[i].1]
                .iter()
                .map(|g| g.x)
                .min()
                .unwrap_or(0);
            let xmax_j = ql.glyphs[ql.col_ranges[i + 1].0..ql.col_ranges[i + 1].1]
                .iter()
                .map(|g| g.x)
                .max()
                .unwrap_or(0);
            if xmin_i <= xmax_j {
                col_ok = false; // 右列必须整体在左列之右
            }
        }
        for i in 0..ncol {
            let (l, r) = ql.col_x[i];
            if l >= r || l < 0 || r > a.cfg.w {
                col_ok = false;
            }
            if i > 0 && ql.col_x[i - 1].0 <= l {
                col_ok = false; // 左邻列的左缘必须更靠左
            }
            if i > 0 && ql.col_x[i].1 != ql.col_x[i - 1].0 {
                col_ok = false; // 中列瓦片必须无缝相接
            }
        }
        if ncol >= 1 && ql.col_x[0].1 != band.2 {
            col_ok = false; // 首列右缘贴版心右缘
        }
        rows.push((
            "PR3·竖版列范围与阅读序（右→左、无缝瓦片）",
            col_ok,
            format!(
                "{} 列 / {} 字形全覆盖 · 瓦片 {}..{} · 版心右缘 {}",
                ncol,
                ql.glyphs.len(),
                ql.col_x.first().map(|t| t.0).unwrap_or(-1),
                ql.col_x.last().map(|t| t.1).unwrap_or(-1),
                band.2
            ),
        ));

        // 选一条有出处的格言（出处列随格言过渡要有对象可验）
        a.trans.settle();
        a.phase = 0.0;
        let op = a.text_alpha();
        build_frame(&mut a, op);
        let v1: Vec<u32> = a.frame.out_pixels().to_vec();
        let m0 = a.frame.channel_mask(0);
        let m1 = a.frame.channel_mask(1);
        let m2 = a.frame.channel_mask(2);
        // 落款取样坐标必须用 **hug 之后**的落款条 —— build_frame 里落款
        // 会右移贴主文，拿 layout_sign 的原位取样就是 0 px（真管线和
        // 自检走同一个 hug_sign，不许各推一遍）。
        let cb0 = layout_clock(&mut a);
        let ql0 = frame_layout(&a, &cb0);
        let cb = hug_sign(&a.cfg, cb0, &ql0);
        let ql = frame_layout(&a, &cb);
        let ncol = ql.col_ranges.len();

        // 驱到 Arrive 35%：右列已过半、左列刚起步，错峰最可辨的时刻。
        // **一拍到位**：`step(LEAVE+GAP+ε)` 已经把 Leave/Gap 穿完、落在
        // Arrive 起点，再补一拍 GAP 就会多走 0.14s（t=0.231 时两列 k 都
        // 归 1，错峰不可辨 —— 这里曾经踩过）。
        a.trans.start_leave(0);
        a.trans.step(transition::LEAVE_S + transition::GAP_S + 1e-4);
        a.trans.step(transition::ARRIVE_S * 0.35);
        let t_arrive = a.trans.phase_t();
        build_frame(&mut a, op);
        let v2: Vec<u32> = a.frame.out_pixels().to_vec();

        // ⑥ 逐列错峰：按列带统计「纯格言像素」的 alpha 比值，必须
        //    右列 > 左列，且每列 ≈ 该列的 column_k。
        let fw = a.cfg.w;
        let fh = a.cfg.h;
        let mut band_sum = vec![0f64; ncol];
        let mut band_cnt = vec![0usize; ncol];
        let mut clock_bad = 0usize;
        let mut clock_n = 0usize;
        for y in 0..fh {
            for x in 0..fw {
                let i = (y * fw + x) as usize;
                let (c0, c1, c2) = (m0[i], m1[i], m2[i]);
                let live = (c0 > 0) as u8 + (c1 > 0) as u8 + (c2 > 0) as u8;
                if live > 1 {
                    continue; // 重叠像素归属由最强通道定，不参与比例判据
                }
                if c0 > 0 {
                    // 时钟（含竖版时间列 / 分隔线）：必须逐位不动
                    clock_n += 1;
                    if v1[i] != v2[i] {
                        clock_bad += 1;
                    }
                    continue;
                }
                if c2 == 0 || c1 > 0 {
                    continue;
                }
                let a1 = (v1[i] >> 24) & 0xFF;
                let a2 = (v2[i] >> 24) & 0xFF;
                if a1 < 48 {
                    continue; // 低覆盖像素的比例噪声大
                }
                for (ci, &(l, r)) in ql.col_x.iter().enumerate() {
                    let (l, r) = if ci == 0 {
                        (l, fw)
                    } else if ci == ncol - 1 {
                        (0, r)
                    } else {
                        (l, r)
                    };
                    if x >= l && x < r {
                        band_sum[ci] += a2 as f64 / a1 as f64;
                        band_cnt[ci] += 1;
                        break;
                    }
                }
            }
        }
        let mut stagger_ok = ncol >= 2 && clock_bad == 0 && clock_n > 100;
        let mut ratios = Vec::new();
        for ci in 0..ncol {
            if band_cnt[ci] < 50 {
                stagger_ok = false;
                ratios.push(f64::NAN);
                continue;
            }
            let r = band_sum[ci] / band_cnt[ci] as f64;
            ratios.push(r);
            let want = transition::column_k(ci, ncol, t_arrive) as f64;
            if (r - want).abs() > 0.08 {
                stagger_ok = false;
            }
        }
        for ci in 0..ncol.saturating_sub(1) {
            // 注：band_cnt<50 时 ratios 里是 NaN 且 stagger_ok 已置假，
            // 这里 NaN 比较为 false 仍落到同一结局，语义不变。
            if ratios[ci] <= ratios[ci + 1] + 0.05 {
                stagger_ok = false; // 右列必须明显先亮
            }
        }
        let ratio_s: Vec<String> = ratios
            .iter()
            .map(|r| format!("{:.3}", r))
            .collect();
        rows.push((
            "PR3·竖版入场按列错峰（右列先亮，逐带读 alpha）",
            stagger_ok,
            format!(
                "{} 列比值 [{}] · 时钟 {} px 逐位相同（异常 {}）",
                ncol,
                ratio_s.join(", "),
                clock_n,
                clock_bad
            ),
        ));

        // ⑦ 出处随格言：出处列 alpha ≈ 最左列 k；日期列逐位不动。
        let author_col = cb.sig.iter().find(|c| c.is_author);
        let date_col = cb.sig.iter().find(|c| !c.main_color && !c.is_author);
        let (mut au_ok, mut au_n, mut dt_ok, mut dt_n) = (true, 0usize, true, 0usize);
        let want_author = transition::column_k(ncol - 1, ncol, t_arrive) as f64;
        if let (Some(ac), Some(dc)) = (author_col, date_col) {
            for y in 0..fh {
                for x in 0..fw {
                    let i = (y * fw + x) as usize;
                    if m1[i] < 128 || m0[i] > 0 || m2[i] > 0 {
                        continue; // 只取纯日期通道像素
                    }
                    let a1 = (v1[i] >> 24) & 0xFF;
                    let a2 = (v2[i] >> 24) & 0xFF;
                    if a1 < 48 {
                        continue;
                    }
                    if x >= ac.x && x < ac.x + cb.sig_w.max(1) {
                        au_n += 1;
                        let r = a2 as f64 / a1 as f64;
                        if (r - want_author).abs() > 0.08 {
                            au_ok = false;
                        }
                    } else if x >= dc.x && x < dc.x + cb.sig_w.max(1) {
                        dt_n += 1;
                        if (a2 as i32 - a1 as i32).abs() > 1 {
                            dt_ok = false;
                        }
                    }
                }
            }
        } else {
            au_ok = false; // 没有出处列可验 = 环境不对，不许假绿
        }
        rows.push((
            "PR3·出处随格言、日期列不动",
            au_ok && dt_ok && au_n > 20 && dt_n > 20,
            format!(
                "出处 {} px 比值应≈{:.3} · 日期 {} px 逐位相同 · 出处动={} 日期动={}",
                au_n, want_author, dt_n, !au_ok, !dt_ok
            ),
        ));

        // ⑧ 竖版最右列完整可见（孤儿项：右缘墨迹被裁剪区切掉的回归守卫）。
        //    判据两条：版心右缘 4px 内有格言墨（谁删了右列谁红）；
        //    **瓦片右缘外也有墨** —— 列瓦片是半开区间 `[l, r)`（layout.rs），
        //    首列右缘贴版心右缘 br，字形墨迹天然越过它 1px（默认字体实测
        //    最右墨迹 x=br；`--dump` 实测 300×660 右缘曾被切 9px 的那次，
        //    切的正是 x ≥ br 这几列）。任何静态裁剪区切在 br 都会先抹掉
        //    这圈外溢 ⇒ 外溢窗口从 br 起算（含），注毒实测内测墨查不出裁剪。
        let br = band.2;
        let mut edge_ink = 0usize;
        let mut spill_ink = 0usize;
        let mut max_ink_x: i32 = -1;
        for y in 0..fh {
            for x in 0..fw {
                if m2[(y * fw + x) as usize] > 0 && x > max_ink_x {
                    max_ink_x = x;
                }
            }
            for x in (br - 4).max(0)..(br + 2).min(fw) {
                if m2[(y * fw + x) as usize] > 0 {
                    edge_ink += 1;
                }
            }
            for x in br..((br + 3).min(fw)) {
                if m2[(y * fw + x) as usize] > 0 {
                    spill_ink += 1;
                }
            }
        }
        rows.push((
            "PR3·竖版最右列完整可见（无静态裁剪区）",
            edge_ink > 0 && spill_ink > 0,
            format!(
                "版心右缘 {} 附近 {} px + 外溢 {} px 格言墨 · 最右墨迹 x={} · 瓦片右缘 {:?}（外溢为 0 ⇒ 有裁剪区在切）",
                br, edge_ink, spill_ink, max_ink_x, ql.col_x.first().copied(),
            ),
        ));

        // 还原横版跑马灯环境（后续「字号可配置」等断言要它）
        a.cfg.orient = Orient::Horizontal;
        a.cfg.w = w_was;
        a.cfg.h = h_was;
        a.cfg.anim = Anim::Marquee;
        a.cfg.shadow = true;
        a.trans.settle();
        a.phase = 0.0;
        a.m = 260.0;
        a.rebuild_fonts();
        let o = a.text_alpha();
        build_frame(&mut a, o);
    }

    // ---- PR2 · 悬停暂停（计划第五章 + 12.3）：迟滞判定、冻结、恢复、不出帧
    {
        // ① 纯函数全边界。cargo test 里有同款（transition::tests）；
        //    「会红的断言才算断言」，selftest 这个入口也得能独立红。
        let mut hover_ok = true;
        // 进侧迟滞：50 段 299ms 的「路过」（中间离开清零）永不暂停
        let mut st = transition::HoverState::default();
        for _ in 0..50 {
            let (p, ns) = transition::hover_decide(st, true, 299);
            if p {
                hover_ok = false;
            }
            let (p2, ns2) = transition::hover_decide(ns, false, 10);
            if p2 {
                hover_ok = false;
            }
            st = ns2;
        }
        // 恰好 HOVER_HOLD_MS：确认暂停（≥ 语义）
        let (p3, _) = transition::hover_decide(st, true, transition::HOVER_HOLD_MS);
        if !p3 {
            hover_ok = false;
        }
        // 出侧不对称：离开**立即**恢复，且累计器清零（不许带旧账）
        let (_, held) = transition::hover_decide(transition::HoverState::default(), true, 10_000);
        let (p4, st4) = transition::hover_decide(held, false, 1);
        if p4 || st4 != transition::HoverState::default() {
            hover_ok = false;
        }
        rows.push((
            "PR2·悬停迟滞（≥300ms 才停、离开立恢复、路过不触发）",
            hover_ok,
            "hover_decide 全边界（不对称迟滞；299ms×50 段路过 + 恰 300ms + 离开即清）".into(),
        ));

        // 下面三条走**真路径** step()/needs_frame()。先把时钟显示关掉：
        // clock_stamp 恒 0，排除「自检跑到一半墙钟跨秒」造成的假红
        //（断言的时机是断言的一部分）。
        let (clk_was, date_was) = (a.cfg.show_clock, a.cfg.show_date);
        a.cfg.show_clock = false;
        a.cfg.show_date = false;
        a.clock_stamp = clock_stamp(&a.cfg);

        // ② 悬停冻结 dwell：Static 置 hover_paused，推 20s 动画时间 ——
        //    不换句、dwell 不走、过渡不启动、needs_frame 恒假（一帧不出）。
        a.cfg.anim = Anim::Static;
        a.trans.settle();
        a.phase = 0.0;
        a.hover_paused = true;
        let (qi0, ph0) = (a.qi, a.phase);
        render_if_due(&mut a, true); // 清掉环境切换残留的脏标记，别算进「不出帧」
        let r0 = a.renders;
        let mut nf_true = 0usize;
        for _ in 0..200 {
            step(&mut a, 0.1);
            if needs_frame(&a) {
                nf_true += 1;
            }
        }
        rows.push((
            "PR2·悬停冻结 dwell（不换句、dwell 不走、不出帧）",
            a.qi == qi0
                && a.phase == ph0
                && a.renders == r0
                && nf_true == 0
                && !a.trans.is_active(),
            format!(
                "qi/phase 漂移={} · renders 增量={} · needs_frame 真 {} 拍",
                a.qi != qi0 || a.phase != ph0,
                a.renders.wrapping_sub(r0),
                nf_true
            ),
        ));

        // ③ 出侧不对称在真路径上：解冻后 dwell 续走、到点过渡正常启动
        a.hover_paused = false;
        let mut started = false;
        for _ in 0..400 {
            step(&mut a, 0.05);
            if a.trans.is_active() {
                started = true;
                break;
            }
        }
        rows.push((
            "PR2·离开立即恢复（dwell 续走、过渡启动）",
            started,
            "解冻后 ≤20s 内过渡应启动（dwell 最长 10s）".into(),
        ));
        a.trans.settle();
        a.phase = 0.0;

        // ④ 跑马灯悬停停：m 冻结 + needs_frame 假；解冻一拍 m 即推进。
        //    跑马灯是悬停暂停的主场景（dwell 冻结无视觉表现，滚动冻结有）。
        a.cfg.anim = Anim::Marquee;
        a.primed = true;
        a.hover_paused = true;
        render_if_due(&mut a, true);
        let m0 = a.m;
        let r1 = a.renders;
        let nf_paused = needs_frame(&a);
        for _ in 0..50 {
            step(&mut a, 0.1);
        }
        let m_frozen = a.m;
        let r_frozen = a.renders;
        a.hover_paused = false;
        let nf_resumed = needs_frame(&a);
        let m_before = a.m;
        step(&mut a, 0.1);
        rows.push((
            "PR2·跑马灯悬停停（m 冻结 + 不出帧，解冻即滚）",
            m_frozen == m0
                && r_frozen == r1
                && !nf_paused
                && nf_resumed
                && a.m > m_before,
            format!(
                "暂停 5s m 增量={} · renders 增量={} · 解冻一拍 m 增量={}",
                m_frozen - m0,
                r_frozen.wrapping_sub(r1),
                a.m - m_before
            ),
        ));

        // 还原环境（后续断言 / 默认时钟显示要它）
        a.cfg.show_clock = clk_was;
        a.cfg.show_date = date_was;
        a.hover_paused = false;
        a.hover = transition::HoverState::default();
    }

    rows.push((
        "格言库装载",
        a.list.len() >= 40,
        format!("{} 条", a.list.len()),
    ));
    rows.push((
        "帧缓冲尺寸",
        fw == 620 && fh == 176,
        format!("{}x{} = {} px", fw, fh, total),
    ));
    rows.push((
        "字形已绘制（高覆盖率像素）",
        solid > 2000,
        format!("{} px 接近实心，峰值 alpha {}", solid, alpha_max),
    ));
    rows.push((
        "逐像素 alpha（存在半透明边缘）",
        partial > 2000,
        format!(
            "{} px 处于 0<a<200（占非空像素 {:.1}%）",
            partial,
            partial as f64 * 100.0 / (partial + solid).max(1) as f64
        ),
    ));
    rows.push((
        "预乘 ARGB 合法（颜色分量 ≤ alpha）",
        overflow == 0,
        format!("{} px 越界", overflow),
    ));
    rows.push((
        "灰度抗锯齿（字形为白，无色边）",
        colored == 0,
        format!("{} px 带彩色分量", colored),
    ));
    rows.push((
        "整体不透明度生效",
        alpha_sum > 0 && (alpha_sum / total as u64) < op as u64,
        format!(
            "平均 alpha {:.1} / 设定 {}",
            alpha_sum as f64 / total as f64,
            op
        ),
    ));

    // ================================================================
    // 以下逐条验证新加的可配置项：**真的改配置 → 真的重渲染 → 量像素**，
    // 不看代码不猜结论。
    // ================================================================

    // 两个整数比是否成比例（容差 12%）：预乘后各分量按同一比例缩放，
    // 所以「配置色的色相」应当原样保留下来。
    fn ratio_close(x: u32, y: u32, xr: u32, yr: u32) -> bool {
        let lhs = x as i64 * yr as i64;
        let rhs = y as i64 * xr as i64;
        (lhs - rhs).abs() * 100 <= lhs.max(rhs).max(1) * 12
    }

    // 区域内按 alpha 加权的「墨量」
    fn ink_in(px: &[u32], w: i32, l: i32, t: i32, r: i32, b: i32) -> u64 {
        let mut s = 0u64;
        for y in t.max(0)..b {
            for x in l.max(0)..r {
                let i = (y * w + x) as usize;
                if i < px.len() {
                    s += ((px[i] >> 24) & 0xFF) as u64;
                }
            }
        }
        s
    }

    // ---- ① 主色可配置
    const WARM: u32 = 0xF5_D98B;
    a.cfg.color = WARM;
    a.cfg.date_color = WARM; // 先让两者一致，隔离出「主色」这一个变量
    let o = a.text_alpha();
    build_frame(&mut a, o);
    let (mut hue_ok, mut hue_bad) = (0usize, 0usize);
    {
        let px = a.frame.out_pixels();
        for p in px {
            let al = (p >> 24) & 0xFF;
            if al < 80 {
                continue; // 边缘像素的取整误差不参与判定
            }
            let (r, g, b) = ((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF);
            let ok = ratio_close(r, g, color_r(WARM), color_g(WARM))
                && ratio_close(g, b, color_g(WARM), color_b(WARM));
            if ok {
                hue_ok += 1;
            } else {
                hue_bad += 1;
            }
        }
    }
    rows.push((
        "文字颜色生效（像素色相 = 配置色）",
        hue_bad == 0 && hue_ok > 1000,
        format!(
            "#{:06X}：{} px 色相一致 / {} px 偏差",
            WARM, hue_ok, hue_bad
        ),
    ));

    // ---- ② 日期是独立的一条颜色通道
    a.cfg.color = 0xFF_FFFF;
    a.cfg.date_color = 0xFF_0000; // 纯红，跟主色彻底分开
    let o = a.text_alpha();
    build_frame(&mut a, o);
    let (mut red_px, mut gray_px) = (0usize, 0usize);
    {
        let px = a.frame.out_pixels();
        for p in px {
            let al = (p >> 24) & 0xFF;
            if al < 80 {
                continue;
            }
            let (r, g, b) = ((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF);
            if r == g && g == b {
                gray_px += 1;
            } else if r > g + 20 && r > b + 20 {
                red_px += 1;
            }
        }
    }
    rows.push((
        "日期独立配色（两条掩码通道）",
        red_px > 100 && gray_px > 500,
        format!("{} px 红（日期）/ {} px 白（时钟+格言）", red_px, gray_px),
    ));

    // ---- ②b 「通道即语义」：每类内容只落在**自己那一条**字节上。
    //
    // 上面那条只看「两种颜色都出现过」是不够的 —— 把 `CH_CLOCK` 与
    // `CH_DATE` 对调，红白像素数只是互换，它照样绿（实测过）。而颜色串台
    // 恰恰是这套机制唯一会静默出错的方式：画面依然好看，只是颜色对不上号。
    //
    // 判据用**开关反证**：是谁的通道，关掉谁就会空。「关日期」却让另一条
    // 通道空了 —— 那就是通道接错了。
    //
    // 写成嵌套 `fn` 而不是闭包：闭包会借住 `a`，后面 `build_frame(&mut a, ..)`
    // 就借不动了。
    fn chan_ink(a: &App, k: u32) -> usize {
        a.frame.channel_mask(k).iter().filter(|&&v| v > 0).count()
    }
    a.cfg.show_clock = true;
    a.cfg.show_date = true;
    a.cfg.show_quote = true;
    let o = a.text_alpha();
    build_frame(&mut a, o);
    let (c0, c1, c2) = (chan_ink(&a, 0), chan_ink(&a, 1), chan_ink(&a, 2));
    a.cfg.show_date = false;
    let o = a.text_alpha();
    build_frame(&mut a, o);
    let (d1, d2) = (chan_ink(&a, 1), chan_ink(&a, 2));
    a.cfg.show_quote = false;
    let o = a.text_alpha();
    build_frame(&mut a, o);
    let (q1, q2) = (chan_ink(&a, 1), chan_ink(&a, 2));
    a.cfg.show_clock = false;
    let o = a.text_alpha();
    build_frame(&mut a, o);
    let e0 = chan_ink(&a, 0);
    a.cfg.show_clock = true;
    a.cfg.show_date = true;
    a.cfg.show_quote = true;
    let ok = c0 > 0 && c1 > 0 && c2 > 0 && d1 == 0 && d2 > 0 && q1 == 0 && q2 == 0 && e0 == 0;
    rows.push((
        "掩码通道各归其位（关掉谁只清空谁）",
        ok,
        format!(
            "默认 byte0 {} / byte1 {} / byte2 {} · 关日期 → byte1 {} · 关格言 → byte2 {} · 关时钟 → byte0 {}",
            c0, c1, c2, d1, q2, e0
        ),
    ));

    // ---- ③ 时间 / 日期可开关
    a.cfg.color = config::COLOR_PRESETS[0];
    a.cfg.date_color = config::DATE_COLOR_PRESETS[0];
    let o = a.text_alpha();
    build_frame(&mut a, o);
    let on_ink = {
        let (l, t, r, b) = (
            a.clock_rect.left,
            a.clock_rect.top,
            a.clock_rect.right,
            a.clock_rect.bottom,
        );
        ink_in(a.frame.out_pixels(), fw, l, t, r, b)
    };
    a.cfg.show_clock = false;
    a.cfg.show_date = false;
    let o = a.text_alpha();
    build_frame(&mut a, o);
    let off_ink = {
        let (l, t, r, b) = (
            a.clock_rect.left,
            a.clock_rect.top,
            a.clock_rect.right,
            a.clock_rect.bottom,
        );
        ink_in(a.frame.out_pixels(), fw, l, t, r, b)
    };
    a.cfg.show_clock = true;
    a.cfg.show_date = true;
    rows.push((
        "时间 / 日期可关闭",
        on_ink > 0 && off_ink * 5 < on_ink,
        format!("关掉后该区域墨量 {} → {}（↓{:.1}%）", on_ink, off_ink,
            (1.0 - off_ink as f64 / on_ink.max(1) as f64) * 100.0),
    ));

    // ---- ④ 位置可拖动（并且拖不丢）
    a.cfg.clock_dx = 0;
    a.cfg.clock_dy = 0;
    let o = a.text_alpha();
    build_frame(&mut a, o);
    let base_left = a.clock_rect.left;
    a.cfg.clock_dx = 200;
    let o = a.text_alpha();
    build_frame(&mut a, o);
    let moved_left = a.clock_rect.left;
    a.cfg.clock_dx = 99999; // 往死里拖
    normalize_clock_offset(&mut a);
    let o = a.text_alpha();
    build_frame(&mut a, o);
    let clamped_r = a.clock_rect.right;
    let clamped_in = clamped_r <= a.cfg.w && a.clock_rect.left >= 0;
    a.cfg.clock_dx = 0;
    let o = a.text_alpha();
    build_frame(&mut a, o);
    rows.push((
        "时间日期位置可拖动",
        moved_left > base_left + 100,
        format!(
            "dx=0 → x{} ; dx=200 → x{}（位移 {}px）",
            base_left,
            moved_left,
            moved_left - base_left
        ),
    ));
    rows.push((
        "拖出边界自动夹回窗口内",
        clamped_in,
        format!("dx=99999 → 右缘 x{} ≤ 窗宽 {}", clamped_r, a.cfg.w),
    ));

    // ---- ⑤ 字号可配置
    // 墨量对**时分秒**敏感（默认显示秒，每一跑时钟字形都不同）—— 曾观测到
    // 「同一份代码，这一跑 PASS 下一跑 FAIL」的存量抖动：阈值余量只有 1%，
    // 跨一个秒边界就翻脸。修法：秒位钉掉；两个单旋钮**各自建基线**
    // （量格言时藏时钟、量时钟时藏格言），把另一个的干扰和时钟字形
    // 一起排除在判据之外。
    a.cfg.seconds = false;
    // 环境也钉死（跑马灯的墨量取决于**哪句在滚、滚到哪**）：前面的断言块
    // 会换句、会动 qi —— 不钉的话这里的基线跟着前面漂（实测曾被换成
    // 最长句后 ↑50% 的阈值量成 ↑33%）。
    a.qi = 0;
    a.qn = 1 % a.list.len();
    a.cur_w = measure_quote(&a, &a.list[a.qi]);
    a.m = 260.0;
    let ink_of = |a: &mut App,
                  scale: i32,
                  clock: i32,
                  quote: i32,
                  show_c: bool,
                  show_q: bool|
     -> u64 {
        a.cfg.scale = scale;
        a.cfg.clock_scale = clock;
        a.cfg.quote_scale = quote;
        a.cfg.show_clock = show_c;
        a.cfg.show_quote = show_q;
        a.rebuild_fonts();
        let o = a.text_alpha();
        build_frame(a, o);
        a.frame
            .out_pixels()
            .iter()
            .map(|p| ((p >> 24) & 0xFF) as u64)
            .sum()
    };
    // 整体：三样全开。阈值**不能**按面积律定 —— 这一帧的墨量由**跑马灯格言**
    // 主导，而跑马灯是次线性律（实测 200% 只到 ×1.51，不是面积律的 ×4）：
    // 放大后裁剪窗里可见字数变少，把单字面积的涨幅吃掉大半。
    // 旧注释写「2.25 倍面积对 1.5 倍阈值有余量」，是默认格言也走面积律
    // ⇒ **新装机器上稳定误报**：干净数据目录实测 ×1.492 / ×1.4978 / ×1.5011
    // （比值随「当前分钟」的时钟字形与「库里第 0 句」漂），1.5 的阈值正好落在
    // 抖动带正中间，同一份代码同一目录能一次 FAIL 一次 PASS。
    // 4/3 仍在实测下界（1.49）之下留 12% 余量，而「字号根本没生效」是 ×1.0
    // —— 与本条判据的本意（整体放大有效）拉开足够距离。
    let ink_100 = ink_of(&mut a, 100, 100, 100, true, true);
    let ink_150 = ink_of(&mut a, 150, 100, 100, true, true);
    // 单旋钮：各自单独量、各自跟自己的 100% 比
    let ink_clock100 = ink_of(&mut a, 100, 100, 100, true, false);
    let ink_clock200 = ink_of(&mut a, 100, 200, 100, true, false);
    let ink_quote100 = ink_of(&mut a, 100, 100, 100, false, true);
    let ink_quote200 = ink_of(&mut a, 100, 100, 200, false, true);
    rows.push((
        "整体字号可配置",
        ink_150 > ink_100 * 4 / 3,
        format!(
            "100% → 墨量 {}，150% → {}（↑{:.2}0%；阈值 4/3，跑马灯次线性律主导）",
            ink_100,
            ink_150,
            (ink_150 as f64 / ink_100.max(1) as f64 - 1.0) * 100.0
        ),
    ));
    // 时钟字号和格言字号必须是**两个互不干扰**的旋钮：
    // 各自都能单独把画面显著撑大，且两次测量结果不同（不是同一个数）。
    // 两个旋钮的增长律不同，阈值必须分开定（曾误设同一 ×2 导致格言 FAIL）：
    // - 时钟整串可见 ⇒ 墨量按**面积律**涨（200% 实测 3.8×）；
    // - 格言是跑马灯：固定宽度裁剪窗内可见字数 ∝ 1/字号、单字面积 ∝ 字号²
    //   ⇒ 墨量按**次线性律**涨（×1.51 ~ ×2.27，取决于「库里第 0 句」多长：
    //   句子短到能整句进窗 ⇒ 接近面积律；长到必须滚动 ⇒ 压到 1.5 附近）。
    //   拿面积律的阈值要求线性律的对象，等于让一条物理上不成立的判据永远红。
    // 阈值取 5/4：实测下界 1.51 之上留 20% 余量，仍远高于「旋钮没接上」的 ×1.0。
    // （2026-10-15 订正：原为 3/2，干净数据目录实测 ×1.51 —— 余量只剩 0.7%，
    //   换台机器换套格言库就会翻红。）
    rows.push((
        "时钟 / 格言字号可分别单独调",
        ink_clock200 > ink_clock100 * 2
            && ink_quote200 > ink_quote100 * 5 / 4
            && ink_clock200 != ink_quote200,
        format!(
            "整体仍 100%：时钟 200% → {}（100% 时 {}，×{:.2} 面积律），格言 200% → {}（100% 时 {}，×{:.2} 跑马灯次线性律）（两个旋钮各管各的）",
            ink_clock200, ink_clock100,
            ink_clock200 as f64 / ink_clock100.max(1) as f64,
            ink_quote200, ink_quote100,
            ink_quote200 as f64 / ink_quote100.max(1) as f64
        ),
    ));
    a.cfg.seconds = true;

    // ---- ⑥ 字体可配置（换一个完全不同的字面，管线不能崩）
    a.cfg.scale = 100;
    a.cfg.clock_scale = 100;
    a.cfg.quote_scale = 100;
    let mut font_rows: Vec<(String, u64)> = Vec::new();
    for f in config::FONT_PRESETS {
        a.cfg.font = f.to_string();
        a.rebuild_fonts();
        let o = a.text_alpha();
    build_frame(&mut a, o);
        let ink: u64 = a.frame.out_pixels().iter().map(|p| ((p >> 24) & 0xFF) as u64).sum();
        font_rows.push((f.to_string(), ink));
    }
    let fonts_ok = font_rows.iter().all(|(_, ink)| *ink > 1000);
    let any_differ = font_rows
        .iter()
        .map(|(_, ink)| *ink)
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        > 1;
    rows.push((
        "字体可配置（逐个字面重建渲染）",
        fonts_ok && any_differ,
        format!(
            "{} 种字面墨量 {} .. {}（不同字面度量确实不同）",
            font_rows.len(),
            font_rows.iter().map(|(_, i)| *i).min().unwrap_or(0),
            font_rows.iter().map(|(_, i)| *i).max().unwrap_or(0)
        ),
    ));

    // ---- ⑥b 设置面板的字体列表：枚举 + 中文过滤
    //
    // 这条断言的靶子是「列表会不会悄悄变成空的 / 混进非中文字体」——
    // 枚举 API 一旦用错（回调签名、charset、去掉 retain），列表要么静默为空
    // （界面照常打开、只是一行都没有，肉眼很难归因），要么把一堆只有拉丁字形的
    // 字体也列进去（选上去满屏豆腐块）。所以两头都查：
    //   * 原始枚举非空、过滤后非空，且 过滤后 ≤ 原始（过滤没把集合搞大）
    //   * **列出来的每一个字体都能通过 `font_has_cjk` 反查** ——
    //     这一条正是「把 `retain` 删掉就会红」的那条（西文字体混进来立刻失败）
    //   * 没有 `@` 开头的竖排别名混进列表
    let all_families = config_ui::enumerate_font_families();
    let cjk_fonts = config_ui::enumerate_cjk_fonts();
    let list_sane = !cjk_fonts.is_empty()
        && cjk_fonts.len() <= all_families.len()
        && cjk_fonts.iter().all(|f| config_ui::font_has_cjk(f))
        && cjk_fonts.iter().all(|f| !f.starts_with('@'));
    rows.push((
        "设置面板·字体列表（只列含汉字的字体）",
        list_sane,
        format!(
            "枚举 {} 个家族 → 过滤后 {} 个中文字体 · 全部含汉字={} · 无 @ 竖排条目={}",
            all_families.len(),
            cjk_fonts.len(),
            cjk_fonts.iter().all(|f| config_ui::font_has_cjk(f)),
            cjk_fonts.iter().all(|f| !f.starts_with('@')),
        ),
    ));

    // ---- ⑥c 设置面板的控件清单：表格就是规格
    //
    // 这条断言是「面板上到底有哪些可调项」的**唯一**书面记录。它红了只有两种含义：
    // 要么是漏了 / 重了控件（copy-paste 出错的典型症状：两个控件发同一条命令，
    // 看起来都能点，其实有一个在改另一个的字段），要么是**有意**增删了配置项，
    // 那就顺手把这份清单改了 —— 一致性不是靠自觉，是靠这条断言钉住的。
    //
    // 顺带把「加了一行之后最下面一行跑出窗口」也钉死：面板没有滚动条，
    // 排不下就是看不见。判据直接问布局函数要余量，不是拿放大镜看截图。
    {
        let expected: [u32; 40] = [
            MI_CLOCK,
            MI_DATE,
            MI_SECONDS,
            MI_QUOTE,
            MI_SHADOW,
            MI_TOPMOST,
            MI_AUTOHIDE,
            MI_AUTOSTART,
            MI_LAYOUT,
            MI_SHRINK_FIT,
            MI_GLASS,
            MI_TEXT,
            MI_ORIENT_BASE,
            MI_ORIENT_BASE + 1,
            MI_ANIM_BASE,
            MI_ANIM_BASE + 1,
            MI_ANIM_BASE + 2,
            MI_ANIM_BASE + 3,
            MI_SPEED_DOWN,
            MI_SPEED_UP,
            MI_SCALE_DOWN,
            MI_SCALE_UP,
            MI_OP_DOWN,
            MI_OP_UP,
            MI_CLOCK_DOWN,
            MI_CLOCK_UP,
            MI_QUOTE_DOWN,
            MI_QUOTE_UP,
            MI_COLOR,
            MI_DATE_COLOR,
            MI_QUOTE_COLOR,
            MI_SPACING_DOWN,
            MI_SPACING_UP,
            MI_PREV,
            MI_NEXT,
            MI_FAV,
            MI_CLOCK_RESET,
            MI_RELOAD,
            MI_RESET,
            MI_TOGGLE,
        ];
        let got = config_ui::table_cmds();
        let mut uniq = got.clone();
        uniq.sort_unstable();
        uniq.dedup();
        let same = got == expected.to_vec();
        let margin = config_ui::plan_margin_at_min();
        let overlap = config_ui::plan_overlaps();
        let ok = same && uniq.len() == got.len() && margin.is_some() && overlap.is_none();
        rows.push((
            "设置面板·控件清单（表格即规格）",
            ok,
            format!(
                "{} 条命令 · 无重复={} · 与期望清单一致={}{} · 最小窗口下底部余量 {}px · 无矩形交叠={}{}",
                got.len(),
                uniq.len() == got.len(),
                same,
                if same {
                    String::new()
                } else {
                    format!(
                        " · 期望 {} 条，第一处不同在第 {} 个",
                        expected.len(),
                        got.iter()
                            .zip(expected.iter())
                            .position(|(x, y)| x != y)
                            .map(|i| i + 1)
                            .unwrap_or(got.len().min(expected.len()) + 1)
                    )
                },
                match margin {
                    Some(m) => format!("{}", m),
                    None => "排不下（最后一行会被切掉）".to_string(),
                },
                overlap.is_none(),
                match &overlap {
                    Some(m) => format!("（{}）", m),
                    None => String::new(),
                }
            ),
        ));
    }

    // ---- ⑦ 阴影不许「跑出自己的地盘」
    //
    // 阴影层只抬 alpha、不碰颜色分量，所以它一旦出现在没有字形的地方，
    // 合成出来就是一片半透明黑。这不是理论风险：格言阴影的裁剪区曾在
    // 阴影趟里退回内边距，结果在时钟上盖了一层黑影幽灵。
    //
    // 合法情况只有字形外扩的那一圈 2px 偏移晕边；因此拿「孤立阴影像素」
    // 和「字形像素」的比值当判据，跑偏时这个比值会暴涨。
    //
    // 刻意把跑马灯停在「引文正跨过格言带左缘」的位置：这是每一轮滚动
    // 都会经过的状态，也正是阴影裁剪区一旦算错就会大范围露馅的时刻。
    a.cfg.shadow = true;
    a.cfg.anim = Anim::Marquee;
    a.m = 520.0;
    a.primed = true;
    // 文本环境也**钉死**：前面的断言块会改 qi（比如挑带出处的句子量出处列），
    // 而孤影比值取决于「m=520 时哪个字正跨在裁剪左缘」——不同字形的
    // hinting 在边缘的覆盖率不同，不钉的话谁在前面换句，这条断言就跟着抖。
    a.qi = 0;
    a.qn = 1 % a.list.len();
    a.cur_w = measure_quote(&a, &a.list[a.qi]);
    let o = a.text_alpha();
    build_frame(&mut a, o);
    // 阴影趟是「同一批字形整体位移 (off,off) 再画一遍」，所以这条不变量是
    // **精确**的（不是启发式）：阴影掩码在 (x,y) 有墨，当且仅当
    // 掩码在 (x-off, y-off) 有字形。唯一例外是**裁剪左/上缘**：源点
    // (x-off, y-off) 落在裁剪矩之外的字形没被画（掩码趟同裁剪），可它的
    // 阴影在 (x,y) 仍会被画 —— 这是裁剪的定义，不是 bug（当年裁剪区算错
    // 的教训是阴影**大面积**越界，不是这圈 2px 的合法边）。
    // 因此判据排除源点在版心之外的情况，剩下的不变量精确为 0 ——
    // 不再依赖「哪个字恰好在边缘」的 hinting 抖动（那曾让比值在
    // 0.3% ~ 1.35% 之间随文本漂移）。
    let off = shadow_offset(&a.cfg);
    let cb7 = layout_clock(&mut a);
    let (bl, bt, _br, _bb) = quote_band(&a.cfg, &cb7);
    let (fw2, fh2) = (a.frame.w, a.frame.h);
    let (glyph_px, orphan_px) = {
        let m = a.frame.ink_mask();
        let sh = a.frame.shadow_mask();
        let mut glyph = 0usize;
        let mut orphan = 0usize;
        for y in 0..fh2 {
            for x in 0..fw2 {
                let i = (y * fw2 + x) as usize;
                if m[i] > 0 {
                    glyph += 1;
                }
                if sh[i] == 0 {
                    continue;
                }
                let (sx, sy) = (x - off, y - off);
                // 源点在版心左/上之外 ⇒ 该阴影像素是裁剪的合法产物，跳过
                if sx < bl || sy < bt {
                    continue;
                }
                let backed = sx >= 0 && sy >= 0 && sx < fw2 && sy < fh2 && {
                    let j = (sy * fw2 + sx) as usize;
                    m[j] > 0
                };
                if !backed {
                    orphan += 1;
                }
            }
        }
        (glyph, orphan)
    };
    rows.push((
        "阴影不越界（每一点阴影都能追到字形）",
        // 排除裁剪缘的合法孤影后不变量**精确为 0**；判据仍留 0（有 1px 都红）
        // 而不是放宽 —— 这才是「每一点阴影都能追到字形」的本义。
        glyph_px > 0 && orphan_px == 0,
        format!(
            "游离阴影 {} px / 字形 {} px（版心左上 ({},{})，位移 {}px，裁剪缘合法孤影已排除）",
            orphan_px,
            glyph_px,
            bl,
            bt,
            off
        ),
    ));

    // 收尾：恢复默认外观，别把后面的行带偏
    a.cfg.font = config::FONT_PRESETS[0].to_string();
    a.cfg.scale = 100;
    a.cfg.clock_scale = 100;
    a.cfg.quote_scale = 100;
    a.cfg.color = config::COLOR_PRESETS[0];
    a.cfg.date_color = config::DATE_COLOR_PRESETS[0];
    a.cfg.clock_dx = 0;
    a.cfg.clock_dy = 0;
    a.rebuild_fonts();
    let o = a.text_alpha();
    build_frame(&mut a, o);

    // ---- 开机自启的开关决策（纯函数）。
    // 写错注册表是会殃及**别人启动项**的事故，必须对全部边界组合下断言 ——
    // 「看起来写对了」在这个场景里不算数。
    {
        /// (场景, Run 键现值, 备份现值, 目标状态, 期望动作)
        type Case<'a> = (&'a str, Option<&'a str>, Option<&'a str>, bool, autostart::Action);
        let exe = "C:\\Apps\\desk-geyan.exe";
        let cases: Vec<Case> = vec![
            ("无现值·开", None, None, true, autostart::Action::Write),
            (
                "现值是自己·开",
                Some(exe),
                None,
                true,
                autostart::Action::Noop,
            ),
            (
                "现值是自己（带引号/大小写不同）·开",
                Some("\"c:\\apps\\DESK-GEYAN.exe\""),
                None,
                true,
                autostart::Action::Noop,
            ),
            (
                "现值是别人·开（须先备份）",
                Some("C:\\Other\\thing.exe"),
                None,
                true,
                autostart::Action::BackupAndWrite,
            ),
            ("无现值·关", None, None, false, autostart::Action::Noop),
            (
                "有现值·关",
                Some(exe),
                None,
                false,
                autostart::Action::Delete,
            ),
            (
                "有现值+有备份·关（须恢复）",
                Some(exe),
                Some("C:\\Old\\old.exe"),
                false,
                autostart::Action::DeleteAndRestore,
            ),
            (
                "无现值但有备份·关（异常态也须恢复）",
                None,
                Some("C:\\Old\\old.exe"),
                false,
                autostart::Action::DeleteAndRestore,
            ),
        ];
        let bad: Vec<&str> = cases
            .iter()
            .filter(|(_, c, p, w, e)| autostart::decide(*c, *p, exe, *w) != *e)
            .map(|(d, _, _, _, _)| *d)
            .collect();
        rows.push((
            "开机自启决策（8 组合）",
            bad.is_empty(),
            if bad.is_empty() {
                format!("{} 组全部符合预期", cases.len())
            } else {
                format!("不符：{}", bad.join(" / "))
            },
        ));
    }

    // ---- 全屏自动隐藏的判定（纯函数）。同样覆盖全部边界组合，
    // 尤其是「用户在全屏期间手动关掉可见性」这条 —— 它决定了退出全屏时
    // 会不会弹回一个用户刚亲手关掉的窗口。
    {
        /// (场景, 是否全屏, 是否已因此藏着, 用户要可见, 期望动作)
        type FsCase<'a> = (&'a str, bool, bool, bool, FsAction);
        let cases: Vec<FsCase> = vec![
            ("进全屏·可见 → 藏", true, false, true, FsAction::Hide),
            ("已藏·仍全屏 → 不动", true, true, true, FsAction::None),
            (
                "全屏·用户本就藏着 → 不动",
                true,
                false,
                false,
                FsAction::None,
            ),
            (
                "退出全屏·正藏着 → 恢复",
                false,
                true,
                true,
                FsAction::Restore,
            ),
            (
                "退出全屏·没藏 → 不动",
                false,
                false,
                true,
                FsAction::None,
            ),
        ];
        let bad: Vec<&str> = cases
            .iter()
            .filter(|(_, f, h, v, e)| fs_decide(*f, *h, *v) != *e)
            .map(|(d, _, _, _, _)| *d)
            .collect();
        rows.push((
            "全屏隐藏判定（5 组合）",
            bad.is_empty(),
            if bad.is_empty() {
                format!("{} 组全部符合预期", cases.len())
            } else {
                format!("不符：{}", bad.join(" / "))
            },
        ));
    }

    // ---- 多文件分组：造**临时目录**，不碰 `%APPDATA%` ——
    // 直接测真实数据目录的话，结果会随机器上用户自己的 quotes.txt 变化。
    {
        let base = std::env::temp_dir().join(format!("desk-geyan-selftest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::create_dir_all(base.join("quotes"));
        // 组 A（单文件）：2 条；组 B：2 条；组 C：1 条
        std::fs::write(base.join("quotes.txt"), "甲一 —— A\n甲二 —— A\n").unwrap();
        std::fs::write(base.join("quotes/01_b.txt"), "乙一 —— B\n乙二 —— B\n").unwrap();
        std::fs::write(base.join("quotes/02_c.txt"), "丙一 —— C\n").unwrap();
        let list = quotes::library_from(&base);
        // round-robin：甲一 乙一 丙一（第 0 轮）→ 甲二 乙二（第 1 轮）
        let got: Vec<&str> = list.iter().take(5).map(|q| q.text.as_str()).collect();
        let want = ["甲一", "乙一", "丙一", "甲二", "乙二"];
        let n_expect = 5 + quotes::builtin_len();
        rows.push((
            "分组轮播：组间均匀穿插",
            got == want && list.len() == n_expect,
            format!("前 5 条 {:?} · 共 {} 条", got, list.len()),
        ));

        // 单文件向后兼容：只有一个组时，顺序必须与旧实现逐字节一致
        let single = base.join("single");
        let _ = std::fs::create_dir_all(&single);
        std::fs::write(single.join("quotes.txt"), "一 —— A\n二 —— A\n三 —— A\n").unwrap();
        let l3 = quotes::library_from(&single);
        let compat = l3.len() == 3 + quotes::builtin_len()
            && l3[0].text == "一"
            && l3[1].text == "二"
            && l3[2].text == "三";
        rows.push((
            "单文件向后兼容（round-robin 退化）",
            compat,
            format!(
                "顺序 一/二/三 {} · 共 {} 条",
                if compat { "保持" } else { "被改变" },
                l3.len()
            ),
        ));

        // BOM + 注释：记事本「另存为 UTF-8」的两种典型坏法
        std::fs::write(
            base.join("quotes.txt"),
            "\u{feff}# 分区注释\n甲一 —— A\n",
        )
        .unwrap();
        let l4 = quotes::library_from(&base);
        // 甲一组剩 1 条 → 第 0 轮：甲一 乙一 丙一；第 1 轮：乙二 = 4 条外部
        let bom_ok = l4.len() == 4 + quotes::builtin_len()
            && l4[0].text == "甲一"
            && l4.iter().all(|q| !q.text.starts_with('#'));
        rows.push((
            "分组 + BOM + 注释剥离",
            bom_ok,
            format!("共 {} 条外部{}", l4.len() - quotes::builtin_len(), if bom_ok { "" } else { "（异常）" }),
        ));

        let _ = std::fs::remove_dir_all(&base);
    }

    // ================================================================
    // 收藏当前句子：复制到剪贴板 + 加入轮播。
    // 两件事必须分开验 —— 剪贴板写坏了不影响收藏，收藏写坏了也不影响复制。
    // ================================================================
    {
        use favorites::FavAction;

        // ---- ① 查重决策（纯函数）。重复点同一句不该写第二次，
        // 否则收藏夹里同一个组出现同一内容，轮播会连着播两句一样的。
        {
            let existing = vec![
                crate::quotes::Quote {
                    text: "海内存知己".into(),
                    author: "王勃".into(),
                },
                crate::quotes::Quote {
                    text: "天涯若比邻".into(),
                    author: "".into(),
                },
            ];
            /// (场景, 待判句子, 期望动作)
            type Case<'a> = (&'a str, crate::quotes::Quote, FavAction);
            let cases: Vec<Case> = vec![
                (
                    "全新句子 → 追加",
                    crate::quotes::Quote {
                        text: "千里之行，始于足下。".into(),
                        author: "老子".into(),
                    },
                    FavAction::Append,
                ),
                (
                    "完全重复 → 跳过",
                    crate::quotes::Quote {
                        text: "海内存知己".into(),
                        author: "王勃".into(),
                    },
                    FavAction::Skip,
                ),
                (
                    "只差空白 → 跳过（norm 去空白）",
                    crate::quotes::Quote {
                        text: " 海内 存 知 己 ".into(),
                        author: "".into(),
                    },
                    FavAction::Skip,
                ),
                (
                    "只差标点 → 跳过（norm 去标点）",
                    crate::quotes::Quote {
                        text: "海内存知己。".into(),
                        author: "".into(),
                    },
                    FavAction::Skip,
                ),
                (
                    "同正文无出处 → 跳过",
                    crate::quotes::Quote {
                        text: "天涯若比邻".into(),
                        author: "".into(),
                    },
                    FavAction::Skip,
                ),
                (
                    "空正文 → 跳过（不写空行）",
                    crate::quotes::Quote {
                        text: "   ".into(),
                        author: "".into(),
                    },
                    FavAction::Skip,
                ),
            ];
            let bad: Vec<&str> = cases
                .iter()
                .filter(|(_, q, e)| favorites::decide(&existing, q) != *e)
                .map(|(d, _, _)| *d)
                .collect();
            rows.push((
                "收藏查重决策（6 组合）",
                bad.is_empty(),
                if bad.is_empty() {
                    format!("{} 组全部符合预期", cases.len())
                } else {
                    format!("不符：{}", bad.join(" / "))
                },
            ));
        }

        // ---- ② 重载后 qi 必须仍指着**同一条句子**（防「点了收藏画面自己跳了」）。
        // 收藏发生在列表尾部，索引不越界但 round-robin 交织后位置全变 ——
        // 用 `qi.min(len-1)` 兜底就是这里最隐蔽的错法。
        {
            fn q(t: &str, a: &str) -> crate::quotes::Quote {
                crate::quotes::Quote {
                    text: t.into(),
                    author: a.into(),
                }
            }
            let list = vec![
                q("甲", "A"),
                q("乙", "B"),
                q("丙", "C"),
                q("丁", "D"),
                q("戊", "E"),
            ];
            /// (场景, 当前句 display, 兜底索引, 期望 qi)
            type Case<'a> = (&'a str, &'a str, usize, usize);
            let cases: Vec<Case> = vec![
                ("句在列表中部 → 精确找回", "丙　——　C", 0, 2),
                ("句被挪到队尾 → 跟着走", "戊　——　E", 0, 4),
                ("句已不在（文件被改坏）→ 退回兜底", "己　——　F", 3, 3),
                ("兜底越界 → 夹到末条", "己　——　F", 99, 4),
            ];
            let bad: Vec<&str> = cases
                .iter()
                .filter(|(_, cur, fb, e)| keep_index(cur, &list, *fb) != *e)
                .map(|(d, _, _, _)| *d)
                .collect();
            rows.push((
                "收藏后 qi 保持（4 组合）",
                bad.is_empty(),
                if bad.is_empty() {
                    format!("{} 组全部符合预期", cases.len())
                } else {
                    format!("不符：{}", bad.join(" / "))
                },
            ));
        }

        // ---- ③ 存储形态往返：写出去的行必须能被同一个解析器读回原样。
        // 「写进去了」和「读出来了」中间隔着一个解析器 —— 这句话项目里
        // 已经用 BOM 栽过一次，这里不赌。
        {
            let with_author = favorites::line_of(&crate::quotes::Quote {
                text: "千里之行，始于足下。".into(),
                author: "老子".into(),
            });
            let no_author = favorites::line_of(&crate::quotes::Quote {
                text: "路漫漫其修远兮".into(),
                author: "".into(),
            });
            let probe_dir = std::env::temp_dir().join(format!("desk-geyan-favline-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&probe_dir);
            let _ = std::fs::create_dir_all(&probe_dir);
            std::fs::write(
                probe_dir.join("t.txt"),
                format!("{}\n{}\n", with_author, no_author),
            )
            .unwrap();
            let back = quotes::read_external(&probe_dir.join("t.txt")).unwrap();
            let _ = std::fs::remove_dir_all(&probe_dir);
            let ok = back.len() == 2
                && back[0].text == "千里之行，始于足下。"
                && back[0].author == "老子"
                && back[1].text == "路漫漫其修远兮"
                && back[1].author.is_empty();
            rows.push((
                "收藏行往返（写入 → 解析 → 逐字相等）",
                ok,
                format!(
                    "「{}」/「{}」→ 正文+出处 {}",
                    with_author,
                    no_author,
                    if ok { "一致" } else { "不一致" }
                ),
            ));
        }

        // ---- ④ 端到端：造临时目录，不碰 `%APPDATA%`。
        // 核心断言是**「用户的源文件一个字节都没动」** ——
        // 收藏是程序替用户写文件，写错地方就是替用户改内容。
        {
            let base = std::env::temp_dir().join(format!("desk-geyan-fav-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let _ = std::fs::create_dir_all(base.join("quotes"));
            std::fs::write(base.join("quotes.txt"), "甲一 —— A\n甲二 —— A\n").unwrap();
            std::fs::write(base.join("quotes/01_b.txt"), "乙一 —— B\n").unwrap();
            let src_before = std::fs::read(base.join("quotes.txt")).unwrap();
            let group_before = std::fs::read(base.join("quotes/01_b.txt")).unwrap();

            let q = crate::quotes::Quote {
                text: "落霞与孤鹜齐飞".into(),
                author: "王勃".into(),
            };
            let first = favorites::append(&base, &q).unwrap();
            let second = favorites::append(&base, &q).unwrap(); // 幂等
            let fav_list = favorites::load(&base);

            let src_after = std::fs::read(base.join("quotes.txt")).unwrap();
            let group_after = std::fs::read(base.join("quotes/01_b.txt")).unwrap();
            let files_clean = src_before == src_after && group_before == group_after;

            let groups = quotes::load_groups(&base);
            let fav_group = groups
                .iter()
                .find(|(n, _)| n == favorites::FILE)
                .map(|(_, l)| l.len());
            let list = quotes::library_from(&base);

            rows.push((
                "收藏写入：幂等 + 不污染源文件",
                first && !second && fav_list.len() == 1 && files_clean,
                format!(
                    "第一次写入={} 第二次={}（须 false）· 收藏夹 {} 条 · 源文件 {}",
                    first,
                    second,
                    fav_list.len(),
                    if files_clean { "字节未变" } else { "被改动！" }
                ),
            ));
            rows.push((
                "收藏成为独立分组（favorites.txt）",
                fav_group == Some(1) && list.len() == 4 + quotes::builtin_len(),
                format!(
                    "组名 {} · {} 条 · 交织后总库 {} 条",
                    favorites::FILE,
                    fav_group.unwrap_or(0),
                    list.len()
                ),
            ));
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    // ---- 剪贴板往返。**测前读回、测后恢复** —— selftest 是用户会跑的东西，
    // 不能把人家剪贴板里的内容吃掉。原内容非文本时恢复不了，
    // 那一行的判据里如实排除，但在详情里说清楚。
    {
        let hwnd = a.panel;
        let before = clipboard::get_text(hwnd);
        const PROBE_TEXT: &str = "桌面格言 · 剪贴板往返自检";
        let wrote = clipboard::copy_text(hwnd, PROBE_TEXT);
        let back = clipboard::get_text(hwnd);
        let roundtrip = wrote && back.as_deref() == Some(PROBE_TEXT);
        rows.push((
            "剪贴板往返（写 → 读 → 逐字相等）",
            roundtrip,
            format!(
                "写入 {} · 读回 {}",
                if wrote { "成功" } else { "失败" },
                match &back {
                    Some(s) => format!("「{}」", s),
                    None => "空 / 非文本".to_string(),
                }
            ),
        ));
        // 恢复
        let (restored, note) = match &before {
            Some(s) => (clipboard::copy_text(hwnd, s), "原内容为文本，已原样恢复"),
            None => (true, "原内容非文本或为空，无法恢复（如实报告）"),
        };
        rows.push((
            "剪贴板原内容恢复",
            restored,
            format!("{} · 恢复 {}", note, if restored { "完成" } else { "失败" }),
        ));
    }

    // ================================================================
    // 排版引擎与竖版。这里是 D1（长句静默截断）与 D2（大字号整条消失）
    // 的回归位 —— 两条缺陷同源（渲染管线里没有排版层），断言也放在一起。
    // 每条都「真的改配置 → 真的重排版 → 量结果」，不看代码猜结论。
    // ================================================================
    {
        let saved_cfg = a.cfg.clone();
        let saved_quote = a.list[a.qi].clone();
        let long_text = "国".repeat(60);

        // ---- ① D1：长句必须折行，且**一个字都不许丢**
        a.cfg = Config::default();
        a.cfg.anim = Anim::Static;
        a.list[a.qi] = Quote {
            text: long_text.clone(),
            author: "测试".to_string(),
        };
        a.rebuild_fonts();
        let cb = layout_clock(&mut a);
        let ql = frame_layout(&a, &cb);
        // `display()` 会把「正文　——　出处」拼成一整串，所以要按**实际串长**对账
        let want = a.list[a.qi].display().chars().count();
        let inside = ql
            .glyphs
            .iter()
            .all(|g| g.x >= 0 && g.y >= 0 && g.x < a.cfg.w && g.y < a.cfg.h);
        rows.push((
            "排版·长句折行（D1 回归）",
            ql.lanes >= 2 && ql.len() == want && !ql.ellipsis && inside,
            format!(
                "60 字正文（连出处共 {} 字）→ {} 行 / 画出 {} 字 / 占 {}x{} / 省略号 {} / 全在窗内 {}",
                want,
                ql.lanes,
                ql.len(),
                ql.extent.0,
                ql.extent.1,
                ql.ellipsis,
                inside
            ),
        ));

        // ---- ② D2：字号大到把版心吃光时，**绝不许静默消失**。
        // 旧实现在 scale≥205 时 `band_r > band_l + 20` 不成立 → 整段跳过绘制。
        // 现在的保证分两种，都是可量像素的：
        //   * 版心还撑得住 → 格言照画
        //   * 版心退化（时钟把窗口吃光）→ 顶部必须出现那句明确提示
        // 唯一不允许的结果是「什么都不画、什么也不说」。
        let mut d2_detail = String::new();
        let mut d2_ok = true;
        for s in [200, 220, 250, 300] {
            a.cfg.scale = s;
            a.rebuild_fonts();
            let cb = layout_clock(&mut a);
            let ql = frame_layout(&a, &cb);
            let deg = band_degenerate(&a.cfg, &cb);
            let o = a.text_alpha();
            build_frame(&mut a, o);
            let fw = a.frame.w;
            let top_ink = ink_in(a.frame.out_pixels(), fw, 0, 0, fw, 18);
            let ok = if deg { top_ink > 0 } else { !ql.is_empty() };
            if !ok {
                d2_ok = false;
            }
            d2_detail.push_str(&format!(
                "{}%:{}字{} ",
                s,
                ql.len(),
                if deg { "→顶部提示" } else { "" }
            ));
        }
        rows.push(("排版·大字号不静默消失（D2 回归）", d2_ok, d2_detail));

        // ---- ②b 横向挤不开时改成上下堆叠（而不是硬塞在右边被压扁）
        //
        // **不写死一组参数**：时钟块和格言的实际宽度由**系统字体**量出来，
        // 写死的那一组会在换字体 / 换 DPI 之后悄悄失去意义 —— 用户以为这条
        // 断言还在守着「挤不开就堆叠」，其实它测的已经是另一组几何了。
        // 所以扫一小片「时钟偏大 / 窗口偏窄」的组合，要求**至少有一组**
        // 真的把版心切到时钟**下方**。
        //
        // 注毒可红：把 `quote_band` 的第②段删掉，所有组合的 band.1 都会退回
        // `pad`（远小于时钟底）→ hit 永远 None → 这条断言立刻变红。
        a.cfg = Config::default();
        a.cfg.scale = 175;
        a.list[a.qi] = Quote {
            text: "国".repeat(40),
            author: "测试".to_string(),
        };
        let mut hit: Option<(i32, i32, i32, i32, i32, i32)> = None;
        for &(w, h, cs) in &[
            (620, 560, 200),
            (560, 520, 200),
            (620, 640, 180),
            (500, 560, 220),
        ] {
            a.cfg.w = w;
            a.cfg.h = h;
            a.cfg.clock_scale = cs;
            a.rebuild_fonts();
            let cb = layout_clock(&mut a);
            if !cb.show || band_degenerate(&a.cfg, &cb) {
                continue;
            }
            let band = frame_band(&a.cfg, &cb);
            // band.1 落在**时钟底之下** ⇒ 这一帧的版心来自「时钟下方」那一段。
            // 第①段（时钟右侧）的 band.1 恒等于 pad，第③段（整窗）也是 pad ——
            // 所以这条判据同时把「堆叠」和另外两段分开了。
            if band.1 < cb.y + cb.h {
                continue;
            }
            let ql = frame_layout(&a, &cb);
            if !ql.is_empty() {
                hit = Some((w, h, cs, band.1, cb.y + cb.h, ql.len() as i32));
                break;
            }
        }
        // 回到 ③ 期望的初始状态（③ 当年就是在 620x400 下跑的）
        a.cfg = Config::default();
        a.cfg.w = 620;
        a.cfg.h = 400;
        rows.push((
            "排版·挤不开时改成上下堆叠",
            hit.is_some(),
            match hit {
                Some((w, h, cs, by, cb_bottom, n)) => format!(
                    "{}x{} 时钟档={}% → 版心 y={} ≥ 时钟底 {} · 画出 {} 字",
                    w, h, cs, by, cb_bottom, n
                ),
                None => "扫过 4 组「时钟大 / 窗口窄」参数，没有一组落到时钟下方".to_string(),
            },
        ));

        // ---- ③ 缩字阶梯真的会降档，而且关掉开关就一步都不降
        a.cfg.scale = 250;
        a.cfg.shrink_fit = false;
        a.rebuild_fonts();
        let cb = layout_clock(&mut a);
        let off = frame_layout(&a, &cb).shrink;
        a.cfg.shrink_fit = true;
        let cb = layout_clock(&mut a);
        let on = frame_layout(&a, &cb).shrink;
        rows.push((
            "排版·缩字回退阶梯（70% 下限）",
            off == 100 && on != 100 && on >= 70,
            format!("关掉时 {}% · 打开时 {}%（250% 字号下）", off, on),
        ));

        // ---- ④ 竖版：主文竖排、列序自右向左、字形不越出窗口
        a.cfg = Config::default();
        a.cfg.set_orient(Orient::Vertical);
        a.list[a.qi] = Quote {
            text: long_text.clone(),
            author: "测试".to_string(),
        };
        a.rebuild_fonts();
        let cb = layout_clock(&mut a);
        let ql = frame_layout(&a, &cb);
        let v_inside = ql
            .glyphs
            .iter()
            .all(|g| g.x >= 0 && g.y >= 0 && g.x < a.cfg.w && g.y < a.cfg.h);
        let right_first = match (ql.glyphs.first(), ql.glyphs.last()) {
            (Some(f), Some(l)) => f.x >= l.x && f.y <= l.y,
            _ => false,
        };
        rows.push((
            "竖版·主文竖排右起",
            !ql.is_empty() && v_inside && right_first && cb.sig.len() == 3,
            format!(
                "{}x{} · {} 列 / 落款 {} 列 / 全在窗内 {}",
                a.cfg.w,
                a.cfg.h,
                ql.lanes,
                cb.sig.len(),
                v_inside
            ),
        ));
        rows.push((
            "竖版·跑马灯降级为静态",
            a.cfg.eff_anim() == Anim::Static && a.cfg.anim == Anim::Marquee,
            format!(
                "配置动效 {} → 生效 {}",
                a.cfg.anim.label(),
                a.cfg.eff_anim().label()
            ),
        ));

        // 竖版标点归位：拿一条带句读的短句，看「，」是不是被抬到了本格右上
        a.cfg = Config::default();
        a.cfg.set_orient(Orient::Vertical);
        a.list[a.qi] = Quote {
            text: "自强不息，厚德载物。".to_string(),
            author: "《周易》".to_string(),
        };
        a.rebuild_fonts();
        let cb = layout_clock(&mut a);
        let ql = frame_layout(&a, &cb);
        // 判据要拿**它自己那一格**比：点号相对前一字的常规步进位置是上移的。
        // 直接比 `dot.y < prev.y` 是错的 —— 它仍然在上一字下方，
        // 只是比自己本该在的位置高了半格（正是「本格右上」）。
        // 步进值从**前两个正常字形**的差里取，不写死常量。
        let comma_ok = {
            let step = match (ql.glyphs.first(), ql.glyphs.get(1)) {
                (Some(a0), Some(a1)) => a1.y - a0.y,
                _ => 0,
            };
            ql.glyphs
                .iter()
                .position(|g| g.ch == 0xFF0C)
                .and_then(|i| (i > 0).then(|| (ql.glyphs[i], ql.glyphs[i - 1])))
                .map(|(dot, prev)| step > 0 && dot.y < prev.y + step && dot.x > prev.x)
                .unwrap_or(false)
        };
        rows.push((
            "竖版·句读点号归位到单元格右上",
            comma_ok,
            format!(
                "「，」共 {} 个字形",
                ql.glyphs.iter().filter(|g| g.ch == 0xFF0C).count()
            ),
        ));

        // ---- ④b 竖版·仿古排版三连（2026-10-15 老板报：落款与正文脱节 /
        // 断句碎 / 调窗口时文字看不见）。三条判据互为表里：
        // ① 主文不带出处尾巴（display 的「　——　作者」只归落款条）；
        // ② 断句收在句读后（「春社」「衣冠」不被劈到两列）；
        // ③ 落款条右缘贴主文块左缘（宽窗下不再隔几百像素空白）。
        a.cfg = Config::default();
        a.cfg.set_orient(Orient::Vertical);
        a.cfg.w = 1052;
        a.cfg.h = 315;
        a.list[a.qi] = Quote {
            text: "箫鼓追随春社近，衣冠简朴古风存。".to_string(),
            author: "陆游".to_string(),
        };
        a.rebuild_fonts();
        let cb = layout_clock(&mut a);
        let ql = frame_layout(&a, &cb);
        let want_chars = a.list[a.qi].text.chars().count();
        let attr_stripped = ql.len() == want_chars
            && ql
                .glyphs
                .iter()
                .all(|g| g.ch != 0x2014 && g.ch != 0x9646); // —— 与「陆」都不在主文
        rows.push((
            "竖版·主文不带出处（署名归落款条）",
            attr_stripped,
            format!(
                "正文 {} 字 / 画出 {} 字形 / 竖杠 {} 个",
                want_chars,
                ql.len(),
                ql.glyphs.iter().filter(|g| g.ch == 0x2014).count()
            ),
        ));
        let breaks_ok = ql.col_ranges.len() == 2
            && ql.col_ranges.iter().all(|&(g0, g1)| {
                layout::clause_end(ql.glyphs[g1 - 1].ch)
                    && !layout::no_start(ql.glyphs[g0].ch)
            });
        rows.push((
            "竖版·断句收在句读后（一句一列）",
            breaks_ok,
            format!(
                "{} 列 · 列末字 {:04X?}",
                ql.col_ranges.len(),
                ql.col_ranges
                    .iter()
                    .map(|&(_, g1)| ql.glyphs[g1 - 1].ch)
                    .collect::<Vec<_>>()
            ),
        ));
        // ③ 落款跟排的最终位置只能从 build_frame 读（hug 在里面做），
        // clock_rect 就是落款条落定后的矩形 —— 读内核结果，不复制算法。
        let op = a.text_alpha();
        build_frame(&mut a, op);
        let cr = a.clock_rect;
        let main_left = a.cfg.w - 24 - ql.extent.0; // br(w-24) - 主文宽
        let hug_ok = (cr.right - (main_left - 24)).abs() <= 2 && cr.left >= 24;
        rows.push((
            "竖版·落款跟排（右缘贴主文左缘）",
            hug_ok,
            format!(
                "落款右缘 {} · 主文左缘 {} · 期望贴在 {} · 落款左缘 {}（≥24）",
                cr.right,
                main_left,
                main_left - 24,
                cr.left
            ),
        ));

        // ---- ⑥ 合批绘制 vs 逐字绘制：**逐像素必须一模一样**。
        //
        // 合批是一条**关于 GDI 的假设**（假设它按「步进 + 字距」推进字符，
        // 与 `GetTextExtentExPointW` 差出来的步进一致）。假设不改像素才算成立，
        // 所以这里真的画两遍、把正文掩码整块比一遍 —— 不靠「看起来没变」。
        // `build_frame_ex` 的 `Some(true/false)` 就是为这条断言存在的开关；
        // 毒在它身上：把 `runs()` 的 `src` 连续性判据删掉（乱合批），diff 立刻 > 0。
        a.cfg = Config::default();
        a.cfg.anim = Anim::Static;
        a.list[a.qi] = Quote {
            text: long_text.clone(),
            author: "测试".to_string(),
        };
        a.rebuild_fonts();
        let cb = layout_clock(&mut a);
        let ql = frame_layout(&a, &cb);
        build_frame_ex(&mut a, 215, Some(false));
        let batched = a.frame.ink_mask();
        build_frame_ex(&mut a, 215, Some(true));
        let per_glyph = a.frame.ink_mask();
        // 最后再出一帧正常的，别让后续断言读到一个逐字绘制的残留帧
        build_frame_ex(&mut a, 215, Some(false));
        let diff = batched
            .iter()
            .zip(per_glyph.iter())
            .filter(|(x, y)| x != y)
            .count();
        // 除了「差了几个像素」，还要读回两个独立事实：
        // 墨量（少了/多了就说明有字没画出来或画重了）与包围盒（位置整体偏了就看得出来）。
        let fw = a.frame.w;
        let ink_of = |m: &[u8]| m.iter().filter(|&&v| v > 0).count();
        let bbox_of = |m: &[u8]| -> (i32, i32, i32, i32) {
            let (mut l, mut t, mut r, mut b) = (i32::MAX, i32::MAX, -1, -1);
            for (i, &v) in m.iter().enumerate() {
                if v > 0 {
                    let (x, y) = (i as i32 % fw, i as i32 / fw);
                    l = l.min(x);
                    t = t.min(y);
                    r = r.max(x);
                    b = b.max(y);
                }
            }
            (l, t, r, b)
        };
        let (bi, pi) = (ink_of(&batched), ink_of(&per_glyph));
        let (bb, pb) = (bbox_of(&batched), bbox_of(&per_glyph));
        rows.push((
            "排版·合批绘制与原逐字绘制逐像素相同",
            diff == 0 && !batched.is_empty(),
            format!(
                "{} px 不同 · 墨量 {} vs {} · 包围盒 {:?} vs {:?} · 合批 {} 段（原 {} 字）",
                diff,
                bi,
                pi,
                bb,
                pb,
                ql.runs().len(),
                ql.len()
            ),
        ));

        // 竖版再单独验一遍。竖排里每个字都在**不同的 `y`** 上，`TextOutW` 一条
        // 基线都用不上，合批完全依赖 `PolyTextOutW` 每段自带的 `(x, y)` ——
        // 它比横版更容易出错（横版至少还有「同一行」这个天然分组），而它恰恰
        // 是本轮性能改动的主要受益者。只验横版等于没验到这条。
        //
        // 文本刻意带上 `——` 与 `……`：这两个字在竖排里要用**旋转字体**画，
        // 而 `POLYTEXTW` 里没有字体字段（字体来自 DC），所以合批必须按字体
        // 分成两组各提交一次。横版永远不会走到这条分支，只有竖版能验到它。
        a.cfg = Config::default();
        a.cfg.set_orient(Orient::Vertical);
        a.cfg.anim = Anim::Static;
        a.list[a.qi] = Quote {
            text: format!("{}——{}……", "国".repeat(18), "国".repeat(18)),
            author: "测试".to_string(),
        };
        a.rebuild_fonts();
        let cb = layout_clock(&mut a);
        let qlv = frame_layout(&a, &cb);
        build_frame_ex(&mut a, 215, Some(false));
        let v_batched = a.frame.ink_mask();
        build_frame_ex(&mut a, 215, Some(true));
        let v_per_glyph = a.frame.ink_mask();
        build_frame_ex(&mut a, 215, Some(false));
        let v_diff = v_batched
            .iter()
            .zip(v_per_glyph.iter())
            .filter(|(x, y)| x != y)
            .count();
        let (vbi, vpi) = (ink_of(&v_batched), ink_of(&v_per_glyph));
        // 「有旋转字」是这条断言的前提，不是附加信息：文本一旦改回纯汉字，
        // 它就退化成横版那条的副本，而没人会注意到。所以把它写进判据。
        let v_rot = qlv.glyphs.iter().filter(|g| g.rot).count();
        rows.push((
            "排版·竖版合批绘制与原逐字绘制逐像素相同",
            v_diff == 0 && !v_batched.is_empty() && v_rot > 0,
            format!(
                "{}x{} · {} px 不同 · 墨量 {} vs {} · 旋转字 {} 个 · 合批 {} 段（原 {} 字）",
                a.cfg.w,
                a.cfg.h,
                v_diff,
                vbi,
                vpi,
                v_rot,
                qlv.runs().len(),
                qlv.len()
            ),
        ));

        // ---- ⑤ 配置四端一致：apply_kv → save → load 逐字段相等。
        // save 会写用户的 config.ini，所以整段套在 with_config_guard 里 ——
        // 「测一下」绝不能把人家摆好的配置带偏。
        let mut rt_ok = false;
        let mut rt_detail = String::new();
        with_config_guard(|| {
            let mut c = Config::default();
            config::apply_kv(&mut c, "orient", "vertical");
            config::apply_kv(&mut c, "letter_spacing", "5");
            config::apply_kv(&mut c, "quote_color", "#F5D98B");
            config::apply_kv(&mut c, "shrink_fit", "false");
            c.save();
            let back = Config::load();
            rt_ok = back.orient == Orient::Vertical
                && back.letter_spacing == 5
                && back.quote_color == 0xF5_D98B
                && !back.shrink_fit;
            rt_detail = format!(
                "orient={} 字距={} 格言色={} 缩字={}",
                back.orient.key(),
                back.letter_spacing,
                config::quote_color_label(back.quote_color),
                back.shrink_fit
            );
            0
        });
        rows.push(("配置四端一致（orient / 字距 / 格言色）", rt_ok, rt_detail));

        // 恢复现场
        a.cfg = saved_cfg;
        a.list[a.qi] = saved_quote;
        a.rebuild_fonts();
    }

    let mut pass = true;
    for (name, ok, detail) in &rows {
        if !ok {
            pass = false;
        }
        println!(
            "[{}] {:<32} {}",
            if *ok { "PASS" } else { "FAIL" },
            name,
            detail
        );
    }
    println!(
        "\n{}  像素构成: 全不透明 {} / 半透明 {} / 全透明 {}",
        if pass { "全部通过" } else { "存在失败项" },
        solid,
        partial,
        total - solid - partial
    );
    if pass {
        0
    } else {
        3
    }
}

// ---------------------------------------------------------------- 窗口探测

enum R {
    Pass,
    Fail,
    Info,
}

/// `desk-geyan --probe`：真的把窗口建出来，再**读回**扩展样式逐位验证。
///
/// 「不影响桌面其他东西使用」这件事必须在系统层面成立，光看代码不算数 ——
/// 这个模式就是把三道闸的样式位从内核读回来对数。
pub fn probe() -> i32 {
    // 探测会真的建窗口、真的写配置，所以必须零副作用 ——
    // 否则「测一下」就把用户摆好的位置带偏了。
    with_config_guard(probe_inner)
}

/// 把「设置面板管得着的那部分配置」压成一行文本。
///
/// 用来判定「点一下到底有没有改到东西」。**刻意不收 `x/y/w/h`** —— 那些是窗口几何，
/// 切换版式 / 归位时会跟着动；把它们算进来，断言就分不清「用户点的是这个控件」
/// 还是「别的什么顺手动了窗口」。
fn cfg_fingerprint(a: &App) -> String {
    let c = &a.cfg;
    format!(
        "clock={clock} date={date} quote={quote} shadow={shadow} sec={sec} top={top} \
         autohide={autohide} layout={layout} shrink={shrink} mode={mode} anim={anim} \
         orient={orient} op={op} spd={spd} scale={scale} cs={cs} qs={qs} sp={sp} \
         col={col:06X} dcol={dcol:06X} qcol={qcol:06X} font={font}",
        clock = c.show_clock,
        date = c.show_date,
        quote = c.show_quote,
        shadow = c.shadow,
        sec = c.seconds,
        top = c.always_on_top,
        autohide = c.auto_hide_fs,
        layout = c.layout_mode,
        shrink = c.shrink_fit,
        mode = c.mode.label(),
        anim = c.anim.key(),
        orient = c.orient.key(),
        op = c.opacity,
        spd = c.speed,
        scale = c.scale,
        cs = c.clock_scale,
        qs = c.quote_scale,
        sp = c.letter_spacing,
        col = c.color,
        dcol = c.date_color,
        qcol = c.quote_color,
        font = c.font,
    )
}

fn probe_inner() -> i32 {
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }
        let Some(mut st) = App::new() else {
            println!("FAIL  初始化失败");
            return 3;
        };
        st.hinst = GetModuleHandleW(null());
        // 探测必须可复现：不能因为用户把配置存成了「纯文字模式」，
        // 毛玻璃这条就静默变成 INFO。显式固定到毛玻璃路径再测。
        st.cfg = Config::for_test();
        // 字体必须跟着基线配置重建 —— 理由见 `Config::for_test()` 的文档
        // （不重建的话，用户那个字体决定时钟块多高，进而决定命中判定认不认得出「整窗拖动」）
        st.rebuild_fonts();
        // 但**建窗之前**要先摆成纯文字模式 —— 这样 boot 之后底板的可见性
        // 才有硬判据，而且正好覆盖「底板被无条件显示出来」那个真出现过的 bug
        // （它是「看不见却拦鼠标」，必须在建窗后**立刻**看，中间不能夹 apply_mode，
        // 否则 apply_glass 会把它抹掉，断言就永远绿了）。
        st.cfg.mode = Mode::TextOnly;
        st.cfg.layout_mode = false;
        let boxed = Box::new(st);
        app_ptr_set(Box::into_raw(boxed));
        let a = app();

        if !bootstrap(a) {
            println!("FAIL  窗口创建失败");
            return 2;
        }
        let boot_panel = IsWindowVisible(a.panel) != 0;
        let boot_text = IsWindowVisible(a.text) != 0;

        let ex_panel = GetWindowLongPtrW(a.panel, GWL_EXSTYLE) as u32;
        let ex_text = GetWindowLongPtrW(a.text, GWL_EXSTYLE) as u32;
        let bit = |ex: u32, flag: u32| ex & flag != 0;
        let both = |flag: u32| bit(ex_panel, flag) && bit(ex_text, flag);

        let mut rows: Vec<(&str, R, String)> = Vec::new();
        rows.push((
            "面板窗口（毛玻璃底板）",
            if a.panel.is_null() { R::Fail } else { R::Pass },
            format!("HWND {:p}", a.panel),
        ));
        rows.push((
            "文字窗口（per-pixel alpha）",
            if a.text.is_null() { R::Fail } else { R::Pass },
            format!("HWND {:p}", a.text),
        ));
        // ---- 底板可见性：建窗后**立刻**读，中间不许夹 apply_mode
        //
        // 补的是一个真出现过的漏洞：`bootstrap` 曾在 `apply_mode` 之后又无条件
        // `ShowWindow(a.panel)`，于是纯文字模式下底板被显示了出来。它是一片
        // **看不见却拦鼠标**的窗口（topmost + 非 layered ⇒ `WS_EX_TRANSPARENT`
        // 不生效），用户表现为「看不见任何东西却点不动底下的窗口」。
        // 光比样式位抓不到它 —— 必须比可见性，而且必须趁 apply_mode 还没再跑。
        rows.push((
            "建窗后底板未越权显形",
            if boot_panel { R::Fail } else { R::Pass },
            format!(
                "纯文字模式建窗后：底板 visible={} （须 0）/ 文字 visible={}",
                boot_panel as u8, boot_text as u8
            ),
        ));

        // 后面几条要看毛玻璃，切到毛玻璃路径再读
        a.cfg.mode = Mode::Glass;
        apply_mode(a);
        pump();

        // ---- 底板可见性：必须与「该不该显示」严格一致
        {
            let (was_mode, was_layout) = (a.cfg.mode, a.cfg.layout_mode);
            a.cfg.mode = Mode::TextOnly;
            a.cfg.layout_mode = false;
            apply_mode(a);
            pump();
            let want = a.should_show_panel();
            let got = IsWindowVisible(a.panel) != 0;
            rows.push((
                "底板只在该显示时才显示",
                if got == want { R::Pass } else { R::Fail },
                format!(
                    "纯文字模式：可见={} 应为={}（底板显形却非 layered 时会把点击全吃掉）",
                    got as u8, want as u8
                ),
            ));
            a.cfg.mode = was_mode;
            a.cfg.layout_mode = was_layout;
            apply_mode(a);
            pump();
        }

        // ---- 「挂件回到右上角」：菜单命令端到端（读回内核真实矩形）。
        // 这条曾经回归成空操作：处理器设好目标位置后错调了反向的
        // `sync_from_panel`（读窗口矩形覆写 cfg），目标立刻被旧位置改回去。
        // 断言必须打在窗口的真实位置上，cfg 自证无效（它可能根本没写进去）。
        {
            // MI_RESET 末尾会 `cfg.save()` —— 先把数据目录指到临时目录，
            // 别把测试位置写进用户的真实 config.ini。
            let dir = std::env::temp_dir().join(format!("desk-geyan-probe-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            std::env::set_var("DG_DATA_DIR", &dir);

            // 先把窗口真的挪到已知别处，保证「点菜单前不在右上角」
            a.cfg.x = 120;
            a.cfg.y = 400;
            apply_geometry(a, false, true);
            pump();
            let before = win_rect(a.panel);

            on_menu(a, MI_RESET);
            pump();
            let after = win_rect(a.panel);
            let (wa, _) = work_area_at(before.left, before.top, a.cfg.w);
            let want_x = wa.right - a.cfg.w - 24;
            let want_y = wa.top + 24;
            let err = (after.left - want_x).abs().max((after.top - want_y).abs());
            rows.push((
                "「挂件回到右上角」端到端",
                if err <= 2 { R::Pass } else { R::Fail },
                format!(
                    "点菜单前 ({},{}) → 菜单后窗口左上角 ({},{})，期望 ({},{}) · 偏差 {}px（≤2）",
                    before.left, before.top, after.left, after.top, want_x, want_y, err
                ),
            ));
        }
        rows.push((
            "文字窗口穿透位（仅样式位）",
            if bit(ex_text, WS_EX_TRANSPARENT) {
                R::Pass
            } else {
                R::Fail
            },
            "WS_EX_TRANSPARENT —— 样式位证明不了行为，行为用 --clicktest 实测".into(),
        ));
        rows.push((
            "永不抢焦点 → 不打断输入",
            if both(WS_EX_NOACTIVATE) {
                R::Pass
            } else {
                R::Fail
            },
            "WS_EX_NOACTIVATE".into(),
        ));
        rows.push((
            "不进任务栏 / Alt-Tab",
            if both(WS_EX_TOOLWINDOW) {
                R::Pass
            } else {
                R::Fail
            },
            "WS_EX_TOOLWINDOW".into(),
        ));
        rows.push((
            "文字窗口分层（ULW 前提）",
            if bit(ex_text, WS_EX_LAYERED) {
                R::Pass
            } else {
                R::Fail
            },
            "WS_EX_LAYERED".into(),
        ));
        rows.push((
            "总在最前",
            if a.cfg.always_on_top && both(WS_EX_TOPMOST) {
                R::Pass
            } else {
                R::Info
            },
            format!("always_on_top={}", a.cfg.always_on_top),
        ));
    rows.push((
        "全局热键",
        if a.hotkeys.len() >= HOTKEY_EXPECT {
            R::Pass
        } else if a.hotkey_err == ERROR_HOTKEY_ALREADY_EXISTS {
            // 被另一个实例占着：单实例互斥量保证正式运行不会互相抢，
            // 这里如实标 INFO，而不是判 FAIL 让整轮探测失去意义
            R::Info
        } else {
            R::Fail
        },
        format!(
            // 期望数从注册表本身推，不写死 —— 加一个热键就改一处常量的时代
            // 已经让这个文案变成过「18/17 注册成功」这种笑话。
            "{}/{} 注册成功{}",
            a.hotkeys.len(),
            HOTKEY_EXPECT,
                if a.hotkey_err == ERROR_HOTKEY_ALREADY_EXISTS {
                    "（其余被另一个实例占用：本机正跑着挂件）"
                } else if a.hotkey_err != 0 {
                    "（注册被系统拒绝，见 GetLastError）"
                } else {
                    ""
                }
            ),
        ));
        rows.push((
            "托盘图标",
            if a.tray_ok { R::Pass } else { R::Fail },
            if a.tray_ok {
                "已挂载".into()
            } else {
                format!(
                    "挂载失败 · GetLastError={}（0 = shell 未给出理由）",
                    a.tray_err
                )
            },
        ));
        // ---- 系统集成：开机自启 / 全屏检测。这两项不依赖窗口，
        // 放进来是为了「一次 --probe 覆盖整个桌面集成面」。
        {
            // 开机自启：判据不是「开没开」，而是「如果开着，必须指向当前 exe」。
            // exe 被挪窝后陈旧的启动项会指向一个不存在的文件 —— 用户以为
            // 设了自启，开机时静默失败，那比没开更坏。
            rows.push((
                "开机自启：无陈旧项",
                if autostart::is_stale() { R::Fail } else { R::Pass },
                if autostart::enabled() {
                    format!("已启用 · {}", autostart::detail())
                } else if autostart::is_stale() {
                    format!("⚠ 陈旧项：{}", autostart::detail())
                } else {
                    "未启用（托盘菜单可开）".into()
                },
            ));
            // 全屏检测 API：老系统上没这个函数时自动隐藏会静默不工作。
            // 必须真的去解析一次 —— 「代码里调了」不等于系统里有。
            let quns = crate::ffi::user_notification_state();
            rows.push((
                "全屏检测 API",
                if quns.is_some() { R::Pass } else { R::Fail },
                match quns {
                    Some(s) => format!(
                        "SHQueryUserNotificationState 可用 · 当前 QUNS={} ({})",
                        s,
                        quns_label(s)
                    ),
                    None => "shell32 无 SHQueryUserNotificationState（Vista 之前的系统？）".into(),
                },
            ));
        }
        rows.push((
            "毛玻璃 Acrylic",
            if a.accent_ok { R::Pass } else { R::Info },
            if a.accent_ok {
                "DWM Acrylic 已生效".into()
            } else {
                "未生效，已降级为「整窗 alpha + 自绘底板」".to_string()
            },
        ));
        // 把「为什么」也测出来，免得只看到一句「未生效」无从下手
        {
            let mut enabled: windows_sys::Win32::Foundation::BOOL = 0;
            let hr = windows_sys::Win32::Graphics::Dwm::DwmIsCompositionEnabled(&mut enabled);
            rows.push((
                "  └ DWM 桌面合成已启用",
                if hr == 0 && enabled != 0 { R::Pass } else { R::Info },
                format!("hr={:#x} enabled={}", hr, enabled),
            ));
            let ok4 = set_accent(a.panel, ACCENT_ENABLE_ACRYLICBLURBEHIND, 200);
            let ok3 = set_accent(a.panel, ACCENT_ENABLE_BLURBEHIND, 200);
            rows.push((
                "  └ 未文档化接口解析",
                if crate::ffi::accent_api_available() {
                    R::Pass
                } else {
                    R::Info
                },
                format!(
                    "SetWindowCompositionAttribute {}",
                    if crate::ffi::accent_api_available() {
                        "已从 user32.dll 取得"
                    } else {
                        "取不到（系统过旧）"
                    }
                ),
            ));
            rows.push((
                "  └ Accent 接口返回",
                if ok4 || ok3 { R::Pass } else { R::Info },
                format!("Acrylic(4)={} BlurBehind(3)={}", ok4, ok3),
            ));
        }
        rows.push((
            "配置已落盘",
            if Config::path().exists() {
                R::Pass
            } else {
                R::Fail
            },
            Config::path().display().to_string(),
        ));

        // ---- 布局模式：真的切过去，再读回样式位看穿透有没有被摘掉。
        // 「位置可拖动」的前提就是这一位。
        {
            let was = a.cfg.layout_mode;
            a.cfg.layout_mode = true;
            apply_mode(a);
            let ex_now = GetWindowLongPtrW(a.panel, GWL_EXSTYLE) as u32;
            rows.push((
                "布局模式 → 面板可点击（拖动的前提）",
                if !bit(ex_now, WS_EX_TRANSPARENT) {
                    R::Pass
                } else {
                    R::Fail
                },
                "WS_EX_TRANSPARENT 已摘除".into(),
            ));
            // 时间日期块的位置矩形必须在窗口内，否则命中判定会落空
            let inside = a.clock_rect.right > a.clock_rect.left
                && a.clock_rect.bottom > a.clock_rect.top
                && a.clock_rect.left >= 0
                && a.clock_rect.top >= 0
                && a.clock_rect.right <= a.cfg.w
                && a.clock_rect.bottom <= a.cfg.h;
            rows.push((
                "时间日期块可命中区域",
                if inside { R::Pass } else { R::Fail },
                format!(
                    "[{},{}]-[{},{}] in {}x{}",
                    a.clock_rect.left,
                    a.clock_rect.top,
                    a.clock_rect.right,
                    a.clock_rect.bottom,
                    a.cfg.w,
                    a.cfg.h
                ),
            ));
            // 块中心点应当判为命中，窗口外一点应当判为不命中
            let mid = (
                a.clock_rect.left + (a.clock_rect.right - a.clock_rect.left) / 2,
                a.clock_rect.top + (a.clock_rect.bottom - a.clock_rect.top) / 2,
            );
            let hit_mid = hit_clock_client(a, mid.0, mid.1);
            let hit_out = hit_clock_client(a, -500, -500);
            rows.push((
                "命中判定正确（块内命中 / 块外不命中）",
                if hit_mid && !hit_out { R::Pass } else { R::Fail },
                format!("块内={} 块外={}", hit_mid, hit_out),
            ));
            a.cfg.layout_mode = was;
            apply_mode(a);
        }

        // ---- 空转判定：没必要出帧就不能出帧
        //
        // 这一条防的是「静默变费电」：一帧 800µs，25fps 就是每秒 2% 的 CPU
        // 白烧在没有变化的画面上，而且每次空转都在跟拖动抢时间片。
        {
            let keep = (
                a.cfg.anim,
                a.cfg.show_clock,
                a.cfg.show_date,
                a.cfg.show_quote,
                a.cfg.seconds,
            );
            // ① 静态 + 时间日期都关掉：画面从这一秒到下一秒完全一样
            a.cfg.anim = Anim::Static;
            a.cfg.show_clock = false;
            a.cfg.show_date = false;
            let o = a.text_alpha();
            build_frame(a, o);
            let idle_ok = !needs_frame(a);
            // ② 显示秒的时钟：刻度走一格就必须出帧。
            //    用「把上次记录的刻度改掉」来模拟时间前进一秒，不必真等一秒。
            a.cfg.show_clock = true;
            a.cfg.seconds = true;
            let o = a.text_alpha();
            build_frame(a, o);
            let fresh_ok = !needs_frame(a);
            a.clock_stamp = a.clock_stamp.wrapping_sub(1);
            let tick_ok = needs_frame(a);
            // ③ 跑马灯在跑：每一拍都得画
            a.cfg.anim = Anim::Marquee;
            a.cfg.show_quote = true;
            let anim_ok = needs_frame(a);
            (
                a.cfg.anim,
                a.cfg.show_clock,
                a.cfg.show_date,
                a.cfg.show_quote,
                a.cfg.seconds,
            ) = keep;
            rows.push((
                "空转判定（静止不出帧 / 刚出完不出 / 刻度变必出 / 有动效必出）",
                if idle_ok && fresh_ok && tick_ok && anim_ok {
                    R::Pass
                } else {
                    R::Fail
                },
                format!(
                    "静止={} 刚出完={} 刻度变={} 有动效={}",
                    idle_ok, fresh_ok, tick_ok, anim_ok
                ),
            ));
        }

        // ---- 多显示器基准（P1）
        //
        // 判据是「夹取所用屏 == 挂件所在屏」。**单屏机上这条恒真** ——
        // 挂件只在主屏上时，按位置取的屏就是主屏，与旧的 `SPI_GETWORKAREA`
        // 行为完全一致，所以它证明的是「新代码路径活着且自洽」，不是
        // 「多屏下一定对」。真正会红的场景是接上副屏之后：把挂件拖到副屏，
        // 若基准仍取自主屏，`clamp_keep_visible` 会把副屏上的窗口夹到主屏的
        // 坐标范围内，视觉上就是「窗口自己跳走了」。**这条要接副屏才是真验。**
        {
            let cx = a.cfg.x + a.cfg.w / 2;
            let cy = a.cfg.y + 8;
            let hmon = MonitorFromPoint(POINT { x: cx, y: cy }, MONITOR_DEFAULTTONEAREST);
            let mut mi: MONITORINFO = zeroed();
            mi.cbSize = size_of::<MONITORINFO>() as u32;
            let (ok, detail) = if !hmon.is_null() && GetMonitorInfoW(hmon, &mut mi) != 0 {
                let (work, full) = work_area_at(a.cfg.x, a.cfg.y, a.cfg.w);
                let same = rect_eq(&full, &mi.rcMonitor) && rect_eq(&work, &mi.rcWork);
                (
                    same,
                    format!(
                        "挂件({},{}) · 屏[{},{}→{},{}] 工作区[{},{}→{},{}] 一致={}",
                        a.cfg.x,
                        a.cfg.y,
                        mi.rcMonitor.left,
                        mi.rcMonitor.top,
                        mi.rcMonitor.right,
                        mi.rcMonitor.bottom,
                        work.left,
                        work.top,
                        work.right,
                        work.bottom,
                        same
                    ),
                )
            } else {
                (false, "MonitorFromPoint / GetMonitorInfoW 失败".to_string())
            };
            rows.push((
                "多显示器：夹取基准 = 挂件所在屏",
                if ok { R::Pass } else { R::Fail },
                detail,
            ));
        }

        // ---- 外观配置（信息行：这三项都是可以改的）
        rows.push((
            "外观：字体 / 字号 / 颜色",
            R::Info,
            format!(
                "{} · 整体{}% 时钟{}% 格言{}% · 主色{} 日期{}",
                a.cfg.font,
                a.cfg.scale,
                a.cfg.clock_scale,
                a.cfg.quote_scale,
                fmt_color(a.cfg.color),
                fmt_color(a.cfg.date_color)
            ),
        ));
        {
            let face = String::from_utf16_lossy(
                &a.fonts.face[..a.fonts.face.len().saturating_sub(1)],
            );
            // 逐个点名，不数常量：漏了哪一档就直接报出来。
            // `quote_fit` 两档和 `quote_rot` 是竖版与缩字回退的依赖 ——
            // 它们建失败会返回 NULL，而 GDI 用 NULL 字体是**静默**画回默认字体的
            // （不报错、不返回失败），所以只有在这里点得到名才守得住。
            let hs = [
                ("时钟", a.fonts.clock),
                ("小号时钟", a.fonts.clock_small),
                ("日期", a.fonts.date),
                ("格言", a.fonts.quote),
                ("提示", a.fonts.hint),
                ("格言85%", a.fonts.quote_fit[0]),
                ("格言70%", a.fonts.quote_fit[1]),
                ("竖排旋转", a.fonts.quote_rot),
            ];
            let missing: Vec<&str> = hs
                .iter()
                .filter(|(_, h)| h.is_null())
                .map(|(n, _)| *n)
                .collect();
            let ok = missing.is_empty();
            rows.push((
                "  └ 字体对象已按配置重建",
                if ok { R::Pass } else { R::Fail },
                if ok {
                    format!("字面「{}」· {} 个字体句柄全部有效", face, hs.len())
                } else {
                    format!("字面「{}」· 创建失败：{}", face, missing.join(" / "))
                },
            ));
        }

        // ---- 设置面板：真的能开、列表真的填好、关掉后句柄真的清掉
        //
        // 「代码里调了 open()」不算数 —— 枚举回调签名写错、控件样式不对、
        // 忘了 LB_ADDSTRING，都会让窗口照常弹出却一行字体都没有
        // （用户只会觉得「这个界面是坏的」）。所以三件事都读回来：
        // 窗口句柄非空、列表项数 > 0、关掉之后 is_open 变回 false。
        //
        // 再加一条：**读回真实 GDI 计数，开关两轮**。这个窗口为列表里每个字体名
        // 建了一个字体句柄（本机 67 个）+ 界面字体 + 预览字体 + 两个色刷，
        // `free_resources` 一旦漏掉一件，每开关一次就漏掉一件 —— 而 `--memtest`
        // 根本不碰配置窗口，抓不到它。
        //
        // 判据是「**第二轮 == 第一轮**」而不是「回到起点」：首轮实测 +5（22→27），
        // 而第二、三轮恒定不动 —— 那 5 个是首次创建主题化控件时 uxtheme/comctl32
        // 按**进程**缓存的一次性系统对象，不是我们建的、也不随开关累积。
        // 和 `--memtest` 先跑 5 轮预热是同一个道理：一次性成本 ≠ 泄漏。
        // 建 / 毁都是**同步**完成的（`CreateWindowExW` / `DestroyWindow` 内联派发
        // WM_CREATE / WM_DESTROY），所以这里不需要 pump 也能量准。
        {
            let was = a.config.is_open();
            let gdi = |a: &App| -> u32 {
                let _ = a;
                crate::ffi::gui_resources().map(|(g, _)| g).unwrap_or(0)
            };
            let g0 = gdi(a);
            config_ui::open(a);
            let opened = a.config.is_open();
            let n = a.config.list_len();
            config_ui::close(a);
            let closed = !a.config.is_open();
            let g1 = gdi(a);
            // 再来一轮：这一轮必须一点都不涨
            config_ui::open(a);
            config_ui::close(a);
            let g2 = gdi(a);

            let no_leak = g1 == 0 || g2 <= g1;
            rows.push((
                "设置面板：建窗 + 字体列表 + 开关两轮不涨句柄",
                if !was && opened && n > 0 && closed && no_leak {
                    R::Pass
                } else {
                    R::Fail
                },
                format!(
                    "建窗={} · 字体列表 {} 项 · 关闭后句柄已清={} · GDI {}→{}→{}（首轮一次性 +{}，第二轮 Δ{}）",
                    opened,
                    n,
                    closed,
                    g0,
                    g1,
                    g2,
                    g1.saturating_sub(g0),
                    g2 as i64 - g1 as i64
                ),
            ));
        }

        // ---- 连续换字体后，预览区不能出现「叠字」
        //
        // 用户报的现象：在列表里连着点几个字体，预览区的新字和旧字叠在一起。
        // 根因是重画预览区时用了 `InvalidateRect(..., bErase = false)` —— 不擦底，
        // 新字体直接盖在旧字体上，每换一次就多一层。
        //
        // 判据用**像素往返**，不去数代码里有没有写 `FillRect`：先记下「直接设成字体 A」
        // 的样子，再走 A→B→C→A，两张抓图必须逐像素相同。没清底的话，第二张里会叠着
        // B / C 的字形，差异是一大片而不是零。抓不回窗口时标 INFO —— 不假装验过。
        {
            config_ui::open(a);
            let n = a.config.list_len();
            if n >= 3 {
                let (i0, i1, i2) = (0usize, n / 3, (2 * n) / 3);
                let shot = |a: &App| {
                    std::thread::sleep(std::time::Duration::from_millis(60));
                    config_ui::capture_preview(a)
                };
                config_ui::test_select(a, i0);
                pump();
                let s1 = shot(a);
                config_ui::test_select(a, i1);
                pump();
                config_ui::test_select(a, i2);
                pump();
                config_ui::test_select(a, i0);
                pump();
                let s2 = shot(a);

                let (kind, detail) = match (&s1, &s2) {
                    (Some((p1, w, h)), Some((p2, _, _))) if p1.len() == p2.len() => {
                        let diff = p1.iter().zip(p2.iter()).filter(|(x, y)| x != y).count();
                        (
                            if diff == 0 { R::Pass } else { R::Fail },
                            format!("{}x{} 预览区 · A→B→C→A 往返后 {} px 不同（须 0）", w, h, diff),
                        )
                    }
                    _ => (R::Info, "预览区抓取失败（PrintWindow 不可用），未验证".to_string()),
                };
                rows.push(("设置面板·连续换字体后预览区不叠字", kind, detail));
            }
            config_ui::close(a);
        }

        // ---- 设置面板：控件是不是都接上了、点了是不是真的改到了东西
        //
        // 这是整个面板最要紧的一条断言。面板上每一个控件都是「壳」——
        // 真正干活的是 `on_menu()`。壳和里面的线一旦接错（`Ctl.cmd` 填错、
        // id 串了位置、忘了 `sync_panel`），症状是**界面上什么都没发生**，
        // 而代码里每一行都「看起来对」。
        //
        // 三条判据：
        //   ① 面板建出来的可点控件，命令码逐个等于表格里那份清单（建漏 / 建重都红）
        //   ② 每个设置控件点一下，`cfg` 必须变；能配对的反向再点一下必须回到原值
        //      （数值行 `−`/`+`、复选框「再点一次」）——「只生效一次」「点完回不去」
        //      都会在这里露出来
        //   ③ 点完之后面板显示的状态必须和 `cfg` 一致（`widget_mismatch`）
        //      —— 这条抓的是「改了没刷新」：复选框勾着、配置其实是关的
        //
        // 动作类控件（8 个）刻意不自动点：`MI_AUTOSTART` 会**写注册表**
        // （自动点它 = 偷偷改用户的开机项），`MI_FAV` 写收藏文件（由 `--favtest` 专门测），
        // 其余几个各有专门的用例。它们进不了断言也不能是「没人知道」——
        // 所以把跳过的条数报出来，和表格里那份清单对数。
        {
            let keep = a.cfg.clone();
            config_ui::open(a);

            let live = config_ui::interactive_cmds(a);
            let plan = config_ui::table_cmds();
            let first_diff = live
                .iter()
                .zip(plan.iter())
                .position(|(x, y)| x != y)
                .map(|i| i + 1)
                .unwrap_or(if live.len() == plan.len() { 0 } else { live.len().min(plan.len()) + 1 });
            rows.push((
                "设置面板·控件与表格一一对应",
                if live == plan { R::Pass } else { R::Fail },
                if live == plan {
                    format!(
                        "{} 个可点控件，命令码与表格完全一致（设置区共 {} 个控件）",
                        live.len(),
                        config_ui::ctl_count(a)
                    )
                } else {
                    format!(
                        "面板 {} 个 / 表格 {} 条 · 第 {} 个起不同",
                        live.len(),
                        plan.len(),
                        first_diff
                    )
                },
            ));

            // 建完就该「显示 == cfg」；这是底，后面的循环再逐次复查
            let mis0 = config_ui::widget_mismatch(a);
            rows.push((
                "设置面板·建好后显示与配置一致",
                if mis0.is_none() { R::Pass } else { R::Fail },
                mis0.clone().unwrap_or_else(|| "所有控件与 cfg 一致".to_string()),
            ));

            const ACTION: [u32; 8] = [
                MI_AUTOSTART,
                MI_FAV,
                MI_PREV,
                MI_NEXT,
                MI_CLOCK_RESET,
                MI_RELOAD,
                MI_RESET,
                MI_TOGGLE,
            ];

            let mut tested = 0usize;
            let mut skipped = 0usize;
            let mut paired = 0usize;
            let mut fails: Vec<String> = Vec::new();
            for cmd in &live {
                if ACTION.contains(cmd) {
                    skipped += 1;
                    continue;
                }
                let label = config_ui::cmd_label(*cmd);
                if config_ui::is_num_down(a, *cmd) {
                    // 数值行的「−」由同一行的「+」成对覆盖（见下），不单独点：
                    // 基线正好在下界上的项（格言字距 0）点「−」合法地什么都不改，
                    // 单独点它就没法区分「没接上」和「本来就在边界上」。
                    paired += 1;
                    continue;
                }
                let before = cfg_fingerprint(a);
                if !config_ui::test_click_cmd(a, *cmd) {
                    fails.push(format!("{}（#{}）找不到控件", label, cmd));
                    continue;
                }
                tested += 1;
                let mid = cfg_fingerprint(a);
                if config_ui::is_radio(a, *cmd) {
                    // 单选项：点它 = 「我要这一项」。本来就选着的话再点一次不算变化，
                    // 所以判据是「点完它必须是选中的那一个」，而不是「cfg 变了」；
                    // 也**不做往返**—— 单选项不是开关，再点一次不会回到上一个选项
                    // （那是另一个单选项的事，整轮结束后统一还原）。
                    if !config_ui::radio_selected(a, *cmd) {
                        fails.push(format!("{}（#{}）点了但没选中", label, cmd));
                    }
                } else {
                    if mid == before {
                        fails.push(format!("{}（#{}）点了 cfg 没变", label, cmd));
                    }
                    // 往返：数值行走反方向那条命令，复选框再点一次
                    if let Some(inv) = config_ui::inverse_cmd(a, *cmd) {
                        config_ui::test_click_cmd(a, inv);
                        if cfg_fingerprint(a) != before {
                            fails.push(format!(
                                "{}（#{}）再点一次（#{}）没回到原值",
                                label, cmd, inv
                            ));
                        }
                    }
                }
                if let Some(m) = config_ui::widget_mismatch(a) {
                    fails.push(format!("{}（#{}）点完显示不一致：{}", label, cmd, m));
                }
            }
            rows.push((
                "设置面板·每个控件都改得动、显示跟得上",
                if fails.is_empty() { R::Pass } else { R::Fail },
                if fails.is_empty() {
                    format!(
                        "点过 {} 个设置控件（cfg 有变化 + 往返还原 + 显示与 cfg 一致）；\
                         另 {} 个「−」由成对点击覆盖 · 跳过 {} 个动作项",
                        tested, paired, skipped
                    )
                } else {
                    format!("{} 项失败：{}", fails.len(), fails.join(" · "))
                },
            ));

            // ---- 色块必须真的跟着换色（像素级）
            //
            // 上面那些判据都在读 `cfg` / 控件状态，读不到「画出来的那个方块」。
            // 换色是循环的（7 个预设），所以判据是**往返**：点一下色块像素必须变，
            // 点满一圈回到同一个颜色后像素必须一模一样。不比对具体颜色值 ——
            // 那会把「PrintWindow 的通道顺序」这个无关事实写进断言里。
            if let Some(sw) = config_ui::swatch_rect(a, MI_COLOR) {
                let cx = sw.left + (sw.right - sw.left) / 2;
                let cy = sw.top + (sw.bottom - sw.top) / 2;
                let px = |a: &App| -> Option<u32> {
                    // `sync_panel` 只是把整窗标脏，WM_PAINT 要回到消息循环里才轮到它
                    // （WM_PAINT 是最低优先级的消息）。不 pump 就抓，读到的是**上一帧**
                    // 的合成结果 —— 那样这条断言永远绿，等于没测。
                    pump();
                    std::thread::sleep(std::time::Duration::from_millis(60));
                    let (p, w, _) = config_ui::capture_client(a)?;
                    let h = p.len() as i32 / w.max(1);
                    if cx < 0 || cy < 0 || cx >= w || cy >= h {
                        return None;
                    }
                    Some(p[(cy * w + cx) as usize])
                };
                let one = px(a);
                config_ui::test_click_cmd(a, MI_COLOR);
                let two = px(a);
                // 从「换过一次」这个状态再点满一圈（预设是个环），应当回到同一色
                for _ in 0..config::COLOR_PRESETS.len() {
                    config_ui::test_click_cmd(a, MI_COLOR);
                }
                let three = px(a);
                let (kind, detail) = match (one, two, three) {
                    (Some(p1), Some(p2), Some(p3)) => (
                        if p1 != p2 && p2 == p3 { R::Pass } else { R::Fail },
                        format!(
                            "色块中心像素：换色前 {:08X} → 换后 {:08X} → 绕一圈回来 {:08X}（须先变、再一致）",
                            p1, p2, p3
                        ),
                    ),
                    _ => (R::Info, "色块读不到（PrintWindow 不可用），未验证".to_string()),
                };
                rows.push(("设置面板·换色后色块真的重画", kind, detail));
            }

            // ---- 预览框是视口、不是漏勺：样例文字必须真的量得下
            //
            // 用户报过的现象之一就是预览区的字压到了右边的设置区上。
            // 修法是两层：句子改短（看得见的部分）+ `ETO_CLIPPED` 裁到框内（兜底）。
            // 这条断言守的是第一层 —— 只留裁剪不留断言的话，句子写长了画面上
            // 就是「字被切了一半」，而没有任何东西会报警。
            match config_ui::preview_overflow(a) {
                None => rows.push((
                    "设置面板·预览样例不超出预览框",
                    R::Pass,
                    "三行样例在当前字体下都装得下".to_string(),
                )),
                Some(m) => rows.push(("设置面板·预览样例不超出预览框", R::Fail, m)),
            }

            // 抓一张面板整图：`DG_CAPTURE_PANEL=<png 路径> desk-geyan --probe`
            //
            // README 里那张设置面板的截图就是这么来的 —— 不是手动截屏，而是
            // **可复现的产物**：改一行布局重新跑一次，新图自动出来，不会出现
            // 「文档里的图和三年前的代码对不上」这种事。
            if let Ok(path) = std::env::var("DG_CAPTURE_PANEL") {
                // 截图前把字体设成**产品默认那个**（`FONT_PRESETS[0]`）：
                // 上面几轮断言刚把字体换来换去（换字体正是「预览区不叠字」那条
                // 断言的靶子），不还原的话文档里那张图的字体是「测试顺手留下的」，
                // 看起来像产品默认字体是别的。这里刻意不用 `keep.font` ——
                // `keep` 是在这几轮之后才快照的。
                a.set_font(config::FONT_PRESETS[0]);
                config_ui::resync(a);
                // `PrintWindow` 读的是窗口**已经合成好的表面**，它不会替我们重走一遍
                // WM_PAINT —— `resync` 只是把窗口标脏。所以先让队列里的绘制落地
                // 再抓，否则抓到的还是改动之前那一帧（这条坑见 README 踩坑第 15 条）。
                pump();
                std::thread::sleep(std::time::Duration::from_millis(120));
                pump();
                let (kind, detail) = match config_ui::capture_client(a) {
                    Some((px, w, h)) if px.len() >= (w * h) as usize => {
                        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
                        for y in 0..h {
                            for x in 0..w {
                                let p = px[(y * w + x) as usize];
                                // DIB 是 BGRA（见 `gfx::CH_*` 那段），PNG 要 RGBA
                                rgba.extend_from_slice(&[
                                    ((p >> 16) & 0xFF) as u8,
                                    ((p >> 8) & 0xFF) as u8,
                                    (p & 0xFF) as u8,
                                    255,
                                ]);
                            }
                        }
                        match crate::png::write_png(&path, w as u32, h as u32, &rgba) {
                            Ok(()) => (R::Info, format!("已写入 {}（{}x{}）", path, w, h)),
                            Err(e) => (R::Fail, format!("写文件失败：{}", e)),
                        }
                    }
                    _ => (R::Info, "PrintWindow 不可用，未抓取".to_string()),
                };
                rows.push(("设置面板截图", kind, detail));
            }

            config_ui::close(a);
            // 把这一轮点出来的状态全部还回去 —— 断言不能改变被测对象
            a.cfg = keep;
            a.rebuild_fonts();
            apply_resize(a, a.cfg.x, a.cfg.y, a.cfg.w, a.cfg.h);
            apply_mode(a);
            render_and_present(a);
        }

        let mut pass = true;
        for (name, kind, detail) in &rows {
            let tag = match kind {
                R::Pass => "PASS",
                R::Fail => {
                    pass = false;
                    "FAIL"
                }
                R::Info => "INFO",
            };
            println!("[{}] {:<32} {}", tag, name, detail);
        }
        println!(
            "\n{}\n提示：本模式建完窗口即退出，不常驻。",
            if pass {
                "窗口行为全部符合预期"
            } else {
                "存在不符合预期项"
            }
        );

        // 收拾干净再走
        del_tray(a);
        unregister_hotkeys(a);
        DestroyWindow(a.text);
        DestroyWindow(a.panel);
        if pass {
            0
        } else {
            3
        }
    }
}

// ---------------------------------------------------------------- 单帧导出

/// `desk-geyan --dump <png> [key=value ...]`
///
/// 把当前配置下的一帧渲染出来，叠加在棋盘格上存成 PNG。
/// 用途有两个：
///   * 调颜色 / 字号 / 时间位置时，不用盯着跑马灯等它滚过来，直接看图；
///   * 渲染结果可以有客观留底，而不是只靠自检里的数字。
///
/// `key=value` 与配置文件同源（都走 `config::apply_kv`），
/// 所以 `--dump a.png color=#F5D98B clock_dx=200 scale=140` 就是「临时试一下这套外观」。
pub fn dump(path: &str, overrides: &[(String, String)]) -> i32 {
    let Some(mut a) = App::new() else {
        println!("FAIL  初始化失败");
        return 3;
    };
    for (k, v) in overrides {
        config::apply_kv(&mut a.cfg, k, v);
    }
    a.list = quotes::library();
    a.qi = 0;
    a.qn = 1 % a.list.len();
    // 重建字体会把跑马灯停到带内，导出的就是「格言完整可见」的一屏
    a.rebuild_fonts();
    // 过渡状态机显式落定：dump 的职责是「这个配置的**稳态**一帧」，
    // 不能跟着任何相位漂。`App::new` 出来本就是 Dwell，这里是防未来改动
    // 把「跑过半段」的状态带进 dump —— 像素级 A/B 的确定性全押在这。
    a.trans.settle();
    a.phase = 0.0;

    let o = a.text_alpha();
    build_frame(&mut a, o);

    let (w, h) = (a.frame.w, a.frame.h);
    let px = a.frame.out_pixels();
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let p = px[(y * w + x) as usize];
            let al = (p >> 24) & 0xFF;
            let (r, g, b) = ((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF);
            // 16px 棋盘格：透明区域一眼可辨，抗锯齿边缘看得清
            let bg = if ((x / 16) + (y / 16)) % 2 == 0 { 0xF2u32 } else { 0x9Eu32 };
            // 预乘色直接加背景的 (1-α) 就是合成结果，不需要反预乘
            let mix = |c: u32| -> u8 { (c + bg * (255 - al) / 255).min(255) as u8 };
            rgba.extend_from_slice(&[mix(r), mix(g), mix(b), 255]);
        }
    }

    match crate::png::write_png(path, w as u32, h as u32, &rgba) {
        Ok(()) => {
            println!(
                "已写入 {}  ({}x{}，叠加于棋盘格之上)\n外观：{} {}% · 时钟{}% 格言{}% · 主色{} 日期{} 格言{} · 时间位置 +{}/+{}\n版式：{} · 字距 {} · 动效 {}",
                path,
                w,
                h,
                a.cfg.font,
                a.cfg.scale,
                a.cfg.clock_scale,
                a.cfg.quote_scale,
                fmt_color(a.cfg.color),
                fmt_color(a.cfg.date_color),
                config::quote_color_label(a.cfg.quote_color),
                a.cfg.clock_dx,
                a.cfg.clock_dy,
                a.cfg.orient.label(),
                a.cfg.letter_spacing,
                a.cfg.eff_anim().label(),
            );
            0
        }
        Err(e) => {
            println!("FAIL  写文件失败: {}", e);
            3
        }
    }
}

// ---------------------------------------------------------------- 拖动链路端到端

/// 抽干消息队列。`--dragtest` 发的鼠标消息必须真的被派发到 wndproc，
/// 而不是停在队列里当作「测试通过」。
///
/// 这条和 `bench` 模块里那条是同一个函数 —— 测试夹具要发的消息就是它，
/// 没道理让 wndproc 里的调试计数另起一套。经 `use crate::bench::*` 引入。
pub(crate) use crate::bench::{
    mouse_moves_at, press, pump, pump_wait, send_mouse, with_config_guard,
};

// ---------------------------------------------------------------- 穿透行为实测

/// `desk-geyan --clicktest`：用**真实输入**验证「鼠标真能穿过挂件点到下面的窗口」。
///
/// 这是本项目唯一一条**行为级**的穿透断言，因为样式级的断言证明不了它：
/// `WS_EX_TRANSPARENT` 在**非分层**顶层窗口上不生效 —— 底板正是带着这个属性
/// 却把整块面积的点击全吃掉了，而 `--probe` 只读属性、于是全程 PASS。
/// （这条 bug 真发生过：纯文字模式下底板被无条件显示出来，用户看不见任何东西
/// 却点不动底下的窗口。）
///
/// 判据两条方向相反，缺一不可：
///   * **非布局模式** → 我们自己的窗口必须**一条鼠标移动都收不到**（该落到底下）
///   * **布局模式**   → 必须**收得到**（这时底板就是拖拽把手，收不到就拖不动）
///
/// 副作用：把光标挪到挂件上再挪回来（约 1 秒），**不发送任何点击**。
pub fn clicktest() -> i32 {
    with_config_guard(clicktest_inner)
}

fn clicktest_inner() -> i32 {
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }
        let Some(mut st) = App::new() else {
            println!("FAIL  初始化失败");
            return 3;
        };
        st.hinst = GetModuleHandleW(null());
        // 固定到「纯文字 + 非布局」：这是默认使用形态，也正是坏掉过的那一种。
        // 位置另选一处必在屏内的空地 —— 行为判据要靠真实光标，必须点得到。
        st.cfg = Config::for_test();
        // 字体必须跟着基线配置重建 —— 理由见 `Config::for_test()` 的文档
        // （不重建的话，用户那个字体决定时钟块多高，进而决定命中判定认不认得出「整窗拖动」）
        st.rebuild_fonts();
        st.cfg.mode = Mode::TextOnly;
        st.cfg.layout_mode = false;
        st.cfg.visible = true;
        st.cfg.x = 300;
        st.cfg.y = 300;
        st.cfg.w = 420;
        st.cfg.h = 140;
        let boxed = Box::new(st);
        app_ptr_set(Box::into_raw(boxed));
        let a = app();

        if !bootstrap(a) {
            println!("FAIL  窗口创建失败");
            return 2;
        }

        let mut rows: Vec<(&str, R, String)> = Vec::new();
        let (cx, cy) = (a.cfg.x + a.cfg.w / 2, a.cfg.y + a.cfg.h / 2);

        // ---- ⓪ 建窗后**立刻**看：纯文字模式下底板不许越权显形
        //
        // 必须在任何 `apply_mode` 之前读 —— 那正是这个 bug 的形态：
        // `bootstrap` 在 `apply_mode` 之后又无条件把底板显示了出来，
        // 而它是一片看不见却拦鼠标的窗口（topmost + 非 layered）。
        let boot_panel = IsWindowVisible(a.panel) != 0;
        let boot_text = IsWindowVisible(a.text) != 0;
        rows.push((
            "建窗后底板未越权显形",
            if boot_panel { R::Fail } else { R::Pass },
            format!(
                "纯文字模式：底板 visible={}（须 0）/ 文字 visible={}",
                boot_panel as u8, boot_text as u8
            ),
        ));

        // ---- ① 非布局模式：底板不该显形，且必须完全穿透
        a.cfg.layout_mode = false;
        apply_mode(a);
        pump_wait(60);
        let panel_shown = IsWindowVisible(a.panel) != 0;
        let text_shown = IsWindowVisible(a.text) != 0;
        let hits_through = mouse_moves_at(cx, cy);

        rows.push((
            "纯文字模式下底板已隐藏",
            if panel_shown { R::Fail } else { R::Pass },
            format!(
                "底板 visible={} / 文字 visible={}",
                panel_shown as u8, text_shown as u8
            ),
        ));
        rows.push((
            "非布局模式：点得到下面的窗口",
            if hits_through == 0 { R::Pass } else { R::Fail },
            format!("光标停在挂件正中，我们的窗口收到 {hits_through} 条移动（须 0）"),
        ));

        // ---- ② 布局模式：必须收得到，否则看不见也摸不着
        a.cfg.layout_mode = true;
        apply_mode(a);
        pump_wait(60);
        let hits_layout = mouse_moves_at(cx, cy);
        rows.push((
            "布局模式：鼠标能落在挂件自己身上",
            if hits_layout > 0 { R::Pass } else { R::Fail },
            format!("同一位置收到 {hits_layout} 条移动（须 >0）"),
        ));

        // ---- ③ 切回非布局：穿透必须立刻恢复（而不是一次性的）
        a.cfg.layout_mode = false;
        apply_mode(a);
        pump_wait(60);
        let hits_back = mouse_moves_at(cx, cy);
        rows.push((
            "切回非布局后立刻恢复穿透",
            if hits_back == 0 { R::Pass } else { R::Fail },
            format!("收到 {hits_back} 条移动（须 0）"),
        ));

        // ---- ④ **毛玻璃模式**下也必须穿透（这一条推翻了项目早期的错误结论）
        //
        // 早期 README 写着一条「硬约束」：毛玻璃要求底板非分层，而跨进程穿透要求
        // 分层，所以 `mode=glass` 时底板**必然**拦住那一块面积的鼠标，
        // 「已实测可行」的解法是把底板挂到分层宿主下做子窗口。
        //
        // 两句话都是错的：
        // * 把 `mode` 直接改成 `Glass` 跑本条断言，顶层非分层底板（Acrylic 正常
        //   生效）下**收到 0 条鼠标移动** —— 毛玻璃与穿透本来就可以同时成立。
        //   当初之所以看到「拦住鼠标」，真正原因是 `bootstrap` 在 `apply_mode`
        //   之后又无条件 `ShowWindow(panel)`，让本该隐藏的底板显了形
        //   （见 README「踩过的坑」第 7 条），归因却归到了架构约束上。
        // * 而「挂到宿主下做子窗口」这条路真的走不通：`SetWindowCompositionAttribute`
        //   在子窗口上直接返回 false（实测 `Acrylic(4)=false BlurBehind(3)=false`，
        //   与宿主是否分层、是否设 LWA 无关），毛玻璃会静默降级。
        //
        // 判据必须是**行为**（数我们自己的窗口收到几条鼠标移动），不能是样式位。
        a.cfg.mode = Mode::Glass;
        a.cfg.layout_mode = false;
        apply_mode(a);
        pump_wait(80);
        let glass_panel = IsWindowVisible(a.panel) != 0;
        let glass_panel_transparent =
            (GetWindowLongPtrW(a.panel, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT) != 0;
        let hits_glass = mouse_moves_at(cx, cy);
        rows.push((
            "毛玻璃模式：点得到下面的窗口",
            if hits_glass == 0 && glass_panel { R::Pass } else { R::Fail },
            format!(
                "底板 visible={}（毛玻璃显形须 1）· 底板 TRANSPARENT={} · 收到 {} 条移动（须 0）",
                glass_panel as u8, glass_panel_transparent, hits_glass
            ),
        ));

        // ---- ⑤ 毛玻璃 + 布局模式：要能拖，退回后要恢复穿透
        a.cfg.layout_mode = true;
        apply_mode(a);
        pump_wait(80);
        let hits_glass_layout = mouse_moves_at(cx, cy);
        rows.push((
            "毛玻璃 + 布局：拖得动",
            if hits_glass_layout > 0 { R::Pass } else { R::Fail },
            format!("同一位置收到 {} 条移动（须 >0）", hits_glass_layout),
        ));
        a.cfg.layout_mode = false;
        a.cfg.mode = Mode::TextOnly;
        apply_mode(a);
        pump_wait(80);
        let hits_glass_back = mouse_moves_at(cx, cy);
        rows.push((
            "毛玻璃退回纯文字：穿透恢复",
            if hits_glass_back == 0 { R::Pass } else { R::Fail },
            format!("收到 {} 条移动（须 0）", hits_glass_back),
        ));

        let mut pass = true;
        for (name, kind, detail) in &rows {
            let tag = match kind {
                R::Pass => "PASS",
                R::Fail => {
                    pass = false;
                    "FAIL"
                }
                R::Info => "INFO",
            };
            println!("[{}] {:<32} {}", tag, name, detail);
        }
        println!(
            "\n{}\n（本模式会临时挪动光标并复原，不发送任何点击。）",
            if pass {
                "鼠标穿透行为全部符合预期"
            } else {
                "存在不符合预期项"
            }
        );

        del_tray(a);
        unregister_hotkeys(a);
        DestroyWindow(a.text);
        DestroyWindow(a.panel);
        if pass {
            0
        } else {
            3
        }
    }
}

/// 用**真实键盘输入**按一次 `Ctrl+Alt+F`。
///
/// 和 `--clicktest` 的鼠标注入同一个道理：`PostMessage(WM_HOTKEY)` 也能让
/// wndproc 跑一遍，但那跳过的是**系统热键分发**这一环 —— 而「注册了热键
/// 却按不出来」恰恰是这个功能最常见的坏法（被别的进程占着、修饰键组合
/// 写错、注册在了一个收不到消息的窗口上）。要验就得走完整链路。
///
/// `SendInput` 是异步的：消息由系统线程投递，所以注入完必须再跑一会儿消息泵。
unsafe fn press_favorite_hotkey() {
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    let keys: [(u16, u32); 6] = [
        (VK_CONTROL as u16, 0),
        (VK_MENU as u16, 0),
        (VK_F as u16, 0),
        (VK_F as u16, KEYEVENTF_KEYUP),
        (VK_MENU as u16, KEYEVENTF_KEYUP),
        (VK_CONTROL as u16, KEYEVENTF_KEYUP),
    ];
    let mut inputs: [INPUT; 6] = [zeroed(); 6];
    for (i, (vk, flags)) in keys.iter().enumerate() {
        inputs[i].r#type = INPUT_KEYBOARD;
        inputs[i].Anonymous.ki.wVk = *vk;
        inputs[i].Anonymous.ki.dwFlags = *flags;
    }
    SendInput(6, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32);
}

/// `desk-geyan --favtest`：**端到端**验证「收藏当前句子」。
///
/// 这是项目里唯一一条真的去按热键的测试。分三件事验，缺一不可：
///
/// 1. **剪贴板**里是当前那一句（写进去 ≠ 写对了，还要读回来逐字比）
/// 2. **收藏夹文件**里多了这一行，而且格式能被解析器读回原样
/// 3. **再按一次不会写第二遍**（幂等）—— 否则同一个组里出现同一内容，
///    轮播会连着播两句一样的
///
/// 副作用与处理：
/// * 剪贴板：**测前读回、测后恢复**（仅文本；原内容非文本时如实报告）
/// * 收藏夹：指向**临时目录**，完全不碰 `%APPDATA%` —— 和
///   `quotes::library_from` 同一个理由（测试不能污染用户的格言库）
/// * 配置：`with_config_guard` 快照写回
/// * 键盘：注入的是 `Ctrl+Alt+F`，**不发送任何字符**，不会打进别的窗口
///
/// **环境前提**：本进程必须真的是 `Ctrl+Alt+F` 的持有者。本机若已跑着另一个
/// 挂件实例，热键会被它占着（`RegisterHotKey` 报 1409），注入的按键归它处理，
/// 这边什么都测不到 —— 此时四条依赖热键的断言如实标 `INFO`（不是 `FAIL`），
/// 并在文案里说明原因。想跑出真正的 `PASS`，先退出正在运行的挂件实例。
pub fn favtest() -> i32 {
    with_config_guard(favtest_inner)
}

fn favtest_inner() -> i32 {
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }
        let Some(mut st) = App::new() else {
            println!("FAIL  初始化失败");
            return 3;
        };
        st.hinst = GetModuleHandleW(null());
        // 用**确定的**测试配置，不受用户当前配置影响（`Config::for_test` 的理由）
        st.cfg = Config::for_test();
        // 字体必须跟着基线配置重建 —— 理由见 `Config::for_test()` 的文档
        // （不重建的话，用户那个字体决定时钟块多高，进而决定命中判定认不认得出「整窗拖动」）
        st.rebuild_fonts();
        st.cfg.x = 300;
        st.cfg.y = 300;
        let boxed = Box::new(st);
        app_ptr_set(Box::into_raw(boxed));
        let a = app();

        if !bootstrap(a) {
            println!("FAIL  窗口创建失败");
            return 2;
        }

        // 这一轮注入的 `Ctrl+Alt+F`，到底会不会被**我们**收到？如果本机已经跑着
        // 另一个挂件实例，`RegisterHotKey` 会失败（系统错误码 1409），那个实例
        // 会把按键收走 —— 我们这边剪贴板、收藏夹都不会动，但这**不是回归**。
        // `--probe` 的「全局热键」一条早就按同一个判据报 INFO 了，这里沿用：
        // 「测不了」和「坏了」必须分开，否则一条环境相关的结果会被读成产品坏了。
        let fav_ours = a.hotkeys.contains(&HK_FAV);
        let not_ours = if fav_ours {
            String::new()
        } else {
            format!(
                "（HK_FAV 不在本进程名下：错误码 {} ⇒ 注入的热键被别人收走，\
                 本轮测不了；先退出正在运行的挂件实例再跑）",
                a.hotkey_err
            )
        };

        // 收藏走临时目录：测试绝不能往用户的格言库里写东西。
        // `favorite()` 内部走 `util::data_dir()`，所以这里用 `DG_DATA_DIR`
        // 把它整体指到临时目录 —— 这也是这个覆盖存在的第一个理由。
        let dir = std::env::temp_dir().join(format!("desk-geyan-favtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("DG_DATA_DIR", &dir);
        // 放一条已知句子进去，让「当前句」是确定的 —— 用户机器上
        // quotes.txt 的第一条是什么，测试不该依赖。
        std::fs::write(
            dir.join("quotes.txt"),
            "农，天下之本，务莫大焉。 —— 《史记》\n",
        )
        .unwrap();

        let mut rows: Vec<(&str, R, String)> = Vec::new();

        // 重载成这个临时库，并把 qi 固定到 0（第一条 = 上面那句）
        a.list = quotes::library_from(&dir);
        a.qi = 0;
        a.qn = 1 % a.list.len();
        a.cur_w = measure_quote(a, &a.list[a.qi]);
        let expect_display = a.list[a.qi].display();
        let expect_line = favorites::line_of(&a.list[a.qi]);

        // 剪贴板备份（仅文本）
        let clip_before = clipboard::get_text(a.panel);

        // ---- ① 真的按一次 Ctrl+Alt+F
        press_favorite_hotkey();
        pump_wait(300);
        let clip_after = clipboard::get_text(a.panel);
        let fav_path = favorites::path(&dir);
        let fav_after = favorites::load(&dir);

        rows.push((
            "热键触发后剪贴板 = 当前句子",
            if !fav_ours {
                R::Info
            } else if clip_after.as_deref() == Some(expect_display.as_str()) {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "期望「{}」· 实得「{}」{}",
                expect_display,
                clip_after.as_deref().unwrap_or("<空/非文本>"),
                not_ours
            ),
        ));
        rows.push((
            "热键触发后收藏夹多了一行",
            if !fav_ours {
                R::Info
            } else if fav_after.len() == 1
                && favorites::line_of(&fav_after[0]) == expect_line
                && fav_path.exists()
            {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "{} → {} 条（须 1）：{}{}",
                fav_path.display(),
                fav_after.len(),
                expect_line,
                not_ours
            ),
        ));

        // ---- ② 再按一次：幂等。这一条防的是「收藏夹里同一个组出现同一内容」，
        // 轮播时会连着播两句一样的。
        press_favorite_hotkey();
        pump_wait(300);
        let fav_twice = favorites::load(&dir);
        rows.push((
            "重复触发不写第二遍（幂等）",
            if !fav_ours {
                R::Info
            } else if fav_twice.len() == 1 {
                R::Pass
            } else {
                R::Fail
            },
            format!("再按一次后 {} 条（须 1）{}", fav_twice.len(), not_ours),
        ));

        // ---- ③ qi 仍指着同一句（画面不许自己跳）
        rows.push((
            "收藏后当前句不变（画面不跳）",
            if a.qi < a.list.len() && a.list[a.qi].display() == expect_display {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "qi={} →「{}」",
                a.qi,
                a.list.get(a.qi).map(|q| q.display()).unwrap_or_default()
            ),
        ));

        // ---- ④ 收藏的这句真的进了轮播候选（= 「加入播放」）
        // 判据：重载后的库里能按文本找到它，且收藏夹作为独立一组存在。
        let list_after = quotes::library_from(&dir);
        let in_rotation = list_after.iter().any(|q| q.display() == expect_display);
        let groups = quotes::load_groups(&dir);
        let has_fav_group = groups.iter().any(|(n, _)| n == favorites::FILE);
        rows.push((
            "收藏后进入轮播候选",
            if !fav_ours {
                R::Info
            } else if in_rotation && has_fav_group {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "库内可找到={} · 独立分组 {}={} · 总库 {} 条{}",
                in_rotation,
                favorites::FILE,
                has_fav_group,
                list_after.len(),
                not_ours
            ),
        ));

        // 剪贴板恢复
        let (restored, note) = match &clip_before {
            Some(s) => (clipboard::copy_text(a.panel, s), "原内容为文本，已原样恢复"),
            None => (true, "原内容非文本或为空，无法恢复（如实报告）"),
        };
        rows.push((
            "剪贴板原内容恢复",
            if restored { R::Pass } else { R::Fail },
            format!("{} · 恢复 {}", note, if restored { "完成" } else { "失败" }),
        ));

        unregister_hotkeys(a);
        DestroyWindow(a.text);
        DestroyWindow(a.panel);
        let _ = std::fs::remove_dir_all(&dir);

        let mut pass = true;
        println!("\n收藏功能端到端实测（真实键盘 Ctrl+Alt+F）");
        for (name, r, detail) in &rows {
            let tag = match r {
                R::Pass => "PASS",
                R::Fail => {
                    pass = false;
                    "FAIL"
                }
                R::Info => "INFO",
            };
            println!("[{}] {:<34} {}", tag, name, detail);
        }
        println!(
            "\n{}\n",
            if pass {
                if fav_ours {
                    "收藏链路全部符合预期"
                } else {
                    "热键不归本进程 ⇒ 依赖热键的条目未实测（退出正在运行的挂件实例可跑出真 PASS）"
                }
            } else {
                "存在不符合预期项"
            }
        );
        if pass {
            0
        } else {
            3
        }
    }
}

/// `desk-geyan --dragtest`：把一次完整的「按下 → 移动 → 松开」投进**真正的
/// 窗口过程**，验证拖动链路端到端成立。
///
/// 走 `PostMessage` 而不是注入系统级鼠标：一是不会打扰用户当前的鼠标操作，
/// 二是消息自带坐标，整条链路是可复现的。这也正是 wndproc 用 lparam 而不是
/// `GetCursorPos()` 的原因 —— 它让一次拖动成为自包含的事件序列。
pub fn dragtest() -> i32 {
    with_config_guard(dragtest_inner)
}

// ---------------------------------------------------------------- 性能标定

/// `desk-geyan --perf`：把「拖动期间一条鼠标消息要花多少微秒」拆开量化。
///
/// 这是「跟不跟手」唯一的硬指标 —— 手感是主观的，队列堆积是客观的：
/// 鼠标每秒上报 N 条移动消息，每条的处理成本必须远小于 1/N 秒，否则消息在
/// 队列里越积越多，窗口只能跳着追赶光标，用户感知到的就是「粘」「拖后腿」。
///
/// 参考线：125Hz 鼠标每条预算 8000µs，1000Hz 鼠标 1000µs。
pub fn perf() -> i32 {
    with_config_guard(perf_inner)
}

fn perf_inner() -> i32 {
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }
        let Some(mut st) = App::new() else {
            println!("FAIL  初始化失败");
            return 3;
        };
        st.hinst = GetModuleHandleW(null());
        st.cfg = Config::for_test();
        // 字体必须跟着基线配置重建 —— 理由见 `Config::for_test()` 的文档
        // （不重建的话，用户那个字体决定时钟块多高，进而决定命中判定认不认得出「整窗拖动」）
        st.rebuild_fonts();
        st.cfg.layout_mode = true;
        let boxed = Box::new(st);
        app_ptr_set(Box::into_raw(boxed));
        let a = app();
        if !bootstrap(a) {
            println!("FAIL  窗口创建失败");
            return 2;
        }
        // 隔离测量：定时器会往消息队列里塞 WM_TIMER，先停掉
        KillTimer(a.text, TIMER_ID);
        render_and_present(a);

        const N: usize = 200;
        let mut rows: Vec<(&'static str, f64, String)> = Vec::new();

        // ---- 成本模型：拆开量，才能看出「哪里是白花的钱」
        let t = std::time::Instant::now();
        for _ in 0..N {
            let o = a.text_alpha();
            build_frame(a, o);
        }
        let t_build = t.elapsed().as_secs_f64() * 1e6 / N as f64;
        let t = std::time::Instant::now();
        for _ in 0..N {
            a.frame.present(a.text, a.cfg.x, a.cfg.y);
        }
        let t_ulw = t.elapsed().as_secs_f64() * 1e6 / N as f64;
        let t = std::time::Instant::now();
        for _ in 0..N {
            render_and_present(a);
        }
        let t_full = t.elapsed().as_secs_f64() * 1e6 / N as f64;

        let move_panel = |n: usize| -> f64 {
            let t = std::time::Instant::now();
            for i in 0..n {
                let x = 60 + (i as i32 % 40);
                SetWindowPos(
                    a.panel,
                    null_mut(),
                    x,
                    60,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                );
            }
            t.elapsed().as_secs_f64() * 1e6 / n as f64
        };
        let t_panel = move_panel(N);

        // ---- 真实消息链路：三种拖动各灌一次，量每条消息的成本。
        // 这是唯一诚实的口径 —— 拖动的手感只取决于「鼠标每秒报多少条，
        // 每条要花多久处理」，跟函数级微基准不是一回事。
        let mid = (a.cfg.w / 2, a.cfg.h / 2);
        let clock_pt = (
            a.clock_rect.left + (a.clock_rect.right - a.clock_rect.left) / 2,
            a.clock_rect.top + (a.clock_rect.bottom - a.clock_rect.top) / 2,
        );
        let right_edge = (a.cfg.w - 2, a.cfg.h / 2);

        let t_move_new = drag_batch(a, mid, N, true, true);
        a.legacy_drag = true;
        let t_move_legacy = drag_batch(a, mid, N, true, true);
        a.legacy_drag = false;
        let t_clock = drag_batch(a, clock_pt, N, true, false);
        let t_resize = drag_batch(a, right_edge, N, true, false);

        // 整窗移动时系统发来的 WM_MOVE：现在只同步几何，不重绘
        let t = std::time::Instant::now();
        for _ in 0..N {
            send_mouse(a.panel, WM_MOVE, 0, 0);
        }
        pump();
        let t_wmmove = t.elapsed().as_secs_f64() * 1e6 / N as f64;

        // ---- 最坏帧：横版多行折行 + 竖版卷轴。
        //
        // 上面的「完整一帧」量的是默认配置下的**一行短句** —— 那是最好情况，
        // 拿它当性能保证等于没量。排版层新引入的成本全在后两种形态里：
        // 一条 60 字长句折成好几行（每行都要单独定位 + 居中），
        // 以及竖版逐字落格 + 落款三列。
        // 计划定的红线是**单帧 ≤ 1200µs** —— 1000Hz 鼠标下每条消息的预算就 1000µs，
        // 拖动期间大概每 8ms 出一帧，超过这条线就会跟拖动抢时间片。
        let base_cfg = a.cfg.clone();
        let base_quote = a.list[a.qi].clone();
        let long = Quote {
            text: "国".repeat(60),
            author: "测试".to_string(),
        };
        let mut worst_us = 0.0f64;
        let mut worst_what = "";
        let mut worst_rows: Vec<(&'static str, f64, String)> = Vec::new();
        for (name, orient, w, h) in [
            ("最坏帧 · 横版多行（60 字折行）", Orient::Horizontal, 620, 260),
            ("最坏帧 · 竖版卷轴（落款 3 列）", Orient::Vertical, 300, 660),
        ] {
            a.cfg.orient = orient;
            a.cfg.w = w;
            a.cfg.h = h;
            a.cfg.anim = Anim::Static; // 去掉动效：量的是排版 + 绘制本身
            a.list[a.qi] = long.clone();
            a.rebuild_fonts();
            // 预热：换尺寸后的第一趟要把帧缓冲重建到新尺寸，那是**一次性**的钱。
            // 不预热的话这一档会系统性偏高 —— 而它恰好是「排版层贵不贵」的唯一数据。
            for _ in 0..20 {
                build_frame(a, 215);
                a.frame.present(a.text, a.cfg.x, a.cfg.y);
            }
            let t = std::time::Instant::now();
            for _ in 0..N {
                build_frame(a, 215);
            }
            let tb = t.elapsed().as_secs_f64() * 1e6 / N as f64;
            let t = std::time::Instant::now();
            for _ in 0..N {
                a.frame.present(a.text, a.cfg.x, a.cfg.y);
            }
            let tu = t.elapsed().as_secs_f64() * 1e6 / N as f64;
            let total = tb + tu;
            if total > worst_us {
                worst_us = total;
                worst_what = name;
            }
            worst_rows.push((
                name,
                total,
                format!("画 {:.0} + 上屏 {:.0} µs · 红线 1200", tb, tu),
            ));
        }
        // PR1.1/12.8 留档：**过渡期（Leave 半程）**的一帧成本。过渡期比稳态
        // 多「阴影趟按 k 写灰度」这一处差别，量级该与稳态同档 —— 只留档，
        // 不设红线（计划 12.8 定的口径）。
        a.cfg.orient = Orient::Vertical;
        a.cfg.w = 300;
        a.cfg.h = 660;
        a.cfg.anim = Anim::Static;
        a.list[a.qi] = long.clone();
        a.rebuild_fonts();
        if !a.trans.reduced() {
            a.trans.start_leave(transition::slide_px(a.cfg.quote_font_px()));
            a.trans.step(transition::LEAVE_S / 2.0); // k≈0.875，阴影灰度在场
            for _ in 0..20 {
                build_frame(a, 215); // 预热，口径与上面两档一致
            }
            let t = std::time::Instant::now();
            for _ in 0..N {
                build_frame(a, 215);
            }
            let t_leave = t.elapsed().as_secs_f64() * 1e6 / N as f64;
            a.trans.settle();
            worst_rows.push((
                "过渡帧 · 竖版 Leave 半程（留档）",
                t_leave,
                "只画不上屏 · 对照稳态同形态读数".to_string(),
            ));
        }
        // 恢复默认配置与帧缓冲，后面的空转测量读的必须是默认形态
        a.cfg = base_cfg;
        a.list[a.qi] = base_quote;
        a.rebuild_fonts();
        render_and_present(a);

        rows.push((
            "渲染一帧（只画，不上屏）",
            t_build,
            format!("{:.0} fps 上限", 1e6 / t_build.max(1.0)),
        ));
        rows.push((
            "上屏一次（UpdateLayeredWindow）",
            t_ulw,
            format!("{:.0} fps 上限", 1e6 / t_ulw.max(1.0)),
        ));
        rows.push((
            "完整一帧（渲染+上屏）",
            t_full,
            format!("{:.0} fps 上限", 1e6 / t_full.max(1.0)),
        ));
        rows.extend(worst_rows);
        rows.push((
            "只挪底板（1×SetWindowPos）",
            t_panel,
            format!("{:.0} 次/秒上限", 1e6 / t_panel.max(1.0)),
        ));
        rows.push((
            "一条 WM_MOVE（只同步几何）",
            t_wmmove,
            format!("{:.0} 次/秒上限", 1e6 / t_wmmove.max(1.0)),
        ));

        println!("拖动性能标定（每项 {} 次平均，1000Hz 鼠标每条预算 1000µs）", N);
        println!("{}", "-".repeat(74));
        for (name, us, note) in &rows {
            println!("  {:<34} {:>8.0} µs   {}", name, us, note);
        }
        println!("{}", "-".repeat(74));
        println!("  拖动：一条鼠标移动消息的成本（**节流关**口径 = 成本上限）");
        let drag_rows: [(&str, f64); 4] = [
            ("整体移动 · 节流关（成本上限）", t_move_new),
            ("整体移动 · 旧做法（每条整帧重绘）", t_move_legacy),
            ("拖时间日期块", t_clock),
            ("缩放窗口", t_resize),
        ];
        for (name, us) in drag_rows {
            let rate = 1e6 / us.max(1.0);
            let verdict = if us < 250.0 {
                "1000Hz 鼠标也跟得上"
            } else if us < 1000.0 {
                "1000Hz 勉强，125Hz 富余"
            } else {
                "会积压 —— 就是「粘手」的来源"
            };
            println!(
                "  {:<34} {:>8.0} µs   {:.0} 条/秒 · {}",
                name, us, rate, verdict
            );
        }
        println!("{}", "-".repeat(74));

        // ---- 空转成本：定时器每拍都跑，但「跑」不等于「画」。
        // 这里用同样的 15 拍（每拍 12ms）对比两种配置的出帧数，
        // 静态那边必须一帧都不出 —— 否则就是白烧 CPU，还会跟拖动抢时间片。
        // 嵌套 fn 不捕获环境，避免闭包可变借用 `a` 之后又去改 `a.cfg`。
        // 循环体就是 `tick` 的身子（固定 dt 版）—— 量的必须是定时器真实那条路径。
        fn beat(a: &mut App, n: usize) -> u32 {
            let r0 = a.renders;
            for _ in 0..n {
                std::thread::sleep(std::time::Duration::from_millis(12));
                step(a, 0.04);
                render_if_due(a, false);
            }
            a.renders - r0
        }

        let idle_frames;
        let anim_frames;
        {
            let keep = (
                a.cfg.anim,
                a.cfg.show_clock,
                a.cfg.show_date,
                a.cfg.show_quote,
            );
            // 静态格言 + 不显示时间日期 = 真实的「挂着不动」场景
            a.cfg.anim = Anim::Static;
            a.cfg.show_clock = false;
            a.cfg.show_date = false;
            a.cfg.show_quote = true;
            a.phase = 0.0;
            // 基线对齐：刚把时钟关掉时 `clock_stamp` 会从「真实刻度」跌回 0，
            // 那一拍确实该重绘一次（时钟要从画面上消失）。这里要量的是**之后**的稳态空转，
            // 所以先把基线压好，别把这一次性的过渡帧算成空转。
            a.clock_stamp = clock_stamp(&a.cfg);
            a.dirty = false;
            idle_frames = beat(a, 15);
            // 跑马灯：每拍内容都在动 —— 帧数应逼近拍数
            a.cfg.anim = Anim::Marquee;
            a.dirty = false;
            anim_frames = beat(a, 15);
            (
                a.cfg.anim,
                a.cfg.show_clock,
                a.cfg.show_date,
                a.cfg.show_quote,
            ) = keep;
        }
        println!(
            "  空转成本（同样 15 拍定时器）  静态静止 {} 帧 · 跑马灯 {} 帧",
            idle_frames, anim_frames
        );
        println!("{}", "-".repeat(74));

        let worst = t_move_new.max(t_clock).max(t_resize);
        let frame_ok = worst_us < 1200.0;
        let ok = worst < 1000.0 && idle_frames == 0 && anim_frames >= 10 && frame_ok;
        println!(
            "两条参考线：125Hz 鼠标 8000µs/条，1000Hz 鼠标 1000µs/条。最慢的一条拖动 = {:.0} µs",
            worst
        );
        println!(
            "单帧红线：1200µs —— 最贵的一帧 = {:.0} µs（{}）{}",
            worst_us,
            worst_what,
            if frame_ok { "· 达标" } else { "· **超红线**" }
        );
        if ok {
            0
        } else {
            3
        }
    }
}

// ---------------------------------------------------------------- 跟手标定
//
// `--perf` 量的是**吞吐**：一条消息摊到多少微秒。跟手是**延迟**，两者不相等 ——
// 上一版就栽在这儿：单条 190µs 看着余量很大（1000Hz 的预算有 1000µs），
// 可那个数是在 0/+1px 的抖动下量的，窗口其实一步都没挪，也没算上一条鼠标移动
// 连带引发的隐藏消息。于是「优化」完了手感照旧。
//
// 这一节换成三个能证伪的问题：
//   ① 让窗口**真的走** —— 800 条消息应当走 800px，位移对不上就是丢移动；
//   ② 一条鼠标移动到底引发几条消息（`SetWindowPos` 会同步回调 `WM_MOVE`）；
//   ③ `SetWindowPos` / 旧 ULW / 关节流 三档对比，改动到底买到了什么。

/// `desk-geyan --dragbench`：拖动跟手的端到端标定。
pub fn dragbench() -> i32 {
    with_config_guard(dragbench_inner)
}

/// 计数槽（`msg & 0xFF`）→ 名字。标定输出要给人读，不认十六进制。
fn msg_name(idx: u32) -> &'static str {
    match idx {
        0x00 => "WM_MOUSEMOVE",
        0x01 => "WM_LBUTTONDOWN",
        0x02 => "WM_LBUTTONUP",
        0x03 => "WM_MOVE",
        0x05 => "WM_SIZE",
        0x0F => "WM_PAINT",
        0x13 => "WM_TIMER",
        0x14 => "WM_ERASEBKGND",
        0x15 => "WM_CAPTURECHANGED",
        0x20 => "WM_SETCURSOR",
        0x47 => "WM_WINDOWPOSCHANGED",
        0x84 => "WM_NCHITTEST",
        _ => "其它",
    }
}

/// 一档拖动标定的结果。
struct DragBench {
    /// 单条移动消息的处理成本（µs）
    us: f64,
    /// 窗口**实际**走过的像素 —— 读内核，不信 `cfg.x`
    moved: i32,
    /// 这一路上 `panel_proc` 收到的消息总条数
    msgs: u32,
    /// 消息分布（按条数降序）
    by_msg: Vec<(u32, u32)>,
    /// 「目标位置 − 窗口实际位置」的峰值（px）
    lag_max: i32,
    /// 同上，均值
    lag_avg: f64,
}

/// 灌一次「按下 → n 条移动 → 松开」，让窗口**真的**一路挪过去。
///
/// 客户区坐标恒定用 `cx + 1`：含义是「光标相对**当前**窗口又前进了 1px」。
/// 处理第 i 条时窗口已经走了 i−1 px，`cfg.x + (cx+1)` 算出来正好是「当前位置 + 1」，
/// 于是窗口每条前进 1px、n 条走 n px。这就是「光标 1000Hz 匀速移动、窗口完全跟手」
/// 那条理想轨迹 —— 位移对不上，就是丢移动，也就是「拖后腿」。
///
/// `paced` = 每条之间等 1000µs，模拟 1000Hz 的真实到达节奏。**节流是按 wall-clock
/// 判定的**，把 800 条一口气灌进去只会被节流吃掉 799 条 —— 那量到的是
/// 「消息一次性到达」这个不存在的场景，数字好看但毫无意义。
unsafe fn bench_drag(a: &mut App, way: MoveWay, throttle: u128, n: usize, paced: bool) -> DragBench {
    // 统一起点：每一档都从 (60,60) 出发，数字才可比
    a.grab = Grab::None;
    a.cfg.x = 60;
    a.cfg.y = 60;
    a.move_way = way;
    a.move_throttle = throttle;
    apply_geometry(a, true, true);
    pump();

    let (cx, cy) = (a.cfg.w / 2, a.cfg.h / 2);
    send_mouse(a.panel, WM_LBUTTONDOWN, cx, cy);
    pump();

    let b = win_rect(a.text);
    let before = b.left;
    // 光标在**屏幕**上的起点 = 窗口位置 + 客户区偏移
    let (csx, csy) = (b.left + cx, b.top + cy);
    tally_reset();

    let mut busy = std::time::Duration::ZERO;
    let mut lag_max = 0i32;
    let mut lag_sum: i64 = 0;
    for i in 0..n {
        // 轨迹要按 `paced` 分开构造，两种模式的「真实」不是一回事：
        //
        // * `paced`（每条之间等 1000µs）：消息随到随处理，所以每条都能按**窗口
        //   当时的位置**回算客户区坐标 —— 这才是真实鼠标的轨迹：光标在屏幕上走，
        //   窗口落后于它，于是客户区坐标一路变大。
        // * `flood`（一次性排空）：坐标必须在**发送前**全部算好，回不了处理时的
        //   窗口位置。所以它只能用「光标相对窗口恒定」，那是**关节流**时的真实
        //   情形（窗口每条都追上光标，光标在窗口内的相对位置确实不变）。
        //   硬套真实轨迹会让位移按三角数膨胀、跑到边界被夹住 —— 量出来的
        //   成本（52µs/条）和位移（1780px）就都是假的。
        //
        // 取值一律放在计时之外，别把 `GetWindowRect` 算进单条成本。
        let (nx, ny) = if paced {
            let r = win_rect(a.panel);
            ((csx + 1 + i as i32) - r.left, csy - r.top)
        } else {
            (cx + 1, cy)
        };
        let t0 = std::time::Instant::now();
        send_mouse(a.panel, WM_MOUSEMOVE, nx, ny);
        if paced {
            pump();
        }
        busy += t0.elapsed();
        // 滞后 = 目标位置（状态，每条消息都更新）− 窗口实际位置（落定，按节拍）。
        // 这是节流引入的唯一代价，量出来才知道值不值。
        let lag = (target_pos().0 - win_rect(a.text).left).abs();
        lag_max = lag_max.max(lag);
        lag_sum += lag as i64;
        if paced {
            // 忙等而不是 `sleep`：Windows 的默认时基是 15.6ms，
            // `sleep(1ms)` 实际会睡掉十几毫秒，1000Hz 节奏根本模拟不出来。
            let deadline = std::time::Instant::now() + std::time::Duration::from_micros(1000);
            while std::time::Instant::now() < deadline {
                std::hint::spin_loop();
            }
        }
    }
    if !paced {
        let t0 = std::time::Instant::now();
        pump();
        busy += t0.elapsed();
    }

    let msgs = tally_total();
    let by_msg = tally_rows();

    send_mouse(a.panel, WM_LBUTTONUP, cx, cy);
    pump();
    // 位移在**松手之后**量：节流允许拖动途中欠一点位置（那是 `lag` 在记账），
    // 但松手那一刻是用户眼里的终点，必须一条不差地停在手放的地方。
    let moved = win_rect(a.text).left - before;

    DragBench {
        us: busy.as_secs_f64() * 1e6 / n as f64,
        moved,
        msgs,
        by_msg,
        lag_max,
        lag_avg: lag_sum as f64 / n as f64,
    }
}

/// 窗口移动的成本拆解：到底花在底板、文字层、还是「动一下」这件事本身。
///
/// 上一版 `--perf` 报的「整体移动 190µs」就是这么漏掉的：那个数是在
/// 0/+1px 抖动下量的，窗口几乎没真的移动过。这里让窗口**真的**一段段走，
/// 并把各条路径单独拎出来比。
///
/// 实测结论（和最初的直觉相反，所以留着这个入口）：
/// * **底板（Acrylic）最贵**，接近 300µs —— DWM 每换一次位置就要重算模糊区域。
/// * **分层窗口用 ULW 移位置（~96µs）比 SetWindowPos（~193µs）快一倍**：
///   `SetWindowPos` 要跑完整套窗口变更流程（`WM_WINDOWPOSCHANGING` /
///   `WM_NCCALCSIZE` / `WM_WINDOWPOSCHANGED`…），而 ULW 是专门给分层窗口开的路。
fn move_cost_breakdown(a: &App, n: usize) -> Vec<(&'static str, f64, i32)> {
    const FLAGS: u32 = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
    // 每档都跑完再**读回窗口的真实位置**：只报耗时不报位移，
    // 会把「走了完整流程但什么也没动」也算成一次成功的移动。
    //
    // 但位移读数还差一步：每档都从 `bx` 挪到 `bx + n - 1`，若上一档结束时窗口
    // 已经停在 `bx + n - 1`，本档就会「踱出 0 px」——耗时照测，位移恒为 0。
    // 所以计时前必须先用 `reset` 把窗口**归位到同一起点**（归位不计时）。
    let row = |name: &'static str,
               probe: HWND,
               reset: &mut dyn FnMut(),
               f: &mut dyn FnMut(i32)| {
        reset();
        let before = unsafe { win_rect(probe).left };
        let t = std::time::Instant::now();
        for i in 0..n {
            f(i as i32);
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        let moved = unsafe { win_rect(probe).left } - before;
        (name, us, moved)
    };
    let (bx, by) = (200, 200);
    let (panel, text, frame) = (a.panel, a.text, &a.frame);
    // 归位用的闭包，按 target 的移动路径选：ULW 移的窗口要用 ULW 归位，
    // 否则归位动作本身就不是我们要测的那条路径。
    let mut home_panel = || unsafe {
        SetWindowPos(panel, null_mut(), bx, by, 0, 0, FLAGS);
    };
    let mut home_text = || unsafe {
        SetWindowPos(text, null_mut(), bx, by, 0, 0, FLAGS);
    };
    let mut home_text_ulw = || {
        frame.present(text, bx, by);
    };
    let mut out = Vec::new();
    unsafe {
        out.push(row("只挪底板（Acrylic 显形）", panel, &mut home_panel, &mut |i| {
            SetWindowPos(panel, null_mut(), bx + i, by, 0, 0, FLAGS);
        }));
        // 关掉 Acrylic 再挪一次：这一档决定「布局模式该不该继续用毛玻璃」。
        // 隐藏是彻底不合成，无 Acrylic 是「在合成，但不模糊」——
        // 两者之间的差额才是 DWM 每次重算模糊的真实代价。
        set_accent(panel, ACCENT_DISABLED, 0);
        out.push(row(
            "只挪底板（关掉 Acrylic）",
            panel,
            &mut home_panel,
            &mut |i| {
                SetWindowPos(panel, null_mut(), bx + i, by, 0, 0, FLAGS);
            },
        ));
        ShowWindow(panel, SW_HIDE);
        out.push(row(
            "只挪底板（隐藏时）",
            panel,
            &mut home_panel,
            &mut |i| {
                SetWindowPos(panel, null_mut(), bx + i, by, 0, 0, FLAGS);
            },
        ));
        ShowWindow(panel, SW_SHOWNOACTIVATE);
        set_accent(panel, ACCENT_ENABLE_ACRYLICBLURBEHIND, a.panel_alpha());
        out.push(row(
            "只挪文字层（SetWindowPos）",
            text,
            &mut home_text,
            &mut |i| {
                SetWindowPos(text, null_mut(), bx + i, by, 0, 0, FLAGS);
            },
        ));
        out.push(row("只挪文字层（ULW）", text, &mut home_text_ulw, &mut |i| {
            frame.present(text, bx + i, by);
        }));
        out.push(row(
            "两个一起挪（都是 SetWindowPos）",
            text,
            &mut || {
                SetWindowPos(panel, null_mut(), bx, by, 0, 0, FLAGS);
                SetWindowPos(text, null_mut(), bx, by, 0, 0, FLAGS);
            },
            &mut |i| {
                SetWindowPos(panel, null_mut(), bx + i, by, 0, 0, FLAGS);
                SetWindowPos(text, null_mut(), bx + i, by, 0, 0, FLAGS);
            },
        ));
        out.push(row(
            "底板 SWP + 文字层 ULW（= 现在默认）",
            text,
            &mut || {
                SetWindowPos(panel, null_mut(), bx, by, 0, 0, FLAGS);
                frame.present(text, bx, by);
            },
            &mut |i| {
                SetWindowPos(panel, null_mut(), bx + i, by, 0, 0, FLAGS);
                frame.present(text, bx + i, by);
            },
        ));
    }
    out
}

fn dragbench_inner() -> i32 {
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }
        let Some(mut st) = App::new() else {
            println!("FAIL  初始化失败");
            return 3;
        };
        st.hinst = GetModuleHandleW(null());
        st.cfg = Config::for_test();
        // 字体必须跟着基线配置重建 —— 理由见 `Config::for_test()` 的文档
        // （不重建的话，用户那个字体决定时钟块多高，进而决定命中判定认不认得出「整窗拖动」）
        st.rebuild_fonts();
        st.cfg.layout_mode = true;
        let boxed = Box::new(st);
        app_ptr_set(Box::into_raw(boxed));
        let a = app();

        if !bootstrap(a) {
            println!("FAIL  窗口创建失败");
            return 2;
        }
        apply_mode(a);
        render_and_present(a);
        // 定时器会在背后来 WM_TIMER 插帧重绘 —— 那正是拖动时跟鼠标抢 UI 线程的东西。
        // 这一节量的是「一条鼠标移动自己的账」，先摘掉它做隔离。
        KillTimer(a.text, TIMER_ID);

        const N: usize = 800;

        println!("拖动跟手标定（1000Hz 鼠标每条消息的预算 = 1000µs）");
        println!("{}", "-".repeat(80));
        println!("  一档 = {} 条移动消息，窗口应当实打实走过 {} px", N, N);
        println!("{}", "-".repeat(80));

        // ⓪ 先拆 API 成本：「移动窗口」这件事本身值多少钱，钱花在哪个窗口上
        //
        // 先跑一轮丢掉：**第一档永远是最先跑的**，少了预热它的读数会系统性偏高
        // （实测「只挪底板（Acrylic 显形）」在 255~307µs 之间飘，而后面几档很稳），
        // 而那正好是「关掉 Acrylic 值不值」这个结论的第一手数据 —— 偏了就会误导。
        move_cost_breakdown(a, 100);
        let bd = move_cost_breakdown(a, 400);
        let get = |k: &str| {
            bd.iter()
                .find(|(n, _, _)| *n == k)
                .map(|(_, v, _)| *v)
                .unwrap_or(0.0)
        };
        println!("  单次窗口移动的成本拆解（离屏直调，每种 400 次平均）：");
        for (name, us, moved) in &bd {
            println!(
                "    {:<34} {:>8.1} µs   实际移动 {:>5} px",
                name, us, moved
            );
        }
        println!("{}", "-".repeat(80));

        // ① 成本上限：每条都真的落窗口（关节流），和旧实现同口径
        let swp = bench_drag(a, MoveWay::Swp, 0, N, false);
        let ulw = bench_drag(a, MoveWay::Ulw, 0, N, false);
        println!("  每条都落窗口（节流关）—— 成本上限，两条移动路径同口径：");
        for (name, r) in [
            ("SetWindowPos 移动", &swp),
            ("UpdateLayeredWindow 移动（现在默认）", &ulw),
        ] {
            println!(
                "    {:<34} {:>8.1} µs {:>8.0} 条/秒  位移 {}/{} px",
                name,
                r.us,
                1e6 / r.us.max(1.0),
                r.moved,
                N
            );
        }
        println!("{}", "-".repeat(80));

        // ② 真实节奏：每条间隔 1000µs（这才是有意义的那个数）
        let paced_off = bench_drag(a, MoveWay::Swp, 0, N, true);
        let paced_swp = bench_drag(a, MoveWay::Swp, DRAG_MOVE_MS, N, true);
        let paced_on = bench_drag(a, MoveWay::Ulw, DRAG_MOVE_MS, N, true);
        println!("  真实节奏（每条等 1000µs，模拟 1000Hz 连续甩鼠标）：");
        for (name, r) in [
            ("节流关 · SetWindowPos（成本上限）", &paced_off),
            ("节流 8ms · SetWindowPos", &paced_swp),
            ("节流 8ms · ULW 移动（现在默认）", &paced_on),
        ] {
            println!(
                "    {:<34} {:>7.1} µs  松手位移 {}/{} px  途中滞后 峰 {} / 均 {:.1} px",
                name, r.us, r.moved, N, r.lag_max, r.lag_avg
            );
        }
        println!("{}", "-".repeat(80));

        // ③ 一条鼠标移动引发多少条消息
        println!("  一条鼠标移动引发的消息（节流 8ms，照 {} 条移动消息计）：", N);
        for (m, c) in paced_on.by_msg.iter().take(8) {
            println!("    {:<30} {:>7} 条", msg_name(*m), c);
        }
        println!("    {:<30} {:>7} 条（合计）", "—— 总计", paced_on.msgs);
        println!("{}", "-".repeat(80));

        // ULW 提交是异步的，**调用时间**未必比 SetWindowPos 贵多少；
        // 真正吃亏的是 DWM 侧的拷贝和排队。所以这条对比只报数、不下结论，
        // 硬判据放在「位移对不对」和「真实节奏下够不够快」上。
        //
        // 下面这段结论**全部从上面测出来的数推**，不写死。原先这里写死了
        // 「Acrylic 重算模糊是最大一笔」—— 被自己的数据推翻了：关掉 Acrylic
        // 几乎不省，说明成本在「合成一个可见窗口」，不在模糊。别再写死结论。
        let (p_acr, p_off, p_hid) = (
            get("只挪底板（Acrylic 显形）"),
            get("只挪底板（关掉 Acrylic）"),
            get("只挪底板（隐藏时）"),
        );
        let d = p_off - p_acr;
        println!(
            "  底板是最贵的一笔：显形 {:.0}µs / 关掉 Acrylic {:.0}µs / 整窗隐藏 {:.0}µs。",
            p_acr, p_off, p_hid
        );
        println!(
            "  关掉 Acrylic 只差 {:+.1}µs（{:+.0}%）—— 钱不在「模糊」，在「合成一个可见窗口」本身；\n\
             \x20 只有整窗隐藏才真的省下 {:.0}µs。所以布局模式没必要为拖动去关毛玻璃。",
            d,
            d / p_acr * 100.0,
            p_acr - p_hid
        );
        println!(
            "  文字层：ULW 挪 {:.0}µs 比 SetWindowPos 挪 {:.0}µs 快一倍 ——\n\
             \x20 分层窗口的位置变更走 ULW 是专线，SetWindowPos 要跑整套窗口变更流程。",
            get("只挪文字层（ULW）"),
            get("只挪文字层（SetWindowPos）")
        );
        let (both, mixed) = (
            get("两个一起挪（都是 SetWindowPos）"),
            get("底板 SWP + 文字层 ULW（= 现在默认）"),
        );
        println!(
            "  合起来：两条都走 SetWindowPos {:.0}µs；改成「底板 SWP + 文字层 ULW」{:.0}µs，省 {:.0}µs（{:.0}%）。",
            both,
            mixed,
            both - mixed,
            (both - mixed) / both * 100.0
        );

        let ok = paced_off.moved == N as i32
            && paced_on.moved == N as i32
            && paced_on.us < 1000.0
            && paced_on.lag_max <= 9;
        println!(
            "  跟手判据：松手位移一条不丢（{}/{}、{}/{}）；真实节奏下单条 < 1000µs（{:.0}µs）；\n\
             \x20 途中滞后 ≤ 一个节拍（8px，实测峰值 {}px）",
            paced_off.moved, N, paced_on.moved, N, paced_on.us, paced_on.lag_max
        );
        if ok {
            0
        } else {
            3
        }
    }
}

// ---------------------------------------------------------------- 资源 soak
//
// 老板的要求很直白：「运行不能内存泄漏，占用也不能扩大」。
// 光看 Private Bytes 不够 —— GDI 对象 / USER 对象 / 内核句柄各自有独立配额
// （GDI 默认 10000），泄漏的话先撞配额，表现是「画着画着就不刷新了」，
// 而不是内存爆掉。所以四样一起盯。

/// 一次资源采样。
#[derive(Clone, Copy, Default)]
struct Res {
    /// 私有提交内存（判断泄漏看这个，不看 Working Set —— 工作集会被系统随时回收）
    private: usize,
    working: usize,
    gdi: u32,
    user: u32,
    handles: u32,
}

fn sample() -> Res {
    let (private, working) = crate::ffi::process_memory().unwrap_or((0, 0));
    let (gdi, user) = crate::ffi::gui_resources().unwrap_or((0, 0));
    let handles = crate::ffi::handle_count().unwrap_or(0);
    Res {
        private,
        working,
        gdi,
        user,
        handles,
    }
}

/// 一次短拖动：按下 → n 步移动 → 松开。
///
/// 坐标给的是**客户区**坐标，位移量 = 坐标变化量（`raw = cfg + (cx − cx0)`），
/// 所以这里能精确控制窗口走多远。
unsafe fn drag_steps(a: &mut App, start: (i32, i32), dx: i32, dy: i32, n: i32) {
    send_mouse(a.panel, WM_LBUTTONDOWN, start.0, start.1);
    pump();
    for i in 1..=n {
        send_mouse(
            a.panel,
            WM_MOUSEMOVE,
            start.0 + dx * i / n,
            start.1 + dy * i / n,
        );
        pump();
    }
    send_mouse(a.panel, WM_LBUTTONUP, start.0 + dx, start.1 + dy);
    pump();
}

/// 跑一轮「什么都会碰到」的操作，把所有会**真的分配资源**的路径走一遍。
///
/// 这里刻意不去只测内存：字体句柄（`rebuild_fonts` → `Fonts::destroy` +
/// `Fonts::build`）、帧缓冲（缩放 → 三条掩码 `Vec` + 两个 DIB 重分配）、
/// 窗口样式切换（`apply_mode`）才是最可能漏 release 的地方，
/// 而它们的泄漏首先体现在 GDI 对象数上。
unsafe fn soak_round(a: &mut App, round: usize) {
    let (w, h) = (a.cfg.w, a.cfg.h);
    let mid = (w / 2, h / 2);
    let corner = (w - 2, h - 2);
    let clock = (a.clock_rect.left + 4, a.clock_rect.top + 4);
    let even = round.is_multiple_of(2);

    // ① 整窗移动（来回），② 时间日期块，③ 右下角缩放（每次都重建帧缓冲）
    drag_steps(a, mid, if even { 40 } else { -40 }, 0, 4);
    drag_steps(a, clock, 20, 10, 3);
    drag_steps(a, corner, if even { 24 } else { -24 }, if even { 16 } else { -16 }, 3);

    // ④ 换字号 → 销毁并重建全部字体句柄
    a.cfg.scale = if even { 100 } else { 130 };
    a.rebuild_fonts();

    // ⑤ 颜色 / 时钟日期开关 / 动效 / 模式（每种都触发一次完整的重绘或样式切换）
    a.cfg.color = if even { 0xFFFFFF } else { 0xF5D98B };
    a.cfg.show_date = even;
    a.cfg.show_clock = even;
    a.cfg.anim = Anim::all()[round % 4];
    a.cfg.mode = if round.is_multiple_of(3) {
        Mode::TextOnly
    } else {
        Mode::Glass
    };
    apply_mode(a);

    // 复位，免得拖着拖着被工作区边界夹住
    a.cfg.x = 60;
    a.cfg.y = 60;
    apply_geometry(a, true, true);
}

/// 一个指标该怎么判。
#[derive(Clone, Copy)]
enum Judged {
    /// 精确计数（GDI / USER / 句柄）：一个都不许涨
    Exact,
    /// 提交内存：允许分配器保留一点余量
    Bytes,
    /// 只报数：驻留集会被系统按内存压力回收，拿它当泄漏判据会天天误报
    Report,
}

/// `desk-geyan --memtest`：长时间 soak，看内存和句柄会不会涨。
///
/// 判据分两段比：前 1/3 轮 vs 后 1/3 轮。
/// * GDI / USER / 句柄：**一个都不许涨**。这几个是精确计数的资源，
///   只要 soak 里每次都成对地建/删，稳态就该回到同一个数；涨了就是漏了。
/// * Private Bytes：允许小幅波动（分配器不保证把释放的内存还给系统），
///   但后段的**峰值**不得比前段的高出 5% 或 2MB。
pub fn memtest() -> i32 {
    with_config_guard(memtest_inner)
}

fn memtest_inner() -> i32 {
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }
        // 探针本身取不到就把结论也标成不可信，别默认「通过」
        let probe_ok = crate::ffi::process_memory().is_some()
            && crate::ffi::gui_resources().is_some()
            && crate::ffi::handle_count().is_some();

        let Some(mut st) = App::new() else {
            println!("FAIL  初始化失败");
            return 3;
        };
        st.hinst = GetModuleHandleW(null());
        st.cfg = Config::for_test();
        // 字体必须跟着基线配置重建 —— 理由见 `Config::for_test()` 的文档
        // （不重建的话，用户那个字体决定时钟块多高，进而决定命中判定认不认得出「整窗拖动」）
        st.rebuild_fonts();
        st.cfg.layout_mode = true;
        let boxed = Box::new(st);
        app_ptr_set(Box::into_raw(boxed));
        let a = app();

        if !bootstrap(a) {
            println!("FAIL  窗口创建失败");
            return 2;
        }
        apply_mode(a);
        KillTimer(a.text, TIMER_ID); // soak 自己驱动，不要定时器插队

        const ROUNDS: usize = 120;
        const WARMUP: usize = 5;

        println!("资源 soak（{} 轮 + {} 轮预热）", ROUNDS, WARMUP);
        println!("{}", "-".repeat(78));
        println!("  每轮：3 次拖动（整窗 / 时间日期块 / 缩放）+ 换字号 + 换色 + 换动效 + 换模式");
        println!("{}", "-".repeat(78));

        // 预热：字体、帧缓冲、词库这些「本来就该分配一次」的东西先落地，
        // 否则第一次分配会被当成增长。
        for i in 0..WARMUP {
            soak_round(a, i);
        }

        let mut hist = Vec::with_capacity(ROUNDS);
        for i in 0..ROUNDS {
            soak_round(a, i);
            hist.push(sample());
        }

        let third = (ROUNDS / 3).max(1);
        let head = &hist[..third];
        let tail = &hist[ROUNDS - third..];
        let avg = |v: &[Res], f: fn(&Res) -> usize| -> f64 {
            v.iter().map(|r| f(r) as f64).sum::<f64>() / v.len() as f64
        };
        let max_of = |v: &[Res], f: fn(&Res) -> usize| -> usize {
            v.iter().map(f).max().unwrap_or(0)
        };

        type Row = (&'static str, fn(&Res) -> usize, Judged);
        let rows: [Row; 5] = [
            ("Private Bytes", |r| r.private, Judged::Bytes),
            ("Working Set", |r| r.working, Judged::Report),
            ("GDI 对象", |r| r.gdi as usize, Judged::Exact),
            ("USER 对象", |r| r.user as usize, Judged::Exact),
            ("内核句柄", |r| r.handles as usize, Judged::Exact),
        ];

        let mut ok = probe_ok;
        println!(
            "  {:<16} {:>12} {:>12} {:>11} {:>11}   判定",
            "指标", "起始", "结束", "前1/3峰", "后1/3峰"
        );
        for (name, f, kind) in rows {
            let first = f(&hist[0]);
            let last = f(&hist[ROUNDS - 1]);
            let hmax = max_of(head, f);
            let tmax = max_of(tail, f);
            let (verdict, pass) = match kind {
                // 精确计数的资源：一点都不能涨
                Judged::Exact => {
                    if tmax <= hmax {
                        ("持平".to_string(), true)
                    } else {
                        (format!("↑ 涨了 {} —— 泄漏", tmax - hmax), false)
                    }
                }
                // 提交内存：分配器不保证把释放的还给系统，给 5% / 2MB 的波动余量
                Judged::Bytes => {
                    let grow = tmax.saturating_sub(hmax);
                    let tol = (hmax / 20).max(2 * 1024 * 1024);
                    if grow <= tol {
                        (format!("稳定（峰波动 {}，容差 {}）", fmt_mb(grow), fmt_mb(tol)), true)
                    } else {
                        (format!("↑ 涨了 {}", fmt_mb(grow)), false)
                    }
                }
                // 驻留集：会被系统按内存压力随时回收，长了不代表漏了。
                // 只报数不判定 —— 拿它当泄漏判据会天天误报。
                Judged::Report => ("仅参考（驻留集可被系统回收）".to_string(), true),
            };
            ok &= pass;
            let show = |v: usize| match kind {
                Judged::Exact => v.to_string(),
                _ => fmt_mb(v),
            };
            println!(
                "  {:<16} {:>12} {:>12} {:>11} {:>11}   {}",
                name,
                show(first),
                show(last),
                show(hmax),
                show(tmax),
                verdict
            );
        }
        println!("{}", "-".repeat(78));

        let ha = avg(head, |r| r.private);
        let ta = avg(tail, |r| r.private);
        println!(
            "  Private Bytes 均值：前 {:.1} MB → 后 {:.1} MB（Δ {:+.1} MB）",
            ha / 1048576.0,
            ta / 1048576.0,
            (ta - ha) / 1048576.0
        );

        // 静置观察：不再碰任何东西，只让消息循环空转。
        // 工作集在 soak 期间涨了不一定是泄漏（刚碰过的页面自然驻留），
        // 但**静下来之后回落不了**就是实打实的占用扩大 —— 这才是老板要的那个答案。
        const IDLE_SECS: u64 = 20;
        let before_idle = sample();
        for _ in 0..(IDLE_SECS * 100) {
            std::thread::sleep(std::time::Duration::from_millis(10));
            pump();
        }
        let after_idle = sample();
        println!(
            "  静置 {}s 后：Private {:.1} → {:.1} MB（{:+.1} MB）· Working Set {:.1} → {:.1} MB（{:+.1} MB）",
            IDLE_SECS,
            before_idle.private as f64 / 1048576.0,
            after_idle.private as f64 / 1048576.0,
            (after_idle.private as f64 - before_idle.private as f64) / 1048576.0,
            before_idle.working as f64 / 1048576.0,
            after_idle.working as f64 / 1048576.0,
            (after_idle.working as f64 - before_idle.working as f64) / 1048576.0,
        );
        if after_idle.working > before_idle.working {
            println!("  注意：静置期间驻留集没回落（可能是页面正好都在用，也可能是真占用）");
        }

        if !probe_ok {
            println!("  注意：本机取不到某个资源计数器，结论不可信");
        }
        if ok {
            0
        } else {
            3
        }
    }
}

fn fmt_mb(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1048576.0)
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

/// 灌一次「按下 → N 条移动 → 松开」进**真正的窗口过程**，返回每条移动消息的平均成本（µs）。
///
/// 起点取客户区里一个确定的位置，位移在 0/+1px 之间交替：既真的触发了移动/缩放，
/// 又不会一路跑到边界上被夹住（那会让后段消息的成本失真）。
/// 一次灌满再排空（`flood`）才是真实鼠标的行为：连续移动时消息是成串来的。
/// `moving` = 整窗移动。两种拖动的**真实光标轨迹不一样**，坐标构造必须跟着变：
///
/// * 整窗移动：窗口跟着光标跑，所以「光标相对窗口没动」才是真实情形 ——
///   客户区坐标**恒定**，处理第 i 条时窗口已经走了 i−1 px，算出来正好 +1。
/// * 缩放 / 拖时间日期块：窗口原点不动，光标是真的在往前爬 —— 客户区坐标**递增**。
///
/// 一开始这里两种都灌恒定坐标，于是缩放那档等于「光标一直没动、尺寸压根没变」，
/// 量出来的 23µs 是个假数（`SetWindowPos` 发现无事可做就秒返回）。
unsafe fn drag_batch(a: &mut App, start: (i32, i32), n: usize, flood: bool, moving: bool) -> f64 {
    let panel = a.panel;
    // 与旧实现同口径：每条都真的落一次窗口（关节流）。
    // 不关掉的话，一口气灌进去的消息会被节流吃掉 99%，量到的是
    // 「消息一次性到达」这个不存在的场景 —— 数字好看，但没意义。
    a.move_throttle = 0;
    a.move_way = MoveWay::Swp;
    send_mouse(panel, WM_LBUTTONDOWN, start.0, start.1);
    pump();
    let t = std::time::Instant::now();
    for i in 0..n {
        let cx = if moving {
            start.0 + 1
        } else {
            start.0 + 1 + i as i32
        };
        send_mouse(panel, WM_MOUSEMOVE, cx, start.1);
        if !flood {
            pump();
        }
    }
    if flood {
        pump();
    }
    let us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
    send_mouse(panel, WM_LBUTTONUP, start.0, start.1);
    pump();
    us
}


fn dragtest_inner() -> i32 {
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }
        let Some(mut st) = App::new() else {
            println!("FAIL  初始化失败");
            return 3;
        };
        st.hinst = GetModuleHandleW(null());
        st.cfg = Config::for_test();
        // 字体必须跟着基线配置重建 —— 理由见 `Config::for_test()` 的文档
        // （不重建的话，用户那个字体决定时钟块多高，进而决定命中判定认不认得出「整窗拖动」）
        st.rebuild_fonts();
        // 关掉位置节流：这一节验的是**拖动逻辑**（位移量、夹取、锚点重置、
        // 松手落盘），而不是节拍时序。而且这里灌消息是同步的、没有时间流逝，
        // 开着节流会把绝大多数移动直接吃掉 —— 那样量到的是「8ms 里能落几次」，
        // 跟逻辑对不对毫无关系。节流本身由 `--dragbench` 专门验。
        st.move_throttle = 0;
        let boxed = Box::new(st);
        app_ptr_set(Box::into_raw(boxed));
        let a = app();

        if !bootstrap(a) {
            println!("FAIL  窗口创建失败");
            return 2;
        }
        render_and_present(a); // 先出一帧，把 clock_rect 量出来

        let mut rows: Vec<(&str, R, String)> = Vec::new();
        let center = |a: &App| {
            (
                a.clock_rect.left + (a.clock_rect.right - a.clock_rect.left) / 2,
                a.clock_rect.top + (a.clock_rect.bottom - a.clock_rect.top) / 2,
            )
        };

        // ---- ① 非布局模式：拖动必须无效。
        // 这是「不影响桌面其他东西使用」在交互侧的另一半 ——
        // 平时鼠标是穿透的，状态机上也不该有任何入口。
        {
            a.cfg.layout_mode = false;
            a.cfg.clock_dx = 0;
            a.cfg.clock_dy = 0;
            render_and_present(a);
            let (bx, by) = center(a);
            press(a.panel, bx, by);
            pump();
            send_mouse(a.panel, WM_MOUSEMOVE, bx + 200, by + 30);
            pump();
            send_mouse(a.panel, WM_LBUTTONUP, bx + 200, by + 30);
            pump();
            rows.push((
                "非布局模式：拖动被忽略（防误触）",
                if a.cfg.clock_dx == 0 && a.cfg.clock_dy == 0 {
                    R::Pass
                } else {
                    R::Fail
                },
                format!("偏移仍为 {}/{}", a.cfg.clock_dx, a.cfg.clock_dy),
            ));
        }

        // ---- ② 布局模式：一次真实拖动
        a.cfg.layout_mode = true;
        apply_mode(a);
        render_and_present(a);
        let (bx, by) = center(a);
        let x0 = a.clock_rect.left;

        press(a.panel, bx, by);
        pump();
        // 分 10 步移动，模拟真实拖拽的连续过程
        for i in 1..=10 {
            send_mouse(a.panel, WM_MOUSEMOVE, bx + i * 20, by + i * 3);
            pump();
        }
        send_mouse(a.panel, WM_LBUTTONUP, bx + 200, by + 30);
        pump();

        let moved = a.clock_rect.left - x0;
        rows.push((
            "布局模式：拖动真的移动了时间日期块",
            if a.cfg.clock_dx == 200 && a.cfg.clock_dy == 30 && moved > 150 {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "光标 +200/+30 → 偏移 {}/{}, 块 x {} → {}（位移 {}）",
                a.cfg.clock_dx, a.cfg.clock_dy, x0, a.clock_rect.left, moved
            ),
        ));
        // 拖完必须把捕获还回去，否则整个桌面的鼠标都会被这个窗口吞掉
        rows.push((
            "释放鼠标捕获（不吞桌面鼠标）",
            if GetCapture().is_null() {
                R::Pass
            } else {
                R::Fail
            },
            format!("GetCapture = {:p}", GetCapture()),
        ));
        rows.push((
            "拖动结果已落盘",
            if std::fs::read_to_string(Config::path())
                .map(|t| t.contains("clock_dx=200"))
                .unwrap_or(false)
            {
                R::Pass
            } else {
                R::Fail
            },
            "config.ini 里 clock_dx=200".into(),
        ));

        // ---- ③ 往死里拖：必须被夹住，且**偏移回写**成夹取后的值
        let (bx, by) = center(a);
        press(a.panel, bx, by);
        pump();
        send_mouse(a.panel, WM_MOUSEMOVE, 6000, 6000);
        pump();
        send_mouse(a.panel, WM_LBUTTONUP, 6000, 6000);
        pump();
        let inside = a.clock_rect.right <= a.cfg.w && a.clock_rect.bottom <= a.cfg.h;
        rows.push((
            "拖出边界：夹回窗口内",
            if inside {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "光标 +6000 → 块 [{},{}]-[{},{}] in {}x{}",
                a.clock_rect.left,
                a.clock_rect.top,
                a.clock_rect.right,
                a.clock_rect.bottom,
                a.cfg.w,
                a.cfg.h
            ),
        ));
        // 夹取后偏移必须等于「实际落点 − 默认落点」，否则往回拖会空走
        let clamped_dx = a.cfg.clock_dx;
        press(a.panel, center(a).0, center(a).1);
        pump();
        send_mouse(a.panel, WM_MOUSEMOVE, center(a).0 - 100, center(a).1);
        pump();
        send_mouse(a.panel, WM_LBUTTONUP, 0, 0);
        pump();
        let immediately_moved = (clamped_dx - a.cfg.clock_dx) == 100;
        rows.push((
            "边界回拖立即跟手（无空走）",
            if immediately_moved {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "夹取到 {} 后往回拖 100px → 偏移 {}（位移正好 100）",
                clamped_dx, a.cfg.clock_dx
            ),
        ));

        // ---- ④ 整体移动：位置必须**即时**跟手，且不该产生任何整帧重绘
        //
        // 这次修延迟的核心断言。旧实现每个 WM_MOVE 都跑一遍整帧重绘
        // （实测 1089µs/条，而 1000Hz 鼠标每条消息的预算只有 1000µs ——
        // 卡在线上，消息就会在队列里越积越多，手感就是「粘」）。
        // 而移动只改窗口位置，内容的像素一个都没变，那些工作全是白花的。
        // 这里用**整帧重绘计数**把它钉住：位置变化不该产生任何渲染工作。
        a.cfg.clock_dx = 0;
        a.cfg.clock_dy = 0;
        // 读回像素做对比时不能让跑马灯跑：动效停住，两次抓图才只差位置
        a.cfg.anim = Anim::Static;
        render_and_present(a);
        KillTimer(a.text, TIMER_ID); // 定时器自己会出帧，先停掉才数得准

        let w0 = win_rect(a.panel);
        let mid = (a.cfg.w / 2, a.cfg.h / 2);
        let shot_before = gfx::capture_window(a.text, a.cfg.w, a.cfg.h);

        let renders0 = a.renders;
        press(a.panel, mid.0, mid.1);
        pump();
        send_mouse(a.panel, WM_MOUSEMOVE, mid.0 + 40, mid.1 + 25);
        pump();
        let first = win_rect(a.panel);
        let step = (first.left - w0.left, first.top - w0.top);
        for i in 1..40 {
            send_mouse(a.panel, WM_MOUSEMOVE, mid.0 + 40 + i % 2, mid.1 + 25 + i % 2);
            pump();
        }
        let renders_move = a.renders - renders0;
        let r1 = win_rect(a.panel);
        let text1 = win_rect(a.text);
        let shot_after = gfx::capture_window(a.text, a.cfg.w, a.cfg.h);
        send_mouse(a.panel, WM_LBUTTONUP, mid.0, mid.1);
        pump();

        rows.push((
            "整体移动：第一条消息就到位",
            if step == (40, 25) { R::Pass } else { R::Fail },
            format!("光标 +40/+25 → 窗口马上位移 {:?}", step),
        ));
        rows.push((
            "整体移动：41 条消息 0 次整帧重绘",
            if renders_move == 0 { R::Pass } else { R::Fail },
            format!(
                "重绘 {} 次（旧实现每条消息一次 = 41 次，每帧 ~800µs）",
                renders_move
            ),
        ));
        rows.push((
            "整体移动：文字层与底板不脱层",
            if r1.left == text1.left && r1.top == text1.top {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "底板 ({},{}) · 文字层 ({},{})",
                r1.left, r1.top, text1.left, text1.top
            ),
        ));
        rows.push((
            "整体移动：新位置已落盘",
            if std::fs::read_to_string(Config::path())
                .map(|t| t.contains(&format!("x={}", r1.left)))
                .unwrap_or(false)
            {
                R::Pass
            } else {
                R::Fail
            },
            format!("config.ini 里 x={}", r1.left),
        ));

        // 位置变了，画的东西必须一个像素都没动。分层窗口的像素归 DWM 持有，
        // 只能靠 PrintWindow 读回合成结果 —— 读不回就如实标 INFO，不假装验过。
        match (&shot_before, &shot_after) {
            (Some(b), Some(f)) if b.len() == f.len() => {
                let diff = b.iter().zip(f.iter()).filter(|(x, y)| x != y).count();
                rows.push((
                    "整体移动：图层内容逐像素未变",
                    if diff == 0 { R::Pass } else { R::Fail },
                    format!("{} px 不同 / {} px 总量", diff, b.len()),
                ));
            }
            _ => rows.push((
                "整体移动：图层内容逐像素未变",
                R::Info,
                "本环境 PrintWindow 读不回分层窗口，未验证".into(),
            )),
        }

        // ---- ⑤ 缩放：从右下角拉，宽和高都该跟着涨
        let rw0 = win_rect(a.panel);
        let corner = (a.cfg.w - 2, a.cfg.h - 2);
        press(a.panel, corner.0, corner.1);
        pump();
        send_mouse(a.panel, WM_MOUSEMOVE, corner.0 + 60, corner.1 + 20);
        pump();
        send_mouse(a.panel, WM_LBUTTONUP, corner.0 + 60, corner.1 + 20);
        pump();
        let rw1 = win_rect(a.panel);
        let grew = (rw1.right - rw1.left) == (rw0.right - rw0.left) + 60
            && (rw1.bottom - rw1.top) == (rw0.bottom - rw0.top) + 20;
        rows.push((
            "缩放：右下角拉出 +60/+20",
            if grew { R::Pass } else { R::Fail },
            format!(
                "{}x{} → {}x{}",
                rw0.right - rw0.left,
                rw0.bottom - rw0.top,
                rw1.right - rw1.left,
                rw1.bottom - rw1.top
            ),
        ));
        rows.push((
            "缩放：帧缓冲跟着重建",
            if a.frame.w == a.cfg.w && a.frame.h == a.cfg.h {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "frame {}x{} · cfg {}x{}",
                a.frame.w, a.frame.h, a.cfg.w, a.cfg.h
            ),
        ));
        rows.push((
            "缩放：松手即出最终帧（不留待重绘）",
            if !a.dirty { R::Pass } else { R::Fail },
            format!("dirty = {}", a.dirty),
        ));

        // ---- ⑥ 整体移动的边界保护：拖到天边也得留一部分在屏幕里
        let mid2 = (a.cfg.w / 2, a.cfg.h / 2);
        press(a.panel, mid2.0, mid2.1);
        pump();
        send_mouse(a.panel, WM_MOUSEMOVE, mid2.0 + 20000, mid2.1 + 20000);
        pump();
        let far = win_rect(a.panel);
        send_mouse(a.panel, WM_LBUTTONUP, mid2.0, mid2.1);
        pump();
        // 夹取判据必须按**窗口被拖到天边后的真实位置**取所在屏 ——
        // 这里正是 P1 的多显示器基准：挂件落在哪块屏，就按哪块屏的工作区夹。
        let (wa, _) = work_area_at(far.left, far.top, a.cfg.w);
        let kept = wa.right - far.left >= KEEP_VISIBLE && wa.bottom - far.top >= KEEP_VISIBLE;
        rows.push((
            "整体移动：拖出天边仍留可见部分",
            if kept { R::Pass } else { R::Fail },
            format!(
                "窗口 ({},{}) · 工作区右下 ({},{}) → 屏内还有 {}x{}",
                far.left,
                far.top,
                wa.right,
                wa.bottom,
                wa.right - far.left,
                wa.bottom - far.top
            ),
        ));
        // 夹住之后往回拖必须立刻跟手：判据和上面那条时钟的完全同源
        press(a.panel, mid2.0, mid2.1);
        pump();
        send_mouse(a.panel, WM_MOUSEMOVE, mid2.0 - 50, mid2.1);
        pump();
        let back = win_rect(a.panel).left;
        send_mouse(a.panel, WM_LBUTTONUP, mid2.0, mid2.1);
        pump();
        rows.push((
            "整体移动：夹取后回拖立即跟手",
            if back == far.left - 50 { R::Pass } else { R::Fail },
            format!("{} → {}（位移正好 -50）", far.left, back),
        ));

        // ---- ⑦ 拖时间日期块：状态每条消息都跟手，画面按节拍出
        //
        // 「收状态」和「出画面」解耦之后，这两个比值就是解耦的直接证据。
        // 分两种节奏量，因为它们对应两种真实场景：
        //   * 洪泛（高回报率鼠标的极端情况）：40 条在一瞬间到齐。
        //     正确行为是**合流** —— 中间帧全都可以不要，但最后必须落到最终位置。
        //   * 常规（125Hz 鼠标）：消息间隔 > 8ms，每一条都该得到自己的那一帧。
        a.cfg.clock_dx = 0;
        a.cfg.clock_dy = 0;
        render_and_present(a);
        let (ccx, ccy) = center(a);

        let r0 = a.renders;
        press(a.panel, ccx, ccy);
        pump();
        for i in 0..40 {
            send_mouse(a.panel, WM_MOUSEMOVE, ccx + i, ccy);
            pump();
        }
        let renders_burst = a.renders - r0;
        send_mouse(a.panel, WM_LBUTTONUP, ccx + 39, ccy);
        pump();
        let renders_total = a.renders - r0;
        rows.push((
            "拖时间日期块：偏移每条消息都即时更新",
            if a.cfg.clock_dx == 39 { R::Pass } else { R::Fail },
            format!("40 条消息 → 偏移 {}", a.cfg.clock_dx),
        ));
        rows.push((
            "拖时间日期块：40 条洪泛消息只出 1 帧",
            if renders_burst == 0 && renders_total == 1 {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "过程 {} 帧 + 松手 1 帧 = {} 帧（旧实现 40 帧 × 800µs）",
                renders_burst, renders_total
            ),
        ));

        // 常规节奏：每条消息间隔 12ms（> 8ms 节流窗口），每条都该有自己的帧
        let (ccx2, ccy2) = center(a);
        let r1 = a.renders;
        press(a.panel, ccx2, ccy2);
        pump();
        for i in 1..=5 {
            std::thread::sleep(std::time::Duration::from_millis(12));
            send_mouse(a.panel, WM_MOUSEMOVE, ccx2 - i, ccy2);
            pump();
        }
        let renders_paced = a.renders - r1;
        send_mouse(a.panel, WM_LBUTTONUP, ccx2 - 5, ccy2);
        pump();
        rows.push((
            "拖时间日期块：常规节奏每条都有帧",
            if renders_paced >= 5 { R::Pass } else { R::Fail },
            format!("5 条消息（间隔 12ms）→ 出 {} 帧", renders_paced),
        ));

        // ---- ⑧ 节流路径：拖动中允许欠、松手必须精确到位，且**途中不许漂**
        //
        // 这条必须用**真实光标轨迹**。真实鼠标拖动时窗口位置会落后于 `cfg`
        // （这正是节流的定义），而消息里的客户区坐标是**相对窗口真实位置**的 ——
        // 光标往前走、窗口没跟上，客户区坐标就会一路变大。
        //
        // 一开始这里灌的是**恒定**客户区坐标（注释写「相对当前窗口又前进 1px」），
        // 那个假设只在**关掉节流**时成立。开着节流时它恰好能把一类严重的累积误差
        // 伪装成正常的等差位移：窗口每收到一条消息，就把上一条「欠着的」位移又算一遍，
        // 误差按三角数放大 —— 用户看到的就是「鼠标一动，窗口乱飘」。
        {
            a.move_throttle = DRAG_MOVE_MS;
            // 先归位到屏幕中央：避开边界夹取对轨迹的干扰，也让「位移」这条断言干净
            a.cfg.x = 300;
            a.cfg.y = 300;
            apply_geometry(a, true, true);
            pump();

            let (bx, by) = (a.cfg.w / 2, a.cfg.h / 2);
            let w0 = win_rect(a.text);
            let (ox, oy) = (w0.left, w0.top);
            let s0 = (ox + bx, oy + by); // 按下时，光标的**屏幕**坐标

            press(a.panel, bx, by);
            pump();

            const STEPS: i32 = 40;
            let mut max_err = 0;
            for i in 1..=STEPS {
                let want = (s0.0 + i, s0.1 + i); // 光标在屏幕上匀速走
                // 客户区坐标 = 光标屏幕坐标 − **窗口真实位置**（不是 `cfg`）
                let r = win_rect(a.panel);
                send_mouse(a.panel, WM_MOUSEMOVE, want.0 - r.left, want.1 - r.top);
                pump();
                let got = target_pos();
                let exp = (ox + i, oy + i); // 窗口该去的地方 = 起点 + 光标位移
                max_err = max_err.max((got.0 - exp.0).abs().max((got.1 - exp.1).abs()));
            }
            let lagging = win_rect(a.text).left - ox;

            send_mouse(a.panel, WM_LBUTTONUP, 0, 0);
            pump();
            let settled = win_rect(a.text).left - ox;
            let saved = std::fs::read_to_string(Config::path())
                .map(|t| t.contains(&format!("x={}", ox + STEPS)))
                .unwrap_or(false);

            rows.push((
                "节流 + 真实轨迹：途中不漂、松手精确到位",
                if max_err == 0 && lagging < STEPS && settled == STEPS && saved {
                    R::Pass
                } else {
                    R::Fail
                },
                format!(
                    "光标走 {}px：途中 cfg 偏差峰值 {}px（须恒 0）· 拖动中窗口欠 {}px · 松手 {}/{} px · 落盘 {}",
                    STEPS,
                    max_err,
                    STEPS - lagging,
                    settled,
                    STEPS,
                    if saved { "一致" } else { "不一致" }
                ),
            ));
            a.move_throttle = 0;
        }

        // ---- ⑨ 命中带：哪儿拖动、哪儿缩放
        //
        // 自己接管鼠标之后，这一层映射错了的表现是「某条边拉不动」或
        // 「拖动从某处开始变成缩放」—— 只靠手感极难定位，所以逐点验。
        {
            let (w, h) = (a.cfg.w, a.cfg.h);
            let cases: [(&str, (i32, i32), isize); 9] = [
                ("左上角", (1, 1), HTTOPLEFT),
                ("上边", (w / 2, 1), HTTOP),
                ("右上角", (w - 1, 1), HTTOPRIGHT),
                ("左边", (1, h / 2), HTLEFT),
                ("中间", (w / 2, h / 2), HTCLIENT),
                ("右边", (w - 1, h / 2), HTRIGHT),
                ("左下角", (1, h - 1), HTBOTTOMLEFT),
                ("下边", (w / 2, h - 1), HTBOTTOM),
                ("右下角", (w - 1, h - 1), HTBOTTOMRIGHT),
            ];
            let bad: Vec<&str> = cases
                .iter()
                .filter(|(_, p, want)| edge_zone(a, p.0, p.1) != *want)
                .map(|(n, _, _)| *n)
                .collect();
            rows.push((
                "命中带：8 个方向 + 中间各归其位",
                if bad.is_empty() { R::Pass } else { R::Fail },
                format!(
                    "{}/9 正确（边宽 {}px）{}",
                    cases.len() - bad.len(),
                    (8 * a.cfg.scale / 100).max(6),
                    if bad.is_empty() {
                        String::new()
                    } else {
                        format!(" · 错配: {:?}", bad)
                    }
                ),
            ));
        }

        let mut pass = true;
        for (name, r, detail) in &rows {
            let tag = match r {
                R::Pass => "PASS",
                R::Fail => {
                    pass = false;
                    "FAIL"
                }
                R::Info => "INFO",
            };
            println!("[{}] {:<36} {}", tag, name, detail);
        }
        println!(
            "\n{}",
            if pass {
                "拖动链路端到端通过"
            } else {
                "存在失败项"
            }
        );

        del_tray(a);
        unregister_hotkeys(a);
        a.cfg.layout_mode = false;
        DestroyWindow(a.text);
        DestroyWindow(a.panel);
        if pass {
            0
        } else {
            3
        }
    }
}

// ---------------------------------------------------------------- 入口

pub fn run() -> i32 {
    unsafe {
        // DPI：分层窗口的位图是物理像素，不声明感知的话高 DPI 下会被系统
        // 再放大一次，字直接糊掉。
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }
        // 单实例：两个实例叠在一起只会互相打架
        let name = wide("Local\\DeskGeyan_SingleInstance");
        let _m = CreateMutexW(null(), 0, name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return 0;
        }

        let Some(mut st) = App::new() else {
            return 1;
        };
        st.hinst = GetModuleHandleW(null());

        // 首次运行：贴到**挂件所在屏**的工作区右上角（未放置时坐标是 0，即主屏）
        if !st.cfg.placed {
            let (wa, _) = work_area_at(st.cfg.x, st.cfg.y, st.cfg.w);
            st.cfg.x = wa.right - st.cfg.w - 24;
            st.cfg.y = wa.top + 24;
            st.cfg.placed = true;
        }
        clamp_to_work_area(&mut st.cfg);

        let boxed = Box::new(st);
        app_ptr_set(Box::into_raw(boxed));
        let a = app();

        if !bootstrap(a) {
            return 2;
        }

        // 诊断用：`DG_OPEN_CONFIG=1` 时启动即把设置面板打开。
        // 与 `DG_MOVE` / `DG_LEGACY_DRAG` 同类 —— 只为让「打开设置窗」这件事
        // 在无人值守的脚本里可复现（截屏 / 回归），不设时零影响。
        if std::env::var("DG_OPEN_CONFIG").is_ok() {
            config_ui::open(a);
        }

        // ---- 消息泵
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        0
    }
}

/// 矩形逐字段相等。
///
/// `windows-sys` 的 `RECT` 没 derive `PartialEq`，而多显示器那条判据要求的正是
/// 「两块屏的工作区**逐字段**相同」—— 按面积或宽高比相等都不算，
/// 副屏的工作区完全可能与主屏等宽等高但整体偏移。
fn rect_eq(a: &RECT, b: &RECT) -> bool {
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
fn work_area_at(x: i32, y: i32, w: i32) -> (RECT, RECT) {
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

fn clamp_to_work_area(cfg: &mut Config) {
    let (wa, _) = work_area_at(cfg.x, cfg.y, cfg.w);
    cfg.w = cfg.w.clamp(MIN_W, wa.right.max(320));
    cfg.h = cfg.h.clamp(MIN_H, wa.bottom.max(160));
    // 至少留 80px 在屏幕内，避免用户把窗口拖到找不回来的地方
    let (x, y) = clamp_keep_visible(cfg.x, cfg.y, cfg.w);
    cfg.x = x;
    cfg.y = y;
}

// ---------------------------------------------------------------- 建窗

unsafe fn create_panel(a: &App) -> Option<HWND> {
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

unsafe fn create_text(a: &App) -> Option<HWND> {
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
fn raise_pair(a: &App) {
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

fn apply_topmost(a: &App) {
    raise_pair(a);
}

// ---------------------------------------------------------------- 毛玻璃

fn set_accent(hwnd: HWND, state: i32, alpha: u8) -> bool {
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
fn apply_glass(a: &mut App) {
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

fn apply_mode(a: &mut App) {
    // 布局模式必须能点得到：临时关掉穿透，底板当拖拽把手
    set_panel_click_through(a, !a.cfg.layout_mode);
    // 窗口尺寸可能变了，把时间日期块重新夹一次，别让它跑出可视区
    normalize_clock_offset(a);
    apply_glass(a);
    raise_pair(a);
    render_and_present(a);
    a.cfg.save();
}

fn set_visible(a: &mut App, on: bool) {
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

// ---------------------------------------------------------------- 渲染

/// 一帧的绘制输入。把 `App` 的字段拆开借，避免 `&mut frame` 和 `&fonts` 打架。
struct RC<'a> {
    cfg: &'a Config,
    fonts: &'a Fonts,
    frame: &'a mut Frame,
    list: &'a [Quote],
    qi: usize,
    qn: usize,
    m: f32,
    brush: HBRUSH,
    /// **生效动效**（已把「竖版降级为静态」「放不下转跑马灯」两件事算进去）。
    /// 绘制只认它，不再自己去看 `cfg.anim`。
    anim: Anim,
    /// 格言整块的 x 偏移（过渡滑入，px）。**两趟必须同值** ——
    /// 掩码趟与阴影趟几何一致是铁律（`band_l` 那个坑的教训）。
    slide: i32,
    /// 格言通道覆盖率 k（过渡/呼吸，0..1）。阴影趟据此把**格言自己的**
    /// 阴影按同一比例压暗（写灰度 `255(1-k)`）；时钟/日期的阴影不动。
    /// 竖版入场错峰时它只代表「整块进度」，逐列的灰度看 `col_gray`。
    qk: f32,
    /// **竖版入场错峰专用**：阴影趟按列写灰度，`(首字形下标, 字数, 灰度色)`，
    /// 列序自右向左。空 = 整块一个灰度（横版 / 退场 / 空档 / 稳态）。
    col_gray: &'a [(usize, usize, u32)],
    /// 出处列的阴影灰度（0 = 纯黑不缩放）。竖版过渡时出处随格言淡，
    /// 它的阴影同步；横版 / 无出处 / 非过渡期为 0。
    author_gray: u32,
    /// 强制逐字绘制（不合批）的**逐帧**覆盖。`None` = 听 `gfx::DevFlags`
    /// （生产路径走这条，`DevFlags::no_batch` 恒 `false` ⇒ 合批）。
    ///
    /// 走显式参数而不是读 `gfx::DevFlags`：`DevFlags` 是进程级一次性的
    /// （`OnceLock`），而这条断言要在**同一个进程里**先出合批帧、再出逐字帧
    /// 做比对 —— 一次性开关表达不了「按帧切」。见 `build_frame` 的文档。
    ///
    /// 必须是 `Option` 而不是 `bool`：`false` 会被 `draw_glyphs` 读成
    /// 「明确要求合批」，把 `DevFlags` 的回退路径整个短路掉 —— 那样
    /// `--perf` 的 `no_batch` 档位就会静默失效，而画面看不出任何异常。
    no_batch: Option<bool>,
}

/// 一帧的**逐带 / 逐列调制**（PR3）。`build_frame` 算一次，compose 与两趟
/// 绘制共用 —— 和 `quote_mod` 同一条铁律：一帧之内不许有两份几何/调制。
struct FrameMod {
    /// compose 的逐带调制表
    bands: gfx::BandTable,
    /// 阴影趟的逐列灰度（格言列，列序自右向左）
    col_grays: Vec<(usize, usize, u32)>,
    /// 出处列阴影灰度（0 = 不缩放）
    author_gray: u32,
}

/// 阴影灰度：覆盖率 `k` ⇒ 阴影位图写 `255(1-k)`（白底黑字的「白」变灰）。
/// k=1 时退化为 0（纯黑），与旧路径逐位相同。
fn shadow_gray(k: f32) -> u32 {
    let v = (255.0 * (1.0 - k)).round().clamp(0.0, 255.0) as u32;
    v * 0x0101_0101
}

/// 算这一帧的调制表。四条规则：
///
/// 1. **横版 / 非过渡 / 退场 / 空档** = 一条通带（格言 × qk）—— 与 PR1 逐位相同；
/// 2. **竖版入场（Arrive）且列数 > 1** = 每列一条带，右列先亮（`col_k`）；
/// 3. **出处列**（竖版、日期通道上唯一随格言走的一列）= 单独一条日期带，
///    k 取**最左列**（阅读序最末：出处在主文之后落定）；
/// 4. 阴影不走 compose：逐列灰度由阴影趟写（竖版按字分段，零额外提交），
///    `shad` 位图保持「阴影缩放的唯一真相源」。
fn frame_mod(a: &App, ql: &QuoteLayout, cb: &ClockBox, qk: f32) -> FrameMod {
    let qk_u8 = (qk * 255.0).round().clamp(0.0, 255.0) as u8;
    let vertical = a.cfg.orient == Orient::Vertical && !ql.col_ranges.is_empty();
    if !vertical {
        return FrameMod {
            bands: gfx::BandTable::uniform(qk_u8, 255),
            col_grays: Vec::new(),
            author_gray: 0,
        };
    }

    let ncols = ql.col_ranges.len();
    let staggered = a.trans.phase() == transition::Phase::Arrive && ncols > 1;

    // 出处列（日期通道的带）：k = 最左列（阅读序最末）
    let author = cb.sig.iter().find(|c| c.is_author);
    let author_k = a.trans.col_k(ncols - 1, ncols);
    let author_gray = if author.is_some() && author_k < 1.0 {
        shadow_gray(author_k)
    } else {
        0
    };

    let mut col_grays = Vec::new();
    // **先建出处带、再建列带**：`get` 先匹配先赢，而末列瓦片为了兜住阴影
    // 外溢会左扩到窗口边缘、恰好盖住出处区 —— 出处带后 push 就永远排不上号
    // （实测：比值全 1.000，出处只动了阴影没动颜色）。
    // 非错峰相位 `col_k == quote_k`，出处随整块 qk 走即可，不需要单独的带。
    let bands = if staggered {
        let mut t = gfx::BandTable::empty();
        if let Some(ac) = author {
            t.push(
                ac.x,
                ac.x + cb.sig_w.max(1),
                255,
                (author_k * 255.0).round().clamp(0.0, 255.0) as u8,
            );
        }
        for (i, &(g0, g1)) in ql.col_ranges.iter().enumerate() {
            let k = a.trans.col_k(i, ncols);
            let (bx0, bx1) = ql.col_x[i];
            // 首列右缘 / 末列左缘扩到窗口边缘：阴影外溢 2px 也要落在某一列里
            let (x0, x1) = if i == 0 {
                (bx0, a.cfg.w)
            } else if i == ncols - 1 {
                (0, bx1)
            } else {
                (bx0, bx1)
            };
            t.push(x0, x1, (k * 255.0).round().clamp(0.0, 255.0) as u8, 255);
            col_grays.push((g0, g1, shadow_gray(k)));
        }
        t
    } else {
        gfx::BandTable::uniform(qk_u8, 255)
    };
    FrameMod {
        bands,
        col_grays,
        author_gray,
    }
}

/// 出一帧并上屏。**会清掉脏标记** —— 刚出的这一帧就是当前状态。
///
/// 第一件事是夹时间日期块的偏移。缩放拖动刻意把这件事推到这里（见
/// `apply_resize` 的注释）：每条鼠标消息只做 `SetWindowPos` + 标脏，
/// 夹取按 8ms 节拍跟着出帧走一次。代价是拖动中时钟块可能越界一帧，
/// 换来的是单条消息成本从 1156µs 降到 ~450µs。
fn render_and_present(a: &mut App) {
    normalize_clock_offset(a);
    // 先取纯量，避免下面拆借 a 之后再回头借它
    let op = a.text_alpha();
    build_frame(a, op);
    let (x, y) = (a.cfg.x, a.cfg.y);
    let hwnd = a.text;
    a.frame.present(hwnd, x, y);
    a.last_present = std::time::Instant::now();
    a.renders = a.renders.wrapping_add(1);
    a.dirty = false;
}

/// 标脏：内容变了，等下一次出帧。
fn mark_dirty(a: &mut App) {
    a.dirty = true;
}

/// 需要就出帧，但 `DRAG_FRAME_MS` 内最多一次。
///
/// 拖动时鼠标消息动辄每秒几百上千条，而每条都重绘的话——
/// 单是重绘就 700µs——消息只会在队列里越积越多。这里把「状态接收」和
/// 「画面输出」解耦：状态每条消息都即时更新（微秒级，绝无积压），
/// 画面按固定节拍出，且**最后一帧保证是最终状态**（`force`）。
fn render_if_due(a: &mut App, force: bool) {
    if !a.dirty {
        return;
    }
    if !force && a.last_present.elapsed().as_millis() < DRAG_FRAME_MS {
        return;
    }
    render_and_present(a);
}

/// 记下「窗口该去哪」，并按节拍把它落到窗口上 —— 移动布局的最优路径。
///
/// 位置变化不影响任何一个像素：掩码、合成、预乘结果都和窗口在哪无关。
/// 所以「挪」这件事的全部工作就是**改窗口位置**，一次内容提交都不该发生。
/// 旧实现在每个 `WM_MOVE` 里跑一遍整帧重绘（~800µs）＋上屏，
/// 这些工作对「位置变了」这件事毫无贡献。
fn request_move(a: &mut App, x: i32, y: i32) {
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
fn apply_geometry(a: &mut App, resize: bool, force: bool) {
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
fn apply_resize(a: &mut App, x: i32, y: i32, w: i32, h: i32) {
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

/// 只把一帧渲染进 `frame`，不上屏。自检路径也走这里，保证测的就是真管线。
///
/// 一帧只跑**两趟** GDI 绘制，却拿到**四条**掩码通道：
///
/// ```text
///   ① 黑底，一趟画全「时钟 + 分隔线 + 格言 + 日期」
///        ├─ CH_CLOCK 通道（byte0）→ 时钟 / 分隔线 / 布局提示
///        ├─ CH_DATE  通道（byte1）→ 日期（或竖版落款）
///        └─ CH_QUOTE 通道（byte2）→ 格言
///   ② 白底黑字再画一遍同样的内容（整体位移 off）→ 阴影覆盖率
///            ↓
///        compose(主色, 日期色, 格言色) → 预乘 ARGB（就地覆写）
/// ```
///
/// ① 之所以能一趟画三种颜色：**GDI 的抗锯齿是逐通道线性的** —— 用「只有某
/// 一位为满值」的颜色画到同一块 32bpp 位图上，三个字节互不串扰，合成时读回
/// 就是三条掩码（见 `gfx::CH_*`）。同一条通道只能有一个颜色，所以颜色本身
/// 仍然只在 `compose` 里乘，掩码层永远是「白字黑底」。
///
/// 这比旧实现省掉了「每类内容各一趟：清屏 + 画 + 采集」—— 7 趟 `w*h` 变 3 趟，
/// 且**逐像素等价**（合批绘制那条断言守着）。
fn build_frame(a: &mut App, opacity: u8) {
    build_frame_ex(a, opacity, None)
}

/// `build_frame` 的可显式指定「逐字绘制」的版本。
///
/// 只给 `--selftest` 的合批 A/B 断言用：它要在**同一个进程里**先出一帧合批、
/// 再出一帧逐字，把两块掩码逐像素比对。`gfx::DevFlags` 是 `OnceLock`
/// （进程级、一次性置位），表达不了「按帧切」，所以这条路必须是显式参数。
///
/// 生产代码一律调 `build_frame` —— 那儿的 `None` 让 `draw_glyphs` 回退到
/// `DevFlags`（生产构建里恒 `false`）。
fn build_frame_ex(a: &mut App, opacity: u8, no_batch: Option<bool>) {
    let shadow = a.cfg.shadow;
    let (color, date_color) = (a.cfg.color, a.cfg.date_color);
    let quote_color = a.cfg.quote_paint_color();
    let shk: u8 = if shadow { 170 } else { 0 };

    // 时间日期块的几何先算出来：绘制要用它排版，wndproc 要用它做拖动命中。
    // 命中判定在 wndproc 里，那里没有 DC 去量文字，所以位置必须缓存在 App 上。
    let cb = layout_clock(a);

    // ---- 排版：**一帧只算一次**，三趟绘制共用这一份定位表。
    // 这条不变量是本项目最贵的教训（`band_l` 那段注释就是它的墓碑）：
    // 只要哪一趟自己重算一份，两趟就会用不同的几何，阴影立刻错位。
    let ql = if a.cfg.show_quote {
        frame_layout(a, &cb)
    } else {
        QuoteLayout::default()
    };

    // ---- 竖版落款跟排（仿古条幅，2026-10-15 老板报「落款和正文脱节」）。
    // 主文右起贴右缘，落款条的**右缘**贴主文块左缘 —— 整个卷轴是紧凑的
    // 一块，而不是落款钉在窗口左缘、窗口一宽就和正文隔着几百像素空白。
    // 只挪落款、**不重排主文**：主文右起贴 br，与版心左缘无关；frame_mod
    // 在错峰相位本来就把末列带扩到 x=0、出处带先 push 先赢（见 6605 的
    // 实测教训），瓦片不需要跟着落款走。反侧重排窄版心反而会截列。
    let cb = hug_sign(&a.cfg, cb, &ql);
    let crect = RECT {
        left: cb.x,
        top: cb.y,
        right: cb.x + cb.w.max(CLOCK_MIN_GRAB),
        bottom: cb.y + cb.h.max(CLOCK_MIN_GRAB),
    };

    // 连缩到 70% 都放不下 → 横版转跑马灯。**竖版没有跑马灯**（横向滚动 ×
    // 竖排字格 = 横竖混排的怪物），就地带省略号收尾。
    a.force_marquee =
        ql.overflow && a.cfg.show_quote && a.cfg.orient == Orient::Horizontal;
    let eff_anim = if a.force_marquee {
        Anim::Marquee
    } else {
        a.cfg.eff_anim()
    };

    // 记下这一帧用的是哪一刻的时钟 —— `needs_frame` 拿它判断下一拍要不要重绘
    a.clock_stamp = clock_stamp(&a.cfg);
    a.clock_rect = crect;

    // 格言调制（覆盖率 k + 滑入偏移）：过渡优先于呼吸。
    // **必须在拆借 `a` 之前取**；两趟绘制与 compose 共用这一对值。
    let (qk, slide) = a.quote_mod();
    // 逐带 / 逐列调制（PR3）：竖版入场错峰 + 出处随格言。横版 / 稳态下
    // 它就是一条通带，与 PR1 逐位相同。
    let fmod = frame_mod(a, &ql, &cb, qk);

    let App {
        cfg,
        fonts,
        frame,
        list,
        qi,
        qn,
        m,
        brush,
        ..
    } = a;
    let (w, h) = (cfg.w, cfg.h);
    if !frame.ensure(w, h) {
        return;
    }
    frame.set_shadow(shadow);

    let mut rc = RC {
        cfg,
        fonts,
        frame,
        list,
        qi: *qi,
        qn: *qn,
        m: *m,
        brush: *brush,
        anim: eff_anim,
        slide,
        qk,
        col_gray: &fmod.col_grays,
        author_gray: fmod.author_gray,
        no_batch,
    };

    // ① 掩码趟：时钟 / 分隔线 / 日期 / 格言**一趟画完**。
    //
    // 三类内容靠**不同的颜色通道**区分（`gfx::CH_CLOCK` / `CH_DATE` /
    // `CH_QUOTE`），所以「日期单独一条通道、关掉时还要显式清空」那套没有了 ——
    // 整块位图每帧重清，没画到的通道就是 0。
    // 旧实现这条路上跑的是 3 次「清屏 + 画 + 采集」，现在只剩 1 次清屏。
    {
        let dc = rc.frame.begin_ink();
        paint_main(&mut rc, dc, &cb, Pass::Ink, &ql);
        paint_date(&mut rc, dc, &cb, Pass::Ink);
    }

    // ② 阴影趟：白底黑字，把**同一批字形整体位移 `(off, off)`** 再画一遍。
    //
    // 这一趟看着是白花 —— 整帧最贵的一项就是它。试过把它换成「从掩码位移
    // 采样」（省一整趟绘制 + 一次清屏），但 A/B 证明那样与重新光栅化
    // **不等价**（2% 像素不同、最大差 65 级灰度、且全朝变暗方向），
    // 详见 `gfx::Frame` 的文档注释。性能的大头本来也不在这里。
    //
    // 阴影关掉时不必清这块位图：`compose` 见到 `shadow_k == 0` 根本不读它。
    if shadow {
        let dc = rc.frame.begin_shadow();
        paint_main(&mut rc, dc, &cb, Pass::Shadow, &ql);
        paint_date(&mut rc, dc, &cb, Pass::Shadow);
    }

    // ③ 预乘合成：颜色在这里才乘进去，**就地**覆写掩码位图。
    //
    // 格言色不再需要版心矩形分区 —— 它本来就画在自己那条通道上。
    // `bands` 逐带调制：通带（255 = 满覆盖率）与旧路径逐位相同；
    // 竖版入场时每列一条带（右列先亮）+ 出处列一条日期带。
    rc.frame
        .compose(opacity, shk, &fmod.bands, color, date_color, quote_color);
}

/// 阴影位移（px）。
///
/// 阴影趟按它整体平移字形；`--selftest` 那条「阴影不越界」断言也读**同一个
/// 函数** —— 两处各算一份迟早会走偏（这正是当年 `band_l` 那个坑的教训）。
fn shadow_offset(cfg: &Config) -> i32 {
    ((2.0 * cfg.scale as f32 / 100.0).round() as i32).max(0)
}

/// 时间日期块的几何。
///
/// 位置 = **默认位置**（左侧、垂直居中）+ 用户拖动偏移，再夹进窗口内。
/// 夹取这一步是「位置可拖动」和「拖完找不回来」之间的分界线：
/// 无论怎么拖，整块内容始终至少完整可见地留在窗口里。
#[derive(Clone)]
struct ClockBox {
    show: bool,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    time: Option<Vec<u16>>,
    date: Option<Vec<u16>>,
    time_h: i32,
    gap: i32,
    /// 竖版落款：逐列小字，**右起**（第 0 列最靠主文）。空 = 横版。
    ///
    /// 竖版下 `time` / `date` 留空，内容全在这里 —— 落款是一串**竖排**小字，
    /// 跟横版「一行时间、一行日期」是两种几何，硬塞进同两个字段只会互相打结。
    sig: Vec<SigCol>,
    /// 竖版落款的单元格高
    sig_cell: i32,
    /// 竖版落款的列宽（日期字体一个汉字的实测宽）。出处列随格言过渡时
    /// 要用它圈出日期通道上的 x 带（PR3）。
    sig_w: i32,
}

/// 竖版落款里的一列。
#[derive(Clone)]
struct SigCol {
    s: Vec<u16>,
    x: i32,
    /// true → 走主色通道（mask）；false → 走日期色通道（dmask）
    main_color: bool,
    /// true → **出处列**：换句过渡时随格言一起淡（PR3，老板定的语义）。
    /// 时间列是时钟（主职、静帧锚点）、日期列是落款配角，都不动。
    is_author: bool,
}

/// 量一次时间日期块的几何（无缓存，见 `layout_clock`）。
fn layout_clock_raw(cfg: &Config, fonts: &Fonts, dc: HDC, quote: &Quote) -> ClockBox {
    if cfg.orient == Orient::Vertical {
        return layout_sign(cfg, fonts, dc, quote);
    }
    let scale = cfg.scale as f32 / 100.0;
    let (w, h) = (cfg.w, cfg.h);
    let pad = ((24.0 * scale).round() as i32).max(10);

    let time = if cfg.show_clock {
        Some(wide(&fmt_clock(&local_time(), cfg.seconds)))
    } else {
        None
    };
    let date = if cfg.show_date {
        Some(wide(&fmt_date(&local_time())))
    } else {
        None
    };

    let mut cb = ClockBox {
        show: false,
        x: pad,
        y: 0,
        w: 0,
        h: 0,
        time,
        date,
        time_h: 0,
        gap: 0,
        sig: Vec::new(),
        sig_cell: 0,
        sig_w: 0,
    };
    if cb.time.is_none() && cb.date.is_none() {
        return cb;
    }

    let clock_face = if w as f32 >= 430.0 * scale {
        fonts.clock
    } else {
        fonts.clock_small
    };
    let mut bw = 0;
    let mut date_h = 0;

    unsafe { SelectObject(dc, clock_face as HGDIOBJ) };
    if let Some(s) = &cb.time {
        let (cw, ch) = gfx::measure_text(dc, s);
        bw = bw.max(cw);
        cb.time_h = ch;
    }
    unsafe { SelectObject(dc, fonts.date as HGDIOBJ) };
    if let Some(s) = &cb.date {
        let (dw, dh) = gfx::measure_text(dc, s);
        bw = bw.max(dw);
        date_h = dh;
    }

    let gap = if cb.time.is_some() && cb.date.is_some() {
        (6.0 * scale).round() as i32
    } else {
        0
    };
    cb.gap = gap;
    cb.w = bw;
    cb.h = cb.time_h + gap + date_h;
    cb.show = true;

    // 默认位置 + 拖动偏移，然后夹进窗口
    let max_x = (w - bw).max(0);
    let max_y = (h - cb.h).max(0);
    cb.x = (pad + cfg.clock_dx).clamp(0, max_x);
    cb.y = ((h - cb.h) / 2 + cfg.clock_dy).clamp(0, max_y);
    cb
}

/// 量一次时间日期块的几何，**带结果缓存**。
///
/// 缩放拖动时每条鼠标消息都要夹一次时钟偏移，而 `ClockBox` 的 `w/h` 只由
/// 字面量和字号决定，与窗口尺寸无关。所以按输入指纹缓存整个 `ClockBox`：
/// 命中就直接返回，一次 `measure_text` / `SelectObject` 都不做。
///
/// `--perf` 实测这是缩放拖动成本的大头：不缓存 1052µs/条（顶到 1000Hz
/// 鼠标的 1000µs 预算），命中后 ~300µs。
///
/// # 指纹必须覆盖全部输入
///
/// `w/h`（夹取范围）、`scale`（默认位置）、`seconds`（字面量宽度）、
/// `show_clock`/`show_date`/`orient`（量什么）、`qi`（竖版出处列宽）、
/// `clock_dx/dy`（夹取会回写它们）、`font_epoch`（字号/字体）。
///
/// 换句走 `qi`、换字号走 `font_epoch` —— 两条自动覆盖，不必在每个
/// 写配置的地方手工插一句作废。漏一项的表现是「改了没生效」，极难复现，
/// 所以宁可多列几个分量。
fn layout_clock(a: &mut App) -> ClockBox {
    let key = (
        a.cfg.w,
        a.cfg.h,
        a.cfg.scale,
        i32::from(a.cfg.seconds),
        a.qi as i32,
        u32::from(a.cfg.show_clock),
        u32::from(a.cfg.show_date),
        match a.cfg.orient {
            Orient::Horizontal => 0u32,
            Orient::Vertical => 1u32,
        },
        a.cfg.clock_dx,
        a.cfg.clock_dy,
        a.font_epoch,
    );
    if key == a.clock_norm_key {
        if let Some(cb) = &a.clock_cached {
            return cb.as_ref().clone();
        }
    }
    // `frame.dc()` 是 `&self` 借用，先把裸指针取出来再借 `a` —— 否则同时
    // 有 `&App`（拿 DC）和 `&mut App`（写缓存），借用检查器不放行。
    let dc = a.frame.dc();
    let cb = layout_clock_raw(&a.cfg, &a.fonts, dc, &a.list[a.qi]);
    a.clock_cached = Some(Box::new(cb.clone()));
    a.clock_norm_key = key;
    cb
}

/// 竖版形态 A 的**落款条**：窗口左侧一列列竖排小字，主文竖排在它右边。
///
/// 列序**自右向左**：时间 → 出处 → 日期（先读到的靠右，和主文同向）。
/// 时间走**主色**通道，它首先是块表 —— 扫一眼就得看见；
/// 出处与日期走**日期色**通道，它们是落款，本来就该比主角弱一档。
/// 两条通道都是现成的 ⇒ 不新增第四条掩码，`Frame` / `compose` 一行都不用动。
fn layout_sign(cfg: &Config, fonts: &Fonts, dc: HDC, quote: &Quote) -> ClockBox {
    let scale = cfg.scale as f32 / 100.0;
    let (w, h) = (cfg.w, cfg.h);
    let pad = ((24.0 * scale).round() as i32).max(10);

    // 右起第一列是时间（主色），然后出处、日期
    // 第三分量 = 是否出处列（过渡时随格言淡，PR3）
    let mut cols: Vec<(Vec<u16>, bool, bool)> = Vec::new();
    if cfg.show_clock {
        cols.push((wide(&fmt_clock(&local_time(), cfg.seconds)), true, false));
    }
    if !quote.author.is_empty() {
        // 出处原样用，**不套书名号** —— `display()` 里也是原样，
        // 而内置库里已经有「《周易》」这种自带书名号的出处，再套一层就成了《《周易》》
        cols.push((wide(&quote.author), false, true));
    }
    if cfg.show_date {
        cols.push((wide(&fmt_date(&local_time())), false, false));
    }

    let mut cb = ClockBox {
        show: false,
        x: 0,
        y: 0,
        w: 0,
        h: 0,
        time: None,
        date: None,
        time_h: 0,
        gap: 0,
        sig: Vec::new(),
        sig_cell: 0,
        sig_w: 0,
    };
    if cols.is_empty() {
        return cb;
    }

    // 拿日期字体量一个汉字：列宽 / 单元格高 / 列距全从这里推，不按比例估算
    unsafe { SelectObject(dc, fonts.date as HGDIOBJ) };
    let (cw, ch) = gfx::measure_text(dc, &wide("国"));
    let (cw, ch) = (cw.max(1), ch.max(1));
    let cell = ((ch as f32) * 1.30).round() as i32;
    let ncol = cols.len() as i32;
    // 列距先取自然值（字宽的 1.45 倍，竖排才不挤），但**整条落款不许超过窗口的 34%**：
    // 落款的字号跟着 scale 一起涨，而窗口不一定跟着涨 —— 不设上限的话，
    // 大字号 + 窄窗口下它会一路把主文挤到只剩一列（实测 240×520@150% 就发生过）。
    let natural = ((cw as f32) * 1.45).round() as i32;
    let cap_w = ((w as f32) * 0.34).round() as i32;
    let pitch = if ncol > 1 {
        natural.min(((cap_w - cw).max(cw / 2)) / (ncol - 1))
    } else {
        natural
    }
    .max(6);

    let rows = cols
        .iter()
        .map(|(s, _, _)| s.len().saturating_sub(1))
        .max()
        .unwrap_or(0) as i32;
    let bw = (ncol - 1) * pitch + cw;
    let bh = (rows - 1).max(0) * cell + ch;

    // 默认位置：贴左侧、垂直居中；再叠用户拖动偏移，夹进窗口
    let x0 = pad;
    let y0 = (h - bh) / 2;
    let max_x = (w - bw).max(0);
    let max_y = (h - bh).max(0);
    let bx = (x0 + cfg.clock_dx).clamp(0, max_x);
    let by = (y0 + cfg.clock_dy).clamp(0, max_y);

    cb.sig = Vec::with_capacity(cols.len());
    for (i, (s, mc, au)) in cols.into_iter().enumerate() {
        let x = bx + (ncol - 1 - i as i32) * pitch;
        cb.sig.push(SigCol {
            s,
            x,
            main_color: mc,
            is_author: au,
        });
    }
    cb.x = bx;
    cb.y = by;
    cb.w = bw;
    cb.h = bh;
    cb.sig_cell = cell;
    cb.sig_w = cw;
    cb.show = true;
    cb
}

/// 最小可用版心宽度。低于它就换一种摆法，而不是整段不画。
const MIN_QUOTE_BAND: i32 = 96;
/// 堆叠摆法下，版心至少要有这个高度
const MIN_QUOTE_H: i32 = 44;

/// 竖版**落款跟排**（仿古条幅，2026-10-15 老板报「落款和正文脱节」）：
/// 主文右起贴右缘，落款条的**右缘**贴主文块左缘 —— 整个卷轴是紧凑的
/// 一块，而不是落款钉在窗口左缘、窗口一宽就和正文隔着几百像素空白。
///
/// 只挪落款、**不重排主文**：主文右起贴 br，与版心左缘无关；frame_mod
/// 在错峰相位本来就把末列带扩到 x=0、出处带先 push 先赢（见 6605 的
/// 实测教训），瓦片不需要跟着落款走。反侧重排窄版心反而会截列。
/// 用户拖动偏移（clock_dx）照样叠加；贴过去会出左界 / 会压到主文时
/// 退回 layout_sign 的原位。`build_frame` 与自检都走这一个函数。
fn hug_sign(cfg: &Config, mut cb: ClockBox, ql: &QuoteLayout) -> ClockBox {
    if cfg.orient != Orient::Vertical || !cb.show || ql.is_empty() {
        return cb;
    }
    let scale = cfg.scale as f32 / 100.0;
    let pad = ((24.0 * scale).round() as i32).max(10);
    let band = frame_band(cfg, &cb);
    // 主文右起贴 br ⇒ 主文块左缘 = br - ext_w（band 元组是 (bl, bt, br, bb)）
    let main_left = band.2 - ql.extent.0;
    let want_bx = (main_left - pad - cb.w + cfg.clock_dx)
        .clamp(pad, (cfg.w - cb.w - pad).max(pad));
    if want_bx > cb.x {
        let dx = want_bx - cb.x;
        cb.x = want_bx;
        for col in &mut cb.sig {
            col.x += dx;
        }
    }
    cb
}

/// 横版里「格言能用的那块」。
///
/// 三段式，从好到差，**每一段都有实宽** ——
/// 这正是旧实现里 `band_r > band_l + 20` 那条守卫的替代品：那条守卫在
/// 版心不够宽时**整段跳过绘制**，于是 scale≥205 时格言在屏幕上完全消失、
/// 一点提示都没有（`--dump` 实测：200 还能看到一个「自」字，210 起全空）。
///
/// ① 时钟右侧 —— 本来唯一的那条路
/// ② 时钟下方 —— 横向挤不开时改成上下堆叠，仍然是一块干净的矩形
/// ③ 整窗 —— 连堆叠都放不下（字号大到窗口装不下任何东西）：会与时钟重叠，
///    但用户至少**看得见字、知道是窗口太小**，而不是面对一片空白猜哪儿去了
fn quote_band(cfg: &Config, cb: &ClockBox) -> (i32, i32, i32, i32) {
    let scale = cfg.scale as f32 / 100.0;
    let (w, h) = (cfg.w, cfg.h);
    let pad = ((24.0 * scale).round() as i32).max(10);
    let full = (pad, pad, w - pad, h - pad);
    if !cb.show {
        return full;
    }
    let right_l = pad + cb.w + 2 * pad;
    if w - pad - right_l >= MIN_QUOTE_BAND {
        return (right_l, pad, w - pad, h - pad);
    }
    let top = cb.y + cb.h + pad / 2;
    if h - pad - top >= MIN_QUOTE_H {
        return (pad, top, w - pad, h - pad);
    }
    full
}

/// 竖版主文的版心：落款条右侧那一整片。
fn quote_band_v(cfg: &Config, cb: &ClockBox) -> (i32, i32, i32, i32) {
    let scale = cfg.scale as f32 / 100.0;
    let (w, h) = (cfg.w, cfg.h);
    let pad = ((24.0 * scale).round() as i32).max(10);
    let full = (pad, pad, w - pad, h - pad);
    if !cb.show {
        return full;
    }
    let l = (cb.x + cb.w + pad).min(w - pad - 1).max(pad);
    (l, pad, w - pad, h - pad)
}

/// 版心是不是退化到了「只能整窗画」这一步（横版）。
///
/// 退化不是 bug，是**窗口真的装不下**了：时钟/日期块已经把可用的排版空间吃光。
/// 但用户不该对着一片叠在一起的字猜哪儿出问题 —— 调用方据此给一句提示。
fn band_degenerate(cfg: &Config, cb: &ClockBox) -> bool {
    if !cb.show || cfg.orient == Orient::Vertical {
        return false;
    }
    let scale = cfg.scale as f32 / 100.0;
    let (w, h) = (cfg.w, cfg.h);
    let pad = ((24.0 * scale).round() as i32).max(10);
    let right_l = pad + cb.w + 2 * pad;
    let stacked_top = cb.y + cb.h + pad / 2;
    !(w - pad - right_l >= MIN_QUOTE_BAND) && !(h - pad - stacked_top >= MIN_QUOTE_H)
}

/// 当前这一帧的格言版心（横竖各一套规则）。
///
/// 抽出来是为了让**自检也能读到同一个版心** —— 自检要是自己再推一遍，
/// 验的就不是真正跑的那条路了。`build_frame` 与 `--dump` 走的都是它。
fn frame_band(cfg: &Config, cb: &ClockBox) -> (i32, i32, i32, i32) {
    match cfg.orient {
        Orient::Vertical => quote_band_v(cfg, cb),
        Orient::Horizontal => quote_band(cfg, cb),
    }
}

/// 当前这一帧的排版结果。
fn frame_layout(a: &App, cb: &ClockBox) -> QuoteLayout {
    let band = frame_band(&a.cfg, cb);
    // 度量时字距必须为 0 —— 字距由排版层自己加进定位表
    unsafe { SetTextCharacterExtra(a.frame.dc(), 0) };
    // 竖版主文**不带出处尾巴**：「正文　——　作者」在竖排里会变成一列
    // 孤零零的旋转竖杠 + 与落款条重复的作者名，还把诗文的断列搅乱。
    // 古籍正文从不夹署名 —— 出处归落款条（仿古排版，2026-10-15 老板拍板）。
    // 横版维持 display()（跑马灯一行带出处）。
    let q = &a.list[a.qi];
    let text = if a.cfg.orient == Orient::Vertical {
        q.text.clone()
    } else {
        q.display()
    };
    layout_quote(&a.cfg, &a.fonts, a.frame.dc(), band, &text)
}

/// 排一次格言，走「缩字阶梯」。
///
/// 候选档 100% → 85% → 70%（老板定的下限）。**每档都换字体对象重新量**，
/// 不按比例缩 `adv`：不同字号的 GDI 取整不一样，估算出来的宽度和实际画出来的对不上。
///
/// 返回的定位表在**同一帧的三趟绘制里共用** —— 排版只算一次，这是本项目
/// 最贵的教训（`band_l` 那段注释就是它的墓碑）。
fn layout_quote(
    cfg: &Config,
    fonts: &Fonts,
    dc: HDC,
    band: (i32, i32, i32, i32),
    text: &str,
) -> QuoteLayout {
    let s = wide(text);
    let chars: Vec<u16> = s[..s.len() - 1].to_vec();
    let spacing = cfg.quote_spacing();
    // 度量时字距必须是 0：`SetTextCharacterExtra` 会被 `GetTextExtentExPointW`
    // 计入且严格线性，字距由排版层自己加，混进度量会得到「末字多算一份」。
    unsafe { SetTextCharacterExtra(dc, 0) };

    let steps = if cfg.shrink_fit {
        QUOTE_SHRINK_STEPS.len()
    } else {
        1
    };
    let mut last = QuoteLayout {
        shrink: QUOTE_SHRINK_STEPS[0],
        ..Default::default()
    };
    for (k, &pct) in QUOTE_SHRINK_STEPS.iter().enumerate().take(steps) {
        let font = fonts.quote_at(k);
        unsafe { SelectObject(dc, font as HGDIOBJ) };
        let Some(adv) = gfx::char_advances(dc, &s) else {
            break;
        };
        let (cw, ch) = gfx::measure_text(dc, &wide("国"));
        let m = Metrics::new(cw.max(1), ch.max(1));
        let mut ql = layout::layout(&chars, &adv, m, cfg.orient, band, spacing);
        ql.shrink = pct;
        if !ql.overflow {
            return ql;
        }
        last = ql;
    }
    last
}

/// 一趟绘制。两条路的差别只有三处：**写哪块位图**、**用什么颜色**、
/// **要不要整体位移**。
///
/// `Ink` 往 `ink` 上写三条彩色通道（`CH_*`）；`Shadow` 往 `shad` 上白底黑字
/// 地再画一遍、整体位移 `(off, off)`。阴影**不能**改成「从掩码位移采样」——
/// 那不是等价变换，理由见 `gfx::Frame` 的文档注释。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    Ink,
    Shadow,
}

impl Pass {
    /// 是不是阴影趟。
    fn is_shadow(self) -> bool {
        matches!(self, Pass::Shadow)
    }

    /// 本趟写字用的颜色。`Ink` 用调用方给的**通道色**（决定这个字形落在
    /// 哪条掩码上）；`Shadow` 一律写黑 —— 阴影位图是白底，覆盖率 =
    /// `255 - 写入值`，写任何非零的通道色都会让阴影整体变淡。
    fn paint_color(self, ch: u32) -> u32 {
        match self {
            Pass::Ink => ch,
            Pass::Shadow => 0,
        }
    }

    /// 本趟的字形整体位移。阴影就是「同一批字形往右下挪一点再画一遍」。
    fn offset(self, cfg: &Config) -> (i32, i32) {
        match self {
            Pass::Ink => (0, 0),
            Pass::Shadow => {
                let o = shadow_offset(cfg);
                (o, o)
            }
        }
    }
}

/// 主内容：时钟 + 分隔线 + 格言 + 布局提示。
///
/// 每类内容落在掩码位图的**哪一条颜色通道**上，由这里决定（见 `gfx::CH_*`），
/// 跟最终显示的颜色无关 —— 颜色只在 `compose` 里乘。
///
/// `ql` 是这一帧**唯一的**排版结果（`build_frame` 算好传进来）。
fn paint_main(rc: &mut RC, dc: HDC, cb: &ClockBox, pass: Pass, ql: &QuoteLayout) {
    let cfg = rc.cfg;
    let shadow_pass = pass == Pass::Shadow;
    let (ox, oy) = pass.offset(cfg);
    let scale = cfg.scale as f32 / 100.0;
    let (w, h) = (cfg.w, cfg.h);
    let pad = ((24.0 * scale).round() as i32).max(10);

    // ---- 时钟（横版：左上角的时间块；竖版：落款里走主色通道的那一列）
    if cfg.orient == Orient::Vertical {
        unsafe { SelectObject(dc, rc.fonts.date as HGDIOBJ) };
        for c in cb.sig.iter().filter(|c| c.main_color) {
            gfx::draw_vtext(
                dc,
                &c.s,
                c.x + ox,
                cb.y + oy,
                cb.sig_cell,
                pass.paint_color(gfx::CH_CLOCK),
            );
        }
    } else if cb.show {
        if let Some(s) = &cb.time {
            let clock_face = if w as f32 >= 430.0 * scale {
                rc.fonts.clock
            } else {
                rc.fonts.clock_small
            };
            unsafe { SelectObject(dc, clock_face as HGDIOBJ) };
            gfx::draw_text(
                dc,
                s,
                cb.x + ox,
                cb.y + oy,
                pass.paint_color(gfx::CH_CLOCK),
            );
        }
    }

    // ---- 分隔线。只有横版才有（竖版是落款 / 主文两块，中间本就有留白）。
    //
    // 它的位置只由时钟块的**宽度**决定（不跟着拖动偏移跑），
    // 这样拖动时间不会连带把格言带挤窄。这也是「两趟必须算出同一个几何」
    // 那个老坑的现场：`band_l` 由这里的 `dx` 推出来，把整段 if 用
    // `!shadow_pass` 包住就会让阴影趟拿到一个更宽的裁剪区。
    // 现在绘制用的几何全部来自 `ql`（版心已在排版时定死），这里的 `if`
    // 只决定「画不画那根线」—— 而线本身**不带阴影**（它是版式装饰，不是字）。
    if cfg.orient == Orient::Horizontal && cfg.show_quote && cb.show && !shadow_pass {
        let dx = pad + cb.w + pad;
        if dx < w - pad {
            let line = RECT {
                left: dx,
                top: pad + h / 4,
                right: dx + 1,
                bottom: h - pad,
            };
            unsafe { FillRect(dc, &line, rc.brush) };
        }
    }

    // ---- 格言
    if cfg.show_quote {
        let (bl, bt, br, bb) = match cfg.orient {
            Orient::Vertical => quote_band_v(cfg, cb),
            Orient::Horizontal => quote_band(cfg, cb),
        };
        // 版心退化 = 时钟/日期块已经把窗口吃光。这时**不能**把格言压在时钟上：
        // 两块都会变得不可读，观感比「格言没了」更差。时钟是这块挂件的主职，
        // 所以这时只画一句明确提示（下面那段），格言留到用户把窗口放大或把字号降回来。
        let degenerate = band_degenerate(cfg, cb);
        if degenerate {
            // 有意什么都不画 —— 提示在下面统一画
        } else if matches!(rc.anim, Anim::Marquee) {
            // 跑马灯：整串滚动，排版表用不上，仍走整串绘制
            let s = wide(&rc.list[rc.qi].display());
            let s2 = wide(&rc.list[rc.qn].display());
            unsafe { SelectObject(dc, rc.fonts.quote as HGDIOBJ) };
            unsafe { SetTextCharacterExtra(dc, cfg.quote_spacing()) };
            let (tw, th) = gfx::measure_text(dc, &s);
            let gap = (130.0 * scale).round() as i32;
            let ty = (h - th) / 2 + oy;
            let x1 = (br as f32 - rc.m).round() as i32;
            let x2 = x1 + tw + gap;
            // 裁剪区跟着做**同一个**竖直位移：阴影趟里字往下挪了 `oy`，
            // 裁剪区也要往下挪 `oy`，这样「裁掉字的哪一块」两趟完全一致。
            // 只挪 y 不挪 x 是沿旧实现的做法（`ox` 只有 2~3px，落在裁剪区内的
            // 部分本就完整可见）。
            gfx::with_clip(dc, bl, bt + oy, br, bb + oy, |dc| {
                gfx::draw_text(dc, &s, x1 + ox, ty, pass.paint_color(gfx::CH_QUOTE));
                gfx::draw_text(dc, &s2, x2 + ox, ty, pass.paint_color(gfx::CH_QUOTE));
            });
            unsafe { SetTextCharacterExtra(dc, 0) };
        } else if !ql.is_empty() {
            // 静态 / 淡入淡出 / 呼吸：照定位表画（折行、缩字、标点归位都在表里）。
            // 内部按「同一基线的相邻原文段」合批成一次 `PolyTextOutW` —— 见 `draw_glyphs`。
            // `(ox, oy)` 就是阴影位移：定位表本身**不含**它，两趟共用同一份表。
            //
            // **不加裁剪区**：版心是排版层的输入，字已经排在里面了；再裁一刀
            // 只会把竖版最右一列那些「墨迹越过步进框 1~2px」的字切掉
            // （`--dump` 实测：300×660 竖版右缘一列被切 9 个像素，
            // 而横版因为版心留白宽、切不到）。裁剪是给**滚动**内容准备的，
            // 静态内容不需要。
            //
            // 阴影趟里，格言**自己的**阴影按同一个 k 压暗：写灰度 `255(1-k)`。
            // GDI 抗锯齿逐通道线性 ⇒ 每个覆盖率层级同比缩放，与 compose 侧的
            // `quote_k` 对齐（文字淡下去、它的影子同步淡下去）。
            // `k=1` 时灰度退化为 0（黑），与旧路径**逐位相同**；时钟/日期的
            // 阴影仍是 `paint_color` 的纯黑，不参与缩放。
            //
            // 竖版入场错峰时灰度**按列**给（`rc.col_gray`，右列先亮）——
            // 竖版每字本就各自成段，按列换颜色零额外提交。
            let color = if shadow_pass && rc.qk < 1.0 {
                shadow_gray(rc.qk)
            } else {
                pass.paint_color(gfx::CH_QUOTE)
            };
            let groups = if shadow_pass && !rc.col_gray.is_empty() {
                Some(rc.col_gray)
            } else {
                None
            };
            let font = rc.fonts.quote_at(ql.font_tier());
            let rot = rc.fonts.quote_rot;
            let spacing = cfg.quote_spacing();
            // 滑入偏移**只加在格言上**：时钟/日期/底板是静帧，不跟着漂。
            // 偏移量由 `transition` 统一给，两趟同值（几何一致铁律）。
            // 静态内容**不加裁剪区**（「PR3·竖版最右列完整可见」钉着这条：
            // 裁一刀会把竖版右列墨迹越出步进框的那 1~2px 切掉）。
            gfx::draw_glyphs(dc, font, rot, ql, ox + rc.slide, oy, color, spacing, groups, rc.no_batch);
        }

        // 那句提示必须说出来 —— 让用户对着一片叠着的字自己猜，
        // 正是这次要根除的毛病（旧实现是格言整条消失、零提示）。
        //
        // 提示用**固定 13px** 的临时字体：这是一句给用户看的话，不该跟着 scale
        // 一起放大到装不下（300% 下 hint 字体是 36px，整句要 600 多 px）。
        //
        // `!shadow_pass`：这是一句**界面提示**，不是版式内容，不带阴影
        // （和下面布局模式那几条一个道理）。
        if degenerate && !shadow_pass {
            let f = gfx::make_font(13, FW_NORMAL, &rc.fonts.face);
            unsafe { SelectObject(dc, f as HGDIOBJ) };
            let hint = wide("窗口装不下 · 请放大窗口或降低字号");
            let (hw, _) = gfx::measure_text(dc, &hint);
            // 提示要画在**主色通道**上、而且给满值：掩码强度就是那个字节的值，
            // 给个 0xC0 之类「看起来够亮」的颜色，提取出来 alpha 只有 75%，
            // 叠到棋盘格上只差几级灰度 —— 画了等于没画。
            // 也不能顺手写成 `WHITE`：那是三条通道一起点亮，
            // 在「通道即语义」的新布局里，这一片像素会同时归属三种颜色。
            gfx::draw_text(dc, &hint, (w - hw).max(0) / 2, 2, gfx::CH_CLOCK);
            unsafe { DeleteObject(f as HGDIOBJ) };
        }
    }

    // ---- 布局模式的提示与边框。整块都是**界面元素**，阴影趟一律不画
    // （旧实现同样如此：阴影只属于版式内容，UI 提示投影会显得脏）。
    if cfg.layout_mode && !shadow_pass {
        let hint = wide("布局模式  ·  拖空白处移动 / 拖时间移动时间 / 四边缩放  ·  Ctrl+Alt+E 退出");
        unsafe { SelectObject(dc, rc.fonts.hint as HGDIOBJ) };
        let (hw, _) = gfx::measure_text(dc, &hint);
        gfx::draw_text(
            dc,
            &hint,
            (w - hw).max(0) / 2,
            (h - pad - 2).max(0),
            gfx::ch_dim(0x8C),
        );

        // 时间日期块的位置框：告诉用户「这块是可以单独拖的」
        if cb.show {
            let t = 1;
            let eb = unsafe { CreateSolidBrush(gfx::ch_dim(0x8A)) };
            let r = RECT {
                left: (cb.x - 4).max(0),
                top: (cb.y - 4).max(0),
                right: (cb.x + cb.w + 4).min(w),
                bottom: (cb.y + cb.h + 4).min(h),
            };
            for e in [
                RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + t },
                RECT { left: r.left, top: r.bottom - t, right: r.right, bottom: r.bottom },
                RECT { left: r.left, top: r.top, right: r.left + t, bottom: r.bottom },
                RECT { left: r.right - t, top: r.top, right: r.right, bottom: r.bottom },
            ] {
                unsafe { FillRect(dc, &e, eb) };
            }
            unsafe { DeleteObject(eb as HGDIOBJ) };
        }

        let t = 1;
        let edge = gfx::ch_dim(0x96);
        let rects = [
            RECT { left: 0, top: 0, right: w, bottom: t },
            RECT { left: 0, top: h - t, right: w, bottom: h },
            RECT { left: 0, top: 0, right: t, bottom: h },
            RECT { left: w - t, top: 0, right: w, bottom: h },
        ];
        let eb = unsafe { CreateSolidBrush(edge) };
        for r in &rects {
            unsafe { FillRect(dc, r, eb) };
        }
        unsafe { DeleteObject(eb as HGDIOBJ) };
    }
}

/// 日期（横版）/ 落款（竖版）。它画在**日期通道**上，所以天生就与时钟、
/// 格言分得开 —— 不再需要「单独一条掩码通道 + 关掉时显式清空」那套。
fn paint_date(rc: &mut RC, dc: HDC, cb: &ClockBox, pass: Pass) {
    if !cb.show {
        return;
    }
    let (ox, oy) = pass.offset(rc.cfg);

    if rc.cfg.orient == Orient::Vertical {
        unsafe { SelectObject(dc, rc.fonts.date as HGDIOBJ) };
        for c in cb.sig.iter().filter(|c| !c.main_color) {
            // 出处列（`is_author`）的阴影随格言的 k 压暗；日期列不缩放。
            // 非阴影趟 / 非过渡期两者都是纯通道色（`author_gray == 0`）。
            let color = if pass.is_shadow() && c.is_author && rc.author_gray != 0 {
                rc.author_gray
            } else {
                pass.paint_color(gfx::CH_DATE)
            };
            gfx::draw_vtext(dc, &c.s, c.x + ox, cb.y + oy, cb.sig_cell, color);
        }
        return;
    }

    let Some(s) = &cb.date else { return };
    unsafe { SelectObject(dc, rc.fonts.date as HGDIOBJ) };
    let y = cb.y + (if cb.time.is_some() { cb.time_h + cb.gap } else { 0 });
    gfx::draw_text(dc, s, cb.x + ox, y + oy, pass.paint_color(gfx::CH_DATE));
}


// ---------------------------------------------------------------- 动效

/// 全屏检测的轮询间隔。`SHQueryUserNotificationState` 是跨进程的 shell 查询，
/// 有真实成本；而「进 / 出全屏」本身就是秒级事件，1Hz 完全够。
/// 定时器是 25Hz，也就是每 25 拍才查一次。
const FS_CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// `QUERY_USER_NOTIFICATION_STATE` 的可读名字（`--probe` 报当前状态用）。
fn quns_label(s: u32) -> &'static str {
    match s {
        QUNS_NOT_PRESENT => "NOT_PRESENT",
        QUNS_BUSY => "BUSY",
        QUNS_RUNNING_D3D_FULL_SCREEN => "RUNNING_D3D_FULL_SCREEN",
        QUNS_PRESENTATION_MODE => "PRESENTATION_MODE",
        QUNS_ACCEPTS_NOTIFICATIONS => "ACCEPTS_NOTIFICATIONS",
        QUNS_QUIET_TIME => "QUIET_TIME",
        QUNS_APP => "APP",
        _ => "未知",
    }
}

/// 全屏自动隐藏要做的动作。
#[derive(PartialEq, Eq, Debug)]
enum FsAction {
    Hide,
    Restore,
    None,
}

/// 全屏隐藏的判定（**纯函数**，可对全部边界组合下断言而不造一个真全屏应用）。
///
/// * `(true, false)` 且用户要可见 → `Hide`：进全屏，藏。
/// * `(true, _)` 但用户本来就藏着 → `None`：不用记 `fs_hidden`，
///   否则退出全屏时会弹回用户亲手关掉的窗口。
/// * `(false, true)` → `Restore`：退出全屏，恢复。
/// * `(true, true)` → `None`：已经藏着了，别再 ShowWindow 一次。
fn fs_decide(fullscreen: bool, already_hidden: bool, user_visible: bool) -> FsAction {
    match (fullscreen, already_hidden) {
        (true, false) if user_visible => FsAction::Hide,
        (false, true) => FsAction::Restore,
        _ => FsAction::None,
    }
}

/// 全屏应用（游戏 / 全屏播放器）运行时自动隐藏，退出后恢复。
///
/// 判定与执行分离：`fs_decide` 是纯函数（测试覆盖全组合），这里只负责
/// 1Hz 节流、查询和状态记账。
fn fullscreen_guard(a: &mut App, now: std::time::Instant) {
    // 用户关了这个功能：不查询、不动作（查询有成本，关了就该是零成本）。
    // 关闭开关时若正藏着，`on_menu` 会立刻恢复 —— 不存在「关了还藏着」的残留。
    if !a.cfg.auto_hide_fs {
        return;
    }
    if now.duration_since(a.fs_last) < FS_CHECK_EVERY {
        return;
    }
    a.fs_last = now;
    let st = crate::ffi::user_notification_state();
    let fullscreen = st == Some(QUNS_RUNNING_D3D_FULL_SCREEN);
    a.fs_active = fullscreen;
    match fs_decide(fullscreen, a.fs_hidden, a.cfg.visible) {
        FsAction::Hide => {
            temp_hide(a, true);
            a.fs_hidden = true;
        }
        FsAction::Restore => {
            temp_hide(a, false);
            a.fs_hidden = false;
        }
        FsAction::None => {}
    }
}

/// 不经过 `cfg.visible` 的临时显隐。**不动配置、不写盘。**
///
/// 全屏藏窗口是系统的临时状态，用户意图必须原样留在 `cfg.visible` 里 ——
/// 走 `set_visible` 会把「用户本来开着」这个事实覆盖掉，退出全屏后就
/// 再也分不清该不该恢复。
fn temp_hide(a: &App, hide: bool) {
    unsafe {
        if hide {
            ShowWindow(a.text, SW_HIDE);
            ShowWindow(a.panel, SW_HIDE);
        } else {
            // 恢复时底板只在「该显形」时显形 —— 与 `set_visible` 同一套判据。
            // 少这一个判断，纯文字模式下就会把一片看不见却拦鼠标的底板放出来
            // （README「踩过的坑」第 7 条同款事故）。
            if a.should_show_panel() {
                ShowWindow(a.panel, SW_SHOWNOACTIVATE);
            }
            ShowWindow(a.text, SW_SHOWNOACTIVATE);
        }
    }
}

fn tick(a: &mut App) {
    let now = std::time::Instant::now();
    let dt = (now - a.last_tick).as_secs_f32().min(0.25);
    a.last_tick = now;
    // 全屏检测放在 grab 早退**之前**：拖动中也可能撞上「用户切进了游戏」，
    // 那时候同样该藏。1Hz 节流在函数内部，不每拍都去问 shell。
    fullscreen_guard(a, now);
    // 拖动期间一拍都不出：手的动作优先。
    //
    // 定时器 25Hz，跑马灯每拍是 ~780µs 的整帧重绘，而它跟鼠标消息抢的是
    // **同一条 UI 线程时间线** —— 拖动时它带来的唯一效果就是让窗口更跟不上光标。
    // 冻结动效这段时间用户根本不会注意（注意力全在位置上），松手 `end_grab`
    // 会强制补一帧，画面立刻回到最新状态。
    if a.grab != Grab::None {
        // 但**位置要补**：节流可能把最后一次移动欠着（用户停手后就没有下一次
        // 鼠标事件来推动它了）。拖动期间定时器被收紧到 `DRAG_TICK_MS`，
        // 正好和位置节拍同频，这一拍就把欠的那一格落下去。
        apply_geometry(a, false, false);
        return;
    }
    // PR2 悬停暂停：光标停在挂件上 = 强阅读意图，冻结轮播/滚动。
    // 判定读**真实窗口矩形**（家规：不拿 cfg 顶替）；tick 是轮询、无消息可用，
    // `GetCursorPos` 不违反「命中判定用消息自带坐标」（那条针对拖动事件序列）。
    // 拖动中（上方早退）不判 —— 手在拖窗口，谈不上「在读」。
    {
        let mut pt: POINT = unsafe { zeroed() };
        let mut rc: RECT = unsafe { zeroed() };
        unsafe {
            GetCursorPos(&mut pt);
            GetWindowRect(a.text, &mut rc);
        }
        let inside =
            pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom;
        // 25Hz 一拍 ~40ms；clamp 已把 dt 压在 0.25s 内，毫秒换算不会爆
        let (paused, hs) = transition::hover_decide(a.hover, inside, (dt * 1000.0) as u32);
        a.hover = hs;
        a.hover_paused = paused;
    }
    // `step` 尾声已经含「需要才出帧」：它按 `needs_frame` 标脏、按 `DRAG_FRAME_MS` 节流。
    // 这里**不能**再无脑补一帧 —— 那就把空转判定整个绕过去了：定时器 25Hz，
    // 每拍白跑一遍 ~790µs 的整帧重绘，一秒 5 万 µs 纯烧，
    // 而且拖动时正好从鼠标消息嘴里抢时间片。静态画面（尤其拖动中）本来就该一帧不出。
    step(a, dt);
    // 兜住 `step` 的早退分支（词库为空时它直接 return）留下的脏标记 —— 没脏时这是个空调用。
    render_if_due(a, false);
}

/// 只推进动效状态，不碰窗口。抽出来是为了让自检能脱离定时器跑动效。
fn step(a: &mut App, dt: f32) {
    let scale = a.cfg.scale as f32 / 100.0;
    let n = a.list.len();
    if n == 0 {
        return;
    }

    // **生效动效**的唯一判定：竖版一律静态（跑马灯在竖排字格里没有意义），
    // 格言放不下时横版转跑马灯。`needs_frame` 读的是同一个表达式。
    let anim = if a.force_marquee {
        Anim::Marquee
    } else {
        a.cfg.eff_anim()
    };
    match anim {
        Anim::Marquee => {
            // 跑马灯与过渡互斥：「放不下转跑马灯」可能把在飞的过渡半路截断，
            // 落定它，否则切回来那一下会接着半个动画放
            if a.trans.is_active() {
                a.trans.settle();
            }
            let band_w = marquee_band_w(&a.cfg);
            if !a.primed {
                // 首帧直接贴到带左缘，用户一启动就能读到，而不是等它慢慢滑进来
                a.primed = true;
                a.m = band_w;
            }
            if a.hover_paused {
                // PR2 悬停暂停：光标停在挂件上（连续 ≥300ms 确认过「想读」），
                // m 不推进 —— 跑马灯停下让人读。needs_frame 同步排除了出帧，
                // 暂停期间一帧不出（空转不出帧铁律）。
            } else {
                a.m += a.cfg.speed as f32 * dt;
                if a.m >= band_w + a.cur_w as f32 {
                    let gap = 130.0 * scale;
                    a.m -= a.cur_w as f32 + gap;
                    a.qi = a.qn;
                    a.qn = (a.qn + 1) % n;
                    a.cur_w = measure_quote(a, &a.list[a.qi]);
                }
            }
        }
        Anim::Static | Anim::Fade => {
            // 过渡状态机先行：Leave/Gap/Arrive 期间 dwell 不累计。
            // `Swap` 在 Leave→Gap 的边界产出 —— 旧句 alpha 归 0 的同一拍换句，
            // 这是「无硬切」的机制保证（换句永远伴随一段过渡）。
            match a.trans.step(dt) {
                transition::StepOut::Swap => {
                    advance(a);
                    a.phase = 0.0;
                }
                transition::StepOut::Settled => a.phase = 0.0,
                transition::StepOut::None => {}
            }
            // dwell 只在停留期累计，时长 ∝ 当前这句的字数
            // （5 字/s 从容读 + 50% 余量，夹在 4~10s）
            if a.trans.phase() == transition::Phase::Dwell {
                // PR2 悬停暂停：光标停着就别换句（暂停无上限，12.3#3）。
                // 过渡中不冻结 —— 过渡本就比任何 dwell 短，冻结它反而拖泥带水；
                // 这里的分支只在 Dwell 相位进入，过渡期天然不受影响。
                if a.hover_paused {
                    // dwell 相位冻结，不累计、不启动过渡
                } else {
                    a.phase += dt;
                    let chars = a.list[a.qi].display().chars().count();
                    // 库里只有一句（或空）时**不启动过渡**：Leave→Swap→Arrive 会
                    // 把同一句淡出去再淡进来，观感是「自己闪自己」（PR1.1）。
                    // 一句格言的 dwell 没有终点可言，就让它静静待着。
                    if n >= 2 && a.phase >= transition::dwell_secs(chars) {
                        // Fade 是纯淡入淡出（无位移）；Static 横版滑入 0.5em。
                        // **竖版恒 0**：竖排的阅读流向是自上而下、列自右向左，
                        // 横向滑入与字格方向不正交，会把列间节律打乱（PR1.1
                        // 修复实施偏差：原先只看 anim 不看 orient）。
                        let slide = if anim == Anim::Fade || a.cfg.orient == Orient::Vertical {
                            0
                        } else {
                            transition::slide_px(a.cfg.quote_font_px())
                        };
                        a.trans.start_leave(slide);
                        a.phase = 0.0;
                    }
                }
            }
        }
        Anim::Breathe => {
            // 呼吸只推进相位时钟；k 由 `quote_mod` 在出帧时按 phase 现算。
            // 它**只作用于格言通道**（见 quote_mod）—— 时钟/日期/底板
            // 不再被绑架，毛玻璃也不再跟着起伏（那正是旧 P0 的一半）。
            a.phase += dt;
        }
    }

    // 画面会不会变由上一步的状态决定，出帧交给节流统一安排
    if needs_frame(a) {
        mark_dirty(a);
    }
    render_if_due(a, false);
}

/// 时钟的「刻度」：显示秒就精确到秒，否则到分。
///
/// 静态画面下用它回答「这一拍有没有像素会变」—— 不显示秒时一帧能顶一分钟，
/// 没必要 25fps 空转重绘（也让拖动时少一个抢 CPU 的）。
fn clock_stamp(cfg: &Config) -> u64 {
    if !cfg.show_clock && !cfg.show_date {
        return 0; // 时间日期都没显示，这一格永远是常量
    }
    let t = crate::util::local_time();
    if cfg.seconds {
        ((t.wHour as u64) << 16) | ((t.wMinute as u64) << 8) | t.wSecond as u64
    } else {
        ((t.wHour as u64) << 8) | t.wMinute as u64
    }
}

/// 这一拍画面会不会自己变。不会变就别出帧 —— 空转的那一帧既费 CPU，
/// 又会在拖动时和鼠标消息抢时间片。
fn needs_frame(a: &App) -> bool {
    if clock_stamp(&a.cfg) != a.clock_stamp {
        return true;
    }
    // 过渡进行中：相位时钟在走，每帧都要出（560ms 的窗口期，可接受）
    if a.trans.is_active() {
        return true;
    }
    // 与 `step` 读**同一个**表达式：生效动效 + 「放不下转跑马灯」。
    // 两处要是各写各的，就会出现「跑马灯在滚但不出帧」或「静态却每帧重绘」。
    let anim = if a.force_marquee {
        Anim::Marquee
    } else {
        a.cfg.eff_anim()
    };
    match anim {
        // 跑马灯只影响格言；格言关掉了就只剩时钟，和静态没区别。
        // **悬停暂停期间一帧不出**（PR2）：m 冻结了画面不变，再逐帧重绘
        // 就是暂停版本的空转烧 CPU —— 需要帧的判定必须和 step 的冻结同步。
        Anim::Marquee => a.cfg.show_quote && !a.hover_paused,
        // 呼吸动的是格言通道的 k（时钟/日期/底板不动），必须逐帧出。
        // **减动模式下呼吸已降级为静态**（quote_mod 最早返回 1.0），
        // 再逐帧空转就是纯烧 CPU（PR1.1）。
        // 呼吸**不冻结**于悬停（PR2，12.3#2）：呼吸是氛围不是阅读内容，
        // 冻了反而像坏了。
        Anim::Breathe => !a.trans.reduced(),
        // 静态/淡入淡出：dwell 只用于计时换句；换句那一刻过渡会自己标脏，
        // 过渡结束（落定）后一拍都不多出 —— 空转不出帧铁律在这里守着
        Anim::Static | Anim::Fade => false,
    }
}

fn measure_quote(a: &App, q: &Quote) -> i32 {
    let s = wide(&q.display());
    // 借用 frame 只是拿它的 DC 量一下，不改内容
    unsafe { SelectObject(a.frame.dc(), a.fonts.quote as HGDIOBJ) };
    gfx::measure_text(a.frame.dc(), &s).0
}

/// 跑马灯滚动带的有效宽度（消掉左右内边距）。
/// `step` 的轮转判定和 `rebuild_fonts` 的落点是同一把尺子，抽出来防止两处走偏。
fn marquee_band_w(cfg: &Config) -> f32 {
    let scale = cfg.scale as f32 / 100.0;
    (cfg.w - 2 * ((24.0 * scale).round() as i32)).max(80) as f32
}

fn advance(a: &mut App) {
    let n = a.list.len();
    a.qi = a.qn;
    a.qn = (a.qn + 1) % n;
    a.cur_w = measure_quote(a, &a.list[a.qi]);
    a.m = 0.0;
    // 换句了，这一帧必须出（静态动效下它是一帧里唯一会变的东西）
    mark_dirty(a);
}

// ---------------------------------------------------------------- 热键

/// 全部全局热键：`(id, 虚拟键)`，统一 `Ctrl+Alt+<键>`。
///
/// 抽成模块级常量是为了让「注册」和「`--probe` 的期望值」**同源**：
/// 曾经期望值是文案里写死的 `17`，加了收藏热键之后它变成了
/// 「18/17 注册成功」—— 一个自己打自己脸的输出。
const HOTKEY_TABLE: [(i32, u32); 19] = [
    (HK_TOGGLE, VK_H),
    (HK_ANIM, VK_M),
    (HK_MODE, VK_T),
    (HK_LAYOUT, VK_E),
    (HK_UP, VK_UP),
    (HK_DOWN, VK_DOWN),
        (HK_PREV, VK_LEFT),
        (HK_NEXT, VK_RIGHT),
        (HK_TOPMOST, VK_P),
        (HK_HIDE, VK_S),
        (HK_QUOTES, VK_R),
        (HK_CLOCK, VK_C),
        (HK_DATE, VK_D),
        (HK_SIZE_UP, VK_OEM_PLUS),
        (HK_SIZE_DOWN, VK_OEM_MINUS),
        (HK_COLOR, VK_K),
        (HK_FAV, VK_F),
        (HK_CONFIG, VK_O),
        (HK_QUIT, VK_X),
    ];

/// 期望注册成功的热键数。`register_hotkeys` 与 `--probe` 共用这一份。
const HOTKEY_EXPECT: usize = HOTKEY_TABLE.len();

fn register_hotkeys(a: &mut App) {
    const M: u32 = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
    for (id, vk) in HOTKEY_TABLE {
        let ok = unsafe { RegisterHotKey(a.panel, id, M, vk) } != 0;
        if ok {
            a.hotkeys.push(id);
        } else {
            // 记下失败码：1409 表示被**别的进程**占着（本机同时跑着另一个实例
            // 时必然如此），那是「测不了」，跟「坏了」是两件事。
            a.hotkey_err = unsafe { GetLastError() };
        }
    }
}

fn unregister_hotkeys(a: &App) {
    for id in &a.hotkeys {
        unsafe { UnregisterHotKey(a.panel, *id) };
    }
}

fn on_hotkey(a: &mut App, id: i32) {
    match id {
        HK_TOGGLE => set_visible(a, !a.cfg.visible),
        HK_HIDE => set_visible(a, false),
        HK_ANIM => {
            let mut next = a.cfg.anim.next();
            // 竖版不支持跑马灯：热键循环时直接跳过这一档，
            // 否则会出现「按了没反应」（cfg 变了，生效动效还是静态）
            if a.cfg.orient == Orient::Vertical && next == Anim::Marquee {
                next = next.next();
            }
            a.cfg.anim = next;
            a.phase = 0.0;
            a.m = 0.0;
            // 切动效时把在飞的过渡落定：否则从静态切走的那一下
            // 会接着上个模式的半个动画放
            a.trans.settle();
            a.cfg.save();
            render_and_present(a);
        }
        HK_MODE => {
            a.cfg.mode = if a.cfg.mode == Mode::Glass {
                Mode::TextOnly
            } else {
                Mode::Glass
            };
            apply_mode(a);
        }
        HK_LAYOUT => {
            a.cfg.layout_mode = !a.cfg.layout_mode;
            apply_mode(a);
        }
        HK_UP => bump_opacity(a, 12),
        HK_DOWN => bump_opacity(a, -12),
        HK_PREV | HK_NEXT => {
            let n = a.list.len();
            a.qi = if id == HK_NEXT {
                (a.qi + 1) % n
            } else {
                (a.qi + n - 1) % n
            };
            a.qn = (a.qi + 1) % n;
            a.m = 0.0;
            a.phase = 0.0;
            // 手动换句是 **0 帧直切**：用户主动操作要即时响应，不等动画。
            // 在飞的过渡直接落定，不会留下半段影子。
            a.trans.settle();
            a.cur_w = measure_quote(a, &a.list[a.qi]);
            render_and_present(a);
        }
        HK_TOPMOST => {
            a.cfg.always_on_top = !a.cfg.always_on_top;
            apply_topmost(a);
            a.cfg.save();
        }
        HK_QUOTES => {
            a.list = quotes::library();
            if a.qi >= a.list.len() {
                a.qi = 0;
            }
            a.qn = (a.qi + 1) % a.list.len();
            a.m = 0.0;
            a.cur_w = measure_quote(a, &a.list[a.qi]);
            render_and_present(a);
        }
        HK_FAV => favorite(a),
        HK_CLOCK => on_menu(a, MI_CLOCK),
        HK_DATE => on_menu(a, MI_DATE),
        HK_SIZE_UP => on_menu(a, MI_SCALE_UP),
        HK_SIZE_DOWN => on_menu(a, MI_SCALE_DOWN),
        HK_COLOR => on_menu(a, MI_COLOR),
        HK_CONFIG => config_ui::open(a),
        HK_QUIT => quit(a),
        _ => {}
    }
}

fn bump_opacity(a: &mut App, delta: i32) {
    a.cfg.opacity = (a.cfg.opacity + delta).clamp(30, 255);
    apply_glass(a);
    render_and_present(a);
    a.cfg.save();
}

// ---------------------------------------------------------------- 托盘

fn add_tray(a: &mut App) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = a.panel;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = LoadIconW(null_mut(), IDI_APPLICATION_RES);
        let tip = wide("桌面格言 · 左键菜单 / Ctrl+Alt+M 换动效");
        for (i, c) in tip.iter().take(127).enumerate() {
            nid.szTip[i] = *c;
        }
        nid.szTip[127] = 0;
        a.tray_ok = Shell_NotifyIconW(NIM_ADD, &nid) != 0;
        if !a.tray_ok {
            // 注意：Shell_NotifyIcon 失败时**未必**会 SetLastError，
            // 所以 err=0 的含义是「shell 没给理由」，不等于「成功」。
            a.tray_err = GetLastError();
        }
    }
}

fn del_tray(a: &App) {
    if !a.tray_ok {
        return;
    }
    unsafe {
        let mut nid: NOTIFYICONDATAW = zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = a.panel;
        nid.uID = 1;
        Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// 弹一条托盘气泡。**不抢焦点、不弹窗** —— 挂件类程序的反馈必须是这种
/// 「看得见但不用处理」的形态，`MessageBox` 会把用户正在打字的输入框焦点抢走。
///
/// 托盘没挂上时静默失败：那条路径下热键和主功能都还是好的，
/// 为一条提示去降级反而更吵。
fn toast(a: &App, msg: &str) {
    if !a.tray_ok {
        return;
    }
    unsafe {
        let mut nid: NOTIFYICONDATAW = zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = a.panel;
        nid.uID = 1;
        nid.uFlags = NIF_INFO;
        let info = wide(msg);
        for (i, c) in info.iter().take(255).enumerate() {
            nid.szInfo[i] = *c;
        }
        nid.szInfo[255] = 0;
        let title = wide("桌面格言");
        for (i, c) in title.iter().take(63).enumerate() {
            nid.szInfoTitle[i] = *c;
        }
        nid.szInfoTitle[63] = 0;
        // NIIF_INFO = 1（信息图标）。不用 NIIF_USER 之类 —— 那要自定义图标，
        // 而这里只是告知结果。
        nid.dwInfoFlags = 1;
        Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

// ---------------------------------------------------------------- 收藏

/// 重载格言库之后，`qi` 该落在哪。
///
/// 直接 `qi.min(len - 1)` 是**错的**：收藏发生在列表**尾部**，索引并不越界，
/// 但 round-robin 交织之后位置全变了 —— `qi` 会安安静静地指向另一句，
/// 用户看到的就是「点了收藏，画面自己跳了」。
/// 所以必须按**文本**把同一条找回来；只有真的找不到了（用户把文件改坏了）
/// 才退回夹取。
fn keep_index(cur: &str, list: &[crate::quotes::Quote], fallback: usize) -> usize {
    list.iter()
        .position(|q| q.display() == cur)
        .unwrap_or_else(|| fallback.min(list.len().saturating_sub(1)))
}

/// 收藏**当前正在显示的这句**：复制到剪贴板 + 加入轮播。
///
/// 挂件常态是鼠标穿透的，点不到它本身，所以入口只能是托盘菜单和
/// `Ctrl+Alt+F`。两个入口走的是同一条路，不存在「点了没反应」的分裂。
///
/// 两件事**互不牵连**：剪贴板可能被别的窗口占着（`OpenClipboard` 失败），
/// 那是「复制没成」；写盘可能因为目录只读而失败，那是「收藏没成」。
/// 任何一半失败，另一半照做，气泡里如实说。
fn favorite(a: &mut App) {
    favorite_at(a, &crate::util::data_dir());
}

/// `favorite` 的可测核心：数据目录由调用方给。
///
/// 抽出来和 `quotes::library_from` 同一个理由 —— 直接测真实
/// `%APPDATA%\DeskGeyan` 的话，测试会**真的往用户的格言库里写东西**，
/// 而收藏夹文件一旦被测试污染就再也分不清哪些是用户自己收藏的。
/// `--favtest` 指向临时目录；运行时才用真实目录。
fn favorite_at(a: &mut App, dir: &std::path::Path) {
    if a.list.is_empty() {
        // 空库不该发生（内置 72 条兜底），但 `list` 是运行时状态，
        // 别在这里panic —— 静默返回比崩掉强。
        return;
    }
    let q = a.list[a.qi].clone();

    // ① 复制。窗口句柄传底板：托盘气泡的回调也挂在它上面，同一扇门。
    let copied = clipboard::copy_text(a.panel, &q.display());

    // ② 加入收藏夹（幂等：已在夹里就不重复写）
    let added = favorites::append(dir, &q).unwrap_or(false);

    // ③ 真的加了 → 重载列表，让这句**立刻**进入轮播候选。
    // 关键：`qi` 必须仍指向同一条句子。直接 `a.qi = a.qi.min(len-1)`
    // 是不够的 —— 追加发生在列表尾部，索引虽然不越界，但 round-robin
    // 交织后位置全变，qi 会 silently 指向另一句，用户就觉得「画面自己跳了」。
    if added {
        let cur = q.display();
        a.list = quotes::library();
        a.qi = keep_index(&cur, &a.list, a.qi);
        a.qn = (a.qi + 1) % a.list.len();
        // **不碰** m / phase / alpha / cur_w：这句还在显示，位置和宽度都没变，
        // 重置滚动相位会让画面无谓地跳一下。收藏对当前画面必须无感。
    }

    // ④ 反馈
    let msg = match (added, copied) {
        (true, true) => "已收藏，并复制到剪贴板",
        (true, false) => "已收藏（剪贴板被占用，复制失败）",
        (false, true) => "这句已在收藏夹里（已复制）",
        (false, false) => "这句已在收藏夹里（剪贴板被占用）",
    };
    toast(a, msg);
}

fn menu_add(menu: *mut c_void, id: u32, text: &str, checked: bool, enabled: bool) {
    let s = wide(text);
    let mut flags = MF_STRING;
    if checked {
        flags |= MF_CHECKED;
    }
    if !enabled {
        flags |= MF_GRAYED;
    }
    unsafe { AppendMenuW(menu, flags, id as usize, s.as_ptr()) };
}

fn sep(menu: *mut c_void) {
    let s = wide("");
    unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, s.as_ptr()) };
}

fn show_menu(a: &mut App) {
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }

        menu_add(menu, MI_TOGGLE, "显示 / 隐藏  (Ctrl+Alt+H)", a.cfg.visible, true);
        // 设置面板是**全部配置的唯一入口**：菜单里不再重复任何设置项。
        // 旧版五个子菜单（版式 / 显示内容 / 外观 / 动效 / 各项微调）与面板
        // 逐项重复，两处各画一份状态纯属维护税；菜单只留高频操作。
        menu_add(
            menu,
            MI_CONFIG,
            "设置…（字体 / 显示 / 外观 / 版式 / 动效）  (Ctrl+Alt+O)",
            false,
            true,
        );
        sep(menu);
        menu_add(menu, MI_PREV, "上一条格言  (Ctrl+Alt+←)", false, true);
        menu_add(menu, MI_NEXT, "下一条格言  (Ctrl+Alt+→)", false, true);
        // 收藏：挨着「上/下一条」放 —— 三条都是针对**当前这句**的操作，
        // 用户找的时候在同一处。
        menu_add(menu, MI_FAV, "收藏当前句子  (Ctrl+Alt+F)", false, true);
        sep(menu);
        menu_add(menu, MI_RELOAD, "重新载入格言 / 配置  (Ctrl+Alt+R)", false, true);
        menu_add(menu, MI_RESET, "挂件回到右上角", false, true);
        menu_add(menu, MI_CLOCK_RESET, "重置时间日期位置", false, a.clock_visible());
        sep(menu);
        menu_add(menu, MI_QUIT, "退出", false, true);

        let mut pt: POINT = zeroed();
        GetCursorPos(&mut pt);

        // WS_EX_NOACTIVATE 的窗口弹菜单必须临时允许被激活，
        // 否则菜单不会因为点击别处而收起（经典坑）
        let ex = GetWindowLongPtrW(a.panel, GWL_EXSTYLE) as u32;
        SetWindowLongPtrW(a.panel, GWL_EXSTYLE, (ex & !WS_EX_NOACTIVATE) as isize);
        SetForegroundWindow(a.panel);

        let cmd = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            pt.x,
            pt.y,
            0,
            a.panel,
            null(),
        ) as u32;

        PostMessageW(a.panel, WM_NULL, 0, 0);
        SetWindowLongPtrW(a.panel, GWL_EXSTYLE, ex as isize);
        DestroyMenu(menu);

        if cmd != 0 {
            on_menu(a, cmd);
        }
    }
}

/// 执行一条菜单命令。
///
/// **托盘菜单和设置面板共用这一条路**（面板从 `config_ui` 里调它）：菜单项和面板
/// 控件都只是「发命令的界面」，语义只在这里定义一次。新增一个可改的配置项时，
/// 加一条 `MI_*` + 一个 `match` 分支，面板和菜单就同时支持了。
pub(crate) fn on_menu(a: &mut App, cmd: u32) {
    match cmd {
        MI_TOGGLE => set_visible(a, !a.cfg.visible),
        MI_GLASS => {
            a.cfg.mode = Mode::Glass;
            apply_mode(a);
        }
        MI_TEXT => {
            a.cfg.mode = Mode::TextOnly;
            apply_mode(a);
        }
        MI_LAYOUT => {
            a.cfg.layout_mode = !a.cfg.layout_mode;
            apply_mode(a);
        }
        // ---- 版式
        MI_ORIENT_BASE | MI_ORIENT_V => {
            let new = Orient::all()[(cmd - MI_ORIENT_BASE) as usize];
            if new == a.cfg.orient {
                return;
            }
            let was_marquee = a.cfg.anim == Anim::Marquee;
            a.cfg.set_orient(new);
            a.rebuild_fonts();
            // 尺寸可能换了（只在「原来正好是旧形态默认值」时才换），
            // 所以要走缩放那条路：重建帧缓冲 + 落到窗口上
            apply_resize(a, a.cfg.x, a.cfg.y, a.cfg.w, a.cfg.h);
            a.cfg.save();
            render_and_present(a);
            if new == Orient::Vertical && was_marquee {
                toast(a, "竖版下跑马灯已降级为静态");
            }
        }
        MI_SHRINK_FIT => {
            a.cfg.shrink_fit = !a.cfg.shrink_fit;
            a.cfg.save();
            render_and_present(a);
        }
        MI_SPACING_UP => {
            a.cfg.letter_spacing = (a.cfg.letter_spacing + 1).clamp(0, 8);
            a.cfg.save();
            render_and_present(a);
        }
        MI_SPACING_DOWN => {
            a.cfg.letter_spacing = (a.cfg.letter_spacing - 1).clamp(0, 8);
            a.cfg.save();
            render_and_present(a);
        }
        MI_QUOTE_COLOR => {
            a.cfg.quote_color = config::next_quote_color(a.cfg.quote_color);
            a.cfg.save();
            render_and_present(a);
        }
        MI_OP_UP => bump_opacity(a, 12),
        MI_OP_DOWN => bump_opacity(a, -12),
        MI_TOPMOST => {
            a.cfg.always_on_top = !a.cfg.always_on_top;
            apply_topmost(a);
            a.cfg.save();
        }
        MI_AUTOSTART => {
            // 注册表是唯一状态源：菜单勾选态每次开菜单都重读，
            // 这里按「读到的真实状态」取反，而不是按 cfg（cfg 里根本没有这项）。
            let on = !autostart::enabled();
            if autostart::apply(on) {
                a.autostart_err = 0;
            } else {
                a.autostart_err = autostart::last_err();
            }
        }
        MI_AUTOHIDE => {
            a.cfg.auto_hide_fs = !a.cfg.auto_hide_fs;
            // 关闭的瞬间若正因全屏藏着，立刻恢复 —— 绝不能留下
            // 「功能关了、窗口还藏着」这种说不清的状态。
            if !a.cfg.auto_hide_fs && a.fs_hidden {
                temp_hide(a, false);
                a.fs_hidden = false;
            }
            a.cfg.save();
        }
        MI_SECONDS => {
            a.cfg.seconds = !a.cfg.seconds;
            a.cfg.save();
            render_and_present(a);
        }
        MI_SHADOW => {
            a.cfg.shadow = !a.cfg.shadow;
            a.cfg.save();
            render_and_present(a);
        }
        // ---- 显示内容
        MI_CLOCK => {
            a.cfg.show_clock = !a.cfg.show_clock;
            a.cfg.save();
            render_and_present(a);
        }
        MI_DATE => {
            a.cfg.show_date = !a.cfg.show_date;
            a.cfg.save();
            render_and_present(a);
        }
        MI_QUOTE => {
            a.cfg.show_quote = !a.cfg.show_quote;
            a.cfg.save();
            render_and_present(a);
        }
        // ---- 外观
        MI_SCALE_UP => {
            a.cfg.scale = config::next_scale(a.cfg.scale, 1);
            a.rebuild_fonts();
            a.cfg.save();
            render_and_present(a);
        }
        MI_SCALE_DOWN => {
            a.cfg.scale = config::next_scale(a.cfg.scale, -1);
            a.rebuild_fonts();
            a.cfg.save();
            render_and_present(a);
        }
        MI_CLOCK_UP => {
            a.cfg.clock_scale = config::next_step(&config::RATIO_STEPS, a.cfg.clock_scale, 1);
            a.rebuild_fonts();
            a.cfg.save();
            render_and_present(a);
        }
        MI_CLOCK_DOWN => {
            a.cfg.clock_scale = config::next_step(&config::RATIO_STEPS, a.cfg.clock_scale, -1);
            a.rebuild_fonts();
            a.cfg.save();
            render_and_present(a);
        }
        MI_QUOTE_UP => {
            a.cfg.quote_scale = config::next_step(&config::RATIO_STEPS, a.cfg.quote_scale, 1);
            a.rebuild_fonts();
            a.cfg.save();
            render_and_present(a);
        }
        MI_QUOTE_DOWN => {
            a.cfg.quote_scale = config::next_step(&config::RATIO_STEPS, a.cfg.quote_scale, -1);
            a.rebuild_fonts();
            a.cfg.save();
            render_and_present(a);
        }
        MI_CONFIG => config_ui::open(a),
        MI_FONT => {
            a.cfg.font = config::next_font(&a.cfg.font);
            a.rebuild_fonts();
            a.cfg.save();
            render_and_present(a);
        }
        MI_COLOR => {
            a.cfg.color = config::next_color(a.cfg.color);
            a.cfg.save();
            render_and_present(a);
        }
        MI_DATE_COLOR => {
            a.cfg.date_color = config::next_date_color(a.cfg.date_color);
            a.cfg.save();
            render_and_present(a);
        }
        MI_CLOCK_RESET => {
            a.cfg.clock_dx = 0;
            a.cfg.clock_dy = 0;
            a.cfg.save();
            render_and_present(a);
        }
        MI_SPEED_UP => {
            a.cfg.speed = (a.cfg.speed + 10).min(400);
            a.cfg.save();
        }
        MI_SPEED_DOWN => {
            a.cfg.speed = (a.cfg.speed - 10).max(10);
            a.cfg.save();
        }
        MI_PREV => on_hotkey(a, HK_PREV),
        MI_NEXT => on_hotkey(a, HK_NEXT),
        MI_FAV => favorite(a),
        MI_RELOAD => {
            a.list = quotes::library();
            let old = a.cfg.clone();
            let new = Config::load();
            a.cfg = new;
            a.cfg.x = old.x;
            a.cfg.y = old.y;
            a.qi = a.qi.min(a.list.len().saturating_sub(1));
            a.qn = (a.qi + 1) % a.list.len();
            // 配置里的字号/字体/颜色可能都变了，无条件重建一次最省心
            a.rebuild_fonts();
            sync_from_panel(a);
            apply_mode(a);
        }
        MI_RESET => {
            // 回「挂件当前所在屏」的右上角，不是主屏 —— 副屏用户点这条菜单
            // 期待的是回到自己那块屏的角上。
            // 方向必须是「cfg 设目标 → apply_geometry 落到窗口」。这里曾经调
            // `sync_from_panel`，而它是**反向**同步（读窗口真实矩形覆写 cfg），
            // 刚设好的目标位置立刻被窗口旧位置改回去 —— 菜单项成了空操作。
            let (wa, _) = work_area_at(a.cfg.x, a.cfg.y, a.cfg.w);
            a.cfg.x = wa.right - a.cfg.w - 24;
            a.cfg.y = wa.top + 24;
            apply_geometry(a, false, true);
            a.cfg.save();
        }
        MI_QUIT => quit(a),
        c if (MI_ANIM_BASE..MI_ANIM_BASE + 4).contains(&c) => {
            a.cfg.anim = Anim::all()[(c - MI_ANIM_BASE) as usize];
            a.phase = 0.0;
            a.m = 0.0;
            // 同 HK_ANIM：落定在飞的过渡再切
            a.trans.settle();
            a.cfg.save();
            render_and_present(a);
        }
        _ => {}
    }
}

fn quit(a: &mut App) {
    unregister_hotkeys(a);
    del_tray(a);
    // 设置面板（如果开着）先收掉：它的 WM_DESTROY 会释放自己那一堆字体/刷子句柄
    config_ui::close(a);
    unsafe {
        KillTimer(a.text, TIMER_ID);
        a.cfg.save();
        let text = a.text;
        let panel = a.panel;
        a.text = null_mut();
        a.panel = null_mut();
        if !text.is_null() {
            DestroyWindow(text);
        }
        if !panel.is_null() {
            DestroyWindow(panel);
        }
    }
    std::process::exit(0);
}

// ---------------------------------------------------------------- 窗口位置同步

/// 底板是唯一权威：窗口被系统移动/缩放后，把结果同步到配置。
///
/// **不在这里出帧**：位置变化不影响像素内容，而出帧要 700µs。
/// 只有尺寸变了才标脏 —— 那才是真的需要重画的事。
fn sync_from_panel(a: &mut App) {
    if a.panel.is_null() {
        return;
    }
    let mut r: RECT = unsafe { zeroed() };
    if unsafe { GetWindowRect(a.panel, &mut r) } == 0 {
        return;
    }
    let w = (r.right - r.left).max(MIN_W);
    let h = (r.bottom - r.top).max(MIN_H);
    let resized = w != a.cfg.w || h != a.cfg.h;
    a.cfg.x = r.left;
    a.cfg.y = r.top;
    a.cfg.w = w;
    a.cfg.h = h;
    if resized {
        a.m = 0.0;
        // 窗口缩小后时间日期块可能露在外面，重新夹一次
        normalize_clock_offset(a);
        mark_dirty(a);
    }
}

/// 同步几何并立刻出一帧（系统事件、热键这类「对用户是一次动作」的场合用）
fn sync_from_panel_now(a: &mut App) {
    sync_from_panel(a);
    render_and_present(a);
}

// ---------------------------------------------------------------- 窗口过程

/// 从 `WM_LBUTTONDOWN` / `WM_MOUSEMOVE` 的 lparam 取出坐标。
///
/// 刻意用消息自带的坐标而不是 `GetCursorPos()`：一是消息里的坐标永远不会
/// 因为队列延迟而和事件本身错位，二是这样一条鼠标消息就是**自包含**的，
/// 可以用 `PostMessage` 把「按下 → 移动 → 松开」整条链路灌进去做端到端验证
/// （见 `--dragtest`），不必去注入系统级鼠标。
fn lparam_pt(lparam: LPARAM) -> (i32, i32) {
    let x = (lparam & 0xFFFF) as u16 as i16 as i32;
    let y = ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32;
    (x, y)
}

/// 窗口的实际矩形。断言要验的是「窗口真的动了」，所以必须读内核，
/// 不能拿配置里的 x/y 自证。
unsafe fn win_rect(hwnd: HWND) -> RECT {
    let mut r: RECT = zeroed();
    GetWindowRect(hwnd, &mut r);
    r
}

/// 光标当前的屏幕坐标。只在 `WM_SETCURSOR` 里用 —— 那个消息不带坐标。
fn cursor_screen() -> (i32, i32) {
    let mut pt: POINT = unsafe { zeroed() };
    unsafe { GetCursorPos(&mut pt) };
    (pt.x, pt.y)
}

/// 客户区坐标 → 边框命中码。不在边框带里就返回 `HTCLIENT`（= 中间，整体拖动）。
fn edge_zone(a: &App, cx: i32, cy: i32) -> isize {
    let m = (8i32 * a.cfg.scale / 100).max(6);
    let (w, h) = (a.cfg.w, a.cfg.h);
    let left = cx < m;
    let right = cx >= w - m;
    let top = cy < m;
    let bottom = cy >= h - m;
    match (left, right, top, bottom) {
        (true, _, true, _) => HTTOPLEFT,
        (_, true, true, _) => HTTOPRIGHT,
        (true, _, _, true) => HTBOTTOMLEFT,
        (_, true, _, true) => HTBOTTOMRIGHT,
        (true, _, _, _) => HTLEFT,
        (_, true, _, _) => HTRIGHT,
        (_, _, true, _) => HTTOP,
        (_, _, _, true) => HTBOTTOM,
        _ => HTCLIENT,
    }
}

/// 命中码 → 光标形状。
///
/// 自己接管鼠标之后就没有系统的光标反馈了，这里得补回来：
/// 光靠一个「能拖」的窗口，用户根本看不出四条边是可以拉的。
fn drag_cursor(zone: isize) -> HCURSOR {
    let res = match zone {
        HTLEFT | HTRIGHT => IDC_SIZEWE_RES,
        HTTOP | HTBOTTOM => IDC_SIZENS_RES,
        HTTOPLEFT | HTBOTTOMRIGHT => IDC_SIZENWSE_RES,
        HTTOPRIGHT | HTBOTTOMLEFT => IDC_SIZENESW_RES,
        _ => IDC_SIZEALL_RES,
    };
    unsafe { LoadCursorW(null_mut(), res) }
}

/// 松开鼠标的收尾：放手、把最终画面**强制**出一帧、落盘。
///
/// 强制出帧这一步不能省：节流最多会把重绘推迟 8ms，
/// 而「松手」是用户眼里的动作终点，画面必须立刻停在最终状态。
fn end_grab(a: &mut App) {
    a.grab = Grab::None;
    unsafe {
        ReleaseCapture();
        // 定时器调回平时的 25Hz（拖动期间被收紧到 DRAG_TICK_MS）
        SetTimer(a.text, TIMER_ID, TIMER_MS, None);
    }
    // 位置也要**强制**落一次：节流最多把它推迟 8ms，而松手是动作终点 ——
    // 窗口必须停在你松手的地方，不能还欠着最后那一格。
    apply_geometry(a, true, true);
    a.cfg.save();
    render_if_due(a, true);
}

/// 拖动时的边界保护：至少留 `KEEP_VISIBLE` px 在工作区内。
///
/// 和 `clamp_to_work_area` 共用同一份判据 —— 「别把挂件拖丢」这条约定
/// 在拖动路径和启动路径上必须是同一个，而且**四条边同一个数**：
/// 上下留 40、左右留 80 这种不对称，会让用户拖到下方时以为「到底了」，
/// 再往下却还能走一点，手感上就是「边界会漂」。
/// 上边例外：顶不允许越过工作区上沿（挂件没有标题栏，钻上去就抓不回来了）。
fn clamp_keep_visible(x: i32, y: i32, w: i32) -> (i32, i32) {
    let (wa, _) = work_area_at(x, y, w);
    let lo_x = wa.left - w + KEEP_VISIBLE;
    let hi_x = (wa.right - KEEP_VISIBLE).max(lo_x);
    let lo_y = wa.top;
    let hi_y = (wa.bottom - KEEP_VISIBLE).max(lo_y);
    (x.clamp(lo_x, hi_x), y.clamp(lo_y, hi_y))
}

/// 客户区坐标是否落在时间日期块上（含 4px 容差，块很小时也抓得住）
fn hit_clock_client(a: &App, cx: i32, cy: i32) -> bool {
    if !a.clock_visible() {
        return false;
    }
    cx >= a.clock_rect.left - 4
        && cx < a.clock_rect.right + 4
        && cy >= a.clock_rect.top - 4
        && cy < a.clock_rect.bottom + 4
}

/// 把时间日期块的偏移夹回窗口内，并**回写夹取后的实际偏移**。
///
/// 回写是拖动跟手的关键：如果只夹显示位置、不回写偏移，
/// 用户拖出边界 200px 再往回拖时，前 200px 光标动了但内容不动 —— 空走。
///
/// 缩放拖动时每条鼠标消息都会走到这儿，而夹取公式是纯算术、不碰 GDI ——
/// 真正的开销在量文字（`layout_clock`），那份已由 `layout_clock` 的结果
/// 缓存挡掉。见 `layout_clock` 的文档：`--perf` 实测缩放拖动 1052µs/条
/// 就是这么降到 ~300µs 的。
fn normalize_clock_offset(a: &mut App) {
    // 竖版下 `layout_sign` 的默认为「贴左侧、垂直居中」，与横版同一套夹取公式，
    // 所以这里一行都不用分叉。
    let cb = layout_clock(a);
    if !cb.show {
        return;
    }

    let scale = a.cfg.scale as f32 / 100.0;
    let pad = ((24.0 * scale).round() as i32).max(10);
    let (w, h) = (a.cfg.w, a.cfg.h);
    let (x0, y0) = (pad, (h - cb.h) / 2);
    let max_x = (w - cb.w).max(0);
    let max_y = (h - cb.h).max(0);
    let x = (x0 + a.cfg.clock_dx).clamp(0, max_x);
    let y = (y0 + a.cfg.clock_dy).clamp(0, max_y);
    a.cfg.clock_dx = x - x0;
    a.cfg.clock_dy = y - y0;
}

unsafe extern "system" fn panel_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let Some(a) = app_ready() else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };
    MSG_TALLY[(msg & 0xFF) as usize].fetch_add(1, Ordering::Relaxed);
    if msg == WM_MOUSEMOVE {
        // 恒在最前面计数（在「是不是布局模式」的判断**之前**）：
        // 这里问的是「消息有没有落到我们手里」，与之后怎么处理无关。
        WIDGET_MOUSE_HITS.fetch_add(1, Ordering::Relaxed);
    }
    match msg {
        WM_ERASEBKGND => 1,

        // 底板在 Acrylic 模式下必须什么都不画。
        // 唯一的例外是降级路径（整窗 alpha），那时需要自绘一块实心深色板。
        WM_PAINT => {
            if a.glass == Glass::Layered {
                let mut ps: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(hwnd, &mut ps);
                let mut rc: RECT = zeroed();
                GetClientRect(hwnd, &mut rc);
                let brush = CreateSolidBrush(
                    (PANEL_B << 16) | (PANEL_G << 8) | PANEL_R,
                );
                FillRect(dc, &rc, brush);
                DeleteObject(brush as HGDIOBJ);
                EndPaint(hwnd, &ps);
            } else {
                let mut ps: PAINTSTRUCT = zeroed();
                BeginPaint(hwnd, &mut ps);
                EndPaint(hwnd, &ps);
            }
            0
        }

        WM_HOTKEY => {
            on_hotkey(a, wparam as i32);
            0
        }

        WM_TRAY => {
            let ev = lparam as u32;
            if ev == WM_LBUTTONUP || ev == WM_RBUTTONUP || ev == WM_LBUTTONDBLCLK {
                show_menu(a);
            }
            0
        }

        // ---- 布局模式：鼠标全部由我们自己接管。
        //
        // 关键改动：这里不再返回 HTCAPTION / HTLEFT，把拖动交给系统。
        // 系统的模态移动循环是个黑盒 —— 它每收到一个鼠标消息就 SetWindowPos 一次，
        // 每次都连带触发我们的 WM_MOVE（旧实现在那里跑整帧重绘），
        // 而且整个循环期间我们没有任何节流的机会，消息堆积了也只能干等。
        // 自己接管之后，「收状态」和「出画面」才拆得开（见 `render_if_due`）。
        //
        // 光标形状用 WM_SETCURSOR 单独给：自己接管不等于丢掉「哪里能拖」的提示。
        WM_NCHITTEST => {
            if !a.cfg.layout_mode || a.cfg.w <= 0 {
                return HTTRANSPARENT;
            }
            HTCLIENT
        }

        WM_SETCURSOR => {
            if !a.cfg.layout_mode {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            // 抓取期间直接用按下那一刻算好的命中带：`WM_SETCURSOR` 每次鼠标移动
            // 都会来一发，这时候再去 `GetCursorPos` + `GetWindowRect` 纯属白花 ——
            // 窗口正在动，现算的结果还不如按下的那一刻准。
            let zone = if a.grab != Grab::None {
                a.grab_zone
            } else {
                let (sx, sy) = cursor_screen();
                let mut r: RECT = zeroed();
                GetWindowRect(hwnd, &mut r);
                edge_zone(a, sx - r.left, sy - r.top)
            };
            SetCursor(drag_cursor(zone));
            1
        }

        WM_LBUTTONDOWN => {
            if a.cfg.layout_mode {
                let (cx, cy) = lparam_pt(lparam);
                let zone = edge_zone(a, cx, cy);
                a.grab_zone = zone;
                // 锚点一律取**窗口真实位置**。按下那一刻它当然等于 `cfg.x/y`，
                // 但拖动一开始两者就会脱钩（节流让 `cfg` 领先于实际窗口），
                // 所以基准从第一刻起就只认一个来源，免得中途换算法。
                let wp = win_rect(hwnd);
                // 时间日期块优先于缩放边框：块本来就贴在左内边距上，
                // 常常和左边框的命中带重叠，先判它才拖得动
                if zone == HTCLIENT && hit_clock_client(a, cx, cy) {
                    a.grab = Grab::Clock;
                    a.grab_pt = (cx, cy);
                    a.grab_off = (a.cfg.clock_dx, a.cfg.clock_dy);
                } else if zone == HTCLIENT {
                    a.grab = Grab::Move;
                    a.grab_pt = (wp.left + cx, wp.top + cy);
                    a.grab_win = (wp.left, wp.top, a.cfg.w, a.cfg.h);
                } else {
                    a.grab = Grab::Edge(zone);
                    a.grab_pt = (wp.left + cx, wp.top + cy);
                    a.grab_win = (wp.left, wp.top, a.cfg.w, a.cfg.h);
                }
                SetCapture(hwnd);
                // 拖动期间把定时器收紧到位置节拍，好把「停手后欠着的那一格」补上
                // （见 `DRAG_TICK_MS`）。松手时 `end_grab` 会调回去。
                SetTimer(a.text, TIMER_ID, DRAG_TICK_MS, None);
            }
            0
        }

        WM_MOUSEMOVE => {
            if a.grab == Grab::None {
                return 0;
            }
            // 消息里的**客户区**坐标。窗口原点在拖动前是已知的实际值
            // （位置由我们自己应用，`cfg.x/y` 永远等于窗口实际位置），
            // 所以屏幕坐标 = 原点 + 客户区坐标，不需要 GetCursorPos()。
            // 这也是整条链路可复现、能用 PostMessage 灌消息做端到端验证的原因。
            let (cx, cy) = lparam_pt(lparam);
            match a.grab {
                // 挪整个挂件：只改窗口位置，一个像素都不用重画
                Grab::Move => {
                    // 客户区坐标是相对**窗口真实位置**的，所以重建屏幕坐标必须用
                    // 真实位置 —— 不能用 `cfg.x`：节流会让它领先于窗口（那是
                    // 「目标」，不是「现状」）。两者一旦脱钩，每条消息都会把上一条
                    // 欠着的位移重新算一遍，误差按三角数放大 ——
                    // 表现就是「鼠标一动，窗口乱飘」（`--dragtest` 第 ⑧ 条守着它）。
                    let wp = win_rect(hwnd);
                    let (sx, sy) = (wp.left + cx, wp.top + cy);
                    let raw = (
                        a.grab_win.0 + (sx - a.grab_pt.0),
                        a.grab_win.1 + (sy - a.grab_pt.1),
                    );
                    let (nx, ny) = clamp_keep_visible(raw.0, raw.1, a.cfg.w);
                    if (nx, ny) != raw {
                        // 被边界夹住了：把基准重置到「当前实际位置 + 当前光标」，
                        // 这样光标一往回走就立刻跟手，不会有一段拖不动的空走
                        a.grab_win = (wp.left, wp.top, a.cfg.w, a.cfg.h);
                        a.grab_pt = (sx, sy);
                    }
                    request_move(a, nx, ny);
                    if a.legacy_drag {
                        // 旧实现：位置变了也照样整帧重绘一遍（~700µs）。
                        // 这些工作对「窗口在哪」毫无贡献，纯属白花 ——
                        // 而且它把每条消息的成本抬到 1ms 量级，
                        // 1000Hz 鼠标一来就在队列里排队，手感就是「粘」。
                        render_and_present(a);
                    }
                }
                // 挪时间日期块：块的位置变了 → 内容要重画，但按节拍出
                Grab::Clock => {
                    a.cfg.clock_dx = a.grab_off.0 + (cx - a.grab_pt.0);
                    a.cfg.clock_dy = a.grab_off.1 + (cy - a.grab_pt.1);
                    normalize_clock_offset(a);
                    mark_dirty(a);
                    render_if_due(a, false);
                }
                // 缩放：底板立刻跟手，内容按节拍重画
                Grab::Edge(zone) => {
                    // 同 `Grab::Move`：屏幕坐标必须由窗口**真实位置**重建。
                    // 缩放时窗口原点本来就会动（拖左边/上边），这里更容易踩坑。
                    let wp = win_rect(hwnd);
                    let (sx, sy) = (wp.left + cx, wp.top + cy);
                    let dx = sx - a.grab_pt.0;
                    let dy = sy - a.grab_pt.1;
                    let (mut x, mut y, mut w, mut h) = a.grab_win;
                    if matches!(zone, HTLEFT | HTTOPLEFT | HTBOTTOMLEFT) {
                        x += dx;
                        w -= dx;
                    }
                    if matches!(zone, HTRIGHT | HTTOPRIGHT | HTBOTTOMRIGHT) {
                        w += dx;
                    }
                    if matches!(zone, HTTOP | HTTOPLEFT | HTTOPRIGHT) {
                        y += dy;
                        h -= dy;
                    }
                    if matches!(zone, HTBOTTOM | HTBOTTOMLEFT | HTBOTTOMRIGHT) {
                        h += dy;
                    }
                    // 夹到最小尺寸时把被顶住的那条边锚回去，
                    // 否则继续拖会把窗口「推」着走
                    if w < MIN_W {
                        if matches!(zone, HTLEFT | HTTOPLEFT | HTBOTTOMLEFT) {
                            x -= MIN_W - w;
                        }
                        w = MIN_W;
                    }
                    if h < MIN_H {
                        if matches!(zone, HTTOP | HTTOPLEFT | HTTOPRIGHT) {
                            y -= MIN_H - h;
                        }
                        h = MIN_H;
                    }
                    apply_resize(a, x, y, w, h);
                }
                Grab::None => {}
            }
            0
        }

        WM_LBUTTONUP => {
            if a.grab != Grab::None {
                end_grab(a);
            }
            0
        }

        // 捕获被别处抢走（比如别的窗口弹出来）也要按「松开」收尾，
        // 否则会留下一个永远不放手的拖动状态
        WM_CAPTURECHANGED => {
            if a.grab != Grab::None {
                end_grab(a);
            }
            0
        }

        // 位置变化不需要重绘 —— 像素内容和窗口在哪无关。
        // 尺寸变了才标脏（那是真的会改画面的事），重绘交给节拍。
        //
        // 拖动期间**必须跳过**：这两条消息是我们自己 `SetWindowPos` 的**回声**，
        // 而拖动中窗口位置是「按节拍落定」的 —— 回声里的位置正是上一拍的旧值，
        // 让它盖回 `cfg.x/y`，等于每条消息都把状态拽回 8ms 前，
        // 拖动判定和边界夹取会集体以旧位置为基准。
        WM_MOVE | WM_SIZE => {
            if a.grab == Grab::None {
                sync_from_panel(a);
            }
            0
        }

        WM_ENTERSIZEMOVE => 0,
        WM_EXITSIZEMOVE => {
            a.cfg.save();
            0
        }

        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            clamp_to_work_area(&mut a.cfg);
            sync_from_panel_now(a);
            a.cfg.save();
            0
        }

        WM_CLOSE => {
            quit(a);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn text_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let Some(a) = app_ready() else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };
    if msg == WM_MOUSEMOVE {
        // 文字层理论上永远收不到鼠标移动（layered + transparent）。
        // 收得到就说明穿透破了 —— `--clicktest` 把它当失败信号。
        WIDGET_MOUSE_HITS.fetch_add(1, Ordering::Relaxed);
    }
    match msg {
        WM_ERASEBKGND => 1,
        WM_TIMER => {
            if wparam == TIMER_ID {
                tick(a);
            }
            0
        }
        WM_PAINT => {
            // 分层窗口的内容全部来自 UpdateLayeredWindow，
            // 这里只要把重绘请求消化掉、顺带补一帧
            let mut ps: PAINTSTRUCT = zeroed();
            BeginPaint(hwnd, &mut ps);
            EndPaint(hwnd, &ps);
            render_and_present(a);
            0
        }
        // 文字窗口恒穿透：它只负责显示，绝不参与命中
        WM_NCHITTEST => HTTRANSPARENT,
        WM_CLOSE => {
            quit(a);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
