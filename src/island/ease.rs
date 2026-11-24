//! 灵动岛的缓动曲线。**纯函数、零 Win32、无依赖** —— 可以直接 `cargo test`。
//!
//! 只有两种曲线：展开用 ease-out（前段快、后段缓），收起用对称 ease-in-out。
//! 回弹（overshoot）刻意不做：它会让窗口矩形超出目标，牵扯裁剪与命中，
//! 收益远小于风险（见规格书 §3.2）。

/// 平滑阶跃：`t` 在 `[a, b]` 外分别取 0 / 1，区间内 `3x² - 2x³`。
///
/// `a > b` 视为退化，按「t < a 取 0」处理 —— 调用方不该传反向区间，
/// 但一个明确的兜底比一个 NaN 好定位。
pub fn smoothstep(a: f32, b: f32, t: f32) -> f32 {
    if b <= a {
        return if t < a { 0.0 } else { 1.0 };
    }
    let x = ((t - a) / (b - a)).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// 三次贝塞尔的两个控制点在某参数 `u` 处的分量值（起止点固定 0 / 1）。
fn bez(p1: f32, p2: f32, u: f32) -> f32 {
    let v = 1.0 - u;
    3.0 * v * v * u * p1 + 3.0 * v * u * u * p2 + u * u * u
}

/// `bez` 对 `u` 的导数。
fn bez_d(p1: f32, p2: f32, u: f32) -> f32 {
    let v = 1.0 - u;
    3.0 * v * v * p1 + 6.0 * v * u * (p2 - p1) + 3.0 * u * u * (1.0 - p2)
}

/// CSS `cubic-bezier(x1, y1, x2, y2)` 在横坐标 `t` 处的纵坐标。
///
/// 贝塞尔给的是参数方程 `(x(u), y(u))`，要的是 `y(x = t)` —— 所以先解
/// `x(u) = t`（牛顿迭代 4 次；导数太小或迭代跑出 `[0,1]` 时直接回退到
/// 线性初值，保证单调不失真），再代回 `y(u)`。
///
/// 误差量级：4 次迭代后 `|x(u) - t| < 1e-4`，对应纵向误差远小于 1px。
pub fn bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    if t <= 0.0 {
        return 0.0;
    }
    if t >= 1.0 {
        return 1.0;
    }
    let mut u = t;
    for _ in 0..4 {
        let x = bez(x1, x2, u) - t;
        let d = bez_d(x1, x2, u);
        if d.abs() < 1e-6 {
            break;
        }
        let next = (u - x / d).clamp(0.0, 1.0);
        // 收敛到同一点就停，省掉后面的迭代（也避免在极平段来回抖）
        if (next - u).abs() < 1e-6 {
            u = next;
            break;
        }
        u = next;
    }
    bez(y1, y2, u)
}

/// 展开曲线 `cubic-bezier(0.22, 0.61, 0.36, 1)`：前段快、后段缓。
pub fn expand(t: f32) -> f32 {
    bezier(0.22, 0.61, 0.36, 1.0, t)
}

/// 收起曲线 `cubic-bezier(0.40, 0.00, 0.60, 1)`：对称 ease-in-out。
pub fn collapse(t: f32) -> f32 {
    bezier(0.40, 0.0, 0.60, 1.0, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoothstep_endpoints_and_clamp() {
        assert_eq!(smoothstep(0.0, 1.0, -0.5), 0.0);
        assert_eq!(smoothstep(0.0, 1.0, 0.0), 0.0);
        assert_eq!(smoothstep(0.0, 1.0, 1.0), 1.0);
        assert_eq!(smoothstep(0.0, 1.0, 2.0), 1.0);
        // 中点 0.5，两端一阶导为 0（平滑起落）
        assert!((smoothstep(0.0, 1.0, 0.5) - 0.5).abs() < 1e-6);
        let e = 1e-4;
        assert!(smoothstep(0.0, 1.0, e) < 1e-6);
        assert!(smoothstep(0.0, 1.0, 1.0 - e) > 1.0 - 1e-6);
    }

    #[test]
    fn bezier_endpoints_exact() {
        for (x1, y1, x2, y2) in [(0.22, 0.61, 0.36, 1.0), (0.40, 0.0, 0.60, 1.0)] {
            assert_eq!(bezier(x1, y1, x2, y2, 0.0), 0.0);
            assert_eq!(bezier(x1, y1, x2, y2, 1.0), 1.0);
            assert_eq!(bezier(x1, y1, x2, y2, -1.0), 0.0);
            assert_eq!(bezier(x1, y1, x2, y2, 2.0), 1.0);
        }
    }

    #[test]
    fn bezier_monotonic_and_in_range() {
        for i in 0..=100 {
            let t = i as f32 / 100.0;
            let e = expand(t);
            let c = collapse(t);
            assert!((0.0..=1.0).contains(&e), "expand({t}) = {e} 越界");
            assert!((0.0..=1.0).contains(&c), "collapse({t}) = {c} 越界");
            if i > 0 {
                let pt = (i - 1) as f32 / 100.0;
                assert!(e >= expand(pt) - 1e-6, "expand 在 {t} 处回退");
                assert!(c >= collapse(pt) - 1e-6, "collapse 在 {t} 处回退");
            }
        }
    }

    /// 两条曲线的性格必须真的不同：ease-out 在中点应领先于线性，
    /// ease-in-out 在中点应正好等于线性。若把两条曲线写成同一组参数，
    /// 这条会红 —— 它是「展开显得利落、收起显得对称」的唯一判据。
    #[test]
    fn expand_leads_linear_collapse_is_symmetric() {
        assert!(expand(0.5) > 0.5, "expand(0.5) = {} 不大于线性", expand(0.5));
        assert!(
            (collapse(0.5) - 0.5).abs() < 0.02,
            "collapse(0.5) = {} 偏离对称",
            collapse(0.5)
        );
    }
}
