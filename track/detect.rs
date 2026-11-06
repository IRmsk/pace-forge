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
/// - 距离阈值 `10.0` 米：低于它拿满 30 分
/// - 总分门槛 `65`
/// - 基础分门槛：`w3 ≥ 8` 才可能凑够
#[derive(Debug, Clone, Copy)]
pub struct PointRule {
    /// 相邻点间距上限（米）。超过就丢分，但不超过 2 倍不做硬性拒绝。
    pub max_gap_m: f64,
    /// 硬性拒绝的间距（米）。
    pub reject_gap_m: f64,
    /// 单桶位移上限（米）。10 秒跑不了 100 米。
    pub max_bucket_m: f64,
    /// 单桶位移下限（米）。10 秒 0 米说明停了。
    pub min_bucket_m: f64,
}

impl Default for PointRule {
    fn default() -> Self {
        Self {
            max_gap_m: 10.0,     // 满分区
            reject_gap_m: 20.0,  // 超过两倍直接判异常
            max_bucket_m: 100.0, // 10 秒 100 米 = 36km/h
            min_bucket_m: 0.5,   // 10 秒不到半米 = 站着
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
                format!("第 {index} 点间距 {gap_m:.1}m，超出 10m 满分区")
            }
            Risk::HugeGap { index, gap_m } => {
                format!("第 {index} 点间距 {gap_m:.1}m，超过 20m 判异常")
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
pub fn inspect(points: &[(f64, f64)], buckets: &[SpeedBucket], rule: PointRule) -> Report {
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
        if b.distance <= 0.0 {
            zero += 1;
        } else if b.distance > rule.max_bucket_m {
            risks.push(Risk::FastBucket {
                index: i,
                dist_m: b.distance,
            });
        } else if b.distance < rule.min_bucket_m {
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
        // 5m 一个点，全部在满分区
        let pts: Vec<(f64, f64)> = (0..50)
            .map(|i| (30.5 + i as f64 * 0.000045, 104.0))
            .collect();
        let bs = vec![bucket(10.0), bucket(10.0), bucket(10.0)];
        let r = inspect(&pts, &bs, PointRule::default());
        assert!(!r.blocked(), "{:?}", r.risks);
        assert!(r.full_score_ratio > 0.95);
        assert!((r.avg_gap_m - 5.0).abs() < 0.5, "间距 {:.2}", r.avg_gap_m);
    }

    #[test]
    fn wide_gap_is_flagged_not_blocked() {
        // 纬度 0.000135° ≈ 15m
        let pts = vec![(30.5, 104.0), (30.500135, 104.0), (30.50027, 104.0)];
        let bs = vec![bucket(10.0)];
        let r = inspect(&pts, &bs, PointRule::default());
        assert!(!r.blocked(), "15m 只丢分，不该拦截: {:?}", r.risks);
        assert!(matches!(r.risks[0], Risk::WideGap { .. }));
    }

    #[test]
    fn huge_gap_blocks() {
        // 0.0003° ≈ 33m
        let pts = vec![(30.5, 104.0), (30.5003, 104.0)];
        let bs = vec![bucket(10.0)];
        let r = inspect(&pts, &bs, PointRule::default());
        assert!(r.blocked());
        assert!(matches!(r.risks[0], Risk::HugeGap { .. }));
    }

    #[test]
    fn zero_buckets_block() {
        let pts: Vec<(f64, f64)> = (0..10).map(|i| (30.5 + i as f64 * 1e-5, 104.0)).collect();
        let bs = vec![bucket(0.0), bucket(10.0), bucket(0.0)];
        let r = inspect(&pts, &bs, PointRule::default());
        assert!(r.blocked());
        assert!(r.risks.iter().any(|x| matches!(x, Risk::ZeroBuckets { count: 2 })));
    }

    #[test]
    fn fast_bucket_flagged() {
        let pts: Vec<(f64, f64)> = (0..10).map(|i| (30.5 + i as f64 * 1e-5, 104.0)).collect();
        let bs = vec![bucket(150.0)];
        let r = inspect(&pts, &bs, PointRule::default());
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
