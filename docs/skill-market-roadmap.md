# 技能市场落地路线（WebUI 闭环）

> 范围：只做路线与设计，本轮不改代码。所有代码锚点均为 2026-11-10 实际查证；未查到的写「需确认」。

## 0. 结论先行

1. **不新建一套技能发现机制。** 现有 `commands` crate 已经具备完整的「多根发现 + 同名 shadow + frontmatter 解析 + 安装到用户目录」能力，市场只是在它上面加「索引 + 安装编排 + 启停状态 + Web 外壳」。
2. **技能的标识只能是 `name`，不能是 `name@market`。** 发现层按目录名/frontmatter `name` 查找（`commands/src/lib.rs:3966`、`:2898`），`@` 进入目录名会同时污染调用名和发现名。市场来源只作为记录字段，不进入主键。
3. **`version` 无法来自 SKILL.md。** 现有 frontmatter 解析器只读 `name` 和 `description`（`commands/src/lib.rs:4067-4127`），不解析 `version`。版本只能来自 `marketplace.json`，缺省记 `0.0.0+unknown`，且**不参与**发现与匹配。
4. **启停状态写入市场自己的注册表，绝不移动目录。** 我建议 `~/.claw/skill-market/state.json` 与 `installed.json` **分文件**（理由见 §2.3），二者都在发现根 `~/.claw/skills/` 之外，避免半成品目录被当成技能。
5. **安装同步、原子、可回滚。** 本地源秒级；git 源用 `spawn_blocking` + 30s 超时。先写暂存区 → 校验 → rename 进安装根 → 写注册表；任一步失败按逆序清理。并发同一技能由 `AppState` 内的名字锁返回 `409`。
6. **前端不引状态库。** 在 `market/` 骨架内用 `useState` 管理「列表 / 详情 / 已安装 / 来源」四个视图；`shared/router.js` 只加一个 `skills/<name>` 前缀识别，保持 `useRoute()` 返回字符串不变。
7. **最大的隐性交付成本是 `web/dist`。** `serve.rs:40` 用 `include_str!("../web/dist/index.html")` 编译期内联，改前端必须 `npm run build` 并提交 `dist`，否则 WebUI 永远是旧页面（`web/.gitignore` 只忽略 `node_modules/`）。

---

## 1. 现状重审

### 1.1 技能发现：三条路径、两级优先级

**根目录枚举**：`discover_skill_roots(cwd)`（`rust/crates/commands/src/lib.rs:3327`）

| 段 | 位置 | 行号 |
| --- | --- | --- |
| 项目级（逐级祖先，近→远） | `<ancestor>/.claw/skills`、`.omc/skills`、`.agents/skills`、`.codex/skills`、`.claude/skills`，然后同层的 `.claw/commands`、`.codex/commands`、`.claude/commands` | `3330-3379` |
| 用户配置目录 | `$CLAW_CONFIG_HOME/skills`、`$CLAW_CONFIG_HOME/commands`；`$CODEX_HOME/skills`、`/commands` | `3381-3411` |
| 用户主目录 | `$HOME/.claw/skills`、`.omc/skills`、`.claw/commands`、`.codex/skills`、`.codex/commands`、`.claude/skills`、`.claude/skills/omc-learned`、`.claude/commands` | `3413-3463` |
| `$CLAUDE_CONFIG_DIR` | `skills`、`skills/omc-learned`、`commands` | `3465-3486` |
| 内置 | `<ancestor>/rust/skills`、`<exe-dir>/skills`、`<exe-dir>/../../skills`、`~/.claw/builtin-skills` | `3488-3537` |

**关键实现细节**：

- `push_unique_skill_root` 只在 `path.is_dir()` 为真时入列，且按路径去重（`3751-3764`）。所以「安装根不存在」时该根不会进入发现列表——安装时必须先 `create_dir_all`。
- 两个 origin：`SkillsDir`（每个子目录一个技能，必须有 `SKILL.md`）与 `LegacyCommandsDir`（子目录或单 `.md` 文件）（`2418-2431`，加载逻辑 `3974-4025`）。
- 同名优先级 = **先入列者胜**。`load_skills_from_roots` 用 `BTreeMap<name_lowercase, source>` 记录首次出现，后来者标 `shadowed_by`（`4027-4037`）。即项目级覆盖用户级，用户级覆盖内置。
- **shadow 不删除**：`load_skills_from_roots` 返回全部（含 shadowed）；`list_skill_summaries` 才 `retain(is_active())`（`4139-4145`）。市场列表若要展示「被项目同名技能遮蔽」，必须走前者。

**frontmatter 解析**：手写，非 YAML 库（`4067-4127`）。只认首行 `---` 起始的块内 `name:` / `description:`；`description` 支持 `>`、`|-` 等块标量，其余键全部忽略。**没有 `version`、`license`、`allowed-tools`**。

**二义性风险**：`tools/src/lib.rs:6595-6697` 另有一套兼容根 `skill_lookup_roots`，与 `commands::discover_skill_roots` 不共享代码。市场安装/校验必须只信任 `commands` 这一套，否则会出现「Web 能装、`Skill` 工具找不到」。

### 1.2 `/skills` 命令现状

- 命令声明：`name: "skills"`，别名 `skill`，参数提示 `[list|install <path>|help|<skill> [args]]`（`commands/src/lib.rs:282-285`）。
- 入口：`handle_skills_slash_command`（`:2647`）与 JSON 版 `handle_skills_slash_command_json`（`:2712`）；分类器 `classify_skills_slash_command`（`:2778`）。
- `install` 的参数解析仅支持一个路径（`:1993-2006`），**只接受本地目录或 `.md` 文件**，不接受 git URL。
- 安装实现：`install_skill` → `install_skill_into`（`:3542`、`:3547`）。行为：
  1. `resolve_skill_install_source` 规范化路径，目录必须含 `SKILL.md`，或本身是 `.md`（`:3608-3648`）；
  2. 名字取 frontmatter `name`，否则取目录名/文件名，经 `sanitize_skill_invocation_name` 归一（小写、`[a-z0-9._-]`、空白与斜杠转 `-`）（`:3650-3697`）；
  3. 目标已存在直接 `AlreadyExists` 报错（`:3559-3567`），**没有覆盖/升级语义**；
  4. 递归复制，失败则 `remove_dir_all` 回滚（`:3569-3581`）。
- 安装根：`default_skill_install_root` = `$CLAW_CONFIG_HOME/skills` → `$CODEX_HOME/skills` → `$HOME/.claw/skills`（`:3592-3606`）。
  - **P0 已修正**：原实现只看 `HOME`，Windows 上 `HOME` 常为空会退化成相对路径。现改为 `CLAW_CONFIG_HOME` → `CODEX_HOME` → `runtime::default_config_home()`（后者本身按 `HOME` → `USERPROFILE` → 相对 `.claw` 兜底）。若最终解析到相对 `.claw`，直接返回 `NotFound` 而不是生成相对安装根。
- 输出：文本报告（`:4393`）与 JSON（`:4414`，`kind/action/...`；列表 JSON 在 `:4376`，结构为 `kind/action/summary{total,active,shadowed}/skills[]`）。

**结论**：后端不需要重写安装逻辑，只需要把 `install_skill_into` 提升为 `pub` 并加一个「允许覆盖」参数 + 返回公开 DTO。现有 `InstalledSkill`（`:2458`）是私有结构，需转成公开结构体。

### 1.3 插件 registry 的形状（作为对照，不复用）

- 文件名常量 `installed.json`（`plugins/src/lib.rs:18`）；位置 `config_home/plugins/installed.json`（`:1063-1070`）。
- 记录结构 `InstalledPluginRecord`：`kind/id/name/version/description/install_path/source/installed_at_unix_ms/updated_at_unix_ms`（`:358-370`）；容器是 `BTreeMap<id, record>`（`:372-376`）。
- 安装根 `config_home/plugins/installed/`（`:1055-1060`）。
- 安装流程已有正确范式可抄：`install_root/.tmp` 暂存 → `materialize_source`（git 源 `git clone --depth 1`，`:2167-2198`）→ 校验 manifest → 复制到最终目录 → 写注册表 → 默认启用（`:1127-1168`）。
- **启停状态不在 `installed.json`**：`write_enabled_state` 写 `config_home/settings.json` 的 `enabledPlugins`（`:1494-1510`，路径 `:1073-1075`）。
- 写注册表是 `fs::write` 直写，非原子（`:1485-1492`）——技能这边要改成 tmp + rename。

### 1.4 serve 的路由组织

- 单文件 axum，单个 `Router`，全部端点集中在 `serve.rs:592-635`；`AppState` 定义在 `:44-60`（token / sessions / workspace / port / scheduler / events / approvals）。
- 鉴权是每 handler 手动 `authorize(&state, &auth)`（`:3210-3212`），失败 `unauthorized()` 返回 401 纯文本「缺少或无效的访问令牌」（`:3214-3216`）。query 提取器统一 `Query<AuthQuery>`（`t`）。
- 返回风格：成功 `Json(json!({...})).into_response()`；失败 `(StatusCode::X, "中文文案".to_string()).into_response()`，**不是 JSON error body**。已有 `422 UNPROCESSABLE_ENTITY` 先例（`:872`）。
- `GET /` 是内联单页（`:726`、`:40`）；`/api/meta` 可作最小 handler 模板（`:3105-3115`）。
- **当前 serve 完全没有 skill 相关端点**（全文件 grep `skill` 零命中）。

### 1.5 前端现状

- hash 路由：`ROUTES = ['chat','plugins','skills']`，`parseRoute` 只接受精确匹配，未知回落 `chat`（`web/src/shared/router.js:5-11`）；`navigate` 对 `chat` 用 `replaceState` + 自定义事件（`:13-22`）。
- 市场页：`MarketPage({kind})`，纯占位，两套文案，无请求（`web/src/market/MarketPage.jsx:20-43`）。
- 挂载：`App.jsx:553-555` 构造 `marketMain`，`:583` 按 `chatRoute` 二选一。
- 侧栏入口：`Sidebar.jsx:67-68` 两个按钮（插件市场 / 技能市场）。
- 请求封装：`api.js` 统一带 `t`、非 2xx 抛 `HTTP <status> · <text 前 200 字>`（`:14-22`），`postJson`（`:24-29`）。
- 已有可复用的渲染器：`web/src/markdown.jsx`（SKILL.md 预览用）。
- 构建：`vite build` + `vite-plugin-singlefile`（`web/package.json:6-19`），产物 `web/dist/index.html` 被 Rust 编译期内联。

### 1.6 本轮重审发现的三个真实缺口（先修或先兜底）

| 编号 | 缺口 | 证据 | 处置 |
| --- | --- | --- | --- |
| R1 | Windows 上 `HOME` 常为空，`default_skill_install_root` 直接失败；用户级发现也漏掉 `%USERPROFILE%\.claw\skills` | `commands:3599` 只读 `HOME`；`3413` 同样只读 `HOME`；`3528` 内置根才兜底 `USERPROFILE` | 统一改用 `runtime::config::default_config_home()`（`runtime/src/config.rs:966-972` 已处理 `CLAW_CONFIG_HOME → HOME → USERPROFILE`） |
| R2 | `SkillSummary` 没有路径字段，Web 列表拿不到「这个技能在哪」 | 结构定义 `commands:2374-2381` 仅 name/description/source/shadowed_by/origin | 在加载阶段补 `path: PathBuf`（内部结构，需同步 `skill_summary_json`），或在 API 层对每个 name 调 `resolve_skill_path`（O(根数×目录数)，可接受但不稳） |
| R3 | 存在第二套兼容发现根 | `tools:6632-6697` vs `commands:3327` | 市场只以 `commands` 为准；短期不改 `tools`，但在文档和测试里锁定「两者必须都能解析已安装技能」 |

---

## 2. 数据模型

### 2.1 三个目录，职责不重叠

```
~/.claw/skills/                 # 发现根（必须存在；内含 <name>/SKILL.md）
~/.claw/skill-market/
    installed.json              # 安装清单（只记录市场安装的技能）
    state.json                  # 启停状态（可记录任意技能名）
    sources.json                # 索引源列表
    staging/<uuid>/             # 安装暂存（在发现根之外！）
    trash/<uuid>/               # 卸载暂存（在发现根之外！）
```

- 注册表与暂存区放在 `~/.claw/skill-market/` 而不是 `~/.claw/skills/` 内：发现层会遍历 skills 根的所有子目录，虽然没有 `SKILL.md` 的目录会被跳过（`commands:3975-3981`），但**复制到一半的暂存目录可能已经含 `SKILL.md`**，会被并发请求当成真技能。移出发现根从结构上消除这个窗口。
- `installed.json` / `state.json` 是普通文件，即使放在 skills 根也会被 `!entry.path().is_dir()` 跳过，但仍不值得冒风险。

### 2.2 `installed.json`

```json
{
  "schema": "claw.skill-installed/v1",
  "skills": {
    "frontend-design": {
      "name": "frontend-design",
      "display_name": "Frontend Design",
      "version": "1.2.0",
      "marketplace": "team-market",
      "source": {
        "type": "git",
        "value": "https://example.com/skills.git",
        "subdir": "skills/frontend-design",
        "resolved_ref": "a1b2c3d"
      },
      "checksum": "sha256:9f2c…",
      "installed_at_unix_ms": 1762732800000,
      "updated_at_unix_ms": 1762732800000,
      "managed": true
    }
  }
}
```

| 字段 | 是否必需 | 说明 |
| --- | --- | --- |
| `name` | 是 | 主键，等于安装目录名，等于调用名（`$name`） |
| `version` | 是 | 索引给的版本；缺省 `0.0.0+unknown`。仅展示与升级判断，不参与发现 |
| `marketplace` | 是 | 来源市场的 `name`，用于「来源已删除但技能还在」时展示 |
| `source` | 是 | 复现安装所需的最小信息（git 要 `resolved_ref` 才能固定版本） |
| `checksum` | 否 | 见 §3.4；缺失时记 `null`，不做强校验 |
| `managed` | 是 | `true` = 市场安装，可卸载；`false` 保留给未来的「接管」场景，本期不写 |

### 2.3 `state.json`：为什么和 `installed.json` 分开

```json
{
  "schema": "claw.skill-state/v1",
  "disabled": ["some-builtin-skill"]
}
```

三个理由：

1. **生命周期不同。** `installed.json` 是「磁盘上有什么」的事实清单；`state.json` 是「用户想不想用」的偏好。项目级/内置技能不在清单里，但用户依然可能想关掉——把偏好塞进安装清单，就得为未安装的技能伪造记录，清单立刻失真。
2. **卸载要能彻底。** 卸载时删记录 + 删目录 + 从 `disabled` 移除，三件事顺序明确；若合成一个文件，一次写失败就可能出现「目录没了但 enabled=false 残留」。
3. **和插件现状的差异是有意的。** 插件把 enabled 放进 `settings.json` 的 `enabledPlugins`（`plugins:1494-1510`）。技能不跟随，是因为插件的 `settings.json` 同时承载模型/权限等大量共享配置，而技能的启用偏好没有其他字段要共存；单独 `state.json` 让写入面更小，也避免用户手改配置时误删技能状态。**若评审坚持插件对称，可退化为 `settings.json` 的 `disabledSkills`，但需要接受「配置加载失败 → 启停全部失效」的耦合。**

**过滤点只有一个**：`list_skill_summaries`（`commands:4139`）之后。新增 `commands::list_enabled_skill_summaries(cwd, disabled: &BTreeSet<String>)`，Web 与 `tools` 的技能注入都走它。必须保证 `/skills list`、`SkillSearch`、系统提示 catalog（`render_skill_catalog`，`:4170`）看到的是同一份过滤结果，否则会出现「Web 显示已禁用，模型仍能调用」。

### 2.4 标识、版本、来源、校验、状态一览

| 维度 | 存哪 | 格式 | 备注 |
| --- | --- | --- | --- |
| 标识 | 目录名 + `installed.json` key | 小写 `[a-z0-9._-]`，由 `sanitize_skill_invocation_name`（`:3669`）保证 | 冲突即覆盖语义，需显式 `upgrade` |
| 版本 | `installed.json` + `marketplace.json` | SemVer 字符串，宽松校验 | SKILL.md 里没有 |
| 来源 | `installed.json.source` + `sources.json` | 结构化对象，不是裸字符串 | 便于「刷新源」和「重装」 |
| 校验 | `installed.json.checksum` | `sha256:<hex>` | 见 §3.4 |
| 启停 | `state.json.disabled` | 名字数组 | 不动目录 |

---

## 3. 索引协议 `marketplace.json`

### 3.1 文件格式

```json
{
  "schema": "claw.skill-market/v1",
  "name": "team-market",
  "description": "内部技能索引",
  "updated_at": "2026-11-10T00:00:00Z",
  "skills": [
    {
      "name": "frontend-design",
      "version": "1.2.0",
      "description": "Create distinctive, production-grade frontend interfaces.",
      "path": "skills/frontend-design",
      "sha256": "9f2c…",
      "tags": ["frontend", "design"],
      "min_claw_version": "0.1.0"
    }
  ]
}
```

| 字段 | 必需性 | 契约 |
| --- | --- | --- |
| `schema` | 建议 | 只认 `claw.skill-market/v1`；不匹配 → 该源标记 `unsupported`，不阻塞其他源 |
| `name` | 必需 | 源的稳定标识；缺省时用源路径/URL 的末段 |
| `skills[].name` | **必需** | 安装名；必须能被 `sanitize_skill_invocation_name` 归一且归一后不变（否则 422，避免「索引名」与「安装名」不一致） |
| `skills[].path` | **必需** | 相对 `marketplace.json` 所在目录；目录或 `.md` 文件 |
| `skills[].version` | 建议 | 缺失 → `0.0.0+unknown`，仍可安装，UI 打「版本未知」 |
| `skills[].description` | 可选 | 缺失时取 `SKILL.md` frontmatter，再缺则空串 |
| `skills[].sha256` | 可选 | 缺失 → 可安装，UI 提示「未提供校验值」 |
| `skills[].tags` | 可选 | 仅前端筛选 |
| `skills[].min_claw_version` | 可选 | 与 `env!("CARGO_PKG_VERSION")` 比较（`serve.rs:3110` 已在用），不满足 → 条目标 `incompatible`，不可安装 |

### 3.2 本地文件源

- `type: "local"`，`value` 是 `marketplace.json` 的绝对路径，或包含它的目录。
- 解析：`canonicalize(value)`，若是目录则拼 `marketplace.json`；每个条目 `canonicalize(join(path))` 后必须仍以源的 `canonical` 前缀开头，否则该条目 `invalid_path`（防 `../../../` 逃逸）。
- 刷新成本极低，来源列表每次请求现读，不做缓存。

### 3.3 git 源

- `type: "git"`，`value` 是 URL；可选 `subdir` 指向仓库内的 marketplace 目录。
- 解析沿用插件范式：`git clone --depth 1 <url> <staging>/src-<uuid>`（参考 `plugins:2167-2198`），然后读 `<clone>/<subdir>/marketplace.json`。
- `resolved_ref` = `git -C <clone> rev-parse --short HEAD`，安装时写进 `installed.json`，保证「同一版本可复现」。
- 刷新 = 重新 clone。不做 `git pull`，不做增量，不做 tag 解析（本期）。
- 失败一律归类：`git_not_found`（本机无 git）/ `clone_failed`（网络、鉴权）/ `timeout`，文案见 §4.6。

### 3.4 一条技能最少需要什么才能安装

**最小集合：`name` + `path`，且 `path` 处必须存在可读的 `SKILL.md`。** 满足即安装成功；其余字段全部可降级。

降级矩阵：

| 缺失项 | 行为 |
| --- | --- |
| `version` | 安装，记 `0.0.0+unknown`；升级判断退化为「同名即冲突」 |
| `sha256` | 安装，记 `checksum: null`，UI 黄标「未校验」 |
| `description` | 从 `SKILL.md` frontmatter 补；仍无则空串 |
| `tags` / `min_claw_version` | 忽略 |
| `schema` | 按 v1 处理，仅日志告警 |
| `name` 或 `path` | **不安装**，条目在索引里可见但按钮禁用，附原因 |

### 3.5 校验做到什么程度（明确不做 PKI）

足够即可的四条：

1. **路径约束**：所有 `path` 规范化后必须落在源根内；拒绝符号链接逃逸（`canonicalize` 后前缀比对）。
2. **内容约束**：目标必须是目录（含 `SKILL.md`）或单个 `.md`；目录总大小 ≤ 2 MB、文件数 ≤ 200；拒绝 `..`、绝对路径、以 `.` 开头的暂存目录名。
3. **一致性**：`SKILL.md` frontmatter 的 `name`（若有）归一后必须等于索引 `name`，否则 422（防止装进去却按另一个名字被调用）。
4. **完整性（可选）**：索引给了 `sha256` 就校验，算法为「按相对路径字典序遍历文件，逐个喂 `路径字节 + 0x00 + 内容字节`」的树摘要；不匹配 → 422 且不落盘。

**明确不做**：签名、证书链、发布者身份、透明日志、远程吊销列表。理由：技能是纯文本提示词，安装者与使用者是同一人（本地单人 WebUI），威胁模型是「误装」而非「供应链投毒」；真正的风险控制是 §5 的安装前全文预览 + 首次信任确认。

---

## 4. 后端接口

风格约束（对齐 `serve.rs`）：全部挂在 `/api` 下；token 走 `?t=`；成功 `Json`；失败 `(StatusCode, 中文纯文本)`；不引中间件，每个 handler 第一行 `authorize`。新增状态只加两个字段到 `AppState`：`skill_install_locks: Arc<Mutex<HashSet<String>>>`、`skill_market_root: PathBuf`。

### 4.1 端点总表

| # | 方法 | 路径 | 作用 | 成功 |
| --- | --- | --- | --- | --- |
| 1 | GET | `/api/skills` | 本地技能目录（含状态/遮蔽/来源） | 200 JSON |
| 2 | GET | `/api/skills/{name}` | 单个技能详情 + 文件清单 | 200 JSON |
| 3 | GET | `/api/skills/{name}/raw?path=SKILL.md` | 原文预览 | 200 text/plain |
| 4 | GET | `/api/skill-sources` | 来源列表 + 最近状态 | 200 JSON |
| 5 | POST | `/api/skill-sources` | 新增来源（立即校验） | 201 JSON |
| 6 | DELETE | `/api/skill-sources/{id}` | 删除来源（不动已装技能） | 204 |
| 7 | POST | `/api/skill-sources/{id}/refresh` | 重新解析（git 会 clone） | 200 JSON |
| 8 | GET | `/api/skill-market?source={id}` | 该源的技能索引 | 200 JSON |
| 9 | POST | `/api/skills/install` | 安装 | 201 JSON |
| 10 | PATCH | `/api/skills/{name}` | 启停 | 200 JSON |
| 11 | DELETE | `/api/skills/{name}` | 卸载 | 204 |

### 4.2 请求 / 响应契约

**1. `GET /api/skills`**

```json
{
  "summary": { "total": 12, "active": 10, "shadowed": 1, "disabled": 1, "managed": 3 },
  "skills": [{
    "name": "frontend-design",
    "description": "…",
    "source": "project|user|user_config|builtin|installed",
    "origin": "skills_dir|legacy_commands",
    "path": "C:\\…\\.claw\\skills\\frontend-design",
    "enabled": true,
    "shadowed_by": null,
    "managed": true,
    "version": "1.2.0",
    "marketplace": "team-market"
  }]
}
```
数据源：`load_skills_from_roots`（要 shadowed 全量）+ `installed.json` + `state.json` 左连接。`path` 依赖 §1.6 R2 的补字段。

**2. `GET /api/skills/{name}`**：在上一条基础上加 `files: [{"path":"SKILL.md","size":1234}]`、`installed: {…}|null`、`invoke_as: "$name"`。

**3. `GET /api/skills/{name}/raw?path=…`**：`text/plain; charset=utf-8`。`path` 缺省 `SKILL.md`；仅允许该技能目录内的相对路径。上限 512 KB（沿用 `serve.rs:2133` 的既有阈值口径）。

**4. `GET /api/skill-sources`**

```json
{ "sources": [{
  "id": "s_01",
  "type": "local|git",
  "value": "D:\\tmp\\market",
  "name": "team-market",
  "subdir": null,
  "status": "ok|unreadable|unsupported|git_not_found|clone_failed|timeout",
  "skill_count": 4,
  "last_error": null,
  "checked_at_unix_ms": 1762732800000
}] }
```

**5. `POST /api/skill-sources`**：body `{"type":"local|git","value":"…","name":"…","subdir":null}`。立即解析一次：成功 201 返回该 source（含 `skill_count`）；解析失败也要落盘并返回 201 + `status`（让用户能修路径），只有 `type` 非法/`value` 为空才 400。

**7. `POST /api/skill-sources/{id}/refresh`**：重新解析并更新 `sources.json` 的 `status/checked_at`；git 失败返回 502（因为这是用户主动触发的操作，必须报错），body 为文案。

**8. `GET /api/skill-market?source={id}`**

```json
{ "source": { "id": "s_01", "name": "team-market", "resolved_ref": "a1b2c3d" },
  "skills": [{
    "name": "frontend-design",
    "version": "1.2.0",
    "description": "…",
    "tags": ["frontend"],
    "installable": true,
    "reason": null,
    "installed": { "version": "1.0.0", "enabled": true },
    "checksum": "sha256:…"
  }] }
```
`installable=false` 的 `reason` 枚举：`missing_name` / `missing_path` / `path_not_found` / `path_escape` / `missing_skill_md` / `name_mismatch` / `incompatible` / `too_large`。

**9. `POST /api/skills/install`**：body `{"source_id":"s_01","name":"frontend-design","version":"1.2.0","upgrade":false}`。
响应 201：

```json
{ "name": "frontend-design", "version": "1.2.0", "path": "C:\\…\\.claw\\skills\\frontend-design",
  "enabled": true, "checksum": "sha256:…", "shadowed_by": null }
```

**10. `PATCH /api/skills/{name}`**：body `{"enabled": false}` → 200 `{"name":"…","enabled":false}`。

**11. `DELETE /api/skills/{name}`**：204，无 body。

### 4.3 安装：同步还是异步

**同步**，但分两条执行路径：

- `local`：直接在 handler 内做（目录复制毫秒级），超过 5s 视为异常并 500。
- `git`：`tokio::task::spawn_blocking` 内 clone + 复制，`timeout(30s)`；超时返回 504 并清理暂存。
- 网络传输只发生在 clone 阶段，安装本身是本地 rename，不存在长时间占用。

不做「异步任务 + 轮询」的原因：单人本地场景，一次只能装一个技能，UI 一个 spinner 足够；引入 job 表会带来「服务重启后 job 丢失」的新状态，收益为零。若后续要「一次装整个索引」，再升级为 `202 + /api/skill-jobs/{id}`，本期明确不做。

### 4.4 失败回滚（顺序敏感）

安装：

1. 解析索引条目（校验 §3.5），失败 → 422，**磁盘零写入**。
2. 复制到 `~/.claw/skill-market/staging/<uuid>/`（git 源先在 staging 内 clone）。
3. 在 staging 内校验 `SKILL.md` 存在、frontmatter 名字一致、大小上限。
4. 若目标 `~/.claw/skills/<name>` 已存在：`upgrade=false` → 409；`upgrade=true` → 先把旧目录 rename 到 `trash/<uuid>`（保留到注册表写成功）。
5. `fs::rename(staging/<uuid>, skills/<name>)`（同卷原子）。
6. 写 `installed.json.tmp` → `rename` 覆盖；写 `state.json` 移除该名字（若曾被禁用）。
7. 任一步 5-6 失败：把旧目录从 `trash` rename 回去、删除新目录、删除注册表 tmp，返回 500「安装失败，已回滚」。

**关键点**：注册表最后写；目录先落地。这样「目录在、清单缺」最多表现为「未受管技能」，不会出现「清单说有、磁盘没有」。

卸载：

1. 非 `managed` → 409「只能卸载从市场安装的技能」。
2. `rename(skills/<name>, trash/<uuid>)`。
3. 删 `installed.json` 记录 + `state.json` 中的名字（各自 tmp+rename）。
4. 4 成功后 `remove_dir_all(trash/<uuid>)`；若第 3 步失败 → rename 回原位并 500。

### 4.5 并发安装同一技能

`AppState.skill_install_locks: Arc<Mutex<HashSet<String>>>`：进入安装前 `insert(name)`，已存在 → **409「技能 frontend-design 正在安装，请稍候」**；用 guard 结构在 drop 时移除（含 panic 路径）。单进程 + 单人，够用，不引文件锁。

不同技能并发安装是允许的（各自独立 staging），但 `installed.json` 写入需要同一把 `Mutex<()>` 串行化，避免丢更新。

### 4.6 错误码与文案（直接给可粘贴的中文）

| 码 | 场景 | 文案 |
| --- | --- | --- |
| 400 | 缺 token | 缺少或无效的访问令牌（复用 `unauthorized`） |
| 400 | body 非法 / `type` 非 local|git / `value` 空 | 来源类型必须是 local 或 git；路径不能为空 |
| 400 | `enabled` 非布尔 | enabled 必须是布尔值 |
| 404 | 技能不存在 | 技能 {name} 不存在 |
| 404 | 来源不存在 | 来源 {id} 不存在 |
| 409 | 同名已安装且未 upgrade | 技能 {name} 已安装（{version}）；如需覆盖请勾选「升级安装」 |
| 409 | 并发安装 | 技能 {name} 正在安装，请稍候 |
| 409 | 卸载非受管技能 | 只能卸载从市场安装的技能；项目或内置技能请用「禁用」 |
| 409 | 卸载正在安装 | 技能 {name} 正在安装，暂时不能卸载 |
| 422 | 索引条目字段缺失 | 索引条目 {name} 缺少 {field}，无法安装 |
| 422 | 路径越界 | 技能路径越出来源目录：{path} |
| 422 | 无 SKILL.md | {path} 下没有 SKILL.md，不是有效技能目录 |
| 422 | 名字不一致 | SKILL.md 声明的名字 {actual} 与索引 {expected} 不一致 |
| 422 | 校验和不匹配 | 技能内容校验失败（期望 {expected}，实际 {actual}） |
| 422 | 大小超限 | 技能目录过大（超过 2MB 或 200 个文件） |
| 422 | 版本不兼容 | 需要 claw ≥ {min}，当前 {current} |
| 502 | clone 失败 | 拉取仓库失败：{stderr 首行}。请检查网络或仓库地址 |
| 502 | 无 git | 本机未找到 git，无法安装 git 来源的技能 |
| 504 | clone 超时 | 拉取仓库超时（30 秒），请重试或改用本地来源 |
| 500 | IO | 安装失败：{error}（已回滚） |

---

## 5. 前端页面

### 5.1 路由

`shared/router.js` 最小改动：

- `ROUTES` 不变（`chat/plugins/skills`）。
- `parseRoute` 增加前缀规则：`hash` 以 `skills/` 开头时仍返回 `skills`；新增导出 `parseSkillSub(hash)` 返回 `{view: 'list'|'installed'|'sources'|'detail', name?}`。
- `navigate('skills')` 清 hash 的现有行为保持不变；详情用 `#/skills/<name>`、来源用 `#/skills/sources`。
- 这样 `App.jsx:583` 的 `chatRoute` 判断和 `Sidebar.jsx:67-68` 的高亮都不用改。

### 5.2 组件拆分（全部在 `web/src/market/` 内，不新建顶层目录）

| 文件 | 职责 |
| --- | --- |
| `MarketPage.jsx` | 保留插件占位分支；`kind === 'skills'` 时渲染 `SkillMarketPage` |
| `SkillMarketPage.jsx` | 持有 `view` / `selected` / `marketIndex` / `loading` 等 `useState`；四个子视图切换 |
| `SkillList.jsx` | 本地目录 + 分面：全部 / 已安装 / 已禁用 / 被遮蔽；搜索框（纯 `filter`） |
| `SkillDetail.jsx` | 元信息（来源/版本/路径/`$invoke`）+ `markdown.jsx` 渲染 `SKILL.md` + 操作按钮 |
| `SkillSources.jsx` | 来源列表、添加表单（local/git 二选一）、刷新、删除 |
| `SkillInstallList.jsx` | 某来源的索引条目 + 「安装/升级」按钮 + 不可安装原因 |
| `ConfirmDialog.jsx` | 若 `web/src/shared/` 无现成模态，则新增；只有确认/取消两个动作 |

**状态管理**：不引库。每个页面自己 `useState` + `useEffect` 拉数据；写操作成功后重拉当前视图（不做乐观更新，避免与磁盘真实状态打架）。跨页面共享的只有 `token`（`api.js` 已有）。

### 5.3 交互与确认弹窗

必须二次确认：

| 操作 | 弹窗文案要点 |
| --- | --- |
| 卸载 | 标题「卸载 {name}？」正文「将删除 ~/.claw/skills/{name}，且不可恢复。项目里的同名技能不受影响。」 |
| 升级覆盖 | 「已安装 {old}，将覆盖为 {new}」+ 显示校验和是否一致 |
| 删除来源 | 「只删除索引来源，已安装的技能不会被卸载」 |
| 首次从 git 来源安装 | 「技能内容会作为提示词注入模型上下文。安装前请确认来源可信。」+ 强制先展开全文预览 |
| 禁用正在被项目遮蔽的技能 | 不弹窗，但按钮置灰并提示「该技能已被 {source} 遮蔽，禁用不会改变实际行为」 |

不需要确认：启用、切 tab、刷新索引。

### 5.4 SKILL.md 预览

- 列表页不预取正文；进入详情才 `GET /api/skills/{name}/raw?path=SKILL.md`。
- 用现有 `markdown.jsx` 渲染；顶部固定一条「原始文本 / 渲染」切换（原始视图用 `<pre>`，长文件 `max-height` 内滚动）。
- 超过 512 KB 后端直接 413，前端展示「文件过大，仅支持在线预览 512KB 以内」+ 复制路径按钮。
- 安装前预览（来源索引页）复用同一组件，数据直接从索引源读取（若源为本地目录，后端在 `/api/skill-market` 响应里带 `skill_md_preview` 前 8 KB，避免额外请求）。

### 5.5 页面状态清单

- 列表：`loading | empty | error(带重试) | ok`。
- 详情：`loading | not_found | ok`；`shadowed_by` 存在时顶部黄条。
- 来源：每行独立 `status`（ok/unreadable/clone_failed…），刷新按钮有行内 spinner。
- 安装：按钮 `idle | installing | done`，失败在按钮下方就地显示 §4.6 文案，不弹 toast（`Toasts` 留给全局错误）。

---

## 6. 分期

每期都能独立上线；判据全部可在本机用命令验证，不写主观描述。

### 6.0 基线核实与勘误（2026-11-10 实测）

开工前对本路线之前的口头结论做了实测核对：

| 项目 | 此前口头结论 | 实测结果 |
| --- | --- | --- |
| 工作区状态（本轮开工前） | 「S2a 改动仍在工作区未提交」 | **错误**。`git status --short` 仅有未跟踪的 `docs/skill-market-roadmap.md`；最新提交为 `36a51ce3`，与另一路报告一致。 |
| 构建 | 「当前构建可能未通过」 | **错误**。`cargo check --workspace --all-targets` 全量通过（约 12s）。 |

结论：S2a 已随 `36a51ce3` 之前的历史提交合入，工作区在 P0 开工前是干净的。本节下方 P0 改动产生的未提交文件属于本次执行范围，不是遗留问题。

### P0：只读目录（1 个 PR）

- 范围：`SkillSummary` 补 `path`；`GET /api/skills`、`GET /api/skills/{name}`、`/raw`；前端列表 + 详情 + 预览；`parseSkillSub`；R1 安装根统一到 `default_config_home()`。
- 上线判据：
  1. `cd rust && cargo test -p commands -p rusty-claude-cli` 全绿；新增至少 3 个单测：路径归一、shadow 顺序、frontmatter 缺失。
  2. `cargo clippy --workspace --all-targets -- -D warnings` 无告警。
  3. `curl "http://127.0.0.1:<port>/api/skills?t=<token>"` 返回的 `skills[].name` 集合，与 `claw --skills list` 输出的名字集合完全一致（脚本 `diff`）。
  4. `curl ".../api/skills/<name>/raw"` 的字节数等于磁盘 `SKILL.md` 的字节数。
  5. 浏览器打开 `#/skills`，列表条数等于第 3 步的 JSON 条数。
- 回滚：删除三个路由 + 前端三个文件；`dist` 回退到上一提交。无数据写入，无迁移。
- **P0 实测记录（2026-11-10，本机 Windows）**：
  | 判据 | 实测 |
  | --- | --- |
  | `cargo test -p commands` | 55 passed / 0 failed（含新增 `skill_summary_exposes_prompt_file_path`、`skill_install_root_never_falls_back_to_relative_path`） |
  | `cargo test -p rusty-claude-cli` | 687 passed / 0 failed / 1 ignored。`session_list_surfaces_saved_dirty_abandoned_lifecycle` 在并行全量跑时偶发 `remove_dir_all` 失败（单测独跑稳定通过），属既有竞态，与本轮改动无关 |
  | `cargo clippy --workspace --all-targets -- -D warnings` | 通过，无告警 |
  | `cargo fmt --all -- --check` | 通过（本机无 bash，用 cargo fmt 等价替代 `scripts/fmt.sh --check`） |
  | 判据 3：API 名字集合 vs `claw --output-format json skills list` | 146 vs 146，排序后完全一致 |
  | 判据 4：`/raw` 字节数 | `demo` 70=70、`shadow-runner` 65=65 |
  | 判据 5：`#/skills` 列表条数 | 浏览器实测 `.skill-item` = 146，与 API 一致；页面显示「74 / 146 生效」 |
  | HOME 兜底（`HOME` 未设置、`USERPROFILE=C:\Users\38225`） | `summary.install_root = C:\Users\38225\.claw\skills` |
  | HOME 兜底（`HOME`、`USERPROFILE` 全未设置） | `summary.install_root = ""`（不回退相对路径） |
  | 安全性 | 路径逃逸 400、未知技能 404、缺 token 401 |

### P1：本地来源 + 安装/卸载

- 范围：`sources.json`、`installed.json`、`GET/POST/DELETE /api/skill-sources`、`refresh`、`GET /api/skill-market`、`install`、`uninstall`；`install_skill_into` 公开化 + `upgrade` 参数；暂存/垃圾桶目录。
- 判据：
  1. 准备 `D:\tmp\market\marketplace.json` 指向 `skills/demo`，安装后 `%USERPROFILE%\.claw\skills\demo\SKILL.md` 存在，`installed.json` 有一条记录。
  2. `claw --skills list` 出现 `demo`；`/skills demo` 能加载（打印其 JSON 输出）。
  3. 同一条目再次安装（`upgrade:false`）返回 409，目录 mtime 不变。
  4. 把 `SKILL.md` 改名为 `SKILL.md.bak` 后安装一个未安装条目 → 422，`skills\` 下无新目录，`staging\` 为空。
  5. 卸载后目录消失、`installed.json.skills` 为 `{}`、`trash\` 为空。
  6. 两个终端同时 `POST install` 同名 → 一个 201、一个 409（文案含「正在安装」）。
- 回滚：删 `~/.claw/skill-market/` 与已装技能目录；卸掉路由。已安装技能本身就是普通目录，回退后 CLI 仍可用。
- **实现偏差（2026-11-10）**：`install_skill_into` 未被市场复用 —— 它直接写最终目录，不满足「staging → 校验 → rename → 注册表」的原子性要求。市场流水线改为复用 `commands::copy_directory_contents` 与 `commands::sanitize_skill_invocation_name`（两者已公开），`/skills install` 行为不变。git 来源在 P1 接受落盘但固定 `status=unreadable`、不参与安装（P3 实现）。
- **P1 实测记录（2026-11-10，本机 Windows）**：

  | 判据 | 实测 |
  | --- | --- |
  | 单测 | `cargo test -p rusty-claude-cli skill_market::tests`：9 passed（原子安装、两种故障注入回滚、并发一胜一负、卸载+prompt 重建、路径逃逸、校验和不匹配） |
  | 判据 1：安装落盘 | `D:\tmp\p1-config\skills\demo\SKILL.md` 存在；`installed.json` 1 条记录，checksum `sha256:4f5ac45b…` |
  | 判据 2：CLI 可见 | `claw --output-format json skills list` 含 `demo`（source=`user_claw_config_home`） |
  | 判据 3：重复安装 | `upgrade:false` → 409「技能 demo 已安装（1.2.0）；如需覆盖请勾选「升级安装」」；目录 mtime `2026-11-10T09:22:33.0974406Z` 前后一致 |
  | 判据 4：校验失败 | `SKILL.md` 改名后安装 → 422「skills/broken 下没有 SKILL.md，不是有效技能目录」；`skills\` 无新目录、`staging\` 0 项、注册表仍 1 条 |
  | 判据 5：卸载 | 204；目录消失；`installed.json.skills == {}`；`trash\` 与 `staging\` 均为 0 项 |
  | 判据 6：并发同名 | 两个并发 POST（.NET `HttpClient`）→ 201 + 409「技能 broken 正在安装，请稍候」；仅 1 个目录、1 条记录、staging 0 项 |
  | 故障注入回滚 | `Fault::ValidateAfterCopy` / `Fault::RenameToTarget`：新装与升级两条路径均恢复安装前状态（旧目录内容逐字节一致，staging/trash 清空，注册表不变） |
  | prompt 重建 | 单测调用真实 `app::load_skill_catalog`：安装后 catalog 含 `aaa-demo`，卸载后不再包含（`skill_catalog_fingerprint` 覆盖 SKILL.md mtime/长度，缓存自然失效） |
  | 浏览器 | `#/skills/sources` 显示 p1-market「可用」并展开 demo/broken 与安装按钮；`#/skills` 三页签正常、列表 145 条；`#/skills/installed` 空状态文案正确 |
  | 安全 | 路径逃逸 422、无 SKILL.md 422、索引名不一致 422、校验和不匹配 422 |
  | 回归 | `cargo clippy --workspace --all-targets -- -D warnings` 通过；`cargo fmt --all -- --check` 通过；`cargo test --workspace` 除既有 flake `session_list_surfaces_saved_dirty_abandoned_lifecycle`（单跑通过）外全绿 |
  | 环境相关既有失败 | `runtime::source_root` 与 `runtime::evolution_inbox` 两个测试在本机 `~/.claw/source-root` 指向本仓库时失败；`CLAW_CONFIG_HOME` 指向空目录后两者通过，与 P1 无关 |


### P2：启停

- 范围：`state.json`、`list_enabled_skill_summaries`、`PATCH /api/skills/{name}`、列表开关；`tools` 技能注入与系统提示 catalog 改用过滤后的列表。
- 判据：
  1. 关闭 `demo` → `GET /api/skills` 中 `enabled=false`；`claw --skills list` 与 `SkillSearch` 结果中不再出现 `demo`；`Skill` 工具按名加载返回 unknown。
  2. 重新开启 → 三处同时恢复。
  3. 关闭某内置技能（如 `frontend-design`）后，重启 WebUI 仍保持禁用（`state.json` 持久化）。
  4. 对比禁用前后 `skills\demo` 目录的 `mtime` 与文件列表，完全一致（证明未动目录）。
- 回滚：删除 `state.json`（等价于全部启用）；路由保留也不影响，因为过滤函数对空集合是恒等。

### P3：git 来源 + 超时/并发

- 范围：`materialize_source` 等价实现、`resolved_ref`、30s 超时、`git_not_found` 分类；来源页显示 ref。
- 判据：
  1. 本机建本地仓库：`git init D:\tmp\mrepo`（含 marketplace.json）→ 用 `file:///D:/tmp/mrepo` 作为来源，安装成功，`installed.json.source.resolved_ref` 等于 `git -C D:\tmp\mrepo rev-parse --short HEAD`。
  2. 故意填 `https://127.0.0.1:1/x.git` → 502，`staging\` 与 `trash\` 均为空。
  3. 用 PATH 里去掉 git 的环境启动 serve → 502 文案为「本机未找到 git」。
  4. 用一个会阻塞的假 git（PATH 前置一个 sleep 脚本）→ 30s 后 504，无残留目录。
- 回滚：来源条目标 `unreadable`，已安装技能不受影响；代码回退只需删 git 分支。

### P4：体验收尾

- 范围：搜索/标签筛选、升级版本对比、安装前全文预览、错误文案统一、来源批量刷新。
- 判据：`npm run build` 后 `dist/index.html` 体积增量 < 60 KB；连续安装/卸载 20 次后 `skill-market/` 下无残留 `staging`/`trash` 目录；§4.6 每个错误码至少手动触发一次且文案一致。
- 回滚：纯前端，回退 `dist` 即可。

---

## 7. 风险与不做的事

### 7.1 风险

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| 前端改动忘了 build `dist` | WebUI 显示旧页面，后端已变 | P0 起把 `npm run build` 写进验收判据；CI 加检查：`web/src` 有改动而 `web/dist` 无改动则失败 |
| 安装名被项目同名技能遮蔽 | 用户以为装好了但模型调不到 | 安装响应带 `shadowed_by`，UI 黄条提示「已被项目技能覆盖」 |
| 技能内容是提示词，可注入指令 | 恶意索引可诱导模型 | 安装前全文预览 + git 来源首装强确认；不执行技能内任何脚本（本来也不会执行） |
| Windows 跨卷 rename 失败 | 安装/卸载中途失败 | 暂存区与安装根固定在同一个 `%USERPROFILE%` 卷下；失败走 §4.4 回滚 |
| `installed.json` 手改损坏 | 目录有、清单无 | 读取失败按空清单处理 + 打日志；`GET /api/skills` 以磁盘为准，未受管技能标 `managed:false` 只读展示 |
| 两套发现根漂移（R3） | Web 能装、`Skill` 工具找不到 | 测试里加一条：已安装技能必须能被 `commands::resolve_skill_path` 与 `tools` 兼容路径同时解析 |
| 误删用户技能 | 数据丢失 | 卸载只允许 `managed:true`；删除进 `trash/` 后延迟清理（本期保留 7 天后由下次启动清理） |

### 7.2 明确不做（及原因）

1. **不做插件市场后端。** 插件已有 `plugin_registry`/`install`/`enable` 全套（`plugins/src/lib.rs:1077-1187`），与技能模型差异大（manifest、hooks、工具）；混在一个 PR 会同时动两个数据模型。市场骨架里的 `plugins` 分支继续占位。
2. **不做远程托管索引服务。** 本机单人自用，本地文件源 + git 源已覆盖「团队共享」场景；托管服务需要鉴权、CDN、可用性承诺，本期收益为负。
3. **不做签名/PKI/发布者身份。** 见 §3.5；先有预览和确认，再谈密码学。
4. **不做自动更新/定时检查。** 引入后台任务 = 引入「静默改文件」，与「可回滚」冲突；升级必须由用户点。
5. **不做依赖解析。** 技能之间没有依赖语义，索引里也不设计 `depends_on`。
6. **不做按技能配权限。** 技能只影响提示词，不新增工具权限；权限仍由会话的 permission mode 控制。
7. **不移动目录实现启停。** 用户已明确要求；且移动目录会改变 `source` 语义、破坏 shadow 优先级计算。
8. **不引状态库（Redux/Zustand/React Query）。** 四个视图、单用户、数据量 <100，`useState` + 重拉足够。
9. **不做安装进度 SSE 流。** 同步请求 + spinner；异步 job 留到「批量安装」真实出现时。
10. **不在 Web 端编辑 SKILL.md。** 只读预览；编辑属另一条产品线（会牵出版本与冲突合并）。
11. **不改 TUI。** 已放弃的路线，任何改动都会产生双份维护成本。

---

## 附录 A：锚点核查表（全部为本次实查）

| 结论 | 文件:行 |
| --- | --- |
| 技能命令声明与参数提示 | `rust/crates/commands/src/lib.rs:282-285` |
| `/skills` 文本 / JSON 入口 | `commands/src/lib.rs:2647`、`:2712` |
| 分类与调用解析 | `commands/src/lib.rs:2778`、`:2805-2840` |
| 路径解析（按 frontmatter 名匹配） | `commands/src/lib.rs:2898` |
| 根枚举（项目/用户/内置） | `commands/src/lib.rs:3327`、`:3330-3537` |
| 根去重且仅收已存在目录 | `commands/src/lib.rs:3751-3764` |
| 安装入口 / 安装根 / 源解析 | `commands/src/lib.rs:3542`、`:3592`、`:3608` |
| 名字归一 | `commands/src/lib.rs:3669-3697` |
| 加载与 shadow（先到先得） | `commands/src/lib.rs:3966`、`:4027-4037` |
| frontmatter 仅 name/description | `commands/src/lib.rs:4067-4127` |
| 仅活跃技能 + catalog 折叠 | `commands/src/lib.rs:4139-4152`、`:4170` |
| 列表/安装 JSON 输出 | `commands/src/lib.rs:4376`、`:4414` |
| `SkillSummary` 无 path | `commands/src/lib.rs:2374-2381` |
| 工具侧技能注入 | `rust/crates/tools/src/lib.rs:980-996` |
| 第二套兼容发现根 | `tools/src/lib.rs:6595-6697` |
| 插件注册表文件名/结构 | `rust/crates/plugins/src/lib.rs:18`、`:358-376` |
| 插件安装根 / 注册表路径 | `plugins/src/lib.rs:1055-1070` |
| 插件安装范式（暂存+clone） | `plugins/src/lib.rs:1127-1168`、`:2167-2198` |
| 插件启停写 settings.json | `plugins/src/lib.rs:1494-1510` |
| 插件注册表非原子写 | `plugins/src/lib.rs:1485-1492` |
| serve 路由总表 | `rust/crates/rusty-claude-cli/src/serve.rs:592-635` |
| AppState 字段 | `serve.rs:44-60` |
| 鉴权 / 401 | `serve.rs:3210-3216` |
| JSON 与纯文本错误风格 | `serve.rs:3105-3115`、`:872` |
| 内联单页与 dist 依赖 | `serve.rs:40`、`:726` |
| 前端 hash 路由 | `rust/crates/rusty-claude-cli/web/src/shared/router.js:5-22` |
| 市场骨架 | `web/src/market/MarketPage.jsx:20-43` |
| 市场挂载点 / 侧栏入口 | `web/src/App.jsx:553-555`、`:583`；`web/src/shell/Sidebar.jsx:67-68` |
| 请求封装 | `web/src/api.js:14-29` |
| 前端构建与 dist 提交 | `web/package.json:6-19`、`web/.gitignore:1` |
| 配置家目录统一入口 | `rust/crates/runtime/src/config.rs:966-972` |

**需确认（未逐行核到）**：

- 系统提示里技能 catalog 的注入点（推测在 `rust/crates/runtime/src/prompt.rs`，本次未打开确认行号）。
- `skill_summary_json` 的具体字段集合（`commands/src/lib.rs:4400` 附近，未逐字段核对；API 层若要直接复用它需先确认）。
- `web/src/shared/` 是否已有可复用的确认弹窗组件（本次只确认了 `errorBoundary.jsx`/`useToasts.js`/`icons.jsx`/`utils.js`/`constants.js`，无 `modal`/`dialog`；新增时需先全局搜一次 `dialog`/`modal`）。

## 附录 B：本机验收常用命令

```powershell
# 1) 后端单测
cd d:\claw-code-src\rust
cargo test -p commands -p rusty-claude-cli
cargo clippy --workspace --all-targets -- -D warnings

# 2) 起服务（端口与 token 看启动输出）
cargo run -p rusty-claude-cli -- serve --workspace d:\claw-code-src

# 3) 目录对齐检查
$t = "<token>"; $p = "<port>"
(curl "http://127.0.0.1:$p/api/skills?t=$t" | ConvertFrom-Json).skills.name | Sort-Object > api.txt
claw --skills list > cli.txt   # 名字集合应与 api.txt 一致

# 4) 前端
cd d:\claw-code-src\rust\crates\rusty-claude-cli\web
npm run build
# 确认 dist/index.html 已更新并提交，否则 serve 仍是旧页面
```
