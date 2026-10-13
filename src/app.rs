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
use std::mem::zeroed;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{
    GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, SelectObject,
    HBRUSH, HDC, HGDIOBJ, PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, RegisterHotKey, ReleaseCapture, SendInput, SetCapture, UnregisterHotKey, INPUT,
    INPUT_MOUSE, MOUSEEVENTF_MOVE,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetClientRect, GetCursorPos, GetMessageW, GetWindowLongPtrW, GetWindowRect,
    IsWindowVisible, KillTimer, LoadCursorW, LoadIconW, PeekMessageW, PostMessageW, PostQuitMessage,
    RegisterClassW, SetCursor, SetCursorPos, SetForegroundWindow, SetLayeredWindowAttributes,
    SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, SystemParametersInfoW, TrackPopupMenu,
    TranslateMessage, HCURSOR, MSG, WNDCLASSW,
};

use crate::config::{self, Anim, Config, Mode};
use crate::consts::*;
use crate::ffi::{
    set_window_composition_attribute, AccentPolicy, SetProcessDpiAwarenessContext,
    SetProcessDPIAware, WinCompAttrData, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use crate::gfx::{self, Fonts, Frame};
use crate::quotes::{self, Quote};
use crate::util::*;

// ---------------------------------------------------------------- 标识

const PANEL_CLASS: &str = "DeskGeyanPanel";
const TEXT_CLASS: &str = "DeskGeyanText";
const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 40;

/// `panel_proc` 收到的消息计数（`--dragbench` / `--memtest` 用）。
///
/// 用静态原子量而不是 `App` 上的字段 —— 这不是洁癖。整个程序靠
/// `static mut APP_PTR` 让 wndproc 拿到 `&mut App`，而消息泵里
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
    unsafe { ((*APP_PTR).cfg.x, (*APP_PTR).cfg.y) }
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

// 菜单 id
const MI_TOGGLE: u32 = 1;
const MI_GLASS: u32 = 2;
const MI_TEXT: u32 = 3;
const MI_ANIM_BASE: u32 = 10; // 10..14 对应四个动效
const MI_OP_UP: u32 = 20;
const MI_OP_DOWN: u32 = 21;
const MI_TOPMOST: u32 = 22;
const MI_LAYOUT: u32 = 23;
const MI_SECONDS: u32 = 24;
const MI_SHADOW: u32 = 25;
const MI_SPEED_UP: u32 = 26;
const MI_SPEED_DOWN: u32 = 27;
const MI_PREV: u32 = 30;
const MI_NEXT: u32 = 31;
const MI_RELOAD: u32 = 32;
const MI_RESET: u32 = 33;
// 显示内容
const MI_CLOCK: u32 = 40;
const MI_DATE: u32 = 41;
const MI_QUOTE: u32 = 42;
// 外观
const MI_SCALE_UP: u32 = 43;
const MI_SCALE_DOWN: u32 = 44;
const MI_CLOCK_UP: u32 = 45;
const MI_CLOCK_DOWN: u32 = 46;
const MI_QUOTE_UP: u32 = 47;
const MI_QUOTE_DOWN: u32 = 48;
const MI_COLOR: u32 = 49;
const MI_DATE_COLOR: u32 = 50;
const MI_FONT: u32 = 51;
const MI_CLOCK_RESET: u32 = 52;
const MI_QUIT: u32 = 99;

/// 时间日期块拖动命中的最小判定范围（避免块很小时抓不住）
const CLOCK_MIN_GRAB: i32 = 28;

// ---------------------------------------------------------------- 全局状态
//
// 刻意用裸指针而不是 Mutex：
// `TrackPopupMenu` / `DestroyWindow` / `ShowWindow` 会在**同一线程内同步派发**
// 消息回到我们自己的 wndproc。如果状态挂在 Mutex 后面，这就是一次必然的自锁。
// 程序是严格单线程的（所有窗口同属创建它们的那个线程），裸指针反而更诚实。

static mut APP_PTR: *mut App = null_mut();

fn app() -> &'static mut App {
    unsafe { &mut *APP_PTR }
}

fn app_ready() -> Option<&'static mut App> {
    unsafe {
        if APP_PTR.is_null() {
            None
        } else {
            Some(&mut *APP_PTR)
        }
    }
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
    alpha: f32,

    fonts: Fonts,
    frame: Frame,
    brush: HBRUSH,

    glass: Glass,
    accent_alpha: i32,
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
    /// 主时钟最后出帧时的「时间戳」（时/分/秒），静态画面下用它决定要不要重画
    clock_stamp: u64,
    /// 跑马灯首帧：格言应当已经在带内，而不是让用户先看一段空白
    primed: bool,
    last_tick: std::time::Instant,
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
        let brush = unsafe { CreateSolidBrush(0x0050_5050) };
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
            alpha: 1.0,
            cfg,
            list,
            fonts,
            frame,
            brush,
            glass: Glass::Acrylic,
            accent_alpha: -1,
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
            clock_stamp: u64::MAX,
            primed: false,
            last_tick: std::time::Instant::now(),
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
        })
    }

    fn should_show_panel(&self) -> bool {
        self.cfg.mode == Mode::Glass || self.cfg.layout_mode
    }

    /// 底板的不透明度：整体不透明度 × 底板系数 × 呼吸系数
    fn panel_alpha(&self) -> u8 {
        let breathe = if self.cfg.anim == Anim::Breathe {
            self.alpha
        } else {
            1.0
        };
        let a = self.cfg.opacity as f32 * PANEL_FACTOR * breathe;
        a.round().clamp(0.0, 255.0) as u8
    }

    /// 文字层的不透明度：整体不透明度 × 动效系数
    fn text_alpha(&self) -> u8 {
        let a = self.cfg.opacity as f32 * self.alpha;
        a.round().clamp(0.0, 255.0) as u8
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
        // 字号变了，块的大小跟着变，偏移得重新夹一次
        normalize_clock_offset(self);
    }

    /// 时间日期块是否显示
    fn clock_visible(&self) -> bool {
        self.cfg.show_clock || self.cfg.show_date
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
    // 这个时长足以跨过跑马灯的轮转点(约 14.5s)、静态的换句点(15s)、
    // 淡入淡出的换句点(6.4s)，把每一条状态迁移都真正走一遍。
    a.cfg.speed = 200; // 加快跑马灯，让轮转在 soak 内发生不止一次
    for an in Anim::all() {
        a.cfg.anim = an;
        a.phase = 0.0;
        a.m = 0.0;
        a.primed = true;
        let mut ok = true;
        let mut why = String::from("180 帧无异常");
        let mut bad_px = 0usize;
        let mut rotations = 0usize;
        let mut last_qi = a.qi;
        let mut peak_alpha = 0u8;
        let mut min_alpha = 255u8;
        for i in 0..180 {
            step(&mut a, 0.1);
            if !(a.alpha.is_finite() && (0.0..=1.0).contains(&a.alpha)) {
                ok = false;
                why = format!("第 {} 帧 alpha={} 越界", i, a.alpha);
                break;
            }
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
                last_qi = a.qi;
            }
            let op = a.text_alpha();
            build_frame(&mut a, op);
            peak_alpha = peak_alpha.max(op);
            min_alpha = min_alpha.min(op);
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
        rows.push((
            an.label(),
            ok,
            format!(
                "{}（换句 {} 次，alpha {}..{}）",
                why, rotations, min_alpha, peak_alpha
            ),
        ));
    }
    a.cfg.speed = 60;

    // 回到默认动效，后面的行继续沿用
    a.cfg.anim = Anim::Marquee;
    a.alpha = 1.0;
    a.m = 260.0;
    let op2 = a.text_alpha();
    build_frame(&mut a, op2);

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
    let ink_of = |a: &mut App, scale: i32, clock: i32, quote: i32| -> u64 {
        a.cfg.scale = scale;
        a.cfg.clock_scale = clock;
        a.cfg.quote_scale = quote;
        a.rebuild_fonts();
        let o = a.text_alpha();
        build_frame(a, o);
        a.frame.out_pixels().iter().map(|p| ((p >> 24) & 0xFF) as u64).sum()
    };
    let ink_100 = ink_of(&mut a, 100, 100, 100);
    let ink_150 = ink_of(&mut a, 150, 100, 100);
    let ink_clock200 = ink_of(&mut a, 100, 200, 100);
    let ink_quote200 = ink_of(&mut a, 100, 100, 200);
    rows.push((
        "整体字号可配置",
        ink_150 > ink_100 * 3 / 2,
        format!("100% → 墨量 {}，150% → {}（↑{:.0}%）", ink_100, ink_150,
            (ink_150 as f64 / ink_100.max(1) as f64 - 1.0) * 100.0),
    ));
    // 时钟字号和格言字号必须是**两个互不干扰**的旋钮：
    // 各自都能单独把画面显著撑大，且效果互不相同。
    rows.push((
        "时钟 / 格言字号可分别单独调",
        ink_clock200 > ink_100 * 3 / 2
            && ink_quote200 > ink_100 * 3 / 2
            && ink_clock200 != ink_quote200,
        format!(
            "整体仍 100%：时钟 200% → {}，格言 200% → {}（两个旋钮各管各的）",
            ink_clock200, ink_quote200
        ),
    ));

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
    let o = a.text_alpha();
    build_frame(&mut a, o);
    // 阴影趟是「同一批字形整体位移 (off,off) 再画一遍」，所以这条不变量是
    // **精确**的（不是启发式）：阴影掩码在 (x,y) 有墨，当且仅当
    // 掩码在 (x-off, y-off) 有字形。裁到哪就画到哪，一个像素都不许例外。
    let off = ((2.0 * a.cfg.scale as f32 / 100.0).round() as i32).max(0);
    let (fw2, fh2) = (a.frame.w, a.frame.h);
    let (glyph_px, orphan_px) = {
        let (m, dm, sh) = a.frame.masks();
        let mut glyph = 0usize;
        let mut orphan = 0usize;
        for y in 0..fh2 {
            for x in 0..fw2 {
                let i = (y * fw2 + x) as usize;
                if m[i] > 0 || dm[i] > 0 {
                    glyph += 1;
                }
                if sh[i] == 0 {
                    continue;
                }
                let (sx, sy) = (x - off, y - off);
                let backed = sx >= 0 && sy >= 0 && sx < fw2 && sy < fh2 && {
                    let j = (sy * fw2 + sx) as usize;
                    m[j] > 0 || dm[j] > 0
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
        // 判据 1%：修好之后实测 0.3%（来自字形 hinting 在不同偏移下
        // 覆盖率取整的抖动），而裁剪区一旦算错会飙到 20% 左右。
        glyph_px > 0 && orphan_px * 100 <= glyph_px,
        format!(
            "游离阴影 {} px / 字形 {} px（{:.2}%，判据：阴影必须在字形位移 {}px 处）",
            orphan_px,
            glyph_px,
            orphan_px as f64 * 100.0 / glyph_px.max(1) as f64,
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
        // 但**建窗之前**要先摆成纯文字模式 —— 这样 boot 之后底板的可见性
        // 才有硬判据，而且正好覆盖「底板被无条件显示出来」那个真出现过的 bug
        // （它是「看不见却拦鼠标」，必须在建窗后**立刻**看，中间不能夹 apply_mode，
        // 否则 apply_glass 会把它抹掉，断言就永远绿了）。
        st.cfg.mode = Mode::TextOnly;
        st.cfg.layout_mode = false;
        let boxed = Box::new(st);
        APP_PTR = Box::into_raw(boxed);
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
            if a.hotkeys.len() >= 15 {
                R::Pass
            } else if a.hotkey_err == ERROR_HOTKEY_ALREADY_EXISTS {
                // 被另一个实例占着：单实例互斥量保证正式运行不会互相抢，
                // 这里如实标 INFO，而不是判 FAIL 让整轮探测失去意义
                R::Info
            } else {
                R::Fail
            },
            format!(
                "{}/17 注册成功{}",
                a.hotkeys.len(),
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
            let ok = !a.fonts.clock.is_null()
                && !a.fonts.clock_small.is_null()
                && !a.fonts.date.is_null()
                && !a.fonts.quote.is_null()
                && !a.fonts.hint.is_null();
            rows.push((
                "  └ 字体对象已按配置重建",
                if ok { R::Pass } else { R::Fail },
                format!("字面「{}」· 5 个字体句柄", face),
            ));
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
                "已写入 {}  ({}x{}，叠加于棋盘格之上)\n外观：{} {}% · 时钟{}% 格言{}% · 主色{} 日期{} · 时间位置 +{}/+{}",
                path,
                w,
                h,
                a.cfg.font,
                a.cfg.scale,
                a.cfg.clock_scale,
                a.cfg.quote_scale,
                fmt_color(a.cfg.color),
                fmt_color(a.cfg.date_color),
                a.cfg.clock_dx,
                a.cfg.clock_dy
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

/// 把配置快照起来，跑完原样写回。`--probe` / `--dragtest` 都必须零副作用，
/// 否则「测一下」就把用户摆好的位置带偏了。
fn with_config_guard<F: FnOnce() -> i32>(f: F) -> i32 {
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
unsafe fn pump() {
    let mut msg: MSG = zeroed();
    while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// 按 `MAKELPARAM` 打包坐标后投递一条鼠标消息
unsafe fn send_mouse(hwnd: HWND, msg: u32, x: i32, y: i32) {
    let lp = ((x & 0xFFFF) | ((y & 0xFFFF) << 16)) as isize;
    PostMessageW(hwnd, msg, 0, lp);
}

/// 抽一段时间队列。注入的系统级输入是**异步**送达的，
/// 只泵一次可能还什么都收不到 —— 那样「收到 0 条」会把失败读成成功。
unsafe fn pump_wait(ms: u64) {
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
unsafe fn mouse_moves_at(cx: i32, cy: i32) -> u32 {
    let mut old: POINT = zeroed();
    GetCursorPos(&mut old);

    SetCursorPos(cx - 2, cy);
    pump_wait(60);
    WIDGET_MOUSE_HITS.store(0, Ordering::Relaxed);

    let mut inp: INPUT = zeroed();
    inp.r#type = INPUT_MOUSE;
    inp.Anonymous.mi.dx = 2;
    inp.Anonymous.mi.dy = 0;
    inp.Anonymous.mi.dwFlags = MOUSEEVENTF_MOVE;
    SendInput(1, &inp, std::mem::size_of::<INPUT>() as i32);
    pump_wait(120);
    let n = WIDGET_MOUSE_HITS.load(Ordering::Relaxed);

    SetCursorPos(old.x, old.y);
    pump_wait(40);
    n
}

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
        st.cfg.mode = Mode::TextOnly;
        st.cfg.layout_mode = false;
        st.cfg.visible = true;
        st.cfg.x = 300;
        st.cfg.y = 300;
        st.cfg.w = 420;
        st.cfg.h = 140;
        let boxed = Box::new(st);
        APP_PTR = Box::into_raw(boxed);
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
        st.cfg.layout_mode = true;
        let boxed = Box::new(st);
        APP_PTR = Box::into_raw(boxed);
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
        let ok = worst < 1000.0 && idle_frames == 0 && anim_frames >= 10;
        println!(
            "两条参考线：125Hz 鼠标 8000µs/条，1000Hz 鼠标 1000µs/条。最慢的一条拖动 = {:.0} µs",
            worst
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
        st.cfg.layout_mode = true;
        let boxed = Box::new(st);
        APP_PTR = Box::into_raw(boxed);
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
        st.cfg.layout_mode = true;
        let boxed = Box::new(st);
        APP_PTR = Box::into_raw(boxed);
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
        // 关掉位置节流：这一节验的是**拖动逻辑**（位移量、夹取、锚点重置、
        // 松手落盘），而不是节拍时序。而且这里灌消息是同步的、没有时间流逝，
        // 开着节流会把绝大多数移动直接吃掉 —— 那样量到的是「8ms 里能落几次」，
        // 跟逻辑对不对毫无关系。节流本身由 `--dragbench` 专门验。
        st.move_throttle = 0;
        let boxed = Box::new(st);
        APP_PTR = Box::into_raw(boxed);
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
            send_mouse(a.panel, WM_LBUTTONDOWN, bx, by);
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

        send_mouse(a.panel, WM_LBUTTONDOWN, bx, by);
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
        send_mouse(a.panel, WM_LBUTTONDOWN, bx, by);
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
        send_mouse(a.panel, WM_LBUTTONDOWN, center(a).0, center(a).1);
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
        send_mouse(a.panel, WM_LBUTTONDOWN, mid.0, mid.1);
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
        send_mouse(a.panel, WM_LBUTTONDOWN, corner.0, corner.1);
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
        send_mouse(a.panel, WM_LBUTTONDOWN, mid2.0, mid2.1);
        pump();
        send_mouse(a.panel, WM_MOUSEMOVE, mid2.0 + 20000, mid2.1 + 20000);
        pump();
        let far = win_rect(a.panel);
        send_mouse(a.panel, WM_LBUTTONUP, mid2.0, mid2.1);
        pump();
        let (wa, _) = work_area();
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
        send_mouse(a.panel, WM_LBUTTONDOWN, mid2.0, mid2.1);
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
        send_mouse(a.panel, WM_LBUTTONDOWN, ccx, ccy);
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
        send_mouse(a.panel, WM_LBUTTONDOWN, ccx2, ccy2);
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

            send_mouse(a.panel, WM_LBUTTONDOWN, bx, by);
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

        // 首次运行：贴到工作区右上角
        if !st.cfg.placed {
            let (wa, _) = work_area();
            st.cfg.x = wa.right - st.cfg.w - 24;
            st.cfg.y = wa.top + 24;
            st.cfg.placed = true;
        }
        clamp_to_work_area(&mut st.cfg);

        let boxed = Box::new(st);
        APP_PTR = Box::into_raw(boxed);
        let a = app();

        if !bootstrap(a) {
            return 2;
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

fn work_area() -> (RECT, RECT) {
    unsafe {
        let mut r: RECT = zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut r as *mut _ as *mut c_void, 0);
        let screen = RECT {
            left: 0,
            top: 0,
            right: r.right.max(1280),
            bottom: r.bottom.max(720),
        };
        (r, screen)
    }
}

fn clamp_to_work_area(cfg: &mut Config) {
    let (wa, _) = work_area();
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
            a.accent_alpha = alpha as i32;
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
            a.accent_alpha = alpha as i32;
        }
    } else {
        unsafe { SetLayeredWindowAttributes(a.panel, 0, alpha, LWA_ALPHA) };
        a.accent_alpha = alpha as i32;
    }
    unsafe {
        ShowWindow(a.panel, SW_SHOWNOACTIVATE);
        InvalidateRect(a.panel, null(), 0);
    }
}

// ---------------------------------------------------------------- 模式 / 穿透

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
}

/// 出一帧并上屏。**会清掉脏标记** —— 刚出的这一帧就是当前状态。
fn render_and_present(a: &mut App) {
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
fn apply_resize(a: &mut App, x: i32, y: i32, w: i32, h: i32) {
    let resized = w != a.cfg.w || h != a.cfg.h;
    a.cfg.x = x;
    a.cfg.y = y;
    a.cfg.w = w;
    a.cfg.h = h;
    if resized {
        // 跑马灯一条要走多远由宽度决定，宽度变了得重新夹一次偏移，
        // 否则时间日期块会留在已经被裁掉的区域里
        normalize_clock_offset(a);
        mark_dirty(a);
    }
    apply_geometry(a, true, false);
    render_if_due(a, false);
}

/// 只把一帧渲染进 `frame`，不上屏。自检路径也走这里，保证测的就是真管线。
///
/// 一帧要跑三趟 GDI 绘制，得到三条独立的掩码通道：
///
/// ```text
///   ① 白字黑底画「时钟 + 分隔线 + 格言」 → mask   ─┐
///   ② 白字黑底画「日期」                → dmask  ─┼→ compose(主色, 日期色) → 预乘 ARGB
///   ③ 黑字白底画「全部内容」            → shad   ─┘   （只贡献 alpha，不碰颜色）
/// ```
///
/// 拆三条通道是「颜色可配置」的代价：同一条通道只能有一个颜色，
/// 而时钟和日期天生就该是两个颜色（主次分明）。多画一遍的成本在
/// 620×176 上可以忽略（实测 25fps 下 CPU 占用不到 1%）。
fn build_frame(a: &mut App, opacity: u8) {
    let show_date = a.cfg.show_date;
    let shadow = a.cfg.shadow;
    let (color, date_color) = (a.cfg.color, a.cfg.date_color);
    let shk: u8 = if shadow { 170 } else { 0 };

    // 时间日期块的几何先算出来：绘制要用它排版，wndproc 要用它做拖动命中。
    // 命中判定在 wndproc 里，那里没有 DC 去量文字，所以位置必须缓存在 App 上。
    let cb = layout_clock(&a.cfg, &a.fonts, a.frame.dc());
    let crect = RECT {
        left: cb.x,
        top: cb.y,
        right: cb.x + cb.w.max(CLOCK_MIN_GRAB),
        bottom: cb.y + cb.h.max(CLOCK_MIN_GRAB),
    };

    // 记下这一帧用的是哪一刻的时钟 —— `needs_frame` 拿它判断下一拍要不要重绘
    a.clock_stamp = clock_stamp(&a.cfg);
    a.clock_rect = crect;

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
    };

    // ① 主内容掩码
    {
        let dc = rc.frame.begin_text();
        paint_main(&mut rc, dc, WHITE, &cb);
    }
    rc.frame.extract_text();

    // ② 日期掩码。日期关掉时必须显式清空 —— 否则上一帧的日期会残留在缓冲区里
    if show_date {
        {
            let dc = rc.frame.begin_date();
            paint_date(&mut rc, dc, WHITE, &cb);
        }
        rc.frame.extract_date();
    } else {
        rc.frame.clear_date_mask();
    }

    // ③ 阴影掩码
    if shadow {
        let dc = rc.frame.begin_shadow();
        paint_main(&mut rc, dc, BLACK, &cb);
        paint_date(&mut rc, dc, BLACK, &cb);
        rc.frame.extract_shadow();
    } else {
        rc.frame.clear_shadow_mask();
    }

    // ④ 预乘合成：颜色在这里才乘进去
    rc.frame.compose(opacity, shk, color, date_color);
}

/// 时间日期块的几何。
///
/// 位置 = **默认位置**（左侧、垂直居中）+ 用户拖动偏移，再夹进窗口内。
/// 夹取这一步是「位置可拖动」和「拖完找不回来」之间的分界线：
/// 无论怎么拖，整块内容始终至少完整可见地留在窗口里。
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
}

fn layout_clock(cfg: &Config, fonts: &Fonts, dc: HDC) -> ClockBox {
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

/// 主内容：时钟 + 分隔线 + 格言 + 布局提示。`color` 只是掩码用的绘制色
/// （WHITE = 正文掩码，BLACK = 阴影掩码），跟最终显示的颜色无关。
fn paint_main(rc: &mut RC, dc: HDC, color: u32, cb: &ClockBox) {
    let shadow_pass = color == BLACK;
    let cfg = rc.cfg;
    let scale = cfg.scale as f32 / 100.0;
    let (w, h) = (cfg.w, cfg.h);
    let pad = ((24.0 * scale).round() as i32).max(10);
    // 阴影偏移：往右下挪一点
    let (ox, oy) = if shadow_pass {
        ((2.0 * scale).round() as i32, (2.0 * scale).round() as i32)
    } else {
        (0, 0)
    };

    let clock_face = if w as f32 >= 430.0 * scale {
        rc.fonts.clock
    } else {
        rc.fonts.clock_small
    };

    // ---- 时钟
    if cb.show {
        if let Some(s) = &cb.time {
            unsafe { SelectObject(dc, clock_face as HGDIOBJ) };
            gfx::draw_text(dc, s, cb.x + ox, cb.y + oy, color);
        }
    }

    // ---- 分隔线与格言带。
    //
    // `band_l` 必须在**两趟绘制里算出同一个值** —— 只有「画不画那根线」随趟次变。
    // 反例（真实踩过的坑）：把整个 if 都用 `!shadow_pass` 包住，阴影趟的
    // `band_l` 就会退回 `pad`，于是格言的阴影用了一个更宽的裁剪区，
    // 结果在时钟区域留下一片黑色的影子幽灵 —— 阴影只抬 alpha 不碰颜色，
    // 落在没有字形的像素上合成出来就是半透明黑。
    let mut band_l = pad;
    let band_r = w - pad;
    if cfg.show_quote && cb.show {
        // 线的位置只由块的**宽度**决定（不跟着拖动偏移跑），
        // 这样拖动时间不会连带把格言带挤窄。
        let dx = pad + cb.w + pad;
        if !shadow_pass && dx < band_r {
            let line = RECT {
                left: dx,
                top: pad + h / 4,
                right: dx + 1,
                bottom: h - pad,
            };
            unsafe { FillRect(dc, &line, rc.brush) };
        }
        band_l = dx + pad;
    }

    // ---- 格言
    if cfg.show_quote && band_r > band_l + 20 {
        let (band_t, band_b) = (pad, h - pad);
        let s = wide(&rc.list[rc.qi].display());
        unsafe { SelectObject(dc, rc.fonts.quote as HGDIOBJ) };
        let (tw, th) = gfx::measure_text(dc, &s);
        let ty = (h - th) / 2 + oy;

        match cfg.anim {
            Anim::Marquee => {
                let gap = (130.0 * scale).round() as i32;
                let s2 = wide(&rc.list[rc.qn].display());
                let x1 = (band_r as f32 - rc.m).round() as i32;
                let x2 = x1 + tw + gap;
                gfx::with_clip(dc, band_l, band_t + oy, band_r, band_b + oy, |dc| {
                    gfx::draw_text(dc, &s, x1 + ox, ty, color);
                    gfx::draw_text(dc, &s2, x2 + ox, ty, color);
                });
            }
            _ => {
                let x = (band_l + (band_r - band_l - tw) / 2).max(band_l);
                gfx::with_clip(dc, band_l, band_t + oy, band_r, band_b + oy, |dc| {
                    gfx::draw_text(dc, &s, x + ox, ty, color);
                });
            }
        }
    }

    // ---- 布局模式的提示与边框（阴影层不画）
    if cfg.layout_mode && !shadow_pass {
        let hint = wide("布局模式  ·  拖空白处移动 / 拖时间移动时间 / 四边缩放  ·  Ctrl+Alt+E 退出");
        unsafe { SelectObject(dc, rc.fonts.hint as HGDIOBJ) };
        let (hw, _) = gfx::measure_text(dc, &hint);
        gfx::draw_text(dc, &hint, (w - hw).max(0) / 2, (h - pad - 2).max(0), 0x8C8C8C);

        // 时间日期块的位置框：告诉用户「这块是可以单独拖的」
        if cb.show {
            let t = 1;
            let eb = unsafe { CreateSolidBrush(0x8A8A8A) };
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
        let edge = 0x969696;
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

/// 日期：单独一条掩码通道，所以能用独立的日期色。
fn paint_date(rc: &mut RC, dc: HDC, color: u32, cb: &ClockBox) {
    if !cb.show {
        return;
    }
    let Some(s) = &cb.date else { return };
    let shadow_pass = color == BLACK;
    let scale = rc.cfg.scale as f32 / 100.0;
    let (ox, oy) = if shadow_pass {
        ((2.0 * scale).round() as i32, (2.0 * scale).round() as i32)
    } else {
        (0, 0)
    };
    unsafe { SelectObject(dc, rc.fonts.date as HGDIOBJ) };
    let y = cb.y + (if cb.time.is_some() { cb.time_h + cb.gap } else { 0 });
    gfx::draw_text(dc, s, cb.x + ox, y + oy, color);
}


// ---------------------------------------------------------------- 动效

fn tick(a: &mut App) {
    let now = std::time::Instant::now();
    let dt = (now - a.last_tick).as_secs_f32().min(0.25);
    a.last_tick = now;
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

    match a.cfg.anim {
        Anim::Marquee => {
            a.alpha = 1.0;
            let band_w = marquee_band_w(&a.cfg);
            if !a.primed {
                // 首帧直接贴到带左缘，用户一启动就能读到，而不是等它慢慢滑进来
                a.primed = true;
                a.m = band_w;
            }
            a.m += a.cfg.speed as f32 * dt;
            if a.m >= band_w + a.cur_w as f32 {
                let gap = 130.0 * scale;
                a.m -= a.cur_w as f32 + gap;
                a.qi = a.qn;
                a.qn = (a.qn + 1) % n;
                a.cur_w = measure_quote(a, &a.list[a.qi]);
            }
        }
        Anim::Static => {
            a.alpha = 1.0;
            a.phase += dt;
            if a.phase > 15.0 {
                a.phase = 0.0;
                advance(a);
            }
        }
        Anim::Fade => {
            a.phase += dt;
            let t = a.phase;
            if t >= 6.4 {
                a.phase = 0.0;
                a.alpha = 0.0;
                advance(a);
            } else if t < 0.7 {
                a.alpha = smoothstep(t / 0.7);
            } else if t < 5.7 {
                a.alpha = 1.0;
            } else {
                a.alpha = 1.0 - smoothstep((t - 5.7) / 0.7);
            }
        }
        Anim::Breathe => {
            a.phase += dt;
            let period = 5.0;
            let t = (a.phase % period) / period;
            a.alpha = 0.78 + 0.22 * (t * std::f32::consts::TAU).sin();
        }
    }

    // 呼吸时毛玻璃浓度跟着起伏，但要节流：
    // SetWindowCompositionAttribute 每帧调一次会让 DWM 反复重建背景
    if a.cfg.anim == Anim::Breathe && a.should_show_panel() {
        let want = a.panel_alpha() as i32;
        if (want - a.accent_alpha).abs() >= 4 {
            apply_glass(a);
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
    match a.cfg.anim {
        // 跑马灯只影响格言；格言关掉了就只剩时钟，和静态没区别
        Anim::Marquee => a.cfg.show_quote,
        // 这两种动的是整体 alpha（时钟也跟着一起呼吸），必须逐帧出
        Anim::Fade | Anim::Breathe => true,
        // 静态：相位只用于计时换句，换句那一刻会自己标脏
        Anim::Static => false,
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

fn register_hotkeys(a: &mut App) {
    const M: u32 = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
    let table: [(i32, u32); 17] = [
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
        (HK_QUIT, VK_X),
    ];
    for (id, vk) in table {
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
            a.cfg.anim = a.cfg.anim.next();
            a.phase = 0.0;
            a.m = 0.0;
            a.alpha = 1.0;
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
            a.alpha = 1.0;
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
        HK_CLOCK => on_menu(a, MI_CLOCK),
        HK_DATE => on_menu(a, MI_DATE),
        HK_SIZE_UP => on_menu(a, MI_SCALE_UP),
        HK_SIZE_DOWN => on_menu(a, MI_SCALE_DOWN),
        HK_COLOR => on_menu(a, MI_COLOR),
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

/// 子菜单挂到父菜单上。`TrackPopupMenu` 返回的仍然是子项自己的 id，
/// 所以 `on_menu` 里的匹配不需要关心层级。
fn submenu(menu: *mut c_void, sub: *mut c_void, text: &str) {
    let s = wide(text);
    unsafe { AppendMenuW(menu, MF_POPUP | MF_STRING, sub as usize, s.as_ptr()) };
}

fn show_menu(a: &mut App) {
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let op_pct = (a.cfg.opacity * 100 / 255).to_string();

        menu_add(menu, MI_TOGGLE, "显示 / 隐藏  (Ctrl+Alt+H)", a.cfg.visible, true);
        sep(menu);
        menu_add(menu, MI_GLASS, Mode::Glass.label(), a.cfg.mode == Mode::Glass, true);
        menu_add(menu, MI_TEXT, Mode::TextOnly.label(), a.cfg.mode == Mode::TextOnly, true);
        menu_add(menu, MI_LAYOUT, "布局模式（可拖动 / 缩放）", a.cfg.layout_mode, true);
        sep(menu);

        // ---- 显示内容
        let m_show = CreatePopupMenu();
        menu_add(m_show, MI_CLOCK, "显示时间  (Ctrl+Alt+C)", a.cfg.show_clock, true);
        menu_add(m_show, MI_DATE, "显示日期  (Ctrl+Alt+D)", a.cfg.show_date, true);
        menu_add(m_show, MI_QUOTE, "显示格言", a.cfg.show_quote, true);
        sep(m_show);
        menu_add(m_show, MI_SECONDS, "时钟显示秒", a.cfg.seconds, true);
        submenu(menu, m_show, "显示内容");

        // ---- 外观：字体 / 字号 / 颜色
        let m_look = CreatePopupMenu();
        menu_add(
            m_look,
            MI_FONT,
            &format!("字体：{}", a.cfg.font),
            false,
            true,
        );
        sep(m_look);
        menu_add(
            m_look,
            MI_SCALE_UP,
            &format!("整体字号 {}%  +", a.cfg.scale),
            false,
            true,
        );
        menu_add(m_look, MI_SCALE_DOWN, "整体字号  -", false, true);
        sep(m_look);
        menu_add(
            m_look,
            MI_CLOCK_UP,
            &format!("时钟字号 {}%  +", a.cfg.clock_scale),
            false,
            true,
        );
        menu_add(m_look, MI_CLOCK_DOWN, "时钟字号  -", false, true);
        menu_add(
            m_look,
            MI_QUOTE_UP,
            &format!("格言字号 {}%  +", a.cfg.quote_scale),
            false,
            true,
        );
        menu_add(m_look, MI_QUOTE_DOWN, "格言字号  -", false, true);
        sep(m_look);
        menu_add(
            m_look,
            MI_COLOR,
            &format!("文字颜色：{}  (Ctrl+Alt+K)", fmt_color(a.cfg.color)),
            false,
            true,
        );
        menu_add(
            m_look,
            MI_DATE_COLOR,
            &format!("日期颜色：{}", fmt_color(a.cfg.date_color)),
            false,
            true,
        );
        menu_add(m_look, MI_SHADOW, "文字阴影", a.cfg.shadow, true);
        submenu(menu, m_look, "外观");

        // ---- 动效
        let m_anim = CreatePopupMenu();
        for (i, an) in Anim::all().iter().enumerate() {
            menu_add(
                m_anim,
                MI_ANIM_BASE + i as u32,
                &format!("动效：{}", an.label()),
                a.cfg.anim == *an,
                true,
            );
        }
        sep(m_anim);
        menu_add(m_anim, MI_SPEED_UP, "跑马灯加速", false, a.cfg.anim == Anim::Marquee);
        menu_add(m_anim, MI_SPEED_DOWN, "跑马灯减速", false, a.cfg.anim == Anim::Marquee);
        submenu(menu, m_anim, "动效");
        sep(menu);

        menu_add(menu, MI_OP_UP, &format!("提高不透明度  ({}%)", op_pct), false, true);
        menu_add(menu, MI_OP_DOWN, "降低不透明度", false, true);
        menu_add(menu, MI_TOPMOST, "总在最前", a.cfg.always_on_top, true);
        sep(menu);
        menu_add(menu, MI_PREV, "上一条格言  (Ctrl+Alt+←)", false, true);
        menu_add(menu, MI_NEXT, "下一条格言  (Ctrl+Alt+→)", false, true);
        menu_add(menu, MI_CLOCK_RESET, "重置时间日期位置", false, a.clock_visible());
        menu_add(menu, MI_RELOAD, "重新载入格言 / 配置", false, true);
        menu_add(menu, MI_RESET, "挂件回到右上角", false, true);
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

fn on_menu(a: &mut App, cmd: u32) {
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
        MI_OP_UP => bump_opacity(a, 12),
        MI_OP_DOWN => bump_opacity(a, -12),
        MI_TOPMOST => {
            a.cfg.always_on_top = !a.cfg.always_on_top;
            apply_topmost(a);
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
            let (wa, _) = work_area();
            a.cfg.x = wa.right - a.cfg.w - 24;
            a.cfg.y = wa.top + 24;
            sync_from_panel(a);
            a.cfg.save();
        }
        MI_QUIT => quit(a),
        c if (MI_ANIM_BASE..MI_ANIM_BASE + 4).contains(&c) => {
            a.cfg.anim = Anim::all()[(c - MI_ANIM_BASE) as usize];
            a.phase = 0.0;
            a.m = 0.0;
            a.alpha = 1.0;
            a.cfg.save();
            render_and_present(a);
        }
        _ => {}
    }
}

fn quit(a: &mut App) {
    unregister_hotkeys(a);
    del_tray(a);
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
    let (wa, _) = work_area();
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

/// 把当前偏移夹进合法范围，并**回写夹取后的实际偏移**。
///
/// 这一步是拖动跟手的关键：如果只夹显示位置、不回写偏移，
/// 用户拖出边界 200px 再往回拖时，前 200px 光标动了但内容不动 —— 空走。
fn normalize_clock_offset(a: &mut App) {
    let cb = layout_clock(&a.cfg, &a.fonts, a.frame.dc());
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
