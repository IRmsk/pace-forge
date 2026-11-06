//! 入口：`pace-forge <命令>`
//!
//! 命令：
//! - `web [addr]`      启动 Web 控制台
//! - `dry <距离> <分钟>` 只做流程预演，不发写请求
//! - `stadium <圈数>`   生成操场轨迹并打印参数
//! - `version` / `help`

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("web");

    match cmd {
        "web" => {
            let addr = args.get(1).cloned().unwrap_or_else(|| "127.0.0.1:9998".into());
            if let Err(e) = pace_forge::web::serve(&addr) {
                eprintln!("启动失败: {e}");
                std::process::exit(1);
            }
        }
        "semester" => show_semester(),
        "check" => {
            let dist_km: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2.0);
            let dur_min: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(12.0);
            local_check(dist_km, dur_min);
        }
        "run" => {
            let dist_km: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2.0);
            let dur_min: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(12.0);
            let yes = args.iter().any(|a| a == "-y" || a == "--yes");
            do_run(dist_km, dur_min, yes);
        }
        "dry" => {
            let dist_km: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2.0);
            let dur_min: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(12.0);
            dry_run(dist_km, dur_min);
        }
        "stadium" => {
            let laps: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5.0);
            let lane: u8 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
            stadium_probe(laps, lane);
        }
        "version" | "-V" | "--version" => {
            println!("pace-forge {}", pace_forge::VERSION);
        }
        "help" | "-h" | "--help" => {
            println!("pace-forge {} — 运动世界校园协议工具\n", pace_forge::VERSION);
            println!("用法:");
            println!("  pace-forge web [addr]          启动 Web 控制台（默认 127.0.0.1:9998）");
            println!("  pace-forge semester            查学期进度（记分跑）");
            println!("  pace-forge run [km] [min] [-y] 刷跑并提交（-y 跳过确认）");
            println!("  pace-forge check [km] [min]    离线体检轨迹（不联网、不登录）");
            println!("  pace-forge dry [km] [min]      只预演，不提交（要登录）");
            println!("  pace-forge stadium [圈] [道次] 操场轨迹参数");
            println!("  pace-forge version             版本号");
            println!();
            println!("run/dry 需要已保存的登录会话（$HOME/.pace-forge/session.json）。");
        }
        other => {
            eprintln!("未知命令: {other}\n用 `pace-forge help` 查看用法");
            std::process::exit(2);
        }
    }
}

fn dry_run(dist_km: f64, dur_min: f64) {
    use pace_forge::api::{flow, session};

    let sess = session::load_session();
    if !sess.is_valid() {
        eprintln!("未登录：先通过 Web 控制台登录一次（会话存在 $HOME/.pace-forge/session.json）");
        std::process::exit(1);
    }
    let client = pace_forge::api::client::Client::new(pace_forge::api::client::DeviceProfile::default())
        .with_credentials(pace_forge::api::client::Credentials {
            uid: Some(sess.uid.to_string()),
            token: Some(sess.token.clone()),
            unid: Some(sess.unid.clone()),
        });

    let p = flow::RunParams {
        distance_m: dist_km * 1000.0,
        duration_sec: (dur_min * 60.0).round() as i64,
        ..Default::default()
    };

    println!(
        "预演：{:.1} km / {:.0} 分钟（不发任何写请求）\n",
        dist_km, dur_min
    );
    let mut log = |s: &str| println!("{s}");
    match flow::dry_run(&client, &p, &mut log) {
        Ok(o) => {
            println!("\n=== 完成 ===");
            println!("  规划点数    {}", o.track_points);
            println!("  实际里程    {:.1} m", o.actual_length_m);
            println!("  总步数      {}", o.total_steps);
            println!("  平均步频    {:.1} 步/分", o.avg_cadence);
            println!("  自检        {}", o.report.summary());
            println!("  已提交      {}", if o.submitted { "是" } else { "否（预演）" });
        }
        Err(e) => {
            eprintln!("\n预演失败: {e}");
            std::process::exit(1);
        }
    }
}

/// 完全离线的轨迹体检：不登录、不联网，只看本地合成的数据像不像真跑。
fn local_check(dist_km: f64, dur_min: f64) {
    use pace_forge::track::{
        buckets::{make_jitter, speed_buckets, step_buckets},
        detect::{inspect, PointRule},
        plan, simulate, KinParams, PlanRequest, SimParams, StadiumSpec,
    };

    let total_m = dist_km * 1000.0;
    let total_sec = (dur_min * 60.0).round() as i64;
    let start_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    println!("离线体检：{dist_km:.1} km / {dur_min:.0} 分钟");
    println!("（不登录、不发请求，只验证本地合成数据）\n");

    // 用操场轨迹，几何确定，便于看点位间距
    let spec = StadiumSpec::new(30.5, 104.0).with_lane(1);
    let lap = pace_forge::track::lap_length(1);
    let laps = (total_m / lap).max(0.5);
    let xy = pace_forge::track::stadium_track(&spec, laps, 5.0);
    println!("  轨迹点      {}", xy.len());
    println!("  绕圈        {laps:.2} 圈（单圈 {lap:.1} m）");

    // 时间戳
    let (times, _sp, real_len) = pace_forge::track::retime(
        &xy, total_sec, start_ms, start_ms as u64,
        &KinParams::default(), 1.0, 8.0,
    );
    let pts3: Vec<(f64, f64, i64)> = xy.iter().zip(&times)
        .map(|((a, b), t)| (*a, *b, *t)).collect();

    // 分桶
    let sb = speed_buckets(&pts3, start_ms, total_sec);
    let jit = make_jitter(sb.len().max(1), 1.0, start_ms as u64);
    let stb = step_buckets(start_ms, total_sec, 180, &jit);

    // 合成
    let sp = SimParams {
        total_distance_m: real_len as f64,
        total_sec,
        start_ms,
        seed: start_ms as u64,
        ..Default::default()
    };
    let sim = match simulate(&pts3, &sp) {
        Ok(s) => s,
        Err(e) => { eprintln!("合成失败: {e}"); return; }
    };

    println!("  实际里程    {:.1} m", real_len);
    println!("  总步数      {}", sim.total_steps);
    println!("  平均步频    {:.1} 步/分", sim.avg_cadence);

    // 反检测体检
    let rep = inspect(&xy, &sb, PointRule::default());
    println!("\n=== 点位体检（模拟 7.4.01 本地规则）===");
    println!("  {}", rep.summary());
    for line in rep.lines().iter().take(8) {
        println!("  · {line}");
    }
    println!(
        "\n  判定: {}",
        if rep.blocked() {
            "✗ 会被判异常"
        } else {
            "✓ 通过"
        }
    );

    // 分桶统计
    let caps: Vec<f64> = sb.iter().map(|b| b.distance).collect();
    if !caps.is_empty() {
        let mx = caps.iter().cloned().fold(f64::MIN, f64::max);
        let mn = caps.iter().cloned().fold(f64::MAX, f64::min);
        let avg = caps.iter().sum::<f64>() / caps.len() as f64;
        println!("  分桶        {} 个，{:.1}~{:.1} m（均值 {:.1}）", caps.len(), mn, mx, avg);
    }
}

fn show_semester() {
    use pace_forge::api::{semester, session};

    let sess = session::load_session();
    if !sess.is_valid() {
        eprintln!("未登录：先执行 `pace-forge web` 在网页里登录一次");
        std::process::exit(1);
    }
    let client = pace_forge::api::client::Client::new(pace_forge::api::client::DeviceProfile::default())
        .with_credentials(pace_forge::api::client::Credentials {
            uid: Some(sess.uid.to_string()),
            token: Some(sess.token.clone()),
            unid: Some(sess.unid.clone()),
        });

    match semester::fetch_summary(&client) {
        Ok(sm) => {
            println!("{}", sm.summary());
            println!("  有效比例 {:.1}%", sm.valid_ratio() * 100.0);
        }
        Err(e) => eprintln!("查学期汇总失败: {e}"),
    }

    match semester::fetch_goal(&client, 1) {
        Ok(g) => {
            if g.target_dis > 0.0 {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                println!(
                    "学期目标 {:.1}km / {} 次，还剩 {} 天",
                    g.target_dis / 1000.0,
                    g.target_count,
                    g.days_left(now)
                );
                if !g.in_term(now) {
                    println!("⚠ 当前不在学期内");
                }
            } else {
                println!("· 未取到学期目标");
            }
        }
        Err(e) => eprintln!("查学期目标失败: {e}"),
    }
}

fn do_run(dist_km: f64, dur_min: f64, yes: bool) {
    use pace_forge::api::{flow, session};

    let sess = session::load_session();
    if !sess.is_valid() {
        eprintln!("未登录：先执行 `pace-forge web` 在网页里登录一次");
        std::process::exit(1);
    }
    let client = pace_forge::api::client::Client::new(pace_forge::api::client::DeviceProfile::default())
        .with_credentials(pace_forge::api::client::Credentials {
            uid: Some(sess.uid.to_string()),
            token: Some(sess.token.clone()),
            unid: Some(sess.unid.clone()),
        });

    let p = flow::RunParams {
        distance_m: dist_km * 1000.0,
        duration_sec: (dur_min * 60.0).round() as i64,
        ..Default::default()
    };

    println!("刷跑：{:.1} km / {:.0} 分钟\n", dist_km, dur_min);
    let mut log = |s: &str| println!("{s}");
    let mut confirm = |o: &flow::RunOutcome| -> bool {
        println!();
        println!("=== 待提交 ===");
        println!("  轨迹点   {}", o.track.len());
        println!("  里程     {:.1} m", o.actual_length_m);
        println!("  步数     {}", o.total_steps);
        println!("  步频     {:.1} 步/分", o.avg_cadence);
        println!("  自检     {}", o.report.summary());
        if yes {
            println!("  （-y 已跳过确认）");
            return true;
        }
        print!("\n确认提交？[y/N] ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        matches!(line.trim(), "y" | "Y" | "yes")
    };

    match flow::run_full(&client, &p, &mut log, &mut confirm) {
        Ok(o) => {
            if o.submitted {
                println!("\n✅ 提交成功，记录 id={:?}", o.record_id);
                println!("   {} 秒内不要再跑", flow::SILENCE_SEC);
            } else {
                println!("\n已取消");
            }
        }
        Err(e) => {
            eprintln!("\n失败: {e}");
            std::process::exit(1);
        }
    }
}

fn stadium_probe(laps: f64, lane: u8) {
    use pace_forge::track::{arc_and_curvature, lap_length, path_length, stadium_track, StadiumSpec};

    let spec = StadiumSpec::new(30.5, 104.0).with_lane(lane);
    let pts = stadium_track(&spec, laps, 5.0);
    let len = path_length(&pts);
    let (_, k) = arc_and_curvature(&pts);
    let max_k = k.iter().cloned().fold(0.0f64, f64::max);

    println!("操场第 {lane} 道");
    println!("  单圈      {:.2} m", lap_length(lane));
    println!("  {laps:.1} 圈      {:.1} m", len);
    println!("  采样点    {}", pts.len());
    println!("  最大曲率  {max_k:.5} /m（理论 {:.5}）", 1.0 / (36.5 + 0.3 + 1.22 * (lane as f64 - 1.0)));
    println!();
    println!("建议：{:.0}m 跑 {:.0} 秒（配速 {:.2} 分/km）", len, (len / 2.78).round(), 1000.0 / 2.78 / 60.0);
}
