//! 校园点位：`POST /api/v560/get/1/distance/1`（sportType=4）。
//!
//! 这个接口和别的不一样，有两个特殊字段：
//!
//! - `runec`：把 `{uid}{经度6位}{纬度6位}{整秒时间戳}` 打成**额外一层信封**，
//!   外层顺序是 `k,p,d,h,t`（不是标准体的 `d,h,k,p,t`）
//! - `sign`：`MD5(http版URL + 盐)`，注意是 **http://** 不是 https://
//!
//! 还有限流：**5 分钟 3 次**（错误码 10603）。所以必须有缓存，不能每次跑都拉。

use serde_json::{json, Value};

use super::client::{expect_ok, Client};
use super::policy::point_xy;
use crate::crypto::consts::{hosts, SIGN_SALT};
use crate::crypto::envelope::{now_ms, seal_with_order, OuterOrder};

/// 点位接口路径。
pub const POINTS_PATH: &str = "/api/v560/get/1/distance/1";

/// 缓存有效期（毫秒）。服务端 5 分钟 3 次，取 5 分钟做保守值。
pub const POINTS_TTL_MS: i64 = 300_000;

/// 点位上下文。
#[derive(Debug, Clone, Default)]
pub struct PointsContext {
    /// 原始点位数组（服务端原样结构）。
    pub points: Vec<Value>,
    /// 解析后的 BD-09 坐标。
    pub coords: Vec<(f64, f64)>,
}

/// 经纬度六位小数字符串。
fn six_digit(v: f64) -> String {
    format!("{v:.6}")
}

/// URL 签名：`MD5(http版URL + 盐)`。
///
/// 注意是把 `https://` 换成 `http://` 再算——早期用 https 算出来全是
/// `error=10003 签名错误`。
pub fn url_sign(url: &str) -> String {
    let http_url = url.replacen("https://", "http://", 1);
    let mut hasher = md5::Md5::new();
    use md5::Digest;
    hasher.update(format!("{http_url}{SIGN_SALT}").as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 构造 `runec` 载荷。
///
/// 明文 = `{uid}{lon:.6}{lat:.6}{整秒ms}`
pub fn runec_plain(uid: &str, lon: f64, lat: f64, start_ms: i64) -> String {
    format!(
        "{uid}{}{}{}",
        six_digit(lon),
        six_digit(lat),
        (start_ms / 1000) * 1000
    )
}

/// 拉点位（不带缓存）。
pub fn fetch_points(
    client: &Client,
    lat: f64,
    lon: f64,
    run_area_id: Option<&str>,
) -> Result<PointsContext, String> {
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(format!("锚点越界: ({lat}, {lon})"));
    }
    if lat == 0.0 && lon == 0.0 {
        return Err("锚点不能是 (0,0)".into());
    }

    let uid = client
        .credentials()
        .and_then(|c| c.uid.as_ref())
        .cloned()
        .unwrap_or_else(|| "0".into());
    let unid = client
        .credentials()
        .and_then(|c| c.unid.as_ref())
        .cloned()
        .unwrap_or_else(|| "0".into());

    let url = format!("{}{}", hosts::RUN, POINTS_PATH);
    let start_ms = now_ms() as i64;
    let runec_plain = runec_plain(&uid, lon, lat, start_ms);
    let env = seal_with_order(&runec_plain, OuterOrder::Observed)?;

    let mut body = json!({
        "sportType": 4,
        "longitude": lon,
        "latitude": lat,
        "sign": url_sign(&url),
        "uuid": super::client::uuid_v4(),
        "selectedUnid": unid,
        "runec": env.wire,
    });
    if let Some(area) = run_area_id {
        body["runAreaId"] = json!(area);
    }

    let raw = client.request("POST", &url, Some(&body))?;
    let v = expect_ok(&raw)?;
    parse_points(&v)
}

/// 解析点位响应。
pub fn parse_points(v: &Value) -> Result<PointsContext, String> {
    let payload = v.get("data").unwrap_or(v);
    let points = extract_points(payload);
    let coords = points.iter().filter_map(point_xy).collect();
    Ok(PointsContext { points, coords })
}

/// 从响应里找点位数组。
fn extract_points(payload: &Value) -> Vec<Value> {
    // 优先显式的打卡点字段名
    for name in [
        "pointsResModels",
        "pointList",
        "points",
        "runPoints",
        "checkPointList",
    ] {
        let Some(val) = payload.get(name) else { continue };
        match val {
            Value::Array(a) => return a.clone(),
            // 有些版本把数组序列化成字符串
            Value::String(s) => {
                if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(s) {
                    return a;
                }
            }
            _ => {}
        }
    }
    // 再试一层嵌套
    for wrapper in ["result", "data"] {
        if let Some(inner) = payload.get(wrapper) {
            if inner.get("pointsResModels").is_some() {
                return extract_points(inner);
            }
        }
    }
    Vec::new()
}

/// 点位中心（BD-09）。
pub fn center_bd(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    if points.is_empty() {
        return None;
    }
    let n = points.len() as f64;
    let lat = points.iter().map(|p| p.0).sum::<f64>() / n;
    let lon = points.iter().map(|p| p.1).sum::<f64>() / n;
    Some((lat, lon))
}

/// 点位包围盒（BD-09）：(min_lat, min_lon, max_lat, max_lon)。
pub fn bbox(points: &[(f64, f64)]) -> Option<(f64, f64, f64, f64)> {
    if points.is_empty() {
        return None;
    }
    let mut b = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for &(lat, lon) in points {
        b.0 = b.0.min(lat);
        b.1 = b.1.min(lon);
        b.2 = b.2.max(lat);
        b.3 = b.3.max(lon);
    }
    Some(b)
}

/// 把点位转成 GCJ-02（OSM 路网对齐用）。
pub fn to_gcj(points: &[(f64, f64)]) -> Vec<(f64, f64)> {
    points
        .iter()
        .map(|&(lat, lon)| super::coord::bd09_to_gcj02(lat, lon))
        .collect()
}

/// 边长（米），用于判断点位分布范围是否合理。
pub fn span_m(points: &[(f64, f64)]) -> f64 {
    match bbox(points) {
        Some((min_lat, min_lon, max_lat, max_lon)) => {
            const LAT_M: f64 = 111_132.0;
            let dy = (max_lat - min_lat) * LAT_M;
            let dx = (max_lon - min_lon) * LAT_M * ((min_lat + max_lat) / 2.0).to_radians().cos();
            (dx * dx + dy * dy).sqrt()
        }
        None => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_sign_uses_http() {
        // 同一个 URL，http 与 https 版本必须给出不同结果
        let a = url_sign("https://run.gxapp.iydsj.com/api/x");
        let b = url_sign("http://run.gxapp.iydsj.com/api/x");
        assert_eq!(a, b, "实现内部会把 https 换成 http，两者应一致");
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn runec_plain_format() {
        let s = runec_plain("12345", 104.0668001, 30.5728002, 1_730_000_123_456);
        // uid + lon6 + lat6 + 整秒ms
        assert!(s.starts_with("12345"));
        assert!(s.contains("104.066800"));
        assert!(s.contains("30.572800"));
        assert!(s.ends_with("1730000123000"), "时间戳应截断到整秒: {s}");
    }

    #[test]
    fn parses_points_from_various_shapes() {
        let v = json!({
            "data": {
                "pointsResModels": [
                    {"lat": 30.5, "lon": 104.0},
                    {"lat": 30.51, "lon": 104.01}
                ]
            }
        });
        let ctx = parse_points(&v).unwrap();
        assert_eq!(ctx.points.len(), 2);
        assert_eq!(ctx.coords.len(), 2);
        assert!((ctx.coords[0].0 - 30.5).abs() < 1e-9);
    }

    #[test]
    fn parses_string_encoded_array() {
        let inner = r#"[{"lat":30.5,"lon":104.0}]"#;
        let v = json!({ "data": { "pointsResModels": inner } });
        let ctx = parse_points(&v).unwrap();
        assert_eq!(ctx.points.len(), 1);
    }

    #[test]
    fn empty_response_is_not_error() {
        let v = json!({ "data": {} });
        let ctx = parse_points(&v).unwrap();
        assert!(ctx.points.is_empty());
    }

    #[test]
    fn center_and_bbox() {
        let pts = vec![(30.5, 104.0), (30.6, 104.2)];
        let (lat, lon) = center_bd(&pts).unwrap();
        assert!((lat - 30.55).abs() < 1e-9);
        assert!((lon - 104.1).abs() < 1e-9);
        let (a, b, c, d) = bbox(&pts).unwrap();
        assert_eq!((a, b, c, d), (30.5, 104.0, 30.6, 104.2));
        assert!(span_m(&pts) > 19_000.0, "跨 0.1 度纬度应约 11km+");
    }

    #[test]
    fn gcj_conversion_changes_coords() {
        let pts = vec![(30.5, 104.0)];
        let g = to_gcj(&pts);
        assert!((g[0].0 - 30.5).abs() > 1e-6, "BD→GCJ 应有偏移");
    }
}
