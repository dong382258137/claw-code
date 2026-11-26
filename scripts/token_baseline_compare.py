#!/usr/bin/env python3
"""对比两份 token 基线 JSON，输出降幅报告。

用法：
    python scripts/token_baseline_compare.py before.json after.json
    python scripts/token_baseline_compare.py --fixtures   # 用仓库内 fixture 自动对比

退出码：0 = 各指标均未上升（允许持平）；1 = 存在指标上升（回归）。
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile

FIXTURE_BEFORE = "tests/fixtures/active_plan_before.json"
FIXTURE_AFTER = "tests/fixtures/active_plan_after.json"
SCRIPT = "scripts/token_baseline.py"

# 关注指标：键名与 baseline JSON 的 baseline 段一致
METRICS = [
    ("total_tail_tokens", "尾部注入合计"),
    ("active_plan_tokens", "Active Plan"),
    ("current_task_tokens", "Current Task"),
    ("project_context_tokens", "Project context"),
    ("notebook_tokens", "Notebook"),
    ("step_status_tokens", "Step Status"),
]


def run_baseline(fixture: str, out: str) -> dict:
    cmd = [sys.executable, SCRIPT, "--input", fixture, "--output", out, "--json"]
    proc = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8")
    if proc.returncode != 0:
        sys.exit(f"基线脚本失败（{fixture}）：\n{proc.stderr}")
    with open(out, encoding="utf-8") as fh:
        return json.load(fh)


def compare(before: dict, after: dict) -> tuple[list[dict], bool]:
    rows = []
    regressed = False
    for key, label in METRICS:
        b = before["baseline"][key]
        a = after["baseline"][key]
        if b == 0 and a == 0:
            delta = "n/a"
        elif b == 0:
            delta = "+inf"
            regressed = True
        else:
            pct = (a - b) / b * 100
            delta = f"{pct:+.1f}%"
            if a > b:
                regressed = True
        rows.append({"key": key, "label": label, "before": b, "after": a, "delta": delta})

    # 重复模板数量变化（越少越好）
    rb = before["baseline"]["duplicate_markers"]["repeated_templates"]
    ra = after["baseline"]["duplicate_markers"]["repeated_templates"]
    rows.append(
        {
            "key": "repeated_templates",
            "label": "重复模板数",
            "before": len(rb),
            "after": len(ra),
            "delta": f"{len(ra) - len(rb):+d}",
        }
    )
    return rows, regressed


def render(rows: list[dict]) -> str:
    w = 24
    lines = [
        "=" * 68,
        "Token 基线前后对比",
        "=" * 68,
        f"{'指标':<{w}}{'before':>12}{'after':>12}{'变化':>12}",
        "-" * 68,
    ]
    for r in rows:
        lines.append(
            f"{r['label']:<{w}}{r['before']:>12.1f}{r['after']:>12.1f}{r['delta']:>12}"
        )
    lines.append("-" * 68)
    return "\n".join(lines)


def main() -> None:
    ap = argparse.ArgumentParser(description="对比两份 token 基线")
    ap.add_argument("before", nargs="?", help="before 基线 JSON")
    ap.add_argument("after", nargs="?", help="after 基线 JSON")
    ap.add_argument(
        "--fixtures",
        action="store_true",
        help=f"用仓库内 {FIXTURE_BEFORE} / {FIXTURE_AFTER} 自动对比",
    )
    ap.add_argument("--output", help="把对比结果写入 JSON")
    args = ap.parse_args()

    tmpdir = tempfile.mkdtemp(prefix="token-cmp-")
    if args.fixtures:
        before = run_baseline(FIXTURE_BEFORE, os.path.join(tmpdir, "before.json"))
        after = run_baseline(FIXTURE_AFTER, os.path.join(tmpdir, "after.json"))
    else:
        if not (args.before and args.after):
            ap.error("需要同时提供 before 与 after，或使用 --fixtures")
        with open(args.before, encoding="utf-8") as fh:
            before = json.load(fh)
        with open(args.after, encoding="utf-8") as fh:
            after = json.load(fh)

    rows, regressed = compare(before, after)
    print(render(rows))

    if args.output:
        parent = os.path.dirname(os.path.abspath(args.output))
        if parent:
            os.makedirs(parent, exist_ok=True)
        with open(args.output, "w", encoding="utf-8") as fh:
            json.dump({"rows": rows, "regressed": regressed}, fh, ensure_ascii=False, indent=2)
        print(f"\n已写入 {args.output}")

    sys.exit(1 if regressed else 0)


if __name__ == "__main__":
    main()
