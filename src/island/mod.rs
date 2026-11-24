//! 灵动岛：屏幕顶部居中的一枚胶囊。**与格言挂件并列的独立功能**。
//!
//! # 它为什么是独立窗口
//!
//! 规格书 §1.4 列了四条不能共用的理由（可见性 / 位置 / 尺寸 / 排版语义冲突）。
//! 这里只说工程上的结论：
//!
//! * 自己的窗口类 `DeskGeyanIsland`、自己的定时器、自己的 `tick`；
//! * 自己的排版（`layout.rs`）与绘制（`draw.rs`）；
//! * **从不写 `cfg.x/y/w/h`** —— 几何存在 `Island::rect` 里；
//! * 关掉 = `DestroyWindow`，所以 `island=0` 时进程里**没有这个窗口**。
//!
//! `frame.rs` 与 `render.rs` 因此一行都不用改，`island=0` 与基线逐位相同
//! 是**结构性**的，不需要专门去证明。
//!
//! # 为什么必须有自己的定时器
//!
//! `frame::tick` 里有一句 `if !a.cfg.visible || a.fs_hidden { return; }` ——
//! 那是**格言的可见性**。用户「只看岛、不要格言」时 `visible=0`，
//! 共用定时器会让岛在这一句里被静默饿死（症状是「岛不刷新了」，
//! 而且极难定位到是因为格言被隐藏）。独立定时器把这条从「要小心保住」
//! 变成「结构上不可能错」。

pub(crate) mod content;
pub(crate) mod draw;
pub(crate) mod ease;
pub(crate) mod geom;
pub(crate) mod layout;

use std::mem::zeroed;
use std::ptr::{null, null_mut};
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITORINFO, MonitorFromPoint, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetWindowRect, KillTimer,
    LoadCursorW, RegisterClassW, SetTimer, SetWindowPos, ShowWindow, HWND_TOPMOST,
    SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOMOVE, SWP_NOSIZE, SW_SHOWNOACTIVATE, SW_HIDE,
    WM_DISPLAYCHANGE, WM_DPICHANGED, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
};

use self::content::{Items, Samples};
use self::geom::{IslandRect, State};
use crate::app::{module_handle, window_dpi, with_app, App};
use crate::consts::*;
use crate::gfx::Frame;
use crate::util::wide;

/// 窗口类名。`--selftest` 用 `FindWindowW` 找它，判「island=0 时窗口不存在」。
pub(crate) const ISLAND_CLASS: &str = "DeskGeyanIsland";
/// 岛自己的定时器 id（与格言的 `TIMER_ID = 1` 分开）
const ISLAND_TIMER_ID: usize = 1;

pub(crate) struct Island {
    pub hwnd: HWND,
    pub state: State,
    /// 线性时间进度 0..1（几何再套缓动曲线）
    pub progress: f32,
    pub hover: geom::HoverState,
    /// 当前是否「想要展开」（悬停判定的意图，不是状态机状态）
    pub want: bool,
    pub samples: Samples,
    pub items: Items,
    pub rect: IslandRect,
    pub visible: bool,
    pub dirty: bool,
    /// 累计出帧数。自检靠它证明「静止零出帧」。
    pub renders: u32,
    /// 当前所在屏的 DPI 百分比（字体与几何都按它缩放）
    pub dpi: i32,
    /// 当前定时器周期（ms），用于判断要不要改
    pub tick_ms: u32,
    pub last_tick: Instant,
    /// 进入当前状态的时刻（`MIN_VISIBLE_MS` 用它）
    pub state_since: Instant,
    /// 收起端点的宽度（由内容算出，`draw` 每帧回写，下一拍算几何时用）
    pub compact_w: i32,
    /// 收起态一共分了几页（`draw` 每帧回写；`pages<=1` 时轮播恒不启动）
    pub pages: usize,
    /// 轮播相位（顺序淡出淡入）
    pub rot: geom::Rot,
    /// 轮播的内容系数：乘在掩码趟的 `tk` 上。**不动几何、不动胶囊底**
    pub rot_k: f32,
    /// 是否**消费**事件文件（把读过的搬进 `seen\`）。真运行 `true`；
    /// `--islandtest` / `--dump-island` 必须置 `false` —— 验证入口不许搬用户的
    /// 真实消息文件（家规：除 `--favtest` 外零副作用）。
    pub consume_events: bool,
    frame: Frame,
    fonts: Option<draw::IslandFonts>,
}

impl Island {
    pub fn new() -> Island {
        Island {
            hwnd: null_mut(),
            state: State::Collapsed,
            progress: 0.0,
            hover: geom::HoverState::default(),
            want: false,
            samples: Samples::default(),
            items: Items::parse("clock,dot"),
            rect: IslandRect::default(),
            visible: false,
            dirty: false,
            renders: 0,
            dpi: 100,
            tick_ms: geom::IDLE_TICK_MS,
            last_tick: Instant::now(),
            state_since: Instant::now(),
            compact_w: 0,
            pages: 1,
            rot: geom::Rot::default(),
            rot_k: 1.0,
            consume_events: true,
            frame: Frame::new(1, 1),
            fonts: None,
        }
    }

    pub fn on(&self) -> bool {
        !self.hwnd.is_null()
    }

    /// 几何用的展开度（已经过缓动）
    pub fn geo_t(&self) -> f32 {
        match self.state {
            State::Collapsed => 0.0,
            State::Expanded => 1.0,
            State::Expanding => ease::expand(self.progress),
            State::Collapsing => 1.0 - ease::collapse(self.progress),
        }
    }

    /// 内容相位用的展开度（**线性时间**，不进缓动曲线）
    pub fn content_t(&self) -> f32 {
        match self.state {
            State::Collapsed => 0.0,
            State::Expanded => 1.0,
            State::Expanding => self.progress,
            State::Collapsing => 1.0 - self.progress,
        }
    }

    /// 这一拍画面会不会自己变。**与格言的 `needs_frame` 没有任何共享代码**。
    pub fn needs_frame(&self) -> bool {
        // 轮播的淡出淡入**每拍都要出新帧**（k 在变），所以它也算「自己会变」。
        // 停在 Hold 时不算 —— 那正是「不轮播 / 只有一页 / 两页之间的等待」
        // 三种情形，一帧都不该出。
        self.state.morphing() || self.rot.fading() || self.dirty
    }

    /// 导出用：当前帧的像素（**预乘** ARGB）与尺寸。
    ///
    /// 只给 `--dump-island` 用。之所以不把 `frame` 直接开成 `pub`：
    /// `Frame` 里还有 `present` / `begin_*` 这些会改状态的入口，
    /// 对外只暴露「读一块像素」这一件事，导出的代码就不可能把画面搞脏。
    pub(crate) fn frame_pixels(&self) -> (&[u32], i32, i32) {
        (self.frame.out_pixels(), self.frame.w, self.frame.h)
    }
}

// ---------------------------------------------------------------- 几何

/// `island_screen` 记的是「上次所在屏左上角」，格式 `x,y`。
/// 解析失败或没设 ⇒ 主屏（`MonitorFromPoint(0,0)`）。
fn screen_point(cfg_screen: &str) -> POINT {
    let mut it = cfg_screen.split(',');
    let x = it.next().and_then(|s| s.trim().parse::<i32>().ok()).unwrap_or(0);
    let y = it.next().and_then(|s| s.trim().parse::<i32>().ok()).unwrap_or(0);
    POINT { x, y }
}

/// 岛所在屏的工作区。锚点用**工作区**（`rcWork`）而不是全屏区 ——
/// 任务栏在顶部时不该被胶囊盖住，与格言的 `clamp_to_work_area` 同一套口径。
fn work_area(cfg_screen: &str) -> RECT {
    let p = screen_point(cfg_screen);
    unsafe {
        let hmon = MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !hmon.is_null() && GetMonitorInfoW(hmon, &mut mi) != 0 {
            mi.rcWork
        } else {
            RECT {
                left: 0,
                top: 0,
                right: 1280,
                bottom: 720,
            }
        }
    }
}

/// 由「当前端点 + 环境」算出窗口矩形。纯计算，不碰窗口。
fn make_rect(dpi: i32, cfg_screen: &str, geo_t: f32, compact_w: i32, expanded_w: i32, expanded_h: i32) -> IslandRect {
    let k = dpi.max(48) as f32 / 100.0;
    let wa = work_area(cfg_screen);
    let margin = ((geom::ANCHOR_MARGIN_Y as f32) * k).round() as i32;
    let avail_w = (wa.right - wa.left).max(200);
    let env = geom::IslandEnv {
        cx: (wa.left + wa.right) / 2,
        top: wa.top + margin,
        compact_w: if compact_w > 0 { compact_w } else { ((geom::COMPACT_MIN_W as f32) * k) as i32 },
        // 展开宽度不超过工作区（留 32px 边距）
        expanded_w: expanded_w.min(avail_w - ((32.0 * k).round() as i32).max(8)),
        expanded_h,
    };
    geom::geom(geo_t, &env)
}

/// 估算收起态宽度（在还没有真实度量结果时的兜底）。
///
/// 中文字按 1em、ASCII 按 0.5em 估 —— 与 `layout` 用的真实度量最多差几个像素，
/// 而 `draw` 每帧会把真实宽度回写进 `compact_w`，下一拍就准了。
fn estimate_compact_w(samples: &Samples, items: &Items, dpi: i32) -> i32 {
    let k = dpi.max(48) as f32 / 100.0;
    let px = (13.0 * k).round() as i32;
    let pad = (geom::COMPACT_PAD_X as f32 * k).round() as i32;
    let gap = (geom::COMPACT_GAP as f32 * k).round() as i32;
    let segs = content::segments(samples, items, true);
    let mut total = 0;
    for (i, s) in segs.iter().enumerate() {
        if i > 0 {
            total += gap;
        }
        if s.dot {
            total += (6.0 * k).round() as i32;
        } else {
            for ch in s.text.chars() {
                total += if ch.is_ascii() { px / 2 } else { px };
            }
        }
    }
    (total + 2 * pad).max((geom::COMPACT_MIN_W as f32 * k).round() as i32)
}

// ---------------------------------------------------------------- 建 / 销

unsafe extern "system" fn island_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            // 闸门内处理；若外层已持有 `App`（例如正从 `on_menu` 里建窗），
            // 这一拍直接跳过 —— 与既有 `text_proc` 同一个约定。
            with_app(tick);
            0
        }
        WM_DPICHANGED | WM_DISPLAYCHANGE => {
            with_app(|a| {
                a.island.dpi = crate::app::window_dpi(a.island.hwnd);
                a.island.fonts = None; // 强制按新 DPI 重建
                recompute(a);
                a.island.dirty = true;
            });
            0
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

/// 创建窗口。**幂等**：已经存在就返回 true。
///
/// `island=0` 时调用方根本不会走到这里（`bootstrap` 判过），
/// 这条幂等只是为了「连点两次菜单」不出事。
pub(crate) unsafe fn create(a: &mut App) -> bool {
    if a.island.on() {
        return true;
    }
    // 只读一次配置（`&mut a.island` 与 `&a.cfg` 不能同时借）
    let (pref_open, cfg_w, cfg_h, screen) = {
        let c = a.cfg();
        (c.island_state, c.island_w, c.island_h, c.island_screen.clone())
    };
    let cls = wide(ISLAND_CLASS);
    let mut wc: WNDCLASSW = zeroed();
    wc.style = 0;
    wc.lpfnWndProc = Some(island_proc);
    wc.hInstance = module_handle();
    wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW_RES);
    // 与格言底板同一约定：不给背景刷，画面全由 ULW 提交
    wc.hbrBackground = null_mut();
    wc.lpszClassName = cls.as_ptr();
    RegisterClassW(&wc);

    // 先用系统 DPI 算一次（窗口还没有，问不到它的 DPI）
    a.island.dpi = crate::ffi::dpi_pct(crate::ffi::dpi_for_system().unwrap_or(96));
    a.island.compact_w = estimate_compact_w(&a.island.samples, &a.island.items, a.island.dpi);
    let k = a.island.dpi as f32 / 100.0;
    let rect = make_rect(
        a.island.dpi,
        &screen,
        if pref_open { 1.0 } else { 0.0 },
        a.island.compact_w,
        (cfg_w as f32 * k).round() as i32,
        (cfg_h as f32 * k).round() as i32,
    );

    let title = wide("DeskGeyanIsland");
    let h = CreateWindowExW(
        // LAYERED 是硬需求：ULW + 逐像素 alpha。TRANSPARENT 只有分层窗口上才有效
        // （铁律 9），这里两个都给，P0/P1 恒定穿透。
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TRANSPARENT,
        cls.as_ptr(),
        title.as_ptr(),
        WS_POPUP | WS_VISIBLE,
        rect.x,
        rect.y,
        rect.w,
        rect.h,
        null_mut(),
        null_mut(),
        module_handle(),
        null(),
    );
    if h.is_null() {
        return false;
    }
    a.island.hwnd = h;
    a.island.visible = true;
    // 窗口建好了，现在能问「它落在哪块屏」
    a.island.dpi = window_dpi(h);
    a.island.state = if pref_open {
        State::Expanded
    } else {
        State::Collapsed
    };
    a.island.progress = if pref_open { 1.0 } else { 0.0 };
    a.island.rot = geom::Rot::default();
    a.island.rot_k = 1.0;
    a.island.state_since = Instant::now();
    recompute(a);
    a.island.dirty = true;
    draw::render(a);
    a.island.tick_ms = geom::IDLE_TICK_MS;
    SetTimer(h, ISLAND_TIMER_ID, geom::IDLE_TICK_MS, None);
    true
}

/// 销毁窗口。**真销毁，不是 `SW_HIDE`** —— 关掉就该是零成本，
/// 而隐藏仍占一个窗口对象 + 一条 topmost 记录。
pub(crate) fn destroy(a: &mut App) {
    if !a.island.on() {
        return;
    }
    unsafe {
        KillTimer(a.island.hwnd, ISLAND_TIMER_ID);
        DestroyWindow(a.island.hwnd);
    }
    // 换成一块干净的：`Frame` / 字体随旧值 Drop，句柄不残留。
    // `consume_events` 要**带过去**：它是调用方（真运行 / 验证入口）的意图，
    // 一次「关掉再打开」不该把它悄悄改回 true —— 那会让自检在第二次建窗后
    // 开始搬用户的真实消息，而前半段还是干净的。
    let consume = a.island.consume_events;
    a.island = Island::new();
    a.island.consume_events = consume;
    a.island.items = Items::parse(&a.cfg().island_items);
}

/// 按 `cfg.island` 落地：开则建窗、关则销毁。**只读配置**。
///
/// 写 `cfg.island` 是 `on_menu` 的事 —— 与「单一写路径」的既定约定一致：
/// 改一个设置除了赋值还要 `save()`，两处各写一遍必然走偏。
pub(crate) fn apply_enabled(a: &mut App) {
    if a.cfg().island {
        if !a.island.on() {
            unsafe { create(a) };
        }
    } else {
        destroy(a);
    }
}

/// 重算几何并落到窗口（**只算，不写 `cfg`**）。
///
/// 落地靠 `draw::render` 里的 `UpdateLayeredWindow`：它把位置 + 尺寸 + 内容
/// 一次性提交，所以不存在「窗口已经是新尺寸、内容还是旧的」的中间态
/// （这正是 `apply_geometry` 那段注释踩过的坑）。
pub(crate) fn recompute(a: &mut App) {
    if !a.island.on() {
        return;
    }
    let (screen, cfg_w, cfg_h) = {
        let c = a.cfg();
        (c.island_screen.clone(), c.island_w, c.island_h)
    };
    let dpi = a.island.dpi;
    let k = dpi.max(48) as f32 / 100.0;
    let ew = ((cfg_w as f32) * k).round().max(80.0) as i32;
    let eh = ((cfg_h as f32) * k).round().max(40.0) as i32;
    let cw = if a.island.compact_w > 0 {
        a.island.compact_w
    } else {
        estimate_compact_w(&a.island.samples, &a.island.items, dpi)
    };
    a.island.rect = make_rect(dpi, &screen, a.island.geo_t(), cw, ew, eh);
}

/// 把岛提到 z 序最前。`raise_pair` 每次提格言之后都要调它 ——
/// TOPMOST 组内后设置者在上，不补这一步格言会把岛压下去。
pub(crate) fn raise(a: &App) {
    if !a.island.on() {
        return;
    }
    unsafe {
        SetWindowPos(
            a.island.hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );
    }
}

// ---------------------------------------------------------------- 每拍

fn cursor_inside(hwnd: HWND) -> bool {
    unsafe {
        let mut pt: POINT = zeroed();
        let mut rc: RECT = zeroed();
        if GetCursorPos(&mut pt) == 0 || GetWindowRect(hwnd, &mut rc) == 0 {
            return false;
        }
        // 用**真实窗口矩形**，不拿 `cfg` 顶替（家规）。tick 是轮询、没有消息
        // 自带坐标可用，与格言的 `hover_paused` 同一条豁免。
        pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom
    }
}

/// 岛的一拍。**不读 `cfg.visible` / `hover_paused` / `trans` / `eff_anim()`**。
pub(crate) fn tick(a: &mut App) {
    if !a.island.on() {
        return;
    }
    let hwnd = a.island.hwnd;
    let now = Instant::now();
    let dt = now.duration_since(a.island.last_tick).as_millis().min(250) as u32;
    a.island.last_tick = now;

    // ---- ① 全屏自动隐藏：只**读**格言的 `fs_hidden`，不改它
    if a.cfg().auto_hide_fs && a.fs_hidden() {
        if a.island.visible {
            unsafe { ShowWindow(hwnd, SW_HIDE) };
            a.island.visible = false;
        }
        return; // 隐藏期间零采样、零出帧
    }
    if !a.island.visible {
        unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        a.island.visible = true;
        a.island.dirty = true;
    }

    // ---- ② 悬停判定（轮询真实矩形；不对称迟滞在纯函数里）
    let (hover_in, leave) = {
        let c = a.cfg();
        (
            c.island_hover_ms.clamp(20, 5000) as u32,
            c.island_leave_ms.clamp(20, 10000) as u32,
        )
    };
    let inside = cursor_inside(hwnd);
    let (want, hs) = geom::hover_decide(
        a.island.want,
        a.island.hover,
        inside,
        dt,
        hover_in,
        leave,
    );
    a.island.want = want;
    a.island.hover = hs;

    // ---- ③ 状态推进
    // 只读一次配置：`island_items` 每拍解析是为了让「手改 config.ini + 热重载」
    // 也能立刻生效，成本是 split 六个短串（几十纳秒），不值得加同步点。
    let items = Items::parse(&a.cfg().island_items);
    if items != a.island.items {
        a.island.items = items.clone();
        a.island.dirty = true;
        a.island.compact_w = 0; // 让下一拍按新内容重新度量
    }

    let min_visible = (geom::MIN_VISIBLE_MS as f32 * (a.island.dpi.max(48) as f32 / 100.0))
        .round()
        .max(80.0) as u128;
    match a.island.state {
        State::Collapsed => {
            if want {
                a.island.state = State::Expanding;
                a.island.progress = 0.0;
                a.island.state_since = now;
            }
        }
        State::Expanded => {
            if !want && a.island.state_since.elapsed().as_millis() >= min_visible {
                a.island.state = State::Collapsing;
                a.island.progress = 0.0;
                a.island.state_since = now;
            }
        }
        State::Expanding => {
            a.island.progress += dt as f32 / geom::EXPAND_MS as f32;
            if a.island.progress >= 1.0 {
                a.island.progress = 1.0;
                a.island.state = State::Expanded;
                a.island.state_since = now;
                a.island.dirty = true;
            }
        }
        State::Collapsing => {
            a.island.progress += dt as f32 / geom::COLLAPSE_MS as f32;
            if a.island.progress >= 1.0 {
                a.island.progress = 0.0;
                a.island.state = State::Collapsed;
                a.island.state_since = now;
                a.island.dirty = true;
            }
        }
    }

    // ---- ③b 收起态轮播。只在**收起端点**推进：展开时用户在看面板，
    //          切页既无意义，也会让收起后的第一秒就撞上切页。
    let rotate_s = a.cfg().island_rotate.clamp(0, 600) as u32;
    if a.island.state == State::Collapsed {
        let was = a.island.rot;
        let was_k = a.island.rot_k;
        let (rot, rk) = geom::rot_step(was, dt, rotate_s, a.island.pages);
        a.island.rot = rot;
        a.island.rot_k = rk;
        // **只在真的变了的时候置脏**：Hold 相位里 `t_ms` 每拍都在涨，
        // 拿它当判据会让「静止」也每拍出一帧（那就废了「空转不出帧」）。
        if rot.phase != was.phase || rot.page != was.page || (rk - was_k).abs() > 1e-6 {
            a.island.dirty = true;
        }
    } else {
        a.island.rot = geom::Rot::default();
        a.island.rot_k = 1.0;
    }

    // ---- ④ 采样（1Hz 节流在 `Samples::update` 内部）
    let stamps_before = (
        a.island.samples.clock.clone(),
        a.island.samples.cpu,
        a.island.samples.mem,
        a.island.samples.battery,
        a.island.samples.unread,
        a.island.samples.msg.clone(),
    );
    let now_time = crate::util::local_time();
    let opts = {
        let c = a.cfg();
        content::Opts {
            seconds: c.seconds,
            unread: c.island_unread,
            // 真运行才消费磁盘：导出/自检会把 `Island::consume_events` 置 false
            consume: a.island.consume_events,
            keep: c.island_events_keep,
        }
    };
    a.island.samples.update(&items, opts, &now_time);
    let samples_changed = stamps_before.0 != a.island.samples.clock
        || stamps_before.1 != a.island.samples.cpu
        || stamps_before.2 != a.island.samples.mem
        || stamps_before.3 != a.island.samples.battery
        || stamps_before.4 != a.island.samples.unread
        || stamps_before.5 != a.island.samples.msg;
    if samples_changed {
        a.island.dirty = true;
    }

    // ---- ⑤ 几何（端点尺寸由 DPI 决定；`compact_w` 用上一帧回写的真实值）
    recompute(a);

    // ---- ⑥ 出帧
    if a.island.needs_frame() {
        a.island.dirty = false;
        draw::render(a);
    }

    // ---- ⑦ 定时器节奏：动画 16ms、静止 40ms。**只改岛自己的定时器**，
    //         格言的跑马灯不会被悄悄拉到 60fps。
    let want_ms = if a.island.state.morphing() || a.island.rot.fading() {
        geom::MORPH_TICK_MS
    } else {
        geom::IDLE_TICK_MS
    };
    if want_ms != a.island.tick_ms {
        unsafe { SetTimer(hwnd, ISLAND_TIMER_ID, want_ms, None) };
        a.island.tick_ms = want_ms;
    }
}

/// 自检 / 导出用：把状态机直接推到某个端点（不经过 hover 与动画）。
pub(crate) fn force_state(a: &mut App, state: State) {
    a.island.state = state;
    a.island.progress = if state.is_expanded() { 1.0 } else { 0.0 };
    a.island.want = state.is_expanded();
    a.island.state_since = Instant::now();
    a.island.dirty = true;
    recompute(a);
}

/// 落下「收起」端点（菜单 / 设置面板用）。配置的 `island_state` 由调用方写。
pub(crate) fn collapse(a: &mut App) {
    force_state(a, State::Collapsed);
}

/// 落下「展开」端点。
pub(crate) fn expand(a: &mut App) {
    force_state(a, State::Expanded);
}
