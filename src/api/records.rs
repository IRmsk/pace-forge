//! 历史记录查询。
//!
//! - 列表：`POST /api/v70230/runnings/records` `{pageNum, pageSize, sid, type}`
//! - 详情：`POST /api/v70320/runnings/get_one_record` `{rrid}`
//!
//! 列表接口不含轨迹；详情接口是否含轨迹取决于服务端配置。
//! 前端对原始 JSON 做自动探测，遇到坐标数组就画地图，所以这里原样透传 `raw`。

use serde_json::{json, Value};

use super::client::{expect_ok, Client};

/// 分页拉取跑步记录列表。
pub fn list_records(
    client: &Client,
    page_num: u32,
    page_size: u32,
    sport_type: u32,
) -> Result<Value, String> {
    let raw = client.request(
        "POST",
        &format!("{}/api/v70230/runnings/records", crate::crypto::hosts::RUN),
        Some(&json!({
            "pageNum": page_num,
            "pageSize": page_size,
            "sid": 0,
            "type": sport_type,
        })),
    )?;
    let v = expect_ok(&raw)?;
    Ok(v.get("data").cloned().unwrap_or(Value::Null))
}

/// 拉取单条记录详情（含服务端给的原始字段）。
pub fn get_record(client: &Client, rrid: i64) -> Result<Value, String> {
    let raw = client.request(
        "POST",
        &format!("{}/api/v70320/runnings/get_one_record", crate::crypto::hosts::RUN),
        Some(&json!({"rrid": rrid})),
    )?;
    let v = expect_ok(&raw)?;
    Ok(v.get("data").cloned().unwrap_or(Value::Null))
}

/// 打卡日历（含每月出勤标记）。
pub fn calendar(client: &Client, year: i32, month: i32) -> Result<Value, String> {
    let raw = client.request(
        "POST",
        &format!("{}/api/v70100/run/calendar/list", crate::crypto::hosts::DISCOVERY),
        Some(&json!({"year": year, "month": month})),
    )?;
    let v = expect_ok(&raw)?;
    Ok(v.get("data").cloned().unwrap_or(Value::Null))
}
