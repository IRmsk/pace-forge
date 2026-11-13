# 必到点位（fixed_point_json / fivePointJson）与真机逐字段对齐

对拍样本：`/sdcard/workspace/re/zjwh740/evidence/obs_real_20261002.json`
（rrid 1328288734，键 `fixed_point_json` 已解析成对象，内层 `fivePointJson`
是字符串化数组）。

我们的改前产物：`/tmp/n2_obs.json`（rrid 1364081873，gzip+base64，解出来的
`fixed_point_json` 里五个点的 `position` = 0/71/88/170/51）。

**没有执行 commit / push，也没有改动 `web/index.html`。**

---

## 一、改前 / 改后一句话

改前：`position` 填的是「这个必到点落在轨迹点数组里的第几个采样点」，
`isFixed` 原样回显服务端值（落到了第 4 个点上），`id` 从 1 起，
`runAreaId` 发真实围栏 id（1668），`geoFencesJson` 只有 `points` 一个字段。

App 详情页的「必到点位」是拿这份报文列表画的：`position` 既不连续也不递增
（最大 170，而总共只有 5 个点），按它排序/取下标就只显示得出一部分点位。

改后：`position` = `0..N-1` 连续序号，数组按轨迹实际途经顺序排列
（数组下标 == position），`isFixed` 只有首点为 1，`id` 从 81 起连续，
`flag` 全场同一个起始戳，`runAreaId` = -1，`geoFencesJson` 补齐
`id` / `name` / `distance`。

---

## 二、改了哪些函数

### `api/submit.rs`

| 符号 | 行（改后） | 改动 |
| --- | --- | --- |
| `FixedPoint` | 101 | **删掉 `position` / `is_fixed` 两个字段**：序号类字段一律不再由调用方填，只留「是哪个点」（两套坐标 + 名字）。这是那次 bug 的根因（调用方按轨迹下标填 position），把字段删掉才能防止它再回来 |
| `FIXED_POINT_ID_BASE` | 115 | 新增，`= 81`。真机 id 是 81~85 |
| `FIXED_POINT_HIT_M` | 121 | 新增，`= 10.0`。判定「轨迹经过这个必到点」的半径，与 `flow` 的点位顺序自检同一阈值 |
| `checkpoint_track_indexes` | 128 | 新增。必到点 → 轨迹里的**首次经过下标**（内部量，`allLocJson` 的 `type=2` 标记靠它）。这就是改前被误当成 `position` 的那个量，现在它只做内部用 |
| `fixed_point_items` | 164 | 新增，**唯一发号处**。按途经顺序重排 + 发 `position` / `id` / `isFixed` / `flag` / `isPass` / `state`；`glat`/`glon` 缺失（0）时由 BD-09 反算 |
| `payload_run_area_id` | 234 | 新增。`fixed_point_json` envelope 里的 `runAreaId` 默认发 -1（真机值），`PACE_RUN_AREA_ID=real` 可回退成真实围栏 id |
| `build` | 1105 | `cp_idxs` 改由 `checkpoint_track_indexes` 现算（不再读 `f.position`）；`fp_items` 改由 `fixed_point_items` 生成；envelope 键序按真机排（`fivePointJson` / `freedomShowFence` / `geoFencesJson` / `runAreaId` / `useZip`） |

### `api/flow.rs`

| 位置 | 改动 |
| --- | --- |
| `dry_run`（必到点构造） | 删掉 `PACE_POS_MODE` 那个「position 用序号还是轨迹下标」的开关（两种语义里只有一种是真机行为，留开关等于留 bug），只填坐标与名字 |
| `dry_run`（围栏） | `canonical_json_for(polygons[0])` → `geo_fence_json(run_area_id, fence_id, name, ring)` |
| `dry_run`（提交后日志） | 新增一行 `[fixed-point]` 日志，回显报文里五个点的 `position:名字#id(isFixed=)`——以后再出「显示不全」，先看这一行 |
| `detail_payload_rrid` | 打卡点下标不再从 `fivePointJson` 的 `position` 反解（那是连续序号，不是轨迹下标），改成按坐标调 `submit::checkpoint_track_indexes`，与提交侧同口径 |

### `api/fence.rs`

| 符号 | 改动 |
| --- | --- |
| `canonical_json_for` → `geo_fence_json` | 围栏对象补 `distance` / `id` / `name` 三个字段（真机有，我们原来只发 `points`）；坐标保留 6 位小数，键序按真机（`glat` / `glng` / `lat` / `lng`） |

---

## 三、真机依据（逐字段）

`evidence/obs_real_20261002.json` → `fixed_point_json.fivePointJson`：

```json
[{"flag":1790913882164,"glat":47.379397,"glon":123.950598,"id":81,"isFixed":1,
  "isPass":true,"lat":47.385718,"lon":123.956956,"pointName":"丁香园正门",
  "position":0,"state":0},
 {"flag":1790913882164,"glat":47.38045,"glon":123.953128,"id":82,"isFixed":0,
  "isPass":true,"lat":47.386792,"lon":123.959479,"pointName":"梅园2栋西",
  "position":1,"state":0},
 {"flag":1790913882164,"glat":47.378406,"glon":123.95016,"id":83,"isFixed":0,
  "isPass":true,"lat":47.384723,"lon":123.956519,"pointName":"综合超市西北",
  "position":2,"state":0},
 {"flag":1790913882164,"glat":47.380039,"glon":123.952308,"id":84,"isFixed":0,
  "isPass":true,"lat":47.386375,"lon":123.958661,"pointName":"秋实城北门",
  "position":3,"state":0},
 {"flag":1790913882164,"glat":47.379316,"glon":123.949474,"id":85,"isFixed":0,
  "isPass":true,"lat":47.385626,"lon":123.955836,"pointName":"丁香2栋西南",
  "position":4,"state":0}]
```

从这一份能读出的六条硬约束（全部已落进代码与测试）：

1. `position` = `0,1,2,3,4` —— **连续序号，从 0 起**，不是轨迹下标；
2. 数组顺序与 `position` 递增一致（数组下标 == position）；
3. `isFixed` 只有第 1 个点是 `1`，其余 `0`；
4. `id` = `81..85`，从 81 起连续；
5. `flag` 五点全等（整场起始戳 1790913882164），不是逐点时间；
6. `lat`/`lon` 与 `glat`/`glon` 都是**真值**（两套坐标系，不是 -1/0）。

envelope 顶层与围栏（同一文件）：

```text
fixed_point_json = {
  "fivePointJson": "[…5 个点…]",
  "freedomShowFence": false,
  "geoFencesJson": "[{\"distance\":0.0,\"id\":1668,\"name\":\"学校\",\"points\":[{\"glat\":47.382098,\"glng\":123.952159,\"lat\":47.388433,\"lng\":123.958514}, …20 个点…]}]",
  "runAreaId": -1,
  "useZip": false
}
```

即：`runAreaId` 是 `-1`，真实围栏 id（1668）只出现在 `geoFencesJson[0].id`。

---

## 四、改完后的实际取值（用我们那份真实产物重算）

输入：`/tmp/n2_obs.json`（rrid 1364081873）里解出来的 372 个轨迹采样点
（`gLat`/`gLng` 是 GCJ-02，反算回 BD-09）＋ 我们那 5 个必到点的服务端坐标。
跑 `submit::fixed_point_items` 的结果：

| position | id | isFixed | isPass | pointName | 首次经过下标（内部量） |
| --- | --- | --- | --- | --- | --- |
| 0 | 81 | 1 | true | 梅园2栋西 | 0 |
| 1 | 82 | 0 | true | 杏林3栋北 | 51 |
| 2 | 83 | 0 | true | 杏林3栋西北 | 71 |
| 3 | 84 | 0 | true | 丁香3栋北 | 88 |
| 4 | 85 | 0 | true | 梅园4栋西 | 170 |

`flag` 五个点全等 = `1794561660939`（本场 `startTime`，与 `save_record.startTime`
同一个值；真机那场的 `flag` 是 1790913882164，语义相同、数值必然不同）。

报文里的实际串（键序与真机一致）：

```json
[{"flag":1794561660939,"glat":47.38045,"glon":123.953128,"id":81,"isFixed":1,"isPass":true,"lat":47.386792,"lon":123.959479,"pointName":"梅园2栋西","position":0,"state":0},
 {"flag":1794561660939,"glat":47.381798,"glon":123.952096,"id":82,"isFixed":0,"isPass":true,"lat":47.388133,"lon":123.958451,"pointName":"杏林3栋北","position":1,"state":0},
 {"flag":1794561660939,"glat":47.381423,"glon":123.95104,"id":83,"isFixed":0,"isPass":true,"lat":47.387749,"lon":123.957398,"pointName":"杏林3栋西北","position":2,"state":0},
 {"flag":1794561660939,"glat":47.381063,"glon":123.950126,"id":84,"isFixed":0,"isPass":true,"lat":47.38738,"lon":123.956487,"pointName":"丁香3栋北","position":3,"state":0},
 {"flag":1794561660939,"glat":47.381403,"glon":123.953139,"id":85,"isFixed":0,"isPass":true,"lat":47.387745,"lon":123.95949,"pointName":"梅园4栋西","position":4,"state":0}]
```

注意首点变了：改前 `isFixed=1` 落在「梅园4栋西」（服务端 `isFixed` 原值），
改后落在「梅园2栋西」——它是我们这条轨迹**最先经过**的必到点（下标 0），
也就是真机语义里 `position=0` 的那个点。真机首点是「丁香园正门」，因为
它那条路线从那里起步。

`geoFencesJson`（同一份围栏环重算，与真机逐字段对比）：

```text
我们: [{"distance":0.0,"id":1668,"name":"学校","points":[{"glat":47.382099,"glng":123.952159,"lat":47.388433,"lng":123.958514}, …]}] 
真机: [{"distance":0.0,"id":1668,"name":"学校","points":[{"glat":47.382098,"glng":123.952159,"lat":47.388433,"lng":123.958514}, …]}]
                                              ↑ 末位 1e-6 的差（≈0.1m），见「还剩哪些不一致」
```

字段集合、字段顺序、`id`/`name`/`distance` 取值、`lat`/`lng` 全部一致。

---

## 五、测试结果

```text
$ PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR=/tmp/pace-target cargo test
test result: ok. 273 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 0.38s
```

（改前基线 271 passed；新增 2 个回归测试，另有 1 个旧测试改名并加强。）

新增 / 改写的测试：

| 测试 | 位置 | 钉住什么 |
| --- | --- | --- |
| `fixed_points_numbered_like_real_device` | `api/submit.rs` `audit_tests` | **主回归**。5 个必到点、故意打乱传入：`position == [0,1,2,3,4]`、数组顺序 == 途经顺序、`isFixed == [1,0,0,0,0]`、`id == [81,82,83,84,85]`（且 ≥80）、`flag` 全等、两套坐标都非 0 且互不相等、`isPass` 按真实匹配给、内部打卡下标仍按轨迹顺序 |
| `five_point_items_carry_device_fields` | `api/submit.rs` `audit_tests` | 11 个字段一个不少；`id` 断言从 `i+1` 改成 `81+i` |
| `obs_checkpoint_marking_uses_coords_not_position` | `api/flow.rs` `fixed_point_payload_tests` | OBS 明细里的 `type=2` 打卡标记必须按**坐标**匹配（`position=0` 的必到点实际落在第 25 个采样点时，标记必须落在第 21~27 个点且包含第 25 个）——防止有人再把 `position` 当轨迹下标用 |
| `geo_fence_json_matches_device_shape` | `api/fence.rs` `tests` | 围栏对象必须带 `id` / `name` / `distance`；坐标 6 位小数；无围栏 id 时退回 `runAreaId`（不发 null） |

---

## 六、还剩哪些与真机不一致

按影响从大到小：

1. **必到点数组的顺序来源**。真机那条记录的数组顺序就是它那场的途经顺序；
   我们按「首次经过下标升序」现排。如果服务端的「按顺序通过所有点位」
   是拿**它自己下发的顺序**去比轨迹，两套顺序不一致时这项仍会失败
   ——目前没有证据说它比对的是报文里的顺序（`flow::dry_run` 的点位顺序
   自检是拿服务端下发顺序比的，日志里会出现 `[point-order] 顺序错误`）。
   这次提交后要确认详情页那五项判定。
2. **`isPass` 的取值规则**。真机五个点全 `true`（它确实都经过了）。我们按
   「轨迹是否真的经过」填，没经过的点会发 `false`——真机样本里不存在这种
   点，服务端如何处理 `false` 没有样本可查（改前是无条件 `true`，这次
   改成如实填，属于行为变化，先记在这里）。
3. **必到点 `id` 的含义**。真机 `81..85` 极可能是服务端必到点表的自增列，
   我们只是「落在同一量级且连续」。如果详情页/后台按 `id` 回查必到点，
   我们这批 id 与它库里那批对不上。要彻底对齐得让服务端把点 id 下发到
   点位响应里（现在的点位响应只有坐标 + `isFixed` + `pointName`）。
4. **`save_record` 顶层的 `runAreaId` 仍发真实围栏 id**（1668）。真机只有
   `fixed_point_json` 这一份的对象样本（`-1`），`save/record` 的 body 没抓到，
   所以只把**envelope** 改成 -1，顶层字段保持原样——改它等于拿一次真实
   提交去赌。要一起改：`SubmitParams.run_area_id` 传给 `build` 时换成
   `payload_run_area_id(...)` 即可（一个调用点）。
5. **围栏 GCJ 坐标末位差 ±2e-6°（≈0.2m）**。`lat`/`lng` 与真机逐位相同，
   `glat`/`glng` 是我们本地 `bd09_to_gcj02` 算的，真机那套浮点实现的末位
   略有差异。0.1~0.2m 的差对面不会区分，但如果要逐字节一致，得逆出真机
   那个转换函数（`api/coord.rs` 的常数已经对过，差在舍入）。
6. **围栏 `name`**。真机是 `"学校"`，我们原样用围栏响应里的 `name`。
   没有抓到 `getGeoFenceForRun` 的响应样本，无法离线确认它一定带
   `name`；缺了就会发空串（不是猜测填死"学校"）。
7. **`state` 恒 0**。真机五个点也全是 0，一致；但没有「速度不合规」场景
   的样本，非 0 时该填什么仍未知（保持 0 是当前最稳的选择）。
8. **必到点坐标精度**。`lat`/`lon`/`glat`/`glon` 原样用服务端下发值
   （真机是 5~6 位小数），没做二次归一化——服务端下几位就发几位。

已对齐、不必再查的项：`flag` 全场同值（本场起始戳）、`position` 连续递增、
数组与 `position` 同序、`isFixed` 只在首点、`id` 从 81 连续、envelope
五个键与键序、`geoFencesJson` 的字段集合与取值、`lat/lon` 与 `glat/glon`
两套都是真值（不是 -1）。

---

## 七、回退开关

| 环境变量 | 作用 |
| --- | --- |
| `PACE_RUN_AREA_ID=real` | `fixed_point_json` envelope 里的 `runAreaId` 发真实围栏 id（默认 -1，真机值） |
| ~~`PACE_POS_MODE=idx`~~ | **已删除**。它能在「position = 轨迹下标 / = 连续序号」之间切换，而后者才是真机行为；留着这个开关等于留着复现该 bug 的路径 |
