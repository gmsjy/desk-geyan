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

// ---------------------------------------------------------------- 数据结构

/// 一个字符画在哪。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Glyph {
    pub ch: u16,
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
        self.glyphs.is_empty()
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
            cell_h: ((char_h as f32) * 1.05).round() as i32,
            col_pitch: ((char_w as f32) * 1.15).round() as i32,
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

const ELLIPSIS: u16 = 0x2026; // …

pub fn no_start(c: u16) -> bool {
    NO_START.contains(&c)
}
/// 句读收尾符：竖版断列优先收在它们**后面**（仿古排版「一句一列」的节奏）。
pub fn clause_end(c: u16) -> bool {
    matches!(
        c,
        0xFF0C | // ，
        0x3002 | // 。
        0x3001 | // 、
        0xFF1B | // ；
        0xFF1A | // ：
        0xFF1F | // ？
        0xFF01 | // ！
        0x2026 | // …
        0x300D | // 」
        0x300F | // 』
        0xFF09 // ）
    )
}
pub fn no_end(c: u16) -> bool {
    NO_END.contains(&c)
}
pub fn is_vert_dot(c: u16) -> bool {
    VERT_DOT.contains(&c)
}
fn is_vert_rot(c: u16) -> bool {
    VERT_ROT.contains(&c)
}

// ---------------------------------------------------------------- 主入口

/// 排一段文字。**纯函数**：同样的输入永远同样的输出，不碰任何全局状态。
///
/// * `chars` 是逐字符的码点（不含结尾 NUL），`adv[i]` 是第 i 个字**不含字距**的步进。
/// * `band` 是版心 `(左, 上, 右, 下)`，客户区坐标。
/// * `spacing` 是字距（px），横版加在字与字之间，竖版加在格与格之间。
pub fn layout(
    chars: &[u16],
    adv: &[i32],
    m: Metrics,
    orient: Orient,
    band: (i32, i32, i32, i32),
    spacing: i32,
) -> QuoteLayout {
    if chars.is_empty() || adv.len() < chars.len() {
        return QuoteLayout {
            shrink: 100,
            ..Default::default()
        };
    }
    match orient {
        Orient::Horizontal => layout_h(chars, adv, m, band, spacing),
        Orient::Vertical => layout_v(chars, adv, m, band, spacing),
    }
}

// ---------------------------------------------------------------- 横版

fn layout_h(
    chars: &[u16],
    adv: &[i32],
    m: Metrics,
    band: (i32, i32, i32, i32),
    spacing: i32,
) -> QuoteLayout {
    let (bl, bt, br, bb) = band;
    let avail_w = (br - bl).max(1);
    let avail_h = (bb - bt).max(1);
    let n = chars.len();

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

    // ---- ① 贪心折行（含行首 / 行尾禁则）
    let mut lines: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    for i in 0..n {
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

    // ---- ② 纵向容量：放不下就截断到最后一行 + 省略号
    let max_lanes = (avail_h / m.line_h.max(1)).max(1) as usize;
    let mut truncated = lines.len() > max_lanes;
    let used = lines.len().min(max_lanes);
    let ext_h = if used == 0 {
        0
    } else {
        (used as i32 - 1) * m.line_h + m.char_h
    };
    let oy = bt + (avail_h - ext_h) / 2;

    let mut glyphs: Vec<Glyph> = Vec::new();
    'outer: for (j, line) in lines.iter().take(used).enumerate() {
        let x0 = bl + (avail_w - line_w(line)) / 2;
        let mut x = x0;
        for (k, &i) in line.iter().enumerate() {
            if k > 0 {
                x += spacing;
            }
            // 最后一行的字若越过右缘，就地截断（禁则带出来的那一字宽的溢出）
            if j + 1 == used && x + adv[i] > br && !glyphs.is_empty() {
                truncated = true;
                break 'outer;
            }
            glyphs.push(Glyph {
                ch: chars[i],
                x,
                y: oy + j as i32 * m.line_h,
                rot: false,
                src: i as u32,
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
            });
        }
        ellipsis = true;
    }

    let (ext_w, ext_h) = bounds(&glyphs, m);
    QuoteLayout {
        glyphs,
        lanes: used as i32,
        extent: (ext_w, ext_h),
        shrink: 100,
        overflow: truncated,
        ellipsis,
        col_ranges: Vec::new(),
        col_x: Vec::new(),
    }
}

// ---------------------------------------------------------------- 竖版

fn layout_v(
    chars: &[u16],
    adv: &[i32],
    m: Metrics,
    band: (i32, i32, i32, i32),
    spacing: i32,
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
    let max_cols = (avail_w / m.col_pitch.max(1)).max(1) as usize;
    let truncated = cols.len() > max_cols;
    let used = cols.len().min(max_cols);
    let rows = cols.iter().take(used).map(|c| c.len()).max().unwrap_or(0) as i32;
    let ext_h = if rows == 0 {
        0
    } else {
        (rows - 1) * cell + m.char_h
    };
    let oy = bt + (avail_h - ext_h) / 2;
    // 第 0 列贴版心右缘（右起），于是整块天然右对齐 —— 条幅的读法
    let x_r = br - m.char_w;

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
                gx += ((m.col_pitch as f32) * 0.55).round() as i32;
                gy -= ((m.cell_h as f32) * 0.55).round() as i32;
                gy = gy.max(bt);
            }
            glyphs.push(Glyph {
                ch,
                x: gx,
                y: gy,
                rot: is_vert_rot(ch),
                src: i as u32,
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
            });
        }
        ellipsis = true;
    }

    let (ext_w, ext_h) = bounds(&glyphs, m);
    let _ = avail_w;

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

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    /// 等宽假字：每个字步进 20px、字格 20px —— 测试只关心几何，不关心字形
    fn uni(n: usize) -> (Vec<u16>, Vec<i32>) {
        (vec![0x56FD; n], vec![20; n]) // 0x56FD = 国
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

    #[test]
    fn 行首禁则不被违反() {
        // 每行只能放 5 个字，第 6 个是「，」——它必须被挤在第 5 个后面，
        // 而不是掉到下一行开头。
        // 注意断言的是「**每行的第一个字**不是禁则字」，
        // 不是「禁则字不存在」—— 挤在行尾正是我们要的结果。
        let mut ch = vec![0x56FD; 12];
        ch[5] = 0xFF0C; // ，
        ch[11] = 0xFF0C;
        let adv = vec![20; 12];
        let q = layout(&ch, &adv, metrics(), Orient::Horizontal, (0, 0, 100, 200), 0);
        let mut by_line: std::collections::BTreeMap<i32, u16> = Default::default();
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
        let mut ch = vec![0x56FD; 10];
        ch[4] = 0x300A; // 《  —— 恰好落在第 4 位（第 5 个字）
        let adv = vec![20; 10];
        let q = layout(&ch, &adv, metrics(), Orient::Horizontal, (0, 0, 100, 200), 0);
        // 没有任何一行是拿「《」收尾的
        for g in &q.glyphs {
            let _ = g;
        }
        // 用行分组检查尾字
        let mut by_line: std::collections::BTreeMap<i32, Vec<u16>> = Default::default();
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
        let mut ch = vec![0x56FD; 4];
        ch[2] = 0xFF0C; // ，
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
        let ch: Vec<u16> = text.encode_utf16().collect();
        let adv = vec![20; ch.len()];
        let q = layout(&ch, &adv, metrics(), Orient::Vertical, band(), 0);
        assert_eq!(q.col_ranges.len(), 2, "应按句读断成两列");
        for (g0, g1) in &q.col_ranges {
            let last = q.glyphs[g1 - 1].ch;
            assert!(clause_end(last), "列末字不是句读: {:04X}", last);
            assert!(!no_start(q.glyphs[*g0].ch), "列首出现禁则字");
        }
        assert_eq!(q.glyphs.len(), 16, "一个字都不许丢");
    }

    #[test]
    fn 竖版长句超过句读窗口时退回满切() {
        // 句读离 cap 边界超过 3 格时按容量硬切（古人雕版同理），但不许丢字
        let text = "国之大道生生不息天下为公大同小异和而不同";
        let ch: Vec<u16> = text.encode_utf16().collect();
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
        let mut ch = vec![0x56FD; 3];
        ch[1] = 0x2014; // —
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
        let q = layout(&ch, &adv, metrics(), Orient::Vertical, (0, 0, 300, 30), 0);
        assert!(q.col_ranges.len() > 1, "该版心下应当是「每列一字」");
        assert_eq!(q.col_ranges.len(), 12, "300px / 列距 23px 装得下 12 列，不该截断");
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
        let q = layout(&ch, &adv, metrics(), Orient::Vertical, (0, 0, 300, 30), 0);
        assert_eq!(q.col_ranges.len(), 12, "12 字 / 每列 1 字 = 12 列");
        for (s, n) in q.runs() {
            assert_eq!(n, 1, "竖版每列一字时不许跨列合批，实际段 ({}, {})", s, n);
        }
    }
}
