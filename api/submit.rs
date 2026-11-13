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

/// 打卡点。
///
/// 字段对齐服务端 `points` 接口返回的原始结构，不是自己拼的三个字段。
/// 点位响应是 `{lon, lat, isFixed, pointName, glon, glat}`，服务端要靠
/// 这套字段把轨迹和它自己发的打卡点对上号；只回传 lat/lon/position 时
/// 它认不出是同一个点，`按顺序通过所有点位` 就会判 False（实测 6 条全中）。
#[derive(Debug, Clone)]
pub struct FixedPoint {
    pub lat: f64,
    pub lon: f64,
    pub position: i32,
    /// 服务端原始字段，原样回显。
    pub is_fixed: i64,
    pub point_name: String,
    pub glat: f64,
    pub glon: f64,
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
    /// 提交用的点位数组（`allLocJson` 那份）。报文里这个字段现在发空串
    /// （真机/Neko 都这样，轨迹只走 OBS），所以派生字段自检从这里取点。
    pub points: Vec<Value>,
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
    _start_pt: (f64, f64),
    prev: &mut (f64, f64),
    cum: &mut (f64, i64, i64),
    prev_time: &mut i64,
    total_steps: i64,
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
    let prev_cum = *cum; // 本段的起点累计量，供局部步幅计算
    cum.0 += step_m;
    cum.1 = (p.time_ms - start_ms) / 1000;
    *prev = (p.lat, p.lon);

    // 本段步数：累计步数按点序线性分配后取差
    let cum_steps_now = ((total_steps as f64) * (idx as f64 + 1.0) / n as f64).round() as i64;
    let cum_steps_prev = ((total_steps as f64) * idx as f64 / n as f64).round() as i64;
    let seg_steps = cum_steps_now - cum_steps_prev;

    // 瞬时速度用真实采样间隔，不是固定 5 秒
    let gap_sec = if idx == 0 {
        0.0
    } else {
        ((p.time_ms - *prev_time) as f64 / 1000.0).max(0.1)
    };
    *prev_time = p.time_ms;

    let dt = if idx == 0 { 0.0 } else { (p.time_ms - start_ms) as f64 / 1000.0 };
    let avg_speed = if dt > 0.5 { (cum.0 / dt).clamp(0.0, 6.3) } else { 0.0 };
    // 点间速度只算一次，`speed`（km/h）和 `bdS`（m/s）都从它派生。
    //
    // 早先这两个字段各夹各的区间：`speed` 先夹 1.9~6.3 m/s 再 ×3.6，`bdS`
    // 单独夹 0~5 m/s。速度落在两端时两者互相矛盾——v=0.8 时 speed 被抬到
    // 6.84km/h 而 bdS 仍是 0.8，服务端拿 speed/3.6 和 bdS 一对是 1.9 ≠ 0.8。
    // 首点更离谱：2.6 是当 m/s 写的，却直接当 km/h 发出去，等于每秒 0.72m。
    //
    // 现在只有一份来源，两个字段天然满足 bdS == speed/3.6。
    let v_mps = if idx == 0 {
        // 起步速度 m/s（约 9.4km/h）。不能是 0，首点速度为 0 等于「原地不动」
        2.6
    } else {
        (step_m / gap_sec).clamp(1.9, 5.0)
    };
    let inst = v_mps * 3.6;

    // 点位类型是有语义的，不是装饰。
    //
    // 老版本按概率瞎填（38% 得 3、54% 得 0、3% 得 1、2% 得 2），纯粹凑分布。
    // 真机的 isValidPoint（libswsport.so@0x10e84，XXXXBully 反汇编还原成
    // isvalidpoint.py:130-181）对它打分：type=1 得 30 分、type=2 得 20 分、
    // 其他得 15 分，及格线 65。也就是说「这个采样点算不算有效打卡点」直接
    // 由 type 决定，瞎填等于每个点都拿不到类型分。
    //
    //   0  起跑
    //   1  有效打卡点（最高分）
    //   2  次一级打卡点
    //   6  终点
    //   7  / -1  灰点：GPS 弱，跳变检查豁免
    let ptype = if idx == 0 {
        0
    } else if idx + 1 == n {
        6
    } else if checkpoint_idxs.iter().any(|&c| idx.abs_diff(c) <= 2) {
        // 真机经过打卡点时，连续几个采样点都会被标成打卡点类型——GPS
        // 定位是连续的，一个点位不可能只对应孤零零一个采样点。只标一个
        // 点的话，服务端按范围找点位时看到的密度太低，validPoint 打分
        // 过不了 65 的及格线，这次经过就不作数。
        1
    } else {
        let h = (p.lat.to_bits() ^ p.lon.to_bits() ^ seed).wrapping_mul(6364136223846793005);
        // 途中普通采样只允许 0 / 2，不再随机撒 3 和 8
        match (h >> 33) % 100 {
            0..=11 => 2,
            _ => 0,
        }
    };

    // GPS 精度半径：真机聚在 ~3.8m。早先 1.5~3.0 整体偏小，等于「每一次
    // 定位都精确到 1.5m」，比手持机实际水平还准。
    let h2 = (p.lon.to_bits() ^ seed).wrapping_mul(6364136223846793005);
    let radius = ((3.6 + ((h2 >> 40) % 70) as f64 / 100.0) * 100.0).round() / 100.0;

    // 卫星数：真机 10~34、均值≈28，恒 1 是「只锁定一颗卫星」的机器指纹。
    // 逐点按序号伪随机游走，同 seed 可复现。
    let h3 = (idx as u64).wrapping_mul(0x2545F4914F6CDD1D) ^ seed;
    let count = 10 + ((h3 >> 32) % 25) as i64;

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
    // 真机单条 2km 记录里海拔在 20~64m 之间起伏，每公里累计爬升 50m+——
    // 那是气压计/GPS 高程的传感器噪声，不是地形（平地校园）。
    //
    // 早先用 130m 基准 + 亚米级漂移，全程累计爬升近乎 0，和真机差一个
    // 数量级；服务端按相邻点差值累加就知道这条「没有海拔数据」。
    //
    // 现在用三条不同周期的正弦叠在 ~42m 基准上。关键约束是**相邻点只能动
    // 几十厘米**——真机 2km / 424 点、每公里累计爬升 50m+，反推相邻点平均
    // 变化只有 0.2~0.4m。所以慢项负责整条几十米的漂移，快项只做亚米抖动，
    // 不能拿大幅值高频正弦去凑范围（那会让相邻点跳三四米，比真机假得多）。
    // 相位由 seed 定，同 seed 可复现。
    let ph = |k: u64| {
        ((seed ^ k).wrapping_mul(0x9E3779B97F4A7C15) >> 40) as f64 / 16_777_216.0
            * std::f64::consts::TAU
    };
    let fi = idx as f64;
    // 幅值分配：慢项占 85% 负责整条几十米漂移，中项 10%、快项 5% 只做
    // 亚米抖动。三项加起来正好等于半宽，所以叠加后不会越界，也就不需要
    // 靠 clamp 兜——clamp 会在边界压出一排平点，那才是明显的假。
    let (base, half, lo, hi) = match alt_range {
        Some((lo, hi)) if hi > lo + 1.0 => {
            ((lo + hi) / 2.0, (hi - lo) / 2.0, lo, hi)
        }
        _ => (42.0, 21.0, 20.0, 64.0),
    };
    let altitude = (base
        + half * 0.85 * (fi * 0.02 + ph(1)).sin()
        + half * 0.10 * (fi * 0.15 + ph(2)).sin()
        + half * 0.05 * (fi * 0.5 + ph(3)).sin())
    .clamp(lo, hi);

    // 局部步幅：本段位移 / 本段步数。原来那个表达式化简后是
    // 「累计里程 / 全程步数」，单调递增的累积平均值，不是即时步幅。
    let d_seg = cum.0 - prev_cum.0;
    let step_distance = if d_seg > 0.5 && seg_steps >= 1 {
        let s_seg = seg_steps as f64;
        ((d_seg / s_seg) * 10000.0).round() / 10000.0
    } else {
        0.0
    };

    json!({
        "avgSpeed": (avg_speed * 10000.0).round() / 10000.0,
        // 海拔三兄弟不能是 0：服务端按它们算累计爬升，全 0 等于「在海底跑」
        "bdA": (altitude * 10.0).round() / 10.0,
        "bdD": (bearing * 10.0).round() / 10.0,
        "bdG": 1,
        "bdS": (v_mps * 100.0).round() / 100.0,
        "coorType": "gcj02",
        "count": count,
        "dtr": 0.0,
        // flag 是**这个采样点的定位时刻**，不是整条轨迹的开始时刻。
        //
        // 原来这里写死 start_ms，419 个点的 flag 全一样。App 画轨迹时
        // 按 flag 排点、按相邻两点的 flag 差算时间间隔——全部相等等于
        // 「所有点同时定位」，折线拿不到任何时间维度的信息，地图上就
        // 只剩里程/时长/配速这些标量，画不出线。表现是记录点进去只有
        // 数据、没有轨迹，一看就是刷的。
        //
        // verify_view 也从首点 flag 反解 startTime，说明这个字段本来就
        // 是绝对毫秒时间戳，与 totalTime（累计秒）不是一回事。
        "flag": p.time_ms,
        "gLat": (glat * 1e7).round() / 1e7,
        "gLng": (glng * 1e7).round() / 1e7,
        "gainTime": gain_time,
        "id": idx as i64 + 1,
        // lat/lng 必须是真的 BD-09 坐标。
        //
        // 这里原来写死 -1.0，只有 gLat/gLng 带真坐标。服务端按点位判定时
        // 读的是 lat/lng，拿到 -1 就等于每个采样点都不在任何打卡点附近，
        // 「按顺序通过所有点位」永远不会成立——轨迹明明精确穿过（最近
        // 0.1m），六条记录全是 False。
        "lat": (p.lat * 1e7).round() / 1e7,
        "lng": (p.lon * 1e7).round() / 1e7,
        "locType": 1,
        "locationId": "",
        "queueNum": idx as i32,
        "radius": (radius * 100.0).round() / 100.0,
        "speed": (inst * 10000.0).round() / 10000.0,
        // state 是速度合规三态（0=合规 / 1=慢于下限 / 2=快于上限）。
        // 原来首点硬编码成 1，理由是"起步速度 0 必然慢于下限"。但 state
        // 不是纯展示字段——它参与「这个采样点算不算有效经过」的判定，
        // 而起点往往就是 0 号打卡点，于是 0 号点带着 state=1 被跳过，
        // 「按顺序通过所有点位」永远拼不齐。
        //
        // 真机首点 GPS 速度记 0 但 state 标 0（定位刚锁定，还没进速度
        // 判定窗口）。这里跟随：首点记 0，其余按瞬时速度与允许区间比。
        "state": if idx == 0 {
            0
        } else {
            let lo = 1.39;
            let hi = 8.33;
            let v = step_m / gap_sec;
            if v < lo { 1 } else if v > hi { 2 } else { 0 }
        },
        "steps": ((total_steps as f64) * (idx as f64 + 1.0) / n as f64).round() as i64,
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

/// 逐点 `speed`(km/h) / `bdS`(m/s) 改成前后各 5 点的窗口均速。
///
/// 手机 GPS 速度是多普勒测速，本身平滑，App 配速图上是一条稳定的线。
/// 我们原来用「相邻两点位移 / 间隔」，而轨迹带 GPS 抖动，配速图满屏锯齿。
pub fn smooth_point_speed(items: &mut [Value]) {
    const HALF: usize = 5;
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
    for (i, item) in items.iter_mut().enumerate().take(n).skip(1) {
        let a = i.saturating_sub(HALF);
        let b = (i + HALF).min(n - 1);
        let dt = tim[b] - tim[a];
        if dt <= 0.0 {
            continue;
        }
        // 上限 6.3 m/s 是给异常抖动兜底（世界纪录配速的两倍还多）
        let v = ((dis[b] - dis[a]) / dt).clamp(0.0, 6.3);
        item["speed"] = json!(((v * 3.6) * 10000.0).round() / 10000.0);
        item["bdS"] = json!((v * 100.0).round() / 100.0);
    }
}

/// 从已组装好的点位数组切圈（每 1000m 一圈，末圈 `isFullLap=false`）。
///
/// 结构对齐真机 OBS `laps_json`（13 个字段，见 Neko `track/wire.rs::build_laps`）：
/// `avgPace` 用十进制分钟/公里、`avgStride` 用厘米、`elevationGain` 按圈内
/// 正增量累计、`endAltRel` 相对首点海拔。
///
/// pace 原来发空数组。真机跑 2 公里必有 2 圈，空数组等于「这条轨迹没有
/// 分段信息」，而服务端的分段审核、圈配速展示都读这一列。
pub fn build_laps_from_points(pts: &[Value], start_ms: i64) -> Vec<Value> {
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

    let (mut prev_d, mut prev_t, mut prev_steps, mut prev_gain) = (0.0f64, 0i64, 0i64, 0.0f64);
    // 上限防止异常数据把循环卡死（每圈至少 1m 才继续）
    let mut guard = 0usize;
    while prev_d < last_dis && guard < 10_000 {
        guard += 1;
        let d_now = (prev_d + 1000.0).min(last_dis);
        let (mut t_now, mut steps, mut altitude) = (
            num(last, "totalTime") as i64,
            num(last, "steps") as i64,
            num(last, "bdA"),
        );
        let mut gain = 0.0f64;
        for pair in pts.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            let climb = (num(b, "bdA") - num(a, "bdA")).max(0.0);
            let (da, db) = (num(a, "totalDis"), num(b, "totalDis"));
            if d_now < last_dis && db >= d_now && db > da {
                let k = (d_now - da) / (db - da);
                t_now = (num(a, "totalTime") + (num(b, "totalTime") - num(a, "totalTime")) * k).round() as i64;
                steps = (num(a, "steps") + (num(b, "steps") - num(a, "steps")) * k).round() as i64;
                altitude = num(a, "bdA") + (num(b, "bdA") - num(a, "bdA")) * k;
                gain += climb * k;
                break;
            }
            gain += climb;
        }
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
    let laps = build_laps_from_points(&pts, body.get("startTime").and_then(|v| v.as_i64()).unwrap_or(0));
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

    // 逐点生成，累计量必须顺序推进
    let mut prev = points.first().map(|p| (p.lat, p.lon)).unwrap_or((0.0, 0.0));
    let start_pt = prev;
    let mut cum = (0.0f64, 0i64, 0i64);
    let seed = params.start_time_ms as u64 ^ 0x9E3779B97F4A7C15;
    let mut prev_time = points.first().map(|p| p.time_ms).unwrap_or(params.start_time_ms);
    // 打卡点落在轨迹的哪几个采样点上：position 是首次经过索引
    let cp_idxs: Vec<usize> = fixed
        .iter()
        .filter(|f| f.position >= 0)
        .map(|f| f.position as usize)
        .filter(|&i| i < points.len())
        .collect();
    let mut loc_items: Vec<Value> = points
        .iter()
        .enumerate()
        .map(|(i, p)| {
            point_json_full(
                p, i, points.len(), params.start_time_ms, start_pt,
                &mut prev, &mut cum, &mut prev_time, params.total_steps as i64, seed,
                &cp_idxs, params.alt_range,
            )
        })
        .collect();

    // 配速平滑必须在这里做：逐点 speed 是「相邻两点位移 / 间隔」，轨迹带 GPS
    // 抖动时 App 的配速图满屏锯齿。真机是 GPS 多普勒测速，曲线本来就平滑。
    smooth_point_speed(&mut loc_items);

    // 累计爬升必须从**已生成的点位**反算，与 allLocJson 里的 bdA 物理同源。
    // 原来恒发 0：点位有起伏、总爬升却是 0，服务端按两点海拔差累计时立刻
    // 得到与上报值冲突的两套数字。
    let total_ascent = total_ascent_from_points(&loc_items);

    // 原样回显服务端的点位结构：lon/lat 是 BD-09，glon/glat 是 GCJ-02，
    // isFixed 区分必经点与普通点，pointName 是点位标识。缺任何一个，
    // 服务端都可能认不出这是它发的那批点。
    let fp_items: Vec<Value> = fixed
        .iter()
        .enumerate()
        .map(|(i, f)| {
            json!({
                "lon": f.lon,
                "lat": f.lat,
                "isFixed": f.is_fixed,
                "pointName": f.point_name,
                "glon": f.glon,
                "glat": f.glat,
                "position": f.position,
                // 下面四个是真机五点数组里的必现字段，缺了服务端存不下点位，
                // 详情页回显 fivePointJson 就是空串，App 那边也拿不到点位连线。
                // 参照实现 Neko 的 five_point_payload 逐项对拍：
                //   flag 定位时刻 / id 序号 / isPass 是否经过 / state 速度合规三态
                "flag": params.start_time_ms,
                "id": i as i64 + 1,
                "isPass": f.position >= 0,
                "state": 0,
            })
        })
        .collect();
    // 主报文这一层要 envelope：改成裸数组服务端直接 -10000
    // （成绩上传失败）。记录详情里 fivePointJson 回显 "" 是正常的，
    // 点位数据走的是 OBS 明细里的 fixed_point_json。
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

    /// 每个采样点的 `flag` 必须是它自己的定位时刻，不能整条轨迹一个值。
    #[test]
    fn flag_advances_per_point() {
        let pts: Vec<TrackPoint> = (0..10)
            .map(|i| tp(31.2304 + i as f64 * 2e-4, 121.4737, 1_790_000_000_000 + i * 5000))
            .collect();
        let params = params();
        let sim = test_sim(&params, &pts);
        let sub = build(&params, &pts, &[], &sim).unwrap();
        let arr: Vec<serde_json::Value> = sub.points.clone();

        // 全同 = App 拿不到时间维度的信息，画不出折线，记录只剩标量数据
        let flags: Vec<i64> = arr.iter().map(|p| p["flag"].as_i64().unwrap()).collect();
        assert!(
            flags.windows(2).all(|w| w[1] > w[0]),
            "flag 必须逐点递增，实际 {flags:?}"
        );
        // 首点就是 startTime，verify_view 反过来靠它反解
        assert_eq!(flags[0], params.start_time_ms, "首点 flag 应等于 startTime");
        // 与 totalTime 同源：末点累计秒对应的时间戳要对得上
        let last_total_time = arr.last().unwrap()["totalTime"].as_i64().unwrap();
        assert_eq!(
            flags.last().unwrap() - flags[0],
            last_total_time * 1000,
            "flag 跨度应与 totalTime 一致"
        );
    }

    /// 采样点字段值域必须落在真机观测范围内，且相互自洽。
    ///
    /// 这条测试锁的是「注释声称 vs 代码实际」——早先 `count` 注释写
    /// 10~34，代码 `24 + %11` 只产 24~34，下界永远给不到；`altitude`
    /// 注释写 20~64，实际只到 52。两者都是注释比代码宽，靠肉眼看不出来。
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

        // ① 卫星数：真机 10~34，且不是常量
        let counts: Vec<i64> = arr.iter().map(|p| p["count"].as_i64().unwrap()).collect();
        for &c in &counts {
            assert!((10..=34).contains(&c), "count={c} 超出真机 10~34");
        }
        let uniq: std::collections::HashSet<i64> = counts.iter().copied().collect();
        assert!(uniq.len() >= 8, "count 只有 {} 种取值，等于常量", uniq.len());

        // ② 海拔：真机 20~64，相邻点不许跳变过大（真实 GPS 高程是连续量）
        let alts: Vec<f64> = arr.iter().map(|p| p["bdA"].as_f64().unwrap()).collect();
        for &a in &alts {
            assert!((20.0..=64.0).contains(&a), "bdA={a} 超出真机 20~64");
        }
        let spread = alts.iter().cloned().fold(f64::MIN, f64::max)
            - alts.iter().cloned().fold(f64::MAX, f64::min);
        assert!(spread > 5.0, "海拔跨度只有 {spread:.1}m，太像常量");
        let max_step = alts
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f64::max);
        assert!(max_step < 3.0, "相邻点海拔跳变 {max_step:.2}m，比真机噪声大");

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

        // ⑤ speed(km/h) 与 bdS(m/s) 必须满足 bdS == speed/3.6
        for p in &arr {
            let speed = p["speed"].as_f64().unwrap();
            let bds = p["bdS"].as_f64().unwrap();
            assert!(
                (speed / 3.6 - bds).abs() < 0.02,
                "speed={speed}km/h 与 bdS={bds}m/s 不自洽（speed/3.6={:.2}）",
                speed / 3.6
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
        assert!(max_step < 3.0, "相邻点海拔跳变 {max_step:.2}m");
    }

    #[test]
    fn altitude_range_rejects_bad_input() {
        let pts: Vec<TrackPoint> = (0..30)
            .map(|i| tp(31.2304 + i as f64 * 2e-4, 121.4737, 1_790_000_000_000 + i * 5000))
            .collect();
        let mut params = params();
        // 给反了：应当忽略，退回默认 42±21
        params.alt_range = Some((300.0, 100.0));
        let sim = test_sim(&params, &pts);
        let sub = build(&params, &pts, &[], &sim).unwrap();
        let mean = sub.points.iter().map(|p| p["bdA"].as_f64().unwrap()).sum::<f64>()
            / sub.points.len() as f64;
        assert!((mean - 42.0).abs() < 25.0, "反向区间应退回默认基准，实际均值 {mean:.1}");
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
        // lat/lng 是真实 BD-09，不是占位符。
        //
        // 这个断言原来写的是 `assert_eq!(p0["lat"], -1.0)`，把 bug 当成了
        // 规格：服务端按 lat/lng 判点位，-1 等于每个采样点都不在打卡点附近，
        // 「按顺序通过所有点位」永远 False。
        assert!(
            (p0["lat"].as_f64().unwrap() - 31.2304).abs() < 1e-6,
            "lat 应是原始 BD-09 坐标，实际 {:?}",
            p0["lat"]
        );
        assert!(
            (p0["lng"].as_f64().unwrap() - 121.4737).abs() < 1e-6,
            "lng 应是原始 BD-09 坐标，实际 {:?}",
            p0["lng"]
        );
        // 另一套坐标在 gLat/gLng，已转到 gcj02
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
            sub.points.clone();

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

    /// stepDistance 必须是**局部**步幅（1.0~1.5m 量级），不是累积平均。
    #[test]
    fn step_distance_is_local_stride() {
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
        assert!(!vals.is_empty(), "stepDistance 全缺失");

        // 真实单步步幅 0.8~1.8m。旧实现是「累计里程/总步数」，
        // 前段趋近 0、末段趋近平均值，中间的会远小于 1m。
        let mid: Vec<f64> = vals.iter().skip(vals.len()/4).take(vals.len()/2)
            .copied().filter(|&v| v > 0.0).collect();
        assert!(!mid.is_empty());
        let avg = mid.iter().sum::<f64>() / mid.len() as f64;
        assert!(avg > 0.6 && avg < 2.5, "局部步幅均值 {avg:.2}m 不在合理区间");
        for (i, &v) in mid.iter().enumerate().take(20) {
            assert!(v > 0.3 && v < 4.0, "第{i}个局部步幅 {v}m 异常");
        }
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
        let laps = build_laps_from_points(&arr, p.start_time_ms);

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
        // 圈步数累加必须等于总步数分配后的末值
        let last_steps = arr.last().unwrap()["steps"].as_i64().unwrap();
        let sum_steps: i64 = laps.iter().map(|l| l["step"].as_i64().unwrap()).sum();
        assert_eq!(sum_steps, last_steps);
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
        let laps = build_laps_from_points(&arr, p.start_time_ms);
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
                lat: 47.385199, lon: 123.959484, position: 0, is_fixed: 1,
                point_name: "秋实城东南".into(), glat: 47.378857, glon: 123.953134,
            },
            FixedPoint {
                lat: 47.386100, lon: 123.960100, position: 5, is_fixed: 0,
                point_name: "梅园2栋西".into(), glat: 47.379700, glon: 123.953800,
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
            assert_eq!(f["id"], json!(i as i64 + 1), "id 应从 1 递增");
        }
    }
}
