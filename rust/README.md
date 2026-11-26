# 🦞 CLAW — Rust Workspace

> 早期代码基于 [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code)（MIT License），现已独立演进。
> 产品形态为本地 Web UI；唯一启动入口是 `claw serve`（CLI/TUI 已在 Phase 3 移除）。

任务导向的使用说明见 [`../USAGE.md`](../USAGE.md)。

## Quick Start

```bash
cd rust

# 构建 Web 服务（产出 claw 与兼容名 claw-plus）
cargo build --release -p claw-server

# 启动（浏览器打开日志中带 token 的 URL）
./target/release/claw serve --port 8787 --no-open

# 可选：语义检索需要 onnxruntime.dll >= 1.24 与可执行文件同目录
```

## Configuration

通过环境变量或 `~/.claw/settings.json` 提供 Provider 凭证：

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
export ANTHROPIC_BASE_URL="https://your-proxy.com"   # 可选
```

配置层级：`~/.claw/settings.json` → `.claw.json` → `.claw/settings.json`
→ `.claw/settings.local.json`。

## Features

| Feature | Status |
|---------|--------|
| 本地 Web UI（会话、流式对话、工具卡片、文件/终端面板） | ✅ |
| Anthropic / OpenAI-compatible provider flows + streaming | ✅ |
| 工具系统（bash, read, write, edit, grep, glob） | ✅ |
| Web tools（search, fetch） | ✅ |
| 多智能体 DAG / 子智能体 | ✅ |
| 任务、计划与定时任务（真实到点执行） | ✅ |
| 插件 / Skills / MCP / Hooks | ✅ |
| 会话持久化、索引、导出、压缩 | ✅ |
| 权限系统（read-only / workspace-write / full-access） | ✅ |
| 中文语义检索（bge-small-zh-v1.5，可选 onnxruntime.dll） | ✅ |
| IM 桥接（独立二进制） | ✅ |

## Workspace Layout

```text
rust/
├── Cargo.toml              # Workspace root
├── Cargo.lock
└── crates/
    ├── api/                  # Provider 客户端 + SSE + 请求预检
    ├── claw-server/          # Web 服务、调度器、Web 前端（claw / claw-plus）
    ├── runtime/              # 会话、记忆、工具运行时、SQLite 索引
    ├── tools/                # 工具实现与调度
    ├── commands/             # 斜杠命令、技能/插件市场
    ├── plugins/              # 插件生命周期
    ├── im-bridge/            # 独立 IM 桥接二进制
    ├── claw-acp/             # ACP 协议
    ├── claw-shell/           # 独立 shell 二进制
    ├── web-bridge/           # DeepSeek 网页登录后端（独立 crate）
    ├── telemetry/            # 会话追踪与用量遥测
    ├── llm-eval/             # 评测工具
    └── mock-deepseek-service/# 本地 mock 服务
```

### Crate Responsibilities

- **api** —— provider 客户端、SSE 流式、认证、上下文窗口预检
- **claw-server** —— axum 路由、调度器、turn 执行、Web 前端（内嵌单页）
- **runtime** —— 会话、配置、权限、MCP、提示词组装、FTS5/向量索引、记忆
- **tools** —— 工具规格与执行：Bash、文件 I/O、搜索、Web、Agent、Todo、Skill 等
- **commands** —— 斜杠命令定义、解析、帮助文本、技能/插件市场
- **plugins** —— 插件元数据、安装/启停/更新
- **im-bridge** —— 飞书 / Discord 等远程操控，独立进程

## Verification

```bash
# 仓库根
scripts/fmt.sh --check
cd rust
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Web 前端
cd crates/claw-server/web && npm ci && npm run build
```

## Stats

- **Binary:** `claw`（兼容 `claw-plus`）
- **Release 二进制：** 22.88 MB（Phase 8 实测，Windows）
- **默认权限：** `workspace-write`（以 Web 设置页为准）

## License

See repository root.
