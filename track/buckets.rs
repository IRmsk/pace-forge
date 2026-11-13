//! 10 秒分桶。
//!
//! 服务端的反作弊核心在这两个数组上，不是总量：
//! - `speedPerTenSec`：每 10 秒的位移
//! - `stepsPerTenSec`：每 10 秒的步数，外加窗口内的 `maxDiff/minDiff/avgDiff`
//!
//! 总量对但分桶像机器，一样判无效。所以桶里必须有自然抖动。

use serde::{Deserialize, Serialize};

use super::geo::distance;

/// 速度桶。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeedBucket {
    pub distance: f64,
    #[serde(rename = "beginTime")]
    pub begin_time: i64,
    #[serde(rename = "endTime")]
    pub end_time: i64,
    /// 整次跑步的起始毫秒戳（每桶都填同一个值）。
    pub flag: i64,
    /// 桶 id：提交用 `60000+i`，OBS 用 `(rrid%100000)*1000+endSec`。
    pub id: i64,
    pub state: i32,
    #[serde(rename = "queueNum")]
    pub queue_num: i32,
}

/// 步频桶。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepBucket {
    #[serde(rename = "stepsNum")]
    pub steps_num: i32,
    #[serde(rename = "beginTime")]
    pub begin_time: i64,
    #[serde(rename = "endTime")]
    pub end_time: i64,
    pub flag: i64,
    pub id: i64,
    #[serde(rename = "maxDiff")]
    pub max_diff: i32,
    #[serde(rename = "minDiff")]
    pub min_diff: i32,
    #[serde(rename = "avgDiff")]
    pub avg_diff: i32,
    pub state: i32,
    #[serde(rename = "queueNum")]
    pub queue_num: i32,
}

/// 按 10 秒切桶，统计每个桶的位移。
///
/// `points` 必须是 (lat, lon, 毫秒时间戳) 且按时间递增。
pub fn speed_buckets(points: &[(f64, f64, i64)], start_ms: i64, total_sec: i64) -> Vec<SpeedBucket> {
    let n = (total_sec / 10).max(1) as usize;
    let mut out = Vec::with_capacity(n);

    // 预先算好每段的距离和起止时间，再按与桶区间的**重叠比例**分配。
    //
    // 原来是单指针 cursor 跨桶不重置，只在 `points[cursor+1].t >= begin`
    // 时才累加距离——点如果已经落在桶之前，那一段距离被永久丢弃，
    // 该桶就成了 0 位移。实测 72 个桶里 17 个是空的，服务端看到的是
    // 「跑着跑着静止十秒」。
    let mut segs: Vec<(f64, f64, i64, i64)> = Vec::with_capacity(points.len().saturating_sub(1));
    for w in points.windows(2) {
        let d = distance((w[0].0, w[0].1), (w[1].0, w[1].1));
        segs.push((d, 0.0, w[0].2, w[1].2));
    }

    for i in 0..n {
        let begin = start_ms + i as i64 * 10_000;
        let end = begin + 10_000;
        let mut dist = 0.0f64;
        for &(d, _, t0, t1) in &segs {
            if t1 <= begin || t0 >= end || d <= 0.0 {
                continue;
            }
            let span = (t1 - t0).max(1) as f64;
            let lo = t0.max(begin);
            let hi = t1.min(end);
            let overlap = (hi - lo).max(0) as f64;
            if overlap > 0.0 {
                dist += d * overlap / span;
            }
        }
        out.push(SpeedBucket {
            distance: (dist * 100.0).round() / 100.0,
            begin_time: begin,
            end_time: end,
            flag: start_ms,
            id: 0,
            state: 0,
            queue_num: 0,
        });
    }
    out
}

/// 按 10 秒切桶，把总步数分配到各桶，并给出桶内波动统计。
///
/// `jitter` 是均匀抖动幅度（如 0.18 表示 ±18%），由调用方用固定种子生成，
/// 保证可复现。
pub fn step_buckets(
    start_ms: i64,
    total_sec: i64,
    total_steps: i32,
    jitter: &[f64],
) -> Vec<StepBucket> {
    let n = (total_sec / 10).max(1) as usize;

    // 先生成带抖动的权重，再归一化到总步数
    let mut weights: Vec<f64> = (0..n)
        .map(|i| {
            let j = jitter.get(i % jitter.len().max(1)).copied().unwrap_or(0.0);
            1.0 + j
        })
        .collect();
    let wsum: f64 = weights.iter().sum();
    for w in weights.iter_mut() {
        *w /= wsum;
    }

    // 最大余数法分配：每桶先取整，剩下的按小数部分从大到小补 1。
    //
    // 原来用 `round()` 再「把误差全补到最后一个桶」——1843 步 / 72 桶时
    // 每项四舍五入成 26，总和 1872，误差 -29 全压到末桶，末桶变成 **-3**。
    // 报文里出现负步数是硬伤，服务端一眼就能看到。
    let exact: Vec<f64> = weights.iter().map(|w| w * total_steps as f64).collect();
    let mut counts: Vec<i32> = exact.iter().map(|v| v.floor() as i32).collect();
    let mut remain: i32 = total_steps - counts.iter().sum::<i32>();
    if remain > 0 {
        // 小数部分大的先补
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| {
            let fa = exact[a] - exact[a].floor();
            let fb = exact[b] - exact[b].floor();
            fb.partial_cmp(&fa).unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut k = 0usize;
        while remain > 0 && k < order.len() * 4 {
            counts[order[k % order.len()]] += 1;
            remain -= 1;
            k += 1;
        }
    } else if remain < 0 {
        // 浮点误差导致 floor 之和偏大：从最小项开始减
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&i| counts[i]);
        let mut k = 0usize;
        while remain < 0 && k < order.len() * 4 {
            let idx = order[k % order.len()];
            if counts[idx] > 0 {
                counts[idx] -= 1;
                remain += 1;
            }
            k += 1;
        }
    }

    counts
        .iter()
        .enumerate()
        .map(|(i, &steps)| {
            let begin = start_ms + i as i64 * 10_000;
            // diff 三件套：真机每个桶恒为 avgDiff=0 / maxDiff=0 / minDiff=1000。
            //
            // 这三个字段是「桶内三段步数的相邻差值」。早先为了让它们看起来
            // 有起伏，用确定性伪随机把 10 秒切成三段再算差值——那是在没有
            // 真机样本时的推测。真机 581 个桶无一例外都是这三个常量：
            // maxDiff 0、avgDiff 0、minDiff 1000（1000 是服务端的「无差值」
            // 哨兵值，正常差值不会到这个量级）。
            //
            // 造出来的「波动」和真机相反，那不是去指纹，是加指纹。
            let max_diff = 0;
            let min_diff = 1000;
            let avg_diff = 0;
            StepBucket {
                steps_num: steps,
                begin_time: begin,
                end_time: begin + 10_000,
                flag: start_ms,
                id: 0,
                max_diff,
                min_diff,
                avg_diff,
                state: 0,
                queue_num: 0,
            }
        })
        .collect()
}

/// 生成长度 `n` 的固定抖动序列（±`amp`），值域 [0.85, 1.15] 之外会被夹住。
pub fn make_jitter(n: usize, amp: f64, seed: u64) -> Vec<f64> {
    let mut state = seed | 1;
    (0..n)
        .map(|_| {
            // xorshift64*，够用且不引入额外依赖
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let r = (state.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64;
            (r * 2.0 - 1.0) * amp
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_points(start: i64, sec: i64, n: usize) -> Vec<(f64, f64, i64)> {
        let step = (sec * 1000) / (n as i64 - 1);
        (0..n)
            .map(|i| {
                let t = if i == n - 1 { start + sec * 1000 } else { start + i as i64 * step };
                (30.5 + i as f64 * 1e-5, 104.0, t)
            })
            .collect()
    }

    #[test]
    fn bucket_count_matches_duration() {
        let pts = line_points(0, 760, 200);
        let b = speed_buckets(&pts, 0, 760);
        assert_eq!(b.len(), 76, "760s 应有 76 个 10 秒桶");
        assert_eq!(b[0].begin_time, 0);
        assert_eq!(b[0].end_time, 10_000);
        assert_eq!(b[75].begin_time, 750_000);
    }

    #[test]
    fn distances_sum_to_near_total() {
        let pts = line_points(0, 760, 400);
        let total: f64 = pts
            .windows(2)
            .map(|w| distance((w[0].0, w[0].1), (w[1].0, w[1].1)))
            .sum();
        let buckets = speed_buckets(&pts, 0, 760);
        let sum: f64 = buckets.iter().map(|b| b.distance).sum();
        // 允许每个桶 0.01 的舍入误差
        assert!((sum - total).abs() < buckets.len() as f64 * 0.02, "sum={sum} total={total}");
    }

    #[test]
    fn steps_sum_exactly() {
        let j = make_jitter(76, 0.15, 42);
        let b = step_buckets(0, 760, 2300, &j);
        assert_eq!(b.len(), 76);
        let sum: i32 = b.iter().map(|x| x.steps_num).sum();
        assert_eq!(sum, 2300, "步数总和必须精确等于 totalSteps");
    }

    #[test]
    fn steps_have_variation() {
        let j = make_jitter(76, 0.15, 7);
        let b = step_buckets(0, 760, 2300, &j);
        let counts: Vec<i32> = b.iter().map(|x| x.steps_num).collect();
        let min = *counts.iter().min().unwrap();
        let max = *counts.iter().max().unwrap();
        assert!(max > min, "各桶步数不应完全相同（max={max} min={min}）");
    }

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        let a = make_jitter(100, 0.2, 12345);
        let b = make_jitter(100, 0.2, 12345);
        assert_eq!(a, b, "同种子必须同结果");
        for v in &a {
            assert!(v.abs() <= 0.2 + 1e-9, "抖动超界: {v}");
        }
        let c = make_jitter(100, 0.2, 999);
        assert_ne!(a, c, "不同种子应不同");
    }

    #[test]
    fn short_duration_still_one_bucket() {
        let pts = line_points(0, 5, 10);
        let b = speed_buckets(&pts, 0, 5);
        assert_eq!(b.len(), 1, "不足 10 秒也要有一个桶，不能返回空");
    }
}

/// 桶网格上的起步窗口长度（**整桶数**）。
///
/// 真机 555 秒 → 起步窗口 [`super::startup_sec`] = 86 秒 → `86 / 10 = 8` 个桶：
/// 前 8 个桶（0~80s）`queueNum` 恒 0、速度桶 `state=2`；第 9 个桶（80~90s）
/// 真正跨过 86 秒这条线，真机把它算进「已起步」。所以这里取**整桶截断**
/// （floor），不是四舍五入——`86/10` 用 `round` 会得到 9，前面就多一个 0。
///
/// 这个 8 不是从点位那边套过来的：点位 `su=19`（86 秒内的 19 个采样点），
/// 桶 `su=8`，两者是同一个时间窗口在两张不同网格上的落点。
pub fn bucket_startup(total_sec: i64) -> usize {
    (super::startup_sec(total_sec) / 10).max(0) as usize
}

/// 提交报文用的桶 id：`60000+i`，`queueNum` 走与点位同一个分区函数。
///
/// 服务端在提交时按这个规则校验；和 OBS 对象里的 id 规则**不同**（OBS 是
/// 服务端自增列量级的连续 id）。
///
/// `queueNum` 早先写成 `queue_num: i as i32`（桶序号原样），55 个桶于是变成
/// 0..54 —— 真机全场 `queueNum` 上限是 24，0..54 里有一大半落在不存在的台阶上，
/// 整个字段语义是错的（不是「差几档」，是另一个量纲）。现在与 OBS 桶、
/// 与点位共用 [`super::queue_num`]，`su` 用桶自己的起步窗口。
pub fn apply_submit_ids(speed: &mut [SpeedBucket], steps: &mut [StepBucket]) {
    let n = speed.len().max(steps.len());
    let su = bucket_startup((n as i64) * 10);
    for (i, b) in speed.iter_mut().enumerate() {
        b.id = 60000 + i as i64;
        b.queue_num = super::queue_num(i, n, su);
    }
    for (i, b) in steps.iter_mut().enumerate() {
        b.id = 60000 + i as i64;
        b.queue_num = super::queue_num(i, n, su);
    }
}

/// OBS 对象用的桶 id / `queueNum` / `state`。
///
/// 真机依据（`evidence/obs_real_20261002.json`，55 个速度桶 + 55 个步频桶）：
///
/// - `id` **连续递增**（1628、1629、…、1682），不是按 `endSec` 跳号。
///   旧实现用 `(rrid%100000)*1000+endSec`，同一个数组里相邻桶的 id 差 10；
/// - 两个数组共用同一条 id 序列（速度桶和步频桶第 i 项 id 相同）；
/// - `queueNum` 起步段（前 8 个桶）是 0，之后**每 2 个桶 +1** 到 24
///   （`8→1、10→2、…、54→24`，55 个桶逐元素与真机一致）；
/// - `speed_json` 的 `state`：起步段的 8 个桶是 **2**，其余 47 个是 **0**。
///   起步段（前 86 秒）配速还没起来，App 把这几个桶标成「非正常」；
/// - `step_freq_json` 的 `state` 恒 0。
///
/// 依赖提交返回的 `rrid`，所以 OBS 必须在提交**之后**上传。
pub fn apply_obs_ids(speed: &mut [SpeedBucket], steps: &mut [StepBucket], rrid: i64, start_ms: i64) {
    // 真机 id 是服务端自增列（1628 起）。客户端只需落在同量级、且同一条
    // 记录内连续递增，所以按 rrid + 起始时间派生一个稳定偏移。
    let base = 1600
        + (((rrid as u64).wrapping_mul(0x9E3779B97F4A7C15) ^ start_ms as u64) % 400) as i64;
    let n = speed.len().max(steps.len());
    let total_sec = speed
        .last()
        .map(|b| (b.end_time - start_ms) / 1000)
        .unwrap_or(10)
        .max(10);
    let su = bucket_startup(total_sec);
    for (i, b) in speed.iter_mut().enumerate() {
        b.id = base + i as i64;
        b.queue_num = super::queue_num(i, n, su);
        b.state = if i < su { 2 } else { 0 };
    }
    for (i, b) in steps.iter_mut().enumerate() {
        b.id = base + i as i64;
        b.queue_num = super::queue_num(i, n, su);
        b.state = 0;
    }
}

#[cfg(test)]
mod id_tests {
    use super::*;

    #[test]
    fn submit_and_obs_ids_differ() {
        let pts: Vec<(f64, f64, i64)> = (0..=60)
            .map(|i| (30.5 + i as f64 * 1e-5, 104.0, 1_700_000_000_000 + i * 1000))
            .collect();
        let start = 1_700_000_000_000i64;
        let mut sp = speed_buckets(&pts, start, 60);
        let mut st = step_buckets(start, 60, 200, &[1.0; 6]);

        apply_submit_ids(&mut sp, &mut st);
        assert_eq!(sp[0].id, 60000);
        assert_eq!(sp[1].id, 60001);
        // 60 秒 → 起步窗口 9 秒 → 桶网格上 `9/10 = 0` 个整桶，所以没有 0 档；
        // 6 个桶 / 24 档 → 步长取整为 1，queueNum 是 1..6（不再是桶序号 0..5，
        // 也不再是「首桶 0」——那个 0 只在有整桶落在起步窗口里时出现）。
        assert_eq!(bucket_startup(60), 0);
        assert_eq!(
            sp.iter().map(|b| b.queue_num).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            st.iter().map(|b| b.queue_num).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6]
        );

        apply_obs_ids(&mut sp, &mut st, 123_456_789, start);
        // 真机 OBS 的桶 id 是**连续递增**的（1628、1629、…），不是按 endSec 跳号
        assert_eq!(sp[1].id, sp[0].id + 1, "相邻桶 id 必须差 1");
        assert!(sp[0].id >= 1600 && sp[0].id < 2000, "id 量级要对齐真机 1628 起");
        // 速度桶和步频桶共用同一条 id 序列（真机两个数组第 i 项 id 相同）
        for (a, b) in sp.iter().zip(st.iter()) {
            assert_eq!(a.id, b.id, "speed/step 桶 id 必须一一对应");
        }
    }

    /// 真机 `speed_json[i].queueNum`（55 个桶，rrid 1328288734）：
    /// 前 8 个桶 0，之后**每 2 个桶 +1** 到 24。整段写在这里逐元素比 ——
    /// 旧断言只断 `sp[0]==0` / `sp.last()==24`，中间 22/55 的偏移全部漏过
    /// （`docs/audit/AUDIT-round.md` A5）。
    const REAL_BUCKET_QUEUE: [i32; 55] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11,
        11, 12, 12, 13, 13, 14, 14, 15, 15, 16, 16, 17, 17, 18, 18, 19, 19, 20, 20, 21, 21, 22,
        22, 23, 23, 24,
    ];

    /// OBS 的 `state` / `queueNum`：真机速度桶前 8 个是 2、其余 0，
    /// 步频桶恒 0；`queueNum` 起步段 0 之后每 2 个桶 +1 到 24，逐元素对拍。
    #[test]
    fn obs_states_and_queue_follow_real_device() {
        let pts: Vec<(f64, f64, i64)> = (0..=111)
            .map(|i| (30.5 + i as f64 * 1e-5, 104.0, 1_700_000_000_000 + i * 5000))
            .collect();
        let start = 1_700_000_000_000i64;
        let mut sp = speed_buckets(&pts, start, 555);
        let mut st = step_buckets(start, 555, 670, &[1.0; 56]);
        apply_obs_ids(&mut sp, &mut st, 1_328_288_734, start);

        assert_eq!(sp.len(), 55, "555 秒应有 55 个 10 秒桶");
        let two = sp.iter().filter(|b| b.state == 2).count();
        assert_eq!(two, 8, "起步段（前 80 秒）应有 8 个 state=2 的桶，实际 {two}");
        assert!(sp[8..].iter().all(|b| b.state == 0), "起步段之后 state 应为 0");
        assert!(st.iter().all(|b| b.state == 0), "步频桶 state 恒 0");
        assert_eq!(bucket_startup(555), 8, "555 秒的起步窗口落在 8 个整桶上");

        // 逐元素：速度桶与步频桶都必须与真机序列完全相等
        assert_eq!(REAL_BUCKET_QUEUE.len(), sp.len());
        for (i, &real) in REAL_BUCKET_QUEUE.iter().enumerate() {
            assert_eq!(sp[i].queue_num, real, "速度桶第 {i} 个 queueNum：真机 {real} vs {}", sp[i].queue_num);
            assert_eq!(st[i].queue_num, real, "步频桶第 {i} 个 queueNum：真机 {real} vs {}", st[i].queue_num);
        }
    }

    /// 提交报文里的桶（`speedPerTenSec` / `stepsPerTenSec`）走同一条分区函数：
    /// 55 个桶必须也是 0..24 的那套台阶，而不是桶序号 0..54。
    #[test]
    fn submit_bucket_queue_follows_real_device() {
        let pts: Vec<(f64, f64, i64)> = (0..=111)
            .map(|i| (30.5 + i as f64 * 1e-5, 104.0, 1_700_000_000_000 + i * 5000))
            .collect();
        let start = 1_700_000_000_000i64;
        let mut sp = speed_buckets(&pts, start, 555);
        let mut st = step_buckets(start, 555, 670, &[1.0; 56]);
        apply_submit_ids(&mut sp, &mut st);

        assert_eq!(sp.len(), REAL_BUCKET_QUEUE.len());
        for (i, &real) in REAL_BUCKET_QUEUE.iter().enumerate() {
            assert_eq!(sp[i].id, 60000 + i as i64);
            assert_eq!(sp[i].queue_num, real, "提交速度桶第 {i} 个 queueNum：真机 {real} vs {}", sp[i].queue_num);
            assert_eq!(st[i].queue_num, real, "提交步频桶第 {i} 个 queueNum：真机 {real} vs {}", st[i].queue_num);
        }
        // 旧实现是桶序号原样，上限会到 54 —— 只要有人改回去，这行立刻红
        let max = sp.iter().map(|b| b.queue_num).max().unwrap();
        assert_eq!(max, 24, "全场 queueNum 上限必须是 24，不是桶序号 {max}");
    }

    #[test]
    fn flag_is_start_ms() {
        let pts: Vec<(f64, f64, i64)> = vec![(30.5, 104.0, 1_700_000_000_000), (30.5001, 104.0, 1_700_000_010_000)];
        let sp = speed_buckets(&pts, 1_700_000_000_000, 10);
        assert_eq!(sp[0].flag, 1_700_000_000_000);
    }
}

#[cfg(test)]
mod diff_not_degenerate_tests {
    use super::*;

    /// diff 三件套必须与真机一致：恒为 avgDiff=0 / maxDiff=0 / minDiff=1000。
    ///
    /// 实测 6 条真机记录 581 个桶，这三个字段无一例外都是这三个常量。
    /// 早先按「桶内三段差值」把它们做成变化的，方向是反的——真机本身就是
    /// 常量，造出来的起伏反而成了和真机不一致的指纹。
    #[test]
    fn diff_fields_match_real_device() {
        let j = vec![1.0; 72];
        let b = step_buckets(0, 720, 1843, &j);
        for x in &b {
            assert!(x.steps_num > 0, "不该有空桶");
            assert_eq!(x.max_diff, 0, "真机 maxDiff 恒为 0");
            assert_eq!(x.min_diff, 1000, "真机 minDiff 恒为 1000（无差值哨兵）");
            assert_eq!(x.avg_diff, 0, "真机 avgDiff 恒为 0");
        }
    }
}

#[cfg(test)]
mod speed_bucket_coverage_tests {
    use super::*;

    /// 各桶位移之和必须等于轨迹总长（重叠比例分配不该丢距离）。
    #[test]
    fn bucket_distances_sum_to_total() {
        // 273 点 / 720 秒，非均匀采样
        let pts: Vec<(f64, f64, i64)> = (0..273)
            .map(|i| {
                let t = 1_790_000_000_000 + i as i64 * 2640;
                (31.2304 + i as f64 * 7.0 / 111_320.0, 121.4737, t)
            })
            .collect();
        let total: f64 = pts
            .windows(2)
            .map(|w| distance((w[0].0, w[0].1), (w[1].0, w[1].1)))
            .sum();
        let b = speed_buckets(&pts, 1_790_000_000_000, 720);
        let sum: f64 = b.iter().map(|x| x.distance).sum();
        assert!(
            (sum - total).abs() < total * 0.02,
            "桶和 {sum:.1}m 与总长 {total:.1}m 差太多"
        );
    }

    /// 正常 720 秒轨迹不该出现大量空桶。
    ///
    /// 旧实现单指针跨桶不重置，落在桶前的那段距离被丢掉，实测
    /// 72 个桶里 17 个位移为 0。
    #[test]
    fn no_empty_buckets_on_normal_track() {
        let pts: Vec<(f64, f64, i64)> = (0..273)
            .map(|i| {
                let t = 1_790_000_000_000 + i as i64 * 2640;
                (31.2304 + i as f64 * 7.0 / 111_320.0, 121.4737, t)
            })
            .collect();
        let b = speed_buckets(&pts, 1_790_000_000_000, 720);
        let empty = b.iter().filter(|x| x.distance < 0.5).count();
        assert!(empty == 0, "{empty} 个桶位移为 0（共 {} 个）", b.len());
    }
}
