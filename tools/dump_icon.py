"""把 exe 里的图标取出来存成 PNG，用于人工核对（不是构建流程的一部分）。"""
import ctypes
import ctypes.wintypes as w

from PIL import Image

user32 = ctypes.windll.user32
shell32 = ctypes.windll.shell32
gdi32 = ctypes.windll.gdi32


class ICONINFO(ctypes.Structure):
    _fields_ = [("fIcon", w.BOOL), ("xHotspot", w.DWORD), ("yHotspot", w.DWORD),
                ("hbmMask", w.HBITMAP), ("hbmColor", w.HBITMAP)]


class BITMAP(ctypes.Structure):
    _fields_ = [("bmType", w.LONG), ("bmWidth", w.LONG), ("bmHeight", w.LONG),
                ("bmWidthBytes", w.LONG), ("bmPlanes", w.WORD), ("bmBitsPixel", w.WORD),
                ("bmBits", ctypes.c_void_p)]


class BITMAPINFOHEADER(ctypes.Structure):
    _fields_ = [("biSize", w.DWORD), ("biWidth", w.LONG), ("biHeight", w.LONG),
                ("biPlanes", w.WORD), ("biBitCount", w.WORD), ("biCompression", w.DWORD),
                ("biSizeImage", w.DWORD), ("biXPelsPerMeter", w.LONG),
                ("biYPelsPerMeter", w.LONG), ("biClrUsed", w.DWORD), ("biClrImportant", w.DWORD)]


def icon_to_image(hicon):
    ii = ICONINFO()
    user32.GetIconInfo(hicon, ctypes.byref(ii))
    bm = BITMAP()
    gdi32.GetObjectW(ii.hbmColor, ctypes.sizeof(bm), ctypes.byref(bm))
    wd, ht = bm.bmWidth, bm.bmHeight

    bi = BITMAPINFOHEADER()
    bi.biSize = ctypes.sizeof(bi)
    bi.biWidth, bi.biHeight = wd, -ht  # 负高度 ⇒ top-down
    bi.biPlanes, bi.biBitCount = 1, 32
    buf = ctypes.create_string_buffer(wd * ht * 4)
    hdc = user32.GetDC(None)
    gdi32.GetDIBits(hdc, ii.hbmColor, 0, ht, buf, ctypes.byref(bi), 0)
    user32.ReleaseDC(None, hdc)

    im = Image.frombytes("RGBA", (wd, ht), bytes(buf))
    b, g, r, a = im.split()
    return Image.merge("RGBA", (r, g, b, a))


if __name__ == "__main__":
    import sys

    exe = sys.argv[1] if len(sys.argv) > 1 else r"D:/work/dev/desk-geyan/target/release/desk-geyan.exe"
    large = (w.HICON * 1)()
    small = (w.HICON * 1)()
    n = shell32.ExtractIconExW(exe, 0, large, small, 1)
    print("count", n, "large", bool(large[0]), "small", bool(small[0]))
    for tag, h in (("large", large[0]), ("small", small[0])):
        if not h:
            continue
        im = icon_to_image(h)
        print(tag, im.size)
        im.save(rf"D:/work/dev/desk-geyan/target/exe_{tag}.png")
        im.resize((im.width * 4, im.height * 4), Image.NEAREST).save(
            rf"D:/work/dev/desk-geyan/target/exe_{tag}_zoom.png")
