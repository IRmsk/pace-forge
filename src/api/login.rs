//! 登录流程。
//!
//! ```text
//! 1. POST /api/v65/security/checkGeeUse   {username, uuid, unid:0, type:2}
//!    → data == true 表示当前设备免验证码，直接跳到第 3 步
//! 2. POST /api/v70270/security/geevalidate （极验滑块，需要外部求解器）
//! 3. POST /api/v70100/login               Basic Auth + 设备信息
//! ```
//!
//! 密码不单独加密，走 HTTP `Authorization: Basic base64(user:pass)`。

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};

use super::client::{expect_ok, Client, Credentials};

/// 极验滑块在登录场景下的固定 captcha_id。
pub const LOGIN_CAPTCHA_ID: &str = "3b02ad39bd099fd3a8336d9347a189ab";

/// 登录成功后的用户信息。
#[derive(Debug, Clone, serde::Serialize)]
pub struct UserProfile {
    pub uid: String,
    pub unid: String,
    pub token: String,
    pub name: String,
    pub username: String,
    pub campus_name: String,
    pub campus_id: String,
    pub depart: String,
    pub grade_class: String,
    pub sex: String,
}

fn s(v: Option<&Value>) -> String {
    v.map(|x| match x {
        Value::String(t) => t.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    })
    .unwrap_or_default()
}

impl UserProfile {
    pub fn from_data(d: &Value) -> Self {
        let sex = match d.get("sex").and_then(|x| x.as_i64()) {
            Some(1) => "男",
            Some(2) => "女",
            _ => "未知",
        };
        Self {
            uid: s(d.get("uid")),
            unid: s(d.get("unid")),
            token: s(d.get("token")),
            name: s(d.get("name")),
            username: s(d.get("username")),
            campus_name: s(d.get("campusName")),
            campus_id: s(d.get("campusId")),
            depart: s(d.get("depart")),
            grade_class: s(d.get("gradeClass")),
            sex: sex.into(),
        }
    }
}

/// 登录结果。
#[derive(Debug)]
pub enum LoginOutcome {
    /// 成功，已把凭据写回 client。
    Ok(UserProfile),
    /// 需要过极验滑块。`uuid` 要透传给求解器。
    NeedCaptcha { uuid: String, message: String },
}

/// 执行登录。成功时直接修改 `client.creds`。
pub fn login(
    client: &mut Client,
    username: &str,
    password: &str,
) -> Result<LoginOutcome, String> {
    let uuid = uuid_v4();

    // 1. 安全初审
    let chk = client.request(
        "POST",
        &format!("{}/api/v65/security/checkGeeUse", crate::crypto::hosts::RUN),
        Some(&json!({"username": username, "uuid": uuid, "unid": 0, "type": 2})),
    )?;
    let chk_v = expect_ok(&chk)?;
    let no_captcha = chk_v.get("data").and_then(|d| d.as_bool()).unwrap_or(false);

    // 2. 需要验证码：本地求解极验 v4，然后上报凭证
    if !no_captcha {
        let mut log = |msg: &str| eprintln!("{msg}");
        let solution = match super::gt4::solve_gt4(LOGIN_CAPTCHA_ID, &mut log) {
            Ok(v) => v,
            Err(e) => {
                return Ok(LoginOutcome::NeedCaptcha {
                    uuid,
                    message: format!("极验求解失败: {e}"),
                })
            }
        };
        let payload = json!({
            "lotNumber": solution["lotNumber"],
            "captchaOutput": solution["captchaOutput"],
            "passToken": solution["passToken"],
            "genTime": solution["genTime"],
            "isOffline": false,
            "osType": 0,
            "businessType": 0,
            "uuid": uuid,
            "username": username,
        });
        let val = client.request(
            "POST",
            &format!("{}/api/v70270/security/geevalidate", crate::crypto::hosts::RUN),
            Some(&payload),
        )?;
        let v = expect_ok(&val)?;
        // 服务端可能用 data 里的布尔值表示是否放行
        if v.get("data").and_then(|d| d.as_bool()) == Some(false) {
            return Ok(LoginOutcome::NeedCaptcha {
                uuid,
                message: "极验通过但二次校验未放行".into(),
            });
        }
    }

    // 3. 正式登录
    let auth = B64.encode(format!("{username}:{password}").as_bytes());
    let auth_header = format!("Basic {auth}");
    let body = json!({
        "loginType": 1,
        "device_model": client.device.device_name,
        "os_version": client.device.os_version,
        "username": username,
        "uuid": uuid,
    });
    let raw = client.request_with_headers(
        "POST",
        &format!("{}/api/v70100/login", crate::crypto::hosts::RUN),
        Some(&body),
        &[("Authorization", auth_header.as_str())],
    )?;
    let v = expect_ok(&raw)?;
    let data = v.get("data").ok_or("登录响应缺 data")?;
    let profile = UserProfile::from_data(data);

    client.creds = Credentials {
        uid: Some(profile.uid.clone()),
        token: Some(profile.token.clone()),
        unid: Some(profile.unid.clone()),
    };

    // 会话必须落盘：只放内存的话进程重启后 token 就没了，
    // 届时无法通知服务端登出，账号会被锁一小时。
    let sess = super::session::Session {
        uid: profile.uid.to_string(),
        token: profile.token.clone(),
        unid: profile.unid.to_string(),
        username: profile.username.clone(),
        name: profile.name.clone(),
        campus_name: profile.campus_name.clone(),
        depart: profile.depart.clone(),
        grade_class: profile.grade_class.clone(),
        device_id: client.device.device_id.clone(),
    };
    if let Err(e) = super::session::save_session(&sess) {
        eprintln!("[login] 会话保存失败: {e}");
    } else {
        eprintln!("[login] 会话已保存 uid={}", sess.uid);
    }

    Ok(LoginOutcome::Ok(profile))
}

use super::client::uuid_v4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_maps_fields() {
        let d = json!({
            "uid": 123, "unid": 88, "token": "tk", "name": "张三",
            "username": "13800000000", "campusName": "某大学",
            "campusId": "c1", "depart": "计算机", "gradeClass": "2024级1班", "sex": 1
        });
        let p = UserProfile::from_data(&d);
        assert_eq!(p.uid, "123");
        assert_eq!(p.name, "张三");
        assert_eq!(p.sex, "男");
        assert_eq!(p.campus_name, "某大学");
    }

    #[test]
    fn profile_handles_missing() {
        let p = UserProfile::from_data(&json!({}));
        assert_eq!(p.uid, "");
        assert_eq!(p.sex, "未知");
    }
}

/// 调服务端登出接口，让当前 token 失效。
///
/// **必须调**：只清本地状态的话，服务端那边的会话仍然有效，账号被标记为
/// 「已登录」。这时候再登录会被判成「其他设备」直接返回
/// `10121 此账号已在其他设备登录，请过一小时后登录或原设备退出后再登录`。
///
/// 返回 `Ok(true)` 表示服务端确认登出。
pub fn logout(client: &mut Client) -> Result<bool, String> {
    let token = match &client.creds.token {
        Some(t) if !t.is_empty() => t.clone(),
        _ => return Ok(false), // 本来就没登录，不用通知
    };

    // 实测可用的形式：token 走 header，body 为空对象
    let url = format!("{}/api/v6/user/logout", crate::crypto::hosts::RUN);
    let body = serde_json::json!({});
    let _ = &token;
    match client.request("POST", &url, Some(&body)) {
        Ok(raw) => {
            let v: serde_json::Value =
                serde_json::from_str(&raw).unwrap_or(serde_json::json!({}));
            // 服务端可能直接返回明文，也可能包在信封里
            let inner = v.get("r").and_then(|r| {
                serde_json::from_str::<serde_json::Value>(r.as_str().unwrap_or("{}")).ok()
            });
            let ok = inner
                .as_ref()
                .and_then(|x| x.get("error").or_else(|| x.get("code")))
                .and_then(|e| e.as_i64())
                .map(|e| e == crate::crypto::consts::CODE_OK)
                .or_else(|| v.get("error").and_then(|e| e.as_i64()).map(|e| e == crate::crypto::consts::CODE_OK))
                .unwrap_or(true);
            Ok(ok)
        }
        Err(_) => Ok(false),
    }
}
