---
name: "evolution-autosave"
description: "进化引擎自动保存：对话保存由 MCP conversation save 强制执行（已集成在 user_rules 中），任务轨迹通过 CLI record-task 可选记录。"
---

# Evolution Autosave - 进化引擎自动保存

## 保存架构（统一入口）

```
对话保存（硬性强制）          任务轨迹记录（可选增强）
      │                              │
      ▼                              ▼
┌──────────────────┐     ┌──────────────────────┐
│ MCP conversation  │     │ evolution_cli.py      │
│ save (主方案)      │     │ record-task (辅助)     │
│ ✅ IDE自动可见     │     │ ⚠️ 需 run_command调用  │
│ ✅ user_rules强制  │     │                       │
└──────────────────┘     └──────────────────────┘
      │                              │
      ▼                              ▼
      统一写入 assistant-mcp memory-mcp
      (user_profile.json + ChromaDB)
```

---

## 规则1：对话保存 — MCP conversation save（主方案，强制）

对话保存已通过 `user_rules` 中的**记忆保存规则**强制执行：

```
对话结束 → conversation(action="save", role="user", content="...")
         → conversation(action="save", role="assistant", content="...")
```

这是 **MCP 工具调用**，IDE 自动可见，无需额外配置。执行时机在 `user_rules` 中已硬性规定。

**豁免清单**：仅有纯社交寒暄且无信息性内容时可跳过。详见 `persistent-memory` 技能和 `user_rules` 中的"唯一豁免清单"。

---

## 规则2：任务轨迹 — CLI record-task（辅助方案，按需使用）

对于涉及 **≥5 次工具调用且具有复用价值** 的任务，可选调用 CLI 记录轨迹以触发进化引擎分析：

```bash
# 1. 先写轨迹 JSON 到临时文件
# 2. 然后调用:
python "d:\BCAD\AutoCAD 2014\.trae\messaging\evolution_cli.py" record-task "任务描述" "@trace.json" true "AI回复摘要" false
```

**注意**：此调用是可选的。进化引擎的核心价值在于 `hermes_evolution.py` 的周期性复盘和质量评分，而非每次任务都触发。

---

## 规则3：上下文加载 — MCP profile get（强制）

在每轮对话开始时，调用 `profile(action="get")` 获取用户上下文。这在 `user_rules` 中的"上下文加载规则"中已强制执行。

**不要**使用 CLI `get-context` 替代 MCP `profile get`。

---

## CLI 命令速查（仅任务轨迹相关）

```bash
# record-task（记录任务轨迹+触发进化引擎）
python "d:\BCAD\AutoCAD 2014\.trae\messaging\evolution_cli.py" record-task "任务描述" "@trace.json" true "AI回复摘要" false

# skill-stats（查看进化引擎技能库统计）
python "d:\BCAD\AutoCAD 2014\.trae\messaging\evolution_cli.py" skill-stats
```

---

## 两层硬性保障机制

### 第一层：规则约束（指导层）

通过 `global_rules.md` 中的**后响应协议**（结构化6步检查清单）对 AI 行为进行约束。此层仍有赖于 AI 协议遵循能力。

### 第二层：自动审计（硬性层）

通过 `save_auditor.py` + heartbeat-mcp 实现**完全独立于 AI 自觉行为**的硬性监督：

1. **每10分钟自动运行** `save_auditor.py`（通过 heartbeat-mcp 定时触发）
2. **直接查询 conversation_history.db**，统计最近5分钟/30分钟的对话保存活动
3. **检测异常模式**：
   - 30分钟内有对话但最近5分钟无保存 → 可能 AI 忘记在最后一步保存
   - 上次检查以来无新增对话 → 本轮对话没有调用 conversation save
   - 异常波动 → 批量操作或服务中断
4. **结果持久化**到 `heartbeat/save_audit.log` + `heartbeat/save_audit_marker.json`

### 可靠性评级

| 操作 | 保障方式 | 可靠性 |
|------|---------|--------|
| 对话保存 | 规则协议 + 心跳审计器（双保险） | ★★★★ |
| 上下文加载 | user_rules 规则约束 | ★★★★ |
| 任务轨迹 | AI 按需调用 CLI record-task | ★★★ |
| 技能质量 | RetrospectiveAnalyzer 周期性复盘+自动清理 | ★★★★ |

---

## 注意事项

1. **不要再同时使用 CLI `save-conversation`** — 对话保存走 MCP `conversation save`，避免重复写入
2. **任务轨迹按需记录** — 仅对具有复用价值的任务记录，一次性操作无需记录
3. **进化引擎自动维护** — `hermes_evolution.py` 的 `RetrospectiveAnalyzer` 每小时复盘一次，自动执行质量评分和低质量技能清理
4. **路径便携化** — 项目移动后 `setup_project.py` 会自动更新路径引用
