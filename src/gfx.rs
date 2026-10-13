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
//!   ① 离屏 32bpp DIB 涂黑，GDI 用灰度抗锯齿在白字上绘制
//!        → 每个像素 RGB 都等于字形覆盖率 c（0..255）
//!   ② 掩码：alpha = R（白字黑底，R 就是覆盖率）
//!        → 得到纯 alpha 掩码，跟颜色无关
//!   ③ 预乘：白色预乘后 R=G=B = 255*alpha/255 = alpha
//!        → 一个乘加就能同时得到颜色和 alpha，无需浮点
//!   ④ UpdateLayeredWindow(AC_SRC_ALPHA) 上屏，DWM 按像素 alpha 合成
//! ```
//!
//! 阴影走同一条管线的第二次绘制：白底黑字 → `alpha = 255 - R`，
//! 它的预乘颜色恒为 (0,0,0)，所以合成时只贡献 alpha、不污染颜色 ——
//! 这就是浅色壁纸下白字仍然能立起来的原因。
//!
//! # 颜色为什么在最后一刻才乘
//!
//! 掩码层全程只用 **白字黑底**，所以掩码永远是纯粹的「字形覆盖率」，
//! 跟颜色无关。颜色在 `compose` 里以 `c = color * alpha / 255` 参与预乘。
//! 这样一来：
//!   * 换颜色不会影响抗锯齿质量，也不会引入任何色彩误差；
//!   * 预乘不变式 `c ≤ a` 自动成立（因为 `color ≤ 255`）；
//!   * 时钟和日期可以用**两个独立颜色**（两条掩码通道），
//!     而不是把颜色画进位图里再想办法抠回来。
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
    GetTextExtentPoint32W, IntersectClipRect, ReleaseDC, RestoreDC, SaveDC, SelectObject,
    SetBkMode, SetTextColor, TextOutW, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, HDC, HFONT,
    HGDIOBJ, HBITMAP,
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
}

impl Fonts {
    pub fn build(face: &str, scale: i32, clock_ratio: i32, quote_ratio: i32) -> Fonts {
        let f = crate::util::wide(face);
        let s = |px: i32, ratio: i32| ((px * scale / 100) * ratio / 100).max(6);
        Fonts {
            clock: make_font(s(46, clock_ratio), FW_SEMIBOLD, &f),
            clock_small: make_font(s(34, clock_ratio), FW_SEMIBOLD, &f),
            date: make_font(s(16, clock_ratio), FW_NORMAL, &f),
            quote: make_font(s(20, quote_ratio), FW_NORMAL, &f),
            hint: make_font(s(12, 100), FW_NORMAL, &f),
            face: f,
        }
    }

    pub fn destroy(&self) {
        unsafe {
            for h in [self.clock, self.clock_small, self.date, self.quote, self.hint] {
                if !h.is_null() {
                    DeleteObject(h as HGDIOBJ);
                }
            }
        }
    }
}

pub fn make_font(px: i32, weight: i32, face: &[u16]) -> HFONT {
    unsafe {
        CreateFontW(
            -px, // 负值 = 字符高度（不含内部行距），排版更可控
            0,
            0,
            0,
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

// ---------------------------------------------------------------- DIB

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

/// 一帧的渲染目标。
///
/// `work` 是给 GDI 画字的草稿位图（黑底白字 / 白底黑字），
/// `out` 是最终喂给 `UpdateLayeredWindow` 的预乘 ARGB 位图。
pub struct Frame {
    pub w: i32,
    pub h: i32,
    work: Option<Dib>,
    out: Option<Dib>,
    /// 字形覆盖率掩码（时钟 + 分隔线 + 格言，用主色）
    mask: Vec<u8>,
    /// 日期覆盖率掩码（单独一条通道，才能用独立的日期颜色）
    dmask: Vec<u8>,
    /// 阴影覆盖率掩码
    shad: Vec<u8>,
    shadow_enabled: bool,
}

impl Frame {
    pub fn new(w: i32, h: i32) -> Frame {
        let mut f = Frame {
            w,
            h,
            work: None,
            out: None,
            mask: Vec::new(),
            dmask: Vec::new(),
            shad: Vec::new(),
            shadow_enabled: false,
        };
        f.ensure(w, h);
        f
    }

    pub fn ensure(&mut self, w: i32, h: i32) -> bool {
        if w <= 0 || h <= 0 {
            return false;
        }
        if self.w == w && self.h == h && self.work.is_some() && self.out.is_some() {
            return true;
        }
        let n = (w * h) as usize;
        self.work = Dib::new(w, h);
        self.out = Dib::new(w, h);
        self.mask = vec![0u8; n];
        self.dmask = vec![0u8; n];
        self.shad = vec![0u8; n];
        self.w = w;
        self.h = h;
        self.work.is_some() && self.out.is_some()
    }

    pub fn set_shadow(&mut self, on: bool) {
        self.shadow_enabled = on;
    }

    /// 草稿位图的 DC（用于测量文字；调用前记得 `SelectObject` 换字体）
    pub fn dc(&self) -> HDC {
        self.work.as_ref().map(|d| d.dc).unwrap_or(null_mut())
    }

    /// 开始「正文」绘制：黑底，后面用白字画
    pub fn begin_text(&mut self) -> HDC {
        let d = self.work.as_mut().expect("frame not ready");
        d.clear(BLACK);
        d.dc
    }

    /// 开始「阴影」绘制：白底，后面用黑字画
    pub fn begin_shadow(&mut self) -> HDC {
        let d = self.work.as_mut().expect("frame not ready");
        d.clear(WHITE);
        d.dc
    }

    /// 开始「日期」绘制：同样是黑底白字，只是单独采到 `dmask` 通道
    pub fn begin_date(&mut self) -> HDC {
        let d = self.work.as_mut().expect("frame not ready");
        d.clear(BLACK);
        d.dc
    }

    /// 采集正文掩码：白字黑底 → R 通道即覆盖率
    pub fn extract_text(&mut self) {
        let Some(d) = self.work.as_mut() else { return };
        let px = d.pixels();
        for (m, p) in self.mask.iter_mut().zip(px.iter()) {
            *m = (*p & 0xFF) as u8;
        }
    }

    /// 采集日期掩码
    pub fn extract_date(&mut self) {
        let Some(d) = self.work.as_mut() else { return };
        let px = d.pixels();
        for (m, p) in self.dmask.iter_mut().zip(px.iter()) {
            *m = (*p & 0xFF) as u8;
        }
    }

    /// 日期被关掉时清空这条通道。
    /// 缓冲区是复用的，不清就会把上一帧的日期「留在」这一帧上。
    pub fn clear_date_mask(&mut self) {
        self.dmask.fill(0);
    }

    /// 阴影关掉时同理清空
    pub fn clear_shadow_mask(&mut self) {
        self.shad.fill(0);
    }

    /// 三条掩码通道的只读视图 `(正文, 日期, 阴影)`，只给自检用 ——
    /// 「阴影有没有跑到没有字形的地方」这类断言必须直接看掩码，
    /// 合成完的像素上分不出来。
    pub fn masks(&self) -> (&[u8], &[u8], &[u8]) {
        (&self.mask, &self.dmask, &self.shad)
    }

    /// 采集阴影掩码：黑字白底 → 255 - R 即覆盖率
    pub fn extract_shadow(&mut self) {
        let Some(d) = self.work.as_mut() else { return };
        let px = d.pixels();
        for (m, p) in self.shad.iter_mut().zip(px.iter()) {
            *m = 255 - (*p & 0xFF) as u8;
        }
    }

    /// 合成预乘 ARGB。
    ///
    /// `opacity` 是整体不透明度，`shadow_k` 是阴影强度（0 表示本帧没有阴影层），
    /// `color` / `date_color` 是 `0x00RRGGBB` 逻辑顺序的主色与日期色。
    ///
    /// 颜色在这里才乘进去：`c = color * alpha / 255`。因为 `color ≤ 255`，
    /// 预乘不变式 `c ≤ a` 恒成立 —— 不需要任何夹紧也不会违反 DWM 的约定。
    /// 阴影是纯黑，只把 alpha 抬上去、不动颜色分量，于是合成回来就是「暗边」。
    pub fn compose(&mut self, opacity: u8, shadow_k: u8, color: u32, date_color: u32) {
        let sk = if self.shadow_enabled { shadow_k as u32 } else { 0 };
        let op = opacity as u32;
        let Some(d) = self.out.as_mut() else { return };
        let out = d.pixels();
        let (cr, cg, cb) = (color_r(color), color_g(color), color_b(color));
        let (dr, dg, db) = (color_r(date_color), color_g(date_color), color_b(date_color));

        for (i, o) in out.iter_mut().enumerate() {
            let ma = (self.mask[i] as u32 * op) / 255;
            let da = (self.dmask[i] as u32 * op) / 255;

            // 一个像素只会属于「时钟+格言」或「日期」其中之一（两块区域不重叠）。
            // 万一用户把它们拖叠在一起，取更实的一路，不会出现颜色混叠。
            let (ta, r, g, b) = if da > ma {
                (da, dr * da / 255, dg * da / 255, db * da / 255)
            } else {
                (ma, cr * ma / 255, cg * ma / 255, cb * ma / 255)
            };

            let a = if sk == 0 {
                ta
            } else {
                let sa = (self.shad[i] as u32 * sk * op) / (255 * 255);
                ta + (sa * (255 - ta)) / 255
            };
            *o = (a << 24) | (r << 16) | (g << 8) | b;
        }
    }

    /// 合成后的预乘 ARGB，供自检与调试读取
    pub fn out_pixels(&self) -> &[u32] {
        match self.out.as_ref() {
            Some(d) => unsafe { std::slice::from_raw_parts(d.bits, (d.w * d.h) as usize) },
            None => &[],
        }
    }

    /// 上屏：`UpdateLayeredWindow` + `AC_SRC_ALPHA`，DWM 逐像素合成。
    pub fn present(&self, hwnd: HWND, x: i32, y: i32) -> bool {
        let Some(d) = self.out.as_ref() else {
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
