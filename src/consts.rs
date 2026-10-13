//! 自持 Win32 常量表。
//!
//! 刻意不依赖 `windows-sys` 的常量导出：常量值本身是 ABI 稳定的事实标准，
//! 自己声明可以完全消除「某个版本的 crate 把这个常量放进了哪个 feature」
//! 这类编译噪音，让 FFI 层只依赖真正的函数签名。

#![allow(dead_code)]

// ---------------------------------------------------------------- 窗口样式
pub const WS_POPUP: u32 = 0x8000_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;

pub const WS_EX_TOPMOST: u32 = 0x0000_0008;
pub const WS_EX_TRANSPARENT: u32 = 0x0000_0020; // 鼠标穿透：点击落到下层窗口
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080; // 不进任务栏 / 不进 Alt-Tab
pub const WS_EX_LAYERED: u32 = 0x0008_0000; // 分层窗口：ULW / LWA 的前提
pub const WS_EX_NOACTIVATE: u32 = 0x0800_0000; // 永不抢焦点，不打断当前输入

pub const GWL_EXSTYLE: i32 = -20;

// ---------------------------------------------------------------- SetWindowPos
pub const SWP_NOSIZE: u32 = 0x0001;
pub const SWP_NOMOVE: u32 = 0x0002;
pub const SWP_NOZORDER: u32 = 0x0004;
pub const SWP_NOACTIVATE: u32 = 0x0010;
pub const SWP_FRAMECHANGED: u32 = 0x0020;
pub const SWP_NOOWNERZORDER: u32 = 0x0200;
pub const SWP_NOSENDCHANGING: u32 = 0x0400;

pub const HWND_TOPMOST: isize = -1;
pub const HWND_NOTOPMOST: isize = -2;

// ---------------------------------------------------------------- 窗口消息
pub const WM_DESTROY: u32 = 0x0002;
pub const WM_MOVE: u32 = 0x0003;
pub const WM_SIZE: u32 = 0x0005;
pub const WM_PAINT: u32 = 0x000F;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_ERASEBKGND: u32 = 0x0014;
pub const WM_DISPLAYCHANGE: u32 = 0x007E;
pub const WM_NCHITTEST: u32 = 0x0084;
pub const WM_TIMER: u32 = 0x0113;
pub const WM_WINDOWPOSCHANGED: u32 = 0x0047;
pub const WM_SETTINGCHANGE: u32 = 0x001A;
pub const WM_EXITSIZEMOVE: u32 = 0x0232;
pub const WM_ENTERSIZEMOVE: u32 = 0x0231;
pub const WM_HOTKEY: u32 = 0x0312;
pub const WM_MOUSEMOVE: u32 = 0x0200;
pub const WM_LBUTTONDOWN: u32 = 0x0201;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_LBUTTONDBLCLK: u32 = 0x0203;
pub const WM_RBUTTONUP: u32 = 0x0205;
pub const WM_CAPTURECHANGED: u32 = 0x0215;
pub const WM_SETCURSOR: u32 = 0x0020;
/// PrintWindow：要求把 DWM 合成后的内容画出来（分层窗口只能这样读回）
pub const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;
pub const PM_REMOVE: u32 = 0x0001;
pub const WM_NULL: u32 = 0x0000;
pub const WM_APP: u32 = 0x8000;
/// 托盘图标的自定义回调消息
pub const WM_TRAY: u32 = WM_APP + 1;

// ---------------------------------------------------------------- 窗口几何
/// 窗口最小尺寸。拖动缩放时的夹取和配置加载时的夹取共用这一对值，
/// 两处各写一个数字迟早会走偏。
pub const MIN_W: i32 = 240;
pub const MIN_H: i32 = 80;
/// 拖动时至少留在工作区内的像素数（免得手一滑把挂件推出屏幕找不回来）
pub const KEEP_VISIBLE: i32 = 80;

// ---------------------------------------------------------------- 命中测试
pub const HTTRANSPARENT: isize = -1;
pub const HTCLIENT: isize = 1;
pub const HTCAPTION: isize = 2;
pub const HTLEFT: isize = 10;
pub const HTRIGHT: isize = 11;
pub const HTTOP: isize = 12;
pub const HTTOPLEFT: isize = 13;
pub const HTTOPRIGHT: isize = 14;
pub const HTBOTTOM: isize = 15;
pub const HTBOTTOMLEFT: isize = 16;
pub const HTBOTTOMRIGHT: isize = 17;

// ---------------------------------------------------------------- ShowWindow
pub const SW_HIDE: i32 = 0;
pub const SW_SHOWNOACTIVATE: i32 = 4;

// ---------------------------------------------------------------- 分层窗口属性
pub const LWA_ALPHA: u32 = 0x0000_0002;
pub const LWA_COLORKEY: u32 = 0x0000_0001;
/// UpdateLayeredWindow 的有效标志位
pub const ULW_ALPHA: u32 = 0x0000_0002;
pub const AC_SRC_OVER: u8 = 0x00;
pub const AC_SRC_ALPHA: u8 = 0x01;

// ---------------------------------------------------------------- Accent（毛玻璃）
pub const WCA_ACCENT_POLICY: i32 = 19;
pub const ACCENT_DISABLED: i32 = 0;
pub const ACCENT_ENABLE_BLURBEHIND: i32 = 3;
pub const ACCENT_ENABLE_ACRYLICBLURBEHIND: i32 = 4;

// ---------------------------------------------------------------- DWM
/// DwmSetWindowAttribute 的属性号：圆角偏好
pub const DWMWA_WINDOW_CORNER_PREFERENCE: i32 = 33;
pub const DWMWCP_ROUND: i32 = 2;
pub const DWMWCP_DONOTROUND: i32 = 1;

// ---------------------------------------------------------------- 全局热键
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_NOREPEAT: u32 = 0x4000;

pub const VK_LEFT: u32 = 0x25;
pub const VK_UP: u32 = 0x26;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_DOWN: u32 = 0x28;
pub const VK_C: u32 = 0x43;
pub const VK_D: u32 = 0x44;
pub const VK_E: u32 = 0x45;
pub const VK_H: u32 = 0x48;
pub const VK_K: u32 = 0x4B;
pub const VK_M: u32 = 0x4D;
pub const VK_P: u32 = 0x50;
pub const VK_R: u32 = 0x52;
pub const VK_S: u32 = 0x53;
pub const VK_T: u32 = 0x54;
pub const VK_V: u32 = 0x56;
pub const VK_X: u32 = 0x58;
/// `Ctrl+Alt+=` / `Ctrl+Alt+-` 调字号
pub const VK_OEM_PLUS: u32 = 0xBB;
pub const VK_OEM_MINUS: u32 = 0xBD;

// ---------------------------------------------------------------- GDI
pub const BI_RGB: u32 = 0;
pub const DIB_RGB_COLORS: u32 = 0;
pub const TRANSPARENT: i32 = 1;
pub const DEFAULT_CHARSET: u32 = 1;
pub const OUT_TT_PRECIS: u32 = 4;
pub const CLIP_DEFAULT_PRECIS: u32 = 0;
pub const FF_DONTCARE: u32 = 0;
pub const FW_NORMAL: i32 = 400;
pub const FW_SEMIBOLD: i32 = 600;
pub const FW_BOLD: i32 = 700;
/// 灰度抗锯齿（不要用 CLEARTYPE_QUALITY，次像素渲染会给掩码带上彩色边）
pub const ANTIALIASED_QUALITY: u32 = 4;

pub const WHITE: u32 = 0x00FF_FFFF;
pub const BLACK: u32 = 0x0000_0000;

// ---------------------------------------------------------------- 系统
pub const SPI_GETWORKAREA: u32 = 0x0030;
pub const ERROR_HOTKEY_ALREADY_EXISTS: u32 = 1409;
/// 单实例互斥量已存在
pub const ERROR_ALREADY_EXISTS: u32 = 183;
pub const CS_HREDRAW: u32 = 0x0002;
pub const CS_VREDRAW: u32 = 0x0001;
pub const IDC_ARROW_RES: *const u16 = 32512 as *const u16;
/// 拖动时的光标：整体移动 / 横向 / 纵向 / 两个斜向
pub const IDC_SIZEALL_RES: *const u16 = 32646 as *const u16;
pub const IDC_SIZEWE_RES: *const u16 = 32644 as *const u16;
pub const IDC_SIZENS_RES: *const u16 = 32645 as *const u16;
pub const IDC_SIZENWSE_RES: *const u16 = 32642 as *const u16;
pub const IDC_SIZENESW_RES: *const u16 = 32643 as *const u16;
pub const IDI_APPLICATION_RES: *const u16 = 32512 as *const u16;

// ---------------------------------------------------------------- 托盘
pub const NIM_ADD: u32 = 0x0000_0000;
pub const NIM_MODIFY: u32 = 0x0000_0001;
pub const NIM_DELETE: u32 = 0x0000_0002;
pub const NIF_MESSAGE: u32 = 0x0000_0001;
pub const NIF_ICON: u32 = 0x0000_0002;
pub const NIF_TIP: u32 = 0x0000_0004;

// ---------------------------------------------------------------- 菜单
pub const MF_STRING: u32 = 0x0000_0000;
pub const MF_SEPARATOR: u32 = 0x0000_0800;
pub const MF_CHECKED: u32 = 0x0000_0008;
pub const MF_GRAYED: u32 = 0x0000_0001;
pub const MF_POPUP: u32 = 0x0000_0010;
pub const TPM_LEFTALIGN: u32 = 0x0000;
pub const TPM_RIGHTBUTTON: u32 = 0x0002;
pub const TPM_RETURNCMD: u32 = 0x0100;
pub const TPM_NONOTIFY: u32 = 0x0080;
