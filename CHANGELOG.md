# Changelog

All notable changes to this project will be documented in this file.

## [2026.8.8] - 2026-08-11

### Added

- **Self-evolving Harness MVP（design-gaps #2）** — 新增 `runtime/harness_evolution` 模块：Weakness Mining（复用 `TraceAnalyzer::cluster_failures` 聚类失败）+ 规则式 Mixed Proposer（7+ 种预定义错误模式，零 LLM 调用）+ 两重门控验证（Validity 基础设施噪声过滤 + Significance z-test, alpha=0.05）防 misevolution，Active edits（≤10 条）全量注入 `SystemPromptSplit::dynamic_sections`
- **`claw harness` CLI 子命令** — `list [--status]` / `stats` / `rollback --all | --id` / `evolve --dry-run`，操作 `.claw/decision_log.db` 中 `harness_edits` 表，支持 `--output-format json`
- **Trace 记录新增 `task_success` 列** — CSV 表头扩展为 7 列，旧 6 列格式仍可加载（按 `failure_kind` 推断），向后兼容
- **Hooks 配置热重载（design-gaps #1）** — 每 turn 检查配置源 mtime，`Arc<RwLock>` 原子替换 hooks 配置，会话无需重启
- **异步 HookRunner** — 生命周期事件 fire-and-forget 后台执行，决策事件保留同步 + 60s 全局预算兜底
- **ACP/IDE 应用层闭环（design-gaps #6，0.10.4 + 1.3 双路径）** — `claw-shell` 1.3 路径 Stage 3 完成：`ClawAgentV13Builder` / `AgentCommand` / `create_session` / `run_prompt` / `cancel`（含 HookAbortSignal），`stdio_v1_3.rs` 升级为真实 handler（initialize/auth/session-new/session-prompt/session-cancel）；`claw acp serve` 接线 1.3（`acp-1_5` feature）
- **VS Code 扩展首次运行配置向导** — 新增 `setup-wizard.ts`：自动检测 claw binary 与 API key（SecretStorage 存储），交互式引导缺失项，核心逻辑可单测
- **VS Code 扩展 ACP 链路修复** — binary 名 `claw-headless`→`claw-plus-headless`、默认 model 对齐 `deepseek-v4-flash`、`session/new` 补必填 `mcpServers`、`SessionUpdate` wire 格式修正（内部 tag `sessionUpdate` + snake_case variant）、新增 `scripts/acp-smoke-test.mjs` 全链路 smoke test（initialize + session/new + prompt + assistant 文本推送）
- **IM 桥接 Agent 工作区配置** — `[agent]` 段支持 `workspace_root` / `workspace_roots`；未配置时自动枚举本机所有盘符根作为白名单（默认最大权限，零配置跨盘访问）
- **IM 桥接会话持久化防覆盖** — 进程重启后首轮 persist 与磁盘历史元数据合并去重（活跃优先），不再用空内存 map 覆盖历史记录；原子写改用唯一临时文件名（pid + 纳秒）消除多实例并发冲突
- **turn 迭代软硬双层护栏** — 新增 `SOFT_MAX_ITERATIONS=64`（注入收敛警告，仅触发一次），硬上限提升至 192，消除对长程只读分析任务的误杀
- **ToolResultCallback** — runtime 内置工具完成回调，TUI 注入后转发为 `StatusEvent::ToolResult` 闭合 ToolCard
- **Windows 一键安装脚本** — 新增 `install.ps1`（镜像 install.sh：环境检测 → Rust 工具链检查 → 构建 → 部署 `~/.cargo/bin` → 验证 → API key 引导）
- **Markdown 流式渲染保留尾部换行** — `render_markdown_with_width_trim(.., false)` 修复跨 flush 段落/表格内容粘连错位

### Changed

- hooks 生命周期事件（SessionStart/Stop 等）改为异步后台执行，决策事件（PreToolUse 等）保持同步
- ACP 版本通过 feature 显式控制：`claw-shell` 默认 `acp-0_10`，`--features acp-1_5` 走 1.3 路径
- 修复 `ClawAgentV13` 在 async 上下文 drop 内部 tokio Runtime 的 panic（Rc 双层持有）

### Fixed

- LSP 路径匹配补 Windows 大小写不敏感（`normalize_lsp_path` 统一抽象）
- 深层 Markdown 流式增量渲染段落/表格跨 flush 粘连错位
- IM 桥接持久化多实例并发写 `os error 5`（唯一 tmp 文件名）
- VS Code 扩展 `session/new` 缺 `mcpServers` 导致 `-32602 Invalid params`

## [1.0.0] - 2026-07-24

### Added

- Initial public release of Claw Plus
- Multi-agent orchestration with DAG-based task decomposition
- IM bridge connectors (WeChat Work, Feishu)
- Completion verifier and slop scanner for output quality
- VS Code extension for IDE integration
- GitHub Actions CI/CD with automated binary releases
- Cross-platform support (Linux x64, macOS ARM64, Windows x64)

[1.0.0]: https://github.com/dong382258137/claw-code/releases/tag/v1.0.0
