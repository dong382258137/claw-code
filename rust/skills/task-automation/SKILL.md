---
name: "task-automation"
description: "任务自动化技能。支持自动化工作流设计、条件触发器配置、任务链编排、失败重试机制。当需要创建复杂自动化流程时调用。"
---

# Task Automation - 任务自动化技能

## 核心功能

1. **工作流编排** - 多步骤任务自动化
2. **Webhook集成** - 外部系统触发
3. **条件触发** - 基于事件的自动化
4. **失败重试** - 自动错误恢复

## 使用示例

### 创建工作流

```
用户: "创建一个每日报告工作流"

AI操作:
workflow(action="create",
    name="daily_report",
    steps=[
        {"type": "script", "path": "collect_data.py"},
        {"type": "script", "path": "generate_report.py"},
        {"type": "api_call", "url": "https://api.example.com/send", "method": "POST"}
    ]
)
```

### 触发工作流

```
用户: "执行每日报告工作流"

AI操作:
workflow(action="trigger", name="daily_report")
```

详见 automation-mcp 文档。