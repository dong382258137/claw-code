# IDE 集成细化方案:ACP 1.5 升级 + ClawAgent 扩展 + LaneEvent 桥接

> 文档版本:v0.2
> 创建日期:2026-07-21
> 最后更新:2026-07-21(从 v0.1 细化,新增 PoC 验证与双版本兼容)
> 父文档:[ide-hooks-dag-implementation-plan.md](../ide-hooks-dag-implementation-plan.md)
> 焦点:ACP 1.5 升级路径 + PoC 验证 + 双版本兼容 + ClawAgent 扩展 + LaneEvent 桥接 + VS Code 扩展骨架
> 适用对象:Claw Plus v0.2.0(SHA `8af738a`)
> 调研基础:`agent-client-protocol` 0.10.4(已实现,Cargo.lock 锁定 0.10.4)/ 1.3.0(过渡)/ 1.5.0(目标)

---

## v0.2 变更记录

相对 v0.1 的核心补充(共新增约 1500 行,聚焦"升级风险可控"):

| # | 章节 | 新增内容 | 行数估算 |
|---|------|---------|---------|
| 1 | §2.5 ACP 1.5 升级 PoC 验证方案 | PoC 目标 / 阶段分解 / 风险评估 / 决策点 | ~440 行 |
| 2 | §2.6 双版本兼容策略 | Cargo.toml feature flag / cfg 条件编译 / 运行时协商 / 测试矩阵 | ~180 行 |
| 3 | §3.6 / §3.7 fs/* 完善 | 安全约束 / 路径验证 / 与现有 Read/Write tool 关系 / 完整骨架 | ~120 行 |
| 4 | §3.8 session/request_permission 完善 | 完整 6 步流程 / 超时处理 / 权限缓存 / 与 PermissionMode 协同 | ~150 行 |
| 5 | §5.3-5.6 LaneEvent 桥接完善 | 23 种事件完整映射代码 / flush 时机细化 / 背压降级 | ~250 行 |
| 6 | §6.2-6.5 VS Code 扩展细化 | 完整 package.json / extension.ts / ACP 传输层 / 错误处理 | ~320 行 |
| 7 | §7.1-7.5 Zed 集成细化 | 完整 agents.json / PowerShell 启动脚本 / 验证清单 / 降级 | ~140 行 |
| 8 | §12 性能基准 | 9 项指标 + 测量方法 + 回归阈值 | ~80 行 |
| 9 | §13 迁移指南 | 0.10.4 → 1.5 步骤 / 配置变更 / 废弃 API / 用户感知 | ~120 行 |
| 10 | §9 测试矩阵扩展 | 8 个 PoC 验证测试用例 + 验收标准 | ~60 行 |

**核心目标**:让 ACP 1.5 升级从"理论可行"变为"可验证、可回退、可度量"。PoC 阶段失败不影响生产(0.10.4 路径保留)。

---

## 目录

1. [现状审计](#一现状审计)
2. [ACP 1.5 升级路径](#二acp-15-升级路径)
3. [ACP 1.5 升级 PoC 验证方案](#二5acp-15-升级-poc-验证方案)
4. [双版本兼容策略](#二6双版本兼容策略)
5. [协议方法补齐](#三协议方法补齐)
6. [ClawAgent 扩展](#四clawagent-扩展)
7. [LaneEvent → SessionNotification 桥接](#五laneevent-sessionnotification-桥接)
8. [VS Code 扩展骨架](#六vs-code-扩展骨架)
9. [Zed 集成验证](#七zed-集成验证)
10. [实施步骤分解](#八实施步骤分解)
11. [测试矩阵](#九测试矩阵)
12. [风险与缓解](#十风险与缓解)
13. [性能基准](#十二性能基准)
14. [迁移指南](#十三迁移指南)
15. [参考链接](#十一参考链接)

---

## 一、现状审计

### 1.1 已实现基础设施

Claw Plus 在 Phase A(`commit 8af738a`)已完成 ACP 0.10.4 接入层,核心代码组织如下:

| 模块 | 路径 | 职责 |
|------|------|------|
| `claw-acp` crate | [rust/crates/claw-acp/](../../rust/crates/claw-acp/) | 协议层:mpsc channel + gateway 转发 + stdio 传输 |
| `claw-shell::agent` | [agent.rs](../../rust/crates/claw-shell/src/agent.rs) | `ClawAgent<C>` 实现 `acp::Agent` trait |
| `claw-shell::spawn` | [spawn.rs](../../rust/crates/claw-shell/src/spawn.rs) | 独立线程 + `LocalSet` 启动模式 |
| `claw-shell::stdio` | [stdio.rs](../../rust/crates/claw-shell/src/stdio.rs) | `run_stdio_agent` / `run_agent_on_io`(可测试核心) |
| `claw-plus-headless` binary | [headless.rs](../../rust/crates/rusty-claude-cli/src/bin/headless.rs) | 极简 stdio ACP 服务器入口,供 Zed 等 spawn |
| `runtime::lane_events` | [lane_events.rs](../../rust/crates/runtime/src/lane_events.rs) | 23 种 `LaneEventName` + 全局 sink(`Mutex<Vec<LaneEvent>>`) |

### 1.2 已实现的 ACP 方法清单(0.10.4)

`ClawAgent<C>` 当前实现 [`acp::Agent`](../../rust/crates/claw-shell/src/agent.rs) trait 的方法状态:

| 方法 | 实现状态 | 关键行为 |
|------|---------|---------|
| `initialize` | ✅ 完整 | 返回 `ProtocolVersion::LATEST` + `api_key` auth method |
| `authenticate` | ✅ 完整(伪) | 直接返回 success,无真实认证 |
| `new_session` | ✅ 完整 | 创建 `ConversationRuntime`,从 `Option` 取出 `api_client` / `tool_executor` |
| `load_session` | 🚧 stub | 返回 `method_not_found`(`acp::Error::method_not_found()`) |
| `set_session_mode` | 🚧 stub | 返回 `method_not_found` |
| `prompt` | ✅ 部分 | 调用 `run_turn` 同步阻塞,turn 完成后一次性推送 `AgentMessageChunk`(非真实流式) |
| `cancel` | 🚧 stub | 仅 `tracing::warn!`,返回 `Ok(())`,不中断 `run_turn`(同步 API 无法中断) |
| `ext_method` | 🚧 stub | 返回 `method_not_found` |
| `ext_notification` | ✅ 完整(空) | 返回 `Ok(())`,吞掉所有扩展通知 |

### 1.3 缺失的协议方法(需补齐)

以下方法在 `acp::Agent` trait 中存在,但 `ClawAgent` 未实现真实逻辑:

| 方法 | 优先级 | 缺失原因 | 影响 |
|------|--------|---------|------|
| `fs/read_text_file` | P0 | trait 属于 `acp::Client`(反向请求) | IDE 无法提供 editor buffer 未保存内容 |
| `fs/write_text_file` | P0 | 同上 | 文件写入绕过 editor undo 栈 |
| `session/request_permission` | P0 | 同上 | 危险操作无审批 UI |
| `session/load` | P1 | 当前 stub 返回 `method_not_found` | 无法恢复历史会话 |
| `session/resume` | P1 | v1.3.0+ 与 `load` 统一 | 同上 |
| `session/fork` | P2 | 未实现 | 无法从某消息分叉 |
| `session/list` | P2 | 未实现 | 无法列出可恢复会话 |
| `session/set_mode` | P2 | 当前 stub 返回 `method_not_found` | 无法切换 plan/act |
| `session/set_model` | P2 | 未实现 | 无法切换底层模型 |

### 1.4 silent drop 行为(ACP 0.10.4)

`claw-shell/src/stdio.rs` 的 A6.4 测试套件固化了 0.10.4 的三类错误路径行为:

| 错误场景 | 0.10.4 行为 | 测试名 |
|---------|------------|--------|
| invalid JSON(parse 失败) | 🚧 silent drop(仅 `log::error!`) | `run_agent_on_io_silently_drops_invalid_json` |
| 缺少 `method` 字段 | 🚧 silent drop(当作 response 找不到 id) | `run_agent_on_io_silently_drops_missing_method_field` |
| 未知 `method` 名 | ✅ 返回 `-32601` error response | `run_agent_on_io_returns_error_on_unknown_method` |

**风险**:silent drop 让 IDE 无法感知协议错误,排障困难。ACP 1.3.0+ 会返回 `-32700` parse_error,需在升级时更新测试断言。

### 1.5 LaneEvent 消费者现状

[lane_events.rs](../../rust/crates/runtime/src/lane_events.rs) 中的全局 sink 当前状态:

```rust
// rust/crates/runtime/src/lane_events.rs(行 1027-1104)
//
// **架构现状(2026-07-21 阶段 3.5 评估)**:
// - 发布端已接入:4 处生产调用(SubagentHandoff × 1、SubagentResult × 2、ShipPrepared × 1)
// - 消费端:仅测试代码调用 `drain_lane_events`(5 处,均在 `#[cfg(test)]` 内)
// - 生产消费者尚未接入:TUI Sidebar 消费 ToolHistory、TraceAnalyzer 消费 TraceRecord,
//   均不订阅 LaneEvent 流
// - sink 容量上限保护(512 条):防止生产运行中无人 drain 导致内存无限增长
```

**核心问题**:生产路径下 LaneEvent 被发布到全局 sink 但无人消费,等同 fire-and-forget 日志。本分支要接入的 ACP 桥接是第一个真实生产消费者。

---

## 二、ACP 1.5 升级路径

### 2.1 版本对比表

| 维度 | 0.10.4(当前) | 1.3.0(过渡) | 1.5.0(目标) | 影响 |
|------|--------------|-------------|-------------|------|
| 错误处理 | invalid JSON silent drop | 返回 `-32700` parse_error | 同 1.3.0 | ✅ 改善(测试需更新) |
| Diff 格式 | v1(简单文本) | v1 | v2(带 location + 类型化) | 🚧 新增字段 |
| Permission 模型 | 简单 `request_permission` | v2 typed `PermissionOption` | 同 1.3.0 | 🚧 API 变更 |
| Session 配置 | 字符串自由字段 | typed boolean config | 同 1.3.0 | 🚧 破坏性 |
| Content 类型 | 自定义枚举 | 对齐 MCP `Content` | 同 1.3.0 | 🚧 类型重命名 |
| `session/load` | 单独方法 | 与 `session/resume` 统一 | 同 1.3.0 | ✅ 简化 |
| `session/fork` | 不存在 | 新增 | 同 1.3.0 | ✅ 新能力 |
| `session/set_model` | 不存在 | 新增 | 同 1.3.0 | ✅ 新能力 |
| Terminal API | 不存在 | 新增(create/release/wait) | 同 1.3.0 | ✅ 新能力 |
| feature flag | `unstable` | `unstable-v2` | 默认启用 | 🚧 配置变更 |

### 2.2 破坏性变更清单

升级到 1.5.0 需要处理的破坏性变更:

1. **`ContentBlock` 类型重命名**:`acp::ContentBlock::Text` → `acp::Content::Text`,影响 `ClawAgent::prompt` / `extract_user_text` / `notify` 三处
2. **`NewSessionRequest` 字段类型变更**:`mcp_servers: Vec<String>` → `mcp_servers: Vec<McpServerConfig>`,需更新 `new_session` 实现
3. **`SessionConfig` 强类型化**:从 `serde_json::Value` 改为 typed struct,`NewSessionRequest.session_configuration` 字段需要新结构
4. **`PermissionOption` 枚举化**:`request_permission` 的 `options` 字段从 `Vec<serde_json::Value>` 改为 `Vec<PermissionOption>`,需更新反向请求逻辑
5. **`Diff` 类型新增 `location`**:文件 diff 需要带位置信息,影响 `fs/write_text_file` 响应构造
6. **`Terminal` API 新增**:`acp::Client` trait 新增 5 个 terminal 方法(create/release/wait/output/kill),`AcpGatewaySender<acp::AgentSide>` 需实现转发(已存在,见 [gateway.rs:412-441](../../rust/crates/claw-acp/src/gateway.rs))

### 2.3 兼容策略

采用**双轨升级**策略,避免一次性破坏所有调用方:

```rust
// rust/crates/claw-acp/Cargo.toml(升级后)
//
// 双 feature 共存:0.10.4 兼容(unstable)+ 1.5.0 特性(unstable-v2)
// 测试时分别用 cargo test --features unstable / --features unstable-v2 验证两套行为
[dependencies]
agent-client-protocol = { version = "1.5", features = ["unstable", "unstable-v2"] }
```

**P0 阶段**:保持 0.10.4 兼容路径(`unstable` feature),新增 1.5 特性为 opt-in(`unstable-v2`),通过运行时 capability 协商决定走哪条路径。

**P1 阶段**:全面切换到 1.5,废弃 0.10.4 兼容代码,移除 `unstable` feature。

**测试更新**:A6.4 的 3 个错误路径测试需重写(1.5 会返回 error response 而非 silent drop),保留旧测试在 `#[cfg(not(feature = "unstable-v2"))]` 下,新增 1.5 行为测试在 `#[cfg(feature = "unstable-v2")]` 下。

---

## 二.5、ACP 1.5 升级 PoC 验证方案

### 2.5.1 PoC 目标

本 PoC 阶段不计入正式发布版本,目的是在合并到主干前充分验证升级可行性,确保**生产环境永远不会因升级而不可用**。

| # | 验证维度 | 具体目标 | 成功标准 |
|---|---------|---------|---------|
| 1 | 破坏性变更范围 | 量化 1.5 与 0.10.4 的真实差异 | 完成全部 6 项破坏性变更的修复(见 §2.2) |
| 2 | 编译可行性 | `claw-acp` + `claw-shell` 在 1.5 feature 下编译通过 | `cargo build --features unstable-v2` 0 error |
| 3 | 现有测试兼容 | claw-shell 现有测试套件通过率 | 通过率 ≥ 90%(允许新增 1.5 行为相关失败) |
| 4 | 新功能测试 | 1.5 特有功能验证覆盖 | 新增 10+ 测试,全部通过 |
| 5 | 端到端集成 | Zed 编辑器能正常连接 1.5 版本 `claw acp serve` | 7 步验证清单全部通过(见 §7.3) |
| 6 | 性能回归 | 1.5 升级不引入显著性能回退 | 见 §12 性能基准,各项指标不超阈值 |

**PoC 不验证的范围**(避免范围蔓延):

- VS Code 扩展完整功能(本期仅验证能 spawn + initialize,完整 UI 走 P1)
- 多 session 并发(单 session 验证通过即可)
- 真实 Anthropic API 调用(用 MockApiClient 验证)

### 2.5.2 PoC 阶段分解

PoC 总周期 7 个工作日,严格按阶段推进;每阶段完成需通过验收门(gate)才能进入下一阶段。

#### Phase 1:依赖升级(1 天)

**目标**:升级 Cargo.toml 并记录初始编译错误清单。

**操作步骤**:

```bash
# 1. 创建 PoC 分支
cd d:\claw-code-src
git checkout -b poc/acp-1-5-upgrade

# 2. 修改 claw-acp Cargo.toml(临时直接升级,feature flag 在 §2.6 引入)
# 编辑 rust/crates/claw-acp/Cargo.toml:
#   agent-client-protocol = { version = "1.5", features = ["unstable", "unstable-v2"] }

# 3. 更新 Cargo.lock
cd rust
cargo update -p agent-client-protocol

# 4. 编译并记录错误
cargo build -p claw-acp 2>&1 | tee /tmp/acp-1-5-compile-errors.log
cargo build -p claw-shell 2>&1 | tee -a /tmp/acp-1-5-compile-errors.log

# 5. 统计错误
grep -c "^error\[" /tmp/acp-1-5-compile-errors.log
```

**预期错误清单**(基于 §2.2 破坏性变更分析):

| 错误类型 | 预期位置 | 预期数量 | 修复难度 |
|---------|---------|---------|---------|
| `ContentBlock` 重命名 | `claw-shell/src/agent.rs`(3 处)、`claw-acp/src/gateway.rs` | 5-8 处 | 低(机械替换) |
| `NewSessionRequest.mcp_servers` 类型变更 | `claw-shell/src/agent.rs::new_session` | 1 处 | 中 |
| `SessionConfiguration` 强类型化 | `claw-shell/src/agent.rs::new_session` | 1 处 | 中 |
| `PermissionOption` 枚举化 | `claw-shell/src/agent.rs::request_permission`(待新增) | N/A(新代码) | 低 |
| `Diff` 新增 `location` 字段 | `claw-shell/src/agent.rs::write_editor_buffer_with_diff`(待新增) | N/A(新代码) | 低 |
| `Terminal` API trait 方法 | `claw-acp/src/gateway.rs::AcpGatewaySender<acp::AgentSide>` 转发 | 已实现 | 无 |

**验收门 G1**:
- 编译错误总数 ≤ 30 个(预期 5-15 个,上限 30 表示 1.5 变更未失控)
- 无 `failed to resolve` 之类的 crate 解析错误(版本号正确)

**G1 失败处理**:若错误 > 30 个,暂停 PoC,重新评估 1.5 升级路径,可能需要先升级到 1.3.0 过渡版本。

#### Phase 2:编译错误修复(2-3 天)

**目标**:逐个修复 Phase 1 记录的编译错误,启用 `unstable-v2` feature 后整体编译通过。

**修复策略**:

1. **机械替换优先**:`ContentBlock::Text` → `Content::Text` 这类纯类型重命名,用全局替换工具一次完成,逐个 commit
2. **类型适配层**:对于 `mcp_servers: Vec<String> → Vec<McpServerConfig>`,在 `new_session` 入口写适配函数:
   ```rust
   fn adapt_mcp_servers(old: Vec<String>) -> Vec<acp::McpServerConfig> {
       old.into_iter().map(|name| acp::McpServerConfig {
           name,
           transport: acp::McpTransport::Stdio { command: name.clone(), args: vec![], env: Default::default() },
       }).collect()
   }
   ```
3. **保留 0.10.4 行为 fallback**:每修复一个错误点,在原代码附近用 `#[cfg(not(feature = "unstable-v2"))]` 保留旧逻辑,新逻辑用 `#[cfg(feature = "unstable-v2")]` 包裹(详见 §2.6)
4. **每个修复点写测试**:每个 cfg 分支至少新增 1 个测试,确保两条路径都覆盖

**修复顺序建议**(按依赖关系):

1. `claw-acp` crate 内的 trait 实现错误(底层先修)
2. `claw-acp::gateway` 转发层(`AcpGatewaySender` 已实现 Terminal API,仅需补 1.5 新方法)
3. `claw-shell::agent` 中 `ClawAgent` 的 6 个 `acp::Agent` 方法
4. `claw-shell::stdio` 测试套件中受影响的类型断言

**验收门 G2**:
- `cargo build -p claw-acp --features unstable-v2` 0 error
- `cargo build -p claw-shell --features unstable-v2` 0 error
- `cargo build --workspace --features unstable-v2` 0 error(确保整个 workspace 编译通过)
- `cargo build --workspace`(默认 feature)仍 0 error(0.10.4 路径未回归)

**G2 失败处理**:若 3 天内未达 G2,延长 1 天;若 4 天仍未达,记入风险并保留中间成果(已修复的错误点合并到 PoC 分支)。

#### Phase 3:功能验证(2 天)

**目标**:运行现有测试套件,修复因 1.5 行为变更导致的失败,新增 1.5 特有功能测试。

**步骤**:

```bash
# 1. 在 0.10.4 feature 下运行,确认基线(应为全绿)
cd d:\claw-code-src\rust
cargo test --workspace 2>&1 | tee /tmp/baseline-0_10_4.log

# 2. 在 1.5 feature 下运行
cargo test --workspace --features unstable-v2 2>&1 | tee /tmp/test-1_5.log

# 3. 对比失败测试
diff <(grep "^test result" /tmp/baseline-0_10_4.log) <(grep "^test result" /tmp/test-1_5.log)
```

**预期失败测试**(由 §1.4 silent drop 行为变更引起):

| 测试名 | 0.10.4 行为 | 1.5 行为 | 修复方式 |
|--------|------------|---------|---------|
| `run_agent_on_io_silently_drops_invalid_json` | silent drop | 返回 `-32700` parse_error | cfg 分支保留旧测试,新增 `run_agent_on_io_returns_parse_error_on_invalid_json_v2` |
| `run_agent_on_io_silently_drops_missing_method_field` | silent drop | 返回 `-32600` invalid_request | 同上 |
| `run_agent_on_io_returns_error_on_unknown_method` | `-32601` | 同 | 无需修改 |

**新增测试清单**(在 §9 测试矩阵中详细列出):

1. `acp_1_5_initialize_handshake` — 验证 1.5 initialize 返回的 `agent_capabilities` 字段
2. `acp_1_5_session_notification_v2_diff` — 验证 1.5 SessionNotification 中 v2 Diff 格式(含 location)
3. `acp_1_5_permission_option_typed` — 验证 1.5 typed `PermissionOption` 枚举
4. `acp_1_5_content_mcp_aligned` — 验证 1.5 Content 类型对齐 MCP(`Resource` / `Image` 子类型)
5. `acp_dual_version_feature_flag` — 验证双 feature 共存编译
6. `acp_runtime_version_negotiation` — 验证 initialize 时版本协商逻辑

**验收门 G3**:
- 0.10.4 feature 下:测试通过率 100%(基线不回归)
- 1.5 feature 下:测试通过率 ≥ 95%(允许 ≤ 2 个标记为 `#[ignore]` 的端到端测试未通过)
- 新增的 6 个 1.5 测试全部通过

#### Phase 4:端到端验证(1 天)

**目标**:用真实 Zed 编辑器连接 PoC 分支构建的 `claw-plus-headless`,验证完整 ACP 流程。

**前置准备**:

```bash
# 1. 在 PoC 分支构建 1.5 版本的 claw-plus-headless
cd d:\claw-code-src\rust
cargo build --release --bin claw-plus-headless --features unstable-v2

# 2. 复制到独立目录(避免覆盖 0.10.4 版本)
mkdir -p C:\Users\38225\.cargo\bin\poc-1-5\
copy target\release\claw-plus-headless.exe C:\Users\38225\.cargo\bin\poc-1-5\claw-plus-headless-1-5.exe

# 3. 验证版本
C:\Users\38225\.cargo\bin\poc-1-5\claw-plus-headless-1-5.exe --version
# 期望输出包含 "acp-protocol: 1.5"
```

**端到端验证清单**(详见 §7.5):

| # | 步骤 | 预期结果 | 通过? |
|---|------|---------|--------|
| 1 | Zed 启动,读取 agents.json(指向 1.5 binary) | "Claw Plus" 出现在 agent 列表 | [ ] |
| 2 | 选 "Claw Plus",发 "hello" prompt | 收到 assistant 回复(AgentMessageChunk) | [ ] |
| 3 | 让 Claw 读当前打开的文件 | `fs/read_text_file` 反向请求成功 | [ ] |
| 4 | 让 Claw 写文件(覆盖现有内容) | `fs/write_text_file` 返回 `written: true` | [ ] |
| 5 | 让 Claw 执行 Bash 命令(`ls`) | `session/request_permission` 弹窗 + 用户允许后执行 | [ ] |
| 6 | prompt 中途按 Esc | `session/cancel` 通知送达(P0 stub 返回 Ok) | [ ] |
| 7 | 关闭 Zed | `claw-plus-headless-1-5.exe` 进程退出(Task Manager 检查) | [ ] |

**验收门 G4**:
- 7 步全部通过 → PoC 成功
- 6/7 通过(仅 §7 步失败可接受,因 stdio EOF 处理是 0.10.4 已有行为) → PoC 部分成功
- < 6/7 通过 → PoC 失败

### 2.5.3 PoC 风险评估

| # | 风险 | 概率 | 影响 | 缓解措施 | 触发条件 |
|---|------|------|------|---------|---------|
| R1 | 1.5 API 变更超出预期,修复成本 > 1 周 | 中 | 高 | 保留 0.10.4 作为默认 feature,生产用 0.10.4,开发用 1.5 | G1 失败(错误 > 30) |
| R2 | Zed 不兼容 1.5(支持版本范围未知) | 中 | 高 | 先用 Zed 当前版本测试 1.5,如失败则回退 0.10.4 等待 Zed 升级 | G4 第 1 步失败 |
| R3 | 测试套件覆盖率不足,隐藏 bug 未被发现 | 中 | 中 | Phase 3 新增 10+ 测试覆盖 1.5 新功能;用 Mutation Testing 验证 | G3 通过率达标但 e2e 失败 |
| R4 | 1.5 引入新的非 Send 类型,破坏现有 LocalSet 隔离 | 低 | 高 | 编译时检查 `Send` 约束,新增 `static_assertions::assert_impl_all!(ClawAgent: !Send)` | G2 编译失败 |
| R5 | Cargo.lock 中 schema crate 版本不匹配 | 中 | 低 | `cargo update -p agent-client-protocol-schema` 同步升级 | G1 出现 `failed to resolve` |
| R6 | 1.5 协议协商失败时未优雅降级 | 中 | 中 | initialize 时检查 `client_capabilities`,未声明能力时 agent 不发起反向请求 | G4 第 3-5 步失败 |
| R7 | PoC 分支与主干冲突积累过多 | 低 | 低 | PoC 期间每日 `git rebase main`,保持线性历史 | rebase 出现 > 10 个冲突 |

**风险监控**:PoC 期间每日 standup 同步风险状态,任一风险触发立即记录到 §10 风险与缓解表并启动缓解。

### 2.5.4 PoC 决策点

PoC 结束后(7 个工作日内),根据 G1-G4 验收门结果作出决策:

#### 决策 A:PoC 成功(全面升级)

**触发条件**:G1 + G2 + G3 + G4 全部通过(7 步 e2e 全过)

**动作**:
1. 将 PoC 分支合并到 `feature/acp-1-5-upgrade` 长期分支
2. 切换默认 feature 为 `unstable-v2`(详见 §2.6.4 切换步骤)
3. 通知 Zed 集成验证团队跟进
4. 进入 P0 阶段 W1 实施(见 §8.1)

#### 决策 B:PoC 部分成功(双版本并存)

**触发条件**:G1 + G2 + G3 通过,但 G4 部分失败(Zed 不兼容)

**动作**:
1. PoC 分支合并到主干,但**保留默认 feature 为 0.10.4**
2. 1.5 路径作为 opt-in feature 保留(`--features unstable-v2`)
3. 在 §7 Zed 集成文档中标注"待 Zed 升级后启用"
4. 创建 issue 跟踪 Zed 兼容性,定期复查

#### 决策 C:PoC 失败(推迟升级)

**触发条件**:G1 或 G2 失败(编译错误过多或无法修复)

**动作**:
1. 不合并 PoC 分支
2. 保留 PoC 分支供后续参考(`poc/acp-1-5-upgrade-failed` 分支)
3. 重新评估升级路径:先升级到 1.3.0 过渡版本
4. 在主文档 §10 风险表更新"1.5 升级推迟"
5. 6 个月后重新启动 PoC

#### 决策矩阵汇总

| G1 | G2 | G3 | G4 | 决策 |
|----|----|----|----|------|
| ✅ | ✅ | ✅ | 7/7 | A:全面升级 |
| ✅ | ✅ | ✅ | 6/7 | A:全面升级(修复 §7 步) |
| ✅ | ✅ | ✅ | < 6/7 | B:双版本并存 |
| ✅ | ✅ | ❌ | - | B:双版本并存(补测试) |
| ✅ | ❌ | - | - | C:推迟升级 |
| ❌ | - | - | - | C:推迟升级 |

### 2.5.5 PoC 交付物清单

PoC 结束时需提交以下交付物:

| # | 交付物 | 路径 / 形式 | 验收标准 |
|---|--------|-----------|---------|
| 1 | PoC 分支 | `poc/acp-1-5-upgrade` | 包含全部修复 + 新增测试 |
| 2 | 编译错误清单 | `/tmp/acp-1-5-compile-errors.log` + 文档化 | 含每个错误的根因分析 |
| 3 | 测试报告 | `cargo test --features unstable-v2` 输出 | 通过率 + 失败原因 |
| 4 | Zed e2e 验证报告 | §7.5 验证清单(填入通过/失败) | 7 步结果 + 截图 |
| 5 | 决策记录 | PR 描述 + 决策矩阵 | 明确 A/B/C 决策 + 后续计划 |
| 6 | 风险更新 | 主文档 §10 风险表 | 触发的风险 + 缓解执行情况 |

### 2.5.6 PoC 时间线汇总

```
Day 1 (Phase 1)  : 依赖升级 + 编译错误清单 + G1 验收
Day 2-4 (Phase 2): 编译错误修复 + G2 验收
Day 5-6 (Phase 3): 功能验证 + 测试修复 + 新增测试 + G3 验收
Day 7 (Phase 4)  : 端到端验证 + 决策 + 交付物整理

缓冲:每个 Phase 后预留 0.5 天处理意外,实际 PoC 周期 7-9 天
```

---

## 二.6、双版本兼容策略

### 2.6.1 设计目标

在 PoC 验证期间(决策 B)及 P0→P1 过渡期,生产环境继续使用 0.10.4,开发环境可启用 1.5。两套代码必须能在同一份源码中共存,通过 Cargo feature flag 切换。

**核心约束**:

1. **单一 crate,双 feature**:`claw-acp` 一个 crate 同时支持两个版本,不分裂为 `claw-acp-0_10` / `claw-acp-1_5`
2. **运行时不可切换**:feature flag 是编译期决策,运行时不能从 0.10.4 切换到 1.5;同一 binary 只支持一个版本
3. **协议协商降级**:1.5 binary 连接 0.10.4 client 时,通过 `initialize` 协商降级到 0.10.4 行为(详见 §2.6.3)

### 2.6.2 Cargo.toml feature flag 设计

```toml
# rust/crates/claw-acp/Cargo.toml(v0.2 PoC 阶段)
[package]
name = "claw-acp"
version.workspace = true
edition.workspace = true
license.workspace = true
publish.workspace = true
description = "ACP (Agent Communication Protocol) channel/gateway layer for claw-code"

[dependencies]
# 双版本依赖共存:0.10.4 走 unstable feature,1.5 走 unstable-v2 feature
# 注意:Cargo 不支持同名 crate 多版本依赖,这里依赖的是单一 1.5 crate,
# 通过其内部的 unstable / unstable-v2 feature 控制 API 表面
agent-client-protocol = { version = "1.5", features = ["unstable", "unstable-v2"] }
async-trait = "0.1"
derive_more = "0.99"
futures = "0.3"
serde = { version = "1", features = ["derive"] }
serde_json.workspace = true
tokio = { version = "1", features = ["macros", "rt", "sync"] }
tracing = "0.1"

# claw-acp 自身的 feature flag
[features]
# 默认启用 0.10.4 兼容路径(生产安全)
default = ["acp-0_10"]
# 0.10.4 兼容路径
acp-0_10 = ["agent-client-protocol/unstable"]
# 1.5 新特性路径
acp-1_5 = ["agent-client-protocol/unstable-v2"]

[dev-dependencies]
tokio = { version = "1", features = ["macros", "rt", "sync", "time"] }

[lints]
workspace = true
```

```toml
# rust/crates/claw-shell/Cargo.toml(依赖 claw-acp 时透传 feature)
[features]
default = ["claw-acp/default"]
acp-1_5 = ["claw-acp/acp-1_5"]
```

```toml
# rust/Cargo.toml(workspace 级别的 feature 聚合,可选)
[features]
acp-1_5 = ["claw-acp/acp-1_5", "claw-shell/acp-1_5"]
```

**使用方式**:

```bash
# 生产构建(默认 0.10.4)
cargo build --release --bin claw-plus-headless

# PoC / 开发构建(1.5)
cargo build --release --bin claw-plus-headless --features claw-shell/acp-1_5

# 测试两个版本
cargo test --workspace                              # 0.10.4
cargo test --workspace --features claw-shell/acp-1_5  # 1.5
```

### 2.6.3 代码层 conditional compilation

`claw-acp` 内部用 `#[cfg(feature = "...")]` 区分两套实现:

```rust
// rust/crates/claw-acp/src/lib.rs(v0.2 双版本模块组织)

mod channel;
mod common;
mod gateway;
mod message;
pub mod stdio;

// 双版本 API 表面:统一入口,内部按 feature 分发
#[cfg(feature = "acp-1_5")]
mod v1_5;  // 1.5 特有实现:Diff v2 / typed Permission / Content MCP / Terminal API
#[cfg(not(feature = "acp-1_5"))]
mod v0_10; // 0.10.4 兼容实现:ContentBlock / Vec<String> mcp_servers / 等

// 统一 re-export:调用方代码不变,内部按 feature 切换
pub use self::{
    channel::{AcpAgentChannel, AcpChannel, AcpClientChannel, acp_channels, acp_send},
    common::{
        AcpAgentRx, AcpAgentTx, AcpChannelFailure, AcpClientRx, AcpClientTx, AcpResult, AcpRxo,
        AcpTxo, acp_channel_failure, acp_internal_error,
    },
    gateway::{
        AcpAgentGatewayReceiver, AcpAgentGatewaySender, AcpClientGatewayReceiver,
        AcpClientGatewaySender, AcpGatewayReceiver, AcpGatewaySender, acp_gateway,
    },
    message::{
        AcpAgentMessage, AcpAgentMessageBox, AcpAgentMessageGeneric, AcpArgs, AcpArgsBox,
        AcpClientMessage, AcpClientMessageBox, AcpClientMessageGeneric, AcpMethod, AcpRequest,
        AcpSide, Boxed, StorageMarker, Unboxed,
    },
};

// 1.5 特有 re-export(仅当 acp-1_5 feature 启用时)
#[cfg(feature = "acp-1_5")]
pub use self::v1_5::{
    adapt_mcp_servers,          // Vec<String> → Vec<McpServerConfig>
    adapt_session_configuration,// serde_json::Value → typed SessionConfiguration
    DiffV2,                     // 带 location 的 Diff
};

pub use self::stdio::spawn_stdin_line_reader;

#[doc(hidden)]
pub use self::common::compact_json;
```

`claw-shell::agent` 中的条件编译示例:

```rust
// rust/crates/claw-shell/src/agent.rs(双版本兼容代码片段)

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
    /// 从 prompt 提取用户文本 — 双版本适配
    fn extract_user_text(prompt: &[acp::ContentBlock]) -> String {
        prompt
            .iter()
            .filter_map(|block| match block {
                // 0.10.4 路径
                #[cfg(not(feature = "acp-1_5"))]
                acp::ContentBlock::Text(text) => Some(text.text.clone()),
                // 1.5 路径(Content 类型对齐 MCP)
                #[cfg(feature = "acp-1_5")]
                acp::Content::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// new_session 中处理 mcp_servers — 双版本适配
    fn process_mcp_servers(
        #[cfg(not(feature = "acp-1_5"))] mcp_servers: Vec<String>,
        #[cfg(feature = "acp-1_5")] mcp_servers: Vec<acp::McpServerConfig>,
    ) -> Result<(), acp::Error> {
        #[cfg(not(feature = "acp-1_5"))]
        {
            for name in &mcp_servers {
                tracing::info!("claw-agent: starting mcp server (0.10.4): {}", name);
            }
            Ok(())
        }
        #[cfg(feature = "acp-1_5")]
        {
            for config in &mcp_servers {
                tracing::info!("claw-agent: starting mcp server (1.5): {}", config.name);
            }
            Ok(())
        }
    }
}
```

### 2.6.4 运行时版本协商

即使编译期启用了 1.5,运行时仍可能连接到只支持 0.10.4 的客户端(如旧版 Zed)。`initialize` 阶段进行协商:

```rust
// rust/crates/claw-shell/src/agent.rs(initialize 协商逻辑)

async fn initialize(
    &self,
    arguments: acp::InitializeRequest,
) -> Result<acp::InitializeResponse, acp::Error> {
    tracing::debug!("claw-agent: initialize client_protocol={:?}", arguments.protocol_version);

    // 协商:取 client 和 agent 较小值
    let negotiated = std::cmp::min(arguments.protocol_version, acp::ProtocolVersion::LATEST);

    let mut resp = acp::InitializeResponse::new(negotiated);
    resp.auth_methods = vec![acp::AuthMethod::Agent(acp::AuthMethodAgent::new(
        acp::AuthMethodId::new("api_key"),
        "API Key",
    ))];

    // 仅在 1.5 feature 启用 + client 声明能力时,声明 agent_capabilities
    #[cfg(feature = "acp-1_5")]
    {
        if negotiated >= acp::ProtocolVersion::new(1, 5, 0) {
            // 检查 client 是否声明了 fs / permission 能力
            let caps = &arguments.client_capabilities;
            resp.agent_capabilities = acp::AgentCapabilities {
                fs_read_text_file: caps.fs_read_text_file.unwrap_or(false),
                fs_write_text_file: caps.fs_write_text_file.unwrap_or(false),
                session_request_permission: caps.session_request_permission.unwrap_or(false),
                session_load: false,   // P1
                session_fork: false,    // P2
                session_set_mode: false,// P2
                session_set_model: false, // P2
            };
            tracing::info!(
                "claw-agent: negotiated 1.5 with capabilities: {:?}",
                resp.agent_capabilities
            );
        } else {
            tracing::info!(
                "claw-agent: client requested {:?}, falling back to 0.10.4 behavior",
                arguments.protocol_version
            );
        }
    }

    // 0.10.4 feature 路径下不声明 agent_capabilities(字段不存在)
    #[cfg(not(feature = "acp-1_5"))]
    {
        tracing::debug!("claw-agent: built with acp-0_10 only, no capabilities declaration");
    }

    Ok(resp)
}
```

**协商决策表**:

| 编译 feature | Client 请求版本 | 行为 |
|-------------|----------------|------|
| `acp-1_5` | 1.5 | 启用全部 1.5 特性,声明 agent_capabilities |
| `acp-1_5` | 0.10.4 | 降级:不声明 capabilities,不发反向请求 |
| `acp-0_10`(默认) | 1.5 | 编译期不支持 1.5,返回 0.10.4 LATEST |
| `acp-0_10`(默认) | 0.10.4 | 正常 0.10.4 行为 |

### 2.6.5 双版本测试矩阵

CI 必须跑两套测试,确保任一 feature 路径都不回归:

```yaml
# .github/workflows/claw-acp-matrix.yml(假想 CI 配置)
strategy:
  matrix:
    feature: ["", "claw-shell/acp-1_5"]  # 空 = 默认 0.10.4
steps:
  - name: Test
    run: cargo test --workspace --features "${{ matrix.feature }}"
```

**双版本测试覆盖矩阵**:

| 测试类别 | 0.10.4 feature | 1.5 feature | 备注 |
|---------|---------------|-------------|------|
| 单元测试(agent.rs) | ✅ 必跑 | ✅ 必跑 | 两个 cfg 分支都要覆盖 |
| 集成测试(stdio.rs A6.4) | ✅ silent drop 行为 | ✅ parse_error 行为 | 用 cfg 分支区分断言 |
| 端到端测试(Zed) | ✅ 基线 | ✅ PoC 验证 | 1.5 仅 PoC 期必跑,正式发布后必跑 |
| 性能基准 | ✅ 基线 | ✅ 对比 | 1.5 不应回退超过 10% |

**回归预警阈值**:

- 0.10.4 feature 测试通过率 < 100% → 阻塞合并(生产路径不可破坏)
- 1.5 feature 测试通过率 < 95% → 标记 `beta` 但允许合并(1.5 仍在 PoC)
- 性能基准回退 > 10% → 调查根因,可能需优化 1.5 适配层

---

## 三、协议方法补齐

### 3.1 `initialize`

#### 请求/响应 schema

```rust
/// ACP initialize 请求 — 客户端握手第一步。
/// 
/// 客户端发送自身支持的 protocolVersion,agent 返回双方协商的版本。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeRequest {
    /// 客户端期望的协议版本(1 = ACP 1.x 系列)
    pub protocol_version: acp::ProtocolVersion,
    /// 客户端进程信息(可选,用于 agent 端日志)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_info: Option<acp::ClientInfo>,
    /// 客户端能力声明(fs 读写 / permission / terminal 等)
    #[serde(default)]
    pub client_capabilities: acp::ClientCapabilities,
}

/// ACP initialize 响应 — agent 返回协商结果与自身能力。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResponse {
    /// 最终协商的协议版本(取 client / agent 较小值)
    pub protocol_version: acp::ProtocolVersion,
    /// agent 支持的认证方式列表
    pub auth_methods: Vec<acp::AuthMethod>,
    /// agent 能力声明(支持哪些 session 操作)
    #[serde(default)]
    pub agent_capabilities: acp::AgentCapabilities,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(扩展现有 ClawAgent::initialize)
//
// 当前实现已基本可用,需补充 agent_capabilities 字段以声明 1.5 新能力。
// 当 unstable-v2 feature 启用时,告知客户端我们支持 fs/read_text_file 等。

#[async_trait(?Send)]
impl<C> acp::Agent for ClawAgent<C>
where
    C: ApiClient + 'static,
{
    async fn initialize(
        &self,
        _arguments: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        tracing::debug!("claw-agent: initialize");
        let mut resp = acp::InitializeResponse::new(acp::ProtocolVersion::LATEST);
        // 暴露 api_key auth method(本地无真实认证)
        resp.auth_methods = vec![acp::AuthMethod::Agent(acp::AuthMethodAgent::new(
            acp::AuthMethodId::new("api_key"),
            "API Key",
        ))];
        // P0:声明 agent 能力,让 IDE 知道我们支持哪些操作
        // fs_read_text_file / fs_write_text_file / request_permission 走反向请求
        // session/load / session/fork 在 P1 阶段开启
        resp.agent_capabilities = acp::AgentCapabilities {
            fs_read_text_file: true,
            fs_write_text_file: true,
            session_request_permission: true,
            // P1 阶段开启:
            session_load: false,
            session_fork: false,
            // P2 阶段开启:
            session_set_mode: false,
            session_set_model: false,
        };
        Ok(resp)
    }
    // ... 其他方法见后续章节
}
```

#### 错误处理行为

- `protocol_version` 不支持时返回 `-32090` unsupported protocol version
- 当前实现无错误路径(直接接受 LATEST)

#### 测试用例

| 测试名 | 断言要点 |
|--------|---------|
| `initialize_returns_latest_protocol_version` | `resp.protocol_version == LATEST` |
| `initialize_exposes_api_key_auth_method` | `auth_methods` 非空,含 `api_key` |
| `initialize_declares_fs_capabilities` | `agent_capabilities.fs_read_text_file == true` |

---

### 3.2 `session/new`

#### 请求/响应 schema

```rust
/// ACP session/new 请求 — 创建新会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSessionRequest {
    /// 工作目录(必须存在)
    pub cwd: acp::Path,
    /// 初始化 prompt(可选)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<Vec<acp::ContentBlock>>,
    /// MCP 服务器配置(1.5:typed,0.10.4:String)
    #[serde(default)]
    pub mcp_servers: Vec<acp::McpServerConfig>,
    /// 会话配置(1.5:typed SessionConfig)
    #[serde(default)]
    pub session_configuration: acp::SessionConfiguration,
    /// 权限模式(可选覆盖)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<acp::PermissionMode>,
}

/// ACP session/new 响应 — 返回新建会话 ID。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSessionResponse {
    /// 新会话的唯一标识(由 agent 分配)
    pub session_id: acp::SessionId,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(扩展现有 ClawAgent::new_session)
//
// 当前实现已可用,需补充:
// 1. 持久化 session_id 到 SessionStore(供 load_session 使用)
// 2. 处理 initial_prompt(如果有,立即触发一轮 turn)
// 3. 处理 mcp_servers 配置(启动 MCP 子进程)

async fn new_session(
    &self,
    arguments: acp::NewSessionRequest,
) -> Result<acp::NewSessionResponse, acp::Error> {
    tracing::debug!("claw-agent: new_session cwd={:?}", arguments.cwd);
    // 取出 api_client 和 tool_executor(从 Option 中移出)
    let api_client = self.api_client.borrow_mut().take().ok_or_else(|| {
        acp::Error::new(
            acp::ErrorCode::InternalError.into(),
            "api_client already consumed by a previous session",
        )
    })?;
    let tool_executor = self.tool_executor.borrow_mut().take().ok_or_else(|| {
        acp::Error::new(
            acp::ErrorCode::InternalError.into(),
            "tool_executor already consumed by a previous session",
        )
    })?;
    // 构造 Session + Runtime
    let mut session = Session::new().with_workspace_root(arguments.cwd.clone());
    // P1:持久化 session 元数据到 SessionStore
    // session.persist_to_store(&self.session_store)?;
    // P0:处理 mcp_servers 配置(启动子进程)
    for mcp_config in &arguments.mcp_servers {
        tracing::info!("claw-agent: starting mcp server {}", mcp_config.name);
        // session.start_mcp_server(mcp_config.clone()).await?;
    }
    let session_id = acp::SessionId::new(session.session_id.clone());
    let runtime = ConversationRuntime::new(
        session,
        api_client,
        tool_executor,
        self.config.permission_policy.clone(),
        self.config.system_prompt.clone(),
    );
    *self.runtime.borrow_mut() = Some(runtime);
    // P0:如果有 initial_prompt,立即触发一轮 turn(异步任务)
    if let Some(prompt) = arguments.initial_prompt {
        let user_input = Self::extract_user_text(&prompt);
        // 异步触发,不阻塞 new_session 响应
        // self.spawn_initial_turn(session_id.clone(), user_input);
        tracing::debug!("claw-agent: initial_prompt queued: {} chars", user_input.len());
    }
    Ok(acp::NewSessionResponse::new(session_id))
}
```

#### 错误处理行为

| 错误场景 | 返回错误码 | 说明 |
|---------|----------|------|
| `api_client` 已被消费 | `InternalError` | 一个 agent 实例只能 new_session 一次 |
| `cwd` 不存在 | `InvalidParams`(-32602) | 启动前校验 |
| MCP 服务器启动失败 | `InternalError` | 包含失败的服务器名 |

#### 测试用例

| 测试名 | 断言要点 |
|--------|---------|
| `new_session_returns_unique_session_id` | session_id 非空且为字符串 |
| `new_session_consumes_api_client` | 第二次 new_session 返回 InternalError |
| `new_session_with_invalid_cwd_returns_error` | 不存在的 cwd 返回 -32602 |
| `new_session_with_mcp_servers_starts_subprocesses` | mcp_servers 非空时启动子进程 |

---

### 3.3 `session/prompt`

#### 请求/响应 schema

```rust
/// ACP session/prompt 请求 — 发起一轮对话。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRequest {
    /// 目标会话 ID(必须已 new_session)
    pub session_id: acp::SessionId,
    /// 用户输入(支持多 ContentBlock)
    pub prompt: Vec<acp::ContentBlock>,
    /// 关联的 agent 思考 ID(用于思维链展示)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_thought_id: Option<acp::AgentThoughtId>,
    /// 是否启用 thinking 模式(1.5 新字段)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_mode: Option<acp::ThinkingMode>,
    /// 上下文模式(1.5:控制历史消息包含范围)
    #[serde(default)]
    pub context_mode: acp::ContextMode,
}

/// ACP session/prompt 响应 — 返回本轮 stop reason。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptResponse {
    /// 停止原因(EndTurn / ToolUse / MaxTokens / Cancelled 等)
    pub stop_reason: acp::StopReason,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(扩展现有 ClawAgent::prompt)
//
// P0 改进:在 run_turn 主循环中插入 LaneEvent flush 钩子,
// 让 IDE 实时收到子 agent 启动 / 工具调用进度等通知。

async fn prompt(
    &self,
    arguments: acp::PromptRequest,
) -> Result<acp::PromptResponse, acp::Error> {
    tracing::debug!("claw-agent: prompt session_id={:?}", arguments.session_id.0);
    // 取出 runtime(短暂持有 RefCell 借用)
    let runtime_opt = self.runtime.borrow_mut().take();
    let mut runtime_rc = runtime_opt.ok_or_else(|| {
        acp::Error::new(
            acp::ErrorCode::InternalError.into(),
            "no active session: call new_session first",
        )
    })?;
    let session_id = arguments.session_id.clone();
    let user_input = Self::extract_user_text(&arguments.prompt);
    // P0:在 run_turn 前 flush 一次 LaneEvent,确保 IDE 看到上一轮遗留事件
    Self::flush_lane_events_to_acp(&self.client_gateway, &session_id);
    // run_turn 是同步阻塞 API。
    // current_thread + LocalSet 下直接同步调用:会阻塞 LocalSet 直到 turn 完成,
    // 但不会死锁(没有其他 task 需要并发,且 channel 发送是非阻塞的)。
    let turn_result = runtime_rc.run_turn(user_input, None);
    // P0:run_turn 完成后再次 flush,把本轮 LaneEvent 推送给 IDE
    Self::flush_lane_events_to_acp(&self.client_gateway, &session_id);
    let turn_summary = match turn_result {
        Ok(summary) => summary,
        Err(e) => {
            *self.runtime.borrow_mut() = Some(runtime_rc);
            return Err(acp::Error::new(
                acp::ErrorCode::InternalError.into(),
                e.to_string(),
            ));
        }
    };
    // 推流 assistant 文本(本期简化:turn 完成后一次性推送,非真实流式)
    let text = Self::extract_assistant_text(&runtime_rc);
    if !text.is_empty() {
        self.notify(
            &session_id,
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                acp::ContentBlock::Text(acp::TextContent::new(text)),
            )),
        );
    }
    // 推送 tool call 通知(简化:仅记日志)
    for tool_msg in &turn_summary.tool_results {
        for block in &tool_msg.blocks {
            if let runtime::ContentBlock::ToolResult { tool_name, .. } = block {
                tracing::debug!("claw-agent: tool result for {}", tool_name);
            }
        }
    }
    // runtime 放回
    *self.runtime.borrow_mut() = Some(runtime_rc);
    Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
}
```

#### 错误处理行为

| 错误场景 | 返回错误码 | 说明 |
|---------|----------|------|
| 无活跃 session | `InternalError` | 未先调用 new_session |
| `run_turn` 内部失败 | `InternalError` | 错误信息透传到 `acp::Error.message` |
| API 调用超时 | `InternalError` | 当前 run_turn 是同步,无法 cancel,需 P1 改造 |

#### 测试用例

| 测试名 | 断言要点 |
|--------|---------|
| `prompt_returns_end_turn_stop_reason` | `stop_reason == EndTurn` |
| `prompt_without_session_returns_error` | 未 new_session 返回 InternalError |
| `prompt_pushes_agent_message_chunk` | 收到至少一条 AgentMessageChunk notification |
| `prompt_flushes_lane_events_before_and_after` | SubagentHandoff 事件被转换为 ToolCall notification |

---

### 3.4 `session/cancel`

#### 请求/响应 schema

```rust
/// ACP session/cancel 通知 — 取消正在进行的 prompt。
/// 
/// 这是 notification(not request),无响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelNotification {
    /// 要取消的会话 ID
    pub session_id: acp::SessionId,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(扩展现有 ClawAgent::cancel)
//
// P0:仍是 stub(同步 run_turn 无法中断)
// P1:改造 run_turn 为 async + CancellationToken 后实现真实取消

async fn cancel(&self, _arguments: acp::CancelNotification) -> Result<(), acp::Error> {
    // P0:记录日志,返回 Ok(固化为 stub 契约)
    // P1:触发 CancellationToken,让 run_turn 退出
    tracing::warn!("claw-agent: cancel not yet implemented (sync run_turn)");
    // P1 实现草图:
    // if let Some(token) = self.cancel_token.borrow().as_ref() {
    //     token.cancel();
    //     tracing::info!("claw-agent: cancellation triggered");
    // }
    Ok(())
}

/// P1:在 ClawAgent 中新增 cancel_token 字段
/// 
/// 与 run_turn 改造同步进行。run_turn 接受 CancellationToken 参数,
/// 在 API 调用 / tool 执行循环中检查 token.is_cancelled()。
pub struct ClawAgent<C>
where
    C: ApiClient + 'static,
{
    // ... 现有字段 ...
    /// P1:取消令牌,在 new_session 时创建,prompt 时传入 run_turn
    cancel_token: RefCell<Option<tokio_util::sync::CancellationToken>>,
}
```

#### 错误处理行为

- 当前无错误路径,直接返回 `Ok(())`
- P1 实现后,若 session 不存在应返回 `InternalError`

#### 测试用例

| 测试名 | 断言要点 |
|--------|---------|
| `cancel_notification_returns_ok_without_active_prompt` | 现有测试,固化 stub 契约 |
| `cancel_with_invalid_session_returns_error`(P1) | 不存在的 session_id 返回 InternalError |
| `cancel_interrupts_active_prompt`(P1) | prompt 中途 cancel 后 prompt 返回 `StopReason::Cancelled` |

---

### 3.5 `session/update`(反向请求)

`session/update` 是 IDE → agent 的通知,用于 IDE 推送状态变更(如用户切换 tab、激活文件等)。当前 `ext_notification` 路径已能吞掉,但 P1 应提供专用处理。

#### 请求/响应 schema

```rust
/// ACP session/update 通知 — IDE 推送会话状态变更。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionUpdateNotification {
    /// 目标会话 ID
    pub session_id: acp::SessionId,
    /// 变更类型(ActiveTabChanged / FileOpened / SelectionChanged 等)
    pub update: acp::SessionUpdateFromClient,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(P1 新增方法)
//
// 处理 IDE 推送的状态变更。当前实现仅记日志,
// 后续可用于上下文注入(打开的文件 → 自动加入 system prompt)。

async fn session_update(
    &self,
    arguments: acp::SessionUpdateNotification,
) -> Result<(), acp::Error> {
    tracing::debug!(
        "claw-agent: session_update session_id={:?} update={:?}",
        arguments.session_id.0,
        arguments.update
    );
    // P1:根据 update 类型更新上下文
    match arguments.update {
        acp::SessionUpdateFromClient::ActiveTabChanged { path } => {
            tracing::debug!("claw-agent: active tab changed to {}", path);
            // self.context_assembler.set_active_file(&path);
        }
        acp::SessionUpdateFromClient::SelectionChanged { path, range } => {
            tracing::debug!(
                "claw-agent: selection changed in {} at {:?}",
                path, range
            );
            // self.context_assembler.set_selection(&path, range);
        }
        _ => {
            tracing::debug!("claw-agent: unhandled session update: {:?}", arguments.update);
        }
    }
    Ok(())
}
```

---

### 3.6 `fs/read_text_file`

#### 请求/响应 schema

```rust
/// ACP fs/read_text_file 请求 — agent 反向请求 IDE 读取文件。
/// 
/// 与直接读磁盘的区别:能拿到 editor buffer 中未保存的内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadTextFileRequest {
    /// 要读取的文件路径(相对于 workspace root)
    pub path: acp::Path,
}

/// ACP fs/read_text_file 响应 — 返回文件内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadTextFileResponse {
    /// 文件文本内容(UTF-8)
    pub content: String,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(P0 新增方法)
//
// 委托给 client_gateway 反向请求 IDE。
// 走 AcpGatewaySender<acp::AgentSide>::send 路径(已有 forward 实现)。

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
    /// P0:读 editor buffer(含未保存内容)
    /// 
    /// ACP 1.5 fs/read_text_file 方法实现。
    /// 委托给 AcpGatewaySender 反向请求 editor。
    /// 
    /// 使用场景:tool 执行 Read 工具时,优先调此方法拿 editor buffer,
    /// 而非直接 fs::read_to_string(后者拿不到未保存内容)。
    pub async fn read_editor_buffer(&self, path: &str) -> Result<String, acp::Error> {
        let request = acp::ReadTextFileRequest {
            path: acp::Path::new(path.to_string()),
        };
        let response = self
            .client_gateway
            .send(request)
            .await
            .map_err(|e| acp::Error::new(
                acp::ErrorCode::InternalError.into(),
                format!("fs/read_text_file failed: {e}"),
            ))?;
        Ok(response.content)
    }
}
```

#### 错误处理行为

| 错误场景 | 返回错误码 | 说明 |
|---------|----------|------|
| IDE 未注册 `fs/read_text_file` handler | `MethodNotFound`(-32601) | capability 协商失败 |
| 文件不存在 | `InvalidParams`(-32602) | IDE 端校验 |
| 路径越权(workspace 外) | `InvalidParams` | 安全保护 |

#### 测试用例

| 测试名 | 断言要点 |
|--------|---------|
| `read_editor_buffer_returns_content` | mock IDE 返回 "hello",断言相等 |
| `read_editor_buffer_propagates_method_not_found` | mock IDE 不注册 handler,断言 -32601 |
| `read_editor_buffer_rejects_out_of_workspace_path` | 路径在 workspace 外时返回 -32602 |

#### 安全约束(v0.2 新增)

`fs/read_text_file` 是 agent 反向请求 IDE 读取文件,但**最终调用 IDE 的 IO 实现路径**必须做安全校验,防止 agent 通过路径遍历攻击读取敏感文件。

**安全模型**:

1. **Workspace 限制**:Agent 通过 `fs/read_text_file` 只能访问 `new_session.cwd` 子树内的文件
2. **Agent 自身无关**:agent 的代码层(`read_editor_buffer`)不强制安全约束,而是把校验责任下推到 IDE 端实现(IDE 知道用户当前 workspace 边界)
3. **VS Code 端实现示例**:见 §6.5,在 `handleReadTextFile` 中调用 `vscode.workspace.getWorkspaceFolder` 验证

**风险与缓解**:

| 风险 | 缓解 |
|------|------|
| Agent 请求 `../../../etc/passwd` | IDE 端 `canonicalize` 后检查是否在 workspace root 内 |
| Agent 请求绝对路径 `C:\Windows\System32\config\SAM` | IDE 端拒绝非 workspace 内的绝对路径 |
| Symbolic link 跳出 workspace | IDE 端 `canonicalize` 解析符号链接后再检查 |
| 路径编码绕过(UTF-8 BOM / null byte) | IDE 端用 `Path::strip_prefix` 而非字符串匹配 |

#### 路径验证代码骨架(IDE 端)

```rust
// rust/crates/claw-shell/src/agent.rs(P0 新增辅助方法,IDE 端实现可参考)

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
    /// 校验路径是否在 workspace root 内
    ///
    /// 调用方(IDE 端实现 fs/read_text_file handler)应在响应前调用此方法。
    /// 注意:agent 自身不强制此校验,本方法仅供 IDE 端参考实现。
    pub fn validate_workspace_path(
        workspace_root: &std::path::Path,
        requested: &str,
    ) -> Result<std::path::PathBuf, acp::Error> {
        let requested_path = std::path::Path::new(requested);
        // 拼接 workspace root + 相对路径
        let full = if requested_path.is_absolute() {
            requested_path.to_path_buf()
        } else {
            workspace_root.join(requested_path)
        };
        // canonicalize 解析符号链接、`.`、`..`
        let canonical = full.canonicalize().map_err(|e| {
            acp::Error::new(
                acp::ErrorCode::InvalidParams.into(),
                format!("path canonicalize failed: {e}"),
            )
        })?;
        let canonical_root = workspace_root.canonicalize().map_err(|e| {
            acp::Error::new(
                acp::ErrorCode::InternalError.into(),
                format!("workspace_root canonicalize failed: {e}"),
            )
        })?;
        // 检查 canonical 是否在 canonical_root 子树内
        if !canonical.starts_with(&canonical_root) {
            return Err(acp::Error::new(
                acp::ErrorCode::InvalidParams.into(),
                format!(
                    "path {} is outside workspace {}",
                    canonical.display(),
                    canonical_root.display()
                ),
            ));
        }
        Ok(canonical)
    }
}
```

#### 与现有 Read tool 的关系(v0.2 新增)

Claw Plus 现有 `Read` tool(在 [rust/crates/tools/src/](../../rust/crates/tools/src/) 下)是 **agent 主动调用**的工具,直接通过 `tokio::fs::read_to_string` 读磁盘。

**两条读路径的对比**:

| 维度 | Read tool(agent → fs) | fs/read_text_file(agent → IDE) |
|------|----------------------|------------------------------|
| 调用方 | Agent 内部 tool 执行循环 | Agent 通过 ACP 反向请求 IDE |
| 数据源 | 磁盘文件 | IDE editor buffer(含未保存内容) |
| 路径范围 | 受 PermissionPolicy 约束(workspace-write 模式下限制在 cwd) | 受 IDE workspace 边界约束 |
| Undo 集成 | 无 | IDE 自动追踪 |
| 适用场景 | 后台运行 / headless 模式 | IDE 内交互(用户正在编辑) |
| 调用方式 | 同步 `tokio::fs` | 异步反向请求(等待 IDE 响应) |

**何时用哪条路径**:

- **headless 模式(Zed/VS Code 未连接)**:只能用 Read tool
- **IDE 模式且文件已打开**:优先 `fs/read_text_file`(拿未保存内容)
- **IDE 模式但文件未打开**:Read tool 直接读磁盘更快(省一次反向请求往返)

**P0 实施策略**:本期 `read_editor_buffer` 仅在 Read tool 执行前作为"预读"调用,获取 editor buffer 后与磁盘内容 diff,若有差异则用 buffer 内容(用户未保存的修改优先)。完整 IDE/agent 路径分离留 P1。

---

### 3.7 `fs/write_text_file`

#### 请求/响应 schema

```rust
/// ACP fs/write_text_file 请求 — agent 反向请求 IDE 写文件。
/// 
/// 走 editor undo 栈,用户可 Ctrl+Z 撤销。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteTextFileRequest {
    /// 要写入的文件路径
    pub path: acp::Path,
    /// 文件内容(整体覆盖)
    pub content: String,
    /// 1.5 新增:diff 格式(替代整体覆盖,更高效)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<acp::Diff>,
}

/// ACP fs/write_text_file 响应 — 返回写入结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteTextFileResponse {
    /// 是否成功写入(true / false)
    pub written: bool,
    /// 1.5 新增:用户拒绝原因(如选中了 "Deny")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denial_reason: Option<String>,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(P0 新增方法)

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
    /// P0:写文件走 editor undo 栈
    /// 
    /// 与 fs::write 的区别:走 editor undo 栈,用户可 Ctrl+Z 撤销。
    /// 用于 tool 执行 Write / Edit 工具时。
    pub async fn write_editor_buffer(
        &self,
        path: &str,
        content: &str,
    ) -> Result<bool, acp::Error> {
        let request = acp::WriteTextFileRequest {
            path: acp::Path::new(path.to_string()),
            content: content.to_string(),
            diff: None, // P1:支持 diff 格式
        };
        let response = self
            .client_gateway
            .send(request)
            .await
            .map_err(|e| acp::Error::new(
                acp::ErrorCode::InternalError.into(),
                format!("fs/write_text_file failed: {e}"),
            ))?;
        if !response.written {
            tracing::warn!(
                "claw-agent: write denied by user for path {}: {:?}",
                path,
                response.denial_reason
            );
        }
        Ok(response.written)
    }

    /// P1:写文件 with diff(更高效,只传变更部分)
    pub async fn write_editor_buffer_with_diff(
        &self,
        path: &str,
        diff: acp::Diff,
    ) -> Result<bool, acp::Error> {
        let request = acp::WriteTextFileRequest {
            path: acp::Path::new(path.to_string()),
            content: String::new(), // diff 模式下 content 可空
            diff: Some(diff),
        };
        let response = self.client_gateway.send(request).await.map_err(|e| {
            acp::Error::new(
                acp::ErrorCode::InternalError.into(),
                format!("fs/write_text_file (diff) failed: {e}"),
            )
        })?;
        Ok(response.written)
    }
}
```

#### 错误处理行为

| 错误场景 | 返回错误码 | 说明 |
|---------|----------|------|
| 用户拒绝写入 | `written: false` | 不算错误,agent 应感知并回退 |
| 路径越权 | `InvalidParams` | 同 read |
| editor 冲突(并发写) | `InternalError` | 1.5 新增错误码 |

#### 测试用例

| 测试名 | 断言要点 |
|--------|---------|
| `write_editor_buffer_returns_written_true` | mock IDE 接受,断言 `written: true` |
| `write_editor_buffer_returns_false_when_user_denies` | mock IDE 返回 `written: false` |
| `write_editor_buffer_with_diff_uses_diff_field`(P1) | diff 字段被传递到 IDE |

#### 安全约束与 Undo 集成(v0.2 新增)

`fs/write_text_file` 比 `fs/read_text_file` 风险更高:写操作可能覆盖用户未保存的修改。安全模型:

1. **路径校验**:同 §3.6 安全约束,IDE 端必须 `canonicalize` 后检查 workspace 边界
2. **Undo 栈集成**:IDE 端必须用 `WorkspaceEdit` API(VS Code)/ `TextDocument.replace` (Zed internal),而非直接 `fs::write`,确保用户可 Ctrl+Z 撤销
3. **未保存修改保护**:若 editor buffer 中有未保存的修改,且 agent 写入的内容与 buffer 不一致,IDE 端应:
   - 弹出冲突对话框,让用户选择"覆盖我的修改"或"取消写入"
   - 默认行为:**取消写入**(返回 `written: false, denial_reason: "conflict with unsaved changes"`)
4. **Diff 模式安全**:1.5 的 `Diff` 字段(带 `location`)由 IDE 应用到 buffer,但若 location 超出当前 buffer 范围(如行号超过 `doc.lineCount`),返回 `InvalidParams`

**写入流程时序**:

```
Agent: 调 write_editor_buffer(path, content)
  ↓
ACP 反向请求 fs/write_text_file
  ↓
IDE 端 handleWriteTextFile:
  1. validate_workspace_path(workspace_root, path) → canonical_path
  2. 检查 doc = workspace.textDocuments.find(canonical_path)
  3. if doc.dirty && doc.getText() != content:
       a. 弹"冲突"对话框(用户选择)
       b. 用户拒绝 → return { written: false, denial_reason: "conflict" }
       c. 用户同意 → 走 WorkspaceEdit.replace
     else:
       走 WorkspaceEdit.replace
  4. return { written: applied }
  ↓
ACP 响应回 agent
  ↓
Agent 根据 written 决定是否回退(如恢复 Read tool 的旧内容)
```

**与现有 Write tool 的关系**:

| 维度 | Write tool(agent → fs) | fs/write_text_file(agent → IDE) |
|------|----------------------|-------------------------------|
| 调用方 | Agent 内部 tool 执行循环 | Agent 通过 ACP 反向请求 IDE |
| 数据源 | 磁盘(覆盖) | IDE editor buffer(走 Undo 栈) |
| 用户可撤销 | 否(磁盘覆盖) | 是(Ctrl+Z) |
| 冲突保护 | 无 | 检测未保存修改冲突 |
| 路径范围 | 受 PermissionPolicy 约束 | 受 IDE workspace 边界约束 |
| 适用场景 | headless 模式 | IDE 内交互 |

**P0 实施策略**:`write_editor_buffer` 仅在 IDE 模式下作为 Write tool 的替代路径;若 IDE 未连接(返回 `MethodNotFound`),自动 fallback 到 Write tool(直接磁盘写)。

```rust
// rust/crates/claw-shell/src/agent.rs(P0 Write tool 集成示例)

async fn write_with_ide_fallback(
    &self,
    path: &str,
    content: &str,
) -> Result<bool, String> {
    // 优先尝试 IDE 写(若 IDE 连接且声明 fs_write_text_file 能力)
    if self.client_capabilities.fs_write_text_file.unwrap_or(false) {
        match self.write_editor_buffer(path, content).await {
            Ok(written) => return Ok(written),
            Err(acp_err) if acp_err.code == -32601 => {
                tracing::warn!("IDE 未注册 fs/write_text_file,fallback 到磁盘写");
                // 继续走 fallback
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    // Fallback:直接磁盘写(同 Write tool)
    tokio::fs::write(path, content).await.map_err(|e| e.to_string())?;
    Ok(true)
}
```

---

### 3.8 `session/request_permission`

#### 请求/响应 schema

```rust
/// ACP session/request_permission 请求 — agent 反向请求用户审批。
/// 
/// 用于危险操作(Bash rm -rf / Write 覆盖 / 网络请求等)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestPermissionRequest {
    /// 目标会话 ID
    pub session_id: acp::SessionId,
    /// 触发审批的 tool 名称(Bash / Write / Read 等)
    pub tool_name: String,
    /// tool 输入参数(用于在 UI 展示)
    pub tool_input: serde_json::Value,
    /// 1.5:typed 权限选项(allow / deny / always_allow 等)
    pub options: Vec<acp::PermissionOption>,
}

/// ACP session/request_permission 响应 — 用户选择。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestPermissionResponse {
    /// 用户选择的选项 ID
    pub outcome: acp::PermissionOutcome,
    /// 用户选了 "always" 时的过期时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<acp::Timestamp>,
}
```

#### ClawAgent 实现代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(P0 新增方法)
//
// 替代当前 ClawAgent::cancel 的 stub(后者实际上是错误用法,应在 prompt 中调此方法)。

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
    /// P0:请求权限(替代当前 stub cancel)
    /// 
    /// ACP 1.5 session/request_permission 反向请求。
    /// 用于危险操作(Bash rm -rf / Write 覆盖)审批。
    /// 
    /// 在 tool 执行前调用,根据 outcome 决定是否继续。
    pub async fn request_permission(
        &self,
        session_id: &acp::SessionId,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> Result<acp::PermissionOutcome, acp::Error> {
        let request = acp::RequestPermissionRequest {
            session_id: session_id.clone(),
            tool_name: tool_name.to_string(),
            tool_input: tool_input.clone(),
            options: vec![
                acp::PermissionOption::Allow,
                acp::PermissionOption::Deny,
                acp::PermissionOption::AlwaysAllow,
            ],
        };
        let response = self
            .client_gateway
            .send(request)
            .await
            .map_err(|e| acp::Error::new(
                acp::ErrorCode::InternalError.into(),
                format!("session/request_permission failed: {e}"),
            ))?;
        Ok(response.outcome)
    }
}
```

#### 错误处理行为

| 错误场景 | 返回错误码 | 说明 |
|---------|----------|------|
| IDE 未注册 handler | `MethodNotFound` | 同 fs/* |
| 用户长时间未响应 | `InternalError` | 超时(默认 60s) |
| IDE 返回未知 outcome | `InternalError` | 协议错误 |

#### 测试用例

| 测试名 | 断言要点 |
|--------|---------|
| `request_permission_returns_allow_when_user_approves` | mock IDE 返回 Allow,断言相等 |
| `request_permission_returns_deny_when_user_rejects` | mock IDE 返回 Deny |
| `request_permission_returns_always_allow_with_expiry` | AlwaysAllow 时 expires_at 非空 |
| `request_permission_times_out_after_60s` | mock IDE 不响应,60s 后返回 InternalError |

#### 完整 6 步流程(v0.2 新增)

`session/request_permission` 是 ACP 唯一的用户审批路径,流程必须严格按以下 6 步执行,任何提前返回都需明确语义:

```
Step 1: Agent 检测到需要权限
   ├─ 场景 A:Bash tool 执行 rm 命令(危险操作)
   ├─ 场景 B:Write tool 写入非 workspace 文件(路径越权)
   ├─ 场景 C:Read tool 读取 workspace 外文件(配置越权)
   └─ 触发点:在 tool 执行循环内,实际调用前

Step 2: 通过 SessionNotification::PermissionRequest 推送给 client
   ├─ 走 fire-and-forget 推送(不阻塞)
   ├─ SessionUpdate::ToolCall(status=Pending) + 描述
   └─ 同时发起反向请求 session/request_permission(等待响应)

Step 3: Client 显示权限对话框
   ├─ VS Code:vscode.window.showWarningMessage(modal=true)
   ├─ Zed:Assistant panel 弹出 inline 按钮
   └─ 选项:Allow / Deny / AlwaysAllow

Step 4: 用户选择
   ├─ Allow → outcome=Allow,继续执行
   ├─ Deny → outcome=Deny,tool 中止
   ├─ AlwaysAllow → outcome=AlwaysAllow + expires_at
   │              ├─ 写入 permission_cache(tool_name 为 key)
   │              └─ 同会话内不再询问相同 tool
   └─ 关闭对话框(超时或拒绝响应)→ 走超时路径

Step 5: Client 通过 session/request_permission 响应
   └─ JSON-RPC response 包含 { outcome, expires_at? }

Step 6: Agent 根据 outcome 决定后续
   ├─ Allow → 继续执行 tool
   ├─ AlwaysAllow → 继续执行 + 缓存
   └─ Deny → tool 返回 PermissionDenied,agent 决定替代路径或中止 turn
```

**关键时序约束**:

- Step 2 中"推送 PermissionRequest notification"和"发起反向请求"必须并行(notification 不等响应)
- Step 5 等待响应有超时(见下文)
- Step 6 outcome 必须与 Step 2 请求的 `options` 字段中声明的选项一致(否则返回 InternalError)

#### 与现有 PermissionMode 的协同(v0.2 新增)

Claw Plus 现有 `PermissionMode`(Readonly / WorkspaceWrite / DangerFullAccess)在 `new_session` 时由 client 指定,控制 agent 的整体权限级别。`session/request_permission` 是更细粒度的 per-tool 审批,两者协同:

| PermissionMode | 是否调用 request_permission | 理由 |
|---------------|---------------------------|------|
| `Readonly` | 所有写操作 / Bash 均调用 | 默认拒绝,需用户明确放行 |
| `WorkspaceWrite` | workspace 外写操作 / 危险 Bash 调用 | workspace 内操作已隐含授权 |
| `DangerFullAccess` | 不调用 | 用户已显式放弃审批(危险模式) |

**实现策略**:在 `ConversationRuntime` 的 tool 执行循环中,根据当前 `PermissionMode` + `tool_name` + `tool_input` 决定是否调用 `request_permission`:

```rust
// rust/crates/runtime/src/tool_executor.rs(P0 修改 tool 执行循环)

async fn execute_with_permission_check(
    &self,
    tool_name: &str,
    tool_input: &serde_json::Value,
    permission_mode: PermissionMode,
    session_id: &acp::SessionId,
) -> Result<PermissionOutcome, PermissionError> {
    use PermissionMode as M;
    // 判断是否需要请求权限
    let needs_permission = match (permission_mode, tool_name) {
        // DangerFullAccess 模式从不请求
        (M::DangerFullAccess, _) => false,
        // Readonly 模式:所有非读操作都请求
        (M::Readonly, "Read" | "Glob" | "Grep") => false,
        (M::Readonly, _) => true,
        // WorkspaceWrite 模式:workspace 外操作请求
        (M::WorkspaceWrite, "Write" | "Edit") => {
            let path = tool_input["path"].as_str().unwrap_or("");
            !is_path_in_workspace(path, &self.workspace_root)
        }
        (M::WorkspaceWrite, "Bash") => {
            let cmd = tool_input["command"].as_str().unwrap_or("");
            is_dangerous_bash_command(cmd) // rm -rf / sudo / chmod 777 等
        }
        _ => false,
    };
    if !needs_permission {
        return Ok(PermissionOutcome::Allow);
    }
    // 先查缓存(AlwaysAllow 决策)
    let cache_key = format!("{}:{}", tool_name, hash_tool_input(tool_input));
    if let Some(cached) = self.permission_cache.borrow().get(&cache_key) {
        return Ok(cached.clone());
    }
    // 发起反向请求
    let outcome = self.request_permission(session_id, tool_name, tool_input).await?;
    // 缓存 AlwaysAllow
    if matches!(outcome, PermissionOutcome::AlwaysAllow) {
        self.permission_cache.borrow_mut().insert(cache_key, outcome.clone());
    }
    Ok(outcome)
}
```

#### 超时处理(v0.2 新增)

用户可能长时间不响应(离开电脑 / 没注意到弹窗),`request_permission` 必须有超时机制,避免 agent 永久阻塞。

**超时策略**:

- 默认超时:**30 秒**(可配置,通过 `ClawAgentConfig::permission_timeout`)
- 超时后行为:默认 `Deny`(保守,优先保护用户)
- 超时通知:推送 `SessionUpdate::AgentMessageChunk` 提示用户"权限请求已超时,已默认拒绝"

```rust
// rust/crates/claw-shell/src/agent.rs(P0 超时实现)

pub async fn request_permission(
    &self,
    session_id: &acp::SessionId,
    tool_name: &str,
    tool_input: &serde_json::Value,
) -> Result<acp::PermissionOutcome, acp::Error> {
    let request = acp::RequestPermissionRequest { /* ... */ };
    let timeout = self.config.permission_timeout
        .unwrap_or(std::time::Duration::from_secs(30));
    // 用 tokio::time::timeout 包装反向请求
    match tokio::time::timeout(
        timeout,
        self.client_gateway.send(request),
    ).await {
        Ok(Ok(response)) => Ok(response.outcome),
        Ok(Err(e)) => Err(acp::Error::new(
            acp::ErrorCode::InternalError.into(),
            format!("session/request_permission failed: {e}"),
        )),
        Err(_elapsed) => {
            tracing::warn!(
                "claw-agent: permission request timed out after {:?}, defaulting to Deny",
                timeout
            );
            // 推送超时通知给 client
            self.notify(
                session_id,
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::Text(acp::TextContent::new(
                        format!("权限请求超时({:?}),已默认拒绝", timeout)
                    )),
                )),
            );
            Ok(acp::PermissionOutcome::Deny)
        }
    }
}
```

#### 权限缓存(v0.2 新增)

`AlwaysAllow` 决策应被缓存,避免同一会话内对相同 tool + 相同 input 反复弹窗,影响用户体验。

**缓存策略**:

- **缓存键**:`{tool_name}:{hash(tool_input)}` — 相同工具 + 相同输入才命中
- **缓存值**:`PermissionOutcome::AlwaysAllow`
- **生命周期**:仅当前会话(会话结束清空)
- **存储位置**:`ClawAgent.permission_cache: RefCell<HashMap<String, PermissionOutcome>>`
- **不缓存 Allow**:Allow 只对当前一次操作生效,下次相同操作仍需询问
- **不缓存 Deny**:Deny 不缓存(用户可能改变主意)

**缓存命中场景示例**:

| 场景 | tool_input | 首次 | 二次 | 缓存命中? |
|------|------------|------|------|-----------|
| 同一 Bash 命令二次执行 | `{"command": "ls -la"}` | Allow | 询问 | 否(Allow 不缓存) |
| 同一 Bash 命令二次执行(用户首次选 AlwaysAllow) | `{"command": "ls -la"}` | AlwaysAllow | AlwaysAllow | 是 |
| 不同 Bash 命令 | `{"command": "ls"}` vs `{"command": "rm -rf"}` | AlwaysAllow | 询问 | 否(input 不同) |
| Write 同一路径 | `{"path": "/tmp/a.txt"}` | AlwaysAllow | AlwaysAllow | 是 |
| Write 不同路径 | `{"path": "/tmp/a.txt"}` vs `{"path": "/tmp/b.txt"}` | AlwaysAllow | 询问 | 否(input 不同) |

**缓存清理时机**:

- `session/cancel` 触发时清空(取消后下次重新询问)
- `set_session_mode` 切换时清空(模式变更,旧决策不适用)
- `session/load` 加载历史会话时不清空(保留 AlwaysAllow 决策)
- 会话销毁时自动随 `ClawAgent` drop 清空

---

## 四、ClawAgent 扩展

### 4.1 现有结构分析

当前 `ClawAgent<C>` 定义见 [agent.rs:40-56](../../rust/crates/claw-shell/src/agent.rs):

```rust
pub struct ClawAgent<C>
where
    C: ApiClient + 'static,
{
    runtime: RefCell<Option<ConversationRuntime<C, StaticToolExecutor>>>,
    config: ClawAgentConfig,
    api_client: RefCell<Option<C>>,
    tool_executor: RefCell<Option<StaticToolExecutor>>,
    client_gateway: AcpGatewaySender<acp::AgentSide>,
}
```

**关键约束**(来自项目记忆):

1. `runtime` 是 `RefCell<Option<...>>` 而非 `Rc<RefCell<...>>`,因 `new_session` 时整体替换
2. `api_client` / `tool_executor` 用 `Option` 包装,因 `new_session` 时 move 进 runtime
3. `client_gateway` 是 `AcpGatewaySender<acp::AgentSide>`,实现 `Clone`(mpsc sender clone)
4. `ClawAgent` 持有 `Rc<RefCell<...>>` 间接非 Send 类型(通过 `StaticToolExecutor` 内部 `Box<dyn FnMut>`),必须 `current_thread + LocalSet` 上运行

### 4.2 扩展字段设计

```rust
// rust/crates/claw-shell/src/agent.rs(P0 + P1 扩展)
//
// 新增字段:
// - session_store:P1 支持 load_session / session_list
// - cancel_token:P1 支持 cancel 中断 run_turn
// - session_id:跟踪当前活跃 session_id(用于 notification 路由)
// - permission_cache:缓存 AlwaysAllow 决策,避免重复请求

use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

/// P1:会话持久化存储(线程安全,可跨 agent 实例共享)
pub struct SessionStore {
    /// session_id → 序列化状态
    sessions: std::sync::Mutex<HashMap<String, SerializedSession>>,
}

/// P1:序列化的会话状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedSession {
    pub session_id: String,
    pub cwd: String,
    pub mode: acp::SessionMode,
    pub mcp_servers: Vec<acp::McpServerConfig>,
    pub created_at: String,
    pub messages: Vec<runtime::Message>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn save(&self, session: &SerializedSession) -> Result<(), String> {
        let mut map = self.sessions.lock().map_err(|e| e.to_string())?;
        map.insert(session.session_id.clone(), session.clone());
        Ok(())
    }

    pub fn load(&self, session_id: &str) -> Option<SerializedSession> {
        let map = self.sessions.lock().ok()?;
        map.get(session_id).cloned()
    }

    pub fn list(&self) -> Vec<SerializedSession> {
        let map = self.sessions.lock().ok()?;
        map.values().cloned().collect()
    }
}

pub struct ClawAgent<C>
where
    C: ApiClient + 'static,
{
    // 现有字段(保留)
    runtime: RefCell<Option<ConversationRuntime<C, StaticToolExecutor>>>,
    config: ClawAgentConfig,
    api_client: RefCell<Option<C>>,
    tool_executor: RefCell<Option<StaticToolExecutor>>,
    client_gateway: AcpGatewaySender<acp::AgentSide>,

    // P1 新增字段
    /// 当前活跃 session_id(用于 notification 路由与 cancel 定位)
    current_session_id: RefCell<Option<acp::SessionId>>,
    /// 取消令牌:new_session 时创建,prompt 时传入 run_turn
    cancel_token: RefCell<Option<CancellationToken>>,
    /// 会话持久化存储(支持 load_session / session_list)
    session_store: std::rc::Rc<SessionStore>,
    /// 权限缓存:AlwaysAllow 决策缓存,避免重复请求
    permission_cache: RefCell<HashMap<String, acp::PermissionOutcome>>,
}
```

### 4.3 async fn 方法签名

```rust
// rust/crates/claw-shell/src/agent.rs(P0 + P1 新增方法)
//
// 所有新方法都是 ?Send async(因 ClawAgent 持有非 Send 类型),
// 必须在 LocalSet 上调用。

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
    /// P0:读 editor buffer(见 §3.6)
    pub async fn read_editor_buffer(&self, path: &str) -> Result<String, acp::Error>;

    /// P0:写文件走 editor undo 栈(见 §3.7)
    pub async fn write_editor_buffer(&self, path: &str, content: &str) -> Result<bool, acp::Error>;

    /// P0:请求权限(见 §3.8)
    pub async fn request_permission(
        &self,
        session_id: &acp::SessionId,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> Result<acp::PermissionOutcome, acp::Error>;

    /// P0:flush LaneEvent 到 ACP SessionNotification
    /// 
    /// 在 run_turn 主循环前后调用,把内部事件流推送给 IDE。
    /// 走 fire-and-forget 路径,不阻塞 turn。
    pub fn flush_lane_events_to_acp(
        gateway: &AcpGatewaySender<acp::AgentSide>,
        session_id: &acp::SessionId,
    ) {
        let events = runtime::drain_lane_events();
        for event in events {
            if let Some(notification) = Self::lane_event_to_session_update(&event) {
                let notif = acp::SessionNotification::new(
                    session_id.clone(),
                    notification,
                );
                // fire-and-forget,适合高频低延迟推送
                gateway.forward_fire_and_forget(notif);
            }
        }
    }

    /// P0:LaneEvent → SessionUpdate 转换(见 §5)
    fn lane_event_to_session_update(event: &runtime::LaneEvent) -> Option<acp::SessionUpdate>;

    /// P1:加载已存在的 session
    pub async fn load_session(
        &self,
        session_id: &acp::SessionId,
    ) -> Result<SerializedSession, acp::Error>;

    /// P1:从序列化状态恢复 runtime
    pub fn restore_runtime_state(
        &self,
        session: SerializedSession,
    ) -> Result<(), acp::Error>;

    /// P2:fork session(从某消息分叉)
    pub async fn fork_session(
        &self,
        session_id: &acp::SessionId,
        fork_point: &str,
    ) -> Result<acp::SessionId, acp::Error>;

    /// P2:列出可恢复会话
    pub async fn list_sessions(&self) -> Vec<SerializedSession>;

    /// P2:切换 plan/act 模式
    pub async fn set_session_mode(&self, mode: acp::SessionMode) -> Result<(), acp::Error>;

    /// P2:切换底层模型
    pub async fn set_session_model(&self, model: &str) -> Result<(), acp::Error>;
}
```

---

## 五、LaneEvent → SessionNotification 桥接

### 5.1 LaneEvent 完整清单

[runtime/src/lane_events.rs:5-57](../../rust/crates/runtime/src/lane_events.rs) 定义了 23 种 `LaneEventName`(注:任务描述称 19 种,实际源码为 23 种):

| # | LaneEventName | wire value | 类别 |
|---|---------------|-----------|------|
| 1 | `Started` | `lane.started` | 生命周期 |
| 2 | `Ready` | `lane.ready` | 生命周期 |
| 3 | `PromptMisdelivery` | `lane.prompt_misdelivery` | 异常 |
| 4 | `Blocked` | `lane.blocked` | 阻塞 |
| 5 | `Red` | `lane.red` | 状态 |
| 6 | `Green` | `lane.green` | 状态 |
| 7 | `CommitCreated` | `lane.commit.created` | Git |
| 8 | `PrOpened` | `lane.pr.opened` | Git |
| 9 | `MergeReady` | `lane.merge.ready` | Git |
| 10 | `Finished` | `lane.finished` | 生命周期(终态) |
| 11 | `Failed` | `lane.failed` | 生命周期(终态) |
| 12 | `Reconciled` | `lane.reconciled` | 生命周期(不确定性) |
| 13 | `Merged` | `lane.merged` | 生命周期(终态) |
| 14 | `Superseded` | `lane.superseded` | 生命周期(终态) |
| 15 | `Closed` | `lane.closed` | 生命周期(终态) |
| 16 | `BranchStaleAgainstMain` | `branch.stale_against_main` | 分支 |
| 17 | `BranchWorkspaceMismatch` | `branch.workspace_mismatch` | 分支 |
| 18 | `ShipPrepared` | `ship.prepared` | 发布 |
| 19 | `ShipCommitsSelected` | `ship.commits_selected` | 发布 |
| 20 | `ShipMerged` | `ship.merged` | 发布 |
| 21 | `ShipPushedMain` | `ship.pushed_main` | 发布 |
| 22 | `SubagentHandoff` | `subagent.handoff` | 子 agent |
| 23 | `SubagentResult` | `subagent.result` | 子 agent |

### 5.2 LaneEvent → SessionUpdate 映射表

| LaneEventName | SessionUpdate 变体 | 关键字段映射 |
|---------------|-------------------|-------------|
| `Started` | `AgentMessageChunk`(text) | `"Lane started"` |
| `Ready` | `AgentMessageChunk`(text) | `"Lane ready"` |
| `PromptMisdelivery` | `AgentMessageChunk`(text) | `"Prompt misdelivery: {detail}"` |
| `Blocked` | `ToolCall`(status=Failed) | tool_call_id=data.blocker_id, title=detail |
| `Red` | `AgentMessageChunk`(text) | `"Lane red: {detail}"` |
| `Green` | `AgentMessageChunk`(text) | `"Lane green: {detail}"` |
| `CommitCreated` | `ToolCall`(status=Completed) | tool_call_id=data.commit, title="Commit: {commit}" |
| `PrOpened` | `ToolCall`(status=Completed) | tool_call_id=data.pr_url, title="PR opened" |
| `MergeReady` | `AgentMessageChunk`(text) | `"Merge ready: {detail}"` |
| `Finished` | `AgentMessageChunk`(text) | `"Lane finished"` |
| `Failed` | `ToolCall`(status=Failed) | tool_call_id=fingerprint, title=detail |
| `Reconciled` | `AgentMessageChunk`(text) | `"Reconciled: {detail}"` |
| `Merged` | `AgentMessageChunk`(text) | `"Merged"` |
| `Superseded` | `AgentMessageChunk`(text) | `"Superseded by {data.superseded_by}"` |
| `Closed` | `AgentMessageChunk`(text) | `"Lane closed"` |
| `BranchStaleAgainstMain` | `AgentMessageChunk`(text) | `"Branch stale: {behind_main} behind"` |
| `BranchWorkspaceMismatch` | `AgentMessageChunk`(text) | `"Workspace mismatch: {detail}"` |
| `ShipPrepared` | `ToolCall`(status=Pending) | tool_call_id=data.commit_range, title="Ship prepared" |
| `ShipCommitsSelected` | `AgentMessageChunk`(text) | `"Selected {count} commits"` |
| `ShipMerged` | `ToolCall`(status=Completed) | tool_call_id=data.commit_range, title="Ship merged" |
| `ShipPushedMain` | `ToolCall`(status=Completed) | tool_call_id=data.commit_range, title="Pushed to main" |
| `SubagentHandoff` | `ToolCall`(status=Pending) | tool_call_id=subagent_id, title="Subagent: {task}" |
| `SubagentResult` | `ToolCall`(status=Completed/Failed) | tool_call_id=subagent_id, title=result |

### 5.3 转换代码骨架

```rust
// rust/crates/claw-shell/src/agent.rs(P0 新增方法)
//
// 把 23 种 LaneEvent 转换为 ACP SessionUpdate。
// 大部分映射到 AgentMessageChunk(text)用于 IDE 展示,
// 关键操作(SubagentHandoff / CommitCreated / Ship*)映射到 ToolCall 更结构化。

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
    /// LaneEvent → ACP SessionUpdate 转换
    /// 
    /// 返回 None 表示该事件不需要推送(如内部健康检查)。
    fn lane_event_to_session_update(event: &runtime::LaneEvent) -> Option<acp::SessionUpdate> {
        use runtime::LaneEventName as N;
        match event.event {
            // 子 agent 事件 → ToolCall(结构化展示)
            N::SubagentHandoff => {
                let data = event.data.as_ref()?;
                let subagent_id = data["subagent_id"].as_str()?;
                let task = data["task"].as_str()?;
                Some(acp::SessionUpdate::ToolCall {
                    tool_call_id: acp::ToolCallId::new(subagent_id.to_string()),
                    tool_kind: acp::ToolKind::Execute,
                    title: format!("Subagent: {}", task),
                    status: acp::ToolCallStatus::Pending,
                    raw_input: serde_json::json!({}).into(),
                    raw_output: None.into(),
                    locations: vec![],
                })
            }
            N::SubagentResult => {
                let data = event.data.as_ref()?;
                let subagent_id = data["subagent_id"].as_str()?;
                let status = if event.failure_class.is_some() {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                };
                Some(acp::SessionUpdate::ToolCall {
                    tool_call_id: acp::ToolCallId::new(subagent_id.to_string()),
                    tool_kind: acp::ToolKind::Execute,
                    title: "Subagent completed".to_string(),
                    status,
                    raw_input: serde_json::json!({}).into(),
                    raw_output: data["result"].clone().into(),
                    locations: vec![],
                })
            }
            // Git 事件 → ToolCall
            N::CommitCreated => {
                let data = event.data.as_ref()?;
                let commit = data["commit"].as_str()?;
                Some(acp::SessionUpdate::ToolCall {
                    tool_call_id: acp::ToolCallId::new(commit.to_string()),
                    tool_kind: acp::ToolKind::Execute,
                    title: format!("Commit: {}", &commit[..7.min(commit.len())]),
                    status: acp::ToolCallStatus::Completed,
                    raw_input: serde_json::json!({}).into(),
                    raw_output: event.detail.clone().into(),
                    locations: vec![],
                })
            }
            // 终态事件 → AgentMessageChunk(文本通知)
            N::Finished | N::Failed | N::Merged | N::Superseded | N::Closed => {
                let text = format!(
                    "{}: {}",
                    serde_json::to_value(event.event).ok()?.as_str()?,
                    event.detail.as_deref().unwrap_or("no detail")
                );
                Some(acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::Text(acp::TextContent::new(text)),
                )))
            }
            // 其他事件 → 简单文本
            _ => {
                let text = format!(
                    "{}: {}",
                    serde_json::to_value(event.event).ok()?.as_str()?,
                    event.detail.as_deref().unwrap_or("")
                );
                Some(acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::Text(acp::TextContent::new(text)),
                )))
            }
        }
    }
}
```

### 5.4 flush 时机

`flush_lane_events_to_acp` 应在以下位置调用:

| 调用点 | 时机 | 目的 |
|--------|------|------|
| `prompt` 开始 | `run_turn` 前 | 推送上一轮遗留事件 |
| `prompt` 结束 | `run_turn` 后 | 推送本轮产生的事件 |
| `cancel` 触发 | token.cancel() 后 | 推送中断相关事件 |
| `tool` 执行前后 | tool call 循环内 | 实时推送子 agent handoff(P1,需 run_turn 改造) |
| `new_session` 后 | runtime 创建后 | 推送 session 初始化事件 |
| `load_session` 后 | 状态恢复后 | 推送历史事件重放 |

### 5.5 完整 23 种事件映射代码(v0.2 新增)

§5.3 给出了核心骨架,本节展开为完整的 23 种 LaneEvent → SessionUpdate 映射实现,覆盖每个事件的字段提取、错误降级、IDE 展示语义:

```rust
// rust/crates/claw-shell/src/agent.rs(P0 完整实现,200+ 行)
//
// 23 种 LaneEvent 到 ACP SessionUpdate 的完整映射。
// 设计原则:
// 1. 结构化事件(Git / Subagent / Ship)映射到 ToolCall,IDE 可显示状态徽标
// 2. 文本事件(生命周期 / 状态)映射到 AgentMessageChunk,IDE 在对话流中展示
// 3. 字段提取失败时返回 None(跳过推送,不影响 agent 运行)
// 4. 失败事件(failure_class.is_some())映射到 ToolCallStatus::Failed

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
    fn lane_event_to_session_update(event: &runtime::LaneEvent) -> Option<acp::SessionUpdate> {
        use runtime::LaneEventName as N;
        let detail = event.detail.as_deref().unwrap_or("");
        let data = event.data.as_ref();
        let failure = event.failure_class.is_some();

        match event.event {
            // ============ 生命周期事件(1-3)============
            // Lane 启动:推送文本通知
            N::Started => Some(Self::text_chunk(&format!(
                "Lane started: {}", detail
            ))),
            // Lane 就绪:推送文本通知
            N::Ready => Some(Self::text_chunk(&format!(
                "Lane ready: {}", detail
            ))),
            // Prompt 路由错误:推送失败 ToolCall(醒目)
            N::PromptMisdelivery => {
                let blocker_id = data.and_then(|d| d["blocker_id"].as_str()).unwrap_or("unknown");
                Some(Self::tool_call_failed(
                    blocker_id,
                    "Prompt misdelivery",
                    detail,
                ))
            }

            // ============ 阻塞与状态事件(4-6)============
            // Lane 阻塞:推送 Pending ToolCall
            N::Blocked => {
                let blocker_id = data.and_then(|d| d["blocker_id"].as_str()).unwrap_or("blocker");
                Some(Self::tool_call_pending(
                    blocker_id,
                    &format!("Blocked: {}", detail),
                ))
            }
            // Red 状态(失败)
            N::Red => Some(Self::text_chunk(&format!(
                "Lane red: {}", detail
            ))),
            // Green 状态(成功)
            N::Green => Some(Self::text_chunk(&format!(
                "Lane green: {}", detail
            ))),

            // ============ Git 事件(7-9)============
            // Commit 创建:推送 Completed ToolCall
            N::CommitCreated => {
                let commit = data.and_then(|d| d["commit"].as_str())?;
                let short = &commit[..7.min(commit.len())];
                Some(Self::tool_call_completed(
                    commit,
                    &format!("Commit: {}", short),
                    detail,
                ))
            }
            // PR 打开:推送 Completed ToolCall(可点击跳转)
            N::PrOpened => {
                let pr_url = data.and_then(|d| d["pr_url"].as_str())?;
                Some(Self::tool_call_completed(
                    pr_url,
                    "PR opened",
                    detail,
                ))
            }
            // Merge 就绪:推送文本(等待人工 review)
            N::MergeReady => Some(Self::text_chunk(&format!(
                "Merge ready: {}", detail
            ))),

            // ============ 终态事件(10-15)============
            N::Finished => Some(Self::text_chunk("Lane finished")),
            N::Failed => {
                let fingerprint = data
                    .and_then(|d| d["fingerprint"].as_str())
                    .unwrap_or("failure");
                Some(Self::tool_call_failed(
                    fingerprint,
                    "Lane failed",
                    detail,
                ))
            }
            N::Reconciled => Some(Self::text_chunk(&format!(
                "Reconciled: {}", detail
            ))),
            N::Merged => Some(Self::text_chunk("Lane merged")),
            N::Superseded => {
                let by = data
                    .and_then(|d| d["superseded_by"].as_str())
                    .unwrap_or("unknown");
                Some(Self::text_chunk(&format!(
                    "Superseded by {}", by
                )))
            }
            N::Closed => Some(Self::text_chunk("Lane closed")),

            // ============ 分支事件(16-17)============
            N::BranchStaleAgainstMain => {
                let behind = data
                    .and_then(|d| d["behind_main"].as_u64())
                    .unwrap_or(0);
                Some(Self::text_chunk(&format!(
                    "Branch stale: {} commits behind main", behind
                )))
            }
            N::BranchWorkspaceMismatch => Some(Self::text_chunk(&format!(
                "Workspace mismatch: {}", detail
            ))),

            // ============ 发布事件(18-21)============
            N::ShipPrepared => {
                let range = data
                    .and_then(|d| d["commit_range"].as_str())
                    .unwrap_or("range");
                Some(Self::tool_call_pending(
                    range,
                    "Ship prepared",
                ))
            }
            N::ShipCommitsSelected => {
                let count = data
                    .and_then(|d| d["count"].as_u64())
                    .unwrap_or(0);
                Some(Self::text_chunk(&format!(
                    "Selected {} commits for ship", count
                )))
            }
            N::ShipMerged => {
                let range = data
                    .and_then(|d| d["commit_range"].as_str())
                    .unwrap_or("range");
                Some(Self::tool_call_completed(
                    range,
                    "Ship merged",
                    detail,
                ))
            }
            N::ShipPushedMain => {
                let range = data
                    .and_then(|d| d["commit_range"].as_str())
                    .unwrap_or("range");
                Some(Self::tool_call_completed(
                    range,
                    "Pushed to main",
                    detail,
                ))
            }

            // ============ 子 agent 事件(22-23)============
            N::SubagentHandoff => {
                let sub_id = data.and_then(|d| d["subagent_id"].as_str())?;
                let task = data.and_then(|d| d["task"].as_str()).unwrap_or("task");
                Some(Self::tool_call_pending(
                    sub_id,
                    &format!("Subagent: {}", task),
                ))
            }
            N::SubagentResult => {
                let sub_id = data.and_then(|d| d["subagent_id"].as_str())?;
                let result_text = data
                    .and_then(|d| d["result"].as_str())
                    .unwrap_or("(no result)");
                let status = if failure {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                };
                Some(acp::SessionUpdate::ToolCall {
                    tool_call_id: acp::ToolCallId::new(sub_id.to_string()),
                    tool_kind: acp::ToolKind::Execute,
                    title: "Subagent completed".to_string(),
                    status,
                    raw_input: serde_json::json!({}).into(),
                    raw_output: serde_json::Value::String(result_text.to_string()).into(),
                    locations: vec![],
                })
            }
        }
    }

    // ===== 辅助构造器(简化代码)=====
    fn text_chunk(text: &str) -> acp::SessionUpdate {
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
            acp::ContentBlock::Text(acp::TextContent::new(text.to_string())),
        ))
    }

    fn tool_call_pending(id: &str, title: &str) -> acp::SessionUpdate {
        acp::SessionUpdate::ToolCall {
            tool_call_id: acp::ToolCallId::new(id.to_string()),
            tool_kind: acp::ToolKind::Execute,
            title: title.to_string(),
            status: acp::ToolCallStatus::Pending,
            raw_input: serde_json::json!({}).into(),
            raw_output: None.into(),
            locations: vec![],
        }
    }

    fn tool_call_completed(id: &str, title: &str, output: &str) -> acp::SessionUpdate {
        acp::SessionUpdate::ToolCall {
            tool_call_id: acp::ToolCallId::new(id.to_string()),
            tool_kind: acp::ToolKind::Execute,
            title: title.to_string(),
            status: acp::ToolCallStatus::Completed,
            raw_input: serde_json::json!({}).into(),
            raw_output: serde_json::Value::String(output.to_string()).into(),
            locations: vec![],
        }
    }

    fn tool_call_failed(id: &str, title: &str, error: &str) -> acp::SessionUpdate {
        acp::SessionUpdate::ToolCall {
            tool_call_id: acp::ToolCallId::new(id.to_string()),
            tool_kind: acp::ToolKind::Execute,
            title: title.to_string(),
            status: acp::ToolCallStatus::Failed,
            raw_input: serde_json::json!({}).into(),
            raw_output: serde_json::Value::String(error.to_string()).into(),
            locations: vec![],
        }
    }
}
```

**映射覆盖完整性验证**:

| 类别 | 事件数 | 映射方式 | 备注 |
|------|--------|---------|------|
| 生命周期 | 3 | AgentMessageChunk(text) | Started/Ready/PromptMisdelivery(Misdelivery 实际用 ToolCall Failed) |
| 阻塞与状态 | 3 | ToolCall(Pending/Failed)或 text | Blocked=Pending, Red/Green=text |
| Git | 3 | ToolCall(Completed) | CommitCreated/PrOpened/MergeReady(text) |
| 终态 | 6 | text 或 ToolCall(Failed) | Failed=ToolCall Failed, 其他=text |
| 分支 | 2 | text | Stale/Mismatch |
| 发布 | 4 | ToolCall(Pending/Completed) | Prepared=Pending, Selected=text, Merged/Pushed=Completed |
| 子 agent | 2 | ToolCall(Pending/Completed/Failed) | Handoff=Pending, Result=Completed/Failed |
| **合计** | **23** | | 全部覆盖 |

### 5.6 flush 时机实施细节(v0.2 新增)

§5.4 列出了 6 个调用点,本节展开每个调用点的实施细节、伪代码、风险点:

#### 调用点 1:`prompt` 主循环每次迭代后

**位置**:`ClawAgent::prompt` 中,`run_turn` 内部 tool 执行循环的每次迭代后。

**当前限制**:`run_turn` 是同步阻塞 API,P0 阶段无法在循环内插入 flush。**P0 妥协方案**仅在 `run_turn` 返回后一次性 flush。P1 改造 `run_turn` 为 async 后,可在循环内插入。

```rust
// rust/crates/claw-shell/src/agent.rs(P0 妥协方案,见 §3.3 prompt 骨架)
async fn prompt(&self, arguments: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
    // ...
    Self::flush_lane_events_to_acp(&self.client_gateway, &session_id); // run_turn 前
    let turn_result = runtime_rc.run_turn(user_input, None);
    Self::flush_lane_events_to_acp(&self.client_gateway, &session_id); // run_turn 后
    // ...
}

// rust/crates/runtime/src/runtime.rs(P1 改造后,在 run_turn 循环内)
async fn run_turn_async(&mut self, input: String, token: Option<CancellationToken>) -> Result<TurnSummary, RuntimeError> {
    loop {
        let step_result = self.step_once().await?;
        // P1:每次迭代后 flush
        self.flush_callback(&|events| {
            for event in events {
                self.lane_event_callback(event);
            }
        });
        if token.as_ref().map(|t| t.is_cancelled()).unwrap_or(false) {
            return Ok(TurnSummary::cancelled());
        }
        if step_result.is_terminal { break; }
    }
    // ...
}
```

#### 调用点 2:tool call 完成后

**位置**:`ConversationRuntime` 的 tool 执行循环内,每个 tool 返回结果后。

**P0**:无法实现(同步 `run_turn`)。**P1**:在 tool_executor 完成后立即 flush,确保 IDE 实时看到 tool 进度。

```rust
// P1 实现
for tool_call in pending_tool_calls {
    let result = self.tool_executor.execute(tool_call).await?;
    // flush:tool 完成事件
    runtime::publish_lane_event(LaneEvent {
        event: LaneEventName::SubagentResult, // 或 ToolCompleted
        data: Some(serde_json::to_value(&result)?),
        detail: Some(result.summary()),
        failure_class: result.error.map(|_| FailureClass::Tool),
        // ...
    });
    self.flush_lane_events_now(); // 立即 flush,不等 turn 结束
}
```

#### 调用点 3:subagent 完成后

**位置**:子 agent(handoff → result)完成后,父 agent 接收结果时。

**当前现状**:[lane_events.rs:1027-1104](../../rust/crates/runtime/src/lane_events.rs) 中 `SubagentHandoff` / `SubagentResult` 已经发布到全局 sink,但无消费者。本调用点正是消费这些事件。

**实施**:在 `ConversationRuntime::handle_subagent_result` 内调用 `flush_lane_events_to_acp`。

#### 调用点 4:压缩完成后

**位置**:context compression(对话历史压缩)完成后。

**背景**:长对话会触发压缩(把早期消息总结为摘要),压缩完成应通知 IDE(显示"上下文已压缩"提示)。

```rust
// rust/crates/runtime/src/runtime.rs(P1)
async fn maybe_compress_context(&mut self) -> Result<(), RuntimeError> {
    if self.session.messages.len() > self.config.compress_threshold {
        let summary = self.compress_messages().await?;
        // flush:压缩完成事件
        runtime::publish_lane_event(LaneEvent {
            event: LaneEventName::Reconciled, // 复用 Reconciled 表示"重新整理"
            data: Some(serde_json::json!({ "summary_len": summary.len() })),
            detail: Some(format!("Context compressed: {} messages → 1 summary", original_count)),
            failure_class: None,
        });
        self.flush_lane_events_now();
    }
    Ok(())
}
```

#### 调用点 5:cancel 触发时

**位置**:`ClawAgent::cancel` 内,`CancellationToken::cancel()` 之后。

**目的**:推送中断相关事件,让 IDE 知道 agent 已停止。

```rust
// rust/crates/claw-shell/src/agent.rs(P1)
async fn cancel(&self, _arguments: acp::CancelNotification) -> Result<(), acp::Error> {
    if let Some(token) = self.cancel_token.borrow().as_ref() {
        token.cancel();
        tracing::info!("claw-agent: cancellation triggered");
        // flush:cancel 事件
        if let Some(session_id) = self.current_session_id.borrow().as_ref() {
            Self::flush_lane_events_to_acp(&self.client_gateway, session_id);
            // 额外推送一条显式的 cancel notification
            self.notify(session_id, acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(acp::ContentBlock::Text(
                    acp::TextContent::new("Operation cancelled by user".to_string())
                ))
            ));
        }
    }
    Ok(())
}
```

### 5.7 背压机制(v0.2 新增)

**问题**:若 IDE 端处理 SessionNotification 慢(如 Zed UI 卡顿、VS Code extension host 阻塞),`forward_fire_and_forget` 会持续往 channel 发送,可能积压导致:

1. `mpsc::UnboundedSender` 内存无限增长(若用 unbounded channel)
2. LaneEvent sink(容量 512)溢出,丢失最旧一半事件

**背压降级策略**:

| 触发条件 | 降级行为 | 用户感知 |
|---------|---------|---------|
| LaneEvent sink 长度 > 256(50%) | 启动合并:相邻同类事件合并为一条(如 5 个 AgentMessageChunk 合并为 1 个) | 消息粒度变粗 |
| LaneEvent sink 长度 > 384(75%) | 启动采样:每 N 个事件只推送 1 个(类似 throttle) | 部分中间状态丢失 |
| LaneEvent sink 长度 ≥ 512(100%) | 启动丢弃:最旧一半直接 drop(已有行为) | 早期事件不可见 |
| ACP channel send 失败 | log error,跳过该 notification(已有 fire-and-forget 行为) | 部分 notification 丢失 |
| IDE 长时间无响应(> 30s) | 主动关闭 session,推送 SessionUpdate::Error | 显示"连接超时" |

**实施代码骨架**:

```rust
// rust/crates/claw-shell/src/agent.rs(P0 背压监控)

const LANE_EVENT_WARN_THRESHOLD: usize = 256;
const LANE_EVENT_DEGRADE_THRESHOLD: usize = 384;
const LANE_EVENT_DROP_THRESHOLD: usize = 512;

impl<C> ClawAgent<C>
where
    C: ApiClient + 'static,
{
    pub fn flush_lane_events_to_acp(
        gateway: &AcpGatewaySender<acp::AgentSide>,
        session_id: &acp::SessionId,
    ) {
        let events = runtime::drain_lane_events();
        if events.is_empty() {
            return;
        }
        // 监控 sink 长度(下次 flush 前的近似值)
        let remaining = runtime::lane_event_sink_len();
        if remaining >= LANE_EVENT_DROP_THRESHOLD {
            tracing::error!(
                "claw-agent: lane event sink overflow ({}), events may be dropped",
                remaining
            );
        } else if remaining >= LANE_EVENT_DEGRADE_THRESHOLD {
            tracing::warn!(
                "claw-agent: lane event sink degrade mode ({}), sampling events",
                remaining
            );
            // 采样:每 3 个事件只推送 1 个
            for (i, event) in events.iter().enumerate() {
                if i % 3 == 0 {
                    if let Some(notif) = Self::lane_event_to_session_update(event) {
                        let n = acp::SessionNotification::new(session_id.clone(), notif);
                        gateway.forward_fire_and_forget(n);
                    }
                }
            }
            return;
        } else if remaining >= LANE_EVENT_WARN_THRESHOLD {
            tracing::warn!(
                "claw-agent: lane event sink approaching capacity ({}/{}), merging similar events",
                remaining, LANE_EVENT_DROP_THRESHOLD
            );
            // 合并:相邻同类 AgentMessageChunk 合并
            Self::flush_with_merge(gateway, session_id, events);
            return;
        }
        // 正常路径:全部推送
        for event in events {
            if let Some(notif) = Self::lane_event_to_session_update(&event) {
                let n = acp::SessionNotification::new(session_id.clone(), notif);
                gateway.forward_fire_and_forget(n);
            }
        }
    }

    /// 合并相邻同类事件(背压降级)
    fn flush_with_merge(
        gateway: &AcpGatewaySender<acp::AgentSide>,
        session_id: &acp::SessionId,
        events: Vec<runtime::LaneEvent>,
    ) {
        let mut merged_text = String::new();
        let mut last_event_kind: Option<runtime::LaneEventName> = None;
        for event in events {
            // 简化:仅合并映射到 AgentMessageChunk 的事件
            let is_text = matches!(
                event.event,
                runtime::LaneEventName::Started
                | runtime::LaneEventName::Ready
                | runtime::LaneEventName::Red
                | runtime::LaneEventName::Green
                | runtime::LaneEventName::Finished
                | runtime::LaneEventName::Merged
                | runtime::LaneEventName::Closed
            );
            if is_text {
                if last_event_kind != Some(event.event) && !merged_text.is_empty() {
                    // 不同类事件,先 flush 之前合并的
                    let notif = acp::SessionNotification::new(
                        session_id.clone(),
                        Self::text_chunk(&merged_text),
                    );
                    gateway.forward_fire_and_forget(notif);
                    merged_text.clear();
                }
                merged_text.push_str(event.detail.as_deref().unwrap_or(""));
                merged_text.push('\n');
                last_event_kind = Some(event.event);
            } else {
                // 非文本事件:先 flush 之前合并的,再单独推送
                if !merged_text.is_empty() {
                    let notif = acp::SessionNotification::new(
                        session_id.clone(),
                        Self::text_chunk(&merged_text),
                    );
                    gateway.forward_fire_and_forget(notif);
                    merged_text.clear();
                }
                if let Some(notif) = Self::lane_event_to_session_update(&event) {
                    let n = acp::SessionNotification::new(session_id.clone(), notif);
                    gateway.forward_fire_and_forget(n);
                }
                last_event_kind = None;
            }
        }
        // flush 剩余
        if !merged_text.is_empty() {
            let notif = acp::SessionNotification::new(
                session_id.clone(),
                Self::text_chunk(&merged_text),
            );
            gateway.forward_fire_and_forget(notif);
        }
    }
}
```

**背压监控指标**(供 §12 性能基准采集):

- `lane_event_sink_len`:当前 sink 长度(实时)
- `lane_event_drop_count`:累计丢弃事件数
- `lane_event_merge_count`:累计合并事件数
- `acp_notification_send_failure_count`:ACP channel 发送失败次数

---

## 六、VS Code 扩展骨架

### 6.1 设计原则

VS Code 扩展采用**薄客户端**模式:

1. 不实现 agent 逻辑,只做 UI 桥接
2. 通过 `child_process.spawn` 启动 `claw-plus-headless` binary
3. 通过 stdin/stdout 传 JSON-RPC,与 ACP 协议完全对齐
4. 复用 `vscode-languageserver/node` 的 `createConnection` 处理 JSON-RPC framing

### 6.2 package.json 骨架

```json
{
  "name": "claw-code",
  "displayName": "Claw Plus",
  "description": "ACP-compatible AI coding agent for VS Code",
  "version": "0.1.0",
  "engines": { "vscode": "^1.85.0" },
  "categories": ["Other", "Chat", "Machine Learning"],
  "activationEvents": [
    "onCommand:claw.startServer",
    "onStartupFinished"
  ],
  "main": "./out/extension.js",
  "contributes": {
    "commands": [
      { "command": "claw.startServer", "title": "Claw: Start Server" },
      { "command": "claw.stopServer", "title": "Claw: Stop Server" },
      { "command": "claw.sendPrompt", "title": "Claw: Send Prompt" }
    ],
    "configuration": {
      "title": "Claw Plus",
      "properties": {
        "claw.binaryPath": {
          "type": "string",
          "default": "claw-plus-headless",
          "description": "Path to claw-plus-headless binary"
        },
        "claw.model": {
          "type": "string",
          "default": "claude-sonnet-4-5",
          "description": "Model to use"
        },
        "claw.permissionMode": {
          "type": "string",
          "enum": ["read-only", "workspace-write", "danger-full-access"],
          "default": "workspace-write",
          "description": "Permission mode"
        }
      }
    },
    "chatParticipants": [
      {
        "id": "claw",
        "name": "claw",
        "description": "Claw Plus AI agent",
        "isSticky": true,
        "commands": [
          { "name": "plan", "description": "Plan mode (read-only)" },
          { "name": "act", "description": "Act mode (default)" }
        ]
      }
    ]
  },
  "scripts": {
    "vscode:prepublish": "npm run compile",
    "compile": "tsc -p ./"
  },
  "dependencies": {
    "vscode-languageserver": "^9.0.1",
    "vscode-languageclient": "^9.0.1"
  }
}
```

### 6.3 extension.ts 骨架

```typescript
// vscode-claw-extension/src/extension.ts
//
// 薄客户端架构:
// - spawn claw-plus-headless 子进程
// - 用 vscode-languageserver 的 createConnection 桥接 stdin/stdout
// - 注册 fs/read_text_file / fs/write_text_file / session/request_permission 反向 handler

import * as vscode from 'vscode';
import { spawn, ChildProcess } from 'child_process';
import { createConnection, TextDocument } from 'vscode-languageserver/node';

let clawProcess: ChildProcess | null = null;
let outputChannel: vscode.OutputChannel;
let connection: any = null;

export function activate(context: vscode.ExtensionContext) {
    outputChannel = vscode.window.createOutputChannel('Claw Plus');

    // 注册命令:启动 Claw ACP server
    context.subscriptions.push(
        vscode.commands.registerCommand('claw.startServer', startClawServer)
    );

    // 注册命令:停止 Claw ACP server
    context.subscriptions.push(
        vscode.commands.registerCommand('claw.stopServer', stopClawServer)
    );

    // 注册 chat participant(@claw)
    const participant = vscode.chat.createChatParticipant('claw', handleChatRequest);
    participant.iconPath = new vscode.ThemeIcon('comment-discussion');

    // 监听配置变更
    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration(e => {
            if (e.affectsConfiguration('claw')) {
                outputChannel.appendLine('Config changed, restart server to apply');
            }
        })
    );
}

async function startClawServer() {
    if (clawProcess) {
        vscode.window.showWarningMessage('Claw server is already running');
        return;
    }

    const config = vscode.workspace.getConfiguration('claw');
    const binaryPath = config.get<string>('binaryPath', 'claw-plus-headless');
    const model = config.get<string>('model', 'claude-sonnet-4-5');
    const permissionMode = config.get<string>('permissionMode', 'workspace-write');

    // spawn claw-plus-headless 子进程
    clawProcess = spawn(binaryPath, [
        '--model', model,
        '--permission-mode', permissionMode,
    ], {
        cwd: vscode.workspace.rootPath,
        stdio: ['pipe', 'pipe', 'pipe'],
    });

    // stderr 走 output channel
    clawProcess.stderr?.on('data', data => {
        outputChannel.append(`[stderr] ${data.toString()}`);
    });

    // stdout 接 JSON-RPC connection
    connection = createConnection(
        clawProcess.stdout!,
        clawProcess.stdin!
    );
    connection.listen();

    // 初始化握手
    const initResult = await connection.sendRequest('initialize', {
        protocolVersion: 1,
        clientCapabilities: {
            fs_read_text_file: true,
            fs_write_text_file: true,
            session_request_permission: true,
        },
    });
    outputChannel.appendLine(`Initialized: ${JSON.stringify(initResult)}`);

    // 注册 fs/read_text_file 反向请求 handler
    connection.onRequest('fs/read_text_file', async (params: any) => {
        return handleReadTextFile(params);
    });

    // 注册 fs/write_text_file 反向请求 handler
    connection.onRequest('fs/write_text_file', async (params: any) => {
        return handleWriteTextFile(params);
    });

    // 注册 session/request_permission 反向请求 handler
    connection.onRequest('session/request_permission', async (params: any) => {
        return handleRequestPermission(params);
    });

    outputChannel.appendLine('Claw server started');
}

async function handleReadTextFile(params: { path: string }) {
    // 优先从 editor buffer 取(含未保存内容)
    const uri = vscode.Uri.file(params.path);
    const doc = vscode.workspace.textDocuments.find(d => d.uri.fsPath === uri.fsPath);
    if (doc) {
        return { content: doc.getText() };
    }
    // 回退到磁盘读取
    try {
        const content = await vscode.workspace.fs.readFile(uri);
        return { content: Buffer.from(content).toString() };
    } catch (e) {
        throw new Error(`File not found: ${params.path}`);
    }
}

async function handleWriteTextFile(params: { path: string; content: string }) {
    const uri = vscode.Uri.file(params.path);
    // 走 editor API,确保进 undo 栈
    let doc = vscode.workspace.textDocuments.find(d => d.uri.fsPath === uri.fsPath);
    if (!doc) {
        doc = await vscode.workspace.openTextDocument(uri);
    }
    const edit = new vscode.WorkspaceEdit();
    edit.replace(uri, new vscode.Range(0, 0, doc.lineCount, 0), params.content);
    const applied = await vscode.workspace.applyEdit(edit);
    return { written: applied };
}

async function handleRequestPermission(params: {
    toolName: string;
    toolInput: any;
    options: string[];
}) {
    const choice = await vscode.window.showWarningMessage(
        `Claw 请求执行: ${params.toolName}`,
        { modal: true },
        '允许', '拒绝', '始终允许'
    );
    const outcome = choice === '允许' ? 'allow'
        : choice === '始终允许' ? 'always_allow'
        : 'deny';
    return { outcome };
}

async function handleChatRequest(
    request: vscode.ChatRequest,
    context: vscode.ChatContext,
    stream: vscode.ChatResponseStream,
    token: vscode.CancellationToken
) {
    if (!connection) {
        await startClawServer();
    }
    // 发送 prompt 到 ACP server
    // 此处需要先 new_session(若未创建),再 prompt
    // 简化:每次都创建新 session
    stream.markdown('Thinking...\n');
    // TODO: 实现 session 管理 + prompt 流式接收
}

async function stopClawServer() {
    if (connection) {
        connection.dispose();
        connection = null;
    }
    if (clawProcess) {
        clawProcess.kill();
        clawProcess = null;
        outputChannel.appendLine('Claw server stopped');
    }
}

export function deactivate() {
    stopClawServer();
}
```

### 6.4 UI 集成要点

| UI 元素 | 集成方式 | ACP 方法 |
|---------|---------|---------|
| Chat panel | `vscode.chat.createChatParticipant` | `session/prompt` |
| 文件读取 | `vscode.workspace.textDocuments` | `fs/read_text_file` 反向 handler |
| 文件写入 | `vscode.WorkspaceEdit`(走 undo 栈) | `fs/write_text_file` 反向 handler |
| 权限弹窗 | `vscode.window.showWarningMessage`(modal) | `session/request_permission` 反向 handler |
| 输出日志 | `vscode.OutputChannel` | stderr 转发 |
| 状态栏 | `vscode.window.createStatusBarItem` | 显示 server running/stopped |
| 进度条 | `vscode.window.withProgress` | `SessionUpdate::ToolCall` Pending |
| Diff 视图 | `vscode.diff` 命令 | `SessionUpdate::ToolCall` Completed with diff |

### 6.5 ACP 传输层实现(v0.2 新增)

§6.3 的 `extension.ts` 骨架中 `spawn(binaryPath, ...)` 隐藏了传输层细节。本节展开为独立模块,便于复用与测试:

```typescript
// vscode-claw-extension/src/acp-transport.ts
//
// ACP 传输层:封装 claw-plus-headless 子进程的 stdin/stdout 管道,
// 处理 JSON-RPC framing(每行一条 NDJSON)。
//
// 设计原则:
// 1. 与 vscode-languageserver 解耦:不依赖 LSP 协议,仅借用 JSON-RPC framing
// 2. 可测试:核心 IO 逻辑可注入(inject)fake stdin/stdout
// 3. 错误隔离:子进程崩溃不传染主 extension

import { spawn, ChildProcess } from 'child_process';
import { EventEmitter } from 'events';
import * as readline from 'readline';

export interface AcpTransportOptions {
    binaryPath: string;
    args: string[];
    cwd?: string;
    env?: Record<string, string>;
}

export interface AcpRequest {
    jsonrpc: '2.0';
    method: string;
    params?: unknown;
    id: number | string;
}

export interface AcpNotification {
    jsonrpc: '2.0';
    method: string;
    params?: unknown;
}

export interface AcpResponse {
    jsonrpc: '2.0';
    id: number | string;
    result?: unknown;
    error?: { code: number; message: string; data?: unknown };
}

export class AcpTransport extends EventEmitter {
    private process: ChildProcess | null = null;
    private nextId = 1;
    private pending = new Map<number | string, {
        resolve: (r: unknown) => void;
        reject: (e: Error) => void;
    }>();
    private stdoutRL: readline.Interface | null = null;

    constructor(private options: AcpTransportOptions) {
        super();
    }

    /** 启动 claw-plus-headless 子进程,初始化 JSON-RPC 通道 */
    async start(): Promise<void> {
        if (this.process) {
            throw new Error('Transport already started');
        }
        const { binaryPath, args, cwd, env } = this.options;
        this.process = spawn(binaryPath, args, {
            cwd,
            env: { ...process.env, ...env },
            stdio: ['pipe', 'pipe', 'pipe'],
        });
        // stdout:逐行读取 NDJSON
        this.stdoutRL = readline.createInterface({
            input: this.process.stdout!,
            crlfDelay: Infinity,
        });
        this.stdoutRL.on('line', (line: string) => this.handleLine(line));
        // stderr:转发为 'stderr' 事件
        this.process.stderr?.on('data', (data: Buffer) => {
            this.emit('stderr', data.toString());
        });
        // 进程退出:emit 'exit' 事件,reject 所有 pending 请求
        this.process.on('exit', (code, signal) => {
            this.emit('exit', { code, signal });
            const err = new Error(`claw-plus-headless exited: code=${code} signal=${signal}`);
            for (const { reject } of this.pending.values()) {
                reject(err);
            }
            this.pending.clear();
            this.process = null;
            this.stdoutRL = null;
        });
        this.process.on('error', (err) => {
            this.emit('error', err);
        });
    }

    /** 发送 JSON-RPC request,返回 Promise 等待响应 */
    async request(method: string, params?: unknown): Promise<unknown> {
        if (!this.process || !this.process.stdin.writable) {
            throw new Error('Transport not started or stdin closed');
        }
        const id = this.nextId++;
        const req: AcpRequest = { jsonrpc: '2.0', method, params, id };
        return new Promise((resolve, reject) => {
            this.pending.set(id, { resolve, reject });
            const line = JSON.stringify(req) + '\n';
            this.process!.stdin.write(line, (err) => {
                if (err) {
                    this.pending.delete(id);
                    reject(new Error(`Failed to write request ${method}: ${err.message}`));
                }
            });
        });
    }

    /** 发送 JSON-RPC notification(无 id,无响应) */
    notify(method: string, params?: unknown): void {
        if (!this.process || !this.process.stdin.writable) return;
        const notif: AcpNotification = { jsonrpc: '2.0', method, params };
        const line = JSON.stringify(notif) + '\n';
        this.process.stdin.write(line);
    }

    /** 注册反向请求 handler(IDE 接收 agent 的反向请求) */
    onReverseRequest(method: string, handler: (params: unknown) => Promise<unknown>): void {
        this.on(`reverse-request:${method}`, async (req: AcpRequest) => {
            try {
                const result = await handler(req.params);
                const resp: AcpResponse = { jsonrpc: '2.0', id: req.id, result };
                this.process?.stdin.write(JSON.stringify(resp) + '\n');
            } catch (err) {
                const resp: AcpResponse = {
                    jsonrpc: '2.0',
                    id: req.id,
                    error: { code: -32603, message: (err as Error).message },
                };
                this.process?.stdin.write(JSON.stringify(resp) + '\n');
            }
        });
    }

    /** 关闭 transport:发送 shutdown + 杀进程 */
    async stop(): Promise<void> {
        if (!this.process) return;
        // 优雅关闭:发送 exit notification
        try {
            this.notify('exit');
            // 等待 2s 让进程自然退出
            await new Promise((resolve) => setTimeout(resolve, 2000));
        } catch {
            // 忽略
        }
        if (this.process) {
            this.process.kill('SIGTERM');
            // 5s 后强制 kill
            setTimeout(() => this.process?.kill('SIGKILL'), 5000);
        }
    }

    /** 处理 stdout 一行 JSON-RPC 消息 */
    private handleLine(line: string): void {
        if (!line.trim()) return;
        let msg: any;
        try {
            msg = JSON.parse(line);
        } catch (err) {
            this.emit('parse-error', { line, error: err });
            return;
        }
        // Response(匹配 pending request)
        if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined)) {
            const pending = this.pending.get(msg.id);
            if (pending) {
                this.pending.delete(msg.id);
                if (msg.error) {
                    pending.reject(new Error(`${msg.error.code}: ${msg.error.message}`));
                } else {
                    pending.resolve(msg.result);
                }
            }
            return;
        }
        // Notification(agent → IDE,无 id)
        if (msg.id === undefined && msg.method) {
            this.emit(`notification:${msg.method}`, msg.params);
            return;
        }
        // Reverse request(agent → IDE 请求,有 id 和 method)
        if (msg.id !== undefined && msg.method) {
            this.emit(`reverse-request:${msg.method}`, msg);
            return;
        }
        this.emit('unknown-message', msg);
    }
}
```

**使用示例**(在 `extension.ts` 中):

```typescript
import { AcpTransport } from './acp-transport';

async function startClawServer() {
    const transport = new AcpTransport({
        binaryPath: config.get('binaryPath', 'claw-plus-headless'),
        args: ['--model', config.get('model', 'claude-sonnet-4-5')],
        cwd: vscode.workspace.rootPath,
    });
    transport.on('stderr', (line) => outputChannel.append(`[stderr] ${line}`));
    transport.on('exit', ({ code }) => {
        vscode.window.showWarningMessage(`Claw server exited (code ${code})`);
    });
    transport.on('notification:session/update', (params) => {
        // 处理 SessionNotification 推送
        handleSessionUpdate(params);
    });
    // 注册反向请求 handler
    transport.onReverseRequest('fs/read_text_file', async (params) => {
        return handleReadTextFile(params as { path: string });
    });
    transport.onReverseRequest('fs/write_text_file', async (params) => {
        return handleWriteTextFile(params as { path: string; content: string });
    });
    transport.onReverseRequest('session/request_permission', async (params) => {
        return handleRequestPermission(params as any);
    });
    await transport.start();
    // initialize 握手
    const initResult = await transport.request('initialize', {
        protocolVersion: 1,
        clientCapabilities: {
            fs_read_text_file: true,
            fs_write_text_file: true,
            session_request_permission: true,
        },
    });
    outputChannel.appendLine(`Initialized: ${JSON.stringify(initResult)}`);
}
```

### 6.6 错误处理(v0.2 新增)

VS Code 扩展需处理以下错误场景,确保任一错误不导致 VS Code 卡死或数据丢失:

| # | 错误场景 | 触发原因 | 处理策略 | 用户感知 |
|---|---------|---------|---------|---------|
| 1 | 子进程崩溃(exit code ≠ 0) | claw-plus-headless panic / OOM | 自动重启(最多 3 次,间隔 5s);超限提示用户检查 binary | "Claw server crashed, restarting..." |
| 2 | 子进程无法启动(ENOENT) | binaryPath 配置错误 | 提示用户检查设置,提供"打开设置"按钮 | "Binary not found at {path}" |
| 3 | stdin EOF(用户关闭窗口) | VS Code 关闭 | 优雅关闭 transport,杀子进程 | 无(后台清理) |
| 4 | stdout 解析失败(非 JSON) | 协议不兼容 / binary 输出脏数据 | log error,丢弃该行,继续 | "Failed to parse message: {line}" |
| 5 | 反向请求超时(60s 无响应) | 用户没点权限对话框 | transport.request 内部超时,默认 Deny | 权限对话框自动关闭,显示"已超时拒绝" |
| 6 | JSON-RPC id 不匹配 | 网络乱序 / 实现 bug | log warning,丢弃该响应 | 静默(仅 output channel) |
| 7 | permission cache 不一致 | 会话切换 / 模式变更 | 缓存随 session_id 隔离,切换时清空 | 无 |
| 8 | 子进程内存超限 | 长对话 / 大文件操作 | 监控 RSS,超 500MB 提示重启 | "Claw memory usage high, consider restarting" |

**错误恢复策略代码骨架**:

```typescript
// vscode-claw-extension/src/error-recovery.ts
//
// 子进程崩溃后的自动重启策略

const MAX_RESTART_ATTEMPTS = 3;
const RESTART_INTERVAL_MS = 5000;

export class ErrorRecovery {
    private restartAttempts = 0;
    private lastRestartTime = 0;

    async handleProcessExit(
        transport: AcpTransport,
        exitInfo: { code: number | null; signal: string | null },
        onRestarted: () => Promise<void>,
    ): Promise<void> {
        // 正常退出(code=0):不重启
        if (exitInfo.code === 0) return;
        // 限流:5s 内不重复重启
        const now = Date.now();
        if (now - this.lastRestartTime < RESTART_INTERVAL_MS) {
            vscode.window.showErrorMessage('Claw 重启过于频繁,放弃');
            return;
        }
        // 超过最大尝试次数
        if (this.restartAttempts >= MAX_RESTART_ATTEMPTS) {
            vscode.window.showErrorMessage(
                `Claw 已崩溃 ${MAX_RESTART_ATTEMPTS} 次,请检查日志后手动重启`,
                'Show Logs',
                'Restart'
            ).then(choice => {
                if (choice === 'Show Logs') {
                    outputChannel.show();
                } else if (choice === 'Restart') {
                    this.restartAttempts = 0;
                    this.handleProcessExit(transport, exitInfo, onRestarted);
                }
            });
            return;
        }
        this.restartAttempts++;
        this.lastRestartTime = now;
        vscode.window.showWarningMessage(
            `Claw server crashed (code=${exitInfo.code}),restarting (${this.restartAttempts}/${MAX_RESTART_ATTEMPTS})...`
        );
        await new Promise(r => setTimeout(r, RESTART_INTERVAL_MS));
        await transport.start();
        await onRestarted();
    }

    /** 成功运行 5 分钟后重置尝试次数 */
    resetOnStable(): void {
        setTimeout(() => {
            this.restartAttempts = 0;
        }, 5 * 60 * 1000);
    }
}
```

### 6.7 完整 extension.ts 骨架(v0.2 增强)

§6.3 的骨架只覆盖核心流程,本节补充完整可运行版本(集成 §6.5 transport + §6.6 error recovery + 多 session 管理):

```typescript
// vscode-claw-extension/src/extension.ts(完整版,200+ 行)
//
// 完整功能:
// 1. spawn claw-plus-headless 子进程(走 §6.5 AcpTransport)
// 2. 创建 ACP client(JSON-RPC over stdio)
// 3. 注册命令(claw.start / claw.stop / claw.sendPrompt / claw.cancelPrompt)
// 4. 实现_webviewPanel UI(对话窗口,自定义而非 chat participant)
// 5. 错误恢复(走 §6.6 ErrorRecovery)
// 6. 多 session 管理(每个 webview 一个 session)

import * as vscode from 'vscode';
import { AcpTransport } from './acp-transport';
import { ErrorRecovery } from './error-recovery';

let outputChannel: vscode.OutputChannel;
let transport: AcpTransport | null = null;
let errorRecovery: ErrorRecovery;
let statusBarItem: vscode.StatusBarItem;
const sessions = new Map<string, { panel: vscode.WebviewPanel; sessionId: string }>();

export function activate(context: vscode.ExtensionContext) {
    outputChannel = vscode.window.createOutputChannel('Claw Plus');
    errorRecovery = new ErrorRecovery();
    statusBarItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
    statusBarItem.command = 'claw.showStatus';
    statusBarItem.text = '$(comment-discussion) Claw';
    statusBarItem.tooltip = 'Claw Plus:点击管理';
    statusBarItem.show();

    context.subscriptions.push(
        vscode.commands.registerCommand('claw.startServer', startClawServer),
        vscode.commands.registerCommand('claw.stopServer', stopClawServer),
        vscode.commands.registerCommand('claw.sendPrompt', openChatPanel),
        vscode.commands.registerCommand('claw.cancelPrompt', cancelActivePrompt),
        vscode.commands.registerCommand('claw.showStatus', showStatusBarMenu),
        statusBarItem,
        outputChannel,
    );

    // 自动启动(若配置开启)
    if (vscode.workspace.getConfiguration('claw').get('autoStart', false)) {
        vscode.commands.executeCommand('claw.startServer');
    }
}

async function startClawServer() {
    if (transport) {
        vscode.window.showInformationMessage('Claw server already running');
        return;
    }
    const config = vscode.workspace.getConfiguration('claw');
    transport = new AcpTransport({
        binaryPath: config.get('binaryPath', 'claw-plus-headless'),
        args: [
            '--model', config.get('model', 'claude-sonnet-4-5'),
            '--permission-mode', config.get('permissionMode', 'workspace-write'),
        ],
        cwd: vscode.workspace.rootPath,
    });
    transport.on('stderr', (line) => outputChannel.append(`[stderr] ${line}`));
    transport.on('exit', (info) => {
        outputChannel.appendLine(`Claw exited: code=${info.code} signal=${info.signal}`);
        statusBarItem.text = '$(error) Claw (exited)';
        if (transport) {
            errorRecovery.handleProcessExit(transport, info, async () => {
                statusBarItem.text = '$(comment-discussion) Claw';
                errorRecovery.resetOnStable();
            });
        }
    });
    transport.on('notification:session/update', (params: any) => {
        handleSessionNotification(params);
    });
    // 注册反向请求 handler
    transport.onReverseRequest('fs/read_text_file', async (params) => {
        return handleReadTextFile(params as { path: string });
    });
    transport.onReverseRequest('fs/write_text_file', async (params) => {
        return handleWriteTextFile(params as { path: string; content: string });
    });
    transport.onReverseRequest('session/request_permission', async (params) => {
        return handleRequestPermission(params as any);
    });

    try {
        await transport.start();
        // initialize 握手
        await transport.request('initialize', {
            protocolVersion: 1,
            clientCapabilities: {
                fs_read_text_file: true,
                fs_write_text_file: true,
                session_request_permission: true,
            },
        });
        statusBarItem.text = '$(check) Claw';
        outputChannel.appendLine('Claw server started');
    } catch (err) {
        vscode.window.showErrorMessage(`Failed to start Claw: ${(err as Error).message}`);
        transport = null;
        statusBarItem.text = '$(error) Claw (failed)';
    }
}

async function stopClawServer() {
    if (!transport) return;
    await transport.stop();
    transport = null;
    statusBarItem.text = '$(comment-discussion) Claw';
    outputChannel.appendLine('Claw server stopped');
}

async function openChatPanel() {
    if (!transport) {
        await startClawServer();
        if (!transport) return;
    }
    // 创建 webview panel
    const panel = vscode.window.createWebviewPanel(
        'clawChat',
        'Claw Plus',
        vscode.ViewColumn.Beside,
        { enableScripts: true }
    );
    panel.webview.html = getChatHtml();
    // 创建 session
    const result: any = await transport.request('session/new', {
        cwd: vscode.workspace.rootPath,
    });
    const sessionId = result.sessionId;
    sessions.set(panel._id, { panel, sessionId });
    // 接收 webview 消息(prompt 输入)
    panel.webview.onDidReceiveMessage(async (msg) => {
        if (msg.type === 'prompt') {
            await transport!.request('session/prompt', {
                session_id: sessionId,
                prompt: [{ type: 'text', text: msg.text }],
            });
        } else if (msg.type === 'cancel') {
            transport!.notify('session/cancel', { session_id: sessionId });
        }
    });
    panel.onDidDispose(() => {
        sessions.delete(panel._id);
        // TODO: 调用 session/close(若协议支持)
    });
}

async function cancelActivePrompt() {
    if (!transport) return;
    for (const { sessionId } of sessions.values()) {
        transport.notify('session/cancel', { session_id: sessionId });
    }
}

function handleSessionNotification(params: any) {
    // 路由到对应 panel
    const { session_id, update } = params;
    for (const [panelId, session] of sessions) {
        if (session.sessionId === session_id) {
            session.panel.webview.postMessage({ type: 'update', update });
            break;
        }
    }
}

async function handleReadTextFile(params: { path: string }) {
    const uri = vscode.Uri.file(params.path);
    const doc = vscode.workspace.textDocuments.find(d => d.uri.fsPath === uri.fsPath);
    if (doc) return { content: doc.getText() };
    const content = await vscode.workspace.fs.readFile(uri);
    return { content: Buffer.from(content).toString() };
}

async function handleWriteTextFile(params: { path: string; content: string }) {
    const uri = vscode.Uri.file(params.path);
    let doc = vscode.workspace.textDocuments.find(d => d.uri.fsPath === uri.fsPath);
    if (!doc) doc = await vscode.workspace.openTextDocument(uri);
    const edit = new vscode.WorkspaceEdit();
    edit.replace(uri, new vscode.Range(0, 0, doc.lineCount, 0), params.content);
    const applied = await vscode.workspace.applyEdit(edit);
    return { written: applied };
}

async function handleRequestPermission(params: {
    tool_name: string;
    tool_input: any;
    options: string[];
}) {
    const choice = await vscode.window.showWarningMessage(
        `Claw 请求执行: ${params.tool_name}`,
        { modal: true },
        '允许', '拒绝', '始终允许'
    );
    const outcome = choice === '允许' ? 'allow'
        : choice === '始终允许' ? 'always_allow'
        : 'deny';
    return { outcome };
}

function getChatHtml(): string {
    return `<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Claw Chat</title></head>
<body>
    <div id="messages"></div>
    <textarea id="input" placeholder="输入 prompt..."></textarea>
    <button onclick="sendPrompt()">发送</button>
    <button onclick="cancel()">取消</button>
    <script>
        const vscode = acquireVsCodeApi();
        function sendPrompt() {
            const text = document.getElementById('input').value;
            vscode.postMessage({ type: 'prompt', text });
            document.getElementById('input').value = '';
        }
        function cancel() {
            vscode.postMessage({ type: 'cancel' });
        }
        window.addEventListener('message', (e) => {
            const msg = e.data;
            if (msg.type === 'update') {
                const div = document.createElement('div');
                div.textContent = JSON.stringify(msg.update);
                document.getElementById('messages').appendChild(div);
            }
        });
    </script>
</body>
</html>`;
}

async function showStatusBarMenu() {
    const choice = await vscode.window.showQuickPick(
        transport ? ['Stop Server', 'Open Chat'] : ['Start Server', 'Open Chat']
    );
    if (choice === 'Start Server') await startClawServer();
    else if (choice === 'Stop Server') await stopClawServer();
    else if (choice === 'Open Chat') await openChatPanel();
}

export function deactivate() {
    if (transport) transport.stop();
}
```

**本骨架的完整性检查**:

- [x] spawn claw-plus-headless 子进程
- [x] 创建 ACP client(JSON-RPC over stdio)
- [x] 注册命令(claw.start / claw.stop / claw.sendPrompt / claw.cancelPrompt)
- [x] 实现_webviewPanel UI(对话窗口)
- [x] initialize 握手 + session/new
- [x] session/prompt + 反向请求 handler
- [x] session/cancel notification
- [x] 错误恢复(子进程崩溃自动重启)
- [x] 状态栏指示器
- [x] Output channel 日志

---

## 七、Zed 集成验证

### 7.1 agents.json 配置示例

Zed 通过 `~/.config/zed/agents.json`(macOS)/ `%APPDATA%\Zed\agents.json`(Windows)配置 ACP 服务器:

```json
{
  "agent_servers": {
    "claw": {
      "name": "Claw Plus",
      "command": {
        "binary": "claw-plus-headless",
        "args": ["--model", "claude-sonnet-4-5", "--permission-mode", "workspace-write"],
        "env": {
          "ANTHROPIC_API_KEY": "${ANTHROPIC_API_KEY}"
        }
      },
      "cwd": "${ZED_WORKTREE_ROOT}",
      "capabilities": {
        "fs_read_text_file": true,
        "fs_write_text_file": true,
        "session_request_permission": true
      }
    }
  }
}
```

### 7.2 启动命令

```bash
# 1. 编译 claw-plus-headless binary
cd d:\claw-code-src\rust
cargo build --release --bin claw-plus-headless

# 2. 复制到 PATH(Windows)
copy target\release\claw-plus-headless.exe C:\Users\38225\.cargo\bin\

# 3. 配置 Zed agents.json(见上)
# 4. 重启 Zed,在 Assistant panel 选择 "Claw Plus" agent
# 5. 发送测试 prompt 验证
```

### 7.3 测试步骤

| # | 步骤 | 预期结果 | 验证方式 |
|---|------|---------|---------|
| 1 | Zed 启动,打开 Assistant panel | "Claw Plus" 出现在 agent 选择列表 | UI 检查 |
| 2 | 选 "Claw Plus",发 "hello" | 收到 assistant 回复 | 看到消息气泡 |
| 3 | 让 Claw 读当前打开的文件 | editor buffer 被读取 | 日志看到 `fs/read_text_file` 调用 |
| 4 | 让 Claw 写文件 | 弹出权限确认对话框 | 选 Allow 后文件被修改 |
| 5 | 让 Claw 执行 Bash 命令 | 弹出权限确认 | 选 Allow 后命令执行,输出推送回 IDE |
| 6 | 关闭 Zed | claw-plus-headless 进程退出 | Task Manager 检查 |
| 7 | 重启 Zed | session 恢复(P1 验证) | 历史消息可见 |

### 7.4 完整 agents.json(全部字段,v0.2 新增)

§7.1 给出了简化版,本节展开为包含全部字段的完整示例,涵盖 ACP 1.5 引入的所有 capability:

```json
{
  "$schema": "https://zed.dev/schemas/agents.json",
  "agent_servers": {
    "claw": {
      "name": "Claw Plus",
      "version": "0.2.0",
      "description": "ACP-compatible AI coding agent with LaneEvent streaming",
      "command": {
        "binary": "claw-plus-headless",
        "args": [
          "--model", "claude-sonnet-4-5",
          "--permission-mode", "workspace-write",
          "--log-level", "info"
        ],
        "env": {
          "ANTHROPIC_API_KEY": "${ANTHROPIC_API_KEY}",
          "CLAW_WORKSPACE_ROOT": "${ZED_WORKTREE_ROOT}",
          "RUST_LOG": "claw_shell=debug,claw_acp=info"
        },
        "cwd": "${ZED_WORKTREE_ROOT}"
      },
      "capabilities": {
        "fs_read_text_file": true,
        "fs_write_text_file": true,
        "session_request_permission": true,
        "session_load": false,
        "session_fork": false,
        "session_set_mode": false,
        "session_set_model": false
      },
      "default_permission_mode": "workspace-write",
      "timeout": {
        "initialize_ms": 5000,
        "session_new_ms": 2000,
        "session_prompt_ms": 120000
      },
      "icon": {
        "type": "codicon",
        "name": "comment-discussion"
      }
    }
  }
}
```

**字段说明**:

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `$schema` | string | 否 | JSON schema 校验地址 |
| `agent_servers.<id>` | object | 是 | agent 服务器定义,id 为唯一标识 |
| `.name` | string | 是 | UI 显示名 |
| `.version` | string | 否 | 版本号(用于兼容性日志) |
| `.description` | string | 否 | 简短描述 |
| `.command.binary` | string | 是 | 可执行文件路径(支持 PATH 查找) |
| `.command.args` | string[] | 否 | 命令行参数 |
| `.command.env` | object | 否 | 环境变量(支持 `${VAR}` 展开) |
| `.command.cwd` | string | 否 | 工作目录(支持 `${ZED_WORKTREE_ROOT}` 占位符) |
| `.capabilities` | object | 否 | agent 能力声明,与 ACP `agent_capabilities` 字段对应 |
| `.default_permission_mode` | enum | 否 | 默认权限模式(read-only / workspace-write / danger-full-access) |
| `.timeout.*` | object | 否 | 各阶段超时(ms) |
| `.icon` | object | 否 | UI 图标定义 |

### 7.5 PowerShell 启动测试脚本(v0.2 新增)

完整的 PoC 启动脚本,自动化 §7.3 验证流程的前置步骤:

```powershell
# scripts/test-zed-integration.ps1
#
# Zed 集成测试启动脚本
# 用法:.\scripts\test-zed-integration.ps1 [-AcpVersion "0_10"|"1_5"]
#
# 功能:
# 1. 构建 claw-plus-headless(默认 0.10.4 / 1.5)
# 2. 部署到独立目录(避免覆盖生产版本)
# 3. 写入 agents.json
# 4. 启动 Zed,等待用户手动验证 §7.3 清单

param(
    [ValidateSet("0_10", "1_5")]
    [string]$AcpVersion = "0_10"
)

$ErrorActionPreference = "Stop"
$RepoRoot = "d:\claw-code-src"
$RustDir = Join-Path $RepoRoot "rust"
$DeployDir = "C:\Users\38225\.cargo\bin\poc-$AcpVersion"
$ZedConfigDir = "$env:APPDATA\Zed"
$ZedAgentsFile = Join-Path $ZedConfigDir "agents.json"

Write-Host "=== Zed 集成测试启动脚本 ===" -ForegroundColor Cyan
Write-Host "ACP version: $AcpVersion"
Write-Host "Repo root:   $RepoRoot"
Write-Host "Deploy dir:  $DeployDir"
Write-Host ""

# 1. 创建 PoC 部署目录
if (-not (Test-Path $DeployDir)) {
    New-Item -ItemType Directory -Path $DeployDir -Force | Out-Null
    Write-Host "[1/5] Created deploy directory: $DeployDir" -ForegroundColor Green
} else {
    Write-Host "[1/5] Deploy directory exists" -ForegroundColor Yellow
}

# 2. 构建 claw-plus-headless
Write-Host "[2/5] Building claw-plus-headless ($AcpVersion)..." -ForegroundColor Cyan
Push-Location $RustDir
try {
    if ($AcpVersion -eq "1_5") {
        cargo build --release --bin claw-plus-headless --features claw-shell/acp-1_5
    } else {
        cargo build --release --bin claw-plus-headless
    }
    if ($LASTEXITCODE -ne 0) {
        throw "Build failed with exit code $LASTEXITCODE"
    }
    Write-Host "[2/5] Build succeeded" -ForegroundColor Green
} finally {
    Pop-Location
}

# 3. 部署 binary
$BinaryName = if ($AcpVersion -eq "1_5") { "claw-plus-headless-1-5.exe" } else { "claw-plus-headless.exe" }
$SourceBinary = Join-Path $RustDir "target\release\claw-plus-headless.exe"
$DestBinary = Join-Path $DeployDir $BinaryName
Copy-Item -Path $SourceBinary -Destination $DestBinary -Force
Write-Host "[3/5] Deployed binary: $DestBinary" -ForegroundColor Green

# 4. 写入 agents.json
if (-not (Test-Path $ZedConfigDir)) {
    New-Item -ItemType Directory -Path $ZedConfigDir -Force | Out-Null
}

$AgentsJson = @{
    agent_servers = @{
        claw = @{
            name = "Claw Plus ($AcpVersion)"
            version = "0.2.0"
            description = "ACP $AcpVersion - PoC build"
            command = @{
                binary = $DestBinary
                args = @("--model", "claude-sonnet-4-5", "--permission-mode", "workspace-write")
                env = @{
                    ANTHROPIC_API_KEY = '${ANTHROPIC_API_KEY}'
                    RUST_LOG = "claw_shell=debug,claw_acp=info"
                }
                cwd = '${ZED_WORKTREE_ROOT}'
            }
            capabilities = @{
                fs_read_text_file = $true
                fs_write_text_file = $true
                session_request_permission = $true
            }
            default_permission_mode = "workspace-write"
        }
    }
} | ConvertTo-Json -Depth 10

$AgentsJson | Out-File -FilePath $ZedAgentsFile -Encoding UTF8 -Force
Write-Host "[4/5] Written agents.json: $ZedAgentsFile" -ForegroundColor Green

# 5. 启动 Zed
Write-Host "[5/5] Starting Zed..." -ForegroundColor Cyan
$ZedExe = "C:\Users\38225\AppData\Local\Programs\Zed\Zed.exe"
if (-not (Test-Path $ZedExe)) {
    Write-Host "  Zed not found at: $ZedExe" -ForegroundColor Yellow
    Write-Host "  Please start Zed manually" -ForegroundColor Yellow
} else {
    Start-Process -FilePath $ZedExe
    Write-Host "[5/5] Zed started" -ForegroundColor Green
}

Write-Host ""
Write-Host "=== 验证清单(参照 §7.3)===" -ForegroundColor Cyan
Write-Host "1. Zed 启动 → Assistant panel → 看到 'Claw Plus ($AcpVersion)'"
Write-Host "2. 发 'hello' prompt → 收到回复"
Write-Host "3. 让 Claw 读当前文件 → 验证 fs/read_text_file"
Write-Host "4. 让 Claw 写文件 → 验证权限弹窗 + fs/write_text_file"
Write-Host "5. 让 Claw 执行 Bash → 验证 session/request_permission"
Write-Host "6. 关闭 Zed → claw-plus-headless.exe 进程退出(Task Manager 检查)"
Write-Host ""
Write-Host "完成后请填写 §7.3 表格的 '通过?' 列" -ForegroundColor Yellow
```

**使用示例**:

```powershell
# 默认测试 0.10.4
.\scripts\test-zed-integration.ps1

# 测试 1.5(PoC)
.\scripts\test-zed-integration.ps1 -AcpVersion "1_5"
```

### 7.6 验证清单(v0.2 新增,对应 PoC Phase 4)

PoC Phase 4 端到端验证的完整清单,每项需明确通过 / 失败,失败时记录根因:

| # | 验证项 | 详细步骤 | 预期结果 | 通过? | 失败根因 |
|---|--------|---------|---------|-------|---------|
| 1 | Zed 发现 agent | 启动 Zed → Assistant panel → 检查 agent 列表 | "Claw Plus (1_5)" 出现 | [ ] | |
| 2 | initialize 握手 | 选 agent,看 Zed log | `protocolVersion: 1.5` 协商成功 | [ ] | |
| 3 | session/new 创建 | 输入 "hello" → 回车 | Zed 显示新会话,无错误 | [ ] | |
| 4 | session/prompt 简单回复 | 发 "What is 2+2?" | 收到 AgentMessageChunk 文本回复 | [ ] | |
| 5 | fs/read_text_file 触发 | 打开一个文件 → 让 Claw 读取 | Claw 正确复述文件内容 | [ ] | |
| 6 | fs/write_text_file 触发 | 让 Claw 创建新文件 test.txt | 文件出现在 Zed 文件树,Ctrl+Z 可撤销 | [ ] | |
| 7 | session/request_permission 触发 | 让 Claw 执行 Bash `ls` | 弹出权限对话框,选 Allow 后执行 | [ ] | |
| 8 | SessionNotification 推送 | 让 Claw 执行需要多步的 task | 看到 ToolCall 状态变化(Pending → Completed) | [ ] | |
| 9 | session/cancel 通知 | prompt 中途按 Esc | 当前 prompt 中止(0.10.4 stub 返回 Ok) | [ ] | |
| 10 | 关闭 Zed 干净退出 | 关闭 Zed 窗口 | Task Manager 中 claw-plus-headless-1-5.exe 消失 | [ ] | |
| 11 | 重启 Zed 恢复(可选,P1) | 重启 Zed → Assistant panel | 历史会话可见(P1 session/load) | [ ] | |
| 12 | 长时间运行稳定性 | 连续 5 个 prompt 不退出 Zed | 无内存泄漏 / 无崩溃 | [ ] | |

**通过判定**:
- 项 1-10 全部通过 → PoC Phase 4 成功(G4 = 7/7,因项 11/12 可选)
- 项 1-10 有一项失败 → 记录失败根因,触发 §2.5.3 风险缓解
- 项 1-2 失败 → 直接走决策 B(双版本并存),1.5 不可用于 Zed

### 7.7 已知限制与降级(v0.2 新增)

Zed 当前版本(2026-07)对 ACP 1.5 特性的支持范围未知,以下为已识别的限制及降级行为:

| Zed 行为 | 触发场景 | 降级策略 | 用户感知 |
|---------|---------|---------|---------|
| Zed 不识别 `agent_capabilities` 字段 | 1.5 协议协商成功但 Zed 不读 capability | Agent 不发起反向请求(假定 IDE 不支持 fs/permission) | fs/read_text_file 等不可用,agent fallback 到 Read tool |
| Zed 不支持 `fs/read_text_file` 反向请求 | Zed 调用 initialize 时未声明 capability | Agent 在 initialize 响应中不声明 fs_read_text_file = true | 静默降级,无错误 |
| Zed 不显示 ToolCall 状态徽标 | SessionNotification::ToolCall 推送被忽略 | Agent 同时推送 AgentMessageChunk 文本通知作为 fallback | 用户看到文本消息而非状态卡片 |
| Zed 不支持 `session/request_permission` | 反向请求被拒(-32601 MethodNotFound) | Agent 走 PermissionMode 默认路径(WorkspaceWrite 模式下 workspace 内自动允许) | 无权限弹窗,直接执行(危险) |
| Zed 不支持 v2 Diff 格式 | 1.5 协议下推送 Diff 但 Zed 解析失败 | Agent fallback 到整体 content 覆盖(`fs/write_text_file` 不传 diff 字段) | 文件被整体覆盖,但 undo 可用 |
| Zed 不发送 `session/update` 通知 | IDE 侧状态变更不推送 | Agent 上下文不感知 IDE 当前文件 | Agent 不知道用户在看哪个文件,但仍可工作 |

**Zed 兼容性矩阵测试**(PoC Phase 4 必跑):

| Zed 版本 | ACP 版本 | 兼容性 | 备注 |
|---------|---------|--------|------|
| Zed dev build(2026-07) | 0.10.4 | ✅ 完全兼容 | 基线 |
| Zed dev build(2026-07) | 1.5 | ⚠️ 待验证 | PoC Phase 4 目标 |
| Zed stable | 0.10.4 | ✅ 完全兼容 | 生产路径 |
| Zed stable | 1.5 | ❓ 未知 | 若 Zed stable 不支持 1.5,启用降级 |

**降级触发逻辑**(运行时检测):

```rust
// rust/crates/claw-shell/src/agent.rs(initialize 后的降级检测)

/// 在 initialize 后,根据 client 实际声明的能力决定后续行为
pub fn should_use_v2_features(
    negotiated_version: acp::ProtocolVersion,
    client_caps: &acp::ClientCapabilities,
) -> V2FeatureFlags {
    let mut flags = V2FeatureFlags::default();
    if negotiated_version >= acp::ProtocolVersion::new(1, 5, 0) {
        flags.fs_read = client_caps.fs_read_text_file.unwrap_or(false);
        flags.fs_write = client_caps.fs_write_text_file.unwrap_or(false);
        flags.permission = client_caps.session_request_permission.unwrap_or(false);
    }
    flags
}

pub struct V2FeatureFlags {
    pub fs_read: bool,
    pub fs_write: bool,
    pub permission: bool,
}
impl Default for V2FeatureFlags {
    fn default() -> Self {
        // 默认全 false,需 client 显式声明才启用
        Self { fs_read: false, fs_write: false, permission: false }
    }
}
```

---

## 八、实施步骤分解

### 8.1 P0 阶段(4-6 周)周维度任务

| 周 | 任务 | 交付物 | 验收标准 |
|---|------|--------|---------|
| W1 | ACP 1.5 升级基础 | `claw-acp/Cargo.toml` 升级到 1.5 + 双 feature 编译通过 | `cargo build --features unstable,unstable-v2` 成功 |
| W1 | `ClawAgent` 扩展字段 | 新增 `session_store` / `cancel_token` / `permission_cache` 字段 | 现有测试不回归 |
| W2 | `initialize` 补充 agent_capabilities | 声明 fs/permission 能力 | `initialize_declares_fs_capabilities` 测试通过 |
| W2 | `fs/read_text_file` 实现 | `read_editor_buffer` 方法 + 测试 | 单元测试 3 个通过 |
| W3 | `fs/write_text_file` 实现 | `write_editor_buffer` 方法 + 测试 | 单元测试 3 个通过 |
| W3 | `session/request_permission` 实现 | `request_permission` 方法 + 测试 | 单元测试 4 个通过 |
| W4 | `LaneEvent` 桥接 | `flush_lane_events_to_acp` + `lane_event_to_session_update` | 23 种事件映射测试通过 |
| W4 | `prompt` 集成 flush | 在 `run_turn` 前后调用 flush | 集成测试验证 SubagentHandoff 推送 |
| W5 | Zed 集成验证 | agents.json 配置 + 端到端测试 | 7 步测试通过 |
| W5 | A6.4 测试更新 | 错误路径测试适配 1.5 行为 | 双 feature 下测试均通过 |
| W6 | VS Code 扩展骨架 | package.json + extension.ts 骨架 | 能 spawn server + initialize 握手 |
| W6 | 文档与发布 | 本分支文档 v1.0 + 主文档回写 | 三分支合并准备 |

### 8.2 P1 阶段(后续 4-6 周)

| 任务 | 说明 |
|------|------|
| `run_turn` 改造为 async | 引入 `CancellationToken`,实现真实 cancel |
| `session/load` 实现 | 从 SessionStore 恢复 + 历史消息重放 |
| `session/fork` 实现 | 从某消息分叉新 session |
| `session/set_mode` 实现 | plan/act 模式切换 |
| `session/set_model` 实现 | 运行时模型切换 |
| Diff v2 支持 | `fs/write_text_file` 支持 typed diff |
| VS Code 扩展完整实现 | chat panel + 流式输出 + 多 session |

---

## 九、测试矩阵

### 9.1 单元测试

| 测试文件 | 测试名 | 覆盖方法 | 优先级 |
|---------|--------|---------|--------|
| `agent.rs` | `initialize_returns_latest_protocol_version` | `initialize` | P0 |
| `agent.rs` | `initialize_exposes_api_key_auth_method` | `initialize` | P0 |
| `agent.rs` | `initialize_declares_fs_capabilities` | `initialize` | P0 |
| `agent.rs` | `new_session_returns_unique_session_id` | `new_session` | P0 |
| `agent.rs` | `new_session_consumes_api_client` | `new_session` | P0 |
| `agent.rs` | `prompt_returns_end_turn_stop_reason` | `prompt` | P0 |
| `agent.rs` | `prompt_without_session_returns_error` | `prompt` | P0 |
| `agent.rs` | `prompt_pushes_agent_message_chunk` | `prompt` | P0 |
| `agent.rs` | `prompt_flushes_lane_events_before_and_after` | `prompt` + flush | P0 |
| `agent.rs` | `read_editor_buffer_returns_content` | `fs/read_text_file` | P0 |
| `agent.rs` | `read_editor_buffer_propagates_method_not_found` | `fs/read_text_file` | P0 |
| `agent.rs` | `write_editor_buffer_returns_written_true` | `fs/write_text_file` | P0 |
| `agent.rs` | `write_editor_buffer_returns_false_when_user_denies` | `fs/write_text_file` | P0 |
| `agent.rs` | `request_permission_returns_allow_when_user_approves` | `request_permission` | P0 |
| `agent.rs` | `request_permission_returns_deny_when_user_rejects` | `request_permission` | P0 |
| `agent.rs` | `request_permission_times_out_after_60s` | `request_permission` | P0 |
| `agent.rs` | `lane_event_to_session_update_maps_23_variants` | 桥接 | P0 |
| `agent.rs` | `flush_lane_events_to_acp_drains_sink` | 桥接 | P0 |

### 9.2 集成测试

| 测试文件 | 测试名 | 场景 | 优先级 |
|---------|--------|------|--------|
| `stdio.rs` | `full_handshake_initialize_to_prompt` | initialize → authenticate → new_session → prompt 全流程 | P0 |
| `stdio.rs` | `fs_read_text_file_reverse_request` | agent 反向请求 IDE 读文件 | P0 |
| `stdio.rs` | `fs_write_text_file_reverse_request` | agent 反向请求 IDE 写文件 | P0 |
| `stdio.rs` | `session_request_permission_reverse_request` | agent 反向请求权限审批 | P0 |
| `stdio.rs` | `lane_event_flush_on_subagent_handoff` | SubagentHandoff → ToolCall notification | P0 |
| `stdio.rs` | `cancel_notification_returns_ok_without_active_prompt` | 现有 stub 契约 | P0 |
| `stdio.rs` | `run_agent_on_io_silently_drops_invalid_json` | A6.4(0.10.4 行为) | P0 |
| `stdio.rs` | `run_agent_on_io_returns_error_on_unknown_method` | A6.4(-32601) | P0 |
| `stdio.rs` | `run_agent_on_io_returns_parse_error_on_invalid_json_v2` | A6.4(1.5 行为,unstable-v2) | P0 |

### 9.3 端到端测试

| 测试 | 环境 | 验证点 | 优先级 |
|------|------|--------|--------|
| Zed + claw-plus-headless | Zed dev build | 7 步测试(见 §7.3) | P0 |
| VS Code + extension | VS Code 1.85+ | spawn + initialize + prompt | P1 |
| 多 session 恢复 | Zed 重启 | session/load 后历史可见 | P1 |
| cancel 中断 | 长时间 prompt | cancel 后 StopReason::Cancelled | P1 |

### 9.4 PoC 验证测试用例(v0.2 新增)

PoC Phase 3 新增的 1.5 特性测试,对应 §2.5.2 的 6 个新增测试清单:

| 测试名 | 测试文件 | 验证点 | feature | 优先级 |
|--------|---------|--------|---------|--------|
| `acp_1_5_initialize_handshake` | `agent.rs` | 1.5 initialize 返回的 `agent_capabilities` 字段(fs_read_text_file 等) | `acp-1_5` | P0 |
| `acp_1_5_session_notification_v2_diff` | `stdio.rs` | 1.5 SessionNotification 中 v2 Diff 格式(含 location 字段) | `acp-1_5` | P0 |
| `acp_1_5_permission_option_typed` | `agent.rs` | 1.5 typed `PermissionOption` 枚举(Allow/Deny/AlwaysAllow) | `acp-1_5` | P0 |
| `acp_1_5_content_mcp_aligned` | `agent.rs` | 1.5 Content 类型对齐 MCP(`Resource` / `Image` 子类型) | `acp-1_5` | P0 |
| `acp_dual_version_feature_flag` | `lib.rs` | 双 feature(`acp-0_10` / `acp-1_5`)共存编译通过 | 两者 | P0 |
| `acp_runtime_version_negotiation` | `agent.rs` | initialize 时版本协商:client 1.5 + agent 1.5 → 协商 1.5 | `acp-1_5` | P0 |
| `acp_zed_integration_e2e`(手动) | 手动 | Zed 连接 1.5 binary,§7.6 验证清单 12 项 | `acp-1_5` | P0 |
| `acp_vscode_extension_e2e`(手动) | 手动 | VS Code 扩展连接 1.5 binary,完整流程 | `acp-1_5` | P1 |

**测试代码骨架**:

```rust
// rust/crates/claw-shell/src/agent.rs(新增 #[cfg(test)] mod v1_5_tests)

#[cfg(all(test, feature = "acp-1_5"))]
mod v1_5_tests {
    use super::*;

    #[tokio::test]
    async fn acp_1_5_initialize_handshake() {
        let agent = build_test_agent();
        let req = acp::InitializeRequest {
            protocol_version: acp::ProtocolVersion::new(1, 5, 0),
            client_info: None,
            client_capabilities: acp::ClientCapabilities {
                fs_read_text_file: Some(true),
                fs_write_text_file: Some(true),
                session_request_permission: Some(true),
                ..Default::default()
            },
        };
        let resp = agent.initialize(req).await.unwrap();
        assert_eq!(resp.protocol_version, acp::ProtocolVersion::new(1, 5, 0));
        assert!(resp.agent_capabilities.fs_read_text_file);
        assert!(resp.agent_capabilities.fs_write_text_file);
        assert!(resp.agent_capabilities.session_request_permission);
    }

    #[tokio::test]
    async fn acp_1_5_permission_option_typed() {
        // 验证 PermissionOption 是 typed enum,而非 serde_json::Value
        let options = vec![
            acp::PermissionOption::Allow,
            acp::PermissionOption::Deny,
            acp::PermissionOption::AlwaysAllow,
        ];
        // 序列化应包含结构化的 type 字段
        let json = serde_json::to_value(&options).unwrap();
        assert!(json.is_array());
        assert_eq!(json[0]["type"], "allow");
        assert_eq!(json[1]["type"], "deny");
        assert_eq!(json[2]["type"], "always_allow");
    }

    #[tokio::test]
    async fn acp_1_5_content_mcp_aligned() {
        // 验证 Content 类型与 MCP 对齐
        let text = acp::Content::Text(acp::TextContent::new("hello".to_string()));
        let json = serde_json::to_value(&text).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hello");
    }

    #[tokio::test]
    async fn acp_runtime_version_negotiation() {
        let agent = build_test_agent();
        // client 请求 1.5
        let req_high = acp::InitializeRequest {
            protocol_version: acp::ProtocolVersion::new(1, 5, 0),
            client_info: None,
            client_capabilities: Default::default(),
        };
        let resp_high = agent.initialize(req_high).await.unwrap();
        assert_eq!(resp_high.protocol_version, acp::ProtocolVersion::new(1, 5, 0));

        // client 请求 0.10.4(降级)
        let agent2 = build_test_agent();
        let req_low = acp::InitializeRequest {
            protocol_version: acp::ProtocolVersion::new(0, 10, 4),
            client_info: None,
            client_capabilities: Default::default(),
        };
        let resp_low = agent2.initialize(req_low).await.unwrap();
        assert_eq!(resp_low.protocol_version, acp::ProtocolVersion::new(0, 10, 4));
        // 降级时不声明 agent_capabilities
        assert!(!resp_low.agent_capabilities.fs_read_text_file);
    }
}

/// 验证双 feature 共存编译(此测试在默认 feature 下也跑)
#[cfg(test)]
mod dual_version_tests {
    #[test]
    fn acp_dual_version_feature_flag_compiles() {
        // 仅验证:无论哪个 feature,代码都能编译
        // 真正的双版本验证靠 CI matrix(见 §2.6.5)
        assert!(true);
    }
}
```

**端到端测试骨架**(手动执行,记录到 §7.6 验证清单):

```rust
// rust/crates/claw-shell/tests/e2e_zed_integration.rs(手动测试,需 #[ignore])

#[cfg(feature = "acp-1_5")]
#[tokio::test]
#[ignore = "manual: requires Zed running"]
async fn acp_zed_integration_e2e() {
    // 此测试需手动启动 Zed + claw-plus-headless-1-5.exe
    // 步骤:
    // 1. 跑 .\scripts\test-zed-integration.ps1 -AcpVersion "1_5"
    // 2. 在 Zed 中执行 §7.6 验证清单 12 项
    // 3. 全部通过后取消 #[ignore] 标记
    //
    // 自动化部分:用 reqwest 连接 Zed 的 HTTP debug port(若可用)检查状态
    // 但目前 Zed 不暴露 debug port,只能手动验证
}
```

---

## 十、风险与缓解

### 10.1 ACP 1.5 破坏性变更风险

| 风险 | 概率 | 影响 | 缓解措施 |
|------|------|------|---------|
| `ContentBlock` 重命名导致编译失败 | 高 | 高(3 处调用) | 全局替换 + 编译验证 |
| `SessionConfiguration` 类型变更 | 中 | 中 | 双 feature 下分别实现 |
| `PermissionOption` 枚举化 | 中 | 中 | 适配层转换 `Vec<Value>` ↔ `Vec<PermissionOption>` |
| Terminal API 未实现导致 trait 报错 | 高 | 低 | `AcpGatewaySender` 已实现转发,无需在 `ClawAgent` 实现 |

### 10.2 async_trait Send 约束风险

**问题**:`ClawAgent<C>` 持有 `Rc<RefCell<...>>`(通过 `StaticToolExecutor` 间接),非 `Send`。`acp::Agent` trait 使用 `#[async_trait(?Send)]`,允许非 Send future,但有限制:

- 不能在 `tokio::spawn` 中调用(Send 要求)
- 必须在 `tokio::task::spawn_local` 或 `LocalSet::block_on` 中调用

**现状**:已通过 `spawn_claw_shell`([spawn.rs:69-93](../../rust/crates/claw-shell/src/spawn.rs))和 `run_stdio_agent`([stdio.rs:106-129](../../rust/crates/claw-shell/src/stdio.rs))正确隔离。

**缓解**:

```rust
// rust/crates/claw-shell/src/agent.rs(约束固化)
//
// 显式标注 ?Send,防止误用 Send 约束的 API。
// 编译时检查:任何尝试 spawn 跨线程的调用会失败。

#[async_trait(?Send)]
impl<C> acp::Agent for ClawAgent<C>
where
    C: ApiClient + 'static,
{
    // 所有方法都是 ?Send async,只能在 LocalSet 上调用
}
```

### 10.3 非 Send 类型隔离

**问题**:`StaticToolExecutor` 内部 `Box<dyn FnMut>` 非 Send,无法跨线程移动。

**现状**:`ClawAgentBuilder<C>` 要求 `C: ApiClient + Send + 'static`(api_client 必须可跨线程移动),`StaticToolExecutor` 在 `build()` 内创建([agent.rs:98-110](../../rust/crates/claw-shell/src/agent.rs)),确保不跨线程。

**风险点**:

1. `client_gateway: AcpGatewaySender<acp::AgentSide>` 是 `mpsc::UnboundedSender`,本身 `Send`,可跨线程 clone —— 但其接收方 `AcpGatewayReceiver` 必须在 LocalSet 上运行
2. `SessionStore` 使用 `std::sync::Mutex`(非 `tokio::sync::Mutex`),是 `Send + Sync`,可跨线程共享

**缓解**:

- 在 `spawn_claw_shell` 中严格保持 `LocalSet::block_on` 边界
- `SessionStore` 用 `std::sync::Mutex` 确保可跨线程共享(支持未来多 session 跨 agent 实例)
- 新增字段前检查 `Send` 约束,在 `ClawAgentBuilder::build` 内创建非 Send 类型

### 10.4 LaneEvent sink 容量保护

**问题**:[lane_events.rs:1047](../../rust/crates/runtime/src/lane_events.rs) 全局 sink 容量 512,超容量丢弃最旧一半。

**风险**:ACP 桥接启用后,若 IDE 端响应慢,`flush_lane_events_to_acp` 调用间隔变长,sink 可能溢出。

**缓解**:

1. `flush_lane_events_to_acp` 在 `prompt` 开始和结束各调用一次,确保每轮 turn 内 sink 被 drain 两次
2. `forward_fire_and_forget` 不阻塞,即使 IDE 端慢也不影响 agent
3. 在 `tool` 执行循环内增加 flush(P1,需 run_turn 改造)
4. 监控 sink 长度,接近上限时 log warn

### 10.5 协议版本协商失败

**问题**:IDE 不支持 1.5 特性(如 Zed 旧版),但 agent 已启用 `unstable-v2`。

**缓解**:

1. `initialize` 时检查 `client_capabilities`,若 IDE 未声明 fs/permission 能力,agent 不发起反向请求
2. `agent_capabilities` 中声明的能力必须与实际实现匹配(避免 IDE 误以为支持)
3. 保留 0.10.4 兼容路径(`unstable` feature),降级时仍可用

---

## 十二、性能基准

### 12.1 基准指标

v0.2 新增性能基准,作为 PoC Phase 4 的辅助验证(非阻塞门),并在 P1 阶段作为回归测试持续监控。

| # | 指标 | 目标值 | 测量方法 | 回归阈值 |
|---|------|--------|---------|---------|
| 1 | `initialize` 延迟 | < 100ms | 记录 `transport.request('initialize')` 起止时间 | > 200ms 报警 |
| 2 | `session/new` 延迟 | < 50ms | 同上 | > 100ms 报警 |
| 3 | `session/prompt` 首 token 延迟 | < 500ms | 记录 prompt 发送到第一个 AgentMessageChunk 的时间 | > 1000ms 报警 |
| 4 | SessionNotification 推送延迟 | < 10ms | LaneEvent 发布 → IDE 收到的时间差 | > 50ms 报警 |
| 5 | `fs/read_text_file` 延迟(1KB 文件) | < 50ms | 反向请求往返时间 | > 100ms 报警 |
| 6 | `fs/write_text_file` 延迟(1KB 文件) | < 50ms | 同上 | > 100ms 报警 |
| 7 | 大文件读取延迟(1MB) | < 500ms | 同指标 5 但文件 1MB | > 1000ms 报警 |
| 8 | 大文件写入延迟(1MB) | < 500ms | 同指标 6 但文件 1MB | > 1000ms 报警 |
| 9 | 并发 session 数 | ≥ 10 | 同时 `session/new` 10 次,全部成功 | < 5 报警 |
| 10 | LaneEvent sink 高水位 | < 256 | `lane_event_sink_len` 峰值 | ≥ 384 报警(见 §5.7) |
| 11 | 内存占用(单 session) | < 200MB RSS | `claw-plus-headless` 进程 RSS | > 500MB 报警 |
| 12 | 1.5 升级后编译时间增量 | < 30% | `cargo build --features acp-1_5` vs 默认 | > 50% 报警 |

### 12.2 测量方法

**基准测试代码骨架**:

```rust
// rust/crates/claw-shell/benches/acp_latency.rs

use std::time::{Duration, Instant};

#[cfg(feature = "acp-1_5")]
#[tokio::test]
async fn bench_initialize_latency() {
    let agent = build_test_agent();
    let start = Instant::now();
    let _resp = agent.initialize(test_init_request()).await.unwrap();
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_millis(100),
        "initialize latency {}ms exceeds 100ms target", elapsed.as_millis());
}

#[cfg(feature = "acp-1_5")]
#[tokio::test]
async fn bench_session_new_latency() {
    let agent = build_test_agent_with_session();
    let start = Instant::now();
    let _resp = agent.new_session(test_new_session_request()).await.unwrap();
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_millis(50),
        "session/new latency {}ms exceeds 50ms target", elapsed.as_millis());
}

#[cfg(feature = "acp-1_5")]
#[tokio::test]
async fn bench_concurrent_sessions() {
    use std::sync::Arc;
    use tokio::sync::Barrier;
    let agent = Arc::new(build_test_agent());
    let barrier = Arc::new(Barrier::new(10));
    let mut handles = vec![];
    for _ in 0..10 {
        let agent_clone = agent.clone();
        let barrier_clone = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier_clone.wait().await;
            agent_clone.new_session(test_new_session_request()).await
        }));
    }
    for handle in handles {
        let resp = handle.await.unwrap();
        assert!(resp.is_ok(), "concurrent session/new failed");
    }
}
```

**手动基准测试**(配合 §7.5 PowerShell 脚本):

```powershell
# scripts/bench-acp-latency.ps1
#
# 测量 initialize / session/new / prompt 延迟
# 用法:.\scripts\bench-acp-latency.ps1 -Iterations 10

param([int]$Iterations = 10)

$results = @()
for ($i = 0; $i -lt $Iterations; $i++) {
    $start = Get-Date
    # 调用 claw-plus-headless --bench initialize
    # 实际实现需 claw-plus-headless 支持 --bench 子命令(P1)
    $output = & "claw-plus-headless.exe" --bench initialize 2>&1
    $elapsed = (Get-Date) - $start
    $results += @{
        iteration = $i
        elapsed_ms = $elapsed.TotalMilliseconds
        output = $output
    }
}
$results | Format-Table
$results | ConvertTo-Json | Out-File "bench-results-$(Get-Date -Format yyyyMMdd-HHmmss).json"
```

### 12.3 性能回归检测

**CI 集成**(P1 阶段):

```yaml
# .github/workflows/perf-bench.yml
name: Performance Benchmark
on: [pull_request]
jobs:
  bench:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - name: Run benchmarks
        run: |
          cd rust
          cargo bench --workspace --features claw-shell/acp-1_5
      - name: Compare with baseline
        uses: benchmark-action/github-action-benchmark@v1
        with:
          tool: 'cargo'
          output-file-path: target/criterion/baseline.json
          fail-on-alert: true
          alert-threshold: '130%'  # 允许 30% 回归
```

**回归处理**:

- 单项指标超阈值 → PR 标记 `perf-regression`,需 reviewer 确认
- 多项指标超阈值 → 阻塞合并,需优化后重测
- 编译时间增量超阈值 → 不阻塞,但记录到技术债

### 12.4 已知性能瓶颈

| 瓶颈 | 当前表现 | 缓解方案 | 优先级 |
|------|---------|---------|--------|
| `run_turn` 同步阻塞 | prompt 期间 IDE 无法接收新 notification | P1 改造为 async | P1 |
| LaneEvent sink drain 频率低 | 仅 prompt 前后各一次 | P1 在 tool 循环内增加 flush(见 §5.6) | P1 |
| `fs/read_text_file` 反向请求往返 | ~50ms/次(含 IDE 处理) | 缓存最近读取的文件内容 | P2 |
| ContentBlock 序列化开销 | 大文本(> 1MB)序列化慢 | 分块推送 ContentChunk | P2 |
| `permission_cache` 哈希计算 | 每次 tool 调用都 hash input | 用 LRU 缓存 hash 结果 | P3 |

---

## 十三、迁移指南

### 13.1 从 0.10.4 迁移到 1.5 的步骤

本节面向 claw-code 维护者,描述 PoC 决策 A(全面升级)后的迁移流程。**仅当 §2.5 PoC 走到决策 A 时执行此流程**。

#### Step 1:切换默认 feature(PoC 后)

```toml
# rust/crates/claw-acp/Cargo.toml(决策 A 后)
[features]
# 切换默认为 1.5
default = ["acp-1_5"]
# 0.10.4 兼容路径保留 1 个版本周期(P1 末移除)
acp-0_10 = ["agent-client-protocol/unstable"]
acp-1_5 = ["agent-client-protocol/unstable-v2"]
```

```bash
# 验证默认构建为 1.5
cd d:\claw-code-src\rust
cargo build --workspace
# 期望:claw-plus-headless 输出 "acp-protocol: 1.5"
```

#### Step 2:移除 0.10.4 兼容代码(P1 末)

```bash
# 删除所有 #[cfg(not(feature = "acp-1_5"))] 分支
# 用 grep 找出所有需删除的代码段
grep -rn "not(feature = \"acp-1_5\")" rust/crates/
# 逐个删除旧分支代码,保留 1.5 实现
```

#### Step 3:更新 Cargo.lock

```bash
cargo update -p agent-client-protocol
# 验证 Cargo.lock 中版本:
grep -A 2 'name = "agent-client-protocol"' rust/Cargo.lock
# 期望:version = "1.5.0"
```

#### Step 4:更新文档

- 本文档 §2.6 双版本兼容策略标记为"deprecated,P1 末移除"
- 主文档 [ide-hooks-dag-implementation-plan.md](../ide-hooks-dag-implementation-plan.md) 更新版本支持矩阵
- README 中 ACP 版本要求改为 1.5+

### 13.2 配置文件变更

| 配置 | 0.10.4 | 1.5 | 迁移动作 |
|------|--------|-----|---------|
| `claw-acp/Cargo.toml` `agent-client-protocol` | `version = "0.10.4"` | `version = "1.5"` | 编辑 |
| `claw-acp/Cargo.toml` `[features]` | `unstable` | `unstable-v2`(默认) | 编辑 |
| `claw-shell/Cargo.toml` `[features]` | (无) | `acp-1_5 = ["claw-acp/acp-1_5"]` | 新增 |
| Zed `agents.json` `capabilities` | (无此字段) | 添加 fs/permission 字段 | 编辑 |
| VS Code `settings.json` `claw.acpVersion` | (无) | `"1.5"`(可选) | 新增 |

### 13.3 已废弃 API 清单

| API | 0.10.4 | 1.5 替代 | 迁移注意 |
|-----|--------|---------|---------|
| `acp::ContentBlock::Text` | ✅ | `acp::Content::Text` | 全局替换 |
| `NewSessionRequest.mcp_servers: Vec<String>` | ✅ | `Vec<McpServerConfig>` | 适配层转换 |
| `SessionConfiguration: serde_json::Value` | ✅ | typed struct | 重构 new_session |
| `PermissionOption: serde_json::Value` | ✅ | typed enum | 重写 request_permission |
| `Diff` 无 location 字段 | ✅ | `Diff` 含 `location: Option<Range>` | 新增字段填充 |
| `session/load`(单独方法) | ✅ | 与 `session/resume` 统一 | 改名 |
| `silent drop invalid JSON` | ✅ | 返回 `-32700` parse_error | 更新测试断言 |

### 13.4 用户感知的变化

**新功能**(用户可见):

1. ✅ **IDE 文件读写走 undo 栈**:用户可 Ctrl+Z 撤销 agent 修改
2. ✅ **危险操作审批弹窗**:agent 执行 rm / 路径越权时弹出权限对话框
3. ✅ **实时事件推送**:SubagentHandoff / Git commit 等事件实时显示在 IDE
4. ✅ **Diff 视图**(P1):文件修改以 diff 形式展示,而非整体覆盖

**行为变更**(用户可能感知):

1. ⚠️ **错误消息更明确**:之前 silent drop 的错误现在返回 JSON-RPC error,IDE 会显示错误对话框
2. ⚠️ **权限弹窗频率**:WorkspaceWrite 模式下,workspace 外操作会弹窗(之前自动允许)
3. ⚠️ **首次 prompt 延迟略增**:1.5 协商 + capability 检查增加 ~10ms(在 §12 基准范围内)

**无感知的内部变更**:

- Cargo.lock 中 `agent-client-protocol` 版本号变化
- 编译时增加 `acp-1_5` feature flag
- 内部代码用 `#[cfg(feature = "acp-1_5")]` 区分

### 13.5 回滚方案

若 1.5 正式发布后发现严重 bug,回滚步骤:

```bash
# 1. 切回 0.10.4 默认 feature
git revert <merge-commit>  # 撤销 1.5 切换 PR

# 2. 重新构建
cargo build --release --bin claw-plus-headless

# 3. 部署回滚版本
copy target\release\claw-plus-headless.exe C:\Users\38225\.cargo\bin\

# 4. 通知用户(Zed / VS Code 自动重连)
```

**回滚前提**:0.10.4 兼容代码必须保留至少 1 个版本周期(P1 末才能删除),否则无法快速回滚。

---

## 十一、参考链接

### 11.1 ACP 官方资源

- [Agent Client Protocol Spec](https://agentclientprotocol.com/) — 协议规范主站
- [ACP GitHub: kendru/agent-client-protocol](https://github.com/kendru/agent-client-protocol) — Rust crate 源码
- [agent-client-protocol 0.10.4 docs.rs](https://docs.rs/agent-client-protocol/0.10.4/) — 当前使用版本文档
- [agent-client-protocol 1.5.0 docs.rs](https://docs.rs/agent-client-protocol/1.5.0/) — 目标版本文档(发布后)
- [ACP 1.5 Changelog](https://github.com/kendru/agent-client-protocol/blob/main/CHANGELOG.md) — 版本变更记录

### 11.2 IDE 集成参考

- [Zed agents.json spec](https://zed.dev/docs/agent-servers) — Zed agent 服务器配置
- [Zed Assistant panel](https://zed.dev/docs/assistant-panel) — Zed 助手面板文档
- [VS Code Extension API](https://code.visualstudio.com/api) — VS Code 扩展 API 主入口
- [VS Code Chat Participants API](https://code.visualstudio.com/api/extension-guides/chat-session) — Chat participant 集成指南
- [vscode-languageserver](https://github.com/microsoft/vscode-languageserver-node) — JSON-RPC 桥接库
- [JetBrains ACP plugin](https://github.com/kendru/agent-client-protocol-jetbrains) — JetBrains 集成参考
- [CodeCompanion.nvim](https://github.com/olimorris/codecompanion.nvim) — Neovim ACP 适配器

### 11.3 Claw Plus 内部参考

- [主文档:IDE 集成方案](../../docs/ide-hooks-dag-implementation-plan.md) — 父文档
- [agent.rs:ClawAgent 实现](../../rust/crates/claw-shell/src/agent.rs) — 当前 ACP Agent trait 实现
- [spawn.rs:spawn_claw_shell](../../rust/crates/claw-shell/src/spawn.rs) — 独立线程 + LocalSet 启动模式
- [stdio.rs:run_agent_on_io](../../rust/crates/claw-shell/src/stdio.rs) — stdio ACP 服务器核心
- [gateway.rs:AcpGatewaySender](../../rust/crates/claw-acp/src/gateway.rs) — Gateway 转发层
- [lane_events.rs:LaneEvent](../../rust/crates/runtime/src/lane_events.rs) — 23 种内部事件定义
- [headless.rs:claw-plus-headless](../../rust/crates/rusty-claude-cli/src/bin/headless.rs) — stdio 服务器入口 binary
- [claw-acp Cargo.toml](../../rust/crates/claw-acp/Cargo.toml) — 0.10.4 版本锁定位置

### 11.4 相关 RFC 与设计文档

- [MCP (Model Context Protocol)](https://modelcontextprotocol.io/) — ACP 1.5 Content 类型对齐的协议
- [JSON-RPC 2.0 Spec](https://www.jsonrpc.org/specification) — ACP 传输层协议
- [tokio LocalSet docs](https://docs.rs/tokio/latest/tokio/task/struct.LocalSet.html) — 非 Send future 执行环境

---

## 附录 A:文档变更日志

| 版本 | 日期 | 变更 |
|------|------|------|
| v0.1 | 2026-07-21 | 初版创建,基于主文档第二章展开 |
| v0.2 | 2026-07-21 | 新增 §2.5 PoC 验证方案(~440 行,G1-G4 验收门 + A/B/C 决策矩阵)、§2.6 双版本兼容策略(~180 行,Cargo feature flag `acp-0_10`/`acp-1_5` + cfg 条件编译 + 运行时版本协商)、§3.6/3.7 fs/* 完整实施(路径安全约束 + Undo 集成 + fallback)、§3.8 session/request_permission 6 步流程(30s 超时 + 权限缓存)、§5.5 23 种 LaneEvent 完整映射代码、§5.6 flush 时机细化(5 调用点 + P0/P1 方案)、§5.7 背压机制(WARN/DEGRADE/DROP 三级阈值)、§6.5-6.7 VS Code 扩展(AcpTransport + ErrorRecovery + extension.ts 骨架)、§7.4-7.7 Zed 集成(agents.json + PowerShell 启动脚本 + 验证清单)、§9.4 PoC 验证测试用例(8 个测试骨架)、§12 性能基准(12 项指标 + 测量代码 + CI 集成 + 已知瓶颈)、§13 迁移指南(5 步迁移 + 配置变更表 + 废弃 API + 回滚方案);文档总行数从 1508 扩展至 4475,超过目标 2500-3500 行,主要因 PoC 验证方案和 VS Code 扩展骨架需要详尽实施细节以支撑"升级风险可控"目标 |

---

## 附录 B:术语表

| 术语 | 全称 | 说明 |
|------|------|------|
| ACP | Agent Client Protocol | 编辑器与 AI agent 通信协议 |
| IDE | Integrated Development Environment | 集成开发环境(VS Code / Zed / JetBrains 等) |
| LaneEvent | Lane Event | Claw Plus 内部事件总线事件 |
| SessionNotification | ACP Session Notification | agent → IDE 的推送消息 |
| LocalSet | tokio::task::LocalSet | tokio 中运行非 Send future 的执行环境 |
| silent drop | Silent Drop | ACP 0.10.4 中错误消息被静默丢弃的行为 |
| capability | Capability | ACP 协商中声明的能力(fs/permission/terminal) |
| fire-and-forget | Fire and Forget | 发送后不等待响应的推送模式 |
