# Parity Status — claw-code Rust Port

> 本文档继承自上游 [dong382258137/claw-code](https://github.com/dong382258137/claw-code)（MIT License）。
> Phase 9（2026-11-26）起收缩为「当前 Web-first 形态下仍适用」的条目：
> CLI/TUI/REPL、命令行 flag、终端渲染与 mock parity harness 相关记录随能力删除一并移除。
> 历史版本见 git 历史，回退点 tag：`phase8-pre-rename`。

## 当前产品形态

- 唯一产品界面：本地 Web UI；唯一启动入口 `claw serve`。
- 主二进制 `claw`（唯一构建产物，兼容名 `claw-plus` 已移除）；独立二进制 `im-bridge`。
- 前端源码 `rust/crates/claw-server/web`，构建产物 `dist/index.html` 内嵌进二进制。
- 语义检索：可选 `onnxruntime.dll` + `bge-small-zh-v1.5`（512 维）；缺失时降级关键词匹配。

## 仍然有效的运行时对齐项

- **工具面**：`tools` crate 暴露内置工具规格并统一走 `execute_tool()` 分发；任务、团队、cron、MCP、LSP 均有 registry 支撑。
- **权限**：`PermissionEnforcer` + `PermissionPolicy` 强制执行 read-only / workspace-write / full-access；文件写入做工作区边界校验。
- **文件工具**：读/写大小上限、二进制检测、符号链接逃逸与 `../` 逃逸防护。
- **多根工作区**：`additional_workspace_roots` + `WorkspacePathScope`；Windows `\\?\` verbatim 前缀在比较前剥离。
- **MCP / LSP**：stdio/http/sse/ws 传输的 MCP 生命周期；LSP registry 提供 symbols/references/diagnostics/definition/hover。
- **会话与记忆**：JSONL 为正文真相源；`.claw/history.db`（FTS5 + 向量）与 `.claw/sessions.db`（读取索引）为派生缓存。
- **定时任务**：5 段 cron 解析、真实到点执行、专属会话、停机只补最近一次。
- **上下文压缩**：自动/手动压缩，压缩前后 token 可观测。

## 已删除能力（不再是 parity 目标）

- TUI、REPL、`prompt`/`doctor`/`status`/`config`/`skills`/`mcp` 等全部 CLI 子命令（Phase 3）。
- 终端渲染栈、输出 verbosity、`--add-dir` 等 CLI flag（随界面层删除）。
- DeepSeek 网页登录后端（`web-bridge`）不再参与主二进制依赖图（Phase 3）。
- mock parity harness 及其脚本（对应集成测试已删除）。

## 仍有限制

- MCP 的端到端深度依赖外部服务器实现，注册表层之外的行为不做保证。
- `im-bridge` 与部分独立 crate 不在主二进制依赖图中，需单独构建。
- CI 目前覆盖 fmt/clippy/test、Windows `--version`/`--help` smoke 与前端构建；服务级端到端用例仍在补充。

## 验证入口

```bash
# 仓库根
scripts/fmt.sh --check
cd rust
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# 前端
cd crates/claw-server/web && npm ci && npm run build
```

发布体积与启动/内存数据见
[`docs/2026-11-25-web-first-slimming-performance-plan.md`](./docs/2026-11-25-web-first-slimming-performance-plan.md) 的 Phase 8 实测表。
