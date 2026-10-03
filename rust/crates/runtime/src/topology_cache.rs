//! Topology 磁盘缓存 — 跨会话复用 cargo metadata + 符号索引结果。
//!
//! # 设计
//!
//! - 缓存文件按语言隔离：`.claw/topology_rust.json` / `.claw/topology_python.json`。
//! - 命中必须通过 [`SourceFingerprint`] 指纹校验：清单文件 mtime、主语言源文件
//!   mtime 哈希、git HEAD、工作区 dirty 状态，任一变化 → 缓存作废重建，
//!   杜绝"启动快但数据旧"误导 LLM（含非 git 仓库场景）。
//! - 写盘采用跨平台策略：tmp + rename，Windows 上 rename 无法覆盖目标时
//!   降级为 remove 目标再 rename；失败仅降级，不阻断（best-effort）。
//! - 逃生阀：`CLAW_DISABLE_CACHE`（全禁）/ `CLAW_DISABLE_TOPOLOGY_CACHE` /
//!   `CLAW_DISABLE_REPOMAP_CACHE`（分项），值 `1` 即禁用。
//!
//! 本模块同时承载 RepoMap 磁盘缓存复用的通用工具（逃生阀、大小守卫、root
//! 规范化、超时子进程执行），避免在 repomap.rs 重复实现。

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::project_topology::TopologyData;

/// 缓存 schema 版本。序列化语义变化时递增，旧缓存作废重建。
pub const TOPOLOGY_CACHE_SCHEMA_VERSION: u32 = 1;
/// 默认缓存大小上限（50MB）。巨型 monorepo 可经 `CLAW_CACHE_MAX_BYTES` 调高。
const DEFAULT_CACHE_MAX_BYTES: u64 = 50 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Language 探测
// ---------------------------------------------------------------------------

/// 支持的项目语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    Rust,
    Python,
}

impl Language {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
        }
    }

    /// 该语言对应的源文件扩展名（无点号）。
    fn source_ext(self) -> &'static str {
        match self {
            Language::Rust => "rs",
            Language::Python => "py",
        }
    }

    /// 该语言在 grep 时的 `--type` 参数。
    #[must_use]
    pub fn grep_type(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "py",
        }
    }
}

/// 探测项目包含的语言（可多个）。Cargo.toml → Rust；Python 标记文件 → Python。
#[must_use]
pub fn detect_languages(root: &Path) -> Vec<Language> {
    let mut langs = Vec::new();
    if root.join("Cargo.toml").exists() {
        langs.push(Language::Rust);
    }
    if has_python_marker(root) {
        langs.push(Language::Python);
    }
    langs
}

/// 判断根 Cargo.toml 是否为 workspace 根：存在"去行首空白后以 `[workspace]`
/// 开头"的行（行首匹配，不用 `contains`，避免注释/字符串/`[workspace.metadata]`
/// 等子段误判）。
fn is_workspace_root_manifest(root: &Path) -> bool {
    let content = match fs::read_to_string(root.join("Cargo.toml")) {
        Ok(c) => c,
        Err(_) => return false,
    };
    content.lines().any(|line| {
        let t = line.trim_start();
        t.strip_prefix("[workspace]")
            .is_some_and(|rest| !rest.trim_start().starts_with('.'))
    })
}

fn has_python_marker(root: &Path) -> bool {
    const PY_MARKERS: [&str; 4] = [
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "requirements.txt",
    ];
    PY_MARKERS.iter().any(|m| root.join(m).exists())
}

/// 选定主语言（混合项目策略，防 Python 主项目被 Rust 子 crate 覆盖）：
/// 1. `CLAW_PRIMARY_LANGUAGE`（rust/python）显式覆盖；
/// 2. 根 Cargo.toml 存在且为 workspace 根 → Rust；
/// 3. 否则 Python 标记文件存在 → Python；
/// 4. 否则 `detect_languages` 第一个。
///
/// 闭环说明：单 crate Rust 项目（根 Cargo.toml 无 `[workspace]` 段）不命中规则
/// 2、无 Python 标记也不命中规则 3，落入规则 4——`detect_languages` 因
/// Cargo.toml 存在而含 Rust 且排在首位，仍判 Rust。无歧义。
#[must_use]
pub fn pick_primary_language(root: &Path) -> Option<Language> {
    pick_primary_language_with(root, std::env::var("CLAW_PRIMARY_LANGUAGE").ok().as_deref())
}

/// `pick_primary_language` 的纯函数内核：`override_lang` 为环境变量值（可注入），
/// 便于确定性单测覆盖 `CLAW_PRIMARY_LANGUAGE` 分支（env 全局态无法安全并发测试）。
#[must_use]
pub fn pick_primary_language_with(root: &Path, override_lang: Option<&str>) -> Option<Language> {
    if let Some(v) = override_lang {
        match v.to_ascii_lowercase().as_str() {
            "rust" => return Some(Language::Rust),
            "python" => return Some(Language::Python),
            _ => {}
        }
    }
    if root.join("Cargo.toml").exists() && is_workspace_root_manifest(root) {
        return Some(Language::Rust);
    }
    if has_python_marker(root) {
        return Some(Language::Python);
    }
    detect_languages(root).into_iter().next()
}

// ---------------------------------------------------------------------------
// SourceFingerprint
// ---------------------------------------------------------------------------

/// 缓存失效指纹：构建时记录、加载时重算，任一字段不符 → 缓存作废。
///
/// `git_dirty_hash` 单独存放：它依赖 `git status`（可能较慢），加载时仅当
/// 结构部分全部命中后才计算比较，避免每次启动都执行 git status。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFingerprint {
    /// 清单文件（Cargo.toml 等）相对路径 + mtime 毫秒。
    pub manifest_mtimes: Vec<(String, u64)>,
    /// 主语言源文件 `(relpath, mtime)` 排序后的稳定哈希。
    pub source_mtime_hash: u64,
    /// git HEAD commit（非 git 仓库为 None）。
    pub git_head: Option<String>,
    /// 工作区 dirty 状态排序哈希（非 git / 超时为 None）。
    pub git_dirty_hash: Option<u64>,
}

impl SourceFingerprint {
    /// 完整指纹（构建后保存用）：结构部分 + dirty 部分。
    #[must_use]
    pub fn compute(root: &Path, language: Language) -> Self {
        let mut fp = Self::compute_structural(root, language);
        fp.git_dirty_hash = compute_git_dirty_hash(root);
        fp
    }

    /// 仅结构部分（加载时先比较）：清单 mtime + 源文件 mtime 哈希 + git HEAD。
    /// 不含 dirty（其计算成本较高，仅在结构命中后再比较）。
    #[must_use]
    pub fn compute_structural(root: &Path, language: Language) -> Self {
        Self {
            manifest_mtimes: collect_manifest_mtimes(root),
            source_mtime_hash: hash_source_mtimes(root, language),
            git_head: git_head_hash(root),
            git_dirty_hash: None,
        }
    }
}

/// mtime 转毫秒（SystemTime → ms；失败记 0）。
fn mtime_ms(p: &Path) -> u64 {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn collect_manifest_mtimes(root: &Path) -> Vec<(String, u64)> {
    const MANIFESTS: [&str; 6] = [
        "Cargo.toml",
        "Cargo.lock",
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "requirements.txt",
    ];
    MANIFESTS
        .iter()
        .filter_map(|name| {
            let p = root.join(name);
            if !p.exists() {
                return None;
            }
            Some(((*name).to_string(), mtime_ms(&p)))
        })
        .collect()
}

/// 主语言源文件 `(relpath, mtime)` 排序后哈希。
/// 只枚举不读内容（ms 级）。按语言过滤，避免混合项目里其他语言改动
/// 导致本语言缓存无谓重建。
/// 注：部分文件系统 mtime 仅秒级精度，快速连续修改可能同 mtime，best-effort 可接受。
fn hash_source_mtimes(root: &Path, language: Language) -> u64 {
    let ext = language.source_ext();
    let mut entries: Vec<(String, u64)> = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            if e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    "target" | "node_modules" | "__pycache__" | "venv" | ".venv"
                ) && !name.starts_with('.')
            } else {
                true
            }
        })
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        if p.extension().and_then(|e| e.to_str()) != Some(ext) {
            continue;
        }
        let rel = p
            .strip_prefix(root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        entries.push((rel, mtime_ms(p)));
    }
    entries.sort_unstable();
    let mut hasher = DefaultHasher::new();
    entries.hash(&mut hasher);
    hasher.finish()
}

fn git_head_hash(root: &Path) -> Option<String> {
    // 优先 git rev-parse HEAD；git 不存在/失败时读 .git/HEAD；再失败记 None。
    if let Some(out) = run_git(root, &["rev-parse", "HEAD"], Duration::from_secs(2)) {
        let s = out.trim();
        if !s.is_empty() && s.len() == 40 {
            return Some(s.to_string());
        }
    }
    let head_file = root.join(".git").join("HEAD");
    let content = fs::read_to_string(head_file).ok()?;
    let s = content.trim();
    if let Some(hash) = s.strip_prefix("ref: ") {
        let ref_path = root.join(".git").join(hash);
        fs::read_to_string(ref_path)
            .ok()
            .map(|c| c.trim().to_string())
    } else if s.len() == 40 {
        Some(s.to_string())
    } else {
        None
    }
}

/// 工作区 dirty 状态：`git status --porcelain` 输出排序后哈希。
/// 非 git 仓库或命令超时 → None。
fn compute_git_dirty_hash(root: &Path) -> Option<u64> {
    let out = run_git(root, &["status", "--porcelain"], Duration::from_millis(500))?;
    let mut lines: Vec<&str> = out.lines().collect();
    lines.sort_unstable();
    let mut hasher = DefaultHasher::new();
    lines.join("\n").hash(&mut hasher);
    Some(hasher.finish())
}

// ---------------------------------------------------------------------------
// 缓存读写
// ---------------------------------------------------------------------------

/// 缓存文件结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyCache {
    pub schema_version: u32,
    /// "rust" / "python"。
    pub language: String,
    /// 规范化后的 workspace root。
    pub root: String,
    pub fingerprint: SourceFingerprint,
    pub data: TopologyData,
}

/// 按语言隔离的缓存文件路径：`<root>/.claw/topology_<language>.json`。
#[must_use]
pub fn topology_cache_path(root: &Path, language: Language) -> PathBuf {
    root.join(".claw")
        .join(format!("topology_{}.json", language.as_str()))
}

/// 加载缓存。任一校验（存在/schema/root/语言/指纹）不通过 → `None`（重建）。
/// 指纹比较分两步：先结构部分（便宜），命中后再比较 dirty（`git status` 慢）。
/// 超限策略判定：是否需要对该缓存 kind 施加 `CLAW_CACHE_MAX_BYTES` 网关。
///
/// 策略为 **Topology 优先持久化，RepoMap 放弃**：Topology 更小、查询价值更高，
/// 豁免大小网关（读/写均不受限）；RepoMap 双向受限，超限即放弃（不写、不读）。
#[must_use]
fn size_gate_applies(kind: &str) -> bool {
    kind == "repomap"
}

#[must_use]
fn within_limit_with(kind: &str, size: u64, max: u64) -> bool {
    if size_gate_applies(kind) {
        size <= max
    } else {
        true
    }
}

/// 给定缓存 `kind` 与字节数是否在缓存上限内（Topology 恒为 `true`）。
#[must_use]
pub fn cache_within_limit(kind: &str, size: u64) -> bool {
    within_limit_with(kind, size, cache_max_bytes())
}

pub fn load_cache(root: &Path, language: Language) -> Option<TopologyCache> {
    if cache_disabled("topology") {
        return None;
    }
    let root_norm = normalize_root(root);
    let path = topology_cache_path(&root_norm, language);
    let Ok(meta) = fs::metadata(&path) else {
        return None;
    };
    if !cache_within_limit("topology", meta.len()) {
        return None;
    }
    let content = fs::read_to_string(&path).ok()?;
    let cached: TopologyCache = serde_json::from_str(&content).ok()?;
    if cached.schema_version != TOPOLOGY_CACHE_SCHEMA_VERSION {
        return None;
    }
    if cached.language != language.as_str() {
        return None;
    }
    if cached.root != root_norm.to_string_lossy() {
        return None;
    }
    let structural = SourceFingerprint::compute_structural(&root_norm, language);
    if structural.manifest_mtimes != cached.fingerprint.manifest_mtimes
        || structural.source_mtime_hash != cached.fingerprint.source_mtime_hash
        || structural.git_head != cached.fingerprint.git_head
    {
        return None;
    }
    // 仅当缓存里记录了 dirty 哈希时才比较（避免每次启动都跑 git status）。
    if let Some(dirty) = cached.fingerprint.git_dirty_hash {
        if compute_git_dirty_hash(&root_norm) != Some(dirty) {
            return None;
        }
    }
    Some(cached)
}

/// 轻量读取：只校验存在 + schema + root + language，**不校验指纹**。
/// 供 P4 RepoMap 排序权重注入用（best-effort，读到旧索引只会让排序略偏，
/// 不影响查询正确性；成本 ~ms 级，不做 git status / 源文件枚举）。
pub fn load_cache_quick(root: &Path, language: Language) -> Option<TopologyData> {
    if cache_disabled("topology") {
        return None;
    }
    let root_norm = normalize_root(root);
    let path = topology_cache_path(&root_norm, language);
    let Ok(meta) = fs::metadata(&path) else {
        return None;
    };
    if !cache_within_limit("topology", meta.len()) {
        return None;
    }
    let content = fs::read_to_string(&path).ok()?;
    let cached: TopologyCache = serde_json::from_str(&content).ok()?;
    if cached.schema_version != TOPOLOGY_CACHE_SCHEMA_VERSION
        || cached.language != language.as_str()
        || cached.root != root_norm.to_string_lossy()
    {
        return None;
    }
    Some(cached.data)
}

/// 保存缓存（best-effort，失败静默降级不阻断）。跨平台写盘：
/// 写唯一 tmp（带 pid）→ rename；Windows 上 rename 无法覆盖已存在目标时
/// 降级为 remove 目标再 rename；仍失败则丢弃（接受竞态，最坏下次重建）。
pub fn save_cache(root: &Path, language: Language, data: &TopologyData) {
    if cache_disabled("topology") {
        return;
    }
    let root_norm = normalize_root(root);
    let fingerprint = SourceFingerprint::compute(&root_norm, language);
    let cache = TopologyCache {
        schema_version: TOPOLOGY_CACHE_SCHEMA_VERSION,
        language: language.as_str().to_string(),
        root: root_norm.to_string_lossy().to_string(),
        fingerprint,
        data: data.clone(),
    };
    let json = match serde_json::to_string_pretty(&cache) {
        Ok(j) => j,
        Err(_) => return,
    };
    // 写侧网关：Topology 优先持久化（豁免）；仅当策略要求受限且超限时放弃。
    if !cache_within_limit("topology", json.len() as u64) {
        return;
    }
    let claw_dir = root_norm.join(".claw");
    if fs::create_dir_all(&claw_dir).is_err() {
        return;
    }
    let final_path = topology_cache_path(&root_norm, language);
    let tmp = claw_dir.join(format!(
        "topology_{}.{}.tmp",
        language.as_str(),
        std::process::id()
    ));
    if fs::write(&tmp, json).is_err() {
        return;
    }
    if fs::rename(&tmp, &final_path).is_err() {
        // Windows:rename 无法覆盖已存在目标 → 删除目标后重试（接受非原子窗口）。
        let _ = fs::remove_file(&final_path);
        let _ = fs::rename(&tmp, &final_path);
    }
}

// ---------------------------------------------------------------------------
// 通用工具（RepoMap 缓存复用）
// ---------------------------------------------------------------------------

/// 缓存逃生阀。`kind` ∈ {"topology", "repomap"}；分项优先于统一开关。
#[must_use]
pub fn cache_disabled(kind: &str) -> bool {
    let env = |k: &str| std::env::var(k).map(|v| v == "1").unwrap_or(false);
    env("CLAW_DISABLE_CACHE")
        || (kind == "topology" && env("CLAW_DISABLE_TOPOLOGY_CACHE"))
        || (kind == "repomap" && env("CLAW_DISABLE_REPOMAP_CACHE"))
}

/// 缓存大小上限（字节），`CLAW_CACHE_MAX_BYTES` 可覆盖。
#[must_use]
pub fn cache_max_bytes() -> u64 {
    std::env::var("CLAW_CACHE_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_CACHE_MAX_BYTES)
}

/// root 规范化：`std::path::absolute`（失败时 `current_dir().join`）后对
/// `components()` 归一化 `.` / `..`。**不用 `fs::canonicalize`**——Windows 上
/// 会返回 `\\?\C:\...` 前缀且解析符号链接，导致比较失败。
#[must_use]
pub fn normalize_root(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path)
        .or_else(|_| std::env::current_dir().map(|c| c.join(path)))
        .unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        abs
    } else {
        out
    }
}

/// 带超时的子进程执行（`std::process::Child` 无标准库 `wait_timeout`，自实现）：
/// `try_wait` + `sleep(50ms)` 轮询（勿忙等），超时 `kill()` 后必须 `wait()` 回收
/// 僵尸进程。stdout 由读线程持续消费（防管道缓冲填满阻塞子进程），返回 `None`
/// 表示 spawn 失败 / 超时。调用方判断 `status.success()`。
#[must_use]
pub fn run_command_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    // 必须 wait() 回收，防僵尸进程。
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    };
    let stdout = stdout_handle.join().unwrap_or_default();
    Some(Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

fn run_git(root: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(root);
    let out = run_command_with_timeout(&mut cmd, timeout)?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

    #[test]
    fn normalize_root_handles_relative_and_dotdot() {
        let base = std::env::current_dir().unwrap();
        let abs = base.join("a").join("..").join("b");
        let norm = normalize_root(&abs);
        assert_eq!(norm, base.join("b"));
    }

    #[test]
    fn detect_languages_rust_and_python() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_languages(dir.path()).is_empty());
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(detect_languages(dir.path()), vec![Language::Rust]);
        std::fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(
            detect_languages(dir.path()),
            vec![Language::Rust, Language::Python]
        );
    }

    #[test]
    fn pick_primary_language_workspace_root_prefers_rust() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(pick_primary_language(dir.path()), Some(Language::Rust));
    }

    #[test]
    fn pick_primary_language_python_only_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(pick_primary_language(dir.path()), Some(Language::Python));
    }

    #[test]
    fn pick_primary_language_single_crate_falls_back_to_rust() {
        // 单 crate Rust 项目（无 [workspace] 段）：规则 2/3 均不命中 → 规则 4 得 Rust。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        assert_eq!(pick_primary_language(dir.path()), Some(Language::Rust));
    }

    #[test]
    fn workspace_detection_ignores_subsection_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "# [workspace]\n[workspace.metadata]\n[workspace]\nmembers=[]\n",
        )
        .unwrap();
        assert!(is_workspace_root_manifest(dir.path()));
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "# [workspace]\n[workspace.metadata]\n",
        )
        .unwrap();
        assert!(!is_workspace_root_manifest(dir.path()));
    }

    #[test]
    fn fingerprint_changes_when_source_mtime_changes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "pub fn a() {}\n").unwrap();
        let fp1 = SourceFingerprint::compute_structural(dir.path(), Language::Rust);
        // 等一帧，确保 mtime 可区分（部分文件系统 mtime 精度秒级）。
        std::thread::sleep(StdDuration::from_millis(1100));
        std::fs::write(dir.path().join("a.rs"), "pub fn a() {}\npub fn b() {}\n").unwrap();
        let fp2 = SourceFingerprint::compute_structural(dir.path(), Language::Rust);
        assert_ne!(fp1.source_mtime_hash, fp2.source_mtime_hash);
    }

    #[test]
    fn fingerprint_ignores_other_language_changes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "pub fn a() {}\n").unwrap();
        let fp1 = SourceFingerprint::compute_structural(dir.path(), Language::Rust);
        std::fs::write(dir.path().join("b.py"), "def b():\n    pass\n").unwrap();
        let fp2 = SourceFingerprint::compute_structural(dir.path(), Language::Rust);
        assert_eq!(fp1.source_mtime_hash, fp2.source_mtime_hash);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let data = TopologyData {
            module_graph: crate::project_topology::ModuleGraph {
                workspace_root: dir.path().to_path_buf(),
                crates: Vec::new(),
                reverse_deps: Default::default(),
            },
            symbol_index: None,
            built_at_ms: 0,
        };
        save_cache(dir.path(), Language::Rust, &data);
        let loaded = load_cache(dir.path(), Language::Rust);
        assert!(loaded.is_some(), "cache should round-trip");
        let loaded = loaded.unwrap();
        assert_eq!(loaded.data.built_at_ms, 0);
    }

    #[test]
    fn load_returns_none_on_corrupted_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claw")).unwrap();
        std::fs::write(
            dir.path().join(".claw").join("topology_rust.json"),
            "not json{{{",
        )
        .unwrap();
        assert!(load_cache(dir.path(), Language::Rust).is_none());
    }

    #[test]
    fn load_quick_skips_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let data = TopologyData {
            module_graph: crate::project_topology::ModuleGraph {
                workspace_root: dir.path().to_path_buf(),
                crates: Vec::new(),
                reverse_deps: Default::default(),
            },
            symbol_index: None,
            built_at_ms: 7,
        };
        save_cache(dir.path(), Language::Rust, &data);
        // 改源文件 → 指纹失效 → 强校验 load_cache 返回 None，但 load_cache_quick 仍返回。
        std::fs::write(dir.path().join("a.rs"), "pub fn changed() {}\n").unwrap();
        assert!(load_cache(dir.path(), Language::Rust).is_none());
        let quick = load_cache_quick(dir.path(), Language::Rust);
        assert_eq!(quick.map(|d| d.built_at_ms), Some(7));
    }

    #[test]
    fn within_limit_topology_exempt_repomap_gated() {
        // 超限策略：Topology 优先持久化（豁免），RepoMap 受上限约束。
        let max = 50;
        assert!(within_limit_with("topology", max + 1, max));
        assert!(within_limit_with("topology", u64::MAX, max));
        assert!(within_limit_with("repomap", max, max));
        assert!(!within_limit_with("repomap", max + 1, max));
    }

    #[test]
    fn pick_primary_language_with_override_wins() {
        // 显式覆盖优先于所有探测规则（含 workspace 根与 Python 标记）。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(
            pick_primary_language_with(dir.path(), Some("python")),
            Some(Language::Python)
        );
        assert_eq!(
            pick_primary_language_with(dir.path(), Some("RUST")),
            Some(Language::Rust)
        );
        // 无效值 → 回落到常规规则（workspace 根 → Rust）。
        assert_eq!(
            pick_primary_language_with(dir.path(), Some("golang")),
            Some(Language::Rust)
        );
    }

    #[test]
    fn pick_primary_language_override_python_on_single_crate() {
        // 单 crate Rust 项目 + 覆盖 python → Python（覆盖压过规则 2/4）。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        assert_eq!(
            pick_primary_language_with(dir.path(), Some("python")),
            Some(Language::Python)
        );
        assert_eq!(
            pick_primary_language_with(dir.path(), None),
            Some(Language::Rust)
        );
    }

    #[test]
    fn save_cache_creates_claw_dir_and_survives_repeated_save() {
        let dir = tempfile::tempdir().unwrap();
        let data = TopologyData {
            module_graph: crate::project_topology::ModuleGraph {
                workspace_root: dir.path().to_path_buf(),
                crates: Vec::new(),
                reverse_deps: Default::default(),
            },
            symbol_index: None,
            built_at_ms: 0,
        };
        // 第一次保存（创建 .claw 目录）。
        save_cache(dir.path(), Language::Rust, &data);
        assert!(dir.path().join(".claw").join("topology_rust.json").exists());
        // 第二次保存：目标已存在，验证跨平台写盘分支不 panic 且能覆盖。
        let data2 = TopologyData {
            built_at_ms: 42,
            ..data
        };
        save_cache(dir.path(), Language::Rust, &data2);
        let loaded = load_cache(dir.path(), Language::Rust);
        assert_eq!(loaded.map(|c| c.data.built_at_ms), Some(42));
    }
}
