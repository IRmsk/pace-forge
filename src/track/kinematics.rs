//! 运动学速度剖面：OU 配速 + 弯道预判刹车。
//!
//! 与「逐点硬限速」的区别：真人不会在入弯瞬间才减速，而是在**进入弯道前**
//! 就以有限制动减速度（约 1.5 m/s²）开始收速。这里对每个弧长点向后看
//! `lookahead_m` 范围内的曲率限速，施加 `v_now² ≤ v_corner² + 2·b·d`
//! 的可刹停约束，得到平滑且带预判的速度剖面。
//!
//! 为什么这比「随机抖动」强：服务端拿得到 `speedPerTenSec` 的全部数据，
//! 只要统计一下每桶速度的分布就能区分两类轨迹——
//!
//! - 均匀随机数：相邻桶互相独立，自相关接近 0，像白噪声
//! - OU 过程：带惯性地向目标回归，自相关显著（真人特征）
//!
//! 再加上弯道减速，直道快、弯道慢的节奏也会自然出现在数据里。
//!
//! 曲率由三点法现场计算，速度剖面基于 OU 过程。

use rand::rngs::StdRng;
use rand::SeedableRng;

use super::geo::{distance, M_PER_DEG_LAT, m_per_deg_lng};

/// 标准正态抽样（Box-Muller）。
///
/// 不引 `rand_distr`：它要额外拉依赖，而这里只需要一个标准正态。
fn normal_sample(rng: &mut impl rand::Rng) -> f64 {
    let u1 = rng.gen::<f64>().max(1e-12);
    let u2 = rng.gen::<f64>();
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

/// 运动学参数。
#[derive(Clone, Copy, Debug)]
pub struct KinParams {
    /// 最大向心加速度 m/s²（决定弯道限速上限）。
    pub a_c_max: f64,
    /// 最大制动减速度 m/s²（人体可接受的收速斜率）。
    pub brake_max: f64,
    /// 弯道预判前瞻距离（米）。
    pub lookahead_m: f64,
    /// 热身时长（秒）：起点从 0.85× 匀速 ramp 到 1.0×。
    pub warmup_s: f64,
    /// OU 均值回归速率 θ（越小越持久）。
    pub theta: f64,
    /// OU 波动率 σ（m/s / √s）。
    pub sigma: f64,
    /// 终点冲刺：最后这一段距离（米）开始提速，0 表示不冲刺。
    ///
    /// 真实跑者最后 100~200m 会提起速来，全程配速完全均匀反而不自然。
    pub kick_m: f64,
    /// 冲刺时相对巡航速度的提升幅度（0.12 = 快 12%）。
    pub kick_gain: f64,
}

impl Default for KinParams {
    fn default() -> Self {
        Self {
            a_c_max: 2.5,
            brake_max: 1.5,
            lookahead_m: 40.0,
            warmup_s: 20.0,
            theta: 0.12,
            sigma: 0.06,
            kick_m: 150.0,
            kick_gain: 0.12,
        }
    }
}

/// 弯道限速：由向心加速度上限反解 `v = sqrt(a_c / κ)`。
pub fn turn_speed_limit(a_c_max: f64, curvature: f64) -> f64 {
    if curvature <= 1e-9 {
        f64::INFINITY
    } else {
        (a_c_max / curvature).sqrt()
    }
}

/// 三点法的曲率（1/米）。
///
/// 平面近似：把经纬度按等距圆柱投影换算成米，再算转角除以平均边长。
fn curvature_at(
    a: (f64, f64),
    b: (f64, f64),
    c: (f64, f64),
    mid_lat: f64,
) -> f64 {
    // 必须取「沿运动方向」的两条边：a→b 和 b→c。
    // 早先取的是 b→a 和 b→c（都从 b 出发），直线上两者方向相反，
    // atan2 会给出 π 而不是 0，于是直线被算成超急弯。
    let (x1, y1) = (
        (b.1 - a.1) * m_per_deg_lng(mid_lat),
        (b.0 - a.0) * M_PER_DEG_LAT,
    );
    let (x2, y2) = (
        (c.1 - b.1) * m_per_deg_lng(mid_lat),
        (c.0 - b.0) * M_PER_DEG_LAT,
    );
    // 两条边的夹角
    let len1 = (x1 * x1 + y1 * y1).sqrt();
    let len2 = (x2 * x2 + y2 * y2).sqrt();
    if len1 < 1e-9 || len2 < 1e-9 {
        return 0.0;
    }
    let cross = (x1 * y2 - y1 * x2).abs();
    let dot = x1 * x2 + y1 * y2;
    let angle = cross.atan2(dot);
    // 曲率 ≈ 转角 / 平均边长
    angle / ((len1 + len2) / 2.0)
}

/// 轨迹的逐点累计弧长与曲率。
pub fn arc_and_curvature(points: &[(f64, f64)]) -> (Vec<f64>, Vec<f64>) {
    let n = points.len();
    let mut s = Vec::with_capacity(n);
    let mut k = vec![0.0; n];
    if n == 0 {
        return (s, k);
    }
    s.push(0.0);
    for i in 1..n {
        s.push(s[i - 1] + distance(points[i - 1], points[i]));
    }
    if n >= 3 {
        for i in 1..n - 1 {
            let mid_lat = points[i].0;
            k[i] = curvature_at(points[i - 1], points[i], points[i + 1], mid_lat);
        }
        // 端点沿用相邻值
        k[0] = k[1];
        k[n - 1] = k[n - 2];
    }
    (s, k)
}

/// 从弧长 `s0` 出发，在 lookahead 内取最小的「可刹停速度」。
///
/// 对每个前方曲率限速 `v_c`（距离 d），当前速度最多允许
/// `sqrt(v_c² + 2·b·d)`，取所有候选最小值。
pub fn speed_limit_ahead(s_list: &[f64], k_list: &[f64], s0: f64, p: &KinParams) -> f64 {
    let mut v_max = f64::INFINITY;
    for (i, &si) in s_list.iter().enumerate() {
        let d = si - s0;
        if d <= 0.0 || d > p.lookahead_m {
            continue;
        }
        let vc = turn_speed_limit(p.a_c_max, k_list[i].abs());
        if vc.is_finite() {
            let ok = (vc * vc + 2.0 * p.brake_max * d).sqrt();
            if ok < v_max {
                v_max = ok;
            }
        }
    }
    v_max
}

/// 生成速度序列：OU 均值回归 + 热身 ramp + 弯道预判限速 + 精确命中总里程。
///
/// 返回逐点速度（m/s），总里程精确等于 `target_dist`。
#[allow(clippy::too_many_arguments)]
pub fn pace_profile(
    points: &[(f64, f64)],
    v0: f64,
    dts: &[f64],
    target_dist: f64,
    seed: u64,
    p: &KinParams,
    floor: f64,
    ceil: f64,
) -> Vec<f64> {
    let n = dts.len();
    if n == 0 {
        return Vec::new();
    }
    let mut rng = StdRng::seed_from_u64(seed);

    // ① OU 均值回归 + 热身 ramp
    let mut w = Vec::with_capacity(n);
    let mut v = v0;
    let mut t = 0.0f64;
    for &dt in dts {
        let ramp = if t < p.warmup_s {
            0.85 + 0.15 * (t / p.warmup_s.max(1.0))
        } else {
            1.0
        };
        let mu = v0 * ramp;
        v += p.theta * (mu - v) * dt + p.sigma * dt.sqrt() * normal_sample(&mut rng);
        v = v.clamp(floor, ceil);
        w.push(v);
        t += dt;
    }

    // ①.5 终点冲刺
    //
    // 最后 kick_m 米线性提速到 (1 + kick_gain)×。真实跑者的收尾会明显快于
    // 巡航，全程一个速度跑完是最容易识别出来的特征之一。
    if p.kick_m > 0.0 && p.kick_gain > 0.0 {
        let total: f64 = w.iter().zip(dts).map(|(x, dt)| x * dt).sum();
        if total > p.kick_m * 2.0 {
            let kick_start = total - p.kick_m;
            let mut acc = 0.0f64;
            for i in 0..w.len() {
                let before = acc;
                acc += w[i] * dts[i];
                if acc > kick_start {
                    // 进入冲刺区，按已跑比例线性抬升
                    let prog = ((acc - kick_start) / p.kick_m).clamp(0.0, 1.0);
                    let boost = 1.0 + p.kick_gain * prog;
                    w[i] = (w[i] * boost).clamp(floor, ceil);
                }
                let _ = before;
            }
        }
    }

    // ② 弯道预判限速
    let (s_list, k_list) = arc_and_curvature(points);
    let mut caps = Vec::with_capacity(n);
    for i in 0..n {
        let cap = if i < s_list.len() {
            speed_limit_ahead(&s_list, &k_list, s_list[i], p).clamp(floor, ceil)
        } else {
            ceil
        };
        w[i] = w[i].min(cap);
        caps.push(cap);
    }

    // ③ 精确命中目标里程（尊重逐点 cap）
    fit_capped(&mut w, dts, target_dist, floor, &caps);
    w
}

/// 等比分摊使 `Σ v·dt` 精确等于 `target`，越界点钳在自己的 cap 上。
fn fit_capped(w: &mut [f64], dts: &[f64], target: f64, floor: f64, caps: &[f64]) {
    for _ in 0..64 {
        let cur: f64 = w.iter().zip(dts).map(|(x, dt)| x * dt).sum();
        if (cur - target).abs() <= 0.5 {
            break;
        }
        let k = target / cur;
        for i in 0..w.len() {
            let cap = caps.get(i).copied().unwrap_or(f64::INFINITY);
            w[i] = (w[i] * k).clamp(floor, cap);
        }
    }
    for i in 0..w.len() {
        let cap = caps.get(i).copied().unwrap_or(f64::INFINITY);
        w[i] = w[i].clamp(floor, cap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个带直角弯的折线，用于验证弯道降速。
    fn l_shaped(n: usize) -> Vec<(f64, f64)> {
        let mut pts = Vec::with_capacity(n);
        let half = n / 2;
        for i in 0..n {
            let d = i as f64 * 5.0; // 每点 5 米
            if i < half {
                pts.push((30.5, 104.0 + d / m_per_deg_lng(30.5)));
            } else {
                let turn = (half as f64) * 5.0;
                pts.push((30.5 + (d - turn) / M_PER_DEG_LAT, 104.0 + turn / m_per_deg_lng(30.5)));
            }
        }
        pts
    }

    #[test]
    fn curvature_zero_on_straight() {
        let pts: Vec<(f64, f64)> = (0..20)
            .map(|i| (30.5, 104.0 + i as f64 * 1e-5))
            .collect();
        let (_, k) = arc_and_curvature(&pts);
        for (i, v) in k.iter().enumerate() {
            assert!(v.abs() < 1e-4, "直线第 {i} 点曲率 {v} 应接近 0");
        }
    }

    #[test]
    fn curvature_high_at_corner() {
        let pts = l_shaped(40);
        let (_, k) = arc_and_curvature(&pts);
        let max_k = k.iter().cloned().fold(0.0f64, f64::max);
        assert!(max_k > 1e-3, "直角弯应有明显曲率，实测 max={max_k}");
    }

    #[test]
    fn speed_limits_on_corner() {
        let pts = l_shaped(40);
        let (s, k) = arc_and_curvature(&pts);
        let p = KinParams::default();
        // 弯道处的限速应低于直道
        let corner_i = k.iter().enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap()).unwrap().0;
        let v_corner = turn_speed_limit(p.a_c_max, k[corner_i].abs());
        assert!(v_corner.is_finite() && v_corner < 20.0, "弯道限速 {v_corner}");
        // 弯道前 40m 的点应感受到可刹停约束
        let before = s.get(corner_i.saturating_sub(6)).copied().unwrap_or(0.0);
        let lim = speed_limit_ahead(&s, &k, before, &p);
        assert!(lim.is_finite(), "弯道前应有限速约束");
    }

    #[test]
    fn profile_hits_target_distance() {
        let pts: Vec<(f64, f64)> = (0..200)
            .map(|i| (30.5, 104.0 + i as f64 * 5.0 / m_per_deg_lng(30.5)))
            .collect();
        let dts = vec![2.0; 199];
        let target = 1000.0;
        let w = pace_profile(&pts, 2.8, &dts, target, 42, &KinParams::default(), 1.0, 6.0);
        let total: f64 = w.iter().zip(&dts).map(|(v, dt)| v * dt).sum();
        assert!((total - target).abs() < 1.0, "总里程 {total} 偏离目标 {target}");
    }

    #[test]
    fn profile_respects_envelope() {
        let pts: Vec<(f64, f64)> = (0..100)
            .map(|i| (30.5, 104.0 + i as f64 * 5.0 / m_per_deg_lng(30.5)))
            .collect();
        let dts = vec![2.0; 99];
        let w = pace_profile(&pts, 2.5, &dts, 500.0, 1, &KinParams::default(), 1.5, 4.0);
        for v in &w {
            assert!((1.5..=4.0).contains(v), "速度 {v} 越界");
        }
    }

    /// 速度序列必须自相关（不是白噪声），否则分桶统计会露馅。
    #[test]
    fn profile_is_autocorrelated() {
        let pts: Vec<(f64, f64)> = (0..300)
            .map(|i| (30.5, 104.0 + i as f64 * 5.0 / m_per_deg_lng(30.5)))
            .collect();
        let dts = vec![1.5; 299];
        let w = pace_profile(&pts, 3.0, &dts, 1200.0, 7, &KinParams::default(), 1.0, 6.0);
        let diffs: Vec<f64> = w.windows(2).map(|x| (x[1] - x[0]).abs()).collect();
        let mean_diff = diffs.iter().sum::<f64>() / diffs.len() as f64;
        let spread = w.iter().cloned().fold(f64::MIN, f64::max)
            - w.iter().cloned().fold(f64::MAX, f64::min);
        assert!(
            mean_diff < spread * 0.4,
            "相邻速度差 {mean_diff:.4} 相对波动 {spread:.4} 过大，不像 OU 过程"
        );
    }

    /// 最后一段应该比中段快（终点冲刺）。
    #[test]
    fn finishes_with_a_kick() {
        let pts: Vec<(f64, f64)> = (0..400)
            .map(|i| (30.5, 104.0 + i as f64 * 5.0 / m_per_deg_lng(30.5)))
            .collect();
        let dts = vec![1.8; 399];
        let w = pace_profile(&pts, 2.8, &dts, 2000.0, 11, &KinParams::default(), 1.0, 8.0);
        let n = w.len();
        let mid_avg: f64 = w[n / 3..n * 2 / 3].iter().sum::<f64>() / (n / 3) as f64;
        let tail_avg: f64 = w[n - 20..].iter().sum::<f64>() / 20.0;
        assert!(
            tail_avg > mid_avg * 1.03,
            "尾段 {tail_avg:.3} 应明显快于中段 {mid_avg:.3}"
        );
    }

    /// 关掉冲刺时速度应回到平稳状态。
    #[test]
    fn kick_can_be_disabled() {
        let pts: Vec<(f64, f64)> = (0..200)
            .map(|i| (30.5, 104.0 + i as f64 * 5.0 / m_per_deg_lng(30.5)))
            .collect();
        let dts = vec![1.8; 199];
        let mut p = KinParams::default();
        p.kick_m = 0.0;
        let w = pace_profile(&pts, 2.8, &dts, 1000.0, 11, &p, 1.0, 8.0);
        let n = w.len();
        let mid: f64 = w[n / 3..n * 2 / 3].iter().sum::<f64>() / (n / 3) as f64;
        let tail: f64 = w[n - 20..].iter().sum::<f64>() / 20.0;
        assert!((tail - mid).abs() / mid < 0.06, "关闭冲刺后尾段 {tail:.3} 应接近中段 {mid:.3}");
    }

    #[test]
    fn deterministic_for_same_seed() {
        let pts: Vec<(f64, f64)> = (0..80)
            .map(|i| (30.5, 104.0 + i as f64 * 5.0 / m_per_deg_lng(30.5)))
            .collect();
        let dts = vec![2.0; 79];
        let a = pace_profile(&pts, 2.8, &dts, 400.0, 99, &KinParams::default(), 1.0, 6.0);
        let b = pace_profile(&pts, 2.8, &dts, 400.0, 99, &KinParams::default(), 1.0, 6.0);
        assert_eq!(a, b);
    }
}
