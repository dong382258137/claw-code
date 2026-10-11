#!/usr/bin/env python3
"""双向校验 ground-truth 任务(FAIL_TO_PASS 的完整条件)。

单侧校验(仅「pin→失败」)会产生**假阳性**:若任务基线本身测试就是红的,
钉住后同样失败,会被误判为有效。完整条件必须**两侧都成立**:

  (a) pin  态(退掉修复)→ 验收**失败**   ⇒ 存在可修复的缺陷
  (b) fixed态(base_commit 原样)→ 验收**通过** ⇒ 修复确实让它变绿

只有 (a)∧(b) 都成立,任务才算有效 —— 这才是 SWE-bench 式的 FAIL_TO_PASS。

用法:
  python validate_two_sided.py --repo D:/claw-code-src \
    --tasks .../groundtruth_tasks.jsonl --out .../groundtruth_validated.jsonl
"""
from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import threading
import time
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import groundtruth_tasks as gt  # noqa: E402


def _acceptance_cmd(task: dict, own_tests: bool) -> str:
    """验收命令。`own_tests=True` 时只跑该任务**自身**的回归测试函数
    (SWE-bench FAIL_TO_PASS 语义),避免整 crate 中无关测试/环境依赖(需 Chromium、
    网络、计时)污染判定。任务 meta 无 test_functions 时回退整 crate。"""
    base = task["acceptance"]["command"]
    if not own_tests:
        return base
    fns = task.get("meta", {}).get("test_functions") or []
    if not fns:
        return base
    return f"{base} --lib -- {' '.join(fns)}"


def run_side(repo: str, task: dict, work_root: Path, side: str,
             timeout_s: int = 900, own_tests: bool = False) -> dict:
    """side ∈ {'pin','fixed'}。返回 {exit, ok_expected}。"""
    wt = work_root / f"{side}-{task['id']}"
    try:
        if side == "pin":
            gt.pin_task(repo, task, str(wt))
        else:
            gt.git(repo, "worktree", "add", "--detach", str(wt), task["base_commit"])
        t2 = dict(task)
        t2["acceptance"] = dict(task["acceptance"])
        t2["acceptance"]["command"] = _acceptance_cmd(task, own_tests)
        code, tail = gt.run_acceptance(t2, str(wt), repo, timeout_s=timeout_s)
        expect = task["acceptance"]["expect_exit"]
        if side == "pin":
            # 期望「失败」:非零(编译错或测试红都算,但需区分)。
            kind = gt.classify_pinned_failure(code, tail, expect)
            return {"exit": code, "ok": kind != "unexpected_pass", "kind": kind}
        # fixed 侧:期望「通过」。
        return {"exit": code, "ok": code == expect}
    except SystemExit as e:
        return {"exit": None, "ok": False, "kind": "pin_failed", "detail": str(e)[:160]}
    except Exception as e:  # noqa: BLE001
        return {"exit": None, "ok": False, "kind": "error", "detail": f"{type(e).__name__}: {e}"[:160]}
    finally:
        if wt.exists():
            subprocess.run(["git", "-C", repo, "worktree", "remove", "--force", str(wt)],
                           capture_output=True)


def validate_two_sided(repo: str, task: dict, work_root: Path,
                       timeout_s: int = 900, retries: int = 1,
                       own_tests: bool = False) -> dict:
    """双向校验。`retries`:对「不利侧」重试次数。

    必要性:验收命令是整 crate `cargo test -p <crate>`,其中可能含**与本任务无关的
    flaky 测试**(实测 runtime-f27e4026 的 fixed 侧因 `bash::tests::
    start_process_bg_returns_in_fixed_timeout_mode` 在 2090 测试并发下偶发超时,
    单跑 3/3 通过),导致判定在两轮间翻转。SWE-bench 官方 harness 对失败侧做多次
    重试取最优;此处同法:pin 侧「竟然通过」或 fixed 侧「编译/测试失败」时各再试。
    `own_tests`:只跑任务自身测试函数(FAIL_TO_PASS 语义),消除无关测试污染。
    """
    pin = run_side(repo, task, work_root, "pin", timeout_s=timeout_s, own_tests=own_tests)
    for _ in range(retries):
        if pin.get("ok"):
            break
        print(" pin-retry", flush=True, end="")
        pin = run_side(repo, task, work_root, "pin", timeout_s=timeout_s, own_tests=own_tests)
    fixed = run_side(repo, task, work_root, "fixed", timeout_s=timeout_s, own_tests=own_tests)
    for _ in range(retries):
        if fixed.get("ok"):
            break
        print(" fixed-retry", flush=True, end="")
        fixed = run_side(repo, task, work_root, "fixed", timeout_s=timeout_s, own_tests=own_tests)
    valid = bool(pin.get("ok")) and bool(fixed.get("ok"))
    return {
        "task_id": task["id"],
        "crate": task["crate"],
        "valid": valid,
        "pin_exit": pin.get("exit"),
        "pin_kind": pin.get("kind"),
        "fixed_exit": fixed.get("exit"),
        "verdict": "valid" if valid else _why(pin, fixed),
    }


def _why(pin: dict, fixed: dict) -> str:
    if not pin.get("ok"):
        return "pin_side_failed"  # 钉住后竟然通过/补丁打不上 → 无效
    if not fixed.get("ok"):
        return "baseline_red"  # 修复态也不过 → 基线本身红,无效
    return "unknown"


def _run_isolated(repo: str, args, task: dict) -> dict:
    """在**独立子进程**中校验单条任务。

    必要性:部分任务(如 runtime-4ebb08f5「后台服务检测 — 所有超时统一杀进程树」)
    的回归测试本身会调用 `taskkill`,若与主循环同进程运行,会把校验器/批处理一并杀掉
    (实测:批处理总在该任务处无 traceback 整体消失)。放进子进程后,毒性测试至多杀死
    自己的子进程,主循环检测到非零退出/超时即记 `isolated_killed`,继续下一条。
    """
    result_file = Path(repo) / ".gt-two-sided" / f"res-{task['id']}.json"
    if result_file.exists():
        result_file.unlink()
    cmd = [
        sys.executable, str(Path(__file__).resolve()),
        "--repo", repo, "--tasks", args.tasks, "--out", args.out,
        "--task-timeout", str(args.task_timeout),
        "--retries", str(args.retries),
        *(["--own-tests"] if args.own_tests else []),
        "--single-task", "--task-id", task["id"],
        "--result-file", str(result_file),
    ]
    try:
        proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            proc.wait(timeout=args.task_timeout * 2 + 120)
        except subprocess.TimeoutExpired:
            subprocess.run(["taskkill", "/F", "/T", "/PID", str(proc.pid)], capture_output=True)
            return {"task_id": task["id"], "crate": task["crate"], "valid": False,
                    "verdict": "isolated_timeout"}
    except Exception as e:  # noqa: BLE001
        return {"task_id": task["id"], "crate": task["crate"], "valid": False,
                "verdict": "runner_error", "detail": f"{type(e).__name__}: {e}"[:160]}
    if not result_file.exists():
        # 子进程被杀/崩溃且未落结果 —— 典型的毒性测试(自灭进程树)信号。
        return {"task_id": task["id"], "crate": task["crate"], "valid": False,
                "verdict": "isolated_killed", "child_exit": proc.returncode}
    r = json.loads(result_file.read_text(encoding="utf-8"))
    result_file.unlink(missing_ok=True)
    return r


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--tasks", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--task-timeout", type=int, default=600,
                    help="每侧 cargo 验收超时(秒);超过视为该类失败,防止挂起拖垮整批")
    ap.add_argument("--max-new", type=int, default=0,
                    help="本次最多新判定多少条(分批跑,0=不限);已缓存的不计入")
    ap.add_argument("--retries", type=int, default=1,
                    help="对不利侧(flaky 无关测试导致)的重试次数,默认 1")
    ap.add_argument("--own-tests", action="store_true",
                    help="只跑任务自身测试函数(FAIL_TO_PASS 语义),消除无关测试污染")
    ap.add_argument("--single-task", action="store_true",
                    help="仅校验 --task-id 一条,结果写 --result-file(供主循环隔离调用)")
    ap.add_argument("--task-id", default="")
    ap.add_argument("--result-file", default="")
    args = ap.parse_args()

    repo = gt.normalize_path(args.repo)
    tasks = [json.loads(l) for l in open(args.tasks, encoding="utf-8") if l.strip()]
    if args.limit:
        tasks = tasks[: args.limit]
    work_root = Path(repo) / ".gt-two-sided"

    # 心跳线程:cargo 编译/挂起时主线程静默,而沙箱 idle 监控在「5 分钟无输出」时
    # 会 SIGKILL 整个进程树。每 45s 打印一次心跳,使 stdout 永不静默。
    # **必须在 --single-task 分支之前启动** —— 隔离子进程也是本脚本,若心跳在其 return
    # 之后才起,子进程同样会静默被杀(实测:_run_isolated 的每条都静默消失)。
    _stop = threading.Event()

    def _heartbeat() -> None:
        while not _stop.wait(45):
            print(f"  [heartbeat {time.strftime('%H:%M:%S')}] still working...", flush=True)

    threading.Thread(target=_heartbeat, daemon=True).start()

    # 单条隔离模式:只跑一条,结果写文件。**不**清理 work_root(避免删掉兄弟任务的 worktree)。
    if args.single_task:
        work_root.mkdir(parents=True, exist_ok=True)
        t = next(x for x in tasks if x["id"] == args.task_id)
        r = validate_two_sided(repo, t, work_root, timeout_s=args.task_timeout,
                               retries=args.retries, own_tests=args.own_tests)
        Path(args.result_file).write_text(json.dumps(r, ensure_ascii=False), encoding="utf-8")
        return 0

    if work_root.exists():
        shutil.rmtree(work_root)
    work_root.mkdir(parents=True)

    print(f"two-sided validation: {len(tasks)} tasks (repo={repo})", flush=True)

    # 增量落盘 + 断点续跑:后台长任务可能中途被杀(实测 24/40 处死掉,成果全丢)。
    # 每条完成即追加到 `.partial.jsonl`,重启时先读回已完成项跳过。
    partial = Path(args.out).with_suffix(".partial.jsonl")
    done: dict[str, dict] = {}
    if partial.exists():
        for line in open(partial, encoding="utf-8"):
            line = line.strip()
            if line:
                r = json.loads(line)
                done[r["task_id"]] = r
        print(f"resume: {len(done)} task(s) already done, skipping", flush=True)

    results: list[dict] = []
    new_done = 0
    with open(partial, "a", encoding="utf-8") as pf:
        for i, t in enumerate(tasks, 1):
            if t["id"] in done:
                results.append(done[t["id"]])
                print(f"[{i}/{len(tasks)}] {t['id']} ... (cached)", flush=True)
                continue
            if args.max_new and new_done >= args.max_new:
                # 分批模式:已达本次上限,剩余留待下次续跑(避免单次静默窗口过长被 idle 杀)。
                print(f"batch limit reached ({args.max_new}); stopping for resume", flush=True)
                break
            print(f"[{i}/{len(tasks)}] {t['id']} ...", flush=True, end="")
            r = _run_isolated(repo, args, t)
            results.append(r)
            new_done += 1
            pf.write(json.dumps(r, ensure_ascii=False) + "\n")
            pf.flush()
            print(f" valid={r['valid']} ({r['verdict']})", flush=True)

    # 按任务顺序对齐(续跑时 results 已按 tasks 顺序填充)
    by_id = {r["task_id"]: r for r in results}
    ordered = [by_id[t["id"]] for t in tasks if t["id"] in by_id]
    valid = [t for t in tasks if by_id.get(t["id"], {}).get("valid")]
    with open(args.out, "w", encoding="utf-8") as fh:
        for t in valid:
            fh.write(json.dumps(t, ensure_ascii=False) + "\n")

    verdicts = Counter(r["verdict"] for r in ordered)
    print("\n=== TWO-SIDED SUMMARY ===", flush=True)
    print(f"total={len(results)}  valid={len(valid)}  invalid={len(results) - len(valid)}", flush=True)
    print(f"verdicts={dict(verdicts)}", flush=True)
    print(f"wrote {len(valid)} valid tasks -> {args.out}", flush=True)
    with open(args.out + ".report.json", "w", encoding="utf-8") as fh:
        json.dump({"summary": {"total": len(results), "valid": len(valid),
                               "verdicts": dict(verdicts)}, "results": results},
                  fh, ensure_ascii=False, indent=2)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
