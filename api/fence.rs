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

/// 围栏里下发的必经打卡点。
///
/// 有的学校**只**在围栏响应里下发必经点，点位接口返回空数组——这时如果
/// 只看点位接口，就会得到「这个学校没有打卡点」的错误结论。字段与服务端
/// 原始 `passPoints[]` 对齐，提交时要原样回显。
#[derive(Debug, Clone, PartialEq)]
pub struct PassPoint {
    /// BD-09。
    pub lat: f64,
    pub lon: f64,
    /// GCJ-02（有的响应只给这一套）。
    pub glat: f64,
    pub glon: f64,
    pub is_fixed: i64,
    pub point_name: String,
}

/// 单个围栏。
#[derive(Debug, Clone)]
pub struct Fence {
    /// 服务端 id，`runAreaId` 通常取这个值。
    pub id: Option<i64>,
    pub name: String,
    /// 闭合环，(lat, lon)，BD-09。
    pub ring: Vec<(f64, f64)>,
    /// 围栏下发的必经点（可能为空）。
    pub pass_points: Vec<PassPoint>,
}

/// 解析 `passPoints[]`。
///
/// 宽容处理：坐标可能在 `lat/lon`（BD-09）或 `glat/glon`（GCJ-02），也可能
/// 两者都有但其中一套是 0。只有两套都拿不到的条目才丢弃。
pub fn parse_pass_points(v: &Value) -> Vec<PassPoint> {
    let arr = match v.get("passPoints") {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or_default(),
        _ => return Vec::new(),
    };
    arr.iter()
        .filter_map(|p| {
            let num = |k: &str| p.get(k).and_then(|x| x.as_f64());
            let (mut lat, mut lon) = (num("lat").unwrap_or(0.0), num("lon").unwrap_or(0.0));
            let (glat, glon) = (num("glat").unwrap_or(0.0), num("glon").unwrap_or(0.0));
            // 只有 GCJ 坐标时反推 BD-09，交给 coord 统一处理
            if (lat == 0.0 || lon == 0.0) && glat != 0.0 && glon != 0.0 {
                let (a, b) = super::coord::gcj02_to_bd09(glat, glon);
                lat = a;
                lon = b;
            }
            if lat == 0.0 && lon == 0.0 {
                return None;
            }
            Some(PassPoint {
                lat,
                lon,
                glat,
                glon,
                is_fixed: p.get("isFixed").and_then(value_as_i64).unwrap_or(1),
                point_name: p
                    .get("pointName")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

/// `pass_point_items` 的返回：原始对象 + BD-09 坐标 + GCJ-02 坐标。
pub type PassPointItems = (Vec<Value>, Vec<(f64, f64)>, Vec<(f64, f64)>);

/// 把必经点转成点位接口同形状的原始对象 + 两套坐标。
///
/// 返回 `(items, bd_coords, gcj_coords)`：`items` 可直接进 `fixed_point_json`，
/// 两套坐标给规划器用（路网是 WGS84→BD09，展示层是 GCJ-02）。
pub fn pass_point_items(points: &[PassPoint]) -> PassPointItems {
    let mut items = Vec::with_capacity(points.len());
    let mut bd = Vec::with_capacity(points.len());
    let mut gcj = Vec::with_capacity(points.len());
    for p in points {
        let (glat, glon) = if p.glat != 0.0 && p.glon != 0.0 {
            (p.glat, p.glon)
        } else {
            super::coord::bd09_to_gcj02(p.lat, p.lon)
        };
        items.push(json!({
            "lat": p.lat,
            "lon": p.lon,
            "glat": glat,
            "glon": glon,
            "isFixed": p.is_fixed,
            "pointName": p.point_name,
            "position": -1,
        }));
        bd.push((p.lat, p.lon));
        gcj.push((glat, glon));
    }
    (items, bd, gcj)
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
    /// 选中围栏下发的必经点（点位接口为空时的唯一来源）。
    pub pass_points: Vec<PassPoint>,
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
///
/// 服务端返回了围栏但解析不出边界时，这里返回 `Err` 并在消息里带上
/// 诊断——「没返回围栏」和「返回了但不成环」对用户是两种完全不同的
/// 处境，而前者那句提示只会让人反复重试。
pub fn fetch_fences(client: &Client) -> Result<Vec<Fence>, String> {
    let raw = client.request(
        "POST",
        &format!("{}{}", hosts::RUN, FENCE_PATH),
        Some(&json!({ "geoFenceUpdateTime": 0 })),
    )?;
    let v = expect_ok(&raw)?;
    let fences = parse_fences(&v)?;
    if fences.is_empty() {
        return Err(format!("服务端下发的围栏不成边界（{}）", fence_diag(&v)));
    }
    Ok(fences)
}

/// 围栏响应诊断：说明到底返回了什么、为什么解析不出环。
///
/// 只用于错误信息，不参与业务逻辑。字段名一并列出——服务端若把
/// `points` 改名成别的，光看「点数为 0」是查不出来的。
fn fence_diag(v: &Value) -> String {
    let data = v.get("data").unwrap_or(v);
    let raw = data
        .get("geoFences")
        .or_else(|| v.get("geoFences"));

    let Some(raw) = raw else {
        return "响应里没有 geoFences 字段".into();
    };
    let items: Vec<Value> = match raw {
        Value::Array(a) => a.clone(),
        Value::String(s) => serde_json::from_str(s).unwrap_or_default(),
        _ => return "geoFences 既不是数组也不是 JSON 字符串".into(),
    };
    if items.is_empty() {
        return "geoFences 是空数组".into();
    }

    let counts: Vec<String> = items
        .iter()
        .map(|f| {
            f.get("points")
                .and_then(|p| p.as_array())
                .map_or(0, |a| a.len())
                .to_string()
        })
        .collect();
    let keys: Vec<String> = items[0]
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();

    format!(
        "geoFences={}，各围栏点数=[{}]，首个围栏字段=[{}]",
        items.len(),
        counts.join(","),
        keys.join(",")
    )
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
                pass_points: parse_pass_points(f),
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
                // 重叠围栏合并时取所有命中围栏的必经点（去重后按名字稳定排序）
                pass_points: {
                    let mut all: Vec<PassPoint> = containing
                        .iter()
                        .flat_map(|&i| fences[i].pass_points.clone())
                        .collect();
                    all.sort_by(|a, b| {
                        a.point_name
                            .cmp(&b.point_name)
                            .then(a.lat.total_cmp(&b.lat))
                    });
                    all.dedup();
                    all
                },
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
            pass_points: fence.pass_points.clone(),
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
            pass_points: Vec::new(),
        })
        .collect();
    let pick = select_by_anchor(&fences, anchor, -1).ok()?;
    (!pick.fallback).then_some(pick.selected)
}

/// 围栏 → `geoFencesJson` 形状（提交报文里回显这次跑的是哪个围栏）。
///
/// 逐字段对齐真机 rrid 1328288734（`evidence/obs_real_20261002.json`）：
///
/// ```text
/// [{"distance":0.0,"id":1668,"name":"学校",
///   "points":[{"glat":47.382098,"glng":123.952159,"lat":47.388433,"lng":123.958514}, …]}]
/// ```
///
/// 早先只发 `[{"points":[…]}]`，少了 `id` / `name` / `distance` 三个字段。
/// 这三处里 `id` 最要紧：真机报文里 `runAreaId` 是 `-1`，围栏 id **只**
/// 出现在这里——缺了它，服务端就没有「这条记录属于哪个围栏」的凭据。
///
/// `id` 取围栏响应里的原始 id；围栏响应不带 id 时退回 `run_area_id`
/// （`select_by_anchor` 在没有围栏 id 时会用 service_area_id 兜底）。
/// 坐标按真机那样保留 6 位小数（≈0.1m 精度）：BD-09 原样带出，
/// GCJ-02 由 BD-09 现算，两套都是真值——真机的 `points[]` 里没有 0 或 -1。
pub fn geo_fence_json(
    run_area_id: i64,
    fence_id: Option<i64>,
    name: &str,
    ring: &[(f64, f64)],
) -> String {
    let round6 = |v: f64| (v * 1e6).round() / 1e6;
    let points: Vec<Value> = ring
        .iter()
        .map(|&(lat, lon)| {
            let (glat, glon) = bd09_to_gcj02(lat, lon);
            // 键序照真机（字母序）：glat / glng / lat / lng
            json!({
                "glat": round6(glat),
                "glng": round6(glon),
                "lat": round6(lat),
                "lng": round6(lon),
            })
        })
        .collect();
    json!([{
        "distance": 0.0,
        "id": fence_id.unwrap_or(run_area_id),
        "name": name,
        "points": points,
    }])
    .to_string()
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
                pass_points: Vec::new(),
            },
            Fence {
                id: Some(1775),
                name: "西校区".into(),
                ring: square(39.90, 116.30, 0.01),
                pass_points: Vec::new(),
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
            pass_points: Vec::new(),
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
    fn fence_diag_names_the_actual_cause() {
        // 三条互不相同的失败原因，必须能区分开——用户看到哪一句，
        // 决定了他该等一会儿重试还是该来找我。

        // 1. 压根没有这个字段
        let missing = json!({ "code": 0, "data": {} });
        assert!(fence_diag(&missing).contains("没有 geoFences"));

        // 2. 字段在，但是空数组
        let empty = json!({ "data": { "geoFences": [] } });
        assert!(fence_diag(&empty).contains("空数组"));

        // 3. 有围栏但点数不够成环，且要能报出字段真名
        let degenerate = json!({
            "data": {
                "geoFences": [
                    { "id": 7, "points": [{"lat": 47.38, "lon": 123.95}] },
                    { "id": 8, "points": [] }
                ]
            }
        });
        let d = fence_diag(&degenerate);
        assert!(d.contains("geoFences=2"), "应报出围栏个数: {d}");
        assert!(d.contains("[1,0]"), "应报出每个围栏的点数: {d}");
        assert!(d.contains("points"), "应列出字段名: {d}");
    }

    #[test]
    fn fetch_fences_rejects_degenerate_instead_of_empty() {
        // 解析不出环不该安静地返回空数组——上游拿不到区别，
        // 只能给用户一句「没返回围栏」，查不下去。
        let v = json!({ "data": { "geoFences": [{ "id": 1, "points": [] }] } });
        assert!(parse_fences(&v).unwrap().is_empty());
        let diag = fence_diag(&v);
        assert!(diag.contains("geoFences=1"));
    }

    /// 围栏对象必须带真机那三个顶层字段（`distance` / `id` / `name`）。
    ///
    /// 真机 `geoFencesJson` 是
    /// `[{"distance":0.0,"id":1668,"name":"学校","points":[…]}]`；
    /// 我们早先只发 `[{"points":[…]}]`，围栏 id 就整条丢了——而报文里
    /// `runAreaId` 是 -1，围栏归属只剩这一个字段能表达。
    #[test]
    fn geo_fence_json_matches_device_shape() {
        let ring = square(30.5, 104.0, 0.001);
        let s = geo_fence_json(1668, Some(1668), "学校", &ring);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v[0]["id"], json!(1668), "围栏 id 必须是真实围栏 id");
        assert_eq!(v[0]["name"], json!("学校"));
        assert_eq!(v[0]["distance"], json!(0.0));
        let pts = v[0]["points"].as_array().unwrap();
        assert_eq!(pts.len(), 4);
        assert!(pts[0].get("glat").is_some());
        assert!(pts[0].get("glng").is_some());
        // 两套坐标都是真值，且 GCJ-02 与 BD-09 不相等
        let (lat, lng) = (pts[0]["lat"].as_f64().unwrap(), pts[0]["lng"].as_f64().unwrap());
        let (glat, glng) = (pts[0]["glat"].as_f64().unwrap(), pts[0]["glng"].as_f64().unwrap());
        assert!(lat != 0.0 && lng != 0.0 && glat != 0.0 && glng != 0.0);
        assert!((lat - glat).abs() > 1e-4 && (lng - glng).abs() > 1e-4);
        // 6 位小数：与真机样本的精度一致（≈0.1m）
        let frac = format!("{glat}").split('.').nth(1).map(|f| f.len()).unwrap_or(0);
        assert!(frac <= 6, "围栏坐标应保留 6 位小数，实际 {glat}");
        // 围栏响应没有 id 时退回 runAreaId，不能发 null
        let s2 = geo_fence_json(1775, None, "", &ring);
        let v2: Value = serde_json::from_str(&s2).unwrap();
        assert_eq!(v2[0]["id"], json!(1775));
    }

    #[test]
    fn point_in_polygon_basic() {
        let ring = square(0.0, 0.0, 10.0);
        assert!(ring_contains(&ring, (5.0, 5.0)));
        assert!(!ring_contains(&ring, (15.0, 5.0)));
        assert!(!ring_contains(&ring, (-1.0, 5.0)));
    }

    #[test]
    fn parses_pass_points_with_bd09() {
        let v = serde_json::json!({
            "passPoints": [
                {"lat": 30.5, "lon": 104.0, "glat": 30.501, "glon": 104.002,
                 "isFixed": 1, "pointName": "P1"},
                {"lat": 30.6, "lon": 104.1, "isFixed": 0, "pointName": "P2"}
            ]
        });
        let pps = parse_pass_points(&v);
        assert_eq!(pps.len(), 2);
        assert_eq!(pps[0].point_name, "P1");
        assert_eq!(pps[0].is_fixed, 1);
        assert!((pps[0].glat - 30.501).abs() < 1e-9);
        // 第二条没有 glat，输出时要现算
        let (items, bd, gcj) = pass_point_items(&pps);
        assert_eq!(items.len(), 2);
        assert_eq!(bd.len(), 2);
        assert_eq!(gcj.len(), 2);
        assert!((items[1]["glat"].as_f64().unwrap() - pps[1].glat).abs() > 1e-9,
            "缺 GCJ 坐标时必须由 BD-09 反算出来");
    }

    #[test]
    fn pass_points_only_glat_are_recovered() {
        let v = serde_json::json!({
            "passPoints": "[{\"glat\": 30.5, \"glon\": 104.0, \"isFixed\": 1, \"pointName\": \"X\"}]"
        });
        let pps = parse_pass_points(&v);
        assert_eq!(pps.len(), 1, "字符串化数组必须能解析");
        assert!(pps[0].lat != 0.0 && pps[0].lon != 0.0, "只有 GCJ 时要反算出 BD-09");
        assert_eq!(pps[0].point_name, "X");
    }

    #[test]
    fn pass_points_empty_and_garbage() {
        assert!(parse_pass_points(&serde_json::json!({})).is_empty());
        assert!(parse_pass_points(&serde_json::json!({"passPoints": []})).is_empty());
        assert!(parse_pass_points(&serde_json::json!({
            "passPoints": [{"lat": 0, "lon": 0}, {"pointName": "无坐标"}]
        })).is_empty());
    }

    #[test]
    fn fences_carry_pass_points() {
        let v = serde_json::json!({
            "data": {"geoFences": [{
                "id": 1864,
                "name": "主校区",
                "points": [
                    {"lat": 30.50, "lon": 104.00},
                    {"lat": 30.50, "lon": 104.02},
                    {"lat": 30.52, "lon": 104.02},
                    {"lat": 30.52, "lon": 104.00}
                ],
                "passPoints": [{"lat": 30.51, "lon": 104.01, "isFixed": 1, "pointName": "必经"}]
            }]}
        });
        let fs = parse_fences(&v).unwrap();
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].pass_points.len(), 1);
        let pick = select_by_anchor(&fs, (30.51, 104.01), -1).unwrap();
        assert_eq!(pick.selected.pass_points.len(), 1);
        assert_eq!(pick.selected.pass_points[0].point_name, "必经");
    }
}
