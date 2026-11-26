//! 轮次工作区检查点（与 git 无关）。
//!
//! 每轮结束（以及首轮开始前）把工作区**纳入范围的文件内容**存成一份内容寻址快照：
//!
//! ```text
//! <workspace>/.claw/checkpoints/<session>/
//!   index.json                 # 检查点列表（seq/label/created_ms/files/bytes/partial）
//!   c0001/manifest.json        # 相对路径 → blob 哈希 + size + mtime
//!   blobs/<sha256>             # 文件内容，跨检查点自动去重
//! ```
//!
//! 设计取舍：
//! - 只读工作区、不碰 git index / stash，因此和「未提交改动」天然共存；回滚时
//!   覆盖的是文件内容本身，用户自己没提交的修改同样会回到快照时刻。
//! - 非 git 目录同样可用：默认递归枚举，跳过 `.git/.claw/node_modules/target`
//!   等重目录；git 仓库改用 `git ls-files --cached --others --exclude-standard`
//!   尊重 `.gitignore`。
//! - 增量：与上一份 manifest 比较 size+mtime，未变文件直接复用 blob 哈希，不重读。
//! - 上限：单文件 2 MiB、单次新增 blob 128 MiB、最多 2 万个文件；超限标记
//!   `partial`，此时回滚只覆盖已捕获文件、**不做删除**，避免误删未捕获内容。
//! - 回滚前先 `plan`（dry-run）给出恢复/删除/未变数量与样例路径，并返回对计划
//!   内容的哈希 `token`；真正执行时重新计算并比对 token，工作区变了就 409。

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAX_FILES: usize = 20_000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_NEW_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const SAMPLE_LIMIT: usize = 40;

/// 枚举时跳过的目录：构建产物/依赖目录通常体积巨大，且不属于「用户源码」。
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".claw",
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".gradle",
    ".idea",
    ".cache",
    "vendor",
    "Pods",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointMeta {
    pub id: String,
    /// 0 = 首轮开始前的起始状态；之后每轮 +1。
    pub seq: u64,
    pub label: String,
    pub created_ms: u64,
    pub files: usize,
    pub bytes: u64,
    #[serde(default)]
    pub partial: bool,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct IndexFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    checkpoints: Vec<CheckpointMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    blob: String,
    size: u64,
    mtime_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    created_ms: u64,
    #[serde(default)]
    partial: bool,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    skipped: usize,
    #[serde(default)]
    files: BTreeMap<String, Entry>,
}

/// 回滚计划（dry-run 与执行结果共用）。
#[derive(Debug, Clone, Serialize)]
pub struct RollbackPlan {
    pub checkpoint: String,
    /// 需要从快照写回的文件数（含被删除后重建的文件）。
    pub restore: usize,
    /// 快照里没有、需要删除的当前文件数；`partial` 检查点恒为 0。
    pub delete: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub partial: bool,
    /// 计划内容哈希；执行时带回，用于发现「预览后工作区又变了」。
    pub token: String,
    /// 前 [`SAMPLE_LIMIT`] 条动作，形如 `R src/main.rs` / `D src/new.rs`。
    pub sample: Vec<String>,
}

#[derive(Debug)]
pub enum CheckpointError {
    NotFound(String),
    Conflict(String),
    Internal(String),
}

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(message) | Self::Conflict(message) | Self::Internal(message) => {
                write!(f, "{message}")
            }
        }
    }
}

impl std::error::Error for CheckpointError {}

impl CheckpointError {
    pub fn status(&self) -> u16 {
        match self {
            Self::NotFound(_) => 404,
            Self::Conflict(_) => 409,
            Self::Internal(_) => 500,
        }
    }
}

enum Action {
    Restore {
        path: String,
        /// 目标 blob + 预览时的当前内容哈希；任一变化都说明预览已过期。
        fingerprint: String,
    },
    Delete {
        path: String,
        /// 预览时该文件的内容哈希。
        fingerprint: String,
    },
}

// ---------------------------------------------------------------------------
// 路径与哈希
// ---------------------------------------------------------------------------

fn valid_session(session: &str) -> bool {
    !session.is_empty()
        && !session.contains("..")
        && !session.contains('/')
        && !session.contains('\\')
        && session
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

/// 只接受工作区内的普通相对路径（无 `..`、无根前缀、不碰管理目录）。
fn safe_rel(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.starts_with('\\') {
        return false;
    }
    let candidate = Path::new(path);
    if !candidate
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return false;
    }
    !(path.starts_with(".git/") || path.starts_with(".claw/"))
}

fn checkpoint_root(workspace: &Path, session: &str) -> PathBuf {
    workspace.join(".claw").join("checkpoints").join(session)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn file_mtime_ms(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= limit {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

// ---------------------------------------------------------------------------
// 工作区枚举
// ---------------------------------------------------------------------------

/// 工作区是否在 git 工作树内（子目录也算）。供 diff 面板与非 git 提示共用。
pub fn is_git_work_tree(root: &Path) -> bool {
    runtime::sandbox::quiet_command("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
        })
        .unwrap_or(false)
}

/// 列出纳入快照范围的文件（POSIX 相对路径，已排序去重）。
fn list_workspace_files(root: &Path) -> Vec<String> {
    if is_git_work_tree(root) {
        let output = runtime::sandbox::quiet_command("git")
            .arg("-C")
            .arg(root)
            .args([
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ])
            .output();
        if let Ok(output) = output {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                let mut paths: Vec<String> = text
                    .split('\0')
                    .filter(|path| safe_rel(path))
                    .map(str::to_string)
                    .collect();
                paths.sort();
                paths.dedup();
                return paths;
            }
        }
        // git 枚举失败时退回文件系统遍历，而不是给出空快照。
    }
    let mut paths = Vec::new();
    walk_files(root, root, 0, &mut paths);
    paths.sort();
    paths.dedup();
    paths
}

fn walk_files(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_DEPTH || out.len() >= MAX_FILES * 4 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        if file_type.is_symlink() {
            // 不跟随符号链接：既避免越界，也避免把链接目标重复存一遍。
            continue;
        }
        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk_files(root, &path, depth + 1, out);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_string_lossy().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
        if safe_rel(&relative) {
            out.push(relative);
        }
    }
}

// ---------------------------------------------------------------------------
// 存储
// ---------------------------------------------------------------------------

fn read_index(dir: &Path) -> IndexFile {
    fs::read_to_string(dir.join("index.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), CheckpointError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            CheckpointError::Internal(format!("创建目录失败 {}: {error}", parent.display()))
        })?;
    }
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| CheckpointError::Internal(format!("序列化失败: {error}")))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|error| {
        CheckpointError::Internal(format!("写入失败 {}: {error}", tmp.display()))
    })?;
    fs::rename(&tmp, path).map_err(|error| {
        CheckpointError::Internal(format!("重命名失败 {}: {error}", path.display()))
    })
}

fn read_manifest(dir: &Path, id: &str) -> Result<Manifest, CheckpointError> {
    let path = dir.join(id).join("manifest.json");
    let text = fs::read_to_string(&path).map_err(|_| {
        CheckpointError::NotFound(format!("检查点 {id} 的清单不存在，可能已被清理"))
    })?;
    serde_json::from_str(&text)
        .map_err(|error| CheckpointError::Internal(format!("检查点 {id} 清单损坏: {error}")))
}

fn blob_path(dir: &Path, hash: &str) -> PathBuf {
    dir.join("blobs").join(hash)
}

fn blob_exists(dir: &Path, hash: &str) -> bool {
    blob_path(dir, hash).is_file()
}

fn write_blob(dir: &Path, hash: &str, bytes: &[u8]) -> Result<(), CheckpointError> {
    let path = blob_path(dir, hash);
    if path.is_file() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            CheckpointError::Internal(format!("创建 blob 目录失败 {}: {error}", parent.display()))
        })?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, bytes)
        .map_err(|error| CheckpointError::Internal(format!("写入 blob 失败: {error}")))?;
    fs::rename(&tmp, &path)
        .map_err(|error| CheckpointError::Internal(format!("落盘 blob 失败: {error}")))
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<(), CheckpointError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            CheckpointError::Internal(format!("创建目录失败 {}: {error}", parent.display()))
        })?;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let tmp = path.with_file_name(format!(".{name}.cp-{}", std::process::id()));
    fs::write(&tmp, bytes)
        .map_err(|error| CheckpointError::Internal(format!("写入失败: {error}")))?;
    fs::rename(&tmp, path)
        .map_err(|error| CheckpointError::Internal(format!("重命名失败: {error}")))
}

// ---------------------------------------------------------------------------
// 创建
// ---------------------------------------------------------------------------

/// 记录一份检查点。`seq_override` 仅用于首轮前的 `0`。
pub fn create(
    workspace: &Path,
    session: &str,
    label: &str,
    seq_override: Option<u64>,
) -> Result<CheckpointMeta, CheckpointError> {
    if !valid_session(session) {
        return Err(CheckpointError::Internal("无效的会话 ID".to_string()));
    }
    let dir = checkpoint_root(workspace, session);
    fs::create_dir_all(dir.join("blobs")).map_err(|error| {
        CheckpointError::Internal(format!("创建检查点目录失败 {}: {error}", dir.display()))
    })?;

    let mut index = read_index(&dir);
    let seq = seq_override.unwrap_or_else(|| {
        index
            .checkpoints
            .iter()
            .map(|item| item.seq)
            .max()
            .map_or(0, |max| max + 1)
    });
    let id = format!("c{seq:04}");
    if index.checkpoints.iter().any(|item| item.id == id) {
        return Err(CheckpointError::Conflict(format!("检查点 {id} 已存在")));
    }

    let previous = index
        .checkpoints
        .iter()
        .filter(|item| item.seq < seq)
        .max_by_key(|item| item.seq)
        .and_then(|item| read_manifest(&dir, &item.id).ok());

    let paths = list_workspace_files(workspace);
    let mut manifest = Manifest {
        version: 1,
        created_ms: now_ms(),
        partial: false,
        reason: String::new(),
        skipped: 0,
        files: BTreeMap::new(),
    };
    let mut new_bytes: u64 = 0;

    for path in paths {
        if manifest.files.len() >= MAX_FILES {
            manifest.partial = true;
            manifest.skipped += 1;
            manifest.reason = format!("文件数超过上限 {MAX_FILES}，其余文件未纳入");
            break;
        }
        let full = workspace.join(&path);
        let Ok(metadata) = fs::metadata(&full) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let size = metadata.len();
        let mtime_ms = file_mtime_ms(&metadata);

        // 未变化就直接复用上一份清单里的 blob，避免每轮重读整个仓库。
        if let Some(previous_entry) = previous.as_ref().and_then(|value| value.files.get(&path)) {
            if previous_entry.size == size
                && previous_entry.mtime_ms == mtime_ms
                && blob_exists(&dir, &previous_entry.blob)
            {
                manifest.files.insert(path, previous_entry.clone());
                continue;
            }
        }

        if size > MAX_FILE_BYTES {
            manifest.partial = true;
            manifest.skipped += 1;
            manifest.reason = format!(
                "存在超过 {} MiB 的文件，未纳入",
                MAX_FILE_BYTES / 1024 / 1024
            );
            continue;
        }
        if new_bytes.saturating_add(size) > MAX_NEW_BYTES {
            manifest.partial = true;
            manifest.skipped += 1;
            manifest.reason = format!(
                "新增快照数据超过 {} MiB，后续文件未纳入",
                MAX_NEW_BYTES / 1024 / 1024
            );
            continue;
        }
        let Ok(bytes) = fs::read(&full) else {
            manifest.skipped += 1;
            continue;
        };
        let blob = sha256_hex(&bytes);
        if !blob_exists(&dir, &blob) {
            write_blob(&dir, &blob, &bytes)?;
            new_bytes = new_bytes.saturating_add(size);
        }
        manifest.files.insert(
            path,
            Entry {
                blob,
                size,
                mtime_ms,
            },
        );
    }

    let meta = CheckpointMeta {
        id: id.clone(),
        seq,
        label: truncate_chars(label.trim(), 80),
        created_ms: manifest.created_ms,
        files: manifest.files.len(),
        bytes: manifest.files.values().map(|entry| entry.size).sum(),
        partial: manifest.partial,
        reason: manifest.reason.clone(),
    };

    write_json(&dir.join(&id).join("manifest.json"), &manifest)?;
    index.version = 1;
    index.checkpoints.push(meta.clone());
    write_json(&dir.join("index.json"), &index)?;
    Ok(meta)
}

/// 列出检查点（按 seq 升序）。
pub fn list(workspace: &Path, session: &str) -> Vec<CheckpointMeta> {
    if !valid_session(session) {
        return Vec::new();
    }
    let mut items = read_index(&checkpoint_root(workspace, session)).checkpoints;
    items.sort_by_key(|item| item.seq);
    items
}

// ---------------------------------------------------------------------------
// 回滚
// ---------------------------------------------------------------------------

fn compute_actions(
    workspace: &Path,
    manifest: &Manifest,
) -> Result<(Vec<Action>, usize, usize), CheckpointError> {
    let mut actions = Vec::new();
    let mut unchanged = 0;
    for (path, entry) in &manifest.files {
        if !safe_rel(path) {
            continue;
        }
        let full = workspace.join(path);
        let current = fs::read(&full).ok();
        let current_hash = current
            .as_deref()
            .map_or_else(|| "missing".to_string(), sha256_hex);
        if current_hash == entry.blob {
            unchanged += 1;
        } else {
            actions.push(Action::Restore {
                path: path.clone(),
                fingerprint: format!("{}+{}", entry.blob, current_hash),
            });
        }
    }

    let mut deleted = 0;
    // partial 快照有未捕获的文件，无法区分「新增」与「当时就存在但被跳过」，
    // 因此只覆盖、不删除。
    if !manifest.partial {
        for path in list_workspace_files(workspace) {
            if manifest.files.contains_key(&path) || !safe_rel(&path) {
                continue;
            }
            let current_hash = fs::read(workspace.join(&path))
                .ok()
                .as_deref()
                .map_or_else(|| "missing".to_string(), sha256_hex);
            actions.push(Action::Delete {
                path,
                fingerprint: current_hash,
            });
            deleted += 1;
        }
    }
    Ok((actions, unchanged, deleted))
}

fn plan_token(actions: &[Action]) -> String {
    let mut feed = String::new();
    for action in actions {
        match action {
            Action::Restore { path, fingerprint } => {
                feed.push_str(&format!("R\0{path}\0{fingerprint}\n"));
            }
            Action::Delete { path, fingerprint } => {
                feed.push_str(&format!("D\0{path}\0{fingerprint}\n"));
            }
        }
    }
    sha256_hex(feed.as_bytes())
}

fn build_plan(
    checkpoint: &str,
    workspace: &Path,
    manifest: &Manifest,
) -> Result<(RollbackPlan, Vec<Action>, String), CheckpointError> {
    let (actions, unchanged, deleted) = compute_actions(workspace, manifest)?;
    let token = plan_token(&actions);
    let restore = actions
        .iter()
        .filter(|action| matches!(action, Action::Restore { .. }))
        .count();
    let sample = actions
        .iter()
        .take(SAMPLE_LIMIT)
        .map(|action| match action {
            Action::Restore { path, .. } => format!("R {path}"),
            Action::Delete { path, .. } => format!("D {path}"),
        })
        .collect();
    Ok((
        RollbackPlan {
            checkpoint: checkpoint.to_string(),
            restore,
            delete: deleted,
            unchanged,
            skipped: manifest.skipped,
            partial: manifest.partial,
            token: token.clone(),
            sample,
        },
        actions,
        token,
    ))
}

/// 只计算计划，不动文件（供确认弹层展示）。
pub fn plan(
    workspace: &Path,
    session: &str,
    checkpoint: &str,
) -> Result<RollbackPlan, CheckpointError> {
    if !valid_session(session) {
        return Err(CheckpointError::NotFound("无效的会话 ID".to_string()));
    }
    let dir = checkpoint_root(workspace, session);
    let manifest = read_manifest(&dir, checkpoint)?;
    build_plan(checkpoint, workspace, &manifest).map(|(plan, _, _)| plan)
}

/// 执行回滚；`token` 必须等于当前重新计算出的计划哈希。
pub fn rollback(
    workspace: &Path,
    session: &str,
    checkpoint: &str,
    token: &str,
) -> Result<RollbackPlan, CheckpointError> {
    if !valid_session(session) {
        return Err(CheckpointError::NotFound("无效的会话 ID".to_string()));
    }
    let dir = checkpoint_root(workspace, session);
    let manifest = read_manifest(&dir, checkpoint)?;
    let (plan, actions, current_token) = build_plan(checkpoint, workspace, &manifest)?;
    if current_token != token {
        return Err(CheckpointError::Conflict(
            "工作区在预览后已发生变化，请重新预览再确认".to_string(),
        ));
    }
    for action in &actions {
        match action {
            Action::Restore { path, .. } => {
                let Some(entry) = manifest.files.get(path) else {
                    continue;
                };
                let blob = blob_path(&dir, &entry.blob);
                let bytes = fs::read(&blob).map_err(|error| {
                    CheckpointError::Internal(format!(
                        "读取快照内容失败 {}: {error}",
                        blob.display()
                    ))
                })?;
                atomic_write_bytes(&workspace.join(path), &bytes)?;
            }
            Action::Delete { path, .. } => {
                let full = workspace.join(path);
                if full.is_file() {
                    fs::remove_file(&full).map_err(|error| {
                        CheckpointError::Internal(format!(
                            "删除新增文件失败 {}: {error}",
                            full.display()
                        ))
                    })?;
                }
            }
        }
    }
    Ok(plan)
}

/// 从会话 JSONL 尾部取最后一条用户消息，作为检查点的可读标签。
pub fn last_user_label(session_path: &Path) -> Option<String> {
    let mut file = fs::File::open(session_path).ok()?;
    let length = file.metadata().ok()?.len();
    let budget = 256 * 1024u64;
    let start = length.saturating_sub(budget);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).ok()?;
    let text = String::from_utf8_lossy(&buffer);
    // 从中间截断时第一行可能不完整，解析失败会被跳过。
    for line in text.lines().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value["message"]["role"] != "user" {
            continue;
        }
        let Some(blocks) = value["message"]["blocks"].as_array() else {
            continue;
        };
        let joined = blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let joined = joined.trim();
        if joined.is_empty() {
            continue;
        }
        let first = joined
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with("[Web ") && !line.starts_with('['))
            .or_else(|| joined.lines().map(str::trim).find(|line| !line.is_empty()))?;
        return Some(truncate_chars(first, 60));
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(root: &Path, path: &str, content: &str) {
        let full = root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    #[test]
    fn create_and_rollback_restores_previous_turn() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.txt", "one\n");
        write(root, "src/b.txt", "keep\n");

        let first = create(root, "session-1", "起始状态", Some(0)).unwrap();
        assert_eq!(first.seq, 0);
        assert_eq!(first.files, 2);

        // 第 1 轮：改 a、删 b、新增 c。
        write(root, "src/a.txt", "two\n");
        fs::remove_file(root.join("src/b.txt")).unwrap();
        write(root, "src/c.txt", "new\n");
        let second = create(root, "session-1", "第 1 轮", None).unwrap();
        assert_eq!(second.seq, 1);
        assert_eq!(second.files, 2); // a + c

        // 再改一轮。
        write(root, "src/a.txt", "three\n");
        let plan = plan(root, "session-1", &first.id).unwrap();
        assert_eq!(plan.restore, 2, "a 与 b 都要恢复");
        assert_eq!(plan.delete, 1, "c 是快照后新增，要删除");

        let restored = rollback(root, "session-1", &first.id, &plan.token).unwrap();
        assert_eq!(restored.restore, 2);
        assert_eq!(fs::read_to_string(root.join("src/a.txt")).unwrap(), "one\n");
        assert_eq!(
            fs::read_to_string(root.join("src/b.txt")).unwrap(),
            "keep\n"
        );
        assert!(!root.join("src/c.txt").exists());
    }

    #[test]
    fn rollback_rejects_stale_token() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.txt", "one\n");
        let first = create(root, "s", "起始状态", Some(0)).unwrap();
        write(root, "a.txt", "two\n");
        let plan = plan(root, "s", &first.id).unwrap();

        // 预览之后工作区又变了：token 应失配。
        write(root, "a.txt", "three\n");
        let error = rollback(root, "s", &first.id, &plan.token).unwrap_err();
        assert!(matches!(error, CheckpointError::Conflict(_)));
    }

    #[test]
    fn partial_checkpoint_does_not_delete() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.txt", "one\n");
        let first = create(root, "s", "起始状态", Some(0)).unwrap();
        // 伪造 partial：模拟文件数/体积超限导致未捕获。
        let checkpoint_dir = checkpoint_root(root, "s");
        let mut manifest = read_manifest(&checkpoint_dir, &first.id).unwrap();
        manifest.partial = true;
        manifest.reason = "测试".to_string();
        write_json(
            &checkpoint_dir.join(&first.id).join("manifest.json"),
            &manifest,
        )
        .unwrap();

        write(root, "b.txt", "new\n");
        let plan = plan(root, "s", &first.id).unwrap();
        assert_eq!(plan.delete, 0);
        assert!(plan.partial);
    }

    #[test]
    fn git_repo_uses_gitignore_and_round_trips() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let output = runtime::sandbox::quiet_command("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
        };
        git(&["init"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "test"]);
        write(root, ".gitignore", "ignored/\n");
        write(root, "src/a.txt", "one\n");
        write(root, "ignored/big.txt", "ignored\n");
        git(&["add", "."]);
        git(&["commit", "-m", "init"]);

        let first = create(root, "s", "起始状态", Some(0)).unwrap();
        assert_eq!(first.files, 2, "只应捕获 .gitignore 与 src/a.txt");
        write(root, "src/a.txt", "two\n");
        let plan = plan(root, "s", &first.id).unwrap();
        rollback(root, "s", &first.id, &plan.token).unwrap();
        assert_eq!(fs::read_to_string(root.join("src/a.txt")).unwrap(), "one\n");
    }

    #[test]
    fn session_id_traversal_is_rejected() {
        let dir = tempdir().unwrap();
        assert!(create(dir.path(), "../escape", "x", None).is_err());
        assert!(create(dir.path(), "a/b", "x", None).is_err());
        assert!(list(dir.path(), "../escape").is_empty());
    }
}
