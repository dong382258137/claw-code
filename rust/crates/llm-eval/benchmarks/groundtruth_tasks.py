#!/usr/bin/env python3
"""路径 C:从本仓库 git 历史自建 ground-truth 任务集(零依赖,仅内部回归)。

# 原理:回归钉(regression-pin)

等价 SWE-bench 的 FAIL_TO_PASS 机制,但**不需要 Docker**,也**不要求测试
位于独立 tests/ 目录** —— 本仓库的测试大多是内联的(`#[cfg(test)] mod tests`
写在 src/*.rs 内),因此按**代码块(hunk)级**而非文件级做还原:

  1. 扫 git 历史,找「同一次提交里既有源码修改、又新增了 `#[test]`」的提交 C
     —— 这类提交通常 = 「修复缺陷 + 补回归测试」;
  2. 还原:在 C 的隔离 worktree 上,**反向应用「非测试 hunk」**(即把修复代码
     退掉),测试 hunk 原样保留 → 该回归测试此刻**必然失败**;
  3. 任务 prompt = C 的提交信息(症状描述,不泄露解法);
  4. 客观判据 = `cargo test -p <crate>` 退出码为 0。

判据是 cargo 退出码(客观、可执行、不可被模型自评刷高)。

# 子命令

  build   扫描历史 → 生成任务集 JSONL
  pin     在隔离 worktree 还原缺陷(退修复、留测试)
  verify  在(模型已编辑的)worktree 跑验收命令 → 判定 resolved
  validate 自检:pin 后验收命令应**失败**(证明任务有效/FAIL_TO_PASS)

# 诚实性说明(重要)

- 本任务集**仅用于本项目内部回归**,不可与 SWE-bench 官方分数比较(环境不同)。
- hunk 分类用启发式(新增行含 `#[test]`/`mod tests` 视为测试 hunk);若某 hunk
  混合了测试与修复,该任务可能无效 —— 用 `validate` 检出并剔除。
- prompt 不含 gold patch;判据是 cargo 退出码。
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

TEST_MARKERS = ("#[test]", "#[cfg(test)]", "mod tests")


def is_test_path(path: str) -> bool:
    return (
        re.search(r"(^|/)tests?/", path) is not None
        or re.search(r"(^|/)test_[^/]*\.rs$", path) is not None
        or path.endswith("_test.rs")
        or path.endswith("_tests.rs")
    )


def is_rust_source(path: str) -> bool:
    return path.endswith(".rs")


def crate_of(path: str) -> str | None:
    m = re.match(r"rust/crates/([^/]+)/", path)
    return m.group(1) if m else None


def git(repo: str, *args: str) -> str:
    out = subprocess.run(
        ["git", "-C", repo, *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    if out.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)}: {out.stderr.strip()}")
    return out.stdout


def parse_diff(diff_text: str) -> list[dict]:
    """把 unified diff 拆成 [{header:[...], hunks:[[...],...]}, ...](按文件)。"""
    files: list[dict] = []
    cur: dict | None = None
    cur_hunk: list[str] | None = None
    for line in diff_text.splitlines():
        if line.startswith("diff --git"):
            cur = {"header": [line], "hunks": []}
            files.append(cur)
            cur_hunk = None
        elif cur is None:
            continue
        elif line.startswith("@@"):
            cur_hunk = [line]
            cur["hunks"].append(cur_hunk)
        elif cur_hunk is None:
            cur["header"].append(line)
        else:
            cur_hunk.append(line)
    return files


def hunk_added_lines(hunk: list[str]) -> str:
    return "\n".join(ln for ln in hunk[1:] if ln.startswith("+"))


def is_test_hunk(hunk: list[str]) -> bool:
    added = hunk_added_lines(hunk)
    return any(marker in added for marker in TEST_MARKERS)


def test_fns_in_hunk(hunk: list[str]) -> list[str]:
    names = []
    for ln in hunk[1:]:
        if not ln.startswith("+"):
            continue
        m = re.search(r"\bfn\s+([a-zA-Z0-9_]+)\s*\(", ln)
        if m:
            names.append(m.group(1))
    return names


def build_reverse_patch(repo: str, sha: str, files: list[str]) -> tuple[str, list[str], int]:
    """构造「仅非测试 hunk」的反向补丁 → (patch_text, test_fns, non_test_hunk_count)。"""
    diff = git(repo, "show", sha, "--", *files)
    out_lines: list[str] = []
    test_fns: list[str] = []
    non_test = 0
    for f in parse_diff(diff):
        kept = []
        for h in f["hunks"]:
            if is_test_hunk(h):
                test_fns.extend(test_fns_in_hunk(h))
            else:
                kept.append(h)
                non_test += 1
        if kept:
            out_lines.extend(f["header"])
            for h in kept:
                out_lines.extend(h)
    return "\n".join(out_lines) + ("\n" if out_lines else ""), sorted(set(test_fns)), non_test


def build_tasks(repo: str, limit: int, scan: int) -> list[dict]:
    log = git(repo, "log", f"-{scan}", "--pretty=format:%H\x1f%s")
    commits = [l.split("\x1f", 1) for l in log.splitlines() if "\x1f" in l]

    tasks: list[dict] = []
    for sha, subject in commits:
        if len(tasks) >= limit:
            break
        files = [f for f in git(repo, "show", "--name-only", "--pretty=format:", sha).split() if f]
        src = [f for f in files if is_rust_source(f)]
        if not src:
            continue
        crates = {crate_of(f) for f in src if crate_of(f)}
        if len(crates) != 1:
            continue
        crate = next(iter(crates))
        patch, test_fns, non_test = build_reverse_patch(repo, sha, src)
        # 必须:有非测试 hunk(可退的修复) + 有测试 hunk(可留的回归钉)。
        if non_test == 0 or not test_fns or not patch:
            continue
        tasks.append(
            {
                "id": f"{crate}-{sha[:8]}",
                "benchmark": "groundtruth-regression-pin",
                "crate": crate,
                "base_commit": sha,
                "revert_files": sorted(src),
                "acceptance": {"command": f"cargo test -p {crate}", "expect_exit": 0},
                "prompt": (
                    f"本仓库存在一个已修复的缺陷(症状见下)。请定位并修复它,"
                    f"使回归测试通过。\n\n症状(摘自提交信息):\n{subject}\n\n"
                    f"约束:`cargo test -p {crate}` 必须通过;不得修改测试来让它变绿。"
                ),
                "meta": {
                    "gold_subject": subject,
                    "test_functions": test_fns,
                    "non_test_hunks": non_test,
                },
            }
        )
    return tasks


def load_task(path: str, task_id: str) -> dict:
    for line in open(path, encoding="utf-8"):
        line = line.strip()
        if line and json.loads(line)["id"] == task_id:
            return json.loads(line)
    raise SystemExit(f"task not found: {task_id}")


def cmd_build(a: argparse.Namespace) -> int:
    tasks = build_tasks(a.repo, a.limit, a.scan)
    with open(a.out, "w", encoding="utf-8") as fh:
        for t in tasks:
            fh.write(json.dumps(t, ensure_ascii=False) + "\n")
    print(f"wrote {len(tasks)} ground-truth tasks -> {a.out}", file=sys.stderr)
    for t in tasks[:6]:
        print(
            f"  {t['id']:30s} tests={len(t['meta']['test_functions']):2d} "
            f"hunks={t['meta']['non_test_hunks']:2d}  {t['meta']['gold_subject'][:48]}",
            file=sys.stderr,
        )
    return 0


def pin_task(repo: str, task: dict, worktree: str) -> None:
    wt = Path(worktree)
    if wt.exists():
        raise SystemExit(f"worktree already exists: {wt}")
    git(repo, "worktree", "add", "--detach", str(wt), task["base_commit"])
    patch, _fns, _n = build_reverse_patch(repo, task["base_commit"], task["revert_files"])
    # `--recount`:剔除部分 hunk 后行号不再自洽,让 git 按 hunk 体重新计数。
    proc = subprocess.run(
        ["git", "-C", str(wt), "apply", "-R", "--recount", "--whitespace=nowarn", "-"],
        input=patch, text=True, capture_output=True, encoding="utf-8", errors="replace",
    )
    if proc.returncode != 0:
        raise SystemExit(f"reverse apply failed:\n{proc.stderr}")


def cmd_pin(a: argparse.Namespace) -> int:
    task = load_task(a.tasks, a.task_id)
    pin_task(a.repo, task, a.worktree)
    print(f"pinned {task['id']} at {a.worktree} (fix reverted, tests kept)", file=sys.stderr)
    print(task["acceptance"]["command"], file=sys.stderr)
    return 0


def normalize_path(p: str) -> str:
    """把 git-bash/MSYS 风格路径(`/d/foo`, `/c/Users/...`)归一化为 Windows 路径。

    Python(Windows 原生)不认 MSYS 盘符挂载点,而用户惯用 `git-bash` 路径传参。
    若不做归一化,`Path('/d/x/rust/Cargo.toml').exists()` 恒为 False → 验收命令
    cwd 落到错误目录 → cargo 报 "could not find Cargo.toml" → 被误判为「验收失败」
    (假阳性)。
    """
    if len(p) >= 2 and p[0] == "/" and p[1].isalpha() and (len(p) == 2 or p[2] == "/"):
        return f"{p[1].upper()}:{p[2:] or '/'}"
    return p


def workspace_root(worktree: str) -> str:
    """验收命令的工作目录。

    cargo workspace 位于 `<repo>/rust/`(crate 路径形如 `rust/crates/<name>/...`),
    故必须在此目录下执行 `cargo test`,否则报 "could not find Cargo.toml"
    —— 那会被误判为「验收失败」从而把无效任务当成有效。
    """
    wt = normalize_path(worktree)
    rust_dir = Path(wt) / "rust"
    return str(rust_dir) if (rust_dir / "Cargo.toml").exists() else wt


def shared_target_dir(repo: str) -> str:
    """复用主仓库 target 目录,避免每个 worktree 重复编译依赖(显著省时)。"""
    return str(Path(normalize_path(repo)) / "rust" / "target")


def run_acceptance(
    task: dict, worktree: str, repo: str | None = None, timeout_s: int = 900
) -> tuple[int, str]:
    """执行验收命令 → (退出码, 输出尾部)。输出用于区分「编译失败」与「测试失败」。

    `timeout_s` 防护:个别任务 pin 后可能**挂起**(实测 `runtime-1aa64fc9` 两次运行
    都精确卡死在该任务,导致整个批处理停滞)。超时视为验收失败(退出码 -1),
    使批处理能继续推进而非无限等待。
    """
    cmd = task["acceptance"]["command"]
    env = None
    if repo:
        import os

        env = dict(os.environ)
        env["CARGO_TARGET_DIR"] = shared_target_dir(repo)
    # 关键:输出重定向到**临时文件**而非管道。
    # 若用 `subprocess.run(capture_output=True)`,当子进程(cargo)派生的孙进程(测试)
    # 挂起并持有 stdout 管道时,`subprocess.run` 即使在 timeout 触发后仍会**永久阻塞**
    # 等待管道 EOF —— 这是 Windows 上实测到的真正死锁(导致批处理永远停在某条任务)。
    # 改文件重定向后,`proc.wait()` 只等直接子进程;超时即 `taskkill /T` 杀整棵进程树。
    import tempfile

    with tempfile.TemporaryFile(mode="w+", encoding="utf-8", errors="replace") as outf:
        proc = subprocess.Popen(
            cmd.split(), cwd=workspace_root(worktree),
            stdout=outf, stderr=subprocess.STDOUT, env=env,
        )
        try:
            proc.wait(timeout=timeout_s)
        except subprocess.TimeoutExpired:
            _kill_tree(proc)
            return -1, f"TIMEOUT after {timeout_s}s: {cmd}"
        outf.seek(0)
        data = outf.read()
    return proc.returncode, data[-6000:]


def _kill_tree(proc: "subprocess.Popen") -> None:
    """杀掉整个进程树(Windows 上 cargo 会派生 rustc/测试孙进程,单杀直接子进程不够)。"""
    try:
        subprocess.run(["taskkill", "/F", "/T", "/PID", str(proc.pid)], capture_output=True)
    except Exception:  # noqa: BLE001
        try:
            proc.kill()
        except Exception:  # noqa: BLE001
            pass


def classify_pinned_failure(code: int, output: str, expect_exit: int) -> str:
    """把「pin 后的失败」分类,便于判断任务有效性。

    - `test_failure`  : 编译通过但测试失败(理想的 FAIL_TO_PASS);
    - `compile_error` : 源码回退导致编译失败(仍是有效 FAIL_TO_PASS,但需模型修复
                        使之可编译;比 test_failure 弱,单独标注);
    - `unexpected_pass`: pin 后仍通过 ⇒ 任务**无效**(未真正注入缺陷)。
    """
    if code == expect_exit:
        return "unexpected_pass"
    if "could not compile" in output or "error[" in output or "error:" in output:
        return "compile_error"
    return "test_failure"


def cmd_verify(a: argparse.Namespace) -> int:
    task = load_task(a.tasks, a.task_id)
    code, _out = run_acceptance(task, a.worktree, a.repo)
    resolved = code == task["acceptance"]["expect_exit"]
    print(json.dumps({"task_id": task["id"], "resolved": resolved, "exit_code": code}))
    return 0 if resolved else 1


def cmd_validate(a: argparse.Namespace) -> int:
    """自检:pin 后验收命令必须**失败**(FAIL_TO_PASS),否则任务无效。"""
    task = load_task(a.tasks, a.task_id)
    wt = Path(a.worktree)
    try:
        pin_task(a.repo, task, str(wt))
        code, out = run_acceptance(task, str(wt), a.repo)
        kind = classify_pinned_failure(code, out, task["acceptance"]["expect_exit"])
        valid = kind != "unexpected_pass"
        print(json.dumps({"task_id": task["id"], "valid": valid,
                          "kind": kind, "pinned_exit": code}, ensure_ascii=False))
        return 0 if valid else 2
    finally:
        if wt.exists():
            subprocess.run(["git", "-C", a.repo, "worktree", "remove", "--force", str(wt)],
                           capture_output=True)


def main() -> int:
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)

    b = sub.add_parser("build")
    b.add_argument("--repo", required=True)
    b.add_argument("--limit", type=int, default=20)
    b.add_argument("--scan", type=int, default=400)
    b.add_argument("--out", required=True)
    b.set_defaults(fn=cmd_build)

    for name, fn in (("pin", cmd_pin), ("verify", cmd_verify), ("validate", cmd_validate)):
        p = sub.add_parser(name)
        p.add_argument("--repo", required=True)
        p.add_argument("--tasks", required=True)
        p.add_argument("--task-id", required=True)
        p.add_argument("--worktree", required=True)
        p.set_defaults(fn=fn)

    args = ap.parse_args()
    return args.fn(args)


if __name__ == "__main__":
    raise SystemExit(main())
