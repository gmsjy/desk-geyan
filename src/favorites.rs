//! 收藏夹：把**当前正在显示的那句**单独存起来，让它进入轮播。
//!
// 触发途径是托盘菜单「收藏当前句子」和 `Ctrl+Alt+F`。挂件常态是鼠标穿透的，
//! 点不到它本身，所以「点击」只能落在托盘上 —— 这也是挂件类程序唯一
//! 不会和「不干扰桌面」打架的入口。
//!
//! 三个设计决定，都是有理由的：
//!
//! 1. **写独立文件 `quotes/favorites.txt`，不追加进用户的 `quotes.txt`。**
//!    用户的源文件往往按主题精心分区（申论素材 152 条就是），
//!    程序往里追一行等于把排版搅乱；而且用户自己编辑那个文件时，
//!    看到一行不知道什么时候冒出来的东西会很困惑。独立文件还让
//!    「清空收藏」变成删一个文件的事。
//! 2. **只在收藏夹内查重，不全库查重。** 收藏的语义是「这句话进我的收藏组」，
//!    不是「这句话全世界唯一」。用户收藏一句内置格言，就是想让它和
//!    古诗、申论一起轮流出现（round-robin 下同组不同条、组间轮流，
//!    效果正是「更常出现」）。反过来若按全库查重，点一句内置格言
//!    会被告知「已在库中」而什么都没发生 —— 那不是用户要的。
//! 3. **写盘和剪贴板是两件独立的事**，任一部分失败不阻止另一部分：
//!    剪贴板可能被别的窗口占着（`OpenClipboard` 会失败），
//!    那是「复制没成」，不该连累「收藏没成」。

use std::io;
use std::path::{Path, PathBuf};

use crate::quotes::{self, Quote};

/// 收藏夹文件名。放在 `quotes/` 目录下，于是它自动成为分组机制里的**一组**，
/// 和用户的 `01_乡村振兴.txt` 们按 round-robin 轮流出现 —— 「加入播放」
/// 不需要任何额外机制，分组轮播本身就干这个。
pub const FILE: &str = "favorites.txt";

/// 收藏夹完整路径：`<data_dir>/quotes/favorites.txt`。
pub fn path(dir: &Path) -> PathBuf {
    dir.join("quotes").join(FILE)
}

/// 收藏夹当前内容。文件不存在 = 还没收藏过任何东西 = 空。
pub fn load(dir: &Path) -> Vec<Quote> {
    quotes::read_external(&path(dir)).unwrap_or_default()
}

/// 收藏夹里这条的**存储形态**。
///
/// 用半角空格包 `——`：`parse_line` 对两侧做 `trim()`，读回来时
/// 正文和出处与写入时逐字相等（`--selftest` 对这条下断言）。
/// 没有出处的句子只写正文，读回来作者仍是空 —— 不会凭空多出一个分隔符。
pub fn line_of(q: &Quote) -> String {
    if q.author.is_empty() {
        q.text.clone()
    } else {
        format!("{} —— {}", q.text, q.author)
    }
}

/// 该不该把这条写进收藏夹。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FavAction {
    /// 收藏夹里没有 → 追加
    Append,
    /// 收藏夹里已经有了 → 什么都不写
    Skip,
}

/// 查重用 `quotes::norm`（去空白、去标点，不碰文字）——
/// 「同一句话写了两遍」才算重复，「看起来像」不算。
/// 于是用户重复点同一句、或点了一句只差空格的变体，都不会写进去。
pub fn decide(existing: &[Quote], q: &Quote) -> FavAction {
    let key = quotes::norm(&q.text);
    if key.is_empty() {
        // 空正文没有收藏价值，也不该把空行写进文件
        return FavAction::Skip;
    }
    for e in existing {
        if quotes::norm(&e.text) == key {
            return FavAction::Skip;
        }
    }
    FavAction::Append
}

/// 追加一条到收藏夹。返回 `Ok(true)` 表示**真的写了**，
/// `Ok(false)` 表示收藏夹里已有（幂等，不重复写）。
///
/// 目录不存在就建 —— 「第一次收藏」时 `quotes/` 往往还不存在。
pub fn append(dir: &Path, q: &Quote) -> io::Result<bool> {
    let existing = load(dir);
    if decide(&existing, q) == FavAction::Skip {
        return Ok(false);
    }
    let p = path(dir);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 追加写。文件以 `\n` 结尾时不重复补 —— 用户手工编辑过的文件
    // 可能没有行尾符，那时补一个，免得两条黏成一行。
    use std::io::Write;
    let mut need_nl = false;
    if let Ok(meta) = std::fs::metadata(&p) {
        if meta.len() > 0 {
            if let Ok(bytes) = std::fs::read(&p) {
                need_nl = bytes.last() != Some(&b'\n');
            }
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)?;
    if need_nl {
        f.write_all(b"\n")?;
    }
    f.write_all(line_of(q).as_bytes())?;
    f.write_all(b"\n")?;
    Ok(true)
}
