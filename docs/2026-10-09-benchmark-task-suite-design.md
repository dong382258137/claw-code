# 权威评测任务集选型与落地设计

> 日期：2026-10-09
> 关联：`docs/2026-10-09-metrics-anchoring-design.md`（指标口径锚定）
> 目的：解决「有标准、无权威任务」——为已锚定的 5 项指标提供**业界一致认定**的测试任务来源。

## 1. 问题陈述

`docs/2026-10-09-metrics-anchoring-design.md` 已把指标锚定到 OTel GenAI semconv 与 tau-bench/SWE-bench。但**指标需要任务才能产生数值**——尤其是 M5（一次做对率 = `pass^1`），其分母必须是**独立、客观、有 ground truth 的任务样本**，不能用会话内信号（如收敛声明）充当，否则指标可被模型刷高。

本文件解决：**任务从哪来、怎么取、怎么评分、本机能否运行。**

## 2. 选定的权威任务集

| 基准 | 覆盖指标 | 权威性 | 访问方式 |
|---|---|---|---|
| **SWE-bench Verified** | M5 `pass^1` / resolved rate、M2/M3/M4 | ICLR'24 oral；OpenAI/Anthropic 均在此报告 | HuggingFace datasets-server REST（**免 Docker**） |
| **SWE-bench Lite** | 同上（300 条轻量子集） | 官方维护 | 同上 |
| **tau-bench** | M5 可靠性 `pass^k` | Sierra Research；`pass^k` 原始出处 | GitHub 仓库内 JSON（`tau_bench/envs/<domain>/data/`） |

**为什么是这三个**：它们是被 leaderboard 长期使用、被论文/厂商一致引用的基准。SWE-bench 提供**客观二值判据**（测试通过与否），tau-bench 提供**可靠性**维度（同一任务跑 k 次是否次次成功）——正好覆盖「一次做对率」的两个语义面。

## 3. SWE-bench 任务结构（逐字字段，取自上游）

从 HF datasets-server 实测取回的字段：`instance_id`、`repo`、`base_commit`、`environment_setup_commit`、`version`、`problem_statement`、`patch`（gold）、`test_patch`、`FAIL_TO_PASS`、`PASS_TO_PASS`、`eval_script`、`image`、`difficulty`。

**客观判据（resolved 定义，来自官方 harness）**：
> 一个 instance 被判定 **resolved**，当且仅当：
> 1. `FAIL_TO_PASS` 中的**全部**测试由 fail 转为 pass；
> 2. `PASS_TO_PASS` 中的**全部**测试仍然 pass。

这是**二值、可复现、不可被模型刷高**的判据——正是 M5 需要的。

## 4. 已交付物

| 文件 | 说明 |
|---|---|
| `rust/crates/llm-eval/benchmarks/fetch_authoritative_tasks.py` | 从 HF 免 Docker 拉取 SWE-bench 任务，转 llm-eval 兼容 JSONL；含**跨仓库均匀采样**（避免整批同仓库偏差） |
| `rust/crates/llm-eval/benchmarks/swebench_verified_20.jsonl` | 20 条真实 SWE-bench Verified 任务（实测跨 7 个仓库） |

**实测输出**：`wrote 20 cases; distinct repos = 7`（astropy / django / matplotlib / seaborn / flask / requests / xarray）。每 case 携带完整 `meta`（base_commit / FAIL_TO_PASS / PASS_TO_PASS / gold_patch / test_patch）。

## 5. 本机可行性约束（诚实标注）

**决定性约束：本机 Docker 不可用。**

| 依赖 | 实测结果 |
|---|---|
| `docker` | ❌ `command not found` |
| WSL | ❌ 未安装（`WSL_E_DISTRO_NOT_FOUND`） |
| Docker Desktop | ❌ 无安装痕迹 |
| Python 3.11 / pytest 9.0.2 | ✅ 可用 |
| HuggingFace / PyPI 网络 | ✅ HTTP 200 |
| 磁盘 | ✅ 174G 可用 |

**影响**：SWE-bench **官方评测 harness 无法本机运行**——它依赖每个 repo 的专用容器镜像（`swebench/sweb.eval.x86_64.<repo>_<ver>:latest`）来隔离执行 `test_patch` 并跑 `FAIL_TO_PASS`/`PASS_TO_PASS`。

**这不是数据问题，是执行环境问题**：任务定义已可取（第 4 节已证），但**评分的执行隔离层缺失**。

### 三条可选落地路径（按成本排序）

**路径 A：远程/云评测（推荐用于正式基线）**
用官方 `sb-cli`（SWE-bench 云评测工具）提交预测补丁，由官方 harness 返回 resolved 结果。无需本地 Docker。
- 成本：需 API key / 联网提交。
- 优点：判据与 leaderboard 完全一致，结果**可对外比较**。

**路径 B：本地轻量自建 harness（推荐用于日常回归）**
本机装 Docker Desktop（或 Linux/WSL2）后，用官方 harness 跑子集。或**自建最小 harness**：`git clone --depth 1 <repo> @ base_commit` → 应用模型补丁 → `pip install -e .` → `pytest <FAIL_TO_PASS> <PASS_TO_PASS>`。
- 成本：首次装环境重；部分 repo 依赖复杂（astropy/django 编译依赖多）。
- 优点：本地可跑、可调试。
- 风险：**非官方隔离，判据可能与 leaderboard 有偏差**（环境漂移）——须标注为「本地近似」而非官方分数。

**路径 C：自建 ground-truth 任务集（推荐作为**本项目专属**补充）**
用 4 步构造法（见下），针对本仓库自身历史 turn 建任务集。
- 优点：零外部依赖、完全本地、直接度量「本框架在本项目上」的表现。
- 缺点：非业界可比，仅作**内部回归**。

**四步构造法（路径 C）**：
1. 从 `history.db` 抽取已完成的、有客观终态的 turn（如「测试通过」「文件按预期生成」）；
2. 记录其 **初始状态**（base commit / 起始文件树）与**客观终态**（可执行的验收命令）；
3. 反向构造 prompt（隐藏答案，只给 issue 描述）；
4. 用同一验收命令判定 resolved → 产生 `pass^1`。

## 6. 诚实性红线（防止指标被刷）

1. **M5 的分母必须是外部任务集**，不能用会话内「收敛声明通过率」——后者模型多输出声明段即可通过。
2. **路径 B/C 的结果标注为「本地近似」**，不得冒充 leaderboard 官方分数（环境漂移会导致不可比）。
3. **不使用模型自评作为 resolved 判据**——必须用可执行测试（`pytest`/`cargo test`）的退出码。
4. **tau-bench `pass^k` 需要 k 次独立重放**，若只跑 1 次则只能报 `pass^1`，不得标为 pass^k。

## 7. 与 5 项指标的对应

| 指标 | 任务来源 | 判据 |
|---|---|---|
| M1 首响应 TTFT | 任意任务集（测量层不依赖任务） | 埋点直采 |
| M2 总时长 | SWE-bench / tau-bench turn | 埋点直采 |
| M3 迭代数 | 同上 | 埋点直采 |
| M4 $/task | 同上（token 求和） | 埋点直采 |
| **M5 一次做对率** | **SWE-bench Verified（客观二值）** | **FAIL_TO_PASS ∪ PASS_TO_PASS 全绿** |

## 8. 建议的立即动作

1. **先用路径 C 起步**（零依赖，立即可用）：从 `history.db` 造 20–50 条本项目 ground-truth 任务；
2. **并行准备路径 A**（云评测）用于正式基线；
3. **M1 埋点先行**：TTFT 测量层与任务集正交，可先做（下一刀）。

## 参考来源

- SWE-bench（princeton-nlp / SWE-bench）：https://github.com/princeton-nlp/SWE-bench —— dataset 字段、resolved 定义、官方 harness、`sb-cli` 云评测
- SWE-bench Verified 数据集：https://huggingface.co/datasets/SWE-bench/SWE-bench_Verified （500 条人审子集）
- tau-bench（Sierra Research）：https://github.com/sierra-research/tau-bench —— `pass^k` / `pass@k` 定义、数据位于 `tau_bench/envs/<domain>/data/`
- OpenTelemetry GenAI semconv：https://github.com/open-telemetry/semantic-conventions-genai
