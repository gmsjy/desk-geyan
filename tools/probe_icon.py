"""复现程序内部 `crate::icon::app_icon()` 的取值路径：
把 exe 当数据文件加载 → 取其模块句柄 → LoadImageW(RT_GROUP_ICON #1)。
用来确认「程序交给 shell 的 HICON 就是设计稿那版」。
"""
import ctypes
import ctypes.wintypes as w

from PIL import Image

k32 = ctypes.windll.kernel32
u32 = ctypes.windll.user32
g32 = ctypes.windll.gdi32

EXE = r"D:/work/dev/desk-geyan/target/release/desk-geyan.exe"
LOAD_LIBRARY_AS_DATAFILE = 0x00000002
IMAGE_ICON, LR_DEFAULTCOLOR = 1, 0


class ICONINFO(ctypes.Structure):
    _fields_ = [("fIcon", w.BOOL), ("xHotspot", w.DWORD), ("yHotspot", w.DWORD),
                ("hbmMask", w.HBITMAP), ("hbmColor", w.HBITMAP)]


class BITMAP(ctypes.Structure):
    _fields_ = [("bmType", w.LONG), ("bmWidth", w.LONG), ("bmHeight", w.LONG),
                ("bmWidthBytes", w.LONG), ("bmPlanes", w.WORD), ("bmBitsPixel", w.WORD),
                ("bmBits", ctypes.c_void_p)]


class BIH(ctypes.Structure):
    _fields_ = [("biSize", w.DWORD), ("biWidth", w.LONG), ("biHeight", w.LONG),
                ("biPlanes", w.WORD), ("biBitCount", w.WORD), ("biCompression", w.DWORD),
                ("biSizeImage", w.DWORD), ("biXPelsPerMeter", w.LONG),
                ("biYPelsPerMeter", w.LONG), ("biClrUsed", w.DWORD), ("biClrImportant", w.DWORD)]


def to_image(hicon):
    ii = ICONINFO()
    u32.GetIconInfo(hicon, ctypes.byref(ii))
    bm = BITMAP()
    g32.GetObjectW(ii.hbmColor, ctypes.sizeof(bm), ctypes.byref(bm))
    wd, ht = bm.bmWidth, bm.bmHeight
    bi = BIH()
    bi.biSize = ctypes.sizeof(bi)
    bi.biWidth, bi.biHeight = wd, -ht
    bi.biPlanes, bi.biBitCount = 1, 32
    buf = ctypes.create_string_buffer(wd * ht * 4)
    hdc = u32.GetDC(None)
    g32.GetDIBits(hdc, ii.hbmColor, 0, ht, buf, ctypes.byref(bi), 0)
    u32.ReleaseDC(None, hdc)
    im = Image.frombytes("RGBA", (wd, ht), bytes(buf))
    b, g, r, a = im.split()
    return Image.merge("RGBA", (r, g, b, a))


mod = k32.LoadLibraryExW(EXE, None, LOAD_LIBRARY_AS_DATAFILE)
print("module handle:", hex(mod) if mod else 0)
if mod:
    for size in (16, 24, 32):
        h = u32.LoadImageW(mod, ctypes.cast(1, w.LPCWSTR), IMAGE_ICON, size, size, LR_DEFAULTCOLOR)
        if not h:
            print(f"  {size:3d}px -> FAIL (err {k32.GetLastError()})")
            continue
        im = to_image(h)
        ink = sum(1 for p in im.getdata() if p[3] > 8)
        print(f"  {size:3d}px -> HICON ok, 实际 {im.width}x{im.height}, 不透明像素 {ink}")
        im.resize((im.width * 6, im.height * 6), Image.NEAREST).save(
            rf"D:/work/dev/desk-geyan/target/probe_{size}.png")
