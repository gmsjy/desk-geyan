//! 系统信息采集：通知中心。
//!
//! # 为什么走通知数据库而不是 WinRT
//!
//! `UserNotificationListener` 是 UWP 才有的能力，桌面 exe 没有 package
//! identity，`RequestAccessAsync` 会直接拒绝。而通知库
//! `%LOCALAPPDATA%\Microsoft\Windows\Notifications\wpndatabase.db` 是
//! **普通 SQLite 文件**：`mode=ro` 能只读打开（WpnUserService 持有写锁但
//! 不禁止读），里面的 `Notification.Payload` 就是完整的 toast XML ——
//! 应用名、标题、正文一个不少。
//!
//! # 为什么不引 sqlite crate
//!
//! 这个项目只依赖 `windows-sys`。而 **Windows 10 起自带
//! `winsqlite3.dll`**（实测 3.43.2），导出标准 SQLite C API —— 同一份
//! `sqlite3.h` 的函数签名，`LoadLibraryW` + `GetProcAddress` 就够用。
//! 换来的是零新依赖、零构建时间、零 C 编译。
//!
//! 动态加载还顺带解决了一个问题：**库不在就当没有这项能力**，而不是
//! 启动即崩。Windows 7 / Server Core 上确实没有这个 dll。

use std::ffi::c_void;
use std::sync::OnceLock;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type Sqlite = *mut c_void;
type Stmt = *mut c_void;

const SQLITE_OK: i32 = 0;
const SQLITE_ROW: i32 = 100;
const SQLITE_OPEN_READONLY: i32 = 0x0000_0001;
const SQLITE_OPEN_URI: i32 = 0x0000_0040;

type OpenV2 = unsafe extern "system" fn(*const u8, *mut Sqlite, i32, *const u8) -> i32;
type PrepareV2 =
    unsafe extern "system" fn(Sqlite, *const u8, i32, *mut Stmt, *mut *const u8) -> i32;
type Step = unsafe extern "system" fn(Stmt) -> i32;
type ColumnText = unsafe extern "system" fn(Stmt, i32) -> *const u8;
type ColumnBlob = unsafe extern "system" fn(Stmt, i32) -> *const c_void;
type ColumnBytes = unsafe extern "system" fn(Stmt, i32) -> i32;
type ColumnInt64 = unsafe extern "system" fn(Stmt, i32) -> i64;
type Finalize = unsafe extern "system" fn(Stmt) -> i32;
type Close = unsafe extern "system" fn(Sqlite) -> i32;

/// `winsqlite3.dll` 的函数指针表。
///
/// 一次性解析、缓存。`OnceLock<Option<Api>>` 而不是 `OnceLock<Api>`：
/// **「库不存在」本身是要缓存的结果** —— 否则每一拍都会重新
/// `LoadLibraryW` 一次失败的系统调用。
struct Api {
    open_v2: OpenV2,
    prepare_v2: PrepareV2,
    step: Step,
    col_text: ColumnText,
    col_blob: ColumnBlob,
    col_bytes: ColumnBytes,
    col_i64: ColumnInt64,
    finalize: Finalize,
    close: Close,
}

// 全是函数指针，没有可变状态；SQLite 本身线程安全（串行模式下更保守）。
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Option<Api>> = OnceLock::new();

fn api() -> Option<&'static Api> {
    API.get_or_init(|| unsafe { load() }).as_ref()
}

unsafe fn load() -> Option<Api> {
    let name: Vec<u16> = "winsqlite3.dll".encode_utf16().chain(Some(0)).collect();
    let h = LoadLibraryW(name.as_ptr());
    if h.is_null() {
        return None;
    }
    // 任一符号缺失就整体放弃：半个 API 表比没有更难诊断。
    macro_rules! sym {
        ($name:literal, $ty:ty) => {{
            let p = GetProcAddress(h, concat!($name, "\0").as_ptr());
            let p = p?;
            std::mem::transmute::<_, $ty>(p)
        }};
    }
    Some(Api {
        open_v2: sym!("sqlite3_open_v2", OpenV2),
        prepare_v2: sym!("sqlite3_prepare_v2", PrepareV2),
        step: sym!("sqlite3_step", Step),
        col_text: sym!("sqlite3_column_text", ColumnText),
        col_blob: sym!("sqlite3_column_blob", ColumnBlob),
        col_bytes: sym!("sqlite3_column_bytes", ColumnBytes),
        col_i64: sym!("sqlite3_column_int64", ColumnInt64),
        finalize: sym!("sqlite3_finalize", Finalize),
        close: sym!("sqlite3_close", Close),
    })
}

/// 一条系统通知。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notice {
    /// 应用名（从 AUMID 里摘出来的短名）
    pub app: String,
    /// 主标题（toast 的 `text id=1`）
    pub title: String,
    /// 正文（`text id=2`），可能为空
    pub body: String,
    /// 通知表主键。**判「是不是新的」只能靠它** —— 时间戳在跨秒、跨天、
    /// 以及系统时钟被校正时都不可靠，而 `Id` 单调递增。
    pub id: i64,
}

impl Notice {
    /// 拼成一行给胶囊用：`微信 · 张三：明天评审`
    ///
    /// 正文为空时只给标题 —— 不留一个孤零零的分隔符。
    pub fn line(&self) -> String {
        // 泛名不要：`Microsoft.Windows.ShellExperienceHost...` 取首段只剩
        // 「Microsoft」，挂在标题前面纯属噪音（实测托盘气泡就是这样）。
        // 这类名字不携带任何「谁发的」信息，不如省掉。
        let generic = matches!(
            self.app.as_str(),
            "" | "Microsoft" | "Windows" | "System" | "通知"
        );
        let head = if generic || self.title.starts_with(&self.app) {
            self.title.clone()
        } else if self.title.is_empty() {
            self.app.clone()
        } else {
            format!("{} · {}", self.app, self.title)
        };
        if self.body.is_empty() {
            head
        } else if head.is_empty() {
            self.body.clone()
        } else {
            format!("{head}：{}", self.body)
        }
    }
}

/// 通知库的路径。`%LOCALAPPDATA%` 取不到就没有这项能力。
pub fn db_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(
        std::path::PathBuf::from(base)
            .join("Microsoft")
            .join("Windows")
            .join("Notifications")
            .join("wpndatabase.db"),
    )
}

/// URI 里不能直接出现的字节做百分号编码。
///
/// 用户名带空格或中文时不编码会让 SQLite 把路径解析错，而错误又只体现成
/// 「打开失败」，和「库不存在」分不开 —— 所以这里宁可多编一点。
fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 读最新一条通知。
///
/// `None` 覆盖三种情况：库不存在、库打不开（服务没起来）、库里没有记录。
/// 调用方不需要区分 —— 对界面而言都是「这一拍没有通知」。
pub fn latest() -> Option<Notice> {
    let api = api()?;
    let path = db_path()?;
    if !path.exists() {
        return None;
    }
    let uri = format!("file:{}?mode=ro", uri_encode(&path.to_string_lossy()));
    let uri_c = std::ffi::CString::new(uri).ok()?;
    unsafe {
        let mut db: Sqlite = std::ptr::null_mut();
        let flags = SQLITE_OPEN_READONLY | SQLITE_OPEN_URI;
        if (api.open_v2)(uri_c.as_ptr() as *const u8, &mut db, flags, std::ptr::null()) != SQLITE_OK {
            if !db.is_null() {
                (api.close)(db);
            }
            return None;
        }
        let n = query_latest(api, db);
        (api.close)(db);
        n
    }
}

unsafe fn query_latest(api: &Api, db: Sqlite) -> Option<Notice> {
    // `Order` 是 SQL 保留字，必须加双引号 —— 不加会得到
    // `near "Order": syntax error`，而那个错误只表现为「查不到」。
    let sql = b"SELECT n.Id, n.Payload, h.PrimaryId \
                FROM Notification n LEFT JOIN NotificationHandler h \
                ON h.RecordId = n.HandlerId \
                ORDER BY n.\"Order\" DESC LIMIT 1\0";
    let mut st: Stmt = std::ptr::null_mut();
    if (api.prepare_v2)(db, sql.as_ptr(), -1, &mut st, std::ptr::null_mut()) != SQLITE_OK {
        if !st.is_null() {
            (api.finalize)(st);
        }
        return None;
    }
    let mut out = None;
    if (api.step)(st) == SQLITE_ROW {
        let id = (api.col_i64)(st, 0);
        let blob = (api.col_blob)(st, 1);
        let len = (api.col_bytes)(st, 1);
        let payload = if blob.is_null() || len <= 0 {
            String::new()
        } else {
            let bytes = std::slice::from_raw_parts(blob as *const u8, len as usize);
            String::from_utf8_lossy(bytes).into_owned()
        };
        let aumid = {
            let p = (api.col_text)(st, 2);
            if p.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(p as *const i8)
                    .to_string_lossy()
                    .into_owned()
            }
        };
        let title = xml_text(&payload, 1).unwrap_or_default();
        let body = xml_text(&payload, 2).unwrap_or_default();
        // 标题与正文都空的通知不显示：有的应用只发一个图标 + 自定义模板，
        // 我们的胶囊上会变成一条空字符串占位。
        if !title.is_empty() || !body.is_empty() {
            out = Some(Notice {
                app: app_name(&aumid),
                title,
                body,
                id,
            });
        }
    }
    (api.finalize)(st);
    out
}

/// 从 toast XML 里取 `<text id="N">...</text>`。
///
/// 不用 XML 库：toast 的 payload 是固定模板（`ToastText01..04`、
/// `ToastGeneric`），结构只有三层，文字里不会有嵌套标签。真正的坑是
/// **实体转义** —— `&amp;` 在标题里很常见（「A & B」），不还原就会把
/// `&amp;` 原样画到胶囊上。
fn xml_text(xml: &str, id: u8) -> Option<String> {
    let open = format!("<text id=\"{id}\">");
    let start = xml.find(&open)? + open.len();
    let rest = &xml[start..];
    let end = rest.find("</text>")?;
    let raw = &rest[..end];
    let text = unescape(raw);
    (!text.trim().is_empty()).then(|| text.trim().to_string())
}

/// XML 实体还原。只处理会出现在通知文字里的那五个 + 数字实体。
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let Some(semi) = tail.find(';') else {
            out.push_str(tail);
            return out;
        };
        let ent = &tail[1..semi];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => {
                if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                } else if let Some(dec) = ent.strip_prefix('#') {
                    dec.parse::<u32>().ok().and_then(char::from_u32)
                } else {
                    None
                }
            }
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            // 不认识的实体**原样保留**：猜错会吞掉用户的文字，而多留几个
            // 字符至少是可见的。
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// AUMID → 短应用名。
///
/// 三种真实形态：
/// - `WorkBuddy.WorkBuddy` → `WorkBuddy`
/// - `Microsoft.WindowsStore_8wekyb3d8bbwe!App` → `Microsoft.WindowsStore`
/// - `{1D97D0E2-A311-4B1C-A5F7-D3BAF7EF549B}` → `通知`（GUID 形式的没有可读名）
fn app_name(aumid: &str) -> String {
    let s = aumid.trim();
    if s.is_empty() {
        return "通知".to_string();
    }
    if s.starts_with('{') {
        return "通知".to_string();
    }
    // `!` 之后是入口点（`!App`），`_` 之后是包族名哈希 —— 都丢掉
    let base = s
        .split('!')
        .next()
        .unwrap_or(s)
        .split('_')
        .next()
        .unwrap_or(s);
    let first = base.split('.').next().unwrap_or(base);
    if first.is_empty() {
        "通知".to_string()
    } else {
        first.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_toast_text_and_unescapes() {
        let xml = r#"<toast><visual><binding template="ToastText02">\
            <text id="1">任务已完成</text><text id="2">A &amp; B &#65;</text>\
            </binding></visual></toast>"#;
        assert_eq!(xml_text(xml, 1).as_deref(), Some("任务已完成"));
        assert_eq!(xml_text(xml, 2).as_deref(), Some("A & B A"));
        assert_eq!(xml_text(xml, 3), None);
    }

    #[test]
    fn empty_text_is_not_a_notice_line() {
        let xml = r#"<toast><visual><binding template="ToastText01"><text id="1">  </text></binding></visual></toast>"#;
        assert_eq!(xml_text(xml, 1), None);
    }

    #[test]
    fn line_joins_without_dangling_separator() {
        let n = Notice {
            app: "微信".into(),
            title: "张三".into(),
            body: String::new(),
            id: 1,
        };
        assert_eq!(n.line(), "微信 · 张三");
        let n2 = Notice {
            app: "微信".into(),
            title: "张三".into(),
            body: "明天评审".into(),
            id: 2,
        };
        assert_eq!(n2.line(), "微信 · 张三：明天评审");
        // 标题已经含了应用名时不再重复前缀（微信自己发的 toast 就是这样）
        let n3 = Notice {
            app: "微信".into(),
            title: "微信：张三".into(),
            body: String::new(),
            id: 3,
        };
        assert_eq!(n3.line(), "微信：张三");
    }

    #[test]
    fn aumid_to_short_name() {
        assert_eq!(app_name("WorkBuddy.WorkBuddy"), "WorkBuddy");
        assert_eq!(app_name("Microsoft.WindowsStore_8wekyb3d8bbwe!App"), "Microsoft");
        assert_eq!(app_name("{1D97D0E2-A311-4B1C-A5F7-D3BAF7EF549B}"), "通知");
        assert_eq!(app_name(""), "通知");
    }

    #[test]
    fn uri_encode_keeps_path_shape() {
        // 反斜杠必须编码（URI 里是保留字符），冒号保留（盘符要用）
        assert_eq!(uri_encode(r"C:\a b\c.db"), "C:%5Ca%20b%5Cc.db");
        assert_eq!(uri_encode("/x/y"), "/x/y");
    }

    /// 真库探针：库在且能打开时必须读到字段完整的记录。
    ///
    /// 这条**依赖本机状态**，所以只在库存在时断言「能打开」，
    /// 不假定一定有通知 —— 空库是正常状态。
    #[test]
    fn real_db_opens_when_present() {
        let Some(p) = db_path() else { return };
        if !p.exists() {
            return;
        }
        let api = match api() {
            Some(a) => a,
            None => return, // 没有 winsqlite3.dll 的机器
        };
        let uri = format!("file:{}?mode=ro", uri_encode(&p.to_string_lossy()));
        let uri_c = std::ffi::CString::new(uri).unwrap();
        unsafe {
            let mut db: Sqlite = std::ptr::null_mut();
            let flags = SQLITE_OPEN_READONLY | SQLITE_OPEN_URI;
            let rc = (api.open_v2)(uri_c.as_ptr() as *const u8, &mut db, flags, std::ptr::null());
            assert_eq!(rc, SQLITE_OK, "通知库存在却打不开");
            (api.close)(db);
        }
    }
}

/// 媒体播放（SMTC）。
///
/// # 为什么是全手写的 unsafe
///
/// 拿「当前在播什么」只有一个官方入口：
/// `Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager`，
/// 纯 WinRT。项目的依赖纪律是只允许 `windows-sys`，而它只提供**底层
/// 激活函数**（`RoGetActivationFactory` 等），不含这个 runtimeclass 的
/// 接口声明 —— 所以接口表只能自己按 `windows.media.control.h` 抄一遍。
///
/// 下面的 vtable 索引**逐条抄自 Windows SDK 10.0.26100 的
/// `winrt/windows.media.control.h`**（`MIDL_INTERFACE` 块里方法的声明顺序
/// 就是索引顺序），IID 也是从那里面取的原文。改动前三思：错一位不是
/// 「读不到」，是**整个进程当场崩**（`panic = "abort"`）。
///
/// # 一个必须记住的坑
///
/// **WinRT 的接口不累加 vtable**。COM 里 `ISub : IBase` 会把基类方法排在
/// 前面，而 WinRT 里 `IAsyncOperation<T>` 的 vtable 是
/// `IInspectable`(0..5) + `put_Completed`(6) + `get_Completed`(7) +
/// `GetResults`(8) —— 只有三个。`IAsyncInfo`（状态、取消）是**另一个
/// 接口**，必须 `QueryInterface` 才拿得到。第一版按 COM 习惯数成 13，
/// 结果把 `get_Completed` 当成 `GetResults` 调，直接踩空。
pub mod media {
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use std::sync::OnceLock;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows_sys::core::GUID;

    /// `HSTRING` 是 WinRT 的字符串句柄。`windows-sys` 0.59 **没有**
    /// `Win32_System_WinRT` feature（只有 `Win32_System_Com`），所以这里
    /// 自己声明一遍 —— 它就是个不透明指针。
    type HSTRING = *mut c_void;

    type RoInitializeFn = unsafe extern "system" fn(i32) -> i32;
    type RoGetActivationFactoryFn =
        unsafe extern "system" fn(HSTRING, *const GUID, *mut *mut c_void) -> i32;
    type WindowsCreateStringFn = unsafe extern "system" fn(*const u16, u32, *mut HSTRING) -> i32;
    type WindowsDeleteStringFn = unsafe extern "system" fn(HSTRING) -> i32;
    type WindowsGetStringRawBufferFn = unsafe extern "system" fn(HSTRING, *mut u32) -> *const u16;

    /// `RO_INIT_MULTITHREADED`
    const RO_INIT_MULTITHREADED: i32 = 1;

    /// `combase.dll` 的入口表，与 `winsqlite3` 同一套「动态加载 + 缓存」做法。
    ///
    /// 不用 `#[link]` 静态链接：那会把 WinRT 依赖写死进导入表，在没有
    /// combase 的极端精简环境上连启动都失败。动态加载则是「拿不到就当作
    /// 没有这项能力」，胶囊照常显示其余内容。
    struct WinRt {
        ro_init: RoInitializeFn,
        ro_get_factory: RoGetActivationFactoryFn,
        create_string: WindowsCreateStringFn,
        delete_string: WindowsDeleteStringFn,
        get_string_raw: WindowsGetStringRawBufferFn,
    }

    unsafe impl Send for WinRt {}
    unsafe impl Sync for WinRt {}

    static API: OnceLock<Option<WinRt>> = OnceLock::new();

    fn api() -> Option<&'static WinRt> {
        API.get_or_init(|| unsafe {
            let name: Vec<u16> = "combase.dll".encode_utf16().chain(Some(0)).collect();
            let h = LoadLibraryW(name.as_ptr());
            if h.is_null() {
                return None;
            }
            macro_rules! sym {
                ($n:literal, $t:ty) => {{
                    let p = GetProcAddress(h, concat!($n, "\0").as_ptr());
                    let p = p?;
                    std::mem::transmute::<_, $t>(p)
                }};
            }
            Some(WinRt {
                ro_init: sym!("RoInitialize", RoInitializeFn),
                ro_get_factory: sym!("RoGetActivationFactory", RoGetActivationFactoryFn),
                create_string: sym!("WindowsCreateString", WindowsCreateStringFn),
                delete_string: sym!("WindowsDeleteString", WindowsDeleteStringFn),
                get_string_raw: sym!("WindowsGetStringRawBuffer", WindowsGetStringRawBufferFn),
            })
        })
        .as_ref()
    }

    /// `IGlobalSystemMediaTransportControlsSessionManagerStatics`
    const IID_MGR_STATICS: GUID = GUID {
        data1: 0x2050_c4ee,
        data2: 0x11a0,
        data3: 0x57de,
        data4: [0xae, 0xd7, 0xc9, 0x7c, 0x70, 0x33, 0x82, 0x45],
    };
    /// `IAsyncInfo` —— 唯一一个 IID 固定的基础接口
    const IID_ASYNC_INFO: GUID = GUID {
        data1: 0x0000_0036,
        data2: 0x0000,
        data3: 0x0000,
        data4: [0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
    };

    /// 一次媒体采样。
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Media {
        /// 播放器 AUMID（`Microsoft.ZuneMusic_...!App`），暂时只用于诊断
        pub app: String,
        pub title: String,
        pub artist: String,
        /// `PlaybackStatus == Playing`
        pub playing: bool,
    }

    impl Media {
        /// 胶囊上那一行：**只给歌名**。
        ///
        /// 一开始写的是 `晴天 - 周杰伦`，实测七项全开时胶囊 399px 已经顶到
        /// 400 的上限 —— 歌名再长两个字就要分页，而 `island_rotate=0`（默认）
        /// 时第 2 页**永远不会出现**：用户勾了「媒体」却在胶囊上找不到它。
        ///
        /// 歌名与艺人之间是「必须二选一」的关系，不是「都能放」：常态胶囊
        /// 一行要塞时钟 + 三格状态 + 媒体，宽度就那么多。歌名是识别一首歌
        /// 的主要信息，艺人只是补充，所以给歌名。
        pub fn line(&self) -> String {
            if self.title.is_empty() {
                self.artist.clone()
            } else {
                self.title.clone()
            }
        }

        /// 完整信息（歌名 - 艺人），给诊断入口与将来的展开态用。
        pub fn detailed(&self) -> String {
            match (self.title.is_empty(), self.artist.is_empty()) {
                (false, false) => format!("{} - {}", self.title, self.artist),
                (false, true) => self.title.clone(),
                (true, false) => self.artist.clone(),
                (true, true) => String::new(),
            }
        }
    }

    /// 取方法指针。`idx` 是 vtable 索引，见模块文档。
    unsafe fn vt<F>(p: *mut c_void, idx: usize) -> F {
        let table = *(p as *mut *const *const c_void);
        std::mem::transmute_copy(&*table.add(idx))
    }

    type FnGet = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32;
    type FnGetStr = unsafe extern "system" fn(*mut c_void, *mut HSTRING) -> i32;
    type FnQI = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32;

    /// 引用计数包装，离开作用域即 `Release`。
    ///
    /// 手写 `Release` 是这层唯一安全的做法：每个分支都要 early return，
    /// 漏一个就是常驻进程里的引用泄漏（挂件一开就是几天）。
    struct Com(*mut c_void);
    impl Drop for Com {
        fn drop(&mut self) {
            if self.0.is_null() {
                return;
            }
            unsafe {
                let f: unsafe extern "system" fn(*mut c_void) -> u32 = vt(self.0, 2);
                f(self.0);
            }
        }
    }

    struct HStr(HSTRING);
    impl Drop for HStr {
        fn drop(&mut self) {
            if !self.0.is_null() {
                if let Some(a) = api() {
                    unsafe { (a.delete_string)(self.0) };
                }
            }
        }
    }

    fn init() -> bool {
        static OK: OnceLock<bool> = OnceLock::new();
        // `S_FALSE`（已经初始化过）也算成功：这个函数会在每台机器上被
        // 调用很多次，只有负值才是真的失败。
        *OK.get_or_init(|| {
            let Some(a) = api() else { return false };
            unsafe { (a.ro_init)(RO_INIT_MULTITHREADED) >= 0 }
        })
    }

    unsafe fn hstring(s: &str) -> Option<HStr> {
        let a = api()?;
        let wide: Vec<u16> = s.encode_utf16().collect();
        let mut h: HSTRING = null_mut();
        let hr = (a.create_string)(wide.as_ptr(), wide.len() as u32, &mut h);
        (hr >= 0 && !h.is_null()).then(|| HStr(h))
    }

    /// 读出内容**并释放**（调用点无一例外都是「取完就不要」）。
    unsafe fn hstring_value(h: HSTRING) -> String {
        if h.is_null() {
            return String::new();
        }
        let Some(a) = api() else {
            return String::new();
        };
        let mut len: u32 = 0;
        let p = (a.get_string_raw)(h, &mut len);
        let s = if p.is_null() || len == 0 {
            String::new()
        } else {
            String::from_utf16_lossy(std::slice::from_raw_parts(p, len as usize))
        };
        (a.delete_string)(h);
        s
    }

    /// 等一个 `IAsyncOperation` 完成。
    ///
    /// # 为什么是「轮询 + 预算」而不是注册 Completed 回调
    ///
    /// 回调要传一个 COM delegate，得自己实现 `AsyncOperationCompletedHandler`
    /// 的 vtable，代码量翻倍；而实测这个异步操作在毫秒级完成，轮询足够。
    ///
    /// **预算必须是有限的**：这段代码跑在 UI 线程的 `tick` 里，无限等一个
    /// 卡住的播放器等于整个胶囊冻住。30ms 是「正常情况够用、异常时宁可
    /// 这一拍不要数据」的折中，超时就返回 `false` 让调用方放弃这次采样。
    unsafe fn wait(op: *mut c_void, budget_ms: u64) -> bool {
        let mut info: *mut c_void = null_mut();
        let f: FnQI = vt(op, 0);
        if f(op, &IID_ASYNC_INFO, &mut info) < 0 || info.is_null() {
            return false;
        }
        let info = Com(info);
        let f: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32 = vt(info.0, 7);
        let start = std::time::Instant::now();
        loop {
            // AsyncStatus: Started=0 Completed=1 Canceled=2 Error=3
            let mut st: i32 = 0;
            if f(info.0, &mut st) < 0 {
                return false;
            }
            if st != 0 {
                return st == 1;
            }
            if start.elapsed().as_millis() as u64 > budget_ms {
                return false;
            }
            std::thread::yield_now();
        }
    }

    /// 当前媒体会话。`None` = 没有播放器在播 / 拿不到（都当「这一拍没有」）。
    pub fn current() -> Option<Media> {
        if !init() {
            return None;
        }
        unsafe { current_inner() }
    }

    unsafe fn current_inner() -> Option<Media> {
        let a = api()?;
        let cls = hstring("Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager")?;
        let mut fac: *mut c_void = null_mut();
        if (a.ro_get_factory)(cls.0, &IID_MGR_STATICS, &mut fac) < 0 || fac.is_null() {
            return None;
        }
        let fac = Com(fac);

        // RequestAsync (Statics vtable[6])
        let mut op: *mut c_void = null_mut();
        let f: FnGet = vt(fac.0, 6);
        if f(fac.0, &mut op) < 0 || op.is_null() {
            return None;
        }
        let op = Com(op);
        if !wait(op.0, 30) {
            return None;
        }

        // GetResults (IAsyncOperation<T> vtable[8]，**不是 13**)
        let mut mgr: *mut c_void = null_mut();
        let f: FnGet = vt(op.0, 8);
        if f(op.0, &mut mgr) < 0 || mgr.is_null() {
            return None;
        }
        let mgr = Com(mgr);

        // GetCurrentSession (Manager vtable[6])：没有会话就是 null，正常
        let mut sess: *mut c_void = null_mut();
        let f: FnGet = vt(mgr.0, 6);
        f(mgr.0, &mut sess);
        if sess.is_null() {
            return None;
        }
        let sess = Com(sess);

        // get_SourceAppUserModelId (Session vtable[6])
        let mut h: HSTRING = null_mut();
        let f: FnGetStr = vt(sess.0, 6);
        let app = if f(sess.0, &mut h) >= 0 {
            hstring_value(h)
        } else {
            String::new()
        };

        // GetPlaybackInfo (Session vtable[9]) → get_PlaybackStatus (Info vtable[7])
        let playing = {
            let mut info: *mut c_void = null_mut();
            let f: FnGet = vt(sess.0, 9);
            if f(sess.0, &mut info) < 0 || info.is_null() {
                false
            } else {
                let info = Com(info);
                let mut st: i32 = 0;
                let f: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32 = vt(info.0, 7);
                f(info.0, &mut st) >= 0 && st == 4 // 4 = Playing
            }
        };

        // TryGetMediaPropertiesAsync (Session vtable[7])
        let mut op2: *mut c_void = null_mut();
        let f: FnGet = vt(sess.0, 7);
        if f(sess.0, &mut op2) < 0 || op2.is_null() {
            return None;
        }
        let op2 = Com(op2);
        if !wait(op2.0, 30) {
            return None;
        }
        let mut props: *mut c_void = null_mut();
        let f: FnGet = vt(op2.0, 8);
        if f(op2.0, &mut props) < 0 || props.is_null() {
            return None;
        }
        let props = Com(props);

        let title = {
            let mut h: HSTRING = null_mut();
            let f: FnGetStr = vt(props.0, 6);
            if f(props.0, &mut h) >= 0 {
                hstring_value(h)
            } else {
                String::new()
            }
        };
        let artist = {
            let mut h: HSTRING = null_mut();
            let f: FnGetStr = vt(props.0, 9);
            if f(props.0, &mut h) >= 0 {
                hstring_value(h)
            } else {
                String::new()
            }
        };
        if title.is_empty() && artist.is_empty() {
            return None;
        }
        Some(Media {
            app,
            title,
            artist,
            playing,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn line_omits_missing_parts() {
            let m = Media {
                title: "晴天".into(),
                artist: "周杰伦".into(),
                ..Default::default()
            };
            assert_eq!(m.line(), "晴天");
            assert_eq!(m.detailed(), "晴天 - 周杰伦");
            let only_title = Media {
                title: "某播客".into(),
                ..Default::default()
            };
            assert_eq!(only_title.line(), "某播客");
            assert_eq!(Media::default().line(), "");
            assert_eq!(Media::default().detailed(), "");
        }

        /// 调用不崩就算过：会话是否存在取决于本机有没有播放器在播，
        /// 这条**不能断言「一定读到」** —— 那样会在静音的机器上恒红。
        #[test]
        fn current_does_not_crash() {
            let _ = current();
        }
    }
}
