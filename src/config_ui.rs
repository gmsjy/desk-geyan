//! 设置面板：一个**普通的、可点击的**顶层窗口，把所有可调项集中到一处。
//!
//! # 为什么需要它
//!
//! 挂件本体是鼠标穿透的（`WS_EX_TRANSPARENT` + 逐像素 alpha），点不到；
//! 于是过去所有配置只能从托盘右键菜单里改。菜单能用，但有两个天然的毛病：
//!
//! 1. **看不见当前值。** 只能一项项点开子菜单去看勾选态 —— 想调个字号要开四层菜单，
//!    还不知道自己现在是多少。
//! 2. **字体只能在 7 个内置预设里循环。** 想用系统里任何一个别的中文字体
//!    （思源宋体 / 霞鹜文楷 / 等线 / 寒蝉…）过去只能手改 `config.ini` 的 `font=` 键，
//!    而且改错了不会有任何提示（GDI 的 `CreateFontW` 对不存在的字体名**静默回退**
//!    成默认字体，画出来一片「不是你要的字体」）。
//!
//! 所以这里有这个窗口：一屏看全所有配置项，改一项立刻生效，字体从系统字体列表里选。
//! 现在它是**配置的唯一入口**——托盘菜单已不再重复列任何设置项，只留高频操作
//! （显示/隐藏、换句、收藏、重载、归位、退出）。
//!
//! # 三条硬约定
//!
//! 1. **一切改动都走 `app::on_menu()`。** 面板上的控件只做两件事：把命令码发出去、
//!    把 `cfg` 里的当前值读回来显示。**不在面板里自己改 `cfg`** —— 改一个设置除了
//!    赋值还要 `save()` / 重建字体 / `apply_mode()` / 重绘，漏一步就是一个
//!    「改了没反应」的 bug。托盘菜单和面板共用 `on_menu` 那一个 `match`，
//!    语义全 crate 只有一份定义。
//! 2. **只列「真的含汉字」的字体。** 挂件画的是中文格言和「2026年10月14日 星期三」，
//!    一个只有拉丁字形的字体选上去只会让整块内容变成豆腐块（□□□）而不报错。
//!    判据不能靠字体名猜 —— 走 `GetGlyphIndicesW` 逐字查字形索引，返回 `0xFFFF`
//!    就是没有这个字形（见 `font_has_cjk`）。
//! 3. **控件状态从 `cfg` 回读，不靠控件自己记。** 单选按钮的互斥、复选框的勾选态，
//!    每次改完都由 `refresh_widgets()` 按 `cfg` 重写一遍。这样面板显示的永远等于
//!    真实状态（比如切到竖版后动效会降级，面板里那个「跑马灯」立刻置灰），
//!    也**不依赖** BUTTON 类那套「靠 WS_GROUP 找同组兄弟」的隐式行为。
//!
//! # 表格即规格
//!
//! 设置区的布局不存在「手写坐标」：`table()` 里一行一个控件，写清「叫什么、占几格、
//! 点击发哪条命令」，`slots()` 按 6 格一行自动折行算出每个控件的矩形。
//! 想加一个配置项，就是往表里加一行 —— 布局、创建、刷新、自检覆盖全自动跟上。
//! 列表是 **owner-draw** 的：每个字体名用它**自己**的字形渲染 —— 这是字体选择器
//! 唯一真正有用的形态，光看「SimSun」这几个字母分不出宋体长什么样。

use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateSolidBrush, DeleteDC, DeleteObject, EndPaint,
    EnumFontFamiliesExW, ETO_CLIPPED, ExtTextOutW, FillRect, FrameRect, GetDC, GetGlyphIndicesW,
    GetMonitorInfoW, GetSysColor, GetTextExtentPoint32W, GetTextMetricsW, InvalidateRect,
    MapWindowPoints,
    MonitorFromPoint, ReleaseDC, SelectObject, SetBkColor, SetBkMode, SetTextColor, COLOR_GRAYTEXT,
    COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT, HBRUSH, HDC, HFONT,
    HGDIOBJ, LOGFONTW, MONITORINFO, PAINTSTRUCT, TEXTMETRICW,
};
use windows_sys::Win32::UI::Controls::{
    BST_CHECKED, DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_DISABLED, ODS_SELECTED, ODT_LISTBOX,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect,
    GetCursorPos, GetSystemMetrics, GetWindowRect, GetWindowTextW, LoadCursorW,
    LoadIconW, MoveWindow, RegisterClassW, SendMessageW, SetForegroundWindow, SetWindowTextW,
    ShowWindow, BM_CLICK, BM_GETCHECK, BM_SETCHECK, BN_CLICKED, BS_AUTOCHECKBOX,
    BS_AUTORADIOBUTTON, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, LBN_SELCHANGE, LB_ADDSTRING,
    LB_GETCURSEL, LB_SETCURSEL, LB_SETITEMHEIGHT, LBS_HASSTRINGS, LBS_NOINTEGRALHEIGHT,
    LBS_NOTIFY, LBS_OWNERDRAWFIXED, SM_CXSCREEN, SM_CYSCREEN, WM_CLOSE, WM_COMMAND, WM_CREATE,
    WM_CTLCOLORSTATIC, WM_DESTROY, WM_DRAWITEM, WM_GETMINMAXINFO, WM_MEASUREITEM, WM_PAINT,
    WM_SETFONT, WM_SETICON, WM_SIZE, WS_CHILD, WS_EX_APPWINDOW, WS_EX_CLIENTEDGE,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE, WS_VSCROLL, WNDCLASSW,
};

use crate::app::{
    on_menu, App, MI_ANIM_BASE, MI_AUTOHIDE, MI_AUTOSTART, MI_CLOCK, MI_CLOCK_DOWN, MI_CLOCK_RESET,
    MI_CLOCK_UP, MI_COLOR, MI_DATE, MI_DATE_COLOR, MI_FAV, MI_GLASS, MI_LAYOUT, MI_NEXT, MI_OP_DOWN,
    MI_OP_UP, MI_ORIENT_BASE, MI_PREV, MI_QUOTE, MI_QUOTE_COLOR, MI_QUOTE_DOWN, MI_QUOTE_UP,
    MI_RELOAD, MI_RESET, MI_SCALE_DOWN, MI_SCALE_UP, MI_SECONDS, MI_SHADOW, MI_SHRINK_FIT,
    MI_SPACING_DOWN, MI_SPACING_UP, MI_SPEED_DOWN, MI_SPEED_UP, MI_TEXT, MI_TOGGLE, MI_TOPMOST,
};
use crate::config::{self, Anim, Mode, Orient, FONT_PRESETS};
use crate::consts::*;
use crate::gfx;
use crate::util::{fmt_color, wide};

// ---------------------------------------------------------------- 尺寸与 id

/// 窗口类名
const CFG_CLASS: &str = "DeskGeyanConfig";
/// 左列（字体列表 / 恢复默认字体）的控件 id
const ID_FONT_LIST: i32 = 1001;
const ID_BTN_FONT_DEFAULT: i32 = 1002;
/// 设置区控件的 id 从这里起按表格顺序自增。
/// 它们和 `MI_*` 命令码是两码事：id 只用来在 `WM_COMMAND` 里找回「是哪个控件」，
/// 而「点击要干什么」由 `Ctl.cmd` 决定。
const ID_SETTINGS_BASE: i32 = 2000;

const PAD: i32 = 14;
const GAP: i32 = 10;
/// 左列宽度（字体列表 + 预览）
const LIST_W: i32 = 340;
/// 设置区行高 / 分组标题高度
const ROW_H: i32 = 28;
const SEC_H: i32 = 20;
const SEC_GAP: i32 = 6;
/// 控件比行高矮一点，垂直居中
const CTL_INSET_Y: i32 = 3;
/// 数值行内部：标签 / [−] / 值 / [+]
const NUM_LABEL_W: i32 = 74;
const NUM_STEP_W: i32 = 26;
const NUM_VAL_W: i32 = 54;
/// 颜色行内部：色块宽度 / 色块与名称的间隙 / 「改」按钮宽度
const COLOR_SWATCH_W: i32 = 16;
const COLOR_SWATCH_GAP: i32 = 4;
const COLOR_BTN_W: i32 = 40;
/// 设置区一行 = 6 格
const UNITS: i32 = 6;
/// 列表每行高度（物理像素）
const ITEM_H: i32 = 26;
/// 列表里每个字体名用它自己的字号渲染
const ITEM_PX: i32 = 18;
/// 预览区字号（物理像素）
const PREVIEW_PX: i32 = 26;
/// 预览区高度
const PREVIEW_H: i32 = 120;
const BTN_H: i32 = 28;
const BTN_W_FONT_DEFAULT: i32 = 200;
const CLIENT_W: i32 = 800;
const CLIENT_H: i32 = 620;
/// 窗口最小尺寸。比这个小，设置区就会被挤到放不下（数值行的 [+]
/// 会被挤出行外）—— 与其让用户看到半截界面，不如不许缩到那么小。
const MIN_TRACK_W: i32 = 780;
const MIN_TRACK_H: i32 = 620;

/// 判「字体含不含汉字」用的样本字。挑的都是最常用的字：一个字体连「永」
/// （CJK 覆盖率的事实标准测试字）都没有，挂件上的中文必然糊成豆腐。
const CJK_SAMPLE: &str = "永格时日";

/// 预览区的三行样例。`big` = 用预览字号（大），否则用界面字号（小）。
///
/// **绘制和自检读的是同一份常量**：分头写就会出现「断言量的是甲、画的是乙」——
/// 那种断言绿得毫无意义（这正是「写了两份实现必然走偏」在测试里的样子）。
/// 预览框的宽度是固定的 340px，所以这些句子必须真的放得下；
/// `preview_overflow()` 就是拿这份常量去量的。
const PREVIEW_LINES: [(&str, bool); 3] = [
    ("慢慢来，比较快。", true),
    ("千里之行，始于足下。", true),
    ("12:34:56 · 2026年10月14日 星期三", false),
];

// ---------------------------------------------------------------- 状态

/// 一个控件（或一块自己画的内容）在面板里的角色。
///
/// 只区分「怎么建、怎么刷新、点了算什么」这三件事 —— 具体改哪个配置项由 `Ctl.cmd`
/// （`app::MI_*`）决定，不在这里展开。否则每加一个配置项就要动一次这个枚举。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// 分组标题：不是控件，自己在 `paint` 里画
    Sec,
    /// 复选框
    Check,
    /// 单选按钮
    Radio,
    /// 数值行的「−」
    NumDown,
    /// 数值行的「+」
    NumUp,
    /// 数值行的值（只显示）
    NumVal,
    /// 数值行 / 颜色行的标签（只显示）
    Label,
    /// 颜色行的名称（只显示）
    ColorName,
    /// 颜色行的色块（自己在 `paint` 里画）
    ColorSwatch,
    /// 颜色行的「改」
    ColorNext,
    /// 动作按钮（和菜单里那一条一模一样）
    Menu,
    /// 「关闭」
    Close,
}

impl Role {
    /// 点一下会发命令的角色。自检按这个集合决定「哪些控件必须点到」。
    fn interactive(self) -> bool {
        matches!(
            self,
            Role::Check
                | Role::Radio
                | Role::NumDown
                | Role::NumUp
                | Role::ColorNext
                | Role::Menu
        )
    }
}

/// 表格里的一行。
struct W {
    role: Role,
    /// 点击时发出的命令（`app::on_menu` 的 `MI_*`）。同时决定「读哪个值来显示」。
    cmd: u32,
    /// 数值行的「+」命令。写死成对的两个码，不做 `cmd ± 1` 这种算术 ——
    /// 算术依赖「同一行两条命令的码是相邻的」这个约定，而那个约定没有任何东西守着。
    cmd2: u32,
    text: &'static str,
    /// 占几格（共 6 格）
    span: i32,
}

const fn sec(text: &'static str) -> W {
    W { role: Role::Sec, cmd: 0, cmd2: 0, text, span: UNITS }
}

const fn chk(cmd: u32, text: &'static str, span: i32) -> W {
    W { role: Role::Check, cmd, cmd2: 0, text, span }
}

const fn rad(cmd: u32, text: &'static str, span: i32) -> W {
    W { role: Role::Radio, cmd, cmd2: 0, text, span }
}

const fn num(down: u32, up: u32, text: &'static str, span: i32) -> W {
    W { role: Role::NumDown, cmd: down, cmd2: up, text, span }
}

const fn col(cmd: u32, text: &'static str, span: i32) -> W {
    W { role: Role::ColorNext, cmd, cmd2: 0, text, span }
}

const fn act(cmd: u32, text: &'static str, span: i32) -> W {
    W { role: Role::Menu, cmd, cmd2: 0, text, span }
}

/// 设置区的全部内容。**这就是规格**：`slots()` 从它算出坐标。
///
/// 顺序 = 屏幕上的顺序（从左到右、从上到下，遇 `sec` 换行）。
/// 一行放满 6 格就自动折行，所以增减控件不用手动改任何坐标。
fn table() -> Vec<W> {
    use Role::*;
    vec![
        // ---- 显示内容
        sec("显示内容"),
        chk(MI_CLOCK, "时钟", 2),
        chk(MI_DATE, "日期", 2),
        chk(MI_SECONDS, "秒", 2),
        chk(MI_QUOTE, "格言", 2),
        chk(MI_SHADOW, "文字阴影", 2),
        // ---- 窗口与行为
        sec("窗口与行为"),
        chk(MI_TOPMOST, "总在最前", 2),
        chk(MI_AUTOHIDE, "全屏时隐藏", 2),
        chk(MI_AUTOSTART, "开机自启", 2),
        chk(MI_LAYOUT, "布局模式（可拖动 / 缩放）", 3),
        chk(MI_SHRINK_FIT, "放不下时缩小字号", 3),
        rad(MI_GLASS, "毛玻璃", 2),
        rad(MI_TEXT, "纯文字", 2),
        // ---- 版式
        sec("版式"),
        rad(MI_ORIENT_BASE, "横版", 2),
        rad(MI_ORIENT_BASE + 1, "竖版（卷轴）", 3),
        // ---- 动效
        sec("动效"),
        // 跑马灯占 3 格（比另外两个宽）：竖版下它的文字会变成
        // 「跑马灯（竖版不支持）」—— 9 个字，2 格（140px）装不下，会顶到隔壁
        // 「静态」的头上。给宽一点比缩成黑话强。
        rad(MI_ANIM_BASE, "跑马灯", 3),
        rad(MI_ANIM_BASE + 1, "静态", 1),
        rad(MI_ANIM_BASE + 2, "淡入淡出", 2),
        rad(MI_ANIM_BASE + 3, "呼吸", 2),
        num(MI_SPEED_DOWN, MI_SPEED_UP, "速度", 4),
        // ---- 外观
        sec("外观"),
        num(MI_SCALE_DOWN, MI_SCALE_UP, "整体字号", 3),
        num(MI_OP_DOWN, MI_OP_UP, "不透明度", 3),
        num(MI_CLOCK_DOWN, MI_CLOCK_UP, "时钟字号", 3),
        num(MI_QUOTE_DOWN, MI_QUOTE_UP, "格言字号", 3),
        col(MI_COLOR, "文字颜色", 3),
        col(MI_DATE_COLOR, "日期颜色", 3),
        col(MI_QUOTE_COLOR, "格言颜色", 3),
        num(MI_SPACING_DOWN, MI_SPACING_UP, "格言字距", 3),
        // ---- 操作
        sec("操作"),
        act(MI_PREV, "上一条", 2),
        act(MI_NEXT, "下一条", 2),
        act(MI_FAV, "收藏当前句", 2),
        act(MI_CLOCK_RESET, "时钟归位", 2),
        act(MI_RELOAD, "重载配置 / 格言", 2),
        act(MI_RESET, "回到右上角", 2),
        act(MI_TOGGLE, "隐藏 / 显示挂件", 3),
        W { role: Close, cmd: 0, cmd2: 0, text: "关闭", span: 3 },
    ]
}

/// 面板上「点一下会发命令」的全部命令码，按屏幕顺序。
///
/// 数值行会贡献**两条**（`−` 和 `+` 是两个独立控件），所以这里推的是 `cmd` + `cmd2`。
/// 自检拿它和期望清单对数：**新加一个控件忘了写断言**，这里就会多出一个没人管的
/// 命令码，`--selftest` 立刻红。
pub fn table_cmds() -> Vec<u32> {
    let mut out = Vec::new();
    for w in table() {
        if !w.role.interactive() {
            continue;
        }
        out.push(w.cmd);
        if w.cmd2 != 0 {
            out.push(w.cmd2);
        }
    }
    out
}

/// 一个格子（可能是一个控件、一块画出来的内容、或者一段静态文字）。
#[derive(Clone, Copy)]
struct Slot {
    role: Role,
    cmd: u32,
    cmd2: u32,
    text: &'static str,
    rc: RECT,
}

/// 一个已经建出来的控件。`hwnd` 为空表示这一格是自己画的（分组标题 / 色块）。
#[derive(Clone, Copy)]
struct Ctl {
    id: i32,
    hwnd: HWND,
    role: Role,
    cmd: u32,
    cmd2: u32,
    text: &'static str,
    rc: RECT,
}

/// 设置面板的全部状态。挂在 `App` 上（和 `Fonts` / `Frame` 一样），
/// 于是窗口过程里 `app()` 一次就够，不必再引一层全局。
pub struct ConfigUi {
    hwnd: HWND,
    list: HWND,
    btn_font_default: HWND,
    /// 「字体：仅列含中文的字体，共 N 个」—— 数量要等枚举完才填，所以留个句柄
    lbl_font_title: HWND,
    /// 设置区的控件，按 `slots()` 的顺序（与 `table()` 一一对应）
    ctls: Vec<Ctl>,
    /// 列表里的字体名（去重 + 排序 + 只含中文字体）。行序即插入序。
    items: Vec<String>,
    /// 与 `items` 对齐：每行**用它自己的字体**渲染。窗口销毁时逐个 `DeleteObject`。
    item_fonts: Vec<HFONT>,
    /// 控件共用的界面字体
    ui_font: HFONT,
    /// 分组标题用的粗体（就是界面字体的粗一号）
    bold_font: HFONT,
    /// 预览区字体（当前选中字体的放大版）
    preview_font: HFONT,
    /// 选中行底色刷 / 常态底色刷 / 色块描边刷（都是系统色，跟随主题）
    sel_brush: HBRUSH,
    bg_brush: HBRUSH,
    frame_brush: HBRUSH,
    /// 预览区矩形（客户端坐标）。布局时算出来，绘制时直接用 ——
    /// 两处各算一遍迟早会走偏。
    preview_rc: RECT,
}

impl Default for ConfigUi {
    fn default() -> Self {
        ConfigUi {
            hwnd: null_mut(),
            list: null_mut(),
            btn_font_default: null_mut(),
            lbl_font_title: null_mut(),
            ctls: Vec::new(),
            items: Vec::new(),
            item_fonts: Vec::new(),
            ui_font: null_mut(),
            bold_font: null_mut(),
            preview_font: null_mut(),
            sel_brush: null_mut(),
            bg_brush: null_mut(),
            frame_brush: null_mut(),
            preview_rc: unsafe { zeroed() },
        }
    }
}

impl ConfigUi {
    pub fn is_open(&self) -> bool {
        !self.hwnd.is_null()
    }

    /// 列表里的字体项数。给 `--probe` 证明「窗口真的建起来了、列表真的填进去了」用
    /// —— 光看「open() 被调用了」证明不了控件建对没有。
    pub fn list_len(&self) -> usize {
        self.items.len()
    }

    /// 按 id 找回一个控件（`WM_COMMAND` 只给 id，不给指针）
    fn by_id(&self, id: i32) -> Option<&Ctl> {
        self.ctls.iter().find(|c| c.id == id)
    }

    /// 释放所有 GDI 资源。**新增句柄必须加进来** —— 漏一个就是一次真实泄漏，
    /// 而 `--memtest` 只会报「GDI 对象数在涨」，不会告诉你是谁。
    fn free_resources(&mut self) {
        unsafe {
            for h in self.item_fonts.drain(..) {
                if !h.is_null() {
                    DeleteObject(h as HGDIOBJ);
                }
            }
            for h in [self.ui_font, self.bold_font, self.preview_font] {
                if !h.is_null() {
                    DeleteObject(h as HGDIOBJ);
                }
            }
            for b in [self.sel_brush, self.bg_brush, self.frame_brush] {
                if !b.is_null() {
                    DeleteObject(b as HGDIOBJ);
                }
            }
        }
        self.ui_font = null_mut();
        self.bold_font = null_mut();
        self.preview_font = null_mut();
        self.sel_brush = null_mut();
        self.bg_brush = null_mut();
        self.frame_brush = null_mut();
        self.hwnd = null_mut();
        self.list = null_mut();
        self.btn_font_default = null_mut();
        self.lbl_font_title = null_mut();
        self.ctls.clear();
        self.items.clear();
    }

    fn rebuild_preview_font(&mut self, face: &str) {
        unsafe {
            if !self.preview_font.is_null() {
                DeleteObject(self.preview_font as HGDIOBJ);
            }
        }
        self.preview_font = gfx::make_font(PREVIEW_PX, FW_NORMAL, &wide(face));
    }

    /// 列表里某一行的字体。下标越界时回 null（绘制里再退到界面字体）。
    fn item_font(&self, i: usize) -> HFONT {
        self.item_fonts.get(i).copied().unwrap_or(null_mut())
    }
}

// ---------------------------------------------------------------- 字体枚举

/// 枚举系统里的字体家族名：去重 + 排序 + 去掉 `@` 竖排别名，**不做**中文过滤。
///
/// 拆出来是为了让自检能分别看到「枚举到多少个」和「过滤后剩多少个」——
/// 两个数一对比就知道过滤是不是真的在干活（见 `--selftest` 的字体列表断言）。
pub fn enumerate_font_families() -> Vec<String> {
    let mut sink: Vec<String> = Vec::new();
    unsafe {
        let dc = GetDC(null_mut());
        if dc.is_null() {
            return sink;
        }
        let mut lf: LOGFONTW = zeroed();
        // DEFAULT_CHARSET + 空字体名 = 「把所有字符集的字体家族都列一遍」
        lf.lfCharSet = DEFAULT_CHARSET as u8;
        EnumFontFamiliesExW(dc, &lf, Some(enum_proc), &mut sink as *mut _ as LPARAM, 0);
        ReleaseDC(null_mut(), dc);
    }
    // 同一家族的字重 / 斜体各回调一次，按名字去重（不区分大小写）
    sink.sort_by_key(|a| a.to_lowercase());
    sink.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    sink
}

/// 枚举系统里**能显示中文**的字体家族名。
///
/// 用 `EnumFontFamiliesExW` 而不是读注册表：后者只能拿到「装了哪些名字」，
/// 拿不到「这个字体有没有这个字形」—— 而后者才是这里真正要问的问题。
pub fn enumerate_cjk_fonts() -> Vec<String> {
    let mut names = enumerate_font_families();
    names.retain(|f| font_has_cjk(f));
    names
}

/// `EnumFontFamiliesExW` 的回调。`lparam` 是调用方给的 `&mut Vec<String>`。
unsafe extern "system" fn enum_proc(
    lf: *const LOGFONTW,
    _tm: *const TEXTMETRICW,
    _font_type: u32,
    lparam: LPARAM,
) -> i32 {
    if lf.is_null() {
        return 1;
    }
    let out = &mut *(lparam as *mut Vec<String>);
    let name = &(*lf).lfFaceName;
    // '@' 开头的是「竖排专用」别名条目（@宋体、@微软雅黑…）。它们只是同一套字形的
    // 旋转包装，普通横排选择里列出来纯属噪声 —— 竖版形态自有排版层处理。
    if name[0] == b'@' as u16 {
        return 1;
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    if len > 0 {
        out.push(String::from_utf16_lossy(&name[..len]));
    }
    1 // 继续枚举
}

/// 这个字体家族有没有 `CJK_SAMPLE` 里每个字的字形。
///
/// 判据是 `GetGlyphIndicesW`：它把字符映射成字形索引，配上
/// `GGI_MARK_NONEXISTING_GLYPHS` 时「没有的字形」回 `0xFFFF`（而不是静默回 0），
/// 于是「有没有」变成一次可判定的比较。
///
/// 注意这里创建字体时**必须**指名 face：`CreateFontW` 拿到一个不存在的名字
/// 会静默退回默认字体，那会让「有没有字形」这个问题的答案变成「问的是另一个字体」。
pub fn font_has_cjk(face: &str) -> bool {
    unsafe {
        let hf = gfx::make_font(24, FW_NORMAL, &wide(face));
        if hf.is_null() {
            return false;
        }
        let dc = CreateCompatibleDC(null_mut());
        if dc.is_null() {
            DeleteObject(hf as HGDIOBJ);
            return false;
        }
        let old = SelectObject(dc, hf as HGDIOBJ);
        let chars: Vec<u16> = CJK_SAMPLE.encode_utf16().collect();
        let mut gi = [0u16; 16];
        let n = GetGlyphIndicesW(
            dc,
            chars.as_ptr(),
            chars.len() as i32,
            gi.as_mut_ptr(),
            GGI_MARK_NONEXISTING_GLYPHS,
        );
        let ok = n as usize == chars.len() && gi[..chars.len()].iter().all(|&g| g != 0xFFFF);
        SelectObject(dc, old);
        DeleteDC(dc);
        DeleteObject(hf as HGDIOBJ);
        ok
    }
}

// ---------------------------------------------------------------- 读当前值：只读、只认命令码
//
// 下面三个函数是「一条命令码 → 当前该显示成什么」的唯一出口。它们和 `on_menu` 的
// 写入口共用同一批 `MI_*` 常量，所以「读的」和「写的」不可能指向不同的字段。

/// 复选框的状态。`MI_AUTOSTART` 特殊：它不在 `cfg` 里，注册表才是唯一状态源。
fn read_bool(a: &App, cmd: u32) -> bool {
    let c = a.cfg();
    match cmd {
        MI_CLOCK => c.show_clock,
        MI_DATE => c.show_date,
        MI_QUOTE => c.show_quote,
        MI_SECONDS => c.seconds,
        MI_SHADOW => c.shadow,
        MI_TOPMOST => c.always_on_top,
        MI_AUTOHIDE => c.auto_hide_fs,
        MI_LAYOUT => c.layout_mode,
        MI_SHRINK_FIT => c.shrink_fit,
        MI_AUTOSTART => crate::autostart::enabled(),
        _ => false,
    }
}

/// 单选项是否被选中。
fn read_radio(a: &App, cmd: u32) -> bool {
    let c = a.cfg();
    if cmd == MI_GLASS {
        return c.mode == Mode::Glass;
    }
    if cmd == MI_TEXT {
        return c.mode == Mode::TextOnly;
    }
    if (MI_ORIENT_BASE..MI_ORIENT_BASE + 2).contains(&cmd) {
        return c.orient == Orient::all()[(cmd - MI_ORIENT_BASE) as usize];
    }
    if (MI_ANIM_BASE..MI_ANIM_BASE + 4).contains(&cmd) {
        return c.anim == Anim::all()[(cmd - MI_ANIM_BASE) as usize];
    }
    false
}

/// 数值行要显示的文字。同一行的两条命令（−/+）显示同一个值。
fn num_text(a: &App, cmd: u32) -> String {
    let c = a.cfg();
    match cmd {
        MI_OP_DOWN | MI_OP_UP => format!("{}%", c.opacity * 100 / 255),
        MI_SPEED_DOWN | MI_SPEED_UP => c.speed.to_string(),
        MI_SCALE_DOWN | MI_SCALE_UP => format!("{}%", c.scale),
        MI_CLOCK_DOWN | MI_CLOCK_UP => format!("{}%", c.clock_scale),
        MI_QUOTE_DOWN | MI_QUOTE_UP => format!("{}%", c.quote_scale),
        MI_SPACING_DOWN | MI_SPACING_UP => c.letter_spacing.to_string(),
        _ => String::new(),
    }
}

/// 颜色行要显示的东西：`(色块用的 0x00RRGGBB, 名字)`。
/// 格言色可能是 `COLOR_UNSET`（= 跟随主色），色块得拿**生效值**画，
/// 名字得是「跟随主色」而不是一个十六进制 —— 显示的和实际画出来的必须是一回事。
fn color_info(a: &App, cmd: u32) -> (u32, String) {
    let c = a.cfg();
    match cmd {
        MI_COLOR => (c.color, fmt_color(c.color)),
        MI_DATE_COLOR => (c.date_color, fmt_color(c.date_color)),
        MI_QUOTE_COLOR => (c.quote_paint_color(), config::quote_color_label(c.quote_color)),
        _ => (0, String::new()),
    }
}

/// 竖版下「跑马灯」是无效的（`eff_anim` 会把它降级成静态）。
/// 菜单里那条是置灰 + 写明原因，面板里也要给出同一个信号 —— 否则用户点了
/// 跑马灯看它不动，只会以为坏了。
fn anim_label(anim: Anim, vertical: bool) -> String {
    match (anim, vertical) {
        (Anim::Marquee, true) => "跑马灯（竖版不支持）".to_string(),
        _ => anim.label().to_string(),
    }
}

/// 固定角色（不随 `cfg` 变）的期望文字；动态显示的返回 `None`。
fn ctl_expected_text(a: &App, c: &Ctl) -> Option<String> {
    match c.role {
        Role::NumVal => Some(num_text(a, c.cmd)),
        Role::ColorName => Some(color_info(a, c.cmd).1),
        Role::Radio if (MI_ANIM_BASE..MI_ANIM_BASE + 4).contains(&c.cmd) => {
            let anim = Anim::all()[(c.cmd - MI_ANIM_BASE) as usize];
            Some(anim_label(anim, a.cfg().orient == Orient::Vertical))
        }
        _ => None,
    }
}

/// 这个控件此刻该不该是可用的。
fn ctl_enabled(a: &App, c: &Ctl) -> bool {
    // 竖版下跑马灯不可选（点了也不会生效）
    !(c.role == Role::Radio && c.cmd == MI_ANIM_BASE && a.cfg().orient == Orient::Vertical)
}

// ---------------------------------------------------------------- 布局

/// 按表格把每个控件的位置算出来。**纯函数**：同样的客户区尺寸必得同样的结果，
/// 于是「创建」和「重新布局」用同一份坐标，不可能对不上。
///
/// 折行规则就一条：一行 6 格，放不下就换行；遇到分组标题强制换行。
///
/// `_ch` 现在不参与计算：行高是固定的，最小窗口高度（`MIN_TRACK_H`）已经保证
/// 整张表放得下。留这个参数是为了让签名和 `layout_children` 一致，读代码时不必
/// 怀疑「高度是不是在别处被用了」。
fn slots(cw: i32, _ch: i32) -> Vec<Slot> {
    let rx = PAD + LIST_W + GAP;
    let rw = (cw - rx - PAD).max(UNITS * 40);
    let unit = (rw / UNITS).max(40);
    let mut out: Vec<Slot> = Vec::new();
    let (mut col, mut y) = (0i32, PAD);
    // 当前这一行是不是已经放了东西。分组标题必须**另起一行**，否则它会落在
    // 上一段的最后一行上面（画出来的标题和控件叠在一起 —— 打印一遍坐标才看出来）。
    let mut line_used = false;

    for w in table() {
        if w.role == Role::Sec {
            if line_used {
                y += ROW_H;
                col = 0;
                line_used = false;
            }
            out.push(Slot {
                role: Role::Sec,
                cmd: 0,
                cmd2: 0,
                text: w.text,
                rc: RECT { left: rx, top: y, right: rx + rw, bottom: y + SEC_H },
            });
            y += SEC_H + SEC_GAP;
            continue;
        }
        if col + w.span > UNITS {
            y += ROW_H;
            col = 0;
        }
        line_used = true;
        let x0 = rx + col * unit;
        let x1 = rx + (col + w.span) * unit;
        let top = y + CTL_INSET_Y;
        let bottom = y + ROW_H - CTL_INSET_Y;
        let push = |out: &mut Vec<Slot>, role: Role, cmd: u32, cmd2: u32, text: &'static str, l: i32, r: i32| {
            out.push(Slot { role, cmd, cmd2, text, rc: RECT { left: l, top, right: r, bottom } });
        };
        match w.role {
            Role::Check | Role::Radio | Role::Menu | Role::Close => {
                push(&mut out, w.role, w.cmd, w.cmd2, w.text, x0, x1 - 2);
            }
            Role::NumDown => {
                // 标签 | [−] | 值 | [+]
                // 两个按钮**各自**都带着成对的两条命令（自己的 + 反向的），
                // 这样 `inverse_cmd()` 不必去猜「同一行的另一条叫什么」。
                push(&mut out, Role::Label, w.cmd, 0, w.text, x0 + 2, x0 + NUM_LABEL_W);
                let b = x0 + NUM_LABEL_W;
                push(&mut out, Role::NumDown, w.cmd, w.cmd2, "−", b, b + NUM_STEP_W);
                push(
                    &mut out,
                    Role::NumVal,
                    w.cmd,
                    w.cmd2,
                    "",
                    b + NUM_STEP_W,
                    b + NUM_STEP_W + NUM_VAL_W,
                );
                let p = b + NUM_STEP_W + NUM_VAL_W;
                push(&mut out, Role::NumUp, w.cmd2, w.cmd, "+", p, p + NUM_STEP_W);
            }
            Role::ColorNext => {
                // 标签 | 色块 | 名称 | [改]
                push(&mut out, Role::Label, w.cmd, 0, w.text, x0 + 2, x0 + NUM_LABEL_W);
                let s = x0 + NUM_LABEL_W;
                push(
                    &mut out,
                    Role::ColorSwatch,
                    w.cmd,
                    0,
                    "",
                    s + 2,
                    s + 2 + COLOR_SWATCH_W,
                );
                let n = s + 2 + COLOR_SWATCH_W + COLOR_SWATCH_GAP;
                push(&mut out, Role::ColorName, w.cmd, 0, "", n, (x1 - COLOR_BTN_W - 6).max(n + 20));
                push(
                    &mut out,
                    Role::ColorNext,
                    w.cmd,
                    0,
                    "改",
                    x1 - COLOR_BTN_W,
                    x1 - 2,
                );
            }
            _ => {}
        }
        col += w.span;
    }
    out
}

/// 左列 + 设置区一起摆。窗口可缩放，所以每一处都从**当前**客户区算，不缓存尺寸。
unsafe fn layout_children(c: &mut ConfigUi, cw: i32, ch: i32) {
    // ---- 设置区：表格决定，重新算一遍坐标（控件已在 WM_CREATE 建好）
    for (i, s) in slots(cw, ch).into_iter().enumerate() {
        let Some(ctl) = c.ctls.get_mut(i) else { break };
        ctl.rc = s.rc;
        if ctl.hwnd.is_null() {
            continue;
        }
        MoveWindow(
            ctl.hwnd,
            s.rc.left,
            s.rc.top,
            (s.rc.right - s.rc.left).max(8),
            (s.rc.bottom - s.rc.top).max(8),
            1,
        );
    }

    // ---- 左列：标题 → 字体列表 → [恢复默认字体] → 预览
    let preview_bottom = ch - PAD;
    let preview_top = preview_bottom - PREVIEW_H;
    let lbl_preview_y = preview_top - 20 - 4;
    let btn_y = lbl_preview_y - 8 - BTN_H;
    let list_top = PAD + 20 + 6;
    let list_h = (btn_y - 8 - list_top).max(80);

    MoveWindow(c.list, PAD, list_top, LIST_W, list_h, 1);
    MoveWindow(c.btn_font_default, PAD, btn_y, BTN_W_FONT_DEFAULT, BTN_H, 1);

    c.preview_rc = RECT { left: PAD, top: preview_top, right: PAD + LIST_W, bottom: preview_bottom };
}

// ---------------------------------------------------------------- 打开 / 关闭

/// 打开设置面板。已经开着就置前，不重复建。
pub fn open(a: &mut App) {
    if a.config.is_open() {
        unsafe {
            ShowWindow(a.config.hwnd, SW_SHOWNORMAL);
            SetForegroundWindow(a.config.hwnd);
        }
        return;
    }

    unsafe {
        let hinst = crate::app::module_handle();
        let cls = wide(CFG_CLASS);
        let mut wc: WNDCLASSW = zeroed();
        wc.style = CS_HREDRAW | CS_VREDRAW;
        wc.lpfnWndProc = Some(config_proc);
        wc.hInstance = hinst;
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW_RES);
        wc.hIcon = LoadIconW(null_mut(), IDI_APPLICATION_RES);
        // COLOR_WINDOW + 1：让系统按主题给底色（浅色主题白、深色主题深）。
        // 静态文字控件自己的类刷子是 COLOR_BTNFACE，会在白底上留下灰块 ——
        // 那一路由 `WM_CTLCOLORSTATIC` 处理（见 `config_proc`）。
        wc.hbrBackground = (COLOR_WINDOW + 1) as HBRUSH;
        wc.lpszClassName = cls.as_ptr();
        RegisterClassW(&wc);

        // 窗口尺寸由客户区反推（含标题栏与边框）
        let style = WS_OVERLAPPEDWINDOW;
        let mut rc = RECT { left: 0, top: 0, right: CLIENT_W, bottom: CLIENT_H };
        AdjustWindowRectEx(&mut rc, style, 0, WS_EX_APPWINDOW);
        let (ww, wh) = (rc.right - rc.left, rc.bottom - rc.top);
        let (x, y) = place_on_cursor_screen(ww, wh);

        let title = wide("desk-geyan 设置");
        let h = CreateWindowExW(
            // WS_EX_APPWINDOW：设置面板是普通的可交互窗口，该出现在任务栏里
            WS_EX_APPWINDOW,
            cls.as_ptr(),
            title.as_ptr(),
            style,
            x,
            y,
            ww,
            wh,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if h.is_null() {
            return;
        }
        // 子控件在 WM_CREATE 里建（`a.config.hwnd` 也在那里落定）——
        // 这里刻意不再读写 `a`，免得踩「&mut 别名观测」那口井（见 README 踩坑第 4 条）。
        let icon = LoadIconW(null_mut(), IDI_APPLICATION_RES);
        SendMessageW(h, WM_SETICON, 0, icon as LPARAM); // ICON_SMALL
        SendMessageW(h, WM_SETICON, 1, icon as LPARAM); // ICON_BIG
        ShowWindow(h, SW_SHOWNORMAL);
        SetForegroundWindow(h);
    }
}

/// 关掉设置面板（退出程序时用；用户点 X 也走同一条路）。
pub fn close(a: &mut App) {
    if a.config.is_open() {
        unsafe { DestroyWindow(a.config.hwnd) };
    }
}

/// 把窗口摆到「光标所在那块屏」的**底部居中**。
///
/// 为什么要靠下：挂件默认待在右上角，设置面板是要一边改一边看挂件的 ——
/// 摆在正中很容易把挂件整块盖住，那就成了盲改。
fn place_on_cursor_screen(ww: i32, wh: i32) -> (i32, i32) {
    unsafe {
        let mut pt: POINT = zeroed();
        GetCursorPos(&mut pt);
        let hmon = MonitorFromPoint(POINT { x: pt.x, y: pt.y }, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        let wa = if !hmon.is_null() && GetMonitorInfoW(hmon, &mut mi) != 0 {
            mi.rcWork
        } else {
            RECT {
                left: 0,
                top: 0,
                right: GetSystemMetrics(SM_CXSCREEN).max(1024),
                bottom: GetSystemMetrics(SM_CYSCREEN).max(768),
            }
        };
        let x = wa.left + ((wa.right - wa.left) - ww) / 2;
        let y = (wa.bottom - wa.top - wh - 24).max(wa.top);
        (x, wa.top + y)
    }
}

// ---------------------------------------------------------------- 子控件

/// 建一个子控件：统一在这里设字体，免得漏掉一处。
#[allow(clippy::too_many_arguments)]
unsafe fn make_child(
    parent: HWND,
    class: &str,
    ex_style: u32,
    text: &str,
    style: u32,
    id: i32,
    rc: &RECT,
    font: HFONT,
) -> HWND {
    let hinst = crate::app::module_handle();
    let cls = wide(class);
    let t = wide(text);
    let h = CreateWindowExW(
        ex_style,
        cls.as_ptr(),
        t.as_ptr(),
        style,
        rc.left,
        rc.top,
        (rc.right - rc.left).max(8),
        (rc.bottom - rc.top).max(8),
        parent,
        id as *mut std::ffi::c_void,
        hinst,
        null(),
    );
    if !h.is_null() {
        SendMessageW(h, WM_SETFONT, font as WPARAM, 1);
    }
    h
}

/// 在 `WM_CREATE` 里建所有子控件，并把字体列表灌进去。
unsafe fn create_children(a: &mut App, hwnd: HWND) {
    a.config = ConfigUi::default(); // 上次关闭时清过；这里显式重来一遍更稳
    a.config.hwnd = hwnd;

    // 界面字体：控件里有大量中文（字体名本身可能就是中文），
    // 默认的「MS Sans Serif」位图字体在中文 + 高 DPI 下又糊又难认。
    a.config.ui_font = gfx::make_font(15, FW_NORMAL, &wide("Microsoft YaHei UI"));
    a.config.bold_font = gfx::make_font(15, FW_BOLD, &wide("Microsoft YaHei UI"));
    let ui = a.config.ui_font;

    // 系统色刷（跟随主题）
    a.config.sel_brush = CreateSolidBrush(GetSysColor(COLOR_HIGHLIGHT));
    a.config.bg_brush = CreateSolidBrush(GetSysColor(COLOR_WINDOW));
    // 色块描边：白色块在白底上、深色块在深底上都会「看不见」，必须有一圈边。
    // 用 GRAYTEXT —— 浅色主题下是深灰（白底可见），深色主题下是中灰（深底可见）。
    a.config.frame_brush = CreateSolidBrush(GetSysColor(COLOR_GRAYTEXT));

    let mut rc: RECT = zeroed();
    let (mut cw, mut ch) = (CLIENT_W, CLIENT_H);
    if GetClientRect(hwnd, &mut rc) != 0 {
        cw = rc.right;
        ch = rc.bottom;
    }

    // ---- 左列（字体列表 / 预览）
    let lc = RECT { left: PAD, top: PAD, right: PAD + LIST_W, bottom: PAD + 20 };
    // 数量要等字体枚举完才知道，先建出来占位，后面 `SetWindowTextW` 补上
    a.config.lbl_font_title = make_child(
        hwnd,
        "STATIC",
        0,
        "字体：",
        WS_CHILD | WS_VISIBLE,
        0,
        &lc,
        ui,
    );
    let lp = RECT { left: PAD, top: PAD + 20, right: PAD + LIST_W, bottom: PAD + 40 };
    make_child(hwnd, "STATIC", 0, "预览：", WS_CHILD | WS_VISIBLE, 0, &lp, ui);

    let ll = RECT { left: PAD, top: PAD + 26, right: PAD + LIST_W, bottom: PAD + 300 };
    let list = make_child(
        hwnd,
        "LISTBOX",
        WS_EX_CLIENTEDGE,
        "",
        WS_CHILD
            | WS_VISIBLE
            | WS_VSCROLL
            // windows-sys 里 LBS_* / BS_* 是 i32，而 CreateWindowExW 收 u32 —— 逐个转
            | LBS_NOTIFY as u32
            | LBS_OWNERDRAWFIXED as u32
            | LBS_HASSTRINGS as u32
            | LBS_NOINTEGRALHEIGHT as u32,
        ID_FONT_LIST,
        &ll,
        ui,
    );
    a.config.list = list;
    // owner-draw 固定行高：0 表示「所有行同一高度」
    SendMessageW(list, LB_SETITEMHEIGHT, 0, ITEM_H as LPARAM);

    let lb = RECT { left: PAD, top: PAD, right: PAD + BTN_W_FONT_DEFAULT, bottom: PAD + BTN_H };
    a.config.btn_font_default = make_child(
        hwnd,
        "BUTTON",
        0,
        "恢复默认字体",
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32,
        ID_BTN_FONT_DEFAULT,
        &lb,
        ui,
    );

    // ---- 设置区：表格说几行就几行，坐标由 `slots()` 算
    for (i, s) in slots(cw, ch).iter().enumerate() {
        let id = ID_SETTINGS_BASE + i as i32;
        let (class, style) = match s.role {
            Role::Check => ("BUTTON", WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32),
            Role::Radio => ("BUTTON", WS_CHILD | WS_VISIBLE | BS_AUTORADIOBUTTON as u32),
            Role::NumDown | Role::NumUp | Role::ColorNext | Role::Menu => {
                ("BUTTON", WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32)
            }
            Role::Close => ("BUTTON", WS_CHILD | WS_VISIBLE | BS_DEFPUSHBUTTON as u32),
            Role::NumVal | Role::ColorName => ("STATIC", WS_CHILD | WS_VISIBLE | SS_CENTER),
            Role::Label => ("STATIC", WS_CHILD | WS_VISIBLE),
            // 分组标题和色块没有控件，自己在 `paint` 里画
            Role::Sec | Role::ColorSwatch => ("", 0),
        };
        let hwnd_ctl = if class.is_empty() {
            null_mut()
        } else {
            make_child(hwnd, class, 0, s.text, style, id, &s.rc, ui)
        };
        a.config.ctls.push(Ctl {
            id,
            hwnd: hwnd_ctl,
            role: s.role,
            cmd: s.cmd,
            cmd2: s.cmd2,
            text: s.text,
            rc: s.rc,
        });
    }

    // ---- 灌字体列表
    a.config.items = enumerate_cjk_fonts();
    for face in &a.config.items {
        let w = wide(face);
        SendMessageW(list, LB_ADDSTRING, 0, w.as_ptr() as LPARAM);
        a.config.item_fonts.push(gfx::make_font(ITEM_PX, FW_NORMAL, &w));
    }
    SendMessageW(list, LB_SETITEMHEIGHT, 0, ITEM_H as LPARAM);
    // 数量现在才准
    let title = wide(&format!("字体：仅列含中文的字体，共 {} 个", a.config.items.len()));
    SetWindowTextW(a.config.lbl_font_title, title.as_ptr());

    layout_children(&mut a.config, cw, ch);
    sync_panel(a);
}

/// 让面板整体回到「与 `App` 一致」：列表选中项、预览字体、每个控件的显示值。
///
/// **任何一次改动之后都要调它**。控件不自己记状态，全部从 `cfg` 回读 ——
/// 于是「改了 A 顺带把 B 也改了」这种情况（比如切竖版把跑马灯降级）在面板上
/// 一定看得见，不会出现界面和实际不一致。
fn sync_panel(a: &mut App) {
    if !a.config.is_open() {
        return;
    }
    let face = a.font_face().to_string();
    if let Some(i) = a.config.items.iter().position(|f| f.eq_ignore_ascii_case(&face)) {
        unsafe { SendMessageW(a.config.list, LB_SETCURSEL, i, 0) };
    }
    a.config.rebuild_preview_font(&face);
    refresh_widgets(a);
    unsafe { InvalidateRect(a.config.hwnd, null(), 1) };
}

/// 按 `cfg` 重写每个控件的显示状态：勾选态、可用性、动态文字。
fn refresh_widgets(a: &App) {
    for c in &a.config.ctls {
        if c.hwnd.is_null() {
            continue;
        }
        unsafe {
            match c.role {
                Role::Check => {
                    let want = if read_bool(a, c.cmd) { BST_CHECKED as usize } else { 0 };
                    SendMessageW(c.hwnd, BM_SETCHECK, want, 0);
                }
                Role::Radio => {
                    let want = if read_radio(a, c.cmd) { BST_CHECKED as usize } else { 0 };
                    SendMessageW(c.hwnd, BM_SETCHECK, want, 0);
                    EnableWindow(c.hwnd, if ctl_enabled(a, c) { 1 } else { 0 });
                }
                _ => {}
            }
            if let Some(t) = ctl_expected_text(a, c) {
                let w = wide(&t);
                SetWindowTextW(c.hwnd, w.as_ptr());
            }
        }
    }
}

/// 处理一次「设置区控件被点击」。
///
/// 复选框按**控件自己的新勾选态**和 `cfg` 比：只有真的要变才发命令。
/// 这样即使 `cfg` 被别处改过（热键 / 菜单），点一下也能正确回到用户想要的状态，
/// 而不是「翻一次」。
unsafe fn on_setting_click(a: &mut App, id: i32) -> bool {
    let Some(c) = a.config.by_id(id).copied() else { return false };
    match c.role {
        Role::Check => {
            let want = (SendMessageW(c.hwnd, BM_GETCHECK, 0, 0) as u32) == BST_CHECKED;
            if read_bool(a, c.cmd) != want {
                on_menu(a, c.cmd);
            }
        }
        Role::Radio => on_menu(a, c.cmd),
        Role::NumDown | Role::NumUp | Role::ColorNext | Role::Menu => on_menu(a, c.cmd),
        Role::Close => {
            DestroyWindow(a.config.hwnd);
            return true;
        }
        _ => {}
    }
    sync_panel(a);
    // 列表 / 预览 / 色块之外的东西不用管：`sync_panel` 已经整窗 invalidate 了
    true
}

// ---------------------------------------------------------------- 自检入口

/// **仅供自检**：把列表选中项设到 `i`，并**走与用户点击完全相同的那条路**应用它
/// （`LB_SETCURSEL` + `WM_COMMAND` / `LBN_SELCHANGE`）。
///
/// 直接改 `cfg.font` 不行 —— 要验的正是「换字体 → 重建预览字体 → 重画预览区」
/// 这条链路，绕过任何一环，断言就抓不到「重画前没清底」这类 bug。
pub fn test_select(a: &mut App, i: usize) {
    unsafe {
        let list = a.config.list;
        if list.is_null() || a.config.hwnd.is_null() {
            return;
        }
        SendMessageW(list, LB_SETCURSEL, i, 0);
        let wp = ((LBN_SELCHANGE as usize) << 16) | (ID_FONT_LIST as usize);
        SendMessageW(a.config.hwnd, WM_COMMAND, wp, list as LPARAM);
    }
}

/// **仅供自检**：面板上「点一下会发命令」的控件，按其命令码列出（屏幕顺序）。
pub fn interactive_cmds(a: &App) -> Vec<u32> {
    a.config
        .ctls
        .iter()
        .filter(|c| c.role.interactive() && c.cmd != 0)
        .map(|c| c.cmd)
        .collect()
}

/// **仅供自检**：点一下「命令码为 `cmd` 的那个控件」。
///
/// 用 `BM_CLICK` 而不是自己发 `WM_COMMAND` —— 它会把按钮的勾选态 / 单选态也一并
/// 翻过去，然后才通知父窗口，跟真人点鼠标走的是同一条路。返回 `false` 表示
/// 面板上没有这条命令的控件（断言要能发现这一点，不能静默当成「点过了」）。
pub fn test_click_cmd(a: &mut App, cmd: u32) -> bool {
    let Some(c) = a.config.ctls.iter().find(|c| c.role.interactive() && c.cmd == cmd).copied()
    else {
        return false;
    };
    if a.config.hwnd.is_null() || c.hwnd.is_null() {
        return false;
    }
    unsafe { SendMessageW(c.hwnd, BM_CLICK, 0, 0) };
    true
}

/// **仅供自检**：`cmd` 的「反命令」—— 再点一下就能回到原值的那条。
///
/// * 数值行：`−` ↔ `+`（走它们各自那条命令）
/// * 复选框 / 单选：它自己（再点一次 = 取消勾选 / 保持选中）
/// * 颜色（循环换色）：没有反命令，返回 `None`
pub fn inverse_cmd(a: &App, cmd: u32) -> Option<u32> {
    let c = a.config.ctls.iter().find(|c| c.role.interactive() && c.cmd == cmd)?;
    match c.role {
        Role::NumDown | Role::NumUp => Some(c.cmd2),
        Role::Check | Role::Radio => Some(c.cmd),
        _ => None,
    }
}

/// **仅供自检**：面板上有没有哪个控件的显示和 `cfg` 对不上。
///
/// 返回第一个不一致的描述。这是「控件状态从 `cfg` 回读」这条约定的**唯一**判据 ——
/// 它一红就说明有控件在撒谎（新建时没初始化 / 改完没刷新 / 读错了字段）。
pub fn widget_mismatch(a: &App) -> Option<String> {
    for c in &a.config.ctls {
        if c.hwnd.is_null() {
            continue;
        }
        match c.role {
            Role::Check | Role::Radio => {
                let want = if c.role == Role::Check {
                    read_bool(a, c.cmd)
                } else {
                    read_radio(a, c.cmd)
                };
                let got = unsafe { SendMessageW(c.hwnd, BM_GETCHECK, 0, 0) } as u32 == BST_CHECKED;
                if want != got {
                    return Some(format!(
                        "「{}」勾选态：显示 {} / `cfg` {}",
                        c.text, got, want
                    ));
                }
                let en = unsafe { IsWindowEnabled(c.hwnd) } != 0;
                if en != ctl_enabled(a, c) {
                    return Some(format!("「{}」可用性：显示 {} / 应为 {}", c.text, en, ctl_enabled(a, c)));
                }
            }
            _ => {
                if let Some(want) = ctl_expected_text(a, c) {
                    let got = window_text(c.hwnd);
                    if got != want {
                        return Some(format!("「{}」文字：显示「{}」/ 应为「{}」", c.text, got, want));
                    }
                }
            }
        }
    }
    None
}

/// **仅供自检**：预览区三行样例在当前字体下，有没有哪一行比预览框还宽。
///
/// 返回第一处超宽的描述（含实测宽度和可用宽度）。
///
/// 这条断言的靶子是**真发生过的那个 bug**：26px 的「千里之行，始于足下。 —— 老子」
/// 比 340px 的预览框还宽，尾巴直接压到右边的设置区上。修法有两层 ——
/// 句子改短（用户看得见的部分）+ 绘制时 `ETO_CLIPPED` 裁到框内（兜底）。
/// 而这条断言守的是第一层：**样例文字必须真的量得下**。
/// 只留裁剪不留断言的话，句子写长了画面上就是「字被切了一半」，
/// 没人会发现 —— 那正是「断言必须能红」的意义。
pub fn preview_overflow(a: &App) -> Option<String> {
    if !a.config.is_open() {
        return None;
    }
    unsafe {
        let dc = CreateCompatibleDC(null_mut());
        if dc.is_null() {
            return None;
        }
        let box_w = a.config.preview_rc.right - a.config.preview_rc.left - 12;
        let mut bad = None;
        for (text, big) in PREVIEW_LINES.iter() {
            let f = if *big { a.config.preview_font } else { a.config.ui_font };
            if f.is_null() {
                continue;
            }
            let old = SelectObject(dc, f as HGDIOBJ);
            let w = wide(text);
            let mut sz: SIZE = zeroed();
            let ok = GetTextExtentPoint32W(dc, w.as_ptr(), (w.len() - 1) as i32, &mut sz) != 0;
            SelectObject(dc, old);
            if ok && sz.cx > box_w && bad.is_none() {
                bad = Some(format!(
                    "「{}」实测 {}px > 可用 {}px",
                    text, sz.cx, box_w
                ));
            }
        }
        DeleteDC(dc);
        bad
    }
}

/// **仅供自检 / 截图**：把面板整体按 `App` 现在的状态刷一遍
/// （列表选中项、预览字体、每个控件的值）。窗口没开时什么都不做。
///
/// 「面板显示 == `cfg` 真实状态」这条约定在运行期由 `on_setting_click` 保证；
/// 但只要还有别的入口能改 `cfg`（磁贴菜单、热键、从磁盘重载），
/// 就需要一个「无条件再对齐一次」的出口 —— 就是它。
pub fn resync(a: &mut App) {
    sync_panel(a);
}

/// **仅供自检**：设置区控件的数量（含自己画的分组标题、色块）。
pub fn ctl_count(a: &App) -> usize {
    a.config.ctls.len()
}

/// **仅供自检**：`cmd` 对应的那个单选项此刻是否被选中（读的是 `cfg`，不是控件）。
pub fn radio_selected(a: &App, cmd: u32) -> bool {
    read_radio(a, cmd)
}

/// **仅供自检**：这条命令是不是单选项。
pub fn is_radio(a: &App, cmd: u32) -> bool {
    a.config
        .ctls
        .iter()
        .any(|c| c.role == Role::Radio && c.cmd == cmd)
}

/// **仅供自检**：这条命令是不是数值行的「−」。
///
/// 自检不单独点「−」，而是点同一行的「+」再点它的反向（= 这条「−」）——
/// 基线值正好在下界上的项（格言字距 0）点「−」会**合法地**什么都不改，
/// 单独点它就没法区分「没接上」和「本来就在边界上」。
pub fn is_num_down(a: &App, cmd: u32) -> bool {
    a.config
        .ctls
        .iter()
        .any(|c| c.role == Role::NumDown && c.cmd == cmd)
}

/// **仅供自检**：这条命令属于表格里的哪一行（失败信息里要说人话）。
///
/// 从**表格**里查而不是从控件里查：数值行的 `−` / `+` 两个控件自己的文字就是
/// 「−」「+」，拿它拼错误信息等于没说是什么控件坏了。
pub fn cmd_label(cmd: u32) -> String {
    for w in table() {
        if w.cmd == cmd || w.cmd2 == cmd {
            return w.text.to_string();
        }
    }
    format!("#{}", cmd)
}

/// **仅供自检**：某个颜色行的色块矩形（客户端坐标）。用来断言「色块真的跟着换色」。
pub fn swatch_rect(a: &App, cmd: u32) -> Option<RECT> {
    a.config
        .ctls
        .iter()
        .find(|c| c.role == Role::ColorSwatch && c.cmd == cmd)
        .map(|c| c.rc)
}

/// **仅供自检**：整张表在**最小窗口尺寸**下排完之后的底部余量（px）。
///
/// 返回 `None` = 最下面一行会被窗口底边切掉。面板没有滚动条，切掉就是看不见 ——
/// 而「加了两个控件之后最后一行跑出窗口」是这张表最容易犯的错，所以在自检里钉死。
pub fn plan_margin_at_min() -> Option<i32> {
    let all = slots(MIN_TRACK_W, MIN_TRACK_H);
    let bottom = all.iter().map(|s| s.rc.bottom).max().unwrap_or(0);
    let margin = MIN_TRACK_H - PAD - bottom;
    if margin >= 0 {
        Some(margin)
    } else {
        None
    }
}

/// **仅供自检**：布局里有没有两格**矩形交叠**。
///
/// 这是「表格即规格」这条路上最容易犯、又最难肉眼发现的错：换行少算一次，
/// 分组标题就落在上一段的最后一行上（标题文字和控件叠在一起）。
/// 判据是逐对求交 —— 几十个矩形两两比较，成本可以忽略，但能挡住整类错误。
/// （写这条断言之前，上面那个 bug 是靠手动打印坐标才发现的。教训：能自动查的
/// 事情不要靠眼睛。）
pub fn plan_overlaps() -> Option<String> {
    let all = slots(CLIENT_W, CLIENT_H);
    let name = |s: &Slot| -> String {
        match s.role {
            Role::Sec => format!("标题「{}」", s.text),
            Role::Check | Role::Radio | Role::Label | Role::Menu | Role::Close => {
                s.text.to_string()
            }
            Role::NumDown => format!("{} 的「−」", cmd_label(s.cmd)),
            Role::NumUp => format!("{} 的「+」", cmd_label(s.cmd)),
            Role::NumVal => format!("{} 的值", cmd_label(s.cmd)),
            Role::ColorSwatch => format!("{} 的色块", cmd_label(s.cmd)),
            Role::ColorName => format!("{} 的色名", cmd_label(s.cmd)),
            Role::ColorNext => format!("{} 的「改」", cmd_label(s.cmd)),
        }
    };
    for (i, a) in all.iter().enumerate() {
        for b in all.iter().skip(i + 1) {
            let ox = a.rc.left.max(b.rc.left) < a.rc.right.min(b.rc.right);
            let oy = a.rc.top.max(b.rc.top) < a.rc.bottom.min(b.rc.bottom);
            if ox && oy {
                return Some(format!("{} 与 {} 的矩形交叠", name(a), name(b)));
            }
        }
    }
    None
}

/// **仅供自检**：抓回设置面板**预览区**的像素，用来断言「连续换字体后不叠字」。
///
/// `PrintWindow(PW_RENDERFULLCONTENT)` 读的是窗口**已经合成好的表面**，不会替我们
/// 重走一遍 WM_PAINT —— 所以「没擦底就往上画」留下的残影，在这里是看得见的。
/// 返回 `(像素, 宽, 高)`；窗口没开或抓不回来时返回 `None`（调用方应按 INFO 处理，
/// 不假装验过）。
pub fn capture_preview(a: &App) -> Option<(Vec<u32>, i32, i32)> {
    capture_rect(a, a.config.preview_rc)
}

/// **仅供自检**：抓回设置面板**整个客户区**的像素。
pub fn capture_client(a: &App) -> Option<(Vec<u32>, i32, i32)> {
    let mut rc: RECT = unsafe { zeroed() };
    unsafe {
        if GetClientRect(a.config.hwnd, &mut rc) == 0 {
            return None;
        }
    }
    capture_rect(a, rc)
}

fn capture_rect(a: &App, r: RECT) -> Option<(Vec<u32>, i32, i32)> {
    if !a.config.is_open() {
        return None;
    }
    unsafe {
        let hwnd = a.config.hwnd;
        let mut wr: RECT = zeroed();
        if GetWindowRect(hwnd, &mut wr) == 0 {
            return None;
        }
        let (ww, wh) = (wr.right - wr.left, wr.bottom - wr.top);
        if ww <= 0 || wh <= 0 {
            return None;
        }
        // 客户区原点 → 窗口坐标（PrintWindow 的像素用的是窗口坐标系）
        let mut org = POINT { x: 0, y: 0 };
        MapWindowPoints(hwnd, null_mut(), &mut org, 1);
        let (ox, oy) = (org.x - wr.left, org.y - wr.top);

        let (pw, ph) = (r.right - r.left, r.bottom - r.top);
        if pw <= 0 || ph <= 0 {
            return None;
        }
        let full = crate::gfx::capture_window(hwnd, ww, wh)?;
        if full.len() < (ww * wh) as usize {
            return None;
        }
        let mut out = Vec::with_capacity((pw * ph) as usize);
        for y in 0..ph {
            for x in 0..pw {
                let (wx, wy) = (ox + r.left + x, oy + r.top + y);
                if wx < 0 || wy < 0 || wx >= ww || wy >= wh {
                    out.push(0);
                } else {
                    out.push(full[(wy * ww + wx) as usize]);
                }
            }
        }
        Some((out, pw, ph))
    }
}

/// 读控件现在的文字（`widget_mismatch` 用）
fn window_text(h: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let n = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

// ---------------------------------------------------------------- 绘制

/// 一行文字画在 `(x, y)`（左上角）。
unsafe fn text_out(dc: HDC, x: i32, y: i32, s: &str) {
    let w = wide(s);
    ExtTextOutW(dc, x, y, 0, null(), w.as_ptr(), (w.len() - 1) as u32, null());
}

/// 同上，但把像素**裁在 `clip` 之内**。
///
/// 预览区是一块「视口」：里面的字再长也不许越出边框。真出过事 ——
/// 26px 的「千里之行，始于足下。 —— 老子」比 340px 的预览框还宽，
/// 尾巴直接压到右边设置区上（`--probe` 的 `preview_spill` 就是为它写的）。
/// 光把句子改短是治标：用户换个更宽的字体（或把手写体调大）又会越界。
unsafe fn text_out_clip(dc: HDC, x: i32, y: i32, s: &str, clip: &RECT) {
    let w = wide(s);
    ExtTextOutW(
        dc,
        x,
        y,
        ETO_CLIPPED,
        clip,
        w.as_ptr(),
        (w.len() - 1) as u32,
        null(),
    );
}

/// `0x00RRGGBB` → `COLORREF`（`0x00BBGGRR`）。内存序和宏定义是反的，这一步不能省。
fn colorref(rgb: u32) -> u32 {
    ((rgb & 0x0000_00FF) << 16) | (rgb & 0x0000_FF00) | ((rgb >> 16) & 0x0000_00FF)
}

/// `WM_DRAWITEM`：列表每一行 —— 用该字体自己的字形渲染。
unsafe fn draw_item(c: &ConfigUi, dis: *const DRAWITEMSTRUCT) {
    let d = &*dis;
    if d.CtlType != ODT_LISTBOX || d.CtlID as i32 != ID_FONT_LIST {
        return;
    }
    let dc = d.hDC;
    let idx = d.itemID as usize;
    let Some(face) = c.items.get(idx) else {
        return;
    };
    let selected = d.itemState & ODS_SELECTED != 0;
    let disabled = d.itemState & ODS_DISABLED != 0;

    // 底色：选中用高亮色，否则用窗口底色把回收来的旧内容盖掉
    let brush = if selected { c.sel_brush } else { c.bg_brush };
    if !brush.is_null() {
        FillRect(dc, &d.rcItem, brush);
    }

    // 行内字体拿不到就退回界面字体（正常情况下不会发生）
    let mut font = c.item_font(idx);
    if font.is_null() {
        font = c.ui_font;
    }
    let old = SelectObject(dc, font as HGDIOBJ);

    SetBkMode(dc, TRANSPARENT);
    SetTextColor(
        dc,
        if disabled || !selected {
            GetSysColor(COLOR_WINDOWTEXT)
        } else {
            GetSysColor(COLOR_HIGHLIGHTTEXT)
        },
    );

    let w = wide(face);
    // 行高 26、字号 18 时上留 4px 大致垂直居中；给裁剪矩形是防超长字体名溢出到邻行
    ExtTextOutW(
        dc,
        d.rcItem.left + 8,
        d.rcItem.top + 4,
        0,
        &d.rcItem,
        w.as_ptr(),
        (w.len() - 1) as u32,
        null(),
    );

    SelectObject(dc, old);
}

unsafe fn paint(a: &mut App, hwnd: HWND) {
    let mut ps: PAINTSTRUCT = zeroed();
    let dc = BeginPaint(hwnd, &mut ps);

    let app: &App = a;
    let ui = app.config.ui_font;
    let old = SelectObject(dc, ui as HGDIOBJ);
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, GetSysColor(COLOR_WINDOWTEXT));

    // 设置区里只有两种东西需要自己画：分组标题和颜色色块。
    // 其余文字（标签 / 值 / 色号）都是真控件 —— 由系统按主题画，不用管。
    for c in &app.config.ctls {
        match c.role {
            Role::Sec => {
                let old_f = SelectObject(dc, app.config.bold_font as HGDIOBJ);
                text_out(dc, c.rc.left, c.rc.top + 2, c.text);
                // 标题下面一条细线，把分组切开
                if !app.config.frame_brush.is_null() {
                    let line = RECT {
                        left: c.rc.left,
                        top: c.rc.top + SEC_H - 4,
                        right: c.rc.right,
                        bottom: c.rc.top + SEC_H - 3,
                    };
                    FillRect(dc, &line, app.config.frame_brush);
                }
                SelectObject(dc, old_f);
            }
            Role::ColorSwatch => {
                let (rgb, _) = color_info(app, c.cmd);
                let br = CreateSolidBrush(colorref(rgb));
                if !br.is_null() {
                    FillRect(dc, &c.rc, br);
                    DeleteObject(br as HGDIOBJ);
                }
                if !app.config.frame_brush.is_null() {
                    FrameRect(dc, &c.rc, app.config.frame_brush);
                }
            }
            _ => {}
        }
    }

    // 预览区**先清底再画**。换一次字体这里就整块重画一次，而重画走的是
    // `InvalidateRect(..., bErase=false)` —— 不擦背景。不清底的话，上一个字体的
    // 字形会原样留在下面，新字体的字叠上去，用户看到的就是「多次换字体后预览区
    // 文字重叠」。这里是自带的兜底：不管调用方怎么 invalidate，画之前先填一遍底色。
    let r = app.config.preview_rc;
    if !app.config.bg_brush.is_null() {
        FillRect(dc, &r, app.config.bg_brush);
    }
    if !app.config.frame_brush.is_null() {
        FrameRect(dc, &r, app.config.frame_brush);
    }

    let pv = app.config.preview_font;
    if !pv.is_null() {
        // 行距按预览字体的**真实**行高算，不写死常数：26px 的中文字体行高普遍在
        // 34~40 之间，写死 36 会让笔画多的标题字（方正大标宋、黑体）上下行贴到一起。
        let old2 = SelectObject(dc, pv as HGDIOBJ);
        let mut tm: TEXTMETRICW = zeroed();
        GetTextMetricsW(dc, &mut tm);
        let step = ((tm.tmHeight + tm.tmExternalLeading) as i32)
            .clamp(PREVIEW_PX + 6, (PREVIEW_H - 26) / 2);
        SelectObject(dc, old2);

        for (i, (line, big)) in PREVIEW_LINES.iter().enumerate() {
            let f = if *big { pv } else { ui };
            let old_f = SelectObject(dc, f as HGDIOBJ);
            // 真要越界由 `text_out_clip` 兜住 —— 预览框必须像视口，不能像漏勺
            text_out_clip(dc, r.left + 6, r.top + 4 + step * i as i32, line, &r);
            SelectObject(dc, old_f);
        }
    }

    SelectObject(dc, old);
    EndPaint(hwnd, &ps);
}

// ---------------------------------------------------------------- 窗口过程

/// `MINMAXINFO` 的最小可跟踪尺寸。
///
/// `windows-sys` 把 `MINMAXINFO` 放在 `Win32_UI_Controls` 之外、且字段名各版本
/// 略有出入；这里只需要里面那几个 `POINT`，按 ABI 自己声明最省事
/// （5 个 `POINT`，共 40 字节，`#[repr(C)]` 逐字段对齐）。
#[repr(C)]
struct MinMaxInfo {
    pt_reserved: POINT,
    pt_max_size: POINT,
    pt_max_position: POINT,
    pt_min_track: POINT,
    pt_max_track: POINT,
}

unsafe extern "system" fn config_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let Some(a) = crate::app::app_ready() else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };
    match msg {
        WM_CREATE => {
            create_children(a, hwnd);
            0
        }

        WM_SIZE => {
            let mut rc: RECT = zeroed();
            GetClientRect(hwnd, &mut rc);
            layout_children(&mut a.config, rc.right, rc.bottom);
            0
        }

        WM_GETMINMAXINFO => {
            let mm = &mut *(lparam as *mut MinMaxInfo);
            mm.pt_min_track = POINT { x: MIN_TRACK_W, y: MIN_TRACK_H };
            0
        }

        // 静态文字控件自己的类刷子是 COLOR_BTNFACE：不管的话在白底窗口上会留下
        // 一个个灰方块。这里把底色和字色都交回给系统色 —— 于是标签看起来就是
        // 「直接印在窗口上」的。
        WM_CTLCOLORSTATIC => {
            let dc = wparam as HDC;
            SetBkColor(dc, GetSysColor(COLOR_WINDOW));
            SetTextColor(dc, GetSysColor(COLOR_WINDOWTEXT));
            a.config.bg_brush as LRESULT
        }

        WM_PAINT => {
            paint(a, hwnd);
            0
        }

        WM_MEASUREITEM => {
            let m = &mut *(lparam as *mut MEASUREITEMSTRUCT);
            if m.CtlType == ODT_LISTBOX && m.CtlID as i32 == ID_FONT_LIST {
                m.itemHeight = ITEM_H as u32;
                return 1; // TRUE：已处理
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }

        WM_DRAWITEM => {
            draw_item(&a.config, lparam as *const DRAWITEMSTRUCT);
            1 // TRUE：已处理
        }

        WM_COMMAND => {
            let id = (wparam & 0xFFFF) as i32;
            let code = ((wparam >> 16) & 0xFFFF) as u32;
            if id == ID_FONT_LIST && code == LBN_SELCHANGE {
                let sel = SendMessageW(a.config.list, LB_GETCURSEL, 0, 0) as isize;
                if sel >= 0 {
                    if let Some(face) = a.config.items.get(sel as usize).cloned() {
                        a.set_font(&face);
                        a.config.rebuild_preview_font(&face);
                        // bErase=true：换字体后预览区必须连底一起重画（否则叠字）
                        InvalidateRect(hwnd, &a.config.preview_rc, 1);
                    }
                }
            } else if id == ID_BTN_FONT_DEFAULT && code == BN_CLICKED {
                let face = FONT_PRESETS[0].to_string();
                a.set_font(&face);
                // 列表 + 预览 + 所有控件一起回到与 cfg 一致的状态
                sync_panel(a);
            } else if code == BN_CLICKED {
                on_setting_click(a, id);
            }
            0
        }

        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }

        WM_DESTROY => {
            // ⚠️ 这里**不能** PostQuitMessage —— 这是设置面板，不是主窗口。
            a.config.free_resources();
            0
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
