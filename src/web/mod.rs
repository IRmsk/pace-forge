//! Web 控制台：登录、记录列表、轨迹地图。
//!
//! 设计上刻意保持薄：
//! - 会话状态只存一份 `Client`（含 uid/token/unid），进程退出即失效
//! - 不落盘密码，登录成功后密码直接丢弃
//! - 所有业务逻辑复用 `api/*`，Web 层只做转发与 JSON 序列化
//!
//! 轨迹字段问题：服务端记录详情里字段名不稳定（不同版本、不同接口各有差异），
//! 所以后端不解析轨迹，原样返回 `raw`，由前端递归探测坐标数组。这样即使
//! 服务端改字段名，地图仍然能画出来。

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tiny_http::{Header, Method, Response, Server};

use crate::api::client::{Client, DeviceProfile};
use crate::api::login::{login, LoginOutcome, UserProfile};
use crate::api::records;

type Shared = Arc<Mutex<State>>;

struct State {
    client: Option<Client>,
    profile: Option<UserProfile>,
}

impl State {
    fn new() -> Self {
        Self { client: None, profile: None }
    }

    /// 从落盘的会话恢复登录态。
    ///
    /// 进程重启后不必重新登录，更重要的是：只有恢复出 token，
    /// 退出时才能通知服务端让会话失效。
    fn restore() -> Self {
        use crate::api::client::{Client, Credentials, DeviceProfile};
        use crate::api::login::UserProfile;

        let sess = crate::api::session::load_session();
        if !sess.is_valid() {
            return Self::new();
        }
        let mut client = Client::new(DeviceProfile::default());
        client.creds = Credentials {
            uid: Some(sess.uid.clone()),
            token: Some(sess.token.clone()),
            unid: Some(sess.unid.clone()),
        };
        let profile = UserProfile {
            uid: sess.uid,
            unid: sess.unid,
            token: sess.token,
            name: sess.name,
            username: sess.username,
            campus_name: sess.campus_name,
            campus_id: String::new(),
            depart: sess.depart,
            grade_class: sess.grade_class,
            sex: String::new(),
        };
        println!("[web] 已恢复会话 uid={}", profile.uid);
        Self { client: Some(client), profile: Some(profile) }
    }

    fn logged_in(&self) -> bool {
        self.client.as_ref().map(|c| c.creds.token.is_some()).unwrap_or(false)
    }
}

/// 启动 Web 控制台。
pub fn serve(addr: &str) -> Result<(), String> {
    let server = Server::http(addr).map_err(|e| format!("监听 {addr} 失败: {e}"))?;
    let state: Shared = Arc::new(Mutex::new(State::restore()));

    println!("Web 控制台已启动: http://{addr}");
    println!("按 Ctrl+C 停止");

    for request in server.incoming_requests() {
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            if let Err(e) = handle(request, state) {
                eprintln!("请求处理失败: {e}");
            }
        });
    }
    Ok(())
}

fn handle(mut request: tiny_http::Request, state: Shared) -> Result<(), String> {
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("/").to_string();
    let query = url.split('?').nth(1).unwrap_or("").to_string();
    let method = request.method().clone();

    match (method, path.as_str()) {
        (Method::Get, "/") => respond_html(request, INDEX_HTML),
        (Method::Post, "/api/login") => {
            let body = read_json(&mut request)?;
            handle_login(request, state, &body)
        }
        (Method::Get, "/api/me") => {
            let st = state.lock().map_err(|e| e.to_string())?;
            match &st.profile {
                Some(p) => json_ok(request, json!(p)),
                None => json_err(request, "未登录"),
            }
        }
        (Method::Post, "/api/logout") => {
            // 先通知服务端让 token 失效，再清本地。
            //
            // 顺序不能反：只清本地的话服务端会话还活着，账号被标记为已登录，
            // 再登录会拿到 10121（已在其他设备登录），而且锁一小时。
            let remote = {
                let mut st = state.lock().map_err(|e| e.to_string())?;
                if let Some(client) = st.client.as_mut() {
                    crate::api::login::logout(client).unwrap_or(false)
                } else {
                    false
                }
            };
            {
                let mut st = state.lock().map_err(|e| e.to_string())?;
                *st = State::new();
            }
            // 服务端已通知（或本来就没登录），本地会话文件也要删，
            // 否则下次启动会从文件里把旧 token 恢复回来。
            crate::api::session::clear_session();
            json_ok(request, json!({"ok": true, "remoteLogout": remote}))
        }
        (Method::Post, "/api/plan") => {
            let body = read_json(&mut request)?;
            handle_plan(request, state, &body)
        }
        (Method::Get, "/api/records") => handle_records(request, state, &query),
        (Method::Get, "/api/health") => json_ok(request, json!({"ok": true, "version": crate::VERSION})),
        _ => {
            if let Some(rest) = path.strip_prefix("/api/record/") {
                let rrid: i64 = rest.parse().map_err(|_| "rrid 不是数字")?;
                return handle_record_detail(request, state, rrid);
            }
            json_err_code(request, 404, "not found")
        }
    }
}

fn handle_login(request: tiny_http::Request, state: Shared, body: &Value) -> Result<(), String> {
    let username = body.get("username").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let password = body.get("password").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if username.is_empty() || password.is_empty() {
        return json_err(request, "手机号和密码都要填");
    }

    let mut client = Client::new(DeviceProfile::default());
    match login(&mut client, &username, &password) {
        Ok(LoginOutcome::Ok(profile)) => {
            let mut st = state.lock().map_err(|e| e.to_string())?;
            st.client = Some(client);
            st.profile = Some(profile.clone());
            json_ok(request, json!(profile))
        }
        Ok(LoginOutcome::NeedCaptcha { uuid, message }) => {
            json_err(request, &format!("{message}（uuid={uuid}）"))
        }
        Err(e) => json_err(request, &e),
    }
}

/// 轨迹预演：本地生成轨迹 + 提交报文，不联网。
///
/// 这一步能验证轨迹几何、分桶、签名是否合规，但**不会**真的提交——
/// 真实提交需要先拿服务端的规则/围栏/打卡点，且要过静默期。
fn handle_plan(
    request: tiny_http::Request,
    _state: Shared,
    body: &serde_json::Value,
) -> Result<(), String> {
    let distance_km = body["distance"].as_f64().unwrap_or(2.0);
    let duration_min = body["duration"].as_f64().unwrap_or(12.0);
    let steps = body["steps"].as_i64().unwrap_or((distance_km * 1150.0) as i64) as i32;
    let sport_type = body["sportType"].as_i64().unwrap_or(1) as i32;
    let lat = body["lat"].as_f64().unwrap_or(30.5);
    let lng = body["lng"].as_f64().unwrap_or(104.0);

    if distance_km <= 0.0 || distance_km > 42.0 {
        return json_err(request, "距离需在 0~42 公里之间");
    }
    if duration_min <= 0.0 || duration_min > 300.0 {
        return json_err(request, "时长需在 0~300 分钟之间");
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
            sample_step_m: 5.0,
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

    // ── 由速度反推时间戳 ──
    let (times, _speeds, real_len) = crate::track::retime(
        &xy,
        total_sec,
        start_ms,
        start_ms as u64,
        &crate::track::KinParams::default(),
        1.0,
        8.0,
    );
    if times.len() != xy.len() {
        return json_err(request, "时间戳生成失败");
    }
    let track_points: Vec<crate::track::TrackPoint> = xy
        .iter()
        .zip(&times)
        .map(|((a, b), t)| crate::track::TrackPoint {
            lat: *a,
            lon: *b,
            time_ms: *t,
        })
        .collect();
    let pts3: Vec<(f64, f64, i64)> = xy
        .iter()
        .zip(&times)
        .map(|((a, b), t)| (*a, *b, *t))
        .collect();

    // ── 生物力学合成 ──
    let sim_params = crate::track::SimParams {
        total_distance_m: real_len as f64,
        total_sec,
        start_ms,
        target_steps: body["steps"].as_i64().map(|v| v as i32),
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
    };
    let report = crate::track::inspect(&check);

    // ── 报文 ──
    let jitter: Vec<f64> = vec![1.0; sim.step_buckets.len().max(1)];
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
        geo_fences_json: "[]".into(),
        total_ascent: 0,
    };

    let sub = match crate::api::submit::build(&params, &track_points, &[], &jitter) {
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
        let c: Vec<f64> = sim.step_buckets.iter().map(|b| b.steps_num as f64).collect();
        if c.len() >= 4 {
            let m = c.iter().sum::<f64>() / c.len() as f64;
            let den: f64 = c.iter().map(|x| (x - m).powi(2)).sum();
            if den > 1e-9 {
                c.windows(2).map(|w| (w[0] - m) * (w[1] - m)).sum::<f64>() / den
            } else { 0.0 }
        } else { 0.0 }
    };
    json_ok(request, json!({
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
        "lane": lane,
        "avgCadence": (sim.avg_cadence * 10.0).round() / 10.0,
        "stepAutocorr": (ac * 1000.0).round() / 1000.0,
        "blocked": report.blocked(),
        "issueSummary": report.summary(),
        "issues": issues,
        "note": "本地预演，未提交到服务器",
    }))
}

fn handle_records(request: tiny_http::Request, state: Shared, query: &str) -> Result<(), String> {
    let page = query_param(query, "page").and_then(|v| v.parse().ok()).unwrap_or(1u32);
    let size = query_param(query, "size").and_then(|v| v.parse().ok()).unwrap_or(20u32);
    let sport = query_param(query, "type").and_then(|v| v.parse().ok()).unwrap_or(1u32);

    let st = state.lock().map_err(|e| e.to_string())?;
    if !st.logged_in() {
        return json_err(request, "请先登录");
    }
    let client = st.client.as_ref().unwrap();
    match records::list_records(client, page, size, sport) {
        Ok(data) => json_ok(request, data),
        Err(e) => json_err(request, &e),
    }
}

fn handle_record_detail(
    request: tiny_http::Request,
    state: Shared,
    rrid: i64,
) -> Result<(), String> {
    let st = state.lock().map_err(|e| e.to_string())?;
    if !st.logged_in() {
        return json_err(request, "请先登录");
    }
    let client = st.client.as_ref().unwrap();
    match records::get_record(client, rrid) {
        Ok(data) => json_ok(request, data),
        Err(e) => json_err(request, &e),
    }
}

// ---------- HTTP 小工具 ----------

fn read_json(request: &mut tiny_http::Request) -> Result<Value, String> {
    let mut s = String::new();
    let r = request.as_reader();
    r.read_to_string(&mut s).map_err(|e| e.to_string())?;
    serde_json::from_str(&s).map_err(|e| format!("请求体不是 JSON: {e}"))
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let mut it = pair.splitn(2, '=');
        let k = it.next()?;
        if k == key {
            it.next().map(|v| v.to_string())
        } else {
            None
        }
    })
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("header 构造失败")
}

fn respond_json(
    request: tiny_http::Request,
    status: u16,
    body: Value,
) -> Result<(), String> {
    let s = serde_json::to_string(&body).unwrap_or_else(|_| "{}".into());
    let resp = Response::from_string(s)
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json; charset=utf-8"));
    request.respond(resp).map_err(|e| e.to_string())
}

fn json_ok(request: tiny_http::Request, body: Value) -> Result<(), String> {
    respond_json(request, 200, json!({"ok": true, "data": body}))
}

fn json_err(request: tiny_http::Request, msg: &str) -> Result<(), String> {
    respond_json(request, 200, json!({"ok": false, "error": msg}))
}

fn json_err_code(request: tiny_http::Request, code: u16, msg: &str) -> Result<(), String> {
    respond_json(request, code, json!({"ok": false, "error": msg}))
}

fn respond_html(request: tiny_http::Request, html: &str) -> Result<(), String> {
    let resp = Response::from_string(html)
        .with_header(header("Content-Type", "text/html; charset=utf-8"))
        // 不设这个头，浏览器会一直用缓存的旧页面：改了按钮逻辑用户也看不到，
        // 点了还是旧行为（表现为「退出键没反应」）。
        .with_header(header("Cache-Control", "no-store, no-cache, must-revalidate, max-age=0"))
        .with_header(header("Pragma", "no-cache"))
        .with_header(header("Expires", "0"));
    request.respond(resp).map_err(|e| e.to_string())
}

const INDEX_HTML: &str = include_str!("index.html");
