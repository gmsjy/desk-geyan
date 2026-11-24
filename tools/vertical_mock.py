"""竖排界面设计草案的排版样机（1:1 像素，非 CSS 近似）。

为什么不用 HTML 摆一个「差不多」的预览：竖排的观感全押在「字格」那套度量上 ——
列距 1.15 字宽还是 1.70 字宽，屏幕上就是「糊成一片」和「一列一列读得下去」的
差别。CSS 的 line-height / writing-mode 与 GDI 的字符步进没有对应关系，量出来
的数字对不上实现，草案就没有参考价值。

所以这里用与实现同一套思路重画：以 `em = 正文字号` 为单位给出全部度量，逐字
摆到字格中心，句读点号按**墨迹外框**归位到右上角，落款按块堆叠 —— 每一个数字
都能直接搬进 `src/layout.rs` 与 `src/app/render.rs`。

三个形态：
    A 条幅   落款竖列 + 界栏 + 正文，整块居中（推荐，默认）
    B 接排   无界栏，落款接在最左一列正文底下（题跋式，短句最好看）
    C 栏头   时间横排压在顶上，一条细线分界，正文竖排（宽窗，时间最好读）

用法：
    python tools/vertical_mock.py out/mock
"""

import os
import sys

from PIL import Image, ImageDraw, ImageFilter, ImageFont

FONT_DIR = os.path.expanduser(r"~\AppData\Local\Microsoft\Windows\Fonts")
SERIF = os.path.join(FONT_DIR, "屏显臻宋.ttf")
FALLBACK = r"C:\Windows\Fonts\simkai.ttf"

# 主色不是纯白：纯白贴在任何壁纸上都发飘。暖白更接近宣纸，也压得住彩色壁纸
FG = (243, 234, 218)
AUTHOR = (208, 198, 178)
DATE = (172, 163, 147)
RULE = (243, 234, 218)

DOT = set("，。、；：！？")
ROT = set("—…")


def font(px):
    path = SERIF if os.path.exists(SERIF) else FALLBACK
    return ImageFont.truetype(path, max(6, int(round(px))))


# ---------------------------------------------------------------- 背景

def wallpaper(w, h):
    img = Image.new("RGB", (w, h), (26, 28, 46))
    d = ImageDraw.Draw(img)
    for y in range(h):
        k = y / max(1, h - 1)
        d.line([(0, y), (w, y)], fill=(int(24 + 22 * k), int(26 + 16 * k), int(46 + 30 * k)))
    for cx, cy, r, col in [
        (0.18, 0.22, 0.42, (86, 52, 120)),
        (0.82, 0.14, 0.34, (30, 70, 110)),
        (0.62, 0.78, 0.46, (96, 40, 60)),
        (0.12, 0.88, 0.30, (26, 60, 74)),
    ]:
        layer = Image.new("RGB", (w, h), (0, 0, 0))
        dl = ImageDraw.Draw(layer)
        rr = r * min(w, h)
        dl.ellipse([cx * w - rr, cy * h - rr, cx * w + rr, cy * h + rr], fill=col)
        layer = layer.filter(ImageFilter.GaussianBlur(rr / 2.2))
        img = Image.blend(img, Image.blend(img, layer, 0.55), 0.9)
    return img


def panel(w, h, pad=120):
    """把窗口贴到壁纸上：窗下那块先模糊压暗（Acrylic 的观感），再上文字层。"""
    bg = wallpaper(w + 2 * pad, h + 2 * pad)
    mask = Image.new("L", bg.size, 0)
    ImageDraw.Draw(mask).rectangle([pad, pad, pad + w, pad + h], fill=255)
    soft = bg.filter(ImageFilter.GaussianBlur(26))
    plate = Image.blend(soft, Image.new("RGB", bg.size, (14, 14, 20)), 0.42)
    bg = Image.composite(plate, bg, mask.filter(ImageFilter.GaussianBlur(1.2)))
    edge = Image.new("RGBA", bg.size, (0, 0, 0, 0))
    ImageDraw.Draw(edge).rectangle(
        [pad, pad, pad + w - 1, pad + h - 1], outline=(255, 255, 255, 18), width=1
    )
    return Image.alpha_composite(bg.convert("RGBA"), edge), pad


# ---------------------------------------------------------------- 竖排绘制

_INK_CACHE = {}


def ink_offset(ch, f, px):
    """把点号的**墨迹**搬到字格右上角，返回相对字格中心的偏移。

    横排里 `，` 的墨迹待在字格左下，竖排同一个位置看起来像掉到下一个字下面去。
    按实际墨迹外框算，而不是「格心 + 拍脑袋的固定偏移」—— 不同字体、字号的
    点号在字框里落点差得远，估出来的值会把它顶到上一格去。
    """
    key = (ch, px)
    if key not in _INK_CACHE:
        box = int(px * 3)
        im = Image.new("L", (box, box), 0)
        ImageDraw.Draw(im).text((box / 2, box / 2), ch, font=f, fill=255, anchor="mm")
        b = im.getbbox()
        if b is None:
            off = (0.0, 0.0)
        else:
            off = ((b[0] + b[2]) / 2 - box / 2, (b[1] + b[3]) / 2 - box / 2)
        _INK_CACHE[key] = off
    off = _INK_CACHE[key]
    return (px * 0.30 - off[0], -px * 0.32 - off[1])


def vcol(img, x_center, y_top, s, px, color, cell_k=1.10):
    d = ImageDraw.Draw(img)
    f = font(px)
    cell = px * cell_k
    for i, ch in enumerate(s):
        cy = y_top + i * cell + cell / 2
        cx = x_center
        if ch in DOT:
            dx, dy = ink_offset(ch, f, px)
            cx += dx
            cy += dy
        if ch in ROT:
            glyph = Image.new("RGBA", (int(px * 2), int(px * 2)), (0, 0, 0, 0))
            ImageDraw.Draw(glyph).text((px, px), ch, font=f, fill=color + (255,), anchor="mm")
            glyph = glyph.rotate(-90, resample=Image.BICUBIC)
            img.alpha_composite(glyph, (int(cx - glyph.width / 2), int(cy - glyph.height / 2)))
            continue
        d.text((cx, cy), ch, font=f, fill=color + (255,), anchor="mm")
    return len(s) * cell


def htext(img, x, y, s, px, color, anchor="ra", tracking=0.0):
    d = ImageDraw.Draw(img)
    f = font(px)
    if tracking <= 0:
        d.text((x, y), s, font=f, fill=color + (255,), anchor=anchor)
        return d.textlength(s, font=f)
    total = sum(d.textlength(c, font=f) for c in s) + tracking * px * (len(s) - 1)
    cx = x - total if "r" in anchor else x
    for c in s:
        d.text((cx, y), c, font=f, fill=color + (255,), anchor="l" + anchor[1])
        cx += d.textlength(c, font=f) + tracking * px
    return total


def clock_block(img, x_right, y_top, s, px, color, tracking=0.05):
    """时间：横排整块顺时针转 90°。

    竖排里逐字直立 `1 5 : 4 0`，四个数字读成四拍，还得先认列；整块旋转之后
    它仍然是一个「词」，歪一下头就一眼读出来。
    """
    probe = ImageDraw.Draw(Image.new("RGBA", (8, 8)))
    f = font(px)
    w = int(sum(probe.textlength(c, font=f) for c in s) + tracking * px * (len(s) - 1)) + 6
    h = int(px * 1.5)
    strip = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    ds = ImageDraw.Draw(strip)
    cx = 3
    for c in s:
        ds.text((cx, h * 0.76), c, font=f, fill=color + (255,), anchor="ls")
        cx += probe.textlength(c, font=f) + tracking * px
    strip = strip.rotate(-90, resample=Image.BICUBIC, expand=True)
    img.alpha_composite(strip, (int(x_right - strip.width), int(y_top)))
    return strip.width, strip.height


def hairline_v(img, x, y0, y1, alpha=0.16):
    ImageDraw.Draw(img).line([(x, y0), (x, y1)], fill=RULE + (int(255 * alpha),), width=1)


def shadow_pass(layer, blur=2.2, off=(1, 2), alpha=120):
    a = layer.split()[3].point(lambda v: int(v * alpha / 255))
    sh = Image.new("RGBA", layer.size, (8, 8, 12, 255))
    sh.putalpha(a)
    sh = sh.filter(ImageFilter.GaussianBlur(blur))
    out = Image.new("RGBA", layer.size, (0, 0, 0, 0))
    out.alpha_composite(sh, off)
    out.alpha_composite(layer)
    return out


# ---------------------------------------------------------------- 断列

def split_cols(s, cap):
    """列内自上而下、列序自右向左。

    **一句一列**：断列点优先落在句读之后，允许列长在 cap 的 0.75~1.3 倍之间浮动。
    硬按 cap 切，8 字一列会把「九层之台」劈成两半 —— 竖排读起来是「一列一口气」，
    气被切断比列高不齐难看得多。
    """
    cols, cur, n = [], [], len(s)
    lo = max(2, int(cap * 0.75))
    hi = max(lo, int(cap * 1.3))
    for i, ch in enumerate(s):
        cur.append(ch)
        if len(cur) < lo:
            continue
        if len(cur) >= hi:
            cols.append(cur)
            cur = []
        elif ch in DOT and len(cur) >= lo:
            cols.append(cur)
            cur = []
    if cur:
        cols.append(cur)
    return cols


def plan(text, em, cell, P, n_fit, height_cap, target=0.55):
    """定「用几列、每列几字」。

    两个约束先钉死，再谈比例：

    * `cap <= height_cap` —— 一列排不下就是溢出，没有商量余地；
    * `cap >= 10`（短句除外）—— 一列只排七八个字，读起来是「一段话被切碎」，
      还不如少一列、每列读完整。切碎的列再整齐也难看。

    剩下的按**字块长宽比最接近 0.55** 选：传统竖幅画心大致 1:1.8，只看窗口
    高度会排出「600 宽也两列」的细高条，两侧全是白。
    """
    n = len(text)
    best = None
    for k in range(1, max(1, n_fit) + 1):
        cap = max(2, -(-n // k))
        if cap > height_cap:
            continue
        if cap < 10 and k > 1:
            continue
        cols = split_cols(text, cap)
        w = (len(cols) - 1) * P + em
        h = max(len(c) for c in cols) * cell
        if h <= 0:
            continue
        score = abs(w / h - target)
        if best is None or score < best[0]:
            best = (score, cols)
    return best[1] if best else split_cols(text, height_cap)


# ---------------------------------------------------------------- A 条幅

def scheme_a(W, H, em, text, author, clk, date):
    """左落款（时间+出处 / 日期）｜界栏｜右正文，整块居中，落款与正文同底。"""
    T, B, L, R = 1.55 * em, 1.15 * em, 0.85 * em, 0.85 * em
    cell = 1.10 * em
    P = 1.70 * em
    G = 1.45 * em
    clk_px = em * 1.10
    au_px = em * 0.58
    au_cell = au_px * 1.30
    dt_px = em * 0.44
    dt_cell = dt_px * 1.32
    clk_th = clk_px * 1.45          # 旋转后时钟块的厚度
    dt_pitch = dt_px * 1.30
    sig_w = clk_th + dt_pitch       # 落款两列的总宽
    avail_h = H - T - B
    height_cap = max(1, int(avail_h // cell))
    n_fit = max(1, int((W - L - R - sig_w - G) // P) + 1)

    cols = plan(text, em, cell, P, n_fit, height_cap)
    n = len(cols)
    body_w = (n - 1) * P + em
    rows = max(len(c) for c in cols)
    body_h = rows * cell
    # 正文块竖直居中；落款与正文**同底**收尾 —— 卷轴读到底，落款正好收住
    body_top = T + (avail_h - body_h) / 2
    body_bottom = body_top + body_h

    total = sig_w + G + body_w
    left = max(L, (W - total) / 2)
    body_r = left + total

    layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    for ci, col in enumerate(cols):
        vcol(layer, body_r - em / 2 - ci * P, body_top, col, em, FG, cell / em)

    main_cx = left + sig_w - clk_th / 2
    clk_h = len(clk) * clk_px * 0.62 + clk_px * 0.10
    au_h = len(author) * au_cell
    clk_y = body_bottom - (clk_h + em * 1.00 + au_h)
    clock_block(layer, left + sig_w, clk_y, clk, clk_px, FG)
    vcol(layer, main_cx, clk_y + clk_h + em * 1.00, author, au_px, AUTHOR, au_cell / au_px)
    vcol(layer, left + dt_px * 0.55, body_bottom - len(date) * dt_cell, date,
         dt_px, DATE, dt_cell / dt_px)

    hairline_v(layer, left + sig_w + G / 2, body_top, body_bottom, 0.16)
    return shadow_pass(layer)


# ---------------------------------------------------------------- B 极简

def scheme_b(W, H, em, text, author, clk, date):
    """极简：只有正文 + 一小段落款，没有界栏、没有日期、没有分区。

    落款优先接在最左一列**底下**（题跋式，整块仍是一根连续的柱）；接不下才
    另起一列。竖排里落款天然属于「读完之后」的位置，接排比另开一列少一处空白。
    """
    T, B, L, R = 1.45 * em, 1.10 * em, 0.90 * em, 0.90 * em
    cell = 1.10 * em
    P = 1.62 * em
    sg_px = em * 0.44
    sg_cell = sg_px * 1.34
    gap = em * 1.30

    avail_h = H - T - B
    height_cap = max(1, int(avail_h // cell))
    n_fit = max(1, int((W - L - R) // P) + 1)
    cols = plan(text, em, cell, P, n_fit, height_cap)
    n = len(cols)
    body_w = (n - 1) * P + em
    body_h = max(len(c) for c in cols) * cell
    body_top = T + (avail_h - body_h) / 2

    # 落款只有两样：时间（旋转成小块）+ 出处。日期在极简形态里让位
    sig_px = em * 0.40
    sig_cell = sig_px * 1.34
    clk_px = em * 0.85
    clk_th = clk_px * 1.45
    clk_h = len(clk) * clk_px * 0.62 + clk_px * 0.10
    au_h = len(author) * sig_cell
    need = clk_h + em * 0.70 + au_h
    col_w = max(clk_th, sig_px)

    last_bottom = body_top + len(cols[-1]) * cell
    attached = last_bottom + gap + need <= H - B
    group_w = body_w if attached else body_w + P
    left = max(L, (W - group_w) / 2)

    layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    for ci, col in enumerate(cols):
        vcol(layer, left + group_w - em / 2 - ci * P, body_top, col, em, FG, cell / em)

    last_cx = left + group_w - body_w + em / 2
    sig_x = last_cx if attached else last_cx - P
    sig_y = last_bottom + gap if attached else body_top + body_h - need
    clock_block(layer, sig_x + clk_th / 2, sig_y, clk, clk_px, AUTHOR)
    vcol(layer, sig_x, sig_y + clk_h + em * 0.70, author, sg_px, AUTHOR, sg_cell / sg_px)
    return shadow_pass(layer)


# ---------------------------------------------------------------- C 栏头

def scheme_c(W, H, em, text, author, clk, date):
    """时间横排压在顶上（+日期），一条细线分界，正文竖排居中。"""
    L, R = 0.90 * em, 0.90 * em
    cell = 1.10 * em
    P = 1.72 * em
    clk_px = em * 1.05
    head_top = 0.85 * em
    head_h = 1.85 * em
    T = head_top + head_h + 0.90 * em
    B = 1.15 * em

    layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    lw = htext(layer, W - R, head_top + clk_px * 0.86, clk, clk_px, FG, anchor="rs", tracking=0.05)
    htext(layer, W - R - lw - em * 0.55, head_top + clk_px * 0.84, date, em * 0.42, DATE, anchor="rs")
    y_rule = head_top + head_h
    d.line([(L, y_rule), (W - R, y_rule)], fill=RULE + (34,), width=1)

    height_cap = max(1, int((H - T - B) // cell))
    n_fit = max(1, int((W - L - R) // P) + 1)
    cols = plan(text, em, cell, P, n_fit, height_cap)
    n = len(cols)
    body_w = (n - 1) * P + em
    rows = max(len(c) for c in cols)
    body_h = rows * cell
    body_top = T + (H - T - B - body_h) / 2
    left = max(L, (W - body_w) / 2)
    for ci, col in enumerate(cols):
        vcol(layer, left + body_w - em / 2 - ci * P, body_top, col, em, FG, cell / em)

    if author:
        au_px = em * 0.46
        au_cell = au_px * 1.34
        h = len(author) * au_cell
        last_cx = left + em / 2
        last_bottom = body_top + len(cols[-1]) * cell + em * 1.15
        if last_bottom + h <= H - B:
            vcol(layer, last_cx, last_bottom, author, au_px, DATE, au_cell / au_px)
        else:
            vcol(layer, L + au_px * 0.6, H - B - h, author, au_px, DATE, au_cell / au_px)
    return shadow_pass(layer)


# ---------------------------------------------------------------- 现状复刻

def scheme_old(W, H, em, text, author, clk, date):
    """按当前实现的度量复刻「现状」，用来做同底片对照。

    参数取自代码，不是估计值：
      * 列距 `Metrics::col_pitch = 1.15 * char_w`（layout.rs）
      * 单元格 `cell_h = 1.05 * char_h`（同上）
      * 内边距 `pad = 24 * scale / 100`（config.rs `pad()`）
      * 落款三列并排、时间按字逐个竖排（render.rs `layout_sign`）
    """
    pad = 1.2 * em
    cell = 1.24 * em
    P = 1.15 * em
    d_px = 0.80 * em
    d_cell = 1.16 * em

    layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    # 落款：时间 / 出处 / 日期 三列并排，列距压到 1.25 字宽
    cols = [(clk, True), (author, False), (date, False)]
    cw = d_px
    pitch = cw * 1.25
    bw = (len(cols) - 1) * pitch + cw
    bx = pad
    rows = max(len(c[0]) for c in cols)
    by = (H - rows * d_cell) / 2
    for i, (txt, main) in enumerate(cols):
        vcol(layer, bx + (len(cols) - 1 - i) * pitch + cw / 2, by, txt, d_px,
             FG if main else DATE, d_cell / d_px)

    # 正文：右起贴右缘、顶部对齐，列距 1.15
    body_l = bx + bw + pad
    body_r = W - pad
    cap = max(1, int((H - 2 * pad) // cell))
    cols2 = split_cols(text, cap)
    n_fit = max(1, int((body_r - body_l) // P))
    cols2 = cols2[:n_fit]
    for ci, col in enumerate(cols2):
        vcol(layer, body_r - em / 2 - ci * P, pad, col, em, FG, cell / em)
    return shadow_pass(layer)


# ---------------------------------------------------------------- 输出

def put(out_dir, name, layer, W, H):
    bg, pad = panel(W, H)
    bg.alpha_composite(layer, (pad, pad))
    path = os.path.join(out_dir, name)
    bg.convert("RGB").save(path, optimize=True)
    print("写出", path, bg.size)


def main():
    out_dir = sys.argv[1] if len(sys.argv) > 1 else "out/mock"
    os.makedirs(out_dir, exist_ok=True)

    long_text = "合抱之木，生于毫末；九层之台，起于累土；千里之行，始于足下。"
    short_text = "千里之行，始于足下。"
    author = "《道德经》"
    clk = "15:40"
    date = "十一月四日周三"
    clk_cn = "十五时四十"

    for (W, H, em, txt, tag) in [
        (431, 660, 40, long_text, "431x660"),
        (600, 1084, 60, long_text, "600x1084"),
        (300, 660, 20, long_text, "300x660_100"),
        (431, 660, 40, short_text, "431x660_short"),
        (300, 660, 20, "君子之学必好问。问与学，相辅而行者也；非学无以致疑，非问无以广识。好学而不勤问，非真能好学者也。", "300x660_long"),
    ]:
        put(out_dir, f"A_{tag}.png", scheme_a(W, H, em, txt, author, clk, date), W, H)
        put(out_dir, f"B_{tag}.png", scheme_b(W, H, em, txt, author, clk, date), W, H)
        put(out_dir, f"C_{tag}.png", scheme_c(W, H, em, txt, author, clk, "十一月四日 周三"), W, H)

    put(out_dir, "OLD_431x660.png",
        scheme_old(431, 660, 40, long_text, author, "15:40:36", "十一月四日周三"), 431, 660)


if __name__ == "__main__":
    main()
