# Claw Enterprise Audit Module 设计方案

| 项 | 值 |
|---|---|
| 文档版本 | v1.0 |
| 创建日期 | 2026-07-21 |
| 文档类型 | 设计方案 / 商业化实施指南 |
| 适用项目 | claw-code-src (Rust 实现的 Claude Code 开源克隆) |
| 商业定位 | Open Core 模式下的企业版核心模块 |
| 依赖现有组件 | PermissionEnforcer / TraceAnalyzer / ToolResultArchive / SessionTracer / SandboxBuilder |

---

## 一、背景与商业定位

### 1.1 商业目标

本项目走开源路线，采用 **Open Core + 企业版** 变现模式。企业审计模块作为企业版的核心差异化能力，承担以下商业目标:

| 目标 | 说明 |
|---|---|
| 现金流回笼 | 私有化部署 + SLA 订阅，单客户年费支撑小团队运转 |
| 客户验证 | 对接金融/政企/医疗等强合规需求客户，验证 PMF |
| 护城河构建 | 基于 Rust + Sandbox + PermissionEnforcer 底座，社区版无法轻易复制 |
| 社区保护 | 企业版功能聚焦企业级需求(审计/SSO/合规)，不阉割社区版核心能力 |

### 1.2 模块定位

审计模块是 **G 层(治理)的可观测性延伸**，为以下场景提供合规证据链:

- **谁** 在 **何时** 对 **什么资源** 做了 **什么操作**，**结果如何**
- 模型调用与计费可追溯(防止 API key 滥用、成本失控)
- 敏感数据访问留痕(满足 GDPR/SOC2/ISO27001 审计要求)
- 不可篡改的执行证据链(满足司法取证需求)

### 1.3 设计原则

| 原则 | 说明 |
|---|---|
| 最小侵入 | 社区版默认 `audit_sink = None`，零开销，零代码改动 |
| 源头控制 | 脱敏在采集点完成，原文不进审计日志(数据最小化原则) |
| 复用现有桥 | 不重复埋点，在 `SessionTracer` / `record_tool_finished` 等已有 hook 旁采集 |
| Tamper-Evident | hash chain 保证日志完整性，删除任意一条都会断链 |
| 双轨授权 | 社区版保留本地审计培养习惯，企业版独占远程 sink 与合规格式 |

---

## 二、现状盘点

基于代码探查，现有组件的审计能力缺口如下:

| 现有组件 | 文件路径 | 现状 | 审计字段缺口 |
|---|---|---|---|
| `PermissionEnforcer` | `rust/crates/runtime/src/permission_enforcer.rs` | `EnforcementResult::Denied` 仅 4 字段 | **缺** timestamp / user / session / input snapshot |
| `TraceAnalyzer` | `rust/crates/runtime/src/trace_analyzer.rs` | `TraceRecord` 6 字段，仅 CSV 导出 | **缺** JSONL sink、失败种类生产路径只写 `"runtime_error"` |
| `ToolResultArchive` | `rust/crates/runtime/src/tool_result_archive.rs` | JSONL append-only | **缺** hash chain、actor、脱敏、远程 sink |
| `SessionTracer` + `TelemetrySink` | `rust/crates/telemetry/src/lib.rs` | 已有 trait 化 sink，turn 级事件已埋点 | **未接**审计字段 |
| `LaneEvent` 全局 sink | `rust/crates/runtime/src/lane_events.rs` | 22 种事件 + provenance + ownership | **生产无消费者** |
| `ApprovalTokenAudit` | `rust/crates/runtime/src/approval_tokens.rs:194` | 有 actor 但无 timestamp | 仅覆盖审批 token，非通用审计 |

**结论**: 基础设施 80% 已就位，缺的是「审计专用数据模型 + 不可篡改存储 + 多 sink + 查询/导出」。

---

## 三、模块边界

### 3.1 新增独立 crate

```
rust/crates/audit/
├── Cargo.toml
├── src/
│   ├── lib.rs              # 公共 API re-export
│   ├── event.rs            # AuditEvent 数据模型
│   ├── sink.rs             # AuditSink trait + LocalJsonlSink / HttpSink / S3Sink
│   ├── chain.rs            # hash chain(tamper-evident)
│   ├── redact.rs           # 字段脱敏(API key / PII / path)
│   ├── query.rs            # 查询 API(时间/user/tool/decision 过滤)
│   ├── export.rs           # 合规导出(CSV / JSON / JSONL / CEF)
│   ├── license.rs          # 企业版 license gate(feature flag)
│   └── envelope.rs         # 与 SessionTracer / LaneEvent 的桥接
```

### 3.2 与现有 crate 的关系

- **`runtime` crate**: 仅新增一处 hook — 在 `conversation.rs:2227` `record_turn_completed` 旁加 `audit_sink: Option<Arc<dyn AuditSink>>` 字段 + `with_audit_sink()` builder。社区版编译时 `cfg(feature = "enterprise-audit")` 移除该字段。
- **`tools` crate**: 在 `tools/lib.rs:1476` `maybe_enforce_permission_check_with_mode` 返回前调 `audit.record(Permission)`。
- **`rusty-claude-cli` crate**: 新增 `claw audit` 子命令，调用 `audit` crate 的查询/导出 API。

---

## 四、核心数据模型

### 4.1 AuditEvent

```rust
// audit/src/event.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    // 身份与上下文(补齐 PermissionEnforcer 缺失字段)
    pub event_id: String,          // UUIDv7，时序友好
    pub timestamp_ms: u64,         // 决策发生时刻
    pub session_id: String,        // 来自 SessionTracer.session_id
    pub turn_id: String,           // 来自 TraceRecord.turn_id
    pub user_id: Option<String>,   // 企业 SSO 注入；社区版 None
    pub actor: Actor,              // user / agent / subagent / system

    // 决策本体
    pub kind: AuditKind,           // Permission / ToolCall / Sandbox / Compaction / ModelInvoke / DataAccess
    pub outcome: AuditOutcome,     // Allowed / Denied / Executed / Failed / Redacted
    pub tool: Option<String>,
    pub input_fingerprint: Option<String>,  // SHA256(input)[:16]，不存原文
    pub input_summary: Option<String>,      // 脱敏后的 80 字符摘要
    pub reason: Option<String>,

    // 沙箱与执行环境
    pub sandbox_active: bool,
    pub sandbox_platform: Option<String>,   // linux/windows/macos
    pub workspace_root: Option<String>,

    // 模型与计费(合规审计需要)
    pub model: Option<String>,
    pub provider: Option<String>,
    pub tokens_in: Option<u32>,
    pub tokens_out: Option<u32>,
    pub cost_usd: Option<f64>,

    // 不可篡改链
    pub prev_hash: String,         // 上一条 event_id + payload 的 SHA256
    pub chain_seq: u64,            // 链内单调序号
}

pub enum AuditKind {
    Permission,        // PermissionEnforcer.check() 决策
    ToolCall,          // 工具实际执行(含 sandbox 是否生效)
    Sandbox,           // 沙箱启用/降级/fallback
    Compaction,        // microcompact 触发(敏感:上下文丢失风险)
    ModelInvoke,       // 调用 LLM(model/provider/tokens/cost)
    DataAccess,        // 文件读写、网络请求、敏感数据访问
    Subagent,          // subagent 启动/结果
}

pub enum AuditOutcome {
    Allowed,
    Denied,
    Executed,
    Failed,
    Redacted,
}

pub enum Actor {
    User { id: Option<String> },
    Agent { model: String },
    Subagent { id: String, parent_turn: String },
    System,
}
```

### 4.2 设计要点

- **`input_fingerprint` 不存原文** — 合规:GDPR 数据最小化原则。原文走 `ToolResultArchive` 已有路径
- **`prev_hash` + `chain_seq` 形成 tamper-evident 链** — 删除任意一条都会断链
- **`cost_usd` 复用 `runtime::pricing_for_model`** — 与 StatusBar/Sidebar 计费口径一致
- **`actor` 区分 user/agent/subagent/system** — 满足 RBAC 审计需求

---

## 五、摄入点设计(最小侵入 hook)

总计 6 个 hook 点，全部在已存在的事件埋点旁，不新增执行路径:

| 摄入点 | 现有函数位置 | 新增调用 | 采集的 AuditKind |
|---|---|---|---|
| 权限决策 | `tools/lib.rs:1476` `maybe_enforce_permission_check_with_mode` | 在返回前调 `audit.record(Permission)` | `Permission` |
| 工具执行 | `conversation.rs:1320` `record_tool_finished` | 已有 hook，加 audit 调用 | `ToolCall` + `DataAccess` |
| 沙箱决策 | `bash.rs:299` `sandbox_status_for_input` | 返回前调 `audit.record(Sandbox)` | `Sandbox` |
| 微压缩 | `compact.rs:525` `microcompact_with_archiver` archiver 闭包 | 在 archiver 闭包内追加 `audit.record(Compaction)` | `Compaction` |
| 模型调用 | `streaming.rs:500` `MessageStart` Usage 分支 | 已有 emit，加 audit 调用 | `ModelInvoke` |
| Subagent | `conversation.rs:1263` `dispatch_subagent` | 在 `start()` 旁加 audit | `Subagent` |

### 5.1 注入示例(权限决策点)

```rust
// tools/src/lib.rs (伪代码示意，不直接修改)
fn maybe_enforce_permission_check_with_mode(
    enforcer: &PermissionEnforcer,
    name: &str,
    input: &str,
    required_mode: PermissionMode,
    audit: Option<&AuditRecorder>,  // 新增参数
) -> Result<(), String> {
    let result = enforcer.check_with_required_mode(name, input, required_mode);

    // 新增:审计采集(社区版 audit=None 时跳过)
    if let Some(audit) = audit {
        audit.record(AuditEvent::permission_decision(
            name, input, &result,
        ));
    }

    match result {
        EnforcementResult::Allowed => Ok(()),
        EnforcementResult::Denied { reason, .. } => Err(reason),
    }
}
```

---

## 六、存储层

### 6.1 AuditSink trait

```rust
// audit/src/sink.rs
pub trait AuditSink: Send + Sync {
    fn record(&self, event: AuditEvent) -> Result<(), AuditError>;
    fn flush(&self) -> Result<(), AuditError>;
}

pub struct LocalJsonlSink { /* path, file lock, hash chain state */ }
pub struct HttpSink { /* endpoint, auth token, batch buffer, retry */ }
pub struct S3Sink { /* bucket, prefix, multipart upload */ }
pub struct CompoundSink { /* Vec<Arc<dyn AuditSink>>, fan-out */ }
```

### 6.2 本地存储

- **路径**: `<workspace_root>/.claw/audit/audit-YYYYMMDD.jsonl`(按日切分，便于归档)
- **格式**: JSONL，每行一个 `AuditEvent`
- **hash chain**: 每条记录的 `prev_hash` = SHA256(prev.event_id || prev.payload)，链头为固定 genesis hash
- **写入**: append-only，文件权限 `0600`(Unix)/ ACL 限制(Windows)
- **切分**: 每日 0 点滚动，旧文件压缩为 `.jsonl.gz`

### 6.3 远程 sink(企业版独占)

- **`HttpSink`**: POST 到企业 SIEM(Splunk HEC / Elastic / Datadog Logs API)
- **`S3Sink`**: 直接归档到对象存储(WORM bucket 配置 lifecycle)
- **批量缓冲**: 默认 100 条或 5 秒触发一次 flush
- **失败重试**: 指数退避，本地 fallback 到 `audit-failed.jsonl`

---

## 七、脱敏策略(源头控制)

遵循用户原则 #2「源头控制，不在下游过滤」:

```rust
// audit/src/redact.rs
pub fn redact_input(tool: &str, input: &str, config: &RedactConfig) -> RedactedInput {
    let fp = sha256_short(input, 16);
    let summary = match tool {
        "bash" => redact_bash_command(input),         // 遮蔽管道中的密钥
        "edit" | "write" => redact_file_path(input),  // 仅保留相对路径
        "read" => redact_file_path(input),
        _ => truncate(input, 80),
    };
    RedactedInput { fingerprint: fp, summary }
}
```

**关键**: 原文不进审计日志(数据最小化)，但通过 `input_fingerprint` 可与 `ToolResultArchive` 关联，需要原文时走 `recall_full` 严格授权路径。

### 7.1 默认脱敏规则

| 模式 | 匹配 | 替换为 |
|---|---|---|
| `api_keys` | `sk-ant-*`、`sk-*`、`Bearer *` | `[REDACTED:api_key]` |
| `email` | RFC 5322 简化模式 | `[REDACTED:email]` |
| `phone` | `\d{11}` 等 | `[REDACTED:phone]` |
| `path_prefixes` | `/etc`、`~/.ssh`、`C:\Users\*\AppData` | `[REDACTED:path]` |

---

## 八、配置

`.claw/audit.toml`(社区版不存在此文件即禁用):

```toml
[enabled]
value = true  # 总开关

[local]
path = ".claw/audit"           # 相对 workspace_root
rotate_daily = true
hash_chain = true
retention_days = 90

[redact]
api_keys = true
pii_patterns = ["email", "phone", "ssn"]
path_prefixes = ["/etc", "~/.ssh"]

[[sinks]]                      # 企业版才生效
type = "http"
endpoint = "https://siem.example.local/claw-ingest"
auth_token_env = "CLAW_AUDIT_TOKEN"
batch_size = 100
flush_interval_secs = 5

[license]
enterprise = false             # 由 license.rs 校验
```

---

## 九、CLI 命令

新增 `claw audit` 子命令(仅企业版启用时可用):

```bash
# 查询(支持过滤器)
claw audit query --user alice --tool bash --outcome denied --since 24h

# 导出(合规报告)
claw audit export --format csv  --since 2026-01-01 --output report.csv
claw audit export --format json --since 2026-01-01 --output report.json
claw audit export --format cef  # CEF 兼容 Splunk

# 完整性校验(hash chain)
claw audit verify                  # 验证链未断
claw audit verify --repair-dry-run # 查看断点

# 统计概览
claw audit stats --since 7d        # 复用 TraceAnalyzer.stats() 思路

# 隐私合规(GDPR right to be forgotten)
claw audit forget --user alice     # 删除某用户所有记录并记录 meta-audit
```

---

## 十、License Gate(企业版边界)

```rust
// audit/src/license.rs
pub enum AuditLicense {
    Community,      // 仅 LocalJsonlSink，无远程 sink，retention 7 天
    Enterprise {    // 全功能
        features: EnterpriseFeatures,
        expires_at: u64,
        seats: u32,
    },
}

pub fn validate_license() -> AuditLicense { /* 校验签名 */ }
```

### 10.1 社区版 vs 企业版功能矩阵

| 能力 | Community | Team | Enterprise |
|---|---|---|---|
| 本地 JSONL sink | ✅ | ✅ | ✅ |
| 基础查询 | ✅ | ✅ | ✅ |
| CSV 导出 | ✅ | ✅ | ✅ |
| hash chain | ❌ | ✅ | ✅ |
| 留存期 | 7 天 | 90 天 | 无限 |
| HTTP sink (SIEM) | ❌ | ✅ (3 个集成) | ✅ (无限) |
| S3 WORM sink | ❌ | ❌ | ✅ |
| SSO user_id 注入 | ❌ | ❌ | ✅ |
| CEF/Splunk 格式 | ❌ | ❌ | ✅ |
| `claw audit forget` (GDPR) | ❌ | ❌ | ✅ |
| SLA | ❌ | ❌ | ✅ |
| 私有化部署 | ❌ | ❌ | ✅ |

---

## 十一、与现有组件的复用关系

| 现有组件 | 复用方式 |
|---|---|
| `TelemetrySink` trait | `AuditSink` 可包装 `TelemetrySink`，反之亦然(双向桥) |
| `SessionTracer` | 已埋点的 turn 级事件自动转 `AuditEvent`，无需重复埋点 |
| `ToolResultArchive` | 审计日志存 fingerprint，原文仍走 archive；`recall_full` 增加 audit 记录 |
| `TraceAnalyzer` | `claw audit stats` 内部调 `TraceAnalyzer::stats()` + audit-specific 统计 |
| `LaneEvent` 全局 sink | `drain_lane_events()` 作为 audit 的 subagent/lane 事件来源 |
| `pricing_for_model` | 直接复用计算 `cost_usd` |
| `SandboxBuilder` | `sandbox_platform()` 字段直接进 `AuditEvent.sandbox_platform` |
| `PermissionEnforcer` | 不改 enforcer 本身，只在调用方包一层 audit recorder |

---

## 十二、落地里程碑

### M1(2 周)— MVP:本地审计闭环

- [ ] 新建 `audit` crate，定义 `AuditEvent` / `AuditSink` / `AuditLicense`
- [ ] 实现 `LocalJsonlSink` + hash chain
- [ ] 在 6 个 hook 点埋入 `audit.record()`(社区版默认 `None`)
- [ ] `claw audit query` + `claw audit verify` 命令
- [ ] 基础脱敏(API key、文件路径)
- [ ] 单元测试覆盖:链完整性、脱敏正确性、并发写入

### M2(2 周)— 企业版能力

- [ ] `HttpSink` + `S3Sink` + 批量缓冲
- [ ] `claw audit export --format cef/json`
- [ ] license 校验 + feature gate
- [ ] SSO user_id 注入接口(OIDC/SAML)
- [ ] 集成测试:与 Splunk HEC / Elastic 对接

### M3(2 周)— 合规与可观测

- [ ] SOC2 / ISO27001 字段对齐文档
- [ ] `claw audit stats` 可视化(复用 TraceAnalyzer K-means 聚类失败原因)
- [ ] 异常检测:权限拒绝突增、沙箱降级告警
- [ ] 审计日志自身的访问审计(meta-audit)
- [ ] 性能基准:单 turn 审计开销 < 1ms，磁盘 < 5KB/turn

---

## 十三、风险与对策

| 风险 | 对策 |
|---|---|
| 审计写入阻塞主流程 | sink 全异步，失败降级为 `eprintln` + 本地 fallback 文件 |
| hash chain 损坏 | `claw audit verify` 提供 dry-run 修复；genesis 重置需 admin token |
| 脱敏漏网 | 提供 `claw audit scan` 扫描历史日志中的可疑明文 |
| 企业版判定被绕过 | license 校验放在 sink 层，社区版编译时 `cfg` 移除远程 sink 代码 |
| 与 ToolResultArchive 重复 | 严格分工:audit 存决策元数据，archive 存原文；通过 fingerprint 关联 |
| 隐私合规(GDPR) | 默认 `user_id=None`；启用需显式配置；支持 `claw audit forget --user xxx` |

---

## 十四、商业化定价参考

| 档位 | 价格 | 包含 |
|---|---|---|
| Community | 免费 | 本地 JSONL、7 天留存、CSV 导出、基础查询 |
| Team | $20/seat/月 | 90 天留存、hash chain、HTTP sink、3 个 SIEM 集成 |
| Enterprise | $50/seat/月 | 无限留存、S3 WORM、SSO、CEF/Splunk、SLA、私有部署 |
| On-prem | 一次性 + 年费 | 私有化部署、定制 sink、源码授权 |

---

## 十五、参考

### 15.1 学术与行业标准

| 资料 | 来源 |
|---|---|
| Agent Harness Engineering: A Survey | https://openreview.net/pdf/f358711a95aaaf61fdeffd4ef3fc60fba9b8da57.pdf |
| Common Event Format (CEF) | https://www.microfocus.com/documentation/arcsight/arcsight-smartconnectors-8.3/cef-implementation-standard/ |
| SOC 2 Trust Services Criteria | https://www.aicpa.org/interestareas/frc/assuranceadvisoryservices/trustservices.html |
| ISO/IEC 27001:2022 | https://www.iso.org/standard/27001 |
| GDPR Article 30 (Records of processing activities) | https://gdpr-info.eu/art-30-gdpr/ |

### 15.2 内部依赖

| 组件 | 文件 |
|---|---|
| PermissionEnforcer | `rust/crates/runtime/src/permission_enforcer.rs` |
| TraceAnalyzer | `rust/crates/runtime/src/trace_analyzer.rs` |
| ToolResultArchive | `rust/crates/runtime/src/tool_result_archive.rs` |
| SessionTracer | `rust/crates/telemetry/src/lib.rs` |
| LaneEvent | `rust/crates/runtime/src/lane_events.rs` |
| SandboxBuilder | `rust/crates/runtime/src/sandbox.rs` |
| ApprovalTokenAudit | `rust/crates/runtime/src/approval_tokens.rs` |
| Session JSONL | `rust/crates/runtime/src/session.rs` |
| microcompact_with_archiver | `rust/crates/runtime/src/compact.rs:463` |

---

## 十六、下一步

| 步骤 | 说明 | 状态 |
|---|---|---|
| 社区版源码脱敏 | 扫描硬编码密钥/PII/内部路径，上传 GitHub | 进行中 |
| M1 实施启动 | 新建 `audit` crate 骨架 | 待启动 |
| 种子客户对接 | 找 2-3 个金融/政企客户验证需求 | 待启动 |

---

*文档结束。如需调整字段、里程碑或定价，请直接编辑本文件并在 commit message 中说明变更原因。*
