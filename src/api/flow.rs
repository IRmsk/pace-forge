//! 完整提交流程编排。
//!
//! 顺序不能乱，每一步的产物都是下一步的输入：
//!
//! ```text
//! ① policy   → policy 串、minDistance
//! ② fence    → 选中校区、runAreaId、围栏几何
//! ③ points   → 打卡点（BD-09）
//! ④ 规划轨迹 → 必经点全部命中的闭环
//! ⑤ retime   → 由速度反推时间戳
//! ⑥ simulate → 步数、10 秒分桶
//! ⑦ preflight→ 本地自检，不过就不发
//! ⑧ OBS      → 上传轨迹明细，拿 objectKey
//! ⑨ submit   → 提交结算
//! ⑩ 静默窗口 → 提交后一段时间内不能再有跑步请求
//! ```
//!
//! 第 ⑦ 步是安全闸：**宁可本地失败，也不要把注定被驳回的报文发出去**。
//! 驳回会记错误次数，异常多了触发风控。

use serde_json::{json, Value};

use super::client::Client;
use super::fence::{self, FencePick};
use super::obs;
use super::points::{self, PointsContext};
use super::policy::{self, PolicyInfo};
use crate::track::{
    inspect, plan, retime, simulate, Check, Checkpoint, KinParams, PlanRequest, Report, SimParams,
};

/// 一次跑步的参数。
#[derive(Debug, Clone)]
pub struct RunParams {
    /// 中心点纬度（WGS84）。
    pub center_lat: f64,
    /// 中心点经度（WGS84）。
    pub center_lng: f64,
    /// 目标里程（米）。
    pub distance_m: f64,
    /// 目标时长（秒）。
    pub duration_sec: i64,
    /// 运动类型（提交报文里的 sportType）。
    pub sport_type: i32,
    /// runMode：1=校园跑，2=户外跑，3=室外锻炼。
    pub run_mode: i64,
    /// 指定步数，`None` 由模型推算。
    pub steps: Option<i32>,
    /// 随机种子。
    pub seed: u64,
    /// 采样间距（米）。
    pub sample_step_m: f64,
}

impl Default for RunParams {
    fn default() -> Self {
        Self {
            center_lat: 30.5,
            center_lng: 104.0,
            distance_m: 2000.0,
            duration_sec: 720,
            sport_type: 1,
            run_mode: 1,
            steps: None,
            seed: 0,
            sample_step_m: 5.0,
        }
    }
}

/// 每一步的日志。
pub type Log = dyn FnMut(&str);

/// 流程产物。
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub policy: PolicyInfo,
    pub fence: Option<FencePick>,
    pub points: PointsContext,
    pub track_points: usize,
    pub actual_length_m: f64,
    pub total_steps: i32,
    pub avg_cadence: f64,
    pub report: Report,
    pub obs_keys: Vec<String>,
    /// 是否已提交。
    pub submitted: bool,
    pub record_id: Option<i64>,
}

/// 只做到自检为止（不发任何写请求）。
///
/// 用来在提交前确认数据没问题——大部分坑在这一步就能发现。
pub fn dry_run(client: &Client, p: &RunParams, log: &mut Log) -> Result<RunOutcome, String> {
    let start_ms = crate::crypto::envelope::now_ms() as i64;

    // ① policy
    log(&format!("[policy] 拉取策略 runMode={}…", p.run_mode));
    let pol = policy::fetch_policy_for(client, p.run_mode)?;
    log(&format!(
        "√ [policy] ts={} policy={} minDistance={} validTime={}",
        pol.timestamp, pol.policy, pol.min_distance, pol.valid_time
    ));

    // ② fence
    log("[fence] 拉取围栏…");
    let pick = match fence::fetch_fences(client) {
        Ok(fs) if !fs.is_empty() => {
            match fence::select_by_anchor(&fs, (p.center_lat, p.center_lng), -1) {
                Ok(pk) => {
                    if pk.fallback {
                        log(&format!(
                            "⚠ [fence] 锚点不在任何围栏内，按最近「{}」兜底（{:.0}m）",
                            pk.selected.name, pk.distance_m
                        ));
                    } else {
                        log(&format!(
                            "√ [fence] 命中「{}」id={:?}，共 {} 个围栏",
                            pk.selected.name,
                            pk.selected.fence_id,
                            fs.len()
                        ));
                    }
                    Some(pk)
                }
                Err(e) => {
                    log(&format!("⚠ [fence] 选择失败，按自由跑继续: {e}"));
                    None
                }
            }
        }
        Ok(_) => {
            log("· [fence] 服务端未配置围栏，按自由跑继续");
            None
        }
        Err(e) => {
            log(&format!("⚠ [fence] 接口失败，按自由跑继续: {e}"));
            None
        }
    };

    // ③ points
    log("[points] 拉取打卡点…");
    let pt = match points::fetch_points(
        client,
        p.center_lat,
        p.center_lng,
        pick.as_ref().map(|k| k.selected.run_area_id.to_string()).as_deref(),
    ) {
        Ok(c) => {
            log(&format!(
                "√ [points] {} 个点位，分布跨度 {:.0}m",
                c.coords.len(),
                points::span_m(&c.coords)
            ));
            c
        }
        Err(e) => {
            log(&format!("⚠ [points] 接口失败（可能限流），无打卡点继续: {e}"));
            PointsContext::default()
        }
    };

    // ④ 规划
    let cps: Vec<Checkpoint> = pt
        .coords
        .iter()
        .enumerate()
        .map(|(i, &(lat, lng))| Checkpoint {
            lat,
            lng,
            name: format!("P{}", i + 1),
        })
        .collect();
    log(&format!("[track] 规划 {}m，{} 个必经点…", p.distance_m, cps.len()));

    let req = PlanRequest {
        center_lat: p.center_lat,
        center_lng: p.center_lng,
        checkpoints: cps.clone(),
        total_distance_m: p.distance_m,
        total_sec: p.duration_sec,
        start_ms,
        sport_type: p.sport_type,
        sample_step_m: p.sample_step_m,
    };
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::seed_from_u64(if p.seed != 0 { p.seed } else { start_ms as u64 });
    let track = plan(&req, &mut rng)?;
    log(&format!(
        "√ [track] {} 点，实测 {:.1}m（目标 {:.0}m，偏差 {:.2}%）",
        track.points.len(),
        track.actual_length_m,
        p.distance_m,
        (track.actual_length_m - p.distance_m).abs() / p.distance_m * 100.0
    ));

    // ⑤ retime
    let xy: Vec<(f64, f64)> = track.points.iter().map(|q| (q.lat, q.lon)).collect();
    let (times, _speeds, real_len) = retime(
        &xy,
        p.duration_sec,
        start_ms,
        start_ms as u64,
        &KinParams::default(),
        1.0,
        8.0,
    );
    if times.len() != xy.len() {
        return Err("时间戳生成失败".into());
    }
    let timed: Vec<(f64, f64, i64)> = xy
        .iter()
        .zip(&times)
        .map(|((a, b), t)| (*a, *b, *t))
        .collect();
    let track_points: Vec<crate::track::TrackPoint> = xy
        .iter()
        .zip(&times)
        .map(|((a, b), t)| crate::track::TrackPoint {
            lat: *a,
            lon: *b,
            time_ms: *t,
        })
        .collect();
    log(&format!(
        "√ [retime] 时间跨度 {}s → 平均 {:.3} m/s",
        (times[times.len() - 1] - times[0]) / 1000,
        real_len as f64 / p.duration_sec as f64
    ));

    // ⑥ simulate
    let sp = SimParams {
        total_distance_m: real_len as f64,
        total_sec: p.duration_sec,
        start_ms,
        target_steps: p.steps,
        seed: if p.seed != 0 { p.seed } else { start_ms as u64 },
        ..Default::default()
    };
    let sim = simulate(&timed, &sp)?;
    log(&format!(
        "√ [simulate] {} 步，平均步频 {:.1} 步/分，{} 个分桶",
        sim.total_steps,
        sim.avg_cadence,
        sim.step_buckets.len()
    ));

    // ⑦ 自检
    let cp_tuples: Vec<(f64, f64, String)> =
        cps.iter().map(|c| (c.lat, c.lng, c.name.clone())).collect();
    let check = Check {
        points: &track_points,
        sim: &sim,
        params: &sp,
        checkpoints: &cp_tuples,
        total_distance_m: real_len,
        total_time_sec: p.duration_sec,
        total_steps: sim.total_steps,
        start_time_ms: start_ms,
        stop_time_ms: start_ms + p.duration_sec * 1000,
        pace_range: None,
    };
    let report = inspect(&check);
    if report.blocked() {
        for line in report.lines() {
            log(&format!("  {line}"));
        }
        return Err(format!("自检未通过：{}", report.summary()));
    }
    log(&format!("√ [preflight] {}", report.summary()));
    for line in report.lines() {
        log(&format!("  {line}"));
    }

    Ok(RunOutcome {
        policy: pol,
        fence: pick,
        points: pt,
        track_points: track.points.len(),
        actual_length_m: track.actual_length_m,
        total_steps: sim.total_steps,
        avg_cadence: sim.avg_cadence,
        report,
        obs_keys: Vec::new(),
        submitted: false,
        record_id: None,
    })
}

/// 提交后必须保持静默的时长（秒）。
///
/// 服务端会在提交后一段时间内拒绝新的跑步请求。提前再发就会被判异常。
pub const SILENCE_SEC: i64 = 600;

/// 判断距离上次提交是否已过静默窗口。
pub fn silence_ok(last_submit_ms: i64, now_ms: i64) -> bool {
    now_ms - last_submit_ms >= SILENCE_SEC * 1000
}

/// 构造 OBS 明细载荷。
///
/// 服务端详情页会读这个对象来画轨迹，字段名要和 App 一致。
pub fn detail_payload(
    points: &[crate::track::TrackPoint],
    sim: &crate::track::SimResult,
    p: &RunParams,
    uid: &str,
) -> Value {
    let route: Vec<Value> = points
        .iter()
        .map(|q| {
            json!({
                "lat": q.lat,
                "lon": q.lon,
                "time": q.time_ms,
            })
        })
        .collect();

    let speed_per_ten: Vec<Value> = sim
        .speed_buckets
        .iter()
        .map(|b| {
            json!({
                "beginTime": b.begin_time,
                "endTime": b.end_time,
                "distance": b.distance,
            })
        })
        .collect();
    let steps_per_ten: Vec<Value> = sim
        .step_buckets
        .iter()
        .map(|b| {
            json!({
                "beginTime": b.begin_time,
                "endTime": b.end_time,
                "stepsNum": b.steps_num,
                "maxDiff": b.max_diff,
                "minDiff": b.min_diff,
                "avgDiff": b.avg_diff,
            })
        })
        .collect();

    json!({
        "uid": uid,
        "sportType": p.sport_type,
        "totalDistance": sim.actual_length_m,
        "totalTime": p.duration_sec,
        "totalSteps": sim.total_steps,
        "route": route,
        "speedPerTenSec": speed_per_ten,
        "stepsPerTenSec": steps_per_ten,
    })
}

/// 生成两个 objectKey 并上传。
pub fn upload_detail(
    client: &Client,
    payload: &Value,
    p: &RunParams,
    start_ms: i64,
    uid: &str,
    log: &mut Log,
) -> Result<Vec<String>, String> {
    let uuid = super::client::uuid_v4();
    let keys = vec![
        obs::detail_key(start_ms, &uuid),
        obs::fallback_key(uid, p.sport_type, start_ms / 1000, &uuid),
    ];
    let bytes = serde_json::to_vec(payload).map_err(|e| format!("明细序列化失败: {e}"))?;
    log(&format!("[obs] 上传明细 {} 字节…", bytes.len()));
    let (ok, errs) = obs::upload_keys(client, &keys, &bytes);
    for e in &errs {
        log(&format!("  ⚠ {e}"));
    }
    if ok == 0 {
        return Err("OBS 全部 key 上传失败".into());
    }
    log(&format!("√ [obs] {ok}/{} 个 key 上传成功", keys.len()));
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_window() {
        let t0 = 1_730_000_000_000i64;
        assert!(silence_ok(t0, t0 + 600_000));
        assert!(silence_ok(t0, t0 + 700_000));
        assert!(!silence_ok(t0, t0 + 599_999));
        assert!(!silence_ok(t0, t0));
    }

    #[test]
    fn detail_payload_has_required_fields() {
        let pts = vec![
            crate::track::TrackPoint { lat: 30.5, lon: 104.0, time_ms: 1000 },
            crate::track::TrackPoint { lat: 30.5001, lon: 104.0, time_ms: 2000 },
        ];
        let sim = crate::track::SimResult {
            speeds: vec![2.0],
            cadences: vec![150.0],
            strides: vec![0.8],
            total_steps: 100,
            avg_cadence: 150.0,
            speed_buckets: vec![],
            step_buckets: vec![],
            actual_length_m: 11.1,
        };
        let p = RunParams::default();
        let v = detail_payload(&pts, &sim, &p, "12345");
        for k in ["uid", "sportType", "totalDistance", "totalTime", "totalSteps", "route"] {
            assert!(v.get(k).is_some(), "缺字段 {k}");
        }
        assert_eq!(v["route"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn default_params_are_sane() {
        let p = RunParams::default();
        assert!(p.distance_m > 0.0);
        assert!(p.duration_sec > 0);
        // 配速应该在合理区间
        let pace = 1000.0 / (p.distance_m / p.duration_sec as f64) / 60.0;
        assert!((3.0..=12.0).contains(&pace), "默认配速 {pace:.2} 不合理");
    }
}
