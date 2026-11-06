//! 跑步开始时间计算（纯函数）。
//!
//! 补跑的核心规则：**开始 + 用时 + 抖动余量 ≤ 当前时刻**。
//! 提交时 `flow` 会给开始时间再加 0~4 秒抖动，所以这里必须预留 5 秒余量，
//! 否则会出现"记录结束时间在未来"这种一眼假的破绽。
//!
//! 所有函数显式接收 `now`，不读系统时钟：调用方读一次真实时钟传进来，
//! 测试就能用固定时刻复现，不会因为跑测试的时间点不同而时好时坏。

/// 提交时的 0~4 秒抖动，预留 5 秒余量。
pub const JITTER_MARGIN_MS: i64 = 5_000;
/// 最多往前补 3 天。
pub const MAX_DAYS_AGO: i64 = 3;
/// `ago` 分钟数上限：与 3 天上限一致。
pub const MAX_AGO_MIN: i64 = (MAX_DAYS_AGO + 1) * 24 * 60;
/// 默认随机窗口下界：30 分钟前。
pub const DEFAULT_AGO_MIN: i64 = 30;
/// 默认随机窗口上界：300 分钟前。
pub const DEFAULT_AGO_MAX: i64 = 300;
/// 随机时刻模式的窗口（本地时区小时）。
pub const RAND_HOUR_LO: u32 = 7;
pub const RAND_HOUR_HI: u32 = 20;

/// 计算结果。`clamped` 为 true 表示原时刻会让结束时间越过当前，已提前。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartTime {
    pub ms: i64,
    pub clamped: bool,
}

impl StartTime {
    /// 结束时刻。
    pub fn stop_ms(&self, dur_s: i64) -> i64 {
        self.ms + dur_s.max(0) * 1000
    }

    /// 一句话描述，给日志用。
    pub fn describe(&self, dur_s: i64) -> String {
        let s = format_ms(self.ms, default_tz_offset_min());
        if self.clamped {
            format!("{s}（原时刻超界，已提前）")
        } else {
            format!("{s}（结束 {}，用时 {}s）", format_ms(self.stop_ms(dur_s), default_tz_offset_min()), dur_s)
        }
    }
}

/// 开跑时刻上限：当前 - 用时 - 抖动余量。
pub fn latest_start_ms(now_ms: i64, dur_s: i64) -> i64 {
    now_ms - dur_s.max(0) * 1000 - JITTER_MARGIN_MS
}

/// 把开始时刻钳制到上限以内。
pub fn clamp(start_ms: i64, now_ms: i64, dur_s: i64) -> StartTime {
    let latest = latest_start_ms(now_ms, dur_s);
    if start_ms > latest {
        StartTime { ms: latest, clamped: true }
    } else {
        StartTime { ms: start_ms, clamped: false }
    }
}

/// 解析 `HH:MM`（也接受只有小时 `H`，分钟按 0）。
pub fn parse_hhmm(s: &str) -> Result<(u32, u32), String> {
    let s = s.trim();
    let (h, m) = s.split_once(':').unwrap_or((s, "0"));
    let num = |part: &str, max: u32| -> Option<u32> {
        if part.is_empty() || part.len() > 2 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse::<u32>().ok().filter(|v| *v <= max)
    };
    match (num(h, 23), num(m, 59)) {
        (Some(h), Some(m)) => Ok((h, m)),
        _ => Err(format!("时间格式应为 HH:MM（00:00-23:59），收到 {s:?}")),
    }
}

/// 校验天数偏移（0=今天，最多 3 天前）。
pub fn check_days_ago(days_ago: i64) -> Result<i64, String> {
    if (0..=MAX_DAYS_AGO).contains(&days_ago) {
        Ok(days_ago)
    } else {
        Err(format!("天数应为 0-{MAX_DAYS_AGO}，收到 {days_ago}"))
    }
}

// ---------------------------------------------------------------------------
// 公历换算（Howard Hinnant 算法，无第三方依赖）
// ---------------------------------------------------------------------------

/// 公历日期 → 1970-01-01 起的天数。
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64; // [0, 399]
    let mp = ((m as i64) + 9) % 12; // 3月=0
    let doy = (153 * mp + 2) / 5 + (d as i64) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// 1970-01-01 起的天数 → 公历日期。
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 毫秒戳 → 本地 `YYYY-MM-DD HH:MM:SS`。
pub fn format_ms(ms: i64, tz_offset_min: i64) -> String {
    let local = ms + tz_offset_min * 60_000;
    let secs = local.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 毫秒戳 → 本地 `HH:MM`。
pub fn format_hhmm(ms: i64, tz_offset_min: i64) -> String {
    let local = ms + tz_offset_min * 60_000;
    let rem = local.div_euclid(1000).rem_euclid(86_400);
    format!("{:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

/// 默认时区偏移（分钟）。中国固定 +8，不做夏令时。
pub fn default_tz_offset_min() -> i64 {
    480
}

// ---------------------------------------------------------------------------
// 三种补跑模式
// ---------------------------------------------------------------------------

/// 指定时刻：`days_ago` 天前（0=今天）的 `hour:minute`，再按用时钳制。
pub fn specified(
    now_ms: i64,
    days_ago: i64,
    hour: u32,
    minute: u32,
    dur_s: i64,
) -> Result<StartTime, String> {
    let days_ago = check_days_ago(days_ago)?;
    if hour > 23 || minute > 59 {
        return Err(format!("无效时刻 {hour:02}:{minute:02}"));
    }
    let tz = default_tz_offset_min();
    let local_now = now_ms + tz * 60_000;
    let today = local_now.div_euclid(86_400_000);
    let target_day = today - days_ago;
    let local_target = target_day * 86_400_000 + (hour as i64) * 3_600_000 + (minute as i64) * 60_000;
    let start_ms = local_target - tz * 60_000;
    Ok(clamp(start_ms, now_ms, dur_s))
}

/// `ago N`：N 分钟前开跑（1 ≤ N ≤ 5760）。
pub fn ago(now_ms: i64, ago_min: i64, dur_s: i64) -> Result<StartTime, String> {
    if !(1..=MAX_AGO_MIN).contains(&ago_min) {
        return Err(format!("分钟数应为 1-{MAX_AGO_MIN}，收到 {ago_min}"));
    }
    Ok(clamp(now_ms - ago_min * 60_000, now_ms, dur_s))
}

/// 默认：30~300 分钟前随机（`r` ∈ [0,1)）。
pub fn random_ago(now_ms: i64, dur_s: i64, r: f64) -> StartTime {
    let r = if r.is_finite() { r.clamp(0.0, 1.0) } else { 0.0 };
    let span_ms = ((DEFAULT_AGO_MAX - DEFAULT_AGO_MIN) * 60_000) as f64;
    let start = now_ms - DEFAULT_AGO_MIN * 60_000 - (r * span_ms) as i64;
    clamp(start, now_ms, dur_s)
}

/// 指定日期内的随机时刻（7:00~20:00）。`r` ∈ [0,1)。
pub fn random_in_day(now_ms: i64, days_ago: i64, dur_s: i64, r: f64) -> Result<StartTime, String> {
    let days_ago = check_days_ago(days_ago)?;
    let r = if r.is_finite() { r.clamp(0.0, 1.0) } else { 0.0 };
    let tz = default_tz_offset_min();
    let today = (now_ms + tz * 60_000).div_euclid(86_400_000);
    let day = today - days_ago;
    let span_min = ((RAND_HOUR_HI - RAND_HOUR_LO) * 60) as f64;
    let minute_of_day = (RAND_HOUR_LO as f64 * 60.0 + r * span_min) as i64;
    let local_target = day * 86_400_000 + minute_of_day * 60_000;
    Ok(clamp(local_target - tz * 60_000, now_ms, dur_s))
}

/// 从字符串生成一个稳定的随机数（给 CLI 用，避免每次不同）。
pub fn seeded_unit(seed: &str) -> f64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in seed.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h % 1_000_000) as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-06 12:00:00 +08:00
    fn now() -> i64 {
        let days = days_from_civil(2026, 11, 6);
        days * 86_400_000 + 12 * 3_600_000 - 480 * 60_000
    }

    #[test]
    fn civil_roundtrip() {
        for (y, m, d) in [(1970, 1, 1), (2026, 11, 6), (2000, 2, 29), (1999, 12, 31)] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d), "{y}-{m}-{d}");
        }
    }

    #[test]
    fn epoch_is_day_zero() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn format_is_local() {
        let s = format_ms(now(), 480);
        assert_eq!(s, "2026-11-06 12:00:00");
        assert_eq!(format_hhmm(now(), 480), "12:00");
    }

    #[test]
    fn latest_reserves_jitter_margin() {
        // 12 分钟跑，12:00 开跑 → 结束 12:12，超界
        let l = latest_start_ms(now(), 720);
        assert_eq!(now() - l, 720_000 + 5_000, "要留 5 秒余量");
    }

    #[test]
    fn clamp_pulls_back_when_too_late() {
        // 想在 11:59 开跑 12 分钟，会到 12:11（未来）
        let st = specified(now(), 0, 11, 59, 720).unwrap();
        assert!(st.clamped, "超界要标记");
        assert!(st.stop_ms(720) <= now(), "结束时间不能在未来");
        // 至少提前了 (12:11 - 12:00) 那么多
        assert!(st.ms <= now() - 720_000);
    }

    #[test]
    fn clamp_keeps_earlier_time() {
        // 10:00 开跑 12 分钟，完全在过去
        let st = specified(now(), 0, 10, 0, 720).unwrap();
        assert!(!st.clamped);
        assert_eq!(st.stop_ms(720), st.ms + 720_000);
        assert!(st.stop_ms(720) < now());
    }

    #[test]
    fn yesterday_works() {
        let st = specified(now(), 1, 20, 30, 1800).unwrap();
        assert!(!st.clamped, "昨天的时间一定在过去");
        let s = format_ms(st.ms, 480);
        assert!(s.starts_with("2026-11-05 20:30"), "实际 {s}");
    }

    #[test]
    fn days_ago_bounds() {
        assert!(specified(now(), 3, 8, 0, 600).is_ok());
        assert!(specified(now(), 4, 8, 0, 600).is_err(), "超过 3 天要报错");
        assert!(specified(now(), -1, 8, 0, 600).is_err());
    }

    #[test]
    fn hhmm_parsing() {
        assert_eq!(parse_hhmm("14:30").unwrap(), (14, 30));
        assert_eq!(parse_hhmm("9").unwrap(), (9, 0));
        assert_eq!(parse_hhmm(" 7:05 ").unwrap(), (7, 5));
        assert!(parse_hhmm("24:00").is_err());
        assert!(parse_hhmm("12:60").is_err());
        assert!(parse_hhmm("abc").is_err());
    }

    #[test]
    fn ago_minutes() {
        let st = ago(now(), 60, 600).unwrap();
        assert_eq!(st.ms, now() - 60 * 60_000);
        assert!(!st.clamped);
        assert!(ago(now(), 0, 600).is_err());
        assert!(ago(now(), MAX_AGO_MIN + 1, 600).is_err());
    }

    #[test]
    fn ago_clamped_when_duration_overruns() {
        // 5 分钟前开跑却要跑 12 分钟 → 结束在未来
        let st = ago(now(), 5, 720).unwrap();
        assert!(st.clamped);
        assert!(st.stop_ms(720) <= now());
    }

    #[test]
    fn random_stays_in_window() {
        for r in [0.0, 0.25, 0.5, 0.99, 1.0] {
            let st = random_ago(now(), 600, r);
            let back = (now() - st.ms) / 60_000;
            assert!(
                back >= DEFAULT_AGO_MIN && back <= DEFAULT_AGO_MAX + 15,
                "r={r} 回溯 {back} 分钟，超出 30~300 窗口"
            );
            assert!(st.stop_ms(600) <= now(), "结束时间不能在未来");
        }
    }

    #[test]
    fn random_in_day_lands_in_hour_window() {
        let st = random_in_day(now(), 2, 600, 0.5).unwrap();
        let hhmm = format_hhmm(st.ms, 480);
        let h: u32 = hhmm[..2].parse().unwrap();
        assert!((RAND_HOUR_LO..=RAND_HOUR_HI).contains(&h), "落在 {hhmm}，不在 7-20 点");
        assert!(format_ms(st.ms, 480).starts_with("2026-11-04"));
    }

    #[test]
    fn never_produces_future_end() {
        // 遍历各种组合，结束时刻都不能超过当前
        for dur in [60, 300, 720, 1800, 3600] {
            for (h, m) in [(0u32, 0u32), (6, 30), (11, 50), (12, 0), (23, 59)] {
                let st = specified(now(), 0, h, m, dur).unwrap();
                assert!(
                    st.stop_ms(dur) <= now(),
                    "dur={dur} {h:02}:{m:02} 结束 {} 超过当前 {}",
                    format_ms(st.stop_ms(dur), 480),
                    format_ms(now(), 480)
                );
            }
        }
    }

    #[test]
    fn seeded_unit_is_stable() {
        let a = seeded_unit("2026-11-06");
        let b = seeded_unit("2026-11-06");
        assert_eq!(a, b);
        assert!((0.0..1.0).contains(&a));
        assert_ne!(seeded_unit("2026-11-05"), seeded_unit("2026-11-06"));
    }
}
