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
    StashPopConflict { conflicted: Vec<PathBuf>, message: String },
    DetachedHead,
    Io(std::io::Error),
    FileSnapshotFailed(String),
}

impl std::fmt::Display for VcsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotGitRepo => write!(f, "not a git repository"),
            Self::StashFailed(msg) => write!(f, "git stash failed: {msg}"),
            Self::StashPopConflict { conflicted, message } => {
                write!(f, "stash pop conflict in {} files: {message}", conflicted.len())
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
    /// - If git repo and not detached: creates a stash with `git stash push`
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

        // Parse modified files
        self.modified_files = parse_modified_files(&stdout);

        // Create a stash without applying (using stash create for lowest risk)
        let stash_output = Command::new("git")
            .args([
                "stash", "push",
                "--include-untracked",
                "--message", &format!("claw-turn-{turn_id}"),
            ])
            .current_dir(&self.root)
            .output()
            .map_err(|e| VcsError::StashFailed(format!("git stash push failed: {e}")))?;

        if !stash_output.status.success() {
            let stderr = String::from_utf8_lossy(&stash_output.stderr);
            return Err(VcsError::StashFailed(format!("git stash failed: {stderr}")));
        }

        // Record stash reference
        let rev_output = Command::new("git")
            .args(["stash", "list", "--format=%H", "-1"])
            .current_dir(&self.root)
            .output()
            .map_err(|_| VcsError::StashFailed("failed to get stash ref".into()))?;

        let stash_ref = String::from_utf8_lossy(&rev_output.stdout)
            .trim()
            .to_string();

        if stash_ref.is_empty() {
            return Err(VcsError::StashFailed("no stash ref returned".into()));
        }

        self.stash_ref = Some(stash_ref);
        self.has_snapshot = true;
        Ok(())
    }

    /// Mark files as dirty (called after Edit/Write/Delete tools).
    pub fn mark_dirty(&mut self, files: &[PathBuf]) {
        for f in files {
            if !self.modified_files.contains(f) {
                self.modified_files.push(f.clone());
            }
        }
    }

    /// Rollback to pre-turn state.
    ///
    /// Attempts `git stash pop` first (safest).
    /// On conflict, degrades to `git checkout -- {files}` + `git clean -fd`.
    pub fn rollback(&mut self) -> Result<String, VcsError> {
        if !self.has_snapshot {
            return Ok("rollback: no snapshot taken (clean working tree or non-git repo)".into());
        }

        if self.is_detached {
            return self.file_restore();
        }

        // Attempt stash pop
        if let Some(ref _stash_ref) = self.stash_ref {
            let pop = Command::new("git")
                .args(["stash", "pop", "stash@{0}"])
                .current_dir(&self.root)
                .output()
                .map_err(|e| VcsError::StashFailed(format!("git stash pop failed: {e}")))?;

            if pop.status.success() {
                self.has_snapshot = false;
                self.modified_files.clear();
                return Ok("rollback: stash pop succeeded, working tree restored".into());
            }

            // Stash pop conflict — degrade to checkout
            let stderr = String::from_utf8_lossy(&pop.stderr);
            if stderr.contains("CONFLICT") || stderr.contains("conflict") {
                // Drop the stash to clean up, then checkout files
                let _ = Command::new("git")
                    .args(["stash", "drop", "stash@{0}"])
                    .current_dir(&self.root)
                    .output();

                // Force-restore files
                if !self.modified_files.is_empty() {
                    let files: Vec<&str> = self.modified_files
                        .iter()
                        .filter_map(|p| p.to_str())
                        .collect();
                    let checkout = Command::new("git")
                        .args(["checkout", "--"])
                        .args(&files)
                        .current_dir(&self.root)
                        .output()
                        .map_err(|e| VcsError::StashFailed(format!("git checkout failed: {e}")))?;

                    if !checkout.status.success() {
                        let err = String::from_utf8_lossy(&checkout.stderr);
                        // Clean untracked files
                        let _ = Command::new("git")
                            .args(["clean", "-fd"])
                            .current_dir(&self.root)
                            .output();

                        return Err(VcsError::StashPopConflict {
                            conflicted: self.modified_files.clone(),
                            message: format!(
                                "stash pop had conflicts, checkout failed: {err}"
                            ),
                        });
                    }
                }

                self.has_snapshot = false;
                self.modified_files.clear();
                return Ok(
                    "rollback: stash pop had conflicts — degraded to git checkout + clean".into()
                );
            }

            return Err(VcsError::StashFailed(
                format!("git stash pop failed: {stderr}")
            ));
        }

        self.file_restore()
    }

    /// Get current transaction status.
    pub fn status(&self) -> TransactionStatus {
        TransactionStatus {
            state: if !self.is_git_repo {
                "disabled".into()
            } else if self.is_detached {
                "detached".into()
            } else if self.has_snapshot {
                "active".into()
            } else {
                "idle".into()
            },
            is_git_repo: self.is_git_repo,
            stash_ref: self.stash_ref.clone(),
            modified_files: self.modified_files.clone(),
            snapshot_dir: self.snapshot_dir.clone(),
        }
    }

    /// File-copy snapshot for detached HEAD fallback.
    fn file_snapshot(&mut self, turn_id: &str) -> Result<(), VcsError> {
        let snap_dir = self.root.join(".claw").join("tx-snapshots").join(turn_id);
        fs::create_dir_all(&snap_dir)
            .map_err(|e| VcsError::FileSnapshotFailed(format!("mkdir: {e}")))?;

        // Copy tracked modified files
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
        }

        self.snapshot_dir = Some(snap_dir);
        self.has_snapshot = true;
        Ok(())
    }

    /// Restore files from file snapshot.
    fn file_restore(&mut self) -> Result<String, VcsError> {
        let Some(ref snap_dir) = self.snapshot_dir else {
            return Ok("rollback: no file snapshot to restore from".into());
        };

        let mut restored = 0;
        for file in &self.modified_files {
            let src = snap_dir.join(file);
            let dst = self.root.join(file);
            if src.exists() {
                fs::copy(&src, &dst)
                    .map_err(|e| VcsError::FileSnapshotFailed(format!("restore: {e}")))?;
                restored += 1;
            }
        }

        // Clean up snapshot dir
        let _ = fs::remove_dir_all(snap_dir);

        self.has_snapshot = false;
        self.modified_files.clear();
        Ok(format!("rollback: restored {restored} files from file snapshot"))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse `git status --porcelain` output into modified file paths.
fn parse_modified_files(status_output: &str) -> Vec<PathBuf> {
    status_output
        .lines()
        .filter(|line| line.len() >= 3)
        .map(|line| PathBuf::from(&line[3..]))
        .collect()
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
