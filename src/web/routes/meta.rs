//! 元信息与调试路由：学期、开跑时刻、设备口径、出口、网络自检、协议透传。

use serde_json::{Value, json};

use crate::web::http::{host_short, json_err, json_ok, percent_encode, query_param};
use crate::web::state::{Shared, device_scope, lock_state};

// ---------- 补跑时刻 ----------

/// 算补跑时刻。查询参数：`mode`=at|ago|rand，`hhmm`、`days`、`minutes`、`dur`。
///
/// 只算不跑，前端拿到结果给用户确认；确认后再带着 `startAtMs` 调 `/api/run`。
pub(crate) fn handle_starttimes(request: tiny_http::Request) -> Result<(), String> {
    use crate::api::start_time as st;

    let q = request.url().split_once('?').map(|x| x.1).unwrap_or("");
    let get = |k: &str| -> Option<String> {
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
            let r: f64 = rand::rng().random();
            st::random_in_day(now, days, dur_s, r)
        }
        // 现在跑就是现在跑，不参与钳制（前端也不会拿它去弹确认框）
        "now" => Ok(st::StartTime {
            ms: now,
            clamped: false,
        }),
        other => return json_err(request, &format!("未知模式 {other}")),
    };

    match result {
        Ok(x) => json_ok(
            request,
            json!({
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
            }),
        ),
        Err(e) => json_err(request, &e),
    }
}

pub(crate) fn handle_semester(request: tiny_http::Request, state: Shared) -> Result<(), String> {
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

    let mut out = serde_json::Map::new();
    // 原始响应也带上，字段名不对时能直接看。
    // 这里必须和 fetch_summary 一样带 sid：空 body 服务端回的是空学期
    // （sname=""、全部 0），和下面的 summary 对不上。
    let sid = crate::api::semester::fetch_goal(&client, 1)
        .ok()
        .map(|g| g.sid)
        .filter(|s| *s > 0);
    let raw_body = match sid {
        Some(s) => json!({ "sid": s }),
        None => json!({}),
    };
    if let Ok(raw) = client.request(
        "POST",
        &format!(
            "{}{}",
            crate::crypto::consts::hosts::RUN,
            crate::api::semester::SUMMARY_PATH
        ),
        Some(&raw_body),
    ) && let Ok(v) = serde_json::from_str::<Value>(&raw)
        && let Some(d) = v.get("data")
    {
        out.insert("summaryRaw".into(), d.clone());
    }

    match crate::api::semester::fetch_summary(&client) {
        Ok(sm) => {
            out.insert(
                "summary".into(),
                json!({
                    "sname": sm.sname,
                    "semesterDis": sm.semester_dis,
                    "semesterValidDis": sm.semester_valid_dis,
                    "semesterCount": sm.semester_count,
                    "semesterValidCount": sm.semester_valid_count,
                    "validRatio": sm.valid_ratio(),
                    "text": sm.summary(),
                }),
            );
        }
        Err(e) => {
            out.insert("summaryError".into(), json!(e));
        }
    }
    if let Ok(raw) = client.request(
        "POST",
        &format!(
            "{}{}",
            crate::crypto::consts::hosts::RUN,
            crate::api::semester::PERSONAL_PATH
        ),
        Some(&json!({"runMode": 1})),
    ) && let Ok(v) = serde_json::from_str::<Value>(&raw)
        && let Some(d) = v.get("data")
    {
        out.insert("goalRaw".into(), d.clone());
    }

    match crate::api::semester::fetch_goal(&client, 1) {
        Ok(g) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            out.insert(
                "goal".into(),
                json!({
                    "targetDis": g.target_dis,
                    "targetCount": g.target_count,
                    "daysLeft": g.days_left(now),
                    "inTerm": g.in_term(now),
                    // 2 = 按里程考核，次数两项服务端给 0；前端据此说明而不是显示成 0 次
                    "goalMethod": g.goal_method,
                }),
            );
        }
        Err(e) => {
            out.insert("goalError".into(), json!(e));
        }
    }
    json_ok(request, Value::Object(out))
}

/// 按 rrid 从 OBS 拉历史记录的原始轨迹。
///
/// 轨迹不在 `get_one_record` 里返回（实测 50 条记录该字段全为 null），
/// 但记录列表带了 uuid 和 startTime，拼出 objectKey 就能直接从 OBS 取。
/// App 自己走不走这条路还没定论，见 docs/notes/TRAJECTORY-DISPLAY.md。
/// 读本地手绘禁区。
/// 保存出口配置。改完要重建所有 client——agent 是建 client 时创建的，
/// 不重建的话新出口要到下次重启才生效。
pub(crate) fn handle_egress_put(
    request: tiny_http::Request,
    state: Shared,
    body: &Value,
) -> Result<(), String> {
    use crate::api::egress;
    let spec = body["proxy"]
        .as_str()
        .map(str::trim)
        .filter(|p| !p.is_empty());
    if let Err(e) = egress::save_proxy(spec) {
        return json_err(request, &e);
    }
    let n = {
        let mut st = lock_state(&state);
        st.rebuild_all_clients()
    };
    json_ok(
        request,
        json!({
            "proxy": egress::current_proxy(),
            "desc": egress::proxy_description(),
            "rebuilt": n,
            "note": if spec.is_some() {
                "已保存，提交链路下一次请求起走这个出口"
            } else {
                "已清除，回到直连"
            },
        }),
    )
}

/// 当前账号的机型配置（`?q=` 有值时顺带搜官方目录）。
pub(crate) fn handle_device_get(
    request: tiny_http::Request,
    state: Shared,
    query: &str,
) -> Result<(), String> {
    use crate::api::{client, device_table};
    let (scope, cur_model) = {
        let st = lock_state(&state);
        match st.current_account() {
            Some(a) => (
                device_scope(&a.id),
                client::account_model(&device_scope(&a.id)),
            ),
            None => (String::new(), None),
        }
    };
    let cpu = client::resolve_cpu_model(cur_model.as_deref());
    let hit = cur_model
        .as_deref()
        .and_then(device_table::catalog_lookup)
        .map(|e| {
            json!({
                "name": e.name, "brand": e.brand, "codename": e.codename
            })
        });
    let mut out = json!({
        "scope": scope,
        "model": cur_model,
        "cpuModel": cpu,
        "abiFallback": cpu == device_table::ABI_FALLBACK,
        "catalogHit": hit,
        "devices": device_table::catalog().len(),
    });
    // 三种查询：?brand= 取该品牌机型；?q= 关键词搜；都没有就列品牌 + 平台。
    let q = query_param(query, "q").unwrap_or_default();
    let brand = query_param(query, "brand").unwrap_or_default();
    let list = if !brand.trim().is_empty() {
        device_table::models_of_brand(&brand, 300)
    } else if q.trim().is_empty() {
        Vec::new()
    } else {
        device_table::search_catalog(&q, 12)
    };
    let found: Vec<Value> = list
        .into_iter()
        .map(|(code, e)| {
            json!({"model": code, "name": e.name, "brand": e.brand, "codename": e.codename})
        })
        .collect();
    out["results"] = json!(found);
    if brand.trim().is_empty() && q.trim().is_empty() {
        let brands: Vec<Value> = device_table::brands()
            .into_iter()
            .map(|(name, count)| json!({"name": name, "count": count}))
            .collect();
        let platforms: Vec<Value> = device_table::platforms()
            .into_iter()
            .map(|(code, soc, name, vendor)| {
                json!({"code": code, "soc": soc, "name": name, "vendor": vendor})
            })
            .collect();
        out["brands"] = json!(brands);
        out["platforms"] = json!(platforms);
        out["suggested"] = json!(
            device_table::suggested(9)
                .into_iter()
                .map(|(code, e)| json!({"model": code, "name": e.name, "brand": e.brand}))
                .collect::<Vec<_>>()
        );
    }
    json_ok(request, out)
}

/// 设置当前账号的机型。`model` 为空/null 表示清掉（回到 ABI）。
pub(crate) fn handle_device_put(
    request: tiny_http::Request,
    state: Shared,
    body: &Value,
) -> Result<(), String> {
    use crate::api::client;
    let scope = {
        let st = lock_state(&state);
        match st.current_account() {
            Some(a) => device_scope(&a.id),
            None => return json_err(request, "请先登录再设置机型"),
        }
    };
    let model = body["model"]
        .as_str()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    if let Some(m) = model
        && (m.len() > 64 || m.chars().any(|c| c.is_control()))
    {
        return json_err(request, "机型 code 不合法（最长 64，不能含控制字符）");
    }
    // 给了平台代号就顺手记一条用户级映射（内置表查不到 SoC 时用得上）
    if let (Some(m), Some(plat)) = (
        model,
        body["platform"]
            .as_str()
            .map(str::trim)
            .filter(|p| !p.is_empty()),
    ) && let Err(e) = crate::api::device_table::set_user_model(m, plat)
    {
        return json_err(request, &e);
    }
    if let Err(e) = client::save_account_model(&scope, model) {
        return json_err(request, &e);
    }
    // 重建当前 client，让新的 cpuModel 立刻生效（下一次请求就是新机型）
    {
        let mut st = lock_state(&state);
        st.rebuild_client(&scope);
    }
    let cpu = client::resolve_cpu_model(model);
    let hit = model
        .and_then(crate::api::device_table::catalog_lookup)
        .map(|e| json!({"name": e.name, "brand": e.brand, "codename": e.codename}));
    json_ok(
        request,
        json!({
            "model": model,
            "cpuModel": cpu,
            "abiFallback": cpu == crate::api::device_table::ABI_FALLBACK,
            "catalogHit": hit,
            "note": "只改 cpuModel，不改 device_id；已生效到下一次请求"
        }),
    )
}

/// 透传任意业务接口，用于探测没写进代码的接口。
///
/// body: {"path":"/api/v670/...","body":{...},"host":"run|discovery"}
pub(crate) fn handle_raw(
    request: tiny_http::Request,
    state: Shared,
    body: &Value,
) -> Result<(), String> {
    let path = body.get("path").and_then(|v| v.as_str()).unwrap_or("");
    if !path.starts_with("/api/") {
        return json_err(request, "path 必须是 /api/ 开头");
    }
    let inner = body.get("body").cloned().unwrap_or_else(|| json!({}));
    let host = match body.get("host").and_then(|v| v.as_str()).unwrap_or("run") {
        "discovery" => crate::crypto::consts::hosts::DISCOVERY,
        _ => crate::crypto::consts::hosts::RUN,
    };

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
    let method = match body
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("POST")
    {
        "GET" => "GET",
        _ => "POST",
    };
    let full = if method == "GET" && !inner.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        let qs: Vec<String> = inner
            .as_object()
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| match v {
                        Value::String(sv) => Some(format!("{k}={}", sv.replace(' ', "%20"))),
                        Value::Number(n) => Some(format!("{k}={n}")),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if qs.is_empty() {
            format!("{host}{path}")
        } else {
            format!("{host}{path}?{}", qs.join("&"))
        }
    } else {
        format!("{host}{path}")
    };
    let payload = if method == "GET" { None } else { Some(&inner) };
    match client.request(method, &full, payload) {
        Ok(raw) => {
            let v: Value = serde_json::from_str(&raw).unwrap_or_else(
                |_| json!({"raw_text": raw.chars().take(2000).collect::<String>()}),
            );
            json_ok(request, v)
        }
        Err(e) => json_err(request, &e),
    }
}

/// 网络自检：分别探 Overpass 三个端点和高德瓦片，各自报耗时与错误。
///
/// 生成轨迹卡在「拉路网 / 建筑 / 瓦片」时，最需要回答的是「到底哪条不通」。
/// 这里只做连通性探测（一个空 bbox 的极小查询、一张瓦片），不参与生成，
/// 也不写缓存，失败不影响任何功能。
pub(crate) fn handle_netcheck(request: tiny_http::Request) -> Result<(), String> {
    use std::time::{Duration, Instant};
    let mut items: Vec<Value> = Vec::new();

    // Overpass：空 bbox 查询，只验证能否拿到 elements。
    let enc = percent_encode("[out:json][timeout:5];node(0,0,0.01,0.01);out;");
    for host in crate::track::roadnet::OVERPASS_HOSTS {
        let t0 = Instant::now();
        let (ok, detail) = match ureq::get(&format!("{host}?data={enc}"))
            .set("User-Agent", "pace-forge/0.1 (netcheck)")
            .timeout(Duration::from_secs(8))
            .call()
        {
            Ok(r) => match r.into_string() {
                Ok(t) if t.contains("\"elements\"") => (true, "连通".to_string()),
                Ok(_) => (false, "返回内容异常".to_string()),
                Err(e) => (false, format!("读取失败: {e}")),
            },
            Err(e) => (false, format!("{e}")),
        };
        items.push(json!({
            "name": format!("Overpass {}", host_short(host)),
            "ok": ok,
            "ms": t0.elapsed().as_millis() as u64,
            "detail": detail
        }));
    }

    // 高德瓦片：拿一张固定瓦片（北京附近），只验证能不能下载到图。
    let tile = "https://webrd01.is.autonavi.com/appmaptile?lang=zh_cn&size=1&scale=1&style=8&x=3360&y=1558&z=17";
    let t0 = Instant::now();
    let (ok, detail) = match ureq::get(tile)
        .set("User-Agent", "Mozilla/5.0 (compatible; pace-forge/0.1)")
        .set("Referer", "https://www.amap.com/")
        .timeout(Duration::from_secs(6))
        .call()
    {
        Ok(r) => {
            use std::io::Read as _;
            let mut rd = r.into_reader().take(4096);
            let mut buf = Vec::new();
            match rd.read_to_end(&mut buf) {
                Ok(_) if buf.len() > 512 => (true, "连通".to_string()),
                Ok(_) => (false, "返回内容过短（可能被拦截）".to_string()),
                Err(e) => (false, format!("读取失败: {e}")),
            }
        }
        Err(e) => (false, format!("{e}")),
    };
    items.push(json!({
        "name": "高德瓦片 webrd01",
        "ok": ok,
        "ms": t0.elapsed().as_millis() as u64,
        "detail": detail
    }));

    let all_ok = items.iter().all(|i| i["ok"].as_bool().unwrap_or(false));
    let hint = if all_ok {
        "链路都通：卡住就不是网络问题，看结果面板里的「[耗时]」行定位在哪一段"
    } else {
        "有一项不通：对应那一步会等满超时才放弃，生成因此变慢；日志里的 [roads]/[block]/[tiles] 就是它"
    };
    json_ok(
        request,
        json!({"allOk": all_ok, "hint": hint, "items": items, "version": crate::VERSION}),
    )
}
