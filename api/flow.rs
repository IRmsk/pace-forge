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
    inspect, plan, retime, simulate, Check, Checkpoint, KinParams, PlanRequest, PlanResult,
    Report, SimParams, TrackPoint,
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
    /// 是否优先沿真实道路规划（OSM）。查不到路网时自动降级。
    pub prefer_roads: bool,
    /// 指定开始时刻（毫秒戳）。`None` 表示现在开跑。
    ///
    /// 补跑时由 `start_time` 模块算好传进来——那边已经保证了
    /// 「开始 + 用时 + 抖动余量 ≤ 现在」，所以这里不再二次校验。
    pub start_at_ms: Option<i64>,
}

/// 尝试沿 OSM 真实道路规划。任何一步失败都返回 None（静默降级）。
fn try_route_on_roads(
    fence: &[(f64, f64)],
    must: &[(f64, f64)],
    want_m: f64,
    seed: u64,
    log: &mut dyn FnMut(&str),
) -> Option<crate::track::fence_route::FenceRoute> {
    use crate::track::roadnet;
    let bbox = roadnet::bbox_of(fence, 80.0);
    let net = match roadnet::fetch(bbox) {
        Ok(n) => n,
        Err(e) => {
            log(&format!("[roads] 拉路网失败（{e}），改用几何规划"));
            return None;
        }
    };
    let clipped = net.clip_to_fence(fence);
    let total = clipped.total_length_m();
    if clipped.is_empty() || total < 300.0 {
        log(&format!(
            "[roads] OSM 在该校区只有 {total:.0}m 可用道路，改走几何规划"
        ));
        return None;
    }
    log(&format!("[roads] 路网 {total:.0}m，按真实道路规划"));
    let start = must.first().copied().unwrap_or(fence[0]);
    match roadnet::route_on_roads(&clipped, start, must, want_m, seed) {
        Ok(r) => {
            // 质量门槛：路网只有几百米时，凑里程会「跳」到另一段路上，
            // 造成几十上百米的瞬移（实测 125m）。这种轨迹服务端一眼判异常，
            // 宁可退回几何规划。
            let max_gap = r
                .points
                .windows(2)
                .map(|w| crate::track::geo::distance(w[0], w[1]))
                .fold(0.0f64, f64::max);
            if max_gap > 50.0 {
                log(&format!(
                    "[roads] 路网仅 {total:.0}m，拼接出 {max_gap:.0}m 的跳变，\
                     改用几何规划"
                ));
                return None;
            }
            Some(crate::track::fence_route::FenceRoute {
                points: r.points,
                length_m: r.length_m,
                lines: 1,
            })
        }
        Err(e) => {
            log(&format!("[roads] 沿路规划失败（{e}），改用几何规划"));
            None
        }
    }
}

impl RunParams {
    /// 统一校验。CLI、Web、库调用都必须走这里。
    ///
    /// 之前三个入口各写一套：Web 拦到 300 分钟，CLI 什么都不拦，
    /// `MAX_RUN_TIME`（7200 秒 = 120 分钟）这个常量定义了却从没被引用。
    /// 结果 `run 2 200` 能一路走到提交，必然被服务端驳回，还累计风控次数。
    pub fn validate(&self) -> Result<(), String> {
        if !self.distance_m.is_finite() || self.distance_m <= 0.0 {
            return Err(format!("距离必须是正数，收到 {}", self.distance_m));
        }
        if self.distance_m > 42_000.0 {
            return Err(format!(
                "距离 {} m 超过上限 42000 m（42 公里）",
                self.distance_m
            ));
        }
        if self.duration_sec <= 0 {
            return Err(format!("时长必须为正，收到 {} 秒", self.duration_sec));
        }
        if self.duration_sec > super::submit::MAX_RUN_TIME {
            return Err(format!(
                "时长 {} 秒超过上限 {} 秒（{} 分钟）——超过这个值服务端必然驳回",
                self.duration_sec,
                super::submit::MAX_RUN_TIME,
                super::submit::MAX_RUN_TIME / 60
            ));
        }
        if let Some(st) = self.steps {
            if st < 0 {
                return Err(format!("步数不能为负，收到 {st}"));
            }
        }
        if !(-90.0..=90.0).contains(&self.center_lat) || !(-180.0..=180.0).contains(&self.center_lng) {
            return Err(format!(
                "定位锚点越界: ({}, {})",
                self.center_lat, self.center_lng
            ));
        }
        if self.sample_step_m <= 0.0 || self.sample_step_m > 100.0 {
            return Err(format!("采样间距不合理: {} m", self.sample_step_m));
        }
        // 报文的 totalTime 只到秒，采样间隔必须 ≥ 最快速度下的 1 秒，
        // 否则反解后相邻点时间戳相同，服务端会看到 0 时长段。
        // 8 m/s 上限下，1 秒 = 8 米。
        if self.sample_step_m < 8.0 && self.duration_sec > 0 {
            let avg_v = self.distance_m / self.duration_sec as f64;
            let gap = self.sample_step_m / avg_v.max(0.5);
            if gap < 1.0 {
                return Err(format!(
                    "采样间距 {}m 在 {avg_v:.2} m/s 下只有 {gap:.2} 秒，                     报文时间精度是秒，会画出 0 时长段；把间距调到 {:.0}m 以上",
                    self.sample_step_m,
                    avg_v.max(0.5)
                ));
            }
        }
        Ok(())
    }
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
            prefer_roads: true,
            start_at_ms: None,
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
    /// 轨迹点（含时间戳），提交与明细上传都要用。
    pub track: Vec<crate::track::TrackPoint>,
    /// 合成的步数/分桶数据。
    pub sim: crate::track::SimResult,
    /// 本次跑步的起始时间戳（毫秒）。
    pub start_ms: i64,
    /// 几何实测里程（可能和目标有小偏差）。
    pub real_length_m: i64,
    pub obs_keys: Vec<String>,
    /// 是否已提交。
    pub submitted: bool,
    pub record_id: Option<i64>,
}

/// 只做到自检为止（不发任何写请求）。
///
/// 用来在提交前确认数据没问题——大部分坑在这一步就能发现。
pub fn dry_run(client: &Client, p: &RunParams, log: &mut Log) -> Result<RunOutcome, String> {
    p.validate()?;
    let start_ms = match p.start_at_ms {
        // 补跑：时间是外部算好的，直接用它，但照样加抖动让秒数不整齐
        Some(t) => {
            use rand::Rng;
            let mut r = rand::thread_rng();
            t + r.gen_range(0..4_000i64)
        }
        // 现在开跑：加 0~4 秒偏移，每次都整秒开始太规律
        None => {
            use rand::Rng;
            let mut r = rand::thread_rng();
            crate::crypto::envelope::now_ms() as i64 + r.gen_range(0..5_000i64)
        }
    };

    // ⓿ 学期进度：记分跑要先知道还差多少
    if p.run_mode == 1 {
        match super::semester::fetch_summary(client) {
            Ok(sm) => log(&format!("√ [semester] {}", sm.summary())),
            Err(e) => log(&format!("· [semester] 查询失败（不影响流程）: {e}")),
        }
        match super::semester::fetch_goal(client, p.run_mode) {
            Ok(g) if g.target_dis > 0.0 => {
                let left = g.remaining_dis(0.0) / 1000.0;
                log(&format!(
                    "√ [goal] 学期目标 {:.1}km / {} 次{}",
                    left,
                    g.target_count,
                    if g.days_left(crate::crypto::envelope::now_ms() as i64) >= 0 {
                        format!("，还剩 {} 天", g.days_left(crate::crypto::envelope::now_ms() as i64))
                    } else {
                        String::new()
                    }
                ));
            }
            Ok(_) => {}
            Err(e) => log(&format!("· [goal] 查询失败（不影响流程）: {e}")),
        }
    }

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
    let pt = match points::fetch_points_cached(
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
            // 「按顺序通过所有点位」判定失败时，得知道服务端给的正式顺序
            if let Some(first) = c.points.first() {
                let keys: Vec<&String> = first
                    .as_object()
                    .map(|o| o.keys().collect())
                    .unwrap_or_default();
                log(&format!("[points] 原始字段: {keys:?}"));
                log(&format!("[points] 首点: {first}"));
            }
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
    // 有围栏就在围栏内铺路：早先不管围栏形状，按中心点画固定半径的圆，
    // 围栏一大轨迹就有一半在外面，服务端判越界，整条记录作废（距离 0.00km）。
    let fence_ring: Vec<(f64, f64)> = pick
        .as_ref()
        .map(|k| {
            // 多个多边形取最大的那块，避免拼成跨越围栏的直线
            k.selected
                .polygons
                .iter()
                .max_by(|a, b| a.len().cmp(&b.len()))
                .cloned()
                .unwrap_or_default()
        })
        .unwrap_or_default();

    let track = if fence_ring.len() >= 3 {
        log(&format!(
            "[track] 围栏内规划 {}m，{} 个必经点…",
            p.distance_m,
            cps.len()
        ));
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(if p.seed != 0 { p.seed } else { start_ms as u64 });
        let _ = &mut rng;

        let must: Vec<(f64, f64)> = cps.iter().map(|c| (c.lat, c.lng)).collect();
        // 路线优先级：
        //   ① 沿围栏边界内缩跑——围墙边界一定是能走的路，绝不会落在湖面
        //      或楼里；形状也像人绕着校园跑。
        //   ② 几何蛇形填充——边界周长不够或内缩失败时的兜底。
        //
        // 之前只有 ②，纯几何不看地形，轨迹直接从水面划过去。
        // 服务端判「按顺序通过所有点位」用的是点位数组顺序，那个顺序不是
        // 地理上的环形顺序（实测是锯齿）。所以骨架必须按数组顺序穿点，
        // 绕圈再漂亮也没用——顺序错一条记录就废。
        let seed = if p.seed != 0 { p.seed } else { start_ms as u64 };

        // 优先走真实道路。OSM 在部分校区完全没有数据（实测某校整个 bbox
        // 只有 1 个 landuse 标签），所以这是「能走就走」，失败静默降级到
        // 几何规划——不能因为查不到路网就不跑了。
        let on_roads = if p.prefer_roads {
            try_route_on_roads(&fence_ring, &must, p.distance_m, seed, &mut *log)
        } else {
            None
        };
        let route = if let Some(r) = on_roads {
            r
        } else if must.len() >= 2 {
            match crate::track::fence_route::route_through_in_order(
                &fence_ring,
                p.distance_m,
                &must,
                seed,
            ) {
                Ok(r) => {
                    log(&format!(
                        "[track] 按打卡点顺序串 {} 段，{:.0}m",
                        r.lines, r.length_m
                    ));
                    r
                }
                Err(e) => {
                    log(&format!("[track] 按序串点失败（{e}），退回沿边界绕圈"));
                    crate::track::fence_route::route_along_fence(
                        &fence_ring,
                        p.distance_m,
                        25.0,
                        &must,
                    )?
                }
            }
        } else {
            log("[track] 没有打卡点，沿围栏内缩 25m 跑");
            crate::track::fence_route::route_along_fence(&fence_ring, p.distance_m, 25.0, &must)?
        };
        // 沿边界是贴着围墙的折线，拐角磨圆 + 轻微摆动，别像尺子画的
        let rounded = crate::track::fence_route::round_corners(&route.points, 20.0, 6);
        let wandered = crate::track::fence_route::add_wander(
            &rounded,
            6.0,
            120.0,
            if p.seed != 0 { p.seed } else { start_ms as u64 },
        );
        let sampled = crate::track::fence_route::resample_with_checkpoints(
            &wandered,
            p.sample_step_m,
            &must,
            15.0,
        );
        // 时间戳先占位：⑤ 的 retime 会按速度剖面重算并整体覆盖，
        // 这里算一遍是白算（原来 assign_times 的结果直接被丢弃，
        // 还让人误以为时间来自两处）。占位填 start_ms 即可。
        let pts: Vec<TrackPoint> = sampled
            .iter()
            .map(|&(lat, lng)| TrackPoint {
                lat,
                lon: lng,
                time_ms: start_ms,
            })
            .collect();
        let actual = crate::track::geo::path_length(&sampled);
        PlanResult {
            points: pts,
            checkpoints: cps.clone(),
            actual_length_m: actual,
        }
    } else {
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
        let _ = &mut rng;
        plan(&req, &mut rng)?
    };
    log(&format!(
        "√ [track] {} 点，实测 {:.1}m（目标 {:.0}m，偏差 {:.2}%）",
        track.points.len(),
        track.actual_length_m,
        p.distance_m,
        (track.actual_length_m - p.distance_m).abs() / p.distance_m * 100.0
    ));

    // ⑤ retime
    //
    // 速度上下界由**服务端策略**的配速区间反推，不再硬编码 1.0~8.0。
    // 8.0 m/s = 2:05 配速，学生账号规则通常只给到 3~10 分/公里，
    // 硬编码会生成超出区间的配速，自检却因为 pace_range=None 报通过。
    let (v_floor, v_ceil) = match pol.pace_range {
        Some((a, b)) => {
            // 分/公里 → m/s；配速越小越快
            let fast = a.min(b).max(0.1);
            let slow = a.max(b).max(fast + 0.01);
            let hi = 1000.0 / (fast * 60.0);
            let lo = 1000.0 / (slow * 60.0);
            log(&format!(
                "[policy] 配速区间 {fast:.1}~{slow:.1} 分/km → 速度 {lo:.2}~{hi:.2} m/s"
            ));
            (lo.clamp(0.5, 10.0), hi.clamp(0.5, 10.0))
        }
        None => (1.0, 8.0),
    };
    let xy: Vec<(f64, f64)> = track.points.iter().map(|q| (q.lat, q.lon)).collect();
    let (times, _speeds, real_len) = retime(
        &xy,
        p.duration_sec,
        start_ms,
        start_ms as u64,
        &KinParams::default(),
        v_floor,
        v_ceil,
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
    //
    // 校验对象必须是**报文里那份数据**，不是内存里的 sim/params。
    // 之前 preflight 吃的是 sim，而报文是 build() 另外拼的——两边一旦
    // 分叉（历史 bug：桶被重新生成过），自检就在守一份发不出去的数据。
    let cp_tuples: Vec<(f64, f64, String)> =
        cps.iter().map(|c| (c.lat, c.lng, c.name.clone())).collect();
    // 先按真实参数把报文组装出来（纯本地计算，不联网），再把它反解成
    // 校验视图。这一步是「自检守 A、提交发 B」的根治：检查对象从内存结构
    // 换成报文 JSON 反解的结果，两者物理上是同一份数据。
    // uid/unid 用占位符——校验不看身份字段。
    let probe = super::submit::SubmitParams {
        uid: "probe".into(),
        unid: "0".into(),
        uuid: "00000000-0000-4000-8000-000000000000".into(),
        sport_type: p.sport_type,
        total_distance_m: real_len,
        total_time_sec: p.duration_sec,
        total_steps: sim.total_steps,
        start_time_ms: start_ms,
        stop_time_ms: start_ms + p.duration_sec * 1000,
        sel_distance_m: pol.min_distance,
        sel_run_time_sec: p.duration_sec,
        policy: pol.policy.to_string(),
        run_area_id: -1,
        geo_fences_json: "[]".into(),
        total_ascent: 0,
    };
    let probe_sub = super::submit::build(&probe, &track_points, &[], &sim)?;
    let view = super::submit::verify_view(&probe_sub.save_record)?;
    log(&format!(
        "√ [verify] 从报文反解：{} 点 / {:.0}m / {}s / {} 步 / {} 个步数桶",
        view.point_count,
        view.total_distance_m,
        view.total_time_sec,
        view.total_steps,
        view.step_buckets.len()
    ));
    let view_sim = view.as_sim();

    let check = Check {
        points: &view.points,
        sim: &view_sim,
        params: &sp,
        checkpoints: &cp_tuples,
        total_distance_m: view.total_distance_m as i64,
        total_time_sec: view.total_time_sec,
        total_steps: view.total_steps,
        start_time_ms: view.start_time_ms,
        stop_time_ms: view.stop_time_ms,
        pace_range: pol.pace_range,
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
        track: track_points,
        sim,
        start_ms,
        real_length_m: real_len,
        obs_keys: Vec::new(),
        submitted: false,
        record_id: None,
    })
}

/// 完整流程：自检通过后上传明细并提交成绩。
///
/// `confirm` 返回 false 时在提交前中止，已上传的 OBS 对象保留（不影响）。
pub fn run_full(
    client: &Client,
    p: &RunParams,
    log: &mut Log,
    confirm: &mut dyn FnMut(&RunOutcome) -> bool,
) -> Result<RunOutcome, String> {
    // 前七步与 dry_run 完全一致
    let mut out = dry_run(client, p, log)?;

    if out.report.blocked() {
        return Err(format!("自检未通过，不提交：{}", out.report.summary()));
    }
    if !confirm(&out) {
        log("[flow] 已取消，未提交");
        return Ok(out);
    }

    // ❼.5 反检测体检：按 7.4.01 的本地规则先过一遍
    {
        // 必须是轨迹点（400 个），不是打卡点（5 个）
        let xy: Vec<(f64, f64)> = out.track.iter().map(|t| (t.lat, t.lon)).collect();
        let rep = crate::track::detect::inspect(
            &xy,
            &out.sim.speed_buckets,
            crate::track::detect::PointRule::default(),
        );
        log(&format!("[detect] {}", rep.summary()));
        for line in rep.lines().iter().take(5) {
            log(&format!("  · {line}"));
        }
        if rep.blocked() {
            return Err("点位体检未通过，提交会被判异常，已中止".into());
        }
    }

    // ⑧ 组装并提交（必须先提交，OBS 的 objectKey 和桶 id 都要 rrid）
    let start_ms = out.start_ms;
    let uid = client
        .credentials()
        .and_then(|c| c.uid.as_ref())
        .cloned()
        .unwrap_or_default();
    let unid = client
        .credentials()
        .and_then(|c| c.unid.as_ref())
        .cloned()
        .unwrap_or_default();
    let uuid = super::client::uuid_v4().to_uppercase();
    let stop_ms = start_ms + p.duration_sec * 1000;

    let fixed: Vec<super::submit::FixedPoint> = out
        .points
        .coords
        .iter()
        .enumerate()
        .map(|(i, &(lat, lon))| super::submit::FixedPoint {
            lat,
            lon,
            position: i as i32 + 1,
        })
        .collect();

    let geo_fences = out
        .fence
        .as_ref()
        .map(|f| super::fence::canonical_json_for(&f.selected.polygons[0]))
        .unwrap_or_else(|| "[]".into());

    let params = super::submit::SubmitParams {
        uid: uid.clone(),
        unid: unid.clone(),
        uuid: uuid.clone(),
        sport_type: p.sport_type,
        total_distance_m: out.real_length_m,
        total_time_sec: p.duration_sec,
        total_steps: out.total_steps,
        start_time_ms: start_ms,
        stop_time_ms: stop_ms,
        // selDistance 用规则给的最小里程，不是用户设的目标
        sel_distance_m: out.policy.min_distance,
        sel_run_time_sec: p.duration_sec,
        policy: out.policy.policy.to_string(),
        run_area_id: out
            .fence
            .as_ref()
            .map(|f| f.selected.run_area_id)
            .unwrap_or(-1),
        geo_fences_json: geo_fences,
        total_ascent: 0,
    };

    // 报文的桶必须来自 sim，不能在这里重新生成——否则自检、提交、明细
    // 三处数据不一致（见 submit::build 的注释）。
    let submission = super::submit::build(&params, &out.track, &fixed, &out.sim)?;

    // 7.4.01 已移除 BODY_SENSORS，报文里不许出现心率字段
    let mut body = submission.save_record.clone();
    let dropped = crate::track::detect::strip_forbidden(&mut body);
    if !dropped.is_empty() {
        log(&format!("[detect] 已剔除不该有的字段: {}", dropped.join(", ")));
    }
    log(&format!(
        "√ [submit] 报文已组装，{} 个轨迹点",
        out.track.len()
    ));

    log("[submit] 提交成绩…");
    let data = super::submit::save_record(client, &body)?;
    out.submitted = true;
    out.record_id = super::submit::extract_record_id(&data);
    log(&format!("√ [submit] 提交成功，记录 id={:?}", out.record_id));

    // ⑨ OBS：用 rrid 重算桶 id 和 objectKey
    let rrid = out.record_id.unwrap_or(0);
    log(&format!("[obs] 用 rrid={rrid} 上传明细…"));
    let five_pt = body
        .get("fivePointJson")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let payload = detail_payload_rrid(&out.track, &out.sim, p, &uid, rrid, start_ms, &uuid, &five_pt);
    let keys = vec![
        super::obs::detail_key(start_ms, &uuid),
        format!("run_data/{}/{rrid}.json", rrid / 1_000_000),
    ];
    let bytes = serde_json::to_vec(&payload).map_err(|e| format!("明细序列化失败: {e}"))?;
    let (ok, errs) = obs::upload_keys(client, &keys, &bytes);
    for e in &errs {
        log(&format!("  ⚠ {e}"));
    }
    if ok == 0 {
        log("⚠ [obs] 明细上传全部失败，记录已提交但详情页可能无轨迹");
    } else {
        log(&format!("√ [obs] {ok}/{} 个 key 上传成功", keys.len()));
    }
    out.obs_keys = keys;

    log(&format!(
        "  静默窗口 {} 秒内不要再发跑步请求",
        SILENCE_SEC
    ));

    Ok(out)
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

/// 带 rrid 的明细载荷（OBS 用，桶 id 走 `(rrid%100000)*1000+endSec`）。
pub fn detail_payload_rrid(
    points: &[crate::track::TrackPoint],
    sim: &crate::track::SimResult,
    _p: &RunParams,
    uid: &str,
    rrid: i64,
    start_ms: i64,
    uuid: &str,
    five_point_json: &str,
) -> Value {
    let mut speed = sim.speed_buckets.clone();
    let mut steps = sim.step_buckets.clone();
    crate::track::buckets::apply_obs_ids(&mut speed, &mut steps, rrid, start_ms);

    // OBS 对象是 10 个 key，每个值都 gzip + base64。
    // 之前传的是明文 JSON，结构也不对，服务端读不出轨迹，详情页就显示
    // 「这条记录没有轨迹坐标」。
    // OBS 点数集和提交用同一套 28 字段，区别只在 id / queueNum
    let pts: Vec<Value> = {
        let n = points.len();
        let mut prev = points.first().map(|q| (q.lat, q.lon)).unwrap_or((0.0, 0.0));
        let start_pt = prev;
        let mut cum = (0.0f64, 0i64, 0i64);
        let mut prev_time = points.first().map(|q| q.time_ms).unwrap_or(start_ms);
        points
            .iter()
            .enumerate()
            .map(|(i, q)| {
                super::submit::point_json_full(
                    q, i, n, start_ms, start_pt, &mut prev, &mut cum, &mut prev_time,
                    sim.total_steps as i64, start_ms as u64,
                )
            })
            .collect()
    };
    let run_wrap = json!({
        "allLocJson": Value::Array(pts).to_string(),
        "useZip": false,
    });

    json!({
        "rrid": gz_str(&rrid.to_string()),
        "uuid": gz_str(uuid),
        "uid": gz_str(uid),
        "run_data": gz_json(&run_wrap),
        "fixed_point_json": gz_str(five_point_json),
        "segment_json": gz_str(""),
        "speed_json": gz_json(&Value::Array(
            speed.iter().map(|b| serde_json::to_value(b).unwrap_or(Value::Null)).collect::<Vec<_>>()
        )),
        "step_freq_json": gz_json(&Value::Array(
            steps.iter().map(|b| serde_json::to_value(b).unwrap_or(Value::Null)).collect::<Vec<_>>()
        )),
        "laps_json": gz_json(&Value::Array(Vec::<Value>::new())),
        "runFaceCheck": gz_str(""),
    })
}

/// gzip + base64。服务端解这层压缩才读得到内容。
fn gz(bytes: &[u8]) -> String {
    use base64::Engine;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    let _ = e.write_all(bytes);
    let out = e.finish().unwrap_or_default();
    base64::engine::general_purpose::STANDARD.encode(out)
}

fn gz_str(v: &str) -> String {
    gz(v.as_bytes())
}

fn gz_json(v: &Value) -> String {
    gz(v.to_string().as_bytes())
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
mod obs_tests {
    use super::*;
    use base64::Engine;

    /// OBS 对象必须是 10 个键，且每个值都是 gzip+base64。
    ///
    /// 之前传的是明文 JSON，键名也不同，服务端读不出轨迹，详情页显示
    /// 「这条记录没有轨迹坐标」。
    #[test]
    fn obs_payload_is_ten_gzipped_keys() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let p = RunParams { distance_m: 1000.0, duration_sec: 300, ..Default::default() };
        let pts: Vec<crate::track::TrackPoint> = (0..50)
            .map(|i| crate::track::TrackPoint {
                lat: 31.23 + i as f64 * 5.0 / 111320.0,
                lon: 121.47,
                time_ms: 1_793_966_000_000 + i as i64 * 1500,
            })
            .collect();
        let sim = crate::track::SimResult {
            speeds: vec![3.0; 50],
            cadences: vec![150.0; 50],
            strides: vec![1.2; 50],
            total_steps: 300,
            avg_cadence: 150.0,
            speed_buckets: vec![],
            step_buckets: vec![],
            actual_length_m: 1000.0,
        };
        let v = detail_payload_rrid(&pts, &sim, &p, "123", 99, 1_793_966_000_000, "U", "[]");

        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 10, "必须是 10 个键，实际 {keys:?}");
        for k in ["rrid", "uuid", "uid", "run_data", "fixed_point_json",
                  "segment_json", "speed_json", "step_freq_json", "laps_json", "runFaceCheck"] {
            assert!(v.get(k).is_some(), "缺 key: {k}");
        }

        // 每个值都要能 base64 解码 + gzip 解压
        for k in keys {
            let raw = v[k].as_str().unwrap_or_else(|| panic!("{k} 不是字符串"));
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(raw)
                .unwrap_or_else(|e| panic!("{k} base64 解码失败: {e}"));
            assert_eq!(&bytes[..2], &[31, 139], "{k} 不是 gzip 头");
            let mut out = String::new();
            GzDecoder::new(&bytes[..])
                .read_to_string(&mut out)
                .unwrap_or_else(|e| panic!("{k} gzip 解压失败: {e}"));
        }
    }
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
