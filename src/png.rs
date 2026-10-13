//! 极简 PNG 写出（零依赖）。
//!
//! 整个程序刻意不带任何运行时依赖，所以这里不引 `image` / `png` crate，
//! 自己拼一个合法的 PNG：
//!
//! ```text
//!   签名 + IHDR + IDAT + IEND
//!                     └ zlib 流：0x78 0x01 + **stored（不压缩）** deflate 块 + adler32
//! ```
//!
//! deflate 的 stored 块（BTYPE=00）只是「长度 + 取反长度 + 原始字节」，
//! 不需要 Huffman 编码表，几十行就能写完且**一定**能被任何解码器读出来。
//! 代价是文件不压缩 —— 620×176 的 RGBA 大约 440KB，对调试输出完全够用。
//!
//! 用途：`desk-geyan --dump out.png` 把渲染结果落成图片，
//! 调颜色 / 调位置时不用盯着跑马灯等它滚过来。

/// PNG 每个 chunk 的 CRC32（多项式 0xEDB88320）
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let m = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & m);
        }
    }
    !crc
}

/// zlib 流末尾的 adler32
fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut body = Vec::with_capacity(4 + data.len());
    body.extend_from_slice(kind);
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
}

/// `rgba` 必须是 `w * h * 4` 字节、行优先、已经是**非预乘**的显示色。
pub fn write_png(path: &str, w: u32, h: u32, rgba: &[u8]) -> std::io::Result<()> {
    let stride = (w * 4) as usize;
    // 每条扫描线前面加一个 filter 字节（0 = None）
    let mut raw = Vec::with_capacity((stride + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        let off = y * stride;
        raw.extend_from_slice(&rgba[off..off + stride]);
    }

    let mut out = Vec::with_capacity(raw.len() + 1024);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.push(8); // 位深
    ihdr.push(6); // 颜色类型 6 = RGBA
    ihdr.push(0); // 压缩方式（固定 0）
    ihdr.push(0); // 过滤方式（固定 0）
    ihdr.push(0); // 隔行（0 = 不隔行）
    chunk(&mut out, b"IHDR", &ihdr);

    // zlib 容器 + stored deflate 块
    let mut z = vec![0x78, 0x01];
    let mut i = 0usize;
    while i < raw.len() {
        let n = (raw.len() - i).min(0xFFFF);
        let last = i + n >= raw.len();
        z.push(if last { 1 } else { 0 }); // BFINAL + BTYPE=00
        z.extend_from_slice(&(n as u16).to_le_bytes());
        z.extend_from_slice(&(!(n as u16)).to_le_bytes()); // LEN 的取反
        z.extend_from_slice(&raw[i..i + n]);
        i += n;
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);

    std::fs::write(path, out)
}
