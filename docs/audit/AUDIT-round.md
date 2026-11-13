# AUDIT-round：未提交改动的错误审查

审计对象：`/sdcard/workspace/pace-forge` 工作区未提交改动（基线 `f813749`，7 个文件 +2399/−278）。
唯一可信证据：`/sdcard/workspace/re/zjwh740/evidence/obs_real_20261002.json`（真机 rrid 1328288734：114 点 / 555s / 1683.83m / 25 段 / 55 速度桶 / 55 步频桶 / 2 圈）。
本轮**没有修改任何代码**；所有验证都在 `/tmp/pf`（仓库的完整副本，`/sdcard` 挂载不允许执行，`cargo` 才能跑起来）里做。

验证手段（可复现）：

```bash
cp -a /sdcard/workspace/pace-forge /tmp/pf && cd /tmp/pf
cargo test --offline                       # 276 passed / 0 failed / 3 ignored —— 全绿
cargo test --offline --test probe  -- --nocapture   # 点位/桶/极值参数
cargo test --offline --test probe2 -- --nocapture   # preflight 端到端
cargo test --offline --test probe3 -- --nocapture   # 10 个种子
cargo test --offline --test probe5 -- --nocapture   # 导出 our_points.json 逐字段对比
node -e "…TP-CORE…"                         # 播放器时间轴（真机点喂进去）
```

结论统计：**A（确定改错了）6 条 / B（可疑、有隐患）9 条 / C（无证据的假设）6 条**。
下文每条都带「位置 / 现象 / 依据 / 后果 / 正确做法 / 复现」。

---

## A. 确定改错了

### A1. `compress_startup` 把「采样点正落在打卡点上」这条硬约束打掉了 —— 官方点位的跑必被自检拦死

**位置**
- `track/mod.rs:100` `compress_startup`（沿弧长把**所有**点重新插值铺一遍）
- `api/flow.rs:1037`（`retime` 之后无条件压缩）、`api/flow.rs:1040-1046`（压缩后重算 `real_len`）
- `track/planner.rs:241-245`（`nudge_to` 后 `flat[idx] = (cp.lat, cp.lng)`：**采样点被精确放在打卡点上**）
- `track/preflight.rs:228-243`（`CP_MISS` 阈值 **0.5 m**，`Fatal`）
- `api/flow.rs:1129-1133`（`report.blocked()` → `Err("自检未通过…")`）、`api/flow.rs:1241`（`run_full` 第一句就是 `dry_run(...)?`）

**现象**
压缩前每个打卡点与最近采样点距离 **0.000 m**；压缩后变成 0.377 / **0.584** / 0.328 / 0.364 / **2.244** m。
`CP_MISS` 要求 ≤0.5 m → 五个点位里两个直接 Fatal：

```
✗ 致命 [CP_MISS] 打卡点 CP1 最近距离 0.6m，未命中（要求 ≤0.5m）
✗ 致命 [CP_MISS] 打卡点 CP4 最近距离 2.2m，未命中（要求 ≤0.5m）
```

10 个种子 × 4 组参数（1000m/360s、2000m/700s、3000m/1100s、1683m/555s）× 5 点位 **全部被拦**（10/10）；只有 2 个点位时 `1/2` 超标，同样被拦。开/关压缩的对照：

```
compress_startup = false → blocked=false
compress_startup = true  → blocked=true   （同上两条 CP_MISS）
```

**依据（为什么必然发生）**：压缩对 `i >= k` 的点也是「按 `want[i]` 弧长重新插值」，原来那一批点里唯一精确落在打卡点上的那个顶点被移到了别的弧长位置。新采样网格间距 ≈ 5.9 m，最近点落在 ±2.95 m 内随机 → 单点命中 0.5 m 的概率约 17%，5 个点位全中约 0.01%。这不是概率问题，是设计冲突。

**后果**：`sportType=1`（有必到点）的跑**一次都发不出去**——本地自检先死。若把自检关掉，上传的 `type=2` 标记与真实经过点也已经错位（`checkpoint_track_indexes` 用的 10 m 容差还过得去，但服务端「按顺序经过所有点位」是它自己从轨迹里找，点位被压缩移开 2 m 属于同一类风险）。

**正确做法**：压缩**之后**再吸附一次（用 `checkpoint_track_indexes` 找到每个打卡点在压缩后轨迹上的最近下标，写回打卡点坐标），然后 `real_len` 重算（`flow.rs` 本来就在压缩后重算，顺序天然合适）；或只压窗口内 `i<k` 的点、窗口外保持原采样位置（但这会在窗口边界留一个 70 m 级的 `GEO_JUMP_SOFT`，要一起处理）；或把 `CP_MISS` 阈值改成服务端真实容差（**当前无证据**，见 C2）。

**复现**：`/tmp/pf/tests/probe2.rs::probe_preflight_checkpoint_block`（两条 `CP_MISS`）、`probe3.rs::probe_seed_sweep`（10/10）、`probe.rs::probe_checkpoint_exact_hit_after_compress`（0.000 m → 0.584/2.244 m）。

---

### A2. 点 `speed` 的单位认反了；`avgSpeed` 的语义也错（真机是「点间速度」，代码发「累计均值」）

**位置**
- `api/submit.rs:322-331`（注释：`speed` 单位 km/h，`bdS` 单位 m/s，「两个字段天然满足 `bdS == speed/3.6`」）
- `api/submit.rs:338` `let inst = v_mps * 3.6;`、`api/submit.rs:509` `"speed": inst`、`api/submit.rs:477` `"bdS": v_mps`
- `api/submit.rs:340` + `472` `avg_speed = cum.0/dt`（累计均值）
- `web/index.html:1775`（播放器把 `speed` 当 m/s：`1000/s.speed` 当配速、`s.speed*3.6` 当 km/h）

**现象（拿真机 114 点现算）**

- `speed` 与「配速（分/公里）」的**中位绝对误差 0.003**，且 `speed × v_geom ≈ 16.67`（=1000/60）恒定：`speed` 就是**配速 min/km**。
  | idx | 真机 speed | 点间速度 v (m/s) | 1000/(60v) | 我们发的 v×3.6 |
  |---|---|---|---|---|
  | 20 | 2.94 | 5.664 | 2.94 | 20.4 |
  | 40 | 4.98 | 3.345 | 4.98 | 12.0 |
  | 60 | 5.08 | 3.278 | 5.08 | 13.1 |
  | 111 | 26.29 | 0.632 | 26.37 | 2.3 |
  | 112 | 146.2 | 0.114 | 146.24 | 0.4 |
- `avgSpeed` 真机是**点间速度**：idx 30 真机 5.51 = 24.06m/5s；idx 42 真机 **0.38**（此刻 `totalDis/totalTime` = 521.8/210 ≈ 2.5，累计均值绝不可能是 0.38）。我们发的是 `cum.0/dt`，全程几乎恒等于 3.0（probe5：`avgSpeed` 范围 0.00~3.00，真机 0.06~5.66）。

**后果**：`speed` 与真机差 2.4~3.6 倍**而且方向相反**（越慢值越大）；`avgSpeed` 从「逐点起伏的曲线」被压成一条水平线 —— 服务端拿 `speed/3.6` 与 `bdS` 一对全是错的，配速曲线和「跑得是否自然」的判断建在错量纲上。这是本次 diff 声称要修的那类差异（`submit.rs:1717-1785` 的断言把错模型写成了法律，见附-2/3）。

**正确做法**：`speed = 1000.0/(60.0*v)`（`v>0`，起步窗口 0）；`avgSpeed = v_point`（本点到上一点的位移/间隔，clamp 6.3）；`bdS = v_point` 保持不变。播放器侧同步：`speed` 当配速用，`km/h` 由 `500/pace` 或 `bdS*3.6` 算。

**复现**：`python3` 用真机 `raw_points` 逐点算 `totalDis/totalTime` 差值与 `1000/(60v)` 对比（脚本已跑，误差表见上）；`probe5.rs` 打印我方 `speed/avgSpeed/bdS` 区间；`node` 里 `tpSampleAt(tl,300)` 显示 `speed*3.6 = 16.5 km/h` 而真机同时刻 `bdS*3.6 = 14.8 km/h`。

---

### A3. `flag` 改成常量、新播放器却只认「非常量 flag」：回放功能对本仓库产出的任何记录都不可用

**位置**
- `api/submit.rs:491` `"flag": start_ms`（diff 把原来的 `p.time_ms` 改成整场常量，注释在 `submit.rs:388-407`，真机确实如此）
- `web/index.html:1775` `t: num(p.flag) || tpParseTime(p.gainTime)`（`flag` 只要非 0 就永远轮不到 `gainTime`）
- `web/index.html:1861-1900` `tpBuildTimeline`：`uniq(t).size >= 2` 才算有时间轴
- `web/index.html:2097-2150` `tpInit`：`canPlay=false` 时按钮全 `disabled`
- `web/index.html:1857-1860` 注释「flag 绝对毫秒戳，单调递增 → 时间轴」——与 Rust 侧改动**直接矛盾**

**现象**：把真机 114 点按 `obsTrack` 的映射喂进 TP-CORE：

```
真机点（flag 全同）→ hasTime= false ok= false total= 113   unique t = 1
若改用 gainTime  → hasTime= true  total= 561 s（真机 555s）
```

即：任何用本代码上传的记录（`flag` 全点同值）在详情页都会显示「这条轨迹没有可用时间戳（flag / gainTime 都缺或全相同），只能看静态轨迹，无法回放」，播放/倍速/进度条全部禁用。而真机 `gainTime` 是**逐点有值**的（`2026-10-02 12:04:42 … 12:14:03`），本来可以兜住。

**后果**：本次新增的「轨迹回放」在真实数据形状下是死功能；`cargo test` 与任何前端测试都发现不了（TP-CORE 单测用的是合成点，`flag` 递增）。

**正确做法**：`tpBuildTimeline` 里先统计 `flag` 的唯一值个数，`<2` 就把每个点的时间改为 `tpParseTime(gainTime)`（`obsTrack` 里要保留 `gain`），再判定 `hasTime`；或干脆主用 `gainTime`（真机逐点必有），`flag` 只用于对齐起始毫秒。

**复现**：`node` 脚本（本报告开头的 TP-CORE 提取 + 真机点喂入），输出 `hasTime=false`；把 `t` 换成 `tpParseTime(p.gainTime)` 立刻 `hasTime=true, total=561`。

---

### A4. 点位有**两份实例**：自检用的那份做了平滑、且 seed 不同；上传的那份不平滑、seed 也不同 —— 「自检守 A、上传发 B」在点位上仍然存在（桶修了，点没修）

**位置**
- `api/submit.rs:1183` `let seed = params.start_time_ms as u64 ^ 0x9E3779B97F4A7C15;`（自检件）
- `api/submit.rs:1219-1230` `smooth_point_speed(&mut loc_items)` 只作用在自检件上
- `api/flow.rs:1581` `apply_obs_ids` 之后到 `1626-1650`：OBS 那份调 `point_json_full(..., sim.total_steps, start_ms as u64, ...)`，**seed 不同、且完全没有 `smooth_point_speed`**
- `api/flow.rs:1466`（`detail_payload_rrid(&out.track, …)` 才是真正上传的）

**现象**：同一份轨迹，
- OBS 里出去的 `speed/bdS` 是「相邻两点位移/间隔」的**锯齿**（`smooth_point_speed` 根本没用上）；
- 自检里看到的 `speed/bdS` 是前后 5 点窗口均值；
- `id / count / radius / type / bdA` 因为 seed 不同，两套值完全不同（probe5 的 `our_points.json` vs `build(...).points`：`id` 3560 vs 另一套基数、`count`/`radius` 分布区间相同但逐点不同）。

**后果**：`preflight` 与 `detect` 检查的点位不是要发出去的那份；`submit.rs:1222-1224` 自己写的「配速平滑必须在这里做」的意图（去锯齿）对线上数据**零效果** —— 服务端看到的仍是满屏锯齿，而注释和测试都以为修好了。

**正确做法**：`detail_payload_rrid` 也调一次 `smooth_point_speed`，并把 seed 由调用方传入同一个值；更彻底的是让 `build()` 产出的 `loc_items` 直接传给 OBS 构造，只替换 `id/queueNum/flag` 这类「OBS 专有」字段，彻底一份数据。

**复现**：`probe5.rs` 写出 `/tmp/our_points.json`；与 `build()` 的 `sub.points` 对比 `id/count/radius/type/bdA`；`grep -n smooth_point_speed api/flow.rs` → 0 命中。

---

### A5. `queue_num` 用「线性插值 + round」，真机是「每 4 个点一档 / 每 2 个桶一档」的 floor 分区；注释里连步长都写错了

**位置**
- `track/mod.rs:173-189`（注释「第 19 个点开始**每 5 个点** +1」；实现 `(1.0 + step*23.0).round()`）
- 调用点：`api/submit.rs:507`（点位）、`track/buckets.rs:328`/`333`（速度桶 / 步频桶）

**现象（真机逐序对比，n=114/startup=19 与 n=55/su=8）**

| | 真机分段（首下标, 值） | 当前实现 | 不一致数 |
|---|---|---|---|
| 点位 | (0,0)(19,1)(23,2)(27,3)(31,4)…(103,22)(108,23)(112,24) | (0,0)(19,1)(**22**,2)(**26**,3)(**30**,4)…(107,23)(111,24) | **14/114**（22,26,30,34,…,107,111，恒为真机+1） |
| 桶 | (0,0)(8,1)(10,2)(12,3)…(52,23)(54,24) | (0,0)(8,1)(**9**,2)(**11**,3)…(53,24) | **22/55**（9,11,13,…,53） |

真机是**每 4 个点一档、每 2 个桶一档**（floor），末段吸收余数（22 档占 5 个点、24 档占 2 个点）。按 `1 + floor((pos-startup)/ceil((n-1-startup)/24))` 复算：点位 **2/114** 不一致（仅末两档），桶 **0/55** 完全一致。

**后果**：`queueNum` 是「这一趟的第几个采样档」，错一档就是错位；`track/mod.rs:174` 的「每 5 个点」也是错的（真机 4）。现有断言只断首尾（`buckets.rs:391-395`：`sp[0]==0`、`sp.last()==24`），中间 22/55 的错全部漏过。

**正确做法**：改成 floor 分区 + 末段吸收余数；注释改成「每 4 个点 / 每 2 个桶」。

**复现**：`probe.rs::probe_queue_num` 打印两套序列；上表由 python 用真机 `raw_points[i].queueNum`、`speed_json[i].queueNum` 与公式逐项比对得到。

---

### A6. `stepDistance` 真机是**全点同值** 3.7468809127807674，代码发逐点变化的「本段位移/本段步数」（0~1.26）

**位置**：`api/submit.rs:457-465`（`d_seg/seg_steps`）、`523` `"stepDistance": step_distance`；断言 `api/submit.rs:2151+`（`step_distance_is_local_stride` 要求 1.0~1.5 m）

**现象**：真机 114 点 `stepDistance` 的**唯一值集合只有 `{0.0, 3.7468809127807674}`**（起步窗口 0，其余全部同一个常量）。我们发的是每点不同的值（probe5：idx 60 = 1.2576，范围 0~3.9 之类逐点跳）。

**后果**：形态（常量 vs 逐点）与量级（3.75 vs 1.0~1.5）都不符，是本 diff 想消除的那类指纹；而断言把「1.0~1.5 m 步幅」写成法律，等于锁死错模型。

**正确做法**：这个字段的语义**没有证据**（见 C4），只能照抄真机形态 —— 起步窗口 0、其余全点同值；要留可配置项（有些设备可能按用户身高算）。

**复现**：`python3 -c "print(set(p['stepDistance'] for p in raw_points))"` → `{0.0, 3.74688…}`；`probe5.rs` 打印我方序列。

---

## B. 可疑 / 有隐患

### B1. 账号切换、上传后刷新都不过 `tpDestroy()`：上一个账号的轨迹和动画留在页面上

`web/index.html:522-530` `enter(me)` 只做 `initMap(); loadRecords();`，`$('reload').onclick = loadRecords`（`:551`）也只重绘 `#list`。切账号走 `boot()` → `enter()`，`#detail` 里上一条记录的播放器 **rAF 继续跑、图层还挂在地图上**，列表却已经是新账号的记录 —— 跨账号串数据。
另一条路径：`selectRec` 的「0/1 个点」分支（`:2331`）直接 `$('detail').innerHTML = …` 覆盖，**不调 `tpDestroy()`**。慢点击 A 记录 → 立刻点 B 记录（B 先返回并挂好播放器）→ A 的 OBS 拉回来是空 → 覆盖 DOM 把 B 的播放器变成「孤儿」：`tp` 全局还指着它、rAF 还在跑、地图上的线停不住（`setText` 有 `if (el)` 保护所以不崩，纯属视觉与 CPU 泄漏）。
**修**：`enter()` 开头加 `tpDestroy(); if (map && layer) layer.clearLayers(); $('detail').innerHTML='';`；`:2331` 分支在写 innerHTML 前先 `tpDestroy()`。

### B2. 播放器的「步数」是死指标（`steps` 字段已删）

`web/index.html:1776` 仍读 `p.steps`，但真机从无此字段、本 diff 也明确删了（`submit.rs:1902`）→ 恒为 `-1` → `:1971 firstStep=-1` → `:1979 sf=0` → `:1981-1982` 全部点 `steps=0` → `tpSteps` 永远显示 0。要么删掉这个指标，要么用 `step_freq_json` 按时间累加。

### B3. 测试隔离只做到 `api/session.rs`，另外 5 处仍直连真实 `$HOME`

`api/accounts.rs:157-165`（`accounts.json`）、`api/client.rs:175-196`（`.pace-forge-device-id*`，自己 `create_dir_all` + `fs::write`）、`track/roadnet.rs:188/357/426/472`（`roads-*`/`blockers`/`built-*`/`water-*`）、`track/tileroad.rs:144-145`（`tiles/`）、`api/policy.rs:222-228`（`PACE_DUMP_POLICY`）。这些都没有注入点。
实测：`cargo test --offline --lib`（276 项）跑完 `~/.pace-forge/accounts.json`、`session.json` 的 mtime **未变**，目录也没新增文件 —— 即**当前**测试恰好都不触发，但没有任何机制阻止以后触发（`docs/notes/TEST-ISOLATION.md` 只承诺 session.rs，这条文档本身没过度声称）。建议一并迁到同一套 `*_below(std::path)` 注入。

### B4. `data_dir()` 不再 `create_dir_all` 的连带面

`session.rs` 里 `write_json_below` 会建目录，`accounts::save`、`roadnet` 缓存、`tiles` 都自己建；**唯独 `client.rs` 之外的裸写**（`policy.rs:228` 的 `fs::write`，只在 `PACE_DUMP_POLICY` 打开时）和「全新机器上第一次就只要 device-id」的场景没有兜底。风险面小，但 `data_dir()` 语义从「取目录并确保存在」变成「纯路径计算」这件事没有在文档里说清。

### B5. `bdA` 默认硬编码 `150 ± 8`（齐齐哈尔）—— 别的城市跑就和坐标自相矛盾

`api/submit.rs:440-441`。真机那次在齐齐哈尔（平原，142~158 m），把单点样本写成全局默认；在成都（≈500 m）、昆明（≈1890 m）跑，`bdA` 和 `laps_json.endAltAbs` 会与坐标差几百米到两千米。`alt_range` 可覆盖，但**默认值只有一个城市的**。建议默认从定位锚点查/按经纬度粗估，或强制要求传 `alt_range`。

### B6. 生产采样 5 m/点 ⇒ 337 点，真机是 5 s/点 ⇒ 114 点；所有「按点数比例」的规则在 3 倍点数下形状全变

`planner.rs:220` `n = round(dist/sample_step_m)` + `plan.rs` 默认 `sample_step_m=5.0`；真机 1683.83/113 ≈ 14.9 m/点、4.87 s/点。于是 `startup_point_count` 的 15.5% 变成 52 个点（真机 19 个）、`tail = 0.73·n` 变成第 246 点、`queueNum` 变成「每 13 个点一档」。曲线/分布层面的指纹和真机不是一个形状（`type` 分布 89/16/5/1/1/1 在 n=113 时对得上，n=337 时 `-1` 会变成 52 个）。要么把采样改成「按时间 5 s」（真机口径），要么所有比例规则改成按时间戳切。

### B7. `parseCoordArray` 不认 `gLat/gLng`：详情接口一旦回带点位，地图会跳到 (-1,-1)

`web/index.html:2446-2448` 只读 `p.lat/p.lng`，而 `valid()`（`:2410-2414`）认为 `-1/-1` 合法（`|lat|<=90`）。本次改动把点位的 `lat/lng` 统一成 `-1.0`，一旦某个接口把 28 字段点位吐给 `findTrack`，`pts.length > 1` 会**短路掉 OBS 兜底**，地图 `fitBounds` 到几内亚湾。修：对象分支优先读 `gLat/gLng`，并把 `(-1,-1)` 排除。

### B8. 速度/步频桶只覆盖 `floor(total_sec/10)*10` 秒，末段 5 秒没有桶

`track/buckets.rs:56` `n = (total_sec/10).max(1)`：555 s → 55 桶覆盖 0~550 s，最后 5 s 没有桶；真机末桶 `endTime` 到 554.388 s（几乎贴住结束）。这与本次新加的 `segment_json`（首段 + 20 s 窗口把尾巴单独成段）口径不一致：段覆盖到 555 s，桶只到 550 s。预存在问题，但本次声称「同源」，值得一起收。

### B9. `STARTUP_RATIO=0.155` / `STARTUP_DIST_RATIO=0.011` 是单一 555 s 样本的比例外推，边界参数下会产出物理上不可能的形状

| dist / dur | 起步窗口 | 窗口内总位移 | 窗口外需要的速度 |
|---|---|---|---|
| 400 m / 120 s | 19 s（15.8%） | 0.80 m | 3.95 m/s（14.2 km/h） |
| 1683.8 m / 555 s | 86 s（15.5%） | 13.5 m | 3.56 m/s（12.8 km/h） |
| 5000 m / 1800 s | 279 s（15.5%） | 50 m | 3.25 m/s（11.7 km/h） |
| 42195 m / 7200 s | **1116 s（18.6 分钟）** | **459 m** | **6.86 m/s（24.7 km/h）** |
| 300 m / 1800 s | 279 s（15.5%） | 0.60 m | **0.20 m/s（0.7 km/h）** |

真机的「起步窗口」是 GPS 锁星造成的**固定约 86 秒**物理现象，不是时长的 15.5%。2 小时跑出 18.6 分钟的原地漂移、或 30 分钟只挪 0.6 m 的「跑步」，本身就是新指纹；42 km 情形还压到点速上限（`avgSpeed` clamp 6.3 m/s，而几何要求 6.86）。建议窗口取 `min(86s, 时长·0.155)` 并对超长跑单独复核，或按时长约 60~120 s 的固定窗口做。

---

## C. 无证据的假设（赌的是什么）

- **C1** 服务端「经过点位」到底比 `lat/lng` 还是 `gLat/gLng`。代码与注释在两套之间来回改过（`submit.rs:1878-1890` 自己写了「中间被反着改过一次」），最终按 `gLat/gLng` 判点是**推测**；手里没有一次真实提交的服务端响应可以对照。
- **C2** `count` / `radius` / `bdD`(方位) / `state` / `queueNum` 这些字段服务端是否真的校验、按什么规则校验 —— 无证据。A5 只能证明「与真机不一致」，不能证明「会被拒」。
- **C3** 服务端是否需要 `extension_json`（本 diff 删掉第 11 个键）。判断依据是「真机对象顶层 10 个键，多一个键就是差异」——合理的对拍逻辑，但服务端是否容忍多余键**没有证据**。
- **C4** `stepDistance` 常量 3.7468809127807674 的语义。它与 670 步、1683.83 m、449.4 都不构成简单关系（不是步幅、不是累计均值），只能照抄形态。
- **C5** `fivePointJson.runAreaId = -1`（`submit.rs:1246`、`payload_run_area_id`）。我们手上只有 `fixed_point_json.runAreaId = -1` 这一份证据；「顶层 `save_record.runAreaId` 必须是真实 id」**没有真机报文原件**支撑（C 级）。围栏匹配逻辑本身没读这个信封值（已核对：`fence.rs`、`flow.rs` 的 `geoFencesJson` 都走独立参数），这一条是安全的。
- **C6** `STARTUP_RATIO` 取 0.155 而不是 0.15 的理由（「86/555=15.50%，0.15 会切出 83 秒」）把一次取整当规律；且 `startup_sec` 对 555 s 给 86 s、对 700 s 给 109 s，真机在别的时长下是不是同比例**无证据**（见 B9）。

---

## 附：被改写成「迁就实现」的断言（审计项 8）

`cargo test` 276 项全绿，而上面 A1~A6 全部存在 —— 这就是「断言迁就实现」的直接证据。逐条判断：

| # | 断言 | 判 | 理由 |
|---|---|---|---|
| 1 | `points_match_real_device_fields`（`submit.rs:2933+`）把 count/radius/bdA/bdD/queueNum/state/type 全部改成**区间夹逼** | **可疑** | 区间能过，语义错也能过。A2/A5/A6 全在区间内照样溜过去；一个字段都没跟真机 114 点逐点对拍 |
| 2 | `assert!(sp > 1.0 && sp < 30.0, "speed 应在 km/h 量级")`（`submit.rs:1914`） | **改得错** | 配速 3.3~16.7 同样落在区间内 → 这就是 A2 没被发现的原因 |
| 3 | `bdS == speed/3.6`（`submit.rs:1778-1785`、`2993-3000`） | **改得错** | 把「speed 是 km/h」写成不变量；真机 `speed·v ≈ 16.67` 说明它是配速 |
| 4 | `step_distance_is_local_stride` 断言 1.0~1.5 m（`submit.rs:2151+`） | **改得错** | 真机全点同值 3.7469，这条断言锁死错模型（A6） |
| 5 | `obs_states_and_queue_follow_real_device` 只断 `sp[0].queueNum==0`、`sp.last()==24`（`buckets.rs:391-395`） | **可疑** | 中间 22/55 不一致全部漏过（A5）。既然手上有真机 55 个桶，就该逐项对拍 |
| 6 | `assert!(sp[0].id >= 1600 && sp[0].id < 2000)`（`buckets.rs:366`） | **可疑** | 真机首发 id 是 1628，这条只锁量级，锁不住值 |
| 7 | `obs_payload_has_ten_gzipped_keys`（`flow.rs:1758+`） | **改得对** | 键集合与真机 10 键一致，`extension_json` 缺席断言明确 |
| 8 | `all_loc_json_has_server_fields` 里 `lat/lng == -1.0`（`submit.rs:1888-1893`） | **改得对** | 与真机 114 点一致；注释还标了「中间被反着改过一次」，这次方向正确 |
| 9 | `obs_checkpoint_marking_uses_coords_not_position`（`flow.rs:1951+`） | **改得对（但有缺口）** | 坐标口径正确；但把「点位是否**仍然落在**打卡点上」这件事漏了 —— 正是 A1 |

**缺的断言（建议补上，比改现有断言更有价值）**：
1. 端到端一条：`plan(带官方点位) → retime → compress_startup → 逐点距离 ≤0.5m`（A1）；
2. 拿 `evidence/obs_real_20261002.json` 做**逐点回放对拍**（`speed`、`avgSpeed`、`queueNum`、`stepDistance`、`type/state` 序列），不要只做区间；
3. `OBS 点位 == 自检点位`（逐字段 hash 相等），防 A4 这类「两份实例」再犯；
4. TP-CORE 用「flag 全同 + gainTime 逐点」的真机形状做一次 `hasTime` 断言（A3）。

---

## 一句话结论

改动方向（`lat/lng=-1`、10 键 OBS、段/圈/点位字段补齐、桶/点位同一份 sim 数据、`queueNum` 起步段=0、测试隔离）**大部分是对的**，但有 3 个致命处：**压缩起步窗把打卡点命中打掉（A1，官方点位跑直接被自检拦死）**、**`speed`/`avgSpeed` 的单位与语义认错（A2）**、**`flag` 常量与新播放器互斥（A3，回放功能不可用）**；另有 2 处「声称修好但没修到位」（A4 点位两份实例、A5/A6 与真机逐点不符），以及 9 项隐患。**改代码之前先修 A1**，否则任何 `sportType=1` 的跑都发不出去。
