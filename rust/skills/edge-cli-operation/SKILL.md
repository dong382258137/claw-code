---
name: edge-cli-operation
description: "Edge浏览器CLI自动化操作。触发词：打开网页、浏览器操作、网页截图、网页搜索、Edge自动化、edge-cli。基于Playwright，支持--json结构化输出，Trae IDE和Hermes均可调用。"
version: 1.0.0
适用智能体: 全部
最后更新: 2026-06-06
项目: 牙南村委会产业配套设施完善项目
合同金额: 833万元
---

# Edge CLI - 智能体Edge浏览器自动化操作

基于Playwright的命令行工具，让AI智能体通过结构化命令操控Microsoft Edge浏览器。

> **本技能是浏览器自动化的底层工具**，被以下上层技能引用：
> - `cost-estimation` — 造价评估（查询造价信息网站）
> - `construction-document-composer` — 工程文书（查询法规标准）
> - `correspondence-management` — 联系函（查询项目信息）

---

# 第一章：安装与验证

## 安装

```bash
pip install -e C:\Users\38225\edge-cli
```

## 验证

```bash
edge-cli --version    # 应输出 1.0.0
edge-cli --help       # 列出全部27个命令
```

## 依赖

- Python 3.10+
- playwright >= 1.40.0
- click >= 8.0.0
- Microsoft Edge浏览器已安装

---

# 第二章：核心设计原则

## 1. 浏览器会话是持久的

首次调用 `open` 启动浏览器后，后续命令复用同一浏览器实例（通过CDP端口9222重连）。直到调用 `close` 才关闭。

## 2. Headless模式是默认的

默认无头模式运行（不显示浏览器窗口），适合智能体后台操作。需要可视化调试时用 `--visible`。

## 3. JSON输出是智能体的标准接口

所有命令支持 `--json` 标志。**智能体调用时务必加 --json**。

## 4. CSS选择器是核心交互方式

所有交互命令使用CSS选择器定位元素。可用 `html` 命令查看页面结构来确定选择器。

---

# 第三章：命令完整参考

## 3.1 导航（6个命令）

### open - 打开URL

```bash
edge-cli open <url> [--visible] [--wait MS] [--json]
```

### navigate - 导航到URL

```bash
edge-cli navigate <url> [--wait MS] [--json]
```

### back / forward / reload / close

```bash
edge-cli back [--json]
edge-cli forward [--json]
edge-cli reload [--json]
edge-cli close [--json]
```

---

## 3.2 交互（8个命令）

### click - 点击元素

```bash
edge-cli click <selector> [--wait MS] [--json]
```

### fill - 填充输入框（清空后填入）

```bash
edge-cli fill <selector> <value> [--json]
```

### type - 逐字输入（追加模式）

```bash
edge-cli type <selector> <value> [--delay MS] [--json]
```

### press - 按键

```bash
edge-cli press <key> [--json]
```

常用键：`Enter`、`Tab`、`Escape`、`ArrowDown`、`Control+a`

### select / hover / scroll / upload

```bash
edge-cli select <selector> <value> [--json]
edge-cli hover <selector> [--json]
edge-cli scroll [--direction up|down] [--amount N] [--json]
edge-cli upload <selector> <file_path> [--json]
```

---

## 3.3 内容获取（6个命令）

### screenshot - 截图

```bash
edge-cli screenshot [--path PATH] [--selector SELECTOR] [--full-page] [--json]
```

### text - 获取文本内容

```bash
edge-cli text [--selector SELECTOR] [--json]
```

### html - 获取HTML内容

```bash
edge-cli html [--selector SELECTOR] [--json]
```

### links / title / url

```bash
edge-cli links [--json]
edge-cli title [--json]
edge-cli url [--json]
```

---

## 3.4 JavaScript执行

### eval - 执行JavaScript

```bash
edge-cli eval <script> [--json]
```

---

## 3.5 标签页管理（4个命令）

```bash
edge-cli tabs [--json]
edge-cli switch-tab <index> [--json]
edge-cli new-tab <url> [--json]
edge-cli close-tab [--index N] [--json]
```

---

## 3.6 工具命令

```bash
edge-cli wait [--selector SELECTOR] [--timeout MS] [--json]
edge-cli pdf <path> [--json]
edge-cli cookies [--url URL] [--json]
```

---

# 第四章：典型工作流

## 工作流A：搜索并提取信息

```bash
edge-cli open "https://www.bing.com" --wait 2000 --json
edge-cli fill "#sb_form_q" "海南造价信息 2026" --json
edge-cli click "#search_icon" --wait 2000 --json
edge-cli links --json
edge-cli click ".b_algo h2 a" --wait 2000 --json
edge-cli text --json
edge-cli close --json
```

## 工作流B：网页截图存档

```bash
edge-cli open "https://example.com" --wait 3000 --json
edge-cli screenshot --path "d:\牙南项目\08-照片\网页截图.png" --full-page --json
edge-cli close --json
```

## 工作流C：保存网页为PDF

```bash
edge-cli open "https://example.com" --wait 3000 --json
edge-cli pdf "d:\牙南项目\11-其他\网页存档.pdf" --json
edge-cli close --json
```

## 工作流D：查询造价信息（预算员）

```bash
edge-cli open "http://www.hnzjxx.com" --wait 3000 --json
edge-cli html --selector "nav" --json
edge-cli fill "input[name='keyword']" "水泥" --json
edge-cli click "button.search" --wait 2000 --json
edge-cli text --json
edge-cli screenshot --path "d:\牙南项目\10-材料设备\造价信息-水泥.png" --json
edge-cli close --json
```

---

# 第五章：智能体调用规范

## 5.1 调用前必做

1. **先open再操作**：所有交互命令需要先调用 `open` 启动浏览器
2. **加 --json**：智能体调用时务必加 `--json`
3. **操作后关闭**：完成操作后调用 `close` 释放资源

## 5.2 CSS选择器获取流程

```
需要交互但不知道选择器
    → 先调用 html --json 查看页面结构
    → 从HTML中确定CSS选择器
    → 使用选择器执行交互
```

## 5.3 等待策略

| 场景 | 等待方式 |
|------|---------|
| 页面加载 | `open --wait 3000` |
| 点击后等待 | `click --wait 2000` |
| 等待元素出现 | `wait --selector ".result" --timeout 10000` |
| 固定等待 | `wait --timeout 2000` |

## 5.4 错误处理

| 错误信息 | 原因 | 解决方法 |
|---------|------|---------|
| `Timeout exceeded` | 元素未找到或页面未加载 | 增加wait时间，检查选择器 |
| `Element not found` | CSS选择器错误 | 先用html命令查看页面结构 |
| `Browser not connected` | 浏览器未启动或已关闭 | 重新调用open命令 |

---

# 第六章：已知限制与替代方案

| 限制 | 原因 | 替代方案 |
|------|------|---------|
| 跨进程不共享浏览器 | CLI每次调用是独立进程 | 通过CDP端口9222重连已运行的浏览器 |
| headless模式页面可能不同 | 网站检测headless | 使用 `--visible` 模式 |
| 验证码无法自动处理 | 需要人工识别 | 使用 `--visible` 模式手动处理 |
| PDF仅headless可用 | Playwright限制 | 需PDF时不要用--visible |
| 复杂SPA页面需更多等待 | 动态加载 | 使用 `wait --selector` 等待元素 |

---

# 第七章：经验记录

### 执行记录

| 日期 | 场景 | 执行结果 | 经验教训 | 状态 |
|------|------|----------|----------|------|
| 2026-06-06 | 全面测试25个命令 | 25通过/0失败 | UTF-8编码修复、Bing选择器适配 | 已验证 |
| 2026-06-06 | 百度搜索框选择器 | 失败 | headless模式下百度搜索框选择器不同，改用Bing | 已验证 |
| 2026-06-06 | CDP重连机制 | 成功 | 通过端口9222实现跨进程浏览器复用 | 已验证 |

### 已验证流程

- 25个命令全部可用，E2E测试通过
- 搜索交互工作流完整跑通
- 截图和PDF保存工作流完整跑通
- CDP重连机制可用

### 待修正问题

- 百度搜索框在headless模式下选择器不稳定，建议优先使用Bing
- 复杂SPA页面可能需要更长的等待时间

---

# 参考

- `word-cli` — Word文档CLI自动化（与edge-cli同属CLI工具系列）
- `cost-estimation` — 造价评估（引用本技能查询造价信息网站）
- `construction-document-composer` — 工程文书（引用本技能查询法规标准）
