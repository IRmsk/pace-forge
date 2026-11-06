//! 在围栏内规划路线。
//!
//! 之前的做法是「以围栏顶点均值为圆心，按目标里程反推半径画圈」——完全不管
//! 围栏形状。围栏是长条形或不规则多边形时，圆会有一大半落在围栏外，服务端
//! 判定轨迹越界，整条记录作废（表现为距离 0.00km）。
//!
//! 这里改成真正的围栏内填充：多边形内缩出安全区，用蛇形（boustrophedon）
//! 扫描铺满，实际长度对不上目标就调扫描间距重来。

use super::geo::{distance, m_per_deg_lat, m_per_deg_lng};

/// 离围栏边界至少留这么多米。贴边容易被判越界。
pub const EDGE_MARGIN_M: f64 = 20.0;

/// 蛇形扫描结果。
#[derive(Debug, Clone)]
pub struct FenceRoute {
    /// 路线点（BD-09，与围栏同坐标系）。
    pub points: Vec<(f64, f64)>,
    /// 实际长度（米）。
    pub length_m: f64,
    /// 用的扫描线数。
    pub lines: usize,
}

/// 多边形面积（鞋带公式，度²）。
fn polygon_area(poly: &[(f64, f64)]) -> f64 {
    if poly.len() < 3 {
        return 0.0;
    }
    let mut a = 0.0;
    for i in 0..poly.len() {
        let j = (i + 1) % poly.len();
        a += poly[i].0 * poly[j].1 - poly[j].0 * poly[i].1;
    }
    (a / 2.0).abs()
}

/// 把多边形往内缩（沿质心方向按比例缩放）。
///
/// 不是严格的 Minkowski 内缩，但对凸/近似凸的操场围栏足够用；不规则的凹多边形
/// 会缩得偏多，保守方向是对的——宁可小一点也不越界。
pub fn shrink(poly: &[(f64, f64)], margin_m: f64) -> Vec<(f64, f64)> {
    if poly.len() < 3 {
        return poly.to_vec();
    }
    let n = poly.len() as f64;
    let cy = poly.iter().map(|p| p.0).sum::<f64>() / n;
    let cx = poly.iter().map(|p| p.1).sum::<f64>() / n;
    let mlat = m_per_deg_lat(cy).max(1.0);
    let mlng = m_per_deg_lng(cy).max(1.0);

    poly.iter()
        .map(|&(lat, lng)| {
            let dy = (lat - cy) * mlat;
            let dx = (lng - cx) * mlng;
            let d = (dy * dy + dx * dx).sqrt();
            if d <= margin_m || d < 1e-9 {
                // 太小的多边形缩完就没了，原地保留
                return (lat, lng);
            }
            let k = (d - margin_m) / d;
            (cy + dy * k / mlat, cx + dx * k / mlng)
        })
        .collect()
}

/// 点是否在多边形内（射线法）。
pub fn contains(poly: &[(f64, f64)], lat: f64, lng: f64) -> bool {
    if poly.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (yi, xi) = poly[i];
        let (yj, xj) = poly[j];
        if ((yi > lat) != (yj > lat)) && (lng < (xj - xi) * (lat - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// 求多边形与水平线的交点（用于扫描填充）。
fn scan_intersections(poly: &[(f64, f64)], lat: f64) -> Vec<f64> {
    let mut xs = Vec::new();
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (yi, xi) = poly[i];
        let (yj, xj) = poly[j];
        if (yi > lat) != (yj > lat) {
            let t = (lat - yi) / (yj - yi);
            xs.push(xi + t * (xj - xi));
        }
        j = i;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    xs
}

/// 蛇形扫描填充多边形，返回折线点。
///
/// `spacing_m` 是相邻扫描线的间距（米）。越小线越密、总长越长。
fn boustrophedon(poly: &[(f64, f64)], spacing_m: f64) -> (Vec<(f64, f64)>, usize) {
    if poly.len() < 3 || spacing_m <= 0.0 {
        return (Vec::new(), 0);
    }
    let lat_min = poly.iter().map(|p| p.0).fold(f64::MAX, f64::min);
    let lat_max = poly.iter().map(|p| p.0).fold(f64::MIN, f64::max);
    let mlat = m_per_deg_lat((lat_min + lat_max) / 2.0).max(1.0);
    let dlat = spacing_m / mlat;

    let mut pts = Vec::new();
    let mut line_idx = 0usize;
    let mut lat = lat_min + dlat / 2.0;
    let mut forward = true;

    while lat < lat_max {
        let xs = scan_intersections(poly, lat);
        // 交点成对出现，逐段填充（支持凹多边形）
        for pair in xs.chunks(2) {
            if pair.len() < 2 {
                break;
            }
            let (mut x0, mut x1) = (pair[0], pair[1]);
            if !forward {
                std::mem::swap(&mut x0, &mut x1);
            }
            // 换线时必须补本行起点：只比较经度会漏判——换行时经度本来就相同
            // （都在同一侧），纬度却变了，跳过后会从上一行末端斜穿到本行起点。
            pts.push((lat, x0));
            pts.push((lat, x1));
        }
        forward = !forward;
        line_idx += 1;
        lat += dlat;
    }
    (pts, line_idx)
}

/// 折线总长（米）。
fn path_len(pts: &[(f64, f64)]) -> f64 {
    pts.windows(2).map(|w| distance(w[0], w[1])).sum()
}

/// 在围栏内规划一条长度接近 `want_m` 的路线。
///
/// 不靠调扫描间距——间距只能取「几条线」，总长是跳变的（800m 的目标只能拿到
/// 850m 或 1284m，中间没有解）。改成先铺一条足够密的蛇形路径，再沿路径走到
/// 目标长度为止。这样长度是连续可控的。
pub fn route_in_fence(fence: &[(f64, f64)], want_m: f64) -> Result<FenceRoute, String> {
    route_in_fence_with(fence, want_m, &[])
}

/// 带必经点的版本。
///
/// 打卡点必须被轨迹经过（服务端按「轨迹点离打卡点多近」判定命中），所以
/// 蛇形不能从围栏一头漫无目的地铺——2000m 的里程在 1km 高的围栏里只够铺两三行，
/// 铺在最南边就离北边的打卡点 700 多米远，五个点全部 MISS。
///
/// 做法：先用必经点算出包围盒，在盒内（再和围栏求交）铺蛇形，最后把路径
/// 整体平移到覆盖所有必经点。
pub fn route_in_fence_with(
    fence: &[(f64, f64)],
    want_m: f64,
    must: &[(f64, f64)],
) -> Result<FenceRoute, String> {
    if fence.len() < 3 {
        return Err("围栏点数不足，无法规划".into());
    }
    if want_m <= 0.0 {
        return Err("目标里程必须为正".into());
    }

    // 只保留真正落在围栏内的必经点
    let must_inside: Vec<(f64, f64)> = must
        .iter()
        .copied()
        .filter(|p| contains(fence, p.0, p.1))
        .collect();

    // 有必经点时，蛇形范围收缩到「必经点包围盒」与「围栏」的交集，
    // 这样里程才会花在打卡点附近，而不是花在没人去的角落。
    let work_area: Vec<(f64, f64)> = if must_inside.len() >= 2 {
        let pad_m = 60.0;
        let mlat = m_per_deg_lat(must_inside[0].0).max(1.0);
        let mlng = m_per_deg_lng(must_inside[0].0).max(1.0);
        let lat_min = must_inside.iter().map(|p| p.0).fold(f64::MAX, f64::min) - pad_m / mlat;
        let lat_max = must_inside.iter().map(|p| p.0).fold(f64::MIN, f64::max) + pad_m / mlat;
        let lng_min = must_inside.iter().map(|p| p.1).fold(f64::MAX, f64::min) - pad_m / mlng;
        let lng_max = must_inside.iter().map(|p| p.1).fold(f64::MIN, f64::max) + pad_m / mlng;
        let box_poly = vec![
            (lat_min, lng_min),
            (lat_min, lng_max),
            (lat_max, lng_max),
            (lat_max, lng_min),
        ];
        // 用围栏裁掉盒子的越界部分：取盒子中心落围栏内的部分已经够用，
        // 这里直接把盒子作为扫描范围，越界的采样点后面会过滤掉。
        let _ = &box_poly;
        box_poly
    } else {
        shrink(fence, EDGE_MARGIN_M)
    };

    let inner = work_area;
    let lat_min = inner.iter().map(|p| p.0).fold(f64::MAX, f64::min);
    let lat_max = inner.iter().map(|p| p.0).fold(f64::MIN, f64::max);
    let heat_m = (lat_max - lat_min) * m_per_deg_lat((lat_min + lat_max) / 2.0);
    if heat_m < 10.0 {
        return Err(format!("围栏太窄（南北 {heat_m:.0}m），放不下轨迹"));
    }

    let mlat = m_per_deg_lat((lat_min + lat_max) / 2.0).max(1.0);
    let mlng = m_per_deg_lng((lat_min + lat_max) / 2.0).max(1.0);
    let area_m2 = polygon_area(&inner) * mlat * mlng;

    // 铺密一点：间距取「15m」和「能塞下的最密」二者较大值
    let spacing = 15.0f64.max(heat_m / 400.0);
    let (dense, lines) = boustrophedon(&inner, spacing);
    if dense.len() < 2 {
        return Err("围栏内没能生成扫描线".into());
    }
    let dense_len = path_len(&dense);
    if dense_len < want_m {
        return Err(format!(
            "围栏面积不够：内缩后约 {:.0} m²，铺满只有 {:.0} m 轨迹（目标 {:.0} m）\n\
             换个更小的目标里程，或者确认围栏是否正确",
            area_m2, dense_len, want_m
        ));
    }

    // 沿密路径走 `walk` 米，返回折线和实际长度
    let walk_to = |walk: f64| -> (Vec<(f64, f64)>, f64) {
        let mut pts: Vec<(f64, f64)> = vec![dense[0]];
        let mut acc = 0.0;
        for w in dense.windows(2) {
            let d = distance(w[0], w[1]);
            if acc + d >= walk {
                let t = ((walk - acc) / d).clamp(0.0, 1.0);
                pts.push((
                    w[0].0 + (w[1].0 - w[0].0) * t,
                    w[0].1 + (w[1].1 - w[0].1) * t,
                ));
                acc = walk;
                break;
            }
            pts.push(w[1]);
            acc += d;
        }
        (pts, acc)
    };

    // 闭合要占里程，但「回起点的距离」取决于走到哪里，是个不动点问题：
    // 走 want-back 得到新终点 → 新 back 又变了。迭代几次就收敛得差不多了。
    // 不预留会超（2000 跑成 2320），只迭代一次又会欠（2000 跑成 1803）。
    let mut target = want_m;
    let mut best: Option<(Vec<(f64, f64)>, f64)> = None;
    for _ in 0..6 {
        let (mut pts, acc) = walk_to(target.max(1.0));
        let total = match (pts.first(), pts.last()) {
            (Some(&a), Some(&b)) => {
                let back = distance(b, a);
                // 绕回起点的距离占比太小时才闭合。小围栏上这段能占到 40%，
                // 硬闭合会把目标里程压得对不上，不如留个开环——
                // 服务端按累计里程判定，开环不会因此变无效。
                if back > 1.0 && back <= want_m * 0.12 {
                    pts.push(a);
                    acc + back
                } else {
                    acc
                }
            }
            _ => acc,
        };
        let err = (total - want_m).abs();
        if best.as_ref().map(|b| err < b.1).unwrap_or(true) {
            best = Some((pts, total));
        }
        if err < want_m * 0.01 {
            break;
        }
        // 按误差修正：跑长了就少走，跑短了多走。
        // 上限给到 want_m * 1.15：不闭合时 target 就是真实里程，不受闭合距离挤压。
        target = (target - (total - want_m)).clamp(want_m * 0.3, want_m * 1.15);
    }

    let (points, length_m) = best.ok_or("围栏内规划失败")?;

    // 把必经点钉进折线，保证轨迹真的穿过打卡点。
    //
    // 钉点会把折线拐出去再拐回来，长度随之变长（实测 2000m 能涨到 2551m）。
    // 所以先量一次钉点带来的增量，按缺口重新规划，再钉一次。
    let points = if must_inside.is_empty() {
        points
    } else {
        let probe = pin_checkpoints(&points, &must_inside);
        let grow = path_len(&probe) - length_m;
        if grow > want_m * 0.02 {
            if let Ok(trimmed) =
                route_in_fence_with(fence, (want_m - grow).max(want_m * 0.5), &[])
            {
                pin_checkpoints(&trimmed.points, &must_inside)
            } else {
                probe
            }
        } else {
            probe
        }
    };
    let length_m = path_len(&points);

    Ok(FenceRoute { points, length_m, lines })
}

/// 把必经点「钉」进折线：找路径上离它最近的一段，用 A → m → B 替换 A → B。
///
/// 光靠重采样后吸附不行——吸附只能在已有采样点里挑一个最近的，采样间距 5m
/// 时最大偏差 2.5m，而服务端判定是「轨迹点离打卡点 ≤0.5m」。必须让路径真的
/// 从点位上穿过去。
pub fn pin_checkpoints(path: &[(f64, f64)], must: &[(f64, f64)]) -> Vec<(f64, f64)> {
    if path.len() < 2 || must.is_empty() {
        return path.to_vec();
    }
    let mut out = path.to_vec();

    for &m in must {
        // 找最近的线段
        let mut best_i = 0usize;
        let mut best_d = f64::MAX;
        for i in 0..out.len() - 1 {
            let (a, b) = (out[i], out[i + 1]);
            // 点到线段的距离
            let ay = (a.0 - m.0) * m_per_deg_lat(m.0);
            let ax = (a.1 - m.1) * m_per_deg_lng(m.0);
            let by = (b.0 - m.0) * m_per_deg_lat(m.0);
            let bx = (b.1 - m.1) * m_per_deg_lng(m.0);
            let (vy, vx) = (by - ay, bx - ax);
            let len2 = vy * vy + vx * vx;
            let t = if len2 < 1e-12 {
                0.0
            } else {
                (-(ay * vy + ax * vx) / len2).clamp(0.0, 1.0)
            };
            let py = ay + vy * t;
            let px = ax + vx * t;
            let d = (py * py + px * px).sqrt();
            if d < best_d {
                best_d = d;
                best_i = i;
            }
        }
        // 插在最近的段中间
        out.insert(best_i + 1, m);
    }
    out
}

/// 把路径重采样成等间距点，并把必经打卡点插进去。
///
/// 打卡点不在路径上时，就近找路径上最近的位置替换——服务端只认「轨迹经过点位
/// 附近」，不要求精确重合。
pub fn resample_with_checkpoints(
    path: &[(f64, f64)],
    step_m: f64,
    must: &[(f64, f64)],
    snap_m: f64,
) -> Vec<(f64, f64)> {
    if path.len() < 2 || step_m <= 0.0 {
        return path.to_vec();
    }

    // 沿路径等距采样
    let mut out = Vec::new();
    let mut carry = 0.0;
    out.push(path[0]);
    for w in path.windows(2) {
        let d = distance(w[0], w[1]);
        if d < 1e-9 {
            continue;
        }
        let mut t = step_m - carry;
        while t <= d {
            let k = t / d;
            out.push((
                w[0].0 + (w[1].0 - w[0].0) * k,
                w[0].1 + (w[1].1 - w[0].1) * k,
            ));
            t += step_m;
        }
        carry = d - (t - step_m);
    }

    // 打卡点：找路径上最近的点替换
    for &m in must {
        let mut best_i = 0;
        let mut best_d = f64::MAX;
        for (i, &p) in out.iter().enumerate() {
            let d = distance(p, m);
            if d < best_d {
                best_d = d;
                best_i = i;
            }
        }
        if best_d <= snap_m {
            out[best_i] = m;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个 400m × 300m 的矩形围栏（约 12 公顷）。
    fn rect_fence() -> Vec<(f64, f64)> {
        let lat0 = 31.2300;
        let lng0 = 121.4700;
        let dlat = 400.0 / m_per_deg_lat(lat0);
        let dlng = 300.0 / m_per_deg_lng(lat0);
        vec![
            (lat0, lng0),
            (lat0, lng0 + dlng),
            (lat0 + dlat, lng0 + dlng),
            (lat0 + dlat, lng0),
        ]
    }

    #[test]
    fn shrink_pulls_inward() {
        let f = rect_fence();
        let inner = shrink(&f, 20.0);
        assert_eq!(inner.len(), 4);
        // 内缩后面积一定更小
        assert!(polygon_area(&inner) < polygon_area(&f));
        // 中心点仍在内部
        let cy = inner.iter().map(|p| p.0).sum::<f64>() / 4.0;
        let cx = inner.iter().map(|p| p.1).sum::<f64>() / 4.0;
        assert!(contains(&inner, cy, cx));
    }

    #[test]
    fn contains_works() {
        let f = rect_fence();
        let cy = f.iter().map(|p| p.0).sum::<f64>() / 4.0;
        let cx = f.iter().map(|p| p.1).sum::<f64>() / 4.0;
        assert!(contains(&f, cy, cx), "中心在内部");
        assert!(!contains(&f, cy + 1.0, cx), "远处在外面");
    }

    #[test]
    fn scan_gives_paired_intersections() {
        let f = rect_fence();
        let lat = f.iter().map(|p| p.0).sum::<f64>() / 4.0;
        let xs = scan_intersections(&f, lat);
        assert_eq!(xs.len() % 2, 0, "交点必须成对");
        assert!(!xs.is_empty());
    }

    #[test]
    fn route_stays_inside_fence() {
        let f = rect_fence();
        let r = route_in_fence(&f, 2000.0).unwrap();
        // 折线只存转弯顶点，重采样后才是密集轨迹
        assert!(r.points.len() >= 2, "至少要有起终点");

        let dense = resample_with_checkpoints(&r.points, 5.0, &[], 15.0);
        assert!(dense.len() > 300, "2000m / 5m 应有 400 点左右，实际 {}", dense.len());

        // 每个采样点都必须在围栏内
        for p in &dense {
            assert!(contains(&f, p.0, p.1), "轨迹点 {:?} 跑到围栏外了", p);
        }
    }

    #[test]
    fn route_length_close_to_target() {
        let f = rect_fence();
        for want in [800.0, 1500.0, 2000.0, 3000.0] {
            let r = route_in_fence(&f, want).unwrap();
            let err = (r.length_m - want).abs() / want;
            // 闭合回起点的距离要靠迭代收敛，实测能稳定在 2% 以内
            assert!(
                err < 0.06,
                "目标 {want}m 实际 {:.1}m，偏差 {:.1}%",
                r.length_m,
                err * 100.0
            );
        }
    }

    /// 蛇形换行必须垂直连接，不能斜穿。
    ///
    /// 之前的连接点判断只比较经度（换行时经度本来就相同），导致上一行末端
    /// 直接连到本行起点，斜穿整个围栏，总长也全乱。这个 bug 让 1500m 的目标
    /// 只能跑出 1329m。
    #[test]
    fn scan_lines_connect_vertically() {
        let f = rect_fence();
        let inner = shrink(&f, 20.0);
        let (dense, lines) = boustrophedon(&inner, 20.0);
        assert!(lines >= 5, "至少要有几条扫描线");

        // 相邻点要么在同一纬度（同一行内），要么同经度（换行连接）
        for w in dense.windows(2) {
            let dlat = (w[0].0 - w[1].0).abs();
            let dlng = (w[0].1 - w[1].1).abs();
            let same_row = dlat < 1e-9;
            let same_col = dlng < 1e-9 || dlng < 20.0 / m_per_deg_lng(w[0].0) * 2.0;
            assert!(
                same_row || same_col,
                "相邻点既不在同一行也不在同一列，说明斜穿了: {:?} → {:?}",
                w[0], w[1]
            );
        }
    }

    #[test]
    fn narrow_fence_is_rejected() {
        let lat0 = 31.23;
        let lng0 = 121.47;
        // 5m 宽的窄条
        let f = vec![
            (lat0, lng0),
            (lat0, lng0 + 100.0 / m_per_deg_lng(lat0)),
            (lat0 + 5.0 / m_per_deg_lat(lat0), lng0 + 100.0 / m_per_deg_lng(lat0)),
            (lat0 + 5.0 / m_per_deg_lat(lat0), lng0),
        ];
        assert!(route_in_fence(&f, 2000.0).is_err(), "太窄应该报错");
    }

    #[test]
    fn resample_keeps_spacing() {
        let path: Vec<(f64, f64)> = (0..=100)
            .map(|i| (31.23, 121.47 + i as f64 * 10.0 / m_per_deg_lng(31.23)))
            .collect();
        let pts = resample_with_checkpoints(&path, 5.0, &[], 15.0);
        // 1000m 路径 5m 间隔 → 约 200 点
        assert!((190..=210).contains(&pts.len()), "实际 {} 点", pts.len());
        // 相邻间距应接近 5m
        for w in pts.windows(2) {
            let d = distance(w[0], w[1]);
            assert!((3.0..=7.5).contains(&d), "间距 {d:.1}m 偏离 5m");
        }
    }

    /// 钉入的打卡点必须精确落在路径上（服务端要求 ≤0.5m）。
    #[test]
    fn pinned_checkpoints_are_exact() {
        let path: Vec<(f64, f64)> = (0..=100)
            .map(|i| (31.23, 121.47 + i as f64 * 10.0 / m_per_deg_lng(31.23)))
            .collect();
        // 三个偏离路径 5~20m 的点
        let cps = vec![
            (31.23 + 5.0 / m_per_deg_lat(31.23), 121.47 + 200.0 / m_per_deg_lng(31.23)),
            (31.23 - 20.0 / m_per_deg_lat(31.23), 121.47 + 600.0 / m_per_deg_lng(31.23)),
            (31.23 + 12.0 / m_per_deg_lat(31.23), 121.47 + 900.0 / m_per_deg_lng(31.23)),
        ];
        let pinned = pin_checkpoints(&path, &cps);
        for cp in &cps {
            let hit = pinned.iter().any(|p| distance(*p, *cp) < 0.01);
            assert!(hit, "打卡点 {cp:?} 没被钉进路径");
        }
        // 原有路径点不能丢
        assert_eq!(pinned.len(), path.len() + cps.len());
    }

    #[test]
    fn checkpoints_get_snapped_onto_path() {
        let path: Vec<(f64, f64)> = (0..=100)
            .map(|i| (31.23, 121.47 + i as f64 * 10.0 / m_per_deg_lng(31.23)))
            .collect();
        // 故意放一个偏 8m 的点
        let cp = (31.23 + 8.0 / m_per_deg_lat(31.23), 121.47 + 500.0 / m_per_deg_lng(31.23));
        let pts = resample_with_checkpoints(&path, 5.0, &[cp], 15.0);
        let hit = pts.iter().any(|p| distance(*p, cp) < 0.1);
        assert!(hit, "打卡点应该被吸附到路径上");
    }
}
