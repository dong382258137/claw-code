# 斜杠命令状态表（单人自用精简版）

> 2026-10-24 · 与 `commands_handler.rs::STUB_COMMANDS` 保持一致。
> 被列入 `STUB_COMMANDS` 的命令不会出现在 `/help`、`/` 补全和 TUI 斜杠菜单中；直接输入时返回"本构建未启用"。
> 不在表中的命令均已实现。

## 已实现（本轮新增 8 条）

| 命令 | 行为 | 备注 |
|---|---|---|
| `/vim` | 切换 vim 编辑模式 | TUI 即时生效；REPL 写入 `~/.claw/settings.json`，下次启动生效 |
| `/exit` `/quit` `/q` | 退出 | TUI 空闲时立即退出；turn 运行中忽略 |
| `/keybindings` | 打印实际键位表 | 与 `?` 帮助浮层同源信息 |
| `/files` | 列出当前目录文件 | 跳过隐藏项与 `target`/`node_modules`/`dist`/`build`；深度 3、上限 100 |
| `/tasks [get <id>]` | 任务注册表列表 / 详情 | 读 `runtime::task_registry::global()` |
| `/copy [last\|all]` | 复制到剪贴板 | OSC52（SSH 可用），同时备份到 `~/.claw/last-copy.txt`；超长文本受终端 OSC52 限制 |
| `/fast` | 快速模式 | 在 `flash` 与 `pro` 之间切换模型（通过 `set_model` 重建 runtime） |
| `/feedback` | 记录本地反馈 | 追加到 `~/.claw/feedback.jsonl`，不外发 |

## 未启用（23 条，已从帮助与补全中隐藏）

| 命令 | 原因 | 替代做法 |
|---|---|---|
| `/login` `/logout` | 单账号、无云端会话 | 用 `DEEPSEEK_API_KEY` 环境变量或 `.env` |
| `/share` `/desktop` `/advisor` `/stickers` `/insights` `/thinkback` | 云服务/社交/增值功能 | 不适用单人本地 |
| `/brief` | 输出风格已有 | `/output-style` |
| `/release-notes` `/security-review` `/privacy-settings` | 面向发行与合规 | — |
| `/plan` `/review` | 计划/评审流程未接入 | `EnterPlanMode` 工具 + 直接对话 |
| `/theme` `/color` | 主题系统未做 | 终端自身配色 |
| `/rename` `/tag` `/branch` `/rewind` | 会话元数据/分支未建模 | `/session list`、`git` |
| `/hooks` | 运行时 hooks 未暴露列表命令 | 配置文件 + `hooks` 运行时 |
| `/ide` | IDE 集成走 ACP | `claw-shell`（Zed/VS Code） |
| `/add-dir` | 额外工作根在启动期构造，缺少会话内热加路径 | 启动参数/配置中设置工作区根 |

## 维护规则

1. 实现某条命令后，必须同时从 `STUB_COMMANDS` 移除，并更新本表。
2. 新增未实现命令时，加入 `STUB_COMMANDS` 并在本表登记原因，禁止只在文档里写"未实现"。
3. 每次改动跑：`cargo fmt --all`、`cargo clippy -p rusty-claude-cli --all-targets -- -D warnings`、`cargo test -p rusty-claude-cli`。
