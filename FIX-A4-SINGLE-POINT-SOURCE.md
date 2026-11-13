# A4 修复：点位只有一个来源（自检校验的 == 上传的）

> 范围：只修「同一轨迹造两份点位」这一处。没碰 `web/index.html`，没做 commit / push。
> 交付物顺序：差异表（修复前）→ 修法 → 白名单说明 → 红/绿证据 → 并排对比表 → 验收。

---

## 0. 结论速览

| 项 | 修复前 | 修复后 |
|---|---|---|
| 点位构造次数（一条轨迹） | **2 次**（`submit::build` 一次，`flow::detail_payload_rrid` 又造一次） | **1 次**（`submit::build_points`，其余全部传引用） |
| 自检校验的对象 | `submit::build` 那份（seed `start_ms^φ`、平滑过、打卡点 `&[]`） | 与上传**同一份** `Vec<Value>` |
| 上传到 OBS 的对象 | `detail_payload_rrid` 内部重建那份（seed `start_ms`、不平滑、cp 靠 envelope 反推） | 就是上面那同一份 |
| `id`/`count`/`radius`/`type`/`bdA` | 逐点不同 | 逐字节相同 |
| `type=2` 打卡标记 / `isFixed` | 只有上传那份有（probe 传空 `fixed`） | 两份都有 |
| `point_json_full` 调用点 | 2 处（`build`、`detail_payload_rrid`）+ A2 测试里 1 处手抄 | **1 处**（`build_points` 内） |
| `cargo test` | 有红（本条回归测试） | 289 passed / 0 failed / 3 ignored |
| `cargo clippy --all-targets` | — | 0 告警 |

---

## 1. 修复前：两份实例的差异表

同一条轨迹，被两个地方各自按自己的入参拼了一遍点位。

| 维度 | 自检那份（`submit::build` → `verify_view`/`inspect`） | 上传那份（`flow::detail_payload_rrid` → OBS `run_data.allLocJson`） |
|---|---|---|
| 代码位置 | `api/submit.rs` 原 `build()` 尾部（`loc_items`） | `api/flow.rs` 原 `detail_payload_rrid()` 内部 `let pts: Vec<Value> = {…}` |
| **seed** | `params.start_time_ms as u64 ^ 0x9E3779B97F4A7C15` | `start_ms as u64`（没有 φ） |
| **平滑** | `smooth_point_speed(&mut loc_items)` | 不调（A2 只把口径对齐了，链路本身仍然不平滑） |
| **打卡点入参** | `fixed`（但 `flow::dry_run` 的 probe 传的是 `&[]`） | 从 `fivePointJson` envelope 反解 `checkpoint_track_indexes` |
| `alt_range` | `params.alt_range` | `p.alt_range`（同一个值，唯一一处本来就一致） |
| 受影响字段 | `id`（`id_base(seed,1200)+3000+idx`）、`count`、`radius`、`type`、`bdA`（相位 `ph(k)=f(seed)`）、`flag`… | 同上，逐点不同 |
| `type=2` 打卡标记 | probe 传 `&[]` → **一个都没有** | 有（21~27 附近那段连续标记） |
| `fivePointJson`（`isFixed` / `isPass` / `position` / `id`） | `fixed_point_items(&[], …)` → **空数组** | 5 条完整必到点 |

### 1.1 实测差异（离线对象：334 点 / 1683m / 555s / 5 个打卡点）

修复前同一份 `track`+`fixed`+`sim`，两处各自拼出来的前 6 个点（`≠` 是逐字段比对的判定）：

```
[fixture] 334 点 / 1683m / 555s / 1500 步
idx | 自检: id / count / radius / type / bdA | OBS 上传: 同五项
  0 |           3335    51    1.6     7    146.7 |           3500    40    1.4     7    150.3 ≠
  1 |           3336    41    2.3     5    147.5 |           3501    42    1.7     5    149.3 ≠
  2 |           3337    48    2.1    -1    148.4 |           3502    45    1.4    -1    147.3 ≠
  3 |           3338    52    2.3    -1    148.1 |           3503    47    2.3    -1    145.9 ≠
  4 |           3339    40    2.1    -1    146.3 |           3504    46    2.2    -1    146.2 ≠
  5 |           3340    49    1.6    -1    144.1 |           3505    48    1.5    -1    148.1 ≠
```

（跑这一份时夹具还是 `laps=4.72` → 1682m；修好后为对齐 1683m 微调到 `4.723`。判据与结论不受影响。）

`id` 差 165（两个 seed 落在 `id_base` 的不同桶），`count` 40~53 两个区间都「合法」但逐点不同，
`radius` 1.4~2.3 也一样，`bdA` 相位完全错开——**任何按字段区间做的自检都查不出来**，
只有逐点对拍才看得见。

---

## 2. 修法

### 2.1 结构（改后）

```text
                       ┌───────────────────────────────────────────┐
  track(几何+时间戳) ──► │  submit::build_points(track, fixed,       │──► Vec<Value>  ← 全流程唯一一次构造
  fixed(打卡点)      ──► │      start_ms, alt_range)                 │      │
  start_ms(seed/flag) ─► │  · point_json_full ×N（id/count/radius/   │      │
                         │    type/bdA/queueNum…）                   │      │
                         │  · checkpoint_track_indexes(fixed) → type=2│     │
                         │  · startup_point_count(timestamps)        │      │
                         │  · smooth_point_speed（一次）              │     │
                         └───────────────────────────────────────────┘      │
                                                                            │
   dry_run ──► probe build ── build_from_points(probe, …, points.clone()) ──┤
                 │                                                          │
                 ├─► verify_view(body, &points) → inspect()  ← 自检（逐点校验的就是这份）│
                 └─► RunOutcome { submission_points: points, fixed }  ─────┤
                                                                            │
   run_full ─► build_from_points(真实 params, &out.fixed, out.submission_points.clone())
                 │                                    │                     │
                 │                                    └─► audit_derived(body, &points)
                 └─► Submission.points ──────────────► detail_payload_rrid(…, &submission.points)
                                                          （只序列化这个引用，不重算）
```

一句话：**点位数组只造一次，从 `dry_run` 一路带到 OBS 上传**；两次 `build_from_points`
只负责「套不同的 envelope（身份 / 围栏 / 签名）」，不再碰点位。

### 2.2 代码改动清单

| 文件 | 改动 |
|---|---|
| `api/submit.rs` | 新增 [`build_points`](api/submit.rs:1267)：**唯一点位构造点**（`point_json_full` ×N + `checkpoint_track_indexes` + `startup_point_count` + `smooth_point_speed`），入参只有 `track/fixed/start_ms/alt_range` 四项；新增 [`build_from_points`](api/submit.rs:1329)：用「已造好的点位」组装报文（校验条数与轨迹一致）；[`build`](api/submit.rs:1235) 收缩成 `build_points` + `build_from_points` 两行 |
| `api/submit.rs` | `Submission::points` 的注释改成「与自检 / OBS 明细同一份实例」；`step_distance_const` 的注释里那句「`detail_payload_rrid` 传的 seed 不同」是 A4 的事实描述，改成「点位已收敛到 `build_points`」；`smooth_point_speed` 的「两条链同一口径」同步更新 |
| `api/flow.rs` | `RunOutcome` 新增 [`submission_points`](api/flow.rs:467) 与 [`fixed`](api/flow.rs:472)：`dry_run` 造一次、自检用它、`run_full` 复用它 |
| `api/flow.rs` | `dry_run`：把「服务端点位 → `FixedPoint`」的映射**提前到 probe 之前**（原来在 `run_full` 里，probe 传 `&[]`），probe 现在传真实 `fixed`；preflight 通过后加一条守卫：`fixed` 非空却一个 `type=2` 都没有 → 直接失败（防「probe 又把 fixed 传空」） |
| `api/flow.rs` | `run_full`：删掉重复的 `FixedPoint` 构造，改用 `out.fixed`；提交报文用 `build_from_points(…, out.submission_points.clone())`，并断言 `submission.points == out.submission_points`（否则报「A4 回归」） |
| `api/flow.rs` | `detail_payload_rrid` **不再接受轨迹点去重建**，签名改为接收 `pts: &[Value]`，直接用 `serde_json::to_string(pts)` 写 `run_data.allLocJson`（零复制） |

### 2.3 单元测试同步

| 测试 | 处理 |
|---|---|
| `obs_payload_has_ten_gzipped_keys` | 点位改由 `submit::build_points` 提供后传引用（原来它自己造 50 个点，只测 envelope） |
| `obs_checkpoint_marking_uses_coords_not_position` | 改用真实链路：`build_points(&pts, &[该点 FixedPoint], …)` → `detail_payload_rrid(…, &loc)` → 解 `run_data`。断言收紧成「第 25 点必须在标记集合里 + 标记不得出现在轨迹开头」——原来那句「全部标记必须落在 21~27」是错的：`type=2` 同时也是「次一级打卡 / 弱定位」，`point_json_full` 会给普通点按 5% 概率撒这个值（真机分布里 type=2 有 5 个点），换 seed 后多出 31/47/48 三个随机 2 是既有设计，不是回归 |
| `point_speed_fields_match_real_device_point_by_point`（A2） | 把里面**手抄的 OBS 链**（自己再调一遍 `point_json_full`）换成真正上传对象反解：`detail_payload_rrid(…, &sub.points)` → gunzip `run_data` → `allLocJson`。谁把 `detail_payload_rrid` 改回「自己再拼一份」，这条立刻红 |

---

## 3. 白名单说明（哪些差异是必然的）

`selfcheck_and_upload_points_are_identical` 里有两张白名单，规则是「envelope 可以有、点位一个都不许」：

| 白名单 | 内容 | 为什么这些差异是**必然**的 |
|---|---|---|
| `POINT_ALLOWED_DIFFS` | **空表** | 点位由同一份 `Vec<Value>` 派生，任何字段差异都只能是 bug。测试额外断言这张表必须为空，且 envelope 白名单里不许混进 [`POINT_FIELDS`](api/flow.rs:2157) 里的任何点位字段名 |
| `ENVELOPE_ALLOWED_DIFFS` | `uid`、`selectedUnid`、`uuid` | 自检用占位身份（`probe` / `0` / 全 0 UUID），校验不看身份字段；上传必须用真实账号 |
| | `signature`、`originalSign`、`geeToken` | 这三个都是 `uid`+`uuid` 的派生量（31 字段签名、GEE 帧），身份不同必然不同 |
| | `runAreaId` | probe 没有围栏上下文（`-1`），上传要发真实围栏 id（`save_record` 顶层那个字段；`fivePointJson` 里那个仍按真机发 `-1`，见 `payload_run_area_id`） |
| | `fivePointJson` | 它外层是 envelope（`geoFencesJson` 一个是 `[]`、一个是真实围栏对象），所以整串会不同。**但内层点位数组必须逐字段相同**——测试单独用 `five_point_items()` 抽出来比对（`id` / `isFixed` / `position` / `isPass` / `flag` 全等） |

OBS 对象层面还有两个「必然要等提交后才有」的量：`rrid`（桶 id 与 objectKey 都要它）与
对象 key 本身。这正是「点位必须与 envelope 解耦」的理由——**点位不跟着 rrid 一起重算**：
`detail_payload_rrid` 只拿 `&submission.points` 去序列化，`rrid` 只影响 `speed_json` /
`step_freq_json` / `laps_json` 的 id 与对象 key。

---

## 4. 修复前红 / 修复后绿

### 4.1 红（修复前，同一条测试）

```
$ cargo test --lib single_point_source -- --nocapture
thread 'api::flow::single_point_source_tests::selfcheck_and_upload_points_are_identical' panicked at api/flow.rs:2274:17:
assertion `left == right` failed: 自检 vs OBS 上传 第 0 点字段 `bdA` 不同：自检 Number(146.7) vs 上传 Number(150.3)
  left: Number(146.7)
 right: Number(150.3)

test api::flow::single_point_source_tests::selfcheck_and_upload_points_are_identical ... FAILED
test api::flow::single_point_source_tests::print_selfcheck_vs_upload_first_six_points ... FAILED
test result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 285 filtered out; finished in 0.32s
```

它红的位置就是缺陷所在：`bdA`（seed 派生相位）、`id`、`count`、`radius`、`type` 一路都不等。
把断言逐个放开也修不好——根因是点位被造了两次。

### 4.2 绿（修复后）

```
$ PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR=/tmp/pace-target cargo test
test result: ok. 289 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 4.94s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

$ PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR=/tmp/pace-target cargo clippy --all-targets
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.81s      ← 0 告警
```

点名复核（8/8 通过，一条没动）：

```
$ cargo test --lib -- checkpoint_hit_after_compress checkpoint_hit_sweep_no_block \
    checkpoint_hit_sweep_planner_branch p1_invariants_hold_with_checkpoints \
    snap_only_moves_sampling_error startup_window_and_segment_distance_sum \
    point_speed_fields_match_real_device_point_by_point selfcheck_and_upload_points_are_identical
test track::checkpoint_hit_tests::snap_only_moves_sampling_error ... ok
test api::submit::parity_regression_tests::startup_window_and_segment_distance_sum ... ok
test api::submit::parity_regression_tests::point_speed_fields_match_real_device_point_by_point ... ok
test track::checkpoint_hit_tests::p1_invariants_hold_with_checkpoints ... ok
test track::checkpoint_hit_tests::checkpoint_hit_after_compress ... ok
test api::flow::single_point_source_tests::selfcheck_and_upload_points_are_identical ... ok
test track::checkpoint_hit_tests::checkpoint_hit_sweep_planner_branch ... ok
test track::checkpoint_hit_tests::checkpoint_hit_sweep_no_block ... ok
test result: ok. 8 passed; 0 failed
```

打卡点吸附顺序**没动**：`compress_startup` → `snap_checkpoints` → `real_len` 重算，
`api/flow.rs` 那三段的相对位置保持原样（A1 的 4 条断言全绿即证据之一）。

### 4.3 关于测试条数（282 → 289）

本会话第一次跑 `cargo test` 报的是 **282 passed / 3 ignored**（与历史报告一致），
但重编译后同一份源码执行的是 **292 条**（`grep -c '#\[test\]'` 逐文件求和也是 292）。
两次之间我没有增删过别人的测试，差额只能来自共享的 `CARGO_TARGET_DIR=/tmp/pace-target`
里那份陈旧的测试二进制（本轮多次出现 `Blocking waiting for file lock on build directory`，
该目录被多个执行智能体共用）。以源码为准：

| 口径 | 数字 |
|---|---|
| 源里 `#[test]` 总数（含本轮新增 2 条） | 292 |
| 本轮执行 | 289 passed + 3 ignored = 292，0 failed |
| 排除本轮新增模块（`--skip single_point_source_tests`） | **287 passed / 3 ignored / 2 filtered** = 290 |
| 净增 | +2（`selfcheck_and_upload_points_are_identical`、`print_selfcheck_vs_upload_first_six_points`），无删除、无跳过 |

---

## 5. 修复后并排对比（同一个离线对象：334 点 / 1683m / 555s / 5 个打卡点）

```
$ cargo test --lib single_point_source -- --nocapture
[fixture] 334 点 / 1683m / 555s / 1500 步
idx | 自检: id / count / radius / type / bdA | OBS 上传: 同五项
  0 |           3335    51    1.6     7    146.7 |           3335    51    1.6     7    146.7 =
  1 |           3336    41    1.9     5    147.5 |           3336    41    1.9     5    147.5 =
  2 |           3337    48    1.7    -1    148.4 |           3337    48    1.7    -1    148.4 =
  3 |           3338    52    2.1    -1    148.1 |           3338    52    2.1    -1    148.1 =
  4 |           3339    40    2.3    -1    146.3 |           3339    40    2.3    -1    146.3 =
  5 |           3340    49    1.9    -1    144.1 |           3340    49    1.9    -1    144.1 =
cp  | 自检: id / isFixed / position | OBS 上传(同一个 envelope): 同三项
  0 |             81     1     0 | 打卡点1
  1 |             82     0     1 | 打卡点2
  2 |             83     0     2 | 打卡点5
  3 |             84     0     3 | 打卡点4
  4 |             85     0     4 | 打卡点3
```

- 六个点的 `id` / `count` / `radius` / `type` / `bdA` **逐个 `=`**（不是「同量级」）；
- 必到点数组 `id` 81..85、`isFixed` 只有首点为 1、`position` 0..4 连续——与真机
  `rrid 1328288734` 的形态一致，且自检与上传是同一份；
- `type` 里 `7/5/-1…` 的分布、`type=2` 的打卡标记都在**自检那份**里出现了
  （修复前 probe 传空 `fixed`，自检那份一个标记都没有）。

---

## 6. 验收对照

| 要求 | 状态 |
|---|---|
| 自检校验的就是最终上传的那一份 | ✅ 点位只构造一次（`build_points`），`RunOutcome::submission_points` 一路带到 `detail_payload_rrid(…, &submission.points)` |
| 若必须两次 build，则入参完全一致 + 断言逐字段相同 | ✅ 两次只差 envelope；`run_full` 断言 `submission.points == out.submission_points`；回归测试逐元素逐字段比对 |
| `probe` / `params` 入参差异逐个比对 | ✅ 见 §3：`uid`/`unid`/`uuid`/`signature`/`originalSign`/`geeToken`/`runAreaId`/`fivePointJson` 必然不同，点位数组零差异 |
| 必须重建时只建一次、传引用 | ✅ 点位与 `rrid` 解耦：`rrid` 只进桶 id / objectKey，点位传引用且零复制序列化 |
| 新增回归测试 `selfcheck_and_upload_points_are_identical` | ✅ [`api/flow.rs:2362`](api/flow.rs:2362)，修复前红（§4.1）、修复后绿 |
| 允许差异白名单只含 envelope 级字段 | ✅ `POINT_ALLOWED_DIFFS = []` + 禁止 envelope 白名单混入点位字段名 |
| A1 五条 / P1 一条 / A2 一条不回归 | ✅ §4.2 逐条通过 |
| 吸附顺序（`compress_startup` 之后、`real_len` 之前）不动 | ✅ 未改 |
| `cargo test` 全绿 | ✅ 289 passed / 0 failed / 3 ignored（连跑两次一致） |
| `cargo clippy --all-targets` 零告警 | ✅ |
| 不 commit / 不 push / 不碰 `web/index.html` | ✅ `git status` 里 `web/` 无改动 |

---

## 7. 遗留（不在本次范围）

1. **`Submission::points` 在 `run_full` 里是一次 `clone`**：值完全相同、构造只有一次，
   但严格说仍是「一份数据两个 owner」。要彻底零复制得让 `Submission` 持 `Arc<[Value]>`
   或让 `build_from_points` 收 `&[Value]` 后再由 `run_full` 决定归属——属于结构调整，
   与「点位两份实例」这个缺陷无关，留作后续。
2. `detail_payload`（非 rrid 版）目前只在测试里用，仍然自己拼 `route`；它不参与上传链路。
