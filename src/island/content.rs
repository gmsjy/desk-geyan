//! 灵动岛的内容：收集什么、格式化成什么。
//!
//! 这一层是「岛有什么可显示」的唯一出处。**格言不在其中** —— 那是另一个
//! 功能的职责（规格书 §1）。这里的每一项都来自系统 API 或外部事件文件，
//! 任何一项失败都只影响它自己（返回 `None`，绘制端整项跳过），
//! 绝不显示 `0%` 这种假数据。

use std::path::PathBuf;

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::GetSystemTimes;

use crate::util::{data_dir, fmt_clock, fmt_date};

/// 收起态可以出现的片段。顺序即显示顺序（来自 `island_items`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Item {
    Clock,
    Dot,
    Cpu,
    Mem,
    Battery,
    Msg,
}

pub const ALL_ITEMS: [(&str, Item); 6] = [
    ("clock", Item::Clock),
    ("dot", Item::Dot),
    ("cpu", Item::Cpu),
    ("mem", Item::Mem),
    ("battery", Item::Battery),
    ("msg", Item::Msg),
];

/// 启用哪些片段。**`order` 是显示顺序的真相**，那几个 bool 只是给人读的
/// 便捷视图 —— 早期版本只用 bool，结果 `island_items=cpu,clock` 画出来
/// 仍然是 clock 在前（顺序被 `segments` 里写死），配置形同虚设。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Items {
    pub order: Vec<Item>,
    pub clock: bool,
    pub dot: bool,
    pub cpu: bool,
    pub mem: bool,
    pub battery: bool,
    pub msg: bool,
}

pub fn item_name(item: Item) -> &'static str {
    match item {
        Item::Clock => "clock",
        Item::Dot => "dot",
        Item::Cpu => "cpu",
        Item::Mem => "mem",
        Item::Battery => "battery",
        Item::Msg => "msg",
    }
}

impl Items {
    fn set(&mut self, item: Item, on: bool) {
        match item {
            Item::Clock => self.clock = on,
            Item::Dot => self.dot = on,
            Item::Cpu => self.cpu = on,
            Item::Mem => self.mem = on,
            Item::Battery => self.battery = on,
            Item::Msg => self.msg = on,
        }
    }

    /// 未知名字静默忽略（向前兼容）；重复名字只保留第一次出现的位置。
    pub fn parse(s: &str) -> Items {
        let mut it = Items::default();
        for part in s.split(',') {
            let p = part.trim();
            for (name, item) in ALL_ITEMS {
                if p.eq_ignore_ascii_case(name) && !it.order.contains(&item) {
                    it.order.push(item);
                    it.set(item, true);
                }
            }
        }
        // 空集合回落到 `clock`，否则岛会变成一条没有任何内容的空胶囊
        if it.order.is_empty() {
            it.order.push(Item::Clock);
            it.clock = true;
        }
        it
    }

    /// 规范形式（按显示顺序）。写回配置用。
    pub fn to_csv(&self) -> String {
        self.order
            .iter()
            .map(|i| item_name(*i))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// 一次采样的环境开关。**刻意是窄结构而不是 `&Config`**：调用方要同时
/// 可变借用 `Island`，`&a.cfg` 会和 `&mut App` 打架（这一版踩过三次）。
#[derive(Clone, Copy, Debug)]
pub struct Opts {
    /// 时钟是否显示秒（`cfg.seconds`）
    pub seconds: bool,
    /// 是否统计未读数（`island_unread`）
    pub unread: bool,
    /// 是否**消费**事件文件（真运行 `true`；`--dump-island` / `--islandtest` `false`）
    pub consume: bool,
    /// 归档保留（`island_events_keep`）。`false` = 读完即删
    pub keep: bool,
}

impl Default for Opts {
    fn default() -> Opts {
        // 默认值给「真运行」之外最安全的组合：**不消费**磁盘、计数开。
        // 单测造夹具时不会碰用户目录，也不用逐条写字段。
        Opts {
            seconds: false,
            unread: true,
            consume: false,
            keep: true,
        }
    }
}

/// 一次采样的结果。`clock` / `date` 每拍更新（格式化是微秒级），
/// 其余 1Hz 更新 —— 它们背后是真实系统调用，不该 25Hz 去问。
#[derive(Clone, Debug)]
pub struct Samples {
    pub clock: String,
    pub date: String,
    pub cpu: Option<u32>,
    pub mem: Option<u32>,
    /// `(百分比, 是否充电中)`
    pub battery: Option<(u32, bool)>,
    /// 最近一条消息正文
    pub msg: Option<String>,
    /// 未读条数（封顶 99）
    pub unread: u32,
    /// `island_unread`：`false` 时收起态显示正文而不是 `MSG n`。
    ///
    /// 放在采样里而不是当参数传：`segments()` 的调用点有五处（排版 / 估算 /
    /// 导出 / 测试），多一个参数就有五处要记得同步；而「这一轮是不是在计数」
    /// 本来就是采样的属性。
    pub counting: bool,
    /// `island_events_keep=0` 时用进程内累计消费量当未读数
    /// （归档被删了，`seen\` 里没有历史可数）
    pub(crate) consumed: u32,
    /// 上次系统采样的「当日秒数」：同一个值内不重复采
    ///
    /// `pub(crate)` 是为了让别的模块能用 `Samples { ..Default::default() }`
    /// 造测试夹具 —— 私有字段会挡住 FRU 语法。
    pub(crate) stamp: u64,
    /// `GetSystemTimes` 的上一帧累计量（idle, kernel, user），用于差分
    pub(crate) cpu_prev: Option<(u64, u64, u64)>,
}

impl Default for Samples {
    fn default() -> Samples {
        Samples {
            clock: String::new(),
            date: String::new(),
            cpu: None,
            mem: None,
            battery: None,
            msg: None,
            unread: 0,
            // 默认按**计数模式**：这是默认配置（`island_unread=true`）的行为，
            // 单测造出来的夹具也就默认落在用户会看到的那一档上。
            counting: true,
            consumed: 0,
            stamp: 0,
            cpu_prev: None,
        }
    }
}

fn filetime_u64(f: &FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

/// CPU 占用率。`GetSystemTimes` 给的是**开机以来的累计量**，必须两次差分；
/// `kernel` 已含 `idle`，所以总量 = `kernel + user`，忙 = 总量 - idle 增量。
///
/// 首次调用没有基准，返回 `None`（界面显示 `CPU --` 而不是一个假 0%）：
/// 直接拿累计量当「一整段时间」算出来的数字毫无意义。
fn cpu_pct(prev: &mut Option<(u64, u64, u64)>) -> Option<u32> {
    let mut idle: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } != 0;
    if !ok {
        return None;
    }
    let cur = (filetime_u64(&idle), filetime_u64(&kernel), filetime_u64(&user));
    let base = prev.replace(cur)?;
    let di = cur.0.saturating_sub(base.0);
    let dk = cur.1.saturating_sub(base.1);
    let du = cur.2.saturating_sub(base.2);
    let total = dk.saturating_add(du);
    if total == 0 {
        return None;
    }
    let busy = total.saturating_sub(di);
    Some(((busy.saturating_mul(100) / total) as u32).min(100))
}

fn mem_pct() -> Option<u32> {
    let mut st: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    st.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut st) } == 0 {
        return None;
    }
    Some(st.dwMemoryLoad.min(100))
}

fn battery_state() -> Option<(u32, bool)> {
    let mut st: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut st) } == 0 {
        return None;
    }
    // BatteryFlag 的 128 位表示「无系统电池」（台式机）
    const NO_BATTERY: u8 = 128;
    if st.BatteryFlag & NO_BATTERY != 0 {
        return None;
    }
    // 255 = 未知（某些虚拟机 / 远程桌面）
    if st.BatteryLifePercent == 255 {
        return None;
    }
    // AC 在线（1）时 AC 线接着，但「充电中」要看 BatteryFlag 的 8 位
    let charging = st.BatteryFlag & 8 != 0 || (st.ACLineStatus == 1 && st.BatteryFlag & 2 == 0);
    Some((st.BatteryLifePercent as u32, charging))
}

/// 事件目录：`%APPDATA%\DeskGeyan\events\*.msg`
pub fn events_dir() -> PathBuf {
    data_dir().join("events")
}

/// 归档目录：读过的事件移进这里（**不删**，留 7 天）
fn seen_dir(root: &std::path::Path) -> PathBuf {
    root.join("seen")
}

/// 「未读」的统计窗口：`seen\` 里 24h 内的条数。
const UNREAD_WINDOW: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
/// 归档保留天数
const KEEP_DAYS: u64 = 7;

/// 一次扫描的产出。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Events {
    /// 最新一条正文。**只有本次真的读到新文件才给 `Some`** —— 归档之后
    /// 正文仍要留在胶囊上，所以上一轮读到的那条由 `Samples` 自己保存着。
    pub latest: Option<String>,
    /// 本次新消费的条数（`keep=false` 时的未读数）
    pub fresh: u32,
    /// `seen\` 里 24h 内的条数（`keep=true` 时的未读数）
    pub recent: u32,
}

/// 消息正文的规范化：取**第一行**、剥 BOM、`正文\t来源` → `来源 · 正文`。
///
/// 为什么不是「整篇」：协议说文件名是 `yyyyMMddHHmmss_序号`，正文一整行。
/// 允许有第二行的理由只有一个 —— 用户用编辑器手写时多一个换行。
/// 多行拼接会让展开态的第三行变成「一行放不下的长文」，而它只能截断。
pub fn parse_body(text: &str) -> Option<String> {
    let line = crate::quotes::strip_bom(text)
        .lines()
        .find(|l| !l.trim().is_empty())?;
    // 注意顺序：**先按 `\t` 切、再各自 `trim`**。反过来（先整行 trim）会把
    // 「只有来源没有正文」的 `\tCI` 修剪成 `CI`，于是它被当成一条正文 ——
    // 一个空消息于是变成一条内容为「CI」的消息。
    match line.split_once('\t') {
        // `正文\t来源`：来源放前面，读起来是「谁说的 · 说了什么」
        Some((body, from)) => {
            let (body, from) = (body.trim(), from.trim());
            match (body.is_empty(), from.is_empty()) {
                (true, _) => None,
                (false, true) => Some(body.to_string()),
                (false, false) => Some(format!("{from} · {body}")),
            }
        }
        None => {
            let t = line.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
    }
}

/// 目录里所有 `*.msg` 的**文件名**，升序。
///
/// 排序用文件名而不是文件时间：文件名按 `yyyyMMddHHmmss` 前缀生成，
/// 同一秒内多条时用后缀序号区分；文件时间戳在 FAT / 网络盘上精度只有 2s，
/// 靠它排序会出现「先写的后到」。
fn msg_names(dir: &std::path::Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("msg")))
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
        .collect();
    names.sort();
    names
}

/// 目录里 `within` 时间窗内的 `*.msg` 条数。
fn count_recent(dir: &std::path::Path, within: std::time::Duration) -> u32 {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    rd.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("msg")))
        .filter(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age <= within)
        })
        .count() as u32
}

/// 扫一遍事件目录并（可选地）消费它。
///
/// # `consume` 为什么要显式开关
///
/// 「消费」= 把读过的文件搬进 `seen\`、按需删除、清理 7 天前的归档。这些都是
/// **写用户磁盘**的操作，而 `--islandtest` / `--dump-island` 会反复调 `tick`：
/// 让验证入口顺手搬走用户的真实消息，等于在「零副作用」这条家规上开了个洞，
/// 而且症状极隐蔽（消息文件莫名跑到 `seen\` 里去了）。
///
/// 所以消费是**调用方显式要求**的：真运行说 `true`，导出与自检说 `false`。
/// `consume=false` 时只读：`latest` 取文件名最大那一条，`recent` 退回「目录里的
/// 待读条数」—— 对导出无影响（导出把采样整块冻结了），对自检足够。
pub fn pump(root: &std::path::Path, consume: bool, keep: bool) -> Events {
    let names = msg_names(root);
    if !consume {
        let latest = names
            .last()
            .and_then(|n| std::fs::read_to_string(root.join(n)).ok())
            .and_then(|t| parse_body(&t));
        return Events {
            latest,
            fresh: 0,
            // 只读模式下「未读」退回待读条数（没有任何归档动作，数什么都不对）
            recent: (names.len() as u32).min(99),
        };
    }

    // `seen` 目录顺便建出来：用户要往里丢文件时不必自己 mkdir
    // （脚本一行 `echo > ...\events\xxx.msg` 在目录不存在时会直接失败）。
    let seen = seen_dir(root);
    if keep {
        let _ = std::fs::create_dir_all(&seen);
    }
    let mut latest = None;
    let mut fresh = 0u32;
    for n in &names {
        let src = root.join(n);
        let Ok(text) = std::fs::read_to_string(&src) else {
            continue; // 正被别的进程写（读到半个文件）时留到下一拍
        };
        if let Some(b) = parse_body(&text) {
            latest = Some(b);
        }
        fresh += 1;
        if keep {
            let dst = seen.join(n);
            // 同名归档已存在（同一秒重复跑脚本）时先清掉，否则 `rename` 会失败，
            // 然后这个文件会被**每一拍重复读一遍**，正文永远停在它上面。
            if dst.exists() {
                let _ = std::fs::remove_file(&dst);
            }
            if std::fs::rename(&src, &dst).is_err() {
                let _ = std::fs::copy(&src, &dst);
                let _ = std::fs::remove_file(&src);
            }
        } else {
            let _ = std::fs::remove_file(&src);
        }
    }
    // 清理**每拍都跑**：只在「有新消息」时清，等于把「留 7 天」变成
    // 「留 7 天或者直到下一条消息到来」—— 一个安静的用户永远等不到清理。
    if keep {
        prune_seen(&seen);
    }
    Events {
        latest,
        fresh,
        recent: if keep { count_recent(&seen, UNREAD_WINDOW).min(99) } else { 0 },
    }
}

/// 清掉超过 `KEEP_DAYS` 天的归档。失败静默 —— 清理不是功能，别为它报错。
fn prune_seen(seen: &std::path::Path) {
    let Ok(rd) = std::fs::read_dir(seen) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let max_age = std::time::Duration::from_secs(KEEP_DAYS * 24 * 3600);
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > max_age);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

impl Samples {
    /// 拍一次的采样。`now` 由调用方给（自检要能喂固定时刻）。
    ///
    /// 时钟/日期每拍重算字符串；其余按秒节流。**只在对应片段启用时采** ——
    /// 用户关掉 CPU 就一个系统调用都不该发生。
    pub fn update(
        &mut self,
        items: &Items,
        opts: Opts,
        now: &windows_sys::Win32::Foundation::SYSTEMTIME,
    ) {
        self.clock = fmt_clock(now, opts.seconds);
        self.date = fmt_date(now);
        self.counting = opts.unread;
        let stamp = (now.wDay as u64) * 86400
            + (now.wHour as u64) * 3600
            + (now.wMinute as u64) * 60
            + now.wSecond as u64;
        if stamp == self.stamp {
            return;
        }
        self.stamp = stamp;
        if items.cpu {
            self.cpu = cpu_pct(&mut self.cpu_prev);
        } else {
            self.cpu = None;
            self.cpu_prev = None;
        }
        if items.mem {
            self.mem = mem_pct();
        } else {
            self.mem = None;
        }
        if items.battery {
            self.battery = battery_state();
        } else {
            self.battery = None;
        }
        if opts.unread || items.msg {
            let ev = pump(&events_dir(), opts.consume, opts.keep);
            // 只有真读到新文件才覆盖：归档之后上一句还要留在胶囊上
            if ev.latest.is_some() {
                self.msg = ev.latest;
            }
            self.consumed = self.consumed.saturating_add(ev.fresh);
            self.unread = if !opts.unread {
                0
            } else if opts.keep {
                ev.recent
            } else {
                self.consumed.min(99)
            };
        } else {
            self.msg = None;
            self.unread = 0;
        }
    }
}

/// 一个收起态片段：文字 + 它该落在哪条掩码通道上。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seg {
    pub text: String,
    /// `true` = 主色通道（时钟），`false` = 次色通道
    pub main: bool,
    /// `true` = 这是一个圆点而不是文字（宽度固定 6，`text` 为空）
    pub dot: bool,
}

impl Seg {
    fn text(s: impl Into<String>, main: bool) -> Seg {
        Seg {
            text: s.into(),
            main,
            dot: false,
        }
    }
}

/// 把采样结果翻成「收起态要显示的那一串片段」。
///
/// 返回值里**已剔除不可用项**：没有电池就没有 Battery 片段，CPU 还没采到
/// 第二次就没有 Cpu 片段。宁可少一项，也不显示 `--` 占位 —— 一条随时会变
/// 的胶囊里，占位符和真数据混在一起看不出区别。
pub fn segments(s: &Samples, items: &Items, clock_short: bool) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::new();
    for it in &items.order {
        match it {
            Item::Clock => out.push(Seg::text(
                if clock_short { short_clock(&s.clock) } else { s.clock.clone() },
                true,
            )),
            // 无未读时不画点（而不是画一个"灰点"）：一条常驻胶囊里，
            // 任何常驻的装饰都会变成噪音。
            Item::Dot => {
                if s.unread > 0 {
                    out.push(Seg { text: String::new(), main: true, dot: true });
                }
            }
            Item::Cpu => {
                if let Some(c) = s.cpu {
                    out.push(Seg::text(format!("CPU {c}%"), false));
                }
            }
            Item::Mem => {
                if let Some(m) = s.mem {
                    out.push(Seg::text(format!("MEM {m}%"), false));
                }
            }
            Item::Battery => {
                if let Some((p, charging)) = s.battery {
                    out.push(Seg::text(
                        if charging { format!("+{p}%") } else { format!("{p}%") },
                        false,
                    ));
                }
            }
            Item::Msg => {
                if s.counting {
                    // 计数模式：只给条数。正文放展开态第三行 —— 收起态的胶囊
                    // 宽度是靠内容撑出来的，塞一句正文会把胶囊拉成一条横条。
                    if s.unread > 0 {
                        let n = if s.unread >= 99 { "99+".to_string() } else { s.unread.to_string() };
                        out.push(Seg::text(format!("MSG {n}"), false));
                    }
                } else if let Some(m) = &s.msg {
                    // 不计数（`island_unread=0`）：规格书 §4.11 的「只显示最近一条不计数」。
                    // 超宽由 `compact` 按整字截断加省略号，不会画到窗外。
                    out.push(Seg::text(m.clone(), false));
                }
            }
        }
    }
    out
}

/// `09:41:33` → `09:41`。收起态默认只到分：秒在 34px 的胶囊里跳变太吵，
/// 而展开态显示完整时间 —— 两处本来就不必一致。
pub fn short_clock(clock: &str) -> String {
    match clock.find(':') {
        Some(i) => clock[..i].to_string() + &clock[i..clock.len().min(i + 3)],
        None => clock.to_string(),
    }
}

/// 展开态的第 3 行：有消息就显示正文，否则 `None`（整行不画）。
pub fn msg_line(s: &Samples) -> Option<String> {
    s.msg.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_parse_ignores_unknown_and_keeps_input_order() {
        // 顺序**跟随输入**（`island_items` 的 csv 顺序就是显示顺序）
        let it = Items::parse("cpu, clock ,nonsense,mem");
        assert!(it.cpu && it.clock && it.mem);
        assert!(!it.battery);
        assert_eq!(it.to_csv(), "cpu,clock,mem");
        // 重复项只保留第一次出现的位置
        assert_eq!(Items::parse("mem,cpu,mem").to_csv(), "mem,cpu");
        // 空集合回落到 clock，避免空胶囊
        assert_eq!(Items::parse("").to_csv(), "clock");
        assert_eq!(Items::parse("nonsense").to_csv(), "clock");
    }

    #[test]
    fn segments_skip_unavailable_items() {
        let s = Samples {
            clock: "09:41:33".into(),
            date: "2026年11月23日 星期一".into(),
            cpu: None,
            mem: Some(63),
            battery: None,
            msg: None,
            unread: 0,
            ..Default::default()
        };
        let items = Items::parse("clock,dot,cpu,mem,battery,msg");
        let segs = segments(&s, &items, true);
        let texts: Vec<&str> = segs.iter().map(|x| x.text.as_str()).collect();
        // CPU 还没采到 → 不出现；没有电池 → 不出现；无未读 → dot 与 msg 都不出现
        assert_eq!(texts, vec!["09:41", "MEM 63%"]);
    }

    #[test]
    fn unread_dot_and_counter_appear_together() {
        let s = Samples {
            clock: "08:00".into(),
            unread: 3,
            ..Default::default()
        };
        let items = Items::parse("clock,dot,msg");
        let segs = segments(&s, &items, false);
        assert!(segs.iter().any(|x| x.dot), "有未读却没有圆点");
        assert!(segs.iter().any(|x| x.text == "MSG 3"));
        // 99 以上封顶
        let s99 = Samples {
            unread: 99,
            ..Default::default()
        };
        let segs = segments(&s99, &items, false);
        assert!(segs.iter().any(|x| x.text == "MSG 99+"));
    }

    #[test]
    fn short_clock_truncates_seconds_only() {
        assert_eq!(short_clock("09:41:33"), "09:41");
        assert_eq!(short_clock("09:41"), "09:41");
        assert_eq!(short_clock("坏数据"), "坏数据");
    }

    // ------------------------------------------------ 事件管道（§4.11）

    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// 每个用例一个独立目录：并行跑单测时共用一个目录会互相搬文件。
    struct TmpDir(std::path::PathBuf);
    impl TmpDir {
        fn new() -> TmpDir {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("dg-ev-{}-{}", std::process::id(), n));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("建临时目录");
            TmpDir(p)
        }
        fn write(&self, name: &str, body: &str) {
            std::fs::write(self.0.join(name), body).expect("写事件文件");
        }
        fn names(&self) -> Vec<String> {
            msg_names(&self.0)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parse_body_first_line_bom_and_source() {
        // 第一行；多余的行忽略
        assert_eq!(parse_body("构建完成\n第二行").as_deref(), Some("构建完成"));
        // 前后空白去掉
        assert_eq!(parse_body("  完成  \r\n").as_deref(), Some("完成"));
        // BOM（PowerShell 的 `Set-Content -Encoding utf8` 会写）
        assert_eq!(parse_body("\u{feff}完成").as_deref(), Some("完成"));
        // `正文\t来源` → 来源在前
        assert_eq!(parse_body("构建完成\tCI").as_deref(), Some("CI · 构建完成"));
        // 只有来源没有正文 ⇒ 当空文件
        assert_eq!(parse_body("\tCI"), None);
        // 空 / 纯空白
        assert_eq!(parse_body(""), None);
        assert_eq!(parse_body(" \r\n\t\r\n"), None);
    }

    #[test]
    fn pump_consumes_in_name_order_and_keeps_history() {
        let d = TmpDir::new();
        d.write("20261123090000_1.msg", "第一条");
        d.write("20261123090100_1.msg", "第二条");
        d.write("20261123090200_1.msg", "第三条");
        // 非 .msg 不参与
        d.write("readme.txt", "不是事件");

        let ev = pump(&d.0, true, true);
        assert_eq!(ev.latest.as_deref(), Some("第三条"), "取的应该是文件名最大的那条");
        assert_eq!(ev.fresh, 3);
        assert_eq!(ev.recent, 3, "归档里 24h 内应有 3 条");
        assert!(d.names().is_empty(), "消费后事件目录应该空了");
        assert_eq!(msg_names(&seen_dir(&d.0)).len(), 3, "三条都应归档");
        assert!(d.0.join("readme.txt").exists(), "非 .msg 文件被误删");

        // 再来一拍：没有新文件，但**未读仍要数归档**（否则读一条就永久归零）
        let ev2 = pump(&d.0, true, true);
        assert_eq!(ev2.latest, None, "没有新文件时不该给正文");
        assert_eq!(ev2.fresh, 0);
        assert_eq!(ev2.recent, 3, "归档还在，未读窗口要照常统计");
    }

    #[test]
    fn pump_keep_off_deletes_instead_of_archiving() {
        let d = TmpDir::new();
        d.write("20261123090000_1.msg", "一次性");
        let ev = pump(&d.0, true, false);
        assert_eq!(ev.latest.as_deref(), Some("一次性"));
        assert_eq!(ev.fresh, 1);
        assert_eq!(ev.recent, 0, "直接删的模式下没有历史可数");
        assert!(d.names().is_empty());
        assert!(
            !seen_dir(&d.0).exists(),
            "keep=false 时不该凭空建出 seen 目录"
        );
    }

    #[test]
    fn pump_read_only_moves_nothing() {
        let d = TmpDir::new();
        d.write("20261123090000_1.msg", "只读");
        d.write("20261123090100_1.msg", "只读2");
        let ev = pump(&d.0, false, true);
        assert_eq!(ev.latest.as_deref(), Some("只读2"), "只读模式取最新一条正文");
        assert_eq!(ev.fresh, 0);
        assert_eq!(ev.recent, 2, "只读模式下 recent 退回待读条数");
        assert_eq!(d.names().len(), 2, "只读模式搬了文件");
        assert!(!seen_dir(&d.0).exists());
    }

    #[test]
    fn pump_removes_archives_older_than_a_week() {
        let d = TmpDir::new();
        let seen = seen_dir(&d.0);
        std::fs::create_dir_all(&seen).expect("建 seen");
        let stale = seen.join("20260101000000_1.msg");
        let fresh = seen.join("20261123000000_1.msg");
        std::fs::write(&stale, "老").expect("写老归档");
        std::fs::write(&fresh, "新").expect("写新归档");
        // 把老归档的 mtime 拨到 8 天前
        let f = std::fs::OpenOptions::new().write(true).open(&stale).expect("开老归档");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 3600);
        f.set_modified(old).expect("改 mtime");
        drop(f);

        let ev = pump(&d.0, true, true);
        assert!(!stale.exists(), "超过 7 天的归档没被清掉");
        assert!(fresh.exists(), "7 天内的归档被误删");
        assert_eq!(ev.recent, 1, "只有 7 天内那条该算进未读");
    }

    #[test]
    fn msg_segment_shows_body_when_not_counting() {
        let s = Samples {
            msg: Some("构建完成".into()),
            unread: 5,
            counting: false,
            ..Default::default()
        };
        let items = Items::parse("msg");
        let segs = segments(&s, &items, true);
        assert_eq!(segs.len(), 1, "不计数时 Msg 片段应显示正文");
        assert_eq!(segs[0].text, "构建完成");

        // 计数模式：同样的数据应给条数
        let s2 = Samples { counting: true, ..s.clone() };
        let segs2 = segments(&s2, &items, true);
        assert_eq!(segs2[0].text, "MSG 5");
    }
}
