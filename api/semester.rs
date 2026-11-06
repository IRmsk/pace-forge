//! 学期完成度（记分跑的成绩单）。
//!
//! 记分跑的"成绩"不在单次记录里，而是学期累计：
//!
//! - `semesterDis` / `semesterValidDis`：累计里程 / 有效里程
//! - `semesterCount` / `semesterValidCount`：累计次数 / 有效次数
//!
//! 刷记分跑之前先查这个，才知道还差多少、够不够。
//!
//! 两个接口：
//! - `POST /api/v55/runnings/recordssummary/semester`  学期汇总
//! - `POST /api/v41/running/getPersonalSemesterInfo`   个人目标（含起止日期）

use serde_json::{json, Value};

use super::client::{expect_ok, Client};
use crate::crypto::consts::hosts;

/// 学期汇总路径。
pub const SUMMARY_PATH: &str = "/api/v55/runnings/recordssummary/semester";
/// 个人完成度路径。
pub const PERSONAL_PATH: &str = "/api/v41/running/getPersonalSemesterInfo";

/// 学期汇总。
#[derive(Debug, Clone, Default)]
pub struct SemesterSummary {
    /// 学期名，如「2024-2025学年第一学期」。
    pub sname: String,
    /// 累计里程（米）。
    pub semester_dis: f64,
    /// 有效里程（米）。
    pub semester_valid_dis: f64,
    /// 累计次数。
    pub semester_count: i64,
    /// 有效次数。
    pub semester_valid_count: i64,
}

impl SemesterSummary {
    /// 有效里程占累计的比例。低于 1 说明有记录被判无效。
    pub fn valid_ratio(&self) -> f64 {
        if self.semester_dis <= 0.0 {
            return 0.0;
        }
        self.semester_valid_dis / self.semester_dis
    }

    /// 一句话摘要。
    pub fn summary(&self) -> String {
        format!(
            "{}：{:.2}km 有效（共 {:.2}km）· {} 次有效（共 {} 次）",
            if self.sname.is_empty() { "本学期" } else { &self.sname },
            self.semester_valid_dis / 1000.0,
            self.semester_dis / 1000.0,
            self.semester_valid_count,
            self.semester_count,
        )
    }
}

/// 个人学期目标。
#[derive(Debug, Clone, Default)]
pub struct SemesterGoal {
    /// 学期名，如「2026-2027-1 2阶段」。
    pub sname: String,
    /// 已完成里程（米），对应 `semesterValid`。
    pub valid_dis: f64,
    /// 目标方式（`goalMethod`），2 表示按里程。
    pub goal_method: i64,
    /// 学期 id。
    pub sid: i64,
    /// 目标里程（米）。
    pub target_dis: f64,
    /// 目标次数。
    pub target_count: i64,
    /// 学期开始（毫秒）。
    pub begin_ms: i64,
    /// 学期结束（毫秒）。
    pub end_ms: i64,
    /// 原始响应，字段名不稳定时供排查。
    pub raw: Value,
}

impl SemesterGoal {
    /// 还差多少里程（米），用服务端返回的已完成量算。
    pub fn remaining_dis(&self, done: f64) -> f64 {
        (self.target_dis - done).max(0.0)
    }

    /// 用内置的已完成量算还差多少。
    pub fn remaining(&self) -> f64 {
        self.remaining_dis(self.valid_dis)
    }

    /// 完成百分比（0~100）。
    pub fn percent(&self) -> f64 {
        if self.target_dis <= 0.0 {
            return 0.0;
        }
        (self.valid_dis / self.target_dis * 100.0).clamp(0.0, 100.0)
    }

    /// 还差几次。
    pub fn remaining_count(&self, done: i64) -> i64 {
        (self.target_count - done).max(0)
    }

    /// 距离学期结束还有多少天。
    pub fn days_left(&self, now_ms: i64) -> i64 {
        if self.end_ms <= 0 {
            return -1;
        }
        ((self.end_ms - now_ms) / 86_400_000).max(-1)
    }

    /// 是否还在学期内。
    pub fn in_term(&self, now_ms: i64) -> bool {
        if self.begin_ms <= 0 || self.end_ms <= 0 {
            return true; // 无法判断时按在学期内处理
        }
        (self.begin_ms..=self.end_ms).contains(&now_ms)
    }
}

/// 查学期汇总。
pub fn fetch_summary(client: &Client) -> Result<SemesterSummary, String> {
    let raw = client.request(
        "POST",
        &format!("{}{}", hosts::RUN, SUMMARY_PATH),
        Some(&json!({})),
    )?;
    let v = expect_ok(&raw)?;
    Ok(parse_summary(&v))
}

/// 解析学期汇总。
pub fn parse_summary(v: &Value) -> SemesterSummary {
    let d = v.get("data").unwrap_or(v);
    SemesterSummary {
        sname: d.get("sname").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        semester_dis: d.get("semesterDis").and_then(|x| x.as_f64()).unwrap_or(0.0),
        semester_valid_dis: d
            .get("semesterValidDis")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0),
        semester_count: d.get("semesterCount").and_then(|x| x.as_i64()).unwrap_or(0),
        semester_valid_count: d
            .get("semesterValidCount")
            .and_then(|x| x.as_i64())
            .unwrap_or(0),
    }
}

/// 查个人学期目标（runMode=1 校园跑 / 记分跑）。
pub fn fetch_goal(client: &Client, run_mode: i64) -> Result<SemesterGoal, String> {
    let raw = client.request(
        "POST",
        &format!("{}{}", hosts::RUN, PERSONAL_PATH),
        Some(&json!({ "runMode": run_mode })),
    )?;
    let v = expect_ok(&raw)?;
    Ok(parse_goal(&v))
}

/// 解析个人目标。字段名不稳定，逐个试探。
pub fn parse_goal(v: &Value) -> SemesterGoal {
    let d = v.get("data").unwrap_or(v);
    let num = |keys: &[&str]| -> f64 {
        for k in keys {
            if let Some(x) = d.get(*k) {
                if let Some(n) = x.as_f64() {
                    return n;
                }
                if let Some(s) = x.as_str().and_then(|s| s.parse::<f64>().ok()) {
                    return s;
                }
            }
        }
        0.0
    };
    SemesterGoal {
        // 真实字段名（7.4.01 实测）：semesterGoal / semesterValid / startDate / endDate
        target_dis: num(&["semesterGoal", "targetDis", "semesterTargetDis", "selDistance"]),
        target_count: num(&["targetCount", "semesterTargetCount", "selCount"]) as i64,
        begin_ms: num(&["startDate", "beginTime", "startTime"]) as i64,
        end_ms: num(&["endDate", "endTime", "stopTime"]) as i64,
        sname: d.get("sname").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        valid_dis: num(&["semesterValid"]),
        goal_method: num(&["goalMethod"]) as i64,
        sid: num(&["sid"]) as i64,
        raw: d.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_summary() {
        let v = json!({
            "data": {
                "sname": "2024-2025学年第二学期",
                "semesterDis": 12500.5,
                "semesterValidDis": 9800.0,
                "semesterCount": 12,
                "semesterValidCount": 9
            }
        });
        let s = parse_summary(&v);
        assert_eq!(s.sname, "2024-2025学年第二学期");
        assert!((s.semester_valid_dis - 9800.0).abs() < 1e-9);
        assert_eq!(s.semester_valid_count, 9);
        assert!((s.valid_ratio() - 0.784).abs() < 0.001);
    }

    #[test]
    fn empty_summary_is_zero() {
        let s = parse_summary(&json!({}));
        assert_eq!(s.semester_dis, 0.0);
        assert_eq!(s.valid_ratio(), 0.0);
    }

    #[test]
    fn parses_goal_variants() {
        // 7.4.01 实测字段名
        let g = parse_goal(&json!({"data": {
            "sid": 9679,
            "sname": "2026-2027-1 2阶段",
            "startDate": 1793548800000i64,
            "endDate": 1796227199000i64,
            "semesterGoal": 60000,
            "semesterValid": 15000,
            "goalMethod": 2
        }}));
        assert_eq!(g.sname, "2026-2027-1 2阶段");
        assert!((g.target_dis - 60000.0).abs() < 1e-9);
        assert!((g.valid_dis - 15000.0).abs() < 1e-9);
        assert_eq!(g.sid, 9679);
        assert_eq!(g.goal_method, 2);
        assert_eq!(g.begin_ms, 1793548800000);
        assert!((g.percent() - 25.0).abs() < 1e-9);
        assert!((g.remaining() - 45000.0).abs() < 1e-9);

        // 旧字段名仍可用
        let g2 = parse_goal(&json!({"data": {"selDistance": "45000", "selCount": "20"}}));
        assert!((g2.target_dis - 45000.0).abs() < 1e-9);
        assert_eq!(g2.target_count, 20);
    }

    #[test]
    fn remaining_is_clamped() {
        let g = SemesterGoal {
            target_dis: 60000.0,
            target_count: 30,
            ..Default::default()
        };
        assert!((g.remaining_dis(20000.0) - 40000.0).abs() < 1e-9);
        assert_eq!(g.remaining_dis(80000.0), 0.0, "超额时不能为负");
        assert_eq!(g.remaining_count(10), 20);
        assert_eq!(g.remaining_count(50), 0);
    }

    #[test]
    fn term_window() {
        let g = SemesterGoal {
            begin_ms: 1_700_000_000_000,
            end_ms: 1_700_000_000_000 + 100 * 86_400_000,
            ..Default::default()
        };
        assert!(g.in_term(1_700_000_000_000 + 50 * 86_400_000));
        assert!(!g.in_term(1_700_000_000_000 - 86_400_000));
        assert_eq!(g.days_left(1_700_000_000_000 + 90 * 86_400_000), 10);
    }
}

#[cfg(test)]
mod real_payload_tests {
    use super::*;

    /// 7.4.01 真实响应（字段名经实测确认，账号信息已脱敏）
    #[test]
    fn parses_real_7401_payload() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"error":10000,"message":"成功","data":{"sid":9679,"sname":"2026-2027-1 2阶段","startDate":1793548800000,"endDate":1796227199000,"semesterGoal":0,"semesterValid":0,"goalMethod":2}}"#,
        ).unwrap();

        let g = parse_goal(&v);
        assert_eq!(g.sname, "2026-2027-1 2阶段", "学期名要能解出来");
        assert_eq!(g.sid, 9679);
        assert_eq!(g.goal_method, 2);
        assert_eq!(g.begin_ms, 1793548800000, "startDate 毫秒");
        assert_eq!(g.end_ms, 1796227199000, "endDate 毫秒");
        assert!(g.end_ms > 0, "结束时间必须解析成功，否则天数算不出来");

        // 2026-11-15 在学期内
        let now = 1_794_700_000_000i64;
        assert!(g.in_term(now));
        let d = g.days_left(now);
        assert!(d > 0 && d < 60, "剩余天数应合理，实际 {d}");
    }
}
