//! Subworkspace discovery — 目录层级控制的基础设施（设计文档
//! `docs/2026-08-11-dir-hierarchy-control-design.md` §2.1）。
//!
//! 从 workspace_root 递归向下发现子工作区（monorepo 中的子 crate /
//! 嵌套项目），供 `dispatch_subagent` 的 `workspace` 字段校验与目录绑定派发使用。
//!
//! # 设计要点
//! - 项目目录判定标记镜像自 `rusty-claude-cli/src/tui-ports/project_picker.rs`
//!   `is_project_dir`（runtime 不能依赖 rusty-claude-cli，避免循环依赖，故复制常量）。
//! - 跳过目录：`.git` / `target` / `node_modules` / `.claw`（避免把内部状态目录当工作区）。
//! - 最大深度 [`MAX_DISCOVER_DEPTH`]（默认 4），防大 monorepo 全量扫描。
//! - 结果缓存到 `.claw/subworkspaces.json`，`discover_subworkspaces_cached` 命中即返回。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 子工作区缓存文件名（相对 workspace_root 的 `.claw/` 下）。
pub const SUBWORKSPACE_CACHE_FILENAME: &str = "subworkspaces.json";

/// 目录发现的默认最大深度。
pub const MAX_DISCOVER_DEPTH: usize = 4;

/// 扫描时跳过的目录名。
pub const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".claw"];

/// 项目目录标记（与 `project_picker::is_project_dir` 保持一致）。
const PROJECT_MARKERS: &[&str] = &[
    ".git",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    ".hg",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
];

/// 子工作区条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subworkspace {
    /// 绝对路径（已 canonicalize）。
    pub path: PathBuf,
    /// 相对 workspace_root 的路径，如 "crates/api"。
    pub relative_path: String,
    /// 命中的项目标记文件名，如 ["Cargo.toml"]。
    pub markers: Vec<String>,
    /// 是否存在独立配置（`.claw.json` 或 `.claw/settings.json`）。
    pub has_own_config: bool,
    /// 距根的深度（根 = 0，子 = 1，…）。
    pub depth: usize,
}

/// 判定 `path` 是否是"项目目录"（含任一常见项目标记文件）。
///
/// 与 `rusty-claude-cli/src/tui-ports/project_picker.rs::is_project_dir`
/// 的标记集合保持一致（镜像，避免 runtime → rusty-claude-cli 循环依赖）。
#[must_use]
pub fn is_project_dir(path: &Path) -> bool {
    PROJECT_MARKERS
        .iter()
        .any(|marker| path.join(marker).exists())
}

/// 从 workspace_root 同步发现全部子工作区（不含根自身）。
///
/// 返回按 `relative_path` 字典序排序的列表。扫描是纯文件系统只读操作，
/// 对 demo-monorepo 级别仓库毫秒级完成；超大仓库可配合缓存
/// [`discover_subworkspaces_cached`] 或未来 Epic 3 的后台异步构建。
pub fn discover_subworkspaces(workspace_root: &Path) -> Result<Vec<Subworkspace>, String> {
    let root = workspace_root.canonicalize().map_err(|e| {
        format!(
            "canonicalize workspace_root {} failed: {e}",
            workspace_root.display()
        )
    })?;
    let mut out = Vec::new();
    let mut seen = HashMap::<PathBuf, usize>::new();
    walk(&root, &root, 0, &mut out, &mut seen)?;
    out.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(out)
}

/// 递归扫描目录树。
///
/// - `root`：canonicalize 后的 workspace 根。
/// - `dir`：当前扫描目录。
/// - `depth`：当前深度（根 = 0）。
/// - `seen`：去重（Windows 符号链接环防护）。
fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<Subworkspace>,
    seen: &mut HashMap<PathBuf, usize>,
) -> Result<(), String> {
    if depth >= MAX_DISCOVER_DEPTH {
        return Ok(());
    }
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    if let Some(&seen_depth) = seen.get(&canonical) {
        if seen_depth <= depth {
            return Ok(()); // 已访问过，防环
        }
    }
    seen.insert(canonical.clone(), depth);

    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("read_dir {} failed: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if SKIP_DIRS.contains(&name_str.as_ref()) {
            continue;
        }
        if is_project_dir(&path) {
            let relative_path = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/"); // 跨平台统一正斜杠(Windows 下 path 用 \)
            out.push(Subworkspace {
                path: path.clone(),
                relative_path,
                markers: project_markers(&path),
                has_own_config: path.join(".claw.json").exists()
                    || path.join(".claw").join("settings.json").exists(),
                depth,
            });
        }
        // 继续向深层递归（支持嵌套工作区），深度上限由 walk 顶部把关。
        walk(root, &path, depth + 1, out, seen)?;
    }
    Ok(())
}

/// 收集目录命中的项目标记文件名。
fn project_markers(dir: &Path) -> Vec<String> {
    PROJECT_MARKERS
        .iter()
        .filter(|marker| dir.join(marker).exists())
        .map(|marker| (*marker).to_string())
        .collect()
}

/// 缓存文件路径：`{workspace_root}/.claw/subworkspaces.json`。
#[must_use]
pub fn cache_path(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(".claw")
        .join(SUBWORKSPACE_CACHE_FILENAME)
}

/// 优先读缓存；缓存缺失时执行扫描并写缓存。
pub fn discover_subworkspaces_cached(workspace_root: &Path) -> Result<Vec<Subworkspace>, String> {
    if let Some(cached) = read_cache(workspace_root) {
        return Ok(cached);
    }
    let discovered = discover_subworkspaces(workspace_root)?;
    let _ = write_cache(workspace_root, &discovered); // 缓存失败不阻塞调用方
    Ok(discovered)
}

/// 读取缓存（best-effort：损坏/缺失返回 None）。
pub fn read_cache(workspace_root: &Path) -> Option<Vec<Subworkspace>> {
    let path = cache_path(workspace_root);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 写入缓存（best-effort，原子写：先写 `{dir}/.claw/tmp/` 再 rename）。
pub fn write_cache(workspace_root: &Path, subworkspaces: &[Subworkspace]) -> Result<(), String> {
    let path = cache_path(workspace_root);
    let dir = path.parent().ok_or("cache path has no parent")?;
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("create_dir_all {} failed: {e}", dir.display()))?;
    let tmp_dir = workspace_root.join(".claw").join("tmp");
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("create tmp dir failed: {e}"))?;
    let tmp_path = tmp_dir.join(SUBWORKSPACE_CACHE_FILENAME);
    let bytes = serde_json::to_vec_pretty(subworkspaces)
        .map_err(|e| format!("serialize subworkspaces failed: {e}"))?;
    std::fs::write(&tmp_path, &bytes).map_err(|e| format!("write tmp cache failed: {e}"))?;
    std::fs::rename(&tmp_path, &path).map_err(|e| format!("rename cache failed: {e}"))
}

/// 解析并校验 `workspace` 相对路径参数。
///
/// 校验规则（设计文档 §2.2 第 1 步）：
/// 1. 必须是相对路径（拒绝绝对路径）。
/// 2. canonicalize 后必须严格位于 workspace_root 之内（拒绝 `..` 逃逸与等于根）。
/// 3. 必须存在于 [`discover_subworkspaces`] 的结果中。
///
/// 返回校验通过的绝对路径。
pub fn resolve_subworkspace(workspace_root: &Path, workspace: &str) -> Result<PathBuf, String> {
    let raw = Path::new(workspace);
    if raw.is_absolute() {
        return Err(format!(
            "invalid workspace '{workspace}': absolute paths are not allowed"
        ));
    }
    let root = workspace_root
        .canonicalize()
        .map_err(|e| format!("canonicalize workspace_root failed: {e}"))?;
    let candidate = root.join(raw);
    let resolved = candidate
        .canonicalize()
        .map_err(|e| format!("invalid workspace '{workspace}': path does not exist: {e}"))?;
    if resolved == root {
        return Err(format!(
            "invalid workspace '{workspace}': workspace must be a subdirectory, not the workspace root"
        ));
    }
    if !resolved.starts_with(&root) {
        return Err(format!(
            "invalid workspace '{workspace}': path escapes workspace root"
        ));
    }
    if !resolved.is_dir() {
        return Err(format!("invalid workspace '{workspace}': not a directory"));
    }
    // 用缓存版本：派发路径每次调用都会走到这里，全量扫描大 monorepo 代价高。
    let discovered = discover_subworkspaces_cached(&root)?;
    if !discovered.iter().any(|s| s.path == resolved) {
        return Err(format!(
            "invalid workspace '{workspace}': no project markers found (expected one of {})",
            PROJECT_MARKERS.join(" / ")
        ));
    }
    Ok(resolved)
}

/// Epic 1 T5(TOCTOU 缓解):子代理 turn 开始处对派发时解析的 workspace
/// 目录重新校验。
///
/// 派发时 [`resolve_subworkspace`] 的校验结果是缓存快照,子代理实际执行
/// 期间目录可能变化(被删除/替换为 symlink/项目标记消失)。本函数重新
/// canonicalize 并复核,返回重新解析的绝对路径(供 scope/handoff 基准
/// 使用),失败返回 `Err` → 子代理首轮即被拒。
///
/// 行为预期保持"false-negative 安全方向可接受"(设计文档 T5):目录状态
/// 变化只会导致误拒(而非误放)。
///
/// 校验规则与 [`resolve_subworkspace`] 一致:
/// 1. canonicalize 成功(目录仍存在)。
/// 2. canonicalize 后严格位于 `workspace_root` 之内(拒绝 symlink 逃逸与等于根)。
/// 3. 是目录。
/// 4. 仍存在于 [`discover_subworkspaces_cached`] 结果中(项目标记未消失)。
pub fn revalidate_subworkspace(workspace_root: &Path, resolved: &Path) -> Result<PathBuf, String> {
    let root = workspace_root
        .canonicalize()
        .map_err(|e| format!("canonicalize workspace_root failed: {e}"))?;
    let re_resolved = resolved
        .canonicalize()
        .map_err(|e| format!("workspace directory no longer exists: {e}"))?;
    if re_resolved == root {
        return Err(
            "invalid workspace: workspace must be a subdirectory, not the workspace root"
                .to_string(),
        );
    }
    if !re_resolved.starts_with(&root) {
        return Err(format!(
            "invalid workspace: path escapes workspace root: {}",
            re_resolved.display()
        ));
    }
    if !re_resolved.is_dir() {
        return Err(format!(
            "invalid workspace: not a directory: {}",
            re_resolved.display()
        ));
    }
    let discovered = discover_subworkspaces_cached(&root)?;
    if !discovered.iter().any(|s| s.path == re_resolved) {
        return Err(format!(
            "invalid workspace: no project markers found (expected one of {})",
            PROJECT_MARKERS.join(" / ")
        ));
    }
    Ok(re_resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        // crates/api + crates/core 是项目目录
        fs::create_dir_all(root.join("crates/api/src")).unwrap();
        fs::create_dir_all(root.join("crates/core/src")).unwrap();
        fs::create_dir_all(root.join("crates/app/src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(root.join("crates/api/Cargo.toml"), "[package]").unwrap();
        fs::write(root.join("crates/core/Cargo.toml"), "[package]").unwrap();
        // crates/app 无项目标记 → 不应被记录
        // 应跳过的目录
        fs::create_dir_all(root.join("crates/api/target/debug")).unwrap();
        fs::write(root.join("crates/api/target/debug/x.rs"), "x").unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref").unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/package.json"), "{}").unwrap();
        // crates/api 有独立配置
        fs::write(root.join("crates/api/.claw.json"), "{}").unwrap();
        dir
    }

    #[test]
    fn discover_finds_project_dirs_and_skips_junk() {
        let fixture = make_fixture();
        let root = fixture.path();
        let found = discover_subworkspaces(root).expect("discover should succeed");
        let rels: Vec<&str> = found.iter().map(|s| s.relative_path.as_str()).collect();
        assert_eq!(rels, vec!["crates/api", "crates/core"]);
        // 跳过 target/node_modules/.git 内的内容
        assert!(!rels
            .iter()
            .any(|r| r.contains("target") || r.contains("node_modules") || r.contains(".git")));
    }

    #[test]
    fn discover_records_markers_and_config() {
        let fixture = make_fixture();
        let found = discover_subworkspaces(fixture.path()).expect("discover");
        let api = found
            .iter()
            .find(|s| s.relative_path == "crates/api")
            .expect("api");
        assert!(api.markers.contains(&"Cargo.toml".to_string()));
        assert!(api.has_own_config);
        let core = found
            .iter()
            .find(|s| s.relative_path == "crates/core")
            .expect("core");
        assert!(!core.has_own_config);
        assert_eq!(api.depth, 1);
    }

    #[test]
    fn cache_roundtrip_returns_same_data() {
        let fixture = make_fixture();
        let root = fixture.path();
        let discovered = discover_subworkspaces(root).expect("discover");
        write_cache(root, &discovered).expect("write cache");
        let cached = read_cache(root).expect("read cache");
        assert_eq!(cached, discovered);
    }

    #[test]
    fn resolve_rejects_absolute_and_escape() {
        let fixture = make_fixture();
        let root = fixture.path();
        assert!(resolve_subworkspace(root, "/etc/passwd").is_err());
        assert!(resolve_subworkspace(root, "..").is_err());
        assert!(resolve_subworkspace(root, "crates/api/../../..").is_err());
    }

    #[test]
    fn resolve_accepts_valid_subworkspace() {
        let fixture = make_fixture();
        let root = fixture.path();
        let resolved = resolve_subworkspace(root, "crates/api").expect("valid workspace");
        assert!(resolved.ends_with("crates/api"));
    }

    #[test]
    fn resolve_rejects_non_project_dir() {
        let fixture = make_fixture();
        let root = fixture.path();
        // crates/app 无项目标记
        let err = resolve_subworkspace(root, "crates/app").unwrap_err();
        assert!(err.contains("no project markers"), "got: {err}");
    }

    // Epic 1 T5(TOCTOU 缓解):目录未变时 revalidate 通过并返回同一绝对路径。
    #[test]
    fn revalidate_accepts_unchanged_subworkspace() {
        let fixture = make_fixture();
        let root = fixture.path();
        let resolved = resolve_subworkspace(root, "crates/api").expect("resolve");
        let re = revalidate_subworkspace(root, &resolved).expect("revalidate");
        assert_eq!(
            re, resolved,
            "unchanged workspace should revalidate to same path"
        );
    }

    // Epic 1 T5(TOCTOU 缓解):派发后删除子目录 → revalidate 拒绝(目录不存在)。
    #[test]
    fn revalidate_rejects_after_removal() {
        let fixture = make_fixture();
        let root = fixture.path();
        let resolved = resolve_subworkspace(root, "crates/api").expect("resolve");
        fs::remove_dir_all(root.join("crates/api")).expect("remove subdir");
        let err = revalidate_subworkspace(root, &resolved).unwrap_err();
        assert!(err.contains("no longer exists"), "got: {err}");
    }

    // Epic 1 T5(TOCTOU 缓解):传入 root 本身 → 拒绝(必须为子目录)。
    #[test]
    fn revalidate_rejects_workspace_root() {
        let fixture = make_fixture();
        let root = fixture.path().canonicalize().expect("canonicalize root");
        let err = revalidate_subworkspace(&root, &root).unwrap_err();
        assert!(err.contains("subdirectory"), "got: {err}");
    }
}
