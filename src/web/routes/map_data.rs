//! 地图数据路由：打卡点、围栏、blockers。

use serde_json::{Value, json};

use crate::web::http::{json_err, json_ok, query_param};
use crate::web::state::{Shared, lock_state};

// ---------- 围栏列表（地图上画出来） ----------

/// 只查围栏 + 打卡点，不生成轨迹。
///
/// 以前围栏和打卡点**只**出现在 `/api/plan` 的响应里，也就是说用户得先
/// 画一条线、跑完一次规划才能看见自己学校的电子围栏和必到点在哪儿——
/// issue #3 里「得先在手绘图上生成轨迹之后才能看到点位」说的就是这个。
/// 查点位本身是一次只读的 GET，没理由绑在规划上。
///
/// 锚点优先级：query 的 `lat`/`lng` > 围栏质心。两边都没有就没法查
/// （服务端必须知道你在哪个校区），明确报错而不是猜一个坐标。
pub(crate) fn handle_points(
    request: tiny_http::Request,
    state: Shared,
    query: &str,
) -> Result<(), String> {
    let client = {
        let st = lock_state(&state);
        if !st.logged_in() {
            return json_err(request, "请先登录");
        }
        let Some(c) = st.client() else {
            return json_err(request, "当前账号没有可用会话，请重新登录");
        };
        c.clone()
    };

    // 围栏：能拿到就一起返回（画图要它），拿不到也不阻断查点位
    let fences = crate::api::fence::fetch_fences(&client).unwrap_or_default();
    let first_ring = |f: &crate::api::fence::Fence| -> Vec<Value> {
        f.ring
            .iter()
            .map(|&(la, lo)| {
                let (gl, go) = crate::api::coord::bd09_to_gcj02(la, lo);
                json!([gl, go])
            })
            .collect()
    };
    let fence_json = fences.first().map(|f| {
        json!({
            "id": f.id,
            "name": f.name,
            "ringGcj": first_ring(f),
            "ring": f.ring,
        })
    });

    // 锚点：query 优先，其次围栏质心
    let lat = query_param(query, "lat").and_then(|v| v.parse::<f64>().ok());
    let lng = query_param(query, "lng").and_then(|v| v.parse::<f64>().ok());
    let anchor = match (lat, lng) {
        (Some(a), Some(b)) if !(a == 0.0 && b == 0.0) => {
            // query 给的是底图坐标（GCJ-02），服务端要 BD-09
            let (ba, bo) = crate::api::coord::gcj02_to_bd09(a, b);
            (ba, bo, "query".to_string())
        }
        _ => {
            let Some(f) = fences.iter().find(|f| f.ring.len() >= 3) else {
                return json_err(
                    request,
                    "拿不到校区围栏，也没有给锚点：请在「纬度/经度」里填学校坐标再查",
                );
            };
            let n = f.ring.len() as f64;
            let a = f.ring.iter().map(|p| p.0).sum::<f64>() / n;
            let b = f.ring.iter().map(|p| p.1).sum::<f64>() / n;
            (a, b, format!("围栏「{}」质心", f.name))
        }
    };
    let distance_m: i64 = query_param(query, "distance")
        .and_then(|v| v.parse::<f64>().ok())
        .map(|km| (km * 1000.0).round() as i64)
        .unwrap_or(2000);

    match crate::api::points::fetch_points_at_distance(
        &client, anchor.0, anchor.1, None, distance_m,
    ) {
        Ok(ctx) => {
            let points: Vec<Value> = ctx
                .coords
                .iter()
                .enumerate()
                .map(|(i, &(a, b))| {
                    let (ga, go) = ctx.gcj_coords.get(i).copied().unwrap_or((a, b));
                    let raw = ctx.points.get(i);
                    json!({
                        "index": i + 1,
                        "lat": a,
                        "lng": b,
                        "gcjLat": ga,
                        "gcjLng": go,
                        "name": raw
                            .and_then(|r| r.get("pointName"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                        "isFixed": raw
                            .and_then(|r| r.get("isFixed"))
                            .and_then(|v| v.as_i64())
                            .unwrap_or(-1),
                    })
                })
                .collect();
            json_ok(
                request,
                json!({
                    "count": points.len(),
                    "points": points,
                    "fence": fence_json,
                    "anchorFrom": anchor.2,
                    "distance": distance_m,
                }),
            )
        }
        Err(e) => json_err(request, &format!("拉点位失败: {e}")),
    }
}

pub(crate) fn handle_fences(
    request: tiny_http::Request,
    state: Shared,
    _query: &str,
) -> Result<(), String> {
    // 先把 client 克隆出来再放锁。Client 是 Clone，网络请求几十秒，
    // 持着 Mutex 做 IO 会让 /api/accounts 之类的请求全部排队，
    // 页面看起来就是「服务假死」。
    let client = {
        let st = lock_state(&state);
        if !st.logged_in() {
            return json_err(request, "请先登录");
        }
        let Some(c) = st.client() else {
            return json_err(request, "当前账号没有可用会话，请重新登录");
        };
        c.clone()
    };
    match crate::api::fence::fetch_fences(&client) {
        Ok(fs) => {
            let list: Vec<Value> = fs
                .iter()
                .map(|f| {
                    // 同上：`ring` 是 BD-09，底图吃 GCJ-02。两个都给，
                    // 前端优先用 `ringGcj`（老前端读 `ring` 也不会崩）。
                    let ring_gcj: Vec<Value> = f
                        .ring
                        .iter()
                        .map(|&(la, lo)| {
                            let (gl, go) = crate::api::coord::bd09_to_gcj02(la, lo);
                            json!([gl, go])
                        })
                        .collect();
                    json!({
                        "id": f.id,
                        "name": f.name,
                        "ring": f.ring,
                        "ringGcj": ring_gcj,
                    })
                })
                .collect();
            json_ok(request, json!({"count": list.len(), "fences": list}))
        }
        Err(e) => json_err(request, &format!("拉围栏失败: {e}")),
    }
}

pub(crate) fn handle_blockers_get(request: tiny_http::Request) -> Result<(), String> {
    let polys = crate::track::roadnet::load_local_blockers();
    json_ok(
        request,
        json!({
            "count": polys.len(),
            "polys": polys
                .iter()
                .map(|p| p.iter().map(|q| vec![q.0, q.1]).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        }),
    )
}

/// 保存本地手绘禁区。body: {"polys":[[[lat,lng],...], ...]}
///
/// 用法：OSM 在国内校园没有建筑轮廓，规划出来的轨迹会从楼里穿过去。
/// 在地图上把楼圈成矩形，存一份，之后所有规划都会绕开。
pub(crate) fn handle_blockers_put(request: tiny_http::Request, body: &Value) -> Result<(), String> {
    let Some(arr) = body.get("polys").and_then(|v| v.as_array()) else {
        return json_err(request, "polys 必须是数组");
    };
    if arr.len() > 200 {
        return json_err(request, "禁区最多 200 块");
    }
    let mut polys: Vec<Vec<(f64, f64)>> = Vec::new();
    for poly in arr {
        let Some(pts) = poly.as_array() else {
            return json_err(request, "每块禁区要是 [[lat,lng], ...]");
        };
        let pts: Vec<(f64, f64)> = pts
            .iter()
            .filter_map(|p| {
                let a = p.as_array()?;
                Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
            })
            .collect();
        if pts.len() < 3 {
            return json_err(request, "每块禁区至少要 3 个点");
        }
        for p in &pts {
            if !(-90.0..=90.0).contains(&p.0) || !(-180.0..=180.0).contains(&p.1) {
                return json_err(request, "坐标超出经纬度范围");
            }
        }
        polys.push(pts);
    }
    match crate::track::roadnet::save_local_blockers(&polys) {
        Ok(()) => json_ok(request, json!({"saved": polys.len()})),
        Err(e) => json_err(request, &e),
    }
}
