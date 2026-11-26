# CLAW 使用指南（Web-first）

> Phase 3 起 CLI/TUI/REPL 与全部子命令已移除；Phase 9 将 crate 更名为 `claw-server`。
> 当前唯一产品界面是本地 Web UI，唯一启动入口是 `claw serve`。
> 构建后运行 `claw serve`，浏览器打开启动日志中带 token 的 URL。

## 1. 构建

```bash
cd rust
cargo build --release -p claw-server
```

产物：`rust/target/release/claw.exe`（Windows）或 `claw`（macOS/Linux）。
同一 crate 还会产出兼容名 `claw-plus`（保留一个版本周期）。

需要语义检索时，把 `onnxruntime.dll`（≥1.24）放到 `claw.exe` 同目录；
不需要时可直接忽略，服务会自动降级为关键词匹配（见 §7）。

## 2. 首次启动

### 2.1 配置模型凭证

任选一个 Provider，通过环境变量或设置文件提供凭证：

| Provider | 模型前缀 | 凭证 |
|---|---|---|
| Anthropic | `anthropic/` 或无前缀 | `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` |
| OpenAI | `openai/` 或 `gpt-` | `OPENAI_API_KEY` |
| xAI | `grok/` | `XAI_API_KEY` |
| 阿里 DashScope | `qwen/` | `DASHSCOPE_API_KEY` |
| 本地 OpenAI 兼容服务 | `openai/` + 自定义 base URL | 通常无需 |

### 2.2 启动服务

```powershell
# Windows（推荐：先 cd 到目标工作区所在目录）
.\rust\target\release\claw.exe serve --port 8787 --no-open

# 指定工作区
.\rust\target\release\claw.exe serve --port 8787 --cwd D:\work\my-repo --no-open
```

启动日志会打印：

```text
claw web: http://127.0.0.1:8787/?t=<一次性 token>
```

复制整条 URL 到浏览器打开即可。`--no-open` 表示不自动拉起浏览器；
默认只监听 `127.0.0.1`，所有 API 都需要 URL 中的 token。

## 3. Web UI 能做什么

- **会话侧栏**：新建、恢复、删除、导出（Markdown / JSON）、跨工作区聚合。
- **流式对话**：SSE 逐 token 渲染 Markdown，展示工具卡片与上下文占用条。
- **工具审批**：每次工具调用带状态、输入/输出预览与权限确认。
- **文件与终端面板**：工作区文件浏览、diff 查看、内嵌终端。
- **工作区初始化**：一键生成 `.claw.json`、`.gitignore`、`CLAUDE.md`。
- **上下文压缩**：手动压缩并落盘，显示压缩前后 token。
- **任务与计划**：任务面板、计划面板、定时任务（cron）。
- **技能 / 插件市场**：浏览、安装、启停、更新。
- **设置页**：主题、权限模式、模型、语义检索状态。

## 4. 工作区与配置

会话正文仍以 JSONL 为准；工作区 `.claw/` 下是同名 SQLite 索引：

- `.claw/history.db` —— 全文（FTS5）+ 向量检索索引
- `.claw/sessions.db` —— 会话列表/消息区间索引
- `.claw/crons.json` —— 定时任务定义
- `.claw/settings.local.json` —— 本机覆盖项

配置优先级（后者覆盖前者）：

1. `~/.claw/settings.json`（用户）
2. `.claw.json`（项目，可提交）
3. `.claw/settings.json`（项目共享）
4. `.claw/settings.local.json`（本机，不提交）

## 5. 权限模式

- `read-only` —— 只读文件与搜索
- `workspace-write` —— 允许工作区内写入
- `danger-full-access` —— 完全访问（需显式选择）

定时任务继承 Web 设置页的权限，并强制最高为 `workspace-write`。

## 6. 定时任务

在 Web「任务」面板新建 cron 任务（5 段：分 时 日 月 周，支持 `*`、`*/n`、`a-b`、列表）。
任务到点后进入既有调度器排队执行，每个任务绑定一个专属会话；
进程停机期间错过的执行只补最近一次。运行结果与下次执行时间在面板中可见。

## 7. 语义检索与 onnxruntime.dll

- 本地嵌入模型：`bge-small-zh-v1.5`（512 维，中文检索）。
- ONNX Runtime **不内嵌**在主二进制中，运行时从 `onnxruntime.dll` 动态加载。
- 查找顺序：`ORT_DYLIB_PATH` → `claw.exe` 同目录 → 当前目录。
- 缺失或版本不兼容时：服务正常启动，检索自动降级为关键词匹配；
  设置页「关于 → 语义检索」显示「关键词降级」及原因。
- 首次检索会把 ONNX 模型下载到 `%USERPROFILE%\.cache\fastembed`
  （可用 `FASTEMBED_CACHE_DIR` 或 `HF_HOME` 覆盖）。
- 发布打包：`powershell -ExecutionPolicy Bypass -File scripts/package-release.ps1`
  会复制 `claw.exe`、`claw-plus.exe`、`onnxruntime.dll`、README 并打 zip。

## 8. 排障

| 现象 | 处理 |
|---|---|
| 端口被占用 | 换 `--port`，或结束占用端口的旧 `claw` 进程 |
| 页面 401 / 无法访问 | 使用启动日志里的完整 URL（含 `?t=`），token 每次启动都会更新 |
| 模型 401 / 404 | 检查对应 Provider 凭证与模型前缀；本地服务检查 base URL |
| 语义检索不可用 | 确认 `onnxruntime.dll` 与 exe 同目录（≥1.24），或设置 `ORT_DYLIB_PATH` |
| 首次检索很慢 | 首次需下载 ONNX 模型；观察 `.cache\fastembed` 是否增长 |
| 前端改动不生效 | 在 `rust/crates/claw-server/web` 下 `npm run build` 重新生成内嵌单页 |
| release 构建慢 | 已启用 fat LTO + 单 codegen-unit，属于预期；日常开发用 debug 构建 |

启动日志中的 `embedding unavailable (...)` 会给出降级原因；
`/api/meta` 的 `embedding.state` 取值为 `ready` / `keyword` / `loading` / `disabled`。

## 9. 测试与 CI

```bash
# Rust 校验（仓库根）
scripts/fmt.sh --check
cd rust
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Web 前端
cd rust/crates/claw-server/web
npm ci && npm run build
```

CI（`.github/workflows/rust-ci.yml`）包含 workspace 测试、Windows smoke
（`--version` / `--help`）、前端构建校验；仅构建 `claw-server`（`claw` /
`claw-plus`）与独立二进制 `im-bridge`。

## 10. 相关文档

- [`README.md`](./README.md) —— 项目概览与构建入口
- [`docs/2026-11-25-web-first-slimming-performance-plan.md`](./docs/2026-11-25-web-first-slimming-performance-plan.md) —— Web-first 瘦身与性能方案及进度
- [`docs/windows-install-release.md`](./docs/windows-install-release.md) —— Windows 安装与发布
- [`rust/README.md`](./rust/README.md) —— Rust workspace 与 crate 地图
- [`PARITY.md`](./PARITY.md) —— 与上游的行为对齐现状
