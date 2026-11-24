// 桌面不要控制台窗口：release 下走 windows 子系统
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod backup;
mod bench;
mod clipboard;
mod config;
mod config_ui;
mod consts;
mod favorites;
mod ffi;
mod gfx;
mod icon;
mod island;
mod layout;
mod png;
mod quotes;
mod tex;
mod transition;
mod util;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // 诊断追踪（`%TEMP%\desk-geyan-trace.log`）。做成命令行开关而不是只认环境变量：
    // 托盘/快捷方式启动的进程拿不到我们临时设的 `DG_TRACE`，而排查「点了没反应」
    // 这类问题恰恰要用**用户那条启动路径**复现 —— 两种入口都得能用。
    if args.iter().any(|a| a == "--trace") {
        util::trace_enable();
    }
    if args.iter().any(|a| a == "--selftest") {
        std::process::exit(app::selftest());
    }
    if args.iter().any(|a| a == "--probe") {
        std::process::exit(app::probe());
    }
    // 灵动岛端到端（建窗 / 端点几何 / 静止零出帧 / 开关不涨句柄 / visible=0 仍活）
    if args.iter().any(|a| a == "--islandtest") {
        std::process::exit(app::islandtest());
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
    // 图标资源：验 PE 里的 RT_GROUP_ICON 能取出、尺寸对、不是系统默认图标
    if args.iter().any(|a| a == "--icontest") {
        std::process::exit(app::icontest());
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
    // --notifprobe：读当前系统通知（不建窗、不读配置、零副作用）。
    // 排查「通知没显示」时先跑这一条 —— 能分清是采集不到还是画不出来。

    if args.iter().any(|a| a == "--notifprobe") {
        match island::feed::latest() {
            Some(n) => {
                println!("id    = {}", n.id);
                println!("app   = {}", n.app);
                println!("title = {}", n.title);
                println!("body  = {}", n.body);
                println!("line  = {}", n.line());
            }
            None => println!("(读不到通知：库不存在 / 打不开 / 没有记录)"),
        }
        std::process::exit(0);
    }
    // --mediaprobe：读当前媒体会话（同上，零副作用）。
    if args.iter().any(|a| a == "--mediaprobe") {
        match island::feed::media::current() {
            Some(m) => {
                println!("app    = {}", m.app);
                println!("title  = {}", m.title);
                println!("artist = {}", m.artist);
                println!("playing= {}", m.playing);
                println!("line   = {}", m.line());
            }
            None => println!("(没有媒体会话 / 拿不到)"),
        }
        std::process::exit(0);
    }
    // --dump-island <png> [key=value ...]：导出岛的一帧（透明底），供像素回归
    if let Some(i) = args.iter().position(|a| a == "--dump-island") {
        let Some(path) = args.get(i + 1) else {
            println!("用法: desk-geyan --dump-island <输出.png> [state=expanded|compact ...]");
            std::process::exit(2);
        };
        let overrides: Vec<(String, String)> = args[i + 2..]
            .iter()
            .filter_map(|s| s.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        std::process::exit(app::dump_island(path, &overrides));
    }
    // --cfgshot <目录>：把设置面板两页各导出一张 PNG（审查 / 宣传用的当前外观）
    if let Some(i) = args.iter().position(|a| a == "--cfgshot") {
        let Some(dir) = args.get(i + 1) else {
            println!("用法: desk-geyan --cfgshot <输出目录>");
            std::process::exit(2);
        };
        std::process::exit(app::cfgshot(dir));
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
    // --math [<png>]：把公式子集的样张画成图片（不启动挂件、不建窗口）。
    // 纯粹是公式层的验证入口 —— 排版对不对，看图和读数比看代码快。
    if let Some(i) = args.iter().position(|a| a == "--math") {
        let path = args
            .get(i + 1)
            .cloned()
            .unwrap_or_else(|| "math.svg.png".to_string());
        std::process::exit(app::mathdemo(&path));
    }
    // --mathquote "<一句话>" [前缀]：用真实渲染管线排一句带公式的格言，
    // 横竖两种朝向各出一张 PNG。验的是「接线」，不是公式盒子本身。
    if let Some(i) = args.iter().position(|a| a == "--mathquote") {
        let text = args
            .get(i + 1)
            .cloned()
            .unwrap_or_else(|| "质能关系 $E = mc^2$ 说明质量就是能量".to_string());
        let prefix = args
            .get(i + 2)
            .cloned()
            .unwrap_or_else(|| "mathquote".to_string());
        std::process::exit(app::mathquote(&text, &prefix));
    }
    std::process::exit(app::run());
}
