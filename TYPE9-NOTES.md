# type=9「按顺序通过所有点位」排查记录

服务端判定里其余四项稳定通过，只有 `type=9` 一直是 false：

    ✓ type=1  最少要跑1.50公里
    ✓ type=7  跑12分钟以上
    ✓ type=11 步频控制在40步/分钟-300步/分钟之间
    ✓ type=12 配速控制在2'00"-12'00"
    ✗ type=9  按顺序通过所有点位

这份文件记录已经排除的变量，避免以后重走。

## 已排除的变量

| 变量 | 做法 | 结果 |
|---|---|---|
| 轨迹经过点位的顺序 | 加 `[point-order]` 自检，逐段避让后续点位，连续 5 次规划全绿 | 仍 false |
| 首尾闭合 | 环线闭合 / 不闭合各提交一次 | 两种都 false |
| 总里程 | 1149m / 1645m / 1941m / 2037m / 2069m | 与 type=9 无关 |
| `complete` | 2037≥1500 且 720≥720 → complete=true，unCompleteReason=0 | 已满足 |
| `state` 三态 | 首点由硬编码 1 改成 0，其余按瞬时速度判 0/1/2 | 仍 false |
| `type` 覆盖 | 打卡点 ±2 邻域标 1，不再随机撒 3/8 | 修好了 type=2/type=3，type=9 不变 |
| `runes` / `runef` 头 | 按 `policy_ts+uid`、`uuid+start_ms` 下发 | 仍 false |
| `geeToken` | 由空串改成 AES-256-CBC 本地 token，黄金向量逐字节对齐 | 仍 false |
| `extension_json` | 由 `gzip("{}")` 改成 `gzip("")`（长度 32→28） | 仍 false |
| 桶距离 vs totalDis | 实测 2037.150 vs 2037.177，相对误差 0.000013 | 已互洽 |
| OBS 键数 | 11 个，与服务端 schema 一致 | 已齐 |
| 设备指纹头 | 补 androidId/blMac/wifiMac/IMEI/appInstallTime/appUpdateTime/CustomDeviceId | 仍 false |
| objectKey 时区 | 由 UTC 改成 UTC+8，回读确认对象已在 `run_data/2026110720/` | 仍 false |

## 关键发现（与 type=9 无关，但都确实修掉了 bug）

1. **`type` 原来按坐标哈希瞎填**（38% 得 3、54% 得 0…）。真机的
   `isValidPoint`（libswsport.so@0x10e84）对 type 打分：1→30、2→20、
   其余 15，及格线 65。改成有语义的取值后，`type=2 经过必经点位` 和
   `type=3 至少通过2个普通点位` 双双转为通过。

2. **服务端不看 `fixed_point_json` 里的 `position`**。它自己从轨迹里找
   每个点位的经过位置再比较序列。早期几轮一直在调 `position`，方向
   从头就是错的。

3. **手绘轨迹不能等比缩放到目标里程**。用户是照着地图上显示的点位画的，
   原尺度下点位天然在线上；放大 2.5~3 倍后路径变大而点位坐标不动，
   这个性质当场失效，点位飘出几十米，之后怎么吸附都是打补丁。

4. **`extension_json` 是空串的 gzip，不是 `{}` 的 gzip**（28 字节 vs 32）。

5. **objectKey 的小时段是本地时区**，不是 UTC。用 UTC 拼会早 8 小时。

6. **步频区间是 40~300**，不是某些参考实现里写的 60~250。现在从服务端
   响应的 `stepTop`/`stepBottom` 解析，不抄常量。

## 下一步

按排查决策树，采样点维度的候选已全部排除。剩下两个方向：

- `crypto/envelope.rs` 的 `kd1/kd2/kd3`：真机可能是固定 ASCII 串而不是
  随机 hex。但 `response_key` 由这四段推导，改成固定值会连带换掉响应
  解密密钥，必须先从真机抓包确认，不能盲改。
- 真机采样对照：拿同一个 rrid 下真机记录的 `allLocJson`、
  `fixed_point_json`、两个桶数组与生成版逐字段 diff。
  配套工具：`tools/diff_payload.py`。

## 配套工具

    python3 tools/diff_payload.py \
        --sample captures/real_submit.json \
        --local  artifacts/payload.json \
        --out    diff_report.json

留空 `--keys` 会比对两份报文里所有顶层键，能识别：缺字段 / 多字段 /
类型不符 / 数值超差 / 距离单位差 100 倍 / 时间是秒而不是毫秒。
