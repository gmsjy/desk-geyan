//! 帧构建与绘制：三通道掩码 → 合批 → ULW 上屏。
//!
//! 关键约束（改这里前先读 `gfx.rs` 的模块文档）：
//! * 一块 32bpp `ink` 的三字节分当 `CH_CLOCK` / `CH_DATE` / `CH_QUOTE` 三条掩码，
//!   `compose` 就地覆写为预乘 ARGB；**颜色只在 compose 里乘**。
//! * 合批（`PolyTextOutW`）的段内不变量是「`x` 按步进相接」，且**不跨列**。
//! * 每趟绘制的裁剪区 / 布局量必须一致，只有「画不画」可随趟次变。

use super::*;

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect, HBRUSH, HDC, HGDIOBJ, SelectObject, SetTextCharacterExtra};
use crate::transition;
use crate::config::{Anim, Config, Orient};
use crate::gfx::{self, Fonts, Frame, QUOTE_SHRINK_STEPS};
use crate::layout::{self, Metrics, QuoteLayout};
use crate::quotes::Quote;
use crate::consts::*;
use crate::util::*;

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
pub(crate) fn render_and_present(a: &mut App) {
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
pub(crate) fn mark_dirty(a: &mut App) {
    a.dirty = true;
}

/// 需要就出帧，但 `DRAG_FRAME_MS` 内最多一次。
///
/// 拖动时鼠标消息动辄每秒几百上千条，而每条都重绘的话——
/// 单是重绘就 700µs——消息只会在队列里越积越多。这里把「状态接收」和
/// 「画面输出」解耦：状态每条消息都即时更新（微秒级，绝无积压），
/// 画面按固定节拍出，且**最后一帧保证是最终状态**（`force`）。
pub(crate) fn render_if_due(a: &mut App, force: bool) {
    if !a.dirty {
        return;
    }
    if !force && a.last_present.elapsed().as_millis() < DRAG_FRAME_MS {
        return;
    }
    render_and_present(a);
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
pub(crate) fn build_frame(a: &mut App, opacity: u8) {
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
pub(crate) fn build_frame_ex(a: &mut App, opacity: u8, no_batch: Option<bool>) {
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
pub(crate) fn shadow_offset(cfg: &Config) -> i32 {
    ((2.0 * cfg.scale as f32 / 100.0).round() as i32).max(0)
}

/// 时间日期块的几何。
///
/// 位置 = **默认位置**（左侧、垂直居中）+ 用户拖动偏移，再夹进窗口内。
/// 夹取这一步是「位置可拖动」和「拖完找不回来」之间的分界线：
/// 无论怎么拖，整块内容始终至少完整可见地留在窗口里。
#[derive(Clone)]
pub(crate) struct ClockBox {
    pub(crate) show: bool,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) w: i32,
    pub(crate) h: i32,
    pub(crate) time: Option<Vec<u16>>,
    pub(crate) date: Option<Vec<u16>>,
    pub(crate) time_h: i32,
    pub(crate) gap: i32,
    /// 竖版落款：逐列小字，**右起**（第 0 列最靠主文）。空 = 横版。
    ///
    /// 竖版下 `time` / `date` 留空，内容全在这里 —— 落款是一串**竖排**小字，
    /// 跟横版「一行时间、一行日期」是两种几何，硬塞进同两个字段只会互相打结。
    pub(crate) sig: Vec<SigCol>,
    /// 竖版落款的单元格高
    pub(crate) sig_cell: i32,
    /// 竖版落款的列宽（日期字体一个汉字的实测宽）。出处列随格言过渡时
    /// 要用它圈出日期通道上的 x 带（PR3）。
    pub(crate) sig_w: i32,
}

/// 竖版落款里的一列。
#[derive(Clone)]
pub(crate) struct SigCol {
    s: Vec<u16>,
    pub(crate) x: i32,
    /// true → 走主色通道（mask）；false → 走日期色通道（dmask）
    pub(crate) main_color: bool,
    /// true → **出处列**：换句过渡时随格言一起淡（PR3，老板定的语义）。
    /// 时间列是时钟（主职、静帧锚点）、日期列是落款配角，都不动。
    pub(crate) is_author: bool,
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
pub(crate) fn layout_clock(a: &mut App) -> ClockBox {
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
pub(crate) fn hug_sign(cfg: &Config, mut cb: ClockBox, ql: &QuoteLayout) -> ClockBox {
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
pub(crate) fn quote_band(cfg: &Config, cb: &ClockBox) -> (i32, i32, i32, i32) {
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
pub(crate) fn band_degenerate(cfg: &Config, cb: &ClockBox) -> bool {
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
pub(crate) fn frame_band(cfg: &Config, cb: &ClockBox) -> (i32, i32, i32, i32) {
    match cfg.orient {
        Orient::Vertical => quote_band_v(cfg, cb),
        Orient::Horizontal => quote_band(cfg, cb),
    }
}

/// 当前这一帧的排版结果。
pub(crate) fn frame_layout(a: &App, cb: &ClockBox) -> QuoteLayout {
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
pub(crate) enum Pass {
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
