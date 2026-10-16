//! 验证与基准：把「看起来对」变成「量过且会红」。
//!
//! 每个入口都是**可独立运行的 CLI**（`--selftest` / `--probe` / `--dump` ...），
//! 断言一律带可读的诊断串；新增断言必须先注毒变红才算数。
//!
//! 分三个文件：`selftest.rs`（64 条自检）、`bench.rs`（性能 / 拖动 / 内存标定）、
//! 本文件（探测、单帧导出、穿透与收藏的端到端实测）。

mod bench;
mod selftest;

// glob 不链式传递：`selftest.rs` / `bench.rs` 里要被 `app/mod.rs`
// 再导出的入口，必须在这里显式转一手。
pub(crate) use bench::{dragbench, memtest, perf};
pub(crate) use selftest::selftest;

use super::*;

use std::mem::{size_of, zeroed};
use std::ptr::null;
use windows_sys::Win32::Foundation::{POINT};
use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO, MonitorFromPoint};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetCapture, INPUT, INPUT_KEYBOARD, SendInput};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyWindow, GetWindowLongPtrW, IsWindowVisible, KillTimer};
use crate::{autostart, clipboard, favorites};
// 抽干消息队列。`--dragtest` 发的鼠标消息必须真的被派发到 wndproc，
// 而不是停在队列里当作「测试通过」。
//
// 这条和 `bench` 模块里那条是同一个函数 —— 测试夹具要发的消息就是它，
// 没道理让 wndproc 里的调试计数另起一套。
use crate::bench::{mouse_moves_at, press, pump, pump_wait, send_mouse, with_config_guard};
use crate::config::{self, Anim, Config, Mode};
use crate::config_ui;
use crate::ffi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDPIAware, SetProcessDpiAwarenessContext};
use crate::gfx;
use crate::quotes;
use crate::consts::*;
use crate::util::*;

// ---------------------------------------------------------------- 窗口探测

pub(crate) enum R {
    Pass,
    Fail,
    Info,
}

/// `desk-geyan --probe`：真的把窗口建出来，再**读回**扩展样式逐位验证。
///
/// 「不影响桌面其他东西使用」这件事必须在系统层面成立，光看代码不算数 ——
/// 这个模式就是把三道闸的样式位从内核读回来对数。
pub(crate) fn probe() -> i32 {
    // 探测会真的建窗口、真的写配置，所以必须零副作用 ——
    // 否则「测一下」就把用户摆好的位置带偏了。
    with_config_guard(probe_inner)
}

/// 把「设置面板管得着的那部分配置」压成一行文本。
///
/// 用来判定「点一下到底有没有改到东西」。**刻意不收 `x/y/w/h`** —— 那些是窗口几何，
/// 切换版式 / 归位时会跟着动；把它们算进来，断言就分不清「用户点的是这个控件」
/// 还是「别的什么顺手动了窗口」。
fn cfg_fingerprint(a: &App) -> String {
    let c = &a.cfg;
    format!(
        "clock={clock} date={date} quote={quote} shadow={shadow} sec={sec} top={top} \
         autohide={autohide} layout={layout} shrink={shrink} mode={mode} anim={anim} \
         orient={orient} op={op} spd={spd} scale={scale} cs={cs} qs={qs} sp={sp} \
         col={col:06X} dcol={dcol:06X} qcol={qcol:06X} font={font}",
        clock = c.show_clock,
        date = c.show_date,
        quote = c.show_quote,
        shadow = c.shadow,
        sec = c.seconds,
        top = c.always_on_top,
        autohide = c.auto_hide_fs,
        layout = c.layout_mode,
        shrink = c.shrink_fit,
        mode = c.mode.label(),
        anim = c.anim.key(),
        orient = c.orient.key(),
        op = c.opacity,
        spd = c.speed,
        scale = c.scale,
        cs = c.clock_scale,
        qs = c.quote_scale,
        sp = c.letter_spacing,
        col = c.color,
        dcol = c.date_color,
        qcol = c.quote_color,
        font = c.font,
    )
}

fn probe_inner() -> i32 {
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }
        let Some(mut st) = App::new() else {
            println!("FAIL  初始化失败");
            return 3;
        };
        st.hinst = GetModuleHandleW(null());
        // 探测必须可复现：不能因为用户把配置存成了「纯文字模式」，
        // 毛玻璃这条就静默变成 INFO。显式固定到毛玻璃路径再测。
        st.cfg = Config::for_test();
        // 字体必须跟着基线配置重建 —— 理由见 `Config::for_test()` 的文档
        // （不重建的话，用户那个字体决定时钟块多高，进而决定命中判定认不认得出「整窗拖动」）
        st.rebuild_fonts();
        // 但**建窗之前**要先摆成纯文字模式 —— 这样 boot 之后底板的可见性
        // 才有硬判据，而且正好覆盖「底板被无条件显示出来」那个真出现过的 bug
        // （它是「看不见却拦鼠标」，必须在建窗后**立刻**看，中间不能夹 apply_mode，
        // 否则 apply_glass 会把它抹掉，断言就永远绿了）。
        st.cfg.mode = Mode::TextOnly;
        st.cfg.layout_mode = false;
        let boxed = Box::new(st);
        app_ptr_set(Box::into_raw(boxed));
        let a = app();

        if !bootstrap(a) {
            println!("FAIL  窗口创建失败");
            return 2;
        }
        let boot_panel = IsWindowVisible(a.panel) != 0;
        let boot_text = IsWindowVisible(a.text) != 0;

        let ex_panel = GetWindowLongPtrW(a.panel, GWL_EXSTYLE) as u32;
        let ex_text = GetWindowLongPtrW(a.text, GWL_EXSTYLE) as u32;
        let bit = |ex: u32, flag: u32| ex & flag != 0;
        let both = |flag: u32| bit(ex_panel, flag) && bit(ex_text, flag);

        let mut rows: Vec<(&str, R, String)> = Vec::new();
        rows.push((
            "面板窗口（毛玻璃底板）",
            if a.panel.is_null() { R::Fail } else { R::Pass },
            format!("HWND {:p}", a.panel),
        ));
        rows.push((
            "文字窗口（per-pixel alpha）",
            if a.text.is_null() { R::Fail } else { R::Pass },
            format!("HWND {:p}", a.text),
        ));
        // ---- 底板可见性：建窗后**立刻**读，中间不许夹 apply_mode
        //
        // 补的是一个真出现过的漏洞：`bootstrap` 曾在 `apply_mode` 之后又无条件
        // `ShowWindow(a.panel)`，于是纯文字模式下底板被显示了出来。它是一片
        // **看不见却拦鼠标**的窗口（topmost + 非 layered ⇒ `WS_EX_TRANSPARENT`
        // 不生效），用户表现为「看不见任何东西却点不动底下的窗口」。
        // 光比样式位抓不到它 —— 必须比可见性，而且必须趁 apply_mode 还没再跑。
        rows.push((
            "建窗后底板未越权显形",
            if boot_panel { R::Fail } else { R::Pass },
            format!(
                "纯文字模式建窗后：底板 visible={} （须 0）/ 文字 visible={}",
                boot_panel as u8, boot_text as u8
            ),
        ));

        // 后面几条要看毛玻璃，切到毛玻璃路径再读
        a.cfg.mode = Mode::Glass;
        apply_mode(a);
        pump();

        // ---- 底板可见性：必须与「该不该显示」严格一致
        {
            let (was_mode, was_layout) = (a.cfg.mode, a.cfg.layout_mode);
            a.cfg.mode = Mode::TextOnly;
            a.cfg.layout_mode = false;
            apply_mode(a);
            pump();
            let want = a.should_show_panel();
            let got = IsWindowVisible(a.panel) != 0;
            rows.push((
                "底板只在该显示时才显示",
                if got == want { R::Pass } else { R::Fail },
                format!(
                    "纯文字模式：可见={} 应为={}（底板显形却非 layered 时会把点击全吃掉）",
                    got as u8, want as u8
                ),
            ));
            a.cfg.mode = was_mode;
            a.cfg.layout_mode = was_layout;
            apply_mode(a);
            pump();
        }

        // ---- 「挂件回到右上角」：菜单命令端到端（读回内核真实矩形）。
        // 这条曾经回归成空操作：处理器设好目标位置后错调了反向的
        // `sync_from_panel`（读窗口矩形覆写 cfg），目标立刻被旧位置改回去。
        // 断言必须打在窗口的真实位置上，cfg 自证无效（它可能根本没写进去）。
        {
            // MI_RESET 末尾会 `cfg.save()` —— 先把数据目录指到临时目录，
            // 别把测试位置写进用户的真实 config.ini。
            let dir = std::env::temp_dir().join(format!("desk-geyan-probe-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            std::env::set_var("DG_DATA_DIR", &dir);

            // 先把窗口真的挪到已知别处，保证「点菜单前不在右上角」
            a.cfg.x = 120;
            a.cfg.y = 400;
            apply_geometry(a, false, true);
            pump();
            let before = win_rect(a.panel);

            on_menu(a, MI_RESET);
            pump();
            let after = win_rect(a.panel);
            let (wa, _) = work_area_at(before.left, before.top, a.cfg.w);
            let want_x = wa.right - a.cfg.w - 24;
            let want_y = wa.top + 24;
            let err = (after.left - want_x).abs().max((after.top - want_y).abs());
            rows.push((
                "「挂件回到右上角」端到端",
                if err <= 2 { R::Pass } else { R::Fail },
                format!(
                    "点菜单前 ({},{}) → 菜单后窗口左上角 ({},{})，期望 ({},{}) · 偏差 {}px（≤2）",
                    before.left, before.top, after.left, after.top, want_x, want_y, err
                ),
            ));
        }
        rows.push((
            "文字窗口穿透位（仅样式位）",
            if bit(ex_text, WS_EX_TRANSPARENT) {
                R::Pass
            } else {
                R::Fail
            },
            "WS_EX_TRANSPARENT —— 样式位证明不了行为，行为用 --clicktest 实测".into(),
        ));
        rows.push((
            "永不抢焦点 → 不打断输入",
            if both(WS_EX_NOACTIVATE) {
                R::Pass
            } else {
                R::Fail
            },
            "WS_EX_NOACTIVATE".into(),
        ));
        rows.push((
            "不进任务栏 / Alt-Tab",
            if both(WS_EX_TOOLWINDOW) {
                R::Pass
            } else {
                R::Fail
            },
            "WS_EX_TOOLWINDOW".into(),
        ));
        rows.push((
            "文字窗口分层（ULW 前提）",
            if bit(ex_text, WS_EX_LAYERED) {
                R::Pass
            } else {
                R::Fail
            },
            "WS_EX_LAYERED".into(),
        ));
        rows.push((
            "总在最前",
            if a.cfg.always_on_top && both(WS_EX_TOPMOST) {
                R::Pass
            } else {
                R::Info
            },
            format!("always_on_top={}", a.cfg.always_on_top),
        ));
    rows.push((
        "全局热键",
        if a.hotkeys.len() >= HOTKEY_EXPECT {
            R::Pass
        } else if a.hotkey_err == ERROR_HOTKEY_ALREADY_EXISTS {
            // 被另一个实例占着：单实例互斥量保证正式运行不会互相抢，
            // 这里如实标 INFO，而不是判 FAIL 让整轮探测失去意义
            R::Info
        } else {
            R::Fail
        },
        format!(
            // 期望数从注册表本身推，不写死 —— 加一个热键就改一处常量的时代
            // 已经让这个文案变成过「18/17 注册成功」这种笑话。
            "{}/{} 注册成功{}",
            a.hotkeys.len(),
            HOTKEY_EXPECT,
                if a.hotkey_err == ERROR_HOTKEY_ALREADY_EXISTS {
                    "（其余被另一个实例占用：本机正跑着挂件）"
                } else if a.hotkey_err != 0 {
                    "（注册被系统拒绝，见 GetLastError）"
                } else {
                    ""
                }
            ),
        ));
        rows.push((
            "托盘图标",
            if a.tray_ok { R::Pass } else { R::Fail },
            if a.tray_ok {
                "已挂载".into()
            } else {
                format!(
                    "挂载失败 · GetLastError={}（0 = shell 未给出理由）",
                    a.tray_err
                )
            },
        ));
        // ---- 系统集成：开机自启 / 全屏检测。这两项不依赖窗口，
        // 放进来是为了「一次 --probe 覆盖整个桌面集成面」。
        {
            // 开机自启：判据不是「开没开」，而是「如果开着，必须指向当前 exe」。
            // exe 被挪窝后陈旧的启动项会指向一个不存在的文件 —— 用户以为
            // 设了自启，开机时静默失败，那比没开更坏。
            rows.push((
                "开机自启：无陈旧项",
                if autostart::is_stale() { R::Fail } else { R::Pass },
                if autostart::enabled() {
                    format!("已启用 · {}", autostart::detail())
                } else if autostart::is_stale() {
                    format!("⚠ 陈旧项：{}", autostart::detail())
                } else {
                    "未启用（托盘菜单可开）".into()
                },
            ));
            // 全屏检测 API：老系统上没这个函数时自动隐藏会静默不工作。
            // 必须真的去解析一次 —— 「代码里调了」不等于系统里有。
            let quns = crate::ffi::user_notification_state();
            rows.push((
                "全屏检测 API",
                if quns.is_some() { R::Pass } else { R::Fail },
                match quns {
                    Some(s) => format!(
                        "SHQueryUserNotificationState 可用 · 当前 QUNS={} ({})",
                        s,
                        quns_label(s)
                    ),
                    None => "shell32 无 SHQueryUserNotificationState（Vista 之前的系统？）".into(),
                },
            ));
        }
        rows.push((
            "毛玻璃 Acrylic",
            if a.accent_ok { R::Pass } else { R::Info },
            if a.accent_ok {
                "DWM Acrylic 已生效".into()
            } else {
                "未生效，已降级为「整窗 alpha + 自绘底板」".to_string()
            },
        ));
        // 把「为什么」也测出来，免得只看到一句「未生效」无从下手
        {
            let mut enabled: windows_sys::Win32::Foundation::BOOL = 0;
            let hr = windows_sys::Win32::Graphics::Dwm::DwmIsCompositionEnabled(&mut enabled);
            rows.push((
                "  └ DWM 桌面合成已启用",
                if hr == 0 && enabled != 0 { R::Pass } else { R::Info },
                format!("hr={:#x} enabled={}", hr, enabled),
            ));
            let ok4 = set_accent(a.panel, ACCENT_ENABLE_ACRYLICBLURBEHIND, 200);
            let ok3 = set_accent(a.panel, ACCENT_ENABLE_BLURBEHIND, 200);
            rows.push((
                "  └ 未文档化接口解析",
                if crate::ffi::accent_api_available() {
                    R::Pass
                } else {
                    R::Info
                },
                format!(
                    "SetWindowCompositionAttribute {}",
                    if crate::ffi::accent_api_available() {
                        "已从 user32.dll 取得"
                    } else {
                        "取不到（系统过旧）"
                    }
                ),
            ));
            rows.push((
                "  └ Accent 接口返回",
                if ok4 || ok3 { R::Pass } else { R::Info },
                format!("Acrylic(4)={} BlurBehind(3)={}", ok4, ok3),
            ));
        }
        rows.push((
            "配置已落盘",
            if Config::path().exists() {
                R::Pass
            } else {
                R::Fail
            },
            Config::path().display().to_string(),
        ));

        // ---- 布局模式：真的切过去，再读回样式位看穿透有没有被摘掉。
        // 「位置可拖动」的前提就是这一位。
        {
            let was = a.cfg.layout_mode;
            a.cfg.layout_mode = true;
            apply_mode(a);
            let ex_now = GetWindowLongPtrW(a.panel, GWL_EXSTYLE) as u32;
            rows.push((
                "布局模式 → 面板可点击（拖动的前提）",
                if !bit(ex_now, WS_EX_TRANSPARENT) {
                    R::Pass
                } else {
                    R::Fail
                },
                "WS_EX_TRANSPARENT 已摘除".into(),
            ));
            // 时间日期块的位置矩形必须在窗口内，否则命中判定会落空
            let inside = a.clock_rect.right > a.clock_rect.left
                && a.clock_rect.bottom > a.clock_rect.top
                && a.clock_rect.left >= 0
                && a.clock_rect.top >= 0
                && a.clock_rect.right <= a.cfg.w
                && a.clock_rect.bottom <= a.cfg.h;
            rows.push((
                "时间日期块可命中区域",
                if inside { R::Pass } else { R::Fail },
                format!(
                    "[{},{}]-[{},{}] in {}x{}",
                    a.clock_rect.left,
                    a.clock_rect.top,
                    a.clock_rect.right,
                    a.clock_rect.bottom,
                    a.cfg.w,
                    a.cfg.h
                ),
            ));
            // 块中心点应当判为命中，窗口外一点应当判为不命中
            let mid = (
                a.clock_rect.left + (a.clock_rect.right - a.clock_rect.left) / 2,
                a.clock_rect.top + (a.clock_rect.bottom - a.clock_rect.top) / 2,
            );
            let hit_mid = hit_clock_client(a, mid.0, mid.1);
            let hit_out = hit_clock_client(a, -500, -500);
            rows.push((
                "命中判定正确（块内命中 / 块外不命中）",
                if hit_mid && !hit_out { R::Pass } else { R::Fail },
                format!("块内={} 块外={}", hit_mid, hit_out),
            ));
            a.cfg.layout_mode = was;
            apply_mode(a);
        }

        // ---- 空转判定：没必要出帧就不能出帧
        //
        // 这一条防的是「静默变费电」：一帧 800µs，25fps 就是每秒 2% 的 CPU
        // 白烧在没有变化的画面上，而且每次空转都在跟拖动抢时间片。
        {
            let keep = (
                a.cfg.anim,
                a.cfg.show_clock,
                a.cfg.show_date,
                a.cfg.show_quote,
                a.cfg.seconds,
            );
            // ① 静态 + 时间日期都关掉：画面从这一秒到下一秒完全一样
            a.cfg.anim = Anim::Static;
            a.cfg.show_clock = false;
            a.cfg.show_date = false;
            let o = a.text_alpha();
            build_frame(a, o);
            let idle_ok = !needs_frame(a);
            // ② 显示秒的时钟：刻度走一格就必须出帧。
            //    用「把上次记录的刻度改掉」来模拟时间前进一秒，不必真等一秒。
            a.cfg.show_clock = true;
            a.cfg.seconds = true;
            let o = a.text_alpha();
            build_frame(a, o);
            let fresh_ok = !needs_frame(a);
            a.clock_stamp = a.clock_stamp.wrapping_sub(1);
            let tick_ok = needs_frame(a);
            // ③ 跑马灯在跑：每一拍都得画
            a.cfg.anim = Anim::Marquee;
            a.cfg.show_quote = true;
            let anim_ok = needs_frame(a);
            (
                a.cfg.anim,
                a.cfg.show_clock,
                a.cfg.show_date,
                a.cfg.show_quote,
                a.cfg.seconds,
            ) = keep;
            rows.push((
                "空转判定（静止不出帧 / 刚出完不出 / 刻度变必出 / 有动效必出）",
                if idle_ok && fresh_ok && tick_ok && anim_ok {
                    R::Pass
                } else {
                    R::Fail
                },
                format!(
                    "静止={} 刚出完={} 刻度变={} 有动效={}",
                    idle_ok, fresh_ok, tick_ok, anim_ok
                ),
            ));
        }

        // ---- 多显示器基准（P1）
        //
        // 判据是「夹取所用屏 == 挂件所在屏」。**单屏机上这条恒真** ——
        // 挂件只在主屏上时，按位置取的屏就是主屏，与旧的 `SPI_GETWORKAREA`
        // 行为完全一致，所以它证明的是「新代码路径活着且自洽」，不是
        // 「多屏下一定对」。真正会红的场景是接上副屏之后：把挂件拖到副屏，
        // 若基准仍取自主屏，`clamp_keep_visible` 会把副屏上的窗口夹到主屏的
        // 坐标范围内，视觉上就是「窗口自己跳走了」。**这条要接副屏才是真验。**
        {
            let cx = a.cfg.x + a.cfg.w / 2;
            let cy = a.cfg.y + 8;
            let hmon = MonitorFromPoint(POINT { x: cx, y: cy }, MONITOR_DEFAULTTONEAREST);
            let mut mi: MONITORINFO = zeroed();
            mi.cbSize = size_of::<MONITORINFO>() as u32;
            let (ok, detail) = if !hmon.is_null() && GetMonitorInfoW(hmon, &mut mi) != 0 {
                let (work, full) = work_area_at(a.cfg.x, a.cfg.y, a.cfg.w);
                let same = rect_eq(&full, &mi.rcMonitor) && rect_eq(&work, &mi.rcWork);
                (
                    same,
                    format!(
                        "挂件({},{}) · 屏[{},{}→{},{}] 工作区[{},{}→{},{}] 一致={}",
                        a.cfg.x,
                        a.cfg.y,
                        mi.rcMonitor.left,
                        mi.rcMonitor.top,
                        mi.rcMonitor.right,
                        mi.rcMonitor.bottom,
                        work.left,
                        work.top,
                        work.right,
                        work.bottom,
                        same
                    ),
                )
            } else {
                (false, "MonitorFromPoint / GetMonitorInfoW 失败".to_string())
            };
            rows.push((
                "多显示器：夹取基准 = 挂件所在屏",
                if ok { R::Pass } else { R::Fail },
                detail,
            ));
        }

        // ---- 外观配置（信息行：这三项都是可以改的）
        rows.push((
            "外观：字体 / 字号 / 颜色",
            R::Info,
            format!(
                "{} · 整体{}% 时钟{}% 格言{}% · 主色{} 日期{}",
                a.cfg.font,
                a.cfg.scale,
                a.cfg.clock_scale,
                a.cfg.quote_scale,
                fmt_color(a.cfg.color),
                fmt_color(a.cfg.date_color)
            ),
        ));
        {
            let face = String::from_utf16_lossy(
                &a.fonts.face[..a.fonts.face.len().saturating_sub(1)],
            );
            // 逐个点名，不数常量：漏了哪一档就直接报出来。
            // `quote_fit` 两档和 `quote_rot` 是竖版与缩字回退的依赖 ——
            // 它们建失败会返回 NULL，而 GDI 用 NULL 字体是**静默**画回默认字体的
            // （不报错、不返回失败），所以只有在这里点得到名才守得住。
            let hs = [
                ("时钟", a.fonts.clock),
                ("小号时钟", a.fonts.clock_small),
                ("日期", a.fonts.date),
                ("格言", a.fonts.quote),
                ("提示", a.fonts.hint),
                ("格言85%", a.fonts.quote_fit[0]),
                ("格言70%", a.fonts.quote_fit[1]),
                ("竖排旋转", a.fonts.quote_rot),
            ];
            let missing: Vec<&str> = hs
                .iter()
                .filter(|(_, h)| h.is_null())
                .map(|(n, _)| *n)
                .collect();
            let ok = missing.is_empty();
            rows.push((
                "  └ 字体对象已按配置重建",
                if ok { R::Pass } else { R::Fail },
                if ok {
                    format!("字面「{}」· {} 个字体句柄全部有效", face, hs.len())
                } else {
                    format!("字面「{}」· 创建失败：{}", face, missing.join(" / "))
                },
            ));
        }

        // ---- 设置面板：真的能开、列表真的填好、关掉后句柄真的清掉
        //
        // 「代码里调了 open()」不算数 —— 枚举回调签名写错、控件样式不对、
        // 忘了 LB_ADDSTRING，都会让窗口照常弹出却一行字体都没有
        // （用户只会觉得「这个界面是坏的」）。所以三件事都读回来：
        // 窗口句柄非空、列表项数 > 0、关掉之后 is_open 变回 false。
        //
        // 再加一条：**读回真实 GDI 计数，开关两轮**。这个窗口为列表里每个字体名
        // 建了一个字体句柄（本机 67 个）+ 界面字体 + 预览字体 + 两个色刷，
        // `free_resources` 一旦漏掉一件，每开关一次就漏掉一件 —— 而 `--memtest`
        // 根本不碰配置窗口，抓不到它。
        //
        // 判据是「**第二轮 == 第一轮**」而不是「回到起点」：首轮实测 +5（22→27），
        // 而第二、三轮恒定不动 —— 那 5 个是首次创建主题化控件时 uxtheme/comctl32
        // 按**进程**缓存的一次性系统对象，不是我们建的、也不随开关累积。
        // 和 `--memtest` 先跑 5 轮预热是同一个道理：一次性成本 ≠ 泄漏。
        // 建 / 毁都是**同步**完成的（`CreateWindowExW` / `DestroyWindow` 内联派发
        // WM_CREATE / WM_DESTROY），所以这里不需要 pump 也能量准。
        {
            let was = a.config.is_open();
            let gdi = |a: &App| -> u32 {
                let _ = a;
                crate::ffi::gui_resources().map(|(g, _)| g).unwrap_or(0)
            };
            let g0 = gdi(a);
            config_ui::open(a);
            let opened = a.config.is_open();
            let n = a.config.list_len();
            config_ui::close(a);
            let closed = !a.config.is_open();
            let g1 = gdi(a);
            // 再来一轮：这一轮必须一点都不涨
            config_ui::open(a);
            config_ui::close(a);
            let g2 = gdi(a);

            let no_leak = g1 == 0 || g2 <= g1;
            rows.push((
                "设置面板：建窗 + 字体列表 + 开关两轮不涨句柄",
                if !was && opened && n > 0 && closed && no_leak {
                    R::Pass
                } else {
                    R::Fail
                },
                format!(
                    "建窗={} · 字体列表 {} 项 · 关闭后句柄已清={} · GDI {}→{}→{}（首轮一次性 +{}，第二轮 Δ{}）",
                    opened,
                    n,
                    closed,
                    g0,
                    g1,
                    g2,
                    g1.saturating_sub(g0),
                    g2 as i64 - g1 as i64
                ),
            ));
        }

        // ---- 连续换字体后，预览区不能出现「叠字」
        //
        // 用户报的现象：在列表里连着点几个字体，预览区的新字和旧字叠在一起。
        // 根因是重画预览区时用了 `InvalidateRect(..., bErase = false)` —— 不擦底，
        // 新字体直接盖在旧字体上，每换一次就多一层。
        //
        // 判据用**像素往返**，不去数代码里有没有写 `FillRect`：先记下「直接设成字体 A」
        // 的样子，再走 A→B→C→A，两张抓图必须逐像素相同。没清底的话，第二张里会叠着
        // B / C 的字形，差异是一大片而不是零。抓不回窗口时标 INFO —— 不假装验过。
        {
            config_ui::open(a);
            let n = a.config.list_len();
            if n >= 3 {
                let (i0, i1, i2) = (0usize, n / 3, (2 * n) / 3);
                let shot = |a: &App| {
                    std::thread::sleep(std::time::Duration::from_millis(60));
                    config_ui::capture_preview(a)
                };
                config_ui::test_select(a, i0);
                pump();
                let s1 = shot(a);
                config_ui::test_select(a, i1);
                pump();
                config_ui::test_select(a, i2);
                pump();
                config_ui::test_select(a, i0);
                pump();
                let s2 = shot(a);

                let (kind, detail) = match (&s1, &s2) {
                    (Some((p1, w, h)), Some((p2, _, _))) if p1.len() == p2.len() => {
                        let diff = p1.iter().zip(p2.iter()).filter(|(x, y)| x != y).count();
                        (
                            if diff == 0 { R::Pass } else { R::Fail },
                            format!("{}x{} 预览区 · A→B→C→A 往返后 {} px 不同（须 0）", w, h, diff),
                        )
                    }
                    _ => (R::Info, "预览区抓取失败（PrintWindow 不可用），未验证".to_string()),
                };
                rows.push(("设置面板·连续换字体后预览区不叠字", kind, detail));
            }
            config_ui::close(a);
        }

        // ---- 设置面板：控件是不是都接上了、点了是不是真的改到了东西
        //
        // 这是整个面板最要紧的一条断言。面板上每一个控件都是「壳」——
        // 真正干活的是 `on_menu()`。壳和里面的线一旦接错（`Ctl.cmd` 填错、
        // id 串了位置、忘了 `sync_panel`），症状是**界面上什么都没发生**，
        // 而代码里每一行都「看起来对」。
        //
        // 三条判据：
        //   ① 面板建出来的可点控件，命令码逐个等于表格里那份清单（建漏 / 建重都红）
        //   ② 每个设置控件点一下，`cfg` 必须变；能配对的反向再点一下必须回到原值
        //      （数值行 `−`/`+`、复选框「再点一次」）——「只生效一次」「点完回不去」
        //      都会在这里露出来
        //   ③ 点完之后面板显示的状态必须和 `cfg` 一致（`widget_mismatch`）
        //      —— 这条抓的是「改了没刷新」：复选框勾着、配置其实是关的
        //
        // 动作类控件（8 个）刻意不自动点：`MI_AUTOSTART` 会**写注册表**
        // （自动点它 = 偷偷改用户的开机项），`MI_FAV` 写收藏文件（由 `--favtest` 专门测），
        // 其余几个各有专门的用例。它们进不了断言也不能是「没人知道」——
        // 所以把跳过的条数报出来，和表格里那份清单对数。
        {
            let keep = a.cfg.clone();
            config_ui::open(a);

            let live = config_ui::interactive_cmds(a);
            let plan = config_ui::table_cmds();
            let first_diff = live
                .iter()
                .zip(plan.iter())
                .position(|(x, y)| x != y)
                .map(|i| i + 1)
                .unwrap_or(if live.len() == plan.len() { 0 } else { live.len().min(plan.len()) + 1 });
            rows.push((
                "设置面板·控件与表格一一对应",
                if live == plan { R::Pass } else { R::Fail },
                if live == plan {
                    format!(
                        "{} 个可点控件，命令码与表格完全一致（设置区共 {} 个控件）",
                        live.len(),
                        config_ui::ctl_count(a)
                    )
                } else {
                    format!(
                        "面板 {} 个 / 表格 {} 条 · 第 {} 个起不同",
                        live.len(),
                        plan.len(),
                        first_diff
                    )
                },
            ));

            // 建完就该「显示 == cfg」；这是底，后面的循环再逐次复查
            let mis0 = config_ui::widget_mismatch(a);
            rows.push((
                "设置面板·建好后显示与配置一致",
                if mis0.is_none() { R::Pass } else { R::Fail },
                mis0.clone().unwrap_or_else(|| "所有控件与 cfg 一致".to_string()),
            ));

            const ACTION: [u32; 8] = [
                MI_AUTOSTART,
                MI_FAV,
                MI_PREV,
                MI_NEXT,
                MI_CLOCK_RESET,
                MI_RELOAD,
                MI_RESET,
                MI_TOGGLE,
            ];

            let mut tested = 0usize;
            let mut skipped = 0usize;
            let mut paired = 0usize;
            let mut fails: Vec<String> = Vec::new();
            for cmd in &live {
                if ACTION.contains(cmd) {
                    skipped += 1;
                    continue;
                }
                let label = config_ui::cmd_label(*cmd);
                if config_ui::is_num_down(a, *cmd) {
                    // 数值行的「−」由同一行的「+」成对覆盖（见下），不单独点：
                    // 基线正好在下界上的项（格言字距 0）点「−」合法地什么都不改，
                    // 单独点它就没法区分「没接上」和「本来就在边界上」。
                    paired += 1;
                    continue;
                }
                let before = cfg_fingerprint(a);
                if !config_ui::test_click_cmd(a, *cmd) {
                    fails.push(format!("{}（#{}）找不到控件", label, cmd));
                    continue;
                }
                tested += 1;
                let mid = cfg_fingerprint(a);
                if config_ui::is_radio(a, *cmd) {
                    // 单选项：点它 = 「我要这一项」。本来就选着的话再点一次不算变化，
                    // 所以判据是「点完它必须是选中的那一个」，而不是「cfg 变了」；
                    // 也**不做往返**—— 单选项不是开关，再点一次不会回到上一个选项
                    // （那是另一个单选项的事，整轮结束后统一还原）。
                    if !config_ui::radio_selected(a, *cmd) {
                        fails.push(format!("{}（#{}）点了但没选中", label, cmd));
                    }
                } else {
                    if mid == before {
                        fails.push(format!("{}（#{}）点了 cfg 没变", label, cmd));
                    }
                    // 往返：数值行走反方向那条命令，复选框再点一次
                    if let Some(inv) = config_ui::inverse_cmd(a, *cmd) {
                        config_ui::test_click_cmd(a, inv);
                        if cfg_fingerprint(a) != before {
                            fails.push(format!(
                                "{}（#{}）再点一次（#{}）没回到原值",
                                label, cmd, inv
                            ));
                        }
                    }
                }
                if let Some(m) = config_ui::widget_mismatch(a) {
                    fails.push(format!("{}（#{}）点完显示不一致：{}", label, cmd, m));
                }
            }
            rows.push((
                "设置面板·每个控件都改得动、显示跟得上",
                if fails.is_empty() { R::Pass } else { R::Fail },
                if fails.is_empty() {
                    format!(
                        "点过 {} 个设置控件（cfg 有变化 + 往返还原 + 显示与 cfg 一致）；\
                         另 {} 个「−」由成对点击覆盖 · 跳过 {} 个动作项",
                        tested, paired, skipped
                    )
                } else {
                    format!("{} 项失败：{}", fails.len(), fails.join(" · "))
                },
            ));

            // ---- 色块必须真的跟着换色（像素级）
            //
            // 上面那些判据都在读 `cfg` / 控件状态，读不到「画出来的那个方块」。
            // 换色是循环的（7 个预设），所以判据是**往返**：点一下色块像素必须变，
            // 点满一圈回到同一个颜色后像素必须一模一样。不比对具体颜色值 ——
            // 那会把「PrintWindow 的通道顺序」这个无关事实写进断言里。
            if let Some(sw) = config_ui::swatch_rect(a, MI_COLOR) {
                let cx = sw.left + (sw.right - sw.left) / 2;
                let cy = sw.top + (sw.bottom - sw.top) / 2;
                let px = |a: &App| -> Option<u32> {
                    // `sync_panel` 只是把整窗标脏，WM_PAINT 要回到消息循环里才轮到它
                    // （WM_PAINT 是最低优先级的消息）。不 pump 就抓，读到的是**上一帧**
                    // 的合成结果 —— 那样这条断言永远绿，等于没测。
                    pump();
                    std::thread::sleep(std::time::Duration::from_millis(60));
                    let (p, w, _) = config_ui::capture_client(a)?;
                    let h = p.len() as i32 / w.max(1);
                    if cx < 0 || cy < 0 || cx >= w || cy >= h {
                        return None;
                    }
                    Some(p[(cy * w + cx) as usize])
                };
                let one = px(a);
                config_ui::test_click_cmd(a, MI_COLOR);
                let two = px(a);
                // 从「换过一次」这个状态再点满一圈（预设是个环），应当回到同一色
                for _ in 0..config::COLOR_PRESETS.len() {
                    config_ui::test_click_cmd(a, MI_COLOR);
                }
                let three = px(a);
                let (kind, detail) = match (one, two, three) {
                    (Some(p1), Some(p2), Some(p3)) => (
                        if p1 != p2 && p2 == p3 { R::Pass } else { R::Fail },
                        format!(
                            "色块中心像素：换色前 {:08X} → 换后 {:08X} → 绕一圈回来 {:08X}（须先变、再一致）",
                            p1, p2, p3
                        ),
                    ),
                    _ => (R::Info, "色块读不到（PrintWindow 不可用），未验证".to_string()),
                };
                rows.push(("设置面板·换色后色块真的重画", kind, detail));
            }

            // ---- 预览框是视口、不是漏勺：样例文字必须真的量得下
            //
            // 用户报过的现象之一就是预览区的字压到了右边的设置区上。
            // 修法是两层：句子改短（看得见的部分）+ `ETO_CLIPPED` 裁到框内（兜底）。
            // 这条断言守的是第一层 —— 只留裁剪不留断言的话，句子写长了画面上
            // 就是「字被切了一半」，而没有任何东西会报警。
            match config_ui::preview_overflow(a) {
                None => rows.push((
                    "设置面板·预览样例不超出预览框",
                    R::Pass,
                    "三行样例在当前字体下都装得下".to_string(),
                )),
                Some(m) => rows.push(("设置面板·预览样例不超出预览框", R::Fail, m)),
            }

            // 抓一张面板整图：`DG_CAPTURE_PANEL=<png 路径> desk-geyan --probe`
            //
            // README 里那张设置面板的截图就是这么来的 —— 不是手动截屏，而是
            // **可复现的产物**：改一行布局重新跑一次，新图自动出来，不会出现
            // 「文档里的图和三年前的代码对不上」这种事。
            if let Ok(path) = std::env::var("DG_CAPTURE_PANEL") {
                // 截图前把字体设成**产品默认那个**（`FONT_PRESETS[0]`）：
                // 上面几轮断言刚把字体换来换去（换字体正是「预览区不叠字」那条
                // 断言的靶子），不还原的话文档里那张图的字体是「测试顺手留下的」，
                // 看起来像产品默认字体是别的。这里刻意不用 `keep.font` ——
                // `keep` 是在这几轮之后才快照的。
                a.set_font(config::FONT_PRESETS[0]);
                config_ui::resync(a);
                // `PrintWindow` 读的是窗口**已经合成好的表面**，它不会替我们重走一遍
                // WM_PAINT —— `resync` 只是把窗口标脏。所以先让队列里的绘制落地
                // 再抓，否则抓到的还是改动之前那一帧（这条坑见 README 踩坑第 15 条）。
                pump();
                std::thread::sleep(std::time::Duration::from_millis(120));
                pump();
                let (kind, detail) = match config_ui::capture_client(a) {
                    Some((px, w, h)) if px.len() >= (w * h) as usize => {
                        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
                        for y in 0..h {
                            for x in 0..w {
                                let p = px[(y * w + x) as usize];
                                // DIB 是 BGRA（见 `gfx::CH_*` 那段），PNG 要 RGBA
                                rgba.extend_from_slice(&[
                                    ((p >> 16) & 0xFF) as u8,
                                    ((p >> 8) & 0xFF) as u8,
                                    (p & 0xFF) as u8,
                                    255,
                                ]);
                            }
                        }
                        match crate::png::write_png(&path, w as u32, h as u32, &rgba) {
                            Ok(()) => (R::Info, format!("已写入 {}（{}x{}）", path, w, h)),
                            Err(e) => (R::Fail, format!("写文件失败：{}", e)),
                        }
                    }
                    _ => (R::Info, "PrintWindow 不可用，未抓取".to_string()),
                };
                rows.push(("设置面板截图", kind, detail));
            }

            config_ui::close(a);
            // 把这一轮点出来的状态全部还回去 —— 断言不能改变被测对象
            a.cfg = keep;
            a.rebuild_fonts();
            apply_resize(a, a.cfg.x, a.cfg.y, a.cfg.w, a.cfg.h);
            apply_mode(a);
            render_and_present(a);
        }

        let mut pass = true;
        for (name, kind, detail) in &rows {
            let tag = match kind {
                R::Pass => "PASS",
                R::Fail => {
                    pass = false;
                    "FAIL"
                }
                R::Info => "INFO",
            };
            println!("[{}] {:<32} {}", tag, name, detail);
        }
        println!(
            "\n{}\n提示：本模式建完窗口即退出，不常驻。",
            if pass {
                "窗口行为全部符合预期"
            } else {
                "存在不符合预期项"
            }
        );

        // 收拾干净再走
        del_tray(a);
        unregister_hotkeys(a);
        DestroyWindow(a.text);
        DestroyWindow(a.panel);
        if pass {
            0
        } else {
            3
        }
    }
}

// ---------------------------------------------------------------- 单帧导出

/// `desk-geyan --dump <png> [key=value ...]`
///
/// 把当前配置下的一帧渲染出来，叠加在棋盘格上存成 PNG。
/// 用途有两个：
///   * 调颜色 / 字号 / 时间位置时，不用盯着跑马灯等它滚过来，直接看图；
///   * 渲染结果可以有客观留底，而不是只靠自检里的数字。
///
/// `key=value` 与配置文件同源（都走 `config::apply_kv`），
/// 所以 `--dump a.png color=#F5D98B clock_dx=200 scale=140` 就是「临时试一下这套外观」。
pub(crate) fn dump(path: &str, overrides: &[(String, String)]) -> i32 {
    let Some(mut a) = App::new() else {
        println!("FAIL  初始化失败");
        return 3;
    };
    for (k, v) in overrides {
        config::apply_kv(&mut a.cfg, k, v);
    }
    a.list = quotes::library();
    a.qi = 0;
    a.qn = 1 % a.list.len();
    // 重建字体会把跑马灯停到带内，导出的就是「格言完整可见」的一屏
    a.rebuild_fonts();
    // 过渡状态机显式落定：dump 的职责是「这个配置的**稳态**一帧」，
    // 不能跟着任何相位漂。`App::new` 出来本就是 Dwell，这里是防未来改动
    // 把「跑过半段」的状态带进 dump —— 像素级 A/B 的确定性全押在这。
    a.trans.settle();
    a.phase = 0.0;

    let o = a.text_alpha();
    build_frame(&mut a, o);

    let (w, h) = (a.frame.w, a.frame.h);
    let px = a.frame.out_pixels();
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let p = px[(y * w + x) as usize];
            let al = (p >> 24) & 0xFF;
            let (r, g, b) = ((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF);
            // 16px 棋盘格：透明区域一眼可辨，抗锯齿边缘看得清
            let bg = if ((x / 16) + (y / 16)) % 2 == 0 { 0xF2u32 } else { 0x9Eu32 };
            // 预乘色直接加背景的 (1-α) 就是合成结果，不需要反预乘
            let mix = |c: u32| -> u8 { (c + bg * (255 - al) / 255).min(255) as u8 };
            rgba.extend_from_slice(&[mix(r), mix(g), mix(b), 255]);
        }
    }

    match crate::png::write_png(path, w as u32, h as u32, &rgba) {
        Ok(()) => {
            println!(
                "已写入 {}  ({}x{}，叠加于棋盘格之上)\n外观：{} {}% · 时钟{}% 格言{}% · 主色{} 日期{} 格言{} · 时间位置 +{}/+{}\n版式：{} · 字距 {} · 动效 {}",
                path,
                w,
                h,
                a.cfg.font,
                a.cfg.scale,
                a.cfg.clock_scale,
                a.cfg.quote_scale,
                fmt_color(a.cfg.color),
                fmt_color(a.cfg.date_color),
                config::quote_color_label(a.cfg.quote_color),
                a.cfg.clock_dx,
                a.cfg.clock_dy,
                a.cfg.orient.label(),
                a.cfg.letter_spacing,
                a.cfg.eff_anim().label(),
            );
            0
        }
        Err(e) => {
            println!("FAIL  写文件失败: {}", e);
            3
        }
    }
}

// ---------------------------------------------------------------- 拖动链路端到端

// ---------------------------------------------------------------- 穿透行为实测

/// `desk-geyan --clicktest`：用**真实输入**验证「鼠标真能穿过挂件点到下面的窗口」。
///
/// 这是本项目唯一一条**行为级**的穿透断言，因为样式级的断言证明不了它：
/// `WS_EX_TRANSPARENT` 在**非分层**顶层窗口上不生效 —— 底板正是带着这个属性
/// 却把整块面积的点击全吃掉了，而 `--probe` 只读属性、于是全程 PASS。
/// （这条 bug 真发生过：纯文字模式下底板被无条件显示出来，用户看不见任何东西
/// 却点不动底下的窗口。）
///
/// 判据两条方向相反，缺一不可：
///   * **非布局模式** → 我们自己的窗口必须**一条鼠标移动都收不到**（该落到底下）
///   * **布局模式**   → 必须**收得到**（这时底板就是拖拽把手，收不到就拖不动）
///
/// 副作用：把光标挪到挂件上再挪回来（约 1 秒），**不发送任何点击**。
pub(crate) fn clicktest() -> i32 {
    with_config_guard(clicktest_inner)
}

fn clicktest_inner() -> i32 {
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }
        let Some(mut st) = App::new() else {
            println!("FAIL  初始化失败");
            return 3;
        };
        st.hinst = GetModuleHandleW(null());
        // 固定到「纯文字 + 非布局」：这是默认使用形态，也正是坏掉过的那一种。
        // 位置另选一处必在屏内的空地 —— 行为判据要靠真实光标，必须点得到。
        st.cfg = Config::for_test();
        // 字体必须跟着基线配置重建 —— 理由见 `Config::for_test()` 的文档
        // （不重建的话，用户那个字体决定时钟块多高，进而决定命中判定认不认得出「整窗拖动」）
        st.rebuild_fonts();
        st.cfg.mode = Mode::TextOnly;
        st.cfg.layout_mode = false;
        st.cfg.visible = true;
        st.cfg.x = 300;
        st.cfg.y = 300;
        st.cfg.w = 420;
        st.cfg.h = 140;
        let boxed = Box::new(st);
        app_ptr_set(Box::into_raw(boxed));
        let a = app();

        if !bootstrap(a) {
            println!("FAIL  窗口创建失败");
            return 2;
        }

        let mut rows: Vec<(&str, R, String)> = Vec::new();
        let (cx, cy) = (a.cfg.x + a.cfg.w / 2, a.cfg.y + a.cfg.h / 2);

        // ---- ⓪ 建窗后**立刻**看：纯文字模式下底板不许越权显形
        //
        // 必须在任何 `apply_mode` 之前读 —— 那正是这个 bug 的形态：
        // `bootstrap` 在 `apply_mode` 之后又无条件把底板显示了出来，
        // 而它是一片看不见却拦鼠标的窗口（topmost + 非 layered）。
        let boot_panel = IsWindowVisible(a.panel) != 0;
        let boot_text = IsWindowVisible(a.text) != 0;
        rows.push((
            "建窗后底板未越权显形",
            if boot_panel { R::Fail } else { R::Pass },
            format!(
                "纯文字模式：底板 visible={}（须 0）/ 文字 visible={}",
                boot_panel as u8, boot_text as u8
            ),
        ));

        // ---- ① 非布局模式：底板不该显形，且必须完全穿透
        a.cfg.layout_mode = false;
        apply_mode(a);
        pump_wait(60);
        let panel_shown = IsWindowVisible(a.panel) != 0;
        let text_shown = IsWindowVisible(a.text) != 0;
        let hits_through = mouse_moves_at(cx, cy);

        rows.push((
            "纯文字模式下底板已隐藏",
            if panel_shown { R::Fail } else { R::Pass },
            format!(
                "底板 visible={} / 文字 visible={}",
                panel_shown as u8, text_shown as u8
            ),
        ));
        rows.push((
            "非布局模式：点得到下面的窗口",
            if hits_through == 0 { R::Pass } else { R::Fail },
            format!("光标停在挂件正中，我们的窗口收到 {hits_through} 条移动（须 0）"),
        ));

        // ---- ② 布局模式：必须收得到，否则看不见也摸不着
        a.cfg.layout_mode = true;
        apply_mode(a);
        pump_wait(60);
        let hits_layout = mouse_moves_at(cx, cy);
        rows.push((
            "布局模式：鼠标能落在挂件自己身上",
            if hits_layout > 0 { R::Pass } else { R::Fail },
            format!("同一位置收到 {hits_layout} 条移动（须 >0）"),
        ));

        // ---- ③ 切回非布局：穿透必须立刻恢复（而不是一次性的）
        a.cfg.layout_mode = false;
        apply_mode(a);
        pump_wait(60);
        let hits_back = mouse_moves_at(cx, cy);
        rows.push((
            "切回非布局后立刻恢复穿透",
            if hits_back == 0 { R::Pass } else { R::Fail },
            format!("收到 {hits_back} 条移动（须 0）"),
        ));

        // ---- ④ **毛玻璃模式**下也必须穿透（这一条推翻了项目早期的错误结论）
        //
        // 早期 README 写着一条「硬约束」：毛玻璃要求底板非分层，而跨进程穿透要求
        // 分层，所以 `mode=glass` 时底板**必然**拦住那一块面积的鼠标，
        // 「已实测可行」的解法是把底板挂到分层宿主下做子窗口。
        //
        // 两句话都是错的：
        // * 把 `mode` 直接改成 `Glass` 跑本条断言，顶层非分层底板（Acrylic 正常
        //   生效）下**收到 0 条鼠标移动** —— 毛玻璃与穿透本来就可以同时成立。
        //   当初之所以看到「拦住鼠标」，真正原因是 `bootstrap` 在 `apply_mode`
        //   之后又无条件 `ShowWindow(panel)`，让本该隐藏的底板显了形
        //   （见 README「踩过的坑」第 7 条），归因却归到了架构约束上。
        // * 而「挂到宿主下做子窗口」这条路真的走不通：`SetWindowCompositionAttribute`
        //   在子窗口上直接返回 false（实测 `Acrylic(4)=false BlurBehind(3)=false`，
        //   与宿主是否分层、是否设 LWA 无关），毛玻璃会静默降级。
        //
        // 判据必须是**行为**（数我们自己的窗口收到几条鼠标移动），不能是样式位。
        a.cfg.mode = Mode::Glass;
        a.cfg.layout_mode = false;
        apply_mode(a);
        pump_wait(80);
        let glass_panel = IsWindowVisible(a.panel) != 0;
        let glass_panel_transparent =
            (GetWindowLongPtrW(a.panel, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT) != 0;
        let hits_glass = mouse_moves_at(cx, cy);
        rows.push((
            "毛玻璃模式：点得到下面的窗口",
            if hits_glass == 0 && glass_panel { R::Pass } else { R::Fail },
            format!(
                "底板 visible={}（毛玻璃显形须 1）· 底板 TRANSPARENT={} · 收到 {} 条移动（须 0）",
                glass_panel as u8, glass_panel_transparent, hits_glass
            ),
        ));

        // ---- ⑤ 毛玻璃 + 布局模式：要能拖，退回后要恢复穿透
        a.cfg.layout_mode = true;
        apply_mode(a);
        pump_wait(80);
        let hits_glass_layout = mouse_moves_at(cx, cy);
        rows.push((
            "毛玻璃 + 布局：拖得动",
            if hits_glass_layout > 0 { R::Pass } else { R::Fail },
            format!("同一位置收到 {} 条移动（须 >0）", hits_glass_layout),
        ));
        a.cfg.layout_mode = false;
        a.cfg.mode = Mode::TextOnly;
        apply_mode(a);
        pump_wait(80);
        let hits_glass_back = mouse_moves_at(cx, cy);
        rows.push((
            "毛玻璃退回纯文字：穿透恢复",
            if hits_glass_back == 0 { R::Pass } else { R::Fail },
            format!("收到 {} 条移动（须 0）", hits_glass_back),
        ));

        let mut pass = true;
        for (name, kind, detail) in &rows {
            let tag = match kind {
                R::Pass => "PASS",
                R::Fail => {
                    pass = false;
                    "FAIL"
                }
                R::Info => "INFO",
            };
            println!("[{}] {:<32} {}", tag, name, detail);
        }
        println!(
            "\n{}\n（本模式会临时挪动光标并复原，不发送任何点击。）",
            if pass {
                "鼠标穿透行为全部符合预期"
            } else {
                "存在不符合预期项"
            }
        );

        del_tray(a);
        unregister_hotkeys(a);
        DestroyWindow(a.text);
        DestroyWindow(a.panel);
        if pass {
            0
        } else {
            3
        }
    }
}

/// 用**真实键盘输入**按一次 `Ctrl+Alt+F`。
///
/// 和 `--clicktest` 的鼠标注入同一个道理：`PostMessage(WM_HOTKEY)` 也能让
/// wndproc 跑一遍，但那跳过的是**系统热键分发**这一环 —— 而「注册了热键
/// 却按不出来」恰恰是这个功能最常见的坏法（被别的进程占着、修饰键组合
/// 写错、注册在了一个收不到消息的窗口上）。要验就得走完整链路。
///
/// `SendInput` 是异步的：消息由系统线程投递，所以注入完必须再跑一会儿消息泵。
unsafe fn press_favorite_hotkey() {
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    let keys: [(u16, u32); 6] = [
        (VK_CONTROL as u16, 0),
        (VK_MENU as u16, 0),
        (VK_F as u16, 0),
        (VK_F as u16, KEYEVENTF_KEYUP),
        (VK_MENU as u16, KEYEVENTF_KEYUP),
        (VK_CONTROL as u16, KEYEVENTF_KEYUP),
    ];
    let mut inputs: [INPUT; 6] = [zeroed(); 6];
    for (i, (vk, flags)) in keys.iter().enumerate() {
        inputs[i].r#type = INPUT_KEYBOARD;
        inputs[i].Anonymous.ki.wVk = *vk;
        inputs[i].Anonymous.ki.dwFlags = *flags;
    }
    SendInput(6, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32);
}

/// `desk-geyan --favtest`：**端到端**验证「收藏当前句子」。
///
/// 这是项目里唯一一条真的去按热键的测试。分三件事验，缺一不可：
///
/// 1. **剪贴板**里是当前那一句（写进去 ≠ 写对了，还要读回来逐字比）
/// 2. **收藏夹文件**里多了这一行，而且格式能被解析器读回原样
/// 3. **再按一次不会写第二遍**（幂等）—— 否则同一个组里出现同一内容，
///    轮播会连着播两句一样的
///
/// 副作用与处理：
/// * 剪贴板：**测前读回、测后恢复**（仅文本；原内容非文本时如实报告）
/// * 收藏夹：指向**临时目录**，完全不碰 `%APPDATA%` —— 和
///   `quotes::library_from` 同一个理由（测试不能污染用户的格言库）
/// * 配置：`with_config_guard` 快照写回
/// * 键盘：注入的是 `Ctrl+Alt+F`，**不发送任何字符**，不会打进别的窗口
///
/// **环境前提**：本进程必须真的是 `Ctrl+Alt+F` 的持有者。本机若已跑着另一个
/// 挂件实例，热键会被它占着（`RegisterHotKey` 报 1409），注入的按键归它处理，
/// 这边什么都测不到 —— 此时四条依赖热键的断言如实标 `INFO`（不是 `FAIL`），
/// 并在文案里说明原因。想跑出真正的 `PASS`，先退出正在运行的挂件实例。
pub(crate) fn favtest() -> i32 {
    with_config_guard(favtest_inner)
}

fn favtest_inner() -> i32 {
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }
        let Some(mut st) = App::new() else {
            println!("FAIL  初始化失败");
            return 3;
        };
        st.hinst = GetModuleHandleW(null());
        // 用**确定的**测试配置，不受用户当前配置影响（`Config::for_test` 的理由）
        st.cfg = Config::for_test();
        // 字体必须跟着基线配置重建 —— 理由见 `Config::for_test()` 的文档
        // （不重建的话，用户那个字体决定时钟块多高，进而决定命中判定认不认得出「整窗拖动」）
        st.rebuild_fonts();
        st.cfg.x = 300;
        st.cfg.y = 300;
        let boxed = Box::new(st);
        app_ptr_set(Box::into_raw(boxed));
        let a = app();

        if !bootstrap(a) {
            println!("FAIL  窗口创建失败");
            return 2;
        }

        // 这一轮注入的 `Ctrl+Alt+F`，到底会不会被**我们**收到？如果本机已经跑着
        // 另一个挂件实例，`RegisterHotKey` 会失败（系统错误码 1409），那个实例
        // 会把按键收走 —— 我们这边剪贴板、收藏夹都不会动，但这**不是回归**。
        // `--probe` 的「全局热键」一条早就按同一个判据报 INFO 了，这里沿用：
        // 「测不了」和「坏了」必须分开，否则一条环境相关的结果会被读成产品坏了。
        let fav_ours = a.hotkeys.contains(&HK_FAV);
        let not_ours = if fav_ours {
            String::new()
        } else {
            format!(
                "（HK_FAV 不在本进程名下：错误码 {} ⇒ 注入的热键被别人收走，\
                 本轮测不了；先退出正在运行的挂件实例再跑）",
                a.hotkey_err
            )
        };

        // 收藏走临时目录：测试绝不能往用户的格言库里写东西。
        // `favorite()` 内部走 `util::data_dir()`，所以这里用 `DG_DATA_DIR`
        // 把它整体指到临时目录 —— 这也是这个覆盖存在的第一个理由。
        let dir = std::env::temp_dir().join(format!("desk-geyan-favtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("DG_DATA_DIR", &dir);
        // 放一条已知句子进去，让「当前句」是确定的 —— 用户机器上
        // quotes.txt 的第一条是什么，测试不该依赖。
        std::fs::write(
            dir.join("quotes.txt"),
            "农，天下之本，务莫大焉。 —— 《史记》\n",
        )
        .unwrap();

        let mut rows: Vec<(&str, R, String)> = Vec::new();

        // 重载成这个临时库，并把 qi 固定到 0（第一条 = 上面那句）
        a.list = quotes::library_from(&dir);
        a.qi = 0;
        a.qn = 1 % a.list.len();
        a.cur_w = measure_quote(a, &a.list[a.qi]);
        let expect_display = a.list[a.qi].display();
        let expect_line = favorites::line_of(&a.list[a.qi]);

        // 剪贴板备份（仅文本）
        let clip_before = clipboard::get_text(a.panel);

        // ---- ① 真的按一次 Ctrl+Alt+F
        press_favorite_hotkey();
        pump_wait(300);
        let clip_after = clipboard::get_text(a.panel);
        let fav_path = favorites::path(&dir);
        let fav_after = favorites::load(&dir);

        rows.push((
            "热键触发后剪贴板 = 当前句子",
            if !fav_ours {
                R::Info
            } else if clip_after.as_deref() == Some(expect_display.as_str()) {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "期望「{}」· 实得「{}」{}",
                expect_display,
                clip_after.as_deref().unwrap_or("<空/非文本>"),
                not_ours
            ),
        ));
        rows.push((
            "热键触发后收藏夹多了一行",
            if !fav_ours {
                R::Info
            } else if fav_after.len() == 1
                && favorites::line_of(&fav_after[0]) == expect_line
                && fav_path.exists()
            {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "{} → {} 条（须 1）：{}{}",
                fav_path.display(),
                fav_after.len(),
                expect_line,
                not_ours
            ),
        ));

        // ---- ② 再按一次：幂等。这一条防的是「收藏夹里同一个组出现同一内容」，
        // 轮播时会连着播两句一样的。
        press_favorite_hotkey();
        pump_wait(300);
        let fav_twice = favorites::load(&dir);
        rows.push((
            "重复触发不写第二遍（幂等）",
            if !fav_ours {
                R::Info
            } else if fav_twice.len() == 1 {
                R::Pass
            } else {
                R::Fail
            },
            format!("再按一次后 {} 条（须 1）{}", fav_twice.len(), not_ours),
        ));

        // ---- ③ qi 仍指着同一句（画面不许自己跳）
        rows.push((
            "收藏后当前句不变（画面不跳）",
            if a.qi < a.list.len() && a.list[a.qi].display() == expect_display {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "qi={} →「{}」",
                a.qi,
                a.list.get(a.qi).map(|q| q.display()).unwrap_or_default()
            ),
        ));

        // ---- ④ 收藏的这句真的进了轮播候选（= 「加入播放」）
        // 判据：重载后的库里能按文本找到它，且收藏夹作为独立一组存在。
        let list_after = quotes::library_from(&dir);
        let in_rotation = list_after.iter().any(|q| q.display() == expect_display);
        let groups = quotes::load_groups(&dir);
        let has_fav_group = groups.iter().any(|(n, _)| n == favorites::FILE);
        rows.push((
            "收藏后进入轮播候选",
            if !fav_ours {
                R::Info
            } else if in_rotation && has_fav_group {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "库内可找到={} · 独立分组 {}={} · 总库 {} 条{}",
                in_rotation,
                favorites::FILE,
                has_fav_group,
                list_after.len(),
                not_ours
            ),
        ));

        // 剪贴板恢复
        let (restored, note) = match &clip_before {
            Some(s) => (clipboard::copy_text(a.panel, s), "原内容为文本，已原样恢复"),
            None => (true, "原内容非文本或为空，无法恢复（如实报告）"),
        };
        rows.push((
            "剪贴板原内容恢复",
            if restored { R::Pass } else { R::Fail },
            format!("{} · 恢复 {}", note, if restored { "完成" } else { "失败" }),
        ));

        unregister_hotkeys(a);
        DestroyWindow(a.text);
        DestroyWindow(a.panel);
        let _ = std::fs::remove_dir_all(&dir);

        let mut pass = true;
        println!("\n收藏功能端到端实测（真实键盘 Ctrl+Alt+F）");
        for (name, r, detail) in &rows {
            let tag = match r {
                R::Pass => "PASS",
                R::Fail => {
                    pass = false;
                    "FAIL"
                }
                R::Info => "INFO",
            };
            println!("[{}] {:<34} {}", tag, name, detail);
        }
        println!(
            "\n{}\n",
            if pass {
                if fav_ours {
                    "收藏链路全部符合预期"
                } else {
                    "热键不归本进程 ⇒ 依赖热键的条目未实测（退出正在运行的挂件实例可跑出真 PASS）"
                }
            } else {
                "存在不符合预期项"
            }
        );
        if pass {
            0
        } else {
            3
        }
    }
}

/// `desk-geyan --dragtest`：把一次完整的「按下 → 移动 → 松开」投进**真正的
/// 窗口过程**，验证拖动链路端到端成立。
///
/// 走 `PostMessage` 而不是注入系统级鼠标：一是不会打扰用户当前的鼠标操作，
/// 二是消息自带坐标，整条链路是可复现的。这也正是 wndproc 用 lparam 而不是
/// `GetCursorPos()` 的原因 —— 它让一次拖动成为自包含的事件序列。
pub(crate) fn dragtest() -> i32 {
    with_config_guard(dragtest_inner)
}

// ---------------------------------------------------------------- 资源 soak
//
// 老板的要求很直白：「运行不能内存泄漏，占用也不能扩大」。
// 光看 Private Bytes 不够 —— GDI 对象 / USER 对象 / 内核句柄各自有独立配额
// （GDI 默认 10000），泄漏的话先撞配额，表现是「画着画着就不刷新了」，
// 而不是内存爆掉。所以四样一起盯。

/// 一次资源采样。
#[derive(Clone, Copy, Default)]
pub(crate) struct Res {
    /// 私有提交内存（判断泄漏看这个，不看 Working Set —— 工作集会被系统随时回收）
    pub(crate) private: usize,
    pub(crate) working: usize,
    pub(crate) gdi: u32,
    pub(crate) user: u32,
    pub(crate) handles: u32,
}

pub(crate) fn sample() -> Res {
    let (private, working) = crate::ffi::process_memory().unwrap_or((0, 0));
    let (gdi, user) = crate::ffi::gui_resources().unwrap_or((0, 0));
    let handles = crate::ffi::handle_count().unwrap_or(0);
    Res {
        private,
        working,
        gdi,
        user,
        handles,
    }
}

/// 一次短拖动：按下 → n 步移动 → 松开。
///
/// 坐标给的是**客户区**坐标，位移量 = 坐标变化量（`raw = cfg + (cx − cx0)`），
/// 所以这里能精确控制窗口走多远。
unsafe fn drag_steps(a: &mut App, start: (i32, i32), dx: i32, dy: i32, n: i32) {
    send_mouse(a.panel, WM_LBUTTONDOWN, start.0, start.1);
    pump();
    for i in 1..=n {
        send_mouse(
            a.panel,
            WM_MOUSEMOVE,
            start.0 + dx * i / n,
            start.1 + dy * i / n,
        );
        pump();
    }
    send_mouse(a.panel, WM_LBUTTONUP, start.0 + dx, start.1 + dy);
    pump();
}

/// 跑一轮「什么都会碰到」的操作，把所有会**真的分配资源**的路径走一遍。
///
/// 这里刻意不去只测内存：字体句柄（`rebuild_fonts` → `Fonts::destroy` +
/// `Fonts::build`）、帧缓冲（缩放 → 三条掩码 `Vec` + 两个 DIB 重分配）、
/// 窗口样式切换（`apply_mode`）才是最可能漏 release 的地方，
/// 而它们的泄漏首先体现在 GDI 对象数上。
pub(crate) unsafe fn soak_round(a: &mut App, round: usize) {
    let (w, h) = (a.cfg.w, a.cfg.h);
    let mid = (w / 2, h / 2);
    let corner = (w - 2, h - 2);
    let clock = (a.clock_rect.left + 4, a.clock_rect.top + 4);
    let even = round.is_multiple_of(2);

    // ① 整窗移动（来回），② 时间日期块，③ 右下角缩放（每次都重建帧缓冲）
    drag_steps(a, mid, if even { 40 } else { -40 }, 0, 4);
    drag_steps(a, clock, 20, 10, 3);
    drag_steps(a, corner, if even { 24 } else { -24 }, if even { 16 } else { -16 }, 3);

    // ④ 换字号 → 销毁并重建全部字体句柄
    a.cfg.scale = if even { 100 } else { 130 };
    a.rebuild_fonts();

    // ⑤ 颜色 / 时钟日期开关 / 动效 / 模式（每种都触发一次完整的重绘或样式切换）
    a.cfg.color = if even { 0xFFFFFF } else { 0xF5D98B };
    a.cfg.show_date = even;
    a.cfg.show_clock = even;
    a.cfg.anim = Anim::all()[round % 4];
    a.cfg.mode = if round.is_multiple_of(3) {
        Mode::TextOnly
    } else {
        Mode::Glass
    };
    apply_mode(a);

    // 复位，免得拖着拖着被工作区边界夹住
    a.cfg.x = 60;
    a.cfg.y = 60;
    apply_geometry(a, true, true);
}

pub(crate) fn fmt_mb(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1048576.0)
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

/// 灌一次「按下 → N 条移动 → 松开」进**真正的窗口过程**，返回每条移动消息的平均成本（µs）。
///
/// 起点取客户区里一个确定的位置，位移在 0/+1px 之间交替：既真的触发了移动/缩放，
/// 又不会一路跑到边界上被夹住（那会让后段消息的成本失真）。
/// 一次灌满再排空（`flood`）才是真实鼠标的行为：连续移动时消息是成串来的。
/// `moving` = 整窗移动。两种拖动的**真实光标轨迹不一样**，坐标构造必须跟着变：
///
/// * 整窗移动：窗口跟着光标跑，所以「光标相对窗口没动」才是真实情形 ——
///   客户区坐标**恒定**，处理第 i 条时窗口已经走了 i−1 px，算出来正好 +1。
/// * 缩放 / 拖时间日期块：窗口原点不动，光标是真的在往前爬 —— 客户区坐标**递增**。
///
/// 一开始这里两种都灌恒定坐标，于是缩放那档等于「光标一直没动、尺寸压根没变」，
/// 量出来的 23µs 是个假数（`SetWindowPos` 发现无事可做就秒返回）。
pub(crate) unsafe fn drag_batch(a: &mut App, start: (i32, i32), n: usize, flood: bool, moving: bool) -> f64 {
    let panel = a.panel;
    // 与旧实现同口径：每条都真的落一次窗口（关节流）。
    // 不关掉的话，一口气灌进去的消息会被节流吃掉 99%，量到的是
    // 「消息一次性到达」这个不存在的场景 —— 数字好看，但没意义。
    a.move_throttle = 0;
    a.move_way = MoveWay::Swp;
    send_mouse(panel, WM_LBUTTONDOWN, start.0, start.1);
    pump();
    let t = std::time::Instant::now();
    for i in 0..n {
        let cx = if moving {
            start.0 + 1
        } else {
            start.0 + 1 + i as i32
        };
        send_mouse(panel, WM_MOUSEMOVE, cx, start.1);
        if !flood {
            pump();
        }
    }
    if flood {
        pump();
    }
    let us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
    send_mouse(panel, WM_LBUTTONUP, start.0, start.1);
    pump();
    us
}

fn dragtest_inner() -> i32 {
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }
        let Some(mut st) = App::new() else {
            println!("FAIL  初始化失败");
            return 3;
        };
        st.hinst = GetModuleHandleW(null());
        st.cfg = Config::for_test();
        // 字体必须跟着基线配置重建 —— 理由见 `Config::for_test()` 的文档
        // （不重建的话，用户那个字体决定时钟块多高，进而决定命中判定认不认得出「整窗拖动」）
        st.rebuild_fonts();
        // 关掉位置节流：这一节验的是**拖动逻辑**（位移量、夹取、锚点重置、
        // 松手落盘），而不是节拍时序。而且这里灌消息是同步的、没有时间流逝，
        // 开着节流会把绝大多数移动直接吃掉 —— 那样量到的是「8ms 里能落几次」，
        // 跟逻辑对不对毫无关系。节流本身由 `--dragbench` 专门验。
        st.move_throttle = 0;
        let boxed = Box::new(st);
        app_ptr_set(Box::into_raw(boxed));
        let a = app();

        if !bootstrap(a) {
            println!("FAIL  窗口创建失败");
            return 2;
        }
        render_and_present(a); // 先出一帧，把 clock_rect 量出来

        let mut rows: Vec<(&str, R, String)> = Vec::new();
        let center = |a: &App| {
            (
                a.clock_rect.left + (a.clock_rect.right - a.clock_rect.left) / 2,
                a.clock_rect.top + (a.clock_rect.bottom - a.clock_rect.top) / 2,
            )
        };

        // ---- ① 非布局模式：拖动必须无效。
        // 这是「不影响桌面其他东西使用」在交互侧的另一半 ——
        // 平时鼠标是穿透的，状态机上也不该有任何入口。
        {
            a.cfg.layout_mode = false;
            a.cfg.clock_dx = 0;
            a.cfg.clock_dy = 0;
            render_and_present(a);
            let (bx, by) = center(a);
            press(a.panel, bx, by);
            pump();
            send_mouse(a.panel, WM_MOUSEMOVE, bx + 200, by + 30);
            pump();
            send_mouse(a.panel, WM_LBUTTONUP, bx + 200, by + 30);
            pump();
            rows.push((
                "非布局模式：拖动被忽略（防误触）",
                if a.cfg.clock_dx == 0 && a.cfg.clock_dy == 0 {
                    R::Pass
                } else {
                    R::Fail
                },
                format!("偏移仍为 {}/{}", a.cfg.clock_dx, a.cfg.clock_dy),
            ));
        }

        // ---- ② 布局模式：一次真实拖动
        a.cfg.layout_mode = true;
        apply_mode(a);
        render_and_present(a);
        let (bx, by) = center(a);
        let x0 = a.clock_rect.left;

        press(a.panel, bx, by);
        pump();
        // 分 10 步移动，模拟真实拖拽的连续过程
        for i in 1..=10 {
            send_mouse(a.panel, WM_MOUSEMOVE, bx + i * 20, by + i * 3);
            pump();
        }
        send_mouse(a.panel, WM_LBUTTONUP, bx + 200, by + 30);
        pump();

        let moved = a.clock_rect.left - x0;
        rows.push((
            "布局模式：拖动真的移动了时间日期块",
            if a.cfg.clock_dx == 200 && a.cfg.clock_dy == 30 && moved > 150 {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "光标 +200/+30 → 偏移 {}/{}, 块 x {} → {}（位移 {}）",
                a.cfg.clock_dx, a.cfg.clock_dy, x0, a.clock_rect.left, moved
            ),
        ));
        // 拖完必须把捕获还回去，否则整个桌面的鼠标都会被这个窗口吞掉
        rows.push((
            "释放鼠标捕获（不吞桌面鼠标）",
            if GetCapture().is_null() {
                R::Pass
            } else {
                R::Fail
            },
            format!("GetCapture = {:p}", GetCapture()),
        ));
        rows.push((
            "拖动结果已落盘",
            if std::fs::read_to_string(Config::path())
                .map(|t| t.contains("clock_dx=200"))
                .unwrap_or(false)
            {
                R::Pass
            } else {
                R::Fail
            },
            "config.ini 里 clock_dx=200".into(),
        ));

        // ---- ③ 往死里拖：必须被夹住，且**偏移回写**成夹取后的值
        let (bx, by) = center(a);
        press(a.panel, bx, by);
        pump();
        send_mouse(a.panel, WM_MOUSEMOVE, 6000, 6000);
        pump();
        send_mouse(a.panel, WM_LBUTTONUP, 6000, 6000);
        pump();
        let inside = a.clock_rect.right <= a.cfg.w && a.clock_rect.bottom <= a.cfg.h;
        rows.push((
            "拖出边界：夹回窗口内",
            if inside {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "光标 +6000 → 块 [{},{}]-[{},{}] in {}x{}",
                a.clock_rect.left,
                a.clock_rect.top,
                a.clock_rect.right,
                a.clock_rect.bottom,
                a.cfg.w,
                a.cfg.h
            ),
        ));
        // 夹取后偏移必须等于「实际落点 − 默认落点」，否则往回拖会空走
        let clamped_dx = a.cfg.clock_dx;
        press(a.panel, center(a).0, center(a).1);
        pump();
        send_mouse(a.panel, WM_MOUSEMOVE, center(a).0 - 100, center(a).1);
        pump();
        send_mouse(a.panel, WM_LBUTTONUP, 0, 0);
        pump();
        let immediately_moved = (clamped_dx - a.cfg.clock_dx) == 100;
        rows.push((
            "边界回拖立即跟手（无空走）",
            if immediately_moved {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "夹取到 {} 后往回拖 100px → 偏移 {}（位移正好 100）",
                clamped_dx, a.cfg.clock_dx
            ),
        ));

        // ---- ④ 整体移动：位置必须**即时**跟手，且不该产生任何整帧重绘
        //
        // 这次修延迟的核心断言。旧实现每个 WM_MOVE 都跑一遍整帧重绘
        // （实测 1089µs/条，而 1000Hz 鼠标每条消息的预算只有 1000µs ——
        // 卡在线上，消息就会在队列里越积越多，手感就是「粘」）。
        // 而移动只改窗口位置，内容的像素一个都没变，那些工作全是白花的。
        // 这里用**整帧重绘计数**把它钉住：位置变化不该产生任何渲染工作。
        a.cfg.clock_dx = 0;
        a.cfg.clock_dy = 0;
        // 读回像素做对比时不能让跑马灯跑：动效停住，两次抓图才只差位置
        a.cfg.anim = Anim::Static;
        render_and_present(a);
        KillTimer(a.text, TIMER_ID); // 定时器自己会出帧，先停掉才数得准

        let w0 = win_rect(a.panel);
        let mid = (a.cfg.w / 2, a.cfg.h / 2);
        let shot_before = gfx::capture_window(a.text, a.cfg.w, a.cfg.h);

        let renders0 = a.renders;
        press(a.panel, mid.0, mid.1);
        pump();
        send_mouse(a.panel, WM_MOUSEMOVE, mid.0 + 40, mid.1 + 25);
        pump();
        let first = win_rect(a.panel);
        let step = (first.left - w0.left, first.top - w0.top);
        for i in 1..40 {
            send_mouse(a.panel, WM_MOUSEMOVE, mid.0 + 40 + i % 2, mid.1 + 25 + i % 2);
            pump();
        }
        let renders_move = a.renders - renders0;
        let r1 = win_rect(a.panel);
        let text1 = win_rect(a.text);
        let shot_after = gfx::capture_window(a.text, a.cfg.w, a.cfg.h);
        send_mouse(a.panel, WM_LBUTTONUP, mid.0, mid.1);
        pump();

        rows.push((
            "整体移动：第一条消息就到位",
            if step == (40, 25) { R::Pass } else { R::Fail },
            format!("光标 +40/+25 → 窗口马上位移 {:?}", step),
        ));
        rows.push((
            "整体移动：41 条消息 0 次整帧重绘",
            if renders_move == 0 { R::Pass } else { R::Fail },
            format!(
                "重绘 {} 次（旧实现每条消息一次 = 41 次，每帧 ~800µs）",
                renders_move
            ),
        ));
        rows.push((
            "整体移动：文字层与底板不脱层",
            if r1.left == text1.left && r1.top == text1.top {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "底板 ({},{}) · 文字层 ({},{})",
                r1.left, r1.top, text1.left, text1.top
            ),
        ));
        rows.push((
            "整体移动：新位置已落盘",
            if std::fs::read_to_string(Config::path())
                .map(|t| t.contains(&format!("x={}", r1.left)))
                .unwrap_or(false)
            {
                R::Pass
            } else {
                R::Fail
            },
            format!("config.ini 里 x={}", r1.left),
        ));

        // 位置变了，画的东西必须一个像素都没动。分层窗口的像素归 DWM 持有，
        // 只能靠 PrintWindow 读回合成结果 —— 读不回就如实标 INFO，不假装验过。
        match (&shot_before, &shot_after) {
            (Some(b), Some(f)) if b.len() == f.len() => {
                let diff = b.iter().zip(f.iter()).filter(|(x, y)| x != y).count();
                rows.push((
                    "整体移动：图层内容逐像素未变",
                    if diff == 0 { R::Pass } else { R::Fail },
                    format!("{} px 不同 / {} px 总量", diff, b.len()),
                ));
            }
            _ => rows.push((
                "整体移动：图层内容逐像素未变",
                R::Info,
                "本环境 PrintWindow 读不回分层窗口，未验证".into(),
            )),
        }

        // ---- ⑤ 缩放：从右下角拉，宽和高都该跟着涨
        let rw0 = win_rect(a.panel);
        let corner = (a.cfg.w - 2, a.cfg.h - 2);
        press(a.panel, corner.0, corner.1);
        pump();
        send_mouse(a.panel, WM_MOUSEMOVE, corner.0 + 60, corner.1 + 20);
        pump();
        send_mouse(a.panel, WM_LBUTTONUP, corner.0 + 60, corner.1 + 20);
        pump();
        let rw1 = win_rect(a.panel);
        let grew = (rw1.right - rw1.left) == (rw0.right - rw0.left) + 60
            && (rw1.bottom - rw1.top) == (rw0.bottom - rw0.top) + 20;
        rows.push((
            "缩放：右下角拉出 +60/+20",
            if grew { R::Pass } else { R::Fail },
            format!(
                "{}x{} → {}x{}",
                rw0.right - rw0.left,
                rw0.bottom - rw0.top,
                rw1.right - rw1.left,
                rw1.bottom - rw1.top
            ),
        ));
        rows.push((
            "缩放：帧缓冲跟着重建",
            if a.frame.w == a.cfg.w && a.frame.h == a.cfg.h {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "frame {}x{} · cfg {}x{}",
                a.frame.w, a.frame.h, a.cfg.w, a.cfg.h
            ),
        ));
        rows.push((
            "缩放：松手即出最终帧（不留待重绘）",
            if !a.dirty { R::Pass } else { R::Fail },
            format!("dirty = {}", a.dirty),
        ));

        // ---- ⑥ 整体移动的边界保护：拖到天边也得留一部分在屏幕里
        let mid2 = (a.cfg.w / 2, a.cfg.h / 2);
        press(a.panel, mid2.0, mid2.1);
        pump();
        send_mouse(a.panel, WM_MOUSEMOVE, mid2.0 + 20000, mid2.1 + 20000);
        pump();
        let far = win_rect(a.panel);
        send_mouse(a.panel, WM_LBUTTONUP, mid2.0, mid2.1);
        pump();
        // 夹取判据必须按**窗口被拖到天边后的真实位置**取所在屏 ——
        // 这里正是 P1 的多显示器基准：挂件落在哪块屏，就按哪块屏的工作区夹。
        let (wa, _) = work_area_at(far.left, far.top, a.cfg.w);
        let kept = wa.right - far.left >= KEEP_VISIBLE && wa.bottom - far.top >= KEEP_VISIBLE;
        rows.push((
            "整体移动：拖出天边仍留可见部分",
            if kept { R::Pass } else { R::Fail },
            format!(
                "窗口 ({},{}) · 工作区右下 ({},{}) → 屏内还有 {}x{}",
                far.left,
                far.top,
                wa.right,
                wa.bottom,
                wa.right - far.left,
                wa.bottom - far.top
            ),
        ));
        // 夹住之后往回拖必须立刻跟手：判据和上面那条时钟的完全同源
        press(a.panel, mid2.0, mid2.1);
        pump();
        send_mouse(a.panel, WM_MOUSEMOVE, mid2.0 - 50, mid2.1);
        pump();
        let back = win_rect(a.panel).left;
        send_mouse(a.panel, WM_LBUTTONUP, mid2.0, mid2.1);
        pump();
        rows.push((
            "整体移动：夹取后回拖立即跟手",
            if back == far.left - 50 { R::Pass } else { R::Fail },
            format!("{} → {}（位移正好 -50）", far.left, back),
        ));

        // ---- ⑦ 拖时间日期块：状态每条消息都跟手，画面按节拍出
        //
        // 「收状态」和「出画面」解耦之后，这两个比值就是解耦的直接证据。
        // 分两种节奏量，因为它们对应两种真实场景：
        //   * 洪泛（高回报率鼠标的极端情况）：40 条在一瞬间到齐。
        //     正确行为是**合流** —— 中间帧全都可以不要，但最后必须落到最终位置。
        //   * 常规（125Hz 鼠标）：消息间隔 > 8ms，每一条都该得到自己的那一帧。
        a.cfg.clock_dx = 0;
        a.cfg.clock_dy = 0;
        render_and_present(a);
        let (ccx, ccy) = center(a);

        let r0 = a.renders;
        press(a.panel, ccx, ccy);
        pump();
        for i in 0..40 {
            send_mouse(a.panel, WM_MOUSEMOVE, ccx + i, ccy);
            pump();
        }
        let renders_burst = a.renders - r0;
        send_mouse(a.panel, WM_LBUTTONUP, ccx + 39, ccy);
        pump();
        let renders_total = a.renders - r0;
        rows.push((
            "拖时间日期块：偏移每条消息都即时更新",
            if a.cfg.clock_dx == 39 { R::Pass } else { R::Fail },
            format!("40 条消息 → 偏移 {}", a.cfg.clock_dx),
        ));
        rows.push((
            "拖时间日期块：40 条洪泛消息只出 1 帧",
            if renders_burst == 0 && renders_total == 1 {
                R::Pass
            } else {
                R::Fail
            },
            format!(
                "过程 {} 帧 + 松手 1 帧 = {} 帧（旧实现 40 帧 × 800µs）",
                renders_burst, renders_total
            ),
        ));

        // 常规节奏：每条消息间隔 12ms（> 8ms 节流窗口），每条都该有自己的帧
        let (ccx2, ccy2) = center(a);
        let r1 = a.renders;
        press(a.panel, ccx2, ccy2);
        pump();
        for i in 1..=5 {
            std::thread::sleep(std::time::Duration::from_millis(12));
            send_mouse(a.panel, WM_MOUSEMOVE, ccx2 - i, ccy2);
            pump();
        }
        let renders_paced = a.renders - r1;
        send_mouse(a.panel, WM_LBUTTONUP, ccx2 - 5, ccy2);
        pump();
        rows.push((
            "拖时间日期块：常规节奏每条都有帧",
            if renders_paced >= 5 { R::Pass } else { R::Fail },
            format!("5 条消息（间隔 12ms）→ 出 {} 帧", renders_paced),
        ));

        // ---- ⑧ 节流路径：拖动中允许欠、松手必须精确到位，且**途中不许漂**
        //
        // 这条必须用**真实光标轨迹**。真实鼠标拖动时窗口位置会落后于 `cfg`
        // （这正是节流的定义），而消息里的客户区坐标是**相对窗口真实位置**的 ——
        // 光标往前走、窗口没跟上，客户区坐标就会一路变大。
        //
        // 一开始这里灌的是**恒定**客户区坐标（注释写「相对当前窗口又前进 1px」），
        // 那个假设只在**关掉节流**时成立。开着节流时它恰好能把一类严重的累积误差
        // 伪装成正常的等差位移：窗口每收到一条消息，就把上一条「欠着的」位移又算一遍，
        // 误差按三角数放大 —— 用户看到的就是「鼠标一动，窗口乱飘」。
        {
            a.move_throttle = DRAG_MOVE_MS;
            // 先归位到屏幕中央：避开边界夹取对轨迹的干扰，也让「位移」这条断言干净
            a.cfg.x = 300;
            a.cfg.y = 300;
            apply_geometry(a, true, true);
            pump();

            let (bx, by) = (a.cfg.w / 2, a.cfg.h / 2);
            let w0 = win_rect(a.text);
            let (ox, oy) = (w0.left, w0.top);
            let s0 = (ox + bx, oy + by); // 按下时，光标的**屏幕**坐标

            press(a.panel, bx, by);
            pump();

            const STEPS: i32 = 40;
            let mut max_err = 0;
            for i in 1..=STEPS {
                let want = (s0.0 + i, s0.1 + i); // 光标在屏幕上匀速走
                // 客户区坐标 = 光标屏幕坐标 − **窗口真实位置**（不是 `cfg`）
                let r = win_rect(a.panel);
                send_mouse(a.panel, WM_MOUSEMOVE, want.0 - r.left, want.1 - r.top);
                pump();
                let got = target_pos();
                let exp = (ox + i, oy + i); // 窗口该去的地方 = 起点 + 光标位移
                max_err = max_err.max((got.0 - exp.0).abs().max((got.1 - exp.1).abs()));
            }
            let lagging = win_rect(a.text).left - ox;

            send_mouse(a.panel, WM_LBUTTONUP, 0, 0);
            pump();
            let settled = win_rect(a.text).left - ox;
            let saved = std::fs::read_to_string(Config::path())
                .map(|t| t.contains(&format!("x={}", ox + STEPS)))
                .unwrap_or(false);

            rows.push((
                "节流 + 真实轨迹：途中不漂、松手精确到位",
                if max_err == 0 && lagging < STEPS && settled == STEPS && saved {
                    R::Pass
                } else {
                    R::Fail
                },
                format!(
                    "光标走 {}px：途中 cfg 偏差峰值 {}px（须恒 0）· 拖动中窗口欠 {}px · 松手 {}/{} px · 落盘 {}",
                    STEPS,
                    max_err,
                    STEPS - lagging,
                    settled,
                    STEPS,
                    if saved { "一致" } else { "不一致" }
                ),
            ));
            a.move_throttle = 0;
        }

        // ---- ⑨ 命中带：哪儿拖动、哪儿缩放
        //
        // 自己接管鼠标之后，这一层映射错了的表现是「某条边拉不动」或
        // 「拖动从某处开始变成缩放」—— 只靠手感极难定位，所以逐点验。
        {
            let (w, h) = (a.cfg.w, a.cfg.h);
            let cases: [(&str, (i32, i32), isize); 9] = [
                ("左上角", (1, 1), HTTOPLEFT),
                ("上边", (w / 2, 1), HTTOP),
                ("右上角", (w - 1, 1), HTTOPRIGHT),
                ("左边", (1, h / 2), HTLEFT),
                ("中间", (w / 2, h / 2), HTCLIENT),
                ("右边", (w - 1, h / 2), HTRIGHT),
                ("左下角", (1, h - 1), HTBOTTOMLEFT),
                ("下边", (w / 2, h - 1), HTBOTTOM),
                ("右下角", (w - 1, h - 1), HTBOTTOMRIGHT),
            ];
            let bad: Vec<&str> = cases
                .iter()
                .filter(|(_, p, want)| edge_zone(a, p.0, p.1) != *want)
                .map(|(n, _, _)| *n)
                .collect();
            rows.push((
                "命中带：8 个方向 + 中间各归其位",
                if bad.is_empty() { R::Pass } else { R::Fail },
                format!(
                    "{}/9 正确（边宽 {}px）{}",
                    cases.len() - bad.len(),
                    (8 * a.cfg.scale / 100).max(6),
                    if bad.is_empty() {
                        String::new()
                    } else {
                        format!(" · 错配: {:?}", bad)
                    }
                ),
            ));
        }

        let mut pass = true;
        for (name, r, detail) in &rows {
            let tag = match r {
                R::Pass => "PASS",
                R::Fail => {
                    pass = false;
                    "FAIL"
                }
                R::Info => "INFO",
            };
            println!("[{}] {:<36} {}", tag, name, detail);
        }
        println!(
            "\n{}",
            if pass {
                "拖动链路端到端通过"
            } else {
                "存在失败项"
            }
        );

        del_tray(a);
        unregister_hotkeys(a);
        a.cfg.layout_mode = false;
        DestroyWindow(a.text);
        DestroyWindow(a.panel);
        if pass {
            0
        } else {
            3
        }
    }
}
