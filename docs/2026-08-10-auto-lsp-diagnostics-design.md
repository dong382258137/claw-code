# 编辑后自动 LSP 诊断 — 设计文档

- 日期：2026-08-10
- 状态：设计评审中
- 关联问题：LSP 依赖模型主动调用，0 调用时整个诊断能力闲置；已存在的"编辑后自动诊断"兜底只读缓存、从未生效

---

## 1. 目标与背景

### 1.1 用户诉求

> "能不能做到系统在 AI 修改代码后自动调用 LSP，把诊断结果填入到上下文的变动区？"

即：**编辑动作本身触发 LSP 诊断**，结果作为 tool result 的一部分进入对话历史（"上下文的变动区"），模型无需（也不应依赖）主动调用 `LSP` 工具。

### 1.2 现状（审计结论）

- 会话审计：LSP 工具 **0 调用**（95 会话 / 5132 次调用）。
- 基础设施完备：`LspRegistry`（全局单例，进程内管理 server 子进程）、后台 reader 线程、`publishDiagnostics` 缓存、`try_auto_start`、`ensure_did_open` 均存在。
- **已有半成品**：`tools/src/lib.rs::run_lsp_diagnostics_for_file` 已在 `write_file` / `edit_file` / `replace_lines` 三个工具结果后附加 `--- LSP diagnostics ---` 块（L3138/L3163/L3191）。
- **半成品缺陷**：该函数只读缓存（"不 spawn server、不阻塞、不提示安装"）。服务器从未启动 → 缓存恒空 → 静默跳过。**当前会话形态下从未生效过。**

### 1.3 目标

编辑代码文件后，系统自动：

1. 确保 LSP server 可用（已连接或按需 auto-start）；
2. 通知 server 重新诊断该文件（didOpen 首开 / didChange 增量）；
3. 等待 `publishDiagnostics` 推送到缓存（带超时）；
4. 将诊断块附加到编辑工具结果（0 诊断不附加，避免噪音与 token 浪费）。

## 2. 架构总览

```
edit_file / write_file / replace_lines (tools crate)
        │ 编辑成功,组装 result 文本
        ▼
run_lsp_diagnostics_for_file(path)         [升级:读缓存 → 主动刷新]
        │
        ▼
LspRegistry::refresh_diagnostics_for_path(path)   [新增, runtime/lsp_client.rs]
        │ 1. 语言映射(扩展名) + 冷却检查
        │ 2. 未 spawn → try_auto_start (失败→语言级冷却+首条安装提示)
        │ 3. didOpen(首次) / didChange(增量, 版本递增)
        │ 4. 轮询推送计数器变化, 读 get_diagnostics(path), 2.5s 超时
        ▼
format_lsp_diagnostics → 附加到 tool result  ──▶ 进入对话历史(上下文变动区)
```

## 3. 现状能力核对（设计依据）

| 能力 | 位置 | 状态 |
|---|---|---|
| 语言映射（扩展名） | `lsp_language_for_extension` (tools/src/lib.rs L2997) | ✅ 已有 |
| auto-start（含安装提示） | `LspRegistry::try_auto_start` (lsp_client.rs L334) | ✅ 已有，同步 spawn |
| didOpen 通知 | `ProcessLspTransport::ensure_did_open` (L1274)，带 `opened_files` 去重 | ✅ 已有 |
| 诊断推送 → 缓存（按 path 全量替换） | reader 线程 (L1207-1246) | ✅ 已有 |
| 按 path 查缓存 | `get_diagnostics(path)` (L532) | ✅ 已有 |
| 编辑结果附加点 | `run_lsp_diagnostics_for_file` 三处调用 | ✅ 已有（读缓存版） |

## 4. 核心新增 API

### 4.1 `LspRegistry::refresh_diagnostics_for_path(path) -> Option<String>`

```rust
/// 编辑后主动刷新诊断:确保 server 就绪 → didOpen/didChange → 等待推送。
/// 返回格式化诊断块;无法获得诊断(语言不支持/服务器缺失/超时)返回 None。
pub fn refresh_diagnostics_for_path(&self, path: &str) -> Option<String>
```

流程（伪代码）：

```
ext → language; 不支持 → return None
语言处于冷却期(上次 auto-start 失败) → return None
server 已 spawn?
  ├─ 否 → try_auto_start(language)
  │        ├─ Ok  → 继续
  │        └─ Err(含 not in PATH 安装提示) →
  │             记录冷却(该语言 60s 不再尝试)
  │             首次失败:返回格式化的安装提示文本(可附结果)
  │             后续失败:return None(静默)
  └─ 是 → 继续
c0 = push_counter.load()                      // 记录推送基线
transport.ensure_did_open(path, lang)          // 首开(读盘→didOpen)
transport.notify_did_change(path, lang, v+1)   // 增量(读盘→didChange, version+1)
// 轮询:推送计数器越过 c0 或超时(默认 2500ms,可配)
loop { push_counter > c0 → return format(get_diagnostics(path));
       elapsed > timeout → return None(不报错,保留 stale 缓存) }
```

### 4.2 推送检测（关键设计点：空诊断歧义）

**问题**：`publishDiagnostics` 是全量替换。服务器推送"0 错误"时会清空缓存，`get_diagnostics` 返回空——无法区分"已推送 0 错误"与"还没推送"。

**方案**：新增 `push_counter: Arc<AtomicU64>`，reader 线程每次处理 `publishDiagnostics` 通知时递增（在更新缓存处一并完成）。refresh 以"计数器越过 didOpen/didChange 前基线"作为**推送已到达**的可靠信号，无论诊断为空或非空。

### 4.3 `ProcessLspTransport::notify_did_change(path, language_id, version)`

**为什么需要 didChange**：`ensure_did_open` 有 `opened_files` 去重——首次编辑后文件已 open，后续编辑再调 didOpen 不会重新发送，服务器不会重新诊断。因此编辑后必须发 `textDocument/didChange`（contentChanges 全量替换 + version 递增）。

需将 `opened_files: HashSet` 升级为 `HashMap<path, version>`，`ensure_did_open` 记录 v1，`notify_did_change` 递增。

## 5. 编辑工具接入点（升级而非新增）

`run_lsp_diagnostics_for_file` 语义从"只读缓存"改为"主动刷新"：

```rust
fn run_lsp_diagnostics_for_file(file_path: &Path) -> Option<String> {
    // 保留:扩展名过滤、非代码文件跳过
    // 变更:调用 registry.refresh_diagnostics_for_path(...) 而非 get_diagnostics(...)
}
```

三处调用点（write_file/edit_file/replace_lines）**不变**——结果的附加逻辑已存在，只换数据来源。

## 6. 延迟预算与成本控制

| 项 | 设计值 | 理由 |
|---|---|---|
| 推送等待超时 | 2500ms（可配 `lspAutoDiagnosticsTimeoutMs`） | pylsp ~0.5-2s；rust-analyzer 首次 3-5s，超时即放弃 |
| 语言级冷却 | 60s（auto-start 失败后） | 服务器缺失时不重复 spawn/弹提示 |
| 冷却期提示 | 首次失败附一条安装提示，其后静默 | 引导安装 vs 避免刷屏 |
| 诊断截断 | 已有 `take(20)` + `... and N more` | 防止 context 膨胀 |
| 0 诊断附加 | 不附加（返回 None） | 无信息不占 token |

**已连接 server 快路径**：服务器已 spawn 时，didChange→推送典型 <1s，且多数场景为零往返（快速路径）。延迟主要来自**首次 auto-start**（仅每语言一次）。

## 7. 与 cargo check 协调

现状：`.rs` 文件编辑后 `run_cargo_check_for_file` 已同步跑 `cargo check`（60s 超时）。

设计决策：

- **默认**：`.rs` 维持 cargo check 现状，不叠加 LSP 自动刷新（避免双重同步等待与重复诊断）。
- **非 `.rs`**（py/ts/js/go/java/c/cpp/rb/lua）：走 LSP 自动刷新。
- **配置项** `lspAutoDiagnostics: "auto" | "all" | "off"`：
  - `auto`（默认）：上述分流
  - `all`：`.rs` 也走 LSP（若用户更看重单文件精准诊断，可配置）
  - `off`：完全关闭自动诊断，恢复纯读缓存兜底（回滚档位）

## 8. 行为语义与边界

| 场景 | 行为 |
|---|---|
| 语言不支持（.md/.json 等） | 跳过，不触碰 registry |
| server 缺失（not in PATH） | 首次附安装提示，进入冷却，之后静默 |
| server 启动慢/超时 | 返回 None，保留 stale 缓存；不阻塞编辑结果 |
| 诊断为空（0 错误） | 附加"0 issue"摘要（证明已刷新）或直接不附加——**倾向不附加**（已由推送计数器保证"确实刷新过"） |
| 编辑失败 | 不触发自动诊断（仅成功路径） |

## 9. 测试计划

1. **单元（lsp_client.rs）**：
   - `refresh_diagnostics_for_path`：非代码扩展名 → None（不触碰 registry）
   - didChange 版本递增：同路径二次刷新发送 v2（可 mock transport 记录）
   - 推送计数器：模拟 reader 推送后返回诊断；空诊断数组也算刷新
   - 超时路径：不阻塞、返回 None
2. **单元（tools/src/lib.rs）**：
   - 升级后 `run_lsp_diagnostics_for_file` 对未 spawn 语言不 panic、快速返回
   - 冷却期二次调用不重复尝试（计数器/时间戳断言）
3. **集成（手工/脚本）**：
   - 新会话中仅执行 edit_file 修改 .py，观察结果是否带 `--- LSP diagnostics ---`
   - 编辑产生语法错误 → 诊断出现在 tool result；修正后 → 0 issue（不附加）
   - `.rs` 文件：确认仍走 cargo check，无 LSP 叠加
   - 卸载/隐藏 pylsp → 编辑 .py 附安装提示一次，后续静默

## 10. 验收标准

- [ ] 新会话中编辑 `.py`（含故意语法错误），tool result 自动出现诊断块，模型无需调用 `LSP`
- [ ] 修正后再次编辑，诊断块消失（0 issue 不附加），证明刷新链路闭环
- [ ] pylsp 缺失时编辑不卡死、不刷屏（首条提示 + 冷却）
- [ ] `.rs` 行为与当前一致（cargo check，无 LSP 叠加）
- [ ] 编辑工具延迟增量在超时预算内（已连接 <1s，auto-start 例外）

## 11. 实施步骤（建议顺序）

1. `lsp_client.rs`：`push_counter` 注入 reader 线程 + `opened_files` 改 `HashMap` + `notify_did_change` + `refresh_diagnostics_for_path`（含冷却表）
2. `tools/src/lib.rs`：`run_lsp_diagnostics_for_file` 改用 refresh；配置项读取与分流（`.rs` → cargo check 现状）
3. 单测补齐（见 §9）
4. 部署 + 会话级验证（§9.3 手工脚本）
