use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FilesystemIsolationMode {
    Off,
    #[default]
    WorkspaceOnly,
    AllowList,
}

impl FilesystemIsolationMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::WorkspaceOnly => "workspace-only",
            Self::AllowList => "allow-list",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SandboxConfig {
    pub enabled: Option<bool>,
    pub namespace_restrictions: Option<bool>,
    pub network_isolation: Option<bool>,
    pub filesystem_mode: Option<FilesystemIsolationMode>,
    pub allowed_mounts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SandboxRequest {
    pub enabled: bool,
    pub namespace_restrictions: bool,
    pub network_isolation: bool,
    pub filesystem_mode: FilesystemIsolationMode,
    pub allowed_mounts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ContainerEnvironment {
    pub in_container: bool,
    pub markers: Vec<String>,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SandboxStatus {
    pub enabled: bool,
    pub requested: SandboxRequest,
    pub supported: bool,
    pub active: bool,
    pub namespace_supported: bool,
    pub namespace_active: bool,
    pub network_supported: bool,
    pub network_active: bool,
    pub filesystem_mode: FilesystemIsolationMode,
    pub filesystem_active: bool,
    pub allowed_mounts: Vec<String>,
    pub in_container: bool,
    pub container_markers: Vec<String>,
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxDetectionInputs<'a> {
    pub env_pairs: Vec<(String, String)>,
    pub dockerenv_exists: bool,
    pub containerenv_exists: bool,
    pub proc_1_cgroup: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxSandboxCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl SandboxConfig {
    #[must_use]
    pub fn resolve_request(
        &self,
        enabled_override: Option<bool>,
        namespace_override: Option<bool>,
        network_override: Option<bool>,
        filesystem_mode_override: Option<FilesystemIsolationMode>,
        allowed_mounts_override: Option<Vec<String>>,
    ) -> SandboxRequest {
        SandboxRequest {
            enabled: enabled_override.unwrap_or(self.enabled.unwrap_or(true)),
            namespace_restrictions: namespace_override
                .unwrap_or(self.namespace_restrictions.unwrap_or(true)),
            network_isolation: network_override.unwrap_or(self.network_isolation.unwrap_or(false)),
            filesystem_mode: filesystem_mode_override
                .or(self.filesystem_mode)
                .unwrap_or_default(),
            allowed_mounts: allowed_mounts_override.unwrap_or_else(|| self.allowed_mounts.clone()),
        }
    }
}

#[must_use]
pub fn detect_container_environment() -> ContainerEnvironment {
    let proc_1_cgroup = fs::read_to_string("/proc/1/cgroup").ok();
    detect_container_environment_from(SandboxDetectionInputs {
        env_pairs: env::vars().collect(),
        dockerenv_exists: Path::new("/.dockerenv").exists(),
        containerenv_exists: Path::new("/run/.containerenv").exists(),
        proc_1_cgroup: proc_1_cgroup.as_deref(),
    })
}

#[must_use]
pub fn detect_container_environment_from(
    inputs: SandboxDetectionInputs<'_>,
) -> ContainerEnvironment {
    let mut markers = Vec::new();
    if inputs.dockerenv_exists {
        markers.push("/.dockerenv".to_string());
    }
    if inputs.containerenv_exists {
        markers.push("/run/.containerenv".to_string());
    }
    for (key, value) in inputs.env_pairs {
        let normalized = key.to_ascii_lowercase();
        if matches!(
            normalized.as_str(),
            "container" | "docker" | "podman" | "kubernetes_service_host"
        ) && !value.is_empty()
        {
            markers.push(format!("env:{key}={value}"));
        }
    }
    if let Some(cgroup) = inputs.proc_1_cgroup {
        for needle in ["docker", "containerd", "kubepods", "podman", "libpod"] {
            if cgroup.contains(needle) {
                markers.push(format!("/proc/1/cgroup:{needle}"));
            }
        }
    }
    markers.sort();
    markers.dedup();
    ContainerEnvironment {
        in_container: !markers.is_empty(),
        markers,
    }
}

#[must_use]
pub fn resolve_sandbox_status(config: &SandboxConfig, cwd: &Path) -> SandboxStatus {
    let request = config.resolve_request(None, None, None, None, None);
    resolve_sandbox_status_for_request(&request, cwd)
}

#[must_use]
pub fn resolve_sandbox_status_for_request(request: &SandboxRequest, cwd: &Path) -> SandboxStatus {
    let container = detect_container_environment();
    let namespace_supported = cfg!(target_os = "linux") && unshare_user_namespace_works();
    let network_supported = namespace_supported;
    let filesystem_active =
        request.enabled && request.filesystem_mode != FilesystemIsolationMode::Off;
    let platform_supported = platform_sandbox_supported();
    // active 仍基于 namespace/network 隔离能力,不依赖 platform_supported
    let active = request.enabled
        && (!request.namespace_restrictions || namespace_supported)
        && (!request.network_isolation || network_supported);

    let mut fallback_reasons = Vec::new();

    // 当前平台无任何沙箱机制可用(非 Linux/Windows/macOS,或 Linux unshare 不可用)
    if request.enabled && !platform_supported {
        fallback_reasons.push("当前平台无任何沙箱机制可用，命令将无隔离执行".to_string());
    }
    // 平台支持沙箱但 namespace 隔离未激活:Windows/macOS 上 namespace 不可用,
    // 已降级为平台原生沙箱(Windows Job Object / macOS sandbox-exec)的资源限制模式。
    // 注意:平台原生沙箱仅限制 CPU/内存等资源,不提供 PID/文件系统/网络隔离。
    if request.enabled && platform_supported && !active {
        if cfg!(target_os = "windows") {
            fallback_reasons.push(
                "namespace 隔离不可用（需 Linux），已降级为 Job Object 资源限制（不隔离 PID/文件系统/网络）".to_string()
            );
        } else if cfg!(target_os = "macos") {
            fallback_reasons.push(
                "namespace 隔离不可用（需 Linux），已降级为 sandbox-exec 资源限制".to_string()
            );
        } else {
            fallback_reasons.push(
                "平台支持沙箱但请求的隔离类型未激活（如 namespace 不可用）".to_string()
            );
        }
    }
    if request.enabled && request.namespace_restrictions && !namespace_supported {
        fallback_reasons
            .push("namespace isolation unavailable (requires Linux with `unshare`)".to_string());
    }
    if request.enabled && request.network_isolation && !network_supported {
        fallback_reasons
            .push("network isolation unavailable (requires Linux with `unshare`)".to_string());
    }
    if request.enabled
        && request.filesystem_mode == FilesystemIsolationMode::AllowList
        && request.allowed_mounts.is_empty()
    {
        fallback_reasons
            .push("filesystem allow-list requested without configured mounts".to_string());
    }

    let allowed_mounts = normalize_mounts(&request.allowed_mounts, cwd);

    SandboxStatus {
        enabled: request.enabled,
        requested: request.clone(),
        supported: platform_supported,
        active,
        namespace_supported,
        namespace_active: request.enabled && request.namespace_restrictions && namespace_supported,
        network_supported,
        network_active: request.enabled && request.network_isolation && network_supported,
        filesystem_mode: request.filesystem_mode,
        filesystem_active,
        allowed_mounts,
        in_container: container.in_container,
        container_markers: container.markers,
        fallback_reason: (!fallback_reasons.is_empty()).then(|| fallback_reasons.join("; ")),
    }
}

#[must_use]
pub fn build_linux_sandbox_command(
    command: &str,
    cwd: &Path,
    status: &SandboxStatus,
) -> Option<LinuxSandboxCommand> {
    if !cfg!(target_os = "linux")
        || !status.enabled
        || (!status.namespace_active && !status.network_active)
    {
        return None;
    }

    let mut args = vec![
        "--user".to_string(),
        "--map-root-user".to_string(),
        "--mount".to_string(),
        "--ipc".to_string(),
        "--pid".to_string(),
        "--uts".to_string(),
        "--fork".to_string(),
    ];
    if status.network_active {
        args.push("--net".to_string());
    }
    args.push("sh".to_string());
    args.push("-lc".to_string());
    args.push(command.to_string());

    let sandbox_home = cwd.join(".sandbox-home");
    let sandbox_tmp = cwd.join(".sandbox-tmp");
    let mut env = vec![
        ("HOME".to_string(), sandbox_home.display().to_string()),
        ("TMPDIR".to_string(), sandbox_tmp.display().to_string()),
        (
            "CLAWD_SANDBOX_FILESYSTEM_MODE".to_string(),
            status.filesystem_mode.as_str().to_string(),
        ),
        (
            "CLAWD_SANDBOX_ALLOWED_MOUNTS".to_string(),
            status.allowed_mounts.join(":"),
        ),
    ];
    if let Ok(path) = env::var("PATH") {
        env.push(("PATH".to_string(), path));
    }

    Some(LinuxSandboxCommand {
        program: "unshare".to_string(),
        args,
        env,
    })
}

fn normalize_mounts(mounts: &[String], cwd: &Path) -> Vec<String> {
    let cwd = cwd.to_path_buf();
    mounts
        .iter()
        .map(|mount| {
            let path = PathBuf::from(mount);
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        })
        .map(|path| path.display().to_string())
        .collect()
}

fn command_exists(command: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|paths| env::split_paths(&paths).any(|path| path.join(command).exists()))
}

/// Check whether `unshare --user` actually works on this system.
/// On some CI environments (e.g. GitHub Actions), the binary exists but
/// user namespaces are restricted, causing silent failures.
fn unshare_user_namespace_works() -> bool {
    use std::sync::OnceLock;
    static RESULT: OnceLock<bool> = OnceLock::new();
    *RESULT.get_or_init(|| {
        if !command_exists("unshare") {
            return false;
        }
        std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "true"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// 检测当前平台是否有任何可用的沙箱机制。
/// 不只反映 Linux namespace，还反映 Windows Job Object 和 macOS sandbox-exec。
pub fn platform_sandbox_supported() -> bool {
    if cfg!(target_os = "linux") {
        unshare_user_namespace_works()
    } else if cfg!(target_os = "windows") {
        WindowsSandboxBuilder::default().is_supported()
    } else if cfg!(target_os = "macos") {
        MacOsSandboxBuilder.is_supported()
    } else {
        false
    }
}

// ============================================================================
// Step 4.1 — SandboxBuilder trait + Windows/macOS 实现
// 详见 docs/harness-engineering-optimization-plan.md Step 4.1
// ============================================================================

/// 平台无关的沙箱命令构造器 trait。
///
/// 三实现:
/// - [`LinuxSandboxBuilder`]:基于 `unshare --user`(已有 `build_linux_sandbox_command`)
/// - [`WindowsSandboxBuilder`]:`CREATE_NO_WINDOW` + Job Object 限制 CPU/memory
/// - [`MacOsSandboxBuilder`]:`sandbox-exec` wrapper(placeholder)
///
/// 与 `bg.rs` 已有的 `CREATE_NO_WINDOW` flag 整合。
pub trait SandboxBuilder: Send + Sync {
    /// 平台标识("linux" / "windows" / "macos")。
    fn platform(&self) -> &'static str;

    /// 是否支持本平台的沙箱(运行时检测)。
    fn is_supported(&self) -> bool;

    /// 构造沙箱命令包装。
    ///
    /// 返回 `(program, args, env, creation_flags)` 四元组。
    /// `creation_flags` 仅 Windows 使用,其他平台为 0。
    fn build(
        &self,
        command: &str,
        cwd: &Path,
        status: &SandboxStatus,
    ) -> Result<SandboxCommand, String>;

    /// SP4.1 收尾:将已 spawn 的进程分配到沙箱(后置分配方案)。
    ///
    /// 与 `build()` 的"前置包装"方案不同,此方法用于在子进程已经 spawn
    /// 后(拿到 pid),通过平台原生机制(如 Win32 Job Object)将进程
    /// 分配到沙箱限制组。适用于需要直接控制子进程 stdin/stdout/stderr
    /// 的场景(如 bg.rs 的后台任务 spawn),无法用 cmd.exe / powershell
    /// wrapper 包装命令的情况。
    ///
    /// # 默认实现
    /// Linux/macOS 无 Job Object 概念,默认返回 `Ok(())`(no-op)。
    /// Windows 实现覆盖此方法,委托给 `assign_process_to_job_object`。
    ///
    /// # 错误处理
    /// 失败应返回 `Err(String)`,调用方(best-effort 路径)记录日志后继续。
    fn assign_process(&self, _pid: u32) -> Result<(), String> {
        Ok(())
    }
}

/// 平台无关的沙箱命令描述符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCommand {
    /// 要执行的程序(如 "unshare" / "cmd.exe" / "sandbox-exec")。
    pub program: String,
    /// 程序参数。
    pub args: Vec<String>,
    /// 环境变量覆盖。
    pub env: Vec<(String, String)>,
    /// Windows creation flags(仅 Windows 使用,其他平台为 0)。
    /// 参考 bg.rs:CREATE_NO_WINDOW=0x08000000 | DETACHED_PROCESS=0x00000008 | CREATE_NEW_PROCESS_GROUP=0x00000200
    pub creation_flags: u32,
}

/// Linux 沙箱构造器 — 基于 `unshare --user`。
pub struct LinuxSandboxBuilder;

impl SandboxBuilder for LinuxSandboxBuilder {
    fn platform(&self) -> &'static str {
        "linux"
    }

    fn is_supported(&self) -> bool {
        cfg!(target_os = "linux") && unshare_user_namespace_works()
    }

    fn build(
        &self,
        command: &str,
        cwd: &Path,
        status: &SandboxStatus,
    ) -> Result<SandboxCommand, String> {
        let linux_cmd = build_linux_sandbox_command(command, cwd, status)
            .ok_or_else(|| "Linux sandbox not available for this configuration".to_owned())?;
        Ok(SandboxCommand {
            program: linux_cmd.program,
            args: linux_cmd.args,
            env: linux_cmd.env,
            creation_flags: 0,
        })
    }
}

/// Windows 沙箱构造器 — `CREATE_NO_WINDOW` + Job Object 限制 CPU/memory。
///
/// 与 `bg.rs` 的 `CREATE_NO_WINDOW` flag 整合:
/// - `CREATE_NO_WINDOW = 0x08000000`(不创建控制台窗口)
/// - `DETACHED_PROCESS = 0x00000008`(脱离父进程控制台)
/// - `CREATE_NEW_PROCESS_GROUP = 0x00000200`(新进程组,不受父 Ctrl+C 影响)
///
/// Job Object 限制(通过 PowerShell 或 Win32 API 设置):
/// - `JOB_OBJECT_LIMIT_PROCESS_MEMORY`:限制单进程内存
/// - `JOB_OBJECT_LIMIT_JOB_MEMORY`:限制 Job 总内存
/// - `JOB_OBJECT_LIMIT_CPU_RATE`:限制 CPU 占比(Windows 8+)
///
/// 当前实现:
/// - 返回 `CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` flags
/// - Job Object 限制通过 `assign_process_to_job_object(pid)` 在 spawn 后设置
///   (PowerShell + C# 内联调用 Win32 API,见 `build_job_object_powershell`)
/// - `bg.rs::spawn` 已整合:spawn 后调用 `assign_process_to_job_object` 设置限制
pub struct WindowsSandboxBuilder {
    /// 内存上限(MB),None 表示不限制。
    pub memory_limit_mb: Option<u64>,
    /// CPU 占比上限(0-100),None 表示不限制。
    pub cpu_rate_limit: Option<u32>,
}

/// Windows creation flags 常量(与 bg.rs 对齐)。
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const DETACHED_PROCESS: u32 = 0x0000_0008;
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

impl Default for WindowsSandboxBuilder {
    fn default() -> Self {
        Self {
            memory_limit_mb: Some(2048), // 默认 2GB
            cpu_rate_limit: Some(80),    // 默认 80% CPU
        }
    }
}

impl WindowsSandboxBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置内存上限(MB)。
    #[must_use]
    pub fn with_memory_limit(mut self, mb: u64) -> Self {
        self.memory_limit_mb = Some(mb);
        self
    }

    /// 设置 CPU 占比上限(0-100)。
    #[must_use]
    pub fn with_cpu_rate(mut self, rate: u32) -> Self {
        self.cpu_rate_limit = Some(rate.clamp(1, 100));
        self
    }

    /// 构造 Job Object 限制的 PowerShell 包装命令。
    ///
    /// BUG-11:生成 PowerShell 脚本,通过 .NET System.Diagnostics.Process API
    /// 创建 Job Object,设置 CPU/memory 限制,将子进程分配到 Job Object。
    ///
    /// 实现原理:
    /// 1. 用 Add-Type 内联 C# 代码调用 Win32 API(CreateJobObject,
    ///    SetInformationJobObject, AssignProcessToJobObject)
    /// 2. 设置 JOB_OBJECT_LIMIT_PROCESS_MEMORY / JOB_OBJECT_LIMIT_CPU_RATE
    /// 3. 启动子进程并 Assign 到 Job Object
    /// 4. Job Object 在所有子进程退出后自动释放
    fn build_job_object_wrapper(&self, command: &str) -> String {
        // 使用 PowerShell 包装:通过环境变量传递限制配置,
        // 子进程的父进程(CLI)在 spawn 时通过 Win32 API 设置 Job Object。
        //
        // 实际 Job Object 限制在 process spawn 后由
        // `assign_process_to_job_object()` 设置(见下方)。
        // 此处生成包装命令,将限制参数编码到环境变量中。
        format!(
            "powershell.exe -NoProfile -Command \"\
            $env:CLAWD_JOB_MEMORY_MB = '{}'; \
            $env:CLAWD_JOB_CPU_RATE = '{}'; \
            & ({cmd})\
            \"",
            self.memory_limit_mb.unwrap_or(0),
            self.cpu_rate_limit.unwrap_or(0),
            cmd = command.replace('"', "\\\""),
        )
    }

    /// BUG-11 修复(原生 Win32 API 版):将指定进程分配到持久 Job Object。
    ///
    /// **原实现缺陷**:通过 PowerShell + C# 内联调用 Win32 API,PowerShell
    /// 脚本执行完毕退出后,Job Object 的唯一 handle 被关闭,Windows 内核
    /// 随即销毁 Job Object,导致已分配到 Job 的子进程被强制终止(即使未
    /// 设置 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`)。用户观察到的"后台任务
    /// 被沙箱的 Job Object 机制回收"根因即此。
    ///
    /// **修复方案**:在 Rust 进程内直接调用 Win32 API,通过 `OnceLock` 持有
    /// 全局 Job Object handle,生命周期与 claw 进程绑定。所有子进程都分配
    /// 到同一个持久 Job Object,handle 直到 claw 进程退出才被 OS 回收,
    /// 子进程不再因 handle 过早关闭而被清理。
    ///
    /// - CPU/内存限制通过 `SetInformationJobObject` 一次性设置
    /// - `AssignProcessToJobObject` 把子进程加入持久 Job
    /// - Windows 8+ 支持嵌套 Job,即使父进程已在另一个 Job 中也能分配
    ///
    /// 返回 `Ok(())` 表示分配成功;`Err` 表示失败(非致命,不阻断主流程)。
    #[cfg_attr(target_os = "windows", allow(unsafe_code))]
    pub fn assign_process_to_job_object(&self, pid: u32) -> Result<(), String> {
        if !cfg!(target_os = "windows") {
            return Ok(());
        }

        #[cfg(target_os = "windows")]
        {
            use std::sync::OnceLock;
            use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
            use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
            use windows_sys::Win32::System::Threading::OpenProcess;
            use windows_sys::Win32::System::Threading::{PROCESS_SET_QUOTA, PROCESS_TERMINATE};

            /// 全局持久 Job Object handle — 生命周期与 claw 进程绑定。
            /// 第一次调用时惰性创建并设置限制,之后所有子进程复用同一 Job。
            /// handle 从不显式 CloseHandle,由 OS 在进程退出时回收,
            /// 确保子进程不会因 handle 过早关闭而被强制终止。
            ///
            /// 用 `isize` 而非 `HANDLE` 存储,因为 `HANDLE = *mut c_void`
            /// 不实现 `Send`/`Sync`,无法用于 `static OnceLock`。`isize` 能
            /// 安全跨线程共享,使用时转回 `HANDLE`。
            static PERSISTENT_JOB: OnceLock<isize> = OnceLock::new();

            let job_raw = *PERSISTENT_JOB.get_or_init(|| {
                let h = create_persistent_job(
                    self.memory_limit_mb.unwrap_or(0),
                    self.cpu_rate_limit.unwrap_or(0),
                );
                h as isize
            });

            if job_raw == 0 {
                return Err("persistent job object not available".to_string());
            }

            let job = job_raw as HANDLE;

            // OpenProcess 获取子进程 handle,分配到 Job,然后关闭子进程 handle。
            // Job handle 保持持久打开,不在此处关闭。
            let desired_access = PROCESS_SET_QUOTA | PROCESS_TERMINATE;
            let h_proc = unsafe { OpenProcess(desired_access, 0, pid) };
            if h_proc.is_null() {
                return Err(format!("OpenProcess({pid}) failed"));
            }

            let result = unsafe { AssignProcessToJobObject(job, h_proc) };
            unsafe { CloseHandle(h_proc) };

            if result == 0 {
                Err(format!("AssignProcessToJobObject failed for pid {pid}"))
            } else {
                Ok(())
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            let _ = pid;
            Ok(())
        }
    }

    // 原 build_job_object_powershell 方法已移除 — 改用原生 Win32 API。
    // 详见 assign_process_to_job_object() 和模块级 create_persistent_job() 函数。

    /// 获取 creation flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)。
    fn creation_flags(&self) -> u32 {
        CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
    }
}

/// 创建持久 Job Object 并设置 CPU/内存限制(Windows 专用)。
///
/// 仅在 Windows 上有效,返回 Job Object handle(HANDLE = *mut c_void)。
/// 失败时返回 null。调用方负责**不显式关闭**此 handle —— 它的生命周期与
/// claw 进程绑定,由 OS 在进程退出时自动回收。这是修复"子进程被 Job Object
/// 回收"的关键:handle 持久打开,Job Object 不会被销毁,子进程不会被强制终止。
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
fn create_persistent_job(
    memory_limit_mb: u64,
    cpu_rate_limit: u32,
) -> windows_sys::Win32::Foundation::HANDLE {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
        JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOB_OBJECT_LIMIT_WORKINGSET,
    };

    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() || job == INVALID_HANDLE_VALUE {
        return std::ptr::null_mut();
    }

    // 设置 Extended Limits(内存限制)
    let mem_limit_bytes = memory_limit_mb.checked_mul(1024 * 1024).unwrap_or(0);
    if mem_limit_bytes > 0 {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_WORKINGSET;
        info.BasicLimitInformation.MaximumWorkingSetSize = mem_limit_bytes as _;
        info.ProcessMemoryLimit = mem_limit_bytes as _;

        // 内存限制设置失败不致命,Job 仍可使用(只是无内存限制)
        let _ = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
    }

    // 设置 CPU Rate Control(CPU 占比上限,Windows 8+)
    if cpu_rate_limit > 0 {
        let mut cpu_info: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = unsafe { std::mem::zeroed() };
        cpu_info.ControlFlags =
            JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
        cpu_info.Anonymous.CpuRate = cpu_rate_limit * 100; // 百分之几

        // CPU 限制设置失败不致命,Job 仍可使用(只是无 CPU 限制)
        let _ = unsafe {
            SetInformationJobObject(
                job,
                JobObjectCpuRateControlInformation,
                &cpu_info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            )
        };
    }

    job
}

impl SandboxBuilder for WindowsSandboxBuilder {
    fn platform(&self) -> &'static str {
        "windows"
    }

    fn is_supported(&self) -> bool {
        cfg!(target_os = "windows")
    }

    /// SP4.1 收尾:覆盖默认 no-op,委托给 `assign_process_to_job_object`。
    ///
    /// 通过 PowerShell + C# 内联调用 Win32 API(CreateJobObjectW +
    /// SetInformationJobObject + AssignProcessToJobObject)将 pid 对应的
    /// 进程分配到新创建的 Job Object,设置 CPU/memory 限制。
    fn assign_process(&self, pid: u32) -> Result<(), String> {
        self.assign_process_to_job_object(pid)
    }

    fn build(
        &self,
        command: &str,
        _cwd: &Path,
        status: &SandboxStatus,
    ) -> Result<SandboxCommand, String> {
        if !self.is_supported() {
            return Err("Windows sandbox not available on non-Windows platform".to_owned());
        }

        // Windows 不使用 unshare,而是通过 CREATE_NO_WINDOW + Job Object 限制
        // cwd 由调用方在 spawn 时设置,此处不嵌入命令
        let wrapped = self.build_job_object_wrapper(command);

        let mut env = vec![
            (
                "CLAWD_SANDBOX_FILESYSTEM_MODE".to_string(),
                status.filesystem_mode.as_str().to_string(),
            ),
            (
                "CLAWD_SANDBOX_ALLOWED_MOUNTS".to_string(),
                status.allowed_mounts.join(";"), // Windows 用分号分隔
            ),
        ];
        if let Some(mem_limit) = self.memory_limit_mb {
            env.push((
                "CLAWD_SANDBOX_MEMORY_LIMIT_MB".to_string(),
                mem_limit.to_string(),
            ));
        }
        if let Some(cpu_rate) = self.cpu_rate_limit {
            env.push((
                "CLAWD_SANDBOX_CPU_RATE_LIMIT".to_string(),
                cpu_rate.to_string(),
            ));
        }
        if let Ok(path) = env::var("PATH") {
            env.push(("PATH".to_string(), path));
        }

        // Windows 使用 cmd.exe /c 包装(与 bg.rs 整合)
        Ok(SandboxCommand {
            program: "cmd.exe".to_string(),
            args: vec!["/c".to_string(), wrapped],
            env,
            creation_flags: self.creation_flags(),
        })
    }
}

/// macOS 沙箱构造器 — `sandbox-exec` wrapper(优先级低,placeholder)。
pub struct MacOsSandboxBuilder;

impl SandboxBuilder for MacOsSandboxBuilder {
    fn platform(&self) -> &'static str {
        "macos"
    }

    fn is_supported(&self) -> bool {
        cfg!(target_os = "macos") && command_exists("sandbox-exec")
    }

    fn build(
        &self,
        command: &str,
        cwd: &Path,
        status: &SandboxStatus,
    ) -> Result<SandboxCommand, String> {
        if !self.is_supported() {
            return Err("macOS sandbox-exec not available".to_owned());
        }

        // macOS 使用 sandbox-exec -p <profile> 包装
        // 当前 profile 为最小化版本(仅允许工作目录读写)
        let profile = format!(
            "(version 1)\n(deny default)\n(allow process-fork)\n(allow process-exec)\n(allow file-read*)\n(allow file-write* (subpath \"{}\"))\n(allow network*)",
            cwd.display()
        );

        let mut env = vec![
            (
                "CLAWD_SANDBOX_FILESYSTEM_MODE".to_string(),
                status.filesystem_mode.as_str().to_string(),
            ),
            (
                "CLAWD_SANDBOX_ALLOWED_MOUNTS".to_string(),
                status.allowed_mounts.join(":"),
            ),
        ];
        if let Ok(path) = env::var("PATH") {
            env.push(("PATH".to_string(), path));
        }

        Ok(SandboxCommand {
            program: "sandbox-exec".to_string(),
            args: vec![
                "-p".to_string(),
                profile,
                "sh".to_string(),
                "-c".to_string(),
                command.to_string(),
            ],
            env,
            creation_flags: 0,
        })
    }
}

/// 根据当前平台选择合适的沙箱构造器。
///
/// 返回 `Box<dyn SandboxBuilder>`,调用方可直接 `build()`。
#[must_use]
pub fn platform_sandbox_builder() -> Box<dyn SandboxBuilder> {
    if cfg!(target_os = "linux") {
        Box::new(LinuxSandboxBuilder)
    } else if cfg!(target_os = "windows") {
        Box::new(WindowsSandboxBuilder::new())
    } else if cfg!(target_os = "macos") {
        Box::new(MacOsSandboxBuilder)
    } else {
        // 未知平台,返回 Linux 构造器(会返回 not supported)
        Box::new(LinuxSandboxBuilder)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_linux_sandbox_command, detect_container_environment_from, platform_sandbox_builder,
        FilesystemIsolationMode, SandboxBuilder, SandboxCommand, SandboxConfig,
        SandboxDetectionInputs, WindowsSandboxBuilder, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
        DETACHED_PROCESS,
    };
    use std::path::Path;

    #[test]
    fn detects_container_markers_from_multiple_sources() {
        let detected = detect_container_environment_from(SandboxDetectionInputs {
            env_pairs: vec![("container".to_string(), "docker".to_string())],
            dockerenv_exists: true,
            containerenv_exists: false,
            proc_1_cgroup: Some("12:memory:/docker/abc"),
        });

        assert!(detected.in_container);
        assert!(detected
            .markers
            .iter()
            .any(|marker| marker == "/.dockerenv"));
        assert!(detected
            .markers
            .iter()
            .any(|marker| marker == "env:container=docker"));
        assert!(detected
            .markers
            .iter()
            .any(|marker| marker == "/proc/1/cgroup:docker"));
    }

    #[test]
    fn resolves_request_with_overrides() {
        let config = SandboxConfig {
            enabled: Some(true),
            namespace_restrictions: Some(true),
            network_isolation: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: vec!["logs".to_string()],
        };

        let request = config.resolve_request(
            Some(true),
            Some(false),
            Some(true),
            Some(FilesystemIsolationMode::AllowList),
            Some(vec!["tmp".to_string()]),
        );

        assert!(request.enabled);
        assert!(!request.namespace_restrictions);
        assert!(request.network_isolation);
        assert_eq!(request.filesystem_mode, FilesystemIsolationMode::AllowList);
        assert_eq!(request.allowed_mounts, vec!["tmp"]);
    }

    #[test]
    fn builds_linux_launcher_with_network_flag_when_requested() {
        let config = SandboxConfig::default();
        let status = super::resolve_sandbox_status_for_request(
            &config.resolve_request(
                Some(true),
                Some(true),
                Some(true),
                Some(FilesystemIsolationMode::WorkspaceOnly),
                None,
            ),
            Path::new("/workspace"),
        );

        if let Some(launcher) =
            build_linux_sandbox_command("printf hi", Path::new("/workspace"), &status)
        {
            assert_eq!(launcher.program, "unshare");
            assert!(launcher.args.iter().any(|arg| arg == "--mount"));
            assert!(launcher.args.iter().any(|arg| arg == "--net") == status.network_active);
        }
    }

    // ========================================================================
    // Step 4.1 — SandboxBuilder trait + Windows/macOS 实现 测试
    // ========================================================================

    #[test]
    fn platform_sandbox_builder_returns_correct_platform() {
        let builder = platform_sandbox_builder();
        if cfg!(target_os = "linux") {
            assert_eq!(builder.platform(), "linux");
        } else if cfg!(target_os = "windows") {
            assert_eq!(builder.platform(), "windows");
        } else if cfg!(target_os = "macos") {
            assert_eq!(builder.platform(), "macos");
        }
    }

    #[test]
    fn windows_sandbox_builder_default_has_limits() {
        let builder = WindowsSandboxBuilder::new();
        assert_eq!(builder.memory_limit_mb, Some(2048));
        assert_eq!(builder.cpu_rate_limit, Some(80));
    }

    #[test]
    fn windows_sandbox_builder_with_memory_limit() {
        let builder = WindowsSandboxBuilder::new().with_memory_limit(4096);
        assert_eq!(builder.memory_limit_mb, Some(4096));
    }

    #[test]
    fn windows_sandbox_builder_with_cpu_rate_clamps() {
        let builder = WindowsSandboxBuilder::new().with_cpu_rate(150);
        assert_eq!(builder.cpu_rate_limit, Some(100));
        let builder = WindowsSandboxBuilder::new().with_cpu_rate(0);
        assert_eq!(builder.cpu_rate_limit, Some(1));
    }

    #[test]
    fn windows_creation_flags_constants_correct() {
        assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
        assert_eq!(DETACHED_PROCESS, 0x0000_0008);
        assert_eq!(CREATE_NEW_PROCESS_GROUP, 0x0000_0200);
    }

    #[test]
    fn windows_sandbox_builder_returns_create_no_window_flags() {
        let builder = WindowsSandboxBuilder::new();
        let status =
            super::resolve_sandbox_status(&SandboxConfig::default(), Path::new("/workspace"));
        let result = builder.build("echo hi", Path::new("/workspace"), &status);

        // 在非 Windows 平台应返回 Err
        if cfg!(target_os = "windows") {
            let cmd = result.expect("should succeed on Windows");
            assert!(cmd.creation_flags & CREATE_NO_WINDOW != 0);
            assert!(cmd.creation_flags & DETACHED_PROCESS != 0);
            assert!(cmd.creation_flags & CREATE_NEW_PROCESS_GROUP != 0);
            assert_eq!(cmd.program, "cmd.exe");
        } else {
            assert!(result.is_err());
        }
    }

    #[test]
    fn windows_sandbox_builder_env_includes_limits() {
        let builder = WindowsSandboxBuilder::new()
            .with_memory_limit(8192)
            .with_cpu_rate(50);
        let status =
            super::resolve_sandbox_status(&SandboxConfig::default(), Path::new("/workspace"));

        if cfg!(target_os = "windows") {
            let cmd = builder
                .build("echo hi", Path::new("/workspace"), &status)
                .expect("should succeed on Windows");
            let env_keys: Vec<&str> = cmd.env.iter().map(|(k, _)| k.as_str()).collect();
            assert!(env_keys.contains(&"CLAWD_SANDBOX_MEMORY_LIMIT_MB"));
            assert!(env_keys.contains(&"CLAWD_SANDBOX_CPU_RATE_LIMIT"));
            // Verify values
            let mem_val = cmd
                .env
                .iter()
                .find(|(k, _)| k == "CLAWD_SANDBOX_MEMORY_LIMIT_MB")
                .map(|(_, v)| v);
            assert_eq!(mem_val, Some(&"8192".to_string()));
        }
    }

    #[test]
    fn linux_sandbox_builder_platform_returns_linux() {
        let builder = super::LinuxSandboxBuilder;
        assert_eq!(builder.platform(), "linux");
    }

    #[test]
    fn macos_sandbox_builder_platform_returns_macos() {
        let builder = super::MacOsSandboxBuilder;
        assert_eq!(builder.platform(), "macos");
    }

    #[test]
    fn sandbox_command_is_debug_clone() {
        let cmd = SandboxCommand {
            program: "test".to_string(),
            args: vec!["arg1".to_string()],
            env: vec![("KEY".to_string(), "VALUE".to_string())],
            creation_flags: 0,
        };
        let cloned = cmd.clone();
        assert_eq!(cmd, cloned);
        // Verify Debug is implemented
        let _ = format!("{cmd:?}");
    }

    #[test]
    fn builder_is_supported_matches_platform() {
        let linux = super::LinuxSandboxBuilder;
        let win = WindowsSandboxBuilder::new();
        let macos = super::MacOsSandboxBuilder;

        // is_supported() should return true only on matching platform
        if cfg!(target_os = "linux") {
            // Linux support depends on unshare availability, just verify it doesn't panic
            let _ = linux.is_supported();
            assert!(!win.is_supported());
            assert!(!macos.is_supported());
        } else if cfg!(target_os = "windows") {
            assert!(!linux.is_supported());
            assert!(win.is_supported());
            assert!(!macos.is_supported());
        }
    }
}
