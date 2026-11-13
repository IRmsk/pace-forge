# FIX-A5：`queueNum` 从「线性插值 + round」改成「固定步长 floor 分区」

审计出处：`AUDIT-round.md` A5。本文件只修这一个字段，不涉及 `api/flow.rs`、
`api/fence.rs`、`api/session.rs`、`web/index.html`（并行任务正在改，见 §7）。

---

## 0. 结论

| 项 | 修前 | 修后 |
|---|---|---|
| 点位（真机 114 点） | 与真机差 **14/114**（`22,26,30,…,62,66,…` 恒为真机 +1，另有 107/111） | 与真机差 **2/114**（仅尾部 107、111，原因见 §5） |
| 速度桶 / 步频桶（真机 55 桶） | 与真机差 **22/55**（`9,11,13,…,53` 恒为真机 +1） | 与真机差 **0/55**，逐元素完全相等 |
| 提交报文的桶 `queueNum` | `i as i32` → **0..54**（量纲整个错，真机上限 24） | 与 OBS 桶同一条分区函数 → **0..24** |
| 注释 | 「每 5 个点 +1」 | 「每 4 个点 +1 / 每 2 个桶 +1」 |

---

## 1. 真机序列（判据）

真机 rrid 1328288734，555 秒 / 1683.83m / 114 点 / 55 桶。

**点位 `raw_points[i].queueNum`（114 个）**

```
0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,            → 下标 0..18 全 0（19 个）
1,1,1,1, 2,2,2,2, 3,3,3,3, …                       → 从下标 19 起每 4 个点 +1
变化位置：19,23,27,31,…,99,103,108,112
相邻步长：4,4,4,…,4,5,4                            → 只有 103→108 这一步是 5
尾部宽度：22 档占 5 个点(103..107)、23 档占 4 个点(108..111)、24 档占 2 个点(112..113)
```

**速度桶 `speed_json[i].queueNum`（55 个）**

```
0,0,0,0,0,0,0,0,                                   → 下标 0..7 全 0（8 个）
1,1,2,2,3,3,…,23,23,24                             → 从下标 8 起每 2 个桶 +1
变化位置：8,10,12,…,52,54（步长恒为 2）
```

两条序列都以常量数组的形式写进了测试（`track/mod.rs::queue_num_tests::
REAL_POINT_QUEUE`、`track/buckets.rs::id_tests::REAL_BUCKET_QUEUE`），
测试直接逐下标比，不再用区间夹逼。

---

## 2. 旧实现错在哪

```rust
// 旧：线性插值 + round（track/mod.rs，改前）
let span = (n - 1 - startup).max(1) as f64;
let step = (pos - startup) as f64 / span;
(1.0 + step * 23.0).round().clamp(1.0, 24.0) as i32
```

1. **口径错**：真机是「把起步段之后的采样格切成 24 段等宽整数台阶」，不是按位置
   比例连续插值。114 点 / 94 个待分格 → 真机每档恰好 4 个点；线性插值给的是
   `94/23 = 4.09` 个点一档，**每档宽 0.09 个点，误差一路累积**，于是从第 3 档
   （下标 22）开始，档位边界比真机晚 1 个点，一直到下标 62 才因为 `round` 的四舍
   五入追上，随后再次漂开。
2. **桶侧更糟**：桶是 46 个待分格铺 23 档，真机恰好 2 个桶一档；线性插值
   `46/23 = 2.0` 本应整除，但 `round` 在半档处（`k+0.5`）取上，奇数下标全部偏
   一档（9,11,…,53，共 22 个）。
3. **注释把步长写错**：原注释「第 19 个点开始每 5 个点 +1」。真机是 4 个点、
   桶是 2 个桶。
4. **提交报文的桶 `queueNum` 是另一个量纲**：`apply_submit_ids` 里
   `b.queue_num = i as i32`（`api/submit.rs` 的 `test_sim` 同形），55 个桶于是发
   `0..54`。真机全场 `queueNum` 上限是 24，`25..54` 是不存在的台阶——这不是
   「差几档」，是字段语义错。

---

## 3. floor 公式与验算

```rust
// 新（track/mod.rs:350）
pub fn queue_num(pos: usize, n: usize, startup: usize) -> i32 {
    if n == 0 || pos < startup || n <= startup + 1 {
        return 0;
    }
    let span = n - 1 - startup;          // 起步段之后还有多少格
    let step = span.div_ceil(24).max(1); // ceil(span / 24)，整数运算不碰浮点
    (1 + (pos - startup) / step).min(24) as i32
}
```

`1 + floor((pos - startup) / ceil((n - 1 - startup) / 24))`，`pos < startup` 时为 0。

**点位** n=114 / su=19：`span = 94`，`step = ceil(94/24) = 4`

| pos | 期望 | 计算 | 真机 |
|---|---|---|---|
| 19 | 1 | `1 + 0/4` | 1 ✅ |
| 23 | 2 | `1 + 4/4` | 2 ✅ |
| 103 | 22 | `1 + 84/4 = 22` | 22 ✅ |
| 108 | 23 | `1 + 89/4 = 23` | 23 ✅ |
| 112 | 24 | `1 + 93/4 = 24` | 24 ✅ |

**桶** n=55 / su=8：`span = 46`，`step = ceil(46/24) = 2`

| pos | 期望 | 计算 | 真机 |
|---|---|---|---|
| 8 | 1 | `1 + 0/2` | 1 ✅ |
| 10 | 2 | `1 + 2/2` | 2 ✅ |
| 52 | 23 | `1 + 44/2` | 23 ✅ |
| 54 | 24 | `1 + 46/2` | 24 ✅ |

---

## 4. 桶的 `su` 凭什么取 8，而不是点位的 19

`su` 是**同一个时间窗口（起步 86 秒）在两张不同网格上的落点**，不是从点位抄来的：

- 点位是 5 秒一个采样点（真机采样间隔 3~5 秒），86 秒窗口里落了 **19 个点**
  ——这正是 `startup_point_count()` 数出来的值（`times < 86s`，真机索引 0..18）；
- 桶是 10 秒一格，86 秒窗口盖住前 **8 个整桶**（0~80s）；第 9 个桶是 80~90s，
  跨在 86 秒这条线上，真机把它算作「已起步」（它的 `state` 已经是 0，
  `queueNum` 已经是 1）。

所以桶的 `su = floor(startup_sec / 10) = floor(86/10) = 8`，实现为
`track/buckets.rs:294` 的 `bucket_startup(total_sec) = (startup_sec/10)`，
**整数除法（截断）而不是四舍五入**：`round(86/10) = 9` 会把第 9 个桶也算成 0 档，
和真机差一档。

反过来验：`su = 8, n = 55 → ceil(46/24) = 2 → 8→1、10→2、…、54→24`，55 个桶逐
元素与真机完全相等（§6）。若误用点位的 `su = 19`，桶序列会变成
`0×19, 1,1,2,2,…`，与真机差 19 个下标的 0 段——所以这个值必须由桶自己的网格算。

另外两个用例说明 `bucket_startup` 的取值是稳定的：

| total_sec | startup_sec | 桶 su | N 桶 | step | 结果 |
|---|---|---|---|---|---|
| 555 | 86 | 8 | 55 | 2 | 0×8 后每 2 桶 +1 到 24 ✅ 与真机一致 |
| 720 | 112 | 11 | 72 | 3 | 0×11 后每 3 桶 +1，末桶落在第 21 档（`24×3 = 72 > 60`，整数台阶用不满 24 档，见 §8） |
| 60 | 9 | 0 | 6 | 1 | 没有整桶落在起步窗口里，直接 1..6 |

---

## 5. 尾部两处差异：为什么 floor 公式复现不了 107/111

修后逐元素比对，点位只剩两名不一致，且方向固定（我们高一档）：

| 下标 | 真机 | 我们 | 说明 |
|---|---|---|---|
| 107 | 22 | 23 | 真机 22 档覆盖 103..107（**5 个点**），等宽网格只覆盖 103..106 |
| 111 | 23 | 24 | 真机 23 档覆盖 108..111（4 个点），等宽网格是 107..110 |

**这是等宽整数网格表达不了的尾部余数分配，数学上无解**：

设档位 `q(i) = 1 + floor((i - 19) · K)`。

- 前 21 档都是 4 个点一档，要求 `K = 1/4`（`q(23) = 2 ⇒ 4K ≥ 1`，`q(22) = 1 ⇒ 3K < 1`）；
- 真机 `q(107) = 22` 要求 `88K < 22 ⇒ K < 0.25`；
- 真机 `q(103) = 22` 要求 `84K ≥ 21 ⇒ K ≥ 0.25`。

两条约束互斥 ⇒ 不存在任何「等宽线性档位」能同时满足前 21 档与尾部。
真机尾部宽度 5/4/2 说明 App 侧在收尾时把余数（95 − 84 = 11 个点铺 3 档）做了
自己的分配，形态上不是等宽网格。**能做的最接近的实现就是等宽网格**：它把 14 个
错误下标压到 2 个，且 2 个都落在最后一档边界上、方向一致、不影响任何中间段。

测试没有把这 2 个差异「放宽成大区间」，而是**写死**：

```rust
// track/mod.rs::queue_num_tests::point_queue_num_matches_real_device_element_by_element
assert_eq!(diffs, vec![(107, 22, 23), (111, 23, 24)], "尾部的两处已知差必须恰好是 …");
// 其余 112 个下标逐个 assert_eq!(ours[i], real)
```

后果评估：`queueNum` 是「第几个采样档」的序号，尾部这两点差一档不改变任何其它
字段（不参与 `speed`/`avgSpeed`/打卡判定），且限制在最后 6 个点里。

---

## 6. 修复前后逐元素对比（离线对象实测）

用真机 114 点（坐标 + 真实时间戳）喂进 `build()`，打印 `allLocJson[].queueNum`；
用同一条轨迹切 10 秒桶后打印 `speedPerTenSec[].queueNum`。探针跑在仓库的一份
`/tmp` 副本里（**仓库本身没加探针文件**），`su` 由链路自己数出来（`su_points=19`）。

### 6.1 点位前 40 个

```
真机   [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,1,1,1,2,2,2,2,3,3,3,3,4,4,4,4,5,5,5,5,6]
修前   [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,1,1,2,2,2,2,3,3,3,3,4,4,4,4,5,5,5,5,6,6]
修后   [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,1,1,1,2,2,2,2,3,3,3,3,4,4,4,4,5,5,5,5,6]
```

修前从下标 21 起就开始晚一档；修后前 40 个与真机**逐元素完全相等**。

### 6.2 点位全部 114 个（修后 vs 真机）

```
真机   [0×19, 1×4, 2×4, 3×4, …, 20×4, 21×4, 22×5, 23×4, 24×2]
修后   [0×19, 1×4, 2×4, 3×4, …, 20×4, 21×4, 22×4, 23×4, 24×3]
差异   仅下标 107（22→23）、111（23→24）
```

（完整数组见测试里的 `REAL_POINT_QUEUE`，共 114 项。）

### 6.3 全部 55 个桶（修后 vs 真机）

```
真机   [0,0,0,0,0,0,0,0,1,1,2,2,3,3,4,4,5,5,6,6,7,7,8,8,9,9,10,10,11,11,12,12,13,13,14,14,
        15,15,16,16,17,17,18,18,19,19,20,20,21,21,22,22,23,23,24]
修后   [0,0,0,0,0,0,0,0,1,1,2,2,3,3,4,4,5,5,6,6,7,7,8,8,9,9,10,10,11,11,12,12,13,13,14,14,
        15,15,16,16,17,17,18,18,19,19,20,20,21,21,22,22,23,23,24]
差异   无（0/55）
```

步频桶同一序列（`PROBE buckets step_ours` 与 `ours` 一致）。

### 6.4 修前的差集（审计数字复核）

```
点位 14 处：(22,1→2) (26,2→3) (30,3→4) (34,4→5) (38,5→6) (42,6→7) (46,7→8)
           (50,8→9) (54,9→10) (58,10→11) (62,11→12) (66,12→13) (107,22→23) (111,23→24)
桶   22 处：(9,1→2) (11,2→3) (13,3→4) (15,4→5) (17,5→6) (19,6→7) (23,8→9) (25,9→10)
           (27,10→11) (29,11→12) (31,12→13) (33,13→14) (35,14→15) (37,15→16) (39,16→17)
           (41,17→18) (43,18→19) (45,19→20) (47,20→21) (49,21→22) (51,22→23) (53,23→24)
```

与 `AUDIT-round.md` A5 的表格（14/114、22/55）逐项一致，说明这次修的是审计点名的
那件事，不是别的东西。

### 6.5 提交报文的桶（修前是 `i as i32`）

| | 前 12 个 | 末 3 个 | 上限 |
|---|---|---|---|
| 修前（55 桶） | 0,1,2,3,4,5,6,7,8,9,10,11 | 52,53,54 | **54** ❌ |
| 修后（55 桶） | 0,0,0,0,0,0,0,0,1,1,2,2 | 23,23,24 | **24** ✅ |

（720 秒 / 72 桶的默认参数下：修前 0..71，修后 0×11 + 每 3 桶 +1，上限 ≤ 24。）
断言 `submit_buckets_use_60000_ids` 里加了 `assert!(max <= 24)` 与
`assert!(max < n-1)` 两条，防止有人再把它写回桶序号。

---

## 7. 改动清单（只动这三处）

### 7.1 我改的行

| 文件 | 行 | 内容 |
|---|---|---|
| `track/mod.rs` | 331–358 | `queue_num` 实现换成 floor 分区（整数 `div_ceil`，无浮点）+ 注释改正（每 4 个点 / 每 2 个桶，尾部分配说明） |
| `track/mod.rs` | 360–489 | 新增 `#[cfg(test)] mod queue_num_tests`：真机 114 点 / 55 桶两套序列常量、逐元素断言、线性插值回归断言、边界断言 |
| `track/buckets.rs` | 285–296 | 新增 `bucket_startup(total_sec)`：桶网格上的起步窗口（整桶截断），含判定依据注释 |
| `track/buckets.rs` | 298–318 | `apply_submit_ids`：`queue_num` 从 `i as i32` 改成 `super::queue_num(i, n, su)` |
| `track/buckets.rs` | 320–356 | `apply_obs_ids`：`su` 改走 `bucket_startup`（等价重构，行为不变），注释补「每 2 个桶 +1」 |
| `track/buckets.rs` | 362–395 | `submit_and_obs_ids_differ`：`sp[0]==0 / sp[1]==1` 的旧断言按新语义改成整段比对 |
| `track/buckets.rs` | 397–455 | 新增 `REAL_BUCKET_QUEUE`；`obs_states_and_queue_follow_real_device`（408–431）从「只断首尾」改成逐元素；新增 `submit_bucket_queue_follows_real_device`（434–455） |
| `api/submit.rs` | 579–583 | `queueNum` 调用点注释改正（每 4 个点 +1，说明不是线性插值） |
| `api/submit.rs` | 1409–1411 | `apply_submit_ids` 前的注释：`queueNum` 不是 `i` |
| `api/submit.rs` | 1544–1581 | `tests::test_sim`：加 `let su = bucket_startup(...)`（1547），两处 `queue_num: i as i32` → `crate::track::queue_num(i, n, su)`（1562、1576） |
| `api/submit.rs` | 1729–1772 | `submit_buckets_use_60000_ids`：旧的 `[0]==0 / [1]==1` 断言换成整段 72 桶逐元素比对 + 上限 24 断言 |
| `FIX-A5-QUEUE-NUM.md` | 新文件 | 本报告 |

### 7.2 我没有碰的文件（并行任务在改）

```
$ cd /sdcard/workspace/pace-forge && git diff --stat
 api/fence.rs     |   67 +-     ← 并行任务
 api/flow.rs      |  830 ++--   ← 并行任务（api/flow.rs + 可能含 submit 的 build）
 api/session.rs   |  188 ++-    ← 并行任务
 api/submit.rs    | 2086 ++--   ← 并行任务的主体改动 + 我的 4 处（见 7.1）
 track/buckets.rs |  153 ++-    ← 我（另有本任务之前 A-系列的既有改动）
 track/mod.rs     | 1041 ++-    ← 我（另有本任务之前 A-系列的既有改动）

$ git diff --stat web/index.html
(空 —— 一行未动)
```

`api/submit.rs` 里我改的 4 处全部落在 `tests` 模块内的 `test_sim` /
`submit_buckets_use_60000_ids` 和两处注释上；`build()` 的函数体、`api/flow.rs`
的 `detail_payload_rrid` / `point_json_full` 调用链一行未动。每次写盘前都跑了
`git diff --stat && git diff api/submit.rs | head -40`，未回滚任何并行改动。

---

## 8. 已知副作用（不掩盖）

`step = ceil(span/24)` 是整数台阶：当 `span` 不是 24 的整数倍附近时，顶部几档
用不满。例：720 秒 / 72 桶 → `su=11`、`span=60`、`step=3`，`24×3 = 72 > 60`，
末桶落在**第 21 档**而不是 24。真机 555 秒那两组（94/4、46/2）恰好走到 24。
这不是 bug 而是等宽整数网格的固有性质；改成「按比例拉伸以必然触顶」的变体
（`1 + floor(24·(pos-su)/span)`）会立刻退化：点位差 **14/114**、桶差 **12/55**
（已实测），比现在差得多。因此保留 `ceil` 版本。

---

## 9. 验收

### 9.1 逐元素断言（测试里跑的不是「大体一致」）

- `track::queue_num_tests::bucket_queue_num_matches_real_device_element_by_element`
  —— 55 个桶 × `queue_num(i,55,8)`，**要求完全相等**；
- `track::queue_num_tests::point_queue_num_matches_real_device_element_by_element`
  —— 114 个点，112 个要求完全相等，尾部 2 个写死期望值 `(107,22,23)(111,23,24)`，
  并有「变化位置」序列的独立断言；
- `track::buckets::id_tests::obs_states_and_queue_follow_real_device`
  —— 真机 55 桶：速度桶 + 步频桶逐元素、`state` 前 8 个 2 其余 0；
- `track::buckets::id_tests::submit_bucket_queue_follows_real_device`
  —— 提交路径 55 桶逐元素 + 上限 24；
- `api::submit::tests::submit_buckets_use_60000_ids`
  —— 720 秒 72 桶：id `60000+i`、`queueNum` 与分区函数逐桶相等、`<= 24`、不退化成序号。

### 9.2 全量测试

```
$ PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR=/tmp/pace-target cargo test
running 292 tests
test result: ok. 289 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out
```

基线（本任务开工时）：`282 passed; 2 failed; 3 ignored`——那 2 个失败是并行任务
在 `api/flow.rs` 里还没落定的 `single_point_source_tests`（`bdA` 自检/上传不一致），
与本任务无关；本任务收尾时并行任务自己的改动已经落定，全量转绿（289/0/3）。

本任务相关测试（含点名复核）单独跑：

```
$ cargo test --lib -- checkpoint_hit_after_compress checkpoint_hit_sweep_no_block \
    startup_window_and_segment_distance_sum p1_invariants_hold_with_checkpoints \
    point_speed_fields_match_real_device_point_by_point queue_num \
    obs_states_and_queue submit_bucket_queue submit_buckets_use_60000
test result: ok. 12 passed; 0 failed; 0 ignored
```

| 点名项 | 结果 |
|---|---|
| `checkpoint_hit_after_compress`（A1） | ok |
| `checkpoint_hit_sweep_no_block`（A1） | ok |
| `startup_window_and_segment_distance_sum`（P1） | ok |
| `p1_invariants_hold_with_checkpoints`（P1） | ok |
| `point_speed_fields_match_real_device_point_by_point`（A2） | ok |
| `queue_num_tests::*`（4 个，本任务新增） | ok |
| `obs_states_and_queue_follow_real_device` / `submit_bucket_queue_follows_real_device` | ok |
| `submit_buckets_use_60000_ids` | ok |

### 9.3 clippy

```
$ PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR=/tmp/pace-target cargo clippy --all-targets
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 14.58s
```

零告警（这次是 `touch lib.rs` 之后的真实重编译，不是缓存命中）。中间出现过一条
`api/flow.rs:2432 needless_range_loop`，那是并行任务的文件，已随其改动一起消失。
