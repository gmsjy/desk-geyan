//! 灵动岛的几何与交互判定。**纯函数、零 Win32**。
//!
//! 唯一入口是 `geom(t, env)`：`t` 是「展开度」，0 = 完全收起，1 = 完全展开。
//! 状态机（`State`）只负责把「线性时间进度」翻译成 `t`，几何本身不做任何分支
//! 判断 —— 这样「任意时刻的窗口矩形」是一个可对全区间下断言的函数，
//! 而不是一条只能在真机上看着像对的路径。

use super::ease;

/// 收起态高度（逻辑像素）
pub const COMPACT_H: i32 = 34;
/// 收起态左右内边距
pub const COMPACT_PAD_X: i32 = 14;
/// 收起态最小宽度（只有两个字也保持胶囊形状）
pub const COMPACT_MIN_W: i32 = 96;
/// 收起态最大宽度（再宽就不像「岛」了，交给轮播）
pub const COMPACT_MAX_W: i32 = 260;
/// 收起态片段之间的间距
pub const COMPACT_GAP: i32 = 8;
/// 展开态圆角
pub const EXPAND_RADIUS: i32 = 18;
/// 距工作区顶边的边距
pub const ANCHOR_MARGIN_Y: i32 = 8;
/// 展开态默认尺寸（逻辑像素）
pub const EXPANDED_W: i32 = 420;
pub const EXPANDED_H: i32 = 136;

/// 悬停多久才展开
pub const HOVER_IN_MS: u32 = 180;
/// 移开多久才收起（明显长于进入延时，防手抖）
pub const LEAVE_GRACE_MS: u32 = 620;
/// 展开时长
pub const EXPAND_MS: u32 = 380;
/// 收起时长（比展开快 21%，退场要利落）
pub const COLLAPSE_MS: u32 = 300;
/// 展开后至少停留这么久才允许收起
pub const MIN_VISIBLE_MS: u32 = 240;
/// 静止态定时器周期（悬停判定 180ms ≈ 4.5 拍）
pub const IDLE_TICK_MS: u32 = 40;
/// 动画期定时器周期（60fps）
pub const MORPH_TICK_MS: u32 = 16;
/// 切页时单侧淡出 / 淡入的时长（合计 240ms）
pub const PAGE_FADE_MS: u32 = 120;

// ---------------------------------------------------------------- 轮播

/// 切页相位。**顺序淡出淡入**，不是交叉淡入。
///
/// 规格书 §3.3 写的是「180ms 交叉淡入」。落地时改成顺序相位，理由是硬的：
/// 掩码通道是**一条字节存一份覆盖率**，两页内容同时存在就得把两份覆盖率
/// 叠进同一条通道，而它们的颜色可能不同（主色/次色/强调色各一条通道）。
/// 叠不了，就只能让两页轮流占用 —— 那就是顺序淡出淡入。
///
/// 这也与本项目既有的口味一致：格言的过渡相位（Dwell → Leave → Gap → Arrive）
/// 就是用它消灭交叉淡入的重影（见 `transition.rs` 的注释）。
/// 用户能看到的是「文字暗下去、换一页、亮起来」，240ms 内完成，没有重影。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RotPhase {
    /// 停在当前页
    #[default]
    Hold,
    /// 淡出（当前页 → 暗）
    Out,
    /// 淡入（新页 → 亮）
    In,
}

/// 轮播状态。`t_ms` 是**当前相位内**已经过的毫秒数。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rot {
    pub phase: RotPhase,
    pub t_ms: u32,
    pub page: usize,
}

impl Rot {
    pub fn fading(self) -> bool {
        self.phase != RotPhase::Hold
    }
}

/// 推进一拍，返回 `(新状态, 内容系数 k)`。
///
/// `k` 直接乘在掩码趟的 `tk` 上（只影响文字，不影响胶囊底），所以切页时
/// 用户看到的是「字暗下去、换一页、字亮起来」，胶囊本身不动、不闪。
///
/// **`rotate_s == 0` 或只有一页时恒为 `Hold` 且 `k == 1`** —— 这是「不轮播就
/// 零成本」的判据：`k` 恒等于 1、相位恒为 Hold，于是 `needs_frame` 不会因此
/// 变真，一帧都不出。
pub fn rot_step(r: Rot, dt_ms: u32, rotate_s: u32, pages: usize) -> (Rot, f32) {
    if rotate_s == 0 || pages <= 1 {
        return (
            Rot {
                phase: RotPhase::Hold,
                t_ms: 0,
                page: 0,
            },
            1.0,
        );
    }
    let fade = PAGE_FADE_MS.max(1);
    match r.phase {
        RotPhase::Hold => {
            let t = r.t_ms.saturating_add(dt_ms);
            if t >= rotate_s.saturating_mul(1000) {
                (
                    Rot {
                        phase: RotPhase::Out,
                        t_ms: 0,
                        page: r.page,
                    },
                    1.0,
                )
            } else {
                (Rot { t_ms: t, ..r }, 1.0)
            }
        }
        RotPhase::Out => {
            let t = r.t_ms.saturating_add(dt_ms);
            if t >= fade {
                // 换页并进入淡入。多出来的毫秒**带过去**，否则 16ms 的拍子会
                // 让淡入比淡出长一截（视觉上「暗下去很久才亮起来」）。
                let t2 = t - fade;
                let page = (r.page + 1) % pages;
                let k = (t2 as f32 / fade as f32).clamp(0.0, 1.0);
                (
                    Rot {
                        phase: RotPhase::In,
                        t_ms: t2,
                        page,
                    },
                    k,
                )
            } else {
                let k = 1.0 - (t as f32 / fade as f32);
                (Rot { t_ms: t, ..r }, k.max(0.0))
            }
        }
        RotPhase::In => {
            let t = r.t_ms.saturating_add(dt_ms);
            if t >= fade {
                (
                    Rot {
                        phase: RotPhase::Hold,
                        t_ms: 0,
                        page: r.page,
                    },
                    1.0,
                )
            } else {
                let k = t as f32 / fade as f32;
                (Rot { t_ms: t, ..r }, k)
            }
        }
    }
}

/// 内容相位的两个分界（顺序相位：淡出 → 空档 → 淡入）
const K_OUT_END: f32 = 0.30;
const K_IN_START: f32 = 0.42;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum State {
    #[default]
    Collapsed,
    Expanding,
    Expanded,
    Collapsing,
}

impl State {
    pub fn morphing(self) -> bool {
        matches!(self, State::Expanding | State::Collapsing)
    }

    pub fn is_expanded(self) -> bool {
        matches!(self, State::Expanded)
    }
}

/// 算几何需要的环境量。全部是**物理像素**（调用方已乘过 DPI）。
#[derive(Clone, Copy, Debug)]
pub struct IslandEnv {
    /// 工作区水平中线（`x + w/2` 恒等于它）
    pub cx: i32,
    /// 窗口顶边（工作区顶 + 边距）
    pub top: i32,
    /// 收起态宽度（由内容度量得出）
    pub compact_w: i32,
    /// 展开态宽高
    pub expanded_w: i32,
    pub expanded_h: i32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct IslandRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub r: i32,
}

fn lerp_i(a: i32, b: i32, t: f32) -> i32 {
    (a as f32 + (b - a) as f32 * t).round() as i32
}

/// 展开度为 `t` 时的窗口矩形。**纯函数**。
///
/// * `x` 由「水平中线不动」推出来：`x = cx - w/2`。视觉上是胶囊向两侧鼓出来，
///   而不是从左上角长开 —— 这是「岛」与「小窗口」的分界。
/// * `y` 恒为 `env.top`：从顶部向下垂。
/// * 圆角从 `h/2`（真胶囊）线性过渡到 18，并夹在 `h/2` 以内 ——
///   中间态高 60px 时若用 18 会显得「方」，用 h/2 才一直是胶囊感。
pub fn geom(t: f32, env: &IslandEnv) -> IslandRect {
    let t = t.clamp(0.0, 1.0);
    let compact_h = COMPACT_H.max(8);
    let w = lerp_i(env.compact_w.max(COMPACT_MIN_W), env.expanded_w.max(COMPACT_MIN_W), t)
        .max(1);
    let h = lerp_i(compact_h, env.expanded_h.max(compact_h), t).max(1);
    let r0 = compact_h / 2;
    let r = lerp_i(r0, EXPAND_RADIUS.max(2), t).min(h / 2).max(1);
    IslandRect {
        x: env.cx - w / 2,
        y: env.top,
        w,
        h,
        r,
    }
}

/// 内容相位：返回 `(收起内容系数, 展开内容系数)`，都落在 `[0,1]`。
///
/// **顺序相位**而不是交叉淡入：`0 → 0.30` 收起内容淡出，`0.30 → 0.42` 空档，
/// `0.42 → 1` 展开内容淡入。两者相乘恒为 0，所以永远不会同时可见 ——
/// 于是只需要一个全局文字系数 `tk`，不需要第四条颜色通道，也不会出现重影
/// （这是 PR1 换句过渡踩出来的经验）。
///
/// 展开与收起共用同一张表（收起时 `t` 从 1 递减），因此两条路径的观感对称。
pub fn phase_k(t: f32) -> (f32, f32) {
    let t = t.clamp(0.0, 1.0);
    if t <= K_OUT_END {
        (1.0 - ease::smoothstep(0.0, K_OUT_END, t), 0.0)
    } else if t < K_IN_START {
        (0.0, 0.0)
    } else {
        (0.0, ease::smoothstep(K_IN_START, 1.0, t))
    }
}

/// 悬停累计器。**只存累计时长，不存「是否展开」** —— 意图由调用方的
/// `want` 单独持有，两者分开才能对「Expanding 期间用户又移开了」这种
/// 中间态下断言。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HoverState {
    pub inside_ms: u32,
    pub outside_ms: u32,
}

/// 不对称迟滞：进入要连续 `hover_in` 才认，离开要连续 `leave` 才认。
///
/// `want` 是上一次的意图，函数返回新的意图与新的累计器。
/// 这条纯函数替掉了「真实鼠标事件的时序」，所以全部边界条件都能在单测里
/// 用 1ms 步进覆盖，而不必造一台会抖手的机器。
pub fn hover_decide(
    want: bool,
    h: HoverState,
    inside: bool,
    dt_ms: u32,
    hover_in: u32,
    leave: u32,
) -> (bool, HoverState) {
    let mut h = h;
    if inside {
        h.inside_ms = h.inside_ms.saturating_add(dt_ms);
        h.outside_ms = 0;
    } else {
        h.outside_ms = h.outside_ms.saturating_add(dt_ms);
        h.inside_ms = 0;
    }
    let want = if inside {
        if h.inside_ms >= hover_in.max(1) {
            true
        } else {
            want
        }
    } else if h.outside_ms >= leave.max(1) {
        false
    } else {
        want
    };
    (want, h)
}

/// 把片段宽度切成若干页：每页尽量装满、**不拆片段**、顺序不变。
///
/// 单个片段就比可用宽度还宽时，它单独占一页（由绘制端截断加省略号）——
/// 绝不静默丢弃，那会变成「勾了 CPU 却看不到」。
pub fn pack_pages(widths: &[i32], gap: i32, avail: i32) -> Vec<Vec<usize>> {
    let mut pages: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut used = 0;
    for (i, &w) in widths.iter().enumerate() {
        let need = if cur.is_empty() { w } else { used + gap + w };
        if !cur.is_empty() && need > avail {
            pages.push(std::mem::take(&mut cur));
            used = w;
            cur.push(i);
        } else {
            used = need;
            cur.push(i);
        }
    }
    if !cur.is_empty() {
        pages.push(cur);
    }
    if pages.is_empty() {
        pages.push(Vec::new());
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> IslandEnv {
        IslandEnv {
            cx: 960,
            top: 8,
            compact_w: 140,
            expanded_w: EXPANDED_W,
            expanded_h: EXPANDED_H,
        }
    }

    #[test]
    fn endpoints_are_exact_constants() {
        let e = env();
        let c = geom(0.0, &e);
        assert_eq!((c.w, c.h), (140, COMPACT_H));
        assert_eq!(c.y, 8);
        assert_eq!(c.r, COMPACT_H / 2);
        let x = geom(1.0, &e);
        assert_eq!((x.w, x.h), (EXPANDED_W, EXPANDED_H));
        assert_eq!(x.r, EXPAND_RADIUS);
        // 越界的 t 被夹到端点
        assert_eq!(geom(-5.0, &e), c);
        assert_eq!(geom(5.0, &e), x);
    }

    #[test]
    fn width_height_monotonic_and_center_fixed() {
        let e = env();
        let mut prev = geom(0.0, &e);
        for i in 1..=100 {
            let t = i as f32 / 100.0;
            let g = geom(t, &e);
            assert!(g.w >= prev.w, "w 在 t={t} 处回退");
            assert!(g.h >= prev.h, "h 在 t={t} 处回退");
            // 中心恒不动（允许奇数宽带来的 1px 取整偏差）
            assert!(
                (g.x + g.w / 2 - e.cx).abs() <= 1,
                "t={t}: 中心 {}+{}/2 != {}",
                g.x,
                g.w,
                e.cx
            );
            // 圆角永远是合法值
            assert!(g.r >= 1 && g.r <= g.h / 2, "t={t}: r={} h={}", g.r, g.h);
            prev = g;
        }
    }

    #[test]
    fn content_phases_never_coexist() {
        for i in 0..=100 {
            let t = i as f32 / 100.0;
            let (ck, ek) = phase_k(t);
            assert!((ck * ek).abs() < 1e-9, "t={t}: 两套内容同时可见 {ck}/{ek}");
            assert!((0.0..=1.0).contains(&ck) && (0.0..=1.0).contains(&ek));
        }
        assert_eq!(phase_k(0.0), (1.0, 0.0));
        assert_eq!(phase_k(1.0), (0.0, 1.0));
        // 空档区确实两边都是 0
        let (ck, ek) = phase_k(0.36);
        assert_eq!((ck, ek), (0.0, 0.0));
    }

    #[test]
    fn hover_thresholds_are_exact() {
        let mut h = HoverState::default();
        // 进入：179ms 不展开，180ms 展开
        for _ in 0..179 {
            let (w, nh) = hover_decide(false, h, true, 1, HOVER_IN_MS, LEAVE_GRACE_MS);
            assert!(!w, "179ms 内就展开了");
            h = nh;
        }
        let (w, _) = hover_decide(false, h, true, 1, HOVER_IN_MS, LEAVE_GRACE_MS);
        assert!(w, "180ms 仍未展开");
    }

    #[test]
    fn hover_leave_grace_and_jitter() {
        let mut h = HoverState::default();
        let (want, nh) = hover_decide(false, h, true, 1000, HOVER_IN_MS, LEAVE_GRACE_MS);
        assert!(want);
        h = nh;
        // 移开 619ms 仍保持展开
        let (w, nh) = hover_decide(true, h, false, LEAVE_GRACE_MS - 1, HOVER_IN_MS, LEAVE_GRACE_MS);
        assert!(w, "宽限期内就收起了");
        h = nh;
        // 再 1ms 就收起
        let (w, _) = hover_decide(true, h, false, 1, HOVER_IN_MS, LEAVE_GRACE_MS);
        assert!(!w, "宽限期过了还没收起");
    }

    #[test]
    fn hover_jitter_resets_accumulator() {
        let mut h = HoverState::default();
        // 连续两次 100ms 的进入、中间夹一帧「在外面」⇒ 累计器被清零
        let (_, nh) = hover_decide(false, h, true, 100, HOVER_IN_MS, LEAVE_GRACE_MS);
        h = nh;
        let (_, nh) = hover_decide(false, h, false, 16, HOVER_IN_MS, LEAVE_GRACE_MS);
        h = nh;
        assert_eq!(h.inside_ms, 0, "离开一帧没有清零进入累计器");
        let (w, _) = hover_decide(false, h, true, 100, HOVER_IN_MS, LEAVE_GRACE_MS);
        assert!(!w, "抖动后 100ms 就展开了");
    }

    #[test]
    fn pack_pages_never_splits_and_keeps_order() {
        // 三项各 50 宽、间距 10、可用 170 ⇒ [0,1] 恰好 110、加第三项 170 也恰好装下
        assert_eq!(pack_pages(&[50, 50, 50], 10, 170), vec![vec![0, 1, 2]]);
        // 可用 120 ⇒ [0,1]=110 装得下，第三项 120+10+50 超了 ⇒ 两页
        assert_eq!(pack_pages(&[50, 50, 50], 10, 120), vec![vec![0, 1], vec![2]]);
        // 单片段超宽：自己一页，不丢
        assert_eq!(pack_pages(&[400, 20], 10, 100), vec![vec![0], vec![1]]);
        // 空输入也得有一页（调用方不必判空）
        assert_eq!(pack_pages(&[], 10, 100), vec![Vec::<usize>::new()]);
        // 顺序在所有页里保持递增
        let pages = pack_pages(&[30, 40, 50, 60, 70], 8, 90);
        let flat: Vec<usize> = pages.iter().flatten().copied().collect();
        assert_eq!(flat, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn rot_is_inert_when_off_or_single_page() {
        // rotate=0：无论推多久都是 Hold、k 恒 1（「不轮播零成本」的判据）
        let mut r = Rot::default();
        for _ in 0..1000 {
            let (nr, k) = rot_step(r, 1000, 0, 5);
            r = nr;
            assert_eq!(k, 1.0, "rotate=0 时 k 不是 1");
            assert_eq!(r.phase, RotPhase::Hold);
            assert_eq!(r.page, 0);
        }
        // 只有一页：同上
        let (r2, k2) = rot_step(r, 60_000, 3, 1);
        assert_eq!((r2.phase, k2, r2.page), (RotPhase::Hold, 1.0, 0));
    }

    #[test]
    fn rot_page_switches_exactly_at_interval() {
        // rotate=1s、每拍 100ms：第 10 拍从 Hold 转 Out，且**这一拍 k 仍是 1**
        let mut r = Rot::default();
        for i in 0..9 {
            let (nr, k) = rot_step(r, 100, 1, 3);
            r = nr;
            assert_eq!(r.phase, RotPhase::Hold, "第 {i} 拍就切相了");
            assert_eq!(k, 1.0);
        }
        let (nr, k) = rot_step(r, 100, 1, 3);
        r = nr;
        assert_eq!(r.phase, RotPhase::Out, "满 1s 没有开始淡出");
        assert_eq!(k, 1.0, "刚开始淡出时 k 就该是 1");
        assert_eq!(r.page, 0, "淡出期间还是旧页");

        // 淡出中段 k 单调下降：60ms 时 ≈ 0.5
        let (nr, k) = rot_step(r, 60, 1, 3);
        r = nr;
        assert!((k - 0.5).abs() < 1e-6, "60/120 处 k 应为 0.5，实际 {k}");
        assert_eq!(r.phase, RotPhase::Out);

        // 再推 60ms（累计 120）⇒ 换页且进入淡入，k 从 0 开始
        let (nr, k) = rot_step(r, 60, 1, 3);
        r = nr;
        assert_eq!(r.phase, RotPhase::In, "淡出结束没有进入淡入");
        assert_eq!(r.page, 1, "淡出结束没有换页");
        assert!(k < 1e-6, "淡入起点 k 应为 0，实际 {k}");

        // 淡入 60ms ⇒ 0.5；再 60ms ⇒ 回到 Hold、k=1、页面停在 1
        let (nr, k) = rot_step(r, 60, 1, 3);
        r = nr;
        assert!((k - 0.5).abs() < 1e-6);
        let (nr, k) = rot_step(r, 60, 1, 3);
        r = nr;
        assert_eq!((r.phase, k, r.page), (RotPhase::Hold, 1.0, 1));
        assert_eq!(r.t_ms, 0, "回到 Hold 时计时没清零（下一轮会提前切）");
    }

    #[test]
    fn rot_wraps_page_and_carries_overshoot() {
        // 页号取模：最后一页之后回到 0
        let r = Rot {
            phase: RotPhase::Out,
            t_ms: 110,
            page: 2,
        };
        let (nr, _) = rot_step(r, 10, 2, 3); // 110+10 >= 120 ⇒ 换页
        assert_eq!(nr.page, 0, "最后一页没有回绕到 0");
        // 超出的毫秒被带进新相位：130 进 ⇒ 淡入已过 10ms
        let r = Rot {
            phase: RotPhase::Out,
            t_ms: 120,
            page: 0,
        };
        let (nr, k) = rot_step(r, 10, 2, 3);
        assert_eq!(nr.phase, RotPhase::In);
        assert_eq!(nr.t_ms, 10, "超出的毫秒没带进淡入（淡入会被拉长）");
        assert!((k - 10.0 / 120.0).abs() < 1e-6);
    }

    #[test]
    fn rot_k_stays_within_unit_range_and_is_continuous() {
        // 从 Hold 一路推到 Hold，逐拍记 k：必须落在 [0,1] 且相邻两拍不跳变 > 0.2
        let mut r = Rot::default();
        let mut prev = 1.0f32;
        let mut saw_dark = false;
        for _ in 0..200 {
            let (nr, k) = rot_step(r, 16, 1, 2);
            r = nr;
            assert!((0.0..=1.0).contains(&k), "k 越界：{k}");
            if (k - prev).abs() > 0.2 {
                // 唯一允许的跳变：Out→In 的换页瞬间（k 从接近 0 回到 0）
                assert!(prev < 0.2 && k < 0.2, "k 在 {prev} → {k} 处跳变");
            }
            prev = k;
            // 16ms 的拍子下最暗只到 k≈0.067（淡出末期 t=112/120），
            // 因为换页那一刻把超出的毫秒带进了淡入 —— 两段在 k 上是**连续**的，
            // 最暗不到 0 但不跳变。阈值取 0.1 就是为这条实测口径。
            if k < 0.1 {
                saw_dark = true;
            }
        }
        assert!(saw_dark, "跑完一整轮没出现过暗场（淡出没生效）");
        // 200 拍 = 3.2s ⇒ rotate=1s 至少切过两次
        assert!(r.page != usize::MAX);
    }
}
