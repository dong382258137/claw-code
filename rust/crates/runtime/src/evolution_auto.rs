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

/// 部署完成标记:由进化进程写,父进程(有会话可重启者)读取后触发热升级。
///
/// 这是断点③的解法核心 —— 部署动作发生在哪个进程,热升级就必须发生在
/// **能重启会话的那个进程**,两者不能混为一谈。
pub const DEPLOYED_MARKER_REL: &str = ".claw/evolution/deployed.json";

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

/// 部署完成标记的绝对路径。
#[must_use]
pub fn deployed_marker_path(workspace: &Path) -> std::path::PathBuf {
    workspace.join(DEPLOYED_MARKER_REL)
}

/// 写入部署完成标记(由执行部署的进程调用)。
///
/// `commit` 为本次部署对应的提交号/版本标签,供热升级前后对比。
pub fn write_deployed_marker(workspace: &Path, commit: &str) -> Result<std::path::PathBuf, String> {
    let path = deployed_marker_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建目录 {} 失败: {e}", parent.display()))?;
    }
    let body = format!("{{\"commit\":\"{commit}\"}}\n");
    std::fs::write(&path, body).map_err(|e| format!("写 {} 失败: {e}", path.display()))?;
    Ok(path)
}

/// 读取并**消费**部署标记(读完即删,保证一次部署只触发一次热升级)。
///
/// 返回标记里的 commit 字符串;无标记或内容损坏时返回 `None`。
#[must_use]
pub fn take_deployed_marker(workspace: &Path) -> Option<String> {
    let path = deployed_marker_path(workspace);
    let text = std::fs::read_to_string(&path).ok()?;
    // 消费语义:无论内容是否可解析,读走即删,避免死循环触发。
    let _ = std::fs::remove_file(&path);
    let start = text.find("\"commit\"")?;
    let rest = &text[start..];
    let colon = rest.find(':')?;
    let quoted = rest[colon + 1..].trim_start();
    let quoted = quoted.strip_prefix('"')?;
    let end = quoted.find('"')?;
    Some(quoted[..end].to_string())
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

    #[test]
    fn deployed_marker_roundtrip_is_consumed_once() {
        let tmp = std::env::temp_dir().join(format!(
            "claw-evo-marker-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let path = write_deployed_marker(&tmp, "abc1234").expect("write marker");
        assert!(path.is_file());
        assert_eq!(take_deployed_marker(&tmp).as_deref(), Some("abc1234"));
        // 消费语义:第二次必须为空,否则父进程会反复热升级
        assert_eq!(take_deployed_marker(&tmp), None);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
