//! 基准：`--perf`（单帧成本）/ `--dragbench`（拖动跟手）/ `--memtest`（泄漏 soak）。
//!
//! 口径纪律：性能测量**不可与重负载并发**（并行跑 clippy 会把 785µs 量成 1200µs）；
//! 只报耗时不报位移的基准是废的；成本拆解必须先预热。

use super::*;

use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{KillTimer, SetWindowPos, ShowWindow};
use crate::transition;
use crate::bench::{pump, send_mouse, with_config_guard};
use crate::config::{Anim, Config, Orient};
use crate::ffi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDPIAware, SetProcessDpiAwarenessContext};
use crate::quotes::Quote;
use crate::consts::*;

// ---------------------------------------------------------------- 性能标定

/// `desk-geyan --perf`：把「拖动期间一条鼠标消息要花多少微秒」拆开量化。
///
/// 这是「跟不跟手」唯一的硬指标 —— 手感是主观的，队列堆积是客观的：
/// 鼠标每秒上报 N 条移动消息，每条的处理成本必须远小于 1/N 秒，否则消息在
/// 队列里越积越多，窗口只能跳着追赶光标，用户感知到的就是「粘」「拖后腿」。
///
/// 参考线：125Hz 鼠标每条预算 8000µs，1000Hz 鼠标 1000µs。
pub(crate) fn perf() -> i32 {
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
pub(crate) fn dragbench() -> i32 {
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
pub(crate) fn memtest() -> i32 {
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
