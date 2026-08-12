# 目录层级控制（Dir Hierarchy Control）设计文档

> **持久化文档** — 本方案固化到文件以绕过会话上下文限制。所有改动点带 `文件:行号` 锚定，便于 subagent 独立执行时无需重新调研。
>
> 状态：Epic 0 已实施（2026-08-11） · Epic 1 已实施 · Epic 2 已实施（2026-08-12） · Epic 3 已实施（2026-08-12）

**Goal:** 让 CLAW 按目录层级实现"高层控制低层"——根目录的父会话可发现子工作区、向子目录派发受控子代理、约束子代理只读写自己的目录子树、回收其结构化结果，并让子目录配置继承父级。

**Architecture:** 新增 `subworkspace` 模块负责目录发现（复用 `is_project_dir` 递归向下，跳过 `.git/target/node_modules/.claw`）；新增工具 `dispatch_dir_agent`（在 `dispatch_subagent` 基础上扩展 `workspace` 字段）绑定子目录 cwd 生成受控子代理，用 `WorkspacePathScope` 硬约束越界；结果经 `SubagentHandoff` 写回 `{dir}/.claw/subagents/{id}.md`，父会话解析 frontmatter summary；`ConfigLoader::discover` 增加 ancestor walk 实现父子配置合并。

**Tech Stack:** Rust, tokio, serde_json, 现有 `multi_agent/` + `file_ops.rs::WorkspacePathScope` + `claw-shell::spawn_claw_shell`。

---

## 1. 现状与差距

### 1.1 已有的"层级"能力（都不是控制链）

| 能力 | 实现位置 | 说明 |
|---|---|---|
| 配置优先级 | [config.rs:410-444](file:///d:/claw-code-src/rust/crates/runtime/src/config.rs#L410-L444) `ConfigLoader::discover` | 仅 user / project(cwd) / local 三层，**基于单一 cwd，无 ancestor walk** |
| 多根路径权限 | [file_ops.rs:192-201](file:///d:/claw-code-src/rust/crates/runtime/src/file_ops.rs#L192-L201) `WorkspacePathScope` | 多根白名单校验（`--add-dir`），是约束而非控制链 |
| 子代理目录隔离 | [multi_agent/mod.rs:363-446](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/mod.rs#L363-L446) `CoordinationMode::Worktree` | 独立 git worktree 防文件冲突，但不绑定子目录 cwd |
| 目录检测 | [project_picker.rs:47-63](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/tui-ports/project_picker.rs#L47-L63) `is_project_dir` | 判单目录是否为项目根，未做树状发现 |
| 拓扑图 | [project_topology.rs](file:///d:/claw-code-src/rust/crates/runtime/src/project_topology.rs) `ModuleGraph`（cargo metadata） | crate 依赖图，**非目录控制层级** |
| monorepo 靶场 | [demo-monorepo](file:///d:/claw-code-src/demo-monorepo)（`crates/{api,app,core,utils}` 多 crate + 根 `.claw/settings.json`） | 验证扫描/派发的最现成样例 |

### 1.2 已有派发链路（可直接扩展）

| 环节 | 实现位置 | 复用点 |
|---|---|---|
| 派发入口 | [conversation.rs:3661](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3661) `execute_dispatch_subagent_async` | 扩展 input 加 `workspace` 字段 |
| 子代理 LLM 执行 | [conversation.rs:4129](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L4129) `run_subagent_turn_with_model` | 传子目录 cwd 的 workspace |
| 子代理进程 spawn | [spawn.rs](file:///d:/claw-code-src/rust/crates/claw-shell/src/spawn.rs) `spawn_claw_shell`（im-bridge 已用同一模式 [session.rs:395](file:///d:/claw-code-src/rust/crates/im-bridge/src/session.rs#L395-L436)） | 绑定 cwd 生成独立会话 |
| 结果回收 | [handoff.rs](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/handoff.rs) `SubagentHandoff`（YAML frontmatter：summary ≤500 字符进主上下文，details 按需读） | 子代理产出 → 父会话 |
| 并发写保护 | [file_guard.rs](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/file_guard.rs) | 父子并发改同一文件防冲突 |
| 循环防护 | [loop_detection.rs](file:///d:/claw-code-src/rust/crates/runtime/src/loop_detection.rs) | 禁止子代理递归派发到父级 |

### 1.3 差距清单

1. **无目录发现**：没有"从根向下找出所有子工作区"的机制。
2. **无目录绑定派发**：`dispatch_subagent` 不支持 `workspace`，子代理永远跑在父 cwd。
3. **无父子路径硬约束**：子代理没有"只能写自己子树"的强制边界（`Worktree` 只隔离，不限定读写范围）。
4. **无配置继承**：子目录 `.claw.json` 与父级不合并。
5. **无子代理生命周期可见性**：父无法 /resume、steer、kill 绑定子目录的子代理。

---

## 2. 架构设计

### 2.1 目录发现：`subworkspace` 模块（runtime 新增 `src/subworkspace.rs`）

```rust
/// 子工作区条目。
pub struct Subworkspace {
    pub path: PathBuf,
    pub relative_path: String,      // "crates/api"
    pub markers: Vec<String>,       // Cargo.toml / package.json / ...
    pub has_own_config: bool,       // 存在 .claw.json 或 .claw/settings.json
    pub depth: usize,
}

/// 从 workspace_root 递归向下发现子工作区。
///
/// - 判定复用 `project_picker::is_project_dir` 的标记集合
///   （.git / Cargo.toml / package.json / pyproject.toml / go.mod / pom.xml / build.gradle*）。
/// - 跳过目录：`.git` / `target` / `node_modules` / `.claw`（避免把内部状态目录当工作区）。
/// - 默认最大深度 4，可配置。
/// - 结果缓存到 `.claw/subworkspaces.json`（沿用 topology.json 懒加载模式），
///   首次调用后台线程构建（沿用 [project_topology.rs](file:///d:/claw-code-src/rust/crates/runtime/src/project_topology.rs) 的 `ensure_built()` 异步模式）。
pub fn discover_subworkspaces(workspace_root: &Path) -> Result<Vec<Subworkspace>, String>;
```

工具描述（供 LLM 使用）：`发现子工作区。返回路径/标记/是否有独立配置，供按目录派发任务。若正在构建，不要立刻重试。`

### 2.2 目录绑定派发：`dispatch_dir_agent`

在 `execute_dispatch_subagent_async`（[conversation.rs:3661](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L3661)）的 input 中增加可选字段 `workspace`（相对路径），或独立工具 `dispatch_dir_agent`。推荐**扩展既有工具**（向后兼容：缺省 = 当前 cwd，行为不变）：

```json
{
  "name": "dispatch_subagent",
  "input": {
    "name": "api-worker",
    "task": "重构 auth 模块",
    "mode": "fork",
    "workspace": "crates/api"      // 新增：缺省 = 父会话 cwd
  }
}
```

执行流程：
1. **校验**：`workspace` 解析后必须落在父 `WorkspacePathScope` 根内，且已存在于 `discover_subworkspaces()` 结果中；否则拒绝（`invalid_workspace`）。
2. **spawn**：用 `spawn_claw_shell`（[spawn.rs](file:///d:/claw-code-src/rust/crates/claw-shell/src/spawn.rs)）以 `cwd = {workspace}` 生成独立会话（复用 im-bridge [session.rs:395](file:///d:/claw-code-src/rust/crates/im-bridge/src/session.rs#L395-L436) 的握手模式：initialize → authenticate → new_session(cwd)）。
3. **硬约束**：子代理上下文注入 `WorkspacePathScope::from_roots([workspace])`（[file_ops.rs:192-201](file:///d:/claw-code-src/rust/crates/runtime/src/file_ops.rs#L192-L201)），`permission_enforcer` 对越界读写一律拒绝（读取工具 / edit_file / write_file / bash 统一走该 scope）。
4. **能力分级**：capability 沿用 `SubagentCapability`（Analyze/ReadOnly/Execute，[multi_agent/mod.rs](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/mod.rs) 已定义）。Epic 0 仅允许 `ReadOnly`。
5. **结果回收**：完成后 `SubagentHandoff` 写 `{workspace}/.claw/subagents/{id}.md`；父会话解析 frontmatter `summary` + `changed_files` 进上下文，`details` 按需 Read（[handoff.rs](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/handoff.rs) 现有机制，无需改动）。
6. **防递归**：子代理工具白名单不含 `dispatch_subagent`（现有 guard 已禁止递归派发），追加防"派发到父级目录"的检查（loop_detection）。

### 2.3 配置继承（ancestor walk）

[config.rs:410-444](file:///d:/claw-code-src/rust/crates/runtime/src/config.rs#L410-L444) `discover()` 扩展：当 cwd 位于某子工作区且检测到祖先 `.claw.json` 时，额外收集祖先配置，合并顺序 **祖先 → 子目录（后者覆盖）**：

```text
~/.claw.json
~/.config/claw/settings.json
<repo>/.claw.json            ← 祖先（父级，低优先级）
<repo>/.claw/settings.json
<subdir>/.claw.json          ← 子目录（高优先级）
<subdir>/.claw/settings.json
<subdir>/.claw/settings.local.json
```

规则：
- 祖先发现：从子目录向上逐级找 `.claw.json`，最近一级为父。
- `settings.local.json` 例外：永远只读本地，不参与继承合并（防机器级泄漏）。
- 合并逻辑复用现有 `ConfigEntry` 优先级机制，不改动合并算法本身。

### 2.4 子代理生命周期可见性

- 父侧记录 `subagent_id → workspace` 映射（写 `.claw/subagents/manifest.json`）。
- `/subagent list`（现有命令）显示每个子代理的 `workspace` 字段。
- `/subagent steer <id> <msg>` / `kill <id>` 保持现有通道，额外校验目标 workspace 权限。

---

## 3. 改动清单（文件级）

| 文件 | 改动 |
|---|---|
| `runtime/src/subworkspace.rs` | **新建**：`discover_subworkspaces` + 缓存 + 异步构建 |
| `runtime/src/lib.rs` | re-export `subworkspace` 模块 |
| `runtime/src/conversation.rs` | `execute_dispatch_subagent_async` 解析 `workspace` 字段 + 绑定 cwd + 注入子目录 `WorkspacePathScope` |
| `runtime/src/file_ops.rs` | `WorkspacePathScope::from_roots` 已在；新增从相对路径解析校验辅助函数 |
| `runtime/src/config.rs` | `discover()` ancestor walk + 合并顺序 |
| `rusty-claude-cli/src/commands_handler.rs` | `/subagent` 列表显示 workspace 字段 |
| `runtime/src/loop_detection.rs` | 防"子代理派发到父级目录"检查 |

---

## 4. 实施阶段（Pilot 优先）

### Epic 0（Pilot，ReadOnly，零写风险）

**范围**：`subworkspace` 目录发现 + `dispatch_subagent` 支持 `workspace` 字段 + 子代理 `ReadOnly` 能力 + 越界拦截 + handoff 回收。

**验证目标（靶场 [demo-monorepo](file:///d:/claw-code-src/demo-monorepo)）**：
1. 在根目录运行 `/subagent list` 前先 `dispatch_subagent {workspace: "crates/api", task: "分析 api crate 的公开接口并总结"}`。
2. 子代理只读 `crates/api/`，handoff 摘要回到父会话。
3. 尝试让子代理读 `crates/core` 或父目录文件 → 被 `WorkspacePathScope` 拒绝。
4. `discover_subworkspaces` 返回 4 个 crate，忽略 `.git/target/.claw`。

改动面：`subworkspace.rs` + `conversation.rs`（workspace 解析 + scope 注入）+ `file_ops.rs` 辅助 + 测试。

### Epic 1（Execute + 并发保护 + 目录视角收敛 + 执行链统一）

**待办任务**（[ ] 未开始 / [x] 已完成）：

- [x] **T1 允许 `capability=Execute`**：子代理可写 `{workspace}` 内文件（放开 [conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs) 的 `execute_subagent_llm` 能力约束）。完成：Guard 2.5 封 bash 逃逸（scope 子代理禁 bash）+ spec 暴露 `workspace`/capability 注明 bash 约束 + workspace 绑定 Execute 写文件成功/越界拒绝测试（`process_tool_uses_*` 10 个通过）。
- [x] **T2 父子并发写保护**：子代理写路径先过 `file_guard.rs` 锁；默认仍推荐 `mode=worktree` 隔离。完成：子代理间互锁已有（进程级注册表 + per-file 锁 + 30s 超时）；补主 agent（父会话）写路径加锁（[conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs) 外部工具分支对 `write_file`/`edit_file` 过 `SubagentFileGuard`，锁失败回填 is_error 不执行）；新增 `process_tool_uses_write_conflicts_with_parent_lock` 测试。**依赖 T4**：cwd 切换后子代理传相对 workspace 路径，`normalize_path` 的锁 key 需改为 scope 感知，否则父子锁 key 错位。
- [x] **T3 `changed_files` 喂 validation gate**：子代理写改动回归到验证链（沿用 [validation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/validation.rs)）。完成：`detect_changed_files` 合并 untracked（`git ls-files --others` 补 `git diff --name-only HEAD`，T1 Execute 新建文件不再跳过验证链）；`compute_changed_files_mismatch` 路径归一化（声称绝对路径 strip workspace_root 前缀后与 git diff 相对路径比对）；`CommandValidationGate` 触发补充 `subagent_changed_files`（声称改了也触发）；4 个新测试。
- [x] **T4 子代理 cwd/scope 视角切换**（原限制"子代理 cwd 未切换"）：`run_subagent_turn_with_model`/`build_subagent_context` 中，当 `workspace_override` 存在时，`project_context.cwd` 设为子目录路径、`ProjectContext::instruction_files` 从子目录 `.claw` 收集，使 LLM 路径基准视角与工具执行基准一致。完成：**Guard 3 双基准**（主 root 相对候选 `workspace_root.join(target)` + scope 相对候选 `scope_root.join(target)`，scope 候选条件启用——target 第一组件非主 root 顶层目录，防 `../` 归一化后误落 scope 内的 P0 越界；任一过 lexical+canonicalize 双校验即放行）；**落位改写**：scope 相对采用时 `rewrite_path_to_workspace_relative` 将 input 中 file_path/path 改写为主 root 相对；**file lock 用 `resolved_abs`**（候选绝对路径，避免 cwd 切换后锁 key 错位，T2 依赖项解除）；**Windows verbatim 修复**：`normalize_lexical` 剥离 `\\?\` 前缀防 `strip_prefix` 失败；新增 4 测试（cwd 切换断言 / scope 相对写改写语义 / scope 相对越界拒绝 / rewrite helper），P0 回归测试恢复通过。回归：dispatch 45 + runtime 全量 1671 通过。
- [x] **T5 TOCTOU 缓解**：派发时 `resolve_subworkspace` 校验的目录在子代理执行期间可能变化（新增/删除子目录），缓存导致 false-negative（安全方向，可接受）。执行前对 `workspace` 目录重新 `canonicalize` 校验（校验点从派发处迁移到子代理 turn 开始处），行为预期保持"false-negative 安全方向可接受"并在文档标注。测试：派发后删除子目录 → 子代理首轮工具调用被拒或明确报错。完成：新增 `subworkspace::revalidate_subworkspace`（[subworkspace.rs](file:///d:/claw-code-src/rust/crates/runtime/src/subworkspace.rs)）——重新 canonicalize + 拒绝 symlink 逃逸/等于根/非目录/项目标记消失，返回重新解析的绝对路径；[conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs) `run_subagent_turn_with_model` 开头重绑 `workspace_override`（失败记 diag + Err → 首轮即拒），scope/handoff 基准用 revalidate 后的新路径，与 Guard 3 每次工具调用的 canonicalize 判定一致。4 个新测试（3 revalidate 单测 + 1 turn 级：派发后删目录 → turn 明确报错"no longer exists"）。回归：runtime 全量 1675 通过。
- [x] **T6 MultiAgent 路径接入目录隔离**（路径 B 治理统一）：`SubagentDispatcher` 增加 `workspace_override` 字段并透传 `dispatch_impl`，构造 `subagent_scope` 传入 `process_tool_uses` 第 9 参（当前硬编码 `None`，[subagent_dispatcher.rs](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs#L291-L299)），handoff 落盘到 workspace 基准；消除"单发派发有隔离、批量并行无隔离"的治理不一致。测试：`spawn_parallel_via_dag` 子代理越界读 / 整仓扫描 → 被 Guard 3 / Guard 2.5 拒绝。完成：[subagent_dispatcher.rs](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs) 加 `workspace_override` 字段 + `with_workspace_override` builder，`dispatch_impl` 构造 `handoff_root`（5 处 write_handoff 迁移）与 `subagent_scope`（`process_tool_uses` 第 9 参 `None` → `subagent_scope.as_ref()`）；[conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs) `with_dag_coordinator` 加第 5 参 `workspace_override`（21 处调用点更新：19 测试 + [app.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/app.rs) 生产接入 + 集成测试）。3 个 dispatcher 单测（handoff 落盘 scope / ReadOnly+scope 越界读 Guard 3 拒 / Execute+scope bash Guard 2.5 拒）+ 1 个 `spawn_parallel_via_dag` 集成测试（绑定 scope → 工具请求被 guard 拒 + Failed handoff 落盘子目录、主 root 无）。**遗留（T7 范畴）**：`SpawnRequest.capability` 尚未透传 dispatcher（集成路径下默认 Analyze，Guard 2 先拦），执行链统一后按 node 能力生效。回归：runtime 全量 1679 + workspace 编译 + 集成测试通过。
- [x] **T7 执行链统一**（消除双执行循环）：`SubagentDispatcher::dispatch_impl` 委托 `execute_subagent_llm`（已为无 `&self` 自由函数形态，[conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L4451-L4454)）；同步线程 `std::thread::spawn` + `oneshot` 桥接保留，async `stream_async` 为主；删除路径 B 的重复多轮循环（[subagent_dispatcher.rs](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs#L181-L319)）与 `build_subagent_request` prompt 构造，一处 guard / 循环 / prompt。收益：T4/T5/Guard 2.5 等能力自动覆盖全部子代理，消除双轨漂移。测试：现有 `spawn_parallel_via_dag` 全量测试通过（回归门禁）。完成：`execute_subagent_llm` 从 impl 方法提取为模块级 `pub(crate)` 自由函数（3 处 `Self::` 调用点改直接调用）；`dispatch_impl` 重写为委托——线程内自建 current_thread runtime 后 `block_on(execute_subagent_llm(...))`，删除路径 B 的重复多轮循环 / handoff 构造 / prompt 构造（`build_subagent_request` 转 `#[cfg(test)]`，生产仅用 `build_subagent_system_prompt`）；新增 `NoToolExecutor` 拒绝型 stub（无 executor 时保持统一签名完整）；dispatch_impl 补 `revalidate_subworkspace`（T5 TOCTOU 一致性）。T6 遗留（`SpawnRequest.capability` 透传）自动部分解决——统一后 capability 仍为 dispatcher 级，per-node 透传留待 T8 编排层。4 个测试调整/补 marker（T6 3 个 scope 测试 fixture 加 Cargo.toml 满足 revalidate；无 executor 测试改为 Truncated 语义）。回归：subagent_dispatcher 9 + spawn_parallel 29 + dispatch_subagent 20 + run_subagent_turn 1 + runtime 全量 1679 连续两次通过 + workspace 编译零 warning。
- [x] **T8 handoff 统一 + 编排层正交化**：handoff 落盘统一 workspace 基准（路径 B 不再写主 root）；`MultiAgentCoordinator`（任务 DAG / 重试 / 校验）与 `SessionBus`（通信 / 状态）经统一执行入口协作，互不直接调用。收益：并行子代理注册为 bus peer，状态 / 结果 / 未读对全部会话可见。测试：并行派发后 `/bus list` 显示子代理 peer 且终态 `Done`。完成：**handoff 统一已由 T6/T7 达成**（`execute_subagent_llm` 为唯一 write_handoff 点，`handoff_root = workspace_override || workspace_root`）；**bus 生命周期移入统一执行入口**——[conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs) `execute_subagent_llm` 入口 `bus.register(Streaming)` + `BusPeerDoneGuard`（Drop guard 置 `Done`，任意返回路径生效）；[dispatch_subagent](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs) 删除手动 register 段（编排层不再直接调用 SessionBus；终态 Handoff 广播保留）。MultiAgentCoordinator 无 SessionBus 依赖（grep 验证正交）。新增 `spawn_parallel_via_dag_registers_bus_peers` 集成测试（并行 2 子代理 → `/bus list` 2 个 Subagent peer 且 Done；路径 A 现有测试 peer+Done+Handoff 断言不变通过）。回归：spawn_parallel 30 + dispatch_subagent 20 + subagent_dispatcher 9 + runtime 全量 1680 + workspace 编译 + 集成测试通过。

### Epic 2（配置继承 + 生命周期）— 已实施（2026-08-12）

**待办任务**（[ ] 未开始 / [x] 已完成）：

- [x] **A2.1 `ConfigLoader::discover` ancestor walk 落地**：从 cwd 向上收集各层 `.claw.json` + `.claw/settings.json`（父先子后，子覆盖父），到用户主目录（`USERPROFILE`/`HOME`，与 diag.rs 约定一致）即止，并跳过主目录与 config_home 层级（其配置已由 User source 收集，避免把 `~/.claw.json` 误当 Project 配置重复合并；cwd 自身永不跳过）；`settings.local.json` 仅当前目录不参与继承。完成：[config.rs](file:///d:/claw-code-src/rust/crates/runtime/src/config.rs#L417-L458) `discover()` 重写 + 3 测试（`ancestor_walk_merges_parent_config_with_child_override` / `ancestor_walk_does_not_inherit_settings_local` / `ancestor_walk_priority_user_parent_child_local`）。回归：config 模块 34 + runtime 全量通过。
- [x] **A2.2 子代理会话可 `/resume`**：核实 `.claw/sessions/` 按 workspace 分目录天然分域、父 store 列表不可见；但 `validate_loaded_session` 强制 workspace 完全匹配导致父 store 显式路径引用子 workspace 会话被 `WorkspaceMismatch` 拒 → [session_control.rs](file:///d:/claw-code-src/rust/crates/runtime/src/session_control.rs#L177-L213) `load_session` 增加"显式路径引用授权分支"：位于 store 管理区（`<ws>/.claw/sessions/`）之外的路径视为跨 workspace 恢复意图，仅要求会话 workspace 在本 store 工作区树内；不相关 workspace 仍拒。完成：2 测试（`parent_store_loads_child_workspace_session_via_explicit_path` / `parent_store_rejects_unrelated_workspace_session_via_explicit_path`）。
- [x] **A2.3 manifest.json 生命周期映射 + `/subagent steer/kill` 校验 workspace**：
  - A2.3a [multi_agent/mod.rs](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/mod.rs) `Subagent` 加 `workspace: Option<PathBuf>` 字段 + `SpawnRequest.with_workspace` + coordinator `set_workspace`；单发派发路径（`execute_dispatch_subagent`）注入 workspace_override，`spawn_parallel` 传播。
  - A2.3b 新建 [manifest.rs](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/manifest.rs)：`subagent_id → workspace` 映射与状态流转持久化到 `.claw/subagents/manifest.json`（JSON 数组原子写）；coordinator `persist_manifest` 挂接 spawn/start/complete/fail/cancel/set_workspace（best-effort）；**生产缺口修复**：`ConversationRuntime::set_workspace_root`/`with_workspace_root` 同步注入 coordinator（否则 coordinator 的 workspace_root 恒为 None，manifest 永不落盘）。
  - A2.3c steer/kill 工具（bus Command 注入，用户选定方案）：新增 `steer_subagent`（注入运行中指令）/`kill_subagent`（终止 + coordinator.cancel 同步 + 终态 no-op）工具 spec + 分发分支 + `validate_subagent_target`（存在性 + workspace 绑定 revalidate 校验）；`HandoffStatus::Cancelled` 新变体。
  - A2.3d [conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L1428-L1476) `execute_subagent_llm` 循环顶部每轮消费 bus Command：steer → 追加 user 指令（下一轮 LLM 请求可见）；kill → 落盘 Cancelled handoff + Err。
  - 测试：manifest 3 单测 + lifecycle 2 集成 + steer/kill 4 集成（投递/终态 no-op/kill 消费/steer 注入可见性）。回归：runtime 全量 1694 + workspace 编译零 warning。

### Epic 3（拓扑感知派发）— 已实施（2026-08-12）

**待办任务**（[ ] 未开始 / [x] 已完成）：

- [x] **集成 [project_topology.rs](file:///d:/claw-code-src/rust/crates/runtime/src/project_topology.rs) `ModuleGraph`：按 crate 边界自动推导建议的 `workspace` 参数**：
  - 新增 `ProjectTopology::suggest_workspaces(query)` + 纯函数 `format_workspace_suggestions`：每个 crate 映射为建议 `workspace` 相对路径（`manifest_path.parent()` 相对 workspace_root，正斜杠；workspace 根自身 crate → `"."`），按 crate 名排序，`query` 按 crate 名过滤（大小写不敏感）；Building/Failed 降级提示（同 query_project_graph 语义）。
  - 新增 `suggest_workspace` 工具（[conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs) `SUGGEST_WORKSPACE_TOOL_SPEC` + `execute_suggest_workspace` + 分发分支）：无 ProjectTopology 时返回降级提示（不报错，LLM 可继续派发不带 workspace）。
  - 测试：4 个纯函数单测（相对路径映射/query 过滤/无匹配提示/Windows 路径正斜杠归一）+ 1 个 conversation 降级测试。回归：runtime 全量 1699 + workspace 编译零 warning。
  - 限制：`suggest_workspace` 与 dispatch/steer/kill 同等待遇——spec 预留注册（`#[allow(dead_code)]`），分发分支已就位，待 main.rs tool registry 启用。

---

## 5. 测试策略（TDD）

| 层 | 测试 | 断言 |
|---|---|---|
| unit | `discover_subworkspaces`（demo-monorepo fixture） | 返回 4 个 crate；忽略 `.git/target/node_modules/.claw`；relative_path 正确 |
| unit | workspace 参数校验 | 越界路径（`../crates/core`、绝对路径逃逸）→ `invalid_workspace` 拒绝 |
| unit | `WorkspacePathScope` 越界拦截 | 子代理 scope=api 时读 core 文件 → 拒绝（沿用 [path_scope_enforcement.rs](file:///d:/claw-code-src/rust/crates/tools/tests/path_scope_enforcement.rs) 测试模式） |
| unit | config ancestor walk | 子目录配置合并父级 + 覆盖；`settings.local.json` 不继承 |
| unit | 防递归派发到父级 | 子代理尝试 `workspace: ".."` → 拒绝 |
| integration | 父派发子 → handoff 回收 | mock ApiClient 下父会话收到子代理 summary（[conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs) 现有 dispatch 测试模式） |
| security | 越权写 | 子代理 edit_file 到父目录 → permission_enforcer 拒绝（测试 `dispatch_subagent_fails_gracefully_without_workspace_root` 扩展） |
| integration | 并行路径隔离 + 执行链统一（Epic 1 T6/T7） | `spawn_parallel_via_dag` 子代理越界读 / 整仓扫描 → Guard 3 / 2.5 拒绝；T7 后现有 DAG 测试全量回归通过 |

---

## 6. 实现可行性清单（9 项）

| # | 检查项 | 结论 |
|---|---|---|
| 1 | **代码事实锚定** | 所有改动点已锚定 `文件:行号`，见 §1/§2/§3 |
| 2 | **依赖完整性** | 零新增依赖：复用 `walkdir`（[runtime/Cargo.toml](file:///d:/claw-code-src/rust/crates/runtime/Cargo.toml) 已有）+ 现有 `multi_agent`/`file_ops` |
| 3 | **编译可行性** | `WorkspacePathScope`/`SubagentHandoff`/`spawn_claw_shell` 均为现有公开类型；conversation.rs 扩展不引入循环依赖 |
| 4 | **测试覆盖** | 7 类测试覆盖发现/校验/约束/合并/递归/回收/安全（§5） |
| 5 | **平台兼容（Windows） | 路径用 `Path` 比较 + `canonicalize`；避免 `:` 盘符解析问题（沿用 [project_topology.rs](file:///d:/claw-code-src/rust/crates/runtime/src/project_topology.rs) grep 行解析经验） |
| 6 | **并发与状态安全** | 扫描后台线程更新共享状态（沿用 `ensure_built()` 模式）；`subworkspaces.json` 原子写 |
| 7 | **性能与内存** | 扫描 O(目录数)；深度上限 4 + 缓存；避免对大 monorepo 全量 read |
| 8 | **向后兼容** | `workspace` 缺省 = 当前 cwd，现有调用零影响；`is_project_dir` 未改动 |
| 9 | **可回滚** | Epic 0 仅 `ReadOnly`；越界拦截为纯新增校验，失败可禁用；`discover_subworkspaces` 失败仅降级提示 |

---

## 7. 风险与对策

| 风险 | 对策 |
|---|---|
| 父子并行写同一文件 | Epic 1 起强制 `file_guard.rs` + 推荐 `mode=worktree` |
| 子代理越权逃出目录 | `WorkspacePathScope` 硬约束 + 7 类越权测试 |
| 配置继承导致子目录行为被父级意外覆盖 | 合并顺序文档化 + `settings.local.json` 例外 + `/config` 展示已加载文件（[config.rs](file:///d:/claw-code-src/rust/crates/runtime/src/config.rs) 现有 report 已支持） |
| 大仓库扫描慢 | 深度上限 + 后台异步 + 缓存到 `.claw/subworkspaces.json` |
| 子代理递归派发回父级 | loop_detection 扩展 + 白名单不含 dispatch 工具（现有 guard） |

---

## 8. 验收标准

1. `discover_subworkspaces` 在 demo-monorepo 返回 4 个 crate，忽略 `.git/target/.claw`。
2. 父会话派发 `workspace: "crates/api"` 的子代理成功，handoff summary 回到父上下文。
3. 子代理读/写目录外文件被拒绝（`ReadOnly` 下写直接拒绝，读越界被 scope 拦截）。
4. 子目录内 `/config` 显示父级 + 子目录配置已合并，`settings.local.json` 未混入。
5. 无 panic、无 `expect`（遵循 project_memory L2 Mutex 规则）。
6. Epic 1 T7 后，`dispatch_subagent` 与 `spawn_parallel_via_dag` 子代理行为一致：越界拦截、cwd 视角、handoff 基准统一（回归门禁通过）。

---

## 9. 审查修复记录（2026-08-11）

| 级别 | 发现 | 修复 |
|---|---|---|
| P0 | **Guard 3 相对路径 join 基准错误（越界漏洞）**：candidate 用 `scope_root.join(target)`，子代理传相对主 root 的路径（如 `crates/api/../core/foo.rs` 或 `crates/core/x.rs`）归一化后字符串前缀仍落在子目录内被放行，而工具执行器以主 root 解析 → **越界读取仓库任意文件** | candidate 改为 `workspace_root.join(target)` 归一化后校验是否在子目录 scope 内；补 P0 回归测试（场景 3 拒绝 + 场景 4 合法放行） |
| P1 | **repomap / lsp_diagnostics 不受 Guard 3 覆盖**：无 `file_path/path` 参数可校验，绑定 workspace 的子代理可扫描整个仓库（文件/符号结构泄露） | 新增 Guard 2.5：`scope.is_some()` 时禁止这两个工具；单测 `process_tool_uses_scoped_rejects_whole_repo_scan_tools` |
| P1 | **symlink 逃逸**：Guard 3 是字符串层（lexical）校验，子目录内 symlink 指向外部时，字符串前缀在 scope 内被放行，底层 canonicalize 后读取外部文件 | Guard 3 增加 canonicalize 二次校验：lexical 通过后对真实路径再过 `scope.validate_resolved`，链接目标在 scope 外即拒绝；单测 `process_tool_uses_scope_rejects_symlink_escape`（平台不支持建 symlink 时跳过） |
| P2 | **resolve_subworkspace 每次派发全量扫描目录树**：用非缓存 `discover_subworkspaces`，大 monorepo 下每次 dispatch 代价高 | 改用 `discover_subworkspaces_cached`（缓存命中即返回） |

**已知限制（Epic 1 修复）**：
- ~~**子代理 cwd 未切换**~~：已由 T4 修复（cwd 切到子目录 + Guard 3 双基准 + 落位改写）。
- ~~**TOCTOU**~~：派发时校验的目录在子代理执行期间可能变化（新增/删除子目录），缓存导致 false-negative（安全方向），可接受。→ 已由 T5 缓解（turn 开始处 `revalidate_subworkspace` 复核，失效即拒；剩余窗口为 turn 内部目录变化，仍由 Guard 3 每次工具调用 canonicalize 兜底）。
- ~~**双执行链并存（治理不一致）**~~：已由 T6/T7/T8 统一——路径 B 接入目录隔离（T6）、`dispatch_impl` 委托 `execute_subagent_llm`（T7，一处 guard/循环/prompt/handoff）、bus 生命周期经统一执行入口（T8）。
- `dispatch_subagent` 的 `workspace` 字段与 `mode=worktree` 叠加时双重约束，行为以更严者为准（无冲突，未专门处理）。
- **Epic 2 A2 限制**：steer/kill 为协作式中断（子代理在下一轮工具循环检测 bus Command，不强制中断进行中的 LLM 调用）；manifest 写入为 best-effort（coordinator 无 workspace_root 时静默跳过）；steer 仅对运行中子代理生效（终态返回 no-op 提示）；ancestor walk 以用户主目录为继承边界，主目录之上的配置不进入项目继承链。

---

## 10. 与 Session Bus 的关系

两份文档可独立落地，但互补：

- **目录层级控制**（本文档）解决"按目录分配工作"——派发/约束/回收，是**静态结构**。
- **会话互通**（[2026-08-11-session-bus-design.md](./2026-08-11-session-bus-design.md)）解决"会话之间对话"——注册/路由/广播，是**动态协作**。

落地顺序建议：先 Epic 0 目录层级（风险最低、验证派发链路），再 Session Bus Epic 0（在同一派发链路上叠加互通），两者在 `execute_dispatch_subagent` 处汇合，改动面不重叠。
