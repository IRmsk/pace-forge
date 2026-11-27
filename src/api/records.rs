//! 历史记录查询。
//!
//! - 列表：`POST /api/v70230/runnings/records` `{pageNum, pageSize, sid, type}`
//! - 详情：`POST /api/v70320/runnings/get_one_record` `{rrid}`
//!
//!   ⚠️ 7.4.20 客户端的 Dart 层用的是 **`v70260`**（`libapp.so` 里
//!   `v70260/runnings/get_one_record`），`v70320` 只出现在 `classes.dex`，
//!   说明两套都还在用。我们保留 v70320 是因为实测可用；改回 v70260 需要
//!   一次真机验证（旧结论「App 从不用 v70320」在 7.4.20 上不成立，已撤回）。
//!
//! 列表接口不含轨迹；详情接口是否含轨迹取决于服务端配置。
//! 前端对原始 JSON 做自动探测，遇到坐标数组就画地图，所以这里原样透传 `raw`。

use serde_json::{Map, Value, json};

use super::client::{Client, expect_ok};
use crate::error::Error;

/// 服务端字段名 → 统一字段名的别名表。
///
/// 服务端下发的是 `totalDis` / `validDis`（**单位米**），前端 historically
/// 读的是 `totalDistance` / `validDistance`，于是列表里每条都是
/// `undefined / 1000 = 0.00 km`。前后端各猜一套字段名必然会再错一次，
/// 所以统一在出口处补一份标准字段，前端只读标准字段。
const DIST_ALIASES: &[&str] = &[
    "totalDis",
    "totalDistance",
    "total_distance",
    "distance",
    "runDistance",
    "mileage",
    "dis",
];
const VALID_DIST_ALIASES: &[&str] = &[
    "validDis",
    "validDistance",
    "valid_distance",
    "validMileage",
];
const TIME_ALIASES: &[&str] = &["totalTime", "total_time", "runTime", "duration", "useTime"];
const VALID_TIME_ALIASES: &[&str] = &["validTime", "valid_time", "validDuration"];

/// 从若干别名里取第一个正数。
fn pick_num(obj: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    for k in keys {
        let Some(v) = obj.get(*k) else { continue };
        let n = match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        };
        if let Some(n) = n
            && n > 0.0
        {
            return Some(n);
        }
    }
    None
}

/// 字段存在且已经是数值吗。字符串形式的数字也要重新写成数值，
/// 否则前端 `int()`/`num()` 各处理一次，端口不同表现就不同。
fn has_num(obj: &Map<String, Value>, k: &str) -> bool {
    matches!(obj.get(k), Some(Value::Number(_)))
}

/// 距离统一成**米**。
///
/// 服务端有的版本给米、有的接口给公里（2 还是 2000），单看数字分不清。
/// 判据取 200：跑步记录最短 0.5 km = 500 m，任何小于 200 的正数都不可能是米。
fn to_meters(n: f64) -> f64 {
    if n > 0.0 && n < 200.0 { n * 1000.0 } else { n }
}

/// 给一条记录补上统一字段，原始字段原样保留。
pub fn normalize_record(r: &mut Value) {
    let Some(obj) = r.as_object() else { return };
    let mut add: Vec<(String, Value)> = Vec::new();

    if !has_num(obj, "totalDistance")
        && let Some(d) = pick_num(obj, DIST_ALIASES)
    {
        let m = to_meters(d);
        add.push((
            "totalDistance".into(),
            json!(crate::number::round_to_i64(m).unwrap_or(0)),
        ));
        add.push((
            "distanceKm".into(),
            json!((m / 1000.0 * 100.0).round() / 100.0),
        ));
    }
    if !has_num(obj, "validDistance")
        && let Some(d) = pick_num(obj, VALID_DIST_ALIASES)
    {
        add.push((
            "validDistance".into(),
            json!(crate::number::round_to_i64(to_meters(d)).unwrap_or(0)),
        ));
    }
    if !has_num(obj, "totalTime")
        && let Some(t) = pick_num(obj, TIME_ALIASES)
    {
        add.push((
            "totalTime".into(),
            json!(crate::number::round_to_i64(t).unwrap_or(0)),
        ));
    }
    if !has_num(obj, "validTime")
        && let Some(t) = pick_num(obj, VALID_TIME_ALIASES)
    {
        add.push((
            "validTime".into(),
            json!(crate::number::round_to_i64(t).unwrap_or(0)),
        ));
    }
    if !has_num(obj, "avgStepFreq")
        && let Some(f) = pick_num(obj, &["avgStepFreq", "avg_step_freq", "stepFreq"])
    {
        add.push((
            "avgStepFreq".into(),
            json!(crate::number::round_to_i64(f).unwrap_or(0)),
        ));
    }

    // 列表里的「是否达标」是 complete（bool）。前端曾经读 isValid/status，
    // 两个字段服务端都不给，于是每条都显示「未达标」。
    // 这里额外补一个 completed，任何视图只读它就不会再猜错。
    if obj.get("completed").is_none()
        && let Some(c) = obj.get("complete").and_then(|v| v.as_bool())
    {
        add.push(("completed".into(), json!(c)));
    }

    if let Some(obj) = r.as_object_mut() {
        for (k, v) in add {
            obj.insert(k, v);
        }
    }
}

/// 从列表响应里取出记录数组，兼容**直接数组**与 **`{"records": [...]}`** 两种形态。
///
/// [`normalize_list`] 把列表响应统一成对象 `{"records": [...], total, page, size}`，
/// 而 [`list_records`] 的返回值正是这个对象——所以**只判 `as_array()` 拿不到任何
/// 记录**。这个坑在仓库里已经踩过一次：`semester::derived_count` 里是「数组或
/// `records` 字段」两解，`flow` 里的日上限统计却只判数组，于是「今日已跑」恒为 0，
/// 日上限从来没有真正拦过任何一次提交（实测当天已跑 2262m，日志仍报「今日已跑 0m」）。
///
/// 两处读同一份数据的地方统一走这里，免得再各写各的。
pub fn records_of(v: &Value) -> Vec<Value> {
    match v.as_array() {
        Some(a) => a.clone(),
        None => v
            .get("records")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default(),
    }
}

/// 列表响应归一化：兼容 `{records:[]}` / `{list:[]}` / 直接数组三种形态，
/// 统一成 `{records:[...], total, page, size}`，并逐条补标准字段。
pub fn normalize_list(data: Value, page: u32, size: u32) -> Value {
    let rows: Vec<Value> = match &data {
        Value::Array(a) => a.clone(),
        Value::Object(o) => ["records", "list", "rows", "items", "data"]
            .iter()
            .find_map(|k| o.get(*k).and_then(|v| v.as_array()).cloned())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let total = match &data {
        Value::Object(o) => o
            .get("total")
            .or_else(|| o.get("totalCount"))
            .and_then(|v| v.as_i64())
            .unwrap_or(rows.len() as i64),
        _ => rows.len() as i64,
    };
    let mut out = Vec::with_capacity(rows.len());
    for mut r in rows {
        normalize_record(&mut r);
        out.push(r);
    }
    json!({ "records": out, "total": total, "page": page, "size": size })
}

/// 分页拉取跑步记录列表。
pub fn list_records(
    client: &Client,
    page_num: u32,
    page_size: u32,
    sport_type: u32,
) -> Result<Value, Error> {
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
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    Ok(normalize_list(data, page_num, page_size))
}

/// 拉取单条记录详情（含服务端给的原始字段）。
pub fn get_record(client: &Client, rrid: i64) -> Result<Value, Error> {
    let raw = client.request(
        "POST",
        &format!(
            "{}/api/v70320/runnings/get_one_record",
            crate::crypto::hosts::RUN
        ),
        Some(&json!({"rrid": rrid})),
    )?;
    let v = expect_ok(&raw)?;
    let mut data = v.get("data").cloned().unwrap_or(Value::Null);
    // 详情接口的字段名与列表不一致（validDis / avgStepFreq 等），同样归一化
    normalize_record(&mut data);
    Ok(data)
}

/// 打卡日历（含每月出勤标记）。
pub fn calendar(client: &Client, year: i32, month: i32) -> Result<Value, Error> {
    let raw = client.request(
        "POST",
        &format!(
            "{}/api/v70100/run/calendar/list",
            crate::crypto::hosts::DISCOVERY
        ),
        Some(&json!({"year": year, "month": month})),
    )?;
    let v = expect_ok(&raw)?;
    Ok(v.get("data").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 列表响应归一化之后是**对象** `{"records": [...]}`——所以读记录数组的
    /// 地方只判 `as_array()` 就会拿到空列表。
    ///
    /// 这条守着 `records_of` 的两个方向：对象形态（`list_records` 的真形态）
    /// 与数组形态（服务端偶尔直接给数组）。
    #[test]
    fn records_of_reads_both_shapes() {
        // 对象形态：`normalize_list` 的产物，也是 `list_records` 的返回值。
        let obj = normalize_list(
            json!({"records": [{"totalDis": 2000}, {"totalDis": 3000}]}),
            1,
            30,
        );
        assert!(obj.get("records").is_some(), "归一化后必须是对象形态");
        assert!(obj.as_array().is_none(), "归一化后不是数组形态");
        let got = records_of(&obj);
        assert_eq!(got.len(), 2, "对象形态必须能读出记录");
        assert_eq!(got[0]["totalDis"], json!(2000));

        // 数组形态：兼容老接口。
        let arr = json!([{"totalDis": 500}]);
        assert_eq!(records_of(&arr).len(), 1);

        // 别的键名（`list`/`rows`…）由 `normalize_list` 归一过了，
        // 这里只保证拿不到东西时是空数组而不是 panic。
        assert!(records_of(&json!({"nope": 1})).is_empty());
        assert!(records_of(&Value::Null).is_empty());
    }

    /// 守的是「日上限统计」这条链：`list_records` 的返回值（对象）喂给
    /// `records_of` 必须真的能读出记录，否则「今日已跑」恒为 0 ⇒
    /// 日上限从来没真正拦过任何一次提交。
    #[test]
    fn daily_cap_source_is_readable() {
        let v = normalize_list(
            json!({"records": [{"startTime": 1_700_000_000_000i64, "totalDis": 2262}]}),
            1,
            30,
        );
        let arr = records_of(&v);
        assert_eq!(arr.len(), 1, "今日已跑的记录必须读得到");
        assert_eq!(
            arr[0]["totalDis"].as_f64(),
            Some(2262.0),
            "里程字段是 totalDis（米）"
        );
    }

    #[test]
    fn normalizes_meters_from_total_dis() {
        // 服务端真实字段：totalDis（米）
        let mut r = json!({"rrid": 1790000000001i64, "totalDis": 2000, "totalTime": 720,
                           "validDis": 1850, "validTime": 700, "avgStepFreq": 168});
        normalize_record(&mut r);
        assert_eq!(r["totalDistance"], json!(2000), "米不需要换算");
        assert_eq!(r["distanceKm"], json!(2.0));
        assert_eq!(r["validDistance"], json!(1850));
        assert_eq!(r["validTime"], json!(700));
        // 原始字段必须还在，别把详情页依赖的字段删了
        assert_eq!(r["totalDis"], json!(2000));
        assert_eq!(r["rrid"], json!(1790000000001i64));
    }

    #[test]
    fn upgrades_kilometers_to_meters() {
        // 有的接口给公里：2 而不是 2000
        let mut r = json!({"totalDis": 2.5, "validDis": 2.4});
        normalize_record(&mut r);
        assert_eq!(r["totalDistance"], json!(2500));
        assert_eq!(r["validDistance"], json!(2400));
        assert_eq!(r["distanceKm"], json!(2.5));
    }

    #[test]
    fn accepts_string_numbers() {
        let mut r = json!({"totalDis": "3200.50", "totalTime": "960"});
        normalize_record(&mut r);
        assert_eq!(
            r["totalDistance"],
            json!(3201),
            "字符串数字要能解析并四舍五入"
        );
        assert_eq!(r["totalTime"], json!(960));
    }

    #[test]
    fn keeps_existing_unified_fields() {
        let mut r = json!({"totalDistance": 1500, "totalDis": 9999});
        normalize_record(&mut r);
        assert_eq!(r["totalDistance"], json!(1500), "已有标准字段时不能覆盖");
    }

    #[test]
    fn ignores_zero_and_missing() {
        let mut r = json!({"totalDis": 0, "validDis": 0});
        normalize_record(&mut r);
        assert!(r.get("totalDistance").is_none(), "0 不算有效里程");
        let mut empty = json!({});
        normalize_record(&mut empty);
        assert_eq!(empty, json!({}));
    }

    #[test]
    fn list_response_is_unified() {
        // 三种历史形态都要统一成 {records,total,page,size}
        let a = normalize_list(json!([{"totalDis": 2000}]), 1, 20);
        assert_eq!(a["records"][0]["distanceKm"], json!(2.0));
        assert_eq!(a["total"], json!(1));
        assert_eq!(a["page"], json!(1));

        let b = normalize_list(json!({"records": [{"totalDis": 3000}], "total": 42}), 2, 10);
        assert_eq!(b["records"][0]["distanceKm"], json!(3.0));
        assert_eq!(b["total"], json!(42));
        assert_eq!(b["page"], json!(2));

        let c = normalize_list(json!({"list": [{"totalDis": 500}]}), 1, 20);
        assert_eq!(c["records"][0]["distanceKm"], json!(0.5));

        let d = normalize_list(Value::Null, 1, 20);
        assert_eq!(d["records"], json!([]));
        assert_eq!(d["total"], json!(0));
    }

    #[test]
    fn mirrors_complete_into_completed() {
        let mut r = json!({"rrid": 1, "totalDis": 2056, "complete": true,
                           "status": 0, "statusInfo": "太棒了，恭喜您完成了跑步"});
        normalize_record(&mut r);
        assert_eq!(r["completed"], json!(true), "达标判定必须能别名字段读出来");
        assert_eq!(r["complete"], json!(true));
        assert_eq!(r["status"], json!(0), "status=0 不代表未达标，别拿它当判据");
    }

    #[test]
    fn completed_false_is_kept() {
        let mut r = json!({"complete": false});
        normalize_record(&mut r);
        assert_eq!(r["completed"], json!(false));
    }
}
