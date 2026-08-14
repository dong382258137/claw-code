//! subagent 完成后的验证门禁 — Multi-Agent Hardening Plan §4.4。
//!
//! 设计文档:`docs/multi-agent-hardening-plan.md` §4.4
//!
//! 架构:
//! - [`ValidationGate`][]:验证门禁 trait,实现此 trait 并注册到 coordinator。
//! - [`ValidationContext`][]:统一注入参数,避免 trait 方法签名膨胀。
//! - [`CommandValidationGate`][]:通用命令验证门禁(支持 cargo/npm/python/pytest 等)。
//! - [`LlmJudgeGate`][]:v3 P0 trait 预留,LLM-as-judge 用于诊断/架构任务(MVP 仅占位)。
//! - [`detect_changed_files`][]:从 `git diff --name-only` 检测实际修改的文件。
//!
//! v2 修正:
//! - 抽象 `CommandValidationGate` 支持任意命令,非 Rust 项目可用
//! - 引入 `ValidationContext` 传递 workspace_root / changed_files / model
//! - 用 `git diff --name-only` 检测实际修改的文件,而非 v1 的 result_text 关键字匹配
//!
//! v3 新增:
//! - `LlmJudgeGate` trait 预留(实现留待 v2),用于诊断/架构任务
//! - 借鉴 Anthropic Multi-Agent Research System 的 rubric 评分 + end-state evaluation

use std::path::{Path, PathBuf};

/// 验证上下文 — 传递给每个 gate,避免 trait 方法参数膨胀。
///
/// v1 trait 方法签名只有 (id, task, result_path),gate 实现需自带 workspace_root,
/// v2 改为通过 context 统一注入。
pub struct ValidationContext<'a> {
    /// subagent ID。
    pub subagent_id: &'a str,
    /// subagent 任务描述。
    pub task: &'a str,
    /// subagent 结果文件路径(如 `.claw/subagents/{id}.md`)。
    pub result_path: &'a Path,
    /// workspace 根目录,用于执行验证命令。
    pub workspace_root: &'a Path,
    /// `git diff --name-only` 检测到的修改文件列表(全局,含主 agent 并发修改)。
    pub changed_files: &'a [PathBuf],
    /// subagent handoff 中**声称**修改的文件列表(§8.4 双列表检查)。
    /// 与 `changed_files`(git diff 全局)交叉比对:
    /// - 声称改了但 git diff 没有 → 子智能体声称与实际不符,需排查
    /// - git diff 有但子智能体没声称 → 主 agent 并发修改,不归此子智能体 validation
    pub subagent_changed_files: &'a [PathBuf],
    /// subagent 使用的模型名(用于诊断日志和 LlmJudgeGate)。
    pub model: &'a str,
}

/// subagent 完成后的验证门禁 trait。
///
/// 实现此 trait 并通过 [`MultiAgentCoordinator::add_validation_gate`] 注册,
/// 在 subagent 完成后自动验证。
///
/// [`MultiAgentCoordinator::add_validation_gate`]: super::MultiAgentCoordinator::add_validation_gate
pub trait ValidationGate: Send + Sync {
    /// 验证 subagent 结果。返回 Err 表示验证失败。
    fn validate(&self, ctx: &ValidationContext) -> Result<(), ValidationError>;

    /// gate 名称,用于诊断日志。
    fn name(&self) -> &'static str {
        "unnamed"
    }
}

/// 验证错误。
#[derive(Debug, Clone)]
pub struct ValidationError {
    /// 错误消息。
    pub message: String,
    /// 是否可重试:true=可重试(如编译错误),false=直接失败(如环境错误)。
    pub retryable: bool,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tag = if self.retryable { "retryable" } else { "fatal" };
        write!(f, "[validation:{tag}] {}", self.message)
    }
}

impl std::error::Error for ValidationError {}

/// 通用命令验证门禁。
///
/// 支持任意 shell 命令(cargo build / npm run build / python -m pytest 等),
/// 通过 `file_filter` 正则决定是否触发(避免非代码修改触发编译)。
///
/// # 示例
/// - Rust:  `CommandValidationGate::new("cargo-build", ["cargo","build"], root, r"\.rs$")`
/// - Node:  `CommandValidationGate::new("npm-build", ["npm","run","build"], root, r"\.(ts|tsx|js|jsx)$")`
/// - Python:`CommandValidationGate::new("pytest", ["python","-m","pytest"], root, r"\.py$")`
pub struct CommandValidationGate {
    gate_name: String,
    command: Vec<String>,
    workspace_root: PathBuf,
    /// 匹配 changed_files 中任一文件才触发验证。
    file_filter: regex::Regex,
}

impl CommandValidationGate {
    /// 创建一个新的命令验证门禁。
    ///
    /// # 参数
    /// - `name`: gate 名称(用于诊断日志)
    /// - `command`: 要执行的命令(第一个元素是可执行文件,其余是参数)
    /// - `workspace_root`: 命令执行的 working directory
    /// - `file_filter_pattern`: 正则表达式,匹配 changed_files 中任一文件才触发验证
    pub fn new(
        name: impl Into<String>,
        command: impl IntoIterator<Item = impl Into<String>>,
        workspace_root: impl Into<PathBuf>,
        file_filter_pattern: &str,
    ) -> Self {
        Self {
            gate_name: name.into(),
            command: command.into_iter().map(|s| s.into()).collect(),
            workspace_root: workspace_root.into(),
            file_filter: regex::Regex::new(file_filter_pattern)
                .expect("invalid file_filter pattern"),
        }
    }

    /// gate 名称(用于诊断日志,与 `ValidationGate::name` 配合)。
    pub fn gate_name(&self) -> &str {
        &self.gate_name
    }
}

impl ValidationGate for CommandValidationGate {
    fn validate(&self, ctx: &ValidationContext) -> Result<(), ValidationError> {
        // v2 修正:用 changed_files + 正则判断是否触发,而非 v1 的 result_text 关键字匹配
        // Epic 1 T3 增强:git diff 可能检测不到 untracked 新文件(detect_changed_files
        // 已补 git ls-files),此处再补充子智能体声称的变更集(subagent_changed_files,
        // 绝对路径,正则匹配路径尾部即可),声称改了相关文件也触发验证,互为兜底。
        let triggered = ctx
            .changed_files
            .iter()
            .any(|f| self.file_filter.is_match(&f.to_string_lossy()))
            || ctx
                .subagent_changed_files
                .iter()
                .any(|f| self.file_filter.is_match(&f.to_string_lossy()));
        if !triggered {
            // 无相关文件修改,跳过验证(避免 README 修改触发 cargo build)
            return Ok(());
        }

        // 同步诊断日志:记录验证开始
        crate::diag::global().append(
            crate::diag::DiagEntry::new(
                crate::diag::DiagLevel::Info,
                "validation",
                format!(
                    "gate '{}' triggered for subagent {}",
                    self.gate_name, ctx.subagent_id
                ),
            )
            .with_field("gate", serde_json::Value::String(self.gate_name.clone()))
            .with_field("command", serde_json::Value::String(self.command.join(" "))),
        );

        let output = std::process::Command::new(&self.command[0])
            .args(&self.command[1..])
            .current_dir(&self.workspace_root)
            .output();

        match output {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => {
                let msg = format!(
                    "{} failed (exit={}):\nstdout: {}\nstderr: {}",
                    self.gate_name,
                    o.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr),
                );
                crate::diag::global().append(
                    crate::diag::DiagEntry::new(crate::diag::DiagLevel::Error, "validation", &msg)
                        .with_field("retryable", serde_json::Value::Bool(true)),
                );
                Err(ValidationError {
                    message: msg,
                    retryable: true, // 编译/测试失败可重试
                })
            }
            Err(e) => {
                let msg = format!("failed to run {}: {e}", self.gate_name);
                crate::diag::global().append(
                    crate::diag::DiagEntry::new(crate::diag::DiagLevel::Error, "validation", &msg)
                        .with_field("retryable", serde_json::Value::Bool(false)),
                );
                Err(ValidationError {
                    message: msg,
                    retryable: false, // 环境错误不可重试(如命令不存在)
                })
            }
        }
    }

    fn name(&self) -> &'static str {
        "command"
    }
}

/// Rust 专用门禁(基于 `CommandValidationGate` 的便捷构造)。
///
/// 等价于 `CommandValidationGate::new("cargo-build", ["cargo", "build"], workspace_root, r"\.rs$")`。
pub fn rust_compile_gate(workspace_root: PathBuf) -> CommandValidationGate {
    CommandValidationGate::new("cargo-build", ["cargo", "build"], workspace_root, r"\.rs$")
}

/// Node.js / TypeScript 专用门禁(v2 新增,§10.5 多 ValidationGate)。
///
/// 等价于 `CommandValidationGate::new("npm-build", ["npm","run","build"], workspace_root, r"\.(ts|tsx|js|jsx)$")`。
///
/// 仅在 `changed_files` 含 `.ts/.tsx/.js/.jsx` 时触发;无相关文件修改时静默跳过。
/// 若项目无 `package.json` 或 `build` script,gate 会以 retryable=false 失败中止
/// validation 链(环境错误,非代码错误)。
pub fn npm_build_gate(workspace_root: PathBuf) -> CommandValidationGate {
    CommandValidationGate::new(
        "npm-build",
        ["npm", "run", "build"],
        workspace_root,
        r"\.(ts|tsx|js|jsx)$",
    )
}

/// Python 专用门禁(v2 新增,§10.5 多 ValidationGate)。
///
/// 等价于 `CommandValidationGate::new("pytest", ["python","-m","pytest"], workspace_root, r"\.py$")`。
///
/// 仅在 `changed_files` 含 `.py` 时触发;无相关文件修改时静默跳过。
/// 若项目无 `pytest` 或 `pyproject.toml`,gate 会以 retryable=false 失败。
pub fn pytest_gate(workspace_root: PathBuf) -> CommandValidationGate {
    CommandValidationGate::new(
        "pytest",
        ["python", "-m", "pytest"],
        workspace_root,
        r"\.py$",
    )
}

/// 从 git 工作区检测 subagent 修改的文件。
///
/// 在 [`MultiAgentCoordinator::validate`] 调用前执行,结果填入 [`ValidationContext`]。
///
/// 覆盖两类变更(Epic 1 T1 Execute 放开 write_file 后新建文件是常态):
/// - tracked 修改:`git diff --name-only HEAD`(工作区 + index 相对 HEAD 的差异)
/// - untracked 新文件:`git ls-files --others --exclude-standard`
///   (git diff 检测不到 untracked 文件,若只靠它,新建文件会跳过验证链)
///
/// [`MultiAgentCoordinator::validate`]: super::MultiAgentCoordinator::validate
pub fn detect_changed_files(workspace_root: &Path) -> Vec<PathBuf> {
    let mut files = run_git(&["diff", "--name-only", "HEAD"], workspace_root);
    files.extend(run_git(
        &["ls-files", "--others", "--exclude-standard"],
        workspace_root,
    ));
    // 去重:同一文件可能同时出现在 tracked 修改与 untracked 列表
    let mut seen = std::collections::HashSet::new();
    files.retain(|p| seen.insert(p.clone()));
    files
}

/// 运行 git 子命令,成功时返回相对路径行(正斜杠);失败(非 git 仓库/git 不可用)
/// 返回空 Vec(门禁可能跳过,向后兼容)。
fn run_git(args: &[&str], workspace_root: &Path) -> Vec<PathBuf> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(workspace_root)
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(PathBuf::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// Epic 5 §8.4:从 handoff frontmatter 读取子智能体声称的变更文件列表。
///
/// 解析失败(旧格式纯文本、IO 错误、无 frontmatter)时返回空 `Vec`,向后兼容。
/// 路径规范化由 handoff 落盘侧(`extract_changed_files`)保证,此处直接转 `PathBuf`。
pub fn read_handoff_changed_files(handoff_path: &Path) -> Vec<PathBuf> {
    match crate::multi_agent::read_handoff(handoff_path) {
        Ok(h) => h.changed_files.into_iter().map(PathBuf::from).collect(),
        Err(_) => Vec::new(),
    }
}

/// Epic 5 §8.4:双列表交叉检查 — 子智能体声称的变更集 vs `git diff` 全局变更集。
///
/// 返回 `(unverified, concurrent)`:
/// - `unverified`:子智能体声称改了但 git diff 没有(可能未落盘/被还原,需排查)
/// - `concurrent`:git diff 有但子智能体没声称(主 agent 并发修改,不归此子智能体)
///
/// 归一化:handoff 声称的是绝对路径(`extract_changed_files` 用 `workspace_root.join`),
/// git diff 是相对路径。比对前把声称路径 strip 掉 workspace_root 前缀并统一正斜杠,
/// 否则绝对/相对恒不匹配(§8.4 双列表检查失真)。
pub fn compute_changed_files_mismatch(ctx: &ValidationContext<'_>) -> (Vec<String>, Vec<String>) {
    if ctx.subagent_changed_files.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let git_set: std::collections::HashSet<&str> = ctx
        .changed_files
        .iter()
        .filter_map(|p| p.to_str())
        .collect();
    let root = ctx.workspace_root;
    let claimed_set: std::collections::HashSet<String> = ctx
        .subagent_changed_files
        .iter()
        .map(|p| {
            p.strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();

    let unverified = claimed_set
        .iter()
        .filter(|n| !git_set.contains(n.as_str()))
        .map(|s| s.to_string())
        .collect();
    let concurrent = git_set
        .iter()
        .filter(|n| !claimed_set.contains(**n))
        .map(|s| s.to_string())
        .collect();
    (unverified, concurrent)
}

/// Epic 5 §8.4:双列表交叉检查 — 诊断版(写 `diag` 日志,不触发 validation 失败/retry)。
///
/// 不作为硬失败的原因:声称与实际不符也可能是子智能体 edit 后又还原(净零变更),
/// 硬失败会触发 retry 风暴,与 FailFast::Off 理念冲突;诊断日志已足够 surface 异常。
pub fn diagnose_changed_files_mismatch(ctx: &ValidationContext<'_>) {
    let (unverified, concurrent) = compute_changed_files_mismatch(ctx);
    if !unverified.is_empty() {
        crate::diag::global().warn(
            "subagent_changed_files_mismatch",
            &format!(
                "subagent {} claimed changes not in git diff: [{}] (可能未落盘/被还原,需排查)",
                ctx.subagent_id,
                unverified.join(", "),
            ),
        );
    }
    if !concurrent.is_empty() {
        crate::diag::global().info(
            "subagent_concurrent_changes",
            &format!(
                "subagent {} git diff has files not claimed by subagent: [{}] (主 agent 并发修改,不归此子智能体 validation)",
                ctx.subagent_id,
                concurrent.join(", "),
            ),
        );
    }
}

/// v3 新增(P0 trait 预留, v2 实现):LLM-as-judge 验证门禁。
///
/// 用于诊断/架构类任务,确定性命令无法验证的场景。
///
/// # 借鉴
/// Anthropic Multi-Agent Research System 的 LLM-as-judge 模式:
/// - 单次 LLM 调用 + 单一 prompt 输出 0.0-1.0 分数
/// - rubric 包含:准确性/完整性/根因定位/方案可行性
/// - end-state evaluation:只评判最终状态,不评判中间步骤
///
/// # v2 实现要点(§10.5 Epic 5)
/// - **依赖倒置**:runtime crate 不直接依赖 api crate,通过 [`JudgeClient`] trait 注入。
///   生产环境由上层(rusty-claude-cli)构造 `Arc<dyn JudgeClient>` 注入;
///   测试环境用 [`MockJudgeClient`] 注入预设分数。
/// - **无 client 时降级为 stub**:返回 `Ok(())` + Warn 诊断日志,
///   保证未接入 judge 的项目不阻塞 validation 链。
/// - **分数解析容错**:LLM 输出可能含解释文本,用正则提取首个 0.0-1.0 浮点数;
///   解析失败按 `ValidationError` (retryable=false) 处理,避免无限重试。
pub struct LlmJudgeGate {
    /// 评判模型(建议用旗舰,如 deepseek-v4-pro,保证判断质量)。
    pub judge_model: String,
    /// 评分标准(rubric),注入到 judge prompt。
    pub rubric: String,
    /// 通过阈值(0.0-1.0),低于此分则 validation 失败。
    pub pass_threshold: f64,
    /// workspace_root(用于读取 changed_files 内容供 judge 参考)。
    pub workspace_root: PathBuf,
    /// judge client(v2 依赖倒置)。None 时降级为 stub。
    client: Option<std::sync::Arc<dyn JudgeClient>>,
}

impl std::fmt::Debug for LlmJudgeGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmJudgeGate")
            .field("judge_model", &self.judge_model)
            .field("pass_threshold", &self.pass_threshold)
            .field("workspace_root", &self.workspace_root)
            .field("has_client", &self.client.is_some())
            .finish()
    }
}

impl Clone for LlmJudgeGate {
    fn clone(&self) -> Self {
        Self {
            judge_model: self.judge_model.clone(),
            rubric: self.rubric.clone(),
            pass_threshold: self.pass_threshold,
            workspace_root: self.workspace_root.clone(),
            client: self.client.clone(),
        }
    }
}

/// judge client trait — v2 §10.5 Epic 5 依赖倒置。
///
/// runtime crate 不直接依赖 api crate(避免循环依赖),通过此 trait 注入 LLM 调用。
/// 生产实现由上层 crate 构造(封装 `ProviderClient::from_model` + async-to-sync 桥接)。
///
/// # 接口约定
/// - 输入:judge prompt(含 task/model/result/rubric)
/// - 输出:LLM 原始文本响应(由 `LlmJudgeGate::parse_score` 解析分数)
/// - 错误:网络/API/超时等返回 `Err(String)`,`LlmJudgeGate::validate` 负责降级处理
///
/// # 测试
/// 使用 [`MockJudgeClient`] 注入预设响应,验证分数解析 + 阈值比较逻辑。
pub trait JudgeClient: Send + Sync {
    /// 调用 judge 模型,返回原始文本响应。
    fn judge(&self, prompt: &str) -> Result<String, String>;
}

/// mock judge client — 仅用于测试。
#[cfg(test)]
pub struct MockJudgeClient {
    /// 预设响应文本。
    pub response: String,
    /// 是否强制返回 Err(模拟 API 故障)。
    pub force_error: bool,
}

#[cfg(test)]
impl JudgeClient for MockJudgeClient {
    fn judge(&self, _prompt: &str) -> Result<String, String> {
        if self.force_error {
            return Err("mock API failure".to_string());
        }
        Ok(self.response.clone())
    }
}

impl LlmJudgeGate {
    /// 诊断任务默认 rubric(借鉴 Anthropic rubric 设计)。
    ///
    /// **注意**:此构造器不注入 client,validate 时降级为 stub(返回 Ok + Warn)。
    /// 生产使用请用 [`LlmJudgeGate::with_client`] 注入 client。
    pub fn diagnostic_default(judge_model: impl Into<String>, workspace_root: PathBuf) -> Self {
        Self {
            judge_model: judge_model.into(),
            rubric: r#"请按以下 rubric 评分(0.0-1.0),只输出分数:
1. 根因定位准确性 (0.3):是否正定位问题根本原因?
2. 修复方案可行性 (0.3):方案是否真正解决问题,非治标不治本?
3. 完整性 (0.2):是否覆盖所有相关场景和边界条件?
4. 副作用评估 (0.2):是否评估引入新问题的风险?
总分 = 各项加权求和。"#
                .to_string(),
            pass_threshold: 0.7, // 默认 0.7 通过
            workspace_root,
            client: None,
        }
    }

    /// 注入 judge client — v2 生产入口。
    ///
    /// 在 `diagnostic_default` 基础上注入 `Arc<dyn JudgeClient>`,
    /// 使 `validate` 执行真实的 LLM judge 调用。
    pub fn with_client(mut self, client: std::sync::Arc<dyn JudgeClient>) -> Self {
        self.client = Some(client);
        self
    }

    /// 构造 judge prompt(v2 §10.5 Epic 5)。
    ///
    /// prompt 结构:
    /// ```text
    /// 你是一个代码评审 judge,请评估 subagent 完成的任务质量。
    ///
    /// ## 任务
    /// {task}
    ///
    /// ## subagent 使用的模型
    /// {model}
    ///
    /// ## 修改的文件
    /// {changed_files}
    ///
    /// ## subagent 结果
    /// {result_content}
    ///
    /// {rubric}
    /// ```
    fn build_judge_prompt(&self, ctx: &ValidationContext) -> String {
        let changed_files_str = if ctx.changed_files.is_empty() {
            "(无文件修改)".to_string()
        } else {
            ctx.changed_files
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(", ")
        };

        let result_content = std::fs::read_to_string(ctx.result_path)
            .unwrap_or_else(|e| format!("(读取结果文件失败: {e})"));

        format!(
            "你是一个代码评审 judge,请评估 subagent 完成的任务质量。\n\n\
             ## 任务\n{task}\n\n\
             ## subagent 使用的模型\n{model}\n\n\
             ## 修改的文件\n{changed_files}\n\n\
             ## subagent 结果\n{result}\n\n\
             {rubric}",
            task = ctx.task,
            model = ctx.model,
            changed_files = changed_files_str,
            result = result_content,
            rubric = self.rubric,
        )
    }

    /// 从 LLM 响应文本解析 0.0-1.0 分数。
    ///
    /// 容错策略:
    /// 1. 尝试提取首个 `0.0`-`1.0` 范围的浮点数(正则 `\d+\.\d+`)
    /// 2. 若无匹配,尝试纯整数(如 "1" 视为 1.0,"0" 视为 0.0)
    /// 3. 解析失败返回 Err
    fn parse_score(text: &str) -> Result<f64, String> {
        // 优先匹配浮点数
        let re = regex::Regex::new(r"(\d+\.\d+)").map_err(|e| e.to_string())?;
        if let Some(cap) = re.captures(text) {
            if let Ok(score) = cap[1].parse::<f64>() {
                if (0.0..=1.0).contains(&score) {
                    return Ok(score);
                }
            }
        }
        // 回退:纯整数
        let int_re = regex::Regex::new(r"\b(\d+)\b").map_err(|e| e.to_string())?;
        if let Some(cap) = int_re.captures(text) {
            if let Ok(n) = cap[1].parse::<i64>() {
                let score = n as f64;
                if (0.0..=1.0).contains(&score) {
                    return Ok(score);
                }
            }
        }
        Err(format!("无法从 judge 响应解析分数: {text}"))
    }
}

impl ValidationGate for LlmJudgeGate {
    fn validate(&self, ctx: &ValidationContext) -> Result<(), ValidationError> {
        // 无 client 时降级为 stub(向后兼容 P0 行为)
        let client = match &self.client {
            None => {
                crate::diag::global().append(crate::diag::DiagEntry::new(
                    crate::diag::DiagLevel::Warn,
                    "validation",
                    "LlmJudgeGate no client injected: skipping LLM judge (stub mode)",
                ));
                return Ok(());
            }
            Some(c) => c.clone(),
        };

        // 1. 构造 judge prompt
        let prompt = self.build_judge_prompt(ctx);

        // 2. 调用 judge client
        let response = client.judge(&prompt).map_err(|e| ValidationError {
            message: format!("LLM judge 调用失败: {e}"),
            retryable: false, // API 故障不重试(避免无限重试 + 成本失控)
        })?;

        // 空响应降级:judge LLM 返回空文本(如 reasoning-only 响应,content 无 Text 块)时,
        // 与"无 client"一致降级为 stub 放行,避免空响应硬失败所有子代理。
        if response.trim().is_empty() {
            crate::diag::global().append(crate::diag::DiagEntry::new(
                crate::diag::DiagLevel::Warn,
                "validation",
                "LlmJudgeGate empty judge response: skipping LLM judge (stub mode)",
            ));
            return Ok(());
        }

        // 3. 解析分数
        let score = Self::parse_score(&response).map_err(|e| ValidationError {
            message: e.to_string(),
            retryable: false, // 解析失败不重试(LLM 输出格式问题,重试也不一定改善)
        })?;

        crate::diag::global().append(
            crate::diag::DiagEntry::new(
                crate::diag::DiagLevel::Info,
                "validation",
                format!(
                    "LlmJudgeGate score: {score:.3} (threshold: {:.3})",
                    self.pass_threshold
                ),
            )
            .with_field("score", serde_json::Value::from(score))
            .with_field("threshold", serde_json::Value::from(self.pass_threshold)),
        );

        // 4. 阈值比较
        if score < self.pass_threshold {
            return Err(ValidationError {
                message: format!(
                    "LLM judge 评分 {score:.3} 低于阈值 {:.3}",
                    self.pass_threshold
                ),
                retryable: true, // 评分低可重试(换模型或重做任务)
            });
        }

        Ok(())
    }

    fn name(&self) -> &'static str {
        "llm-judge"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx<'a>(
        subagent_id: &'a str,
        task: &'a str,
        result_path: &'a Path,
        workspace_root: &'a Path,
        changed_files: &'a [PathBuf],
        model: &'a str,
    ) -> ValidationContext<'a> {
        make_ctx_with_subagent_files(
            subagent_id,
            task,
            result_path,
            workspace_root,
            changed_files,
            &[],
            model,
        )
    }

    fn make_ctx_with_subagent_files<'a>(
        subagent_id: &'a str,
        task: &'a str,
        result_path: &'a Path,
        workspace_root: &'a Path,
        changed_files: &'a [PathBuf],
        subagent_changed_files: &'a [PathBuf],
        model: &'a str,
    ) -> ValidationContext<'a> {
        ValidationContext {
            subagent_id,
            task,
            result_path,
            workspace_root,
            changed_files,
            subagent_changed_files,
            model,
        }
    }

    #[test]
    fn command_gate_skips_when_no_matching_files() {
        let gate = CommandValidationGate::new(
            "test-gate",
            ["echo", "hello"],
            std::env::temp_dir(),
            r"\.rs$",
        );
        let changed_files: Vec<PathBuf> = vec![PathBuf::from("README.md")];
        let ctx = make_ctx(
            "sub-1",
            "test task",
            Path::new("/tmp/result.md"),
            Path::new("/tmp"),
            &changed_files,
            "test-model",
        );
        // 无 .rs 文件修改,应跳过(返回 Ok)
        assert!(gate.validate(&ctx).is_ok());
    }

    #[test]
    fn command_gate_runs_when_matching_files() {
        // 用一个总是成功的命令 `cmd /c exit 0`(Windows)/ `true`(Unix)
        #[cfg(windows)]
        let cmd = vec![
            "cmd".to_string(),
            "/c".to_string(),
            "exit".to_string(),
            "0".to_string(),
        ];
        #[cfg(not(windows))]
        let cmd = vec!["true".to_string()];

        let tmp = std::env::temp_dir();
        let gate = CommandValidationGate::new("test-gate", cmd, tmp.clone(), r"\.rs$");
        let changed_files: Vec<PathBuf> = vec![PathBuf::from("src/main.rs")];
        let ctx = make_ctx(
            "sub-1",
            "test task",
            Path::new("/tmp/result.md"),
            tmp.as_path(),
            &changed_files,
            "test-model",
        );
        // 有 .rs 文件修改,应触发执行命令(exit 0 / true 总是成功)
        assert!(gate.validate(&ctx).is_ok());
    }

    #[test]
    fn command_gate_returns_retryable_on_failure() {
        #[cfg(windows)]
        let cmd = vec![
            "cmd".to_string(),
            "/c".to_string(),
            "exit".to_string(),
            "1".to_string(),
        ];
        #[cfg(not(windows))]
        let cmd = vec!["false".to_string()];

        let tmp = std::env::temp_dir();
        let gate = CommandValidationGate::new("failing-gate", cmd, tmp.clone(), r"\.rs$");
        let changed_files: Vec<PathBuf> = vec![PathBuf::from("src/main.rs")];
        let ctx = make_ctx(
            "sub-1",
            "test task",
            Path::new("/tmp/result.md"),
            tmp.as_path(),
            &changed_files,
            "test-model",
        );
        let err = gate.validate(&ctx).unwrap_err();
        assert!(err.retryable, "命令失败应可重试");
    }

    #[test]
    fn command_gate_returns_fatal_on_missing_command() {
        let tmp = std::env::temp_dir();
        let gate = CommandValidationGate::new(
            "missing-gate",
            ["this-command-does-not-exist-12345"],
            tmp.clone(),
            r"\.rs$",
        );
        let changed_files: Vec<PathBuf> = vec![PathBuf::from("src/main.rs")];
        let ctx = make_ctx(
            "sub-1",
            "test task",
            Path::new("/tmp/result.md"),
            tmp.as_path(),
            &changed_files,
            "test-model",
        );
        let err = gate.validate(&ctx).unwrap_err();
        assert!(!err.retryable, "环境错误不可重试");
    }

    #[test]
    fn llm_judge_gate_mvp_returns_ok() {
        let tmp = std::env::temp_dir();
        let gate = LlmJudgeGate::diagnostic_default("deepseek-v4-pro", tmp.clone());
        let changed_files: Vec<PathBuf> = vec![];
        let ctx = make_ctx(
            "sub-1",
            "诊断任务",
            Path::new("/tmp/result.md"),
            tmp.as_path(),
            &changed_files,
            "deepseek-v4-flash",
        );
        // MVP stub 应返回 Ok
        assert!(gate.validate(&ctx).is_ok());
        assert_eq!(gate.name(), "llm-judge");
    }

    #[test]
    fn rust_compile_gate_constructs_correctly() {
        let gate = rust_compile_gate(PathBuf::from("/tmp"));
        assert_eq!(gate.gate_name(), "cargo-build");
        assert_eq!(gate.name(), "command");
    }

    #[test]
    fn validation_error_display() {
        let retryable = ValidationError {
            message: "cargo build failed".into(),
            retryable: true,
        };
        assert!(format!("{retryable}").contains("[validation:retryable]"));

        let fatal = ValidationError {
            message: "command not found".into(),
            retryable: false,
        };
        assert!(format!("{fatal}").contains("[validation:fatal]"));
    }

    // ===== Multi-Agent Hardening P0 步骤 9:LlmJudgeGate trait 预留验证 =====
    // 依据 docs/multi-agent-hardening-plan.md §10.4 "LlmJudgeGate trait 预留(P0)"

    /// §10.4 LlmJudgeGate 实现 ValidationGate trait,编译通过
    #[test]
    fn llm_judge_gate_implements_validation_gate_trait() {
        fn assert_validation_gate<T: ValidationGate>(_t: &T) {}
        let gate = LlmJudgeGate::diagnostic_default("deepseek-v4-pro", std::env::temp_dir());
        assert_validation_gate(&gate);
        assert_eq!(gate.name(), "llm-judge");
    }

    /// §10.4 LlmJudgeGate::diagnostic_default 构造的 rubric 含四维:
    /// 根因定位 / 方案可行性 / 完整性 / 副作用评估
    #[test]
    fn llm_judge_gate_diagnostic_default_rubric_contains_four_dimensions() {
        let gate = LlmJudgeGate::diagnostic_default("deepseek-v4-pro", std::env::temp_dir());
        let rubric = &gate.rubric;
        assert!(
            rubric.contains("根因定位"),
            "rubric 缺少根因定位维度: {rubric}"
        );
        assert!(
            rubric.contains("方案可行性"),
            "rubric 缺少方案可行性维度: {rubric}"
        );
        assert!(rubric.contains("完整性"), "rubric 缺少完整性维度: {rubric}");
        assert!(
            rubric.contains("副作用评估"),
            "rubric 缺少副作用评估维度: {rubric}"
        );
        // 权重总和应为 1.0(0.3 + 0.3 + 0.2 + 0.2)
        assert!(rubric.contains("0.3") && rubric.contains("0.2"));
    }

    /// §10.4 LlmJudgeGate 默认 pass_threshold = 0.7
    #[test]
    fn llm_judge_gate_default_pass_threshold_is_0_7() {
        let gate = LlmJudgeGate::diagnostic_default("deepseek-v4-pro", std::env::temp_dir());
        assert!((gate.pass_threshold - 0.7).abs() < 1e-9);
    }

    /// §10.4 LlmJudgeGate MVP 阶段不注册(诊断任务用人工验收 + rust_compile_gate)
    /// 此测试验证 LlmJudgeGate 单独调用时返回 Ok(stub),不影响主流程
    #[test]
    fn llm_judge_gate_mvp_stub_returns_ok_without_judge_call() {
        let gate = LlmJudgeGate::diagnostic_default("deepseek-v4-pro", std::env::temp_dir());
        let changed_files: Vec<PathBuf> = vec![PathBuf::from("src/main.rs")];
        let workspace_root = std::env::temp_dir();
        let ctx = make_ctx(
            "sub-judge",
            "诊断任务",
            Path::new("/tmp/result.md"),
            workspace_root.as_path(),
            &changed_files,
            "deepseek-v4-flash",
        );
        // MVP stub 应返回 Ok,不实际调用 judge model
        assert!(gate.validate(&ctx).is_ok());
    }

    /// §10.4 rust_compile_gate 命名正确(cargo-build / command)
    #[test]
    fn rust_compile_gate_has_correct_naming() {
        let gate = rust_compile_gate(PathBuf::from("/tmp"));
        assert_eq!(gate.gate_name(), "cargo-build");
        assert_eq!(gate.name(), "command");
    }

    // ===== v2 §10.5 Epic 5:LlmJudgeGate 完整实现测试 =====

    /// §10.5 Epic 5:with_client 注入 client 后,高分(0.85 > 0.7)验证通过
    #[test]
    fn llm_judge_gate_with_client_passes_when_score_above_threshold() {
        let tempdir = tempfile::tempdir().expect("create temp dir");
        let result_path = tempdir.path().join("result.md");
        std::fs::write(&result_path, "修复了根因,验证通过").unwrap();

        let mock = MockJudgeClient {
            response: "0.85".to_string(),
            force_error: false,
        };
        let gate =
            LlmJudgeGate::diagnostic_default("deepseek-v4-pro", tempdir.path().to_path_buf())
                .with_client(std::sync::Arc::new(mock));

        let changed_files = vec![PathBuf::from("src/main.rs")];
        let ctx = make_ctx(
            "sub-1",
            "诊断任务",
            &result_path,
            tempdir.path(),
            &changed_files,
            "deepseek-v4-flash",
        );

        assert!(gate.validate(&ctx).is_ok(), "score 0.85 > 0.7 应通过");
    }

    /// §10.5 Epic 5:with_client 注入 client 后,低分(0.40 < 0.7)验证失败(retryable)
    #[test]
    fn llm_judge_gate_with_client_fails_when_score_below_threshold() {
        let tempdir = tempfile::tempdir().expect("create temp dir");
        let result_path = tempdir.path().join("result.md");
        std::fs::write(&result_path, "修复不完整").unwrap();

        let mock = MockJudgeClient {
            response: "0.40".to_string(),
            force_error: false,
        };
        let gate =
            LlmJudgeGate::diagnostic_default("deepseek-v4-pro", tempdir.path().to_path_buf())
                .with_client(std::sync::Arc::new(mock));

        let changed_files = vec![];
        let ctx = make_ctx(
            "sub-1",
            "诊断任务",
            &result_path,
            tempdir.path(),
            &changed_files,
            "deepseek-v4-flash",
        );

        let err = gate.validate(&ctx).expect_err("0.40 < 0.7 应失败");
        assert!(err.retryable, "评分低应可重试");
        assert!(
            err.message.contains("0.40"),
            "错误消息应含分数: {}",
            err.message
        );
        assert!(
            err.message.contains("0.7"),
            "错误消息应含阈值: {}",
            err.message
        );
    }

    /// §10.5 Epic 5:client 调用失败(API 故障)时返回 fatal 错误(retryable=false)
    #[test]
    fn llm_judge_gate_with_client_returns_fatal_error_on_api_failure() {
        let tempdir = tempfile::tempdir().expect("create temp dir");
        let result_path = tempdir.path().join("result.md");
        std::fs::write(&result_path, "any content").unwrap();

        let mock = MockJudgeClient {
            response: String::new(),
            force_error: true,
        };
        let gate =
            LlmJudgeGate::diagnostic_default("deepseek-v4-pro", tempdir.path().to_path_buf())
                .with_client(std::sync::Arc::new(mock));

        let changed_files = vec![];
        let ctx = make_ctx(
            "sub-1",
            "诊断任务",
            &result_path,
            tempdir.path(),
            &changed_files,
            "deepseek-v4-flash",
        );

        let err = gate.validate(&ctx).expect_err("API 故障应失败");
        assert!(!err.retryable, "API 故障不可重试(避免无限重试 + 成本失控)");
        assert!(
            err.message.contains("LLM judge 调用失败"),
            "unexpected: {}",
            err.message
        );
    }

    /// §10.5 Epic 5:LLM 响应含解释文本时仍能解析分数
    #[test]
    fn llm_judge_gate_parse_score_extracts_score_from_explanatory_text() {
        // LLM 常输出"评分: 0.82\n理由: ..."格式
        let score =
            LlmJudgeGate::parse_score("根据 rubric,评分: 0.82\n理由: 根因定位准确...").unwrap();
        assert!((score - 0.82).abs() < 1e-9, "应提取 0.82, got {score}");
    }

    /// §10.5 Epic 5:parse_score 解析纯整数("1" → 1.0, "0" → 0.0)
    #[test]
    fn llm_judge_gate_parse_score_handles_integer_responses() {
        assert!((LlmJudgeGate::parse_score("1").unwrap() - 1.0).abs() < 1e-9);
        assert!((LlmJudgeGate::parse_score("0").unwrap() - 0.0).abs() < 1e-9);
    }

    /// §10.5 Epic 5:parse_score 对无数字或越界数字返回 Err
    #[test]
    fn llm_judge_gate_parse_score_errors_for_invalid_input() {
        assert!(LlmJudgeGate::parse_score("无分数").is_err());
        assert!(LlmJudgeGate::parse_score("2.5").is_err(), "2.5 越界应失败");
        assert!(LlmJudgeGate::parse_score("5").is_err(), "5 越界应失败");
    }

    /// §10.5 Epic 5:parse_score 解析失败时 validate 返回 fatal 错误
    #[test]
    fn llm_judge_gate_returns_fatal_error_when_score_unparseable() {
        let tempdir = tempfile::tempdir().expect("create temp dir");
        let result_path = tempdir.path().join("result.md");
        std::fs::write(&result_path, "content").unwrap();

        let mock = MockJudgeClient {
            response: "我无法评分".to_string(),
            force_error: false,
        };
        let gate =
            LlmJudgeGate::diagnostic_default("deepseek-v4-pro", tempdir.path().to_path_buf())
                .with_client(std::sync::Arc::new(mock));

        let ctx = make_ctx(
            "sub-1",
            "诊断任务",
            &result_path,
            tempdir.path(),
            &[],
            "deepseek-v4-flash",
        );

        let err = gate.validate(&ctx).expect_err("解析失败应报错");
        assert!(!err.retryable, "解析失败不可重试");
    }

    /// 空响应(judge LLM 返回空文本,如 reasoning-only 响应)降级为 stub,避免硬失败所有子代理。
    #[test]
    fn llm_judge_gate_empty_response_degrades_to_stub() {
        let tempdir = tempfile::tempdir().expect("create temp dir");
        let result_path = tempdir.path().join("result.md");
        std::fs::write(&result_path, "content").unwrap();

        let mock = MockJudgeClient {
            response: String::new(),
            force_error: false,
        };
        let gate =
            LlmJudgeGate::diagnostic_default("deepseek-v4-pro", tempdir.path().to_path_buf())
                .with_client(std::sync::Arc::new(mock));

        let ctx = make_ctx(
            "sub-1",
            "诊断任务",
            &result_path,
            tempdir.path(),
            &[],
            "deepseek-v4-flash",
        );

        assert!(
            gate.validate(&ctx).is_ok(),
            "空响应应降级为 stub 返回 Ok"
        );
    }

    /// §10.5 Epic 5:build_judge_prompt 包含 task/model/rubric 三要素
    #[test]
    fn llm_judge_gate_build_prompt_contains_required_sections() {
        let tempdir = tempfile::tempdir().expect("create temp dir");
        let result_path = tempdir.path().join("result.md");
        std::fs::write(&result_path, "修复内容").unwrap();

        let gate =
            LlmJudgeGate::diagnostic_default("deepseek-v4-pro", tempdir.path().to_path_buf());
        let changed_files = vec![PathBuf::from("src/main.rs")];
        let ctx = make_ctx(
            "sub-1",
            "诊断崩溃任务",
            &result_path,
            tempdir.path(),
            &changed_files,
            "deepseek-v4-flash",
        );

        let prompt = gate.build_judge_prompt(&ctx);
        assert!(prompt.contains("诊断崩溃任务"), "prompt 应含 task");
        assert!(
            prompt.contains("deepseek-v4-flash"),
            "prompt 应含 subagent model"
        );
        assert!(prompt.contains("src/main.rs"), "prompt 应含 changed_files");
        assert!(prompt.contains("修复内容"), "prompt 应含 result_content");
        assert!(prompt.contains("根因定位"), "prompt 应含 rubric");
    }

    /// §10.5 Epic 5:无 client 时降级为 stub(向后兼容 P0 行为)
    #[test]
    fn llm_judge_gate_without_client_degrades_to_stub() {
        let tempdir = tempfile::tempdir().expect("create temp dir");
        let result_path = tempdir.path().join("result.md");
        let gate =
            LlmJudgeGate::diagnostic_default("deepseek-v4-pro", tempdir.path().to_path_buf());
        // 不调用 with_client,client 为 None
        let ctx = make_ctx(
            "sub-1",
            "诊断任务",
            &result_path,
            tempdir.path(),
            &[],
            "deepseek-v4-flash",
        );
        // stub 模式应返回 Ok
        assert!(
            gate.validate(&ctx).is_ok(),
            "无 client 时应降级为 stub 返回 Ok"
        );
    }

    /// §10.5 Epic 5:阈值边界 — 分数恰好等于阈值应通过
    #[test]
    fn llm_judge_gate_passes_when_score_equals_threshold() {
        let tempdir = tempfile::tempdir().expect("create temp dir");
        let result_path = tempdir.path().join("result.md");
        std::fs::write(&result_path, "content").unwrap();

        let mock = MockJudgeClient {
            response: "0.7".to_string(), // 恰好等于默认阈值
            force_error: false,
        };
        let gate =
            LlmJudgeGate::diagnostic_default("deepseek-v4-pro", tempdir.path().to_path_buf())
                .with_client(std::sync::Arc::new(mock));

        let ctx = make_ctx(
            "sub-1",
            "诊断任务",
            &result_path,
            tempdir.path(),
            &[],
            "deepseek-v4-flash",
        );

        assert!(gate.validate(&ctx).is_ok(), "分数 0.7 == 阈值 0.7 应通过");
    }

    // ===== Epic 5 §8.4:双列表交叉检查 =====

    /// §8.4:子智能体声称改了但 git diff 没有 → unverified 非空;
    /// git diff 有但子智能体没声称 → concurrent 非空。
    #[test]
    fn compute_mismatch_detects_unverified_and_concurrent() {
        let tmp = std::env::temp_dir();
        // git diff 显示:a.rs, b.rs
        let changed_files = vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")];
        // 子智能体声称:a.rs(已落盘), c.rs(声称但 git diff 没有)
        let subagent_changed_files = vec![PathBuf::from("src/a.rs"), PathBuf::from("src/c.rs")];
        let ctx = make_ctx_with_subagent_files(
            "sub-1",
            "fix bug",
            Path::new("/tmp/result.md"),
            tmp.as_path(),
            &changed_files,
            &subagent_changed_files,
            "deepseek-v4-pro",
        );
        let (unverified, concurrent) = compute_changed_files_mismatch(&ctx);
        assert!(
            unverified.iter().any(|f| f == "src/c.rs"),
            "c.rs 声称但 git diff 没有,应在 unverified: {unverified:?}"
        );
        assert!(
            concurrent.iter().any(|f| f == "src/b.rs"),
            "b.rs 在 git diff 但子智能体没声称,应在 concurrent: {concurrent:?}"
        );
    }

    /// §8.4:子智能体声称的变更与 git diff 完全一致 → 无 mismatch。
    #[test]
    fn compute_mismatch_empty_when_lists_agree() {
        let tmp = std::env::temp_dir();
        let changed_files = vec![PathBuf::from("src/a.rs")];
        let subagent_changed_files = vec![PathBuf::from("src/a.rs")];
        let ctx = make_ctx_with_subagent_files(
            "sub-1",
            "fix bug",
            Path::new("/tmp/result.md"),
            tmp.as_path(),
            &changed_files,
            &subagent_changed_files,
            "deepseek-v4-pro",
        );
        let (unverified, concurrent) = compute_changed_files_mismatch(&ctx);
        assert!(
            unverified.is_empty(),
            "一致时 unverified 应为空: {unverified:?}"
        );
        assert!(
            concurrent.is_empty(),
            "一致时 concurrent 应为空: {concurrent:?}"
        );
    }

    /// §8.4:子智能体无声称变更(Analyze/ReadOnly/旧格式)→ 跳过,返回空。
    #[test]
    fn compute_mismatch_skips_when_subagent_claims_nothing() {
        let tmp = std::env::temp_dir();
        let changed_files = vec![PathBuf::from("src/a.rs")];
        let ctx = make_ctx_with_subagent_files(
            "sub-1",
            "analyze task",
            Path::new("/tmp/result.md"),
            tmp.as_path(),
            &changed_files,
            &[], // 子智能体无声称变更
            "deepseek-v4-flash",
        );
        let (unverified, concurrent) = compute_changed_files_mismatch(&ctx);
        assert!(unverified.is_empty());
        assert!(
            concurrent.is_empty(),
            "无声称变更时不报 concurrent(避免噪声)"
        );
    }

    /// §8.4:`read_handoff_changed_files` 解析失败时返回空(向后兼容旧格式)。
    #[test]
    fn read_handoff_changed_files_returns_empty_on_missing_file() {
        let files = read_handoff_changed_files(Path::new("/nonexistent/handoff-12345.md"));
        assert!(files.is_empty(), "文件不存在时应返回空 Vec");
    }

    // ===== Epic 1 T3:changed_files 喂 validation gate =====

    /// T3:detect_changed_files 合并 untracked 新文件。
    /// `git diff --name-only HEAD` 检测不到 untracked,须补 `git ls-files --others`,
    /// 否则子代理 write_file 新建文件(Execute 能力)会跳过验证链。
    #[test]
    fn detect_changed_files_includes_untracked() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let root = tmp.path();
        // git 不可用(如精简环境)→ 跳过本测试
        let init = std::process::Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output();
        if !init.ok().is_some_and(|o| o.status.success()) {
            eprintln!("git unavailable, skipping detect_changed_files test");
            return;
        }
        std::fs::write(root.join("tracked.txt"), "v1").expect("write tracked");

        // 首次提交(tracked 基线)
        let _ = std::process::Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(root)
            .output();
        let commit = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@test.com",
                "commit",
                "-m",
                "init",
            ])
            .current_dir(root)
            .output();
        if !commit.ok().is_some_and(|o| o.status.success()) {
            eprintln!("git commit unavailable, skipping detect_changed_files test");
            return;
        }

        // 修改 tracked + 新建 untracked
        std::fs::write(root.join("tracked.txt"), "v2").expect("modify tracked");
        std::fs::write(root.join("untracked.txt"), "new").expect("write untracked");

        let files = detect_changed_files(root);
        assert!(
            files.iter().any(|f| f.to_string_lossy() == "tracked.txt"),
            "tracked modification should be detected: {files:?}"
        );
        assert!(
            files.iter().any(|f| f.to_string_lossy() == "untracked.txt"),
            "untracked new file should be detected: {files:?}"
        );
        // 去重后无重复
        let mut uniq = files.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), files.len(), "no duplicates expected: {files:?}");
    }

    /// T3:双列表比对归一化 — 声称的绝对路径与 git diff 相对路径视为一致。
    /// 修复前绝对/相对恒不匹配,unverified/concurrent 恒非空(诊断噪声)。
    #[test]
    fn compute_mismatch_normalizes_absolute_claimed_paths() {
        let tmp = std::env::temp_dir();
        // git diff 相对路径:src/a.rs;声称绝对路径:<root>/src/a.rs → 应一致(无 mismatch)
        let changed_files = vec![PathBuf::from("src/a.rs")];
        let subagent_changed_files = vec![tmp.join("src/a.rs")];
        let ctx = make_ctx_with_subagent_files(
            "sub-1",
            "fix bug",
            Path::new("/tmp/result.md"),
            tmp.as_path(),
            &changed_files,
            &subagent_changed_files,
            "deepseek-v4-pro",
        );
        let (unverified, concurrent) = compute_changed_files_mismatch(&ctx);
        assert!(
            unverified.is_empty(),
            "absolute claimed path should match git diff relative: {unverified:?}"
        );
        assert!(
            concurrent.is_empty(),
            "no concurrent mismatch expected: {concurrent:?}"
        );
    }

    /// T3:gate 触发补充声称变更 — git diff 无记录但子代理声称改了 .rs 文件 → 触发。
    /// 用必然失败的命令验证"确实触发了"(skip 时返回 Ok,触发失败返回 Err)。
    #[test]
    fn command_gate_triggers_on_claimed_changes() {
        #[cfg(windows)]
        let cmd = vec![
            "cmd".to_string(),
            "/c".to_string(),
            "exit".to_string(),
            "1".to_string(),
        ];
        #[cfg(not(windows))]
        let cmd = vec!["false".to_string()];

        let tmp = std::env::temp_dir();
        let gate = CommandValidationGate::new("failing-gate", cmd, tmp.clone(), r"\.rs$");
        // git diff 无记录(changed_files 空),但子代理声称改了 src/main.rs
        let changed_files: Vec<PathBuf> = vec![];
        let subagent_changed_files: Vec<PathBuf> = vec![PathBuf::from("src/main.rs")];
        let ctx = make_ctx_with_subagent_files(
            "sub-1",
            "fix bug",
            Path::new("/tmp/result.md"),
            tmp.as_path(),
            &changed_files,
            &subagent_changed_files,
            "deepseek-v4-flash",
        );
        let err = gate
            .validate(&ctx)
            .expect_err("claimed .rs change should trigger failing gate");
        assert!(err.retryable, "触发后命令失败应可重试");
    }

    /// T3:gate 触发 — 声称的变更与 filter 不匹配(README.md)时仍跳过。
    #[test]
    fn command_gate_skips_when_claimed_changes_irrelevant() {
        #[cfg(windows)]
        let cmd = vec![
            "cmd".to_string(),
            "/c".to_string(),
            "exit".to_string(),
            "1".to_string(),
        ];
        #[cfg(not(windows))]
        let cmd = vec!["false".to_string()];

        let tmp = std::env::temp_dir();
        let gate = CommandValidationGate::new("failing-gate", cmd, tmp.clone(), r"\.rs$");
        let changed_files: Vec<PathBuf> = vec![];
        let subagent_changed_files: Vec<PathBuf> = vec![PathBuf::from("README.md")];
        let ctx = make_ctx_with_subagent_files(
            "sub-1",
            "fix bug",
            Path::new("/tmp/result.md"),
            tmp.as_path(),
            &changed_files,
            &subagent_changed_files,
            "deepseek-v4-flash",
        );
        assert!(
            gate.validate(&ctx).is_ok(),
            "claimed non-.rs change should skip gate"
        );
    }
}
