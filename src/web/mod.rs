//! Web 控制台：登录、记录列表、轨迹地图。
//!
//! 设计上刻意保持薄：
//! - 会话状态只存一份 `Client`（含 uid/token/unid），进程退出即失效
//! - 不落盘密码，登录成功后密码直接丢弃
//! - 所有业务逻辑复用 `api/*`，Web 层只做转发与 JSON 序列化
//! - 本文件只保留 serve 循环、在跑上限、panic 隔离和路由分发；
//!   鉴权 / HTTP 工具 / 静态资源 / 各域 handler 各归各的模块
//!
//! 轨迹字段问题：服务端记录详情里字段名不稳定（不同版本、不同接口各有差异），
//! 所以后端不解析轨迹，原样返回 `raw`，由前端递归探测坐标数组。这样即使
//! 服务端改字段名，地图仍然能画出来。

mod assets;
mod console_auth;
mod http;
mod routes;
mod state;

use serde_json::json;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Response, Server};

use assets::{INDEX_HTML, STATIC_ASSETS, respond_static};
use console_auth::{auth_token, check_auth, origin_matches_host};
use http::{json_err, json_err_code, json_ok, query_param, read_json};
use routes::accounts::{
    handle_accounts, handle_add_account, handle_batch, handle_del_account, handle_login_all,
    handle_login_one, handle_logout_current, handle_logout_one, handle_me, handle_switch,
    handle_toggle,
};
use routes::login::handle_login;
use routes::map_data::{handle_blockers_get, handle_blockers_put, handle_fences, handle_points};
use routes::meta::{
    handle_device_get, handle_device_put, handle_egress_put, handle_netcheck, handle_raw,
    handle_semester, handle_starttimes,
};
use routes::records::{handle_cheat, handle_obs_fetch, handle_record_detail, handle_records};
use routes::run::{handle_plan, handle_run};
use state::{Shared, State};

/// 同时在处理的请求上限。
///
/// 控制台是本机工具，几十个并发已经远超正常使用。绑 `0.0.0.0` 暴露到局域网
/// 后，扫描器或慢速连接会一条条堆线程（tiny_http 没有读超时），线程数无上限
/// 时内存跟着涨。超限直接回 503，让新连接快速失败，而不是继续排队。
const MAX_INFLIGHT: usize = 64;

static INFLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 请求处理名额：拿到才算被允许起线程，线程结束自动归还。
struct InflightGuard;

impl Drop for InflightGuard {
    fn drop(&mut self) {
        INFLIGHT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn try_admit() -> Option<InflightGuard> {
    let prev = INFLIGHT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if prev >= MAX_INFLIGHT {
        INFLIGHT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        None
    } else {
        Some(InflightGuard)
    }
}

pub fn serve(addr: &str) -> Result<(), String> {
    let server = Server::http(addr).map_err(|e| format!("监听 {addr} 失败: {e}"))?;
    let state: Shared = Arc::new(Mutex::new(State::restore()));

    println!("Web 控制台已启动: http://{addr}");
    match auth_token() {
        Some(_) => println!("已启用口令（PACE_FORGE_TOKEN），远程访问需在请求头带 X-Pace-Token"),
        None => println!(
            "未设置 PACE_FORGE_TOKEN：只接受本机访问。\
             要让同网段设备访问，请先 `export PACE_FORGE_TOKEN=你的口令`"
        ),
    }
    println!("按 Ctrl+C 停止");

    for request in server.incoming_requests() {
        let Some(slot) = try_admit() else {
            eprintln!("请求超过 {MAX_INFLIGHT} 个在跑，直接回 503");
            let _ = request.respond(
                Response::from_string("{\"error\":\"控制台繁忙，请稍后重试\"}")
                    .with_status_code(503)
                    .with_header(
                        Header::from_bytes("Content-Type", "application/json; charset=utf-8")
                            .expect("静态头合法"),
                    ),
            );
            continue;
        };
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            let _slot = slot;
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

    if path.starts_with("/api/") {
        let token = auth_token();
        if let Err((code, msg)) = check_auth(&request, token.as_deref()) {
            return json_err_code(request, code, &msg);
        }
        // 没设口令时再挡一层跨站写：任何网页都能盲打 POST /api/run 或删账号，
        // 不需要读响应也能造成后果。读接口不拦，免得把正常浏览器访问误杀。
        let mutating = !matches!(method, Method::Get | Method::Head | Method::Options);
        if mutating && token.is_none() {
            let origin = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Origin"))
                .map(|h| h.value.as_str().to_string());
            let host = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Host"))
                .map(|h| h.value.as_str().to_string());
            if !origin_matches_host(origin.as_deref(), host.as_deref()) {
                return json_err(request, "拒绝跨站写请求：控制台默认只接受本机页面调用");
            }
        }
    }

    // 本地静态资源（css/js）与首页同级公开：不进 /api 的口令链，浏览器
    // 加载页面时就要拉它们。只拦 GET，写方法一律落到下面的 404/路由表。
    if let Method::Get = method
        && let Some((_, body, ctype)) = STATIC_ASSETS.iter().find(|(p, ..)| *p == path.as_str())
    {
        return respond_static(request, body, ctype);
    }

    match (method.clone(), path.as_str()) {
        (Method::Get, "/") => respond_static(request, INDEX_HTML, "text/html; charset=utf-8"),
        (Method::Post, "/api/login") => {
            // body 坏掉是客户端问题，必须回 400 + 具体原因；早先经 `?` 上抛
            // 到 serve 只写 stderr，tiny_http 兜底回空体 500，调用方无法区分
            // 「请求写错了」和「服务器崩了」。
            let body = match read_json(&mut request) {
                Ok(v) => v,
                Err(e) => return json_err_code(request, 400, &e),
            };
            handle_login(request, state, &body)
        }
        (Method::Get, "/api/me") => handle_me(request, state),
        (Method::Post, "/api/logout") => handle_logout_current(request, state),
        (Method::Post, "/api/plan") => {
            // body 坏掉是客户端问题，必须回 400 + 具体原因；早先经 `?` 上抛
            // 到 serve 只写 stderr，tiny_http 兜底回空体 500，调用方无法区分
            // 「请求写错了」和「服务器崩了」。
            let body = match read_json(&mut request) {
                Ok(v) => v,
                Err(e) => return json_err_code(request, 400, &e),
            };
            handle_plan(request, state, &body)
        }
        (Method::Get, "/api/accounts") => handle_accounts(request, state),
        (Method::Get, "/api/starttimes") => handle_starttimes(request),
        (Method::Post, "/api/accounts") => {
            // body 坏掉是客户端问题，必须回 400 + 具体原因；早先经 `?` 上抛
            // 到 serve 只写 stderr，tiny_http 兜底回空体 500，调用方无法区分
            // 「请求写错了」和「服务器崩了」。
            let body = match read_json(&mut request) {
                Ok(v) => v,
                Err(e) => return json_err_code(request, 400, &e),
            };
            handle_add_account(request, state, &body)
        }
        (Method::Post, "/api/accounts/login-all") => handle_login_all(request, state),
        (Method::Post, "/api/batch") => {
            // body 坏掉是客户端问题，必须回 400 + 具体原因；早先经 `?` 上抛
            // 到 serve 只写 stderr，tiny_http 兜底回空体 500，调用方无法区分
            // 「请求写错了」和「服务器崩了」。
            let body = match read_json(&mut request) {
                Ok(v) => v,
                Err(e) => return json_err_code(request, 400, &e),
            };
            handle_batch(request, state, &body)
        }
        (Method::Get, "/api/semester") => handle_semester(request, state),
        (Method::Post, "/api/run") => {
            // body 坏掉是客户端问题，必须回 400 + 具体原因；早先经 `?` 上抛
            // 到 serve 只写 stderr，tiny_http 兜底回空体 500，调用方无法区分
            // 「请求写错了」和「服务器崩了」。
            let body = match read_json(&mut request) {
                Ok(v) => v,
                Err(e) => return json_err_code(request, 400, &e),
            };
            handle_run(request, state, &body)
        }
        (Method::Get, "/api/fences") => handle_fences(request, state, &query),
        (Method::Get, "/api/points") => handle_points(request, state, &query),
        (Method::Get, "/api/records") => handle_records(request, state, &query),
        (Method::Post, "/api/egress") => {
            let body = match read_json(&mut request) {
                Ok(v) => v,
                Err(e) => return json_err_code(request, 400, &e),
            };
            handle_egress_put(request, state, &body)
        }
        (Method::Get, "/api/device") => handle_device_get(request, state, &query),
        (Method::Post, "/api/device") => {
            let body = match read_json(&mut request) {
                Ok(v) => v,
                Err(e) => return json_err_code(request, 400, &e),
            };
            handle_device_put(request, state, &body)
        }
        (Method::Get, "/api/cheat") => handle_cheat(request, state, &query),
        (Method::Get, "/api/blockers") => handle_blockers_get(request),
        (Method::Post, "/api/blockers") => {
            // body 坏掉是客户端问题，必须回 400 + 具体原因；早先经 `?` 上抛
            // 到 serve 只写 stderr，tiny_http 兜底回空体 500，调用方无法区分
            // 「请求写错了」和「服务器崩了」。
            let body = match read_json(&mut request) {
                Ok(v) => v,
                Err(e) => return json_err_code(request, 400, &e),
            };
            handle_blockers_put(request, &body)
        }
        (Method::Post, "/api/obs/fetch") => {
            // body 坏掉是客户端问题，必须回 400 + 具体原因；早先经 `?` 上抛
            // 到 serve 只写 stderr，tiny_http 兜底回空体 500，调用方无法区分
            // 「请求写错了」和「服务器崩了」。
            let body = match read_json(&mut request) {
                Ok(v) => v,
                Err(e) => return json_err_code(request, 400, &e),
            };
            handle_obs_fetch(request, state, &body)
        }
        (Method::Post, "/api/raw") => {
            // body 坏掉是客户端问题，必须回 400 + 具体原因；早先经 `?` 上抛
            // 到 serve 只写 stderr，tiny_http 兜底回空体 500，调用方无法区分
            // 「请求写错了」和「服务器崩了」。
            let body = match read_json(&mut request) {
                Ok(v) => v,
                Err(e) => return json_err_code(request, 400, &e),
            };
            handle_raw(request, state, &body)
        }
        (Method::Get, "/api/health") => {
            json_ok(request, json!({"ok": true, "version": crate::VERSION}))
        }
        // 出口 IP 自检（#10）：显示提交时服务端用的公网 IP，并标出是否走了代理。
        // 探测失败返回 ok:false，前端显示「出口未知」，不阻塞任何功能。
        (Method::Get, "/api/egress-ip") => json_ok(request, crate::api::egress::cached_json()),
        // 网络自检：生成卡住时用来回答「哪条链路不通」，只探连通性。
        (Method::Get, "/api/netcheck") => handle_netcheck(request),
        _ => {
            if let Some(rest) = path.strip_prefix("/api/accounts/") {
                // /api/accounts/{id}/switch  |  /api/accounts/{id}  (DELETE)
                let mut it = rest.splitn(2, '/');
                let id = it.next().unwrap_or("").to_string();
                let action = it.next().unwrap_or("");
                let force = query_param(&query, "force")
                    .map(|v| v == "1" || v == "true")
                    .unwrap_or(false);
                return match (method, action) {
                    (Method::Delete, "") => handle_del_account(request, state, &id, force),
                    (Method::Post, "switch") => handle_switch(request, state, &id),
                    (Method::Post, "logout") => handle_logout_one(request, state, &id),
                    (Method::Post, "login") => handle_login_one(request, state, &id),
                    (Method::Post, "toggle") => handle_toggle(request, state, &id),
                    _ => json_err_code(request, 404, "unknown account action"),
                };
            }
            if let Some(rest) = path.strip_prefix("/api/record/") {
                let rrid: i64 = match rest.parse() {
                    Ok(v) => v,
                    Err(_) => return json_err_code(request, 400, "rrid 不是数字"),
                };
                return handle_record_detail(request, state, rrid);
            }
            json_err_code(request, 404, "not found")
        }
    }
}

#[cfg(test)]
mod inflight_tests {
    use super::{INFLIGHT, MAX_INFLIGHT, try_admit};
    use std::sync::atomic::Ordering;

    /// 名额发满后新请求立刻被拒；处理完归还后名额恢复。
    #[test]
    fn inflight_cap_is_enforced_and_released() {
        assert_eq!(INFLIGHT.load(Ordering::SeqCst), 0, "测试开始时计数应为 0");
        let mut held = Vec::new();
        while let Some(g) = try_admit() {
            held.push(g);
        }
        assert_eq!(held.len(), MAX_INFLIGHT, "同时持有的名额应恰好等于上限");
        assert!(try_admit().is_none(), "满了以后不该再发名额");
        drop(held);
        assert_eq!(INFLIGHT.load(Ordering::SeqCst), 0, "全部归还后计数应归零");
        assert!(try_admit().is_some(), "归还后应立刻能再拿到名额");
    }
}
