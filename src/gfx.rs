//! 渲染核心：GDI 画字 → 灰度当掩码 → 预乘 ARGB → UpdateLayeredWindow。
//!
//! # 为什么必须走这条路
//!
//! 一个 `< 8bpp` 的思路（LWA_COLORKEY / 全局 alpha）都会在字形边缘留下硬边，
//! 结果就是「浮在桌面上的一块黑框」。要让文字真正贴在桌面/任务栏上，
//! 每个像素必须各自带自己的 alpha —— 这就是 `UpdateLayeredWindow` 的
//! `ULW_ALPHA` + `AC_SRC_ALPHA` 组合，而它喂进去的位图必须是**预乘** ARGB。
//!
//! # 管线
//!
//! ```text
//!   ① 离屏 32bpp DIB 涂黑，GDI 用**单通道颜色**画字
//!        → 主色内容写 byte0、日期写 byte1、格言写 byte2（见 `CH_*`）
//!   ② 掩码：alpha 就是这三个字节的值（背景黑 ⇒ 未覆盖处恒为 0）
//!        → 纯 alpha 掩码，跟最终颜色无关
//!   ③ 预乘：颜色在 `compose` 里才乘 —— `c = color * alpha / 255`
//!        → **就地覆写回同一块位图**，不另设采集缓冲
//!   ④ UpdateLayeredWindow(AC_SRC_ALPHA) 上屏，DWM 按像素 alpha 合成
//! ```
//!
//! # 一趟画完三种语义：靠「颜色通道」而不是「三条独立通道」
//!
//! 时钟/分隔线、日期、格言需要三种不同的显示颜色，但**它们不必各自占一块
//! 位图**：GDI 的抗锯齿混合是**逐通道线性**的，所以把三种内容分别用「只有
//! B 分量」「只有 G 分量」「只有 R 分量」的颜色画到**同一块位图**上，三个
//! 字节互不串扰 —— 合成时读回这三个字节就是三条掩码。
//!
//! 这一条把整帧的像素趟数从 7 降到 3（旧实现是「清屏 + 画 + 采集」各三趟，
//! 外加一趟合成）。它押在「DIB 的哪个字节对应 COLORREF 的哪个分量」上，
//! 所以那条映射**不许猜** —— `--selftest` 有一条断言专门钉住它。
//!
//! 阴影走同一条管线的第二次绘制，但必须是**另一块位图**：它是「白底黑字」，
//! 底色与掩码位图（黑底）相反，合不到一起。`alpha = 255 - byte0`，
//! 它的预乘颜色恒为 (0,0,0)，所以合成时只贡献 alpha、不污染颜色 ——
//! 这就是浅色壁纸下白字仍然能立起来的原因。
//!
//! # 颜色为什么在最后一刻才乘
//!
//! 掩码层全程只用**单通道白**，所以掩码永远是纯粹的「字形覆盖率」，
//! 跟颜色无关。颜色在 `compose` 里以 `c = color * alpha / 255` 参与预乘。
//! 这样一来：
//!   * 换颜色不会影响抗锯齿质量，也不会引入任何色彩误差；
//!   * 预乘不变式 `c ≤ a` 自动成立（因为 `color ≤ 255`）；
//!   * 时钟 / 日期 / 格言可以用**三个独立颜色**，而不是把颜色画进位图里
//!     再想办法抠回来。
//!
//! 两个反直觉但必须守住的点：
//!   * 字体质量必须用 `ANTIALIASED_QUALITY`（灰度）。`CLEARTYPE_QUALITY`
//!     是次像素渲染，字缘会带上红蓝边，进掩码就变成脏边。
//!   * 滚动文字靠 `IntersectClipRect` 裁，绝对不能让字形溢出到时钟区域。

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{HWND, POINT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, GetDC,
    GetTextExtentExPointW, GetTextExtentPoint32W, IntersectClipRect, ReleaseDC, RestoreDC, SaveDC,
    PolyTextOutW, SelectObject, SetBkMode, SetTextCharacterExtra, SetTextColor, TextOutW,
    BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, POLYTEXTW,
    HDC, HFONT, HGDIOBJ, HBITMAP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::UpdateLayeredWindow;

use crate::consts::*;
use crate::util::{color_b, color_g, color_r};

// ---------------------------------------------------------------- 字体

/// 字体集合。字号变了就整体重建（GDI 字体对象不可变）。
///
/// `scale` 是整体字号，`clock_ratio` / `quote_ratio` 是时钟和格言各自的
/// 相对倍率 —— 这样「整体调大」和「只把时间调大」是两件互不干扰的事。
pub struct Fonts {
    pub face: Vec<u16>,
    pub clock: HFONT,
    pub clock_small: HFONT,
    pub date: HFONT,
    pub quote: HFONT,
    pub hint: HFONT,
    /// 格言缩字回退的两档：85% / 70%（100% 就是 `quote` 本身）。
    /// 缩字档**必须重新建字体对象再量**，不能按比例缩 `adv` ——
    /// 不同字号的 GDI 取整不一样，估算出来的宽度和实际画出来的对不上。
    pub quote_fit: [HFONT; 2],
    /// 竖排旋转字体（`lfEscapement = 900`），给 `——` / `……` 用
    pub quote_rot: HFONT,
}

/// 缩字回退的候选档（百分比）。老板定的下限是 70%。
pub const QUOTE_SHRINK_STEPS: [i32; 3] = [100, 85, 70];

impl Fonts {
    pub fn build(face: &str, scale: i32, clock_ratio: i32, quote_ratio: i32) -> Fonts {
        let f = crate::util::wide(face);
        let s = |px: i32, ratio: i32| ((px * scale / 100) * ratio / 100).max(6);
        let quote_px = s(20, quote_ratio);
        Fonts {
            clock: make_font(s(46, clock_ratio), FW_SEMIBOLD, &f),
            clock_small: make_font(s(34, clock_ratio), FW_SEMIBOLD, &f),
            date: make_font(s(16, clock_ratio), FW_NORMAL, &f),
            quote: make_font(quote_px, FW_NORMAL, &f),
            hint: make_font(s(12, 100), FW_NORMAL, &f),
            quote_fit: [
                make_font((quote_px * QUOTE_SHRINK_STEPS[1] / 100).max(6), FW_NORMAL, &f),
                make_font((quote_px * QUOTE_SHRINK_STEPS[2] / 100).max(6), FW_NORMAL, &f),
            ],
            quote_rot: make_font_ex(quote_px, FW_NORMAL, &f, 900),
            face: f,
        }
    }

    /// 取缩字档对应的字体对象。`k` 是 0/1/2，对应 100/85/70。
    pub fn quote_at(&self, k: usize) -> HFONT {
        match k {
            0 => self.quote,
            1 => self.quote_fit[0],
            _ => self.quote_fit[1],
        }
    }

    pub fn destroy(&self) {
        unsafe {
            // ⚠️ 新增句柄必须加进这个数组。漏了的话 memtest 的 GDI 计数会涨，
            // 那时看起来像泄漏，其实只是「建了不删」—— 而且没人会想到来这里找。
            let all = [
                self.clock,
                self.clock_small,
                self.date,
                self.quote,
                self.hint,
                self.quote_fit[0],
                self.quote_fit[1],
                self.quote_rot,
            ];
            for h in all {
                if !h.is_null() {
                    DeleteObject(h as HGDIOBJ);
                }
            }
        }
    }
}

pub fn make_font(px: i32, weight: i32, face: &[u16]) -> HFONT {
    make_font_ex(px, weight, face, 0)
}

/// `escapement` 是 `lfEscapement` / `lfOrientation`（单位 0.1°，900 = 顺时针 90°）。
/// 竖排里的 `——` / `……` 要旋转成竖的一条，就靠它 ——
/// 别用 `@字体名`（那是给整个字体加旋转，会把字变成躺倒的）。
pub fn make_font_ex(px: i32, weight: i32, face: &[u16], escapement: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -px, // 负值 = 字符高度（不含内部行距），排版更可控
            0,
            escapement,
            escapement,
            weight,
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

// ---------------------------------------------------------------- 掩码通道

/// 掩码位图里的通道编码 —— 一趟绘制同时写三种语义，靠的就是这三个值。
///
/// 32bpp 的 `BI_RGB` DIB 在内存里是 **BGRA**：作为小端 `u32` 读，
/// `byte0 = B`、`byte1 = G`、`byte2 = R`。而 `SetTextColor` 收的是
/// `COLORREF`（`0x00BBGGRR`），所以「哪一位控制哪个字节」是错位的：
///
/// | 想写的字节 | `SetTextColor` 该给 | 常量 |
/// | --- | --- | --- |
/// | `byte0` | `0x00FF_0000`（BB 位） | `CH_CLOCK` |
/// | `byte1` | `0x0000_FF00`（GG 位） | `CH_DATE` |
/// | `byte2` | `0x0000_00FF`（RR 位） | `CH_QUOTE` |
///
/// **必须是满值 `FF`**：掩码强度就是写进去的字节值，用 `FE` 会让覆盖率
/// 永久缺 1/255，合成的 alpha 跟着差一档 —— 那就不再是「逐像素等价」的
/// 优化，而是一次真实的画质降级。
///
/// **这张映射表是实测出来的，不是推的**：`--selftest` 里那条「掩码通道
/// 映射与串扰」断言逐个画一遍、读回字节。当初代码注释里写的「掩码取 R
/// 通道」其实是错的（真实取到的是 `byte0`，恰好 `WHITE` 三通道全满，
/// 错了也看不出来）—— 所以这次把结论钉在断言上，而不是钉在注释上。
pub const CH_CLOCK: u32 = 0x00FF_0000;
pub const CH_DATE: u32 = 0x0000_FF00;
pub const CH_QUOTE: u32 = 0x0000_00FF;

/// 主色通道上的「弱化」档（分隔线 / 布局提示）。旧实现用 `0x505050`
/// 这类灰值，在只读 `byte0` 的掩码里等于 `0x50`；这里等价地只填 B 位。
pub const fn ch_dim(level: u32) -> u32 {
    level << 16
}

/// 只给自检/基准用的渲染开关。**生产构建里两个都是 `false`**（`Default`），
/// 由 `--selftest` / `--perf` 在进程启动时一次性置位。
///
/// # 为什么是 `OnceLock` 而不是两个 `AtomicBool`
///
/// 这两个开关有个共同的坏味道：**它们的生命周期比一次调用长得多，但读得比
/// 谁都频繁**。合成每像素查一次、绘制每帧查一次，而置位只发生在进程启动那
/// 一下。原来的写法是「读 = 一次 atomic load，写 = 随时可写」：
///
/// * 好处是省事，坏处是**没有任何东西保证它会被复位**。`--selftest` 里
///   合批断言要在「合批 / 逐字」之间来回切四次，中间只要有一条早退路径
///   （`return` / panic），开关就留在错误的一边，而后面每一条断言都会读到
///   一个被污染的全局 —— 那种 bug 报出来的现象和「渲染真坏了」一模一样。
/// * 更糟的是它把「测试夹具的状态」和「被测代码的行为」用同一个变量连着，
///   于是夹具的一个笔误会变成被测对象的永久损伤。
///
/// `OnceLock` 把这两件事按时间切开：**置位只允许一次，且必须在任何渲染
/// 之前**（`App::new` 之前由 selftest/perf 做）。之后 `get()` 拿到的是
/// 一份不可变快照，谁来读都是同一份 —— 忘复位的可能性从根上消失，
/// 因为压根没有「复位」这个动作。
///
/// 每帧多出来的一次 `OnceLock::get` 是 `Relaxed` 序的一次原子读 + 分支预测，
/// 实测进不了 `--perf` 任何一档的误差范围（合成 687µs/帧，读它 ~1ns）。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct DevFlags {
    /// 合成前把掩码快照下来（`Frame::snap`）。
    ///
    /// `compose` 是**就地**把掩码覆写成预乘 ARGB 的，之后 `ink_mask()` 再也读不到
    /// 掩码 —— 而「阴影有没有跑到没字形的地方」这类断言必须看掩码（合成完的像素
    /// 上分不出来）。
    pub keep_mask: bool,
    /// 强制退回「逐字一次 `TextOutW`」，当合批的比对基准。
    ///
    /// 存在的意义是**证明合批没有改变任何一个像素**。不然「性能优化」就只是
    /// 一句没人验证过的话 —— 而合批的可行性本来就是一个**关于 GDI 的假设**
    /// （假设它按「步进 + 字距」推进字符，与 `GetTextExtentExPointW` 差分出的
    /// 步进一致）。假设必须能被证伪，否则它迟早会在某个字体上悄悄不成立。
    pub no_batch: bool,
}

/// 进程级的 `DevFlags`。`None` = 还没置位（生产路径走这条）。
static DEV_FLAGS: std::sync::OnceLock<DevFlags> = std::sync::OnceLock::new();

/// 置位。**重复调用会静默保留第一次的那份**（`OnceLock::set` 的语义），
/// 并返回实际生效的那份 —— 调用方可以据此发现自己是不是抢晚了。
///
/// 之所以不 `expect`/panic：置位失败只可能因为前面已经置过一次，而那正是
/// 自检与基准共用同一个进程时的正常情形（`--selftest` 内部按用例切口径）。
/// 把这种情形判成错误只会逼着调用方绕路，而返回实际值就够它判断了。
pub fn set_dev_flags(f: DevFlags) -> DevFlags {
    let _ = DEV_FLAGS.set(f);
    dev_flags()
}

/// 当前生效的 `DevFlags`。未置位 = 两个开关全关。
pub fn dev_flags() -> DevFlags {
    DEV_FLAGS.get().copied().unwrap_or_default()
}

/// 一块 32bpp 顶朝下的 DIB。顶朝下（biHeight 为负）是为了让像素索引
/// `y * w + x` 直接对应屏幕坐标，省掉翻转。
pub struct Dib {
    bmp: HBITMAP,
    pub dc: HDC,
    old: HGDIOBJ,
    bits: *mut u32,
    w: i32,
    h: i32,
}

impl Dib {
    pub fn new(w: i32, h: i32) -> Option<Dib> {
        if w <= 0 || h <= 0 {
            return None;
        }
        unsafe {
            let screen = GetDC(null_mut());
            let dc = CreateCompatibleDC(screen);
            let mut bmi: BITMAPINFO = zeroed();
            bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h;
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut c_void = null_mut();
            let bmp = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            ReleaseDC(null_mut(), screen);
            if bmp.is_null() || bits.is_null() {
                if !dc.is_null() {
                    DeleteDC(dc);
                }
                if !bmp.is_null() {
                    DeleteObject(bmp as HGDIOBJ);
                }
                return None;
            }
            let old = SelectObject(dc, bmp as HGDIOBJ);
            Some(Dib {
                bmp,
                dc,
                old,
                bits: bits as *mut u32,
                w,
                h,
            })
        }
    }

    /// 像素视图。要可变引用是因为调用方接下来会直接写它 ——
    /// DIB 的 bits 只是裸指针，可变性得靠签名表达。
    pub fn pixels(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h) as usize) }
    }

    /// 整块填成一个不带 alpha 的颜色（alpha 由掩码阶段决定）
    pub fn clear(&mut self, rgb: u32) {
        self.pixels().fill(rgb & 0x00FF_FFFF);
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bmp as HGDIOBJ);
            DeleteDC(self.dc);
        }
    }
}

// ---------------------------------------------------------------- 帧

/// 一帧的渲染目标。只有**两块**位图：
///
///   * `ink` —— 绘制阶段装三条掩码（`CH_CLOCK` / `CH_DATE` / `CH_QUOTE`
///     各占一个字节），`compose` 就地把预乘 ARGB 覆写回**同一块内存**，
///     所以它同时就是喂给 `UpdateLayeredWindow` 的最终位图；
///   * `shad` —— 阴影掩码（白底黑字）。底色与 `ink` 相反（黑底），
///     两块合不到一起，只能分开。
///
/// 旧实现是「一块草稿位图 + 三个 `Vec<u8>` 掩码数组 + 一块输出位图」，
/// 每帧跑 7 趟 `w*h`（3 清屏 + 3 采集 + 1 合成）；现在 3 趟。
///
/// **阴影为什么仍是「重画一遍」而不是从掩码位移采样**：试过采样（省掉
/// 一整趟绘制 + 一次清屏，竖版还能再快 400µs），但 A/B 比对显示新旧输出
/// 有 2% 的像素不同、最大差 65 级灰度，且**全部朝同一个方向** —— 那不是
/// 抗锯齿取整（那应当有明有暗），而是 GDI 在「位移后重新光栅化」与
/// 「整数平移同一份光栅结果」之间真的不等价。一个解释不了的差异不能放行，
/// 而性能的大头本来就在 `PolyTextOutW`（竖版 83 次调用 → 1 次），
/// 所以这里换回重画，把「逐像素等价」这条保证拿回来。
pub struct Frame {
    pub w: i32,
    pub h: i32,
    ink: Option<Dib>,
    shad: Option<Dib>,
    shadows_on: bool,
    /// 合成前的掩码快照，只给自检用 —— `compose` 就地覆写 `ink`，
    /// 之后再也读不到掩码了，而「阴影有没有跑到没字形的地方」这类断言
    /// 必须看掩码（合成完的像素上分不出来）。生产路径不打开它为 0 开销。
    ///
    /// 存的是**原样的 u32**、不是「三通道取 max」：`--selftest` 要能按通道
    /// 分开读 —— 「日期画到了时钟那条通道上」这种错只有看原字节才看得见，
    /// 取了 max 就只剩一句「这里确实有墨」。
    snap: Option<Vec<u32>>,
}

impl Frame {
    pub fn new(w: i32, h: i32) -> Frame {
        let mut f = Frame {
            w,
            h,
            ink: None,
            shad: None,
            shadows_on: false,
            snap: None,
        };
        f.ensure(w, h);
        f
    }

    pub fn ensure(&mut self, w: i32, h: i32) -> bool {
        if w <= 0 || h <= 0 {
            return false;
        }
        if self.w == w && self.h == h && self.ink.is_some() && self.shad.is_some() {
            return true;
        }
        self.ink = Dib::new(w, h);
        self.shad = Dib::new(w, h);
        self.snap = None;
        self.w = w;
        self.h = h;
        self.ink.is_some() && self.shad.is_some()
    }

    pub fn set_shadow(&mut self, on: bool) {
        self.shadows_on = on;
    }

    /// 掩码位图的 DC（用于测量文字；调用前记得 `SelectObject` 换字体）。
    /// 测量只读文字尺寸、不读像素，所以复用绘制那块位图没有副作用。
    pub fn dc(&self) -> HDC {
        self.ink.as_ref().map(|d| d.dc).unwrap_or(null_mut())
    }

    /// 开始**掩码趟**：清成黑底，随后用 `CH_*` 三种颜色把内容画上去。
    ///
    /// 清屏不能省：掩码读的就是「没画到的地方为 0」。**日期关掉时也不需要
    /// 单独清一条通道** —— 整块位图每帧重清，不画就是 0。
    pub fn begin_ink(&mut self) -> HDC {
        self.snap = None;
        let d = self.ink.as_mut().expect("frame not ready");
        d.clear(BLACK);
        d.dc
    }

    /// 开始**阴影趟**：白底黑字 ⇒ `覆盖率 = 255 - byte0`。
    /// 阴影关掉时不必调用 —— `compose` 见到 `shadow_k == 0` 就不读它。
    pub fn begin_shadow(&mut self) -> HDC {
        let d = self.shad.as_mut().expect("frame not ready");
        d.clear(WHITE);
        d.dc
    }

    /// 掩码位图的**字形覆盖**（三个通道取最大），只给自检用。
    ///
    /// 「哪条通道」要看单条通道就调 `channel_mask`；这里回答的是
    /// 「此处有没有字形」—— 通道是按**位置**分的（时钟在左、日期在下、
    /// 格言在版心），不少断言只关心「有没有」。
    ///
    /// **必须在 `compose` 之前调用** —— 合成会把掩码就地覆写成预乘 ARGB。
    pub fn ink_mask(&self) -> Vec<u8> {
        if let Some(s) = &self.snap {
            return s
                .iter()
                .map(|p| (p & 0xFF).max((p >> 8) & 0xFF).max((p >> 16) & 0xFF) as u8)
                .collect();
        }
        let Some(d) = self.ink.as_ref() else {
            return Vec::new();
        };
        let px = unsafe { std::slice::from_raw_parts(d.bits, (d.w * d.h) as usize) };
        px.iter()
            .map(|p| (p & 0xFF).max((p >> 8) & 0xFF).max((p >> 16) & 0xFF) as u8)
            .collect()
    }

    /// **单条掩码通道**的覆盖率，只给自检用。`0` = `CH_CLOCK`（时钟 / 分隔线 /
    /// 界面提示）、`1` = `CH_DATE`、`2` = `CH_QUOTE`。
    ///
    /// 存在的理由是「通道即语义」这个机制本身需要一条断言：`compose` 把三个
    /// 字节分别当成三条掩码，谁落在哪个字节一旦错位，画面**照样好看**，只是
    /// 颜色整体对不上号 —— 那种错只能靠直接读字节抓。
    pub fn channel_mask(&self, k: u32) -> Vec<u8> {
        match &self.snap {
            Some(s) => s.iter().map(|p| ((p >> (8 * k)) & 0xFF) as u8).collect(),
            None => Vec::new(),
        }
    }

    /// 阴影覆盖率（`255 - byte0`，因为阴影位图是**白底黑字**）—— 与 `compose`
    /// 读的是**同一块** `shad` 位图，只给自检用。
    ///
    /// **必须在 `compose` 之前调用**：`compose` 不写 `shad`，但它会就地覆写
    /// `ink`；两条掩码一起取、时机统一，才不容易在某次改动里走偏。
    pub fn shadow_mask(&self) -> Vec<u8> {
        let Some(d) = self.shad.as_ref() else {
            return Vec::new();
        };
        let px = unsafe { std::slice::from_raw_parts(d.bits, (d.w * d.h) as usize) };
        px.iter().map(|p| 255 - (p & 0xFF) as u8).collect()
    }

    /// 合成预乘 ARGB —— **就地覆写掩码位图**。
    ///
    /// `opacity` 是整体不透明度，`shadow_k` 是阴影强度（0 表示本帧没有阴影层），
    /// `color` / `date_color` / `quote_color` 是 `0x00RRGGBB` 逻辑顺序的三个显示色。
    ///
    /// `bands` 是**逐带调制表**（PR3）：格言通道（byte2）乘 `qk`、日期通道
    /// （byte1）乘 `dk`，时钟通道（byte0）永不参与 —— 换句过渡「只动格言」的
    /// 落点。通带（绝大多数帧：横版 / 退场 / 空档 / 稳态）下 `qk == 255` 时
    /// 与旧路径**逐位相同**（`(x * 255) / 255 == x` 对整数恒成立）。
    /// 竖版入场按列错峰：每列一条带，右列先亮；出处列是日期通道单独一条。
    ///
    /// 颜色在这里才乘进去：`c = color * alpha / 255`。因为 `color ≤ 255`，
    /// 预乘不变式 `c ≤ a` 恒成立 —— 不需要任何夹紧也不会违反 DWM 的约定。
    /// 阴影是纯黑，只把 alpha 抬上去、不动颜色分量，于是合成回来就是「暗边」。
    ///
    /// 三条彩色掩码就是 `ink` 位图的三个字节（见 `CH_*`），**不再需要「版心矩形」
    /// 给格言分区** —— 谁有内容谁就在自己那条通道上，读出来自然分得开。
    /// 三块内容天生不重叠；万一用户把它们拖叠在一起，取最强的一路
    /// （平局按 格言 > 日期 > 主色），不会出现颜色混叠。
    ///
    /// 阴影来自**独立的一块位图** `shad`（白底黑字，覆盖率 = `255 - byte0`），
    /// **不是**从 `ink` 位移采样 —— 采样看着等价、实测不等价，理由见上文
    /// `Frame` 的文档注释。阴影的过渡缩放**不在这里做**：由阴影趟按列写
    /// 灰度实现（见 `BandTable` 的注释）。
    pub fn compose(
        &mut self,
        opacity: u8,
        shadow_k: u8,
        bands: &BandTable,
        color: u32,
        date_color: u32,
        quote_color: u32,
    ) {
        let sk = if self.shadows_on { shadow_k as u32 } else { 0 };
        let op = opacity as u32;
        let (cr, cg, cb) = (color_r(color), color_g(color), color_b(color));
        let (dr, dg, db) = (color_r(date_color), color_g(date_color), color_b(date_color));
        let (qr, qg, qb) = (
            color_r(quote_color),
            color_g(quote_color),
            color_b(quote_color),
        );
        let (w, h) = (self.w, self.h);
        let n = (w as usize) * (h as usize);

        // 读一次就够整帧用：`DevFlags` 是 `Copy`，而 `compose` 内层是逐像素的循环，
        // 不可能每像素都去 `OnceLock::get` —— 那会把一次原子读放大成 n 次。
        let flags = dev_flags();
        if flags.keep_mask {
            if let Some(d) = self.ink.as_ref() {
                let src = unsafe { std::slice::from_raw_parts(d.bits, n) };
                self.snap = Some(src.to_vec());
            }
        }

        // `ink` 当输出（可变），`shad` 当阴影源（只读），所以要同时拿到两块。
        // 分两次借 `self.ink` / `self.shad` 会被借用检查挡住；而位图在
        // `ensure()` 之后地址稳定、`Drop` 之前不会搬走，取裸指针是安全的。
        let Some(ink_d) = self.ink.as_ref() else {
            return;
        };
        let ink_ptr = ink_d.bits;
        let shad_ptr = self.shad.as_ref().map(|d| d.bits);
        let wi = w as usize;

        let px = unsafe { std::slice::from_raw_parts_mut(ink_ptr, n) };
        // 阴影关掉时连读都不读：`sk == 0` 时 `sa` 恒为 0，那块位图根本没被画过。
        let sh: &[u32] = match (sk, shad_ptr) {
            (0, _) | (_, None) => &[],
            (_, Some(p)) => unsafe { std::slice::from_raw_parts(p, n) },
        };

        for y in 0..h {
            for x in 0..w {
                let i = (y as usize) * wi + (x as usize);

                let p = px[i];
                // 阴影位图同一点的值（白底黑字 ⇒ 覆盖率 = 255 - byte0）。
                // 阴影趟画的是「整体位移 (off,off) 之后的那批字形」，位移
                // 已经体现在位图内容里，这里不做任何坐标换算。
                let shv = if sh.len() > i { sh[i] & 0xFF } else { 255 };

                // ---- 快路径：三条通道全空。
                //
                // 这块挂件九成以上的像素是背景，而它们全都要陪跑下面那一整套
                // 「乘不透明度 + 比三路 + 乘颜色」。实测（`--perf` 的帧内拆解）
                // 合成一趟曾是整帧的一半以上，绝大部分就烧在这些空像素上。
                //
                // 位图刚被 `begin_ink` 清过零，所以空像素**本来就是 0** ——
                // 预乘 ARGB 下 alpha=0 就要求 RGB 也全 0，于是连写都不必写。
                // 唯一不能跳过的是「有阴影落在这里」：阴影是字形位移过来的，
                // 出现在「没有字形」的像素上正是它的常态。
                // 这条路径与下面那条的结果**逐位相等**（`ta=0` 时原式
                // `a = 0 + sa*(255-0)/255 = sa`，颜色分量恒为 0），
                // 所以它是纯加速，不是近似。
                //
                // **查带也走这条捷径**：无墨无影的像素一次 `get` 都不做
                // （带表最多 16 项线性扫描，只该花在有内容的像素上）。
                if p & 0x00FF_FFFF == 0 {
                    if shv == 255 {
                        continue;
                    }
                    let sa = ((255 - shv) * sk * op) / (255 * 255);
                    if sa == 0 {
                        continue;
                    }
                    px[i] = sa << 24;
                    continue;
                }

                let (bq, bd) = bands.get(x as u16);
                let sa = if shv == 255 {
                    0
                } else {
                    ((255 - shv) * sk * op) / (255 * 255)
                };

                let ma = ((p & 0xFF) as u32 * op) / 255;
                let da = ((((p >> 8) & 0xFF) as u32 * op) / 255) * bd / 255;
                // 带系数 255 时 `(x * 255) / 255 == x` 对整数恒成立
                // ⇒ 与旧路径逐位相同
                let qa = ((((p >> 16) & 0xFF) as u32 * op) / 255) * bq / 255;
                // `qa` 先缩放再进下面的最强通道比较 —— 于是「时钟压在格言上」
                // 的重叠区在格言淡出过程中不淡（时钟更强），这与「取最强通道」
                // 的既有语义一致，不是特例。

                let (ta, r, g, b) = if qa >= da && qa >= ma {
                    (qa, qr * qa / 255, qg * qa / 255, qb * qa / 255)
                } else if da >= ma {
                    (da, dr * da / 255, dg * da / 255, db * da / 255)
                } else {
                    (ma, cr * ma / 255, cg * ma / 255, cb * ma / 255)
                };

                let a = if sa == 0 {
                    ta
                } else {
                    ta + (sa * (255 - ta)) / 255
                };
                px[i] = (a << 24) | (r << 16) | (g << 8) | b;
            }
        }
    }

    /// 合成后的预乘 ARGB，供自检与调试读取
    pub fn out_pixels(&self) -> &[u32] {
        match self.ink.as_ref() {
            Some(d) => unsafe { std::slice::from_raw_parts(d.bits, (d.w * d.h) as usize) },
            None => &[],
        }
    }

    /// 上屏：`UpdateLayeredWindow` + `AC_SRC_ALPHA`，DWM 逐像素合成。
    pub fn present(&self, hwnd: HWND, x: i32, y: i32) -> bool {
        let Some(d) = self.ink.as_ref() else {
            return false;
        };
        let pt_dst = POINT { x, y };
        let pt_src = POINT { x: 0, y: 0 };
        let size = SIZE {
            cx: self.w,
            cy: self.h,
        };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA,
        };
        unsafe {
            UpdateLayeredWindow(
                hwnd,
                null_mut(),
                &pt_dst,
                &size,
                d.dc,
                &pt_src,
                0,
                &blend,
                ULW_ALPHA,
            ) != 0
        }
    }
}

/// 合成时的**逐带调制表**（PR3 竖版列错峰 + 出处随格言）。
///
/// 每条带是一个 x 区间 `[x0, x1)` 和该区间内两个通道的覆盖率系数：
/// `qk` 乘格言通道（byte2）、`dk` 乘日期通道（byte1）。时钟通道（byte0）
/// **永不参与调制** —— 时钟是静帧，是空间锚点。
///
/// 带的语义由调用方（`app::frame_mod`）定：横版 / 退场 / 空档 = 一条通带；
/// 竖版入场 = 每列一条（右列先亮）；出处列 = 日期通道单独一条（竖版，
/// x 区间在主文之左，与列带不相交）。
///
/// **阴影不走这里**：阴影的缩放由阴影趟按列写灰度实现（`255(1-k)`）——
/// 竖版绘制本就按字分段，按列换灰度零额外提交，且 `shad` 位图仍是
/// 阴影缩放的唯一真相源（`shadow_mask()` 断言直接读它）。
///
/// 容量 16：竖版主文 4~12 列 + 出处 1 条，富余。
pub const MAX_BANDS: usize = 16;

#[derive(Clone, Copy)]
pub struct Band {
    pub x0: u16,
    pub x1: u16,
    /// 格言通道系数（0..=255）
    pub qk: u8,
    /// 日期通道系数（0..=255）
    pub dk: u8,
}

#[derive(Clone, Copy)]
pub struct BandTable {
    bands: [Band; MAX_BANDS],
    n: usize,
}

impl BandTable {
    /// 空表：任何 x 都查不到带 ⇒ `(255, 255)` 都不缩放。
    /// 逐列带场景从它开始 `push` —— **不能先建通带再 push**：通带
    /// `(0, u16::MAX)` 匹配一切，后面的列带永远排不上号。
    pub fn empty() -> Self {
        Self {
            bands: [Band {
                x0: 0,
                x1: 0,
                qk: 255,
                dk: 255,
            }; MAX_BANDS],
            n: 0,
        }
    }

    /// 通带：整个宽度上格言 × `qk`、日期 × `dk`。
    pub fn uniform(qk: u8, dk: u8) -> Self {
        let mut t = Self::empty();
        t.push(0, i32::MAX, qk, dk);
        t
    }

    /// 加一条带。区间为空或超出容量时返回 false（调用方保证列数 ≤ 容量：
    /// 竖版窗口宽度装不下 16 列主文；真超了就是排版层的 bug，由 `--selftest`
    /// 的列顺序断言间接守着）。
    pub fn push(&mut self, x0: i32, x1: i32, qk: u8, dk: u8) -> bool {
        if self.n >= MAX_BANDS || x1 <= x0 {
            return false;
        }
        self.bands[self.n] = Band {
            x0: (x0.max(0) as u32).min(u16::MAX as u32) as u16,
            x1: (x1.max(0) as u32).min(u16::MAX as u32) as u16,
            qk,
            dk,
        };
        self.n += 1;
        true
    }

    /// 查 `x` 落在哪条带：`(格言系数, 日期系数)`；无带 = 都不缩放。
    /// 通带（n==1）是绝大多数帧的形态，一次比较就返回。
    #[inline]
    pub fn get(&self, x: u16) -> (u32, u32) {
        if self.n == 1 {
            let b = &self.bands[0];
            return (b.qk as u32, b.dk as u32);
        }
        for b in &self.bands[..self.n] {
            if x >= b.x0 && x < b.x1 {
                return (b.qk as u32, b.dk as u32);
            }
        }
        (255, 255)
    }
}

impl Default for BandTable {
    fn default() -> Self {
        Self::uniform(255, 255)
    }
}

// ---------------------------------------------------------------- 文本绘制

/// `s` 是 NUL 结尾的宽串
pub fn draw_text(hdc: HDC, s: &[u16], x: i32, y: i32, color: u32) {
    if s.len() <= 1 {
        return;
    }
    unsafe {
        SetTextColor(hdc, color);
        SetBkMode(hdc, TRANSPARENT);
        TextOutW(hdc, x, y, s.as_ptr(), (s.len() - 1) as i32);
    }
}

pub fn measure_text(hdc: HDC, s: &[u16]) -> (i32, i32) {
    if s.len() <= 1 {
        return (0, 0);
    }
    let mut sz: SIZE = unsafe { zeroed() };
    unsafe {
        GetTextExtentPoint32W(hdc, s.as_ptr(), (s.len() - 1) as i32, &mut sz);
    }
    (sz.cx, sz.cy)
}

/// 每字步进（**不含字距**）。一次 GDI 调用拿全串 ——
/// `GetTextExtentExPointW` 填的是**累计宽度**，差分一次就是每字步进，
/// 比逐字量省掉 N-1 次调用。
///
/// 调用方必须保证当前字距为 0：字距由排版层自己加进定位表。
/// 混进度量会得到「末字多算一份」的经典错误 —— 居中时整行右偏半个字距。
pub fn char_advances(hdc: HDC, s: &[u16]) -> Option<Vec<i32>> {
    if s.len() <= 1 {
        return Some(Vec::new());
    }
    let n = (s.len() - 1) as i32;
    let mut dx = vec![0i32; n as usize];
    let mut sz: SIZE = unsafe { zeroed() };
    let ok = unsafe {
        GetTextExtentExPointW(
            hdc,
            s.as_ptr(),
            n,
            i32::MAX,
            null_mut(),
            dx.as_mut_ptr(),
            &mut sz,
        )
    };
    if ok == 0 {
        return None;
    }
    let mut out = Vec::with_capacity(n as usize);
    let mut prev = 0;
    for &c in &dx {
        out.push(c - prev);
        prev = c;
    }
    Some(out)
}

/// 照定位表绘制，**一次 `PolyTextOutW` 提交所有合批段**。
///
/// # 为什么必须合批
///
/// `--perf` 实测：横版 66 字长句逐字画，每帧要多花约 2000µs
/// （≈37µs/字，主趟 + 阴影趟）—— 每条 `TextOutW` 都要重走一遍 GDI 的文本
/// 流水线，而 66 个字其实只分属 3～4 条基线。合批后这一档整帧
/// 3441µs → 1349µs（再叠上「三通道并进一趟绘制」是 872µs）。
///
/// # 为什么是 `PolyTextOutW` 而不是 `TextOutW`
///
/// `TextOutW` 只能顺着**一条基线**排字，所以它对**竖版一无所用** ——
/// 竖排每个字都在不同的 `y` 上。`POLYTEXTW` 是「多段、每段自带 `(x, y)`」，
/// 一次调用就能把 60 个竖排字全部交出去（自检里「竖版合批 46 段」那条就是这个形态）。
///
/// 代价是 `POLYTEXTW` 里**没有字体字段**（字体来自 DC）—— 所以旋转字
/// （竖版的 `——` / `……`，靠 `lfEscapement` 画成竖的一条）必须**按字体分成
/// 两组、各提交一次**。这是合批唯一的结构性限制。
///
/// # 合批的前提
///
/// 段内其余字的位置**交给 GDI 按步进推**，这就要求同一段里：
///   * 同一 `y`（同一条基线）；
///   * 同一支字体（即**都不需要旋转**）；
///   * `src` 紧挨着 —— 原文里本来就相邻的两个字。
///
/// 最后一条是 `Glyph.src` 存在的唯一理由：折行会把**不相邻**的两个字排到
/// 同一行（`cur.pop()` 那个回溯就是现场），那种情况必须断开。
///
/// # 被否掉的做法（别再走一遍）
///
/// 试过 `ExtTextOutW` + `ETO_PDY`（一串 `(dx,dy)` 逐字定位），理论上能把
/// 折行 / 竖排列 / 标点归位全塞进**一次调用**。实测**不可用**：`(dx,dy)`
/// 无论按「相对串原点的绝对偏移」还是「相对前一字的增量」解释，都复现不了
/// 定位表 —— 自检读回墨量 3122 / 7937，而逐字绘制是 11100（包围盒顶还从
/// y=35 跑到 y=4）。
///
/// 注意**否掉的只是「一段之内按 `(dx,dy)` 推进」这一种语义**，别把它推广成
/// 「竖版没法合批」—— `PolyTextOutW` 是「多段各画各的独立位置」，
/// 语义清楚得多，所以它成立，竖版也因此拿到了合批收益。
///
/// # 不变式
///
/// 两条路径必须产出**逐像素相同**的结果，由自检里**两条**断言守着
/// （横版一条、竖版一条；`NO_BATCH` 是它们的开关）——
/// 竖版那条的用例文本带 `——` / `……`，专门盯上面那个「按字体分组」。
/// 上面那次失败尝试就是被这条不变式拦下来的：没有它，
/// 这种「不报错、画出来全乱」的改动会被当成优化直接通过。
///
/// `(ox, oy)` 是整趟的偏移（阴影趟用），字形自己的坐标已经在表里了
/// （含居中 / 右对齐 / 标点归位）。颜色与背景模式在循环外只设一次。
///
/// `no_batch` 显式覆盖 `DevFlags::no_batch`（`Some(true)` 强制逐字）——
/// 自检的合批 A/B 断言要在同一进程里先出合批帧、再出逐字帧，而 `DevFlags`
/// 是 `OnceLock`（一次性）。`None` = 听 `DevFlags` 的。
#[allow(clippy::too_many_arguments)]
pub fn draw_glyphs(
    hdc: HDC,
    font: HFONT,
    rot_font: HFONT,
    ql: &crate::layout::QuoteLayout,
    ox: i32,
    oy: i32,
    color: u32,
    spacing: i32,
    // `col_groups`：**逐列灰度组**（仅阴影趟的竖版入场错峰用）：
    // `(首字形下标, 字数, 颜色)`，按列序给出该列字形用的颜色。
    // `None` = 整块一个 `color`（稳态 / 横版 / 非阴影趟）。
    // 组合起来必须覆盖全部字形 —— 竖版排版按列切分，天然满足。
    col_groups: Option<&[(usize, usize, u32)]>,
    no_batch: Option<bool>,
) {
    let glyphs = &ql.glyphs;
    if glyphs.is_empty() {
        return;
    }
    let no_batch = no_batch.unwrap_or_else(|| dev_flags().no_batch);
    unsafe {
        SetBkMode(hdc, TRANSPARENT);
        // 字距交给 GDI：`SetTextCharacterExtra` 与步进严格线性（见 README 度量约定），
        // 与排版层「每个字自己加一份」的结果相同。
        SetTextCharacterExtra(hdc, spacing);

        if no_batch {
            // ---- 参考路径：逐字一次 `TextOutW` ----
            // `--selftest` 拿它跟下面那条比对**逐像素**是否相同。
            // 注意不能图省事把一整段仍当成一次调用：那会让 66 个字顺着
            // 一条基线排下去、冲出窗口，参考值自己就是错的。
            let mut buf: Vec<u16> = Vec::new();
            for (gi, g) in glyphs.iter().enumerate() {
                let want = if g.rot && !rot_font.is_null() {
                    rot_font
                } else {
                    font
                };
                SelectObject(hdc, want as HGDIOBJ);
                SetTextColor(hdc, group_color(col_groups, gi, color));
                buf.clear();
                buf.push(g.ch);
                TextOutW(hdc, ox + g.x, oy + g.y, buf.as_ptr(), 1);
            }
        } else {
            // ---- 合批路径：**一次 `PolyTextOutW`** 提交所有段 ----
            //
            // 段内字符由 GDI 按步进推，段间位置由每段自己的 `(x, y)` 指定。
            // 这一点让**竖版**也吃到了合批：它的每个字都在不同的 y 上，
            // `TextOutW` 只能顺着一条基线排、一个都用不上，而
            // `PolyTextOutW` 每段自带坐标，六十多个竖排字一次提交完。
            //
            // 旋转字要换字体对象，而 `POLYTEXTW` 里**没有字体字段**
            // （字体来自 DC），所以只能按字体分两组、各提交一次。
            // 逐列灰度（col_groups）是第三层切分：只在阴影趟的竖版入场
            // 错峰时出现，且竖版每字本就各自成段，多一层过滤零额外提交。
            let runs = ql.runs();
            let mut store: Vec<u16> = Vec::with_capacity(glyphs.len());
            let mut offs: Vec<(usize, usize)> = Vec::with_capacity(runs.len());
            let mut pt: Vec<POLYTEXTW> = Vec::with_capacity(runs.len());
            let default = [(0usize, usize::MAX, color)];
            let groups: &[(usize, usize, u32)] = col_groups.unwrap_or(&default);
            for use_rot in [false, true] {
                for &(g0, g1, gc) in groups {
                    let group: Vec<(usize, usize)> = runs
                        .iter()
                        .copied()
                        .filter(|&(s, _)| glyphs[s].rot == use_rot && s >= g0 && s < g1)
                        .collect();
                    if group.is_empty() {
                        continue;
                    }
                    let want = if use_rot && !rot_font.is_null() {
                        rot_font
                    } else {
                        font
                    };
                    SelectObject(hdc, want as HGDIOBJ);
                    SetTextColor(hdc, gc);
                    // **先把字符全铺进 `store` 再取指针**：边铺边记，中途一次
                    // 扩容就会让先前记下的 `lpstr` 全部悬空 —— 那是个会静默画错的坑。
                    store.clear();
                    offs.clear();
                    for &(s, n) in &group {
                        offs.push((store.len(), n));
                        store.extend(glyphs[s..s + n].iter().map(|g| g.ch));
                    }
                    let base = store.as_ptr();
                    pt.clear();
                    pt.extend(group.iter().zip(offs.iter()).map(|(&(s, _), &(off, n))| {
                        POLYTEXTW {
                            x: ox + glyphs[s].x,
                            y: oy + glyphs[s].y,
                            n: n as u32,
                            lpstr: base.add(off),
                            uiFlags: 0,
                            rcl: zeroed(),
                            pdx: null_mut(),
                        }
                    }));
                    PolyTextOutW(hdc, pt.as_ptr(), pt.len() as i32);
                }
            }
        }
        // 还原：后面还有 `measure_text` / 时钟排版要量文字，字距必须是 0
        SetTextCharacterExtra(hdc, 0);
    }
}

/// 第 `gi` 个字形属于哪个灰度组（`col_groups` 按下标范围切分）。
/// 无组 / 不属任何组 = 整块默认色。
#[inline]
fn group_color(col_groups: Option<&[(usize, usize, u32)]>, gi: usize, default: u32) -> u32 {
    match col_groups {
        Some(gs) => {
            for &(g0, g1, c) in gs {
                if gi >= g0 && gi < g1 {
                    return c;
                }
            }
            default
        }
        None => default,
    }
}

/// 竖排逐字下落的一串小字（竖版落款用）。
///
/// 整列**一次 `PolyTextOutW`** 提交：每个字一条 `POLYTEXTW`（各自带
/// `(x, y)`），一次调用画完。逐字 `TextOutW` 在这里毫无办法 —— 它们
/// 全在不同的 `y` 上，`TextOutW` 只能顺着一条基线排。
pub fn draw_vtext(hdc: HDC, s: &[u16], x: i32, y: i32, cell: i32, color: u32) {
    if s.len() <= 1 {
        return;
    }
    let chars = &s[..s.len() - 1];
    unsafe {
        SetTextColor(hdc, color);
        SetBkMode(hdc, TRANSPARENT);
        let base = chars.as_ptr();
        let pt: Vec<POLYTEXTW> = chars
            .iter()
            .enumerate()
            .map(|(j, _)| POLYTEXTW {
                x,
                y: y + j as i32 * cell,
                n: 1,
                lpstr: base.add(j),
                uiFlags: 0,
                rcl: zeroed(),
                pdx: null_mut(),
            })
            .collect();
        PolyTextOutW(hdc, pt.as_ptr(), pt.len() as i32);
    }
}

/// 抓一个窗口的**合成后内容**（`PrintWindow` + `PW_RENDERFULLCONTENT`）。
///
/// 分层窗口的像素归 DWM 持有：对它 `GetDC` + `BitBlt` 抓到的是一片空。
/// 所以要验证「窗口被移动之后图层内容还在」，只能请系统把窗口按合成结果重画一遍。
/// 纯粹给测试用的读回通道，正常运行一帧都不走这里。
pub fn capture_window(hwnd: HWND, w: i32, h: i32) -> Option<Vec<u32>> {
    let mut d = Dib::new(w, h)?;
    if !crate::ffi::print_window(hwnd, d.dc, PW_RENDERFULLCONTENT) {
        return None;
    }
    Some(d.pixels().to_vec())
}

/// 在裁剪区内绘制（跑马灯必须用，否则字形会盖到时钟上）
pub fn with_clip<F: FnOnce(HDC)>(hdc: HDC, l: i32, t: i32, r: i32, b: i32, f: F) {
    unsafe {
        let saved = SaveDC(hdc);
        IntersectClipRect(hdc, l, t, r, b);
        f(hdc);
        RestoreDC(hdc, saved);
    }
}
