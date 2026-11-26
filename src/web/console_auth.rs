//! 控制台鉴权：口令校验、本机判定、跨站写防护。
//!
//! 不设口令时只接受本机访问；设了口令才放行同网段（请求带 X-Pace-Token）。
//! 没设口令时的跨站写防护见 `origin_matches_host`。

/// 启动 Web 控制台。
/// 控制台口令。配置 `server.token`（env `PACE_FORGE_TOKEN`）后，受保护的
/// 请求必须带 `X-Pace-Token` 请求头。
///
/// 不设置时只允许本机访问——绑 0.0.0.0 又没有口令，等于把「登录/提交/删账号」
/// 暴露给同网段任意主机，密码还是明文 HTTP 传的。
pub(crate) fn auth_token() -> Option<String> {
    // 原文直用（不 trim）：带空格的口令也是合法口令，与旧实现逐字节一致
    let cfg = crate::config::global();
    let t = &cfg.server.token;
    (!t.is_empty()).then(|| t.clone())
}

/// 请求来源是否本机。
///
/// 覆盖整个 `127.0.0.0/8`、`::1`，以及 IPv4-mapped 形态 `::ffff:127.x.x.x`：
/// 服务绑在 `[::]` 时，本机用 IPv4 回环访问，对端看到的正是 mapped 地址。
/// 早先只精确匹配了 `::ffff:127.0.0.1` 一个值，`127.0.0.2` 这类会被误判成远程。
fn is_loopback(remote: &str) -> bool {
    use std::net::IpAddr;
    let r = remote.trim().trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = r.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => v4.is_loopback(),
            IpAddr::V6(v6) => {
                v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
            }
        };
    }
    // 解析不出来时退回字符串判据（宁可严格）
    r.starts_with("127.") || r == "::1"
}

/// 口令比较，走常量时间。
///
/// 直接 `==` 在第一个不同的字节就返回，同网段攻击者能从响应时间差里逐字节
/// 试探口令。两边先摘成 SHA-256 再逐字节 XOR 累加，比较路径与内容无关。
fn ct_eq(got: &str, want: &str) -> bool {
    use sha2::{Digest, Sha256};
    let a = Sha256::digest(got.as_bytes());
    let b = Sha256::digest(want.as_bytes());
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 鉴权通过返回 Ok，否则返回 HTTP 状态码和错误说明。
pub(crate) fn check_auth(
    request: &tiny_http::Request,
    token: Option<&str>,
) -> Result<(), (u16, String)> {
    let path = request.url().split('?').next().unwrap_or_default();
    // 版本探测不拦：它只吐一个版本号，页面靠它显示构建号，
    // 拦掉的话远程模式下页脚永远显示「本地控制台」。
    if path == "/api/health" {
        return Ok(());
    }
    match token {
        Some(want) => {
            let got = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("X-Pace-Token"))
                .map_or("", |h| h.value.as_str());
            if ct_eq(got, want) {
                Ok(())
            } else {
                Err((401, "缺少或错误的口令（X-Pace-Token）".into()))
            }
        }
        None => {
            let remote = request
                .remote_addr()
                .map(|a| a.ip().to_string())
                .unwrap_or_default();
            if is_loopback(&remote) {
                Ok(())
            } else {
                Err((
                    403,
                    format!(
                        "拒绝来自 {remote} 的访问：控制台默认只允许本机，\
                         远程使用请设置 PACE_FORGE_TOKEN 并在请求头带 X-Pace-Token"
                    ),
                ))
            }
        }
    }
}

/// Origin 与 Host 是否同源。浏览器跨站发写请求时一定带 Origin，
/// 而请求行里的 Host 是真实目标地址；两者对不上就是别人家的页面在打我们。
pub(crate) fn origin_matches_host(origin: Option<&str>, host: Option<&str>) -> bool {
    let Some(o) = origin else { return true }; // curl / 脚本不带 Origin
    let Some(h) = host else { return false };
    let oh = o
        .split("//")
        .nth(1)
        .unwrap_or("")
        .split('/')
        .next()
        .unwrap_or("");
    oh.eq_ignore_ascii_case(h.trim())
}

#[cfg(test)]
mod origin_tests {
    use super::origin_matches_host;

    #[test]
    fn same_origin_passes_and_cross_site_is_blocked() {
        // 本机页面自己的写请求
        assert!(origin_matches_host(
            Some("http://127.0.0.1:9999"),
            Some("127.0.0.1:9999")
        ));
        assert!(origin_matches_host(
            Some("http://localhost:8899"),
            Some("localhost:8899")
        ));
        // curl / 脚本不带 Origin
        assert!(origin_matches_host(None, Some("127.0.0.1:9999")));
        // 恶意网页盲打 127.0.0.1：Origin 是它自己，Host 才是目标
        assert!(!origin_matches_host(
            Some("https://evil.example"),
            Some("127.0.0.1:9999")
        ));
        // 有 Origin 却没有 Host —— 不认
        assert!(!origin_matches_host(Some("http://127.0.0.1:9999"), None));
    }
}

#[cfg(test)]
mod loopback_tests {
    use super::is_loopback;

    /// 本机判定必须覆盖整个回环段，包括双栈下 IPv4 的 mapped 形态。
    #[test]
    fn loopback_covers_whole_range_and_mapped_ipv4() {
        for ok in [
            "127.0.0.1",
            "127.0.0.2",
            "::1",
            "[::1]",
            "::ffff:127.0.0.1",
            "::ffff:127.0.0.2",
        ] {
            assert!(is_loopback(ok), "{ok} 应判为本机");
        }
        for no in [
            "10.0.0.1",
            "192.168.1.2",
            "::ffff:192.168.1.2",
            "114.114.114.114",
        ] {
            assert!(!is_loopback(no), "{no} 不该判为本机");
        }
    }
}

#[cfg(test)]
mod token_tests {
    use super::{check_auth, ct_eq};
    use tiny_http::{Header, Request, TestRequest};

    #[test]
    fn authenticated_api_accepts_header_and_ignores_query() {
        let token = "test-console-token";
        let query_only: Request = TestRequest::new()
            .with_path(&format!("/api/me?token={token}"))
            .into();
        assert_eq!(check_auth(&query_only, Some(token)).unwrap_err().0, 401);

        let matching_header: Request = TestRequest::new()
            .with_path("/api/me?token=ignored")
            .with_header(Header::from_bytes("x-pace-token", token).unwrap())
            .into();
        assert!(check_auth(&matching_header, Some(token)).is_ok());

        let wrong_header: Request = TestRequest::new()
            .with_path(&format!("/api/me?token={token}"))
            .with_header(Header::from_bytes("X-Pace-Token", "wrong-token").unwrap())
            .into();
        assert_eq!(check_auth(&wrong_header, Some(token)).unwrap_err().0, 401);
    }

    #[test]
    fn health_is_public_and_unconfigured_api_remains_local() {
        let remote = "192.0.2.10:80".parse().unwrap();
        let health: Request = TestRequest::new()
            .with_remote_addr(remote)
            .with_path("/api/health?t=1")
            .into();
        assert!(check_auth(&health, Some("test-console-token")).is_ok());

        let unconfigured_remote: Request = TestRequest::new()
            .with_remote_addr(remote)
            .with_path("/api/me")
            .into();
        assert_eq!(check_auth(&unconfigured_remote, None).unwrap_err().0, 403);

        let unconfigured_local: Request = TestRequest::new().with_path("/api/me").into();
        assert!(check_auth(&unconfigured_local, None).is_ok());
    }

    /// 口令比较只认逐字节全等；前缀相同、长度不同的都不能过。
    #[test]
    fn token_compare_is_exact_and_prefix_safe() {
        let want = "0123456789abcdef0123456789abcdef";
        assert!(ct_eq(want, want));
        assert!(!ct_eq(&want[..16], want), "前缀相同不能算相等");
        assert!(!ct_eq("", want));
        assert!(!ct_eq("0123456789abcdef0123456789abcdee", want));
    }
}
