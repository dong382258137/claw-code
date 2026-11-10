//! 技能市场：本地索引来源、原子安装/卸载与市场注册表。
//!
//! 设计约束（详见 `docs/skill-market-roadmap.md` §2/§4）：
//!
//! - 注册表与暂存区放在 `<config_home>/skill-market/`，绝不在发现根
//!   `skills/` 内 —— 复制到一半的目录即使已含 `SKILL.md` 也不会被并发
//!   请求当成真技能。
//! - 安装顺序固定为：复制到 staging → staging 校验 → rename 入安装根 →
//!   最后写注册表。任一步失败按逆序回滚，不留下半个技能目录。
//! - 卸载只允许 `managed: true` 的记录；目录先 rename 到 trash，注册表
//!   写成功后才真正删除。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const INSTALLED_SCHEMA: &str = "claw.skill-installed/v1";
pub const SOURCES_SCHEMA: &str = "claw.skill-sources/v1";
pub const MARKET_SCHEMA: &str = "claw.skill-market/v1";

/// 单个技能目录的大小/文件数上限（与路线 §3.5 一致）。
const MAX_SKILL_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SKILL_FILES: usize = 200;

/// 市场错误：`status` 直接映射 HTTP 状态码，`message` 为中文纯文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketError {
    pub status: u16,
    pub message: String,
}

impl MarketError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(409, message)
    }

    pub fn unprocessable(message: impl Into<String>) -> Self {
        Self::new(422, message)
    }

    pub fn bad_gateway(message: impl Into<String>) -> Self {
        Self::new(502, message)
    }
}

impl std::fmt::Display for MarketError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

/// 市场根与技能安装根。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketPaths {
    /// `<config_home>/skill-market`：注册表、staging、trash。
    pub root: PathBuf,
    /// `<config_home>/skills`：技能发现/安装根。
    pub install_root: PathBuf,
}

impl MarketPaths {
    /// 生产路径：配置家目录 + `commands` 的安装根解析。
    ///
    /// 安装根解析失败（HOME/USERPROFILE 全缺失）时保留空路径，由端点
    /// 返回明确错误，而不是把技能装到随 cwd 漂移的相对目录。
    #[must_use]
    pub fn discover() -> Self {
        Self {
            root: runtime::default_config_home().join("skill-market"),
            install_root: crate::skill_install_root().unwrap_or_default(),
        }
    }

    #[must_use]
    pub fn with_roots(root: PathBuf, install_root: PathBuf) -> Self {
        Self { root, install_root }
    }

    fn installed_path(&self) -> PathBuf {
        self.root.join("installed.json")
    }

    fn sources_path(&self) -> PathBuf {
        self.root.join("sources.json")
    }

    fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }

    fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }

    /// git 来源的本地镜像：`<skill-market>/mirrors/<source_id>`。
    ///
    /// 只在新增来源/显式刷新时 clone；列表与索引读取都走镜像，避免每次
    /// `GET /api/skill-sources` 都触发网络。
    fn mirror_dir(&self) -> PathBuf {
        self.root.join("mirrors")
    }

    fn trash_dir(&self) -> PathBuf {
        self.root.join("trash")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledSource {
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledRecord {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub version: String,
    pub marketplace: String,
    pub source: InstalledSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    pub installed_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub managed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledRegistry {
    #[serde(default = "installed_schema")]
    pub schema: String,
    #[serde(default)]
    pub skills: BTreeMap<String, InstalledRecord>,
}

fn installed_schema() -> String {
    INSTALLED_SCHEMA.to_string()
}

impl Default for InstalledRegistry {
    fn default() -> Self {
        Self {
            schema: installed_schema(),
            skills: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceRecord {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at_unix_ms: Option<u64>,
    /// git 来源最近一次 clone 的短 commit（本地来源为空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourcesRegistry {
    #[serde(default = "sources_schema")]
    pub schema: String,
    #[serde(default)]
    pub sources: Vec<SourceRecord>,
}

fn sources_schema() -> String {
    SOURCES_SCHEMA.to_string()
}

impl Default for SourcesRegistry {
    fn default() -> Self {
        Self {
            schema: sources_schema(),
            sources: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MarketplaceIndex {
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub skills: Vec<MarketplaceEntry>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MarketplaceEntry {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub min_claw_version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceView {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
    pub name: Option<String>,
    pub subdir: Option<String>,
    pub status: String,
    pub skill_count: usize,
    pub last_error: Option<String>,
    pub checked_at_unix_ms: u64,
    /// git 来源最近一次 clone 的短 commit；本地来源恒为 `null`。
    pub git_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstalledBrief {
    pub version: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MarketSkillView {
    pub name: String,
    pub version: String,
    pub description: String,
    pub tags: Vec<String>,
    pub installable: bool,
    pub reason: Option<&'static str>,
    pub reason_detail: Option<String>,
    pub installed: Option<InstalledBrief>,
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MarketSourceBrief {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: Option<String>,
    pub status: String,
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MarketIndexView {
    pub source: MarketSourceBrief,
    pub skills: Vec<MarketSkillView>,
}

/// 安装故障注入点：仅用于单测确定性验证回滚，生产固定 `None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    None,
    /// 复制完成、staging 校验阶段强制失败。
    ValidateAfterCopy,
    /// 旧目录已移入 trash、staging 尚未 rename 入安装根时强制失败。
    RenameToTarget,
}

struct ResolvedSource {
    status: &'static str,
    last_error: Option<String>,
    root: Option<PathBuf>,
    index: Option<MarketplaceIndex>,
}

struct AnalyzedEntry {
    raw_name: String,
    install_name: Option<String>,
    version: String,
    description: String,
    tags: Vec<String>,
    display_name: Option<String>,
    source_path: Option<PathBuf>,
    expected_sha256: Option<String>,
    reason: Option<&'static str>,
    detail: Option<String>,
}

/// 同名安装/卸载互斥：持有期间其他请求得到 409，Drop 时释放。
pub struct NameLock {
    locks: Arc<Mutex<BTreeSet<String>>>,
    name: String,
}

impl NameLock {
    pub fn acquire(
        locks: &Arc<Mutex<BTreeSet<String>>>,
        name: &str,
        busy_message: &str,
    ) -> Result<Self, MarketError> {
        let mut set = locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !set.insert(name.to_string()) {
            return Err(MarketError::conflict(busy_message.to_string()));
        }
        drop(set);
        Ok(Self {
            locks: Arc::clone(locks),
            name: name.to_string(),
        })
    }
}

impl Drop for NameLock {
    fn drop(&mut self) {
        if let Ok(mut set) = self.locks.lock() {
            set.remove(&self.name);
        }
    }
}

/// 注册表读取：文件缺失或损坏一律按空清单处理（磁盘才是事实来源）。
#[must_use]
pub fn read_installed(paths: &MarketPaths) -> InstalledRegistry {
    let path = paths.installed_path();
    let Ok(text) = fs::read_to_string(&path) else {
        return InstalledRegistry::default();
    };
    match serde_json::from_str::<InstalledRegistry>(&text) {
        Ok(mut registry) => {
            registry.schema = installed_schema();
            registry
        }
        Err(error) => {
            eprintln!(
                "[skill-market] 解析 {} 失败，按空清单处理：{error}",
                path.display()
            );
            InstalledRegistry::default()
        }
    }
}

fn read_sources(paths: &MarketPaths) -> SourcesRegistry {
    let path = paths.sources_path();
    let Ok(text) = fs::read_to_string(&path) else {
        return SourcesRegistry::default();
    };
    match serde_json::from_str::<SourcesRegistry>(&text) {
        Ok(mut registry) => {
            registry.schema = sources_schema();
            registry
        }
        Err(error) => {
            eprintln!(
                "[skill-market] 解析 {} 失败，按空清单处理：{error}",
                path.display()
            );
            SourcesRegistry::default()
        }
    }
}

fn write_installed(paths: &MarketPaths, registry: &InstalledRegistry) -> io::Result<()> {
    write_json_atomic(&paths.installed_path(), registry)
}

fn write_sources(paths: &MarketPaths, registry: &SourcesRegistry) -> io::Result<()> {
    write_json_atomic(&paths.sources_path(), registry)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "market.json".to_string());
    let tmp = path.with_file_name(format!("{file_name}.tmp-{}", std::process::id()));
    let text = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    fs::write(&tmp, text)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            Err(error)
        }
    }
}

/// 卸载时把名字从 `state.json.disabled` 移除（文件不存在即无操作）。
fn remove_disabled(paths: &MarketPaths, name: &str) -> io::Result<()> {
    let path = paths.state_path();
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(());
    };
    let Ok(mut value) = serde_json::from_str::<Value>(&text) else {
        return Ok(());
    };
    let Some(disabled) = value.get_mut("disabled").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    let before = disabled.len();
    disabled.retain(|item| item.as_str() != Some(name));
    if disabled.len() == before {
        return Ok(());
    }
    write_json_atomic(&path, &value)
}

/// 技能启停状态文件版本。
pub const STATE_SCHEMA: &str = "claw.skill-state/v1";

/// `<market root>/state.json`：只记录**被禁用**的技能名。
///
/// 文件缺失或损坏一律按「全部启用」处理 —— 状态读失败绝不能把技能锁死。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default)]
    pub disabled: BTreeSet<String>,
}

/// 读取启停状态（容错：读失败 / 解析失败 / 版本不符都按空状态处理）。
#[must_use]
pub fn read_state(paths: &MarketPaths) -> SkillState {
    let Ok(text) = fs::read_to_string(paths.state_path()) else {
        return SkillState::default();
    };
    let state = serde_json::from_str::<SkillState>(&text).unwrap_or_default();
    match state.schema.as_deref() {
        Some(STATE_SCHEMA) | None => state,
        Some(_) => SkillState::default(),
    }
}

fn write_state(paths: &MarketPaths, state: &SkillState) -> io::Result<()> {
    let mut state = state.clone();
    state.schema = Some(STATE_SCHEMA.to_string());
    write_json_atomic(&paths.state_path(), &state)
}

/// 当前被禁用的技能名（读全局市场根；读不到即为空集）。
#[must_use]
pub fn disabled_skill_names() -> BTreeSet<String> {
    read_state(&MarketPaths::discover()).disabled
}

/// 启用/禁用技能（幂等）。技能不存在返回 404。
///
/// 只改 `state.json`，**不碰技能目录** —— 启停不得修改文件内容或 mtime。
pub fn set_enabled(
    paths: &MarketPaths,
    name: &str,
    enabled: bool,
    cwd: &Path,
) -> Result<bool, MarketError> {
    let name = crate::sanitize_skill_invocation_name(name)
        .ok_or_else(|| MarketError::bad_request("技能名不合法"))?;
    let exists = crate::list_skill_summaries(cwd)
        .is_ok_and(|skills| skills.iter().any(|skill| skill.name() == name));
    if !exists {
        return Err(MarketError::not_found(format!("技能 {name} 不存在")));
    }
    let mut state = read_state(paths);
    let changed = if enabled {
        state.disabled.remove(&name)
    } else {
        state.disabled.insert(name.clone())
    };
    if changed {
        write_state(paths, &state)
            .map_err(|error| MarketError::new(500, format!("写入启停状态失败：{error}")))?;
    }
    Ok(enabled)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn next_source_id(sources: &[SourceRecord]) -> String {
    let max = sources
        .iter()
        .filter_map(|source| source.id.strip_prefix("s_"))
        .filter_map(|suffix| suffix.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("s_{:02}", max + 1)
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

/// 从一个已确定的索引目录读 `marketplace.json`。
fn read_index(root: &Path) -> Result<MarketplaceIndex, (String, String)> {
    let file = root.join("marketplace.json");
    let text = fs::read_to_string(&file).map_err(|error| {
        (
            "unreadable".to_string(),
            format!("读取 {} 失败：{error}", file.display()),
        )
    })?;
    let index: MarketplaceIndex = serde_json::from_str(&text).map_err(|error| {
        (
            "unreadable".to_string(),
            format!("marketplace.json 解析失败：{error}"),
        )
    })?;
    // schema 缺失按 v1 处理（§3.4 降级矩阵）；存在但不匹配则标记 unsupported。
    if let Some(schema) = index.schema.as_deref() {
        if schema != MARKET_SCHEMA {
            return Err((
                "unsupported".to_string(),
                format!("不支持的索引版本：{schema}"),
            ));
        }
    }
    Ok(index)
}

/// 解析本地来源：返回 marketplace.json 所在目录（canonical）与索引。
fn load_local_index(value: &str) -> Result<(PathBuf, MarketplaceIndex), (String, String)> {
    let raw = PathBuf::from(value);
    let canonical = fs::canonicalize(&raw).map_err(|error| {
        (
            "unreadable".to_string(),
            format!("路径不可读：{value}（{error}）"),
        )
    })?;
    let root = if canonical.is_dir() {
        canonical
    } else {
        canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| canonical.clone())
    };
    let index = read_index(&root)?;
    Ok((root, index))
}

/// git 来源的索引目录：镜像根 + 可选 `subdir`，canonical 后必须仍在镜像内。
fn git_index_root(mirror: &Path, subdir: Option<&str>) -> Result<PathBuf, String> {
    // 必须 canonical 化：Windows 上 canonicalize 带 `\\?\` 前缀，
    // 与未规范化的镜像路径混用会让 `analyze_entry` 的越界检查误报。
    let base = fs::canonicalize(mirror).map_err(|error| format!("仓库镜像不可读：{error}"))?;
    let Some(subdir) = subdir.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(base);
    };
    let candidate = fs::canonicalize(mirror.join(subdir))
        .map_err(|error| format!("子目录 {subdir} 不可读：{error}"))?;
    if !candidate.starts_with(&base) {
        return Err(format!("子目录 {subdir} 越出仓库根"));
    }
    Ok(candidate)
}

/// 解析来源状态。git 只读本地镜像，不在这里发起网络请求。
fn resolve_source(paths: &MarketPaths, record: &SourceRecord) -> ResolvedSource {
    match record.kind.as_str() {
        "local" => match load_local_index(&record.value) {
            Ok((root, index)) => ResolvedSource {
                status: "ok",
                last_error: None,
                root: Some(root),
                index: Some(index),
            },
            Err((status, error)) => ResolvedSource {
                status: if status == "unsupported" {
                    "unsupported"
                } else {
                    "unreadable"
                },
                last_error: Some(error),
                root: None,
                index: None,
            },
        },
        "git" => {
            let mirror = paths.mirror_dir().join(&record.id);
            if !mirror.is_dir() {
                return ResolvedSource {
                    status: "clone_failed",
                    last_error: Some("尚未拉取仓库，请刷新来源".to_string()),
                    root: None,
                    index: None,
                };
            }
            let root = match git_index_root(&mirror, record.subdir.as_deref()) {
                Ok(root) => root,
                Err(error) => {
                    return ResolvedSource {
                        status: "unreadable",
                        last_error: Some(error),
                        root: None,
                        index: None,
                    }
                }
            };
            match read_index(&root) {
                Ok(index) => ResolvedSource {
                    status: "ok",
                    last_error: None,
                    root: Some(root),
                    index: Some(index),
                },
                Err((status, error)) => ResolvedSource {
                    status: if status == "unsupported" {
                        "unsupported"
                    } else {
                        "unreadable"
                    },
                    last_error: Some(error),
                    root: None,
                    index: None,
                },
            }
        }
        other => ResolvedSource {
            status: "unreadable",
            last_error: Some(format!("未知来源类型：{other}")),
            root: None,
            index: None,
        },
    }
}

/// 拉取仓库超时上限（路线 §4.3）。
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

fn git_short_ref(dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("rev-parse")
        .arg("--short")
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// `git clone --depth 1`，30 秒硬超时；失败归类为 `git_not_found` / `clone_failed` / `timeout`。
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
                    "本机未找到 git，无法安装 git 来源的技能".to_string(),
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
                let first = stderr
                    .lines()
                    .next()
                    .unwrap_or("未知错误")
                    .trim()
                    .to_string();
                return Err((
                    "clone_failed",
                    format!("拉取仓库失败：{first}。请检查网络或仓库地址"),
                ));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err((
                        "timeout",
                        "拉取仓库超时（30 秒），请重试或改用本地来源".to_string(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(("clone_failed", format!("等待 git 失败：{error}")));
            }
        }
    }
}

/// 重新克隆并把旧镜像原子替换掉；失败时清空临时目录。返回短 commit。
fn sync_git_mirror(
    paths: &MarketPaths,
    record: &SourceRecord,
) -> Result<Option<String>, (&'static str, String)> {
    fs::create_dir_all(paths.mirror_dir())
        .map_err(|error| ("clone_failed", format!("创建镜像目录失败：{error}")))?;
    let temp = paths.mirror_dir().join(format!(".tmp-{}", Uuid::new_v4()));
    if let Err(error) = clone_with_timeout(&record.value, &temp) {
        let _ = remove_any(&temp);
        return Err(error);
    }
    let git_ref = git_short_ref(&temp);
    let final_dir = paths.mirror_dir().join(&record.id);
    if final_dir.exists() {
        remove_any(&final_dir)
            .map_err(|error| ("clone_failed", format!("替换旧镜像失败：{error}")))?;
    }
    fs::rename(&temp, &final_dir).map_err(|error| {
        let _ = remove_any(&temp);
        ("clone_failed", format!("保存仓库镜像失败：{error}"))
    })?;
    // 子目录越界/不可读在这里就失败，避免把坏来源标成可用。
    git_index_root(&final_dir, record.subdir.as_deref()).map_err(|error| ("unreadable", error))?;
    Ok(git_ref)
}

/// git 来源：clone 到镜像并回填 `git_ref`；非 git 直接返回 `Ok(())`。
fn sync_if_git(
    paths: &MarketPaths,
    record: &mut SourceRecord,
) -> Result<(), (&'static str, String)> {
    if record.kind != "git" {
        return Ok(());
    }
    record.git_ref = sync_git_mirror(paths, record)?;
    Ok(())
}

/// git 解析失败 → HTTP 错误：无 git/拉取失败 502，超时 504。
fn git_error(status: &str, message: String) -> MarketError {
    match status {
        "timeout" => MarketError::new(504, message),
        "git_not_found" | "clone_failed" => MarketError::bad_gateway(message),
        _ => MarketError::unprocessable(message),
    }
}

fn source_view(record: &SourceRecord, resolved: &ResolvedSource) -> SourceView {
    SourceView {
        id: record.id.clone(),
        kind: record.kind.clone(),
        value: record.value.clone(),
        name: record.name.clone(),
        subdir: record.subdir.clone(),
        status: resolved.status.to_string(),
        skill_count: resolved
            .index
            .as_ref()
            .map_or(0, |index| index.skills.len()),
        last_error: resolved.last_error.clone(),
        checked_at_unix_ms: record.checked_at_unix_ms.unwrap_or_else(now_unix_ms),
        git_ref: record.git_ref.clone(),
    }
}

/// 来源列表：每次请求现读磁盘，不缓存（§3.2）。
#[must_use]
pub fn list_sources(paths: &MarketPaths) -> Vec<SourceView> {
    read_sources(paths)
        .sources
        .iter()
        .map(|record| source_view(record, &resolve_source(paths, record)))
        .collect()
}

/// 新增来源：解析失败也落盘并返回 201 + status，只有类型/值非法才 400。
pub fn add_source(
    paths: &MarketPaths,
    kind: &str,
    value: &str,
    name: Option<&str>,
    subdir: Option<&str>,
) -> Result<SourceView, MarketError> {
    let kind = kind.trim().to_ascii_lowercase();
    if !matches!(kind.as_str(), "local" | "git") {
        return Err(MarketError::bad_request("来源类型必须是 local 或 git"));
    }
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(MarketError::bad_request("路径不能为空"));
    }
    let mut registry = read_sources(paths);
    let mut record = SourceRecord {
        id: next_source_id(&registry.sources),
        kind,
        value,
        name: non_empty(name),
        subdir: non_empty(subdir),
        status: None,
        last_error: None,
        checked_at_unix_ms: None,
        git_ref: None,
    };
    // git 来源立即 clone 一次；失败也落盘，让用户能修正地址后重试。
    let resolved = match sync_if_git(paths, &mut record) {
        Ok(()) => resolve_source(paths, &record),
        Err((status, error)) => ResolvedSource {
            status,
            last_error: Some(error),
            root: None,
            index: None,
        },
    };
    record.status = Some(resolved.status.to_string());
    record.last_error = resolved.last_error.clone();
    record.checked_at_unix_ms = Some(now_unix_ms());
    registry.sources.push(record.clone());
    write_sources(paths, &registry)
        .map_err(|error| MarketError::new(500, format!("保存来源失败：{error}")))?;
    Ok(source_view(&record, &resolved))
}

pub fn delete_source(paths: &MarketPaths, id: &str) -> Result<(), MarketError> {
    let mut registry = read_sources(paths);
    let before = registry.sources.len();
    let removed: Vec<String> = registry
        .sources
        .iter()
        .filter(|source| source.id == id && source.kind == "git")
        .map(|source| source.id.clone())
        .collect();
    registry.sources.retain(|source| source.id != id);
    if registry.sources.len() == before {
        return Err(MarketError::not_found(format!("来源 {id} 不存在")));
    }
    // 删除来源时一并清掉 git 镜像，避免下次同 id 复用陈旧内容。
    for id in removed {
        let _ = remove_any(&paths.mirror_dir().join(id));
    }
    write_sources(paths, &registry)
        .map_err(|error| MarketError::new(500, format!("保存来源失败：{error}")))
}

/// 重新解析来源并持久化状态。git 来源会重新 clone（`resolved_ref` 随之更新）。
pub fn refresh_source(paths: &MarketPaths, id: &str) -> Result<SourceView, MarketError> {
    let mut registry = read_sources(paths);
    let Some(position) = registry.sources.iter().position(|source| source.id == id) else {
        return Err(MarketError::not_found(format!("来源 {id} 不存在")));
    };
    let mut record = registry.sources[position].clone();
    // 刷新是用户主动触发的操作，git 失败必须报错（§4.2 第 7 条）。
    let synced = sync_if_git(paths, &mut record);
    let resolved = resolve_source(paths, &record);
    registry.sources[position].status = Some(resolved.status.to_string());
    registry.sources[position].last_error = resolved.last_error.clone();
    registry.sources[position].checked_at_unix_ms = Some(now_unix_ms());
    registry.sources[position].git_ref = record.git_ref.clone();
    let updated = registry.sources[position].clone();
    write_sources(paths, &registry)
        .map_err(|error| MarketError::new(500, format!("保存来源失败：{error}")))?;
    if let Err((status, error)) = synced {
        return Err(git_error(status, error));
    }
    Ok(source_view(&updated, &resolved))
}

pub fn market_index(paths: &MarketPaths, source_id: &str) -> Result<MarketIndexView, MarketError> {
    let registry = read_sources(paths);
    let Some(record) = registry
        .sources
        .iter()
        .find(|source| source.id == source_id)
    else {
        return Err(MarketError::not_found(format!("来源 {source_id} 不存在")));
    };
    let resolved = resolve_source(paths, record);
    let installed = read_installed(paths);
    let skills = match (resolved.root.as_deref(), resolved.index.as_ref()) {
        (Some(root), Some(index)) => index
            .skills
            .iter()
            .map(|entry| {
                let analyzed = analyze_entry(root, entry);
                let installed_brief = analyzed
                    .install_name
                    .as_deref()
                    .and_then(|name| installed.skills.get(name))
                    .map(|record| InstalledBrief {
                        version: record.version.clone(),
                        enabled: true,
                    });
                MarketSkillView {
                    name: if analyzed.raw_name.is_empty() {
                        "(未命名)".to_string()
                    } else {
                        analyzed.raw_name.clone()
                    },
                    version: analyzed.version.clone(),
                    description: analyzed.description.clone(),
                    tags: analyzed.tags.clone(),
                    installable: analyzed.reason.is_none(),
                    reason: analyzed.reason,
                    reason_detail: analyzed.detail.clone(),
                    installed: installed_brief,
                    checksum: analyzed.expected_sha256.clone(),
                }
            })
            .collect(),
        _ => Vec::new(),
    };
    Ok(MarketIndexView {
        source: MarketSourceBrief {
            id: record.id.clone(),
            kind: record.kind.clone(),
            name: record
                .name
                .clone()
                .or_else(|| resolved.index.as_ref().and_then(|index| index.name.clone())),
            status: resolved.status.to_string(),
            last_error: resolved.last_error.clone(),
            resolved_ref: None,
        },
        skills,
    })
}

fn analyze_entry(root: &Path, entry: &MarketplaceEntry) -> AnalyzedEntry {
    let raw_name = entry.name.as_deref().unwrap_or("").trim().to_string();
    let mut analyzed = AnalyzedEntry {
        raw_name: raw_name.clone(),
        install_name: None,
        version: entry
            .version
            .as_deref()
            .map(str::trim)
            .filter(|version| !version.is_empty())
            .map_or_else(|| "0.0.0+unknown".to_string(), ToString::to_string),
        description: entry.description.clone().unwrap_or_default(),
        tags: entry.tags.clone(),
        display_name: None,
        source_path: None,
        expected_sha256: normalize_sha256(entry.sha256.as_deref()),
        reason: None,
        detail: None,
    };

    if raw_name.is_empty() {
        analyzed.reason = Some("missing_name");
        analyzed.detail = Some("name".to_string());
        return analyzed;
    }
    match crate::sanitize_skill_invocation_name(&raw_name) {
        Some(sanitized) if sanitized == raw_name => analyzed.install_name = Some(sanitized),
        _ => {
            analyzed.reason = Some("name_mismatch");
            analyzed.detail = Some(format!(
                "索引名 {raw_name} 不是合法技能名（应归一为小写字母、数字、-_.）"
            ));
            return analyzed;
        }
    }

    let Some(relative) = entry
        .path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        analyzed.reason = Some("missing_path");
        analyzed.detail = Some("path".to_string());
        return analyzed;
    };

    let resolved = match fs::canonicalize(root.join(relative)) {
        Ok(path) => path,
        Err(_) => {
            analyzed.reason = Some("path_not_found");
            analyzed.detail = Some(relative.to_string());
            return analyzed;
        }
    };
    if !resolved.starts_with(root) {
        analyzed.reason = Some("path_escape");
        analyzed.detail = Some(relative.to_string());
        return analyzed;
    }
    analyzed.source_path = Some(resolved.clone());

    let prompt_path = if resolved.is_dir() {
        resolved.join("SKILL.md")
    } else if resolved
        .extension()
        .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("md"))
    {
        resolved.clone()
    } else {
        analyzed.reason = Some("missing_skill_md");
        analyzed.detail = Some(relative.to_string());
        return analyzed;
    };

    if !prompt_path.is_file() {
        analyzed.reason = Some("missing_skill_md");
        analyzed.detail = Some(relative.to_string());
        return analyzed;
    }

    match path_stats(&resolved) {
        Ok((bytes, files)) if bytes > MAX_SKILL_BYTES || files > MAX_SKILL_FILES => {
            analyzed.reason = Some("too_large");
            return analyzed;
        }
        Ok(_) => {}
        Err(error) => {
            analyzed.reason = Some("path_not_found");
            analyzed.detail = Some(format!("{relative}（{error}）"));
            return analyzed;
        }
    }

    let Ok(contents) = fs::read_to_string(&prompt_path) else {
        analyzed.reason = Some("missing_skill_md");
        analyzed.detail = Some(relative.to_string());
        return analyzed;
    };
    let (declared, description) = crate::parse_skill_frontmatter(&contents);
    if let Some(declared) = declared.as_deref() {
        let sanitized = crate::sanitize_skill_invocation_name(declared);
        if sanitized.as_deref() != analyzed.install_name.as_deref() {
            analyzed.reason = Some("name_mismatch");
            analyzed.detail = Some(format!(
                "SKILL.md 声明的名字 {declared} 与索引 {raw_name} 不一致"
            ));
            return analyzed;
        }
    }
    analyzed.display_name = declared;
    if analyzed.description.trim().is_empty() {
        analyzed.description = description.unwrap_or_default();
    }

    if let Some(expected) = &analyzed.expected_sha256 {
        match tree_digest(&resolved) {
            Ok(actual) if actual.eq_ignore_ascii_case(expected) => {}
            Ok(actual) => {
                analyzed.reason = Some("checksum_mismatch");
                analyzed.detail = Some(format!("期望 {expected}，实际 {actual}"));
                return analyzed;
            }
            Err(error) => {
                analyzed.reason = Some("path_not_found");
                analyzed.detail = Some(format!("{relative}（{error}）"));
                return analyzed;
            }
        }
    }

    if let Some(minimum) = entry
        .min_claw_version
        .as_deref()
        .map(str::trim)
        .filter(|minimum| !minimum.is_empty())
    {
        let current = env!("CARGO_PKG_VERSION");
        if !version_ge(current, minimum) {
            analyzed.reason = Some("incompatible");
            analyzed.detail = Some(format!("需要 claw ≥ {minimum}，当前 {current}"));
        }
    }

    analyzed
}

fn reason_message(analyzed: &AnalyzedEntry) -> String {
    let name = if analyzed.raw_name.is_empty() {
        "(未命名)"
    } else {
        analyzed.raw_name.as_str()
    };
    match analyzed.reason.unwrap_or_default() {
        "missing_name" => format!("索引条目 {name} 缺少 name，无法安装"),
        "missing_path" => format!("索引条目 {name} 缺少 path，无法安装"),
        "path_not_found" => format!(
            "技能路径不存在：{}",
            analyzed.detail.as_deref().unwrap_or(name)
        ),
        "path_escape" => format!(
            "技能路径越出来源目录：{}",
            analyzed.detail.as_deref().unwrap_or(name)
        ),
        "missing_skill_md" => format!(
            "{} 下没有 SKILL.md，不是有效技能目录",
            analyzed.detail.as_deref().unwrap_or(name)
        ),
        "name_mismatch" => analyzed
            .detail
            .clone()
            .unwrap_or_else(|| format!("技能名 {name} 不合法")),
        "too_large" => "技能目录过大（超过 2MB 或 200 个文件）".to_string(),
        "incompatible" => analyzed.detail.clone().unwrap_or_default(),
        "checksum_mismatch" => format!(
            "技能内容校验失败（{}）",
            analyzed.detail.as_deref().unwrap_or("校验和不匹配")
        ),
        other => format!("索引条目 {name} 不可安装（{other}）"),
    }
}

fn normalize_sha256(value: Option<&str>) -> Option<String> {
    let value = value?.trim().to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }
    if value.starts_with("sha256:") {
        Some(value)
    } else {
        Some(format!("sha256:{value}"))
    }
}

fn version_ge(current: &str, minimum: &str) -> bool {
    let left = parse_version(current);
    let right = parse_version(minimum);
    for index in 0..left.len().max(right.len()) {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    true
}

fn parse_version(value: &str) -> Vec<u64> {
    value
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

fn path_stats(path: &Path) -> io::Result<(u64, usize)> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        let mut bytes = 0_u64;
        let mut files = 0_usize;
        collect_stats(path, &mut bytes, &mut files)?;
        Ok((bytes, files))
    } else {
        Ok((metadata.len(), 1))
    }
}

fn collect_stats(dir: &Path, bytes: &mut u64, files: &mut usize) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_stats(&entry.path(), bytes, files)?;
        } else {
            *files += 1;
            *bytes += entry.metadata().map_or(0, |metadata| metadata.len());
        }
    }
    Ok(())
}

/// 树摘要：按相对路径字典序遍历文件，逐个喂 `路径字节 + 0x00 + 内容字节`。
fn tree_digest(path: &Path) -> io::Result<String> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    collect_files(path, path, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut hasher = Sha256::new();
    for (relative, absolute) in files {
        hasher.update(relative.as_bytes());
        hasher.update([0_u8]);
        hasher.update(fs::read(absolute)?);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(format!("sha256:{hex}"))
}

fn collect_files(base: &Path, current: &Path, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    if current.is_file() {
        let name = current
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        out.push((name, current.to_path_buf()));
        return Ok(());
    }
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_files(base, &path, out)?;
        } else {
            let relative = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((relative, path));
        }
    }
    Ok(())
}

fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

struct StagingGuard {
    path: PathBuf,
}

impl StagingGuard {
    fn create(base: &Path) -> io::Result<Self> {
        fs::create_dir_all(base)?;
        let path = base.join(Uuid::new_v4().to_string());
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn copy_into_staging(source: &Path, staging: &Path) -> io::Result<()> {
    if source.is_dir() {
        crate::copy_directory_contents(source, staging)
    } else {
        fs::copy(source, staging.join("SKILL.md")).map(|_| ())
    }
}

fn validate_staging(staging: &Path, name: &str) -> Result<String, MarketError> {
    let prompt = staging.join("SKILL.md");
    if !prompt.is_file() {
        return Err(MarketError::unprocessable(format!(
            "{} 下没有 SKILL.md，不是有效技能目录",
            staging.display()
        )));
    }
    match path_stats(staging) {
        Ok((bytes, files)) if bytes > MAX_SKILL_BYTES || files > MAX_SKILL_FILES => {
            return Err(MarketError::unprocessable(
                "技能目录过大（超过 2MB 或 200 个文件）",
            ));
        }
        Ok(_) => {}
        Err(error) => {
            return Err(MarketError::new(
                500,
                format!("安装失败：校验暂存目录失败（{error}）（已回滚）"),
            ));
        }
    }
    let contents = fs::read_to_string(&prompt).map_err(|error| {
        MarketError::new(
            500,
            format!("安装失败：读取 SKILL.md 失败（{error}）（已回滚）"),
        )
    })?;
    let (declared, _) = crate::parse_skill_frontmatter(&contents);
    if let Some(declared) = declared.as_deref() {
        if crate::sanitize_skill_invocation_name(declared).as_deref() != Some(name) {
            return Err(MarketError::unprocessable(format!(
                "SKILL.md 声明的名字 {declared} 与索引 {name} 不一致"
            )));
        }
    }
    let digest = tree_digest(staging).map_err(|error| {
        MarketError::new(
            500,
            format!("安装失败：计算校验和失败（{error}）（已回滚）"),
        )
    })?;
    Ok(digest)
}

/// 已解析、已校验的安装输入 —— 市场安装与 CLI `/skills install` 共用。
struct PreparedInstall {
    name: String,
    display_name: Option<String>,
    version: String,
    /// 已 canonicalize 的源路径（目录或单个 `.md`）。
    source_path: PathBuf,
    /// `Some` 时安装成功后写 `installed.json`；CLI 本地安装为 `None`。
    registry: Option<RegistryMeta>,
}

/// 只有市场安装才有：记录来源市场与来源坐标。
struct RegistryMeta {
    marketplace: String,
    source: InstalledSource,
}

/// 安装结果（两条安装路径共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    pub name: String,
    pub display_name: Option<String>,
    pub version: String,
    pub checksum: String,
    pub source: PathBuf,
    pub install_path: PathBuf,
}

/// **唯一**的原子安装流水线。
///
/// 顺序：staging → staging 校验 →（升级且目标已存在时）旧目录进 trash →
/// rename 入安装根 → 最后写注册表。任何一步失败都按逆序回滚：不留半个技能
/// 目录，也不留半条注册表记录。
fn run_install_pipeline(
    paths: &MarketPaths,
    prepared: PreparedInstall,
    upgrade: bool,
    registry_lock: &Mutex<()>,
    fault: Fault,
) -> Result<InstallOutcome, MarketError> {
    if paths.install_root.as_os_str().is_empty() {
        return Err(MarketError::new(
            500,
            "无法解析技能安装目录；请设置 CLAW_CONFIG_HOME、HOME 或 USERPROFILE",
        ));
    }
    let PreparedInstall {
        name,
        display_name,
        version,
        source_path,
        registry: registry_meta,
    } = prepared;

    let target = paths.install_root.join(&name);
    let registry = read_installed(paths);
    if target.exists() && !upgrade {
        let old_version = registry
            .skills
            .get(&name)
            .map_or_else(|| "未知版本".to_string(), |record| record.version.clone());
        return Err(MarketError::conflict(format!(
            "技能 {name} 已安装（{old_version}）；如需覆盖请使用升级安装"
        )));
    }
    if target.exists()
        && !registry
            .skills
            .get(&name)
            .is_some_and(|record| record.managed)
    {
        return Err(MarketError::conflict(format!(
            "技能 {name} 已存在且不是市场安装，无法覆盖"
        )));
    }

    fs::create_dir_all(&paths.install_root).map_err(|error| {
        MarketError::new(
            500,
            format!("安装失败：创建安装目录失败（{error}）（已回滚）"),
        )
    })?;
    let staging = StagingGuard::create(&paths.staging_dir()).map_err(|error| {
        MarketError::new(
            500,
            format!("安装失败：创建暂存目录失败（{error}）（已回滚）"),
        )
    })?;
    copy_into_staging(&source_path, staging.path()).map_err(|error| {
        MarketError::new(
            500,
            format!("安装失败：复制技能文件失败（{error}）（已回滚）"),
        )
    })?;
    if fault == Fault::ValidateAfterCopy {
        return Err(MarketError::new(
            422,
            "技能暂存校验失败（注入故障）（已回滚）",
        ));
    }
    let checksum = validate_staging(staging.path(), &name)?;

    let mut trash: Option<PathBuf> = None;
    if target.exists() {
        fs::create_dir_all(paths.trash_dir()).map_err(|error| {
            MarketError::new(
                500,
                format!("安装失败：创建回收目录失败（{error}）（已回滚）"),
            )
        })?;
        let trash_path = paths.trash_dir().join(Uuid::new_v4().to_string());
        fs::rename(&target, &trash_path).map_err(|error| {
            MarketError::new(
                500,
                format!("安装失败：移动旧目录失败（{error}），未做任何改动"),
            )
        })?;
        trash = Some(trash_path);
    }

    if fault == Fault::RenameToTarget {
        if let Some(trash_path) = &trash {
            let _ = fs::rename(trash_path, &target);
        }
        return Err(MarketError::new(
            500,
            "安装失败：重命名失败（注入故障）（已回滚）",
        ));
    }
    if let Err(error) = fs::rename(staging.path(), &target) {
        if let Some(trash_path) = &trash {
            let _ = fs::rename(trash_path, &target);
        }
        return Err(MarketError::new(
            500,
            format!("安装失败：{error}（已回滚）"),
        ));
    }

    let now = now_unix_ms();
    if let Some(meta) = registry_meta {
        let installed_at = registry
            .skills
            .get(&name)
            .map_or(now, |record| record.installed_at_unix_ms);
        let record = InstalledRecord {
            name: name.clone(),
            display_name: display_name.clone(),
            version: version.clone(),
            marketplace: meta.marketplace,
            source: meta.source,
            checksum: Some(checksum.clone()),
            installed_at_unix_ms: installed_at,
            updated_at_unix_ms: now,
            managed: true,
        };

        let write_result = {
            let _guard = registry_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut latest = read_installed(paths);
            latest.schema = installed_schema();
            latest.skills.insert(name.clone(), record.clone());
            remove_disabled(paths, &name).and_then(|()| write_installed(paths, &latest))
        };
        if let Err(error) = write_result {
            let _ = remove_any(&target);
            if let Some(trash_path) = &trash {
                let _ = fs::rename(trash_path, &target);
            }
            return Err(MarketError::new(
                500,
                format!("安装失败：{error}（已回滚）"),
            ));
        }
    }

    if let Some(trash_path) = &trash {
        let _ = remove_any(trash_path);
    }
    Ok(InstallOutcome {
        name,
        display_name,
        version,
        checksum,
        source: source_path,
        install_path: target,
    })
}

/// 安装一个本地索引条目（WebUI 市场路径）。
///
/// 负责来源/索引解析与条目校验，真正的落盘交给 [`run_install_pipeline`]。
#[allow(clippy::too_many_arguments)]
pub fn install(
    paths: &MarketPaths,
    source_id: &str,
    name: &str,
    client_version: Option<&str>,
    upgrade: bool,
    registry_lock: &Mutex<()>,
    fault: Fault,
) -> Result<InstalledRecord, MarketError> {
    if paths.install_root.as_os_str().is_empty() {
        return Err(MarketError::new(
            500,
            "无法解析技能安装目录；请设置 CLAW_CONFIG_HOME、HOME 或 USERPROFILE",
        ));
    }
    let name = crate::sanitize_skill_invocation_name(name)
        .ok_or_else(|| MarketError::bad_request("技能名不合法"))?;

    let sources = read_sources(paths);
    let source = sources
        .sources
        .iter()
        .find(|source| source.id == source_id)
        .ok_or_else(|| MarketError::not_found(format!("来源 {source_id} 不存在")))?;
    // git 来源正常在新增/刷新时已 clone；镜像丢失时补一次（30 秒超时）。
    if source.kind == "git" && !paths.mirror_dir().join(&source.id).is_dir() {
        if let Err((status, error)) = sync_git_mirror(paths, source) {
            return Err(git_error(status, error));
        }
    }
    let resolved = resolve_source(paths, source);
    let (root, index) = match (resolved.root, resolved.index) {
        (Some(root), Some(index)) => (root, index),
        _ => {
            let message = resolved
                .last_error
                .unwrap_or_else(|| "来源不可用".to_string());
            let label = source.name.as_deref().unwrap_or(&source.id);
            return Err(if resolved.status == "unsupported" {
                MarketError::unprocessable(format!("来源 {label} 不受支持：{message}"))
            } else {
                MarketError::unprocessable(format!("来源 {label} 不可用：{message}"))
            });
        }
    };
    let entry = index
        .skills
        .iter()
        .find(|entry| {
            entry
                .name
                .as_deref()
                .and_then(crate::sanitize_skill_invocation_name)
                .as_deref()
                == Some(name.as_str())
        })
        .ok_or_else(|| MarketError::not_found(format!("技能 {name} 不存在")))?;
    let analyzed = analyze_entry(&root, entry);
    if analyzed.reason.is_some() {
        return Err(MarketError::unprocessable(reason_message(&analyzed)));
    }
    if let Some(client_version) = client_version.map(str::trim).filter(|v| !v.is_empty()) {
        if client_version != analyzed.version {
            return Err(MarketError::conflict(format!(
                "索引版本已变化（{}），请刷新后重试",
                analyzed.version
            )));
        }
    }
    let source_path = analyzed
        .source_path
        .clone()
        .ok_or_else(|| MarketError::new(500, "安装失败：索引条目缺少源路径（已回滚）"))?;

    run_install_pipeline(
        paths,
        PreparedInstall {
            name: name.clone(),
            display_name: analyzed.display_name,
            version: analyzed.version,
            source_path,
            registry: Some(RegistryMeta {
                marketplace: source.name.clone().unwrap_or_else(|| source.id.clone()),
                source: InstalledSource {
                    kind: source.kind.clone(),
                    value: source.value.clone(),
                    subdir: source.subdir.clone(),
                    resolved_ref: if source.kind == "git" {
                        git_short_ref(&paths.mirror_dir().join(&source.id))
                    } else {
                        None
                    },
                },
            }),
        },
        upgrade,
        registry_lock,
        fault,
    )?;

    read_installed(paths)
        .skills
        .get(&name)
        .cloned()
        .ok_or_else(|| MarketError::new(500, "安装成功但注册表缺少记录"))
}

/// `/skills install <path>`：本地目录或单个 `.md` 也走**同一条**原子流水线。
///
/// 与市场安装的差别只有两点：没有索引来源；不写 `installed.json`（因此 WebUI
/// 不展示市场版本/来源）。副本仍在安装根内，WebUI 按路径判定可卸载。
pub fn install_local(
    paths: &MarketPaths,
    source: &str,
    cwd: &Path,
    upgrade: bool,
    registry_lock: &Mutex<()>,
    fault: Fault,
) -> Result<InstallOutcome, MarketError> {
    if paths.install_root.as_os_str().is_empty() {
        return Err(MarketError::new(
            500,
            "无法解析技能安装目录；请设置 CLAW_CONFIG_HOME、HOME 或 USERPROFILE",
        ));
    }
    let resolved = resolve_local_source(source, cwd)?;
    let root = resolved
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| resolved.clone());
    let relative = resolved
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| MarketError::bad_request("技能源路径不合法"))?;
    let name = derive_local_install_name(&resolved)?;
    let entry = MarketplaceEntry {
        name: Some(name),
        version: None,
        path: Some(relative),
        description: None,
        tags: Vec::new(),
        sha256: None,
        min_claw_version: None,
    };
    let analyzed = analyze_entry(&root, &entry);
    if analyzed.reason.is_some() {
        return Err(MarketError::unprocessable(reason_message(&analyzed)));
    }
    let source_path = analyzed
        .source_path
        .clone()
        .ok_or_else(|| MarketError::new(500, "安装失败：源路径缺失（已回滚）"))?;

    run_install_pipeline(
        paths,
        PreparedInstall {
            name: analyzed.install_name.unwrap_or_default(),
            display_name: analyzed.display_name,
            version: analyzed.version,
            source_path,
            registry: None,
        },
        upgrade,
        registry_lock,
        fault,
    )
}

/// 把 `/skills install` 的入参解析成已 canonicalize 的源路径。
fn resolve_local_source(source: &str, cwd: &Path) -> Result<PathBuf, MarketError> {
    let candidate = PathBuf::from(source);
    let source = if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(candidate)
    };
    let source = fs::canonicalize(&source)
        .map_err(|error| MarketError::bad_request(format!("技能源不可读：{error}")))?;
    if source.is_dir() {
        if !source.join("SKILL.md").is_file() {
            return Err(MarketError::bad_request(format!(
                "技能目录 '{}' 必须包含 SKILL.md",
                source.display()
            )));
        }
        return Ok(source);
    }
    if source
        .extension()
        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("md"))
    {
        return Ok(source);
    }
    Err(MarketError::bad_request(format!(
        "技能源 '{}' 必须是含 SKILL.md 的目录或 markdown 文件",
        source.display()
    )))
}

/// 本地安装的技能名：SKILL.md 声明名优先，否则目录名 / 文件名。
fn derive_local_install_name(resolved: &Path) -> Result<String, MarketError> {
    let (prompt, fallback) = if resolved.is_dir() {
        (
            resolved.join("SKILL.md"),
            resolved
                .file_name()
                .map(|n| n.to_string_lossy().to_string()),
        )
    } else {
        (
            resolved.to_path_buf(),
            resolved
                .file_stem()
                .map(|n| n.to_string_lossy().to_string()),
        )
    };
    if let Ok(contents) = fs::read_to_string(&prompt) {
        let (declared, _) = crate::parse_skill_frontmatter(&contents);
        if let Some(name) = declared
            .as_deref()
            .and_then(crate::sanitize_skill_invocation_name)
        {
            return Ok(name);
        }
    }
    fallback
        .as_deref()
        .and_then(crate::sanitize_skill_invocation_name)
        .ok_or_else(|| {
            MarketError::bad_request(format!("无法从 '{}' 推导技能名", resolved.display()))
        })
}

/// 卸载安装根内的技能副本：目录先进 trash，注册表更新成功后才真正删除。
///
/// 判定只看「副本是否落在安装根」，不依赖 `installed.json`：命令行
/// `/skills install` 与市场安装写入同一个根，前者没有市场元数据，
/// 但同样必须能从 WebUI 卸载。项目级/内置技能不在安装根内，仍只能禁用。
/// 注册表有记录时联动删除并清理 `state.json`；没有记录时只清禁用状态，
/// 不凭空创建 `installed.json`。
pub fn uninstall(
    paths: &MarketPaths,
    name: &str,
    registry_lock: &Mutex<()>,
) -> Result<(), MarketError> {
    if paths.install_root.as_os_str().is_empty() {
        return Err(MarketError::new(
            500,
            "无法解析技能安装目录；请设置 CLAW_CONFIG_HOME、HOME 或 USERPROFILE",
        ));
    }
    let name = crate::sanitize_skill_invocation_name(name)
        .ok_or_else(|| MarketError::bad_request("技能名不合法"))?;
    let registry = read_installed(paths);
    let record = registry.skills.get(&name);
    if record.is_some_and(|record| !record.managed) {
        return Err(MarketError::conflict(
            "该技能记录为「非市场安装」，不能卸载；项目或内置技能请用「禁用」",
        ));
    }

    let target = paths.install_root.join(&name);
    if record.is_none() && !target.exists() {
        return Err(MarketError::not_found(format!("技能 {name} 不存在")));
    }
    fs::create_dir_all(paths.trash_dir())
        .map_err(|error| MarketError::new(500, format!("卸载失败：{error}")))?;
    let trash_path = paths.trash_dir().join(Uuid::new_v4().to_string());
    let moved = if target.exists() {
        fs::rename(&target, &trash_path)
            .map_err(|error| MarketError::new(500, format!("卸载失败：{error}")))?;
        true
    } else {
        false
    };

    let had_record = record.is_some();
    let write_result = {
        let _guard = registry_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if had_record {
            let mut latest = read_installed(paths);
            latest.skills.remove(&name);
            remove_disabled(paths, &name).and_then(|()| write_installed(paths, &latest))
        } else {
            remove_disabled(paths, &name)
        }
    };
    if let Err(error) = write_result {
        if moved {
            let _ = fs::rename(&trash_path, &target);
        }
        return Err(MarketError::new(
            500,
            format!("卸载失败：{error}（已回滚）"),
        ));
    }

    if moved {
        let _ = remove_any(&trash_path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Barrier;
    use tempfile::TempDir;

    /// 测试内临时改写 `CLAW_CONFIG_HOME`，Drop 时恢复，避免污染并行测试。
    struct EnvGuard(Option<std::ffi::OsString>);

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
                None => std::env::remove_var("CLAW_CONFIG_HOME"),
            }
        }
    }

    /// 同一进程内改写 `CLAW_CONFIG_HOME` 的用例必须串行，否则并行时互相
    /// 覆盖对方的环境变量，导致技能发现指向别人的临时目录。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct Fixture {
        _tmp: TempDir,
        paths: MarketPaths,
        source_root: PathBuf,
        workspace: PathBuf,
    }

    fn write_skill(root: &Path, name: &str, description: &str, declared: Option<&str>) {
        let dir = root.join("skills").join(name);
        fs::create_dir_all(&dir).expect("create skill dir");
        let declared_line = declared.map_or_else(String::new, |value| format!("name: {value}\n"));
        fs::write(
            dir.join("SKILL.md"),
            format!("---\n{declared_line}description: {description}\n---\n\n# {name}\n"),
        )
        .expect("write SKILL.md");
    }

    fn write_index(root: &Path, entries: Value) {
        fs::create_dir_all(root).expect("create market root");
        fs::write(
            root.join("marketplace.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": MARKET_SCHEMA,
                "name": "test-market",
                "description": "P1 测试索引",
                "skills": entries,
            }))
            .expect("encode index"),
        )
        .expect("write marketplace.json");
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = MarketPaths::with_roots(
            tmp.path().join("config").join("skill-market"),
            tmp.path().join("config").join("skills"),
        );
        let source_root = tmp.path().join("market");
        write_skill(&source_root, "demo", "P1 演示技能", Some("demo"));
        write_skill(&source_root, "second", "第二个技能", Some("second"));
        write_index(
            &source_root,
            json!([
                { "name": "demo", "version": "1.2.0", "path": "skills/demo", "tags": ["demo"] },
                { "name": "second", "version": "0.1.0", "path": "skills/second" },
            ]),
        );
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        Fixture {
            _tmp: tmp,
            paths,
            source_root,
            workspace,
        }
    }

    fn source_id(fixture: &Fixture) -> String {
        add_source(
            &fixture.paths,
            "local",
            fixture.source_root.to_str().expect("source root utf8"),
            Some("test-market"),
            None,
        )
        .expect("add source")
        .id
    }

    fn install_demo(
        fixture: &Fixture,
        upgrade: bool,
        fault: Fault,
    ) -> Result<InstalledRecord, MarketError> {
        let id = source_id(fixture);
        install(
            &fixture.paths,
            &id,
            "demo",
            None,
            upgrade,
            &Mutex::new(()),
            fault,
        )
    }

    fn staging_is_empty(paths: &MarketPaths) -> bool {
        fs::read_dir(paths.staging_dir())
            .map(|entries| entries.count() == 0)
            .unwrap_or(true)
    }

    fn trash_is_empty(paths: &MarketPaths) -> bool {
        fs::read_dir(paths.trash_dir())
            .map(|entries| entries.count() == 0)
            .unwrap_or(true)
    }

    #[test]
    fn install_local_is_atomic_and_registers_once() {
        let fixture = fixture();
        let record = install_demo(&fixture, false, Fault::None).expect("install demo");
        assert_eq!(record.version, "1.2.0");
        assert!(fixture
            .paths
            .install_root
            .join("demo")
            .join("SKILL.md")
            .is_file());
        assert!(staging_is_empty(&fixture.paths), "staging 必须清空");

        let registry = read_installed(&fixture.paths);
        assert_eq!(registry.skills.len(), 1);
        assert_eq!(registry.skills["demo"].version, "1.2.0");
        assert!(registry.skills["demo"].managed);

        let mtime_before = fs::metadata(fixture.paths.install_root.join("demo"))
            .and_then(|meta| meta.modified())
            .expect("mtime");
        let error = install_demo(&fixture, false, Fault::None).expect_err("duplicate must fail");
        assert_eq!(error.status, 409);
        assert!(error.message.contains("已安装"));
        let mtime_after = fs::metadata(fixture.paths.install_root.join("demo"))
            .and_then(|meta| meta.modified())
            .expect("mtime");
        assert_eq!(mtime_before, mtime_after, "重复安装不能改动目录");
    }

    #[test]
    fn staging_validation_failure_rolls_back_new_and_upgrade() {
        let fixture = fixture();
        let error = install_demo(&fixture, false, Fault::ValidateAfterCopy)
            .expect_err("validation fault must fail");
        assert_eq!(error.status, 422);
        assert!(!fixture.paths.install_root.join("demo").exists());
        assert!(staging_is_empty(&fixture.paths));
        assert!(read_installed(&fixture.paths).skills.is_empty());

        install_demo(&fixture, false, Fault::None).expect("initial install");
        let old_contents =
            fs::read_to_string(fixture.paths.install_root.join("demo").join("SKILL.md"))
                .expect("read installed");

        write_skill(&fixture.source_root, "demo", "升级后的描述", Some("demo"));
        let error = install_demo(&fixture, true, Fault::ValidateAfterCopy)
            .expect_err("upgrade validation fault must fail");
        assert_eq!(error.status, 422);
        let restored = fs::read_to_string(fixture.paths.install_root.join("demo").join("SKILL.md"))
            .expect("old skill restored");
        assert_eq!(old_contents, restored, "升级失败必须恢复旧目录");
        assert!(staging_is_empty(&fixture.paths));
        assert!(trash_is_empty(&fixture.paths));
        assert_eq!(
            read_installed(&fixture.paths).skills["demo"].version,
            "1.2.0"
        );
    }

    #[test]
    fn rename_failure_rolls_back_new_and_upgrade() {
        let fixture = fixture();
        let error = install_demo(&fixture, false, Fault::RenameToTarget)
            .expect_err("rename fault must fail");
        assert_eq!(error.status, 500);
        assert!(!fixture.paths.install_root.join("demo").exists());
        assert!(staging_is_empty(&fixture.paths));
        assert!(trash_is_empty(&fixture.paths));
        assert!(read_installed(&fixture.paths).skills.is_empty());

        install_demo(&fixture, false, Fault::None).expect("initial install");
        let old_contents =
            fs::read_to_string(fixture.paths.install_root.join("demo").join("SKILL.md"))
                .expect("read installed");
        write_skill(&fixture.source_root, "demo", "升级后的描述", Some("demo"));
        let error =
            install_demo(&fixture, true, Fault::RenameToTarget).expect_err("upgrade rename fault");
        assert_eq!(error.status, 500);
        let restored = fs::read_to_string(fixture.paths.install_root.join("demo").join("SKILL.md"))
            .expect("old skill restored");
        assert_eq!(old_contents, restored, "rename 失败必须恢复旧目录");
        assert!(staging_is_empty(&fixture.paths));
        assert!(trash_is_empty(&fixture.paths));
    }

    #[test]
    fn concurrent_same_name_install_has_one_winner() {
        let fixture = fixture();
        let id = source_id(&fixture);
        let paths = Arc::new(fixture.paths.clone());
        let locks = Arc::new(Mutex::new(BTreeSet::new()));
        let registry_lock = Arc::new(Mutex::new(()));
        let barrier = Arc::new(Barrier::new(2));

        let mut handles = Vec::new();
        for _ in 0..2 {
            let paths = Arc::clone(&paths);
            let locks = Arc::clone(&locks);
            let registry_lock = Arc::clone(&registry_lock);
            let barrier = Arc::clone(&barrier);
            let id = id.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let lock = match NameLock::acquire(&locks, "demo", "技能 demo 正在安装，请稍候")
                {
                    Ok(lock) => lock,
                    Err(error) => return Err(error),
                };
                let result = install(
                    &paths,
                    &id,
                    "demo",
                    None,
                    false,
                    &registry_lock,
                    Fault::None,
                );
                drop(lock);
                result
            }));
        }
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("join"))
            .collect();
        let successes = results.iter().filter(|result| result.is_ok()).count();
        let conflicts: Vec<_> = results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .collect();
        assert_eq!(successes, 1, "只能有一个安装成功：{results:?}");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].status, 409);
        assert_eq!(read_installed(&paths).skills.len(), 1);
        assert!(paths.install_root.join("demo").is_dir());
    }

    #[test]
    fn uninstall_removes_dir_registry_and_prompt_catalog() {
        let fixture = fixture();
        write_skill(
            &fixture.source_root,
            "aaa-demo",
            "prompt 验证技能",
            Some("aaa-demo"),
        );
        write_index(
            &fixture.source_root,
            json!([
                { "name": "aaa-demo", "version": "1.0.0", "path": "skills/aaa-demo" },
            ]),
        );
        let previous_config_home = std::env::var_os("CLAW_CONFIG_HOME");
        let _env_lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config_home = fixture.paths.install_root.parent().expect("config home");
        std::env::set_var("CLAW_CONFIG_HOME", config_home);
        let env_guard = EnvGuard(previous_config_home);
        let id = source_id(&fixture);
        install(
            &fixture.paths,
            &id,
            "aaa-demo",
            None,
            false,
            &Mutex::new(()),
            Fault::None,
        )
        .expect("install aaa-demo");

        let before = crate::list_skill_summaries(&fixture.workspace).expect("summaries");
        assert!(
            before.iter().any(|skill| skill.name() == "aaa-demo"),
            "安装后技能列表应包含该技能"
        );
        let fingerprint_before = crate::skill_catalog_fingerprint(&fixture.workspace);

        uninstall(&fixture.paths, "aaa-demo", &Mutex::new(())).expect("uninstall");
        assert!(!fixture.paths.install_root.join("aaa-demo").exists());
        assert!(read_installed(&fixture.paths).skills.is_empty());
        assert!(trash_is_empty(&fixture.paths));

        let after = crate::list_skill_summaries(&fixture.workspace).expect("summaries");
        assert!(
            !after.iter().any(|skill| skill.name() == "aaa-demo"),
            "卸载后技能列表不应再包含该技能"
        );
        let fingerprint_after = crate::skill_catalog_fingerprint(&fixture.workspace);
        assert_ne!(
            fingerprint_before, fingerprint_after,
            "卸载后技能目录指纹必须变化，否则下一次 prompt 构建会命中旧缓存"
        );
        drop(env_guard);
    }

    #[test]
    fn path_escape_and_name_mismatch_are_rejected_without_disk_writes() {
        let fixture = fixture();
        fs::write(
            fixture._tmp.path().join("outside.md"),
            "---\nname: outside\ndescription: x\n---\n",
        )
        .expect("write outside");
        write_index(
            &fixture.source_root,
            json!([
                { "name": "escape", "version": "1.0.0", "path": "../outside.md" },
                { "name": "mismatch", "version": "1.0.0", "path": "skills/demo" },
            ]),
        );
        let id = source_id(&fixture);
        let escape = install(
            &fixture.paths,
            &id,
            "escape",
            None,
            false,
            &Mutex::new(()),
            Fault::None,
        )
        .expect_err("path escape must fail");
        assert_eq!(escape.status, 422);
        assert!(escape.message.contains("越出来源目录"));

        let mismatch = install(
            &fixture.paths,
            &id,
            "mismatch",
            None,
            false,
            &Mutex::new(()),
            Fault::None,
        )
        .expect_err("name mismatch must fail");
        assert_eq!(mismatch.status, 422);
        assert!(mismatch.message.contains("不一致"));

        assert!(!fixture.paths.install_root.join("escape").exists());
        assert!(!fixture.paths.install_root.join("mismatch").exists());
        assert!(staging_is_empty(&fixture.paths));
        assert!(read_installed(&fixture.paths).skills.is_empty());
    }

    #[test]
    fn checksum_mismatch_blocks_install() {
        let fixture = fixture();
        write_index(
            &fixture.source_root,
            json!([
                { "name": "demo", "version": "1.2.0", "path": "skills/demo", "sha256": "deadbeef" },
            ]),
        );
        let id = source_id(&fixture);
        let error = install(
            &fixture.paths,
            &id,
            "demo",
            None,
            false,
            &Mutex::new(()),
            Fault::None,
        )
        .expect_err("checksum mismatch must fail");
        assert_eq!(error.status, 422);
        assert!(error.message.contains("校验失败"));
        assert!(!fixture.paths.install_root.join("demo").exists());
    }

    #[test]
    fn sources_can_be_listed_refreshed_and_deleted() {
        let fixture = fixture();
        let id = source_id(&fixture);
        let sources = list_sources(&fixture.paths);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].status, "ok");
        assert_eq!(sources[0].skill_count, 2);

        let view = market_index(&fixture.paths, &id).expect("market index");
        assert_eq!(view.source.name.as_deref(), Some("test-market"));
        assert_eq!(view.skills.len(), 2);
        assert!(view.skills.iter().all(|skill| skill.installable));

        let refreshed = refresh_source(&fixture.paths, &id).expect("refresh");
        assert_eq!(refreshed.status, "ok");
        delete_source(&fixture.paths, &id).expect("delete");
        assert!(list_sources(&fixture.paths).is_empty());
        let missing = delete_source(&fixture.paths, &id).expect_err("delete twice");
        assert_eq!(missing.status, 404);
    }

    /// 命令行安装不写 `installed.json`，但落在安装根内，WebUI 必须能卸载。
    #[test]
    fn uninstall_removes_unregistered_root_skill() {
        let fixture = fixture();
        let source = fixture.source_root.join("skills").join("demo");
        install_local(
            &fixture.paths,
            source.to_str().expect("source utf8"),
            &fixture.workspace,
            false,
            &Mutex::new(()),
            Fault::None,
        )
        .expect("local install");
        assert!(read_installed(&fixture.paths).skills.is_empty());
        let target = fixture.paths.install_root.join("demo");
        assert!(target.is_dir());

        uninstall(&fixture.paths, "demo", &Mutex::new(())).expect("uninstall unregistered");
        assert!(!target.exists(), "目录必须被删除");
        assert!(staging_is_empty(&fixture.paths));
        assert!(trash_is_empty(&fixture.paths));
    }

    /// 注册表明确标了 `managed: false` 的记录仍然不可卸载。
    #[test]
    fn uninstall_rejects_non_managed_record() {
        let fixture = fixture();
        let target = fixture.paths.install_root.join("manual");
        fs::create_dir_all(&target).expect("manual skill");
        fs::write(
            target.join("SKILL.md"),
            "---\nname: manual\ndescription: x\n---\n",
        )
        .expect("write manual");
        let mut registry = InstalledRegistry::default();
        registry.skills.insert(
            "manual".to_string(),
            InstalledRecord {
                name: "manual".to_string(),
                display_name: None,
                version: "0.0.0+unknown".to_string(),
                marketplace: String::new(),
                source: InstalledSource {
                    kind: "local".to_string(),
                    value: String::new(),
                    subdir: None,
                    resolved_ref: None,
                },
                checksum: None,
                installed_at_unix_ms: 0,
                updated_at_unix_ms: 0,
                managed: false,
            },
        );
        write_installed(&fixture.paths, &registry).expect("write registry");

        let error = uninstall(&fixture.paths, "manual", &Mutex::new(()))
            .expect_err("non-managed record must be rejected");
        assert_eq!(error.status, 409);
        assert!(target.is_dir());
    }

    /// 目录快照：相对路径 + 大小 + 修改时间。启停前后必须完全一致。
    fn snapshot_tree(root: &Path) -> Vec<(String, u64, u128)> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64, u128)>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(root, &path, out);
                    continue;
                }
                let metadata = entry.metadata().ok();
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                let len = metadata.as_ref().map_or(0, std::fs::Metadata::len);
                let mtime = metadata
                    .and_then(|meta| meta.modified().ok())
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |duration| duration.as_millis());
                out.push((relative, len, mtime));
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    /// P2 判据：开关只写 `state.json`，列表口径随之变化，技能目录一个字节都不动；
    /// 新开一次 `read_state`（等价重启 WebUI）仍保持禁用。
    #[test]
    fn toggle_filters_lists_without_touching_skill_dir() {
        let fixture = fixture();
        let config_home = fixture
            .paths
            .install_root
            .parent()
            .expect("config home")
            .to_path_buf();
        let previous = std::env::var_os("CLAW_CONFIG_HOME");
        let _env_lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("CLAW_CONFIG_HOME", &config_home);
        let _guard = EnvGuard(previous);

        install_demo(&fixture, false, Fault::None).expect("install demo");
        let skill_dir = fixture.paths.install_root.join("demo");
        let before = snapshot_tree(&skill_dir);

        let visible = crate::list_skill_summaries_filtered(&fixture.workspace, &BTreeSet::new())
            .expect("summaries");
        assert!(visible.iter().any(|skill| skill.name() == "demo"));

        let disabled = set_enabled(&fixture.paths, "demo", false, &fixture.workspace);
        assert!(
            disabled.is_ok(),
            "禁用失败：{:?}",
            disabled.err().map(|error| (error.status, error.message))
        );
        // 重新读盘 = WebUI 重启后的状态。
        let persisted = read_state(&fixture.paths);
        assert!(persisted.disabled.contains("demo"), "禁用状态必须落盘");

        let hidden = crate::list_skill_summaries_filtered(&fixture.workspace, &persisted.disabled)
            .expect("summaries");
        assert!(
            !hidden.iter().any(|skill| skill.name() == "demo"),
            "禁用后列表不应再出现该技能"
        );
        assert_eq!(snapshot_tree(&skill_dir), before, "启停不得修改技能目录");

        assert!(set_enabled(&fixture.paths, "demo", true, &fixture.workspace).expect("enable"));
        let restored = crate::list_skill_summaries_filtered(
            &fixture.workspace,
            &read_state(&fixture.paths).disabled,
        )
        .expect("summaries");
        assert!(restored.iter().any(|skill| skill.name() == "demo"));
        assert_eq!(snapshot_tree(&skill_dir), before, "启停不得修改技能目录");
    }

    #[test]
    fn toggle_unknown_skill_is_not_found() {
        let fixture = fixture();
        let error = set_enabled(&fixture.paths, "no-such-skill", false, &fixture.workspace)
            .expect_err("unknown skill must fail");
        assert_eq!(error.status, 404);
        assert!(read_state(&fixture.paths).disabled.is_empty());
    }
}
