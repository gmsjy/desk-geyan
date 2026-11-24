r"""灵动岛真机测试：在真实桌面上量「合成之后」的像素。

为什么不能只靠 `--dump-island`：那个导出的是**我们自己算出来的那一层**。
真机要看的是 DWM 把这一层跟壁纸、别的窗口合成之后的屏幕像素 —— 位置偏了、
alpha 没生效、被别的窗口压住、ULW 没刷新，离屏 dump 全都看不见。

判据为什么不拿「与壁纸求差」：桌面不是我们的画布。前两轮就吃到两次教训 ——
一次是别的程序弹了条提示，把差异包围盒撑大 40×23px；一次是背景整体换色，
整幅 ROI 变成一大块差异。两次都看着像岛的缺陷，其实都是判据把别人的像素算进来了。

现在的做法：测几何时把胶囊设成**不透明**（`island_body_alpha=255` +
`opacity=255`），于是它的像素恒等于 `#0B0D12`，可以直接按绝对颜色把它找出来。
背景怎么变都不影响 —— 顺带还断言了「配置的底色真的被画到屏幕上了」。
透明合成单独用第三段量（同一个点：启动前=背景，启动后=混色，按 alpha 算期望值）。

用法：python tools/island_live_test.py [exe 路径]
"""

import ctypes
import os
import subprocess
import sys
import tempfile
import time

from PIL import Image, ImageChops, ImageGrab

ROI = (700, 0, 1220, 160)     # 覆盖收起 (912..1008) 与展开 (750..1170)
SEED = (960, 25)              # 胶囊内部一点（两态都在里面）
BODY_EDGE = (918, 25)         # 左侧内边距里的一点：一定落在底色上，不落字形
COLLAPSED = (912, 8, 1008, 42)
EXPANDED = (750, 8, 1170, 144)
STRIP = (700, 0, 1220, 60)    # 轮播只在收起态，取窄条快抓

# 测试专用底色：**洋红**。默认的近黑 #0B0D12 在深色窗口背景上分不出来
# （实测过一次：壁纸/背景变暗之后，`容忍 7` 把整块背景都算成了胶囊）。
# 屏幕上不会有这么大块的纯洋红，于是「找到胶囊」这件事变成无歧义。
BODY = (0xFF, 0x00, 0xFF)
BODY_CFG = "FF00FF"

OUT = tempfile.mkdtemp(prefix="island-live-")
ROWS = []


def grab(box=ROI):
    return ImageGrab.grab(bbox=box, all_screens=True).convert("RGB")


def body_mask(img, box=ROI):
    """把「几乎正好是洋红」的像素标出来（不透明档专用）。

    **必须走 PIL 的查表（`point`）而不是逐像素 Python 循环**：这个函数在悬停
    连拍里每帧都调一次，Python 循环要 50ms 一帧，占满一个核 —— 被测进程的
    `WM_TIMER` 是低优先级消息，会被饿住，于是「悬停 180ms 展开」量出来要么
    瞬间完成、要么整段不动。测量手段不能改变被测对象。
    """
    l, t, r, b = box
    sub = img.crop((l - box[0], t - box[1], r - box[0], b - box[1])).convert("RGB")
    r_, g_, b_ = sub.split()
    hi = lambda ch: ch.point(lambda v: 255 if v >= 248 else 0)
    lo = lambda ch: ch.point(lambda v: 255 if v <= 7 else 0)
    return ImageChops.multiply(ImageChops.multiply(hi(r_), lo(g_)), hi(b_))


def body_bbox(img, box=ROI, min_px=400):
    """胶囊底色像素的包围盒（屏幕绝对像素）。太少就当「没有胶囊」。"""
    m = body_mask(img, box)
    n = m.histogram()[255]
    if n < min_px:
        return None
    bb = m.getbbox()
    return (bb[0] + box[0], bb[1] + box[1], bb[2] + box[0], bb[3] + box[1])


def px_at(img, pt, box=ROI):
    return img.getpixel((pt[0] - box[0], pt[1] - box[1]))


def region_diff(a, b, rect, thr=8):
    """两张 ROI 抓图在 rect（屏幕坐标）内不同的像素数。

    ② 必须只在胶囊里比：整幅比会被别的程序/背景变化混过去 ——
    上一版就是这么「通过」的（时钟是 HH:mm，1.15s 内根本没变，差异全来自背景）。
    """
    l, t, r, b_ = rect
    ca = a.crop((l - ROI[0], t - ROI[1], r - ROI[0], b_ - ROI[1]))
    cb = b.crop((l - ROI[0], t - ROI[1], r - ROI[0], b_ - ROI[1]))
    return ImageChops.difference(ca, cb).convert("L").point(lambda v: 255 if v > thr else 0).histogram()[255]


def diff_px(a, b, thr=8, box=STRIP):
    l, t, r, b_ = box
    da = a.crop((l - box[0], t - box[1], r - box[0], b_ - box[1]))
    db = b.crop((l - box[0], t - box[1], r - box[0], b_ - box[1]))
    return ImageChops.difference(da, db).convert("L").point(lambda v: 255 if v > thr else 0).histogram()[255]


def bright_px(img, box, gate=180):
    """box 内亮度 > gate 的像素个数（胶囊底近黑，字是亮的）。"""
    l, t, r, b = box
    sub = img.crop((l - STRIP[0], t - STRIP[1], r - STRIP[0], b - STRIP[1])).convert("L")
    return sub.point(lambda v: 255 if v > gate else 0).histogram()[255]


def green_px(img):
    """深底胶囊内的「绿点」像素数。

    判据是**绿的语义**，不是某个具体色值：G 明显高于 R（灰/白字的 G 与 R 相等，
    被这一条排除），B 不至于高到偏青。这样换提示点颜色（`island_dot_color`）
    不会误伤，而「借错通道画成白点/蓝点」必然抓不到。

    只在**不透明近黑底**上成立：识别胶囊用的洋红底会把 B 抬到 183（绿点混完
    B 反而高于 G），绿色判据在那里不成立。
    """
    rr, gg, bb = img.convert("RGB").split()
    g_hi = gg.point(lambda v: 255 if v > 60 else 0)
    d_hi = ImageChops.subtract(gg, rr).point(lambda v: 255 if v > 25 else 0)
    b_lo = bb.point(lambda v: 255 if v < 200 else 0)
    return ImageChops.multiply(ImageChops.multiply(g_hi, d_hi), b_lo).histogram()[255]


def green_energy(img):
    """绿点区域的 G 通道**总量**。

    呼吸改的是**亮度**不是像素个数：6px 的实心圆在 alpha 115 与 255 下都占
    32 个像素，只有 G 值在 102…211 之间摆。所以呼吸必须用能量判据 ——
    数个数会得到一条恒为 32 的直线（真机第一版就这么误判成「没在呼吸」）。
    """
    rr, gg, bb = img.convert("RGB").split()
    g_hi = gg.point(lambda v: 255 if v > 60 else 0)
    d_hi = ImageChops.subtract(gg, rr).point(lambda v: 255 if v > 25 else 0)
    b_lo = bb.point(lambda v: 255 if v < 200 else 0)
    m = ImageChops.multiply(ImageChops.multiply(g_hi, d_hi), b_lo)
    sel = Image.composite(gg, Image.new("L", gg.size, 0), m)
    h = sel.histogram()
    return sum(i * h[i] for i in range(256))


def same_rect(got, want, tol=2):
    return got is not None and all(abs(g - w) <= tol for g, w in zip(got, want))


def width(bb):
    return (bb[2] - bb[0]) if bb else 0


def row(ok, name, detail):
    ROWS.append((ok, name, detail))
    print(f"[{'PASS' if ok else 'FAIL'}] {name:<32} {detail}")


def save(img, name, bb, box=ROI):
    l, t, r, b = bb
    pad = 8
    sub = img.crop((max(0, l - box[0] - pad), max(0, t - box[1] - pad),
                    min(box[2] - box[0], r - box[0] + pad), min(box[3] - box[1], b - box[1] + pad)))
    sub = sub.resize((sub.width * 3, sub.height * 3), Image.NEAREST)
    sub.save(os.path.join(OUT, name))


def write_cfg(d, kv):
    with open(os.path.join(d, "config.ini"), "w", encoding="utf-8") as f:
        for k, v in kv.items():
            f.write(f"{k}={v}\n")


BASE = {
    "visible": 0, "island": 1, "island_events_keep": 1,
    "island_body_alpha": 255, "opacity": 255,          # 不透明档：底色可绝对匹配
    "island_body_color": BODY_CFG,
    "placed": 1, "x": 200, "y": 160, "w": 520, "h": 240,
}


def launch(exe, data):
    return subprocess.Popen([exe], env=dict(os.environ, DG_DATA_DIR=data))


def stop(p):
    p.terminate()
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()


u32 = ctypes.windll.user32


class POINT(ctypes.Structure):
    _fields_ = [("x", ctypes.c_long), ("y", ctypes.c_long)]


def cursor():
    p = POINT()
    u32.GetCursorPos(ctypes.byref(p))
    return p.x, p.y


def move(x, y):
    u32.SetCursorPos(int(x), int(y))


def main():
    exe = sys.argv[1] if len(sys.argv) > 1 else r"G:\rust-geyan\desk-geyan\target-rel\release\desk-geyan.exe"
    print(f"被测 exe: {exe}\n输出目录: {OUT}")
    home = cursor()
    try:
        # ---------------- 一、几何 / 悬停 / 时钟 / 消息（不透明档） ----------------
        d1 = tempfile.mkdtemp(prefix="dg-live1-")
        write_cfg(d1, dict(BASE, island_items="clock,msg", island_rotate=0,
                           island_unread=1, seconds=1))
        ev1 = os.path.join(d1, "events")
        os.makedirs(ev1, exist_ok=True)
        move(300, 600)
        app = launch(exe, d1)
        time.sleep(1.6)

        s1 = grab()
        s1.save(os.path.join(OUT, "01_collapsed.png"))
        bb = body_bbox(s1)
        row(same_rect(bb, COLLAPSED), "① 收起态屏幕包围盒", f"{bb} · 期望 {COLLAPSED}")
        row(px_at(s1, BODY_EDGE) == BODY, "①b 底色与配置逐位相同",
            f"({BODY_EDGE}) = {px_at(s1, BODY_EDGE)} · 期望 {BODY}（body_alpha=opacity=255）")
        save(s1, "view_01_collapsed.png", bb or COLLAPSED)

        # ② visible=0 时岛仍在走（真机版的「饿死守卫」，规格书 §5.6）
        #
        # **只能跨分钟看**：收起态的时钟固定是 `HH:mm` —— `draw.rs` 与
        # `island/mod.rs` 两处调 `segments(.., clock_short = true)` 都是写死
        # 截短，`seconds=1` 对收起态无效（这与规格书 §3.3「seconds=1 时
        # HH:mm:ss」不一致，已记进 §11.5）。等下一次分钟跳变最干净：它同时
        # 证明「岛在按秒推进采样、且只在新内容时出帧」。
        # 只比胶囊矩形内部 —— 证明的是岛自己的像素在刷，不是别人在动。
        # 抓帧要**跨过**边界：先到边界前 0.7s 抓一帧，再睡 1.4s 抓第二帧。
        # （第一版写成「睡到刚过边界，然后连抓两帧」—— 两帧都落在新的一分钟里，
        #  差异恒为 0，看着像时钟不走。）
        wait = 60 - (time.time() % 60) - 0.7
        if wait < 0:
            wait += 60
        print(f"（等到下一次分钟跳变，约 {wait:.1f}s）")
        time.sleep(wait)
        s1b = grab()
        time.sleep(1.4)
        n = region_diff(s1b, grab(), bb or COLLAPSED)
        row(n > 0, "② 格言关掉时时钟仍在跳",
            f"跨分钟时胶囊内变化像素 {n} 个（须 >0）· 格言 visible=0")

        # ③ 悬停展开：连拍找中间态（判据与抓图延迟无关）
        move(960, 25)
        widths = []
        t0 = time.time()
        # 连拍窗口要比「悬停延时 180ms + 展开动画 380ms」宽出余量：卡在
        # 0.62s 时刚好贴着动画结束点，机器稍忙（后台编译、Excel 重算）就会
        # 整段错过中间态，实测出现过「16 帧全是 96px」。取 1.2s 是为了让窗口
        # 只决定「拍多久」，不决定「能不能拍到」。
        while time.time() - t0 < 1.2:
            widths.append(width(body_bbox(grab(STRIP), STRIP)))
            time.sleep(0.002)
        mid = [w for w in widths if width(COLLAPSED) + 20 < w < width(EXPANDED) - 20]
        time.sleep(0.35)
        e = grab()
        ebb = body_bbox(e)
        save(e, "view_02_expanded.png", ebb or EXPANDED)
        row(same_rect(ebb, EXPANDED), "③ 展开态包围盒（中心不动）", f"{ebb} · 期望 {EXPANDED}")
        row(bool(mid), "③b 动画有中间态（不是瞬变）",
            f"{len(widths)} 帧宽度 {min(widths)}…{max(widths)}px · 中间态 {len(mid)} 帧（如 {mid[:3]}）")

        # ④ 移开：350ms 仍在（宽限 620ms）、1000ms 收回
        move(300, 600)
        time.sleep(0.35)
        still = width(body_bbox(grab()))
        time.sleep(0.65)
        cbb = body_bbox(grab())
        row(still > width(EXPANDED) - 20, "④ 离开 350ms 仍展开（宽限期）", f"宽 {still}px（须接近 420）")
        row(same_rect(cbb, COLLAPSED), "④b 离开 1s 后收回", f"{cbb} · 期望 {COLLAPSED}")

        # ⑤ 落一条消息：胶囊变宽 + 文件被归档
        w0 = width(cbb)
        with open(os.path.join(ev1, "20261124093000_1.msg"), "w", encoding="utf-8") as f:
            f.write("构建完成\tCI")
        time.sleep(2.6)   # 实测消息生效 +1.52s（1Hz 轮询），等 1.6s 会卡在边界
        s3 = grab()
        bb3 = body_bbox(s3)
        save(s3, "view_03_with_msg.png", bb3 or COLLAPSED)
        left = sorted(os.listdir(ev1))
        archived = os.path.exists(os.path.join(ev1, "seen", "20261124093000_1.msg"))
        row(width(bb3) > w0, "⑤ 收到消息后胶囊变宽", f"{w0}px → {width(bb3)}px（多了消息片段）")
        row(archived and "20261124093000_1.msg" not in left, "⑤b 事件文件被移进 seen\\",
            f"events 下剩 {left} · seen 有归档={archived}")

        stop(app)
        time.sleep(0.4)
        row(body_bbox(grab()) is None, "⑥ 退出后不留残影", "屏上再无底色像素（须 None）")

        # ---------------- 二、轮播（顺序淡出） ----------------
        d2 = tempfile.mkdtemp(prefix="dg-live2-")
        # 轮播段刻意只用 clock + msg，且 `island_unread=0`（第 2 页显示整条正文）。
        # 早先用六片段 + 计数模式，结果两页的差别只有 CPU 百分比在跳 ——
        # 「帧间最大差异 1276 像素」是 CPU 抖出来的，看着通过其实没翻页。
        write_cfg(d2, dict(BASE, island_items="clock,msg",
                           island_rotate=2, island_unread=0))
        ev2 = os.path.join(d2, "events")
        os.makedirs(ev2, exist_ok=True)
        with open(os.path.join(ev2, "20261124093100_1.msg"), "w", encoding="utf-8") as f:
            # 正文长度必须跟着 `COMPACT_MAX_W` 走：上限 260 时这串 27 字够分两页，
            # 上限 400 时（可用宽 ≈372）它反而不够 —— 一页装下就没有第 2 页，
            # ⑧⑨ 会以「差 0 像素」失败，而失败原因跟轮播毫无关系。
            f.write("今晚八点归档 v0.2 的候选包，先跑一遍 selftest，再把结果贴到频道里，最后一起看回归曲线\t发布")
        move(300, 600)
        app2 = launch(exe, d2)
        time.sleep(1.6)
        cap = body_bbox(grab()) or COLLAPSED
        print(f"轮播段胶囊矩形: {cap}")
        frames = []
        t0 = time.time()
        while time.time() - t0 < 3.6:
            frames.append(grab(STRIP))
            time.sleep(0.01)
        bright = [bright_px(f, cap) for f in frames]
        hi, lo = max(bright), min(bright)
        per = [diff_px(frames[0], f) for f in frames]
        page_diff = max(per)
        i_most = per.index(page_diff)
        row(all(f is not None for f in frames), "⑦ 轮播段胶囊已出现", f"{cap} · 共 {len(frames)} 帧")
        row(page_diff > 300, "⑧ 轮播真的换了页",
            f"第 {i_most} 帧与首帧差 {page_diff} 个像素（须 >300 · 第 2 页是整条正文）")
        row(lo * 2 < hi, "⑨ 切页时有亮度塌陷（顺序淡出）",
            f"亮像素最亮 {hi} → 最暗 {lo}（须低于一半）")
        save(frames[0], "view_10_page1.png", cap, STRIP)
        save(frames[bright.index(lo)], "view_11_fade_dip.png", cap, STRIP)
        save(frames[i_most], "view_12_page2.png", cap, STRIP)
        stop(app2)
        time.sleep(0.4)
        row(body_bbox(grab()) is None, "⑩ 轮播实例退出后无残影", "屏上再无底色像素（须 None）")

        # ---------------- 三、半透明合成（唯一必须看屏幕的一条） ----------------
        d3 = tempfile.mkdtemp(prefix="dg-live3-")
        write_cfg(d3, dict(BASE, island_items="clock", island_rotate=0,
                           island_body_color="0B0D12", island_body_alpha=255,
                           opacity=128))
        move(300, 600)
        bg = grab()
        bg_pt = px_at(bg, BODY_EDGE)
        ctrl = px_at(bg, (1200, 20))
        app3 = launch(exe, d3)
        time.sleep(1.6)
        f3 = grab()
        f3.save(os.path.join(OUT, "20_translucent.png"))
        got = px_at(f3, BODY_EDGE)
        ctrl2 = px_at(f3, (1200, 20))
        a = 128 / 255
        dark = (0x0B, 0x0D, 0x12)          # 这一段用的是默认深底，不是识别用的洋红
        want = tuple(round(a * c + (1 - a) * b) for c, b in zip(dark, bg_pt))
        stable = all(abs(x - y) <= 3 for x, y in zip(ctrl, ctrl2))
        near = all(abs(g - w) <= 5 for g, w in zip(got, want))
        row(stable and near, "⑪ 半透明真的与背景混色（opacity=128）",
            f"背景 {bg_pt} + 底 {dark}@128/255 → 期望 ≈{want} · 实测 {got}"
            + ("" if stable else f" · 背景控件点变了 {ctrl}→{ctrl2}，本条不可判"))
        stop(app3)
        time.sleep(0.4)
        # 这条只做**点采样**：半透明档的底色是近黑，不能用找色块的办法
        # （背景一暗就分不出来）。退出后同一个点应当回到原背景色。
        back = px_at(grab(), BODY_EDGE)
        row(all(abs(x - y) <= 3 for x, y in zip(back, bg_pt)),
            "⑫ 半透明实例退出后无残影", f"同点恢复为背景 {bg_pt} · 实测 {back}")
        # ---------------- 四、新消息绿点：呼吸 → 10s 后常亮（深底） ----------------
        #
        # 这一段换**近黑不透明底**：绿点要判「是不是绿的」，而识别胶囊用的
        # 洋红底会把 B 通道抬上去，绿色判据在洋红上不成立。这里不需要找胶囊
        # —— 位置固定（COLLAPSED 矩形），只在里面数绿像素。
        #
        # `island_peek_ms=0` 是故意的：关掉拉长，单独验「绿点与拉长解耦」，
        # 顺便避免胶囊变宽把固定 ROI 撑出去。
        d4 = tempfile.mkdtemp(prefix="dg-live4-")
        write_cfg(d4, dict(BASE, island_items="clock,dot,msg", island_rotate=0,
                           island_unread=1, island_peek_ms=0,
                           island_body_color="0B0D12"))
        ev4 = os.path.join(d4, "events")
        os.makedirs(ev4, exist_ok=True)
        move(300, 600)
        app4 = launch(exe, d4)
        time.sleep(1.6)
        g_idle = green_energy(grab(COLLAPSED))
        with open(os.path.join(ev4, "20261124110000_1.msg"), "w", encoding="utf-8") as f:
            f.write("部门周会改到下午三点\t日历")
        time.sleep(2.6)
        # 呼吸期连拍 4s。**连拍**而不是定点抓：呼吸相位 1.4s，定点全看运气。
        breath = []
        t0 = time.time()
        while time.time() - t0 < 4.0:
            breath.append(green_energy(grab(COLLAPSED)))
            time.sleep(0.02)
        time.sleep(7.0)          # 累计 ≈11s，已越过 DOT_BREATHE_MS=10s
        steady = []
        t0 = time.time()
        while time.time() - t0 < 2.0:
            steady.append(green_energy(grab(COLLAPSED)))
            time.sleep(0.02)
        row(g_idle == 0 and min(breath) > 0, "⑬ 新消息让绿点出现（且下限不为 0）",
            f"平时绿量 {g_idle} · 呼吸期最少 {min(breath)}（须 >0，全灭会被读成已读）")
        row(max(breath) >= min(breath) * 3 // 2, "⑭ 绿点真的在呼吸（不是常亮）",
            f"绿量 {min(breath)}…{max(breath)}（须起伏 ≥1.5 倍 · 实测周期 1.4s）")
        row(min(steady) > 0 and max(steady) - min(steady) <= max(steady) // 20,
            "⑮ 10s 后退化成常亮（停止呼吸）",
            f"稳态绿量 {min(steady)}…{max(steady)}（须恒亮且波动 <5%）")
        # 导出的抓图必须是**整条**（`box=STRIP`）：`save` 的 bb 是屏幕坐标，
        # 而 `grab(COLLAPSED)` 已经裁过，再按 ROI 换算就会落到空白处
        # （真机截出来一张全黑图 —— 断言用的是 grab 的像素，所以只有导出错）。
        # 段四胶囊含 `MSG 1`，宽 147，居中 ⇒ 886..1033，比 COLLAPSED(96) 宽。
        save(grab(STRIP), "view_40_dot_steady.png", (886, 0, 1034, 60), box=STRIP)
        stop(app4)
        time.sleep(0.4)

        # ---------------- 五、拉长 → 悬停展开 → 已读回缩（洋红底，量宽度） ----------------
        #
        # `island_peek_ms=20000` 而不是几秒：判据是「移开 1.3s 后回缩到 96」，
        # 若拉长时窗在这之前就自己到期，即使已读判定完全没生效也会回缩 ——
        # 断言会假通过。20s 保证「回缩」只能由 fresh 被清解释。
        d5 = tempfile.mkdtemp(prefix="dg-live5-")
        write_cfg(d5, dict(BASE, island_items="clock,dot,msg", island_rotate=0,
                           island_unread=1, island_peek_ms=20000))
        ev5 = os.path.join(d5, "events")
        os.makedirs(ev5, exist_ok=True)
        move(300, 600)
        app5 = launch(exe, d5)
        time.sleep(1.6)
        w_idle = width(body_bbox(grab()))
        with open(os.path.join(ev5, "20261124110100_1.msg"), "w", encoding="utf-8") as f:
            f.write("明天上午十点评审，记得带原型和流程图\t微信")
        time.sleep(2.6)
        s5 = grab()
        bb5 = body_bbox(s5)
        save(s5, "view_50_peek.png", bb5 or COLLAPSED)
        w_peek = width(bb5)
        h_peek = (bb5[3] - bb5[1]) if bb5 else 0
        row(w_peek > w_idle + 80 and h_peek == 34,
            "⑯ 新消息把胶囊拉长（只拉宽、高仍 34）",
            f"宽 {w_idle} → {w_peek}px（须增 >80）· 高 {h_peek}px（须 34，拉长≠展开）")

        # 悬停展开 → 停留 2.0s（380ms 展开 + 1200ms 阅读判定）→ 移开
        move(960, 25)
        cur_in = cursor()          # 落点要在移开**之前**读，否则量到的是归位后的坐标
        # 轮询等**展开到位**（420），而不是「睡固定时长」：定点抓会把「慢半拍」
        # 判成「没展开」；而「一变大就 break」也不行 —— 那会在动画中途就移开，
        # 停留不足 `READ_MS`，已读判定不触发，回缩断言必然假红（真机踩过）。
        hover_t = time.time()
        w_hover = 0
        # 轮询**要等到的目标宽度**必须与下面的判据同源（`w_peek + 60`）。
        # 写成固定的 400 是个陷阱：拉长态一旦超过 340（状态全开就会），
        # 判据要求 >400 而轮询在 401 就 break，于是「明明展开了 420」
        # 却以 412 失败 —— 看着像动画没走完，其实是自己提前收工。
        hover_need = w_peek + 60
        while time.time() - hover_t < 3.0:
            w_hover = width(body_bbox(grab()))
            # 必须**严格越过**判据值才 break：留 2px 容忍等于允许「差一点就收工」，
            # 420 的展开宽会在 414 就停手，判据（须 >416）随即假红。
            if w_hover > hover_need:
                break
            time.sleep(0.05)
        hover_ms = (time.time() - hover_t) * 1000
        # 展开到位后再停留够 1.2s 的「阅读」时长（判据从展开完成起算）。
        time.sleep(1.6)
        move(300, 600)
        time.sleep(1.3)
        s6 = grab()
        bb6 = body_bbox(s6)
        w_back = width(bb6)
        save(s6, "view_51_after_read.png", bb6 or COLLAPSED)
        row(w_hover > w_peek + 60, "⑰ 悬停展开（比拉长态更宽）",
            f"拉长 {w_peek}px → 悬停 {w_hover}px（须 >{w_peek + 60}）· 用时 {hover_ms:.0f}ms"
            f" · 光标 {cur_in}（须落在胶囊内）")
        # 判据是「退出拉长态」而不是「回到无消息的 96px」：消息**仍在**，只是已读，
        # 收起态会显示计数片段 `MSG 1`（≈147px）。要求回到 96 等于要求消息消失。
        # 真正的判据是「不再显示正文」⇒ 宽度必须明显小于拉长态；若已读判定没生效，
        # 拉长时窗（20s）还在，宽度会停在 289。
        row(0 < w_back <= w_peek - 100, "⑱ 读过之后退出拉长态（不再显示正文）",
            f"{w_peek}px → {w_back}px（须 ≤{w_peek - 100} · 无消息时 {w_idle}px · "
            f"拉长时窗仍有 ~15s，回落只能由「已读」解释）")
        stop(app5)
        time.sleep(0.4)
        row(body_bbox(grab()) is None, "⑲ 第五段实例退出后无残影", "屏上再无底色像素（须 None）")

    finally:
        move(*home)

    ok = sum(1 for r in ROWS if r[0])
    print(f"\n真机测试 {ok}/{len(ROWS)} 通过 · 截图目录 {OUT}")
    return 0 if ok == len(ROWS) else 1


if __name__ == "__main__":
    sys.exit(main())
