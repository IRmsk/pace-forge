//! 账号路由：账号库增删、会话切换、批量提交、当前账号会话。

use serde_json::{Value, json};

use crate::api::client::{Client, DeviceProfile};
use crate::api::login::{LoginOutcome, login};
use crate::api::session;
use crate::web::http::{json_err, json_ok};
use crate::web::routes::run::parse_alt_range;
use crate::web::state::{AccountSessionAction, Shared, lock_state, remote_logout};

// ---------- 记分跑：学期进度 ----------

// ---------- 账号管理 ----------

pub(crate) fn account_view(a: &crate::api::accounts::Account, current: bool) -> Value {
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

pub(crate) fn handle_accounts(request: tiny_http::Request, state: Shared) -> Result<(), String> {
    let st = lock_state(&state);
    let cur = st.current.clone().unwrap_or_default();
    let list: Vec<Value> = st
        .book
        .accounts
        .iter()
        .map(|a| account_view(a, a.id == cur))
        .collect();
    json_ok(
        request,
        json!({
            "count": list.len(),
            "ready": st.book.ready().len(),
            "current": cur,
            "accounts": list,
        }),
    )
}

pub(crate) fn handle_add_account(
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
            let mut st = lock_state(&state);
            let id = if profile.uid.is_empty() {
                username.clone()
            } else {
                profile.uid.clone()
            };
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
            json_ok(
                request,
                json!({
                    "id": id,
                    "name": profile.name,
                    "school": profile.campus_name,
                    "uid": profile.uid,
                }),
            )
        }
        Ok(LoginOutcome::NeedCaptcha { uuid, message }) => {
            json_err(request, &format!("{message}（uuid={uuid}）"))
        }
        Err(e) => json_err(request, &e),
    }
}

pub(crate) fn handle_del_account(
    request: tiny_http::Request,
    state: Shared,
    id: &str,
    force: bool,
) -> Result<(), String> {
    // 先退出登录，再从本地删掉。顺序不能反：直接删只是丢掉本地凭据，服务端
    // 会话还活着，账号仍被标记「已登录」——下次登录触发 10121 并锁一小时，
    // 用户看到的就是「删完就锁我」。
    //
    // 关键改动：以前不管退出成功与否都照删。但「服务端没确认退出」和「服务端
    // 本来就没有活会话」在返回码上分不开，一律照删的结果是把「确保不会锁」
    // 这件事交给了运气。现在有存盘 token 且退出没成功时**先不删**，把后果
    // 交给用户确认（`?force=1` 才跳过），这样「删完锁一小时」不会再意外发生。
    let mut st = lock_state(&state);
    let Some(account) = st.book.get(id) else {
        return json_ok(
            request,
            json!({"removed": false, "remoteLogout": false, "needConfirm": false}),
        );
    };
    let had_token = !account.token.is_empty();
    let remote = remote_logout(&mut st, id);

    if had_token && !remote && !force {
        return json_ok(
            request,
            json!({
                "removed": false,
                "remoteLogout": false,
                "needConfirm": true,
                "warning": "服务端没有确认退出登录（可能是网络或接口失败），现在删掉的话服务端会话可能还在，重新登录会锁一小时。建议先点「退出」，成功了再删。",
            }),
        );
    }

    if let Err(error) =
        st.end_account_session(&session::data_dir(), id, AccountSessionAction::Delete)
    {
        return json_err(request, &error);
    }
    json_ok(
        request,
        json!({"removed": true, "remoteLogout": remote, "needConfirm": false}),
    )
}

/// 按账号退出登录：只清登录态，保留手机号和存盘密码。
///
/// 顶部的「退出」选中的是当前账号，这里选中的是列表中的某一行。
/// 两个入口都会清理该账号的兼容会话，保留账号与密码。
///
/// 同样要先调服务端 `/api/v6/user/logout`。只清本地 token 的话服务端
/// 会话还活着，账号被标记「已登录」，再登触发 10121 并锁一小时。
pub(crate) fn handle_logout_one(
    request: tiny_http::Request,
    state: Shared,
    id: &str,
) -> Result<(), String> {
    let mut st = lock_state(&state);
    let remote = remote_logout(&mut st, id);
    let was_current =
        match st.end_account_session(&session::data_dir(), id, AccountSessionAction::Logout) {
            Ok(was_current) => was_current,
            Err(error) => return json_err(request, &error),
        };
    json_ok(
        request,
        json!({"ok": true, "remoteLogout": remote, "wasCurrent": was_current}),
    )
}

pub(crate) fn handle_switch(
    request: tiny_http::Request,
    state: Shared,
    id: &str,
) -> Result<(), String> {
    let mut st = lock_state(&state);
    if st.book.get(id).is_none() {
        return json_err(request, "账号不存在");
    }
    st.current = Some(id.to_string());
    json_ok(request, json!({"current": id}))
}

pub(crate) fn handle_toggle(
    request: tiny_http::Request,
    state: Shared,
    id: &str,
) -> Result<(), String> {
    let mut st = lock_state(&state);
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
pub(crate) fn handle_login_one(
    request: tiny_http::Request,
    state: Shared,
    id: &str,
) -> Result<(), String> {
    let (username, password) = {
        let st = lock_state(&state);
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
            let mut st = lock_state(&state);
            st.apply_login(id, &profile);
            json_ok(
                request,
                json!({"id": id, "name": profile.name, "uid": profile.uid}),
            )
        }
        Ok(LoginOutcome::NeedCaptcha { message, .. }) => json_err(request, &message),
        Err(e) => json_err(request, &e),
    }
}

/// 批量登录所有账号。
pub(crate) fn handle_login_all(request: tiny_http::Request, state: Shared) -> Result<(), String> {
    let todo: Vec<(String, String, String)> = {
        let st = lock_state(&state);
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
                let mut st = lock_state(&state);
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
    json_ok(
        request,
        json!({"total": results.len(), "ok": ok, "results": results}),
    )
}

/// 批量刷：所有就绪账号依次跑一遍。
pub(crate) fn handle_batch(
    request: tiny_http::Request,
    state: Shared,
    body: &Value,
) -> Result<(), String> {
    let distance_km = body["distance"].as_f64().unwrap_or(2.0);
    let duration_min = body["duration"].as_f64().unwrap_or(12.0);
    let run_mode = body["runMode"].as_i64().unwrap_or(1);
    let start_at_ms = body["startAtMs"].as_i64().filter(|v| *v > 0);
    let sport_type = body["sportType"].as_i64().unwrap_or(1) as i32;
    // 跑之前必须确认「当前会话是谁」。
    //
    // 出过事：中途登录过另一个号做对比，回来忘了切，三条记录落到别人账号下，
    // 本地又没有删除接口，只能去 App 里手动删。提交是不可撤销的操作，宁可在
    // 这里多问一句——调用方明确给了 `expectUid` 就核对，对不上直接拒。
    if let Some(want) = body["expectUid"].as_str().filter(|v| !v.is_empty()) {
        let now = {
            let st = lock_state(&state);
            st.current.clone()
        };
        let now_str = now.clone().unwrap_or_else(|| "（未登录）".into());
        if now.as_deref() != Some(want) {
            return json_err(
                request,
                &format!("当前登录的是 {now_str}，不是 expectUid 指定的 {want}，已拒绝提交"),
            );
        }
    }
    let dry = body["dry"].as_bool().unwrap_or(false);
    // 账号间隔（毫秒），默认 8 秒，避免同 IP 高频
    let gap_ms = body["gapMs"].as_u64().unwrap_or(8000);
    let only: Vec<String> = body["ids"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    if distance_km <= 0.0 || distance_km > 42.0 {
        return json_err(request, "距离需在 0~42 公里之间");
    }
    if duration_min <= 0.0 || duration_min > 120.0 {
        return json_err(request, "时长需在 0~120 分钟之间（超过服务端必然驳回）");
    }

    // 挑出要跑的账号
    let targets: Vec<String> = {
        let st = lock_state(&state);
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
            let st = lock_state(&state);
            st.client_of(id).cloned()
        };
        let Some(client) = client else {
            results.push(json!({"id": id, "ok": false, "error": "无会话"}));
            continue;
        };

        // 静默窗口
        let since = {
            let st = lock_state(&state);
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
            free_run: sport_type != 1,
            start_at_ms,
            alt_range: parse_alt_range(body),
            prefer_stadium: body["mode"].as_str() == Some("stadium"),
            prefer_roads: body["preferRoads"].as_bool().unwrap_or(true),
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
                    let mut st = lock_state(&state);
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
    json_ok(
        request,
        json!({
            "total": results.len(),
            "ok": ok,
            "submitted": submitted,
            "results": results,
        }),
    )
}

/// 当前登录账号摘要。`GET /api/me`
pub(crate) fn handle_me(request: tiny_http::Request, state: Shared) -> Result<(), String> {
    let st = lock_state(&state);
    match st.current_account() {
        Some(a) => json_ok(
            request,
            json!({
                "id": a.id, "uid": a.uid, "unid": a.unid,
                "name": a.name, "username": a.username, "school": a.school,
                "loggedIn": a.logged_in(),
            }),
        ),
        None => json_err(request, "未登录"),
    }
}

/// 退出当前账号：先通知服务端让 token 失效，再清本地。
///
/// 顺序不能反：只清本地的话服务端会话还活着，账号被标记为已登录，
/// 再登录会拿到 10121（已在其他设备登录），而且锁一小时。
/// `POST /api/logout`
pub(crate) fn handle_logout_current(
    request: tiny_http::Request,
    state: Shared,
) -> Result<(), String> {
    let mut st = lock_state(&state);
    let Some(id) = st.current.clone() else {
        return json_ok(request, json!({"ok": true, "remoteLogout": false}));
    };
    let remote = st
        .clients
        .get_mut(&id)
        .is_some_and(|client| crate::api::login::logout(client).unwrap_or(false));
    if let Err(error) =
        st.end_account_session(&session::data_dir(), &id, AccountSessionAction::Logout)
    {
        return json_err(request, &error);
    }
    json_ok(request, json!({"ok": true, "remoteLogout": remote}))
}
