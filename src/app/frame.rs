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
    // 查询失败不等于「退出全屏」。若把 None 当成 false，窗口正因全屏藏着时
    // 一次瞬态失败就会把它弹回来盖住游戏。保持上一次状态，等下一次轮询。
    let Some(st) = crate::ffi::user_notification_state() else {
        return;
    };
    let fullscreen = st == QUNS_RUNNING_D3D_FULL_SCREEN;
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
    // 隐藏时不再推进动效，也不做悬停查询 / 出帧。以前这里只跳过了「窗口是否可见」
    // 的绘制判断，跑马灯和呼吸仍会以 25fps 在后台生成整帧，纯属浪费。
    // 恢复可见由 `set_visible` / `temp_hide` 主动补一帧。
    if !a.cfg.visible || a.fs_hidden {
        return;
    }
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
        // 尺寸的欠账也在这里还：缩放时文字层**不**在 `apply_geometry` 里改尺寸
        // （那会让「窗口已变大、内容还是旧的」露一帧空白），改成下一帧
        // `present` 一次性提交。用户按住不动时没有鼠标消息推动那一帧，
        // 所以这一拍必须补一次 —— `dirty` 没置位时它是个空调用。
        render_if_due(a, false);
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
    let n = a.list.len();
    if n == 0 {
        return;
    }
    // 用户暂停：只让时钟继续走。暂停命令里会落定正在飞的过渡，
    // 所以这里不需要再处理 Leave/Gap/Arrive。
    if a.cfg.paused {
        if clock_stamp(&a.cfg) != a.clock_stamp {
            mark_dirty(a);
        }
        render_if_due(a, false);
        return;
    }

    // **生效动效**读 `App::eff_anim()`：竖版降级 + 「放不下转跑马灯」都在里面。
    // 以前这段判定在四个地方各写一份，`quote_mod` 那份漏了 force_marquee。
    let anim = a.eff_anim();
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
                    // 间隙与绘制端**同一个出口**（`Config::marquee_gap`）：
                    // 以前这里按 f32 算、绘制端按 round 算，最多差 0.5px，
                    // 每轮累积——表现为「偶尔跳一下字」。
                    let gap = a.cfg.marquee_gap() as f32;
                    a.m -= a.cur_w as f32 + gap;
                    a.qi = a.qn;
                    a.qn = (a.qn + 1) % n;
                    a.cur_w = measure_quote(a, &a.list[a.qi]);
                }
            }
        }
        // 静态 / 淡入淡出 / 呼吸：**三者共用同一套 dwell → 换句流程**。
        //
        // 呼吸以前的 `step` 只有一句 `a.phase += dt`：没有 dwell 计时、不调
        // `advance`，于是 455 条格言库里只有第一条会被看到（实测 14 秒内
        // 画面纹丝不动，而同一句在静态模式下早该换 3 次）。「呼吸」是
        // **亮度**的动效，不该顺手把「换句」关掉。
        Anim::Static | Anim::Fade | Anim::Breathe => {
            // 呼吸相位独立推进：悬停暂停、过渡期间都照走（它是氛围，不是内容）
            if anim == Anim::Breathe {
                a.pulse += dt;
            }
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
                        // Fade / Breathe 是纯淡入淡出（无位移）；Static 横版滑入 0.5em。
                        // **竖版恒 0**：竖排的阅读流向是自上而下、列自右向左，
                        // 横向滑入与字格方向不正交，会把列间节律打乱（PR1.1
                        // 修复实施偏差：原先只看 anim 不看 orient）。
                        let slide = if matches!(anim, Anim::Fade | Anim::Breathe)
                            || a.cfg.orient == Orient::Vertical
                        {
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
    clock_stamp_parts(
        t.wYear,
        t.wMonth,
        t.wDay,
        t.wHour,
        t.wMinute,
        t.wSecond,
        cfg,
    )
}

/// `clock_stamp` 的纯函数核心，便于用固定时刻验证「日期变化也必须触发重绘」。
fn clock_stamp_parts(
    year: u16,
    month: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    cfg: &Config,
) -> u64 {
    if !cfg.show_clock && !cfg.show_date {
        return 0;
    }
    let date = ((year as u64) << 32) | ((month as u64) << 24) | ((day as u64) << 16);
    if cfg.show_clock && cfg.seconds {
        date | ((hour as u64) << 12) | ((minute as u64) << 6) | second as u64
    } else if cfg.show_clock {
        date | ((hour as u64) << 6) | minute as u64
    } else {
        // 只显示日期时不能带时分秒，否则静态画面会每秒白重绘一次；
        // 日月年已经在 date 里，午夜到来时仍会变化。
        date
    }
}

/// 这一拍画面会不会自己变。不会变就别出帧 —— 空转的那一帧既费 CPU，
/// 又会在拖动时和鼠标消息抢时间片。
pub(crate) fn needs_frame(a: &App) -> bool {
    if clock_stamp(&a.cfg) != a.clock_stamp {
        return true;
    }
    // 暂停轮播：画面上只剩时钟还会自己走；跑马灯/呼吸/过渡全部冻结。
    if a.cfg.paused {
        return false;
    }
    if !a.cfg.visible || a.fs_hidden {
        return false;
    }
    // 过渡进行中：相位时钟在走，每帧都要出（560ms 的窗口期，可接受）
    if a.trans.is_active() {
        return true;
    }
    // 与 `step` 读**同一个**方法：生效动效 + 「放不下转跑马灯」。
    // 两处要是各写各的，就会出现「跑马灯在滚但不出帧」或「静态却每帧重绘」。
    let anim = a.eff_anim();
    match anim {
        // 跑马灯只影响格言；格言关掉了就只剩时钟，和静态没区别。
        // **悬停暂停期间一帧不出**（PR2）：m 冻结了画面不变，再逐帧重绘
        // 就是暂停版本的空转烧 CPU —— 需要帧的判定必须和 step 的冻结同步。
        Anim::Marquee => a.cfg.show_quote && !a.hover_paused,
        // 呼吸动的是格言通道的 k（时钟/日期/底板不动），必须逐帧出。
        // **减动模式下呼吸已降级为静态**（quote_mod 最早返回 1.0），
        // 再逐帧空转就是纯烧 CPU（PR1.1）。
        // **格言关掉时也必须停**：只剩时钟的画面逐位不变，而 25fps 空转
        // 实测 5.47% CPU（静态同条件是 0.98%）—— 这是「空转不出帧」铁律
        // 在呼吸这条路上唯一的缺口，之前漏了 `show_quote`。
        // 呼吸**不冻结**于悬停（PR2，12.3#2）：呼吸是氛围不是阅读内容，
        // 冻了反而像坏了。
        Anim::Breathe => a.cfg.show_quote && !a.trans.reduced(),
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
    // 内边距必须走 `cfg.pad()` —— 以前这里写的是 `(24.0*scale).round()`，
    // 漏了版心那侧的 `.max(10)`：scale < 42% 时带比版心宽 3px，起始位整体右偏。
    (cfg.w - 2 * cfg.pad()).max(80) as f32
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
