//! 跑跑路由：`/api/plan`（预演）与 `/api/run`（提交）。

use serde_json::{Value, json};

use crate::web::http::{json_err, json_ok};
use crate::web::state::{Shared, lock_state};

/// 轨迹预演：本地生成轨迹 + 提交报文，不联网。
///
/// 这一步能验证轨迹几何、分桶、签名是否合规，但**不会**真的提交——
/// 真实提交需要先拿服务端的规则/围栏/打卡点，且要过静默期。
/// 把用户在界面上填的中心点统一转成 BD-09。
///
/// 服务端的围栏、打卡点，以及规划器内部的几何全是 BD-09；前端地图用的
/// 却是高德瓦片（GCJ-02 网格）。所以外部坐标进规划器之前必须先落到
/// BD-09，否则整条轨迹会整体平移 400~600 米，看起来「跑了但点全错」。
/// 解析前端给的终点（可选）。坐标口径与锚点一致（默认 GCJ-02，服务端要 BD-09）。
///
/// 两个字段都没填、或都是 0，就当没指定终点——保持原来的闭环行为。
pub(crate) fn parse_end(body: &Value) -> Option<(f64, f64)> {
    let la = body["endLat"].as_f64()?;
    let ln = body["endLng"].as_f64()?;
    if la == 0.0 && ln == 0.0 {
        return None;
    }
    let cs = body["coord"].as_str().unwrap_or("gcj02");
    input_to_bd09(la, ln, cs).ok()
}

pub(crate) fn input_to_bd09(lat: f64, lng: f64, sys: &str) -> Result<(f64, f64), String> {
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
        return Err(format!("经纬度超出范围: ({lat}, {lng})"));
    }
    if lat.abs() < 1e-9 && lng.abs() < 1e-9 {
        return Err("(0, 0) 不是有效位置；请填校区经纬度，或点「在地图上点选」".into());
    }
    Ok(match sys {
        "wgs84" => crate::api::coord::wgs84_to_bd09(lat, lng),
        "bd09" => (lat, lng),
        // 默认按 GCJ-02：国内能复制到的坐标九成来自高德/腾讯
        _ => crate::api::coord::gcj02_to_bd09(lat, lng),
    })
}

pub(crate) fn handle_plan(
    request: tiny_http::Request,
    state: Shared,
    body: &serde_json::Value,
) -> Result<(), String> {
    let distance_km = body["distance"].as_f64().unwrap_or(2.0);
    let duration_min = body["duration"].as_f64().unwrap_or(12.0);
    // 0 或缺失都表示「没指定」，交给模型按配速推算。
    // 手填步数很容易和配速对不上——12 分钟 2300 步 = 192 步/分，
    // 而 2.78 m/s 的典型步频只有 158，差 34 步会被自检判 GAIT_MISMATCH。
    let steps = body["steps"].as_i64().filter(|v| *v > 0).map(|v| v as i32);
    let sport_type = body["sportType"].as_i64().unwrap_or(1) as i32;
    let run_mode = body["runMode"].as_i64().unwrap_or(1);
    let start_at_ms = body["startAtMs"].as_i64().filter(|v| *v > 0);

    // 中心点：请求里给的坐标 → 围栏环心 → 明确报错。
    //
    // 这里原来兜底 `(30.5, 104.0)`（成都）。校区在东北某平原校区，围栏一旦拉不到
    // 就规划出一条跨省轨迹，用户看到的是「能生成、但点全错」。兜底坐标不该
    // 出现在业务路径上——报错比假轨迹有用。
    //
    // 顺带修掉上一版的问题：原来持着 state 锁调 fetch_fences 做网络请求，
    // 会把 /api/accounts 之类的请求全部堵住，页面看起来像卡死。
    let coord_sys = body["coord"].as_str().unwrap_or("gcj02");
    let (lat, lng) = match (body["lat"].as_f64(), body["lng"].as_f64()) {
        (Some(a), Some(b)) => match input_to_bd09(a, b, coord_sys) {
            Ok(c) => c,
            Err(e) => return json_err(request, &e),
        },
        _ => {
            let client = {
                let st = lock_state(&state);
                st.client().cloned()
            };
            let Some(client) = client else {
                return json_err(
                    request,
                    "未登录时必须在表单里填写校区经纬度，否则不知道往哪儿规划",
                );
            };
            match crate::api::fence::fetch_fences(&client)
                .and_then(|fs| crate::api::fence::center_from_fences(&fs))
            {
                Ok(c) => c,
                Err(e) => return json_err(request, &format!("无法确定校区位置: {e}")),
            }
        }
    };

    if distance_km <= 0.0 || distance_km > 42.0 {
        return json_err(request, "距离需在 0~42 公里之间");
    }
    if duration_min <= 0.0 || duration_min > 120.0 {
        return json_err(request, "时长需在 0~120 分钟之间（超过服务端必然驳回）");
    }

    // ---- 已登录：走真实流程拿围栏和打卡点 ----
    {
        let maybe_client = {
            let st = lock_state(&state);
            if !st.logged_in() {
                None
            } else {
                st.client().cloned()
            }
        };
        if let Some(client) = maybe_client {
            let p = crate::api::flow::RunParams {
                center_lat: lat,
                center_lng: lng,
                distance_m: distance_km * 1000.0,
                duration_sec: (duration_min * 60.0).round() as i64,
                sport_type,
                run_mode,
                free_run: sport_type != 1,
                start_at_ms,
                custom_path: parse_custom_path(body),
                end: parse_end(body),
                alt_range: parse_alt_range(body),
                prefer_stadium: body["mode"].as_str() == Some("stadium"),
                prefer_roads: body["preferRoads"].as_bool().unwrap_or(true),
                ..Default::default()
            };
            let logs: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
                std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let l2 = std::rc::Rc::clone(&logs);
            let mut log = move |s: &str| l2.borrow_mut().push(s.to_string());
            let r = crate::api::flow::dry_run(&client, &p, &mut log);
            drop(log);

            if let Ok(o) = r {
                // 围栏多边形
                let fence = o.fence.as_ref().map(|f| {
                    // `Fence.ring` 是 **BD-09**（`fence::point_xy` 统一转过了），
                    // 而底图是高德瓦片（GCJ-02 网格）。以前直接把 BD-09 的环
                    // 喂给 `L.polygon`，围栏整体偏出去几百米——issue #3 里
                    // 「电子围栏处于偏离状态」就是这个。轨迹点和打卡点当时都
                    // 转过了，唯独围栏漏了。
                    let to_gcj = |ring: &Vec<(f64, f64)>| -> Vec<Value> {
                        ring.iter()
                            .map(|&(la, lo)| {
                                let (gl, go) = crate::api::coord::bd09_to_gcj02(la, lo);
                                json!([gl, go])
                            })
                            .collect()
                    };
                    let ring = f.selected.polygons.first().cloned().unwrap_or_default();
                    json!({
                        "name": f.selected.name,
                        "runAreaId": f.selected.run_area_id,
                        "fallback": f.fallback,
                        "distance": f.distance_m,
                        "ring": ring.clone(),
                        "ringGcj": to_gcj(&ring),
                        "allPolygons": f.selected.polygons,
                        "allPolygonsGcj": f.selected.polygons.iter().map(to_gcj).collect::<Vec<_>>(),
                    })
                });

                // 打卡点 + 是否被轨迹覆盖
                let track_xy: Vec<(f64, f64)> = o.track.iter().map(|t| (t.lat, t.lon)).collect();
                let cps: Vec<Value> = o
                    .points
                    .coords
                    .iter()
                    .enumerate()
                    .map(|(i, &(a, b))| {
                        let nearest = track_xy
                            .iter()
                            .map(|q| crate::track::geo::distance((a, b), *q))
                            .fold(f64::MAX, f64::min);
                        // 高德地图按 GCJ-02 网格切瓦片，前端摆点位必须用这一套，
                        // 否则点位会整体偏出去几百米
                        let (ga, go) = o.points.gcj_coords.get(i).copied().unwrap_or((a, b));
                        json!({
                            "index": i + 1,
                            "lat": a,
                            "lng": b,
                            "gcjLat": ga,
                            "gcjLng": go,
                            "passed": nearest < 15.0,
                            "nearest": (nearest * 10.0).round() / 10.0,
                        })
                    })
                    .collect();

                let passed = cps.iter().filter(|c| c["passed"] == true).count();
                let step = (o.track.len() / 300).max(1);
                let coords: Vec<Value> = o
                    .track
                    .iter()
                    .step_by(step)
                    // 底图是高德（GCJ-02 网格），轨迹点也要转过去才和瓦片对齐
                    .map(|t| {
                        let (gl, go) = crate::api::coord::bd09_to_gcj02(t.lat, t.lon);
                        json!([gl, go])
                    })
                    .collect();

                let ac = {
                    let c: Vec<f64> = o
                        .sim
                        .step_buckets
                        .iter()
                        .map(|b| b.steps_num as f64)
                        .collect();
                    if c.len() >= 4 {
                        let m = c.iter().sum::<f64>() / c.len() as f64;
                        let den: f64 = c.iter().map(|x| (x - m).powi(2)).sum();
                        if den > 1e-9 {
                            c.windows(2).map(|w| (w[0] - m) * (w[1] - m)).sum::<f64>() / den
                        } else {
                            0.0
                        }
                    } else {
                        0.0
                    }
                };

                return json_ok(
                    request,
                    json!({
                        "online": true,
                        "distance": distance_km,
                        "duration": duration_min,
                        "actualLength": o.real_length_m,
                        "steps": o.total_steps,
                        "avgCadence": (o.avg_cadence * 10.0).round() / 10.0,
                        "pointCount": o.track_points,
                        "bucketCount": o.sim.step_buckets.len(),
                        "stepAutocorr": (ac * 1000.0).round() / 1000.0,
                        "blocked": o.report.blocked(),
                        "issueSummary": o.report.summary(),
                        "issues": o.report.lines(),
                        "fence": fence,
                        "checkpoints": cps,
                        "checkpointTotal": o.points.coords.len(),
                        "checkpointPassed": passed,
                        "coords": coords,
                        "mode": "online",
                        "trackMode": o.track_mode,
                        "stadiumFallback": o.stadium_fallback,
                        "policy": {
                            "minDistance": o.policy.min_distance,
                            "validTime": o.policy.valid_time,
                        },
                        "logs": logs.borrow().clone(),
                        "note": "已连服务端：围栏与打卡点为真实数据，未提交",
                    }),
                );
            }
            // dry_run 失败就退回本地合成，但要把原因带出来——
            // 否则页面只显示「本地合成」，完全不知道服务端哪一步卡住了
            if let Err(e) = r {
                // 把日志一起带出去。自检失败时日志里逐条写着哪个检查没过，
                // 只回一句「自检未通过」等于让用户猜。
                return json_ok(
                    request,
                    json!({
                        "ok": false,
                        "error": format!("真实流程失败：{e}"),
                        "logs": logs.borrow().clone(),
                    }),
                );
            }
        }
    }

    let total_m = distance_km * 1000.0;
    let total_sec = (duration_min * 60.0).round() as i64;
    let start_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // ── 模式：自由路线 or 操场绕圈 ──
    let mode = body["mode"].as_str().unwrap_or("free");
    let lane = body["lane"].as_i64().unwrap_or(1).clamp(1, 8) as u8;

    // 离线预览用的是**固定尺寸**的标准跑道（`STRAIGHT` + `lane_radius`），
    // 而以你填的坐标当圆心。真正提交时走的是 `try_stadium_track`——拿打卡点
    // 反推场心、尺寸、朝向。两者不是同一条线，所以离线预览只能当「大概长这
    // 样」看；要看真轨迹得连上服务端（在线模式会跑一次 dry_run）。
    let (xy, _planner_len) = if mode == "stadium" {
        let spec = crate::track::StadiumSpec::new(lat, lng).with_lane(lane);
        let lap = crate::track::lap_length(lane);
        let laps = (total_m / lap).max(0.5);
        let pts = crate::track::stadium_track(&spec, laps, 5.0);
        let l = crate::track::geo::path_length(&pts);
        (pts, l)
    } else {
        let req = crate::track::PlanRequest {
            center_lat: lat,
            center_lng: lng,
            checkpoints: Vec::new(),
            total_distance_m: total_m,
            total_sec,
            start_ms,
            sport_type,
            // 5m 一个采样点。实测真机三条记录都是 4.8m/点
            // （1.93km/406 点、1.81km/375 点、2.04km/419 点），
            // 改成 10m 点数就只剩一半，反而和真机对不上。
            sample_step_m: 5.0,
            end: None,
        };
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(start_ms as u64);
        let t = match crate::track::plan(&req, &mut rng) {
            Ok(t) => t,
            Err(e) => return json_err(request, &format!("轨迹规划失败: {e}")),
        };
        let pts: Vec<(f64, f64)> = t.points.iter().map(|p| (p.lat, p.lon)).collect();
        (pts, t.actual_length_m)
    };

    // ── 上报采样：等时间 5 秒（真机节奏）──
    //
    // 以前是「按 5m 密采样出点 → 由速度反推时间戳」，同一条 1683m/555s 的
    // 轨迹会在预览里画 337 个点、1.65 秒一个，真机是 114 个点、4.91 秒一个
    // ——预览和真正提交上去的那份数据不是一个节奏。这里跟 `flow::dry_run`
    // 走同一条采样路径（几何仍是 5m 密折线，上报点按 5 秒沿折线取）。
    // 这一支是「自由规划」预览，`PlanRequest.checkpoints` 是空的，
    // 所以没有点位可对齐（带点位的支路在 `flow::dry_run`）。
    let cp_pairs: Vec<(f64, f64)> = Vec::new();
    let times = crate::track::sample_times(total_sec, start_ms, crate::track::SAMPLE_INTERVAL_SEC);
    let track_points: Vec<crate::track::TrackPoint> = crate::track::sample_by_time(
        &xy,
        &times,
        start_ms as u64,
        &crate::track::KinParams::default(),
        1.0,
        8.0,
        &[],
        &cp_pairs,
    );
    if track_points.len() < 2 {
        return json_err(request, "时间戳生成失败");
    }
    let real_len = crate::track::geo::path_length(
        &track_points
            .iter()
            .map(|q| (q.lat, q.lon))
            .collect::<Vec<_>>(),
    )
    .round() as i64;
    let pts3: Vec<(f64, f64, i64)> = track_points
        .iter()
        .map(|q| (q.lat, q.lon, q.time_ms))
        .collect();

    // ── 生物力学合成 ──
    let sim_params = crate::track::SimParams {
        total_distance_m: real_len as f64,
        total_sec,
        start_ms,
        target_steps: steps,
        seed: start_ms as u64,
        ..Default::default()
    };
    let sim = match crate::track::simulate(&pts3, &sim_params) {
        Ok(s) => s,
        Err(e) => return json_err(request, &format!("数据合成失败: {e}")),
    };
    let steps = sim.total_steps;

    // ── 提交前自检 ──
    let cp_tuples: Vec<(f64, f64, String)> = Vec::new();
    let check = crate::track::Check {
        points: &track_points,
        sim: &sim,
        params: &sim_params,
        checkpoints: &cp_tuples,
        total_distance_m: real_len,
        total_time_sec: total_sec,
        total_steps: steps,
        start_time_ms: start_ms,
        stop_time_ms: start_ms + total_sec * 1000,
        pace_range: None,
        step_range: None,
        expected_interval_sec: Some(crate::track::SAMPLE_INTERVAL_SEC),
    };
    let report = crate::track::inspect(&check);

    // ── 报文 ──
    //
    // 这里原来把 jitter 写死成 vec![1.0; n] 再交给 build，结果所有桶权重
    // 相同，步数退化成常数序列（一阶自相关 ≈ 0），正是 README 说「一眼假」
    // 的那种机器指纹。现在 build 直接取 sim 的桶，不再吃 jitter。
    let params = crate::api::submit::SubmitParams {
        uid: "preview".into(),
        unid: "0".into(),
        uuid: "00000000-0000-4000-8000-000000000000".into(),
        sport_type,
        total_distance_m: real_len,
        total_time_sec: total_sec,
        total_steps: steps,
        start_time_ms: start_ms,
        stop_time_ms: start_ms + total_sec * 1000,
        sel_distance_m: (real_len as f64 * 0.9) as i64,
        sel_run_time_sec: (total_sec as f64 * 0.9) as i64,
        policy: "preview".into(),
        run_area_id: 0,
        alt_range: None,
        geo_fences_json: "[]".into(),
        total_ascent: 0,
    };

    let sub = match crate::api::submit::build(&params, &track_points, &[], &sim) {
        Ok(s) => s,
        Err(e) => return json_err(request, &format!("报文组装失败: {e}")),
    };
    let track = crate::track::PlanResult {
        points: track_points.clone(),
        checkpoints: Vec::new(),
        actual_length_m: real_len as f64,
    };

    // 轨迹点抽稀给前端画图（全量 400+ 点没必要都传）
    let step = (track.points.len() / 200).max(1);
    let coords: Vec<serde_json::Value> = track
        .points
        .iter()
        .step_by(step)
        .map(|p| json!([p.lat, p.lon]))
        .collect();

    let bucket_count = (total_sec / 10).max(1);
    let issues: Vec<String> = report.lines();
    let ac = {
        let c: Vec<f64> = sim
            .step_buckets
            .iter()
            .map(|b| b.steps_num as f64)
            .collect();
        if c.len() >= 4 {
            let m = c.iter().sum::<f64>() / c.len() as f64;
            let den: f64 = c.iter().map(|x| (x - m).powi(2)).sum();
            if den > 1e-9 {
                c.windows(2).map(|w| (w[0] - m) * (w[1] - m)).sum::<f64>() / den
            } else {
                0.0
            }
        } else {
            0.0
        }
    };
    json_ok(
        request,
        json!({
            "distance": distance_km,
            "duration": duration_min,
            "steps": steps,
            "predictedSpeed": sub.save_record["speed"],
            "predictedCalorie": sub.save_record["calorie"],
            "avgStepFreq": sub.save_record["avgStepFreq"],
            "complete": sub.save_record["complete"],
            "signature": sub.save_record["signature"],
            "payloadBytes": serde_json::to_string(&sub.save_record).map(|s| s.len()).unwrap_or(0),
            "pointCount": track.points.len(),
            "actualLength": (track.actual_length_m * 100.0).round() / 100.0,
            "bucketCount": bucket_count,
            "checkpoints": track.checkpoints.len(),
            "coords": coords,
            "mode": mode,
            // 离线预览不是真实提交的几何：操场模式这里画的是固定尺寸标准跑道，
            // 在线提交时是从打卡点反推的椭圆。前端据此提示一句。
            "preview": true,
            "lane": lane,
            "avgCadence": (sim.avg_cadence * 10.0).round() / 10.0,
            "stepAutocorr": (ac * 1000.0).round() / 1000.0,
            "blocked": report.blocked(),
            "issueSummary": report.summary(),
            "issues": issues,
            "note": "本地预演，未提交到服务器",
        }),
    )
}

// ---------- 真跑：提交 ----------

/// 解析手绘轨迹。
///
/// 前端底图是高德，瓦片按 GCJ-02 网格切，Leaflet 拿到手的就是 GCJ-02，
/// 提交报文要 BD-09，所以逐点 `gcj02_to_bd09`。
/// 少于 2 个点当作没传，退回算法规划。
/// 从请求体读海拔区间 `[altMin, altMax]`（米）。
///
/// 不同校区海拔差得远，写死一个基准等于把「在哪跑」暴露出来。用户给了
/// 区间就按中点+半宽生成，没给或给反了就返回 `None` 用默认值，不报错。
pub(crate) fn parse_alt_range(body: &Value) -> Option<(f64, f64)> {
    let lo = body.get("altMin").and_then(|v| v.as_f64())?;
    let hi = body.get("altMax").and_then(|v| v.as_f64())?;
    if hi > lo + 1.0 && lo > -500.0 && hi < 9000.0 {
        Some((lo, hi))
    } else {
        None
    }
}

pub(crate) fn parse_custom_path(body: &Value) -> Option<Vec<(f64, f64)>> {
    body.get("customPath")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let pair = item.as_array()?;
                    let lat = pair.first()?.as_f64()?;
                    let lng = pair.get(1)?.as_f64()?;
                    // 明显不是经纬度的值直接丢掉，别把脏数据带进报文
                    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
                        return None;
                    }
                    Some(crate::api::coord::gcj02_to_bd09(lat, lng))
                })
                .collect::<Vec<(f64, f64)>>()
        })
        .filter(|v| v.len() >= 2)
}

pub(crate) fn handle_run(
    request: tiny_http::Request,
    state: Shared,
    body: &Value,
) -> Result<(), String> {
    let distance_km = body["distance"].as_f64().unwrap_or(2.0);
    let duration_min = body["duration"].as_f64().unwrap_or(12.0);
    let lat = body["lat"].as_f64();
    let lng = body["lng"].as_f64();
    let run_mode = body["runMode"].as_i64().unwrap_or(1);
    let start_at_ms = body["startAtMs"].as_i64().filter(|v| *v > 0);
    let sport_type = body["sportType"].as_i64().unwrap_or(1) as i32;

    // 提交前核对当前会话账号。
    //
    // 出过两次事：为了对比登录过另一个号，回来忘了切，记录落到别人账号下，
    // 本地没有删除接口，只能去 App 里手动删。调用方给了 `expectUid` 就核对，
    // 对不上直接拒。**注意这个校验必须落在 `handle_run` 里**——上一版误加到了
    // `handle_batch`，那条入口根本没人走，等于没加。
    if let Some(want) = body["expectUid"].as_str().filter(|v| !v.is_empty()) {
        let now = { lock_state(&state).current.clone() };
        let now_str = now.clone().unwrap_or_else(|| "（未登录）".into());
        if now.as_deref() != Some(want) {
            return json_err(
                request,
                &format!("当前登录的是 {now_str}，不是 expectUid 指定的 {want}，已拒绝提交"),
            );
        }
    }

    if distance_km <= 0.0 || distance_km > 42.0 {
        return json_err(request, "距离需在 0~42 公里之间");
    }
    if duration_min <= 0.0 || duration_min > 120.0 {
        return json_err(request, "时长需在 0~120 分钟之间（超过服务端必然驳回）");
    }

    // 先克隆 client 再放锁——后面 fetch_fences 是网络请求，持着 Mutex
    // 做 IO 会让 /api/accounts 之类的请求全部排队，页面看起来像服务死了。
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

    // 中心点：请求里没给就用围栏反推。
    //
    // 这里原来兜底成 `(30.5, 104.0)`（成都），而校区在东北某平原校区。围栏拉取
    // 一旦失败，闭环就变成「成都 → 各打卡点 → 成都」，规划器报
    // 「最短闭环 1777736m」，用户看到一条跨越半个中国的轨迹。
    //
    // 兜底坐标不该出现在业务路径上：拿不到真实位置就明确报错，别静默
    // 生成一条荒唐轨迹——错误信息比一条假轨迹有用。
    let coord_sys = body["coord"].as_str().unwrap_or("gcj02");
    let (clat, clng) = match (lat, lng) {
        (Some(a), Some(b)) => match input_to_bd09(a, b, coord_sys) {
            Ok(c) => c,
            Err(e) => return json_err(request, &e),
        },
        _ => match crate::api::fence::fetch_fences(&client)
            .and_then(|fs| crate::api::fence::center_from_fences(&fs))
        {
            // 空围栏（geoFences=null）由 center_from_fences 报可诊断的错误，
            // 不能对空数组取 [0]（issue #36）。
            Ok(c) => c,
            // 错误信息自带诊断（geoFences 个数、各围栏点数、字段名），
            // 用户直接贴过来就能判断是「服务端没数据」还是「字段改名了」。
            Err(e) => {
                return json_err(request, &format!("无法确定校区位置: {e}"));
            }
        },
    };

    let custom_path = parse_custom_path(body);

    let p = crate::api::flow::RunParams {
        center_lat: clat,
        center_lng: clng,
        distance_m: distance_km * 1000.0,
        duration_sec: (duration_min * 60.0).round() as i64,
        sport_type,
        run_mode,
        free_run: sport_type != 1,
        start_at_ms,
        custom_path,
        alt_range: parse_alt_range(body),
        prefer_stadium: body["mode"].as_str() == Some("stadium"),
        prefer_roads: body["preferRoads"].as_bool().unwrap_or(true),
        ..Default::default()
    };

    // 日志用共享容器，闭包借用结束后还能读
    let logs: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
        std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let logs2 = std::rc::Rc::clone(&logs);
    let mut log = move |s: &str| logs2.borrow_mut().push(s.to_string());
    let mut confirm = |_o: &crate::api::flow::RunOutcome| true;

    let result = crate::api::flow::run_full(&client, &p, &mut log, &mut confirm);
    drop(log);

    match result {
        Ok(o) => {
            // 记下这次提交的时刻。
            //
            // 这个字段原来从没被更新过——`last_submit_ms` 只在切换账号时
            // 保留旧值，永远是 0。于是静默窗口判断 `now - last_submit >= 600s`
            // 恒成立，客户端这边等于完全不拦；连续点几下全发给服务端，
            // 每次都要等它回 10603 才知道太频繁。
            if o.submitted {
                {
                    let mut st = lock_state(&state);
                    if let Some(id) = st.current.clone() {
                        let now = crate::crypto::envelope::now_ms() as i64;
                        if let Some(a) = st.book.get_mut(&id) {
                            a.last_submit_ms = now;
                        }
                        st.persist();
                    }
                }
            }
            json_ok(
                request,
                json!({
                "submitted": o.submitted,
                "recordId": o.record_id,
                "distance": o.real_length_m,
                "steps": o.total_steps,
                "cadence": o.avg_cadence,
                "points": o.track_points,
                "obsKeys": o.obs_keys.len(),
                "logs": logs.borrow().clone(),
                }),
            )
        }
        Err(e) => json_err(request, &format!("{e}\n---\n{}", logs.borrow().join("\n"))),
    }
}

#[cfg(test)]
mod fence_coord_tests {
    use crate::api::coord::{bd09_to_gcj02, gcj02_to_bd09};

    /// 围栏环必须从 BD-09 转到 GCJ-02，且转换量级要**明显可见**。
    ///
    /// issue #3 里「电子围栏还是处于偏离状态」的根因就是这个：`Fence.ring`
    /// 是 BD-09，前端铺的是高德瓦片（GCJ-02），以前直接画，整圈偏几百米。
    /// 这条测试钉住两件事：
    ///
    /// 1. `ringGcj` 的每个点都等于 `bd09_to_gcj02(ring)`（不是原样拷贝）；
    /// 2. 偏移量在百米量级——如果哪天有人把转换删了，位移会变成 0，
    ///    这条立刻红。
    #[test]
    fn fence_ring_must_be_converted_to_gcj() {
        // 东北某平原校区一带（issue 里那位用户的学校）
        let ring_bd = [
            (47.3794, 103.9502),
            (47.3800, 103.9510),
            (47.3790, 103.9520),
        ];
        let ring_gcj: Vec<(f64, f64)> = ring_bd.iter().map(|&(a, b)| bd09_to_gcj02(a, b)).collect();

        assert_eq!(ring_gcj.len(), ring_bd.len(), "点数不能变");
        for (i, (&(ba, bo), &(ga, go))) in ring_bd.iter().zip(&ring_gcj).enumerate() {
            assert_ne!((ba, bo), (ga, go), "第 {i} 点没有转换（原样拷贝）");
            let d = crate::track::geo::distance((ba, bo), (ga, go));
            assert!(
                (80.0..=1200.0).contains(&d),
                "第 {i} 点 BD-09→GCJ-02 位移 {d:.1}m，不在百米量级——\n\
                 这条判断的依据是真机围栏整体偏几百米；如果位移接近 0，\n\
                 说明转换被删了，或者两个坐标系被搞混了"
            );
        }
    }

    /// 往返一次要回到原点。
    ///
    /// 容差 1e-5 度（≈1.1m）不是凑数：BD-09 ↔ GCJ-02 是有名的**近似**变换，
    /// 官方从不公开逆变换，业界这套多项式的往返误差本身就在 0.1~1m 量级
    /// （实测这里 1.6e-6 度 ≈ 0.18m）。容差设成 1e-6 是在要求这个近似算法
    /// 精确可逆，那做不到，也不该由这条测试来保证。
    #[test]
    fn fence_ring_roundtrip_rounds_back() {
        let p = (47.3794, 103.9502);
        let (ga, go) = bd09_to_gcj02(p.0, p.1);
        let (ba, bo) = gcj02_to_bd09(ga, go);
        assert!(
            (ba - p.0).abs() < 1e-5 && (bo - p.1).abs() < 1e-5,
            "往返 ({} {}) → ({} {}) 偏差过大",
            p.0,
            p.1,
            ba,
            bo
        );
    }
}

#[cfg(test)]
mod alt_range_tests {
    use super::parse_alt_range;

    /// 合法的海拔区间原样返回。
    #[test]
    fn accepts_valid_range() {
        let v = serde_json::json!({"altMin": 100, "altMax": 140});
        assert_eq!(parse_alt_range(&v), Some((100.0, 140.0)));
        // 沿海校区：区间窄但仍有意义（比 GPS 高程噪声 0.8m 宽就行）
        let v = serde_json::json!({"altMin": 5.0, "altMax": 9.0});
        assert_eq!(parse_alt_range(&v), Some((5.0, 9.0)));
    }

    /// 上下界搞反、只填一个、超出地形范围，一律当没填（退回默认），
    /// 不报错——用户填这个是无所谓的，填错不该让整次生成失败。
    #[test]
    fn ignores_bad_input_without_failing() {
        let cases = [
            serde_json::json!({"altMin": 140, "altMax": 100}), // 反了
            serde_json::json!({"altMin": 100}),                // 缺一个
            serde_json::json!({"altMax": 100}),                // 缺一个
            serde_json::json!({}),                             // 都没填
            serde_json::json!({"altMin": -600, "altMax": 100}), // 死海以下
            serde_json::json!({"altMin": 100, "altMax": 12000}), // 高于珠峰
            serde_json::json!({"altMin": 100, "altMax": 100.5}), // 区间太窄
            serde_json::json!({"altMin": "高", "altMax": "低"}), // 不是数字
        ];
        for c in cases {
            assert_eq!(parse_alt_range(&c), None, "该拒绝的没拒绝: {c}");
        }
    }
}

#[cfg(test)]
mod center_input_tests {
    use super::input_to_bd09;
    use crate::track::geo::distance;

    /// 三个坐标系喂同一个地点，转成 BD-09 后彼此相差几百米——这正是
    /// 不转换的代价。若哪天有人把转换删了，这条会立刻炸。
    #[test]
    fn each_coord_system_lands_somewhere_different() {
        let (lat, lng) = (47.3864, 103.9586);
        let gcj = input_to_bd09(lat, lng, "gcj02").unwrap();
        let wgs = input_to_bd09(lat, lng, "wgs84").unwrap();
        let bd = input_to_bd09(lat, lng, "bd09").unwrap();

        // bd09 原样透传
        assert_eq!(bd, (lat, lng));

        // BD-09 相对 GCJ-02 的偏移随纬度变化（经度方向被 cos(lat) 压缩，
        // 这个纬度上实测 855m），所以只断言量级，不断言具体值。
        let d_gcj = distance((lat, lng), gcj);
        assert!(
            d_gcj > 100.0 && d_gcj < 1500.0,
            "gcj02→bd09 偏移异常: {d_gcj}m"
        );

        // WGS-84 多叠一层，和 GCJ-02 的结果必须真的不一样——两者若被
        // 当成同一套处理，说明转换链有一环没接上。
        let d_wgs = distance((lat, lng), wgs);
        assert!(
            d_wgs > 100.0 && d_wgs < 2000.0,
            "wgs84→bd09 偏移异常: {d_wgs}m"
        );
        assert!(
            (d_wgs - d_gcj).abs() > 50.0,
            "两套转换结果几乎重合，可能有一层没生效: {d_wgs}m vs {d_gcj}m"
        );
    }

    #[test]
    fn rejects_invalid_center() {
        // (0,0) 是几内亚湾，不是任何一所学校
        assert!(input_to_bd09(0.0, 0.0, "gcj02").is_err());
        assert!(input_to_bd09(91.0, 123.0, "gcj02").is_err());
        assert!(input_to_bd09(47.0, 181.0, "gcj02").is_err());
        assert!(input_to_bd09(-91.0, 0.0, "wgs84").is_err());
    }

    #[test]
    fn unknown_system_falls_back_to_gcj02() {
        // 前端下拉以外的值不该让整个请求炸掉，按最常见的 GCJ-02 处理
        let a = input_to_bd09(47.3864, 103.9586, "whatever").unwrap();
        let b = input_to_bd09(47.3864, 103.9586, "gcj02").unwrap();
        assert_eq!(a, b);
    }
}
