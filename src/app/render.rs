//! 帧构建与绘制：三通道掩码 → 合批 → ULW 上屏。
//!
//! 关键约束（改这里前先读 `gfx.rs` 的模块文档）：
//! * 一块 32bpp `ink` 的三字节分当 `CH_CLOCK` / `CH_DATE` / `CH_QUOTE` 三条掩码，
//!   `compose` 就地覆写为预乘 ARGB；**颜色只在 compose 里乘**。
//! * 合批（`PolyTextOutW`）的段内不变量是「`x` 按步进相接」，且**不跨列**。
//! * 每趟绘制的裁剪区 / 布局量必须一致，只有「画不画」可随趟次变。

use super::*;

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect, HBRUSH, HDC, HGDIOBJ, HFONT, SelectObject, SetTextCharacterExtra};
use crate::transition;
use crate::config::{Anim, Config, Orient, VStyle};
use crate::gfx::{self, Fonts, Frame, QUOTE_SHRINK_STEPS};
use crate::layout::{self, Metrics, QuoteLayout};
use crate::quotes::Quote;
use crate::util::*;

// ---------------------------------------------------------------- 渲染

/// 逐列渐变因为列数超出带容量而降级成「整块一起过渡」的次数。
///
/// 这是**可观测的降级**，不是错误：排版层的列数上界由窗口宽度决定
/// （`avail_w / col_pitch`），而带表是定长数组，两者天生不对齐。
/// 与其让超出的列静默满亮（视觉上右半边闪一下），不如整块退化成通带，
/// 并把次数记下来 —— 重现不了的问题，至少能从这里看出它发生过。
pub(crate) static BAND_FALLBACK: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// 逐列渐变降级次数（0 = 从未发生）
pub(crate) fn band_fallback() -> u32 {
    BAND_FALLBACK.load(std::sync::atomic::Ordering::Relaxed)
}

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
    /// 竖版界栏专用（见 `App::rule_brush`）
    rule_brush: HBRUSH,
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

    // 出处列的**有效系数**：错峰期跟最左列（阅读序最末才落定），其余相位跟整块 `qk`。
    //
    // 以前只在错峰期算它，于是 Leave / Gap 期出处**完全不淡**：颜色系数恒 255、
    // 阴影灰度恒 0，换句时正文在淡、署名钉着不动，收尾那一下像「跳」了一下。
    let author = cb.sig.iter().find(|c| c.is_author);
    let author_k = if staggered {
        a.trans.col_k(ncols - 1, ncols)
    } else {
        qk
    };
    let author_u8 = (author_k * 255.0).round().clamp(0.0, 255.0) as u8;
    let author_gray = if author.is_some() && author_u8 < 255 {
        shadow_gray(author_k)
    } else {
        0
    };

    // 出处带：x 范围是署名那一列，**y 范围限定到署名自己的高度**。
    //
    // 为什么必须带 y：出处和页头日期**都在 `CH_DATE` 通道上**，而两者的 x
    // 区间经常重叠（竖版极简实测：日期 x=145..、出处 x=140..）。只按 x 分带
    // 的话，出处淡入会把页头日期一起压暗 —— 那半截日期在 260ms 里莫名变灰，
    // 而它本该是静帧锚点。
    //
    // 两条系数都给 `author_u8`，但**真正起作用的是日期那条**：署名画在
    // `CH_DATE` 通道上，格言系数在这个矩形里没有墨迹可乘。
    //
    // 格言那条不写死 255 是防一手：`get` 是先匹配先赢，万一哪天排版让
    // 「署名列」与「某条主文列」x 区间重叠，写 255 就等于给那几行正文
    // 免了淡出（入场时「提前浮出」、出场时「不肯消失」）。给它同一个 k，
    // 重叠时最坏也只是「跟着署名一起淡」，不会露馅。
    let push_author = |t: &mut gfx::BandTable| {
        if let Some(ac) = author {
            let h = (ac.s.len() as i32) * cb.sig_cell.max(1);
            t.push_box(
                ac.x,
                ac.x + cb.sig_w.max(1),
                ac.y,
                ac.y + h.max(1),
                author_u8,
                author_u8,
            );
        }
    };

    let mut col_grays = Vec::new();
    // **先建出处带、再建列带**：`get` 先匹配先赢，而末列瓦片为了兜住阴影
    // 外溢会左扩到窗口边缘、恰好盖住出处区 —— 出处带后 push 就永远排不上号
    // （实测：比值全 1.000，出处只动了阴影没动颜色）。
    // 非错峰相位所有列同一个 k，出处随整块 qk 走即可，不需要单独的列带。
    let bands = if staggered {
        // 列数 + 出处那条必须装得下。装不下就整体退化成通带：
        // 逐列带只装进去一部分的话，落在外面的列 `get` 查不到带、
        // 系数退回 255（满亮），于是过渡期间右半边会「不淡入地闪一下」——
        // 比「整块一起淡入」难看得多，而且几乎不可能凭描述复现。
        let need = ql.col_x.len() + usize::from(author.is_some() && author_u8 < 255);
        if need > gfx::MAX_BANDS {
            BAND_FALLBACK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            gfx::BandTable::uniform(qk_u8, 255)
        } else {
            let mut t = gfx::BandTable::empty();
            if author_u8 < 255 {
                push_author(&mut t);
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
        }
    } else if author.is_some() && author_u8 < 255 {
        // 非错峰（出场 / 空档 / 呼吸）：整块一条通带，但出处那一列要单独
        // 一条日期带，否则署名不跟着淡。通带**后 push**，出处先赢。
        let mut t = gfx::BandTable::empty();
        push_author(&mut t);
        t.push(0, i32::MAX, qk_u8, 255);
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
    // 不可见时消费掉脏标记但不画：恢复可见的路径会强制补一帧。
    if !a.cfg.visible || a.fs_hidden {
        a.dirty = false;
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
    // **取色只走 `fit_paint()`**：桌面适配档位（浅底 / 深底）在这里生效。
    // 别再单独读 `cfg.color` / `cfg.shadow` —— 两处各读一份，档位就会在
    // 某些绘制路径上悄悄失效，而画面照样好看、只是颜色不对。
    let paint = a.cfg.fit_paint();
    let shadow = paint.shadow;
    let (color, date_color) = (paint.color, paint.date);
    let quote_color = paint.quote;
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

    // ---- 竖版署名跟排（题跋式，2026-11-04 竖排改版）：署名接在正文末尾，
    // 短句时跟得紧、长句时退到地脚，正文占满版面时让位。只对 A / B 生效。
    let cb = place_sign(&a.cfg, cb, &ql);
    let crect = RECT {
        left: cb.x,
        top: cb.y,
        right: cb.x + cb.w.max(CLOCK_MIN_GRAB),
        bottom: cb.y + cb.h.max(CLOCK_MIN_GRAB),
    };

    // 连缩到 70% 都放不下 → 横版转跑马灯。**竖版没有跑马灯**（横向滚动 ×
    // 竖排字格 = 横竖混排的怪物），就地带省略号收尾。
    //
    // 含公式的句子**同样可以转**：跑马灯那条路现在会为公式句排一张定位表
    // （`marquee_item`），不再是「画 `display()` 原文、露出 `$...$`」。
    a.force_marquee = ql.overflow
        && a.cfg.show_quote
        && a.cfg.orient == Orient::Horizontal;
    // 生效动效**只从 `App::eff_anim()` 取**（它已含 force_marquee + 竖版降级）。
    // 这里以前又内联判断了一遍，成了「同一概念第四份定义」。
    let eff_anim = a.eff_anim();

    // 首次进入跑马灯：把 `m` 贴到带左缘。
    //
    // `step` 里也有一段同样的初始化，但**拖动期间 `step` 不跑**（tick 在
    // `grab != None` 时早退）—— 而「窗口缩小到放不下」正好可能发生在拖动中。
    // 那时 `m` 还是 0，绘制端算出的 `x = br - 0 = br`：整串落在带右缘之外，
    // 一帧空白。用户看到的就是「一缩，字先没了」。
    if eff_anim == Anim::Marquee && !a.primed {
        a.primed = true;
        a.m = marquee_band_w(&a.cfg);
    }

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
        rule_brush,
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
        rule_brush: *rule_brush,
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
    ((2.0 * cfg.eff_scale()).round() as i32).max(0)
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
    /// 默认位置（未叠 `clock_dx/dy`）。夹取偏移时按它反推，
    /// 于是「默认位置」只有一处定义，横竖两套都在各自量的时候写进去。
    pub(crate) dx0: i32,
    pub(crate) dy0: i32,
    pub(crate) time: Option<Vec<u16>>,
    pub(crate) date: Option<Vec<u16>>,
    pub(crate) time_h: i32,
    pub(crate) gap: i32,
    /// 竖版页头：时间 / 日期各自的左上角（**都横排**）。
    /// 横版用不到（位置由 `x/y + time_h + gap` 推）。
    pub(crate) time_at: (i32, i32),
    pub(crate) date_at: (i32, i32),
    /// 竖版页头用的时钟字体（大钟或小钟，按实测宽度选）。
    pub(crate) clock_face: HFONT,
    /// 竖版界栏：`(y, x0, x1)`。`None` = 这一形态没有界栏（B 极简）。
    pub(crate) rule: Option<(i32, i32, i32)>,
    /// 竖版页头下端（含界栏）。主文档心从它往下算 —— **页头永远在正文之上**，
    /// 这是「时间放顶部」这条设计决定的唯一落点。
    pub(crate) head_bottom: i32,
    /// 竖版落款：逐列小字，**右起**（第 0 列最靠主文）。空 = 横版。
    ///
    /// 竖版下 `time` / `date` 是**横排**的页头，`sig` 只剩署名一列竖排 ——
    /// 两者是两种几何，混在同一个字段里只会互相打结。
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
    /// 这一列的字（UTF-16）。写入方在 `layout_header_v` / `place_sign`。
    pub(crate) s: Vec<u16>,
    pub(crate) x: i32,
    /// 这一列的**顶端**。竖版的列可以贴地脚（署名，A/B）也可以贴天头（C），
    /// 位置不再共用一个 `cb.y` —— 所以每列自带 y。
    pub(crate) y: i32,
    /// true → **出处列**：换句过渡时随格言一起淡（PR3，老板定的语义）。
    pub(crate) is_author: bool,
}

/// 量一次时间日期块的几何（无缓存，见 `layout_clock`）。
fn layout_clock_raw(cfg: &Config, fonts: &Fonts, dc: HDC, quote: &Quote) -> ClockBox {
    if cfg.orient == Orient::Vertical {
        return layout_header_v(cfg, fonts, dc, quote);
    }
    let scale = cfg.eff_scale();
    let (w, h) = (cfg.w, cfg.h);
    let pad = cfg.pad();

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
        dx0: pad,
        dy0: 0,
        time,
        date,
        time_h: 0,
        gap: 0,
        time_at: (0, 0),
        date_at: (0, 0),
        clock_face: null_mut(),
        rule: None,
        head_bottom: 0,
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
    cb.clock_face = clock_face;
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
    cb.dx0 = pad.clamp(0, max_x);
    cb.dy0 = ((h - cb.h) / 2).clamp(0, max_y);
    cb.x = (pad + cfg.clock_dx).clamp(0, max_x);
    cb.y = ((h - cb.h) / 2 + cfg.clock_dy).clamp(0, max_y);
    cb.time_at = (cb.x, cb.y);
    cb.date_at = (cb.x, cb.y + cb.time_h + gap);
    cb.head_bottom = cb.y + cb.h;
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
/// 时钟刻度（格式化后的文本已经进入 `ClockBox`）、`w/h`（夹取范围）、
/// `scale`（默认位置）、`seconds`（字面量宽度）、
/// `show_clock`/`show_date`/`orient`（量什么）、`qi`（竖版出处列宽）、
/// `clock_dx/dy`（夹取会回写它们）、`font_epoch`（字号/字体）。
///
/// 换句走 `qi`、换字号走 `font_epoch`、走秒走 `clock_stamp` —— 三条自动覆盖，
/// 不必在每个写配置的地方手工插一句作废。漏一项的表现是「改了没生效」，
/// 极难复现，所以宁可多列几个分量。
///
/// # 唯一覆盖不到的写法
///
/// 指纹认的是 `qi`（**哪一句**），不是句子内容。所以「就地替换 `list[qi]`」
/// （不换 `qi`、只换文本或出处）不会让缓存失效 —— 拿回来的是上一句的页头，
/// 署名用的是旧 `author`，整个署名列会凭空消失或停在旧位置。现有代码里
/// 没有这种写法（换句一律动 `qi`，自检里造句子紧跟 `rebuild_fonts`，
/// 后者涨 `font_epoch`）；将来要加，就得自己把 `font_epoch` 也涨一下。
pub(crate) fn layout_clock(a: &mut App) -> ClockBox {
    let key = (
        clock_stamp(&a.cfg),
        a.cfg.w,
        a.cfg.h,
        a.cfg.scale,
        i32::from(a.cfg.seconds),
        a.qi as i32,
        u32::from(a.cfg.show_clock),
        u32::from(a.cfg.show_date),
        // 版式 + 竖版形态打包成一个分量：页头的排法两者都决定，漏一个就会出现
        // 「切了形态，时间还停在旧位置」——缓存命中比正确性更「持久」。
        // （元组的 `PartialEq` 只到 12 元，所以不能各占一格。）
        u32::from(a.cfg.orient == Orient::Vertical) * 4
            + match a.cfg.v_style {
                VStyle::Banner => 0u32,
                VStyle::Minimal => 1u32,
                VStyle::Head => 2u32,
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

/// 竖版落款列的文字适配：超出「一列能排的格数」就截断并补省略号。
///
/// 竖排一列是从上往下读完的，超出的部分会被窗口底直接裁掉 —— 画面上就是
/// 半句话凭空没了（实测 600×1084@300%：日期只剩一个「星」）。宁可显式给省略号。
fn sign_fit(s: &str, max_cells: usize) -> String {
    let n = s.chars().count();
    if n <= max_cells {
        return s.to_string();
    }
    if max_cells <= 1 {
        return "…".to_string();
    }
    let mut out: String = s.chars().take(max_cells - 1).collect();
    out.push('…');
    out
}

/// 竖版**页头**：时间与日期横排压顶，下面一条界栏（A / C 才有）。
///
/// # 为什么时间一定要横排、一定要在顶部
///
/// 旧实现把时间当落款的一列逐字竖排，`15:40:36` 于是成了八格八个字，
/// 读起来是「一 / 五 / 冒号 / 四 / 零 …」八拍，扫一眼拿不到时间。
/// 时钟是这块挂件的**静帧锚点**，必须一眼可读 —— 横排、置顶、字号独立于正文。
///
/// 三个形态共用这一个函数，差别只在三段：
///
/// | | 时间 | 日期 | 界栏 | 署名 |
/// |---|---|---|---|---|
/// | A 条幅 | 居中 | 居中在下一行 | 有 | 正文末尾左下方 |
/// | B 极简 | 左上角 | 尽量与时间同一行 | 无 | 正文末尾左下方 |
/// | C 栏头 | 左上角 | 右上角同一行 | 有 | 版心左侧一列（贴天头） |
///
/// 日期**始终带星期**（老板拍板）：`2026年11月4日 星期三`，放不下才逐级降级
/// 到「11月4日 星期三」→「11月4日」—— 降级到最后一级也不丢「几号」。
fn layout_header_v(cfg: &Config, fonts: &Fonts, dc: HDC, quote: &Quote) -> ClockBox {
    let (w, h) = (cfg.w, cfg.h);
    let pad = cfg.pad_v();
    let em = fonts.quote_px[0].max(6);
    let style = cfg.v_style;
    let inner_l = pad;
    let inner_r = (w - pad).max(inner_l + 1);
    let inner_w = (inner_r - inner_l).max(1);
    let cap = ((inner_w as f32) * 0.92).round() as i32;
    let t = local_time();

    let mut cb = ClockBox {
        show: false,
        x: 0,
        y: 0,
        w: 0,
        h: 0,
        dx0: pad,
        dy0: pad,
        time: None,
        date: None,
        time_h: 0,
        gap: 0,
        time_at: (pad, pad),
        date_at: (pad, pad),
        clock_face: null_mut(),
        rule: None,
        head_bottom: pad,
        sig: Vec::new(),
        sig_cell: 0,
        sig_w: 0,
    };

    // ---- 量文字：时间用**竖版专用**的那支（正文 1.25 倍），装不下再逐级退；
    //      日期按宽度逐级降级（星期是老板点名要保的，排在降级链第二级）
    let mut time_w = 0;
    let mut time_h = 0;
    if cfg.show_clock {
        let s = wide(&fmt_clock(&t, cfg.seconds));
        for face in [fonts.clock_v, fonts.clock_small, fonts.date] {
            unsafe { SelectObject(dc, face as HGDIOBJ) };
            let (tw, th) = gfx::measure_text(dc, &s);
            cb.clock_face = face;
            time_w = tw;
            time_h = th;
            if tw <= cap {
                break;
            }
        }
        cb.time = Some(s);
        cb.time_h = time_h;
    }

    let mut date_w = 0;
    let mut date_h = 0;
    let gap_x = ((em as f32) * 0.55).round().max(2.0) as i32;
    let gap_y = ((em as f32) * 0.32).round().max(1.0) as i32;
    if cfg.show_date {
        const WEEK: [&str; 7] = ["日", "一", "二", "三", "四", "五", "六"];
        let wk = WEEK[(t.wDayOfWeek as usize).min(6)];
        // 降级链：带年份 → 只留月日但保住星期 → 只剩月日。
        // 「星期」是老板点名要保的，所以它在第二级还在，只有窄到装不下才丢。
        let cands = [
            fmt_date(&t),
            format!("{}月{}日 星期{}", t.wMonth, t.wDay, wk),
            format!("{}月{}日", t.wMonth, t.wDay),
        ];
        unsafe { SelectObject(dc, fonts.date_v as HGDIOBJ) };
        // **星期优先**：老板点名要保。所以先在「带星期」的两档里挑能整行放下的
        // 最长那个（先用带年份的，放不下退到「11月4日 星期三」）；
        // 连带星期的都放不下，才退到「11月4日」。
        //
        // 注意判据是「整行放得下」而不是「能跟时间挤一行」：为了同一行而丢星期，
        // 是拿用户明确要的信息去换版式，方向反了。能不能挤同一行由下面摆位那段
        // 再判 —— 挤不下就落到第二行。
        let mut pick = None;
        for (i, s) in cands.iter().take(2).enumerate() {
            let (dw, dh) = gfx::measure_text(dc, &wide(s));
            if dw <= cap {
                pick = Some((i, dw, dh));
                break;
            }
        }
        let (idx, dw, dh) = match pick {
            Some(p) => p,
            None => {
                let (dw, dh) = gfx::measure_text(dc, &wide(&cands[2]));
                (2, dw, dh)
            }
        };
        date_w = dw;
        date_h = dh;
        cb.date = Some(wide(&cands[idx]));
    }

    // ---- 摆位（先按「页头从 pad 起」算默认位置，之后再整体叠加拖动偏移）
    let top = pad;
    let (head_bottom, bx0, by0, bx1, by1);

    match style {
        VStyle::Minimal => {
            // 一行放得下就一行（桌面挂件最省高度），放不下分成两行、都靠左
            let mut x = inner_l;
            let mut line_bottom = top;
            if cb.time.is_some() {
                cb.time_at = (inner_l, top);
                x = inner_l + time_w + gap_x;
                line_bottom = top + time_h;
            }
            if cb.date.is_some() {
                if cb.time.is_some() && x + date_w <= inner_r {
                    cb.date_at = (x, top + (time_h - date_h) / 2);
                    line_bottom = line_bottom.max(top + time_h.max(date_h));
                } else {
                    let dy = if cb.time.is_some() {
                        top + time_h + gap_y
                    } else {
                        top
                    };
                    cb.date_at = (inner_l, dy);
                    line_bottom = dy + date_h;
                }
            }
            head_bottom = line_bottom;
            bx0 = inner_l;
            by0 = top;
            bx1 = inner_r;
            by1 = line_bottom;
        }
        VStyle::Banner => {
            // 居中压顶：时间一行、日期一行，下面是界栏
            let mut y = top;
            if cb.time.is_some() {
                cb.time_at = (inner_l + (inner_w - time_w).max(0) / 2, y);
                y += time_h;
            }
            if cb.date.is_some() {
                let dy = if cb.time.is_some() { y + gap_y } else { y };
                cb.date_at = (inner_l + (inner_w - date_w).max(0) / 2, dy);
                y = dy + date_h;
            }
            let rule_y = y + ((em as f32) * 0.55).round().max(2.0) as i32;
            cb.rule = Some((rule_y, inner_l, inner_r));
            head_bottom = rule_y;
            bx0 = inner_l;
            by0 = top;
            bx1 = inner_r;
            by1 = rule_y;
        }
        VStyle::Head => {
            // 栏头：时间靠左、日期靠右，同一行（放不下才让日期落到第二行右端）
            let mut y = top;
            let line_bottom;
            if cb.time.is_some() {
                cb.time_at = (inner_l, y);
            }
            if cb.date.is_some() {
                let right_x = inner_r - date_w;
                let same_line = cb.time.is_some() && right_x >= inner_l + time_w + gap_x;
                if same_line {
                    cb.date_at = (right_x, y + (time_h - date_h) / 2);
                    line_bottom = y + time_h.max(date_h);
                } else {
                    let dy = if cb.time.is_some() { y + time_h + gap_y } else { y };
                    cb.date_at = (right_x.max(inner_l), dy);
                    line_bottom = dy + date_h;
                }
            } else {
                line_bottom = y + time_h;
            }
            y = line_bottom;
            let rule_y = y + ((em as f32) * 0.55).round().max(2.0) as i32;
            cb.rule = Some((rule_y, inner_l, inner_r));
            head_bottom = rule_y;
            bx0 = inner_l;
            by0 = top;
            bx1 = inner_r;
            by1 = rule_y;
        }
    }

    // ---- 署名（竖排一列，**小字**）
    //
    // 用页头日期那支 55% 的字体，不用正文那支：题跋本来就是小字落款，
    // 用正文同级字号会喧宾夺主 —— 实测 300×660@200% 下署名三字占掉
    // 205px 纵向空间，比十二个字的正文还高，正文被迫挤成 4 列每列 3 字。
    //
    // 出处原样用，**不套书名号** —— 内置库里已经有「《周易》」这种自带书名号的
    // 出处，再套一层就成了《《周易》》。
    if !quote.author.is_empty() && cfg.show_quote {
        unsafe { SelectObject(dc, fonts.date_v as HGDIOBJ) };
        let (cw, ch) = gfx::measure_text(dc, &wide("国"));
        let (cw, ch) = (cw.max(1), ch.max(1));
        let cell = ((ch as f32) * 1.28).round() as i32;
        let floor_y = if style == VStyle::Head {
            // C：署名贴版心顶部，与正文同起
            head_bottom + ((em as f32) * 0.95).round() as i32
        } else {
            pad
        };
        let max_cells = (((h - pad - floor_y) / cell.max(1)).max(1)) as usize;
        let s = sign_fit(&quote.author, max_cells);
        let rows = s.chars().count() as i32;
        // A / B 的最终 y 要等正文排完才知道（题跋式跟排，见 `place_sign`），
        // 这里先给一个「贴窗口底」的保守位置。
        let y = if style == VStyle::Head {
            floor_y
        } else {
            (h - pad - rows * cell).max(floor_y)
        };
        cb.sig.push(SigCol {
            s: wide(&s),
            x: inner_l,
            y,
            is_author: true,
        });
        cb.sig_cell = cell;
        cb.sig_w = cw;
        // 署名**不进**页头的包围盒：页头框是拖动命中的范围，署名在窗口底部，
        // 算进去会让「拖空白处移动整窗」被误判成「拖时间」。
    }
    cb.head_bottom = head_bottom;

    // ---- 整体叠加拖动偏移，然后夹进窗口
    let bw = (bx1 - bx0).max(1);
    let bh = (by1 - by0).max(1);
    cb.dx0 = bx0.clamp(0, (w - bw).max(0));
    cb.dy0 = by0.clamp(0, (h - bh).max(0));
    let max_x = (w - bw).max(0);
    let max_y = (h - bh).max(0);
    let nx = (cb.dx0 + cfg.clock_dx).clamp(0, max_x);
    let ny = (cb.dy0 + cfg.clock_dy).clamp(0, max_y);
    let (dx, dy) = (nx - bx0, ny - by0);
    if dx != 0 || dy != 0 {
        // 时间 / 日期 / 界栏 / 署名一起挪 —— 页头是一块，不是几个独立元素
        cb.time_at = (cb.time_at.0 + dx, cb.time_at.1 + dy);
        cb.date_at = (cb.date_at.0 + dx, cb.date_at.1 + dy);
        if let Some((ry, x0, x1)) = cb.rule {
            cb.rule = Some((ry + dy, x0 + dx, x1 + dx));
        }
        for c in &mut cb.sig {
            c.x += dx;
            c.y += dy;
        }
        cb.head_bottom += dy;
    }
    cb.x = nx;
    cb.y = ny;
    cb.w = bw;
    cb.h = bh;
    cb.show = cb.time.is_some() || cb.date.is_some();
    cb
}

/// 最小可用版心宽度。低于它就换一种摆法，而不是整段不画。
const MIN_QUOTE_BAND: i32 = 96;

/// 堆叠摆法下，版心至少要有这个高度
const MIN_QUOTE_H: i32 = 44;

/// 竖版**署名跟排**：排完正文才知道它该落在哪。
///
/// 竖排的落款天然属于「读完之后」的位置 —— 所以 A / B 两套把它**接在正文
/// 末尾**（题跋式），而不是钉在窗口左下角：短句时跟得紧，长句时退到地脚。
/// 正文把版面占满时干脆不画（正文优先，署名是配角）。
///
/// 只对 A / B 生效：C 栏头的署名是版心左侧的一整列，位置在 `layout_header_v`
/// 里就定死了（贴天头），不需要这一步。
pub(crate) fn place_sign(cfg: &Config, mut cb: ClockBox, ql: &QuoteLayout) -> ClockBox {
    if cfg.orient != Orient::Vertical || cb.sig.is_empty() || ql.is_empty() {
        return cb;
    }
    let pad = cfg.pad_v();
    let em = cfg.quote_font_px();
    // 正文块的**实际**上下缘直接从定位表读，不再从版心反推 ——
    // 正文排完之后 `frame_layout` 还会整体上提一把（把页头占掉的半高还给它），
    // 反推出来的位置会比实际低一截，署名就会被判成「压到正文」而整列消失。
    let origin = layout::content_origin(ql);
    let body_bottom = origin.1 + ql.extent.1;
    let rows = cb.sig[0].s.len().saturating_sub(1) as i32;
    let tall = (rows - 1).max(0) * cb.sig_cell + cb.sig_w;
    let gap = ((em as f32) * 1.25).round() as i32;

    if cfg.v_style == VStyle::Head {
        // C 栏头：署名是**正文左边的一列**，贴住正文块（而不是钉在窗口左缘）。
        // 钉左缘时正文靠右、署名靠左，中间空出一大片 —— 看着像两个不相关的块。
        let x = origin.0 - ((em as f32) * 0.75).round() as i32 - cb.sig_w;
        if x < pad {
            cb.sig.clear(); // 正文占满宽度：署名让位
            return cb;
        }
        cb.sig[0].x = x;
        // 竖排从上往下读，署名与正文**同起**才对得齐 —— 贴版心上边界会浮在
        // 正文头顶（正文是竖直居中的，版心顶往往高出正文一截）。
        cb.sig[0].y = origin.1.max(pad);
        return cb;
    }

    // A / B：接在**最后一列正下方**（题跋式）。竖排是「读完正文再落名」，
    // 名字贴着末列读下来才是一句话；钉在窗口左下角会和正文隔开一大片空白，
    // 看起来像两个不相干的东西（草案 B 的实测结论）。
    let want = body_bottom + gap;
    let floor = cfg.h - pad - tall;
    let y = want.min(floor);
    if y < body_bottom + 1 {
        // 下方确实没地方了（正文一路排到窗口底 = 窗口太矮或字号太大）：
        // **横向**让位到正文块左侧，而不是直接消失。
        //
        // 「消失」看着像「这句本来就没有出处」—— 用户没法区分那是排版
        // 决策还是 bug。挪到左边仍然是题跋：紧贴正文、读得下去，只是
        // 换了个方向落款（C 栏头本来就这么排）。
        let x = origin.0 - ((em as f32) * 0.5).round() as i32 - cb.sig_w;
        if x >= pad {
            cb.sig[0].x = x;
            cb.sig[0].y = (body_bottom - tall).max(origin.1).max(pad);
            return cb;
        }
        cb.sig.clear(); // 连左边都挤不出：正文占满整窗，署名让位
        return cb;
    }
    cb.sig[0].x = origin.0.max(pad);
    cb.sig[0].y = y.max(pad);
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
    let (w, h) = (cfg.w, cfg.h);
    let pad = cfg.pad();
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

/// 竖版主文的版心：页头**下面**那一整片。
///
/// 页头在顶部（时间 / 日期 / 界栏）是这次设计的硬约束 —— 主文永远从页头下方
/// 起排，绝不可能压在时间上。C 栏头另有一条：署名占左侧一列，版心让开它。
fn quote_band_v(cfg: &Config, cb: &ClockBox) -> (i32, i32, i32, i32) {
    let (w, h) = (cfg.w, cfg.h);
    let pad = cfg.pad_v();
    let em = cfg.quote_font_px();

    let mut l = pad;
    // C 栏头：署名占正文左边一列，版心让开它（`||` 而不是嵌两层 `if`）。
    if cfg.v_style == VStyle::Head && cb.sig.iter().any(|c| c.is_author) {
        l = (pad + cb.sig_w + ((em as f32) * 0.55).round() as i32).min(w - pad - 1);
        l = l.max(pad);
    }
    let top = if cb.show {
        // 页头下方留一口气：有界栏时 0.95em，纯文字页头 1.15em（没有线帮忙分块，
        // 靠留白分）。
        let g = if cb.rule.is_some() { 0.95 } else { 1.15 };
        (cb.head_bottom + ((em as f32) * g).round() as i32).min(h - pad - 1)
    } else {
        pad
    };
    let top = top.max(pad);

    // 正文**吃满整块版心**，不给署名预留纵向空间。
    //
    // 试过留：版心先减掉「署名列高 + 间距」再交给排版，署名就一定有地方落。
    // 但那是拿正文换署名 —— 300×660@200% 下署名三字要 160px，正文只剩
    // 三分之二，十二个字被迫挤成「待/到/重」这种三字一列，断句全碎。
    // 宁可让正文满版、署名改用**横向让位**（见 `place_sign`：下方放不下
    // 就挪到正文块左侧），两边都不受委屈。
    (l, top, w - pad, h - pad)
}

/// 版心是不是退化到了「只能整窗画」这一步（横版）。
///
/// 退化不是 bug，是**窗口真的装不下**了：时钟/日期块已经把可用的排版空间吃光。
/// 但用户不该对着一片叠在一起的字猜哪儿出问题 —— 调用方据此给一句提示。
pub(crate) fn band_degenerate(cfg: &Config, cb: &ClockBox) -> bool {
    if !cb.show || cfg.orient == Orient::Vertical {
        return false;
    }
    let (w, h) = (cfg.w, cfg.h);
    let pad = cfg.pad();
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
    // 古籍正文从不夹署名 —— 出处归落款（仿古排版，2026-10-15 老板拍板）。
    // 横版维持 display()（跑马灯一行带出处）。
    let q = &a.list[a.qi];
    let (text, attr_from) = if a.cfg.orient == Orient::Vertical {
        (q.text.clone(), None)
    } else {
        // 横版：出处（「——　作者」）独立成行右对齐（审查 U3）。
        // display() = 正文 + 「　——　」 + 作者，分界跳过那个全角空格，
        // 让出处行从「——」起画。无出处时 None = 整段正文。
        let af = if q.author.is_empty() {
            None
        } else {
            Some(q.text.chars().count() + 1)
        };
        (q.display(), af)
    };
    let mut ql = layout_quote(&a.cfg, &a.fonts, a.frame.dc(), band, &text, attr_from);

    // ---- 竖版：把页头占掉的那半高**还给正文**
    //
    // 版心从页头下方起算，正文在版心里竖直居中 ⇒ 整块会系统性偏下半个页头高，
    // 短句时下面一大片空、上面贴着日期，观感像「排版掉了下去」。
    // 这里把整块上提 `(页头高)/2`，但**留有上限**：
    //   * 上提不超过竖直余量的一半（否则会顶到页头）；
    //   * 上提后顶端绝不高于版心上边界。
    // 只是整体平移，不重排 —— 列切分仍然按真实可用的版心算，不会被挤出窗口。
    if a.cfg.orient == Orient::Vertical && !ql.is_empty() {
        let pad = a.cfg.pad_v();
        let slack = (band.3 - band.1) - ql.extent.1;
        let lift = ((band.1 - pad) / 2).min(slack / 2).max(0);
        if lift > 0 {
            layout::shift_by(&mut ql, 0, -lift);
        }
    }
    ql
}

/// 排一次格言，走「缩字阶梯」。
///
/// 候选档 100% → 85% → 70%（老板定的下限）。**每档都换字体对象重新量**，
/// 不按比例缩 `adv`：不同字号的 GDI 取整不一样，估算出来的宽度和实际画出来的对不上。
///
/// 返回的定位表在**同一帧的三趟绘制里共用** —— 排版只算一次，这是本项目
/// 最贵的教训（`band_l` 那段注释就是它的墓碑）。
/// 跑马灯排版用的「版心」上界：跑马灯**不折行**，版心宽高都取一个远超
/// 任何句子的值，排完再整体平移到原点（`layout::content_origin`）。
const MARQUEE_BAND: i32 = 1 << 20;

/// 跑马灯里的一句：要么是一张**定位表**（含公式，能画出分式 / 根号），
/// 要么是原来那串 UTF-16（纯文字，走老路径）。
struct MarqueeItem {
    ql: Option<QuoteLayout>,
    s: Vec<u16>,
    /// 整句宽（含公式块）—— 下一句的起点靠它算
    w: i32,
    /// **汉字行高**（不是内容外框高）—— 竖直居中靠它算
    h: i32,
    /// 内容顶到汉字行顶的距离。含分式的句子，公式会向上伸出去，
    /// 这段留白必须扣掉，否则汉字会被公式顶着往上跑。
    pad_top: i32,
}

/// 把一句话备成跑马灯条目。
///
/// 纯文字**不进** `layout_rich`：那条路要按字号现量每个字，代价比
/// `measure_text` 高一两个数量级，而它产出的定位表在「整串平移」这件事上
/// 并不比 `TextOutW` 多任何信息。
fn marquee_item(cfg: &Config, fonts: &Fonts, dc: HDC, text: &str) -> MarqueeItem {
    if crate::quotes::has_math(text) {
        let (segs, _) = crate::quotes::segments(text);
        unsafe { SelectObject(dc, fonts.quote as HGDIOBJ) };
        let (cw, ch) = gfx::measure_text(dc, &wide("国"));
        let m = Metrics::new(cw.max(1), ch.max(1));
        let mut gm = crate::tex::GdiMeasure::borrowed(dc, fonts);
        let mut ql = layout::layout_rich(
            &segs,
            fonts.px_at(0),
            m,
            Orient::Horizontal,
            (0, 0, MARQUEE_BAND, MARQUEE_BAND),
            cfg.quote_spacing(),
            &mut gm,
        );
        // 排版给的是「版心内」坐标（横版每行还会在版心里居中），滚动只要
        // 「这一句自己从 0 开始」，所以把内容左上角平移到原点。
        let (qx, qy) = layout::content_origin(&ql);
        layout::shift_by(&mut ql, -qx, -qy);
        // 居中基准取**汉字行**，不取内容外框。
        //
        // 分式是有深度的：`T = 2π√(L/g)` 的分母 `g` 落在基线下方一截，
        // 外框因此比一行字高出一倍。拿外框居中，整句汉字就被顶到窗口上半，
        // 屏幕上就是「有的句子上下不居中」—— 而纯文字句外框≈字行，看着正常，
        // 于是同一屏里两种竖直位置来回跳。
        // 取字行之后，所有句子的汉字都落在同一条水平线上，公式按自身基线
        // 该伸多高就伸多高。
        let (mut gt, mut gb) = (i32::MAX, i32::MIN);
        for g in &ql.glyphs {
            gt = gt.min(g.y);
            gb = gb.max(g.y + m.char_h);
        }
        let (pad_top, hh) = if gt <= gb {
            (gt, gb - gt)
        } else {
            // 整句都是公式、一个字都没有：退回去用外框居中也比贴顶强
            (0, ql.extent.1)
        };
        let w = ql.extent.0;
        return MarqueeItem {
            ql: Some(ql),
            s: Vec::new(),
            w: w.max(1),
            h: hh.max(1),
            pad_top,
        };
    }
    let s = wide(text);
    unsafe { SelectObject(dc, fonts.quote as HGDIOBJ) };
    let (tw, th) = gfx::measure_text(dc, &s);
    MarqueeItem {
        ql: None,
        s,
        w: tw.max(1),
        h: th.max(1),
        pad_top: 0,
    }
}

/// 画跑马灯里的一句。`pos` 是这句的**左上角**在窗口里的位置（已含阴影位移
/// `ox` / `oy`），竖直居中的算法在调用处 —— 那里才知道窗口高。
fn draw_marquee_item(
    dc: HDC,
    cfg: &Config,
    fonts: &Fonts,
    it: &MarqueeItem,
    pos: (i32, i32),
    color: u32,
    no_batch: Option<bool>,
) {
    let (x, dy) = pos;
    match &it.ql {
        Some(ql) => {
            gfx::draw_glyphs(
                dc,
                fonts.quote,
                fonts.quote_rot_at(0),
                ql,
                x,
                dy,
                color,
                cfg.quote_spacing(),
                None,
                no_batch,
            );
            // 公式块与正文**同偏移、同颜色**：分式横线 / 根号上横线不是字符，
            // 得单独画；跟着一起挪，否则阴影趟里字有影子、横线留在原地。
            if !ql.math.is_empty() {
                let mut gm = crate::tex::GdiMeasure::borrowed(dc, fonts);
                for mb in &ql.math {
                    crate::tex::draw(dc, &mb.mb, x + mb.x, dy + mb.y, color, &mut gm);
                }
            }
        }
        None => {
            unsafe { SelectObject(dc, fonts.quote as HGDIOBJ) };
            unsafe { SetTextCharacterExtra(dc, cfg.quote_spacing()) };
            gfx::draw_text(dc, &it.s, x, dy, color);
            unsafe { SetTextCharacterExtra(dc, 0) };
        }
    }
}

fn layout_quote(
    cfg: &Config,
    fonts: &Fonts,
    dc: HDC,
    band: (i32, i32, i32, i32),
    text: &str,
    attr_from: Option<usize>,
) -> QuoteLayout {
    let s = wide(text);
    // 排版单位是 Unicode 标量值，不是 UTF-16 code unit：代理对必须留在同一个
    // 字形里，否则竖排会把一个 emoji / 生僻字拆成两个无效半字。
    let chars: Vec<char> = text.chars().collect();
    let spacing = cfg.quote_spacing();
    // 竖版的横向摆法：C 栏头走古法「右起贴右缘」，A / B 按墨迹居中。
    // 横版一律 `Right` —— 横版排版自己会在版心里逐行居中，与它无关；
    // 给 `Center` 只是让语义含糊，不出错但也没道理。
    let align = if cfg.orient == Orient::Vertical && cfg.v_style != VStyle::Head {
        layout::Align::Center
    } else {
        layout::Align::Right
    };
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

    // ---- 含公式的句子走另一条排版路（`layout::layout_rich`）
    //
    // 分岔判据只有「正文里有没有 `$`」这一条：**不含公式的句子一个字节都
    // 不走这里**，逐字逐位与旧实现相同。公式排版要按字号现量每个字
    // （含上标、分式那几档），拿不到渲染层预先量好的 `adv` 数组，所以只能
    // 分开走；但两条路的产物是同一个 `QuoteLayout`，下游（合批 / 阴影 /
    // 分带 / 动效）不需要知道有两条。
    if crate::quotes::has_math(text) {
        let (segs, _unmatched) = crate::quotes::segments(text);
        // 度量器**借用 `Fonts` 的字体缓存**：每帧新建一个，但字体不重建，
        // GDI 句柄数才是平的（见 `gfx::Fonts::math_font` 的注释）。
        let mut gm = crate::tex::GdiMeasure::borrowed(dc, fonts);
        for (k, &pct) in QUOTE_SHRINK_STEPS.iter().enumerate().take(steps) {
            unsafe { SelectObject(dc, fonts.quote_at(k) as HGDIOBJ) };
            let (cw, ch) = gfx::measure_text(dc, &wide("国"));
            let m = Metrics::new(cw.max(1), ch.max(1));
            let mut ql = layout::layout_rich_ex(
                &segs,
                fonts.px_at(k),
                m,
                cfg.orient,
                band,
                spacing,
                &mut gm,
                align,
            );
            ql.shrink = pct;
            if !ql.overflow {
                return ql;
            }
            last = ql;
        }
        return last;
    }
    for (k, &pct) in QUOTE_SHRINK_STEPS.iter().enumerate().take(steps) {
        let font = fonts.quote_at(k);
        unsafe { SelectObject(dc, font as HGDIOBJ) };
        let Some(raw_adv) = gfx::char_advances(dc, &s) else {
            break;
        };
        // GDI 的累计宽度按 UTF-16 单元给出，把它聚合成每个标量值的步进。
        let mut adv = Vec::with_capacity(chars.len());
        let mut u = 0usize;
        for c in &chars {
            let n = c.len_utf16();
            let end = (u + n).min(raw_adv.len());
            let sum: i32 = raw_adv[u.min(end)..end].iter().sum();
            adv.push(sum.max(0));
            u = end;
        }
        let (cw, ch) = gfx::measure_text(dc, &wide("国"));
        let m = Metrics::new(cw.max(1), ch.max(1));
        let mut ql = layout::layout_ex(&chars, &adv, m, cfg.orient, band, spacing, attr_from, align);
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
    let scale = cfg.eff_scale();
    let (w, h) = (cfg.w, cfg.h);
    let pad = cfg.pad();

    // ---- 时钟（横版：左上角的时间块；竖版：顶部的横排时间）
    //
    // 竖版的时间**不再逐字竖排**：`15:40:36` 拆成八格会被读成八拍，
    // 时钟是静帧锚点，必须一眼可读（2026-11-04 竖排改版）。
    if cfg.orient == Orient::Vertical {
        if let Some(s) = &cb.time {
            let face = if cb.clock_face.is_null() {
                rc.fonts.clock_small
            } else {
                cb.clock_face
            };
            unsafe { SelectObject(dc, face as HGDIOBJ) };
            gfx::draw_text(
                dc,
                s,
                cb.time_at.0 + ox,
                cb.time_at.1 + oy,
                pass.paint_color(gfx::CH_CLOCK),
            );
        }
        // 界栏：一条 1px 的横线，把页头和正文分开（A / C 有，B 没有）。
        // 与横版的竖分隔线同理 —— 它是版式装饰，不带阴影。
        if let Some((ry, x0, x1)) = cb.rule {
            if !shadow_pass {
                let line = RECT {
                    left: x0,
                    top: ry,
                    right: x1,
                    bottom: ry + 1,
                };
                unsafe { FillRect(dc, &line, rc.rule_brush) };
            }
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
        // 版心**只从 `frame_band` 取**：这里以前自己内联了一遍同样的 match，
        // 与排版时用的那份是两处定义 —— 加第三种朝向必漏一处。
        // 这里只要左右边界：竖直方向跑马灯按整个窗口裁（见下方注释）。
        let (bl, _, br, _) = frame_band(cfg, cb);
        // 版心退化 = 时钟/日期块已经把窗口吃光。平时**不能**把格言压在时钟上：
        // 两块都会变得不可读，观感比「格言没了」更差，所以只画一句提示。
        //
        // **布局模式下例外**：正在调尺寸的人全靠这句字判断「现在够不够放」，
        // 这时候把它藏起来，观感就是「一拖就没了」（老板原话：调整时文字先看不见）。
        // 提示照旧画，但字必须留着 —— 难看两秒钟 << 不知道自己在调什么。
        let degenerate = band_degenerate(cfg, cb);
        let hide_quote = degenerate && !cfg.layout_mode;
        if hide_quote {
            // 有意什么都不画 —— 提示在下面统一画
        } else if matches!(rc.anim, Anim::Marquee) {
            // 跑马灯：整串滚动。
            //
            // **含公式的句子照样滚**。这里以前一律 `draw_text(display())` ——
            // 画的是原文，公式会原样滚出一串 `$\frac{a}{b}$`。先前的补救是
            // 「公式句不滚，降级成静态」，结果是满库公式时挂件一动不动。
            // 现在按句分两条画法：
            //   * 纯文字 —— 整串 `TextOutW`（老路径，一个字节没改）；
            //   * 含公式 —— 排成定位表，按横向偏移画，宽取 `extent`。
            //
            // 定位表每帧重排一次（滚动偏移是绘制期的 `x`，不参与排版），
            // 两句一起约 0.4ms，相对 40ms 的滚动帧不到 1%。
            let gap = cfg.marquee_gap();
            let color = pass.paint_color(gfx::CH_QUOTE);
            let it1 = marquee_item(cfg, rc.fonts, dc, &rc.list[rc.qi].display());
            let it2 = marquee_item(cfg, rc.fonts, dc, &rc.list[rc.qn].display());
            let x1 = (br as f32 - rc.m).round() as i32;
            let x2 = x1 + it1.w + gap;
            // 裁剪区只裁**横向**（滚动要裁掉窗口外的部分），竖直方向给整个
            // 窗口高。以前竖直用的是版心（`bt..bb`），公式句一进来就出问题：
            // 含分式的句子比版心高，分母 / 根号下沿正好落在版心外，被整条切掉
            // —— 屏幕上看到的是「字只剩上半截」，像被压扁。
            // `oy` 仍要跟着走：阴影趟里内容整体下移，裁剪区同移，两趟裁掉
            // 的才是同一块。
            gfx::with_clip(dc, bl, 0, br, h, |dc| {
                // 竖直：汉字行居中（`pad_top` 扣掉公式向上伸出的那截），
                // 不含公式的句子 `pad_top` 是 0，与旧实现逐像素相同。
                let y1 = (h - it1.h) / 2 - it1.pad_top + oy;
                let y2 = (h - it2.h) / 2 - it2.pad_top + oy;
                draw_marquee_item(dc, cfg, rc.fonts, &it1, (x1 + ox, y1), color, rc.no_batch);
                draw_marquee_item(dc, cfg, rc.fonts, &it2, (x2 + ox, y2), color, rc.no_batch);
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
            // 旋转字必须跟着**同一档**字号走：竖版缩字回退之后正缩到 85%/70%，
            // 破折号还按 100% 画就会大出一圈，把上下两个字盖住。
            let rot = rc.fonts.quote_rot_at(ql.font_tier());
            let spacing = cfg.quote_spacing();
            // 滑入偏移**只加在格言上**：时钟/日期/底板是静帧，不跟着漂。
            // 偏移量由 `transition` 统一给，两趟同值（几何一致铁律）。
            // 静态内容**不加裁剪区**（「PR3·竖版最右列完整可见」钉着这条：
            // 裁一刀会把竖版右列墨迹越出步进框的那 1~2px 切掉）。
            gfx::draw_glyphs(dc, font, rot, ql, ox + rc.slide, oy, color, spacing, groups, rc.no_batch);

            // 公式块：整块交给 `tex`，**同一趟、同一个偏移、同一个颜色**。
            //
            // 位置（含居中 / 折行 / 旋转）在排版层就定死了，这里只加两趟
            // 共用的位移 `(ox + slide, oy)` —— 阴影趟的 `oy` 就是那个
            // 「往右下挪一点再画一遍」，公式必须跟着挪，否则文字有影子、
            // 公式没有，像贴上去的。
            if !ql.math.is_empty() {
                let fonts: &gfx::Fonts = rc.fonts;
                let mut gm = crate::tex::GdiMeasure::borrowed(dc, fonts);
                for mb in &ql.math {
                    if mb.rot {
                        crate::tex::draw_rot(dc, &mb.mb, mb.x + ox + rc.slide, mb.y + oy, color, &mut gm);
                    } else {
                        crate::tex::draw(dc, &mb.mb, mb.x + ox + rc.slide, mb.y + oy, color, &mut gm);
                    }
                }
            }
        }

        // 那句提示必须说出来 —— 让用户对着一片叠着的字自己猜，
        // 正是这次要根除的毛病（旧实现是格言整条消失、零提示）。
        //
        // 提示用**固定 13px** 的字体：这是一句给用户看的话，不该跟着 scale
        // 一起放大到装不下（300% 下 `hint` 是 36px，整句要 600 多 px）。
        // 字体对象随 `Fonts` 建一次就够 —— 旧实现在这里每帧 `make_font` +
        // `DeleteObject`，一句退化提示付一次建字体的代价。
        if degenerate && !shadow_pass {
            unsafe { SelectObject(dc, rc.fonts.hint_fixed as HGDIOBJ) };
            let hint = wide("窗口装不下 · 请放大窗口或降低字号");
            let (hw, _) = gfx::measure_text(dc, &hint);
            // 提示要画在**主色通道**上、而且给满值：掩码强度就是那个字节的值，
            // 给个 0xC0 之类「看起来够亮」的颜色，提取出来 alpha 只有 75%，
            // 叠到棋盘格上只差几级灰度 —— 画了等于没画。
            // 也不能顺手写成 `WHITE`：那是三条通道一起点亮，
            // 在「通道即语义」的新布局里，这一片像素会同时归属三种颜色。
            gfx::draw_text(dc, &hint, (w - hw).max(0) / 2, 2, gfx::CH_CLOCK);
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
    if !cb.show && cb.sig.is_empty() {
        return;
    }
    let (ox, oy) = pass.offset(rc.cfg);

    if rc.cfg.orient == Orient::Vertical {
        unsafe { SelectObject(dc, rc.fonts.date as HGDIOBJ) };
        // 页头的日期：横排、日期色。它是静帧，换句不跟着淡。
        if let Some(s) = &cb.date {
            unsafe { SelectObject(dc, rc.fonts.date_v as HGDIOBJ) };
            gfx::draw_text(
                dc,
                s,
                cb.date_at.0 + ox,
                cb.date_at.1 + oy,
                pass.paint_color(gfx::CH_DATE),
            );
        }
        // 署名（出处）：竖排一列，小字，换句过渡时**随格言一起淡**。
        unsafe { SelectObject(dc, rc.fonts.date_v as HGDIOBJ) };
        for c in cb.sig.iter() {
            let color = if pass.is_shadow() && c.is_author && rc.author_gray != 0 {
                rc.author_gray
            } else {
                pass.paint_color(gfx::CH_DATE)
            };
            gfx::draw_vtext(dc, &c.s, c.x + ox, c.y + oy, cb.sig_cell, color);
        }
        return;
    }

    let Some(s) = &cb.date else { return };
    unsafe { SelectObject(dc, rc.fonts.date as HGDIOBJ) };
    let y = cb.y + (if cb.time.is_some() { cb.time_h + cb.gap } else { 0 });
    gfx::draw_text(dc, s, cb.x + ox, y + oy, pass.paint_color(gfx::CH_DATE));
}
