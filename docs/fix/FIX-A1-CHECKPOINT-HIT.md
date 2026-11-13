# FIX-A1：压缩后重新吸附打卡点（打卡点命中回归）

修的是 `docs/audit/AUDIT-round.md` A1 那条**致命回归**：只要一笔跑带必到点（`sportType=1`），
本地自检必然拦死，一单都发不出去。

- 缺陷位置：`track/mod.rs` `compress_startup`（对所有点重铺）+ `api/flow.rs` 无条件调用
- 拦截位置：`track/preflight.rs` `CP_MISS`（0.5 m，`Fatal`）→ `api/flow.rs` `report.blocked()` → `Err`
- 死亡路径：`api/flow.rs` `run_full` 第一句 `dry_run(...)?`，所以**提交流程在第一步就结束**

改动只有两处：`track/mod.rs`（新增 `snap_checkpoints` 与回归测试）、
`api/flow.rs`（`dry_run` 里在 `compress_startup` 之后、`real_len` 重算之前插一步吸附）。
`CP_MISS` 阈值 **0.5 m 没动**，`preflight.rs` 一个字没改 —— 守门员还在，改的是让它守的
数据重新成立。`web/index.html` 未触碰，没有 commit / push。

---

## 一、缺陷机制（为什么必然发生，不是概率）

三条链各自成立，接口处却互相打脸：

1. **规划侧把采样点精确放在打卡点上。**
   `track/planner.rs:241-245` 的 `nudge_to(..., 2)` 之后还有一句
   `flat[idx] = (cp.lat, cp.lng)`；围栏支路更直接
   （`track/fence_route.rs` `resample_with_checkpoints` → `pin_checkpoints`）。
   压缩前残距实测 **0.000 m**。

2. **压缩侧把所有点按目标弧长重铺一遍，包括窗口外的点。**
   `track/mod.rs:100` `compress_startup` 先算原折线累计弧长 `arc`，再给每个点算一个
   目标弧长 `want[i]`（窗口内 `cluster * t^1.5`，窗口外线性铺满），最后**沿原折线按
   `want` 重新插值**，把结果写回 `pts[i]`。
   → 「i 号点原本精确落在打卡点上」这件事完全不被保留：它的新位置由 `want[i]` 决定，
   跟打卡点无关了。

3. **自检侧按最终几何量最近距离。**
   `track/preflight.rs:228-239` 对每个打卡点取「与所有采样点的最小距离」，`> 0.5 m`
   就 `Fatal CP_MISS`。压缩后的采样网格间距 ≈5.9 m，最近点落在 ±2.95 m 内随机 →
   单点命中的概率约 17%，5 个点位全中约 0.01%。

**审计实测（本轮在仓库内复现，数字逐位对齐）**

| 打卡点 | 压缩前残距 | 压缩后残距 | 与 `CP_MISS`(0.5 m) |
|---|---|---|---|
| CP0 | 0.000 m | 0.377 m | 通过（但只剩 0.12 m 余量） |
| CP1 | 0.000 m | **0.584 m** | ✗ 致命 |
| CP2 | 0.000 m | 0.328 m | 通过 |
| CP3 | 0.000 m | 0.364 m | 通过 |
| CP4 | 0.000 m | **2.244 m** | ✗ 致命 |

复现命令（本轮固化成了测试，见第四节）：
`cargo test checkpoint_hit_after_compress -- --nocapture`

---

## 二、修法：压缩之后重新吸附，改轨迹不改打卡点

### 2.1 为什么必须放在压缩**之后**

压缩会再打散一次，压缩前吸附等于白发。所以吸附点只有一个位置：
`compress_startup` 之后、`real_len` 重算之前。

### 2.2 为什么改轨迹、不改打卡点

任务里给了两个方向，选了「把采样点挪到打卡点上」，理由：

- 打卡点是**服务端自己下发的**，服务端判「按顺序通过所有点位」用的是它那份坐标。
  把我们上报的 `fixed_point_json` 坐标改成轨迹附近（±3 m），等于让上报点位偏离真实
  点位，`type=2` 标记也跟着漂——错的方向。
- 轨迹侧本来就有 GPS 抖动，采样点落在点位 ±3 m 内是自然区间；而且位移量就是原来的
  残距（≤ 半个采样间隔），属于「采样误差」量级，不改变轨迹形状。

### 2.3 实现（`track/mod.rs::snap_checkpoints`）

```
for cp in cps:
    idx, before = 该 cp 在（未被占用的）轨迹点里的最近点与距离
    if before <= 1e-6: 已经命中，不动（起跑点常常就在 CP0 上）
    elif before <= CHECKPOINT_SNAP_MAX_M(15m):
        geo::nudge_to(&mut xy, idx, cp, 2)      # 中心点精确命中，两侧按 1/(k+1) 衰减
    else:
        skipped.push(cp)                        # 太远 = 轨迹没经过它，不硬贴，交给 preflight 报 CP_MISS
写回 pts
用**最终几何**重算每个 cp 的 after_m，以及相邻点最大间距 max_gap_m
```

四条约束，都对应任务里的要求：

| 要求 | 做法 |
|---|---|
| 残距 ≤ 0.05 m | 中心点被 `nudge_to` 直接设成打卡点坐标，`after_m` 用最终几何量，实测 **0.0000 m** |
| 不破坏轨迹形状 | 用 `nudge_to(spread=2)`，位移摊到邻域，不在折线上留单点尖刺；位移上限 = 原残距（≤ 半个采样间隔） |
| 不触发 `GEO_JUMP_SOFT` | 相邻间距最多涨 50%（实测最大 **7.2 m**，阈值 60 m；`detect` 的 `HugeGap` 20 m 也没碰到） |
| `totalDis` 与几何同源 | 吸附在 `real_len` 重算**之前**完成，重算吃的是吸附后的坐标 |

### 2.4 顺手加的两条护栏

- 单个打卡点残距 > 15 m 时**不吸附**，记进 `SnapReport::skipped` 并打日志——这时
  `preflight` 该报 `CP_MISS` 就报，不把「轨迹没经过点位」伪装成通过。
- 两个打卡点抢同一个最近下标时，先到的保住精确命中（后到的贴上去只会把前一个顶走）。

---

## 三、验收

### 3.1 打卡点命中（要求 ≤0.05 m）

`track::checkpoint_hit_tests::checkpoint_hit_after_compress`（审计同一组 5 个点位 /
2000 m / 700 s / seed 2026，planner 支路）：

```
| 打卡点 | 压缩后残距(改前) | 吸附后残距(改后) | 位移(m) |
| CP0 | 0.377 | 0.0000 | 0.377 |
| CP1 | 0.584 | 0.0000 | 0.584 |
| CP2 | 0.328 | 0.0000 | 0.328 |
| CP3 | 0.364 | 0.0000 | 0.364 |
| CP4 | 2.244 | 0.0000 | 2.244 |
| 汇总 | 最差 2.244 | 最差 0.0000 | 相邻最大间距 7.0 |
```

断言写在测试里：`min_dist(after, cp) <= CHECKPOINT_HIT_M (0.05)`，并且
`preflight` 里既没有 `CP_MISS` 也没有 `GEO_JUMP`。

### 3.2 种子扫描：40 种组合全部放行

10 个 seed × 4 组参数（1000m/360s、2000m/700s、3000m/1100s、1683m/555s）× 5 个点位。
测试 `checkpoint_hit_sweep_no_block` 对每一组**同时**跑改前（只压平）与改后（压平+吸附），
改前必须 `blocked=true`（证明扫描抓得住回归），改后必须 `blocked=false` 且残距 ≤0.05 m。

| 里程/时长 | seed | 改前最差残距(m) | 改前 CP_MISS | 改后最差残距(m) | 相邻最大间距(m) | 改后 blocked |
| 1000/360 | 1 | 2.869 | 4 | 0.0000 | 7.2 | false |
| 1000/360 | 2 | 2.591 | 3 | 0.0000 | 6.6 | false |
| 1000/360 | 3 | 2.591 | 3 | 0.0000 | 6.6 | false |
| 1000/360 | 4 | 2.814 | 2 | 0.0000 | 6.7 | false |
| 1000/360 | 5 | 2.814 | 2 | 0.0000 | 6.7 | false |
| 1000/360 | 6 | 1.949 | 4 | 0.0000 | 6.7 | false |
| 1000/360 | 7 | 1.949 | 4 | 0.0000 | 6.7 | false |
| 1000/360 | 8 | 2.270 | 2 | 0.0000 | 6.5 | false |
| 1000/360 | 9 | 2.270 | 2 | 0.0000 | 6.5 | false |
| 1000/360 | 10 | 2.659 | 4 | 0.0000 | 7.1 | false |
| 2000/700 | 1 | 1.771 | 3 | 0.0000 | 6.7 | false |
| 2000/700 | 2 | 2.192 | 4 | 0.0000 | 6.9 | false |
| 2000/700 | 3 | 2.192 | 4 | 0.0000 | 6.9 | false |
| 2000/700 | 4 | 2.723 | 3 | 0.0000 | 7.1 | false |
| 2000/700 | 5 | 2.723 | 3 | 0.0000 | 7.1 | false |
| 2000/700 | 6 | 2.623 | 4 | 0.0000 | 7.1 | false |
| 2000/700 | 7 | 2.623 | 4 | 0.0000 | 7.1 | false |
| 2000/700 | 8 | 1.917 | 1 | 0.0000 | 6.7 | false |
| 2000/700 | 9 | 1.917 | 1 | 0.0000 | 6.7 | false |
| 2000/700 | 10 | 1.792 | 4 | 0.0000 | 6.7 | false |
| 3000/1100 | 1 | 2.314 | 4 | 0.0000 | 7.0 | false |
| 3000/1100 | 2 | 2.709 | 1 | 0.0000 | 7.2 | false |
| 3000/1100 | 3 | 2.709 | 1 | 0.0000 | 7.2 | false |
| 3000/1100 | 4 | 2.670 | 2 | 0.0000 | 7.1 | false |
| 3000/1100 | 5 | 2.670 | 2 | 0.0000 | 7.1 | false |
| 3000/1100 | 6 | 2.166 | 4 | 0.0000 | 6.9 | false |
| 3000/1100 | 7 | 2.166 | 4 | 0.0000 | 6.9 | false |
| 3000/1100 | 8 | 2.820 | 3 | 0.0000 | 7.2 | false |
| 3000/1100 | 9 | 2.820 | 3 | 0.0000 | 7.2 | false |
| 3000/1100 | 10 | 2.586 | 3 | 0.0000 | 7.1 | false |
| 1683/555 | 1 | 2.723 | 3 | 0.0000 | 7.1 | false |
| 1683/555 | 2 | 2.309 | 4 | 0.0000 | 6.9 | false |
| 1683/555 | 3 | 2.309 | 4 | 0.0000 | 6.9 | false |
| 1683/555 | 4 | 2.806 | 4 | 0.0000 | 7.2 | false |
| 1683/555 | 5 | 2.806 | 4 | 0.0000 | 7.2 | false |
| 1683/555 | 6 | 2.631 | 4 | 0.0000 | 7.1 | false |
| 1683/555 | 7 | 2.631 | 4 | 0.0000 | 7.1 | false |
| 1683/555 | 8 | 2.621 | 2 | 0.0000 | 7.0 | false |
| 1683/555 | 9 | 2.621 | 2 | 0.0000 | 7.0 | false |
| 1683/555 | 10 | 2.860 | 3 | 0.0000 | 7.2 | false |

**通过率 40/40（100%）**，改前 40/40 全部被拦（`CP_MISS` 每条 1~4 个）。

两点如实说明：

- **1000m/360s 这一组走的是围栏支路。** 这 5 个点位的最短闭环是 2109 m，
  `planner::plan` 的 `slack = 目标 × 1.15` 判定「装不下」，在 `plan()` 里就返回 Err
  ——它是**规划容量**限制，跟本次修的 `CP_MISS` 是两件事（审计 probe3 把这种 Err
  也算成了「被拦」，所以那一格其实没有跑到 preflight）。围栏支路
  （`route_through_in_order` → `resample_with_checkpoints`）能覆盖 1000 m，并且在
  `flow::dry_run` 里跟 planner 支路汇合到同一个 `retime → compress_startup → snap`，
  所以测的是同一件事。
- 围栏支路的 `route_through_in_order` 用 `seed | 1` 起 xorshift，奇偶 seed 会合并成
  同一条路线 → 10 个 seed 实际是 6 条不同几何。planner 支路的 10 个 seed 是各自独立的
  （见 3.3）。

### 3.3 planner 支路扫描（可规划的 20 组）

| 支路/里程/时长 | seed | 改后最差残距(m) | 致命项 | blocked |
|---|---|---|---|---|
| planner 2000/700 | 1 | 0.0000 | 0 | false |
| planner 2000/700 | 2 | 0.0000 | 0 | false |
| planner 2000/700 | 3 | 0.0000 | 0 | false |
| planner 2000/700 | 4 | 0.0000 | 0 | false |
| planner 2000/700 | 5 | 0.0000 | 0 | false |
| planner 2000/700 | 6 | 0.0000 | 0 | false |
| planner 2000/700 | 7 | 0.0000 | 0 | false |
| planner 2000/700 | 8 | 0.0000 | 0 | false |
| planner 2000/700 | 9 | 0.0000 | 0 | false |
| planner 2000/700 | 10 | 0.0000 | 0 | false |
| planner 3000/1100 | 1 | 0.0000 | 0 | false |
| planner 3000/1100 | 2 | 0.0000 | 0 | false |
| planner 3000/1100 | 3 | 0.0000 | 0 | false |
| planner 3000/1100 | 4 | 0.0000 | 0 | false |
| planner 3000/1100 | 5 | 0.0000 | 0 | false |
| planner 3000/1100 | 6 | 0.0000 | 0 | false |
| planner 3000/1100 | 7 | 0.0000 | 0 | false |
| planner 3000/1100 | 8 | 0.0000 | 0 | false |
| planner 3000/1100 | 9 | 0.0000 | 0 | false |
| planner 3000/1100 | 10 | 0.0000 | 0 | false |

`checkpoint_hit_sweep_planner_branch`：可规划的两组（2000/700、3000/1100）× 10 seed
= 20 组全部 `blocked=false`、残距 0.0000 m；1000/360 与 1683/555 两组如实断言
`plan()` 返回的是「装不下」（若哪天变成别的错误，这条测试会失败，不会静默跳过）。

### 3.4 P1（起步窗口）成果没有回归

`track::checkpoint_hit_tests::p1_invariants_hold_with_checkpoints` —— **带 5 个打卡点**
走围栏支路（1683m/555s），把 `startup_window_and_segment_distance_sum` 那几条断言再跑一遍：

| P1 断言 | 要求 | 实测 |
|---|---|---|
| Σ段距离 vs 末点 `totalDis` | ≤0.5% | 1506.48 m vs 1506.49 m → **0.000%** |
| 报文 `totalDis` vs 末点 `totalDis` | ≤1 m | 1506 vs 1506.49 |
| 首段 `state` | `131072`（0x20000） | `131072`，其余段全 `0` |
| 首段时长 | ≈15.5% 总时长（±2s） | **86 s**（555×0.155=86.0） |
| 首段里程 | ≈1.1% 总里程（±30%） | 14.57 m（基准 16.6 m，−12%） |
| 段间缝隙 | 0 ms | 25 段首尾相接，末段 `endTime = startTime + 555000` |

原有无点位的 P1 用例 `startup_window_and_segment_distance_sum` 也在
`cargo test` 里照常通过（未被改动）。

### 3.5 测试统计与 clippy

```
$ PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR=/tmp/pace-target cargo test
test result: ok. 281 passed; 0 failed; 3 ignored

$ PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR=/tmp/pace-target cargo clippy --all-targets
Finished `dev` profile — 0 warning
```

基线 276 passed / 3 ignored → 现在 281 passed / 3 ignored（+5 条新测试，无删除、无跳过）。

> `/sdcard` 挂载不支持执行二进制，但 `CARGO_TARGET_DIR=/tmp/pace-target` 已经能把
> 产物落到 tmpfs，所以编译测试直接在 `/sdcard/workspace/pace-forge` 里跑，没有复制仓库。

### 3.6 防复发

新增测试（都在 `track/mod.rs` 的 `mod checkpoint_hit_tests`）：

| 测试 | 守什么 |
|---|---|
| `checkpoint_hit_after_compress` | 压缩后每个打卡点残距 ≤0.05 m、`preflight` 不 blocked、无 `CP_MISS`/`GEO_JUMP`；同时断言改前残距 >0.5 m（测试自己会验证它抓得住回归） |
| `checkpoint_hit_sweep_no_block` | 40 组合扫描，改前必拦 / 改后必过 |
| `checkpoint_hit_sweep_planner_branch` | planner 支路 20 组 + 「装不下」断言 |
| `p1_invariants_hold_with_checkpoints` | 带点位的 P1 六条断言 |
| `snap_only_moves_sampling_error` | 吸附位移 <5 m（不许整体拽轨迹）、点数不变 |

`checkpoint_hit_after_compress` 里那条「改前必拦」的断言是关键：它保证任何把吸附
去掉/挪到压缩前的改法都会当场变红。

---

## 四、改动函数清单

| 文件 | 符号 | 改动 |
|---|---|---|
| `track/mod.rs:183` | `CHECKPOINT_SNAP_MAX_M` | 新增常量 15 m（吸附上限，超了不硬贴） |
| `track/mod.rs:191` | `CHECKPOINT_HIT_M` | 新增常量 0.05 m（验收残距） |
| `track/mod.rs:195` | `CheckpointSnap` | 新增结构（cp / index / before_m / after_m / shifted） |
| `track/mod.rs:210` | `SnapReport` + `moved()`/`worst_before()`/`worst_after()` | 新增结构（items / max_gap_m / max_shift_m / skipped） |
| `track/mod.rs:260` | `snap_checkpoints` | **新增**：压缩后按最近下标把采样点精确放到打卡点上（`nudge_to` spread=2），最终几何重算残距与相邻最大间距 |
| `track/mod.rs:346` | `mod checkpoint_hit_tests` | **新增** 5 条回归测试 |
| `api/flow.rs:1039-1066` | `dry_run`（⑤.6 段） | **新增一步**：`compress_startup` 之后调 `snap_checkpoints(&mut track_points, &snap_cps)`，打日志（改前/改后残距、最大位移、相邻最大间距、被跳过的点位），然后才重算 `real_len` |

未改动（刻意）：`track/preflight.rs`（`CP_MISS` 阈值仍是 0.5 m / Fatal）、
`track/planner.rs`、`track/fence_route.rs`、`api/submit.rs`、`web/index.html`。

`compress_startup` 本身也**没改**——它「按弧长重铺所有点」是 P1（起步窗口 15%/1.1%）
的实现基础，动它等于把 P1 一起推翻；打散打卡点是它的副作用，正确的位置是在它之后补一步。

---

## 五、没做的事（如实记）

- **没有放宽 `CP_MISS` 阈值。** 0.5 m 保持原样：这只守门员是本地唯一挡「点位没被经过」
  的东西，服务端真实容差目前没有证据（`docs/audit/AUDIT-round.md` C2）。
- **没有在压缩前吸附。** 压缩会再打散，那样等于没吸。
- **`planner` 的「装不下」判定没动。** 那 5 个点位最短闭环 2109 m，1000 m / 1683 m
  的目标在 planner 支路里确实装不下，把 `slack` 放宽等于允许轨迹穿过点位之外几百米，
  是另一个话题。围栏支路能覆盖这些参数，生产默认路径也不受影响。
- **`Startup` 段内的点也会被吸附。** 压缩段内的采样间距只有 0.1~0.3 m，那里的残距本来
  就 ≤ 半个间距（≤0.15 m），吸附位移也是这个量级，不会在密集段制造尖刺；实测
  相邻最大间距 7.2 m 就是窗口边界那一段。
