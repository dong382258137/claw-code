use runtime::{permission_enforcer::PermissionEnforcer, PermissionMode, PermissionPolicy};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tools::{mvp_tool_specs, GlobalToolRegistry, TurnEnv};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn temp_path(name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    std::env::temp_dir().join(format!("claw-path-scope-{unique}-{name}"))
}

fn workspace_write_registry() -> GlobalToolRegistry {
    let policy = mvp_tool_specs().into_iter().fold(
        PermissionPolicy::new(PermissionMode::WorkspaceWrite),
        |policy, spec| policy.with_tool_requirement(spec.name, spec.required_permission),
    );
    GlobalToolRegistry::builtin().with_enforcer(PermissionEnforcer::new(policy))
}

fn run_powershell(command: &str) -> Result<String, String> {
    workspace_write_registry().execute("PowerShell", &json!({ "command": command }))
}

/// 工作区绑定注册表：不再依赖进程 cwd，权限判定与执行共用同一 `TurnEnv`。
fn registry_for(workspace: &Path) -> GlobalToolRegistry {
    workspace_write_registry().with_turn_env(TurnEnv::new(workspace.to_path_buf(), Vec::new()))
}

fn assert_permission_denied(result: Result<String, String>, case_name: &str) {
    let err = result
        .unwrap_err_or_else(|ok| panic!("{case_name} should be denied before execution, got {ok}"));
    assert!(
        (err.contains("requires danger-full-access permission")
            || err.contains("requires \'danger-full-access\' permission"))
            || err.contains("current mode is workspace-write")
            || err.contains("escapes workspace"),
        "{case_name} should fail in permission enforcement, got: {err}"
    );
}

trait UnwrapErrOrElse<T, E> {
    fn unwrap_err_or_else<F: FnOnce(T) -> E>(self, op: F) -> E;
}

impl<T, E> UnwrapErrOrElse<T, E> for Result<T, E> {
    fn unwrap_err_or_else<F: FnOnce(T) -> E>(self, op: F) -> E {
        match self {
            Ok(value) => op(value),
            Err(error) => error,
        }
    }
}

#[test]
fn direct_paths_allow_workspace_file_and_deny_absolute_outside_file() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("direct");
    fs::create_dir_all(root.join("src")).expect("create workspace");
    fs::write(root.join("src/lib.rs"), "workspace\n").expect("write workspace file");
    let outside = temp_path("direct-outside.txt");
    fs::write(&outside, "secret\n").expect("write outside file");

    let registry = registry_for(&root);
    let allowed = registry
        .execute("bash", &json!({ "command": "cat src/lib.rs" }))
        .expect("workspace-relative read should execute");
    assert!(allowed.contains("workspace"));
    assert_permission_denied(
        registry.execute(
            "bash",
            &json!({ "command": format!("cat {}", outside.display()) }),
        ),
        "absolute outside file",
    );

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(outside);
}

#[test]
fn file_tool_direct_outside_path_is_denied_before_reading() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("file-tool-direct");
    fs::create_dir_all(&root).expect("create workspace");
    let outside = temp_path("file-tool-secret.txt");
    fs::write(&outside, "secret\n").expect("write outside file");

    let registry = registry_for(&root);
    assert_permission_denied(
        registry.execute(
            "read_file",
            &json!({ "path": outside.display().to_string() }),
        ),
        "read_file outside workspace",
    );

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(outside);
}

#[cfg(unix)]
#[test]
fn symlink_resolving_outside_workspace_is_denied_before_execution() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("symlink");
    fs::create_dir_all(&root).expect("create workspace");
    let outside = temp_path("symlink-secret.txt");
    fs::write(&outside, "secret\n").expect("write outside file");
    std::os::unix::fs::symlink(&outside, root.join("secret-link")).expect("create symlink");

    let registry = registry_for(&root);
    assert_permission_denied(
        registry.execute("bash", &json!({ "command": "cat secret-link" })),
        "outside symlink",
    );

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(outside);
}

#[test]
fn shell_expansion_and_glob_parent_traversal_are_denied_before_execution() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("expansion");
    fs::create_dir_all(&root).expect("create workspace");

    let registry = registry_for(&root);
    for (name, command) in [
        ("parent glob", "ls ../*"),
        ("PWD parent expansion", "cat $PWD/../secret.txt"),
        ("braced PWD parent expansion", "cat ${PWD}/../secret.txt"),
        (
            "command substitution parent expansion",
            "cat $(pwd)/../secret.txt",
        ),
    ] {
        assert_permission_denied(
            registry.execute("bash", &json!({ "command": command })),
            name,
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn nested_worktree_paths_are_allowed_but_parent_escape_is_denied() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("worktree");
    let worktree = root.join("main").join("linked-worktree");
    fs::create_dir_all(worktree.join("src")).expect("create worktree");
    fs::write(worktree.join("src/lib.rs"), "worktree\n").expect("write worktree file");

    let registry = registry_for(&worktree);
    let allowed = registry
        .execute("bash", &json!({ "command": "cat src/lib.rs" }))
        .expect("nested worktree-relative read should execute");
    assert!(allowed.contains("worktree"));
    assert_permission_denied(
        registry.execute("bash", &json!({ "command": "cat ../../outside.txt" })),
        "worktree parent escape",
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn windows_style_absolute_paths_are_denied_before_execution() {
    for (name, command) in [
        (
            "windows drive backslash",
            r"cat C:\Users\attacker\secret.txt",
        ),
        ("windows drive slash", r"cat C:/Users/attacker/secret.txt"),
    ] {
        assert_permission_denied(
            workspace_write_registry().execute("bash", &json!({ "command": command })),
            name,
        );
    }

    for (name, command) in [
        (
            "powershell windows drive backslash",
            r"Get-Content -Path C:\Users\attacker\secret.txt",
        ),
        (
            "powershell windows drive slash",
            r"Get-Content -Path C:/Users/attacker/secret.txt",
        ),
    ] {
        assert_permission_denied(run_powershell(command), name);
    }
}

/// 防回归：生产代码不得回退到"读进程 cwd / 临时切目录再恢复"的兼容路径。
///
/// 允许清单只包含显式的兼容包装（CLI/测试入口或 S2b 待办），其余命中即失败。
#[test]
fn production_code_does_not_depend_on_process_cwd() {
    const ALLOWED_FNS: &[&str] = &[
        // 兼容构造器：CLI/测试包装，等价于 `TurnEnv::new(env::current_dir(), vec![])`
        "for_current_dir",
        // 旧工具执行入口的默认 env
        "builtin",
        "with_plugin_tools",
        "execute_tool",
        // 明确留给 S2b 的 lane 溯源链路
        "maybe_commit_provenance",
    ];

    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    for entry in fs::read_dir(&src).expect("read tools/src") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("read source");
        let lines: Vec<&str> = text.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            if !line.contains("env::current_dir") && !line.contains("env::set_current_dir") {
                continue;
            }
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            let (indent, fn_name) = enclosing_fn(&lines, idx);
            // 缩进函数 = 测试模块内的辅助/用例，不在生产路径上。
            if indent > 0 || ALLOWED_FNS.contains(&fn_name.as_str()) {
                continue;
            }
            violations.push(format!(
                "{}:{} in fn {}: {}",
                path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                idx + 1,
                fn_name,
                trimmed
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "生产代码出现进程 cwd 依赖（请改为显式 workspace）:\n{}",
        violations.join("\n")
    );
}

fn enclosing_fn(lines: &[&str], idx: usize) -> (usize, String) {
    for j in (0..=idx).rev() {
        let line = lines[j];
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        if let Some(pos) = trimmed.find("fn ") {
            let rest = &trimmed[pos + 3..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return (line.len() - trimmed.len(), name);
            }
        }
    }
    (0, String::new())
}
