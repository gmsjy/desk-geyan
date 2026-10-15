// 桌面不要控制台窗口：release 下走 windows 子系统
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod bench;
mod clipboard;
mod config;
mod config_ui;
mod consts;
mod favorites;
mod ffi;
mod gfx;
mod layout;
mod png;
mod quotes;
mod transition;
mod util;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--selftest") {
        std::process::exit(app::selftest());
    }
    if args.iter().any(|a| a == "--probe") {
        std::process::exit(app::probe());
    }
    if args.iter().any(|a| a == "--dragtest") {
        std::process::exit(app::dragtest());
    }
    // 穿透的**行为**实测（会用真实光标，不是读样式位）
    if args.iter().any(|a| a == "--clicktest") {
        std::process::exit(app::clicktest());
    }
    // 收藏当前句子：**真的按一次 Ctrl+Alt+F**，再验剪贴板 / 收藏夹 / 幂等
    if args.iter().any(|a| a == "--favtest") {
        std::process::exit(app::favtest());
    }
    if args.iter().any(|a| a == "--quotes") {
        std::process::exit(quotes::audit());
    }
    if args.iter().any(|a| a == "--dragbench") {
        std::process::exit(app::dragbench());
    }
    if args.iter().any(|a| a == "--memtest") {
        std::process::exit(app::memtest());
    }
    // --autostart on|off|query：开机自启的查询 / 设置。
    // 除了给脚本用，也是**端到端验证通道**：设完要用独立工具（reg query）
    // 读回注册表对数，不能只信自己写进去的返回值。
    if let Some(i) = args.iter().position(|a| a == "--autostart") {
        match args.get(i + 1).map(|s| s.as_str()) {
            Some("on") => std::process::exit(if autostart::apply(true) { 0 } else { 4 }),
            Some("off") => std::process::exit(if autostart::apply(false) { 0 } else { 4 }),
            Some("query") => {
                println!("{}", if autostart::enabled() { "on" } else { "off" });
                std::process::exit(0);
            }
            _ => {
                println!("用法: desk-geyan --autostart on|off|query");
                std::process::exit(2);
            }
        }
    }
    if args.iter().any(|a| a == "--perf") {
        std::process::exit(app::perf());
    }
    // --dump <png> [key=value ...]：渲染一帧存成图片，可临时覆盖任意配置项
    if let Some(i) = args.iter().position(|a| a == "--dump") {
        let Some(path) = args.get(i + 1) else {
            println!("用法: desk-geyan --dump <输出.png> [key=value ...]");
            std::process::exit(2);
        };
        let overrides: Vec<(String, String)> = args[i + 2..]
            .iter()
            .filter_map(|s| s.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        std::process::exit(app::dump(path, &overrides));
    }
    std::process::exit(app::run());
}
