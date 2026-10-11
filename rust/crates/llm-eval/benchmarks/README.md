# Groundtruth Regression Task Suite

pass^1 的**客观分母**。每个任务 = 一条本仓库真实修复过的缺陷回归:
在 `base_commit` 上注入 `revert_files` 回退,验收命令应**失败**(pin 侧),
恢复修复后应**通过**(fixed 侧)。只有双向校验都符合预期的任务才进 validated 集。

> 为什么必须版本控制:`pass^1`(tau-bench `Pass^1` / SWE-bench `resolved rate`)
> 的分母必须是**独立、客观、可复现**的任务样本。这些 .jsonl 是经多轮双向
> 校验筛选出的 curated artifact(8→16→23→26→32→35),**不是**可随手重生成的
> 中间产物 —— 丢失即分母失真。故 `.gitignore` 对它们开后门
> (见仓库根 `.gitignore` 的 negation 规则),而再生型中间产物
> (`groundtruth_tasks*.jsonl`、`swebench_*.jsonl`、`*.partial.jsonl`)仍忽略。

## 文件

| 文件 | 内容 | 版本控制 |
|---|---|---|
| `groundtruth_validated_40.jsonl` | **35 条** curated 任务(当前分母) | ✅ 入库 |
| `groundtruth_validated.jsonl` | 早期 8 条集(保留供追溯) | ✅ 入库 |
| `groundtruth_validated*.jsonl.report.json` | 每条任务的校验判定(证据) | ✅ 入库 |
| `validate_two_sided.py` | 双向校验器(判据的实现) | ✅ 入库 |
| `groundtruth_tasks*.jsonl` / `swebench_*.jsonl` | 再生型原始池 | ❌ 忽略 |
| `*.partial.jsonl` | 中断残留 | ❌ 忽略 |

## 固化判据(Frozen Criteria)

一条任务 `valid` 当且仅当:

1. **pin 侧**:回退 `revert_files` 后跑 `acceptance.command`,退出码 **非期望**
   (通常 101 = `compile_error`;即缺陷确实被复现 → 基线红);
2. **fixed 侧**:恢复全部修复后跑同一 `acceptance.command`,退出码 **== `acceptance.expect_exit`**(通常 0);

任一不符合 → 判定为 `baseline_red`(两侧同红,任务不区分修复)、
`pin_side_failed`(补丁不可注入 / pin 侧意外通过)、
`skipped_toxic_taskkill`(任务自杀,环境不稳定)。

**重要口径**(来自 2026-10-11 重校验):旧版把**整 crate 验收**
(`cargo test -p <crate>`)当作判据,导致大量**假阴性** —— 整 crate 含需
Chromium/网络/计时的无关 flaky 测试,污染判定。现行做法:
`validate_two_sided.py --retries <n>` 重试 + `--own-tests` 仅看任务自身
相关测试,消除这类假阴性(11 条因此翻 valid)。

## 复现

```bash
# 全量重校验(需 Rust 工具链)
python rust/crates/llm-eval/benchmarks/validate_two_sided.py \
  --tasks rust/crates/llm-eval/benchmarks/groundtruth_validated_40.jsonl \
  --retries 2 --own-tests --task-timeout 600 \
  --out rust/crates/llm-eval/benchmarks/groundtruth_validated_40.jsonl.report.json

# 单任务隔离重跑(排查 batch_crash 用)
python rust/crates/llm-eval/benchmarks/validate_two_sided.py --single-task <task_id> ...
```

## 已知不稳定项(隔离重跑方可判定)

- `runtime-4ebb08f5`:任务自杀(toxic taskkill),批处理中会整体死亡,已跳过;
- `runtime-1aa64fc9`:batch_crash_skipped(隔离重跑进程消失,疑似 pin 后挂起);
- `single-task` 隔离模式即为此类场景提供。
