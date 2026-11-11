//! 插件市场：Claude `marketplace.json` 归一化、来源管理与安装。
//!
//! 职责边界：
//!
//! - 本模块只负责「索引 → 归一化 → 完整性校验 → 安装请求」；
//!   真正落盘、原子替换、启停状态仍由 [`plugins::PluginManager`] 唯一负责。
//! - 启用/停用/卸载后的可见性统一走 [`plugins::RegisteredPlugin::is_enabled`]，
//!   这里不维护第二份状态。
//! - 公开索引按 Claude 真实格式解析：`source` 可以是仓库内相对路径字符串，
//!   也可以是 `{source:"url"|"git-subdir", url, path?, ref?, sha?}` 对象。
//!   不要求上游改写格式，只在本地做一层归一化。

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use plugins::{
    compute_plugin_tree_sha256, load_plugin_from_directory, ClaudeComponents, PluginError,
    PluginInstallRequest, PluginInstallSource, PluginManager,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub use crate::skill_market::MarketError;

pub const PLUGIN_SOURCES_SCHEMA: &str = "claw.plugin-sources/v1";
const INDEX_CACHE_SCHEMA: &str = "claw.plugin-index-cache/v1";
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
/// `anthropics/skills` 这类真实仓库首次浅克隆约需 40 秒，超时必须留足余量，
/// 否则内置默认源在本机网络上永远拉不下来。
const GIT_TIMEOUT: Duration = Duration::from_secs(120);
/// 索引缓存的新鲜期：期内直接读磁盘，避免每次列表都下载 700KB+。
const INDEX_TTL: Duration = Duration::from_secs(600);

/// 市场根与配置根。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginMarketPaths {
    /// `<config_home>/plugin-market`：来源、索引缓存、git 镜像。
    pub root: PathBuf,
    /// `<config_home>`：插件安装根与 `settings.json` 所在目录。
    pub config_home: PathBuf,
}

impl PluginMarketPaths {
    #[must_use]
    pub fn discover() -> Self {
        let config_home = runtime::default_config_home();
        Self {
            root: config_home.join("plugin-market"),
            config_home,
        }
    }

    #[must_use]
    pub fn with_roots(root: PathBuf, config_home: PathBuf) -> Self {
        Self { root, config_home }
    }

    fn sources_path(&self) -> PathBuf {
        self.root.join("sources.json")
    }

    fn cache_dir(&self) -> PathBuf {
        self.root.join("index-cache")
    }

    fn mirrors_dir(&self) -> PathBuf {
        self.root.join("mirrors")
    }

    fn cache_path(&self, id: &str) -> Option<PathBuf> {
        safe_id(id).then(|| self.cache_dir().join(format!("{id}.json")))
    }

    fn mirror_path(&self, id: &str) -> Option<PathBuf> {
        safe_id(id).then(|| self.mirrors_dir().join(id))
    }
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

/// 插件来源记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginSourceRecord {
    pub id: String,
    /// `http` / `git` / `local`。
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    /// 官方内置源为 `true`；手动添加的来源一律 `false`。
    #[serde(default)]
    pub trusted: bool,
    /// 内置源不可通过接口删除（避免误删默认公开库）。
    #[serde(default)]
    pub builtin: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSourcesRegistry {
    #[serde(default = "sources_schema")]
    pub schema: String,
    #[serde(default)]
    pub sources: Vec<PluginSourceRecord>,
}

fn sources_schema() -> String {
    PLUGIN_SOURCES_SCHEMA.to_string()
}

impl Default for PluginSourcesRegistry {
    fn default() -> Self {
        Self {
            schema: sources_schema(),
            sources: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginSourceView {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
    pub name: Option<String>,
    pub subdir: Option<String>,
    pub trusted: bool,
    pub builtin: bool,
    pub status: String,
    pub plugin_count: usize,
    pub last_error: Option<String>,
    pub checked_at_unix_ms: u64,
    pub cached_at_unix_ms: Option<u64>,
    pub git_ref: Option<String>,
}

/// 内置公开源；首次启动写进 `sources.json`。
#[must_use]
pub fn default_sources() -> Vec<PluginSourceRecord> {
    vec![
        PluginSourceRecord {
            id: "anthropics-plugins-community".to_string(),
            kind: "http".to_string(),
            value: "anthropics/claude-plugins-community".to_string(),
            name: Some("Claude Plugins Community".to_string()),
            subdir: None,
            trusted: true,
            builtin: true,
            status: None,
            last_error: None,
            checked_at_unix_ms: None,
            git_ref: None,
            last_success_at_unix_ms: None,
        },
        PluginSourceRecord {
            id: "anthropics-skills".to_string(),
            kind: "git".to_string(),
            value: "https://github.com/anthropics/skills.git".to_string(),
            name: Some("Anthropic Skills".to_string()),
            subdir: None,
            trusted: true,
            builtin: true,
            status: None,
            last_error: None,
            checked_at_unix_ms: None,
            git_ref: None,
            last_success_at_unix_ms: None,
        },
    ]
}

/// 首次启动写入默认公开源；已有同名 id 时保持用户配置不变。
pub fn ensure_default_sources(paths: &PluginMarketPaths) -> io::Result<()> {
    let mut registry = read_sources(paths);
    let mut changed = false;
    for source in default_sources() {
        if !registry.sources.iter().any(|item| item.id == source.id) {
            registry.sources.push(source);
            changed = true;
        }
    }
    if changed {
        write_sources(paths, &registry)?;
    }
    Ok(())
}

fn read_sources(paths: &PluginMarketPaths) -> PluginSourcesRegistry {
    match fs::read_to_string(paths.sources_path()) {
        Ok(text) if text.trim().is_empty() => PluginSourcesRegistry::default(),
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => PluginSourcesRegistry::default(),
    }
}

fn write_sources(paths: &PluginMarketPaths, registry: &PluginSourcesRegistry) -> io::Result<()> {
    write_json_atomic(&paths.sources_path(), registry)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let text = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    fs::write(&temp, text)?;
    fs::rename(&temp, path)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn next_source_id(sources: &[PluginSourceRecord]) -> String {
    let mut index = sources.len() + 1;
    loop {
        let candidate = format!("plugin-source-{index}");
        if !sources.iter().any(|source| source.id == candidate) {
            return candidate;
        }
        index += 1;
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// 来源管理
// ---------------------------------------------------------------------------

#[must_use]
pub fn list_sources(paths: &PluginMarketPaths) -> Vec<PluginSourceView> {
    read_sources(paths)
        .sources
        .iter()
        .map(|record| source_view(paths, record))
        .collect()
}

fn source_view(paths: &PluginMarketPaths, record: &PluginSourceRecord) -> PluginSourceView {
    let (plugin_count, cached_at) = cached_index_summary(paths, record);
    let status = record.status.clone().unwrap_or_else(|| {
        if cached_at.is_some() || plugin_count > 0 {
            "ok".to_string()
        } else {
            "not_loaded".to_string()
        }
    });
    PluginSourceView {
        id: record.id.clone(),
        kind: record.kind.clone(),
        value: record.value.clone(),
        name: record.name.clone(),
        subdir: record.subdir.clone(),
        trusted: record.trusted,
        builtin: record.builtin,
        status,
        plugin_count,
        last_error: record.last_error.clone(),
        checked_at_unix_ms: record.checked_at_unix_ms.unwrap_or_else(now_unix_ms),
        cached_at_unix_ms: cached_at.or(record.last_success_at_unix_ms),
        git_ref: record.git_ref.clone(),
    }
}

/// 来源列表只读已有缓存/镜像，不发起网络请求。
fn cached_index_summary(
    paths: &PluginMarketPaths,
    record: &PluginSourceRecord,
) -> (usize, Option<u64>) {
    match record.kind.as_str() {
        "http" => {
            let cache = paths
                .cache_path(&record.id)
                .as_deref()
                .and_then(read_index_cache);
            match cache {
                Some(cache) => (
                    parse_marketplace(&cache.body).map_or(0, |market| market.plugins.len()),
                    Some(cache.fetched_at_unix_ms),
                ),
                None => (0, None),
            }
        }
        "git" => {
            let Some(mirror) = paths.mirror_path(&record.id) else {
                return (0, None);
            };
            match load_marketplace_from_dir(&mirror, record.subdir.as_deref()) {
                Ok((_, market)) => (market.plugins.len(), record.last_success_at_unix_ms),
                Err(_) => (0, record.last_success_at_unix_ms),
            }
        }
        "local" => match load_local_marketplace(&record.value, record.subdir.as_deref()) {
            Ok((_, market)) => (market.plugins.len(), record.checked_at_unix_ms),
            Err(_) => (0, None),
        },
        _ => (0, None),
    }
}

pub fn add_source(
    paths: &PluginMarketPaths,
    kind: &str,
    value: &str,
    name: Option<&str>,
    subdir: Option<&str>,
) -> Result<PluginSourceView, MarketError> {
    let kind = kind.trim().to_ascii_lowercase();
    if !matches!(kind.as_str(), "http" | "git" | "local") {
        return Err(MarketError::bad_request(
            "来源类型必须是 http、git 或 local",
        ));
    }
    let value = value.trim();
    if value.is_empty() {
        return Err(MarketError::bad_request("来源地址不能为空"));
    }
    let value = match kind.as_str() {
        "http" => resolve_http_url(value)?.0,
        "git" => normalize_git_url(value)?,
        _ => {
            let canonical = fs::canonicalize(value).map_err(|error| {
                MarketError::bad_request(format!("本地来源路径不可读：{value}（{error}）"))
            })?;
            canonical.display().to_string()
        }
    };
    let mut registry = read_sources(paths);
    let mut record = PluginSourceRecord {
        id: next_source_id(&registry.sources),
        kind,
        value,
        name: non_empty(name),
        subdir: non_empty(subdir),
        // 手动来源一律不信任：安装必须显式确认。
        trusted: false,
        builtin: false,
        status: None,
        last_error: None,
        checked_at_unix_ms: None,
        git_ref: None,
        last_success_at_unix_ms: None,
    };
    // 新增时立即拉一次索引，用户能马上看到可用性；失败也落盘，不静默。
    let _ = refresh_record(paths, &mut record);
    registry.sources.push(record);
    write_sources(paths, &registry)
        .map_err(|error| MarketError::new(500, format!("保存来源失败：{error}")))?;
    let record = registry.sources.last().expect("just pushed");
    Ok(source_view(paths, record))
}

pub fn delete_source(paths: &PluginMarketPaths, id: &str) -> Result<(), MarketError> {
    let mut registry = read_sources(paths);
    let Some(position) = registry.sources.iter().position(|source| source.id == id) else {
        return Err(MarketError::not_found(format!("来源 {id} 不存在")));
    };
    if registry.sources[position].builtin {
        return Err(MarketError::new(403, "内置公开源不能删除"));
    }
    registry.sources.remove(position);
    if let Some(path) = paths.cache_path(id) {
        let _ = fs::remove_file(path);
    }
    if let Some(path) = paths.mirror_path(id) {
        let _ = remove_any(&path);
    }
    write_sources(paths, &registry)
        .map_err(|error| MarketError::new(500, format!("保存来源失败：{error}")))
}

pub fn refresh_source(
    paths: &PluginMarketPaths,
    id: &str,
) -> Result<PluginSourceView, MarketError> {
    let mut registry = read_sources(paths);
    let Some(position) = registry.sources.iter().position(|source| source.id == id) else {
        return Err(MarketError::not_found(format!("来源 {id} 不存在")));
    };
    let mut record = registry.sources[position].clone();
    let result = refresh_record(paths, &mut record);
    registry.sources[position] = record.clone();
    write_sources(paths, &registry)
        .map_err(|error| MarketError::new(500, format!("保存来源失败：{error}")))?;
    result?;
    Ok(source_view(paths, &registry.sources[position]))
}

fn refresh_record(
    paths: &PluginMarketPaths,
    record: &mut PluginSourceRecord,
) -> Result<(), MarketError> {
    let checked_at = now_unix_ms();
    let outcome = match record.kind.as_str() {
        "http" => refresh_http(paths, record),
        "git" => refresh_git(paths, record),
        "local" => load_local_marketplace(&record.value, record.subdir.as_deref()).map(|_| ()),
        other => Err(MarketError::bad_request(format!("未知来源类型：{other}"))),
    };
    record.checked_at_unix_ms = Some(checked_at);
    match outcome {
        Ok(()) => {
            record.status = Some("ok".to_string());
            record.last_error = None;
            record.last_success_at_unix_ms = Some(checked_at);
            Ok(())
        }
        Err(error) => {
            record.status = Some("error".to_string());
            record.last_error = Some(error.message.clone());
            Err(error)
        }
    }
}

fn refresh_http(paths: &PluginMarketPaths, record: &PluginSourceRecord) -> Result<(), MarketError> {
    let url = resolve_http_url(&record.value)?.0;
    let body = fetch_http(&url).map_err(|(status, message)| MarketError::new(status, message))?;
    parse_marketplace(&body)
        .map_err(|error| MarketError::new(502, format!("marketplace.json 解析失败：{error}")))?;
    if let Some(path) = paths.cache_path(&record.id) {
        let snapshot = IndexCache {
            schema: INDEX_CACHE_SCHEMA.to_string(),
            fetched_at_unix_ms: now_unix_ms(),
            url,
            body,
        };
        write_json_atomic(&path, &snapshot)
            .map_err(|error| MarketError::new(500, format!("写入索引缓存失败：{error}")))?;
    }
    Ok(())
}

fn refresh_git(
    paths: &PluginMarketPaths,
    record: &mut PluginSourceRecord,
) -> Result<(), MarketError> {
    fs::create_dir_all(paths.mirrors_dir())
        .map_err(|error| MarketError::new(500, format!("创建镜像目录失败：{error}")))?;
    let temp = paths.mirrors_dir().join(format!(".tmp-{}", Uuid::new_v4()));
    if let Err((status, message)) = clone_with_timeout(&record.value, &temp) {
        let _ = remove_any(&temp);
        return Err(git_error(status, message));
    }
    load_marketplace_from_dir(&temp, record.subdir.as_deref()).inspect_err(|_| {
        let _ = remove_any(&temp);
    })?;
    let git_ref = git_full_ref(&temp);
    let Some(final_dir) = paths.mirror_path(&record.id) else {
        let _ = remove_any(&temp);
        return Err(MarketError::bad_request("来源 id 不合法"));
    };
    if final_dir.exists() {
        let _ = remove_any(&final_dir);
    }
    fs::rename(&temp, &final_dir).map_err(|error| {
        let _ = remove_any(&temp);
        MarketError::new(500, format!("保存仓库镜像失败：{error}"))
    })?;
    record.git_ref = git_ref;
    Ok(())
}

fn git_error(status: &str, message: String) -> MarketError {
    match status {
        "timeout" => MarketError::new(504, message),
        "git_not_found" | "clone_failed" => MarketError::new(502, message),
        _ => MarketError::new(422, message),
    }
}

// ---------------------------------------------------------------------------
// 索引读取
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Default)]
struct RawMarketplace {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    owner: Option<Value>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    plugins: Vec<RawPluginEntry>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawPluginEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    source: Option<Value>,
    #[serde(default)]
    strict: Option<bool>,
    #[serde(default)]
    skills: Value,
    /// `strict:false` 条目的内联组件；Claude 允许不写 plugin.json，直接在索引里声明。
    #[serde(default)]
    agents: Value,
    #[serde(default)]
    commands: Value,
    #[serde(default)]
    hooks: Option<Value>,
    #[serde(rename = "mcpServers", default)]
    mcp_servers: Option<Value>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

/// `strict:false` 条目内联声明的组件路径（相对插件根）。
#[derive(Debug, Clone, Default)]
struct InlineComponents {
    skills: Vec<String>,
    agents: Vec<String>,
    commands: Vec<String>,
    hooks: Option<Value>,
    mcp_servers: Option<Value>,
}

impl InlineComponents {
    fn paths(&self) -> impl Iterator<Item = &String> {
        self.skills
            .iter()
            .chain(self.agents.iter())
            .chain(self.commands.iter())
    }

    fn has_paths(&self) -> bool {
        self.paths().next().is_some()
    }

    fn is_empty(&self) -> bool {
        !self.has_paths() && self.hooks.is_none() && self.mcp_servers.is_none()
    }
}

impl RawPluginEntry {
    fn inline_components(&self) -> InlineComponents {
        InlineComponents {
            skills: skill_list(&self.skills),
            agents: skill_list(&self.agents),
            commands: skill_list(&self.commands),
            hooks: self.hooks.clone(),
            mcp_servers: self.mcp_servers.clone(),
        }
    }

    /// 是否允许按 `strict:false` 语义从索引内联声明合成插件清单。
    fn declares_inline_plugin(&self) -> bool {
        self.strict == Some(false) && !self.inline_components().is_empty()
    }
}

fn parse_marketplace(body: &str) -> Result<RawMarketplace, String> {
    serde_json::from_str(body).map_err(|error| format!("{error}"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexCache {
    #[serde(default = "cache_schema")]
    schema: String,
    fetched_at_unix_ms: u64,
    url: String,
    body: String,
}

fn cache_schema() -> String {
    INDEX_CACHE_SCHEMA.to_string()
}

fn read_index_cache(path: &Path) -> Option<IndexCache> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

struct LoadedIndex {
    marketplace: RawMarketplace,
    /// 仓库内相对路径的解析根；HTTP 来源没有本地根。
    root: Option<PathBuf>,
    live: bool,
    cached: bool,
    cached_at_unix_ms: Option<u64>,
    last_error: Option<String>,
}

fn load_index(
    paths: &PluginMarketPaths,
    record: &PluginSourceRecord,
) -> Result<LoadedIndex, MarketError> {
    match record.kind.as_str() {
        "local" => {
            let (root, marketplace) =
                load_local_marketplace(&record.value, record.subdir.as_deref())?;
            Ok(LoadedIndex {
                marketplace,
                root: Some(root),
                live: true,
                cached: false,
                cached_at_unix_ms: Some(now_unix_ms()),
                last_error: None,
            })
        }
        "http" => load_http_index(paths, record),
        "git" => {
            let Some(mirror) = paths.mirror_path(&record.id) else {
                return Err(MarketError::bad_request("来源 id 不合法"));
            };
            if !mirror.is_dir() {
                return Err(MarketError::new(502, "来源尚未拉取，请先刷新来源后重试"));
            }
            let (root, marketplace) = load_marketplace_from_dir(&mirror, record.subdir.as_deref())?;
            let failed = record
                .status
                .as_deref()
                .is_some_and(|status| status != "ok");
            Ok(LoadedIndex {
                marketplace,
                root: Some(root),
                live: !failed,
                cached: failed,
                cached_at_unix_ms: record.last_success_at_unix_ms,
                last_error: record.last_error.clone(),
            })
        }
        other => Err(MarketError::bad_request(format!("未知来源类型：{other}"))),
    }
}

fn load_http_index(
    paths: &PluginMarketPaths,
    record: &PluginSourceRecord,
) -> Result<LoadedIndex, MarketError> {
    let cache_path = paths.cache_path(&record.id);
    let cache = cache_path.as_deref().and_then(read_index_cache);
    // 新鲜缓存直接复用，避免列表/详情每次都下载整份索引。
    if let Some(cache) = &cache {
        let age = now_unix_ms().saturating_sub(cache.fetched_at_unix_ms);
        if age < INDEX_TTL.as_millis() as u64 {
            if let Ok(marketplace) = parse_marketplace(&cache.body) {
                return Ok(LoadedIndex {
                    marketplace,
                    root: None,
                    live: false,
                    cached: false,
                    cached_at_unix_ms: Some(cache.fetched_at_unix_ms),
                    last_error: None,
                });
            }
        }
    }
    let url = resolve_http_url(&record.value)?.0;
    match fetch_http(&url) {
        Ok(body) => match parse_marketplace(&body) {
            Ok(marketplace) => {
                if let Some(path) = cache_path.as_deref() {
                    let snapshot = IndexCache {
                        schema: cache_schema(),
                        fetched_at_unix_ms: now_unix_ms(),
                        url,
                        body,
                    };
                    let _ = write_json_atomic(path, &snapshot);
                }
                Ok(LoadedIndex {
                    marketplace,
                    root: None,
                    live: true,
                    cached: false,
                    cached_at_unix_ms: Some(now_unix_ms()),
                    last_error: None,
                })
            }
            Err(error) => cached_or_error(
                cache.as_ref(),
                format!("marketplace.json 解析失败：{error}"),
                502,
            ),
        },
        Err((status, message)) => cached_or_error(cache.as_ref(), message, status),
    }
}

fn cached_or_error(
    cache: Option<&IndexCache>,
    error: String,
    status: u16,
) -> Result<LoadedIndex, MarketError> {
    let Some(cache) = cache else {
        return Err(MarketError::new(status, error));
    };
    let Ok(marketplace) = parse_marketplace(&cache.body) else {
        return Err(MarketError::new(status, error));
    };
    Ok(LoadedIndex {
        marketplace,
        root: None,
        live: false,
        cached: true,
        cached_at_unix_ms: Some(cache.fetched_at_unix_ms),
        last_error: Some(error),
    })
}

/// 从目录读 Claude marketplace：优先 `.claude-plugin/marketplace.json`。
fn load_marketplace_from_dir(
    base: &Path,
    subdir: Option<&str>,
) -> Result<(PathBuf, RawMarketplace), MarketError> {
    let dir = match subdir.map(str::trim).filter(|value| !value.is_empty()) {
        Some(subdir) => base.join(subdir),
        None => base.to_path_buf(),
    };
    let candidates = [
        dir.join(".claude-plugin").join("marketplace.json"),
        dir.join("marketplace.json"),
    ];
    let file = candidates
        .iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            MarketError::new(
                422,
                format!(
                    "目录 {} 内没有 .claude-plugin/marketplace.json",
                    dir.display()
                ),
            )
        })?;
    let text = fs::read_to_string(file)
        .map_err(|error| MarketError::new(422, format!("读取索引失败：{error}")))?;
    let marketplace = parse_marketplace(&text)
        .map_err(|error| MarketError::new(422, format!("marketplace.json 解析失败：{error}")))?;
    let root = marketplace_root(file);
    Ok((root, marketplace))
}

fn load_local_marketplace(
    value: &str,
    subdir: Option<&str>,
) -> Result<(PathBuf, RawMarketplace), MarketError> {
    let path = PathBuf::from(value);
    let base = if path.is_file() { path.clone() } else { path };
    if base.is_file() {
        let text = fs::read_to_string(&base)
            .map_err(|error| MarketError::new(422, format!("读取本地索引失败：{error}")))?;
        let marketplace = parse_marketplace(&text).map_err(|error| {
            MarketError::new(422, format!("marketplace.json 解析失败：{error}"))
        })?;
        let root = marketplace_root(&base);
        return Ok((root, marketplace));
    }
    if !base.is_dir() {
        return Err(MarketError::new(
            422,
            format!("本地来源路径不可读：{value}"),
        ));
    }
    load_marketplace_from_dir(&base, subdir)
}

/// 相对路径基准是 marketplace.json 所属仓库根，而不是 `.claude-plugin/`。
fn marketplace_root(file: &Path) -> PathBuf {
    let parent = file.parent().unwrap_or_else(|| Path::new("."));
    if parent
        .file_name()
        .is_some_and(|name| name == ".claude-plugin")
    {
        parent
            .parent()
            .map_or_else(|| parent.to_path_buf(), Path::to_path_buf)
    } else {
        parent.to_path_buf()
    }
}

// ---------------------------------------------------------------------------
// 归一化与校验
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct PluginEntrySourceView {
    /// `url` / `git-subdir` / `path` / `invalid`。
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// 上游声明的固定 commit。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// 仓库内相对路径条目在本地镜像上计算出的内容摘要。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

enum NormalizedSource {
    Remote {
        kind: String,
        url: String,
        path: Option<String>,
        git_ref: Option<String>,
        sha: Option<String>,
    },
    Path {
        path: String,
    },
    Invalid(String),
}

fn normalize_entry_source(source: Option<&Value>) -> NormalizedSource {
    let Some(source) = source else {
        return NormalizedSource::Invalid("条目缺少 source 字段".to_string());
    };
    match source {
        Value::String(value) => normalize_string_source(value),
        Value::Object(map) => normalize_object_source(map),
        _ => NormalizedSource::Invalid("source 必须是字符串或对象".to_string()),
    }
}

fn normalize_string_source(value: &str) -> NormalizedSource {
    let value = value.trim();
    if value.is_empty() {
        return NormalizedSource::Invalid("source 不能为空".to_string());
    }
    if value.starts_with("http://") || value.starts_with("https://") || value.starts_with("git@") {
        return NormalizedSource::Remote {
            kind: "url".to_string(),
            url: value.to_string(),
            path: None,
            git_ref: None,
            sha: None,
        };
    }
    match normalize_relative_path(value) {
        Some(path) => NormalizedSource::Path { path },
        None => NormalizedSource::Invalid(format!("非法的仓库内路径：{value}")),
    }
}

fn normalize_object_source(map: &serde_json::Map<String, Value>) -> NormalizedSource {
    let kind = map
        .get("source")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let url = map
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let path = map
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let git_ref = map
        .get("ref")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let sha = map
        .get("sha")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    match kind {
        "url" => match url {
            Some(url) => NormalizedSource::Remote {
                kind: "url".to_string(),
                url: normalize_remote_url(url),
                path,
                git_ref,
                sha,
            },
            None => NormalizedSource::Invalid("source=url 缺少 url".to_string()),
        },
        "git-subdir" => {
            let Some(url) = url else {
                return NormalizedSource::Invalid("source=git-subdir 缺少 url".to_string());
            };
            let Some(path) = path else {
                return NormalizedSource::Invalid("source=git-subdir 缺少 path".to_string());
            };
            NormalizedSource::Remote {
                kind: "git-subdir".to_string(),
                url: normalize_remote_url(url),
                path: Some(path),
                git_ref,
                sha,
            }
        }
        // Claude 还支持 `{"source":"github","repo":"owner/repo"}`，一并归一化。
        "github" => {
            let repo = map
                .get("repo")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty());
            match repo {
                Some(repo) => NormalizedSource::Remote {
                    kind: "git-subdir".to_string(),
                    url: normalize_remote_url(repo),
                    path,
                    git_ref,
                    sha,
                },
                None => NormalizedSource::Invalid("source=github 缺少 repo".to_string()),
            }
        }
        "" => NormalizedSource::Invalid("source 对象缺少 source 类型".to_string()),
        other => NormalizedSource::Invalid(format!("不支持的 source 类型：{other}")),
    }
}

fn normalize_relative_path(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed == "." || trimmed == "./" {
        return Some(".".to_string());
    }
    let stripped = trimmed.strip_prefix("./").unwrap_or(trimmed);
    if stripped.is_empty() {
        return None;
    }
    let path = Path::new(stripped);
    if path.is_absolute() {
        return None;
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return None;
    }
    Some(stripped.replace('\\', "/"))
}

/// `owner/repo` 简写补成 GitHub git URL；完整 URL 原样保留。
fn normalize_remote_url(value: &str) -> String {
    let value = value.trim();
    if let Some(repo) = github_repo(value) {
        format!("https://github.com/{repo}.git")
    } else {
        value.to_string()
    }
}

fn github_repo(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches('/');
    let value = value.strip_suffix(".git").unwrap_or(value);
    let stripped = value
        .strip_prefix("https://github.com/")
        .or_else(|| value.strip_prefix("http://github.com/"))
        .or_else(|| value.strip_prefix("github.com/"))
        .unwrap_or(value);
    let mut parts = stripped.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return None;
    }
    if owner.contains(':') || owner.contains('@') || repo.contains(':') {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// HTTP 来源：完整 URL 或 `owner/repo[@branch]` 简写。
fn resolve_http_url(value: &str) -> Result<(String, Option<String>), MarketError> {
    let value = value.trim();
    if value.starts_with("http://") || value.starts_with("https://") {
        return Ok((value.to_string(), None));
    }
    let (repo_part, branch) = match value.split_once('@') {
        Some((repo, branch)) if !branch.trim().is_empty() => (repo, branch.trim()),
        _ => (value, "main"),
    };
    let Some(repo) = github_repo(repo_part) else {
        return Err(MarketError::bad_request(
            "HTTP 来源需要完整 URL，或 GitHub 的 owner/repo[@branch] 形式",
        ));
    };
    Ok((
        format!(
            "https://raw.githubusercontent.com/{repo}/{branch}/.claude-plugin/marketplace.json"
        ),
        Some(repo),
    ))
}

fn normalize_git_url(value: &str) -> Result<String, MarketError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(MarketError::bad_request("git 来源地址不能为空"));
    }
    if value.starts_with("file://")
        || value.starts_with("git@")
        || value.starts_with("ssh://")
        || value.starts_with("http://")
        || value.starts_with("https://")
        || Path::new(value).is_absolute()
    {
        return Ok(value.to_string());
    }
    github_repo(value)
        .map(|repo| format!("https://github.com/{repo}.git"))
        .ok_or_else(|| MarketError::bad_request("git 来源需要仓库 URL 或 owner/repo"))
}

fn is_pinned_commit(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn resolve_entry_path(root: &Path, relative: &str) -> Option<PathBuf> {
    let root = fs::canonicalize(root).ok()?;
    let candidate = fs::canonicalize(root.join(relative)).ok()?;
    candidate.starts_with(&root).then_some(candidate)
}

fn has_plugin_manifest(root: &Path) -> bool {
    root.join(".claude-plugin").join("plugin.json").is_file() || root.join("plugin.json").is_file()
}

struct AnalyzedEntry {
    view: PluginEntryView,
}

fn analyze_entry(
    raw: &RawPluginEntry,
    root: Option<&Path>,
    anchor: Option<&str>,
    snapshot: Option<&InstalledSnapshot>,
) -> AnalyzedEntry {
    let name = raw.name.as_deref().unwrap_or("").trim().to_string();
    let description = raw.description.clone().unwrap_or_default();
    let declared_skills = skill_list(&raw.skills);
    let mut view = PluginEntryView {
        name: name.clone(),
        description,
        version: raw.version.clone(),
        homepage: raw.homepage.clone(),
        category: raw.category.clone(),
        strict: raw.strict,
        declared_skills,
        source: PluginEntrySourceView::default(),
        installable: false,
        reason: None,
        reason_detail: None,
        installed: None,
    };
    if name.is_empty() {
        view.reason = Some("invalid_entry".to_string());
        view.reason_detail = Some("条目缺少 name".to_string());
        view.source.kind = "invalid".to_string();
        return AnalyzedEntry { view };
    }
    let analyzed = analyze_source(raw, root, anchor);
    view.source = analyzed.source;
    view.installable = analyzed.installable;
    view.reason = analyzed.reason;
    view.reason_detail = analyzed.detail;
    if let Some(snapshot) = snapshot {
        view.installed = snapshot.match_entry(&name, &view.source);
    }
    AnalyzedEntry { view }
}

struct SourceAnalysis {
    source: PluginEntrySourceView,
    installable: bool,
    reason: Option<String>,
    detail: Option<String>,
}

fn analyze_source(
    raw: &RawPluginEntry,
    root: Option<&Path>,
    anchor: Option<&str>,
) -> SourceAnalysis {
    match normalize_entry_source(raw.source.as_ref()) {
        NormalizedSource::Invalid(detail) => SourceAnalysis {
            source: PluginEntrySourceView {
                kind: "invalid".to_string(),
                ..PluginEntrySourceView::default()
            },
            installable: false,
            reason: Some("invalid_source".to_string()),
            detail: Some(detail),
        },
        NormalizedSource::Path { path } => {
            let mut source = PluginEntrySourceView {
                kind: "path".to_string(),
                path: Some(path.clone()),
                ..PluginEntrySourceView::default()
            };
            let Some(root) = root else {
                return SourceAnalysis {
                    source,
                    installable: false,
                    reason: Some("missing_integrity_anchor".to_string()),
                    detail: Some(
                        "该条目是仓库内相对路径，但当前来源只有远程索引，无法绑定内容摘要；请改用 git 或本地来源，或等待上游提供 sha"
                            .to_string(),
                    ),
                };
            };
            let Some(dir) = resolve_entry_path(root, &path) else {
                return SourceAnalysis {
                    source,
                    installable: false,
                    reason: Some("missing_path".to_string()),
                    detail: Some(format!("仓库内路径 `{path}` 不存在或越出仓库根")),
                };
            };
            if !dir.is_dir() {
                return SourceAnalysis {
                    source,
                    installable: false,
                    reason: Some("missing_path".to_string()),
                    detail: Some(format!("仓库内路径 `{path}` 不是目录")),
                };
            }
            if !has_plugin_manifest(&dir) {
                return analyze_inline_plugin(raw, &path, &dir, source, anchor);
            }
            match compute_plugin_tree_sha256(&dir) {
                Ok(sha256) => {
                    source.sha256 = Some(sha256);
                    SourceAnalysis {
                        source,
                        installable: true,
                        reason: None,
                        detail: None,
                    }
                }
                Err(error) => SourceAnalysis {
                    source,
                    installable: false,
                    reason: Some("unreadable_path".to_string()),
                    detail: Some(format!("计算目录摘要失败：{error}")),
                },
            }
        }
        NormalizedSource::Remote {
            kind,
            url,
            path,
            git_ref,
            sha,
        } => {
            let mut source = PluginEntrySourceView {
                kind,
                url: Some(url),
                path,
                git_ref: git_ref.clone(),
                sha: sha.clone(),
                sha256: None,
            };
            let pinned = sha
                .as_deref()
                .filter(|value| is_pinned_commit(value))
                .map(str::to_string);
            if let Some(pinned) = pinned {
                source.sha = Some(pinned);
                return SourceAnalysis {
                    source,
                    installable: true,
                    reason: None,
                    detail: None,
                };
            }
            if sha.is_some() {
                return SourceAnalysis {
                    source,
                    installable: false,
                    reason: Some("invalid_sha".to_string()),
                    detail: Some("上游 sha 不是 40 位 commit".to_string()),
                };
            }
            if let Some(reference) = git_ref.filter(|value| is_pinned_commit(value)) {
                source.sha = Some(reference);
                return SourceAnalysis {
                    source,
                    installable: true,
                    reason: None,
                    detail: None,
                };
            }
            SourceAnalysis {
                source,
                installable: false,
                reason: Some("missing_integrity_anchor".to_string()),
                detail: Some("条目没有 pinned sha，接口层按不可安装处理".to_string()),
            }
        }
    }
}

/// `strict:false` 且目录没有 plugin.json：按索引内联声明判断可安装性。
///
/// 完整性锚定在来源仓库的固定提交（`git_ref`）上；安装时把声明的组件
/// 复制到暂存目录并生成 `.claude-plugin/plugin.json`，再按 sha256 校验。
fn analyze_inline_plugin(
    raw: &RawPluginEntry,
    path: &str,
    dir: &Path,
    mut source: PluginEntrySourceView,
    anchor: Option<&str>,
) -> SourceAnalysis {
    let Some(anchor) = anchor.filter(|value| is_pinned_commit(value)) else {
        return SourceAnalysis {
            source,
            installable: false,
            reason: Some("missing_plugin_manifest".to_string()),
            detail: Some(format!(
                "插件目录 `{path}` 缺少 .claude-plugin/plugin.json，且来源没有可锚定的提交"
            )),
        };
    };
    if !raw.declares_inline_plugin() {
        return SourceAnalysis {
            source,
            installable: false,
            reason: Some("missing_plugin_manifest".to_string()),
            detail: Some(format!(
                "插件目录 `{path}` 缺少 .claude-plugin/plugin.json，索引条目也没有内联声明组件"
            )),
        };
    }
    let inline = raw.inline_components();
    let mut missing = Vec::new();
    for relative in inline.paths() {
        if resolve_entry_path(dir, relative).is_none() {
            missing.push(relative.clone());
        }
    }
    if !missing.is_empty() {
        return SourceAnalysis {
            source,
            installable: false,
            reason: Some("missing_path".to_string()),
            detail: Some(format!("条目声明的组件路径不存在：{}", missing.join("、"))),
        };
    }
    source.sha = Some(anchor.to_string());
    SourceAnalysis {
        source,
        installable: true,
        reason: None,
        detail: None,
    }
}

fn skill_list(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
        Value::String(item) if !item.trim().is_empty() => vec![item.trim().to_string()],
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// 市场视图
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct PluginEntryView {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub homepage: Option<String>,
    pub category: Option<String>,
    pub strict: Option<bool>,
    pub declared_skills: Vec<String>,
    pub source: PluginEntrySourceView,
    pub installable: bool,
    pub reason: Option<String>,
    pub reason_detail: Option<String>,
    pub installed: Option<InstalledPluginBrief>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstalledPluginBrief {
    pub plugin_id: String,
    pub version: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginMarketSourceBrief {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: Option<String>,
    pub trusted: bool,
    pub status: String,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginMarketIndexView {
    pub source: PluginMarketSourceBrief,
    pub plugins: Vec<PluginEntryView>,
    /// 本次是否真的从来源读取（网络/镜像/本地），而不是复用缓存。
    pub live: bool,
    /// 是否因来源不可用而回退到上次成功的快照。
    pub cached: bool,
    pub cached_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginMarketDetailView {
    pub marketplace: String,
    pub owner: Option<String>,
    pub source: PluginMarketSourceBrief,
    pub plugin: PluginEntryView,
}

fn source_brief(
    record: &PluginSourceRecord,
    loaded: Option<&LoadedIndex>,
) -> PluginMarketSourceBrief {
    let status = loaded.map_or_else(
        || {
            record
                .status
                .clone()
                .unwrap_or_else(|| "not_loaded".to_string())
        },
        |loaded| {
            if loaded.cached {
                "cached".to_string()
            } else {
                record.status.clone().unwrap_or_else(|| "ok".to_string())
            }
        },
    );
    PluginMarketSourceBrief {
        id: record.id.clone(),
        kind: record.kind.clone(),
        name: record.name.clone(),
        trusted: record.trusted,
        status,
        last_error: loaded
            .and_then(|loaded| loaded.last_error.clone())
            .or_else(|| record.last_error.clone()),
    }
}

fn marketplace_name(raw: &RawMarketplace) -> String {
    raw.name
        .clone()
        .unwrap_or_else(|| "(未命名市场)".to_string())
}

fn marketplace_owner(raw: &RawMarketplace) -> Option<String> {
    let owner = raw.owner.as_ref()?;
    if let Some(name) = owner.get("name").and_then(Value::as_str) {
        return Some(name.to_string());
    }
    if let Some(name) = raw
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("owner"))
        .and_then(Value::as_str)
    {
        return Some(name.to_string());
    }
    None
}

pub fn market_index(
    paths: &PluginMarketPaths,
    manager: Option<&PluginManager>,
    source_id: &str,
) -> Result<PluginMarketIndexView, MarketError> {
    let registry = read_sources(paths);
    let Some(record) = registry
        .sources
        .iter()
        .find(|source| source.id == source_id)
    else {
        return Err(MarketError::not_found(format!("来源 {source_id} 不存在")));
    };
    let loaded = load_index(paths, record)?;
    let snapshot = manager.map(InstalledSnapshot::load);
    let plugins = loaded
        .marketplace
        .plugins
        .iter()
        .map(|entry| {
            analyze_entry(
                entry,
                loaded.root.as_deref(),
                record.git_ref.as_deref(),
                snapshot.as_ref(),
            )
            .view
        })
        .collect();
    Ok(PluginMarketIndexView {
        source: source_brief(record, Some(&loaded)),
        plugins,
        live: loaded.live,
        cached: loaded.cached,
        cached_at_unix_ms: loaded.cached_at_unix_ms,
    })
}

pub fn plugin_detail(
    paths: &PluginMarketPaths,
    manager: Option<&PluginManager>,
    source_id: &str,
    name: &str,
) -> Result<PluginMarketDetailView, MarketError> {
    let registry = read_sources(paths);
    let Some(record) = registry
        .sources
        .iter()
        .find(|source| source.id == source_id)
    else {
        return Err(MarketError::not_found(format!("来源 {source_id} 不存在")));
    };
    let loaded = load_index(paths, record)?;
    let snapshot = manager.map(InstalledSnapshot::load);
    let Some(raw) = loaded
        .marketplace
        .plugins
        .iter()
        .find(|entry| entry.name.as_deref().map(str::trim) == Some(name))
    else {
        return Err(MarketError::not_found(format!("插件 {name} 不存在")));
    };
    Ok(PluginMarketDetailView {
        marketplace: marketplace_name(&loaded.marketplace),
        owner: marketplace_owner(&loaded.marketplace),
        source: source_brief(record, Some(&loaded)),
        plugin: analyze_entry(
            raw,
            loaded.root.as_deref(),
            record.git_ref.as_deref(),
            snapshot.as_ref(),
        )
        .view,
    })
}

// ---------------------------------------------------------------------------
// 安装与已安装插件
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct McpServerBrief {
    pub name: String,
    pub transport: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PluginComponentsSummary {
    /// 实际扫描到的技能数（不是目录数）。
    pub skill_count: usize,
    pub skill_paths: Vec<String>,
    pub agent_count: usize,
    pub command_count: usize,
    pub hook_count: usize,
    pub mcp_server_count: usize,
    pub mcp_servers: Vec<McpServerBrief>,
    pub unsupported: Vec<String>,
}

impl PluginComponentsSummary {
    #[must_use]
    pub fn from_components(components: &ClaudeComponents) -> Self {
        Self {
            skill_count: count_skills(&components.skill_paths),
            skill_paths: path_strings(&components.skill_paths),
            agent_count: components.agent_paths.len(),
            command_count: count_commands(&components.command_paths),
            hook_count: components.hooks.len(),
            mcp_server_count: components.mcp_servers.len(),
            mcp_servers: components
                .mcp_servers
                .iter()
                .map(|server| McpServerBrief {
                    name: server.name.clone(),
                    transport: server.transport.clone(),
                })
                .collect(),
            unsupported: components.unsupported.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginInstallReport {
    pub source_id: String,
    pub entry_name: String,
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub path: String,
    pub enabled: bool,
    pub trusted: bool,
    pub components: PluginComponentsSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstalledPluginView {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub kind: String,
    pub source: String,
    pub enabled: bool,
    pub root: Option<String>,
    pub components: PluginComponentsSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginListReport {
    pub plugins: Vec<InstalledPluginView>,
    pub failures: Vec<String>,
}

pub fn install_plugin(
    paths: &PluginMarketPaths,
    manager: &mut PluginManager,
    source_id: &str,
    name: &str,
    confirm_untrusted: bool,
) -> Result<PluginInstallReport, MarketError> {
    let registry = read_sources(paths);
    let Some(record) = registry
        .sources
        .iter()
        .find(|source| source.id == source_id)
    else {
        return Err(MarketError::not_found(format!("来源 {source_id} 不存在")));
    };
    if !record.trusted && !confirm_untrusted {
        return Err(MarketError::new(
            403,
            "来源未受信任；安装前必须显式确认（confirm_untrusted=true）",
        ));
    }
    let loaded = load_index(paths, record)?;
    if loaded.cached {
        return Err(MarketError::new(
            502,
            "来源当前不可用，索引来自缓存；请刷新来源后再安装",
        ));
    }
    let Some(raw) = loaded
        .marketplace
        .plugins
        .iter()
        .find(|entry| entry.name.as_deref().map(str::trim) == Some(name))
    else {
        return Err(MarketError::not_found(format!("插件 {name} 不存在")));
    };
    let analyzed = analyze_entry(raw, loaded.root.as_deref(), record.git_ref.as_deref(), None).view;
    if !analyzed.installable {
        let reason = analyzed.reason.as_deref().unwrap_or("not_installable");
        let detail = analyzed
            .reason_detail
            .as_deref()
            .unwrap_or("条目未通过校验");
        return Err(MarketError::new(
            422,
            format!("插件不可安装（{reason}）：{detail}"),
        ));
    }
    let (request, staging) = build_install_request(paths, raw, &analyzed, loaded.root.as_deref())?;
    let outcome = manager.install_request(request).map_err(map_plugin_error);
    if let Some(directory) = staging {
        let _ = remove_any(&directory);
        if let Some(parent) = directory.parent() {
            // 只清理空的 staging 根，失败无副作用。
            let _ = fs::remove_dir(parent);
        }
    }
    let outcome = outcome?;
    let manifest = load_plugin_from_directory(&outcome.install_path).map_err(map_plugin_error)?;
    let components = PluginComponentsSummary::from_components(&manifest.components);
    let enabled = manager
        .plugin_registry_report()
        .ok()
        .and_then(|report| {
            report
                .registry()
                .get(&outcome.plugin_id)
                .map(plugins::RegisteredPlugin::is_enabled)
        })
        .unwrap_or(false);
    Ok(PluginInstallReport {
        source_id: record.id.clone(),
        entry_name: name.to_string(),
        plugin_id: outcome.plugin_id,
        name: manifest.name,
        version: outcome.version,
        path: outcome.install_path.display().to_string(),
        enabled,
        trusted: record.trusted,
        components,
    })
}

fn build_install_request(
    paths: &PluginMarketPaths,
    raw: &RawPluginEntry,
    entry: &PluginEntryView,
    root: Option<&Path>,
) -> Result<(PluginInstallRequest, Option<PathBuf>), MarketError> {
    let source = &entry.source;
    match source.kind.as_str() {
        "url" | "git-subdir" => {
            let url = source
                .url
                .clone()
                .ok_or_else(|| MarketError::new(422, "插件来源缺少 url"))?;
            let sha = source
                .sha
                .clone()
                .ok_or_else(|| MarketError::new(422, "插件来源缺少 pinned sha"))?;
            Ok((
                PluginInstallRequest {
                    source: PluginInstallSource::GitUrl { url },
                    subdir: source.path.clone().map(PathBuf::from),
                    git_ref: Some(sha),
                    expected_sha256: None,
                },
                None,
            ))
        }
        "path" => {
            let Some(root) = root else {
                return Err(MarketError::new(
                    422,
                    "仓库内相对路径条目需要本地或 git 来源才能安装",
                ));
            };
            let relative = source
                .path
                .as_deref()
                .ok_or_else(|| MarketError::new(422, "插件来源缺少 path"))?;
            let dir = resolve_entry_path(root, relative)
                .ok_or_else(|| MarketError::new(422, format!("仓库内路径 `{relative}` 不可读")))?;
            if !has_plugin_manifest(&dir) {
                // `strict:false`：索引内联声明即插件契约，生成清单后再校验摘要。
                let staging = stage_inline_plugin(paths, raw, &dir)?;
                let checksum = compute_plugin_tree_sha256(&staging)
                    .inspect_err(|_| {
                        let _ = remove_any(&staging);
                    })
                    .map_err(map_plugin_error)?;
                return Ok((
                    PluginInstallRequest {
                        source: PluginInstallSource::LocalPath {
                            path: staging.clone(),
                        },
                        subdir: None,
                        git_ref: None,
                        expected_sha256: Some(checksum),
                    },
                    Some(staging),
                ));
            }
            let checksum = compute_plugin_tree_sha256(&dir).map_err(map_plugin_error)?;
            if source.sha256.as_deref() != Some(checksum.as_str()) {
                return Err(MarketError::new(
                    409,
                    "插件内容在索引生成后发生变化，请刷新来源后重试",
                ));
            }
            Ok((
                PluginInstallRequest {
                    source: PluginInstallSource::LocalPath { path: dir },
                    subdir: None,
                    git_ref: None,
                    expected_sha256: Some(checksum),
                },
                None,
            ))
        }
        _ => Err(MarketError::new(422, "插件来源类型无法安装")),
    }
}

/// 把 `strict:false` 条目声明的组件复制到暂存目录，并生成插件清单。
///
/// 只复制索引显式声明的组件，避免把整仓（如 `anthropics/skills` 的 11MB
/// 全部技能）塞进每个插件的安装目录；清单内容全部来自索引，不做改写。
fn stage_inline_plugin(
    paths: &PluginMarketPaths,
    raw: &RawPluginEntry,
    dir: &Path,
) -> Result<PathBuf, MarketError> {
    let mut inline = raw.inline_components();
    // `./skills/x` 与 `skills/x` 等价；统一去掉前缀，避免安装路径出现 `./`。
    let clean = |path: &str| path.strip_prefix("./").unwrap_or(path).to_string();
    inline.skills = inline.skills.iter().map(|path| clean(path)).collect();
    inline.agents = inline.agents.iter().map(|path| clean(path)).collect();
    inline.commands = inline.commands.iter().map(|path| clean(path)).collect();
    let staging = paths.root.join("staging").join(Uuid::new_v4().to_string());
    let result = (|| -> Result<(), MarketError> {
        fs::create_dir_all(&staging)
            .map_err(|error| MarketError::new(500, format!("创建暂存目录失败：{error}")))?;
        for relative in inline.paths() {
            let from = resolve_entry_path(dir, relative).ok_or_else(|| {
                MarketError::new(422, format!("条目声明的组件路径 `{relative}` 不存在"))
            })?;
            let to = staging.join(relative);
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| MarketError::new(500, format!("创建暂存目录失败：{error}")))?;
            }
            let copied = if from.is_dir() {
                fs::create_dir_all(&to).and_then(|()| crate::copy_directory_contents(&from, &to))
            } else {
                fs::copy(&from, &to).map(|_| ())
            };
            copied.map_err(|error| {
                MarketError::new(500, format!("复制组件 `{relative}` 失败：{error}"))
            })?;
        }
        let mut manifest = serde_json::json!({
            "name": raw.name.as_deref().unwrap_or("").trim(),
            "version": raw.version.as_deref().unwrap_or("0.0.0"),
            "description": raw.description.as_deref().unwrap_or(""),
        });
        if !inline.skills.is_empty() {
            manifest["skills"] = serde_json::json!(inline.skills);
        }
        if !inline.agents.is_empty() {
            manifest["agents"] = serde_json::json!(inline.agents);
        }
        if !inline.commands.is_empty() {
            manifest["commands"] = serde_json::json!(inline.commands);
        }
        if let Some(hooks) = inline.hooks {
            manifest["hooks"] = hooks;
        }
        if let Some(servers) = inline.mcp_servers {
            manifest["mcpServers"] = servers;
        }
        let manifest_dir = staging.join(".claude-plugin");
        fs::create_dir_all(&manifest_dir)
            .map_err(|error| MarketError::new(500, format!("创建清单目录失败：{error}")))?;
        fs::write(
            manifest_dir.join("plugin.json"),
            serde_json::to_vec_pretty(&manifest)
                .map_err(|error| MarketError::new(500, format!("生成插件清单失败：{error}")))?,
        )
        .map_err(|error| MarketError::new(500, format!("写入插件清单失败：{error}")))?;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(staging),
        Err(error) => {
            let _ = remove_any(&staging);
            Err(error)
        }
    }
}

pub fn installed_plugins(manager: &PluginManager) -> Result<PluginListReport, MarketError> {
    let report = manager.plugin_registry_report().map_err(map_plugin_error)?;
    let plugins = report
        .registry()
        .plugins()
        .iter()
        .map(|plugin| {
            let metadata = plugin.metadata();
            InstalledPluginView {
                id: metadata.id.clone(),
                name: metadata.name.clone(),
                version: metadata.version.clone(),
                description: metadata.description.clone(),
                kind: metadata.kind.to_string(),
                source: metadata.source.clone(),
                enabled: plugin.is_enabled(),
                root: metadata
                    .root
                    .as_ref()
                    .map(|path| path.display().to_string()),
                components: PluginComponentsSummary::from_components(plugin.components()),
            }
        })
        .collect();
    Ok(PluginListReport {
        plugins,
        failures: report.failures().iter().map(ToString::to_string).collect(),
    })
}

pub fn installed_plugin(
    manager: &PluginManager,
    plugin_id: &str,
) -> Result<InstalledPluginView, MarketError> {
    let report = manager.plugin_registry_report().map_err(map_plugin_error)?;
    let plugin = report
        .registry()
        .get(plugin_id)
        .ok_or_else(|| MarketError::not_found(format!("插件 {plugin_id} 未安装")))?;
    let metadata = plugin.metadata();
    Ok(InstalledPluginView {
        id: metadata.id.clone(),
        name: metadata.name.clone(),
        version: metadata.version.clone(),
        description: metadata.description.clone(),
        kind: metadata.kind.to_string(),
        source: metadata.source.clone(),
        enabled: plugin.is_enabled(),
        root: metadata
            .root
            .as_ref()
            .map(|path| path.display().to_string()),
        components: PluginComponentsSummary::from_components(plugin.components()),
    })
}

pub fn set_enabled(
    manager: &mut PluginManager,
    plugin_id: &str,
    enabled: bool,
) -> Result<bool, MarketError> {
    if enabled {
        manager.enable(plugin_id).map_err(map_plugin_error)?;
    } else {
        manager.disable(plugin_id).map_err(map_plugin_error)?;
    }
    Ok(manager
        .plugin_registry_report()
        .ok()
        .and_then(|report| {
            report
                .registry()
                .get(plugin_id)
                .map(plugins::RegisteredPlugin::is_enabled)
        })
        .unwrap_or(false))
}

pub fn uninstall_plugin(manager: &mut PluginManager, plugin_id: &str) -> Result<(), MarketError> {
    manager.uninstall(plugin_id).map_err(map_plugin_error)
}

fn map_plugin_error(error: PluginError) -> MarketError {
    match &error {
        PluginError::NotFound(message) => MarketError::new(404, message.clone()),
        PluginError::InvalidManifest(message) | PluginError::CommandFailed(message) => {
            MarketError::new(422, message.clone())
        }
        PluginError::ManifestValidation(_) | PluginError::LoadFailures(_) => {
            MarketError::new(422, error.to_string())
        }
        PluginError::Io(_) | PluginError::Json(_) => {
            MarketError::new(500, format!("插件操作失败：{error}"))
        }
    }
}

// ---------------------------------------------------------------------------
// 已安装匹配
// ---------------------------------------------------------------------------

struct InstalledSnapshot {
    records: Vec<plugins::InstalledPluginRecord>,
    enabled: BTreeSet<String>,
}

impl InstalledSnapshot {
    fn load(manager: &PluginManager) -> Self {
        let records = fs::read_to_string(manager.registry_path())
            .ok()
            .and_then(|text| serde_json::from_str::<plugins::InstalledPluginRegistry>(&text).ok())
            .map(|registry| registry.plugins.into_values().collect())
            .unwrap_or_default();
        let enabled = manager
            .plugin_registry_report()
            .ok()
            .map(|report| {
                report
                    .registry()
                    .plugins()
                    .iter()
                    .filter(|plugin| plugin.is_enabled())
                    .map(|plugin| plugin.metadata().id.clone())
                    .collect()
            })
            .unwrap_or_default();
        Self { records, enabled }
    }

    fn match_entry(
        &self,
        name: &str,
        source: &PluginEntrySourceView,
    ) -> Option<InstalledPluginBrief> {
        let record = self
            .records
            .iter()
            .find(|record| self.same_source(record, source))
            .or_else(|| self.records.iter().find(|record| record.name == name))?;
        Some(InstalledPluginBrief {
            plugin_id: record.id.clone(),
            version: record.version.clone(),
            enabled: self.enabled.contains(&record.id),
        })
    }

    fn same_source(
        &self,
        record: &plugins::InstalledPluginRecord,
        source: &PluginEntrySourceView,
    ) -> bool {
        match (&record.source, source.kind.as_str()) {
            (PluginInstallSource::GitUrl { url }, "url" | "git-subdir") => {
                let expected_subdir = source.path.as_deref().map(|path| path.replace('\\', "/"));
                let actual_subdir = record
                    .subdir
                    .as_ref()
                    .map(|path| path.to_string_lossy().replace('\\', "/"));
                same_git_url(url, source.url.as_deref().unwrap_or(""))
                    && expected_subdir == actual_subdir
            }
            (PluginInstallSource::LocalPath { path }, "path") => {
                source.path.as_deref().is_some_and(|relative| {
                    let actual = path.to_string_lossy().replace('\\', "/");
                    actual.ends_with(relative)
                })
            }
            _ => false,
        }
    }
}

fn same_git_url(left: &str, right: &str) -> bool {
    let normalize = |value: &str| {
        value
            .trim()
            .trim_end_matches('/')
            .trim_end_matches(".git")
            .to_ascii_lowercase()
    };
    normalize(left) == normalize(right)
}

// ---------------------------------------------------------------------------
// 组件计数
// ---------------------------------------------------------------------------

fn path_strings(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect()
}

fn count_skills(paths: &[PathBuf]) -> usize {
    let mut count = 0;
    for root in paths {
        if root.join("SKILL.md").is_file() {
            count += 1;
            continue;
        }
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join("SKILL.md").is_file() {
                count += 1;
            }
        }
    }
    count
}

fn count_commands(paths: &[PathBuf]) -> usize {
    let mut count = 0;
    for root in paths {
        if root.is_file() {
            if root.extension().is_some_and(|ext| ext == "md") {
                count += 1;
            }
            continue;
        }
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        count += entries
            .flatten()
            .filter(|entry| {
                let path = entry.path();
                path.is_file() && path.extension().is_some_and(|ext| ext == "md")
            })
            .count();
    }
    count
}

// ---------------------------------------------------------------------------
// 网络与 git
// ---------------------------------------------------------------------------

fn fetch_http(url: &str) -> Result<String, (u16, String)> {
    let client = reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent("claw-plugin-market/1.0")
        .build()
        .map_err(|error| (500, format!("初始化 HTTP 客户端失败：{error}")))?;
    let response = client.get(url).send().map_err(|error| {
        if error.is_timeout() {
            (
                504,
                format!(
                    "拉取 marketplace.json 超时（{} 秒），请检查网络后重试",
                    HTTP_TIMEOUT.as_secs()
                ),
            )
        } else {
            (502, format!("拉取 marketplace.json 失败：{error}"))
        }
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err((
            502,
            format!("拉取 marketplace.json 返回 HTTP {}", status.as_u16()),
        ));
    }
    response
        .text()
        .map_err(|error| (502, format!("读取 marketplace.json 失败：{error}")))
}

fn clone_with_timeout(value: &str, dest: &Path) -> Result<(), (&'static str, String)> {
    let mut child = Command::new("git")
        .arg("clone")
        .arg("--depth")
        .arg("1")
        .arg("--quiet")
        .arg("--")
        .arg(value)
        .arg(dest)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                (
                    "git_not_found",
                    "本机未找到 git，无法拉取 git 来源".to_string(),
                )
            } else {
                ("clone_failed", format!("启动 git 失败：{error}"))
            }
        })?;
    let deadline = Instant::now() + GIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                let first = stderr.lines().next().unwrap_or("未知错误").trim();
                return Err(("clone_failed", format!("拉取仓库失败：{first}")));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err((
                        "timeout",
                        format!(
                            "拉取仓库超时（{} 秒），请重试或改用本地来源",
                            GIT_TIMEOUT.as_secs()
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(("clone_failed", format!("等待 git 失败：{error}"))),
        }
    }
}

fn git_full_ref(dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    is_pinned_commit(&value).then_some(value)
}

fn remove_any(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else if path.exists() {
        fs::remove_file(path)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugins::PluginManagerConfig;
    use serde_json::json;

    fn write_plugin(root: &Path, name: &str) {
        fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        fs::write(
            root.join(".claude-plugin").join("plugin.json"),
            json!({
                "name": name,
                "version": "1.0.0",
                "description": "测试插件",
            })
            .to_string(),
        )
        .unwrap();
        let skill = root.join("skills").join(format!("{name}-skill"));
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}-skill\ndescription: 测试技能\n---\n# 测试技能\n"),
        )
        .unwrap();
    }

    fn write_marketplace(repo: &Path, entry_source: Value) {
        fs::create_dir_all(repo.join(".claude-plugin")).unwrap();
        fs::write(
            repo.join(".claude-plugin").join("marketplace.json"),
            json!({
                "name": "test-market",
                "owner": {"name": "tester"},
                "plugins": [{
                    "name": "demo",
                    "description": "测试市场插件",
                    "source": entry_source,
                }],
            })
            .to_string(),
        )
        .unwrap();
    }

    fn git(args: &[&str], cwd: &Path) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} 失败");
    }

    fn file_url(path: &Path) -> String {
        let text = path.to_string_lossy().replace('\\', "/");
        if text.starts_with('/') {
            format!("file://{text}")
        } else {
            format!("file:///{text}")
        }
    }

    #[test]
    fn claude_source_shapes_are_normalized() {
        match normalize_entry_source(Some(&json!("./plugins/a"))) {
            NormalizedSource::Path { path } => assert_eq!(path, "plugins/a"),
            _ => panic!("相对路径没有被识别"),
        }
        match normalize_entry_source(Some(&json!("./"))) {
            NormalizedSource::Path { path } => assert_eq!(path, "."),
            _ => panic!("仓库根 `./` 没有被识别"),
        }
        let sha = "0123456789abcdef0123456789abcdef01234567";
        match normalize_entry_source(Some(&json!({
            "source": "url",
            "url": "https://example.com/a.git",
            "sha": sha
        }))) {
            NormalizedSource::Remote {
                kind,
                url,
                sha: parsed_sha,
                ..
            } => {
                assert_eq!(kind, "url");
                assert_eq!(url, "https://example.com/a.git");
                assert_eq!(parsed_sha.as_deref(), Some(sha));
            }
            _ => panic!("url 来源没有被识别"),
        }
        match normalize_entry_source(Some(&json!({
            "source": "git-subdir",
            "url": "owner/repo",
            "path": "plugins/b",
            "sha": sha
        }))) {
            NormalizedSource::Remote {
                kind, url, path, ..
            } => {
                assert_eq!(kind, "git-subdir");
                assert_eq!(url, "https://github.com/owner/repo.git");
                assert_eq!(path.as_deref(), Some("plugins/b"));
            }
            _ => panic!("git-subdir 来源没有被识别"),
        }
        match normalize_entry_source(Some(&json!({
            "source": "github",
            "repo": "owner/repo"
        }))) {
            NormalizedSource::Remote { url, .. } => {
                assert_eq!(url, "https://github.com/owner/repo.git");
            }
            _ => panic!("github 来源没有被识别"),
        }
    }

    fn raw_entry(source: Value) -> RawPluginEntry {
        serde_json::from_value(json!({"name": "demo", "source": source})).unwrap()
    }

    #[test]
    fn entries_without_pinned_sha_are_not_installable() {
        let analysis = analyze_source(
            &raw_entry(json!({"source": "url", "url": "https://example.com/a.git"})),
            None,
            None,
        );
        assert!(!analysis.installable);
        assert_eq!(analysis.reason.as_deref(), Some("missing_integrity_anchor"));
        let analysis = analyze_source(
            &raw_entry(json!({"source": "url", "url": "https://example.com/a.git", "sha": "abc"})),
            None,
            None,
        );
        assert!(!analysis.installable);
        assert_eq!(analysis.reason.as_deref(), Some("invalid_sha"));
        // HTTP 来源下的仓库内相对路径也没有完整性锚点。
        let analysis = analyze_source(&raw_entry(json!("./plugins/a")), None, None);
        assert!(!analysis.installable);
        assert_eq!(analysis.reason.as_deref(), Some("missing_integrity_anchor"));
    }

    #[test]
    fn local_marketplace_install_enable_and_uninstall_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("market");
        write_plugin(&repo.join("plugins").join("demo"), "demo");
        write_marketplace(&repo, json!("./plugins/demo"));

        let config_home = temp.path().join("home");
        let paths =
            PluginMarketPaths::with_roots(config_home.join("plugin-market"), config_home.clone());
        ensure_default_sources(&paths).unwrap();
        let view = add_source(
            &paths,
            "local",
            repo.to_str().unwrap(),
            Some("本地测试源"),
            None,
        )
        .unwrap();
        assert_eq!(view.plugin_count, 1);
        assert!(!view.trusted, "手动来源必须是 untrusted");

        let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        let index = market_index(&paths, Some(&manager), &view.id).unwrap();
        assert_eq!(index.plugins.len(), 1);
        let entry = &index.plugins[0];
        assert!(entry.installable, "{entry:?}");
        assert!(entry.source.sha256.is_some());

        let denied = install_plugin(&paths, &mut manager, &view.id, "demo", false).unwrap_err();
        assert_eq!(denied.status, 403);

        let report = install_plugin(&paths, &mut manager, &view.id, "demo", true).unwrap();
        assert_eq!(report.components.skill_count, 1);
        assert!(report.enabled);
        assert!(Path::new(&report.path)
            .join("skills")
            .join("demo-skill")
            .join("SKILL.md")
            .is_file());

        let plugin_id = report.plugin_id.clone();
        let list = installed_plugins(&manager).unwrap();
        assert!(list.plugins.iter().any(|plugin| plugin.id == plugin_id
            && plugin.enabled
            && plugin.components.skill_count == 1));

        assert!(!set_enabled(&mut manager, &plugin_id, false).unwrap());
        assert!(!installed_plugin(&manager, &plugin_id).unwrap().enabled);
        assert!(set_enabled(&mut manager, &plugin_id, true).unwrap());

        uninstall_plugin(&mut manager, &plugin_id).unwrap();
        assert!(!installed_plugins(&manager)
            .unwrap()
            .plugins
            .iter()
            .any(|plugin| plugin.id == plugin_id));
        assert!(!Path::new(&report.path).exists());
    }

    #[test]
    fn git_source_relative_path_entry_installs_from_mirror() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("git-market");
        write_plugin(&repo.join("plugins").join("demo"), "demo");
        write_marketplace(&repo, json!("./plugins/demo"));
        git(&["init", "-q"], &repo);
        git(&["add", "."], &repo);
        git(
            &[
                "-c",
                "user.name=Claw Test",
                "-c",
                "user.email=claw@example.com",
                "commit",
                "-q",
                "-m",
                "init",
            ],
            &repo,
        );

        let config_home = temp.path().join("home");
        let paths =
            PluginMarketPaths::with_roots(config_home.join("plugin-market"), config_home.clone());
        ensure_default_sources(&paths).unwrap();
        let view = add_source(&paths, "git", &file_url(&repo), Some("git 测试源"), None).unwrap();
        assert_eq!(view.plugin_count, 1);
        assert!(view.git_ref.as_deref().is_some_and(is_pinned_commit));

        let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        let index = market_index(&paths, Some(&manager), &view.id).unwrap();
        let entry = &index.plugins[0];
        assert!(entry.installable, "{entry:?}");
        assert_eq!(entry.source.kind, "path");
        assert!(entry.source.sha256.is_some());

        let report = install_plugin(&paths, &mut manager, &view.id, "demo", true).unwrap();
        assert_eq!(report.components.skill_count, 1);
        uninstall_plugin(&mut manager, &report.plugin_id).unwrap();
    }

    #[test]
    fn strict_false_inline_skills_install_from_git_source() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("skills-market");
        let skill = repo.join("skills").join("demo-skill");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: demo-skill\ndescription: 内联技能\n---\n# 内联技能\n",
        )
        .unwrap();
        fs::create_dir_all(repo.join(".claude-plugin")).unwrap();
        // 与 `anthropics/skills` 相同的形态：仓库根就是插件，无 plugin.json。
        fs::write(
            repo.join(".claude-plugin").join("marketplace.json"),
            json!({
                "name": "inline-market",
                "plugins": [{
                    "name": "demo",
                    "description": "内联声明的插件",
                    "source": "./",
                    "strict": false,
                    "skills": ["./skills/demo-skill"],
                }],
            })
            .to_string(),
        )
        .unwrap();
        git(&["init", "-q"], &repo);
        git(&["add", "."], &repo);
        git(
            &[
                "-c",
                "user.name=Claw Test",
                "-c",
                "user.email=claw@example.com",
                "commit",
                "-q",
                "-m",
                "init",
            ],
            &repo,
        );

        let config_home = temp.path().join("home");
        let paths =
            PluginMarketPaths::with_roots(config_home.join("plugin-market"), config_home.clone());
        let view = add_source(&paths, "git", &file_url(&repo), Some("内联测试源"), None).unwrap();
        let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        let index = market_index(&paths, Some(&manager), &view.id).unwrap();
        let entry = &index.plugins[0];
        assert!(entry.installable, "{entry:?}");
        assert_eq!(entry.source.kind, "path");
        assert!(entry.source.sha.as_deref().is_some_and(is_pinned_commit));

        let report = install_plugin(&paths, &mut manager, &view.id, "demo", true).unwrap();
        assert_eq!(report.components.skill_count, 1);
        assert!(Path::new(&report.path)
            .join("skills")
            .join("demo-skill")
            .join("SKILL.md")
            .is_file());
        assert!(Path::new(&report.path)
            .join(".claude-plugin")
            .join("plugin.json")
            .is_file());
        // 暂存目录安装后必须清理。
        assert!(!paths.root.join("staging").exists());
        uninstall_plugin(&mut manager, &report.plugin_id).unwrap();
        assert!(!Path::new(&report.path).exists());
    }

    #[test]
    fn http_source_falls_back_to_cached_index() {
        let temp = tempfile::tempdir().unwrap();
        let paths = PluginMarketPaths::with_roots(
            temp.path().join("plugin-market"),
            temp.path().to_path_buf(),
        );
        let source = PluginSourceRecord {
            id: "down".to_string(),
            kind: "http".to_string(),
            value: "http://127.0.0.1:9/marketplace.json".to_string(),
            name: None,
            subdir: None,
            trusted: false,
            builtin: false,
            status: None,
            last_error: None,
            checked_at_unix_ms: None,
            git_ref: None,
            last_success_at_unix_ms: None,
        };
        let cache = IndexCache {
            schema: INDEX_CACHE_SCHEMA.to_string(),
            // 过期缓存才会触发网络请求，从而走降级分支。
            fetched_at_unix_ms: now_unix_ms().saturating_sub(3_600_000),
            url: "http://127.0.0.1:9/marketplace.json".to_string(),
            body: json!({
                "name": "cached",
                "plugins": [{"name": "cached-plugin", "source": "./x"}]
            })
            .to_string(),
        };
        write_json_atomic(&paths.cache_path("down").unwrap(), &cache).unwrap();

        let loaded = load_index(&paths, &source).unwrap();
        assert!(loaded.cached);
        assert!(loaded.cached_at_unix_ms.is_some());
        assert_eq!(loaded.marketplace.plugins.len(), 1);
        assert!(loaded.last_error.is_some());
    }

    #[test]
    #[ignore = "需要公网访问 raw.githubusercontent.com"]
    fn real_community_marketplace_lists_and_has_installable_entries() {
        let temp = tempfile::tempdir().unwrap();
        let paths = PluginMarketPaths::with_roots(
            temp.path().join("plugin-market"),
            temp.path().to_path_buf(),
        );
        let source = PluginSourceRecord {
            id: "community".to_string(),
            kind: "http".to_string(),
            value: "anthropics/claude-plugins-community".to_string(),
            name: Some("Claude Plugins Community".to_string()),
            subdir: None,
            trusted: true,
            builtin: false,
            status: None,
            last_error: None,
            checked_at_unix_ms: None,
            git_ref: None,
            last_success_at_unix_ms: None,
        };
        write_sources(
            &paths,
            &PluginSourcesRegistry {
                schema: sources_schema(),
                sources: vec![source],
            },
        )
        .unwrap();

        let index = market_index(&paths, None, "community").unwrap();
        assert!(index.live);
        assert!(!index.cached);
        assert!(
            index.plugins.len() > 2000,
            "真实社区索引条目过少：{}",
            index.plugins.len()
        );
        let installable = index
            .plugins
            .iter()
            .filter(|entry| entry.installable)
            .count();
        assert!(installable > 2000, "可安装条目过少：{installable}");
        assert!(index.plugins.iter().any(|entry| !entry.installable
            && entry.reason.as_deref() == Some("missing_integrity_anchor")));
        assert!(index
            .plugins
            .iter()
            .any(|entry| entry.source.kind == "git-subdir" && entry.installable));
        assert!(index
            .plugins
            .iter()
            .any(|entry| entry.source.kind == "url" && entry.installable));

        let first = index
            .plugins
            .iter()
            .find(|entry| entry.installable)
            .unwrap();
        assert!(!first.name.is_empty());
        let detail = plugin_detail(&paths, None, "community", &first.name).unwrap();
        assert_eq!(detail.plugin.name, first.name);
        assert!(detail.plugin.installable);
        assert!(detail.plugin.source.url.is_some());
        assert!(detail.plugin.source.sha.is_some());
    }
}
