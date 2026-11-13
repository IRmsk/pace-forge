# FIX-B：数据目录责任归属 + 测试隔离补齐

范围：只修两件事——① `data_dir()` 去掉 `create_dir_all` 副作用之后，「谁负责建目录」没被明确接管；
② 测试隔离只做了 `api/session.rs`，其余模块仍然直连真实 `$HOME`。
**没有** git commit / push，**没有**碰 `web/index.html`。

开工前 `git diff --stat`（基线）：`api/fence.rs api/flow.rs api/session.rs api/submit.rs track/buckets.rs track/mod.rs`（6 文件，3447 insertions）。
收工后多出来的、属于本任务的：`api/accounts.rs`、`api/client.rs`、`api/policy.rs`、`api/session.rs`、`lib.rs`、`track/roadnet.rs`、`track/tileroad.rs` + 新增 `test_support.rs`；`web/index.html`（+108）与 `docs/fix/FIX-B-PARSE-COORD.md` 是**另一个并行任务**的改动，原样保留未回滚。

---

## 一、缺陷与修法

### 缺陷 1：`data_dir()` 不再建目录，「谁建」没有唯一责任点

`api/session.rs::data_dir()` 早先自带 `create_dir_all`，等于任何一次只读调用都会在真实 `$HOME` 下建目录。
副作用去掉是对的，但建目录的责任散落在各调用点（有的建、有的不建、有的 `let _ =` 吞掉错误），
其中最典型的是 `api/policy.rs` 的 `PACE_DUMP_POLICY` 转储：用 `fs::write` 直接写，
**目标目录不存在时静默失败**（`let _ =` 吞掉），文件根本没落盘，还查不出原因。

修法：把「建目录 + 0600 写文件」收敛成**全项目唯一实现**，放在 `api/session.rs`
（路径与落盘原语本来就归这里）：

| 新增/改写的原语 | 作用 |
| --- | --- |
| `ensure_dir(dir) -> Result<(), String>` | `create_dir_all` 的显式责任点，错误带路径，幂等 |
| `write_file_private(path, data) -> Result<(), String>` | **自动建父目录**（多级，覆盖 `tiles/`） + `File::create` + `set_permissions(0600)` + 写入；不再吃 umask |
| `write_json_below`（改写） | 改成 `serde` + `write_file_private`，不再自己拼 create_dir/权限 |

然后**每条写盘路径全部改走 `write_file_private`**（清单见下节）。`data_dir()` / `data_dir_in()` 保持
「纯路径计算、不碰磁盘」，并在文档里写明建目录的责任点就是 `write_file_private`。

逐条核查结果（`~/.pace-forge/` 与同级文件）：

| 写盘路径 | 位置 | 之前建目录？ | 现在 |
| --- | --- | --- | --- |
| `session.json`（`save_session`） | `api/session.rs` | 是（`create_dir_all` 吞错） | ✅ `write_json_below` → `write_file_private`，错误上抛 |
| `session.json`（`load_session`） | `api/session.rs` | 读不建（正确） | ✅ 保持：读路径不建目录 |
| `accounts.json`（`save`） | `api/accounts.rs` | 是（另写一份 `write_private`） | ✅ `ensure_dir` + `write_file_private`，删掉重复实现；仍先写 `.json.tmp` 再 `rename` |
| `accounts.json`（`load`） | `api/accounts.rs` | 读不建（正确） | ✅ 保持 |
| `.pace-forge-device-id*`（`persistent_device_id`） | `api/client.rs` | 是（`let _ =` 吞错，且用 `fs::write` → 吃 umask） | ✅ `write_file_private`（建父目录 + 0600）。注意它是**数据目录的同级文件**，不在 `.pace-forge/` 内，路径布局不变 |
| `roads-*.json` / `blockers.json` / `buildings-*.json` / `water-*.json` | `track/roadnet.rs` | 是（三处 `let _ =` 吞错） | ✅ 全部走 `write_file_private`；`save_local_blockers` 的错误照旧上抛 |
| `tiles/<z>_<x>_<y>.png` | `track/tileroad.rs` | 是（仅新拉到的瓦片） | ✅ `write_file_private`（多级目录一次建成） |
| `policy-runmode<N>.json`（`PACE_DUMP_POLICY`） | `api/policy.rs` | **否**（`fs::write` 直写，目录不存在即静默失败） | ✅ 修好了：`write_file_private` 建目录 + 0600 |

### 缺陷 2：测试隔离只覆盖了 `session.rs`

`api/accounts.rs` / `api/client.rs` / `track/roadnet.rs` / `track/tileroad.rs` / `api/policy.rs`
原来全部自己 `std::env::var("HOME")` 拼路径。后果不只是「可能写脏 `$HOME`」：
`api/client.rs` 的三条测试都在调 `DeviceProfile::default()`，**每条都会读真实
`$HOME/.pace-forge-device-id`；文件不存在时还会就地写一个**。`cargo test` 在干净机器上
等于凭空生成设备标识 → 服务端把真机判成「另一台设备」→ `10121` 锁号一小时。

---

## 二、逐个模块的注入改造清单（公共签名全部保持不变，内部转调注入版）

统一风格：`fn f(args) { f_in(&home_dir(), args) }` + `fn f_in(base: &Path, args)`，
另给纯路径函数 `*_path_in(base, …)`（不碰磁盘）。`home_dir()` 仍是
`api/session.rs` 里唯一的 `$HOME` 读取点。

### 1) `api/session.rs`（缺陷 1 主要落点）
- 新增 `ensure_dir`、`write_file_private`（`pub(crate)`，全项目共用）
- `write_json_below` 改写成 `serde` + `write_file_private`（不再自带权限/建目录逻辑）
- 新增测试 `write_creates_nested_dirs_on_first_use`：多级目录（`<data>/tiles/`）一次建成 + 文件 0600
- 既有 `data_dir_in` / `load_session_below` / `save_session_below` / `clear_session_below`、既有 6 条测试**未改动**

### 2) `api/accounts.rs`
| 生产签名（不变） | 注入版 | 纯路径 |
| --- | --- | --- |
| `load() -> Book` | `load_in(base)` | `book_path_in(base)` |
| `save(&Book)` | `save_in(base, &Book)` | — |
| `clear_tokens()` | `clear_tokens_in(base)` | — |

- 删掉自己拼 `.pace-forge` 的私有 `data_dir()` 与重复的 `write_private()`，改用 `session::{data_dir_in, home_dir, ensure_dir, write_file_private}`
- 测试改造：`password_roundtrip` / `mask_hides_middle` / `upsert_…` / `ready_…` / `since_submit_…` / `book_roundtrip_json` 保持（纯内存）；
  新增 6 条走 `TestDir`：`book_roundtrip_in_isolated_dir`、`save_creates_data_dir_on_first_write`、
  `missing_book_is_empty_and_creates_nothing`、`clear_tokens_works_on_isolated_dir`、
  `accounts_paths_are_isolated_from_real_home`、`default_book_path_matches_production_layout`

### 3) `api/client.rs`
| 生产签名（不变） | 注入版 | 纯路径 |
| --- | --- | --- |
| `DeviceProfile::for_account(scope)` | `DeviceProfile::for_account_in(base, scope)` | — |
| `impl Default for DeviceProfile` | `DeviceProfile::default_in(base)` | — |
| （私有 `persistent_device_id`，已无生产调用方，删除） | `persistent_device_id_in(base, scope)` | `device_id_path_in(base, scope)` |

- 写盘改走 `session::write_file_private`（0600 + 建父目录）
- 测试改造：`device_id_is_32_hex`、`headers_include_signature_and_envelope`、`headers_carry_credentials_when_set`
  三条原来调 `DeviceProfile::default()`，全部改成注入 `TestDir`；新增 `device_id_is_persisted_in_isolated_dir`（含 0600、同 scope 稳定、不同 scope 不同文件）、
  `device_id_path_is_isolated_from_real_home`（护栏）

### 4) `track/roadnet.rs`
| 生产签名（不变） | 注入版 | 纯路径 |
| --- | --- | --- |
| `load_local_blockers()` | `load_local_blockers_in(base)` | `blockers_path_in(base)` |
| `save_local_blockers(&polys)` | `save_local_blockers_in(base, &polys)` | — |
| （内部）`cache_get/put` | `cache_get_in/put_in` | `cache_path_in` |
| （内部）`poly_cache_get/put` | `poly_cache_get_in/put_in` | `poly_cache_path_in` |
| （内部）`water_cache_get/put` | `water_cache_get_in/put_in` | `water_cache_path_in` |

- `fetch_roadnet` / `fetch_water` / `fetch_buildings` 等网络入口公共签名不变，内部走 `_in(&home_dir(), …)`
- 删掉三处 `let _ = create_dir_all` + `fs::write`，统一 `write_file_private`
- 新增 4 条测试：`roadnet_cache_roundtrip_in_isolated_dir`（含首次建目录）、`blockers_roundtrip_in_isolated_dir`、
  `poly_and_water_cache_roundtrip_in_isolated_dir`、`roadnet_cache_paths_are_isolated_from_real_home`（护栏，四类路径全覆盖）

### 5) `track/tileroad.rs`
| 生产签名（不变） | 注入版 | 纯路径 |
| --- | --- | --- |
| （内部）`tile_path(x, y)` | `tile_path_in(base, x, y)` | `cache_dir_in(base)` |

- `fetch_tile` 的落盘改成 `session::write_file_private`；`cache_dir()` 这种「带读 HOME 的私有函数」删除，只留 `*_in`
- 新增 2 条测试：`tile_cache_dir_is_isolated_from_real_home`（护栏）、`tile_path_layout_is_stable_and_injected`（文件名布局 `<z>_<x>_<y>.png` 不能变，否则老缓存全失效）

### 6) `api/policy.rs`
| 生产签名 | 注入版 | 纯路径 |
| --- | --- | --- |
| `dump_dir() -> Option<PathBuf>`（新，语义：默认 `None` = 不落盘） | `dump_policy_in(Option<&Path>, run_mode, &Value)` | `dump_policy_path_in(dir, run_mode)` |

- `fetch_policy_for` 公共签名不变；原来内联的 `if let Ok(dir) = env::var("PACE_DUMP_POLICY") { fs::write }` 换成 `dump_policy_in(dump_dir().as_deref(), …)`
- 转储**默认关闭**（策略响应含考生身份字段，不该无声堆进 `$HOME`），且写时建目录 + 0600
- 新增 2 条测试：`policy_dump_path_is_isolated_from_real_home`（护栏，同时锁住「默认不落盘」）、`dump_policy_writes_only_with_injected_dir`

### 7) 新增 `test_support.rs`（`#[cfg(test)]`，不进生产二进制）
- `TestDir`：`$TMPDIR/pace-forge-test-<tag>-<pid>-<seq>-<nanos>`，`new/home()/data()`，`Drop` 整棵删
- `real_home()` / `real_data_dir()`：护栏对照用，**只算路径不碰磁盘**
- `assert_isolated(injected, what)`：一条断言覆盖「不等于真实数据目录 / 不落在其下 / 在临时目录下 / 带一次性前缀」
- `lib.rs` 里以 `#[cfg(test)] pub(crate) mod test_support;` 引入
- 刻意**不用** `std::env::set_var("HOME", …)`：测试并行执行，改全局变量等于「谁先改谁赢」

---

## 三、首次运行验证（真做了 `~/.pace-forge` 改名）

### 步骤（含数据保护）
1. `cp -a ~/.pace-forge /tmp/pf-home-backup/dot-pace-forge`，并 `cp -a ~/.pace-forge-device-id* /tmp/pf-home-backup/`；备份共 161 个文件（8 个 json/device-id + 153 张瓦片），逐个 `md5sum` 存档 `/tmp/pf-backup-md5.txt`
2. `mv ~/.pace-forge ~/.pace-forge-moved/dot-pace-forge` + `mv ~/.pace-forge-device-id* ~/.pace-forge-moved/` → `~/` 下已无任何 pace-forge 痕迹（模拟首次运行）
3. 用**生产默认路径**（不带 `_in` 注入）的探针跑真实写盘：`session::save_session` / `accounts::save` /
   `track::roadnet::save_local_blockers` / `DeviceProfile::default()` / `DeviceProfile::for_account(手机号)`，
   每条都 `load`/读回校验
4. 观察目录与权限，然后删掉探针产物、`mv` 原目录回来
5. 还原后逐文件比对 md5

### 结果（探针输出，原样）
```
HOME = /root
data_dir() = /root/.pace-forge  (exists=false)
[1] session.json       exists=true  mode=600 /root/.pace-forge/session.json
[2] accounts.json      exists=true  mode=600 /root/.pace-forge/accounts.json
[3] blockers.json      exists=true  mode=600 /root/.pace-forge/blockers.json
[4] device-id          exists=true  mode=600 /root/.pace-forge-device-id
[5] device-id-<scope>  exists=true  mode=600 /root/.pace-forge-device-id-359ea74a
data dir mode = 700

FIRST-RUN OK：目录被正确创建，5 个文件全部 0600，读写往返一致
```

**结论：目录由写盘方自动创建（`0700`），5 个文件全部 `0600`，读写往返一致。**
`roads-/water-/buildings-` 缓存与 `tiles/` 走的是同一个 `write_file_private`，
其「目录不存在时自建」由注入版测试覆盖：
`roadnet_cache_roundtrip_in_isolated_dir`（断言写前 `.pace-forge` 不存在、写后存在）、
`poly_and_water_cache_roundtrip_in_isolated_dir`、`write_creates_nested_dirs_on_first_use`（多级 `tiles/`）。

### 还原校验
```
counts: backup=161 restored=161
diff /tmp/a.txt /tmp/b.txt   →  空
```
`session.json`（262B / Nov 13 10:54）、`accounts.json`（377B / Nov 13 10:56）、3 个 `.pace-forge-device-id*`、
`tiles/` 153 张瓦片，哈希与 mtime 全部一致 —— **现有登录态零丢失、零改动**。

---

## 四、三遍 `cargo test` 的 `$HOME` 零变化证据

按任务给的脚本原样执行：

```
$ ls -la ~/ | grep pace-forge > /tmp/b1.txt
$ md5sum ~/.pace-forge/*.json ~/.pace-forge-device-id* 2>/dev/null >> /tmp/b1.txt
$ # 跑三遍 cargo test
$ ls -la ~/ | grep pace-forge > /tmp/b2.txt
$ md5sum ~/.pace-forge/*.json ~/.pace-forge-device-id* 2>/dev/null >> /tmp/b2.txt
$ diff /tmp/b1.txt /tmp/b2.txt
（空）
```

b1 内容（b2 与之一字不差）：
```
drwx------.  3 root root  3440 Nov 13 12:59 .pace-forge
-rw-------.  1 root root    32 Nov  6 07:31 .pace-forge-device-id
-rw-------.  1 root root    32 Nov  6 15:24 .pace-forge-device-id-80d1c979
-rw-------.  1 root root    32 Nov  6 10:42 .pace-forge-device-id-9e0dc928
drwx------. 11 root root  3440 Nov 11 03:26 pace-forge
ea579eaadaa7dad434b56e89541ec8ed  /root/.pace-forge/accounts.json
9c7e01b0d0fca91ce52fffb6b5bee009  /root/.pace-forge/blockers.json
9c7e01b0d0fca91ce52fffb6b5bee009  /root/.pace-forge/buildings-47.379_123.951_47.389_123.964.json
c749730801cf144fd208bda6f0cba12f  /root/.pace-forge/roads-47.379_123.950_47.390_123.964.json
c749730801cf144fd208bda6f0cba12f  /root/.pace-forge/roads-47.379_123.951_47.389_123.963.json
23977d7fcfeb742040f31c1a6e1ada3d  /root/.pace-forge/session.json
9c7e01b0d0fca91ce52fffb6b5bee009  /root/.pace-forge/water-47.379_123.951_47.389_123.963.json
9c7e01b0d0fca91ce52fffb6b5bee009  /root/.pace-forge/water-47.379_123.951_47.389_123.964.json
dbc6b741d5c3441a867c5678118cfc1a  /root/.pace-forge-device-id
ff5d5e704abd8e09e1089a3246312fab  /root/.pace-forge-device-id-80d1c979
fe5fe3ccc9f7218b5af3d0ec2153a582  /root/.pace-forge-device-id-9e0dc928
```

三轮结果：
```
第 1 遍: test result: ok. 306 passed; 0 failed; 3 ignored
第 2 遍: test result: ok. 306 passed; 0 failed; 3 ignored
第 3 遍: test result: ok. 306 passed; 0 failed; 3 ignored
diff /tmp/b1.txt /tmp/b2.txt → 空  ✅ 三遍 cargo test 后 $HOME 下零变化
```

改完最后一处文档注释后又复核了一次（b3/b4）：`diff` 仍为空。

---

## 五、护栏测试清单（每个模块一条，模式推广自 `session::tests::test_data_dir_is_isolated_from_real_home`）

| 模块 | 护栏测试 | 断言内容 |
| --- | --- | --- |
| `api/session.rs` | `test_data_dir_is_isolated_from_real_home`（原有，未改） | 注入数据目录 ≠ 真实 `$HOME/.pace-forge`、不在其下、在临时目录、带一次性前缀 |
| `api/accounts.rs` | `accounts_paths_are_isolated_from_real_home` | `book_path_in(TestDir)` 走 `assert_isolated`；且 ≠ `book_path_in(real_home)` |
| `api/accounts.rs` | `default_book_path_matches_production_layout` | 生产默认路径仍是 `$HOME/.pace-forge/accounts.json`（布局不许变） |
| `api/client.rs` | `device_id_path_is_isolated_from_real_home` | 注入基目录走 `assert_isolated`；`device_id_path_in` ≠ 生产路径、基目录 = 注入 `$HOME`、文件名仍是 `.pace-forge-device-id` |
| `track/roadnet.rs` | `roadnet_cache_paths_are_isolated_from_real_home` | 四类路径（roads/blockers/buildings/water）逐个：`assert_isolated` + ≠ 生产路径 + 不在真实数据目录下 |
| `track/tileroad.rs` | `tile_cache_dir_is_isolated_from_real_home` | `cache_dir_in` 走 `assert_isolated`；目录名 `tiles`、父目录 `.pace-forge` |
| `api/policy.rs` | `policy_dump_path_is_isolated_from_real_home` | 转储注入路径走 `assert_isolated`；≠ 真实数据目录下的路径；**未设 `PACE_DUMP_POLICY` 时 `dump_dir()` 必须为 `None`**（默认不落盘） |

配套的「会写盘就先进隔离目录」测试（非护栏，但同样是防回归）：
`data_dir_is_created_on_first_write`、`write_creates_nested_dirs_on_first_use`、
`book_roundtrip_in_isolated_dir`、`save_creates_data_dir_on_first_write`、`missing_book_is_empty_and_creates_nothing`、
`clear_tokens_works_on_isolated_dir`、`device_id_is_persisted_in_isolated_dir`、
`roadnet_cache_roundtrip_in_isolated_dir`、`blockers_roundtrip_in_isolated_dir`、
`poly_and_water_cache_roundtrip_in_isolated_dir`、`tile_path_layout_is_stable_and_injected`、
`dump_policy_writes_only_with_injected_dir`。

---

## 六、验收结果

| 项 | 命令 | 结果 |
| --- | --- | --- |
| 全量测试 | `PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR=/tmp/pace-target cargo test` | ✅ **306 passed / 0 failed / 3 ignored** |
| 全量测试（独立目录复跑） | `CARGO_TARGET_DIR=/tmp/pt-b1 cargo test` | ✅ 306 / 0 / 3 |
| clippy | `CARGO_TARGET_DIR=/tmp/pace-target cargo clippy --all-targets` | ✅ 零告警 |
| A4 单跑 | `cargo test --lib single_point_source` | ✅ 2 passed（`selfcheck_and_upload_points_are_identical` + `print_selfcheck_vs_upload_first_six_points`） |

`#[test]` 计数：源码 **309 条**（基线 292 + 17 新增），`#[ignore]` 3 条 → 306 跑 / 3 忽略。

**CARGO_TARGET_DIR 说明**：三遍 `$HOME` 零变化验证用 `/tmp/pt-b1`（独立目录，避免与并行任务的
`/tmp/pt-sample`、共享 `/tmp/pace-target` 抢锁）；验收命令按原样用 `/tmp/pace-target` 也跑通了一次，
**没有出现** `Blocking waiting for file lock`。

### 不许回归清单（逐条确认通过）
```
api::flow::single_point_source_tests::selfcheck_and_upload_points_are_identical      ... ok   (A4)
api::submit::parity_regression_tests::startup_window_and_segment_distance_sum        ... ok   (P1)
api::submit::parity_regression_tests::point_speed_fields_match_real_device_point_by_point ... ok (A2)
track::checkpoint_hit_tests::p1_invariants_hold_with_checkpoints                     ... ok
track::checkpoint_hit_tests::snap_only_moves_sampling_error                          ... ok
track::checkpoint_hit_tests::checkpoint_hit_after_compress                           ... ok
track::checkpoint_hit_tests::checkpoint_hit_sweep_planner_branch                     ... ok
track::checkpoint_hit_tests::checkpoint_hit_sweep_no_block                           ... ok   (A1 全部)
track::queue_num_tests::bucket_queue_num_matches_real_device_element_by_element      ... ok
track::queue_num_tests::no_linear_interpolation_drift                                ... ok
track::queue_num_tests::point_queue_num_matches_real_device_element_by_element       ... ok
track::queue_num_tests::boundaries                                                   ... ok   (A5 全部)
```

---

## 七、改动文件与函数一览

| 文件 | 改动 |
| --- | --- |
| `api/session.rs` | +`ensure_dir`、+`write_file_private`；`write_json_below` 改写为走它；`data_dir()` 文档写明建目录责任点；+测试 `write_creates_nested_dirs_on_first_use` |
| `api/accounts.rs` | +`book_path_in` / `load_in` / `save_in` / `clear_tokens_in`；`load`/`save`/`clear_tokens` 转调；删私有 `data_dir`、`book_path`、`write_private`；+6 测试 |
| `api/client.rs` | +`DeviceProfile::for_account_in` / `default_in`、`device_id_path_in` / `persistent_device_id_in`；`for_account`/`Default` 转调；删无调用方的 `persistent_device_id`；写盘走 `write_file_private`；3 条既有测试改注入 +2 新测试 |
| `api/policy.rs` | +`dump_dir` / `dump_policy_path_in` / `dump_policy_in`；`fetch_policy_for` 内的 `fs::write` 换成 `dump_policy_in`；+2 测试 |
| `track/roadnet.rs` | 四类路径 `*_path_in` + `cache_*_in` / `poly_cache_*_in` / `water_cache_*_in` / `load_local_blockers_in` / `save_local_blockers_in`；生产函数转调；删三处 `create_dir_all`+`fs::write`；+4 测试 |
| `track/tileroad.rs` | `cache_dir_in` / `tile_path_in`（`tile_path` 转调）；瓦片落盘走 `write_file_private`；删私有 `cache_dir`；+2 测试 |
| `lib.rs` | +`#[cfg(test)] pub(crate) mod test_support;` |
| `test_support.rs`（新） | `TestDir` / `real_home` / `real_data_dir` / `assert_isolated` |

未触碰：`web/mod.rs`、`main.rs`、`web/index.html`（并行任务在改）、`api/{fence,flow,submit}.rs`、`track/{buckets,mod}.rs` 等。
`web/mod.rs` / `main.rs` 调用的 `accounts::load/save/clear_tokens`、`session::*`、`DeviceProfile::default/for_account`
公共签名全部未变，编译与行为均不受影响。
