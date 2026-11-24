//! `--selftest`：84 条自检，覆盖排版 / 绘制 / 动效 / 交互 / 配置。
//!
//! 设计前提是「起点是常量」：`a.cfg` 钉成默认值，**格言库也必须钉死** ——
//! 凡是被测结果依赖的用户态数据，都要一起钉住，否则断言会随机器而变。

use super::*;

use crate::{autostart, clipboard, favorites, transition};
use crate::bench::with_config_guard;
use crate::config::{self, Anim, Config, Orient, VStyle};
use crate::config_ui;
use crate::gfx::{self, Fonts};
use crate::layout;
use crate::quotes::{self, Quote};
use crate::util::*;

// ---------------------------------------------------------------- 自检

/// 打印断言表，返回是否全绿。
///
/// 抽出来是为了**两处**都能用：正常收尾，以及前置条件不满足时的早退
/// （见 `selftest` 里的「内置库文件」）。
fn print_rows(rows: &[(&str, bool, String)]) -> bool {
    let mut pass = true;
    for (name, ok, detail) in rows {
        if !ok {
            pass = false;
        }
        println!("[{}] {:<32} {}", if *ok { "PASS" } else { "FAIL" }, name, detail);
    }
    pass
}

/// `desk-geyan --selftest`：不建窗口，直接把一帧走完整条渲染管线，
/// 然后按**预乘 ARGB 的数学约束**逐个像素验算。
///
/// 这些断言不是装饰 —— 它们检查的正是「文字能不能真的贴上去」：
///   * 颜色分量不能超过 alpha（预乘的硬性要求，违反 DWM 会渲染错）
///   * 必须存在大量 0<a<255 的半透明像素（证明是逐像素 alpha，
///     而不是 LWA_COLORKEY 那种非 0 即 255 的硬抠图）
///   * 素材区域必须有足够的不透明像素（证明字真的画出来了，不是空帧）
pub(crate) fn selftest() -> i32 {
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
    a.fonts = Fonts::build(&a.cfg.font, a.cfg.eff_scale_pct(), a.cfg.clock_scale, a.cfg.quote_scale);
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

    // ---- 环境前提：内置库现在是**与 exe 同目录的文件**（`quotes-builtin.txt`），
    // 不再编进二进制。这条断言守的是「装到用户手里时文件齐不齐」—— 缺件时程序
    // 只剩一条提示语，下面那些读 `list[0]` 的断言（字号判据等）全部失去意义，
    // 所以它必须排在第一个报出来。
    // 下界取 100：内置库是「一部格言集」，退化成个位数只可能是文件被截断 /
    // 换成了别的东西。上界不设 —— 条数是会长大的数据，不是契约。
    {
        let path = quotes::builtin_file();
        let n = quotes::load_builtin().len();
        let ok = path.is_some() && n >= 100;
        rows.push((
            "内置库文件（与 exe 同目录）",
            ok,
            match &path {
                Some(p) => format!("{} 条 · {}", n, p.display()),
                None => format!(
                    "未找到 {}（在 exe 目录及其上溯各级里查找）",
                    quotes::BUILTIN_FILE
                ),
            },
        ));
        if !ok {
            // **早退**：库缺件时，后面靠「换句 / 过渡 / 竖排列数」的断言全部
            // 失去对象，跑下去只会刷出 8 条互相传染的 FAIL（实测 56 PASS /
            // 9 FAIL），把「少拷了一个文件」这个真因淹掉。一个前置条件不满足，
            // 就只报这一条。
            print_rows(&rows);
            println!(
                "\n存在失败项  自检中止：内置格言库不可用（{}）—— 补齐后重跑",
                match &path {
                    Some(p) => format!("{} 只解出 {} 条", p.display(), n),
                    None => format!("{} 未找到", quotes::BUILTIN_FILE),
                }
            );
            return 3;
        }
    }

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
        // 呼吸也必须换句：旧实现这条分支只有一句 `phase += dt`——没有 dwell
        // 计时、不调 `advance`，455 条格言库里只有第一条会被看到。
        // 18 秒动画时间足够跨过最短 dwell（4s）好几轮。
        if an == Anim::Breathe && rotations == 0 {
            ok = false;
            why = "18 秒内一次都没换句（呼吸不该冻结轮播）".into();
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

    // ---- 呼吸模式的两条硬约束（回归：两个都是实测出来的缺陷）
    {
        let saved = a.cfg.clone();

        // ① 格言关掉时**一帧都不该出**：只剩时钟的画面逐位不变，
        //    而呼吸每拍都标脏 ⇒ 25fps 空转，实测 5.47% CPU（静态同条件 0.98%）。
        a.cfg.anim = Anim::Breathe;
        a.cfg.seconds = false;
        a.cfg.show_quote = false;
        a.trans.settle();
        a.pulse = 1.0;
        a.clock_stamp = clock_stamp(&a.cfg); // 对齐时钟刻度，隔离出「动效」这一个变量
        let breathe_frames = needs_frame(&a);

        // ② 「呼吸 + 放不下转跑马灯」：整条跑马灯**不许**被呼吸系数压暗。
        //    `quote_mod` 以前读 `cfg.eff_anim()`（不含 force_marquee），
        //    于是跑马灯被乘上一个冻结的 k（实测与纯跑马灯差 1327 px / 最大 35 级）。
        a.cfg.w = 240;
        a.cfg.h = 100;
        a.cfg.quote_scale = 300;
        a.cfg.show_clock = false;
        a.cfg.show_date = false;
        a.cfg.show_quote = true;
        a.rebuild_fonts();
        let op = a.text_alpha();
        build_frame(&mut a, op);
        let forced = a.force_marquee;
        let k = a.eff_quote_k();

        a.cfg = saved;
        a.rebuild_fonts();
        a.trans.settle();
        rows.push((
            "呼吸·关格言不出帧 / 强制跑马灯不压暗",
            !breathe_frames && forced && (k - 1.0).abs() < 1e-6,
            format!(
                "关格言后仍出帧={}（须 false）· 强制跑马灯={} · 跑马灯下 k={:.2}（须 1.00）",
                breathe_frames, forced, k
            ),
        ));
    }

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
            b.fonts = Fonts::build(&b.cfg.font, b.cfg.eff_scale_pct(), b.cfg.clock_scale, b.cfg.quote_scale);
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
                b.pulse = 0.0;
                if an == Anim::Breathe {
                    b.pulse = 1.3; // 呼吸相位推进过
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
                b.pulse = 0.0;
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
            b.pulse = 0.7;
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
        // 对照组：两句库，同样 15s，必须至少启过一次过渡。
        // 库不足两句（内置库缺件时只剩一条兜底提示语）时**合成**第二句 ——
        // 这一组的被测对象是过渡状态机、不是格言库本身，不能因为库小就把对照组
        // 抽掉；缺件该由「内置库文件（与 exe 同目录）」那条断言单独报红。
        a.list = if saved.len() >= 2 {
            vec![saved[0].clone(), saved[1].clone()]
        } else {
            vec![
                saved[0].clone(),
                Quote {
                    text: "对照组第二句".into(),
                    author: String::new(),
                },
            ]
        };
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
            // **排除含公式的句子**：以下几组断言量的是「纯文字竖版」的
            // 列距 / 瓦片 / 错峰规律，公式句走的是 `layout_rich`，
            // 列的语义不同（公式整块占位）。公式句自己的守卫在 `--mathquote`。
            // 不排除的话，谁往词库里加一条长公式，这里就会跟着红 ——
            // 红的原因还跟被验的东西毫无关系。
            .filter(|&i| !a.list[i].author.is_empty() && !a.list[i].has_math())
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
        // 首列右缘贴版心右缘。列瓦片始终按**版心**切分，居中只挪字形在瓦片内的
        // 位置（`Align::Center` 的 inset），所以这里仍应精确等于 `band.2`。
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

        // v1 / v2 是**两次独立 build**，而每次 build 都重新读墙钟
        //（`render.rs` 里 `fmt_clock(&local_time())`）。两次之间跨过秒进位，
        // 下面「时钟通道逐位相同」就会把**秒位数字换了**记成「动效动了时钟」
        // —— 实测 1/10 概率假红，而本条要验的恰恰是「动效不碰时钟」。
        //
        // 采信前先比 `clock_stamp`（秒粒度）：变了就整套重来。过渡推进是
        // 纯逻辑、不吃墙钟，重来零副作用；连跨 5 次才认输（正常一次就过）。
        let (mut v1, mut v2, mut t_arrive): (Vec<u32>, Vec<u32>, f32);
        let mut tries = 0u32;
        loop {
            tries += 1;
            let stamp = clock_stamp(&a.cfg);
            a.trans.settle();
            a.phase = 0.0;
            let op = a.text_alpha();
            build_frame(&mut a, op);
            v1 = a.frame.out_pixels().to_vec();

            // 驱到 Arrive 35%：右列已过半、左列刚起步，错峰最可辨的时刻。
            // **一拍到位**：`step(LEAVE+GAP+ε)` 已经把 Leave/Gap 穿完、落在
            // Arrive 起点，再补一拍 GAP 就会多走 0.14s（t=0.231 时两列 k 都
            // 归 1，错峰不可辨 —— 这里曾经踩过）。
            a.trans.start_leave(0);
            a.trans.step(transition::LEAVE_S + transition::GAP_S + 1e-4);
            a.trans.step(transition::ARRIVE_S * 0.35);
            t_arrive = a.trans.phase_t();
            build_frame(&mut a, op);
            v2 = a.frame.out_pixels().to_vec();
            if clock_stamp(&a.cfg) == stamp || tries >= 5 {
                break;
            }
        }
        let m0 = a.frame.channel_mask(0);
        let m1 = a.frame.channel_mask(1);
        let m2 = a.frame.channel_mask(2);
        // 落款取样坐标必须用 **hug 之后**的落款条 —— build_frame 里落款
        // 会右移贴主文，拿 layout_sign 的原位取样就是 0 px（真管线和
        // 自检走同一个 hug_sign，不许各推一遍）。
        let cb0 = layout_clock(&mut a);
        // `layout_clock` 原样（`place_sign` 之前）有没有造出署名 —— 下面用它
        // 区分「按设计让位」和「压根没生成」。`cb0` 等下要 move 进 `place_sign`。
        let had_sig = !cb0.sig.is_empty();
        let ql0 = frame_layout(&a, &cb0);
        let cb = place_sign(&a.cfg, cb0, &ql0);
        let ql = frame_layout(&a, &cb);
        let ncol = ql.col_ranges.len();

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
                "{} 列比值 [{}] · 时钟 {} px 逐位相同（异常 {}）· 采样 {} 轮",
                ncol,
                ratio_s.join(", "),
                clock_n,
                clock_bad,
                tries
            ),
        ));

        // ⑦ 出处随格言：署名区 alpha ≈ 最左列 k；页头日期逐位不动。
        //
        //    竖排版式的日期从「落款块里的一列」搬到了**页头横排**，于是
        //    date 通道上现在只有两块墨：页头日期 + 署名。两块都按矩形圈定
        //    （不能只按 x 分 —— B 极简的日期偏上、署名居中偏下，x 区间是重叠的），
        //    落在署名矩形里的验「跟着淡」，落在别处的验「纹丝不动」。
        //
        //    **署名可能合法地不存在**：这句是「带出处且最长」的 36 字，竖版
        //    按列自适应会把它撑满整块版心，署名没地方落脚就按设计让位
        //    （宁可不落名，也不叠在字上，见 `place_sign`）。所以页头日期那半
        //    必须无条件验 —— 曾经把整段统计写在 `if let Some(作者列)` 里，
        //    署名一让位、两个计数器就都是 0，红出来的信息还叫「日期 0 px」，
        //    完全指错了方向。
        let author_col = cb.sig.iter().find(|c| c.is_author);
        let (mut au_ok, mut au_n, mut dt_ok, mut dt_n) = (true, 0usize, true, 0usize);
        let want_author = transition::column_k(ncol - 1, ncol, t_arrive) as f64;
        let (mut ax0, mut ax1, mut ay0, mut ay1) = (0i32, 0i32, 0i32, 0i32);
        if let Some(ac) = author_col {
            ax0 = ac.x;
            ax1 = ac.x + cb.sig_w.max(1);
            ay0 = ac.y;
            ay1 = ac.y + ac.s.len() as i32 * cb.sig_cell.max(1); // 一个汉字一个 u16
        }
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
                if x >= ax0 && x < ax1 && y >= ay0 && y < ay1 {
                    au_n += 1;
                    let r = a2 as f64 / a1 as f64;
                    if (r - want_author).abs() > 0.08 {
                        au_ok = false;
                    }
                } else {
                    // 署名之外 = 页头日期（静帧）：换句过渡中必须逐位相同。
                    dt_n += 1;
                    if (a2 as i32 - a1 as i32).abs() > 1 {
                        dt_ok = false;
                    }
                }
            }
        }
        // 署名在 = 两条都得成立；署名让位 = 日期那条仍得成立，但不许是
        // 「压根没造出来」（那是 `layout_header_v` 的事，不是让位）。
        let sig_ok = match author_col {
            Some(_) => au_ok && au_n > 20,
            None => had_sig,
        };
        rows.push((
            "PR3·出处随格言、页头日期不动",
            sig_ok && dt_ok && dt_n > 20,
            if author_col.is_some() {
                format!(
                    "出处 {} px 比值应≈{:.3} · 页头日期 {} px 逐位相同 · 出处动={} 日期动={}",
                    au_n, want_author, dt_n, !au_ok, !dt_ok
                )
            } else {
                format!(
                    "页头日期 {} px 逐位相同（日期动={}）· 署名按设计让位（版面占满）· 正文 {} 列",
                    dt_n, !dt_ok, ncol
                )
            },
        ));

        // ⑧ 竖版首列完整可见（孤儿项：右缘墨迹被裁剪区切掉的回归守卫）。
        //
        //    旧判据是「版心右缘外还有 1px 外溢墨」—— 它成立靠的是一个巧合：
        //    竖版正文**右起**，首列字紧贴版心右缘，字形墨迹天然越过步进框
        //    1~2px。现在 A/B 两形态改成按墨迹居中（`Align::Center`），首列
        //    离版心右缘还有小半列，外溢窗口里永远是 0 px，那条判据就只会
        //    恒红、再也不指示任何东西。
        //
        //    改成量的**几何**：首列每个字形连同字宽都必须落在版心内、
        //    墨迹右缘不越出版心、且版心两边外侧不许有格言墨。版心外的墨
        //    只可能来自两处 —— 排版越界，或有人又给静态内容加了一刀裁剪
        //    （`with_clip` 会把越界的那部分削平，留下正好停在版心边上的
        //    整齐断口）。两条路都在这三条判据里现形。
        let (bl, br) = (band.0, band.2);
        let mut out_ink = 0usize;
        let mut max_ink_x: i32 = -1;
        for y in 0..fh {
            for x in 0..fw {
                if m2[(y * fw + x) as usize] == 0 {
                    continue;
                }
                if x > max_ink_x {
                    max_ink_x = x;
                }
                if x >= br || x < bl {
                    out_ink += 1;
                }
            }
        }
        let mut col_in = true;
        if let Some(&(g0, g1)) = ql.col_ranges.first() {
            for g in &ql.glyphs[g0..g1] {
                // 字形原点在版心内（墨迹越界由上面的 `out_ink` 兜）。
                if g.x < bl || g.x >= br {
                    col_in = false;
                }
            }
        } else {
            col_in = false; // 没有首列 = 环境不对，不许假绿
        }
        // 版心不许虚胖：正文块得**贴着版心中线**。A/B 走居中摆位，
        // 左右留白本来就不相等（列内居中的 inset 偏在一侧），所以容差给一列；
        // 真把版心算错（比如忘了减 pad）时，一侧留白会整块塌掉。
        let (mut min_x, mut max_x) = (i32::MAX, i32::MIN);
        for g in &ql.glyphs {
            min_x = min_x.min(g.x);
            max_x = max_x.max(g.x);
        }
        let left_gap = min_x - bl;
        let right_gap = br - max_x;
        let pitch = layout::Metrics::new(a.cfg.quote_font_px(), a.cfg.quote_font_px()).col_pitch;
        let snug = max_x >= min_x && (left_gap - right_gap).abs() <= (pitch * 2).max(8);
        rows.push((
            "PR3·竖版首列完整可见（版心外无墨 · 无静态裁剪）",
            col_in && out_ink == 0 && snug,
            format!(
                "首列字全在版心内={} · 版心外墨 {} px · 最右墨迹 x={}（版心右缘 {}）· 左右留白 {} / {}",
                col_in, out_ink, max_ink_x, br, left_gap, right_gap
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

        // ⑤ 暂停轮播：语义是「格言不再换、动效不再走，**时钟照常走**」。
        //    两个方向都能写错：做成整体冻结，时钟就停死在某一秒（用户看到
        //    一个坏掉的钟）；做成只挡换句，跑马灯照样滚、照样逐帧烧 CPU。
        {
            let paused_was = a.cfg.paused;
            a.cfg.paused = true;
            a.cfg.anim = Anim::Marquee;
            a.primed = true;
            a.hover_paused = false;
            a.trans.settle();
            a.dirty = true;
            render_if_due(&mut a, true); // 清脏：环境切换不该算进「不出帧」
            let (qi0, m0, r0) = (a.qi, a.m, a.renders);
            let mut nf = 0usize;
            for _ in 0..120 {
                step(&mut a, 0.1);
                if needs_frame(&a) {
                    nf += 1;
                }
            }
            rows.push((
                "暂停轮播：不换句 / 跑马灯冻结 / 一帧不出",
                a.qi == qi0 && a.m == m0 && a.renders == r0 && nf == 0 && !a.trans.is_active(),
                format!(
                    "12s 后 qi 漂移={} · m 增量={} · renders 增量={} · needs_frame 真 {} 拍",
                    a.qi != qi0,
                    a.m - m0,
                    a.renders.wrapping_sub(r0),
                    nf
                ),
            ));

            // 暂停**不**冻时钟：打开时间显示，把缓存刻度拨到与当前不一致，
            // 必须立刻要帧（否则用户会发现秒数停着不动）。
            a.cfg.show_clock = true;
            a.clock_stamp = clock_stamp(&a.cfg).wrapping_add(1);
            let tick_ok = needs_frame(&a);
            a.cfg.show_clock = clk_was;
            a.clock_stamp = clock_stamp(&a.cfg);
            rows.push((
                "暂停轮播不冻时钟（刻度变即出帧）",
                tick_ok,
                format!("刻度不一致时 needs_frame={}", tick_ok),
            ));
            a.cfg.paused = paused_was;
        }

        // 还原环境（后续断言 / 默认时钟显示要它）
        a.cfg.show_clock = clk_was;
        a.cfg.show_date = date_was;
        a.hover_paused = false;
        a.hover = transition::HoverState::default();
    }

    // ---- 「仅播放收藏夹」的两种边界。空收藏夹是危险输入：返回空库会让
    // 下一拍 `next()` 撞上 `len()==0`（除零 / 越界 / 黑屏三选一）。
    {
        let base = std::env::temp_dir().join(format!("desk-geyan-favonly-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::create_dir_all(base.join("quotes"));

        // ① 空收藏夹 → 回退内置库，绝不是空
        let empty = quotes::library_for(&base, true);
        // ② 有收藏 → 只取收藏，且与内置库无关
        std::fs::write(base.join("quotes").join(favorites::FILE), "甲一 —— A\n甲二 —— B\n").unwrap();
        let only = quotes::library_for(&base, true);
        // ③ 关掉开关 → 回到完整库（内置 + 外部）
        std::fs::write(base.join("quotes/01_g.txt"), "乙一 —— C\n").unwrap();
        let full = quotes::library_for(&base, false);

        let ok = empty.len() == quotes::builtin_len()
            && !empty.is_empty()
            && only.len() == 2
            && full.len() > only.len();
        rows.push((
            "仅播放收藏夹（空则回退内置 · 开关可逆）",
            ok,
            format!(
                "空收藏夹 → {} 条（内置 {}）· 有收藏 → {} 条 · 关闭开关 → {} 条",
                empty.len(),
                quotes::builtin_len(),
                only.len(),
                full.len()
            ),
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- 词库编码：中文 Windows 的记事本「另存为」默认是 ANSI(GBK)。
    // `read_to_string` 读这种文件会直接失败，整个文件被静默跳过 ——
    // 用户照着文档写了一整份词库，看到的是「一条都没生效」。
    // 这里用**真实 GBK 字节**走一遍，验回退真的解得对。
    {
        let base = std::env::temp_dir().join(format!("desk-geyan-gbk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::create_dir_all(&base);

        // 「国之器 —— 老子」的代码页 936 字节
        let gbk: &[u8] = b"\xB9\xFA\xD6\xAE\xC6\xF7 \xA1\xAA\xA1\xAA \xC0\xCF\xD7\xD3\n";
        std::fs::write(base.join("quotes.txt"), gbk).unwrap();
        // 同一句的 UTF-8 版，用来确认标准路径没被改坏
        std::fs::write(base.join("utf8.txt"), "国之器 —— 老子\n").unwrap();

        let got_gbk = quotes::read_external(&base.join("quotes.txt"));
        let got_utf8 = quotes::read_external(&base.join("utf8.txt"));
        let acp = unsafe { crate::ffi::GetACP() };

        let gbk_text = got_gbk
            .as_ref()
            .ok()
            .and_then(|l| l.first())
            .map(|q| q.text.clone())
            .unwrap_or_default();
        let utf8_text = got_utf8
            .as_ref()
            .ok()
            .and_then(|l| l.first())
            .map(|q| q.text.clone())
            .unwrap_or_default();

        // 非 936 的系统上，这批字节按当地代码页解出来当然不是汉字 ——
        // 那时只验「UTF-8 正常」和「没崩」，不误判成失败。
        let ok = if acp == 936 {
            gbk_text.starts_with("国之器") && utf8_text.starts_with("国之器")
        } else {
            !utf8_text.is_empty()
        };
        rows.push((
            "词库编码兼容（UTF-8 · 系统 ANSI/GBK）",
            ok,
            format!(
                "代码页 {} · GBK →「{}」· UTF-8 →「{}」{}",
                acp,
                if gbk_text.is_empty() {
                    "读取失败".to_string()
                } else {
                    gbk_text
                },
                if utf8_text.is_empty() {
                    "读取失败".to_string()
                } else {
                    utf8_text
                },
                if acp == 936 {
                    ""
                } else {
                    "（非中文代码页，只验 UTF-8）"
                }
            ),
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- 竖版逐列渐变的**容量边界**。
    //
    // 排版层的列数上限由窗口宽度决定（`avail_w / col_pitch`），而渲染层的带表
    // 是定长数组 —— 两者天生不对齐。容量原先写的是 16（按「竖版 4~12 列」估的），
    // 而 4000×80 这种极端比例能把列数顶到几十：超出的列在过渡里查不到带、
    // 系数退回 255（满亮），右半边会「不淡入地闪一下」。
    //
    // 这条断言做三件事：① 极端版面真的排一遍，量出列数；② 列数超上限时
    // **截断并给省略号**（跟「窗口装不下」完全一样的收尾，不是静默丢列）；
    // ③ 带表在超出容量时明确返回 false（而不是悄悄覆盖或越界）。
    {
        // 极端：最大窗宽 × 最小窗高 × 超长句。每列只装得下 3 个字，
        // 600 字 → 200 列，必然撞上排版上限。
        // 宽度必须留够：`max_cols = min(avail_w / col_pitch, MAX_COLS)`，
        // 列距按 1.7 倍字宽算，4000px 只能排 117 列 —— 触不到 127 那道闸，
        // 断言就退化成「量了个数」而不是「撞上限时的收尾」。
        let text: Vec<char> = "国".repeat(600).chars().collect();
        let adv = vec![20; text.len()];
        let m = layout::Metrics::new(20, 20);
        let q = layout::layout(&text, &adv, m, Orient::Vertical, (0, 0, 6000, 80), 0);
        let cols = q.col_x.len();
        // 撞上限时必须走和「窗口装不下」一样的收尾：截断 + 末字省略号
        let capped = cols == layout::MAX_COLS && q.overflow && q.ellipsis;

        // 带表容量：装满 MAX_BANDS 条之后必须开始拒绝
        let mut t = gfx::BandTable::empty();
        let mut accepted = 0usize;
        for i in 0..(gfx::MAX_BANDS + 8) {
            let x0 = i as i32 * 10;
            if t.push(x0, x0 + 10, 100, 100) {
                accepted += 1;
            }
        }

        rows.push((
            "竖版列数上限（超限截断给省略号 · 带表装得下）",
            capped && accepted == gfx::MAX_BANDS,
            format!(
                "600 字 → {} 列（上限 {} + 出处 1 = 带容量 {}）· 省略号={} · 带表接受 {}/{} 条后拒绝",
                cols,
                layout::MAX_COLS,
                gfx::MAX_BANDS,
                q.ellipsis,
                accepted,
                gfx::MAX_BANDS + 8
            ),
        ));
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
        let expected: &[u32] = &[
            MI_CLOCK,
            MI_DATE,
            MI_SECONDS,
            MI_QUOTE,
            // 桌面适配（2026-11-24）：塞在「显示内容」行尾，不增面板高度
            MI_DESK_FIT,
            MI_TOPMOST,
            MI_AUTOHIDE,
            MI_AUTOSTART,
            MI_LAYOUT,
            MI_PAUSE,
            MI_FAV_ONLY,
            MI_SHRINK_FIT,
            MI_ORIENT_BASE,
            MI_ORIENT_BASE + 1,
            MI_VSTYLE_BASE,
            MI_VSTYLE_BASE + 1,
            MI_VSTYLE_BASE + 2,
            MI_ANIM_BASE,
            MI_ANIM_BASE + 1,
            MI_ANIM_BASE + 2,
            MI_ANIM_BASE + 3,
            MI_SPEED_DOWN,
            MI_SPEED_UP,
            // 渲染模式与阴影归入外观组（2026-11-24 审查 U2）：
            // 它们是外观属性，不是「窗口与行为」/「显示内容」。
            MI_GLASS,
            MI_TEXT,
            MI_SHADOW,
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
            // ---- 灵动岛设置页（第二页）。顺序 = `table(Page::Island)` 的屏幕顺序，
            // 页面在表格里排在挂件页之后，`table_cmds()` 也是这么拼的。
            MI_ISLAND,
            MI_ISLAND_UNREAD,
            MI_ISLAND_KEEP,
            MI_ISLAND_ITEM_BASE,
            MI_ISLAND_ITEM_BASE + 1,
            MI_ISLAND_ITEM_BASE + 2,
            MI_ISLAND_ITEM_BASE + 3,
            MI_ISLAND_ITEM_BASE + 4,
            MI_ISLAND_ITEM_BASE + 5,
            MI_ISLAND_ITEM_BASE + 6,
            MI_ISLAND_ROT_DOWN,
            MI_ISLAND_ROT_UP,
            MI_ISLAND_OP_DOWN,
            MI_ISLAND_OP_UP,
            MI_ISLAND_BODY_COLOR,
            MI_ISLAND_ACCENT_COLOR,
            // 外观组：透明度 + 底色 + 强调色 + 提示点色（2026-11-24 重排岛页）
            MI_ISLAND_DOT_COLOR,
            MI_ISLAND_W_DOWN,
            MI_ISLAND_W_UP,
            MI_ISLAND_H_DOWN,
            MI_ISLAND_H_UP,
            // 新消息的两件事：拉长时长与提示点颜色（2026-11-24 产品逻辑）。
            // 手工登记在这里是**故意的** —— 这条断言的全部价值就是「加了控件
            // 必须有人签字」，自动跟随等于把签字环节删掉。
            MI_ISLAND_PEEK_DOWN,
            MI_ISLAND_PEEK_UP,
        ];
        let got = config_ui::table_cmds();
        let mut uniq = got.clone();
        uniq.sort_unstable();
        uniq.dedup();
        let same = got == expected.to_vec();
        let need_h = config_ui::plan_min_height();
        let slack = config_ui::plan_height_slack();
        let overlap = config_ui::plan_overlaps();
        let ok = same && uniq.len() == got.len() && slack >= 0 && overlap.is_none();
        rows.push((
            "设置面板·控件清单（表格即规格）",
            ok,
            format!(
                "{} 条命令 · 无重复={} · 与期望清单一致={}{} · 最小窗口高度余量 {:+}px（需要 {}px · 常量 {}px） · 无矩形交叠={}{}",
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
                slack,
                need_h,
                config_ui::min_track_h(),
                overlap.is_none(),
                match &overlap {
                    Some(m) => format!("（{}）", m),
                    None => String::new(),
                }
            ),
        ));
    }

    // ---- ⑥d 桌面适配：一键切档必须真的把对比度拉到可用
    //
    // 这条钉的是**产品承诺**：用户点一下「浅底」，字就该看得清。
    // 判据用 WCAG 对比度，而不是「颜色变了没有」—— 改一个相近色也能让
    // 后者通过，而那正是要防的（2026-11-24 实测：浅金 #F5D98B 在浅灰桌面
    // #E2E8ED 上只有 **1.03:1**，灰度上完全糊掉）。
    //
    // 断言还会红的两条：`fit_paint()` 偷偷改了用户原值（切回自定找不回来），
    // 以及两档配色撞成同一个色（「切换」变空操作）。
    {
        let c = a.cfg.clone();
        let paint_of = |fit: i32| {
            let mut t = c.clone();
            t.fit = fit;
            t.fit_paint()
        };
        let (p_custom, p_light, p_dark) = (paint_of(0), paint_of(1), paint_of(2));

        // 背景取实测值：浅灰 Excel 工具栏 / 项目默认深色壁纸。
        // `opacity` 也是实测的（用户配置 191）—— 半透明会把文字拉向背景，
        // 不带上它算出来的对比度是假的。
        const BG_LIGHT: u32 = 0xE2E8ED;
        const BG_DARK: u32 = 0x0B0D12;
        let a8 = c.opacity.clamp(1, 255) as u8;

        let r_custom = contrast_after_alpha(p_custom.color, BG_LIGHT, a8);
        let r_light = contrast_after_alpha(p_light.color, BG_LIGHT, a8);
        let r_dark = contrast_after_alpha(p_dark.color, BG_DARK, a8);

        // 「不动用户原值」是这功能的立身之本：切回自定必须逐字段还原。
        let untouched = p_custom.color == c.color
            && p_custom.date == c.date_color
            && p_custom.quote == c.quote_paint_color()
            && p_custom.shadow == c.shadow;
        let distinct = p_light.color != p_dark.color;
        let ok = untouched
            && distinct
            && p_light.shadow
            && !p_dark.shadow
            && r_light >= 4.5
            && r_dark >= 4.5;

        rows.push((
            "桌面适配·切档后对比度（WCAG 正文 4.5:1）",
            ok,
            format!(
                "浅底 {:.2}:1（同背景自定 {:.2}:1）· 深底 {:.2}:1 · 原值未动={} · 两档不同={} · 描边 {} / {}",
                r_light, r_custom, r_dark, untouched, distinct, p_light.shadow, p_dark.shadow
            ),
        ));
    }

    // ---- ⑥b 设置面板跨屏 DPI：整块界面必须等比换挡
    //
    // 面板是 per-monitor DPI aware 进程里的普通窗口，从 100% 屏拖到 150% 屏时
    // 系统**不做**任何虚拟化放大。布局如果还按同一批物理像素算，界面就缩成
    // 左上角一小块；只缩终点不缩分段的话，标签和按钮会互相压住。
    // 这里把布局函数在两个倍率下各排一遍，两种情况都能挡住：
    //   ① 每个矩形必须**等比**（±1px 取整）；
    //   ② 放大后仍然装得下、仍然不交叠。
    {
        let base = config_ui::plan_rects_at(1.0);
        let dbl = config_ui::plan_rects_at(2.0);
        let n = base.len();
        let same_len = n > 0 && dbl.len() == n;
        let mut worst = 0i32;
        let mut scaled_ok = same_len;
        if same_len {
            for (a, b) in base.iter().zip(dbl.iter()) {
                for (bv, av) in [
                    (b.left, a.left),
                    (b.top, a.top),
                    (b.right, a.right),
                    (b.bottom, a.bottom),
                ] {
                    let d = (bv - av * 2).abs();
                    worst = worst.max(d);
                    if d > 1 {
                        scaled_ok = false;
                    }
                }
            }
        }
        // 150%：客户区高 640×1.5=960，需求高度也得跟着 ×1.5
        let client150 = (config_ui::min_track_h() as f32 * 1.5).round() as i32;
        let need150 = config_ui::plan_need_height_at(1.5);
        let fit150 = need150 <= client150;
        let ov150 = config_ui::plan_overlaps_at(1.5);
        let ok = scaled_ok && fit150 && ov150.is_none();
        rows.push((
            "设置面板·跨屏 DPI 等比换挡",
            ok,
            format!(
                "{} 个矩形 · 200% 下最大偏差 {}px（应 ≤1）· 150% 需要 {}px / 客户区 {}px · 无交叠={}{}",
                n,
                worst,
                need150,
                client150,
                ov150.is_none(),
                match &ov150 {
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

    // ---- 备份：用户数据唯一的「回滚阀门」。这里要验的不是「复制了几个文件」，
    // 而是两条承诺：① 同秒连按两次**不覆盖**前一次（目录名递增后缀）；
    // ② 复制出来的字节和源文件**完全一致**（备份不可信就等于没有备份）。
    {
        let base = std::env::temp_dir().join(format!("desk-geyan-bak-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::create_dir_all(base.join("quotes"));
        std::fs::write(base.join("config.ini"), "font=SimSun\nscale=110\n").unwrap();
        std::fs::write(base.join("quotes.txt"), "甲一 —— A\n").unwrap();
        std::fs::write(base.join("quotes/02_b.txt"), "乙一 —— B\n").unwrap();
        std::fs::write(base.join("quotes/favorites.txt"), "丙一 —— C\n").unwrap();

        let first = crate::backup::backup(&base);
        let second = crate::backup::backup(&base);

        let (ok, detail) = match (&first, &second) {
            (Ok(p1), Ok(p2)) => {
                // ① 两次落在不同目录 —— 同一秒内连按也不许互相踩
                let distinct = p1 != p2;
                // ② 逐个文件逐字节比对
                let mut same = 0usize;
                let mut diff: Vec<String> = Vec::new();
                for name in ["config.ini", "quotes.txt"] {
                    let src = std::fs::read(base.join(name)).unwrap_or_default();
                    let dst = std::fs::read(p1.join(name)).unwrap_or_default();
                    if src == dst && !src.is_empty() {
                        same += 1;
                    } else {
                        diff.push(name.to_string());
                    }
                }
                for name in ["02_b.txt", "favorites.txt"] {
                    let src = std::fs::read(base.join("quotes").join(name)).unwrap_or_default();
                    let dst = std::fs::read(p1.join("quotes").join(name)).unwrap_or_default();
                    if src == dst && !src.is_empty() {
                        same += 1;
                    } else {
                        diff.push(format!("quotes/{}", name));
                    }
                }
                let manifest = p1.join("BACKUP.txt").is_file();
                let all_files = same == 4;
                (
                    distinct && all_files && manifest,
                    format!(
                        "两次备份 → {} / {}（须不同）· 逐字节一致 {}/4{} · BACKUP.txt={}",
                        p1.file_name().unwrap_or_default().to_string_lossy(),
                        p2.file_name().unwrap_or_default().to_string_lossy(),
                        same,
                        if diff.is_empty() {
                            String::new()
                        } else {
                            format!("（不一致：{}）", diff.join("、"))
                        },
                        manifest
                    ),
                )
            }
            _ => (
                false,
                format!("备份失败：{:?}", first.err().or_else(|| second.err())),
            ),
        };
        rows.push(("备份配置与收藏（不覆盖 · 逐字节一致）", ok, detail));
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- 诊断报告：菜单项的典型失败方式是「写完是空文件」或「静默没写」，
    // 两种都不会报错、不会崩，用户在需要它的那一刻才发现没有内容。
    // 顺带钉一条隐私边界：报告里不许出现格言正文 —— 这份文件是拿来贴给别人的。
    {
        let dest = data_dir().join("diagnostics.txt");
        let existed_before = dest.is_file();
        let r = crate::app::diagnostics::export(&a);
        let (ok, detail) = match r {
            Ok(p) => {
                let text = std::fs::read_to_string(&p).unwrap_or_default();
                // 每一节都必须真的落地，缺一段说明某个取值路径坏了
                let need = [
                    "desk-geyan 诊断报告",
                    "版本:",
                    "窗口:",
                    "DPI:",
                    "外观:",
                    "播放:",
                    "全屏守卫:",
                    "资源:",
                    "错误码:",
                    "开机自启:",
                    "词库:",
                ];
                let missing: Vec<&str> = need
                    .iter()
                    .copied()
                    .filter(|k| !text.contains(k))
                    .collect();
                // 隐私：当前句不许出现在报告里（太短的句子容易误判，跳过）
                let cur = &a.list[a.qi].text;
                let leak = cur.chars().count() >= 6 && text.contains(cur.as_str());
                (
                    missing.is_empty() && !leak && !text.is_empty(),
                    format!(
                        "{} 字节 · {} 节齐全={}{} · 泄漏当前句={}",
                        text.len(),
                        need.len() - missing.len(),
                        missing.is_empty(),
                        if missing.is_empty() {
                            String::new()
                        } else {
                            format!("（缺少 {}）", missing.join("、"))
                        },
                        leak
                    ),
                )
            }
            Err(e) => (false, format!("导出失败：{}", e)),
        };
        rows.push(("诊断报告（各节齐全 · 不泄漏正文）", ok, detail));
        if !existed_before {
            let _ = std::fs::remove_file(&dest);
        }
    }

    // ---- 剪贴板往返。**测前读回、测后恢复** —— selftest 是用户会跑的东西，
    // 不能把人家剪贴板里的内容吃掉。没有可读文本（空 / 图片 / 文件列表）时
    // **完全跳过写入**：这三者在 API 层都表现为「无 CF_UNICODETEXT」，
    // 而一旦 EmptyClipboard，后两种就永久丢失了。
    {
        let hwnd = a.panel;
        let before = clipboard::get_text(hwnd);
        if let Some(before_text) = before {
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
            let restored = clipboard::copy_text(hwnd, &before_text);
            rows.push((
                "剪贴板原内容恢复",
                restored,
                format!(
                    "原内容为文本，已原样恢复 · 恢复 {}",
                    if restored { "完成" } else { "失败" }
                ),
            ));
        } else if clipboard::has_text() {
            // 格式挂在那里、内容却读不出来（GlobalLock 失败或长度为 0）。
            // 这跟「剪贴板本来就是空的」是两件事：后者可以跳过，前者是缺陷。
            rows.push((
                "剪贴板往返（写 → 读 → 逐字相等）",
                false,
                "剪贴板声明含 CF_UNICODETEXT，但 get_text 读不出内容（读取路径有缺陷）"
                    .to_string(),
            ));
            rows.push((
                "剪贴板原内容恢复",
                true,
                "无需恢复：未执行任何剪贴板写入".to_string(),
            ));
        } else {
            rows.push((
                "剪贴板往返（写 → 读 → 逐字相等）",
                true,
                "跳过：当前剪贴板没有可读取的文本（不触碰图片 / 文件列表）".to_string(),
            ));
            rows.push((
                "剪贴板原内容恢复",
                true,
                "无需恢复：未执行任何剪贴板写入".to_string(),
            ));
        }
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

        // ---- ③b 旋转字体必须跟着缩字档一起缩
        //
        // 竖版里 `——` / `……` 靠 `lfEscapement = 900` 画成竖的一条，用的是
        // **另一支字体对象**。它曾经只有 100% 一支：正文缩到 85%/70% 之后，
        // 破折号仍是原字号，横贯整列、压住上下两个字 —— 而且只在
        // 「竖版 + 长句 + 缩字」三件事同时成立时出现，平时看不见。
        //
        // 判据是 GDI 读回来的 `tmHeight`（不是创建时传的 px —— 那只是申请值）。
        // **不能要求严格相等**：带 `lfEscapement` 的字形取整方式不同，
        // 实测同档旋转字恒定比正文高 1px（65/55/46 对 66/56/47）。
        // 所以给「2px 或 5%」的容差 —— 而真出 bug 时差的是整套档位
        // （100% 的 65 对上 70% 的 46，差 19px），照样一枪打死。
        {
            let th: Vec<(i32, i32)> = (0..gfx::QUOTE_SHRINK_STEPS.len())
                .map(|k| {
                    (
                        gfx::font_height(a.fonts.quote_at(k)),
                        gfx::font_height(a.fonts.quote_rot_at(k)),
                    )
                })
                .collect();
            let same = th
                .iter()
                .all(|&(t, r)| t > 0 && (t - r).abs() <= 2.max(t / 20));
            // 三档之间真的递减，否则「缩字档」名存实亡
            let shrink = th[0].0 > th[1].0 && th[1].0 > th[2].0;
            rows.push((
                "排版·旋转字体跟随缩字档",
                same && shrink,
                format!(
                    "100%/85%/70% 正文 {:?} · 旋转 {:?} · 同档等高={} · 逐档递减={}",
                    th.iter().map(|p| p.0).collect::<Vec<_>>(),
                    th.iter().map(|p| p.1).collect::<Vec<_>>(),
                    same,
                    shrink
                ),
            ));
        }

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
        // 竖排改版后 `sig` 只剩**署名一列**：时间与日期都搬到页头横排了，
        // 不再是落款块里的三列（出处 + 日期两行）。这里顺带钉死这条语义，
        // 免得谁把日期又塞回 `sig` 里、页头摆位悄悄退回旧样子。
        let sig_ok = cb.sig.len() == 1 && cb.sig[0].is_author;
        rows.push((
            "竖版·主文竖排右起",
            !ql.is_empty() && v_inside && right_first && sig_ok,
            format!(
                "{}x{} · {} 列 / 落款 {} 列（署名={}）· 全在窗内 {}",
                a.cfg.w,
                a.cfg.h,
                ql.lanes,
                cb.sig.len(),
                cb.sig.iter().any(|c| c.is_author),
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

        // 公式句也能转跑马灯。以前这里排除过 `has_math`，理由是「跑马灯画的
        // 是 `display()` 原文，会露出 `$...$`」；现在跑马灯会给公式句排一张
        // 定位表（`render::marquee_item`），排除就必须去掉 —— 否则窄窗口里
        // 的长公式句只能被截断 + 省略号，永远滚不起来。
        //
        // 这里只验「排版确实溢出」：`force_marquee` 的置位在 `build_frame_ex`
        // 里，要真窗口 + DC，自检不碰那一条。
        let saved_qi = a.list[a.qi].clone();
        a.cfg = Config::default();
        a.cfg.set_orient(Orient::Horizontal);
        a.cfg.anim = Anim::Marquee;
        a.cfg.w = 420;
        a.cfg.h = 130;
        a.force_marquee = false;
        a.list[a.qi] = Quote {
            text: "自然对数的底 $e = \\lim_{n \\to \\infty}\\left(1 + \\frac{1}{n}\\right)^n$ 由欧拉给出"
                .to_string(),
            author: "欧拉".to_string(),
        };
        a.rebuild_fonts();
        let cb = layout_clock(&mut a);
        let ql = frame_layout(&a, &cb);
        let math_scrolls = ql.overflow;
        a.list[a.qi] = saved_qi;
        rows.push((
            "横版·长公式句溢出可转滚",
            math_scrolls,
            format!(
                "{}x{} 里 {} 字形 + {} 公式块 → 溢出 {}",
                a.cfg.w,
                a.cfg.h,
                ql.glyphs.len(),
                ql.math.len(),
                ql.overflow
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
                .position(|g| g.ch == '，')
                .and_then(|i| (i > 0).then(|| (ql.glyphs[i], ql.glyphs[i - 1])))
                .map(|(dot, prev)| step > 0 && dot.y < prev.y + step && dot.x > prev.x)
                .unwrap_or(false)
        };
        rows.push((
            "竖版·句读点号归位到单元格右上",
            comma_ok,
            format!(
                "「，」共 {} 个字形",
                ql.glyphs.iter().filter(|g| g.ch == '，').count()
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
        // 高度要**恰好**让一列装得下一个分句（8 字）而装不下整句（16 字）：
        // 断了才有一句一列可验。实测 1052 宽下页头只吃 55px（时间日期同行），
        // 每格 30px ⇒ 373 给出 9 格的列高，列尾 3 格窗口正好罩住第 8 个字的「，」。
        // 给到 520 就是 13 格 —— 第一列能塞 13 个字，断点落在「古」后面，
        // 这条断言会红在一个跟断句逻辑毫无关系的地方。
        a.cfg.h = 373;
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
                .all(|g| g.ch != '—' && g.ch != '陆'); // —— 与「陆」都不在主文
        rows.push((
            "竖版·主文不带出处（署名归落款条）",
            attr_stripped,
            format!(
                "正文 {} 字 / 画出 {} 字形 / 竖杠 {} 个",
                want_chars,
                ql.len(),
                ql.glyphs.iter().filter(|g| g.ch == '—').count()
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
                "{} 列 · 列末字 {:?}",
                ql.col_ranges.len(),
                ql.col_ranges
                    .iter()
                    .map(|&(_, g1)| ql.glyphs[g1 - 1].ch)
                    .collect::<Vec<_>>()
            ),
        ));
        // ③ 署名跟排的最终位置只能从 `place_sign` 读 —— 真管线
        // `build_frame` 里调的就是它，读内核结果，不复制算法。
        //
        // 竖排改版后署名的语义从「落款条」变成了**题跋**：A/B 接在末列
        // 正下方（x 与正文块左缘对齐），C 栏头式挂在正文左侧（x 让开
        // 一个 em + 列宽）。旧的「右缘贴主文左缘、以 clock_rect 为准」
        // 是落款条时代的读法 —— 那时 `clock_rect` 就是落款条矩形，现在
        // 它是页头时钟块，再拿它量署名只会量到天头。
        //
        // 换一条**短句**：题跋要有地方落脚，正文就必须不占满版面。长句在
        // 竖版里会被拉伸到顶满整个版心（列数、列高按可用高度自适应），
        // 底下一点余量都不剩 —— 那时 `place_sign` 按设计让署名让位
        // （宁可不出名字，也不叠在字上），断言拿不到位置。
        a.cfg.h = 520;
        a.list[a.qi] = Quote {
            text: "上善若水。".to_string(),
            author: "老子".to_string(),
        };
        a.rebuild_fonts();
        let cb0 = layout_clock(&mut a);
        let ql0 = frame_layout(&a, &cb0);
        let cb1 = place_sign(&a.cfg, cb0, &ql0);
        let origin = crate::layout::content_origin(&ql0);
        let body_bottom = origin.1 + ql0.extent.1;
        let sig_ok = match cb1.sig.iter().find(|c| c.is_author) {
            Some(c) => {
                let below = c.y >= body_bottom; // 不压在正文上
                let align_x = (c.x - origin.0).abs() <= 2; // 接在末列正下方
                let rows_n = c.s.len() as i32;
                let inside = c.x >= 0
                    && c.y >= 0
                    && c.x + cb1.sig_w.max(1) <= a.cfg.w
                    && c.y + rows_n * cb1.sig_cell.max(1) <= a.cfg.h;
                below && align_x && inside
            }
            None => false,
        };
        rows.push((
            "竖版·署名跟排（题跋式接在末列下方）",
            sig_ok,
            format!(
                "署名 ({}, {}) · 正文 ({},{}) 尺寸 {:?} / 左缘差 {} px · 窗高 {} pad {} 格 {} · 列 {:?}",
                cb1.sig.first().map(|c| c.x).unwrap_or(-1),
                cb1.sig.first().map(|c| c.y).unwrap_or(-1),
                origin.0,
                origin.1,
                ql0.extent,
                cb1.sig
                    .first()
                    .map(|c| (c.x - origin.0).abs())
                    .unwrap_or(-1),
                a.cfg.h,
                a.cfg.pad_v(),
                cb1.sig_cell,
                ql0.col_ranges.iter().map(|&(a, b)| b - a).collect::<Vec<_>>()
            ),
        ));

        // ---- ④ 竖排改版的三条硬约束（2026-11-04 老板定）：
        //      ① 三形态可切，**切了之后 `layout_clock` 的缓存必须失效**；
        //      ② 时间在**所有**形态下都横排（逐字竖排那条路已经删干净了）；
        //      ③ 日期降级先保星期（「11月4日 星期三」放不下才退到「11月4日」）。
        //
        // 三条都是「删掉某段代码就必然红」的写法，不是复述现状。
        {
            let saved = a.cfg.clone();
            a.cfg.orient = Orient::Vertical;
            a.cfg.w = 420;
            a.cfg.h = 660;
            a.cfg.show_clock = true;
            a.cfg.show_date = true;
            a.cfg.show_quote = true;
            a.cfg.shadow = false;
            a.rebuild_fonts();

            // ① 缓存放失效。**必须先让缓存建立起来再切**：只在切完第一次读，
            //    缓存里什么都没有，谁都能"过"。这里每一轮都在上一轮的基础
            //    上换形态 —— 若指纹漏了 v_style，第二轮就会原样返回第一轮
            //    的 `time_at`，三组几何立刻退化成一种。
            /// 一种形态的页头几何：时间锚点、日期锚点、有没有界栏。
            type Geo = ((i32, i32), (i32, i32), bool);
            let mut geoms: Vec<Geo> = Vec::new();
            let mut names = Vec::new();
            for st in VStyle::all() {
                a.cfg.v_style = st;
                let cb = layout_clock(&mut a);
                geoms.push((cb.time_at, cb.date_at, cb.rule.is_some()));
                names.push(st.label());
            }
            let mut uniq = geoms.clone();
            uniq.sort_unstable();
            uniq.dedup();
            rows.push((
                "竖排·切形态缓存必失效（时间/日期/界栏随之变）",
                uniq.len() == geoms.len(),
                format!(
                    "{} 种形态 / {} 种几何 · {}",
                    geoms.len(),
                    uniq.len(),
                    names
                        .iter()
                        .zip(&geoms)
                        .map(|(n, g)| format!(
                            "{}@({},{}){}",
                            n,
                            g.0 .0,
                            g.0 .1,
                            if g.2 { " 有界栏" } else { "" }
                        ))
                        .collect::<Vec<_>>()
                        .join(" · ")
                ),
            ));

            // ② 时间横排：量 CH_CLOCK 通道在**页头范围内**的墨迹包围盒。
            //    "17:16:54" 横着排必然宽 > 高；逐字竖排则是高 > 宽。
            //    上界取 `head_bottom`（界栏正好落在这一行，不能把它算进来）。
            let mut horiz_ok = true;
            let mut detail = Vec::new();
            for st in VStyle::all() {
                a.cfg.v_style = st;
                let cb = layout_clock(&mut a);
                let y_max = cb.head_bottom.clamp(0, a.cfg.h);
                let op = a.text_alpha();
                build_frame(&mut a, op);
                let m0 = a.frame.channel_mask(0);
                let (w, h) = (a.cfg.w, a.cfg.h);
                let (mut x0, mut x1, mut y0, mut y1) = (i32::MAX, i32::MIN, i32::MAX, i32::MIN);
                for y in 0..y_max.min(h) {
                    for x in 0..w {
                        if m0[(y * w + x) as usize] >= 96 {
                            x0 = x0.min(x);
                            x1 = x1.max(x);
                            y0 = y0.min(y);
                            y1 = y1.max(y);
                        }
                    }
                }
                let (bw, bh) = (x1 - x0 + 1, y1 - y0 + 1);
                let ok = bw > 0 && bw >= bh * 2;
                if !ok {
                    horiz_ok = false;
                }
                detail.push(format!("{} {}x{}", st.label(), bw, bh));
            }
            rows.push((
                "竖排·时间三形态一律横排（宽须 ≥ 2 倍高）",
                horiz_ok,
                detail.join(" · "),
            ));

            // ③ 日期降级先保星期：扫一串宽度，凡是**掉了星期**的，都得证明
            //    「11月4日 星期三」那一档当时真的放不下 —— 否则就是降级链
            //    把星期当成了第一个可牺牲的东西。
            // 判据不靠「自己量一遍文本宽度」—— 那需要和排版层用同一个字体、
            // 同一套 padding，复刻一遍就等于把被测逻辑抄了一份，抄错了还
            // 一起错。这里只看**降级链的形态**：一路收窄，必须先丢年份、
            // 再丢星期，绝不能反过来，也不能丢完又冒出来。
            let mut order_ok = true;
            let mut trails: Vec<String> = Vec::new();
            let (mut saw_full, mut saw_no_year, mut saw_drop) = (false, false, false);
            let mut dropped_at: Option<i32> = None;
            for w in [460, 420, 360, 320, 280, 250, 220, 200, 180, 160, 140, 120, 100] {
                a.cfg.w = w;
                a.cfg.v_style = VStyle::Minimal;
                let cb = layout_clock(&mut a);
                let txt = cb
                    .date
                    .as_ref()
                    .map(|s| String::from_utf16_lossy(s))
                    .unwrap_or_default();
                let has_week = txt.contains('星');
                let has_year = txt.contains('年');
                if txt.is_empty() {
                    trails.push(format!("{}空", w)); // 连最短一档都放不下
                    continue;
                }
                if !has_week {
                    saw_drop = true;
                    dropped_at = dropped_at.or(Some(w));
                    trails.push(format!("{}✗", w));
                } else {
                    trails.push(format!("{}{}", w, if has_year { "✓年" } else { "✓" }));
                    // 掉过星期之后又出现带星期的 = 降级不单调
                    if dropped_at.is_some() {
                        order_ok = false;
                    }
                    if has_year {
                        saw_full = true;
                    } else {
                        saw_no_year = true;
                    }
                }
            }
            rows.push((
                "竖排·日期降级（先丢年份 · 再丢星期 · 单调）",
                order_ok && saw_full && saw_no_year && saw_drop,
                format!(
                    "各宽度：{} · 宽窗带年份={} 中档去年前保星期={} 窄窗丢星期={} 首丢于 {}px",
                    trails.join(" "),
                    saw_full,
                    saw_no_year,
                    saw_drop,
                    dropped_at.unwrap_or(-1)
                ),
            ));

            a.cfg = saved;
            a.rebuild_fonts();
        }

        // （431×1084 @300%）。两个失败模式各一条断言：
        //
        // ① 落款三列曾被压到列距 49px / 字宽 48px = 1.02 倍，三列在屏幕上糊成
        //    一条，分不出哪列是时间哪列是署名。现在列距有硬下限，放不下就减列。
        // ② `pad()` 只跟字号走（300% ⇒ 72px），在 431 宽的条幅上吃掉 33% 宽度，
        //    正文只剩一列半 + 省略号。竖版改用 `pad_v()`（按窗宽 8% 封顶）。
        {
            let saved = a.cfg.clone();
            a.cfg.w = 431;
            a.cfg.h = 1084;
            a.cfg.scale = 300;
            a.rebuild_fonts();
            let cb_n = layout_clock(&mut a);
            let ql_n = frame_layout(&a, &cb_n);
            // 列距不许被挤扁。竖排改版后署名只剩一列，量不到「相邻两列」了，
            // 改量**排版尺子**本身 —— 这条守的是 `Metrics::new` 里那个 1.70
            // 倍列距（曾被改成 1.15，窄窗大字号下三列糊成一条黑）。
            let m = layout::Metrics::new(a.cfg.quote_font_px(), a.cfg.quote_font_px());
            let pitch = m.col_pitch;
            let gap_ok = pitch as f32 >= m.char_w as f32 * 1.2;
            // 正文不截断（有省略号 = 真的装不下）。列数不再是判据：列距
            // 放宽到 1.70 倍之后，431 宽 @300% 就是一列，这是版面的必然结果。
            let body_ok = !ql_n.overflow;
            rows.push((
                "竖版·窄窗大字号（列距 / 正文不截断）",
                gap_ok && body_ok,
                format!(
                    "431x1084@300% · 列距 {}px / 字宽 {}px（须 ≥1.2 倍）· 正文 {} 列 · 截断={} · pad={}px",
                    pitch, m.char_w, ql_n.lanes, ql_n.overflow, a.cfg.pad_v()
                ),
            ));
            a.cfg = saved;
            a.rebuild_fonts();
        }

        // ---- ③c 竖版落款列**不许越出窗口**。
        // 落款是竖排的一条，高度 = 最长那列的格数 × 单元格。字号涨、窗口不涨时
        // 它会先顶到窗口底（`--dump 600x1084@300%`：日期列写到最后只剩一个
        // 「星」，星期几的其余字被窗口裁掉）。
        {
            let saved = a.cfg.clone();
            let mut bad = Vec::new();
            for (w, h, sc) in [(431, 1084, 300), (600, 1084, 300), (300, 660, 100)] {
                a.cfg.w = w;
                a.cfg.h = h;
                a.cfg.scale = sc;
                a.rebuild_fonts();
                let cb_n = layout_clock(&mut a);
                let bottom = cb_n.y + cb_n.h;
                if cb_n.show && bottom > h {
                    bad.push(format!("{w}x{h}@{sc}% 落款底 {bottom} > {h}"));
                }
            }
            rows.push((
                "竖版·落款列不越出窗口",
                bad.is_empty(),
                if bad.is_empty() {
                    "431/600×1084@300% 与 300×660@100% 全部在窗内".into()
                } else {
                    bad.join(" · ")
                },
            ));
            a.cfg = saved;
            a.rebuild_fonts();
        }

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

        // ---- ① 配置写入的**原子性**：写一半断电 / 磁盘满会在旧实现里留下
        // 一个截断的 config.ini，下次启动解析失败 → 位置、字体、DPI 全回默认。
        // 现在走「临时文件 + sync + 改名」，这里验的是它的两个可观察后果：
        //   · 写完不该残留 .tmp（残留说明改名失败过，且下次可能被误读）
        //   · 落盘的字段能逐项读回（不是「文件存在」就算过）
        {
            let (mut atom_ok, mut atom_detail) = (false, String::new());
            with_config_guard(|| {
                let c = Config {
                    x: 1234,
                    y: 567,
                    opacity: 187,
                    font: "SimSun".to_string(),
                    paused: true,
                    fav_only: true,
                    dpi: 144,
                    orient: Orient::Vertical,
                    ..Config::default()
                };
                c.save();

                let err = config::last_save_err();
                let tmp = Config::path().with_extension("ini.tmp");
                let leftover = tmp.exists();
                let back = Config::load();
                let same = back.x == 1234
                    && back.y == 567
                    && back.opacity == 187
                    && back.font == "SimSun"
                    && back.paused
                    && back.fav_only
                    && back.dpi == 144
                    && back.orient == Orient::Vertical;
                atom_ok = err == 0 && !leftover && same;
                atom_detail = format!(
                    "错误码={} · .tmp 残留={} · 回读一致={}（x={} y={} 不透明度={} dpi={}）",
                    err, leftover, same, back.x, back.y, back.opacity, back.dpi
                );
                0
            });
            rows.push(("配置原子写入（无 .tmp 残留 · 可原样读回）", atom_ok, atom_detail));
        }

        // ---- ② 写入**失败**必须留下痕迹。旧实现是 `let _ = fs::write(...)`，
        // 磁盘满 / 只读目录 / 被杀毒锁住时全都不吭声 —— 用户改完设置重启发现
        // 白改了，还以为程序没记住。这里把 `.tmp` 占成一个目录，让
        // `File::create` 必然失败，再断言错误码非 0。
        {
            let tmp = Config::path().with_extension("ini.tmp");
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::remove_dir_all(&tmp);

            let mut blocked = false;
            let mut got = 0u32;
            with_config_guard(|| {
                blocked = std::fs::create_dir_all(&tmp).is_ok();
                let c = Config {
                    x: 4321, // 若真的写进去，guard 也会恢复
                    ..Config::default()
                };
                c.save();
                got = config::last_save_err();
                0
            });
            let _ = std::fs::remove_dir_all(&tmp);

            rows.push((
                "配置写入失败会被记下来（不是静默的 ok）",
                blocked && got != 0,
                format!(
                    "阻断手段生效={} · last_save_err={}（须非 0）",
                    blocked, got
                ),
            ));
        }

        // ---- ⑥ DPI 单位换算：窗口 API 给 96 基准原始值，cfg.dpi 存百分数。
        // 这条是实测抓到的缺陷：三处调用都直接把 144 当成「144%」存下去，
        // 于是 150% 屏静默按 144% 排版、100% 屏按 96% —— 各差 4%，
        // 肉眼看不出来，但「跟随 DPI」永远差一截，且窗口放大倍数不是 1.5。
        {
            let cases = [(96, 100), (120, 125), (144, 150), (192, 200), (288, 300)];
            let mut bad = Vec::new();
            for (raw, want) in cases {
                let got = crate::ffi::dpi_pct(raw);
                if got != want {
                    bad.push(format!("{raw}→{got}（应 {want}）"));
                }
            }
            // 换算完再经 eff_scale_pct 走一遍：scale=100 + 150% 屏 ⇒ 150%
            let mut c = Config {
                dpi_auto: true,
                dpi: 150,
                scale: 100,
                ..Config::default()
            };
            let eff = c.eff_scale_pct();
            if eff != 150 {
                bad.push(format!("scale=100 + dpi=150 得 {eff}%（应 150%）"));
            }
            // 关掉跟随必须回到纯 scale，不留 4% 的尾巴
            c.dpi_auto = false;
            if c.eff_scale_pct() != 100 {
                bad.push(format!("关掉跟随仍得 {}%（应 100%）", c.eff_scale_pct()));
            }
            rows.push((
                "DPI 单位换算（原始 DPI → 百分比）",
                bad.is_empty(),
                if bad.is_empty() {
                    "96→100 / 144→150 / 192→200 / 288→300 · 150% 屏 eff=150% · 关跟随回到 100%".into()
                } else {
                    bad.join(" · ")
                },
            ));
        }

        // 恢复现场
        a.cfg = saved_cfg;
        a.list[a.qi] = saved_quote;
        a.rebuild_fonts();
    }

    let pass = print_rows(&rows);
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

// ---------------------------------------------------------------- 对比度

/// WCAG 2.x 相对亮度（sRGB → 线性）。
fn rel_lum(rgb: u32) -> f32 {
    let f = |v: u32| {
        let x = v as f32 / 255.0;
        if x <= 0.03928 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = ((rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF);
    0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
}

/// 文字按 `alpha` 混到背景之后，与背景的 WCAG 对比度。
///
/// **先在 sRGB 空间混，再算亮度** —— 这是绘制时的真实行为（`compose` 里按
/// alpha 逐通道混）。反过来「先算亮度再混」会高估对比度，算出来的数比屏幕
/// 上好看，而这种偏差恰好会掩盖「半透明导致文字发糊」这一类故障。
fn contrast_after_alpha(fg: u32, bg: u32, alpha: u8) -> f32 {
    let k = alpha as f32 / 255.0;
    let mix = |shift: u32| {
        let f = ((fg >> shift) & 0xFF) as f32;
        let b = ((bg >> shift) & 0xFF) as f32;
        (f * k + b * (1.0 - k)).round() as u32
    };
    let composed = (mix(16) << 16) | (mix(8) << 8) | mix(0);
    let (l1, l2) = (rel_lum(composed), rel_lum(bg));
    let (hi, lo) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
    (hi + 0.05) / (lo + 0.05)
}
