//! LiveCli 执行内核：turn 执行、运行时构建、系统提示装配。
//!
//! Phase 3 起 CLI 界面层（REPL、斜杠命令、终端渲染）已删除，
//! 本模块只保留 `claw serve` 依赖的执行内核：构造会话与运行时、
//! 装配系统提示、执行单轮 turn，以及 Web SSE 需要的状态/取消接口。

#![allow(
    dead_code,
    unused_imports,
    unused_variables,
    clippy::unneeded_struct_pattern,
    clippy::unnecessary_wraps,
    clippy::unused_self
)]

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::env;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use api::model_token_limit;
use plugins::PluginRegistry;
use runtime::{
    ConfigLoader, ConversationRuntime, PermissionMode, RepoMap, Session, SystemPromptExtras,
};
use telemetry::{JsonlTelemetrySink, SessionTracer};
use tools::{init_lsp_from_config, GlobalToolRegistry};

use crate::api_client::{permission_policy, AnthropicRuntimeClient, HookAbortMonitor};
use crate::plugin_state::{build_runtime_plugin_state_for, RuntimeMcpState, RuntimePluginState};
use crate::session_mgr::{
    create_managed_session_handle_in, load_session_reference_in, new_cli_session_with_roots_in,
    SessionHandle,
};
use crate::tool_executor::CliToolExecutor;
use crate::AllowedToolSet;

// ===== 系统提示装配 =====

thread_local! {
    /// D1: 规范化 cwd → 共享 `ProjectTopology`。让 `build_system_prompt`
    /// （在 `load_prompt_extras` 之前构造并 `ensure_built`）与随后调用 14 处的
    /// `build_runtime` 复用同一个 `Arc`，避免重复 `cargo metadata`，并使 RepoMap
    /// 注入走内存拓扑（而非 `load_cache_quick` 退化路径）。按规范化 cwd 键控，
    /// 多 cwd 场景不串味（单一 `OnceLock` 会串味，故不可用）。
    static TOPOLOGY_REGISTRY: RefCell<HashMap<PathBuf, Arc<runtime::project_topology::ProjectTopology>>> =
        RefCell::new(HashMap::new());
}

/// D1: 取得（必要时创建）规范化 `cwd` 对应的共享 `ProjectTopology`。
fn topology_for(cwd: &Path) -> Arc<runtime::project_topology::ProjectTopology> {
    let key = runtime::topology_cache::normalize_root(cwd);
    TOPOLOGY_REGISTRY.with(|reg| {
        reg.borrow_mut()
            .entry(key.clone())
            .or_insert_with(|| Arc::new(runtime::project_topology::ProjectTopology::new(key)))
            .clone()
    })
}

pub(crate) fn build_system_prompt(
    model: &str,
    workspace: &Path,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    crate::diag_log(&format!("[build_system_prompt] start, model={model}"));
    let cwd = workspace.to_path_buf();
    crate::diag_log(&format!("[build_system_prompt] cwd={}", cwd.display()));

    // Step 4.3:在 repomap 渲染前初始化 LSP server。
    // build_system_prompt 先于 build_runtime 执行,若不在此提前 spawn,
    // load_prompt_extras 里的 is_server_spawned("rust") 恒为 false,
    // render_with_lsp(跨模块语义重要性)永远不会生效。
    // best-effort:未配置 lspServers 或 spawn 失败时不阻断启动。
    let loader = ConfigLoader::default_for(&cwd);
    if let Ok(config) = loader.load() {
        let _ = init_lsp_from_config(&config, &cwd);
    }

    // D1:提前构造共享 ProjectTopology 并 ensure_built(命中磁盘缓存即 Ready,
    // 否则派发后台构建),使下方 load_prompt_extras 能直接读内存符号索引;
    // 随后 build_runtime 经 topology_for 复用同一 Arc,不重复构建。
    let topology = topology_for(&cwd);
    let _ = topology.ensure_built();

    let extras = load_prompt_extras(&cwd, &topology);
    crate::diag_log(
        "[build_system_prompt] load_prompt_extras OK, calling load_system_prompt_with_extras",
    );
    // 系统提示词日期用启动时的真实日期(而非编译期 BUILD_DATE):
    // 编译期日期跨天后即失真;本函数仅在会话启动时调用一次,session 内字节稳定。
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let result = runtime::load_system_prompt_with_extras(
        cwd,
        &today,
        env::consts::OS,
        "unknown",
        api::model_family_identity_for(model),
        extras,
    )?;
    crate::diag_log("[build_system_prompt] load_system_prompt_with_extras OK");
    Ok(result)
}

/// 工作区级只读产物缓存(跨会话复用 repomap 渲染文本 / 技能目录文本)。
///
/// 只缓存由只读输入派生的渲染结果:缓存键 = 工作区路径 + 输入指纹
/// (源码文件 mtime / 技能定义文件 mtime+长度)。会话级状态(消息、
/// 轮次、工具结果、记忆写入)一律不进入缓存。
///
/// 落盘位置:`<workspace>/.claw/cache/prompt_extras/<name>.json`(gitignored,
/// 与既有 `.claw/repomap_cache.json` 同级)。读写失败一律降级为重新构建。
/// 逃生阀:`CLAW_DISABLE_PROMPT_EXTRAS_CACHE=1` 或 `CLAW_DISABLE_CACHE=1`。
const EXTRAS_CACHE_SCHEMA: u64 = 1;

fn extras_cache_disabled() -> bool {
    ["CLAW_DISABLE_PROMPT_EXTRAS_CACHE", "CLAW_DISABLE_CACHE"]
        .iter()
        .any(|key| std::env::var(key).map(|v| v == "1").unwrap_or(false))
}

fn extras_cache_path(cwd: &Path, name: &str) -> std::path::PathBuf {
    cwd.join(".claw")
        .join("cache")
        .join("prompt_extras")
        .join(format!("{name}.json"))
}

fn extras_cache_get(cwd: &Path, name: &str, fingerprint: u64) -> Option<String> {
    if extras_cache_disabled() {
        return None;
    }
    let raw = std::fs::read_to_string(extras_cache_path(cwd, name)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if value.get("schema")?.as_u64()? != EXTRAS_CACHE_SCHEMA {
        return None;
    }
    if value.get("workspace")?.as_str()? != cwd.to_string_lossy() {
        return None;
    }
    if value.get("fingerprint")?.as_u64()? != fingerprint {
        return None;
    }
    value.get("payload")?.as_str().map(ToString::to_string)
}

fn extras_cache_put(cwd: &Path, name: &str, fingerprint: u64, payload: &str) {
    if extras_cache_disabled() {
        return;
    }
    let path = extras_cache_path(cwd, name);
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let body = serde_json::json!({
        "schema": EXTRAS_CACHE_SCHEMA,
        "workspace": cwd.to_string_lossy(),
        "fingerprint": fingerprint,
        "payload": payload,
    });
    let Ok(text) = serde_json::to_string(&body) else {
        return;
    };
    // 先写临时文件再 rename,避免并发读者读到半截 JSON。
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, text).is_err() {
        return;
    }
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

struct RepomapCacheProbe {
    fingerprint: u64,
    cached: Option<String>,
    /// LSP 渲染结果依赖运行时进程状态,不能回填源码指纹缓存。
    cacheable: bool,
}

fn probe_repomap_cache(cwd: &Path) -> RepomapCacheProbe {
    let fingerprint = runtime::repomap::source_fingerprint(cwd);
    let cacheable = !tools::global_lsp_registry().is_server_spawned("rust");
    let cached = if cacheable {
        extras_cache_get(cwd, "repomap", fingerprint)
    } else {
        None
    };
    RepomapCacheProbe {
        fingerprint,
        cached,
        cacheable,
    }
}

/// Load optional system prompt extras (persistent memory + repository map).
///
/// Both are best-effort: if the memory file does not exist or the RepoMap
/// fails to render, we silently fall back to no extras rather than blocking
/// startup. The persistent memory is loaded-and-frozen so its snapshot stays
/// byte-stable for the session (preserving the prompt-cache prefix).
///
/// **性能守卫**：当 cwd 是"宽泛目录"（用户主目录、Windows 系统目录、驱动器根等）
/// 时跳过 RepoMap 扫描。RepoMap 会递归扫描整个 cwd 子树为每个源文件计算符号树，
/// 在用户主目录下会遍历 AppData / .cargo / .rustup 等数十万文件，耗时 30+ 秒。
/// persistent_memory 不受影响（只是读一个 JSON 文件）。
pub(crate) fn load_prompt_extras(
    cwd: &Path,
    topology: &Arc<runtime::project_topology::ProjectTopology>,
) -> SystemPromptExtras {
    let t0 = std::time::Instant::now();
    // load_and_freeze 内部已处理文件不存在的情况（返回空记忆），
    // 始终构造 PersistentMemory 以启用 nudge → add_entry → persist 链路，
    // 否则 memory.json 永远不会被创建（chicken-and-egg deadlock）。
    let persistent_memory = {
        let memory_path = cwd.join(".claw").join("memory.json");
        Some(runtime::PersistentMemory::load_and_freeze(&memory_path))
    };
    let t_mem = t0.elapsed();

    // 宽泛目录守卫：用户主目录、Windows 系统目录、驱动器根等。
    // 在这些目录下 RepoMap 扫描会遍历数十万文件，耗时 30+ 秒，
    // 且对用户实际工作毫无价值（用户主目录不是代码仓库）。
    let is_broad_cwd = is_broad_working_directory(cwd);
    let repomap = if is_broad_cwd {
        eprintln!(
            "[load_prompt_extras] skipping RepoMap: cwd is a broad directory ({})",
            cwd.display()
        );
        None
    } else {
        let probe = probe_repomap_cache(cwd);
        if let Some(cached) = probe.cached.filter(|text| !text.trim().is_empty()) {
            // 命中:源码指纹未变,直接复用上次渲染文本(只需一次 mtime 遍历)。
            Some(cached)
        } else {
            let mut map = RepoMap::new(cwd).with_max_tokens(1024);
            let rt = std::time::Instant::now();
            let verbose = std::env::var("CLAW_REPOMAP_VERBOSE")
                .map(|v| v == "1")
                .unwrap_or(false);
            let mark = |label: &str, t: &std::time::Instant| {
                if verbose {
                    eprintln!("[repomap-verbose] {label}={:?}", t.elapsed());
                }
            };
            // P1:先加载磁盘缓存(.claw/repomap_cache.json)。命中后 render 内部
            // 仍会做一次 walkdir mtime 校验遍历(增量重扫 + 新文件发现 + 删除清理),
            // 只在 mtime 变化的文件上重新解析,避免每次新会话全量重建。
            // 逃生阀:CLAW_DISABLE_REPOMAP_CACHE / CLAW_DISABLE_CACHE=1 时跳过。
            let _ = map.load_disk();
            mark("load_disk", &rt);
            // P4:注入拓扑符号索引语义重要性(best-effort 排序增强)。
            // D1:`build_system_prompt` 已在构造期创建共享拓扑并 ensure_built,
            // 此处直接读内存 `state()`,命中 Ready 即用(不再走 load_cache_quick
            // 退化路径);未 Ready(Building/Failed/无缓存)时退化为 regex 排序。
            match topology.state() {
                runtime::project_topology::TopologyState::Ready(data) => {
                    if let Some(si) = &data.symbol_index {
                        map.augment_with_symbol_index(si);
                    }
                }
                _ => {
                    // 内存拓扑未就绪:轻量磁盘兜底(不校验指纹,只影响排序权重)。
                    if let Some(lang) = runtime::topology_cache::pick_primary_language(cwd) {
                        if let Some(data) = runtime::topology_cache::load_cache_quick(cwd, lang) {
                            if let Some(si) = &data.symbol_index {
                                map.augment_with_symbol_index(si);
                            }
                        }
                    }
                }
            }
            mark("semantic_augment", &rt);
            // Step 4.3:若 rust-analyzer 已 spawn,用 render_with_lsp 获得 LSP
            // references 语义重要性(regex 子串匹配在 monorepo 跨模块定位不准)。
            // 仅在已 spawn 时启用,避免 prompt 组装阶段意外触发 slow auto-start。
            let registry = tools::global_lsp_registry();
            let rendered = if registry.is_server_spawned("rust") {
                map.render_with_lsp(registry)
            } else {
                map.render()
            };
            mark("render", &rt);
            // P1:渲染后落盘(best-effort,失败静默;禁用时跳过)。
            map.save_disk();
            mark("save_disk", &rt);
            if rendered.trim().is_empty() {
                None
            } else {
                // 回填缓存:LSP 渲染依赖进程内状态,只在纯源码渲染时记录。
                if probe.cacheable {
                    extras_cache_put(cwd, "repomap", probe.fingerprint, &rendered);
                }
                Some(rendered)
            }
        }
    };
    let t_map = t0.elapsed();

    // Skill catalog: load all active skills (project + user + builtin) and
    // render a compact one-line-per-skill summary. Injected into the dynamic
    // region of the system prompt so the model can discover skills without
    // loading each SKILL.md.
    //
    // Best-effort: on filesystem errors we silently fall back to no catalog
    // rather than blocking startup. The catalog is session-stable — captured
    // once at startup, not refreshed per-turn.
    let skill_catalog = {
        let fingerprint = commands::skill_catalog_fingerprint(cwd);
        let cached = if skill_catalog_enabled(cwd) {
            extras_cache_get(cwd, "skill_catalog", fingerprint)
                .filter(|text| !text.trim().is_empty())
        } else {
            None
        };
        if let Some(cached) = cached {
            Some(cached)
        } else {
            let catalog = load_skill_catalog(cwd);
            if let Some(text) = &catalog {
                extras_cache_put(cwd, "skill_catalog", fingerprint, text);
            }
            catalog
        }
    };
    let t_catalog = t0.elapsed();

    // Project blueprint: owner-maintained `.claw/BLUEPRINT.md` injected into
    // the *static* tier so the model sees mission/rules/priorities before any
    // symbol index. Best-effort: a missing file simply disables injection.
    // Static content ⇒ session-stable ⇒ costs nothing after the first cache
    // write.
    let blueprint = ensure_blueprint(cwd, is_broad_cwd);
    let t_blueprint = t0.elapsed();

    eprintln!(
        "[timing] load_prompt_extras: memory={:?} repomap={:?} catalog={:?} blueprint={:?} broad_cwd={} (cwd={})",
        t_mem,
        t_map,
        t_catalog,
        t_blueprint,
        is_broad_cwd,
        cwd.display()
    );
    SystemPromptExtras {
        persistent_memory,
        repomap,
        blueprint,
        skill_catalog,
        // MCP 目录不在此处填充:MCP runtime 此时尚未连接(build_system_prompt
        // 先于 build_runtime_plugin_state 执行),实时连接状态由
        // append_mcp_catalog 在 runtime 装配阶段注入,避免造出"全部 pending"的假目录。
        mcp_catalog: None,
    }
}

/// Load the project blueprint (`BLUEPRINT.md`) as raw markdown.
///
/// Lookup order:
/// 1. `<cwd>/BLUEPRINT.md` — the tracked location. The blueprint is a
///    project instruction asset like `CLAUDE.md`, so it must live in version
///    control; `.claw/` is gitignored (local runtime state) and would make the
///    mechanism disappear for anyone who clones the repo.
/// 2. `<cwd>/.claw/BLUEPRINT.md` — per-machine override, mirroring the
///    `.claw/settings.local.json` convention for local-only state.
///
/// Returns `None` when neither file exists, is unreadable, or is empty — the
/// prompt builder then simply omits the section (no placeholder noise). The
/// file is owner-maintained, so it is session-stable by construction and
/// therefore safe to inject into the static (prompt-cached) tier.
pub(crate) fn load_blueprint(cwd: &Path) -> Option<String> {
    let candidates = [
        cwd.join("BLUEPRINT.md"),
        cwd.join(".claw").join("BLUEPRINT.md"),
    ];
    candidates
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .find(|content| !content.trim().is_empty())
}

/// 首次启动时补齐蓝图:文件不存在则在项目根创建**空模板**,由 AI 在里程碑
/// 边界填写。
///
/// 两条防呆(缺任一都会到处乱建):
/// 1. `is_broad_cwd` 为真时**不建** —— 主目录/盘根/无项目标识文件的目录不算
///    项目,在那里生成 BLUEPRINT.md 只会污染用户环境;
/// 2. 模板必须是**空槽**(标题 + `<!-- TODO -->`),不得预填结论 —— 预填内容会
///    让新项目的蓝图全是臆造事实,比没有蓝图更糟(模型会当真事实援引)。
///
/// 落盘失败仍返回模板:本会话至少拿到结构指引,不因写权限问题退回"无装配图"。
pub(crate) fn ensure_blueprint(cwd: &Path, is_broad_cwd: bool) -> Option<String> {
    if let Some(existing) = load_blueprint(cwd) {
        return Some(existing);
    }
    if is_broad_cwd {
        return None;
    }
    let skeleton = blueprint_skeleton();
    let _ = std::fs::write(cwd.join("BLUEPRINT.md"), &skeleton);
    Some(skeleton)
}

/// 蓝图空模板:只给结构与填写指引,不给任何结论。
fn blueprint_skeleton() -> String {
    r#"# Project Blueprint

> 本文件由 claw 首次启动时自动创建,当前是**空模板** —— 由 AI 在里程碑边界
> 依据仓库事实填写,不要保留 TODO 占位符。
>
> **维护时机(只有两个,其余时间不要动本文件)**:
> 1. 一个任务收敛完成时;
> 2. 首次探索完一个新项目之后。
>
> 本文件注入 system prompt 的缓存前缀区,随意改动会击穿前缀缓存,故属
> **低频演进资产**。填写原则:只写能从仓库事实(README、各 crate 的
> Cargo.toml 依赖、测试、决策记录)验证的内容,不臆测业务意图。

## 1. 使命

<!-- TODO: 一句话说明本项目为谁解决什么问题。取自 README 首段与主入口代码。 -->

## 2. 能力边界

<!-- TODO: 分「做」与「不做」两张清单;每条「不做」要写明理由,否则会成为盲区。 -->

## 3. 架构分层

<!-- TODO: 从 workspace 各 crate 的真实 Cargo.toml 本地依赖推导,不要凭印象。
     标注依赖方向(只能向下)与每层职责。 -->

## 4. 铁律(不可违反的不变量)

<!-- TODO: 每条都必须有实证出处(决策编号 / 测试名 / 历史事故),否则不是铁律。
     **本节只允许增补,不允许删除或弱化既有条目。** -->

## 5. 当前阶段目标与优先级

<!-- TODO: 按优先级排序;并显式列出「明确欠着」(已知但不做)的事项与理由 ——
     没有这一段,模型的每次上抛询问都缺乏依据。 -->

## 6. 决策权边界

<!-- TODO: 哪些属于自决、哪些必须问用户。默认姿态应为
     「选可逆方案 + 声明假设 + 给出回退路径」,而非默认上抛。 -->
"#
    .to_string()
}

/// Load active skill summaries from all roots (project + user + builtin
/// shipped) and render them as a compact catalog string.
///
/// Returns `None` when:
/// - The skills catalog feature is disabled via settings (`skillsCatalogEnabled: false`)
/// - No skills are found (empty catalog would only waste tokens)
/// - A filesystem error occurs during discovery (best-effort fallback)
fn skill_catalog_enabled(cwd: &Path) -> bool {
    runtime::ConfigLoader::default_for(cwd)
        .load()
        .ok()
        .is_some_and(|config| config.feature_config().skills_catalog_enabled_or_default())
}

pub(crate) fn load_skill_catalog(cwd: &Path) -> Option<String> {
    // Respect the settings toggle. We need to load config first to check.
    // ConfigLoader::default_for is the same loader used by the main prompt
    // builder, so the toggle semantics match exactly.
    if !skill_catalog_enabled(cwd) {
        return None;
    }

    // Load all active skills (shadowed ones are filtered out by
    // `list_skill_summaries`).
    let skills = commands::list_enabled_skill_summaries(cwd).ok()?;
    if skills.is_empty() {
        return None;
    }

    let catalog = commands::render_skill_catalog(&skills);
    if catalog.trim().is_empty() {
        None
    } else {
        Some(catalog)
    }
}

/// 判断 cwd 是否是"宽泛目录"——即不应触发 RepoMap 全量扫描的目录。
///
/// 判定规则：
/// 1. Windows: cwd == %USERPROFILE%（用户主目录，如 `C:\Users\38225`）
/// 2. Windows: cwd == 系统盘根（如 `C:\`、`D:\`）
/// 3. Windows: cwd 在 `C:\Windows` 下
/// 4. Unix: cwd == `/` 或 `~`（home 目录）
/// 5. cwd 没有 `.git` 子目录且没有 `Cargo.toml` / `package.json` / `pyproject.toml`
///    等典型项目标识文件（这是宽泛的"不像项目目录"启发式）
pub(crate) fn is_broad_working_directory(cwd: &Path) -> bool {
    use std::path::PathBuf;

    // 规则 1-4: 与用户主目录、系统根目录比较
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if cwd == home.as_path() {
            return true;
        }
    }
    if let Some(userprofile) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
        if cwd == userprofile.as_path() {
            return true;
        }
    }

    // 规则 2: 驱动器根目录（Windows `C:\` / `D:\`）或 Unix `/`
    let cwd_str = cwd.to_string_lossy();
    if cwd_str.len() <= 3 && cwd_str.ends_with('\\') {
        return true; // 如 `C:\` `D:\`
    }
    if cwd == std::path::Path::new("/") {
        return true;
    }

    // 规则 3: cwd 在 Windows 系统目录下
    if let Ok(windir) = std::env::var("WINDIR") {
        let windir = PathBuf::from(windir);
        if cwd.starts_with(&windir) {
            return true;
        }
    }

    // 规则 5: 没有任何项目标识文件 → 不像代码仓库 → 跳过 RepoMap
    const PROJECT_MARKERS: &[&str] = &[
        ".git",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "setup.py",
        ".svn",
        "Makefile",
    ];
    let has_project_marker = PROJECT_MARKERS
        .iter()
        .any(|marker| cwd.join(marker).exists());
    !has_project_marker
}

// ===== 运行时构建 =====

/// design-gaps #5:从 `GlobalToolRegistry` 生成子 agent 工具签名目录。
///
/// 能力白名单使用**规范名**(read_file/grep_search/...,见
/// `SubagentCapability::allowed_tools()`),与注册表一致,无需映射;
/// `## Available Tools` 层展示的可调用名与 guard / API 工具定义全链路统一。
/// 描述取自注册表(`mvp_tool_specs` 单源真值,避免静态表描述漂移)。
fn subagent_tool_catalog(registry: &GlobalToolRegistry) -> Vec<runtime::ToolSummary> {
    let allowed: BTreeSet<String> = runtime::multi_agent::SubagentCapability::Execute
        .allowed_tools()
        .iter()
        .map(|s| s.to_string())
        .collect();
    registry
        .definitions(Some(&allowed))
        .into_iter()
        .map(|definition| runtime::ToolSummary {
            name: definition.name,
            description: definition.description.unwrap_or_default(),
        })
        .collect()
}

#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_runtime(
    session: Session,
    session_id: &str,
    model: String,
    system_prompt: Vec<String>,
    enable_tools: bool,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    workspace: &Path,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    let runtime_plugin_state = build_runtime_plugin_state_for(workspace)?;
    build_runtime_with_plugin_state(
        session,
        session_id,
        model,
        system_prompt,
        enable_tools,
        allowed_tools,
        permission_mode,
        runtime_plugin_state,
        workspace,
    )
}

/// 构造主/子 agent 的 API client。
///
/// 返回 `Box<dyn runtime::ApiClient>`,`ConversationRuntime` 的泛型 slot
/// 已 box 成 trait object。
fn build_api_client(
    session_id: &str,
    model: &str,
    enable_tools: bool,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
) -> Result<Box<dyn runtime::ApiClient>, Box<dyn std::error::Error>> {
    let mut client = AnthropicRuntimeClient::new(
        session_id,
        model.to_string(),
        enable_tools,
        allowed_tools,
        tool_registry,
    )?;
    // P0-2:上游不返回 prompt cache 时,全量工具 schema 会在每次迭代重复
    // prefill。改为:**默认对主会话开启**,
    // `RUSTY_CLAUDE_PROGRESSIVE_TOOLS=0` 显式关闭。
    if enable_tools
        && std::env::var("RUSTY_CLAUDE_PROGRESSIVE_TOOLS").map_or(true, |value| {
            !(value == "0" || value.eq_ignore_ascii_case("false"))
        })
    {
        client.set_progressive_tools(true);
    }
    Ok(Box::new(client))
}

/// 单模型架构(2026-11):Pro 已下线,辅助子件永远使用主模型(deepseek-flash),
/// 不存在任何升级到 Pro 的路径。
#[must_use]
fn resolve_quality_model(main_model: &str) -> String {
    main_model.to_string()
}

/// Append the MCP server health overview to the system prompt.
///
/// Best-effort: no servers configured, feature toggle disabled, or a poisoned
/// lock silently skip injection — the overview is an optional token-budget
/// enhancement and must never block session startup.
fn append_mcp_catalog(
    system_prompt: &mut Vec<String>,
    mcp_state: &Option<Arc<Mutex<RuntimeMcpState>>>,
    feature_config: &runtime::RuntimeFeatureConfig,
) {
    if !feature_config.mcp_catalog_enabled_or_default() {
        return;
    }
    let Some(mcp_state) = mcp_state.as_ref() else {
        return;
    };
    let Some(catalog) = mcp_state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .mcp_catalog_string()
    else {
        return;
    };
    system_prompt.push(catalog);
}

#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_runtime_with_plugin_state(
    mut session: Session,
    session_id: &str,
    model: String,
    mut system_prompt: Vec<String>,
    enable_tools: bool,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    runtime_plugin_state: RuntimePluginState,
    workspace: &Path,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    // Persist the model in session metadata so resumed sessions can report it.
    if session.model.is_none() {
        session.model = Some(model.clone());
    }
    // 根据模型 context window 提前获取 compaction 阈值,
    // 在 model 被 move 到 AnthropicRuntimeClient 之前完成。
    let context_window = model_token_limit(&model).map(|limit| limit.context_window_tokens);
    // 从 session 提取工作区根白名单（主 cwd 根 + `--add-dir` 额外根），
    // 注入到 tool_registry，让 `classify_*_permission_with_roots` 在工具执行路径生效。
    let workspace_roots = session.workspace_roots();
    let RuntimePluginState {
        feature_config,
        tool_registry,
        plugin_registry,
        mcp_state,
    } = runtime_plugin_state;
    // 显式 turn 环境：主工作区根 + `--add-dir` 额外根；子智能体与 DAG 执行器共享同一份。
    let turn_env = tools::TurnEnv::new(workspace.to_path_buf(), workspace_roots);
    let tool_registry = tool_registry.with_turn_env(turn_env.clone());
    plugin_registry.initialize()?;
    // MCP 服务器健康总览注入:runtime registry 已在 RuntimeMcpState::new 中完成
    // best-effort 连接,把每个服务器的实时状态(connected / error / unsupported)
    // 渲染成一节动态 prompt,追加到 system prompt 末尾(动态区,保持静态前缀
    // prompt-cache 稳定)。模型据此知道哪些外部服务器可用,无需盲目探测。
    // 由配置开关 mcpCatalogEnabled 控制,默认启用。
    append_mcp_catalog(&mut system_prompt, &mcp_state, &feature_config);
    let policy = permission_policy(permission_mode, &feature_config, &tool_registry)
        .map_err(std::io::Error::other)?;
    // Phase 4：提前创建 ProjectTopology 实例，供 CliToolExecutor 和 ConversationRuntime 共享。
    // D1:改用线程局部注册表按规范化 cwd 复用 build_system_prompt 已构造的实例
    // （同一 cwd 下不重复建，多个 build_runtime 调用点共享同一拓扑）。
    let shared_topology = Some(topology_for(workspace));
    // v0.2 生产接入:在 model 被 move 进 AnthropicRuntimeClient 之前先 clone 一份,
    // 稍后用于构造独立的 subagent api_client(DAG dispatch 用)。
    // AnthropicRuntimeClient 不是 Clone(内含 tokio::runtime::Runtime),只能重建。
    let model_for_subagent = model.clone();
    // P0:为质量敏感的辅助 LLM 子件解析旗舰模型。
    let quality_model = resolve_quality_model(&model_for_subagent);
    let api_client = build_api_client(
        session_id,
        &model,
        enable_tools,
        allowed_tools.clone(),
        tool_registry.clone(),
    )?;
    let mut runtime = ConversationRuntime::new_with_features(
        session,
        api_client,
        CliToolExecutor::new(
            allowed_tools.clone(),
            tool_registry.clone(),
            mcp_state.clone(),
        )
        .with_project_topology(shared_topology.clone()),
        policy,
        system_prompt,
        &feature_config,
    );
    // design-gaps #5:注入子 agent 工具签名目录。
    // 从 GlobalToolRegistry 生成(描述与 mvp_tool_specs 单源一致),
    // build_subagent_context 按 capability 白名单过滤后注入 Available Tools 层。
    runtime = runtime.with_tool_catalog(subagent_tool_catalog(&tool_registry));
    // design-gaps #1:启用 hooks 配置热重载 — 每 turn 检查配置源
    // (settings.json / .claw.json 等)的 mtime,变化则原子替换 hooks 配置,
    // 无需重启会话。加载失败(如 cwd 不可用)时静默保持默认行为。
    runtime = runtime.with_hooks_hot_reload(ConfigLoader::default_for(workspace.to_path_buf()));
    // P1-6 修复：注入 harness V(验证)层和 O(可观测性)层组件。
    let mut verifier_agent = runtime::VerifierAgent::new();
    match crate::llm_clients::DeepSeekJudgeClient::new(&quality_model, Some(1024)) {
        Ok(judge) => {
            verifier_agent = verifier_agent.with_model_judge(std::sync::Arc::new(judge));
        }
        Err(e) => {
            eprintln!("[startup] VerifierAgent ModelJudge skipped (construction failed): {e}");
        }
    }
    runtime = runtime
        .with_verifier_agent(verifier_agent)
        .with_trace_analyzer(runtime::TraceAnalyzer::new());
    // design-gaps #2:注入 self-evolving harness archive。
    // 打开失败只静默跳过(archive 是可选增强,不应阻塞启动)。
    runtime = runtime.with_harness_evolution(workspace.to_path_buf());
    // P0:注入 LLM harness Proposer(Phase 3)。
    // 构造失败(无 API key)静默跳过,回退到纯规则式路径(零 LLM 调用)。
    match crate::llm_clients::DeepSeekHarnessProposer::new(&quality_model, Some(512)) {
        Ok(proposer) => {
            runtime::harness_evolution::set_global_harness_proposer(std::sync::Arc::new(proposer));
        }
        Err(e) => {
            eprintln!("[startup] HarnessProposer skipped (construction failed): {e}");
        }
    }
    // Epic 2:注入 MultiAgentCoordinator,启用 subagent-as-tool 路由。
    let coordinator = runtime::MultiAgentCoordinator::new();

    // v2 Phase 2 Epic 4:启动恢复 — 扫描 .claw/checkpoints/*.json,
    // 把崩溃前未完成的 subagent 状态恢复到 registry,让 retry loop 接管。
    //
    // 语义边界:只恢复 subagent 注册表 + 元状态,不恢复 LLM 对话历史
    // (与 LangGraph/Temporal durable execution 一致)。
    // Running 状态会自动降级为 Created,允许重新 start() 调度。
    //
    // 失败处理:单个 checkpoint 恢复失败只 log,不中止整个启动
    // (避免一个损坏文件阻塞整个 CLI)。
    {
        let ckpt_dir = workspace.join(".claw").join("checkpoints");
        if let Ok(entries) = std::fs::read_dir(&ckpt_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("json") {
                    match coordinator.restore_from_checkpoint(&path) {
                        Ok(id) => {
                            eprintln!("[restore] subagent {id} restored from {}", path.display())
                        }
                        Err(e) => eprintln!("[restore] failed to restore {}: {e}", path.display()),
                    }
                }
            }
        }
    }

    // v2 §10.5 多 ValidationGate:注册 rust/npm/pytest gate。
    // 命令不存在时跳过注册(避免 retryable=false 中止 validation 链);
    // 无 workspace_root 时全部跳过(CommandValidationGate 需要 cwd)。
    {
        let workspace_root = workspace.to_path_buf();
        if crate::command_exists("cargo") {
            coordinator.add_validation_gate(Box::new(
                runtime::multi_agent::validation::rust_compile_gate(workspace_root.clone()),
            ));
        }
        if crate::command_exists("npm") {
            coordinator.add_validation_gate(Box::new(
                runtime::multi_agent::validation::npm_build_gate(workspace_root.clone()),
            ));
        }
        if crate::command_exists("python") {
            coordinator.add_validation_gate(Box::new(
                runtime::multi_agent::validation::pytest_gate(workspace_root.clone()),
            ));
        }

        // v2 Phase 2 Epic 5:注册 LlmJudgeGate(诊断/架构任务的 LLM-as-judge 评分)。
        // 构造失败(无 API key / 模型名无效)时跳过注册,不阻断启动。
        match crate::llm_clients::DeepSeekJudgeClient::new(&quality_model, Some(1024)) {
            Ok(judge_client) => {
                let judge: std::sync::Arc<dyn runtime::multi_agent::validation::JudgeClient> =
                    std::sync::Arc::new(judge_client);
                coordinator.add_validation_gate(Box::new(
                    runtime::multi_agent::validation::LlmJudgeGate::diagnostic_default(
                        &quality_model,
                        workspace_root.clone(),
                    )
                    .with_client(judge),
                ));
            }
            Err(e) => {
                eprintln!("[startup] LlmJudgeGate skipped (judge client construction failed): {e}");
            }
        }
    }

    // v0.2 生产接入:保留一份 coordinator 的 Arc,用于构造 CoordinatorExecutor。
    let coordinator_arc = std::sync::Arc::new(coordinator.clone());
    runtime = runtime.with_multi_agent_coordinator(coordinator);

    // v2 Phase 2 Epic 6:注入全局 DecisionExtractorClient。
    // 构造失败(无 API key / 模型名无效)时跳过,不阻断启动。
    match crate::llm_clients::DeepSeekDecisionExtractorClient::new(&quality_model, Some(2048)) {
        Ok(extractor) => {
            let extractor_client: std::sync::Arc<
                dyn runtime::decision_log::DecisionExtractorClient,
            > = std::sync::Arc::new(extractor);
            runtime::decision_log::set_global_decision_extractor_client(extractor_client);
            runtime = runtime.with_detection_strategy(
                runtime::decision_log::DetectionStrategy::LlmExtract {
                    model: quality_model.clone(),
                },
            );
        }
        Err(e) => {
            eprintln!("[startup] DecisionExtractorClient skipped (construction failed): {e}");
        }
    }

    // 知识新鲜度门控(Phase 1):注入全局 ResearchClient。
    // 构造失败(无 API key)时跳过,降级为不调研(不阻塞任务)。
    match crate::llm_clients::WebResearchClient::new(&quality_model, Some(2048)) {
        Ok(researcher) => {
            let research_client: std::sync::Arc<dyn runtime::knowledge_freshness::ResearchClient> =
                std::sync::Arc::new(researcher);
            runtime::knowledge_freshness::set_global_research_client(research_client);
        }
        Err(e) => {
            eprintln!("[startup] ResearchClient skipped (construction failed): {e}");
        }
    }

    // 知识新鲜度门控(Phase 2.1):注入全局 FreshnessAssessor。
    // 构造失败(无 API key)时跳过,降级为纯关键词评估(不阻塞任务)。
    match crate::llm_clients::DeepSeekFreshnessAssessor::new(&model_for_subagent, Some(256)) {
        Ok(assessor) => {
            let assessor_client: std::sync::Arc<
                dyn runtime::knowledge_freshness::FreshnessAssessor,
            > = std::sync::Arc::new(assessor);
            runtime::knowledge_freshness::set_global_freshness_assessor(assessor_client);
        }
        Err(e) => {
            eprintln!("[startup] FreshnessAssessor skipped (construction failed): {e}");
        }
    }

    // 知识新鲜度门控(Phase 2.3):注入全局 QueryBuilderClient。
    // 构造失败(无 API key)时跳过,降级为启发式查询(不阻塞任务)。
    match crate::llm_clients::DeepSeekQueryBuilderClient::new(&model_for_subagent, Some(128)) {
        Ok(builder) => {
            let builder_client: std::sync::Arc<
                dyn runtime::knowledge_freshness::QueryBuilderClient,
            > = std::sync::Arc::new(builder);
            runtime::knowledge_freshness::set_global_query_builder(builder_client);
        }
        Err(e) => {
            eprintln!("[startup] QueryBuilderClient skipped (construction failed): {e}");
        }
    }

    // v1.0 LLM context editing:注入全局 CompactionSummarizerClient。
    // 构造失败(无 API key)时跳过,启动不失败。
    match crate::llm_clients::DeepSeekCompactionSummarizerClient::new(
        &model_for_subagent,
        Some(2048),
    ) {
        Ok(summarizer) => {
            let summarizer_client: std::sync::Arc<
                dyn runtime::compact::CompactionSummarizerClient,
            > = std::sync::Arc::new(summarizer);
            runtime::compact::set_global_compaction_summarizer_client(summarizer_client);
            eprintln!(
                "[startup] LLM compaction summarizer registered (model: {model_for_subagent})"
            );
        }
        Err(e) => {
            eprintln!("[startup] CompactionSummarizerClient skipped (construction failed): {e}");
        }
    }

    // D1.0 LLM-driven planning:注入全局 PlanGeneratorClient。
    // 构造失败跳过,不阻断启动。
    match crate::llm_clients::DeepSeekPlanGeneratorClient::new(&quality_model, Some(2048)) {
        Ok(planner_client) => {
            let planner_client: std::sync::Arc<dyn runtime::planner::PlanGeneratorClient> =
                std::sync::Arc::new(planner_client);
            runtime::planner::set_global_plan_generator_client(planner_client);
            eprintln!("[startup] LLM plan generator registered (model: {quality_model})");
        }
        Err(e) => {
            eprintln!("[startup] PlanGeneratorClient skipped (construction failed): {e}");
        }
    }

    // Phase 1 记忆闭环：注册记忆提取器（flash + 512 tokens，纯增量成本）。
    // 未注册时 worker 直接跳过提取（静默），不影响主流程。
    match crate::llm_clients::DeepSeekMemoryExtractorClient::new("deepseek-flash", Some(512)) {
        Ok(extractor) => {
            let extractor: std::sync::Arc<dyn runtime::memory_extract::MemoryExtractorClient> =
                std::sync::Arc::new(extractor);
            runtime::memory_extract::set_global_memory_extractor_client(extractor);
            eprintln!("[startup] LLM memory extractor registered (model: deepseek-flash)");
        }
        Err(e) => {
            eprintln!("[startup] MemoryExtractorClient skipped (construction failed): {e}");
        }
    }

    // v0.2 生产接入:构造 CoordinatorExecutor 并装入 runtime,让 DAG 调度能真正
    // 执行子 agent turn(替代 v0.1 stub 路径)。
    //
    // 因 AnthropicRuntimeClient 不是 Clone(内含 tokio::runtime::Runtime),
    // 这里用之前 clone 的 model_for_subagent 重建一份独立的 client 给 subagent
    // dispatcher。subagent 走独立 LLM 请求 + 独立 prompt cache(§5.2 缓存保护),
    // enable_tools=false(subagent system prompt 明确不需要工具)。
    let workspace_root = workspace.to_path_buf();
    if workspace_root.is_dir() {
        let subagent_api_client = build_api_client(
            session_id,
            &model_for_subagent,
            false, // enable_tools
            None,  // allowed_tools
            tool_registry.clone(),
        )?;
        // DAG 并行派发子代理的 tool executor:执行层白名单覆盖所有能力工具
        // (以 Execute 白名单为超集),实际能力分级过滤由 SubagentCapability
        // 白名单在 process_tool_uses(Guard 2)完成。
        let dag_tool_executor = tools::make_subagent_tool_executor(
            runtime::multi_agent::SubagentCapability::Execute
                .allowed_tools()
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            turn_env.clone(),
        );
        runtime = runtime.with_dag_coordinator(
            coordinator_arc,
            subagent_api_client,
            workspace_root,
            Some(dag_tool_executor),
            None, // workspace_override — 路径 B 未绑定子目录(主 root,向后兼容);Some 时收窄工具作用域
            model_for_subagent.clone(),
        );
    }

    // v0.2 生产接入:把 CoordinatorExecutor 注入 tools 层全局 registry,
    // 让 dag_run 工具的 "start" 分支能取出它构造 DagScheduler。
    if let Some(executor) = runtime.coordinator_executor() {
        tools::set_coordinator_executor(executor.clone());
    }
    // YAML 声明式 DAG 预注册:启动时扫描 <workspace>/.claw/dags/*.yaml,
    // 注入全局 DagStore。与 dag_define 工具共用同一套校验/默认值逻辑,
    // 让模型无需工具调用即可对预定义依赖图执行 dag_run。
    if workspace.is_dir() {
        let dags_dir = workspace.join(".claw").join("dags");
        let (dags, errors) =
            runtime::multi_agent::dag::yaml_loader::load_dag_definitions_with_errors(&dags_dir);
        for err in &errors {
            eprintln!("[dag-loader] {err}");
        }
        for dag in dags {
            if let Err(e) = tools::register_dag(dag) {
                eprintln!("[dag-loader] register failed: {e}");
            }
        }
    }
    // 根据模型 context window 动态设置 compaction 阈值。
    // 1M 模型(DeepSeek V4/GPT-5.4)阈值 650K,200K 模型(Claude)阈值 130K,
    // 避免 100K 一刀切对长上下文模型过度激进压缩。
    if let Some(cw) = context_window {
        runtime = runtime.with_context_window(cw);
    }
    // Attach persistent memory for nudge curation. Loaded-and-frozen so the
    // nudge layer can write new entries to disk while the prompt's frozen
    // snapshot (loaded separately in load_prompt_extras) stays byte-stable
    // for the session — preserving the prompt-cache prefix.
    let cwd = workspace.to_path_buf();
    {
        let memory_path = cwd.join(".claw").join("memory.json");
        // load_and_freeze 内部已处理文件不存在的情况（返回空记忆），
        // 始终注入 PersistentMemory 以启用 nudge → add_entry → persist 链路，
        // 否则 memory.json 永远不会被创建（chicken-and-egg deadlock）。
        let memory = runtime::PersistentMemory::load_and_freeze(&memory_path);
        runtime = runtime.with_persistent_memory(memory);

        // Phase 4 认知外骨骼：注入 DecisionLog / ProjectTopology / RefactorTransaction。
        //
        // DecisionLog：SQLite 决策库，失败时降级为 None（不阻断启动）。
        if let Ok(decision_log) = runtime::DecisionLog::open(&cwd) {
            runtime = runtime.with_decision_log(decision_log);
        }
        // ProjectTopology：复用提前创建的 shared_topology（与 CliToolExecutor 共享同一个 Arc）。
        if let Some(topo) = shared_topology.clone() {
            runtime = runtime.with_project_topology(topo);
        }
        // RefactorTransaction：非 git 仓库自动进入 Disabled 状态，安全无副作用。
        let tx = runtime::RefactorTransaction::new(cwd.clone());
        runtime = runtime.with_refactor_transaction(tx);

        // P2(2026-08-14):注入 runtime 层 workspace_root。
        // with_workspace_root 同时同步注入 multi_agent_coordinator 的 workspace_root。
        runtime = runtime.with_workspace_root(cwd.clone());
    }
    // P1(2026-08-13):Telemetry SessionTracer 接入生产路径。
    // 用 JsonlTelemetrySink 写入 <cwd>/.claw/telemetry.jsonl。
    // 失败(无 cwd / 目录不可写)时静默跳过,不阻塞启动。
    {
        let telemetry_path = workspace.join(".claw").join("telemetry.jsonl");
        match JsonlTelemetrySink::new(&telemetry_path) {
            Ok(sink) => {
                let tracer = SessionTracer::new(session_id, Arc::new(sink));
                runtime = runtime.with_session_tracer(tracer);
            }
            Err(e) => {
                eprintln!(
                    "[startup] SessionTracer skipped (telemetry sink construction failed): {e}"
                );
            }
        }
    }
    Ok(BuiltRuntime::new(runtime, plugin_registry, mcp_state))
}

// ===== LiveCli 执行内核 =====

pub(crate) struct LiveCli {
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    system_prompt: Vec<String>,
    runtime: BuiltRuntime,
    session: SessionHandle,
    /// 本 CLI 绑定的工作区根；所有需要 cwd 的内部路径都从这里派生，
    /// 不读取进程级当前目录（Web 多工作区下进程 cwd 是中性临时目录）。
    workspace: PathBuf,
    // 流式状态事件出口：Web 在 serve.rs 中挂载，驱动 SSE 文本/工具事件。
    status_emitter: Option<runtime::StatusEmitter>,
    /// 热升级优雅中断信号。
    ///
    /// `/upgrade` 命令置位后,当前正在执行的 turn 在下一次 run_turn 迭代
    /// 顶部检测到 → 注入"暂停通知"给 LLM → turn 自然收尾。因
    /// `prepare_turn_runtime` 每轮重建 runtime,信号必须存于此字段并在
    /// 重建时经 `with_upgrade_signal` 注入,否则每轮新 runtime 的
    /// upgrade_pending 都是默认 false,信号丢失。
    upgrade_signal: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 主会话 mid-turn steering 收件箱（inject 模式）。
    ///
    /// 与 `upgrade_signal` 同构：turn 执行期间把用户输入投入此队列，
    /// `run_turn` 每次迭代顶部取出并注入为 user 消息。因
    /// `prepare_turn_runtime` 每轮重建 runtime,队列必须存于此字段并在
    /// 重建时经 `with_steer_inbox` 注入,否则每轮新 runtime 的队列都是空的。
    steer_inbox: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// 外部注入的 abort signal。
    /// Web 层在启动本轮执行前设置（保留 clone 用于「停止」），
    /// prepare_turn_runtime 优先使用此 signal（而非创建新的），
    /// 让调用方能在 turn 执行期间取消当前轮。
    external_abort_signal: Option<runtime::HookAbortSignal>,
    /// S3-M2:会话级子进程登记表（Web worker 每会话一份，跨轮复用）。
    /// prepare_turn_runtime 注入到本轮工具注册表，worker 退出 / 看门狗回收 /
    /// 会话删除时由调度器统一终止登记的进程树。
    session_processes: Option<std::sync::Arc<runtime::process_registry::SessionProcesses>>,
}

pub(crate) struct BuiltRuntime {
    /// `Box<dyn runtime::ApiClient>`:允许在默认 API 后端运行时不改动
    /// `ConversationRuntime` 泛型约束。
    runtime: Option<ConversationRuntime<Box<dyn runtime::ApiClient>, CliToolExecutor>>,
    plugin_registry: PluginRegistry,
    plugins_active: bool,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    mcp_active: bool,
}

impl BuiltRuntime {
    /// 会话边界：通知记忆 worker 收尾（0 等待，失败静默）。
    pub(crate) fn on_memory_boundary(
        &mut self,
        boundary: runtime::memory_extract::MemorySessionBoundary,
    ) {
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.on_memory_boundary(boundary);
        }
    }

    fn new(
        runtime: ConversationRuntime<Box<dyn runtime::ApiClient>, CliToolExecutor>,
        plugin_registry: PluginRegistry,
        mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    ) -> Self {
        Self {
            runtime: Some(runtime),
            plugin_registry,
            plugins_active: true,
            mcp_state,
            mcp_active: true,
        }
    }

    fn with_hook_abort_signal(mut self, hook_abort_signal: runtime::HookAbortSignal) -> Self {
        // 同一份令牌同时驱动 agent loop 检查点与 bash 轮询循环：
        // 「停止」不再依赖进程级全局标志，且按 turn 隔离（并发会话互不影响）。
        let token = hook_abort_signal.token();
        let mut runtime = self
            .runtime
            .take()
            .expect("runtime should exist before installing hook abort signal")
            .with_hook_abort_signal(hook_abort_signal);
        runtime.tool_executor_mut().set_turn_cancel(token);
        self.runtime = Some(runtime);
        self
    }

    /// S3-M2:注入会话级子进程登记表（Web worker 跨轮复用的同一份）。
    fn with_session_processes(
        mut self,
        processes: Option<std::sync::Arc<runtime::process_registry::SessionProcesses>>,
    ) -> Self {
        if let Some(processes) = processes {
            if let Some(runtime) = self.runtime.as_mut() {
                runtime.tool_executor_mut().set_session_processes(processes);
            }
        }
        self
    }

    /// 注入热升级优雅中断信号。
    ///
    /// 与 [`with_hook_abort_signal`](Self::with_hook_abort_signal) 同构:
    /// 把 LiveCli 持有的 `upgrade_signal`(跨 `prepare_turn_runtime` 重建
    /// 持久)注入 runtime,使 `/upgrade` 命令置位后,当前正在执行的 turn
    /// 在 run_turn 循环顶部检测到并优雅收尾。
    fn with_upgrade_signal(
        mut self,
        upgrade_signal: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        let runtime = self
            .runtime
            .take()
            .expect("runtime should exist before installing upgrade signal");
        self.runtime = Some(runtime.with_upgrade_signal(upgrade_signal));
        self
    }

    /// 注入主会话 mid-turn steering 收件箱（inject 模式）。
    fn with_steer_inbox(
        mut self,
        steer_inbox: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Self {
        let runtime = self
            .runtime
            .take()
            .expect("runtime should exist before installing steer inbox");
        self.runtime = Some(runtime.with_steer_inbox(steer_inbox));
        self
    }

    pub(crate) fn shutdown_plugins(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.plugins_active {
            self.plugin_registry.shutdown()?;
            self.plugins_active = false;
        }
        Ok(())
    }

    fn shutdown_mcp(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.mcp_active {
            if let Some(mcp_state) = &self.mcp_state {
                mcp_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .shutdown()?;
            }
            self.mcp_active = false;
        }
        Ok(())
    }
}

impl Deref for BuiltRuntime {
    type Target = ConversationRuntime<Box<dyn runtime::ApiClient>, CliToolExecutor>;

    fn deref(&self) -> &Self::Target {
        self.runtime
            .as_ref()
            .expect("runtime should exist while built runtime is alive")
    }
}

impl DerefMut for BuiltRuntime {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.runtime
            .as_mut()
            .expect("runtime should exist while built runtime is alive")
    }
}

impl Drop for BuiltRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown_mcp();
        let _ = self.shutdown_plugins();
    }
}

impl Drop for LiveCli {
    fn drop(&mut self) {
        // 会话/进程收尾（serve worker 回收）：
        // 通知记忆 worker 做边界巩固（0 等待，失败静默）。
        self.runtime
            .on_memory_boundary(runtime::memory_extract::MemorySessionBoundary::Exited);
    }
}

impl LiveCli {
    pub(crate) fn new(
        model: String,
        enable_tools: bool,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        additional_workspace_roots: Vec<PathBuf>,
        workspace: &Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        crate::diag_log(&format!("[LiveCli::new] start, model={model}"));
        let t0 = std::time::Instant::now();
        crate::diag_log("[LiveCli::new] calling build_system_prompt");
        let system_prompt = build_system_prompt(&model, workspace)?;
        crate::diag_log("[LiveCli::new] build_system_prompt OK");
        let t_sp = t0.elapsed();
        let session_state = new_cli_session_with_roots_in(workspace, additional_workspace_roots)?;
        crate::diag_log("[LiveCli::new] new_cli_session_with_roots OK");
        let t_sess = t0.elapsed();
        let session = create_managed_session_handle_in(workspace, &session_state.session_id)?;
        crate::diag_log("[LiveCli::new] create_managed_session_handle OK");
        let t_handle = t0.elapsed();
        crate::diag_log("[LiveCli::new] calling build_runtime");
        let mut runtime = build_runtime(
            session_state.with_persistence_path(session.path.clone()),
            &session.id,
            model.clone(),
            system_prompt.clone(),
            enable_tools,
            allowed_tools.clone(),
            permission_mode,
            workspace,
        )?;
        let t_rt = t0.elapsed();
        eprintln!(
            "[timing] build_system_prompt={:?} new_cli_session={:?} create_handle={:?} build_runtime={:?} total={:?}",
            t_sp, t_sess, t_handle, t_rt, t_rt
        );
        // Tier S #3 穷鬼模式：从 settings.json 的 `poorMode` 字段读取初始值，
        // 写入进程级全局 AtomicBool。加载失败时保持默认（inactive），不阻断启动。
        {
            let cwd = workspace.to_path_buf();
            // workspace_root 对所有工具路径都需要(notebook_update/recall_full/
            // subagent 等),且与 config 解析正交——config 解析失败时不应导致
            // workspace_root 缺失。因此无条件设置,不嵌套在 loader.load() 分支内。
            runtime.set_workspace_root(cwd.clone());
            let loader = ConfigLoader::default_for(&cwd);
            if let Ok(config) = loader.load() {
                if let Some(poor) = config.feature_config().poor_mode() {
                    runtime::poor_mode::set_active(poor);
                }
                // P3-1:从 settings.json `planMode` 控制 Plan/Execute/Review。
                // 默认启用(true)。
                match config.feature_config().plan_mode() {
                    Some(true) => runtime.set_plan_mode_enabled(true),
                    Some(false) => runtime.set_plan_mode_enabled(false),
                    None => {} // 保持默认(true)
                }
                // SP4.2-B3:从配置初始化 LSP servers(best-effort,失败不阻断启动)
                let _ = init_lsp_from_config(&config, &cwd);
            }
        }
        let cli = Self {
            model,
            allowed_tools,
            permission_mode,
            system_prompt,
            runtime,
            session,
            workspace: workspace.to_path_buf(),
            status_emitter: None,
            upgrade_signal: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            steer_inbox: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            external_abort_signal: None,
            session_processes: None,
        };
        cli.persist_session()?;
        Ok(cli)
    }

    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<String>) {
        if let Some(rt) = self.runtime.runtime.as_mut() {
            rt.api_client_mut().set_reasoning_effort(effort);
        }
    }

    /// 设置 DeepSeek 思考模式开关(Some(true)=开启,Some(false)=关闭,None=模型默认)。
    pub(crate) fn set_thinking_mode(&mut self, thinking: Option<bool>) {
        if let Some(rt) = self.runtime.runtime.as_mut() {
            rt.api_client_mut().set_thinking_mode(thinking);
        }
    }

    /// 读取当前 thinking_mode 设置。
    pub(crate) fn thinking_mode(&self) -> Option<bool> {
        if let Some(rt) = self.runtime.runtime.as_ref() {
            rt.api_client().thinking_mode()
        } else {
            None
        }
    }

    /// 读取当前 reasoning_effort 设置。
    pub(crate) fn reasoning_effort(&self) -> Option<String> {
        if let Some(rt) = self.runtime.runtime.as_ref() {
            rt.api_client().reasoning_effort()
        } else {
            None
        }
    }

    fn prepare_turn_runtime(
        &mut self,
    ) -> Result<
        (BuiltRuntime, HookAbortMonitor, runtime::HookAbortSignal),
        Box<dyn std::error::Error>,
    > {
        // 中断支持：优先使用外部注入的 abort signal
        // （由调用方在 spawn worker / 入队前设置），让 HTTP 层能通过
        // 保留的 clone 取消当前 turn。非中断模式创建新的 signal。
        let hook_abort_signal = self.external_abort_signal.clone().unwrap_or_default();
        let mut runtime = build_runtime(
            self.runtime.session().clone(),
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            &self.workspace,
        )?
        .with_hook_abort_signal(hook_abort_signal.clone())
        .with_session_processes(self.session_processes.clone())
        .with_upgrade_signal(self.upgrade_signal.clone())
        .with_steer_inbox(self.steer_inbox.clone());
        // 推理状态同步：AnthropicRuntimeClient 每轮 turn 都会在
        // prepare_turn_runtime 中重建(见 run_turn → replace_runtime 生命周期),
        // 新 client 上 reasoning_effort / thinking_mode 均为默认值。
        // 从 LiveCli 持有的旧 client 读取并同步到本轮新建的 client。
        {
            let effort = self.reasoning_effort();
            let thinking = self.thinking_mode();
            if let Some(rt) = runtime.runtime.as_mut() {
                let client = rt.api_client_mut();
                client.set_reasoning_effort(effort);
                client.set_thinking_mode(thinking);
            }
        }
        // 把状态事件出口注入本轮新建的 client 与工具执行器：
        // Web SSE 依赖它输出文本与工具事件。
        if let Some(emitter) = &self.status_emitter {
            if let Some(rt) = runtime.runtime.as_mut() {
                rt.api_client_mut().set_status_emitter(Arc::clone(emitter));
                // Also inject into CliToolExecutor so ToolResult events are emitted
                rt.tool_executor_mut()
                    .set_status_emitter(Arc::clone(emitter));
            }
        }
        let hook_abort_monitor = HookAbortMonitor::spawn(hook_abort_signal.clone());

        Ok((runtime, hook_abort_monitor, hook_abort_signal))
    }

    fn replace_runtime(&mut self, runtime: BuiltRuntime) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.shutdown_plugins()?;
        self.runtime = runtime;
        Ok(())
    }

    /// Web（`claw serve`）用：跑一轮但不写终端输出；审批由调用方提供的 prompter 决定，
    /// 流式事件通过 `set_status_emitter` 注入的回调输出。
    pub(crate) fn run_turn_silent_with(
        &mut self,
        input: &str,
        prompter: &mut dyn runtime::PermissionPrompter,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor, _abort_signal) = self.prepare_turn_runtime()?;
        let result = runtime.run_turn(input, Some(prompter));
        hook_abort_monitor.stop();
        let _summary = result?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        Ok(())
    }

    pub(crate) fn persist_session(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.session().save_to_path(&self.session.path)?;
        Ok(())
    }

    /// 当前会话的持久化文件路径（session JSONL）。
    pub(crate) fn session_file_path(&self) -> std::path::PathBuf {
        self.session.path.clone()
    }

    /// Web 写入通道（`claw serve` 子进程）用：从磁盘会话恢复后继续跑。
    pub(crate) fn resume_from_path(
        &mut self,
        session_path: &std::path::Path,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let session_ref = session_path.to_string_lossy().to_string();
        let (handle, mut session) = load_session_reference_in(&self.workspace, &session_ref)?;
        // 上次 turn 若在工具执行中被中断（进程被杀/卡死），会话末尾会留下
        // 没有 tool_result 的 tool_use，API 会以 400 拒绝续聊。恢复时补齐。
        session.repair_dangling_tool_uses();
        let session_id = session.session_id.clone();
        let runtime = build_runtime(
            session,
            &handle.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            &self.workspace,
        )?;
        self.runtime
            .on_memory_boundary(runtime::memory_extract::MemorySessionBoundary::Switched);
        self.replace_runtime(runtime)?;
        self.session = SessionHandle {
            id: session_id,
            path: handle.path,
        };
        Ok(true)
    }

    /// 挂载状态事件出口（Web SSE 文本/工具事件）。
    pub(crate) fn set_status_emitter(&mut self, emitter: runtime::StatusEmitter) {
        self.status_emitter = Some(emitter);
    }

    /// 解除已挂载的状态事件出口。
    pub(crate) fn clear_status_emitter(&mut self) {
        self.status_emitter = None;
    }

    /// 设置外部 abort signal。
    /// 调用方在启动本轮执行前调用此方法，传入 abort signal 的 clone。
    /// prepare_turn_runtime 会优先使用此 signal，让调用方在 turn 执行期间
    /// 取消当前 turn。turn 结束后由 clear_external_abort_signal 清空。
    pub(crate) fn set_external_abort_signal(&mut self, signal: runtime::HookAbortSignal) {
        self.external_abort_signal = Some(signal);
    }

    /// S3-M2:设置会话级进程登记表（Web worker 每会话一份，跨轮复用）。
    pub(crate) fn set_session_processes(
        &mut self,
        processes: std::sync::Arc<runtime::process_registry::SessionProcesses>,
    ) {
        self.session_processes = Some(processes);
    }

    /// 清空外部 abort signal（turn 结束后调用）。
    pub(crate) fn clear_external_abort_signal(&mut self) {
        self.external_abort_signal = None;
    }
}
