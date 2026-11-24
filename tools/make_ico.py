"""把多尺寸 PNG 打包成 .ico，小尺寸用 BMP(DIB)、256 用 PNG。

为什么不能直接用 Pillow 的 ICO 保存：
它会把每一项都写成 PNG。PNG 压缩的图标在 Vista+ 技术上合法，但**托盘、
Alt+Tab 这类 shell 组件对 <64px 的 PNG 项支持很差**（常见表现是不显示或糊成一团）。
微软的推荐是：256×256 用 PNG，其余用 BMP。这里照做。
"""
import struct
import sys
from io import BytesIO

from PIL import Image


def dib_bytes(im: Image.Image) -> bytes:
    """32bpp 非预乘 BGRA 的 BITMAPINFOHEADER + 像素 + AND 掩码。"""
    im = im.convert("RGBA")
    w, h = im.size
    px = im.load()

    # BITMAPINFOHEADER：biHeight 写 2*h（XOR 图 + AND 掩码合在一起算）
    hdr = struct.pack(
        "<IiiHHIIiiII",
        40, w, h * 2, 1, 32, 0,  # biSize biWidth biHeight biPlanes biBitCount biCompression
        w * h * 4, 0, 0, 0, 0,   # biSizeImage biXPels biYPels biClrUsed biClrImportant
    )

    # XOR 位图：bottom-up 逐行，每像素 BGRA（非预乘，alpha 独立）
    rows = []
    for y in range(h - 1, -1, -1):
        row = bytearray()
        for x in range(w):
            r, g, b, a = px[x, y]
            row += bytes((b, g, r, a))
        rows.append(bytes(row))

    # AND 掩码：1bpp，1 = 完全透明。32bpp 下系统优先看 alpha，
    # 但老组件会读这里，所以按 alpha==0 置位。
    stride = ((w + 31) // 32) * 4
    mask = []
    for y in range(h - 1, -1, -1):
        row = bytearray(stride)
        for x in range(w):
            if px[x, y][3] == 0:
                row[x >> 3] |= 0x80 >> (x & 7)
        mask.append(bytes(row))

    return hdr + b"".join(rows) + b"".join(mask)


def png_bytes(im: Image.Image) -> bytes:
    buf = BytesIO()
    im.save(buf, format="PNG", optimize=True)
    return buf.getvalue()


def build(paths, out_path):
    imgs = []
    for p in paths:
        im = Image.open(p).convert("RGBA")
        imgs.append(im)
    imgs.sort(key=lambda i: i.width)

    entries, blobs = [], []
    offset = 6 + 16 * len(imgs)
    for im in imgs:
        w, h = im.size
        blob = png_bytes(im) if w >= 256 else dib_bytes(im)
        entries.append(
            struct.pack(
                "<BBBBHHII",
                w if w < 256 else 0, h if h < 256 else 0,
                0, 0, 1, 32, len(blob), offset,
            )
        )
        blobs.append(blob)
        offset += len(blob)

    with open(out_path, "wb") as f:
        f.write(struct.pack("<HHH", 0, 1, len(imgs)))
        for e in entries:
            f.write(e)
        for b in blobs:
            f.write(b)


if __name__ == "__main__":
    sizes = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
    build([f"target/icon_{s}.png" for s in sizes], "assets/desk-geyan.ico")
    print("wrote assets/desk-geyan.ico")
