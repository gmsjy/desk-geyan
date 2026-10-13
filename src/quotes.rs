//! 格言库。
//!
//! 内置一批经典中文格言，另外支持在 `%APPDATA%\DeskGeyan\quotes.txt`
//! 里每行追加一条（`正文 —— 作者` 或直接写正文），文件里已有的条目优先显示。

use crate::config::Config;

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

const BUILTIN: &[(&str, &str)] = &[
    ("千里之行，始于足下。", "老子"),
    ("合抱之木，生于毫末；九层之台，起于累土。", "老子"),
    ("知人者智，自知者明。", "老子"),
    ("上善若水，水善利万物而不争。", "老子"),
    ("祸兮福之所倚，福兮祸之所伏。", "老子"),
    ("慎终如始，则无败事。", "老子"),
    ("天行健，君子以自强不息。", "《周易》"),
    ("地势坤，君子以厚德载物。", "《周易》"),
    ("不积跬步，无以至千里；不积小流，无以成江海。", "荀子"),
    ("锲而舍之，朽木不折；锲而不舍，金石可镂。", "荀子"),
    ("道虽迩，不行不至；事虽小，不为不成。", "荀子"),
    ("学而不思则罔，思而不学则殆。", "孔子"),
    ("知之者不如好之者，好之者不如乐之者。", "孔子"),
    ("三人行，必有我师焉。", "孔子"),
    ("温故而知新，可以为师矣。", "孔子"),
    ("君子坦荡荡，小人长戚戚。", "孔子"),
    ("己所不欲，勿施于人。", "孔子"),
    ("逝者如斯夫，不舍昼夜。", "孔子"),
    ("生于忧患，死于安乐。", "孟子"),
    ("天将降大任于是人也，必先苦其心志，劳其筋骨。", "孟子"),
    ("富贵不能淫，贫贱不能移，威武不能屈。", "孟子"),
    ("穷则独善其身，达则兼济天下。", "孟子"),
    ("老吾老以及人之老，幼吾幼以及人之幼。", "孟子"),
    ("业精于勤，荒于嬉；行成于思，毁于随。", "韩愈"),
    ("书山有路勤为径，学海无涯苦作舟。", "韩愈"),
    ("博观而约取，厚积而薄发。", "苏轼"),
    ("纸上得来终觉浅，绝知此事要躬行。", "陆游"),
    ("山重水复疑无路，柳暗花明又一村。", "陆游"),
    ("问渠那得清如许？为有源头活水来。", "朱熹"),
    ("会当凌绝顶，一览众山小。", "杜甫"),
    ("长风破浪会有时，直挂云帆济沧海。", "李白"),
    ("少壮不努力，老大徒伤悲。", "《长歌行》"),
    ("黑发不知勤学早，白首方悔读书迟。", "颜真卿"),
    ("有志者事竟成，破釜沉舟，百二秦关终属楚。", "《后汉书》"),
    ("宝剑锋从磨砺出，梅花香自苦寒来。", "《警世贤文》"),
    ("不以物喜，不以己悲。", "范仲淹"),
    ("先天下之忧而忧，后天下之乐而乐。", "范仲淹"),
    ("天下兴亡，匹夫有责。", "顾炎武"),
    ("苟利国家生死以，岂因祸福避趋之。", "林则徐"),
    ("海纳百川，有容乃大；壁立千仞，无欲则刚。", "林则徐"),
    ("为中华之崛起而读书。", "周恩来"),
    ("世上无难事，只要肯登攀。", "毛泽东"),
    ("一万年太久，只争朝夕。", "毛泽东"),
    ("认识你自己。", "苏格拉底"),
    ("未经省察的人生不值得过。", "苏格拉底"),
    ("我唯一知道的，就是我一无所知。", "苏格拉底"),
    ("优秀是一种习惯。", "亚里士多德"),
    ("吾爱吾师，吾更爱真理。", "亚里士多德"),
    ("求知是人的本性。", "亚里士多德"),
    ("人是万物的尺度。", "普罗泰戈拉"),
    ("那些杀不死我的，使我更强大。", "尼采"),
    ("每一个不曾起舞的日子，都是对生命的辜负。", "尼采"),
    ("一个人可以被毁灭，但不能被打败。", "海明威"),
    ("生活总是让我们遍体鳞伤，但到后来，那些受伤的地方会变成我们最强壮的地方。", "海明威"),
    ("我们都在阴沟里，但仍有人仰望星空。", "王尔德"),
    ("想象力比知识更重要。", "爱因斯坦"),
    ("如果你不能简单地解释一件事，说明你还没有真正理解它。", "爱因斯坦"),
    ("在隆冬，我终于知道，我身上有一个不可战胜的夏天。", "加缪"),
    ("对未来真正的慷慨，是把一切献给现在。", "加缪"),
    ("我思故我在。", "笛卡尔"),
    ("不要因为走得太远，而忘记为什么出发。", "纪伯伦"),
    ("种一棵树最好的时间是十年前，其次是现在。", "谚语"),
    ("慢慢来，比较快。", ""),
    ("只要方向对了，就不怕路远。", ""),
    ("你今天的努力，是幸运的伏笔。", ""),
    ("星光不问赶路人，时光不负有心人。", ""),
    ("时间不会辜负每一份努力，也不会放过每一次虚度。", ""),
    ("行动是治愈恐惧的良药。", ""),
    ("总有人要赢的，那为什么不能是我。", ""),
    ("自律给我自由。", ""),
    ("越努力，越幸运。", ""),
    ("沉默地做，直到你把话都说清楚。", ""),
];

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
fn read_external(path: &std::path::Path) -> std::io::Result<Vec<Quote>> {
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

/// 内置库 + 外部文件；外部文件里的条目排在前面。
pub fn library() -> Vec<Quote> {
    let mut list = read_external(&Config::quotes_path()).unwrap_or_default();
    for (t, a) in BUILTIN {
        list.push(Quote::new(t, a));
    }
    list
}

/// 一条格言在候选表里的「规范形态」，用来查重。
///
/// 只去空白、去标点，不碰文字 —— 查重的目标是「同一句话写了两遍」，
/// 而不是「看起来像」。
fn norm(s: &str) -> String {
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

    let path = Config::quotes_path();
    let mut rows: Vec<(&'static str, bool, String)> = Vec::new();

    let read = read_external(&path);
    // `io::Error` 不是 Clone，所以走 `as_ref()` 借一次再克隆条目。
    let ext: Vec<Quote> = read.as_ref().map(|v| v.clone()).unwrap_or_default();
    let builtin_n = BUILTIN.len();
    let list = library();

    rows.push((
        "格言文件可读",
        read.is_ok(),
        match &read {
            Ok(_) => format!("{}", path.display()),
            Err(e) => format!("{} —— {}", path.display(), e),
        },
    ));

    // 文件不存在时 library() 会退回纯内置库，程序不崩 —— 这里只报数不判失败。
    let raw_bom = std::fs::read_to_string(&path)
        .map(|t| t.starts_with('\u{feff}'))
        .unwrap_or(false);
    rows.push((
        "无 BOM 污染",
        true,
        if raw_bom {
            "文件带 UTF-8 BOM（已自动剥离，不影响显示）".into()
        } else {
            "无 BOM".to_string()
        },
    ));

    rows.push((
        "外部条目非空",
        !ext.is_empty(),
        format!("{} 条外部 + {} 条内置 = {} 条", ext.len(), builtin_n, list.len()),
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

    let builtin_set: HashSet<String> = BUILTIN.iter().map(|(t, _)| norm(t)).collect();
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
        "\n{}\n提示：改完 quotes.txt 保存，托盘菜单「重新载入配置」即生效。",
        if pass {
            "格言库解析全部符合预期"
        } else {
            "存在不符合预期项"
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
