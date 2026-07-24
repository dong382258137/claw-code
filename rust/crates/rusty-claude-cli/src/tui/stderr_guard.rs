//! StderrGuard — TUI 模式下的 stderr 边界防护。
//!
//! 在 TUI alternate screen 期间，所有写入 stderr 的内容（包括
//! `eprintln!`、第三方库日志、子进程继承的 stderr 等）都会被
//! 重定向到一个 pipe，由后台 drain 线程收集到 buffer 中。
//! TUI 退出时，buffer 内容会被 flush 回真正的 stderr，确保
//! 任何错误信息都不会丢失。
//!
//! 设计原则（第一性原理）：
//! - 不在每个 `eprintln!` 调用点修补，而是在系统边界（TUI 入口）
//!   一次性切断所有泄漏通道
//! - Drop guard 保证任何退出路径（正常返回、Err、panic）都会恢复
//!   原始 stderr

use std::io::{self, Read, Write};
use std::thread;

/// TUI 期间的 stderr 守护。
///
/// 创建时将 stderr 重定向到匿名 pipe，销毁时恢复并 flush
/// 收集到的内容到原始 stderr。
pub(crate) struct StderrGuard {
    #[cfg(windows)]
    inner: WindowsGuard,
    #[cfg(unix)]
    inner: UnixGuard,
}

impl StderrGuard {
    /// 创建守护：保存原始 stderr，创建 pipe 并重定向，启动 drain 线程。
    pub(crate) fn new() -> io::Result<Self> {
        #[cfg(windows)]
        {
            WindowsGuard::new().map(|inner| StderrGuard { inner })
        }
        #[cfg(unix)]
        {
            UnixGuard::new().map(|inner| StderrGuard { inner })
        }
    }
}

impl Drop for StderrGuard {
    fn drop(&mut self) {
        // 内部 guard 的 drop 会自动恢复并 flush
    }
}

// ── Windows 实现 ──────────────────────────────────────────────

#[cfg(windows)]
#[allow(clippy::upper_case_acronyms)]
mod win32 {
    use std::ffi::c_void;
    pub type HANDLE = *mut c_void;
    pub type BOOL = i32;
    pub type DWORD = u32;
    pub const STD_ERROR_HANDLE: DWORD = u32::MAX - 11; // 0xFFFFFFF4
    pub const INVALID_HANDLE_VALUE: HANDLE = usize::MAX as HANDLE;

    extern "system" {
        pub fn GetStdHandle(nStdHandle: DWORD) -> HANDLE;
        pub fn SetStdHandle(nStdHandle: DWORD, hHandle: HANDLE) -> BOOL;
        pub fn CreatePipe(
            hReadPipe: *mut HANDLE,
            hWritePipe: *mut HANDLE,
            lpPipeAttributes: *mut c_void,
            nSize: DWORD,
        ) -> BOOL;
        pub fn CloseHandle(hObject: HANDLE) -> BOOL;
        pub fn ReadFile(
            hFile: HANDLE,
            lpBuffer: *mut c_void,
            nNumberOfBytesToRead: DWORD,
            lpNumberOfBytesRead: *mut DWORD,
            lpOverlapped: *mut c_void,
        ) -> BOOL;
    }
}

/// Win32 HANDLE 的 Send 安全包装。
///
/// Windows 句柄是进程级资源标识符，跨线程传递是安全的（内核对象
/// 由引用计数管理）。使用 usize 而非裸指针以避免 `*mut c_void: !Send`
/// 的编译限制。
#[cfg(windows)]
#[repr(transparent)]
#[derive(Clone, Copy)]
struct SendHandle(usize);
#[cfg(windows)]
unsafe impl Send for SendHandle {}
#[cfg(windows)]
unsafe impl Sync for SendHandle {}

#[cfg(windows)]
impl SendHandle {
    fn as_handle(self) -> win32::HANDLE {
        self.0 as win32::HANDLE
    }
}

#[cfg(windows)]
struct WindowsGuard {
    saved_handle: win32::HANDLE,
    /// 已启动的 drain 线程句柄
    drain: Option<thread::JoinHandle<Vec<u8>>>,
    /// pipe 的 write 端，drop 时关闭以通知 drain 线程结束
    write_end: win32::HANDLE,
}

#[cfg(windows)]
impl WindowsGuard {
    fn new() -> io::Result<Self> {
        let saved_handle = unsafe { win32::GetStdHandle(win32::STD_ERROR_HANDLE) };

        let mut read_end_raw: win32::HANDLE = std::ptr::null_mut();
        let mut write_end: win32::HANDLE = std::ptr::null_mut();

        let ok = unsafe {
            win32::CreatePipe(
                &mut read_end_raw,
                &mut write_end,
                std::ptr::null_mut(),
                0, // default buffer size
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        // 重定向 stderr 到 pipe 的 write 端
        let set_ok = unsafe { win32::SetStdHandle(win32::STD_ERROR_HANDLE, write_end) };
        if set_ok == 0 {
            unsafe {
                win32::CloseHandle(read_end_raw);
                win32::CloseHandle(write_end);
            }
            return Err(io::Error::last_os_error());
        }

        // 启动 drain 线程，从 read_end 读取所有数据
        let read_end = SendHandle(read_end_raw as usize);
        let drain = thread::spawn(move || {
            let read_handle = read_end.as_handle();
            let mut buf = Vec::with_capacity(4096);
            let mut chunk = [0u8; 1024];
            loop {
                let mut bytes_read: win32::DWORD = 0;
                let ok = unsafe {
                    win32::ReadFile(
                        read_handle,
                        chunk.as_mut_ptr() as *mut std::ffi::c_void,
                        chunk.len() as win32::DWORD,
                        &mut bytes_read,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || bytes_read == 0 {
                    break; // pipe closed (write_end dropped) or error
                }
                buf.extend_from_slice(&chunk[..bytes_read as usize]);
            }
            unsafe { win32::CloseHandle(read_handle) };
            buf
        });

        Ok(WindowsGuard {
            saved_handle,
            drain: Some(drain),
            write_end,
        })
    }
}

#[cfg(windows)]
impl Drop for WindowsGuard {
    fn drop(&mut self) {
        // 1. 恢复原始 stderr 句柄
        unsafe {
            win32::SetStdHandle(win32::STD_ERROR_HANDLE, self.saved_handle);
        }
        // 2. 关闭 write_end，通知 drain 线程 pipe 已关闭
        unsafe {
            win32::CloseHandle(self.write_end);
        }
        // 3. 等待 drain 线程结束，flush 收集到的内容
        if let Some(handle) = self.drain.take() {
            match handle.join() {
                Ok(buf) if !buf.is_empty() => {
                    // 写入真正的 stderr
                    let mut real_stderr = io::stderr();
                    let _ = real_stderr.write_all(&buf);
                    let _ = real_stderr.flush();
                }
                Ok(_) => {}  // 空 buffer，无输出
                Err(_) => {} // drain 线程 panic
            }
        }
    }
}

// ── Unix (Linux/macOS) 实现 ───────────────────────────────────

#[cfg(unix)]
struct UnixGuard {
    saved_fd: i32,
    drain: Option<thread::JoinHandle<Vec<u8>>>,
}

#[cfg(unix)]
impl UnixGuard {
    fn new() -> io::Result<Self> {
        let saved_fd = unsafe { libc::dup(libc::STDERR_FILENO) };
        if saved_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
            unsafe { libc::close(saved_fd) };
            return Err(io::Error::last_os_error());
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);

        // 重定向 stderr 到 pipe 的 write 端
        if unsafe { libc::dup2(write_fd, libc::STDERR_FILENO) } < 0 {
            unsafe {
                libc::close(read_fd);
                libc::close(write_fd);
                libc::close(saved_fd);
            }
            return Err(io::Error::last_os_error());
        }
        unsafe { libc::close(write_fd) };

        // drain 线程
        let drain = thread::spawn(move || {
            use std::os::unix::io::FromRawFd;
            let mut file = unsafe { std::fs::File::from_raw_fd(read_fd) };
            let mut buf = Vec::new();
            let _ = file.read_to_end(&mut buf);
            // from_raw_fd 会 consume read_fd，无需手动 close
            buf
        });

        Ok(UnixGuard {
            saved_fd,
            drain: Some(drain),
        })
    }
}

#[cfg(unix)]
impl Drop for UnixGuard {
    fn drop(&mut self) {
        // 1. 恢复原始 stderr fd
        unsafe {
            libc::dup2(self.saved_fd, libc::STDERR_FILENO);
            libc::close(self.saved_fd);
        }
        // 2. 等待 drain 线程，flush 内容
        if let Some(handle) = self.drain.take() {
            match handle.join() {
                Ok(buf) if !buf.is_empty() => {
                    let mut real_stderr = io::stderr();
                    let _ = real_stderr.write_all(&buf);
                    let _ = real_stderr.flush();
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
    }
}
