//! Epic 4: 文件操作权限隔离 — SubagentFileGuard。
//!
//! 基于 `SubagentCapability` 限制文件写入,防止并行子智能体冲突。
//!
//! # 设计(见 docs/2026-08-06-subagent-trae-alignment-design.md §4)
//!
//! - `Analyze`/`ReadOnly`:直接拒绝 edit/write(capability 白名单已排除,此为二次防护)
//! - `Execute`:写入前获取每文件独立锁,已锁定则等待(30s 超时后 Err)
//! - 全局锁注册表:进程级 `OnceLock`,跨子智能体共享
//! - 路径规范化:`workspace_root.join` + `canonicalize`,fallback 手动去 `\\?\` 前缀
//!
//! # 锁粒度
//!
//! 每文件独立锁(`Arc<LockInner>`),避免全局 Mutex 竞争。
//! 外层 `Mutex<HashMap>` 仅在查找/插入锁条目时短暂持有,实际文件写入保护
//! 由 `AtomicBool` 标记 + `Condvar` 通知提供。
//!
//! # 无 unsafe 设计
//!
//! 用 `AtomicBool` 标记锁状态(替代 `MutexGuard`),`LockHandle` 持有 `Arc<LockInner>`,
//! Drop 时设置 `AtomicBool=false` 并 `notify_one`。避免 `MutexGuard<'static>` 的
//! 生命周期提升(需 unsafe transmute),满足项目 `-D unsafe-code` 约束。

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use crate::multi_agent::SubagentCapability;

/// 默认文件锁超时(秒)。可通过 `CLAW_SUBAGENT_FILE_LOCK_TIMEOUT` 环境变量覆盖。
const DEFAULT_LOCK_TIMEOUT_SECS: u64 = 30;

/// 环境变量名:文件锁超时(秒)。
const LOCK_TIMEOUT_ENV: &str = "CLAW_SUBAGENT_FILE_LOCK_TIMEOUT";

/// 获取文件锁超时时长(从环境变量读取,默认 30s)。
fn lock_timeout() -> Duration {
    env::var(LOCK_TIMEOUT_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&secs| secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_LOCK_TIMEOUT_SECS))
}

/// per-file 锁条目:AtomicBool 标记锁状态 + Condvar 通知等待者 + Mutex 供 Condvar 等待。
struct LockInner {
    /// 锁状态:false=空闲,true=已获取。用 AtomicBool 避免返回 MutexGuard
    /// (MutexGuard 的生命周期绑定到 &Mutex,跨函数返回需 unsafe transmute,被项目禁止)。
    locked: AtomicBool,
    /// 通知等待者:锁释放时 notify_one
    cvar: Condvar,
    /// 仅用于 Condvar.wait_timeout 的关联 Mutex(不保护任何数据,仅满足 Condvar API 约束)
    wait_mutex: Mutex<()>,
}

impl LockInner {
    fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            cvar: Condvar::new(),
            wait_mutex: Mutex::new(()),
        }
    }
}

/// 进程级全局锁注册表。
///
/// `OnceLock` 确保整个进程共享同一份锁表,跨子智能体(无论哪个 dispatcher)
/// 均使用同一注册表。`HashMap<PathBuf, Arc<LockInner>>` 的 key 是规范化后的绝对路径。
type LockRegistry = Arc<Mutex<HashMap<PathBuf, Arc<LockInner>>>>;

static LOCK_REGISTRY: OnceLock<LockRegistry> = OnceLock::new();

fn registry() -> &'static LockRegistry {
    LOCK_REGISTRY.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

/// 文件操作权限守卫 — 基于 capability 限制文件写入,防止并行冲突。
///
/// 每个 `SubagentDispatcher` 持有一个 `SubagentFileGuard` 实例,所有实例
/// 共享同一个进程级锁注册表(`OnceLock`)。
///
/// # 使用方式
///
/// ```ignore
/// let guard = SubagentFileGuard::new(capability, workspace_root);
/// let _lock = guard.try_acquire(path, true)?;  // write=true
/// // 执行 edit/write 操作...
/// // lock 在 _lock drop 时自动释放
/// ```
#[derive(Clone)]
pub struct SubagentFileGuard {
    capability: SubagentCapability,
    workspace_root: PathBuf,
}

/// 锁句柄 — RAII,持有期间保持锁,drop 时自动释放并通知等待者。
///
/// `entry=None` 表示读操作(无锁);`entry=Some` 表示写操作(持有 per-file 锁)。
pub struct LockHandle {
    entry: Option<Arc<LockInner>>,
}

// 手动实现 Debug(LockInner 无 Debug,Arc<LockInner> 无法 derive)。
// 测试中 `Result::unwrap_err` 需要 `T: Debug`,读 handle 打印 "read-noop",
// 写 handle 打印 "write-locked"。
impl std::fmt::Debug for LockHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.entry {
            None => f.write_str("LockHandle(read-noop)"),
            Some(_) => f.write_str("LockHandle(write-locked)"),
        }
    }
}

// Drop:写锁释放时设置 AtomicBool=false 并 notify_one 唤醒等待者。
// 读锁(entry=None)无操作。
impl Drop for LockHandle {
    fn drop(&mut self) {
        if let Some(entry) = &self.entry {
            entry.locked.store(false, Ordering::Release);
            entry.cvar.notify_one();
        }
    }
}

impl SubagentFileGuard {
    #[must_use]
    pub fn new(capability: SubagentCapability, workspace_root: PathBuf) -> Self {
        Self {
            capability,
            workspace_root,
        }
    }

    /// 尝试获取文件锁。
    ///
    /// # 参数
    /// - `path`:文件路径(相对或绝对)
    /// - `write`:是否为写锁(edit/write = true,read = false)
    ///
    /// # 返回
    /// - `Ok(LockHandle)`:获取成功,drop 时自动释放
    /// - `Err(String)`:拒绝或超时
    ///
    /// # 行为
    /// - `write=false`:始终成功(读不需要锁,返回空 handle)
    /// - `write=true` + `Analyze`/`ReadOnly`:拒绝(capability 白名单二次防护)
    /// - `write=true` + `Execute`:获取 per-file 锁,超时返回 Err
    pub fn try_acquire(&self, path: &Path, write: bool) -> Result<LockHandle, String> {
        // 读操作不需要锁,返回空 handle
        if !write {
            return Ok(LockHandle { entry: None });
        }

        // 写操作:capability 二次防护
        if !matches!(self.capability, SubagentCapability::Execute) {
            return Err(format!(
                "write denied: capability {:?} cannot modify files (only Execute can)",
                self.capability
            ));
        }

        let normalized = normalize_path(path, &self.workspace_root);
        let timeout = lock_timeout();

        // 从全局注册表获取 per-file 锁条目
        let entry = {
            let mut reg = registry()
                .lock()
                .expect("file guard registry lock poisoned");
            reg.entry(normalized)
                .or_insert_with(|| Arc::new(LockInner::new()))
                .clone()
        };

        // 尝试获取锁:compare_exchange(false→true),失败则等待 Condvar 通知或超时
        loop {
            if entry
                .locked
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(LockHandle { entry: Some(entry) });
            }

            // 锁被其他子智能体持有,等待 Condvar 通知或超时
            let guard = match entry.wait_mutex.lock() {
                Ok(g) => g,
                Err(e) => return Err(format!("file lock wait_mutex poisoned: {e}")),
            };
            let (_waited_guard, timeout_result) = entry
                .cvar
                .wait_timeout(guard, timeout)
                .unwrap_or_else(|e| e.into_inner());
            if timeout_result.timed_out() {
                return Err(format!(
                    "file lock timeout ({}s): {}",
                    timeout.as_secs(),
                    path.display()
                ));
            }
            // 被唤醒(notify_one),循环重试 compare_exchange
        }
    }
}

/// 规范化路径:相对路径 → 绝对路径,`canonicalize` 成功则用规范化结果,
/// 失败则手动去除 Windows `\\?\` 前缀作为 fallback。
///
/// 与 `handoff.rs::normalize_path` 一致的策略,但此处用于锁 key。
fn normalize_path(path: &Path, workspace_root: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_root.join(path)
    };

    // 尝试 canonicalize
    if let Ok(canon) = std::fs::canonicalize(&abs) {
        return strip_verbatim_prefix(&canon);
    }

    // fallback:手动去 `\\?\` 前缀(Windows canonicalize 不成功时)
    strip_verbatim_prefix(&abs)
}

/// 去除 Windows `\\?\` 前缀(dunce::simplified 的简化替代)。
///
/// Windows canonicalize 返回 `\\?\C:\path\to\file` 形式,
/// 去除 `\\?\` 前缀使路径与 git diff / 用户输入一致。
#[cfg(windows)]
fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        path.to_path_buf()
    }
}

#[cfg(not(windows))]
fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn tmp_workspace() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn analyze_write_denied() {
        let tmp = tmp_workspace();
        let guard = SubagentFileGuard::new(SubagentCapability::Analyze, tmp.path().to_path_buf());

        let result = guard.try_acquire(Path::new("src/foo.rs"), true);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("write denied"));
        assert!(err.contains("Analyze"));
    }

    #[test]
    fn readonly_write_denied() {
        let tmp = tmp_workspace();
        let guard = SubagentFileGuard::new(SubagentCapability::ReadOnly, tmp.path().to_path_buf());

        let result = guard.try_acquire(Path::new("src/foo.rs"), true);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("write denied"));
    }

    #[test]
    fn execute_write_succeeds() {
        let tmp = tmp_workspace();
        let guard = SubagentFileGuard::new(SubagentCapability::Execute, tmp.path().to_path_buf());

        let result = guard.try_acquire(Path::new("src/foo.rs"), true);
        assert!(result.is_ok(), "Execute should acquire write lock");
        // LockHandle drop 时释放锁
        drop(result);
    }

    #[test]
    fn read_always_succeeds() {
        let tmp = tmp_workspace();
        let guard = SubagentFileGuard::new(SubagentCapability::Analyze, tmp.path().to_path_buf());

        let result = guard.try_acquire(Path::new("src/foo.rs"), false);
        assert!(result.is_ok(), "read should always succeed");
    }

    #[test]
    fn lock_release_allows_reacquire() {
        let tmp = tmp_workspace();
        let guard = SubagentFileGuard::new(SubagentCapability::Execute, tmp.path().to_path_buf());

        // 第一次获取
        let lock1 = guard.try_acquire(Path::new("src/foo.rs"), true);
        assert!(lock1.is_ok());

        // 释放
        drop(lock1);

        // 第二次获取(同一线程,应立即成功)
        let lock2 = guard.try_acquire(Path::new("src/foo.rs"), true);
        assert!(lock2.is_ok(), "should reacquire after release");
    }

    #[test]
    fn concurrent_write_different_files_succeeds() {
        let tmp = tmp_workspace();
        let workspace = tmp.path().to_path_buf();

        let guard1 = SubagentFileGuard::new(SubagentCapability::Execute, workspace.clone());
        let guard2 = SubagentFileGuard::new(SubagentCapability::Execute, workspace);

        let file1 = Arc::new(PathBuf::from("src/a.rs"));
        let file2 = Arc::new(PathBuf::from("src/b.rs"));

        let g1 = guard1.clone();
        let f1 = file1.clone();
        let h1 = thread::spawn(move || g1.try_acquire(&f1, true).is_ok());

        let g2 = guard2.clone();
        let f2 = file2.clone();
        let h2 = thread::spawn(move || g2.try_acquire(&f2, true).is_ok());

        assert!(h1.join().unwrap(), "file1 lock should succeed");
        assert!(h2.join().unwrap(), "file2 lock should succeed");
    }

    #[test]
    fn concurrent_write_same_file_second_waits() {
        let tmp = tmp_workspace();
        let workspace = tmp.path().to_path_buf();

        // 创建测试文件(canonicalize 需要文件存在)
        let test_file = workspace.join("src").join("shared.rs");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(&test_file, "// test").unwrap();

        let guard1 = SubagentFileGuard::new(SubagentCapability::Execute, workspace.clone());
        let guard2 = SubagentFileGuard::new(SubagentCapability::Execute, workspace);

        let file = Arc::new(PathBuf::from("src/shared.rs"));

        // 第一个线程获取锁并持有
        let g1 = guard1.clone();
        let f1 = file.clone();
        let h1 = thread::spawn(move || {
            let lock = g1.try_acquire(&f1, true);
            assert!(lock.is_ok());
            // 持有锁 200ms
            thread::sleep(Duration::from_millis(200));
            drop(lock);
        });

        // 短暂等待确保 h1 先获取锁
        thread::sleep(Duration::from_millis(50));

        // 第二个线程尝试获取同一文件锁 — 应等待 h1 释放后成功
        let g2 = guard2.clone();
        let f2 = file.clone();
        let h2 = thread::spawn(move || g2.try_acquire(&f2, true).is_ok());

        h1.join().unwrap();
        let result2 = h2.join().unwrap();
        assert!(result2, "second lock should succeed after first releases");
    }

    #[test]
    fn path_normalization_relative_and_absolute_match() {
        let tmp = tmp_workspace();
        let workspace = tmp.path().to_path_buf();

        // 创建测试文件
        let rel = "src/foo.rs";
        let abs = workspace.join(rel);
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(&abs, "// test").unwrap();

        let guard = SubagentFileGuard::new(SubagentCapability::Execute, workspace.clone());

        // 用相对路径获取锁
        let lock1 = guard.try_acquire(Path::new(rel), true);
        assert!(lock1.is_ok());

        // 验证 normalize_path 产生相同的 key(相对/绝对路径归一)
        let norm_rel = normalize_path(Path::new(rel), &workspace);
        let norm_abs = normalize_path(&abs, &workspace);
        assert_eq!(
            norm_rel, norm_abs,
            "relative and absolute paths should normalize to the same key"
        );

        drop(lock1);
    }

    #[test]
    fn lock_timeout_env_var() {
        // 临时设置超时为 1 秒
        env::set_var(LOCK_TIMEOUT_ENV, "1");
        assert_eq!(lock_timeout(), Duration::from_secs(1));

        // 清理
        env::remove_var(LOCK_TIMEOUT_ENV);
        assert_eq!(
            lock_timeout(),
            Duration::from_secs(DEFAULT_LOCK_TIMEOUT_SECS)
        );
    }

    #[test]
    fn lock_timeout_invalid_env_falls_back_to_default() {
        env::set_var(LOCK_TIMEOUT_ENV, "not-a-number");
        assert_eq!(
            lock_timeout(),
            Duration::from_secs(DEFAULT_LOCK_TIMEOUT_SECS)
        );

        env::set_var(LOCK_TIMEOUT_ENV, "0");
        assert_eq!(
            lock_timeout(),
            Duration::from_secs(DEFAULT_LOCK_TIMEOUT_SECS),
            "0 should fall back to default"
        );

        env::remove_var(LOCK_TIMEOUT_ENV);
    }

    /// 验证 AtomicBool 锁状态在 Drop 后正确重置(回归测试)。
    #[test]
    fn lock_state_resets_after_drop() {
        let tmp = tmp_workspace();
        let workspace = tmp.path().to_path_buf();
        let guard = SubagentFileGuard::new(SubagentCapability::Execute, workspace.clone());

        let path = Path::new("src/state_test.rs");
        let lock = guard
            .try_acquire(path, true)
            .expect("acquire should succeed");

        // 获取内部 entry 检查 locked 状态
        let normalized = normalize_path(path, &workspace);
        let reg = registry().lock().unwrap();
        let entry = reg.get(&normalized).expect("entry should exist");
        assert!(
            entry.locked.load(Ordering::Acquire),
            "locked should be true while LockHandle is alive"
        );
        drop(reg);

        drop(lock);

        // Drop 后 locked 应为 false
        let reg = registry().lock().unwrap();
        let entry = reg.get(&normalized).expect("entry should still exist");
        assert!(
            !entry.locked.load(Ordering::Acquire),
            "locked should be false after LockHandle dropped"
        );
    }
}
