//! `--islandtest`：灵动岛自己的端到端验证。
//!
//! 不依赖真实鼠标：状态机的全部边界在 `geom::hover_decide` 的单测里已经覆盖，
//! 这里验的是**只有真窗口才能验的那部分**：
//!
//! * 窗口真的建出来了 / 关掉之后真的不存在（不是藏起来）；
//! * 端点几何与 DPI 缩放对得上；
//! * 静止时一帧都不出；
//! * **`cfg.visible = 0`（格言关掉）时岛照常工作** —— 这是本版最重要的
//!   一条结构性保证（规格书 §5.6），共用定时器的方案会在这里静默饿死；
//! * 反复开关不涨句柄。
//!
//! 与其它入口一样，跑完把 `config.ini` 原样写回（`with_config_guard`）。

use std::ptr::null;
use std::time::Instant;

use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW;

use super::*;
use crate::bench::with_config_guard;
use crate::island::content::Items;
use crate::island::geom::State;
use crate::island::{self, ISLAND_CLASS};

pub(crate) fn islandtest() -> i32 {
    with_config_guard(islandtest_inner)
}

fn row(ok: bool, name: &str, detail: String) -> bool {
    println!(
        "[{}] {:<34} {}",
        if ok { "PASS" } else { "FAIL" },
        name,
        detail
    );
    ok
}

fn window_exists() -> bool {
    unsafe {
        let cls = wide(ISLAND_CLASS);
        !FindWindowW(cls.as_ptr(), null()).is_null()
    }
}

fn islandtest_inner() -> i32 {
    unsafe {
        // 进程级 DPI 感知：和 `run()` 走同一条路，否则量出来的尺寸会被系统虚拟化
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }
        let Some(st) = App::new() else {
            println!("[FAIL] App 初始化失败（词库为空？）");
            return 1;
        };
        let mut st = st;
        st.hinst = GetModuleHandleW(null());
        // 测试起点必须是**确定的**：不读用户摆好的位置与开关
        st.cfg.placed = true;
        st.cfg.x = 60;
        st.cfg.y = 60;
        st.cfg.island = true;
        // ★ 关键组合：格言关掉、岛开着。共用 tick 的实现会在这里被饿死。
        st.cfg.visible = false;
        st.cfg.auto_hide_fs = false;
        st.cfg.island_state = false;
        // 自检不许搬用户的真实消息文件
        st.island.consume_events = false;
        app_ptr_set(Box::into_raw(Box::new(st)));
    }

    let mut all = true;

    // ⓪ `island=0` ⇒ 窗口不存在（规格书 §7.2 #1，强等式「不存在」）
    //
    // 单独放在最前面而不是复用 ⑦：⑦ 验的是「关掉之后」不存在，
    // 这条验的是「默认不建」。合成一条的话，`bootstrap` 里无条件
    // `create` 这种错会在 ⑦ 之前就把窗口建出来，而 ⑦ 照样通过。
    let no_window = with_app(|a| {
        a.cfg.island = false;
        island::apply_enabled(a);
        !a.island.on()
    })
    .unwrap_or(false);
    all &= row(
        no_window && !window_exists(),
        "⓪ island=0 时不存在岛窗口",
        format!(
            "apply_enabled 后 on()={} · FindWindow={}（均须 false）",
            !no_window,
            window_exists()
        ),
    );

    // ① 建窗
    let created = with_app(|a| unsafe { island::create(a) }).unwrap_or(false);
    all &= row(
        created && window_exists(),
        "① 建岛窗口",
        format!("create={created} · FindWindow={}", window_exists()),
    );

    // ② 收起端点几何
    let (rect, dpi, cw) = with_app(|a| {
        island::collapse(a);
        (a.island.rect, a.island.dpi, a.island.compact_w)
    })
    .unwrap_or_default();
    let k = dpi as f32 / 100.0;
    let want_h = (34.0 * k).round() as i32;
    all &= row(
        rect.h == want_h && rect.w >= (96.0 * k) as i32,
        "② 收起端点几何",
        format!(
            "{}x{} @ ({},{}) · 期望高 {} · 内容宽 {} · dpi {}",
            rect.w, rect.h, rect.x, rect.y, want_h, cw, dpi
        ),
    );

    // ③ 展开端点几何（宽度 = 配置宽 × DPI，且中心不动）
    let (r2, r3) = with_app(|a| {
        let before = a.island.rect;
        island::expand(a);
        (before, a.island.rect)
    })
    .unwrap_or_default();
    let wid = with_app(|a| a.cfg.island_w).unwrap_or(420);
    let want_w = ((wid as f32) * k).round() as i32;
    let center_ok = (r3.x + r3.w / 2 - (r2.x + r2.w / 2)).abs() <= 1;
    all &= row(
        r3.w == want_w && r3.h > r2.h && center_ok,
        "③ 展开端点几何（中心不动）",
        format!(
            "{}x{} → {}x{} · 期望宽 {} · 中心 {} vs {}",
            r2.w, r2.h, r3.w, r3.h, want_w,
            r2.x + r2.w / 2,
            r3.x + r3.w / 2
        ),
    );

    // ④ 两套内容相位不共存（真窗口上的同一条判据）
    let mut overlap = 0.0f32;
    for i in 0..=100 {
        let (ck, ek) = island::geom::phase_k(i as f32 / 100.0);
        overlap = overlap.max(ck * ek);
    }
    all &= row(
        overlap < 1e-9,
        "④ 内容相位不共存",
        format!("max(compact_k × expanded_k) = {overlap:.6}"),
    );

    // ⑤ 静止零出帧：清掉脏标记后连推 100 拍，`renders` 不该动
    let (before, after) = with_app(|a| {
        island::collapse(a);
        a.island.dirty = false;
        let b = a.island.renders;
        for _ in 0..100 {
            island::recompute(a);
            if a.island.needs_frame() {
                a.island.dirty = false;
                crate::island::draw::render(a);
            }
        }
        (b, a.island.renders)
    })
    .unwrap_or((0, 0));
    all &= row(
        after == before,
        "⑤ 收起态静止零出帧",
        format!("100 拍 renders {before} → {after}（须相等）"),
    );

    // ⑥ 静止后窗口尺寸仍等于端点（ULW 没把尺寸带偏）
    let r4 = with_app(|a| a.island.rect).unwrap_or_default();
    all &= row(
        r4.h == want_h,
        "⑥ 静止后几何未被改写",
        format!("{}x{} · 期望高 {}", r4.w, r4.h, want_h),
    );

    // ⑦ 关掉 = 真销毁（不是藏起来）
    with_app(|a| {
        a.cfg.island = false;
        island::apply_enabled(a);
    });
    all &= row(
        !window_exists(),
        "⑦ 关掉后窗口不存在",
        format!("FindWindow={}（须 false）", window_exists()),
    );

    // ⑧ 再开一次：窗口回来、句柄不涨
    let res0 = super::sample();
    let reopened = with_app(|a| {
        a.cfg.island = true;
        island::apply_enabled(a);
        let ok = a.island.on();
        a.cfg.island = false;
        island::apply_enabled(a);
        ok
    })
    .unwrap_or(false);
    let res1 = super::sample();
    let leaked = res1.gdi > res0.gdi || res1.user > res0.user || res1.handles > res0.handles;
    all &= row(
        reopened && !window_exists() && !leaked,
        "⑧ 开关一轮 · 句柄不涨",
        format!(
            "重开={reopened} · 关后不存在={} · GDI {}→{} · USER {}→{} · 句柄 {}→{}",
            !window_exists(),
            res0.gdi,
            res1.gdi,
            res0.user,
            res1.user,
            res0.handles,
            res1.handles
        ),
    );

    // ⑨ `visible=0` 时岛仍能被 tick 推进（结构上不依赖格言的定时器）
    //
    // 直接比「同一秒内推进 20 拍，races 不崩、状态机不动」——真正的判据是
    // 岛拿到了自己的 WM_TIMER，而它由自己的窗口持有时，格言窗口可见与否
    // 根本不在同一条路径上。这里验的是 `tick` 能在 `cfg.visible=0` 下跑完。
    let ticked = with_app(|a| {
        a.cfg.island = true;
        unsafe { island::create(a) };
        let mut ok = true;
        for _ in 0..20 {
            island::tick(a);
            if !a.island.on() {
                ok = false;
            }
        }
        // 收尾：关掉
        a.cfg.island = false;
        island::apply_enabled(a);
        ok
    })
    .unwrap_or(false);
    all &= row(
        ticked,
        "⑨ visible=0 时岛照常 tick",
        format!("20 拍后窗口仍在={ticked}（格言 visible=false）"),
    );

    // ---------------- 以下几步要一个活的窗口，①②③ 已经拆过一次了
    with_app(|a| {
        a.cfg.island = true;
        a.cfg.visible = false;
        a.cfg.opacity = 255; // 底下 ⑩ 要断言「中心 alpha=255」，先把它钉成恒等
        a.cfg.island_body_alpha = 255;
        unsafe { island::create(a) };
    });

    // ⑩ 形状：四角透明、中心不透明、胶囊底左右逐像素对称（§7.2 #3）
    //
    // 「左右对称」只对**胶囊底**成立（文字左对齐、日期右对齐，本来就不对称），
    // 所以取底部一条不可能有字的横带量 —— 那里只有形状。行内对称的
    // 算术理由是 `body_coverage_into` 里左右都用 `dx = r - (x+0.5)`，
    // `x' = w-1-x` 代进去 dx 完全相同。
    let (px, w, h, body_a) = with_app(|a| {
        island::expand(a);
        island::draw::render(a);
        let (p, w, h) = a.island.frame_pixels();
        (p.to_vec(), w, h, a.cfg.island_body_alpha)
    })
    .unwrap_or((Vec::new(), 0, 0, 0));
    let a_at = |x: i32, y: i32| -> u32 { (px[(y * w + x) as usize] >> 24) & 0xFF };
    let mut asym = 0u32;
    for y in (h - 3)..h {
        for x in 0..w / 2 {
            if a_at(x, y) != a_at(w - 1 - x, y) {
                asym += 1;
            }
        }
    }
    all &= row(
        !px.is_empty() && a_at(0, 0) == 0 && a_at(w / 2, h / 2) == 255 && asym == 0,
        "⑩ 形状：角透明 / 中心不透明 / 对称",
        format!(
            "角 alpha={} · 中心 alpha={} · 底 alpha={} · 底部 3 行不对称像素 {} 个",
            a_at(0, 0),
            a_at(w / 2, h / 2),
            body_a,
            asym
        ),
    );

    // ⑪ `compose_ex(tk=255, body=None)` 与旧 `compose` 逐位相同（§7.2 #2）
    //
    // 这条保证「给岛加的第四个参数不会改动格言的任何一个像素」——
    // 也就是 v0.1.4 的 `--dump` 基线在新版本上继续有效。
    // **自带注毒对照**：同一份掩码用 `tk=254` 再合成一次，必须与前者不同，
    // 否则说明这条断言根本没测到 `tk`（比如参数被提前 `unwrap_or` 掉了）。
    let (same, poison_diff) = compose_equiv();
    all &= row(
        same && poison_diff > 0,
        "⑪ tk=255/body=None 与 compose 逐位相同",
        format!("逐位相同={same} · tk=254 时差异像素 {poison_diff} 个（须 >0）"),
    );

    // ⑫ 动画 / 出帧期间 `config.ini` 的 mtime 不变（§7.2 #6）
    //
    // **起始就没有 `config.ini` 也是合法情形**（新用户从没动过设置、或者
    // `DG_DATA_DIR` 指向一个空临时目录 —— 自检自己就这么跑）。这时断言
    // 换成更强的那个：60 拍之后**它仍然不存在**，即「一拍都没写盘」。
    // 早先的写法是 `m0.is_some() && m0 == m1`，在那个情形下直接判红 ——
    // 一条「新装必红」的断言，等于把新用户挡在门外。
    // 两种分支都注得进毒：让 `tick` 每拍 `save()` ⇒ `Some` 分支 mtime 变、
    // `None` 分支文件被创建，都会红。
    let cfg_path = crate::util::data_dir().join("config.ini");
    let mtime = || std::fs::metadata(&cfg_path).ok().and_then(|m| m.modified().ok());
    let m0 = mtime();
    with_app(|a| {
        for _ in 0..60 {
            island::tick(a);
        }
    });
    let m1 = mtime();
    let (ok, detail) = match m0 {
        Some(t0) => (
            m1.is_some() && m1 == Some(t0),
            format!("{} · mtime {} → {}", cfg_path.display(), fmt_systime(t0), m1.map(fmt_systime).unwrap_or_else(|| "（没了）".into())),
        ),
        None => (
            m1.is_none(),
            format!("{} · 起始不存在 → 60 拍后{}（都算未写盘）", cfg_path.display(), if m1.is_none() { "仍不存在" } else { "被创建了" }),
        ),
    };
    all &= row(ok, "⑫ 出帧不写 config.ini", detail);

    // ⑬ 出帧次数**只跟内容变化**走（§7.2 #11 的确定性版本）
    //
    // 规格书写的是「推进 3 秒，静止时增量 0、时钟跳变时 +1」。按秒数推进
    // 会跨过分钟边界，跑在边界上就成了偶发失败（本项目对 flaky 零容忍）。
    // 改成**同一个等式**：`renders 增量 == 时钟字符串变化次数`。快跑 200 拍
    // 时右边通常是 0，偶尔是 1，两种情形都由同一条等式覆盖。
    let (dr, dclock) = with_app(|a| {
        a.island.dirty = false;
        let r0 = a.island.renders;
        let c0 = a.island.samples.clock.clone();
        let mut changes = 0u32;
        let mut prev = c0;
        for _ in 0..200 {
            island::tick(a);
            if a.island.samples.clock != prev {
                changes += 1;
                prev = a.island.samples.clock.clone();
            }
        }
        (a.island.renders.wrapping_sub(r0), changes)
    })
    .unwrap_or((0, 0));
    all &= row(
        dr == dclock,
        "⑬ 出帧次数 == 时钟变化次数",
        format!("200 拍：renders +{dr} · 时钟变化 {dclock} 次（须相等 · visible=false）"),
    );

    // ⑭ **中性灰的次色必须真的画成灰色**（不是强调色）
    //
    // 这条抓的是「把显示色当通道值」这个 bug：掩码趟若写显示色，中性灰
    // （`#B8B8B8`）的三个通道字节**相等**，而 `compose` 的判据是
    // 「哪条字节最大就取哪条」，平局按 `qa >= da && qa >= ma` 判给强调通道
    // ⇒ 日期与指标被画成强调蓝。默认配置的 `date_color` 正是 `#B8B8B8`，
    // 所以这个 bug 在默认外观下 100% 复现，却不会让任何形状断言变红。
    //
    // 判据取「中性灰文字像素必须存在」：把强调色改成**洋红**（与灰色在
    // 色相上不可能混淆），如果次色又跑到强调通道上，灰色像素会一个都没有。
    let (grey, magenta) = with_app(|a| {
        a.cfg.date_color = 0x0080_8080; // 中性灰
        a.cfg.island_accent_color = 0x00FF_00FF; // 洋红
        // 六项全开 + 填满采样：否则指标行与消息行压根不画，判据会退化成
        // 「只有日期是灰的」——那样强调通道有没有串色根本没验到
        a.island.items = Items::parse("clock,dot,cpu,mem,battery,msg");
        a.island.samples.cpu = Some(37);
        a.island.samples.mem = Some(62);
        a.island.samples.battery = Some((88, false));
        a.island.samples.unread = 3;
        a.island.samples.msg = Some("回归".into());
        island::expand(a);
        island::draw::render(a);
        let (p, _, _) = a.island.frame_pixels();
        // 底的合成色：底不透明度与全局 opacity 都已被 ⑩ 钉成 255 ⇒ 底就是
        // 配置里的颜色本身，可以逐通道比对（这一版没有用 alpha 区分文字，
        // 因为底不透明时文字像素的 alpha 也是 255）。
        let bc = a.cfg.island_body_color;
        let (br, bg, bb) = ((bc >> 16) & 0xFF, (bc >> 8) & 0xFF, bc & 0xFF);
        let (mut grey, mut magenta) = (0u32, 0u32);
        for &q in p.iter() {
            if (q >> 24) & 0xFF == 0 {
                continue; // 窗口外（圆角外）
            }
            let (r, g, b) = ((q >> 16) & 0xFF, (q >> 8) & 0xFF, q & 0xFF);
            if r.abs_diff(br) <= 6 && g.abs_diff(bg) <= 6 && b.abs_diff(bb) <= 6 {
                continue; // 只有胶囊底，不是文字
            }
            // 抗锯齿的**外沿**像素也满足「比底亮一点」，但它是深色（预乘后的
            // 底被 alpha 缩小）。不加这道亮度门，它们会被当成灰色的文字像素，
            // 于是「次色串到强调」注毒也不会红（实测会漏 56 个）。
            if r + g + b < 90 {
                continue;
            }
            let spread = |x: u32, y: u32| x.abs_diff(y);
            if spread(r, g) <= 3 && spread(g, b) <= 3 {
                grey += 1;
            }
            if r > g + 40 && b > g + 40 {
                magenta += 1;
            }
        }
        (grey, magenta)
    })
    .unwrap_or((0, 0));
    all &= row(
        grey > 0 && magenta > 0,
        "⑭ 中性次色画成灰色（不串到强调）",
        format!("中性灰文字像素 {grey} 个（须 >0）· 洋红（消息行）{magenta} 个（须 >0）"),
    );

    // ⑮ 轮播真的会翻页（端到端：走 `tick`，不是直接改状态）
    //
    // 计时靠**伪造 `last_tick`**：`tick` 的 dt 来自 `Instant`，把上一拍的时刻
    // 往前挪 100ms 就等于「已经过了 100ms」。这样不用真等 1 秒，也不用 sleep。
    // 纯函数那一半（阈值、取模、超时携带）在 `geom::rot_*` 的单测里逐点覆盖，
    // 这里只验「tick 真的把相位推过去了、页真的换了」。
    let (pages_n, saw_out, saw_in, page_after, renders_up) = with_app(|a| {
        a.cfg.island_rotate = 1;
        // 不计数 ⇒ Msg 片段显示**正文**（长），而不是 4 个字符的 `MSG n`。
        // 正文 20 个汉字必然超过一页的可用宽度，于是「至少两页」是结构性的，
        // 不依赖这台机器有没有电池 / CPU 采样到没有。
        a.cfg.island_unread = false;
        // 进入延时钉到上限 5000ms：`tick` 每拍都会重新判定悬停，若光标正好
        // 落在胶囊上，1.4s 内就会展开 —— 展开态**不轮播**（用户在看面板），
        // 于是这一步会因为它验的东西以外的东西失败。钉住延时后 1.4s 内
        // 根本不可能展开，轮播计时才是这步唯一在推进的量。
        a.cfg.island_hover_ms = 5000;
        // **必须写 `cfg.island_items`**：`tick` 每拍都从配置重新解析一遍片段表，
        // 直接改 `a.island.items` 会在第一拍就被覆盖回默认的 `clock,dot`
        // （只有一页 ⇒ 轮播恒不启动，这一步会以「共 1 页」失败）。
        a.cfg.island_items = "clock,msg".to_string();
        a.island.items = Items::parse(&a.cfg.island_items);
        a.island.samples.counting = false;
        a.island.samples.msg = Some("轮播测试的一句话要足够长才能撑出第二页".into());
        island::collapse(a);
        island::draw::render(a); // 让 draw 把页数回写
        let pages = a.island.pages;
        let r0 = a.island.renders;
        let (mut out, mut inn) = (false, false);
        for _ in 0..14 {
            a.island.last_tick = Instant::now() - std::time::Duration::from_millis(100);
            island::tick(a);
            match a.island.rot.phase {
                island::geom::RotPhase::Out => out = true,
                island::geom::RotPhase::In => inn = true,
                island::geom::RotPhase::Hold => {}
            }
        }
        (pages, out, inn, a.island.rot.page, a.island.renders > r0)
    })
    .unwrap_or((0, false, false, 0, false));
    all &= row(
        pages_n >= 2 && saw_out && saw_in && page_after == 1 && renders_up,
        "⑮ 轮播翻页（tick 推进 1.4s）",
        format!(
            "共 {pages_n} 页 · 见到淡出={saw_out} 淡入={saw_in} · 停在页 {page_after} · 出过帧={renders_up}"
        ),
    );

    // ⑯ 淡出**真的让画面变暗**（不是只改了一个变量）
    //
    // 只断言 `rot_k < 1` 是没牙的：真正的风险是 `draw` 把它算出来却没乘进
    // `tk`（那样 `rot_k` 照样是 0.4，画面一点没变）。所以这里直接读像素：
    // 「亮像素」（主色文字，亮度远超胶囊底）的数量必须显著下降。
    let (full, dim) = with_app(|a| {
        let bright = |a: &App| -> u32 {
            let (p, _, _) = a.island.frame_pixels();
            p.iter()
                .filter(|q| {
                    let q = **q;
                    let m = ((q >> 16) & 0xFF).max((q >> 8) & 0xFF).max(q & 0xFF);
                    m > 180
                })
                .count() as u32
        };
        // 只看时钟：它是**主色**（亮），而上一页的正文是次色灰（最亮通道 128，
        // 压根过不了 180 这道门）—— 拿灰字验「变暗」会得到 0 → 0。
        a.island.items = Items::parse("clock");
        a.island.rot.page = 0;
        island::collapse(a);
        a.island.rot_k = 1.0;
        island::draw::render(a);
        let n1 = bright(a);
        a.island.rot_k = 0.4;
        island::draw::render(a);
        let n2 = bright(a);
        a.island.rot_k = 1.0;
        island::draw::render(a);
        (n1, n2)
    })
    .unwrap_or((0, 0));
    all &= row(
        full > 0 && dim * 2 < full,
        "⑯ 淡出后亮像素显著减少",
        format!("rot_k=1.0 时亮像素 {full} 个 · rot_k=0.4 时 {dim} 个（须 < 一半）"),
    );

    // ⑰ 事件管道端到端（真文件、真搬动、真计数）
    //
    // 单测已经逐条覆盖 `parse_body` / `pump` 的分支，这一条验的是**接线**：
    // `pump(root, ...)` 拿到的 root 与「用户往哪儿丢文件」是同一个约定
    // （`data_dir()/events`），而且消费之后文件真的进了 `seen/`。
    // 目录用临时目录，**不碰用户真实的事件目录** —— 这正是 `pump` 收 root
    // 参数而不是内部写死路径的原因。
    {
        let root = std::env::temp_dir().join(format!("dg-isl-ev-{}", std::process::id()));
        let ev_dir = root.join("events");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::create_dir_all(&ev_dir);
        for (i, body) in ["第一条", "第二条", "构建完成\tCI"].iter().enumerate() {
            let name = format!("20261123090{}00_1.msg", i);
            let _ = std::fs::write(ev_dir.join(name), body);
        }
        let ev = crate::island::content::pump(&ev_dir, true, true);
        let left = std::fs::read_dir(&ev_dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("msg")))
                    .count()
            })
            .unwrap_or(99);
        let seen = std::fs::read_dir(ev_dir.join("seen"))
            .map(|rd| rd.count())
            .unwrap_or(0);
        // 事件目录必须是 `data_dir()/events`（脚本要按这个路径丢文件）
        let wired = crate::island::content::events_dir() == crate::util::data_dir().join("events");
        let _ = std::fs::remove_dir_all(&root);
        all &= row(
            ev.fresh == 3
                && ev.recent == 3
                && left == 0
                && seen == 3
                && ev.latest.as_deref() == Some("CI · 构建完成")
                && wired,
            "⑰ 事件管道：读完归档 · 计数 · 正文规范化",
            format!(
                "3 个 .msg → fresh {} · recent {} · 事件目录剩 {} · seen {} 个 · 最新「{}」· 路径接线={}",
                ev.fresh,
                ev.recent,
                left,
                seen,
                ev.latest.unwrap_or_default(),
                wired
            ),
        );
    }

    // ⑱ 验证入口**不许消费**用户的真实事件文件（家规：零副作用）
    //
    // `--islandtest` 会反复调 `tick`，而 `tick` 里的采样会跑线上的 `pump`。
    // 只要 `consume_events` 被谁改回 `true`，跑一次自检就会把用户的 `.msg`
    // 搬进 `seen/` —— 症状极隐蔽（消息「自己变已读」），所以在这里钉死：
    // 开关关掉之后，**关窗再开窗也不能被改回来**。
    let no_consume = with_app(|a| {
        a.cfg.island = false;
        island::apply_enabled(a);
        a.cfg.island = true;
        unsafe { island::create(a) };
        let kept = !a.island.consume_events;
        a.cfg.island = false;
        island::apply_enabled(a);
        kept
    })
    .unwrap_or(false);
    all &= row(
        no_consume,
        "⑱ 自检不消费事件文件（关窗后仍保持）",
        format!("重开一轮窗口后 consume_events 仍为 false={no_consume}"),
    );

    // 收尾：关窗
    with_app(|a| {
        a.cfg.island = false;
        island::apply_enabled(a);
    });

    println!(
        "\n{}",
        if all {
            "灵动岛自检 19 步全 PASS（⓪ + ①~⑱）"
        } else {
            "存在失败项"
        }
    );
    if all {
        0
    } else {
        3
    }
}

fn fmt_systime(t: std::time::SystemTime) -> String {
    // 只用来印在诊断串里，取「距 UNIX 纪元的毫秒」就够看出变没变
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => format!("{}ms", d.as_millis()),
        Err(_) => "?".to_string(),
    }
}

/// 「同一份掩码，新旧两条合成分支是否逐位相同」+「注毒后是否会不同」。
///
/// 返回 `(逐位相同, tk=254 时的差异像素数)`。第二个返回值是**判据有效性的
/// 自证**：如果 `tk` 被谁改成了常量（或者 `compose` 里提前 `unwrap_or`），
/// 两次结果仍会相同，第二个数就是 0，这条断言会自己变红。
fn compose_equiv() -> (bool, u32) {
    let (w, h) = (64, 48);
    let mut f = gfx::Frame::new(w, h);
    let bands = gfx::BandTable::uniform(255, 255);
    let (c, dc, qc) = (0x00F5_D98B, 0x00B8_B8B8, 0x004C_9AFF);
    // 伪随机掩码：三通道都要有值，且 byte3 必须是 0（旧路径从不写它）
    let mask = |i: usize| -> u32 { (i as u32).wrapping_mul(2654435761) & 0x00FF_FFFF };
    let refill = |f: &mut gfx::Frame| {
        for (i, p) in f.ink_pixels_mut().iter_mut().enumerate() {
            *p = mask(i);
        }
    };

    refill(&mut f);
    f.compose(200, 255, &bands, c, dc, qc);
    let base = f.out_pixels().to_vec();

    refill(&mut f);
    f.compose_ex(200, 255, &bands, c, dc, qc, 255, None);
    let ex = f.out_pixels().to_vec();

    refill(&mut f);
    f.compose_ex(200, 255, &bands, c, dc, qc, 254, None);
    let poison = f.out_pixels();

    let diff = poison.iter().zip(&ex).filter(|(a, b)| a != b).count();
    (base == ex, diff as u32)
}

// ---------------------------------------------------------------- 单帧导出

/// 导出时**冻结**的采样值。`None` = 该项不出现（模拟「这台机器没有这一项」）。
///
/// 默认值刻意让每一项都非空：基线要同时覆盖 时钟 / 日期 / 三个指标 /
/// 消息行 / 未读圆点。少冻结一项，将来那一行画坏了就没人发现。
struct Frozen {
    /// `true` = 收起端点。默认导出**展开**端点：内容多、覆盖面大，
    /// 而收起端点的排版是展开态的子集（同样的字体与度量）。
    compact: bool,
    /// `None` = 用固定时刻格式化出来的字符串（走线上同一条 `fmt_clock`）
    clock: Option<String>,
    cpu: Option<u32>,
    mem: Option<u32>,
    battery: Option<(u32, bool)>,
    msg: Option<String>,
    unread: u32,
    /// `None` = 六项全开（基线默认）；`Some("cfg")` = 服从 `island_items`
    items: Option<String>,
    /// 收起态第几页（越界按取模回绕）。展开态无意义，恒 0
    page: usize,
}

impl Default for Frozen {
    fn default() -> Frozen {
        Frozen {
            compact: false,
            clock: None,
            cpu: Some(37),
            mem: Some(62),
            battery: Some((88, false)),
            msg: Some("今晚八点归档 v0.2".to_string()),
            unread: 3,
            items: None,
            page: 0,
        }
    }
}

/// `-` / 解析失败 ⇒ `None`（该项不出现）。数字可带 `%`。
fn num_or_none(v: &str) -> Option<u32> {
    v.trim().trim_end_matches('%').parse::<u32>().ok().map(|n| n.min(100))
}

/// `88` = 放电中，`88+` = 充电中，`-` = 无电池。
fn parse_battery(v: &str) -> Option<(u32, bool)> {
    let v = v.trim();
    match v.strip_suffix('+') {
        Some(n) => num_or_none(n).map(|p| (p, true)),
        None => num_or_none(v).map(|p| (p, false)),
    }
}

fn freeze(overrides: &[(String, String)]) -> Frozen {
    let mut f = Frozen::default();
    for (k, v) in overrides {
        match k.as_str() {
            "state" => f.compact = matches!(v.as_str(), "compact" | "collapsed" | "收起"),
            "clock" => f.clock = Some(v.clone()),
            "cpu" => f.cpu = num_or_none(v),
            "mem" => f.mem = num_or_none(v),
            "battery" => f.battery = parse_battery(v),
            "msg" => f.msg = Some(v.clone()).filter(|s| !s.is_empty() && s != "-"),
            "unread" => f.unread = v.trim().parse::<u32>().unwrap_or(0),
            "items" => f.items = Some(v.clone()),
            "page" => f.page = v.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    f
}

fn fmt_opt(v: Option<u32>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "-".to_string())
}

/// `desk-geyan --dump-island <png> [key=value ...]`
///
/// 把岛的一帧导出成**保留 alpha 的透明底** PNG，供形状 / 排版做像素级回归
/// （规格书 §7.4「岛自身的像素基线」）。
///
/// # 与格言 `--dump` 的两个差别
///
/// * **不叠棋盘格**。格言那张要看清「哪里透」，所以混了棋盘底；岛的底本身
///   就是半透明胶囊，混了棋盘格反而看不出底色对不对。这里输出非预乘的
///   真 alpha，看图软件里透明区直接显背景。
/// * **系统采样默认冻结**。CPU 占用率每帧都在跳，不冻结的话同一份代码
///   两次导出的 md5 必然不同——像素基线等于废的。所以默认值全部是常数。
///   要拿真实采样看一眼，显式传 `cpu=real` 是不支持的：dump 的职责就是
///   **确定性**，要看真实数据请直接看屏幕上的岛。
///
/// # 导出专用键（`config::apply_kv` 不认识，会被它静默忽略）
///
/// | 键 | 值 | 默认 |
/// |---|---|---|
/// | `state` | `expanded` / `compact` | `expanded` |
/// | `items` | `all` / `cfg` / `clock,mem,...` | 六项全开 |
/// | `clock` | 直接给出时钟字符串 | 由固定时刻格式化 |
/// | `cpu` `mem` `battery` `unread` | 数字，`-` = 不显示 | 37 / 62 / 88 / 3 |
/// | `msg` | 消息正文，`-` = 不显示 | `今晚八点归档 v0.2` |
///
/// 其余 `key=value` 一律走 `config::apply_kv`，与配置文件同源 ——
/// `dpi` 之类改不了（它来自真实显示器），但 `island_w` / `island_h` /
/// `island_body_color` / `island_body_alpha` / `color` / `shadow` / `font`
/// 都能临时试。
pub(crate) fn dump_island(path: &str, overrides: &[(String, String)]) -> i32 {
    let fz = freeze(overrides);

    let Some(mut a) = App::new() else {
        println!("FAIL  初始化失败（词库为空？）");
        return 3;
    };
    for (k, v) in overrides {
        crate::config::apply_kv(&mut a.cfg, k, v);
    }
    // 导出的就是一个真实的岛，所以必须真的把窗口建出来：`render` 要 `hwnd`
    // 才肯干活（它最后一步是 ULW），而度量用的 DC 也挂在 `frame` 上。
    a.cfg.island = true;
    // 导出不许搬用户的真实消息文件（`samples.update` 会跑线上的 `pump`）
    a.island.consume_events = false;
    if !unsafe { island::create(&mut a) } {
        println!("FAIL  岛窗口建不出来");
        return 3;
    }

    // ---- 内容：先用**固定时刻**跑一遍线上的 `samples.update`（格式化路径
    //      与运行时完全同一条，不是另写一套），再把系统量与事件量覆写成常数。
    let use_cfg_items = fz.items.as_deref() == Some("cfg")
        || overrides.iter().any(|(k, _)| k == "island_items");
    let items = if use_cfg_items {
        Items::parse(&a.cfg.island_items)
    } else {
        match fz.items.as_deref() {
            Some(list) if list != "all" => Items::parse(list),
            _ => Items::parse("clock,dot,cpu,mem,battery,msg"),
        }
    };
    let seconds = a.cfg.seconds;
    a.island.items = items.clone();
    {
        // 2026-11-23 是星期一（`wDayOfWeek` 0 = 周日），与项目里其它
        // 固定时刻的夹具同一个日期，出错时对日志方便。
        let mut now: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
        now.wYear = 2026;
        now.wMonth = 11;
        now.wDay = 23;
        now.wDayOfWeek = 1;
        now.wHour = 9;
        now.wMinute = 41;
        now.wSecond = 0;
        // `consume: false` —— 导出**绝不**搬动用户的真实消息文件
        // （`--dump-island` 会跑线上的 `update`，把这条显式关掉才谈得上零副作用）。
        let opts = crate::island::content::Opts {
            seconds,
            unread: false,
            consume: false,
            keep: a.cfg.island_events_keep,
        };
        a.island.samples.update(&items, opts, &now);
    }
    a.island.samples.cpu = fz.cpu;
    a.island.samples.mem = fz.mem;
    a.island.samples.battery = fz.battery;
    a.island.samples.msg = fz.msg.clone();
    a.island.samples.unread = fz.unread;
    if let Some(c) = &fz.clock {
        a.island.samples.clock = c.clone();
    }

    // ---- 端点落定 + 两趟出帧。
    //
    // 收起态的窗口宽度是**内容量出来的**：第一趟把真实宽度回写进
    // `Island::compact_w`，重算几何之后第二趟才是「窗口正好裹住内容」的
    // 最终形态。少跑第二趟，导出的收起态会带着首帧估算宽度的余量，
    // 像素基线就会随中英混排的比例漂。
    let want = if fz.compact {
        State::Collapsed
    } else {
        State::Expanded
    };
    island::force_state(&mut a, want);
    // 页号要在出帧之前定：收起态的排版按 `rot.page` 选页
    a.island.rot.page = fz.page;
    island::draw::render(&mut a);
    island::recompute(&mut a);
    island::draw::render(&mut a);

    // ---- 取像素（**预乘** ARGB）
    let (px, w, h) = a.island.frame_pixels();
    if w <= 0 || h <= 0 || px.len() < (w as usize) * (h as usize) {
        println!("FAIL  帧为空（{}x{}）", w, h);
        return 3;
    }
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    let (mut solid, mut semi, mut clear) = (0u32, 0u32, 0u32);
    for y in 0..h {
        for x in 0..w {
            let p = px[(y * w + x) as usize];
            let al = (p >> 24) & 0xFF;
            let (r, g, b) = ((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF);
            // 帧里是预乘色。透明底 PNG 必须给**非预乘**色，否则看图软件
            // 会把半透明边缘渲染成「发黑的一圈」（乘了两次 alpha）。
            let un = |c: u32| -> u8 {
                (c * 255 + al / 2).checked_div(al).unwrap_or(0).min(255) as u8
            };
            rgba.extend_from_slice(&[un(r), un(g), un(b), al as u8]);
            match al {
                0 => clear += 1,
                255 => solid += 1,
                _ => semi += 1,
            }
        }
    }
    // 画面已经取进 `rgba`，窗口可以拆了：不留一个 topmost 窗口到进程结束。
    // 摘要要用的几个量必须在 `destroy` **之前**抄下来 —— 它会把整个
    // `Island` 换成 `Island::new()`（几何归零、采样清空）。
    let (radius, dpi, clock_shown) = (a.island.rect.r, a.island.dpi, a.island.samples.clock.clone());
    let (a_page, a_pages) = (a.island.rot.page, a.island.pages);
    let (body_c, body_a, main_c, dim_c, accent_c) = (
        a.cfg.island_body_color,
        a.cfg.island_body_alpha,
        a.cfg.color,
        a.cfg.date_color,
        a.cfg.island_accent_color,
    );
    let (shadow, opacity, font_name) = (a.cfg.shadow, a.cfg.opacity, a.cfg.font.clone());
    island::destroy(&mut a);

    match crate::png::write_png(path, w as u32, h as u32, &rgba) {
        Ok(()) => {
            println!(
                "已写入 {}  ({}x{}，透明底 · alpha 保留)\n\
                 形态：{} · 圆角 {}px · dpi {} · 第 {} 页（共 {} 页）\n\
                 外观：底 {}@{} · 主色 {} · 日期 {} · 强调 {} · 阴影 {} · 字体 {} · 全局不透明度 {}\n\
                 内容：{} · clock={} cpu={} mem={} battery={} unread={} msg={}\n\
                 像素：全不透明 {} / 半透明 {} / 全透明 {}（合计 {}）\n\
                 注：外观各行取自**真实 config.ini** —— 基线要跨机对比就显式把这些键钉住",
                path,
                w,
                h,
                if fz.compact { "收起" } else { "展开" },
                radius,
                dpi,
                a_page,
                a_pages,
                crate::util::fmt_color(body_c),
                body_a,
                crate::util::fmt_color(main_c),
                crate::util::fmt_color(dim_c),
                crate::util::fmt_color(accent_c),
                if shadow { "开" } else { "关" },
                font_name,
                opacity,
                items.to_csv(),
                clock_shown,
                fmt_opt(fz.cpu),
                fmt_opt(fz.mem),
                fz.battery
                    .map(|(p, c)| format!("{}{p}%", if c { "+" } else { "" }))
                    .unwrap_or_else(|| "-".to_string()),
                fz.unread,
                fz.msg.clone().unwrap_or_else(|| "-".to_string()),
                solid,
                semi,
                clear,
                w * h
            );
            0
        }
        Err(e) => {
            println!("FAIL  写文件失败: {}", e);
            3
        }
    }
}
