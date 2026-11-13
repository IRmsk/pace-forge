# 测试隔离缺陷修复：`cargo test` 会删掉真实登录态

一份只讲一件事的修复记录：`api/session.rs` 的单元测试直接操作真实
`$HOME/.pace-forge/`，跑一次 `cargo test` 就能把本机登录态删掉。

---

## 1. 缺陷

`api/session.rs` 的三个测试原先都通过「全局读 `$HOME`」的路径落盘：

| 测试 | 原行为 | 后果 |
| --- | --- | --- |
| `missing_session_is_empty_not_error` | 调 `clear_session()` → 删 `$HOME/.pace-forge/session.json` | **删除真实登录态** |
| `session_roundtrip` | `write_json("session_test.json", …)` | 在真实 `$HOME/.pace-forge/` 里写文件 |
| `data_dir_is_created` | 调 `data_dir()`，而它内部 `create_dir_all` | 在真实 `$HOME` 下建目录（副作用在纯只读调用上） |

根因是「数据目录」只有一种取法：运行时从 `std::env::var("HOME")` 全局读，
测试没有任何注入点，所以测试和真实用户数据用的是同一个目录。

原始代码（修复前）：

```rust
/// 数据目录：`$HOME/.pace-forge/`。
pub fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home).join(".pace-forge");
    let _ = std::fs::create_dir_all(&dir);   // 只读调用也建目录
    dir
}

fn write_json<T: Serialize>(name: &str, value: &T) -> Result<(), String> {
    let path = data_dir().join(name);        // 测试写的就是真实目录
    ...
}

pub fn clear_session() {
    let _ = std::fs::remove_file(data_dir().join("session.json"));  // 删真实会话
}
```

```rust
#[test]
fn missing_session_is_empty_not_error() {
    clear_session();          // ← 删的是真实 $HOME/.pace-forge/session.json
    let s = load_session();
    assert!(s.token.is_empty());
    assert!(!s.is_valid());
}
```

## 2. 影响：不只是「本地未登录」，会锁号一小时

`session.json` 一没，CLI 立刻判定未登录（`main.rs` 里读不到会话就直接
`未登录：先通过 Web 控制台登录一次`）。但**服务端会话仍然活着**——本机已经
拿不出 `token`/`uid` 去调登出接口，于是「本地以为没登录、服务端以为还登录着」：

- 重新登录时服务端仍认为旧会话占用该账号，返回
  `10121 此账号已在其他设备登录，请过一小时后登录或原设备退出`；
- 旧会话凭证已随文件一起被删，原设备也退不出来；
- 结果就是**账号被锁一小时**，且这一小时只能干等。

触发条件极低：任何一次 `cargo test`（CI、本地、随手跑一遍单测）都会命中。

### 现场复现（修复前实测）

```
$ ls -la ~/.pace-forge/ > /tmp/base_before.txt && md5sum ~/.pace-forge/*.json >> /tmp/base_before.txt
$ PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR=/tmp/pace-target cargo test
test result: ok. 274 passed; 0 failed; 3 ignored
$ ls -la ~/.pace-forge/ > /tmp/base_after.txt && md5sum ~/.pace-forge/*.json >> /tmp/base_after.txt
$ diff /tmp/base_before.txt /tmp/base_after.txt
1,2c1,2
< total 55
< drwx------.  3 root root  3440 Nov 13 10:18 .
---
> total 51
> drwx------.  3 root root  3440 Nov 13 10:20 .
9d8
< -rw-------.  1 root root   208 Nov 13 10:18 session.json
18d16
< c2f7eb8de7baea3a9959d7fd9b2f0078  /root/.pace-forge/session.json
```

`session.json` 被删掉了 —— 这就是本次事故的直接证据。修复验证前已从
`/tmp/pace-real-backup/session.json` 还原成同一份（md5 `c2f7eb8d…` 一致），
**没有修改其内容、没有尝试登录、没有动 `accounts.json`**。

## 3. 修法：数据目录改成可注入，测试注入一次性临时目录

只改 `api/session.rs` 一个文件。

### 3.1 `data_dir_in(base: &Path)`：纯路径计算，无 IO

```rust
pub const DATA_DIR_NAME: &str = ".pace-forge";

pub(crate) fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// 给定 HOME 基目录，拼出数据目录：`<base>/.pace-forge`。纯路径计算，不碰磁盘。
pub fn data_dir_in(base: &Path) -> PathBuf {
    base.join(DATA_DIR_NAME)
}

/// 数据目录：`$HOME/.pace-forge/`。只算路径，不做 IO。
pub fn data_dir() -> PathBuf {
    data_dir_in(&home_dir())
}
```

顺带把 `data_dir()` 里那个 `create_dir_all` 拿掉了：它对**只读调用**也有副作用，
正是 `data_dir_is_created` 那个测试污染真实 HOME 的来源。建目录的责任回到真正
写盘的两处（`write_json_below` 与 `accounts::save`），二者本来就各自
`create_dir_all`，行为不变。

### 3.2 每个落盘函数多一个「目录参数」版本

| 公共 API（行为不变，仍读 `$HOME`） | 注入版（测试用） |
| --- | --- |
| `data_dir()` | `data_dir_in(base)` |
| `load_session()` | `load_session_below(dir)` |
| `save_session(&s)` | `save_session_below(dir, &s)` |
| `clear_session()` | `clear_session_below(dir)` |
| `read_json(name)` / `write_json(name, &v)` | `read_json_below(dir, name)` / `write_json_below(dir, name, &v)` |

公共 API 一行逻辑都没变，只是转调注入版；`main.rs` / `web/mod.rs` / `api/login.rs`
的调用点不需要改（编译通过、clippy 零告警即证）。

### 3.3 测试侧：`TestDir` 一次性隔离目录 + `Drop` 清理

```rust
struct TestDir { root: PathBuf }

impl TestDir {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "pace-forge-test-{tag}-{}-{n}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&root).expect("建临时基目录失败");
        Self { root }
    }
    fn home(&self) -> &Path { &self.root }
    fn data(&self) -> PathBuf { data_dir_in(self.home()) }   // 隔离环境里的 $HOME/.pace-forge
}

impl Drop for TestDir {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.root); }
}
```

目录名带 `tag + pid + 全局自增序号 + 纳秒`，四个维度保证并行测试互不重叠。
**刻意不用 `std::env::set_var("HOME", …)`**：测试并行跑，改全局环境变量是
「谁先改谁赢」，不同测试会互相串（而且 `set_var` 在多线程下本身就是 UB 边界）。
注入目录是纯函数式参数传递，没有共享可变状态。

`missing_session_is_empty_not_error` 保留原意，只是把 `clear_session()` 换成
`clear_session_below(&t.data())`：先写入一个真会话、再清掉、再断言读到空会话；
额外补一个「从没写过会话的目录」也返回空会话的分支。

## 4. 新增回归断言

`test_data_dir_is_isolated_from_real_home`（`api/session.rs:264`）：

```rust
let test_dir = t.data();
let real_dir = data_dir_in(&home_dir());   // 与 data_dir() 同值，但不创建目录、不读文件

assert_ne!(test_dir, real_dir, "测试数据目录不能等于真实 $HOME/.pace-forge");
assert!(!test_dir.starts_with(&real_dir), "…不能落在真实 $HOME/.pace-forge 之下");
assert!(test_dir.starts_with(std::env::temp_dir()), "…应在临时目录下");
assert!(test_dir.to_string_lossy().contains("pace-forge-test-"), "…应带一次性前缀");
```

谁哪天把 `*_below` 改回 `load_session()` / `clear_session()`，这四条会立刻炸。
（用 `data_dir_in(&home_dir())` 而不是 `data_dir()`，是为了让这条护栏自己不产生
「建真实目录」的副作用。）

另外 `data_dir_is_created_on_first_write` 顶替了原来的 `data_dir_is_created`，
断言意图不变但拆成两半：`data_dir_in` 不该有 IO 副作用；首次落盘才建目录。

## 5. 三遍跑的隔离证据

```
$ ls -la ~/.pace-forge/ > /tmp/before.txt && md5sum ~/.pace-forge/*.json >> /tmp/before.txt
$ for i in 1 2 3; do cargo test; done          # PATH=… CARGO_TARGET_DIR=/tmp/pace-target
--- run 1 --- test result: ok. 276 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 0.59s
--- run 2 --- test result: ok. 276 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 0.76s
--- run 3 --- test result: ok. 276 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 0.74s
$ ls -la ~/.pace-forge/ > /tmp/after.txt && md5sum ~/.pace-forge/*.json >> /tmp/after.txt
$ diff /tmp/before.txt /tmp/after.txt
$ echo $?
0
```

**diff 输出为空，退出码 0。** 三遍都跑完之后，`~/.pace-forge/` 的文件名、
大小、权限、mtime、md5 全部一字未变（`session.json` 仍是
`c2f7eb8de7baea3a9959d7fd9b2f0078`，208 字节，10:18）。

```
$ ls -d /tmp/pace-forge-test-* 2>/dev/null | wc -l
0
```

临时目录也被 `Drop` 清干净了，不留垃圾。

完整快照（三遍跑之前 / 之后完全相同）：

```
total 55
drwx------.  3 root root  3440 Nov 13 10:20 .
drwx------. 29 root root  3440 Nov 13 10:22 ..
-rw-------.  1 root root   377 Nov 13 09:55 accounts.json
-rw-------.  1 root root    12 Nov 10 15:10 blockers.json
-rw-------.  1 root root    12 Nov 10 15:12 buildings-47.379_123.951_47.389_123.964.json
-rw-------.  1 root root   916 Nov  7 05:40 roads-47.379_123.950_47.390_123.964.json
-rw-------.  1 root root   916 Nov  6 14:54 roads-47.379_123.951_47.389_123.963.json
-rw-------.  1 root root   208 Nov 13 10:18 session.json
drwx------.  2 root root 20480 Nov 12 04:22 tiles
-rw-------.  1 root root    12 Nov  6 14:59 water-47.379_123.951_47.389_123.963.json
-rw-------.  1 root root    12 Nov 10 15:12 water-47.379_123.951_47.389_123.964.json
3db3d29e274288a87ea0299378e069cb  /root/.pace-forge/accounts.json
9c7e01b0d0fca91ce52fffb6b5bee009  /root/.pace-forge/blockers.json
9c7e01b0d0fca91ce52fffb6b5bee009  /root/.pace-forge/buildings-47.379_123.951_47.389_123.964.json
c749730801cf144fd208bda6f0cba12f  /root/.pace-forge/roads-47.379_123.950_47.390_123.964.json
c749730801cf144fd208bda6f0cba12f  /root/.pace-forge/roads-47.379_123.951_47.389_123.963.json
c2f7eb8de7baea3a9959d7fd9b2f0078  /root/.pace-forge/session.json
9c7e01b0d0fca91ce52fffb6b5bee009  /root/.pace-forge/water-47.379_123.951_47.389_123.963.json
9c7e01b0d0fca91ce52fffb6b5bee009  /root/.pace-forge/water-47.379_123.951_47.389_123.964.json
```

## 6. 验证

```
$ PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR=/tmp/pace-target cargo test
test result: ok. 276 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out
（baseline 274 passed / 3 ignored → 净增 2 条：json_roundtrip_in_isolated_dir、test_data_dir_is_isolated_from_real_home）

$ CARGO_TARGET_DIR=/tmp/pace-target cargo clippy --all-targets -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 15.17s     # 零告警
```

## 7. 排查结论：同类风险还有哪些

对整个仓库的持久化调用点做了一遍排查（`grep` 全部 `#[cfg(test)]` 模块 +
`data_dir` / `book_path` / `session` / `accounts::load|save` 调用点）：

| 位置 | 是否被测试触碰 | 处理 |
| --- | --- | --- |
| `session::load_session/save_session/clear_session/read_json/write_json` | 是（就是本 bug） | **已修** |
| `accounts::load/save/book_path` | 否 —— `accounts.rs` 的 7 个测试全是纯内存（`Book`/`Account` 结构、`seal_password`/`open_password`、`mask_phone`、`upsert`、`ready`、`since_submit_sec`、JSON 往返），没有一个调 `load()`/`save()` | 不需要改 |
| `session::data_dir()` 内的 `create_dir_all` | 是（`data_dir_is_created`） | **已修**：`data_dir()` 改为纯路径，建目录下移到写盘处 |
| `track::roadnet` / `track::tileroad` 缓存（`~/.pace-forge/{roads,water,buildings,blockers}.json`、`~/.pace-forge/tiles/`） | 否 —— 写盘只发生在 `fetch_roadnet` / `fetch_tile` 这类网络路径里，相关测试全部 `#[ignore]`（`live_campus_roadnet`、`live_closing_leg_not_backtracking`、`live_soften_shape`） | 不需要改（默认 `cargo test` 不联网、不写盘，三遍 diff 已证） |
| `api/client.rs::persistent_device_id` | 是 —— `DeviceProfile::default()` 会写 `$HOME/.pace-forge-device-id*` | **不在本 bug 范围**：路径在 `$HOME` 下但**不在 `~/.pace-forge/` 内**，且是「读不到才写一次」的幂等行为，文件早已存在（`/root/.pace-forge-device-id` 等 3 个，均为 11 月 6 日）。改它属于另一件事（设备指纹注入），本次不动，仅在此记录 |

## 8. 改动清单

- `api/session.rs`（唯一改动的文件）
  - 新增 `home_dir()`（`pub(crate)`，纯读环境变量）、`data_dir_in(base: &Path)`（`pub`，纯路径计算）、`DATA_DIR_NAME` 常量
  - `data_dir()`：去掉 `create_dir_all` 副作用，改为 `data_dir_in(&home_dir())`
  - `read_json` / `write_json` → `read_json_below(dir, …)` / `write_json_below(dir, …)`
  - `load_session` / `save_session` / `clear_session` 保留原签名，转调新增的 `pub(crate) load_session_below` / `save_session_below` / `clear_session_below`
  - 测试模块：新增 `TestDir`（临时目录 + `Drop` 清理）；`session_roundtrip`、`missing_session_is_empty_not_error`、`data_dir_is_created`（→ `data_dir_is_created_on_first_write`）改用隔离目录；新增 `json_roundtrip_in_isolated_dir` 与回归护栏 `test_data_dir_is_isolated_from_real_home`
- 未改 `web/index.html`、未改 `accounts.json` 内容、未改其他源文件、未 git commit / push
- （`git status` 里 `api/fence.rs`、`api/flow.rs`、`api/submit.rs`、`track/buckets.rs`、`track/mod.rs`、`web/index.html` 以及若干 `.md` 的改动是本仓库既有的未提交改动，不是本次产物。）
