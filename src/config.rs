//! 配置：`%APPDATA%\DeskGeyan\config.ini`
//!
//! 手写 key=value 而不是引 serde —— 只有十来个字段，
//! 让整个程序保持零运行时依赖，`cargo build` 出来就是个能直接拷走的 exe。

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::util::{data_dir, ensure_data_dir, fmt_color, parse_color};

/// 最近一次 `save()` 的失败码（0 = 成功，`u32::MAX` = 有错但拿不到 OS 错误码）。
///
/// 托盘菜单和设置面板都弹不出错误框，写入失败会表现为「改了设置，重启后没了」——
/// 用户只会认为程序没记住。原因必须留在一个能被读出来的地方（诊断报告 / `--selftest`），
/// 否则这类问题永远只能靠猜。
static LAST_SAVE_ERR: AtomicU32 = AtomicU32::new(0);

/// 最近一次 `save()` 的失败码（0 = 无失败）
pub fn last_save_err() -> u32 {
    LAST_SAVE_ERR.load(Ordering::Relaxed)
}

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

/// 灵动岛底色预设。**全是深色**：岛压在壁纸上，浅底配亮字会糊成一片。
/// 最后一档给白色 —— 有人用亮色壁纸，深色胶囊反而看不清边界。
pub const ISLAND_BODY_PRESETS: [u32; 6] = [
    0x0B_0D12, // 近黑（默认）
    0x1C_1F26, // 深灰蓝
    0x00_0000, // 纯黑
    0x12_161F, // 墨蓝
    0x2B_2B2B, // 近黑灰
    0xFF_FFFF, // 白
];

pub fn next_island_body_color(cur: u32) -> u32 {
    cycle_u32(&ISLAND_BODY_PRESETS, cur)
}

/// 强调色复用文字色那一组预设（蓝/青/粉/绿/橙…），它们是给「一行字」用的，
/// 正是消息行的用法。
pub fn next_island_accent_color(cur: u32) -> u32 {
    cycle_u32(&COLOR_PRESETS, cur)
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

/// 桌面适配档位的两套配色。**只作用于挂件渲染**（`app/render.rs` 取色处），
/// 不改 `color` / `shadow` 本身 —— 切回「自定义」时原值仍在。
///
/// 取色依据是实测（2026-11-24，真实屏幕）：浅底 `#E2E8ED` + opacity 191 下，
/// 浅金 `#F5D98B` 的 WCAG 对比度只有 **1.03:1**（正文要 4.5:1，灰度上完全糊掉）；
/// `#3E2E0C` 在同一条件下 **5.16:1**。深底 `#0B0D12` 上 `#FFE9A8` 为 9.2:1。
///
/// 日期比正文弱一档是既有设计（`date_color` 与 `color` 本来就是两个键），
/// 所以浅底档的日期色也单独给，而不是偷懒复用正文色。
pub const FIT_DARK_INK: u32 = 0x3E2E0C;
pub const FIT_DARK_INK_DIM: u32 = 0x4A3E28;
pub const FIT_LIGHT_INK: u32 = 0xFFE9A8;
pub const FIT_LIGHT_INK_DIM: u32 = 0xC8C0B0;

/// 一帧实际要用的一组颜色。由 `Config::fit_paint` 从 `cfg` 推出来。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Paint {
    pub color: u32,
    pub date: u32,
    pub quote: u32,
    pub shadow: bool,
}

/// 档位的中文短名。面板按钮、托盘菜单、toast 共用这一份，避免三处各写一遍
/// 然后有一处忘了改。
pub fn fit_label(fit: i32) -> &'static str {
    match fit {
        1 => "浅底",
        2 => "深底",
        _ => "自定",
    }
}

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
    /// 暂停轮播：冻结换句、跑马灯与呼吸，保留当前这一句供阅读。
    pub paused: bool,
    /// 仅播放收藏夹这一组；收藏夹为空时回退到内置库，避免空画面。
    pub fav_only: bool,
    /// 文字阴影：浅色壁纸下把白字托起来
    pub shadow: bool,
    /// 桌面适配档位：0 = 自定义（`color` / `shadow` 原样）
    /// 1 = 浅色桌面（深墨金 + 强制描边）　2 = 深色桌面（亮金 + 不描边）
    ///
    /// **存在的理由**：浅色壁纸 + 浅金字的对比度实测只有 1.03:1（正文要求
    /// 4.5:1），可同一套浅金在深色壁纸上很好看 —— 与其让用户每次手改两个
    /// 颜色再赔上一个开关，不如换挡。**档位只在渲染时覆盖，不动
    /// `color` / `shadow`**，所以切回「自定义」原值还在。
    pub fit: i32,
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
    /// 字号是否跟着**当前显示器 DPI** 走（96dpi = 100%）。
    ///
    /// 关掉时行为与旧版逐像素相同（`scale` 就是最终字号百分比）；
    /// 打开时物理尺寸在 100% / 150% / 200% 屏上看起来一样大 ——
    /// 不跟着走的话，同一份配置在 4K 屏上会明显偏小，在 720p 上偏大。
    pub dpi_auto: bool,
    /// 当前显示器的 DPI 百分比（96 = 100）。
    ///
    /// **必须与物理尺寸一起落盘**：`w/h` 是已经按 DPI 缩放过的物理像素，
    /// 不记录对应的 DPI，下次启动就会把「150% 下保存的 930px」再乘 1.5。
    pub dpi: i32,
    /// 当前配置里的 `dpi` 是否来自磁盘（或新装初始化）。
    ///
    /// 旧版配置没有 `dpi=` 键，不能拿 100 当真相去乘。首次加载时按
    /// 「尺寸已经是当前屏的物理值」迁移一次，不再缩放。
    pub dpi_known: bool,

    // ---- 灵动岛（与格言**并列的独立功能**，见 `src/island/`）
    //
    // 这些键与上面那批**没有任何语义重叠**：岛不写 `w/h/x/y`，也不读
    // `visible` / `orient` / `anim`。两个功能各自开关、各自记住自己的状态。
    /// 总开关。`false` 时**不创建岛窗口**（`--selftest` 用 `FindWindowW` 验）。
    pub island: bool,
    /// 上次端点：`false` = 收起，`true` = 展开
    pub island_state: bool,
    /// 收起态片段顺序，csv（`clock,dot,cpu,mem,battery,msg`）
    ///
    /// 默认**全开**：这一排就是「电脑状态」，用户装它就是想在屏幕上看到
    /// 那三个数。缺省只给 `clock,dot` 的结果是「装完了，但得先自己翻到
    /// 设置第二页、再猜哪几个勾选框才是状态」—— 一项能力默认关着，
    /// 等于没有。不可用的项由 `content::segments` 自动剔除：台式机没有
    /// 电池就只是不画 `BAT`，不会留下一个空位或 `--` 占位。
    pub island_items: String,
    /// 收起态轮播间隔（秒），0 = 不轮播
    pub island_rotate: i32,
    /// 新消息到达后**拉长**显示来源·正文的时长（ms）。0 = 不拉长。
    ///
    /// 注意语义：拉长 ≠ 展开 —— 高度仍是收起态的 34px，只有宽度变大。
    /// 展开仍然只由悬停驱动（见 `island/mod.rs` tick 的 ②b）。
    pub island_peek_ms: i32,
    /// 新消息提示点颜色（收起态胶囊里的那个圆点）。
    ///
    /// 默认绿 `#34D399`：它表达的是「有新东西」，与强调色（信息/链接）不是
    /// 同一个语义，所以单独一个色，不借主色或强调色通道。
    pub island_dot_color: u32,
    /// 展开端点（**逻辑**像素，运行时乘 DPI）
    pub island_w: i32,
    pub island_h: i32,
    /// 胶囊底色 `0x00RRGGBB`
    pub island_body_color: u32,
    /// 胶囊底不透明度 0..=255（再乘 `opacity`）
    pub island_body_alpha: i32,
    /// 消息行强调色
    pub island_accent_color: u32,
    /// 悬停进入延时（ms）
    pub island_hover_ms: i32,
    /// 移开宽限（ms）。**明显长于** `island_hover_ms`：防手抖、防闪回
    pub island_leave_ms: i32,
    /// 上次所在屏左上角 `x,y`（多屏识别；空 = 主屏）
    pub island_screen: String,
    /// 是否统计未读数（关掉时只显示最近一条，不显示计数）
    pub island_unread: bool,
    /// 事件归档：`true` = 读过的 `.msg` 移进 `events\seen\` 留 7 天；
    /// `false` = 读完即删（也就没有历史可数，未读退回进程内累计）
    pub island_events_keep: bool,
    /// 是否采集**系统通知**（`wpndatabase.db`）。
    ///
    /// 与 `events\*.msg` 是两条并列的入口：文件是给别人留的协议口，通知是
    /// 给「本来就弹在屏幕上的那些」用的。关掉后胶囊只认文件。
    pub island_notify: bool,
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
            paused: false,
            fav_only: false,
            shadow: true,
            fit: 0,
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
            // 默认**关**：老配置（没有这个键）载入后行为逐像素不变。
            // 新装（`placed == false`）时由 `run()` 改成 true —— 那台机器上
            // 没有「旧行为」要保护，跟着系统缩放才是对的。
            dpi_auto: false,
            dpi: 100,
            dpi_known: false,
            // 岛默认**关**：新增一个常驻窗口属于强默认，老用户升级不该
            // 「屏幕上多出一条东西」。首次在设置面板里引导。
            island: false,
            island_state: false,
            // 岛本身默认关（强默认），但**开了就要有东西看**：默认给
            // 时钟 + 绿点 + 三格电脑状态。不可用的（台式机没电池）由采样层
            // 自动跳过，不会留下空位。
            //
            // 默认**不含 `msg`**：`MSG n` 会把常态再撑宽约 50px，而消息本身
            // 已经有绿点（有新消息就亮）与拉长态正文两条提示路径 —— 计数
            // 属于「我还想额外知道有多少条」的可选项，留在设置里自己勾。
            island_items: "clock,dot,cpu,mem,battery,media".to_string(),
            island_rotate: 0,
            island_peek_ms: 3000,
            island_dot_color: 0x34D399,
            island_w: 420,
            island_h: 136,
            island_body_color: 0x0B0D12,
            island_body_alpha: 220,
            island_accent_color: 0x4C9AFF,
            island_hover_ms: crate::island::geom::HOVER_IN_MS as i32,
            island_leave_ms: crate::island::geom::LEAVE_GRACE_MS as i32,
            island_screen: String::new(),
            island_unread: true,
            island_events_keep: true,
            // 默认开：用户勾了 `msg` 就是想要「有事就告诉我」，
            // 而系统通知是这条链路上唯一不依赖外部脚本的来源。
            island_notify: true,
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

    /// 档位 → 本帧真正要用的色组。**纯函数**（自检直接拿它算对比度，
    /// 不必起窗口 —— 这样「换了档位对比度到底够不够」可以在毫秒内验完）。
    ///
    /// 三档语义：
    /// - `0` 自定义：完全按 `color` / `date_color` / `quote_color` / `shadow`；
    /// - `1` 浅底：深墨金 + **强制描边**。描边不是装饰 —— 背景不可控时它是
    ///   唯一能保证轮廓可见的手段（WCAG 不把描边算进对比度，但人眼认得出）；
    /// - `2` 深底：比默认亮一档的金 + 不描边（深底上描边只会让字发脏）。
    ///
    /// **只影响挂件**：灵动岛的时钟用自己的 `cfg.color`（`island/draw.rs`），
    /// 不经过这里 —— 岛的胶囊永远是深色，跟着换色只会把时钟画糊。
    pub fn fit_paint(&self) -> Paint {
        match self.fit {
            1 => Paint {
                color: FIT_DARK_INK,
                date: FIT_DARK_INK_DIM,
                quote: FIT_DARK_INK,
                shadow: true,
            },
            2 => Paint {
                color: FIT_LIGHT_INK,
                date: FIT_LIGHT_INK_DIM,
                quote: FIT_LIGHT_INK,
                shadow: false,
            },
            _ => Paint {
                color: self.color,
                date: self.date_color,
                quote: self.quote_paint_color(),
                shadow: self.shadow,
            },
        }
    }

    /// 格言基准字号（px）—— 与 `gfx::Fonts::build` 里的 `s(20, quote_scale)`
    /// 是同一把尺子（缩字阶梯另说）。过渡滑入距离等「一个 em」的量都从它推，
    /// 两边走偏就会「滑入距离和字号脱钩」。
    pub fn quote_font_px(&self) -> i32 {
        crate::util::scaled_px(20, self.eff_scale_pct(), self.quote_scale)
    }

    /// **有效缩放百分比**：用户字号 × 显示器 DPI（`dpi_auto` 关闭时 DPI 恒 100）。
    ///
    /// 这是全程序**唯一**的字号出口：字体、内边距、跑马灯间隙、命中带宽
    /// 全部从它推。以前每个调用点各自写 `scale / 100.0`，于是「跟着屏缩放」
    /// 这种横切改动根本没地方落 —— 漏一处就是字号和内边距不同步。
    pub fn eff_scale_pct(&self) -> i32 {
        let dpi = if self.dpi_auto { self.dpi.max(48) } else { 100 };
        (((self.scale as f32) * (dpi as f32) / 100.0).round() as i32).clamp(20, 1200)
    }

    /// 有效缩放倍数（`eff_scale_pct / 100`）。内边距 / 间隙 / 阴影位移都用它。
    pub fn eff_scale(&self) -> f32 {
        self.eff_scale_pct() as f32 / 100.0
    }

    /// 版心内边距（px）。**唯一**定义：以前 `((24.0 * scale).round()).max(10)`
    /// 在 8 处各写一遍，`frame.rs` 那处还漏了 `.max(10)` —— 于是 scale<42%
    /// 时跑马灯带比版心宽 3px。收成一个方法之后，这类漂移没有发生的地方。
    pub fn pad(&self) -> i32 {
        ((24.0 * self.eff_scale()).round() as i32).max(10)
    }

    /// 跑马灯两条格言之间的间隙（px）。同样是单一出口 ——
    /// 轮转判定（f32）与绘制间距（i32）以前各算一份，取整不同就会累积跳字。
    pub fn marquee_gap(&self) -> i32 {
        (130.0 * self.eff_scale()).round() as i32
    }

    /// 格言字距（px）：配置值是「相对值」，按整体字号缩放。
    /// 放大字号时字距得跟着放大，否则大字号下的字距等于没加。
    pub fn quote_spacing(&self) -> i32 {
        if self.letter_spacing <= 0 {
            return 0;
        }
        ((self.letter_spacing * self.eff_scale_pct()) as f32 / 100.0).round() as i32
    }

    /// 某种版式在**当前 DPI** 下对应的物理默认尺寸。
    ///
    /// `w/h` 是物理像素，而 `Orient::default_size()` 是 100% 下的逻辑尺寸。
    /// 开 DPI 跟随却直接拿两者比较，150% 新装环境永远认不出「这是默认尺寸」，
    /// 切版式时就会保留错误的长宽比。
    pub fn default_size_px(&self, orient: Orient) -> (i32, i32) {
        let (w, h) = orient.default_size();
        let k = if self.dpi_auto {
            self.dpi.clamp(48, 480) as f32 / 100.0
        } else {
            1.0
        };
        ((w as f32 * k).round() as i32, (h as f32 * k).round() as i32)
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
        let (odw, odh) = self.default_size_px(old);
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
        // 旧配置没有 `dpi`，此时 `w/h` 已经是当前屏幕的物理值；
        // 标记为「未知」，由启动阶段按当前 DPI 接管一次且**不再缩放**。
        cfg.dpi_known = kvs.iter().any(|(k, _)| k == "dpi");
        cfg
    }

    pub fn save(&self) {
        let _ = ensure_data_dir();
        let text = format!(
            "# desk-geyan 配置。改完保存，托盘菜单「重新载入配置」即可生效。\n\
             # opacity 30-255；scale/clock_scale/quote_scale 为字号百分比；speed 为跑马灯速度(px/s)\n\
             # color / date_color 支持 #RRGGBB 或 RRGGBB；font 为字体名\n\
             # clock_dx / clock_dy 是时间日期块相对默认位置的像素偏移（布局模式下拖动即可）\n\
             # dpi_auto 为 true 时字号跟随所在显示器 DPI（窗口尺寸同比缩放）\n\
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
             paused={}\n\
             fav_only={}\n\
             shadow={}\n\
             fit={}\n\
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
             dpi_auto={}\n\
             dpi={}\n\
             h_w={}\n\
             h_h={}\n\
             v_w={}\n\
             v_h={}\n\
             # ---- 灵动岛（与格言并列的独立功能；关掉时不创建窗口）\n\
             # island_items 为逗号分隔的片段顺序：clock,dot,cpu,mem,battery,msg\n\
             island={}\n\
             island_state={}\n\
             island_items={}\n\
             island_rotate={}\n\
             island_peek_ms={}\n\
             island_dot_color={}\n\
             island_w={}\n\
             island_h={}\n\
             island_body_color={}\n\
             island_body_alpha={}\n\
             island_accent_color={}\n\
             island_hover_ms={}\n\
             island_leave_ms={}\n\
             island_screen={}\n\
             island_unread={}\n\
             island_events_keep={}\n\
             island_notify={}\n",
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
            self.paused,
            self.fav_only,
            self.shadow,
            self.fit,
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
            self.dpi_auto,
            self.dpi,
            self.h_w,
            self.h_h,
            self.v_w,
            self.v_h,
            self.island,
            self.island_state,
            self.island_items,
            self.island_rotate,
            self.island_peek_ms,
            fmt_color(self.island_dot_color),
            self.island_w,
            self.island_h,
            fmt_color(self.island_body_color),
            self.island_body_alpha,
            fmt_color(self.island_accent_color),
            self.island_hover_ms,
            self.island_leave_ms,
            self.island_screen,
            self.island_unread,
            self.island_events_keep,
            self.island_notify,
        );
        match write_atomic(&Self::path(), text.as_bytes()) {
            Ok(()) => LAST_SAVE_ERR.store(0, Ordering::Relaxed),
            Err(e) => LAST_SAVE_ERR.store(
                e.raw_os_error().map(|c| c as u32).unwrap_or(u32::MAX),
                Ordering::Relaxed,
            ),
        }
    }
}

/// 先写同级临时文件、再改名覆盖。
///
/// 直接 `fs::write` 到 `config.ini` 的失败模式是**写一半就没了**：磁盘满、
/// 杀进程、断电都会留下一个截断的文件，下次启动解析失败 → 位置 / 字体 /
/// DPI / 收藏夹开关全部回默认。用户看到的不是「这次没保存」，而是
/// 「配置全丢了」。
///
/// 改名在 NTFS 同卷上是原子的，最坏情况退化成「这一笔没写进去」。
/// `sync_all` 不能省：不然内容还在系统缓存里就已经改名成功，断电后
/// 得到的是一个长度为 0 的正式文件。
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let tmp = path.with_extension("ini.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // 改名失败时别把 tmp 留下：下次再失败会一直堆着
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
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
        "paused" => cfg.paused = parse_bool(v, cfg.paused),
        "fav_only" => cfg.fav_only = parse_bool(v, cfg.fav_only),
        "shadow" => cfg.shadow = parse_bool(v, cfg.shadow),
        "fit" => cfg.fit = v.trim().parse().unwrap_or(0).clamp(0, 2),
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
        "dpi_auto" => cfg.dpi_auto = parse_bool(v, cfg.dpi_auto),
        // `dpi` 与 `w/h` 是**一对**：窗口尺寸是物理像素，必须记录当时 DPI。
        "dpi" => {
            cfg.dpi = v.parse().unwrap_or(cfg.dpi).clamp(48, 480);
            cfg.dpi_known = true;
        }
        "h_w" => cfg.h_w = v.parse().unwrap_or(cfg.h_w).clamp(240, 4000),
        "h_h" => cfg.h_h = v.parse().unwrap_or(cfg.h_h).clamp(80, 2000),
        "v_w" => cfg.v_w = v.parse().unwrap_or(cfg.v_w).clamp(240, 4000),
        "v_h" => cfg.v_h = v.parse().unwrap_or(cfg.v_h).clamp(80, 2000),

        // ---- 灵动岛。未知片段名由 `Items::parse` 静默忽略（向前兼容）。
        "island" => cfg.island = parse_bool(v, cfg.island),
        "island_state" => cfg.island_state = parse_bool(v, cfg.island_state),
        "island_items" => {
            let items = crate::island::content::Items::parse(v);
            // 规范形式写回：空集合会回落到 `clock`，避免出现「一条空胶囊」
            cfg.island_items = items.to_csv();
        }
        "island_rotate" => cfg.island_rotate = v.parse().unwrap_or(cfg.island_rotate).clamp(0, 600),
        "island_peek_ms" => cfg.island_peek_ms = v.parse().unwrap_or(cfg.island_peek_ms).clamp(0, 60000),
        "island_dot_color" => {
            if let Some(c) = parse_color(v) {
                cfg.island_dot_color = c;
            }
        }
        "island_w" => cfg.island_w = v.parse().unwrap_or(cfg.island_w).clamp(120, 2000),
        "island_h" => cfg.island_h = v.parse().unwrap_or(cfg.island_h).clamp(crate::island::geom::EXPAND_MIN_H, 900),
        "island_body_color" => {
            if let Some(c) = parse_color(v) {
                cfg.island_body_color = c;
            }
        }
        "island_body_alpha" => {
            cfg.island_body_alpha = v.parse().unwrap_or(cfg.island_body_alpha).clamp(0, 255)
        }
        "island_accent_color" => {
            if let Some(c) = parse_color(v) {
                cfg.island_accent_color = c;
            }
        }
        "island_hover_ms" => cfg.island_hover_ms = v.parse().unwrap_or(cfg.island_hover_ms).clamp(20, 5000),
        "island_leave_ms" => cfg.island_leave_ms = v.parse().unwrap_or(cfg.island_leave_ms).clamp(20, 10000),
        "island_screen" => {
            // 只接受 `x,y` 形式，其余（含空串）一律按主屏处理
            let ok = v.split_once(',').map(|(a, b)| {
                a.trim().parse::<i32>().is_ok() && b.trim().parse::<i32>().is_ok()
            });
            if ok == Some(true) {
                cfg.island_screen = v.trim().to_string();
            } else {
                cfg.island_screen.clear();
            }
        }
        "island_unread" => cfg.island_unread = parse_bool(v, cfg.island_unread),
        "island_notify" => cfg.island_notify = parse_bool(v, cfg.island_notify),
        "island_events_keep" => {
            cfg.island_events_keep = parse_bool(v, cfg.island_events_keep)
        }
        _ => {}
    }
}
