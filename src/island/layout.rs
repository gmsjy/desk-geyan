//! 灵动岛的两套排版。**纯函数** —— 文字度量由调用方以闭包传入，
//! 所以单测可以用假度量跑全部边界，不需要真 DC。
//!
//! 与格言的 `layout.rs` 没有任何共享：那边是「三段式版心、缩字阶梯、竖版列、
//! 跑马灯带」，这里只有「一行片段」和「三行面板」。硬要合并的话，每个新
//! 分支都要在两趟绘制里保持一致，分支数是相乘而不是相加（规格书 §1.4）。

use super::content::{Items, Samples, Seg};
use super::geom::{
    COMPACT_GAP, COMPACT_H, COMPACT_MIN_W, COMPACT_PAD_X, EXPANDED_H, EXPANDED_W,
};

/// 字号基准（逻辑像素）。岛的字号不跟格言的 `scale` 走 —— 两个功能各自
/// 独立，用户把格言调到 300% 不该让胶囊里的时间变成巨字。
const COMPACT_PX: f32 = 13.0;
const DOT_D: f32 = 6.0;
const EXPAND_PAD: f32 = 16.0;
const EXPAND_CLOCK_PX: f32 = 32.0;
const EXPAND_TEXT_PX: f32 = 13.0;
const EXPAND_CLOCK_Y: f32 = 12.0;
const EXPAND_DATE_Y: f32 = 24.0;
const EXPAND_LINE_Y: f32 = 56.0;
const EXPAND_METRIC_Y: f32 = 68.0;
const EXPAND_MSG_Y: f32 = 96.0;

/// 字形落在哪条掩码通道上。绘图层再翻成 `gfx::CH_*`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Chan {
    /// 主色（时钟 / 分隔线）
    Main,
    /// 次色（日期 / 指标）
    Dim,
    /// 强调色（消息行）
    Accent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub x: i32,
    pub y: i32,
    pub px: i32,
    pub ch: Chan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dot {
    pub x: i32,
    pub y: i32,
    pub d: i32,
}

/// 一条分隔线：从 `(x0, y)` 到 `(x1, y)`，写主色通道。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    pub x0: i32,
    pub x1: i32,
    pub y: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub runs: Vec<Run>,
    pub dots: Vec<Dot>,
    pub lines: Vec<Line>,
    /// 这一套内容需要的窗口宽（收起态由内容算出，展开态就是目标宽）
    pub width: i32,
    /// 布局所属的坐标系高度（绘制端据此垂直对齐）
    pub height: i32,
}

/// 文字度量：`(宽, 字格高)`。绘制端用 `GetTextExtentPoint32W`，
/// 单测用解析式假实现。
pub type Measure<'a> = &'a dyn Fn(&str, i32) -> (i32, i32);

pub(crate) fn sp(v: f32, k: f32) -> i32 {
    (v * k).round() as i32
}

/// 按可用宽度截断，末尾加省略号。中文按 `char` 切，不会切出半个字。
///
/// 逐字符累加而不是「按平均字宽估一个数」：中英混排（`CPU 12%`、`MEM 63%`）
/// 的平均字宽差异极大，估出来的结果要么留一大截空白，要么把最后一个字切掉。
pub fn truncate_to(text: &str, max_w: i32, px: i32, measure: Measure) -> String {
    if max_w <= 0 {
        return String::new();
    }
    if measure(text, px).0 <= max_w {
        return text.to_string();
    }
    let ell = "…";
    let ell_w = measure(ell, px).0;
    let budget = (max_w - ell_w).max(0);
    let mut out = String::new();
    let mut w = 0;
    for ch in text.chars() {
        let mut probe = out.clone();
        probe.push(ch);
        let cw = measure(&probe, px).0 - w;
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push_str(ell);
    out
}

/// 收起态：一排片段，整组居中在胶囊里（只要第一页）。
///
/// **绘制端走的是 `compact_page`**，这个包装只有单测在用 —— 单测关心「排版对不对」，
/// 不关心页号，多传一个 0 只是噪音。
#[allow(dead_code)]
pub fn compact(segs: &[Seg], measure: Measure, k: f32, max_w: i32) -> Layout {
    compact_page(segs, measure, k, max_w, 0).0
}

/// 收起态排版：返回 `(这一页的布局, 总页数)`。
///
/// # 为什么窗口宽取「所有页里最宽的那页」
///
/// 每页内容长度不同。若按当前页算宽，切页时胶囊会长短跳一下 —— 而宽度变化
/// 不是免费的：`mod.rs` 每拍 `recompute` 都会读 `compact_w`，一跳就会触发
/// ULW 改窗口尺寸，观感上是「胶囊在抽动」。取最宽页之后，页与页之间只换内容、
/// 不动几何。代价是内容少的页两侧留白多一点（本来就居中，看起来是呼吸感）。
pub fn compact_page(
    segs: &[Seg],
    measure: Measure,
    k: f32,
    max_w: i32,
    page: usize,
) -> (Layout, usize) {
    let pad = sp(COMPACT_PAD_X as f32, k).max(2);
    let gap = sp(COMPACT_GAP as f32, k).max(1);
    let px = sp(COMPACT_PX, k).max(6);
    let dot_d = sp(DOT_D, k).max(2);
    let h = sp(COMPACT_H as f32, k).max(8);
    let max_w = max_w.max(sp(COMPACT_MIN_W as f32, k));

    let avail = (max_w - 2 * pad).max(1);
    // 单项就超过可用宽度时**先截断**：`pack_pages` 能让它独占一页，
    // 但不会让它变窄 —— 不截断就会画到窗户外。
    let texts: Vec<String> = segs
        .iter()
        .map(|s| {
            if s.dot {
                String::new()
            } else {
                truncate_to(&s.text, avail, px, measure)
            }
        })
        .collect();
    // 每一项的**占位宽度**：数值型片段取 `max(实际, 基准串)`（见 `Seg::widest`），
    // 于是同一格在不同采样值下宽度不变 —— 胶囊不会随 CPU 读数抽动。
    // 基准串本身也要夹在 `avail` 以内，否则它会把整页撑出窗外。
    let widths: Vec<i32> = segs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if s.dot {
                return dot_d;
            }
            let w = measure(&texts[i], px).0;
            match s.widest {
                Some(base) => w.max(measure(base, px).0.min(avail)),
                None => w,
            }
        })
        .collect();
    let pages = super::geom::pack_pages(&widths, gap, avail);
    let count = pages.len().max(1);
    let total_of = |list: &[usize]| -> i32 {
        let mut t = 0;
        for (i, idx) in list.iter().enumerate() {
            if i > 0 {
                t += gap;
            }
            t += widths[*idx];
        }
        t
    };
    let widest = pages.iter().map(|p| total_of(p)).max().unwrap_or(0);
    let width = (widest + 2 * pad).clamp(sp(COMPACT_MIN_W as f32, k), max_w);

    let list = pages.get(page % count).cloned().unwrap_or_default();
    let total = total_of(&list);

    // 内容整体居中：窗口比内容宽时（被 MIN_W 或最宽页撑大）两边留白相等
    let mut x = ((width - total) / 2).max(pad);
    let mut out = Layout {
        runs: Vec::new(),
        dots: Vec::new(),
        lines: Vec::new(),
        width,
        height: h,
    };
    for (i, idx) in list.iter().enumerate() {
        if i > 0 {
            x += gap;
        }
        let seg = &segs[*idx];
        if seg.dot {
            out.dots.push(Dot {
                x,
                y: (h - dot_d) / 2,
                d: dot_d,
            });
        } else {
            let (_, th) = measure(&texts[*idx], px);
            out.runs.push(Run {
                text: texts[*idx].clone(),
                x,
                y: (h - th) / 2,
                px,
                ch: if seg.main { Chan::Main } else { Chan::Dim },
            });
        }
        x += widths[*idx];
    }
    (out, count)
}

/// 展开态：三行面板。
///
/// 行 1 左时钟右日期（右对齐），行 2 三个指标等宽三等分，行 3 是最近一条消息
/// （没有就整行不画）。**没有跑马灯** —— 超宽就截断加省略号，滚动是格言的机制。
pub fn expanded(s: &Samples, items: &Items, measure: Measure, k: f32, w: i32, h: i32) -> Layout {
    let w = if w > 0 { w } else { sp(EXPANDED_W as f32, k) };
    let h = if h > 0 { h } else { sp(EXPANDED_H as f32, k) };
    let pad = sp(EXPAND_PAD, k).max(2);
    let clock_px = sp(EXPAND_CLOCK_PX, k).max(8);
    let text_px = sp(EXPAND_TEXT_PX, k).max(6);
    let avail = (w - 2 * pad).max(1);
    // 用户把展开高调大时，内容块整体垂直居中，而不是底部留一片空白。
    // 默认高度（EXPANDED_H）时偏移恒 0 —— 默认外观逐像素不变。
    // 调小由 `EXPAND_MIN_H` 夹取兜住（config / on_menu / 面板置灰），这里不处理。
    let y_off = ((h - sp(EXPANDED_H as f32, k)) / 2).max(0);

    let mut out = Layout {
        runs: Vec::new(),
        dots: Vec::new(),
        lines: Vec::new(),
        width: w,
        height: h,
    };

    // ---- 行 1：时钟（左） + 日期（右，右对齐）
    if items.clock {
        out.runs.push(Run {
            text: s.clock.clone(),
            x: pad,
            y: sp(EXPAND_CLOCK_Y, k) + y_off,
            px: clock_px,
            ch: Chan::Main,
        });
    }
    let date_w = measure(&s.date, text_px).0;
    let clock_w = if items.clock {
        measure(&s.clock, clock_px).0
    } else {
        0
    };
    if clock_w + date_w + sp(8.0, k) <= avail {
        out.runs.push(Run {
            text: s.date.clone(),
            x: w - pad - date_w,
            y: sp(EXPAND_DATE_Y, k) + y_off,
            px: text_px,
            ch: Chan::Dim,
        });
    } else {
        // 日期放不下就截断（窄屏 + 大 DPI 时会遇到），但至少留着
        let avail_date = (avail - clock_w - sp(8.0, k)).max(sp(40.0, k));
        let t = truncate_to(&s.date, avail_date, text_px, measure);
        let tw = measure(&t, text_px).0;
        out.runs.push(Run {
            text: t,
            x: w - pad - tw,
            y: sp(EXPAND_DATE_Y, k) + y_off,
            px: text_px,
            ch: Chan::Dim,
        });
    }

    // ---- 分隔线
    out.lines.push(Line {
        x0: pad,
        x1: w - pad,
        y: sp(EXPAND_LINE_Y, k) + y_off,
    });

    // ---- 行 2：三个指标三等分。顺序固定 CPU / MEM / 电池，与 `island_items`
    // 无关 —— 展开态是「信息面板」，不是「收起态的放大版」。
    let col_w = avail / 3;
    let metrics: [Option<String>; 3] = [
        if items.cpu {
            s.cpu.map(|c| format!("CPU {c}%"))
        } else {
            None
        },
        if items.mem {
            s.mem.map(|m| format!("MEM {m}%"))
        } else {
            None
        },
        if items.battery {
            s.battery
                .map(|(p, c)| if c { format!("电池 {p}%+") } else { format!("电池 {p}%") })
        } else {
            None
        },
    ];
    for (i, m) in metrics.iter().enumerate() {
        if let Some(t) = m {
            let x = pad + col_w * i as i32;
            out.runs.push(Run {
                text: truncate_to(t, col_w - sp(6.0, k), text_px, measure),
                x,
                y: sp(EXPAND_METRIC_Y, k) + y_off,
                px: text_px,
                ch: Chan::Dim,
            });
        }
    }

    // ---- 行 3：最近一条消息
    if let Some(m) = super::content::msg_line(s) {
        out.runs.push(Run {
            text: truncate_to(&m, avail, text_px, measure),
            x: pad,
            y: sp(EXPAND_MSG_Y, k) + y_off,
            px: text_px,
            ch: Chan::Accent,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::island::content::Items;
    use crate::island::geom::COMPACT_MAX_W;

    /// 假度量：ASCII 半宽、其它全宽；字格高 = 1.25 × 字号。
    /// 这个比例接近真实中文字体（tmHeight ≈ 1.2~1.3 em）。
    fn fake(_s: &str, px: i32) -> (i32, i32) {
        let w = 0;
        let _ = w;
        (0, (px as f32 * 1.25).round() as i32)
    }

    fn measure(s: &str, px: i32) -> (i32, i32) {
        let mut w = 0;
        for ch in s.chars() {
            w += if ch.is_ascii() { px / 2 } else { px };
        }
        (w, fake(s, px).1)
    }

    fn segs(items: &str, clock: &str) -> Vec<Seg> {
        let s = Samples {
            clock: clock.into(),
            cpu: Some(12),
            mem: Some(63),
            battery: Some((84, false)),
            unread: 0,
            ..Default::default()
        };
        crate::island::content::segments(&s, &Items::parse(items), false, false)
    }

    #[test]
    fn compact_width_wraps_content_plus_padding() {
        let ss = segs("clock,cpu", "09:41");
        let k = 1.0;
        let l = compact(&ss, &measure, k, COMPACT_MAX_W);
        // 每项占的宽度是「实际文本」与「占位基准串」取大（`Seg::widest`），
        // 所以这里必须按同一条规则算 —— 直接量 text 会漏掉 CPU 那格按
        // `CPU 100%` 预留出来的余量。
        let slot = |s: &Seg| -> i32 {
            let w = measure(&s.text, sp(COMPACT_PX, k)).0;
            match s.widest {
                Some(base) => w.max(measure(base, sp(COMPACT_PX, k)).0),
                None => w,
            }
        };
        let inner: i32 = ss.iter().map(slot).sum::<i32>() + sp(COMPACT_GAP as f32, k);
        assert_eq!(l.width, (inner + 2 * sp(COMPACT_PAD_X as f32, k)).max(sp(COMPACT_MIN_W as f32, k)));
        assert_eq!(l.height, sp(COMPACT_H as f32, k));
        // 最后一项的右缘不超出窗口
        let last = l.runs.last().unwrap();
        assert!(
            last.x + measure(&last.text, last.px).0 <= l.width,
            "内容溢出胶囊"
        );
    }

    #[test]
    fn compact_shrinks_to_min_not_below() {
        // 只有一个很短的时钟，宽度被 MIN_W 兜住（否则胶囊会缩成一个点）
        let ss = segs("clock", "08:00");
        let l = compact(&ss, &measure, 1.0, COMPACT_MAX_W);
        assert!(l.width >= COMPACT_MIN_W, "宽度 {} 小于下限", l.width);
        // 并且内容在窗口里居中（左右留白差不超过 1px）
        let first = &l.runs[0];
        let right_blank = l.width - (first.x + measure(&first.text, first.px).0);
        assert!((first.x - right_blank).abs() <= 1, "没有居中");
    }

    #[test]
    fn compact_never_overflows_at_max_width() {
        // 内容总宽远超上限：宽度必须顶在上限附近，且每一项都还在窗内
        let wide = |n: usize| Seg {
            text: "字".repeat(n),
            main: false,
            dot: false,
            widest: None,
        };
        // 上限**用局部固定值**而不是线上的 `COMPACT_MAX_W`：这条验的是
        // 「截断与溢出保护」这套机制，不是「线上能装多少」。绑常量的话，
        // 上限一调（400 → 460）30 字就不再超宽，「单项超宽」的前提没了，
        // 测试会以「没顶满」失败 —— 而失败原因跟机制毫无关系。
        let max_w = 260;
        // 单项就超宽（30 字 = 390px > 可用宽）⇒ 截断后正好顶满上限
        let l = compact(&[wide(30)], &measure, 1.0, max_w);
        // 截断按整字取整，所以会比上限少最多一个字的宽度
        assert!(
            l.width <= max_w && l.width > max_w - 26,
            "单项超宽应当接近顶满上限，实际 {}",
            l.width
        );
        // 多项各自都不超宽、但加起来超 ⇒ 分页，宽度由内容定，绝不溢出
        let l = compact(&[wide(10), wide(10), wide(10)], &measure, 1.0, max_w);
        for r in &l.runs {
            assert!(
                r.x + measure(&r.text, r.px).0 <= l.width,
                "「{}」溢出：x={} w={} 窗口={}",
                r.text,
                r.x,
                measure(&r.text, r.px).0,
                l.width
            );
        }
    }

    #[test]
    fn dpi_scales_everything_proportionally() {
        let ss = segs("clock,cpu", "09:41");
        let a = compact(&ss, &measure, 1.0, COMPACT_MAX_W);
        let b = compact(&ss, &measure, 1.5, sp(COMPACT_MAX_W as f32, 1.5));
        // 高与字号同比；宽度受 MIN/MAX 夹取，只要求不小于 1.4 倍
        assert_eq!(b.height, (COMPACT_H as f32 * 1.5).round() as i32);
        assert!(b.width as f32 >= a.width as f32 * 1.4);
    }

    #[test]
    fn expanded_layout_has_clock_date_line_and_metrics() {
        let s = Samples {
            clock: "09:41:33".into(),
            date: "2026年11月23日 星期一".into(),
            cpu: Some(12),
            mem: Some(63),
            battery: Some((84, true)),
            msg: Some("构建完成".into()),
            unread: 1,
            ..Default::default()
        };
        let l = expanded(&s, &Items::parse("clock,cpu,mem,battery,msg"), &measure, 1.0, 420, 136);
        assert!(l.runs.iter().any(|r| r.text == "09:41:33"));
        assert!(l.runs.iter().any(|r| r.text.contains("2026年")));
        assert!(l.runs.iter().any(|r| r.text == "CPU 12%"));
        assert!(l.runs.iter().any(|r| r.text == "MEM 63%"));
        assert!(l.runs.iter().any(|r| r.text.contains("电池 84%")));
        assert!(l.runs.iter().any(|r| r.text == "构建完成"));
        assert_eq!(l.lines.len(), 1);
        // 所有字都在窗口内
        for r in &l.runs {
            let (tw, _) = measure(&r.text, r.px);
            assert!(r.x >= 0 && r.x + tw <= 420, "「{}」超出面板", r.text);
            assert!(r.y >= 0 && r.y + measure(&r.text, r.px).1 <= 136);
        }
    }

    #[test]
    fn expanded_omits_message_line_when_absent() {
        let s = Samples {
            clock: "09:41".into(),
            date: "2026年11月23日 星期一".into(),
            ..Default::default()
        };
        let l = expanded(&s, &Items::parse("clock,msg"), &measure, 1.0, 420, 136);
        // 没有消息 ⇒ 第三行一个 run 都没有
        assert!(!l.runs.iter().any(|r| r.ch == Chan::Accent));
        // 指标也没启用 ⇒ 只有时钟与日期
        assert_eq!(l.runs.len(), 2);
    }

    #[test]
    fn truncate_keeps_within_budget_and_marks_ellipsis() {
        let long = "这是一条非常长的消息正文，肯定放不下";
        let t = truncate_to(long, 100, 13, &measure);
        assert!(t.ends_with('…'), "截断后没有省略号：{t}");
        assert!(measure(&t, 13).0 <= 100, "截断后仍然超宽");
        assert!(t.chars().count() > 1, "截得太狠");
        // 放得下就原样返回
        assert_eq!(truncate_to("短", 100, 13, &measure), "短");
    }

    /// 三条通道的**分配**（规格书 §7.2 #5 的纯函数那一半）。
    ///
    /// #5 的完整命题是「时钟在 byte0、指标在 byte1、消息在 byte2」。
    /// 它由三段拼起来，每段各有人钉：
    ///
    /// 1. **这里**：排版给每条 run 贴 `Chan`（Main / Dim / Accent）；
    /// 2. `draw::chan_color`：`Chan` → 用哪个颜色参数；
    /// 3. `gfx::compose`：颜色参数 → 落到哪个字节。这一段由既有的
    ///    格言掩码断言直接读字节钉住（「通道值必须满值 FF」那几条），
    ///    而岛走的是同一个 `compose_ex`。
    ///
    /// 分开测是因为第 1 段能纯函数跑（不建窗口、不建字体），
    /// 而把它塞进像素断言里只会得到一条又慢又难定位的用例。
    #[test]
    fn channels_are_assigned_per_role() {
        let s = Samples {
            clock: "09:41:33".into(),
            date: "2026年11月23日 星期一".into(),
            cpu: Some(12),
            mem: Some(63),
            battery: Some((84, true)),
            msg: Some("构建完成".into()),
            unread: 2,
            ..Default::default()
        };
        let items = Items::parse("clock,dot,cpu,mem,battery,msg");
        let l = expanded(&s, &items, &measure, 1.0, 420, 136);
        let ch = |needle: &str| -> Option<Chan> {
            l.runs.iter().find(|r| r.text == needle).map(|r| r.ch)
        };
        // 时钟 = 主色；日期与三指标 = 次色；消息 = 强调色
        assert_eq!(ch("09:41:33"), Some(Chan::Main), "时钟不在主色通道");
        assert!(
            l.runs.iter().any(|r| r.text.contains("2026年") && r.ch == Chan::Dim),
            "日期不在次色通道"
        );
        for m in ["CPU 12%", "MEM 63%", "电池 84%+"] {
            assert_eq!(ch(m), Some(Chan::Dim), "指标「{m}」不在次色通道");
        }
        assert_eq!(ch("构建完成"), Some(Chan::Accent), "消息不在强调色通道");
        // 三条通道都真的有内容（否则上面几条可能全是「找不到 ⇒ None」）
        assert!(l.runs.len() >= 6);

        // 收起态：时钟主色，其余一律次色 —— 收起态没有强调色通道
        let ss = segs("clock,dot,cpu,mem,battery,msg", "09:41");
        let c = compact(&ss, &measure, 1.0, COMPACT_MAX_W);
        assert!(c.runs.iter().any(|r| r.px == sp(COMPACT_PX, 1.0)));
        assert_eq!(c.runs[0].ch, Chan::Main);
        assert!(
            c.runs.iter().skip(1).all(|r| r.ch == Chan::Dim),
            "收起态出现了非次色的后续片段"
        );
    }

    /// 收起态分页：宽度取**最宽页**，于是切页时几何不动。
    ///
    /// 这条同时钉住两件事：页数算得对（不拆片段）、每一页都不溢出。
    #[test]
    fn compact_pages_share_one_width_and_never_overflow() {
        let wide = |t: &str| Seg {
            text: t.to_string(),
            main: false,
            dot: false,
            widest: None,
        };
        // 上限**故意用一个小值**（260）而不是线上的 `COMPACT_MAX_W`：
        // 这条测的是「分页机制」，不是「线上能装多少」。绑到常量上的话，
        // 每次调上限（260 → 400）都会让夹具失去分页前提，测试就从
        // 「验分页」退化成「验一页装得下」，还偏偏是绿的。
        let max_w = 260;
        // 8 个汉字 = 104px（假度量全宽 13px）；可用宽 = 260 - 2*14 = 232
        // ⇒ 104+8+104 = 216 装得下两项，第三项单独一页
        let ss = [wide("一二三四五六七八"), wide("ABCDEFGH一二三四"), wide("九十")];
        let (p0, count) = compact_page(&ss, &measure, 1.0, max_w, 0);
        let (p1, count1) = compact_page(&ss, &measure, 1.0, max_w, 1);
        assert_eq!(count, 2, "分页数不对");
        assert_eq!(count1, 2, "页数与请求的页无关");
        assert_eq!(p0.runs.len(), 2, "第一页应装两项");
        assert_eq!(p1.runs.len(), 1, "第二页应只剩一项");
        assert_eq!(p0.width, p1.width, "两页宽度不一致 ⇒ 切页时胶囊会长短跳");
        // 页号超界按取模回绕（轮播与手改配置都可能给出越界值）
        let (wrap, _) = compact_page(&ss, &measure, 1.0, max_w, 2);
        assert_eq!(wrap, p0, "越界页号没有回绕到第一页");
        for l in [&p0, &p1] {
            for r in &l.runs {
                assert!(
                    r.x >= 0 && r.x + measure(&r.text, r.px).0 <= l.width,
                    "「{}」溢出：x={} 宽={} 窗口={}",
                    r.text,
                    r.x,
                    measure(&r.text, r.px).0,
                    l.width
                );
            }
        }
    }

    #[test]
    fn items_order_is_respected_in_compact() {
        let a = compact(&segs("clock,cpu", "09:41"), &measure, 1.0, COMPACT_MAX_W);
        let b = compact(
            &crate::island::content::segments(
                &Samples {
                    clock: "09:41".into(),
                    cpu: Some(12),
                    ..Default::default()
                },
                &Items::parse("cpu,clock"),
                false,
                false,
            ),
            &measure,
            1.0,
            COMPACT_MAX_W,
        );
        assert_eq!(a.runs[0].text, "09:41");
        assert_eq!(b.runs[0].text, "CPU 12%");
    }
}
