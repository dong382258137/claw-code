#!/usr/bin/env python3
"""pass^1 执行器:驱动 claw 作为**解题者**跑 pinned 回归任务,聚合 pass^1。

这是「提升 CLAW」主线缺失的最后一环。既有脚本只做任务**构造/校验**
(`groundtruth_tasks.py`)与**判分** (`run_acceptance`),从不真正驱动 agent 解题。

链路(每任务):
  1. `git worktree add --detach <base_commit>` → 隔离检出;
  2. 反向应用修复 hunk(回退源码、保留测试)⇒ 缺陷重现、回归测试应失败(pin 侧);
  3. `claw -p <task.prompt> --dangerously-skip-permissions` 在 worktree 内解题;
  4. `cargo test -p <crate>` 退出码 == `acceptance.expect_exit` ⇒ resolved;
  5. 聚合 `pass^1 = resolved / total`。

诚实性红线(见 docs/2026-10-09-groundtruth-task-suite-design.md §6):
  - 判据是 **cargo test 退出码**,不是模型自评;
  - 结果标注「内部回归」,不冒充 SWE-bench leaderboard 官方分数。

用法:
  python run_pass1.py --repo D:/claw-code-src --claw /c/Users/38225/.cargo/bin/claw.exe \
      --tasks rust/crates/llm-eval/benchmarks/groundtruth_validated_40.jsonl \
      --max-new 5 --out .gt-run/pass1.json
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import groundtruth_tasks as gt  # noqa: E402
import validate_two_sided as vts  # noqa: E402


def _kill_tree(proc: subprocess.Popen) -> None:
    """杀掉整个进程树(Windows 上 cargo 会派生 rustc/测试孙进程)。"""
    try:
        subprocess.run(
            ["taskkill", "/F", "/T", "/PID", str(proc.pid)], capture_output=True
        )
    except Exception:  # noqa: BLE001
        try:
            proc.kill()
        except Exception:  # noqa: BLE001
            pass


def run_claw(
    claw_bin: str, worktree: str, prompt: str, timeout_s: int, extra_args: list[str]
) -> dict:
    """在 worktree 内以无头 `-p` 模式驱动 claw 解题。

    关键:参数顺序。`-p` 会把它**之后**的全部参数 join 成 prompt
    (见 commands_handler.rs),故 flag(`--dangerously-skip-permissions` 等)
    必须置于 `-p` **之前**。

    stdin 置 DEVNULL:无头模式下若 agent 触发 AskUserQuestion,原生路径
    `read_line` 立即得 EOF(空答案)而非阻塞。

    输出重定向到临时文件(而非管道):避免子进程持有管道导致 wait 永久死锁
    —— 与 `run_acceptance` 同因。
    """
    cmd = [claw_bin, *extra_args, "-p", prompt]
    t0 = time.time()
    timed_out = False
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8", errors="replace") as outf:
        proc = subprocess.Popen(
            cmd,
            cwd=gt.workspace_root(worktree),
            stdin=subprocess.DEVNULL,
            stdout=outf,
            stderr=subprocess.STDOUT,
        )
        try:
            proc.wait(timeout=timeout_s)
        except subprocess.TimeoutExpired:
            _kill_tree(proc)
            timed_out = True
        outf.seek(0)
        out = outf.read()
    return {
        "exit": proc.returncode,
        "timeout": timed_out,
        "seconds": round(time.time() - t0, 1),
        "output_tail": out[-3000:],
    }


def eval_one(
    claw_bin: str, repo: str, task: dict, work_root: Path, args: argparse.Namespace
) -> dict:
    task_id = task["id"]
    wt = work_root / task_id
    result: dict = {"task_id": task_id, "crate": task["crate"]}
    if wt.exists():
        subprocess.run(
            ["git", "-C", repo, "worktree", "remove", "--force", str(wt)],
            capture_output=True,
        )
    try:
        gt.pin_task(repo, task, str(wt))
    except BaseException as e:  # noqa: BLE001
        result.update({"resolved": False, "verdict": "pin_failed", "detail": str(e)[:500]})
        return result

    try:
        solve = run_claw(claw_bin, str(wt), task["prompt"], args.task_timeout, args.claw_arg)
        result["solve"] = {k: solve[k] for k in ("exit", "timeout", "seconds")}
        # 判分口径与任务校验**必须一致**:任务集是拿 `--own-tests`(只跑任务自身回归
        # 测试函数)校验的;若这里用整 crate `cargo test -p <crate>`,无关 flaky 测试
        # (需 Chromium/网络/计时/环境锁)会污染判分 → 假阴性(实测 runtime-607e2c11 /
        # runtime-6f4c0e4b 因此被误判 compile_error,实则自身测试无关失败)。
        t2 = dict(task)
        t2["acceptance"] = dict(task["acceptance"])
        t2["acceptance"]["command"] = vts._acceptance_cmd(task, args.own_tests)
        code, out = gt.run_acceptance(t2, str(wt), repo, timeout_s=args.accept_timeout)
        expect = task["acceptance"]["expect_exit"]
        result["accept_exit"] = code
        result["resolved"] = code == expect
        result["verdict"] = (
            "resolved" if code == expect else gt.classify_pinned_failure(code, out, expect)
        )
        if not result["resolved"]:
            result["accept_tail"] = out[-1500:]
    finally:
        if args.keep:
            result["worktree"] = str(wt)
        else:
            subprocess.run(
                ["git", "-C", repo, "worktree", "remove", "--force", str(wt)],
                capture_output=True,
            )
    return result


def load_tasks(path: str) -> list[dict]:
    tasks = []
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            tasks.append(json.loads(line))
    return tasks


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--tasks", required=True)
    ap.add_argument("--claw", required=True, help="claw 可执行文件路径")
    ap.add_argument("--out", default="")
    ap.add_argument("--max-new", type=int, default=0, help="只跑前 N 条(0=全部)")
    ap.add_argument("--task-timeout", type=int, default=1200, help="单任务解题超时(秒)")
    ap.add_argument("--accept-timeout", type=int, default=900, help="验收超时(秒)")
    ap.add_argument("--work-root", default=".gt-run/worktrees")
    ap.add_argument("--claw-arg", action="append", default=None,
                    help="透传给 claw 的 flag(置于 -p 之前);可重复")
    ap.add_argument("--keep", action="store_true", help="保留 worktree 供排查")
    ap.add_argument(
        "--own-tests",
        dest="own_tests",
        action="store_true",
        default=True,
        help="只用任务自身回归测试函数判分(默认开;与任务集校验口径一致)",
    )
    ap.add_argument(
        "--no-own-tests",
        dest="own_tests",
        action="store_false",
        help="改用整 crate 判分(会引入无关 flaky → 假阴性,不推荐)",
    )
    args = ap.parse_args()

    if args.claw_arg is None:
        args.claw_arg = ["--dangerously-skip-permissions"]

    tasks = load_tasks(args.tasks)
    if args.max_new > 0:
        tasks = tasks[: args.max_new]

    work_root = Path(gt.normalize_path(args.work_root)).resolve()
    work_root.mkdir(parents=True, exist_ok=True)

    results = []
    for i, task in enumerate(tasks, 1):
        print(f"[{i}/{len(tasks)}] {task['id']} ...", file=sys.stderr, flush=True)
        r = eval_one(args.claw, args.repo, task, work_root, args)
        resolved = r.get("resolved", False)
        print(
            f"    -> resolved={resolved} verdict={r.get('verdict')} "
            f"solve={r.get('solve', {}).get('seconds')}s accept_exit={r.get('accept_exit')}",
            file=sys.stderr,
            flush=True,
        )
        results.append(r)

    total = len(results)
    passed = sum(1 for r in results if r.get("resolved"))
    pass1 = passed / total if total else 0.0
    summary = {
        "benchmark": "groundtruth-regression-pin",
        "note": "内部回归(路径 C),不可与 SWE-bench leaderboard 比较",
        "total": total,
        "resolved": passed,
        "pass1": round(pass1, 4),
        "verdicts": {},
    }
    for r in results:
        v = r.get("verdict", "unknown")
        summary["verdicts"][v] = summary["verdicts"].get(v, 0) + 1

    report = {"summary": summary, "results": results}
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    if args.out:
        Path(gt.normalize_path(args.out)).write_text(
            json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
