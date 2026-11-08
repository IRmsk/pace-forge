#!/usr/bin/env python3
"""提交报文与真机抓包做字段级 diff。

用途：抓到一个真机提交请求体之后，和本地生成的 payload 逐字段比对，
把「类型不对 / 少字段 / 多字段 / 数值差 / 单位差 100 倍 / 时间是秒不是
毫秒」这些一眼看不出来的差异全部列出来。

字段上百个，肉眼对必漏。这个脚本只依赖标准库。

用法：

    python3 tools/diff_payload.py \
        --sample captures/real_submit.json \
        --local  artifacts/payload.json \
        --keys allLocJson,fivePointJson,speedPerTenSec,stepsPerTenSec \
        --out    diff_report.json

`allLocJson`、`fivePointJson` 这类字段本身是字符串化 JSON，脚本会先
解析再比，不会把整串当成字符串长度差异。
"""

import argparse
import json
import sys
from typing import Any, Dict, List

DEFAULT_KEYS = ["allLocJson", "fivePointJson", "speedPerTenSec", "stepsPerTenSec"]
TOL_REL = 1e-3
TOL_ABS = 0.01


def _try_parse_str_json(s: Any) -> Any:
    """字符串化 JSON 先解析；解析不了就原样返回（真的是普通字符串）。"""
    if isinstance(s, str):
        stripped = s.lstrip()
        if stripped[:1] in ("{", "["):
            try:
                return json.loads(s)
            except Exception:
                return s
    return s


def _type_name(v: Any) -> str:
    if v is None:
        return "null"
    if isinstance(v, bool):
        return "bool"
    if isinstance(v, int):
        return "int"
    if isinstance(v, float):
        return "float"
    if isinstance(v, str):
        return "str"
    if isinstance(v, list):
        return "array"
    if isinstance(v, dict):
        return "object"
    return type(v).__name__


def _num_close(a: float, b: float) -> bool:
    return abs(a - b) <= max(TOL_ABS, TOL_REL * max(abs(a), abs(b)))


def _time_hint(v: Any, path: str) -> str:
    """13 位毫秒 vs 10 位秒。"""
    if not isinstance(v, (int, float)) or isinstance(v, bool):
        return ""
    low = path.lower()
    if "time" not in low and not low.endswith("ms"):
        return ""
    if 1e12 < v < 1e13:
        return " (ms)"
    if 1e9 < v < 1e10:
        return " (s! 疑似发了秒)"
    return ""


def _unit_hint(a: float, b: float, path: str) -> str:
    """距离字段差 100 倍 → 米 vs 厘米。"""
    low = path.lower()
    if "distance" not in low and not low.endswith("dis"):
        return ""
    if a and abs(b / a - 100) < 2:
        return " [单位差 100x：疑似米 vs 厘米]"
    if b and abs(a / b - 100) < 2:
        return " [单位差 100x：疑似米 vs 厘米]"
    return ""


def diff_value(a: Any, b: Any, path: str, out: List[Dict[str, Any]]) -> None:
    a = _try_parse_str_json(a)
    b = _try_parse_str_json(b)
    ta, tb = _type_name(a), _type_name(b)
    numeric = {"int", "float"}

    if ta != tb and not (ta in numeric and tb in numeric):
        out.append({"path": path, "kind": "type", "sample": a, "local": b,
                    "note": f"type {ta} vs {tb}"})
        return

    # int/float 差异在 wire 上必然出现（0 与 0.0），记一笔但继续按数值比
    if ta in numeric and tb in numeric and ta != tb:
        out.append({"path": path, "kind": "num_width", "sample": a, "local": b,
                    "note": "int vs float（wire 会差 .0）"})

    if ta == "null":
        return

    if ta == "object":
        ka, kb = set(a.keys()), set(b.keys())
        for k in sorted(kb - ka):
            out.append({"path": f"{path}.{k}", "kind": "missing_in_sample",
                        "sample": "<缺失>", "local": b[k],
                        "note": "本地生成了，抓包里没有"})
        for k in sorted(ka - kb):
            out.append({"path": f"{path}.{k}", "kind": "missing_in_local",
                        "sample": a[k], "local": "<缺失>",
                        "note": "抓包里有，本地没生成"})
        for k in sorted(ka & kb):
            diff_value(a[k], b[k], f"{path}.{k}", out)
        return

    if ta == "array":
        la, lb = len(a), len(b)
        if la != lb:
            out.append({"path": path, "kind": "array_len", "sample": la,
                        "local": lb, "note": ""})
        # 只比前几个元素，整条轨迹几百点全比没意义
        for i in range(min(la, lb, 3)):
            diff_value(a[i], b[i], f"{path}[{i}]", out)
        return

    if ta == "str":
        if a != b:
            out.append({"path": path, "kind": "str_val" if len(a) == len(b) else "str_len",
                        "sample": a[:40], "local": b[:40],
                        "note": f"len {len(a)} vs {len(b)}" if len(a) != len(b) else ""})
        return

    if ta in numeric or tb in numeric:
        fa, fb = float(a), float(b)
        if not _num_close(fa, fb):
            # 两侧都要看：抓包是毫秒、本地发成秒（或反过来）都要标出来
            note = _unit_hint(fa, fb, path) + _time_hint(fa, path) + _time_hint(fb, path)
            out.append({"path": path, "kind": "num", "sample": a, "local": b, "note": note})
        return

    if a != b:
        out.append({"path": path, "kind": "val", "sample": a, "local": b, "note": ""})


def main() -> int:
    ap = argparse.ArgumentParser(description="提交报文与真机抓包做字段级 diff")
    ap.add_argument("--sample", required=True, help="真机抓包的请求体 JSON")
    ap.add_argument("--local", required=True, help="本地生成的 payload.json")
    ap.add_argument("--keys", default="",
                    help="逗号分隔的顶层键；留空则比对两份报文里所有顶层键")
    ap.add_argument("--out", default="diff_report.json", help="报告输出路径")
    args = ap.parse_args()

    with open(args.sample, encoding="utf-8") as f:
        sample = json.load(f)
    with open(args.local, encoding="utf-8") as f:
        local = json.load(f)
    if args.keys.strip():
        keys = [k for k in args.keys.split(",") if k]
    else:
        # 留空就是全比：抓包和本地各有一批键，两边的并集都要过一遍，
        # 否则「本地多了个字段」「抓包多了个字段」永远看不见。
        keys = sorted(set(sample.keys()) | set(local.keys()))

    findings: List[Dict[str, Any]] = []
    for k in keys:
        if k not in sample:
            findings.append({"path": k, "kind": "missing_in_sample",
                             "sample": "<缺失>", "local": local.get(k),
                             "note": "抓包报文里没这个键"})
            continue
        if k not in local:
            findings.append({"path": k, "kind": "missing_in_local",
                             "sample": sample.get(k), "local": "<缺失>",
                             "note": "本地没生成这个键"})
            continue
        diff_value(sample[k], local[k], k, findings)

    print(f"{'path':<44} {'kind':<20} {'sample':<20} {'local':<20} note")
    print("-" * 130)
    for f in findings:
        print(f"{f['path']:<44} {f['kind']:<20} "
              f"{str(f['sample'])[:20]:<20} {str(f['local'])[:20]:<20} {f['note']}")
    print(f"\n共 {len(findings)} 条差异")

    with open(args.out, "w", encoding="utf-8") as fp:
        json.dump({"findings": findings, "count": len(findings)},
                  fp, ensure_ascii=False, indent=2)
    print(f"已写入 {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
