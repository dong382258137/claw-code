//! TransactionManager — turn-level VCS snapshot for rollback safety.
//!
//! # Design
//!
//! `RefactorTransaction` 提供 turn 级事务保障：一次 turn 内的所有修改
//! 要么全成功要么全回滚。核心机制是文件级 git stash（只 stash 实际修改的文件），
//! 配合多重边界场景防护。
//!
//! ## Safety
//!
//! - 文件级 stash：只 stash `modified_files` 列表，不影响其他未提交修改
//! - Stash pop 冲突：降级为 `git checkout -- {files}` + 报告冲突
//! - 非 git 仓库：进入 `Disabled` 状态
//! - Detached HEAD：回退到文件复制快照（`.claw/tx-snapshots/`）

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// VcsError
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum VcsError {
    NotGitRepo,
    StashFailed(String),
    StashPopConflict {
        conflicted: Vec<PathBuf>,
        message: String,
    },
    DetachedHead,
    Io(std::io::Error),
    FileSnapshotFailed(String),
}

impl std::fmt::Display for VcsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotGitRepo => write!(f, "not a git repository"),
            Self::StashFailed(msg) => write!(f, "git stash failed: {msg}"),
            Self::StashPopConflict {
                conflicted,
                message,
            } => {
                write!(
                    f,
                    "stash pop conflict in {} files: {message}",
                    conflicted.len()
                )
            }
            Self::DetachedHead => write!(f, "detached HEAD — cannot use stash"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::FileSnapshotFailed(msg) => write!(f, "file snapshot failed: {msg}"),
        }
    }
}

impl std::error::Error for VcsError {}

// ---------------------------------------------------------------------------
// TransactionStatus
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct TransactionStatus {
    pub state: String,
    pub is_git_repo: bool,
    pub stash_ref: Option<String>,
    pub modified_files: Vec<PathBuf>,
    pub snapshot_dir: Option<PathBuf>,
    /// P1-3 修复：非 git 仓库时返回 reason 字段，符合计划规范。
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// RefactorTransaction
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct RefactorTransaction {
    root: PathBuf,
    /// Stash reference returned by `git stash create`.
    stash_ref: Option<String>,
    /// Tracked modified files for file-level stash.
    modified_files: Vec<PathBuf>,
    /// Detached HEAD fallback: copy files to `.claw/tx-snapshots/{turn_id}/`.
    snapshot_dir: Option<PathBuf>,
    /// Whether this is a git repository.
    is_git_repo: bool,
    /// Whether HEAD is detached.
    is_detached: bool,
    /// Whether a snapshot was taken.
    has_snapshot: bool,
}

impl RefactorTransaction {
    /// Create a new RefactorTransaction for the given workspace root.
    ///
    /// Detects git repo status synchronously.
    pub fn new(root: PathBuf) -> Self {
        let is_git_repo = root.join(".git").exists();
        let is_detached = if is_git_repo {
            Self::check_detached_head(&root)
        } else {
            false
        };
        Self {
            root,
            stash_ref: None,
            modified_files: Vec::new(),
            snapshot_dir: None,
            is_git_repo,
            is_detached,
            has_snapshot: false,
        }
    }

    /// Check if HEAD is detached.
    fn check_detached_head(root: &Path) -> bool {
        Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(root)
            .output()
            .map(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                s.trim() == "HEAD"
            })
            .unwrap_or(false)
    }

    /// Take a pre-turn snapshot.
    ///
    /// - Reads `git status --porcelain` to find modified files
    /// - If git repo and not detached: creates a stash commit via `git stash create`
    ///   (不入栈，避免污染用户的 stash 栈)
    /// - If detached HEAD: copies files to `.claw/tx-snapshots/`
    /// - If not a git repo: marks as disabled (no snapshot possible)
    pub fn pre_turn_snapshot(&mut self, turn_id: &str) -> Result<(), VcsError> {
        if !self.is_git_repo {
            return Ok(()); // Not a git repo — nothing to snapshot
        }

        if self.is_detached {
            return self.file_snapshot(turn_id);
        }

        // Check for uncommitted changes
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&self.root)
            .output()
            .map_err(|e| VcsError::StashFailed(format!("git status failed: {e}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            // No changes to snapshot
            return Ok(());
        }

        // Parse modified files (excluding .claw/ management files)
        self.modified_files = parse_modified_files(&stdout);

        // P1-2 修复：用 `git stash create` 创建 dangling commit（不入栈），
        // 避免污染用户的 stash 栈。返回值是 commit hash，可直接用于 `git stash apply`。
        // 之前用 `git stash push --include-untracked` 会全量 stash 整个工作树，
        // 且用 `stash@{0}` 引用会在并发 stash 时 pop 错误的 stash。
        let stash_output = Command::new("git")
            .args(["stash", "create"])
            .current_dir(&self.root)
            .output()
            .map_err(|e| VcsError::StashFailed(format!("git stash create failed: {e}")))?;

        let stash_ref = String::from_utf8_lossy(&stash_output.stdout)
            .trim()
            .to_string();

        if stash_ref.is_empty() {
            // stash create 返回空表示没有可 stash 的修改（可能都是 untracked 文件）
            // 此时不需要 stash，但仍然记录 modified_files 以便 rollback 时 checkout
            self.has_snapshot = true;
            return Ok(());
        }

        self.stash_ref = Some(stash_ref);
        self.has_snapshot = true;
        Ok(())
    }

    /// Mark files as dirty (called after Edit/Write/Delete tools).
    ///
    /// P2-3 修复：统一路径分隔符为 POSIX 风格，避免 Windows 上
    /// `src\main.rs` 和 `src/main.rs` 被视为不同文件导致去重失效。
    pub fn mark_dirty(&mut self, files: &[PathBuf]) {
        for f in files {
            // 标准化路径分隔符
            let normalized = PathBuf::from(f.to_string_lossy().replace('\\', "/"));
            if !self.modified_files.contains(&normalized) {
                self.modified_files.push(normalized);
            }
        }
    }

    /// Rollback to pre-turn state.
    ///
    /// P1-2/P1-3 修复后的策略：
    /// 1. 如果有 stash_ref（pre_turn 时有未提交修改）：
    ///    a. `git checkout -- .` 恢复所有 tracked 文件到 HEAD（丢弃 turn 内修改）
    ///    b. `git stash apply <hash>` 恢复用户原始修改
    ///    c. 如果 apply 冲突：**不 drop stash**，保留 stash 让 LLM 决策，
    ///    报告冲突文件列表
    /// 2. 如果没有 stash_ref（pre_turn 时是 clean tree）：
    ///    a. `git checkout -- <modified_files>` 恢复 turn 内修改的文件
    ///    b. `git clean -fd <modified_files>` 清理 turn 内创建的新文件（带路径限制）
    pub fn rollback(&mut self) -> Result<String, VcsError> {
        if !self.has_snapshot {
            return Ok("rollback: no snapshot taken (clean working tree or non-git repo)".into());
        }

        if self.is_detached {
            return self.file_restore();
        }

        if let Some(ref stash_ref) = self.stash_ref {
            // Step 1: Restore all tracked files to HEAD state (discard turn modifications)
            let _ = Command::new("git")
                .args(["checkout", "--", "."])
                .current_dir(&self.root)
                .output();

            // Step 2: Apply the pre-turn stash to restore user's original modifications
            let apply = Command::new("git")
                .args(["stash", "apply", stash_ref])
                .current_dir(&self.root)
                .output()
                .map_err(|e| VcsError::StashFailed(format!("git stash apply failed: {e}")))?;

            if apply.status.success() {
                self.has_snapshot = false;
                self.modified_files.clear();
                return Ok(
                    "rollback: restored to pre-turn state via git checkout + stash apply".into(),
                );
            }

            // P1-3 修复：stash apply 冲突时 **不 drop stash**，保留 stash 让 LLM 决策。
            // 之前先 `git stash drop` 再 `git checkout` 会永久丢失用户原始未提交修改。
            let stderr = String::from_utf8_lossy(&apply.stderr);
            let stdout = String::from_utf8_lossy(&apply.stdout);
            let combined = format!("{stdout}\n{stderr}");

            // 收集冲突文件列表
            let conflicted = self.get_conflicted_files();

            return Err(VcsError::StashPopConflict {
                conflicted,
                message: format!(
                    "stash apply had conflicts. The stash commit ({stash_ref}) is preserved \
                     — resolve conflicts manually with `git checkout --theirs/--ours <file>`, \
                     then run `git stash drop {stash_ref}` (or `git stash apply` again after \
                     resolving). Do NOT drop the stash before resolving. Error: {combined}"
                ),
            });
        }

        // 没有 stash_ref（pre_turn 时是 clean tree 或只有 untracked 文件）
        // 直接 checkout modified_files 恢复到 HEAD 状态
        if !self.modified_files.is_empty() {
            // 恢复 tracked 文件
            let tracked_files: Vec<&str> = self
                .modified_files
                .iter()
                .filter_map(|p| p.to_str())
                .collect();
            if !tracked_files.is_empty() {
                let _ = Command::new("git")
                    .args(["checkout", "--"])
                    .args(&tracked_files)
                    .current_dir(&self.root)
                    .output();
            }

            // P1-3 修复：`git clean -fd` 带路径限制，不再全仓库清理。
            // 只清理 modified_files 中标记为新增（untracked）的文件。
            // 通过 git status 检测哪些是 untracked 的。
            let untracked_files = self.get_untracked_files();
            if !untracked_files.is_empty() {
                let clean_files: Vec<&str> =
                    untracked_files.iter().filter_map(|p| p.to_str()).collect();
                let _ = Command::new("git")
                    .args(["clean", "-fd", "--"])
                    .args(&clean_files)
                    .current_dir(&self.root)
                    .output();
            }
        }

        self.has_snapshot = false;
        self.modified_files.clear();
        Ok("rollback: restored tracked files to HEAD state".into())
    }

    /// 获取当前冲突文件列表（通过 `git diff --name-only --diff-filter=U`）。
    fn get_conflicted_files(&self) -> Vec<PathBuf> {
        Command::new("git")
            .args(["diff", "--name-only", "--diff-filter=U"])
            .current_dir(&self.root)
            .output()
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 获取当前 untracked 文件列表（通过 `git ls-files --others --exclude-standard`）。
    fn get_untracked_files(&self) -> Vec<PathBuf> {
        Command::new("git")
            .args(["ls-files", "--others", "--exclude-standard"])
            .current_dir(&self.root)
            .output()
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get current transaction status.
    pub fn status(&self) -> TransactionStatus {
        let (state, reason) = if !self.is_git_repo {
            (
                "disabled".to_string(),
                Some("not a git repository".to_string()),
            )
        } else if self.is_detached {
            ("detached".to_string(), None)
        } else if self.has_snapshot {
            ("active".to_string(), None)
        } else {
            ("idle".to_string(), None)
        };
        TransactionStatus {
            state,
            is_git_repo: self.is_git_repo,
            stash_ref: self.stash_ref.clone(),
            modified_files: self.modified_files.clone(),
            snapshot_dir: self.snapshot_dir.clone(),
            reason,
        }
    }

    /// File-copy snapshot for detached HEAD fallback.
    ///
    /// P1-3 修复：处理已删除文件——记录删除标记，rollback 时删除 turn 内新建的同名文件。
    fn file_snapshot(&mut self, turn_id: &str) -> Result<(), VcsError> {
        let snap_dir = self.root.join(".claw").join("tx-snapshots").join(turn_id);
        fs::create_dir_all(&snap_dir)
            .map_err(|e| VcsError::FileSnapshotFailed(format!("mkdir: {e}")))?;

        // Copy tracked modified files (记录哪些文件在 pre_turn 时存在)
        for file in &self.modified_files {
            let src = self.root.join(file);
            if src.exists() {
                let dst = snap_dir.join(file);
                if let Some(parent) = dst.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                fs::copy(&src, &dst)
                    .map_err(|e| VcsError::FileSnapshotFailed(format!("copy: {e}")))?;
            }
            // 如果文件不存在（已删除），不复制，但 modified_files 中仍记录该路径，
            // rollback 时如果该文件重新出现则删除它（恢复到"已删除"状态）。
        }

        // 记录 pre_turn 时的 untracked 文件列表，以便 rollback 时区分 turn 内新建的文件
        let untracked_list: Vec<PathBuf> = self.get_untracked_files();
        let untracked_path = snap_dir.join(".claw-untracked-list");
        if let Ok(untracked_json) = serde_json::to_string(&untracked_list) {
            let _ = fs::write(&untracked_path, untracked_json);
        }

        self.snapshot_dir = Some(snap_dir);
        self.has_snapshot = true;
        Ok(())
    }

    /// Restore files from file snapshot.
    ///
    /// P1-3 修复：恢复已删除文件（删除 turn 内新建的同名文件），
    /// 清理 turn 内新建的 untracked 文件。
    fn file_restore(&mut self) -> Result<String, VcsError> {
        let Some(ref snap_dir) = self.snapshot_dir else {
            return Ok("rollback: no file snapshot to restore from".into());
        };

        let mut restored = 0;
        for file in &self.modified_files {
            let src = snap_dir.join(file);
            let dst = self.root.join(file);
            if src.exists() {
                // pre_turn 时文件存在，恢复它
                if let Some(parent) = dst.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                fs::copy(&src, &dst)
                    .map_err(|e| VcsError::FileSnapshotFailed(format!("restore: {e}")))?;
                restored += 1;
            } else {
                // pre_turn 时文件不存在（已删除），如果 turn 内重新创建了则删除它
                if dst.exists() {
                    let _ = fs::remove_file(&dst);
                }
            }
        }

        // 清理 turn 内新建的 untracked 文件（不在 pre_turn untracked 列表中的）
        let untracked_path = snap_dir.join(".claw-untracked-list");
        let pre_turn_untracked: Vec<PathBuf> = fs::read_to_string(&untracked_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let current_untracked = self.get_untracked_files();
        for file in &current_untracked {
            if !pre_turn_untracked.contains(file) && !self.modified_files.contains(file) {
                let full_path = self.root.join(file);
                if full_path.exists() {
                    let _ = fs::remove_file(&full_path);
                }
            }
        }

        // Clean up snapshot dir
        let _ = fs::remove_dir_all(snap_dir);

        self.has_snapshot = false;
        self.modified_files.clear();
        Ok(format!(
            "rollback: restored {restored} files from file snapshot"
        ))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse `git status --porcelain` output into modified file paths.
///
/// P2-3 修复：
/// - 处理重命名格式 `R  old -> new`（取 new 路径）
/// - 剥离含特殊字符路径的引号 `"..."`
/// - 过滤 `.claw/` 管理文件（不应被 stash）
/// - 统一路径分隔符为 POSIX 风格（避免 Windows 路径混用导致去重失效）
fn parse_modified_files(status_output: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for line in status_output.lines() {
        if line.len() < 3 {
            continue;
        }
        let status_code = &line[..2];
        let path_part = &line[3..];

        // 处理重命名：`R  old_path -> new_path`，取 new_path
        let path_str = if status_code.starts_with('R') || status_code.starts_with('C') {
            // 重命名/复制格式：`old -> new`
            if let Some(arrow_pos) = path_part.find(" -> ") {
                &path_part[arrow_pos + 4..]
            } else {
                path_part
            }
        } else {
            path_part
        };

        // 剥离引号（git status --porcelain 对含特殊字符的路径用 "..." 包裹）
        let path_str =
            if path_str.starts_with('"') && path_str.ends_with('"') && path_str.len() >= 2 {
                &path_str[1..path_str.len() - 1]
            } else {
                path_str
            };

        // 过滤 .claw/ 管理文件
        if path_str.starts_with(".claw/") || path_str.starts_with(".claw\\") {
            continue;
        }

        // 统一路径分隔符为 POSIX 风格（解决 Windows 路径混用问题）
        let normalized = path_str.replace('\\', "/");
        let path = PathBuf::from(normalized);

        // 去重（PathBuf 比较是字节级的，统一为 POSIX 后可正确去重）
        if !files.contains(&path) {
            files.push(path);
        }
    }
    files
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_transaction_detects_non_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let tx = RefactorTransaction::new(dir.path().to_path_buf());
        assert!(!tx.is_git_repo);
        assert_eq!(tx.status().state, "disabled");
    }

    #[test]
    fn new_transaction_detects_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        // Init a git repo
        Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let tx = RefactorTransaction::new(dir.path().to_path_buf());
        assert!(tx.is_git_repo);
        // Fresh git init without commits: HEAD is detached
        assert!(tx.status().state == "idle" || tx.status().state == "detached");
    }

    #[test]
    fn pre_turn_snapshot_in_non_git_repo_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut tx = RefactorTransaction::new(dir.path().to_path_buf());
        let result = tx.pre_turn_snapshot("test-turn");
        assert!(result.is_ok());
        assert!(!tx.has_snapshot);
    }

    #[test]
    fn pre_turn_snapshot_with_clean_tree() {
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        // Configure git user for stash
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        // Create and commit a file so working tree is clean-ish
        fs::write(dir.path().join("f.txt"), "hello").unwrap();
        Command::new("git")
            .args(["add", "f.txt"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let mut tx = RefactorTransaction::new(dir.path().to_path_buf());
        let result = tx.pre_turn_snapshot("test-turn");
        assert!(result.is_ok());
    }

    #[test]
    fn mark_dirty_tracks_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut tx = RefactorTransaction::new(dir.path().to_path_buf());
        tx.mark_dirty(&[PathBuf::from("src/main.rs"), PathBuf::from("Cargo.toml")]);
        assert_eq!(tx.modified_files.len(), 2);
        // Deduplication
        tx.mark_dirty(&[PathBuf::from("src/main.rs")]);
        assert_eq!(tx.modified_files.len(), 2);
    }

    #[test]
    fn rollback_without_snapshot_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut tx = RefactorTransaction::new(dir.path().to_path_buf());
        let result = tx.rollback().unwrap();
        assert!(result.contains("no snapshot"));
    }

    #[test]
    fn status_serializes_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let tx = RefactorTransaction::new(dir.path().to_path_buf());
        let status = tx.status();
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("disabled"));
    }

    #[test]
    fn parse_modified_files_parses_porcelain() {
        let output = " M src/main.rs\n?? new_file.txt\n M Cargo.toml\n";
        let files = parse_modified_files(output);
        assert!(files.contains(&PathBuf::from("src/main.rs")));
        assert!(files.contains(&PathBuf::from("new_file.txt")));
        assert!(files.contains(&PathBuf::from("Cargo.toml")));
        assert_eq!(files.len(), 3);
    }
}
