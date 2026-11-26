#!/usr/bin/env python3
"""Token 基线测量脚本 —— 量化 system / tools / messages 尾部 的 token 成本。

为什么需要它：claw 的请求体里，不同区域**计价方式不同**。
- system + tools 落在 prompt cache 断点之内 ⇒ 按「命中价」计费（约未命中的 1/10）
- messages 尾部每轮重新注入的块 ⇒ 按「全价」计费

因此「哪块 token 多」不等于「哪块贵」。本脚本按**加权成本**排序，
避免把优化精力花在便宜的静态区上。

用法：
    python scripts/token_baseline.py --latest
    python scripts/token_baseline.py --input <request-dump.json> --output out.json
    python scripts/token_baseline.py --latest --json

输入格式：prompt-cache dump（含 current_request.system/tools/messages），
或裸的请求体（含 system/tools/messages）。

token 估算：CJK 字符按 1 token/字，其余按 3.6 字符/token。
这是**保守估算**（对中文偏保守），用于横向比较，不作计费依据。
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import sys

CACHE_HIT_DISCOUNT = 0.1  # 命中价 ≈ 未命中价的 1/10

# 尾部注入块的识别特征（顺序即槽位顺序）
TAIL_MARKERS = [
    ("固定记忆", "# 固定记忆"),
    ("Project context", "# Project context"),
    ("Runtime Context", "# Runtime Context"),
    ("设计决策与证据", "# 设计决策与实验证据"),
]

SLOT_MARKERS = [
    ("Notebook", "## Notebook"),
    ("Active Plan", "## Active Plan"),
    ("Step Status", "## Step Status"),
    ("Current Task", "## Current Task"),
    ("Semantic Memory Recall", "## Semantic Memory Recall"),
    ("Verification Remediation", "## Verification Remediation"),
    ("Cognitive Stall", "## Cognitive Stall"),
    ("Failure Advice", "## Failure Advice"),
    ("Previous Session Digest", "## Previous Session Digest"),
    ("Compaction Notice", "## Compaction Notice"),
]


def est_tokens(text: str) -> float:
    """CJK 感知的 token 估算。"""
    if not text:
        return 0.0
    cjk = sum(
        1
        for ch in text
        if "\u4e00" <= ch <= "\u9fff"      # CJK 统一表意
        or "\u3000" <= ch <= "\u303f"      # CJK 标点
        or "\uff00" <= ch <= "\uffef"      # 全角字符
    )
    return cjk + (len(text) - cjk) / 3.6


def block_text(block) -> str:
    if isinstance(block, str):
        return block
    if isinstance(block, dict):
        return block.get("text") or ""
    return ""


def msg_text(msg) -> str:
    content = msg.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(block_text(b) for b in content)
    return ""


def load_request(path: str) -> dict:
    with open(path, encoding="utf-8") as fh:
        data = json.load(fh)
    # prompt-cache dump 包了一层 current_request
    if "current_request" in data:
        return data["current_request"]
    return data


def find_latest_dump() -> str:
    """在 ~/.claude/cache/prompt-cache 下找最新的请求 dump。"""
    home = os.path.expanduser("~")
    pattern = os.path.join(home, ".claude", "cache", "prompt-cache", "*", "dumps", "req-*.json")
    files = glob.glob(pattern)
    if not files:
        sys.exit(f"未找到请求 dump：{pattern}\n请用 --input 显式指定。")
    return max(files, key=os.path.getmtime)


def analyze(request: dict) -> dict:
    system = request.get("system") or []
    tools = request.get("tools") or []
    messages = request.get("messages") or []

    # ---- 缓存覆盖范围：最后一个带 cache_control 的位置 ----
    sys_cc = [i for i, b in enumerate(system) if isinstance(b, dict) and b.get("cache_control")]
    tool_cc = [i for i, t in enumerate(tools) if isinstance(t, dict) and t.get("cache_control")]

    system_tokens = sum(est_tokens(block_text(b)) for b in system)
    tools_tokens = sum(est_tokens(json.dumps(t, ensure_ascii=False)) for t in tools)
    messages_tokens = sum(est_tokens(json.dumps(m, ensure_ascii=False)) for m in messages)

    # system + tools 是否整体被缓存：tools 末尾有断点即视为整体命中
    system_tools_cached = bool(tool_cc) and tool_cc[-1] >= len(tools) - 1

    # ---- 尾部注入块（全价区）----
    # 关键：只有**末尾追加区**才是每轮新增、无法命中缓存的部分。
    # 早期消息（如 msg[0] 的冻结段）位于 messages 头部，属 DeepSeek 自动
    # 前缀缓存覆盖范围，按命中价计费 —— 计入全价会严重高估成本。
    # 判定：取每个 marker 出现的**最大** msg_index，且必须落在末尾窗口内。
    tail_window = 8
    tail_start = max(0, len(messages) - tail_window)
    tail_blocks = []
    for name, marker in TAIL_MARKERS:
        hit_idx = None
        for idx, msg in enumerate(messages):
            if marker in msg_text(msg):
                hit_idx = idx
        if hit_idx is not None and hit_idx >= tail_start:
            text = msg_text(messages[hit_idx])
            tail_blocks.append(
                {
                    "name": name,
                    "msg_index": hit_idx,
                    "chars": len(text),
                    "tokens": round(est_tokens(text), 1),
                }
            )

    # ---- Runtime Context 槽位拆解 ----
    slots = []
    for msg in messages:
        text = msg_text(msg)
        if "# Runtime Context" not in text:
            continue
        body = text.split("# Runtime Context", 1)[1]
        positions = []
        for name, marker in SLOT_MARKERS:
            pos = body.find(marker)
            if pos >= 0:
                positions.append((pos, name))
        positions.sort()
        for i, (pos, name) in enumerate(positions):
            end = positions[i + 1][0] if i + 1 < len(positions) else len(body)
            seg = body[pos:end]
            slots.append(
                {
                    "name": name,
                    "chars": len(seg),
                    "tokens": round(est_tokens(seg), 1),
                }
            )

    # ---- 重复检测：同一段长文本在 system 与 messages 都出现 ----
    duplicates = []
    sys_texts = [block_text(b) for b in system if len(block_text(b)) > 200]
    for idx, msg in enumerate(messages):
        text = msg_text(msg)
        if len(text) < 200:
            continue
        for st in sys_texts:
            # 取 system 段落的首个标题行做特征比对
            head = next((ln for ln in st.splitlines() if ln.startswith("#")), "")
            if head and head in text and len(text) > 200:
                duplicates.append(
                    {
                        "heading": head,
                        "system_chars": len(st),
                        "message_index": idx,
                        "message_chars": len(text),
                        "note": "同名段落在 system 与 messages 尾部各存一份",
                    }
                )
                break

    tail_tokens = sum(b["tokens"] for b in tail_blocks)

    # ---- 重复模板检测：同一段协议正文按步重复 ----
    repeated_templates = []
    for msg in messages:
        text = msg_text(msg)
        if "# Runtime Context" not in text:
            continue
        for probe in ("PRE-COMMITMENT REQUIRED", "acceptance:", "verify:"):
            n = text.count(probe)
            if n > 1:
                repeated_templates.append({"marker": probe, "occurrences": n})
        break

    # ---- 固定字段：供 step_8 前后对比（键名即验收口径）----
    def slot_tokens(name: str) -> float:
        return next((s["tokens"] for s in slots if s["name"] == name), 0.0)

    project_context_tokens = next(
        (b["tokens"] for b in tail_blocks if b["name"] == "Project context"), 0.0
    )
    baseline = {
        "total_tail_tokens": round(tail_tokens, 1),
        "active_plan_tokens": slot_tokens("Active Plan"),
        "current_task_tokens": slot_tokens("Current Task"),
        "project_context_tokens": round(project_context_tokens, 1),
        "notebook_tokens": slot_tokens("Notebook"),
        "step_status_tokens": slot_tokens("Step Status"),
        "duplicate_markers": {
            "cross_region_duplicates": len(duplicates),
            "repeated_templates": repeated_templates,
        },
    }

    # ---- 加权成本（关键：命中价 vs 全价）----
    cached_tokens = system_tokens + tools_tokens if system_tools_cached else 0.0
    weighted_cached = cached_tokens * CACHE_HIT_DISCOUNT
    weighted_tail = tail_tokens  # 尾部每轮重注入 ⇒ 全价

    return {
        "source": "request dump",
        "counts": {
            "system_blocks": len(system),
            "tools": len(tools),
            "messages": len(messages),
        },
        "cache_layout": {
            "system_cache_control_at": sys_cc,
            "tools_cache_control_at": tool_cc,
            "system_tools_fully_cached": system_tools_cached,
        },
        "raw_tokens": {
            "system": round(system_tokens, 1),
            "tools": round(tools_tokens, 1),
            "messages": round(messages_tokens, 1),
        },
        "weighted_cost": {
            "system_plus_tools_at_hit_price": round(weighted_cached, 1),
            "messages_tail_at_full_price": round(weighted_tail, 1),
            "note": "尾部块每轮重新注入，无法命中缓存，按全价计",
        },
        "tail_blocks": tail_blocks,
        "tail_total_tokens": round(tail_tokens, 1),
        "runtime_context_slots": slots,
        "duplicates": duplicates,
        "baseline": baseline,
    }


def print_report(result: dict) -> None:
    c = result["counts"]
    raw = result["raw_tokens"]
    w = result["weighted_cost"]
    layout = result["cache_layout"]

    print("=" * 68)
    print("Token 基线")
    print("=" * 68)
    print(f"system 块 {c['system_blocks']} 个 / tools {c['tools']} 个 / messages {c['messages']} 条")
    print(f"缓存断点：system@{layout['system_cache_control_at']}  tools@{layout['tools_cache_control_at']}")
    print(f"system+tools 整体命中缓存：{layout['system_tools_fully_cached']}")
    print()
    print(f"{'区域':<28}{'原始 tok':>12}{'加权成本':>12}")
    print("-" * 68)
    print(f"{'system':<28}{raw['system']:>12,.0f}{'命中价' if layout['system_tools_fully_cached'] else '全价':>12}")
    print(f"{'tools':<28}{raw['tools']:>12,.0f}{'命中价' if layout['system_tools_fully_cached'] else '全价':>12}")
    print(f"{'messages(含尾部注入)':<28}{raw['messages']:>12,.0f}{'混合':>12}")
    print("-" * 68)
    print(f"{'system+tools 加权':<28}{'':>12}{w['system_plus_tools_at_hit_price']:>12,.0f}")
    print(f"{'尾部注入加权(全价)':<28}{'':>12}{w['messages_tail_at_full_price']:>12,.0f}")
    print()

    if result["tail_blocks"]:
        print("尾部注入块（全价区）：")
        for b in result["tail_blocks"]:
            print(f"  msg[{b['msg_index']:>3}] {b['tokens']:>7,.0f} tok  {b['name']}")
        print(f"  合计 {result['tail_total_tokens']:,.0f} tok/轮")
        print()

    if result["runtime_context_slots"]:
        print("Runtime Context 槽位：")
        for s in result["runtime_context_slots"]:
            print(f"  {s['tokens']:>7,.0f} tok  {s['name']}")
        print()

    if result["duplicates"]:
        print("⚠ 检测到跨区重复：")
        for d in result["duplicates"]:
            print(f"  {d['heading']}  system {d['system_chars']}ch vs msg[{d['message_index']}] {d['message_chars']}ch")
        print()

    verdict = "尾部全价 > 静态区命中价" if w["messages_tail_at_full_price"] > w["system_plus_tools_at_hit_price"] else "静态区仍是主要成本"
    print(f"结论：{verdict}")


def main() -> None:
    ap = argparse.ArgumentParser(description="测量 claw 请求体的 token 成本（命中价 vs 全价）")
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--input", help="请求 dump JSON 路径")
    src.add_argument("--latest", action="store_true", help="自动取最新 dump")
    ap.add_argument("--output", help="结果写入 JSON 文件")
    ap.add_argument("--json", action="store_true", help="仅输出 JSON")
    args = ap.parse_args()

    path = find_latest_dump() if args.latest else args.input
    result = analyze(load_request(path))
    result["input_path"] = path

    if args.output:
        # Windows 下 `/tmp/x.json` 会被解析为 `D:\tmp\x.json`；父目录未必存在。
        parent = os.path.dirname(os.path.abspath(args.output))
        if parent:
            os.makedirs(parent, exist_ok=True)
        with open(args.output, "w", encoding="utf-8") as fh:
            json.dump(result, fh, ensure_ascii=False, indent=2)
        if not args.json:
            print(f"已写入 {args.output}")

    if args.json:
        print(json.dumps(result, ensure_ascii=False, indent=2))
    elif not args.output:
        print_report(result)
    else:
        print_report(result)


if __name__ == "__main__":
    main()
