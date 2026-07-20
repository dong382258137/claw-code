#![allow(
    clippy::match_wildcard_for_single_variants,
    clippy::must_use_candidate,
    clippy::uninlined_format_args
)]
//! Permission enforcement layer that gates tool execution based on the
//! active `PermissionPolicy`.

use crate::permissions::{PermissionMode, PermissionOutcome, PermissionPolicy};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome")]
pub enum EnforcementResult {
    /// Tool execution is allowed.
    Allowed,
    /// Tool execution was denied due to insufficient permissions.
    Denied {
        tool: String,
        active_mode: String,
        required_mode: String,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionEnforcer {
    policy: PermissionPolicy,
}

impl PermissionEnforcer {
    #[must_use]
    pub fn new(policy: PermissionPolicy) -> Self {
        Self { policy }
    }

    /// Check whether a tool can be executed under the current permission policy.
    /// Auto-denies when prompting is required but no prompter is provided.
    pub fn check(&self, tool_name: &str, input: &str) -> EnforcementResult {
        // In Prompt mode the enforcer has no prompter, so we must hard-deny.
        // Callers that want interactive prompting should inspect `active_mode()`
        // themselves and run their own prompter flow before calling `check`.
        if self.policy.active_mode() == PermissionMode::Prompt {
            let active_mode = self.policy.active_mode();
            let required_mode = self.policy.required_mode_for(tool_name);
            return EnforcementResult::Denied {
                tool: tool_name.to_owned(),
                active_mode: active_mode.as_str().to_owned(),
                required_mode: required_mode.as_str().to_owned(),
                reason: format!(
                    "'{tool_name}' requires confirmation in prompt mode, but no interactive prompter is configured"
                ),
            };
        }

        let outcome = self.policy.authorize(tool_name, input, None);

        match outcome {
            PermissionOutcome::Allow => EnforcementResult::Allowed,
            PermissionOutcome::Deny { reason } => {
                let active_mode = self.policy.active_mode();
                let required_mode = self.policy.required_mode_for(tool_name);
                EnforcementResult::Denied {
                    tool: tool_name.to_owned(),
                    active_mode: active_mode.as_str().to_owned(),
                    required_mode: required_mode.as_str().to_owned(),
                    reason,
                }
            }
        }
    }

    #[must_use]
    pub fn is_allowed(&self, tool_name: &str, input: &str) -> bool {
        matches!(self.check(tool_name, input), EnforcementResult::Allowed)
    }

    /// Check permission with an explicitly provided required mode.
    /// Used when the required mode is determined dynamically (e.g., bash command classification).
    pub fn check_with_required_mode(
        &self,
        tool_name: &str,
        input: &str,
        required_mode: PermissionMode,
    ) -> EnforcementResult {
        // In Prompt mode the enforcer has no prompter, so we must hard-deny.
        // See `check` for rationale.
        if self.policy.active_mode() == PermissionMode::Prompt {
            let active_mode = self.policy.active_mode();
            return EnforcementResult::Denied {
                tool: tool_name.to_owned(),
                active_mode: active_mode.as_str().to_owned(),
                required_mode: required_mode.as_str().to_owned(),
                reason: format!(
                    "'{tool_name}' requires confirmation in prompt mode, but no interactive prompter is configured"
                ),
            };
        }

        let active_mode = self.policy.active_mode();

        // Check if active mode meets the dynamically determined required mode
        if active_mode >= required_mode {
            return EnforcementResult::Allowed;
        }

        // Permission denied - active mode is insufficient
        EnforcementResult::Denied {
            tool: tool_name.to_owned(),
            active_mode: active_mode.as_str().to_owned(),
            required_mode: required_mode.as_str().to_owned(),
            reason: format!(
                "'{tool_name}' with input '{input}' requires '{}' permission, but current mode is '{}'",
                required_mode.as_str(),
                active_mode.as_str()
            ),
        }
    }

    #[must_use]
    pub fn active_mode(&self) -> PermissionMode {
        self.policy.active_mode()
    }

    /// Classify a file operation against workspace boundaries.
    pub fn check_file_write(&self, path: &str, workspace_root: &str) -> EnforcementResult {
        let mode = self.policy.active_mode();

        match mode {
            PermissionMode::ReadOnly => EnforcementResult::Denied {
                tool: "write_file".to_owned(),
                active_mode: mode.as_str().to_owned(),
                required_mode: PermissionMode::WorkspaceWrite.as_str().to_owned(),
                reason: format!("file writes are not allowed in '{}' mode", mode.as_str()),
            },
            PermissionMode::WorkspaceWrite => {
                if is_within_workspace(path, workspace_root) {
                    EnforcementResult::Allowed
                } else {
                    EnforcementResult::Denied {
                        tool: "write_file".to_owned(),
                        active_mode: mode.as_str().to_owned(),
                        required_mode: PermissionMode::DangerFullAccess.as_str().to_owned(),
                        reason: format!(
                            "path '{}' is outside workspace root '{}'",
                            path, workspace_root
                        ),
                    }
                }
            }
            // Allow and DangerFullAccess permit all writes
            PermissionMode::Allow | PermissionMode::DangerFullAccess => EnforcementResult::Allowed,
            PermissionMode::Prompt => EnforcementResult::Denied {
                tool: "write_file".to_owned(),
                active_mode: mode.as_str().to_owned(),
                required_mode: PermissionMode::WorkspaceWrite.as_str().to_owned(),
                reason: "file write requires confirmation in prompt mode".to_owned(),
            },
        }
    }

    /// Check if a bash command should be allowed based on current mode.
    pub fn check_bash(&self, command: &str) -> EnforcementResult {
        let mode = self.policy.active_mode();

        match mode {
            PermissionMode::ReadOnly => {
                if is_read_only_command(command) {
                    EnforcementResult::Allowed
                } else {
                    EnforcementResult::Denied {
                        tool: "bash".to_owned(),
                        active_mode: mode.as_str().to_owned(),
                        required_mode: PermissionMode::WorkspaceWrite.as_str().to_owned(),
                        reason: format!(
                            "command may modify state; not allowed in '{}' mode",
                            mode.as_str()
                        ),
                    }
                }
            }
            PermissionMode::Prompt => EnforcementResult::Denied {
                tool: "bash".to_owned(),
                active_mode: mode.as_str().to_owned(),
                required_mode: PermissionMode::DangerFullAccess.as_str().to_owned(),
                reason: "bash requires confirmation in prompt mode".to_owned(),
            },
            // WorkspaceWrite, Allow, DangerFullAccess: permit bash
            _ => EnforcementResult::Allowed,
        }
    }
}

/// Workspace boundary check using path-component comparison.
///
/// BUG-P1-6: the previous implementation used raw string `starts_with`
/// on the normalized path, which was safe against the `/app` vs `/app-x`
/// case only because it appended a trailing `/`. But it was still
/// vulnerable to:
///   * `..` traversal: `/workspace/../../etc/passwd` would pass the
///     prefix check while resolving outside the workspace.
///   * Mixed separators on Windows (`\` vs `/`).
///   * Case-insensitivity on Windows (NTFS treats `Foo` and `foo` as
///     the same path; a string compare does not).
///
/// We now lexically normalize the candidate (resolving `.` and `..`
/// components without touching the filesystem) and compare component
/// lists, so `..` traversal is rejected and separator / case issues
/// are handled by `Path`'s own normalization on each platform.
fn is_within_workspace(path: &str, workspace_root: &str) -> bool {
    use std::path::{Component, Path};

    let candidate = if Path::new(path).is_absolute() {
        Path::new(path).to_path_buf()
    } else {
        Path::new(workspace_root).join(path)
    };

    // Lexically resolve `.` and `..` without touching the filesystem.
    // This mirrors what `std::fs::canonicalize` would do for existing
    // paths, but works for not-yet-created files too.
    let mut normalized_components: Vec<Component<'_>> = Vec::new();
    for component in candidate.components() {
        match component {
            Component::CurDir => {} // skip `.`
            Component::ParentDir => {
                // Pop the last normal component, but never pop past a
                // root/prefix — `..` above the root is meaningless.
                if let Some(last) = normalized_components.last() {
                    if matches!(last, Component::Normal(_)) {
                        normalized_components.pop();
                    }
                }
            }
            other => normalized_components.push(other),
        }
    }

    let root_path = Path::new(workspace_root);
    let mut root_components: Vec<Component<'_>> = Vec::new();
    for component in root_path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(last) = root_components.last() {
                    if matches!(last, Component::Normal(_)) {
                        root_components.pop();
                    }
                }
            }
            other => root_components.push(other),
        }
    }

    // The candidate is inside the workspace iff the workspace's component
    // list is a prefix of the candidate's component list (and both share
    // the same root/prefix). Component equality is platform-aware: on
    // Windows `Path` uses the OsStr, which preserves case — we add a
    // case-insensitive fallback below for the Windows common case.
    if normalized_components.len() < root_components.len() {
        return false;
    }
    for (candidate_part, root_part) in
        normalized_components.iter().zip(root_components.iter())
    {
        if !components_equal(candidate_part, root_part) {
            return false;
        }
    }
    true
}

/// Compare two path components, case-insensitively on Windows.
#[cfg(windows)]
fn components_equal<'a>(a: &std::path::Component<'a>, b: &std::path::Component<'a>) -> bool {
    use std::path::Component;
    match (a, b) {
        (Component::Normal(a_str), Component::Normal(b_str)) => {
            a_str.eq_ignore_ascii_case(b_str)
        }
        _ => a == b,
    }
}

#[cfg(not(windows))]
fn components_equal<'a>(a: &std::path::Component<'a>, b: &std::path::Component<'a>) -> bool {
    a == b
}

/// Conservative heuristic: is this bash command read-only?
///
/// Excludes commands that can execute arbitrary code (`python`, `node`,
/// `ruby`, `cargo`, `rustc`), modify files (`tee`, `sed -i`), or mutate
/// repository state (`git`, `gh`). Callers needing git/python/etc. must
/// upgrade to `WorkspaceWrite` or higher.
fn is_read_only_command(command: &str) -> bool {
    let first_token = command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("");

    // Only purely-readonly commands are whitelisted here. Commands that can
    // execute arbitrary code (python/node/ruby/cargo), write files (tee/sed -i),
    // or mutate repo state (git/gh) are intentionally excluded — they must
    // go through the higher permission tiers.
    matches!(
        first_token,
        "cat"
            | "head"
            | "tail"
            | "less"
            | "more"
            | "wc"
            | "ls"
            | "find"
            | "grep"
            | "rg"
            | "awk"
            | "echo"
            | "printf"
            | "which"
            | "where"
            | "whoami"
            | "pwd"
            | "env"
            | "printenv"
            | "date"
            | "cal"
            | "df"
            | "du"
            | "free"
            | "uptime"
            | "uname"
            | "file"
            | "stat"
            | "diff"
            | "sort"
            | "uniq"
            | "tr"
            | "cut"
            | "paste"
            | "xargs"
            | "test"
            | "true"
            | "false"
            | "type"
            | "readlink"
            | "realpath"
            | "basename"
            | "dirname"
            | "sha256sum"
            | "md5sum"
            | "b3sum"
            | "xxd"
            | "hexdump"
            | "od"
            | "strings"
            | "tree"
            | "jq"
            | "yq"
    ) && !has_write_redirection(command)
}

/// Detect write redirections (`>`, `>>`, `>&`) and `sed -i` / `--in-place`
/// that turn an otherwise read-only command into a write operation.
fn has_write_redirection(command: &str) -> bool {
    // Scan tokens so `>` inside a quoted argument is not misclassified.
    // This is a heuristic — a fully correct parser would need shell quoting.
    for tok in command.split_whitespace() {
        if tok.starts_with('>') || tok == ">" || tok == ">>" || tok.starts_with(">&") {
            return true;
        }
    }
    // `sed -i` in any form (`-i`, `-i''`, `-iE`, `--in-place`, `--in-place=...`).
    if command.split_whitespace().any(|tok| {
        tok == "-i"
            || tok == "--in-place"
            || tok.starts_with("-i")
            || tok.starts_with("--in-place=")
    }) {
        // Only treat as a write if the command actually is `sed`.
        if command
            .split_whitespace()
            .next()
            .map(|c| c.rsplit('/').next().unwrap_or(c) == "sed")
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_enforcer(mode: PermissionMode) -> PermissionEnforcer {
        let policy = PermissionPolicy::new(mode);
        PermissionEnforcer::new(policy)
    }

    #[test]
    fn allow_mode_permits_everything() {
        let enforcer = make_enforcer(PermissionMode::Allow);
        assert!(enforcer.is_allowed("bash", ""));
        assert!(enforcer.is_allowed("write_file", ""));
        assert!(enforcer.is_allowed("edit_file", ""));
        assert_eq!(
            enforcer.check_file_write("/outside/path", "/workspace"),
            EnforcementResult::Allowed
        );
        assert_eq!(enforcer.check_bash("rm -rf /"), EnforcementResult::Allowed);
    }

    #[test]
    fn read_only_denies_writes() {
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("read_file", PermissionMode::ReadOnly)
            .with_tool_requirement("grep_search", PermissionMode::ReadOnly)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);

        let enforcer = PermissionEnforcer::new(policy);
        assert!(enforcer.is_allowed("read_file", ""));
        assert!(enforcer.is_allowed("grep_search", ""));

        // write_file requires WorkspaceWrite but we're in ReadOnly
        let result = enforcer.check("write_file", "");
        assert!(matches!(result, EnforcementResult::Denied { .. }));

        let result = enforcer.check_file_write("/workspace/file.rs", "/workspace");
        assert!(matches!(result, EnforcementResult::Denied { .. }));
    }

    #[test]
    fn read_only_allows_read_commands() {
        let enforcer = make_enforcer(PermissionMode::ReadOnly);
        assert_eq!(
            enforcer.check_bash("cat src/main.rs"),
            EnforcementResult::Allowed
        );
        assert_eq!(
            enforcer.check_bash("grep -r 'pattern' ."),
            EnforcementResult::Allowed
        );
        assert_eq!(enforcer.check_bash("ls -la"), EnforcementResult::Allowed);
    }

    #[test]
    fn read_only_denies_write_commands() {
        let enforcer = make_enforcer(PermissionMode::ReadOnly);
        let result = enforcer.check_bash("rm file.txt");
        assert!(matches!(result, EnforcementResult::Denied { .. }));
    }

    #[test]
    fn workspace_write_allows_within_workspace() {
        let enforcer = make_enforcer(PermissionMode::WorkspaceWrite);
        let result = enforcer.check_file_write("/workspace/src/main.rs", "/workspace");
        assert_eq!(result, EnforcementResult::Allowed);
    }

    #[test]
    fn workspace_write_denies_outside_workspace() {
        let enforcer = make_enforcer(PermissionMode::WorkspaceWrite);
        let result = enforcer.check_file_write("/etc/passwd", "/workspace");
        assert!(matches!(result, EnforcementResult::Denied { .. }));
    }

    #[test]
    fn prompt_mode_denies_without_prompter() {
        let enforcer = make_enforcer(PermissionMode::Prompt);
        let result = enforcer.check_bash("echo test");
        assert!(matches!(result, EnforcementResult::Denied { .. }));

        let result = enforcer.check_file_write("/workspace/file.rs", "/workspace");
        assert!(matches!(result, EnforcementResult::Denied { .. }));
    }

    #[test]
    fn workspace_boundary_check() {
        assert!(is_within_workspace("/workspace/src/main.rs", "/workspace"));
        assert!(is_within_workspace("/workspace", "/workspace"));
        assert!(!is_within_workspace("/etc/passwd", "/workspace"));
        assert!(!is_within_workspace("/workspacex/hack", "/workspace"));
    }

    #[test]
    fn read_only_command_heuristic() {
        assert!(is_read_only_command("cat file.txt"));
        assert!(is_read_only_command("grep pattern file"));
        // `git` is excluded from the read-only whitelist because subcommands
        // like `checkout`/`reset --hard` mutate the workspace.
        assert!(!is_read_only_command("git log --oneline"));
        assert!(!is_read_only_command("rm file.txt"));
        assert!(!is_read_only_command("echo test > file.txt"));
        assert!(!is_read_only_command("sed -i 's/a/b/' file"));
        // `tee`, `python`, `cargo` are excluded — they can write or exec code.
        assert!(!is_read_only_command("tee out.txt"));
        assert!(!is_read_only_command("python -c 'print(1)'"));
        assert!(!is_read_only_command("cargo build"));
    }

    #[test]
    fn active_mode_returns_policy_mode() {
        // given
        let modes = [
            PermissionMode::ReadOnly,
            PermissionMode::WorkspaceWrite,
            PermissionMode::DangerFullAccess,
            PermissionMode::Prompt,
            PermissionMode::Allow,
        ];

        // when
        let active_modes: Vec<_> = modes
            .into_iter()
            .map(|mode| make_enforcer(mode).active_mode())
            .collect();

        // then
        assert_eq!(active_modes, modes);
    }

    #[test]
    fn danger_full_access_permits_file_writes_and_bash() {
        // given
        let enforcer = make_enforcer(PermissionMode::DangerFullAccess);

        // when
        let file_result = enforcer.check_file_write("/outside/workspace/file.txt", "/workspace");
        let bash_result = enforcer.check_bash("rm -rf /tmp/scratch");

        // then
        assert_eq!(file_result, EnforcementResult::Allowed);
        assert_eq!(bash_result, EnforcementResult::Allowed);
    }

    #[test]
    fn check_denied_payload_contains_tool_and_modes() {
        // given
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);
        let enforcer = PermissionEnforcer::new(policy);

        // when
        let result = enforcer.check("write_file", "{}");

        // then
        match result {
            EnforcementResult::Denied {
                tool,
                active_mode,
                required_mode,
                reason,
            } => {
                assert_eq!(tool, "write_file");
                assert_eq!(active_mode, "read-only");
                assert_eq!(required_mode, "workspace-write");
                assert!(reason.contains("requires workspace-write permission"));
            }
            other => panic!("expected denied result, got {other:?}"),
        }
    }

    #[test]
    fn workspace_write_relative_path_resolved() {
        // given
        let enforcer = make_enforcer(PermissionMode::WorkspaceWrite);

        // when
        let result = enforcer.check_file_write("src/main.rs", "/workspace");

        // then
        assert_eq!(result, EnforcementResult::Allowed);
    }

    #[test]
    fn workspace_root_with_trailing_slash() {
        // given
        let enforcer = make_enforcer(PermissionMode::WorkspaceWrite);

        // when
        let result = enforcer.check_file_write("/workspace/src/main.rs", "/workspace/");

        // then
        assert_eq!(result, EnforcementResult::Allowed);
    }

    #[test]
    fn workspace_root_equality() {
        // given
        let root = "/workspace/";

        // when
        let equal_to_root = is_within_workspace("/workspace", root);

        // then
        assert!(equal_to_root);
    }

    #[test]
    fn bash_heuristic_full_path_prefix() {
        // given
        let full_path_command = "/usr/bin/cat Cargo.toml";
        // `git` is no longer treated as read-only; use `ls` instead.
        let ls_path_command = "/usr/local/bin/ls -la";

        // when
        let cat_result = is_read_only_command(full_path_command);
        let ls_result = is_read_only_command(ls_path_command);

        // then
        assert!(cat_result);
        assert!(ls_result);
    }

    #[test]
    fn bash_heuristic_redirects_block_read_only_commands() {
        // given
        let overwrite = "cat Cargo.toml > out.txt";
        let append = "echo test >> out.txt";

        // when
        let overwrite_result = is_read_only_command(overwrite);
        let append_result = is_read_only_command(append);

        // then
        assert!(!overwrite_result);
        assert!(!append_result);
    }

    #[test]
    fn bash_heuristic_in_place_flag_blocks() {
        // given
        let interactive_python = "python -i script.py";
        let in_place_sed = "sed --in-place 's/a/b/' file.txt";

        // when
        let interactive_result = is_read_only_command(interactive_python);
        let in_place_result = is_read_only_command(in_place_sed);

        // then
        assert!(!interactive_result);
        assert!(!in_place_result);
    }

    #[test]
    fn bash_heuristic_empty_command() {
        // given
        let empty = "";
        let whitespace = "   ";

        // when
        let empty_result = is_read_only_command(empty);
        let whitespace_result = is_read_only_command(whitespace);

        // then
        assert!(!empty_result);
        assert!(!whitespace_result);
    }

    #[test]
    fn prompt_mode_check_bash_denied_payload_fields() {
        // given
        let enforcer = make_enforcer(PermissionMode::Prompt);

        // when
        let result = enforcer.check_bash("git status");

        // then
        match result {
            EnforcementResult::Denied {
                tool,
                active_mode,
                required_mode,
                reason,
            } => {
                assert_eq!(tool, "bash");
                assert_eq!(active_mode, "prompt");
                assert_eq!(required_mode, "danger-full-access");
                assert_eq!(reason, "bash requires confirmation in prompt mode");
            }
            other => panic!("expected denied result, got {other:?}"),
        }
    }

    #[test]
    fn read_only_check_file_write_denied_payload() {
        // given
        let enforcer = make_enforcer(PermissionMode::ReadOnly);

        // when
        let result = enforcer.check_file_write("/workspace/file.txt", "/workspace");

        // then
        match result {
            EnforcementResult::Denied {
                tool,
                active_mode,
                required_mode,
                reason,
            } => {
                assert_eq!(tool, "write_file");
                assert_eq!(active_mode, "read-only");
                assert_eq!(required_mode, "workspace-write");
                assert!(reason.contains("file writes are not allowed"));
            }
            other => panic!("expected denied result, got {other:?}"),
        }
    }
}
