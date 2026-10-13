//! 未文档化 / 未在 SDK 导入库中导出的 Win32 入口。
//!
//! 只有两件事必须自己处理：
//!
//! * `SetWindowCompositionAttribute` —— 毛玻璃（Acrylic / BlurBehind）的唯一入口。
//!   它**被 user32.dll 导出，但不在 SDK 的 user32.lib 导入库里**，
//!   所以不能 `#[link]` 声明（会 LNK2019），只能运行时 `GetProcAddress` 取地址。
//!   这也是它至今没有官方头文件的直接后果。
//! * `SetProcessDpiAwarenessContext` —— 分层窗口的位置/尺寸都是物理像素，
//!   不声明 DPI 感知的话，高 DPI 屏上系统会先放大一次、我们再按物理像素画一次，字会糊。
//!   这个在 user32.lib 里有，正常链接即可。

use std::ffi::c_void;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{BOOL, HWND};
use windows_sys::Win32::Graphics::Gdi::HDC;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LoadLibraryW,
};

/// 传给 `SetWindowCompositionAttribute` 的 accent 策略。
///
/// 字段顺序必须与 user32 内部的 `AccentPolicy` 完全一致 ——
/// 这块内存会被 user32 按自己的理解解释。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AccentPolicy {
    /// ACCENT_STATE：0 关闭 / 3 BlurBehind / 4 AcrylicBlurBehind
    pub accent_state: i32,
    pub accent_flags: u32,
    /// 0xAABBGGRR —— 最高字节是 tint 的 alpha，正是「毛玻璃浓度」的旋钮
    pub gradient_color: u32,
    pub animation_id: u32,
}

#[repr(C)]
pub struct WinCompAttrData {
    /// WCA_ACCENT_POLICY = 19
    pub attribute: i32,
    pub data: *const c_void,
    pub size_of_data: usize,
}

type SetWcaFn = unsafe extern "system" fn(HWND, *const WinCompAttrData) -> BOOL;

fn resolve_set_wca() -> Option<SetWcaFn> {
    static CACHE: OnceLock<Option<SetWcaFn>> = OnceLock::new();
    *CACHE.get_or_init(|| unsafe {
        // 必须显式给出 user32.dll：GetProcAddress **只**在指定模块的导出表里找，
        // 传 GetModuleHandle(NULL)（主模块）是找不到 user32 的函数的。
        let name = crate::util::wide("user32.dll");
        let module = GetModuleHandleW(name.as_ptr());
        if module.is_null() {
            return None;
        }
        let addr = GetProcAddress(module, c"SetWindowCompositionAttribute".as_ptr() as *const u8);
        // FARPROC 在 windows-sys 里是 Option<extern fn>，没解析到就是 None
        addr.map(|a| std::mem::transmute::<_, SetWcaFn>(a))
    })
}

/// 解析是否成功（诊断用）
pub fn accent_api_available() -> bool {
    resolve_set_wca().is_some()
}

/// 返回 `false` 表示当前系统不支持（Win7 / 未加载 user32 / 该 API 被移除）
pub fn set_window_composition_attribute(hwnd: HWND, data: &WinCompAttrData) -> bool {
    match resolve_set_wca() {
        Some(f) => unsafe { f(hwnd, data) != 0 },
        None => false,
    }
}

type PrintWindowFn = unsafe extern "system" fn(HWND, HDC, u32) -> BOOL;

/// `PrintWindow(hwnd, hdc, PW_RENDERFULLCONTENT)`：把窗口按 DWM 合成结果画进 DC。
///
/// windows-sys 把它归在 `Win32_Storage_Xps` 特性下（历史原因），
/// 为一个测试用的读回通道把整个 Xps 特性拉进来不值当 ——
/// 仍然走运行时解析这条老路，user32.dll 本来就在进程里。
pub fn print_window(hwnd: HWND, hdc: HDC, flags: u32) -> bool {
    static CACHE: OnceLock<Option<PrintWindowFn>> = OnceLock::new();
    let f = *CACHE.get_or_init(|| unsafe {
        let name = crate::util::wide("user32.dll");
        let module = GetModuleHandleW(name.as_ptr());
        if module.is_null() {
            return None;
        }
        GetProcAddress(module, c"PrintWindow".as_ptr() as *const u8)
            .map(|a| std::mem::transmute::<_, PrintWindowFn>(a))
    });
    match f {
        Some(f) => unsafe { f(hwnd, hdc, flags) != 0 },
        None => false,
    }
}

#[link(name = "user32")]
extern "system" {
    /// 传入 `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`（= -4）
    pub fn SetProcessDpiAwarenessContext(ctx: *mut c_void) -> BOOL;

    /// V2 不可用时的降级（Win7 / Win8.1）
    pub fn SetProcessDPIAware();
}

/// `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`
pub const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: *mut c_void = -4isize as *mut c_void;

// ---------------------------------------------------------------- 资源计数
//
// `--memtest` 要回答的是「长时间跑会不会涨」。光看 Private Bytes 不够：
// GDI 对象、USER 对象、内核句柄这三样**各自有独立配额**（GDI 默认 10000），
// 泄漏的话先撞配额而不是先爆内存，表现是「画着画着就不刷新了」。
// 这三样也都不在 SDK 的常规导入路径上，同样走运行时解析。

/// `PROCESS_MEMORY_COUNTERS_EX` 的前半段布局（与 SDK 一致）。
///
/// 只用得上前两个字段，但**必须整个结构都对**：`GetProcessMemoryInfo` 会按
/// `cb` 指定的长度往这块内存里写，少给一个字段就是缓冲区溢出。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MemCounters {
    pub cb: u32,
    pub page_fault_count: u32,
    pub peak_working_set: usize,
    pub working_set: usize,
    pub quota_peak_paged: usize,
    pub quota_paged: usize,
    pub quota_peak_nonpaged: usize,
    pub quota_nonpaged: usize,
    pub pagefile: usize,
    pub peak_pagefile: usize,
    /// 这一项是 `_EX` 版本才有的，也就是「私有提交内存」
    pub private_usage: usize,
}

type GetMemFn = unsafe extern "system" fn(*mut c_void, *mut MemCounters, u32) -> BOOL;

/// 进程内存：`(Private Bytes, Working Set)`，字节。
///
/// `Private Bytes`（私有提交）才是判断泄漏的那个数 —— Working Set 会被
/// 系统按压力随时回收，一个泄漏的程序在工作集上可能看不出问题。
pub fn process_memory() -> Option<(usize, usize)> {
    static CACHE: OnceLock<Option<GetMemFn>> = OnceLock::new();
    let f = *CACHE.get_or_init(|| unsafe {
        // psapi.dll 未必已经被加载进本进程，`GetModuleHandleW` 拿不到就白跑；
        // 这里用 `LoadLibraryW` 显式拉一次。符号名也有两套：psapi 里叫
        // `GetProcessMemoryInfo`，kernel32 转发过去的是 `K32GetProcessMemoryInfo`。
        for (dll, sym) in [
            ("psapi.dll", c"GetProcessMemoryInfo"),
            ("kernel32.dll", c"K32GetProcessMemoryInfo"),
        ] {
            let name = crate::util::wide(dll);
            let h = {
                let m = GetModuleHandleW(name.as_ptr());
                if m.is_null() {
                    LoadLibraryW(name.as_ptr())
                } else {
                    m
                }
            };
            if h.is_null() {
                continue;
            }
            if let Some(a) = GetProcAddress(h, sym.as_ptr() as *const u8) {
                return Some(std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    GetMemFn,
                >(a));
            }
        }
        None
    });
    let f = f?;
    let mut c: MemCounters = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<MemCounters>() as u32;
    let me = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() };
    let ok = unsafe { f(me, &mut c, c.cb) };
    (ok != 0).then_some((c.private_usage, c.working_set))
}

type GetGuiResFn = unsafe extern "system" fn(*mut c_void, u32) -> u32;

/// 本进程的 `(GDI 对象数, USER 对象数)`。
///
/// GDI 对象是 pens / brushes / fonts / bitmaps 的总额（默认上限 10000），
/// USER 对象是窗口 / 菜单 / 光标。**两者都必须显式销毁**：
/// 它们的计数不会因为 Rust 的 Drop 而自动减少 —— 手写的 `CreateXxx`
/// 就得手写对应的 `DeleteObject`，漏一个就永久占一格。
pub fn gui_resources() -> Option<(u32, u32)> {
    const GR_GDIOBJECTS: u32 = 0;
    const GR_USEROBJECTS: u32 = 1;
    static CACHE: OnceLock<Option<GetGuiResFn>> = OnceLock::new();
    let f = *CACHE.get_or_init(|| unsafe {
        let name = crate::util::wide("user32.dll");
        let m = GetModuleHandleW(name.as_ptr());
        if m.is_null() {
            return None;
        }
        GetProcAddress(m, c"GetGuiResources".as_ptr() as *const u8)
            .map(|a| std::mem::transmute::<_, GetGuiResFn>(a))
    });
    let f = f?;
    let me = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() };
    let gdi = unsafe { f(me, GR_GDIOBJECTS) };
    let user = unsafe { f(me, GR_USEROBJECTS) };
    Some((gdi, user))
}

type GetHandleCountFn = unsafe extern "system" fn(*mut c_void, *mut u32) -> BOOL;

/// 本进程打开的内核句柄数（文件 / 事件 / 互斥量 / 线程…）。
pub fn handle_count() -> Option<u32> {
    static CACHE: OnceLock<Option<GetHandleCountFn>> = OnceLock::new();
    let f = *CACHE.get_or_init(|| unsafe {
        let name = crate::util::wide("kernel32.dll");
        let m = GetModuleHandleW(name.as_ptr());
        if m.is_null() {
            return None;
        }
        GetProcAddress(m, c"GetProcessHandleCount".as_ptr() as *const u8)
            .map(|a| std::mem::transmute::<_, GetHandleCountFn>(a))
    });
    let f = f?;
    let me = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() };
    let mut n: u32 = 0;
    let ok = unsafe { f(me, &mut n) };
    (ok != 0).then_some(n)
}
