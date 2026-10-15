"""抓真实毛玻璃效果图（README 里那三张就是这么来的）。

`--dump` 只能导出文字层 —— Acrylic 是 DWM 合成出来的，dump 拿不到。所以毛玻璃
效果图必须走**屏幕抓图**：先在挂件下方铺一张彩色背景（模拟桌面内容，让模糊有
东西可模糊），再启动真实实例，最后裁出窗口矩形（四周留背景余量，形成
「清晰背景 vs 挂件下的模糊」对照）。

背景窗口用 ctypes 直接建 GDI 窗口绘制 DIB（本机 Python 没有 tkinter）。

用法：
    python glass_shot.py <输出.png> <w> <h> <x> <y> [config key=value ...]

环境变量：
    DG_SHOT_EXE   要启动的 exe（默认 target/release/desk-geyan.exe）
    DG_SHOT_BG    calm = 宁净背景（给纯文字模式用，太花的背景会喧宾夺主）

**跑之前先退出正在运行的实例**：程序有单实例互斥（`Local\\DeskGeyan_SingleInstance`），
第二个进程会静默退出、截图里就没有挂件。若确实需要并存，可临时把源码里那个
互斥名改掉、用独立 target 目录出一个临时构建跑（本次就是这么做的，跑完即撤）。
"""
import ctypes
import ctypes.wintypes as wt
import os
import subprocess
import sys
import threading
import time

from PIL import Image, ImageDraw, ImageGrab

user32 = ctypes.WinDLL("user32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

# 不声明这些签名的话，LPARAM 会在回调里溢出（OverflowError），
# 窗口过程直接抛异常、窗口画不出来。
user32.DefWindowProcW.argtypes = [wt.HWND, ctypes.c_uint, wt.WPARAM, wt.LPARAM]
user32.DefWindowProcW.restype = ctypes.c_longlong
user32.GetMessageW.argtypes = [ctypes.POINTER(wt.MSG), wt.HWND, ctypes.c_uint, ctypes.c_uint]
user32.GetMessageW.restype = ctypes.c_int
user32.PostThreadMessageW.argtypes = [wt.DWORD, ctypes.c_uint, wt.WPARAM, wt.LPARAM]
user32.EnumWindows.argtypes = [ctypes.c_void_p, wt.LPARAM]
user32.GetClassNameW.argtypes = [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]
user32.GetWindowThreadProcessId.argtypes = [wt.HWND, ctypes.POINTER(wt.DWORD)]
user32.GetWindowThreadProcessId.restype = wt.DWORD
user32.GetWindowRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]

try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)
except Exception:
    user32.SetProcessDPIAware()

ROOT = r"G:\rust-geyan\desk-geyan"
EXE = os.environ.get(
    "DG_SHOT_EXE", os.path.join(ROOT, "target", "release", "desk-geyan.exe"))
DATA = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "dg-glass-data")

SW = user32.GetSystemMetrics(0)
SH = user32.GetSystemMetrics(1)


class PAINTSTRUCT(ctypes.Structure):
    _fields_ = [("hdc", wt.HDC), ("fErase", wt.BOOL), ("rcPaint", wt.RECT),
                ("fRestore", wt.BOOL), ("fIncUpdate", wt.BOOL),
                ("rgbReserved", ctypes.c_byte * 32)]


class BMIH(ctypes.Structure):
    _fields_ = [("biSize", wt.DWORD), ("biWidth", ctypes.c_long), ("biHeight", ctypes.c_long),
                ("biPlanes", wt.WORD), ("biBitCount", wt.WORD), ("biCompression", wt.DWORD),
                ("biSizeImage", wt.DWORD), ("biXPelsPerMeter", ctypes.c_long),
                ("biYPelsPerMeter", ctypes.c_long), ("biClrUsed", wt.DWORD),
                ("biClrImportant", wt.DWORD)]


class BMI(ctypes.Structure):
    _fields_ = [("bmiHeader", BMIH), ("bmiColors", wt.DWORD * 3)]


class WNDCLASS(ctypes.Structure):
    _fields_ = [("style", ctypes.c_uint), ("lpfnWndProc", ctypes.c_void_p),
                ("cbClsExtra", ctypes.c_int), ("cbWndExtra", ctypes.c_int),
                ("hInstance", wt.HINSTANCE), ("hIcon", wt.HICON), ("hCursor", wt.HANDLE),
                ("hbrBackground", wt.HBRUSH), ("lpszMenuName", wt.LPCWSTR),
                ("lpszClassName", wt.LPCWSTR)]


def make_bg_calm(w: int, h: int) -> Image.Image:
    """宁净背景：深色渐变 + 几团大柔光。给「纯文字模式」用 ——
    没有底板，文字直接压在背景上，背景太花会喧宾夺主。"""
    import random

    img = Image.new("RGB", (w, h))
    d = ImageDraw.Draw(img)
    for y in range(h):
        t = y / max(1, h - 1)
        d.line([(0, y), (w, y)],
               fill=(int(22 + 68 * t), int(30 + 62 * t), int(58 + 104 * t)))
    random.seed(11)
    pal = [(64, 74, 150), (44, 104, 132), (108, 66, 142),
           (54, 92, 120), (86, 70, 128)]
    for _ in range(26):
        r = random.randint(90, 260)
        cx, cy = random.randint(-r, w + r), random.randint(-r, h + r)
        d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=random.choice(pal))
    return img


def make_bg(w: int, h: int) -> Image.Image:
    """「壁纸感」的彩色背景：深色渐变 + 密集彩色圆点。

    圆点是**高频**内容 —— 毛玻璃（Acrylic）的模糊半径小，大色块边缘看着
    仍然锐利、容易看不出模糊；密集小圆点被模糊后会化成柔和光斑，
    「清晰区 vs 挂件下」的对照一眼可辨。
    """
    import random

    img = Image.new("RGB", (w, h))
    d = ImageDraw.Draw(img)
    for y in range(h):
        t = y / max(1, h - 1)
        d.line([(0, y), (w, y)],
               fill=(int(44 + 150 * t), int(56 + 120 * t), int(104 + 140 * t)))
    random.seed(7)
    pal = [(255, 138, 96), (255, 222, 118), (128, 240, 168),
           (150, 130, 250), (104, 200, 250), (250, 130, 200), (255, 255, 255)]
    for _ in range(3000):
        r = random.randint(4, 13)
        cx, cy = random.randint(0, w), random.randint(0, h)
        d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=random.choice(pal))
    return img


def run_bg_window(bg: Image.Image, ready: threading.Event, stop: threading.Event) -> None:
    data = bg.tobytes("raw", "BGRX")          # 32bpp，自上而下
    buf = ctypes.create_string_buffer(data, len(data))
    bmi = BMI()
    bmi.bmiHeader.biSize = ctypes.sizeof(BMIH)
    bmi.bmiHeader.biWidth = SW
    bmi.bmiHeader.biHeight = -SH
    bmi.bmiHeader.biPlanes = 1
    bmi.bmiHeader.biBitCount = 32
    bmi.bmiHeader.biCompression = 0

    def proc(hwnd, msg, wp, lp):
        if msg == 0x000F:                      # WM_PAINT
            ps = PAINTSTRUCT()
            hdc = user32.BeginPaint(hwnd, ctypes.byref(ps))
            gdi32.StretchDIBits(hdc, 0, 0, SW, SH, 0, 0, SW, SH,
                                buf, ctypes.byref(bmi), 0, 0x00CC0020)
            user32.EndPaint(hwnd, ctypes.byref(ps))
            return 0
        if msg == 0x0010:                      # WM_CLOSE
            user32.DestroyWindow(hwnd)
            return 0
        if msg == 0x0002:                      # WM_DESTROY
            user32.PostQuitMessage(0)
            return 0
        return user32.DefWindowProcW(hwnd, msg, wp, lp)

    fn = ctypes.WINFUNCTYPE(ctypes.c_longlong, wt.HWND, ctypes.c_uint,
                            wt.WPARAM, wt.LPARAM)(proc)
    hinst = kernel32.GetModuleHandleW(None)
    cls = "DGBgShot"
    wc = WNDCLASS()
    wc.lpfnWndProc = ctypes.cast(fn, ctypes.c_void_p)
    wc.hInstance = hinst
    wc.lpszClassName = cls
    user32.RegisterClassW(ctypes.byref(wc))
    hwnd = user32.CreateWindowExW(0, cls, "bg", 0x80000000,  # WS_POPUP
                                  0, 0, SW, SH, None, None, hinst, None)
    user32.ShowWindow(hwnd, 5)
    user32.UpdateWindow(hwnd)
    ready.set()

    msg = wt.MSG()
    while user32.GetMessageW(ctypes.byref(msg), None, 0, 0) > 0:
        user32.TranslateMessage(ctypes.byref(msg))
        user32.DispatchMessageW(ctypes.byref(msg))
        if stop.is_set():
            break


def find_panel_of_pid(pid: int):
    """按 PID 找挂件底板窗口 —— 机器上可能同时跑着用户自己的实例，
    只按类名找会抓错窗口。"""
    hit = []
    fn = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)

    def cb(hwnd, lp):
        name = ctypes.create_unicode_buffer(64)
        user32.GetClassNameW(hwnd, name, 64)
        if name.value == "DeskGeyanPanel":
            wpid = wt.DWORD()
            user32.GetWindowThreadProcessId(hwnd, ctypes.byref(wpid))
            if wpid.value == pid:
                hit.append(hwnd)
                return False
        return True

    user32.EnumWindows(ctypes.cast(fn(cb), ctypes.c_void_p), 0)
    return hit[0] if hit else None


def main() -> int:
    out = sys.argv[1]
    w, h, x, y = (int(v) for v in sys.argv[2:6])
    extra = sys.argv[6:]

    os.makedirs(DATA, exist_ok=True)
    cfg = [
        f"x={x}", f"y={y}", f"w={w}", f"h={h}",
        "opacity=255", "mode=glass", "anim=static", "always_on_top=1",
        "show_clock=1", "show_date=1", "show_quote=1", "seconds=1", "shadow=1",
        "scale=100", "clock_scale=100", "quote_scale=100",
        "auto_hide_fs=0", "placed=1", "layout_mode=0", "visible=1",
        "orient=horizontal", "letter_spacing=0", "quote_color=unset", "shrink_fit=1",
    ] + list(extra)
    with open(os.path.join(DATA, "config.ini"), "w", encoding="utf-8") as f:
        f.write("\n".join(cfg) + "\n")

    print(f"screen {SW}x{SH}  target {w}x{h} at ({x},{y})")

    bg = make_bg_calm(SW, SH) if os.environ.get("DG_SHOT_BG") == "calm" else make_bg(SW, SH)
    ready = threading.Event()
    stop = threading.Event()
    t = threading.Thread(target=run_bg_window, args=(bg, ready, stop), daemon=True)
    t.start()
    ready.wait(5)
    time.sleep(0.8)

    env = os.environ.copy()
    env["DG_DATA_DIR"] = DATA
    proc = subprocess.Popen([EXE], env=env, cwd=ROOT)
    time.sleep(3.5)

    hwnd = find_panel_of_pid(proc.pid)
    print("panel hwnd of my pid:", hwnd)
    rect = wt.RECT()
    if hwnd and user32.GetWindowRect(hwnd, ctypes.byref(rect)):
        # 四周留背景余量：上方多留一条，形成「清晰背景 vs 挂件下的模糊」对照
        box = (max(0, rect.left - 30), max(0, rect.top - 150),
               rect.right + 30, rect.bottom + 30)
    else:
        box = (max(0, x - 30), max(0, y - 150), x + w + 30, y + h + 30)
    print("grab box:", box)

    shot = ImageGrab.grab(all_screens=False)
    crop = shot.crop(box)
    crop.save(out)
    print("saved:", out, crop.size)

    proc.terminate()
    try:
        proc.wait(timeout=5)
    except Exception:
        proc.kill()
    stop.set()
    user32.PostThreadMessageW(t.ident, 0x0012, 0, 0)  # WM_QUIT
    time.sleep(0.3)
    return 0


if __name__ == "__main__":
    sys.exit(main())
