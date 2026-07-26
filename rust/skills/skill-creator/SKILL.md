---
name: "skill-creator"
description: "MANDATORY tool for creating SKILLs - MUST be invoked IMMEDIATELY when user wants to create/add any skill"
---

# Skill Creator

This skill helps you create new SKILLs for the workspace.

## When to Use

**CRITICAL: You MUST invoke this skill IMMEDIATELY as your FIRST action when:**
- User wants to create a new skill
- User wants to add a custom skill to the workspace
- User asks to set up a skill template
- User asks "how to create a skill"
- User mentions creating/adding/making any skill

**DO NOT:**
- Just explain how to create a skill without invoking this tool
- Provide manual instructions without calling this skill first
- Defer the skill creation to later steps

## SKILL Structure

A valid SKILL requires:

1. **Directory**: `.trae/skills/<skill-name>/`
2. **File**: `SKILL.md` inside the directory

## SKILL.md Format

```markdown
---
name: "<skill-name>"
description: "<concise description covering: (1) what the skill does, (2) when to invoke it. Keep it under 200 characters for best display>"
---

# <Skill Title>

<Detailed instructions, usage guidelines, and examples>
```

## Required Fields

| Field | Location | Description |
|-------|----------|-------------|
| `name` | frontmatter | Unique identifier for the skill |
| `description` | frontmatter | **CRITICAL**: Must include (1) what the skill does AND (2) when to invoke it. This helps the model decide when to use the skill. Keep under 200 chars. |
| `detail` | body | Full markdown content after frontmatter |

## Creation Steps

1. Ask user for skill name and purpose
2. **IMPORTANT**: When generating the `description` field, ALWAYS include:
   - What the skill does (functionality)
   - **MUST emphasize when to invoke it** (trigger conditions/scenarios)
   - Example format: "Does X. Invoke when Y happens or user asks for Z."
   - **Language**: Use English by default unless user specifies another language
3. Create directory: `.trae/skills/<skill-name>/`
4. Create `SKILL.md` with proper frontmatter and content
5. Validate the structure is correct

## Example

To create a "code-reviewer" skill:

```bash
mkdir -p .trae/skills/code-reviewer
```

Then create `.trae/skills/code-reviewer/SKILL.md`:

```markdown
---
name: "code-reviewer"
description: "Reviews code for best practices, bugs, and improvements. Invoke when user asks for code review or before merging changes."
---

# Code Reviewer

This skill reviews code and provides feedback...
```

---

# 附录：lark-cli Skill 创建（来自 lark-skill-maker）

> 本附录整合自原 `lark-skill-maker` 技能。当需要把飞书 API 操作封装成可复用的 Skill（包装原子 API 或编排多步流程）时，使用此流程。

## lark-cli 核心能力

```bash
lark-cli <service> <resource> <method>          # 已注册 API
lark-cli <service> +<verb>                      # Shortcut（高级封装）
lark-cli api <METHOD> <path> [--data/--params]  # 任意飞书 OpenAPI
lark-cli schema <service.resource.method>       # 查参数定义
```

优先级：Shortcut > 已注册 API > `api` 裸调。

## 调研 API

```bash
# 1. 查看已有的 API 资源和 Shortcut
lark-cli <service> --help

# 2. 查参数定义
lark-cli schema <service.resource.method>

# 3. 未注册的 API，用 api 直接调用
lark-cli api GET /open-apis/vc/v1/rooms --params '{"page_size":"50"}'
lark-cli api POST /open-apis/vc/v1/rooms/search --data '{"query":"5F"}'
```

如果以上命令无法覆盖需求（CLI 没有对应的已注册 API 或 Shortcut），使用 `lark-openapi-explorer` 从飞书官方文档库逐层挖掘原生 OpenAPI 接口，获取完整的方法、路径、参数和权限信息，再通过 `lark-cli api` 裸调完成任务。

通过以上流程确定需要哪些 API、参数和 scope。

## lark-cli SKILL.md 模板

文件放在 `skills/lark-<name>/SKILL.md`：

```markdown
---
name: lark-<name>
version: 1.0.0
description: "<功能描述>。当用户需要<触发场景>时使用。"
metadata:
  requires:
    bins: ["lark-cli"]
---


# <标题>

> **前置条件：** 先阅读 [`../lark-shared/SKILL.md`](../lark-shared/SKILL.md)。

## 命令

\```bash
# 单步操作
lark-cli api POST /open-apis/xxx --data '{...}'

# 多步编排：说明步骤间数据传递
# Step 1: ...（记录返回的 xxx_id）
# Step 2: 使用 Step 1 的 xxx_id
\```

## 权限

| 操作 | 所需 scope |
|------|-----------|
| xxx | `scope:name` |
```

## lark-cli Skill 关键原则

- **description 决定触发** — 包含功能关键词 + "当用户需要...时使用"
- **认证** — 说明所需 scope，登录用 `lark-cli auth login --domain <name>`
- **安全** — 写入操作前确认用户意图，建议 `--dry-run` 预览
- **编排** — 说明数据传递、失败回滚、可并行步骤
