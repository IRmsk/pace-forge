//! 电子围栏：`POST /api/v1/getGeoFenceForRun`。
//!
//! 返回 `data.geoFences[]`，每个围栏是一圈 BD-09 坐标点。学校通常返回多个
//! 围栏（多校区 / 校内分区）。
//!
//! **踩过的坑**：早期实现把所有围栏取并集去裁剪路网，却拿第一个围栏的 id 当
//! `runAreaId`。结果路网横跨两个校区，提交的区域 id 又不是锚点所在的那个，
//! 规划出来的路线整体落在错误校区——打卡点全都 miss。
//!
//! 正确做法：先判断锚点落在哪个围栏里，只用命中的围栏。

use serde_json::{json, Value};

use super::client::{expect_ok, Client};
use super::coord::bd09_to_gcj02;
use super::policy::point_xy;
use crate::crypto::consts::hosts;

/// 围栏接口路径。
pub const FENCE_PATH: &str = "/api/v1/getGeoFenceForRun";

/// 单个围栏。
#[derive(Debug, Clone)]
pub struct Fence {
    /// 服务端 id，`runAreaId` 通常取这个值。
    pub id: Option<i64>,
    pub name: String,
    /// 闭合环，(lat, lon)，BD-09。
    pub ring: Vec<(f64, f64)>,
}

/// 按锚点选中校区后的结果。
#[derive(Debug, Clone)]
pub struct SelectedFence {
    /// 裁剪路网用的多边形（重叠围栏合并，避免把路网裁成互不相连的碎片）。
    pub polygons: Vec<Vec<(f64, f64)>>,
    /// 用于 `fixed_point_json` / OBS 的 `runAreaId`。
    pub run_area_id: i64,
    pub fence_id: Option<i64>,
    pub name: String,
    /// 命中的围栏在原始列表里的下标。
    pub index: usize,
}

/// 选择结果 + 兜底信息。
#[derive(Debug, Clone)]
pub struct FencePick {
    pub selected: SelectedFence,
    /// 锚点不在任何围栏内，按最近围栏兜底。
    pub fallback: bool,
    /// 锚点到所选围栏的距离（米），仅 `fallback` 时有意义。
    pub distance_m: f64,
}

/// 拉取围栏列表。
pub fn fetch_fences(client: &Client) -> Result<Vec<Fence>, String> {
    let raw = client.request(
        "POST",
        &format!("{}{}", hosts::RUN, FENCE_PATH),
        Some(&json!({ "geoFenceUpdateTime": 0 })),
    )?;
    let v = expect_ok(&raw)?;
    parse_fences(&v)
}

/// 解析围栏响应。
pub fn parse_fences(v: &Value) -> Result<Vec<Fence>, String> {
    let data = v.get("data").unwrap_or(v);
    let fences = data
        .get("geoFences")
        .cloned()
        .or_else(|| v.get("geoFences").cloned())
        .and_then(|value| match value {
            Value::Array(items) => Some(items),
            // 有些接口把数组序列化成字符串返回
            Value::String(text) => serde_json::from_str(&text).ok(),
            _ => None,
        })
        .unwrap_or_default();

    let mut out = Vec::new();
    for f in &fences {
        let Some(pts) = f.get("points").and_then(|x| x.as_array()) else {
            continue;
        };
        let ring: Vec<(f64, f64)> = pts
            .iter()
            .filter_map(point_xy)
            .filter(|(lat, lon)| !(*lat == 0.0 && *lon == 0.0))
            .collect();
        if ring.len() >= 3 {
            out.push(Fence {
                id: f
                    .get("id")
                    .and_then(value_as_i64)
                    .or_else(|| f.get("runAreaId").and_then(value_as_i64))
                    .or_else(|| f.get("areaId").and_then(value_as_i64)),
                name: f.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                ring,
            });
        }
    }
    Ok(out)
}

fn value_as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

/// 按锚点选中校区。
///
/// - 锚点落在围栏内 → 用命中的围栏（多个重叠时合并几何），`runAreaId` 取第一个有 id 的
/// - 锚点不在任何围栏内 → 取最近的，`fallback = true`，调用方应提示核对定位
pub fn select_by_anchor(
    fences: &[Fence],
    anchor: (f64, f64),
    service_area_id: i64,
) -> Result<FencePick, String> {
    if fences.is_empty() {
        return Err("没有可用围栏".into());
    }
    let containing: Vec<usize> = fences
        .iter()
        .enumerate()
        .filter(|(_, f)| ring_contains(&f.ring, anchor))
        .map(|(i, _)| i)
        .collect();

    if let Some(&first) = containing.first() {
        let polygons = containing.iter().map(|&i| fences[i].ring.clone()).collect();
        let run_area_id = containing
            .iter()
            .filter_map(|&i| fences[i].id)
            .next()
            .unwrap_or(service_area_id);
        return Ok(FencePick {
            selected: SelectedFence {
                polygons,
                run_area_id,
                fence_id: fences[first].id,
                name: fences[first].name.clone(),
                index: first,
            },
            fallback: false,
            distance_m: 0.0,
        });
    }

    let (index, distance_m) = fences
        .iter()
        .enumerate()
        .map(|(i, f)| (i, ring_min_distance(&f.ring, anchor)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .ok_or("没有可用围栏")?;
    let fence = &fences[index];
    Ok(FencePick {
        selected: SelectedFence {
            polygons: vec![fence.ring.clone()],
            run_area_id: fence.id.unwrap_or(service_area_id),
            fence_id: fence.id,
            name: fence.name.clone(),
            index,
        },
        fallback: true,
        distance_m,
    })
}

/// 从裸多边形（无 id）里按锚点挑校区。锚点不在任何围栏内时返回 `None`。
pub fn pick_from_polygons(
    polygons: &[Vec<(f64, f64)>],
    anchor: (f64, f64),
) -> Option<SelectedFence> {
    let fences: Vec<Fence> = polygons
        .iter()
        .map(|ring| Fence {
            id: None,
            name: String::new(),
            ring: ring.clone(),
        })
        .collect();
    let pick = select_by_anchor(&fences, anchor, -1).ok()?;
    (!pick.fallback).then_some(pick.selected)
}

/// 围栏环 → `geoFencesJson` 形状（提交报文里回显绿色边界用）。
pub fn canonical_json_for(ring: &[(f64, f64)]) -> String {
    let points: Vec<Value> = ring
        .iter()
        .map(|&(lat, lon)| {
            let (glat, glon) = bd09_to_gcj02(lat, lon);
            json!({ "lat": lat, "lng": lon, "glat": glat, "glng": glon })
        })
        .collect();
    json!([{ "points": points }]).to_string()
}

/// 点是否在环内（射线法）。
fn ring_contains(ring: &[(f64, f64)], p: (f64, f64)) -> bool {
    if ring.len() < 3 {
        return false;
    }
    let (mut inside, mut j) = (false, ring.len() - 1);
    for i in 0..ring.len() {
        let (yi, xi) = ring[i];
        let (yj, xj) = ring[j];
        // 注意：环里存的是 (lat, lon)，射线法按 (x=lon, y=lat)
        if ((yi > p.0) != (yj > p.0))
            && (p.1 < (xj - xi) * (p.0 - yi) / (yj - yi) + xi)
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// 点到环的最小距离（米，局部平面近似）。
fn ring_min_distance(ring: &[(f64, f64)], p: (f64, f64)) -> f64 {
    const LAT_M: f64 = 111_132.0;
    let lng_m = LAT_M * p.0.to_radians().cos();
    let pt = [p.1 * lng_m, p.0 * LAT_M];
    let mut best = f64::INFINITY;
    for pair in ring.iter().zip(ring.iter().cycle().skip(1)) {
        let a = [pair.0 .1 * lng_m, pair.0 .0 * LAT_M];
        let b = [pair.1 .1 * lng_m, pair.1 .0 * LAT_M];
        // 点到线段距离
        let ab = [b[0] - a[0], b[1] - a[1]];
        let ap = [pt[0] - a[0], pt[1] - a[1]];
        let len2 = ab[0] * ab[0] + ab[1] * ab[1];
        let t = if len2 > 1e-12 {
            ((ap[0] * ab[0] + ap[1] * ab[1]) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let foot = [a[0] + ab[0] * t, a[1] + ab[1] * t];
        best = best.min((pt[0] - foot[0]).hypot(pt[1] - foot[1]));
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(lat0: f64, lon0: f64, d: f64) -> Vec<(f64, f64)> {
        vec![
            (lat0, lon0),
            (lat0, lon0 + d),
            (lat0 + d, lon0 + d),
            (lat0 + d, lon0),
        ]
    }

    fn two_campuses() -> Vec<Fence> {
        vec![
            Fence {
                id: Some(1774),
                name: "东校区".into(),
                ring: square(39.90, 116.40, 0.01),
            },
            Fence {
                id: Some(1775),
                name: "西校区".into(),
                ring: square(39.90, 116.30, 0.01),
            },
        ]
    }

    #[test]
    fn picks_fence_containing_anchor() {
        let fences = two_campuses();
        // 锚点落在西校区
        let pick = select_by_anchor(&fences, (39.905, 116.305), 0).unwrap();
        assert!(!pick.fallback);
        assert_eq!(pick.selected.run_area_id, 1775, "应选中西校区 id");
        assert_eq!(pick.selected.name, "西校区");
        assert_eq!(pick.selected.polygons.len(), 1);
    }

    #[test]
    fn overlapping_fences_are_merged() {
        let mut fences = two_campuses();
        // 加一个与东校区重叠的围栏
        fences.push(Fence {
            id: Some(1800),
            name: "东区扩展".into(),
            ring: square(39.905, 116.405, 0.01),
        });
        let pick = select_by_anchor(&fences, (39.907, 116.407), 0).unwrap();
        assert!(!pick.fallback);
        assert_eq!(pick.selected.polygons.len(), 2, "重叠围栏应合并几何");
    }

    #[test]
    fn falls_back_to_nearest_when_outside() {
        let fences = two_campuses();
        let pick = select_by_anchor(&fences, (39.95, 116.305), 0).unwrap();
        assert!(pick.fallback, "锚点在围栏外应触发兜底");
        assert_eq!(pick.selected.run_area_id, 1775, "应选最近的西校区");
        assert!(pick.distance_m > 0.0);
    }

    #[test]
    fn empty_fences_is_error() {
        assert!(select_by_anchor(&[], (39.9, 116.4), 0).is_err());
    }

    #[test]
    fn parses_fences_from_response() {
        let v = json!({
            "data": {
                "geoFences": [{
                    "id": 1774,
                    "name": "主校区",
                    "points": [
                        {"lat": 30.5, "lon": 104.0},
                        {"lat": 30.5, "lon": 104.01},
                        {"lat": 30.51, "lon": 104.01}
                    ]
                }]
            }
        });
        let fences = parse_fences(&v).unwrap();
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].id, Some(1774));
        assert_eq!(fences[0].ring.len(), 3);
    }

    #[test]
    fn skips_degenerate_rings() {
        let v = json!({
            "geoFences": [{
                "id": 1,
                "points": [{"lat": 30.5, "lon": 104.0}, {"lat": 30.5, "lon": 104.01}]
            }]
        });
        // 只有 2 个点，构不成环
        assert!(parse_fences(&v).unwrap().is_empty());
    }

    #[test]
    fn canonical_json_has_gl_fields() {
        let ring = square(30.5, 104.0, 0.001);
        let s = canonical_json_for(&ring);
        let v: Value = serde_json::from_str(&s).unwrap();
        let pts = v[0]["points"].as_array().unwrap();
        assert_eq!(pts.len(), 4);
        assert!(pts[0].get("glat").is_some());
        assert!(pts[0].get("glng").is_some());
    }

    #[test]
    fn point_in_polygon_basic() {
        let ring = square(0.0, 0.0, 10.0);
        assert!(ring_contains(&ring, (5.0, 5.0)));
        assert!(!ring_contains(&ring, (15.0, 5.0)));
        assert!(!ring_contains(&ring, (-1.0, 5.0)));
    }
}
