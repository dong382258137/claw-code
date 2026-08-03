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
    // 平台支持沙箱但请求的隔离类型未激活(如 namespace 不可用)
    if request.enabled && platform_supported && !active {
        fallback_reasons
            .push("平台支持沙箱但请求的隔离类型未激活（如 namespace 不可用）".to_string());
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

    /// BUG-11:将当前进程分配到 Job Object 并设置限制。
    ///
    /// 通过 Win32 API(CreateJobObjectW + SetInformationJobObject +
    /// AssignProcessToJobObject)实现。仅在 Windows 上生效。
    ///
    /// 返回 `Ok(())` 表示 Job Object 已创建并设置限制。
    /// 返回 `Err` 表示设置失败(非致命,不阻断主流程)。
    pub fn assign_process_to_job_object(&self, pid: u32) -> Result<(), String> {
        if !cfg!(target_os = "windows") {
            return Ok(());
        }

        // 使用 PowerShell + C# 内联调用 Win32 API
        let ps_script = self.build_job_object_powershell(pid);

        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &ps_script])
            .output();

        match output {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                Err(format!("job object setup failed: {stderr}"))
            }
            Err(e) => Err(format!("powershell error: {e}")),
        }
    }

    /// 生成设置 Job Object 的 PowerShell + C# 内联脚本。
    ///
    /// SP4.1 修复(审查后):
    /// - 改用 `JobObjectExtendedLimitInformation`(Class=9,144 bytes on x64)
    ///   替代 `JobObjectBasicLimitInformation`(Class=2,64 bytes)
    ///   前者才包含 `ProcessMemoryLimit` 字段,真正限制进程内存
    /// - 修正 `JOB_OBJECT_LIMIT_PROCESS_MEMORY` flag:0x00000100(原代码误用 0x00000004,
    ///   实际是 JOB_OBJECT_LIMIT_JOB_TIME,导致内存限制完全失效)
    /// - 修正 CpuRateControl InfoClass:15(原代码误用 9,会污染 ExtendedLimitInformation)
    /// - 加 try/finally 确保 AllocHGlobal 内存和句柄在异常时也释放
    /// - 显式 CloseHandle($job)(原代码依赖 OS 隐式回收)
    fn build_job_object_powershell(&self, pid: u32) -> String {
        let mem_limit_bytes = self.memory_limit_mb.unwrap_or(0) * 1024 * 1024;
        let cpu_rate = self.cpu_rate_limit.unwrap_or(0);

        format!(
            r#"
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

public class Win32JobObject {{
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)]
    public static extern IntPtr CreateJobObjectW(IntPtr lpJobAttributes, string lpName);

    [DllImport("kernel32.dll")]
    public static extern bool SetInformationJobObject(IntPtr hJob, int JobObjectInfoClass, IntPtr lpJobObjectInfo, int cbJobObjectInfoLength);

    [DllImport("kernel32.dll")]
    public static extern bool AssignProcessToJobObject(IntPtr hJob, IntPtr hProcess);

    [DllImport("kernel32.dll")]
    public static extern IntPtr OpenProcess(int dwDesiredAccess, bool bInheritHandle, int dwProcessId);

    [DllImport("kernel32.dll")]
    public static extern bool CloseHandle(IntPtr hObject);
}}
"@

$job = [Win32JobObject]::CreateJobObjectW([IntPtr]::Zero, "ClawSandboxJob")
if ($job -eq [IntPtr]::Zero) {{ throw "CreateJobObjectW failed" }}

try {{
    # Extended Limits: JobObjectExtendedLimitInformation (Class=9)
    # Layout on x64 (144 bytes total):
    #   0-63:   BasicLimitInformation (64 bytes)
    #     0-7:    PerProcessUserTimeLimit (LARGE_INTEGER)
    #     8-15:   PerJobUserTimeLimit (LARGE_INTEGER)
    #     16-19:  LimitFlags (DWORD)
    #     20-23:  padding
    #     24-31:  MinimumWorkingSetSize (SIZE_T)
    #     32-39:  MaximumWorkingSetSize (SIZE_T)
    #     40-43:  ActiveProcessLimit (DWORD)
    #     44-47:  padding
    #     48-55:  Affinity (ULONG_PTR)
    #     56-59:  PriorityClass (DWORD)
    #     60-63:  SchedulingClass (DWORD)
    #   64-111:  IoInfo (IO_COUNTERS, 48 bytes, all zero)
    #   112-119: ProcessMemoryLimit (SIZE_T) — requires JOB_OBJECT_LIMIT_PROCESS_MEMORY flag
    #   120-127: JobMemoryLimit (SIZE_T)
    #   128-135: PeakProcessMemoryUsed (SIZE_T)
    #   136-143: PeakJobMemoryUsed (SIZE_T)
    $extInfo = [System.Runtime.InteropServices.Marshal]::AllocHGlobal(144)
    try {{
        # Zero the entire buffer first(确保 padding 和 IoInfo 为 0)
        for ($i = 0; $i -lt 144; $i += 8) {{
            [System.Runtime.InteropServices.Marshal]::WriteInt64($extInfo, $i, 0)
        }}

        # BasicLimitInformation.LimitFlags (offset 16, DWORD)
        $limitFlags = 0
        if ({mem_limit_bytes} -gt 0) {{
            # JOB_OBJECT_LIMIT_PROCESS_MEMORY = 0x00000100(原代码误用 0x00000004)
            # JOB_OBJECT_LIMIT_WORKINGSET = 0x00000001(若设 MaxWorkingSet 需同时设此 flag)
            $limitFlags = $limitFlags -bor 0x00000100 -bor 0x00000001
        }}
        [System.Runtime.InteropServices.Marshal]::WriteInt32($extInfo, 16, $limitFlags)

        # BasicLimitInformation.MaximumWorkingSetSize (offset 32, SIZE_T)
        if ({mem_limit_bytes} -gt 0) {{
            [System.Runtime.InteropServices.Marshal]::WriteInt64($extInfo, 32, [long]{mem_limit_bytes})
        }}

        # ProcessMemoryLimit (offset 112, SIZE_T) — 真正限制进程提交的虚拟内存
        if ({mem_limit_bytes} -gt 0) {{
            [System.Runtime.InteropServices.Marshal]::WriteInt64($extInfo, 112, [long]{mem_limit_bytes})
        }}

        if (-not [Win32JobObject]::SetInformationJobObject($job, 9, $extInfo, 144)) {{
            throw "SetInformationJobObject(Extended, Class=9) failed"
        }}
    }} finally {{
        [System.Runtime.InteropServices.Marshal]::FreeHGlobal($extInfo)
    }}

    # CPU Rate Control: JobObjectCpuRateControlInformation (Class=15) — Windows 8+
    # 原代码误用 Class=9(ExtendedLimitInformation),会污染 BasicLimitInformation
    if ({cpu_rate} -gt 0) {{
        $cpuInfo = [System.Runtime.InteropServices.Marshal]::AllocHGlobal(8)
        try {{
            # JOB_OBJECT_CPU_RATE_CONTROL_ENABLE = 0x1
            # JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP = 0x4
            $cpuFlags = 0x1 -bor 0x4
            [System.Runtime.InteropServices.Marshal]::WriteInt32($cpuInfo, 0, $cpuFlags)
            # CpuRate = rate * 100(hundredths of percent,如 80 表示 80%)
            [System.Runtime.InteropServices.Marshal]::WriteInt32($cpuInfo, 4, {cpu_rate} * 100)
            if (-not [Win32JobObject]::SetInformationJobObject($job, 15, $cpuInfo, 8)) {{
                # CPU rate control may not be supported(Windows 7 或更早);忽略错误
            }}
        }} finally {{
            [System.Runtime.InteropServices.Marshal]::FreeHGlobal($cpuInfo)
        }}
    }}

    # Assign process to job
    $PROCESS_SET_QUOTA = 0x0100
    $PROCESS_TERMINATE = 0x0001
    $hProc = [Win32JobObject]::OpenProcess($PROCESS_SET_QUOTA -bor $PROCESS_TERMINATE, $false, {pid})
    if ($hProc -eq [IntPtr]::Zero) {{ throw "OpenProcess({pid}) failed" }}
    try {{
        if (-not [Win32JobObject]::AssignProcessToJobObject($job, $hProc)) {{
            throw "AssignProcessToJobObject failed"
        }}
    }} finally {{
        [Win32JobObject]::CloseHandle($hProc)
    }}
}} finally {{
    [Win32JobObject]::CloseHandle($job)
}}
"#,
            mem_limit_bytes = mem_limit_bytes,
            cpu_rate = cpu_rate,
            pid = pid,
        )
    }

    /// 获取 creation flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)。
    fn creation_flags(&self) -> u32 {
        CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
    }
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
