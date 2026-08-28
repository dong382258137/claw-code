---
name: browser-control
description: Drive a persistent Chrome session through the native browser_control tool (Chrome DevTools Protocol). Use when the user needs browser automation — navigating pages, clicking and filling forms, taking accessibility snapshots or screenshots, extracting page data, testing web apps, or any task requiring real browser interaction. Prefer it over WebFetch for JavaScript-heavy pages or anything needing clicks/typing.
allowed-tools: browser_control
---

# Browser Control (browser_control)

`browser_control` 是 claw 的原生浏览器自动化工具：通过 CDP 驱动一个**持久的 Chrome 会话**（跨多次调用复用同一浏览器）。它让 AI 具备"看懂页面 → 精准操作 → 验证结果"的完整闭环，不需要猜测。

## 核心工作流（感知 → 行动 → 验证）

1. **`launch`** — 启动浏览器（默认有窗口；`headless: true` 可静默）。
   **或 `connect`** — 附加到**已运行的**浏览器/Electron 应用：`port: 9222`（展开为 `http://127.0.0.1:9222`）或 `url`（`http://…` / `ws://…` 均可）。
2. **`goto`** — 打开目标 URL（`url` 参数）。
3. **`snapshot`** — 获取可访问性树快照。**每个可交互元素带稳定引用 `[ref=eN]`**，是后续操作元素的依据。
4. **用 `ref` 交互** — `click` / `fill` / `type` / `press_key` / `select_option` / `check` / `uncheck` / `hover` / `scroll`。
5. **验证** — `get_state`（读页面 URL/标题 + 元素实时状态）或 `screenshot` 确认动作生效。
6. **页面变化后必须重新 `snapshot`** 刷新 refs（旧 ref 会失效）。

```text
browser_control { action: "launch" }
browser_control { action: "goto", url: "https://example.com/login" }
browser_control { action: "snapshot" }
# → - textbox "Email" [e1] / - textbox "Password" [e2] / - button "Sign in" [e3]
browser_control { action: "fill", ref: "e1", text: "user@example.com" }
browser_control { action: "fill", ref: "e2", text: "password123" }
browser_control { action: "click", ref: "e3" }
browser_control { action: "get_state" }            # 验证是否已跳转
browser_control { action: "snapshot" }             # 刷新 refs
```

## ref 生命周期（重要）

- refs 来自**最近一次** `snapshot`，页面任何变化（导航、表单提交、动态加载）后**必须重新 snapshot**。
- 找不到 ref 会报错 "ref not found in the last snapshot" → 重新 `snapshot` 即可。
- 有 CSS `selector` 时可作后备，但优先用 ref（更稳、不会命中错误元素）。

## 感知动作

| 动作 | 用途 |
|------|------|
| `snapshot` | 可访问性树快照，含层级、角色、名称、状态标记（disabled/checked 等）与 `[ref=eN]`；大页面会截断（240 行）并提示缩小聚焦范围 |
| `get_state` | 读当前 URL/标题 + 指定元素的实时状态（文本/值/选中/禁用/可见/坐标），用 `ref` 或 `selector` 指定元素；动作后用它验证 |
| `screenshot` | 视觉反馈；默认存 `<cwd>/.claw/browser_shots/<时间戳>.png`，可用 `save_path` 指定 |
| `wait_for` | 条件等待：`url:…`（URL 包含）\| `text:…`（页面文本出现）\| CSS 选择器 \| `networkidle`（网络空闲）；`timeout_ms` 默认 5000 |

## 交互动作

| 动作 | 参数 | 说明 |
|------|------|------|
| `click` | ref/selector | 真实鼠标事件（坐标级），隐藏元素自动回退 JS click |
| `fill` | ref + text | 清空后输入（表单首选） |
| `type` | ref + text | 追加输入 |
| `press_key` | key | Enter/Tab/Escape/Backspace/Delete/ArrowUp/Down/Left/Right/Home/End/PageUp/PageDown/Space 或单字符 |
| `hover` | ref/selector | 鼠标悬停（触发 hover 态） |
| `scroll` | direction | up/down/top/bottom 滚页面；`element`（+ref/selector）滚元素进视口 |
| `select_option` | ref + value | 下拉框选值 |
| `check` / `uncheck` | ref/selector | 复选框 |

## 标签页与其它

- `new_tab { url }` 开新标签、`switch_tab { index }` / `close_tab { index }`、`list_tabs` 查看列表。
- `evaluate_js { script }` 执行 JS 表达式（返回 by-value 结果），适合取数据或绕过复杂交互。
- 完成记得 `close`：自启浏览器会被关闭；**connect 附加的外部浏览器只会断开连接、不会被关闭**（要关闭外部浏览器需用户手动处理）。

## 常见模式

**表单提交**：goto → snapshot → 逐个 fill → click 提交按钮 → wait_for（url 或 text 变化）→ snapshot 验证结果。

**登录**：同上；凭据用环境变量或用户提供，不要写死在对话历史；登录后如需复用会话保持 `launch` 不 `close`。

**数据抽取**：goto → snapshot → 需要时 `evaluate_js` 取结构化数据（如 `document.querySelectorAll(...)` 汇总），或 `get_state` 逐元素读取。

**验证步骤**：任何关键交互后先 `get_state` / `wait_for`，确认页面进入预期状态再继续，避免在错误页面上盲目操作。

## 注意事项

- 会话跨调用持久：同一会话内多个 `browser_control` 调用共享浏览器与标签页，**不要重复 launch**。
- 权限为高危（DangerFullAccess）：涉及表单提交、账号操作、删除等敏感动作前向用户说明并确认。
- 遇到弹窗/对话框卡住：优先 `evaluate_js` 处理或 `press_key { key: "Escape" }`，必要时 `close` 重启会话。
