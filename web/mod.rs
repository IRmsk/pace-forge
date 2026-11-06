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
    /// 账号库（落盘）。
    book: crate::api::accounts::Book,
    /// 每个账号一个独立 Client，token 互不干扰。
    clients: std::collections::HashMap<String, Client>,
    /// 当前选中的账号 id。
    current: Option<String>,
}

impl State {
    fn new() -> Self {
        Self {
            book: crate::api::accounts::Book::default(),
            clients: std::collections::HashMap::new(),
            current: None,
        }
    }

    /// 从磁盘恢复所有账号的登录态。
    fn restore() -> Self {
        let book = crate::api::accounts::load();
        let mut clients = std::collections::HashMap::new();
        for a in &book.accounts {
            if a.logged_in() {
                clients.insert(a.id.clone(), make_client(&a.uid, &a.token, &a.unid));
            }
        }
        let current = book
            .accounts
            .iter()
            .find(|a| a.logged_in())
            .map(|a| a.id.clone());
        if let Some(id) = &current {
            println!("[web] 恢复 {} 个账号，当前={id}", clients.len());
        }
        Self { book, clients, current }
    }

    /// 当前账号。
    fn current_account(&self) -> Option<&crate::api::accounts::Account> {
        self.current.as_ref().and_then(|id| self.book.get(id))
    }

    /// 当前账号是否可发请求。
    fn logged_in(&self) -> bool {
        self.current_account().map(|a| a.logged_in()).unwrap_or(false)
    }

    /// 当前账号的 Client。
    fn client(&self) -> Option<&Client> {
        self.current.as_ref().and_then(|id| self.clients.get(id))
    }

    /// 某账号的 Client。
    fn client_of(&self, id: &str) -> Option<&Client> {
        self.clients.get(id)
    }

    /// 用新登录结果更新一个账号。
    fn apply_login(&mut self, id: &str, p: &UserProfile) {
        if let Some(a) = self.book.get_mut(id) {
            a.uid = p.uid.clone();
            a.token = p.token.clone();
            a.unid = p.unid.clone();
            a.name = p.name.clone();
            a.school = p.campus_name.clone();
            a.last_login_ms = crate::crypto::envelope::now_ms() as i64;
        }
        self.clients
            .insert(id.to_string(), make_client(&p.uid, &p.token, &p.unid));
        self.current = Some(id.to_string());
        let _ = crate::api::accounts::save(&self.book);
    }

    fn persist(&self) {
        if let Err(e) = crate::api::accounts::save(&self.book) {
            eprintln!("保存账号库失败: {e}");
        }
    }
}

fn make_client(uid: &str, token: &str, unid: &str) -> Client {
    // 每个账号独立的 device_id，避免被按设备聚合
    let mut c = Client::new(DeviceProfile::for_account(uid));
    c.creds = crate::api::client::Credentials {
        uid: Some(uid.to_string()),
        token: Some(token.to_string()),
        unid: Some(unid.to_string()),
    };
    c
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
            // 单个请求里的 panic 只杀这一个线程，不带走整个进程和正在跑的批次。
            // 之前 release 配的是 panic="abort"，一处 unwrap 失败就等于全崩。
            let guard = std::panic::AssertUnwindSafe(|| handle(request, state));
            match std::panic::catch_unwind(guard) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("请求处理失败: {e}"),
                Err(_) => eprintln!("请求处理 panic（已隔离，进程继续）"),
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

    match (method.clone(), path.as_str()) {
        (Method::Get, "/") => respond_html(request, INDEX_HTML),
        (Method::Post, "/api/login") => {
            let body = read_json(&mut request)?;
            handle_login(request, state, &body)
        }
        (Method::Get, "/api/me") => {
            let st = state.lock().map_err(|e| e.to_string())?;
            match st.current_account() {
                Some(a) => json_ok(request, json!({
                    "id": a.id, "uid": a.uid, "unid": a.unid,
                    "name": a.name, "username": a.username, "school": a.school,
                    "loggedIn": a.logged_in(),
                })),
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
                let id = st.current.clone();
                match id.and_then(|i| st.clients.get_mut(&i)) {
                    Some(c) => crate::api::login::logout(c).unwrap_or(false),
                    None => false,
                }
            };
            {
                let mut st = state.lock().map_err(|e| e.to_string())?;
                if let Some(id) = st.current.clone() {
                    if let Some(a) = st.book.get_mut(&id) {
                        a.token.clear();
                        a.uid.clear();
                    }
                    st.clients.remove(&id);
                    st.current = None;
                }
                st.persist();
            }
            crate::api::session::clear_session();
            json_ok(request, json!({"ok": true, "remoteLogout": remote}))
        }
        (Method::Post, "/api/plan") => {
            let body = read_json(&mut request)?;
            handle_plan(request, state, &body)
        }
        (Method::Get, "/api/accounts") => handle_accounts(request, state),
        (Method::Get, "/api/starttimes") => handle_starttimes(request),
        (Method::Post, "/api/accounts") => {
            let body = read_json(&mut request)?;
            handle_add_account(request, state, &body)
        }
        (Method::Post, "/api/accounts/login-all") => handle_login_all(request, state),
        (Method::Post, "/api/batch") => {
            let body = read_json(&mut request)?;
            handle_batch(request, state, &body)
        }
        (Method::Get, "/api/semester") => handle_semester(request, state),
        (Method::Post, "/api/run") => {
            let body = read_json(&mut request)?;
            handle_run(request, state, &body)
        }
        (Method::Get, "/api/fences") => handle_fences(request, state, &query),
        (Method::Get, "/api/records") => handle_records(request, state, &query),
        (Method::Get, "/api/health") => json_ok(request, json!({"ok": true, "version": crate::VERSION})),
        _ => {
            if let Some(rest) = path.strip_prefix("/api/accounts/") {
                // /api/accounts/{id}/switch  |  /api/accounts/{id}  (DELETE)
                let mut it = rest.splitn(2, '/');
                let id = it.next().unwrap_or("").to_string();
                let action = it.next().unwrap_or("");
                return match (method, action) {
                    (Method::Delete, "") => handle_del_account(request, state, &id),
                    (Method::Post, "switch") => handle_switch(request, state, &id),
                    (Method::Post, "login") => handle_login_one(request, state, &id),
                    (Method::Post, "toggle") => handle_toggle(request, state, &id),
                    _ => json_err_code(request, 404, "unknown account action"),
                };
            }
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
            let id = if profile.uid.is_empty() {
                username.clone()
            } else {
                profile.uid.clone()
            };
            // 密码加密存盘，下次不用再输
            let enc = crate::api::accounts::seal_password(&password).unwrap_or_default();
            let mut acc = crate::api::accounts::Account {
                id: id.clone(),
                username: username.clone(),
                password_enc: enc,
                ..Default::default()
            };
            if let Some(old) = st.book.get(&id) {
                acc.enabled = old.enabled;
                acc.last_submit_ms = old.last_submit_ms;
            }
            st.book.upsert(acc);
            st.apply_login(&id, &profile);
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
    state: Shared,
    body: &serde_json::Value,
) -> Result<(), String> {
    let distance_km = body["distance"].as_f64().unwrap_or(2.0);
    let duration_min = body["duration"].as_f64().unwrap_or(12.0);
    // 0 或缺失都表示「没指定」，交给模型按配速推算。
    // 手填步数很容易和配速对不上——12 分钟 2300 步 = 192 步/分，
    // 而 2.78 m/s 的典型步频只有 158，差 34 步会被自检判 GAIT_MISMATCH。
    let steps = body["steps"]
        .as_i64()
        .filter(|v| *v > 0)
        .map(|v| v as i32);
    let sport_type = body["sportType"].as_i64().unwrap_or(1) as i32;
    let mut lat = body["lat"].as_f64().unwrap_or(0.0);
    let mut lng = body["lng"].as_f64().unwrap_or(0.0);
    let run_mode = body["runMode"].as_i64().unwrap_or(1);
    let start_at_ms = body["startAtMs"].as_i64().filter(|v| *v > 0);

    // 没传坐标就用围栏中心，别用写死的成都坐标
    if lat == 0.0 || lng == 0.0 {
        let st = state.lock().map_err(|e| e.to_string())?;
        if let Some(client) = st.client() {
            if let Ok(fs) = crate::api::fence::fetch_fences(client) {
                if let Some(f) = fs.first() {
                    if !f.ring.is_empty() {
                        let n = f.ring.len() as f64;
                        let sum = f.ring.iter().fold((0.0, 0.0), |a, q| (a.0 + q.0, a.1 + q.1));
                        lat = sum.0 / n;
                        lng = sum.1 / n;
                    }
                }
            }
        }
        if lat == 0.0 { lat = 30.5; lng = 104.0; }
    }

    if distance_km <= 0.0 || distance_km > 42.0 {
        return json_err(request, "距离需在 0~42 公里之间");
    }
    if duration_min <= 0.0 || duration_min > 300.0 {
        return json_err(request, "时长需在 0~300 分钟之间");
    }

    // ---- 已登录：走真实流程拿围栏和打卡点 ----
    {
        let st = state.lock().map_err(|e| e.to_string())?;
        if st.logged_in() {
            let Some(client) = st.client() else {
                return json_err(request, "当前账号没有可用会话，请重新登录");
            };
            let p = crate::api::flow::RunParams {
                center_lat: lat,
                center_lng: lng,
                distance_m: distance_km * 1000.0,
                duration_sec: (duration_min * 60.0).round() as i64,
                sport_type,
                run_mode,
                start_at_ms,
                ..Default::default()
            };
            let logs: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
                std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let l2 = std::rc::Rc::clone(&logs);
            let mut log = move |s: &str| l2.borrow_mut().push(s.to_string());
            let r = crate::api::flow::dry_run(client, &p, &mut log);
            drop(log);

            if let Ok(o) = r {
                // 围栏多边形
                let fence = o.fence.as_ref().map(|f| {
                    json!({
                        "name": f.selected.name,
                        "runAreaId": f.selected.run_area_id,
                        "fallback": f.fallback,
                        "distance": f.distance_m,
                        "ring": f.selected.polygons.first().cloned().unwrap_or_default(),
                        "allPolygons": f.selected.polygons,
                    })
                });

                // 打卡点 + 是否被轨迹覆盖
                let track_xy: Vec<(f64, f64)> =
                    o.track.iter().map(|t| (t.lat, t.lon)).collect();
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
                        json!({
                            "index": i + 1,
                            "lat": a,
                            "lng": b,
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
                    .map(|t| json!([t.lat, t.lon]))
                    .collect();

                let ac = {
                    let c: Vec<f64> = o.sim.step_buckets.iter().map(|b| b.steps_num as f64).collect();
                    if c.len() >= 4 {
                        let m = c.iter().sum::<f64>() / c.len() as f64;
                        let den: f64 = c.iter().map(|x| (x - m).powi(2)).sum();
                        if den > 1e-9 {
                            c.windows(2).map(|w| (w[0] - m) * (w[1] - m)).sum::<f64>() / den
                        } else { 0.0 }
                    } else { 0.0 }
                };

                return json_ok(request, json!({
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
                    "policy": {
                        "minDistance": o.policy.min_distance,
                        "validTime": o.policy.valid_time,
                    },
                    "logs": logs.borrow().clone(),
                    "note": "已连服务端：围栏与打卡点为真实数据，未提交",
                }));
            }
            // dry_run 失败就退回本地合成
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


// ---------- 记分跑：学期进度 ----------


// ---------- 账号管理 ----------

fn account_view(a: &crate::api::accounts::Account, current: bool) -> Value {
    json!({
        "id": a.id,
        "username": a.username,
        "label": a.label(),
        "name": a.name,
        "school": a.school,
        "uid": a.uid,
        "unid": a.unid,
        "loggedIn": a.logged_in(),
        "enabled": a.enabled,
        "hasPassword": !a.password_enc.is_empty(),
        "lastLoginMs": a.last_login_ms,
        "lastSubmitMs": a.last_submit_ms,
        "current": current,
    })
}

fn handle_accounts(request: tiny_http::Request, state: Shared) -> Result<(), String> {
    let st = state.lock().map_err(|e| e.to_string())?;
    let cur = st.current.clone().unwrap_or_default();
    let list: Vec<Value> = st
        .book
        .accounts
        .iter()
        .map(|a| account_view(a, a.id == cur))
        .collect();
    json_ok(request, json!({
        "count": list.len(),
        "ready": st.book.ready().len(),
        "current": cur,
        "accounts": list,
    }))
}

fn handle_add_account(
    request: tiny_http::Request,
    state: Shared,
    body: &Value,
) -> Result<(), String> {
    let username = body["username"].as_str().unwrap_or("").trim().to_string();
    let password = body["password"].as_str().unwrap_or("").to_string();
    if username.is_empty() || password.is_empty() {
        return json_err(request, "手机号和密码都要填");
    }

    // 登录前还不知道 uid，用手机号做设备作用域；登录成功后再按 uid 重建
    let mut client = Client::new(DeviceProfile::for_account(&username));
    match login(&mut client, &username, &password) {
        Ok(LoginOutcome::Ok(profile)) => {
            let mut st = state.lock().map_err(|e| e.to_string())?;
            let id = if profile.uid.is_empty() { username.clone() } else { profile.uid.clone() };
            let enc = crate::api::accounts::seal_password(&password).unwrap_or_default();
            let mut acc = crate::api::accounts::Account {
                id: id.clone(),
                username: username.clone(),
                password_enc: enc,
                ..Default::default()
            };
            if let Some(old) = st.book.get(&id) {
                acc.enabled = old.enabled;
                acc.last_submit_ms = old.last_submit_ms;
            }
            st.book.upsert(acc);
            st.apply_login(&id, &profile);
            json_ok(request, json!({
                "id": id,
                "name": profile.name,
                "school": profile.campus_name,
                "uid": profile.uid,
            }))
        }
        Ok(LoginOutcome::NeedCaptcha { uuid, message }) => {
            json_err(request, &format!("{message}（uuid={uuid}）"))
        }
        Err(e) => json_err(request, &e),
    }
}

fn handle_del_account(
    request: tiny_http::Request,
    state: Shared,
    id: &str,
) -> Result<(), String> {
    let mut st = state.lock().map_err(|e| e.to_string())?;
    let ok = st.book.remove(id);
    st.clients.remove(id);
    if st.current.as_deref() == Some(id) {
        st.current = None;
    }
    st.persist();
    json_ok(request, json!({"removed": ok}))
}

fn handle_switch(
    request: tiny_http::Request,
    state: Shared,
    id: &str,
) -> Result<(), String> {
    let mut st = state.lock().map_err(|e| e.to_string())?;
    if st.book.get(id).is_none() {
        return json_err(request, "账号不存在");
    }
    st.current = Some(id.to_string());
    json_ok(request, json!({"current": id}))
}

fn handle_toggle(
    request: tiny_http::Request,
    state: Shared,
    id: &str,
) -> Result<(), String> {
    let mut st = state.lock().map_err(|e| e.to_string())?;
    let enabled = match st.book.get_mut(id) {
        Some(a) => {
            a.enabled = !a.enabled;
            a.enabled
        }
        None => return json_err(request, "账号不存在"),
    };
    st.persist();
    json_ok(request, json!({"id": id, "enabled": enabled}))
}

/// 用已存密码重新登录一个账号。
fn handle_login_one(
    request: tiny_http::Request,
    state: Shared,
    id: &str,
) -> Result<(), String> {
    let (username, password) = {
        let st = state.lock().map_err(|e| e.to_string())?;
        match st.book.get(id) {
            Some(a) => {
                if a.password_enc.is_empty() {
                    return json_err(request, "这个账号没存密码，删掉重新添加");
                }
                match crate::api::accounts::open_password(&a.password_enc) {
                    Ok(p) => (a.username.clone(), p),
                    Err(e) => return json_err(request, &e),
                }
            }
            None => return json_err(request, "账号不存在"),
        }
    };

    let mut client = Client::new(DeviceProfile::default());
    match login(&mut client, &username, &password) {
        Ok(LoginOutcome::Ok(profile)) => {
            let mut st = state.lock().map_err(|e| e.to_string())?;
            st.apply_login(id, &profile);
            json_ok(request, json!({"id": id, "name": profile.name, "uid": profile.uid}))
        }
        Ok(LoginOutcome::NeedCaptcha { message, .. }) => json_err(request, &message),
        Err(e) => json_err(request, &e),
    }
}

/// 批量登录所有账号。
fn handle_login_all(request: tiny_http::Request, state: Shared) -> Result<(), String> {
    let todo: Vec<(String, String, String)> = {
        let st = state.lock().map_err(|e| e.to_string())?;
        st.book
            .accounts
            .iter()
            .filter(|a| a.enabled && !a.password_enc.is_empty())
            .filter_map(|a| {
                crate::api::accounts::open_password(&a.password_enc)
                    .ok()
                    .map(|p| (a.id.clone(), a.username.clone(), p))
            })
            .collect()
    };

    if todo.is_empty() {
        return json_err(request, "没有可登录的账号（需要已存密码且启用）");
    }

    let mut results = Vec::new();
    for (i, (id, user, pass)) in todo.iter().enumerate() {
        if i > 0 {
            // 错开，避免同 IP 短时间连续登录触发风控
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
        let mut client = Client::new(DeviceProfile::default());
        match login(&mut client, user, pass) {
            Ok(LoginOutcome::Ok(profile)) => {
                let mut st = state.lock().map_err(|e| e.to_string())?;
                st.apply_login(id, &profile);
                results.push(json!({"id": id, "ok": true, "name": profile.name}));
            }
            Ok(LoginOutcome::NeedCaptcha { message, .. }) => {
                results.push(json!({"id": id, "ok": false, "error": message}));
            }
            Err(e) => results.push(json!({"id": id, "ok": false, "error": e})),
        }
    }

    let ok = results.iter().filter(|r| r["ok"] == true).count();
    json_ok(request, json!({"total": results.len(), "ok": ok, "results": results}))
}

/// 批量刷：所有就绪账号依次跑一遍。
fn handle_batch(
    request: tiny_http::Request,
    state: Shared,
    body: &Value,
) -> Result<(), String> {
    let distance_km = body["distance"].as_f64().unwrap_or(2.0);
    let duration_min = body["duration"].as_f64().unwrap_or(12.0);
    let run_mode = body["runMode"].as_i64().unwrap_or(1);
    let start_at_ms = body["startAtMs"].as_i64().filter(|v| *v > 0);
    let sport_type = body["sportType"].as_i64().unwrap_or(1) as i32;
    let dry = body["dry"].as_bool().unwrap_or(false);
    // 账号间隔（毫秒），默认 8 秒，避免同 IP 高频
    let gap_ms = body["gapMs"].as_u64().unwrap_or(8000);
    let only: Vec<String> = body["ids"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    if distance_km <= 0.0 || distance_km > 42.0 {
        return json_err(request, "距离需在 0~42 公里之间");
    }
    if duration_min <= 0.0 || duration_min > 300.0 {
        return json_err(request, "时长需在 0~300 分钟之间");
    }

    // 挑出要跑的账号
    let targets: Vec<String> = {
        let st = state.lock().map_err(|e| e.to_string())?;
        st.book
            .ready()
            .iter()
            .filter(|a| only.is_empty() || only.contains(&a.id))
            .map(|a| a.id.clone())
            .collect()
    };
    if targets.is_empty() {
        return json_err(request, "没有可跑的账号（需要已登录且启用）");
    }

    let mut results = Vec::new();
    for (i, id) in targets.iter().enumerate() {
        if i > 0 && gap_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(gap_ms));
        }

        // 每个账号自己一份 client 快照
        let client = {
            let st = state.lock().map_err(|e| e.to_string())?;
            st.client_of(id).cloned()
        };
        let Some(client) = client else {
            results.push(json!({"id": id, "ok": false, "error": "无会话"}));
            continue;
        };

        // 静默窗口
        let since = {
            let st = state.lock().map_err(|e| e.to_string())?;
            st.book
                .get(id)
                .map(|a| a.since_submit_sec(crate::crypto::envelope::now_ms() as i64))
                .unwrap_or(i64::MAX)
        };
        if !dry && since < crate::api::flow::SILENCE_SEC {
            results.push(json!({
                "id": id, "ok": false,
                "error": format!("距上次提交 {since}s，静默窗口 {}s 未过", crate::api::flow::SILENCE_SEC)
            }));
            continue;
        }

        let p = crate::api::flow::RunParams {
            distance_m: distance_km * 1000.0,
            duration_sec: (duration_min * 60.0).round() as i64,
            sport_type,
            run_mode,
            start_at_ms,
            ..Default::default()
        };

        let logs = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let l2 = std::rc::Rc::clone(&logs);
        let mut log = move |s: &str| l2.borrow_mut().push(s.to_string());

        let outcome = if dry {
            crate::api::flow::dry_run(&client, &p, &mut log)
        } else {
            let mut confirm = |_o: &crate::api::flow::RunOutcome| true;
            crate::api::flow::run_full(&client, &p, &mut log, &mut confirm)
        };
        drop(log);

        match outcome {
            Ok(o) => {
                if o.submitted {
                    let mut st = state.lock().map_err(|e| e.to_string())?;
                    if let Some(a) = st.book.get_mut(id) {
                        a.last_submit_ms = crate::crypto::envelope::now_ms() as i64;
                    }
                    st.persist();
                }
                results.push(json!({
                    "id": id,
                    "ok": true,
                    "submitted": o.submitted,
                    "recordId": o.record_id,
                    "distance": o.real_length_m,
                    "steps": o.total_steps,
                    "logs": logs.borrow().clone(),
                }));
            }
            Err(e) => results.push(json!({
                "id": id, "ok": false, "error": e,
                "logs": logs.borrow().clone(),
            })),
        }
    }

    let ok = results.iter().filter(|r| r["ok"] == true).count();
    let submitted = results.iter().filter(|r| r["submitted"] == true).count();
    json_ok(request, json!({
        "total": results.len(),
        "ok": ok,
        "submitted": submitted,
        "results": results,
    }))
}


// ---------- 补跑时刻 ----------

/// 算补跑时刻。查询参数：`mode`=at|ago|rand，`hhmm`、`days`、`minutes`、`dur`。
///
/// 只算不跑，前端拿到结果给用户确认；确认后再带着 `startAtMs` 调 `/api/run`。
fn handle_starttimes(request: tiny_http::Request) -> Result<(), String> {
    use crate::api::start_time as st;

    let q = request.url().split_once('?').map(|x| x.1).unwrap_or("");
    let mut get = |k: &str| -> Option<String> {
        q.split('&').find_map(|kv| {
            let (a, b) = kv.split_once('=')?;
            if a == k {
                Some(b.replace("%3A", ":").replace("%20", " ").to_string())
            } else {
                None
            }
        })
    };

    let mode = get("mode").unwrap_or_else(|| "now".into());
    let dur_s: i64 = get("dur").and_then(|v| v.parse().ok()).unwrap_or(720);
    let days: i64 = get("days").and_then(|v| v.parse().ok()).unwrap_or(0);
    let now = crate::crypto::envelope::now_ms() as i64;
    let tz = st::default_tz_offset_min();

    let result = match mode.as_str() {
        "at" => {
            let hhmm = get("hhmm").unwrap_or_else(|| "07:30".into());
            match st::parse_hhmm(&hhmm) {
                Ok((h, m)) => st::specified(now, days, h, m, dur_s),
                Err(e) => return json_err(request, &e),
            }
        }
        "ago" => {
            let mins: i64 = get("minutes").and_then(|v| v.parse().ok()).unwrap_or(60);
            st::ago(now, mins, dur_s)
        }
        "rand" => {
            use rand::Rng;
            let r: f64 = rand::thread_rng().gen();
            st::random_in_day(now, days, dur_s, r)
        }
        // 现在跑就是现在跑，不参与钳制（前端也不会拿它去弹确认框）
        "now" => Ok(st::StartTime { ms: now, clamped: false }),
        other => return json_err(request, &format!("未知模式 {other}")),
    };

    match result {
        Ok(x) => json_ok(request, json!({
            "mode": mode,
            "startMs": x.ms,
            "stopMs": x.stop_ms(dur_s),
            "startText": st::format_ms(x.ms, tz),
            "stopText": st::format_ms(x.stop_ms(dur_s), tz),
            "clamped": x.clamped,
            "durationSec": dur_s,
            "nowText": st::format_ms(now, tz),
            "limits": {
                "maxDaysAgo": st::MAX_DAYS_AGO,
                "maxAgoMin": st::MAX_AGO_MIN,
                "randHourLo": st::RAND_HOUR_LO,
                "randHourHi": st::RAND_HOUR_HI,
            },
        })),
        Err(e) => json_err(request, &e),
    }
}

fn handle_semester(request: tiny_http::Request, state: Shared) -> Result<(), String> {
    let st = state.lock().map_err(|e| e.to_string())?;
    if !st.logged_in() {
        return json_err(request, "请先登录");
    }
    let Some(client) = st.client() else {
        return json_err(request, "当前账号没有可用会话，请重新登录");
    };

    let mut out = serde_json::Map::new();
    // 原始响应也带上，字段名不对时能直接看
    if let Ok(raw) = client.request(
        "POST",
        &format!("{}{}", crate::crypto::consts::hosts::RUN, crate::api::semester::SUMMARY_PATH),
        Some(&json!({})),
    ) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(d) = v.get("data") {
                out.insert("summaryRaw".into(), d.clone());
            }
        }
    }

    match crate::api::semester::fetch_summary(client) {
        Ok(sm) => {
            out.insert("summary".into(), json!({
                "sname": sm.sname,
                "semesterDis": sm.semester_dis,
                "semesterValidDis": sm.semester_valid_dis,
                "semesterCount": sm.semester_count,
                "semesterValidCount": sm.semester_valid_count,
                "validRatio": sm.valid_ratio(),
                "text": sm.summary(),
            }));
        }
        Err(e) => {
            out.insert("summaryError".into(), json!(e));
        }
    }
    if let Ok(raw) = client.request(
        "POST",
        &format!("{}{}", crate::crypto::consts::hosts::RUN, crate::api::semester::PERSONAL_PATH),
        Some(&json!({"runMode": 1})),
    ) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(d) = v.get("data") {
                out.insert("goalRaw".into(), d.clone());
            }
        }
    }

    match crate::api::semester::fetch_goal(client, 1) {
        Ok(g) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            out.insert("goal".into(), json!({
                "targetDis": g.target_dis,
                "targetCount": g.target_count,
                "daysLeft": g.days_left(now),
                "inTerm": g.in_term(now),
            }));
        }
        Err(e) => {
            out.insert("goalError".into(), json!(e));
        }
    }
    json_ok(request, Value::Object(out))
}

// ---------- 围栏列表（地图上画出来） ----------

fn handle_fences(
    request: tiny_http::Request,
    state: Shared,
    _query: &str,
) -> Result<(), String> {
    let st = state.lock().map_err(|e| e.to_string())?;
    if !st.logged_in() {
        return json_err(request, "请先登录");
    }
    let Some(client) = st.client() else {
        return json_err(request, "当前账号没有可用会话，请重新登录");
    };
    match crate::api::fence::fetch_fences(client) {
        Ok(fs) => {
            let list: Vec<Value> = fs
                .iter()
                .map(|f| json!({
                    "id": f.id,
                    "name": f.name,
                    "ring": f.ring,
                }))
                .collect();
            json_ok(request, json!({"count": list.len(), "fences": list}))
        }
        Err(e) => json_err(request, &format!("拉围栏失败: {e}")),
    }
}

// ---------- 真跑：提交 ----------

fn handle_run(
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

    if distance_km <= 0.0 || distance_km > 42.0 {
        return json_err(request, "距离需在 0~42 公里之间");
    }
    if duration_min <= 0.0 || duration_min > 300.0 {
        return json_err(request, "时长需在 0~300 分钟之间");
    }

    let mut st = state.lock().map_err(|e| e.to_string())?;
    if !st.logged_in() {
        return json_err(request, "请先登录");
    }
    let Some(client) = st.client() else {
        return json_err(request, "当前账号没有可用会话，请重新登录");
    };

    // 中心点：没传就用围栏中心反推
    let (clat, clng) = match (lat, lng) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            match crate::api::fence::fetch_fences(client) {
                Ok(fs) if !fs.is_empty() && !fs[0].ring.is_empty() => {
                    let r = &fs[0].ring;
                    let n = r.len() as f64;
                    let s: (f64, f64) = r.iter().fold((0.0, 0.0), |a, q| (a.0 + q.0, a.1 + q.1));
                    (s.0 / n, s.1 / n)
                }
                _ => (30.5, 104.0),
            }
        }
    };

    let p = crate::api::flow::RunParams {
        center_lat: clat,
        center_lng: clng,
        distance_m: distance_km * 1000.0,
        duration_sec: (duration_min * 60.0).round() as i64,
        sport_type,
        run_mode,
        start_at_ms,
        ..Default::default()
    };

    // 日志用共享容器，闭包借用结束后还能读
    let logs: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
        std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let logs2 = std::rc::Rc::clone(&logs);
    let mut log = move |s: &str| logs2.borrow_mut().push(s.to_string());
    let mut confirm = |_o: &crate::api::flow::RunOutcome| true;

    let result = crate::api::flow::run_full(client, &p, &mut log, &mut confirm);
    drop(log);

    match result {
        Ok(o) => json_ok(request, json!({
            "submitted": o.submitted,
            "recordId": o.record_id,
            "distance": o.real_length_m,
            "steps": o.total_steps,
            "cadence": o.avg_cadence,
            "points": o.track_points,
            "obsKeys": o.obs_keys.len(),
            "logs": logs.borrow().clone(),
        })),
        Err(e) => json_err(request, &format!("{e}\n---\n{}", logs.borrow().join("\n"))),
    }
}

fn handle_records(request: tiny_http::Request, state: Shared, query: &str) -> Result<(), String> {
    let page = query_param(query, "page").and_then(|v| v.parse().ok()).unwrap_or(1u32);
    let size = query_param(query, "size").and_then(|v| v.parse().ok()).unwrap_or(20u32);
    let sport = query_param(query, "type").and_then(|v| v.parse().ok()).unwrap_or(1u32);

    let st = state.lock().map_err(|e| e.to_string())?;
    if !st.logged_in() {
        return json_err(request, "请先登录");
    }
    let Some(client) = st.client() else {
        return json_err(request, "当前账号没有可用会话，请重新登录");
    };
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
    let Some(client) = st.client() else {
        return json_err(request, "当前账号没有可用会话，请重新登录");
    };
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
