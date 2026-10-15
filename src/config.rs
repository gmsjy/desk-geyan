//! 配置：`%APPDATA%\DeskGeyan\config.ini`
//!
//! 手写 key=value 而不是引 serde —— 只有十来个字段，
//! 让整个程序保持零运行时依赖，`cargo build` 出来就是个能直接拷走的 exe。

use std::fmt;
use std::path::PathBuf;

use crate::util::{data_dir, ensure_data_dir, fmt_color, parse_color};

/// 可循环切换的字体候选。都是系统自带、一定存在的字体，
/// 不做「字体是否存在」的探测 —— 不存在的字体 GDI 会静默回退，不会崩。
pub const FONT_PRESETS: [&str; 7] = [
    "Microsoft YaHei UI",
    "Microsoft YaHei",
    "SimSun",
    "KaiTi",
    "FangSong",
    "SimHei",
    "Consolas",
];

/// 主色预设（时钟 + 格言）
pub const COLOR_PRESETS: [u32; 7] = [
    0xFF_FFFF, // 白
    0xF5_D98B, // 暖黄
    0xA8_E6E2, // 浅青
    0xF0_B4C8, // 粉
    0xB7_E4A0, // 浅绿
    0xFF_C08A, // 橙
    0x2B_2B2B, // 近黑（浅色壁纸用）
];

/// 日期色预设。默认一档是浅灰 —— 日期本来就该比时钟弱一档。
pub const DATE_COLOR_PRESETS: [u32; 6] = [
    0xB8_B8B8, // 浅灰
    0xFF_FFFF, // 白（跟主色一致）
    0x90_9090, // 中灰
    0xC8_BCA8, // 暖灰
    0xF5_D98B, // 暖黄
    0x2B_2B2B, // 近黑
];

/// 在一组预设里取「下一个」，便于用菜单/热键循环切换
fn cycle_u32(list: &[u32], cur: u32) -> u32 {
    let i = list.iter().position(|&v| v == cur).unwrap_or(usize::MAX);
    list[(i.wrapping_add(1)) % list.len()]
}

pub fn next_color(cur: u32) -> u32 {
    cycle_u32(&COLOR_PRESETS, cur)
}
pub fn next_date_color(cur: u32) -> u32 {
    cycle_u32(&DATE_COLOR_PRESETS, cur)
}

/// 循环「格言颜色」：跟随主色 → 七个预设 → 回到跟随主色。
/// 把 `COLOR_UNSET` 也放进循环，是因为「跟随主色」本来就是最常用的那一档，
/// 不该只能靠手改配置文件走回去。
pub fn next_quote_color(cur: u32) -> u32 {
    if cur == COLOR_UNSET {
        return COLOR_PRESETS[0];
    }
    let i = COLOR_PRESETS
        .iter()
        .position(|&v| v == cur)
        .unwrap_or(usize::MAX);
    let j = i.wrapping_add(1);
    if j >= COLOR_PRESETS.len() {
        COLOR_UNSET
    } else {
        COLOR_PRESETS[j]
    }
}

/// 「格言颜色」的可读标签
pub fn quote_color_label(c: u32) -> String {
    if c == COLOR_UNSET {
        "跟随主色".to_string()
    } else {
        fmt_color(c)
    }
}

/// 字体在其预设表里的下一个
pub fn next_font(cur: &str) -> String {
    let i = FONT_PRESETS
        .iter()
        .position(|&f| f.eq_ignore_ascii_case(cur.trim()))
        .unwrap_or(usize::MAX);
    FONT_PRESETS[(i.wrapping_add(1)) % FONT_PRESETS.len()].to_string()
}

/// 字号档位。走档位而不是 ±N，是为了让「大一点」的观感是均匀的。
pub const SCALE_STEPS: [i32; 12] = [60, 70, 80, 90, 100, 115, 130, 150, 175, 200, 250, 300];
/// 局部倍率档位（时钟 / 格言相对整体字号）
pub const RATIO_STEPS: [i32; 9] = [70, 80, 90, 100, 115, 130, 150, 180, 220];

pub fn next_scale(cur: i32, dir: i32) -> i32 {
    next_step(&SCALE_STEPS, cur, dir)
}

/// 在升序档位表里往 dir 方向走一格。`cur` 不在表里也能正确落格。
pub fn next_step(steps: &[i32], cur: i32, dir: i32) -> i32 {
    if dir > 0 {
        steps
            .iter()
            .copied()
            .find(|&v| v > cur)
            .unwrap_or(steps[steps.len() - 1])
    } else {
        steps
            .iter()
            .rev()
            .copied()
            .find(|&v| v < cur)
            .unwrap_or(steps[0])
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 毛玻璃：毛玻璃底板(底层窗口) + per-pixel alpha 文字(上层窗口)
    Glass,
    /// 纯文字：只有字形本身贴在桌面上，没有任何底板
    TextOnly,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Glass => "毛玻璃模式",
            Mode::TextOnly => "纯文字模式",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Anim {
    /// 跑马灯：从右向左无缝滚动，前一条滚出去后一条立刻接上
    Marquee,
    /// 静态：整句居中显示，按周期换句
    Static,
    /// 淡入淡出：渐显 → 停留 → 渐隐 → 换句
    Fade,
    /// 呼吸：整块内容的 alpha 缓慢起伏
    Breathe,
}

impl Anim {
    pub fn label(self) -> &'static str {
        match self {
            Anim::Marquee => "跑马灯",
            Anim::Static => "静态",
            Anim::Fade => "淡入淡出",
            Anim::Breathe => "呼吸",
        }
    }

    pub fn all() -> [Anim; 4] {
        [Anim::Marquee, Anim::Static, Anim::Fade, Anim::Breathe]
    }

    pub fn next(self) -> Anim {
        match self {
            Anim::Marquee => Anim::Static,
            Anim::Static => Anim::Fade,
            Anim::Fade => Anim::Breathe,
            Anim::Breathe => Anim::Marquee,
        }
    }

    pub fn parse(s: &str) -> Anim {
        match s.trim().to_ascii_lowercase().as_str() {
            "static" | "静态" => Anim::Static,
            "fade" | "淡入淡出" => Anim::Fade,
            "breathe" | "呼吸" => Anim::Breathe,
            _ => Anim::Marquee,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Anim::Marquee => "marquee",
            Anim::Static => "static",
            Anim::Fade => "fade",
            Anim::Breathe => "breathe",
        }
    }
}

impl fmt::Display for Anim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// 版式：横排 / 竖排。
///
/// 放在这里而不是 `layout.rs`，是为了和 `Mode` / `Anim` 一样**同源**：
/// 它同样要经过 `apply_kv → save → 菜单 → --dump` 四端，也要有
/// `label()` / `key()` / `parse()`。排版引擎只是它的消费者。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Orient {
    Horizontal,
    Vertical,
}

impl Orient {
    pub fn all() -> [Orient; 2] {
        [Orient::Horizontal, Orient::Vertical]
    }

    pub fn label(self) -> &'static str {
        match self {
            Orient::Horizontal => "横版",
            Orient::Vertical => "竖版（卷轴式）",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Orient::Horizontal => "horizontal",
            Orient::Vertical => "vertical",
        }
    }

    pub fn parse(s: &str) -> Orient {
        match s.trim().to_ascii_lowercase().as_str() {
            "vertical" | "v" | "竖版" | "竖排" => Orient::Vertical,
            _ => Orient::Horizontal,
        }
    }

    /// 这一形态的**默认窗口尺寸**。切换版式时只在这套默认值上做手脚，
    /// 用户自己改过的尺寸一律不覆盖（见 `set_orient`）。
    pub fn default_size(self) -> (i32, i32) {
        match self {
            Orient::Horizontal => (620, 176),
            Orient::Vertical => (300, 660),
        }
    }
}

impl fmt::Display for Orient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// `quote_color` 的哨兵值：表示「跟随主色」。
///
/// 用哨兵而不是 `Option<u32>`，是为了让**旧 ini（没有这个键）载入后行为逐像素不变** ——
/// 默认就是跟随主色，而 `color` 的取值范围是 `0x00RRGGBB`，永远不会撞上这个值。
pub const COLOR_UNSET: u32 = 0xFFFF_FFFF;

#[derive(Clone)]
pub struct Config {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// 整体不透明度 30..=255
    pub opacity: i32,
    pub mode: Mode,
    pub anim: Anim,
    pub always_on_top: bool,
    pub show_clock: bool,
    pub show_date: bool,
    pub show_quote: bool,
    /// 时钟是否带秒
    pub seconds: bool,
    /// 文字阴影：浅色壁纸下把白字托起来
    pub shadow: bool,
    /// 跑马灯速度 px/s
    pub speed: i32,
    /// 字号缩放百分比（整体，时钟/日期/格言一起放大）
    pub scale: i32,
    /// 时钟（含日期）相对整体字号的倍率 %
    pub clock_scale: i32,
    /// 格言相对整体字号的倍率 %
    pub quote_scale: i32,
    /// 字体名
    pub font: String,
    /// 主色（时钟 + 格言），`0x00RRGGBB`
    pub color: u32,
    /// 日期颜色。单独可配 —— 日期本来就该比时钟弱一档
    pub date_color: u32,
    /// 时间日期块相对「默认位置」的偏移（px）。默认位置是左侧垂直居中；
    /// 布局模式下直接拖它就能改这两个值。
    pub clock_dx: i32,
    pub clock_dy: i32,
    /// 是否在布局模式（可拖动 / 缩放）
    pub layout_mode: bool,
    pub visible: bool,
    /// 全屏应用（游戏 / 全屏播放）运行时自动隐藏，退出后恢复。
    /// 默认开：挂件漂在游戏画面上的观感很差，而这个场景几乎无法手动补救。
    pub auto_hide_fs: bool,
    /// 首次定位时自动吸附到屏幕右上角
    pub placed: bool,
    /// 版式：横排 / 竖排（形态 A 卷轴式）
    pub orient: Orient,
    /// 格言字距（0..=8，按整体字号缩放后生效）。0 = 今天的样子
    pub letter_spacing: i32,
    /// 格言专用色。`COLOR_UNSET` = 跟随主色（默认，保证旧配置行为不变）
    pub quote_color: u32,
    /// 放不下时是否允许「缩字号」回退（100% → 85% → 70%）。
    /// 关掉就直接走跑马灯 / 省略号
    pub shrink_fit: bool,
    /// 横版上次的窗口尺寸（切版式时用来还给用户）
    pub h_w: i32,
    pub h_h: i32,
    /// 竖版上次的窗口尺寸
    pub v_w: i32,
    pub v_h: i32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            x: -1,
            y: -1,
            w: 620,
            h: 176,
            opacity: 215,
            mode: Mode::Glass,
            anim: Anim::Marquee,
            always_on_top: true,
            show_clock: true,
            show_date: true,
            show_quote: true,
            seconds: true,
            shadow: true,
            speed: 60,
            scale: 100,
            clock_scale: 100,
            quote_scale: 100,
            font: FONT_PRESETS[0].to_string(),
            color: COLOR_PRESETS[0],
            date_color: DATE_COLOR_PRESETS[0],
            clock_dx: 0,
            clock_dy: 0,
            layout_mode: false,
            visible: true,
            auto_hide_fs: true,
            placed: false,
            orient: Orient::Horizontal,
            letter_spacing: 0,
            quote_color: COLOR_UNSET,
            shrink_fit: true,
            h_w: 620,
            h_h: 176,
            v_w: 300,
            v_h: 660,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        data_dir().join("config.ini")
    }

    /// 测试基准配置：从**确定值**出发，不受用户当前配置影响。
    ///
    /// 这不是洁癖：上一版三个测试入口都是直接在 `App::new()` 加载出来的
    /// 用户配置上改几个字段。用户一旦按 `Ctrl+Alt+C` 把时间关掉，
    /// 「拖动时间日期块」的测试就再也命中不了目标（命中判定直接返回 false），
    /// 于是测试静默变成一句空跑，还报绿。测试的起点必须是常量。
    ///
    /// ⚠️ **换完 `cfg` 必须接着 `rebuild_fonts()`。** 字体对象是 `App::new()` 按
    /// **用户配置**建好的一版（`Fonts::build(cfg.font, ...)`），它不会跟着 `cfg`
    /// 自己变 —— 只换 `cfg` 等于在「起点是常量」这条规矩上给字体留了个洞，
    /// 而且后果不小：用户选了个纵向度量很大的字体（手写体 / 装饰体）时，
    /// 时钟块会被撑得几乎填满窗口，命中判定于是把「拖空白处移动整窗」认成
    /// 「拖时间日期块」，`--dragtest` 的整窗移动整组失败（实测 `霞鹜文楷 Medium`
    /// → 时钟块高 128px / 窗高 176px，窗口正中心落在块内）。对 `--perf` /
    /// `--dragbench` 这类基准，它还会让读数随用户字体漂移。
    pub fn for_test() -> Config {
        Config {
            placed: true,
            x: 60,
            y: 60,
            ..Config::default()
        }
    }

    /// **唯一**的「生效动效」出口。
    ///
    /// 竖排下跑马灯没有意义（横向滚动的文字配竖排字格 = 横竖混排的怪物），
    /// 所以竖版一律降级为静态。`step` / `needs_frame` / `paint_main` / 菜单
    /// 四处都必须读这一个函数 —— 各写各的必然走偏（菜单说静态、实际在滚）。
    pub fn eff_anim(&self) -> Anim {
        match self.orient {
            Orient::Vertical => Anim::Static,
            Orient::Horizontal => self.anim,
        }
    }

    /// 格言最终用的绘制色。哨兵表示跟随主色。
    pub fn quote_paint_color(&self) -> u32 {
        if self.quote_color == COLOR_UNSET {
            self.color
        } else {
            self.quote_color
        }
    }

    /// 格言基准字号（px）—— 与 `gfx::Fonts::build` 里的 `s(20, quote_scale)`
    /// 是同一把尺子（缩字阶梯另说）。过渡滑入距离等「一个 em」的量都从它推，
    /// 两边走偏就会「滑入距离和字号脱钩」。
    pub fn quote_font_px(&self) -> i32 {
        ((20 * self.scale / 100) * self.quote_scale / 100).max(6)
    }

    /// 格言字距（px）：配置值是「相对值」，按整体字号缩放。
    /// 放大字号时字距得跟着放大，否则大字号下的字距等于没加。
    pub fn quote_spacing(&self) -> i32 {
        if self.letter_spacing <= 0 {
            return 0;
        }
        ((self.letter_spacing * self.scale) as f32 / 100.0).round() as i32
    }

    /// 切换版式，并按「只在默认尺寸上做手脚」的规矩调整窗口大小。
    ///
    /// 规则（三种情况各不相同，缺一个都会让用户骂人）：
    ///   * 当前尺寸**正好等于旧形态的默认值** → 换成新形态上次的尺寸（初次就是它的默认值）
    ///   * 用户手动改过尺寸 → **不动**，只是换版式
    ///   * 无论哪种，都把「离开时的尺寸」记回旧形态的槽里，来回切才找得回来
    pub fn set_orient(&mut self, new: Orient) {
        if new == self.orient {
            return;
        }
        let old = self.orient;
        let (odw, odh) = old.default_size();
        match old {
            Orient::Horizontal => {
                self.h_w = self.w;
                self.h_h = self.h;
            }
            Orient::Vertical => {
                self.v_w = self.w;
                self.v_h = self.h;
            }
        }
        if self.w == odw && self.h == odh {
            let (nw, nh) = match new {
                Orient::Horizontal => (self.h_w, self.h_h),
                Orient::Vertical => (self.v_w, self.v_h),
            };
            self.w = nw;
            self.h = nh;
        }
        self.orient = new;
    }

    pub fn load() -> Config {
        let mut cfg = Config::default();
        let Ok(text) = std::fs::read_to_string(Self::path()) else {
            return cfg;
        };
        let mut kvs: Vec<(String, String)> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                kvs.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        // 版式必须**先**落地：`orient` 会顺手把窗口尺寸换成该形态的尺寸，
        // 而竖版默认尺寸是 300x660。若先读 `w=`/`h=`（竖版存下来的 300/660），
        // 再切版式，就会被误判成「用户把横版改成了 300x660」，
        // 把横版那对记忆槽写成 300x660 —— 以后再切回横版就找不回 620x176 了。
        for (k, v) in kvs.iter().filter(|(k, _)| k == "orient") {
            apply_kv(&mut cfg, k, v);
        }
        for (k, v) in kvs.iter().filter(|(k, _)| k != "orient") {
            apply_kv(&mut cfg, k, v);
        }
        cfg
    }

    pub fn save(&self) {
        let _ = ensure_data_dir();
        let text = format!(
            "# desk-geyan 配置。改完保存，托盘菜单「重新载入配置」即可生效。\n\
             # opacity 30-255；scale/clock_scale/quote_scale 为字号百分比；speed 为跑马灯速度(px/s)\n\
             # color / date_color 支持 #RRGGBB 或 RRGGBB；font 为字体名\n\
             # clock_dx / clock_dy 是时间日期块相对默认位置的像素偏移（布局模式下拖动即可）\n\
             x={}\n\
             y={}\n\
             w={}\n\
             h={}\n\
             opacity={}\n\
             mode={}\n\
             anim={}\n\
             always_on_top={}\n\
             show_clock={}\n\
             show_date={}\n\
             show_quote={}\n\
             seconds={}\n\
             shadow={}\n\
             speed={}\n\
             scale={}\n\
             clock_scale={}\n\
             quote_scale={}\n\
             font={}\n\
             color={}\n\
             date_color={}\n\
             clock_dx={}\n\
             clock_dy={}\n\
             layout_mode={}\n\
             visible={}\n\
             auto_hide_fs={}\n\
             placed={}\n\
             orient={}\n\
             letter_spacing={}\n\
             quote_color={}\n\
             shrink_fit={}\n\
             h_w={}\n\
             h_h={}\n\
             v_w={}\n\
             v_h={}\n",
            self.x,
            self.y,
            self.w,
            self.h,
            self.opacity,
            if self.mode == Mode::Glass { "glass" } else { "text" },
            self.anim.key(),
            self.always_on_top,
            self.show_clock,
            self.show_date,
            self.show_quote,
            self.seconds,
            self.shadow,
            self.speed,
            self.scale,
            self.clock_scale,
            self.quote_scale,
            self.font,
            fmt_color(self.color),
            fmt_color(self.date_color),
            self.clock_dx,
            self.clock_dy,
            self.layout_mode,
            self.visible,
            self.auto_hide_fs,
            self.placed,
            self.orient.key(),
            self.letter_spacing,
            // 哨兵不能过 fmt_color（那是 6 位十六进制），只能原样写成 unset
            if self.quote_color == COLOR_UNSET {
                "unset".to_string()
            } else {
                fmt_color(self.quote_color)
            },
            self.shrink_fit,
            self.h_w,
            self.h_h,
            self.v_w,
            self.v_h,
        );
        let _ = std::fs::write(Self::path(), text);
    }
}

fn parse_bool(v: &str, def: bool) -> bool {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => def,
    }
}

/// 单条 `key=value` 落到配置上。配置文件读取和命令行的
/// `--dump out.png color=#F5D98B` 走的是同一段逻辑，
/// 所以「配置里能写的」和「命令行能临时覆盖的」永远一致。
pub fn apply_kv(cfg: &mut Config, k: &str, v: &str) {
    match k {
        "x" => cfg.x = v.parse().unwrap_or(cfg.x),
        "y" => cfg.y = v.parse().unwrap_or(cfg.y),
        "w" => cfg.w = v.parse().unwrap_or(cfg.w).clamp(240, 4000),
        "h" => cfg.h = v.parse().unwrap_or(cfg.h).clamp(80, 2000),
        "opacity" => cfg.opacity = v.parse().unwrap_or(cfg.opacity).clamp(30, 255),
        "mode" => {
            cfg.mode = if v.eq_ignore_ascii_case("text") || v == "纯文字" {
                Mode::TextOnly
            } else {
                Mode::Glass
            }
        }
        "anim" => cfg.anim = Anim::parse(v),
        "always_on_top" => cfg.always_on_top = parse_bool(v, cfg.always_on_top),
        "show_clock" => cfg.show_clock = parse_bool(v, cfg.show_clock),
        "show_date" => cfg.show_date = parse_bool(v, cfg.show_date),
        "show_quote" => cfg.show_quote = parse_bool(v, cfg.show_quote),
        "seconds" => cfg.seconds = parse_bool(v, cfg.seconds),
        "shadow" => cfg.shadow = parse_bool(v, cfg.shadow),
        "speed" => cfg.speed = v.parse().unwrap_or(cfg.speed).clamp(10, 400),
        "scale" => cfg.scale = v.parse().unwrap_or(cfg.scale).clamp(30, 600),
        "clock_scale" => cfg.clock_scale = v.parse().unwrap_or(cfg.clock_scale).clamp(30, 600),
        "quote_scale" => cfg.quote_scale = v.parse().unwrap_or(cfg.quote_scale).clamp(30, 600),
        "font" => {
            if !v.is_empty() {
                cfg.font = v.to_string();
            }
        }
        "color" => {
            if let Some(c) = parse_color(v) {
                cfg.color = c;
            }
        }
        "date_color" => {
            if let Some(c) = parse_color(v) {
                cfg.date_color = c;
            }
        }
        "clock_dx" => cfg.clock_dx = v.parse().unwrap_or(cfg.clock_dx).clamp(-8000, 8000),
        "clock_dy" => cfg.clock_dy = v.parse().unwrap_or(cfg.clock_dy).clamp(-8000, 8000),
        "layout_mode" => cfg.layout_mode = parse_bool(v, cfg.layout_mode),
        "visible" => cfg.visible = parse_bool(v, cfg.visible),
        "auto_hide_fs" => cfg.auto_hide_fs = parse_bool(v, cfg.auto_hide_fs),
        "placed" => cfg.placed = parse_bool(v, cfg.placed),
        // 版式：走 set_orient 而不是裸赋值 —— 它会顺手把窗口尺寸换成新形态的默认值
        // （只在该形态还没被用户改过时），这正是 `--dump orient=vertical` 想要的。
        "orient" => cfg.set_orient(Orient::parse(v)),
        "letter_spacing" => cfg.letter_spacing = v.parse().unwrap_or(cfg.letter_spacing).clamp(0, 8),
        "quote_color" => {
            let t = v.trim();
            if t.eq_ignore_ascii_case("unset") || t.eq_ignore_ascii_case("auto") || t == "跟随主色" {
                cfg.quote_color = COLOR_UNSET;
            } else if let Some(c) = parse_color(t) {
                cfg.quote_color = c;
            }
        }
        "shrink_fit" => cfg.shrink_fit = parse_bool(v, cfg.shrink_fit),
        "h_w" => cfg.h_w = v.parse().unwrap_or(cfg.h_w),
        "h_h" => cfg.h_h = v.parse().unwrap_or(cfg.h_h),
        "v_w" => cfg.v_w = v.parse().unwrap_or(cfg.v_w),
        "v_h" => cfg.v_h = v.parse().unwrap_or(cfg.v_h),
        _ => {}
    }
}
