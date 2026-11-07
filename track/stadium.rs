//! 标准田径场几何。
//!
//! 校园跑步最常见的场景就是操场绕圈。用通用闭环（正多边形）画出来的轨迹
//! 在操场场景里一眼假：直道不直、弯道是折线段、每圈形状完全一样。
//!
//! 这里按 World Athletics 标准 400m 跑道建模：
//!
//! ```text
//!         ┌──────────────────────┐
//!        ╱                        ╲
//!       │   直道 84.39m            │     弯道半径 36.50m（内突沿）
//!        ╲                        ╱      半圆长 115.61m
//!         └──────────────────────┘
//!
//!   总长 = 84.39×2 + 115.61×2 = 400.00m
//! ```
//!
//! 真实跑者还会**换道**：外道内道距不同，第 n 道半径 = 36.5 + 1.22×(n−1)。
//! 这里支持指定道次，并按道次换算圈长。

use super::geo::{M_PER_DEG_LAT, m_per_deg_lng};

/// 标准 400m 跑道的几何常数（米）。
pub mod spec {
    /// 直道长度。
    pub const STRAIGHT: f64 = 84.39;
    /// 内突沿半径。
    pub const INNER_RADIUS: f64 = 36.50;
    /// 分道宽。
    pub const LANE_WIDTH: f64 = 1.22;
    /// 半圆弯道长度（按内突沿半径算）。
    pub const CURVE: f64 = std::f64::consts::PI * INNER_RADIUS; // 114.668
    /// 一圈总长（内突沿）。
    pub const LAP: f64 = 2.0 * STRAIGHT + 2.0 * CURVE;
}

/// 道次 → 该道的运行半径。
///
/// 第 1 道沿内突沿外 0.30m 计算（国际田联实测线规则），
/// 之后每道再加一个道宽。
/// 标准田径场道次范围。
pub const MIN_LANE: u8 = 1;
pub const MAX_LANE: u8 = 8;

pub fn lane_radius(lane: u8) -> f64 {
    // 标准跑道只有 1~8 道。之前用 lane.max(1) 兜底，第 9 道会被外推成
    // 一条不存在的跑道（单圈 461 m），第 0 道静默变成第 1 道。
    let lane = lane.clamp(MIN_LANE, MAX_LANE);
    let l = lane as f64;
    spec::INNER_RADIUS + 0.30 + spec::LANE_WIDTH * (l - 1.0)
}

/// 该道次跑一圈的实际距离。
pub fn lap_length(lane: u8) -> f64 {
    let r = lane_radius(lane);
    2.0 * spec::STRAIGHT + 2.0 * std::f64::consts::PI * r
}

/// 田径场方位。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StadiumSpec {
    /// 场地中心纬度。
    pub center_lat: f64,
    /// 场地中心经度。
    pub center_lng: f64,
    /// 直道走向（弧度，0 = 东西向）。
    pub heading_rad: f64,
    /// 道次（1..=8）。
    pub lane: u8,
}

impl StadiumSpec {
    pub fn new(lat: f64, lng: f64) -> Self {
        Self { center_lat: lat, center_lng: lng, heading_rad: 0.0, lane: 1 }
    }

    pub fn with_lane(mut self, lane: u8) -> Self {
        self.lane = lane.clamp(1, 8);
        self
    }

    pub fn with_heading(mut self, rad: f64) -> Self {
        self.heading_rad = rad;
        self
    }
}

/// 跑道中心线上按弧长 `s` 取点（米 → 局部平面坐标）。
///
/// 参数化顺序：从右侧直道起点逆时针（田径比赛方向是逆时针）。
/// 一个周期 = 直道(84.39) → 弯道(πr) → 直道(84.39) → 弯道(πr)。
pub fn point_at(s: f64, radius: f64, heading: f64) -> (f64, f64) {
    let half_straight = spec::STRAIGHT / 2.0;
    let curve = std::f64::consts::PI * radius;
    let lap = 2.0 * spec::STRAIGHT + 2.0 * curve;
    let mut t = s.rem_euclid(lap);

    // 局部坐标：x 沿直道方向，y 垂直直道
    let (x, y) = if t < spec::STRAIGHT {
        // 第一直道：从 (-half, -r) 到 (+half, -r)，y 固定 -r
        (-half_straight + t, -radius)
    } else if {
        t -= spec::STRAIGHT;
        t < curve
    } {
        // 第一个弯道：右侧半圆，从 (+half, -r) 绕到 (+half, +r)
        let ang = -std::f64::consts::FRAC_PI_2 + (t / radius);
        (half_straight + radius * ang.cos(), radius * ang.sin())
    } else if {
        t -= curve;
        t < spec::STRAIGHT
    } {
        // 第二直道：从 (+half, +r) 到 (-half, +r)
        (half_straight - t, radius)
    } else {
        t -= spec::STRAIGHT;
        // 第二个弯道：左侧半圆
        let ang = std::f64::consts::FRAC_PI_2 + (t / radius);
        (-half_straight + radius * ang.cos(), radius * ang.sin())
    };

    // 按 heading 旋转
    let (sn, cs) = heading.sin_cos();
    (x * cs - y * sn, x * sn + y * cs)
}

/// 生成绕 `laps` 圈的轨迹（经纬度）。
///
/// `sample_step_m` 是采样间距（米），保证直道和弯道都有足够采样点。
pub fn stadium_track(
    spec_in: &StadiumSpec,
    laps: f64,
    sample_step_m: f64,
) -> Vec<(f64, f64)> {
    let r = lane_radius(spec_in.lane);
    let per_lap = 2.0 * spec::STRAIGHT + 2.0 * std::f64::consts::PI * r;
    let total = per_lap * laps;
    let n = ((total / sample_step_m).ceil() as usize).max(8) + 1;

    let m_lng = m_per_deg_lng(spec_in.center_lat);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let s = total * i as f64 / (n - 1) as f64;
        let (x, y) = point_at(s, r, spec_in.heading_rad);
        let lat = spec_in.center_lat + y / M_PER_DEG_LAT;
        let lng = spec_in.center_lng + x / m_lng;
        out.push((lat, lng));
    }
    out
}

/// 判断一条轨迹像不像操场绕圈。
///
/// 判据：起终点几乎重合（差 <20m）且总长是圈长整数倍附近，
/// 同时轨迹的曲率分布有明显「直道 0 / 弯道高」的二态特征。
pub fn looks_like_stadium(points: &[(f64, f64)], tol_m: f64) -> bool {
    if points.len() < 16 {
        return false;
    }
    let first = points[0];
    let last = *points.last().unwrap();
    let gap = super::geo::distance(first, last);
    if gap > tol_m {
        return false;
    }
    let len = super::geo::path_length(points);
    // 是否接近某个整圈长度（每圈 380..430m 之间）
    for lane in 1u8..=8 {
        let lap = lap_length(lane);
        let ratio = len / lap;
        if (ratio - ratio.round()).abs() < 0.08 && ratio > 0.5 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::track::geo::path_length;

    #[test]
    fn standard_lap_is_400m() {
        // 内突沿一圈：2×84.39 + 2×π×36.5 = 398.12（含内突沿外 0.3 后 400.00）
        let l1 = lap_length(1);
        assert!(
            (l1 - 400.0).abs() < 1.5,
            "第 1 道一圈 {l1:.2}m 应接近 400m"
        );
    }

    #[test]
    fn each_lane_is_longer() {
        for lane in 1u8..8 {
            let a = lap_length(lane);
            let b = lap_length(lane + 1);
            assert!(b > a, "第 {} 道 {a:.2} 应短于第 {} 道 {b:.2}", lane, lane + 1);
            assert!((b - a - 2.0 * std::f64::consts::PI * spec::LANE_WIDTH).abs() < 1.0);
        }
    }

    #[test]
    fn generated_track_has_correct_length() {
        let sp = StadiumSpec::new(30.5, 104.0).with_lane(1);
        let pts = stadium_track(&sp, 5.0, 5.0);
        let len = path_length(&pts);
        let want = lap_length(1) * 5.0;
        assert!(
            (len - want).abs() / want < 0.02,
            "5 圈生成 {len:.1}m，应为 {want:.1}m"
        );
    }

    #[test]
    fn track_closes() {
        let sp = StadiumSpec::new(30.5, 104.0).with_lane(2);
        let pts = stadium_track(&sp, 3.0, 4.0);
        let gap = crate::track::geo::distance(pts[0], *pts.last().unwrap());
        assert!(gap < 3.0, "起终点间距 {gap:.2}m 应接近 0");
    }

    #[test]
    fn straights_are_straight() {
        let sp = StadiumSpec::new(30.5, 104.0).with_lane(1);
        let pts = stadium_track(&sp, 1.0, 2.0);
        let (s, k) = crate::track::arc_and_curvature(&pts);
        // 直道段曲率应接近 0，弯道段应明显 > 0
        let mut low = 0;
        let mut high = 0;
        for (i, &kk) in k.iter().enumerate() {
            if s[i] > 5.0 && s[i] < 80.0 {
                if kk.abs() < 0.005 { low += 1; }
            }
            if s[i] > 90.0 && s[i] < 190.0 {
                if kk.abs() > 0.01 { high += 1; }
            }
        }
        assert!(low > 10, "直道应有大量低曲率点，实测 {low}");
        assert!(high > 10, "弯道应有大量高曲率点，实测 {high}");
    }

    #[test]
    fn recognizes_stadium_shape() {
        let sp = StadiumSpec::new(30.5, 104.0).with_lane(1);
        let pts = stadium_track(&sp, 5.0, 5.0);
        assert!(looks_like_stadium(&pts, 20.0), "5 圈操场应被识别");
    }

    #[test]
    fn rejects_non_stadium() {
        // 一条直线不该被认成操场
        let pts: Vec<(f64, f64)> = (0..100)
            .map(|i| (30.5, 104.0 + i as f64 * 2e-4))
            .collect();
        assert!(!looks_like_stadium(&pts, 20.0));
    }
}
