# Claw Plus 使用说明书

> 版本：基于 d:\claw-code-src 仓库（2026-07-28 快照）
> 二进制名称：`claw-plus`（Windows 上为 `claw.exe`，Unix 上为 `claw-plus`）
> 默认模型：`claude-opus-4-6`
> 默认权限模式：`danger-full-access`

---

## 目录

1. [项目简介](#1-项目简介)
2. [安装与构建](#2-安装与构建)
3. [认证配置](#3-认证配置)
4. [CLI 命令总览](#4-cli-命令总览)
5. [交互式 REPL](#5-交互式-repl)
6. [全屏 TUI 模式](#6-全屏-tui-模式)
7. [多 Provider 支持](#7-多-provider-支持)
8. [斜杠命令（Slash Commands）](#8-斜杠命令slash-commands)
9. [会话管理](#9-会话管理)
10. [权限与安全](#10-权限与安全)
11. [多智能体 DAG 编排](#11-多智能体-dag-编排)
12. [插件与扩展生态](#12-插件与扩展生态)
13. [Hooks 钩子系统](#13-hooks-钩子系统)
14. [IM 桥接](#14-im-桥接)
15. [可观测性与诊断](#15-可观测性与诊断)
16. [配置文件层级](#16-配置文件层级)
17. [本地模型与代理](#17-本地模型与代理)
18. [HTTP 代理](#18-http-代理)
19. [容器化与 CI/CD](#19-容器化与-cicd)
20. [故障排查 FAQ](#20-故障排查-faq)

---

## 1. 项目简介

Claw Plus 是 [dong382258137/claw-code](https://github.com/dong382258137/claw-code)（MIT License）的 fork 二次开发版本，用 Rust 全量重写了 Anthropic Claude Code 的 CLI agent harness 形态。

**核心定位：** 一个 Claude-Code 形态的工作流/运行时，不是 Claude-only 产品。可通过模型前缀路由到 Anthropic、OpenAI、xAI、阿里 DashScope、OpenRouter、Ollama 等多家 Provider。

**核心特性：**

| 类别 | 能力 |
|------|------|
| 高性能 Rust 引擎 | ~20K 行 Rust、9 个 workspace crate、<50ms 冷启动、<50MB 空闲内存 |
| 多 Provider 兼容 | Anthropic / OpenAI / xAI / DashScope / OpenRouter / Ollama，模型名前缀自动路由 |
| 现代 TUI | Ratatui 全屏界面、侧边栏、状态栏、上下文窗口进度条、工具卡片、斜杠命令面板 |
| 多智能体 DAG | 声明式依赖图、3 种协调模式、最多 4 节点并行、SAGA 补偿 |
| 插件生态 | 插件管理器、Skills 系统、MCP 协议、Hooks、自定义 Agent |
| 安全防护 | 三级权限模式、细粒度规则、沙箱隔离、Broad-CWD 防护 |
| IM 桥接 | 飞书 / 企业微信远程操控 |
| 开发者体验 | 50+ 斜杠命令、Doctor 健康检查、会话持久化、Git 集成、JSON 输出 |

---

## 2. 安装与构建

### 2.1 前置依赖

- Rust toolchain（含 `cargo`）— 安装：<https://rustup.rs/>
- 至少一个 Provider 的 API 凭证

### 2.2 从源码构建

```bash
git clone https://github.com/dong382258137/claw-code
cd claw-code/rust
cargo build --workspace
```

构建产物位置：

| 平台 | Debug 路径 | Release 路径 |
|------|-----------|-------------|
| macOS/Linux | `rust/target/debug/claw-plus` | `rust/target/release/claw-plus` |
| Windows | `rust\target\debug\claw-plus.exe`（或 `claw.exe`） | `rust\target\release\claw-plus.exe` |

> **注意：** `cargo install claw-code` 会安装到 crates.io 上的弃用 stub，不要使用。如需通过 cargo 安装，使用 `cargo install --path . --force`（在 `rust/` 目录下）。

### 2.3 Windows PowerShell 安装

```powershell
# 1. 安装 Rust 后验证
cargo --version

# 2. 克隆并构建
git clone https://github.com/dong382258137/claw-code
cd claw-code/rust
cargo build --workspace

# 3. 运行（PowerShell 注意 .exe 和反斜杠）
$env:ANTHROPIC_API_KEY = "sk-ant-..."
.\target\debug\claw-plus.exe prompt "say hello"
```

### 2.4 加入 PATH（可选）

```bash
# macOS/Linux 软链接
ln -s $(pwd)/rust/target/debug/claw-plus /usr/local/bin/claw-plus

# 或在 ~/.bashrc / ~/.zshrc 中
export PATH="$(pwd)/rust/target/debug:$PATH"
```

### 2.5 启用全屏 TUI（可选 feature）

TUI 模块受 `full-tui` Cargo feature 门控，构建时需启用：

```bash
cargo build --workspace --features rusty-claude-cli/full-tui
```

---

## 3. 认证配置

### 3.1 API Key（最常用）

```bash
export ANTHROPIC_API_KEY="sk-ant-..."   # macOS/Linux
$env:ANTHROPIC_API_KEY = "sk-ant-..."   # Windows PowerShell
```

### 3.2 OAuth Bearer Token

适用于 Anthropic 兼容代理或 OAuth 流程颁发的 token：

```bash
export ANTHROPIC_AUTH_TOKEN="anthropic-oauth-or-proxy-bearer-token"
```

### 3.3 凭证选择对照表

| 凭证形态 | 环境变量 | HTTP 头 | 典型来源 |
|---------|---------|---------|---------|
| `sk-ant-*` API key | `ANTHROPIC_API_KEY` | `x-api-key: sk-ant-...` | console.anthropic.com |
| OAuth access token（不透明） | `ANTHROPIC_AUTH_TOKEN` | `Authorization: Bearer ...` | Anthropic 兼容代理 / OAuth 流程 |
| OpenRouter key（`sk-or-v1-*`） | `OPENAI_API_KEY` + `OPENAI_BASE_URL=https://openrouter.ai/api/v1` | `Authorization: Bearer ...` | openrouter.ai/keys |

> **重要：** 两种 Anthropic 凭证不可互换。把 `sk-ant-*` 放到 `ANTHROPIC_AUTH_TOKEN` 会导致 401，因为 Anthropic 拒绝通过 Bearer 头传递 `sk-ant-*`。

### 3.4 其他 Provider 凭证

```bash
export OPENAI_API_KEY="sk-..."      # OpenAI / OpenRouter / Ollama 兼容
export XAI_API_KEY="xai-..."        # xAI Grok
export DASHSCOPE_API_KEY="sk-..."   # 阿里 DashScope Qwen
```

---

## 4. CLI 命令总览

```text
claw-plus [OPTIONS] [COMMAND]

全局选项：
  --model MODEL                      指定模型（别名或完整 ID）
  --output-format text|json          输出格式（默认 text）
  --permission-mode MODE             权限模式：read-only / workspace-write / danger-full-access
  --dangerously-skip-permissions     跳过所有权限提示（CI 用）
  --allowedTools TOOLS               工具白名单（逗号分隔）
  --allowed-tools TOOLS              同上（kebab-case 别名）
  --resume [SESSION.jsonl|id|latest] 恢复会话
  --tui                              启动全屏 TUI 模式（需 full-tui feature）
  --no-tui                           强制不进 TUI
  --enable-plan-mode                 启用规划模式
  --enable-policy-engine             启用策略引擎
  --reasoning-effort low|medium|high OpenAI 推理模型强度
  --compact                          启动时压缩会话
  --cache-stats                      显示 prompt cache 统计
  --base-commit SHA                  指定基线 commit
  --add-dir PATH                     额外加入上下文的目录
  --verbose                          详细输出
  --quiet                            静默模式
  --silent                           完全静默
  --allow-broad-cwd                  允许从 $HOME 或根目录启动（默认拒绝）
  --print                            打印模式（非交互）
  --acp | -acp                       ACP/Zed 状态查询
  --version, -V                      版本信息
  --help, -h                         帮助

顶级子命令：
  prompt <text>            一次性 prompt 模式
  help                     帮助
  version                  版本
  status                   会话状态
  sandbox                  沙箱状态
  state                    读取 .claw/worker-state.json
  acp [serve]              ACP 状态查询（serve 仅为发现性别名）
  dump-manifests           导出工具/prompt 清单
  bootstrap-plan           启动规划
  agents                   列出已配置 agents
  mcp                      MCP 服务器清单
  skills                   Skills 清单
  system-prompt            显示当前 system prompt
  init                     初始化仓库（生成 .claw / CLAUDE.md）
  doctor                   健康检查

无子命令时进入交互式 REPL。
```

### 4.1 常用调用方式

```bash
# 交互式 REPL
claw-plus

# 一次性 prompt
claw-plus prompt "summarize this repository"

# 简写（直接传字符串）
claw-plus "explain rust/crates/runtime/src/lib.rs"

# JSON 输出（脚本集成）
claw-plus --output-format json prompt "status"

# 指定模型 + 权限
claw-plus --model sonnet --permission-mode workspace-write prompt "review this diff"

# 工具白名单
claw-plus --allowedTools read,glob "inspect the runtime crate"

# 恢复最近会话
claw-plus --resume latest
claw-plus --resume latest /status /diff
```

### 4.2 文件上下文（@path 语法）

在 prompt 中用 `@path/to/file` 引用仓库文件作为上下文：

```text
Read @src/app.ts and explain the bug
Compare @old.md and @new.md
Use @logs/error.txt as context and suggest a fix
```

---

## 5. 交互式 REPL

启动方式：直接运行 `claw-plus`（不带子命令）。

### 5.1 REPL 核心能力

- 基于 `rustyline` 的行编辑器，支持历史、`Ctrl-r` 反向搜索
- Tab 补全：斜杠命令名、模型别名、权限模式、会话 ID
- Shift+Enter 或 Ctrl+J 输入多行
- Markdown 终端渲染（标题、列表、表格、代码块带 syntect 高亮、引用块）
- 流式输出（token 逐字渲染）
- 工具调用卡片化展示（`╭─ tool_name ─╮` 边框 + ✓/✗ 状态图标）
- 交互式权限提示（Y/N）

### 5.2 REPL 内置高级命令

| 命令 | 用途 |
|------|------|
| `/ultraplan <task>` | 深度规划，多步推理分解复杂任务，输出编号步骤 + 推理 + 预期结果 |
| `/teleport <symbol-or-path>` | 跳转到文件 / 函数 / 类 / struct，高亮目标符号 |
| `/bughunter [scope]` | 扫描常见 bug、反模式、潜在问题，输出文件/行号/修复建议 |

### 5.3 Worker 状态文件

REPL 或 `claw prompt` 执行后会写入 `.claw/worker-state.json`，可用 `claw state` 读取：

```bash
claw-plus state
claw-plus state --output-format json
```

---

## 6. 全屏 TUI 模式

需 `full-tui` feature 编译，启动方式：`claw-plus --tui`。

### 6.1 布局组件

| 组件 | 作用 |
|------|------|
| **OutputView** | 主输出区，结构化存储 Text / ToolCard / Thinking / Timeline 条目，支持上下滚动（Up/Down 行、PgUp/PgDn 页），自动跟随底部 |
| **InputLine** | 底部输入行，支持多行（Shift+Enter / Ctrl+J）、CJK 宽度计算、bracketed paste |
| **SlashMenu** | 输入 `/` 触发的模糊搜索面板，按 name/aliases/summary 匹配，Up/Down 选择，Tab/Enter 填入输入框（需二次 Enter 提交） |
| **StatusBar** | 底部状态栏，显示 cwd、版本、模型、流式计时器、Token/费用、上下文窗口利用率 |
| **Sidebar** | 侧边栏，实时显示已加载 Skills / Agents / MCP 服务器、ToolHistory、SkillHistory |
| **ToolCard** | 工具调用卡片，超过 5 行自动折叠为摘要，Ctrl+T 或鼠标左键展开/折叠 |

### 6.2 关键快捷键

| 按键 | 行为 |
|------|------|
| `Enter` | 提交输入（runtime 忙时回填到 buffer 防丢失） |
| `Shift+Enter` / `Ctrl+J` | 多行换行 |
| `Ctrl+V` | 粘贴剪贴板（conhost 终端会触发剪贴板读取） |
| `Up` / `Down` | SlashMenu 选项导航 / OutputView 行滚动 |
| `PgUp` / `PgDn` | OutputView 页滚动 |
| `Tab` / `Enter`（在菜单中） | 填入选中项到输入框 |
| `Ctrl+T` | 折叠/展开当前 ToolCard |
| `?` | 帮助 overlay（阻塞其他输入） |
| `Esc` / `Ctrl+C` / `Ctrl+D` | 退出帮助 overlay / 退出 TUI |

### 6.3 TUI 边界防护

- 所有 stdout/stderr 输出在 TUI 模式下被门控，防止污染 alternate screen
- `StderrGuard` 将 stderr 重定向到匿名 pipe，退出时自动恢复
- `TuiSilentPermissionPrompter` 避免与 crossterm event loop 冲突
- 终端状态（raw mode、alternate screen、mouse capture）通过 Drop guard 恢复，panic 也保证还原
- Windows crossterm KeyEventKind 只处理 Press/Repeat，忽略 Release 防止重复输入

---

## 7. 多 Provider 支持

### 7.1 Provider 矩阵

| Provider | 协议 | 认证环境变量 | Base URL 环境变量 | 默认 Base URL |
|----------|------|-------------|------------------|--------------|
| **Anthropic**（直连） | Anthropic Messages API | `ANTHROPIC_API_KEY` 或 `ANTHROPIC_AUTH_TOKEN` | `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` |
| **xAI** | OpenAI 兼容 | `XAI_API_KEY` | `XAI_BASE_URL` | `https://api.x.ai/v1` |
| **OpenAI 兼容** | OpenAI Chat Completions | `OPENAI_API_KEY` | `OPENAI_BASE_URL` | `https://api.openai.com/v1` |
| **DashScope**（阿里） | OpenAI 兼容 | `DASHSCOPE_API_KEY` | `DASHSCOPE_BASE_URL` | `https://dashscope.aliyuncs.com/compatible-mode/v1` |

OpenAI 兼容后端同时作为 OpenRouter、Ollama、任何说 OpenAI `/v1/chat/completions` 协议的服务的网关。

### 7.2 模型名前缀路由

模型名前缀决定 Provider，优先于环境凭证：

| 前缀 | 路由到 |
|------|--------|
| `claude`（或无前缀） | Anthropic |
| `grok` | xAI |
| `openai/` 或 `gpt-` | OpenAI 兼容 |
| `qwen/` 或 `qwen-` | DashScope |
| `kimi/` 或 `kimi-` | DashScope |

`openai/` 在默认 OpenAI API 上是路由前缀（请求前剥离）；自定义 `OPENAI_BASE_URL` 时斜杠模型 ID（如 OpenRouter 的 `openai/gpt-4.1-mini`）会保留原样发给网关。

### 7.3 内置模型别名

| 别名 | 解析为 | Provider | 最大输出 token | 上下文窗口 |
|------|--------|----------|---------------|-----------|
| `opus` | `claude-opus-4-6` | Anthropic | 32 000 | 200 000 |
| `sonnet` | `claude-sonnet-4-6` | Anthropic | 64 000 | 200 000 |
| `haiku` | `claude-haiku-4-5-20251213` | Anthropic | 64 000 | 200 000 |
| `grok` / `grok-3` | `grok-3` | xAI | 64 000 | 131 072 |
| `grok-mini` / `grok-3-mini` | `grok-3-mini` | xAI | 64 000 | 131 072 |
| `grok-2` | `grok-2` | xAI | — | — |
| `kimi` | `kimi-k2.5` | DashScope | 16 384 | 256 000 |
| `gpt-4.1` / `gpt-4.1-mini` / `gpt-4.1-nano` | 同名 | OpenAI 兼容 | 32 768 | 1 047 576 |
| `gpt-5.4` / `gpt-5.4-mini` / `gpt-5.4-nano` | 同名 | OpenAI 兼容 | 128 000 | 1 000 000 / 400 000 |

未匹配别名的模型名在 Provider 路由后原样传递。这是使用 OpenRouter slugs、Ollama tags、完整 Anthropic model ID 的方式。

### 7.4 自定义别名

在任意 settings 文件中（`~/.claw/settings.json`、`.claw/settings.json`、`.claw/settings.local.json`）：

```json
{
  "aliases": {
    "fast": "claude-haiku-4-5-20251213",
    "smart": "claude-opus-4-6",
    "cheap": "grok-3-mini"
  }
}
```

别名可链式解析，`"fast": "haiku"` 也可工作。本地项目 settings 覆盖用户级 settings。

### 7.5 推理强度控制

```bash
claw-plus --reasoning-effort low prompt "quick review"
claw-plus --reasoning-effort high prompt "deep analysis"
```

仅当用户显式启用 reasoning 时才会把 `effective_effort` 设为 `low`，避免误激活 DeepSeek thinking 模式。推理变体模型（`qwen-qwq-*`、`qwq-*`、`*-thinking`）会自动剥离 `temperature` / `top_p` / `frequency_penalty` / `presence_penalty`。

### 7.6 extra_body 透传

`MessageRequest::extra_body` 可透传 Provider 特定 JSON 参数（如 `web_search_options`、`parallel_tool_calls`）。核心协议字段（`model`、`messages`、`stream`、`tools`、`tool_choice`、`max_tokens`、`max_completion_tokens`）受保护，不可被 extra_body 覆盖。

---

## 8. 斜杠命令（Slash Commands）

REPL 内输入 `/` 触发，Tab 可补全。命令分类如下（共 100+ 命令）。

### 8.1 会话与可见性

| 命令 | 说明 |
|------|------|
| `/help` | 显示可用斜杠命令 |
| `/status` | 当前会话状态 |
| `/sandbox` | 沙箱隔离状态 |
| `/cost` | 累计 Token 使用与费用 |
| `/usage` | 详细 API 使用统计 |
| `/stats` | 工作区与会话统计 |
| `/tokens` | 当前会话 Token 计数 |
| `/cache` | Prompt cache 统计 |
| `/version` | CLI 版本与构建信息 |
| `/resume <session>` | 加载已保存会话 |
| `/session [list\|pick\|exists\|switch\|fork\|delete]` | 会话 CRUD |
| `/rename <name>` | 重命名当前会话 |
| `/export [file]` | 导出会话到文件 |
| `/clear [--confirm]` | 开启全新本地会话 |
| `/compact` | 压缩本地会话历史 |
| `/search <query>` | 按关键字搜索会话历史 |
| `/rewind [steps]` | 回退到之前状态 |
| `/summary` | 生成会话摘要 |
| `/tag [label]` | 标记当前会话点 |
| `/bookmarks [add\|remove\|list]` | 书签管理 |
| `/pin [msg-idx]` / `/unpin [msg-idx]` | 固定消息防压缩 |
| `/history [count]` | 会话历史摘要 |
| `/exit` | 退出 REPL |

### 8.2 工作区与 Git

| 命令 | 说明 |
|------|------|
| `/diff` | 当前工作区 git diff |
| `/commit` | 生成 commit message 并提交 |
| `/pr [context]` | 起草或创建 PR |
| `/issue [context]` | 起草或创建 GitHub issue |
| `/branch [name]` | 创建或切换 git 分支 |
| `/git <subcommand>` | 运行 git 子命令 |
| `/stash [pop\|list\|apply]` | stash 管理 |
| `/blame <file> [line]` | git blame |
| `/log [count]` | git log |
| `/files` | 列出上下文窗口中的文件 |
| `/context [show\|clear]` | 检查或管理上下文 |
| `/add-dir <path>` | 额外加入上下文的目录 |
| `/workspace [path]`（别名 `/cwd`） | 显示或切换工作目录 |
| `/focus <path...>` / `/unfocus [path...]` | 聚焦/取消聚焦文件 |
| `/map [depth]` | 代码库结构可视化 |
| `/init` | 生成 CLAUDE.md starter |
| `/init-force` | 覆盖 CLAUDE.md |
| `/release-notes` | 从近期变更生成 release notes |
| `/changelog [count]` | 显示近期代码库变更 |

### 8.3 模型与权限

| 命令 | 说明 |
|------|------|
| `/model [model]` | 显示或切换活动模型 |
| `/permissions [mode]` | 显示或切换权限模式 |
| `/providers` | 列出可用 Provider |
| `/allowed-tools [add\|remove\|list] [tool]` | 工具白名单管理 |
| `/api-key [key]` | 显示或设置 API key |
| `/max-tokens [count]` | 显示或设置最大输出 token |
| `/temperature [value]` | 显示或设置采样温度 |
| `/effort [low\|medium\|high]` | 响应 effort 级别 |
| `/reasoning` | 推理设置 |
| `/system-prompt` | 显示活动 system prompt |

### 8.4 发现与诊断

| 命令 | 说明 |
|------|------|
| `/doctor` | 诊断 setup 问题与环境健康 |
| `/mcp [list\|show <server>\|help]` | MCP 服务器清单 |
| `/agents [list\|help]` | 已配置 agents 列表 |
| `/skills [list\|install <path>\|help\|<skill> [args]]` | Skills 管理 |
| `/plugin [list\|install\|enable\|disable\|uninstall\|update]`（别名 `/plugins`、`/marketplace`） | 插件管理 |
| `/tasks [list\|get <id>\|stop <id>]` | 后台任务管理 |
| `/env` | 工具可见的环境变量 |
| `/project` | 项目检测信息 |
| `/context [show\|clear]` | 上下文检查 |
| `/files` | 上下文窗口文件列表 |
| `/keybindings` | 快捷键显示/配置 |
| `/privacy-settings` | 隐私设置 |

### 8.5 自动化与分析

| 命令 | 说明 |
|------|------|
| `/ultraplan [task]` | 深度规划（多步推理） |
| `/teleport <symbol-or-path>` | 跳转到文件/符号 |
| `/bughunter [scope]` | 扫描潜在 bug |
| `/review [scope]` | 代码审查 |
| `/security-review [scope]` | 安全审查 |
| `/advisor` | 切换 advisor 模式（仅给指导） |
| `/insights` | AI 生成的会话洞察 |
| `/thinkback` | 重放上一次响应的思考过程 |
| `/subagent [list\|steer <target> <msg>\|kill <id>]` | 子 agent 控制 |
| `/agent [list\|spawn\|kill]` | sub-agent 与 spawned 会话管理 |
| `/team [list\|create\|delete]` | agent 团队管理 |
| `/cron [list\|add\|remove]` | 定时任务管理 |
| `/parallel <count> <prompt>` | 在并行 subagent 中运行命令 |
| `/multi <commands>` | 顺序执行多个斜杠命令 |
| `/macro [record\|stop\|play <name>]` | 宏录制/回放 |
| `/alias <name> <command>` | 创建命令别名 |
| `/telemetry [on\|off\|status]` | 遥测设置 |
| `/notifications [on\|off\|status]` | 通知设置 |
| `/benchmark [suite]` | 性能基准 |

### 8.6 输入输出与 UI

| 命令 | 说明 |
|------|------|
| `/theme [name]` | 切换终端配色主题 |
| `/color [scheme]` | 终端颜色设置 |
| `/output-style [style]` | 输出格式风格切换 |
| `/vim` | 切换 vim 键绑定 |
| `/voice [on\|off]` | 语音输入模式 |
| `/listen` | 监听语音输入 |
| `/speak` | 朗读上一次响应 |
| `/language [lang]` | 界面语言 |
| `/brief` | 简短输出模式 |
| `/fast` | 快速/简洁响应模式 |
| `/poor [on\|off\|status]` | poor 模式（跳过非必要 token） |
| `/format [markdown\|plain\|json]` | 重新格式化上一次响应 |
| `/copy [last\|all]` | 复制到剪贴板 |
| `/share` | 分享当前会话 |
| `/paste` | 粘贴剪贴板内容 |
| `/screenshot` | 截屏并加入会话 |
| `/image <path>` | 加入图片文件 |
| `/terminal-setup` | 终端集成设置 |
| `/ide [vscode\|cursor]` | IDE 集成 |
| `/desktop` | 桌面应用集成 |
| `/stickers` | 贴纸包 |
| `/feedback` | 提交反馈 |
| `/upgrade` | 检查并安装 CLI 更新 |

### 8.7 开发工作流

| 命令 | 说明 |
|------|------|
| `/test [filter]` | 运行项目测试 |
| `/lint [filter]` | 运行 lint |
| `/build [target]` | 构建项目 |
| `/run <command>` | 项目上下文中运行命令 |
| `/explain <path> [line-range]` | 解释文件/代码 |
| `/refactor <path> [scope]` | 重构建议 |
| `/docs [path]` | 生成或显示文档 |
| `/fix [path]` | 修复错误 |
| `/perf <path>` | 性能分析 |
| `/symbols <path>` | 列出文件符号 |
| `/references <symbol>` | 查找所有引用 |
| `/definition <symbol>` | 跳转到定义 |
| `/hover <symbol>` | hover 信息 |
| `/diagnostics [path]` | LSP 诊断 |
| `/autofix [path]` | 自动修复可修复诊断 |
| `/chat` | 自由聊天模式 |
| `/web <url>` | 抓取并总结网页 |

### 8.8 审批与控制

| 命令 | 说明 |
|------|------|
| `/approve`（别名 `/yes`、`/y`） | 批准挂起的工具执行 |
| `/deny`（别名 `/no`、`/n`） | 拒绝挂起的工具执行 |
| `/stop` | 停止当前生成 |
| `/retry` | 重试上一次失败的消息 |
| `/undo` | 撤销上一次文件写入/编辑 |
| `/goal [set <text>\|clear\|pause\|resume\|status]` | 持久目标驱动管理 |
| `/bg [ps\|logs <pid>\|kill <pid>\|purge <pid>\|spawn <prompt>]` | 后台 claw 会话管理 |
| `/im [status\|config\|start]` | IM Bridge 管理 |
| `/plan [on\|off]` | 规划模式切换 |
| `/profile [name]` | 用户 profile 切换 |
| `/migrate` | 运行待处理数据迁移 |
| `/reset [section]` | 重置配置到默认 |
| `/templates [list\|apply <name>]` | prompt 模板 |

### 8.9 高级特殊命令

| 命令 | 说明 |
|------|------|
| `/debug-tool-call` | 重放上一次工具调用（带调试详情） |
| `/hooks [list\|run <hook>]` | 生命周期 hook 管理与运行 |
| `/tool-details <tool-name>` | 工具详细信息 |
| `/im [status\|config\|start]` | IM Bridge 控制 |

---

## 9. 会话管理

### 9.1 持久化位置

REPL turns 持久化在当前工作区的 `.claw/sessions/` 目录，JSONL 格式。

### 9.2 恢复会话

```bash
# 恢复最近会话
claw-plus --resume latest

# 恢复后直接执行命令
claw-plus --resume latest /status /diff

# 恢复指定会话
claw-plus --resume <session-id-or-path>
```

### 9.3 REPL 内会话操作

```text
/session list              列出所有会话
/session pick              交互式选择
/session exists <id>       检查会话是否存在
/session switch <id>       切换会话
/session fork [branch]     分叉会话
/session delete <id> [--force]   删除会话
/export [file]             导出当前会话
/rewind [steps]            回退到之前状态
```

### 9.4 上下文压缩

长会话自动检测并智能压缩（`/compact`），保留关键信息的同时控制 token 消耗。压缩触发前会调用 `PreCompact` hook。

---

## 10. 权限与安全

### 10.1 三级权限模式

| 模式 | 行为 |
|------|------|
| `read-only` | 仅读取文件和搜索（最安全） |
| `workspace-write` | 允许工作区内写入和编辑 |
| `danger-full-access` | 完全访问（**默认**，需显式 opt-in 切换） |

```bash
claw-plus --permission-mode read-only prompt "summarize Cargo.toml"
claw-plus --permission-mode workspace-write prompt "update README.md"
```

REPL 内：`/permissions [mode]`。

### 10.2 细粒度规则

在 settings 中配置 `permissions.allow` / `permissions.deny` / `permissions.ask`，按工具名 + 匹配模式控制。

### 10.3 工具白名单

```bash
claw-plus --allowedTools read,glob,grep "inspect only"
```

### 10.4 跳过权限提示（CI 用）

```bash
claw-plus --dangerously-skip-permissions prompt "automated task"
```

### 10.5 沙箱

- Linux namespace 隔离（`unshare`）
- macOS 文件系统提示
- `/sandbox` 命令查看沙箱状态

### 10.6 Broad-CWD 防护

默认拒绝从 `$HOME` 或根目录启动（防止意外全局扫描），需 `--allow-broad-cwd` 显式放行。

### 10.7 MCP 首次使用确认

首次使用 MCP 服务器工具时弹出交互式权限提示（Confirm-on-first-use）。

---

## 11. 多智能体 DAG 编排

位于 `rust/crates/runtime/src/multi_agent/`。

### 11.1 三种协调模式

| 模式 | 隔离 | 适用场景 |
|------|------|---------|
| `Fork` | 共享工作目录 | 只读并行探索 |
| `Teammate` | 共享工作目录 + 任务注册表 | 协作 |
| `Worktree` | 独立 Git worktree（**安全默认**） | 并发写操作 |

### 11.2 DAG 依赖图

基于 `petgraph` 的有向无环图，声明节点间依赖，自动计算并行度。SCC 算法检测环。

### 11.3 异步子智能体分发

`dispatch_subagent` + `check_subagent`，支持轮询和结果收集。

Worker 全生命周期：`WorkerCreate → WorkerObserve → WorkerResolveTrust → WorkerAwaitReady → WorkerSendPrompt → WorkerObserveCompletion`

### 11.4 并行度

最多 4 节点并行执行（`DEFAULT_MAX_PARALLELISM`），DAG 运行时自动识别无依赖节点并行调度。

### 11.5 SAGA 补偿

节点失败时自动执行补偿操作，保证分布式一致性。

### 11.6 团队编排

`TeamCreate` 将多个任务组合为命名团队，统一监控。`/team [list|create|delete]` 命令管理。

### 11.7 验证门禁

`ValidationGate` trait + `CommandValidationGate` + `LlmJudgeGate`（预留）。子 agent 完成后校验。`rust_compile_gate` 内置 Rust 编译验证。

### 11.8 任务复杂度匹配

`TaskComplexity`：`Simple` / `Diagnostic` / `Architectural`，coordinator 据此匹配模型能力层级。

### 11.9 缓存保护

每个子 agent 走独立 LLM 请求 + 独立 prompt cache，不污染主 agent 缓存。"Subagent as Tool" 模式 — 主 agent 通过 tool call 接口调用子 agent。

### 11.10 相关斜杠命令

```text
/subagent [list|steer <target> <msg>|kill <id>]
/agent [list|spawn|kill]
/team [list|create|delete]
/parallel <count> <prompt>
```

---

## 12. 插件与扩展生态

### 12.1 插件管理器

```bash
claw-plus plugins list
claw-plus plugins install <path>
claw-plus plugins enable <name>
claw-plus plugins disable <name>
claw-plus plugins update <id>
claw-plus plugins uninstall <id>
```

REPL 内：`/plugin`（别名 `/plugins`、`/marketplace`）。所有命令支持 `--output-format json`。

### 12.2 Skills 系统

可发现、加载、调用的可复用技能模块。

```text
/skills list
/skills install /absolute/path/to/my-skill
/skills my-skill [args]
/skills help
```

直接 CLI：

```bash
claw-plus skills --output-format json
```

### 12.3 MCP 服务器

完整的 Model Context Protocol 生命周期：config → spawn → initialize → tool discovery → invoke → cleanup。

支持的传输：stdio / http / sse / ws。

```text
/mcp list
/mcp show <server>
/mcp help
```

### 12.4 自定义 Agent

支持 TOML 和 Markdown（YAML frontmatter）格式的 Agent 定义，可自定义 model、tools、reasoning_effort。

```text
/agents list
/agents help
```

### 12.5 内置工具

| 工具 | 用途 |
|------|------|
| `bash` | Bash/PowerShell 命令执行 |
| `read_file` | 读取文件 |
| `write_file` | 写入文件 |
| `edit_file` | 编辑文件（含 diff） |
| `replace_lines` | 替换指定行 |
| `glob_search` | Glob 模式文件查找（支持 `**/*.{rs,toml,md}` 花括号展开） |
| `grep_search` | 内容搜索 |
| `dag_run` | 启动 DAG 运行 |
| `dag_status` | 查询 DAG 状态 |
| Web 工具 | `WebSearch` / `WebFetch` |
| `Agent` / `TodoWrite` / `NotebookEdit` / `Skill` / `ToolSearch` | 工作流工具 |

---

## 13. Hooks 钩子系统

### 13.1 支持的事件

| 事件 | 触发时机 |
|------|---------|
| `PreToolUse` | 工具执行前 |
| `PostToolUse` | 工具执行后 |
| `PostToolUseFailure` | 工具执行失败后 |
| `UserPromptSubmit` | 用户提交 prompt |
| `Notification` | 通知发送 |
| `SessionStart` | 会话启动 |
| `SessionEnd` | 会话结束 |
| `Stop` | agent 停止 |
| `SubagentStop` | 子 agent 停止 |
| `PreCompact` | 压缩前 |
| `PostCustomToolCall` | 自定义工具调用后 |

### 13.2 Handler 类型

4 种 Handler 类型，支持命令行 / 脚本 / 内联等。

### 13.3 配置

通过 settings 中的 `hooks` 段配置，支持热重载（notify crate 文件 watcher）。

### 13.4 决策契约

Hook 输出 JSON 可包含 `decision` / `permissionDecision` / `updatedInput`。exit code 0 = 继续，2 = 短路，其他 = 失败。

### 13.5 命令

```text
/hooks list
/hooks run <hook>
```

详见 [`docs/modules/hooks-system-detail.md`](./modules/hooks-system-detail.md)。

---

## 14. IM 桥接

位于 `rust/crates/im-bridge/`。通过飞书 / 企业微信远程操控 claw。

### 14.1 架构

```text
IM Platform → axum HTTP server → SessionManager → ClawAgent → IM Platform
```

### 14.2 配置

配置文件：`~/.claw/im-bridge.toml`

```toml
listen_addr = "127.0.0.1:3456"
session_timeout_secs = 1800  # 30 分钟

# 至少配置一个平台
[feishu]
app_id = "cli_..."
app_secret = "..."
verification_token = "..."   # 可选
encrypt_key = "..."          # 可选

[wecom]
corp_id = "..."
secret = "..."
token = "..."
encoding_aes_key = "..."     # 43 字符
webhook_url = "https://..."  # 可选，优先于 API
agent_id = 1000002           # 可选
```

### 14.3 启动

```bash
# 直接运行二进制
claw-im-bridge

# 或 REPL 内
/im start

# 状态查询
/im status
/im config
```

### 14.4 IM 端聊天命令

在 IM 频道中发送：

| 命令 | 说明 |
|------|------|
| `/help`（或 `/h`） | 显示帮助 |
| `/new`（或 `/reset`、`/clear`） | 开启新会话 |
| `/status`（或 `/info`） | 当前会话信息 |
| `/history`（或 `/hist`） | 历史消息（占位） |

非命令消息直接发给 claw AI。

### 14.5 特性

- 一个 IM 频道对应一个 agent 会话，自动复用
- 会话持久化，重启可恢复
- Agent 失败时优雅降级
- 异步工作流：发指令后可离线，完成后通知

---

## 15. 可观测性与诊断

### 15.1 Doctor 健康检查

```bash
claw-plus doctor
claw-plus doctor --output-format json
claw-plus doctor --cache-stats
```

检查项：API 密钥、配置文件、工作区状态、沙箱状态、安装来源、Git 状态、Lane 事件、G004 conformance、branch lock、Policy engine、Green contract、Team/Cron registry 等。

### 15.2 Lane 事件模式

23 种类型化事件：`Started` / `Blocked` / `Failed` / `Finished` / `CommitCreated` / `PROpened` / `MergeReady` 等。与 [clawhip](https://github.com/Yeachan-Heo/clawhip) 事件路由系统深度集成。

### 15.3 结构化错误

错误带 `kind` 判别器（`missing_credentials`、`session_not_found`、`cli_parse`、`api_http_error` 等），无需正则匹配。

### 15.4 费用追踪

实时累计 Token 消耗和 API 费用，支持会话级和全局级统计。`/cost`、`/usage`、`/stats`、`/tokens` 命令查看。

### 15.5 Session trace / Telemetry

Turn 级别事件追踪，可接入外部遥测后端。`/telemetry [on|off|status]` 控制。

---

## 16. 配置文件层级

加载顺序（后者覆盖前者）：

1. `~/.claw.json`
2. `~/.config/claw/settings.json`
3. `<repo>/.claw.json`
4. `<repo>/.claw/settings.json`
5. `<repo>/.claw/settings.local.json`

### 16.1 关键配置示例

```json
{
  "model": "claude-sonnet-4-6",
  "permissions": {
    "allow": ["read_file", "glob_search"],
    "deny": ["bash:rm -rf"],
    "ask": ["write_file"]
  },
  "aliases": {
    "fast": "haiku",
    "smart": "opus"
  },
  "hooks": {
    "PreToolUse": [
      { "command": "echo 'about to run tool'", "type": "command" }
    ]
  }
}
```

### 16.2 命令查看配置

```text
/config                 检查 Claude 配置文件或合并段落
/config env             环境变量
/config hooks           hooks 配置
/config model           model 配置
/config plugins         plugins 配置
```

---

## 17. 本地模型与代理

### 17.1 Anthropic 兼容端点

```bash
export ANTHROPIC_BASE_URL="http://127.0.0.1:8080"
export ANTHROPIC_AUTH_TOKEN="local-dev-token"
claw-plus --model "claude-sonnet-4-6" prompt "ready"
```

### 17.2 OpenAI 兼容端点

```bash
export OPENAI_BASE_URL="http://127.0.0.1:8000/v1"
export OPENAI_API_KEY="local-dev-token"
claw-plus --model "qwen2.5-coder" prompt "ready"
```

### 17.3 Ollama

```bash
export OPENAI_BASE_URL="http://127.0.0.1:11434/v1"
unset OPENAI_API_KEY
claw-plus --model "llama3.2" prompt "summarize this repository"
```

### 17.4 OpenRouter

```bash
export OPENAI_BASE_URL="https://openrouter.ai/api/v1"
export OPENAI_API_KEY="sk-or-v1-..."
claw-plus --model "openai/gpt-4.1-mini" prompt "summarize"
```

### 17.5 阿里 DashScope（Qwen）

```bash
export DASHSCOPE_API_KEY="sk-..."
claw-plus --model "qwen/qwen-max" prompt "hello"
# 或裸名
claw-plus --model "qwen-plus" prompt "hello"
```

`qwen/` 或 `qwen-` 前缀自动路由到 DashScope 兼容端点，无需设置 `OPENAI_BASE_URL`，也无需 unset `ANTHROPIC_API_KEY`。

### 17.6 Windows PowerShell 切换 Provider

```powershell
# Anthropic 直连
$env:ANTHROPIC_API_KEY = "sk-ant-REPLACE_ME"
Remove-Item Env:\OPENAI_BASE_URL -ErrorAction SilentlyContinue
.\target\debug\claw-plus.exe --model "sonnet" prompt "ready"

# OpenRouter
Remove-Item Env:\ANTHROPIC_API_KEY -ErrorAction SilentlyContinue
$env:OPENAI_BASE_URL = "https://openrouter.ai/api/v1"
$env:OPENAI_API_KEY = "sk-or-v1-REPLACE_ME"
.\target\debug\claw-plus.exe --model "openai/gpt-4.1-mini" prompt "ready"

# 本地 Ollama
$env:OPENAI_BASE_URL = "http://127.0.0.1:11434/v1"
Remove-Item Env:\OPENAI_API_KEY -ErrorAction SilentlyContinue
.\target\debug\claw-plus.exe --model "llama3.2" prompt "ready"
```

---

## 18. HTTP 代理

### 18.1 环境变量

```bash
export HTTPS_PROXY="http://proxy.corp.example:3128"
export HTTP_PROXY="http://proxy.corp.example:3128"
export NO_PROXY="localhost,127.0.0.1,.corp.example"

claw-plus prompt "hello via the corporate proxy"
```

大小写均接受。空值视为未设置。

### 18.2 程序化 proxy_url

`ProxyConfig.proxy_url` 作为统一代理（同时覆盖 HTTP 和 HTTPS）。设置后优先于 per-scheme 字段。

### 18.3 注意事项

- `HTTPS_PROXY` 用于 `https://` URL，`HTTP_PROXY` 用于 `http://` URL
- `NO_PROXY` 接受逗号分隔的主机后缀（如 `.corp.example`）和 IP 字面量
- 代理 URL 解析失败时回退到直连客户端

---

## 19. 容器化与 CI/CD

### 19.1 容器优先

提供 `Containerfile` + [`docs/container.md`](./container.md) 完整文档。

### 19.2 Mock 服务

确定性 Anthropic 兼容 mock 服务，用于 CI 端到端测试：

```bash
cd rust
./scripts/run_mock_parity_harness.sh

# 手动启动
cargo run -p mock-anthropic-service -- --bind 127.0.0.1:0
```

覆盖场景：`streaming_text`、`read_file_roundtrip`、`grep_chunk_assembly`、`write_file_allowed`、`write_file_denied`、`multi_tool_turn_roundtrip`、`bash_stdout_roundtrip`、`bash_permission_prompt_approved`、`bash_permission_prompt_denied`、`plugin_tool_roundtrip`。

### 19.3 GitHub Actions CI

`fmt --check` + `clippy --workspace -- -D warnings` + `cargo test --workspace`。

### 19.4 工作区测试

```bash
cd rust
cargo test --workspace
```

### 19.5 机器可读 JSON 输出

所有 CLI 命令支持 `--output-format json`，便于 CI 脚本解析。诊断动词（`doctor`、`status`、`sandbox`、`version`）的 `--json` 后缀已被拒绝，统一用 `--output-format json`。

---

## 20. 故障排查 FAQ

### 20.1 `command not found: claw`

二进制在 `rust/target/debug/claw-plus`，未加入 PATH。用全路径或按 §2.4 加入 PATH。

### 20.2 401 Invalid bearer token

把 `sk-ant-*` 错放到 `ANTHROPIC_AUTH_TOKEN`。移到 `ANTHROPIC_API_KEY`。

### 20.3 Provider 误路由

环境中有多个 Provider 凭证时，用模型名前缀明确路由：`--model openai/gpt-4.1-mini`、`--model grok`、`--model qwen-plus`。

### 20.4 `cargo install claw-code` 安装了错误的东西

crates.io 上的 `claw-code` 是弃用 stub，会安装 `claw-code-deprecated.exe`。从源码构建或用 `cargo install agent-code`（上游二进制）。

### 20.5 TUI 模式屏幕错乱

确保所有 stdout/stderr 输出被门控。第三方库日志通过 `StderrGuard` 重定向。退出时 Drop guard 自动恢复终端状态。

### 20.6 Windows crossterm 重复输入

KeyEventKind 过滤：只处理 Press/Repeat 事件，忽略 Release。

### 20.7 CJK 字符显示错位

TUI 使用 `unicode-width` 计算光标位置，按显示宽度 wrap。

### 20.8 多行粘贴失败

TUI 启用 bracketed paste（DECSET 2004）。conhost 终端不支持时，Ctrl+V 触发剪贴板读取兜底。

### 20.9 `claw state` 报错 "no worker state file"

需先在仓库内运行 `claw-plus`（REPL）或 `claw prompt <text>` 至少一次，才会生成 `.claw/worker-state.json`。

### 20.10 ACP / Zed 支持

`claw acp`（或 `claw --acp`）查询当前状态。`claw acp serve` 仅为发现性别名，返回状态并 exit 0。真正的 ACP/Zed daemon 尚未实现，详见 `ROADMAP.md`。JSON 契约见 [`docs/g011-acp-json-rpc-status-contract.md`](./g011-acp-json-rpc-status-contract.md)。

---

## 附录 A：Crate 责任分工

| Crate | 职责 |
|-------|------|
| `api` | Provider 客户端、SSE 流式、请求/响应类型、认证、context window 预检 |
| `commands` | 斜杠命令定义、解析、help 文本、JSON/text 渲染 |
| `compat-harness` | 从上游 TS 源码提取工具/prompt 清单 |
| `mock-anthropic-service` | 确定性 `/v1/messages` mock，用于 CLI parity 测试 |
| `plugins` | 插件元数据、安装/启用/禁用/更新流、hook 集成 |
| `runtime` | `ConversationRuntime`、配置加载、会话持久化、权限策略、MCP 生命周期、system prompt 装配、usage tracking、multi-agent DAG |
| `rusty-claude-cli` | REPL、一次性 prompt、直接 CLI 子命令、流式显示、工具调用渲染、CLI 参数解析、TUI 模块 |
| `telemetry` | session trace 事件与遥测 payload |
| `tools` | 工具规格 + 执行：Bash、ReadFile、WriteFile、EditFile、GlobSearch、GrepSearch、WebSearch、WebFetch、Agent、TodoWrite、NotebookEdit、Skill、ToolSearch |
| `im-bridge` | 飞书 / 企业微信 IM 桥接 |
| `claw-acp` | ACP 协议相关 |
| `claw-shell` | Shell spawn 与 stdio |

## 附录 B：相关文档索引

- [USAGE.md](../USAGE.md) — 任务导向使用指南
- [rust/README.md](../rust/README.md) — Rust workspace 详情
- [PARITY.md](../PARITY.md) — Rust 移植 parity 状态
- [ROADMAP.md](../ROADMAP.md) — 路线图
- [PHILOSOPHY.md](../PHILOSOPHY.md) — 项目意图与系统设计
- [docs/navigation-file-context.md](./navigation-file-context.md) — 终端导航、`@path` 文件上下文
- [docs/local-openai-compatible-providers.md](./local-openai-compatible-providers.md) — 本地 Provider 与 Skills 安装
- [docs/windows-install-release.md](./windows-install-release.md) — Windows 安装与 release
- [docs/container.md](./container.md) — 容器化工作流
- [docs/modules/hooks-system-detail.md](./modules/hooks-system-detail.md) — Hooks 系统细化方案
- [docs/modules/dag-orchestration-detail.md](./modules/dag-orchestration-detail.md) — DAG 编排详情
- [docs/modules/ide-integration-detail.md](./modules/ide-integration-detail.md) — IDE 集成
- [docs/MODEL_COMPATIBILITY.md](./MODEL_COMPATIBILITY.md) — 模型兼容性
- [docs/g011-acp-json-rpc-status-contract.md](./g011-acp-json-rpc-status-contract.md) — ACP JSON-RPC 状态契约
