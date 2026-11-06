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
    // 规则里也可能出现配速区间，一并带出来
    let _ = rule.get("speedTop");
    let _ = rule.get("speedBottom");

    Ok(PolicyInfo {
        timestamp,
        policy,
        min_distance,
        valid_time,
        must_points: extract_must_points(biz),
    })
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
    parse_policy_response(&v)
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
