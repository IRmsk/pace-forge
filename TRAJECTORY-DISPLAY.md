# App 里看不到轨迹的真正原因

记录点开只有里程、时长、配速，地图一条线都没有。这不是采样点数据不对，
是 App **根本不从服务端取轨迹**。

## 证据链

### 1. App 的分支判断

`RunHistoryDetailPImpl$requestSportRecord$1$invokeSuspend$$inlined$map$1$2.smali:232-284`：

```kotlin
val hourKey = DateUtil.format("yyyyMMddHH", startTime)   // 设备时区 UTC+8
if (detailBean.jsonFromObs == 1) {
    RunDataUploadManger.OooO(uuid, uid, ...)
}
```

`getJsonFromObs()` 没有 setter——它是 Gson 反序列化时直接从服务端响应填的
（`grep -rn setJsonFromObs` 全项目零命中）。客户端完全控制不了。

### 2. `OooO` 查的是本地数据库，不是 OBS

`RunDataUploadManger.OooO` → `RunDataUploadManger$queryUUID$2.smali`：

```kotlin
val entity = getRunDataDao().OooO0OO(uuid, uid)   // Room 查询
if (entity != null) {
    val file = File(entity.OooO0O0())              // 本地文件路径
    if (!file.exists()) return entity
    ...
}
return entity       // entity == null 也走这里，返回空
```

两条分支都只返回本地实体，**没有任何网络回退**。轨迹文件是加密的
（`RunDataEntity` 取路径 → `SecretKey` → 解密 → 明文 JSON）。

### 3. 所以

| 场景 | jsonFromObs | 手机本地有文件 | App 里 |
|---|---|---|---|
| 真机跑的 | 1 | 有 | 有线 |
| 本工具刷的 | 1 | 没有 | 只有标量 |

换个手机登录，真机跑的记录同样看不到轨迹——这是服务端的省存储设计，
不是我们这边的 bug。

## 已经排除或修掉的（都不是根因）

| 假设 | 改动 | 结果 |
|---|---|---|
| `flag` 全同 → 画不出线 | 改成 `p.time_ms`，逐点递增 | 仍无线 |
| `bdA` 恒 1.0 → 无海拔特征 | 改成平滑缓坡序列 | 仍无线 |
| OBS key 时区猜错 | 本地时区 + UTC 各传一份 | 仍无线 |
| `useZip` 应为 true | 手工重打包该记录验证 | 仍无线 |
| OBS 键少了 `step_json` | 查 `RunData` 表结构，11 键是对的 | 不是问题 |

关于时区：`DateUtil.OooOO0o` 是 `SimpleDateFormat(fmt, Locale.CHINA)`，
**没有设 TimeZone**，所以用设备默认时区 = UTC+8。这一点我原来猜对了。

关于键集合，`RunData` 表的建表语句是权威：

```sql
CREATE TABLE `RunData` (
  rrid, uuid, uid, run_data, step_freq_json, speed_json, segment_json,
  fixed_point_json, runFaceCheck, type, upload_success, timestamp, file,
  laps_json, runId, extension_json, isUploadSuccess
)
```

对应 OBS 的 11 个键，`extension_json` 确实在里面，不用删。

## 想让刷的记录在 App 里有线，只有一条路

往 App 私有目录写 Room 数据库记录 + 加密轨迹文件。需要 root 或 adb 有
写 `/data/data/com.zjwh.android_wh_physicalfitness/` 的权限，否则做不到。
重打包 App 绕过判断也可以，但签名一变就装不上，也不该做。

因此本项目能保证的是**服务端判定层面正常**：`status=0`、有效里程计入、
`reasonList` 各项通过。App 客户端的轨迹渲染不在可控范围内。
