// 桌面不要控制台窗口：release 下走 windows 子系统
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod consts;
mod ffi;
mod gfx;
mod png;
mod quotes;
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
    if args.iter().any(|a| a == "--quotes") {
        std::process::exit(quotes::audit());
    }
    if args.iter().any(|a| a == "--dragbench") {
        std::process::exit(app::dragbench());
    }
    if args.iter().any(|a| a == "--memtest") {
        std::process::exit(app::memtest());
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
