//! 反检测：把 7.4.01 客户端本地会做的校验，在提交前自己先过一遍。
//!
//! 7.4.01 新增了 `libswsport.so`，其中的 `isValidPoint` 是一个**打分函数**，
//! 不是简单的布尔判断。反汇编还原出的逻辑：
//!
//! ```text
//! score = 基础分(w3 分段) + 模式分(w2) + 距离分(s1) + 时间分(s0)
//! valid = (w5 == 0) && (w4 != 0) && (score > 65)
//! ```
//!
//! 各项上限：基础分 30、模式分 30、距离分 30、时间分 10，合计 80。
//! 要超过 65 分，**基础分必须 ≥ 25**，即 `w3 ≥ 8`——不是随便一个点都算有效点。
//!
//! 这里不复刻它的整数运算（那需要 Java 侧调用点的真实取值），
//! 而是把**能从反汇编确定的边界**变成可执行的检查项。

use super::buckets::SpeedBucket;

/// 引擎认为的一个"有效点"的最低依据。
///
/// 守恒量（都是反汇编里直接看到的常量）：
/// - 距离阈值 `10.0` 米：**低于它拿满 30 分**——这是 `isValidPoint` 打分函数
///   的一个分档边界，不是「超过就判异常」的合法线
/// - 总分门槛 `65`
/// - 基础分门槛：`w3 ≥ 8` 才可能凑够
///
/// 早先这里把那个 `10.0` 当成了**合法性阈值**（`max_gap_m: 10.0`，
/// 双倍 `20.0` 直接拦），于是自己把自己锁死在一个假前提上：
///
/// 真机对象 `evidence/obs_real_20261002.json`（114 点 / 555 秒 / 1683.83m）
/// 逐点实测 `ΔtotalDis`：均值 14.90、p95 28.53、p99 30.60、最大 31.07，
/// **113 个间隔里 80 个超过 10 米、36 个超过 20 米**。按老阈值，
/// 这份「真实到不能再真实」的数据会被自己的体检拦下来（`HugeGap` 是
/// blocking）。也就是说 `reject_gap_m: 20.0` 从一开始就不是服务端的线，
/// 而是我们按「5 米一个采样点」的虚构节奏倒推出来的。
///
/// 现在的阈值直接落在真机分布上：`max_gap_m` 取真机 p99 之上（34），
/// `reject_gap_m` 取真机最大值之上、且仍低于 `preflight` 的 `GEO_JUMP_SOFT`
/// （60）——超过 50 米才叫「不是走出来的间距」。
#[derive(Debug, Clone, Copy)]
pub struct PointRule {
    /// 相邻点间距的提示阈值（米）。真机 p99 是 30.6，取 34。
    pub max_gap_m: f64,
    /// 硬性拒绝的间距（米）。真机最大 31.07，留 1.6 倍余量。
    pub reject_gap_m: f64,
    /// 单桶位移上限（米）。10 秒跑不了 100 米。
    pub max_bucket_m: f64,
    /// 单桶位移下限（米）。10 秒 0 米说明停了。
    pub min_bucket_m: f64,
}

impl Default for PointRule {
    fn default() -> Self {
        Self {
            max_gap_m: 34.0,     // 真机 p99 30.60 之上
            reject_gap_m: 50.0,  // 真机最大 31.07 之上，仍低于 GEO_JUMP_SOFT(60)
            max_bucket_m: 100.0, // 10 秒 100 米 = 36km/h
            min_bucket_m: 0.5,   // 10 秒不到半米 = 站着（起步窗口另算，见 `inspect`）
        }
    }
}

/// 一类问题。
#[derive(Debug, Clone, PartialEq)]
pub enum Risk {
    /// 相邻点间距过大，会被判非满分区。
    WideGap { index: usize, gap_m: f64 },
    /// 间距离谱，直接判异常。
    HugeGap { index: usize, gap_m: f64 },
    /// 某个 10 秒桶位移超上限。
    FastBucket { index: usize, dist_m: f64 },
    /// 某个桶几乎没动。
    StillBucket { index: usize, dist_m: f64 },
    /// 桶里全是零，看起来像断点。
    ZeroBuckets { count: usize },
}

impl Risk {
    /// 这一项是否会导致提交被拒。
    pub fn blocking(&self) -> bool {
        matches!(self, Risk::HugeGap { .. } | Risk::ZeroBuckets { .. })
    }

    pub fn describe(&self) -> String {
        match self {
            Risk::WideGap { index, gap_m } => {
                format!("第 {index} 点间距 {gap_m:.1}m，超出真机分布（p99 30.6m）")
            }
            Risk::HugeGap { index, gap_m } => {
                format!("第 {index} 点间距 {gap_m:.1}m，超过 50m 不像走出来的")
            }
            Risk::FastBucket { index, dist_m } => {
                format!("第 {index} 桶 {dist_m:.1}m/10s，太快")
            }
            Risk::StillBucket { index, dist_m } => {
                format!("第 {index} 桶只有 {dist_m:.1}m/10s，像停着")
            }
            Risk::ZeroBuckets { count } => {
                format!("{count} 个桶位移为 0，轨迹有断点")
            }
        }
    }
}

/// 体检结果。
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub risks: Vec<Risk>,
    /// 满分区内的点占比（0~1）。越高越像真跑。
    pub full_score_ratio: f64,
    /// 平均点间距。
    pub avg_gap_m: f64,
}

impl Report {
    pub fn blocked(&self) -> bool {
        self.risks.iter().any(|r| r.blocking())
    }

    pub fn lines(&self) -> Vec<String> {
        self.risks.iter().map(|r| r.describe()).collect()
    }

    pub fn summary(&self) -> String {
        if self.risks.is_empty() {
            format!(
                "点位检查通过 · 满分区 {:.1}% · 平均间距 {:.1}m",
                self.full_score_ratio * 100.0,
                self.avg_gap_m
            )
        } else {
            format!(
                "{} 项风险（{} 项拦截）· 满分区 {:.1}%",
                self.risks.len(),
                self.risks.iter().filter(|r| r.blocking()).count(),
                self.full_score_ratio * 100.0
            )
        }
    }
}

/// 按引擎的规则检查一遍轨迹点。
///
/// `points` 是 (lat, lon) 序列，`buckets` 是已经算好的 10 秒分桶。
///
/// `startup_end_ms` 是**起步窗口的结束时刻**（毫秒，0 表示不豁免）。
/// 起步窗口里点几乎不动是**真机的正常表现**，不是「站着不动」：
/// 真机对象前 40 秒的 10 秒桶位移是 `0 / 4 / 0 / 0 / 2.24 / 1.92`（米），
/// 三个零桶全在起步期。老的体检把这类桶一律当异常，等于要求真机自己
/// 也不合格。窗口内的桶跳过「几乎没动 / 零位移」两项，间距与小桶上限照查。
pub fn inspect(
    points: &[(f64, f64)],
    buckets: &[SpeedBucket],
    rule: PointRule,
    startup_end_ms: i64,
) -> Report {
    let mut risks = Vec::new();

    // ---- 相邻点间距 ----
    let mut gaps = Vec::with_capacity(points.len().saturating_sub(1));
    for i in 1..points.len() {
        let g = super::geo::distance(points[i - 1], points[i]);
        gaps.push(g);
        if g > rule.reject_gap_m {
            risks.push(Risk::HugeGap {
                index: i,
                gap_m: g,
            });
        } else if g > rule.max_gap_m {
            risks.push(Risk::WideGap {
                index: i,
                gap_m: g,
            });
        }
    }

    let full = gaps.iter().filter(|g| **g <= rule.max_gap_m).count();
    let full_score_ratio = if gaps.is_empty() {
        0.0
    } else {
        full as f64 / gaps.len() as f64
    };
    let avg_gap_m = if gaps.is_empty() {
        0.0
    } else {
        gaps.iter().sum::<f64>() / gaps.len() as f64
    };

    // ---- 分桶 ----
    let mut zero = 0usize;
    for (i, b) in buckets.iter().enumerate() {
        // 起步窗口：点几乎不动是真机行为，不参与「静止」判定
        let in_startup = startup_end_ms > 0 && b.begin_time < startup_end_ms;
        if b.distance <= 0.0 {
            if !in_startup {
                zero += 1;
            }
        } else if b.distance > rule.max_bucket_m {
            risks.push(Risk::FastBucket {
                index: i,
                dist_m: b.distance,
            });
        } else if b.distance < rule.min_bucket_m && !in_startup {
            risks.push(Risk::StillBucket {
                index: i,
                dist_m: b.distance,
            });
        }
    }
    if zero > 0 {
        risks.push(Risk::ZeroBuckets { count: zero });
    }

    Report {
        risks,
        full_score_ratio,
        avg_gap_m,
    }
}

// ---------------------------------------------------------------------------
// 字段策略：7.4.01 不该出现的东西
// ---------------------------------------------------------------------------

/// 7.4.01 移除了 `BODY_SENSORS` 权限，说明不再读心率。
///
/// 如果报文里还带心率字段，反而是"用旧版本伪造"的信号。
pub const FORBIDDEN_FIELDS: &[&str] = &[
    "heartRate",
    "heartRates",
    "hrData",
    "avgHeartRate",
    "maxHeartRate",
    "bodySensors",
];

/// 从报文里剔除 7.4.01 不该有的字段。
///
/// 返回被删掉的字段名，便于日志确认。
pub fn strip_forbidden(obj: &mut serde_json::Value) -> Vec<String> {
    let mut removed = Vec::new();
    if let Some(map) = obj.as_object_mut() {
        for f in FORBIDDEN_FIELDS {
            if map.remove(*f).is_some() {
                removed.push((*f).to_string());
            }
        }
    }
    removed
}

/// 报文是否干净（不含禁用字段）。
pub fn is_clean(obj: &serde_json::Value) -> bool {
    FORBIDDEN_FIELDS
        .iter()
        .all(|f| obj.get(*f).is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::buckets::SpeedBucket;

    fn bucket(d: f64) -> SpeedBucket {
        SpeedBucket {
            distance: d,
            begin_time: 0,
            end_time: 10_000,
            flag: 0,
            id: 0,
            state: 0,
            queue_num: 0,
        }
    }

    #[test]
    fn tight_points_pass() {
        // 5m 一个点（真机 4.8m/点那三条记录的口径），全部在提示阈值内
        let pts: Vec<(f64, f64)> = (0..50)
            .map(|i| (30.5 + i as f64 * 0.000045, 104.0))
            .collect();
        let bs = vec![bucket(10.0), bucket(10.0), bucket(10.0)];
        let r = inspect(&pts, &bs, PointRule::default(), 0);
        assert!(!r.blocked(), "{:?}", r.risks);
        assert!(r.full_score_ratio > 0.95);
        assert!((r.avg_gap_m - 5.0).abs() < 0.5, "间距 {:.2}", r.avg_gap_m);
    }

    /// 真机节奏（5 秒一点 ≈ 14.9m）必须**全部通过**，一个风险项都不许有。
    ///
    /// 这是那条把项目锁死在「5 米一点」上的错阈值的回归：`max_gap_m: 10.0`
    /// 下，真机对象 113 个间隔里有 80 个落在「非满分区」。
    #[test]
    fn real_device_cadence_passes_clean() {
        // 1683.83m / 113 个间隔，逐点按真机 Δdis 的分布走
        // 真机 `ΔtotalDis` 原样 113 个（`evidence/obs_real_20261002.json`）
        let real_dd: [f64; 113] = [
            0.0, 0.0, 0.0, 0.0, 4.0, 0.0, 0.0, 0.0, 0.0, 0.0,
            1.6, 1.6, 0.0, 2.4, 0.8, 1.6, 1.6, 4.8, 22.35, 28.32,
            24.39, 23.59, 29.77, 24.02, 19.51, 7.35, 14.52, 24.06, 31.07, 27.55,
            19.74, 22.54, 28.18, 26.97, 24.45, 23.45, 14.51, 17.47, 20.08, 16.73,
            10.95, 1.88, 0.94, 4.28, 12.49, 18.73, 16.55, 17.55, 6.56, 0.54,
            0.0, 2.91, 14.46, 28.53, 28.74, 30.6, 28.28, 25.67, 20.77, 16.39,
            18.71, 21.58, 19.25, 23.03, 24.7, 19.87, 18.35, 17.9, 17.44, 14.88,
            17.62, 14.71, 14.31, 11.21, 12.53, 14.24, 28.81, 28.47, 19.43, 16.14,
            14.73, 22.91, 21.53, 17.42, 21.93, 19.53, 16.35, 14.57, 18.41, 13.44,
            12.61, 11.81, 11.2, 4.61, 20.57, 24.94, 26.71, 23.45, 24.72, 27.74,
            25.34, 16.74, 18.49, 16.08, 10.0, 14.37, 10.33, 9.23, 12.28, 9.09,
            3.16, 0.57, 0.0,
        ];
        let mlat = 1.0 / 111_320.0;
        let mut pts = vec![(30.5, 104.0)];
        for d in real_dd.iter() {
            let (la, lo) = *pts.last().unwrap();
            pts.push((la + d * mlat, lo));
        }
        assert_eq!(pts.len(), 114, "真机是 114 个点");
        // 10 秒桶也用真机自己的（`totalDis` 在整 10 秒处插值相减）。
        // 起步那几个桶是 0 / 4 / 0 / 0 / 2.24 / 1.92 —— 真机起步期就是这样。
        let real_buckets: [f64; 55] = [
            0.0, 4.0, 0.0, 0.0, 2.24, 1.92, 2.88, 4.48, 36.55, 50.82,
            53.53, 36.87, 28.55, 56.52, 45.29, 52.49, 50.01, 35.56, 37.25, 21.74,
            3.78, 22.55, 34.81, 17.31, 1.49, 27.62, 58.1, 56.9, 42.73, 37.18,
            41.41, 46.46, 37.43, 34.14, 32.43, 27.62, 24.95, 48.74, 42.97, 33.58,
            42.24, 40.19, 33.89, 32.52, 25.39, 20.13, 33.31, 51.06, 49.89, 48.68,
            34.97, 29.23, 22.51, 21.4, 5.43,
        ];
        let bs: Vec<SpeedBucket> = real_buckets
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let mut b = bucket(*d);
                b.begin_time = i as i64 * 10_000;
                b.end_time = b.begin_time + 10_000;
                b
            })
            .collect();
        let r = inspect(&pts, &bs, PointRule::default(), 86_000);
        assert!(
            r.risks.is_empty(),
            "真机节奏不该有任何风险项：{:?}",
            r.lines()
        );
        assert!(!r.blocked());
        assert!(r.full_score_ratio > 0.99, "满区占比 {:.3}", r.full_score_ratio);
        assert!(
            (r.avg_gap_m - 14.9).abs() < 0.5,
            "平均间距 {:.2}m 应贴近真机 14.90m",
            r.avg_gap_m
        );
    }

    #[test]
    fn wide_gap_is_flagged_not_blocked() {
        // 40m（真机 p99 30.6 之上、50 之下）
        let pts = vec![(30.5, 104.0), (30.50036, 104.0), (30.50072, 104.0)];
        let bs = vec![bucket(10.0)];
        let r = inspect(&pts, &bs, PointRule::default(), 0);
        assert!(!r.blocked(), "40m 只提示，不该拦截: {:?}", r.risks);
        assert!(matches!(r.risks[0], Risk::WideGap { .. }));
    }

    #[test]
    fn huge_gap_blocks() {
        // 0.00055° ≈ 61m
        let pts = vec![(30.5, 104.0), (30.50055, 104.0)];
        let bs = vec![bucket(10.0)];
        let r = inspect(&pts, &bs, PointRule::default(), 0);
        assert!(r.blocked());
        assert!(matches!(r.risks[0], Risk::HugeGap { .. }));
    }

    #[test]
    fn zero_buckets_block() {
        let pts: Vec<(f64, f64)> = (0..10).map(|i| (30.5 + i as f64 * 1e-5, 104.0)).collect();
        let bs = vec![bucket(0.0), bucket(10.0), bucket(0.0)];
        let r = inspect(&pts, &bs, PointRule::default(), 0);
        assert!(r.blocked());
        assert!(r.risks.iter().any(|x| matches!(x, Risk::ZeroBuckets { count: 2 })));
    }

    /// 起步窗口里的零桶 / 静止桶不算异常（真机自己就有三个零桶）。
    /// 但窗口**之后**的零桶照样拦。
    #[test]
    fn startup_zero_buckets_are_exempt_but_late_ones_are_not() {
        let pts: Vec<(f64, f64)> = (0..10).map(|i| (30.5 + i as f64 * 1e-5, 104.0)).collect();
        let mut b0 = bucket(0.0);
        b0.begin_time = 0;
        let mut b1 = bucket(0.0);
        b1.begin_time = 20_000;
        let bs = vec![b0, bucket(12.0), b1];
        // 起步窗口 20 秒 → 只有第一个零桶被豁免，第二个照拦
        let r = inspect(&pts, &bs, PointRule::default(), 20_000);
        assert!(r.blocked(), "窗口后的零桶必须拦");
        assert!(r.risks.iter().any(|x| matches!(x, Risk::ZeroBuckets { count: 1 })));
        // 窗口覆盖全部零桶 → 不拦
        let r = inspect(&pts, &bs, PointRule::default(), 60_000);
        assert!(!r.blocked(), "窗口内零桶不该拦：{:?}", r.lines());
    }

    #[test]
    fn fast_bucket_flagged() {
        let pts: Vec<(f64, f64)> = (0..10).map(|i| (30.5 + i as f64 * 1e-5, 104.0)).collect();
        let bs = vec![bucket(150.0)];
        let r = inspect(&pts, &bs, PointRule::default(), 0);
        assert!(matches!(r.risks[0], Risk::FastBucket { .. }));
        assert!(!r.blocked(), "单桶过快只提示");
    }

    #[test]
    fn heart_rate_fields_are_stripped() {
        let mut v = serde_json::json!({
            "totalDis": 2000,
            "heartRate": 150,
            "avgHeartRate": 148,
            "speed": 600
        });
        assert!(!is_clean(&v));
        let removed = strip_forbidden(&mut v);
        assert_eq!(removed.len(), 2);
        assert!(is_clean(&v));
        assert_eq!(v["totalDis"], 2000);
    }
}
