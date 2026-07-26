---
name: web-design-guidelines
description: Review UI code for Web Interface Guidelines compliance. Use when asked to "review my UI", "check accessibility", "audit design", "review UX", or "check my site against best practices".
metadata:
  author: vercel
  version: "1.0.0"
  argument-hint: <file-or-pattern>
---

# Web Interface Guidelines

Review files for compliance with Web Interface Guidelines.

## How It Works

1. Fetch the latest guidelines from the source URL below
2. Read the specified files (or prompt user for files/pattern)
3. Check against all rules in the fetched guidelines
4. Output findings in the terse `file:line` format

## Guidelines Source

Fetch fresh guidelines before each review:

```
https://raw.githubusercontent.com/vercel-labs/web-interface-guidelines/main/command.md
```

Use WebFetch to retrieve the latest rules. The fetched content contains all the rules and output format instructions.

## Usage

When a user provides a file or pattern argument:
1. Fetch guidelines from the source URL above
2. Read the specified files
3. Apply all rules from the fetched guidelines
4. Output findings using the format specified in the guidelines

If no files specified, ask the user which files to review.

## 离线降级方案

当 WebFetch 无法访问源 URL（网络不可用、GitHub 被墙等）时，按以下顺序降级：

1. **检查本地缓存**：查找 `%USERPROFILE%\.trae-cn\cache\web-interface-guidelines.md`（首次成功 fetch 后应缓存到此路径）
2. **使用内置基线规则**：若本地缓存也不存在，应用以下最小基线规则集进行审查：

### 内置基线规则（最小集）

| 维度 | 基线规则 |
|------|---------|
| 可访问性 | 所有图片需有 `alt` 属性；表单控件需有关联 `label`；颜色对比度 ≥ 4.5:1（正常文本） |
| 语义化 | 使用语义化 HTML 标签（`<nav>`/`<main>`/`<article>`/`<section>`）；避免 `div` 滥用 |
| 响应式 | 使用 viewport meta 标签；避免固定宽度；触摸目标 ≥ 44×44 px |
| 性能 | 图片指定 `width`/`height` 或使用 `aspect-ratio`；关键资源预加载；避免 layout shift |
| 键盘导航 | 所有交互元素可通过 Tab 到达；可见 focus 状态；逻辑 tab order |
| 错误处理 | 表单错误需文本说明（非仅颜色）；错误信息关联到字段（`aria-describedby`） |

3. **告知用户**：在审查结果开头注明"⚠️ 已使用离线基线规则，可能不包含最新规则。网络恢复后建议重新审查。"

### 缓存更新

当 WebFetch 成功时，将内容写入本地缓存：
```bash
# 伪代码示例
WebFetch(url) → content → Write("%USERPROFILE%\.trae-cn\cache\web-interface-guidelines.md", content)
```
