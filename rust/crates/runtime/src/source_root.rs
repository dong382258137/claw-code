//! 源码根注册表 —— 热升级元认知闭环的地基(P0)。
//!
//! 问题:claw 在**任意目录**执行任务时(如 `D:\chanlunV2`),无法得知自己的
//! **源码根**在何处(如 `D:\claw-code-src`)。而 `upgrade_helper` 现从 `user_cwd`
//! 推源码根 —— 在任意目录下**必然失败**(见 `upgrade_helper.rs` 中
//! `workspace_root_from_user_cwd` 的注释:部署版 exe 位于 `~/.cargo/bin`,
//! 上溯用户主目录不含 Cargo.toml)。
//!
//! 本模块提供**注册表**:把源码根路径记到 `~/.claw/source-root`(单行文本),
//! 使任意目录下运行的热升级/元认知归档都能定位源码树。
//!
//! 解析优先级(高→低):
//! 1. 环境变量 `CLAW_SOURCE_ROOT`(便于测试/多源码树切换)
//! 2. `~/.claw/source-root` 注册表
//! 3. `None`(调用方回退:从 exe / user_cwd 上溯)
//!
//! 校验:注册与解析均要求目录是 `rusty-claude-cli` 的 cargo workspace
//! (含 `crates/rusty-claude-cli/Cargo.toml`),避免误注册无关仓库。

use std::path::{Path, PathBuf};

/// 注册表文件名(位于 [`default_source_registry_dir`] 下)。
pub const SOURCE_ROOT_FILE: &str = "source-root";

/// 环境变量名:显式指定源码根(最高优先级)。
pub const SOURCE_ROOT_ENV: &str = "CLAW_SOURCE_ROOT";

/// 判断目录是否为 `rusty-claude-cli` 的 cargo workspace。
///
/// 判定不能用"含 Cargo.toml" —— 仓库内可能有多个含 Cargo.toml 的目录
/// (本项目根下同时有 `rust/` 与 `demo-monorepo/`)。可靠判定:该目录是
/// cargo workspace 且含 `crates/rusty-claude-cli` 包。
///
/// 注:workspace 根可能是仓库根(`D:\claw-code-src`)或 `rust/` 子目录,
/// 两种布局都可接受 —— 只要其下能找到 `crates/rusty-claude-cli`。
#[must_use]
pub fn is_claw_workspace(dir: &Path) -> bool {
    dir.join("crates")
        .join("rusty-claude-cli")
        .join("Cargo.toml")
        .is_file()
}

/// 注册表所在目录(复用全局 claw 配置目录 `~/.claw`)。
#[must_use]
pub fn default_source_registry_dir() -> PathBuf {
    crate::config::default_config_home()
}

/// 注册表文件完整路径。
#[must_use]
pub fn source_root_file_path() -> PathBuf {
    default_source_registry_dir().join(SOURCE_ROOT_FILE)
}

/// 读取注册表中的源码根(仅读文件,不含 env)。
///
/// 返回 `None` 当:文件不存在 / 读失败 / 内容为空 / 路径已失效(目录不存在)。
#[must_use]
pub fn read_registered_source_root() -> Option<PathBuf> {
    let text = std::fs::read_to_string(source_root_file_path()).ok()?;
    let line = text.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    let path = PathBuf::from(line);
    path.is_dir().then_some(path)
}

/// 解析源码根(完整优先级:env → 注册表 → None)。
///
/// 环境变量值即便未通过 workspace 校验也**直接采用**(允许测试指向临时目录,
/// 也允许用户显式覆盖而不被校验挡住);注册表读取则要求目录仍存在。
#[must_use]
pub fn resolve_source_root() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os(SOURCE_ROOT_ENV) {
        let path = PathBuf::from(raw);
        if path.is_dir() {
            return Some(path);
        }
    }
    read_registered_source_root()
}

/// 把 `source_root` 登记到注册表(`~/.claw/source-root`)。
///
/// 校验:必须是 claw workspace(见 [`is_claw_workspace`]),否则返回 `Err` ——
/// 防误注册无关仓库导致后续热升级在错误目录 `cargo build`。
/// 幂等:重复登记同一路径无副作用。
pub fn register_source_root(source_root: &Path) -> Result<PathBuf, String> {
    if !is_claw_workspace(source_root) {
        return Err(format!(
            "{} 不是 claw workspace(未找到 crates/rusty-claude-cli/Cargo.toml),拒绝登记",
            source_root.display()
        ));
    }
    let dir = default_source_registry_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败: {e}", dir.display()))?;
    let file = source_root_file_path();
    std::fs::write(&file, format!("{}\n", source_root.display()))
        .map_err(|e| format!("写 {} 失败: {e}", file.display()))?;
    Ok(file)
}

/// 若 `dir`(或其父链)是 claw workspace,则登记并返回源码根。
///
/// 供"在源码树内运行"时自动登记(零配置)。不是 workspace 则返回 `None`,不报错。
pub fn maybe_register_from(dir: &Path) -> Option<PathBuf> {
    let mut cursor = Some(dir);
    while let Some(cur) = cursor {
        if is_claw_workspace(cur) {
            let _ = register_source_root(cur);
            return Some(cur.to_path_buf());
        }
        cursor = cur.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_claw_workspace_detects_this_repo() {
        // 本测试运行于 runtime crate,其 workspace 根含 crates/rusty-claude-cli。
        // 从 CARGO_MANIFEST_DIR 上溯找到 workspace 根。
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let found = {
            let mut cursor = Some(manifest.as_path());
            let mut hit = None;
            while let Some(cur) = cursor {
                if is_claw_workspace(cur) {
                    hit = Some(cur.to_path_buf());
                    break;
                }
                cursor = cur.parent();
            }
            hit
        };
        assert!(
            found.is_some(),
            "应能从 {manifest:?} 上溯找到含 crates/rusty-claude-cli 的 workspace"
        );
    }

    #[test]
    fn is_claw_workspace_rejects_plain_dir() {
        let tmp = std::env::temp_dir().join(format!("claw-srcroot-neg-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).ok();
        assert!(!is_claw_workspace(&tmp));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn register_rejects_non_workspace() {
        let tmp = std::env::temp_dir().join(format!("claw-srcroot-bad-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).ok();
        let err = register_source_root(&tmp).expect_err("非 workspace 应被拒绝");
        assert!(err.contains("不是 claw workspace"), "err={err}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn resolve_prefers_env_then_registry() {
        // 注:不真的写全局注册表(避免污染),仅验证 env 分支与'null 目录被忽略'。
        let tmp = std::env::temp_dir().join(format!("claw-srcroot-env-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).ok();

        // 保存并恢复 env(测试内全局状态需自守)。
        let saved = std::env::var_os(SOURCE_ROOT_ENV);
        std::env::set_var(SOURCE_ROOT_ENV, &tmp);
        assert_eq!(resolve_source_root().as_deref(), Some(tmp.as_path()));

        // 指向不存在的目录 → env 分支失效(应回退注册表,此处不写注册表 → None)
        std::env::set_var(SOURCE_ROOT_ENV, tmp.join("does-not-exist"));
        assert_eq!(resolve_source_root(), None);

        match saved {
            Some(v) => std::env::set_var(SOURCE_ROOT_ENV, v),
            None => std::env::remove_var(SOURCE_ROOT_ENV),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }
}
