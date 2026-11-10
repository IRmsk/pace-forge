# App 里的轨迹线：证据、结论、未定项

> 本文 2026-11-10 重写。上一版把「App 只读本地库、改报文没用」当成定论，
> 与当天实测的现象冲突（三 key 齐全的新记录在 App 里出了线）。这一版把
> **能复现的事实** 和 **没做对照实验的推断** 分开写，不再混在一起。

## 一句话现状

服务端判定层面已经正常：`complete=true`、`reasonList` 五项全过、`status=0`。
App 地图上画不画线，还剩一个**没做对照实验的分歧**，判定方法在文末，
两条记录、两分钟能收口。

## 硬证据（可复现）

### 1. 详情接口不回传轨迹点

50 条历史记录逐条拉 `POST /api/v70260/runnings/get_one_record`，结果一致：

| 字段 | 值 | 条数 |
|---|---|---|
| `allLocJson` | `null` | 50 / 50 |
| `jsonFromObs` | `1` | 50 / 50 |
| `fivePointJson` | `""` | 50 / 50 |

其中包括用 Neko 刷的 19 条（`avgPower` 184~266 非零，能认出来）和 pace 刷的 31 条。
**所以「服务端不回过轨迹点」与用哪个工具无关**，这点可以确定。

### 2. 轨迹对象确实在 OBS 里

`run_data/{rrid/1000000}/{rrid}.json` 用 `/api/obs/temporary/url` 换 Get 签名可下载：

```
rrid 1360341003 → 200，解 gzip 后 423 个点，每点 28 字段
```

所以「轨迹没上传」这个假设可以排除。

### 3. objectKey 的时区，实测分布

| 来源 | 条数 | UTC+8 key | UTC key | rrid key |
|---|---|---|---|---|
| pace 新版 | 3 | **3/3 在** | 3/3 | 3/3 |
| Neko | 19 | **19/19 在** | 0/19 | 0/19 |
| pace 旧版 | 28 | 部分 404（11-07 那批 10+ 条全 404） | 多数在 | 28/28 在 |

`run_data/{YYYYMMDDHH}/{uuid}.json` 里的 `HH` 按设备时区（UTC+8）算，这一点由
Neko 全量命中反证——它只传 UTC+8 这一份，且从未 404。

### 4. App 的分支判断（smali）

`RunHistoryDetailPImpl$requestSportRecord$1$...$map$1$2.smali:232-284`：

```kotlin
if (detailBean.jsonFromObs == 1) {
    RunDataUploadManger.OooO(uuid, uid, ...)
}
```

`getJsonFromObs()` 没有 setter（`grep -rn setJsonFromObs` 零命中），客户端控制不了。

`OooO` → `$queryUUID$2.smali` 查 `getRunDataDao().OooO0OO(uuid, uid)`，即本地 Room；
`entity == null` 时直接返回空。**这段反编译里没有看到网络回退**。

## 未定项：App 到底会不会去 OBS 拉

第 4 条和「用户观察到新记录有线」不能同时成立。两个候选：

**候选 A —— App 会拉 OBS。** 第 4 条只覆盖了一条分支（`jsonFromObs == 1` 且本地
无文件时走 `OooO`），但 `OooO` 内部或调用方可能还有一层「本地没有就用 objectKey
去 OBS 取」，反编译没跟到底。若成立，则 key 齐全（本次修复）就是有线的原因，
旧记录 11-07 那批 UTC+8 key 404 正是它们没线的原因。

**候选 B —— App 只读本地。** 那么无论 key 怎么样都不会有线，用户看到的线来自
别的路径（例如看的是 Web 端详情页，或记混了记录）。本次对报文的三处修正
（`allLocJson` 空串、五点补 `isPass/state/id/flag`、`fixed_point_json` 改 envelope）
解决的只是**服务端判定**（点位规则从 false 变 true），跟画线无关。

**两个候选都没有做对照实验，所以不下结论。** 上一版文档直接断言 B，本次提交
的 commit message 直接断言 A，两者都是过度断言。

## 判定实验（两条记录，两分钟）

| rrid | 时间 | UTC+8 key | 若 A 成立 | 若 B 成立 |
|---|---|---|---|---|
| `1360341003` | 11-10 15:36 | 存在 | **有线** | 无线 |
| `1358389153` | 11-07 20:00 | 404 | 无线 | 无线 |

在 App 里点开 `1360341003`：

- 有线 → 候选 A，本次修复即根因，之后刷的记录都该有线
- 无线 → 候选 B，App 画线不可控，只能用 Web 端看（详情页会从 OBS 拉 423 个点画出来）

只要第一行，第二行是交叉验证。取到结果后把本文的「未定项」一节删掉，直接写结论。

## 已排除的假设

区分两栏：**实测排除** 是有可复现的对照；**单次未复现** 只是改完看了一次。

| 假设 | 改动 | 结果 | 强度 |
|---|---|---|---|
| `flag` 全同导致画不出线 | 改成 `p.time_ms` 逐点递增 | 仍无线 | 单次未复现 |
| `bdA` 恒 1.0 无海拔特征 | 改成平滑缓坡 | 仍无线 | 单次未复现 |
| OBS 键少了 `step_json` | 对 `RunData` 建表语句，11 键是对的 | 不是问题 | 实测排除 |
| `useZip` 应为 true | 手工重打包该记录验证 | 仍无线 | 单次未复现 |
| 轨迹没上传 | 从 OBS 下载并解压，423 点齐全 | 不是问题 | 实测排除 |
| objectKey 时区猜错 | 旧版本地时区+UTC 各传一份 | 仍无线 | **单次未复现，本次重做** |

最后一条要注意：旧版的坑不在「各传一份」这个策略，而在**旧版两次运行取到了不同时区**
（11-07 那批只传了 UTC，10 月那批只传了 UTC+8 的 rrid 路径），所以单条记录可能只有
一个 key 命中、也可能一个都不中。新版固定 `8 * 3600` 并把 UTC 版与 rrid 路径一起传，
三条都命中——这一条是实测的（GET 200 × 3）。

`RunData` 表的 11 个键（`extension_json` 在里面，不用删）：

```sql
CREATE TABLE `RunData` (
  rrid, uuid, uid, run_data, step_freq_json, speed_json, segment_json,
  fixed_point_json, runFaceCheck, type, upload_success, timestamp, file,
  laps_json, runId, extension_json, isUploadSuccess
)
```

## 若要在 App 里必定有线：写本地库

需要往 `/data/data/com.zjwh.android_wh_physicalfitness/` 写 Room 记录 + 加密轨迹文件。
实测过的路径：

| 路径 | 结果 |
|---|---|
| Shizuku/adb shell 写 app 私有目录 | `Permission denied`，shell 域（uid=2000）进不了 `app_data_file` |
| `su -c` 提权 | `[POLICY_BLOCKED]`，设备无 root |
| `appops set android:mock_location allow` | `[POLICY_BLOCKED]`，不在白名单 |
| 伪造 GPS 让 App 自己跑一条 | 依赖上一条，开不了 mock location |
| App 自带导入（gpx/kml/importRun） | 不存在，全项目零命中 |
| 降级到老版本 App | 未验证。老版本可能走 `jsonFromObs=0` 直接读服务端，但服务端有强制更新检查 |
| 重打包 App 改判断 | 签名一变装不上 |

## 附：真机记录为什么一直有线

因为它在手机上跑过，本地库和轨迹文件都在。换台手机登录，自己真跑过的记录同样会
变成空地图——分界线是**这台手机上有没有那份文件**，不是记录是不是刷的。

## 更正记录

- `c2485e4`（轨迹线根因：App 按 UTC+8 时间戳 key 取 OBS…）：标题把候选 A 写成了
  结论，实际只验证了「三 key 齐全」和「点位判定修复」。以本文「未定项」为准。
- 本文上一版把候选 B 写成结论，同样没有对照实验。
