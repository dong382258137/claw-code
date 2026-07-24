# Claw Plus

<p align="center">
  <a href="https://github.com/dong382258137/claw-code">dong382258137/claw-code</a>
  ·
  <a href="./USAGE.md">Usage</a>
  ·
  <a href="./rust/README.md">Rust workspace</a>
  ·
  <a href="./PARITY.md">Parity</a>
  ·
  <a href="./ROADMAP.md">Roadmap</a>
  ·
  <a href="./CONTRIBUTING.md">Contributing</a>
  ·
  <a href="./SECURITY.md">Security</a>
  ·
  <a href="https://github.com/dong382258137/claw-code">GitHub</a>
</p>

<p align="center">
  <img src="assets/claw-hero.jpeg" alt="Claw Plus" width="300" />
</p>

## 📜 项目渊源 (Project Lineage)

本项目有清晰的来源链路，每一层都基于上一层进行改进和扩展：

```
Anthropic Claude Code（概念来源 / 架构参考）
  └─ ultraworkers/claw-code（MIT License 开源实现）
       └─ dong382258137/claw-code → Claw Plus（本仓库）
            ├─ IM 桥接集成 (IM Bridge)
            └─ 其他功能增强与定制
```

| 层级 | 项目 | 角色 |
|------|------|------|
| 概念层 | [Anthropic Claude Code](https://claude.ai/code) | Anthropic 公司的闭源产品，CLI AI 编程助手的原始概念和架构参考 |
| 实现层 | [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code) | MIT License 开源实现，本项目直接 fork 的基座仓库 |
| 扩展层 | **Claw Plus**（本仓库） | 在上游基础上增加 IM 桥接等功能的二次开发 |

> [!IMPORTANT]
> **本仓库是 [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code) 的 fork（MIT License）。**
> - 上游版权 © 2026 UltraWorkers and Claw Plus contributors
> - 下游修改版权 © 2026 dong382258137（IM 桥接及下游改动）
> - "Claude" 和 "Claude Code" 是 Anthropic 的商标，本项目与 Anthropic 无关联，亦非其官方产品

## 参考与借鉴的项目 (Referenced Projects)

以下项目为本项目的设计和实现提供了重要的参考和灵感，但它们是**独立维护的第三方项目**，各有自己的作者和许可证：

| 项目 | 用途 | 作者/维护者 |
|------|------|-------------|
| [clawhip](https://github.com/Yeachan-Heo/clawhip) | 事件与通知路由系统 | [Yeachan-Heo](https://github.com/Yeachan-Heo) |
| [oh-my-openagent](https://github.com/code-yeongyu/oh-my-openagent) | 多智能体协调框架 | [code-yeongyu](https://github.com/code-yeongyu) |
| [oh-my-codex](https://github.com/Yeachan-Heo/oh-my-codex) | 工作流执行层 | [Yeachan-Heo](https://github.com/Yeachan-Heo) |
| [oh-my-claudecode](https://github.com/Yeachan-Heo/oh-my-claudecode) | Claude Code 工作流 | [Yeachan-Heo](https://github.com/Yeachan-Heo) |

> 以上项目均为独立开源项目。本仓库的 PHILOSOPHY.md 等文档中提及它们是为了说明多智能体协作生态的完整图景，并非声称对其拥有所有权。

## 本仓库的独立贡献 (Original Contributions)

在继承上游代码的基础上，本仓库增加了以下原创功能：

- **IM 桥接集成 (IM Bridge)** — 位于 `rust/crates/im-bridge/`
- **多智能体 DAG 并行编排** — 依赖图调度、worktree 隔离、异步子智能体分发
- **现代 TUI 终端界面** — 侧边栏、状态栏、上下文窗口进度条、工具卡片渲染
- **插件与扩展生态** — 插件管理器、Skills 系统、MCP 服务器生命周期、Hooks
- **其他功能增强与定制**

> 完整特性清单见下方 [✨ 核心特性 / Key Features](#-核心特性--key-features) 章节。

---

## ✨ 核心特性 / Key Features

### 🚀 高性能 Rust 引擎 / High-Performance Rust Engine

全项目使用 Rust 语言重写，编译为单一原生二进制文件，无运行时依赖。

The entire project is rewritten in Rust, compiling to a single native binary with zero runtime dependencies.

| 指标 Metric | 数据 Value |
|---|---|
| 代码规模 Codebase | ~20,000+ 行 Rust / lines of Rust |
| Crate 数量 Crates | 9+ 个工作区 crate / workspace crates |
| 启动速度 Startup | <50ms（冷启动）/ <50ms (cold start) |
| 内存占用 Memory | <50MB（空闲时）/ <50MB (idle) |

- **原生工具执行 / Native tool execution** — Bash、PowerShell、文件 I/O 等工具绕过脚本层，直接在 Rust 中调用系统 API
- **零拷贝流式处理 / Zero-copy streaming** — SSE 事件流直接解析，无中间序列化开销
- **编译时安全 / Compile-time safety** — Rust 的所有权模型在编译期消除内存错误、数据竞争和空指针

### 🌐 多 Provider 兼容 / Multi-Provider Support

一套 CLI 接口适配所有主流 LLM Provider，通过模型名前缀自动路由。

One CLI interface for all major LLM providers, with prefix-based automatic routing.

| Provider | 前缀 Prefix | 认证方式 Auth |
|---|---|---|
| Anthropic (Claude) | `anthropic/` 或无前缀 / or bare | `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` |
| OpenAI (GPT-4, GPT-5) | `openai/` 或 `gpt-` / or `gpt-` | `OPENAI_API_KEY` |
| xAI (Grok) | `grok/` | `XAI_API_KEY` |
| 阿里 DashScope (Qwen) | `qwen/` | `DASHSCOPE_API_KEY` |
| OpenRouter | `openai/` + custom base URL | `OPENAI_API_KEY` |
| Ollama / 本地模型 / Local | `openai/` + custom base URL | 无需 / None |

- **模型别名 / Model aliases** — `opus` → `claude-opus-4-6`, `sonnet` → `claude-sonnet-4-6`, `haiku` → `claude-haiku-4-5`；支持自定义别名
- **流式 SSE / Streaming SSE** — 所有 Provider 统一通过 Server-Sent Events 实时返回 token
- **智能回退 / Smart fallback** — 根据模型名前缀自动选择正确的 API endpoint 和认证方式
- **推理强度控制 / Reasoning effort control** — `--reasoning-effort low|medium|high` 支持 OpenAI 推理模型

### 🖥️ 现代化终端 UI / Modern Terminal UI (TUI)

基于 Ratatui 构建的全屏终端界面，实时展示会话状态和交互进度。

A full-screen terminal interface built on Ratatui with real-time session state and interaction progress.

- **侧边栏 / Sidebar** — 实时显示已加载的 Skills、Agents、MCP 服务器和会话信息
- **状态栏 / Status bar** — 模型名、费用统计、Token 消耗、上下文窗口利用率一目了然
- **上下文窗口进度条 / Context window progress bar** — 可视化当前上下文窗口的使用率，防止溢出
- **工具卡片渲染 / Tool card rendering** — 每个工具调用以带边框的卡片展示（同会话框），含状态图标、输入/输出预览
- **流式输出 / Streaming output** — 模型回复逐 token 实时渲染，支持 Markdown 语法高亮
- **斜杠命令集成 / Slash command integration** — 50+ 斜杠命令直接在 TUI 中用结构化面板展示结果

### 🤖 多智能体 DAG 并行编排 / Multi-Agent DAG Orchestration

声明式依赖图调度器，将复杂任务自动分解为并行子任务，在隔离的工作空间中并发执行。

A declarative dependency graph scheduler that decomposes complex tasks into parallel subtasks and executes them concurrently in isolated workspaces.

- **DAG 依赖图 / DAG dependency graph** — 基于 petgraph 的有向无环图，声明节点间的依赖关系，自动计算并行度
- **三种协调模式 / Three coordination modes**：
  - `fork` — 共享工作目录，适合只读并行探索
  - `teammate` — 共享工作目录 + 任务注册表，适合协作
  - `worktree` — 独立 Git worktree 隔离，适合并发写操作（**安全默认**）
- **异步子智能体分发 / Async sub-agent dispatch** — `dispatch_subagent` + `check_subagent`，支持轮询和结果收集
- **Worker 全生命周期 / Full Worker lifecycle** — `WorkerCreate → WorkerObserve → WorkerResolveTrust → WorkerAwaitReady → WorkerSendPrompt → WorkerObserveCompletion`
- **最多 4 节点并行 / Up to 4-way parallel execution** — DAG 运行时自动识别无依赖节点并并行调度
- **SAGA 补偿模式 / SAGA compensation pattern** — 节点失败时自动执行补偿操作，保证分布式一致性
- **TeamCreate 团队编排 / TeamCreate orchestration** — 将多个任务组合为命名团队，统一监控

### 🔌 插件与扩展生态 / Plugin & Extension Ecosystem

完整的插件生命周期管理 + Skills 系统 + MCP 协议支持 + Hooks 系统 + 自定义 Agent。

Full plugin lifecycle management + Skills system + MCP protocol support + Hooks system + custom Agents.

- **插件管理器 / Plugin manager** — `claw plugins [list|install|enable|disable|update|uninstall]`（JSON 输出支持）
- **Skills 技能系统 / Skills system** — 可发现、加载和调用的可复用技能模块（`/skills list|install|invoke`）
- **MCP 服务器 / MCP servers** — 完整的 Model Context Protocol 生命周期（config → spawn → initialize → tool discovery → invoke → cleanup），支持 stdio/http/sse/ws 传输
- **Hooks 钩子系统 / Hooks system** — PreToolUse / PostToolUse / PostToolUseFailure / Stop / Notification / UserPromptSubmit / SessionStart / PreCompact / SubagentStop
- **自定义 Agent / Custom agents** — TOML 和 Markdown (YAML frontmatter) 格式的 Agent 定义，支持自定义 model、tools、reasoning_effort

### 🛡️ 安全与权限 / Security & Permissions

多层安全防护，从权限模式到沙箱隔离。

Multi-layered security from permission modes to sandbox isolation.

- **三级权限模式 / Three permission modes**：
  - `read-only` — 仅读取文件和搜索（最安全）
  - `workspace-write` — 允许工作区内写入和编辑
  - `danger-full-access` — 完全访问（需显式 opt-in）
- **细粒度规则 / Fine-grained rules** — `permissions.allow` / `permissions.deny` / `permissions.ask`，支持按工具名 + 匹配模式控制
- **工具白名单 / Tool allowlist** — `--allowedTools` 精确控制可用工具集
- **沙箱支持 / Sandbox support** — Linux namespace 隔离（`unshare`）、macOS 文件系统提示
- **Broad-CWD 防护 / Broad-CWD guardrail** — 检测并阻止从 `$HOME` 或根目录启动（防止意外全局扫描）
- **Confirm-on-first-use** — 首次使用 MCP 服务器工具时弹出交互式权限提示

### 💬 IM 桥接 / IM Bridge

通过 Discord 等即时通讯平台远程操控 Claw Plus，实现"人在手机上，代码在服务器上"的工作流。

Remote control of Claw Plus via instant messaging platforms like Discord, enabling "human on mobile, code on server" workflows.

- **Discord 集成 / Discord integration** — 在 Discord 频道中发一条消息即可驱动 Claw Plus 执行任务
- **通知路由 / Notification routing** — 将 Agent 状态变更（任务完成/阻塞/失败）推送至 IM 频道
- **频道级协调 / Channel-level coordination** — 多人通过 Discord 频道协作驱动多智能体并行工作
- **异步工作流 / Async workflow** — 发送指令后可离线，Claw Plus 完成后通知结果

> 位于 `rust/crates/im-bridge/`

### 🔧 卓越的开发体验 / Superior Developer Experience

从健康检查到会话管理，每一个细节都为开发者打磨。

From health checks to session management, every detail is polished for developers.

- **50+ 斜杠命令 / 50+ slash commands** — `/doctor`, `/status`, `/diff`, `/commit`, `/pr`, `/export`, `/skills`, `/agents`, `/mcp`, `/config`, `/session`, `/compact`, `/cost`, `/usage`, `/stats`, `/tokens`, `/cache`, `/context`, `/memory`, `/hooks`, `/plugins`, `/tasks`, `/cron`, `/team`, `/subagent` 等
- **Doctor 健康检查 / Doctor health check** — 一键诊断 API 密钥、配置文件、工作区状态、沙箱状态、安装来源
- **配置层级 / Configuration hierarchy** — `~/.claw/settings.json` (用户) → `.claw.json` (项目) → `.claw/settings.json` (项目) → `.claw/settings.local.json` (本地)
- **会话管理 / Session management** — 持久化、恢复、列表、切换、分支、删除、导出（JSONL 格式）
- **上下文压缩 / Context compaction** — 自动检测长会话并智能压缩，保留关键信息的同时控制 token 消耗
- **Prompt Cache 优化 / Prompt cache optimization** — 自动利用 Anthropic/OpenAI 的 prompt caching 降低费用
- **Git 集成 / Git integration** — 自动检测分支、生成 diff、创建 commit、发起 PR
- **Brace 展开 / Brace expansion** — `glob_search` 原生支持 `**/*.{rs,toml,md}` 花括号展开模式
- **机器可读 JSON 输出 / Machine-readable JSON output** — 所有 CLI 命令支持 `--output-format json`
- **Tab 补全 / Tab completion** — REPL 中自动补全斜杠命令、模型别名、权限模式和会话 ID

### 📊 可观测性 / Observability

结构化的遥测和诊断数据，让运维和调试不再靠"看日志猜"。

Structured telemetry and diagnostics — no more "reading logs and guessing."

- **Lane 事件模式 / Lane event schema** — 23 种类型化事件（Started/Blocked/Failed/Finished/CommitCreated/PROpened/MergeReady 等）
- **WhiP 就绪状态 / WhiP-ready** — 与 [clawhip](https://github.com/Yeachan-Heo/clawhip) 事件路由系统深度集成
- **结构化错误 / Typed errors** — 错误带 `kind` 判别器（`missing_credentials`, `session_not_found`, `cli_parse`, `api_http_error` 等），无需正则匹配
- **费用追踪 / Cost tracking** — 实时累计 Token 消耗和 API 费用，支持会话级别和全局级别统计
- **Session trace / Telemetry sink** — Turn 级别的事件追踪，可接入外部遥测后端

### 🐳 CI/CD 友好 / CI/CD Friendly

为容器化和自动化流水线设计的一等公民支持。

First-class support for containerized and automated pipelines.

- **容器优先 / Container-first** — 提供 `Containerfile` + [`docs/container.md`](./docs/container.md) 完整文档
- **Mock 服务 / Mock service** — 确定性 Anthropic 兼容 mock 服务，用于 CI 端到端测试
- **GitHub Actions CI** — `fmt --check` + `clippy --workspace -- -D warnings` + `cargo test --workspace`
- **Release 构建 / Release artifacts** — 自动化 Tag 触发 release 构建和二进制发布
- **Windows 支持 / Windows support** — PowerShell + Git Bash 双路径，完整安装文档

---

Claw Plus is a Rust implementation of the `claw-plus` CLI agent harness.
The canonical implementation lives in [`rust/`](./rust). This fork builds on
the upstream [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code)
project with additional features and modifications.

> [!IMPORTANT]
> Start with [`USAGE.md`](./USAGE.md) for build, auth, CLI, session, and parity-harness workflows. For file submission/navigation questions, see [Navigation and file context](./docs/navigation-file-context.md). For local OpenAI-compatible models and offline skill installs, see [Local OpenAI-compatible providers and skills setup](./docs/local-openai-compatible-providers.md). Windows users can jump to the PowerShell-first [Windows install and release quickstart](./docs/windows-install-release.md). Make `claw doctor` your first health check after building, use [`rust/README.md`](./rust/README.md) for crate-level details, read [`PARITY.md`](./PARITY.md) for the current Rust-port checkpoint, and see [`docs/container.md`](./docs/container.md) for the container-first workflow.
>
> **ACP / Zed status:** `claw-code` does not ship an ACP/Zed daemon or JSON-RPC entrypoint yet. Run `claw acp` (or `claw --acp`) for the current status instead of guessing from source layout; `claw acp serve` is currently a discoverability alias only, returns status with exit code 0, and real ACP support remains tracked separately in `ROADMAP.md`. For the public JSON contract, see [`docs/g011-acp-json-rpc-status-contract.md`](./docs/g011-acp-json-rpc-status-contract.md).

## Current repository shape

- **`rust/`** — canonical Rust workspace and the `claw-plus` CLI binary
- **`USAGE.md`** — task-oriented usage guide for the current product surface
- **`PARITY.md`** — Rust-port parity status and migration notes
- **`ROADMAP.md`** — active roadmap and cleanup backlog
- **`PHILOSOPHY.md`** — project intent and system-design framing
- **`src/` + `tests/`** — companion Python/reference workspace; not the primary runtime surface

## Quick start

> [!WARNING]
> **`cargo install claw-code` installs the wrong thing.** The `claw-code` crate on crates.io is a deprecated stub that places `claw-code-deprecated.exe` — not `claw-plus`. Running it only prints `"claw-code has been renamed to agent-code"`. **Do not use `cargo install claw-code`.** Either build from source (this repo) or install the upstream binary:
> ```bash
> cargo install agent-code   # upstream binary — installs 'agent.exe' (Windows) / 'agent' (Unix), NOT 'agent-code'
> ```
> This repo is the actively maintained fork — follow the steps below to build from source.

```bash
# 1. Clone and build
git clone https://github.com/dong382258137/claw-code
cd claw-code/rust
cargo build --workspace

# 2. Set your API key (Anthropic API key — not a Claude subscription)
export ANTHROPIC_API_KEY="sk-ant-..."

# 3. Verify everything is wired correctly
./target/debug/claw-plus doctor

# 4. Run a prompt
./target/debug/claw-plus prompt "say hello"
```

> [!NOTE]
> **Windows (PowerShell):** the binary is `claw.exe`, not `claw-plus`. Use `.\target\debug\claw-plus.exe` or run `cargo run -- prompt "say hello"` to skip the path lookup.

### Windows setup

**PowerShell is a supported Windows path.** Use whichever shell works for you. The common onboarding issues on Windows are:

1. **Install Rust first** — download from <https://rustup.rs/> and run the installer. Close and reopen your terminal when it finishes.
2. **Verify Rust is on PATH:**
   ```powershell
   cargo --version
   ```
   If this fails, reopen your terminal or run the PATH setup from the Rust installer output, then retry.
3. **Clone and build** (works in PowerShell, Git Bash, or WSL):
   ```powershell
   git clone https://github.com/dong382258137/claw-code
   cd claw-code/rust
   cargo build --workspace
   ```
4. **Run** (PowerShell — note `.exe` and backslash):
   ```powershell
   $env:ANTHROPIC_API_KEY = "sk-ant-..."
   .\target\debug\claw-plus.exe prompt "say hello"
   ```

For release ZIPs, PATH setup, provider switching, and notification smoke checks, see [`docs/windows-install-release.md`](./docs/windows-install-release.md).

**Git Bash / WSL** are optional alternatives, not requirements. If you prefer bash-style paths (`/c/Users/you/...` instead of `C:\Users\you\...`), Git Bash (ships with Git for Windows) works well. In Git Bash, the `MINGW64` prompt is expected and normal — not a broken install.

## Post-build: locate the binary and verify

After running `cargo build --workspace`, the `claw-plus` binary is built but **not** automatically installed to your system. Here's where to find it and how to verify the build succeeded.

### Binary location

After `cargo build --workspace` in `claw-code/rust/`:

**Debug build (default, faster compile):**
- **macOS/Linux:** `rust/target/debug/claw-plus`
- **Windows:** `rust/target/debug/claw-plus.exe`

**Release build (optimized, slower compile):**
- **macOS/Linux:** `rust/target/release/claw-plus`
- **Windows:** `rust/target/release/claw-plus.exe`

If you ran `cargo build` without `--release`, the binary is in the `debug/` folder.

### Verify the build succeeded

Test the binary directly using its path:

```bash
# macOS/Linux (debug build)
./rust/target/debug/claw-plus --help
./rust/target/debug/claw-plus doctor

# Windows PowerShell (debug build)
.\rust\target\debug\claw.exe --help
.\rust\target\debug\claw.exe doctor
```

PowerShell smoke commands that do not require live credentials:

```powershell
$env:CLAW_CONFIG_HOME = Join-Path $env:TEMP "claw config home"
New-Item -ItemType Directory -Force -Path $env:CLAW_CONFIG_HOME | Out-Null
Remove-Item Env:\ANTHROPIC_API_KEY, Env:\ANTHROPIC_AUTH_TOKEN, Env:\OPENAI_API_KEY -ErrorAction SilentlyContinue
.\rust\target\debug\claw.exe help
.\rust\target\debug\claw.exe status
.\rust\target\debug\claw.exe config env
.\rust\target\debug\claw.exe doctor
```

If these commands succeed, the build is working. `claw doctor` is your first health check — it validates your API key, model access, and tool configuration.

### Optional: Add to PATH

If you want to run `claw-plus` from any directory without the full path, choose one of these approaches:

**Option 1: Symlink (macOS/Linux)**
```bash
ln -s $(pwd)/rust/target/debug/claw-plus /usr/local/bin/claw-plus
```
Then reload your shell and test:
```bash
claw --help
```

**Option 2: Use `cargo install` (all platforms)**

Build and install to Cargo's default location (`~/.cargo/bin/`, which is usually on PATH):
```bash
# From the claw-code/rust/ directory
cargo install --path . --force

# Then from anywhere
claw --help
```

**Option 3: Update shell profile (bash/zsh)**

Add this line to `~/.bashrc` or `~/.zshrc`:
```bash
export PATH="$(pwd)/rust/target/debug:$PATH"
```

Reload your shell:
```bash
source ~/.bashrc  # or source ~/.zshrc
claw --help
```

### Troubleshooting

- **"command not found: claw"** — The binary is in `rust/target/debug/claw-plus`, but it's not on your PATH. Use the full path `./rust/target/debug/claw-plus` or symlink/install as above.
- **"permission denied"** — On macOS/Linux, you may need `chmod +x rust/target/debug/claw-plus` if the executable bit isn't set (rare).
- **Debug vs. release** — If the build is slow, you're in debug mode (default). Add `--release` to `cargo build` for faster runtime, but the build itself will take 5–10 minutes.

> [!NOTE]
> **Auth:** claw requires an **API key** (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.) — Claude subscription login is not a supported auth path.

Run the workspace test suite after verifying the binary works:

```bash
cd rust
cargo test --workspace
```

## Documentation map

- [`USAGE.md`](./USAGE.md) — quick commands, auth, sessions, config, parity harness
- [`docs/navigation-file-context.md`](./docs/navigation-file-context.md) — terminal navigation, scrollback, `@path` file context, attachments, and secret-safety guidance
- [`docs/local-openai-compatible-providers.md`](./docs/local-openai-compatible-providers.md) — Ollama/llama.cpp/vLLM setup, Claw multi-provider positioning, and local skills install checks
- [`docs/windows-install-release.md`](./docs/windows-install-release.md) — PowerShell-first install, release artifact, provider switching, and Windows/WSL notification smoke paths
- [`rust/README.md`](./rust/README.md) — crate map, CLI surface, features, workspace layout
- [`PARITY.md`](./PARITY.md) — parity status for the Rust port
- [`rust/MOCK_PARITY_HARNESS.md`](./rust/MOCK_PARITY_HARNESS.md) — deterministic mock-service harness details
- [`ROADMAP.md`](./ROADMAP.md) — active roadmap and open cleanup work
- [`docs/g004-events-reports-contract.md`](./docs/g004-events-reports-contract.md) — Stream 2 lane event/report contract guidance for consumers
- [`PHILOSOPHY.md`](./PHILOSOPHY.md) — why the project exists and how it is operated
- [`CONTRIBUTING.md`](./CONTRIBUTING.md), [`SECURITY.md`](./SECURITY.md), [`SUPPORT.md`](./SUPPORT.md), and [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md) — contribution, vulnerability-reporting, support, and community policies
- [`LICENSE`](./LICENSE) — MIT license for this repository

## Ecosystem

This fork is part of a broader ecosystem of projects. The upstream base is
[ultraworkers/claw-code](https://github.com/ultraworkers/claw-code) (MIT License).
Related projects in the ecosystem (independently maintained by their respective authors):

- [clawhip](https://github.com/Yeachan-Heo/clawhip) — event and notification routing (by [Yeachan-Heo](https://github.com/Yeachan-Heo))
- [oh-my-openagent](https://github.com/code-yeongyu/oh-my-openagent) — multi-agent coordination (by [code-yeongyu](https://github.com/code-yeongyu))
- [oh-my-claudecode](https://github.com/Yeachan-Heo/oh-my-claudecode) — Claude Code workflow (by [Yeachan-Heo](https://github.com/Yeachan-Heo))
- [oh-my-codex](https://github.com/Yeachan-Heo/oh-my-codex) — workflow execution (by [Yeachan-Heo](https://github.com/Yeachan-Heo))

## Ownership / affiliation disclaimer

- 本项目基于 MIT License 的 [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code) 进行二次开发，尊重并保留上游版权声明。
- This repository does **not** claim ownership of the original Claude Code source material.
- This repository is **not affiliated with, endorsed by, or maintained by Anthropic**.
- "Claude" and "Claude Code" are trademarks of Anthropic.
- This is an independent fork of the MIT-licensed [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code) project.
- 参考项目（clawhip、oh-my-openagent 等）均为独立开源项目，各有自己的作者和许可证，本仓库引用它们不代表对其拥有所有权。
