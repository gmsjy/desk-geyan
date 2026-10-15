//! 换句过渡状态机（PR1 动效内核）。
//!
//! **纯逻辑**：不碰 GDI、不碰窗口、不读配置 —— 全部边界都能 `cargo test`。
//! 接线在 `app.rs`（`step` / `needs_frame` / `build_frame` / `paint_main`）。
//!
//! 设计来自两轮思考，三条约束刻在结构里：
//!
//! 1. **入场是「获取」**：眼睛要定位、识别、开始读 ⇒ 快起慢落（ease-out）、
//!    短距离（≤0.5em）。对称 smoothstep 恰好反了，已弃用。
//! 2. **出场是「送走」**：旧内容已消费完、不需要追踪 ⇒ 慢起快走（ease-in）、
//!    决断不拖。拖长的出场是一个不肯走的幽灵占着新内容的位置。
//! 3. **顺序相位，从机制上消灭重影**：旧句先走透（Leave），新句再来（Arrive），
//!    两者从不同时满透明。原地交叉淡入会有约 150ms 的重影糊字；
//!    同方向传送带语义则要求「任何时刻只有一句话在场」—— Gap 相位就是为此存在：
//!    旧句 α 归 0 之后、新句掩码出现之前，谁都不画。

use std::f32::consts::TAU;

/// 退场时长（秒）。
pub const LEAVE_S: f32 = 0.16;
/// 空档时长（秒）：旧句已不可见、新句尚未出现 —— 让「切换」先于「出现」被感知。
pub const GAP_S: f32 = 0.14;
/// 入场时长（秒）。
pub const ARRIVE_S: f32 = 0.26;
/// 停留时长：每字秒数，夹在上下限之间。
pub const DWELL_PER_CHAR_S: f32 = 0.25;
pub const DWELL_MIN_S: f32 = 4.0;
pub const DWELL_MAX_S: f32 = 10.0;
/// 滑入距离（em）。
pub const SLIDE_EM: f32 = 0.5;
/// 呼吸动效参数（只作用于格言通道；时钟/日期/底板不参与）。
pub const BREATHE_FLOOR: f32 = 0.78;
pub const BREATHE_AMP: f32 = 0.22;
pub const BREATHE_PERIOD_S: f32 = 5.0;

/// 停留时长：dwell ∝ 内容量。按 5 字/s 从容读 + 50% 余量折算，
/// 夹在 `[4s, 10s]` —— 短句不浪费 dwell，长句不切断阅读。
pub fn dwell_secs(chars: usize) -> f32 {
    (chars as f32 * DWELL_PER_CHAR_S).clamp(DWELL_MIN_S, DWELL_MAX_S)
}

/// 入场缓动：cubic ease-out（快起慢落）。
pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t)
}

/// 出场缓动：cubic ease-in（慢起快走）。
pub fn ease_in_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t
}

/// 滑入距离（px）：格言字号的 0.5 个 em，至少 1px。
pub fn slide_px(quote_font_px: i32) -> i32 {
    ((quote_font_px as f32 * SLIDE_EM).round() as i32).max(1)
}

/// 呼吸曲线：`[0.56, 1.00]`（0.78 ± 0.22，与旧实现的振幅一致），周期 5s。
/// `phase_s` 是累计秒数（可超出一个周期）。
pub fn breathe_k(phase_s: f32) -> f32 {
    let t = (phase_s % BREATHE_PERIOD_S) / BREATHE_PERIOD_S;
    BREATHE_FLOOR + BREATHE_AMP * (t * TAU).sin()
}

/// 竖版入场的**单列**时长（秒）。比整块 `ARRIVE_S` 短 —— 错峰的总量
/// （`(列数-1) × 列间距`）必须和单列时长一起塞进 `ARRIVE_S` 里，
/// 否则最后一列的 ease-out 会被相位边界截断（k 从 <1 跳到 1 就是硬切）。
pub const COL_ARRIVE_S: f32 = 0.18;
/// 竖版入场的**列间错峰**期望值（秒）。列多、`ARRIVE_S` 装不下时按比例压缩，
/// 见 `column_k` 的 step 计算。
pub const COL_STAGGER_S: f32 = 0.045;

/// 竖版入场第 `i` 列（**0 = 最右**，阅读顺序）在 Arrive 相位 `t` 秒时的覆盖率。
///
/// 错峰总量压进 `ARRIVE_S` 内：`step = min(COL_STAGGER_S, (ARRIVE_S - COL_ARRIVE_S) / (列数-1))`。
/// 最后一列的开始时刻 `step × (列数-1) ≤ ARRIVE_S - COL_ARRIVE_S`，
/// 于是每列的 ease-out 都跑得完（`column_k(最后一列, 列数, ARRIVE_S) == 1`）。
pub fn column_k(i: usize, ncols: usize, t: f32) -> f32 {
    let last = ncols.saturating_sub(1) as f32;
    let step = if last <= 0.0 {
        0.0
    } else {
        ((ARRIVE_S - COL_ARRIVE_S).max(0.0) / last).min(COL_STAGGER_S)
    };
    ease_out_cubic((t - i as f32 * step) / COL_ARRIVE_S)
}

// ---- PR2 · 悬停暂停（纯判定，接线在 app.rs 的 tick/step/needs_frame）

/// 悬停确认时长（ms）：光标**连续**停在挂件上这么久才算「想读」。
/// 不对称迟滞的「进」侧 —— 光标路过挂件是常态（它 topmost 且常在工作区），
/// 没有迟滞跑马灯就会随光标穿过而走走停停。
pub const HOVER_HOLD_MS: u32 = 300;

/// 悬停判定的状态（累计器）。`inside` 只是快照，判据是 `held_ms`。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct HoverState {
    /// 上一拍光标是否在挂件上
    pub inside: bool,
    /// 已连续悬停的毫秒数（离开即清零）
    pub held_ms: u32,
}

/// 悬停暂停判定（纯函数，全边界可测）。
///
/// **不对称迟滞**（计划 12.3）：
/// - 「想读」要确认 —— 连续悬停 ≥ `HOVER_HOLD_MS` 才暂停（`held_ms` 累计，
///   中途离开清零，多段短路过永不触发）；
/// - 「不读了」不用猜 —— 离开**立即**恢复（返回 false 且 held 清零）。
///
/// 返回 `(是否暂停, 新状态)`。调用方把新状态存回 `App`，下一拍传回来。
/// 暂停**无上限**：光标停放多久就停多久（12.3#3，「不打扰」的题中之义）。
pub fn hover_decide(state: HoverState, inside: bool, dt_ms: u32) -> (bool, HoverState) {
    if inside {
        let held = state.held_ms.saturating_add(dt_ms);
        (held >= HOVER_HOLD_MS, HoverState { inside: true, held_ms: held })
    } else {
        (false, HoverState::default())
    }
}

/// 过渡相位。`Dwell` 是唯一的静止态（`is_active() == false`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// 停留：dwell 计时中，格言满覆盖率
    Dwell,
    /// 旧句退场：alpha 1→0、同向（左）退出
    Leave,
    /// 空档：旧句已不可见，新句尚未出现
    Gap,
    /// 新句入场：alpha 0→1、从右侧落定
    Arrive,
}

/// `step` 的产出。调用方（`app::step`）据此换句 / 归零 dwell 计时。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepOut {
    /// 什么都没发生
    None,
    /// 旧句已退场：**这一刻换句**（`app::advance`）。每次过渡恰好一次。
    Swap,
    /// 过渡结束、回到停留：dwell 计时器归零重计
    Settled,
}

pub struct Transition {
    phase: Phase,
    /// 相位内已走时间（秒）
    t: f32,
    /// 滑入距离（px），`start_leave` 时定下；0 = 纯淡入淡出（Fade 模式）
    slide: i32,
    /// 系统减动：三个时长全部归零（直切）。由 `DG_NO_TRANSITION` env 注入，
    /// 真实的系统开关（WinRT `UISettings`）要引 `windows` crate，暂不接。
    reduced: bool,
}

impl Transition {
    pub fn new(reduced: bool) -> Self {
        Self {
            phase: Phase::Dwell,
            t: 0.0,
            slide: 0,
            reduced,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// 是否处于减动模式。调用方据此把常驻动效（Breathe）也降级为静态 ——
    /// 减动用户不该被任何周期性振荡打扰（PR1.1：`DG_NO_TRANSITION` 原本只减过渡）。
    pub fn reduced(&self) -> bool {
        self.reduced
    }

    /// 过渡是否正在进行（= 本帧格言通道需要非常覆盖率）。
    pub fn is_active(&self) -> bool {
        self.phase != Phase::Dwell
    }

    /// 直接落定。手动换句（0 帧直切）/ 切换动效 / dump 的确定性都用它。
    pub fn settle(&mut self) {
        self.phase = Phase::Dwell;
        self.t = 0.0;
        self.slide = 0;
    }

    /// 启动一次退场。`slide` 由调用方按当前格言字号算好传入（见 `slide_px`）。
    pub fn start_leave(&mut self, slide: i32) {
        self.phase = Phase::Leave;
        self.t = 0.0;
        self.slide = slide.max(0);
    }

    /// 当前相位的时长（秒）。减动模式下三个过渡相位都是 0。
    fn dur(&self) -> f32 {
        if self.reduced {
            return 0.0;
        }
        match self.phase {
            Phase::Leave => LEAVE_S,
            Phase::Gap => GAP_S,
            Phase::Arrive => ARRIVE_S,
            Phase::Dwell => f32::INFINITY,
        }
    }

    /// 推进 `dt` 秒。
    ///
    /// 一帧之内可能连穿多个相位（减动模式下必然；掉帧时也可能），
    /// 所以用循环 + 剩余时间结转，而不是「一帧只推一相位」——
    /// 后者在零时长下会永远退不出 Leave。**每次调用最多产出一次 `Swap`**
    /// （循环最多穿过 Leave 一次），掉帧也不会一帧连换两句。
    pub fn step(&mut self, dt: f32) -> StepOut {
        if self.phase == Phase::Dwell || dt <= 0.0 {
            return StepOut::None;
        }
        let mut out = StepOut::None;
        let mut left = dt;
        let mut guard = 0;
        while self.phase != Phase::Dwell && left > 0.0 && guard < 8 {
            guard += 1;
            self.t += left;
            let d = self.dur();
            if self.t < d {
                break; // 本帧走不完这个相位，t 已累计
            }
            left = self.t - d; // 剩余时间结转给下一相位
            self.t = 0.0;
            match self.phase {
                Phase::Leave => {
                    self.phase = Phase::Gap;
                    out = StepOut::Swap;
                }
                Phase::Gap => self.phase = Phase::Arrive,
                Phase::Arrive => {
                    self.phase = Phase::Dwell;
                    if out == StepOut::None {
                        out = StepOut::Settled;
                    }
                }
                Phase::Dwell => {}
            }
        }
        out
    }

    /// 格言通道的覆盖率系数（0..1）—— compose 的 `quote_k`。
    ///
    /// Gap 期恒 0：旧句已不可见、新句还没画。「谁都不满透明」是顺序相位的
    /// 直接推论，也是「无重影」断言的判据。
    pub fn quote_k(&self) -> f32 {
        match self.phase {
            Phase::Dwell => 1.0,
            Phase::Leave => 1.0 - ease_in_cubic(self.t / LEAVE_S),
            Phase::Gap => 0.0,
            Phase::Arrive => ease_out_cubic(self.t / ARRIVE_S),
        }
    }

    /// 格言整块的 x 偏移（px）。退场同向退出、入场从外侧落定。
    ///
    /// 只有「下一句」一个方向：手动「上一句」是 0 帧直切（用户主动操作要
    /// 即时响应），不做动画，所以不需要反向。
    pub fn slide_x(&self) -> i32 {
        let s = self.slide as f32;
        match self.phase {
            Phase::Dwell | Phase::Gap => 0,
            Phase::Leave => (-s * ease_in_cubic(self.t / LEAVE_S)).round() as i32,
            Phase::Arrive => (s * (1.0 - ease_out_cubic(self.t / ARRIVE_S))).round() as i32,
        }
    }

    /// 竖版入场第 `i` 列（0 = 最右）的覆盖率。**只在 Arrive 相位错峰**；
    /// 其他相位（含未落定）返回统一的 `quote_k()` —— 竖版退场是整块均匀
    /// 淡出（送走只求快），不错峰。
    pub fn col_k(&self, i: usize, ncols: usize) -> f32 {
        if self.phase != Phase::Arrive || ncols <= 1 {
            return self.quote_k();
        }
        column_k(i, ncols, self.t)
    }

    /// 当前相位内已走时间（秒）。绘制端算逐列灰度 / 分带时用。
    pub fn phase_t(&self) -> f32 {
        self.t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dwell_clamps_and_monotonic() {
        assert_eq!(dwell_secs(0), DWELL_MIN_S);
        assert_eq!(dwell_secs(1), DWELL_MIN_S);
        assert!((dwell_secs(20) - 5.0).abs() < 1e-6);
        assert_eq!(dwell_secs(1000), DWELL_MAX_S);
        let mut last = -1.0;
        for n in 0..300 {
            let d = dwell_secs(n);
            assert!(d >= last, "dwell 在 {} 字处回落", n);
            last = d;
        }
    }

    #[test]
    fn curves_hit_endpoints_and_fast_start_slow_end() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert_eq!(ease_in_cubic(0.0), 0.0);
        assert_eq!(ease_in_cubic(1.0), 1.0);
        // 入场快起：半程应已走过 87.5%（远大于线性的一半）
        assert!(ease_out_cubic(0.5) > 0.8);
        // 出场慢起：半程只走了 12.5%
        assert!(ease_in_cubic(0.5) < 0.2);
        // 单调不减
        let mut last = 0.0;
        for i in 0..=20 {
            let e = ease_out_cubic(i as f32 / 20.0);
            assert!(e >= last);
            last = e;
        }
        let mut last = 0.0;
        for i in 0..=20 {
            let e = ease_in_cubic(i as f32 / 20.0);
            assert!(e >= last);
            last = e;
        }
    }

    #[test]
    fn phases_visit_in_order_with_one_swap() {
        let mut tr = Transition::new(false);
        tr.start_leave(10);
        assert!(tr.is_active());
        // 退场半程：k 在 (0,1)、位置向左（负）
        tr.step(LEAVE_S / 2.0);
        assert_eq!(tr.phase(), Phase::Leave);
        let k = tr.quote_k();
        assert!(k > 0.0 && k < 1.0, "半程 k={}", k);
        assert!(tr.slide_x() < 0, "退场应向左");
        // 推完退场：恰好一次 Swap，进入 Gap
        assert_eq!(tr.step(LEAVE_S / 2.0 + 1e-4), StepOut::Swap);
        assert_eq!(tr.phase(), Phase::Gap);
        // Gap 期新句掩码恒 0（顺序相位的核心保证）
        assert_eq!(tr.quote_k(), 0.0);
        assert_eq!(tr.slide_x(), 0);
        tr.step(GAP_S + 1e-4);
        assert_eq!(tr.phase(), Phase::Arrive);
        // 入场半程：k 在 (0,1)、位置在右侧（正）
        tr.step(ARRIVE_S / 2.0);
        let k2 = tr.quote_k();
        assert!(k2 > 0.0 && k2 < 1.0, "半程 k={}", k2);
        assert!(tr.slide_x() > 0, "入场应从右侧落定");
        // 落定
        assert_eq!(tr.step(ARRIVE_S / 2.0 + 1e-4), StepOut::Settled);
        assert!(!tr.is_active());
        assert_eq!(tr.quote_k(), 1.0);
        assert_eq!(tr.slide_x(), 0);
    }

    #[test]
    fn exactly_one_swap_per_cycle() {
        let mut tr = Transition::new(false);
        tr.start_leave(8);
        let mut swaps = 0;
        for _ in 0..200 {
            if tr.step(0.02) == StepOut::Swap {
                swaps += 1;
            }
            if !tr.is_active() {
                break;
            }
        }
        assert_eq!(swaps, 1);
    }

    #[test]
    fn long_dt_completes_cycle_without_double_swap() {
        // 掉帧（dt=1s）时整段过渡一帧走完，但仍只换一句
        let mut tr = Transition::new(false);
        tr.start_leave(10);
        assert_eq!(tr.step(1.0), StepOut::Swap);
        assert!(!tr.is_active());
        assert_eq!(tr.quote_k(), 1.0);
        for _ in 0..10 {
            assert_eq!(tr.step(0.05), StepOut::None);
        }
    }

    #[test]
    fn k_stays_in_unit_range_throughout() {
        let mut tr = Transition::new(false);
        tr.start_leave(10);
        for _ in 0..500 {
            let k = tr.quote_k();
            assert!((0.0..=1.0).contains(&k), "k={} 越界", k);
            tr.step(0.005);
        }
    }

    #[test]
    fn reduced_motion_cuts_instantly() {
        let mut tr = Transition::new(true);
        tr.start_leave(10);
        // 一帧之内连穿三个相位：产出 Swap、直接落定、k 回 1
        assert_eq!(tr.step(0.1), StepOut::Swap);
        assert!(!tr.is_active());
        assert_eq!(tr.quote_k(), 1.0);
    }

    #[test]
    fn settle_resets_mid_flight() {
        let mut tr = Transition::new(false);
        tr.start_leave(10);
        tr.step(LEAVE_S / 2.0);
        tr.settle();
        assert!(!tr.is_active());
        assert_eq!(tr.quote_k(), 1.0);
        assert_eq!(tr.slide_x(), 0);
    }

    #[test]
    fn slide_is_half_em_with_floor() {
        assert_eq!(slide_px(20), 10);
        assert_eq!(slide_px(6), 3);
        assert_eq!(slide_px(1), 1);
    }

    #[test]
    fn breathe_stays_in_band() {
        let (mut lo, mut hi) = (2.0f32, -1.0f32);
        for i in 0..1000 {
            let k = breathe_k(i as f32 * 0.01);
            assert!((0.0..=1.0).contains(&k));
            lo = lo.min(k);
            hi = hi.max(k);
        }
        // 与旧实现的振幅一致：0.78 ± 0.22
        assert!((lo - (BREATHE_FLOOR - BREATHE_AMP)).abs() < 0.01, "lo={}", lo);
        assert!((hi - (BREATHE_FLOOR + BREATHE_AMP)).abs() < 0.01, "hi={}", hi);
    }

    #[test]
    fn column_stagger_right_first_and_fits_arrive() {
        // 4 列：右列先亮，且相位走完时每列都到 1（ease-out 不被截断）
        let n = 4;
        let mid = ARRIVE_S * 0.5;
        let ks: Vec<f32> = (0..n).map(|i| column_k(i, n, mid)).collect();
        for w in ks.windows(2) {
            assert!(w[0] > w[1], "右列应先亮：{:?}", ks);
        }
        for i in 0..n {
            assert_eq!(column_k(i, n, ARRIVE_S), 1.0, "第 {} 列未落定", i);
            assert!(column_k(i, n, 0.0) <= column_k(0, n, 0.0) + 1e-6);
        }
        // 列多到装不下 45ms 期望间距时按比例压缩，仍不许截断
        for n in [2usize, 3, 5, 8, 12, 20] {
            for i in 0..n {
                assert_eq!(column_k(i, n, ARRIVE_S), 1.0, "{} 列的第 {} 列被截断", n, i);
            }
            // 任意时刻右列不小于左列
            for s in 0..=26 {
                let t = s as f32 * 0.01;
                for i in 1..n {
                    assert!(column_k(i - 1, n, t) >= column_k(i, n, t) - 1e-6);
                }
            }
        }
        // 单列 = 普通入场曲线（无错峰）：半程即 ease_out(0.5)
        assert!((column_k(0, 1, COL_ARRIVE_S / 2.0) - ease_out_cubic(0.5)).abs() < 1e-6);
        assert_eq!(column_k(0, 1, 0.0), 0.0);
        assert_eq!(column_k(0, 1, ARRIVE_S), 1.0);
    }

    #[test]
    fn col_k_only_staggers_in_arrive() {
        let mut tr = Transition::new(false);
        tr.start_leave(10);
        // Leave / Gap：列间不差（整块均匀）
        tr.step(LEAVE_S / 2.0);
        let n = 4;
        let ks: Vec<f32> = (0..n).map(|i| tr.col_k(i, n)).collect();
        assert!(ks.iter().all(|k| (k - ks[0]).abs() < 1e-6), "Leave 期不应错峰：{:?}", ks);
        // 推进入场半程：开始错峰
        tr.step(LEAVE_S / 2.0 + GAP_S + 1e-4);
        assert_eq!(tr.phase(), Phase::Arrive);
        tr.step(ARRIVE_S / 2.0);
        let ks2: Vec<f32> = (0..n).map(|i| tr.col_k(i, n)).collect();
        for w in ks2.windows(2) {
            assert!(w[0] > w[1], "Arrive 期应右列先亮：{:?}", ks2);
        }
        // 落定后回到统一 1.0
        tr.step(ARRIVE_S);
        let ks3: Vec<f32> = (0..n).map(|i| tr.col_k(i, n)).collect();
        assert!(ks3.iter().all(|k| (*k - 1.0).abs() < 1e-6));
    }

    #[test]
    fn step_on_dwell_is_noop() {
        let mut tr = Transition::new(false);
        assert_eq!(tr.step(0.1), StepOut::None);
        assert_eq!(tr.quote_k(), 1.0);
        // dt=0 不越界
        tr.start_leave(5);
        assert_eq!(tr.step(0.0), StepOut::None);
        assert_eq!(tr.phase(), Phase::Leave);
    }

    // ---- PR2 · 悬停暂停：hover_decide 全边界组合

    #[test]
    fn hover_needs_continuous_hold_to_pause() {
        let s = HoverState::default();
        // 不够 300ms：一路走都不暂停
        let mut st = s;
        for ms in [50u32, 100, 149] {
            let (paused, ns) = hover_decide(st, true, ms);
            assert!(!paused, "{}ms 不应暂停", st.held_ms + ms);
            st = ns;
        }
        // 跨过 300ms 那一拍暂停
        let (paused, _) = hover_decide(st, true, 1);
        assert!(paused);
        // 恰好 300ms 也算（≥）
        let (p2, _) = hover_decide(s, true, HOVER_HOLD_MS);
        assert!(p2);
    }

    #[test]
    fn hover_leave_resumes_immediately_and_resets() {
        // 先悬停到暂停态
        let (_, st) = hover_decide(HoverState::default(), true, HOVER_HOLD_MS);
        // 离开：立即恢复（false），且累计器清零
        let (paused, st2) = hover_decide(st, false, 1000);
        assert!(!paused);
        assert_eq!(st2, HoverState::default());
        // 清零后重新路过 200ms 不暂停（不许带着旧账）
        let (p3, _) = hover_decide(st2, true, 200);
        assert!(!p3);
    }

    #[test]
    fn hover_passing_never_triggers() {
        // 反复「路过」：每段都 <300ms 且中间离开 —— 永不暂停
        let mut st = HoverState::default();
        for _ in 0..100 {
            let (p1, ns) = hover_decide(st, true, 299);
            assert!(!p1);
            let (p2, ns2) = hover_decide(ns, false, 10);
            assert!(!p2);
            st = ns2;
        }
        assert_eq!(st.held_ms, 0);
    }

    #[test]
    fn hover_hold_saturates_without_overflow() {
        // u32 不溢出：长时间停放（几小时）也安全
        let st = HoverState { inside: true, held_ms: u32::MAX - 5 };
        let (paused, ns) = hover_decide(st, true, 100);
        assert!(paused);
        assert_eq!(ns.held_ms, u32::MAX);
    }
}
