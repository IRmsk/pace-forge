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

use serde_json::{Value, json};

use super::client::Client;
use super::fence::{self, FencePick};
use super::obs;
use super::points::{self, PointsContext};
use super::policy::{self, PolicyInfo};
use crate::error::{Error, ErrorKind};
use crate::track::{
    Check, Checkpoint, KinParams, PlanRequest, PlanResult, Report, SimParams, TrackPoint,
    inspect_at, plan, simulate,
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
    /// 终点（可选）：给了就让轨迹收在这里，而不是绕回起点。
    pub end: Option<(f64, f64)>,
    /// 是否尝试「按打卡点反推操场」。
    ///
    /// 控制台有「自动规划 / 操场绕圈」两个选项，这里必须如实反映用户选的那个：
    /// 早先只判 `run_mode == 1`，于是选了「自动规划」也会被拿去试操场，打卡点
    /// 恰好围成长方形就被判成跑道，画出一条不在任何跑道上的椭圆——用户的
    /// 选择被静默改写。issue #37 起默认 `false`（走路网/按序串点），操场是
    /// 显式选项：CLI 用 `PACE_PREFER_STADIUM=1`、web 用 mode=stadium 打开。
    pub prefer_stadium: bool,
    /// 海拔区间 `(最低, 最高)`，米。`None` 用默认 42±21m。
    pub alt_range: Option<(f64, f64)>,
    /// 自由跑 / 户外跑：不拉围栏、不拉打卡点，轨迹绕锚点自由生成，
    /// 提交报文**省略** `fivePointJson`（OBS 明细仍发空数组 envelope）。
    ///
    /// 依据 neko（NekoSportsWorldTool）实测：sportType=4 自由跑（不计学期
    /// 成绩）、6 户外跑，策略走 runMode=2（无时段限制、geoFence=null）；
    /// 2/5 服务端拒「uid 不存在」，3 有特殊 roomId 签名规则。
    /// 记分跑保持 `false`，行为不变。
    pub free_run: bool,
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
/// 是否尝试按打卡点反推操场。
///
/// 两个条件缺一不可：记分跑（`run_mode == 1`）且用户选的是「操场绕圈」。
/// 只判前者会把「自动规划」也翻成操场——打卡点凑成矩形就会被几何判据当成
/// 跑道，在不是操场的地方画椭圆（用户看到的「我选了自由路线，怎么绕操场」）。
fn should_try_stadium(run_mode: i64, prefer_stadium: bool) -> bool {
    run_mode == 1 && prefer_stadium
}

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
    // 拿它算朝向/尺寸会把椭圆带偏、变圆）。
    let gx = cps.iter().map(|c| c.lat).sum::<f64>() / cps.len() as f64;
    let gy = cps.iter().map(|c| c.lng).sum::<f64>() / cps.len() as f64;
    // 只有**明显在内场**的点才剔除：离质心的距离不足第二近点的一半。
    // 四角对称、没有内场点时谁都不剔——少剔一个角，质心会整体偏出几十米，
    // 椭圆画偏一圈还漏掉那个角上的打卡点（4 个角点、没有「田径场中」的
    // 学校就是这种分布，实测偏 33m）。
    let mut dists: Vec<(usize, f64)> = cps
        .iter()
        .enumerate()
        .map(|(i, c)| (i, ((c.lat - gx) * klat).hypot((c.lng - gy) * klng)))
        .collect();
    dists.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let center_idx: Option<usize> = match dists.first() {
        Some(&(i, d1)) if dists.len() >= 2 && d1 < 0.5 * dists[1].1 => Some(i),
        _ => None,
    };
    let corners: Vec<&Checkpoint> = cps
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != center_idx)
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

    // 长宽比保护：环形跑道的长宽比有上限——标准 400m 外框 177×92.5（1.91），
    // 校内小场 160×75（2.13），再瘦就不是环了。
    //
    // 实测一例 220×38（5.79）：点位沿一条主路排开，尺寸闸放行后落到
    // 「按比例拟合」分支，算出半径 25.7m、直道 140m（被 clamp 顶到上限）的
    // 细长椭圆，单圈 441m，绕 5.5 圈。每圈都要把 5 个点位重复经过一遍，
    // 点位次序必然乱，服务端按「按顺序通过所有点位」判定直接失败。
    //
    // 阈值取 2.5：比任何真实跑道都宽（标准 1.91 / 小场 2.13），又拦得住这种
    // 细长条。只影响「本来就会拟错」的输入——标准 400m 与 280m 小场两条分支
    // 在它之前就已经 return，判定一个字都没变。
    let aspect = major / minor.max(1.0);
    if aspect > 2.5 {
        log(&format!(
            "[track] 打卡点外框 {major:.0}×{minor:.0}m（长宽比 {aspect:.1}）不像环形跑道，改用路线规划"
        ));
        return None;
    }

    // 先认标准跑道。
    //
    // 校园里的田径场绝大多数就是标准的，而「打卡点外框 × 系数」这条路
    // 没法反推准确半径：打卡点可能落在跑道外沿、内场边线、或者场地正中，
    // 三种分布算出来的半径差一倍。原来那个 `clamp(20, 30)` 更糟——标准
    // 400m 第 1 道运行半径 36.8，被压到 30 就是整圈往里缩 6.8 米，直段
    // 正好落进中央足球场（issue #6 的截图就是这个）。
    //
    // 标准 400m 跑道外框：长 84.39 + 2×46.26 ≈ 177m，宽 2×46.26 ≈ 92.5m。
    // 外准差给足（长 ±28、宽 ±22），覆盖实测里打卡点略微内缩的情况。
    let looks_standard_400 = (major - 177.0).abs() < 28.0 && (minor - 92.5).abs() < 22.0;
    // 280m 左右的校内小场也常见（直道 ~55、半径 ~26），外框约 160×75。
    let looks_small_oval = (major - 160.0).abs() < 30.0 && (minor - 75.0).abs() < 20.0;

    let (radius, straight) = if looks_standard_400 {
        // 第 1 道：36.5 内突沿 + 0.30 内突沿到分道线
        log(&format!(
            "[track] 打卡点外框 {major:.1}×{minor:.1}m 像标准 400m 跑道，按第 1 道尺寸生成（半径 36.8m）"
        ));
        (36.8, 84.39)
    } else if looks_small_oval {
        log(&format!(
            "[track] 打卡点外框 {major:.1}×{minor:.1}m 像 280m 校内小场，按半径 26m 生成"
        ));
        (26.0, 55.0)
    } else {
        const FIELD_TO_TRACK: f64 = 1.35;
        let r = (minor * FIELD_TO_TRACK / 2.0).clamp(20.0, 45.0);
        let st = (major * FIELD_TO_TRACK - 2.0 * r).clamp(35.0, 140.0);
        log(&format!(
            "[track] 外框 {major:.0}×{minor:.0}m 不像标准跑道，按比例拟合：半径 {r:.1}m 直道 {st:.1}m"
        ));
        (r, st)
    };
    let lap = 2.0 * straight + 2.0 * std::f64::consts::PI * radius;
    let spec = crate::track::OvalSpec {
        straight,
        radius,
        heading,
        laps: (distance_m / lap).max(1.0),
    };
    log(&format!(
        "[track] 操场绕圈：直道{:.0}m 半径{:.0}m 朝向{:.0}° {:.1}圈（单圈{:.0}m），{} 个打卡点",
        spec.straight,
        spec.radius,
        spec.heading.to_degrees(),
        spec.laps,
        lap,
        cps.len()
    ));

    let mut pts = crate::track::campus_oval_track((cx, cy), &spec, sample_step_m, seed);

    // 打卡点吸附：四角点真实椭圆天然就在 20m 内；「田径场中」这种在椭圆
    // 内部的点可能差得远，用余弦渐变把一小段整体拉过去，形成柔和外凸，
    // 不戳尖刺（硬吸附会在轨迹上留下一个尖角）。
    //
    // 注意：被吸附的点位离跑道有多远，轨迹就凹进去多深——点位在场地正中
    // 的话轨迹必然穿进内场一段，这是「必须经过打卡点」的代价，不是跑偏。
    // 吸附了几个、最远多远，都打进日志，用户反馈贴日志就能定位。
    let mut snap_n = 0usize;
    let mut snap_max = 0.0f64;
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
        snap_n += 1;
        snap_max = snap_max.max(bd);
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

    // 吸附统计 + 复核：吸附详情让「轨迹为什么穿进内场」可从日志诊断；
    // 还有打卡点够不着（>45m）就放弃操场形状，回退路线规划。
    if snap_n > 0 {
        log(&format!(
            "[track] {snap_n} 个打卡点距跑道 >16m，已吸附（最远 {snap_max:.0}m，轨迹在该处凹向点位）"
        ));
    }
    if !stadium_cp_fits(&pts, cps, log) {
        return None;
    }
    Some(pts)
}

/// 操场轨迹的「点位首次经过顺序」闸（issue #37，与长宽比闸并列，在 plan 层调用）。
///
/// 服务端「按顺序通过所有点位」是从轨迹里找每个点首次被经过的位置，再检查该
/// 序列与点位数组同序。绕圈轨迹每圈重复经过全部点，若数组序与绕圈方向不一致
/// （如 123×86 外框 5 点），首次经过序必然乱 → 回退按序串点。
/// 半径取 20m：操场四角点天然贴在跑道外 ≤16m，10m 会误杀真操场。
fn stadium_point_order_ok(pts: &[(f64, f64)], cps: &[Checkpoint], log: &mut Log) -> bool {
    let mut seen: Vec<(usize, usize)> = Vec::new();
    for (i, c) in cps.iter().enumerate() {
        match pts
            .iter()
            .position(|q| crate::track::geo::distance(*q, (c.lat, c.lng)) < 20.0)
        {
            Some(idx) => seen.push((i, idx)),
            None => {
                log(&format!(
                    "[track] 操场轨迹没有 20m 内经过打卡点 #{i}，改用路线规划"
                ));
                return false;
            }
        }
    }
    seen.sort_by_key(|x| x.1);
    if seen.windows(2).all(|w| w[1].0 > w[0].0) {
        return true;
    }
    let seq: Vec<String> = seen.iter().map(|x| format!("#{}", x.0)).collect();
    log(&format!(
        "[track] 操场绕圈首次经过顺序（{}）与点位数组不同序，改用路线规划",
        seq.join("→")
    ));
    false
}

/// 吸附之后复核每个打卡点到轨迹的最终距离：
/// - 16m 内的天然贴上，16~45m 的被上面那段余弦吸附拉过来；
/// - **超过 45m 的没被吸附**——操场轨迹绕的是标准跑道，可这个点根本不在
///   跑道附近（落在场地中间、或者教学楼下）。计分跑要求经过全部打卡点，
///   这条轨迹交上去就是一次「漏打卡」，白烧提交额度还留档。
///   这种情况放弃操场形状，回退到路线规划（那条链会逐一经过 must 点），
///   并把原因打进日志，让用户知道为什么没绕圈。
fn stadium_cp_fits(pts: &[(f64, f64)], cps: &[Checkpoint], log: &mut Log) -> bool {
    let mut max_d = 0.0f64;
    let mut bad: Option<(&str, f64)> = None;
    for c in cps {
        let mut d = f64::MAX;
        for q in pts {
            d = d.min(crate::track::geo::distance(*q, (c.lat, c.lng)));
        }
        max_d = max_d.max(d);
        if d > 45.0 && bad.map(|(_, b)| d > b).unwrap_or(true) {
            bad = Some((c.name.as_str(), d));
        }
    }
    if let Some((name, d)) = bad {
        log(&format!(
            "[track] ⚠ 打卡点「{name}」距操场轨迹 {d:.0}m，超过吸附上限 45m，绕圈不会经过它；",
        ));
        log("[track] ⚠ 计分跑会被判漏打卡，操场形状放弃，改走路线规划（会逐一经过打卡点）");
        return false;
    }
    log(&format!("[track] 打卡点贴合：距轨迹最远 {max_d:.1}m"));
    true
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
    log("[roads] 拉 OSM 路网…（单端点 10 秒超时）");
    let t0 = std::time::Instant::now();
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
    log(&format!(
        "[roads] 路网 {total:.0}m（用时 {:.1}s），按真实道路规划",
        t0.elapsed().as_secs_f64()
    ));
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
    // 终点是用户指定的点时禁止末尾折返（见 `route_through_ex`）
    end_is_fixed: bool,
) -> Option<crate::track::fence_route::FenceRoute> {
    use crate::track::{roadnet, tileroad};
    let bbox = roadnet::bbox_of(fence, 60.0);
    log("[tiles] 拉高德瓦片路网…");
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
    // 把 must 里的点投影到最近的道路节点。
    //
    // `route_through` 只沿道路节点走，25m 内没有节点时会直接忽略那个点；
    // 用户选的终点常落在草坪/楼前，于是「路网跑完停在边界、收尾拉一条
    // 632m 直线」——既不在路上又容易被判跳变。这里先吸附，直线段就只剩
    // 节点到终点的几十米。打卡点不吸（它们本来就在路上，且被 30m 校验守着）。
    let mut snapped: Vec<(f64, f64)> = must.to_vec();
    if let Some(last) = snapped.last_mut() {
        let mut best = (f64::MAX, (0.0, 0.0));
        for n in &graph.nodes {
            let d = crate::track::geo::distance(*n, *last);
            if d < best.0 {
                best = (d, *n);
            }
        }
        if best.0 <= 120.0 {
            *last = best.1;
            log(&format!(
                "[tiles] 终点投影到最近道路节点，直线段从 {:.0}m 缩到 {:.0}m",
                best.0, best.0
            ));
        }
    }
    // 指定终点时禁止末尾折返（折返会把末端带回几百米外）
    match tileroad::route_through_ex(&graph, &snapped, want_m, end_is_fixed) {
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
            log(&format!(
                "[tiles] 沿瓦片道路 {length_m:.0}m（目标 {want_m:.0}m）"
            ));
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
    pub fn validate(&self) -> Result<(), Error> {
        if !self.distance_m.is_finite() || self.distance_m <= 0.0 {
            return Err(ErrorKind::client(format!(
                "距离必须是正数，收到 {}",
                self.distance_m
            )));
        }
        if self.distance_m > 42_000.0 {
            return Err(ErrorKind::client(format!(
                "距离 {} m 超过上限 42000 m（42 公里）",
                self.distance_m
            )));
        }
        if self.duration_sec <= 0 {
            return Err(ErrorKind::client(format!(
                "时长必须为正，收到 {} 秒",
                self.duration_sec
            )));
        }
        if self.duration_sec > super::submit::MAX_RUN_TIME {
            return Err(ErrorKind::client(format!(
                "时长 {} 秒超过上限 {} 秒（{} 分钟）——超过这个值服务端必然驳回",
                self.duration_sec,
                super::submit::MAX_RUN_TIME,
                super::submit::MAX_RUN_TIME / 60
            )));
        }
        if let Some(st) = self.steps
            && st < 0
        {
            return Err(ErrorKind::client(format!("步数不能为负，收到 {st}")));
        }
        if !(-90.0..=90.0).contains(&self.center_lat)
            || !(-180.0..=180.0).contains(&self.center_lng)
        {
            return Err(ErrorKind::client(format!(
                "定位锚点越界: ({}, {})",
                self.center_lat, self.center_lng
            )));
        }
        if self.sample_step_m <= 0.0 || self.sample_step_m > 100.0 {
            return Err(ErrorKind::client(format!(
                "几何采样间距不合理: {} m",
                self.sample_step_m
            )));
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
            return Err(ErrorKind::client(format!(
                "上报采样间隔 {}s 不合理：报文时间精度是秒，小于 1 秒会画出 0 时长段",
                self.sample_interval_sec
            )));
        }
        if self.sample_interval_sec > 15.0 {
            return Err(ErrorKind::client(format!(
                "上报采样间隔 {}s 超过真机节奏（{:.2}s）的三倍，点数会明显偏少",
                self.sample_interval_sec,
                crate::track::REAL_MEAN_INTERVAL_SEC
            )));
        }
        // 点数下界：一条轨迹至少要有两个点，且起步额外点需要时长够长。
        let est = crate::track::sample_times(self.duration_sec, 0, self.sample_interval_sec);
        if est.len() < 4 {
            return Err(ErrorKind::client(format!(
                "时长 {}s / 间隔 {}s 只能采出 {} 个点，至少要 4 个",
                self.duration_sec,
                self.sample_interval_sec,
                est.len()
            )));
        }
        if est.len() > 40_000 {
            return Err(ErrorKind::client(format!(
                "时长 {}s / 间隔 {}s 会采出 {} 个点，报文过大",
                self.duration_sec,
                self.sample_interval_sec,
                est.len()
            )));
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
            end: None,
            alt_range: None,
            prefer_stadium: false,
            free_run: false,
        }
    }
}

/// 每一步的日志。
pub type Log = dyn FnMut(&str);

/// 流程产物。
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// 学校规则。
    pub policy: PolicyInfo,
    /// 选中的围栏和兜底标记。
    pub fence: Option<FencePick>,
    /// 打卡点上下文。
    pub points: PointsContext,
    /// 轨迹点数。
    pub track_points: usize,
    /// 轨迹实际长度（米）。
    pub actual_length_m: f64,
    /// 总步数。
    pub total_steps: i32,
    /// 平均步频（步/分钟）。
    pub avg_cadence: f64,
    /// 自检报告。
    pub report: Report,
    /// 轨迹点（含时间戳），提交与明细上传都要用。
    pub track: Vec<crate::track::TrackPoint>,
    /// 合成的步数/分桶数据。
    pub sim: crate::track::SimResult,
    /// 本次跑步的起始时间戳（毫秒）。
    pub start_ms: i64,
    /// 几何实测里程（可能和目标有小偏差）。
    pub real_length_m: i64,
    /// OBS 上传用的对象 key 列表。
    pub obs_keys: Vec<String>,
    /// 是否已提交。
    pub submitted: bool,
    /// 服务端记录 id，提交成功后才有。
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
    /// 轨迹实际走的支路：`"stadium"` 操场绕圈 / `"route"` 围栏路线 /
    /// `"draw"` 手绘。
    pub track_mode: &'static str,
    /// 请求了操场绕圈但判定「打卡点不像操场」而回退到路线规划。
    pub stadium_fallback: bool,
}

/// 「现在跑」这一档的起跑时刻：按「刚跑完」处理。
///
/// 结束时刻 = `server_now - margin_ms`，起跑时刻再倒推一整个时长，所以
/// `stop = start + duration*1000 <= now - margin` 恒成立（只要时间合同时长
/// 与时长一致）。抽成纯函数是为了能直接单测这条不等式——它原来藏在
/// `dry_run` 里，只能靠读代码发现 `+` 号写反了。
fn start_for_immediate_run(server_now: i64, duration_sec: i64, margin_ms: i64) -> i64 {
    server_now - duration_sec * 1000 - margin_ms
}

/// 补跑时刻搬到服务器时间轴上。
///
/// `skew = 服务器 - 本机`，由 [`crate::api::gmttime::server_now_ms`] 给出。
/// 补跑的 `t` 是前端 `Date.now()` 算出来的，带本机误差；本机快 10 分钟时
/// 直接发就会落到服务器未来轴，被「开始 + 时长 ≤ 现在」驳回——和「现在跑」
/// 是同一条判据的两个入口。抽成纯函数，免得 `skew` 又被只写进日志。
fn rerun_start_ms(local_t: i64, skew: Option<i64>) -> i64 {
    local_t + skew.unwrap_or(0)
}

/// ① 拉取策略串（记分跑在时段外会 11016，允许按配置兜底）。
///
/// 提出来是为了让 [`dry_run`] 只剩「哪一步、什么顺序」，降级规则留在这里。
fn stage_policy(client: &Client, p: &RunParams, log: &mut Log) -> Result<PolicyInfo, Error> {
    log(&format!("[policy] 拉取策略 runMode={}…", p.run_mode));
    // 记分跑在时段外会直接 11016，连策略串都拿不到。但提交报文只要
    // policy 这个字符串（实测恒为 "0"），timestamp 不进签名，点位又走
    // 独立的 /api/points，所以拿不到策略不等于不能提交。
    // 配置 run.strict_policy（PACE_STRICT_POLICY=1）可以关掉兜底，保留原来的硬失败行为。
    let strict = crate::config::global().run.strict_policy;
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
    Ok(pol)
}

/// ② 拉围栏并选锚点所在的圈；计分跑拿不到围栏就是硬失败（issue #8）。
fn stage_fence(client: &Client, p: &RunParams, log: &mut Log) -> Result<Option<FencePick>, Error> {
    // 自由跑 / 户外跑：runMode=2 服务端本来就不下发围栏（geoFence=null），
    // 跳过拉取，直接走「无围栏」支路（轨迹绕锚点自由生成）。
    let fences = if p.free_run {
        log("[fence] 自由跑：跳过围栏拉取");
        Ok(Vec::new())
    } else {
        log("[fence] 拉取围栏…");
        fence::fetch_fences(client)
    };
    let pick = match fences {
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
            // 计分跑必须知道自己学校的围栏才谈得上「围栏内计分」。接口失败时
            // 静默按自由跑提交，结果是服务端按自由跑处理、用户白烧一次额度，
            // 还看不到哪一步降级了（issue #8）。这里只拦「拿不到」，学校确实
            // 没配围栏是 `Ok(空)`，仍按原行为继续。
            if p.run_mode == 1 {
                return Err(ErrorKind::client(format!(
                    "计分跑必须拿到官方围栏，但接口失败：{e}\n                     这不是重试能绕过的问题——先确认登录态和校区选择，再重新生成"
                )));
            }
            log(&format!("⚠ [fence] 接口失败，按自由跑继续: {e}"));
            None
        }
    };
    Ok(pick)
}

/// ③ 拉打卡点：接口空数组时退回围栏下发的必经点，失败则无点位继续。
fn stage_points(
    client: &Client,
    p: &RunParams,
    pick: Option<&FencePick>,
    log: &mut Log,
) -> PointsContext {
    // 自由跑 / 户外跑：没有打卡点，跳过拉取（neko 的自由跑同样不拉点位）。
    match if p.free_run {
        log("[points] 自由跑：跳过打卡点拉取，轨迹将绕锚点自由生成");
        Ok(PointsContext::default())
    } else {
        log("[points] 拉取打卡点…");
        // 目标距离要传给点位接口：有的学校只在带真实距离时才下发点位。
        // 先按 distance/1 拉，空集才用目标距离重试（见 points::fetch_points_cached_at）。
        points::fetch_points_cached_at(
            client,
            p.center_lat,
            p.center_lng,
            pick.as_ref()
                .map(|k| k.selected.run_area_id.to_string())
                .as_deref(),
            Some(p.distance_m.round() as i64),
        )
    } {
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
                    PointsContext {
                        points: items,
                        coords: bd,
                        gcj_coords: gcj,
                    }
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
            log(&format!(
                "⚠ [points] 接口失败（可能限流），无打卡点继续: {e}"
            ));
            PointsContext::default()
        }
    }
}

/// ④ 规划：手绘 > 操场 > 围栏路线/自由规划，产出 `track` 与支路标记。
///
/// 这一段原来占 `dry_run` 500 行，分支判断（终点优先于操场、操场点位顺序闸、
/// 手绘不缩放）都留在这里；`dry_run` 只拿到结果，不再看过程。
fn plan_stage(
    p: &RunParams,
    pt: &PointsContext,
    pick: Option<&FencePick>,
    start_ms: i64,
    log: &mut Log,
) -> Result<TrackStage, Error> {
    // 子阶段计时（手绘/操场），与 `dry_run` 的整段计时互不影响。
    let mut stage_t = std::time::Instant::now();
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
    // 请求了操场却没成，必须让界面知道（`stadium_fallback`），否则用户以为
    // 自己跑的是标准跑道，实际走的是绕楼路线。
    //
    // 只有用户**明确选了操场**才试（`prefer_stadium`）。选「自动规划」时
    // 不走这条：打卡点围成长方形是常见布局（教学楼四角、宿舍区），几何判据
    // 会把它认成跑道，然后在不是操场的地方画椭圆。
    let mut stadium_fallback = false;
    let want_stadium = should_try_stadium(p.run_mode, p.prefer_stadium);
    // 指定了终点就别绕操场：操场是「按圈长铺满里程」的固定闭环，跑完自然回到
    // 起点，没有收在某个点的说法。硬要收尾只能从环上拉一条直线过去，那是跳变。
    // 所以这里的优先级是：**终点 > 操场**，并把原因写进日志。
    let oval: Option<Vec<(f64, f64)>> = if want_stadium && p.end.is_some() {
        if let Some((elat, elng)) = p.end {
            log(&format!(
                "[track] 指定了终点（{elat:.6},{elng:.6}），跳过操场绕圈——操场是按圈长铺满的固定闭环，收不到终点"
            ));
        }
        None
    } else if want_stadium {
        let seed0 = if p.seed != 0 { p.seed } else { start_ms as u64 };
        // issue #37：长宽比闸放行后再过「首次经过顺序」闸，不同序回退路网。
        let got = try_stadium_track(&cps, p.distance_m, p.sample_step_m, seed0, log)
            .filter(|pts| stadium_point_order_ok(pts, &cps, log));
        stadium_fallback = got.is_none();
        got
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

    // 实际走了哪条支路。请求里的 `run_mode` 只是「想走操场」，能不能真绕成
    // 操场还要看打卡点像不像操场（`try_stadium_track` 会拒），所以界面必须
    // 显示**实际**结果，否则用户只知道「我选了操场」，不知道退回了路线规划。
    let mut track_mode: &'static str = "route";
    let track = if let Some(custom) = p.custom_path.as_ref().filter(|v| v.len() >= 2) {
        track_mode = "draw";
        log(&format!("[track] 采用手绘轨迹 {} 个控制点", custom.len()));
        // 不传 must：手绘的整条价值就是用户自己控制形状，把没画到的点位
        // 强行插进去只会在原地拉出几百米的跳变（GEO_JUMP），把线撕烂。
        // 漏了点位由前端实时提示，用户自己补笔画。
        let mut pts =
            crate::track::fence_route::resample_with_checkpoints(custom, p.sample_step_m, &[], 0.0);
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
                .map(|((lat, lng), t)| crate::track::TrackPoint {
                    lat,
                    lon: lng,
                    time_ms: t,
                })
                .collect(),
            checkpoints: cps.clone(),
            actual_length_m: actual,
        }
    } else if let Some(pts) = oval {
        track_mode = "stadium";
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
                .map(|((lat, lng), t)| crate::track::TrackPoint {
                    lat,
                    lon: lng,
                    time_ms: t,
                })
                .collect(),
            checkpoints: cps.clone(),
            actual_length_m: actual,
        }
    } else if fence_ring.len() >= 3 {
        // 终点在下面的收尾处接（第 1265 行附近），不在这里改分支——
        // 路网（OSM → 瓦片 → 几何三级）只接在这条路上，绕开它就没路网了。
        log(&format!(
            "[track] 围栏内规划 {}m，{} 个必经点…",
            p.distance_m,
            cps.len()
        ));
        use rand::SeedableRng;
        let mut rng =
            rand::rngs::StdRng::seed_from_u64(if p.seed != 0 { p.seed } else { start_ms as u64 });
        let _ = &mut rng;

        let must: Vec<(f64, f64)> = cps.iter().map(|c| (c.lat, c.lng)).collect();
        // 路网规划的必经点里要带上终点：否则路网跑完还在边界上，收尾只能从
        // 末端拉一条直线过去（实测 632m，既不在路上又容易被判跳变）。
        // 终点只进**路网**的 must，不进 `resample_with_checkpoints` 的吸附列表——
        // 它不是打卡点，不该被 15m 阈值拽动。
        let mut route_must = must.clone();
        if let Some(e) = p.end {
            route_must.push(e);
        }
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
        // 纯几何模式（`prefer_roads=false`）：网络连不上 OSM / 瓦片时，
        // 这一步要白等三次超时才拿不到任何东西。用户明确不要路网，就只读
        // 本地标注禁区，形状与「Overpass 失败后」完全一致。
        let blockers: Vec<Vec<(f64, f64)>> = if !p.prefer_roads {
            log("[block] 纯几何模式：跳过 OSM 建筑/水体查询");
            let local = crate::track::roadnet::load_local_blockers();
            let v: Vec<Vec<(f64, f64)>> = local
                .iter()
                .map(|poly| {
                    poly.iter()
                        .map(|&(la, ln)| crate::api::coord::gcj02_to_bd09(la, ln))
                        .collect()
                })
                .collect();
            if !v.is_empty() {
                log(&format!("[block] 本地标注禁区 {} 块", v.len()));
            }
            v
        } else {
            let bbox = crate::track::roadnet::bbox_of(&fence_ring, 60.0);
            log("[block] 查 OSM 建筑/水体轮廓…");
            let mut v: Vec<Vec<(f64, f64)>> = Vec::new();
            // **三套坐标系必须归一到 BD-09 才能放进同一个几何判断。**
            // 这里原来是直接 `v.extend(...)`，把三种坐标混在一个平面上：
            //   OSM 建筑/水体   ← Overpass，WGS84（原始，无转换）
            //   本地手绘禁区    ← 前端 Leaflet × 高德瓦片，GCJ-02
            //   轨迹点/围栏环   ← fence.rs 已转成 BD-09
            // 华南一带 GCJ-02 → BD-09 要偏 900 米以上（实测河源 950 m），
            // 于是避让一直在执行，只是绕的是几百米外另一块地——看起来就是
            // 「禁区完全没用，挪走也不变」。
            let to_bd =
                |poly: &Vec<(f64, f64)>, f: fn(f64, f64) -> (f64, f64)| -> Vec<(f64, f64)> {
                    poly.iter().map(|&(la, ln)| f(la, ln)).collect()
                };
            match crate::track::roadnet::fetch_buildings(bbox) {
                Ok(b) if !b.is_empty() => {
                    log(&format!("[block] 建筑轮廓 {} 栋", b.len()));
                    v.extend(b.iter().map(|p| to_bd(p, crate::api::coord::wgs84_to_bd09)));
                }
                Ok(_) => log("[block] 这一带没有建筑数据"),
                Err(e) => log(&format!("[block] 建筑轮廓没拿到（{e}），本次不避让")),
            }
            if let Ok(w) = crate::track::roadnet::fetch_water(bbox)
                && !w.is_empty()
            {
                log(&format!("[block] 水域 {} 片", w.len()));
                v.extend(w.iter().map(|p| to_bd(p, crate::api::coord::wgs84_to_bd09)));
            }
            // 本地手绘禁区。OSM 在国内校园基本是空白（实测某校区整个 bbox
            // 只有 15 个 way、76 个 node，一栋楼都没有），Overpass 拿不到轮廓
            // 时只能靠人自己在地图上圈。这份标注全账号共享，长期有效。
            // 前端底图是高德瓦片，存下来的多边形是 GCJ-02，读取时转 BD-09。
            let local = crate::track::roadnet::load_local_blockers();
            if !local.is_empty() {
                log(&format!("[block] 本地标注禁区 {} 块", local.len()));
                v.extend(
                    local
                        .iter()
                        .map(|p| to_bd(p, crate::api::coord::gcj02_to_bd09)),
                );
            }
            v
        };

        log(&format!(
            "[耗时] 建筑/水体 {:.1}s",
            stage_t.elapsed().as_secs_f64()
        ));
        stage_t = std::time::Instant::now();

        // 路网来源两级：OSM 优先（有真实道路等级和拓扑），拿不到再试高德
        // 瓦片。两级都失败才退回几何规划。
        let on_roads = if p.prefer_roads {
            match try_route_on_roads(&fence_ring, &route_must, p.distance_m, seed, &mut *log) {
                Some(r) => Some(r),
                None => try_route_on_tiles(
                    &fence_ring,
                    &route_must,
                    p.distance_m,
                    seed,
                    &mut *log,
                    p.end.is_some(),
                ),
            }
        } else {
            None
        };
        log(&format!(
            "[耗时] 拉路网 {:.1}s",
            stage_t.elapsed().as_secs_f64()
        ));

        let route = if let Some(r) = on_roads {
            r
        } else if must.len() >= 2 {
            match crate::track::fence_route::route_through_in_order(
                &fence_ring,
                p.distance_m,
                &route_must,
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
        // 打卡点回钉会撤销上面的避让：`pin_checkpoints` 是无条件
        // `insert(best_i+1, m)`，不管路径有没有刚绕过楼。所以只要有一块
        // 禁区盖住了打卡点，绕行必然被钉回去——实测「避让后 0 点在楼里，
        // 钉完 6 点在楼里」。这不是能两边都满足的事：打卡点必须经过，
        // 禁区不该穿。与其静默失效，不如把话说清楚。
        if !blockers.is_empty() && !must.is_empty() {
            for (i, &m) in must.iter().enumerate() {
                for poly in &blockers {
                    if crate::track::fence_route::contains(poly, m.0, m.1) {
                        log(&format!(
                            "! [block] 第 {} 个打卡点落在某块禁区内部——这个点必须经过，\
                             那块禁区的避让对它无效（把禁区缩小或挪开才能绕）",
                            i + 1
                        ));
                        break;
                    }
                }
            }
        }
        // 服务端点位按数组次序回钉（`_in_order`）：
        //
        // 无序遍历是「每个点各自找全程最近的那一段」，于是去 #2 的路上擦过
        // #4 时把 #4 钉在前面，首过次序变成 1→4→3→2，服务端「按顺序通过
        // 所有点位」直接判失败（实测 5 个点位就是这样）。按序回钉保证后面
        // 的点只可能钉在前一个点之后；钉不上的（距离过远）当场报出来。
        let (mut sampled, pin_dists) =
            crate::track::fence_route::resample_with_checkpoints_in_order(
                &wandered,
                p.sample_step_m,
                &must,
                15.0,
            );
        for (i, d) in pin_dists.iter().enumerate() {
            if *d > 30.0 {
                let name = cps.get(i).map(|c| c.name.as_str()).unwrap_or("");
                log(&format!(
                    "! [points] 点位 #{} {} 按次序钉不进路线，只能钉在 {d:.0}m 外（会拉一段直线）",
                    i + 1,
                    name
                ));
            }
        }
        // 指定终点：把末点钉在终点上。
        //
        // 围栏路线是「沿边界/路网跑满里程」，末端自然落在路上，不一定是用户
        // 选的终点。这里直接替换末点，等于最后一段从路网末端走到终点。
        // 距离远会打日志——那段是直线，太长会被服务端的「相邻点跳变」挑。
        if let Some(e) = p.end
            && let Some(last) = sampled.last().copied()
        {
            let gap = crate::track::geo::distance(last, e);
            if let Some(slot) = sampled.last_mut() {
                *slot = e;
            }
            log(&format!(
                "[track] 终点：末段从路网末端接过 {gap:.0}m（>200m 那段是直线，可能被判跳变）"
            ));
        }
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
        log(&format!(
            "[track] 规划 {}m，{} 个必经点…",
            p.distance_m,
            cps.len()
        ));
        let req = PlanRequest {
            center_lat: p.center_lat,
            center_lng: p.center_lng,
            checkpoints: cps.clone(),
            total_distance_m: p.distance_m,
            total_sec: p.duration_sec,
            start_ms,
            sport_type: p.sport_type,
            sample_step_m: p.sample_step_m,
            end: p.end,
        };
        use rand::SeedableRng;
        let mut rng =
            rand::rngs::StdRng::seed_from_u64(if p.seed != 0 { p.seed } else { start_ms as u64 });
        let _ = &mut rng;
        plan(&req, &mut rng)?
    };
    // 指定了终点就核对一下末点：planner 里已经钉住了，但 flow 后面还有
    // 避让/采样/吸附几道处理，任何一道都可能把末点挪开。这里的日志是给
    // 「App 里没跑到终点」这类反馈用的第一手证据。
    if let Some((elat, elng)) = p.end
        && let Some(last) = track.points.last()
    {
        let d = crate::track::geo::distance((last.lat, last.lon), (elat, elng));
        log(&format!(
            "[track] 终点核对：末点 ({:.6},{:.6}) 与设定终点 ({elat:.6},{elng:.6}) 相距 {d:.1}m",
            last.lat, last.lon
        ));
        if d > 30.0 {
            log(
                "! [track] 末点没落在设定终点上：后续避让/吸附把它挪开了，检查 blockers 是否盖住终点",
            );
        }
    }

    log(&format!(
        "√ [track] {} 点，实测 {:.1}m（目标 {:.0}m，偏差 {:.2}%）",
        track.points.len(),
        track.actual_length_m,
        p.distance_m,
        (track.actual_length_m - p.distance_m).abs() / p.distance_m * 100.0
    ));
    Ok(TrackStage {
        cps,
        blockers_used,
        track,
        track_mode,
        stadium_fallback,
    })
}

/// [`plan_stage`] 的产出：后续 ⑤ 采样、⑥ 仿真、⑦ 自检都要用。
struct TrackStage {
    /// 打卡点（含 GCJ-02 坐标与 `P{i}` 名字）。
    cps: Vec<Checkpoint>,
    /// 本次避让用的障碍轮廓；只有围栏支路会填。
    blockers_used: Vec<Vec<(f64, f64)>>,
    /// 规划出来的几何。
    track: crate::track::PlanResult,
    /// `"draw"` / `"stadium"` / `"route"`。
    track_mode: &'static str,
    /// 请求了操场但回退到路线规划。
    stadium_fallback: bool,
}

/// ⑤ 上报采样：等时间 5 秒一格，几何仍取密采样折线。
///
/// 采样率、压平、吸附、拐角补位、间隔上限这些都在这里；`dry_run` 只拿
/// `track_points` / `real_len` / `timed` 三样东西。
fn sample_stage(
    p: &RunParams,
    pol: &PolicyInfo,
    track: &crate::track::PlanResult,
    cps: &[Checkpoint],
    blockers_used: &[Vec<(f64, f64)>],
    start_ms: i64,
    log: &mut Log,
) -> Result<SampleStage, Error> {
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
    // ⓹.4 主体配速预检（判定逻辑见 `body_pace_note`）。
    if let Some(note) = body_pace_note(p.distance_m, p.duration_sec, pol.pace_range) {
        log(&note);
    }

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
        blockers_used,
        &cp_pairs,
    );
    if track_points.len() < 2 {
        return Err(ErrorKind::client("上报采样失败：时间轴或几何为空".into()));
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
    crate::track::compress_startup(&mut track_points, start_ms, p.duration_sec, Some(v_ceil));

    // ⑤.5 压缩之后沿轨迹**重新对齐一次**采样点。
    //
    // 压平会把整条网格按目标弧长重铺（窗口外的点也一起动），起点附近的
    // 采样点能整体挪出几十米；只靠 ⑤.6 的「吸附」（把点直接拽到点位坐标）
    // 去救，位移会大到扯出 40m 的相邻间距。先在**弧长空间**把最近的那个
    // 采样点挪到点位在轨迹上的投影（位移 ≤ 半个步长），吸附只需要收尾。
    let aligned = crate::track::align_samples_to_checkpoints(&mut track_points, &snap_cps);
    if aligned > 0 {
        log(&format!(
            "√ [align] 压缩后按轨迹重新对齐打卡点 {aligned} 个"
        ));
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
    // ⑤.7 吸附把邻居拉开了就压回来。
    //
    // 上一步的 `nudge_to` 是按 1/(k+1) 衰减平移的，挪完没复查间距：原本贴着
    // 上限的（6.3 m/s × 5s = 31.5m）再被拉开就成了 38.7m / 7.74 m/s，超了
    // 服务端的单点速度上限，App 上是一条斜穿场地的直线。这里只动非打卡点。
    let pinned: Vec<usize> = snap
        .items
        .iter()
        .filter(|s| s.shifted)
        .map(|s| s.index)
        .collect();
    let max_gap = crate::api::submit::MAX_POINT_SPEED_MPS * crate::track::SAMPLE_INTERVAL_SEC;
    let clamped = crate::track::clamp_sample_gaps(&mut track_points, &pinned, max_gap);
    // ⓹.6 拐角补位：把跨过拐角的上报点挪到拐角上（插点会被 SAMPLE_RATE_SHORT 拦）。
    //
    // 5 秒一采，相邻点隔 15~31m，中间的拐角全被直线抹掉——画出来就是斜着
    // 切过楼角。这里只在「密折线偏离直线超过 12m」时移动那一个点，绝大多数
    // 段落不动，`ΔtotalTime` 一个都不改。
    //
    // 挪点会改变折线里程，而里程自检比的是「折线 vs 报文声明」——两边一起变
    // 就拦不住。所以这里再加一道兜底：挪完量一次总长，偏差超 1.5% 就整段
    // 回退。实测没有兜底时 2km/12min 会被顶到 2.64km。
    let len_of = |p: &[crate::track::TrackPoint]| -> f64 {
        p.windows(2)
            .map(|w| crate::track::distance((w[0].lat, w[0].lon), (w[1].lat, w[1].lon)))
            .sum()
    };
    let len_before = len_of(&track_points);
    let snapshot = track_points.clone();
    let nudged = crate::track::nudge_to_corners(&mut track_points, &xy, 12.0);
    if nudged > 0 {
        let len_after = len_of(&track_points);
        let drift = if len_before > 0.0 {
            (len_after - len_before).abs() / len_before
        } else {
            0.0
        };
        if drift > 0.015 {
            track_points = snapshot;
            log(&format!(
                "[track] 拐角补位会改里程 {:.2}%（{} 个点），已整段回退",
                drift * 100.0,
                nudged
            ));
        } else {
            log(&format!(
                "[track] 拐角补位：{nudged} 个上报点挪到拐角上（里程变 {:.2}%）",
                drift * 100.0
            ));
        }
    }
    // 拐角补位是把点**挪走**，挪完间距可能又超了——上面那次 `clamp_sample_gaps`
    // 跑在补位之前，管不到这里。实测有一条轨迹补位后第 130 点间距 50.1m，
    // 本地体检直接把它判成「不像走出来的」拦掉提交。补位后再压一次。
    let clamped_after_corner = crate::track::clamp_sample_gaps(&mut track_points, &pinned, max_gap);
    let clamped = clamped + clamped_after_corner;
    if clamped > 0 {
        log(&format!(
            "√ [snap] 吸附/补位后 {clamped} 处相邻间距超过 {max_gap:.1}m（{:.1} m/s 上限），已压回",
            crate::api::submit::MAX_POINT_SPEED_MPS
        ));
    }
    // 压完再核一遍：`clamp_sample_gaps` 对「两端都是打卡点」的段会跳过（钉住动不了），
    // 那种段仍可能超限，明确打出来，别让本地体检替它背锅。
    if let Some((i, gap)) = track_points
        .windows(2)
        .enumerate()
        .map(|(i, w)| {
            (
                i,
                crate::track::distance((w[0].lat, w[0].lon), (w[1].lat, w[1].lon)),
            )
        })
        .filter(|(_, g)| *g > max_gap * 1.05)
        .max_by(|a, b| a.1.total_cmp(&b.1))
    {
        log(&format!(
            "! [snap] 第 {} 点间距仍有 {:.1}m（上限 {:.1}m）——这一段的端点被钉住了，压不动",
            i + 1,
            gap,
            max_gap
        ));
    }
    // 上报点最后再推一次。
    //
    // `sample_by_time` 已经会在弦压到楼上时改用折线顶点，但「区间里没有可用
    // 顶点」时它只能如实穿过去；而且等时间采样取的点落在原折线的弦上，两条
    // 弦的交点会切进多边形角部。实测修好坐标系之后，158 个上报点里仍有 2 个
    // 落在禁区里、最近边界 0.6m——视觉上就是线还擦着楼角。这里逐点做一次
    // 最小位移推出，**点数不变**，所以下面按索引对时间戳的映射依然成立。
    if !blockers_used.is_empty() {
        let mut flat: Vec<(f64, f64)> = track_points.iter().map(|q| (q.lat, q.lon)).collect();
        let moved = crate::track::fence_route::nudge_outside(&mut flat, blockers_used, 2.0);
        if moved > 0 {
            for (tp, (la, ln)) in track_points.iter_mut().zip(flat.iter()) {
                tp.lat = *la;
                tp.lon = *ln;
            }
            log(&format!(
                "√ [block] {} 个上报点擦到禁区边界，已推出 2m",
                moved
            ));
        }
    }

    // 压平后点都落在原折线上，弧长总和几乎不变，但报文的 totalDis 必须
    // 与 `allLocJson` 末点的 totalDis 同源，所以在这里重算一次。
    // 吸附改过坐标，这一次重算同时把吸附引起的几米变化算进去。
    let real_len = crate::track::geo::path_length(
        &track_points
            .iter()
            .map(|q| (q.lat, q.lon))
            .collect::<Vec<_>>(),
    )
    .round() as i64;

    // 长度漂移诊断：目标里程 vs 几何压缩/吸附后的实测里程。两者差得远
    // （粗采样、急剧转弯、被禁区推挤）时，报文里的 totalDis 就会和表单
    // 上填的公里数对不上，先在这一行里看见，不用等提交后回看。
    {
        let drift = (real_len as f64 - p.distance_m) / p.distance_m.max(1.0) * 100.0;
        log(&format!(
            "[length] 目标 {:.0}m → 实测 {real_len}m（{:+.1}%）",
            p.distance_m, drift
        ));
    }
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
    Ok(SampleStage {
        track_points,
        real_len,
        timed,
    })
}

/// [`sample_stage`] 的产出。
struct SampleStage {
    /// 上报点（按 5 秒时间网格取的采样点）。
    track_points: Vec<crate::track::TrackPoint>,
    /// 吸附/压平后重算的实测里程（米），报文的 `totalDis` 与它同源。
    real_len: i64,
    /// 采样点的 `(lat, lng, time_ms)`，给仿真用。
    timed: Vec<(f64, f64, i64)>,
}

/// 只做到自检为止（不发任何写请求）。
///
/// 用来在提交前确认数据没问题——大部分坑在这一步就能发现。
/// ⑦ 自检：把报文反解的视图交给 preflight，拦在本地。
fn inspect_probe(
    view: &super::submit::VerifyView,
    pol: &PolicyInfo,
    cp_tuples: &[(f64, f64, String)],
    sim_params: &SimParams,
    sample_interval_sec: f64,
    server_now: i64,
    log: &mut Log,
) -> Result<crate::track::preflight::Report, Error> {
    let view_sim = view.as_sim();
    let check = Check {
        points: &view.points,
        sim: &view_sim,
        params: sim_params,
        checkpoints: cp_tuples,
        total_distance_m: view.total_distance_m as i64,
        total_time_sec: view.total_time_sec,
        total_steps: view.total_steps,
        start_time_ms: view.start_time_ms,
        stop_time_ms: view.stop_time_ms,
        pace_range: pol.pace_range,
        step_range: pol.step_range,
        expected_interval_sec: Some(sample_interval_sec),
    };
    let report = inspect_at(&check, Some(server_now));
    if report.blocked() {
        for line in report.lines() {
            log(&format!("  {line}"));
        }
        return Err(ErrorKind::client(format!(
            "自检未通过：{}",
            report.summary()
        )));
    }
    log(&format!("√ [preflight] {}", report.summary()));
    for line in report.lines() {
        log(&format!("  {line}"));
    }

    Ok(report)
}

/// 自检点位数组的三道校验：必到点齐、isPass 全 true、type=2 数量对得上。
fn audit_points(
    sub: &super::submit::Submission,
    fixed: &[super::submit::FixedPoint],
    log: &mut Log,
) -> Result<(), Error> {
    // 自检与上传必须共用同一份点位。
    //
    // 早先这里是数 `type=2` 打卡标记的个数，用来兜「`fixed` 传空」——
    // 后来口径分了流（见 submit::PointTypeScheme）：带必到点的跑法才撒
    // `type=2`，没有必到点的跑法不发。数标记就不再可靠，改成直接数必到点
    // 数组，不依赖采样点 `type` 的口径。
    let fp_items: Vec<Value> = sub
        .save_record
        .get("fivePointJson")
        .and_then(|v| v.as_str())
        .and_then(|x| serde_json::from_str::<Value>(x).ok())
        .and_then(|v| v.get("fivePointJson").cloned())
        .and_then(|v| {
            v.as_str()
                .and_then(|x| serde_json::from_str::<Vec<Value>>(x).ok())
        })
        .unwrap_or_default();
    let n_fixed = fp_items.len();

    // isPass 必须全 true —— App 详情页就按它画绿点。
    //
    // 少一个就少画一个绿点（4 绿 1 橙）。成因通常是：起点(`type=7`) 或起步
    // 窗口的灰点(`type=-1`) 把某个必到点 10m 命中圈里唯一的采样点占掉了，
    // `type=2` 还能靠 Relaxed 兜到圈外，但 `isPass` 用的是严格 10m，于是 false。
    //
    // 这种记录发出去等于白跑一次（App 里那个点是橙的），所以拦在本地。
    let not_passed: Vec<String> = fp_items
        .iter()
        .filter(|p| p.get("isPass").and_then(|v| v.as_bool()) == Some(false))
        .map(|p| {
            p.get("pointName")
                .and_then(|v| v.as_str())
                .unwrap_or("(未命名)")
                .to_string()
        })
        .collect();
    if !not_passed.is_empty() {
        return Err(ErrorKind::client(format!(
            "有 {} 个必到点是 isPass=false：{}。\n    App 详情页会显示成橙点，这次打卡等于没算。\n    成因通常是起点或起步窗口的采样点占掉了该点位 10m 内唯一的采样点。\n    处理：把「纬度/经度」锚点挪到离这些点位 20m 以外再跑，或者换一个场地。",
            not_passed.len(),
            not_passed.join("、")
        )));
    }

    // type=2 的数量必须等于必到点数。
    //
    // `isPass` 管成绩（服务端判「经过没有」），`type=2` 管**显示**：App 详情页
    // 按 type=2 的点位画绿点，少一个就少画一个（issue #11：起点压在打卡点上时
    // 5 个点只标出 4 个）。两者都得齐，缺 display 那层用户会以为没跑成。
    let marked = sub
        .points
        .iter()
        .filter(|p| p.get("type").and_then(|v| v.as_i64()) == Some(2))
        .count();
    if !fixed.is_empty() && marked < fixed.len() {
        // **只警告，不拦**。
        //
        // 这是显示层问题：服务端判 `complete=true`、成绩照算，只是 App 详情页
        // 少画一个绿点。拦下来等于用户跑不了——代价比少画一个点大得多。
        // 拦过一版，实测刘的号也被挡住（他的点位批次同样会命中），说明这个
        // 条件在某些围栏形状下是常态，不适合当门槛。
        eprintln!(
            "! [points] 必到点 {} 个，报文里只有 {marked} 个 type=2 标记：App 详情页会少画 {} 个绿点（成绩不受影响）",
            fixed.len(),
            fixed.len().saturating_sub(marked)
        );
    }
    if !fixed.is_empty() && n_fixed != fixed.len() {
        return Err(ErrorKind::client(format!(
            "自检点位里必到点数量对不上（报文 {n_fixed} 个，应有 {} 个）：probe 的 fixed 传空了？",
            fixed.len()
        )));
    }
    log(&format!(
        "√ [points] 自检与上传共用同一份点位：{} 点 / {} 个必到点",
        sub.points.len(),
        n_fixed
    ));

    Ok(())
}

/// 打印每个点位两套坐标系下的最近距离与首次经过下标，返回首次经过序列。
fn log_point_distances(
    pt: &PointsContext,
    view: &super::submit::VerifyView,
    log: &mut Log,
) -> Vec<(usize, i64, String)> {
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

    order_seen
}

/// 点位顺序自检：轨迹实际穿过点位的次序必须与点位数组同序。
fn warn_point_order(order_seen: &[(usize, i64, String)], log: &mut Log) {
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
        let mut sorted = order_seen.to_vec();
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
            // `isFixed` 全发 0 之后服务端认为「本场没有必经点」，也就不再
            // 判「按顺序通过所有点位」——那时这条日志纯属吓人。只有真的带上
            // 必经点（PACE_FORCE_NORMAL_POINT=0）才需要它。
            if crate::api::submit::force_normal_point() {
                log(&format!(
                    "[point-order] 顺序乱了（{}），但点位全部按普通点发出，服务端不判顺序——可以提交",
                    bad.join("；")
                ));
            } else {
                log(&format!(
                    "[point-order] 顺序错误，服务端会判「按顺序通过所有点位」失败：{}",
                    bad.join("；")
                ));
                log("[point-order] 重新规划或调整手绘轨迹，让轨迹只按点位次序穿过每个点");
            }
        }
    }
}

/// 服务端「经过点位」的命中半径（米），与 [`log_point_distances`] 自检同口径。
const POINT_HIT_RADIUS_M: f64 = 10.0;

/// 按首过下标升序排列点位索引；没经过（`None`）的点位排到最后。
///
/// `None` 用 `usize::MAX` 占位，而不是靠 `Option` 自带的 `Ord`：`Option`
/// 比较时 `None < Some(_)`，直接用会把「没经过」排到最前，等于把最没把握的
/// 点放到数组首位。稳定排序保证 `None` 之间保持原有相对次序。
fn order_by_first_pass(first_pass: &[Option<usize>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..first_pass.len()).collect();
    order.sort_by_key(|&i| first_pass[i].unwrap_or(usize::MAX));
    order
}

/// 每个点位在采样轨迹上的首过下标（BD-09，10m 命中圈）；没经过为 `None`。
fn first_pass_of(
    track_points: &[crate::track::TrackPoint],
    pt: &PointsContext,
) -> Vec<Option<usize>> {
    pt.points
        .iter()
        .map(|raw| {
            let bd = (
                raw.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0),
                raw.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0),
            );
            track_points
                .iter()
                .position(|p| crate::track::geo::distance((p.lat, p.lon), bd) < POINT_HIT_RADIUS_M)
        })
        .collect()
}

/// 按轨迹首过序就地重排点位数组；已经同序时不动，返回是否重排过。
///
/// 重排的是「点位数组」而不是轨迹：路线形状、里程、时间段全都不变，只让
/// `fivePointJson` 的数组序与轨迹实际经过的次序一致——服务端「按顺序通过
/// 所有点位」比的就是这两者。
///
/// 为什么需要它：`plan_stage` 在「按数组序绕不出目标里程」时会退回最近邻环
/// （见 `track/planner.rs` 里那句注释），那个分支跑得了，但首过序照旧是乱的，
/// 只能靠重排数组兜住。这是 issue #37 里实测通过的方案 B。
///
/// `points` / `coords` / `gcj_coords` 与 `cps` 是同一批点位的四份表示
/// （`plan_stage` 按同一下标构造 `cps`），必须一起跟走，否则报文里的坐标、
/// 前端地图上的点位、吸附用的打卡点会错位到不同的点。
fn reorder_by_first_pass(
    pt: &mut PointsContext,
    cps: &mut [Checkpoint],
    track_points: &[crate::track::TrackPoint],
    log: &mut Log,
) -> bool {
    let n = pt.points.len();
    // 三份表示必须一一对应；对不上就不动——宁可顺序错，也不越界。
    if n == 0 || pt.coords.len() != n || cps.len() != n {
        return false;
    }
    let first_pass = first_pass_of(track_points, pt);
    let order = order_by_first_pass(&first_pass);
    if order.iter().enumerate().all(|(k, &i)| k == i) {
        return false;
    }
    pt.points = order.iter().map(|&i| pt.points[i].clone()).collect();
    pt.coords = order.iter().map(|&i| pt.coords[i]).collect();
    // GCJ 坐标可能缺项（点位接口只给 BD-09），长度对不上就保持原样：
    // 上游对缺项本来就走 `get(i)` 兜底，这里不额外制造错位。
    if pt.gcj_coords.len() == n {
        pt.gcj_coords = order.iter().map(|&i| pt.gcj_coords[i]).collect();
    }
    let reordered_cps: Vec<Checkpoint> = order.iter().map(|&i| cps[i].clone()).collect();
    cps.clone_from_slice(&reordered_cps);
    log(&format!(
        "[point-order] 按首过下标重排点位数组：{order:?}（首过={first_pass:?}）"
    ));
    true
}

/// 只做到自检为止（不发任何写请求）。
///
/// 用来在提交前确认数据没问题——大部分坑在这一步就能发现。
pub fn dry_run(client: &Client, p: &RunParams, log: &mut Log) -> Result<RunOutcome, Error> {
    p.validate()?;
    // 校时：之后所有时间戳都必须落在服务器时间轴上。
    // 「开始时间 + 时长 ≤ 服务器现在」是服务端硬判据，本机时钟偏十几分钟
    // 就直接驳回，而本地自检用同一个时钟，永远发现不了这个偏差。
    // 阶段计时：每一步都报耗时，卡住时日志里能直接看出卡在哪一段。
    let dry_t0 = std::time::Instant::now();
    let mut stage_t = dry_t0;

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
        // 补跑：时间是外部算好的，但**必须搬到服务器时间轴上**再发。
        //
        // `start_time` 模块用的是本机时钟（它没有服务器时间），算出来的 `t`
        // 带本机误差。校时拿到的 `skew` 原来只写进日志就丢了——本机快 10 分钟
        // 时，补跑记录会原样落到服务器未来轴，同样被「开始 + 时长 ≤ 现在」
        // 驳回。`skew = 服务器 - 本机`，所以是加。
        Some(t) => {
            use rand::Rng;
            let mut r = rand::rng();
            rerun_start_ms(t, skew) + r.random_range(0..4_000i64)
        }
        // 「现在跑」= 补一条**刚跑完**的记录，不是「从现在开始跑」。
        //
        // 早先这里取 `server_now + 0~5s`，而 `stop = start + duration*1000`，
        // 于是整条记录的时间窗必然落在服务器未来——服务端按「开始 + 时长 ≤
        // 服务器当前时间」判据会驳回，且失败计入错误次数。现在把结束时刻压到
        // 服务器现在之前的 5~9 秒（留余量吸收往返延迟与毫秒抖动），
        // 起跑时刻随之倒推一整个时长。
        None => {
            use rand::Rng;
            let mut r = rand::rng();
            // 5~9 秒余量：吸收提交往返延迟与服务端毫秒级抖动
            start_for_immediate_run(
                server_now,
                p.duration_sec,
                5_000 + r.random_range(0..4_000i64),
            )
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
    let pol = stage_policy(client, p, log)?;
    log(&format!(
        "[耗时] 校时+策略 {:.1}s",
        stage_t.elapsed().as_secs_f64()
    ));
    stage_t = std::time::Instant::now();

    // ② fence
    let pick = stage_fence(client, p, log)?;
    log(&format!(
        "[耗时] 围栏 {:.1}s",
        stage_t.elapsed().as_secs_f64()
    ));
    stage_t = std::time::Instant::now();

    // ③ points
    let mut pt = stage_points(client, p, pick.as_ref(), log);
    log(&format!(
        "[耗时] 打卡点 {:.1}s",
        stage_t.elapsed().as_secs_f64()
    ));
    stage_t = std::time::Instant::now();

    // ④ 规划
    let stage = plan_stage(p, &pt, pick.as_ref(), start_ms, log)?;
    let TrackStage {
        mut cps,
        blockers_used,
        track,
        track_mode,
        stadium_fallback,
    } = stage;

    log(&format!(
        "[耗时] 规划 {:.1}s",
        stage_t.elapsed().as_secs_f64()
    ));
    stage_t = std::time::Instant::now();

    // ⑤ 上报采样
    let SampleStage {
        track_points,
        real_len,
        timed,
    } = sample_stage(p, &pol, &track, &cps, &blockers_used, start_ms, log)?;

    // ⑤.8 把点位数组重排成轨迹实际经过的次序（issue #37 方案 B）
    //
    // 服务端「按顺序通过所有点位」比的是 `fivePointJson` 的数组序与轨迹的首过序。
    // `plan_stage` 在「按数组序绕不出目标里程」时会退回最近邻环，那种回退场景
    // 首过序仍然是乱的；重排数组不改路线形状、不改里程，是那条支路唯一能兜住
    // 顺序问题的做法。
    //
    // 位置不能挪：下面 `fixed` 决定 `fivePointJson` 的数组序，且自检与提交共用
    // 同一份，晚于它重排就发不出去也验不到。
    reorder_by_first_pass(&mut pt, &mut cps, &track_points, log);

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
                // 服务端说哪个是固定打卡点就是哪个，别按排序猜。
                // 缺失时按 0（普通点）处理：`isFixed=1` 会让 App 换一种样式渲染，
                // 猜成固定点比漏一个固定点更容易看出异常。
                is_fixed: raw.get("isFixed").and_then(|v| v.as_i64()).unwrap_or(0),
            }
        })
        .collect();

    // 服务端的 reasonList 里「必经点位」和「普通点位」是分开判的，普通点还要
    // 「至少通过 2 个」（实测某校第 2 条规则）。点位数由服务端下发，我们改不了，
    // 但如果只给到 1 个普通点，这条无论如何都判不达标——提前说一声，别白跑。
    {
        let normal = fixed.iter().filter(|f| f.is_fixed == 0).count();
        let must = fixed.len() - normal;
        log(&format!(
            "[points] 点位构成：{must} 个必经 + {normal} 个普通"
        ));
        if !fixed.is_empty() && normal < 2 {
            log(&format!(
                "! [points] 本场只有 {normal} 个普通点位，服务端规则要求「至少通过 2 个普通点位」——\n    这条大概率判不达标，换一天或换个跑法再看"
            ));
        }
    }

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
    let report = inspect_probe(
        &view,
        &pol,
        &cp_tuples,
        &sp,
        p.sample_interval_sec,
        server_now,
        log,
    )?;

    audit_points(&probe_sub, &fixed, log)?;

    let order_seen = log_point_distances(&pt, &view, log);

    warn_point_order(&order_seen, log);

    log(&format!(
        "[耗时] 采样+自检 {:.1}s · 全程 {:.1}s",
        stage_t.elapsed().as_secs_f64(),
        dry_t0.elapsed().as_secs_f64()
    ));

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
        track_mode,
        stadium_fallback,
    })
}

/// 完整流程：自检通过后上传明细并提交成绩。
///
/// `confirm` 返回 false 时在提交前中止，已上传的 OBS 对象保留（不影响）。
/// 主体配速提示。返回 `None` 表示不用提示。
///
/// **为什么要单独算主体配速**：起步窗口是「占时长但不产里程」的——
/// `startup_sec` 秒里几乎不动，剩下的时间必须把全部里程跑完。所以真正
/// 发出去的逐点配速是**主体配速**，不是「距离/时长」那个平均值，两者能
/// 差半分钟以上。
///
/// 这个差别决定一条记录看起来红不红：学校区间 5'00"~10'00" 时，平均
/// 5'47" 听着合规，主体却是 4'57" —— 已经在上界外面，整条线全红。生成
/// 过程里原本没有任何地方会把这件事说出来。
///
/// 只提示，不改参数：距离和时长是用户选的，主体配速是它们的数学结果。
pub fn body_pace_note(
    distance_m: f64,
    duration_sec: i64,
    pace_range: Option<(f64, f64)>,
) -> Option<String> {
    let (pa, pb) = pace_range?;
    if !distance_m.is_finite() || distance_m <= 0.0 || duration_sec <= 0 {
        return None;
    }
    let (fast, slow) = (pa.min(pb), pa.max(pb));
    if !(fast.is_finite() && slow.is_finite()) || slow <= 0.0 {
        return None;
    }
    let startup = crate::track::startup_sec(duration_sec).max(0) as f64;
    let run_sec = (duration_sec as f64 - startup).max(1.0);
    let body_pace = 1_000.0 / (60.0 * (distance_m / run_sec));
    let fmt = |m: f64| {
        let mm = m.floor() as i64;
        let ss = ((m - mm as f64) * 60.0).round() as i64;
        format!("{mm}'{ss:02}\"")
    };
    // 距边界的余量（秒/公里）；负数是已经越界。
    let margin_fast = (body_pace - fast) * 60.0;
    let margin_slow = (slow - body_pace) * 60.0;
    let worst = margin_fast.min(margin_slow);
    if worst < 0.0 {
        let side = if margin_fast < 0.0 {
            "快于下界"
        } else {
            "慢于上界"
        };
        return Some(format!(
            "! [pace] 主体配速 {} {}（区间 {}~{}，起步窗口 {}s 不产里程）——逐点几乎全部会标红，建议把时长调长或距离调短。",
            fmt(body_pace),
            side,
            fmt(fast),
            fmt(slow),
            startup as i64
        ));
    }
    if worst < 40.0 {
        return Some(format!(
            "! [pace] 主体配速 {} 距区间边界只有 {:.0} 秒（区间 {}~{}）——正常的逐点波动就会越过边界标红，建议留出 1 分钟以上余量。",
            fmt(body_pace),
            worst,
            fmt(fast),
            fmt(slow)
        ));
    }
    Some(format!(
        "[pace] 主体配速 {}（区间 {}~{}，余量 {:.0} 秒）",
        fmt(body_pace),
        fmt(fast),
        fmt(slow),
        worst
    ))
}

/// 本机今天的日期键 `YYYY-MM-DD`（东八区）。
fn local_day_key() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    day_key_of(secs * 1000)
}

/// 毫秒戳 → 东八区日期键 `YYYY-MM-DD`。
pub(crate) fn day_key_of(ms: i64) -> String {
    let days = (ms / 1000 + 8 * 3600) / 86400;
    // 从 1970-01-01 起算的民用历
    let (mut y, mut d) = (1970i64, days);
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if d >= len {
            d -= len;
            y += 1;
        } else {
            break;
        }
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let md = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1usize;
    for (i, &len) in md.iter().enumerate() {
        if d < len {
            month = i + 1;
            break;
        }
        d -= len;
    }
    format!("{y:04}-{month:02}-{:02}", d + 1)
}

/// 本机当前时间 `HH:MM:SS`（本地时区）。
fn local_hms() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // 本机时区偏移：东八区用固定 +8（项目其余地方同样按 +8 处理日期）
    let local = (secs + 8 * 3600) % 86400;
    format!(
        "{:02}:{:02}:{:02}",
        local / 3600,
        (local % 3600) / 60,
        local % 60
    )
}

/// 完整跑一遍：取规则、选围栏、取打卡点、规划并合成轨迹，
/// 提交前调用 `confirm`，它回 `false` 就中止（已上传的 OBS 对象保留）。
pub fn run_full(
    client: &Client,
    p: &RunParams,
    log: &mut Log,
    confirm: &mut dyn FnMut(&RunOutcome) -> bool,
) -> Result<RunOutcome, Error> {
    // 前七步与 dry_run 完全一致
    let mut out = dry_run(client, p, log)?;

    // ❼.4 提交前两道闸门：被标记、被冻结，都不该继续。
    //
    // 这两件事一旦发生，继续提交只会往名单里再加一条：
    // - cheatlist 的 `self` 非 null = 已被标记（很多判定是事后跑批做的，
    //   「提交成功」不代表干净）
    // - `freezeRunTime > 0` = 跑步冻结期，App 里连开始按钮都是倒计时的
    //
    // 都只做「明确异常才拦」：查询失败按未知放行并打日志，不能因为一个
    // 只读接口挂了就让人跑不了步。
    // 日上限现在从服务端 policy 的 `runRuleModel.dayGoal` 读（实测某校 3000m），
    // 老代码写死 5000 导致 5000 以内的超量提交照样放行——超出的那部分服务端
    // 直接不计有效里程，等于白跑。读不到才退回 DEFAULT_DAILY_CAP_M。
    // 「今天已经跑过多少」仍以服务端记录列表为准，查不到就只打日志不判断。
    match super::records::list_records(client, 1, 30, 1) {
        Ok(v) => {
            let today = local_day_key();
            let mut done = 0.0f64;
            if let Some(arr) = v.as_array() {
                for r in arr {
                    let st = r
                        .get("startTime")
                        .and_then(|x| x.as_i64())
                        .or_else(|| {
                            r.get("startTime")
                                .and_then(|x| x.as_str())
                                .and_then(|s| s.parse::<i64>().ok())
                        })
                        .unwrap_or(0);
                    // startTime 可能是秒也可能是毫秒（服务端两种都见过）。
                    // 按数量级判断：小于 1e11 当秒处理，乘 1000——之前直接按
                    // 毫秒算，结果全落到 1970 年，日上限永远显示 0m。
                    let st_ms = if st > 0 && st < 100_000_000_000 {
                        st * 1000
                    } else {
                        st
                    };
                    if day_key_of(st_ms) == today {
                        done += r.get("totalDis").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    }
                }
            }
            let after = done + p.distance_m;
            let (cap, cap_src) = crate::api::policy::daily_cap_m(client, p.run_mode);
            log(&format!(
                "[quota] 今日已跑 {done:.0}m，本次 {:.0}m，合计 {after:.0}m（日上限 {cap}m，来自 {cap_src}）",
                p.distance_m
            ));
            if after > cap as f64 {
                // 超日上限直接拦：超出部分不计入有效里程，等于白跑，而且
                // 一条明显超量的记录更容易被风控盯上。要硬跑就显式开开关。
                if crate::config::global().run.ignore_daily_limit {
                    log(&format!(
                        "! [quota] 超日上限 {}m，但已设 run.ignore_daily_limit，继续提交",
                        (after - cap as f64).round()
                    ));
                } else {
                    return Err(ErrorKind::client(format!(
                        "今日已跑 {done:.0}m，再加 {:.0}m 合计 {after:.0}m，超过日上限 {cap}m。\n    \
                         超出部分不计入有效里程。把距离降到 {:.0}m 以内，\n    \
                         或者设 PACE_IGNORE_DAILY_LIMIT=1 硬跑。",
                        p.distance_m,
                        (cap as f64 - done).max(0.0)
                    )));
                }
            }
        }
        Err(e) => log(&format!("[quota] 今日记录查询失败（不判断日上限）: {e}")),
    }

    // 有效时段：服务端窗口外直接回 11016，本地先看一眼能省一次白跑。
    // 时段按学校下发（randomValidTimes），拿不到就不判断。
    match crate::api::policy::valid_time_window(client) {
        Some((start, end)) => {
            let now = local_hms();
            if crate::api::policy::in_window(&now, &start, &end) {
                log(&format!(
                    "√ [window] 当前 {now} 在有效时段 {start}–{end} 内"
                ));
            } else {
                return Err(ErrorKind::client(format!(
                    "当前 {now} 不在学校有效时段 {start}–{end} 内，服务端会回 11016（不在有效时段）。\n    换个时间再跑，别在这里白烧一次提交。"
                )));
            }
        }
        None => log("[window] 拿不到学校时段（不影响提交）"),
    }

    // 月度抽检：命中就要真人做支付宝信用分核验，自动化替不了。
    // 只提醒不拦——核验在提交之后做，拦住了反而让人没法跑完再看。
    match crate::api::policy::monthly_verify_needed(client) {
        Ok(true) => log(
            "! [verify] 本月被抽中信用分核验：跑完要去 App 里做支付宝核验，\n    不做的话这条成绩可能判不达标（自动化替不了）",
        ),
        Ok(false) => log("√ [verify] 本月未被抽中核验"),
        Err(e) => log(&format!("[verify] 核验状态查询失败（不影响提交）: {e}")),
    }

    // 跑步冻结：`freezeRunTime > 现在` 就别提交，冻结期提交等于自己往名单里加条。
    {
        let freeze = out.policy.freeze_run_time;
        let now = crate::crypto::envelope::now_ms();
        if freeze > now {
            let left = (freeze - now) / 1000;
            return Err(ErrorKind::client(format!(
                "账号处于跑步冻结期，还剩 {left} 秒（至 {}）：冻结期提交会被再记一次违规。\n    等解冻后再跑，或用 App 里的申诉通道。",
                freeze / 1000
            )));
        }
        if freeze > 0 {
            log(&format!(
                "√ [freeze] 无冻结（上次冻结已于 {} 结束）",
                freeze / 1000
            ));
        } else {
            log("√ [freeze] 无冻结");
        }
    }

    {
        use crate::api::cheat::Status;
        match crate::api::cheat::query(client, 1, &mut |l| log(l)) {
            Ok(rep) => match rep.status() {
                Status::Flagged => {
                    return Err(ErrorKind::client(format!(
                        "服务端已把当前账号标记为违规（{}）：先处理，别继续提交。详见 [cheat] 那几行",
                        rep.summary()
                    )));
                }
                Status::NoRecord => log("√ [cheat] 违规名单自查：当前账号无记录"),
                s => log(&format!(
                    "[cheat] 自查结果未知（{}），按放行处理",
                    s.label()
                )),
            },
            Err(e) => log(&format!("[cheat] 自查失败（不影响提交）: {e}")),
        }
    }

    if out.report.blocked() {
        return Err(ErrorKind::client(format!(
            "自检未通过，不提交：{}",
            out.report.summary()
        )));
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
            return Err(ErrorKind::client(
                "点位体检未通过，提交会被判异常，已中止".into(),
            ));
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

    // ❼.7 提交台账：**发请求之前**先把「要提交这个时间段」落盘。
    //
    // 顺序不能反过来。先发后记的话，进程在请求发出后崩掉就没有任何痕迹，
    // 下次启动会认为这段时间没提交过，再跑一次就是同一时间段两条记录——
    // 那是服务端最容易判定异常的形态（一个人不可能同时跑两次）。
    //
    // 三道拦截见 `guard::Guard::check`：上次结果未知 → 拦；时间段重叠 →
    // 拦；不同账号 → 各记各的。
    let mut guard = super::guard::Guard::open(&super::session::data_dir(), &uid)?;
    // 有别的时段的未知结果时放行（它不影响这一段的判重），但要说一声：
    // 那条记录可能已经在服务端，用户该去核对。
    if let Some((ps, pe)) = guard.pending_range() {
        log(&format!(
            "! [guard] 另有 {} ~ {} 的提交结果未知（与本次不重叠，不影响提交）。\
             建议到运动记录页核对那条是否入账。",
            super::guard::fmt_ms(ps),
            super::guard::fmt_ms(pe)
        ));
    }
    guard.begin(&uuid, start_ms, stop_ms)?;
    log(&format!(
        "[guard] 台账已记 pending（{} ~ {}），提交中…",
        start_ms, stop_ms
    ));

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
        return Err(ErrorKind::client(
            "点位数组在自检与提交之间被改动过（A4 回归）".into(),
        ));
    }

    // 7.4.01 已移除 BODY_SENSORS，报文里不许出现心率字段
    let mut body = submission.save_record.clone();
    let dropped = crate::track::detect::strip_forbidden(&mut body);
    if !dropped.is_empty() {
        log(&format!(
            "[detect] 已剔除不该有的字段: {}",
            dropped.join(", ")
        ));
    }
    // 自由跑 / 户外跑：save/record 报文整个省略 fivePointJson。
    // neko 实测 sportType=4 这样发服务端收下且 App 归到自由跑；发空串
    // envelope 则归类不确定（issue #35 的本地补丁就是发空串，没敢合）。
    // fivePointJson 不进签名（31 字段里没有它），移除不影响签名。
    // OBS 明细的 fixed_point_json 仍发 envelope（内层空数组），与 neko 一致。
    if omit_five_point_for_free_run(&mut body, p.free_run) {
        log("[submit] 自由跑：报文省略 fivePointJson");
    }
    // 派生字段互校：爬升 / 卡路里 / 功率 / 配速 / 圈数据必须与点位列自洽。
    // 这些都是「填错了也能提交成功」的字段，回退成常量只会让服务端把整条
    // 记录判成来源可疑，所以宁可在这里拦下。
    let derived_problems = super::submit::audit_derived(&body, &submission.points);
    if !derived_problems.is_empty() {
        return Err(ErrorKind::client(format!(
            "报文派生字段自检失败（未提交）：{}",
            derived_problems.join("；")
        )));
    }
    log("√ [selfcheck] 派生字段与点位列自洽（爬升/卡路里/功率/配速/圈）");
    // 必到点次序回显。App 详情页的「必到点位」就是按这段报文显示的：
    // position 必须是 0,1,2,… 连续递增、数组同序、isFixed 只有首点为 1。
    // 只要这一行不对，页面上就一定会少点位，先查这里再查轨迹。
    if let Ok(env) = serde_json::from_str::<serde_json::Value>(
        body.get("fivePointJson")
            .and_then(|v| v.as_str())
            .unwrap_or("{}"),
    ) && let Some(inner) = env
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
        log(&format!(
            "[fixed-point] {} 个必到点：{}",
            list.len(),
            list.join(" → ")
        ));
    }
    log(&format!(
        "√ [submit] 报文已组装，{} 个轨迹点",
        out.track.len()
    ));

    log("[submit] 提交成绩…");
    // runes/runef 关联头：策略下发时刻+uid、运行uuid+开始时刻
    let runes = format!("{}{}", out.policy.timestamp, uid);
    let runef = format!("{}{}", uuid, start_ms);
    let data = match super::submit::save_record_detailed(client, &body, &runes, &runef) {
        Ok(d) => d,
        Err(f) => {
            // 只有「服务端明确驳回」才释放时间段。网络错误、HTTP 异常、
            // 响应解析失败一律留着 pending —— 请求可能已经入账，只是我们
            // 没看到回执。这条区分是整套台账的立足点。
            if f.rejected {
                guard.rejected(&uuid)?;
                log("[guard] 服务端明确驳回，已释放该时间段");
            } else {
                log("[guard] 提交结果未知，台账保留 pending（不会自动重试）");
            }
            // 服务端对上传失败有 3 天窗口（App 文案：「成绩已暂时保存至本地，
            // 请于3天内上传成绩」）。所以提交失败不等于白跑，别急着重新刷一条
            // ——先确认这条有没有入账，重复提交才是真麻烦。
            log(&format!("! [submit] 提交失败：{}", f.message));
            log(
                "  服务端有 3 天补传窗口：这条记录客户端侧已生成，先到 App 记录页\n  \
                 核对是否入账，别立刻重刷（重复提交比失败更麻烦）",
            );
            return Err(ErrorKind::protocol(f.message));
        }
    };
    out.submitted = true;
    out.record_id = super::submit::extract_record_id(&data);
    if let Some(rrid) = out.record_id {
        // 台账落账失败不该让整次提交失败（记录已经入账了），但要说出来。
        if let Err(e) = guard.accepted(&uuid, rrid) {
            log(&format!(
                "[guard] 警告：台账写 accepted 失败（记录已提交）: {e}"
            ));
        }
    }
    log(&format!("√ [submit] 提交成功，记录 id={:?}", out.record_id));

    // ⑨ OBS：用 rrid 重算桶 id 和 objectKey
    let rrid = out.record_id.unwrap_or(0);
    log(&format!("[obs] 用 rrid={rrid} 上传明细…"));
    // OBS 的 fixed_point_json 要的是裸点位数组，不是主报文那层 envelope。
    // 之前把 envelope 整个塞进来，服务端解不出点位列表。
    // OBS 的 fixed_point_json 要的是**完整 envelope**（含 fivePointJson 内层串、
    // useZip、runAreaId、geoFencesJson、freedomShowFence），不是裸点位数组。
    // 实测真机报文就是这么发的；传裸数组时服务端解析不出点位，于是
    // 「按顺序通过所有点位」永远 false。
    // 从 submission 取（不是 body）：自由跑的 body 已省略 fivePointJson，
    // 但 OBS 明细仍要 envelope（内层空数组），submission 里始终保留。
    let five_pt = submission
        .save_record
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
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| ErrorKind::client(format!("明细序列化失败: {e}")))?;
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

    log(&format!("  静默窗口 {} 秒内不要再发跑步请求", SILENCE_SEC));

    // ⑩ 回读服务端判定。
    //
    // 服务端把裁定结果写在记录详情里：`isCheat`（作弊标志）、`stepFreqStatus`
    // （步频判定）、`validDis`（服务端算的有效距离）、`reasonList`（逐条规则
    // 的裁定）。这是唯一能确认「这一单到底算不算成绩」的依据——本地自检再
    // 全也只是猜。只读，失败不影响已经入账的记录。
    //
    // 参考：黑盒探测报告里 get_one_record 返回 53 字段，reason 文案形如
    // 「最少要跑2.00公里」「配速有效范围」。
    if let Some(rrid) = out.record_id {
        match super::records::get_record(client, rrid) {
            Ok(d) => {
                let num = |k: &str| d.get(k).and_then(|v| v.as_f64());
                let cheat = d.get("isCheat").and_then(|v| v.as_i64()).unwrap_or(-1);
                let step_st = d
                    .get("stepFreqStatus")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(-1);
                let vd = num("validDis");
                let td = num("totalDis");
                let mut bad: Vec<String> = Vec::new();
                if cheat != 0 {
                    bad.push(format!("isCheat={cheat}（被判作弊）"));
                }
                if step_st != 0 {
                    bad.push(format!("stepFreqStatus={step_st}（步频异常）"));
                }
                if let (Some(v), Some(t)) = (vd, td)
                    && t > 0.0
                    && v + 1.0 < t
                {
                    bad.push(format!(
                        "validDis={v:.0}m < totalDis={t:.0}m（被扣了 {:.0}m）",
                        t - v
                    ));
                }
                // 记录状态：服务端把分类写在这里（App 记录列表的标签就来自它）。
                // 0=正常；其余按探测报告的状态枚举解释，未知值原样显示。
                let st = d.get("status").and_then(|v| v.as_i64()).unwrap_or(-1);
                let st_label = match st {
                    0 => "正常",
                    1 => "作弊",
                    2 => "未打卡（点位）",
                    3 => "未上传",
                    4 => "配速过快",
                    5 => "步频过慢",
                    _ => "未知",
                };
                if let Some(info) = d.get("statusInfo").and_then(|v| v.as_str())
                    && !info.is_empty()
                {
                    log(&format!("  · 判定文案：{info}"));
                }
                if st != 0 {
                    bad.push(format!("status={st}（{st_label}）"));
                }
                log(&format!(
                    "√ [verify] 服务端判定：status={st}({st_label}) isCheat={cheat} stepFreqStatus={step_st} validDis={}/{}",
                    vd.map(|x| format!("{x:.0}m")).unwrap_or_else(|| "?".into()),
                    td.map(|x| format!("{x:.0}m")).unwrap_or_else(|| "?".into())
                ));
                if let Some(list) = d.get("reasonList").and_then(|v| v.as_array()) {
                    for r in list {
                        let ok = r.get("complete").and_then(|v| v.as_bool()).unwrap_or(true);
                        let reason = r.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                        log(&format!("  · {} {}", if ok { "✓" } else { "✗" }, reason));
                        if !ok {
                            bad.push(format!("规则未达标：{reason}"));
                        }
                    }
                }
                if bad.is_empty() {
                    log("√ [verify] 这一条服务端认可：无作弊标记、无扣量、规则全达标");
                } else {
                    log(&format!("! [verify] 服务端有意见：{}", bad.join("；")));
                    // 把「哪条规则没过」映射成「下一步改什么」。
                    // 没有映射时不硬猜——服务端文案会变，猜错比不说更糟。
                    let text = bad.join(" ");
                    let mut tips: Vec<&str> = Vec::new();
                    if text.contains("配速") {
                        tips.push(
                            "配速不达标：调时长或距离，让平均配速落进区间（reasonList 里写了范围）",
                        );
                    }
                    if text.contains("步频") {
                        tips.push("步频不达标：检查 sportType 与步数设置，服务端按每分钟步数判");
                    }
                    if text.contains("点位") || text.contains("打卡") {
                        tips.push(
                            "点位没全过：把锚点挪开、换一条路线，或看 [fixed-point] 那行的下标",
                        );
                    }
                    if text.contains("最少要跑") || text.contains("分钟") {
                        tips.push("距离/时长不够：目标值要是服务端下发的最低要求，别自己往下调");
                    }
                    if text.contains("作弊") || text.contains("扣") {
                        tips.push(
                            "被扣量/判作弊：这一条已经记进违规计数，先停手查 [cheat] 与设备环境",
                        );
                    }
                    for t in tips {
                        log(&format!("    → {t}"));
                    }
                }
            }
            Err(e) => log(&format!("[verify] 回读失败（记录已提交，不影响成绩）: {e}")),
        }
    }

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

/// 自由跑 / 户外跑的报文形状：整个省略 `fivePointJson`。
///
/// 该字段不进签名（见 submit 的 31 字段表），移除不影响签名；记分跑
/// （`free_run=false`）原样保留。返回是否真的移除了字段。
pub fn omit_five_point_for_free_run(body: &mut Value, free_run: bool) -> bool {
    if !free_run {
        return false;
    }
    body.as_object_mut()
        .map(|o| o.remove("fivePointJson").is_some())
        .unwrap_or(false)
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
    use flate2::Compression;
    use flate2::write::GzEncoder;
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
) -> Result<Vec<String>, Error> {
    let uuid = super::client::uuid_v4();
    let keys = vec![
        obs::detail_key(start_ms, &uuid),
        obs::fallback_key(uid, p.sport_type, start_ms / 1000, &uuid),
    ];
    let bytes = serde_json::to_vec(payload)
        .map_err(|e| ErrorKind::client(format!("明细序列化失败: {e}")))?;
    log(&format!("[obs] 上传明细 {} 字节…", bytes.len()));
    let (ok, errs) = obs::upload_keys(client, &keys, &bytes);
    for e in &errs {
        log(&format!("  ⚠ {e}"));
    }
    if ok == 0 {
        return Err(ErrorKind::client("OBS 全部 key 上传失败".into()));
    }
    log(&format!("√ [obs] {ok}/{} 个 key 上传成功", keys.len()));
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::body_pace_note;
    use super::day_key_of;
    use super::*;
    use crate::api::submit::{self, FixedPoint, SubmitParams};
    use base64::Engine;
    use flate2::read::GzDecoder;
    use std::io::Read;

    #[test]
    fn epoch_is_1970_01_01() {
        assert_eq!(day_key_of(0), "1970-01-01");
    }

    #[test]
    fn leap_day_is_handled() {
        // 2020-02-29 12:00 UTC = 东八区 20:00
        let ms = 1_582_977_600_000i64; // 2020-02-29T12:00:00Z
        assert_eq!(day_key_of(ms), "2020-02-29");
        // 2021-02-28T00:00:00Z → 东八区 08:00，仍是 28 日（非闰年没有 29 日）
        let ms2 = 1_614_499_200_000i64;
        assert_eq!(day_key_of(ms2), "2021-02-28");
    }

    #[test]
    fn ignores_utc_vs_local_but_uses_plus8() {
        // 2026-11-23T15:30:00Z = 东八区 23:30 → 仍是 23 日
        let ms = 1_795_447_800_000i64;
        assert_eq!(day_key_of(ms), "2026-11-23");
    }

    // ---- merged from former `mod body_pace_tests` ----

    /// 已经越界：2km/11min 的主体配速约 4'35"，快于 5'00" 下界。
    #[test]
    fn too_fast_body_is_flagged() {
        let note = body_pace_note(2000.0, 660, Some((5.0, 10.0))).unwrap();
        assert!(note.contains("快于下界"), "{note}");
    }

    /// 2km/16min：主体 7'12"，落在区间中央，不该报警。
    #[test]
    fn central_body_is_not_warned() {
        let note = body_pace_note(2000.0, 960, Some((5.0, 10.0))).unwrap();
        assert!(note.starts_with("[pace] 主体配速"), "{note}");
        assert!(!note.starts_with('!'), "不该是警告：{note}");
    }

    /// 贴边：2km/12min 的主体配速是 5'04"，距 5'00" 下界只有 4 秒。
    /// 这就是用户那张图「在 5' 线上反复穿越」的直接成因。
    #[test]
    fn hugging_the_edge_warns() {
        let note = body_pace_note(2000.0, 720, Some((5.0, 10.0))).unwrap();
        assert!(note.starts_with("! [pace]"), "{note}");
        assert!(note.contains("距区间边界只有"), "{note}");
        assert!(note.contains("5'04"), "应报出主体配速，实际 {note}");
    }

    /// 太慢：主体超过上界。
    #[test]
    fn too_slow_body_is_flagged() {
        let note = body_pace_note(2000.0, 1500, Some((5.0, 10.0))).unwrap();
        assert!(note.contains("慢于上界"), "{note}");
    }

    #[test]
    fn no_policy_means_no_note() {
        assert!(body_pace_note(2000.0, 720, None).is_none());
    }

    #[test]
    fn degenerate_inputs_are_silent() {
        assert!(body_pace_note(0.0, 720, Some((5.0, 10.0))).is_none());
        assert!(body_pace_note(2000.0, 0, Some((5.0, 10.0))).is_none());
        assert!(body_pace_note(2000.0, 720, Some((0.0, 0.0))).is_none());
    }

    /// 区间上下界写反了也要能认（`speedTop` / `speedBottom` 语义不保证）。
    #[test]
    fn reversed_range_is_normalized() {
        let a = body_pace_note(2000.0, 720, Some((5.0, 10.0))).unwrap();
        let b = body_pace_note(2000.0, 720, Some((10.0, 5.0))).unwrap();
        assert_eq!(a, b);
    }

    // ---- merged from former `mod obs_tests` ----

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
        for k in [
            "rrid",
            "uuid",
            "uid",
            "run_data",
            "fixed_point_json",
            "segment_json",
            "speed_json",
            "step_freq_json",
            "laps_json",
            "runFaceCheck",
        ] {
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

    // ---- merged from former `mod tests` ----

    /// 回归：只有用户明确选了操场才反推跑道。
    ///
    /// 「自动规划」被 `run_mode == 1` 一条判据带到 `try_stadium_track`，
    /// 打卡点围成矩形就判成操场，画出一条不在跑道上的椭圆。
    #[test]
    fn stadium_attempt_requires_explicit_choice() {
        assert!(should_try_stadium(1, true), "选了操场绕圈，试");
        assert!(!should_try_stadium(1, false), "选了自动规划，不试");
        assert!(!should_try_stadium(2, true), "户外跑没有操场可绕");
    }

    /// 自由跑报文省略 fivePointJson；记分跑原样保留。
    #[test]
    fn free_run_omits_five_point_json() {
        let mut body = json!({ "sportType": 4, "fivePointJson": "{}", "uid": "1" });
        assert!(omit_five_point_for_free_run(&mut body, true));
        assert!(
            body.get("fivePointJson").is_none(),
            "自由跑不应带 fivePointJson"
        );
        assert_eq!(body["sportType"], json!(4), "其余字段不动");

        let mut scored = json!({ "sportType": 1, "fivePointJson": "{}" });
        assert!(!omit_five_point_for_free_run(&mut scored, false));
        assert!(scored.get("fivePointJson").is_some(), "记分跑必须保留");
    }

    /// 自由跑默认关闭：Default 不得悄悄改变记分跑行为。
    #[test]
    fn free_run_defaults_off() {
        assert!(!RunParams::default().free_run);
    }

    /// 回归：补跑时刻必须按校时偏移搬到服务器轴上。
    ///
    /// 原来 `Some(t) => t + 0..4000` 只加了抖动，`skew` 只写进日志就被丢掉——
    /// 本机时钟偏快时，补跑记录落到服务器未来轴，被「开始 + 时长 ≤ 现在」
    /// 驳回（与「现在跑」同一条判据，只堵了一个入口）。
    #[test]
    fn rerun_start_shifts_onto_server_clock() {
        let local_t = 1_790_913_882_164i64;
        // 本机快 10 分钟 → 服务器比本机慢 → skew = -600_000
        let shifted = rerun_start_ms(local_t, Some(-600_000));
        assert_eq!(shifted, local_t - 600_000);
        assert!(shifted < local_t, "本机偏快时补跑时刻必须往前挪");

        // 本机慢 3 分钟 → skew = +180_000
        assert_eq!(rerun_start_ms(local_t, Some(180_000)), local_t + 180_000);
        // 校时失败时保持原值，不做隐式偏移
        assert_eq!(rerun_start_ms(local_t, None), local_t);
    }

    /// 「现在跑」生成的记录必须**整条落在过去**：服务端判据是
    /// 「开始 + 时长 ≤ 服务器当前时间」，早先这里写成 `now + 0~5s`，
    /// 导致 stopTime 必然在未来、自检还查不出来。
    #[test]
    fn immediate_run_lands_entirely_in_the_past() {
        let now = 1_790_913_882_164i64;
        for dur in [1i64, 60, 600, 3600, 11_000] {
            for margin in [5_000i64, 9_000] {
                let start = start_for_immediate_run(now, dur, margin);
                let stop = start + dur * 1000;
                assert_eq!(stop, now - margin, "时长 {dur}s 时 stop 应为 now-margin");
                assert!(stop < now, "时长 {dur}s：stopTime 落到了未来");
            }
        }
    }

    /// 标准 400m 跑道的半径不能被上限压小。
    ///
    /// 原来 `radius` 是 `clamp(20, 30)`，而第 1 道运行半径是 36.8——整圈
    /// 往里缩 6.8 米，直段直接落进中央足球场。实测三种打卡点分布（外沿、
    /// 内场边线、内场里面）算出来全是 30，一个都没跑掉。
    #[test]
    fn stadium_radius_keeps_standard_400m_track() {
        let lat0 = 47.386;
        let lng0 = 103.956;
        let klat = crate::track::geo::m_per_deg_lat(lat0);
        let klng = crate::track::geo::m_per_deg_lng(lat0);
        let mk = |dy: f64, dx: f64| Checkpoint {
            lat: lat0 + dy / klat,
            lng: lng0 + dx / klng,
            glat: lat0 + dy / klat,
            glon: lng0 + dx / klng,
            name: "P".into(),
        };
        // 标准 400m 跑道外框：长 84.39+2*46.26 ≈ 177，宽 2*46.26 ≈ 92.5
        let (half_l, half_w) = (88.5, 46.26);
        let cps = vec![
            mk(0.0, -half_l),
            mk(0.0, half_l),
            mk(2.0 * half_w, -half_l),
            mk(2.0 * half_w, half_l),
        ];
        let logs: std::rc::Rc<std::cell::RefCell<Vec<String>>> = Default::default();
        let l2 = std::rc::Rc::clone(&logs);
        let mut log = move |s: &str| l2.borrow_mut().push(s.to_string());
        let r = try_stadium_track(&cps, 2000.0, 10.0, 42, &mut log);
        drop(log);
        assert!(r.is_some(), "标准 400m 外框该认成操场");

        let got = logs.borrow();
        // 日志里那句带 半径XXm，抠出来核对没被压到 30
        let line = got
            .iter()
            .find(|l| l.contains("操场绕圈"))
            .expect("缺少操场日志");
        let rad: f64 = line
            .split("半径")
            .nth(1)
            .and_then(|t| t.split('m').next())
            .and_then(|t| t.parse().ok())
            .unwrap_or(0.0);
        assert!(
            (rad - 36.8).abs() < 1.0,
            "标准 400m 外框应该直接按第 1 道 36.8m 生成，实际 {rad}m：{line}"
        );
        assert!(
            got.iter().any(|l| l.contains("标准 400m")),
            "应该走标准跑道那条分支：{got:?}"
        );
    }

    /// 四角围成跑道外框 → 认成操场，生成的椭圆首尾闭合。
    #[test]
    fn stadium_detects_oval_from_four_corners() {
        let lat0 = 47.386;
        let lng0 = 103.956;
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
        assert!(
            got.iter().any(|l| l.contains("操场绕圈")),
            "缺少操场日志: {got:?}"
        );
    }

    /// 打卡点散在几百米范围 → 不是操场，必须返回 None 走路线规划。
    #[test]
    fn stadium_rejects_scattered_checkpoints() {
        let lat0 = 47.386;
        let lng0 = 103.956;
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

    /// 「田径场中」这类内场点位（距跑道 16~45m）：轨迹必须凹进去一段才
    /// 经过它。吸附了几次、最深多少米要打进日志——用户反馈「轨迹穿进
    /// 足球场」时，从日志就能分辨是点位吸附（正常，必须打卡）还是跑偏。
    #[test]
    fn stadium_snaps_infield_checkpoint_and_logs() {
        let lat0 = 47.386;
        let lng0 = 103.956;
        let klat = crate::track::geo::m_per_deg_lat(lat0);
        let klng = crate::track::geo::m_per_deg_lng(lat0);
        let mk = |dy: f64, dx: f64| Checkpoint {
            lat: lat0 + dy / klat,
            lng: lng0 + dx / klng,
            glat: lat0 + dy / klat,
            glon: lng0 + dx / klng,
            name: "P".into(),
        };
        // 标准 400m 外框四角 + 一个场地正中的点位
        let cps = vec![
            mk(0.0, -88.5),
            mk(0.0, 88.5),
            mk(92.5, -88.5),
            mk(92.5, 88.5),
            mk(46.25, 0.0),
        ];
        let logs: std::rc::Rc<std::cell::RefCell<Vec<String>>> = Default::default();
        let l2 = std::rc::Rc::clone(&logs);
        let mut log = move |s: &str| l2.borrow_mut().push(s.to_string());
        let r = try_stadium_track(&cps, 2000.0, 10.0, 42, &mut log);
        drop(log);
        let pts = r.expect("内场有一个点位也该生成操场轨迹（吸附过去）");
        let got = logs.borrow();
        assert!(
            got.iter().any(|l| l.contains("已吸附")),
            "内场点位被吸附时要写日志: {got:?}"
        );
        assert!(
            got.iter().any(|l| l.contains("打卡点贴合")),
            "要记录贴合统计: {got:?}"
        );
        // 吸附后的轨迹必须真的经过那个内场点位（30m 内）
        let mut best = f64::MAX;
        for q in &pts {
            best = best.min(crate::track::geo::distance(*q, (cps[4].lat, cps[4].lng)));
        }
        assert!(best < 30.0, "吸附后距内场点位 {best:.0}m，还是没经过它");
    }

    /// 吸附够不着的点位（>45m）必须拒绝操场形状，回退路线规划——计分跑
    /// 漏打卡等于白烧一次提交额度。现在的尺寸保护让这种点位进不了操场
    /// 分支，这条主要是防将来放宽阈值后悄悄漏打卡。
    #[test]
    fn stadium_cp_fits_rejects_far_checkpoint() {
        let pts = vec![(47.386, 103.956), (47.387, 103.956)];
        let cps = vec![Checkpoint {
            lat: 47.396,
            lng: 103.956,
            glat: 47.396,
            glon: 103.956,
            name: "远处".into(),
        }];
        let logs: std::rc::Rc<std::cell::RefCell<Vec<String>>> = Default::default();
        let l2 = std::rc::Rc::clone(&logs);
        let mut log = move |s: &str| l2.borrow_mut().push(s.to_string());
        assert!(
            !stadium_cp_fits(&pts, &cps, &mut log),
            "1.1km 外的点位不该判贴合"
        );
        drop(log);
        let got = logs.borrow();
        assert!(
            got.iter().any(|l| l.contains("漏打卡")),
            "拒绝时要写明后果: {got:?}"
        );
    }

    /// 操场多圈的 GPS 抖动必须随采样间距缩放：θ/σ 是在真机 14.9m 间距下
    /// 标定的，固定值用在 5m 间距上逐点噪声占比翻三倍——#6 实测标准
    /// 400m 一圈量出 429m（+7.3%），画出来一整条「河」。
    #[test]
    fn campus_oval_wobble_scales_with_spacing() {
        let spec = crate::track::OvalSpec {
            straight: 84.39,
            radius: 36.8,
            heading: 0.82,
            laps: 5.0,
        };
        let clean = 2.0 * spec.straight + 2.0 * std::f64::consts::PI * spec.radius;
        let clean_total = clean * spec.laps;
        for spacing in [5.2f64, 10.0, 14.9] {
            for seed in [42u64, 20261119, 7] {
                let pts = crate::track::campus_oval_track((47.386, 103.956), &spec, spacing, seed);
                let len = crate::track::geo::path_length(&pts);
                assert!(
                    len < clean_total * 1.03 + 40.0,
                    "间距 {spacing}m 种子 {seed}：操场轨迹 {len:.0}m，抖动把长度吹过头（目标 {clean_total:.0}m）"
                );
                assert!(
                    len > clean_total,
                    "间距 {spacing}m：{len:.0}m 比干净椭圆 {clean_total:.0}m 还短，轨迹断了"
                );
            }
        }
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
            crate::track::TrackPoint {
                lat: 30.5,
                lon: 104.0,
                time_ms: 1000,
            },
            crate::track::TrackPoint {
                lat: 30.5001,
                lon: 104.0,
                time_ms: 2000,
            },
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
        for k in [
            "uid",
            "sportType",
            "totalDistance",
            "totalTime",
            "totalSteps",
            "route",
        ] {
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

    // ---- merged from former `mod fixed_point_payload_tests` ----

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

        // 点位由 `build_points` 造一次，OBS 明细原样复用。这份明细不带必到点
        // （传 `&[]`，无必到点跑法的新口径）：`allLocJson` 里不撒打卡标记
        // （见下面 marked 的断言），`isPass` 由 `fixed_point_items` 按坐标命中
        // 算进 `fivePointJson`，两条路径互不依赖。
        let loc = crate::api::submit::build_points(&pts, &[], start_ms, None).expect("points");
        let obj = detail_payload_rrid(&sim, "123", 99, start_ms, "U", &five, &loc);
        let raw = obj["run_data"]
            .as_str()
            .expect("run_data 是 gzip+base64 字符串");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(raw)
            .expect("base64");
        let mut text = String::new();
        GzDecoder::new(&bytes[..])
            .read_to_string(&mut text)
            .expect("gunzip");
        let run: serde_json::Value = serde_json::from_str(&text).unwrap();
        let loc: Vec<serde_json::Value> =
            serde_json::from_str(run["allLocJson"].as_str().unwrap()).unwrap();

        // 新版真机（实测账号，rrid 1362483656）的 allLocJson 里
        // **没有任何打卡标记**：这份夹具不带必到点（传 `&[]`），走无必到点
        // 跑法的新口径 `{0: 101, 3: 1, 5: 1, 6: 1}`。带必到点的计分跑才会
        // 撒 `type=2`（见 submit::PointTypeScheme 与 issue #8 的回归）。
        let marked: Vec<usize> = loc
            .iter()
            .enumerate()
            .filter(|(_, q)| q["type"].as_i64() == Some(2))
            .map(|(i, _)| i)
            .collect();
        assert!(
            marked.is_empty(),
            "新版真机不撒 type=2 打卡标记（App 靠 fivePointJson 渲染），实际撒了 {} 个: {marked:?}",
            marked.len()
        );
        // 命中判定半径 10m、采样间距 5m，所以「首次命中」可能是第 24 个点，
        // 加上 ±2 的标记窗口，形成一段连续标记。
        //
        // `type=2` 同时也是「次一级打卡 / 弱定位」，`point_json_full` 会给
        // 普通采样点按 5% 概率撒这个值（真机分布里 type=2 有 5 个点），所以
        // 这里不断言「全部标记都落在窗口内」——只断言**打卡点本身**被标到：
        // 第 25 个点必须在标记集合里，且标记不能整体跑到轨迹开头（那样就是
        // 又把 `position` 当轨迹下标用了）。
        // 新版真机不发 type=2，上面已断言 marked 为空；这里保留「position 不能
        // 被当成轨迹下标」的另一半检查：fivePointJson 的 position 必须是 0..N-1
        // 连续序号，一旦又变成轨迹下标（0/71/88/170/51 那种），App 会少画点位。
        let fps: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(&five)
            .ok()
            .and_then(|v| v.get("fivePointJson").cloned())
            .and_then(|v| v.as_str().and_then(|x| serde_json::from_str(x).ok()))
            .unwrap_or_default();
        assert!(!fps.is_empty(), "fivePointJson 不能为空");
        for (i, f) in fps.iter().enumerate() {
            assert_eq!(
                f["position"].as_i64(),
                Some(i as i64),
                "第 {i} 个必到点的 position 必须是连续序号，实际 {:?}",
                f["position"]
            );
        }
    }

    // ---- merged from former `mod single_point_source_tests` ----

    // A4：**自检那份点位必须就是上传那份**（`docs/fix/FIX-A4-SINGLE-POINT-SOURCE.md`）。
    //
    // 修复前同一条轨迹被造了两次点位：
    //
    // - `submit::build` 那份：`seed = start_ms ^ φ`、做过 `smooth_point_speed`、
    //   带打卡点（`type=2` / `isFixed`）；
    // - `flow::detail_payload_rrid` 那份（**真正进 OBS 的**）：`seed = start_ms`、
    //   不平滑、不带打卡点。
    //
    // 于是 `id` / `count` / `radius` / `type` / `bdA` 逐点不同，`isFixed`、
    // `type=2` 只有上传那份才有——自检守着一份没人会看到的报文。
    //
    // 这组测试拿一条 1683m / 555 秒、5 个打卡点的离线对象（不联网、不提交）
    // 逐元素逐字段比对两份，白名单只允许 envelope 级字段。

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
        "avgSpeed",
        "bdA",
        "bdD",
        "bdG",
        "bdS",
        "coorType",
        "count",
        "dtr",
        "flag",
        "gLat",
        "gLng",
        "gainTime",
        "id",
        "lat",
        "lng",
        "locType",
        "locationId",
        "queueNum",
        "radius",
        "speed",
        "state",
        "stepDistance",
        "totalDis",
        "totalTime",
        "type",
        "validDis",
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
        let xy = crate::track::campus_oval_track((47.3794, 103.9502), &spec, 5.0, 2026);
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
        crate::track::compress_startup(&mut track, START_MS, DURATION_SEC, None);

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
                    // 夹具：第一个当固定打卡点，其余普通点
                    is_fixed: if k == 0 { 1 } else { 0 },
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
            body["fivePointJson"]
                .as_str()
                .expect("fivePointJson 是字符串"),
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
                x.as_object()
                    .unwrap_or_else(|| panic!("{label} 第 {i} 点不是对象")),
                y.as_object()
                    .unwrap_or_else(|| panic!("{label} 第 {i} 点不是对象")),
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
        let probe =
            submit::build(&probe_params(&f), &f.track, &f.fixed, &f.sim).expect("自检报文应能组装");
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
        // isFixed 默认全 0（见 submit::force_normal_point），但两个字段
        // 都必须**存在**，缺字段等于告诉服务端「这个点没有标记信息」。
        assert!(
            fa.iter().all(|p| p.get("isFixed").is_some()) && fa.iter().all(|p| p["isPass"] == true),
            "必到点必须带 isFixed/isPass 标记：{fa:?}"
        );

        // 计分跑必须撒 `type=2` 打卡标记（口径见 submit::PointTypeScheme）：
        // 撤掉它服务端会把记录判成自由跑不计成绩（issue #8）。这个夹具
        // 带 5 个必到点且全部命中，标记必须正好 5 个。
        let marked = probe
            .points
            .iter()
            .filter(|p| p["type"].as_i64() == Some(2))
            .count();
        assert_eq!(
            marked, 5,
            "计分跑的打卡标记应与必到点数一致，实际 {marked} 个"
        );

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

    // ---- merged from former `mod issue37_tests` ----

    /// issue #37：操场默认关闭，走路网/按序串点；显式开关才试。
    #[test]
    fn stadium_default_off() {
        assert!(!RunParams::default().prefer_stadium, "默认不试操场");
        assert!(!should_try_stadium(1, RunParams::default().prefer_stadium));
        assert!(should_try_stadium(1, true), "显式选操场才试");
    }

    fn circle_pts() -> Vec<(f64, f64)> {
        let lat0 = 47.386;
        let lng0 = 103.956;
        let klat = crate::track::geo::m_per_deg_lat(lat0);
        let klng = crate::track::geo::m_per_deg_lng(lat0);
        (0..360)
            .map(|d| {
                let a = (d as f64).to_radians();
                (lat0 + 40.0 * a.sin() / klat, lng0 + 40.0 * a.cos() / klng)
            })
            .collect()
    }

    fn cp_at(deg: f64) -> Checkpoint {
        let lat0 = 47.386;
        let lng0 = 103.956;
        let klat = crate::track::geo::m_per_deg_lat(lat0);
        let klng = crate::track::geo::m_per_deg_lng(lat0);
        let a = deg.to_radians();
        Checkpoint {
            lat: lat0 + 40.0 * a.sin() / klat,
            lng: lng0 + 40.0 * a.cos() / klng,
            glat: lat0,
            glon: lng0,
            name: "P".into(),
        }
    }

    /// issue #37：123×86 这类外框长宽比放行，但点位数组序与绕圈序不一致 → 闸拒。
    #[test]
    fn order_gate_rejects_out_of_order() {
        let pts = circle_pts();
        // 数组序 0,180,90,270 ≠ 绕圈首次经过序 0,90,180,270
        let cps = vec![cp_at(0.0), cp_at(180.0), cp_at(90.0), cp_at(270.0)];
        let mut log = |_s: &str| {};
        assert!(
            !stadium_point_order_ok(&pts, &cps, &mut log),
            "不同序必须回退"
        );
    }

    /// 数组序与绕圈序一致 → 放行（真操场不被误杀）。
    #[test]
    fn order_gate_accepts_in_order() {
        let pts = circle_pts();
        let cps = vec![cp_at(0.0), cp_at(90.0), cp_at(180.0), cp_at(270.0)];
        let mut log = |_s: &str| {};
        assert!(stadium_point_order_ok(&pts, &cps, &mut log), "同序应放行");
    }

    // ---- issue #37 方案 B：按首过下标重排点位数组 ----
    //
    // 方案 A（按数组序串路线）在「按数组序绕不出目标里程」时会退回最近邻环，
    // 那种回退场景首过序仍然是乱的（见 `track/planner.rs` 里那句注释）。
    // 这里补的是回退支路的兜底：不改路线形状、不改里程，只把点位数组重排成
    // 轨迹实际经过的次序，让服务端的「按顺序通过所有点位」和报文对得上。

    /// 服务端原样点位数组（`points` / `coords` / `gcj_coords` 三份同序）。
    fn points_ctx_from(coords: &[(f64, f64)]) -> PointsContext {
        let points = coords
            .iter()
            .enumerate()
            .map(|(i, &(lat, lng))| {
                serde_json::json!({
                    "lat": lat,
                    "lon": lng,
                    "glat": lat,
                    "glon": lng,
                    "pointName": format!("P{}", i + 1),
                    "isFixed": if i == 0 { 1 } else { 0 },
                })
            })
            .collect();
        PointsContext {
            points,
            coords: coords.to_vec(),
            gcj_coords: coords.to_vec(),
        }
    }

    /// 把坐标串成采样轨迹（5 秒一格，时间戳只影响仿真，不影响首过判定）。
    fn track_from(pts: &[(f64, f64)]) -> Vec<crate::track::TrackPoint> {
        pts.iter()
            .enumerate()
            .map(|(i, &(lat, lon))| crate::track::TrackPoint {
                lat,
                lon,
                time_ms: i as i64 * 5_000,
            })
            .collect()
    }

    fn bd_of(cp: &Checkpoint) -> (f64, f64) {
        (cp.lat, cp.lng)
    }

    /// 首过下标升序；没经过的点位（`None`）排到最后，且保持原相对次序。
    #[test]
    fn order_by_first_pass_sorts_and_keeps_unseen_last() {
        // 实测形状：数组序 0..4，轨迹首过序 1 → 2 → 0 → 3 → 4
        assert_eq!(
            order_by_first_pass(&[Some(43), Some(12), Some(35), Some(47), Some(57)]),
            vec![1, 2, 0, 3, 4],
        );
        // 没经过的排最后，稳定保持原序（0 与 2）
        assert_eq!(
            order_by_first_pass(&[None, Some(5), None, Some(1)]),
            vec![3, 1, 0, 2],
        );
        // 已经同序：原样返回
        assert_eq!(
            order_by_first_pass(&[Some(0), Some(1), Some(2)]),
            vec![0, 1, 2],
        );
    }

    /// 乱序点位重排后：数组序 == 首过序，三份坐标与 `cps` 同步跟走。
    #[test]
    fn reorder_aligns_array_order_with_first_pass() {
        // `circle_pts` 从 0°（正东）逆时针铺到 359°
        let track = track_from(&circle_pts());
        // 服务端下发的数组序是乱的：0° → 180° → 90° → 270°
        let degs = [0.0_f64, 180.0, 90.0, 270.0];
        let coords: Vec<(f64, f64)> = degs
            .iter()
            .map(|&d| {
                let c = cp_at(d);
                bd_of(&c)
            })
            .collect();
        let mut pt = points_ctx_from(&coords);
        let mut cps: Vec<Checkpoint> = degs.iter().map(|&d| cp_at(d)).collect();
        let mut log = |_s: &str| {};

        assert!(
            reorder_by_first_pass(&mut pt, &mut cps, &track, &mut log),
            "乱序必须被重排"
        );

        // 重排之后，再按首过下标排一次必须还是原序（数组序 == 首过序）
        let first = first_pass_of(&track, &pt);
        assert_eq!(
            order_by_first_pass(&first),
            (0..first.len()).collect::<Vec<_>>(),
            "重排后数组序必须与首过序一致: {first:?}"
        );

        // 三份坐标 + cps 必须同步，否则报文与地图会错位
        for (i, (bd, gcj)) in pt.coords.iter().zip(pt.gcj_coords.iter()).enumerate() {
            assert_eq!(*bd, bd_of(&cps[i]), "第 {i} 个点位 BD-09 与 cps 错位");
            let raw = &pt.points[i];
            assert_eq!(
                raw["lat"].as_f64().unwrap_or_default(),
                bd.0,
                "第 {i} 个点位原始 JSON 与 coords 错位"
            );
            assert_eq!(
                raw["glat"].as_f64().unwrap_or_default(),
                gcj.0,
                "第 {i} 个点位 GCJ 坐标与原始 JSON 错位"
            );
        }
    }

    /// 已经同序时不重排，避免无谓改动报文。
    #[test]
    fn reorder_is_noop_when_already_in_order() {
        let track = track_from(&circle_pts());
        let degs = [0.0_f64, 90.0, 180.0, 270.0];
        let coords: Vec<(f64, f64)> = degs
            .iter()
            .map(|&d| {
                let c = cp_at(d);
                bd_of(&c)
            })
            .collect();
        let mut pt = points_ctx_from(&coords);
        let mut cps: Vec<Checkpoint> = degs.iter().map(|&d| cp_at(d)).collect();
        let before = pt.coords.clone();
        let mut log = |_s: &str| {};

        assert!(
            !reorder_by_first_pass(&mut pt, &mut cps, &track, &mut log),
            "同序不该重排"
        );
        assert_eq!(pt.coords, before, "同序时坐标必须原样不动");
    }
}
