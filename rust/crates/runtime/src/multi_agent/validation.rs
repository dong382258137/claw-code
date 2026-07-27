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
    /// `git diff --name-only` 检测到的修改文件列表。
    pub changed_files: &'a [PathBuf],
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
        let triggered = ctx
            .changed_files
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
                format!("gate '{}' triggered for subagent {}", self.gate_name, ctx.subagent_id),
            )
            .with_field("gate", serde_json::Value::String(self.gate_name.clone()))
            .with_field(
                "command",
                serde_json::Value::String(self.command.join(" ")),
            ),
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
                    crate::diag::DiagEntry::new(
                        crate::diag::DiagLevel::Error,
                        "validation",
                        &msg,
                    )
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
                    crate::diag::DiagEntry::new(
                        crate::diag::DiagLevel::Error,
                        "validation",
                        &msg,
                    )
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
    CommandValidationGate::new(
        "cargo-build",
        ["cargo", "build"],
        workspace_root,
        r"\.rs$",
    )
}

/// 从 `git diff --name-only` 检测 subagent 修改的文件。
///
/// 在 [`MultiAgentCoordinator::validate`] 调用前执行,结果填入 [`ValidationContext`]。
///
/// [`MultiAgentCoordinator::validate`]: super::MultiAgentCoordinator::validate
pub fn detect_changed_files(workspace_root: &Path) -> Vec<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", "HEAD"])
        .current_dir(workspace_root)
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(PathBuf::from)
            .collect(),
        _ => Vec::new(), // 非 git 仓库或 git 不可用,返回空(门禁可能跳过)
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
/// # MVP 边界
/// **trait 设计在 MVP 就位,实现留待 v2**。
/// 诊断任务 MVP 阶段用人工验收 + `rust_compile_gate` 双重确认。
#[derive(Debug, Clone)]
pub struct LlmJudgeGate {
    /// 评判模型(建议用旗舰,如 deepseek-v4-pro,保证判断质量)。
    pub judge_model: String,
    /// 评分标准(rubric),注入到 judge prompt。
    pub rubric: String,
    /// 通过阈值(0.0-1.0),低于此分则 validation 失败。
    pub pass_threshold: f64,
    /// workspace_root(用于读取 changed_files 内容供 judge 参考)。
    pub workspace_root: PathBuf,
}

impl LlmJudgeGate {
    /// 诊断任务默认 rubric(借鉴 Anthropic rubric 设计)。
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
        }
    }
}

impl ValidationGate for LlmJudgeGate {
    fn validate(&self, ctx: &ValidationContext) -> Result<(), ValidationError> {
        // MVP 边界:trait 已就位,实现留待 v2。
        // 当前返回 Ok,假设通过(诊断任务 MVP 阶段用人工验收 + rust_compile_gate 双重确认)。
        // v2 实现时,此处应:
        // 1. 读取 ctx.result_path 内容
        // 2. 构造 judge prompt(任务 + 模型 + changed_files + 结果 + rubric)
        // 3. 调用 ProviderClient::from_model(&self.judge_model)
        // 4. 解析 0.0-1.0 分数
        // 5. 与 self.pass_threshold 比较
        let _ = ctx;
        crate::diag::global().append(crate::diag::DiagEntry::new(
            crate::diag::DiagLevel::Warn,
            "validation",
            "LlmJudgeGate MVP stub: skipping LLM judge (v2 implementation pending)",
        ));
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
        ValidationContext {
            subagent_id,
            task,
            result_path,
            workspace_root,
            changed_files,
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
        let cmd = vec!["cmd".to_string(), "/c".to_string(), "exit".to_string(), "0".to_string()];
        #[cfg(not(windows))]
        let cmd = vec!["true".to_string()];

        let tmp = std::env::temp_dir();
        let gate = CommandValidationGate::new(
            "test-gate",
            cmd,
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
        // 有 .rs 文件修改,应触发执行命令(exit 0 / true 总是成功)
        assert!(gate.validate(&ctx).is_ok());
    }

    #[test]
    fn command_gate_returns_retryable_on_failure() {
        #[cfg(windows)]
        let cmd = vec!["cmd".to_string(), "/c".to_string(), "exit".to_string(), "1".to_string()];
        #[cfg(not(windows))]
        let cmd = vec!["false".to_string()];

        let tmp = std::env::temp_dir();
        let gate = CommandValidationGate::new(
            "failing-gate",
            cmd,
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
        let gate = LlmJudgeGate::diagnostic_default(
            "deepseek-v4-pro",
            tmp.clone(),
        );
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
        assert!(rubric.contains("根因定位"), "rubric 缺少根因定位维度: {rubric}");
        assert!(rubric.contains("方案可行性"), "rubric 缺少方案可行性维度: {rubric}");
        assert!(rubric.contains("完整性"), "rubric 缺少完整性维度: {rubric}");
        assert!(rubric.contains("副作用评估"), "rubric 缺少副作用评估维度: {rubric}");
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
}
