fn main() {
    use pace_forge::track::fence_route::*;
    let ring: Vec<(f64,f64)> = serde_json::from_str(&std::fs::read_to_string("/tmp/ring.json").unwrap()).unwrap();
    // 模拟 5 个分散的打卡点
    let cps = vec![
        (31.2370, 121.4700), (31.2350, 121.4760), (31.2330, 121.4720),
        (31.2310, 121.4740), (31.2398, 121.4790),
    ];
    for c in &cps { println!("打卡点 {:?} 在围栏内: {}", c, contains(&ring, c.0, c.1)); }
    let r = route_in_fence_with(&ring, 2000.0, &cps).unwrap();
    let dense = resample_with_checkpoints(&r.points, 5.0, &cps, 15.0);
    println!("路径 {:.0}m, 采样 {} 点", r.length_m, dense.len());
    for (i,c) in cps.iter().enumerate() {
        let near = dense.iter().map(|p| {
            let dy=(p.0-c.0)*111320.0; let dx=(p.1-c.1)*111320.0*0.675;
            (dy*dy+dx*dx).sqrt()
        }).fold(f64::MAX, f64::min);
        println!("  P{} 最近距离 {:.1}m", i+1, near);
    }
}
