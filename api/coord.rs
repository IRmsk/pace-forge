//! 坐标系转换：WGS84 / GCJ-02 / BD-09。
//!
//! 服务端在不同接口上混用三套坐标：
//!
//! - **打卡点、围栏**：BD-09（百度系）
//! - **部分响应**：只填 `glat/glon`（GCJ-02），`lat/lon` 置 0
//! - **OSM 路网**：WGS84
//!
//! 混用的后果很直接：BD-09 与 GCJ-02 在同一位置能差 500 米以上，直接拿
//! WGS84 坐标去匹配打卡点会全部 miss。

/// 百度坐标的极坐标偏移常数。
const X_PI: f64 = std::f64::consts::PI * 3000.0 / 180.0;
/// GCJ-02 用的克拉索夫斯基椭球长半轴。
const WGS_A: f64 = 6378245.0;
/// 第一偏心率平方。
const WGS_EE: f64 = 0.00669342162296594323;

/// 是否在中国境外（境外不做偏移）。
fn out_of_china(lat: f64, lng: f64) -> bool {
    !(72.004..=137.8347).contains(&lng) || !(0.8293..=55.8271).contains(&lat)
}

fn transform_lat(x: f64, y: f64) -> f64 {
    let mut ret = -100.0 + 2.0 * x + 3.0 * y + 0.2 * y * y + 0.1 * x * y + 0.2 * x.abs().sqrt();
    ret += (20.0 * (6.0 * x * std::f64::consts::PI).sin() + 20.0 * (2.0 * x * std::f64::consts::PI).sin()) * 2.0 / 3.0;
    ret += (20.0 * (y * std::f64::consts::PI).sin() + 40.0 * (y / 3.0 * std::f64::consts::PI).sin()) * 2.0 / 3.0;
    ret += (160.0 * (y / 12.0 * std::f64::consts::PI).sin() + 320.0 * (y * std::f64::consts::PI / 30.0).sin()) * 2.0 / 3.0;
    ret
}

fn transform_lng(x: f64, y: f64) -> f64 {
    let mut ret = 300.0 + x + 2.0 * y + 0.1 * x * x + 0.1 * x * y + 0.1 * x.abs().sqrt();
    ret += (20.0 * (6.0 * x * std::f64::consts::PI).sin() + 20.0 * (2.0 * x * std::f64::consts::PI).sin()) * 2.0 / 3.0;
    ret += (20.0 * (x * std::f64::consts::PI).sin() + 40.0 * (x / 3.0 * std::f64::consts::PI).sin()) * 2.0 / 3.0;
    ret += (150.0 * (x / 12.0 * std::f64::consts::PI).sin() + 300.0 * (x / 30.0 * std::f64::consts::PI).sin()) * 2.0 / 3.0;
    ret
}

/// WGS84 → GCJ-02（火星坐标）。
pub fn wgs84_to_gcj02(lat: f64, lng: f64) -> (f64, f64) {
    if out_of_china(lat, lng) {
        return (lat, lng);
    }
    let mut dlat = transform_lat(lng - 105.0, lat - 35.0);
    let mut dlng = transform_lng(lng - 105.0, lat - 35.0);
    let radlat = lat / 180.0 * std::f64::consts::PI;
    let mut magic = radlat.sin();
    magic = 1.0 - WGS_EE * magic * magic;
    let sqrtmagic = magic.sqrt();
    dlat = (dlat * 180.0) / ((WGS_A * (1.0 - WGS_EE)) / (magic * sqrtmagic) * std::f64::consts::PI);
    dlng = (dlng * 180.0) / (WGS_A / sqrtmagic * radlat.cos() * std::f64::consts::PI);
    (lat + dlat, lng + dlng)
}

/// GCJ-02 → BD-09。
pub fn gcj02_to_bd09(gcj_lat: f64, gcj_lng: f64) -> (f64, f64) {
    let z = (gcj_lng * gcj_lng + gcj_lat * gcj_lat).sqrt() + 0.00002 * (gcj_lat * X_PI).sin();
    let theta = gcj_lat.atan2(gcj_lng) + 0.000003 * (gcj_lng * X_PI).cos();
    (z * theta.sin() + 0.006, z * theta.cos() + 0.0065)
}

/// BD-09 → GCJ-02。
pub fn bd09_to_gcj02(bd_lat: f64, bd_lng: f64) -> (f64, f64) {
    let x = bd_lng - 0.0065;
    let y = bd_lat - 0.006;
    let z = (x * x + y * y).sqrt() - 0.00002 * (y * X_PI).sin();
    let theta = y.atan2(x) - 0.000003 * (x * X_PI).cos();
    (z * theta.sin(), z * theta.cos())
}

/// WGS84 → BD-09。
pub fn wgs84_to_bd09(lat: f64, lng: f64) -> (f64, f64) {
    let (g_lat, g_lng) = wgs84_to_gcj02(lat, lng);
    gcj02_to_bd09(g_lat, g_lng)
}

/// BD-09 → WGS84。
pub fn bd09_to_wgs84(bd_lat: f64, bd_lng: f64) -> (f64, f64) {
    let (g_lat, g_lng) = bd09_to_gcj02(bd_lat, bd_lng);
    // 逆变换用一次迭代逼近
    let (d_lat, d_lng) = wgs84_to_gcj02(g_lat, g_lng);
    (
        g_lat - (d_lat - g_lat),
        g_lng - (d_lng - g_lng),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 天安门：WGS84 → GCJ02 → BD09 的已知偏移量级。
    #[test]
    fn offsets_are_in_expected_range() {
        let (lat, lng) = (39.9087, 116.3975);
        let (g_lat, g_lng) = wgs84_to_gcj02(lat, lng);
        // GCJ 相对 WGS 在北京偏移约 500m 量级
        let d = ((g_lat - lat).powi(2) + (g_lng - lng).powi(2)).sqrt() * 111_000.0;
        assert!((100.0..1200.0).contains(&d), "GCJ 偏移 {d:.0}m 不在预期量级");

        let (b_lat, b_lng) = gcj02_to_bd09(g_lat, g_lng);
        let d2 = ((b_lat - g_lat).powi(2) + (b_lng - g_lng).powi(2)).sqrt() * 111_000.0;
        assert!((100.0..1200.0).contains(&d2), "BD 偏移 {d2:.0}m 不在预期量级");
    }

    #[test]
    fn bd09_roundtrip() {
        for (lat, lng) in [(39.9087, 116.3975), (30.5728, 104.0668), (22.5431, 114.0579)] {
            let (b_lat, b_lng) = wgs84_to_bd09(lat, lng);
            let (r_lat, r_lng) = bd09_to_wgs84(b_lat, b_lng);
            let err = ((r_lat - lat).powi(2) + (r_lng - lng).powi(2)).sqrt() * 111_000.0;
            assert!(err < 3.0, "往返误差 {err:.2}m 太大 ({lat},{lng})");
        }
    }

    #[test]
    fn overseas_is_identity() {
        let (lat, lng) = (48.8566, 2.3522); // 巴黎
        assert_eq!(wgs84_to_gcj02(lat, lng), (lat, lng));
    }
}

#[cfg(test)]
mod point_coord_tests {
    use super::*;

    /// 采样点位的 lat/lon 与 glat/glon 到底是什么关系。
    #[test]
    fn probe_bd09_gcj02_relation() {
        // 真实点位：lon/lat 与 glon/glat
        let (lat, lon) = (47.384686_f64, 123.961263_f64);
        let (glat, glon) = (47.378334_f64, 123.954917_f64);

        let (c_lat, c_lng) = bd09_to_gcj02(lat, lon);
        println!("bd09_to_gcj02 -> {c_lat:.6},{c_lng:.6}  (期望接近 {glat:.6},{glon:.6})");

        let (b_lat, b_lng) = gcj02_to_bd09(glat, glon);
        println!("gcj02_to_bd09 <- {b_lat:.6},{b_lng:.6}  (期望接近 {lat:.6},{lon:.6})");

        let d1 = crate::track::geo::distance((c_lat, c_lng), (glat, glon));
        let d2 = crate::track::geo::distance((b_lat, b_lng), (lat, lon));
        println!("往返误差 {d1:.1}m / {d2:.1}m");
        assert!(d1 < 50.0, "bd09_to_gcj02 与 glat/glon 差 {d1:.0}m");
        assert!(d2 < 50.0, "gcj02_to_bd09 与 lat/lon 差 {d2:.0}m");
    }
}
