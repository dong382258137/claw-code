# claw-desktop（桌面壳）

> 本目录的名称含义：一个 Electron 桌面"壳"，内嵌浏览器登录 DeepSeek / Claude 网页版，
> 把网页版会话当作本地 claw 守护进程（`claw-web-bridge`）的模型后端。
>
> **许可证：GPL-3.0-only**（见 [LICENSE](./LICENSE)）。
>
> **上游声明**：本项目从 [Cuckoo Code](https://github.com/wangyongpeng90/cuckoo-code)
> （GPL-3.0，名称 Cuckoo Code）裁剪复制而来。上游完整源码保留在本仓库
> `references/cuckoo-code/` 目录下（未改动），本目录的 `src/main/`、`src/preload/`、
> `src/providers/`、`src/ui/` 均派生自上游，按 GPL-3.0 要求保留版权与许可声明。

## 架构

```text
用户(网页版 DeepSeek/Claude 聊天界面)
   │  ①用户点 ⚡claw → user_run
   ▼
electron 壳(本目录)
   │  ②收到 inject_prompt → 填入输入框发送
   │  ③SSE FINISHED(read cuckoo provider 的 observeBody/createExtractor)
   │  ④回传 reply(turn_id, text)
   ▼
claw 守护进程(rust/crates/web-bridge → 二进制 claw-web-bridge)
   ws://127.0.0.1:18793/ws/ext?token=...
   │  ⑤parse_reply 解析 ```claw / ```bash 代码块 → 本地工具执行(权限策略)
   └── ⑥final 结束回执
```

与上游 Cuckoo Code 的关键差异：

| 能力 | Cuckoo Code（上游） | claw-desktop（本目录） |
| --- | --- | --- |
| 工具执行 | `tools/` JsRunner + 主进程 exec（本地自执行） | **整体删除**，执行权在 claw 守护进程（Rust `safe_commands` 二次把关） |
| 回复完成判定 | DOM 稳定 600ms（易截断误判） | SSE 流 `FINISHED` / `message_stop`（服务端权威信号） |
| 浏览器扩展 | 无 | `claw-web-bridge-extension/` 旧扩展已移除，壳内置 WS 客户端替代 |
| MCP / 自定义 Provider / 项目目录 | 壳内管理 | 移交 claw，壳只负责网页驱动 |

## 裁剪说明（相对上游 `references/cuckoo-code`）

**未复制**：`tools/`（JsRunner 与全部工具实现）、`src/main/{updater,tool-registry,project-context,mcp-config,mcp-client}.js`、
`src/preload/{dom/tool-parser,dom/tool-executor,dom/intercept-observer,dom/session-list,overlay/project-dir}.js`、
`src/providers/custom/`、`src/prompt/`、`test/`、构建/发布配置。

**保留并裁剪**：`src/main/index.js`（去 updater/MCP/自定义 provider，接 claw-client）、
`src/main/ipc.js`（只留窗口/通知 + claw 桥 IPC）、`src/preload/index.js`（去拦截-解析执行链）、
`src/preload/overlay/*`（保留窗口管理/命令确认与历史/延迟设置，去掉项目目录/MCP/会话面板）。

**新增**：`src/main/claw-client.js`（WS 客户端，断线重连 + 事件回调）、
`src/preload/dom/claw-turn.js`（inject 灌入 → SSE FINISHED 读回 → 危险命令首道拦截 → reply 闭环 + ⚡claw 按钮）。

## 快速开始

前置：本地 claw 守护进程已运行（`rust/` 下 `cargo run -p web-bridge`，监听 `127.0.0.1:18793`）。

```bash
cd desktop
npm install   # 需下载 electron 二进制，体积较大
npm start     # 或用 node start.js
```

1. 首次启动进入平台选择页，选 DeepSeek，登录一次（profile 使用持久 `partition`，之后无需重复登录）。
2. 在聊天输入框写下任务（如：读取 D:\claw-code-src\rust\CLAUDE.md 并总结）。
3. 点输入框右侧悬浮的 **⚡claw** 按钮 → 任务经 `user_run` 交给本地守护进程，
   claw 会注入多轮提示、本地执行 `await read/write/edit/glob/grep/bash(...)` 等工具，
   最终结果回到对话里。右下角面板可看到连接状态、命令确认与历史。

## 配置

默认连接 `ws://127.0.0.1:18793/ws/ext`，token `dev-claw-token`，与守护进程默认值一致。
可用环境变量覆盖：

- `CLAW_BRIDGE_HOST` / `CLAW_BRIDGE_PORT` / `CLAW_BRIDGE_TOKEN`

> 安全：壳侧先经 `src/main/dangerous-commands.js` 对危险命令做首道确认，
> 守护进程侧 `crates/web-bridge/src/safe_commands.rs` 兜底再拦一道。