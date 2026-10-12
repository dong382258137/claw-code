#!/usr/bin/env python3
"""补验 FAIL_TO_PASS 的另一半:修复态(base_commit 未回退)应**通过**验收。

回归钉机制要求两个条件同时成立,任务才有效:
  (a) 钉住 → 验收失败   ← validate_all.py 已验证
  (b) 修复态 → 验收通过 ← 本脚本验证

只验 (a) 不足以称「可信分母」:若 base_commit 本身测试就是红的(提交本身
未绿),则任务无效。

用法:
  python check_fixed.py --repo D:/claw-code-src --tasks <validated.jsonl> --sample 3
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import groundtruth_tasks as gt  # noqa: E402


def check_one(repo: str, task: dict, work_root: Path) -> dict:
    wt = work_root / f"fixed-{task['id']}"
    out = {"task_id": task["id"], "crate": task["crate"]}
    try:
        # base_commit 未回退 → 即「修复态」
        gt.git(repo, "worktree", "add", "--detach", str(wt), task["base_commit"])
        code, tail = gt.run_acceptance(task, str(wt), repo)
        out["fixed_exit"] = code
        out["passes_when_fixed"] = code == task["acceptance"]["expect_exit"]
        if not out["passes_when_fixed"]:
            errs = [l for l in tail.splitlines() if "error" in l.lower() or "FAILED" in l][:2]
            out["detail"] = " | ".join(errs)[:220]
    except Exception as e:  # noqa: BLE001
        out["passes_when_fixed"] = False
        out["detail"] = f"{type(e).__name__}: {e}"[:220]
    finally:
        if wt.exists():
            subprocess.run(["git", "-C", repo, "worktree", "remove", "--force", str(wt)],
                           capture_output=True)
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--tasks", required=True)
    ap.add_argument("--sample", type=int, default=3)
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    repo = gt.normalize_path(args.repo)
    tasks = [json.loads(l) for l in open(args.tasks, encoding="utf-8") if l.strip()]
    # 取前 sample 条(已验证有效的)
    tasks = tasks[: args.sample]
    work_root = Path(repo) / ".gt-work2"
    work_root.mkdir(exist_ok=True)

    results = []
    for i, t in enumerate(tasks, 1):
        print(f"[{i}/{len(tasks)}] {t['id']} fixed-state check ...", flush=True, end="")
        r = check_one(repo, t, work_root)
        results.append(r)
        print(f" passes_when_fixed={r['passes_when_fixed']} exit={r.get('fixed_exit')}", flush=True)

    print("\n=== FIXED-STATE SUMMARY ===", flush=True)
    ok = sum(1 for r in results if r["passes_when_fixed"])
    print(f"checked={len(results)}  fixed_passes={ok}  fixed_fails={len(results) - ok}", flush=True)
    for r in results:
        print(json.dumps(r, ensure_ascii=False), flush=True)
    if args.out:
        with open(args.out, "w", encoding="utf-8") as fh:
            json.dump(results, fh, ensure_ascii=False, indent=2)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
