//! 进化自动化闸门的决策核心(断点①②)。
//!
//! 现状缺陷(实测):`turn 收尾 → inbox` 这一段是通的(38 条积压 8 天),
//! 但 **inbox → 修复** 零消费者 —— 全仓库只有 `claw evolve-source` 手动入口,
//! `app.rs` / `tui/` 下无任何 daemon、cron、后台轮询。提案因此永久积压。
//!
//! 本模块只做**纯决策**(输入信号 → 是否拉起进化 + 是否该热升级),不落盘、
//! 不 spawn、不调 LLM,便于单测。真正执行由调用方负责。
//!
//! 设计约束(与 `evolution_runner` 一致):
//! - 默认开启,`CLAW_EVOLUTION_AUTO=0` 一键关;
//! - 冷却期防抖:一次进化要跑 cargo test/build 数分钟,不能每 turn 都拉;
//! - 部署后必须让**父进程**(TUI,有可重启会话)去热升级 —— `-p` 子进程
//!   本轮结束即退,没有可被重启的会话,它自己升级等于丢改动。

use std::path::Path;

/// 自动进化总开关环境变量。默认为 `1`(开启);`0` 关闭。
pub const AUTO_ENV: &str = "CLAW_EVOLUTION_AUTO";
/// 两次自动进化之间的最小间隔(毫秒)。默认 30 分钟。
pub const AUTO_ENV_COOLDOWN_MS: &str = "CLAW_EVOLUTION_COOLDOWN_MS";

/// 自动进化门控配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoGateConfig {
    /// 总开关。
    pub enabled: bool,
    /// 两次自动进化最小间隔(毫秒)。0 = 不限。
    pub cooldown_ms: i64,
}

impl Default for AutoGateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cooldown_ms: 30 * 60 * 1000,
        }
    }
}

/// 从环境读取配置(`CLAW_EVOLUTION_AUTO` / `CLAW_EVOLUTION_COOLDOWN_MS`)。
#[must_use]
pub fn config_from_env() -> AutoGateConfig {
    let d = AutoGateConfig::default();
    let enabled = std::env::var(AUTO_ENV)
        .map(|v| v.trim() != "0")
        .unwrap_or(d.enabled);
    let cooldown_ms = std::env::var(AUTO_ENV_COOLDOWN_MS)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(d.cooldown_ms);
    AutoGateConfig {
        enabled,
        cooldown_ms,
    }
}

/// 门控决策:是否应在本 turn 收尾时拉起一次自动进化。
///
/// 入参全部为廉价本地量,零 LLM。返回 `Some(原因)` 表示应拉起。
///
/// 拒绝理由优先级(便于回看日志定位"为什么没动"):
/// 1. 总开关关闭;
/// 2. inbox 无待处理(无事可做,最常见);
/// 3. 另一个进化会话持锁(避免并发 cargo build / exe 覆盖);
/// 4. 冷却期未到(一次进化数分钟,不能每 turn 都拉)。
#[must_use]
pub fn should_auto_evolve(
    pending: usize,
    last_spawn_ms: i64,
    now_ms: i64,
    lock_held: bool,
    cfg: &AutoGateConfig,
) -> Option<&'static str> {
    if !cfg.enabled {
        return None;
    }
    if pending == 0 {
        return None;
    }
    if lock_held {
        return None;
    }
    if cfg.cooldown_ms > 0 && last_spawn_ms != 0 {
        let elapsed = now_ms.saturating_sub(last_spawn_ms);
        if elapsed < cfg.cooldown_ms {
            return None;
        }
    }
    Some("pending_proposals")
}

/// 上次拉起进化进程的时间戳标记(冷却期依据)。
///
/// 必须落盘:进程重启后内存值丢失,否则每次启动都会立刻再拉一次进化,
/// 冷却期形同虚设。
pub const LAST_SPAWN_REL: &str = ".claw/evolution/.last_spawn";

/// 读取上次拉起时间(毫秒);无记录返回 0。
#[must_use]
pub fn read_last_spawn(workspace: &Path) -> i64 {
    std::fs::read_to_string(workspace.join(LAST_SPAWN_REL))
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

/// 记录本次拉起时间。失败不影响主流程(顶多冷却期失效一次)。
pub fn write_last_spawn(workspace: &Path, now_ms: i64) {
    let path = workspace.join(LAST_SPAWN_REL);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{now_ms}\n"));
}

/// 当前毫秒时间戳。
#[must_use]
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 递归防护环境变量:进化子进程内不得再拉起进化。
pub const ACTIVE_ENV: &str = "CLAW_EVOLUTION_ACTIVE";

/// 本次进化是否已在进化子进程内(用于递归防护)。
#[must_use]
pub fn in_evolution_process() -> bool {
    std::env::var_os(ACTIVE_ENV).is_some()
}

/// 取当前 HEAD 提交号(失败返回空串)。
///
/// 用于「子智能体到底有没有提交」的判定 —— 只比 `git show HEAD` 的文件清单
/// 是不够的: 若子智能体**没提交**, HEAD 仍指向上一个提交, 清单非空, 守卫
/// 会失效进而部署旧代码。必须比对运行前后的 HEAD 是否变化。
#[must_use]
pub fn head_sha(source_root: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(source_root)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}
///
/// 用已提交的 HEAD 而非 `git status`:子智能体被要求"测试门全过才提交",
/// 所以 HEAD 存在即代表改动已过测试;未提交的残留工作区不应被部署。
#[must_use]
pub fn committed_file_list(source_root: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["show", "--name-only", "--format=", "HEAD"])
        .current_dir(source_root)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => String::new(),
    }
}

/// 构建 release 并把二进制回拷到当前运行路径(**自动部署到全局**)。
///
/// 从 `rusty-claude-cli` 的 `install` 分支原样下移到 runtime:进化管道
/// (runtime 层)与 CLI 手动入口共用同一实现,避免"自动部署"与"手动部署"
/// 两条路径行为漂移。失败回滚旧 exe,绝不留下半截二进制。
pub fn install_binary(source_root: &Path) -> Result<std::path::PathBuf, String> {
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "rusty-claude-cli"])
        .current_dir(source_root)
        .status()
        .map_err(|e| format!("启动 cargo 失败: {e}"))?;
    if !status.success() {
        return Err("cargo build --release 失败,拒绝部署".to_string());
    }
    let exe = std::env::current_exe().map_err(|e| format!("取当前 exe 失败: {e}"))?;
    let name = exe
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("无法获取当前 exe 文件名")?
        .to_string();
    let built = source_root.join("target").join("release").join(&name);
    if !built.is_file() {
        return Err(format!("构建产物缺失: {}", built.display()));
    }
    // Windows 不允许覆盖运行中的 exe:先 rename 旧文件为 .old。
    let mut old = exe.clone().into_os_string();
    old.push(".old");
    let old = std::path::PathBuf::from(old);
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(&exe, &old) {
        return Err(format!("备份 {} 失败: {e}", exe.display()));
    }
    if let Err(e) = std::fs::copy(&built, &exe) {
        // 回滚:恢复旧 exe,避免 brick。
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("回拷到 {} 失败(已回滚): {e}", exe.display()));
    }
    Ok(exe)
}

/// 进化管道(断点①②③的完整自动链路)。
///
/// 用户构思的直接实现:**一个 BUG 提案 → 在源码根拉起 claw 子进程修 → 测试门
/// 过则自动部署到全局**,全程无需人工介入。
///
/// 安全边界(自动化的前提):
/// - 测试门与提交由子智能体负责(提示词已内嵌);
/// - 部署前用**真实 diff** 硬校验黑名单(`diff_violates_forbidden`) ——
///   提示词对模型的约束只是请求,不能当作防线;
/// - 部署失败自动回滚旧 exe(`install_binary` 内)。
///
/// 返回一行结果描述,供日志/CLI 输出。
pub fn run_pipeline(source_root: &Path) -> Result<String, String> {
    if in_evolution_process() {
        return Err("已在进化子进程内,跳过(递归防护)".to_string());
    }
    let _lock = match crate::evolution_lock::EvolutionLock::acquire(source_root)? {
        Some(l) => l,
        None => return Err("另一个进化会话正在运行(进化锁被持有)".to_string()),
    };
    let inbox = crate::evolution_inbox::resolve_inbox_dir(source_root);
    let pending = crate::evolution_inbox::list_pending(&inbox);
    let Some(proposal_path) = pending.into_iter().next() else {
        return Err("inbox 无待处理提案".to_string());
    };
    let text = std::fs::read_to_string(&proposal_path)
        .map_err(|e| format!("读取提案失败 {}: {e}", proposal_path.display()))?;
    let parsed = crate::evolution_runner::parse_proposal(&text);
    let prompt = crate::evolution_runner::evolution_prompt(&proposal_path, &parsed);

    // 1. 在源码根拉起 claw 子进程修复(继承其自身可执行文件, 不依赖 PATH)。
    let head_before = head_sha(source_root);
    let exe = std::env::current_exe().map_err(|e| format!("取当前 exe 失败: {e}"))?;
    let status = std::process::Command::new(&exe)
        .current_dir(source_root)
        .env(ACTIVE_ENV, "1")
        .args(["--dangerously-skip-permissions", "-p", &prompt])
        .status()
        .map_err(|e| format!("启动进化子进程失败: {e}"))?;
    if !status.success() {
        // 失败不部署、不标记已处理 —— 提案留在 inbox 等下次重试。
        return Err(format!("进化子智能体退出码非零: {status}(未部署)"));
    }

    // 2. 部署前硬校验。
    //    ⚠ 必须先确认"确实产生了新提交": 只看 `git show HEAD` 的文件清单不够 ——
    //    子智能体若**没提交**, HEAD 仍指向上一个提交、清单非空, 守卫会失效,
    //    进而把**旧代码**当成本次修复部署出去。故比对运行前后的 HEAD。
    let head_after = head_sha(source_root);
    if head_after.is_empty() || head_after == head_before {
        return Err("子智能体未产生新提交,跳过部署(提案保留)".to_string());
    }
    let files = committed_file_list(source_root);
    if files.trim().is_empty() {
        return Err("无法读取本次提交的文件清单,跳过部署".to_string());
    }
    if let Some(frag) = crate::evolution_runner::diff_violates_forbidden(&files) {
        return Err(format!("改动命中黑名单 `{frag}`,拒绝部署(提案保留)"));
    }

    // 3. 自动部署到全局。失败则回滚,提案保留。
    let installed = install_binary(source_root)?;

    // 4. 标记提案已处理 + 写下部署标记(供 TUI 决定是否热升级当前会话)。
    let _ = crate::evolution_inbox::mark_processed(&inbox, &proposal_path);
    Ok(format!(
        "已修复并部署: {} → {}",
        proposal_path.display(),
        installed.display()
    ))
}

/// 在后台拉起进化管道(**上报即触发**),不阻塞当前 turn。
///
/// 用 `current_exe` + 隐藏参数 `--evolve-pipeline` 起一个新进程,而不是在
/// 本进程内跑:修复要经历 cargo test/build 数分钟,绝不能阻塞用户正在用的
/// 这个 claw;子进程独立存活,父进程退出也不影响它。
///
/// 触发条件全部走 [`should_auto_evolve`],并落盘 `last_spawn` 做冷却,
/// 防止一次上报风暴拉起多个进化进程同时 `cargo build`。
pub fn spawn_pipeline_detached(workspace: &Path) -> Result<(), String> {
    if in_evolution_process() {
        return Ok(()); // 递归防护:子进程内不再拉起
    }
    let cfg = config_from_env();
    let inbox = crate::evolution_inbox::resolve_inbox_dir(workspace);
    let pending = crate::evolution_inbox::list_pending(&inbox).len();
    let source_root = match crate::source_root::resolve_source_root() {
        Some(r) => r,
        None => return Ok(()), // 未登记源码根:无从进化
    };
    let lock_held = crate::evolution_lock::EvolutionLock::is_held(&source_root);
    let now = now_ms();
    if should_auto_evolve(pending, read_last_spawn(&source_root), now, lock_held, &cfg).is_none() {
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| format!("取当前 exe 失败: {e}"))?;
    std::process::Command::new(&exe)
        .current_dir(&source_root)
        // 复用已存在的 `evolve-source` 子命令而非新隐藏参数: 旧二进制不认
        // `auto` 动作会直接报错退出(安全无副作用), 新版才真正执行全管道。
        .args(["evolve-source", "auto"])
        // 断掉 stdio 继承: 否则子进程的 cargo 构建输出会直接打到用户正在用的
        // TUI 终端上, 污染界面。日志走 .claw 下的既有文件, 不依赖 stdout。
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("拉起进化管道失败: {e}"))?;
    write_last_spawn(&source_root, now);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AutoGateConfig {
        AutoGateConfig::default()
    }

    #[test]
    fn disabled_never_triggers() {
        let c = AutoGateConfig {
            enabled: false,
            ..cfg()
        };
        assert_eq!(should_auto_evolve(5, 0, 1_000_000, false, &c), None);
    }

    #[test]
    fn empty_inbox_never_triggers() {
        // 无待办是最常见路径,必须零开销返回
        assert_eq!(should_auto_evolve(0, 0, 1_000_000, false, &cfg()), None);
    }

    #[test]
    fn held_lock_blocks_to_avoid_concurrent_cargo() {
        assert_eq!(should_auto_evolve(3, 0, 1_000_000, true, &cfg()), None);
    }

    #[test]
    fn cooldown_blocks_rapid_respawn() {
        let now = 1_000_000_000;
        // 刚拉过 → 冷却中
        assert_eq!(should_auto_evolve(3, now - 1000, now, false, &cfg()), None);
        // 冷却期已过 → 允许
        assert_eq!(
            should_auto_evolve(3, now - 31 * 60 * 1000, now, false, &cfg()),
            Some("pending_proposals")
        );
    }

    #[test]
    fn zero_last_spawn_means_never_ran() {
        // 进程内首次(turn 尚末拉过)→ 不受冷却限制
        assert_eq!(
            should_auto_evolve(1, 0, 1_000_000, false, &cfg()),
            Some("pending_proposals")
        );
    }

    #[test]
    fn zero_cooldown_disables_that_axis() {
        let c = AutoGateConfig {
            cooldown_ms: 0,
            ..cfg()
        };
        assert_eq!(
            should_auto_evolve(1, 999_999, 1_000_000, false, &c),
            Some("pending_proposals")
        );
    }
}
