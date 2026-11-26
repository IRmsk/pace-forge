//! 登录路由：账号密码登录（极验本地求解），成功后进入会话。

use serde_json::{Value, json};

use crate::api::client::{Client, DeviceProfile};
use crate::api::login::{LoginOutcome, login};
use crate::web::http::{json_err, json_ok};
use crate::web::state::{Shared, lock_state};

pub(crate) fn handle_login(
    request: tiny_http::Request,
    state: Shared,
    body: &Value,
) -> Result<(), String> {
    let username = body
        .get("username")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let mut password = body
        .get("password")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if username.is_empty() {
        return json_err(request, "手机号要填");
    }
    // 密码留空 = 用本地存着的那份重新登录。
    //
    // 会话被服务端踢掉（token 失效、`/api/me` 回「未登录」）是常事，但密码
    // 早就加密存盘了，不该让人每次重新敲一遍——尤其登录还要过极验滑块，
    // 过程本身就容易失败。这里只在**本地已有该账号**时才走这条捷径，
    // 没存过密码照样要求填。
    if password.is_empty() {
        let st = lock_state(&state);
        let enc = st
            .book
            .accounts
            .iter()
            .find(|a| a.username == username || a.id == username)
            .map(|a| a.password_enc.clone())
            .filter(|e| !e.is_empty());
        drop(st);
        match enc.and_then(|e| crate::api::accounts::open_password(&e).ok()) {
            Some(pw) => password = pw,
            None => return json_err(request, "手机号和密码都要填（本地没有这个账号的密码）"),
        }
    }

    let mut client = Client::new(DeviceProfile::default());
    match login(&mut client, &username, &password) {
        Ok(LoginOutcome::Ok(profile)) => {
            let mut st = lock_state(&state);
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
