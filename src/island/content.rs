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
    /// 正在播放的媒体（歌名 / 艺人），来自 SMTC
    Media,
}

pub const ALL_ITEMS: [(&str, Item); 7] = [
    ("clock", Item::Clock),
    ("dot", Item::Dot),
    ("cpu", Item::Cpu),
    ("mem", Item::Mem),
    ("battery", Item::Battery),
    ("msg", Item::Msg),
    ("media", Item::Media),
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
    pub media: bool,
}

pub fn item_name(item: Item) -> &'static str {
    match item {
        Item::Clock => "clock",
        Item::Dot => "dot",
        Item::Cpu => "cpu",
        Item::Mem => "mem",
        Item::Battery => "battery",
        Item::Msg => "msg",
        Item::Media => "media",
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
            Item::Media => self.media = on,
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
    /// 是否采集系统通知（`island_notify`）
    pub notice: bool,
    /// 是否采集**真实**媒体会话。
    ///
    /// 与 `items.media` 不是一回事：那个决定「显不显示这一格」，这个决定
    /// 「要不要去问系统」。导出必须把它关掉 —— 否则同一份配置在放歌和
    /// 不放歌时导出两张图，像素基线立刻失效。
    pub media: bool,
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
            notice: true,
            media: false,
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
    /// 最近一条**事件**正文。
    ///
    /// 两个来源共用这一格：`events\*.msg` 文件（外部写入）和系统通知
    /// （`feed::latest`）。语义因此从「最近一条消息」变成「最近发生的一件事」
    /// —— 这样绿点、拉长、已读回缩三条既有行为全部自动复用，一处都不用改。
    pub msg: Option<String>,
    /// 已采到的最新通知主键。用来判「是不是又来了一条」：
    /// 通知正文可能与上一条一模一样（「收到一条新消息」重复出现），
    /// 比字符串会把第二条当成没发生。
    pub notice_key: i64,
    /// 正在播放的媒体行（`晴天 - 周杰伦`）。没有会话时为 `None`。
    pub media: Option<String>,
    /// 未读条数（封顶 99）
    pub unread: u32,
    /// **本拍会不会真的画出那个圆点**（`Island::dot_a > 0`）。
    ///
    /// 与 `unread` 分开：`unread` 是磁盘里 24h 的条数，开机就有；圆点只在
    /// 「新到达且没读过」时亮（`fresh`）。两者混用的后果是**占位不画** ——
    /// 有历史未读时 `segments` 给圆点留了 6px 与一个间距，而 `dot_a` 是 0
    /// 什么都没画，胶囊里于是留着一段莫名其妙的空白（真机截图里就在时钟
    /// 与 `CPU` 之间）。留白比少一个点更难看出原因。
    pub dot_visible: bool,
    /// **这一拍圆点还在呼吸**（前 10s），而不是已经退化成的常亮。
    ///
    /// 与 `dot_visible` 是两个不同的语义，别合并：
    /// * `dot_visible` = 有没有像素 ⇒ 决定**占不占位置**；
    /// * `dot_breathing` = 是不是「刚到了」那一段 ⇒ 决定**要不要把是什么事写出来**。
    ///
    /// 混用的后果（踩过）：呼吸 10s 之后圆点会退化成常亮 178（语义从「刚到了」
    /// 变成「还等着你看」），此时 `dot_visible` 仍然为真，于是通知正文**永远
    /// 挂在胶囊上** —— 用户看过了、甚至过了半天，那句话还在占着宽度，还把
    /// 歌名挤掉。而胶囊的常亮圆点只是提醒「还有没读的」，不该配一整句话。
    /// 要看全文有展开态第三行，胶囊本身寸土寸金。
    pub dot_breathing: bool,
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
            notice_key: 0,
            media: None,
            unread: 0,
            dot_visible: false,
            dot_breathing: false,
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

        // 系统通知：与 `events\*.msg` 共用「最近一条事件」这一格。
        //
        // 通知**排在 events 之后**（覆盖它）：文件可能是几分钟前丢进去的，
        // 而通知是刚刚弹出来的，后者才是「此刻发生的事」。
        // 判「新」用 `Notice::id` 而不是正文 —— 正文会重复
        // （「收到一条新消息」这种模板文案），比字符串会把第二条当成没发生。
        if opts.notice && items.msg {
            if let Some(n) = crate::island::feed::latest() {
                // **冷启动基线**：`prev == 0` 是第一次采样，只记住「当前最新是
                // 哪条」，不当成新消息。
                //
                // 不这么做的话，每次启动挂件都会把库里最新一条通知重新弹一遍
                // —— 而那是几小时前、甚至几天前的（库里有跨大半年的残留行）。
                // 用户看到的是「刚打开就冒出一条看过的通知，而且一直不消失」。
                if notice_is_new(self.notice_key, n.id) {
                    self.msg = Some(n.line());
                }
                self.notice_key = n.id;
            }
        }

        // 媒体：SMTC 一次要激活工厂 + 两次异步等待，比 `GetSystemTimes`
        // 重得多，所以**两秒一采**。换歌是秒级事件，两秒的延迟看不出来，
        // 而 1Hz 采样的成本会占掉 `tick` 预算的一截。
        if items.media && opts.media {
            if stamp % 2 == 0 {
                self.media = crate::island::feed::media::current().map(|m| m.line());
            }
        } else {
            self.media = None;
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
    /// 这一格的**占位宽度基准串**：排版量宽时取
    /// `max(measure(text), measure(widest))`，于是同一格在不同采样值下
    /// 宽度恒定，文字左侧对齐、右侧留白。
    ///
    /// # 为什么不能靠「补空格凑够位数」
    ///
    /// 第一版就是这么干的（`format!("CPU {c:>3}%")`），实测**不起作用**：
    /// 胶囊宽度仍然 289 / 293 / 297 地跳。原因是雅黑的空格（≈4px）比数字
    /// （≈8px）窄一半 —— 补两个空格只补了 8px，而数字从 1 位变 3 位涨 16px。
    /// 要真正等宽只能让**排版层**按同一个基准量，而不是在字符串里凑字符数。
    pub widest: Option<&'static str>,
}

impl Seg {
    fn text(s: impl Into<String>, main: bool) -> Seg {
        Seg {
            text: s.into(),
            main,
            dot: false,
            widest: None,
        }
    }

    /// 数值型片段：宽度按 `widest` 定死（该格可能出现的最大数字）。
    fn metric(s: impl Into<String>, widest: &'static str) -> Seg {
        Seg {
            text: s.into(),
            main: false,
            dot: false,
            widest: Some(widest),
        }
    }
}

/// 把采样结果翻成「收起态要显示的那一串片段」。
///
/// 返回值里**已剔除不可用项**：没有电池就没有 Battery 片段，CPU 还没采到
/// 第二次就没有 Cpu 片段。宁可少一项，也不显示 `--` 占位 —— 一条随时会变
/// 的胶囊里，占位符和真数据混在一起看不出区别。
///
/// `peek` 是「新消息到达后的拉长态」：此时 `Msg` 片段改给**正文**
/// （`来源 · 正文`，`parse_body` 已经拼好），胶囊靠内容自然撑宽 ——
/// 这就是「自动拉长显示信息来源应用」，不需要第四条掩码或新的排版路径。
/// 计数模式（`MSG n`）只在非拉长态用：用户要的是「谁发了什么」，
/// 不是一个数字。
pub fn segments(s: &Samples, items: &Items, clock_short: bool, peek: bool) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::new();
    // 媒体标题该用哪一档长度（见 `MEDIA_CHARS_IDLE`）。判据必须与下面
    // `Item::Msg` 的分支**逐条对齐**：两边判得不一致就会「按宽档排版、
    // 实际却多出 `MSG 99+`」⇒ 溢出上限 ⇒ 分页 ⇒ 第 2 页永不显示。
    let msg_slot = if peek {
        s.msg.is_some()
    } else if s.counting {
        s.unread > 0 || (s.dot_breathing && s.msg.is_some())
    } else {
        s.msg.is_some()
    };
    // 呼吸期会多画一段通知正文（见下面 `Item::Msg`），这时媒体**整段让位**：
    // 两段自由文本（通知 12 字 + 媒体 20 字）叠在一起实测 625px，远超
    // `COMPACT_MAX_W`；而此刻「谁发了什么」比「在放什么」重要得多。
    // 呼吸一结束（绿点转常亮）正文收起，媒体自动回来 —— 判据用
    // `dot_breathing` 而不是 `dot_visible`，见 `Samples::dot_breathing`。
    let notice_slot =
        !peek && s.counting && s.unread == 0 && s.dot_breathing && s.msg.is_some();
    let media_chars = if msg_slot {
        MEDIA_CHARS_BUSY
    } else {
        MEDIA_CHARS_IDLE
    };
    for it in &items.order {
        match it {
            Item::Clock => out.push(Seg::text(
                if clock_short { short_clock(&s.clock) } else { s.clock.clone() },
                true,
            )),
            // 点的判据是**这一拍会不会真的画**（`dot_visible`），不是
            // 「有没有未读」：`unread` 数的是 24h 归档，开机就非零，而圆点
            // 只在 fresh 时亮。拿 unread 当判据会「占位不画」留下空白。
            //
            // `peek`（拉长态）单独算：新消息到达那一刻 `dot_a` 已经是呼吸起点，
            // 但导出/自检可能不跑 tick，把这条留成显式或，语义也更清楚 ——
            // 那个点是**内容事件**，不是**计数功能**的附属品。
            Item::Dot => {
                if s.dot_visible || peek {
                    out.push(Seg {
                        text: String::new(),
                        main: true,
                        dot: true,
                        widest: None,
                    });
                }
            }
            // 电脑状态三项：宽度按**该格最大数值**定死（`CPU 100%`），
            // 于是 `CPU 7%` 与 `CPU 100%` 占的宽度一模一样 —— 数字一变
            // 胶囊不会跟着抽。真实值左对齐画在格子里，右侧留白。
            //
            // 定宽为什么非做不可：这三个值一秒一采样，实测不定宽时胶囊宽度
            // 289 / 293 / 297 地跳（+3%），屏幕上就是「一条每秒呼吸一次的
            // 胶囊」—— 把「看得见状态」变成「有个东西一直在动」。
            Item::Cpu => {
                // 拉长态把三格让给正文：常态本身就宽（状态全开 ≈365），
                // 拉长若只多出十几像素就看不出来「这是在给你看消息」。
                if peek {
                    continue;
                }
                if let Some(c) = s.cpu {
                    out.push(Seg::metric(format!("CPU {c}%"), "CPU 100%"));
                }
            }
            Item::Mem => {
                if peek {
                    continue;
                }
                if let Some(m) = s.mem {
                    out.push(Seg::metric(format!("MEM {m}%"), "MEM 100%"));
                }
            }
            // 电池带 `BAT` 标签：裸 `88%` 夹在 `MEM 63%` 后面，读起来像
            // 「MEM 是 63% 和 88%」——同一个数轴上两个百分数，谁是电池说不清。
            // 缩写沿用 `CPU` / `MEM` 的既有风格，不为了一个词把一排打成中文。
            // `+` 放末尾与展开态的「电池 84%+」同一个写法（充电只在插拔时变，
            // 这一个字符的宽度差不会造成每秒抽动）。
            Item::Battery => {
                if peek {
                    continue;
                }
                if let Some((p, charging)) = s.battery {
                    out.push(Seg::metric(
                        if charging { format!("BAT {p}%+") } else { format!("BAT {p}%") },
                        "BAT 100%+",
                    ));
                }
            }
            // 正在播放：`♪ 晴天 - 周杰伦`
            //
            // 拉长态让位给正文（与状态三格同一个理由：常态已经很宽，
            // 拉长不多出宽度就看不出来「这是在给你看消息」）。
            //
            // **截断只数标题**，不能把 `♪ ` 前缀一起算进预算：早先写的是
            // `clip_chars(&format!("♪ {m}"), 10)`，于是 10 个字符里 `♪` 和
            // 空格就吃掉两个 —— 标题实际只剩 7 个字可见，比常量说的少三成。
            //
            // 不做定宽：标题长短不可预测，用中文基准当占位会让英文歌名
            // 右侧留一大片白。换歌才变宽度，与「每秒抽动」不是一类问题。
            Item::Media => {
                if peek || notice_slot {
                    continue;
                }
                if let Some(m) = &s.media {
                    out.push(Seg::text(format!("♪ {}", clip_chars(m, media_chars)), false));
                }
            }
            Item::Msg => {
                if peek && s.msg.is_some() {
                    // 拉长态：优先给「来源 · 正文」。这就是用户要的
                    // 「自动拉长显示信息来源应用」—— 来源本来就在正文串的前缀里。
                    //
                    // **必须在这里就截断**，不能留给 `compact_page`：那边超宽时
                    // 走 `pack_pages` 分页，第 0 页只剩「时钟 + 绿点」，正文被翻到
                    // 第 2 页 —— 拉长了却看不到内容，等于没拉。
                    let m = s.msg.clone().unwrap_or_default();
                    out.push(Seg::text(clip_chars(&m, PEEK_BODY_CHARS), false));
                } else if s.counting {
                    // 计数模式：只给条数。正文放展开态第三行 —— 收起态的胶囊
                    // 宽度是靠内容撑出来的，塞一句正文会把胶囊拉成一条横条。
                    if s.unread > 0 {
                        let n = if s.unread >= 99 { "99+".to_string() } else { s.unread.to_string() };
                        // 同样定宽：未读从 9 变 10 是两位数，不定宽时胶囊
                        // 会为多出的一个字符再抽一下（`MSG 3` 42px → `MSG 99+` 50px）。
                        out.push(Seg::metric(format!("MSG {n}"), "MSG 99+"));
                    } else if s.dot_breathing {
                        // **呼吸期里必须说出来是什么事。**
                        //
                        // 系统通知不计入 `unread`（那个数只数 `events\seen\`
                        // 里的文件），所以「刚弹了一条通知」会落在
                        // `unread == 0 && dot_breathing == true` 这一档。原来这里
                        // 什么都不画，用户看到的是「一个绿点闪了半天，
                        // 但胶囊上没有任何字」—— 比不提示更让人困惑。
                        //
                        // **只在呼吸期（10s）写**：呼吸结束圆点转常亮，语义变成
                        // 「还等着你看」，那时正文必须收起来 —— 否则用户看过之后
                        // 那句话会一直挂在胶囊上，既占宽度又把歌名挤掉。
                        // 要看全文有展开态第三行，不用赖在胶囊里。
                        if let Some(m) = &s.msg {
                            out.push(Seg::text(clip_chars(m, NOTICE_CHARS), false));
                        }
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

/// 拉长态正文的字符上限。
///
/// 28 字 × 13px ≈ 364，加时钟（≈34）、绿点（6 + 两侧间距）与内边距（28）
/// ≈ 440，落在 `geom::COMPACT_PEEK_MAX_W`（460）之内 ⇒ 单页。
/// 拉长态**不带状态三格**（见 `segments` 里 `if peek { continue }`），
/// 所以这里的预算只花在正文上 —— 常态与拉长态的宽度差因此是实打实的。
pub const PEEK_BODY_CHARS: usize = 28;

/// 媒体标题的字符上限 —— **只数标题本身**，不含 `♪ ` 前缀。
///
/// # 为什么分两档
///
/// 媒体标题是自由文本（B 站/YouTube 标题动辄 30 字），而 `COMPACT_MAX_W`
/// 是硬上限（`island_rotate=0` 时第 2 页永不出现 ⇒ 一旦分页就等于丢内容）。
/// 固定一个常数只有两种结局：按「有事件时」定 ⇒ 平时白白浪费半条胶囊；
/// 按「无事件时」定 ⇒ 事件一来就分页。
///
/// 所以按「这一排还有没有别的文本项」分两档 —— 与 `peek` 让位状态三格
/// 是同一个思路：宽度是稀缺资源，先给「此刻最该看的那个」。
///
/// 实测（13px、默认字体、`items` 全勾、数值全满值）：
/// * 无事件档：时钟 + 三格 + 媒体 = 449px
/// * 有事件档：再加 `MSG 99+` 与绿点 = 552px（上限 560）
pub const MEDIA_CHARS_IDLE: usize = 20;
/// 有事件（`MSG n` / 通知正文）占位时的媒体长度，见 [`MEDIA_CHARS_IDLE`]。
pub const MEDIA_CHARS_BUSY: usize = 11;

/// 绿点呼吸期（30s）内常态显示的通知正文长度。
///
/// 比拉长态（`PEEK_BODY_CHARS = 28`）短得多：这段是**常态**里额外塞的，
/// 常态本来就装着时钟 + 状态三格，再长就要分页。12 字够看清「谁发的 +
/// 说了什么开头」，要看全文就悬停展开。
pub const NOTICE_CHARS: usize = 12;

/// 这条通知该不该当作「新到达」。
///
/// `prev == 0` 是**冷启动**：第一次采样只建立基线，不弹。
/// 否则每次启动都会把库里最新一条（可能是几天前的）当成新通知弹出来 ——
/// 而且它会一直挂在胶囊上，看起来就是「看过了也不消失」。
fn notice_is_new(prev: i64, id: i64) -> bool {
    prev != 0 && id != prev
}

/// 按**字符**（不是字节）截断，超长补省略号。
///
/// 汉字 3 字节，用 `&s[..n]` 会在多字节边界上 panic —— 这个函数从第一天
/// 就按 `chars()` 走。
fn clip_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
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
        let segs = segments(&s, &items, true, false);
        let texts: Vec<&str> = segs.iter().map(|x| x.text.as_str()).collect();
        // CPU 还没采到 → 不出现；没有电池 → 不出现；无未读 → dot 与 msg 都不出现
        assert_eq!(texts, vec!["09:41", "MEM 63%"]);
    }

    /// 圆点的位置跟着「这一拍画不画」走，**不跟未读数走**。
    ///
    /// 这条的前身断言的是「有未读就有圆点」，而那个写法在真机上表现为
    /// **占位不画**：`unread` 数的是 24h 归档（开机就非零），圆点却只在
    /// fresh 时亮 —— 于是胶囊里在时钟与 `CPU` 之间留一段 14px 的空白，
    /// 且没有任何东西提示那段空白是干什么的。
    #[test]
    fn dot_follows_visibility_not_unread_count() {
        let items = Items::parse("clock,dot,msg");
        // 有历史未读、这一拍不画点 ⇒ 连位置都不该留
        let s = Samples {
            clock: "08:00".into(),
            unread: 3,
            dot_visible: false,
            ..Default::default()
        };
        let segs = segments(&s, &items, false, false);
        assert!(!segs.iter().any(|x| x.dot), "没画点却占了位置（会留一段空白）");
        assert!(segs.iter().any(|x| x.text == "MSG 3"), "计数本身不该被点影响");

        // 这一拍真要画点 ⇒ 有位置
        let s2 = Samples {
            dot_visible: true,
            ..s.clone()
        };
        assert!(
            segments(&s2, &items, false, false).iter().any(|x| x.dot),
            "要画点却没有位置"
        );

        // 拉长态即使 `dot_visible` 还没更新也要留位置（导出 / 自检不跑 tick）
        let s3 = Samples {
            unread: 0,
            dot_visible: false,
            ..Default::default()
        };
        assert!(
            segments(&s3, &items, false, true).iter().any(|x| x.dot),
            "拉长态没有圆点位置"
        );

        // 99 以上封顶
        let s99 = Samples {
            unread: 99,
            dot_visible: true,
            ..Default::default()
        };
        assert!(segments(&s99, &items, false, false)
            .iter()
            .any(|x| x.text == "MSG 99+"));
    }

    #[test]
    fn short_clock_truncates_seconds_only() {
        assert_eq!(short_clock("09:41:33"), "09:41");
        assert_eq!(short_clock("09:41"), "09:41");
        assert_eq!(short_clock("坏数据"), "坏数据");
    }

    // ------------------------------------------------ 媒体标题的宽度预算

    /// 造一条固定长度的标题，取回 `segments` 里那段 `♪ …`。
    fn media_seg(s: &Samples) -> String {
        let segs = segments(s, &Items::parse("media"), false, false);
        segs.iter()
            .find(|x| x.text.starts_with('♪'))
            .map(|x| x.text.clone())
            .unwrap_or_else(|| panic!("没有媒体片段：{:?}", segs.iter().map(|x| &x.text)))
    }

    /// 标题**可见字数**（去掉 `♪ ` 前缀与末尾省略号）。
    fn visible_title(t: &str) -> usize {
        t.trim_start_matches("♪ ").trim_end_matches('…').chars().count()
    }

    /// `♪ ` 前缀**不占**标题的字符预算。
    ///
    /// 这条的注毒很直接：早先的写法是 `clip_chars(&format!("♪ {m}"), 10)`，
    /// 于是 `♪` 与空格吃掉两个名额 —— 常量写着 10，屏幕上只有 7 个字。
    /// 用「可见字数 == 预算 - 1（省略号占一位）」钉住，任何把前缀算进去的
    /// 实现都会少 2 个字，断言立刻变红。
    #[test]
    fn media_title_budget_excludes_the_note_prefix() {
        let s = Samples {
            media: Some("字".repeat(60)),
            ..Default::default()
        };
        assert_eq!(visible_title(&media_seg(&s)), MEDIA_CHARS_IDLE - 1);
        // 短标题原样给出，不补省略号
        let short = Samples {
            media: Some("晴天".into()),
            ..Default::default()
        };
        assert_eq!(media_seg(&short), "♪ 晴天");
    }

    /// 有 `MSG n` 占位时媒体退到紧档 —— 两档长度必须真的不同，
    /// 否则「分档」只是写在常量名里的愿望。
    #[test]
    fn media_shrinks_when_the_message_slot_is_taken() {
        let title = "字".repeat(60);
        let idle = Samples {
            media: Some(title.clone()),
            unread: 0,
            ..Default::default()
        };
        let busy = Samples {
            media: Some(title),
            unread: 5,
            ..Default::default()
        };
        let (a, b) = (visible_title(&media_seg(&idle)), visible_title(&media_seg(&busy)));
        assert_eq!(a, MEDIA_CHARS_IDLE - 1);
        assert_eq!(b, MEDIA_CHARS_BUSY - 1);
        assert!(b < a, "有事件时媒体没有让位（{} vs {}）", b, a);
        // 让位的另一半：`MSG n` 确实出现了（否则上面量的只是「无事件」）
        assert!(segments(&busy, &Items::parse("media,msg"), false, false)
            .iter()
            .any(|x| x.text == "MSG 5"));
    }

    /// 冷启动不能把历史通知当成新消息。
    ///
    /// 库里最新一条可能是几天前的（41 条 Notification 里最老的跨了大半年、
    /// 用户清空通知中心也不会删行），所以「第一次读到就弹」必然误报：
    /// 每次启动挂件都冒出一条看过的通知。
    #[test]
    fn notice_is_new_ignores_the_cold_start_baseline() {
        assert!(!notice_is_new(0, 2041), "冷启动把历史通知当成新的了");
        assert!(!notice_is_new(2041, 2041), "同一条通知重复弹");
        assert!(notice_is_new(2041, 2042), "新通知没被认出来");
        // `Id` 是自增主键，但判据只要求「不同」—— 库被重建后编号可能回绕，
        // 那时 `id < prev` 也是新的一条，不能写成 `id > prev`。
        assert!(notice_is_new(2041, 7), "编号回绕后的通知没被认出来");
    }

    /// 通知正文只活在呼吸期：圆点转常亮（`dot_visible` 仍为真）时必须收。
    ///
    /// 这条钉的是用户报的那个现象：「通知看过了，没消失」。呼吸 10s 后
    /// 圆点退化成常亮 178，`dot_visible` 还是真 —— 早先正文的判据就写在
    /// `dot_visible` 上，于是那句话一直挂在胶囊里，还把歌名挤掉。
    #[test]
    fn notice_line_lives_only_during_the_breathe_phase() {
        let items = Items::parse("clock,dot,msg");
        let base = Samples {
            msg: Some("张三：明天评审".into()),
            unread: 0,
            counting: true,
            ..Default::default()
        };
        let said = |s: &Samples| {
            segments(s, &items, true, false)
                .iter()
                .any(|x| x.text.contains("张三"))
        };

        let breathing = Samples {
            dot_visible: true,
            dot_breathing: true,
            ..base.clone()
        };
        assert!(said(&breathing), "呼吸期没写正文");

        let steady = Samples {
            dot_visible: true,
            dot_breathing: false,
            ..base.clone()
        };
        assert!(
            !said(&steady),
            "圆点转常亮后正文还挂着 —— 用户看过了它也不会走"
        );

        let read = Samples {
            dot_visible: false,
            dot_breathing: false,
            ..base
        };
        assert!(!said(&read), "已读之后正文还在");
    }

    /// 绿点呼吸期（通知正文占位）媒体**整段让位**，呼吸一结束就回来。
    ///
    /// 两段自由文本叠在一起实测 625px，远超 `COMPACT_MAX_W` 560 ⇒ 分页 ⇒
    /// `island_rotate=0` 时第 2 页永不出现，等于把通知内容丢了。
    /// 所以这一档宁可少画媒体，也要保证「谁发了什么」完整可见。
    #[test]
    fn media_yields_while_notice_body_is_up() {
        let s = Samples {
            media: Some("晴天 - 周杰伦".into()),
            msg: Some("CI · 构建完成".into()),
            unread: 0,
            dot_visible: true,
            dot_breathing: true,
            ..Default::default()
        };
        let texts = |s: &Samples| -> Vec<String> {
            segments(s, &Items::parse("media,msg"), false, false)
                .iter()
                .map(|x| x.text.clone())
                .collect()
        };
        let during = texts(&s);
        assert!(
            !during.iter().any(|t| t.starts_with('♪')),
            "通知正文在的时候媒体没让位：{during:?}"
        );
        assert!(
            during.iter().any(|t| t.contains("构建完成")),
            "通知正文也没画：{during:?}"
        );

        // 呼吸结束（圆点仍常亮）⇒ 媒体回来。不回来就成了「让一次位让到底」。
        let after = Samples {
            dot_breathing: false,
            ..s.clone()
        };
        assert!(
            texts(&after).iter().any(|t| t.starts_with('♪')),
            "呼吸结束后媒体没有回来"
        );

        // 绿点熄灭（已读）也照样回来
        let read = Samples {
            dot_visible: false,
            dot_breathing: false,
            ..s
        };
        assert!(
            texts(&read).iter().any(|t| t.starts_with('♪')),
            "绿点熄灭后媒体没有回来"
        );
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
        let segs = segments(&s, &items, true, false);
        assert_eq!(segs.len(), 1, "不计数时 Msg 片段应显示正文");
        assert_eq!(segs[0].text, "构建完成");

        // 计数模式：同样的数据应给条数
        let s2 = Samples { counting: true, ..s.clone() };
        let segs2 = segments(&s2, &items, true, false);
        assert_eq!(segs2[0].text, "MSG 5");
    }
}
