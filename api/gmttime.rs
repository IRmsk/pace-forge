//! 服务器时间校准。
//!
//! `GET /api/v3/getserver/gmttime` → `{ "error": 10000, "data": <毫秒时间戳> }`
//!
//! 官方 App 在开跑前会校时，之后所有时间戳（`startTime` / `stopTime` /
//! 点位 `flag` / 10 秒桶的 `beginTime`、`endTime`）都落在**服务器时间轴**上。
//!
//! 为什么必须校：服务端有一条硬判据「开始时间 + 时长 ≤ 服务器当前时间」。
//! 本机时钟一旦快十几分钟，这条永远成立不了；慢十几分钟，则会算出
//! 一个「未来的跑步」，同样被驳。本机时钟被改过、跨时区、系统休眠
//! 导致 RTC 漂移，都会踩到这一点，而本地任何自检都发现不了——
//! 因为自检用的也是本机时钟，自己跟自己永远一致。
//!
//! 实现上取一次往返，用单程 RTT/2 补偿，所以偏差里含网络延迟的一半；
//! 判据是「十几分钟级」的，这点误差无关。

use serde_json::Value;

use super::client::Client;

/// 端点路径。
pub const PATH: &str = "/api/v3/getserver/gmttime";

/// 解析校时响应，返回服务端毫秒时间戳。
///
/// `data` 可能是数字、数字字符串，也可能带秒级时间戳（某些网关版本），
/// 三种都接受——校时失败不该让整条跑步链失败，但解析错了会整体错位十几分钟，
/// 所以宁可在这里把形态放宽、把值校验严。
pub fn parse(raw: &str) -> Result<i64, String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| format!("校时响应不是 JSON: {e}"))?;
    let data = v
        .get("data")
        .or_else(|| v.get("serverTime"))
        .ok_or("校时响应缺 data")?;
    let ms = match data {
        Value::Number(n) => n.as_i64().ok_or("校时 data 不是整数")?,
        Value::String(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("校时 data 不是数字字符串: {s}"))?,
        _ => return Err("校时 data 类型不支持".into()),
    };
    // 秒级 → 毫秒级
    let ms = if ms > 0 && ms < 1_000_000_000_000 {
        ms.saturating_mul(1000)
    } else {
        ms
    };
    // 合理性：2001 年之后、2100 年之前
    if !(1_000_000_000_000..4_102_444_800_000).contains(&ms) {
        return Err(format!("校时值不合法: {ms}"));
    }
    Ok(ms)
}

/// 拉一次服务器时间，返回 `(server_ms, offset_ms)`。
///
/// `offset_ms = 服务器时间（含单程补偿）− 本机时间`，直接加到本机时间上即可。
pub fn fetch(client: &Client) -> Result<(i64, i64), String> {
    let t0 = crate::crypto::envelope::now_ms() as i64;
    let raw = client.request(
        "GET",
        &format!("{}{}", crate::crypto::consts::hosts::RUN, PATH),
        None,
    )?;
    let t1 = crate::crypto::envelope::now_ms() as i64;
    let server = parse(&raw)?;
    let rtt = (t1 - t0).max(0);
    let server_at_t1 = server + rtt / 2;
    Ok((server_at_t1, server_at_t1 - t1))
}

/// 校准后的「现在」（毫秒）。失败时退回本机时钟，由调用方决定是否提示。
pub fn server_now_ms(client: &Client) -> (i64, Option<i64>) {
    match fetch(client) {
        Ok((_, off)) => (crate::crypto::envelope::now_ms() as i64 + off, Some(off)),
        Err(_) => (crate::crypto::envelope::now_ms() as i64, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_number_millis() {
        let ms = parse(r#"{"error":10000,"data":1790000000000}"#).unwrap();
        assert_eq!(ms, 1_790_000_000_000);
    }

    #[test]
    fn parses_number_string() {
        let ms = parse(r#"{"error":10000,"data":"1790000000000"}"#).unwrap();
        assert_eq!(ms, 1_790_000_000_000);
    }

    #[test]
    fn upgrades_seconds_to_millis() {
        let ms = parse(r#"{"data":1790000000}"#).unwrap();
        assert_eq!(ms, 1_790_000_000_000);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("<html>502</html>").is_err());
        assert!(parse(r#"{"error":10000}"#).is_err());
        assert!(parse(r#"{"data":"abc"}"#).is_err());
        assert!(parse(r#"{"data":123}"#).is_err(), "1970 年的时间戳要拒绝");
        assert!(parse(r#"{"data":99999999999999}"#).is_err(), "超出 2100 年要拒绝");
    }
}
