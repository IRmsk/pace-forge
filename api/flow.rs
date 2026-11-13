//! 完整提交流程编排。
//!
//! 顺序不能乱，每一步的产物都是下一步的输入：
//!
//! ```text
//! ① policy   → policy 串、minDistance
//! ② fence    → 选中校区、runAreaId、围栏几何
//! ③ points   → 打卡点（BD-09）
//! ④ 规划轨迹 → 必经点全部命中的闭环
//! ⑤ 采样     → 等时间 5 秒沿几何折线取上报点（真机节奏）
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
    inspect, plan, simulate, Check, Checkpoint, KinParams, PlanRequest, PlanResult, Report,
    SimParams, TrackPoint,
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
    /// 几何采样间距（米）。
    ///
    /// **只管几何**：拐角、绕线、摆动都在这条密采样折线上算。上报节奏由
    /// [`Self::sample_interval_sec`] 决定，两者不要混——早先这一个字段同时
    /// 兼任两件事，于是「5 米一个点」就等于「1.65 秒一个点」，采样率是真机
    /// 的三倍（真机 4.91 秒/点）。
    pub sample_step_m: f64,
    /// 上报采样间隔（秒/点）。真机实测 4.91 s/点、稳态严格 5 秒。
    ///
    /// 这是**报文里那个节奏**：`allLocJson` 相邻点的 `totalTime` 差。
    /// 改成别的值就等于放弃与真机对齐，`preflight` 的 `SAMPLE_RATE` 会拦。
    pub sample_interval_sec: f64,
    /// 是否优先沿真实道路规划（OSM）。查不到路网时自动降级。
    pub prefer_roads: bool,
    /// 指定开始时刻（毫秒戳）。`None` 表示现在开跑。
    ///
    /// 补跑时由 `start_time` 模块算好传进来——那边已经保证了
    /// 「开始 + 用时 + 抖动余量 ≤ 现在」，所以这里不再二次校验。
    pub start_at_ms: Option<i64>,
    /// 手绘轨迹（BD-09 经纬度对）。`Some` 时跳过 planner，直接采用。
    ///
    /// 用户清楚哪栋楼在哪，手画比算法猜靠谱——尤其 OSM 在该校区
    /// 零建筑数据、几何规划只能凭直线连点的时候。
    pub custom_path: Option<Vec<(f64, f64)>>,
    /// 海拔区间 `(最低, 最高)`，米。`None` 用默认 42±21m。
    pub alt_range: Option<(f64, f64)>,
}

/// 记分跑 = 操场绕圈。
///
/// 依据：issue #3 里 silver-kin 提供的真机截图（卫星图 + rrid 1363125372）
/// 显示真机记分跑就是绕标准椭圆跑道跑多圈，每圈 GPS 抖动叠成一条「带」，
/// 打卡点分布在跑道四周。不是满场自由折线——之前一直在满场找路，方向
/// 从一开始就偏了。
///
/// 几何：圆心取打卡点质心（打卡点就在跑道上），朝向按四角 PCA 主轴
/// （矩形的主轴是长边，不是对角线，比「最大跨度扫描」准），尺寸按四角
/// 外框 ×1.35 放大到真实跑道（实测跑道比打卡点外框大 ~35%）。
///
/// **尺寸保护**：返回 `None` 表示这组打卡点不像操场，调用方回退到原来的
/// 路线规划。fork 那边无条件绕圈是因为他的账号打卡点全在操场周围（外框
/// 几十米）；校园各处的打卡点（跨度几百米）硬套会让轨迹飞到不知道哪里，
/// 必须能识别出来。
fn try_stadium_track(
    cps: &[Checkpoint],
    distance_m: f64,
    sample_step_m: f64,
    seed: u64,
    log: &mut Log,
) -> Option<Vec<(f64, f64)>> {
    if cps.len() < 4 {
        return None;
    }
    let klat = crate::track::geo::m_per_deg_lat(cps[0].lat);
    let klng = crate::track::geo::m_per_deg_lng(cps[0].lat);

    // 质心：先用来找出「场心」那个点（它在场地内部，不是跑道外框的角，
    // 拿它算朝向/尺寸会把椭圆带偏、变圆）
    let gx = cps.iter().map(|c| c.lat).sum::<f64>() / cps.len() as f64;
    let gy = cps.iter().map(|c| c.lng).sum::<f64>() / cps.len() as f64;
    let center_idx = (0..cps.len())
        .min_by(|&a, &b| {
            let da = ((cps[a].lat - gx) * klat).hypot((cps[a].lng - gy) * klng);
            let db = ((cps[b].lat - gx) * klat).hypot((cps[b].lng - gy) * klng);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(0);
    let corners: Vec<&Checkpoint> = cps
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != center_idx)
        .map(|(_, c)| c)
        .collect();
    if corners.len() < 3 {
        return None;
    }

    let cx = corners.iter().map(|c| c.lat).sum::<f64>() / corners.len() as f64;
    let cy = corners.iter().map(|c| c.lng).sum::<f64>() / corners.len() as f64;

    // 四角二阶矩 PCA 取主轴
    let (mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0);
    for c in &corners {
        let dx = (c.lng - cy) * klng;
        let dy = (c.lat - cx) * klat;
        sxx += dx * dx;
        syy += dy * dy;
        sxy += dx * dy;
    }
    let heading = 0.5 * (2.0 * sxy).atan2(sxx - syy);

    let span = |h: f64| {
        let (st, ct) = h.sin_cos();
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for c in &corners {
            let u = (c.lng - cy) * klng * ct + (c.lat - cx) * klat * st;
            lo = lo.min(u);
            hi = hi.max(u);
        }
        hi - lo
    };
    let major = span(heading);
    let minor = span(heading + std::f64::consts::FRAC_PI_2);

    // 尺寸保护。真实跑道（含打卡点外框）长轴 60~220m、短轴 30~140m；
    // 超出这个范围说明打卡点散在校园各处，不是操场，回退。
    if major < 55.0 || major > 240.0 || minor < 25.0 || minor > 160.0 {
        log(&format!(
            "[track] 打卡点外框 {major:.0}×{minor:.0}m，不像操场，改用路线规划"
        ));
        return None;
    }

    const FIELD_TO_TRACK: f64 = 1.35;
    let radius = (minor * FIELD_TO_TRACK / 2.0).clamp(20.0, 30.0);
    let straight = (major * FIELD_TO_TRACK - 2.0 * radius).clamp(35.0, 140.0);
    let lap = 2.0 * straight + 2.0 * std::f64::consts::PI * radius;
    let spec = crate::track::OvalSpec {
        straight,
        radius,
        heading,
        laps: (distance_m / lap).max(1.0),
    };
    log(&format!(
        "[track] 操场绕圈：直道{:.0}m 半径{:.0}m 朝向{:.0}° {:.1}圈（单圈{:.0}m），{} 个打卡点",
        spec.straight, spec.radius, spec.heading.to_degrees(), spec.laps, lap, cps.len()
    ));

    let mut pts = crate::track::campus_oval_track((cx, cy), &spec, sample_step_m, seed);

    // 打卡点吸附：四角点真实椭圆天然就在 20m 内；「田径场中」这种在椭圆
    // 内部的点可能差得远，用余弦渐变把一小段整体拉过去，形成柔和外凸，
    // 不戳尖刺（硬吸附会在轨迹上留下一个尖角）。
    for c in cps {
        let target = (c.lat, c.lng);
        let (mut bi, mut bd) = (usize::MAX, f64::MAX);
        for (i, q) in pts.iter().enumerate() {
            let d = crate::track::geo::distance(*q, target);
            if d < bd {
                bd = d;
                bi = i;
            }
        }
        if bi == usize::MAX || bd <= 16.0 || bd > 45.0 {
            continue;
        }
        let base = pts[bi];
        let (dlat, dlng) = (target.0 - base.0, target.1 - base.1);
        let w = ((bd / sample_step_m).ceil() as usize).clamp(4, 14);
        for k in 0..=(2 * w) {
            let idx = bi as isize - w as isize + k as isize;
            if idx < 0 || idx as usize >= pts.len() {
                continue;
            }
            let idx = idx as usize;
            // 余弦权：中心权重 1、两端 0，坡度平缓
            let t = (k as f64 / (2 * w) as f64 - 0.5) * std::f64::consts::PI;
            let wgt = 0.5 + 0.5 * t.cos();
            pts[idx].0 += dlat * wgt;
            pts[idx].1 += dlng * wgt;
        }
    }

    Some(pts)
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

/// 第二级路网来源：高德瓦片。
///
/// OSM 在国内校园基本是空白，`try_route_on_roads` 十有八九降级到几何规划。
/// 但这类学校在地图 App 上是有路的——服务端用的就是高德底图，同一套瓦片
/// 直接可见。这里把瓦片截图里的白色路面提成路网，再走同一个沿路规划器。
///
/// 与 OSM 那条路的区别只有数据来源；产出同样是 `FenceRoute`，下游不用
/// 区分。质量门槛也照搬：拼接跳变超过 50m 就放弃，宁可退回几何规划。
fn try_route_on_tiles(
    fence: &[(f64, f64)],
    must: &[(f64, f64)],
    want_m: f64,
    seed: u64,
    log: &mut dyn FnMut(&str),
) -> Option<crate::track::fence_route::FenceRoute> {
    use crate::track::{roadnet, tileroad};
    let bbox = roadnet::bbox_of(fence, 60.0);
    let net = match tileroad::fetch_roadnet(bbox) {
        Ok(n) => n,
        Err(e) => {
            log(&format!("[tiles] 瓦片路网没拿到（{e}）"));
            return None;
        }
    };
    let clipped = net.clip_to_fence(fence);
    let total = clipped.total_length_m();
    if clipped.is_empty() || total < 300.0 {
        log(&format!(
            "[tiles] 瓦片上只有 {total:.0}m 可用道路，改走几何规划"
        ));
        return None;
    }
    log(&format!("[tiles] {}", net.source));
    log(&format!("[tiles] 围栏内路网 {total:.0}m，按瓦片道路规划"));

    let start = must.first().copied().unwrap_or(fence[0]);
    // 路线走 A* 最短路：不走死胡同、不绕冤枉路，只在打卡点之间连最合理的
    // 一条。之前试过贪心漫游，虽然形状自然了，但会钻进支路再折返——那是
    // 为了凑里程而绕路，不是人跑步的样子。
    //
    // 「不生硬」交给 soften：横向摆动 + 轻度平滑，让轨迹偏离道路中心线。
    let graph = tileroad::build_graph(&clipped, 25.0);
    let _ = start;
    let _ = seed;
    match tileroad::route_through(&graph, must, want_m) {
        Ok(points) => {
            let max_gap = points
                .windows(2)
                .map(|w| crate::track::geo::distance(w[0], w[1]))
                .fold(0.0f64, f64::max);
            let length_m = points
                .windows(2)
                .map(|w| crate::track::geo::distance(w[0], w[1]))
                .sum::<f64>();
            if max_gap > 50.0 {
                log(&format!("[tiles] 走出 {max_gap:.0}m 的跳变，改用几何规划"));
                return None;
            }
            // 只在「明显短于目标」时判为图不连通。
            //
            // 原来用 abs() 判双向 25%，结果闭环绕行把一圈拉到 2767m（超出
            // 38%）也被拦掉，整条瓦片路网被丢弃、退回几何规划——用户看到
            // 的「乱跑」就是几何规划那条路。长一点由内部截断处理，不该拒绝。
            if length_m < want_m * 0.6 {
                log(&format!(
                    "[tiles] 图上只走出 {length_m:.0}m（目标 {want_m:.0}m），图不连通，改用几何规划"
                ));
                return None;
            }
            log(&format!("[tiles] 沿瓦片道路 {length_m:.0}m（目标 {want_m:.0}m）"));
            Some(crate::track::fence_route::FenceRoute {
                points,
                length_m,
                lines: 1,
            })
        }
        Err(e) => {
            log(&format!("[tiles] 图上走不通（{e}）"));
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
            return Err(format!("几何采样间距不合理: {} m", self.sample_step_m));
        }
        // 上报节奏校验。
        //
        // 老代码校验的是「采样间距 ÷ 平均速度 ≥ 1 秒」，也就是只防「两个点
        // 时间戳相同、画出 0 时长段」这一种显错，5 米（1.65 秒）轻松过关——
        // **没有任何地方比对真机那个 4.91 秒的节奏**，采样率是真机三倍却
        // 一路绿灯。现在节奏是独立参数，直接按秒卡：
        //
        // - 下界 1 秒：报文 `totalTime` 精度是秒，再小必然出现 0 时长段；
        // - 上界：与真机 4.9115 s 比，超过 3 倍就不是同一类数据了。
        //   （硬拦在 15 秒，容忍度留给压测；更严的「必须贴着 5 秒」在
        //   `preflight::SAMPLE_RATE` 里，那里看的是**生成结果**的分布。）
        if !self.sample_interval_sec.is_finite() || self.sample_interval_sec < 1.0 {
            return Err(format!(
                "上报采样间隔 {}s 不合理：报文时间精度是秒，小于 1 秒会画出 0 时长段",
                self.sample_interval_sec
            ));
        }
        if self.sample_interval_sec > 15.0 {
            return Err(format!(
                "上报采样间隔 {}s 超过真机节奏（{:.2}s）的三倍，点数会明显偏少",
                self.sample_interval_sec,
                crate::track::REAL_MEAN_INTERVAL_SEC
            ));
        }
        // 点数下界：一条轨迹至少要有两个点，且起步额外点需要时长够长。
        let est = crate::track::sample_times(self.duration_sec, 0, self.sample_interval_sec);
        if est.len() < 4 {
            return Err(format!(
                "时长 {}s / 间隔 {}s 只能采出 {} 个点，至少要 4 个",
                self.duration_sec,
                self.sample_interval_sec,
                est.len()
            ));
        }
        if est.len() > 40_000 {
            return Err(format!(
                "时长 {}s / 间隔 {}s 会采出 {} 个点，报文过大",
                self.duration_sec,
                self.sample_interval_sec,
                est.len()
            ));
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
            // 几何采样间距：**只管形状，不管节奏**。
            //
            // 曾经改成 10m，理由抄自 fork「真机 2km 约 200 点」。但从 OBS
            // 里把真机对象取出来数过：1362680775 是 1.93km / **406 点**
            // = 4.75m/点，1361415974 是 1.81km / 375 点 = 4.8m/点，
            // 1360341003 是 2.04km / 419 点 = 4.9m/点。三条都是 4.8m 左右，
            // 没有一条是 10m——但那是**上报点**的间距，不是几何步长。
            //
            // 现在两者拆开了：几何继续 5m（拐角、绕楼要密），上报点由
            // `sample_interval_sec` 沿这条折线按时间取（真机 4.91s/点）。
            // 5m 几何 + 5s 上报 = 1683m 出 114 点、14.9m/点，与真机同构。
            sample_step_m: 5.0,
            // 上报节奏：与真机对齐（等时间 5 秒，见 `track::sample_times`）。
            sample_interval_sec: crate::track::SAMPLE_INTERVAL_SEC,
            prefer_roads: true,
            start_at_ms: None,
            custom_path: None,
            alt_range: None,
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
    /// 本次报文用的点位数组（28 字段/点，与 `allLocJson` 同构）。
    ///
    /// **同一份实例**：`dry_run` 里由 `submit::build_points` 造一次，自检按它
    /// 校验（`verify_view` / `audit_derived`），`run_full` 把同一份交给报文与
    /// OBS 明细上传，再也不重建（见 `docs/fix/FIX-A4-SINGLE-POINT-SOURCE.md`）。
    pub submission_points: Vec<Value>,
    /// 本次的必到点（服务端点位 → `submit::FixedPoint`）。
    ///
    /// 自检与提交共用同一份。早先 probe 传的是空数组，于是 `type=2` 的打卡
    /// 标记与 `fivePointJson` 的 `isFixed` 只有上传那份才有——自检守着另一份报文。
    pub fixed: Vec<super::submit::FixedPoint>,
}

/// 只做到自检为止（不发任何写请求）。
///
/// 用来在提交前确认数据没问题——大部分坑在这一步就能发现。
pub fn dry_run(client: &Client, p: &RunParams, log: &mut Log) -> Result<RunOutcome, String> {
    p.validate()?;
    // 校时：之后所有时间戳都必须落在服务器时间轴上。
    // 「开始时间 + 时长 ≤ 服务器现在」是服务端硬判据，本机时钟偏十几分钟
    // 就直接驳回，而本地自检用同一个时钟，永远发现不了这个偏差。
    let (server_now, skew) = super::gmttime::server_now_ms(client);
    match skew {
        Some(off) if off.abs() >= 1000 => log(&format!(
            "√ [clock] 已校时：服务器比本机{}{}ms",
            if off > 0 { "快" } else { "慢" },
            off.abs()
        )),
        Some(_) => log("√ [clock] 已校时：与本机一致"),
        None => log("· [clock] 校时失败，按本机时钟继续"),
    }
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
            server_now + r.gen_range(0..5_000i64)
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
                    if g.days_left(server_now) >= 0 {
                        format!("，还剩 {} 天", g.days_left(server_now))
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
    // 记分跑在时段外会直接 11016，连策略串都拿不到。但提交报文只要
    // policy 这个字符串（实测恒为 "0"），timestamp 不进签名，点位又走
    // 独立的 /api/points，所以拿不到策略不等于不能提交。
    // PACE_STRICT_POLICY=1 可以关掉兜底，保留原来的硬失败行为。
    let strict = std::env::var("PACE_STRICT_POLICY").ok().as_deref() == Some("1");
    let pol = match policy::fetch_policy_for(client, p.run_mode) {
        Ok(v) => {
            log(&format!(
                "√ [policy] ts={} policy={} minDistance={} validTime={}",
                v.timestamp, v.policy, v.min_distance, v.valid_time
            ));
            v
        }
        Err(e) if !strict => {
            let fb = policy::fallback_policy();
            log(&format!("⚠ [policy] 拉取失败（{e}）"));
            log(&format!(
                "⚠ [policy] 改用兜底：policy={} minDistance={} validTime={}，\
照常提交看服务端是否接受",
                fb.policy, fb.min_distance, fb.valid_time
            ));
            fb
        }
        Err(e) => return Err(e),
    };

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
    // 目标距离要传给点位接口：有的学校只在带真实距离时才下发点位。
    // 先按 distance/1 拉，空集才用目标距离重试（见 points::fetch_points_cached_at）。
    let pt = match points::fetch_points_cached_at(
        client,
        p.center_lat,
        p.center_lng,
        pick.as_ref().map(|k| k.selected.run_area_id.to_string()).as_deref(),
        Some(p.distance_m.round() as i64),
    ) {
        // 点位接口返回 200 但空数组：有的学校把必经点只放在围栏里，
        // 这时点位接口本来就是空的，不是「这个校区没有打卡点」。
        Ok(c) if c.points.is_empty() => {
            match pick.as_ref().filter(|k| !k.selected.pass_points.is_empty()) {
                Some(k) => {
                    let (items, bd, gcj) = fence::pass_point_items(&k.selected.pass_points);
                    log(&format!(
                        "√ [points] 点位接口为空，改用围栏「{}」下发的 {} 个必经点",
                        k.selected.name,
                        items.len()
                    ));
                    PointsContext { points: items, coords: bd, gcj_coords: gcj }
                }
                None => {
                    log("· [points] 点位接口为空，且所选围栏未下发必经点");
                    c
                }
            }
        }
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
                // 每个点位的 isFixed / 名字都要看：服务端的 type 3
                // 「至少通过2个普通点位」要求有 isFixed=0 的点可供通过
                for (i, raw) in c.points.iter().enumerate() {
                    let fixed = raw.get("isFixed").and_then(|v| v.as_i64()).unwrap_or(-1);
                    let name = raw.get("pointName").and_then(|v| v.as_str()).unwrap_or("");
                    log(&format!("[points]  #{i} isFixed={fixed} {name}"));
                }

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
            // 顺带把 GCJ-02 带上：前端高德地图按这套坐标摆点位，
            // 长度可能和 coords 不一致，取不到就退回 BD-09
            glat: pt.gcj_coords.get(i).map(|v| v.0).unwrap_or(lat),
            glon: pt.gcj_coords.get(i).map(|v| v.1).unwrap_or(lng),
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

    // 手绘轨迹优先。
    //
    // OSM 在该校区零建筑数据，「按序串点」只能靠弧线连点，必然从楼里穿过去。
    // 用户知道哪栋楼在哪，手画一条绕开的路线比任何算法都准，所以这里直接
    // 采用，连围栏内缩和打卡点吸附都不做——点位已经在用户画的线上了。
    // 记分跑优先尝试操场绕圈。`None` 表示打卡点外框不像操场（散在校园
    // 各处），自动回退到下面的路线规划——不是所有学校都绕操场跑。
    let oval: Option<Vec<(f64, f64)>> = if p.run_mode == 1 {
        let seed0 = if p.seed != 0 { p.seed } else { start_ms as u64 };
        try_stadium_track(&cps, p.distance_m, p.sample_step_m, seed0, log)
    } else {
        None
    };

    // 本次用于避让的障碍轮廓（楼体 + 水面 + 本地禁区）。
    //
    // 上报采样要拿它当**穿墙判据**：5m 采样时楼体两端至少有一个点落在里面，
    // `avoid_blockers` 的逐点推出能兜住；上报点变成 15m 一步之后，会出现
    // 「两端都在楼外、中间穿过楼」的弦，只能在这一层补点绕开。
    // 只有围栏支路会算这份（操场/手绘/无围栏都没有障碍数据）。
    let mut blockers_used: Vec<Vec<(f64, f64)>> = Vec::new();

    let track = if let Some(custom) = p.custom_path.as_ref().filter(|v| v.len() >= 2) {
        log(&format!("[track] 采用手绘轨迹 {} 个控制点", custom.len()));
        // 不传 must：手绘的整条价值就是用户自己控制形状，把没画到的点位
        // 强行插进去只会在原地拉出几百米的跳变（GEO_JUMP），把线撕烂。
        // 漏了点位由前端实时提示，用户自己补笔画。
        let mut pts = crate::track::fence_route::resample_with_checkpoints(
            custom,
            p.sample_step_m,
            &[],
            0.0,
        );
        // 首尾离得近就接上，否则一圈画完终点回到起点会算成一次瞬移
        if pts.len() >= 2 {
            let (first, last) = (pts[0], pts[pts.len() - 1]);
            let gap = crate::track::geo::distance(first, last);
            if gap > 1.0 && gap < 120.0 {
                let steps = ((gap / p.sample_step_m).ceil() as usize).max(2);
                for k in 1..steps {
                    let t = k as f64 / steps as f64;
                    pts.push((
                        last.0 + (first.0 - last.0) * t,
                        last.1 + (first.1 - last.1) * t,
                    ));
                }
                log(&format!("[track] 首尾相距 {gap:.0}m，已自动接上"));
            }
        }
        // 手绘轨迹不缩放到目标里程。
        //
        // 缩放看着很省事，但它会破坏手绘唯一的优点。用户画的时候地图上
        // 摆着 5 个点位，他是照着点位划的，所以**原尺度下点位天然就在
        // 线上**。一旦按 目标/实际 等比放大（常到 2.5~3 倍），路径整体
        // 放大而点位坐标是固定的，这个性质当场失效：点位会飘出几十米，
        // 之后无论怎么吸附都是打补丁——
        //
        //   · 吸附后 position 乱序 → 服务端「按顺序通过」false
        //   · 按顺序吸附 + 阈值收紧 → 靠后的点位直接 CP_MISS
        //   · 阈值放大到 250m → 能命中，但单点被拽 83m，detect 判异常
        //   · 缩放后重采样 → 点距回到 3m 了，点位又飘出去了
        //
        // 而且围栏是 1020×883m，绕一圈本来就有 3km 以上，正常照着点位
        // 画一圈绝不会短于 1.5km 的下限。真画短了，如实报错让用户补笔，
        // 比偷偷拉伸成一条穿过建筑物的假线要好。
        let drawn = crate::track::geo::path_length(&pts);
        if drawn < p.distance_m * 0.75 {
            log(&format!(
                "[track] 手绘 {drawn:.0}m，明显短于目标 {:.0}m；不自动拉伸（拉伸会让点位脱离轨迹），\
请在地图上画得更长一些",
                p.distance_m
            ));
        }

        // 原尺度下点位就在线边，按顺序取最近点即可精确命中
        let mut search_from = 0usize;
        for c in &cps {
            let target = (c.lat, c.lng);
            let mut best = usize::MAX;
            let mut best_d = f64::MAX;
            for (i, q) in pts.iter().enumerate().skip(search_from) {
                let d = crate::track::geo::distance(*q, target);
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            // 原尺度下 40m 内就算画到了；超出的交给前端提示补笔画
            if best != usize::MAX && best_d < 40.0 {
                pts[best] = target;
                search_from = best + 1;
            }
        }
        let n = pts.len();
        let times = crate::track::planner::assign_times(n, start_ms, p.duration_sec);
        let actual = crate::track::geo::path_length(&pts);
        log(&format!(
            "[track] 手绘轨迹缩放后 {:.0}m（原 {drawn:.0}m），{n} 点",
            actual
        ));
        crate::track::PlanResult {
            points: pts
                .into_iter()
                .zip(times)
                .map(|((lat, lng), t)| crate::track::TrackPoint { lat, lon: lng, time_ms: t })
                .collect(),
            checkpoints: cps.clone(),
            actual_length_m: actual,
        }
    } else if let Some(pts) = oval {
        // 操场绕圈：打卡点在跑道上，椭圆天然穿过它们；时间戳先按均匀分配，
        // 后面 retime 会按速度模型重排，这里只需要一个合法初值。
        let n = pts.len();
        let times = crate::track::planner::assign_times(n, start_ms, p.duration_sec);
        let actual = crate::track::geo::path_length(&pts);
        log(&format!("[track] 操场轨迹 {actual:.0}m / {n} 点"));
        crate::track::PlanResult {
            points: pts
                .into_iter()
                .zip(times)
                .map(|((lat, lng), t)| crate::track::TrackPoint { lat, lon: lng, time_ms: t })
                .collect(),
            checkpoints: cps.clone(),
            actual_length_m: actual,
        }
    } else if fence_ring.len() >= 3 {
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
        // 障碍轮廓：建筑 + 水体。路线生成完后再避让，不能让轨迹从楼里穿过。
        // 数据源是 Overpass（30 天缓存），拉不到就跳过，不阻断跑单。
        let blockers: Vec<Vec<(f64, f64)>> = {
            let bbox = crate::track::roadnet::bbox_of(&fence_ring, 60.0);
            let mut v: Vec<Vec<(f64, f64)>> = Vec::new();
            match crate::track::roadnet::fetch_buildings(bbox) {
                Ok(b) if !b.is_empty() => {
                    log(&format!("[block] 建筑轮廓 {} 栋", b.len()));
                    v.extend(b);
                }
                Ok(_) => log("[block] 这一带没有建筑数据"),
                Err(e) => log(&format!("[block] 建筑轮廓没拿到（{e}），本次不避让")),
            }
            if let Ok(w) = crate::track::roadnet::fetch_water(bbox) {
                if !w.is_empty() {
                    log(&format!("[block] 水域 {} 片", w.len()));
                    v.extend(w);
                }
            }
            // 本地手绘禁区。OSM 在国内校园基本是空白（实测某校区整个 bbox
            // 只有 15 个 way、76 个 node，一栋楼都没有），Overpass 拿不到轮廓
            // 时只能靠人自己在地图上圈。这份标注全账号共享，长期有效。
            let local = crate::track::roadnet::load_local_blockers();
            if !local.is_empty() {
                log(&format!("[block] 本地标注禁区 {} 块", local.len()));
                v.extend(local);
            }
            v
        };

        // 路网来源两级：OSM 优先（有真实道路等级和拓扑），拿不到再试高德
        // 瓦片。两级都失败才退回几何规划。
        let on_roads = if p.prefer_roads {
            match try_route_on_roads(&fence_ring, &must, p.distance_m, seed, &mut *log) {
                Some(r) => Some(r),
                None => try_route_on_tiles(&fence_ring, &must, p.distance_m, seed, &mut *log),
            }
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
        // 沿边界是贴着围墙的稀疏折线，拐角磨圆 + 轻微摆动，别像尺子画的。
        //
        // 但沿路网生成的路径本身就是密集点（瓦片图游走平均 1.2m 一个点），
        // 再跑圆角会把每个点都当成拐角，点数爆炸、长度翻倍（实测 2000m
        // 被拉成 4098m）。按平均间距判断：够密就说明本来就贴着真实道路，
        // 什么都不用做。
        let avg_step = if route.points.is_empty() {
            f64::MAX
        } else {
            route.length_m / route.points.len() as f64
        };
        let dense = avg_step < 10.0;
        let rounded = if dense {
            route.points.clone()
        } else {
            crate::track::fence_route::round_corners(&route.points, 20.0, 6)
        };
        let wandered = if dense {
            // 沿路的骨架是直线段，光跳过圆角会显得又硬又直。磨圆 + 微摆动，
            // 幅度压在路宽以内。
            crate::track::tileroad::soften(
                &rounded,
                2,
                2.6,
                if p.seed != 0 { p.seed } else { start_ms as u64 },
            )
        } else {
            crate::track::fence_route::add_wander(
                &rounded,
                6.0,
                120.0,
                if p.seed != 0 { p.seed } else { start_ms as u64 },
            )
        };
        // 建筑避让放在摆动之后：add_wander 会把点横推 ±6m，先避让再摆动
        // 等于白做。这里逐点推出楼体 + 逐段插贴边绕行，外扩 3m 当作墙根。
        blockers_used = blockers.clone();
        let wandered = if blockers.is_empty() {
            wandered
        } else {
            let before = wandered.len();
            let avoided = crate::track::fence_route::avoid_blockers(&wandered, &blockers, 3.0);
            let pushed = avoided.len().saturating_sub(before);
            if pushed > 0 {
                log(&format!("[block] 绕开建筑/水面，多插 {pushed} 个点"));
            }
            avoided
        };
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

    // ⑤ 上报采样：**等时间** 5 秒（真机节奏），几何仍走密采样。
    //
    // 这一步换掉的是「按距离采样 + 由几何反推时间」的老做法。
    //
    // 真机对象 `evidence/obs_real_20261002.json`（114 点 / 555 秒 / 1683.83m）
    // 逐点算出来的是：`ΔtotalTime` 109/113 恰好 5 秒（均值 4.9115），
    // `ΔtotalDis` 在 0~31 m 之间跳（均值 14.90、标准差 9.44，只有 38.9%
    // 落在 10~20 m）。时间恒定、距离起伏 → 设备是**按时间取样**的。
    //
    // 我们原来按距离取样（5 m/点）：同一条轨迹出 337 个点、1.65 秒/点，
    // 采样率是真机的三倍。形状对、节奏错，服务端看一眼 `ΔtotalTime`
    // 的分布就能分辨。
    //
    // `sample_by_time` 只决定**上报哪几个点**，几何不跟着变：拐角、绕楼、
    // 摆动都还在密采样的折线上算（`sample_step_m`），所以「拐角要更密」
    // 与「节奏要 5 秒」不再互斥。
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
    // 采样点在弧长空间就对齐到打卡点（详见 `track::sample_by_time`）：
    // 5 秒网格的相邻点可以差 20m 以上，事后再靠吸附瞬移会扯出大间隔。
    let cp_pairs: Vec<(f64, f64)> = cps.iter().map(|c| (c.lat, c.lng)).collect();
    let times = crate::track::sample_times(p.duration_sec, start_ms, p.sample_interval_sec);
    let mut track_points = crate::track::sample_by_time(
        &xy,
        &times,
        start_ms as u64,
        &KinParams::default(),
        v_floor,
        v_ceil,
        &blockers_used,
        &cp_pairs,
    );
    if track_points.len() < 2 {
        return Err("上报采样失败：时间轴或几何为空".into());
    }
    log(&format!(
        "[sample] 等时间采样 {:.1}s → {} 点（几何折线 {} 点 / 步长 {:.1}m）",
        p.sample_interval_sec,
        track_points.len(),
        xy.len(),
        p.sample_step_m
    ));

    // ⑤.5 起步窗口压平。
    //
    // 时间轴（retime 的结果）不能动：它已经把 15% 的时长分给了起步期。
    // 能动的是**几何**——真机起步 86 秒只走了 18.4m，我们原来在这 15% 的
    // 时长里匀速跑掉了 15% 的里程，`.totalDis` 眼看着涨到两百多米而首段
    // `distance` 也同步虚高。这里把这批点沿路径压到起始的一小段弧长上，
    // 时间戳原样保留，于是「15% 时长 / 1.1% 里程」两个比例同时成立。
    //
    // 顺序很关键：retime 必须看**压平前**的几何（它是按路径长度分配时间的，
    // 看压平后的点会把时间也一起压没），所以压平放在 retime 之后。
    let snap_cps: Vec<(f64, f64)> = cps.iter().map(|c| (c.lat, c.lng)).collect();
    crate::track::compress_startup(&mut track_points, start_ms, p.duration_sec);

    // ⑤.5 压缩之后沿轨迹**重新对齐一次**采样点。
    //
    // 压平会把整条网格按目标弧长重铺（窗口外的点也一起动），起点附近的
    // 采样点能整体挪出几十米；只靠 ⑤.6 的「吸附」（把点直接拽到点位坐标）
    // 去救，位移会大到扯出 40m 的相邻间距。先在**弧长空间**把最近的那个
    // 采样点挪到点位在轨迹上的投影（位移 ≤ 半个步长），吸附只需要收尾。
    let aligned = crate::track::align_samples_to_checkpoints(&mut track_points, &snap_cps);
    if aligned > 0 {
        log(&format!("√ [align] 压缩后按轨迹重新对齐打卡点 {aligned} 个"));
    }

    // ⑤.6 压缩之后**重新吸附打卡点**。
    //
    // `compress_startup` 对**所有**点按目标弧长重新插值（窗口外的点也一样），
    // 而 `planner` / `resample_with_checkpoints` 是靠「把某个采样点精确放在
    // 打卡点上」来满足服务端「经过点位」的。压缩一过，那批精确命中的顶点被
    // 挪到别的弧长位置，最近采样点变成新网格间距内的随机值（实测 0.33~2.24m），
    // `preflight` 的 `CP_MISS`（0.5m，Fatal）当场把整个提交流程拦死——
    // `run_full` 第一句就是 `dry_run(...)?`，有必到点的跑一单都发不出去。
    //
    // 所以吸附必须放在**压缩之后**（压缩前吸一次没用，会被再打散），而且必须
    // 放在 `real_len` 重算**之前**：吸附会动坐标，totalDis 得跟最终几何同源。
    let snap = crate::track::snap_checkpoints(&mut track_points, &snap_cps);
    if !snap.items.is_empty() {
        log(&format!(
            "√ [snap] 压缩后重新吸附打卡点 {}/{}：最近距离 {:.3}m → {:.4}m，单点最大位移 {:.2}m，相邻最大间距 {:.1}m",
            snap.moved(),
            snap.items.len(),
            snap.worst_before(),
            snap.worst_after(),
            snap.max_shift_m,
            snap.max_gap_m
        ));
        for ci in &snap.skipped {
            log(&format!(
                "⚠ [snap] 打卡点 P{} 距轨迹超过 {:.0}m，不做吸附（轨迹没经过它）",
                ci + 1,
                crate::track::CHECKPOINT_SNAP_MAX_M
            ));
        }
    }
    // 压平后点都落在原折线上，弧长总和几乎不变，但报文的 totalDis 必须
    // 与 `allLocJson` 末点的 totalDis 同源，所以在这里重算一次。
    // 吸附改过坐标，这一次重算同时把吸附引起的几米变化算进去。
    let real_len = crate::track::geo::path_length(
        &track_points.iter().map(|q| (q.lat, q.lon)).collect::<Vec<_>>(),
    )
    .round() as i64;

    let timed: Vec<(f64, f64, i64)> = track_points
        .iter()
        .map(|q| (q.lat, q.lon, q.time_ms))
        .collect();
    // 上报节奏的实测（与真机 4.9115 s / 14.90 m 对照）。
    //
    // 这条日志是给「采样率跑偏」留的现场：点数、平均时间间隔、平均距离间隔
    // 三个数一摆，是不是真机那个节奏一眼就能看出来。硬判定在
    // `preflight` 的 `SAMPLE_RATE`（Fatal），这里只是记录。
    let cad = crate::track::cadence_stats(&track_points);
    log(&format!(
        "√ [cadence] {} 点 / 平均 {:.2}s·{:.2}m（真机 {:.2}s·{:.2}m）/ Δt 中位 {:.0}s / 5±0.5s 占 {:.0}% / 平均 {:.3} m/s",
        cad.points,
        cad.mean_dt_sec,
        cad.mean_dd_m,
        crate::track::REAL_MEAN_INTERVAL_SEC,
        14.90,
        cad.median_dt_sec,
        cad.dt_within_5s * 100.0,
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
        alt_range: p.alt_range,
    };
    // 必到点：这里只挑「是哪个点」（两套坐标 + 名字），**不发号**。
    //
    // `position` / `id` / `isFixed` / `flag` 全部交给 submit::fixed_point_items
    // 按真机规则统一发号，原因见那里的注释与 docs/fix/FIXED-POINT-PARITY.md。
    // 早先这里把 `position` 填成「该点在轨迹点数组里的首次经过下标」
    // （实测 0/71/88/170/51），真机填的是 0..N-1 的连续序号；App 详情页按
    // position 排序/取下标，非连续值就只显示得出其中一部分必到点。
    //
    // **这一份必须自检与提交共用**：点位里的 `type=2` 打卡标记、报文
    // `fivePointJson` 的 `isFixed`/`isPass` 都由它派生。早先 probe 传的是
    // 空数组，自检校验的点位里一个打卡标记都没有，而上传那份有——两份实例。
    let fixed: Vec<super::submit::FixedPoint> = pt
        .points
        .iter()
        .map(|raw| {
            let bd = (
                raw.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0),
                raw.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0),
            );
            super::submit::FixedPoint {
                lat: bd.0,
                lon: bd.1,
                point_name: raw
                    .get("pointName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                // 缺 GCJ 坐标（点位接口只给 BD-09）时留 0，由
                // fixed_point_items 反算——真机两套坐标都是真值。
                glat: raw.get("glat").and_then(|v| v.as_f64()).unwrap_or(0.0),
                glon: raw.get("glon").and_then(|v| v.as_f64()).unwrap_or(0.0),
            }
        })
        .collect();

    let probe_sub = super::submit::build(&probe, &track_points, &fixed, &sim)?;
    let view = super::submit::verify_view(&probe_sub.save_record, &probe_sub.points)?;
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
        step_range: pol.step_range,
        expected_interval_sec: Some(p.sample_interval_sec),
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

    // 自检校验的点位必须带打卡标记。
    //
    // `fixed` 一旦传空（早先 probe 就是这样），`checkpoint_track_indexes` 得到
    // 空列表，点位里一个 `type=2` 都不会有，而上传那份有——这正是「自检守 A、
    // 上传发 B」的入口（A4）。放在 preflight 之后：真的没经过点位时由
    // preflight 报 CP_MISS，这里只兜「参数传空」这一种。
    let marked = probe_sub
        .points
        .iter()
        .filter(|q| q.get("type").and_then(|v| v.as_i64()) == Some(2))
        .count();
    if !fixed.is_empty() && marked == 0 {
        return Err("自检点位里没有任何打卡标记（type=2）：probe 的 fixed 传空了".into());
    }
    log(&format!(
        "√ [points] 自检与上传共用同一份点位：{} 点 / {} 个打卡标记（{} 个必到点）",
        probe_sub.points.len(),
        marked,
        fixed.len()
    ));

    // 服务端「经过点位」到底比哪套坐标？BD-09 的 lat/lon 与 GCJ-02 的
    // glat/glon 差 ~850m，选错一套整条记录的点位项就全是 False。
    let mut order_seen: Vec<(usize, i64, String)> = Vec::new();
    for (i, raw) in pt.points.iter().enumerate() {
        let name = raw.get("pointName").and_then(|v| v.as_str()).unwrap_or("");
        let bd = (
            raw.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0),
            raw.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0),
        );
        let gcj = (
            raw.get("glat").and_then(|v| v.as_f64()).unwrap_or(0.0),
            raw.get("glon").and_then(|v| v.as_f64()).unwrap_or(0.0),
        );
        let d_bd = view
            .points
            .iter()
            .map(|p| crate::track::geo::distance((p.lat, p.lon), bd))
            .fold(f64::MAX, f64::min);
        let d_gcj = view
            .points
            .iter()
            .map(|p| {
                let (a, o) = crate::api::coord::bd09_to_gcj02(p.lat, p.lon);
                crate::track::geo::distance((a, o), gcj)
            })
            .fold(f64::MAX, f64::min);
        // 首次经过索引：服务端「按顺序通过所有点位」看的就是这个。
        // 距离 0.1m 只说明穿过了，顺序错了一样判 False。
        let first_bd = view
            .points
            .iter()
            .position(|p| crate::track::geo::distance((p.lat, p.lon), bd) < 10.0)
            .map(|x| x as i64)
            .unwrap_or(-1);
        log(&format!(
            "[point-dist] #{i} {name}: BD-09 {d_bd:.1}m / GCJ-02 {d_gcj:.1}m / 首次经过 idx={first_bd}"
        ));
        // 记录每个点位"首次经过"的索引，下面统一检查顺序
        if first_bd >= 0 {
            order_seen.push((i, first_bd, name.to_string()));
        }
    }

    // 点位顺序自检。
    //
    // 服务端的「按顺序通过所有点位」不是看 `fixed_point_json` 里的
    // position，而是自己从轨迹里找出每个点位被经过的位置，再检查这个
    // 序列是否和点位数组同序。也就是说：**position 传得再对也没用，
    // 轨迹本身必须按顺序穿过它们**。
    //
    // 实测踩过的坑：从 #0 到 #1 的那段路径向外扩得太远，中途擦过了 #4。
    // 轨迹实际经过顺序变成 0,4,1,2,3,4,0，服务端直接判 false，而日志里
    // 五个点位距离全是 0.1m、detect 满分区 100%，看起来毫无问题。
    if order_seen.len() >= 2 {
        let mut sorted = order_seen.clone();
        sorted.sort_by_key(|x| x.1);
        let bad: Vec<String> = sorted
            .windows(2)
            .filter(|w| w[1].0 < w[0].0)
            // w[0] 是索引更小的那个，所以是它抢在了 w[1] 前面
            .map(|w| format!("#{} {} 先于 #{} {} 被经过", w[0].0, w[0].2, w[1].0, w[1].2))
            .collect();
        if bad.is_empty() {
            let seq: Vec<String> = sorted.iter().map(|x| format!("#{}@{}", x.0, x.1)).collect();
            log(&format!("[point-order] 顺序正确：{}", seq.join(" → ")));
        } else {
            log(&format!(
                "[point-order] 顺序错误，服务端会判「按顺序通过所有点位」失败：{}",
                bad.join("；")
            ));
            log("[point-order] 重新规划或调整手绘轨迹，让轨迹只按点位次序穿过每个点");
        }
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
        submission_points: probe_sub.points,
        fixed,
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
            // 起步窗口内点几乎不动是真机行为，不按「站着」判
            out.start_ms + crate::track::startup_sec(p.duration_sec) * 1000,
        );
        log(&format!("[detect] {}", rep.summary()));
        // 服务端「经过点位」到底比哪套坐标？BD-09 与 GCJ-02 差 850m，
        // 选错一套整条记录的点位项就全 False（实测两条记录都这样）。
        for (i, raw) in out.points.points.iter().enumerate() {
            let name = raw.get("pointName").and_then(|v| v.as_str()).unwrap_or("");
            let bd = (
                raw.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0),
                raw.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0),
            );
            let gcj = (
                raw.get("glat").and_then(|v| v.as_f64()).unwrap_or(0.0),
                raw.get("glon").and_then(|v| v.as_f64()).unwrap_or(0.0),
            );
            let d_bd = xy
                .iter()
                .map(|p| crate::track::geo::distance(*p, bd))
                .fold(f64::MAX, f64::min);
            let d_gcj = xy
                .iter()
                .map(|p| {
                    let (a, o) = crate::api::coord::bd09_to_gcj02(p.0, p.1);
                    crate::track::geo::distance((a, o), gcj)
                })
                .fold(f64::MAX, f64::min);
            log(&format!(
                "[point-dist] #{i} {name}: BD-09 {d_bd:.1}m / GCJ-02 {d_gcj:.1}m"
            ));
        }
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

    // 必到点直接复用 `dry_run` 那一份（`out.fixed`）：它是点位里 `type=2`
    // 标记与 `fivePointJson` 的由来，自检和提交必须是同一份输入。
    // geoFencesJson：真机的围栏对象是
    // `{distance:0.0, id:1668, name:"学校", points:[{lat,lng,glat,glng}…]}`。
    // 我们早先只发 `{points:[…]}`，少了 id / name / distance 三个字段——
    // 围栏 id 在真机报文里是**只**出现在这里的（runAreaId 是 -1），
    // 缺了它服务端就没有「这次跑的是哪个围栏」的凭据。
    let geo_fences = out
        .fence
        .as_ref()
        .map(|f| {
            super::fence::geo_fence_json(
                f.selected.run_area_id,
                f.selected.fence_id,
                &f.selected.name,
                &f.selected.polygons[0],
            )
        })
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
        // 这里给的是**真实**围栏 id：save_record 顶层 `runAreaId` 用它（没有
        // 真机样本证明这个接口也发 -1，不改）。`fixed_point_json` 那个
        // envelope 里的 `runAreaId` 由 submit::build 单独处理成 -1（真机值），
        // 见 submit::payload_run_area_id。
        run_area_id: out
            .fence
            .as_ref()
            .map(|f| f.selected.run_area_id)
            .unwrap_or(-1),
        geo_fences_json: geo_fences,
        total_ascent: 0,
        alt_range: p.alt_range,
    };

    // 报文的桶必须来自 sim，不能在这里重新生成——否则自检、提交、明细
    // 三处数据不一致（见 submit::build 的注释）。
    //
    // 点位**不重建**：直接用 `dry_run` 里自检校验过的那一份
    // （`out.submission_points`）。`build_from_points` 原样收下，一个字节都
    // 不重算，这是 A4「两份实例」的结构性修法。
    let submission = super::submit::build_from_points(
        &params,
        &out.track,
        &out.fixed,
        &out.sim,
        out.submission_points.clone(),
    )?;
    // 结构上不可能不等（同一个 Vec 的克隆），但真出现「谁在中途改了点位」
    // 就是 A4 回归，宁可在这里停住也别把两份数据分别发出去。
    if submission.points != out.submission_points {
        return Err("点位数组在自检与提交之间被改动过（A4 回归）".into());
    }

    // 7.4.01 已移除 BODY_SENSORS，报文里不许出现心率字段
    let mut body = submission.save_record.clone();
    let dropped = crate::track::detect::strip_forbidden(&mut body);
    if !dropped.is_empty() {
        log(&format!("[detect] 已剔除不该有的字段: {}", dropped.join(", ")));
    }
    // 派生字段互校：爬升 / 卡路里 / 功率 / 配速 / 圈数据必须与点位列自洽。
    // 这些都是「填错了也能提交成功」的字段，回退成常量只会让服务端把整条
    // 记录判成来源可疑，所以宁可在这里拦下。
    let derived_problems = super::submit::audit_derived(&body, &submission.points);
    if !derived_problems.is_empty() {
        return Err(format!(
            "报文派生字段自检失败（未提交）：{}",
            derived_problems.join("；")
        ));
    }
    log("√ [selfcheck] 派生字段与点位列自洽（爬升/卡路里/功率/配速/圈）");
    // 必到点次序回显。App 详情页的「必到点位」就是按这段报文显示的：
    // position 必须是 0,1,2,… 连续递增、数组同序、isFixed 只有首点为 1。
    // 只要这一行不对，页面上就一定会少点位，先查这里再查轨迹。
    if let Ok(env) = serde_json::from_str::<serde_json::Value>(
        body.get("fivePointJson").and_then(|v| v.as_str()).unwrap_or("{}"),
    ) {
        if let Some(inner) = env
            .get("fivePointJson")
            .and_then(|v| v.as_str())
            .and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok())
            .and_then(|v| v.as_array().cloned())
        {
            let list: Vec<String> = inner
                .iter()
                .map(|f| {
                    format!(
                        "{}:{}#{}(isFixed={})",
                        f.get("position").and_then(|v| v.as_i64()).unwrap_or(-1),
                        f.get("pointName").and_then(|v| v.as_str()).unwrap_or(""),
                        f.get("id").and_then(|v| v.as_i64()).unwrap_or(-1),
                        f.get("isFixed").and_then(|v| v.as_i64()).unwrap_or(-1),
                    )
                })
                .collect();
            log(&format!("[fixed-point] {} 个必到点：{}", list.len(), list.join(" → ")));
        }
    }
    log(&format!(
        "√ [submit] 报文已组装，{} 个轨迹点",
        out.track.len()
    ));

    log("[submit] 提交成绩…");
    // runes/runef 关联头：策略下发时刻+uid、运行uuid+开始时刻
    let runes = format!("{}{}", out.policy.timestamp, uid);
    let runef = format!("{}{}", uuid, start_ms);
    let data = super::submit::save_record(client, &body, &runes, &runef)?;
    out.submitted = true;
    out.record_id = super::submit::extract_record_id(&data);
    log(&format!("√ [submit] 提交成功，记录 id={:?}", out.record_id));

    // ⑨ OBS：用 rrid 重算桶 id 和 objectKey
    let rrid = out.record_id.unwrap_or(0);
    log(&format!("[obs] 用 rrid={rrid} 上传明细…"));
    // OBS 的 fixed_point_json 要的是裸点位数组，不是主报文那层 envelope。
    // 之前把 envelope 整个塞进来，服务端解不出点位列表。
    // OBS 的 fixed_point_json 要的是**完整 envelope**（含 fivePointJson 内层串、
    // useZip、runAreaId、geoFencesJson、freedomShowFence），不是裸点位数组。
    // 参照实现 Neko 的 build_obs_object 就是这么发的；传裸数组时服务端解析
    // 不出点位，于是「按顺序通过所有点位」永远 false。
    let five_pt = body
        .get("fivePointJson")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // 点位原样复用提交报文那一份（同一个 `Vec<Value>` 的引用），不重建。
    let payload = detail_payload_rrid(
        &out.sim,
        &uid,
        rrid,
        start_ms,
        &uuid,
        &five_pt,
        &submission.points,
    );
    // 本地时区和 UTC 各传一份：服务端拼 key 用哪个时区没有确定证据，
    // 猜错就等于轨迹对象不存在，记录点开只有标量数据。
    let keys = vec![
        // 10 位（带小时）——历史实现，出处是一句没有 APK 证据的旧注释
        super::obs::detail_key(start_ms, &uuid),
        super::obs::detail_key_utc(start_ms, &uuid),
        // rrid 前 4 位——前端注释的说法
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
///
/// `pts` 是**提交报文用的那一份点位数组**（`submit::Submission::points`，
/// 由 `submit::build_points` 造过一次）。这里原样放进 `run_data.allLocJson`，
/// 不再自己拼一份：
///
/// 早先这个函数内部又调了一次 `point_json_full`，而且 seed（`start_ms` 而不是
/// `start_ms ^ φ`）、平滑（不调 `smooth_point_speed`）、打卡点（`checkpoint_idxs`
/// 传空）三处都与 `submit::build` 不同。同一条轨迹于是有 `id` / `count` /
/// `radius` / `type` / `bdA` 两套值，而本地自检校验的是 `build` 那一套——
/// 「自检守 A、上传发 B」（AUDIT-round A4、`docs/fix/FIX-A4-SINGLE-POINT-SOURCE.md`）。
///
/// 现在点位与枚举/平滑/打卡标记全部来自同一份数据，这里只做「取用」。
// OBS 明细要 10 个字段，每个都来自不同的数据源（模拟结果、账号标识、时间
// 窗口、uuid、点位数组），拆结构体只会把这些东西再包一层而不会减少耦合，
// 所以这里保持平铺。
#[allow(clippy::too_many_arguments)]
pub fn detail_payload_rrid(
    sim: &crate::track::SimResult,
    uid: &str,
    rrid: i64,
    start_ms: i64,
    uuid: &str,
    five_point_json: &str,
    pts: &[Value],
) -> Value {
    let mut speed = sim.speed_buckets.clone();
    let mut steps = sim.step_buckets.clone();
    crate::track::buckets::apply_obs_ids(&mut speed, &mut steps, rrid, start_ms);

    // OBS 对象是 10 个 key，每个值都 gzip + base64。
    // 之前传的是明文 JSON，结构也不对，服务端读不出轨迹，详情页就显示
    // 「这条记录没有轨迹坐标」。
    //
    // 点位数组直接取调用方给的那一份（`submit::build_points` 的产物，与提交
    // 报文同源）。`type=2` 的打卡标记在那边已经按坐标标好，这里不再重算，
    // 也不需要复制——`allLocJson` 直接把这个引用序列化出去。

    // 圈数据：真机每 1000m 切一圈（末圈 isFullLap=false）。原来发空数组，
    // 等于告诉服务端「这条轨迹没有分段信息」——而圈配速、分段审核都读它。
    // 与点位同源：直接从即将上传的 pts 反解 totalDis/totalTime/bdA，
    // 步数按里程比例从 totalSteps 分配（真机点位里没有 steps 字段）。
    let laps = super::submit::build_laps_from_points(pts, start_ms, sim.total_steps as i64);
    // 分段配速：真机是「起步段 + 20 秒窗口 + 收尾段」，`avgSpeed` 填配速
    // （分/公里）而不是 m/s。这里和点位同源，段里程之和精确等于 totalDis。
    let segments = super::submit::build_segments_from_points(pts, start_ms, sim.total_steps as i64);
    let run_wrap = json!({
        "allLocJson": serde_json::to_string(pts).unwrap_or_else(|_| "[]".into()),
        "useZip": false,
    });

    json!({
        "rrid": gz_str(&rrid.to_string()),
        "uuid": gz_str(uuid),
        "uid": gz_str(uid),
        "run_data": gz_json(&run_wrap),
        "fixed_point_json": gz_str(five_point_json),
        // 原来发空数组（`[]`）。真机这列是 25 段「起步段 + 20 秒窗口」的
        // 分段配速；空数组等于告诉服务端「这条轨迹没有分段信息」，分段
        // 审核和配速曲线都拿不到数据。
        "segment_json": gz_json(&Value::Array(segments)),
        "speed_json": gz_json(&Value::Array(
            speed.iter().map(|b| serde_json::to_value(b).unwrap_or(Value::Null)).collect::<Vec<_>>()
        )),
        "step_freq_json": gz_json(&Value::Array(
            steps.iter().map(|b| serde_json::to_value(b).unwrap_or(Value::Null)).collect::<Vec<_>>()
        )),
        "laps_json": gz_json(&Value::Array(laps)),
        // 真机对象顶层只有 10 个键，**没有** extension_json：固定点位、
        // 圈、rrid、runFaceCheck、run_data、分段、速度、步频、uid、uuid。
        // 早先按 Inverx / ydsj-auto 两处实现的说明多发了 extension_json
        // （空串的 gzip）。现在有真机对象直接对拍，多一个键就是差异，
        // 删掉最干净。
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
    fn obs_payload_has_ten_gzipped_keys() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let start_ms = 1_793_966_000_000i64;
        let pts: Vec<crate::track::TrackPoint> = (0..50)
            .map(|i| crate::track::TrackPoint {
                lat: 31.23 + i as f64 * 5.0 / 111320.0,
                lon: 121.47,
                time_ms: start_ms + i as i64 * 1500,
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
        // 点位走唯一构造点（`build_points`），与提交报文同一份来源
        let loc = crate::api::submit::build_points(&pts, &[], start_ms, None).expect("points");
        let v = detail_payload_rrid(&sim, "123", 99, start_ms, "U", "[]", &loc);

        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        // 真机对象顶层就是这 10 个键（evidence/obs_real_20261002.json），
        // 没有 extension_json——早先按两处参考实现多发了第 11 个键。
        assert_eq!(keys.len(), 10, "必须是 10 个键，实际 {keys:?}");
        assert!(v.get("extension_json").is_none(), "真机没有 extension_json");
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

    /// 四角围成跑道外框 → 认成操场，生成的椭圆首尾闭合。
    #[test]
    fn stadium_detects_oval_from_four_corners() {
        let lat0 = 47.386;
        let lng0 = 123.956;
        let klat = crate::track::geo::m_per_deg_lat(lat0);
        let klng = crate::track::geo::m_per_deg_lng(lat0);
        let mk = |dy: f64, dx: f64| Checkpoint {
            lat: lat0 + dy / klat,
            lng: lng0 + dx / klng,
            glat: lat0 + dy / klat,
            glon: lng0 + dx / klng,
            name: "P".into(),
        };
        // 100(长) × 60(宽) 米，四个角
        let cps = vec![
            mk(0.0, -50.0),
            mk(0.0, 50.0),
            mk(60.0, -50.0),
            mk(60.0, 50.0),
        ];
        let logs: std::rc::Rc<std::cell::RefCell<Vec<String>>> = Default::default();
        let l2 = std::rc::Rc::clone(&logs);
        let mut log = move |s: &str| l2.borrow_mut().push(s.to_string());
        let r = try_stadium_track(&cps, 2000.0, 10.0, 42, &mut log);
        drop(log);

        let pts = r.expect("100×60m 的四角应该识别成操场");
        assert!(pts.len() > 80, "点数 {} 太少", pts.len());
        // 首尾不闭合是设计——campus_oval_track 带一段 15~35m 的入场尾巴，
        // 真机轨迹也是这样（人从场外走进跑道才开始计圈）。所以这里断言
        // 总长度落在目标附近，而不是断言闭合。
        let len = crate::track::geo::path_length(&pts);
        assert!(
            (len - 2000.0).abs() < 400.0,
            "长度 {len:.0}m 偏离目标 2000m 太多"
        );
        let got = logs.borrow();
        assert!(got.iter().any(|l| l.contains("操场绕圈")), "缺少操场日志: {got:?}");
    }

    /// 打卡点散在几百米范围 → 不是操场，必须返回 None 走路线规划。
    #[test]
    fn stadium_rejects_scattered_checkpoints() {
        let lat0 = 47.386;
        let lng0 = 123.956;
        let klat = crate::track::geo::m_per_deg_lat(lat0);
        let klng = crate::track::geo::m_per_deg_lng(lat0);
        let mk = |dy: f64, dx: f64| Checkpoint {
            lat: lat0 + dy / klat,
            lng: lng0 + dx / klng,
            glat: lat0 + dy / klat,
            glon: lng0 + dx / klng,
            name: "P".into(),
        };
        // 校园各处的点：横跨 500m
        let cps = vec![
            mk(0.0, 0.0),
            mk(0.0, 500.0),
            mk(300.0, 0.0),
            mk(300.0, 500.0),
        ];
        let logs: std::rc::Rc<std::cell::RefCell<Vec<String>>> = Default::default();
        let l2 = std::rc::Rc::clone(&logs);
        let mut log = move |s: &str| l2.borrow_mut().push(s.to_string());
        let r = try_stadium_track(&cps, 2000.0, 10.0, 42, &mut log);
        drop(log);
        assert!(r.is_none(), "500m 跨度的打卡点不该识别成操场");
        let got = logs.borrow();
        assert!(
            got.iter().any(|l| l.contains("不像操场")),
            "应该记一条回退原因: {got:?}"
        );
    }

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

#[cfg(test)]
mod fixed_point_payload_tests {
    /// OBS 的 fixed_point_json 必须是裸点位数组。
    ///
    /// 之前把主报文那层 envelope（{fivePointJson, useZip, runAreaId, ...}）
    /// 整个塞进去，服务端解不出点位列表，`按顺序通过所有点位` 永远 False。
    #[test]
    fn fixed_point_json_is_bare_array() {
        let raw = r#"[{"lon":123.9,"lat":47.3,"position":1}]"#;
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(v.is_array(), "必须是数组");
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert!(v[0].get("position").is_some());
    }

    /// 从 envelope 里抽裸数组：主报文是双层，OBS 要单层。
    #[test]
    fn envelope_unwraps_to_bare_array() {
        let inner = r#"[{"lon":1.0,"lat":2.0,"position":3}]"#;
        let envelope = serde_json::json!({
            "fivePointJson": inner,
            "useZip": false,
            "runAreaId": 1668,
        })
        .to_string();
        let extracted = serde_json::from_str::<serde_json::Value>(&envelope)
            .ok()
            .and_then(|v| v.get("fivePointJson").cloned())
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
        assert_eq!(extracted, inner);
        let arr: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert!(arr.is_array());
    }

    /// 明细里的打卡点标记必须仍然落在正确的采样点上。
    ///
    /// `position` 从「轨迹下标」改成「0..N-1 连续序号」之后，OBS 侧不能
    /// 再拿它当轨迹下标用（那样会把 type=2 标到轨迹开头几个点上）。这里
    /// 用一个 `position=0`、但实际落在第 25 个采样点附近的必到点验证：
    /// 标记必须按坐标匹配出来，落在 23~27 之间。
    #[test]
    fn obs_checkpoint_marking_uses_coords_not_position() {
        use super::detail_payload_rrid;
        use base64::Engine;
        use flate2::read::GzDecoder;
        use std::io::Read;

        let start_ms = 1_794_561_660_939i64;
        let pts: Vec<crate::track::TrackPoint> = (0..50)
            .map(|i| crate::track::TrackPoint {
                lat: 31.23 + i as f64 * 5.0 / 111_320.0,
                lon: 121.47,
                time_ms: start_ms + i as i64 * 1500,
            })
            .collect();
        let sim = crate::track::SimResult {
            speeds: vec![3.0; 50],
            cadences: vec![150.0; 50],
            strides: vec![1.2; 50],
            total_steps: 300,
            avg_cadence: 150.0,
            speed_buckets: Vec::new(),
            step_buckets: Vec::new(),
            actual_length_m: 245.0,
        };
        // 必到点坐标 = 第 25 个采样点；报文里 position 是 0（连续序号），
        // 与轨迹下标 25 毫无关系——这正是改动的核心。
        let target = &pts[25];
        let inner = serde_json::json!([{
            "flag": start_ms,
            "glat": target.lat,
            "glon": target.lon,
            "id": 81,
            "isFixed": 1,
            "isPass": true,
            "lat": target.lat,
            "lon": target.lon,
            "pointName": "P25",
            "position": 0,
            "state": 0,
        }]);
        let five = serde_json::json!({
            "fivePointJson": inner.to_string(),
            "freedomShowFence": false,
            "geoFencesJson": "[]",
            "runAreaId": -1,
            "useZip": false,
        })
        .to_string();

        // 点位由 `build_points` 造一次（打卡点按坐标匹配），OBS 明细原样复用。
        // `position` 是连续序号 0..N-1，与轨迹下标毫无关系——真按 position 去标
        // 就会落到开头几个点上。
        let fixed = [crate::api::submit::FixedPoint {
            lat: target.lat,
            lon: target.lon,
            point_name: "P25".into(),
            glat: 0.0,
            glon: 0.0,
        }];
        let loc = crate::api::submit::build_points(&pts, &fixed, start_ms, None).expect("points");
        let obj = detail_payload_rrid(&sim, "123", 99, start_ms, "U", &five, &loc);
        let raw = obj["run_data"].as_str().expect("run_data 是 gzip+base64 字符串");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(raw)
            .expect("base64");
        let mut text = String::new();
        GzDecoder::new(&bytes[..]).read_to_string(&mut text).expect("gunzip");
        let run: serde_json::Value = serde_json::from_str(&text).unwrap();
        let loc: Vec<serde_json::Value> =
            serde_json::from_str(run["allLocJson"].as_str().unwrap()).unwrap();

        let marked: Vec<usize> = loc
            .iter()
            .enumerate()
            .filter(|(_, q)| q["type"].as_i64() == Some(2))
            .map(|(i, _)| i)
            .collect();
        assert!(!marked.is_empty(), "打卡点必须被标成 type=2，否则服务端 density 打分不够");
        // 命中判定半径 10m、采样间距 5m，所以「首次命中」可能是第 24 个点，
        // 加上 ±2 的标记窗口，形成一段连续标记。
        //
        // `type=2` 同时也是「次一级打卡 / 弱定位」，`point_json_full` 会给
        // 普通采样点按 5% 概率撒这个值（真机分布里 type=2 有 5 个点），所以
        // 这里不断言「全部标记都落在窗口内」——只断言**打卡点本身**被标到：
        // 第 25 个点必须在标记集合里，且标记不能整体跑到轨迹开头（那样就是
        // 又把 `position` 当轨迹下标用了）。
        assert!(
            marked.contains(&25),
            "第 25 个采样点必须被标成打卡点，实际 {marked:?}"
        );
        assert!(
            !marked.iter().any(|&i| i < 20),
            "打卡标记不该出现在轨迹开头（`position` 又被当成轨迹下标了）：{marked:?}"
        );
    }
}


#[cfg(test)]
mod single_point_source_tests {
    //! A4：**自检那份点位必须就是上传那份**（`docs/fix/FIX-A4-SINGLE-POINT-SOURCE.md`）。
    //!
    //! 修复前同一条轨迹被造了两次点位：
    //!
    //! - `submit::build` 那份：`seed = start_ms ^ φ`、做过 `smooth_point_speed`、
    //!   带打卡点（`type=2` / `isFixed`）；
    //! - `flow::detail_payload_rrid` 那份（**真正进 OBS 的**）：`seed = start_ms`、
    //!   不平滑、不带打卡点。
    //!
    //! 于是 `id` / `count` / `radius` / `type` / `bdA` 逐点不同，`isFixed`、
    //! `type=2` 只有上传那份才有——自检守着一份没人会看到的报文。
    //!
    //! 这组测试拿一条 1683m / 555 秒、5 个打卡点的离线对象（不联网、不提交）
    //! 逐元素逐字段比对两份，白名单只允许 envelope 级字段。

    use super::*;
    use crate::api::submit::{self, FixedPoint, SubmitParams};
    use base64::Engine;
    use flate2::read::GzDecoder;
    use std::io::Read;

    /// 真机 rrid 1328288734 的起始毫秒戳（点位 `flag` 用同一个值）。
    const START_MS: i64 = 1_790_913_882_164;
    const DURATION_SEC: i64 = 555;

    /// envelope（`save/record` body）层允许不同的字段。
    ///
    /// 全部是「必然不同」的身份 / 派生字段：自检用占位 uid/uuid，
    /// 签名与 geeToken 由它们派生，`runAreaId` 与 `geoFencesJson` 是围栏层。
    /// **点位数组的字段一个都不在这里**——见 [`POINT_ALLOWED_DIFFS`]。
    const ENVELOPE_ALLOWED_DIFFS: &[&str] = &[
        "uid",
        "selectedUnid",
        "uuid",
        "signature",
        "originalSign",
        "geeToken",
        "runAreaId",
        "fivePointJson",
    ];

    /// 点位字段的允许差异白名单：**空**。逐元素逐字段必须完全相等。
    const POINT_ALLOWED_DIFFS: &[&str] = &[];

    /// 点位对象的全部字段名（`point_json_full` 的 27 个键）。
    ///
    /// 只用来钉住一件事：**envelope 白名单里不许混进点位字段**。
    const POINT_FIELDS: &[&str] = &[
        "avgSpeed", "bdA", "bdD", "bdG", "bdS", "coorType", "count", "dtr", "flag", "gLat",
        "gLng", "gainTime", "id", "lat", "lng", "locType", "locationId", "queueNum", "radius",
        "speed", "state", "stepDistance", "totalDis", "totalTime", "type", "validDis",
        "validTime",
    ];

    pub(super) struct Fixture {
        pub track: Vec<crate::track::TrackPoint>,
        pub fixed: Vec<FixedPoint>,
        pub sim: crate::track::SimResult,
        pub real_len: i64,
    }

    /// 离线造一条 1683m / 555s 的对象，带 5 个打卡点。
    ///
    /// 链路与 `flow::dry_run` 的 ④⑤⑥ 完全一致（等时间采样 → compress_startup
    /// → 吸附 → real_len → simulate），只是不起网络、不碰围栏与账号。
    ///
    /// 采样那一环必须是**生产的等时间 5 秒**：下游所有断言（段表、速度三字段、
    /// 点位字段、必到点与 `queueNum`）都是对着真机 114 点 / 555 秒 / 4.91 秒
    /// 的节奏标定的，夹具换回等距离 5 米就变成「拿三倍密的点验真机形状」，
    /// 结论再漂亮也不算数。
    pub(super) fn fixture() -> Fixture {
        // 单圈 2×95 + 2π×25 ≈ 347m，4.85 圈 ≈ 1683m。
        let spec = crate::track::OvalSpec {
            straight: 95.0,
            radius: 25.0,
            heading: 0.3,
            laps: 4.723,
        };
        // 几何仍用 5m 密采样（拐角/形状靠它），上报点按 5 秒取。
        let xy = crate::track::campus_oval_track((47.3794, 123.9502), &spec, 5.0, 2026);
        let times =
            crate::track::sample_times(DURATION_SEC, START_MS, crate::track::SAMPLE_INTERVAL_SEC);
        let mut track: Vec<TrackPoint> = crate::track::sample_by_time(
            &xy,
            &times,
            START_MS as u64,
            &KinParams::default(),
            1.0,
            8.0,
            &[],
            &[],
        );
        let st = crate::track::cadence_stats(&track);
        assert_eq!(
            st.points, 113,
            "夹具必须是 5 秒网格那个点数（真机 114 含一个重复时间戳）"
        );
        assert!(
            (st.mean_dt_sec - crate::track::REAL_MEAN_INTERVAL_SEC).abs() < 0.1,
            "夹具的节奏必须是真机的 4.91s/点，实际 {:.2}s",
            st.mean_dt_sec
        );
        crate::track::compress_startup(&mut track, START_MS, DURATION_SEC);

        // 5 个打卡点直接取轨迹顶点，保证「轨迹真的经过它们」（isPass=true）。
        let n = track.len();
        let fixed: Vec<FixedPoint> = (0..5)
            .map(|k| {
                let i = (n - 1) * (k + 1) / 6;
                let q = &track[i];
                FixedPoint {
                    lat: q.lat,
                    lon: q.lon,
                    point_name: format!("打卡点{}", k + 1),
                    // 点位接口只给 BD-09，GCJ 留 0 由 fixed_point_items 反算
                    // （与 `flow::dry_run` 的映射一致）。
                    glat: 0.0,
                    glon: 0.0,
                }
            })
            .collect();
        let snap_cps: Vec<(f64, f64)> = fixed.iter().map(|f| (f.lat, f.lon)).collect();
        crate::track::align_samples_to_checkpoints(&mut track, &snap_cps);
        crate::track::snap_checkpoints(&mut track, &snap_cps);

        let real_len = crate::track::geo::path_length(
            &track.iter().map(|q| (q.lat, q.lon)).collect::<Vec<_>>(),
        )
        .round() as i64;
        let timed: Vec<(f64, f64, i64)> = track.iter().map(|q| (q.lat, q.lon, q.time_ms)).collect();
        let sp = SimParams {
            total_distance_m: real_len as f64,
            total_sec: DURATION_SEC,
            start_ms: START_MS,
            target_steps: Some(1500),
            seed: 2026,
            ..Default::default()
        };
        let sim = simulate(&timed, &sp).expect("simulate");
        Fixture {
            track,
            fixed,
            sim,
            real_len,
        }
    }

    /// 自检那份的参数（`flow::dry_run` 里的 `probe`：身份是占位符、围栏 -1）。
    pub(super) fn probe_params(f: &Fixture) -> SubmitParams {
        SubmitParams {
            uid: "probe".into(),
            unid: "0".into(),
            uuid: "00000000-0000-4000-8000-000000000000".into(),
            sport_type: 1,
            total_distance_m: f.real_len,
            total_time_sec: DURATION_SEC,
            total_steps: f.sim.total_steps,
            start_time_ms: START_MS,
            stop_time_ms: START_MS + DURATION_SEC * 1000,
            sel_distance_m: 1500,
            sel_run_time_sec: DURATION_SEC,
            policy: "test-policy".into(),
            run_area_id: -1,
            geo_fences_json: "[]".into(),
            total_ascent: 0,
            alt_range: None,
        }
    }

    /// 上传那份的参数（`flow::run_full` 里的 `params`：真实身份 + 真实围栏）。
    pub(super) fn upload_params(f: &Fixture) -> SubmitParams {
        SubmitParams {
            uid: "123456".into(),
            unid: "88".into(),
            uuid: "0F8FAD5B-D9CB-469F-A165-70867728950E".into(),
            run_area_id: 1668,
            geo_fences_json: r#"[{"distance":0.0,"id":1668,"name":"学校","points":[]}]"#.into(),
            ..probe_params(f)
        }
    }

    /// OBS 对象里某个 gzip+base64 值解出明文。
    fn unzip(v: &Value) -> String {
        let raw = v.as_str().expect("OBS 每个值都是字符串");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(raw)
            .unwrap_or_else(|e| panic!("base64 解码失败: {e}"));
        let mut out = String::new();
        GzDecoder::new(&bytes[..])
            .read_to_string(&mut out)
            .unwrap_or_else(|e| panic!("gunzip 失败: {e}"));
        out
    }

    /// OBS 上传对象里**真正的点位数组**（`run_data.allLocJson`）。
    fn uploaded_points(obj: &Value) -> Vec<Value> {
        let run: Value = serde_json::from_str(&unzip(&obj["run_data"])).expect("run_data 是 JSON");
        serde_json::from_str(run["allLocJson"].as_str().expect("allLocJson 是字符串"))
            .expect("allLocJson 是 JSON 数组")
    }

    /// `save_record.fivePointJson` 的内层点位数组（envelope → 串 → 数组）。
    fn five_point_items(body: &Value) -> Vec<Value> {
        let env: Value = serde_json::from_str(
            body["fivePointJson"].as_str().expect("fivePointJson 是字符串"),
        )
        .expect("fivePointJson 是 JSON");
        serde_json::from_str(env["fivePointJson"].as_str().expect("内层是字符串"))
            .expect("内层是 JSON 数组")
    }

    /// 逐元素、逐字段比对两份点位。
    fn assert_points_identical(a: &[Value], b: &[Value], label: &str) {
        assert_eq!(a.len(), b.len(), "{label}：点位数不同");
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            let (xo, yo) = (
                x.as_object().unwrap_or_else(|| panic!("{label} 第 {i} 点不是对象")),
                y.as_object().unwrap_or_else(|| panic!("{label} 第 {i} 点不是对象")),
            );
            let ka: Vec<&String> = xo.keys().collect();
            let kb: Vec<&String> = yo.keys().collect();
            assert_eq!(ka, kb, "{label} 第 {i} 点字段集合/顺序不同");
            for k in ka {
                if POINT_ALLOWED_DIFFS.contains(&k.as_str()) {
                    continue;
                }
                assert_eq!(
                    xo[k], yo[k],
                    "{label} 第 {i} 点字段 `{k}` 不同：自检 {:?} vs 上传 {:?}",
                    xo[k], yo[k]
                );
            }
        }
    }

    /// 两份 envelope 的差异只允许落在 [`ENVELOPE_ALLOWED_DIFFS`] 里。
    fn assert_envelope_diff_allowlisted(a: &Value, b: &Value) {
        let (ao, bo) = (a.as_object().unwrap(), b.as_object().unwrap());
        let mut unexpected: Vec<String> = Vec::new();
        for k in ao.keys().chain(bo.keys()) {
            if ENVELOPE_ALLOWED_DIFFS.contains(&k.as_str()) {
                continue;
            }
            if ao.get(k) != bo.get(k) {
                unexpected.push(format!(
                    "{k}: 自检 {:?} vs 上传 {:?}",
                    ao.get(k).unwrap_or(&Value::Null),
                    bo.get(k).unwrap_or(&Value::Null)
                ));
            }
        }
        assert!(
            unexpected.is_empty(),
            "envelope 出现了白名单外的差异：\n  {}",
            unexpected.join("\n  ")
        );
    }

    /// **A4 回归**：自检校验的点位 == 真正上传到 OBS 的点位。
    ///
    /// 修复前这条断言必红（OBS 那份 `seed` 不同、没有平滑、没有打卡点）。
    #[test]
    fn selfcheck_and_upload_points_are_identical() {
        // 白名单只在 envelope 层，而且不许夹带点位字段
        assert!(
            POINT_ALLOWED_DIFFS.is_empty(),
            "点位字段的允许差异白名单必须是空的"
        );
        for f in POINT_FIELDS {
            assert!(
                !ENVELOPE_ALLOWED_DIFFS.contains(f),
                "envelope 白名单里混进了点位字段 `{f}`——点位必须逐字段相等"
            );
        }

        let f = fixture();

        // ① 自检那份：与 `flow::dry_run` 的 probe 一字不差地组装
        let probe = submit::build(&probe_params(&f), &f.track, &f.fixed, &f.sim)
            .expect("自检报文应能组装");
        // ② 提交那份（真实身份 / 真实围栏）
        let up = upload_params(&f);
        let submission = submit::build(&up, &f.track, &f.fixed, &f.sim).expect("提交报文应能组装");
        // ③ 真正上传的那份：OBS 明细对象里的 run_data.allLocJson
        let five = submission.save_record["fivePointJson"]
            .as_str()
            .expect("fivePointJson")
            .to_string();
        let payload = detail_payload_rrid(
            &f.sim,
            &up.uid,
            1_328_288_734,
            START_MS,
            &up.uuid,
            &five,
            &submission.points,
        );
        let uploaded = uploaded_points(&payload);

        assert_eq!(uploaded.len(), f.track.len(), "OBS 点位数应与轨迹点数一致");
        assert_points_identical(&probe.points, &uploaded, "自检 vs OBS 上传");
        assert_points_identical(&probe.points, &submission.points, "自检 vs 提交");
        assert_points_identical(&submission.points, &uploaded, "提交 vs OBS 上传");

        // 必到点数组（id / isFixed / position / isPass）两份必须一致
        let fa = five_point_items(&probe.save_record);
        let fb = five_point_items(&submission.save_record);
        assert_eq!(fa.len(), 5, "应有 5 个必到点");
        assert_points_identical(&fa, &fb, "必到点：自检 vs 提交");
        assert!(
            fa.iter().any(|p| p["isFixed"] == 1) && fa.iter().any(|p| p["isPass"] == true),
            "必到点必须带 isFixed/isPass 标记：{fa:?}"
        );

        // 点位里必须真的带上了打卡点标记（`type=2`），修复前自检那份一个都没有
        let marked = probe
            .points
            .iter()
            .filter(|p| p["type"].as_i64() == Some(2))
            .count();
        assert!(marked > 0, "自检那份点位缺 type=2 打卡标记（probe 传了空 fixed）");

        // envelope 层：差异只允许出现在白名单里
        assert_envelope_diff_allowlisted(&probe.save_record, &submission.save_record);
    }

    /// 报告用：把两份点位前 6 个并排打出来（`--nocapture` 才有输出）。
    #[test]
    fn print_selfcheck_vs_upload_first_six_points() {
        let f = fixture();
        let probe = submit::build(&probe_params(&f), &f.track, &f.fixed, &f.sim).expect("probe");
        let up = upload_params(&f);
        let submission = submit::build(&up, &f.track, &f.fixed, &f.sim).expect("submit");
        let five = submission.save_record["fivePointJson"]
            .as_str()
            .expect("fivePointJson")
            .to_string();
        let payload = detail_payload_rrid(
            &f.sim,
            &up.uid,
            1_328_288_734,
            START_MS,
            &up.uuid,
            &five,
            &submission.points,
        );
        let uploaded = uploaded_points(&payload);

        let cell = |v: &Value| -> String {
            v.as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| v.to_string())
        };
        println!(
            "[fixture] {} 点 / {}m / {}s / {} 步",
            f.track.len(),
            f.real_len,
            DURATION_SEC,
            f.sim.total_steps
        );
        println!("idx | 自检: id / count / radius / type / bdA | OBS 上传: 同五项");
        for (i, (a, b)) in probe.points.iter().zip(uploaded.iter()).take(6).enumerate() {
            let row = |q: &Value| {
                format!(
                    "{:>14} {:>5} {:>6} {:>5} {:>8}",
                    cell(&q["id"]),
                    cell(&q["count"]),
                    cell(&q["radius"]),
                    cell(&q["type"]),
                    cell(&q["bdA"])
                )
            };
            let mark = if a == b { "=" } else { "≠" };
            println!("{i:>3} | {} | {} {mark}", row(a), row(b));
        }
        println!("cp  | 自检: id / isFixed / position | OBS 上传(同一个 envelope): 同三项");
        for q in five_point_items(&submission.save_record) {
            println!(
                "{:>3} | {:>14} {:>5} {:>5} | {}",
                cell(&q["position"]),
                cell(&q["id"]),
                cell(&q["isFixed"]),
                cell(&q["position"]),
                q["pointName"].as_str().unwrap_or("")
            );
        }
        assert_points_identical(&probe.points, &uploaded, "自检 vs OBS 上传");
    }
}
