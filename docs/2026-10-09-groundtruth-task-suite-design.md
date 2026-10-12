# 路径 C:自建 ground-truth 任务集(内部回归)

> 日期:2026-10-09
> 关联:`docs/2026-10-09-benchmark-task-suite-design.md`(任务集选型)、`docs/2026-10-09-metrics-anchoring-design.md`(指标口径)
> 目的:在**零外部依赖、零 Docker** 的前提下,为本项目自身的 `pass^1`(一次做对率)提供**客观分母**。

## 1. 为什么需要路径 C

`docs/2026-10-09-benchmark-task-suite-design.md` 已选定权威任务集(SWE-bench Verified / tau-bench),但记录了一个**决定性约束**:

> 本机 Docker 不可用(无 WSL、无 Docker Desktop)→ SWE-bench 官方 harness 无法本地运行。

因此需要一条**不依赖容器**的替代路径,直接度量「本框架在本项目上」的表现。路径 C 即此。

## 2. 原理:回归钉(regression-pin)

等价 SWE-bench 的 **FAIL_TO_PASS** 机制,但**不要求测试位于独立 `tests/` 目录** —— 本仓库测试大多是**内联**的(`#[cfg(test)] mod tests` 写在 `src/*.rs` 内)。

流程:

```
1. 扫 git 历史 → 找「同一次提交里既有源码修改、又新增 `#[test]`」的提交 C
                 (这类提交通常 = 修复缺陷 + 补回归测试)
2. 还原       → 在 C 的隔离 git worktree 上,**反向应用「非测试 hunk」**
                 (退掉修复代码),测试 hunk 原样保留
                 ⇒ 该回归测试此刻**必然失败**(bug 已重新注入,测试仍在)
3. prompt     → C 的提交信息(症状描述,不泄露解法)
4. 判据       → `cargo test -p <crate>` 退出码 == 0  ⇒ resolved
```

**关键设计(hunk 级而非文件级)**:因为测试内联在源码文件里,若按**文件级**回退源码,会把测试一起退掉(任务就不成立了)。故必须按 **hunk 级**分类:
- 新增行含 `#[test]` / `#[cfg(test)]` / `mod tests` → **测试 hunk**(保留);
- 其余 → **修复 hunk**(反向应用)。

## 3. 已交付物

| 文件 | 说明 |
|---|---|
| `rust/crates/llm-eval/benchmarks/groundtruth_tasks.py` | 生成器/钉住器/验收器(三子命令 `build` / `pin` / `verify`,外加 `validate` 自检) |
| `rust/crates/llm-eval/benchmarks/validate_two_sided.py` | **双向校验器**(FAIL_TO_PASS 完整条件:pin→失败 **且** fixed→通过) |
| `rust/crates/llm-eval/benchmarks/groundtruth_tasks.jsonl` | 候选任务 15 条 |
| `rust/crates/llm-eval/benchmarks/groundtruth_validated.jsonl` | **可信任务集 8 条**(双向校验通过) |

**实测产出**:候选 15 条,覆盖 4 个 crate(runtime 11 / rusty-claude-cli 2 / tools 1 / web-bridge 1)。

## 4.1 双向校验结果(关键:单侧校验会假阳性)

**教训**:仅验「钉住→验收失败」**不充分**。若任务基线(`base_commit`)本身测试就是红的,
钉住后同样失败,会被误判为有效。

| 校验方式 | 结果 |
|---|---|
| 单侧(`validate_all.py`,仅 pin→失败) | total=15 **valid=14** invalid=1 ← **假阳性** |
| 双向(`validate_two_sided.py`,pin→失败 **且** fixed→通过) | total=15 **valid=8** invalid=7 |

无效 7 条的成因:
- `baseline_red` × 6:修复态验收也失败(exit=101)→ 基线本身红,任务不成立;
- `pin_side_failed` × 1:`web-bridge-ff3f8563` 反向补丁打不上,无法注入缺陷。

**可信分母 = 8 条**(全部满足 pin=失败 / fixed=通过)。

任务样例:
```
runtime-607e2c11      tests=6 hunks=5  fix(runtime): 站2/8 microcompact 上下文缩水修复——bash/read 信封内容感知压缩
tools-7ab735ac        tests=6 hunks=5  perf(tools): 站6 edit_file 延迟治理——同文件写后诊断冷却窗口
runtime-97c8ae57      tests=2 hunks=7  perf(runtime): 站5/3/4首迭代延迟修复——git快照/MCP目录迁移到消息尾部注入
```

## 4. 端到端验证(证据)

对任务 `runtime-607e2c11` 执行完整链路:

```
$ python groundtruth_tasks.py pin --task-id runtime-607e2c11 --worktree .gt-verify
pinned runtime-607e2c11 at D:/claw-code-src/.gt-verify (fix reverted, tests kept)
cargo test -p runtime

$ git -C .gt-verify diff --stat
 rust/crates/runtime/src/compact.rs             |  41 +++++-----
 rust/crates/runtime/src/content_compression.rs | 103 +++----------------
 2 files changed, 27 insertions(+), 117 deletions(-)

$ cd .gt-verify/rust && cargo test -p runtime --lib envelope_
error[E0425]: cannot find function `summarize_envelope_content` in this scope
error: could not compile `runtime` (lib test) due to 4 previous errors
```

**结论**:钉住后 `cargo test` **失败**(exit ≠ 0),证明任务有效(FAIL_TO_PASS)——修复代码已退,测试仍在并报错。运行器据此可判定:模型修复后 `cargo test` 通过 ⇒ `resolved=true`。

## 5. 用法

```bash
# 生成任务集
python rust/crates/llm-eval/benchmarks/groundtruth_tasks.py build \
  --repo . --limit 20 --out rust/crates/llm-eval/benchmarks/groundtruth_tasks.jsonl

# 钉住某任务(还原缺陷)→ 交给被测 agent 修复
python .../groundtruth_tasks.py pin --repo . --tasks <tasks.jsonl> \
  --task-id runtime-607e2c11 --worktree .gt-verify

# 判定 resolved(在被编辑后的 worktree 中)
python .../groundtruth_tasks.py verify --tasks <tasks.jsonl> \
  --task-id runtime-607e2c11 --worktree .gt-verify
```

## 6. 诚实性说明(与权威基准的区别)

| 维度 | 路径 C(本方案) | SWE-bench 官方 |
|---|---|---|
| 任务来源 | 本项目自身 git 历史 | 人审 issue 子集 |
| 判据 | `cargo test` 退出码 | 容器内 FAIL_TO_PASS ∪ PASS_TO_PASS |
| 环境隔离 | git worktree(原生) | Docker 容器 |
| **可比性** | **仅内部回归,不可与 leaderboard 比较** | 可跨机构比较 |

**必须遵守的红线**:
1. 本任务集产出的 `pass^1` 标注为**「内部回归」**,不得冒充 SWE-bench 官方分数;
2. 判据是 `cargo test` 退出码,**不是模型自评**;
3. prompt 不含 gold patch(只给症状);测试文件不得修改;
4. hunk 分类是启发式 —— 混合了测试与修复的 hunk 可能使任务无效,用 `validate` 子命令检出(`pin` 后验收命令应**失败**,否则任务无效)。

## 7. 与指标 M5 的对接

```
pass^1 = (resolved 任务数) / (任务总数)
resolved = (cargo test -p <crate> 退出码 == 0)
```

分母 = 15 条(可扩展 `--limit`);判据客观、可复现、不可被模型刷高。这是本仓库在**无 Docker** 条件下最接近 SWE-bench `resolved rate` 的可执行代理。

## 参考来源

- SWE-bench(princeton-nlp / SWE-bench):https://github.com/princeton-nlp/SWE-bench —— FAIL_TO_PASS / PASS_TO_PASS / resolved 定义
- 指标口径:`docs/2026-10-09-metrics-anchoring-design.md`(M5 = tau-bench `pass^1`)
