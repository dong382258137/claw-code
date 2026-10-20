//! 进化文件锁(P4)—— 防多实例并发 `cargo build` / exe 覆盖竞争。
//!
//! 设计见 `docs/2026-10-11-metacognitive-evolution-design.md` §3.5 / §5 约束 2。
//!
//! **不引入依赖**:用 `OpenOptions::new().create_new(true)` 的原子性实现锁文件
//! (创建成功=获得锁;已存在=锁被占)。锁内写入持有者 pid + 时间戳,便于诊断;
//! 若锁文件超过 [`STALE_LOCK_MS`] 视为陈旧(持有者已崩溃),自动回收。
//!
//! 释放:删除锁文件(`Drop` 自动清理)。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 锁被视为陈旧的阈值(ms,默认 1 小时):超过则自动回收(防持有者崩溃后死锁)。
pub const STALE_LOCK_MS: i64 = 60 * 60 * 1000;

/// 一个已持有的锁;`Drop` 时自动删除锁文件。
#[derive(Debug)]
pub struct EvolutionLock {
    path: PathBuf,
}

impl EvolutionLock {
    /// 尝试获取 `source_root/.claw/evolution/.lock`。
    ///
    /// 返回 `Ok(Some(lock))` = 获得;`Ok(None)` = 已被他人持有(非陈旧);
    /// `Err` = IO 错误。陈旧锁会被自动回收并重试一次。
    pub fn acquire(source_root: &Path) -> Result<Option<Self>, String> {
        let dir = source_root.join(".claw").join("evolution");
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败: {e}", dir.display()))?;
        let path = dir.join(".lock");
        match Self::try_create(&path) {
            Ok(()) => Ok(Some(Self { path })),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // 已存在:检查是否陈旧。
                if Self::is_stale(&path) {
                    let _ = std::fs::remove_file(&path);
                    match Self::try_create(&path) {
                        Ok(()) => Ok(Some(Self { path })),
                        Err(_) => Ok(None),
                    }
                } else {
                    Ok(None)
                }
            }
            Err(e) => Err(format!("创建锁 {} 失败: {e}", path.display())),
        }
    }

    fn try_create(path: &Path) -> std::io::Result<()> {
        let mut f = OpenOptions::new().write(true).create_new(true).open(path)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        writeln!(f, "pid={} at_ms={now}", std::process::id())?;
        Ok(())
    }

    fn is_stale(path: &Path) -> bool {
        let Ok(text) = std::fs::read_to_string(path) else {
            return false;
        };
        let Some(at) = text
            .split_whitespace()
            .find_map(|kv| kv.strip_prefix("at_ms="))
            .and_then(|v| v.parse::<i64>().ok())
        else {
            return true; // 内容不合法 → 视为陈旧
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        now.saturating_sub(at) > STALE_LOCK_MS
    }

    /// 锁文件路径(诊断用)。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 锁当前是否被他人持有(陈旧锁视为未持有)。
    ///
    /// 供「是否拉起自动进化」判断用:只是**查询**不获取锁,否则调用方会把锁
    /// 拿在手里却并不进化,反而挡住真正的进化进程。
    #[must_use]
    pub fn is_held(source_root: &Path) -> bool {
        let path = source_root.join(".claw").join("evolution").join(".lock");
        if !path.exists() {
            return false;
        }
        !Self::is_stale(&path)
    }
}

impl Drop for EvolutionLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_then_second_fails_until_released() {
        let root = std::env::temp_dir().join(format!("claw-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        {
            let first = EvolutionLock::acquire(&root)
                .expect("acquire")
                .expect("some");
            // 第二个应失败(已被持有)。
            let second = EvolutionLock::acquire(&root).expect("acquire2");
            assert!(second.is_none(), "锁被持有时第二个应为 None");
            drop(first);
        }
        // 释放后可再获取。
        let again = EvolutionLock::acquire(&root)
            .expect("acquire3")
            .expect("some");
        drop(again);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_lock_is_reclaimed() {
        let root = std::env::temp_dir().join(format!("claw-lock-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join(".claw").join("evolution");
        std::fs::create_dir_all(&dir).unwrap();
        // 写一个很旧的锁。
        std::fs::write(dir.join(".lock"), "pid=1 at_ms=1000\n").unwrap();

        let lock = EvolutionLock::acquire(&root)
            .expect("acquire")
            .expect("应回收陈旧锁");
        assert!(lock.path().exists());
        drop(lock);
        let _ = std::fs::remove_dir_all(&root);
    }
}
