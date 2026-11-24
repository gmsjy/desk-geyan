//! 数理公式排版：**高中物理 / 数学**子集。
//!
//! # 为什么自己写，而不是接 typst / MathJax
//!
//! 见 `docs/` 里的取舍讨论，结论只有三条：
//!
//!   * **竖排**是这个挂件的招牌，而 typst 到 0.15（2026-06）都没实现直排
//!     （官方 issue 仍开着，社区包 `vtzone` 自称「功能有限、质量可疑」）。
//!     引进来也接管不了 `layout_v` 那套按列切分。
//!   * **体积**：typst + 字体是十几到几十 MB，而这个 exe 现在才 717 KiB。
//!   * **字体割裂**：typst 用自带数学字体，跟用户在设置面板里选的中文字体对不上。
//!
//! 所以这里只做**结构**，不做通用排版：高中阶段的公式能用的结构是可枚举的
//! （上下标、分式、根号、求和上下限、希腊字母、矢量箭头），砍掉矩阵、
//! 多行对齐、可伸缩定界符之后，剩下的就是一个「递归量盒子」的活。
//!
//! # 和现有渲染管线的关系
//!
//! **不另起一条渲染路径。** 这里产出的 `MBox` 跟 `layout::QuoteLayout` 是
//! 同一类东西 —— 一张「每个字画在哪」的表，只是多了三种辅助图元：
//! 分式横线、根号上横线、矢量箭头。将来接进格言渲染时：
//!
//! ```text
//!   "……能量 E=mc^2 ……"  ──解析──▶  Node
//!                                    │ layout()
//!                                    ▼
//!                              MBox（字 + 线 + 箭头）
//!                                    │ 展开成 Glyph
//!                                    ▼
//!                    layout::Glyph 表（variant 区分上标/下标/横线）
//!                                    │
//!                                    ▼
//!              gfx::draw_glyphs（合批、阴影、竖版、DPI 全部复用）
//! ```
//!
//! # 坐标系
//!
//! `MBox` 里**所有 `y` 都是相对基线的偏移，向上为负**，`x` 相对盒子左缘。
//! 这是盒模型唯一自然的写法（上标就是「抬起来」），换算成 `TextOutW` 的
//! 「单元格左上角」只发生在 `Item::Glyph.dy` 这一处 —— 排版时就把字体的
//! `ascent` 减进去了，于是**绘制方不需要再碰任何度量**。
//!
//! # 纯函数
//!
//! `layout()` 只吃 `&mut dyn Measure`（量字宽 / 上下高），不碰窗口、不建 DC。
//! 测试里用 `FakeMeasure`（汉字 1em、拉丁 0.55em）就能验几何，
//! 不需要真的 GDI —— 和 `layout.rs` 同一个理由：可测。

use std::cell::RefCell;
use std::collections::HashMap;

use windows_sys::Win32::Foundation::{POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontW, CreateSolidBrush, DeleteObject, FillRect, GetGlyphOutlineW, GetTextExtentPoint32W,
    GetTextMetricsW, Polygon, SelectObject, SetBkMode, SetTextCharacterExtra, SetTextColor,
    TextOutW, FIXED, GDI_ERROR, GGO_METRICS, GLYPHMETRICS, HDC, HFONT, HGDIOBJ, MAT2,
    TEXTMETRICW,
};

use crate::consts::{
    ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, FF_DONTCARE, FW_NORMAL,
    OUT_TT_PRECIS, TRANSPARENT,
};

// ---------------------------------------------------------------- 度量抽象

/// 一段文字在某个字号下的度量。`ascent` / `descent` 相对基线。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextMetric {
    pub w: i32,
    pub ascent: i32,
    pub descent: i32,
}

/// 一个字形**真实墨迹**的范围，相对基线、向上为负。
///
/// 为什么不能拿 `ascent/descent` 代替：字体的上下高是**行度量**（给一整行留位），
/// 具体某个字的墨迹可能只占其中一半。`√` 就是最典型的例子 —— 它的墨迹远比
/// 字符格低，按 `ascent` 摆会整根掉到被开方内容下面（第一版就是这么错的）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ink {
    /// 墨迹上缘（负数，越负越高）
    pub top: i32,
    /// 墨迹下缘（正数，越正越低）
    pub bottom: i32,
    /// 墨迹宽度
    pub w: i32,
    /// 字符步进（排版推进量）
    pub adv: i32,
}

/// 量字。测试用假的，生产用 GDI 的（`GdiMeasure`）。
///
/// 之所以抽成 trait：`layout()` 想当纯函数，「字有多宽」是它唯一的外部依赖。
pub trait Measure {
    fn text(&self, s: &[char], px: i32) -> TextMetric;
    /// 单字墨迹框。默认实现按行度量估（测试够用），GDI 版走 `GetGlyphOutlineW`。
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

/// 给单元测试用的假度量：汉字 = 1em 见方，拉丁 = 0.55em 宽，
/// 上高 0.80em、下深 0.20em。数值不重要，**可比**才重要。
#[cfg(test)]
struct FakeMeasure;

#[cfg(test)]
impl Measure for FakeMeasure {
    fn text(&self, s: &[char], px: i32) -> TextMetric {
        let mut w = 0i32;
        for &c in s {
            let full = (c as u32) >= 0x2E80; // 粗略：CJK / 全角区
            w += if full {
                px
            } else {
                ((px as f32) * 0.55).round() as i32
            };
        }
        TextMetric {
            w,
            ascent: ((px as f32) * 0.80).round() as i32,
            descent: ((px as f32) * 0.20).round() as i32,
        }
    }
}

// ---------------------------------------------------------------- 语法树

/// 解析出来的公式树。叶子是字符，内部节点是**结构**。
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// 横排串联
    Row(Vec<Node>),
    /// 一个字符（中文、拉丁、希腊字母、运算符都走这里）
    Char(char),
    /// 显式空白，单位是 1/1000 em（`\,` = 160）
    Space(i32),
    /// 上标
    Sup(Box<Node>, Box<Node>),
    /// 下标
    Sub(Box<Node>, Box<Node>),
    /// 同时有上下标（`v_0^2`）—— **两者并排**，不是嵌套
    SupSub(Box<Node>, Box<Node>, Box<Node>),
    /// 分式
    Frac(Box<Node>, Box<Node>),
    /// 根号（上横线自动伸缩）
    Sqrt(Box<Node>),
    /// 矢量：内容上方一条箭头
    Vec(Box<Node>),
    /// 大算符（`\sum` / `\lim`）：上下限放在**符号上下方**，不是右侧
    BigOp {
        ch: char,
        sub: Option<Box<Node>>,
        sup: Option<Box<Node>>,
    },
    /// 自动缩放的定界符：`\left( ... \right)`。
    ///
    /// `None` 表示 `\left.` / `\right.` 那种「这一侧不画」的空定界符。
    /// 括号按**内容高度**选字号（和根号同一套探针），不用字体的可伸缩变体。
    Delim {
        open: Option<char>,
        close: Option<char>,
        body: Box<Node>,
    },
    /// 省略号。`mid` 为真时点在**数学轴**上（`\cdots`），否则坐在基线上（`\ldots`）。
    Dots { mid: bool },
}

// ---------------------------------------------------------------- 解析

/// 解析错误。带位置，报错时能指出是哪一列 —— 词库里一条写错的公式
/// 不该让整个渲染路径 panic。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub pos: usize,
    pub msg: &'static str,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "第 {} 个字符：{}", self.pos + 1, self.msg)
    }
}

/// 把 LaTeX 子集解析成 `Node`。
///
/// **不做**的事（都是高中的边界，写下来防止范围悄悄膨胀）：
/// 矩阵 / 行列式、多行对齐（`aligned`）、`\overbrace` / `\underbrace`、
/// 积分上下限。
///
/// `\left...\right` 支持，但括号是**等比放大**的（见 `Delim`），
/// 不是字体的可伸缩变体 —— 又高又窄的定界符会显得笔画偏重。
pub fn parse(src: &str) -> Result<Node, ParseError> {
    let cs: Vec<char> = src.chars().collect();
    let mut p = Parser { cs: &cs, i: 0 };
    let row = p.row(false)?;
    if p.i < p.cs.len() {
        // `row(false)` 只在遇到 `}` 时提前停下；到这里说明多了一个右括号
        return Err(ParseError {
            pos: p.i,
            msg: "多余的 }",
        });
    }
    Ok(row)
}

/// 解析失败时的兜底：整串当普通文字。
///
/// 这是**刻意**的降级方向 —— 词库里一条公式写错，正确表现是「难看但能看」，
/// 而不是「这一屏什么都不画」。
pub fn parse_or_text(src: &str) -> Node {
    match parse(src) {
        Ok(n) => n,
        Err(_) => Node::Row(src.chars().map(Node::Char).collect()),
    }
}

struct Parser<'a> {
    cs: &'a [char],
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<char> {
        self.cs.get(self.i).copied()
    }

    /// 读一段「行」：一直读到 `}`（`in_group` 为真时）或串尾。
    fn row(&mut self, in_group: bool) -> Result<Node, ParseError> {
        let mut out: Vec<Node> = Vec::new();
        loop {
            match self.peek() {
                None => {
                    if in_group {
                        return Err(ParseError {
                            pos: self.i,
                            msg: "缺少 }",
                        });
                    }
                    break;
                }
                Some('}') => {
                    if in_group {
                        break;
                    }
                    return Err(ParseError {
                        pos: self.i,
                        msg: "多余的 }",
                    });
                }
                Some(_) => out.push(self.atom()?),
            }
        }
        if out.len() == 1 {
            Ok(out.pop().unwrap())
        } else {
            Ok(Node::Row(out))
        }
    }

    /// 一个「原子 + 它的上下标」。
    fn atom(&mut self) -> Result<Node, ParseError> {
        let base = self.base()?;
        self.scripts(base)
    }

    fn base(&mut self) -> Result<Node, ParseError> {
        let Some(c) = self.peek() else {
            return Ok(Node::Row(Vec::new()));
        };
        match c {
            '{' => {
                self.i += 1;
                let inner = self.row(true)?;
                self.expect('}')?;
                Ok(inner)
            }
            '\\' => self.command(),
            // 空格在数学模式下无意义（间距由运算符自动给）
            ' ' | '\t' | '\n' | '\r' => {
                self.i += 1;
                Ok(Node::Space(0))
            }
            // 数学里的减号是 U+2212（更长、与 `+` 等宽），不是 ASCII 连字符。
            // 直接用 '-' 排出来会像一个小横杠，`-b` 看着像连字符而不是负号。
            '-' => {
                self.i += 1;
                Ok(Node::Char('\u{2212}'))
            }
            // 裸露的 ^ / _ 前面没有基底：按字面画出来，不报错
            '^' | '_' => {
                self.i += 1;
                Ok(Node::Char(c))
            }
            _ => {
                self.i += 1;
                Ok(Node::Char(c))
            }
        }
    }

    /// 吞掉紧随其后的 `_` / `^`。大算符的上下限落到符号上下方，其余走上下标。
    fn scripts(&mut self, base: Node) -> Result<Node, ParseError> {
        let mut sub: Option<Box<Node>> = None;
        let mut sup: Option<Box<Node>> = None;
        loop {
            match self.peek() {
                Some('_') if sub.is_none() => {
                    self.i += 1;
                    sub = Some(Box::new(self.group()?));
                }
                Some('^') if sup.is_none() => {
                    self.i += 1;
                    sup = Some(Box::new(self.group()?));
                }
                _ => break,
            }
        }
        Ok(match (sub, sup, base) {
            (None, None, b) => b,
            // 大算符：上下限摆到符号正上 / 正下
            (s, p, Node::BigOp { ch, .. }) => Node::BigOp { ch, sub: s, sup: p },
            (Some(s), None, b) => Node::Sub(Box::new(b), s),
            (None, Some(p), b) => Node::Sup(Box::new(b), p),
            (Some(s), Some(p), b) => Node::SupSub(Box::new(b), s, p),
        })
    }

    /// 上标 / 下标的操作数：`{...}` 或单个字符。
    fn group(&mut self) -> Result<Node, ParseError> {
        match self.peek() {
            Some('{') => {
                self.i += 1;
                let inner = self.row(true)?;
                self.expect('}')?;
                Ok(inner)
            }
            Some(_) => self.base(),
            None => Err(ParseError {
                pos: self.i,
                msg: "缺少上下标内容",
            }),
        }
    }

    fn expect(&mut self, want: char) -> Result<(), ParseError> {
        if self.peek() == Some(want) {
            self.i += 1;
            Ok(())
        } else {
            Err(ParseError {
                pos: self.i,
                msg: "缺少 }",
            })
        }
    }

    /// `\` 开头的命令。
    fn command(&mut self) -> Result<Node, ParseError> {
        let start = self.i;
        self.i += 1; // 吃掉 '\'
        let Some(c) = self.peek() else {
            return Err(ParseError {
                pos: start,
                msg: "\\ 后面没有命令",
            });
        };
        // 单字符命令：\, \; \! \  \{ \} \%
        match c {
            ',' => {
                self.i += 1;
                return Ok(Node::Space(160));
            }
            ';' => {
                self.i += 1;
                return Ok(Node::Space(280));
            }
            '!' => {
                self.i += 1;
                return Ok(Node::Space(-80));
            }
            ' ' => {
                self.i += 1;
                return Ok(Node::Space(250));
            }
            '{' | '}' | '%' | '#' | '&' | '_' | '^' => {
                self.i += 1;
                return Ok(Node::Char(c));
            }
            _ => {}
        }
        // 多字符命令：读到非字母为止
        let name_start = self.i;
        while matches!(self.peek(), Some(ch) if ch.is_ascii_alphabetic()) {
            self.i += 1;
        }
        if self.i == name_start {
            return Err(ParseError {
                pos: start,
                msg: "无法识别的命令",
            });
        }
        let name: String = self.cs[name_start..self.i].iter().collect();
        match name.as_str() {
            // ---- 结构 ----
            "frac" => {
                let a = self.brace_arg()?;
                let b = self.brace_arg()?;
                Ok(Node::Frac(Box::new(a), Box::new(b)))
            }
            "sqrt" => Ok(Node::Sqrt(Box::new(self.brace_arg()?))),
            "left" => self.delim(),
            "right" => Err(ParseError {
                pos: start,
                msg: "\\right 前面没有配对的 \\left",
            }),
            "vec" => Ok(Node::Vec(Box::new(self.brace_arg()?))),
            "sum" => Ok(Node::BigOp {
                ch: '∑',
                sub: None,
                sup: None,
            }),
            "lim" => Ok(Node::BigOp {
                ch: 'l',
                sub: None,
                sup: None,
            }),
            // 空白
            "quad" => Ok(Node::Space(1000)),
            "qquad" => Ok(Node::Space(2000)),
            // 省略号：`\cdots` 在数学轴上（`1+2+\cdots+n` 用它），
            // `\ldots` / `\dots` 坐在基线上。两者位置不同，不能混成一个。
            "cdots" => Ok(Node::Dots { mid: true }),
            "ldots" | "dots" => Ok(Node::Dots { mid: false }),
            // 函数名：直立排成一行（数学里的 sin / cos 是正体，不是斜体变量）
            "sin" | "cos" | "tan" | "cot" | "sec" | "csc" | "log" | "ln" | "lg" | "max"
            | "min" | "exp" => Ok(Node::Row(name.chars().map(Node::Char).collect())),
            // ---- 符号 / 希腊字母 ----
            other => match symbol(other) {
                Some(ch) => Ok(Node::Char(ch)),
                None => Err(ParseError {
                    pos: start,
                    msg: "未知命令（本子集不支持）",
                }),
            },
        }
    }

    /// `\left( ... \right)`：读到配对的 `\right` 为止。
    ///
    /// 定界符本身**不参与** `atom` 的上下标规则 —— `\left(...\right)^n`
    /// 的 `^n` 挂在整块上（`atom` 会接着调 `scripts`），这正是想要的。
    fn delim(&mut self) -> Result<Node, ParseError> {
        let open = self.delim_char()?;
        let mut out: Vec<Node> = Vec::new();
        loop {
            if self.at_right() {
                self.i += 6; // 吃掉 `\right`
                let close = self.delim_char()?;
                let body = match out.len() {
                    0 => Node::Row(Vec::new()),
                    1 => out.pop().unwrap(),
                    _ => Node::Row(out),
                };
                return Ok(Node::Delim {
                    open,
                    close,
                    body: Box::new(body),
                });
            }
            match self.peek() {
                None => {
                    return Err(ParseError {
                        pos: self.i,
                        msg: "缺少 \\right",
                    })
                }
                // `}` 意味着外层花括号先闭合了，说明这一对 `\left` 没写完
                Some('}') => {
                    return Err(ParseError {
                        pos: self.i,
                        msg: "缺少 \\right",
                    })
                }
                _ => out.push(self.atom()?),
            }
        }
    }

    /// 当前位置是不是 `\right`（而不是 `\rightarrow` 之类的前缀）。
    fn at_right(&self) -> bool {
        const NEEDED: [char; 6] = ['\\', 'r', 'i', 'g', 'h', 't'];
        if self.cs.len() < self.i + 6 || self.cs[self.i..self.i + 6] != NEEDED {
            return false;
        }
        // 命令名读到非字母为止：`\rightX` 里 `X` 是字母就说明是另一个命令
        !matches!(self.cs.get(self.i + 6), Some(c) if c.is_ascii_alphabetic())
    }

    /// `\left` / `\right` 后面的那个定界符。`.` 与 `\.` 表示「这一侧不画」。
    fn delim_char(&mut self) -> Result<Option<char>, ParseError> {
        let err = |pos: usize| ParseError {
            pos,
            msg: "定界符无法识别（可用 ( ) [ ] { } | < > / .）",
        };
        let start = self.i;
        let c = match self.peek() {
            Some('\\') => {
                self.i += 1;
                match self.peek() {
                    Some('{') => '{',
                    Some('}') => '}',
                    // `\|` 是双竖线（范数），单竖线直接写 `|`
                    Some('|') => '‖',
                    Some('.') => '.',
                    _ => return Err(err(start)),
                }
            }
            Some(c) => c,
            None => return Err(err(start)),
        };
        if !"()[]{}|‖<>/.".contains(c) {
            return Err(err(start));
        }
        self.i += 1;
        Ok(if c == '.' { None } else { Some(c) })
    }

    /// 结构命令的必选参数：`{...}`；也容忍裸单字（`\frac12`）。
    fn brace_arg(&mut self) -> Result<Node, ParseError> {
        match self.peek() {
            Some('{') => {
                self.i += 1;
                let inner = self.row(true)?;
                self.expect('}')?;
                Ok(inner)
            }
            Some(_) => self.base(),
            None => Err(ParseError {
                pos: self.i,
                msg: "缺少参数",
            }),
        }
    }
}

/// 命令 → 字符。表里没有的一律报错，**不猜**。
fn symbol(name: &str) -> Option<char> {
    Some(match name {
        // 希腊字母（小写）
        "alpha" => 'α',
        "beta" => 'β',
        "gamma" => 'γ',
        "delta" => 'δ',
        "epsilon" => 'ε',
        "zeta" => 'ζ',
        "eta" => 'η',
        "theta" => 'θ',
        "iota" => 'ι',
        "kappa" => 'κ',
        "lambda" => 'λ',
        "mu" => 'μ',
        "nu" => 'ν',
        "xi" => 'ξ',
        "pi" => 'π',
        "rho" => 'ρ',
        "sigma" => 'σ',
        "tau" => 'τ',
        "upsilon" => 'υ',
        "phi" => 'φ',
        "chi" => 'χ',
        "psi" => 'ψ',
        "omega" => 'ω',
        // 希腊字母（大写）
        "Gamma" => 'Γ',
        "Delta" => 'Δ',
        "Theta" => 'Θ',
        "Lambda" => 'Λ',
        "Xi" => 'Ξ',
        "Pi" => 'Π',
        "Sigma" => 'Σ',
        "Phi" => 'Φ',
        "Psi" => 'Ψ',
        "Omega" => 'Ω',
        // 运算符与关系符
        "cdot" => '·',
        "times" => '×',
        "div" => '÷',
        "pm" => '±',
        "mp" => '∓',
        "le" | "leq" => '≤',
        "ge" | "geq" => '≥',
        "ne" | "neq" => '≠',
        "approx" => '≈',
        "propto" => '∝',
        "infty" => '∞',
        "to" | "rightarrow" => '→',
        "degree" => '°',
        "prime" => '′',
        "angle" => '∠',
        "perp" => '⊥',
        "parallel" => '∥',
        "triangle" => '△',
        "therefore" => '∴',
        "because" => '∵',
        "in" => '∈',
        "subset" => '⊂',
        "cup" => '∪',
        "cap" => '∩',
        "int" => '∫',
        "partial" => '∂',
        "nabla" => '∇',
        "hbar" => 'ℏ',
        _ => return None,
    })
}

// ---------------------------------------------------------------- 盒子

/// 布局出来的一块「墨迹」。`y` 相对基线、**向上为负**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// 一个字。`dy` 已经减掉了该字号的 `ascent`，
    /// 所以直接 `TextOutW(dx, dy)` 就是对的 —— 绘制方不必再查度量。
    Glyph { ch: char, dx: i32, dy: i32, px: i32 },
    /// 横线（分式横线 / 根号上横线）。`thick` 是粗细，至少 1px。
    Rule {
        dx0: i32,
        dx1: i32,
        dy: i32,
        thick: i32,
    },
    /// 矢量箭头：杆 + 右向箭头。`dy` 是**杆的中线**。
    Arrow {
        dx0: i32,
        dx1: i32,
        dy: i32,
        thick: i32,
    },
    /// 省略号：三个方点，`dx` 是起点、`dy` 是点的**垂直中心**。
    ///
    /// 不画成字符：`⋯`（U+22EF）在中文字体里普遍缺字形，GDI 又没有
    /// 按字形回退的习惯 —— 落到屏幕上就是三个方框。三个 `FillRect`
    /// 一点不吃字体的饭，横竖版还都一致。
    Dots {
        dx: i32,
        dy: i32,
        gap: i32,
        size: i32,
    },
}

impl Item {
    /// 整体横向/纵向平移。
    ///
    /// 抽成方法而不是让每个 `layout_*` 各写一遍 match：图元每加一种，
    /// 所有平移点都得跟着改，漏一处就是「公式莫名画歪」——
    /// 而且歪的地方还不在报错里。
    fn shift_x(&mut self, dx: i32) {
        match self {
            Item::Glyph { dx: x, .. } | Item::Dots { dx: x, .. } => *x += dx,
            Item::Rule { dx0, dx1, .. } | Item::Arrow { dx0, dx1, .. } => {
                *dx0 += dx;
                *dx1 += dx;
            }
        }
    }

    fn shift_y(&mut self, dy: i32) {
        match self {
            Item::Glyph { dy: y, .. }
            | Item::Rule { dy: y, .. }
            | Item::Arrow { dy: y, .. }
            | Item::Dots { dy: y, .. } => *y += dy,
        }
    }
}

/// 一个盒子的排版结果。
///
/// `ascent` / `descent` 是墨迹相对基线的上下界（都是正数），
/// 盒子总高 = `ascent + descent`，总宽 = `w`。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MBox {
    pub w: i32,
    pub ascent: i32,
    pub descent: i32,
    pub items: Vec<Item>,
}

impl MBox {
    fn empty() -> MBox {
        MBox::default()
    }

    /// 把整块沿 x 平移（组合子节点时用）
    fn shift(&self, dx: i32) -> MBox {
        let items = self
            .items
            .iter()
            .map(|it| {
                let mut t = it.clone();
                t.shift_x(dx);
                t
            })
            .collect();
        MBox {
            w: self.w,
            ascent: self.ascent,
            descent: self.descent,
            items,
        }
    }

    /// 盒子高度（墨迹总高）
    pub fn h(&self) -> i32 {
        self.ascent + self.descent
    }
}

// ---------------------------------------------------------------- 布局

/// 上下标字号（相对正文）。0.62 是 TeX 的 `\scriptstyle` 那档。
const SCRIPT_SCALE: f32 = 0.62;
/// 字号下限。**不能再小**：再小汉字的笔画会糊成一团，下标里的「初」「合」就认不出了。
const SCRIPT_MIN_PX: i32 = 7;
/// 上标基线上抬（相对正文 em）
const SUP_RAISE: f32 = 0.42;
/// 下标基线下降
const SUB_DROP: f32 = 0.22;
/// 分式横线离基线的「轴高」
const AXIS: f32 = 0.28;
/// 分子 / 分母离横线的空隙。
///
/// 0.18 而不是更小：分子常带**下标**（`m_1 m_2`），下标垂到基线以下，
/// 缝隙一窄横线就压在下标上，看起来像「分子被划掉」。
const FRAC_GAP: f32 = 0.18;
/// 分子分母左右留白
const FRAC_PAD: f32 = 0.12;
/// 分子分母的字号比例。
///
/// 内联公式里**不能**用全字号：`\frac{1}{2}` 按 44px 全字号排出来总高 148px，
/// 是正文的 3.4 倍，撑得整行都散了。TeX 内联档本来就降到 `\scriptstyle`，
/// 这里取 0.82 —— 比 0.7 大一点，是为了让分母里的汉字（如 `F_合/S`）还认得出来。
const FRAC_SCALE: f32 = 0.82;

fn script_px(px: i32) -> i32 {
    (((px as f32) * SCRIPT_SCALE).round() as i32).max(SCRIPT_MIN_PX)
}

/// 递归布局。`px` 是这一层的字号。
pub fn layout(node: &Node, px: i32, m: &mut dyn Measure) -> MBox {
    match node {
        Node::Char(ch) => glyph(*ch, px, m),
        Node::Space(k) => MBox {
            w: ((px as f32) * (*k as f32) / 1000.0).round() as i32,
            ..MBox::empty()
        },
        Node::Row(kids) => layout_row(kids, px, m),
        Node::Sup(base, sup) => layout_scripts(base, None, Some(sup), px, m),
        Node::Sub(base, sub) => layout_scripts(base, Some(sub), None, px, m),
        Node::SupSub(base, sub, sup) => layout_scripts(base, Some(sub), Some(sup), px, m),
        Node::Frac(num, den) => layout_frac(num, den, px, m),
        Node::Sqrt(x) => layout_sqrt(x, px, m),
        Node::Vec(x) => layout_vec(x, px, m),
        Node::BigOp { ch, sub, sup } => layout_bigop(*ch, sub.as_deref(), sup.as_deref(), px, m),
        Node::Delim { open, close, body } => layout_delim(*open, *close, body, px, m),
        Node::Dots { mid } => layout_dots(*mid, px, m),
    }
}

fn glyph(ch: char, px: i32, m: &mut dyn Measure) -> MBox {
    let t = m.text(&[ch], px);
    MBox {
        w: t.w,
        ascent: t.ascent,
        descent: t.descent,
        // dy = -ascent：把「基线坐标」换成「单元格顶坐标」
        items: vec![Item::Glyph {
            ch,
            dx: 0,
            dy: -t.ascent,
            px,
        }],
    }
}

fn layout_row(kids: &[Node], px: i32, m: &mut dyn Measure) -> MBox {
    let mut out = MBox::empty();
    let mut x = 0;
    let mut prev: Option<char> = None;
    let mut started = false;
    for kid in kids {
        // 纯空白节点（源码里的空格）不产生间距，也不参与运算符判定
        if matches!(kid, Node::Space(0)) {
            continue;
        }
        let b = layout(kid, px, m);
        let first = first_char(kid);
        if started {
            x += op_gap(prev, first, px);
        }
        let shifted = b.shift(x);
        out.items.extend(shifted.items);
        x += b.w;
        out.ascent = out.ascent.max(b.ascent);
        out.descent = out.descent.max(b.descent);
        prev = last_char(kid).or(prev);
        started = true;
    }
    out.w = x;
    out
}

/// 运算符两侧的呼吸感。没有它，`E=mc^2` 会糊成一坨。
fn op_gap(prev: Option<char>, next: Option<char>, px: i32) -> i32 {
    let rel = |c: char| matches!(c, '=' | '<' | '>' | '≈' | '≤' | '≥' | '≠' | '→' | '∝');
    let bin = |c: char| matches!(c, '+' | '-' | '±' | '∓' | '×' | '÷' | '·');
    let em = px as f32;
    let k = if prev.is_some_and(rel) || next.is_some_and(rel) {
        0.26
    } else if prev.is_some_and(bin) || next.is_some_and(bin) {
        0.22
    } else if prev == Some(',') {
        0.16
    } else {
        0.0
    };
    (em * k).round() as i32
}

fn first_char(n: &Node) -> Option<char> {
    match n {
        Node::Char(c) => Some(*c),
        Node::Row(k) => k.iter().find_map(first_char),
        Node::Space(_) => None,
        Node::Sup(b, _) | Node::Sub(b, _) | Node::Sqrt(b) | Node::Vec(b) => first_char(b),
        Node::SupSub(b, _, _) => first_char(b),
        Node::Frac(a, _) => first_char(a),
        Node::BigOp { ch, .. } => Some(*ch),
        // 定界符优先报「左侧那个」：`\left(1+\frac1n\right)` 的首字是 `(`
        Node::Delim { open, body, .. } => open.or_else(|| first_char(body)),
        // 省略号不是「运算符邻居」：别让它影响前后间距
        Node::Dots { .. } => None,
    }
}

fn last_char(n: &Node) -> Option<char> {
    match n {
        Node::Char(c) => Some(*c),
        Node::Row(k) => k.iter().rev().find_map(last_char),
        Node::Space(_) => None,
        Node::Sup(b, _) | Node::Sub(b, _) | Node::Sqrt(b) | Node::Vec(b) => last_char(b),
        Node::SupSub(b, _, _) => last_char(b),
        Node::Frac(_, b) => last_char(b),
        Node::BigOp { ch, .. } => Some(*ch),
        Node::Delim { close, body, .. } => close.or_else(|| last_char(body)),
        Node::Dots { .. } => None,
    }
}

/// 上下标。`v_0^2` 是**并排**的两个小字，不是「下标里再套上标」。
fn layout_scripts(
    base: &Node,
    sub: Option<&Node>,
    sup: Option<&Node>,
    px: i32,
    m: &mut dyn Measure,
) -> MBox {
    let b = layout(base, px, m);
    let sp = script_px(px);
    let s = sub.map(|n| layout(n, sp, m));
    let p = sup.map(|n| layout(n, sp, m));

    let raise = ((px as f32) * SUP_RAISE).round() as i32;
    let drop = ((px as f32) * SUB_DROP).round() as i32;
    // 紧贴基底：留一点点缝，不然上标会「粘」在字上
    let kern = ((px as f32) * 0.04).round() as i32;
    let x = b.w + kern;

    let mut out = MBox {
        w: b.w,
        ascent: b.ascent,
        descent: b.descent,
        items: b.items,
    };

    if let Some(p) = &p {
        // 上标基线在 +raise 之上；它的 `dy` 已经是「相对自己基线的顶」，
        // 所以要再减一个 raise 才是相对**主基线**的顶。
        let mut q = p.clone();
        for it in &mut q.items {
            it.shift_y(-raise);
        }
        out.items.extend(q.shift(x).items);
        out.ascent = out.ascent.max(p.ascent + raise);
        out.w = out.w.max(x + p.w);
    }
    if let Some(s) = &s {
        let mut q = s.clone();
        for it in &mut q.items {
            it.shift_y(drop);
        }
        out.items.extend(q.shift(x).items);
        out.descent = out.descent.max(s.descent + drop);
        out.w = out.w.max(x + s.w);
    }
    out
}

/// 分式：横线压在「轴高」上，分子在线上、分母在线下，各自居中。
fn layout_frac(num: &Node, den: &Node, px: i32, m: &mut dyn Measure) -> MBox {
    let fpx = (((px as f32) * FRAC_SCALE).round() as i32).max(SCRIPT_MIN_PX);
    let n = layout(num, fpx, m);
    let d = layout(den, fpx, m);
    let axis = ((px as f32) * AXIS).round() as i32;
    let gap = ((px as f32) * FRAC_GAP).round() as i32;
    let pad = ((px as f32) * FRAC_PAD).round() as i32;
    let thick = (px / 18).max(1);

    let w = n.w.max(d.w) + pad * 2;
    // 竖直定位用**字体行度量**，不能用分子/分母盒子的 `ascent/descent`。
    //
    // 坑：分母是 `r^2` 时，盒子的 `ascent` 里含着抬高的上标，
    // 按 `d.ascent` 摆会把整个分母推下去一大截 —— 现象是横线紧贴分子、
    // 分母掉在半空（第一版实测 `F = G\frac{m_1 m_2}{r^2}` 就是这样）。
    // 行度量只跟字号有关，与盒子里有没有上标无关，所以是稳定的基准线。
    let fm = m.text(&['x'], fpx);
    let n_dy = axis + gap + fm.descent;
    let d_dy = axis + gap + fm.ascent;

    let mut items = Vec::new();
    let nx = (w - n.w) / 2;
    for it in &n.items {
        let mut t = it.clone();
        t.shift_x(nx);
        t.shift_y(-n_dy);
        items.push(t);
    }
    let dx_ = (w - d.w) / 2;
    for it in &d.items {
        let mut t = it.clone();
        t.shift_x(dx_);
        t.shift_y(d_dy);
        items.push(t);
    }
    // 横线最后加：绘制顺序上压在字下面也无所谓（两者不相交），
    // 但保持「先字后线」让像素读回时的顺序稳定。
    items.push(Item::Rule {
        dx0: 0,
        dx1: w,
        dy: -axis,
        thick,
    });

    MBox {
        w,
        ascent: n_dy + n.ascent,
        descent: d_dy + d.descent,
        items,
    }
}

/// 根号：`√` 字根按**墨迹**对齐缩放，上横线从字根右肩拉到内容末尾。
///
/// # 为什么不能按 `ascent` 摆（第一版踩的坑）
///
/// 字体的 `tmAscent/tmDescent` 是给一整行留位的**行度量**，跟单个 `√` 的墨迹
/// 差得很远：实测验光雅黑 44px 时行高约 38px，而 `√` 的墨迹只占其中一半、
/// 且重心偏下。第一版按 `ascent` 摆的结果是**整根掉到被开方内容下面**，
/// 还因为 `descent` 估小把下一行顶穿。
///
/// 现在的做法：先用 100px 探针量出「墨迹高 / 字号」的比值，再按需要的实际高度
/// 反算字号，最后用 `top` 精确对齐 —— 换任何字体都不用改常数。
fn layout_sqrt(x: &Node, px: i32, m: &mut dyn Measure) -> MBox {
    let c = layout(x, px, m);
    // 内边距 0.10em：被开方里常带**上标**（`b^2`），上标的墨迹几乎顶到盒子
    // 上界，0.06em 时横线会贴着它，看起来像「²被划掉」。
    let pad = ((px as f32) * 0.10).round() as i32;
    let thick = (px / 20).max(1);

    let bar_y = -(c.ascent + pad);
    let want_top = bar_y - thick;
    let want_bottom = c.descent;

    // 探针：100px 下这个字体的 `√` 墨迹有多高，据此换算目标字号
    let probe = 100;
    let pi = m.ink('√', probe);
    let probe_h = (pi.bottom - pi.top).max(1);
    let need_h = (want_bottom - want_top).max(px / 2);
    // 上限 2.4 倍：`√` 是**等比缩放的字形**，放大时笔画同比变粗。
    // 嵌套分式（`\sqrt{\frac{L}{g}}`）本该用字体的可伸缩根号变体，
    // 我们没有那个 OpenType 特性，只能让它略短、靠上横线补足覆盖 ——
    // 实测 3.2 倍时那根斜杠比正文还重，比「略短」难看得多。
    let rad_px = ((probe * need_h + probe_h / 2) / probe_h).clamp(px / 2, (px * 12) / 5);
    let ri = m.ink('√', rad_px);
    let rad_asc = m.text(&['√'], rad_px).ascent;

    // TextOutW 的 y 是字符格顶：墨迹顶 = dy + ascent + ink.top
    // ⇒ dy = want_top - ascent - ink.top
    let rad_dy = want_top - rad_asc - ri.top;
    let overlap = ((ri.adv as f32) * 0.26).round() as i32;
    let cx = (ri.adv - overlap).max(0);
    let tail = pad;
    let w = cx + c.w + tail;

    let mut items = vec![Item::Glyph {
        ch: '√',
        dx: 0,
        dy: rad_dy,
        px: rad_px,
    }];
    for it in &c.items {
        let mut t = it.clone();
        t.shift_x(cx);
        items.push(t);
    }
    items.push(Item::Rule {
        dx0: cx,
        dx1: w,
        dy: bar_y,
        thick,
    });

    // 下界取「内容」和「字根墨迹底」的较大者 —— 字根的钩子通常正好落在基线上，
    // 但放大后可能探出去一点，探出去就会被下一行压住。
    let rad_bottom = rad_dy + rad_asc + ri.bottom;
    MBox {
        w,
        ascent: c.ascent + pad + thick,
        descent: c.descent.max(rad_bottom).max(c.descent),
        items,
    }
}

/// 一个定界符字形：按内容高度选字号，墨迹中线对齐 `mid`。
///
/// 返回 `(图元, 前进宽度, 墨迹顶, 墨迹底)`，后两者相对基线、顶为负。
fn one_delim(
    ch: char,
    mid: i32,
    need_h: i32,
    px: i32,
    x: i32,
    m: &mut dyn Measure,
) -> (Item, i32, i32, i32) {
    // 探针：100px 下这个定界符的墨迹有多高，据此换算目标字号
    let probe = 100;
    let p_ink = m.ink(ch, probe);
    let p_h = (p_ink.bottom - p_ink.top).max(1);
    // 上限与根号同一条线（2.4 倍）：再大笔画就比正文还重了
    let dpx = ((probe * need_h + p_h / 2) / p_h).clamp(px, (px * 12) / 5);
    let ink = m.ink(ch, dpx);
    let t = m.text(&[ch], dpx);
    // 墨迹中线对准 mid：dy + ascent + (top+bottom)/2 = mid
    let dy = mid - t.ascent - (ink.top + ink.bottom) / 2;
    (
        Item::Glyph {
            ch,
            dx: x,
            dy,
            px: dpx,
        },
        t.w,
        dy + t.ascent + ink.top,
        dy + t.ascent + ink.bottom,
    )
}

/// `\left( ... \right)`：定界符按**内容高度**等比放大。
///
/// 没有 OpenType 的伸缩变体可用，只能缩字号 —— 好处是简单、
/// 与根号同一套取舍；代价是超高的括号笔画会变粗。
fn layout_delim(
    open: Option<char>,
    close: Option<char>,
    body: &Node,
    px: i32,
    m: &mut dyn Measure,
) -> MBox {
    let c = layout(body, px, m);
    if open.is_none() && close.is_none() {
        // `\left. ... \right.` —— 两侧都不画，等于隐形括号，别白占宽度
        return c;
    }
    // 比内容高一点点才像「抱住」；正好等高看着像贴边
    let pad = ((px as f32) * 0.06).round() as i32;
    let want_top = -(c.ascent + pad);
    let want_bottom = c.descent + pad;
    let need_h = (want_bottom - want_top).max(px);
    let mid = (want_top + want_bottom) / 2;

    let mut items: Vec<Item> = Vec::new();
    let mut x = 0;
    let mut ascent = c.ascent;
    let mut descent = c.descent;

    if let Some(ch) = open {
        let (it, w, top, bottom) = one_delim(ch, mid, need_h, px, x, m);
        x += w;
        ascent = ascent.max(-top);
        descent = descent.max(bottom);
        items.push(it);
    }
    items.extend(c.shift(x).items);
    x += c.w;
    if let Some(ch) = close {
        let (it, w, top, bottom) = one_delim(ch, mid, need_h, px, x, m);
        x += w;
        ascent = ascent.max(-top);
        descent = descent.max(bottom);
        items.push(it);
    }

    MBox {
        w: x,
        ascent,
        descent,
        items,
    }
}

/// 省略号：三个方点。
///
/// 点的大小与间距按字号走（0.10em 的方点、0.16em 的缝），整串宽约 0.62em ——
/// 和 `\cdots` 在 TeX 里的占位差不多。
fn layout_dots(mid: bool, px: i32, m: &mut dyn Measure) -> MBox {
    let size = (px / 10).max(2);
    let gap = ((px as f32) * 0.16).round() as i32;
    let w = size * 3 + gap * 2;
    // 中线档对齐 `+` 的**墨迹中心**，不是写死的轴高：
    // 数学轴的位置由字体决定，雅黑的 `+` 比 0.28em 略高，
    // 按常量摆会让点明显掉在加号下面一截。
    let dy = if mid {
        let ink = m.ink('+', px);
        (ink.top + ink.bottom) / 2
    } else {
        -(size / 2)
    };
    MBox {
        w,
        ascent: (-dy + size / 2).max(0),
        descent: (dy + size / 2).max(0),
        items: vec![Item::Dots {
            dx: 0,
            dy,
            gap,
            size,
        }],
    }
}

/// 矢量箭头：内容上方一条带箭头的线。
fn layout_vec(x: &Node, px: i32, m: &mut dyn Measure) -> MBox {
    let c = layout(x, px, m);
    let lift = ((px as f32) * 0.10).round() as i32;
    let thick = (px / 22).max(1);
    // 箭头顶着**字母的墨迹上缘**，不是字体的行高。
    // 按 `c.ascent` 摆会离字母十几像素，看着像一条飘在半空的上划线 ——
    // 拉丁大写字母的墨迹通常只占行高的八成。
    let ink_top = first_char(x)
        .map(|ch| m.ink(ch, px).top)
        .unwrap_or(-c.ascent);
    let dy = ink_top - lift - thick;
    let mut items = c.items.clone();
    if c.w > 3 {
        items.push(Item::Arrow {
            dx0: 0,
            dx1: c.w - 1,
            dy,
            thick,
        });
    }
    MBox {
        w: c.w,
        ascent: (-dy).max(c.ascent),
        descent: c.descent,
        items,
    }
}

/// 大算符：上下限居中摆在符号**上下方**。
///
/// `\lim` 的 `l` 只是占位，真正画的是三个字（`lim`），这里按整串量。
fn layout_bigop(
    ch: char,
    sub: Option<&Node>,
    sup: Option<&Node>,
    px: i32,
    m: &mut dyn Measure,
) -> MBox {
    let (op_chars, op_px): (Vec<char>, i32) = if ch == 'l' {
        (vec!['l', 'i', 'm'], px)
    } else {
        (vec![ch], ((px as f32) * 1.45).round() as i32)
    };
    let op_t = m.text(&op_chars, op_px);
    let sp = script_px(px);
    let s = sub.map(|n| layout(n, sp, m));
    let p = sup.map(|n| layout(n, sp, m));

    let gap = ((px as f32) * 0.10).round() as i32;
    let w = op_t.w.max(s.as_ref().map_or(0, |b| b.w)).max(p.as_ref().map_or(0, |b| b.w));
    let op_x = (w - op_t.w) / 2;

    let mut items: Vec<Item> = Vec::new();
    // 大算符逐字摆（`lim` 是三个字）
    let mut cx = op_x;
    for c in &op_chars {
        let t = m.text(&[*c], op_px);
        items.push(Item::Glyph {
            ch: *c,
            dx: cx,
            dy: -t.ascent,
            px: op_px,
        });
        cx += t.w;
    }

    let mut ascent = op_t.ascent;
    let mut descent = op_t.descent;

    if let Some(p) = &p {
        let px_x = (w - p.w) / 2;
        let dy_shift = -(op_t.ascent + gap + p.descent);
        for it in &p.items {
            let mut t = it.clone();
            t.shift_x(px_x);
            t.shift_y(dy_shift);
            items.push(t);
        }
        ascent = ascent.max(p.ascent + (-dy_shift));
    }
    if let Some(s) = &s {
        let sx = (w - s.w) / 2;
        let dy_shift = op_t.descent + gap + s.ascent;
        for it in &s.items {
            let mut t = it.clone();
            t.shift_x(sx);
            t.shift_y(dy_shift);
            items.push(t);
        }
        descent = descent.max(s.descent + dy_shift);
    }

    MBox {
        w,
        ascent,
        descent,
        items,
    }
}

// ---------------------------------------------------------------- GDI 度量

/// 字体来源：按字号要一支字体对象。
///
/// # 为什么要抽这一层
///
/// 公式要**两套**字体：正排的、以及 `lfEscapement = 2700`（顺时针 90°，
/// 竖版整块旋转用）的。两套都得和正文共用同一份缓存 —— 每帧现建现删的话，
/// `--memtest` 的 GDI 句柄数会跟着帧数涨，而那种「涨」看起来像泄漏。
///
/// 生产路径实现者是 `gfx::Fonts`（缓存挂在它身上，随窗口生命周期存活），
/// `--math` 样张没有 `Fonts`，就用下面自建自销的 `OwnFonts`。
pub trait FontAt {
    fn font(&self, px: i32) -> HFONT;
    /// 同一号字、顺时针转 90°。
    fn font_rot(&self, px: i32) -> HFONT;
}

/// 建一支字体。`esc` 是 `lfEscapement` / `lfOrientation`（0.1°）。
fn make_tex_font(face: &[u16], px: i32, esc: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -px,
            0,
            esc,
            esc,
            FW_NORMAL,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_TT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            FF_DONTCARE,
            face.as_ptr(),
        )
    }
}

/// 自建自销的字体源。给「手上没有 `gfx::Fonts`」的场合（`--math` 样张、
/// 单元测试之外的小工具）用。
pub struct OwnFonts {
    face: Vec<u16>,
    fonts: RefCell<HashMap<i32, HFONT>>,
    rots: RefCell<HashMap<i32, HFONT>>,
}

impl OwnFonts {
    pub fn new(face: &[u16]) -> OwnFonts {
        OwnFonts {
            face: face.to_vec(),
            fonts: RefCell::new(HashMap::new()),
            rots: RefCell::new(HashMap::new()),
        }
    }
}

impl FontAt for OwnFonts {
    fn font(&self, px: i32) -> HFONT {
        let px = px.max(SCRIPT_MIN_PX);
        if let Some(&f) = self.fonts.borrow().get(&px) {
            return f;
        }
        let f = make_tex_font(&self.face, px, 0);
        self.fonts.borrow_mut().insert(px, f);
        f
    }
    fn font_rot(&self, px: i32) -> HFONT {
        let px = px.max(SCRIPT_MIN_PX);
        if let Some(&f) = self.rots.borrow().get(&px) {
            return f;
        }
        let f = make_tex_font(&self.face, px, ROT_ESCAPEMENT);
        self.rots.borrow_mut().insert(px, f);
        f
    }
}

impl Drop for OwnFonts {
    fn drop(&mut self) {
        unsafe {
            for (_, f) in self.fonts.borrow_mut().drain().chain(self.rots.borrow_mut().drain()) {
                if !f.is_null() {
                    DeleteObject(f as HGDIOBJ);
                }
            }
        }
    }
}

/// 顺时针 90° 的 `lfEscapement`。
///
/// **实测定的，不是抄的**（2026-11-03，ctypes 扫墨迹包围盒）：
/// `900` 让文字沿 **-y** 推进（往上走），`2700` 沿 **+y**（往下走）。
/// 竖版公式要「从上往下读」，所以是 2700。
const ROT_ESCAPEMENT: i32 = 2700;

enum FontSrc<'a> {
    Owned(OwnFonts),
    Ext(&'a dyn FontAt),
}

impl FontSrc<'_> {
    fn font(&self, px: i32) -> HFONT {
        match self {
            FontSrc::Owned(o) => o.font(px),
            FontSrc::Ext(e) => e.font(px),
        }
    }
    fn font_rot(&self, px: i32) -> HFONT {
        match self {
            FontSrc::Owned(o) => o.font_rot(px),
            FontSrc::Ext(e) => e.font_rot(px),
        }
    }
}

/// 真正落到 GDI 上的度量器。度量**按字号缓存**，字体由 `FontSrc` 提供。
///
/// 用 `RefCell` 而不是 `&mut self`：`Measure::ink/text` 的签名是只读的，
/// 但根号那一支的字号是**排版时才算出来**的（要看被开方内容多高），
/// 没法事先预热。`RefCell` 让「查缓存 → 没有就建」发生在只读方法里，
/// 单线程、只在排版一趟内使用，代价可以忽略。
pub struct GdiMeasure<'a> {
    hdc: HDC,
    src: FontSrc<'a>,
    metrics: RefCell<HashMap<i32, (i32, i32)>>,
    inks: RefCell<HashMap<(u32, i32), Ink>>,
}

impl<'a> GdiMeasure<'a> {
    /// 自带字体缓存（自己建、自己删）。
    pub fn new(hdc: HDC, face: &[u16]) -> GdiMeasure<'a> {
        GdiMeasure {
            hdc,
            src: FontSrc::Owned(OwnFonts::new(face)),
            metrics: RefCell::new(HashMap::new()),
            inks: RefCell::new(HashMap::new()),
        }
    }

    /// 借用外部字体缓存 —— **生产路径走这条**（缓存挂在 `gfx::Fonts` 上，
    /// 不随帧重建，GDI 句柄数才是平的）。
    pub fn borrowed(hdc: HDC, src: &'a dyn FontAt) -> GdiMeasure<'a> {
        GdiMeasure {
            hdc,
            src: FontSrc::Ext(src),
            metrics: RefCell::new(HashMap::new()),
            inks: RefCell::new(HashMap::new()),
        }
    }

    /// 取某一档字号的字体对象。
    pub fn font(&self, px: i32) -> HFONT {
        self.src.font(px)
    }

    /// 取同一号字的旋转版（顺时针 90°）。
    pub fn font_rot(&self, px: i32) -> HFONT {
        self.src.font_rot(px)
    }

    fn tm(&self, px: i32) -> (i32, i32) {
        let px = px.max(SCRIPT_MIN_PX);
        if let Some(&m) = self.metrics.borrow().get(&px) {
            return m;
        }
        let f = self.font(px);
        let mut tm: TEXTMETRICW = unsafe { std::mem::zeroed() };
        unsafe {
            SelectObject(self.hdc, f as HGDIOBJ);
            GetTextMetricsW(self.hdc, &mut tm);
        }
        let m = (tm.tmAscent as i32, tm.tmDescent as i32);
        self.metrics.borrow_mut().insert(px, m);
        m
    }

    /// 单字**真实墨迹**（`GetGlyphOutlineW` 的 `GGO_METRICS` 档，不取轮廓数据）。
    ///
    /// `gmptGlyphOrigin` 是「墨迹框左上角相对基线原点」的坐标（**向上为正**），
    /// 所以换成我们的「向下为正」要取负：`top = -origin.y`、
    /// `bottom = gmBlackBoxY - origin.y`。
    fn ink_of(&self, ch: char, px: i32) -> Ink {
        let px = px.max(SCRIPT_MIN_PX);
        let key = (ch as u32, px);
        if let Some(&i) = self.inks.borrow().get(&key) {
            return i;
        }
        let f = self.font(px);
        let mut gm: GLYPHMETRICS = unsafe { std::mem::zeroed() };
        let one = FIXED { fract: 0, value: 1 };
        let zero = FIXED { fract: 0, value: 0 };
        let mat = MAT2 {
            eM11: one,
            eM12: zero,
            eM21: zero,
            eM22: one,
        };
        let r = unsafe {
            SelectObject(self.hdc, f as HGDIOBJ);
            GetGlyphOutlineW(
                self.hdc,
                ch as u32,
                GGO_METRICS,
                &mut gm,
                0,
                std::ptr::null_mut(),
                &mat,
            )
        };
        let (asc, desc) = self.tm(px);
        let i = if r == GDI_ERROR as u32 || gm.gmBlackBoxY == 0 {
            // 取不到（点阵字体 / 字体里没这个字）：退回行度量
            Ink {
                top: -asc,
                bottom: desc,
                w: (px as f32 * 0.6) as i32,
                adv: (px as f32 * 0.6) as i32,
            }
        } else {
            let oy = gm.gmptGlyphOrigin.y;
            Ink {
                top: -oy,
                bottom: gm.gmBlackBoxY as i32 - oy,
                w: gm.gmBlackBoxX as i32,
                adv: gm.gmCellIncX.max(0) as i32,
            }
        };
        self.inks.borrow_mut().insert(key, i);
        i
    }
}

impl Measure for GdiMeasure<'_> {
    fn text(&self, s: &[char], px: i32) -> TextMetric {
        let (ascent, descent) = self.tm(px);
        TextMetric {
            w: measure_run(self.hdc, self.font(px), s),
            ascent,
            descent,
        }
    }

    fn ink(&self, ch: char, px: i32) -> Ink {
        self.ink_of(ch, px)
    }
}

fn measure_run(hdc: HDC, font: HFONT, s: &[char]) -> i32 {
    if s.is_empty() {
        return 0;
    }
    let mut buf: Vec<u16> = Vec::with_capacity(s.len());
    for c in s {
        let mut pair = [0u16; 2];
        buf.extend(c.encode_utf16(&mut pair).iter().copied());
    }
    let mut sz: SIZE = unsafe { std::mem::zeroed() };
    unsafe {
        SelectObject(hdc, font as HGDIOBJ);
        GetTextExtentPoint32W(hdc, buf.as_ptr(), buf.len() as i32, &mut sz);
    }
    sz.cx
}

/// 带 GDI 度量的一次完整排版。
pub fn layout_with_gdi(node: &Node, px: i32, gm: &mut GdiMeasure<'_>) -> MBox {
    layout(node, px, gm)
}

// ---------------------------------------------------------------- 绘制

/// 把一块公式画到 `hdc` 上。`(ox, oy)` 是**基线左端**。
///
/// 辅助图元（横线 / 箭头）用实心刷子 + `FillRect`，箭头帽用 `Polygon` ——
/// 都是 GDI 自带能力，不额外依赖。
pub fn draw(hdc: HDC, mb: &MBox, ox: i32, oy: i32, color: u32, gm: &mut GdiMeasure<'_>) {
    unsafe {
        SetBkMode(hdc, TRANSPARENT);
        SetTextCharacterExtra(hdc, 0);
        SetTextColor(hdc, color);

        // 字：按字号分组换字体（减少 SelectObject 次数）
        let mut cur_px = -1;
        for it in &mb.items {
            if let Item::Glyph { ch, dx, dy, px } = *it {
                if px != cur_px {
                    SelectObject(hdc, gm.font(px) as HGDIOBJ);
                    cur_px = px;
                }
                let mut buf = [0u16; 2];
                let n = ch.encode_utf16(&mut buf).len();
                TextOutW(hdc, ox + dx, oy + dy, buf.as_ptr(), n as i32);
            }
        }

        // 线与箭头：一支刷子画完
        let brush = CreateSolidBrush(color);
        for it in &mb.items {
            match *it {
                Item::Rule {
                    dx0,
                    dx1,
                    dy,
                    thick,
                } => {
                    let r = RECT {
                        left: ox + dx0,
                        top: oy + dy,
                        right: ox + dx1,
                        bottom: oy + dy + thick,
                    };
                    FillRect(hdc, &r, brush);
                }
                Item::Arrow {
                    dx0,
                    dx1,
                    dy,
                    thick,
                } => draw_arrow(hdc, brush, ox + dx0, ox + dx1, oy + dy, thick),
                Item::Dots {
                    dx,
                    dy,
                    gap,
                    size,
                } => {
                    for i in 0..3 {
                        let x0 = ox + dx + i * (size + gap);
                        let top = oy + dy - size / 2;
                        let r = RECT {
                            left: x0,
                            top,
                            right: x0 + size,
                            bottom: top + size,
                        };
                        FillRect(hdc, &r, brush);
                    }
                }
                Item::Glyph { .. } => {}
            }
        }
        DeleteObject(brush as HGDIOBJ);
    }
}

/// 把一块公式**顺时针旋转 90°** 画到 `hdc` 上。
///
/// `(x, y)` 是**旋转后包围盒的左上角**（不是基线）。转完盒子宽高互换：
/// 横着占 `w`、竖着占 `h` 的盒子，转完是宽 `h`、高 `w`。
///
/// # 坐标映射
///
/// 绕「基线左端」顺时针转 90°：`(dx, dy) → (-dy, dx)`（屏幕坐标 y 向下，
/// 于是「基线推进」转到 +y、「字头朝上」转到 +x）。盒子的 `x ∈ [0, w]`、
/// `y ∈ [-ascent, descent]` 因此转到 `x' ∈ [-descent, ascent]`、
/// `y' ∈ [0, w]` —— 左缘由**下深**决定，不是上高：
///
/// ```text
/// 屏幕 x = x + descent - dy
/// 屏幕 y = y + dx
/// ```
///
/// 旋转本身交给字体（`lfEscapement = 2700`，见 `ROT_ESCAPEMENT` 的实测记录），
/// 所以字形还是**矢量抗锯齿**的，不是位图转置。
pub fn draw_rot(hdc: HDC, mb: &MBox, x: i32, y: i32, color: u32, gm: &mut GdiMeasure<'_>) {
    unsafe {
        SetBkMode(hdc, TRANSPARENT);
        SetTextCharacterExtra(hdc, 0);
        SetTextColor(hdc, color);

        let base_x = x + mb.descent;
        let mut cur_px = -1;
        for it in &mb.items {
            if let Item::Glyph { ch, dx, dy, px } = *it {
                if px != cur_px {
                    SelectObject(hdc, gm.font_rot(px) as HGDIOBJ);
                    cur_px = px;
                }
                let mut buf = [0u16; 2];
                let n = ch.encode_utf16(&mut buf).len();
                TextOutW(hdc, base_x - dy, y + dx, buf.as_ptr(), n as i32);
            }
        }

        let brush = CreateSolidBrush(color);
        for it in &mb.items {
            match *it {
                Item::Rule {
                    dx0,
                    dx1,
                    dy,
                    thick,
                } => {
                    // 横线转成竖线：厚度方向从「y」变「x」，长度方向从「x」变「y」。
                    let r = RECT {
                        left: base_x - dy - thick,
                        top: y + dx0,
                        right: base_x - dy,
                        bottom: y + dx1,
                    };
                    FillRect(hdc, &r, brush);
                }
                Item::Arrow {
                    dx0,
                    dx1,
                    dy,
                    thick,
                } => draw_arrow_v(hdc, brush, y + dx0, y + dx1, base_x - dy, thick),
                Item::Dots {
                    dx,
                    dy,
                    gap,
                    size,
                } => {
                    // 竖排时三个点自上而下：`dx` 这一维变成 y
                    for i in 0..3 {
                        let top = y + dx + i * (size + gap);
                        let left = base_x - dy - size / 2;
                        let r = RECT {
                            left,
                            top,
                            right: left + size,
                            bottom: top + size,
                        };
                        FillRect(hdc, &r, brush);
                    }
                }
                Item::Glyph { .. } => {}
            }
        }
        DeleteObject(brush as HGDIOBJ);
    }
}

/// 一条右向箭头：杆 + 三角帽。`y` 是杆的中线。
unsafe fn draw_arrow(hdc: HDC, brush: windows_sys::Win32::Graphics::Gdi::HBRUSH, x0: i32, x1: i32, y: i32, thick: i32) {
    unsafe {
        // 帽子必须比杆**粗得肉眼可辨**，否则矢量箭头会退化成一条上划线。
        // 按杆粗算（4 倍）在 44px 时只有 8px，看起来就是「ē」；改成按长度算。
        let span = (x1 - x0).max(1);
        let head = ((span as f32 * 0.45).round() as i32).clamp(8, 26);
        let half = ((head as f32 * 0.52).round() as i32).max(4);
        let shaft_end = (x1 - head).max(x0);
        // 杆
        let r = RECT {
            left: x0,
            top: y - thick / 2,
            right: shaft_end,
            bottom: y - thick / 2 + thick.max(1),
        };
        FillRect(hdc, &r, brush);
        // 帽
        let pts = [
            POINT { x: x1, y },
            POINT {
                x: shaft_end,
                y: y - half,
            },
            POINT {
                x: shaft_end,
                y: y + half,
            },
        ];
        Polygon(hdc, pts.as_ptr(), 3);
    }
}

/// 一条**向下**的箭头（`draw_arrow` 的旋转版）。`x` 是杆的中线。
unsafe fn draw_arrow_v(
    hdc: HDC,
    brush: windows_sys::Win32::Graphics::Gdi::HBRUSH,
    y0: i32,
    y1: i32,
    x: i32,
    thick: i32,
) {
    unsafe {
        let span = (y1 - y0).max(1);
        let head = ((span as f32 * 0.45).round() as i32).clamp(8, 26);
        let half = ((head as f32 * 0.52).round() as i32).max(4);
        let shaft_end = (y1 - head).max(y0);
        let r = RECT {
            left: x - thick / 2,
            top: y0,
            right: x - thick / 2 + thick.max(1),
            bottom: shaft_end,
        };
        FillRect(hdc, &r, brush);
        let pts = [
            POINT { x, y: y1 },
            POINT {
                x: x - half,
                y: shaft_end,
            },
            POINT {
                x: x + half,
                y: shaft_end,
            },
        ];
        Polygon(hdc, pts.as_ptr(), 3);
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    fn fake() -> FakeMeasure {
        FakeMeasure
    }

    fn lay(src: &str, px: i32) -> MBox {
        let node = parse(src).expect("解析应通过");
        let mut m = fake();
        layout(&node, px, &mut m)
    }

    fn glyphs(mb: &MBox) -> Vec<(char, i32, i32, i32)> {
        mb.items
            .iter()
            .filter_map(|it| match *it {
                Item::Glyph { ch, dx, dy, px } => Some((ch, dx, dy, px)),
                _ => None,
            })
            .collect()
    }

    fn rules(mb: &MBox) -> Vec<(i32, i32, i32, i32)> {
        mb.items
            .iter()
            .filter_map(|it| match *it {
                Item::Rule {
                    dx0,
                    dx1,
                    dy,
                    thick,
                } => Some((dx0, dx1, dy, thick)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn 普通文字原样排成一行() {
        let mb = lay("速度", 40);
        let g = glyphs(&mb);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].0, '速');
        assert_eq!(g[1].0, '度');
        // 第二个字必须排在第一个字右边（宽度 = 一个汉字）
        assert_eq!(g[1].1, 40);
        assert_eq!(mb.w, 80);
    }

    #[test]
    fn 上标缩小并抬起() {
        let mb = lay("x^2", 40);
        let base_only = lay("x", 40);
        let g = glyphs(&mb);
        let x = g.iter().find(|(c, ..)| *c == 'x').expect("底字");
        let two = g.iter().find(|(c, ..)| *c == '2').expect("上标");
        // 上标字号必须更小
        assert!(two.3 < x.3, "上标字号 {} 应小于正文 {}", two.3, x.3);
        // 上标必须抬到正文之上（单元格顶更小）
        assert!(two.2 < x.2, "上标应更高：{} vs {}", two.2, x.2);
        // 盒子上界要容纳上标：比「只有 x」时更高
        assert!(
            mb.ascent > base_only.ascent,
            "盒子 {} 应比单独 x 的 {} 更高",
            mb.ascent,
            base_only.ascent
        );
    }

    #[test]
    fn 下标缩小并下移且盒子下界容纳它() {
        let mb = lay("v_0", 40);
        let g = glyphs(&mb);
        assert_eq!(g.len(), 2);
        let (_, _, dy_base, _) = g[0];
        let (_, _, dy_sub, px_sub) = g[1];
        assert!(px_sub < 40);
        // 下标的单元格顶比正文低（dy 更大）
        assert!(dy_sub > dy_base);
        assert!(mb.descent > 8, "盒子下界 {} 应容纳下标", mb.descent);
    }

    #[test]
    fn 上下标并排而不是嵌套() {
        // v_0^2：三个字形。图元顺序是「底字 → 上标 → 下标」，
        // 所以这里**按字符找**，不按下标猜 —— 猜顺序的断言会在改动顺序时假红。
        let mb = lay("v_0^2", 40);
        let g = glyphs(&mb);
        assert_eq!(g.len(), 3);
        let v = g.iter().find(|(c, ..)| *c == 'v').expect("底字");
        let zero = g.iter().find(|(c, ..)| *c == '0').expect("下标 0");
        let two = g.iter().find(|(c, ..)| *c == '2').expect("上标 2");
        assert!(zero.1 > v.1, "下标应在底字右侧");
        assert!(two.1 >= zero.1, "上下标应并排：sub x={} sup x={}", zero.1, two.1);
        assert!(zero.2 > v.2, "下标应在下：{} vs {}", zero.2, v.2);
        assert!(two.2 < v.2, "上标应在上：{} vs {}", two.2, v.2);
    }

    #[test]
    fn 分式有横线且分子在上分母在下() {
        let mb = lay("\\frac{a}{b}", 40);
        let r = rules(&mb);
        assert_eq!(r.len(), 1, "分式应有一条横线");
        let (x0, x1, bar_y, thick) = r[0];
        assert!(x1 > x0, "横线必须有长度");
        assert!(thick >= 1);
        let g = glyphs(&mb);
        let a = g.iter().find(|(c, ..)| *c == 'a').expect("分子");
        let b = g.iter().find(|(c, ..)| *c == 'b').expect("分母");
        // `dy` 是**单元格顶**，比较「墨迹底」要再加一个 ascent（假度量 = 0.8em）。
        // 分子／分母用的是缩过的字号（FRAC_SCALE），所以按各自的 px 算。
        let asc = |px: i32| ((px as f32) * 0.8).round() as i32;
        let a_bottom = a.2 + asc(a.3);
        assert!(a_bottom < bar_y, "分子应压在横线上：底 {} vs 线 {}", a_bottom, bar_y);
        assert!(b.2 > bar_y, "分母应在横线下：顶 {} vs 线 {}", b.2, bar_y);
    }

    #[test]
    fn 分式分子分母水平居中() {
        // 分母比分子宽：分子应被推到中间
        let mb = lay("\\frac{a}{bbb}", 40);
        let g = glyphs(&mb);
        let a = g.iter().find(|(c, ..)| *c == 'a').expect("分子");
        assert!(a.1 > 0, "分子应右移居中，实际 dx={}", a.1);
    }

    #[test]
    fn 根号上横线覆盖内容且比内容宽() {
        let mb = lay("\\sqrt{ab}", 40);
        let r = rules(&mb);
        assert_eq!(r.len(), 1, "根号应有一条上横线");
        let (x0, x1, _, _) = r[0];
        let g = glyphs(&mb);
        let content_w = g
            .iter()
            .filter(|(c, ..)| *c != '√')
            .map(|(_, x, _, _)| *x)
            .max()
            .unwrap_or(0);
        assert!(x1 > x0 + content_w / 2, "横线要盖住内容");
        // 横线必须从内容左缘之前开始
        assert!(x0 <= content_w);
        assert!(mb.ascent > 40 * 8 / 10);
    }

    #[test]
    fn 根号字根比内容高() {
        let mb = lay("\\sqrt{g}", 40);
        let g = glyphs(&mb);
        let rad = g.iter().find(|(c, ..)| *c == '√').expect("字根");
        let inner = g.iter().find(|(c, ..)| *c == 'g').expect("被开方");
        assert!(rad.3 > inner.3, "字根字号 {} 应大于内容 {}", rad.3, inner.3);
    }

    #[test]
    fn 矢量有箭头且压在字上方() {
        let mb = lay("\\vec{v}", 40);
        let arrows: Vec<_> = mb
            .items
            .iter()
            .filter_map(|it| match *it {
                Item::Arrow { dx0, dx1, dy, .. } => Some((dx0, dx1, dy)),
                _ => None,
            })
            .collect();
        assert_eq!(arrows.len(), 1, "应有一条箭头");
        let (x0, x1, y) = arrows[0];
        assert!(x1 > x0);
        // 箭头要在字的单元格顶之上（dy 更小）
        let v = glyphs(&mb).into_iter().find(|(c, ..)| *c == 'v').unwrap();
        assert!(y < v.2, "箭头应压在字上方：{} vs {}", y, v.2);
    }

    #[test]
    fn 求和把上下限摆在符号上下() {
        let mb = lay("\\sum_{i=1}^{n}", 40);
        let g = glyphs(&mb);
        let sum = g.iter().find(|(c, ..)| *c == '∑').expect("求和号");
        // 上限 n 和下限内容都应在 x 方向上**包含在符号宽度内**（居中）
        let n = g.iter().find(|(c, ..)| *c == 'n').expect("上限 n");
        assert!(n.2 < sum.2, "上限应在符号上方：n.dy={} sum.dy={}", n.2, sum.2);
        for (c, dx, dy, _) in &g {
            if *c == 'n' {
                assert!(dy < &sum.2);
                assert!(dx.abs() < 40, "上限应居中在符号附近，dx={}", dx);
            }
        }
    }

    #[test]
    fn 定界符按内容放大() {
        let px = 40;
        let mb = lay("\\left(\\frac{a}{b}\\right)", px);
        let inner = lay("\\frac{a}{b}", px);
        let g = glyphs(&mb);
        let lp = g.iter().find(|(c, ..)| *c == '(').expect("左括号");
        let rp = g.iter().find(|(c, ..)| *c == ')').expect("右括号");
        assert!(lp.3 > px, "括号应随分式放大，实际字号 {}", lp.3);
        assert_eq!(lp.3, rp.3, "两侧应一样大");
        // 括号的单元格必须真的包住内容：从内容上方起、到下方止
        assert!(mb.ascent >= inner.ascent, "上界 {} < 内容 {}", mb.ascent, inner.ascent);
        assert!(mb.descent >= inner.descent, "下界 {} < 内容 {}", mb.descent, inner.descent);
        assert!(mb.w > inner.w + px / 2, "两侧括号要各占宽度，实际 {} vs {}", mb.w, inner.w);
    }

    #[test]
    fn 各方括号也能放大() {
        let mb = lay("\\left[\\frac{a}{b}\\right]", 40);
        let g = glyphs(&mb);
        let lb = g.iter().find(|(c, ..)| *c == '[').expect("左方括号");
        assert!(lb.3 > 40, "方括号也要放大，实际 {}", lb.3);
    }

    #[test]
    fn 空定界符不占宽度() {
        // `\left.` 是「这一侧不画」：不能凭空多半个字的空白
        let bare = lay("x", 40);
        let dotted = lay("\\left. x \\right.", 40);
        assert_eq!(dotted.w, bare.w, "两侧都不画时宽度应与裸内容相同");
        assert_eq!(glyphs(&dotted).len(), 1, "只应剩下那个 x");
        // 单边画竖线：多一根 `|`，宽度只多那一根
        let one = lay("\\left. x \\right|", 40);
        assert_eq!(glyphs(&one).iter().filter(|g| g.0 == '|').count(), 1);
        assert!(one.w > bare.w, "竖线要占宽度");
    }

    #[test]
    fn left_right必须配对() {
        assert!(parse("\\left( x").is_err(), "缺 \\right 应报错");
        assert!(parse("\\left( x \\right").is_err(), "\\right 后缺定界符应报错");
        assert!(parse("x \\right)").is_err(), "孤立的 \\right 应报错");
    }

    #[test]
    fn right不会吃掉rightarrow() {
        // `\rightarrow` 以 `\right` 开头，不能被当成定界符命令
        let mb = lay("a \\rightarrow b", 40);
        let g = glyphs(&mb);
        assert!(
            g.iter().any(|(c, ..)| *c == '→'),
            "应排出箭头，实际 {:?}",
            g.iter().map(|g| g.0).collect::<Vec<_>>()
        );
    }

    #[test]
    fn 省略号有中线与基线两种() {
        let dots = |mb: &MBox| -> (i32, i32) {
            mb.items
                .iter()
                .find_map(|it| match *it {
                    Item::Dots { dy, size, .. } => Some((dy, size)),
                    _ => None,
                })
                .expect("应有一个省略号图元")
        };
        let (mid_dy, mid_size) = dots(&lay("1 + \\cdots + n", 40));
        let (low_dy, _) = dots(&lay("\\ldots", 40));
        // 中线档的点要**更高**（dy 更负），且落在数学轴上（约 0.28em）
        assert!(mid_dy < low_dy, "\\cdots 应比 \\ldots 高：{} vs {}", mid_dy, low_dy);
        assert!(
            (-16..=-8).contains(&mid_dy),
            "中线点应落在大约 0.2~0.4em 高处，实际 dy={}",
            mid_dy
        );
        assert!(mid_size >= 2, "点不能小到看不见，实际 {}", mid_size);
    }

    #[test]
    fn 希腊字母命令落到字符() {
        let mb = lay("\\alpha", 40);
        assert_eq!(glyphs(&mb)[0].0, 'α');
        let mb = lay("\\Delta", 40);
        assert_eq!(glyphs(&mb)[0].0, 'Δ');
    }

    #[test]
    fn 汉字下标可用() {
        // 物理里 F_合、a_向 这种写法
        let mb = lay("F_{合}", 40);
        let g = glyphs(&mb);
        let he = g.iter().find(|(c, ..)| *c == '合').expect("汉字下标");
        assert!(he.3 < 40, "下标汉字必须缩小，实际 {}", he.3);
    }

    #[test]
    fn 未闭合花括号报错_兜底不panic() {
        assert!(parse("\\frac{1}{").is_err());
        assert!(parse("{a").is_err());
        // 兜底：整串当文字，仍然能排出东西
        let n = parse_or_text("\\frac{1}{");
        let mut m = fake();
        let mb = layout(&n, 40, &mut m);
        assert!(!mb.items.is_empty());
    }

    #[test]
    fn 未知命令报错() {
        assert!(parse("\\unknowncmd").is_err());
        // 但不 panic，兜底成文字
        let n = parse_or_text("\\unknowncmd");
        let mut m = fake();
        let _ = layout(&n, 40, &mut m);
    }

    #[test]
    fn 运算符自动加间距() {
        let tight = lay("ab", 40).w;
        let spaced = lay("a=b", 40).w;
        // a=b 比 ab 多了 '=' 本身 + 两侧间距
        let eq_w = glyphs(&lay("=", 40))[0];
        assert!(
            spaced > tight + 40,
            "等号两侧应留白：{} vs {}",
            spaced,
            tight + eq_w.1
        );
    }

    #[test]
    fn 空串不panic且宽度为零() {
        let mb = lay("", 40);
        assert_eq!(mb.w, 0);
        assert!(mb.items.is_empty());
    }

    #[test]
    fn 高中四条公式都能排且尺寸合理() {
        let cases = [
            "E = mc^2",
            "v = \\sqrt{2gh}",
            "F = G\\frac{m_1 m_2}{r^2}",
            "T = 2\\pi\\sqrt{\\frac{L}{g}}",
        ];
        for src in cases {
            let node = parse(src).unwrap_or_else(|e| panic!("{} 解析失败：{}", src, e));
            let mut m = fake();
            let mb = layout(&node, 44, &mut m);
            assert!(mb.w > 0, "{} 宽度为零", src);
            assert!(mb.ascent > 0 && mb.descent > 0, "{} 上下界异常", src);
            // 所有字形的 x 都必须落在盒子宽度内
            for (c, dx, ..) in glyphs(&mb) {
                assert!(dx <= mb.w, "{} 的 {:?} 越界：dx={} w={}", src, c, dx, mb.w);
            }
        }
    }
}
