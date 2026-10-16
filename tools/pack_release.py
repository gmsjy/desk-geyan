#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""打包 desk-geyan 的 Windows x64 发布包。

产物：`dist/desk-geyan-v<版本>-windows-x64.zip`，**平铺 4 个文件**（不加目录）：
    desk-geyan.exe · README.md · LICENSE · quotes-shenlun.txt

为什么要有这个脚本：
  * 发布包结构是「平铺 4 文件」，手工压容易漏件或压出目录层级；
  * 包里的 README **必须**等于当前 `README.md`（README 一改就得重打包）；
  * 更要紧的是**别拿陈旧 exe 打包** —— 2026-10-16 踩过：`target/release/` 里躺着
    一个早于当日改动的 exe，用它量 A/B 得到 580px 的假回归。这里加了一道
    「exe 必须比全部源文件新」的守卫，不满足直接拒绝打包。
  * 打完包从**新用户视角**自检：解压到空目录 + 空 `DG_DATA_DIR` 跑 `--selftest`
    与 `--quotes`，全绿才算成包。

用法：
    python tools/pack_release.py
    python tools/pack_release.py --exe target/release/desk-geyan.exe
    python tools/pack_release.py --no-verify     # 只打包不自检
"""
import argparse
import hashlib
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import zipfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
PAYLOAD = [  # (zip 内的名字, 仓库内相对路径)
    ("desk-geyan.exe", "target-rel/release/desk-geyan.exe"),
    ("README.md", "README.md"),
    ("LICENSE", "LICENSE"),
    ("quotes-shenlun.txt", "assets/quotes-shenlun.txt"),
]
SRC_SUFFIXES = (".rs", ".toml")


def version() -> str:
    m = re.search(r'^version\s*=\s*"([^"]+)"', (ROOT / "Cargo.toml").read_text(encoding="utf-8"), re.M)
    if not m:
        sys.exit("FAIL  Cargo.toml 里找不到 version")
    return m.group(1)


def newest_source() -> tuple[float, str]:
    best, who = 0.0, ""
    for p in list((ROOT / "src").rglob("*.rs")) + [ROOT / "Cargo.toml"]:
        t = p.stat().st_mtime
        if t > best:
            best, who = t, str(p.relative_to(ROOT))
    return best, who


def sha256(p: pathlib.Path) -> str:
    h = hashlib.sha256()
    with p.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--exe", default="target-rel/release/desk-geyan.exe",
                    help="要打包的 release exe（默认 target-rel，干净口径）")
    ap.add_argument("--out", default="dist")
    ap.add_argument("--no-verify", action="store_true")
    a = ap.parse_args()

    ver = version()
    exe = ROOT / a.exe
    if not exe.exists():
        sys.exit(f"FAIL  找不到 exe：{exe}\n      先跑 CARGO_TARGET_DIR=target-rel cargo build --release")

    # ---- 守卫：exe 必须比所有源文件新，否则打出去的是旧行为
    exe_t = exe.stat().st_mtime
    src_t, who = newest_source()
    if exe_t < src_t:
        import datetime
        fmt = lambda t: datetime.datetime.fromtimestamp(t).strftime("%H:%M:%S")
        sys.exit(
            f"FAIL  exe 比源文件旧，拒绝打包（会打出旧行为）\n"
            f"      exe   {fmt(exe_t)}  {a.exe}\n"
            f"      源文件 {fmt(src_t)}  {who}\n"
            f"      请先重建：CARGO_TARGET_DIR=target-rel cargo build --release"
        )

    files = [(z, ROOT / r) for z, r in PAYLOAD]
    for z, p in files:
        if not p.exists():
            sys.exit(f"FAIL  缺件：{p}")

    outdir = ROOT / a.out
    outdir.mkdir(exist_ok=True)
    zpath = outdir / f"desk-geyan-v{ver}-windows-x64.zip"
    with zipfile.ZipFile(zpath, "w", zipfile.ZIP_DEFLATED) as z:
        for name, p in files:
            z.write(p, arcname=name)  # arcname 无目录 = 平铺

    print(f"版本 {ver}   exe 构建于 " + __import__("datetime").datetime.fromtimestamp(exe_t).strftime("%Y-%m-%d %H:%M:%S"))
    print(f"打包 {zpath.relative_to(ROOT)}")
    for name, p in files:
        print(f"   {p.stat().st_size:>9}  {name}")
    print(f"   {'':>9}  ---- 共 {len(files)} 个文件（平铺）")
    print(f"压缩后 {zpath.stat().st_size} B")
    print(f"sha256 {sha256(zpath)}")

    if a.no_verify:
        return 0

    # ---- 新用户视角自检：空目录解压 + 空数据目录
    print("\n新用户视角自检（空目录解压 + 空 DG_DATA_DIR）：")
    tmp = pathlib.Path(tempfile.mkdtemp(prefix="dg-pack-"))
    try:
        app, data = tmp / "app", tmp / "data"
        app.mkdir(); data.mkdir()
        with zipfile.ZipFile(zpath) as z:
            names = sorted(z.namelist())
            z.extractall(app)
        print(f"   解压得到 {names}")
        exe2 = app / "desk-geyan.exe"
        env = dict(os.environ, DG_DATA_DIR=str(data))
        ok = True
        for args, want, label in (
            (["--selftest"], "全部通过", "selftest"),
            (["--quotes"], "格言库解析全部符合预期", "quotes"),
        ):
            r = subprocess.run([str(exe2), *args], capture_output=True, text=True, env=env, timeout=300)
            out = (r.stdout or "") + (r.stderr or "")
            npass = out.count("[PASS]"); nfail = out.count("[FAIL]")
            good = (r.returncode == 0) and nfail == 0 and want in out
            ok &= good
            print(f"   [{'PASS' if good else 'FAIL'}] {label}: exit={r.returncode} PASS={npass} FAIL={nfail}")
        return 0 if ok else 1
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
