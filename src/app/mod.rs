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

mod diagnostics;
mod frame;
mod input;
mod render;
mod verify;
mod window;

// 兄弟模块之间靠 `use super::*` 互见，所以把各子模块的面汇总到这里。
// 注意 glob **不会链式传递**：`verify/` 下的子文件由 `verify/mod.rs` 自己再导出。
pub(crate) use diagnostics::export as export_diagnostics;
pub(crate) use frame::*;
pub(crate) use input::*;
pub(crate) use render::*;
pub(crate) use window::*;

// 其它模块（`config_ui` / `bench` / `gfx`）与 `main` 只认这几个名字。
// 实现散在各个子模块里，在此集中再导出，免得调用方要知道内部怎么分。
pub(crate) use input::on_menu;
pub(crate) use verify::{
    cfgshot, clicktest, dragbench, dragtest, dump, dump_island, favtest, icontest, islandtest,
    mathdemo, mathquote, memtest, perf, probe, selftest,
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
use crate::island;
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

/// 消息泵自身的失败码（`GetMessageW` 返回 -1）。0 = 正常。
///
/// `GetMessageW` 只有三种返回：>0 有消息、0 `WM_QUIT`、**-1 出错**。
/// 出错时既不能吞掉继续循环（那是 100% CPU 空转），也不该无声 `break`
/// —— 挂件会凭空消失，用户和我们都拿不到任何线索。记下 `GetLastError`
/// 再退，诊断报告里能看到它。
pub(crate) static PUMP_ERR: AtomicU32 = AtomicU32::new(0);

/// 单实例互斥体创建失败码（0 = 成功）。失败时保护失效但仍会启动 ——
/// 这条只用来解释「为什么同时出现了两个挂件」。
pub(crate) static SINGLE_ERR: AtomicU32 = AtomicU32::new(0);

/// 单实例互斥体失败码（0 = 正常）
pub(crate) fn single_err() -> u32 {
    SINGLE_ERR.load(Ordering::Relaxed)
}

/// 消息泵失败码（0 = 正常退出）
pub(crate) fn pump_err() -> u32 {
    PUMP_ERR.load(Ordering::Relaxed)
}

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

/// 字号是否跟随所在显示器的 DPI（高 DPI 屏上物理尺寸一致）
pub(crate) const MI_DPI: u32 = 77;

/// 适应当前屏幕：把窗口缩进工作区、字号同比缩、回到该屏右上角
pub(crate) const MI_FIT: u32 = 78;

/// 暂停 / 继续轮播（保留当前句）
pub(crate) const MI_PAUSE: u32 = 79;

/// 仅播放收藏夹（收藏为空时回退内置库）
pub(crate) const MI_FAV_ONLY: u32 = 80;

/// 备份配置 / 收藏到数据目录
pub(crate) const MI_BACKUP: u32 = 81;

/// 导出纯文本诊断报告
pub(crate) const MI_DIAG: u32 = 82;

// ---- 灵动岛（与格言并列的独立功能）
//
// 只有三条命令：总开关、描边、未读计数。岛的颜色/尺寸走 `config.ini`
// 或设置面板的数值行 —— 菜单里塞一堆「胶囊底不透明度 +2」没有意义。
/// 显示 / 隐藏灵动岛（开则建窗、关则**销毁**）
pub(crate) const MI_ISLAND: u32 = 83;

/// 是否统计未读数（关掉只显示最近一条）
pub(crate) const MI_ISLAND_UNREAD: u32 = 84;

/// 落下 island_state：收起 / 展开（P2 的「钉住」先用它验证落盘）
pub(crate) const MI_ISLAND_COLLAPSE: u32 = 85;
pub(crate) const MI_ISLAND_EXPAND: u32 = 86;

// 灵动岛设置页的命令码。**只出现在设置面板里**（菜单里不重复，与其它设置项
// 同一口径）。片段开关用 `BASE + 0..6`（**七项**），顺序与
// `content::ALL_ITEMS` 一致。
//
// ⚠️ 加第七项（媒体）时踩过一次：`BASE + 6` 正好等于 `ROT_DOWN = 110`，
// 自检报的是「命令码重复」，而症状会是「媒体勾选框调的是轮播间隔」。
// 所以后面这一段整体后移了一位，`BASE` 段要有**与 `ALL_ITEMS` 等长**的空间。
// ⚠️ 起始值必须**大于现有最大命令码**（`MI_QUIT = 99`）。第一版从 87 起排，
// 排到 99 正好撞上 `MI_QUIT` —— 症状是「退出」项变成「展开宽 +20」，
// 而 clippy 报的是 `MI_QUIT` 那条 match 分支不可达。命令码是手工分配的，
// 加新段落时先看一眼这个文件的尾部。
pub(crate) const MI_ISLAND_ITEM_BASE: u32 = 104;
pub(crate) const MI_ISLAND_ROT_DOWN: u32 = 111;
pub(crate) const MI_ISLAND_ROT_UP: u32 = 112;
pub(crate) const MI_ISLAND_OP_DOWN: u32 = 113;
pub(crate) const MI_ISLAND_OP_UP: u32 = 114;
pub(crate) const MI_ISLAND_BODY_COLOR: u32 = 115;
pub(crate) const MI_ISLAND_ACCENT_COLOR: u32 = 116;
pub(crate) const MI_ISLAND_W_DOWN: u32 = 117;
pub(crate) const MI_ISLAND_W_UP: u32 = 118;
pub(crate) const MI_ISLAND_H_DOWN: u32 = 119;
pub(crate) const MI_ISLAND_H_UP: u32 = 120;
pub(crate) const MI_ISLAND_KEEP: u32 = 121;
/// 新消息拉长时长（`island_peek_ms`，按整秒调）
pub(crate) const MI_ISLAND_PEEK_DOWN: u32 = 122;
pub(crate) const MI_ISLAND_PEEK_UP: u32 = 123;
/// 新消息提示点颜色（`island_dot_color`）
pub(crate) const MI_ISLAND_DOT_COLOR: u32 = 124;
/// 桌面适配一键切换（自定义 → 浅底 → 深底 → 自定义）。
/// **别叫 `MI_FIT`** —— 那个名字是「适应当前屏幕」（78），两者语义不同，
/// 撞名会让按钮静默跑到缩放逻辑上去。
pub(crate) const MI_DESK_FIT: u32 = 125;

/// 竖版形态单选命令：87 / 88 / 89 = A 条幅 / B 极简 / C 栏头
///
/// 只影响竖版。横版下点了也存下来（下次切回竖版就是它），不弹提示 ——
/// 三个形态共用一块画布，没有「无效操作」这回事。
///
/// ⚠️ 87–98 是 `MI_QUIT = 99` 与灵动岛段（83–86、104 起）之间的**唯一空档**。
/// 原来排在 84，与 `MI_ISLAND_UNREAD` 撞了 —— 那种重复编译期不报错，
/// 症状是「点竖排极简把未读计数关了」，只有 `--selftest` 的命令码去重能抓到。
pub(crate) const MI_VSTYLE_BASE: u32 = 87;

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
    /// 是否已经有一层代码持有 `&mut App`。
    ///
    /// Win32 的 `TrackPopupMenu` / `DestroyWindow` / `ShowWindow` 会在同一线程
    /// 同步重入窗口过程。若这时再构造一个 `&mut App`，同一对象就同时存在两个
    /// 可变引用 —— 这是 UB，不是「单线程所以没事」。优化器据此把状态读提升成
    /// 常量，项目已经真实踩到过。
    ///
    /// 这里把重入的消息交回 `DefWindowProcW`：外层操作完成后会主动补一帧 /
    /// 落一次几何；嵌套消息不需要、也不允许碰应用状态。这样整个进程里
    /// 任何时刻最多只有一个 `&mut App`。
    static APP_BUSY: Cell<bool> = const { Cell::new(false) };
}

/// 独占访问 `App`。已经有外层访问时返回 `None`，绝不构造第二个可变引用。
///
/// 闭包退出时立即释放闸门；`panic` 展开也会释放。重入的窗口消息走
/// `DefWindowProcW`，由外层动作结束后统一刷新。
pub(crate) fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP_BUSY.with(|busy| {
        if busy.get() {
            return None;
        }
        busy.set(true);
        struct Gate;
        impl Drop for Gate {
            fn drop(&mut self) {
                APP_BUSY.with(|b| b.set(false));
            }
        }
        let _gate = Gate;
        let p = app_ptr_raw();
        assert!(!p.is_null(), "APP_PTR 未初始化：调用 with_app 前必须先 bootstrap");
        Some(f(unsafe { &mut *p }))
    })
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


/// **仅供 CLI 验证程序**：验证入口独占进程，自己先建 App、再 pump 消息。
///
/// 生产窗口过程与消息泵一律走 `with_app`，不会调用这里。验证代码持有这个引用
/// 期间会主动泵消息，因此它仍属于「诊断专用」的裸指针访问 —— 不在主程序路径上。
///
/// 新代码不要使用；验证入口的长期方向是拆成短作用域 `with_app`。
pub(crate) unsafe fn app() -> &'static mut App {
    let p = app_ptr_raw();
    assert!(!p.is_null(), "APP_PTR 未初始化：调用 app() 前必须先 bootstrap");
    &mut *p
}

/// 本模块的 `HINSTANCE`。建窗口时要它 —— 设置面板（`config_ui`）在另一个模块里，
/// 拿不到 `App.hinst`（那是私有字段），而这个句柄对当前进程是唯一的，
/// 问系统要一次比敞一个字段干净。
pub(crate) fn module_handle() -> *mut c_void {
    unsafe { GetModuleHandleW(null()) }
}

// ---------------------------------------------------------------- 状态

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    /// 当前这句已停留的秒数（dwell 计时器）。换句 / 切动效时归零。
    ///
    /// **与呼吸相位分开**：旧实现两者共用一个 `phase`，于是呼吸模式下
    /// 换句把相位归零 = 亮度从曲线谷底重跳一次；而呼吸模式下不累计 dwell
    /// = 永远不换句（455 条库里只有第一条会被看到）。
    phase: f32,
    /// 呼吸相位（秒，只在 `Anim::Breathe` 下推进）。`quote_mod` 用它算 k。
    pulse: f32,
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
    /// 横版的竖分隔线用的画刷（很淡，0x50 档）
    brush: HBRUSH,
    /// 竖版页头下方那条**界栏**用的画刷。比 `brush` 亮一档：界栏是 A/C 两种
    /// 形态唯一的分块手段，淡到看不见就等于没画（实测 0x50 档只把底色推动
    /// 3/255，屏幕上根本认不出有一条线）。
    rule_brush: HBRUSH,

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
    /// **时钟刻度也必须在内**：`ClockBox` 缓存的不只是几何，还带着已经格式化好的
    /// 时间/日期字符串；漏掉它就会出现「定时器在跳、画面上的时间永远不动」。
    clock_cached: Option<Box<ClockBox>>,
    clock_norm_key: (u64, i32, i32, i32, i32, i32, u32, u32, u32, i32, i32, u64),
    /// 竖版「卷轴整体居中」的偏移缓存（审查 U5）：`(指纹, 右缘左移量)`。
    /// 指纹 = (qi, font_epoch, w, h, cb.w) —— 覆盖偏移的全部输入。
    /// 命中时 `frame_layout` 直接按缓存偏移排版，只量一次；未命中才两趟
    /// （先量extent算偏移、再按偏移排）。用 `Cell` 是因为 `frame_layout`
    /// 拿的是 `&App`，而它每条出帧路径都会走。
    v_center: Cell<((usize, u64, i32, i32, i32), i32)>,
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
    /// 灵动岛：**与格言并列的独立功能**（`src/island/`）。
    ///
    /// 它有自己的窗口、定时器、排版与绘制，从不写 `cfg.w/h/x/y`，
    /// 也不读 `cfg.visible` / `orient` / `anim`。`island=0` 时窗口不存在。
    pub(crate) island: island::Island,
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
        let fonts = Fonts::build(
            &cfg.font,
            cfg.eff_scale_pct(),
            cfg.clock_scale,
            cfg.quote_scale,
        );
        let frame = Frame::new(cfg.w, cfg.h);
        // 分隔线画刷：走**主色通道**、强度 0x50。
        // 旧值 0x505050 是三通道同强度，在只读 byte0 的旧实现里恰好等于 0x50；
        // 换到「通道即语义」的新布局就必须只说一个通道，否则会被判成三色叠加。
        let brush = unsafe { CreateSolidBrush(gfx::ch_dim(0x50)) };
        let rule_brush = unsafe { CreateSolidBrush(gfx::ch_dim(0xA8)) };
        let list = quotes::library_for(&data_dir(), cfg.fav_only);
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
            pulse: 0.0,
            // 减动开关：系统级的（WinRT UISettings）要引 windows crate，暂不接；
            // env 覆盖给便携用户和自检用（见 transition::Transition::new）。
            trans: transition::Transition::new(std::env::var("DG_NO_TRANSITION").is_ok()),
            cfg,
            list,
            fonts,
            font_epoch: 1,
            frame,
            brush,
            rule_brush,
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
                u64::MAX,
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
            v_center: Cell::new(((usize::MAX, 0, 0, 0, 0), 0)),
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
            island: island::Island::new(),
        })
    }

    /// 格言窗口是不是**因为全屏**而藏着。
    ///
    /// 岛要读它来做自己的显隐，但只读不写 —— 两个功能的 `fs_hidden`
    /// 记账各自独立，互相覆盖会出现「退出全屏后岛弹回来而格言没弹」。
    pub(crate) fn fs_hidden(&self) -> bool {
        self.fs_hidden
    }

    fn should_show_panel(&self) -> bool {
        self.cfg.mode == Mode::Glass || self.cfg.layout_mode
    }

    /// **生效动效的唯一出口**：竖版降级为静态 + 「放不下转跑马灯」两件事都算进去。
    ///
    /// 以前这套判定被抄了四份（`step` / `needs_frame` / `build_frame` / `quote_mod`），
    /// 而 `Config::eff_anim()` 只做了第一件 —— 名字像唯一真相，实际不是。
    /// 后果是 `quote_mod` 在「呼吸 + 放不下」时给整条跑马灯乘了一个冻结的
    /// 呼吸系数（实测亮度差 35 级），以及这次一并修掉的空转问题。
    /// 现在四处都读这一个方法，加第五种动效也不会再漏。
    pub(crate) fn eff_anim(&self) -> Anim {
        if self.force_marquee {
            Anim::Marquee
        } else {
            self.cfg.eff_anim()
        }
    }

    /// 跟随显示器 DPI。返回 true 表示真的变了（调用方据此重排 / 落盘）。
    ///
    /// **字号和窗口尺寸必须一起缩放**：字体按 DPI 放大而窗口不动，同一份版式
    /// 会立刻挤爆（这正是「换个分辨率就排不下」的来源）。
    pub(crate) fn apply_dpi(&mut self, dpi: i32) -> bool {
        let dpi = dpi.clamp(48, 480);
        if !self.cfg.dpi_auto {
            self.cfg.dpi = 100;
            return false;
        }
        let old = self.cfg.dpi.max(1);
        if dpi == old {
            return false;
        }
        let k = dpi as f32 / old as f32;
        self.cfg.dpi = dpi;
        self.cfg.dpi_known = true;
        self.cfg.w = ((self.cfg.w as f32 * k).round() as i32).max(MIN_W);
        self.cfg.h = ((self.cfg.h as f32 * k).round() as i32).max(MIN_H);
        // 两种版式的尺寸记忆也是物理值，必须与当前窗口同步换挡；
        // 否则在 150% 屏上切回另一版式，会拿到 100% 时记下的小尺寸。
        for (w, h) in [
            (&mut self.cfg.h_w, &mut self.cfg.h_h),
            (&mut self.cfg.v_w, &mut self.cfg.v_h),
        ] {
            *w = ((*w as f32 * k).round() as i32).clamp(MIN_W, 4000);
            *h = ((*h as f32 * k).round() as i32).clamp(MIN_H, 2000);
        }
        // 放大后可能超出工作区（尤其从 100% 屏拖到 200% 屏），先夹再重建
        clamp_to_work_area(&mut self.cfg);
        self.rebuild_fonts();
        true
    }

    /// 按当前播放源重载词库，并尽量把正在显示的同一句找回来。
    ///
    /// 收藏 / 切换「仅收藏夹」都会改变候选集合；只重置 `qi=0` 会出现
    /// 「收藏完画面自己跳了」。
    pub(crate) fn reload_library(&mut self) {
        let cur = self.list.get(self.qi).map(|q| q.display()).unwrap_or_default();
        self.list = quotes::library_for(&data_dir(), self.cfg.fav_only);
        if self.list.is_empty() {
            self.list = quotes::load_builtin();
        }
        self.qi = keep_index(&cur, &self.list, self.qi);
        self.qn = (self.qi + 1) % self.list.len();
        self.cur_w = measure_quote(self, &self.list[self.qi]);
        self.m = if self.primed { marquee_band_w(&self.cfg) } else { 0.0 };
        self.trans.settle();
    }

    /// 开关「跟随屏幕 DPI」。两个方向都**同比缩放窗口**：
    /// 字号变了而窗口不动，版式立刻挤爆（开）或缩成一小块（关）。
    pub(crate) fn set_dpi_auto(&mut self, on: bool, dpi: i32) {
        let old_eff = self.cfg.eff_scale_pct().max(1);
        self.cfg.dpi_auto = on;
        self.cfg.dpi = if on { dpi.clamp(48, 480) } else { 100 };
        self.cfg.dpi_known = true;
        let new_eff = self.cfg.eff_scale_pct().max(1);
        let k = new_eff as f32 / old_eff as f32;
        if (k - 1.0).abs() > 0.001 {
            self.cfg.w = ((self.cfg.w as f32 * k).round() as i32).max(MIN_W);
            self.cfg.h = ((self.cfg.h as f32 * k).round() as i32).max(MIN_H);
        }
        clamp_to_work_area(&mut self.cfg);
        self.rebuild_fonts();
    }

    /// 适应当前屏幕：窗口缩进工作区 → 字号**同比**缩 → 回到该屏右上角。
    ///
    /// 「窗口被缩了但字号没缩」= 版式挤爆，所以两者必须同一个系数。
    /// 这条菜单是给「换了分辨率 / 换了显示器」准备的：点一下，整套比例
    /// 就按新屏重新落一遍，不用自己猜该设多少。
    pub(crate) fn fit_to_screen(&mut self) {
        let before_w = self.cfg.w.max(1);
        clamp_to_work_area(&mut self.cfg);
        let k = self.cfg.w as f32 / before_w as f32;
        if k < 0.999 {
            self.cfg.scale = ((self.cfg.scale as f32 * k).round() as i32).clamp(30, 600);
            self.rebuild_fonts();
        }
        let (wa, _) = work_area_at(self.cfg.x, self.cfg.y, self.cfg.w);
        self.cfg.x = wa.right - self.cfg.w - 24;
        self.cfg.y = wa.top + 24;
        clamp_to_work_area(&mut self.cfg);
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
        } else if self.eff_anim() == Anim::Breathe {
            // 相位是**呼吸自己的时钟**（`pulse`），与 dwell 计时器分开：
            // 以前两者共用一个 `phase`，换句时把 phase 归零会让亮度从
            // 曲线起点重跳一次（每 4~10s 一次可见的亮度台阶）。
            (transition::breathe_k(self.pulse), 0)
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
        // 旧字体由 `Fonts::drop` 自动释放（P2-F），不再手工 destroy
        self.fonts = Fonts::build(
            &self.cfg.font,
            self.cfg.eff_scale_pct(),
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
    // 灵动岛：**独立窗口**，默认不建（`cfg.island = false`）。
    // 放在最后：`create` 里会读 `window_dpi`，那时窗口已经在了，
    // 读到的才是它真正所在屏的 DPI。
    if a.cfg.island {
        island::create(a);
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
        let m = CreateMutexW(null(), 0, name.as_ptr());
        if m.is_null() {
            // 创建失败（权限 / 句柄耗尽）：单实例保护失效，但这不该成为
            // 「程序打不开」的理由 —— 记下来，照常启动。
            SINGLE_ERR.store(GetLastError(), Ordering::Relaxed);
        } else if GetLastError() == ERROR_ALREADY_EXISTS {
            return 0;
        }

        let Some(mut st) = App::new() else {
            return 1;
        };
        st.hinst = GetModuleHandleW(null());

        // 首次运行：贴到**挂件所在屏**的工作区右上角（未放置时坐标是 0，即主屏）
        if !st.cfg.placed {
            // 新装默认**跟随屏幕 DPI**：这台机器上没有「旧行为」要保护，
            // 而 150% / 200% 屏上不跟的话第一眼就明显偏小。
            // 窗口按同一系数放大 —— 字号涨了窗口不涨，版式会立刻挤爆。
            st.cfg.dpi_auto = true;
            let dpi = crate::ffi::dpi_pct(crate::ffi::dpi_for_system().unwrap_or(96));
            st.cfg.dpi = dpi;
            st.cfg.dpi_known = true;
            let k = dpi as f32 / 100.0;
            st.cfg.w = ((st.cfg.w as f32 * k).round() as i32).max(MIN_W);
            st.cfg.h = ((st.cfg.h as f32 * k).round() as i32).max(MIN_H);
            let (wa, _) = work_area_at(st.cfg.x, st.cfg.y, st.cfg.w);
            st.cfg.x = wa.right - st.cfg.w - 24;
            st.cfg.y = wa.top + 24;
            st.cfg.placed = true;
        }
        clamp_to_work_area(&mut st.cfg);

        let boxed = Box::new(st);
        app_ptr_set(Box::into_raw(boxed));

        if !with_app(|a| bootstrap(a)).unwrap_or(false) {
            return 2;
        }

        with_app(|a| {
            // 窗口建出来了 ⇒ 现在能问「挂件实际落在哪块屏」的 DPI。
            // 首次运行上面用的是系统 DPI，这里按真实所在屏校正一次
            // （多屏 DPI 不同时，这一步才是对的）。
            let dpi = window_dpi(a.text);
            if !a.cfg.dpi_known {
                // 旧配置迁移：`w/h` 已经是保存时的物理值，没有对应的 DPI 可用于
                // 再次换算。只接管当前 DPI、不缩放尺寸，下一次启动起走正常往返。
                a.cfg.dpi = dpi.clamp(48, 480);
                a.cfg.dpi_known = true;
                a.rebuild_fonts();
                apply_geometry(a, true, true);
                render_and_present(a);
                a.cfg.save();
            } else if a.apply_dpi(dpi) {
                apply_geometry(a, true, true);
                render_and_present(a);
                a.cfg.save();
            }

            // 诊断用：`DG_OPEN_CONFIG=1` 时启动即把设置面板打开。
            // 与 `DG_MOVE` / `DG_LEGACY_DRAG` 同类 —— 只为让「打开设置窗」这件事
            // 在无人值守的脚本里可复现（截屏 / 回归），不设时零影响。
            if std::env::var("DG_OPEN_CONFIG").is_ok() {
                config_ui::open(a);
            }
        });

        // ---- 消息泵
        // 不在这里持 `&mut App`：每条消息由窗口过程独占取得，处理完立即释放。
        let mut msg: MSG = zeroed();
        loop {
            match GetMessageW(&mut msg, null_mut(), 0, 0) {
                // WM_QUIT：正常收尾
                0 => break,
                // -1：泵出错（无效窗口句柄等）。记下原因再退，
                // 别让「挂件突然没了」变成一件查不动的事。
                -1 => {
                    PUMP_ERR.store(GetLastError(), Ordering::Relaxed);
                    break;
                }
                _ => {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        0
    }
}

#[cfg(test)]
mod command_id_tests {
    use super::*;

    /// 命令码全局唯一（审查 P1-B）。
    ///
    /// 历史事故：岛命令从 87 起排，撞上 `MI_QUIT = 99`。靠「加新段落先看
    /// 一眼文件尾部」的口头纪律迟早再撞。这条测试把**全部** `MI_*` 收进
    /// 一个数组断言无重复 —— 新增命令码忘了避让，这里立刻红。
    ///
    /// 新增 `MI_*` 时：把常量加进 `named`；若它是一段连续码的基址，
    /// 把段内其余成员也展开（见下方三个循环），避免段与命名常量重叠漏检。
    #[test]
    fn 命令码全局唯一() {
        let mut ids: Vec<u32> = vec![
            MI_TOGGLE, MI_GLASS, MI_TEXT, MI_ANIM_BASE, MI_OP_UP, MI_OP_DOWN,
            MI_TOPMOST, MI_LAYOUT, MI_SECONDS, MI_SHADOW, MI_SPEED_UP, MI_SPEED_DOWN,
            MI_PREV, MI_NEXT, MI_RELOAD, MI_RESET, MI_CLOCK, MI_DATE, MI_QUOTE,
            MI_SCALE_UP, MI_SCALE_DOWN, MI_CLOCK_UP, MI_CLOCK_DOWN, MI_QUOTE_UP,
            MI_QUOTE_DOWN, MI_COLOR, MI_DATE_COLOR, MI_FONT, MI_CLOCK_RESET, MI_QUIT,
            MI_AUTOSTART, MI_AUTOHIDE, MI_FAV, MI_ORIENT_BASE, MI_ORIENT_V,
            MI_SPACING_UP, MI_SPACING_DOWN, MI_QUOTE_COLOR, MI_SHRINK_FIT, MI_CONFIG,
            MI_DPI, MI_FIT, MI_PAUSE, MI_FAV_ONLY, MI_BACKUP, MI_DIAG, MI_ISLAND,
            MI_ISLAND_UNREAD, MI_ISLAND_COLLAPSE, MI_ISLAND_EXPAND, MI_ISLAND_ITEM_BASE,
            MI_ISLAND_ROT_DOWN, MI_ISLAND_ROT_UP, MI_ISLAND_OP_DOWN, MI_ISLAND_OP_UP,
            MI_ISLAND_BODY_COLOR, MI_ISLAND_ACCENT_COLOR, MI_ISLAND_W_DOWN,
            MI_ISLAND_W_UP, MI_ISLAND_H_DOWN, MI_ISLAND_H_UP, MI_ISLAND_KEEP,
            MI_ISLAND_PEEK_DOWN, MI_ISLAND_PEEK_UP, MI_ISLAND_DOT_COLOR, MI_DESK_FIT,
            MI_VSTYLE_BASE,
        ];
        // 连续段展开（基址已在 named 里，这里只补段内其余成员）：
        // 动效 4 档、岛条目 7 项、竖排形态 3 档。版式 2 档（70/71）已全命名，无需展开。
        for k in 1..4 {
            ids.push(MI_ANIM_BASE + k);
        }
        for k in 1..7 {
            ids.push(MI_ISLAND_ITEM_BASE + k);
        }
        for k in 1..3 {
            ids.push(MI_VSTYLE_BASE + k);
        }
        ids.sort_unstable();
        for w in ids.windows(2) {
            assert_ne!(
                w[0], w[1],
                "命令码 {} 重复 —— 新增 MI_* 撞了已有值（历史事故：岛命令撞 MI_QUIT=99）",
                w[0]
            );
        }
    }
}
