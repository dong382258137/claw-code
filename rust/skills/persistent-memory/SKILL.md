---
name: "persistent-memory"
description: "持久记忆技能。自动保存重要对话、智能提取用户偏好、上下文感知问答、知识库管理。当用户需要记住信息、回忆历史、更新偏好时自动调用。"
---

# Persistent Memory - 持久记忆技能

## 架构定位（主保存入口）

**本技能是对话保存的唯一主入口。** 所有对话保存走 MCP `conversation save`，不通过 CLI。

```
对话保存（硬性强制）
      │
      ▼
┌──────────────────────────────────┐
│ MCP conversation save（本技能）    │  ← 唯一主入口
│ ✅ IDE 工具列表直接可见            │
│ ✅ user_rules 强制执行             │
└──────────────────────────────────┘
      │
      ▼
┌──────────────────────────────────┐
│ assistant-mcp memory-mcp          │
│ (user_profile.json + ChromaDB)    │
└──────────────────────────────────┘
```

> **注意**：不要同时使用 `evolution_cli.py save-conversation`。对话保存统一走 MCP `conversation save`。
> 任务轨迹记录（可选增强）走 `evolution_cli.py record-task`，详见 `evolution-autosave` 技能。

## 与 evolution-autosave 的边界

| 能力 | 归属 | 调用方式 | 触发时机 |
|------|------|---------|---------|
| 对话保存 | **persistent-memory（本技能）** | MCP `conversation save` | 每轮对话（user_rules 强制） |
| 用户画像管理 | **persistent-memory（本技能）** | MCP `profile get/update` | 每轮对话开始 |
| 偏好设置 | **persistent-memory（本技能）** | MCP `profile set_pref/get_pref` | 检测到偏好关键词 |
| 记忆检索 | **persistent-memory（本技能）** | MCP `memory recall/search/semantic_search` | 用户询问或上下文需要 |
| 任务轨迹记录 | **evolution-autosave** | CLI `evolution_cli.py record-task` | 可选，≥5 次工具调用且具有复用价值 |
| 自动审计 | **evolution-autosave** | `save_auditor.py`（heartbeat-mcp 触发） | 每 10 分钟自动运行 |
| 技能质量复盘 | **evolution-autosave** | `hermes_evolution.py` RetrospectiveAnalyzer | 每小时自动运行 |

**关键原则**：
- 用户相关的**信息保存与检索** → 使用 persistent-memory
- 进化引擎的**任务轨迹与质量审计** → 使用 evolution-autosave
- 不要混淆：对话保存永远走 persistent-memory 的 MCP 入口，不要用 CLI save-conversation

---

## 核心功能

本技能为 TRAE IDE 提供持久记忆能力，让 AI 能够：

1. **记住用户信息** - 用户偏好、兴趣、习惯
2. **保存对话历史** - 对话自动存档（通过 MCP conversation save）
3. **上下文感知** - 基于历史提供个性化响应
4. **知识库管理** - 存储和检索用户知识

---

## 工作流程

### 1. 上下文加载（强制执行，对话第一步）

**在处理任何用户消息之前（无论来自企业微信还是IDE直接输入），必须先执行：**

```python
profile(action="get")  # 加载用户画像、偏好、当前任务、最近对话
```

此步骤的目的是：
- 获取当前用户是谁（name）
- 了解用户的技能、兴趣、当前进行中的项目
- 获取回复风格偏好（detailed/concise）
- 查看最近讨论的话题和对话历史
- 基于以上信息提供个性化回复

**这是强制执行步骤，不可跳过。** 无论是来自企业微信的桥接消息还是用户直接在IDE输入的内容，都必须先加载上下文再处理。

### 2. 自动记忆保存

当检测到以下情况时，自动保存记忆：

- 用户明确表示偏好（"我喜欢..."、"我不想要..."）
- 用户提供个人信息（"我是..."、"我的工作是..."）
- 决策或结论
- 用户请求记住某事

### 3. 智能检索

根据用户查询，智能检索相关记忆：

- 关键词搜索
- 时间范围过滤
- 相关性排序
---

## 使用场景

### 场景1：用户偏好记忆

```
用户: "我喜欢简洁的回答，不要太啰嗦"

AI操作:
1. 调用 profile(action="set_pref", key="response_style", value="concise")
2. 保存记忆: key="user_preference_response_style", value="concise"
3. 确认: "好的，我会记住您喜欢简洁的回答风格。"
```

### 场景2：个人信息记忆

```
用户: "我是Python开发者，主要做数据分析"

AI操作:
1. 调用 profile(action="update", field="skills", value=["Python", "数据分析"])
2. 保存记忆: key="user_profession", value="Python数据分析师"
3. 后续对话中会记住这个信息
```

### 场景3：上下文感知

```
用户: "继续上次的工作"

AI操作:
1. 调用 profile(action="get") 获取上下文
2. 检查 current_task 和 recent_topics
3. 基于历史继续工作
```

### 场景4：知识检索

```
用户: "我之前问过关于ETH的分析吗？"

AI操作:
1. 调用 memory(action="search", query="ETH 分析")
2. 返回相关记忆和对话历史
```

---

## MCP工具调用

### 记忆管理

```python
memory(action="save", key="user_favorite_color", value="蓝色", ttl=None)
memory(action="recall", key="user_favorite_color")
memory(action="search", query="颜色", limit=5)
memory(action="forget", key="user_favorite_color")
```

### 用户画像

```python
profile(action="update", field="interests", value=["加密货币", "AI", "自动化"])
profile(action="update", field="skills.0", value="Python")
```

### 偏好设置

```python
profile(action="set_pref", key="notification_enabled", value=False)
profile(action="get_pref", key="notification_enabled")
```

### 对话历史

```python
conversation(action="save", role="user", content="分析ETH走势", metadata={"topic": "trading"})
profile(action="get")
```

## 自动化规则

### 规则1：偏好关键词检测
当用户消息包含"我喜欢..."、"我不喜欢..."、"设置..."等关键词时自动提取偏好。

### 规则2：个人信息检测
当用户消息包含"我是..."、"我的工作是..."、"我在..."等模式时自动更新画像。

### 规则3：重要信息检测
当用户明确要求"记住..."、"别忘了..."时保存记忆。

---

## 数据结构

### 用户画像 (user_profile.json)
```json
{
  "name": "用户名", "language": "zh-CN",
  "timezone": "Asia/Shanghai",
  "interests": ["加密货币", "AI"],
  "skills": ["Python", "数据分析"]
}
```

### 用户偏好 (preferences.json)
```json
{
  "response_style": "detailed",
  "notification_enabled": true,
  "auto_save_conversations": true,
  "memory_retention_days": 90,
  "privacy_level": "normal"
}
```

### 上下文缓存 (context_cache.json)
```json
{
  "current_task": "分析ETH走势",
  "recent_topics": ["ETH", "BTC", "技术分析"],
  "active_projects": ["交易系统"],
  "last_interaction": "2026-05-13T12:00:00"
}
```

## 与其他技能的协同

### 与 computer-use 协同
用户: "记住这个图表的形态" → computer-use 截图分析 → persistent-memory 保存分析结果

### 与 tradingview-analyzer 协同
用户: "记住我对ETH的分析偏好" → tradingview-analyzer 获取设置 → persistent-memory 保存偏好

---

## 隐私与安全

### 数据存储
- 所有数据存储在本地 `.trae/memory/` 目录
- 敏感数据可加密存储
- 用户可随时查看、修改、删除记忆

### 数据保留
- 默认保留90天
- 可配置保留策略
- 过期记忆自动清理

### 隐私级别
- **normal**: 正常记忆，可被检索
- **sensitive**: 敏感记忆，加密存储
- **temporary**: 临时记忆，短期保存

## 最佳实践

1. **及时保存信息** - 主动识别并保存，不等用户要求
2. **避免重复记忆** - 保存前检查是否已存在
3. **合理使用TTL** - 临时信息短期，重要信息永久
4. **定期清理** - 清理过期和无用记忆
5. **上下文优先** - 回答时优先考虑上下文信息

---

## 配置选项

| 选项 | 默认值 | 说明 |
|------|--------|------|
| auto_save_conversations | true | 自动保存对话 |
| memory_retention_days | 90 | 记忆保留天数 |
| notification_enabled | true | 启用通知 |
| privacy_level | "normal" | 隐私级别 |
| response_style | "detailed" | 响应风格 |

---

## 故障排除

| 问题 | 原因 | 解决 |
|------|------|------|
| 记忆无法保存 | 数据库权限问题 | 检查 `.trae/memory/` 目录权限 |
| 搜索结果不准确 | 关键词匹配不够智能 | 使用语义搜索或更具体搜索词 |
| 上下文丢失 | 上下文缓存被清空 | 检查 `context_cache.json`

## v2.0 语义搜索 (2026-05-13)

升级 memory-mcp 支持向量语义搜索：

### 新增工具
- `memory(action="semantic_search", query=...)` - 自然语言语义搜索记忆
- `conversation(action="search", query=...)` - 搜索话题相关的历史对话
- `memory(action="index")` - 构建记忆向量索引
- `conversation(action="index")` - 构建对话向量索引
- `conversation(action="status")` - 查看向量数据库状态

### 技术栈
- **ChromaDB** - 向量数据库（持久化存储）
- **paraphrase-multilingual-MiniLM-L12-v2** - 多语言嵌入模型
- **cosine 相似度** - 语义匹配算法

### 安装依赖
```bash
pip install chromadb sentence-transformers
```

### 使用方式
首次使用前需构建索引：
1. 调用 `memory(action="index")` 索引已有记忆
2. 调用 `conversation(action="index")` 索引已有对话
3. 之后每次保存记忆/对话会自动索引

### 搜索对比
| 方式 | `memory(action="search")` | `memory(action="semantic_search")` |
|------|----------------|------------------------|
| 精度 | 精确关键词匹配 | 模糊语义匹配 |
| "用户喜欢什么颜色" | 只匹配含"颜色"的记忆 | 匹配"蓝色"、"偏好"等相关记忆 |
| "上次ETH分析结论" | 只匹配含"ETH分析"的记忆 | 匹配所有ETH相关讨论 |
| 速度 | 快 | 中等 |

---

## 更新日志

### v2.0.0 (2026-05-13)
- ✅ 集成 ChromaDB 向量数据库
- ✅ 新增语义搜索记忆
- ✅ 新增语义搜索对话历史
- ✅ 自动向量索引（save时同步写入）
- ✅ 状态查询工具

### v1.0.0 (2026-05-13)
- ✅ 实现基础记忆存储
- ✅ 用户画像管理
- ✅ 偏好设置
- ✅ 对话历史保存
- ✅ 上下文感知
- ✅ 搜索功能