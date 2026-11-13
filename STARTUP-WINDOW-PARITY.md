# 起步窗口几何压平 · `segment_json` 与 `totalDis` 自洽

修的是一个具体缺陷：**`segment_json` 的 Σ段距离 ≠ `totalDis`，且首段时长/距离与真机不符**。

最新实测（rrid 1364081873，600s/1833m）：`segment_json` 26 段，Σ段距离 1559.8 m，
而 `totalDis` = 1833.5 m，差 273.7 m；首段 20 秒 / 59.7 m（2.98 m/s 的匀速）。
真机首段是 86 秒 / 18.4 m（0.21 m/s，几乎原地）。

---

## 1. 根因

两件事叠在一起：

1. **起步窗口只清了字段，没有压平几何。**
   `track::planner::plan` 沿路径**等弧长**采样、`retime` 按路径长度分时间，
   于是在「起步窗口」这 15% 的时长里，轨迹照样匀速跑掉了 15% 的里程。
   `point_json_full` 那边把 `speed` / `bdS` / `avgSpeed` / `stepDistance`
   全填 0，几何却还是匀速的：真机 86 秒走 18.4 m，我们 86 秒走 ~250 m。
   `segment_json` 首段 `distance` 直接由「窗口两端累计里程插值」算出来，
   于是首段 = 一个 20 秒 20 秒窗口的份额（59.7 m），而不是 18.4 m。

2. **段表完全没有和「末点里程」对账。**
   段里程是 `dis_at(we) - dis_at(ws)` 的望远镜求和。窗口边界和点时间戳都是
   **秒级取整**出来的，首段边界一旦落在两个采样点之间偏一点、或者末点
   `totalTime` 与段表末端差 1 秒，差额就悄悄从段表里消失——表现正是
   「Σ段距离 1559.8 m，而 totalDis 1833.5 m」，段表跟点集各说各话，服务端
   逐段累加也加不出上报的里程。

一句话：**字段层的「原地」和几何层的「匀速」是两套事实**，段表又没做闭环校验。

---

## 2. 真机依据（rrid 1328288734，555 s / 1683.83 m / 114 点 / 25 段）

样本：`/sdcard/workspace/re/zjwh740/evidence/obs_real_20261002.json`
（键 `raw_points` 是 114 点数组，`segment_json` 是 25 段）。

起步期（前 18 个点、前 86 秒）逐点实测：

```
i=0  totalDis=0.00  totalTime=0   speed=0.00
i=1  totalDis=0.00  totalTime=0   speed=0.00
i=2  totalDis=0.00  totalTime=3   speed=0.00
i=3  totalDis=0.00  totalTime=8   speed=0.00
i=4  totalDis=0.00  totalTime=13  speed=0.00
i=5  totalDis=4.00  totalTime=18  speed=0.00
...
i=17 totalDis=13.60 totalTime=78  speed=0.00
i=18 totalDis=18.40 totalTime=83  speed=97.77   ← 尖峰噪声
i=19 totalDis=40.75 totalTime=88  speed=3.73    ← 真正起步
i=20 totalDis=69.07 totalTime=93  speed=2.94
```

- 起步 86 秒只走 18.4 m ≈ 0.21 m/s，第 88 秒后才进入 3~3.7 m/s 的正常配速；
- 首段（`segment_json[0]`）：`id=733`，86 s，`distance=18.400000000000006`，
  `avgSpeed=75.14032228861127`（**配速 min/km**，不是 m/s），`avgStep=15`，
  `state=131072`(0x20000)，`speedStandard=900`，`intervalStepModel=0`；
- 段2（`id=734`）：严格 20 s，`distance=98.647`，`avgSpeed=3.3789`，`state=0`；
- 首段 `endTime == 段2 startTime`（差 0 ms），段间无缝隙；
- 25 段 `distance` 之和 = 1683.83 = `totalDis`。

两条比例就是本次要复现的基准：**时间占 86/555 = 15.5%，里程只占 18.4/1683.83 = 1.09%**。

---

## 3. 改动点（三个文件，只碰这条链）

### `track/mod.rs`

| 改动 | 说明 |
| --- | --- |
| `STARTUP_RATIO` 0.15 → **0.155** | 真机首段 86/555 = 15.50%。按 0.15 切得到 83 秒，比真机短 3 秒；这个比例唯一露在报文上的地方就是首段时长。 |
| 新增 `STARTUP_DIST_RATIO = 0.011` | 起步窗口的**里程**预算（真机 18.4/1683.83 = 1.093%）。 |
| 新增 `startup_point_count(times_ms, start_ms, total_sec)` | 起步窗口的点数改成**按时间戳数**，不再按点数比例估。点字段清零、`segment` 首段、几何压缩三处从此共用一个窗口。 |
| 新增 `compress_startup(pts, start_ms, total_sec)` | **核心**：把前 `k` 个点沿路径弧长压到起始的一小段（占全程 1.1%），其余点均分剩余弧长。坐标全部由原折线插值得到，**时间戳一个都不改**，末点仍是原终点。 |
| 删除 `startup_len(n)` | 它是「按点数估窗口」那一套口径的入口，和 `startup_point_count` 是同一件事的两种算法。留着就是两个定义互相打架（实测 555s/112 点：估 17 点、按时间数是 18 点），所以删掉。 |

`compress_startup` 的三条约束（注释里也写了）：

1. 新坐标都由原折线插值 —— 点仍在那条路上，不会凭空造一条更短的路；
2. 目标弧长单调不减且末点 = 全长 —— `Σ点间距离 ≈ 原路径长度`，`totalDis` 不缩水；
3. 时间戳不动 —— 起步窗口仍是「15.5% 时长 / 1.1% 里程」。

缓入曲线用 `t^1.5`：前几个点位移接近 0（真机前 5 点 `totalDis` 全是 0），
窗口末尾走得最多。

另外压缩预算里**预留了一个采样步长**（`cluster = 全长的 1.1% - 平均步长`）：
窗口末尾那个点（索引 `k`）已经出窗、按正常间距铺开，首段里程实际等于
「压缩预算 + 一个步长」。不留余量时实测首段拿到 1.4%（真机 1.1%），留了之后
落回 1.14%。

### `api/submit.rs`

| 改动 | 说明 |
| --- | --- |
| `point_json_full(..., startup: usize, ...)` | 起步窗口由调用方按时间戳数好传进来，函数内不再 `startup_len(n)` 自己估。 |
| `smooth_point_speed` | 平滑的跳过窗口同样按时间戳算（从报文自身的 `totalTime` 反读），和上面同一口径。 |
| `build_segments_from_points` | ① 窗口规则改成「首段 + 严格 20 秒 × n + 末段吃余数」；② 新增**归一化**：`scale = 末点 totalDis / Σ原始窗口位移`，按比例把差额摊回每一段。 |
| `build()` | 算一次 `startup` 传给 `point_json_full`。 |

①的理由：老规则是「末段钉死 10 秒 + 倒数第二段吸收 20~39 秒余数」，
于是中间会冒出一个 35 秒的段，「中间段严格 20 秒」当场自相矛盾。
改完之后 555 s → 86 + 23×20 + 9 = **25 段**，与真机 25 段同数。

②的理由：段表从此**不可能**和 `totalDis` 脱钩。不管差额来自首段边界取整、
末点时间戳差 1 秒，还是别的什么，都被按里程比例摊平——这是「三处口径一致」
的兜底，而不是猜差额从哪来。

### `api/flow.rs`

`dry_run` 在 `retime` **之后**、`simulate` 之前插一步
`crate::track::compress_startup(&mut track_points, start_ms, p.duration_sec)`，
并用压平后的几何重算 `real_len`（报文的 `totalDis` 与 `allLocJson` 末点
`totalDis` 必须同源）。

顺序是关键：`retime` 必须看**压平前**的几何——它是按路径长度分配时间的，
喂它压平后的点会把时间轴也一起压没，起步窗口就变不成 15.5% 的时长。

`detail_payload_rrid` 同步改了 `point_json_full` 的调用（传 `startup`）。

> 顺带说明：`sim` 的步数/步频模型用的仍是压平后的时间轴 + 压平后的坐标，
> 与 `allLocJson` 同一份数据，没有第二套真值。

---

## 4. 离线生成的段表（600 s / 1833.5 m，不联网、不提交）

造法：`plan`（合成环点，5 m 采样）→ `retime` → `compress_startup` →
`simulate` → `submit::build` → `flow::detail_payload_rrid` → 解 `segment_json`
的 gzip+base64。起始戳 `1730000000000`。

```
[plan]     367 点，实测 1842.8 m
[retime]   压平前 1842.8 m → 压平后 1841.4 m，real_len = 1841
[points]   367 点，末点 totalDis = 1841.3658，末点 totalTime = 600
[报文]     totalDis = 1841
[segments] 27 段，Σ = 1841.39 m（与末点 totalDis 偏 0.001%）
```

| # | 时长 | distance | avgSpeed（配速 min/km） | state |
| --- | --- | --- | --- | --- |
| 1 | 93 s | 21.01 | 73.7909 | **131072** |
| 2 | 20 s | 69.65 | 4.7861 | 0 |
| 3 | 20 s | 70.74 | 4.7120 | 0 |
| … | 20 s × 20 | … | … | 0 |
| 25 | 20 s | 73.69 | 4.5235 | 0 |
| 26 | 20 s | 76.64 | 4.3495 | 0 |
| 27 | 7 s | 23.58 | 4.9477 | 0 |
| **Σ** | **600 s** | **1841.39** | — | — |

起步窗口逐点（`startup_point_count` = 57 点 = 15.5% 点数）：

```
i=0  totalDis=0.0000  totalTime=0   speed=0.0 bdS=0.0 avgSpeed=0.0 stepDistance=0.0 type=7
i=1  totalDis=0.0364  totalTime=1   speed=0.0 bdS=0.0 avgSpeed=0.0 stepDistance=0.0 type=5
i=2  totalDis=0.1029  totalTime=3   speed=0.0 bdS=0.0 avgSpeed=0.0 stepDistance=0.0 type=-1
i=3  totalDis=0.1891  totalTime=5   speed=0.0 bdS=0.0 avgSpeed=0.0 stepDistance=0.0 type=-1
i=4  totalDis=0.2911  totalTime=6   speed=0.0 bdS=0.0 avgSpeed=0.0 stepDistance=0.0 type=-1
i=5  totalDis=0.4068  totalTime=8   speed=0.0 bdS=0.0 avgSpeed=0.0 stepDistance=0.0 type=-1
...
i=55 totalDis=14.8416 totalTime=90  speed=0.0 bdS=0.0 avgSpeed=0.0 stepDistance=0.0 type=-1
i=56 totalDis=15.2453 totalTime=92  speed=0.0 bdS=0.0 avgSpeed=0.0 stepDistance=0.0 type=-1
出窗后：t=93 → 21.0 m，t=95 → 26.6，t=96 → 32.4（≈2.9 m/s，正常配速）
```

同一条链跑真机形状（555 s / 1683.83 m）：**25 段**，首段 **86 s / 19.4 m /
配速 73.88**（真机 86 s / 18.4 m / 75.14），Σ段 = 1690.84 m = 末点 totalDis
（偏 0.000%）。

### 逐条自查（对照要求的 6 项）

| # | 要求 | 实测（600 s 对象） | 结论 |
| --- | --- | --- | --- |
| 1 | 起步窗口 ≈15% 时长、≈1.1% 里程；Σ段 = totalDis（≤0.5%） | 93/600 = **15.50%**；21.01/1841.37 = **1.14%**；Σ 1841.39 vs 末点 1841.37 = **0.001%**，报文 `totalDis` 1841（差 0.37 m = 0.02%） | ✅ |
| 2 | 首段 `state=131072`，其余 0 | 首段 131072，26 段全 0 | ✅ |
| 3 | 段间无缝、末段对齐终点 | 27 段相邻 `endTime`/`startTime` 差 **全为 0 ms**；末段 `endTime` = 1730000600000 = stopTime | ✅ |
| 4 | 首段后严格 20 s，末段 ≤20 s | 25×20 s + 末段 7 s，段时长和 = 600 | ✅ |
| 5 | 窗口内 `speed`/`bdS`/`avgSpeed`/`stepDistance` = 0，位移很小 | 57 点四项全 0；窗口末累计 **15.25 m**（真机 18.4 m 同量级），前 2 点 ≈0 | ✅ |
| 6 | `avgSpeed` 是配速 min/km，首段会很大 | 首段 73.79（真机 75.14），段2 4.79 ≈ 20s/69.65m 换算 | ✅ |

`allLocJson` 末点 `totalDis` = 1841.3658，报文 `totalDis` = 1841（i64 取整），
Σ段距离 = 1841.39 —— **三者一致**（差 ≤0.4 m，0.02%）。

---

## 5. 测试

```
$ PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR=/tmp/pace-target cargo test
test api::submit::parity_regression_tests::startup_window_and_segment_distance_sum ... ok
test result: ok. 274 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 0.64s

$ PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR=/tmp/pace-target cargo clippy --all-targets
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.75s
（零告警）
```

基线 273 passed，新增 1 条 → 274 passed。

### 新增回归测试 `startup_window_and_segment_distance_sum`

位置：`api/submit.rs` 的 `parity_regression_tests`。整条链真跑一遍
（`plan` → `retime` → `compress_startup` → `simulate` → `build` →
`build_segments_from_points`），断言：

1. Σ段距离 ≈ 末点 `totalDis`（≤0.5%），且报文 `totalDis` 与末点同源（±1 m）；
2. 首段 `state == 131072`，其余段 `state == 0`；
3. 相邻段 `endTime == startTime`（0 ms），末段 `endTime == stopTime`；
4. 首段时长 ≈ `0.155 × totalTime`（±2 s），其后每段严格 20 s，末段是 1..=20 s 的余数；
5. 首段距离 ≈ `0.011 × totalDis`（±30%）；
6. 起步窗口内 `speed`/`bdS`/`avgSpeed`/`stepDistance` 全 0，窗口内累计位移 < 总里程 3%。

另外两处**既有**断言随之调整（都属本次缺陷范围，不是顺手改）：

- `segments_match_real_device_shape` 里「末段必是 10 秒」→「末段是余数（1..=20 s）」，
  中间段严格 20 s 的循环从 `skip(1).take(len-3)` 收到 `skip(1).take(len-2)`；
- 该测试里数起步窗口的 `startup_len(pts.len())` → `startup_point_count(...)`（同一口径）。

---

## 6. 剩余不一致 / 顺带发现

### 剩余不一致

1. **首段里程仍比真机略高**：1.14% vs 1.09%（600 s 对象 21.0 m vs 同比例 20.1 m）。
   差额来自「窗口末尾那个点已经出窗、按正常间距铺开」这一设计——它让首段
   多吃一个采样步长。已用「预留一个步长」压到 1.14%，要更贴可以再把预算
   调低，但那属于调参不是修 bug，没动。
2. **段数**：600 s 对象是 27 段，用户报的那份对象是 26 段。段数只由
   `首段 + ceil((totalTime-首段)/20)` 决定：600 s → 93 + 25×20 + 7 = 27。
   真机同比例（555 s）我们出 25 段，与真机 25 段一致。26 段对应的是
   「首段 20 s」那套旧窗口，不是比例切出来的，没有刻意去凑它。
3. **`segment_json` 的 `id` 起点**仍是 `id_base(start_ms,300)+700`（真机 733 起），
   段 `id` 的量级对得上但没有逐值对齐真机（真机是服务端自增列，本来也对不齐）。
4. **`totalDis` 是整数**：报文 `totalDis` 是 `i64`，与末点 `totalDis` 的浮点值
   天然差 <1 m。要求是 ≤0.5%（≈9 m），不是问题，只是「三者严格相等」在
   浮点/整型之间只能做到 ±1 m。

### 顺带发现（只记录，没有改）

1. **本地复现不出「Σ段 < totalDis」**。按旧窗口规则重跑同一份 600 s 轨迹，
   Σ段距离 = 1841.75 = 末点 `totalDis`（偏 0.000%）——望远镜求和本身是闭合的。
   所以那份 1559.8 vs 1833.5（差 15%，正好等于 `planner` 的长度容差带宽）
   更可能来自另一条规划支路（手绘/围栏路网那条不做缩放）或更早的构建，
   而不是段表窗口算错。现在加了归一化，任何来源的缝隙都会被补平，
   这个现象不再可能出现；但**根因没有实证**，留一条记录。
2. **`planner` 的长度容差很宽**（`geo::length_tolerance`，最多 15%/300 m）：
   几何落在容差内就不缩放，于是「用户要 1833 m、实际跑 1559 m」在
   `plan` 这一步是合法结果。一旦不缩放，报文 `totalDis` 就跟着是 1559 m 而不是
   1833 m——与真机「点集长度 == 上报里程」不冲突（我们本来就是同源上报），
   但与用户的预期对不上。要改得动 `planner` 的缩放策略（会牵动打卡点吸附），
   不在这次范围内。
3. **`compress_startup` 的边界假设**：它只压前 1.1% 的弧长。如果某个必到点
   恰好落在起点 20 m 之内，压平后采样点会离它变远，`checkpoint_track_indexes`
   的 10 m 命中半径可能失效。真机点位是散在校园各处的，实测没触发；要保险
   可以在 `plan` 里对「距起点 < 30 m 的必到点」跳过压平。
4. **`speed` 的下限夹取**（`clamp(1.9, 5.0)` m/s）在慢速段会把 0.2 m/s 抬到
   6.84 km/h。起步窗口内已用「直接写 0」绕开，但窗口外若出现慢速段（拥堵、
   绕行）仍会被抬。属于既有行为，未改。
