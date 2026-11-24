//! 排版引擎：把「每字步进」变成「每个字画在哪」。
//!
//! # 为什么单独一层
//!
//! 这一层出现之前，渲染管线里**只有一条主轴**（x），格言永远是「居中画一行、
//! 放不下就被裁剪区砍断」。后果是两个同源的缺陷：
//!   * **长句静默截断** —— 墨迹右缘紧贴裁剪缘，后半句和出处直接消失，无省略号；
//!   * **大字号整条消失** —— 版心宽度被时钟块吃光后，`band_r > band_l + 20`
//!     这条守卫直接跳过整段绘制，屏幕上什么都不剩。
//!
//! 两者都不是「少个功能」，而是**没有排版层**。
//!
//! # 这一层是纯函数
//!
//! `layout()` 只吃「预先量好的每字步进」和度量，**不碰 GDI、不建窗口**。
//! 于是折行、避头尾、竖排列切分、截断兜底全部可以用 `cargo test` 验 ——
//! 这是把它单独拆出来的最大理由（见文件末尾的测试）。
//!
//! # 坐标约定
//!
//! `Glyph.x/y` 是**客户区绝对坐标**，就是 `TextOutW` 该用的那两个数。
//! 版心 `band` 也是客户区坐标。**居中/右对齐在 `layout()` 里一次算完**，
//! 绘制方只负责「照着表画」—— 位置绝不能留到绘制趟里去算，
//! 那是「三趟各算一遍」的老坑（见 README 不变量 1）。

use crate::config::Orient;
use crate::quotes::Seg;
use crate::tex::{MBox, Measure};

// ---------------------------------------------------------------- 数据结构

/// 一个字符画在哪。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Glyph {
    /// Unicode 标量值，而不是 UTF-16 code unit。
    ///
    /// 用 `u16` 会把非 BMP 字符（emoji、部分生僻字）的代理对拆成两个字形，
    /// 竖排时各自 `TextOutW` 一次，结果两个都是无效半字。
    pub ch: char,
    pub x: i32,
    pub y: i32,
    /// 需要旋转 90°（竖版下的 `——` 与 `……`）。阶段四才会真正用到。
    pub rot: bool,
    /// 这个字形来自 `chars` 的第几个位置（补出来的「…」是 `u32::MAX`）。
    ///
    /// **存在的唯一理由：合批绘制。** 逐字一次 `TextOutW` 太贵 ——
    /// `--perf` 实测横版 66 字长句每帧要多花约 2000µs（≈37µs/字，主趟+阴影趟）。
    ///
    /// ⚠️ **实测订正（2026-10-15）**：原注释称「折行会把**不相邻**的两个字排到
    /// 同一行（`cur.pop()` 那个回溯就是现场），只有 `src` 能识别这种情况」——
    /// **该场景不存在**。两条排版路径的「行 / 列内下标」**恒连续**：`cur` 始终是
    /// 一个连续区间，行尾禁则的 `pop()` 只从尾部取走一个字、下一轮立刻又在下一行
    /// 行首 `push` 回来，竖版的 `split_off()` 同样保序 ⇒ 该判据当前与 `y` 判据
    /// **冗余**（230 万例穷举 + 随机扫描，「同 `y` 且 `src` 不连续」命中 **0** 次；
    /// 注毒对照也只有连 `y` 一起删才会红）。
    ///
    /// 真正需要守的是「合批不得跨列」（见 `runs()`）：竖版版心极矮时 `cap == 1`，
    /// 每列一字，相邻**列**的字同 `y` 且 `src` 连续 —— 光看 `src` 会放行，
    /// 那是个画错位的真实缺陷。
    pub src: u32,
    /// **在这里断开合批**：本字形必须自己起一段 `TextOutW`，不能并进前一段。
    ///
    /// # 为什么必须有这个字段（2026-11-03，公式接线时踩的坑）
    ///
    /// `runs()` 的合批前提是「段内 `x` 按步进相接」——它靠
    /// 「同 `y` + 同字体 + `src` 连续」来推断，而这三条对**普通文字**
    /// 是充分的：排版就是按步进往前推的。
    ///
    /// 公式块打破了这个前提：公式后面的第一个字，`src` 仍然连续、
    /// `y` 仍然相同，但 `x` 隔了一个公式的宽度。合批于是把它当普通字
    /// 交给 GDI 按步进推 —— 公式让出的空档被吃掉，后半句直接压到公式上。
    /// 实测：`质能关系 $E = mc^2$ 说明质量就是能量` 横版，公式块
    /// 217px 宽，`说明` 被拉回 200 多像素，正好盖住 `E = mc²`。
    ///
    /// 判据只有排版层知道（它才是挪过 x 的人），所以只能由它标出来。
    pub brk: bool,
}

/// 一段文字的排版结果。**同一帧的三趟绘制共用同一份**。
#[derive(Clone, Debug, Default)]
pub struct QuoteLayout {
    pub glyphs: Vec<Glyph>,
    /// 占用的行数（横版）/ 列数（竖版）
    pub lanes: i32,
    /// 实际占用的尺寸 (宽, 高)
    pub extent: (i32, i32),
    /// 实际采用的字宽档：100 / 85 / 70
    pub shrink: i32,
    /// 连最小档都放不下 → 调用方应转跑马灯（横版）或接受省略号（竖版）
    pub overflow: bool,
    /// 结尾补了「…」
    pub ellipsis: bool,
    /// **竖版专用**：每列的字形下标范围 `[起, 止)`，列序自右向左
    /// （第 0 列最靠主文右侧 = 阅读顺序的第一列）。横版为空。
    ///
    /// 存在的理由：竖版入场的**逐列错峰**（PR3）需要按列分组 ——
    /// compose 按 x 分带调 k、阴影趟按列写灰度，两边都要「哪些字形属哪列」。
    /// 排版时列就是切分依据，范围精确到下标，不需要事后按 x 聚类
    /// （聚类会被点号的右移、列内居中的偏移糊弄错列）。
    pub col_ranges: Vec<(usize, usize)>,
    /// **公式块**（横版内联 / 竖版整块旋转）。没有公式时为空。
    ///
    /// 为什么公式**不进** `glyphs`：`glyphs` 是「一个字 + 一个位置」，
    /// 而公式里不但有字，还有分式横线、根号上横线、矢量箭头 ——
    /// 它们不是字符，`TextOutW` 画不出来。硬塞进去要么给 `Glyph` 加三种
    /// 图元变体（`runs()` 合批、竖版列切分、阴影趟全都要跟着改），
    /// 要么在绘制趟里再判一次类型。分成两个列表最省事：文字照旧逐字合批，
    /// 公式整块交给 `tex::draw` / `tex::draw_rot`。
    pub math: Vec<MathBlock>,
    /// **竖版专用**：每列的 x 瓦片 `[左, 右)`，与 `col_ranges` 同序。
    /// 相邻瓦片在列间距中点相接、首列右缘与末列左缘各扩展到版心边缘 ——
    /// **无缝瓦片**：任何格言像素（含点号右移、阴影 2px 外溢）都落在某一列里，
    /// 不会掉到「无带可查」的缺口上。横版为空（整块一个统一 k）。
    pub col_x: Vec<(i32, i32)>,
}

impl QuoteLayout {
    /// 定位表覆盖的字符数（等于「画出来几个字」）
    pub fn len(&self) -> usize {
        self.glyphs.len()
    }
    pub fn is_empty(&self) -> bool {
        // 公式块也是内容：只判 `glyphs` 会让「整句就是一个公式」的格言
        // 被当成空表，一个字都不画（而且是静默的）。
        self.glyphs.is_empty() && self.math.is_empty()
    }
    /// `shrink` 百分比 → 字体档下标（0/1/2 = 100/85/70）。
    /// 绘制方靠它去 `Fonts::quote_at` 取**同一档**字体 ——
    /// 排版量的是哪一档，画的时候就必须用哪一档，否则字宽对不上。
    pub fn font_tier(&self) -> usize {
        match self.shrink {
            85 => 1,
            70 => 2,
            _ => 0,
        }
    }

    /// 把定位表切成「可以一次 `TextOutW` 画完」的段，返回 `(首字形下标, 字数)`。
    ///
    /// 合并条件（全部满足）：
    ///   * 同一 `y`（同一条基线）；
    ///   * 两个字都不需要旋转（旋转字要换字体对象，不能混在一个调用里）；
    ///   * `src` 紧挨着 —— 原文里本来就是相邻的两个字；
    ///   * **不跨列**（竖版）。
    ///
    /// 段内其余字的位置**交给 GDI 按步进推**：排版表里的步进本来就是
    /// 从 `GetTextExtentExPointW` 的累计宽度差分出来的，两者同源。
    /// 所以真正的前提是「**段内 `x` 按步进相接**」，前三条只是它的（偏严的）
    /// 充分条件 —— 第四条不可省，见下。
    ///
    /// # 为什么必须加「不跨列」（2026-10-15 修复的真实缺陷）
    ///
    /// 竖版版心高 < 2 格时 `cap == 1`，每列只有一个字 ⇒ 相邻**列**的字
    /// 落在**同一个 `y`** 上，而它们的 `src` 又是连续的（列按原文顺序切），
    /// 于是前三条**全部满足**、被合进同一段 —— `PolyTextOutW` 随即沿一条
    /// 基线把整列画成一行。实测（竖版 240x80，窗口高度取最小值时可达）：
    /// 合批 vs 逐字 **669 px 差（3.48%）**，差异全落在一条横带
    /// `(107,35)-(239,49)`；同配置 305x451 差 **0 px**。
    /// 列距（`col_pitch`）≠ 步进（`adv`），第四条一加即断。
    ///
    /// 为什么值得这么做：`--perf` 实测逐字 `TextOutW` 画 66 字长句，
    /// 每帧要多花约 2000µs（≈37µs/字，主趟 + 阴影趟）。
    /// 竖版本来就拿不到这份收益（列内每字在不同 `y` 上；补上第四条后
    /// 极矮版心也不再误合），`ExtTextOutW` + `ETO_PDY` 那条路已验证不可用，
    /// 见 `gfx::draw_glyphs` 的注释。
    pub fn runs(&self) -> Vec<(usize, usize)> {
        let g = &self.glyphs;
        // 列首下标。**只对竖版非空**（横版的 `col_ranges` 恒为空 Vec）⇒
        // 这一层在横版上零开销，横版的合批结果逐位不变。
        let is_col_start = |j: usize| {
            !self.col_ranges.is_empty() && self.col_ranges.iter().any(|&(s, _)| s == j)
        };
        let mut out: Vec<(usize, usize)> = Vec::new();
        let mut i = 0;
        while i < g.len() {
            let mut j = i + 1;
            while j < g.len()
                && !is_col_start(j)
                && !g[j].rot
                && !g[j - 1].rot
                && g[j].src != u32::MAX
                && g[j - 1].src != u32::MAX
                && g[j].src == g[j - 1].src + 1
                && !g[j].brk
                && g[j].y == g[j - 1].y
            {
                j += 1;
            }
            out.push((i, j - i));
            i = j;
        }
        out
    }
}

/// 排版用到的度量。全部由**实际字体**量出来，不按比例估算 ——
/// 不同字号的 GDI 取整不一样，按比例乘会跟实际字宽对不上。
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    /// 一个汉字的宽 / 高（实测）
    pub char_w: i32,
    pub char_h: i32,
    /// 横版行距
    pub line_h: i32,
    /// 竖版单元格高
    pub cell_h: i32,
    /// 竖版列距
    pub col_pitch: i32,
}

impl Metrics {
    pub fn new(char_w: i32, char_h: i32) -> Metrics {
        Metrics {
            char_w,
            char_h,
            line_h: ((char_h as f32) * 1.35).round() as i32,
            // 竖排两把尺子（2026-11-04 竖排改版，草案定稿）：
            //
            //   * 单元格 = 1.10 字高 —— 字与字之间留 1/10 的白，读起来是「一串」
            //     而不是「一摞」；
            //   * 列距 = **1.70 字宽** —— 旧值 1.15 只留 0.15 字宽的列间白，
            //     两列几乎粘在一起（`--dump 431×660` 实测列间只剩 6px），
            //     这是「很难获取信息」的头号来源。竖排的列距本来就该宽：
            //     竖着读的时候，眼睛要靠列间白来定位下一列。
            cell_h: ((char_h as f32) * 1.10).round() as i32,
            col_pitch: ((char_w as f32) * 1.70).round() as i32,
        }
    }
}

// ---------------------------------------------------------------- 标点分类

/// 行首禁则：不允许出现在一行的开头。
///
/// 中文排版规范里这条叫「避头尾」。少了它，折行后会出现
/// 「……民免而无耻 / ，道之以德」这种把标点甩到行首的破排。
const NO_START: &[u16] = &[
    0xFF0C, // ，
    0x3002, // 。
    0x3001, // 、
    0xFF1B, // ；
    0xFF1A, // ：
    0xFF1F, // ？
    0xFF01, // ！
    0xFF09, // ）
    0x3015, // 〕
    0x3011, // 】
    0x300B, // 》
    0x300D, // 」
    0x300F, // 』
    0x301D, // 〝
    0x201D, // ”
    0x2019, // ’
    0x0025, // %
    0x2030, // ‰
    0x002C, // ,
    0x002E, // .
    0x003B, // ;
    0x003A, // :
    0x003F, // ?
    0x0021, // !
    0x0029, // )
    0x005D, // ]
    0x007D, // }
];

/// 行尾禁则：不允许出现在一行的末尾（否则它的另一半会掉到下一行开头）。
const NO_END: &[u16] = &[
    0xFF08, // （
    0x3014, // 〔
    0x3010, // 【
    0x300A, // 《
    0x300C, // 「
    0x300E, // 『
    0x201C, // “
    0x2018, // ‘
    0x0028, // (
    0x005B, // [
    0x007B, // {
];

/// 竖排句读点号：要挪到单元格**右上角**。
/// 横排里它们待在字格左下，竖排里同一个位置看起来像「掉到下一字下面」。
const VERT_DOT: &[u16] = &[
    0xFF0C, // ，
    0x3002, // 。
    0x3001, // 、
    0xFF1B, // ；
    0xFF1A, // ：
];

/// 竖排下应当旋转 90° 的字符（破折号 / 省略号）。
const VERT_ROT: &[u16] = &[
    0x2014, // —
    0x2015, // ―
    0x2026, // …
];

const ELLIPSIS: char = '…';

/// 竖版最多切几列。
///
/// **不是排版偏好，是渲染层的硬约束**：竖版入场/退场按列分带做渐变，
/// 带表是定长数组（`gfx::MAX_BANDS`）。几何上能装多少列由窗口决定 ——
/// 4000px 宽配最小字宽能装 170 多列 —— 而带表装不下的那部分列
/// 会「查不到带 = 系数 255 = 满亮」，症状是过渡时右半边闪一下。
///
/// 所以在这里截断：触到上限与触到窗口右缘的处理完全一样（末字换省略号）。
/// 127 + 1 条出处带 = `gfx::MAX_BANDS`；两者由 `--selftest` 的一条断言钉在一起，
/// 只改一边会立刻红。
pub const MAX_COLS: usize = 127;

pub fn no_start(c: char) -> bool {
    NO_START.contains(&(c as u16))
}
/// 句读收尾符：竖版断列优先收在它们**后面**（仿古排版「一句一列」的节奏）。
pub fn clause_end(c: char) -> bool {
    matches!(
        c,
        '\u{FF0C}' | // ，
        '\u{3002}' | // 。
        '\u{3001}' | // 、
        '\u{FF1B}' | // ；
        '\u{FF1A}' | // ：
        '\u{FF1F}' | // ？
        '\u{FF01}' | // ！
        '\u{2026}' | // …
        '\u{300D}' | // 」
        '\u{300F}' | // 』
        '\u{FF09}' // ）
    )
}
pub fn no_end(c: char) -> bool {
    NO_END.contains(&(c as u16))
}
pub fn is_vert_dot(c: char) -> bool {
    VERT_DOT.contains(&(c as u16))
}
fn is_vert_rot(c: char) -> bool {
    VERT_ROT.contains(&(c as u16))
}

// ---------------------------------------------------------------- 主入口

/// 竖排版心内的**水平**摆法。
///
/// 竖排只有一个自由轴（横向）：列序固定自右向左，整块可以贴右缘、也可以居中。
/// 传统立轴是「右起贴右缘」（`Right`，古法），而桌面挂件上居中更稳定 ——
/// 句子只有一列时贴右缘看起来像「排版掉到一边去了」。
///
/// 横版不使用这个参数（横版的行是逐行居中的）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Align {
    #[default]
    Right,
    Center,
}

/// 排一段文字。**纯函数**：同样的输入永远同样的输出，不碰任何全局状态。
///
/// * `chars` 是逐字符的码点（不含结尾 NUL），`adv[i]` 是第 i 个字**不含字距**的步进。
/// * `band` 是版心 `(左, 上, 右, 下)`，客户区坐标。
/// * `spacing` 是字距（px），横版加在字与字之间，竖版加在格与格之间。
pub fn layout(
    chars: &[char],
    adv: &[i32],
    m: Metrics,
    orient: Orient,
    band: (i32, i32, i32, i32),
    spacing: i32,
) -> QuoteLayout {
    layout_with_attr(chars, adv, m, orient, band, spacing, None)
}

/// 带出处分界的排版入口（2026-11-24 审查 U3）。
///
/// `attr_from` 是出处段（「——　作者」）在 `chars` 里的起始下标，**仅横版生效**：
/// 出处独立成行、右对齐；正文多行时左对齐。`None` = 整段都是正文（旧行为）。
/// 竖版忽略它 —— 竖版的出处归落款条，主文本来就不带出处尾巴。
pub fn layout_with_attr(
    chars: &[char],
    adv: &[i32],
    m: Metrics,
    orient: Orient,
    band: (i32, i32, i32, i32),
    spacing: i32,
    attr_from: Option<usize>,
) -> QuoteLayout {
    layout_ex(chars, adv, m, orient, band, spacing, attr_from, Align::Right)
}

/// `layout` 的**带摆法**版本。竖版用它决定整块贴右还是居中。
///
/// 保留 `layout` 这个 6 参入口，是因为 `layout()` 的行为被几十条单元测试
/// 逐像素钉着（含 `runs()` 合批、瓦片边界）—— 加一个默认参数不该让那些
/// 结论全部重算一遍。
///
/// `attr_from`（横版出处分界）与 `align`（竖版摆法）是两条正交的轴：
/// 前者只在横版被 `layout_h` 读，后者只在竖版被 `layout_v` 读。
#[allow(clippy::too_many_arguments)] // 七轴排版接口，拆结构体反而让调用点更难读
pub fn layout_ex(
    chars: &[char],
    adv: &[i32],
    m: Metrics,
    orient: Orient,
    band: (i32, i32, i32, i32),
    spacing: i32,
    attr_from: Option<usize>,
    align: Align,
) -> QuoteLayout {
    if chars.is_empty() || adv.len() < chars.len() {
        return QuoteLayout {
            shrink: 100,
            ..Default::default()
        };
    }
    match orient {
        Orient::Horizontal => layout_h(chars, adv, m, band, spacing, attr_from),
        Orient::Vertical => layout_v(chars, adv, m, band, spacing, align),
    }
}

// ---------------------------------------------------------------- 横版

fn layout_h(
    chars: &[char],
    adv: &[i32],
    m: Metrics,
    band: (i32, i32, i32, i32),
    spacing: i32,
    attr_from: Option<usize>,
) -> QuoteLayout {
    let (bl, bt, br, bb) = band;
    let avail_w = (br - bl).max(1);
    let avail_h = (bb - bt).max(1);
    let n = chars.len();
    // 出处分界：[0..attr_start) 正文，[attr_start..n) 出处（「——　作者」）。
    let mut attr_start = attr_from.map(|k| k.min(n)).unwrap_or(n);

    let line_w = |v: &[usize]| -> i32 {
        let mut s = 0;
        for (k, &i) in v.iter().enumerate() {
            s += adv[i];
            if k > 0 {
                s += spacing;
            }
        }
        s
    };

    // ---- ① 贪心折行（含行首 / 行尾禁则）。正文与出处共用同一条规则，
    // 下标全程是**全局**的（src 要指回原文，合批靠它判相邻）。
    let wrap = |from: usize, to: usize| -> Vec<Vec<usize>> {
        let mut lines: Vec<Vec<usize>> = Vec::new();
        let mut cur: Vec<usize> = Vec::new();
        for i in from..to {
            if !cur.is_empty() && line_w(&cur) + adv[i] + spacing > avail_w {
                if no_start(chars[i]) {
                    // 行首禁则：不换行，把它挤在本行（允许 ≤ 1 字宽的溢出）
                    cur.push(i);
                    continue;
                }
                if cur.len() >= 2 && no_end(chars[*cur.last().unwrap()]) {
                    // 行尾禁则：把它一起拖到下一行去，别让它孤零零收尾
                    let last = cur.pop().unwrap();
                    lines.push(std::mem::take(&mut cur));
                    cur.push(last);
                } else {
                    lines.push(std::mem::take(&mut cur));
                }
            }
            cur.push(i);
        }
        if !cur.is_empty() {
            lines.push(cur);
        }
        lines
    };
    let mut lines = wrap(0, attr_start);
    let mut attr_lines = wrap(attr_start, n);

    // ---- ② 纵向容量：出处行固定占位，正文给它让路；放不下截断 + 省略号
    let max_lanes = (avail_h / m.line_h.max(1)).max(1) as usize;
    // 「一个字都不许丢」压过「出处独立行」：独立行会把正文挤到截断时，
    // 退回内联出处（旧行为）—— D1 回归（60 字长句默认窗口不丢字）钉着这条。
    if attr_start < n && lines.len() + attr_lines.len() > max_lanes {
        lines = wrap(0, n);
        attr_lines = Vec::new();
        attr_start = n;
    }

    // ---- ①b 孤行控制（审查 U3）：正文末行不足 4 字时从上一行借字。
    // 「其大。」这类两字尾行读起来像排版事故。借字受禁则约束：借来的字
    // 落在末行行首（不得犯行首禁则），上一行新尾不得犯行尾禁则，且末行
    // 借完不得超宽 —— 任一不满足就停，宁留孤行不出破排。
    if lines.len() >= 2 && lines.last().unwrap().len() < 4 {
        let need = 4 - lines.last().unwrap().len();
        let prev = lines.len() - 2;
        for _ in 0..need {
            if lines[prev].len() <= 2 {
                break;
            }
            let idx = *lines[prev].last().unwrap();
            if no_start(chars[idx]) {
                break;
            }
            let new_tail = lines[prev][lines[prev].len() - 2];
            if no_end(chars[new_tail]) {
                break;
            }
            if line_w(lines.last().unwrap()) + adv[idx] + spacing > avail_w {
                break;
            }
            lines[prev].pop();
            lines.last_mut().unwrap().insert(0, idx);
        }
    }

    // ---- ②b 容量分配（max_lanes 已在上面算过）
    let attr_used = attr_lines.len().min(max_lanes.saturating_sub(1));
    let body_used = lines.len().min(max_lanes - attr_used);
    let mut truncated = lines.len() > body_used || attr_lines.len() > attr_used;
    let total = body_used + attr_used;
    let ext_h = if total == 0 {
        0
    } else {
        (total as i32 - 1) * m.line_h + m.char_h
    };
    let oy = bt + (avail_h - ext_h) / 2;

    // ---- ③ 对齐（审查 U3）：单行正文且无出处 → 居中（短句卡片感，旧行为）；
    // 其余正文左对齐（消除逐行居中的两侧参差），出处右对齐独立行。
    let body_left = attr_start < n || body_used > 1;

    let mut glyphs: Vec<Glyph> = Vec::new();
    'outer: for j in 0..total {
        let (line, x0) = if j < body_used {
            let line = &lines[j];
            let x0 = if body_left {
                bl
            } else {
                bl + (avail_w - line_w(line)) / 2
            };
            (line, x0)
        } else {
            let line = &attr_lines[j - body_used];
            (line, (br - line_w(line)).max(bl))
        };
        let mut x = x0;
        for (k, &i) in line.iter().enumerate() {
            if k > 0 {
                x += spacing;
            }
            // 最后一行的字若越过右缘，就地截断（禁则带出来的那一字宽的溢出）
            if j + 1 == total && x + adv[i] > br && !glyphs.is_empty() {
                truncated = true;
                break 'outer;
            }
            glyphs.push(Glyph {
                ch: chars[i],
                x,
                y: oy + j as i32 * m.line_h,
                rot: false,
                src: i as u32,
                brk: false,
            });
            x += adv[i];
        }
    }

    // 截断了就用「…」收尾：**绝不能什么都不说地消失**，那是这个模块出生的原因
    let mut ellipsis = false;
    if truncated {
        if let Some(last) = glyphs.pop() {
            glyphs.push(Glyph {
                ch: ELLIPSIS,
                x: last.x,
                y: last.y,
                rot: false,
                src: u32::MAX,
                brk: false,
            });
        }
        ellipsis = true;
    }

    let (ext_w, ext_h) = bounds(&glyphs, m);
    QuoteLayout {
        glyphs,
        lanes: total as i32,
        extent: (ext_w, ext_h),
        shrink: 100,
        overflow: truncated,
        ellipsis,
        col_ranges: Vec::new(),
        col_x: Vec::new(),
        math: Vec::new(),
    }
}

// ---------------------------------------------------------------- 竖版

fn layout_v(
    chars: &[char],
    adv: &[i32],
    m: Metrics,
    band: (i32, i32, i32, i32),
    spacing: i32,
    align: Align,
) -> QuoteLayout {
    let (bl, bt, br, bb) = band;
    let avail_w = (br - bl).max(1);
    let avail_h = (bb - bt).max(1);
    let cell = m.cell_h + spacing;

    // ---- ① 列切分（列内自上而下、列序自右向左）。
    // 断列优先级：**句读后断 > cap 满断 + 列首禁则兜底**（仿古条幅的节奏：
    // 一列尽量是一个语义单位，不把「春社」「衣冠」这类词劈到两列去）。
    // 列比 cap 短一两格，换来的断句节奏是值得的；cap 比一个长句还短时
    // 退回满断 + 禁则 —— 那时任何断点都在句中，古人雕版也是这么办的。
    let cap = (avail_h / cell.max(1)).max(1) as usize;
    let mut cols: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        if cur.len() >= cap {
            // 列满了：先看列尾 1~3 格内有没有句读，有就收在**最靠右**的
            // 点号后面（列缩短，余字挪去下一列开头）；找不到才硬切。
            let mut brk = None;
            for p in (cap.saturating_sub(3)..cap).rev() {
                if clause_end(chars[cur[p]]) {
                    brk = Some(p + 1);
                    break;
                }
            }
            match brk {
                Some(p) => {
                    let rest = cur.split_off(p);
                    cols.push(std::mem::take(&mut cur));
                    cur = rest;
                }
                None => {
                    if no_start(c) && cur.len() >= 2 {
                        // 列首禁则：把当前列末字拖到下一列去当「衬底」
                        let last = cur.pop().unwrap();
                        cols.push(std::mem::take(&mut cur));
                        cur.push(last);
                    } else {
                        cols.push(std::mem::take(&mut cur));
                    }
                }
            }
        }
        cur.push(i);
    }
    if !cur.is_empty() {
        cols.push(cur);
    }

    // ---- ② 横向容量：列太多就只留最右边那几列
    //
    // 两道限制取小：窗口装得下几列（几何），以及 `MAX_COLS`（渲染层的带表
    // 是定长数组）。后者不能省 —— 4000×80 这种比例下几何能装 170 多列，
    // 而带表装不下时逐列渐变会静默失效（见 `gfx::MAX_BANDS`）。
    // 触到 `MAX_COLS` 与触到窗口边缘的处理**完全一样**：截断 + 末字换省略号，
    // 用户看到的是「太长了，后面省略」，而不是「后半截颜色不对」。
    let max_cols = ((avail_w / m.col_pitch.max(1)).max(1) as usize).min(MAX_COLS);
    let truncated = cols.len() > max_cols;
    let used = cols.len().min(max_cols);
    let rows = cols.iter().take(used).map(|c| c.len()).max().unwrap_or(0) as i32;
    let ext_h = if rows == 0 {
        0
    } else {
        (rows - 1) * cell + m.char_h
    };
    let oy = bt + (avail_h - ext_h) / 2;
    // 竖版只有一个横向自由轴。摆位必须按**墨迹**算，不能按列原点算：
    // 列内的字在列距里是居中的（`gx = cx + (col_pitch - adv)/2`，半角字要摆到
    // 列中间），所以墨迹右缘比列原点靠右 `(col_pitch - char_w)/2`。漏掉这一截，
    // 整块会系统性偏右半列（实测 431 宽窗口偏 14px，肉眼一眼能看出「歪了」）。
    let inset = ((m.col_pitch - m.char_w) / 2).max(0);
    let block_w = (used as i32 - 1) * m.col_pitch + m.char_w;
    // `Right` —— 墨迹右缘贴版心右缘（右起，条幅的古法）；
    // `Center` —— 整块摆到版心正中（桌面挂件上更稳：只有一列时贴右缘
    // 看起来像排版掉到一边）。
    let mut x_r = br - m.char_w - inset;
    if align == Align::Center && used > 0 {
        x_r = bl + ((avail_w - block_w).max(0)) / 2 + (used as i32 - 1) * m.col_pitch - inset;
    }

    let mut glyphs: Vec<Glyph> = Vec::new();
    for (ci, col) in cols.iter().take(used).enumerate() {
        let cx = x_r - ci as i32 * m.col_pitch;
        for (j, &i) in col.iter().enumerate() {
            // 列内居中：全角字几乎不偏，半角（ASCII）会被摆到列中间
            let mut gx = cx + (m.col_pitch - adv[i]).max(0) / 2;
            let mut gy = oy + j as i32 * cell;
            let ch = chars[i];
            if is_vert_dot(ch) {
                // 句读点号归位到单元格右上。横排里，`，` `。` 的墨迹待在字格
                // **左下**；竖排里同一个位置看起来像「掉到下一个字下面」去了。
                // 右移约半列、上移约半格，正好落在本格的右上角 ——
                // 这就是竖排点号该待的地方。
                // 偏移量按**字宽**算，不按列距：列距是 1.70 字宽，
                // 乘 0.55 会到 0.94 字宽 —— 点号直接飞进列间空档里。
                gx += ((m.char_w as f32) * 0.55).round() as i32;
                gy -= ((m.cell_h as f32) * 0.55).round() as i32;
                gy = gy.max(bt);
            }
            glyphs.push(Glyph {
                ch,
                x: gx,
                y: gy,
                rot: is_vert_rot(ch),
                src: i as u32,
                brk: false,
            });
        }
    }

    // 列被砍掉 → 最左那列末字换省略号
    let mut ellipsis = false;
    if truncated {
        if let Some(last) = glyphs.pop() {
            glyphs.push(Glyph {
                ch: ELLIPSIS,
                x: last.x,
                y: last.y,
                rot: false,
                src: u32::MAX,
                brk: false,
            });
        }
        ellipsis = true;
    }

    let (ext_w, ext_h) = bounds(&glyphs, m);

    // 列范围（字形下标，列序自右向左）+ 列 x 瓦片（无缝，见字段注释）。
    // 省略号占的是被弹出那个字的下标槽位，列长不变 ⇒ 范围天然覆盖它。
    //
    // 瓦片边界取**列间中点**（相邻两列中心连线的中点）：不重叠、不留缝。
    // 首列右缘 / 末列左缘先贴版心，`build_frame` 再把它们扩到窗口边缘 ——
    // 阴影外溢 2px 也要落在某一列里，不能掉到「无带可查」的缺口上。
    let mut col_ranges: Vec<(usize, usize)> = Vec::with_capacity(used);
    let mut col_x: Vec<(i32, i32)> = Vec::with_capacity(used);
    let mut s = 0usize;
    let pitch = m.col_pitch.max(1);
    // 第 ci 列与其左邻列之间的边界：cx_ci - pitch/2 + char_w/2
    let edge = |ci: usize| x_r - ci as i32 * pitch - pitch / 2 + m.char_w / 2;
    for (ci, col) in cols.iter().take(used).enumerate() {
        let e = s + col.len();
        col_ranges.push((s, e));
        let (l, r) = if used == 1 {
            (bl, br)
        } else if ci == 0 {
            (edge(0), br)
        } else if ci == used - 1 {
            (bl, edge(ci - 1))
        } else {
            (edge(ci), edge(ci - 1))
        };
        col_x.push((l, r));
        s = e;
    }

    QuoteLayout {
        glyphs,
        lanes: used as i32,
        extent: (ext_w, ext_h),
        shrink: 100,
        overflow: truncated,
        ellipsis,
        col_ranges,
        col_x,
        // 纯文字路径**永远**不带公式块：带 `$` 的句子走 `layout_rich`。
        // 这条分岔是有意的 —— 不含公式的句子逐字逐位与旧实现相同，
        // 不因为「加了公式支持」而整体重排（像素级回归会立刻抓到）。
        math: Vec::new(),
    }
}

#[cfg(test)]
mod rich_tests {
    use super::*;
    use crate::tex::{TextMetric, Ink};

    /// 假度量：汉字 1em、拉丁 0.55em，上高 0.8em、下深 0.2em。
    /// 与 `tex` 里的那个同参，但**不能共用**（那个是 `#[cfg(test)]` 私有）。
    struct FM;
    impl crate::tex::Measure for FM {
        fn text(&self, s: &[char], px: i32) -> TextMetric {
            let mut w = 0;
            for &c in s {
                w += if (c as u32) >= 0x2E80 {
                    px
                } else {
                    ((px as f32) * 0.55).round() as i32
                };
            }
            TextMetric {
                w,
                ascent: ((px as f32) * 0.8).round() as i32,
                descent: ((px as f32) * 0.2).round() as i32,
            }
        }
        fn ink(&self, ch: char, px: i32) -> Ink {
            let t = self.text(&[ch], px);
            Ink {
                top: -t.ascent,
                bottom: t.descent,
                w: t.w,
                adv: t.w,
            }
        }
    }

    fn rich(src: &str, orient: Orient, band: (i32, i32, i32, i32)) -> QuoteLayout {
        let (segs, _) = crate::quotes::segments(src);
        let m = Metrics::new(20, 20);
        let mut fm = FM;
        layout_rich(&segs, 20, m, orient, band, 0, &mut fm)
    }

    #[test]
    fn 横版公式不被折行拆开() {
        // 版心只有 60px，而公式本身 200px 宽：它必须整块落在**一行**里，
        // 宁可溢出，也绝不能拆成两半（拆开的分式没有意义）。
        let ql = rich("字$\\frac{a}{b}$", Orient::Horizontal, (0, 0, 60, 300));
        assert_eq!(ql.math.len(), 1, "公式应整块保留");
        assert_eq!(ql.glyphs.len(), 1, "只有开头那个汉字");
        // 两行：第一行 `字`，第二行公式
        let y0 = ql.glyphs[0].y;
        assert!(ql.math[0].y > y0, "公式应折到下一行");
    }

    #[test]
    fn 公式后首字断开合批() {
        let ql = rich("字$E=mc^2$字", Orient::Horizontal, (0, 0, 900, 400));
        assert_eq!(ql.math.len(), 1);
        assert_eq!(ql.glyphs.len(), 2);
        assert!(!ql.glyphs[0].brk, "第一个字前面没有公式");
        assert!(
            ql.glyphs[1].brk,
            "公式之后的字必须断批 —— 否则 GDI 按步进推，会吃掉公式让出的宽度"
        );
        // 断批还必须在 `runs()` 里生效：两段，不是一段
        let runs = ql.runs();
        assert!(runs.len() >= 2, "公式两边的字不能合进同一段：{:?}", runs);
    }

    #[test]
    fn 竖版公式整块旋转且占位等于盒子宽() {
        let ql = rich("字$E=mc^2$字", Orient::Vertical, (0, 0, 600, 600));
        assert_eq!(ql.math.len(), 1, "竖版也必须保住公式块");
        let blk = &ql.math[0];
        assert!(blk.rot, "竖版的公式要整块转 90°");
        // 旋转后宽高互换：占宽 = 盒子上高 + 下深，占高 = 盒子宽
        assert_eq!(blk.w(), blk.mb.h());
        assert_eq!(blk.h(), blk.mb.w);
        assert!(blk.w() > 0 && blk.h() > 0);
    }

    #[test]
    fn 纯文字不进公式块() {
        let ql = rich("春眠不觉晓", Orient::Horizontal, (0, 0, 900, 400));
        assert!(ql.math.is_empty());
        assert_eq!(ql.glyphs.len(), 5);
        assert!(ql.glyphs.iter().all(|g| !g.brk), "纯文字一个字都不该断批");
    }

    #[test]
    fn 公式段解析不吞掉正文() {
        let (segs, bad) = crate::quotes::segments("质能关系 $E=mc^2$ 就是能量");
        assert!(!bad);
        assert_eq!(segs.len(), 3);
        assert!(matches!(&segs[0], crate::quotes::Seg::Text(t) if t == "质能关系 "));
        assert!(matches!(&segs[1], crate::quotes::Seg::Math(t) if t == "E=mc^2"));
        assert!(matches!(&segs[2], crate::quotes::Seg::Text(t) if t == " 就是能量"));
    }

    #[test]
    fn 落单美元符原样保留() {
        // 半个 `$` 不许吞掉后半句 —— 宁可显示一个美元符，也不能让文字消失
        let (segs, bad) = crate::quotes::segments("价格 $5 一句");
        assert!(bad, "应报「有落单的 $」");
        let all: String = segs
            .iter()
            .map(|s| match s {
                crate::quotes::Seg::Text(t) | crate::quotes::Seg::Math(t) => t.clone(),
            })
            .collect();
        assert_eq!(all, "价格 $5 一句");
    }

    #[test]
    fn 示例库里的每条公式都能解析() {
        // 这份示例是**给用户抄的**：里面有一条解析不了，用户复制过去就是
        // 屏幕上多出一串反斜杠。所以它在编译期跟着测试走，不许过期。
        let src = include_str!("../assets/quotes-math-sample.txt");
        let mut n = 0;
        for line in src.lines() {
            if line.trim_start().starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let (segs, bad) = crate::quotes::segments(line);
            assert!(!bad, "示例行有落单的 `$`：{}", line);
            for s in segs {
                if let crate::quotes::Seg::Math(m) = s {
                    n += 1;
                    crate::tex::parse(&m)
                        .unwrap_or_else(|e| panic!("示例公式解析失败 `{}`：{}", m, e));
                }
            }
        }
        assert!(n >= 12, "示例里应有多条公式，实际 {}", n);
    }

    #[test]
    fn 转义的美元符当文字() {
        let (segs, bad) = crate::quotes::segments("价格 \\$5");
        assert!(!bad);
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0], crate::quotes::Seg::Text(t) if t == "价格 $5"));
    }
}

/// 定位表的实际包围盒（宽, 高）
fn bounds(glyphs: &[Glyph], m: Metrics) -> (i32, i32) {
    let Some(first) = glyphs.first() else {
        return (0, 0);
    };
    let (mut l, mut t) = (first.x, first.y);
    let (mut r, mut b) = (first.x + m.char_w, first.y + m.char_h);
    for g in glyphs {
        l = l.min(g.x);
        t = t.min(g.y);
        r = r.max(g.x + m.char_w);
        b = b.max(g.y + m.char_h);
    }
    (r - l, b - t)
}

// ---------------------------------------------------------------- 公式块

/// 一块公式在画面上的位置。
///
/// 两种朝向的语义不同，字段注释写清楚，免得调用方混着用：
///
/// | | `x` | `y` |
/// |---|---|---|
/// | 横版（`rot = false`） | 盒子左缘 | **基线** |
/// | 竖版（`rot = true`） | 旋转后包围盒左缘 | 包围盒**上缘** |
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MathBlock {
    pub x: i32,
    pub y: i32,
    /// 整块顺时针 90°（竖版）。绘制走 `tex::draw_rot`。
    pub rot: bool,
    pub mb: MBox,
}

impl MathBlock {
    /// 占宽（旋转后宽高互换）
    pub fn w(&self) -> i32 {
        if self.rot {
            self.mb.h()
        } else {
            self.mb.w
        }
    }
    /// 占高（旋转后宽高互换）
    pub fn h(&self) -> i32 {
        if self.rot {
            self.mb.w
        } else {
            self.mb.h()
        }
    }
}

/// 排版里的一颗「原子」：一个字，或一整块公式。
///
/// 折行 / 分列都以**原子**为单位 —— 公式是**不可拆的**，不能像文字那样
/// 在中间断开（把 `\frac{a}{b}` 断成两行没有任何意义）。
struct Atom {
    ch: Option<char>,
    mb: Option<MBox>,
    /// 占宽（公式含左右各半个 pad）
    w: i32,
    asc: i32,
    desc: i32,
    /// 原文下标（公式为 `u32::MAX`，与「补出来的 …」同档）
    src: u32,
}

/// 公式缩到能放下时的字号下限（相对正文字号）。
///
/// 再小就没法看了 —— 宁可溢出报错，也不要排出「一坨黑点」。
const MATH_FIT_MIN: f32 = 0.45;

/// 把文字 / 公式的分段序列，摊成一条原子流。
///
/// `max_box_w` 是**公式盒子的可用上限**：横版是版心宽，竖版是版心高
/// （竖版的公式整块旋转，盒子的「宽」转完就是屏幕上的高度）。
/// 超过就只把**这块公式**缩字号，不去动正文 —— 正文跟着缩会让整句
/// 莫名其妙变小，用户只会觉得「字怎么忽大忽小」。
fn atoms_of(
    segs: &[Seg],
    px: i32,
    meas: &mut dyn Measure,
    max_box_w: Option<i32>,
) -> Vec<Atom> {
    let mut out: Vec<Atom> = Vec::new();
    let mut idx: u32 = 0;
    // 公式和两侧文字之间留一点气口，不然 `质能关系E=mc²说明` 会贴在一起。
    // 0.2em 是看着调的：0.125em 时 `²` 和后面的汉字几乎挨上。
    let pad = (px / 5).max(3);
    for seg in segs {
        match seg {
            Seg::Text(t) => {
                for c in t.chars() {
                    let tm = meas.text(&[c], px);
                    out.push(Atom {
                        ch: Some(c),
                        mb: None,
                        w: tm.w.max(0),
                        asc: tm.ascent,
                        desc: tm.descent,
                        src: idx,
                    });
                    idx += 1;
                }
            }
            Seg::Math(src) => {
                let node = crate::tex::parse_or_text(src);
                let mut mpx = px;
                let mut mb = crate::tex::layout(&node, mpx, meas);
                if let Some(cap) = max_box_w {
                    // 缩到能塞进版心为止，最多收到 `MATH_FIT_MIN`。
                    // 缩不动就照原样交出去 —— 溢出由 `overflow` 报给调用方，
                    // 那条路会把整句降到下一档字号，比在这里死循环强。
                    //
                    // **按比例一步到位，而不是每次乘 0.85**：字形 advance 与
                    // 字号是线性的，比一比就能算出目标字号。之前逐次缩 15%
                    // 时，一条比版心高出一倍多的公式要排五六遍，每次都是
                    // 几十次 GDI 量字 —— 实测那正是竖版排版 1.2ms 的大头。
                    // 非线性（取整、上下标档位跳变）靠后面最多两次校正兜住。
                    let floor = ((px as f32) * MATH_FIT_MIN).round().max(8.0) as i32;
                    let mut tries = 0;
                    while mb.w + pad > cap && mpx > floor && tries < 2 {
                        let want = (((cap - pad).max(1) as f64) * (mpx as f64) / (mb.w.max(1) as f64))
                            .floor() as i32;
                        let next = want.clamp(floor, mpx - 1);
                        if next >= mpx {
                            break;
                        }
                        mpx = next;
                        mb = crate::tex::layout(&node, mpx, meas);
                        tries += 1;
                    }
                }
                out.push(Atom {
                    ch: None,
                    w: mb.w + pad,
                    asc: mb.ascent,
                    desc: mb.descent,
                    mb: Some(mb),
                    src: u32::MAX,
                });
            }
        }
    }
    out
}

/// **带公式的排版入口**。只有句子里含 `$...$` 时才走这里。
///
/// # 为什么和 `layout()` 分成两条路
///
/// `layout()` 吃的是「逐字步进数组」，是渲染层用 `GetTextExtentExPointW`
/// 一次性量出来的（含 kerning）。公式块没有「逐字步进」这回事。
///
/// 与其把 `layout()` 改造成「有的字有步进、有的字是块」，不如新开一条：
/// **不含公式的句子逐字逐位与旧实现相同**，公式支持不会让全库的格言
/// 重新排一遍版。这条分岔由 `render::layout_quote` 判 `$` 决定，
/// 并由单元测试钉住「纯文字走老路」。
pub fn layout_rich(
    segs: &[Seg],
    px: i32,
    m: Metrics,
    orient: Orient,
    band: (i32, i32, i32, i32),
    spacing: i32,
    meas: &mut dyn Measure,
) -> QuoteLayout {
    layout_rich_ex(segs, px, m, orient, band, spacing, meas, Align::Right)
}

/// `layout_rich` 的**带摆法**版本（竖版居中 / 贴右）。
#[allow(clippy::too_many_arguments)]
pub fn layout_rich_ex(
    segs: &[Seg],
    px: i32,
    m: Metrics,
    orient: Orient,
    band: (i32, i32, i32, i32),
    spacing: i32,
    meas: &mut dyn Measure,
    align: Align,
) -> QuoteLayout {
    let (bw, bh) = ((band.2 - band.0).max(1), (band.3 - band.1).max(1));
    let max_box_w = match orient {
        Orient::Horizontal => Some(bw),
        // 竖版转 90°：盒子的「宽」变成竖向量，上限是版心**高**
        Orient::Vertical => Some(bh),
    };
    let atoms = atoms_of(segs, px, meas, max_box_w);
    if atoms.is_empty() {
        return QuoteLayout {
            shrink: 100,
            ..Default::default()
        };
    }
    match orient {
        Orient::Horizontal => rich_h(&atoms, m, band, spacing, px),
        Orient::Vertical => rich_v(&atoms, m, band, spacing, px, align),
    }
}

/// 公式块 + 文字的包围盒（宽, 高）
fn rich_bounds(ql: &QuoteLayout, m: Metrics) -> (i32, i32) {
    let mut l = i32::MAX;
    let mut t = i32::MAX;
    let mut r = i32::MIN;
    let mut b = i32::MIN;
    for g in &ql.glyphs {
        l = l.min(g.x);
        t = t.min(g.y);
        r = r.max(g.x + m.char_w);
        b = b.max(g.y + m.char_h);
    }
    for mb in &ql.math {
        l = l.min(mb.x);
        t = t.min(mb.y);
        r = r.max(mb.x + mb.w());
        b = b.max(mb.y + mb.h());
    }
    if l > r {
        (0, 0)
    } else {
        (r - l, b - t)
    }
}

/// 定位表的**内容左上角**：横版排版把每一行居中到版心宽度上，坐标里带着
/// 版心偏移；跑马灯只关心「这句本身有多宽、从哪儿起」，需要把这个偏移剥掉。
pub fn content_origin(ql: &QuoteLayout) -> (i32, i32) {
    let mut l = i32::MAX;
    let mut t = i32::MAX;
    for g in &ql.glyphs {
        l = l.min(g.x);
        t = t.min(g.y);
    }
    for mb in &ql.math {
        l = l.min(mb.x);
        t = t.min(mb.y);
    }
    if l == i32::MAX {
        (0, 0)
    } else {
        (l, t)
    }
}

/// 整体平移定位表（字形与公式块一起），让「内容左上角」落到给定位置。
///
/// 跑马灯把一句当**整块**搬动：排版时的版心（含行居中）在这里没有意义，
/// 滚动只需要「内容从 x 开始」。公式块必须跟着挪 —— 只挪字不挪线，
/// 分式的横线会留在原地。
pub fn shift_by(ql: &mut QuoteLayout, dx: i32, dy: i32) {
    for g in &mut ql.glyphs {
        g.x += dx;
        g.y += dy;
    }
    for mb in &mut ql.math {
        mb.x += dx;
        mb.y += dy;
    }
}

/// 横版：公式当「一个宽字」参与折行，基线与同行文字对齐。
fn rich_h(atoms: &[Atom], m: Metrics, band: (i32, i32, i32, i32), spacing: i32, px: i32) -> QuoteLayout {
    let (bl, bt, br, bb) = band;
    let avail_w = (br - bl).max(1);
    let avail_h = (bb - bt).max(1);

    let lw = |v: &[usize]| -> i32 {
        let mut s = 0;
        for (k, &i) in v.iter().enumerate() {
            if k > 0 {
                s += spacing;
            }
            s += atoms[i].w;
        }
        s
    };

    // ---- ① 贪心折行（行首 / 行尾禁则照旧；公式不可拆）
    let mut lines: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    for (i, a) in atoms.iter().enumerate() {
        if !cur.is_empty() && lw(&cur) + a.w + spacing > avail_w {
            let hard = a.ch.map(no_start).unwrap_or(false);
            if hard {
                // 行首禁则：挤在本行（允许 ≤ 1 字宽的溢出），与 `layout_h` 同策
                cur.push(i);
                continue;
            }
            let tail_bad = cur.len() >= 2
                && atoms[*cur.last().unwrap()]
                    .ch
                    .map(no_end)
                    .unwrap_or(false);
            if tail_bad {
                let last = cur.pop().unwrap();
                lines.push(std::mem::take(&mut cur));
                cur.push(last);
            } else {
                lines.push(std::mem::take(&mut cur));
            }
        }
        cur.push(i);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }

    // ---- ② 行高与基线：含公式的行按实际墨迹撑高
    //
    // 上限 `m.line_h` 不能省：没有公式的行如果按 `asc + desc` 算，会跟
    // `layout()` 排出来的行距不一样 —— 同一版面里两种行距并存，一眼能看出。
    let pad_y = (px / 3).max(6);
    let mut geoms: Vec<(i32, i32, i32)> = Vec::with_capacity(lines.len());
    for line in &lines {
        let mut asc = 1;
        let mut desc = 1;
        let mut has_math = false;
        for &i in line {
            asc = asc.max(atoms[i].asc);
            desc = desc.max(atoms[i].desc);
            has_math |= atoms[i].mb.is_some();
        }
        let lh = if has_math {
            (asc + desc + pad_y).max(m.line_h)
        } else {
            m.line_h.max(asc + desc)
        };
        geoms.push((asc, desc, lh));
    }

    // ---- ③ 纵向容量
    let mut total = 0;
    let mut used = 0;
    for &(_, _, lh) in &geoms {
        if total + lh > avail_h && used > 0 {
            break;
        }
        total += lh;
        used += 1;
    }
    // 两种「放不下」必须分开算：
    //   * `cut`  —— 真的丢了内容（末尾要补 `…`，这是给用户看的）；
    //   * `overflow` —— 「这一档排不下」，要让缩字阶梯往下走。
    // 混成一个标志的后果是：明明一个字都没少，末尾却被换成了 `…`。
    let too_wide = atoms.iter().any(|a| a.w > avail_w);
    let mut cut = used < lines.len();
    let oy = bt + (avail_h - total).max(0) / 2;

    let pad = (px / 5).max(3);
    let mut glyphs: Vec<Glyph> = Vec::new();
    let mut math: Vec<MathBlock> = Vec::new();
    // 公式块之后的首字必须断批：它的 `x` 隔了一个公式的宽度，
    // 而合批是靠「src 连续 + 同 y」判的，会把这段空档吃掉。
    let mut pending_brk = false;
    let mut y = oy;
    for (j, line) in lines.iter().take(used).enumerate() {
        let (asc, _desc, lh) = geoms[j];
        let base = y + asc;
        let mut x = bl + (avail_w - lw(line)) / 2;
        for (k, &i) in line.iter().enumerate() {
            if k > 0 {
                x += spacing;
            }
            let a = &atoms[i];
            match (a.ch, &a.mb) {
                (Some(ch), _) => {
                    // 最后一行的字越过右缘就截断（禁则带出来的那一字宽）
                    if j + 1 == used && x + a.w > br && !glyphs.is_empty() {
                        cut = true;
                        break;
                    }
                    glyphs.push(Glyph {
                        ch,
                        x,
                        // 文字 `y` 是**字格顶**（`TextOutW` 的语义），基线在同档
                        // 字体的 `asc` 之下 —— 用原子自己的 `asc`，不是行首那个。
                        y: base - a.asc,
                        rot: false,
                        src: a.src,
                        brk: pending_brk,
                    });
                    pending_brk = false;
                }
                (None, Some(mb)) => {
                    math.push(MathBlock {
                        x: x + pad / 2,
                        y: base,
                        rot: false,
                        mb: mb.clone(),
                    });
                    pending_brk = true;
                }
                (None, None) => {}
            }
            x += a.w;
        }
        y += lh;
    }

    let mut ellipsis = false;
    if cut {
        if let Some(last) = glyphs.pop() {
            glyphs.push(Glyph {
                ch: ELLIPSIS,
                x: last.x,
                y: last.y,
                rot: false,
                src: u32::MAX,
                brk: false,
            });
        }
        ellipsis = true;
    }

    let mut ql = QuoteLayout {
        glyphs,
        lanes: used as i32,
        shrink: 100,
        overflow: cut || too_wide,
        ellipsis,
        col_ranges: Vec::new(),
        col_x: Vec::new(),
        math,
        extent: (0, 0),
    };
    ql.extent = rich_bounds(&ql, m);
    ql
}

/// 竖版：公式整块**顺时针转 90°**，像书脊上的字一样侧躺，自上而下读。
///
/// # 为什么是旋转而不是横着嵌
///
/// 竖排里横着嵌一块公式（縦中横 的思路），公式一宽就会横向压到邻列 ——
/// 而竖排的列距只有 `1.15 × 字宽`，连 `E = mc²` 都塞不进去。
/// 转 90° 之后，公式的「宽」变成竖向的长度，正好沿着列的方向走，
/// 列距只需要够放下它的「高」（一个多字高），几何上是自洽的。
fn rich_v(
    atoms: &[Atom],
    m: Metrics,
    band: (i32, i32, i32, i32),
    spacing: i32,
    px: i32,
    align: Align,
) -> QuoteLayout {
    let (bl, bt, br, bb) = band;
    let avail_w = (br - bl).max(1);
    let avail_h = (bb - bt).max(1);
    let cell = m.cell_h + spacing;
    // 公式块上下（竖排里是「前后」）留一口气：旋转后的公式和下一个汉字
    // 之间只隔一个 `spacing` 会贴在一起，像被按扁了。
    let breath = (px / 3).max(6);

    let atom_h = |a: &Atom| -> i32 {
        match &a.mb {
            Some(mb) => mb.w + spacing,
            None => cell,
        }
    };

    // ---- ① 分列：满了就断，断点优先落在句读后（仿古条幅的节奏）
    let mut cols: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut used_h = 0;
    for (i, a) in atoms.iter().enumerate() {
        let h = atom_h(a);
        if !cur.is_empty() && used_h + h > avail_h {
            let mut brk = cur.len();
            for p in (cur.len().saturating_sub(3)..cur.len()).rev() {
                if atoms[cur[p]].ch.map(clause_end).unwrap_or(false) {
                    brk = p + 1;
                    break;
                }
            }
            let rest = cur.split_off(brk);
            cols.push(std::mem::take(&mut cur));
            used_h = rest.iter().map(|&j| atom_h(&atoms[j])).sum();
            cur = rest;
        }
        used_h += h;
        cur.push(i);
    }
    if !cur.is_empty() {
        cols.push(cur);
    }

    // ---- ② 每列宽度：**逐列算**，不用统一列距
    //
    // 一开始这里用的是「统一列距 = 最宽那一列的宽度」，结果整篇都被最胖的
    // 那一列拖稀了：一句里有一个公式，其余五六列汉字之间的空隙全都翻倍。
    // 竖排的列距本来就是「字宽 + 一点呼吸」，公式列自己有额外宽度，
    // 两者不该互相绑架。
    let gap = (m.col_pitch - m.char_w).max(2);
    let mut col_w: Vec<i32> = Vec::with_capacity(cols.len());
    for col in &cols {
        let mut w = m.char_w;
        for &i in col {
            let aw = match &atoms[i].mb {
                Some(mb) => mb.h(),
                None => atoms[i].w,
            };
            w = w.max(aw);
        }
        col_w.push(w);
    }

    // 从右往左摆（右起），装不下就截断
    let mut col_left: Vec<i32> = Vec::with_capacity(cols.len());
    let mut x = br;
    let mut used = 0usize;
    for &w in &col_w {
        if x - w < bl && used > 0 {
            break;
        }
        let left = x - w;
        col_left.push(left);
        x = left - gap;
        used += 1;
        if used >= MAX_COLS {
            break;
        }
    }
    // 居中摆法：整块的右缘从 `br` 挪到「版心正中」。
    // 列宽逐列不同（公式列更宽），所以按**实际占用**算偏移，不用统一列距。
    if align == Align::Center && used > 0 {
        let right = col_left[0] + col_w[0];
        let left = col_left[used - 1];
        let total = right - left;
        let want_left = bl + (avail_w - total).max(0) / 2;
        let dx = want_left - left;
        for x in col_left.iter_mut().take(used) {
            *x += dx;
        }
    }
    // 公式整块比一列还高也算溢出（竖排里「一列的高度」就是版心高度）：
    // 不报的话它会直接画出窗口下缘，缩字阶梯也不会被触发。
    //
    // 但「溢出」不等于「砍了字」：`cut` 只认列被砍掉，`…` 只在真丢内容时补。
    let too_tall = atoms.iter().any(|a| atom_h(a) > avail_h);
    let cut = used < cols.len();
    let rows = cols
        .iter()
        .take(used)
        .map(|c| c.iter().map(|&j| atom_h(&atoms[j])).sum::<i32>())
        .max()
        .unwrap_or(0);
    let oy = bt + (avail_h - rows).max(0) / 2;

    let mut glyphs: Vec<Glyph> = Vec::new();
    let mut math: Vec<MathBlock> = Vec::new();
    let mut pending_brk = false;
    let mut col_ranges: Vec<(usize, usize)> = Vec::with_capacity(used);
    let mut col_x: Vec<(i32, i32)> = Vec::with_capacity(used);
    for (ci, col) in cols.iter().take(used).enumerate() {
        let (cx, cw) = (col_left[ci], col_w[ci]);
        let s0 = glyphs.len();
        let mut y = oy;
        for &i in col.iter() {
            let a = &atoms[i];
            match (a.ch, &a.mb) {
                (Some(ch), _) => {
                    let mut gx = cx + (cw - a.w).max(0) / 2;
                    let mut gy = y;
                    if is_vert_dot(ch) {
                        // 同上：按字宽而不是列宽偏移，否则公式列会把点号甩出去
                        gx += ((m.char_w as f32) * 0.55).round() as i32;
                        gy -= ((m.cell_h as f32) * 0.55).round() as i32;
                        gy = gy.max(bt);
                    }
                    glyphs.push(Glyph {
                        ch,
                        x: gx,
                        y: gy,
                        rot: is_vert_rot(ch),
                        src: a.src,
                        brk: pending_brk,
                    });
                    pending_brk = false;
                    y += cell;
                }
                (None, Some(mb)) => {
                    math.push(MathBlock {
                        x: cx + (cw - mb.h()).max(0) / 2,
                        y,
                        rot: true,
                        mb: mb.clone(),
                    });
                    pending_brk = true;
                    y += mb.w + spacing + breath;
                }
                (None, None) => {}
            }
        }
        col_ranges.push((s0, glyphs.len()));
        // 瓦片：相邻列在**列间空隙中点**相接；首列右缘、末列左缘各扩到版心边缘。
        // 无缝是关键 —— 逐列渐变按瓦片查带，掉进缝里的像素会被当成「无带」，
        // 取满亮度（症状是过渡时某几条竖线闪一下）。
        let left_edge = if ci + 1 < used {
            (col_left[ci + 1] + col_w[ci + 1] + cx) / 2
        } else {
            bl
        };
        let right_edge = if ci == 0 {
            br
        } else {
            (cx + cw + col_left[ci - 1]) / 2
        };
        col_x.push((left_edge, right_edge));
    }

    let mut ellipsis = false;
    if cut {
        if let Some(last) = glyphs.pop() {
            glyphs.push(Glyph {
                ch: ELLIPSIS,
                x: last.x,
                y: last.y,
                rot: false,
                src: u32::MAX,
                brk: false,
            });
        }
        ellipsis = true;
    }

    let mut ql = QuoteLayout {
        glyphs,
        lanes: used as i32,
        shrink: 100,
        overflow: cut || too_tall,
        ellipsis,
        col_ranges,
        col_x,
        math,
        extent: (0, 0),
    };
    ql.extent = rich_bounds(&ql, m);
    ql
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    /// 等宽假字：每个字步进 20px、字格 20px —— 测试只关心几何，不关心字形
    fn uni(n: usize) -> (Vec<char>, Vec<i32>) {
        (vec!['国'; n], vec![20; n])
    }

    fn metrics() -> Metrics {
        Metrics::new(20, 20)
    }

    fn band() -> (i32, i32, i32, i32) {
        (0, 0, 200, 200)
    }

    #[test]
    fn 横版长句必须折行() {
        let (ch, adv) = uni(30); // 30 字 × 20px = 600px，版心只有 200px
        let q = layout(&ch, &adv, metrics(), Orient::Horizontal, band(), 0);
        assert!(q.lanes >= 2, "30 字塞进 200px 应当折行，实际 {} 行", q.lanes);
        assert_eq!(q.glyphs.len(), 30, "200x200 够放 3 行以上，不该丢字");
        assert!(!q.ellipsis);
    }

    #[test]
    fn 横版单行无出处仍居中() {
        // 短句卡片感是旧行为里对的部分，保留：5 字 100px 在 200px 版心居中
        let (ch, adv) = uni(5);
        let q = layout(&ch, &adv, metrics(), Orient::Horizontal, band(), 0);
        assert_eq!(q.lanes, 1);
        assert_eq!(q.glyphs[0].x, 50);
    }

    #[test]
    fn 横版出处独立行右对齐正文左对齐() {
        // 正文 12 字 + 出处「——　老子」5 字。出处必须自成一行、右缘贴版心右缘；
        // 正文多行（含出处时）左对齐，不再逐行居中。
        let mut ch: Vec<char> = vec!['国'; 12];
        ch.extend("——　老子".chars());
        let adv = vec![20; ch.len()];
        let q = layout_with_attr(&ch, &adv, metrics(), Orient::Horizontal, band(), 0, Some(12));
        let attr: Vec<&Glyph> = q.glyphs.iter().filter(|g| g.src >= 12).collect();
        assert_eq!(attr.len(), 5, "出处 5 字一个不许丢");
        assert!(
            attr.windows(2).all(|w| w[0].y == w[1].y),
            "出处必须同一行"
        );
        let body_ys: Vec<i32> = q.glyphs.iter().filter(|g| g.src < 12).map(|g| g.y).collect();
        assert!(body_ys.iter().all(|&y| y != attr[0].y), "出处不得与正文同行");
        assert_eq!(attr.last().unwrap().x + 20, band().2, "出处右缘贴版心右缘");
        let body_first = q.glyphs.iter().find(|g| g.src < 12).unwrap();
        assert_eq!(body_first.x, band().0, "正文左对齐");
    }

    #[test]
    fn 横版孤行借字到四字() {
        // 22 字 → 贪心 [10,10,2]，末行两字是孤行；借字后末行 ≥ 4 字。
        let (ch, adv) = uni(22);
        let q = layout(&ch, &adv, metrics(), Orient::Horizontal, band(), 0);
        assert_eq!(q.lanes, 3);
        assert_eq!(q.glyphs.len(), 22, "借字不丢字");
        let last_y = q.glyphs.last().unwrap().y;
        let n_last = q.glyphs.iter().filter(|g| g.y == last_y).count();
        assert!(n_last >= 4, "孤行应借到 ≥4 字，实际 {}", n_last);
    }

    #[test]
    fn 横版放不下要截断并给省略号() {
        let (ch, adv) = uni(200); // 远超 200x200 的容量
        let q = layout(&ch, &adv, metrics(), Orient::Horizontal, band(), 0);
        assert!(q.overflow, "200 字不可能塞进 200x200");
        assert!(q.ellipsis, "截断必须给出省略号，不能静默消失");
        // 末字必须是省略号，且**一个字形都不许越出右缘**
        assert_eq!(q.glyphs.last().unwrap().ch, ELLIPSIS);
        for g in &q.glyphs {
            assert!(g.x + metrics().char_w <= band().2, "字形越过右缘: {:?}", g);
            assert!(g.y + metrics().char_h <= band().3, "字形越过下缘: {:?}", g);
        }
    }

    #[test]
    fn 版心极窄也不能丢掉全部内容() {
        // 退化版心（宽 1px）：这是旧实现里「整条消失」的那种输入
        let (ch, adv) = uni(6);
        let q = layout(&ch, &adv, metrics(), Orient::Horizontal, (10, 10, 11, 200), 0);
        assert!(!q.glyphs.is_empty(), "版心再窄也要留下字形（省略号兜底）");
    }

    // ------------------------------------------------------------
    // 非 BMP：这些字在 UTF-16 里占**两个**单元，在 UTF-32 里只占一个。
    //
    // 旧实现把整条串当 `&[u16]` 逐单元处理，于是「𠀋」被当成两个字：
    // 字形数量凭空翻倍，更糟的是步进数组（按 UTF-16 单元算）与字符数组
    // （按标量算）从第一个非 BMP 字起就**错位一格** —— 后面每个字都拿到
    // 邻居的宽度。症状是「某个字之后整行间距全乱」，且只在含生僻字/emoji
    // 的句子上出现，肉眼极难归因。
    //
    // 类型换成 `char` 之后这类错位在编译期就不成立了（`char` 表达不了孤立
    // 代理），但测试要钉住的是**行为**：一条串里有几个字，就出几个字形。
    // ------------------------------------------------------------

    #[test]
    fn 非bmp字符按标量计数不被拆成两个字形() {
        let text: Vec<char> = "𠀋国𠀋国".chars().collect();
        assert_eq!(text.len(), 4, "4 个标量（UTF-16 长度是 6，这正是旧实现的错源）");
        let adv = vec![20; text.len()];
        let q = layout(&text, &adv, metrics(), Orient::Horizontal, band(), 0);
        assert_eq!(q.glyphs.len(), 4, "4 个字就是 4 个字形，不是 6 个");
        assert_eq!(q.glyphs[0].ch, '𠀋');
        assert_eq!(q.glyphs[1].ch, '国');
        assert_eq!(q.glyphs[2].ch, '𠀋');
        // 步进没有错位：相邻字形 x 差恒为一个字宽（起点由居中算出，不假设 0）
        let xs: Vec<i32> = q.glyphs.iter().map(|g| g.x).collect();
        for w in xs.windows(2) {
            assert_eq!(w[1] - w[0], 20, "步进错位，x 序列 {:?}", xs);
        }
    }

    #[test]
    fn 非bmp与等宽bmp的版面几何完全一致() {
        // 这是比「数量对」更硬的判据：非 BMP 只影响编码长度，不影响版式。
        // 只要有一个字被多算了一次，「𠀋国𠀋国𠀋」和「国国国国国」的坐标
        // 就会从第一个非 BMP 字开始分叉。
        let text: Vec<char> = "𠀋国𠀋国𠀋".chars().collect();
        let ctrl: Vec<char> = "国".repeat(5).chars().collect();
        let adv = vec![20; 5];
        for orient in [Orient::Horizontal, Orient::Vertical] {
            let a = layout(&text, &adv, metrics(), orient, (0, 0, 200, 60), 0);
            let b = layout(&ctrl, &adv, metrics(), orient, (0, 0, 200, 60), 0);
            assert_eq!(a.glyphs.len(), b.glyphs.len(), "{:?} 字形数分叉", orient);
            assert_eq!(a.lanes, b.lanes, "{:?} 列/行数分叉", orient);
            for (g1, g2) in a.glyphs.iter().zip(b.glyphs.iter()) {
                assert_eq!(
                    (g1.x, g1.y, g1.rot),
                    (g2.x, g2.y, g2.rot),
                    "{:?} 第 {} 个字坐标分叉",
                    orient,
                    g1.src
                );
            }
        }
    }

    #[test]
    fn 竖版非bmp字符同样按标量切列() {
        let text: Vec<char> = "𠀋国𠀋国𠀋".chars().collect();
        let adv = vec![20; text.len()];
        let q = layout(&text, &adv, metrics(), Orient::Vertical, (0, 0, 200, 60), 0);
        // 旧实现会给出 8 个字形（3 个非 BMP 各算两次）
        assert_eq!(q.glyphs.len(), 5, "5 个字竖排也是 5 个字形");
        for g in &q.glyphs {
            assert!(g.y + metrics().char_h <= 60, "字形越过版心底缘: {:?}", g);
        }
    }

    #[test]
    fn 步进数组比字符数组短时返回空版面而不是越界() {
        // 上游若误把 UTF-16 长度当标量长度传下来（`adv` 只有一半），
        // 这里必须**安全退化**成空版面，而不是按错位的下标去读。
        let text: Vec<char> = "𠀋国𠀋国".chars().collect();
        let adv = vec![20; 3]; // 故意短于 chars
        let q = layout(&text, &adv, metrics(), Orient::Horizontal, band(), 0);
        assert!(q.glyphs.is_empty(), "长度不匹配时应放弃排版，不能带着错位下标硬排");
        assert_eq!(q.shrink, 100);
    }

    #[test]
    fn 行首禁则不被违反() {
        // 每行只能放 5 个字，第 6 个是「，」——它必须被挤在第 5 个后面，
        // 而不是掉到下一行开头。
        // 注意断言的是「**每行的第一个字**不是禁则字」，
        // 不是「禁则字不存在」—— 挤在行尾正是我们要的结果。
        let mut ch = vec!['国'; 12];
        ch[5] = '，';
        ch[11] = '，';
        let adv = vec![20; 12];
        let q = layout(&ch, &adv, metrics(), Orient::Horizontal, (0, 0, 100, 200), 0);
        let mut by_line: std::collections::BTreeMap<i32, char> = Default::default();
        for g in &q.glyphs {
            by_line.entry(g.y).or_insert(g.ch);
        }
        assert!(by_line.len() >= 2, "12 字 × 20px 塞进 100px 应当折行");
        for (_, first) in by_line {
            assert!(!no_start(first), "禁则字出现在行首");
        }
    }

    #[test]
    fn 行尾禁则被拉到下一行() {
        let mut ch = vec!['国'; 10];
        ch[4] = '《'; // 恰好落在第 4 位（第 5 个字）
        let adv = vec![20; 10];
        let q = layout(&ch, &adv, metrics(), Orient::Horizontal, (0, 0, 100, 200), 0);
        // 没有任何一行是拿「《」收尾的
        for g in &q.glyphs {
            let _ = g;
        }
        // 用行分组检查尾字
        let mut by_line: std::collections::BTreeMap<i32, Vec<char>> = Default::default();
        for g in &q.glyphs {
            by_line.entry(g.y).or_default().push(g.ch);
        }
        for (_, v) in by_line {
            if let Some(&last) = v.last() {
                assert!(!no_end(last), "禁则字出现在行尾");
            }
        }
    }

    #[test]
    fn 竖版列切分正确() {
        // 版心高 200、单元格高 21 → 每列容量 9；20 个字应当切成 3 列
        let (ch, adv) = uni(20);
        let q = layout(&ch, &adv, metrics(), Orient::Vertical, band(), 0);
        assert_eq!(q.lanes, 3, "20 字 / 每列 9 字 应当 3 列");
        assert_eq!(q.glyphs.len(), 20);
    }

    #[test]
    fn 竖版列序自右向左() {
        let (ch, adv) = uni(20);
        let q = layout(&ch, &adv, metrics(), Orient::Vertical, band(), 0);
        // 第 0 个字在第 0 列（最右）；第 9 个字应当在更靠左的列
        let x0 = q.glyphs[0].x;
        let x9 = q.glyphs[9].x;
        assert!(x9 < x0, "列序应当自右向左: 第0字 x={} 第9字 x={}", x0, x9);
        // 同一列内自上而下
        assert!(q.glyphs[1].y > q.glyphs[0].y);
    }

    #[test]
    fn 竖版标点归位到右上() {
        let mut ch = vec!['国'; 4];
        ch[2] = '，';
        let adv = vec![20; 4];
        let q = layout(&ch, &adv, metrics(), Orient::Vertical, band(), 0);
        let dot = q.glyphs[2];
        let prev = q.glyphs[1];
        // 相对于「它本该在的单元格」：上移（y 小于同列前一字的常规步进位置之上半格）
        let normal_y = prev.y + metrics().cell_h;
        assert!(
            dot.y < normal_y,
            "标点没有上移: dot.y={} normal_y={}",
            dot.y,
            normal_y
        );
        // 右移
        assert!(dot.x > prev.x, "标点没有右移");
    }

    #[test]
    fn 竖版断句优先收在句读后() {
        // 16 字对联：cap=9 的盲切是「箫鼓追随春社近，衣 / 冠简朴古风存。」
        // ——把「衣冠」劈开了；句读优先应当收在「，」后面 → [8, 8] 两列
        let text = "箫鼓追随春社近，衣冠简朴古风存。";
        let ch: Vec<char> = text.chars().collect();
        let adv = vec![20; ch.len()];
        let q = layout(&ch, &adv, metrics(), Orient::Vertical, band(), 0);
        assert_eq!(q.col_ranges.len(), 2, "应按句读断成两列");
        for (g0, g1) in &q.col_ranges {
            let last = q.glyphs[g1 - 1].ch;
            assert!(clause_end(last), "列末字不是句读: {:?}", last);
            assert!(!no_start(q.glyphs[*g0].ch), "列首出现禁则字");
        }
        assert_eq!(q.glyphs.len(), 16, "一个字都不许丢");
    }

    #[test]
    fn 竖版长句超过句读窗口时退回满切() {
        // 句读离 cap 边界超过 3 格时按容量硬切（古人雕版同理），但不许丢字
        let text = "国之大道生生不息天下为公大同小异和而不同";
        let ch: Vec<char> = text.chars().collect();
        let adv = vec![20; ch.len()];
        let q = layout(&ch, &adv, metrics(), Orient::Vertical, band(), 0);
        assert_eq!(q.glyphs.len(), 20);
        for &(g0, _) in &q.col_ranges {
            assert!(!no_start(q.glyphs[g0].ch), "列首出现禁则字");
        }
    }

    #[test]
    fn 字距进入行宽() {
        let (ch, adv) = uni(9);
        let tight = layout(&ch, &adv, metrics(), Orient::Horizontal, (0, 0, 400, 60), 0);
        let loose = layout(&ch, &adv, metrics(), Orient::Horizontal, (0, 0, 400, 60), 4);
        assert!(
            loose.extent.0 > tight.extent.0,
            "字距没有进入行宽: {} vs {}",
            tight.extent.0,
            loose.extent.0
        );
    }

    #[test]
    fn 竖版极窄版心也给省略号而不是空白() {
        let (ch, adv) = uni(60);
        let q = layout(&ch, &adv, metrics(), Orient::Vertical, (0, 0, 25, 200), 0);
        assert!(!q.glyphs.is_empty());
        assert!(q.ellipsis || q.glyphs.len() == 60);
    }

    #[test]
    fn 空输入不崩溃() {
        let q = layout(&[], &[], metrics(), Orient::Horizontal, band(), 0);
        assert!(q.is_empty());
        assert_eq!(q.shrink, 100);
    }

    #[test]
    fn 合批段不丢字也不跨行() {
        let (ch, adv) = uni(30);
        let q = layout(&ch, &adv, metrics(), Orient::Horizontal, band(), 0);
        let runs = q.runs();
        // 一个字都不能丢，也不能被算两遍
        let total: usize = runs.iter().map(|(_, n)| *n).sum();
        assert_eq!(total, q.glyphs.len(), "合批段必须完整覆盖定位表");
        assert_eq!(runs.iter().map(|(s, _)| *s).min(), Some(0));
        // 每段内部 src 必须严格连续 —— 这是「一次 TextOut 画出来位置不变」的前提
        for &(start, n) in &runs {
            for k in 1..n {
                assert_eq!(
                    q.glyphs[start + k].src,
                    q.glyphs[start + k - 1].src + 1,
                    "段内 src 不连续：一次 TextOut 会画错位置"
                );
                assert_eq!(q.glyphs[start + k].y, q.glyphs[start].y, "跨行被合进一段了");
            }
        }
        // 30 字 × 20px / 200px 宽 = 每行 10 字 → 3 行 3 段
        assert_eq!(runs.len() as i32, q.lanes, "每行应当正好一段");
        assert_eq!(runs.len(), 3);
    }

    #[test]
    fn 补出来的省略号自成一段() {
        let (ch, adv) = uni(200);
        let q = layout(&ch, &adv, metrics(), Orient::Horizontal, band(), 0);
        let runs = q.runs();
        let &(start, n) = runs.last().unwrap();
        assert_eq!(n, 1, "「…」不是原文里的字，必须单独画");
        assert_eq!(q.glyphs[start].ch, ELLIPSIS);
    }

    #[test]
    fn 旋转字不参与合批() {
        // 竖版下 `——` 要换字体对象，和普通字不能塞进同一次调用
        let mut ch = vec!['国'; 3];
        ch[1] = '—';
        let adv = vec![20; 3];
        let q = layout(&ch, &adv, metrics(), Orient::Vertical, band(), 0);
        assert!(q.glyphs[1].rot);
        for &(start, n) in &q.runs() {
            if n > 1 {
                for k in start..start + n {
                    assert!(!q.glyphs[k].rot, "旋转字被合进了多字段");
                }
            }
        }
    }

    #[test]
    fn 合批段内必须水平按步进相接() {
        // 这才是合批正确性的**真正前提**：段内除首字外的字交给 GDI 按步进推，
        // 必须落在排版算出的位置上。
        //
        // `同基线 + src 连续` 只是它的一个**充分**条件，而且在竖版极矮版心
        // （`cap == 1`，每列一字）下**不成立也会被满足**：相邻的**列**之间
        // 字落同一个 `y`、`src` 也连续，却被排在不同的 `x` 上（相差一个列距）。
        // 只看 `y`/`src` 就会把它们合进一段 —— `PolyTextOutW` 随即沿一条基线
        // 把整列画成一行。2026-10-15 实测：240x80 竖版合批 vs 逐字差 669px。
        let (ch, adv) = uni(12);
        // 版心宽要足够：列距是 1.70 字宽（= 34px），12 列需要 12×34 左右
        let q = layout(&ch, &adv, metrics(), Orient::Vertical, (0, 0, 500, 30), 0);
        assert!(q.col_ranges.len() > 1, "该版心下应当是「每列一字」");
        assert_eq!(q.col_ranges.len(), 12, "500px / 列距 34px 装得下 12 列，不该截断");
        for (s, n) in q.runs() {
            for k in 1..n {
                let (p, g) = (q.glyphs[s + k - 1], q.glyphs[s + k]);
                assert_eq!(
                    g.x,
                    p.x + adv[p.src as usize],
                    "段内 x 不按步进相接（段起点 {}，第 {} 字）：GDI 会画错位置",
                    s,
                    k
                );
            }
        }
    }

    #[test]
    fn 竖版极矮版心合批不跨列() {
        // `cap == 1` ⇒ 每列一字 ⇒ 每列必须**自成一段**（列内只有一个字）。
        // 这条与上一条同源，但直接钉住「不跨列」这个结论，用例更直白。
        //
        // 注意版心宽要**够放下全部字**：一旦触发截断，末字会换成省略号
        // （`src = u32::MAX`），恰好把合并挡住，用例就白测了。
        let (ch, adv) = uni(12);
        let q = layout(&ch, &adv, metrics(), Orient::Vertical, (0, 0, 500, 30), 0);
        assert_eq!(q.col_ranges.len(), 12, "12 字 / 每列 1 字 = 12 列");
        for (s, n) in q.runs() {
            assert_eq!(n, 1, "竖版每列一字时不许跨列合批，实际段 ({}, {})", s, n);
        }
    }
}
