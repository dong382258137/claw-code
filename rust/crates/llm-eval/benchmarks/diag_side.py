#!/usr/bin/env python3
"""诊断单条任务的某一侧:检出 worktree、跑验收,并打印**完整输出尾部**。

与 validate_two_sided.run_side 不同,本脚本把 cargo 输出原样落盘/回显,
用于定位 `fixed_exit=101` 这类「基线本身红」到底是编译错还是测试失败。
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import groundtruth_tasks as gt  # noqa: E402


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--tasks", required=True)
    ap.add_argument("--task-id", required=True)
    ap.add_argument("--side", choices=["pin", "fixed"], default="fixed")
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--no-shared-target", action="store_true",
                    help="不设 CARGO_TARGET_DIR(用 worktree 自己的 target,排除污染)")
    a = ap.parse_args()

    repo = gt.normalize_path(a.repo)
    task = next(json.loads(l) for l in open(a.tasks, encoding="utf-8")
                if l.strip() and json.loads(l)["id"] == a.task_id)
    wt = Path(repo) / ".gt-diag" / f"{a.side}-{task['id']}"
    wt.parent.mkdir(parents=True, exist_ok=True)
    if wt.exists():
        subprocess.run(["git", "-C", repo, "worktree", "remove", "--force", str(wt)],
                       capture_output=True)
    if a.side == "pin":
        gt.pin_task(repo, task, str(wt))
    else:
        gt.git(repo, "worktree", "add", "--detach", str(wt), task["base_commit"])

    cmd = task["acceptance"]["command"]
    env = None
    if not a.no_shared_target:
        import os
        env = dict(os.environ)
        env["CARGO_TARGET_DIR"] = gt.shared_target_dir(repo)
    print(f"=== {a.side} {task['id']} @ {task['base_commit'][:12]} ===", flush=True)
    print(f"cmd: {cmd}  cwd={gt.workspace_root(str(wt))}  shared_target={not a.no_shared_target}",
          flush=True)
    with open(Path(repo) / ".gt-diag" / "out.txt", "w", encoding="utf-8", errors="replace") as f:
        p = subprocess.Popen(cmd.split(), cwd=gt.workspace_root(str(wt)),
                             stdout=f, stderr=subprocess.STDOUT, env=env)
        try:
            p.wait(timeout=a.timeout)
        except subprocess.TimeoutExpired:
            subprocess.run(["taskkill", "/F", "/T", "/PID", str(p.pid)], capture_output=True)
            print(f"TIMEOUT {a.timeout}s", flush=True)
            return 3
    print(f"exit={p.returncode}", flush=True)
    out = (Path(repo) / ".gt-diag" / "out.txt").read_text(encoding="utf-8", errors="replace")
    print("--- output tail (2500 chars) ---")
    print(out[-2500:])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
