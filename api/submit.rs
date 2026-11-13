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
use crate::api::obs::{civil_from_days, tz_offset_sec};

/// 两点间方位角（度，0=正北，顺时针）。
///
/// `bdD` 恒为 0 等于「这个点一直朝正北移动」，一条折线轨迹全程朝北是没法
/// 解释的。真机的 `bdD` 在整条轨迹里连续变化 0~360，跟着路径拐。
fn bearing_deg(from: (f64, f64), to: (f64, f64)) -> f64 {
    let (lat1, lat2) = (from.0.to_radians(), to.0.to_radians());
    let dlon = (to.1 - from.1).to_radians();
    let y = dlon.sin() * lat2.cos();
    let x = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * dlon.cos();
    ((y.atan2(x).to_degrees()) + 360.0) % 360.0
}

/// 定位时刻格式化成真机 `gainTime` 的 `YYYY-MM-DD HH:MM:SS`（本地时区）。
///
/// 日历换算复用 [`civil_from_days`]，时区复用 [`tz_offset_sec`]，两处都不再
/// 各自实现一份——时区规则要改的时候只改一个地方。
fn fmt_run_time(ms: i64) -> String {
    let secs = ms / 1000 + tz_offset_sec();
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, m, d, _) = civil_from_days(days, h);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

use crate::crypto::sign::{sport_signature, SignFields};
use crate::track::TrackPoint;

/// 默认体重（kg）。
///
/// 官方卡路里不是「每公里固定千卡」，而是 MET × 体重 × 时长：
/// 配速越慢，单位时间消耗越低，但单位距离消耗反而更高。用固定 62 kcal/km
/// 会让「慢跑 3 公里」和「快跑 3 公里」报出同一个卡路里，与同时上报的
/// 配速、步频自相矛盾（Neko/LightWhite520 的 `track/calorie.rs` 用的是
/// 官方分档公式，pace 这里对齐）。
pub const DEFAULT_WEIGHT_KG: f64 = 68.0;
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
    /// 海拔区间 `(最低, 最高)`，米。`None` 用默认 42±21m。
    ///
    /// 不同校区海拔差得远（沿海 5m、高原 2000m 以上），写死一个基准值
    /// 等于把「在哪跑」这件事暴露出来。给定区间后基准取中点、幅值按
    /// 半宽分配，生成的序列保证落在区间内。
    pub alt_range: Option<(f64, f64)>,
}

/// 打卡点（必到点）。
///
/// 坐标与名字对齐服务端 `points` 接口返回的原始结构：点位响应是
/// `{lon, lat, isFixed, pointName, glon, glat}`，服务端要靠这套字段把轨迹
/// 和它自己发的打卡点对上号；只回传 lat/lon 时它认不出是同一个点，
/// `按顺序通过所有点位` 就会判 False（实测 6 条全中）。
///
/// **这里只放「是哪个点」，不放任何序号类字段。** `position` / `id` /
/// `isFixed` / `flag` 全部由 [`fixed_point_items`] 统一发号，原因见那个函数
/// 的注释和 FIXED-POINT-PARITY.md：早先 `position` 由调用方填成「该点在轨迹
/// 数组里的首次经过下标」（实测 0/71/88/170/51），而真机是 0..N-1 的连续
/// 序号，App 详情页拿它排序/当索引，非连续值就只显示得出其中一部分点位。
#[derive(Debug, Clone)]
pub struct FixedPoint {
    pub lat: f64,
    pub lon: f64,
    pub point_name: String,
    pub glat: f64,
    pub glon: f64,
}

/// 必到点 `id` 的起始值。
///
/// 真机 rrid 1328288734 的五个必到点是 81/82/83/84/85 —— 与轨迹点的 id
/// （3516 起）完全不是一个量级，说明它是**必到点表自己的自增列**，不是从
/// 轨迹点 id 派生的。早先我们从 1 起发（1..5），比真机小两个数量级，是
/// 一条明显的差异；现在固定从 81 起连续递增。
pub const FIXED_POINT_ID_BASE: i64 = 81;

/// 「轨迹经过这个必到点」的判定半径（米）。
///
/// 与 `flow::dry_run` 的点位顺序自检用同一个阈值：GPS 抖动级别（个位数米）
/// 内就算经过，超过 10m 说明轨迹压根没走到那个点。
pub const FIXED_POINT_HIT_M: f64 = 10.0;

/// 每个必到点在轨迹点数组里**首次**被经过的下标（没经过的不出现在结果里）。
///
/// 这是纯内部量：`allLocJson` 的 `type` 字段要在经过打卡点的采样点附近标 2
/// （服务端 `validPoint` 打分靠点数密度），所以需要知道「落在第几个采样点」。
/// 它**不是** `fivePointJson` 的 `position`，两者语义不同，不能混用。
pub fn checkpoint_track_indexes(track: &[TrackPoint], coords: &[(f64, f64)]) -> Vec<usize> {
    coords
        .iter()
        .filter_map(|&pt| {
            // 取**最近**的采样点，不是「第一个进入半径的采样点」。
            //
            // 命中半径 10m、采样间距 5~15m，所以点位前面那个点也常常落在
            // 半径内：取第一个的话，标记会整体往前挪一个采样点（实测点位在
            // 第 25 点、却被标到第 24 点）。真机是一点一个标记，标到的必须
            // 是真正经过它的那个点。
            let mut best: Option<(f64, usize)> = None;
            for (i, t) in track.iter().enumerate() {
                let d = crate::track::geo::distance((t.lat, t.lon), pt);
                if d >= FIXED_POINT_HIT_M {
                    continue;
                }
                if best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, i));
                }
            }
            best.map(|(_, i)| i)
        })
        .collect()
}

/// 必到点 → `fivePointJson` 的内层点位数组（真机形状）。
///
/// 逐字段对齐真机 rrid 1328288734（`evidence/obs_real_20261002.json`）：
///
/// ```text
/// position 0 丁香园正门   id=81 isFixed=1 isPass=true state=0 flag=1790913882164
/// position 1 梅园2栋西   id=82 isFixed=0 isPass=true state=0 flag=1790913882164
/// ...        position 4 丁香2栋西南 id=85 isFixed=0 isPass=true state=0 flag=...
/// ```
///
/// 三条硬规则，都是「填错了 App 详情页就只显示一部分点位」：
///
/// 1. `position` = **0..N-1 的连续序号**，按轨迹实际途经顺序编号；数组也按
///    同一个顺序排，使「数组下标 == position」。早先这里填的是该点的首次
///    经过轨迹下标（0/71/88/170/51），非连续也不递增，App 排序/取下标直接
///    错位。
/// 2. `isFixed` 只有**第一个**点（position 0，也就是起跑后先到的那个必到点）
///    = 1，其余 0。真机五个点里只有首点 1，服务端 `isFixed` 的原值（我们这
///    批点位里落在第 4 个点上）不是 App 期望的形态。
/// 3. `id` 从 [`FIXED_POINT_ID_BASE`]（81）起连续递增；`flag` 全场同值，
///    等于本场起始毫秒戳（上一轮已对齐，别改回逐点时间）。
///
/// `isPass` 用「轨迹是否真的经过它」判断：真机五个点全经过，所以全 true；
/// 我们按真实匹配结果填，没经过的点标 false（同时 `flow` 的点位顺序自检
/// 会把这种情况打出来），不再无条件发 true。
pub fn fixed_point_items(fixed: &[FixedPoint], track: &[TrackPoint], flag_ms: i64) -> Vec<Value> {
    // 每个点的首次经过下标；None = 轨迹没经过它
    let hits: Vec<Option<usize>> = fixed
        .iter()
        .map(|f| {
            track
                .iter()
                .position(|t| {
                    crate::track::geo::distance((t.lat, t.lon), (f.lat, f.lon)) < FIXED_POINT_HIT_M
                })
        })
        .collect();

    // 途经顺序 = 首次经过下标升序；没经过的点按调用方给的顺序排到最后。
    // 排序必须稳定（`sort_by` 是稳定排序），这样同下标/同未经过的点之间
    // 保持原序，报文可复现。
    let mut order: Vec<usize> = (0..fixed.len()).collect();
    order.sort_by(|&a, &b| match (hits[a], hits[b]) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.cmp(&b),
    });

    order
        .iter()
        .enumerate()
        .map(|(position, &i)| {
            let f = &fixed[i];
            // glat/glon 缺失（0）时由 BD-09 反算：真机两套坐标都是真值，
            // 发 0 等于告诉服务端「这个点没有 GCJ 坐标」，点位可能被丢掉。
            let (glat, glon) = if f.glat != 0.0 && f.glon != 0.0 {
                (f.glat, f.glon)
            } else {
                crate::api::coord::bd09_to_gcj02(f.lat, f.lon)
            };
            // 键序按真机那样字母序排（serde_json 开了 preserve_order，顺序
            // 由宏里的书写次序决定），与真机样本逐字节一致。
            json!({
                "flag": flag_ms,
                "glat": glat,
                "glon": glon,
                "id": FIXED_POINT_ID_BASE + position as i64,
                "isFixed": if position == 0 { 1 } else { 0 },
                "isPass": hits[i].is_some(),
                "lat": f.lat,
                "lon": f.lon,
                "pointName": f.point_name,
                "position": position as i64,
                "state": 0,
            })
        })
        .collect()
}

/// `fixed_point_json` envelope 里的 `runAreaId` 该发什么。
///
/// 真机 rrid 1328288734 这份对象（`evidence/obs_real_20261002.json` 的
/// `fixed_point_json`）里是 `-1`，真实围栏 id（1668）**只**出现在
/// `geoFencesJson[0].id` 里。我们原来发真实 id（1668），与真机不一致。
///
/// 影响面：这个字段只决定报文长相——
///
/// - 围栏**匹配**不靠它：选中哪个围栏由 `fence::select_by_anchor` 按锚点
///   落在哪个环内决定，`SelectedFence::run_area_id` 只是往下游当报文值；
/// - 提交报文顶层那个 `runAreaId`（`save_record`）**不动**，仍发真实 id：
///   那是另一个接口的字段，手里没有真机样本证明它也发 -1，改它等于拿一次
///   真实提交去赌。要改得等抓到真机 `save/record` 的 body。
///
/// 真机样本缺这一项、需要发真实 id 做对照实验时设 `PACE_RUN_AREA_ID=real`。
pub fn payload_run_area_id(real: i64) -> i64 {
    match std::env::var("PACE_RUN_AREA_ID").ok().as_deref() {
        Some("real") => real,
        _ => -1,
    }
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
    /// 点位数组（`allLocJson` 那份，28 字段/点）。报文里 `allLocJson` 现在发
    /// 空串（真机/Neko 都这样，轨迹只走 OBS），所以派生字段自检从这里取点。
    ///
    /// **与自检、OBS 明细是同一份实例**：由 [`build_points`] 造一次，
    /// [`build_from_points`] 原样收下，`flow` 直接交给明细上传。
    pub points: Vec<Value>,
}

/// 单点速度上限（m/s）。
///
/// 只用来兜 GPS 抖动（世界纪录配速的两倍还多），**不是**业务下限/上限：
/// 真机这条记录 `avgSpeed` 实测 0.06~6.21 m/s，慢速点（过弯、避让）本来就
/// 该低到 0.2 以下。早先那个 `clamp(1.9, 5.0)` 把慢速点抬快、快速点压慢，
/// 配速曲线因此整条失真。
pub const MAX_POINT_SPEED_MPS: f64 = 6.3;

/// 段表（`segment_json`）里允许的最短段时长（秒）。
///
/// 真机的段表是「起步段 + 20 秒若干 + 末段」，最短的一段也远大于 3 秒；
/// 而 `(总时长 - 起步段) % 20 == 1` 时会切出一个 1 秒段（实测 5km/1800s
/// 的末段就是 1.0 秒 / 3.8 米），肉眼可辨。短的往上一窗借，借不动就留着。
pub const MIN_SEGMENT_SEC: i64 = 3;

/// `stepDistance` 的整场常量（语义未定，取值形态与真机一致）。
///
/// 真机 114 点的 `stepDistance` 唯一值集合只有 `{0.0, 3.7468809127807674}`：
/// 起步窗口 0，其余**全点同一个值**。这个 3.7469 与 670 步 / 1683.83m /
/// 449.4 都构不成简单关系（`AUDIT-round.md` C4），既不是单步步幅
/// （真机 m/步 = 1683.83/670 = 2.51）也不是累计均值，所以只能照抄**形态**：
///
/// - 整场一个常量（不是逐点的「本段位移/本段步数」——那个形态就不对，
///   逐点在 0~3.9 之间来回跳）；
/// - 量级落在真机同带（3.62~3.88，真机 3.7469 在带的中间）。
///
/// 用 `start_ms` 派生而不是 `seed`：点位此前有两条构造链、seed 各不相同
/// （`AUDIT-round.md` A4），而 `start_ms` 在两条链上是同一个值——那时靠它
/// 才没让 `stepDistance` 也分成两套。点位现在已收敛到 [`build_points`]
/// 单一来源（两条链同一个 seed），但 `start_ms` 与「本场」的关联更直接，
/// 保持不变。
fn step_distance_const(start_ms: i64) -> f64 {
    let h = (start_ms as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0x2545_F491_4F6C_DD1D);
    let f = (h >> 40) as f64 / 16_777_216.0; // [0, 1)
    ((3.62 + f * 0.26) * 10_000.0).round() / 10_000.0
}

/// 单个轨迹点在 `allLocJson` 里的完整结构。
///
/// 字段名与语义对齐服务端：`lat`/`lng` 是**占位**（固定 -1.0），真实坐标走
/// `gLat`/`gLng` 且必须是 gcj02。
///
/// 三个速度字段的口径由真机 114 点逐点反推（见 `FIX-A2-SPEED-UNITS.md`）：
/// - `speed` 是**配速 min/km**（不是 km/h），恒满足 `speed × avgSpeed = 1000/60`；
/// - `avgSpeed` 是**点间速度 m/s**（本点到上一点），不是累计均值；
/// - `bdS` 是**低通后的点间速度 m/s**（比 `avgSpeed` 慢一拍），不是 `speed/3.6`。
///
/// 逐字段取值以真机样本 `evidence/obs_real_20261002.json`（114 点 / 555 秒 /
/// 1683.8m）为准：`flag` 全点同值、`lat`/`lng` 恒 -1.0、`state` 是
/// 运动/暂停/收尾三态（1/2/3）、`type` 含 -1 灰点、`count` 40~53、
/// `radius` 1.4~2.3、`queueNum` 0..24、没有 `steps` 字段。
#[allow(clippy::too_many_arguments)]
pub fn point_json_full(
    p: &TrackPoint,
    idx: usize,
    n: usize,
    startup: usize,
    start_ms: i64,
    _start_pt: (f64, f64),
    prev: &mut (f64, f64),
    cum: &mut (f64, i64, i64),
    prev_time: &mut i64,
    prev_v: &mut f64,
    _total_steps: i64,
    seed: u64,
    checkpoint_idxs: &[usize],
    alt_range: Option<(f64, f64)>,
) -> Value {
    // BD-09 → GCJ-02（原点是 BD-09，服务端要 gcj02）
    let (glat, glng) = crate::api::coord::bd09_to_gcj02(p.lat, p.lon);

    // 累计量
    let prev_pt = *prev; // 上一点坐标，算航向用（下面 *prev 会被覆盖）
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

    // 起步窗口（GPS 刚锁定、位移被过滤的那 15%）：真机这段的 speed / bdS /
    // avgSpeed / stepDistance 全是 0，是「还没进测速窗口」而不是「速度为 0」。
    //
    // `startup` 由调用方按**时间戳**数出来（`track::startup_point_count`），
    // 与几何压缩、`segment_json` 首段用同一个窗口；这里不再自己按点数比例
    // 估一遍，否则三条链会各切各的。
    let in_startup = idx < startup;

    // ── speed / avgSpeed / bdS 的口径 ───────────────────────────────────────
    //
    // 三个字段的口径是拿真机 114 点逐点反推出来的（`FIX-A2-SPEED-UNITS.md`
    // 有残差表），不是照抄谁的注释：
    //
    //   v        = ΔtotalDis / ΔtotalTime              （点间速度，m/s）
    //   avgSpeed = v                                   （中位残差 0.0007，n=88）
    //   speed    = 1000 / (60·v)                        （配速 min/km：把 v 代回
    //                                                     这条式子，88 个点的
    //                                                     中位残差 0.049%）
    //   bdS      = 设备端低通后的 v（m/s；残差与 Δv 的相关系数 −0.94，
    //             也就是它比 avgSpeed 慢一拍，不是另一个量纲）
    //
    // 旧实现三处都错，一起改：
    //   ① `speed` 发的是 `v × 3.6`（km/h）。真机是配速：idx60 v=3.28m/s、
    //      真机 speed=5.08，而 `v*3.6`=11.81 —— 差 2.3 倍且方向相反
    //      （越慢配速越大）。注意「speed × avgSpeed ≈ 16.667」只在两边都
    //      不四舍五入时成立：真机发布值是 2 位小数，乘积会散到 15.6~17.0，
    //      所以判据要用 `speed ?= 1000/(60·avgSpeed)` 而不是乘积。
    //   ② `avgSpeed` 发的是 `cum.0/dt`（累计均值），全程被压成 ≈3.0 一条
    //      直线；真机是逐点起伏的点间速度（idx42 = 0.38，同一时刻累计均值
    //      是 2.5，两者差 6 倍）。
    //   ③ `bdS == speed/3.6` 这个「天然不变量」是把①的错口径写成了定理。
    //
    // 起步窗口（真机前 18 点）三项都是**硬 0**：窗口内 Δdis 其实有
    // 0.16~0.8 m/s 的 GPS 漂移（真机 idx5/idx11/idx14 都非零），真机仍然
    // 写 0 —— 是「还没进测速窗口」，不是「配速无穷大被截断成 0」。
    // 所以这里不能「先算配速再取 0」，窗口必须在算 v 之前就短路。
    let v_mps = if idx == 0 || in_startup {
        0.0
    } else {
        // 上限 6.3 m/s 只兜 GPS 抖动（世界纪录配速的两倍还多）。**不设下限**：
        // 真机 v 实测低到 0.06 m/s（idx113 配速 260.42 min/km），早先那个
        // 1.9 m/s 的下限会把配速压死在 8.77 以内，慢速点全部被抬快。
        (step_m / gap_sec).clamp(0.0, MAX_POINT_SPEED_MPS)
    };
    // 配速（min/km）。v=0 时真机写 0.0（不是 ∞、也不是被截断的大数）：
    // idx113 是唯一一个 v=0 却非 0 的点，那是设备端滤波残差（0.06 m/s），
    // 我们不模拟滤波器残差。
    let pace = if v_mps > 0.0 {
        1_000.0 / (60.0 * v_mps)
    } else {
        0.0
    };
    // `bdS`：两拍低通（本拍与上一拍点间速度的均值）。真机这个量是设备端
    // 滤波输出：加速段 bdS < v、减速段 bdS > v，单点最大残差 2.0 m/s；
    // 换成两拍均值后中位残差 0.19 m/s、p95 0.53 m/s（见报告残差表）。
    // 窗口后的第一点没有上一拍（`prev_v` 还是窗口里的 0），直接取本拍。
    let bd_s = if v_mps <= 0.0 {
        0.0
    } else if *prev_v <= 0.0 {
        v_mps
    } else {
        (v_mps + *prev_v) / 2.0
    };
    *prev_v = v_mps;

    // 点位类型是有语义的，不是装饰。
    //
    // 老版本按概率瞎填（38% 得 3、54% 得 0、3% 得 1、2% 得 2），纯粹凑分布。
    // 真机的 isValidPoint（libswsport.so@0x10e84，XXXXBully 反汇编还原成
    // isvalidpoint.py:130-181）对它打分：type=1 得 30 分、type=2 得 20 分、
    // 其他得 15 分，及格线 65。也就是说「这个采样点算不算有效打卡点」直接
    // 由 type 决定，瞎填等于每个点都拿不到类型分。
    //
    //   0  普通采样（GPS 正常）
    //   1  ——（真机 114 点里 type=1 出现 0 次，已不再生成）
    //   2  次一级打卡点 / 弱定位
    //   5  起步阶段的弱定位点
    //   6  终点
    //   7  / -1  灰点：GPS 弱，跳变检查豁免
    //
    // 真机分布：{0: 89, -1: 17, 2: 5, 5: 1, 6: 1, 7: 1}（共 114 点）。
    // 和老实现的三处差异：起点是 7 不是 0、第二个点是 5；起步段（GPS 刚
    // 锁定、位移被过滤）整段是 **-1**；全程不出现 1，打卡位置改用 2 承载。
    let ptype = if idx == 0 {
        7
    } else if idx + 1 == n {
        6
    } else if idx == 1 {
        5
    } else if idx < startup {
        // 起步灰点：真机第 3~18 个点全是 -1，对应 86 秒 GPS 漂移段
        -1
    } else if checkpoint_idxs.contains(&idx) {
        // 打卡点**只标命中的那一个采样点**，不撒窗口。
        //
        // 真机 114 个点里 type=2 恰好 5 个，而记录里有 5 个打卡点——
        // 一点一个，没有 `idx±2` 那种连续标记。早先按 ±2 撒窗口，5 个点位
        // 能撒出 31 个 type=2（实测），是真机的 6 倍，等于自己给自己加了一组
        // 说明「这不是真机」的特征。
        //
        // 撒窗口的初衷是「怕服务端按范围找点时密度太低」，但那是拿猜测压过
        // 实测：真机样本本身就是一点一个，服务端认它。
        2
    } else {
        // 普通采样点一律 0。真机的 {0: 89} 里没有任何随机 2，
        // 早先按 5% 撒 2 会把 type=2 的总数顶到真机的两倍。
        0
    };

    // GPS 精度半径：真机 1.4~2.3m（众数 1.5~2.0）。早先按 3.6~4.3 生成，
    // 等于「每次定位都差四米」，是明显的机器指纹。
    //
    // 哈希里必须带 idx：只用 lon 的话，一条南北向轨迹全程经度不变，
    // radius 就成了常量（实测整条 2.3 一个值），反而更好认。
    let h2 = ((p.lon.to_bits() ^ seed).wrapping_add((idx as u64).wrapping_mul(0x9E3779B97F4A7C15)))
        .wrapping_mul(6364136223846793005);
    let radius = ((1.4 + ((h2 >> 40) % 10) as f64 * 0.1) * 100.0).round() / 100.0;

    // 卫星数：真机 40~53（众数 46~49）。早先 10~34 整体偏小，等于「只锁定
    // 十几颗星」，而这条真机记录在开阔校园里一直在 46 颗上下。
    // 逐点按序号伪随机，同 seed 可复现。
    let h3 = (idx as u64).wrapping_mul(0x2545F4914F6CDD1D) ^ seed;
    let count = 40 + ((h3 >> 32) % 13) as i64;

    // 航向角：首点没有上一点，记 0；其余按大圆方位角算真实朝向。
    let bearing = if idx == 0 {
        0.0
    } else {
        bearing_deg(prev_pt, (p.lat, p.lon))
    };

    // 定位时刻（可读串）。恒空串等于「这些点没有定位时间」。
    let gain_time = fmt_run_time(p.time_ms);

    // 海拔。
    //
    // 真机这条记录（齐齐哈尔，平原校园）的 `bdA` 实测 142.4~158.5m，首点
    // 158.0、末点 156.3，全程起伏 ±8m——是气压计/GPS 高程的传感器噪声，
    // 不是地形。所以在 145m 量级的基准上叠三条不同周期的正弦。
    //
    // 早先的默认基准是 42±21m（沿海取值），而调用方一旦按别处的海拔传一个
    // 区间进来（比如 2502~2640），整条轨迹的 `bdA` 就飞到两千米以上，和
    // 坐标所在的齐齐哈尔差一个数量级，`laps_json` 的 `endAltAbs` 也跟着错。
    // 现在默认值改成真机实测区间。
    //
    // 幅值分配的真实依据（`evidence/obs_real_20261002.json` 逐点算出来的）：
    // 相邻点 |ΔbdA| 均值 1.13m、中位 0.8m、p90 2.9m、最大 4.7m，全程累计
    // 爬升 62.99m / 下降 64.69m，也就是**每公里约 37m 爬升**。早先按「相邻点
    // 只能动几十厘米」估的（那相对我们是 6.7m/km），爬升只有真机的 1/5，
    // 圈数据的 `elevationGain` 跟着偏小 4 倍。
    //
    // 现在按同一组统计量配：慢项管整条十几米的漂移，中项给 0.3rad/点的
    // 起伏，快项给接近 1rad/点的高频抖动——三项叠加后相邻点平均变化
    // ≈1.2m、最大 ≈2.5m，累计爬升 ≈38m/km，与真机同量级。
    // 相位由 seed 定，同 seed 可复现。
    let ph = |k: u64| {
        ((seed ^ k).wrapping_mul(0x9E3779B97F4A7C15) >> 40) as f64 / 16_777_216.0
            * std::f64::consts::TAU
    };
    let fi = idx as f64;
    // 幅值分配：慢项 45% 管整条十几米漂移，中项 30%、快项 25% 管逐点抖动。
    // 三项加起来正好等于半宽，理论极值就是区间边界，所以 clamp 几乎不触发
    // ——clamp 一旦触发会在边界压出一排平点，那才是明显的假。
    let (base, half, lo, hi) = match alt_range {
        Some((lo, hi)) if hi > lo + 1.0 => {
            ((lo + hi) / 2.0, (hi - lo) / 2.0, lo, hi)
        }
        // 默认：真机 `bdA` 实测 142.4~158.5，取 150 ± 8
        _ => (150.0, 8.0, 142.0, 158.0),
    };
    // 噪声项是**传感器噪声**，绝对量级（±2.4m / ±2.0m），不随海拔区间放大：
    // 传一个 200~260m 的高原区间进去时，地形漂移该变大，但气压计抖动不会
    // 跟着变成 ±8m——那会让相邻点跳近 20 米，比真机假得多。
    let mid_amp = (half * 0.30).min(2.4);
    let fast_amp = (half * 0.25).min(2.0);
    let altitude = (base
        + half * 0.45 * (fi * 0.02 + ph(1)).sin()
        + mid_amp * (fi * 0.30 + ph(2)).sin()
        + fast_amp * (fi * 0.95 + ph(3)).sin())
    .clamp(lo, hi);

    // `stepDistance`：真机是**整场一个常量**（唯一值集合
    // `{0.0, 3.7468809127807674}`：起步窗口 0、其余全点同值）。
    //
    // 早先这里发 `d_seg / seg_steps`（本段位移 / 本段步数），理由是「真机
    // 这是局部量」——凭形态猜语义，猜反了：逐点 0~3.9 来回跳的锯齿本身就是
    // 真机里不存在的形态。真机那个常量的语义没有证据（取值与步数、里程都
    // 不构成简单关系），只能照抄形态，见 [`step_distance_const`]。
    let step_distance = if in_startup {
        0.0 // 真机起步窗口 stepDistance 恒 0（位移被过滤，没有步幅可算）
    } else {
        step_distance_const(start_ms)
    };

    // 收尾段起点：真机第 83 个点（totalTime 408s，占全程 73%）起 state 变成 3
    let tail = if n >= 20 { ((n as f64) * 0.73) as usize } else { n };
    // 自动暂停点：真机是收尾转折点前连着的几个点（第 75~78、82 个点）
    let paused = n >= 20 && idx + 4 >= tail && idx < tail;

    json!({
        // 点间速度（m/s）：本点到上一点的位移 / 采样间隔。真机就是这么算的
        // （触类：idx42 真机 0.38 = 1.884m/5s），不是累计均值。
        "avgSpeed": (v_mps * 10000.0).round() / 10000.0,
        // 海拔三兄弟不能是 0：服务端按它们算累计爬升，全 0 等于「在海底跑」
        "bdA": (altitude * 10.0).round() / 10.0,
        "bdD": (bearing * 10.0).round() / 10.0,
        "bdG": 1,
        // 低通后的点间速度（m/s），两拍均值。真机 2 位小数，且**恒不等于**
        // `speed/3.6`（那是把配速当 km/h 的错口径）。
        "bdS": (bd_s * 100.0).round() / 100.0,
        "coorType": "gcj02",
        "count": count,
        "dtr": 0.0,
        // flag 是**整场跑步的起始毫秒戳**，不是每个采样点各自的定位时刻。
        //
        // 真机 114 个点的 flag 全部相等（1790913882164，跨度 0），
        // segment_json / speed_json / step_freq_json / laps_json /
        // fixed_point_json 里也是同一个值。时间进度另外由 totalTime
        // （累计秒）、gainTime（"YYYY-MM-DD HH:MM:SS"）、beginTime/endTime
        // （毫秒）表达，不需要 flag 再承担一份。
        //
        // 早先这里发 p.time_ms、逐点递增，和真机正好相反：真机是整场一个
        // 常量，递增反而成了指纹。
        "flag": start_ms,
        "gLat": (glat * 1e7).round() / 1e7,
        "gLng": (glng * 1e7).round() / 1e7,
        "gainTime": gain_time,
        // 服务端自增 id 的量级：真机这条记录的点是 3516~3629，连续递增。
        "id": id_base(seed, 1200) + 3000 + idx as i64,
        // lat/lng 是**占位 -1.0**，真实坐标只走 gLat/gLng（GCJ-02）。
        //
        // 真机 114 个点无一例外都是 -1.0 / -1.0。这一条曾经被反着改过一次
        // （当时的理由是「服务端按 lat/lng 判点位」），但真机对象就是 -1，
        // 想和真机逐字段对齐只能填 -1；判点位的坐标取 gLat/gLng。
        "lat": -1.0,
        "lng": -1.0,
        "locType": 1,
        "locationId": "",
        // 排队序号：真机 0..24，起步段（前 `startup` 个点）全 0，之后
        // **每 4 个点 +1** 的等宽台阶到 24（`19→1、23→2、…、103→22`）。
        // 不是按位置比例插值：那样是 4.09 个点一档，会一路累积漂移，
        // 审计点名 14/114 个下标恒为真机 +1（AUDIT-round.md A5）。
        "queueNum": crate::track::queue_num(idx, n, startup),
        "radius": (radius * 100.0).round() / 100.0,
        // **配速 min/km**，不是 km/h。真机 114/114 点满足
        // `speed × avgSpeed = 1000/60 = 16.6667`（idx60: 5.08 × 3.28），
        // 这是判「单位有没有认反」唯一可靠的等式。
        "speed": (pace * 10000.0).round() / 10000.0,
        // state 是「运动 / 暂停 / 收尾」三态：1 运动中、2 自动暂停、3 收尾。
        //
        // 真机实测 {1: 78, 2: 5, 3: 31}：31 个 3 从第 83 个点一直覆盖到最后
        // （收尾阶段），5 个 2 紧挨在这个转折点之前（自动暂停）。
        // 早先把它当「速度合规三态」用（0 合规 / 1 慢于下限 / 2 快于上限），
        // 和真机语义完全不同——真机 114 个点里 state=0 出现 0 次。
        "state": if idx >= tail { 3 } else if paused { 2 } else { 1 },
        // 局部步幅 = 本段位移 / 本段步数。
        //
        // 原来是 ((cum.0 / cum.0.max(1.0)) * (cum.0 / total_steps) ...)——
        // 进这分支的前提就是 cum.0 > 1.0，所以第一个因子恒等于 1.0，
        // 整个表达式化简成「累计里程 / 全程步数」，是个随点序单调增长的
        // 累积平均值，不是采样点的即时步幅。真实 App 这里是局部量。
        "stepDistance": step_distance,
        "totalDis": (cum.0 * 10000.0).round() / 10000.0,
        "totalTime": cum.1,
        "type": ptype,
        "validDis": (cum.0 * 10000.0).round() / 10000.0,
        "validTime": cum.1,
        // 真机点里**没有** steps 字段（步数只出现在 segment_json 的
        // avgStep / laps_json 的 step 里），这里不再多发一个。
    })
}

/// 服务端自增 id 的起点。
///
/// 真机这条记录：点 3516~3629、段 733~757、速度/步频桶 1628~1682，都是
/// 服务端 DB 的连续自增列，客户端不参与发号。我们发出去的值只要落在同一
/// 量级、且同一条记录内连续递增就不会显眼，所以按起始时间派生一个稳定
/// 偏移（同一条记录内确定、不同记录不撞）。
fn id_base(seed: u64, span: i64) -> i64 {
    let span = span.max(1) as u64;
    ((seed ^ seed.rotate_left(17)) % span) as i64
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
pub fn verify_view(body: &Value, pts: &[Value]) -> Result<VerifyView, String> {
    // 报文里的 allLocJson 现在是空串（轨迹只走 OBS），点位由调用方传进来
    let start_ms = body
        .get("startTime")
        .and_then(|v| v.as_i64())
        .or_else(|| {
            pts.first()
                .and_then(|p| p.get("flag"))
                .and_then(|f| f.as_i64())
        })
        .ok_or("报文缺 startTime")?;
    let stop_ms = body.get("stopTime").and_then(|v| v.as_i64()).unwrap_or(0);

    let arr: Vec<Value> = pts.to_vec();
    if arr.is_empty() {
        return Err("点位列是空的".into());
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
    // 真机点位里没有 steps 字段（步数只在 segment_json / laps_json 里），
    // 所以总步数优先从报文顶层的 totalSteps 取，占位点位上带的 steps 只作兜底。
    let total_steps = last
        .get("steps")
        .and_then(|v| v.as_i64())
        .or_else(|| body.get("totalSteps").and_then(|v| v.as_i64()))
        .ok_or("报文缺 totalSteps，点位也没有 steps")? as i32;

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

/// 官方卡路里分档：由配速（分/公里）取 MET。
///
/// 三个分段来自运动世界官方换算（Neko 家族 `LightWhite520/src/track/calorie.rs`
/// 与 `blackpineapple` 两处独立实现一致）：
///
/// | 配速 p（分/公里） | MET |
/// |---|---|
/// | p < 10（快于 6:00/km） | 33/p + 4.1 |
/// | 10 ≤ p < 12 | 102/p − 3.7 |
/// | p ≥ 12（慢于 5:00/km） | 33.6/p + 1 |
pub fn met_for_pace(pace_min_per_km: f64) -> f64 {
    if !(pace_min_per_km.is_finite()) || pace_min_per_km <= 0.0 {
        return 0.0;
    }
    if pace_min_per_km < 10.0 {
        33.0 / pace_min_per_km + 4.1
    } else if pace_min_per_km < 12.0 {
        102.0 / pace_min_per_km - 3.7
    } else {
        33.6 / pace_min_per_km + 1.0
    }
}

/// 卡路里（千卡）= 体重 × MET × 小时数。
pub fn calorie_for(distance_m: f64, time_sec: i64, weight_kg: f64) -> i64 {
    if distance_m <= 0.0 || time_sec <= 0 || weight_kg <= 0.0 {
        return 0;
    }
    let pace = (time_sec as f64 / 60.0) / (distance_m / 1000.0);
    let met = met_for_pace(pace);
    let hours = time_sec as f64 / 3600.0;
    (weight_kg * met * hours).round() as i64
}

/// 平均功率（W）= 1.1 × 体重 × 速度(m/s)，保留一位小数。
///
/// 真机有这个字段且非零；发 `0` 等于自称「全程不做功」，与 pace、步频、
/// 卡路里三项都对不上。乘 1.1 是官方的体重→功率经验系数。
pub fn avg_power_for(distance_m: f64, time_sec: i64, weight_kg: f64) -> i64 {
    if distance_m <= 0.0 || time_sec <= 0 || weight_kg <= 0.0 {
        return 0;
    }
    let v = distance_m / time_sec as f64;
    // **必须取整**。服务端 avgPower 是整型（参考实现 `RunDetail.avg_power: i64`），
    // 发 187.5 时它反序列化截断成 187，再拿 187 重算签名，而我们的签名串里
    // 是 "187.5" —— 两边拼出来的串不同，直接 10501「验证签名失败」。
    // 这种错 body 内部完全自洽、本地 audit 也查不出来，只有真发一次才暴露。
    (1.1 * weight_kg * v).round() as i64
}

/// 从**已组装好的点位数组**算累计爬升（米）。
///
/// 必须用报文里的 `bdA` 序列而不是另算一套海拔：服务端按相邻采样点的
/// 海拔差累计爬升，若上报值与点位序列不一致，`totalAscent` 与轨迹直接
/// 互相矛盾——而 pace 原来恒定发 0，等于「一条有起伏的轨迹零爬升」。
pub fn total_ascent_from_points(pts: &[Value]) -> i32 {
    let ser: Vec<(f64, f64)> = pts
        .iter()
        .filter_map(|p| {
            let a = p.get("bdA").and_then(|v| v.as_f64())?;
            let d = p.get("totalDis").and_then(|v| v.as_f64()).unwrap_or(0.0);
            Some((d, a))
        })
        .collect();
    if ser.len() < 2 {
        return 0;
    }
    // 逐点累加正增量会报 100m+，真机记录只有 17~22m——App 把气压计噪声
    // 滤掉才算爬升。这里先做 ±100m 窗口滑动平均（10m 采样下约 21 个点）
    // 再累加正增量。这个窗口是拿真机数据拟合出来的：算出 18/14/18/18
    // 对真实 22/17/20/22，误差最小。
    const HALF_WIN_M: f64 = 100.0;
    let smooth: Vec<f64> = ser
        .iter()
        .map(|&(d0, _)| {
            let (mut s, mut n) = (0.0, 0.0);
            for &(d, a) in &ser {
                if (d - d0).abs() <= HALF_WIN_M {
                    s += a;
                    n += 1.0;
                }
            }
            s / n
        })
        .collect();
    let sum: f64 = smooth.windows(2).map(|w| (w[1] - w[0]).max(0.0)).sum();
    sum.round() as i32
}

/// 点位里 `speed` / `avgSpeed` / `bdS` 的口径一致化。
///
/// **不是**给 `speed` 做平滑。原来的实现是「前后各 5 点的窗口均速」，理由是
/// 「手机 GPS 速度是多普勒测速、本身平滑」——这个前提被真机数据否掉了：
///
/// - 真机 `speed` 完全没有被平滑：idx42 单点配速 **44.09 min/km**，相邻两点
///   是 2.2 / 1.5 m/s 的量级。锯齿就在报文里，因为它就是「本点位移 / 间隔」
///   折算出来的配速；
/// - 真机 `bdS` 才是滤波量，而且只有**两拍**的滞后（残差与 Δv 的相关系数
///   −0.94）。±5 中心窗口会把它抹成一条几乎不变的线，还会让窗口内的 `bdS`
///   领先于实际速度，与真机的滞后方向相反。
///
/// 所以这里改成：从报文自身的 `totalDis`/`totalTime` 反算点间速度 `v`，再按
/// [`point_json_full`] 的同一套公式写回三个字段
/// （`speed` = 配速 min/km、`avgSpeed` = v、`bdS` = 两拍低通），起步窗口三项
/// 归 0。函数名保留是为了不动调用方；它是**幂等**的（跑两遍结果不变），
/// 因此谁再对同一份点位调第二遍也不会漂。点位现在由 [`build_points`]
/// 单点构造（自检 / 提交 / OBS 明细共用），这里只跑一遍就够了。
pub fn smooth_point_speed(items: &mut [Value]) {
    let n = items.len();
    if n < 3 {
        return;
    }
    let dis: Vec<f64> = items
        .iter()
        .map(|p| p["totalDis"].as_f64().unwrap_or(0.0))
        .collect();
    let tim: Vec<f64> = items
        .iter()
        .map(|p| p["totalTime"].as_f64().unwrap_or(0.0))
        .collect();
    // 起步窗口不参与：真机这一段 speed / bdS / avgSpeed 恒 0（GPS 未锁定），
    // 反算会把窗口内那点漂移折算成非 0 速度。
    //
    // 窗口按时间戳数（与 `point_json_full` 的 `startup` 同一口径），这里手上
    // 的对象就是报文里那几个字段，直接读 totalTime 现算。
    let su = {
        let times: Vec<i64> = items
            .iter()
            .map(|p| p["totalTime"].as_i64().unwrap_or(0) * 1000)
            .collect();
        let total_sec = items
            .last()
            .and_then(|p| p["totalTime"].as_i64())
            .unwrap_or(0);
        crate::track::startup_point_count(&times, 0, total_sec)
    };
    let mut prev_v = 0.0f64;
    for (i, item) in items.iter_mut().enumerate().take(n) {
        let v = if i == 0 || i < su {
            0.0
        } else {
            let dt = tim[i] - tim[i - 1];
            if dt > 0.0 {
                ((dis[i] - dis[i - 1]) / dt).clamp(0.0, MAX_POINT_SPEED_MPS)
            } else {
                0.0
            }
        };
        let pace = if v > 0.0 { 1_000.0 / (60.0 * v) } else { 0.0 };
        let bd_s = if v <= 0.0 {
            0.0
        } else if prev_v <= 0.0 {
            v
        } else {
            (v + prev_v) / 2.0
        };
        prev_v = v;
        item["speed"] = json!((pace * 10000.0).round() / 10000.0);
        item["avgSpeed"] = json!((v * 10000.0).round() / 10000.0);
        item["bdS"] = json!((bd_s * 100.0).round() / 100.0);
    }
}

/// 从已组装好的点位数组切圈（每 1000m 一圈，末圈 `isFullLap=false`）。
///
/// 结构对齐真机 OBS `laps_json`（14 个字段，见 Neko `track/wire.rs::build_laps`）：
/// `avgPace` 用十进制分钟/公里、`avgStride` 用厘米、`elevationGain` 按圈内
/// 正增量累计、`endAltRel` 相对首点海拔。
///
/// pace 原来发空数组。真机跑 2 公里必有 2 圈，空数组等于「这条轨迹没有
/// 分段信息」，而服务端的分段审核、圈配速展示都读这一列。
///
/// `total_steps` 是全程步数：真机点位里**没有** `steps` 字段，所以圈步数
/// 不能再从点位反解，改成按「圈内里程 / 全程里程 × 全程步数」分配，
/// 这样各圈 `step` 之和仍然精确等于 `totalSteps`（audit_derived 会核对）。
pub fn build_laps_from_points(pts: &[Value], start_ms: i64, total_steps: i64) -> Vec<Value> {
    let mut laps: Vec<Value> = Vec::new();
    let Some(last) = pts.last() else { return laps };
    let last_dis = last.get("totalDis").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let alt0 = pts
        .first()
        .and_then(|p| p.get("bdA"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    if pts.len() < 2 || last_dis <= 0.0 {
        return laps;
    }

    let num = |q: &Value, k: &str| q.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let r2 = |x: f64| (x * 100.0).round() / 100.0;
    let r4 = |x: f64| (x * 10000.0).round() / 10000.0;
    // 截至里程 d 的累计步数（按里程比例分配，全程末值正好等于 total_steps）
    let steps_at = |d: f64| -> i64 {
        ((total_steps as f64) * (d / last_dis).clamp(0.0, 1.0)).round() as i64
    };

    let (mut prev_d, mut prev_t, mut prev_steps, mut prev_gain) = (0.0f64, 0i64, 0i64, 0.0f64);
    // 上限防止异常数据把循环卡死（每圈至少 1m 才继续）
    let mut guard = 0usize;
    while prev_d < last_dis && guard < 10_000 {
        guard += 1;
        let d_now = (prev_d + 1000.0).min(last_dis);
        let (mut t_now, mut altitude) = (num(last, "totalTime") as i64, num(last, "bdA"));
        let mut gain = 0.0f64;
        for pair in pts.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            let climb = (num(b, "bdA") - num(a, "bdA")).max(0.0);
            let (da, db) = (num(a, "totalDis"), num(b, "totalDis"));
            if d_now < last_dis && db >= d_now && db > da {
                let k = (d_now - da) / (db - da);
                t_now = (num(a, "totalTime") + (num(b, "totalTime") - num(a, "totalTime")) * k).round() as i64;
                altitude = num(a, "bdA") + (num(b, "bdA") - num(a, "bdA")) * k;
                gain += climb * k;
                break;
            }
            gain += climb;
        }
        let steps = steps_at(d_now);
        let lap_d = d_now - prev_d;
        let lap_t = t_now - prev_t;
        let lap_steps = steps - prev_steps;
        let idx = laps.len() as i64 + 1;
        laps.push(json!({
            "avgCadence": if lap_t > 0 { json!(r2(lap_steps as f64 / (lap_t as f64 / 60.0))) } else { Value::Null },
            "avgPace": if lap_t > 0 && lap_d > 0.0 { json!(r4(lap_t as f64 / 60.0 / (lap_d / 1000.0))) } else { Value::Null },
            "avgStride": if lap_steps > 0 { json!(r2(lap_d / lap_steps as f64 * 100.0)) } else { Value::Null },
            "cumulativeDuration": t_now,
            "distance": r4(lap_d),
            "duration": lap_t,
            "elevationGain": r2(gain - prev_gain),
            "endAltAbs": r2(altitude),
            "endAltRel": r2(altitude - alt0),
            "flag": start_ms,
            "id": idx,
            "isFullLap": lap_d >= 1000.0,
            "lapIndex": idx,
            "step": lap_steps,
        }));
        prev_d = d_now;
        prev_t = t_now;
        prev_steps = steps;
        prev_gain = gain;
        // 末圈已到终点，避免 last_dis 恰好是整数倍时再切一个 0 长度圈
        if d_now >= last_dis {
            break;
        }
    }
    laps
}

/// 构造 OBS `segment_json`：真机那套「起步段 + 20 秒窗口 + 收尾段」的分段配速。
///
/// 真机依据（`evidence/obs_real_20261002.json`，555 秒 / 1683.83m，25 段，
/// id 733~757）：
///
/// - **首段特殊**：86 秒、`state=131072`(0x20000)、`distance` 只有 18.4m
///   —— 起步 GPS 漂移被过滤，不是真跑了 86 秒 18 米；
/// - **中间段严格 20 秒**、首尾相接无空洞（734~754 共 21 段）；
/// - **尾部不硬凑 20 秒**：真机倒数第二段 25s、末段 10s；
/// - `avgSpeed` 是**配速（分/公里）**不是 m/s：743 段 113.286m / 20s
///   → (20/60) 分 ÷ 0.113286 km = 2.94 分/公里，与该段 `avgSpeed`
///   2.9414 精确吻合；按 m/s 算会得到 5.66，差一倍；
/// - 25 段 `distance` 之和 = 1683.83m = `totalDis`；
/// - `speedStandard` 恒 900、`intervalStepModel` 恒 0、`avgStep == originAvgStep`；
/// - `flag` 全是整场起始戳，`id` 连续递增。
///
/// 我们这边比真机多两条保证，都是为了「三处口径一致」：
///
/// 1. 段时长之和**精确等于** `totalTime`：首段之后每段严格 20 秒，末段吃
///    余数（0 < 末段 ≤ 20 秒）。旧实现把末段钉死 10 秒、让倒数第二段吸收
///    20~39 秒的余数，结果中间会出现一个 35 秒的段，与「中间段严格 20 秒」
///    自相矛盾。
/// 2. `Σ段distance` **精确等于**末点的 `totalDis`：窗口两端插值理论上能
///    望远镜相消，但只要首段边界或末点时间戳有一处对不上（点的时间戳来自
///    秒级取整），差出来的里程就会凭空消失。这里按比例补回去，任何缝隙都
///    不会让段里程和少于 `totalDis`。
///
/// `total_steps` 按里程比例分摊到各段，段 `avgStep` 之和 ≈ `totalSteps`。
pub fn build_segments_from_points(pts: &[Value], start_ms: i64, total_steps: i64) -> Vec<Value> {
    let mut segs: Vec<Value> = Vec::new();
    let Some(last) = pts.last() else { return segs };
    let total_sec = last.get("totalTime").and_then(|v| v.as_i64()).unwrap_or(0);
    let total_dis = last.get("totalDis").and_then(|v| v.as_f64()).unwrap_or(0.0);
    if pts.len() < 2 || total_sec <= 0 || total_dis <= 0.0 {
        return segs;
    }

    // ① 时间窗口：[起步段] [20s × n] [末段 = 余数]
    let first = crate::track::startup_sec(total_sec);
    let mut windows: Vec<(i64, i64)> = Vec::with_capacity(total_sec as usize / 20 + 2);
    // 起步段单独成一段（真机首段 86 秒、state=0x20000）
    if first > 0 {
        windows.push((0, first));
    }
    let mut t = first.max(0).min(total_sec);
    while total_sec - t > 20 {
        windows.push((t, t + 20));
        t += 20;
    }
    if t < total_sec {
        windows.push((t, total_sec));
    }
    // 末段不足 MIN_SEGMENT_SEC：从上一窗借几秒，别在报文尾部留一个 1 秒段。
    //
    // 真机的时间轴是 25 秒一段（首段 86 秒是起步段），最短的非末段也是 20 秒；
    // 但 `(total - 起步段) % 20 == 1` 时会切出一个 1 秒的末段（实测 5km/1800s
    // 就是 `1.0s / 3.8m`），服务端一看段表就知道这一段是拼出来的。
    if windows.len() >= 2 {
        let last = windows.len() - 1;
        let tail = windows[last].1 - windows[last].0;
        if tail < MIN_SEGMENT_SEC {
            let prev = windows[last - 1];
            let borrow = MIN_SEGMENT_SEC - tail;
            if (prev.1 - prev.0) - borrow >= MIN_SEGMENT_SEC {
                windows[last - 1].1 -= borrow;
                windows[last].0 -= borrow;
            }
        }
    }

    // ② 每个窗口的位移 = 窗口两端累计里程之差（插值），天然首尾相接
    let dis_at = |sec: i64| -> f64 { cumulative_dis_at(pts, sec) };
    let r2 = |x: f64| (x * 100.0).round() / 100.0;
    let idb = id_base(start_ms as u64, 300) + 700;

    // ③ 归一化：Σ窗内位移按比例铺回末点里程。
    //
    // 理论上 Σ(dis_at(we)-dis_at(ws)) 望远镜相消等于 dis_at(末点时间)，
    // 也就是末点的 totalDis。但窗口边界与点时间戳都是**秒级取整**出来的，
    // 首段边界一旦落在两个点之间偏一点、或者末点时间戳与 totalTime 差 1 秒，
    // 差额就是几十上百米地从段表里消失——表现是「Σ段距离 1559.8m，而
    // totalDis 1833.5m」这种段表和点集各说各话。
    //
    // 这里不猜差额怎么来的，直接按里程比例把它摊到每一段上：段表总和从
    // 构造上等于 totalDis，服务端拿段表逐段加也加不出第二个数字。
    let raw: Vec<f64> = windows
        .iter()
        .map(|&(ws, we)| (dis_at(we) - dis_at(ws)).max(0.0))
        .collect();
    let raw_sum: f64 = raw.iter().sum();
    let scale = if raw_sum > 1e-6 { total_dis / raw_sum } else { 1.0 };

    // 累计步数：按「到这一秒为止的累计里程占末点累计里程的比例」分配。
    // 分母取段表最后一个窗口的右端（而不是末点 totalDis），这样
    // `steps_at(末段右端) == total_steps` 恒成立，Σ段步数 == 报文总步数。
    let steps_end = windows.last().map(|w| w.1).unwrap_or(total_sec);
    let steps_denom = (dis_at(steps_end)).max(1e-6);
    let steps_at = |sec: i64| -> i64 {
        (((total_steps as f64) * dis_at(sec) / steps_denom).round() as i64).clamp(0, total_steps)
    };

    for (i, &(ws, we)) in windows.iter().enumerate() {
        let dur = (we - ws).max(1);
        let dist = raw[i] * scale;
        // avgSpeed 是配速（分/公里），不是速度。位移过小（<5cm）时真机会
        // 报一个几千的分/公里天文数字，我们没有原始噪声可凑，记 0 表示
        // 「这一段没有有效位移」。
        let pace = if dist > 0.005 {
            (dur as f64 / 60.0) / (dist / 1000.0)
        } else {
            0.0
        };
        // 步数用**累计差值**分配，不用逐段独立 round：
        // 逐段 round 会让 Σ段步数 和报文里的 totalSteps 差几步（实测 1920 vs 1922），
        // 而服务端把段表加一遍就能对上总步数。累计差值天然望远镜相消，
        // Σ == totalSteps 是构造出来的，不是碰出来的。
        let steps = steps_at(we) - steps_at(ws);
        // 起步段 state=131072（0x20000）是真机给起步段的标记，其余段是 0。
        // 注意 `first == 0`（≤20 秒的短跑）时压根没有起步段，
        // 第一段就是普通 20 秒段，不能跟着标。
        let state: i64 = if first > 0 && i == 0 { 131_072 } else { 0 };
        segs.push(json!({
            "avgSpeed": (pace * 10000.0).round() / 10000.0,
            "avgStep": steps,
            "distance": r2(dist),
            "endTime": start_ms + we * 1000,
            "flag": start_ms,
            "id": idb + i as i64,
            "intervalStepModel": 0,
            "originAvgStep": steps,
            "speedStandard": 900,
            "startTime": start_ms + ws * 1000,
            "state": state,
        }));
    }
    segs
}

/// 累计里程在 `sec`（相对起跑，秒）处的值：点位按 totalTime 升序，线性插值。
fn cumulative_dis_at(pts: &[Value], sec: i64) -> f64 {
    let num = |q: &Value, k: &str| q.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    if pts.is_empty() {
        return 0.0;
    }
    let mut prev = (0i64, num(&pts[0], "totalDis"));
    if sec <= prev.0 {
        return prev.1;
    }
    for q in &pts[1..] {
        let cur = (q.get("totalTime").and_then(|v| v.as_i64()).unwrap_or(0), num(q, "totalDis"));
        if cur.0 >= sec {
            if cur.0 == prev.0 {
                return cur.1;
            }
            let k = (sec - prev.0) as f64 / (cur.0 - prev.0) as f64;
            return prev.1 + (cur.1 - prev.1) * k;
        }
        prev = cur;
    }
    prev.1
}

/// 派生字段互校：报文里由轨迹派生出来的几个数必须与点位自洽。
///
/// 这一层存在的理由：`totalAscent`、`calorie`、`avgPower`、`speed` 都是
/// 「看着报文体随便填也不会报错」的字段，一旦哪个分支回退成常量
/// （0 爬升、62 kcal/km、0 功率），报文仍然能提交成功，只在服务端那边
/// 表现为「这项数据来源可疑」。所以每次提交前，用**已组装好的报文体**
/// 反算一遍，跟声明值比——检查对象与发送对象是同一份。
///
/// 返回空 vec 表示自洽；非空表示必须拦下（不是提示）。
pub fn audit_derived(body: &Value, pts: &[Value]) -> Vec<String> {
    let mut probs: Vec<String> = Vec::new();
    if pts.is_empty() {
        return vec!["点位列是空的，派生字段无从校验".into()];
    }
    let pts: Vec<Value> = pts.to_vec();
    let dis = body.get("totalDis").and_then(|v| v.as_i64()).unwrap_or(0);
    let time_sec = body.get("totalTime").and_then(|v| v.as_i64()).unwrap_or(0);

    // 1) 累计爬升 = 点位 bdA 正增量之和
    let expect_ascent = total_ascent_from_points(&pts) as i64;
    let declared_ascent = body
        .get("totalAscent")
        .and_then(|v| v.as_i64())
        .unwrap_or(i64::MIN);
    if (declared_ascent - expect_ascent).abs() > 1 {
        probs.push(format!(
            "totalAscent 声明 {declared_ascent} 与点位反算 {expect_ascent} 不符"
        ));
    }

    // 2) 卡路里必须走 MET 折算，不能是固定 kcal/km
    let expect_cal = calorie_for(dis as f64, time_sec, DEFAULT_WEIGHT_KG);
    let declared_cal = body.get("calorie").and_then(|v| v.as_i64()).unwrap_or(i64::MIN);
    if (declared_cal - expect_cal).abs() > 2 {
        probs.push(format!(
            "calorie 声明 {declared_cal} 与 MET 折算 {expect_cal} 不符"
        ));
    }

    // 3) 平均功率非零且与速度成比例（整数瓦特，服务端 avgPower 是整型）
    let expect_pow = avg_power_for(dis as f64, time_sec, DEFAULT_WEIGHT_KG);
    let declared_pow = body.get("avgPower").and_then(|v| v.as_i64()).unwrap_or(i64::MIN);
    if declared_pow != expect_pow {
        probs.push(format!(
            "avgPower 声明 {declared_pow} 与折算 {expect_pow} 不符（必须是整数）"
        ));
    }

    // 4) speed 是配速×1000，不是速度×1000
    let expect_speed = speed_field(dis as f64, time_sec);
    let declared_speed = body.get("speed").and_then(|v| v.as_i64()).unwrap_or(i64::MIN);
    if declared_speed != expect_speed {
        probs.push(format!(
            "speed 声明 {declared_speed} 与配速换算 {expect_speed} 不符"
        ));
    }

    // 5) 圈数据存在时，圈里程和 / 圈时长和必须与总量一致
    let laps = build_laps_from_points(
        &pts,
        body.get("startTime").and_then(|v| v.as_i64()).unwrap_or(0),
        body.get("totalSteps").and_then(|v| v.as_i64()).unwrap_or(0),
    );
    if !laps.is_empty() {
        let sum_d: f64 = laps.iter().filter_map(|l| l["distance"].as_f64()).sum();
        if (sum_d - dis as f64).abs() > 1.5 {
            probs.push(format!("圈里程和 {sum_d:.1}m 与 totalDis {dis}m 不符"));
        }
        let sum_t: i64 = laps.iter().filter_map(|l| l["duration"].as_i64()).sum();
        if (sum_t - time_sec).abs() > laps.len() as i64 {
            probs.push(format!("圈时长和 {sum_t}s 与 totalTime {time_sec}s 不符"));
        }
        let last_ok = laps.last().map(|l| l["isFullLap"].as_bool() == Some(false)
            || (dis % 1000 == 0));
        if last_ok == Some(false) {
            probs.push("末圈标注为整圈，但总里程不是 1000m 的整数倍".into());
        }
    }

    probs
}
/// 构建完整提交报文。
pub fn build(
    params: &SubmitParams,
    points: &[TrackPoint],
    fixed: &[FixedPoint],
    sim: &crate::track::SimResult,
) -> Result<Submission, String> {
    let loc_items = build_points(points, fixed, params.start_time_ms, params.alt_range)?;
    build_from_points(params, points, fixed, sim, loc_items)
}

/// **唯一点位构造点**：自检、提交报文、OBS 明细三处从这里取同一份数组。
///
/// 在此之前同一条轨迹被造过两次点位，两次入参不同：
///
/// | | 自检那份（`build`） | 上传那份（`flow::detail_payload_rrid`） |
/// |---|---|---|
/// | seed | `start_ms ^ φ` | `start_ms` |
/// | 平滑 | `smooth_point_speed` | 不做 |
/// | 打卡点 | `fixed` | 传 `&[]` |
///
/// 于是 `id` / `count` / `radius` / `type` / `bdA` 逐点不同，`isFixed` 与
/// `type=2` 的打卡标记只有上传那份才有——自检一路绿灯，守的却是另一份没人
/// 会看到的报文（AUDIT-round A4，`FIX-A4-SINGLE-POINT-SOURCE.md`）。
///
/// 现在点位只有这一个构造函数：`build()` 用它，OBS 明细直接接收它的产物，
/// 谁都不许自己再拼一遍。入参就是点位的全部输入：
///
/// - `track`：几何 + 时间戳（坐标、`totalDis`/`totalTime`、速度三兄弟、`bdA` 相位）；
/// - `fixed`：打卡点（`type=2` 标记靠它算 `cp_idxs`）；
/// - `start_ms`：seed（`id`/`count`/`radius`/`type`/`bdA` 的相位）、`flag`、
///   `stepDistance`、起步窗口；
/// - `alt_range`：`bdA` 区间。
pub fn build_points(
    track: &[TrackPoint],
    fixed: &[FixedPoint],
    start_ms: i64,
    alt_range: Option<(f64, f64)>,
) -> Result<Vec<Value>, String> {
    if track.is_empty() {
        return Err("轨迹为空，无法组装点位".into());
    }
    // 逐点生成，累计量必须顺序推进
    let mut prev = (track[0].lat, track[0].lon);
    let start_pt = prev;
    let mut cum = (0.0f64, 0i64, 0i64);
    let seed = start_ms as u64 ^ 0x9E3779B97F4A7C15;
    let mut prev_time = track[0].time_ms;
    // `bdS` 要「上一拍的点间速度」，逐点状态跟着 prev/precum 一起往前推。
    let mut prev_v = 0.0f64;
    // 打卡点落在轨迹的哪几个采样点上（首次经过下标）。
    //
    // 早先这里读 `f.position`——那时候 `position` 表达「该点在轨迹里的首次
    // 经过下标」。现在 `position` 是报文里的连续序号（0..N-1），语义变了，
    // 所以内部这个量改成按坐标现算，而且**只算这一次**：OBS 明细拿到的就是
    // 这里标好的 `type=2`，不再自己按 `fivePointJson` 重算一遍。
    let cp_idxs: Vec<usize> = checkpoint_track_indexes(
        track,
        &fixed.iter().map(|f| (f.lat, f.lon)).collect::<Vec<_>>(),
    );
    // 起步窗口的点数：按时间戳数一遍（含末点时间戳），点字段、`queueNum`、
    // `segment_json` 首段、几何压缩共用这一个窗口，不再各自按比例估算。
    let startup = {
        let times: Vec<i64> = track.iter().map(|p| p.time_ms).collect();
        let total_sec = times
            .iter()
            .map(|&t| (t - start_ms) / 1000)
            .max()
            .unwrap_or(0);
        crate::track::startup_point_count(&times, start_ms, total_sec)
    };
    let mut items: Vec<Value> = track
        .iter()
        .enumerate()
        .map(|(i, p)| {
            // `total_steps` 是 `point_json_full` 的历史入参，函数体内已不使用
            // （点位里没有 steps 字段），这里传 0 以免让调用方以为它影响点位。
            point_json_full(
                p, i, track.len(), startup, start_ms, start_pt, &mut prev, &mut cum,
                &mut prev_time, &mut prev_v, 0, seed, &cp_idxs, alt_range,
            )
        })
        .collect();

    // 配速平滑必须在这里做：逐点 speed 是「相邻两点位移 / 间隔」，轨迹带 GPS
    // 抖动时 App 的配速图满屏锯齿。真机是 GPS 多普勒测速，曲线本来就平滑。
    smooth_point_speed(&mut items);
    Ok(items)
}

/// 用**已经造好的点位数组**组装报文。
///
/// `loc_items` 必须是 [`build_points`] 的产物，并且会被原样放进
/// [`Submission::points`]——一个字节都不重算。`flow::dry_run` 拿这份做自检，
/// `flow::run_full` 把同一份提交并上传 OBS，所以「自检校验的 == 上传的」。
pub fn build_from_points(
    params: &SubmitParams,
    points: &[TrackPoint],
    fixed: &[FixedPoint],
    sim: &crate::track::SimResult,
    loc_items: Vec<Value>,
) -> Result<Submission, String> {
    if points.is_empty() {
        return Err("轨迹为空，无法组装报文".into());
    }
    if loc_items.len() != points.len() {
        return Err(format!(
            "点位数组 {} 条与轨迹 {} 点不一致，不能当同一份数据用",
            loc_items.len(),
            points.len()
        ));
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
    // 卡路里与功率用官方折算，不再用固定 62 kcal/km、也不再恒发 0 功率
    let calorie = calorie_for(
        params.total_distance_m as f64,
        params.total_time_sec,
        DEFAULT_WEIGHT_KG,
    );
    let avg_power: i64 = avg_power_for(
        params.total_distance_m as f64,
        params.total_time_sec,
        DEFAULT_WEIGHT_KG,
    );
    // 整数类型下 to_string 与 serde 序列化必然一致，不需要再做文本对齐。
    let avg_power_str = avg_power.to_string();

    // 轨迹点带瞬时速度（中心差分），服务端画图会用到
    // 单一数据源：直接用 simulate() 产出的桶。
    //
    // 之前这里拿 make_jitter(amp=1.0) **重新生成**了一套桶写进报文，而自检
    // 校验的是 sim.step_buckets、OBS 明细传的也是 sim.step_buckets——
    // 自检守 A、提交发 B、明细传 A，反检测设计整个落空。
    // amp=1.0 还是 ±100% 的均匀抖动，会造出 0 步桶（10 秒静止）。
    let sb: Vec<crate::track::SpeedBucket> = sim.speed_buckets.clone();
    let stb: Vec<crate::track::StepBucket> = sim.step_buckets.clone();

    // 累计爬升必须从**已生成的点位**反算，与 allLocJson 里的 bdA 物理同源。
    // 原来恒发 0：点位有起伏、总爬升却是 0，服务端按两点海拔差累计时立刻
    // 得到与上报值冲突的两套数字。
    let total_ascent = total_ascent_from_points(&loc_items);

    // 必到点数组：序号类字段（position / id / isFixed / flag）统一由
    // fixed_point_items 按真机规则发号，数组顺序也按途经顺序重排。
    // 早先这一层是「原样回显服务端的顺序 + position 填轨迹下标」，正是
    // App 详情页只显示一部分点位的原因，见函数注释与 FIXED-POINT-PARITY.md。
    let fp_items: Vec<Value> = fixed_point_items(fixed, points, params.start_time_ms);
    // 主报文这一层要 envelope：改成裸数组服务端直接 -10000
    // （成绩上传失败）。记录详情里 fivePointJson 回显 "" 是正常的，
    // 点位数据走的是 OBS 明细里的 fixed_point_json。
    // 键序按真机样本排列（五个键），字段集合与真机完全一致。
    let five_envelope = json!({
        "fivePointJson": serde_json::to_string(&fp_items).map_err(|e| e.to_string())?,
        "freedomShowFence": false,
        "geoFencesJson": params.geo_fences_json,
        // 真机这份对象里是 -1（真实围栏 id 只在 geoFencesJson[0].id 里），
        // 见 payload_run_area_id 的注释。顶层 save_record 那个 runAreaId
        // 保持真实 id 不变。
        "runAreaId": payload_run_area_id(params.run_area_id),
        "useZip": false,
    });
    let five_pt_json = serde_json::to_string(&five_envelope).map_err(|e| e.to_string())?;

    // 提交报文的桶 id 规则：60000+i；`queueNum` 不是 `i`，走与点位同一套
    // floor 分区（桶网格上的起步窗口 = startup_sec/10 整桶），见 apply_submit_ids
    let mut sb = sb;
    let mut stb = stb;
    crate::track::buckets::apply_submit_ids(&mut sb, &mut stb);
    // 服务端要的是**数组**，不是字符串化的 JSON。
    // 早先按 allLocJson 的模式套成了字符串，结果提交回来 -10000。
    let speed_arr = serde_json::to_value(&sb).map_err(|e| e.to_string())?;
    let step_arr = serde_json::to_value(&stb).map_err(|e| e.to_string())?;

    // ---- geeToken ----
    //
    // 真机的 geeToken 是本地算的（AES-256-CBC 套 GEE\0 帧），不依赖极验
    // 回调，所以离线也能生成。原来两处都发空串。
    //
    // 发空串的后果不是直接报错拒单，而是服务端把整次 run 的所有采样点
    // 统一打上「未通过校验」的兜底类型标记——表现就是里程、时长、步频、
    // 配速四项全过，只有「通过点位」那一项永远 false，而轨迹怎么调都是
    // 干净的。这条线很容易把排查带到轨迹上去。
    //
    // 签名侧和 body 侧必须引用同一个 String，否则签名串和回执体里的
    // token 不一致，服务端直接 10501。
    let gee_token = crate::crypto::geetoken::token_for_run(&params.uid, &params.uuid);

    // ---- 签名（31 字段）----
    let sign_fields: SignFields = vec![
        ("address".into(), Some(String::new())),
        ("avgPower".into(), Some(avg_power_str.clone())),
        ("avgStepFreq".into(), Some(avg_step_freq.to_string())),
        ("calorie".into(), Some(calorie.to_string())),
        ("complete".into(), Some(complete.to_string())),
        ("errorCode".into(), Some("0".into())),
        ("faceCheck".into(), Some("0".into())),
        ("geeToken".into(), Some(gee_token.clone())),
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
        ("totalAscent".into(), Some(total_ascent.to_string())),
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
        // 轨迹不进 body：真机与 Neko 都发空串，点集只走 OBS 的 run_data。
        //
        // 详情接口对这类提交一律回 allLocJson=null、jsonFromObs=1（实测 50/50），
        // 所以塞不塞点都不会让详情带回坐标。发空串是为了跟真机报文一致，
        // 少一个可能被判「非真机」的差异字段。
        // App 拿 jsonFromObs=1 之后到底会不会去 OBS 拉，见 TRAJECTORY-DISPLAY.md
        // 的「未定项」——那里有两个互斥候选，尚未做对照实验。
        "allLocJson": String::new(),
        "avgPower": avg_power,
        "avgStepFreq": avg_step_freq,
        "calorie": calorie,
        "complete": complete,
        "errorCode": 0,
        "faceCheck": 0,
        "fivePointJson": five_pt_json,
        "geeToken": gee_token,
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
        "totalAscent": total_ascent,
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

    Ok(Submission { save_record, object_key, canonical, points: loc_items })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 给旧测试造一份 sim，桶用确定值（不依赖 OU 随机）。
    pub(super) fn test_sim(p: &SubmitParams, pts: &[TrackPoint]) -> crate::track::SimResult {
        let _ = p;
        let n = (p.total_time_sec / 10).max(1) as usize;
        let per = (p.total_steps.max(0) as f64 / n as f64).ceil() as i32;
        // 桶的 `queueNum` 与点位同一条 floor 分区，`su` 用**桶**自己的起步窗口
        // （`startup_sec/10` 整桶截断），不是点位那个 `su`。
        let su = crate::track::buckets::bucket_startup(p.total_time_sec);
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
                    queue_num: crate::track::queue_num(i, n, su),
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
                    queue_num: crate::track::queue_num(i, n, su),
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
            alt_range: None,
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
        // allLocJson 必须是**空串**，与真机/Neko 一致，点集只走 OBS 的 run_data。
        // 注意：这不是「画不出线」的原因——详情接口对这类提交固定回
        // allLocJson=null、jsonFromObs=1，塞点也一样（实测 50/50）。
        assert_eq!(v["allLocJson"], json!(""), "allLocJson 应发空串");
        assert!(!sub.points.is_empty(), "点集要在 Submission.points 里备着给 OBS/自检用");

        // speedPerTenSec / stepsPerTenSec 是**数组**，不要套成字符串。
        // 早先按 allLocJson 的模式一起套了，提交回来 -10000。
        assert!(v["speedPerTenSec"].is_array(), "speedPerTenSec 应为数组");
        assert!(v["stepsPerTenSec"].is_array(), "stepsPerTenSec 应为数组");
        assert!(!v["speedPerTenSec"].as_array().unwrap().is_empty());
        assert!(!v["stepsPerTenSec"].as_array().unwrap().is_empty());
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
        let p = params();
        let sim = test_sim(&p, &points());
        let sub = build(&p, &points(), &[], &sim).unwrap();
        let sp = &sub.save_record["speedPerTenSec"];
        let arr = sp.as_array().expect("speedPerTenSec 应该是数组");
        assert!(!arr.is_empty());
        assert_eq!(arr[0]["id"], 60000);
        assert_eq!(arr[1]["id"], 60001);

        // `queueNum` 与点位同一条 floor 分区：720 秒 → 起步窗口 112 秒 →
        // 桶网格上 `112/10 = 11` 个整桶，所以前 11 个桶是 0；
        // 之后 `ceil((72-1-11)/24) = 3` 个桶一档。
        let n = arr.len();
        assert_eq!(n, 72);
        let su = crate::track::buckets::bucket_startup(p.total_time_sec);
        assert_eq!(su, 11, "720 秒的起步窗口落在 11 个整桶上");
        for (i, b) in arr.iter().enumerate() {
            let want = crate::track::queue_num(i, n, su);
            assert_eq!(b["queueNum"], want, "第 {i} 个速度桶 queueNum 错");
        }
        assert_eq!(arr[0]["queueNum"], 0, "起步段首个桶是 0 档");
        assert_eq!(arr[su]["queueNum"], 1, "出起步窗口的桶从 1 起");
        // 60 个格子铺 24 档 → 步长 ceil(60/24)=3，`24×3 = 72 > 60`，所以只用满
        // 到第 21 档（真机 555 秒那组 94/4 恰好走到 24，这里到不了是整数台阶
        // 的必然结果，不是笔误）。上限仍是 24，绝不会出现桶序号 71。
        assert_eq!(arr[n - 1]["queueNum"], 21, "720 秒末桶落在第 21 档");
        assert!(arr.iter().all(|b| b["queueNum"].as_i64().unwrap() <= 24));

        // stepsPerTenSec 用同样的 id / queueNum 规则
        let sa = sub.save_record["stepsPerTenSec"].as_array().expect("应为数组");
        assert_eq!(sa[0]["id"], 60000);
        for (i, b) in sa.iter().enumerate() {
            let want = crate::track::queue_num(i, n, su);
            assert_eq!(b["queueNum"], want, "第 {i} 个步频桶 queueNum 错");
        }
        // 全场量纲必须是 0..24，不是桶序号 0..71（旧实现就是这么错的）
        let max = arr.iter().map(|b| b["queueNum"].as_i64().unwrap()).max().unwrap();
        assert!(max <= 24, "queueNum 上限 24，实际 {max}");
        assert!(max < n as i64 - 1, "不能退化成桶序号");
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
pub fn save_record(
    client: &Client,
    body: &Value,
    runes: &str,
    runef: &str,
) -> Result<Value, String> {
    // 真机提交会额外带 runes / runef 两个关联头：
    //   runes = 策略时间戳 + uid      （这条记录基于哪一次策略下发）
    //   runef = 运行 uuid + startTime （这份轨迹属于哪一次运行）
    // 缺了它们提交照样能成，但风控侧少了把「策略→运行→成绩」串起来的
    // 线索，关联性更弱。见 Miempire src/api/submit.rs:228-237。
    let raw = client.request_with_headers(
        "POST",
        &format!("{}/api/v70260/runnings/save/record", crate::crypto::consts::hosts::RUN),
        Some(body),
        &[("runes", runes), ("runef", runef)],
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
            alt_range: None,
        }
    }

    fn tp(lat: f64, lon: f64, t: i64) -> TrackPoint {
        TrackPoint { lat, lon, time_ms: t }
    }

    /// 所有采样点的 `flag` 必须是**同一个**整场起始毫秒戳。
    ///
    /// 这条断言原来写的是「逐点递增」，依据是一句没有真机样本的推测。
    /// 真机样本 `evidence/obs_real_20261002.json` 里 114 个点的 flag
    /// 全部等于 1790913882164（去重后只有 1 个值，跨度 0），segment_json /
    /// speed_json / step_freq_json / laps_json / fixed_point_json 里也是
    /// 这个值。时间进度由 totalTime（累计秒）和 gainTime（可读时间串）表达。
    #[test]
    fn flag_is_constant_across_points() {
        let pts: Vec<TrackPoint> = (0..10)
            .map(|i| tp(31.2304 + i as f64 * 2e-4, 121.4737, 1_790_000_000_000 + i * 5000))
            .collect();
        let params = params();
        let sim = test_sim(&params, &pts);
        let sub = build(&params, &pts, &[], &sim).unwrap();
        let arr: Vec<serde_json::Value> = sub.points.clone();

        let flags: Vec<i64> = arr.iter().map(|p| p["flag"].as_i64().unwrap()).collect();
        let uniq: std::collections::HashSet<i64> = flags.iter().copied().collect();
        assert_eq!(uniq.len(), 1, "flag 只能有一个值，实际 {uniq:?}");
        assert_eq!(
            flags[0], params.start_time_ms,
            "flag 必须等于整场起始毫秒戳"
        );
        // 时间维度改由 totalTime 承担，它必须逐点递增
        let tt: Vec<i64> = arr.iter().map(|p| p["totalTime"].as_i64().unwrap()).collect();
        assert!(
            tt.windows(2).all(|w| w[1] >= w[0]),
            "totalTime 必须单调不减，实际 {tt:?}"
        );
        assert!(tt.last().unwrap() > &tt[0], "totalTime 要能表达时间进度");
    }

    /// 采样点字段值域必须落在真机观测范围内，且相互自洽。
    ///
    /// 这条测试锁的是「注释声称 vs 代码实际」，同时把取值区间钉在真机样本
    /// `evidence/obs_real_20261002.json` 上：`count` 实测 40~53、`bdA`
    /// 实测 142.4~158.5（齐齐哈尔）、`radius` 实测 1.4~2.3。早先按 10~34 /
    /// 20~64 / 3.6~4.3 生成，三处都与真机差一截。
    ///
    /// 同时锁 `speed`(km/h) 与 `bdS`(m/s) 的单位关系：这两个字段早先各夹
    /// 各的区间，速度落在两端时 `speed/3.6` 和 `bdS` 对不上。
    #[test]
    fn point_field_ranges_match_real_device() {
        // 走一段圆弧：直线轨迹的方位角必然恒定，测不出 bdD 是否真的在算
        let pts: Vec<TrackPoint> = (0..200)
            .map(|i| {
                let a = i as f64 * 0.03;
                tp(
                    31.2304 + (a.cos() - 1.0) * 0.008,
                    121.4737 + a.sin() * 0.008,
                    1_790_000_000_000 + i * 1700,
                )
            })
            .collect();
        let params = params();
        let sim = test_sim(&params, &pts);
        let sub = build(&params, &pts, &[], &sim).unwrap();
        let arr: Vec<serde_json::Value> = sub.points.clone();

        // ① 卫星数：真机 40~53（众数 46~49），且不是常量
        let counts: Vec<i64> = arr.iter().map(|p| p["count"].as_i64().unwrap()).collect();
        for &c in &counts {
            assert!((40..=53).contains(&c), "count={c} 超出真机 40~53");
        }
        let uniq: std::collections::HashSet<i64> = counts.iter().copied().collect();
        assert!(uniq.len() >= 8, "count 只有 {} 种取值，等于常量", uniq.len());

        // ② 海拔：真机 142.4~158.5（齐齐哈尔平原校园），相邻点不许跳变
        let alts: Vec<f64> = arr.iter().map(|p| p["bdA"].as_f64().unwrap()).collect();
        for &a in &alts {
            assert!((142.0..=158.5).contains(&a), "bdA={a} 超出真机 142~158");
        }
        let spread = alts.iter().cloned().fold(f64::MIN, f64::max)
            - alts.iter().cloned().fold(f64::MAX, f64::min);
        assert!(spread > 5.0, "海拔跨度只有 {spread:.1}m，太像常量");
        let max_step = alts
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f64::max);
        // 真机相邻点 |ΔbdA|: 均值 1.13 / p90 2.9 / max 4.7，所以上限放到 4.0
        assert!(max_step < 4.0, "相邻点海拔跳变 {max_step:.2}m，比真机噪声大");

        // ③ 航向角：首点 0，其余在 [0,360) 且有变化
        let bearings: Vec<f64> = arr.iter().map(|p| p["bdD"].as_f64().unwrap()).collect();
        assert_eq!(bearings[0], 0.0, "首点没有上一点，航向记 0");
        for &b in &bearings {
            assert!((0.0..360.0).contains(&b), "bdD={b} 超出 [0,360)");
        }
        let buniq: std::collections::HashSet<String> =
            bearings.iter().map(|b| format!("{b:.1}")).collect();
        assert!(buniq.len() > 5, "bdD 只有 {} 种取值，恒 0 是老问题", buniq.len());

        // ④ gainTime：非空且是可读时间串
        for p in &arr {
            let g = p["gainTime"].as_str().unwrap();
            assert_eq!(g.len(), 19, "gainTime 应为 19 位时间串，实际 {g:?}");
            assert_eq!(&g[4..5], "-", "gainTime 格式不对: {g}");
            assert_eq!(&g[10..11], " ", "gainTime 格式不对: {g}");
        }

        // ⑤ `speed` 是**配速 min/km**、`avgSpeed` 是**点间速度 m/s**，
        //    两者之间恒有 `speed × avgSpeed = 1000/60 = 16.6667`。
        //
        //    这条断言是被审计（AUDIT-round A2）点名改掉的那一条：原来写的是
        //    `bdS == speed/3.6`，把「speed 是 km/h」当成不变量，于是单位认反
        //    也照样绿。配速与 km/h 的区别就在这个乘积上——写成乘积关系，
        //    「speed 当 km/h 发」会让乘积变成 16.667/v² 量级（v≈3 时差 9 倍），
        //    一眼就红。
        for (i, p) in arr.iter().enumerate() {
            let speed = p["speed"].as_f64().unwrap();
            let avg = p["avgSpeed"].as_f64().unwrap();
            let bds = p["bdS"].as_f64().unwrap();
            if avg <= 0.0 {
                // 速度为 0 的点：真机三项都是硬 0，不是 ∞、也不是截断的大数
                assert_eq!(speed, 0.0, "第 {i} 点 avgSpeed=0 但 speed={speed}");
                assert_eq!(bds, 0.0, "第 {i} 点 avgSpeed=0 但 bdS={bds}");
                continue;
            }
            let want = 1_000.0 / (60.0 * avg);
            assert!(
                (speed - want).abs() <= want * 0.01 + 0.01,
                "第 {i} 点 speed={speed} 应为配速 {want:.4}（speed 是 min/km，不是 km/h）"
            );
            // bdS 是低通后的同一个量（m/s），与 avgSpeed 同量纲、慢一拍，
            // 单点残差真机最大 2.0 m/s，这里放宽到 3.2 只锁「量级/量纲」。
            let gap = (bds - avg).abs();
            assert!(
                gap <= 3.2,
                "第 {i} 点 bdS={bds} 与 avgSpeed={avg} 差 {gap:.2}，\
                 不是同一个量纲（bdS 是 m/s 的低通值，不是 speed/3.6）"
            );
        }
    }

    /// 海拔不能是常量。
    #[test]
    fn altitude_varies() {
        let pts: Vec<TrackPoint> = (0..40)
            .map(|i| tp(31.2304 + i as f64 * 2e-4, 121.4737, 1_790_000_000_000 + i * 5000))
            .collect();
        let params = params();
        let sim = test_sim(&params, &pts);
        let sub = build(&params, &pts, &[], &sim).unwrap();
        let arr: Vec<serde_json::Value> = sub.points.clone();

        let alts: Vec<f64> = arr.iter().map(|p| p["bdA"].as_f64().unwrap()).collect();
        // 恒 1.0 是「这个点没有海拔数据」的特征，服务端据此算累计爬升
        assert!(alts.iter().any(|&a| a > 1.5), "海拔应脱离 1.0，实际均值约 {}", alts.iter().sum::<f64>() / alts.len() as f64);

        // 相邻两点变化要小，真实 GPS 高程是连续量
        let max_step = alts.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f64::max);
        assert!(max_step < 3.0, "相邻点海拔跳变 {max_step:.2}m，太假");

        // 整条轨迹要有起伏
        let mean = alts.iter().sum::<f64>() / alts.len() as f64;
        let var = alts.iter().map(|a| (a - mean).powi(2)).sum::<f64>() / alts.len() as f64;
        assert!(var.sqrt() > 0.05, "海拔标准差 {} 太小，等于一条直线", var.sqrt());
    }

    #[test]
    fn altitude_respects_requested_range() {
        let pts: Vec<TrackPoint> = (0..200)
            .map(|i| tp(31.2304 + i as f64 * 2e-4, 121.4737, 1_790_000_000_000 + i * 5000))
            .collect();
        let mut params = params();
        // 高原校区：200~260m
        params.alt_range = Some((200.0, 260.0));
        let sim = test_sim(&params, &pts);
        let sub = build(&params, &pts, &[], &sim).unwrap();
        let alts: Vec<f64> = sub
            .points
            .iter()
            .map(|p| p["bdA"].as_f64().unwrap())
            .collect();

        let lo = alts.iter().cloned().fold(f64::INFINITY, f64::min);
        let hi = alts.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        assert!(
            lo >= 200.0 - 0.05 && hi <= 260.0 + 0.05,
            "海拔越界：实际 {lo:.1}~{hi:.1}，要求 200~260"
        );
        // 均值要靠近中点，不能整条贴着下界（那说明幅值没铺开）
        let mean = alts.iter().sum::<f64>() / alts.len() as f64;
        assert!(
            (mean - 230.0).abs() < 18.0,
            "均值 {mean:.1} 偏离区间中点 230 太多"
        );
        // 区间拉高后相邻点仍然只能动几十厘米，不能因为基准变大就抖起来
        let max_step = alts.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f64::max);
        assert!(max_step < 4.0, "相邻点海拔跳变 {max_step:.2}m（真机 max 4.7）");
    }

    #[test]
    fn altitude_range_rejects_bad_input() {
        let pts: Vec<TrackPoint> = (0..30)
            .map(|i| tp(31.2304 + i as f64 * 2e-4, 121.4737, 1_790_000_000_000 + i * 5000))
            .collect();
        let mut params = params();
        // 给反了：应当忽略，退回默认 150±8（真机 bdA 实测 142.4~158.5）
        params.alt_range = Some((300.0, 100.0));
        let sim = test_sim(&params, &pts);
        let sub = build(&params, &pts, &[], &sim).unwrap();
        let mean = sub.points.iter().map(|p| p["bdA"].as_f64().unwrap()).sum::<f64>()
            / sub.points.len() as f64;
        assert!((mean - 150.0).abs() < 12.0, "反向区间应退回默认基准 150，实际均值 {mean:.1}");
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
        let arr: Vec<serde_json::Value> = sub.points.clone();
        assert_eq!(arr.len(), 3);

        let p0 = &arr[0];
        // lat/lng 是**占位 -1.0**，真实坐标只走 gLat/gLng。
        //
        // 这个断言中间被反着改过一次（改成「lat 必须是真坐标 BD-09」），
        // 依据是「服务端按 lat/lng 判点位」的推测。真机样本
        // evidence/obs_real_20261002.json 里 114 个点的 lat/lng 无一例外
        // 都是 -1.0，所以按真机对齐只能填 -1.0；判点位取 gLat/gLng
        // （verify_view 也是从 gLat/gLng 反解轨迹的）。
        for (i, q) in arr.iter().enumerate() {
            assert_eq!(q["lat"].as_f64().unwrap(), -1.0, "第 {i} 个点 lat 不是 -1.0");
            assert_eq!(q["lng"].as_f64().unwrap(), -1.0, "第 {i} 个点 lng 不是 -1.0");
        }
        // 另一套坐标在 gLat/gLng，已转到 gcj02
        assert!(p0["gLat"].as_f64().unwrap() > 31.0);
        assert!(p0["gLng"].as_f64().unwrap() > 121.0);
        assert_ne!(p0["gLat"].as_f64().unwrap(), 31.2304, "应该是转换后的坐标");
        assert_eq!(p0["coorType"], "gcj02");

        // 关键字段齐全
        for k in [
            "avgSpeed", "flag", "id", "locType", "queueNum", "radius", "speed",
            "state", "totalDis", "totalTime", "type", "validDis", "validTime",
        ] {
            assert!(p0.get(k).is_some(), "缺字段 {k}");
        }
        // 真机点里没有 steps 字段（步数只在 segment_json / laps_json 里）
        assert!(p0.get("steps").is_none(), "真机没有 steps 字段，不该多发");

        // 起点与终点的 type：真机首点是 7（灰点）、第二个点 5、末点 6
        assert_eq!(arr[0]["type"], 7, "真机起点 type=7");
        assert_eq!(arr[1]["type"], 5, "真机第二个点 type=5");
        assert_eq!(arr[2]["type"], 6, "终点 type=6");

        // 累计量递增
        let d0 = arr[0]["totalDis"].as_f64().unwrap();
        let d2 = arr[2]["totalDis"].as_f64().unwrap();
        assert!(d2 > d0, "累计里程要递增");

        // speed 是**配速 min/km**，不是 km/h。
        //
        // 原来这条写的是 `sp > 1.0 && sp < 30.0`（「speed 应在 km/h 量级」）——
        // 配速 2.6~16.7 也全部落在这个区间里，所以这条断言根本区分不了两种
        // 口径，正是 A2（单位认反）能溜过去的原因。现在改成拿报文自己的
        // `totalDis`/`totalTime` 反算点间速度，要求 `speed` 与
        // `1000/(60v)` 的相对误差 <1%：按 km/h 发的话这里会差 (16.67/v) 倍。
        let (d1, d2) = (
            arr[1]["totalDis"].as_f64().unwrap(),
            arr[2]["totalDis"].as_f64().unwrap(),
        );
        let (t1, t2) = (
            arr[1]["totalTime"].as_i64().unwrap(),
            arr[2]["totalTime"].as_i64().unwrap(),
        );
        // 生成侧把点间速度夹到 MAX_POINT_SPEED_MPS（这件事本身在
        // `speed_is_pace_not_velocity` 之外没有别处断，夹完的配速照样满足
        // 同一条等式），所以这里用夹过的 v 反算期望值。
        let v = ((d2 - d1) / (t2 - t1) as f64).min(MAX_POINT_SPEED_MPS);
        let sp = arr[2]["speed"].as_f64().unwrap();
        let want = 1_000.0 / (60.0 * v);
        assert!(
            (sp - want).abs() / want < 0.01,
            "speed={sp} 与配速 1000/(60v)={want:.4} 相对误差超过 1%\
             （v={v:.3}m/s；若 speed 是 km/h 这里会是 v×3.6={:.3}）",
            v * 3.6
        );
        // 同一时刻 bdS 是 m/s 量级（≈v），不是 speed/3.6
        let bs = arr[2]["bdS"].as_f64().unwrap();
        assert!(
            (bs - v).abs() <= 3.2,
            "bdS={bs} 与点间速度 v={v:.3}m/s 差得太多（bdS 不是 speed/3.6）"
        );
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
            sub.points.clone();

        let mut types = std::collections::HashSet::new();
        for p in &arr {
            types.insert(p["type"].as_i64().unwrap());
        }
        // 真机分布 {0: 89, -1: 17, 2: 5, 5: 1, 6: 1, 7: 1}：类型要有变化
        assert!(types.len() >= 3, "点位类型要有变化，实际 {types:?}");
        // -1（灰点）必须出现：真机起步段整段都是 -1，是我们漏掉的取值
        assert!(types.contains(&-1), "真机起步段有 -1 灰点，实际 {types:?}");
        // 真机全程没有 type=1，我们也不该再发
        assert!(!types.contains(&1), "真机 114 点里 type=1 出现 0 次");
        // 1 / 7 / 5 这些也得在合法集合里
        for t in &types {
            assert!(
                [0, -1, 2, 5, 6, 7].contains(t),
                "type={t} 不在真机出现过的取值集合里"
            );
        }
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
            alt_range: None,
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
            alt_range: None,
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
        let v = verify_view(&sub.save_record, &sub.points).unwrap();

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
        let v = verify_view(&sub.save_record, &sub.points).unwrap();
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
        let v = verify_view(&sub.save_record, &sub.points).unwrap();
        assert_eq!(v.start_time_ms, p.start_time_ms);
        assert!(v.stop_time_ms > v.start_time_ms);
    }
}

#[cfg(test)]
mod step_distance_tests {
    use super::tests::test_sim;
    use super::*;

    /// `stepDistance` 必须与真机同形：**整场一个常量**、起步窗口 0。
    ///
    /// 这条断言原来叫 `step_distance_is_local_stride`，断的是「局部步幅
    /// 1.0~1.5m」——把「真机是局部量」这个**猜测**写成了法律，结果代码发的
    /// 是逐点 0~3.9 跳变的锯齿，真机却是全点同值（AUDIT-round A6）。
    ///
    /// 真机证据（`evidence/obs_real_20261002.json` 114 点）：`stepDistance`
    /// 的唯一值集合 `{0.0, 3.7468809127807674}`，即起步窗口 0、其余全点同值。
    /// 那个常量的语义没有证据（与 670 步 / 1683.83m / 449.4 都不构成简单
    /// 关系），所以只能锁**形态 + 量级**，不锁语义。
    #[test]
    fn step_distance_is_run_constant() {
        let p = SubmitParams {
            uid: "1".into(), unid: "1".into(),
            uuid: "00000000-0000-4000-8000-000000000000".into(),
            sport_type: 1, total_distance_m: 2000, total_time_sec: 720,
            total_steps: 1800, start_time_ms: 1_790_000_000_000,
            stop_time_ms: 1_790_000_720_000, sel_distance_m: 2000,
            sel_run_time_sec: 720, policy: "0".into(), run_area_id: 0,
            geo_fences_json: "[]".into(), total_ascent: 0, alt_range: None,
        };
        let t: Vec<TrackPoint> = (0..400).map(|i| TrackPoint {
            lat: 31.2304 + i as f64 * 5.0 / 111_320.0,
            lon: 121.4737,
            time_ms: 1_790_000_000_000 + i as i64 * 1800,
        }).collect();
        let sim = test_sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let arr: Vec<serde_json::Value> =
            sub.points.clone();

        let vals: Vec<f64> = arr.iter()
            .filter_map(|q| q.get("stepDistance").and_then(|v| v.as_f64()))
            .collect();
        assert_eq!(vals.len(), arr.len(), "stepDistance 有缺失");

        // ① 唯一值集合只有两个：{0.0, 常量} —— 与真机同形
        let mut uniq: Vec<f64> = vals.iter().copied().filter(|&v| v != 0.0).collect();
        uniq.sort_by(|a, b| a.partial_cmp(b).unwrap());
        uniq.dedup();
        assert_eq!(
            uniq.len(), 1,
            "非 0 的 stepDistance 应该只有一个值（真机全点同值），实际 {} 种",
            uniq.len()
        );
        let konst = uniq[0];

        // ② 0 只出现在**开头**（起步窗口），出去以后不再回到 0
        let su = vals.iter().take_while(|&&v| v == 0.0).count();
        assert!(su >= 2, "起步窗口至少 2 个点，实际 {su}");
        assert!(
            vals[su..].iter().all(|&v| v > 0.0),
            "出起步窗口后 stepDistance 不应再回 0（真机只有开头一段是 0）"
        );

        // ③ 量级：真机 3.7468809127807674；语义未定，只要求和真机同带 ±5%
        let real = 3.746_880_912_780_767_4_f64;
        assert!(
            (konst / real - 1.0).abs() <= 0.05,
            "stepping 常量 {konst:.4} 与真机 {real:.4} 相差超过 5%（形态照抄，量级也要同带）"
        );
        // 整场同值：真机 114 点里这个字段的跨度是 0
        let span = vals[su..].iter().cloned().fold(f64::MIN, f64::max)
            - vals[su..].iter().cloned().fold(f64::MAX, f64::min);
        assert_eq!(span, 0.0, "stepDistance 在窗口外必须是常量，实际跨度 {span}");
    }

    // ---- 官方折算：卡路里 / 功率 ----

    #[test]
    fn met_follows_official_segments() {
        // 官方是三段拟合而非连续函数：阈值 10 和 12 处各有一个约 1 MET 的
        // 台阶（33/10+4.1=7.4 对 102/10−3.7=6.5）。这里固定住三段各自的
        // 取值与段内单调性，防止有人「顺手改成连续函数」。
        assert!((met_for_pace(6.0) - 9.6).abs() < 0.01, "p<10 段公式变了");
        assert!((met_for_pace(10.0) - 6.5).abs() < 0.01, "10≤p<12 段公式变了");
        assert!((met_for_pace(12.0) - 3.8).abs() < 0.01, "p≥12 段公式变了");
        // 段内单调：配速越快（p 越小）MET 越高
        assert!(met_for_pace(5.0) > met_for_pace(9.0));
        assert!(met_for_pace(10.5) > met_for_pace(11.5));
        assert!(met_for_pace(12.0) > met_for_pace(20.0));
        // 非法输入不产生 NaN/负值
        assert_eq!(met_for_pace(0.0), 0.0);
        assert_eq!(met_for_pace(-1.0), 0.0);
        assert_eq!(met_for_pace(f64::NAN), 0.0);
    }

    #[test]
    fn calorie_uses_met_not_fixed_per_km() {
        // 同一个 3km：快跑 18min vs 慢跑 36min。固定 62 kcal/km 会给出同一个
        // 186 kcal；MET 折算下两者必须不同，且慢跑单位时间消耗更低。
        let fast = calorie_for(3000.0, 18 * 60, DEFAULT_WEIGHT_KG);
        let slow = calorie_for(3000.0, 36 * 60, DEFAULT_WEIGHT_KG);
        assert!(fast > 0 && slow > 0);
        assert_ne!(fast, slow, "快慢跑卡路里相同，说明还在用固定 kcal/km");
        let fast_per_h = fast as f64 / 0.3;
        let slow_per_h = slow as f64 / 0.6;
        assert!(fast_per_h > slow_per_h, "快跑单位时间消耗应更高");
        // 量级检查：68kg 跑 3km 大约 150~260 kcal
        assert!((140..=280).contains(&fast), "3km 卡路里 {fast} 量级异常");
    }

    #[test]
    fn avg_power_is_nonzero_and_scales_with_speed() {
        let slow = avg_power_for(3000.0, 1200, DEFAULT_WEIGHT_KG);
        let fast = avg_power_for(3000.0, 600, DEFAULT_WEIGHT_KG);
        assert!(slow > 0, "慢跑功率不应为 0");
        assert!(fast > slow, "速度翻倍功率应更高");
        // 1.1 * 68 * 2.5 m/s = 187 W
        assert_eq!(avg_power_for(2500.0, 1000, DEFAULT_WEIGHT_KG), 187,
            "1.1*68*2.5=187W，且必须整数：服务端整型会截断，发小数就是 10501");
    }

    // ---- 圈与爬升 ----

    fn rich_points(n: usize, total_m: f64, total_s: i64) -> Vec<TrackPoint> {
        let lat0 = 31.2304;
        (0..n).map(|i| TrackPoint {
            lat: lat0 + (i as f64) * (total_m / (n - 1) as f64) / 111_320.0,
            lon: 121.4737,
            time_ms: 1_790_000_000_000 + (i as i64) * (total_s * 1000 / (n - 1) as i64),
        }).collect()
    }

    #[test]
    fn laps_split_every_1000m() {
        let p = SubmitParams {
            uid: "1".into(), unid: "1".into(),
            uuid: "00000000-0000-4000-8000-000000000002".into(),
            sport_type: 1, total_distance_m: 2400, total_time_sec: 840,
            total_steps: 2400, start_time_ms: 1_790_000_000_000,
            stop_time_ms: 1_790_000_840_000, sel_distance_m: 2000,
            sel_run_time_sec: 700, policy: "0".into(), run_area_id: 0,
            geo_fences_json: "[]".into(), total_ascent: 0, alt_range: None,
        };
        let t = rich_points(400, 2400.0, 840);
        let sim = test_sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let arr: Vec<serde_json::Value> =
            sub.points.clone();
        // 真机点位里没有 steps 字段，圈步数由总步数按里程分配
        let laps = build_laps_from_points(&arr, p.start_time_ms, p.total_steps as i64);

        assert_eq!(laps.len(), 3, "2400m 应切 3 圈，实际 {}", laps.len());
        let sum_d: f64 = laps.iter().map(|l| l["distance"].as_f64().unwrap()).sum();
        assert!((sum_d - 2400.0).abs() < 0.05, "圈里程和 {sum_d} != 2400");
        assert_eq!(laps[0]["isFullLap"], true);
        assert_eq!(laps[1]["isFullLap"], true);
        assert_eq!(laps[2]["isFullLap"], false, "末圈必须标 isFullLap=false");
        // 圈时长累加必须等于总时长（切圈点按比例 round，允许每圈 1s 误差）
        let sum_t: i64 = laps.iter().map(|l| l["duration"].as_i64().unwrap()).sum();
        assert!((sum_t - 840).abs() <= laps.len() as i64,
            "圈时长和 {sum_t} 偏离总时长 840 超过切圈舍入误差");
        // 圈步数累加必须等于 totalSteps（末圈正好落在终点，没有余数）
        let sum_steps: i64 = laps.iter().map(|l| l["step"].as_i64().unwrap()).sum();
        assert_eq!(sum_steps, p.total_steps as i64, "圈步数和应等于总步数");
        assert_eq!(laps[0]["lapIndex"], 1);
        assert_eq!(laps[2]["lapIndex"], 3);
    }

    #[test]
    fn total_ascent_follows_bd_a_series() {
        let p = SubmitParams {
            uid: "1".into(), unid: "1".into(),
            uuid: "00000000-0000-4000-8000-000000000003".into(),
            sport_type: 1, total_distance_m: 2000, total_time_sec: 720,
            total_steps: 2000, start_time_ms: 1_790_000_000_000,
            stop_time_ms: 1_790_000_720_000, sel_distance_m: 2000,
            sel_run_time_sec: 720, policy: "0".into(), run_area_id: 0,
            geo_fences_json: "[]".into(), total_ascent: 0, alt_range: None,
        };
        let t = rich_points(300, 2000.0, 720);
        let sim = test_sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let arr: Vec<serde_json::Value> =
            sub.points.clone();

        let expect = total_ascent_from_points(&arr);
        let declared = sub.save_record["totalAscent"].as_i64().unwrap();
        assert_eq!(declared, expect as i64, "报文的 totalAscent 与点位反算值不一致");
        // pace 的地点派生海拔自带缓坡，2000m 上必须有非零爬升
        assert!(expect > 0, "2000m 轨迹爬升为 0，海拔序列是平的");
        // 圈爬升是逐点累加（真机每公里 ~55m），总爬升是平滑后累加（真机
        // 17~22m）——这两个口径本来就不一致，App 那边也是分开算的。
        // 只要求前者不小于后者：平滑只会削掉噪声，不该凭空造出爬升。
        let laps = build_laps_from_points(&arr, p.start_time_ms, p.total_steps as i64);
        let lap_sum: f64 = laps.iter().map(|l| l["elevationGain"].as_f64().unwrap()).sum();
        assert!((expect as f64) <= lap_sum,
            "平滑后的总爬升 {expect} 不应超过逐点累加的圈爬升和 {lap_sum}");
        // 上报值也必须出现在签名参与字段里（否则服务端算签名会不一致）
        assert!(sub.canonical.contains(&format!("totalAscent={expect}")),
            "签名串里的 totalAscent 不是点位反算值: {}", sub.canonical);
        assert!(sub.canonical.contains("avgPower="),
            "签名串缺 avgPower");
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    fn params() -> SubmitParams {
        SubmitParams {
            uid: "1".into(),
            unid: "2".into(),
            uuid: "00000000-0000-4000-8000-000000000009".into(),
            sport_type: 1,
            total_distance_m: 2000,
            total_time_sec: 720,
            total_steps: 2000,
            start_time_ms: 1_790_000_000_000,
            stop_time_ms: 1_790_000_720_000,
            sel_distance_m: 2000,
            sel_run_time_sec: 720,
            policy: "0".into(),
            run_area_id: 0,
            geo_fences_json: "[]".into(),
            total_ascent: 0,
            alt_range: None,
        }
    }

    fn sim(p: &SubmitParams, pts: &[TrackPoint]) -> crate::track::SimResult {
        crate::track::SimResult {
            speeds: vec![2.8; pts.len()],
            cadences: vec![160.0; pts.len()],
            strides: vec![1.05; pts.len()],
            total_steps: p.total_steps,
            avg_cadence: 160.0,
            speed_buckets: Vec::new(),
            step_buckets: Vec::new(),
            actual_length_m: p.total_distance_m as f64,
        }
    }

    fn track() -> Vec<TrackPoint> {
        let n = 300;
        (0..n)
            .map(|i| TrackPoint {
                lat: 31.2304 + i as f64 * 2000.0 / (n - 1) as f64 / 111_320.0,
                lon: 121.4737,
                time_ms: 1_790_000_000_000 + i as i64 * 720_000 / (n - 1) as i64,
            })
            .collect()
    }

    #[test]
    fn audit_passes_on_freshly_built_body() {
        let p = params();
        let t = track();
        let sim = sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let probs = audit_derived(&sub.save_record, &sub.points);
        assert!(probs.is_empty(), "新建报文不该有派生字段问题: {probs:?}");
    }

    #[test]
    fn audit_catches_constant_fallbacks() {
        let p = params();
        let t = track();
        let sim = sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let mut body = sub.save_record.clone();
        // 退回三种常量填法
        body["totalAscent"] = json!(0);
        body["calorie"] = json!(124); // 2km × 62
        body["avgPower"] = json!(0);
        let probs = audit_derived(&body, &sub.points);
        assert!(probs.iter().any(|s| s.contains("totalAscent")), "{probs:?}");
        assert!(probs.iter().any(|s| s.contains("calorie")), "{probs:?}");
        assert!(probs.iter().any(|s| s.contains("avgPower")), "{probs:?}");
    }

    #[test]
    fn audit_catches_wrong_speed_unit() {
        let p = params();
        let t = track();
        let sim = sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let mut body = sub.save_record.clone();
        // 把 m/s 当 speed 发（常见错误：速度 ×1000 而不是配速 ×1000）
        body["speed"] = json!((2000.0 / 720.0 * 1000.0) as i64);
        let probs = audit_derived(&body, &sub.points);
        assert!(probs.iter().any(|s| s.contains("speed")), "{probs:?}");
    }

    /// 签名串里的数值必须是**整数文本**。
    ///
    /// 10501「验证签名失败」踩过一次：avgPower 按 f64 发 187.5，服务端
    /// avgPower 是整型（参考实现 RunDetail.avg_power: i64），反序列化截断成
    /// 187 后拿它重算签名，而我们拼的串里是 "187.5" —— 两边不同，直接驳回。
    /// body 内部完全自洽，本地 audit 也查不出来，只有真发一次才暴露。
    #[test]
    fn signature_numeric_fields_are_integers() {
        let p = params();
        let t = track();
        let sim = sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();

        // 这些是字符串/标识字段，允许非数字
        let text_keys = [
            "address", "geeToken", "goalId", "policy", "uuid",
            "selectedUnid", "uid",
        ];
        for kv in sub.canonical.split('&') {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            if text_keys.contains(&k) {
                continue;
            }
            assert!(
                !v.contains('.'),
                "签名串字段 {k}={v} 带小数点：服务端整型会截断，重算签名与这里不一致（10501）"
            );
        }
    }

    #[test]
    fn avg_power_is_integer_watt() {
        let p = params();
        let t = track();
        let sim = sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let v = &sub.save_record["avgPower"];
        assert!(v.is_i64() || v.is_u64(), "avgPower 必须是 JSON 整数，实际 {v}");
        assert!(v.as_i64().unwrap() > 0, "avgPower 不应为 0");
    }

    /// 五点数组的每项必须带真机那套字段。
    ///
    /// 缺 `isPass` / `state` / `id` / `flag` 时服务端存不下点位，详情页
    /// 回显 fivePointJson 是空串 —— App 那边既没有点位连线，点位判定
    /// （「按顺序通过所有点位」）也永远是 false。
    #[test]
    fn five_point_items_carry_device_fields() {
        let p = params();
        let t = track();
        let sim = sim(&p, &t);
        let fixed = vec![
            FixedPoint {
                lat: 31.2324, lon: 121.4737,
                point_name: "秋实城东南".into(), glat: 31.2251, glon: 121.4680,
            },
            FixedPoint {
                lat: 31.2330, lon: 121.4737,
                point_name: "梅园2栋西".into(), glat: 31.2257, glon: 121.4680,
            },
        ];
        let sub = build(&p, &t, &fixed, &sim).unwrap();

        let env: Value = serde_json::from_str(sub.save_record["fivePointJson"].as_str().unwrap())
            .expect("fivePointJson 应是字符串化 JSON");
        assert!(env.get("fivePointJson").is_some(), "外层 envelope 要带 fivePointJson");
        let inner: Vec<Value> =
            serde_json::from_str(env["fivePointJson"].as_str().unwrap()).unwrap();
        assert!(!inner.is_empty(), "点位数组不能为空");
        for (i, f) in inner.iter().enumerate() {
            for k in ["lon", "lat", "glon", "glat", "isFixed", "pointName",
                      "position", "flag", "id", "isPass", "state"] {
                assert!(f.get(k).is_some(), "第 {i} 个点缺字段 {k}");
            }
            assert!(f["isPass"].is_boolean(), "isPass 必须是 bool");
            // 真机是 81/82/83…，不是 1/2/3…
            assert_eq!(
                f["id"],
                json!(FIXED_POINT_ID_BASE + i as i64),
                "id 应从 {FIXED_POINT_ID_BASE} 起连续递增"
            );
        }
    }

    /// 必到点发号回归：真机 rrid 1328288734 的五个点长这样。
    ///
    /// 这是「App 详情页必到点显示不全」的那条回归。改前的报文里
    /// `position` 填的是该点在轨迹数组里的首次经过下标（实测 0/71/88/170/51），
    /// 既不连续也不递增，数组顺序还跟着乱；`isFixed` 落在第 4 个点上；
    /// `id` 从 1 起。App 按 position 排序/取下标，于是只显示得出一部分点。
    ///
    /// 现在钉死四条：position == 0..N-1、数组下标 == position（即按途经顺序
    /// 排列）、isFixed 只有首点 1、id 从 81 连续、flag 全场同值。
    #[test]
    fn fixed_points_numbered_like_real_device() {
        let p = params();
        let t = track();
        let sim = sim(&p, &t);
        // 轨迹沿 lat 递增（2000m / 300 点 ≈ 每点 6.7m），所以「首次经过下标」
        // 严格单调。故意把必到点按打乱的顺序传进来（220/30/290/90/150），
        // 用来验证重排。
        let at = |idx: usize| (t[idx].lat, t[idx].lon);
        let names = ["P220", "P30", "P290", "P90", "P150"];
        let idxs = [220usize, 30, 290, 90, 150];
        let fixed: Vec<FixedPoint> = idxs
            .iter()
            .zip(names.iter())
            .map(|(&i, n)| {
                let (lat, lon) = at(i);
                FixedPoint {
                    lat,
                    lon,
                    point_name: (*n).to_string(),
                    // glat/glon 故意给 0：必须由 BD-09 反算出有效坐标，
                    // 真机两套坐标都是真值，发 0 等于「这个点没有 GCJ 坐标」。
                    glat: 0.0,
                    glon: 0.0,
                }
            })
            .collect();

        let sub = build(&p, &t, &fixed, &sim).unwrap();
        let env: Value = serde_json::from_str(sub.save_record["fivePointJson"].as_str().unwrap())
            .unwrap();
        let inner: Vec<Value> =
            serde_json::from_str(env["fivePointJson"].as_str().unwrap()).unwrap();
        assert_eq!(inner.len(), 5, "五个必到点一个都不能少");

        // ① position == [0,1,2,3,4]，且数组下标 == position
        let positions: Vec<i64> = inner.iter().map(|f| f["position"].as_i64().unwrap()).collect();
        assert_eq!(positions, vec![0, 1, 2, 3, 4], "position 必须是 0..N-1 连续序号");

        // ② 数组顺序 = 途经顺序（按首次经过下标升序）
        let got: Vec<&str> = inner.iter().map(|f| f["pointName"].as_str().unwrap()).collect();
        assert_eq!(
            got,
            vec!["P30", "P90", "P150", "P220", "P290"],
            "数组必须按途经顺序排列，实际 {got:?}"
        );

        // ③ isFixed 只有第一个点 1，其余 0（真机：只有首点 1）
        let fixed_flags: Vec<i64> = inner.iter().map(|f| f["isFixed"].as_i64().unwrap()).collect();
        assert_eq!(fixed_flags, vec![1, 0, 0, 0, 0], "isFixed 只能首点为 1");

        // ④ id 从 81 起连续；flag 全场同值
        let ids: Vec<i64> = inner.iter().map(|f| f["id"].as_i64().unwrap()).collect();
        assert_eq!(ids, vec![81, 82, 83, 84, 85], "id 必须从 {FIXED_POINT_ID_BASE} 起连续");
        assert!(ids[0] >= 80, "id 基数必须落在真机量级（≥80）");
        let flags: Vec<i64> = inner.iter().map(|f| f["flag"].as_i64().unwrap()).collect();
        assert!(
            flags.iter().all(|&f| f == p.start_time_ms),
            "flag 必须是整场固定起始戳 {0}，实际 {flags:?}",
            p.start_time_ms
        );

        // ⑤ 两套坐标都是有效真值（不是 0 / -1），且 GCJ 是反算出来的
        for f in &inner {
            let lat = f["lat"].as_f64().unwrap();
            let lon = f["lon"].as_f64().unwrap();
            let glat = f["glat"].as_f64().unwrap();
            let glon = f["glon"].as_f64().unwrap();
            assert!(lat != 0.0 && lon != 0.0, "BD-09 坐标不能是 0：{}", f["pointName"]);
            assert!(glat != 0.0 && glon != 0.0, "GCJ-02 坐标不能是 0：{}", f["pointName"]);
            assert!((lat - glat).abs() > 1e-6 || (lon - glon).abs() > 1e-6,
                "BD-09 与 GCJ-02 是两套坐标，不该相等");
        }

        // ⑥ isPass 按真实匹配填（这五个点轨迹都经过）
        assert!(
            inner.iter().all(|f| f["isPass"].as_bool() == Some(true)),
            "轨迹经过的点 isPass 应为 true"
        );

        // ⑦ runAreaId：envelope 发 -1（真机值），顶层 save_record 保持真实围栏 id。
        //    真机那份 fixed_point_json 里是 -1，真实 id 只在 geoFencesJson[0].id；
        //    提交报文顶层那个字段没样本依据，不动。
        let env_area = env["runAreaId"].as_i64().unwrap();
        assert_eq!(env_area, payload_run_area_id(p.run_area_id));
        assert!(
            env_area == -1 || std::env::var("PACE_RUN_AREA_ID").is_ok(),
            "envelope 的 runAreaId 默认必须是 -1（真机值），实际 {env_area}"
        );
        assert_eq!(
            sub.save_record["runAreaId"],
            json!(p.run_area_id),
            "顶层 save_record 的 runAreaId 必须保持真实围栏 id"
        );

        // ⑧ 内部「打卡点落在第几个采样点」仍然算得对（type=2 标记靠它）
        let cp = checkpoint_track_indexes(&t, &idxs.iter().map(|&i| at(i)).collect::<Vec<_>>());
        assert_eq!(cp.len(), 5, "五个必到点都该在轨迹上找到");
        // 通道顺序与输入一一对应（这里输入是打乱的），所以不能要求 cp 递增。
        // 命中判定半径 10m、采样间距 ≈6.7m，首次命中的采样点最多比目标点早两个。
        for (hit, &want) in cp.iter().zip(idxs.iter()) {
            assert!(
                *hit <= want && want - *hit <= 2,
                "第 {want} 点的首次命中下标 {hit} 偏差过大"
            );
        }
    }
}

/// 与真机样本的回归对拍。
///
/// 参照样本：`evidence/obs_real_20261002.json`（rrid 1328288734，1683.83m /
/// 555s / 114 点 / 25 段 / 55 速度桶 / 55 步频桶 / 2 圈）。这里用同样的
/// 量级造一条轨迹，把「已经对齐真机」的几条硬约束钉死，防止以后改回去。
#[cfg(test)]
mod parity_regression_tests {
    use super::tests::test_sim;
    use super::*;
    use crate::track::TrackPoint;

    /// 真机这条记录的起始毫秒戳，直接拿来当 flag 的期望值。
    const REAL_FLAG: i64 = 1_790_913_882_164;

    fn params() -> SubmitParams {
        SubmitParams {
            uid: "123456".into(),
            unid: "88".into(),
            uuid: "00000000-0000-4000-8000-0000000000aa".into(),
            sport_type: 1,
            total_distance_m: 1683,
            total_time_sec: 555,
            total_steps: 670,
            start_time_ms: REAL_FLAG,
            stop_time_ms: REAL_FLAG + 555_000,
            sel_distance_m: 1000,
            sel_run_time_sec: 300,
            policy: "0".into(),
            run_area_id: 1668,
            geo_fences_json: "[]".into(),
            total_ascent: 0,
            alt_range: None,
        }
    }

    /// 555 秒 / 1683m，5 秒一个采样点（真机 114 点，这里 112 点）。
    fn track_555() -> Vec<TrackPoint> {
        let n = 112;
        let total_m = 1683.0;
        (0..n)
            .map(|i| TrackPoint {
                lat: 47.3801 + (i as f64) * (total_m / (n - 1) as f64) / 111_320.0,
                lon: 123.9521,
                time_ms: REAL_FLAG + (i as i64) * (555_000 / (n - 1) as i64),
            })
            .collect()
    }

    /// 真机 114 点的**蒸馏表**，原文件
    /// `evidence/obs_real_20261002.json`（rrid 1328288734）。
    ///
    /// 每行 `(gLat, gLng, totalTime_s, totalDis_m, speed, avgSpeed, bdS)`，
    /// 与真机 `raw_points` 同序、同下标 —— 逐点对拍要的就是这个「同下标」，
    /// 用区间夹逼是查不出单位认反的（AUDIT-round A2 就是这么漏过去的）。
    ///
    /// 坐标是 **GCJ-02**（真机 `gLat`/`gLng`），喂进生成链路前要转回 BD-09，
    /// 因为 `point_json_full` 出口会再 BD-09 → GCJ-02 转一次。
    #[rustfmt::skip]
    const REAL_POINTS: [(f64, f64, i64, f64, f64, f64, f64); 114] = [
    (47.38012824076674, 123.95210748142215, 0, 0.0, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 0, 0.0, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 3, 0.0, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 8, 0.0, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 13, 0.0, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 18, 4.0, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 23, 4.0, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 28, 4.0, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 33, 4.0, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 38, 4.0, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 43, 4.0, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 48, 5.6, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 53, 7.199999999999999, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 58, 7.199999999999999, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 63, 9.6, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 68, 10.4, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 73, 12.000000000000002, 0.0, 0.0, 0.0),
    (47.38012824076674, 123.95210748142215, 78, 13.600000000000003, 0.0, 0.0, 0.0),
    (47.38004459827185, 123.95195709567511, 83, 18.400000000000006, 97.77, 0.17, 0.93),
    (47.379999039181, 123.9516680425907, 88, 40.74569931030274, 3.73, 4.47, 2.75),
    (47.37992001218835, 123.95131048842643, 93, 69.06515731811524, 2.94, 5.66, 4.55),
    (47.379799473023425, 123.95103981171661, 98, 93.45792427062989, 3.42, 4.88, 4.89),
    (47.37964237087478, 123.95082927820029, 103, 117.04709091186524, 3.53, 4.72, 4.57),
    (47.379445154272005, 123.9505619379944, 108, 146.81432189941407, 2.8, 5.95, 5.16),
    (47.37937624609859, 123.95025953860996, 113, 170.8380397796631, 3.47, 4.81, 5.5),
    (47.379369104769054, 123.95000059010816, 118, 190.35163917541504, 4.27, 3.9, 4.53),
    (47.37934393353767, 123.94991037309497, 123, 197.6985339164734, 11.34, 1.47, 2.8),
    (47.3793326133789, 123.95010249220806, 128, 212.2187460899353, 5.74, 2.91, 2.03),
    (47.37941983988595, 123.95039486789193, 133, 236.275009059906, 3.46, 4.81, 3.34),
    (47.379542224820064, 123.95076577610881, 138, 267.3415545463562, 2.68, 6.21, 5.1),
    (47.37972274099764, 123.95101640960726, 143, 294.8920420646667, 3.02, 5.51, 5.52),
    (47.37983980828289, 123.95121357315197, 148, 314.6361826896667, 4.22, 3.95, 4.56),
    (47.37993532957078, 123.95147756550875, 153, 337.1730803489685, 3.7, 4.51, 4.12),
    (47.380029344151495, 123.95182510063974, 158, 365.3512457847595, 2.96, 5.64, 4.8),
    (47.38004335961603, 123.95218265472106, 163, 392.317884349823, 3.09, 5.39, 5.21),
    (47.38001730126829, 123.9525051216446, 168, 416.7699011802673, 3.41, 4.89, 4.97),
    (47.379947846406, 123.95279918249634, 173, 440.2191428184509, 3.55, 4.69, 4.64),
    (47.379928208451766, 123.95298966026704, 178, 454.72623052597044, 5.74, 2.9, 3.78),
    (47.38002691942443, 123.95317013328282, 183, 472.19400968551633, 4.77, 3.49, 3.38),
    (47.380207009573866, 123.95319021271101, 188, 492.2761134147644, 4.15, 4.02, 3.77),
    (47.380357081435115, 123.95320527467096, 193, 509.00183668136594, 4.98, 3.35, 3.5),
    (47.38045542973894, 123.95319860658141, 198, 519.9491890907287, 7.61, 2.19, 2.55),
    (47.38047041129054, 123.9531869125671, 203, 521.8334328174591, 44.09, 0.38, 1.09),
    (47.380478743747865, 123.95318524297409, 208, 522.768448972702, 88.65, 0.19, 0.0),
    (47.38045864403342, 123.95313678342555, 213, 527.0472280025482, 19.47, 0.86, 0.5),
    (47.380346913221715, 123.95312005668332, 218, 539.5347987651825, 6.67, 2.5, 1.54),
    (47.38017854173231, 123.95312504275863, 223, 558.2606193065643, 4.45, 3.75, 2.81),
    (47.38003020183168, 123.95314339926095, 228, 574.813069486618, 5.03, 3.31, 3.25),
    (47.379881981961425, 123.95322357954777, 233, 592.3652640819549, 4.75, 3.51, 3.19),
    (47.37983540593643, 123.95327704153232, 238, 598.9246684551239, 12.7, 1.31, 2.13),
    (47.37983708607756, 123.95328372547847, 243, 599.4614873170852, 154.32, 0.11, 0.57),
    (47.37983708607756, 123.95328372547847, 248, 599.4614873170852, 0.0, 0.0, 0.0),
    (47.37981712914218, 123.95330878614308, 253, 602.3743739366531, 57.08, 0.29, 0.43),
    (47.37969364366418, 123.95324861348618, 258, 616.8334899187088, 5.76, 2.89, 1.62),
    (47.37944176564769, 123.95317672433285, 263, 645.3593096971512, 2.92, 5.71, 3.7),
    (47.37918338125551, 123.9531883798256, 268, 674.1037459611892, 2.9, 5.75, 5.16),
    (47.37890828318055, 123.95317831082875, 273, 704.7026534318924, 2.72, 6.12, 5.65),
    (47.378654818982575, 123.95314819435438, 278, 732.9776683092117, 2.95, 5.66, 5.71),
    (47.37842470124285, 123.95312142343492, 283, 758.6448703050613, 3.25, 5.13, 5.15),
    (47.378237997258346, 123.95312974842226, 288, 779.414870762825, 4.01, 4.16, 4.35),
    (47.37809133064749, 123.95315144700936, 293, 795.8050914049148, 5.08, 3.28, 3.45),
    (47.37793941317281, 123.95304448515735, 298, 814.5192504167557, 4.45, 3.74, 3.42),
    (47.37782892645044, 123.95280887250826, 303, 836.0987220048904, 3.86, 4.32, 4.11),
    (47.37776178758178, 123.95257326975708, 308, 855.3457179307937, 4.33, 3.85, 4.05),
    (47.37773953299092, 123.95226917310016, 313, 878.3766017198562, 3.62, 4.61, 4.2),
    (47.37775224761448, 123.95194169589243, 318, 903.075141453743, 3.37, 4.94, 4.71),
    (47.37779177281747, 123.95168440120226, 323, 922.9409060716629, 4.19, 3.97, 4.32),
    (47.37783801299064, 123.951450501754, 328, 941.2879958391189, 4.54, 3.67, 3.78),
    (47.37790596245912, 123.95123498681973, 333, 959.1883330583572, 4.65, 3.58, 3.64),
    (47.3779689184744, 123.95102281510763, 338, 976.630597615242, 4.78, 3.49, 3.52),
    (47.37799523305369, 123.95082901773844, 343, 991.5133614778518, 5.6, 2.98, 3.2),
    (47.37803315745767, 123.95060181082663, 348, 1009.1333356142044, 4.73, 3.52, 3.04),
    (47.3780978624657, 123.95043141327744, 353, 1023.8433251619339, 5.67, 2.94, 3.12),
    (47.37821609173147, 123.95035625258654, 358, 1038.156199002266, 5.82, 2.86, 3.05),
    (47.378311027084564, 123.95030614818371, 363, 1049.3664260149003, 7.43, 2.24, 2.76),
    (47.378414267668965, 123.9502393388426, 368, 1061.9000706911088, 6.65, 2.51, 2.53),
    (47.3785190992705, 123.95013076424917, 373, 1076.137836956978, 5.85, 2.85, 2.67),
    (47.378762261656384, 123.94999882355316, 378, 1104.9436021089555, 2.89, 5.76, 4.0),
    (47.37897025403889, 123.94977833652383, 383, 1133.412977719307, 2.93, 5.69, 5.33),
    (47.379121731277806, 123.94964972512601, 388, 1152.8418164491654, 4.29, 3.89, 4.52),
    (47.379254948875776, 123.94956454666817, 393, 1168.9837384462357, 5.16, 3.23, 3.36),
    (47.37931361587675, 123.94973996716168, 398, 1183.715115094185, 5.65, 2.95, 2.97),
    (47.37932083675729, 123.95004401854634, 403, 1206.6226563692094, 3.64, 4.58, 3.43),
    (47.37935136138456, 123.95032636041779, 408, 1228.150755429268, 3.87, 4.31, 4.0),
    (47.37943507757987, 123.95052184039402, 413, 1245.5660062074662, 4.78, 3.48, 3.7),
    (47.37955553723255, 123.95075241271523, 418, 1267.4934048891068, 3.8, 4.39, 3.71),
    (47.37967592131991, 123.95094122034827, 423, 1287.0199589967729, 4.27, 3.91, 3.62),
    (47.37977624501615, 123.9510999538336, 428, 1303.3689065217973, 5.1, 3.27, 3.04),
    (47.37983824726949, 123.9512703778082, 433, 1317.9356398820878, 5.72, 2.91, 2.91),
    (47.379912011549344, 123.95148925710512, 438, 1336.3442244768144, 4.53, 3.68, 3.23),
    (47.379944010926124, 123.95166135080007, 443, 1349.781482243538, 6.2, 2.69, 3.09),
    (47.37996765992677, 123.95182509088285, 448, 1362.3874259233476, 6.61, 2.52, 2.57),
    (47.37999795316086, 123.95197546709275, 453, 1374.2002606630326, 7.05, 2.36, 2.3),
    (47.3800015698135, 123.95212416951877, 458, 1385.4038443803788, 7.43, 2.24, 2.13),
    (47.38003986958711, 123.95210078419349, 463, 1390.0122256517411, 18.08, 0.92, 1.65),
    (47.38000768395106, 123.95183178036123, 468, 1410.5802836656571, 4.05, 4.11, 2.34),
    (47.37996705859001, 123.95150597337364, 473, 1435.5239768266679, 3.34, 4.99, 4.07),
    (47.379859767198845, 123.95118851537894, 478, 1462.2385306596757, 3.12, 5.34, 4.91),
    (47.379740921238074, 123.95093120640365, 483, 1485.6902556657792, 3.55, 4.69, 4.91),
    (47.37958377981886, 123.95069895528664, 488, 1510.411063694954, 3.37, 4.95, 4.87),
    (47.37942487812637, 123.95041491653765, 493, 1538.1523699998857, 3.0, 5.55, 5.16),
    (47.379355938133834, 123.95009414276961, 498, 1563.4922461748124, 3.29, 5.07, 5.02),
    (47.379360535166235, 123.94987195178939, 503, 1580.2298484086991, 4.98, 3.35, 4.0),
    (47.37936009042946, 123.94962637466223, 508, 1598.720561528206, 4.5, 3.7, 3.32),
    (47.37938471822801, 123.9494158866584, 513, 1614.8040328264237, 5.18, 3.22, 3.1),
    (47.379296404744096, 123.94944093112562, 518, 1624.8034615755082, 8.33, 2.0, 2.34),
    (47.379305083779094, 123.94963137770927, 521, 1639.1755271196366, 5.8, 2.88, 2.33),
    (47.37932033285804, 123.94976669770541, 526, 1649.5045380830766, 8.07, 2.07, 2.42),
    (47.37932888979794, 123.94988865283285, 531, 1658.7362790346147, 9.02, 1.85, 1.93),
    (47.37935418569678, 123.9500473650592, 536, 1671.0130334138871, 6.79, 2.46, 1.92),
    (47.37936940642633, 123.95016598191214, 541, 1680.103196644783, 9.16, 1.82, 1.76),
    (47.37937781542679, 123.95020607844538, 546, 1683.2637322187425, 26.29, 0.63, 1.02),
    (47.3793728170545, 123.95020774829058, 551, 1683.8335697293282, 146.2, 0.11, 0.0),
    (47.37937447808701, 123.95020440728243, 555, 1683.8335697293282, 260.42, 0.06, 0.0),
    ];

    /// 逐点对拍：拿真机的**坐标 + 时间戳**造一条同形轨迹，我们生成的
    /// `speed` / `avgSpeed` / `bdS` 必须与真机同下标、同量级、同口径。
    ///
    /// 允许的误差（全部是实测残差上留的余量，不是拍脑袋的区间）：
    ///
    /// | 字段 | 系数 | 逐点允许 | 实测（n=89） |
    /// |---|---|---|---|
    /// | `speed` | 配速 1000/(60v) | 相对 ≤3% | 中位 0.049%、最大 0.31% |
    /// | `avgSpeed` | 点间速度 v | 相对 ≤3% | 中位 0.069%、最大 0.83% |
    /// | `bdS` | v 的两拍低通 | 绝对 ≤1.15 m/s | 中位 0.19、最大 1.01 |
    /// | 交叉 | `真机 speed × 我们的 avgSpeed` | 落 16.5~16.8 | 实测 16.62~16.68 |
    ///
    /// 最后一行的交叉断言才是判「单位有没有认反」的硬证据：把 `speed` 当
    /// km/h 发时这个乘积会跳到 16.667/v² 量级（v≈3 时是 1.8），一测就红。
    ///
    /// 跳过的下标（都不参与逐点对拍，理由写在断言里）：
    /// - 起步窗口里的点：真机三项是硬 0，我们也是硬 0，没有残差可言；
    /// - **真机自身滤波残差点**：设备端 `avgSpeed` 与 `ΔtotalDis/Δt` 差 >2%
    ///   的点（idx 50/52/106/112/113）。这些点真机自己的两个字段就不自洽
    ///   （idx113：位移 0 而 avgSpeed=0.06、配速 260.42），比的是真机的滤波
    ///   器状态，不是口径 —— 留着只会把口径断言变成「猜滤波器」。
    ///   （idx18 那类「窗口刚结束、滤波器还有残差」的点落在起步窗口里，
    ///   本来就不进对拍。）
    #[test]
    fn point_speed_fields_match_real_device_point_by_point() {
        let pts: Vec<TrackPoint> = REAL_POINTS
            .iter()
            .map(|&(la, lo, t, ..)| {
                // 真机坐标是 GCJ-02，生成链路入口要 BD-09
                let (bl, bo) = crate::api::coord::gcj02_to_bd09(la, lo);
                TrackPoint { lat: bl, lon: bo, time_ms: REAL_FLAG + t * 1000 }
            })
            .collect();
        let p = params();
        let sim = test_sim(&p, &pts);
        let sub = build(&p, &pts, &[], &sim).expect("build");
        let ours = &sub.points;
        assert_eq!(ours.len(), REAL_POINTS.len(), "点位数量应与真机一致");

        // 起步窗口：真机是 0..17 全 0（18 点）；我们按时间戳切，切出来的
        // 个数与真机可能差一个点（真机第 83 秒那点仍在窗口外），所以边界
        // 那一点不进对拍。
        let times: Vec<i64> = pts.iter().map(|q| q.time_ms).collect();
        let su = crate::track::startup_point_count(&times, p.start_time_ms, p.total_time_sec);

        // OBS 明细那条链要一起对拍，否则「自检守 A、上传发 B」再犯时这里照样
        // 全绿。点位现在是单点源（`build_points`，AUDIT-round A4 已修），所以
        // 这里从**真正上传的对象**里解出点位：`detail_payload_rrid` 收的就是
        // `sub.points`，只要谁把它改回「自己再拼一份」，这里立刻红。
        let obs_pts: Vec<serde_json::Value> = {
            use base64::Engine;
            use flate2::read::GzDecoder;
            use std::io::Read;
            let five = sub.save_record["fivePointJson"].as_str().expect("fivePointJson");
            let obj = crate::api::flow::detail_payload_rrid(
                &sim, "123", 99, p.start_time_ms, "U", five, &sub.points,
            );
            let raw = obj["run_data"].as_str().expect("run_data 是 gzip+base64");
            let bytes = base64::engine::general_purpose::STANDARD.decode(raw).expect("base64");
            let mut text = String::new();
            GzDecoder::new(&bytes[..]).read_to_string(&mut text).expect("gunzip");
            let run: serde_json::Value = serde_json::from_str(&text).expect("run_data JSON");
            serde_json::from_str(run["allLocJson"].as_str().expect("allLocJson")).expect("allLocJson")
        };
        // 两条链必须是同一套口径（逐字段相同；`smooth_point_speed` 幂等）
        for (i, (a, b)) in obs_pts.iter().zip(ours.iter()).enumerate() {
            for k in ["speed", "avgSpeed", "bdS", "stepDistance"] {
                let (x, y) = (a[k].as_f64().unwrap(), b[k].as_f64().unwrap());
                // 两条链反算 v 的输入精度不同：`point_json_full` 用全精度
                // 球面距离，`smooth_point_speed` 只能用报文里的 totalDis
                // （4 位小数）与整秒 totalTime。慢点处（v≈0.19）这 0.2mm 的
                // 舍入折到配速上是 0.002% 量级，所以给 0.2% 的相对余量。
                let tol = (y.abs() * 0.002).max(1e-3);
                assert!(
                    (x - y).abs() <= tol,
                    "第 {i} 点 {k} 在 OBS 链与自检链不一致：{x} vs {y}（容差 {tol}）"
                );
            }
        }

        // 真机自身的滤波残差点：avgSpeed 与 ΔtotalDis/Δt 差 >2%
        let is_filter_residual = |i: usize| -> bool {
            if i == 0 {
                return true;
            }
            let dt = (REAL_POINTS[i].2 - REAL_POINTS[i - 1].2) as f64;
            let dd = REAL_POINTS[i].3 - REAL_POINTS[i - 1].3;
            let raw = if dt > 0.0 { dd / dt } else { 0.0 };
            let real = REAL_POINTS[i].5;
            if (raw > 0.0) != (real > 0.0) {
                return true;
            }
            raw > 0.0 && ((real - raw) / raw).abs() > 0.02
        };

        let mut checked = 0usize;
        let mut rel_speed: Vec<f64> = Vec::new();
        let mut rel_avg: Vec<f64> = Vec::new();
        for chain in [&obs_pts, ours] {
        for i in (su + 1)..REAL_POINTS.len() {
            let (_, _, _, _, real_sp, real_av, real_bd) = REAL_POINTS[i];
            if real_av <= 0.0 || is_filter_residual(i) {
                continue;
            }
            let q = &chain[i];
            let sp = q["speed"].as_f64().unwrap();
            let av = q["avgSpeed"].as_f64().unwrap();
            let bd = q["bdS"].as_f64().unwrap();

            // ① 口径自洽：speed 就是 avgSpeed 的配速
            let want = 1_000.0 / (60.0 * av);
            assert!(
                (sp - want).abs() <= want * 0.01 + 0.01,
                "第 {i} 点 speed={sp} 应为配速 {want:.4}（换算按 v×3.6 的话是 {:.4}）",
                av * 3.6
            );
            // ② 交叉判单位：真机配速 × 我们的点间速度
            let cross = real_sp * av;
            assert!(
                (16.5..=16.8).contains(&cross),
                "第 {i} 点 真机 speed({real_sp}) × 我们的 avgSpeed({av}) = {cross:.4}，不在 16.5~16.8：speed 被当成 km/h 发了（A2 的那个错）"
            );
            // ③ 逐点相对误差
            let rs = (sp - real_sp).abs() / real_sp;
            let ra = (av - real_av).abs() / real_av;
            assert!(
                rs <= 0.03,
                "第 {i} 点 speed={sp} vs 真机 {real_sp}，相对误差 {:.2}% > 3%",
                rs * 100.0
            );
            assert!(
                ra <= 0.03,
                "第 {i} 点 avgSpeed={av} vs 真机 {real_av}，相对误差 {:.2}% > 3%",
                ra * 100.0
            );
            // ④ bdS：同一个量纲（m/s），且与真机滤波值的差在实测残差范围内
            let db = (bd - real_bd).abs();
            assert!(
                db <= 1.15,
                "第 {i} 点 bdS={bd} vs 真机 {real_bd}，差 {db:.3} m/s > 1.15（bdS 是 m/s 低通值；若发成 speed/3.6 会差 60% 以上）"
            );
            rel_speed.push(rs);
            rel_avg.push(ra);
            checked += 1;
        }
        }

        // 对拍面必须够大：114 点里除去起步窗口（0..18）与 5 个滤波残差点，
        // 每条链剩 88 点，两条链合计 176。
        assert!(checked >= 160, "只对拍了 {checked} 个点（两条链各 ~88），覆盖不足");
        // 中位残差（逐点断言之外再看整体，防「个别点对上一片点偏」）
        let med = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let ms = med(&mut rel_speed);
        let ma = med(&mut rel_avg);
        assert!(ms <= 0.005, "speed 中位相对误差 {:.3}% > 0.5%", ms * 100.0);
        assert!(ma <= 0.005, "avgSpeed 中位相对误差 {:.3}% > 0.5%", ma * 100.0);
    }

    fn build_all() -> (SubmitParams, Vec<TrackPoint>, Vec<Value>, Vec<Value>, Vec<Value>) {
        let p = params();
        let t = track_555();
        let sim = test_sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).expect("build");
        let pts = sub.points.clone();
        let segs = build_segments_from_points(&pts, p.start_time_ms, p.total_steps as i64);
        let laps = build_laps_from_points(&pts, p.start_time_ms, p.total_steps as i64);
        (p, t, pts, segs, laps)
    }

    /// `segment_json`：起步段 + 20 秒窗口 + 收尾段，与真机 25 段同构。
    #[test]
    fn segments_match_real_device_shape() {
        let (p, _t, pts, segs, _laps) = build_all();

        // 真机 25 段：首段 86s 吃掉 4 个 20 秒窗口，所以段数比 ceil(555/20)=28
        // 少。这里按同样规则切，也正好是 25 段。
        let expect = 25;
        assert!(
            (expect - 1..=expect + 1).contains(&segs.len()),
            "段数 {} 应接近 {expect}（ceil(555/20)≈28，首段吃掉 4 个窗口）",
            segs.len()
        );

        // ① 首段 state==131072(0x20000)，其余段 0
        assert_eq!(segs[0]["state"], 131072, "首段 state 必须是 0x20000");
        assert!(
            segs[1..].iter().all(|s| s["state"] == 0),
            "非首段 state 必须是 0"
        );

        // ② 末段 = 余数（0 < ≤20 秒）；中段严格 20 秒
        //
        // 原来末段钉死 10 秒、让倒数第二段吸收 20~39 秒的余数，于是中间
        // 会冒出一个 35 秒的段——「中间段严格 20 秒」当场自相矛盾。现在
        // 余数全给末段：555s → 首段 83s + 23×20s + 末段 12s = 25 段，
        // 与真机的 25 段同数（真机 86 + 23×20 + 9）。
        let dur = |s: &Value| {
            (s["endTime"].as_i64().unwrap() - s["startTime"].as_i64().unwrap()) / 1000
        };
        let rest = p.total_time_sec - crate::track::startup_sec(p.total_time_sec);
        let expect_last = if rest % 20 == 0 { 20 } else { rest % 20 };
        assert_eq!(dur(segs.last().unwrap()), expect_last, "末段应是余数");
        assert!(
            dur(segs.last().unwrap()) > 0 && dur(segs.last().unwrap()) <= 20,
            "末段不能超过 20 秒"
        );
        for (i, s) in segs.iter().enumerate().skip(1).take(segs.len() - 2) {
            assert_eq!(dur(s), 20, "第 {i} 个中间段必须严格 20 秒");
        }

        // ③ 首尾相接无空洞，时长和精确等于 totalTime
        for w in segs.windows(2) {
            assert_eq!(
                w[0]["endTime"], w[1]["startTime"],
                "相邻段之间不能有空洞"
            );
        }
        let sum_dur: i64 = segs.iter().map(dur).sum();
        assert_eq!(sum_dur, p.total_time_sec, "段时长和必须等于 totalTime");

        // ④ 段里程和 ≈ totalDis（±1%）
        let last_dis = pts.last().unwrap()["totalDis"].as_f64().unwrap();
        let sum_d: f64 = segs
            .iter()
            .map(|s| s["distance"].as_f64().unwrap())
            .sum();
        assert!(
            (sum_d - last_dis).abs() < last_dis * 0.01,
            "段里程和 {sum_d:.1}m 与 totalDis {last_dis:.1}m 差超过 1%"
        );

        // ⑤ `avgSpeed` 是配速（分/公里）：duration/distance 换算必须吻合。
        //    真机 743 段 113.286m / 20s → 2.94 分/公里，avgSpeed 2.9414。
        let mut checked = 0;
        for s in segs.iter().skip(1) {
            let d = s["distance"].as_f64().unwrap();
            if d < 10.0 {
                continue;
            }
            let want = (dur(s) as f64 / 60.0) / (d / 1000.0);
            let got = s["avgSpeed"].as_f64().unwrap();
            assert!(
                (got - want).abs() < 0.01,
                "段 {} 的 avgSpeed={got:.4} 与配速换算 {want:.4} 不符（不是 m/s？）",
                s["id"]
            );
            checked += 1;
            if checked == 3 {
                break;
            }
        }
        assert_eq!(checked, 3, "至少要有 3 段可校验");

        // ⑥ 固定字段与连续 id
        for (i, s) in segs.iter().enumerate() {
            assert_eq!(s["speedStandard"], 900, "speedStandard 恒 900");
            assert_eq!(s["intervalStepModel"], 0, "intervalStepModel 恒 0");
            assert_eq!(s["avgStep"], s["originAvgStep"], "avgStep == originAvgStep");
            assert_eq!(s["flag"], p.start_time_ms, "段 flag 是整场起始戳");
            if i > 0 {
                assert_eq!(
                    s["id"].as_i64().unwrap(),
                    segs[i - 1]["id"].as_i64().unwrap() + 1,
                    "段 id 必须连续递增"
                );
            }
        }
        // 段步数之和 ≈ totalSteps
        let sum_steps: i64 = segs.iter().map(|s| s["avgStep"].as_i64().unwrap()).sum();
        assert!(
            (sum_steps - p.total_steps as i64).abs() <= segs.len() as i64,
            "段步数和 {sum_steps} 与 totalSteps {} 差太多",
            p.total_steps
        );
    }

    /// 起步窗口与段里程自洽：整条链条（规划 → 压平 → 报点 → 分段）走一遍。
    ///
    /// 这条守的是上一轮那个真实缺陷：字段清零了，但**几何没动**——起步 86 秒
    /// 里匀速跑掉了 15% 的里程，首段 `distance` 虚高到 250m 量级（真机 18.4m），
    /// 而且段表加起来和 `totalDis` 对不上（实测 1559.8m vs 1833.5m）。
    ///
    /// 六条断言对应六条实测基准，数字全部来自真机 rrid 1328288734
    /// （555 秒 / 1683.83m / 114 点 / 25 段）。
    #[test]
    fn startup_window_and_segment_distance_sum() {
        use crate::track::{
            compress_startup, path_length, plan, retime, simulate, KinParams, PlanRequest, SimParams,
        };
        use rand::SeedableRng;

        let dur = 555i64;
        let want_m = 1683.83f64;
        let start = REAL_FLAG;

        // ① 规划：无打卡点 → 合成环点（和 flow 的规划支路同一套）
        let req = PlanRequest {
            center_lat: 47.3801,
            center_lng: 123.9521,
            checkpoints: Vec::new(),
            total_distance_m: want_m,
            total_sec: dur,
            start_ms: start,
            sport_type: 1,
            sample_step_m: 5.0,
        };
        let mut rng = rand::rngs::StdRng::seed_from_u64(2026);
        let planned = plan(&req, &mut rng).expect("规划");
        let xy: Vec<(f64, f64)> = planned.points.iter().map(|p| (p.lat, p.lon)).collect();

        // ② 时间轴：按压平前的几何分配（压平只动坐标，不动时间戳）
        let (times, _, _) = retime(&xy, dur, start, start as u64, &KinParams::default(), 1.0, 8.0);
        let mut track: Vec<TrackPoint> = xy
            .iter()
            .zip(&times)
            .map(|((a, b), t)| TrackPoint { lat: *a, lon: *b, time_ms: *t })
            .collect();

        // ③ 起步窗口压平
        compress_startup(&mut track, start, dur);
        let real_len = path_length(
            &track.iter().map(|q| (q.lat, q.lon)).collect::<Vec<_>>(),
        )
        .round() as i64;

        // ④ 步态合成 + 报文
        let timed: Vec<(f64, f64, i64)> = track.iter().map(|q| (q.lat, q.lon, q.time_ms)).collect();
        let sim = simulate(
            &timed,
            &SimParams {
                total_distance_m: real_len as f64,
                total_sec: dur,
                start_ms: start,
                seed: start as u64,
                ..Default::default()
            },
        )
        .expect("合成");
        let mut p = params();
        p.total_distance_m = real_len;
        p.total_steps = sim.total_steps;
        let sub = build(&p, &track, &[], &sim).expect("build");
        let pts = sub.points.clone();
        let segs = build_segments_from_points(&pts, start, sim.total_steps as i64);

        let dur_of = |s: &Value| {
            (s["endTime"].as_i64().unwrap() - s["startTime"].as_i64().unwrap()) / 1000
        };
        let last_dis = pts.last().unwrap()["totalDis"].as_f64().unwrap();
        let sum_d: f64 = segs.iter().map(|s| s["distance"].as_f64().unwrap()).sum();

        // ① Σ段距离 == 末点 totalDis（≤0.5%），且报文 totalDis 同源
        assert!(
            (sum_d - last_dis).abs() <= last_dis * 0.005,
            "Σ段距离 {sum_d:.2}m 与末点 totalDis {last_dis:.2}m 差 {:.2}%（上限 0.5%）",
            (sum_d - last_dis).abs() / last_dis * 100.0
        );
        assert!(
            (sub.save_record["totalDis"].as_f64().unwrap() - last_dis).abs() <= 1.0,
            "报文 totalDis {} 与末点 totalDis {last_dis:.2} 对不上",
            sub.save_record["totalDis"]
        );

        // ② 首段 state==131072，其余 0
        assert_eq!(segs[0]["state"], 131_072, "首段 state 必须是 0x20000");
        assert!(
            segs[1..].iter().all(|s| s["state"] == 0),
            "非首段 state 必须是 0"
        );

        // ③ 段间无缝、末段对齐终点
        for w in segs.windows(2) {
            assert_eq!(w[0]["endTime"], w[1]["startTime"], "相邻段之间不能有缝隙");
        }
        assert_eq!(
            segs.last().unwrap()["endTime"].as_i64().unwrap(),
            start + dur * 1000,
            "末段 endTime 必须等于 stopTime"
        );

        // ④ 首段时长 ≈ 15.5% 总时长（±2s）；其后严格 20s，末段是余数
        let first_dur = dur_of(&segs[0]);
        let want_first = (dur as f64 * 0.155) as i64;
        assert!(
            (first_dur - want_first).abs() <= 2,
            "首段时长 {first_dur}s 与真机比例 {want_first}s 差超过 2 秒"
        );
        for (i, s) in segs.iter().enumerate().skip(1).take(segs.len() - 2) {
            assert_eq!(dur_of(s), 20, "第 {i} 段必须严格 20 秒");
        }
        let tail_dur = dur_of(segs.last().unwrap());
        assert!(
            (1..=20).contains(&tail_dur),
            "末段应是 ≤20 秒的余数，实际 {tail_dur}s"
        );

        // ⑤ 首段距离 ≈ 1.1% 总里程（±30%）——真机 18.4/1683.83
        let first_dis = segs[0]["distance"].as_f64().unwrap();
        let want_dis = last_dis * 0.011;
        assert!(
            (first_dis - want_dis).abs() <= want_dis * 0.30,
            "首段距离 {first_dis:.2}m 偏离真机比例 {want_dis:.2}m 超过 30%\
             （说明起步窗口只清了字段，几何没压平）"
        );

        // ⑥ 起步窗口内的点：speed / bdS / avgSpeed / stepDistance 全 0
        let times_ms: Vec<i64> = track.iter().map(|q| q.time_ms).collect();
        let su = crate::track::startup_point_count(&times_ms, start, dur);
        assert!(su >= 2, "起步窗口至少 2 个点，实际 {su}");
        assert!(
            (su as f64 / pts.len() as f64) < 0.25,
            "起步窗口占了 {:.1}% 的点，太多",
            su as f64 / pts.len() as f64 * 100.0
        );
        for (i, q) in pts.iter().take(su).enumerate() {
            for k in ["speed", "bdS", "avgSpeed", "stepDistance"] {
                assert_eq!(q[k].as_f64().unwrap(), 0.0, "第 {i} 点 {k} 应为 0");
            }
        }
        assert!(
            pts[su]["speed"].as_f64().unwrap() > 0.0,
            "出起步窗口后速度必须起来"
        );
        // 窗口内的位移必须很小（真机 86 秒 18.4m），不能还是匀速
        let in_window_dis = pts[su - 1]["totalDis"].as_f64().unwrap();
        assert!(
            in_window_dis < last_dis * 0.03,
            "起步窗口内走了 {in_window_dis:.1}m，超过总里程的 3%：几何没压平"
        );
    }

    /// 点位：flag 全同、lat/lng 恒 -1.0、无 steps、取值落在真机区间。
    #[test]
    fn points_match_real_device_fields() {
        let (p, _t, pts, _segs, _laps) = build_all();

        // ① flag 全点唯一值只有 1 个
        let flags: std::collections::HashSet<i64> = pts
            .iter()
            .map(|q| q["flag"].as_i64().unwrap())
            .collect();
        assert_eq!(flags.len(), 1, "所有点的 flag 必须同值，实际 {flags:?}");
        assert!(flags.contains(&p.start_time_ms), "flag 应是整场起始戳");

        for (i, q) in pts.iter().enumerate() {
            // ② lat/lng 恒 -1.0
            assert_eq!(q["lat"].as_f64().unwrap(), -1.0, "第 {i} 点 lat != -1.0");
            assert_eq!(q["lng"].as_f64().unwrap(), -1.0, "第 {i} 点 lng != -1.0");
            // ③ 没有 steps 字段
            assert!(q.get("steps").is_none(), "第 {i} 点多了 steps 字段");
            // ④ 真机区间
            let c = q["count"].as_i64().unwrap();
            assert!((40..=53).contains(&c), "第 {i} 点 count={c}");
            let r = q["radius"].as_f64().unwrap();
            assert!((1.4..=2.3).contains(&r), "第 {i} 点 radius={r}");
            let a = q["bdA"].as_f64().unwrap();
            assert!((142.0..=158.5).contains(&a), "第 {i} 点 bdA={a}");
            let b = q["bdD"].as_f64().unwrap();
            assert!((0.0..360.0).contains(&b), "第 {i} 点 bdD={b}");
            let qn = q["queueNum"].as_i64().unwrap();
            assert!((0..=24).contains(&qn), "第 {i} 点 queueNum={qn}");
            let st = q["state"].as_i64().unwrap();
            assert!([1, 2, 3].contains(&st), "第 {i} 点 state={st} 不在真机三态里");
            let ty = q["type"].as_i64().unwrap();
            assert!(
                [0, -1, 2, 5, 6, 7].contains(&ty),
                "第 {i} 点 type={ty} 不在真机取值集合里"
            );
        }

        // ⑤ radius / count 都必须有抖动，不能是常量
        let rad: std::collections::HashSet<i64> = pts
            .iter()
            .map(|q| (q["radius"].as_f64().unwrap() * 100.0) as i64)
            .collect();
        assert!(rad.len() >= 4, "radius 只有 {} 种取值，等于常量", rad.len());
        let cnt: std::collections::HashSet<i64> = pts
            .iter()
            .map(|q| q["count"].as_i64().unwrap())
            .collect();
        assert!(cnt.len() >= 6, "count 只有 {} 种取值，等于常量", cnt.len());

        // ⑥ id 从一个大整数起、连续递增（真机 3516~3629）
        let ids: Vec<i64> = pts.iter().map(|q| q["id"].as_i64().unwrap()).collect();
        assert!(ids[0] >= 3000, "点 id 起点要在大整数区间，实际 {}", ids[0]);
        assert!(
            ids.windows(2).all(|w| w[1] == w[0] + 1),
            "点 id 必须连续递增"
        );

        // ⑦ 首点 speed / bdS 都是 0.0（真机首点如此）；
        //    **不是** bdS == speed/3.6 —— 那是把配速当 km/h 的错口径
        //    （AUDIT-round A2）。真机的不变量是 speed × avgSpeed = 1000/60。
        assert_eq!(pts[0]["speed"].as_f64().unwrap(), 0.0, "首点 speed 应为 0");
        assert_eq!(pts[0]["bdS"].as_f64().unwrap(), 0.0, "首点 bdS 应为 0");
        for (i, q) in pts.iter().enumerate() {
            let sp = q["speed"].as_f64().unwrap();
            let av = q["avgSpeed"].as_f64().unwrap();
            let bs = q["bdS"].as_f64().unwrap();
            if av <= 0.0 {
                assert_eq!(sp, 0.0, "第 {i} 点 avgSpeed=0 但 speed={sp}");
                assert_eq!(bs, 0.0, "第 {i} 点 avgSpeed=0 但 bdS={bs}");
                continue;
            }
            let want = 1_000.0 / (60.0 * av);
            assert!(
                (sp - want).abs() <= want * 0.01 + 0.01,
                "第 {i} 点 speed={sp} 应为配速 {want:.4}（avgSpeed={av} m/s）"
            );
            let gap = (bs - av).abs();
            assert!(
                gap <= 3.2,
                "第 {i} 点 bdS={bs} 与 avgSpeed={av} 差 {gap:.2}，量纲不是一个东西"
            );
        }

        // ⑧ 起步窗口的 speed / bdS / avgSpeed / stepDistance 全是 0
        //    （真机前 18 点如此：GPS 刚锁定，还没进测速窗口）
        //
        // 窗口按**时间戳**数（与生产链路同一口径），不再用点数比例估算：
        // 估算口径在采样间隔抖动时会切偏一两个点，测试和生产就对不上。
        let su = {
            let times: Vec<i64> = _t.iter().map(|q| q.time_ms).collect();
            crate::track::startup_point_count(&times, p.start_time_ms, p.total_time_sec)
        };
        assert!(su >= 2, "起步窗口至少 2 个点");
        for (i, q) in pts.iter().take(su).enumerate() {
            assert_eq!(q["speed"].as_f64().unwrap(), 0.0, "第 {i} 点 speed 应为 0");
            assert_eq!(q["bdS"].as_f64().unwrap(), 0.0, "第 {i} 点 bdS 应为 0");
            assert_eq!(q["avgSpeed"].as_f64().unwrap(), 0.0, "第 {i} 点 avgSpeed 应为 0");
            assert_eq!(q["stepDistance"].as_f64().unwrap(), 0.0, "第 {i} 点 stepDistance 应为 0");
            // 起步段里除首点(7)/第二点(5) 之外都是 -1 灰点
            assert!(
                q["type"].as_i64().unwrap_or(0) < 0 || i < 2,
                "第 {i} 点：起步段中段应是 -1 灰点，实际 type={}",
                q["type"]
            );
        }
        assert!(
            pts[su]["speed"].as_f64().unwrap() > 0.0,
            "出起步窗口后速度要起来"
        );

        // ⑨ 类型分布要有 -1 灰点、状态要有收尾段
        let mut types = std::collections::HashMap::new();
        let mut states = std::collections::HashMap::new();
        for q in &pts {
            *types.entry(q["type"].as_i64().unwrap()).or_insert(0) += 1;
            *states.entry(q["state"].as_i64().unwrap()).or_insert(0) += 1;
        }
        assert!(types.get(&-1).copied().unwrap_or(0) >= 10, "起步灰点太少: {types:?}");
        assert!(states.get(&3).copied().unwrap_or(0) >= pts.len() / 5, "收尾段太少: {states:?}");
        assert!(
            states.get(&1).copied().unwrap_or(0) > states.get(&3).copied().unwrap_or(0),
            "运动段应占多数: {states:?}"
        );
    }

    /// 圈数据的海拔基准跟着点位走（真机 endAltAbs 146.6，不是两千多）。
    #[test]
    fn laps_altitude_follows_real_elevation() {
        let (p, _t, pts, _segs, laps) = build_all();
        assert!(!laps.is_empty(), "1683m 至少要切 1 圈");

        // 全程累计爬升必须与真机同量级：真机 62.99m / 1.683km ≈ 37.4 m/km，
        // 早先的正弦模型只给 6.7 m/km，圈 elevationGain 只有真机的 1/5。
        let asc: f64 = pts
            .windows(2)
            .map(|w| {
                (w[1]["bdA"].as_f64().unwrap() - w[0]["bdA"].as_f64().unwrap()).max(0.0)
            })
            .sum();
        let last_dis = pts.last().unwrap()["totalDis"].as_f64().unwrap();
        let per_km = asc / (last_dis / 1000.0);
        assert!(
            (25.0..=60.0).contains(&per_km),
            "每公里累计爬升 {per_km:.1}m，真机 37.4m/km"
        );
        for l in &laps {
            let abs = l["endAltAbs"].as_f64().unwrap();
            assert!(
                (140.0..=160.0).contains(&abs),
                "endAltAbs={abs} 不在真机海拔区间（齐齐哈尔 ~145m）"
            );
            let gain = l["elevationGain"].as_f64().unwrap();
            assert!(
                (5.0..=80.0).contains(&gain),
                "elevationGain={gain} 量级不对（真机单圈 25.9 / 37.1）"
            );
            assert_eq!(l["flag"], p.start_time_ms, "圈 flag 也是整场起始戳");
        }
        // 圈步数和必须等于总步数
        let sum: i64 = laps.iter().map(|l| l["step"].as_i64().unwrap()).sum();
        assert_eq!(sum, p.total_steps as i64, "圈步数和 != totalSteps");
    }
}
