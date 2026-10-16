//! 应用核心：状态、建窗引导与进程入口。
//!
//! 这里是**唯一持有 `App` 的地方**。其余模块只通过 `&mut App` 与它交互，
//! 不各自持有窗口句柄或配置副本：
//!
//! ```text
//!   app/mod.rs    状态 + 入口 + 建窗引导
//!   app/window.rs Win32 窗口：创建 / Acrylic / 穿透 / 几何
//!   app/render.rs 帧构建与绘制（掩码三通道 + 合批）
//!   app/frame.rs  帧调度：tick / step / needs_frame / 全屏守卫
//!   app/input.rs  窗口过程、拖拽、命中、热键、托盘、菜单
//!   app/verify/   自检 / 探测 / 单帧导出 / 各类基准
//! ```
//!
//! # 拆模块时不能破坏的铁律
//!
//! * `&mut App` **不能用来观测**（别名 UB，编译器会把字段读提升成循环外常量）——
//!   埋点必须走裸指针或独立静态原子量，`MSG_TALLY` / `WIDGET_MOUSE_HITS` 就是为此存在。
//! * 状态用 `thread_local!` 里的 `Cell` 而非 Mutex：`TrackPopupMenu` /
//!   `DestroyWindow` / `ShowWindow` 会**同步重入** wndproc，Mutex 必自锁。
//! * `cfg.x/y` 是**目标位置**，不是窗口实际位置；客户区↔屏幕换算必须读 `win_rect()`。

mod frame;
mod input;
mod render;
mod verify;
mod window;

// 兄弟模块之间靠 `use super::*` 互见，所以把各子模块的面汇总到这里。
// 注意 glob **不会链式传递**：`verify/` 下的子文件由 `verify/mod.rs` 自己再导出。
pub(crate) use frame::*;
pub(crate) use input::*;
pub(crate) use render::*;
pub(crate) use window::*;

// 其它模块（`config_ui` / `bench` / `gfx`）与 `main` 只认这几个名字。
// 实现散在各个子模块里，在此集中再导出，免得调用方要知道内部怎么分。
pub(crate) use input::on_menu;
pub(crate) use verify::{
    clicktest, dragbench, dragtest, dump, favtest, memtest, perf, probe, selftest,
};

use std::cell::Cell;
use std::ffi::c_void;
use std::mem::zeroed;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use windows_sys::Win32::Foundation::{GetLastError, HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{CreateSolidBrush, HBRUSH};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, MSG, SetTimer, ShowWindow, TranslateMessage};
use crate::transition;
use crate::config::{Anim, Config, Mode};
use crate::config_ui::{self, ConfigUi};
use crate::ffi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDPIAware, SetProcessDpiAwarenessContext};
use crate::gfx::{self, Fonts, Frame};
use crate::quotes::{self, Quote};
use crate::consts::*;
use crate::util::*;

// ---------------------------------------------------------------- 标识

pub(crate) const PANEL_CLASS: &str = "DeskGeyanPanel";

pub(crate) const TEXT_CLASS: &str = "DeskGeyanText";

pub(crate) const TIMER_ID: usize = 1;

pub(crate) const TIMER_MS: u32 = 40;

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
pub(crate) static MSG_TALLY: [AtomicU32; 256] = {
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
pub(crate) static WIDGET_MOUSE_HITS: AtomicU32 = AtomicU32::new(0);

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

pub(crate) fn tally_reset() {
    for c in MSG_TALLY.iter() {
        c.store(0, Ordering::Relaxed);
    }
}

pub(crate) fn tally_total() -> u32 {
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
pub(crate) fn target_pos() -> (i32, i32) {
    let p = app_ptr_raw();
    if p.is_null() {
        return (0, 0);
    }
    unsafe { ((*p).cfg.x, (*p).cfg.y) }
}

/// 计数非零的消息槽，按条数降序。标定输出直接用它。
pub(crate) fn tally_rows() -> Vec<(u32, u32)> {
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

pub(crate) const PANEL_R: u32 = 18;

pub(crate) const PANEL_G: u32 = 20;

pub(crate) const PANEL_B: u32 = 26;

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
pub(crate) const CLOCK_MIN_GRAB: i32 = 28;

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
pub(crate) fn app_ptr_set(p: *mut App) {
    APP_PTR.with(|c| c.set(p));
}

/// `APP_PTR` 的裸值（不判空），给 `target_pos()` 用 —— 它读的是 wndproc
/// 写进去的 `cfg.x/y`，必须在诊断侧绕过 `&mut App` 那套别名，见那边的注释。
fn app_ptr_raw() -> *mut App {
    APP_PTR.with(|c| c.get())
}

pub(crate) fn app() -> &'static mut App {
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
pub(crate) enum Glass {
    /// Acrylic 成功，底板交给 DWM 画
    Acrylic,
    /// Acrylic 不可用，退化为「整窗 alpha + 自绘实心底板」
    Layered,
}

pub(crate) struct App {
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
pub(crate) enum Grab {
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
pub(crate) const DRAG_FRAME_MS: u128 = 8;

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
pub(crate) const DRAG_MOVE_MS: u128 = 8;

/// 拖动期间定时器的周期（ms）。平时是 `TIMER_MS`(40)，拖动时临时收紧到和
/// 位置节拍同一个值。
///
/// 为什么需要它：位置落定是「有请求时才检查节拍」的，而**用户按着不动时没有请求** ——
/// 最后一次移动会被节流欠着，窗口停在 8ms 前的位置等下一次鼠标事件。
/// 拖着不停时看不出来（消息源源不断），但「停手但仍按住」就会看到窗口欠一格。
/// 让定时器在拖动期间按同一个节拍跑，就能把欠的那一格补上。
pub(crate) const DRAG_TICK_MS: u32 = 8;

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
pub(crate) enum MoveWay {
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
pub(crate) unsafe fn bootstrap(a: &mut App) -> bool {
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

// ---------------------------------------------------------------- 入口

pub(crate) fn run() -> i32 {
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
