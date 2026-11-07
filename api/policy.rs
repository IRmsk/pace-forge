//! 跑步策略：`POST /api/v70103/runModePolicy`。
//!
//! 提交成绩前必须先拉这个接口，它给出三样必需的东西：
//!
//! - `policy`：策略串，提交时原样回传，服务端用它校验"你是什么时候拉的规则"
//! - `minDistance`：本次有效里程的下限，`selDistance` 不能低于它
//! - `mustPoints`：必经打卡点（BD-09）
//!
//! `runMode` 区分项目：1=校园跑（有 11016 时段限制），2=户外跑，3=室外锻炼。

use serde_json::{json, Value};

use super::client::{expect_ok, Client};
use super::coord::gcj02_to_bd09;
use crate::crypto::consts::hosts;

/// 规则接口路径。
pub const POLICY_PATH: &str = "/api/v70103/runModePolicy";

/// 策略信息。
#[derive(Debug, Clone, Default)]
pub struct PolicyInfo {
    /// 服务端时间戳（毫秒），提交时要带上做时间对齐。
    pub timestamp: i64,
    /// 策略串。
    pub policy: i64,
    /// 有效里程下限（米）。
    pub min_distance: i64,
    /// 规则有效时长（秒）。
    pub valid_time: i64,
    /// 必经点（BD-09）。
    pub must_points: Vec<(f64, f64)>,
    /// 允许的配速区间（分/公里，小=快）。服务端 `speedTop`/`speedBottom`。
    ///
    /// 之前这两个值被 `let _ =` 直接丢掉，导致 retime 用硬编码 1.0~8.0 m/s
    /// （相当于 2:05~16:40 配速），preflight 的配速检查因为拿到 None 永远
    /// 不执行——自检报「全部通过」，发出去却是 2 分速，必驳回。
    pub pace_range: Option<(f64, f64)>,
}

/// 从 policy 响应里取字段，兼容 `data` 包一层和直接平铺两种情况。
fn get_field<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    if let Some(inner) = v.get("data") {
        if inner.get(key).is_some() {
            return inner.get(key);
        }
    }
    v.get(key)
}

/// 解析 policy 响应。
pub fn parse_policy_response(biz: &Value) -> Result<PolicyInfo, String> {
    let timestamp = get_field(biz, "timestamp")
        .and_then(|t| t.as_i64())
        .ok_or("policy 响应缺 timestamp")?;
    let policy = get_field(biz, "policy").and_then(|t| t.as_i64()).unwrap_or(0);
    let rule = get_field(biz, "runRuleModel").cloned().unwrap_or(Value::Null);

    let min_distance = rule
        .get("minDistance")
        .and_then(|t| t.as_i64())
        .unwrap_or(1000);
    let valid_time = rule.get("validTime").and_then(|t| t.as_i64()).unwrap_or(0);
    // 配速区间：speedTop/speedBottom 的单位是「分/公里」。
    // 个别策略把它们放成秒或 m/s，按数量级判断：正常的分钟配速在 2~20 之间。
    let pace_range = parse_pace_range(&rule);

    Ok(PolicyInfo {
        timestamp,
        policy,
        min_distance,
        valid_time,
        must_points: extract_must_points(biz),
        pace_range,
    })
}

/// 从规则里取配速区间，统一换算成「分/公里」。
///
/// 单位有三种可能：分/公里（常见，2~20）、秒/公里（120~1200）、m/s（1~10）。
/// 按数值量级判定，避免把 6.0 分/km 当成 6 m/s 用。
fn parse_pace_range(rule: &Value) -> Option<(f64, f64)> {
    let top = rule.get("speedTop").and_then(num_of);
    let bottom = rule.get("speedBottom").and_then(num_of);
    let (a, b) = (top?, bottom?);
    if a <= 0.0 || b <= 0.0 {
        return None;
    }
    let to_min_per_km = |v: f64| -> f64 {
        if v < 25.0 {
            // 分/公里 或 m/s：4.0 分/km ≈ 4.17 m/s，两者数量级接近，
            // 但配速不会低于 2 分/km（=30 km/h），所以 <25 一律当分/公里
            v
        } else if v < 1500.0 {
            // 秒/公里
            v / 60.0
        } else {
            // m/s → 分/公里
            1000.0 / v / 60.0
        }
    };
    Some((to_min_per_km(a), to_min_per_km(b)))
}

fn num_of(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_i64().map(|x| x as f64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// 从响应里防御式提取必经点。
///
/// 字段名不确定，逐个试。坐标优先 `lat/lon`（BD-09），缺失或全 0 时回退
/// `glat/glon`（GCJ-02）——两端点都实测出现过「只填 GCJ、BD 置 0」的情况。
pub fn extract_must_points(v: &Value) -> Vec<(f64, f64)> {
    let data = v.get("data").unwrap_or(v);
    for name in [
        "pointList",
        "runPointList",
        "mustPointList",
        "passPointList",
        "mustPoints",
        "passPoints",
        "runPoints",
        "points",
        "nodeList",
        "checkPointList",
    ] {
        let Some(arr) = data.get(name).and_then(|x| x.as_array()) else {
            continue;
        };
        let pts: Vec<(f64, f64)> = arr.iter().filter_map(point_xy).collect();
        if !pts.is_empty() {
            return pts;
        }
    }
    Vec::new()
}

/// 从响应里取一个点的 BD-09 坐标。
/// 取点位的 GCJ-02 坐标。
///
/// 服务端点位同时给了 BD-09（lat/lon）和 GCJ-02（glat/glon）。高德地图
/// 用的是 GCJ-02 网格，前端要按这套坐标摆放才和瓦片对齐，所以单独取一份。
pub fn point_gcj(p: &Value) -> Option<(f64, f64)> {
    let (ga, go) = (field_num(p, &["glat"]), field_num(p, &["glon"]));
    match (ga, go) {
        (Some(a), Some(o)) if !(a == 0.0 && o == 0.0) => Some((a, o)),
        _ => {
            // 没有 GCJ 就从 BD 反推，保证前端永远拿得到一套能用的坐标
            let bd = point_xy(p)?;
            Some(super::coord::bd09_to_gcj02(bd.0, bd.1))
        }
    }
}

pub fn point_xy(p: &Value) -> Option<(f64, f64)> {
    let lat = field_num(p, &["lat", "latitude"]);
    let lon = field_num(p, &["lon", "lng", "longitude"]);
    match (lat, lon) {
        (Some(a), Some(o)) if !(a == 0.0 && o == 0.0) => Some((a, o)),
        _ => {
            let (ga, go) = (field_num(p, &["glat"]), field_num(p, &["glon"]));
            match (ga, go) {
                (Some(a), Some(o)) if !(a == 0.0 && o == 0.0) => Some(gcj02_to_bd09(a, o)),
                _ => None,
            }
        }
    }
}

fn field_num(p: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| p.get(*k).and_then(|v| v.as_f64()))
}

/// 拉策略。
///
/// body：`{"runMode":1,"ruleUpdateTime":0,"geoFenceUpdateTime":0,"selectUnid":<unid>,"operateType":0}`
pub fn fetch_policy_for(client: &Client, run_mode: i64) -> Result<PolicyInfo, String> {
    let select_unid = client
        .credentials()
        .and_then(|c| c.unid.as_ref())
        .and_then(|u| u.parse::<i64>().ok())
        .unwrap_or(0);

    let body = json!({
        "runMode": run_mode,
        "ruleUpdateTime": 0,
        "geoFenceUpdateTime": 0,
        "selectUnid": select_unid,
        "operateType": 0,
    });

    let raw = client.request(
        "POST",
        &format!("{}{}", hosts::RUN, POLICY_PATH),
        Some(&body),
    )?;
    let v = expect_ok(&raw)?;
    // 策略响应里可能有点位判定相关的字段，我们只解析了四个。
    // 把原始 JSON 落盘，逐字段比对 runMode=1 与 runMode=2 的差异。
    if let Ok(dir) = std::env::var("PACE_DUMP_POLICY") {
        let path = format!("{dir}/policy-runmode{run_mode}.json");
        let _ = std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap_or_default());
    }
    parse_policy_response(&v)
}

/// 拿不到策略时的兜底值。
///
/// `runModePolicy` 会按**服务器当前时间**判时段，记分跑一旦不在窗口内
/// 就直接回 11016，连策略串都拿不到。但提交报文里只需要 `policy` 这个
/// 字符串（实测恒为 "0"）外加 `minDistance` / `validTime` 两项阈值，
/// `timestamp` 根本没进签名——所以拉不到策略不代表不能提交。
///
/// 这里给一份保守默认值：minDistance 用服务端最常见的 1500，validTime
/// 取户外跑实测的 06:00~21:30 窗口长度。`must_points` 留空，因为点位
/// 走的是独立的 `/api/points` 接口，不依赖策略。
pub fn fallback_policy() -> PolicyInfo {
    PolicyInfo {
        timestamp: crate::crypto::envelope::now_ms() as i64,
        policy: 0,
        min_distance: 1500,
        valid_time: 55_800,
        must_points: Vec::new(),
        pace_range: None,
    }
}

/// 默认按校园跑拉策略。
pub fn fetch_policy(client: &Client) -> Result<PolicyInfo, String> {
    fetch_policy_for(client, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_and_flat_shapes() {
        // 嵌套在 data 里
        let nested = json!({
            "data": {
                "timestamp": 1_700_000_000_123i64,
                "policy": 1,
                "runRuleModel": { "minDistance": 2000, "validTime": 86400 }
            }
        });
        let p = parse_policy_response(&nested).unwrap();
        assert_eq!(p.timestamp, 1_700_000_000_123);
        assert_eq!(p.policy, 1);
        assert_eq!(p.min_distance, 2000);
        assert_eq!(p.valid_time, 86400);

        // 平铺
        let flat = json!({ "timestamp": 42, "policy": 3 });
        let p2 = parse_policy_response(&flat).unwrap();
        assert_eq!(p2.timestamp, 42);
        assert_eq!(p2.min_distance, 1000, "缺字段时用默认值");
    }

    #[test]
    fn extracts_must_points_from_various_keys() {
        let v = json!({
            "data": {
                "pointList": [
                    {"lat": 30.5, "lon": 104.0},
                    {"lat": 30.6, "lon": 104.1}
                ]
            }
        });
        let pts = extract_must_points(&v);
        assert_eq!(pts.len(), 2);
        assert!((pts[0].0 - 30.5).abs() < 1e-9);
    }

    #[test]
    fn falls_back_to_gcj_when_bd_is_zero() {
        let v = json!({
            "data": {
                "pointList": [
                    {"lat": 0, "lon": 0, "glat": 30.5, "glon": 104.0}
                ]
            }
        });
        let pts = extract_must_points(&v);
        assert_eq!(pts.len(), 1);
        // 转换后应该偏离原值（BD 相对 GCJ 有偏移）
        assert!((pts[0].0 - 30.5).abs() > 1e-6);
    }

    #[test]
    fn missing_timestamp_is_error() {
        assert!(parse_policy_response(&json!({"policy": 1})).is_err());
    }
}

#[cfg(test)]
mod pace_range_tests {
    use super::*;

    fn rule(top: Value, bottom: Value) -> Value {
        serde_json::json!({"speedTop": top, "speedBottom": bottom})
    }

    #[test]
    fn parses_minutes_per_km() {
        // 学生账号常见写法：3~10 分/公里
        let r = parse_pace_range(&rule(serde_json::json!(3.0), serde_json::json!(10.0)));
        assert_eq!(r, Some((3.0, 10.0)));
    }

    #[test]
    fn parses_seconds_per_km() {
        // 180 秒 = 3 分，600 秒 = 10 分
        let r = parse_pace_range(&rule(serde_json::json!(180), serde_json::json!(600)));
        assert_eq!(r, Some((3.0, 10.0)));
    }

    #[test]
    fn accepts_string_numbers() {
        let r = parse_pace_range(&rule(
            serde_json::json!("4.5"),
            serde_json::json!("9.0"),
        ));
        assert_eq!(r, Some((4.5, 9.0)));
    }

    #[test]
    fn missing_or_zero_is_none() {
        assert_eq!(parse_pace_range(&rule(Value::Null, Value::Null)), None);
        assert_eq!(
            parse_pace_range(&rule(serde_json::json!(0), serde_json::json!(10))),
            None
        );
        assert_eq!(parse_pace_range(&serde_json::json!({})), None);
    }

    /// 配速区间必须驱动 preflight 的 PACE_OUT 检查——之前它是 None，永远不跑。
    #[test]
    fn pace_out_of_range_is_fatal() {
        // 必须给真实轨迹点：points 为空时 inspect 会提前 return，
        // 后面的配速检查根本不跑（这也是它一直「通过」的原因之一）。
        let pts: Vec<crate::track::TrackPoint> = (0..200)
            .map(|i| crate::track::TrackPoint {
                lat: 31.2304 + i as f64 * 10.0 / 111_320.0,
                lon: 121.4737,
                time_ms: 1_790_000_000_000 + i as i64 * 4000,
            })
            .collect();
        // 2000m / 800s = 2.5 m/s = 6.67 分/km
        let mut c = crate::track::Check {
            points: &pts,
            sim: &crate::track::SimResult {
                speeds: vec![],
                cadences: vec![],
                strides: vec![],
                total_steps: 1600,
                avg_cadence: 160.0,
                speed_buckets: vec![],
                step_buckets: vec![],
                actual_length_m: 2000.0,
            },
            params: &crate::track::SimParams::default(),
            checkpoints: &[],
            total_distance_m: 2000,
            total_time_sec: 800,
            total_steps: 1600,
            start_time_ms: 1_790_000_000_000,
            stop_time_ms: 1_790_000_800_000,
            pace_range: Some((3.0, 5.0)),
        };
        let r = crate::track::inspect(&c);
        let has = r
            .issues
            .iter()
            .any(|i| i.code == "PACE_OUT");
        assert!(has, "6.67 分/km 超出 3~5 区间，必须报 PACE_OUT");

        c.pace_range = Some((6.0, 8.0));
        let r2 = crate::track::inspect(&c);
        assert!(
            !r2.issues.iter().any(|i| i.code == "PACE_OUT"),
            "区间放宽到 6~8 后不该再报"
        );
    }
}
