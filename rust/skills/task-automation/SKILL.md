---
name: "task-automation"
description: "任务自动化技能。支持自动化工作流编排、定时任务、条件触发、监控警报、失败重试机制。当需要创建复杂自动化流程、设置定时任务、监控条件、自动提醒时调用。"
---

# Task Automation - 任务自动化技能

## 核心功能

1. **工作流编排** - 多步骤任务自动化，外部 Webhook 触发
2. **主动监控** - 持续监控价格、系统状态、文件变化
3. **定时任务** - 每日简报、定期报告、定时提醒（Cron 调度）
4. **条件触发** - 价格阈值、文件存在、API 状态、时间范围
5. **失败重试** - 自动错误恢复
6. **智能提醒** - 基于上下文的主动通知

---

## 工作原理

### 心跳引擎架构

```
┌─────────────────────────────────────────┐
│         Heartbeat Engine                │
│      (后台持续运行的服务)                │
└─────────────────────────────────────────┘
              │
              ├─── 监控器 (Monitors)
              │     ├── 价格监控
              │     ├── 系统监控
              │     ├── 文件监控
              │     └── 邮件监控
              │
              ├─── 条件检查 (Conditions)
              │     ├── 时间范围
              │     ├── 价格阈值
              │     ├── 文件存在
              │     └── API状态
              │
              └─── 动作执行 (Actions)
                    ├── 通知
                    ├── 脚本
                    ├── API调用
                    └── 日志记录
```

---

## 使用场景

### 场景1：每日简报

```
用户: "每天早上9点给我发送市场简报"

AI操作:
1. heartbeat(action="register", name="daily_briefing", cron="0 9 * * *", action={"type": "script", "path": ".trae/heartbeat/actions/generate_briefing.py"})
结果: 每天早上9点自动生成并发送简报
```

### 场景2：价格警报

```
用户: "当ETH价格超过2500时提醒我"

AI操作:
1. heartbeat(action="register", name="eth_price_alert", interval=60, condition={"type": "price_threshold", "symbol": "ETHUSDT", "threshold": 2500, "comparison": "above"}, action={"type": "notify", "message": "ETH价格已超过2500！"})
结果: 价格达到阈值时自动通知
```

### 场景3：系统健康检查

```
用户: "每小时检查一次系统状态"

AI操作:
1. heartbeat(action="register", name="system_health_check", interval=3600, action={"type": "script", "path": ".trae/heartbeat/actions/health_check.py"})
结果: 每小时自动检查系统状态并记录
```

### 场景4：文件监控

```
用户: "当有新文件下载时通知我"

AI操作:
1. heartbeat(action="register", name="download_monitor", interval=30, condition={"type": "file_exists", "path": "C:/Downloads/new_file.txt"}, action={"type": "notify", "message": "检测到新文件下载！"})
结果: 文件出现时自动通知
```

### 场景5：多步骤工作流

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

触发: workflow(action="trigger", name="daily_report")
```

详见 automation-mcp 文档。

---

## MCP 工具调用

### 注册心跳任务

```python
heartbeat(action="register", name="daily_report", cron="0 9 * * *", action={"type": "script", "path": "path/to/script.py"})
heartbeat(action="register", name="price_monitor", interval=60, condition={"type": "price_threshold", "symbol": "BTCUSDT", "threshold": 50000, "comparison": "above"}, action={"type": "notify", "message": "BTC价格超过50000！"})
```

### 管理心跳任务

```python
heartbeat(action="list")
heartbeat(action="trigger", name="daily_report")
heartbeat(action="enable", name="price_monitor")
heartbeat(action="disable", name="price_monitor")
heartbeat(action="status", name="daily_report")
heartbeat(action="unregister", name="price_monitor")
```

---

## 条件类型

| 类型 | 说明 | 示例 |
|------|------|------|
| time_range | 时间范围触发 | `{"start": "09:00", "end": "17:00"}` |
| price_threshold | 价格阈值触发 | `{"symbol": "ETHUSDT", "threshold": 2500, "comparison": "above"}` |
| file_exists | 文件存在触发 | `{"path": "/path/to/file"}` |
| api_status | API状态触发 | `{"url": "https://api.example.com/health", "expected_status": 200}` |
| always | 无条件触发 | `{}` |

## 动作类型

| 类型 | 说明 | 示例 |
|------|------|------|
| notify | 通知 | `{"message": "任务完成！", "level": "info"}` |
| script | 脚本执行 | `{"path": "/path/to/script.py", "args": ["arg1"]}` |
| api_call | API调用 | `{"url": "https://api.example.com/webhook", "method": "POST"}` |
| log | 日志记录 | `{"message": "心跳触发", "file": "heartbeat.log"}` |

---

## 与其他技能的协同

- **persistent-memory**: 心跳任务触发 → 保存结果到记忆 → 后续查询历史记录
- **computer-use**: 定时截图任务 → 视觉分析 → 保存分析结果
- **pine-script**: 价格监控触发 → 自动分析图表 → 生成交易建议

---

## 配置选项

在 `.trae/heartbeat/tasks.json` 中配置心跳任务。

## 最佳实践

1. **合理设置间隔** - 高频 60-300 秒，常规 300-3600 秒，定时用 Cron
2. **条件优化** - 避免复杂条件，使用缓存，设置合理阈值
3. **错误处理** - 记录失败日志，设置重试机制
4. **资源管理** - 限制并发任务数，定期清理过期任务

---

## 更新日志

### v1.1.0 (2026-07-25)

- 合并原 `proactive-assistant` 技能（心跳引擎、主动监控、定时任务、条件触发）
- 保留工作流编排能力
- 统一为单一任务自动化入口

### v1.0.0 (2026-05-13)

- 实现心跳引擎核心
- 支持 Cron 和间隔调度
- 条件检查系统
- 多种动作类型
- 任务管理 API
- 状态监控
