# -*- coding: utf-8 -*-
"""打回率基线扫描器(零 LLM,确定性规则) — 收敛契约验收的前置数据。

口径(对齐 docs/2026-09-05-convergence-contract-design.md §6.1):
- turn = 一条"任务性" user 消息(非纯承接词、长度>=4字符),且其前存在 assistant 最终答复
  (该 assistant 消息含 text 且无挂起 tool_use —— 即模型已收尾)。
- 打回(明确) = 该 user 消息命中打回特征词(质疑/纠错/未完成语义)。
- 打回(疑似) = 未命中明确词,但命中"再/继续/还有"类模糊推进词。
- 打回率 = 命中数 / turn 数。只扫主会话文件(非 .rot-),避免轮转归档重复计数。
"""
import json, sys, glob, os

# 纯承接词(复用 task_state.rs ACK_WORDS 精神;不视为新 turn 输入)
ACK_SET = {
    "好","好的","好呀","好嘞","好勒","ok","okay","嗯","嗯嗯","收到","知道了","明白了","明白",
    "可以","行","行吧","好吧","好滴","没问题","了解","了解了","晓得了","是","对","不错",
    "继续","继续吧","继续做","继续干","随便","随意","都行","都行吧","无所谓","谢谢","感谢",
}
# 强打回词(窄口径):直接质疑上次收尾结果 / 要求重做 / 未解决 / 无输出
STRONG_PUNCH_WORDS = [
    "不对","不是","错了","怎么还","怎么还是","仍然","依然","依旧","还是不行","没解决",
    "还没","未完成","不成立","无效","没效果","不起作用","没反应","卡住","死循环",
    "推翻","白做","白干","越改越","更差","更糟","没生效","不生效","还是老问题",
    "仍然不对","还是不","没变","误判","误导","方向不对","错了方向","理解错","理解有误",
    "不对吧","不对啊","这不对","这样不对",
    "重新执行","重新设计","重新论证","重新分析","重新检查","重新弄","重新搞","重新来","重新",
    "为什么还没有","为什么还没","为什么没有解决","怎么还没有","怎么还没","还没解决","仍然没有",
    "卡住了","没看到你的输出","看不到输出","没有输出","无输出",
    "并不构成","不构成","不是那样","不是这样","搞反","白费",
]
# 弱打回词(宽口径):含双义词(问题/缺/漏…),可能为同域新问题报告
WEAK_PUNCH_WORDS = [
    "有问题","少了","漏了","漏掉","缺","再检查","再查一下","弄错","问题依旧","没到位","没落实",
    "不是","没做完","怎么还","怎么还是","仍然","依然","依旧",
]
# 新域/澄清剔除:命中以下前缀的行不是"对上次收尾的打回"。
#   - 自澄清:用户发现自己误解(剔除)
#   - 新域报告:以"我看到/我发现/现在的/关于…"开头的同域新问题(不视为对上次收尾质量不满)
SELF_CLARIFY_PREFIX = ("是","我说错","我误","我明","明白了","是我","算我","我自己")
NEW_DOMAIN_PREFIX = ("我看到","我发现","现在的","当前","关于","我注意到","刚才看到","再补充一个")
# 疑似(纯推进/继续类,常表达"你没做完,继续"),单独成桶避免把正常推进误计入明确打回
FORCE_WORDS = [
    "再跑","再试","再验证","再来","接着","继续排查","继续修","继续改","再加","再确认",
    "还有","另外一个","补充一下","另外","再往下","继续做","继续完成","继续分析","继续调查",
]

def blocks_parts(blocks):
    """返回 (has_text, has_tool_use)"""
    has_text = has_tool = False
    if not isinstance(blocks, list):
        return has_text, has_tool
    for b in blocks:
        if not isinstance(b, dict):
            continue
        t = b.get("type")
        if t == "text" and (b.get("text") or "").strip():
            has_text = True
        elif t == "tool_use":
            has_tool = True
    return has_text, has_tool

def classify_user(s):
    s = s.replace("\u2028", "").replace("\r", " ").strip()
    if not s:
        return None
    n = len(s.replace(" ", ""))
    lower = s.lower()
    if lower in ACK_SET:
        return None
    if n < 4:
        return None
    # 自澄清/新域报告剔除(先于词表命中判断 → 关键:防止假阳性)
    if s.startswith(SELF_CLARIFY_PREFIX):
        return None
    if s.startswith(NEW_DOMAIN_PREFIX):
        return None
    # 强打回(窄口径):优先命中
    for w in STRONG_PUNCH_WORDS:
        if w in s:
            return "punch_strong"
    # 弱打回(含"问题/重新"等双义词,计宽口径)
    if any(w in s for w in WEAK_PUNCH_WORDS):
        return "punch_weak"
    for w in FORCE_WORDS:
        if w in s:
            return "force"
    return None

def scan_file(path):
    turns = strong = weak = force = 0
    examples_p = []
    examples_f = []
    final_assistant = False  # 上一条 assistant 是收尾答复(含 text 无 tool_use)
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                r = json.loads(line)
            except Exception:
                continue
            m = r.get("message") or {}
            if not isinstance(m, dict):
                continue
            role = m.get("role")
            blocks = m.get("blocks") or []
            if role == "assistant":
                has_text, has_tool = blocks_parts(blocks)
                final_assistant = has_text and not has_tool
            elif role == "user":
                text = "".join((b.get("text") or "") for b in blocks if isinstance(b, dict) and b.get("type") == "text")
                cls = classify_user(text)
                if cls is not None:
                    if final_assistant:
                        turns += 1
                        if cls == "punch_strong":
                            strong += 1
                            if len(examples_p) < 40:
                                examples_p.append((path.split("session-")[-1][:16], text[:96]))
                        elif cls == "punch_weak":
                            weak += 1
                        elif cls == "force":
                            force += 1
                            if len(examples_f) < 30:
                                examples_f.append((path.split("session-")[-1][:16], text[:96]))
                # 任何 user 消息都重置收尾态(新的 turn 开始)
                final_assistant = False
    return turns, strong, weak, force, examples_p, examples_f

def main(roots):
    total_turns = total_strong = total_weak = total_force = 0
    rows = []
    all_ep, all_ef = [], []
    for root in roots:
        pat = os.path.join(root, "**", "session-*.jsonl")
        files = [p for p in glob.glob(pat, recursive=True) if ".rot-" not in p]
        for p in files:
            sz = os.path.getsize(p)
            if sz < 400:  # 空壳会话(仅 meta)
                continue
            t, st, wk, fk, ep, ef = scan_file(p)
            total_turns += t; total_strong += st; total_weak += wk; total_force += fk
            all_ep += ep; all_ef += ef
            if t > 0:
                rows.append((os.path.basename(os.path.dirname(p))[:10] + "/" + os.path.basename(p)[:24], t, st, wk, fk))
    print("=" * 100)
    print("打回率基线(无收敛契约,现状) — 口径: 强=直接质疑上次结果/重做/未解决/无输出; 弱=含双义词;")
    print("         剔除: 自澄清前缀 + 新域报告前缀(我看到/关于/现在的…)")
    print("=" * 100)
    print(f"{'会话':<44}{'turn':>6}{'强打回':>7}{'弱':>5}{'疑似':>5}")
    rows.sort(key=lambda r: -r[1])
    for name, t, st, wk, fk in rows:
        print(f"{name:<44}{t:>6}{st:>7}{wk:>5}{fk:>5}")
    print("-" * 100)
    def rate(a, b):
        return f"{a/b*100:.1f}%" if b else "-"
    print(f"合计: turns={total_turns}  强打回={total_strong} ({rate(total_strong,total_turns)})  弱={total_weak}  疑似推进={total_force}")
    print(f"窄口径打回率(强/turn)= {rate(total_strong,total_turns)}   宽口径(强+弱)/turn= {rate(total_strong+total_weak,total_turns)}")
    print("\n-- 强打回示例(前35) --")
    seen=set()
    for sid, txt in all_ep:
        k=(sid,txt[:30])
        if k in seen: continue
        seen.add(k)
        print(f"  [{sid}] {txt}")
    print("\n-- 疑似(强制推进)示例(前20) --")
    seen=set()
    for sid, txt in all_ef:
        k=(sid,txt[:30])
        if k in seen: continue
        seen.add(k)
        print(f"  [{sid}] {txt}")

if __name__ == "__main__":
    roots = sys.argv[1:] or [r"D:\chanlunV2\chanlun_py\.claw\sessions", r"d:\claw-code-src\.claw\sessions"]
    main(roots)