#!/usr/bin/env python3
"""拉取权威 agent 评测任务集 → 转成 llm-eval 可消费的 JSONL。

来源(均为业界被一致认定、leaderboard 在用的基准):
  - SWE-bench Verified (princeton-nlp / SWE-bench)  → resolved rate / pass@1
  - 通过 HuggingFace datasets-server REST 免 Docker 获取任务定义

产出字段:
  id            = instance_id
  name          = repo + 难度
  prompt        = problem_statement(真实 issue 文本,逐字)
  rubric        = 客观判据说明(FAIL_TO_PASS / PASS_TO_PASS)
  golden/meta   = 保留 base_commit / repo / FAIL_TO_PASS / PASS_TO_PASS 供评分器使用

用法:
  python fetch_authoritative_tasks.py --dataset swebench_verified --limit 20 --out <path>.jsonl
"""
from __future__ import annotations

import argparse
import json
import sys
import urllib.parse
import urllib.request

HF = "https://datasets-server.huggingface.co/rows"

DATASETS = {
    # 权威 SWE-bench Verified:500 条人审子集,leaderboard 标准任务集。
    "swebench_verified": {
        "dataset": "SWE-bench/SWE-bench_Verified",
        "config": "default",
        "split": "test",
    },
    # SWE-bench Lite:300 条轻量子集(更易本地运行)。
    "swebench_lite": {
        "dataset": "SWE-bench/SWE-bench_Lite",
        "config": "default",
        "split": "test",
    },
}


def fetch_rows(ds: str, cfg: str, split: str, limit: int) -> list[dict]:
    rows: list[dict] = []
    offset = 0
    page = 100
    while len(rows) < limit:
        n = min(page, limit - len(rows))
        q = urllib.parse.urlencode(
            {"dataset": ds, "config": cfg, "split": split, "offset": offset, "length": n}
        )
        with urllib.request.urlopen(f"{HF}?{q}", timeout=60) as resp:
            payload = json.load(resp)
        batch = payload.get("rows", [])
        if not batch:
            break
        rows.extend(r["row"] for r in batch)
        offset += len(batch)
    return rows


def sample_diverse(rows: list[dict], limit: int) -> list[dict]:
    """跨仓库均匀抽样。

    SWE-bench 在数据集中按 repo 连续排列,直接取前 N 条会全落在同一仓
    (如 astropy)。这里先按 repo 分桶,再轮转取样,保证任务多样性 ——
    单仓库样本无法代表整体难度分布。
    """
    if limit >= len(rows):
        return rows
    buckets: dict[str, list[dict]] = {}
    for r in rows:
        buckets.setdefault(r["repo"], []).append(r)
    out: list[dict] = []
    # 轮转:每轮从各 repo 取 1 条,直到凑够 limit。
    while len(out) < limit:
        progressed = False
        for repo in list(buckets):
            if buckets[repo] and len(out) < limit:
                out.append(buckets[repo].pop(0))
                progressed = True
        if not progressed:
            break
    return out


def to_case(row: dict) -> dict:
    """SWE-bench 行 → llm-eval TaskCase 兼容条目 + 客观判据元数据。"""
    f2p = row.get("FAIL_TO_PASS") or []
    p2p = row.get("PASS_TO_PASS") or []
    if isinstance(f2p, str):
        f2p = json.loads(f2p)
    if isinstance(p2p, str):
        p2p = json.loads(p2p)
    return {
        "id": row["instance_id"],
        "name": f"{row['repo']} @ {row.get('difficulty', 'n/a')}",
        "prompt": row["problem_statement"],
        # rubric:客观判据的人类可读说明(不参与关键词匹配,供评分器/复核用)。
        "rubric": (
            "RESOLVED iff ALL FAIL_TO_PASS tests pass AND ALL PASS_TO_PASS tests still pass. "
            f"FAIL_TO_PASS={len(f2p)} tests, PASS_TO_PASS={len(p2p)} tests."
        ),
        "min_output_chars": 1,
        # 以下为客观评分器所需元数据(非 llm-eval 标准字段,评测器读取)。
        "meta": {
            "benchmark": "swe-bench",
            "repo": row["repo"],
            "base_commit": row["base_commit"],
            "environment_setup_commit": row.get("environment_setup_commit"),
            "version": row.get("version"),
            "image": row.get("image"),
            "FAIL_TO_PASS": f2p,
            "PASS_TO_PASS": p2p,
            "eval_type": row.get("eval_type"),
            "gold_patch": row.get("patch"),
            "test_patch": row.get("test_patch"),
        },
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dataset", default="swebench_verified", choices=sorted(DATASETS))
    ap.add_argument("--limit", type=int, default=20)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    spec = DATASETS[args.dataset]
    # 先拉取更大池(上限 500 = SWE-bench Verified 全集),再跨仓库均匀抽样,
    # 避免按 repo 连续排布导致的"整批同仓库"偏差。
    pool_size = max(args.limit, min(500, args.limit * 15))
    rows = fetch_rows(spec["dataset"], spec["config"], spec["split"], pool_size)
    rows = sample_diverse(rows, args.limit)
    cases = [to_case(r) for r in rows]
    with open(args.out, "w", encoding="utf-8") as fh:
        for c in cases:
            fh.write(json.dumps(c, ensure_ascii=False) + "\n")
    distinct = len({c["meta"]["repo"] for c in cases})
    print(f"wrote {len(cases)} cases -> {args.out}", file=sys.stderr)
    print(f"distinct repos = {distinct}", file=sys.stderr)
    if cases:
        c = cases[0]
        print(
            f"sample: id={c['id']} f2p={len(c['meta']['FAIL_TO_PASS'])} "
            f"p2p={len(c['meta']['PASS_TO_PASS'])}",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
