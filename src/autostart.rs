//! 开机自启：`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下的一个值。
//!
//! 这个目录是**多家共用**的 —— 用户的机器上可能同时躺着 Steam、输入法、
//! 旧版 desk-geyan、用户自己加的东西。所以三条硬规矩：
//!
//! 1. **只碰自己的值名**（`DeskGeyan` / `DeskGeyan_prev`）。永远不
//!    `RegDeleteTree`、不遍历、不删别人的项。关闭自启 ≠ 卸载，
//!    更不是「把这个键清干净」。
//! 2. **写入前先读回原值**。如果 `DeskGeyan` 这个值名已经被别的东西占着
//!    （旧版本 exe 挪了位置、用户自己建的同名项），先把原值备份到
//!    `DeskGeyan_prev` 再写自己的。
//! 3. 关闭自启时：删 `DeskGeyan`；若备份存在则**原样恢复**，再删备份。
//!    于是「开 → 关」一圈下来，注册表回到动手前的状态。
//!
//! 注册表是**唯一状态源**，不写进 `config.ini`：两处各存一份必然打架
//! （用户在 `msconfig` / 管家软件里改了启动项，配置文件不会知道）。
//! 托盘菜单的勾选态每次开菜单都重新读注册表，就是为了反映这种外部改动。

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::consts::*;
use crate::ffi;
use crate::util::wide;

/// 我们的启动项值名。备份值名同前缀 —— 真出了事，用户搜 `DeskGeyan`
/// 就能把这两个一起认出来。
const VALUE: &str = "DeskGeyan";
const VALUE_PREV: &str = "DeskGeyan_prev";
/// 标准「运行」键，所有「开机自启」教程指的就是这里
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

/// 最近一次注册表操作的原始返回码（`WIN32_ERROR`）。托盘菜单弹不出错误框，
/// 只能把它存下来让 `--probe` 报 —— 「开关点了没反应」必须有个地方能看到原因。
static LAST_ERR: AtomicU32 = AtomicU32::new(0);

/// 最近一次失败的 `WIN32_ERROR`（0 = 无失败）
pub fn last_err() -> u32 {
    LAST_ERR.load(Ordering::Relaxed)
}

/// 一次开关操作该做什么。
///
/// 抽成纯函数是为了能对**所有边界组合**下断言而不碰真实注册表 ——
/// 「写坏了别人的启动项」这种事，靠肉眼看代码是看不出来的。
#[derive(PartialEq, Eq, Debug)]
pub enum Action {
    /// 无现值，直接写
    Write,
    /// 现值不是我们的：先备份再写
    BackupAndWrite,
    /// 删我们的值，无备份
    Delete,
    /// 删我们的值，并把备份恢复回去
    DeleteAndRestore,
    /// 已经是目标状态
    Noop,
}

/// 比较用的归一化：去引号、去空白、忽略大小写。
/// Windows 路径大小写不敏感，而注册表里存的常是 `"C:\...\desk-geyan.exe"`
/// （带引号防空格），不归一化就会把「我们自己的项」误判成「别人的项」，
/// 然后走一遍无谓的备份。
fn norm(v: &str) -> String {
    v.trim().trim_matches('"').to_ascii_lowercase()
}

/// `current` = `DeskGeyan` 现值；`prev` = `DeskGeyan_prev` 现值；`exe` = 本进程路径。
pub fn decide(current: Option<&str>, prev: Option<&str>, exe: &str, want: bool) -> Action {
    let mine = norm(exe);
    let cur_is_mine = current.map(norm).as_deref() == Some(mine.as_str());
    if want {
        if cur_is_mine {
            Action::Noop
        } else if current.is_some() {
            // 占了坑的不是我们：备份后覆盖
            Action::BackupAndWrite
        } else {
            Action::Write
        }
    } else if current.is_some() {
        if prev.is_some() {
            Action::DeleteAndRestore
        } else {
            Action::Delete
        }
    } else if prev.is_some() {
        // 我们的值没了但备份还在（异常状态）：把别人的东西还回去
        Action::DeleteAndRestore
    } else {
        Action::Noop
    }
}

// ---------------------------------------------------------------- 注册表 I/O

fn open_key(sam: u32) -> Option<*mut c_void> {
    let mut key: *mut c_void = null_mut();
    let sub = wide(RUN_KEY);
    // advapi32 在导入库里，链接即得；Open 失败只可能是权限或键被删
    let r = unsafe { ffi::RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, sam, &mut key) };
    if r == ERROR_SUCCESS {
        Some(key)
    } else {
        LAST_ERR.store(r, Ordering::Relaxed);
        None
    }
}

/// 读一个 REG_SZ 值。值不存在返回 `None`（`ERROR_FILE_NOT_FOUND` 是正常情况，
/// 不是错误 —— 「没开过自启」和「读失败」对调用方是同一件事）。
fn read_value(name: &str) -> Option<String> {
    let key = open_key(KEY_READ)?;
    let n = wide(name);
    // 路径放 1024 字节（= 512 个 UTF-16 单元）足够；真碰到了 MORE_DATA
    // 就放弃而不是去猜长度 —— 那种值本来就不是正常路径。
    let mut buf = [0u8; 1024];
    let mut len: u32 = buf.len() as u32;
    let mut typ: u32 = 0;
    let r = unsafe {
        ffi::RegQueryValueExW(
            key,
            n.as_ptr(),
            null_mut(),
            &mut typ,
            buf.as_mut_ptr(),
            &mut len,
        )
    };
    unsafe { ffi::RegCloseKey(key) };
    if r != ERROR_SUCCESS || typ != REG_SZ || len < 2 || len as usize > buf.len() {
        if r != ERROR_SUCCESS && r != ERROR_FILE_NOT_FOUND {
            LAST_ERR.store(r, Ordering::Relaxed);
        }
        return None;
    }
    let units = len as usize / 2;
    let s = String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(buf.as_ptr() as *const u16, units)
    });
    let s = s.trim_end_matches('\0').to_string();
    (!s.is_empty()).then_some(s)
}

fn write_value(name: &str, val: &str) -> bool {
    let Some(key) = open_key(KEY_WRITE) else {
        return false;
    };
    let n = wide(name);
    let w = wide(val); // 自带 NUL 结尾
    let bytes =
        unsafe { std::slice::from_raw_parts(w.as_ptr() as *const u8, w.len() * size_of::<u16>()) };
    let r = unsafe { ffi::RegSetValueExW(key, n.as_ptr(), 0, REG_SZ, bytes.as_ptr(), bytes.len() as u32) };
    unsafe { ffi::RegCloseKey(key) };
    if r != ERROR_SUCCESS {
        LAST_ERR.store(r, Ordering::Relaxed);
    }
    r == ERROR_SUCCESS
}

/// 删除一个值。**值不存在也算成功** —— 幂等，免得调用方为「本来就没开过」
/// 专门分一条支。
fn delete_value(name: &str) -> bool {
    let Some(key) = open_key(KEY_WRITE) else {
        return false;
    };
    let n = wide(name);
    let r = unsafe { ffi::RegDeleteValueW(key, n.as_ptr()) };
    unsafe { ffi::RegCloseKey(key) };
    if r != ERROR_SUCCESS && r != ERROR_FILE_NOT_FOUND {
        LAST_ERR.store(r, Ordering::Relaxed);
    }
    r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND
}

// ---------------------------------------------------------------- 对外接口

/// 本进程 exe 的完整路径。启动项必须写绝对路径（Run 键的工作目录是未定义的）。
pub fn exe_path() -> String {
    let mut buf = vec![0u16; 260];
    // 截断时 windows 会返回写入长度并补 NUL；路径超长时退化为「空」而不 panic
    let n = unsafe {
        windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW(
            null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    };
    if n == 0 || n as usize >= buf.len() {
        return String::new();
    }
    buf.truncate(n as usize);
    String::from_utf16_lossy(&buf)
}

/// 当前是否处于「已启用且指向本 exe」状态。
pub fn enabled() -> bool {
    let exe = exe_path();
    !exe.is_empty() && read_value(VALUE).as_deref() == Some(exe.as_str())
}

/// 启动项存在但**不指向当前 exe**（exe 被挪窝 / 用户自己改过）。
///
/// 这比「没开」更坏：用户以为设了自启，开机时静默失败，而且没有任何提示。
/// `--probe` 判的就是这一条 —— 它只可能在 exe 移动后变红。
pub fn is_stale() -> bool {
    let exe = exe_path();
    if exe.is_empty() {
        return false;
    }
    match read_value(VALUE) {
        Some(v) => norm(&v) != norm(&exe),
        None => false,
    }
}

/// 开 / 关开机自启。返回**读回验证后**的真实状态。
///
/// 「写入返回成功」不算成功 —— 必须重新读一次注册表确认落上了：
/// 这条项目已经在样式位上栽过一次（属性写了不等于行为成立）。
pub fn apply(on: bool) -> bool {
    let exe = exe_path();
    if exe.is_empty() {
        return false;
    }
    let current = read_value(VALUE);
    let prev = read_value(VALUE_PREV);
    let ok = match decide(current.as_deref(), prev.as_deref(), &exe, on) {
        Action::Noop => true,
        Action::Write => write_value(VALUE, &exe),
        Action::BackupAndWrite => {
            // 备份失败就不写：宁可保持原状，也不能把别人的启动项弄丢
            let cur = current.clone().unwrap_or_default();
            if !write_value(VALUE_PREV, &cur) {
                return false;
            }
            write_value(VALUE, &exe)
        }
        Action::Delete => delete_value(VALUE),
        Action::DeleteAndRestore => {
            let restored = prev.as_deref().map(|p| write_value(VALUE, p)).unwrap_or(false);
            delete_value(VALUE_PREV) && (restored || prev.is_none())
        }
    };
    if !ok {
        return false;
    }
    LAST_ERR.store(0, Ordering::Relaxed);
    // 读回验证：最终状态必须正好是「想要的状态」
    let now = read_value(VALUE);
    let now_is_mine = now.as_deref() == Some(exe.as_str());
    on == now_is_mine && (on || read_value(VALUE_PREV).is_none())
}

/// 当前值的详情（诊断用，`--probe` 读它出报告）。
pub fn detail() -> String {
    match read_value(VALUE) {
        Some(v) => format!("{} = {}", VALUE, v),
        None => format!("{} = （未设置）", VALUE),
    }
}
