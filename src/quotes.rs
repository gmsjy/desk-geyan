//! 格言库。
//!
//! **内置库**是一部经典中文格言集（317 条，分四节：经典格言 · 诗词名句 ·
//! 谚语俗语 · 名人名言），存放在与 `desk-geyan.exe` **同目录**的
//! `quotes-builtin.txt` 里（定位规则见 `builtin_file`）—— **不编进二进制**，
//! 免得程序本体被几百条文案撑大。启动 / 重载时读盘，改完即生效。
//!
//! 另外支持**外部文件**，外部条目**排在内置库之前**优先显示：
//!
//! * `quotes.txt` —— 单文件用法，一行一条（`正文 —— 作者` 或直接写正文）。
//! * `quotes/*.txt` —— 多文件分组：每个 `.txt` 是一组，**按文件名字典序**排，
//!   组内按行序，组间**轮流**出现（见 `library_from` 的 round-robin）。
//!   想控制分组顺序就用 `01_乡村振兴.txt` / `02_基层治理.txt` 这种命名。
//!
//! 单文件用法完全向后兼容：只有一个组时，round-robin 退化成原行序。

use crate::util::data_dir;
use std::path::PathBuf;

#[derive(Clone)]
pub struct Quote {
    pub text: String,
    pub author: String,
}

impl Quote {
    fn new(text: &str, author: &str) -> Self {
        Self {
            text: text.to_string(),
            author: author.to_string(),
        }
    }

    /// 展示形态：正文 —— 作者
    pub fn display(&self) -> String {
        if self.author.is_empty() {
            self.text.clone()
        } else {
            format!("{}　——　{}", self.text, self.author)
        }
    }
}

/// 内置库文件名。发布时与 `desk-geyan.exe` **同目录**（发布包是平铺的）。
pub const BUILTIN_FILE: &str = "quotes-builtin.txt";

/// 上溯层数上限。4 层已覆盖 `target/debug/deps/` 这种最深的开发布局。
const WALK_UP_MAX: usize = 5;

/// 内置库找不到 / 读不出来时顶上来的那一条。
///
/// **为什么不返回空库**：渲染路径里有二十多处 `list[qi]`、
/// `(qi + 1) % list.len()`（frame / render / input 里都有），空库直接
/// panic；而「什么都不画、什么都不说」也是本项目明令禁止的退化形态。
/// 用一条说明性条目兜住，代价只有几十字节，换来「库永不为空」这条不变量 ——
/// 「缺件」本身由 `--selftest` / `--quotes` 专门报出来，不会靠这条兜底掩盖。
const BUILTIN_MISSING: &str =
    "未找到格言库文件 quotes-builtin.txt（应与 desk-geyan.exe 放在同一目录）";

/// 内置库文件的定位：从 **exe 所在目录**起，逐级上溯找
/// `quotes-builtin.txt` 或 `assets/quotes-builtin.txt`。
///
/// 为什么上溯而不是只看同目录：
///   * 发布包平铺 —— 文件与 exe 同级，第一层就命中；
///   * 开发时 exe 在 `target[-rel]/release/`，库文件在仓库根的 `assets/`，
///     上溯两层命中；
///   * `cargo test` 的测试二进制在 `target/debug/deps/`，上溯四层命中。
///
/// 上溯只认这两个文件名（不会误捡无关文件），层数封顶，免得一路走到盘根。
pub fn builtin_file() -> Option<PathBuf> {
    let mut dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    for _ in 0..=WALK_UP_MAX {
        for cand in [
            dir.join(BUILTIN_FILE),
            dir.join("assets").join(BUILTIN_FILE),
        ] {
            if cand.is_file() {
                return Some(cand);
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// 读出内置库（**每次调用重新读盘，不做缓存**）。
///
/// 缓存会让「热重载」名不副实：改完 `quotes-builtin.txt` 按 `Ctrl+Alt+R`
/// 却不生效。19 KB 读一次是微秒级，而且这些调用只出现在启动 / 重载 / 自检，
/// 不在每帧路径上。
pub fn load_builtin() -> Vec<Quote> {
    match builtin_file().and_then(|p| read_external(&p).ok()) {
        Some(list) if !list.is_empty() => list,
        // 缺件 / 解出空库：退化成一条说明（见 BUILTIN_MISSING），保证调用方
        // 拿到的库**永不为空** —— 二十多处 `list[qi]` 全靠这条不变量。
        _ => vec![Quote::new(BUILTIN_MISSING, "")],
    }
}

/// 剥掉 UTF-8 BOM。
///
/// 坑：用记事本「另存为 UTF-8」会给文件加上 `EF BB BF`。不剥的话那个
/// U+FEFF 会黏在第一行行首，于是本该被跳过的注释行变成**一条以 BOM 开头
/// 的格言**（`#` 前面的字符让它躲过了 `starts_with('#')`），黑底上会飘出
/// 一个不可见的怪符号。这是「用户拿记事本改一下文件」最常见的坏法。
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// 逐行解析外部文件，跳过空行与 `#` / `;` 注释。
///
/// `pub(crate)`：收藏夹（`favorites.rs`）要复用同一个解析器 ——
/// 「写出去的行能被读回来」必须是同一个函数保证的，各写一份必走偏。
pub(crate) fn read_external(path: &std::path::Path) -> std::io::Result<Vec<Quote>> {
    let text = std::fs::read_to_string(path)?;
    let mut list = Vec::new();
    for raw in strip_bom(&text).lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        list.push(parse_line(line));
    }
    Ok(list)
}

/// 一组外部格言：`(组名, 条目)`。组名取文件名（不含目录），
/// 诊断输出里要能看出「哪条来自哪个文件」。
pub(crate) type Group = (String, Vec<Quote>);

/// 收集所有外部分组：先 `quotes.txt`（老用法，保持「行序即优先级」的
/// 向后兼容），再 `quotes/` 目录下每个 `.txt`（按文件名字典序）。
///
/// 空文件、空目录都会被跳过 —— 「建了 quotes/ 但还没放文件」不该让程序
/// 退化成「只有内置库」之外的任何异常状态。
///
/// `pub(crate)`：收藏夹的测试要验「收藏夹确实成为独立一组、且组名对」，
/// 路由到这个函数而不是自己再数一遍文件。
pub(crate) fn load_groups(dir: &std::path::Path) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();

    // ① 单文件：`<dir>/quotes.txt`
    let single = dir.join("quotes.txt");
    if let Ok(list) = read_external(&single) {
        if !list.is_empty() {
            groups.push(("quotes.txt".to_string(), list));
        }
    }

    // ② 分组目录：`<dir>/quotes/*.txt`，按文件名排序
    let qdir = dir.join("quotes");
    let mut files: Vec<std::path::PathBuf> = match std::fs::read_dir(&qdir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .map(|x| x.eq_ignore_ascii_case("txt"))
                        .unwrap_or(false)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    // 字典序 = 「01_、02_、10_」也会按数字意图排（不会 10 排到 2 前面，
    // 因为前导零保证了字典序与数值序一致）。
    files.sort();
    for f in files {
        if let Ok(list) = read_external(&f) {
            if !list.is_empty() {
                let name = f
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| f.to_string_lossy().to_string());
                groups.push((name, list));
            }
        }
    }
    groups
}

/// 组间 round-robin 交织：组内保序，组间轮流取一条。
///
/// ```
/// A = [a1, a2]   B = [b1, b2, b3]
/// → a1, b1, a2, b2, b3
/// ```
///
/// 为什么不是「A 播完再播 B」：组大小往往差一个量级（申论 152 条 vs
/// 古诗 30 条），顺序播放会让小组几分钟才露一次面。round-robin 让每个
/// 文件都按自己的行序均匀出现 —— 组内相邻两条仍相距不远，
/// 而「放到最前的组最先出现」这个老语义也保住了（第一轮就是按组序）。
/// 只有一个组时退化为原行序，与旧实现逐字节一致。
fn interleave(groups: &[Group]) -> Vec<Quote> {
    let max_len = groups.iter().map(|(_, l)| l.len()).max().unwrap_or(0);
    let mut out = Vec::new();
    for i in 0..max_len {
        for (_, list) in groups {
            if i < list.len() {
                out.push(list[i].clone());
            }
        }
    }
    out
}

/// 内置库 + 所有外部分组（外部在前）。分组细节见模块文档。
pub fn library() -> Vec<Quote> {
    library_from(&data_dir())
}

/// 从**指定数据目录**加载。抽出来是为了让测试指向临时目录 ——
/// 直接测真实 `%APPDATA%\DeskGeyan` 的话，测试结果会随机器上
/// 用户自己的 quotes.txt 变化（这条项目已经为「测试读用户配置」
/// 栽过一次，见 `Config::for_test` 的注释）。
pub fn library_from(dir: &std::path::Path) -> Vec<Quote> {
    let mut list = interleave(&load_groups(dir));
    list.extend(load_builtin());
    list
}

/// 内置库条数。测试断言总条数时用，免得把「内置多少条」这个会过期的魔数写进测试。
///
/// 与 `load_builtin()` **同源**（含缺件时的那条兜底）—— 两处若各算各的，
/// 缺件场景下断言会成串错位，反而把真正的问题盖住；缺件本身由
/// 「内置库文件（与 exe 同目录）」那条断言专门报出来。
pub fn builtin_len() -> usize {
    load_builtin().len()
}

/// 一条格言在候选表里的「规范形态」，用来查重。
///
/// 只去空白、去标点，不碰文字 —— 查重的目标是「同一句话写了两遍」，
/// 而不是「看起来像」。
///
/// `pub(crate)`：收藏夹查重要用同一把尺子（见 `favorites::decide`）。
pub(crate) fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && !"，。；：、！？「」\"'".contains(*c))
        .collect()
}

/// `desk-geyan --quotes`：把格言库**真的加载一遍**再逐项验证。
///
/// 「文件写进去了」和「程序读出来了」是两件事，中间隔着一个解析器。
/// 这个模式打印实际加载条数，并对最容易静默出错的地方下断言：
/// BOM、注释被当正文、分隔符残留、出处缺失、重复条目。
pub fn audit() -> i32 {
    use std::collections::HashSet;

    let dir = data_dir();
    let mut rows: Vec<(&'static str, bool, String)> = Vec::new();

    let groups = load_groups(&dir);
    // 交织后的完整外部队列（= 实际播放顺序的前缀）
    let ext: Vec<Quote> = interleave(&groups);
    let list = library();

    // ---- 内置库：从「编译期常量」搬成「与 exe 同目录的文件」之后，它成了
    // **安装完整性**的一部分 —— 少拷一个文件，程序就只剩一条提示语。所以这里
    // 对文件名/条数/内容都下断言，缺件直接判 FAIL 并把查找规则说清楚。
    let builtin_path = builtin_file();
    let builtin_list: Vec<Quote> = builtin_path
        .as_deref()
        .and_then(|p| read_external(p).ok())
        .unwrap_or_default();
    let builtin_n = builtin_list.len();

    match &builtin_path {
        Some(p) => rows.push((
            "内置库文件",
            true,
            format!("{} · {} 条", p.display(), builtin_n),
        )),
        None => rows.push((
            "内置库文件",
            false,
            format!(
                "未找到 {}（在 exe 目录及其上溯各级里查找）—— 缺件时程序只会显示一条提示语",
                BUILTIN_FILE
            ),
        )),
    }

    // 内置库的内容自检：它是发出去的数据文件，被截断 / 手改坏 / 同句写两遍
    // 都不该悄悄放过去（外部库的同类检查见下面几行，尺子是同一把 `norm`）。
    let b_blank = builtin_list
        .iter()
        .filter(|q| q.text.trim().is_empty())
        .count();
    let mut seen_b: HashSet<String> = HashSet::new();
    let b_dup: Vec<&str> = builtin_list
        .iter()
        .filter(|q| !seen_b.insert(norm(&q.text)))
        .map(|q| q.text.as_str())
        .collect();
    let b_ok = b_blank == 0 && b_dup.is_empty() && builtin_n > 0;
    rows.push((
        "内置库数据可用",
        b_ok,
        if b_ok {
            format!("{} 条 · 无空条目 · 无重复", builtin_n)
        } else if builtin_n == 0 {
            "解析出 0 条（文件为空或读不出来）".to_string()
        } else {
            format!(
                "空条目 {} 条 · 重复 {} 条{}",
                b_blank,
                b_dup.len(),
                if b_dup.is_empty() {
                    String::new()
                } else {
                    format!("（例如「{}」）", b_dup[0])
                }
            )
        },
    ));

    // ---- 分组结构（新增：多文件用法要能一眼看出加载了几组、各几条）
    if groups.is_empty() {
        rows.push((
            "外部格言文件",
            true,
            format!(
                "未发现 quotes.txt / quotes/*.txt，仅用内置 {} 条",
                builtin_n
            ),
        ));
    } else {
        let desc = groups
            .iter()
            .map(|(n, l)| format!("{}:{}", n, l.len()))
            .collect::<Vec<_>>()
            .join("  ");
        rows.push((
            "分组加载",
            true,
            format!(
                "{} 组 · {} 条外部（交织后）+ {} 条内置 = {} 条 —— {}",
                groups.len(),
                ext.len(),
                builtin_n,
                list.len(),
                desc
            ),
        ));
    }

    // ---- BOM：记事本「另存为 UTF-8」的产物。已自动剥离，只报数不判失败
    // （漏剥的后果是「# 注释行变成一条格言」，那由「注释未被当成格言」兜住）。
    let bom_files: Vec<String> = {
        let mut v = Vec::new();
        for (name, _) in &groups {
            let p = if name == "quotes.txt" {
                dir.join("quotes.txt")
            } else {
                dir.join("quotes").join(name)
            };
            if let Ok(t) = std::fs::read_to_string(&p) {
                if t.starts_with('\u{feff}') {
                    v.push(name.clone());
                }
            }
        }
        v
    };
    rows.push((
        "无 BOM 污染",
        true,
        if bom_files.is_empty() {
            "无 BOM".to_string()
        } else {
            format!("{} 带 UTF-8 BOM（已自动剥离）", bom_files.join(" / "))
        },
    ));

    let blank: Vec<usize> = ext
        .iter()
        .enumerate()
        .filter(|(_, q)| q.text.trim().is_empty())
        .map(|(i, _)| i + 1)
        .collect();
    rows.push((
        "每条都有正文",
        blank.is_empty(),
        if blank.is_empty() {
            "无空条目".into()
        } else {
            format!("空条目行号 {:?}", blank)
        },
    ));

    let no_author: Vec<&str> = ext
        .iter()
        .filter(|q| q.author.trim().is_empty())
        .map(|q| q.text.as_str())
        .collect();
    rows.push((
        "每条都有出处",
        no_author.is_empty(),
        if no_author.is_empty() {
            format!("{} 条全部带出处", ext.len())
        } else {
            format!("{} 条缺出处，例如「{}」", no_author.len(), no_author[0])
        },
    ));

    // 分隔符只应出现在「正文 / 出处」之间，被切完之后正文与出处里都不该再有。
    let residue: Vec<&str> = ext
        .iter()
        .filter(|q| q.text.contains("——") || q.text.contains('\t') || q.author.contains("——"))
        .map(|q| q.text.as_str())
        .collect();
    rows.push((
        "分隔符已切净",
        residue.is_empty(),
        if residue.is_empty() {
            "正文 / 出处无残留 `——`".into()
        } else {
            format!("{} 条残留，例如「{}」", residue.len(), residue[0])
        },
    ));

    // 注释若漏进库，第一条必然是 `#` 开头（说明注释判定失效）。
    let leaked: Vec<&str> = ext
        .iter()
        .filter(|q| q.text.starts_with('#') || q.text.starts_with(';'))
        .map(|q| q.text.as_str())
        .collect();
    rows.push((
        "注释未被当成格言",
        leaked.is_empty(),
        if leaked.is_empty() {
            "分区注释行已全部跳过".into()
        } else {
            format!("{} 条注释漏入，例如「{}」", leaked.len(), leaked[0])
        },
    ));

    // 跨组查重：同一个文件里写两遍、或者两个文件里放同一句，都算。
    let mut seen: HashSet<String> = HashSet::new();
    let dup_ext: Vec<&str> = ext
        .iter()
        .filter(|q| !seen.insert(norm(&q.text)))
        .map(|q| q.text.as_str())
        .collect();
    rows.push((
        "外部无重复条目",
        dup_ext.is_empty(),
        if dup_ext.is_empty() {
            format!("{} 条互不重复", ext.len())
        } else {
            format!("{} 条重复，例如「{}」", dup_ext.len(), dup_ext[0])
        },
    ));

    let builtin_set: HashSet<String> = builtin_list.iter().map(|q| norm(&q.text)).collect();
    let clash: Vec<&str> = ext
        .iter()
        .filter(|q| builtin_set.contains(&norm(&q.text)))
        .map(|q| q.text.as_str())
        .collect();
    rows.push((
        "不与内置库撞车",
        clash.is_empty(),
        if clash.is_empty() {
            "无重复（内置条目会被外部同句盖住，浪费一个位置）".into()
        } else {
            format!("{} 条与内置重复：{}", clash.len(), clash.join(" / "))
        },
    ));

    let mut pass = true;
    for (name, ok, detail) in &rows {
        if !ok {
            pass = false;
        }
        println!("[{}] {:<22} {}", if *ok { "PASS" } else { "FAIL" }, name, detail);
    }

    println!("\n加载后的实际显示形态（抽样，跑马灯里滚的就是这条）：");
    for q in list.iter().take(4) {
        println!("  · {}", q.display());
    }
    println!("  ……（第 {} 条）· {}", list.len(), list[list.len() - 1].display());

    println!(
        "\n{}\n提示：改完外部 quotes.txt / quotes/*.txt 保存，托盘菜单「重新载入格言 / 配置」即生效。\n\
         内置库（{}）必须与 desk-geyan.exe 放在同一目录，升级覆盖时留意别丢。",
        if pass {
            "格言库解析全部符合预期"
        } else {
            "存在不符合预期项"
        },
        match &builtin_path {
            Some(p) => p.display().to_string(),
            None => format!("{} 未找到", BUILTIN_FILE),
        }
    );
    if pass {
        0
    } else {
        3
    }
}

fn parse_line(line: &str) -> Quote {
    for sep in ["——", "--", " — ", "\t"] {
        if let Some((t, a)) = line.split_once(sep) {
            let t = t.trim().trim_matches(|c| c == '"' || c == '「' || c == '」');
            let a = a.trim();
            if !t.is_empty() {
                return Quote::new(t, a);
            }
        }
    }
    Quote::new(line.trim(), "")
}
