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

use std::path::{Path, PathBuf};

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

/// 跑一遍测试门,返回首个失败的**命令串**(全部通过返回 `None`)。
///
/// 执行器用闭包注入,使单测无需真的跑 `cargo test --workspace`(数分钟)。
fn run_test_gate_with<F>(source_root: &Path, mut run: F) -> Option<String>
where
    F: FnMut(&Path, &str) -> Result<bool, String>,
{
    for cmd in crate::evolution_runner::test_gate_commands() {
        match run(source_root, cmd) {
            Ok(true) => {}
            Ok(false) => return Some(cmd.to_string()),
            Err(e) => return Some(format!("{cmd} ({e})")),
        }
    }
    None
}

/// 真跑测试门。命令串按空白切分为 程序 + 参数。
///
/// **为什么必须由代码跑,而不是交给提示词**: 提示词对模型的约束只是"请求",
/// 被审查对象可能不遵守。若只信 prompt, 子智能体改完就提交、跳过测试, 只要
/// 不碰黑名单路径就会被自动部署到全局 —— 在"用 claw 改 claw"的场景下代价是 brick。
fn run_test_gate(source_root: &Path) -> Option<String> {
    run_test_gate_with(source_root, |root, cmd| {
        let mut parts = cmd.split_whitespace();
        let prog = parts.next().ok_or_else(|| "空命令".to_string())?;
        let args: Vec<&str> = parts.collect();
        let status = std::process::Command::new(prog)
            .args(&args)
            .current_dir(root)
            .status()
            .map_err(|e| format!("启动失败: {e}"))?;
        Ok(status.success())
    })
}

/// 单条提案的最大尝试次数。超过则移入 `quarantine/`,不再自动重试。
///
/// **为什么必须有**: `run_pipeline` 取 inbox 排序后的第一条。若某条提案永远
/// 修不好(无法复现/与现有架构冲突),它会被每轮重试,既永久占住队首把后面
/// 几十条堵死,又每 30 分钟烧一次完整进化会话的 token。自动化系统里这是死锁。
pub const MAX_ATTEMPTS: u32 = 3;

/// 隔离目录名:超过重试上限的提案移入此处,留档但不再自动处理。
pub const QUARANTINE_DIR: &str = "quarantine";

/// 尝试计数目录名(与 inbox 同级,文件名与提案同名 + `.n`)。
pub const ATTEMPTS_DIR: &str = ".attempts";

/// 读取某条提案已尝试次数。
#[must_use]
pub fn attempts_of(inbox: &Path, proposal: &Path) -> u32 {
    let Some(name) = proposal.file_name().and_then(|n| n.to_str()) else {
        return 0;
    };
    std::fs::read_to_string(inbox.join(ATTEMPTS_DIR).join(format!("{name}.n")))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// 记录一次失败尝试,返回累计次数。失败静默(顶多退化为多试几次)。
pub fn bump_attempts(inbox: &Path, proposal: &Path) -> u32 {
    let n = attempts_of(inbox, proposal) + 1;
    let Some(name) = proposal.file_name().and_then(|f| f.to_str()) else {
        return n;
    };
    let dir = inbox.join(ATTEMPTS_DIR);
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join(format!("{name}.n")), format!("{n}\n"));
    }
    n
}

/// 把超出重试上限的提案移入 `quarantine/`,避免继续阻塞队首。
pub fn quarantine_proposal(inbox: &Path, proposal: &Path) -> Result<PathBuf, String> {
    let dir = inbox.join(QUARANTINE_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败: {e}", dir.display()))?;
    let name = proposal
        .file_name()
        .ok_or_else(|| format!("无效提案路径: {}", proposal.display()))?;
    let dst = dir.join(name);
    std::fs::rename(proposal, &dst)
        .map_err(|e| format!("隔离 {} 失败: {e}", proposal.display()))?;
    Ok(dst)
}

/// 从待处理列表中挑出**可尝试**的第一条(跳过已超上限者)。
///
/// 防御性过滤: 正常情况下超限提案已被移入 quarantine, 但进程可能在
/// `bump_attempts` 之后、`quarantine_proposal` 之前被杀, 留下超限残留。
#[must_use]
pub fn next_attemptable(inbox: &Path, pending: &[PathBuf]) -> Option<PathBuf> {
    pending
        .iter()
        .find(|p| attempts_of(inbox, p) < MAX_ATTEMPTS)
        .cloned()
}

/// 记录一次失败尝试,达上限则隔离。返回一段可读后缀,拼进错误信息。
///
/// 达到上限必须**移出 inbox**: 否则它会一直排在队首(排序稳定), 每轮被选中、
/// 每轮失败, 后面几十条永远轮不到 —— 自动化系统里的饥饿死锁。
fn note_failure(inbox: &Path, proposal: &Path) -> String {
    let n = bump_attempts(inbox, proposal);
    if n < MAX_ATTEMPTS {
        return format!("(第 {n}/{MAX_ATTEMPTS} 次尝试, 保留待重试)");
    }
    match quarantine_proposal(inbox, proposal) {
        Ok(_) => format!("(已尝试 {n} 次, 移入 quarantine 不再自动重试)"),
        Err(e) => format!("(已尝试 {n} 次, 隔离失败: {e})"),
    }
}

/// 成功处理后清理尝试计数,避免残留文件被下一条同名提案误用。
fn clear_attempts(inbox: &Path, proposal: &Path) {
    if let Some(name) = proposal.file_name().and_then(|n| n.to_str()) {
        let _ = std::fs::remove_file(inbox.join(ATTEMPTS_DIR).join(format!("{name}.n")));
    }
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
    // 跳过已达重试上限者: 一条永远修不好的提案若一直占队首, 会把它后面
    // 几十条全部饿死, 且每轮重试都烧一次完整进化会话。
    let Some(proposal_path) = next_attemptable(&inbox, &pending) else {
        return Err("inbox 无待处理提案(或均已超重试上限)".to_string());
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
        // 失败不部署、不标记已处理 —— 提案留在 inbox 等下次重试, 但要计入
        // 尝试次数: 反复修不好的提案必须到期隔离, 否则永久占住队首。
        let tag = note_failure(&inbox, &proposal_path);
        return Err(format!("进化子智能体退出码非零: {status}(未部署){tag}"));
    }

    // 2. 部署前硬校验。
    //    ⚠ 必须先确认"确实产生了新提交": 只看 `git show HEAD` 的文件清单不够 ——
    //    子智能体若**没提交**, HEAD 仍指向上一个提交、清单非空, 守卫会失效,
    //    进而把**旧代码**当成本次修复部署出去。故比对运行前后的 HEAD。
    let head_after = head_sha(source_root);
    if head_after.is_empty() || head_after == head_before {
        let tag = note_failure(&inbox, &proposal_path);
        return Err(format!("子智能体未产生新提交,跳过部署{tag}"));
    }
    let files = committed_file_list(source_root);
    if files.trim().is_empty() {
        let tag = note_failure(&inbox, &proposal_path);
        return Err(format!("无法读取本次提交的文件清单,跳过部署{tag}"));
    }
    if let Some(frag) = crate::evolution_runner::diff_violates_forbidden(&files) {
        // 黑名单命中是**安全否决**, 不是"再试一次能好"的失败: 同一条提案
        // 重试必然还是同一批文件。直接隔离, 不留它反复消耗。
        let _ = quarantine_proposal(&inbox, &proposal_path);
        return Err(format!(
            "改动命中黑名单 `{frag}`,拒绝部署(已隔离,不再自动重试)"
        ));
    }

    // 2.5 测试门:由**代码**跑,不信提示词。
    //    `evolution_runner::test_gate_commands` 早已定义门禁命令, 但此前只拼进
    //    提示词、无任何执行者 —— 子智能体跳过测试直接提交也能走到部署。本节
    //    补上执行侧: 任一门失败即中止, 提案留 inbox 计次重试, 绝不半截部署。
    if let Some(failed) = run_test_gate(source_root) {
        let tag = note_failure(&inbox, &proposal_path);
        return Err(format!("测试门未过(`{failed}`),拒绝部署{tag}"));
    }

    // 3. 自动部署到全局。失败则回滚,提案保留。
    let installed = match install_binary(source_root) {
        Ok(p) => p,
        Err(e) => {
            let tag = note_failure(&inbox, &proposal_path);
            return Err(format!("{e}{tag}"));
        }
    };

    // 4. 标记提案已处理 + 清理尝试计数。
    let _ = crate::evolution_inbox::mark_processed(&inbox, &proposal_path);
    clear_attempts(&inbox, &proposal_path);
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

    /// 队首饥饿死锁回归:一条永远修不好的提案不得永久阻塞其后所有提案。
    ///
    /// 缺陷: `run_pipeline` 原先直接取 `pending.into_iter().next()`, 即排序后
    /// 第一条。若该条永远修不好(无法复现/与架构冲突), 它会被每轮重试:
    /// ①永久占住队首, 后面几十条饿死; ②每 30 分钟烧一次完整进化会话。
    #[test]
    fn exhausted_proposal_is_skipped_not_blocking_queue() {
        let tmp = std::env::temp_dir().join(format!(
            "claw-evo-attempts-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let first = tmp.join("100-bad.md");
        let second = tmp.join("200-good.md");
        std::fs::write(&first, "x").unwrap();
        std::fs::write(&second, "y").unwrap();
        let pending = vec![first.clone(), second.clone()];

        // 初始:两条都可尝试, 选第一条
        assert_eq!(next_attemptable(&tmp, &pending), Some(first.clone()));

        // 第一条失败到上限 → 必须被跳过, 轮到第二条
        for _ in 0..MAX_ATTEMPTS {
            bump_attempts(&tmp, &first);
        }
        assert_eq!(attempts_of(&tmp, &first), MAX_ATTEMPTS);
        assert_eq!(
            next_attemptable(&tmp, &pending),
            Some(second.clone()),
            "超限提案仍占队首 ⇒ 后面全部饿死"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn quarantine_moves_proposal_out_of_pending() {
        let tmp = std::env::temp_dir().join(format!(
            "claw-evo-quar-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let p = tmp.join("100-bad.md");
        std::fs::write(&p, "x").unwrap();

        let dst = quarantine_proposal(&tmp, &p).expect("quarantine");
        assert!(!p.exists(), "原文件应已移走");
        assert!(dst.exists(), "应已移入 quarantine");
        assert!(dst.to_string_lossy().contains(QUARANTINE_DIR));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn all_proposals_exhausted_returns_none() {
        // 全部超限 ⇒ 无目标(而不是反复重试第一条)
        let tmp = std::env::temp_dir().join(format!(
            "claw-evo-allbad-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let p = tmp.join("100-bad.md");
        std::fs::write(&p, "x").unwrap();
        for _ in 0..MAX_ATTEMPTS {
            bump_attempts(&tmp, &p);
        }
        assert_eq!(next_attemptable(&tmp, &[p]), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_gate_passes_when_all_commands_succeed() {
        // 全过 ⇒ None(允许继续部署)
        let root = Path::new("/src");
        let got = run_test_gate_with(root, |_, _| Ok(true));
        assert_eq!(got, None);
    }

    #[test]
    fn test_gate_stops_at_first_failure() {
        // 首个失败即返回, 且返回的是**失败的那条命令**(便于日志定位)
        let root = Path::new("/src");
        let mut seen = 0;
        let got = run_test_gate_with(root, |_, _| {
            seen += 1;
            Ok(seen == 1) // 第一条过, 第二条挂
        });
        let expected = crate::evolution_runner::test_gate_commands()[1];
        assert_eq!(got.as_deref(), Some(expected), "应返回第二条(失败)命令");
        assert_eq!(seen, 2, "失败后不应再跑后续命令");
    }

    #[test]
    fn test_gate_treats_spawn_error_as_failure() {
        // 命令启动不起来(cargo 不在 PATH 等)也算门不过 —— 绝不能放行部署
        let root = Path::new("/src");
        let got = run_test_gate_with(root, |_, cmd| Err(format!("启动失败: {cmd}")));
        assert!(got.is_some(), "启动失败必须视为门未过");
        assert!(
            got.unwrap().contains("启动失败"),
            "错误信息应保留原因, 便于日志判断是环境问题还是测试红灯"
        );
    }
}
