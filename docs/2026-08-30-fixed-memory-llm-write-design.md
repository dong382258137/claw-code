# fixed_memory LLM 写入 + 300s 前瞻触发 · 设计文档

> 日期：2026-08-30 · 状态：待实施 · 关联目标：提升记忆简报质量且不破坏缓存命中率

## 1. 背景与目标

现状：`fixed_memory.json` 由规则收敛 `task_state.json + lessons.jsonl` 生成（零成本、确定性），但质量天花板受限于规则关键词——简报缺结构化文件锚点（缺口 D），AI 对已完成改动仍可能发散求证。

目标：改为 **LLM 写入** fixed_memory（输出目标/已完成项含文件锚点/教训/下一步的结构化简报），写入时机绑定 **DeepSeek 缓存的真实 TTL**（距上次同前缀请求 ≤ 300s），用「300s 前瞻触发」把更新成本摊进「反正要重建」的冷启轮。

## 2. 缓存 TTL 的精确语义（设计前提）

- DeepSeek 缓存命中判定：**距上次同前缀请求 ≤ 300s** 且前缀逐字节相同。
- 因此「缓存即将过期」= 距上次请求接近 300s 且期间无新请求重置。
- 运行时可观测量：本会话 `last_request_at_ms`（上一轮请求时间）+ 上轮 `cache_read_input_tokens`（是否命中）。
- 结论：**前瞻窗口取 TTL 的 90%（270s）**——距上次请求 > 270s 时，下一请求大概率冷启（期间无请求重置 TTL），此时更新 fixed_memory 的成本摊进重建轮；若用户恰在 270–300s 窄窗内回来且缓存还热，损失一次命中换取简报更新，可接受。

## 3. 核心设计

### 3.1 触发判定（请求构造时）

```
if now - last_request_at_ms > PRECEDING_WINDOW_MS(270_000)
   AND 距上次摘要 > MIN_SUMMARY_INTERVAL_MS(60_000)   // 防抖
   AND 有变更（摘要点后存在新 tool result / assistant 文本）  // 变更门控
→ 触发 LLM 增量摘要 → 更新 fixed_memory.json（content/fingerprint/injected_at_ms=now）
```

- `last_request_at_ms`：会话内最后一条 assistant 消息时间戳（新增跟踪）。
- 与既有 `cache_hot`（A 修复）兼容：`cache_hot=true` 说明上轮命中（距上轮 < 300s），走复用路径；前瞻触发只看时间窗口，两者独立。

### 3.2 LLM 增量摘要（Graphiti 融合）

- 复用 [compact.rs](file:///d:/claw-code-src/rust/crates/runtime/src/compact.rs#L173) 的 LLM 摘要能力（`summarize_messages_with_llm` / `CompactionSummarizerClient`）。
- **增量输入**：只喂「上次摘要点 `last_summary_msg_index` 之后」的消息（排除 fixed_memory 注入消息本身，防自循环）。
- **摘要点**：`fixed_memory.json` 新增字段 `last_summary_msg_index`，摘要成功后更新。
- Prompt 约束：输出固定结构（目标 / 已完成项(含文件锚点) / 教训 / 下一步），**只总结已有内容，不推断新增**（幻觉护栏之一）。

### 3.3 变更门控

摘要点之后若消息无实质新内容（无新 tool result 文本、无新 assistant 结论），跳过调用并延长窗口——避免每 ~5 分钟空转一次 LLM 调用。

### 3.4 幻觉交叉校验（护栏）

LLM 输出与规则 `task_state.findings` 交叉校验：LLM 声称「已完成」的事项若在 task_state 中无对应且不可从消息中定位验证来源，在简报中标注「(未经验证)」。轻量实现：prompt 要求标注来源 + 落盘后由规则对含文件名的行做存在性 sanity（可选）。

### 3.5 失败降级

LLM 调用失败/超时 → 不更新 fixed_memory（复用旧快照字节），emit_diag 记录。既有 `next_injection` 的 prev 兜底路径天然兼容。

### 3.6 MemGPT 融合（模型主动管理，P2）

激活已死掉的 Persona/Human/Tasks 块：新增 `memory_update` 工具，让模型主动固化用户偏好/身份事实（写 blocks + entries），进入 system 前缀（frozen_render）跨会话稳定；nudge 规则提取保留为兜底。

### 3.7 TTL 对齐前缀（保持既有）

fixed_memory 仍在 messages[0]（前缀区）；LLM 更新只发生在前瞻触发（冷启）轮 → 热窗内前缀逐字节不变，99% 命中率不回退。

## 4. 数据模型变更

`fixed_memory.json`：
```json
{
  "content": "…(LLM 生成的锚点型简报)…",
  "fingerprint": 123,
  "injected_at_ms": 1788060000000,
  "last_summary_msg_index": 42
}
```
`last_summary_msg_index` 缺失时视为 0（全量摘要）。

## 5. 代码落点

- `runtime/src/fixed_memory.rs`：`next_injection` 增加前瞻触发判定入口（或新增 `maybe_trigger_llm_summary`）；`FixedMemorySnapshot` 加字段；摘要点维护。
- `runtime/src/conversation.rs`：请求构造处接入前瞻判定；跟踪 `last_request_at_ms`；触发 LLM 摘要（复用 compact 摘要客户端）。
- `runtime/src/compact.rs`：暴露可复用的摘要函数（若当前为私有则公开）。
- 测试：触发窗口判定、变更门控、失败降级、摘要点推进、前缀仍字节稳定。

## 6. 阶段划分与验证

| 阶段 | 内容 | 验证 |
|---|---|---|
| P0 | LLM 写入 + 前瞻触发 + 变更门控 + 失败降级（不动 task_state 规则提取） | cargo test；真实会话：热窗命中率仍 ≥97%，冷启轮简报含文件锚点 |
| P1 | 增量输入（摘要点）+ 幻觉交叉校验 | 增量输入 token 下降；无未验证断言 |
| P2 | memory_update 工具激活 blocks（MemGPT） | blocks 写入生效、跨会话前缀稳定 |

每阶段独立提交并重编译部署，对照 `_analyze_cache_hit.py` 确认命中率不回落。

## 7. 风险与对策

- **前缀被自己打断**：前瞻窗口 270s 保守取 TTL 90%，且变更门控减少无谓触发；热窗复用逻辑（cache_hot）保持。
- **LLM 成本**：增量输入（通常远小于主请求）+ 变更门控跳过空窗。
- **幻觉**：prompt 约束「只总结」+ 交叉校验 + 注脚。
- **循环摘要**：增量输入排除 fixed_memory 注入消息。
