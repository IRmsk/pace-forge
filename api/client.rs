//! 协议客户端：设备指纹、请求头签名、收发与解密。
//!
//! 一次请求的完整形态：
//!
//! ```text
//! 设备头 + timeStamp + nonce + uid + token
//!   → tokenSign   = MD5(排序拼接 + 盐)
//!   → headerSign  = seal(上述全部头组成的 JSON)   ← 注意：是把 header 自身加密
//!   → body        = seal(业务 JSON)
//! ```
//!
//! `headerSign` 不是签名而是加密信封，这点最容易搞错。见 `crypto::envelope`。

use serde_json::Value;

use crate::crypto::envelope::{seal, SessionKeys};
use sha2::{Digest, Sha256};
use crate::crypto::{decrypt, sign::token_sign, SignFields};

/// 设备指纹。首次运行随机生成后应持久化，否则容易被判为异常设备。
#[derive(Debug, Clone)]
pub struct DeviceProfile {
    pub app_version: String,
    pub physic_pixel: String,
    pub logic_pixel: String,
    pub is_root: String,
    pub os_type: String,
    pub cpu_model: String,
    pub device_name: String,
    pub device_id: String,
    pub os_version: String,
    pub user_agent: String,
    /// 安卓安装 ID：真机是 16 位小写 hex。
    ///
    /// 原来一个都没有。真机这八项是从系统 API 直接读的，服务端把它们当
    /// 设备指纹的一部分；缺失时不会直接拒单，而是在采样点维度打上
    /// 「来源可疑」的兜底标记——排查时很容易一路追到轨迹上去。
    pub android_id: String,
    /// 蓝牙 MAC（大写下划线分隔以外的常见形式）。
    pub bl_mac: String,
    /// Wi-Fi MAC。
    pub wifi_mac: String,
    /// 设备 IMEI。协议允许空串，但真机读得到。
    pub imei: String,
    /// 网卡 MAC（登录 body 里的 `mac_address`）。
    pub mac_address: String,
    /// App 安装时间（毫秒）。必须跨请求稳定，每次变等于自报改机。
    pub app_install_time: i64,
    /// App 上次更新时间（毫秒）。
    pub app_update_time: i64,
    /// 厂商自定义设备 ID。
    pub custom_device_id: String,
}

impl DeviceProfile {
    /// 按账号隔离设备标识的构造。
    ///
    /// `scope` 传账号 uid（或手机号）：同一个账号永远拿到同一个 device_id，
    /// 不同账号拿到不同的，避免被按设备聚合。
    pub fn for_account(scope: &str) -> Self {
        Self::for_account_in(&crate::api::session::home_dir(), scope)
    }

    /// 同上，但 HOME 基目录由调用方指定（测试注入隔离目录用）。
    pub(crate) fn for_account_in(base: &std::path::Path, scope: &str) -> Self {
        let device_id = persistent_device_id_in(base, scope);
        let mut d = Self { device_id: device_id.clone(), ..Self::base() };
        d.fill_derived_identity(&device_id);
        d
    }

    /// 不隔离的默认构造，但 HOME 基目录由调用方指定。
    pub(crate) fn default_in(base: &std::path::Path) -> Self {
        Self::for_account_in(base, "")
    }

    /// 由 device_id 派生其余设备标识。
    ///
    /// 全部是对 device_id 的确定性派生，所以同一账号每次跑都完全一致——
    /// 真机这些值从系统 API 读，一年都不会变；每次提交都换一茬等于自报
    /// 改机。安装时间用一个落在合理区间的固定值，也不用当前时间。
    fn fill_derived_identity(&mut self, seed: &str) {
        let h = |salt: &str| -> String {
            let mut hasher = Sha256::new();
            hasher.update(seed.as_bytes());
            hasher.update(salt.as_bytes());
            hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
        };
        // 安卓 android_id 是 16 位小写 hex
        self.android_id = h("android_id")[..16].to_string();
        self.custom_device_id = h("custom")[..32].to_string();
        // MAC 用本地管理位（第二字节 bit1 置 1），不对应真实网卡
        let mac = |salt: &str, sep: &str| -> String {
            let x = h(salt);
            let bytes: Vec<String> = (0..6)
                .map(|i| format!("{:02x}", u8::from_str_radix(&x[i * 2..i * 2 + 2], 16).unwrap_or(0)))
                .collect();
            let mut b = bytes.clone();
            if let Ok(v) = u8::from_str_radix(&b[0], 16) {
                b[0] = format!("{:02x}", v | 0x02);
            }
            b.join(sep)
        };
        self.wifi_mac = mac("wifi", ":");
        self.bl_mac = mac("bt", ":");
        self.mac_address = mac("nic", ":");
        // IMEI 十五位数字，TAC 前缀取常见值
        let digits: String = h("imei")
            .chars()
            .filter(|c| c.is_ascii_digit())
            .take(15)
            .collect();
        self.imei = format!("86{digits:0<13}");
        // 安装/更新时间：从 device_id 派生一个落在 2023-2024 的固定时刻
        let base = 1_690_000_000_000i64; // 2023-07
        let off: i64 = u32::from_str_radix(&h("inst")[..8], 16).unwrap_or(0) as i64;
        self.app_install_time = base + off % (180 * 86_400_000);
        self.app_update_time = self.app_install_time + off % (7 * 86_400_000);
    }

    /// 不隔离的默认构造（单账号 / CLI 用）。
    fn base() -> Self {
        Self {
            device_id: String::new(),
            app_version: "7.4.01".into(),
            physic_pixel: "1080x2400".into(),
            logic_pixel: "393x873".into(),
            is_root: "0".into(),
            os_type: "0".into(),
            cpu_model: "arm64-v8a".into(),
            device_name: "Android Device".into(),
            os_version: "13".into(),
            user_agent: "okhttp/4.12.0".into(),
            android_id: String::new(),
            bl_mac: String::new(),
            wifi_mac: String::new(),
            imei: String::new(),
            mac_address: "02:00:00:00:00:00".into(),
            app_install_time: 0,
            app_update_time: 0,
            custom_device_id: String::new(),
        }
    }
}

impl Default for DeviceProfile {
    fn default() -> Self {
        Self::default_in(&crate::api::session::home_dir())
    }
}

/// UUID v4 格式字符串。
///
/// `nonce` 与登录 session 都要求这个格式（36 字符、带横线）。
/// 早先误用 32 位裸 hex，导致 `checkGeeUse` 返回 HTTP 200 + 空 body。
pub fn uuid_v4() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut b = [0u8; 16];
    rng.fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// 设备标识文件路径：`<base>/.pace-forge-device-id[-<scope 摘要>]`。
///
/// 注意它**不在** `<base>/.pace-forge/` 目录里，而是与数据目录同级的隐藏
/// 文件——早期版本就是这么放的，改了会让所有老用户被判成新设备（10121）。
///
/// **纯路径计算，不碰磁盘。** 测试用它把路径注入到一次性临时目录。
pub(crate) fn device_id_path_in(base: &std::path::Path, scope: &str) -> std::path::PathBuf {
    let name = if scope.is_empty() {
        ".pace-forge-device-id".to_string()
    } else {
        format!(".pace-forge-device-id-{}", short_hash(scope))
    };
    base.join(name)
}

/// 同上，但 HOME 基目录由调用方指定（测试注入隔离目录用）。
///
/// 写盘走 `session::write_file_private`：父目录不存在时自己建（`$HOME` 正常
/// 一定存在，但不把「环境一定对」当假设），权限收紧到 0600。
pub(crate) fn persistent_device_id_in(base: &std::path::Path, scope: &str) -> String {
    let path = device_id_path_in(base, scope);

    if let Ok(s) = std::fs::read_to_string(&path) {
        let t = s.trim().to_string();
        if t.len() == 32 && t.chars().all(|c| c.is_ascii_hexdigit()) {
            return t;
        }
    }

    let id = random_hex32();
    let _ = crate::api::session::write_file_private(&path, id.as_bytes());
    id
}

/// 8 位十六进制摘要，用来给每个账号派生一个独立的设备文件。
fn short_hash(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let d = h.finalize();
    format!("{:02x}{:02x}{:02x}{:02x}", d[0], d[1], d[2], d[3])
}

fn random_hex32() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| format!("{:x}", rng.gen_range(0..16u8)))
        .collect()
}

/// 鉴权凭据。
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    pub uid: Option<String>,
    pub token: Option<String>,
    pub unid: Option<String>,
}

/// 协议客户端。
#[derive(Clone)]
pub struct Client {
    pub device: DeviceProfile,
    pub creds: Credentials,
    /// 强制验签。
    ///
    /// 默认**关**：服务端的签名格式与 `@ApiCrypto` 注解的四个开关绑定，
    /// 部分接口（如 datapoint 埋点）返回明文、无 `s` 字段。强制验签会让这些
    /// 接口整体失败。已用 Python 实现实测确认：不验签时三个接口全部 error=10000。
    /// 需要完整性保证时再打开。
    pub require_signature: bool,
    pub agent: ureq::Agent,
}

impl Client {
    pub fn new(device: DeviceProfile) -> Self {
        Self {
            device,
            creds: Credentials::default(),
            require_signature: false,
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(20))
                .build(),
        }
    }

    /// 当前凭据的只读引用（未登录时为 `None`）。
    pub fn credentials(&self) -> Option<&Credentials> {
        if self.creds.uid.is_some() || self.creds.token.is_some() {
            Some(&self.creds)
        } else {
            None
        }
    }

    pub fn with_credentials(mut self, creds: Credentials) -> Self {
        self.creds = creds;
        self
    }

    /// 组装请求头，返回 (headers, headerSign 会话)。
    ///
    /// `headerSign` 的会话密钥必须保留——响应是用它解密的。
    fn build_headers(&self) -> Result<(Vec<(String, String)>, SessionKeys), String> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| format!("系统时间异常: {e}"))?
            .as_millis() as u64;
        let nonce = uuid_v4();

        let mut headers: Vec<(String, String)> = vec![
            ("Accept".into(), "application/json".into()),
            ("Content-Type".into(), "application/json".into()),
            ("appVersion".into(), self.device.app_version.clone()),
            ("physicPixel".into(), self.device.physic_pixel.clone()),
            ("logicPixel".into(), self.device.logic_pixel.clone()),
            ("isRoot".into(), self.device.is_root.clone()),
            ("osType".into(), self.device.os_type.clone()),
            ("cpuModel".into(), self.device.cpu_model.clone()),
            ("deviceName".into(), self.device.device_name.clone()),
            ("DeviceId".into(), self.device.device_id.clone()),
            ("osVersion".into(), self.device.os_version.clone()),
            ("User-Agent".into(), self.device.user_agent.clone()),
            ("timeStamp".into(), now_ms.to_string()),
            ("nonce".into(), nonce),
            // 真机从系统 API 读的八项设备指纹。缺这些不会直接拒单，但服务
            // 端会把整次 run 的采样点打上「来源可疑」的兜底标记。
            ("androidId".into(), self.device.android_id.clone()),
            ("blMac".into(), self.device.bl_mac.clone()),
            ("wifiMac".into(), self.device.wifi_mac.clone()),
            ("IMEI".into(), self.device.imei.clone()),
            ("appInstallTime".into(), self.device.app_install_time.to_string()),
            ("appUpdateTime".into(), self.device.app_update_time.to_string()),
            ("CustomDeviceId".into(), self.device.custom_device_id.clone()),
        ];
        if let Some(uid) = &self.creds.uid {
            headers.push(("uid".into(), uid.clone()));
        }
        if let Some(token) = &self.creds.token {
            headers.push(("token".into(), token.clone()));
        }

        // tokenSign
        let sign_fields: SignFields = vec![
            ("timeStamp".into(), Some(now_ms.to_string())),
            ("token".into(), Some(self.creds.token.clone().unwrap_or_default())),
            ("uid".into(), Some(self.creds.uid.clone().unwrap_or_default())),
        ];
        headers.push(("tokenSign".into(), token_sign(&sign_fields)));

        // headerSign：把上面所有头组成 JSON 后整体加密
        let header_json = headers_to_json(&headers);
        let env = seal(&header_json)?;
        headers.push(("headerSign".into(), env.wire));

        Ok((headers, env.session))
    }

    /// 发请求并解密响应。
    ///
    /// 返回业务 JSON 字符串（已解密）。HTTP 非 2xx 也会尝试解密，
    /// 因为服务端有时在错误响应里同样使用加密信封。
    pub fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<&Value>,
    ) -> Result<String, String> {
        self.request_with_headers(method, url, body, &[])
    }

    /// 带额外请求头的版本。登录需要 `Authorization: Basic ...`，
    /// 这类头不参与 `headerSign` 的计算（服务端只对设备头做信封）。
    pub fn request_with_headers(
        &self,
        method: &str,
        url: &str,
        body: Option<&Value>,
        extra_headers: &[(&str, &str)],
    ) -> Result<String, String> {
        let (headers, session) = self.build_headers()?;

        let mut req = match method.to_uppercase().as_str() {
            "GET" => self.agent.get(url),
            "POST" => self.agent.post(url),
            "PUT" => self.agent.put(url),
            m => return Err(format!("不支持的 method: {m}")),
        };
        for (k, v) in &headers {
            req = req.set(k, v);
        }
        for (k, v) in extra_headers {
            req = req.set(k, v);
        }

        // POST body 同样要打信封（`encryptBody`）。响应仍用 headerSign 的会话密钥解密，
        // 所以 body 信封派生出的 key 直接丢弃——服务端不用它加密响应。
        let resp = match body {
            Some(b) => {
                let plain = serde_json::to_string(b).map_err(|e| format!("body 序列化失败: {e}"))?;
                let body_env = seal(&plain)?;
                req.send_string(&body_env.wire)
            }
            None => req.call(),
        };

        let (status, text) = match resp {
            Ok(r) => (r.status(), r.into_string().map_err(|e| e.to_string())?),
            Err(ureq::Error::Status(code, r)) => {
                (code, r.into_string().unwrap_or_default())
            }
            Err(e) => return Err(format!("网络错误: {e}")),
        };

        if text.trim().is_empty() {
            return Err(format!("HTTP {status} 响应为空"));
        }
        // 网关级错误（如 Spring 的 404 页）是明文且无加密信封，直接透出原文
        if text.contains("\"status\":404") || text.contains("No message available") {
            return Err(format!("HTTP {status} 网关错误: {}", truncate(&text, 200)));
        }

        // 有些接口 decryptResponse=false，直接返回业务 JSON。探测：无 r 字段即明文。
        let looks_encrypted = text.contains("\"r\"") && text.contains("\"v\"");
        if !looks_encrypted {
            return Ok(text);
        }

        match decrypt::open(&text, &session, self.require_signature) {
            Ok(d) => Ok(d.body),
            Err(e) => Err(format!("HTTP {status} 解密失败: {e}；原文 {} ", truncate(&text, 120))),
        }
    }
}

fn headers_to_json(headers: &[(String, String)]) -> String {
    let mut map = serde_json::Map::new();
    for (k, v) in headers {
        map.insert(k.clone(), Value::String(v.clone()));
    }
    Value::Object(map).to_string()
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 便捷方法：解析业务 JSON 并检查 `error` 字段。
pub fn expect_ok(body: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("业务响应不是 JSON: {e}"))?;
    let code = v.get("error").and_then(|x| x.as_i64());
    if code == Some(crate::crypto::CODE_OK) {
        return Ok(v);
    }
    let msg = v.get("message").and_then(|x| x.as_str()).unwrap_or("未知错误");
    // 服务端内部故障要单独说清楚。它长得像「业务错误」，其实是后端服务被
    // 注册中心白名单拒绝后把异常原样透传上来，跟本地环境一点关系都没有。
    // 不点明的话，用户会去换网络、重装、换账号、挂代理，全白费
    // （issue #3 里那位折腾了整整两天）。
    if code == Some(crate::crypto::CODE_SERVER_FAULT) {
        return Err(format!(
            "服务端集群故障（error=-10000），跟你这边无关，别折腾本地环境：{msg}\n\n\
             这条错误是对方后端内部服务调用被拦（日志里通常带 \n\
             `Forbid consumer x.x.x.x access service ... from registry ...:2181`），\n\
             登录、查记录、拉策略会一起失败。\n\
             换网络、换账号、重装、重新编译都没有用——等后端恢复即可。"
        ));
    }
    Err(format!("业务错误 error={code:?} message={msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{assert_isolated, TestDir};

    #[test]
    fn nonce_is_uuid_v4_format() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36, "nonce 必须是 36 字符 UUID，否则安全接口返回空 body");
        assert_eq!(u.matches('-').count(), 4);
    }

    #[test]
    fn device_id_is_32_hex() {
        // 必须注入临时目录：DeviceProfile::default() 会读/写真实
        // `$HOME/.pace-forge-device-id`，测试不能碰。
        let t = TestDir::new("client-device-id");
        let d = DeviceProfile::default_in(t.home());
        assert_eq!(d.device_id.len(), 32);
        assert!(d.device_id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// 设备标识必须落在注入目录里，且同一 scope 反复读得到同一个值。
    #[test]
    fn device_id_is_persisted_in_isolated_dir() {
        let t = TestDir::new("client-device-persist");
        let a = persistent_device_id_in(t.home(), "13800000000");
        let path = device_id_path_in(t.home(), "13800000000");
        assert!(path.exists(), "首次应落盘: {}", path.display());
        assert!(
            path.starts_with(t.home()),
            "设备标识必须写在注入基目录下，而不是 .pace-forge/ 里"
        );
        assert_eq!(
            path.parent().unwrap(),
            t.home(),
            "设备标识是数据目录的**同级文件**，不是目录内文件"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "设备标识文件权限必须是 600");
        }
        assert_eq!(persistent_device_id_in(t.home(), "13800000000"), a, "同一 scope 必须稳定");
        assert_ne!(
            persistent_device_id_in(t.home(), "13900000000"),
            a,
            "不同 scope 必须是不同设备标识"
        );
    }

    #[test]
    fn headers_include_signature_and_envelope() {
        let t = TestDir::new("client-headers");
        let c = Client::new(DeviceProfile::for_account_in(t.home(), "u1"));
        let (headers, session) = c.build_headers().unwrap();
        let keys: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"tokenSign"), "缺 tokenSign");
        assert!(keys.contains(&"headerSign"), "缺 headerSign");
        assert!(keys.contains(&"DeviceId"), "缺 DeviceId");
        // headerSign 必须是信封结构
        let h = headers
            .iter()
            .find(|(k, _)| k == "headerSign")
            .map(|(_, v)| v)
            .unwrap();
        let v: Value = serde_json::from_str(h).unwrap();
        assert_eq!(v["p"], "101");
        assert_eq!(session.response_key.len(), 16);
    }

    #[test]
    fn headers_carry_credentials_when_set() {
        let t = TestDir::new("client-creds");
        let c = Client::new(DeviceProfile::for_account_in(t.home(), "u1")).with_credentials(
            Credentials {
                uid: Some("123".into()),
                token: Some("tok".into()),
                unid: Some("88".into()),
            },
        );
        let (headers, _) = c.build_headers().unwrap();
        assert!(headers.iter().any(|(k, v)| k == "uid" && v == "123"));
        assert!(headers.iter().any(|(k, v)| k == "token" && v == "tok"));
    }

    #[test]
    fn expect_ok_accepts_10000() {
        assert!(expect_ok(r#"{"error":10000,"message":"成功","data":1}"#).is_ok());
        assert!(expect_ok(r#"{"error":10121,"message":"设备异常"}"#).is_err());
    }

    /// `-10000` 必须被判成**服务端故障**，而且提示里要明确说「跟你无关」。
    ///
    /// 真实的报文长这样（issue #3 里贴出来的原文）：错误码是负数，`message`
    /// 里塞着 dubbo 的 `Forbid consumer ... from registry ...:2181`。用户看到
    /// 这种信息的默认反应是重装、换号、挂代理，全都是白费。
    #[test]
    fn server_fault_is_explained_not_just_reported() {
        let body = r#"{"error":-10000,"message":"Forbid consumer 10.0.50.83 access service com.iydsj.server.basicuser.service.UserBasicService from registry 10.0.30.34:2181. Please check registry access list (whitelist/blacklist)"}"#;
        let e = expect_ok(body).unwrap_err();
        assert!(e.contains("-10000"), "要带上原始错误码，方便搜：{e}");
        assert!(e.contains("服务端"), "要说明是服务端的问题：{e}");
        assert!(
            e.contains("无关") && e.contains("没有用"),
            "必须明确劝阻换网络/换账号/重装：{e}"
        );
        // 别的非零码仍然走原来的「业务错误」，不要被这条吞掉
        let e2 = expect_ok(r#"{"error":10121,"message":"设备异常"}"#).unwrap_err();
        assert!(e2.starts_with("业务错误"), "10121 不该被当成服务端故障：{e2}");
    }

    /// 护栏：设备标识的注入路径**绝不能**是真实 `$HOME/.pace-forge-device-id*`。
    ///
    /// 模式与 `session::tests::test_data_dir_is_isolated_from_real_home` 一致。
    /// 注意设备标识是 `$HOME` 下的**同级文件**，不在 `.pace-forge/` 目录里，
    /// 所以这里单独断言它的基目录不能是真实 `$HOME`。
    #[test]
    fn device_id_path_is_isolated_from_real_home() {
        let t = TestDir::new("client-isolated");
        assert_isolated(&t.data(), "设备标识注入基目录");
        let injected = device_id_path_in(t.home(), "");
        let real = device_id_path_in(&crate::test_support::real_home(), "");
        assert_ne!(injected, real, "注入路径不能等于生产默认路径");
        assert_eq!(injected.file_name().unwrap(), ".pace-forge-device-id");
        assert_eq!(
            injected.parent().unwrap(),
            t.home(),
            "设备标识必须写在注入基目录下"
        );
    }
}
