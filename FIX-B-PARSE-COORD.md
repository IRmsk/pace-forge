# FIX-B：`parseCoordArray` 不认 `gLat`/`gLng`，把占位 `-1,-1` 当真坐标（连带短路 OBS 兜底）

日期：2026-11-13
改动文件：**`web/index.html` 一个文件**（`+89 / −19`，1951 → 2021 行）
未改动：`api/*.rs`、`track/*.rs`、`web/mod.rs` 一行未碰（`web/mod.rs` 用 `include_str!("index.html")`，改 html 即生效）
未新增功能：没有播放器、没有新面板，只改坐标解析与兜底判据

---

## 1. 缺陷

真机点位对象（`re/zjwh740/evidence/obs_real_20261002.json` 的 `raw_points`，114 点，25 字段）：

```json
{
  "lat": -1.0, "lng": -1.0,                    // ← 固定占位，不是坐标
  "gLat": "48.123456", "gLng": "124.123456",   // ← 真实坐标（GCJ-02；真机上是字符串，OBS 里是数字）
  "coorType": "gcj02", ...其他 21 个字段
}
```

本机实测：`raw_points` 114 点的 `lat` 全是 `-1.0`、`lng` 全是 `-1.0`，`gLat/gLng` 是真值（本次样本里是 JSON number，真机抓包里是字符串，两种都要能收）。

改动前的三处连锁错误：

1. **字段不认**：`parseCoordArray` 的对象分支只取 `p.lat ?? p.latitude` / `p.lng ?? p.lon ?? p.longitude`，`gLat`/`gLng` 完全不读 → 拿到的是 `-1,-1`。
2. **占位放行**：`valid(lat,lng)` 只判「有限 + |lat|≤90 + |lng|≤180 + 不是 (0,0)」，`-1,-1` 全部通过。于是一条 114 点的假轨迹被画到几内亚湾外的海里。
3. **兜底短路**：`selectRec` 的判据是 `pts.length <= 1`（数组长度），假点把数组塞到 114 个，条件永远不成立 → 「接口没带点位就去 OBS 取真轨迹」这条路径再也走不到。
4. **附带**：OBS 兜底自己也是坏的——`obsTrack` 内部虽然写了 `num(p.gLat) || num(p.lat)`，但后面 `.filter(p => p.lat && p.lng)` 判的是「有没有值」，`-1` 是真值，`-1,-1` 照样留下。所以就算兜底被触发，画出来还是几内亚湾。

---

## 2. 改法（7 个改动点，其中 1 处纯注释）

| # | 位置（改后行号） | 改什么 |
|---|---|---|
| 1 | `1841–1888` 新增 `isPlaceholderCoord` / `coordNum` / `coordOf`，并重写 `valid`（`1849`/`1853`/`1861`/`1875`） | 建立「一个点位对象 → 一个有效 `[lat,lng]`」的唯一入口 |
| 2 | `parseCoordArray` 对象分支 `1921–1925` | 字段名与有效性全部交给 `coordOf`（`gLat/gLng` 优先，占位剔除） |
| 3 | `obsTrack` `1704–1709` | 删掉自己那份 `num(gLat)||num(lat)` + `filter`，改用 `parseCoordArray(arr)`，与主路径同一套规则 |
| 4 | `selectRec` `1737–1746`、`1759` | 兜底判据 `pts.length <= 1` → `pts.length === 0`（有效点数为 0 才去 OBS）；文案区分「0 点」与「1 点」 |
| 5 | `drawPreview` 打卡点 `776–783` | 用 `coordOf(c)` 取 GCJ-02 坐标，取不到有效坐标就跳过，不拿占位值打点 |
| 6 | `redrawStrokes` 打卡点命中判定 `1336–1345` | 原来用 `[c.lat, c.lng]`（BD-09）去和手绘 GCJ-02 点算距离，改成同一套 GCJ-02；坐标拿不到的计为「漏」 |
| 7 | `findTrack` 注释 `1808–1817` | 说明真机字段形态与新判据（无行为改动） |

### 2.1 核心解析器

```js
function isPlaceholderCoord(v) {          // -1（真机哨兵）与 0（部分服务端响应）都不是坐标
  return v === 0 || v === -1;
}

function valid(lat, lng) {
  return Number.isFinite(lat) && Number.isFinite(lng) &&
         Math.abs(lat) <= 90 && Math.abs(lng) <= 180 &&
         !isPlaceholderCoord(lat) && !isPlaceholderCoord(lng);   // ← 新增：占位一律无效
}

function coordNum(v) {                     // 空值给 NaN（不是 0，0 是哨兵）；"48.123456" 能转
  if (v === null || v === undefined || v === '') return NaN;
  return Number(v);
}

function coordOf(p) {                      // 成对按优先级找，第一对通过 valid() 的就是它
  if (!p || typeof p !== 'object') return null;
  const pairs = [
    [p.gLat, p.gLng],                      // ① 真机 / OBS 真实坐标（GCJ-02，可能是字符串）
    [p.gcjLat, p.gcjLng],                  // ② /api/plan 的打卡点（GCJ-02，对得上高德瓦片）
    [p.glat, p.glng], [p.glat, p.glon],    // ③ 围栏 / 采样点的 GCJ-02 写法
    [p.lat, p.lng], [p.lat, p.lon],        // ④ 兜底（多为 BD-09，且常是占位）
    [p.latitude, p.longitude], [p.latitude, p.lon],
  ];
  for (const [a, b] of pairs) {
    const lat = coordNum(a), lng = coordNum(b);
    if (valid(lat, lng)) return [lat, lng];
  }
  return null;                             // 全是 -1 / 0 → 这个点没有坐标
}
```

**为什么成对取而不是逐字段取**：`gLat` 是 GCJ-02、`lat` 是 BD-09，逐字段拼（`gLat` + `lng`）会跨套，差几百米。成对取要么整对用 GCJ，要么整对退回。

**为什么 `-1` 和 `0` 都判无效**：真机用 `-1` 当「未定位」哨兵；`api/coord.rs` 顶部注释写明另一批服务端响应「只填 `glat/glon`，`lat/lon` 置 0」。两个都是占位。

### 2.2 兜底判据（明确写清）

```js
let pts = findTrack(d);          // 只返回通过 valid() 的点 → pts.length 就是「有效点数」
if (pts.length === 0) {          // 只有「一个有效点位都没有」才去 OBS
  const obs = await obsTrack(rrid, d);
  if (obs && obs.length) pts = obs;
}
```

* 判据是 **有效点数**，不是「数组非空」「数组长度 > 1」。
* `pts.length === 0` 才兜底；有 1 个及以上有效点位就不请求 OBS。
* `findTrack` 之间比较也是「有效点数最多者胜」：长 114 的占位数组（有效点 0）再也赢不过一段短的真轨迹。
* 0 点时文案是「OBS 里也没有这条记录的轨迹对象…」；1 点时改成「只有 1 个有效点位，连不成线（按规则不再兜底）」，不再误报成 OBS 里没有。

---

## 3. 全文件「涉及点位坐标」的位置清单

全文件搜过一遍（`lat` / `lng` / `lat\b` / `lon` / `latitude` / `gLat` / `gLng` / `gcjLat` / `parseCoord*` / `findTrack` / `polyline` / `circleMarker` / `fitBounds` / `setView`）。造坐标数组的地方只有两处：`findTrack`（→`parseCoordString`/`parseCoordArray`）和 `obsTrack`（本次已改为复用 `parseCoordArray`）。没有第二份解析器。

| 位置 | 用途 | 是否假设 `lat/lng` | 处理 |
|---|---|---|---|
| `parseCoordArray` `1906–1930` | 对象/数组点位 → `[lat,lng][]` | **是（就是这个 bug）** | **已改**：对象分支走 `coordOf`；数组分支的两轴判定继承 `valid()`（`-1/0` 自动剔） |
| `parseCoordString` `1891–1904` | `"lng,lat,lng,lat"` 字符串 | 间接（靠 `valid()`） | 无需改代码：`valid()` 收紧后占位串自动被剔 |
| `findTrack` `1818–1838` | 递归找点数最多的轨迹数组 | 间接 | 无需改代码，只补注释；比较口径因 `parseCoordArray` 收紧而变成「有效点数」 |
| `obsTrack` `1701–1712`（OBS 兜底取轨迹） | `run_data.allLocJson` → 点位 | **是（`-1` 被 `filter` 放行）** | **已改**：改用 `parseCoordArray` |
| `selectRec` `1733–1765` | 画轨迹 / 起终点标记 / `fitBounds` / 兜底判定 | **是（`pts.length <= 1` 短路）** | **已改**：判据换有效点数；`fitBounds` 只在有真轨迹时执行（未改，本来就在 `>1` 分支内） |
| `drawPreview` 打卡点 `774–786` | 画打卡点圆点 | 半（已优先 `gcjLat/gcjLng`，但无占位校验） | **已改**：`coordOf(c)` + 无效跳过 |
| `drawPreview` 中心点 `790–800` | 画手填中心点、`toGcj02` | 否（来自输入框，已校验） | 不改 |
| `drawPreview` 预览轨迹 `766–771` | `L.polyline(coords)` | 否（`/api/plan` 的 `coords` 已由 `web/mod.rs` 转成 GCJ-02，且是服务端算出来的轨迹） | 不改 |
| `redrawStrokes` 命中判定 `1326–1352` | 手绘轨迹到打卡点的距离 → 经过/漏掉 | **是（用 `c.lat/c.lng` BD-09 混算 GCJ-02）** | **已改**：`coordOf(c)`，同网格比较 |
| `metersBetween` / `strokesLength` `1298–1312` | 平面距离累加 | 否（只吃已解析好的 `[lat,lng]`） | 不改 |
| `readLocInputs` `1102–1111` | 手填/点选坐标校验 | 半（只拦了 `(0,0)`） | **已改**：改用 `isPlaceholderCoord`，`-1` 也算没填 |
| `toGcj02` / `wgs84ToGcj02` / `gcj02ToBd09` / `bd09ToGcj02` `1155–1204` | 坐标系换算 | 否（纯数学） | 不改 |
| `setPickLocation` / `initLoc` `1128–1230` | 定位标记、`setView` | 否（输入已校验） | 不改 |
| 手绘 `pointerdown/move` `1487–1513` | 采样点、`metersBetween` | 否（Leaflet 给的 GCJ-02 数值） | 不改 |
| 禁区 `1411–1420`、围栏 `757`/`1081` | `L.polygon` 画围栏/禁区 | 否（服务端 `ring` 直接是 `[lat,lng]`） | 不改 |
| `initMap` `1037` | 初始 `setView([34.5,108.9],4)` | 否（全国视图常量） | 不改 |
| `fitToAccountArea` `1068–1088` | 围栏 `fitBounds` | 否 | 不改 |

---

## 4. 验证（真跑，不是「改好了」）

环境：Node v24.19.0（有 `CompressionStream/DecompressionStream`，所以 `obsTrack` 的 gzip 解压链路是真跑，不是打桩）。**本机没有浏览器**（`chromium` / `google-chrome` / `playwright` 均不存在），改用下面两种替代验证：

1. **把 `web/index.html` 里的真实函数源码切出来**（按函数名做括号匹配、跳过字符串/模板串/注释），在 `node:vm` 沙箱里跑；`selectRec` 的 DOM/`L`/`api` 用最小桩，`obsTrack` 的 `/api/obs/fetch` 返回**真 gzip+base64** 的 `run_data`。
2. **整段内联脚本语法 + 加载期自检**：`node --check` 通过；再用 Proxy 化的 DOM/L 桩把整段脚本执行一遍，确认没有 ReferenceError、关键符号都定义成 `function`。

脚本：`/tmp/fixb_verify.mjs`、`/tmp/smoke.mjs`，全文见附录 A / B。

### 4.1 真机数据（114 点，`lat/lng` 全 `-1`）

```
=== 真机数据 ===
raw_points 点数: 114 | lat 全为: -1 | lng 全为: -1 | gLat 类型: number
PASS  真机 114 点解析出有效点数 > 0  [有效点 114 / 114]
PASS  真机解析点数 == 114  [114]
PASS  全部落在齐齐哈尔范围 lat 43~54  [lat 47.377740 ~ 47.380479]
PASS  全部落在齐齐哈尔范围 lon 120~135  [lon 123.949416 ~ 123.953309]
PASS  没有任何点是 -1,-1  [前 3 个: [[47.38012824076674,123.95210748142215],[47.38012824076674,123.95210748142215],[47.38012824076674,123.95210748142215]]]
PASS  第一个点 == gLat/gLng（48→47.380128, 124→123.952107）  [[47.38012824076674,123.95210748142215]]
PASS  gLat/gLng 为字符串时同样解析出 114 点  [114]
PASS  字符串与数字两种类型结果一致
PASS  findTrack 在占位数组与真数组之间选真数组  [点数 114 首点 [47.38012824076674,123.95210748142215]]
PASS  【对照】旧版把 114 个 -1,-1 全当有效点（这就是 bug）  [旧版有效点 114，全部 [-1,-1]]
```

**114 点 → 114 个有效点**，经纬度范围 47.377740~47.380479 / 123.949416~123.953309（齐齐哈尔，落在要求的 lat 43~54、lon 120~135 内），**没有一个 `-1,-1`**。

最后一条是对照：同一批数据喂 `git show HEAD:web/index.html` 里的旧函数，得到 114 个 `-1,-1` —— 这就是用户看到的「几内亚湾」。

### 4.2 脏数据 + 兜底链路

```
=== 脏数据：接口只回 lat:-1,lng:-1，无 gLat/gLng ===
PASS  脏数据解析出 0 个有效点  [0]
PASS  脏数据不会产生 -1,-1 点
PASS  混合数组只留下 57 个带 gLat 的点  [57]
[obs] 命中 key: run_data/123/123456789.json 点数 114
PASS  脏数据详情 → OBS 兜底被触发（尝试了候选 key）  [试了 1 个 key: run_data/123/123456789.json]
PASS  兜底画的是 OBS 真轨迹 114 点  [polylines=1 点数=114]
PASS  兜底后没有 -1,-1 点
PASS  兜底后地图 fitBounds 到真轨迹
PASS  详情里显示轨迹点数 114  [html="<div class=\"kv\"><span>记录编号</span><span>rrid 123456789</span><span>总距离</span><span>1.68 km</span>…
[obs] 全部候选 key 都没取到轨迹: [ 'run_data/123/123456789.json', 'run_data/12345/123456789.json' ]
PASS  OBS 也没有时不画线
PASS  OBS 也没有时不移动地图
PASS  提示文案说明 OBS 里也没有
[obs] 命中 key: run_data/123/123456789.json 点数 114
PASS  有有效点位 → 不请求 OBS  [0]
PASS  有有效点位 → 直接画详情里的 114 点

=== 汇总 ===
断言 23 条，PASS 23，FAIL 0
```

三条链路分别是：

1. **详情只回 `lat:-1,lng:-1`、没有 `gLat/gLng`**：解析出 0 个有效点 → 触发 OBS → 画 114 个真点、`fitBounds` 到真轨迹、详情显示「轨迹点 114 个」。**这正是改前走不到的那条路。**
2. **脏数据 + OBS 也没有**：不画线、**不移动地图**、文案说清「OBS 里也没有」。
3. **详情自带 `gLat/gLng`**：`/api/obs/fetch` 调用次数 **0** —— 有有效点位就不兜底。

### 4.3 语法与加载期

```
SMOKE OK: 内联脚本在 Proxy 桩里跑完没有抛错
PASS  parseCoordArray = function
PASS  parseCoordString = function
PASS  findTrack = function
PASS  coordOf = function
PASS  valid = function
PASS  isPlaceholderCoord = function
PASS  obsTrack = function
PASS  selectRec = function
```

另：`node --check` 对内联 script 块（56279 字节，单块）语法检查通过。

---

## 5. 改动行数统计

```
$ git diff --numstat -- web/index.html
89	19	web/index.html

$ git diff --stat -- web/index.html
 web/index.html | 108 +++++++++++++++++++++++++++++++++++++++++++++++----------
 1 file changed, 89 insertions(+), 19 deletions(-)
```

`web/index.html`：1951 → 2021 行（净 +70；+89 行里 40 行是注释/文档说明，逻辑新增约 49 行；删除 19 行）。

hunk 分布（13 个 hunk）：

```
@@ -775,0 +776,5 @@    drawPreview 打卡点（注释）
@@ -777,2 +782 @@     drawPreview 打卡点（coordOf + 跳过无效）
@@ -1104 +1108,2 @@  readLocInputs（占位校验）
@@ -1330,0 +1336,5 @@  redrawStrokes（注释）
@@ -1334 +1344 @@     redrawStrokes（coordOf）
@@ -1694,4 +1704,6 @@  obsTrack（改用 parseCoordArray）
@@ -1725 +1737,7 @@  selectRec（兜底判据 + 注释）
@@ -1728 +1746 @@     selectRec（obs 直接接管）
@@ -1741,4 +1759,7 @@  selectRec（0 点 / 1 点文案）
@@ -1792 +1813,4 @@   findTrack（注释）
@@ -1816,0 +1841,12 @@ isPlaceholderCoord + coordNum 新增
@@ -1820 +1856,33 @@  valid 重写 + coordOf 新增
@@ -1853,3 +1921,5 @@  parseCoordArray 对象分支
```

### 工作区隔离核对

开工前（`git diff --stat`）：

```
 api/fence.rs     |   67 +-
 api/flow.rs      |  854 ++++--
 api/session.rs   |  188 +-
 api/submit.rs    | 2086 +++++++--
 track/buckets.rs |  153 +-
 track/mod.rs     | 1041 +++++++
 6 files changed, 3947 insertions(+), 442 deletions(-)
```

收工时（并行任务仍在推进，Rust 侧数字还在变）：

```
 api/accounts.rs   |  179 ++++-
 api/client.rs     |  115 ++-
 api/fence.rs      |   67 +-
 api/flow.rs       |  854 +++++++++----
 api/policy.rs     |   93 ++-
 api/session.rs    |  231 +++++-
 api/submit.rs     | 2086 ++++++++++++++++++----
 lib.rs            |    6 +
 track/buckets.rs  |  153 +++-
 track/mod.rs      | 1041 +++++++++++++++++
 track/roadnet.rs  |  245 ++++-
 track/tileroad.rs |   59 +-
 web/index.html    |  108 ++-     ← 本轮唯一的改动（+89 / −19）
 13 files changed, 4667 insertions(+), 570 deletions(-)
```

多出来的 `api/accounts.rs`、`api/client.rs`、`api/policy.rs`、`lib.rs`、`track/roadnet.rs`、`track/tileroad.rs` 以及既有 Rust 文件的数字变化，是并行任务在同一时间推进的（开工时列表里 6 个文件，收工时 13 个），**不是本轮改的**。本轮 `git diff --numstat -- web/index.html` 是唯一由我产生的条目（开工前 `web/index.html` 根本不在 diff 里）。

`git status --short` 里新增的未跟踪文件只有两份是本轮产物：`FIX-B-PARSE-COORD.md`（本报告）；另一个 `test_support.rs` 是并行任务的。`FIX-A*.md` 等都早于本轮存在。

回归红线：没碰 `api/`、`track/`、`web/mod.rs`、`main.rs`；没有 `git commit` / `git push`；没有新增 web 轨迹播放器或任何新面板/新交互。

---

## 附录 A：`/tmp/fixb_verify.mjs`（真机数据 + 脏数据 + 兜底链路）

```js
// FIX-B 验证：把 web/index.html 里的真实函数抽出来跑，不重写逻辑。
// 用法: node /tmp/fixb_verify.mjs [path/to/index.html]
import fs from 'node:fs';
import vm from 'node:vm';
import zlib from 'node:zlib';

const HTML = process.argv[2] || '/sdcard/workspace/pace-forge/web/index.html';
const OBS_JSON = '/sdcard/workspace/re/zjwh740/evidence/obs_real_20261002.json';
const src = fs.readFileSync(HTML, 'utf8');

// ── 从 HTML 里按函数名切出源码（跳过字符串/模板串/注释的花括号计数） ──
function sliceFunction(text, name) {
  const re = new RegExp(`(?:^|\\n)(?:async\\s+)?function\\s+${name}\\s*\\(`);
  const m = re.exec(text);
  if (!m) throw new Error('找不到函数: ' + name);
  const start = m.index + (m[0].startsWith('\n') ? 1 : 0);
  let i = text.indexOf('{', start);
  let depth = 0;
  let state = 'code'; // code | line | block | sq | dq | tpl
  for (; i < text.length; i++) {
    const c = text[i], n = text[i + 1];
    if (state === 'line') { if (c === '\n') state = 'code'; continue; }
    if (state === 'block') { if (c === '*' && n === '/') { state = 'code'; i++; } continue; }
    if (state === 'sq') { if (c === '\\') { i++; continue; } if (c === "'") state = 'code'; continue; }
    if (state === 'dq') { if (c === '\\') { i++; continue; } if (c === '"') state = 'code'; continue; }
    if (state === 'tpl') { if (c === '\\') { i++; continue; } if (c === '`') state = 'code'; continue; }
    if (c === '/' && n === '/') { state = 'line'; i++; continue; }
    if (c === '/' && n === '*') { state = 'block'; i++; continue; }
    if (c === "'") { state = 'sq'; continue; }
    if (c === '"') { state = 'dq'; continue; }
    if (c === '`') { state = 'tpl'; continue; }
    if (c === '{') depth++;
    else if (c === '}') { depth--; if (depth === 0) return text.slice(start, i + 1); }
  }
  throw new Error('花括号没闭合: ' + name);
}

const NAMES = [
  'ungzipB64', 'stampHour', 'obsTrack', 'selectRec', 'detailHtml', 'km',
  'findTrack', 'isPlaceholderCoord', 'valid', 'coordNum', 'coordOf',
  'parseCoordString', 'parseCoordArray', 'fmtDur', 'pace',
];
const consts = ['const num =', 'const int =', 'const esc ='];

function buildSandbox(text, extraNames = []) {
  const parts = [];
  for (const n of [...NAMES, ...extraNames]) {
    try { parts.push(sliceFunction(text, n)); } catch (e) { /* 旧版本没有这个函数，跳过 */ }
  }
  for (const c of consts) {
    const i = text.indexOf(c);
    const end = text.indexOf('\n', i);
    parts.push(text.slice(i, end));
  }
  const code = parts.join('\n\n');
  const sandbox = {
    console, JSON, Number, String, Math, Date, Array, Object, Uint8Array,
    Blob, Response, atob, btoa, Promise, Infinity, NaN,
    DecompressionStream,
    html: '',
  };
  sandbox.globalThis = sandbox;
  vm.createContext(sandbox);
  vm.runInContext(code, sandbox, { filename: 'extracted.js' });
  return { sandbox, code };
}

const results = [];
function check(name, cond, detail) {
  results.push({ name, ok: !!cond, detail });
  console.log((cond ? 'PASS' : 'FAIL') + '  ' + name + (detail ? '  [' + detail + ']' : ''));
}

// ─────────────────────────────────────────────────────────────────
// 一、真机数据：obs_real_20261002.json 的 raw_points（114 点）
// ─────────────────────────────────────────────────────────────────
const real = JSON.parse(fs.readFileSync(OBS_JSON, 'utf8'));
const rawPoints = real.raw_points;
console.log('\n=== 真机数据 ===');
console.log('raw_points 点数:', rawPoints.length,
  '| lat 全为:', [...new Set(rawPoints.map(p => p.lat))].join(','),
  '| lng 全为:', [...new Set(rawPoints.map(p => p.lng))].join(','),
  '| gLat 类型:', [...new Set(rawPoints.map(p => typeof p.gLat))].join(','));

const NEW = buildSandbox(src);
const ptsReal = vm.runInContext('parseCoordArray', NEW.sandbox)(rawPoints);
const latMin = Math.min(...ptsReal.map(p => p[0])), latMax = Math.max(...ptsReal.map(p => p[0]));
const lngMin = Math.min(...ptsReal.map(p => p[1])), lngMax = Math.max(...ptsReal.map(p => p[1]));

check('真机 114 点解析出有效点数 > 0', ptsReal.length > 0, '有效点 ' + ptsReal.length + ' / ' + rawPoints.length);
check('真机解析点数 == 114', ptsReal.length === 114, String(ptsReal.length));
check('全部落在齐齐哈尔范围 lat 43~54', latMin >= 43 && latMax <= 54, `lat ${latMin.toFixed(6)} ~ ${latMax.toFixed(6)}`);
check('全部落在齐齐哈尔范围 lon 120~135', lngMin >= 120 && lngMax <= 135, `lon ${lngMin.toFixed(6)} ~ ${lngMax.toFixed(6)}`);
check('没有任何点是 -1,-1', ptsReal.every(p => !(p[0] === -1 && p[1] === -1)), '前 3 个: ' + JSON.stringify(ptsReal.slice(0, 3)));
check('第一个点 == gLat/gLng（48→47.380128, 124→123.952107）',
  Math.abs(ptsReal[0][0] - rawPoints[0].gLat) < 1e-9 && Math.abs(ptsReal[0][1] - rawPoints[0].gLng) < 1e-9,
  JSON.stringify(ptsReal[0]));

// 字符串形态（真机把 gLat/gLng 序列化成字符串）
const asStrings = rawPoints.map(p => ({ ...p, gLat: String(p.gLat), gLng: String(p.gLng) }));
const ptsStr = vm.runInContext('parseCoordArray', NEW.sandbox)(asStrings);
check('gLat/gLng 为字符串时同样解析出 114 点', ptsStr.length === 114, String(ptsStr.length));
check('字符串与数字两种类型结果一致',
  JSON.stringify(ptsStr.map(p => p.map(x => +x.toFixed(9)))) === JSON.stringify(ptsReal.map(p => p.map(x => +x.toFixed(9)))));

// findTrack 面对「一堆 -1 占位数组 + 一段真数组」时必须选真那段
const payload = { junk: rawPoints.map(p => ({ lat: -1, lng: -1 })), good: rawPoints };
const viaFind = vm.runInContext('findTrack', NEW.sandbox)(payload);
check('findTrack 在占位数组与真数组之间选真数组', viaFind.length === 114 && viaFind.every(p => p[0] > 40),
  '点数 ' + viaFind.length + ' 首点 ' + JSON.stringify(viaFind[0]));

// 旧版对照：git HEAD 里的 parseCoordArray
let OLD = null;
try {
  const oldSrc = fs.readFileSync('/tmp/index_head.html', 'utf8');
  OLD = buildSandbox(oldSrc);
  const ptsOld = vm.runInContext('parseCoordArray', OLD.sandbox)(rawPoints);
  check('【对照】旧版把 114 个 -1,-1 全当有效点（这就是 bug）',
    ptsOld.length === 114 && ptsOld.every(p => p[0] === -1 && p[1] === -1),
    '旧版有效点 ' + ptsOld.length + '，全部 ' + JSON.stringify(ptsOld[0]));
} catch (e) {
  console.log('（跳过旧版对照：' + e.message + '）');
}

// ─────────────────────────────────────────────────────────────────
// 二、脏数据 + 兜底：接口只回 lat:-1,lng:-1，没有 gLat/gLng
// ─────────────────────────────────────────────────────────────────
console.log('\n=== 脏数据：接口只回 lat:-1,lng:-1，无 gLat/gLng ===');
const dirty = rawPoints.map(p => ({ lat: -1, lng: -1, gainTime: p.gainTime }));
const ptsDirty = vm.runInContext('parseCoordArray', NEW.sandbox)(dirty);
check('脏数据解析出 0 个有效点', ptsDirty.length === 0, String(ptsDirty.length));
check('脏数据不会产生 -1,-1 点', ptsDirty.every(p => p[0] !== -1 && p[1] !== -1));

// 混合：一半只有 -1（无 gLat），一半有真 gLat → 只留真的
const mixed = rawPoints.map((p, i) => i % 2 === 0 ? { lat: -1, lng: -1 } : { lat: -1, lng: -1, gLat: p.gLat, gLng: p.gLng });
const ptsMixed = vm.runInContext('parseCoordArray', NEW.sandbox)(mixed);
check('混合数组只留下 57 个带 gLat 的点', ptsMixed.length === 57, String(ptsMixed.length));

// ── selectRec 全链路：record 无点位 → 必须走 OBS 拿真轨迹 ──
function makeRecHarness(recordJson, obsPayloads) {
  const calls = { fetchKeys: [], lines: [] };
  const sandbox = NEW.sandbox;
  // 精简的 L / DOM 桩：只实现 selectRec 用到的部分
  sandbox.L = {
    polyline: (pts) => { calls.lines.push(pts); return { addTo() { return this; }, getBounds() { return { isValid: () => true }; } }; },
    circleMarker: () => ({ addTo() { return this; } }),
  };
  sandbox.layer = { clearLayers() {} };
  sandbox.map = { fitBounds() { calls.fitted = true; }, invalidateSize() {} };
  sandbox.$ = () => ({ set innerHTML(v) { sandbox.html = v; }, textContent: '' });
  sandbox.document = { querySelectorAll: () => [] };
  sandbox.api = async (path, opts) => {
    if (path.startsWith('/api/record/')) return recordJson;
    if (path === '/api/obs/fetch') {
      const key = JSON.parse(opts.body).key;
      calls.fetchKeys.push(key);
      if (obsPayloads[key]) return obsPayloads[key];
      throw new Error('404');
    }
    throw new Error('unexpected ' + path);
  };
  vm.runInContext('selectRec', sandbox)({ rrid: 123456789 }, null);
  return { calls, sandbox };
}

const gz = o => zlib.gzipSync(Buffer.from(JSON.stringify(o), 'utf8')).toString('base64');
const obsKey = 'run_data/' + Math.floor(123456789 / 1000000) + '/123456789.json';
const obsPayload = { data: { run_data: gz({ allLocJson: JSON.stringify(rawPoints) }) } };

(async () => {
  // 2a. 只有 lat:-1,lng:-1 的详情 + OBS 里有真轨迹 → 必须画 OBS 的 114 点
  const h1 = makeRecHarness(
    { rrid: 123456789, jsonFromObs: 1, totalDistance: 1.68, lat: -1, lng: -1,
      allLocJson: JSON.stringify(dirty) },
    { [obsKey]: obsPayload }
  );
  await new Promise(r => setTimeout(r, 500));
  check('脏数据详情 → OBS 兜底被触发（尝试了候选 key）', h1.calls.fetchKeys.length > 0,
    '试了 ' + h1.calls.fetchKeys.length + ' 个 key: ' + h1.calls.fetchKeys.slice(0, 2).join(' , '));
  check('兜底画的是 OBS 真轨迹 114 点', h1.calls.lines.length === 1 && h1.calls.lines[0].length === 114,
    'polylines=' + h1.calls.lines.length + ' 点数=' + (h1.calls.lines[0] || []).length);
  check('兜底后没有 -1,-1 点', (h1.calls.lines[0] || []).every(p => !(p[0] === -1 && p[1] === -1)));
  check('兜底后地图 fitBounds 到真轨迹', h1.calls.fitted === true);
  check('详情里显示轨迹点数 114', /114 个/.test(h1.sandbox.html || ''),
    'html=' + JSON.stringify((h1.sandbox.html || '').slice(0, 160)));

  // 2b. OBS 里也没有 → 不画线、不乱跳地图
  const h2 = makeRecHarness(
    { rrid: 123456789, jsonFromObs: 1, allLocJson: JSON.stringify(dirty) },
    {}
  );
  await new Promise(r => setTimeout(r, 500));
  check('OBS 也没有时不画线', h2.calls.lines.length === 0);
  check('OBS 也没有时不移动地图', !h2.calls.fitted);
  check('提示文案说明 OBS 里也没有', /OBS 里也没有/.test(h2.sandbox.html || ''));

  // 2c. 详情自带真 gLat/gLng（114 点）→ 有有效点位就不该去 OBS
  const h3 = makeRecHarness(
    { rrid: 123456789, allLocJson: rawPoints },
    { [obsKey]: obsPayload }
  );
  await new Promise(r => setTimeout(r, 500));
  check('有有效点位 → 不请求 OBS', h3.calls.fetchKeys.length === 0, String(h3.calls.fetchKeys.length));
  check('有有效点位 → 直接画详情里的 114 点', h3.calls.lines.length === 1 && h3.calls.lines[0].length === 114);

  // ── 汇总 ──
  const bad = results.filter(r => !r.ok);
  console.log('\n=== 汇总 ===');
  console.log('断言 ' + results.length + ' 条，PASS ' + (results.length - bad.length) + '，FAIL ' + bad.length);
  if (bad.length) { console.log('失败项：', bad.map(b => b.name)); process.exit(1); }
})();
```

复跑：

```bash
cd /sdcard/workspace/pace-forge
git show HEAD:web/index.html > /tmp/index_head.html   # 旧版对照用
node /tmp/fixb_verify.mjs                             # → 断言 23 条，PASS 23，FAIL 0
```

## 附录 B：`/tmp/smoke.mjs`（整段内联脚本加载期自检）

```js
// 用 Proxy DOM 桩把整段内联脚本跑一遍，看加载期有没有 ReferenceError / SyntaxError。
import fs from 'node:fs';
import vm from 'node:vm';

const html = fs.readFileSync('/sdcard/workspace/pace-forge/web/index.html', 'utf8');
const code = /<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/.exec(html)[1];

const mk = (name = 'stub') => {
  const fn = function () { return mk(name + '()'); };
  return new Proxy(fn, {
    get(t, k) {
      if (k === Symbol.toPrimitive) return () => 0;
      if (k === 'toString') return () => '';
      if (k === 'then') return undefined;
      if (k === Symbol.iterator) return function* () {};
      if (k === 'length') return 0;
      return mk(name + '.' + String(k));
    },
    set() { return true; },
    apply() { return mk(name + '()'); },
    construct() { return mk('new ' + name); },
    has() { return true; },
  });
};

const sandbox = {
  console: { log() {}, warn() {}, error() {} },
  document: mk('document'), window: mk('window'), navigator: mk('navigator'),
  L: mk('L'), localStorage: mk('localStorage'), fetch: async () => { throw new Error('no net'); },
  setTimeout: () => 0, setInterval: () => 0, clearTimeout() {}, clearInterval() {},
  alert() {}, confirm: () => false, prompt: () => null,
  location: mk('location'), history: mk('history'),
  DecompressionStream: class {}, Blob, Response, atob, btoa, Uint8Array,
  requestAnimationFrame: () => 0, getComputedStyle: () => mk('style'),
};
sandbox.globalThis = sandbox;
vm.createContext(sandbox);
try {
  vm.runInContext(code, sandbox, { filename: 'web/index.html', timeout: 8000 });
  console.log('SMOKE OK: 内联脚本在 Proxy 桩里跑完没有抛错');
  const names = ['parseCoordArray', 'parseCoordString', 'findTrack', 'coordOf', 'valid', 'isPlaceholderCoord', 'obsTrack', 'selectRec'];
  for (const n of names) {
    const t = vm.runInContext('typeof ' + n, sandbox);
    console.log((t === 'function' ? 'PASS' : 'FAIL') + '  ' + n + ' = ' + t);
  }
} catch (e) {
  console.log('SMOKE FAIL:', e.name, e.message);
  console.log((e.stack || '').split('\n').slice(0, 6).join('\n'));
  process.exit(1);
}
```
