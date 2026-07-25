---
name: "windows-desktop-automation"
description: "Windows桌面应用UI自动化操作。使用winapp-wrapper（合并5个工具）为主力，flaui-mcp为补充（托盘/剪贴板）。Invoke when user needs to automate Windows desktop applications or perform GUI operations."
---

# Windows桌面UI自动化操作助手

通过 winapp-wrapper（WinApp-MCP 的 55→5 合并包装器）为主力 + flaui-mcp（托盘/剪贴板补充）实现 Windows 桌面应用自动化。

## 工具架构

```
┌──────────────────────────────────────────────────────┐
│  winapp-wrapper（主力，5个合并工具）                    │
│  底层调用 WinApp-MCP 的 55 个细粒度工具               │
├──────────────────────────────────────────────────────┤
│  desktop_launch    → 启动应用                         │
│  desktop_snapshot  → 获取UI树                         │
│  desktop_do       → 通用操作（点击/输入/快捷键/滚动等）│
│  desktop_read     → 读取元素属性/文本                 │
│  desktop_util     → 工具函数（截图/等待/释放按键）    │
└──────────────────────────────────────────────────────┘
                        │
┌──────────────────────────────────────────────────────┐
│  flaui-mcp（补充，用于winapp-wrapper不支持的操作）     │
├──────────────────────────────────────────────────────┤
│  windows_tray_list     → 列出系统托盘图标             │
│  windows_tray_invoke   → 点击托盘图标                 │
│  windows_get_clipboard → 读取剪贴板                   │
│  windows_set_clipboard → 写入剪贴板                   │
└──────────────────────────────────────────────────────┘
```

**使用优先级：**
1. ✅ 优先使用 `winapp-wrapper` 的 5 个工具（覆盖 90%+ 场景）
2. ⚠️ 遇到托盘操作或剪贴板读写时，切换到 `flaui-mcp` 的工具

---

## 合并工具详解（winapp-wrapper）

### 1. desktop_launch — 启动应用

```
desktop_launch({ app: "notepad.exe" })
desktop_launch({ app: "msedge.exe" })
desktop_launch({ app: "calc.exe" })
```

返回 appId（后续操作需要用到的标识）。

### 2. desktop_snapshot — 获取UI树

最重要的一步，先 snapshot 看清 UI 结构再操作。

```
desktop_snapshot({})                    # 默认当前窗口，深度3
desktop_snapshot({ target: "appId" })   # 指定窗口
desktop_snapshot({ maxDepth: 5 })       # 展开更深
```

返回 UI 树结构，每个元素有 name（元素名称）和 properties（属性）。
后续的 desktop_do 和 desktop_read 通过 **name** 定位元素。

### 3. desktop_do — 通用操作

一个工具覆盖所有交互操作：

```
# 点击元素
desktop_do({ action: "click", target: "确定" })
desktop_do({ action: "double_click", target: "文件名.txt" })
desktop_do({ action: "right_click", target: "项目视图" })

# 输入文字
desktop_do({ action: "type", target: "文本编辑器", value: "要输入的文字" })

# 键盘快捷键（最重要！解决flaui-mcp不能发送快捷键的问题）
desktop_do({ action: "key_combo", value: "Ctrl+Z" })    # 撤销
desktop_do({ action: "key_combo", value: "Ctrl+S" })    # 保存
desktop_do({ action: "key_combo", value: "Ctrl+A" })    # 全选
desktop_do({ action: "key_combo", value: "Alt+F4" })    # 关闭窗口
desktop_do({ action: "key_combo", value: "Ctrl+Shift+Z" })  # 重做

# 单键
desktop_do({ action: "key_press", value: "RETURN" })    # 回车
desktop_do({ action: "key_press", value: "ESCAPE" })    # 取消
desktop_do({ action: "key_press", value: "TAB" })       # 切换焦点
desktop_do({ action: "key_press", value: "DELETE" })    # 删除
desktop_do({ action: "key_press", value: "F5" })        # 刷新

# 滚动
desktop_do({ action: "scroll", target: "列表元素", direction: "down" })
desktop_do({ action: "scroll", target: "列表元素", direction: "up" })

# 拖放
desktop_do({ action: "drag", sourceTarget: "文件A", destTarget: "文件夹B" })
```

### 4. desktop_read — 读取元素信息

```
# 读取元素文本
desktop_read({ target: "显示为 0" })

# 读取元素详细属性
desktop_read({ target: "确定按钮", mode: "properties" })

# 读取所有表单字段值
desktop_read({ mode: "all_values" })
```

### 5. desktop_util — 工具函数

```
# 截图
desktop_util({ action: "screenshot" })

# 等待元素出现
desktop_util({ action: "wait_element", target: "保存", timeout: 10 })

# 紧急释放卡住的按键
desktop_util({ action: "release_all" })

# 列出所有窗口
desktop_util({ action: "list_windows" })

# 检查会话状态（锁屏/最小化等）
desktop_util({ action: "check_session" })
```

---

## flaui-mcp 补充工具

只在需要以下功能时才使用：

```
# 系统托盘操作
windows_tray_list({ includeOverflow: true })
windows_tray_invoke({ ref: "traye1", button: "left" })

# 剪贴板操作
windows_get_clipboard()              # 读取剪贴板
windows_set_clipboard({ text: "..." }) # 写入剪贴板（winapp-wrapper不支持）
```

---

## 标准操作流程

### 核心原则

```
第一步：desktop_snapshot  → 获取UI树，找到目标元素的name
第二步：desktop_do         → 执行操作（点击/输入/快捷键等）
第三步：desktop_snapshot   → 验证操作结果
第四步：desktop_read       → 读取数据确认
```

### 流程1：启动应用并点击按钮

```
1. desktop_launch({ app: "calc.exe" })
2. desktop_snapshot({ maxDepth: 5 })
   → 看到所有按钮的 name："七", "加", "三", "等于"等
3. desktop_do({ action: "click", target: "七" })
4. desktop_do({ action: "click", target: "加" })
5. desktop_do({ action: "click", target: "三" })
6. desktop_do({ action: "click", target: "等于" })
7. desktop_read({ target: "显示为 0" })
   → 返回 "显示为 10"
```

### 流程2：读取文档内容

```
1. desktop_launch({ app: "notepad.exe" })
2. desktop_snapshot()
   → 找到 name="文本编辑器" 的文档区域
3. desktop_read({ target: "文本编辑器" })
   → 返回完整文档内容
```

### 流程3：发送快捷键（WinApp-MCP 优势）

```
# Ctrl+Z 撤销（1步完成，比flaui-mcp的3步菜单操作高效）
desktop_do({ action: "key_combo", value: "Ctrl+Z" })

# 保存文档
desktop_do({ action: "key_combo", value: "Ctrl+S" })

# 关闭应用
desktop_do({ action: "key_combo", value: "Alt+F4" })
```

### 流程4：完整工作流

```
1. desktop_launch({ app: "notepad.exe" })
2. desktop_snapshot()
   → 找到 "文本编辑器"
3. desktop_do({ action: "type", target: "文本编辑器", value: "施工记录内容" })
4. desktop_do({ action: "key_combo", value: "Ctrl+S" })
   → 保存（如果弹出保存对话框，用snapshot找到确认按钮点击）
5. desktop_snapshot() → 检查弹窗
6. 如有对话框 → snapshot找到"保存/不保存"按钮 → click
7. desktop_util({ action: "screenshot" }) → 截取证据
```

---

## 常用快捷键速查

| 快捷键 | 调用方式 | 用途 |
|--------|---------|------|
| Ctrl+Z | `desktop_do({ action: "key_combo", value: "Ctrl+Z" })` | 撤销 |
| Ctrl+C | `desktop_do({ action: "key_combo", value: "Ctrl+C" })` | 复制 |
| Ctrl+V | `desktop_do({ action: "key_combo", value: "Ctrl+V" })` | 粘贴 |
| Ctrl+X | `desktop_do({ action: "key_combo", value: "Ctrl+X" })` | 剪切 |
| Ctrl+S | `desktop_do({ action: "key_combo", value: "Ctrl+S" })` | 保存 |
| Ctrl+A | `desktop_do({ action: "key_combo", value: "Ctrl+A" })` | 全选 |
| Alt+F4 | `desktop_do({ action: "key_combo", value: "Alt+F4" })` | 关闭窗口 |
| F5 | `desktop_do({ action: "key_press", value: "F5" })` | 刷新 |
| ESC | `desktop_do({ action: "key_press", value: "ESCAPE" })` | 取消 |
| ENTER | `desktop_do({ action: "key_press", value: "RETURN" })` | 确认 |
| TAB | `desktop_do({ action: "key_press", value: "TAB" })` | 切换焦点 |
| DELETE | `desktop_do({ action: "key_press", value: "DELETE" })` | 删除 |
| Ctrl+Shift+Z | `desktop_do({ action: "key_combo", value: "Ctrl+Shift+Z" })` | 重做 |
| Ctrl+P | `desktop_do({ action: "key_combo", value: "Ctrl+P" })` | 打印 |
| Ctrl+F | `desktop_do({ action: "key_combo", value: "Ctrl+F" })` | 查找 |

---

## 实战经验

### 能稳定工作的做法

| 操作 | 推荐做法 |
|------|----------|
| 点击按钮 | snapshot 找到元素name → desktop_do(action:"click") |
| 读取文档 | snapshot 找文档区域 → desktop_read |
| 输入文本 | desktop_do(action:"type", value:"...") |
| 键盘快捷键 | desktop_do(action:"key_combo", value:"Ctrl+Z") |
| 返回/确认 | desktop_do(action:"key_press", value:"RETURN") |
| 取消/关闭 | desktop_do(action:"key_press", value:"ESCAPE") |
| 滚动 | desktop_do(action:"scroll", direction:"down") |
| 拖放 | desktop_do(action:"drag", sourceTarget/destTarget) |
| 截图 | desktop_util(action:"screenshot") |
| 等待元素 | desktop_util(action:"wait_element", target:"名称") |
| 托盘操作 | windows_tray_list / windows_tray_invoke（flaui-mcp） |
| 剪贴板 | windows_get/set_clipboard（flaui-mcp） |

### 易踩坑的地方

| 坑 | 解决方案 |
|----|----------|
| **snapshot 后找不到目标元素** | 增大 maxDepth，或检查窗口是否正确 |
| **快捷键无效** | 确保目标窗口有焦点，先 click 激活窗口 |
| **screenshot 太大** | 用裁剪参数或降低分辨率 |
| **winapp-wrapper 不支持的操作** | 切换回 flaui-mcp 的工具（托盘/剪贴板） |
| **拖放不准确** | 确保 sourceTarget 和 destTarget 名称唯一 |
