//! 生物力学：配速、步频、步幅的非线性耦合。
//!
//! 硬约束：`v = (cadence / 60) · stride`（v: m/s，cadence: 步/分，stride: 米）
//!
//! **这个约束必须始终成立**。早期实现里步频和步数是各自随机生成的，两者与
//! 速度对不上——服务端只要拿 `totalSteps / totalTime` 和 `totalDis / totalTime`
//! 一除就能看出矛盾：步频 192 步/分 × 步幅必然推出的速度，与报文里的配速不符。
//!
//! 步频公式由真实记录回归得到，不是拍脑袋常数。四条样本（配速 5:31 /
//! 6:40 / 7:02 / 8:19）回归出的斜率与截距，拟合误差 ≤2 步/分。

/// 标准正态抽样（Box-Muller）。
///
/// 不引 `rand_distr`：它要额外拉依赖，而这里只需要一个标准正态。
fn normal_sample(rng: &mut impl rand::Rng) -> f64 {
    let u1 = rng.gen::<f64>().max(1e-12);
    let u2 = rng.gen::<f64>();
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

/// 步态：步频与步幅。
#[derive(Clone, Copy, Debug)]
pub struct Gait {
    /// 步频（步/分）。
    pub cadence: f64,
    /// 步幅（米）。
    pub stride: f64,
}

/// 由速度与疲劳度计算步态，严格满足 `v = cadence/60 * stride`。
///
/// 步频公式来自 4 条真实跑步记录的线性回归：`cadence = 34.4 + 44.5 * v`，
/// 拟合误差 ≤2 步/分。
///
/// | 配速 | 实测步频 | 公式预测 |
/// |------|---------|---------|
/// | 5:31/km (3.02 m/s) | 170 | 169 |
/// | 6:40/km (2.50 m/s) | 144 | 146 |
/// | 7:02/km (2.37 m/s) | 139 | 140 |
/// | 8:19/km (2.00 m/s) | 125 | 124 |
///
/// 常见的旧公式 `150 + 12v` 在 6:40/km 会给出 180 步/分，比实测高 36 步，
/// 而且形态是「步频主导」；真实数据是**步幅主导**——慢配速时步频掉得更多。
pub fn gait(v: f64, fatigue: f64) -> Gait {
    let target_cad = (34.4 + 44.5 * v) * (1.0 - 0.05 * fatigue);
    let mut cadence = target_cad.clamp(100.0, 210.0);
    let mut stride = if cadence > 0.0 { v * 60.0 / cadence } else { 0.0 };

    // 步幅越界时反解步频，保持等式成立
    if stride > 1.7 {
        stride = 1.7;
        cadence = v * 60.0 / stride;
    } else if stride < 0.5 {
        stride = 0.5;
        cadence = v * 60.0 / stride;
    }
    Gait { cadence, stride }
}

/// 由速度与疲劳度取步幅（米）。
pub fn stride_for(v: f64, fatigue: f64) -> f64 {
    gait(v, fatigue).stride
}

/// 由速度与步幅反解步频（步/分）。
pub fn cadence_for(v: f64, stride: f64) -> f64 {
    (60.0 * v / stride.max(1e-6)).clamp(110.0, 220.0)
}

/// 由步频与步幅计算速度（m/s）。
pub fn speed_from(cadence: f64, stride: f64) -> f64 {
    (cadence / 60.0) * stride
}

/// 疲劳度（0..0.6）：已跑 0.5km 后开始线性抬升，封顶 0.6。
pub fn fatigue_from_km(km: f64) -> f64 {
    if km <= 0.5 {
        0.0
    } else {
        ((km - 0.5) * 1.5).min(0.6)
    }
}

/// 步频的 OU（Ornstein-Uhlenbeck）均值回归序列。
///
/// ```text
/// c_{t+1} = c_t + θ(target_t − c_t)·Δt + σ·√Δt·Z,   Z ~ N(0,1)
/// ```
///
/// 为什么不用均匀随机抖动：真实步频有**惯性**——既不会逐点乱跳，也不会
/// 一直贴在均值上，而是带着自相关地向目标回归。OU 过程正好描述这个，
/// 均匀随机数做不到（相邻点独立，序列看起来像白噪声）。
pub fn ou_cadence_series(
    targets: &[f64],
    dts: &[f64],
    seed: u64,
    theta: f64,
    sigma: f64,
    floor: f64,
    ceil: f64,
) -> Vec<f64> {
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = Vec::with_capacity(targets.len());
    let mut c = targets.first().copied().unwrap_or(150.0);

    for (i, &target) in targets.iter().enumerate() {
        let dt = dts.get(i).copied().unwrap_or(1.0);
        c += theta * (target - c) * dt + sigma * dt.sqrt() * normal_sample(&mut rng);
        c = c.clamp(floor, ceil);
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 步频必须匹配真实记录，而不是旧的 `150 + 12v`。
    #[test]
    fn cadence_matches_real_records() {
        for (pace_s, want) in [(331.0, 170.0), (400.0, 144.0), (422.0, 139.0), (499.0, 125.0)] {
            let v = 1000.0 / pace_s;
            let g = gait(v, 0.0);
            assert!(
                (g.cadence - want).abs() <= 3.0,
                "配速 {pace_s:.0}s/km 步频 {:.0} 偏离实测 {want} 超过 3 步",
                g.cadence
            );
            assert!(
                (0.85..=1.30).contains(&g.stride),
                "配速 {pace_s:.0}s/km 步幅 {:.2}m 超出真实跑者区间",
                g.stride
            );
        }
    }

    /// 等式约束必须始终成立，否则服务端一除就露馅。
    #[test]
    fn identity_always_holds() {
        for v in [1.5, 2.0, 2.5, 3.0, 3.5, 4.0] {
            for fatigue in [0.0, 0.5, 1.0] {
                let g = gait(v, fatigue);
                let back = g.cadence / 60.0 * g.stride;
                assert!(
                    (back - v).abs() < 1e-9,
                    "v={v} fatigue={fatigue} 重建速度 {back} 不符"
                );
            }
        }
    }

    #[test]
    fn gait_stays_in_envelope() {
        for v in [1.9f64, 2.5, 3.5, 5.0, 6.3] {
            for fat in [0.0f64, 0.3, 0.6] {
                let g = gait(v, fat);
                assert!((0.5..=1.7).contains(&g.stride), "stride={}", g.stride);
                assert!((110.0..=230.0).contains(&g.cadence), "cad={}", g.cadence);
                assert!((speed_from(g.cadence, g.stride) - v).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn ou_series_is_bounded_and_deterministic() {
        let n = 200;
        let targets: Vec<f64> = (0..n).map(|i| 170.0 + 0.1 * i as f64).collect();
        let dts = vec![5.0; n];
        let a = ou_cadence_series(&targets, &dts, 42, 0.25, 0.5, 110.0, 220.0);
        assert_eq!(a.len(), n);
        for c in &a {
            assert!((110.0..=220.0).contains(c), "cadence={c}");
        }
        let b = ou_cadence_series(&targets, &dts, 42, 0.25, 0.5, 110.0, 220.0);
        assert_eq!(a, b, "同种子必须同结果");
    }

    /// OU 序列必须有自相关：相邻点差值的方差应远小于白噪声。
    #[test]
    fn ou_is_not_white_noise() {
        let n = 300;
        let targets = vec![160.0; n];
        let dts = vec![10.0; n];
        let s = ou_cadence_series(&targets, &dts, 7, 0.12, 0.8, 100.0, 220.0);
        let diffs: Vec<f64> = s.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        let mean_diff = diffs.iter().sum::<f64>() / diffs.len() as f64;
        let spread = s.iter().cloned().fold(f64::MIN, f64::max)
            - s.iter().cloned().fold(f64::MAX, f64::min);
        assert!(
            mean_diff < spread * 0.5,
            "相邻差值 {mean_diff:.3} 相对整体波动 {spread:.3} 过大，不像均值回归"
        );
    }

    #[test]
    fn fatigue_is_monotonic_and_capped() {
        assert_eq!(fatigue_from_km(0.4), 0.0);
        assert!(fatigue_from_km(1.0) > 0.0);
        assert_eq!(fatigue_from_km(5.0), 0.6);
        // 上限 0.6 在 0.9km 处就到（(0.9-0.5)*1.5 = 0.6），
        // 所以要在到达上限之前比单调性
        assert!(fatigue_from_km(0.8) > fatigue_from_km(0.6));
        assert_eq!(fatigue_from_km(0.9), 0.6);
    }
}
