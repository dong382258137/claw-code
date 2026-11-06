//! S3-M2：会话级子进程登记与兜底清理。
//!
//! 目标：worker 退出、看门狗回收、会话删除时，能把该会话启动的工具子进程
//! 树（bash 前台/后台）一并终止，避免「停止后孤儿进程继续跑」。
//!
//! 实现说明：Windows 上子进程已经被现有的全局 sandbox Job Object 接管，
//! 再创建「每会话 Job Object」会因嵌套 Job 指派失败（实测
//! `AssignProcessToJobObject failed`）并反过来破坏 sandbox 隔离。因此本轮
//! 采用 PID 登记 + 现有 `kill_process_tree`（按父进程递归终止整棵树）作为
//! 主路径；Job Object 仍由 sandbox 层负责其原有语义。
//!
//! 该结构按会话创建、跨轮复用：同一会话的多个 turn 共享一份登记表，
//! 只有会话级清理（worker 退出 / 看门狗回收 / 删除会话）才会整体终止。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 一个会话的子进程登记表。
#[derive(Debug, Default)]
pub struct SessionProcesses {
    pids: Mutex<HashSet<u32>>,
    /// 会话级清理是否已执行（幂等；清理后的会话不再接受新登记）。
    closed: AtomicBool,
}

impl SessionProcesses {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 登记刚 spawn 的子进程。
    ///
    /// 若会话已被清理（`closed`），立即终止该进程树，避免清理后新起的进程
    /// 逃逸出会话边界。
    pub fn register(self: &Arc<Self>, pid: u32) {
        if self.closed.load(Ordering::SeqCst) {
            crate::bash::kill_process_tree(pid);
            return;
        }
        self.pids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(pid);
    }

    /// 子进程自然结束后摘除登记，避免 PID 复用后误杀。
    pub fn unregister(&self, pid: u32) {
        self.pids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&pid);
    }

    /// 终止该会话登记的所有子进程树。幂等，可重复调用。
    pub fn terminate_all(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let pids: Vec<u32> = self
            .pids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
            .collect();
        for pid in pids {
            crate::bash::kill_process_tree(pid);
        }
    }

    /// 供测试/诊断：当前登记且未摘除的 PID 数量。
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.pids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminate_all_is_idempotent_and_marks_closed() {
        let processes = SessionProcesses::new();
        // 使用不存在的 PID，避免测试真的终止测试进程树。
        processes.register(999_999);
        assert_eq!(processes.tracked(), 1);
        processes.terminate_all();
        assert!(processes.closed.load(Ordering::SeqCst));
        assert_eq!(processes.tracked(), 0);
        processes.terminate_all();
    }
}
