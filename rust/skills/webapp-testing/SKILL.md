---
name: webapp-testing
description: Toolkit for interacting with and testing local web applications using Playwright. Supports verifying frontend functionality, debugging UI behavior, capturing browser screenshots, and viewing browser logs.
license: Complete terms in LICENSE.txt
---

# Web Application Testing

To test local web applications, write native Python Playwright scripts.

**Helper Scripts Available**:
- `scripts/with_server.py` - Manages server lifecycle (supports multiple servers)

**Always run scripts with `--help` first** to see usage. DO NOT read the source until you try running the script first and find that a customized solution is abslutely necessary. These scripts can be very large and thus pollute your context window. They exist to be called directly as black-box scripts rather than ingested into your context window.

## Decision Tree: Choosing Your Approach

```
User task → Is it static HTML?
    ├─ Yes → Read HTML file directly to identify selectors
    │         ├─ Success → Write Playwright script using selectors
    │         └─ Fails/Incomplete → Treat as dynamic (below)
    │
    └─ No (dynamic webapp) → Is the server already running?
        ├─ No → Run: python scripts/with_server.py --help
        │        Then use the helper + write simplified Playwright script
        │
        └─ Yes → Reconnaissance-then-action:
            1. Navigate and wait for networkidle
            2. Take screenshot or inspect DOM
            3. Identify selectors from rendered state
            4. Execute actions with discovered selectors
```

## Example: Using with_server.py

To start a server, run `--help` first, then use the helper:

**Single server:**
```bash
python scripts/with_server.py --server "npm run dev" --port 5173 -- python your_automation.py
```

**Multiple servers (e.g., backend + frontend):**
```bash
python scripts/with_server.py \
  --server "cd backend && python server.py" --port 3000 \
  --server "cd frontend && npm run dev" --port 5173 \
  -- python your_automation.py
```

To create an automation script, include only Playwright logic (servers are managed automatically):
```python
from playwright.sync_api import sync_playwright

with sync_playwright() as p:
    browser = p.chromium.launch(headless=True) # Always launch chromium in headless mode
    page = browser.new_page()
    page.goto('http://localhost:5173') # Server already running and ready
    page.wait_for_load_state('networkidle') # CRITICAL: Wait for JS to execute
    # ... your automation logic
    browser.close()
```

## Reconnaissance-Then-Action Pattern

1. **Inspect rendered DOM**:
   ```python
   page.screenshot(path='/tmp/inspect.png', full_page=True)
   content = page.content()
   page.locator('button').all()
   ```

2. **Identify selectors** from inspection results

3. **Execute actions** using discovered selectors

## Common Pitfall

❌ **Don't** inspect the DOM before waiting for `networkidle` on dynamic apps
✅ **Do** wait for `page.wait_for_load_state('networkidle')` before inspection

## Best Practices

- **Use bundled scripts as black boxes** - To accomplish a task, consider whether one of the scripts available in `scripts/` can help. These scripts handle common, complex workflows reliably without cluttering the context window. Use `--help` to see usage, then invoke directly. 
- Use `sync_playwright()` for synchronous scripts
- Always close the browser when done
- Use descriptive selectors: `text=`, `role=`, CSS selectors, or IDs
- Add appropriate waits: `page.wait_for_selector()` or `page.wait_for_timeout()`

## Reference Files

- **examples/** - Examples showing common patterns:
  - `element_discovery.py` - Discovering buttons, links, and inputs on a page
  - `static_html_automation.py` - Using file:// URLs for local HTML
  - `console_logging.py` - Capturing console logs during automation

---

# 附录：外部网站自动化（edge-cli）

> 本附录整合自原 `edge-cli-operation` 技能。当任务涉及**外部网站浏览、网页抓取、截图存档、PDF生成**等场景（非本地 webapp 测试）时，使用 edge-cli 工具更高效。

## 工具选择决策表

| 场景 | 推荐工具 | 原因 |
|------|---------|------|
| 测试本地 webapp（npm run dev / python server） | **Playwright Python 脚本** + `scripts/with_server.py` | 自动管理服务器生命周期，可深度调试 |
| 静态 HTML 文件测试 | **Playwright Python 脚本**（file:// 协议） | 无需启动服务器 |
| 操控外部网站（搜索、抓取、登录） | **edge-cli** | CLI 工具调用简洁，CDP 重连支持跨命令复用浏览器 |
| 网页截图、PDF 存档 | **edge-cli** | 一行命令完成，无需编写脚本 |
| 造价信息查询等业务网站 | **edge-cli** | 已验证流程（见工作流D） |

## edge-cli 概述

基于 Playwright 的命令行工具，通过结构化命令操控 Microsoft Edge 浏览器。

**安装**：
```bash
pip install -e C:\Users\38225\edge-cli
edge-cli --version    # 应输出 1.0.0
```

**核心设计原则**：
1. 浏览器会话持久 - 首次 `open` 启动，后续命令通过 CDP 端口 9222 重连，直到 `close`
2. Headless 模式默认 - 适合后台操作，调试时用 `--visible`
3. JSON 输出 - 智能体调用务必加 `--json`
4. CSS 选择器 - 通过 `html` 命令查看页面结构确定选择器

## edge-cli 命令参考

### 导航（6个）
```bash
edge-cli open <url> [--visible] [--wait MS] [--json]
edge-cli navigate <url> [--wait MS] [--json]
edge-cli back [--json]
edge-cli forward [--json]
edge-cli reload [--json]
edge-cli close [--json]
```

### 交互（8个）
```bash
edge-cli click <selector> [--wait MS] [--json]
edge-cli fill <selector> <value> [--json]      # 清空后填入
edge-cli type <selector> <value> [--delay MS] [--json]  # 逐字追加
edge-cli press <key> [--json]                   # Enter/Tab/Escape/ArrowDown/Control+a
edge-cli select <selector> <value> [--json]
edge-cli hover <selector> [--json]
edge-cli scroll [--direction up|down] [--amount N] [--json]
edge-cli upload <selector> <file_path> [--json]
```

### 内容获取（6个）
```bash
edge-cli screenshot [--path PATH] [--selector SELECTOR] [--full-page] [--json]
edge-cli text [--selector SELECTOR] [--json]
edge-cli html [--selector SELECTOR] [--json]
edge-cli links [--json]
edge-cli title [--json]
edge-cli url [--json]
```

### JavaScript 执行
```bash
edge-cli eval <script> [--json]
```

### 标签页管理（4个）
```bash
edge-cli tabs [--json]
edge-cli switch-tab <index> [--json]
edge-cli new-tab <url> [--json]
edge-cli close-tab [--index N] [--json]
```

### 工具命令
```bash
edge-cli wait [--selector SELECTOR] [--timeout MS] [--json]
edge-cli pdf <path> [--json]
edge-cli cookies [--url URL] [--json]
```

## edge-cli 典型工作流

### 工作流A：搜索并提取信息
```bash
edge-cli open "https://www.bing.com" --wait 2000 --json
edge-cli fill "#sb_form_q" "海南造价信息 2026" --json
edge-cli click "#search_icon" --wait 2000 --json
edge-cli links --json
edge-cli click ".b_algo h2 a" --wait 2000 --json
edge-cli text --json
edge-cli close --json
```

### 工作流B：网页截图存档
```bash
edge-cli open "https://example.com" --wait 3000 --json
edge-cli screenshot --path "d:\项目\08-照片\网页截图.png" --full-page --json
edge-cli close --json
```

### 工作流C：保存网页为PDF
```bash
edge-cli open "https://example.com" --wait 3000 --json
edge-cli pdf "d:\项目\11-其他\网页存档.pdf" --json
edge-cli close --json
```

### 工作流D：查询造价信息
```bash
edge-cli open "http://www.hnzjxx.com" --wait 3000 --json
edge-cli html --selector "nav" --json
edge-cli fill "input[name='keyword']" "水泥" --json
edge-cli click "button.search" --wait 2000 --json
edge-cli text --json
edge-cli screenshot --path "d:\项目\10-材料设备\造价信息-水泥.png" --json
edge-cli close --json
```

## edge-cli 调用规范

### 调用前必做
1. **先 open 再操作** - 所有交互命令需先调用 `open` 启动浏览器
2. **加 --json** - 智能体调用时务必加 `--json`
3. **操作后关闭** - 完成操作后调用 `close` 释放资源

### CSS 选择器获取流程
```
需要交互但不知道选择器
    → 先调用 html --json 查看页面结构
    → 从 HTML 中确定 CSS 选择器
    → 使用选择器执行交互
```

### 等待策略
| 场景 | 等待方式 |
|------|---------|
| 页面加载 | `open --wait 3000` |
| 点击后等待 | `click --wait 2000` |
| 等待元素出现 | `wait --selector ".result" --timeout 10000` |
| 固定等待 | `wait --timeout 2000` |

### 错误处理
| 错误信息 | 原因 | 解决方法 |
|---------|------|---------|
| `Timeout exceeded` | 元素未找到或页面未加载 | 增加 wait 时间，检查选择器 |
| `Element not found` | CSS 选择器错误 | 先用 html 命令查看页面结构 |
| `Browser not connected` | 浏览器未启动或已关闭 | 重新调用 open 命令 |

## edge-cli 已知限制
| 限制 | 原因 | 替代方案 |
|------|------|---------|
| 跨进程不共享浏览器 | CLI 每次调用独立进程 | 通过 CDP 端口 9222 重连 |
| headless 模式可能被检测 | 网站检测 headless | 使用 `--visible` 模式 |
| 验证码无法自动处理 | 需人工识别 | 使用 `--visible` 模式手动处理 |
| PDF 仅 headless 可用 | Playwright 限制 | 需 PDF 时不要用 --visible |

## 经验记录
- 25 个命令全部 E2E 测试通过（2026-06-06）
- Bing 搜索框选择器稳定，百度在 headless 下不稳定
- CDP 重连机制可用，实现跨进程浏览器复用
- 复杂 SPA 页面需更长的 `wait --selector` 等待