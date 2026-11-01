//! 用户数据备份。
//!
//! 只复制**可重建程序状态所需**的文件：`config.ini`、单文件词库 `quotes.txt`
//! 和 `quotes/` 下的分组（含收藏夹）。目标是不碰系统注册表、不打包 exe，
//! 让用户能把一整个小目录直接带走或回滚。
//!
//! 备份目录使用本地时间戳；同名目录已存在时追加 `-2`、`-3`，绝不覆盖旧备份。

use std::io;
use std::path::{Path, PathBuf};

use crate::util::local_time;

/// 把配置与词库复制到 `<dir>/backups/<YYYYMMDD-HHMMSS>/`。
pub fn backup(dir: &Path) -> io::Result<PathBuf> {
    let now = local_time();
    let base = format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
    );
    let root = dir.join("backups");
    std::fs::create_dir_all(&root)?;
    let mut dest = root.join(&base);
    let mut n = 2u32;
    while dest.exists() {
        dest = root.join(format!("{base}-{n}"));
        n += 1;
    }
    std::fs::create_dir_all(&dest)?;

    let mut copied = 0usize;
    for name in ["config.ini", "quotes.txt"] {
        let src = dir.join(name);
        if src.is_file() {
            std::fs::copy(&src, dest.join(name))?;
            copied += 1;
        }
    }

    let qdir = dir.join("quotes");
    if qdir.is_dir() {
        let out = dest.join("quotes");
        std::fs::create_dir_all(&out)?;
        for ent in std::fs::read_dir(&qdir)? {
            let ent = ent?;
            let path = ent.path();
            if path.is_file() {
                if let Some(name) = path.file_name() {
                    std::fs::copy(&path, out.join(name))?;
                    copied += 1;
                }
            }
        }
    }

    let manifest = format!(
        "desk-geyan 数据备份\n时间: {:04}-{:02}-{:02} {:02}:{:02}:{:02}\n来源: {}\n文件数: {}\n",
        now.wYear,
        now.wMonth,
        now.wDay,
        now.wHour,
        now.wMinute,
        now.wSecond,
        dir.display(),
        copied
    );
    std::fs::write(dest.join("BACKUP.txt"), manifest)?;
    Ok(dest)
}
