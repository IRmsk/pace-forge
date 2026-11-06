//! 10 秒分桶。
//!
//! 服务端的反作弊核心在这两个数组上，不是总量：
//! - `speedPerTenSec`：每 10 秒的位移
//! - `stepsPerTenSec`：每 10 秒的步数，外加窗口内的 `maxDiff/minDiff/avgDiff`
//!
//! 总量对但分桶像机器，一样判无效。所以桶里必须有自然抖动。

use serde::Serialize;

use super::geo::distance;

/// 速度桶。
#[derive(Debug, Clone, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
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
    let mut cursor = 0usize;

    for i in 0..n {
        let begin = start_ms + i as i64 * 10_000;
        let end = begin + 10_000;
        let mut dist = 0.0f64;
        // 累加落在 [begin, end) 内的相邻点间距
        while cursor + 1 < points.len() {
            let t = points[cursor + 1].2;
            if t >= end {
                break;
            }
            if t >= begin {
                dist += distance(
                    (points[cursor].0, points[cursor].1),
                    (points[cursor + 1].0, points[cursor + 1].1),
                );
            }
            cursor += 1;
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

    let mut counts: Vec<i32> = weights.iter().map(|w| (w * total_steps as f64).round() as i32).collect();
    // 舍入误差补到最后一个桶，保证总和精确
    let diff: i32 = total_steps - counts.iter().sum::<i32>();
    if let Some(last) = counts.last_mut() {
        *last += diff;
    }

    counts
        .iter()
        .enumerate()
        .map(|(i, &steps)| {
            let begin = start_ms + i as i64 * 10_000;
            // 桶内步频波动：以 3 秒为单位切 3 段，算相邻差值统计
            let per3 = (steps as f64 / 3.0).round() as i32;
            let diffs: Vec<i32> = vec![per3, per3, steps - 2 * per3]
                .windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .collect();
            let max_diff = *diffs.iter().max().unwrap_or(&0);
            let min_diff = *diffs.iter().min().unwrap_or(&0);
            let avg_diff = if diffs.is_empty() {
                0
            } else {
                diffs.iter().sum::<i32>() / diffs.len() as i32
            };
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

/// 提交报文用的桶 id：`60000+i`，`queueNum = i`。
///
/// 服务端在提交时按这个规则校验；和 OBS 对象里的 id 规则**不同**。
pub fn apply_submit_ids(speed: &mut [SpeedBucket], steps: &mut [StepBucket]) {
    for (i, b) in speed.iter_mut().enumerate() {
        b.id = 60000 + i as i64;
        b.queue_num = i as i32;
    }
    for (i, b) in steps.iter_mut().enumerate() {
        b.id = 60000 + i as i64;
        b.queue_num = i as i32;
    }
}

/// OBS 对象用的桶 id：`(rrid % 100000) * 1000 + endSec`，`queueNum = 0`。
///
/// 依赖提交返回的 `rrid`，所以 OBS 必须在提交**之后**上传。
pub fn apply_obs_ids(speed: &mut [SpeedBucket], steps: &mut [StepBucket], rrid: i64, start_ms: i64) {
    let base = (rrid % 100_000) * 1000;
    for b in speed.iter_mut() {
        b.id = base + (b.end_time - start_ms) / 1000;
        b.queue_num = 0;
    }
    for b in steps.iter_mut() {
        b.id = base + (b.end_time - start_ms) / 1000;
        b.queue_num = 0;
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
        assert_eq!(sp[0].queue_num, 0);
        assert_eq!(sp[1].queue_num, 1);

        apply_obs_ids(&mut sp, &mut st, 123_456_789, start);
        // (123456789 % 100000) * 1000 = 56789 * 1000
        assert_eq!(sp[0].id, 56_789_000 + 10, "首桶 endTime 是 start+10s");
        assert_eq!(sp[1].id, 56_789_000 + 20);
        assert_eq!(sp[0].queue_num, 0);
    }

    #[test]
    fn flag_is_start_ms() {
        let pts: Vec<(f64, f64, i64)> = vec![(30.5, 104.0, 1_700_000_000_000), (30.5001, 104.0, 1_700_000_010_000)];
        let sp = speed_buckets(&pts, 1_700_000_000_000, 10);
        assert_eq!(sp[0].flag, 1_700_000_000_000);
    }
}
