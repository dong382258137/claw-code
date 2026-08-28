---
name: electron
description: Automate Electron desktop apps (VS Code, Slack, Discord, Figma, Notion, Spotify, etc.) by connecting the native browser_control tool to the app's Chrome DevTools Protocol port. Use when the user needs to interact with an Electron app, automate a desktop app, connect to a running app, control a native app, or test an Electron application. Triggers include "automate Slack app", "control VS Code", "interact with Discord app", "test this Electron app", "connect to desktop app", or any task requiring automation of a native Electron application.
allowed-tools: browser_control
---

# Electron App Automation

Electron 应用基于 Chromium，会暴露 Chrome DevTools Protocol (CDP) 端口。用原生 `browser_control` 工具的 `connect` 动作附加到该端口，即可复用与网页相同的"快照 → 交互"工作流。不需要任何外部 CLI。

## 核心工作流

1. **用 `--remote-debugging-port` 启动** Electron 应用（或让用户重启应用带上该参数）
2. **`connect`** 附加到 CDP 端口（`port` 或 `url`）
3. **`snapshot`** 发现可交互元素（拿到 `[ref=eN]`）
4. **用 `ref` 交互**
5. 状态变化后**重新 `snapshot`** 刷新 refs

```text
# 以远程调试端口启动应用（以 Slack 为例，Windows）
"C:\Users\%USERNAME%\AppData\Local\slack\slack.exe" --remote-debugging-port=9222

# 附加到该应用
browser_control { action: "connect", port: 9222 }
# → { "status": "connected", "tab_count": N, ... }  会话建立，已有页面被接管

# 标准工作流
browser_control { action: "snapshot" }
browser_control { action: "click", ref: "e5" }
browser_control { action: "screenshot", save_path: "slack.png" }
```

## 启动 Electron 应用并开启 CDP

所有 Electron 应用都内置 Chromium，支持 `--remote-debugging-port`。

### macOS

```bash
open -a "Slack" --args --remote-debugging-port=9222
open -a "Visual Studio Code" --args --remote-debugging-port=9223
open -a "Discord" --args --remote-debugging-port=9224
open -a "Figma" --args --remote-debugging-port=9225
open -a "Notion" --args --remote-debugging-port=9226
```

### Linux

```bash
slack --remote-debugging-port=9222
code --remote-debugging-port=9223
discord --remote-debugging-port=9224
```

### Windows

```powershell
"C:\Users\%USERNAME%\AppData\Local\slack\slack.exe" --remote-debugging-port=9222
"C:\Users\%USERNAME%\AppData\Local\Programs\Microsoft VS Code\Code.exe" --remote-debugging-port=9223
```

**重要**：应用已在运行时，先退出再用带参数的方式重新启动；`--remote-debugging-port` 必须在启动时带上。连接前可 `sleep 3` 等待应用初始化。

## 连接

```text
# 按端口连接（推荐）
browser_control { action: "connect", port: 9222 }

# 或直接给端点 URL
browser_control { action: "connect", url: "http://127.0.0.1:9222" }
```

`connect` 会接管目标中**已打开的窗口/页面**作为标签页；若目标没有页面则新开一个。之后所有动作都作用于该会话，无需重复 connect。

## 多窗口 / Webview

Electron 应用的多个窗口/webview 会作为多个标签页列出：

```text
browser_control { action: "list_tabs" }
browser_control { action: "switch_tab", index: 1 }
```

## 常见模式

**查看并导航应用**：connect → snapshot（读输出识别 UI 元素）→ click 目标 → 重新 snapshot。

**截图**：`browser_control { action: "screenshot", save_path: "app.png" }`。

**抽取数据**：connect → snapshot → 需要时 `get_state` 逐元素读取，或 `evaluate_js` 取结构化数据。

**填表**：connect → snapshot → fill 各输入 → press_key Enter 或 click 提交 → snapshot 验证。

## 故障排查

- **连接被拒**：确认应用以 `--remote-debugging-port=NNNN` 启动；已在运行的应用需退出重启；确认端口未被占用。
- **快照看不到元素**：应用可能有多个窗口/webview，用 `list_tabs` 列出并用 `switch_tab` 切换。
- **输入框无法输入**：部分应用用自定义输入组件，可改用 `evaluate_js` 设置值或 `press_key` 组合键。

## 支持的应用

任何 Electron 应用均可：Slack、Discord、Teams、VS Code、GitHub Desktop、Postman、Figma、Notion、Obsidian、Spotify、Todoist、Linear、1Password 等。只要应用支持 `--remote-debugging-port`，就能用 `browser_control` 自动化。

**注意**：`connect` 附加的外部应用，`close` 只会断开连接、不会关闭应用本身。
