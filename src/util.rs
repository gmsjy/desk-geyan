//! 字符串 / 时间 / 路径的小工具。

use std::path::PathBuf;
use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

/// UTF-16 + NUL 结尾。所有 Win32 W 版 API 的入参都从这里来。
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `HH:mm:ss`
pub fn fmt_clock(t: &SYSTEMTIME, with_seconds: bool) -> String {
    if with_seconds {
        format!("{:02}:{:02}:{:02}", t.wHour, t.wMinute, t.wSecond)
    } else {
        format!("{:02}:{:02}", t.wHour, t.wMinute)
    }
}

/// `2026年10月13日 星期二`
pub fn fmt_date(t: &SYSTEMTIME) -> String {
    const WEEK: [&str; 7] = ["日", "一", "二", "三", "四", "五", "六"];
    let w = WEEK[(t.wDayOfWeek as usize).min(6)];
    format!("{}年{}月{}日 星期{}", t.wYear, t.wMonth, t.wDay, w)
}

pub fn local_time() -> SYSTEMTIME {
    unsafe {
        let mut st: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut st);
        st
    }
}

/// 数据目录：`%APPDATA%\DeskGeyan`，可用 `DG_DATA_DIR` 覆盖。
///
/// 覆盖不只是为了测试（`--favtest` 要能指向临时目录，否则测试会
/// **真的往用户的格言库里写收藏**）—— 对用户它也是便携模式：
/// U 盘 / 绿色版指一下就能把配置和格言库都带在身上。
///
/// 空字符串视为未设置：`set VAR=` 这种清空写法不该把程序指到当前目录。
pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("DG_DATA_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d);
        }
    }
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(base).join("DeskGeyan")
}

pub fn ensure_data_dir() -> PathBuf {
    let d = data_dir();
    let _ = std::fs::create_dir_all(&d);
    d
}

// ---------------------------------------------------------------- 颜色

/// 解析 `#RRGGBB` / `RRGGBB` / `0xRRGGBB` 为 **逻辑顺序** 的 `0x00RRGGBB`。
///
/// 注意这里刻意不做 `COLORREF` 的 BGR 交换：颜色只在最终预乘合成时
/// 按 R/G/B 分量参与乘加，不经过 GDI 的 `SetTextColor`，所以整条管线
/// 里只需要一种顺序，少一次换序就少一处出错的地方。
pub fn parse_color(s: &str) -> Option<u32> {
    let t = s.trim().trim_start_matches('#');
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    if t.len() != 6 {
        return None;
    }
    u32::from_str_radix(t, 16).ok().map(|c| c & 0x00FF_FFFF)
}

/// `0x00RRGGBB` → `#RRGGBB`
pub fn fmt_color(c: u32) -> String {
    format!("#{:06X}", c & 0x00FF_FFFF)
}

pub fn color_r(c: u32) -> u32 {
    (c >> 16) & 0xFF
}
pub fn color_g(c: u32) -> u32 {
    (c >> 8) & 0xFF
}
pub fn color_b(c: u32) -> u32 {
    c & 0xFF
}
