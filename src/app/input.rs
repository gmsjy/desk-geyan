//! 输入与交互：窗口过程、拖拽、命中判定、热键、托盘与菜单。
//!
//! 挂件自己是恒穿透的（`WS_EX_TRANSPARENT`），所以所有操作都走
//! **全局热键 + 托盘菜单**；需要鼠标直接操作时由 `Ctrl+Alt+E` 进布局模式临时放行。

use super::*;

use super::menu_host;
use std::ffi::c_void;
use std::mem::{zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::Ordering;
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, HGDIOBJ, PAINTSTRUCT};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, ReleaseCapture, SetCapture, UnregisterHotKey};
use windows_sys::Win32::UI::Shell::{NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW};
use windows_sys::Win32::UI::WindowsAndMessaging::{AppendMenuW, CreatePopupMenu, DefWindowProcW, DestroyMenu, DestroyWindow, GetClientRect, GetCursorPos, GetForegroundWindow, GetWindowRect, HCURSOR, KillTimer, LoadCursorW, PostMessageW, PostQuitMessage, SetCursor, SetTimer, TrackPopupMenu};
use crate::{autostart, clipboard, favorites, island};
use crate::config::{self, Anim, Config, Mode, Orient, VStyle};
use crate::config_ui;
use crate::consts::*;
use crate::util::*;

// 热键 id
const HK_TOGGLE: i32 = 101;

const HK_ANIM: i32 = 102;

const HK_MODE: i32 = 103;

const HK_LAYOUT: i32 = 104;

const HK_UP: i32 = 105;

const HK_DOWN: i32 = 106;

const HK_PREV: i32 = 107;

const HK_NEXT: i32 = 108;

const HK_TOPMOST: i32 = 109;

const HK_QUIT: i32 = 110;

const HK_HIDE: i32 = 111;

const HK_QUOTES: i32 = 112;

const HK_CLOCK: i32 = 113;

const HK_DATE: i32 = 114;

const HK_SIZE_UP: i32 = 115;

const HK_SIZE_DOWN: i32 = 116;

const HK_COLOR: i32 = 117;

pub(crate) const HK_FAV: i32 = 118;

/// 打开 / 置前设置面板（选字体等）
const HK_CONFIG: i32 = 119;

const HK_PAUSE: i32 = 120;

/// 桌面适配一键切换（自定 → 浅底 → 深底）
const HK_FIT: i32 = 121;

// ---------------------------------------------------------------- 热键

/// 全部全局热键：`(id, 虚拟键)`，统一 `Ctrl+Alt+<键>`。
///
/// 抽成模块级常量是为了让「注册」和「`--probe` 的期望值」**同源**：
/// 曾经期望值是文案里写死的 `17`，加了收藏热键之后它变成了
/// 「18/17 注册成功」—— 一个自己打自己脸的输出。
const HOTKEY_TABLE: [(i32, u32); 21] = [
    (HK_TOGGLE, VK_H),
    (HK_ANIM, VK_M),
    (HK_MODE, VK_T),
    (HK_LAYOUT, VK_E),
    (HK_UP, VK_UP),
    (HK_DOWN, VK_DOWN),
        (HK_PREV, VK_LEFT),
        (HK_NEXT, VK_RIGHT),
        (HK_TOPMOST, VK_P),
        (HK_HIDE, VK_S),
        (HK_QUOTES, VK_R),
        (HK_CLOCK, VK_C),
        (HK_DATE, VK_D),
        (HK_SIZE_UP, VK_OEM_PLUS),
        (HK_SIZE_DOWN, VK_OEM_MINUS),
        (HK_COLOR, VK_K),
        (HK_FAV, VK_F),
        (HK_CONFIG, VK_O),
        (HK_PAUSE, VK_SPACE),
        (HK_FIT, VK_A),
        (HK_QUIT, VK_X),
    ];

/// 期望注册成功的热键数。`register_hotkeys` 与 `--probe` 共用这一份。
pub(crate) const HOTKEY_EXPECT: usize = HOTKEY_TABLE.len();

pub(crate) fn register_hotkeys(a: &mut App) {
    const M: u32 = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
    for (id, vk) in HOTKEY_TABLE {
        let ok = unsafe { RegisterHotKey(a.panel, id, M, vk) } != 0;
        if ok {
            a.hotkeys.push(id);
        } else {
            // 记下失败码：1409 表示被**别的进程**占着（本机同时跑着另一个实例
            // 时必然如此），那是「测不了」，跟「坏了」是两件事。
            a.hotkey_err = unsafe { GetLastError() };
        }
    }
}

pub(crate) fn unregister_hotkeys(a: &App) {
    for id in &a.hotkeys {
        unsafe { UnregisterHotKey(a.panel, *id) };
    }
}

fn on_hotkey(a: &mut App, id: i32) {
    match id {
        HK_TOGGLE => set_visible(a, !a.cfg.visible),
        HK_HIDE => set_visible(a, false),
        HK_ANIM => {
            let mut next = a.cfg.anim.next();
            // 竖版不支持跑马灯：热键循环时直接跳过这一档，
            // 否则会出现「按了没反应」（cfg 变了，生效动效还是静态）
            if a.cfg.orient == Orient::Vertical && next == Anim::Marquee {
                next = next.next();
            }
            a.cfg.anim = next;
            a.phase = 0.0;
            a.pulse = 0.0;
            a.m = 0.0;
            // 切动效时把在飞的过渡落定：否则从静态切走的那一下
            // 会接着上个模式的半个动画放
            a.trans.settle();
            a.cfg.save();
            render_and_present(a);
        }
        HK_MODE => {
            a.cfg.mode = if a.cfg.mode == Mode::Glass {
                Mode::TextOnly
            } else {
                Mode::Glass
            };
            apply_mode(a);
        }
        HK_LAYOUT => {
            a.cfg.layout_mode = !a.cfg.layout_mode;
            apply_mode(a);
        }
        HK_UP => bump_opacity(a, 12),
        HK_DOWN => bump_opacity(a, -12),
        HK_PREV | HK_NEXT => {
            let n = a.list.len();
            a.qi = if id == HK_NEXT {
                (a.qi + 1) % n
            } else {
                (a.qi + n - 1) % n
            };
            a.qn = (a.qi + 1) % n;
            // 跑马灯已初始化时必须直接落到带左缘。只把 `m` 置 0 的话，
            // 绘制位置是 `band_r - m`（带右缘之外），新句第一帧是空白，
            // 与「0 帧直切」相反。
            a.m = if a.primed { marquee_band_w(&a.cfg) } else { 0.0 };
            a.phase = 0.0;
            a.pulse = 0.0;
            // 手动换句是 **0 帧直切**：用户主动操作要即时响应，不等动画。
            // 在飞的过渡直接落定，不会留下半段影子。
            a.trans.settle();
            a.cur_w = measure_quote(a, &a.list[a.qi]);
            render_and_present(a);
        }
        HK_TOPMOST => {
            a.cfg.always_on_top = !a.cfg.always_on_top;
            apply_topmost(a);
            a.cfg.save();
        }
        HK_QUOTES => {
            a.reload_library();
            render_and_present(a);
        }
        HK_FAV => favorite(a),
        HK_PAUSE => on_menu(a, MI_PAUSE),
        HK_CLOCK => on_menu(a, MI_CLOCK),
        HK_DATE => on_menu(a, MI_DATE),
        HK_SIZE_UP => on_menu(a, MI_SCALE_UP),
        HK_SIZE_DOWN => on_menu(a, MI_SCALE_DOWN),
        HK_COLOR => on_menu(a, MI_COLOR),
        HK_FIT => on_menu(a, MI_DESK_FIT),
        HK_CONFIG => config_ui::open(a),
        HK_QUIT => quit(a),
        _ => {}
    }
    // 热键绕过了设置面板。统一在这里把面板拉回真实状态；窗口没开时是空调用。
    config_ui::resync(a);
}

fn bump_opacity(a: &mut App, delta: i32) {
    a.cfg.opacity = (a.cfg.opacity + delta).clamp(30, 255);
    apply_glass(a);
    render_and_present(a);
    a.cfg.save();
}

// ---------------------------------------------------------------- 托盘

pub(crate) fn add_tray(a: &mut App) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = a.panel;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = crate::icon::app_icon(0, 0);
        let tip = wide("桌面格言 · 左键菜单 / Ctrl+Alt+M 换动效");
        for (i, c) in tip.iter().take(127).enumerate() {
            nid.szTip[i] = *c;
        }
        nid.szTip[127] = 0;
        a.tray_ok = Shell_NotifyIconW(NIM_ADD, &nid) != 0;
        if !a.tray_ok {
            // 注意：Shell_NotifyIcon 失败时**未必**会 SetLastError，
            // 所以 err=0 的含义是「shell 没给理由」，不等于「成功」。
            a.tray_err = GetLastError();
        }
    }
}

pub(crate) fn del_tray(a: &App) {
    if !a.tray_ok {
        return;
    }
    unsafe {
        let mut nid: NOTIFYICONDATAW = zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = a.panel;
        nid.uID = 1;
        Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// 弹一条托盘气泡。**不抢焦点、不弹窗** —— 挂件类程序的反馈必须是这种
/// 「看得见但不用处理」的形态，`MessageBox` 会把用户正在打字的输入框焦点抢走。
///
/// 托盘没挂上时静默失败：那条路径下热键和主功能都还是好的，
/// 为一条提示去降级反而更吵。
fn toast(a: &App, msg: &str) {
    if !a.tray_ok {
        return;
    }
    unsafe {
        let mut nid: NOTIFYICONDATAW = zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = a.panel;
        nid.uID = 1;
        nid.uFlags = NIF_INFO;
        let info = wide(msg);
        for (i, c) in info.iter().take(255).enumerate() {
            nid.szInfo[i] = *c;
        }
        nid.szInfo[255] = 0;
        let title = wide("桌面格言");
        for (i, c) in title.iter().take(63).enumerate() {
            nid.szInfoTitle[i] = *c;
        }
        nid.szInfoTitle[63] = 0;
        // NIIF_INFO = 1（信息图标）。不用 NIIF_USER 之类 —— 那要自定义图标，
        // 而这里只是告知结果。
        nid.dwInfoFlags = 1;
        Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

// ---------------------------------------------------------------- 收藏

/// 重载格言库之后，`qi` 该落在哪。
///
/// 直接 `qi.min(len - 1)` 是**错的**：收藏发生在列表**尾部**，索引并不越界，
/// 但 round-robin 交织之后位置全变了 —— `qi` 会安安静静地指向另一句，
/// 用户看到的就是「点了收藏，画面自己跳了」。
/// 所以必须按**文本**把同一条找回来；只有真的找不到了（用户把文件改坏了）
/// 才退回夹取。
pub(crate) fn keep_index(cur: &str, list: &[crate::quotes::Quote], fallback: usize) -> usize {
    list.iter()
        .position(|q| q.display() == cur)
        .unwrap_or_else(|| fallback.min(list.len().saturating_sub(1)))
}

/// 收藏**当前正在显示的这句**：复制到剪贴板 + 加入轮播。
///
/// 挂件常态是鼠标穿透的，点不到它本身，所以入口只能是托盘菜单和
/// `Ctrl+Alt+F`。两个入口走的是同一条路，不存在「点了没反应」的分裂。
///
/// 两件事**互不牵连**：剪贴板可能被别的窗口占着（`OpenClipboard` 失败），
/// 那是「复制没成」；写盘可能因为目录只读而失败，那是「收藏没成」。
/// 任何一半失败，另一半照做，气泡里如实说。
fn favorite(a: &mut App) {
    favorite_at(a, &crate::util::data_dir());
}

/// `favorite` 的可测核心：数据目录由调用方给。
///
/// 抽出来和 `quotes::library_from` 同一个理由 —— 直接测真实
/// `%APPDATA%\DeskGeyan` 的话，测试会**真的往用户的格言库里写东西**，
/// 而收藏夹文件一旦被测试污染就再也分不清哪些是用户自己收藏的。
/// `--favtest` 指向临时目录；运行时才用真实目录。
fn favorite_at(a: &mut App, dir: &std::path::Path) {
    if a.list.is_empty() {
        // 空库不该发生（内置 455 条兜底），但 `list` 是运行时状态，
        // 别在这里panic —— 静默返回比崩掉强。
        return;
    }
    let q = a.list[a.qi].clone();

    // ① 复制。窗口句柄传底板：托盘气泡的回调也挂在它上面，同一扇门。
    let copied = clipboard::copy_text(a.panel, &q.display());

    // ② 加入收藏夹（幂等：已在夹里就不重复写）。`append` 能区分
    // 新增 / 重复 / I/O 错误，上层不能把三者合并成 false。
    let appended = favorites::append(dir, &q);

    // ③ 真的加了 → 重载列表，让这句**立刻**进入轮播候选。
    // 关键：`qi` 必须仍指向同一条句子。直接 `a.qi = a.qi.min(len-1)`
    // 是不够的 —— 追加发生在列表尾部，索引虽然不越界，但 round-robin
    // 交织后位置全变，qi 会 silently 指向另一句，用户就觉得「画面自己跳了」。
    if matches!(appended, Ok(true)) {
        a.reload_library();
        // **不碰** m / phase / cur_w：这句还在显示，位置和宽度都没变，
        // 重置滚动相位会让画面无谓地跳一下。收藏对当前画面必须无感。
    }

    // ④ 反馈：写盘失败必须说失败，不能伪装成「已在收藏夹」。
    let msg = match (&appended, copied) {
        (Ok(true), true) => "已收藏，并复制到剪贴板".to_string(),
        (Ok(true), false) => "已收藏（剪贴板被占用，复制失败）".to_string(),
        (Ok(false), true) => "这句已在收藏夹里（已复制）".to_string(),
        (Ok(false), false) => "这句已在收藏夹里（剪贴板被占用）".to_string(),
        (Err(e), _) => format!("收藏失败：{e}"),
    };
    toast(a, &msg);
}

fn menu_add(menu: *mut c_void, id: u32, text: &str, checked: bool, enabled: bool) {
    let s = wide(text);
    let mut flags = MF_STRING;
    if checked {
        flags |= MF_CHECKED;
    }
    if !enabled {
        flags |= MF_GRAYED;
    }
    unsafe { AppendMenuW(menu, flags, id as usize, s.as_ptr()) };
}

pub(crate) fn sep(menu: *mut c_void) {
    let s = wide("");
    unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, s.as_ptr()) };
}

fn show_menu(a: &mut App) {
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }

        menu_add(menu, MI_TOGGLE, "显示 / 隐藏  (Ctrl+Alt+H)", a.cfg.visible, true);
        // 灵动岛是**另一个功能**，不是格言的一种形态 —— 所以它是独立一项，
        // 与「显示/隐藏格言」并列，勾选态读它自己的开关。
        menu_add(
            menu,
            MI_ISLAND,
            "显示灵动岛（屏幕顶部胶囊）",
            a.cfg.island,
            true,
        );
        // 设置面板是**全部配置的唯一入口**：菜单里不再重复任何设置项。
        // 旧版五个子菜单（版式 / 显示内容 / 外观 / 动效 / 各项微调）与面板
        // 逐项重复，两处各画一份状态纯属维护税；菜单只留高频操作。
        menu_add(
            menu,
            MI_CONFIG,
            "设置…（字体 / 显示 / 外观 / 版式 / 动效）  (Ctrl+Alt+O)",
            false,
            true,
        );
        sep(menu);
        menu_add(menu, MI_PREV, "上一条格言  (Ctrl+Alt+←)", false, true);
        menu_add(menu, MI_NEXT, "下一条格言  (Ctrl+Alt+→)", false, true);
        menu_add(
            menu,
            MI_PAUSE,
            "暂停轮播（固定当前句）  (Ctrl+Alt+空格)",
            a.cfg.paused,
            true,
        );
        menu_add(menu, MI_FAV_ONLY, "仅播放收藏夹", a.cfg.fav_only, true);
        // 收藏：挨着「上/下一条」放 —— 三条都是针对**当前这句**的操作，
        // 用户找的时候在同一处。
        menu_add(menu, MI_FAV, "收藏当前句子  (Ctrl+Alt+F)", false, true);
        sep(menu);
        // 桌面适配放在「重新载入」上方：两者都是「让挂件适配当前环境」的动作，
        // 文案带当前档位 —— 用户不用打开面板就知道自己现在是哪一档。
        menu_add(
            menu,
            MI_DESK_FIT,
            &format!(
                "桌面适配：{}  (Ctrl+Alt+A)",
                crate::config::fit_label(a.cfg.fit)
            ),
            false,
            true,
        );
        menu_add(menu, MI_RELOAD, "重新载入格言 / 配置  (Ctrl+Alt+R)", false, true);
        menu_add(menu, MI_RESET, "挂件回到右上角", false, true);
        menu_add(menu, MI_BACKUP, "备份配置与收藏…", false, true);
        menu_add(menu, MI_DIAG, "导出诊断报告…", false, true);
        // 分辨率适配两项：换显示器 / 改缩放比例之后，用户需要的是
        // 「一键把整套比例按当前屏重落一遍」，而不是自己去猜字号。
        menu_add(
            menu,
            MI_FIT,
            "适应当前屏幕（缩进工作区 · 字号同比）",
            false,
            true,
        );
        let dpi_label = if a.cfg.dpi_auto {
            let pct = a.cfg.eff_scale_pct();
            format!(
                "跟随屏幕 DPI 缩放  ·  当前 {}%（字号 {}%）",
                a.cfg.dpi, pct
            )
        } else {
            "跟随屏幕 DPI 缩放（关）".to_string()
        };
        menu_add(menu, MI_DPI, &dpi_label, a.cfg.dpi_auto, true);
        menu_add(menu, MI_CLOCK_RESET, "重置时间日期位置", false, a.clock_visible());
        sep(menu);
        menu_add(menu, MI_QUIT, "退出", false, true);

        let mut pt: POINT = zeroed();
        GetCursorPos(&mut pt);

        // 属主必须是「可见、可激活、用户看不见」的窗口 —— 详见 `menu_host` 模块头。
        // 底板窗口不合格：带 `WS_EX_NOACTIVATE`，永远不会失去激活 → 菜单点了别处
        // 也收不起来；而且用户只用灵动岛时它是隐藏的，隐藏窗口连前台资格都没有。
        // 旧实现临时摘掉底板的 `WS_EX_NOACTIVATE` 再 `SetForegroundWindow` 是治
        // 表不治里：隐藏状态下那一步必定失败，菜单于是挂住。
        let host = menu_host::ensure(a);

        let fg0 = GetForegroundWindow();
        menu_host::show_at(host, pt);
        let ok_fg = menu_host::activate(host);
        trace(
            "menu",
            &format!(
                "打开: host={host:?} panel={:?} fg_before={fg0:?} 置前结果={ok_fg} fg_after={:?} cfg_open={}",
                a.panel,
                GetForegroundWindow(),
                a.config.is_open()
            ),
        );

        // 建不出属主也照弹（`TrackPopupMenu` 不挑属主），但要点别处收起就没保证了，
        // 退回底板并留下记录 —— 这比「菜单根本不出现」对用户更友好。
        let owner = if host.is_null() { a.panel } else { host };
        let cmd = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            pt.x,
            pt.y,
            0,
            owner,
            null(),
        ) as u32;
        trace("menu", &format!("收起: cmd={cmd} fg={:?}", GetForegroundWindow()));

        // 文档上的配套动作：给属主补一条空消息，让它把「菜单已结束」这件事
        // 走完自己的消息队列。少了它，菜单消失后偶发一次「下一次点击被吞」。
        PostMessageW(owner, WM_NULL, 0, 0);
        menu_host::hide(host);
        DestroyMenu(menu);

        if cmd != 0 {
            on_menu(a, cmd);
        }
    }
}

/// 片段开关：`island_items` 的增删。
///
/// # 为什么要「插回原来的邻里位置」而不是简单追加
///
/// `island_items` 的 csv 顺序**就是显示顺序**。若一律追加到末尾，
/// 「关掉时钟再打开」会把它排到最后 —— 界面上是「时间跑到最右边去了」，
/// 而用户只是想把它开回来。自检也要求「再点一下必须回到原值」，追加会让
/// csv 从 `clock,dot` 变成 `dot,clock`，往返断言直接红。
///
/// 规则：找它**规范顺序里的前一个**已存在片段，插在它后面；没有前驱就插在
/// **后一个**已存在片段前面；都没有（空集合）就追加。于是对任何「邻里关系
/// 没被手工打乱」的集合都是精确往返。
fn toggle_item(a: &mut App, item: crate::island::content::Item) {
    use crate::island::content::ALL_ITEMS;
    let mut items = crate::island::content::Items::parse(&a.cfg.island_items);
    if items.order.contains(&item) {
        items.order.retain(|x| *x != item);
        // 关光会回落到 `clock`（`Items::parse` 的空集合兜底）：界面上的表现是
        // 「最后一项关不掉」。这是有意的 —— 一条什么都没有的胶囊不是功能。
        a.cfg.island_items = crate::island::content::Items::parse(&items.to_csv()).to_csv();
    } else {
        let canon = |it: crate::island::content::Item| -> usize {
            ALL_ITEMS.iter().position(|(_, x)| *x == it).unwrap_or(0)
        };
        let me = canon(item);
        let pos = items
            .order
            .iter()
            .rposition(|x| canon(*x) < me)
            .map(|i| i + 1)
            .or_else(|| items.order.iter().position(|x| canon(*x) > me))
            .unwrap_or(items.order.len());
        items.order.insert(pos, item);
        a.cfg.island_items = crate::island::content::Items::parse(&items.to_csv()).to_csv();
    }
    a.island.items = crate::island::content::Items::parse(&a.cfg.island_items);
    a.island.compact_w = 0; // 内容变了，宽度要重新度量
    a.island.dirty = true;
    a.cfg.save();
}

/// 执行一条菜单命令。
///
/// **托盘菜单和设置面板共用这一条路**（面板从 `config_ui` 里调它）：菜单项和面板
/// 控件都只是「发命令的界面」，语义只在这里定义一次。新增一个可改的配置项时，
/// 加一条 `MI_*` + 一个 `match` 分支，面板和菜单就同时支持了。
pub(crate) fn on_menu(a: &mut App, cmd: u32) {
    if (MI_ISLAND_ITEM_BASE..MI_ISLAND_ITEM_BASE + 7).contains(&cmd) {
        let item = crate::island::content::ALL_ITEMS[(cmd - MI_ISLAND_ITEM_BASE) as usize].1;
        toggle_item(a, item);
        return;
    }
    match cmd {
        MI_TOGGLE => set_visible(a, !a.cfg.visible),
        MI_GLASS => {
            a.cfg.mode = Mode::Glass;
            apply_mode(a);
        }
        MI_TEXT => {
            a.cfg.mode = Mode::TextOnly;
            apply_mode(a);
        }
        MI_LAYOUT => {
            a.cfg.layout_mode = !a.cfg.layout_mode;
            apply_mode(a);
        }
        // ---- 版式
        MI_ORIENT_BASE | MI_ORIENT_V => {
            let new = Orient::all()[(cmd - MI_ORIENT_BASE) as usize];
            if new == a.cfg.orient {
                return;
            }
            let was_marquee = a.cfg.anim == Anim::Marquee;
            a.cfg.set_orient(new);
            a.rebuild_fonts();
            // 尺寸可能换了（只在「原来正好是旧形态默认值」时才换），
            // 所以要走缩放那条路：重建帧缓冲 + 落到窗口上
            apply_resize(a, a.cfg.x, a.cfg.y, a.cfg.w, a.cfg.h);
            a.cfg.save();
            render_and_present(a);
            if new == Orient::Vertical && was_marquee {
                toast(a, "竖版下跑马灯已降级为静态");
            }
        }
        // ---- 竖版形态（只换页头与对齐，窗口尺寸一律不动）
        //
        // 用 guard 而不是 `MI_VSTYLE_BASE..=MI_VSTYLE_BASE + 2`：模式里不许写
        // 「常量 + 常数」这种表达式，只能写常量本身。
        _ if (MI_VSTYLE_BASE..=MI_VSTYLE_BASE + 2).contains(&cmd) => {
            let new = VStyle::all()[(cmd - MI_VSTYLE_BASE) as usize];
            if new == a.cfg.v_style {
                return;
            }
            a.cfg.v_style = new;
            a.cfg.save();
            render_and_present(a);
            let tip = if a.cfg.orient == Orient::Vertical {
                format!("竖排形态：{}", new.label())
            } else {
                format!("竖排形态：{}（切到竖版生效）", new.label())
            };
            toast(a, &tip);
        }
        MI_SHRINK_FIT => {
            a.cfg.shrink_fit = !a.cfg.shrink_fit;
            a.cfg.save();
            render_and_present(a);
        }
        MI_SPACING_UP => {
            a.cfg.letter_spacing = (a.cfg.letter_spacing + 1).clamp(0, 8);
            a.cfg.save();
            render_and_present(a);
        }
        MI_SPACING_DOWN => {
            a.cfg.letter_spacing = (a.cfg.letter_spacing - 1).clamp(0, 8);
            a.cfg.save();
            render_and_present(a);
        }
        MI_QUOTE_COLOR => {
            a.cfg.quote_color = config::next_quote_color(a.cfg.quote_color);
            a.cfg.save();
            render_and_present(a);
        }
        MI_OP_UP => bump_opacity(a, 12),
        MI_OP_DOWN => bump_opacity(a, -12),
        MI_TOPMOST => {
            a.cfg.always_on_top = !a.cfg.always_on_top;
            apply_topmost(a);
            a.cfg.save();
        }
        MI_AUTOSTART => {
            // 注册表是唯一状态源：菜单勾选态每次开菜单都重读，
            // 这里按「读到的真实状态」取反，而不是按 cfg（cfg 里根本没有这项）。
            let on = !autostart::enabled();
            if autostart::apply(on) {
                a.autostart_err = 0;
            } else {
                a.autostart_err = autostart::last_err();
            }
        }
        MI_AUTOHIDE => {
            a.cfg.auto_hide_fs = !a.cfg.auto_hide_fs;
            // 关闭的瞬间若正因全屏藏着，立刻恢复 —— 绝不能留下
            // 「功能关了、窗口还藏着」这种说不清的状态。
            if !a.cfg.auto_hide_fs && a.fs_hidden {
                temp_hide(a, false);
                a.fs_hidden = false;
            }
            a.cfg.save();
        }
        MI_SECONDS => {
            a.cfg.seconds = !a.cfg.seconds;
            a.cfg.save();
            render_and_present(a);
        }
        MI_SHADOW => {
            a.cfg.shadow = !a.cfg.shadow;
            a.cfg.save();
            render_and_present(a);
        }
        // 桌面适配：一键在「自定 → 浅底 → 深底」之间循环。
        //
        // 只改 `fit` 一个键，`color` / `shadow` 原值一律不动 —— 所以转一圈
        // 回到「自定」就是原样，不存在「切一次就找不回来」。
        // 这是它能当「一键」用的前提：档位是视图，不是破坏性编辑。
        MI_DESK_FIT => {
            a.cfg.fit = (a.cfg.fit + 1) % 3;
            a.cfg.save();
            render_and_present(a);
            toast(a, &format!("桌面适配：{}", crate::config::fit_label(a.cfg.fit)));
        }
        // ---- 显示内容
        MI_CLOCK => {
            a.cfg.show_clock = !a.cfg.show_clock;
            a.cfg.save();
            render_and_present(a);
        }
        MI_DATE => {
            a.cfg.show_date = !a.cfg.show_date;
            a.cfg.save();
            render_and_present(a);
        }
        MI_QUOTE => {
            a.cfg.show_quote = !a.cfg.show_quote;
            a.cfg.save();
            render_and_present(a);
        }
        // ---- 外观
        MI_SCALE_UP => {
            a.cfg.scale = config::next_scale(a.cfg.scale, 1);
            a.rebuild_fonts();
            a.cfg.save();
            render_and_present(a);
        }
        MI_SCALE_DOWN => {
            a.cfg.scale = config::next_scale(a.cfg.scale, -1);
            a.rebuild_fonts();
            a.cfg.save();
            render_and_present(a);
        }
        MI_CLOCK_UP => {
            a.cfg.clock_scale = config::next_step(&config::RATIO_STEPS, a.cfg.clock_scale, 1);
            a.rebuild_fonts();
            a.cfg.save();
            render_and_present(a);
        }
        MI_CLOCK_DOWN => {
            a.cfg.clock_scale = config::next_step(&config::RATIO_STEPS, a.cfg.clock_scale, -1);
            a.rebuild_fonts();
            a.cfg.save();
            render_and_present(a);
        }
        MI_QUOTE_UP => {
            a.cfg.quote_scale = config::next_step(&config::RATIO_STEPS, a.cfg.quote_scale, 1);
            a.rebuild_fonts();
            a.cfg.save();
            render_and_present(a);
        }
        MI_QUOTE_DOWN => {
            a.cfg.quote_scale = config::next_step(&config::RATIO_STEPS, a.cfg.quote_scale, -1);
            a.rebuild_fonts();
            a.cfg.save();
            render_and_present(a);
        }
        MI_CONFIG => config_ui::open(a),
        // ---- 灵动岛。**单写路径**：赋值、落地、落盘三件事都在这里，
        //      设置面板只发命令码，自己不碰 `cfg`。
        MI_ISLAND => {
            a.cfg.island = !a.cfg.island;
            island::apply_enabled(a);
            a.cfg.save();
        }
        MI_ISLAND_UNREAD => {
            a.cfg.island_unread = !a.cfg.island_unread;
            a.island.dirty = true;
            a.cfg.save();
        }
        MI_ISLAND_COLLAPSE => {
            a.cfg.island_state = false;
            island::collapse(a);
            a.cfg.save();
        }
        MI_ISLAND_EXPAND => {
            a.cfg.island_state = true;
            island::expand(a);
            a.cfg.save();
        }
        // ---- 灵动岛设置页（只从设置面板来）
        //
        // 每一条都做三件事：改 `cfg` 的**一个**字段、让岛重画、`save()`。
        // 「单一写路径」在这里的体现是：面板只管发命令码，不知道任何字段名。
        MI_ISLAND_KEEP => {
            a.cfg.island_events_keep = !a.cfg.island_events_keep;
            a.cfg.save();
        }
        // 新消息拉长时长：0..20s，步进 1s。0 = 不拉长（绿点仍然提示）。
        // 上限 20s 而不是 60s：拉长是「瞟一眼」的机制，超过 20 秒它就从
        // 「提示」变成「遮挡」了。
        MI_ISLAND_PEEK_DOWN | MI_ISLAND_PEEK_UP => {
            let d = if cmd == MI_ISLAND_PEEK_UP { 1000 } else { -1000 };
            a.cfg.island_peek_ms = (a.cfg.island_peek_ms + d).clamp(0, 20000);
            a.island.dirty = true;
            a.cfg.save();
        }
        MI_ISLAND_DOT_COLOR => {
            a.cfg.island_dot_color = config::next_color(a.cfg.island_dot_color);
            a.island.dirty = true;
            a.cfg.save();
        }
        MI_ISLAND_ROT_DOWN | MI_ISLAND_ROT_UP => {
            let d = if cmd == MI_ISLAND_ROT_UP { 5 } else { -5 };
            a.cfg.island_rotate = (a.cfg.island_rotate + d).clamp(0, 600);
            a.island.dirty = true;
            a.cfg.save();
        }
        MI_ISLAND_OP_DOWN | MI_ISLAND_OP_UP => {
            // 步长 13 ≈ 5%（面板显示的是 alpha×100/255）。下限 16 而不是 0：
            // 调到 0 胶囊彻底看不见，而岛是**鼠标穿透**的 —— 用户只能靠
            // 设置面板找回来。留 6% 至少还能看出「这儿有个东西」。
            let d = if cmd == MI_ISLAND_OP_UP { 13 } else { -13 };
            a.cfg.island_body_alpha = (a.cfg.island_body_alpha + d).clamp(16, 255);
            a.island.dirty = true;
            a.cfg.save();
        }
        MI_ISLAND_W_DOWN | MI_ISLAND_W_UP => {
            let d = if cmd == MI_ISLAND_W_UP { 20 } else { -20 };
            a.cfg.island_w = (a.cfg.island_w + d).clamp(120, 2000);
            island::recompute(a);
            a.island.dirty = true;
            a.cfg.save();
        }
        MI_ISLAND_H_DOWN | MI_ISLAND_H_UP => {
            let d = if cmd == MI_ISLAND_H_UP { 16 } else { -16 };
            // 下限 = 内容需求（消息行 + 底边距），低于它展开态会裁行。
            // 与 config 加载、面板「−」置灰共用 `EXPAND_MIN_H` 一个定义。
            a.cfg.island_h = (a.cfg.island_h + d).clamp(crate::island::geom::EXPAND_MIN_H, 900);
            island::recompute(a);
            a.island.dirty = true;
            a.cfg.save();
        }
        MI_ISLAND_BODY_COLOR => {
            a.cfg.island_body_color = config::next_island_body_color(a.cfg.island_body_color);
            a.island.dirty = true;
            a.cfg.save();
        }
        MI_ISLAND_ACCENT_COLOR => {
            a.cfg.island_accent_color =
                config::next_island_accent_color(a.cfg.island_accent_color);
            a.island.dirty = true;
            a.cfg.save();
        }
        MI_FONT => {
            a.cfg.font = config::next_font(&a.cfg.font);
            a.rebuild_fonts();
            a.cfg.save();
            render_and_present(a);
        }
        MI_COLOR => {
            a.cfg.color = config::next_color(a.cfg.color);
            a.cfg.save();
            render_and_present(a);
        }
        MI_DATE_COLOR => {
            a.cfg.date_color = config::next_date_color(a.cfg.date_color);
            a.cfg.save();
            render_and_present(a);
        }
        MI_CLOCK_RESET => {
            a.cfg.clock_dx = 0;
            a.cfg.clock_dy = 0;
            a.cfg.save();
            render_and_present(a);
        }
        MI_SPEED_UP => {
            a.cfg.speed = (a.cfg.speed + 10).min(400);
            a.cfg.save();
        }
        MI_SPEED_DOWN => {
            a.cfg.speed = (a.cfg.speed - 10).max(10);
            a.cfg.save();
        }
        MI_PREV => on_hotkey(a, HK_PREV),
        MI_NEXT => on_hotkey(a, HK_NEXT),
        MI_FAV => favorite(a),
        MI_PAUSE => {
            a.cfg.paused = !a.cfg.paused;
            // 暂停必须落在稳定画面上：把在飞的过渡收掉，避免停在半透明/半位移。
            if a.cfg.paused {
                a.trans.settle();
            } else {
                a.phase = 0.0;
                a.pulse = 0.0;
                if a.primed {
                    a.m = marquee_band_w(&a.cfg);
                }
            }
            a.cfg.save();
            render_and_present(a);
        }
        MI_FAV_ONLY => {
            a.cfg.fav_only = !a.cfg.fav_only;
            a.reload_library();
            a.cfg.save();
            render_and_present(a);
        }
        MI_BACKUP => match crate::backup::backup(&crate::util::data_dir()) {
            Ok(p) => toast(a, &format!("已备份到 {}", p.display())),
            Err(e) => toast(a, &format!("备份失败：{e}")),
        },
        MI_DIAG => match export_diagnostics(a) {
            Ok(p) => toast(a, &format!("诊断报告已写入 {}", p.display())),
            Err(e) => toast(a, &format!("诊断报告写入失败：{e}")),
        },
        MI_RELOAD => {
            let old = a.cfg.clone();
            let new = Config::load();
            a.cfg = new;
            a.cfg.x = old.x;
            a.cfg.y = old.y;
            a.reload_library();
            // 配置里的字号/字体/颜色可能都变了，无条件重建一次最省心
            a.rebuild_fonts();
            sync_from_panel(a);
            apply_mode(a);
        }
        MI_RESET => {
            // 回「挂件当前所在屏」的右上角，不是主屏 —— 副屏用户点这条菜单
            // 期待的是回到自己那块屏的角上。
            // 方向必须是「cfg 设目标 → apply_geometry 落到窗口」。这里曾经调
            // `sync_from_panel`，而它是**反向**同步（读窗口真实矩形覆写 cfg），
            // 刚设好的目标位置立刻被窗口旧位置改回去 —— 菜单项成了空操作。
            let (wa, _) = work_area_at(a.cfg.x, a.cfg.y, a.cfg.w);
            a.cfg.x = wa.right - a.cfg.w - 24;
            a.cfg.y = wa.top + 24;
            apply_geometry(a, false, true);
            a.cfg.save();
        }
        // 分辨率适配：把窗口 + 字号按当前屏重落一遍，再回到该屏右上角
        MI_FIT => {
            a.fit_to_screen();
            apply_geometry(a, true, true);
            render_and_present(a);
            a.cfg.save();
        }
        // 跟随屏幕 DPI：开关两个方向都同比缩放窗口（见 `App::set_dpi_auto`）
        MI_DPI => {
            let on = !a.cfg.dpi_auto;
            let dpi = window_dpi(a.text);
            a.set_dpi_auto(on, dpi);
            apply_geometry(a, true, true);
            render_and_present(a);
            a.cfg.save();
        }
        MI_QUIT => quit(a),
        c if (MI_ANIM_BASE..MI_ANIM_BASE + 4).contains(&c) => {
            a.cfg.anim = Anim::all()[(c - MI_ANIM_BASE) as usize];
            a.phase = 0.0;
            a.pulse = 0.0;
            a.m = 0.0;
            // 同 HK_ANIM：落定在飞的过渡再切
            a.trans.settle();
            a.cfg.save();
            render_and_present(a);
        }
        _ => {}
    }
}

fn quit(a: &mut App) {
    unregister_hotkeys(a);
    del_tray(a);
    // 设置面板（如果开着）先收掉：它的 WM_DESTROY 会释放自己那一堆字体/刷子句柄
    config_ui::close(a);
    unsafe {
        KillTimer(a.text, TIMER_ID);
        a.cfg.save();
        let text = a.text;
        let panel = a.panel;
        let host = a.menu_host;
        a.text = null_mut();
        a.panel = null_mut();
        a.menu_host = null_mut();
        if !text.is_null() {
            DestroyWindow(text);
        }
        if !panel.is_null() {
            DestroyWindow(panel);
        }
        // 属主可能在菜单弹出期间被建出来；退出时照样收掉，
        // 让诊断里的「窗口计数」在退出路径上也干净。
        menu_host::destroy(host);
    }
    std::process::exit(0);
}

// ---------------------------------------------------------------- 窗口位置同步

/// 底板是唯一权威：窗口被系统移动/缩放后，把结果同步到配置。
///
/// **不在这里出帧**：位置变化不影响像素内容，而出帧要 700µs。
/// 只有尺寸变了才标脏 —— 那才是真的需要重画的事。
fn sync_from_panel(a: &mut App) {
    if a.panel.is_null() {
        return;
    }
    let mut r: RECT = unsafe { zeroed() };
    if unsafe { GetWindowRect(a.panel, &mut r) } == 0 {
        return;
    }
    let w = (r.right - r.left).max(MIN_W);
    let h = (r.bottom - r.top).max(MIN_H);
    let resized = w != a.cfg.w || h != a.cfg.h;
    a.cfg.x = r.left;
    a.cfg.y = r.top;
    a.cfg.w = w;
    a.cfg.h = h;
    if resized {
        a.m = 0.0;
        // 窗口缩小后时间日期块可能露在外面，重新夹一次
        normalize_clock_offset(a);
        mark_dirty(a);
    }
}

// ---------------------------------------------------------------- 窗口过程

/// 从 `WM_LBUTTONDOWN` / `WM_MOUSEMOVE` 的 lparam 取出坐标。
///
/// 刻意用消息自带的坐标而不是 `GetCursorPos()`：一是消息里的坐标永远不会
/// 因为队列延迟而和事件本身错位，二是这样一条鼠标消息就是**自包含**的，
/// 可以用 `PostMessage` 把「按下 → 移动 → 松开」整条链路灌进去做端到端验证
/// （见 `--dragtest`），不必去注入系统级鼠标。
fn lparam_pt(lparam: LPARAM) -> (i32, i32) {
    let x = (lparam & 0xFFFF) as u16 as i16 as i32;
    let y = ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32;
    (x, y)
}

/// 窗口的实际矩形。断言要验的是「窗口真的动了」，所以必须读内核，
/// 不能拿配置里的 x/y 自证。
pub(crate) unsafe fn win_rect(hwnd: HWND) -> RECT {
    let mut r: RECT = zeroed();
    GetWindowRect(hwnd, &mut r);
    r
}

/// 光标当前的屏幕坐标。只在 `WM_SETCURSOR` 里用 —— 那个消息不带坐标。
fn cursor_screen() -> (i32, i32) {
    let mut pt: POINT = unsafe { zeroed() };
    unsafe { GetCursorPos(&mut pt) };
    (pt.x, pt.y)
}

/// 客户区坐标 → 边框命中码。不在边框带里就返回 `HTCLIENT`（= 中间，整体拖动）。
pub(crate) fn edge_zone(a: &App, cx: i32, cy: i32) -> isize {
    let m = (8.0 * a.cfg.eff_scale()).round().max(6.0) as i32;
    let (w, h) = (a.cfg.w, a.cfg.h);
    let left = cx < m;
    let right = cx >= w - m;
    let top = cy < m;
    let bottom = cy >= h - m;
    match (left, right, top, bottom) {
        (true, _, true, _) => HTTOPLEFT,
        (_, true, true, _) => HTTOPRIGHT,
        (true, _, _, true) => HTBOTTOMLEFT,
        (_, true, _, true) => HTBOTTOMRIGHT,
        (true, _, _, _) => HTLEFT,
        (_, true, _, _) => HTRIGHT,
        (_, _, true, _) => HTTOP,
        (_, _, _, true) => HTBOTTOM,
        _ => HTCLIENT,
    }
}

/// 命中码 → 光标形状。
///
/// 自己接管鼠标之后就没有系统的光标反馈了，这里得补回来：
/// 光靠一个「能拖」的窗口，用户根本看不出四条边是可以拉的。
fn drag_cursor(zone: isize) -> HCURSOR {
    let res = match zone {
        HTLEFT | HTRIGHT => IDC_SIZEWE_RES,
        HTTOP | HTBOTTOM => IDC_SIZENS_RES,
        HTTOPLEFT | HTBOTTOMRIGHT => IDC_SIZENWSE_RES,
        HTTOPRIGHT | HTBOTTOMLEFT => IDC_SIZENESW_RES,
        _ => IDC_SIZEALL_RES,
    };
    unsafe { LoadCursorW(null_mut(), res) }
}

/// 松开鼠标的收尾：放手、把最终画面**强制**出一帧、落盘。
///
/// 强制出帧这一步不能省：节流最多会把重绘推迟 8ms，
/// 而「松手」是用户眼里的动作终点，画面必须立刻停在最终状态。
fn end_grab(a: &mut App) {
    a.grab = Grab::None;
    unsafe {
        ReleaseCapture();
        // 定时器调回平时的 25Hz（拖动期间被收紧到 DRAG_TICK_MS）
        SetTimer(a.text, TIMER_ID, TIMER_MS, None);
    }
    // 位置也要**强制**落一次：节流最多把它推迟 8ms，而松手是动作终点 ——
    // 窗口必须停在你松手的地方，不能还欠着最后那一格。
    apply_geometry(a, true, true);
    a.cfg.save();
    // 缩放之后文字层的尺寸是**由帧缓冲决定的**（`present` 里 ULW 一起提交），
    // 所以收尾这一帧必须无条件出：`dirty` 可能已经被前一条消息的那一帧清掉了，
    // 而那一帧用的是旧尺寸。这里多花 700µs，换「松手后画面一定是对的」。
    mark_dirty(a);
    render_if_due(a, true);
}

/// 拖动时的边界保护：至少留 `KEEP_VISIBLE` px 在工作区内。
///
/// 和 `clamp_to_work_area` 共用同一份判据 —— 「别把挂件拖丢」这条约定
/// 在拖动路径和启动路径上必须是同一个，而且**四条边同一个数**：
/// 上下留 40、左右留 80 这种不对称，会让用户拖到下方时以为「到底了」，
/// 再往下却还能走一点，手感上就是「边界会漂」。
/// 上边例外：顶不允许越过工作区上沿（挂件没有标题栏，钻上去就抓不回来了）。
pub(crate) fn clamp_keep_visible(x: i32, y: i32, w: i32) -> (i32, i32) {
    let (wa, _) = work_area_at(x, y, w);
    let lo_x = wa.left - w + KEEP_VISIBLE;
    let hi_x = (wa.right - KEEP_VISIBLE).max(lo_x);
    let lo_y = wa.top;
    let hi_y = (wa.bottom - KEEP_VISIBLE).max(lo_y);
    (x.clamp(lo_x, hi_x), y.clamp(lo_y, hi_y))
}

/// 客户区坐标是否落在时间日期块上（含 4px 容差，块很小时也抓得住）
pub(crate) fn hit_clock_client(a: &App, cx: i32, cy: i32) -> bool {
    if !a.clock_visible() {
        return false;
    }
    cx >= a.clock_rect.left - 4
        && cx < a.clock_rect.right + 4
        && cy >= a.clock_rect.top - 4
        && cy < a.clock_rect.bottom + 4
}

/// 把时间日期块的偏移夹回窗口内，并**回写夹取后的实际偏移**。
///
/// 回写是拖动跟手的关键：如果只夹显示位置、不回写偏移，
/// 用户拖出边界 200px 再往回拖时，前 200px 光标动了但内容不动 —— 空走。
///
/// 缩放拖动时每条鼠标消息都会走到这儿，而夹取公式是纯算术、不碰 GDI ——
/// 真正的开销在量文字（`layout_clock`），那份已由 `layout_clock` 的结果
/// 缓存挡掉。见 `layout_clock` 的文档：`--perf` 实测缩放拖动 1052µs/条
/// 就是这么降到 ~300µs 的。
pub(crate) fn normalize_clock_offset(a: &mut App) {
    // 默认位置**由量几何的那一侧给出**（`ClockBox::dx0/dy0`）：横版是
    // 「左侧垂直居中」，竖版页头是「贴天头」。以前这里自己再写一遍公式，
    // 竖版页头改到顶部之后就必然对不上 —— 一处定义，两处使用。
    let cb = layout_clock(a);
    if !cb.show {
        return;
    }

    let (w, h) = (a.cfg.w, a.cfg.h);
    let (x0, y0) = (cb.dx0, cb.dy0);
    let max_x = (w - cb.w).max(0);
    let max_y = (h - cb.h).max(0);
    let x = (x0 + a.cfg.clock_dx).clamp(0, max_x);
    let y = (y0 + a.cfg.clock_dy).clamp(0, max_y);
    a.cfg.clock_dx = x - x0;
    a.cfg.clock_dy = y - y0;
}

pub(crate) unsafe extern "system" fn panel_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // 重入（菜单 / ShowWindow / DestroyWindow 的同步回调）时已经有外层
    // `&mut App`，这里必须返回默认处理，不能再造第二份可变引用。
    match with_app(|a| panel_proc_inner(a, hwnd, msg, wparam, lparam)) {
        Some(r) => r,
        None => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn panel_proc_inner(
    a: &mut App,
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    MSG_TALLY[(msg & 0xFF) as usize].fetch_add(1, Ordering::Relaxed);
    if msg == WM_MOUSEMOVE {
        // 恒在最前面计数（在「是不是布局模式」的判断**之前**）：
        // 这里问的是「消息有没有落到我们手里」，与之后怎么处理无关。
        WIDGET_MOUSE_HITS.fetch_add(1, Ordering::Relaxed);
    }
    match msg {
        WM_ERASEBKGND => 1,

        // 底板在 Acrylic 模式下必须什么都不画。
        // 唯一的例外是降级路径（整窗 alpha），那时需要自绘一块实心深色板。
        WM_PAINT => {
            if a.glass == Glass::Layered {
                let mut ps: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(hwnd, &mut ps);
                let mut rc: RECT = zeroed();
                GetClientRect(hwnd, &mut rc);
                let brush = CreateSolidBrush(
                    (PANEL_B << 16) | (PANEL_G << 8) | PANEL_R,
                );
                FillRect(dc, &rc, brush);
                DeleteObject(brush as HGDIOBJ);
                EndPaint(hwnd, &ps);
            } else {
                let mut ps: PAINTSTRUCT = zeroed();
                BeginPaint(hwnd, &mut ps);
                EndPaint(hwnd, &ps);
            }
            0
        }

        WM_HOTKEY => {
            on_hotkey(a, wparam as i32);
            0
        }

        WM_TRAY => {
            let ev = lparam as u32;
            if ev == WM_LBUTTONUP || ev == WM_RBUTTONUP || ev == WM_LBUTTONDBLCLK {
                show_menu(a);
            }
            0
        }

        // ---- 布局模式：鼠标全部由我们自己接管。
        //
        // 关键改动：这里不再返回 HTCAPTION / HTLEFT，把拖动交给系统。
        // 系统的模态移动循环是个黑盒 —— 它每收到一个鼠标消息就 SetWindowPos 一次，
        // 每次都连带触发我们的 WM_MOVE（旧实现在那里跑整帧重绘），
        // 而且整个循环期间我们没有任何节流的机会，消息堆积了也只能干等。
        // 自己接管之后，「收状态」和「出画面」才拆得开（见 `render_if_due`）。
        //
        // 光标形状用 WM_SETCURSOR 单独给：自己接管不等于丢掉「哪里能拖」的提示。
        WM_NCHITTEST => {
            if !a.cfg.layout_mode || a.cfg.w <= 0 {
                return HTTRANSPARENT;
            }
            HTCLIENT
        }

        WM_SETCURSOR => {
            if !a.cfg.layout_mode {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            // 抓取期间直接用按下那一刻算好的命中带：`WM_SETCURSOR` 每次鼠标移动
            // 都会来一发，这时候再去 `GetCursorPos` + `GetWindowRect` 纯属白花 ——
            // 窗口正在动，现算的结果还不如按下的那一刻准。
            let zone = if a.grab != Grab::None {
                a.grab_zone
            } else {
                let (sx, sy) = cursor_screen();
                let mut r: RECT = zeroed();
                GetWindowRect(hwnd, &mut r);
                edge_zone(a, sx - r.left, sy - r.top)
            };
            SetCursor(drag_cursor(zone));
            1
        }

        WM_LBUTTONDOWN => {
            if a.cfg.layout_mode {
                let (cx, cy) = lparam_pt(lparam);
                let zone = edge_zone(a, cx, cy);
                a.grab_zone = zone;
                // 锚点一律取**窗口真实位置**。按下那一刻它当然等于 `cfg.x/y`，
                // 但拖动一开始两者就会脱钩（节流让 `cfg` 领先于实际窗口），
                // 所以基准从第一刻起就只认一个来源，免得中途换算法。
                let wp = win_rect(hwnd);
                // 时间日期块优先于缩放边框：块本来就贴在左内边距上，
                // 常常和左边框的命中带重叠，先判它才拖得动
                if zone == HTCLIENT && hit_clock_client(a, cx, cy) {
                    a.grab = Grab::Clock;
                    a.grab_pt = (cx, cy);
                    a.grab_off = (a.cfg.clock_dx, a.cfg.clock_dy);
                } else if zone == HTCLIENT {
                    a.grab = Grab::Move;
                    a.grab_pt = (wp.left + cx, wp.top + cy);
                    a.grab_win = (wp.left, wp.top, a.cfg.w, a.cfg.h);
                } else {
                    a.grab = Grab::Edge(zone);
                    a.grab_pt = (wp.left + cx, wp.top + cy);
                    a.grab_win = (wp.left, wp.top, a.cfg.w, a.cfg.h);
                }
                SetCapture(hwnd);
                // 拖动期间把定时器收紧到位置节拍，好把「停手后欠着的那一格」补上
                // （见 `DRAG_TICK_MS`）。松手时 `end_grab` 会调回去。
                SetTimer(a.text, TIMER_ID, DRAG_TICK_MS, None);
            }
            0
        }

        WM_MOUSEMOVE => {
            if a.grab == Grab::None {
                return 0;
            }
            // 消息里的**客户区**坐标。窗口原点在拖动前是已知的实际值
            // （位置由我们自己应用，`cfg.x/y` 永远等于窗口实际位置），
            // 所以屏幕坐标 = 原点 + 客户区坐标，不需要 GetCursorPos()。
            // 这也是整条链路可复现、能用 PostMessage 灌消息做端到端验证的原因。
            let (cx, cy) = lparam_pt(lparam);
            match a.grab {
                // 挪整个挂件：只改窗口位置，一个像素都不用重画
                Grab::Move => {
                    // 客户区坐标是相对**窗口真实位置**的，所以重建屏幕坐标必须用
                    // 真实位置 —— 不能用 `cfg.x`：节流会让它领先于窗口（那是
                    // 「目标」，不是「现状」）。两者一旦脱钩，每条消息都会把上一条
                    // 欠着的位移重新算一遍，误差按三角数放大 ——
                    // 表现就是「鼠标一动，窗口乱飘」（`--dragtest` 第 ⑧ 条守着它）。
                    let wp = win_rect(hwnd);
                    let (sx, sy) = (wp.left + cx, wp.top + cy);
                    let raw = (
                        a.grab_win.0 + (sx - a.grab_pt.0),
                        a.grab_win.1 + (sy - a.grab_pt.1),
                    );
                    let (nx, ny) = clamp_keep_visible(raw.0, raw.1, a.cfg.w);
                    if (nx, ny) != raw {
                        // 被边界夹住了：把基准重置到「当前实际位置 + 当前光标」，
                        // 这样光标一往回走就立刻跟手，不会有一段拖不动的空走
                        a.grab_win = (wp.left, wp.top, a.cfg.w, a.cfg.h);
                        a.grab_pt = (sx, sy);
                    }
                    request_move(a, nx, ny);
                    if a.legacy_drag {
                        // 旧实现：位置变了也照样整帧重绘一遍（~700µs）。
                        // 这些工作对「窗口在哪」毫无贡献，纯属白花 ——
                        // 而且它把每条消息的成本抬到 1ms 量级，
                        // 1000Hz 鼠标一来就在队列里排队，手感就是「粘」。
                        render_and_present(a);
                    }
                }
                // 挪时间日期块：块的位置变了 → 内容要重画，但按节拍出
                Grab::Clock => {
                    a.cfg.clock_dx = a.grab_off.0 + (cx - a.grab_pt.0);
                    a.cfg.clock_dy = a.grab_off.1 + (cy - a.grab_pt.1);
                    normalize_clock_offset(a);
                    mark_dirty(a);
                    render_if_due(a, false);
                }
                // 缩放：底板立刻跟手，内容按节拍重画
                Grab::Edge(zone) => {
                    // 同 `Grab::Move`：屏幕坐标必须由窗口**真实位置**重建。
                    // 缩放时窗口原点本来就会动（拖左边/上边），这里更容易踩坑。
                    let wp = win_rect(hwnd);
                    let (sx, sy) = (wp.left + cx, wp.top + cy);
                    let dx = sx - a.grab_pt.0;
                    let dy = sy - a.grab_pt.1;
                    let (mut x, mut y, mut w, mut h) = a.grab_win;
                    if matches!(zone, HTLEFT | HTTOPLEFT | HTBOTTOMLEFT) {
                        x += dx;
                        w -= dx;
                    }
                    if matches!(zone, HTRIGHT | HTTOPRIGHT | HTBOTTOMRIGHT) {
                        w += dx;
                    }
                    if matches!(zone, HTTOP | HTTOPLEFT | HTTOPRIGHT) {
                        y += dy;
                        h -= dy;
                    }
                    if matches!(zone, HTBOTTOM | HTBOTTOMLEFT | HTBOTTOMRIGHT) {
                        h += dy;
                    }
                    // 夹到最小尺寸时把被顶住的那条边锚回去，
                    // 否则继续拖会把窗口「推」着走
                    if w < MIN_W {
                        if matches!(zone, HTLEFT | HTTOPLEFT | HTBOTTOMLEFT) {
                            x -= MIN_W - w;
                        }
                        w = MIN_W;
                    }
                    if h < MIN_H {
                        if matches!(zone, HTTOP | HTTOPLEFT | HTTOPRIGHT) {
                            y -= MIN_H - h;
                        }
                        h = MIN_H;
                    }
                    // **上界 = 工作区**（P1-D）。只夹下界时用户能把挂件拖成
                    // 2926px 宽（屏只有 2560），右侧永远在屏外，而且这个尺寸
                    // 会被 `end_grab → save` 立刻写进配置。
                    // 上界与 `clamp_to_work_area` 共用 `resize_limits`，
                    // 拖的时候和重启之后是同一套标准。
                    let (max_w, max_h) = resize_limits(&a.cfg);
                    if w > max_w {
                        if matches!(zone, HTLEFT | HTTOPLEFT | HTBOTTOMLEFT) {
                            x += max_w - w;
                        }
                        w = max_w;
                    }
                    if h > max_h {
                        if matches!(zone, HTTOP | HTTOPLEFT | HTTOPRIGHT) {
                            y += max_h - h;
                        }
                        h = max_h;
                    }
                    apply_resize(a, x, y, w, h);
                }
                Grab::None => {}
            }
            0
        }

        WM_LBUTTONUP => {
            if a.grab != Grab::None {
                end_grab(a);
            }
            0
        }

        // 捕获被别处抢走（比如别的窗口弹出来）也要按「松开」收尾，
        // 否则会留下一个永远不放手的拖动状态
        WM_CAPTURECHANGED => {
            if a.grab != Grab::None {
                end_grab(a);
            }
            0
        }

        // 位置变化不需要重绘 —— 像素内容和窗口在哪无关。
        // 尺寸变了才标脏（那是真的会改画面的事），重绘交给节拍。
        //
        // 拖动期间**必须跳过**：这两条消息是我们自己 `SetWindowPos` 的**回声**，
        // 而拖动中窗口位置是「按节拍落定」的 —— 回声里的位置正是上一拍的旧值，
        // 让它盖回 `cfg.x/y`，等于每条消息都把状态拽回 8ms 前，
        // 拖动判定和边界夹取会集体以旧位置为基准。
        WM_MOVE | WM_SIZE => {
            if a.grab == Grab::None {
                sync_from_panel(a);
            }
            0
        }

        WM_ENTERSIZEMOVE => 0,
        WM_EXITSIZEMOVE => {
            a.cfg.save();
            0
        }

        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            // 分辨率 / 缩放变了：先按新 DPI 算字号与目标尺寸，再夹进新工作区，
            // 最后**单向应用**到窗口。
            //
            // 这里绝不能调 `sync_from_panel`：它从窗口读回旧矩形并覆盖 cfg，
            // 会把刚算好的 DPI 缩放和夹取结果原样撤销。`WM_MOVE/WM_SIZE` 的
            // 回声在 grab==None 时本来就会做反向同步，不需要在这里补。
            let dpi = window_dpi(a.text);
            a.apply_dpi(dpi);
            clamp_to_work_area(&mut a.cfg);
            apply_geometry(a, true, true);
            render_and_present(a);
            a.cfg.save();
            0
        }

        // 拖到另一块 DPI 不同的屏：字号与窗口必须一起换挡，
        // 否则不是字变小就是版式挤爆。`lparam` 里是系统建议的矩形，
        // 这里不采纳 —— 我们的尺寸由自己的缩放规则决定，采納它等于
        // 把用户调好的比例交给系统重排一次。
        WM_DPICHANGED => {
            // wParam 低字是 96 基准的原始 DPI（150% 屏 = 144），换成百分数再比。
            let dpi = crate::ffi::dpi_pct((wparam & 0xFFFF) as i32);
            if a.apply_dpi(dpi) {
                apply_geometry(a, true, true);
                render_and_present(a);
                a.cfg.save();
            }
            0
        }

        WM_CLOSE => {
            quit(a);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub(crate) unsafe extern "system" fn text_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match with_app(|a| text_proc_inner(a, hwnd, msg, wparam, lparam)) {
        Some(r) => r,
        None => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn text_proc_inner(
    a: &mut App,
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_MOUSEMOVE {
        // 文字层理论上永远收不到鼠标移动（layered + transparent）。
        // 收得到就说明穿透破了 —— `--clicktest` 把它当失败信号。
        WIDGET_MOUSE_HITS.fetch_add(1, Ordering::Relaxed);
    }
    match msg {
        WM_ERASEBKGND => 1,
        WM_TIMER => {
            if wparam == TIMER_ID {
                tick(a);
            }
            0
        }
        WM_PAINT => {
            // 分层窗口的内容全部来自 UpdateLayeredWindow，
            // 这里只要把重绘请求消化掉、顺带补一帧
            let mut ps: PAINTSTRUCT = zeroed();
            BeginPaint(hwnd, &mut ps);
            EndPaint(hwnd, &ps);
            render_and_present(a);
            0
        }
        // 文字窗口恒穿透：它只负责显示，绝不参与命中
        WM_NCHITTEST => HTTRANSPARENT,
        WM_CLOSE => {
            quit(a);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
