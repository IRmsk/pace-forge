//! 记录路由：记录列表、单条详情（含轨迹原文）、OBS 直读、违规名单自查。

use serde_json::{Value, json};

use crate::api::records;
use crate::web::http::{json_err, json_ok, query_param};
use crate::web::state::{Shared, lock_state};

pub(crate) fn handle_obs_fetch(
    request: tiny_http::Request,
    state: Shared,
    body: &Value,
) -> Result<(), String> {
    let key = match body.get("key").and_then(|v| v.as_str()) {
        Some(k) if k.starts_with("run_data/") || k.starts_with("runData/") => k.to_string(),
        _ => return json_err(request, "key 参数缺失或格式不对"),
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
    match crate::api::obs::fetch_object(&client, &key) {
        Ok(v) => json_ok(request, v),
        Err(e) => json_err(request, &e),
    }
}

/// 违规名单自查。`GET /api/cheat?page=1`
///
/// 服务端跑批判定，和提交是否成功无关——提交成功也可能事后被标记。
/// 这个接口是唯一能自己确认的入口，所以结果要原样返回给前端，
/// 由前端区分「没有记录」和「结果未知」两种状态。
pub(crate) fn handle_cheat(
    request: tiny_http::Request,
    state: Shared,
    query: &str,
) -> Result<(), String> {
    let page = query_param(query, "page")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1i64);
    // 先克隆 client 再放锁：网络请求几十秒，持锁做 IO 会让整个面板假死。
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
    let mut lines: Vec<String> = Vec::new();
    match crate::api::cheat::query(&client, page, &mut |l| lines.push(l.to_string())) {
        Ok(report) => {
            for l in &lines {
                eprintln!("{l}");
            }
            let status = match report.status() {
                crate::api::cheat::Status::NoRecord => "no_record",
                crate::api::cheat::Status::Flagged => "flagged",
                crate::api::cheat::Status::Unknown => "unknown",
                crate::api::cheat::Status::Stale => "stale",
            };
            json_ok(
                request,
                json!({
                    "status": status,
                    "label": report.status().label(),
                    "count": report.count(),
                    "reason": report.reason(),
                    "appeal": report.appeal(),
                    "occurredAt": report.occurred_at(),
                    "rrid": report.record_id(),
                    "checkedAtMs": report.checked_at_ms,
                    "listCount": report.list.len(),
                }),
            )
        }
        Err(e) => json_err(request, &e),
    }
}

pub(crate) fn handle_records(
    request: tiny_http::Request,
    state: Shared,
    query: &str,
) -> Result<(), String> {
    let page = query_param(query, "page")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1u32);
    let size = query_param(query, "size")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20u32);
    let sport = query_param(query, "type")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1u32);

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
    match records::list_records(&client, page, size, sport) {
        Ok(data) => json_ok(request, data),
        Err(e) => json_err(request, &e),
    }
}

pub(crate) fn handle_record_detail(
    request: tiny_http::Request,
    state: Shared,
    rrid: i64,
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
    match records::get_record(&client, rrid) {
        Ok(data) => json_ok(request, data),
        Err(e) => json_err(request, &e),
    }
}
