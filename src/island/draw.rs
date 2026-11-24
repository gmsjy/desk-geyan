//! 灵动岛的绘制：掩码趟 → 阴影趟 → 形状趟 → 合成 → 上屏。
//!
//! 和格言**共用** `gfx::Frame`（两块 DIB 的像素引擎），**不共用**排版、
//! 合批、`BandTable` 的分带逻辑。岛的内容是 3~6 个短串，`ExtTextOutW`
//! 逐条画就够了，`PolyTextOutW` 的合批准备成本比收益大。
//!
//! 坐标约定：`layout` 输出的是**端点坐标系**（收起 = 34px 高那条带，
//! 展开 = 目标宽高）。窗口在动画中间态更小，超出的部分由 GDI 的位图裁剪
//! 自然截掉 —— 于是文字基线与位置全程不动，不会随尺寸变化重排抖动。

use std::mem::zeroed;

use windows_sys::Win32::Foundation::{RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    DeleteObject, Ellipse, FillRect, GetStockObject, GetTextExtentPoint32W, SelectObject,
    SetBkMode, SetTextColor, TextOutW, HDC, HFONT, HGDIOBJ, NULL_PEN,
};

use super::geom::COMPACT_MAX_W;
use super::layout::{self, Chan, Layout};
use crate::app::App;
use crate::consts::*;
use crate::gfx::{self, BandTable};
use crate::util::wide;

/// 岛自己的字体集合。**不跟格言的 `scale` 走** —— 两个功能独立，
/// 用户把格言调到 300% 不该让胶囊里的时间变成巨字。
pub struct IslandFonts {
    face: Vec<u16>,
    pub clock: HFONT,
    pub text: HFONT,
    /// 建这套字体时的 DPI 百分比（变了就重建）
    pub k: i32,
}

impl IslandFonts {
    pub fn build(face: &str, k: i32) -> IslandFonts {
        let f = wide(face);
        let sc = |px: f32| ((px * k as f32 / 100.0).round() as i32).max(6);
        IslandFonts {
            clock: gfx::make_font(sc(32.0), FW_SEMIBOLD, &f),
            text: gfx::make_font(sc(13.0), FW_NORMAL, &f),
            face: f,
            k,
        }
    }

    pub fn face_name(&self) -> String {
        String::from_utf16_lossy(&self.face[..self.face.len().saturating_sub(1)])
    }
}

impl Drop for IslandFonts {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.clock as HGDIOBJ);
            DeleteObject(self.text as HGDIOBJ);
        }
    }
}

/// 掩码趟该写什么颜色：**纯单字节**的通道标记，不是显示色。
///
/// # 这里曾经写的是显示色，而且看起来"能用"
///
/// `compose` 是「哪条通道的字节最大就取哪条」，所以把显示色（`#F5D98B`）
/// 当通道值写进去时，它点亮的其实是三条字节 `{8B, D9, F5}` —— 恰好 R 最大，
/// 于是落到主色通道，画面看起来完全正确。
///
/// 但只要显示色是**中性灰**（`#B8B8B8`，三字节相等），三条通道就并列最大，
/// 而比较顺序是 `qa >= da && qa >= ma` ⇒ 平局判给**强调通道**。症状是
/// 「日期与指标本该是灰色，却画成了强调蓝」，而默认配置的 `date_color`
/// 正是 `#B8B8B8` —— 也就是说这个 bug 在默认外观下 100% 复现。
///
/// 与格言同一套口径：掩码只存覆盖率、颜色只在 `compose` 里乘。于是
/// 「三通道相等」这种退化情形根本不存在。
fn chan_color(c: Chan) -> u32 {
    match c {
        Chan::Main => gfx::CH_CLOCK,
        Chan::Dim => gfx::CH_DATE,
        Chan::Accent => gfx::CH_QUOTE,
    }
}

/// 画一趟（掩码趟或阴影趟）。`color` 非 0 时按各 run 自己的通道色，
/// 全 0 时一律黑（阴影位图是白底，覆盖率 = 255 - 写入值）。
fn paint_pass(dc: HDC, lay: &Layout, fonts: &IslandFonts, clock_px: i32, shadow: bool, off: i32) {
    unsafe {
        SetBkMode(dc, TRANSPARENT);
        for r in &lay.runs {
            let f = if r.px >= clock_px { fonts.clock } else { fonts.text };
            SelectObject(dc, f as HGDIOBJ);
            let col = if shadow { 0 } else { chan_color(r.ch) };
            SetTextColor(dc, col);
            let w = wide(&r.text);
            if w.len() > 1 {
                TextOutW(dc, r.x + off, r.y + off, w.as_ptr(), w.len() as i32 - 1);
            }
        }
        // 圆点用 `Ellipse` 而不是 `FillRect`：6px 的方块在一条胶囊里读起来
        // 是「一个色块」，不像未读标记。画笔要换成 `NULL_PEN`，否则默认的
        // 1px 黑边会把 6px 的圆吃掉一圈，看起来是个更小的黑点。
        for d in &lay.dots {
            let col = if shadow { 0 } else { chan_color(Chan::Main) };
            let brush = windows_sys::Win32::Graphics::Gdi::CreateSolidBrush(col);
            let old_b = SelectObject(dc, brush as HGDIOBJ);
            let old_p = SelectObject(dc, GetStockObject(NULL_PEN));
            Ellipse(
                dc,
                d.x + off,
                d.y + off,
                d.x + d.d + off,
                d.y + d.d + off,
            );
            SelectObject(dc, old_p);
            SelectObject(dc, old_b);
            DeleteObject(brush as HGDIOBJ);
        }
        // 分隔线仍是 1px 矩形：`FillRect` 一次写完，没有比它更便宜的路
        for l in &lay.lines {
            let col = if shadow { 0 } else { chan_color(Chan::Main) };
            let brush = windows_sys::Win32::Graphics::Gdi::CreateSolidBrush(col);
            let th = if l.x1 > l.x0 { 1 } else { 0 };
            let rc = RECT {
                left: l.x0,
                top: l.y + off,
                right: l.x1,
                bottom: l.y + off + th,
            };
            FillRect(dc, &rc, brush);
            DeleteObject(brush as HGDIOBJ);
        }
    }
}

/// 出一帧并上屏。返回是否真的画了。
///
/// 调用方负责「该不该出帧」的判定（`needs_frame`）—— 这里只管画。
pub(crate) fn render(a: &mut App) -> bool {
    // 先把 cfg 里要用的值抄成局部量：`&mut a.island` 与 `&a.cfg` 不能同时借。
    let (op, main_c, dim_c, accent_c, shadow, body_c, body_a) = {
        let c = a.cfg();
        (
            c.opacity,
            c.color,
            c.date_color,
            c.island_accent_color,
            c.shadow,
            c.island_body_color,
            c.island_body_alpha,
        )
    };
    let face = {
        let c = a.cfg();
        c.font.clone()
    };

    let rect = a.island.rect;
    if rect.w <= 0 || rect.h <= 0 {
        return false;
    }
    let hwnd = a.island.hwnd;
    if hwnd.is_null() {
        return false;
    }

    // 字体按当前 DPI 建/复用
    let dpi = a.island.dpi;
    let need_fonts = match a.island.fonts.as_ref() {
        Some(f) => f.k != dpi || f.face_name() != face,
        None => true,
    };
    if need_fonts {
        a.island.fonts = Some(IslandFonts::build(&face, dpi));
    }
    let fonts = a.island.fonts.as_ref().expect("刚建过");

    // ---- 端点尺寸（布局坐标系的基准，**不随动画变**）
    let k = dpi as f32 / 100.0;
    let target_w = ((a.cfg().island_w as f32) * k).round().max(80.0) as i32;
    let target_h = ((a.cfg().island_h as f32) * k).round().max(40.0) as i32;

    // ---- 内容相位：哪一套内容可见、系数多少
    let (ck, ek) = super::geom::phase_k(a.island.content_t());
    let tk = if ck > 0.0 { ck } else { ek };
    let is_compact = ck > 0.0;
    let items = a.island.items.clone();
    let samples = a.island.samples.clone();

    // ---- 排版（每帧一次，两趟共用）。度量用 `ink` 的 DC：只读尺寸、不写像素，
    // 所以复用绘制那块位图没有副作用（与 `Frame::dc` 的注释同一理由）。
    let dc_measure = a.island.frame.dc();
    if dc_measure.is_null() {
        return false;
    }
    let clock_px = ((32.0 * k).round() as i32).max(8);
    let measure = |s: &str, px: i32| -> (i32, i32) {
        unsafe {
            let f = if px >= clock_px { fonts.clock } else { fonts.text };
            SelectObject(dc_measure, f as HGDIOBJ);
            let w = wide(s);
            let mut sz: SIZE = zeroed();
            if GetTextExtentPoint32W(dc_measure, w.as_ptr(), w.len() as i32 - 1, &mut sz) == 0 {
                return (0, px);
            }
            (sz.cx, sz.cy)
        }
    };

    let (lay, pages) = if is_compact {
        // 收起内容永远按**收起端点**排（34px 高、宽度上限 260），水平居中于
        // 当前窗口 —— 动画中窗口在变宽，内容不能跟着重排，否则会看到文字横向抖一下。
        let segs = super::content::segments(&samples, &items, true);
        layout::compact_page(
            &segs,
            &measure,
            k,
            layout::sp(COMPACT_MAX_W as f32, k),
            a.island.rot.page,
        )
    } else if ek > 0.0 {
        (layout::expanded(&samples, &items, &measure, k, target_w, target_h), 1)
    } else {
        (Layout::default(), 1)
    };

    // 把真实度量宽度回写给 `mod.rs`：下一拍算几何时用它，胶囊就正好裹住内容。
    // （`estimate_compact_w` 只是首帧之前的兜底，中英混排下会差十几像素。）
    if is_compact && lay.width > 0 {
        a.island.compact_w = lay.width;
    }
    // 页数也要回写：`tick` 靠它判断「要不要轮播」，只有一页时永远不轮播 ——
    // 于是「不轮播」与「内容只有一页」都退化成零出帧，不需要额外的开关。
    if is_compact {
        a.island.pages = pages;
    }

    // 收起内容在当前窗口里水平居中（窗口可能比它宽）
    let dx = if is_compact {
        ((rect.w - lay.width) / 2).max(0)
    } else {
        0
    };

    // 收起态整体水平居中：把 x 平移一次，两趟共用（之前两趟各克隆一遍，
    // 改一处忘另一处就会变成「阴影和文字错位」）。
    let shifted;
    let lay_draw: &Layout = if dx != 0 {
        let mut s = lay.clone();
        for r in &mut s.runs {
            r.x += dx;
        }
        for d in &mut s.dots {
            d.x += dx;
        }
        for l in &mut s.lines {
            l.x0 += dx;
            l.x1 += dx;
        }
        shifted = s;
        &shifted
    } else {
        &lay
    };

    // ---- ① 掩码趟
    let off = if shadow { ((2.0 * k).round() as i32).max(1) } else { 0 };
    let frame = &mut a.island.frame;
    if !frame.ensure(rect.w, rect.h) {
        return false;
    }
    {
        let dc = frame.begin_ink();
        paint_pass(dc, lay_draw, fonts, clock_px, false, 0);
    }

    // ---- ② 阴影趟（白底黑字，覆盖率 = 255 - 读到的值）
    if shadow {
        let dc = frame.begin_shadow();
        paint_pass(dc, lay_draw, fonts, clock_px, true, off);
    }

    // ---- ③ 形状趟：只写 byte3，**必须在所有 GDI 绘制之后**
    gfx::body_coverage_into(frame.ink_pixels_mut(), rect.w, rect.h, rect.r);

    // ---- ④ 合成。`tk` 是内容相位，`rot_k` 是切页的淡出淡入；两者只在
    // 收起态相乘（展开态不轮播，`rot_k` 恒 1，乘了也无害 —— 但显式写出来，
    // 将来给展开态加别的动画时不会误伤）。
    let fade = if is_compact { a.island.rot_k } else { 1.0 };
    let tk8 = (tk * fade * 255.0).round().clamp(0.0, 255.0) as u8;
    let body_a8 = body_a.clamp(0, 255) as u8;
    let bands = BandTable::uniform(255, 255);
    frame.compose_ex(
        op.clamp(0, 255) as u8,
        if shadow { 255 } else { 0 },
        &bands,
        main_c,
        dim_c,
        accent_c,
        tk8,
        Some((body_c, body_a8)),
    );

    // ---- ⑤ 上屏：位置 + 尺寸 + 内容一次提交
    let ok = frame.present(hwnd, rect.x, rect.y);
    a.island.renders = a.island.renders.wrapping_add(1);
    ok
}
