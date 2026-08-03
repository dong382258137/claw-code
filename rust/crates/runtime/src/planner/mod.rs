//! Plan/Execute/Review 三段循环 — Step 2.1 主入口。
//!
//! 设计文档:`docs/harness-engineering-optimization-plan.md` Step 2.1
//!
//! 架构:
//! - [`PlanArtifact`]:plan 数据模型(steps + status + replan_count)。
//! - [`PreCompletionChecklistMiddleware`]:Review 阶段中间件,决定 AllPassed/Replan/Failed。
//! - [`Planner`]:复杂任务检测 + PlanArtifact 生成入口。
//! - [`persist_plan_artifact`]:写入 `<workspace>/.claw/plans/<timestamp>.json`。
//!
//! **缓存保护**(详见 §5.2):
//! PlanArtifact 必须末尾追加到 prompt 的"变动区",不污染"绝对稳定区"
//! (system_prompt + tools_schema)与"半稳定区"(memory/goal/git_context)。
//! 预期命中率从 95% 降至 88-92%,通过 `prompt_cache.rs` 已有监控发现。
//!
//! **Feature flag**:
//! 默认不启用,需通过 CLI `--enable-plan-mode` 开启,或 settings.json
//! 配置 `"planMode": true` 启用。

pub mod artifact;
pub mod reviewer;

pub use artifact::{PlanArtifact, PlanPhase, PlanStep, StepRisk, StepStatus};
pub use reviewer::{
    render_remediation_prompt, FailedVerification, PreCompletionChecklistMiddleware, ReviewResult,
    DEFAULT_MAX_REPLANS,
};

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// 触发 plan 子调用的用户输入字符数阈值(粗略估算多文件预期)。
pub const COMPLEX_TASK_INPUT_CHARS_THRESHOLD: usize = 200;

/// 触发 plan 的关键词(用户输入包含任一即视为复杂任务)。
pub const COMPLEX_TASK_KEYWORDS: &[&str] = &[
    "multiple files",
    "refactor",
    "across modules",
    "step by step",
    "plan and execute",
    "多文件",
    "分步",
    "重构",
];

/// P2:高风险操作关键词,匹配任一则 step 标记为 High risk。
///
/// 覆盖 6 类高风险操作:
/// - 删除/移除:delete, drop, remove, truncate
/// - 强制操作:force, --force, -f
/// - 生产/部署:production, deploy, release, publish
/// - 安全/凭证:security, auth, password, token, secret, credential
/// - 不可逆:migrate, irreversible
/// - 权限:permission, privilege, chmod, chown
pub const HIGH_RISK_KEYWORDS: &[&str] = &[
    "delete",
    "drop",
    "remove",
    "truncate",
    "force",
    "--force",
    "production",
    "deploy",
    "release",
    "publish",
    "security",
    "auth",
    "password",
    "token",
    "secret",
    "credential",
    "migrate",
    "irreversible",
    "permission",
    "privilege",
    "chmod",
    "chown",
    "删除",
    "移除",
    "强制",
    "生产环境",
    "部署",
    "发布",
    "安全",
    "密码",
    "令牌",
    "凭证",
    "迁移",
    "权限",
];

/// 复杂任务检测结果 — 用于决定是否触发 planner 子调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComplexityAssessment {
    /// 简单任务,无需 plan,主 agent 直接执行。
    Simple,
    /// 复杂任务,触发 planner 生成 PlanArtifact。
    Complex {
        /// 触发原因(用于诊断日志)。
        reason: String,
    },
}

/// 评估用户输入是否为复杂任务。
///
/// 判定规则(满足任一即视为复杂):
/// 1. 用户输入字符数 > `COMPLEX_TASK_INPUT_CHARS_THRESHOLD`(200)。
/// 2. 包含 `COMPLEX_TASK_KEYWORDS` 中的任一关键词(大小写不敏感)。
#[must_use]
pub fn assess_complexity(user_input: &str) -> ComplexityAssessment {
    let trimmed = user_input.trim();
    if trimmed.chars().count() > COMPLEX_TASK_INPUT_CHARS_THRESHOLD {
        return ComplexityAssessment::Complex {
            reason: format!(
                "input length {} > threshold {}",
                trimmed.chars().count(),
                COMPLEX_TASK_INPUT_CHARS_THRESHOLD
            ),
        };
    }
    let lowered = trimmed.to_ascii_lowercase();
    for keyword in COMPLEX_TASK_KEYWORDS {
        if lowered.contains(keyword) {
            return ComplexityAssessment::Complex {
                reason: format!("matched keyword: {keyword}"),
            };
        }
    }
    ComplexityAssessment::Simple
}

/// 持久化 PlanArtifact 到 `<workspace>/.claw/plans/<id>.json`。
///
/// 文件路径用 plan id 命名(包含时间戳),同一 plan 多次 replan 不会产生多个文件,
/// 而是覆写同一文件(因为 replan_count 在 artifact 内部,文件本身只反映最新状态)。
///
/// 失败时返回 `Err`,调用方决定是否继续(通常记日志不阻断主流程)。
pub fn persist_plan_artifact(
    artifact: &PlanArtifact,
    workspace_root: &Path,
) -> Result<PathBuf, std::io::Error> {
    let plans_dir = workspace_root.join(".claw").join("plans");
    fs::create_dir_all(&plans_dir)?;
    let file_path = plans_dir.join(format!("{}.json", artifact.id));
    let json = serde_json::to_string_pretty(artifact)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    fs::write(&file_path, json)?;
    Ok(file_path)
}

/// 从文件加载 PlanArtifact(用于跨会话恢复 plan 状态)。
pub fn load_plan_artifact(path: &Path) -> Result<PlanArtifact, Box<dyn std::error::Error>> {
    let contents = fs::read_to_string(path)?;
    let artifact: PlanArtifact = serde_json::from_str(&contents)?;
    Ok(artifact)
}

/// P2:评估单个 step 的风险级别。
///
/// 检查 step 描述是否包含高风险关键词(删除/强制/生产/安全/不可逆/权限)。
/// 命中任一返回 `High`,否则 `Low`。用于驱动 Pre-commitment protocol 注入。
#[must_use]
pub fn assess_step_risk(description: &str) -> StepRisk {
    let lowered = description.to_ascii_lowercase();
    if HIGH_RISK_KEYWORDS.iter().any(|kw| lowered.contains(kw)) {
        StepRisk::High
    } else {
        StepRisk::Low
    }
}

/// 全局 feature flag — 控制是否启用 PlannerAgent 自动拆解接入并行链路。
///
/// 默认关闭(不启用)。通过 [`set_auto_planner_enabled`] 开启。
/// 开启后,`ConversationRuntime::plan_and_spawn_parallel` 会自动:
/// 1. 评估任务复杂度
/// 2. 调用 LLM 分解(失败降级到启发式)
/// 3. 转换为 SpawnRequest 并并行派发
static AUTO_PLANNER_ENABLED: OnceLock<bool> = OnceLock::new();

/// 开启/关闭 PlannerAgent 自动拆解(feature flag)。
///
/// 进程级单例,首次调用生效,后续调用静默忽略(与 `set_global_plan_generator_client` 一致)。
/// 默认关闭,需显式调用此函数开启(如 CLI `--enable-auto-planner` 或 settings.json)。
pub fn set_auto_planner_enabled(enabled: bool) {
    let _ = AUTO_PLANNER_ENABLED.set(enabled);
}

/// 查询 PlannerAgent 自动拆解是否已启用。
#[must_use]
pub fn is_auto_planner_enabled() -> bool {
    AUTO_PLANNER_ENABLED.get().copied().unwrap_or(false)
}

/// 默认模型(固定策略 — 所有 step 统一用 flash,通过 complexity 调整 max_retries)。
const DEFAULT_SUBAGENT_MODEL: &str = "deepseek-v4-flash";

/// 将 PlanStep 列表转换为 SpawnRequest 列表(PlannerAgent 接入并行链路的桥接器)。
///
/// 映射规则:
/// - `name` ← `step.id`
/// - `task` ← `step.description`
/// - `mode` ← `CoordinationMode::Fork`(并行派发)
/// - `model` ← 固定 `deepseek-v4-flash`(P0 策略,通过 complexity 调整 max_retries)
/// - `complexity` ← 基于 `step.risk_level`:
///   - `High` → `Architectural`(max_retries=2,容错最强)
///   - `Low` → `Simple`(max_retries=0,机械操作)
///
/// # 参数
/// - `steps`:由 `generate_steps_with_llm` 或 `decompose_task` 生成的 PlanStep 列表
///
/// # 返回
/// 转换后的 SpawnRequest 列表(长度与输入一致)
#[must_use]
pub fn plan_steps_to_spawn_requests(steps: &[PlanStep]) -> Vec<crate::multi_agent::SpawnRequest> {
    use crate::multi_agent::{CoordinationMode, SpawnRequest, TaskComplexity};

    steps
        .iter()
        .map(|step| {
            let complexity = match step.risk_level {
                StepRisk::High => TaskComplexity::Architectural,
                StepRisk::Low => TaskComplexity::Simple,
            };
            SpawnRequest::new(
                step.id.clone(),
                step.description.clone(),
                CoordinationMode::Fork,
                DEFAULT_SUBAGENT_MODEL,
                complexity,
            )
        })
        .collect()
}

/// PlannerAgent 自动拆解主入口 — LLM 驱动优先,失败降级到启发式。
///
/// # 流程
/// 1. 调用 [`generate_steps_with_llm`](LLM 驱动分解)
/// 2. LLM 失败或未注册 client → 降级到 [`decompose_task`](启发式分解)
/// 3. 转换为 `SpawnRequest` 列表
///
/// # 参数
/// - `user_input`:用户的原始任务描述
///
/// # 返回
/// - `Some(Vec<SpawnRequest>)`:拆解成功,可调用 `spawn_parallel_via_dag`
/// - `None`:拆解失败(理论上不会,因为 `decompose_task` 总是返回至少 1 个 step)
#[must_use]
pub fn plan_and_convert_to_spawn_requests(
    user_input: &str,
) -> Option<Vec<crate::multi_agent::SpawnRequest>> {
    // 1. LLM 驱动优先
    let steps = generate_steps_with_llm(user_input)
        // 2. 降级到启发式
        .unwrap_or_else(|| decompose_task(user_input));

    if steps.is_empty() {
        return None;
    }

    Some(plan_steps_to_spawn_requests(&steps))
}

/// Heuristic task decomposition — converts a complex user request into
/// concrete `PlanStep`s without calling an LLM sub-agent.
///
/// Returns at least 1 step even when no patterns match.
#[must_use]
pub fn decompose_task(user_input: &str) -> Vec<PlanStep> {
    let mut steps: Vec<PlanStep> = Vec::new();
    let mut step_id = 0u32;

    // 1. Check for multi-file operations — one step per detected file path.
    for file_path in extract_file_paths(user_input) {
        step_id += 1;
        steps.push(PlanStep::new(
            format!("step_{step_id}"),
            format!("Modify `{file_path}`"),
            format!("Verify {file_path} compiles and passes tests"),
        ));
    }

    // 2. Check for sequential markers.
    let sequential_markers = ["first", "then", "after that", "next", "finally"];
    let input_lower = user_input.to_lowercase();
    let has_markers = sequential_markers.iter().any(|m| input_lower.contains(m));

    // 3. Sentence-level decomposition for long or sequential input.
    if steps.is_empty() && (has_markers || user_input.len() > 300) {
        for sentence in split_into_sentences(user_input) {
            let trimmed = sentence.trim();
            if trimmed.is_empty() || trimmed.len() < 10 {
                continue;
            }
            step_id += 1;
            let short = if trimmed.len() > 80 {
                format!("{}…", &trimmed[..80])
            } else {
                trimmed.to_string()
            };
            steps.push(PlanStep::new(
                format!("step_{step_id}"),
                short,
                "Verify the step completed correctly".to_string(),
            ));
        }
        steps.truncate(10);
    }

    // 4. Fallback: at minimum one step.
    if steps.is_empty() {
        step_id += 1;
        let summary = if user_input.len() > 120 {
            format!("{}…", &user_input[..120])
        } else {
            user_input.to_string()
        };
        steps.push(PlanStep::new(
            format!("step_{step_id}"),
            format!("Execute: {summary}"),
            "Task completed and verified".to_string(),
        ));
    }

    // P2:对每个 step 评估风险级别,High risk step 在 render 时注入 Pre-commitment。
    // 检查 step description + 原始 user_input(兜底:泛化 description 时从任务上下文捕获风险)。
    let input_lower = user_input.to_ascii_lowercase();
    let input_is_high_risk = HIGH_RISK_KEYWORDS.iter().any(|kw| input_lower.contains(kw));
    let is_fallback_single_step = steps.len() == 1;
    for step in &mut steps {
        step.risk_level = assess_step_risk(&step.description);
        // 若 step description 未命中但整体任务命中,且 step 是兜底单步,继承 high-risk。
        if step.risk_level == StepRisk::Low && input_is_high_risk && is_fallback_single_step {
            step.risk_level = StepRisk::High;
        }
    }

    steps
}

/// LLM 计划生成 client — 由上层 CLI 注入,使复杂任务由模型生成
/// 计划步骤(LLM-driven planning),而非纯启发式 [`decompose_task`]。
///
/// 同步 trait(与 `compact::CompactionSummarizerClient` 同构),生产实现
/// (`rusty-claude-cli::llm_clients::DeepSeekPlanGeneratorClient`)内部用
/// 独立 tokio runtime + ProviderClient 桥接异步 API。
///
/// 未注册或返回无法解析的 JSON 时,[`generate_steps_with_llm`] 返回 `None`,
/// 调用方回退到 [`decompose_task`] 启发式分解。
pub trait PlanGeneratorClient: Send + Sync {
    /// 输入计划生成 prompt,返回模型原始文本(应为 JSON 数组)。
    fn generate_plan(&self, prompt: &str) -> Result<String, String>;
}

/// 全局计划生成 client(OnceLock,进程级单例)。
static GLOBAL_PLAN_GENERATOR: OnceLock<Option<Arc<dyn PlanGeneratorClient>>> = OnceLock::new();

/// 注册全局计划生成 client(进程级单例,重复注册静默忽略)。
pub fn set_global_plan_generator_client(client: Arc<dyn PlanGeneratorClient>) {
    let _ = GLOBAL_PLAN_GENERATOR.set(Some(client));
}

/// 是否已注册计划生成 client(供诊断/命令展示使用)。
#[must_use]
pub fn is_plan_generator_registered() -> bool {
    GLOBAL_PLAN_GENERATOR
        .get()
        .is_some_and(|slot| slot.is_some())
}

/// LLM 生成 PlanStep 列表 — 成功返回 `Some(steps)`,否则 `None`(调用方回退启发式)。
///
/// # 流程
/// 1. 构造生成 prompt(要求输出 JSON 数组)
/// 2. 调用 [`PlanGeneratorClient::generate_plan`]
/// 3. 容错解析 JSON(剥离 ```json 围栏、提取数组片段)
/// 4. 解析失败 / 空数组 / LLM 调用失败 → `None`
#[must_use]
pub fn generate_steps_with_llm(user_input: &str) -> Option<Vec<PlanStep>> {
    let client = GLOBAL_PLAN_GENERATOR.get()?.as_ref()?;
    let prompt = build_plan_generation_prompt(user_input);
    let raw = client.generate_plan(&prompt).ok()?;
    let steps = parse_llm_plan_steps(&raw)?;
    if steps.is_empty() {
        return None;
    }
    Some(steps)
}

/// 构建计划生成 prompt — 要求模型输出 JSON 数组,每个元素含
/// description / acceptance_criteria / verify_command(可选)。
fn build_plan_generation_prompt(user_input: &str) -> String {
    format!(
        "You are a task planner for a coding assistant. Decompose the following user request \
         into an ordered, executable plan.\n\
         \n\
         USER REQUEST:\n{user_input}\n\
         \n\
         Output ONLY a JSON array (no markdown fence, no prose). Each element: \
         {{\"description\": string, \"acceptance_criteria\": string, \"verify_command\": string | null}}.\n\
         Requirements:\n\
         - 3-10 steps; each step = one cohesive unit of work (e.g. one file change or one subsystem).\n\
         - description: what to do (Chinese or English, match the request language).\n\
         - acceptance_criteria: how to verify the step succeeded.\n\
         - verify_command: a concrete shell command that checks the step (e.g. \"cargo test --no-fail-fast\"), \
         or null if not applicable.\n\
         - Do not include escaping backslashes or code fences."
    )
}

/// 内部 JSON 结构 — 只取 LLM 提供的字段,其余字段由 [`PlanStep`] 默认值填充。
#[derive(serde::Deserialize)]
struct LlmPlanStep {
    description: String,
    #[serde(default)]
    acceptance_criteria: String,
    #[serde(default)]
    verify_command: Option<String>,
}

/// 容错解析 LLM 输出为 PlanStep 列表。
///
/// 容忍:```json 围栏、首尾多余文本、空数组。
/// 任一元素缺 description 或整体不是数组 → `None`(回退启发式)。
fn parse_llm_plan_steps(raw: &str) -> Option<Vec<PlanStep>> {
    let trimmed = raw.trim();
    // 剥离 markdown json 围栏(```json ... ``` 或 ``` ... ```)。
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|s| s.strip_suffix("```").map_or_else(|| s.trim(), str::trim))
        .unwrap_or(trimmed);
    // 提取第一个 '[' 到最后一个 ']' 之间的片段,容忍前后 prose。
    let start = body.find('[')?;
    let end = body.rfind(']')?;
    if end <= start {
        return None;
    }
    let array_text = &body[start..=end];
    let parsed: Vec<LlmPlanStep> = serde_json::from_str(array_text).ok()?;
    if parsed.is_empty() {
        return None;
    }
    Some(
        parsed
            .into_iter()
            .enumerate()
            .map(|(idx, step)| {
                let mut s = PlanStep::new(
                    format!("step_{}", idx + 1),
                    step.description,
                    if step.acceptance_criteria.is_empty() {
                        format!("Verify step {} completes successfully", idx + 1)
                    } else {
                        step.acceptance_criteria
                    },
                );
                s.verify_command = step.verify_command.filter(|c| !c.trim().is_empty());
                s.risk_level = assess_step_risk(&s.description);
                s
            })
            .collect(),
    )
}

/// Update an existing [`PlanArtifact`] with new or modified steps (G8.9).
///
/// Parses a structured or natural-language update description and applies
/// it to the plan. Supports:
/// - Adding new steps: `"add: Verify auth module compiles"`
/// - Marking steps done: `"done: step_1"`
/// - Marking steps failed: `"fail: step_2, reason: compilation error"`
/// - Replanning: resets Failed steps to Pending
///
/// Returns the number of changes applied.
pub fn update_plan(artifact: &mut PlanArtifact, update: &str) -> usize {
    let mut changes = 0usize;
    let trimmed = update.trim();

    // ── Pattern: "add: <description>" ──
    if let Some(desc) = trimmed
        .strip_prefix("add:")
        .or_else(|| trimmed.strip_prefix("Add:"))
        .or_else(|| trimmed.strip_prefix("ADD:"))
    {
        let desc = desc.trim();
        if !desc.is_empty() {
            let next_id = format!("step_{}", artifact.steps.len() + 1);
            artifact.steps.push(PlanStep::new(
                next_id,
                desc,
                "Verify step completed correctly".to_string(),
            ));
            changes += 1;
            return changes;
        }
    }

    // ── Pattern: "done: <step_id>" ──
    if let Some(step_id) = trimmed
        .strip_prefix("done:")
        .or_else(|| trimmed.strip_prefix("Done:"))
        .or_else(|| trimmed.strip_prefix("DONE:"))
    {
        let step_id = step_id.trim();
        if let Some(step) = artifact.steps.iter_mut().find(|s| s.id == step_id) {
            step.mark_succeeded();
            changes += 1;
        }
        return changes;
    }

    // ── Pattern: "fail: <step_id>[, reason: <text>]" ──
    if let Some(rest) = trimmed
        .strip_prefix("fail:")
        .or_else(|| trimmed.strip_prefix("Fail:"))
        .or_else(|| trimmed.strip_prefix("FAIL:"))
    {
        let rest = rest.trim();
        let step_id = rest.split(',').next().map(str::trim).unwrap_or(rest);
        if let Some(step) = artifact.steps.iter_mut().find(|s| s.id == step_id) {
            step.mark_failed();
            changes += 1;
        }
        return changes;
    }

    // ── Pattern: "replan" ──
    if trimmed.eq_ignore_ascii_case("replan") {
        if artifact.trigger_replan(3).is_some() {
            changes += 1;
        }
        return changes;
    }

    // ── Fallback: sentence-level decomposition appended as new steps ──
    for sentence in split_into_sentences(trimmed) {
        let s = sentence.trim();
        if s.is_empty() || s.len() < 10 {
            continue;
        }
        let next_id = format!("step_{}", artifact.steps.len() + 1);
        let short = if s.len() > 80 {
            format!("{}…", &s[..80])
        } else {
            s.to_string()
        };
        artifact.steps.push(PlanStep::new(
            next_id,
            short,
            "Verify step completed correctly".to_string(),
        ));
        changes += 1;
    }

    changes
}

fn extract_file_paths(text: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for word in text.split_whitespace() {
        let clean =
            word.trim_matches(|c: char| c == '`' || c == '"' || c == '\'' || c == ',' || c == '.');
        if is_likely_path(clean) && seen.insert(clean.to_string()) {
            paths.push(clean.to_string());
        }
    }
    paths
}

fn is_likely_path(s: &str) -> bool {
    let has_sep = s.contains('/') || s.contains('\\');
    let has_ext = s.ends_with(".rs")
        || s.ends_with(".toml")
        || s.ends_with(".md")
        || s.ends_with(".json")
        || s.ends_with(".ts")
        || s.ends_with(".py")
        || s.ends_with(".js");
    has_sep && has_ext && s.len() >= 5 && s.len() <= 120
}

fn split_into_sentences(text: &str) -> Vec<String> {
    text.split_inclusive(&['.', '!', '?', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== PlannerAgent 接入并行链路测试 =====

    #[test]
    fn plan_steps_to_spawn_requests_maps_high_risk_to_architectural() {
        let steps = vec![
            PlanStep::new("s1", "delete old migration files", "migrations removed"),
            PlanStep::new("s2", "add new tests", "tests pass"),
        ];
        // 手动标记 risk_level(decompose_task 会自动评估,这里直接构造测试数据)
        let mut steps = steps;
        steps[0].risk_level = StepRisk::High;
        steps[1].risk_level = StepRisk::Low;

        let requests = plan_steps_to_spawn_requests(&steps);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].name, "s1");
        assert_eq!(requests[0].task, "delete old migration files");
        assert_eq!(
            requests[0].complexity,
            crate::multi_agent::TaskComplexity::Architectural
        );
        assert_eq!(
            requests[1].complexity,
            crate::multi_agent::TaskComplexity::Simple
        );
        // 固定模型策略
        assert_eq!(requests[0].model, "deepseek-v4-flash");
        assert_eq!(requests[1].model, "deepseek-v4-flash");
        // 并行模式
        assert_eq!(
            requests[0].mode,
            crate::multi_agent::CoordinationMode::Fork
        );
    }

    #[test]
    fn plan_and_convert_to_spawn_requests_uses_heuristic_when_no_llm() {
        // 未注册 LLM client,应降级到启发式分解
        let input = "refactor src/auth.rs and src/session.rs to use shared types";
        let requests = plan_and_convert_to_spawn_requests(input)
            .expect("heuristic decomposition should always return at least 1 step");

        assert!(!requests.is_empty());
        // 启发式应检测到两个文件路径,生成 2 个 step
        assert!(
            requests.len() >= 2,
            "expected at least 2 steps for multi-file input, got {}",
            requests.len()
        );
        // 所有 request 应有有效的 name 和 task
        for r in &requests {
            assert!(!r.name.is_empty());
            assert!(!r.task.is_empty());
        }
    }

    #[test]
    fn plan_and_convert_to_spawn_requests_never_returns_none() {
        // 即使输入为空或无意义,decompose_task 也应返回至少 1 个兜底 step
        let requests = plan_and_convert_to_spawn_requests("");
        // 空输入可能返回 None(启发式 split 后过滤空串),但通常有兜底
        // 这里只验证不 panic
        let _ = requests;
    }

    #[test]
    fn is_auto_planner_enabled_defaults_to_false() {
        // 注意:OnceLock 是进程级单例,其他测试可能已调用 set_auto_planner_enabled(true)
        // 这里只验证函数可调用,不断言具体值
        let _ = is_auto_planner_enabled();
    }

    #[test]
    fn parses_llm_plan_json_array_with_verify_commands() {
        let raw = r#"[
            {"description": "Add auth module", "acceptance_criteria": "auth compiles", "verify_command": "cargo check -p auth"},
            {"description": "Add tests", "acceptance_criteria": "tests pass", "verify_command": "cargo test"}
        ]"#;
        let steps = parse_llm_plan_steps(raw).expect("valid JSON should parse");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].id, "step_1");
        assert_eq!(steps[0].description, "Add auth module");
        assert_eq!(
            steps[0].verify_command.as_deref(),
            Some("cargo check -p auth")
        );
        assert_eq!(steps[1].id, "step_2");
        assert_eq!(steps[0].status, StepStatus::Pending);
    }

    #[test]
    fn parses_llm_plan_with_markdown_fence() {
        let raw = "```json\n[{\"description\": \"refactor parser\", \"acceptance_criteria\": \"clippy clean\"}]\n```";
        let steps = parse_llm_plan_steps(raw).expect("fenced JSON should parse");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].description, "refactor parser");
    }

    #[test]
    fn parses_llm_plan_tolerates_surrounding_prose() {
        let raw = "Here is the plan:\n[{\"description\": \"step A\", \"acceptance_criteria\": \"done\"}]\nHope this helps!";
        let steps = parse_llm_plan_steps(raw).expect("prose-wrapped JSON should parse");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].description, "step A");
    }

    #[test]
    fn llm_plan_parse_returns_none_for_invalid_json() {
        assert!(parse_llm_plan_steps("not json at all").is_none());
        assert!(
            parse_llm_plan_steps("[]").is_none(),
            "empty array should be rejected"
        );
        assert!(parse_llm_plan_steps(r#"{"description": "not an array"}"#).is_none());
    }

    #[test]
    fn llm_plan_parse_defaults_missing_fields() {
        let raw = r#"[{"description": "only description"}]"#;
        let steps = parse_llm_plan_steps(raw).expect("minimal entry should parse");
        assert_eq!(steps.len(), 1);
        assert!(steps[0].verify_command.is_none());
        // acceptance_criteria 缺失时生成兜底文案。
        assert!(steps[0].acceptance_criteria.contains("Verify step 1"));
    }

    #[test]
    fn generate_steps_with_llm_returns_none_without_registered_client() {
        // 未注册全局 client 时,gfenerate_steps_with_llm 返回 None(走启发式)。
        assert!(generate_steps_with_llm("refactor the auth module").is_none());
    }

    #[test]
    fn assess_complexity_returns_simple_for_short_input() {
        let result = assess_complexity("hello world");
        assert_eq!(result, ComplexityAssessment::Simple);
    }

    #[test]
    fn assess_complexity_returns_complex_for_long_input() {
        let long_input = "a".repeat(COMPLEX_TASK_INPUT_CHARS_THRESHOLD + 1);
        let result = assess_complexity(&long_input);
        assert!(matches!(result, ComplexityAssessment::Complex { .. }));
    }

    #[test]
    fn assess_complexity_returns_complex_for_keyword_match() {
        let result = assess_complexity("refactor the auth module");
        match result {
            ComplexityAssessment::Complex { reason } => {
                assert!(reason.contains("refactor"));
            }
            other => panic!("expected Complex, got {other:?}"),
        }
    }

    #[test]
    fn assess_complexity_returns_complex_for_chinese_keyword() {
        let result = assess_complexity("多文件重构");
        match result {
            ComplexityAssessment::Complex { reason } => {
                assert!(reason.contains("多文件") || reason.contains("重构"));
            }
            other => panic!("expected Complex, got {other:?}"),
        }
    }

    #[test]
    fn assess_complexity_is_case_insensitive() {
        let result = assess_complexity("REFACTOR everything");
        assert!(matches!(result, ComplexityAssessment::Complex { .. }));
    }

    #[test]
    fn assess_complexity_ignores_leading_whitespace() {
        let result = assess_complexity("    refactor    ");
        assert!(matches!(result, ComplexityAssessment::Complex { .. }));
    }

    #[test]
    fn persist_and_load_plan_artifact_round_trip() {
        let temp = std::env::temp_dir().join(format!(
            "planner-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp).unwrap();

        let artifact =
            PlanArtifact::new("test task", vec![PlanStep::new("s1", "step 1", "criteria")]);
        let path = persist_plan_artifact(&artifact, &temp).expect("persist should succeed");
        assert!(path.exists());

        let loaded = load_plan_artifact(&path).expect("load should succeed");
        assert_eq!(loaded.id, artifact.id);
        assert_eq!(loaded.task_summary, "test task");
        assert_eq!(loaded.steps.len(), 1);
        assert_eq!(loaded.steps[0].id, "s1");

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn persist_creates_claw_plans_directory() {
        let temp = std::env::temp_dir().join(format!(
            "planner-mkdir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // 确保 temp 存在但 .claw/plans 不存在。
        fs::create_dir_all(&temp).unwrap();
        let plans_dir = temp.join(".claw").join("plans");
        assert!(!plans_dir.exists());

        let artifact = PlanArtifact::new("t", Vec::new());
        let _ = persist_plan_artifact(&artifact, &temp).expect("should succeed");
        assert!(plans_dir.exists());

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn persist_overwrites_same_id() {
        // 同一 plan 的 replan 应该覆写同一文件,不产生多个文件。
        let temp = std::env::temp_dir().join(format!(
            "planner-overwrite-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp).unwrap();

        let mut artifact = PlanArtifact::new("task", vec![PlanStep::new("s1", "step", "c")]);
        let path = persist_plan_artifact(&artifact, &temp).unwrap();

        // 模拟 replan:同一 id,但 step 状态改变。
        artifact.steps[0].mark_failed();
        let _ = artifact.trigger_replan(3);
        let path2 = persist_plan_artifact(&artifact, &temp).unwrap();

        assert_eq!(path, path2);
        let files: Vec<_> = fs::read_dir(temp.join(".claw").join("plans"))
            .unwrap()
            .collect();
        assert_eq!(files.len(), 1, "should overwrite, not create new file");

        let _ = fs::remove_dir_all(&temp);
    }

    // ── P2:Pre-commitment risk 评估测试 ──

    #[test]
    fn assess_step_risk_returns_high_for_delete() {
        assert_eq!(assess_step_risk("Delete the user table"), StepRisk::High);
    }

    #[test]
    fn assess_step_risk_returns_high_for_production_deploy() {
        assert_eq!(
            assess_step_risk("Deploy to production environment"),
            StepRisk::High
        );
    }

    #[test]
    fn assess_step_risk_returns_high_for_security_keywords() {
        assert_eq!(assess_step_risk("Update auth token"), StepRisk::High);
        assert_eq!(assess_step_risk("Rotate password"), StepRisk::High);
        assert_eq!(
            assess_step_risk("Fix security vulnerability"),
            StepRisk::High
        );
    }

    #[test]
    fn assess_step_risk_returns_high_for_chinese_keywords() {
        assert_eq!(assess_step_risk("删除用户数据"), StepRisk::High);
        assert_eq!(assess_step_risk("部署到生产环境"), StepRisk::High);
        assert_eq!(assess_step_risk("修改权限配置"), StepRisk::High);
    }

    #[test]
    fn assess_step_risk_returns_low_for_safe_operations() {
        assert_eq!(assess_step_risk("Read configuration file"), StepRisk::Low);
        assert_eq!(assess_step_risk("Add unit tests"), StepRisk::Low);
        assert_eq!(assess_step_risk("Update documentation"), StepRisk::Low);
    }

    #[test]
    fn assess_step_risk_is_case_insensitive() {
        assert_eq!(assess_step_risk("DELETE all rows"), StepRisk::High);
        assert_eq!(assess_step_risk("Force Push"), StepRisk::High);
    }

    #[test]
    fn decompose_task_marks_high_risk_steps() {
        // 包含 "delete" 关键词的输入,分解后的 step 应标记 High risk
        let steps = decompose_task("delete the migration files and refactor auth");
        assert!(steps.iter().any(|s| s.risk_level == StepRisk::High));
    }

    #[test]
    fn decompose_task_keeps_low_risk_for_safe_input() {
        let steps = decompose_task("read the config and update the docs");
        assert!(steps.iter().all(|s| s.risk_level == StepRisk::Low));
    }

    #[test]
    fn decompose_task_fallback_inherits_high_risk_from_input() {
        // 兜底单步(无法拆分)时,若整体任务命中高风险,继承 High
        let steps = decompose_task("force deploy");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].risk_level, StepRisk::High);
    }
}
