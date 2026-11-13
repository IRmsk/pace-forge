# 文档索引

根目录只留 README（给自己和用的人看的那份）。实现依据、验证记录、排查笔记
都收在这里，因为它们需要跟着代码版本走——`git grep`、`git blame`、切到旧
tag 复核都得能直接打开。

## audit/ 审计

| 文件 | 内容 |
|---|---|
| [AUDIT-round.md](audit/AUDIT-round.md) | 逐项审计清单（A1/A2/A4/A5、C4…）。代码注释里大量引用编号，是最常被翻的一篇 |

## fix/ 修复与对齐记录

每一篇对应一次「发现偏差 → 定位 → 改法 → 实测数字」的完整过程。改那部分代码
之前先看对应的那篇，里面写着为什么不能那么改。

| 文件 | 主题 |
|---|---|
| [FIX-A1-CHECKPOINT-HIT.md](fix/FIX-A1-CHECKPOINT-HIT.md) | 打卡点命中判定 |
| [FIX-A2-SPEED-UNITS.md](fix/FIX-A2-SPEED-UNITS.md) | `speed` 的三个口径（配速 / m/s / 滤波值） |
| [FIX-A4-SINGLE-POINT-SOURCE.md](fix/FIX-A4-SINGLE-POINT-SOURCE.md) | 自检与上传必须用同一份点位 |
| [FIX-A5-QUEUE-NUM.md](fix/FIX-A5-QUEUE-NUM.md) | `queueNum` 台阶 |
| [FIX-B-DATADIR-ISOLATION.md](fix/FIX-B-DATADIR-ISOLATION.md) | 数据目录隔离，测试不碰真实 `$HOME` |
| [FIX-B-PARSE-COORD.md](fix/FIX-B-PARSE-COORD.md) | 坐标字段解析（`gLat`/`gLng` 与占位哨兵） |
| [FIX-SAMPLE-RATE.md](fix/FIX-SAMPLE-RATE.md) | 上报节奏改成 5 秒等时间采样 |
| [FIXED-POINT-PARITY.md](fix/FIXED-POINT-PARITY.md) | 必到点字段对齐 |
| [SEGMENT-JSON-PARITY.md](fix/SEGMENT-JSON-PARITY.md) | `segmentJson` 分段对齐 |
| [STARTUP-WINDOW-PARITY.md](fix/STARTUP-WINDOW-PARITY.md) | 起步窗口（GPS 未锁定期）对齐 |

## notes/ 笔记

| 文件 | 内容 |
|---|---|
| [TYPE9-NOTES.md](notes/TYPE9-NOTES.md) | `type=9` 的观测笔记 |
| [TEST-ISOLATION.md](notes/TEST-ISOLATION.md) | 测试隔离的做法 |
| [TRAJECTORY-DISPLAY.md](notes/TRAJECTORY-DISPLAY.md) | App 里轨迹显示的取数路径 |
| [trajectory-status.html](trajectory-status.html) | 轨迹显示状态的一张自测页 |

## 为什么这些不放进 Releases

Releases 放的是**面向使用者的版本说明**（改了什么、怎么升级），它的文件不进
工作树，`git grep` / `git blame` / 旧 tag 复核都取不到。上面这些是**实现依据**，
代码注释直接按路径引用它们（`AUDIT-round.md` 被引用 19 处），放进 Releases
等于把注释全变成死链。
