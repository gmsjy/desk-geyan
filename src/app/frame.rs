//! 帧调度：什么时候该出一帧、动效怎么推进。
//!
//! 「空转不出帧」是硬约束：`needs_frame()` 为假时一帧都不出。
//! 静态画面 0 帧、跑马灯 25fps（`TIMER_MS = 40`）、悬停暂停 0 帧。

use super::*;

use std::mem::zeroed;
use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{HGDIOBJ, SelectObject};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect, ShowWindow};
use crate::transition;
use crate::config::{Anim, Config, Orient};
use crate::gfx;
use crate::quotes::Quote;
use crate::consts::*;
use crate::util::*;

// ---------------------------------------------------------------- 动效

/// 全屏检测的轮询间隔。`SHQueryUserNotificationState` 是跨进程的 shell 查询，
/// 有真实成本；而「进 / 出全屏」本身就是秒级事件，1Hz 完全够。
/// 定时器是 25Hz，也就是每 25 拍才查一次。
const FS_CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// `QUERY_USER_NOTIFICATION_STATE` 的可读名字（`--probe` 报当前状态用）。
pub(crate) fn quns_label(s: u32) -> &'static str {
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
pub(crate) enum FsAction {
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
pub(crate) fn fs_decide(fullscreen: bool, already_hidden: bool, user_visible: bool) -> FsAction {
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
pub(crate) fn temp_hide(a: &App, hide: bool) {
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

pub(crate) fn tick(a: &mut App) {
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
pub(crate) fn step(a: &mut App, dt: f32) {
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
pub(crate) fn clock_stamp(cfg: &Config) -> u64 {
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
pub(crate) fn needs_frame(a: &App) -> bool {
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

pub(crate) fn measure_quote(a: &App, q: &Quote) -> i32 {
    let s = wide(&q.display());
    // 借用 frame 只是拿它的 DC 量一下，不改内容
    unsafe { SelectObject(a.frame.dc(), a.fonts.quote as HGDIOBJ) };
    gfx::measure_text(a.frame.dc(), &s).0
}

/// 跑马灯滚动带的有效宽度（消掉左右内边距）。
/// `step` 的轮转判定和 `rebuild_fonts` 的落点是同一把尺子，抽出来防止两处走偏。
pub(crate) fn marquee_band_w(cfg: &Config) -> f32 {
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
