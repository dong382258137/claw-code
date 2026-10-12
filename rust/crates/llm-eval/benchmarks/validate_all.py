#!/usr/bin/env python3
"""批量 validate 15 条 ground-truth 任务,剔除无效项。

判定(每条任务):
  pin(还原缺陷) → 跑验收 `cargo test -p <crate>`:
    - exit != 0 且非编译错误 → `test_failure`(理想 FAIL_TO_PASS,有效)
    - exit != 0 且编译错误   → `compile_error`(有效,但偏弱,单独标注)
    - exit == 0              → `unexpected_pass`(任务**无效**,未注入缺陷) → 剔除

产物:`groundtruth_validated.jsonl`(仅有效项)+ 控制台汇总。

用法:
  python validate_all.py --repo D:/claw-code-src \
    --tasks rust/crates/llm-eval/benchmarks/groundtruth_tasks.jsonl \
    --out rust/crates/llm-eval/benchmarks/groundtruth_validated.jsonl
"""
from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import groundtruth_tasks as gt  # noqa: E402


def validate_one(repo: str, task: dict, work_root: Path) -> dict:
    wt = work_root / task["id"]
    result = {"task_id": task["id"], "crate": task["crate"], "valid": False, "kind": "error"}
    try:
        gt.pin_task(repo, task, str(wt))
    except SystemExit as e:
        result["kind"] = "pin_failed"
        result["detail"] = str(e)[:200]
        return result
    try:
        code, out = gt.run_acceptance(task, str(wt), repo)
        kind = gt.classify_pinned_failure(code, out, task["acceptance"]["expect_exit"])
        result["kind"] = kind
        result["pinned_exit"] = code
        result["valid"] = kind != "unexpected_pass"
        # 附上关键错误行(便于人审)
        err_lines = [ln for ln in out.splitlines() if "error" in ln.lower()][:2]
        result["detail"] = " | ".join(err_lines)[:240]
    finally:
        if wt.exists():
            subprocess.run(
                ["git", "-C", repo, "worktree", "remove", "--force", str(wt)],
                capture_output=True,
            )
    return result


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--tasks", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--work-root", default=None)
    args = ap.parse_args()

    repo = gt.normalize_path(args.repo)
    work_root = Path(gt.normalize_path(args.work_root or (repo + "/.gt-work")))
    if work_root.exists():
        shutil.rmtree(work_root)
    work_root.mkdir(parents=True)

    tasks = [json.loads(l) for l in open(args.tasks, encoding="utf-8") if l.strip()]
    print(f"validating {len(tasks)} tasks (repo={repo})", flush=True)

    results = []
    for i, task in enumerate(tasks, 1):
        print(f"[{i}/{len(tasks)}] {task['id']} ...", flush=True, end="")
        r = validate_one(repo, task, work_root)
        results.append(r)
        print(f" {r['kind']} valid={r['valid']}", flush=True)

    valid = [t for t, r in zip(tasks, results) if r["valid"]]
    with open(args.out, "w", encoding="utf-8") as fh:
        for t in valid:
            fh.write(json.dumps(t, ensure_ascii=False) + "\n")

    # 汇总
    from collections import Counter

    kinds = Counter(r["kind"] for r in results)
    print("\n=== SUMMARY ===", flush=True)
    print(f"total={len(results)}  valid={len(valid)}  invalid={len(results) - len(valid)}", flush=True)
    print(f"kinds={dict(kinds)}", flush=True)
    print(f"wrote valid tasks -> {args.out}", flush=True)
    with open(args.out + ".report.json", "w", encoding="utf-8") as fh:
        json.dump({"summary": {"total": len(results), "valid": len(valid),
                               "kinds": dict(kinds)}, "results": results},
                  fh, ensure_ascii=False, indent=2)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
