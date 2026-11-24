//! 把 `assets/desk-geyan.ico` 编进 PE 资源，让 exe 在资源管理器 / 任务栏 / 托盘
//! 里都显示自家图标。
//!
//! 为什么手写 .res 而不是用 `winres` / `embed-resource`：
//!   1. 本项目刻意保持「零依赖」，只吃 windows-sys；
//!   2. 那两个 crate 都要调 MSVC 的 `rc.exe`，而 `rc.exe` 不在 PATH 里
//!      （得自己摸 Windows Kits 的版本子目录），换台机器就可能构建不出来。
//!
//! `.res` 的二进制格式很小（32 字节块头 + 数据），自己写反而更可控、跨机一致。
//!
//! 失败策略：**任何一步不对就静默跳过**，不写链接参数，程序照常编译，
//! 只是退回系统默认图标 —— 图标不该成为构建的硬前置。

use std::fs;
use std::path::PathBuf;

const RT_ICON: u32 = 3;
const RT_GROUP_ICON: u32 = 14;

fn main() {
    println!("cargo:rerun-if-changed=assets/desk-geyan.ico");
    println!("cargo:rerun-if-changed=build.rs");

    let ico = PathBuf::from("assets/desk-geyan.ico");
    let Ok(raw) = fs::read(&ico) else {
        return;
    };
    let Some(items) = parse_ico(&raw) else {
        return;
    };
    let res = build_res(&items);

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let path = out_dir.join("app_icon.res");
    if fs::write(&path, &res).is_err() {
        return;
    }

    // 用 link-search + 裸文件名，避开路径里的空格 / 反斜杠转义问题
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-arg-bins=app_icon.res");
}

/// ICO 目录里的一项。
struct IcoItem<'a> {
    /// 宽 / 高（字节形式，0 表示 256）
    w: u8,
    h: u8,
    /// 调色板色数（真彩恒 0）
    color_count: u8,
    planes: u16,
    bit_count: u16,
    /// PNG 或 DIB 原始字节
    data: &'a [u8],
}

/// 解析 ICONDIR / ICONDIRENTRY。格式不对就返回 None（不做容错修补）。
fn parse_ico(raw: &[u8]) -> Option<Vec<IcoItem<'_>>> {
    if raw.len() < 6 {
        return None;
    }
    let reserved = u16::from_le_bytes([raw[0], raw[1]]);
    let kind = u16::from_le_bytes([raw[2], raw[3]]);
    let count = u16::from_le_bytes([raw[4], raw[5]]) as usize;
    if reserved != 0 || kind != 1 || count == 0 {
        return None;
    }
    if 6 + count * 16 > raw.len() {
        return None;
    }

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let e = &raw[6 + i * 16..6 + i * 16 + 16];
        let bytes = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as usize;
        let off = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as usize;
        if bytes == 0 || off.checked_add(bytes)? > raw.len() {
            return None;
        }
        out.push(IcoItem {
            w: e[0],
            h: e[1],
            color_count: e[2],
            planes: u16::from_le_bytes([e[4], e[5]]),
            bit_count: u16::from_le_bytes([e[6], e[7]]),
            data: &raw[off..off + bytes],
        });
    }
    Some(out)
}

fn u16le(v: u16) -> [u8; 2] {
    v.to_le_bytes()
}

fn u32le(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

/// 一个资源块：32 字节头 + 数据，数据尾部按 4 字节对齐。
///
/// 头布局（Microsoft RES 格式）：
///   DataSize(4) HeaderSize(4) Type(4) Name(4) DataVersion(4)
///   MemoryFlags(2) LanguageId(2) Version(4) Characteristics(4)
///
/// 两个容易踩的坑（都对照 `rc.exe` 的实际产物核过）：
///   * `Type` / `Name` 是整数 ID 时，**低 16 位固定 0xffff、ID 放在高 16 位**
///     （RT_ICON 3 ⇒ `0x0003ffff`，字节序是 `ff ff 03 00`），
///     否则 link.exe 把低位当字符串指针，报 LNK1136 invalid or corrupt。
///   * 文件头的 `DataSize` 填 **0**，不是文件长度 —— 填长度同样 LNK1136。
fn block(kind: u32, name: u32, data: &[u8], flags: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(32 + data.len() + 4);
    v.extend(u32le(data.len() as u32));
    v.extend(u32le(32)); // HeaderSize 恒为 0x20
    v.extend(u32le(0x0000_ffff | (kind << 16))); // 低字 0xffff ⇒ 整数 ID
    v.extend(u32le(0x0000_ffff | (name << 16)));
    v.extend(u32le(0)); // DataVersion
    v.extend(u16le(flags)); // MOVEABLE | PURE（| DISCARDABLE）
    v.extend(u16le(0x0409)); // LanguageId：en-US，与 rc.exe 默认一致
    v.extend(u32le(0)); // Version
    v.extend(u32le(0)); // Characteristics
    debug_assert_eq!(v.len(), 32);
    v.extend(data);
    while v.len() % 4 != 0 {
        v.push(0);
    }
    v
}

fn build_res(items: &[IcoItem<'_>]) -> Vec<u8> {
    let mut body = Vec::new();

    // RT_ICON：每张图一个，ID 从 1 起
    for (i, it) in items.iter().enumerate() {
        body.extend(block(RT_ICON, i as u32 + 1, it.data, 0x1010));
    }

    // RT_GROUP_ICON：ICONDIR + 每项 14 字节的 ICONDIRENTRY（指向上面的 ID）
    let mut g = Vec::new();
    g.extend(u16le(0)); // reserved
    g.extend(u16le(1)); // type = icon
    g.extend(u16le(items.len() as u16));
    for (i, it) in items.iter().enumerate() {
        g.push(it.w);
        g.push(it.h);
        g.push(it.color_count);
        g.push(0); // reserved
        g.extend(u16le(it.planes));
        g.extend(u16le(it.bit_count));
        g.extend(u32le(it.data.len() as u32));
        g.extend(u16le(i as u16 + 1)); // 对应 RT_ICON 的 ID
    }
    body.extend(block(RT_GROUP_ICON, 1, &g, 0x1030));

    // 文件头：DataSize 恒为 0，HeaderSize 32，随后 24 字节为
    // `ff ff 00 00 ff ff 00 00` + 16 个 0（rc.exe 的原样形态）
    let mut out = Vec::with_capacity(32 + body.len());
    out.extend(u32le(0));
    out.extend(u32le(32));
    out.extend([0xff, 0xff, 0x00, 0x00, 0xff, 0xff, 0x00, 0x00]);
    out.extend([0u8; 16]);
    out.extend(body);
    out
}
