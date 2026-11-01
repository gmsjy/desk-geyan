//! 可写文件的诊断报告。
//!
//! release 构建使用 Windows 子系统，`--selftest` 等控制台输出在双击/脚本环境下
//! 并不可靠。这里把用户真正常用的现场信息落成普通文本，方便反馈问题时直接附上。

use super::*;

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;

use crate::{autostart, config, favorites};

/// 把当前运行现场写入 `<数据目录>/diagnostics.txt`。
pub(crate) fn export(a: &App) -> io::Result<PathBuf> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let now = local_time();
    let mut s = String::new();
    let _ = writeln!(s, "desk-geyan 诊断报告");
    let _ = writeln!(
        s,
        "时间: {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
    );
    let _ = writeln!(s, "版本: {}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(s, "平台: {} {}", std::env::consts::OS, std::env::consts::ARCH);
    let _ = writeln!(s, "数据目录: {}", dir.display());
    let _ = writeln!(s, "配置文件: {}", Config::path().display());
    let _ = writeln!(s, "exe: {}", autostart::exe_path());
    let _ = writeln!(
        s,
        "窗口: x={} y={} w={} h={} · 可见={} · 布局模式={}",
        a.cfg.x, a.cfg.y, a.cfg.w, a.cfg.h, a.cfg.visible, a.cfg.layout_mode
    );
    let _ = writeln!(
        s,
        "DPI: 当前={}% · 跟随={} · 有效字号={}%",
        a.cfg.dpi,
        a.cfg.dpi_auto,
        a.cfg.eff_scale_pct()
    );
    let _ = writeln!(
        s,
        "外观: 模式={} · 版式={} · 动效(生效)={} · 字体={} · 不透明度={}",
        a.cfg.mode.label(),
        a.cfg.orient.key(),
        a.eff_anim().key(),
        a.cfg.font,
        a.cfg.opacity
    );
    let _ = writeln!(
        s,
        "播放: 暂停={} · 仅收藏夹={} · 当前索引={}/{}",
        a.cfg.paused,
        a.cfg.fav_only,
        a.qi,
        a.list.len()
    );
    let _ = writeln!(
        s,
        "全屏守卫: 启用={} · 当前全屏={} · 因全屏隐藏={}",
        a.cfg.auto_hide_fs, a.fs_active, a.fs_hidden
    );
    let _ = writeln!(
        s,
        "资源: 毛玻璃={:?} · Acrylic={} · 托盘={} · 热键 {}/{}",
        a.glass,
        a.accent_ok,
        a.tray_ok,
        a.hotkeys.len(),
        HOTKEY_EXPECT
    );
    let _ = writeln!(
        s,
        "错误码: 托盘={} · 热键={} · 自启={} · 消息泵={} · 单实例={}",
        a.tray_err,
        a.hotkey_err,
        a.autostart_err,
        pump_err(),
        single_err()
    );
    // 配置没写进去时界面一切都是「正常」的，用户只会在重启后发现设置丢了。
    // 这一行是唯一能指出「问题出在磁盘」的地方。
    match config::last_save_err() {
        0 => {
            let _ = writeln!(s, "配置写入: 上次保存成功");
        }
        code => {
            let _ = writeln!(
                s,
                "配置写入: **上次保存失败**（错误码 {}）· 设置面板里改的项重启后会丢失",
                code
            );
        }
    }
    // 逐列渐变降级：非 0 说明出现过「列数超出带容量」，画面会整块过渡
    // 而不是逐列。不是错误，但解释「为什么某次过渡看着不一样」。
    let fallback = band_fallback();
    if fallback > 0 {
        let _ = writeln!(
            s,
            "逐列渐变降级: {} 次（列数超出带容量 {}，已退化为整块过渡）",
            fallback,
            gfx::MAX_BANDS
        );
    }
    let _ = writeln!(
        s,
        "开机自启: 启用={} · 指向别处={} · last_err={} · {}",
        autostart::enabled(),
        autostart::is_stale(),
        autostart::last_err(),
        autostart::detail()
    );

    let groups = quotes::load_groups(&dir);
    let _ = writeln!(
        s,
        "词库: 已加载 {} 条 · 外部分组 {} 组 · 内置 {} 条",
        a.list.len(),
        groups.len(),
        quotes::builtin_len()
    );
    for (name, items) in &groups {
        let _ = writeln!(s, "  - {}: {} 条", name, items.len());
    }
    if a.cfg.fav_only && groups.iter().all(|(n, _)| n != favorites::FILE) {
        let _ = writeln!(s, "  注意: 已开启仅播放收藏夹，但收藏夹为空，当前回退到内置库");
    }

    let path = dir.join("diagnostics.txt");
    std::fs::write(&path, s)?;
    Ok(path)
}
