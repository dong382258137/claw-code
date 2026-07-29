//! Basic tool handlers for the IM bridge agent.
//!
//! Registers simple implementations of the core tools (bash, read_file,
//! write_file, edit_file, glob_search, grep_search) on a `StaticToolExecutor`.
//!
//! These are intentionally lightweight — they don't have the full permission
//! enforcement, progress reporting, or MCP integration of the CLI's
//! `CliToolExecutor`. They exist so the agent can actually read/write files
//! and run commands when invoked via IM.

use runtime::{StaticToolExecutor, ToolError};

/// Register all default tool handlers on the given executor.
///
/// Uses `std::mem::take` because `StaticToolExecutor::register` consumes
/// `self` (builder pattern), but our caller passes `&mut`. Since
/// `StaticToolExecutor: Default`, we can temporarily take ownership and
/// write the result back.
pub fn register_default_tools(executor: &mut StaticToolExecutor) {
    let owned = std::mem::take(executor);
    let owned = owned
        .register("bash", handle_bash)
        .register("read_file", handle_read_file)
        .register("write_file", handle_write_file)
        .register("edit_file", handle_edit_file)
        .register("replace_lines", handle_replace_lines)
        .register("glob_search", handle_glob_search)
        .register("grep_search", handle_grep_search);
    *executor = owned;
}

// ── Helpers ────────────────────────────────────────────────

fn parse_json(input: &str) -> Result<serde_json::Value, ToolError> {
    serde_json::from_str(input).map_err(|e| ToolError::new(format!("invalid JSON input: {e}")))
}

fn get_str<'a>(v: &'a serde_json::Value, field: &str) -> Result<&'a str, ToolError> {
    v.get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::new(format!("missing required field '{field}'")))
}

fn get_u64(v: &serde_json::Value, field: &str) -> Result<u64, ToolError> {
    v.get(field)
        .and_then(|v| v.as_u64())
        .ok_or_else(|| ToolError::new(format!("missing or invalid field '{field}'")))
}

// ── Tool handlers ──────────────────────────────────────────

fn handle_bash(input: &str) -> Result<String, ToolError> {
    let v = parse_json(input)?;
    let command = get_str(&v, "command")?;

    #[cfg(windows)]
    let result = std::process::Command::new("cmd").args(["/C", command]).output();
    #[cfg(not(windows))]
    let result = std::process::Command::new("sh").args(["-c", command]).output();

    let output = result.map_err(|e| ToolError::new(format!("failed to execute command: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        if stderr.is_empty() {
            Ok(stdout.to_string())
        } else {
            Ok(format!("{stdout}\n[stderr]\n{stderr}"))
        }
    } else {
        Ok(format!(
            "Exit code: {}\n[stdout]\n{stdout}\n[stderr]\n{stderr}",
            output.status.code().unwrap_or(-1)
        ))
    }
}

fn handle_read_file(input: &str) -> Result<String, ToolError> {
    let v = parse_json(input)?;
    let path = get_str(&v, "path")?;

    let content = std::fs::read_to_string(path)
        .map_err(|e| ToolError::new(format!("failed to read '{path}': {e}")))?;

    // Apply offset/limit if provided
    let offset = v.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let limit = v.get("limit").and_then(|v| v.as_u64());

    if offset == 0 && limit.is_none() {
        return Ok(content);
    }

    let lines: Vec<&str> = content.lines().collect();
    let start = offset.min(lines.len());
    let end = limit
        .map(|l| (start + l as usize).min(lines.len()))
        .unwrap_or(lines.len());

    Ok(lines[start..end].join("\n"))
}

fn handle_write_file(input: &str) -> Result<String, ToolError> {
    let v = parse_json(input)?;
    let path = get_str(&v, "path")?;
    let content = get_str(&v, "content")?;

    // Create parent directories if needed
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ToolError::new(format!("failed to create directories: {e}")))?;
        }
    }

    std::fs::write(path, content)
        .map_err(|e| ToolError::new(format!("failed to write '{path}': {e}")))?;

    Ok(format!("Successfully wrote to {path}"))
}

fn handle_edit_file(input: &str) -> Result<String, ToolError> {
    let v = parse_json(input)?;
    let path = get_str(&v, "path")?;
    let old_string = get_str(&v, "old_string")?;
    let new_string = get_str(&v, "new_string")?;
    let replace_all = v
        .get("replace_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let content = std::fs::read_to_string(path)
        .map_err(|e| ToolError::new(format!("failed to read '{path}': {e}")))?;

    let new_content = if replace_all {
        content.replace(old_string, new_string)
    } else {
        // Replace first occurrence only
        match content.find(old_string) {
            Some(idx) => {
                let mut result = String::with_capacity(content.len());
                result.push_str(&content[..idx]);
                result.push_str(new_string);
                result.push_str(&content[idx + old_string.len()..]);
                result
            }
            None => {
                return Err(ToolError::new(format!(
                    "old_string not found in '{path}'"
                )))
            }
        }
    };

    std::fs::write(path, &new_content)
        .map_err(|e| ToolError::new(format!("failed to write '{path}': {e}")))?;

    Ok(format!("Successfully edited {path}"))
}

fn handle_replace_lines(input: &str) -> Result<String, ToolError> {
    let v = parse_json(input)?;
    let path = get_str(&v, "path")?;
    let start_line = get_u64(&v, "start_line")? as usize;
    let end_line = get_u64(&v, "end_line")? as usize;
    let new_content = get_str(&v, "new_content")?;

    let content = std::fs::read_to_string(path)
        .map_err(|e| ToolError::new(format!("failed to read '{path}': {e}")))?;

    let mut lines: Vec<String> = content.lines().map(String::from).collect();

    if start_line == 0 || end_line == 0 || start_line > lines.len() {
        return Err(ToolError::new("invalid line range"));
    }

    let start_idx = start_line - 1;
    let end_idx = end_line.min(lines.len());

    // Replace the range with new content lines
    let new_lines: Vec<String> = new_content.lines().map(String::from).collect();
    lines.splice(start_idx..end_idx, new_lines);

    let result = lines.join("\n");
    std::fs::write(path, &result)
        .map_err(|e| ToolError::new(format!("failed to write '{path}': {e}")))?;

    Ok(format!("Successfully replaced lines {start_line}-{end_line} in {path}"))
}

fn handle_glob_search(input: &str) -> Result<String, ToolError> {
    let v = parse_json(input)?;
    let pattern = get_str(&v, "pattern")?;
    let path = v.get("path").and_then(|v| v.as_str()).unwrap_or(".");

    // Simple glob implementation using std
    let base = std::path::Path::new(path);
    let mut results = Vec::new();

    fn walk_dir(
        dir: &std::path::Path,
        pattern: &str,
        results: &mut Vec<String>,
    ) -> Result<(), ToolError> {
        let entries = std::fs::read_dir(dir)
            .map_err(|e| ToolError::new(format!("failed to read dir '{}': {e}", dir.display())))?;

        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                // Skip hidden directories and common ignore patterns
                if let Some(name) = entry_path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with('.') || name == "node_modules" || name == "target" {
                        continue;
                    }
                }
                walk_dir(&entry_path, pattern, results)?;
            } else if let Some(name) = entry_path.file_name().and_then(|n| n.to_str()) {
                if glob_match(pattern, name) {
                    results.push(entry_path.display().to_string());
                }
            }
        }
        Ok(())
    }

    walk_dir(base, pattern, &mut results)?;

    if results.is_empty() {
        Ok("No files found.".to_string())
    } else {
        Ok(results.join("\n"))
    }
}

/// Simple glob matcher supporting *, ?, and literal characters.
fn glob_match(pattern: &str, name: &str) -> bool {
    // Very basic glob: support * and ? wildcards
    let pattern_bytes = pattern.as_bytes();
    let name_bytes = name.as_bytes();
    glob_match_impl(pattern_bytes, name_bytes)
}

fn glob_match_impl(pattern: &[u8], name: &[u8]) -> bool {
    if pattern.is_empty() {
        return name.is_empty();
    }
    match pattern[0] {
        b'*' => {
            // Try matching zero or more characters
            for i in 0..=name.len() {
                if glob_match_impl(&pattern[1..], &name[i..]) {
                    return true;
                }
            }
            false
        }
        b'?' => {
            if name.is_empty() {
                false
            } else {
                glob_match_impl(&pattern[1..], &name[1..])
            }
        }
        c => {
            if name.is_empty() || name[0] != c {
                false
            } else {
                glob_match_impl(&pattern[1..], &name[1..])
            }
        }
    }
}

fn handle_grep_search(input: &str) -> Result<String, ToolError> {
    let v = parse_json(input)?;
    let pattern = get_str(&v, "pattern")?;
    let path = v.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let glob_filter = v.get("glob").and_then(|v| v.as_str());
    let case_insensitive = v.get("-i").and_then(|v| v.as_bool()).unwrap_or(false);
    let show_line_numbers = v.get("-n").and_then(|v| v.as_bool()).unwrap_or(false);
    let head_limit = v.get("head_limit").and_then(|v| v.as_u64());

    let mut results = Vec::new();
    let mut count = 0u64;

    #[allow(clippy::too_many_arguments)]
    fn walk_and_grep(
        dir: &std::path::Path,
        pattern: &str,
        glob_filter: Option<&str>,
        case_insensitive: bool,
        show_line_numbers: bool,
        results: &mut Vec<String>,
        count: &mut u64,
        head_limit: Option<u64>,
    ) -> Result<(), ToolError> {
        if let Some(limit) = head_limit {
            if *count >= limit {
                return Ok(());
            }
        }

        let entries = std::fs::read_dir(dir)
            .map_err(|e| ToolError::new(format!("failed to read dir: {e}")))?;

        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                if let Some(name) = entry_path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with('.') || name == "node_modules" || name == "target" {
                        continue;
                    }
                }
                walk_and_grep(
                    &entry_path,
                    pattern,
                    glob_filter,
                    case_insensitive,
                    show_line_numbers,
                    results,
                    count,
                    head_limit,
                )?;
            } else if let Some(name) = entry_path.file_name().and_then(|n| n.to_str()) {
                // Apply glob filter if specified
                if let Some(gf) = glob_filter {
                    if !glob_match(gf, name) {
                        continue;
                    }
                }

                // Read file and search
                if let Ok(content) = std::fs::read_to_string(&entry_path) {
                    let search_pattern = if case_insensitive {
                        pattern.to_lowercase()
                    } else {
                        pattern.to_string()
                    };

                    for (line_num, line) in content.lines().enumerate() {
                        let line_to_check = if case_insensitive {
                            line.to_lowercase()
                        } else {
                            line.to_string()
                        };

                        if line_to_check.contains(&search_pattern) {
                            if show_line_numbers {
                                results.push(format!(
                                    "{}:{}: {}",
                                    entry_path.display(),
                                    line_num + 1,
                                    line
                                ));
                            } else {
                                results.push(format!("{}: {}", entry_path.display(), line));
                            }
                            *count += 1;
                            if let Some(limit) = head_limit {
                                if *count >= limit {
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    walk_and_grep(
        std::path::Path::new(path),
        pattern,
        glob_filter,
        case_insensitive,
        show_line_numbers,
        &mut results,
        &mut count,
        head_limit,
    )?;

    if results.is_empty() {
        Ok("No matches found.".to_string())
    } else {
        Ok(results.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_match() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(glob_match("*.rs", "lib.rs"));
        assert!(!glob_match("*.rs", "main.ts"));
        assert!(glob_match("*.ts", "app.ts"));
        assert!(glob_match("?.rs", "a.rs"));
        assert!(!glob_match("?.rs", "ab.rs"));
        assert!(glob_match("*", "anything"));
    }
}
