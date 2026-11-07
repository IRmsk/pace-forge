//! 提交报文构建。
//!
//! 两条约束最容易踩：
//! 1. **签名参与字段 ≠ 报文字段**。签名字段 31 个，报文字段 45 个，
//!    差集是 `allLocJson` / `fivePointJson` / `signature` 等大字段（不进签名，
//!    否则报文太大且服务端也是这么算的）。
//! 2. **`speed` 是配速（分/公里）× 1000**，不是速度 m/s × 1000。
//!    判据是规则字段 `speedTop/speedBottom` 的单位为分/公里。
//!
//! 报文里 `allLocJson` / `fivePointJson` / `speedPerTenSec` 等是**字符串化的 JSON**
//! （双重序列化），不是嵌套对象。服务端按字符串取出来再解析一层。

use serde_json::{json, Value};

use super::client::Client;

use crate::crypto::sign::{sport_signature, SignFields};
use crate::track::buckets::{speed_buckets, step_buckets, StepBucket};
use crate::track::TrackPoint;

/// 每公里消耗千卡。
const CALORIE_PER_KM: f64 = 62.0;
/// 最长允许跑步时长（秒）。
pub const MAX_RUN_TIME: i64 = 7200;
/// 最少步数。
pub const MIN_STEPS: i32 = 2000;

/// 构建参数。
#[derive(Debug, Clone)]
pub struct SubmitParams {
    pub uid: String,
    pub unid: String,
    pub uuid: String,
    pub sport_type: i32,
    pub total_distance_m: i64,
    pub total_time_sec: i64,
    pub total_steps: i32,
    pub start_time_ms: i64,
    pub stop_time_ms: i64,
    /// 学期目标距离（米），决定 `complete`。
    pub sel_distance_m: i64,
    /// 学期目标时长（秒）。
    pub sel_run_time_sec: i64,
    /// 规则串，服务端原样回显。
    pub policy: String,
    pub run_area_id: i64,
    pub geo_fences_json: String,
    pub total_ascent: i32,
}

/// 打卡点。
#[derive(Debug, Clone)]
pub struct FixedPoint {
    pub lat: f64,
    pub lon: f64,
    pub position: i32,
}

/// 构建结果。
#[derive(Debug, Clone)]
pub struct Submission {
    /// `save/record` 的 body。
    pub save_record: Value,
    /// OBS 直传的对象 key。
    pub object_key: String,
    /// 供排查用的签名原文。
    pub canonical: String,
}

/// 单个轨迹点在 `allLocJson` 里的完整结构。
///
/// 字段名与语义对齐服务端：`lat`/`lng` 是**占位**（固定 -1.0），真实坐标走
/// `gLat`/`gLng` 且必须是 gcj02；`speed` 单位 km/h，`avgSpeed` 单位 m/s。
#[allow(clippy::too_many_arguments)]
pub fn point_json_full(
    p: &TrackPoint,
    idx: usize,
    n: usize,
    start_ms: i64,
    start_pt: (f64, f64),
    prev: &mut (f64, f64),
    cum: &mut (f64, i64, i64),
    prev_time: &mut i64,
    total_steps: i64,
    seed: u64,
) -> Value {
    // BD-09 → GCJ-02（原点是 BD-09，服务端要 gcj02）
    let (glat, glng) = crate::api::coord::bd09_to_gcj02(p.lat, p.lon);

    // 累计量
    let step_m = if idx == 0 {
        0.0
    } else {
        crate::track::geo::distance(*prev, (p.lat, p.lon))
    };
    cum.0 += step_m;
    cum.1 = (p.time_ms - start_ms) / 1000;
    *prev = (p.lat, p.lon);

    // 瞬时速度用真实采样间隔，不是固定 5 秒
    let gap_sec = if idx == 0 {
        0.0
    } else {
        ((p.time_ms - *prev_time) as f64 / 1000.0).max(0.1)
    };
    *prev_time = p.time_ms;

    let dt = if idx == 0 { 0.0 } else { (p.time_ms - start_ms) as f64 / 1000.0 };
    let avg_speed = if dt > 0.5 { (cum.0 / dt).clamp(0.0, 6.3) } else { 0.0 };
    // 夹到跑步的物理区间（1.9~6.3 m/s），和规则里的 SPEED_FLOOR/CEIL 一致
    let inst = if idx == 0 {
        // 起点给个常见起步速度，不能是 0
        2.6
    } else {
        (step_m / gap_sec).clamp(1.9, 6.3) * 3.6
    };

    // 点位类型：起点 0，途中以 0/3 为主，偶发 1/2
    let ptype = if idx == 0 {
        0
    } else if idx + 1 == n {
        6
    } else {
        // 用坐标做确定性伪随机，同一轨迹重放结果一致
        let h = (p.lat.to_bits() ^ p.lon.to_bits() ^ seed).wrapping_mul(6364136223846793005);
        match (h >> 33) % 100 {
            0..=38 => 3,
            39..=92 => 0,
            93..=95 => 1,
            96..=97 => 2,
            _ => 8,
        }
    };

    // GPS 精度半径
    let h2 = (p.lon.to_bits() ^ seed).wrapping_mul(6364136223846793005);
    let radius = 1.5 + ((h2 >> 40) % 150) as f64 / 100.0;

    json!({
        "avgSpeed": (avg_speed * 10000.0).round() / 10000.0,
        // 海拔三兄弟不能是 0：服务端按它们算累计爬升，全 0 等于「在海底跑」
        "bdA": 1.0,
        "bdD": 0.0,
        "bdG": 1,
        "bdS": 1.0,
        "coorType": "gcj02",
        "count": 1,
        "dtr": 0.0,
        "flag": start_ms,
        "gLat": (glat * 1e7).round() / 1e7,
        "gLng": (glng * 1e7).round() / 1e7,
        "gainTime": "",
        "id": idx as i64 + 1,
        "lat": -1.0,
        "lng": -1.0,
        "locType": 1,
        "locationId": "",
        "queueNum": idx as i32,
        "radius": (radius * 100.0).round() / 100.0,
        "speed": (inst * 10000.0).round() / 10000.0,
        "state": if idx == 0 { 1 } else { 0 },
        "steps": ((total_steps as f64) * (idx as f64 + 1.0) / n as f64).round() as i64,
        "stepDistance": if cum.0 > 1.0 {
            ((cum.0 / cum.0.max(1.0)) * (cum.0 / (total_steps as f64).max(1.0)) * 10000.0).round() / 10000.0
        } else {
            0.0
        },
        "totalDis": (cum.0 * 10000.0).round() / 10000.0,
        "totalTime": cum.1,
        "type": ptype,
        "validDis": (cum.0 * 10000.0).round() / 10000.0,
        "validTime": cum.1,
    })
}

/// 从**已组装好的报文**反解出校验视图。
///
/// 这是「自检守 A、提交发 B」的根治手段：不再让 preflight 吃内存里的
/// `sim`，而是先把报文 JSON 造出来、再反解回来校验。检查对象和发送对象
/// 从此物理上是同一份数据——不管以后谁在 build() 里塞新东西，只要它进了
/// 报文就会被检查到，进不了报文的东西也不会被误认为已检查。
#[derive(Debug, Clone)]
pub struct VerifyView {
    /// 从报文点位反解出的轨迹（时间戳来自 totalTime）。
    pub points: Vec<TrackPoint>,
    pub total_distance_m: f64,
    pub total_time_sec: i64,
    pub total_steps: i32,
    pub start_time_ms: i64,
    pub stop_time_ms: i64,
    /// 报文里的步数桶（原样）。
    pub step_buckets: Vec<crate::track::StepBucket>,
    /// 报文里的速度桶（原样）。
    pub speed_buckets: Vec<crate::track::SpeedBucket>,
    /// 报文声明的配速字段（分/公里 × 1000）。
    pub speed_field: i64,
    /// 点位条数。
    pub point_count: usize,
}

impl VerifyView {
    /// 包成 preflight 要的 SimResult 形状。
    ///
    /// 只填能从报文读到的东西；读不到的（瞬时速度、步幅）留空，
    /// 让相关检查自己跳过，而不是拿内存里的值冒充。
    pub fn as_sim(&self) -> crate::track::SimResult {
        crate::track::SimResult {
            speeds: Vec::new(),
            cadences: Vec::new(),
            strides: Vec::new(),
            total_steps: self.total_steps,
            avg_cadence: if self.total_time_sec > 0 {
                self.total_steps as f64 / (self.total_time_sec as f64 / 60.0)
            } else {
                0.0
            },
            speed_buckets: self.speed_buckets.clone(),
            step_buckets: self.step_buckets.clone(),
            actual_length_m: self.total_distance_m,
        }
    }
}

/// 从提交报文反解出校验视图。字段名和 `point_json_full` / `build` 一一对应。
pub fn verify_view(body: &Value) -> Result<VerifyView, String> {
    let start_ms = body
        .get("startTime")
        .and_then(|v| v.as_i64())
        .or_else(|| {
            body.get("allLocJson")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok())
                .and_then(|a| a.first().and_then(|p| p.get("flag")).and_then(|f| f.as_i64()))
        })
        .ok_or("报文缺 startTime")?;
    let stop_ms = body.get("stopTime").and_then(|v| v.as_i64()).unwrap_or(0);

    let raw = body
        .get("allLocJson")
        .and_then(|v| v.as_str())
        .ok_or("报文缺 allLocJson")?;
    let arr: Vec<Value> = serde_json::from_str(raw).map_err(|e| format!("allLocJson 解析失败: {e}"))?;
    if arr.is_empty() {
        return Err("allLocJson 为空".into());
    }

    let mut points: Vec<TrackPoint> = Vec::with_capacity(arr.len());
    for (i, q) in arr.iter().enumerate() {
        // 真实坐标在 gLat/gLng（GCJ-02）；lat/lng 是 -1 占位
        let glat = q.get("gLat").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let glng = q.get("gLng").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if glat == 0.0 || glng == 0.0 {
            return Err(format!("第 {i} 个点缺 gLat/gLng"));
        }
        let (lat, lon) = crate::api::coord::gcj02_to_bd09(glat, glng);
        let total_time = q.get("totalTime").and_then(|v| v.as_i64()).unwrap_or(0);
        points.push(TrackPoint {
            lat,
            lon,
            time_ms: start_ms + total_time * 1000,
        });
    }

    let last = arr.last().unwrap();
    let total_distance_m = last
        .get("totalDis")
        .and_then(|v| v.as_f64())
        .ok_or("末点缺 totalDis")?;
    let total_time_sec = last
        .get("totalTime")
        .and_then(|v| v.as_i64())
        .ok_or("末点缺 totalTime")?;
    let total_steps = last
        .get("steps")
        .and_then(|v| v.as_i64())
        .ok_or("末点缺 steps")? as i32;

    let parse_buckets = |key: &str| -> Vec<Value> {
        match body.get(key) {
            Some(Value::Array(a)) => a.clone(),
            Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_default(),
            _ => Vec::new(),
        }
    };

    let step_buckets: Vec<crate::track::StepBucket> = parse_buckets("stepsPerTenSec")
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    let speed_buckets: Vec<crate::track::SpeedBucket> = parse_buckets("speedPerTenSec")
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();

    Ok(VerifyView {
        point_count: points.len(),
        points,
        total_distance_m,
        total_time_sec,
        total_steps,
        start_time_ms: start_ms,
        stop_time_ms: if stop_ms > 0 {
            stop_ms
        } else {
            start_ms + total_time_sec * 1000
        },
        step_buckets,
        speed_buckets,
        speed_field: body.get("speed").and_then(|v| v.as_i64()).unwrap_or(0),
    })
}

/// 计算 `speed` 字段：配速（分/公里）× 1000。
pub fn speed_field(distance_m: f64, time_sec: i64) -> i64 {
    if distance_m <= 0.0 || time_sec <= 0 {
        return 0;
    }
    let pace_min = (time_sec as f64 / 60.0) / (distance_m / 1000.0);
    (pace_min * 1000.0).round() as i64
}

/// 构建完整提交报文。
pub fn build(
    params: &SubmitParams,
    points: &[TrackPoint],
    fixed: &[FixedPoint],
    sim: &crate::track::SimResult,
) -> Result<Submission, String> {
    if points.is_empty() {
        return Err("轨迹为空，无法组装报文".into());
    }
    if params.total_distance_m <= 0 || params.total_steps < 0 {
        return Err("距离必须为正，步数不能为负".into());
    }

    let complete = params.total_distance_m >= params.sel_distance_m
        && params.total_time_sec >= params.sel_run_time_sec;
    let un_complete_reason = if complete { 0 } else { 10 };
    let avg_step_freq = if params.total_time_sec > 0 {
        (params.total_steps as f64 * 60.0 / params.total_time_sec as f64).round() as i64
    } else {
        0
    };
    let speed_val = speed_field(params.total_distance_m as f64, params.total_time_sec);
    let calorie = (params.total_distance_m as f64 / 1000.0 * CALORIE_PER_KM) as i64;

    // 轨迹点带瞬时速度（中心差分），服务端画图会用到
    let pts: Vec<(f64, f64, i64)> =
        points.iter().map(|p| (p.lat, p.lon, p.time_ms)).collect();
    // 单一数据源：直接用 simulate() 产出的桶。
    //
    // 之前这里拿 make_jitter(amp=1.0) **重新生成**了一套桶写进报文，而自检
    // 校验的是 sim.step_buckets、OBS 明细传的也是 sim.step_buckets——
    // 自检守 A、提交发 B、明细传 A，反检测设计整个落空。
    // amp=1.0 还是 ±100% 的均匀抖动，会造出 0 步桶（10 秒静止）。
    let sb: Vec<crate::track::SpeedBucket> = sim.speed_buckets.clone();
    let stb: Vec<crate::track::StepBucket> = sim.step_buckets.clone();

    // 逐点生成，累计量必须顺序推进
    let mut prev = points.first().map(|p| (p.lat, p.lon)).unwrap_or((0.0, 0.0));
    let start_pt = prev;
    let mut cum = (0.0f64, 0i64, 0i64);
    let seed = params.start_time_ms as u64 ^ 0x9E3779B97F4A7C15;
    let mut prev_time = points.first().map(|p| p.time_ms).unwrap_or(params.start_time_ms);
    let loc_items: Vec<Value> = points
        .iter()
        .enumerate()
        .map(|(i, p)| {
            point_json_full(
                p, i, points.len(), params.start_time_ms, start_pt,
                &mut prev, &mut cum, &mut prev_time, params.total_steps as i64, seed,
            )
        })
        .collect();
    let all_loc_json = serde_json::to_string(&loc_items).map_err(|e| e.to_string())?;

    let fp_items: Vec<Value> = fixed
        .iter()
        .map(|f| json!({"lat": f.lat, "lon": f.lon, "position": f.position}))
        .collect();
    // fivePointJson 是一层 envelope，不是裸数组：服务端要同时拿到点位列表、
    // 围栏回显和 runAreaId 才能校验打卡合规。
    let five_envelope = json!({
        "fivePointJson": serde_json::to_string(&fp_items).map_err(|e| e.to_string())?,
        "useZip": false,
        "freedomShowFence": false,
        "runAreaId": params.run_area_id,
        "geoFencesJson": params.geo_fences_json,
    });
    let five_pt_json = serde_json::to_string(&five_envelope).map_err(|e| e.to_string())?;

    // 提交报文的桶 id 规则：60000+i，queueNum=i
    let mut sb = sb;
    let mut stb = stb;
    crate::track::buckets::apply_submit_ids(&mut sb, &mut stb);
    // 服务端要的是**数组**，不是字符串化的 JSON。
    // 早先按 allLocJson 的模式套成了字符串，结果提交回来 -10000。
    let speed_arr = serde_json::to_value(&sb).map_err(|e| e.to_string())?;
    let step_arr = serde_json::to_value(&stb).map_err(|e| e.to_string())?;

    // ---- 签名（31 字段）----
    let sign_fields: SignFields = vec![
        ("address".into(), Some(String::new())),
        ("avgPower".into(), Some("0".into())),
        ("avgStepFreq".into(), Some(avg_step_freq.to_string())),
        ("calorie".into(), Some(calorie.to_string())),
        ("complete".into(), Some(complete.to_string())),
        ("errorCode".into(), Some("0".into())),
        ("faceCheck".into(), Some("0".into())),
        ("geeToken".into(), Some(String::new())),
        // Android 的 String.valueOf(false) 是小写；写成 "False" 签名必然不过（10501）
        ("getPrize".into(), Some("false".into())),
        // 签名侧固定字符串 "null"，body 侧才是 JSON null
        ("goalId".into(), Some("null".into())),
        ("policy".into(), Some(params.policy.clone())),
        ("selDistance".into(), Some(params.sel_distance_m.to_string())),
        ("selRunTime".into(), Some(params.sel_run_time_sec.to_string())),
        ("selectedUnid".into(), Some(params.unid.clone())),
        ("speed".into(), Some(speed_val.to_string())),
        ("sportType".into(), Some(params.sport_type.to_string())),
        ("startTime".into(), Some(params.start_time_ms.to_string())),
        ("status".into(), Some("0".into())),
        ("stopTime".into(), Some(params.stop_time_ms.to_string())),
        ("themeId".into(), Some("0".into())),
        ("totalAscent".into(), Some(params.total_ascent.to_string())),
        ("totalDis".into(), Some(params.total_distance_m.to_string())),
        ("totalSteps".into(), Some(params.total_steps.to_string())),
        ("totalTime".into(), Some(params.total_time_sec.to_string())),
        ("uid".into(), Some(params.uid.clone())),
        ("unCompleteReason".into(), Some(un_complete_reason.to_string())),
        ("unauthorized".into(), Some("0".into())),
        ("useMobilityTools".into(), Some("0".into())),
        ("uuid".into(), Some(params.uuid.clone())),
        ("validDis".into(), Some(params.total_distance_m.to_string())),
        ("validTime".into(), Some(params.total_time_sec.to_string())),
    ];
    let signature = sport_signature(&sign_fields);
    let canonical = crate::crypto::sign::canonical_string(&sign_fields);

    let save_record = json!({
        "address": "",
        "allLocJson": all_loc_json,
        "avgPower": 0,
        "avgStepFreq": avg_step_freq,
        "calorie": calorie,
        "complete": complete,
        "errorCode": 0,
        "faceCheck": 0,
        "fivePointJson": five_pt_json,
        "geeToken": "",
        "getPrize": false,
        "goalId": Value::Null,
        "isUpload": false,
        "latitude": points[0].lat,
        "longitude": points[0].lon,
        // 服务端从策略里取，客户端传 0
        "maxRunTime": 0,
        "minSteps": 0,
        "policy": params.policy,
        "recordUrl": "",
        "roomId": 0,
        "runAreaId": params.run_area_id,
        "segmentJson": "[]",
        "selDistance": params.sel_distance_m,
        "selRunTime": params.sel_run_time_sec,
        "selectedUnid": params.unid,
        "more": false,
        "originalSign": crate::crypto::sign::original_sign(&sign_fields),
        "signature": signature,
        "speed": speed_val,
        "speedPerTenSec": speed_arr,
        "sportType": params.sport_type,
        "startTime": params.start_time_ms,
        "status": 0,
        "stepsPerTenSec": step_arr,
        "stopTime": params.stop_time_ms,
        "themeId": 0,
        "totalAscent": params.total_ascent,
        "totalDis": params.total_distance_m,
        "totalSteps": params.total_steps,
        "totalTime": params.total_time_sec,
        "uid": params.uid,
        "unCompleteReason": un_complete_reason,
        "unauthorized": 0,
        "useMobilityTools": 0,
        "uuid": params.uuid,
        "validDis": params.total_distance_m,
        "validTime": params.total_time_sec,
    });

    // 真正的 objectKey 要等提交返回 rrid 才能算，这里先给出 detail key 的雏形
    let object_key = crate::api::obs::detail_key(params.start_time_ms, &params.uuid);

    Ok(Submission { save_record, object_key, canonical })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 给旧测试造一份 sim，桶用确定值（不依赖 OU 随机）。
    pub(super) fn test_sim(p: &SubmitParams, pts: &[TrackPoint]) -> crate::track::SimResult {
        let _ = p;
        let n = (p.total_time_sec / 10).max(1) as usize;
        let per = (p.total_steps.max(0) as f64 / n as f64).ceil() as i32;
        crate::track::SimResult {
            speeds: vec![2.8; pts.len().max(1)],
            cadences: vec![160.0; pts.len().max(1)],
            strides: vec![1.05; pts.len().max(1)],
            total_steps: p.total_steps,
            avg_cadence: 160.0,
            speed_buckets: (0..n)
                .map(|i| crate::track::SpeedBucket {
                    distance: 28.0,
                    begin_time: p.start_time_ms + i as i64 * 10_000,
                    end_time: p.start_time_ms + (i as i64 + 1) * 10_000,
                    flag: p.start_time_ms,
                    id: 60_000 + i as i64,
                    state: 0,
                    queue_num: i as i32,
                })
                .collect(),
            step_buckets: (0..n)
                .map(|i| crate::track::StepBucket {
                    steps_num: per,
                    begin_time: p.start_time_ms + i as i64 * 10_000,
                    end_time: p.start_time_ms + (i as i64 + 1) * 10_000,
                    flag: p.start_time_ms,
                    id: 60_000 + i as i64,
                    max_diff: 2,
                    min_diff: 0,
                    avg_diff: 1,
                    state: 0,
                    queue_num: i as i32,
                })
                .collect(),
            actual_length_m: p.total_distance_m as f64,
        }
    }

    fn params() -> SubmitParams {
        SubmitParams {
            uid: "123456".into(),
            unid: "88".into(),
            uuid: "00000000-0000-4000-8000-000000000001".into(),
            sport_type: 1,
            total_distance_m: 2000,
            total_time_sec: 720,
            total_steps: 2300,
            start_time_ms: 1_730_000_000_000,
            stop_time_ms: 1_730_000_720_000,
            sel_distance_m: 1800,
            sel_run_time_sec: 600,
            policy: "policy_str".into(),
            run_area_id: 7,
            geo_fences_json: "[]".into(),
            total_ascent: 0,
        }
    }

    fn points() -> Vec<TrackPoint> {
        let n = 200;
        let step = 720_000 / (n - 1) as i64;
        (0..n)
            .map(|i| TrackPoint {
                lat: 30.5 + i as f64 * 1e-5,
                lon: 104.0,
                time_ms: if i == n - 1 {
                    1_730_000_000_000 + 720_000
                } else {
                    1_730_000_000_000 + i as i64 * step
                },
            })
            .collect()
    }

    #[test]
    fn speed_is_pace_not_velocity() {
        // 2000m / 720s → 配速 6.0 分/km → 6000
        assert_eq!(speed_field(2000.0, 720), 6000);
        // 若误按 m/s×1000 会得到 2777，明显不同
        assert_ne!(speed_field(2000.0, 720), (2000.0f64 / 720.0 * 1000.0).round() as i64);
        assert_eq!(speed_field(0.0, 720), 0);
        assert_eq!(speed_field(2000.0, 0), 0);
    }

    #[test]
    fn payload_has_all_required_fields() {
        let sim = test_sim(&params(), &points());
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        let v = &sub.save_record;
        for k in [
            "allLocJson", "fivePointJson", "speedPerTenSec", "stepsPerTenSec",
            "signature", "speed", "avgStepFreq", "complete", "unCompleteReason",
            "totalDis", "totalTime", "totalSteps", "uid", "uuid", "sportType",
        ] {
            assert!(v.get(k).is_some(), "缺字段 {k}");
        }
    }

    #[test]
    fn nested_json_is_double_serialized() {
        let sim = test_sim(&params(), &points());
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        let v = &sub.save_record;
        // allLocJson 是字符串化的 JSON（双重序列化）
        assert!(v["allLocJson"].is_string(), "allLocJson 应为字符串");
        let inner: Value = serde_json::from_str(v["allLocJson"].as_str().unwrap()).unwrap();
        assert!(inner.is_array(), "allLocJson 解一层后应是数组");

        // speedPerTenSec / stepsPerTenSec 是**数组**，不要套成字符串。
        // 早先按 allLocJson 的模式一起套了，提交回来 -10000。
        assert!(v["speedPerTenSec"].is_array(), "speedPerTenSec 应为数组");
        assert!(v["stepsPerTenSec"].is_array(), "stepsPerTenSec 应为数组");
        assert!(v["speedPerTenSec"].as_array().unwrap().len() > 0);
        assert!(v["stepsPerTenSec"].as_array().unwrap().len() > 0);
    }

    /// 签名里的 bool 必须是小写。
    ///
    /// Android 的 `String.valueOf(false)` 是 `"false"`，而 Rust 的
    /// `false.to_string()` 是 `"false"`、手写 `"False"` 就成了大写。
    /// 差一个字母，服务端直接回 10501 验证签名失败。
    #[test]
    fn sign_bool_is_lowercase() {
        let sim = test_sim(&params(), &points());
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        let orig = sub.save_record["originalSign"].as_str().unwrap();
        assert!(
            !orig.contains("=True") && !orig.contains("=False"),
            "originalSign 里出现大写 bool: {orig}"
        );
        assert!(orig.contains("getPrize=false"), "getPrize 应为小写: {orig}");
        assert!(orig.contains("complete="), "complete 应参与签名");
    }

    #[test]
    fn signature_is_32_hex() {
        let sim = test_sim(&params(), &points());
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        let sig = sub.save_record["signature"].as_str().unwrap();
        assert_eq!(sig.len(), 32);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn complete_flag_follows_targets() {
        let sim = test_sim(&params(), &points());
        // 达标
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        assert_eq!(sub.save_record["complete"], true);
        assert_eq!(sub.save_record["unCompleteReason"], 0);

        // 不达标
        let mut p = params();
        p.sel_distance_m = 5000;
        let sub2 = build(&p, &points(), &[], &sim).unwrap();
        assert_eq!(sub2.save_record["complete"], false);
        assert_eq!(sub2.save_record["unCompleteReason"], 10);
    }

    #[test]
    fn buckets_match_duration() {
        let sim = test_sim(&params(), &points());
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        let sb = sub.save_record["speedPerTenSec"]
            .as_array()
            .expect("speedPerTenSec 应为数组");
        assert_eq!(sb.len(), 72, "720s 应有 72 个桶");
        let st = sub.save_record["stepsPerTenSec"]
            .as_array()
            .expect("stepsPerTenSec 应为数组");
        assert_eq!(st.len(), 72, "步数桶也要 72 个");
    }

    #[test]
    fn object_key_shape() {
        let sim = test_sim(&params(), &points());
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        // 详情页 key：run_data/{YYYYMMDDHH}/{uuid}.json
        assert!(sub.object_key.starts_with("run_data/"), "{}", sub.object_key);
        assert!(sub.object_key.ends_with(".json"), "{}", sub.object_key);
        assert!(sub.object_key.contains("00000000-0000-4000-8000-000000000001"));
    }

    #[test]
    fn submit_buckets_use_60000_ids() {
        let sim = test_sim(&params(), &points());
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        let sp = &sub.save_record["speedPerTenSec"];
        let arr = sp.as_array().expect("speedPerTenSec 应该是数组");
        assert!(!arr.is_empty());
        assert_eq!(arr[0]["id"], 60000);
        assert_eq!(arr[0]["queueNum"], 0);
        assert_eq!(arr[1]["id"], 60001);
        assert_eq!(arr[1]["queueNum"], 1);

        // stepsPerTenSec 用同样的 id 规则
        let sa = sub.save_record["stepsPerTenSec"].as_array().expect("应为数组");
        assert_eq!(sa[0]["id"], 60000);
        assert_eq!(sa[0]["queueNum"], 0);
    }

    #[test]
    fn rejects_empty_track() {
        assert!(build(&params(), &[], &[], &test_sim(&params(), &[])).is_err());
    }
}

/// 提交成绩：`POST /api/v70260/runnings/save/record`。
///
/// 这是整条链路唯一会产生真实记录的请求。失败要区分两类：
/// - 网络/加密失败 → 可重试
/// - 业务驳回 → **不能重试**，错误次数会累积
///
/// 成功码是 **10000**，不是 0。这一点很容易踩：接口在顶层 `error` 里回
/// `{"error":10000,"message":"成功"}`，照「err != 0 就是失败」判断就会把
/// 一次成功的提交当成驳回，用户看到「提交被驳回 error=10000: 成功」。
pub fn save_record(client: &Client, body: &Value) -> Result<Value, String> {
    let raw = client.request(
        "POST",
        &format!("{}/api/v70260/runnings/save/record", crate::crypto::consts::hosts::RUN),
        Some(body),
    )?;
    let v = crate::api::client::expect_ok(&raw)?;
    let err = v.get("error").and_then(|e| e.as_i64()).unwrap_or(0);
    // 10000 是成功码（和 HTTP 的 200 一个道理，别拿 0 当成功）
    if err != 0 && err != 10000 {
        let msg = v
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("未知错误");
        let hint = match err {
            11016 => "（不在有效跑步时段，记分跑有固定时段限制，等等再来）",
            10003 => "（签名错误，检查 device_id 是否被重置）",
            10121 => "（账号已在其他设备登录，先退出或等 1 小时）",
            10603 => "（请求过于频繁，被限流）",
            _ => "",
        };
        return Err(format!("提交被驳回 error={err}: {msg}{hint}"));
    }
    Ok(v.get("data").cloned().unwrap_or(Value::Null))
}

/// 提交后的记录 id。
pub fn extract_record_id(data: &Value) -> Option<i64> {
    data.get("rrid")
        .or_else(|| data.get("id"))
        .and_then(|x| x.as_i64())
        .or_else(|| {
            data.get("recordId")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse().ok())
        })
}

#[cfg(test)]
mod point_shape_tests {
    use super::tests::test_sim;
    use super::*;

    fn params() -> SubmitParams {
        SubmitParams {
            uid: "123456".into(),
            unid: "88".into(),
            uuid: "00000000-0000-4000-8000-000000000001".into(),
            sport_type: 1,
            total_distance_m: 2000,
            total_time_sec: 720,
            total_steps: 2300,
            start_time_ms: 1_790_000_000_000,
            stop_time_ms: 1_790_000_720_000,
            sel_distance_m: 1800,
            sel_run_time_sec: 600,
            policy: "policy_str".into(),
            run_area_id: 7,
            geo_fences_json: "[]".into(),
            total_ascent: 0,
        }
    }

    fn tp(lat: f64, lon: f64, t: i64) -> TrackPoint {
        TrackPoint { lat, lon, time_ms: t }
    }

    #[test]
    fn all_loc_json_has_server_fields() {
        let pts = vec![
            tp(31.2304, 121.4737, 1_790_000_000_000),
            tp(31.2306, 121.4741, 1_790_000_005_000),
            tp(31.2309, 121.4745, 1_790_000_010_000),
        ];
        let params = params();
        let sim = test_sim(&params, &pts);
        let sub = build(&params, &pts, &[], &sim).unwrap();
        let s = sub.save_record["allLocJson"].as_str().unwrap();
        let arr: Vec<serde_json::Value> = serde_json::from_str(s).unwrap();
        assert_eq!(arr.len(), 3);

        let p0 = &arr[0];
        // 占位坐标必须是 -1
        assert_eq!(p0["lat"], -1.0, "lat 是占位");
        assert_eq!(p0["lng"], -1.0, "lng 是占位");
        // 真实坐标在 gLat/gLng，且已转到 gcj02
        assert!(p0["gLat"].as_f64().unwrap() > 31.0);
        assert!(p0["gLng"].as_f64().unwrap() > 121.0);
        assert_ne!(p0["gLat"].as_f64().unwrap(), 31.2304, "应该是转换后的坐标");
        assert_eq!(p0["coorType"], "gcj02");

        // 关键字段齐全
        for k in [
            "avgSpeed", "flag", "id", "locType", "queueNum", "radius", "speed",
            "state", "steps", "totalDis", "totalTime", "type", "validDis", "validTime",
        ] {
            assert!(p0.get(k).is_some(), "缺字段 {k}");
        }

        // 起点与终点的 type
        assert_eq!(arr[0]["type"], 0, "起点 type=0");
        assert_eq!(arr[2]["type"], 6, "终点 type=6");

        // 累计量递增
        let d0 = arr[0]["totalDis"].as_f64().unwrap();
        let d2 = arr[2]["totalDis"].as_f64().unwrap();
        assert!(d2 > d0, "累计里程要递增");

        // speed 是 km/h（步行 5km/h 量级，不是 1.4）
        let sp = arr[2]["speed"].as_f64().unwrap();
        assert!(sp > 1.0 && sp < 30.0, "speed 应在 km/h 量级，实际 {sp}");
    }

    #[test]
    fn point_types_cover_realistic_mix() {
        let n = 200;
        let pts: Vec<TrackPoint> = (0..n)
            .map(|i| tp(31.2304 + i as f64 * 1e-5, 121.4737, 1_790_000_000_000 + i as i64 * 5000))
            .collect();
        let p0 = params();
        let sim = test_sim(&p0, &pts);
        let sub = build(&p0, &pts, &[], &sim).unwrap();
        let arr: Vec<serde_json::Value> =
            serde_json::from_str(sub.save_record["allLocJson"].as_str().unwrap()).unwrap();

        let mut types = std::collections::HashSet::new();
        for p in &arr {
            types.insert(p["type"].as_i64().unwrap());
        }
        // 不该只有一种类型
        assert!(types.len() >= 2, "点位类型要有变化，实际 {types:?}");
        // 不该出现异常点 -1（那是 GPS 失效，故意不生成）
        assert!(!types.contains(&-1), "不生成异常点");
    }
}

#[cfg(test)]
mod single_source_tests {
    use super::tests::test_sim;
    use super::*;
    use crate::track::TrackPoint;

    fn params() -> SubmitParams {
        SubmitParams {
            uid: "1".into(),
            unid: "1".into(),
            uuid: "00000000-0000-4000-8000-000000000000".into(),
            sport_type: 1,
            total_distance_m: 2000,
            total_time_sec: 720,
            total_steps: 1800,
            start_time_ms: 1_790_000_000_000,
            stop_time_ms: 1_790_000_720_000,
            sel_distance_m: 2000,
            sel_run_time_sec: 720,
            policy: "0".into(),
            run_area_id: 0,
            geo_fences_json: "[]".into(),
            total_ascent: 0,
        }
    }

    fn track() -> Vec<TrackPoint> {
        let n = 400;
        (0..n)
            .map(|i| TrackPoint {
                lat: 31.2304 + i as f64 * 5.0 / 111_320.0,
                lon: 121.4737,
                time_ms: 1_790_000_000_000 + i as i64 * 1800,
            })
            .collect()
    }

    /// 报文里的分桶必须和 simulate() 产出的**逐项相等**。
    ///
    /// 这是全项目最关键的一条不变量。之前 build() 里拿 make_jitter(amp=1.0)
    /// 重新生成了一套桶写进报文，而自检校验的是 sim.step_buckets、OBS 明细
    /// 传的也是 sim.step_buckets——自检守 A、提交发 B、明细传 A，
    /// 所有反检测设计都作用在了发不出去的那份数据上。
    #[test]
    fn payload_buckets_equal_sim_buckets() {
        let p = params();
        let t = track();
        let sim = test_sim(&p, &t);

        let sub = build(&p, &t, &[], &sim).expect("build");
        let body = &sub.save_record;

        // 报文里是数组（不是字符串化的 JSON）
        let sent_steps = body["stepsPerTenSec"]
            .as_array()
            .expect("stepsPerTenSec 必须是数组");
        assert_eq!(
            sent_steps.len(),
            sim.step_buckets.len(),
            "桶数不一致：报文 {} vs sim {}",
            sent_steps.len(),
            sim.step_buckets.len()
        );
        for (i, (sent, want)) in sent_steps.iter().zip(sim.step_buckets.iter()).enumerate() {
            let v = sent
                .get("stepsNum")
                .or_else(|| sent.get("steps"))
                .and_then(|x| x.as_i64());
            if let Some(v) = v {
                assert_eq!(
                    v, want.steps_num as i64,
                    "第 {i} 桶步数不一致：报文 {v} vs sim {}",
                    want.steps_num
                );
            }
        }

        // 速度桶同理
        let sent_speed = body["speedPerTenSec"]
            .as_array()
            .expect("speedPerTenSec 必须是数组");
        assert_eq!(sent_speed.len(), sim.speed_buckets.len());
    }

    /// 报文里不许出现 0 步的桶——10 秒 0 步等于静止，服务端判断点。
    #[test]
    fn payload_has_no_empty_step_bucket() {
        let p = params();
        let t = track();
        let sim = test_sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).expect("build");
        let sent = sub.save_record["stepsPerTenSec"].as_array().unwrap();
        for (i, b) in sent.iter().enumerate() {
            let v = b
                .get("stepsNum")
                .or_else(|| b.get("steps"))
                .and_then(|x| x.as_i64())
                .unwrap_or(-1);
            assert!(v > 0, "第 {i} 桶步数是 {v}，会出现静止断点");
        }
    }
}

#[cfg(test)]
mod verify_roundtrip_tests {
    use super::tests::test_sim;
    use super::*;

    fn params() -> SubmitParams {
        SubmitParams {
            uid: "1".into(),
            unid: "1".into(),
            uuid: "00000000-0000-4000-8000-000000000000".into(),
            sport_type: 1,
            total_distance_m: 2000,
            total_time_sec: 720,
            total_steps: 1800,
            start_time_ms: 1_790_000_000_000,
            stop_time_ms: 1_790_000_720_000,
            sel_distance_m: 2000,
            sel_run_time_sec: 720,
            policy: "0".into(),
            run_area_id: 0,
            geo_fences_json: "[]".into(),
            total_ascent: 0,
        }
    }

    fn track() -> Vec<TrackPoint> {
        (0..400)
            .map(|i| TrackPoint {
                lat: 31.2304 + i as f64 * 5.0 / 111_320.0,
                lon: 121.4737,
                time_ms: 1_790_000_000_000 + i as i64 * 1800,
            })
            .collect()
    }

    /// 报文 → 反解 → 视图，桶必须原样回来。
    /// 这条保证 preflight 校验的对象就是发出去的那份。
    #[test]
    fn verify_view_roundtrips_buckets() {
        let p = params();
        let t = track();
        let sim = test_sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let v = verify_view(&sub.save_record).unwrap();

        assert_eq!(v.step_buckets.len(), sim.step_buckets.len());
        for (a, b) in v.step_buckets.iter().zip(sim.step_buckets.iter()) {
            assert_eq!(a.steps_num, b.steps_num, "反解后步数桶变了");
        }
        assert_eq!(v.point_count, t.len());
        assert_eq!(v.total_steps, sim.total_steps);
    }

    /// 反解出来的坐标必须是 BD-09，并且和原轨迹对得上（±1m）。
    #[test]
    fn verify_view_roundtrips_coords() {
        let p = params();
        let t = track();
        let sim = test_sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let v = verify_view(&sub.save_record).unwrap();
        assert_eq!(v.points.len(), t.len());
        for (got, want) in v.points.iter().zip(t.iter()) {
            let d = crate::track::geo::distance((got.lat, got.lon), (want.lat, want.lon));
            assert!(d < 1.0, "第 {} 点偏移 {d:.2}m（BD→GCJ→BD 往返误差过大）", got.lat);
            // 报文 totalTime 的单位是**秒**，毫秒精度在这里必然丢失。
            // 这也意味着采样间隔必须 ≥1 秒，否则反解后会出现时间戳重复的
            // 0 时长段（RunParams::validate 里有对应的下限检查）。
            assert!(
                (got.time_ms - want.time_ms).abs() < 1000,
                "时间戳反解误差超过 1 秒：got {} want {}",
                got.time_ms,
                want.time_ms
            );
        }
    }

    /// 时间轴端点必须精确等于报文的 startTime/stopTime。
    #[test]
    fn verify_view_time_matches_payload() {
        let p = params();
        let t = track();
        let sim = test_sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let v = verify_view(&sub.save_record).unwrap();
        assert_eq!(v.start_time_ms, p.start_time_ms);
        assert!(v.stop_time_ms > v.start_time_ms);
    }
}
