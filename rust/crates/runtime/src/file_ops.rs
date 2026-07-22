use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use glob::Pattern;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use walkdir::{DirEntry, WalkDir};

/// Maximum file size that can be read (10 MB).
const MAX_READ_SIZE: u64 = 10 * 1024 * 1024;

/// Maximum file size that can be written (10 MB).
const MAX_WRITE_SIZE: usize = 10 * 1024 * 1024;

const GLOB_SEARCH_IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    ".build",
    "target",
    "dist",
    "coverage",
];

/// Check whether a file appears to contain binary content by examining
/// the first chunk for NUL bytes.
fn is_binary_file(path: &Path) -> io::Result<bool> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut buffer = [0u8; 8192];
    let bytes_read = file.read(&mut buffer)?;
    Ok(buffer[..bytes_read].contains(&0))
}

/// `read_file_for_search` 的跳过原因(诊断用,非致命错误)。
/// 调用方统一计数为 skipped_files,不区分具体原因(LLM 只需知道
/// "有 N 个文件被跳过,结果可能不完整")。
enum ReadForSearchError {
    /// 文件过大(超过 MAX_READ_SIZE),跳过避免 OOM。
    TooLarge,
    /// 文件被识别为二进制(含 NUL 字节),无文本匹配价值。
    Binary,
    /// 读取失败(权限/IO 错误等)。
    Io,
}

/// 读取文件内容用于 grep 搜索。
///
/// 与 `fs::read_to_string` 的关键差异:
/// - 使用字节读取 + `String::from_utf8_lossy`,使含 BOM 或部分非 UTF-8
///   字节的源码文件也能被搜索(痛点 1 根因修复)。
/// - 跳过二进制文件(含 NUL 字节)而非整文件静默丢失。
/// - 跳过超大文件(超过 MAX_READ_SIZE)避免 OOM。
/// - 任何跳过都返回 `ReadForSearchError` 供调用方计数,而非静默 `continue`。
fn read_file_for_search(path: &Path) -> Result<String, ReadForSearchError> {
    // 先检查大小,避免把 10MB+ 的二进制全读进内存
    let metadata = fs::metadata(path).map_err(|_| ReadForSearchError::Io)?;
    if metadata.len() > MAX_READ_SIZE {
        return Err(ReadForSearchError::TooLarge);
    }
    // 二进制文件(含 NUL)无文本搜索价值,跳过
    if matches!(is_binary_file(path), Ok(true)) {
        return Err(ReadForSearchError::Binary);
    }
    // 字节读取 + lossy 转换:UTF-8 错误不再致命
    let bytes = fs::read(path).map_err(|_| ReadForSearchError::Io)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Validate that a resolved path stays within the given workspace root.
/// Returns the canonical path on success, or an error if the path escapes
/// the workspace boundary (e.g. via `../` traversal or symlink).
#[allow(dead_code)]
fn validate_workspace_boundary(resolved: &Path, workspace_root: &Path) -> io::Result<()> {
    if !resolved.starts_with(workspace_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "path {} escapes workspace boundary {}",
                resolved.display(),
                workspace_root.display()
            ),
        ));
    }
    Ok(())
}

/// 在 Windows 上剥离 `\\?\`（Verbatim Disk）和 `\\?\UNC\`（Verbatim UNC）前缀。
///
/// 背景：`Path::canonicalize()` 在 Windows 上返回带 `\\?\` verbatim 前缀的路径
/// （例如 `\\?\D:\foo\bar`），而 `std::env::current_dir()` 返回不带前缀的路径
/// （例如 `D:\foo\bar`）。两类路径在 `starts_with` 比较时因前缀 component 不同
/// 被判为不相干，导致合法路径被误判为越界。
///
/// 此函数把 verbatim 前缀转回普通磁盘/UNC 前缀，使比较两端格式统一。
/// 在非 Windows 平台上为 no-op。
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut components = path.components();
    let first = match components.next() {
        Some(Component::Prefix(prefix)) => prefix,
        _ => return path.to_path_buf(),
    };

    let rest: PathBuf = components.collect();
    match first.kind() {
        std::path::Prefix::VerbatimDisk(disk_byte) => {
            // `\\?\D:\foo` -> `D:\foo`（disk_byte 是盘符 ASCII，例如 'D' = 68）
            let disk_char = char::from_u32(disk_byte as u32).unwrap_or('?');
            let mut result = PathBuf::from(format!("{}:", disk_char));
            if !rest.as_os_str().is_empty() {
                result.push(rest);
            }
            result
        }
        std::path::Prefix::VerbatimUNC(server, share) => {
            // `\\?\UNC\server\share\foo` -> `\\server\share\foo`
            let mut result = PathBuf::from(format!(
                "\\\\{}\\{}",
                server.to_string_lossy(),
                share.to_string_lossy()
            ));
            if !rest.as_os_str().is_empty() {
                result.push(rest);
            }
            result
        }
        _ => path.to_path_buf(),
    }
}

/// 多根版本的工作区边界校验。任一根包含即放行，全部不包含才拒绝。
/// 错误消息列出所有根，方便用户诊断。
///
/// 注意：错误消息保留 "escapes workspace" 子串，与单根版 `validate_workspace_boundary`
/// 保持向后兼容（既有测试断言 `contains("escapes workspace")`）。
///
/// Windows 兼容：比较前对 `resolved` 和每个 root 都剥离 `\\?\` verbatim 前缀，
/// 避免 `canonicalize()` 返回的 verbatim 路径与 `current_dir()` 返回的普通路径
/// 因前缀 component 不同而 `starts_with` 失败。
fn validate_workspace_boundary_multi(
    resolved: &Path,
    workspace_roots: &[PathBuf],
) -> io::Result<()> {
    if workspace_roots.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "path {} rejected: no workspace roots configured",
                resolved.display()
            ),
        ));
    }
    let normalized_resolved = strip_verbatim_prefix(resolved);
    let normalized_roots: Vec<PathBuf> = workspace_roots
        .iter()
        .map(|r| strip_verbatim_prefix(r))
        .collect();
    if normalized_roots
        .iter()
        .any(|root| normalized_resolved.starts_with(root))
    {
        return Ok(());
    }
    let roots_display: Vec<String> = normalized_roots
        .iter()
        .map(|r| r.display().to_string())
        .collect();
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "path {} escapes workspace boundaries [{}]",
            normalized_resolved.display(),
            roots_display.join(", ")
        ),
    ))
}

/// 将主工作区根与额外根合并为规范化（canonicalize）后的根列表。
/// 主根始终在首位。空 `extra_roots` 时返回单元素列表。
fn canonicalize_roots(workspace_root: &Path, extra_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = vec![canonicalize_workspace_root(workspace_root)];
    for root in extra_roots {
        roots.push(canonicalize_workspace_root(root));
    }
    roots
}

/// 多根工作区路径校验器。移植自 Python `path_scope.py::WorkspacePathScope`。
///
/// 支持多个工作区根目录，任一根包含即放行。用于 `--add-dir` CLI flag
/// 允许用户在主工作区之外添加额外的允许目录。
#[derive(Debug, Clone)]
pub struct WorkspacePathScope {
    /// 已解析（canonicalize）的工作区根目录列表。至少包含一个根。
    roots: Vec<PathBuf>,
}

impl WorkspacePathScope {
    /// 从单个根创建。根会被 canonicalize。
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let canonical = root.canonicalize().unwrap_or(root);
        Self {
            roots: vec![canonical],
        }
    }

    /// 从多个根创建。每个根都会被 canonicalize，重复的会被去重。
    pub fn from_roots(roots: Vec<PathBuf>) -> Self {
        let mut canonical_roots: Vec<PathBuf> = roots
            .into_iter()
            .map(|r| r.canonicalize().unwrap_or(r))
            .collect();
        canonical_roots.sort();
        canonical_roots.dedup();
        if canonical_roots.is_empty() {
            // 退化为当前目录，避免空根导致所有路径被拒
            canonical_roots.push(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        }
        Self {
            roots: canonical_roots,
        }
    }

    /// 返回工作区根列表的引用。
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// 校验已解析的路径是否在任一工作区根内。
    pub fn validate_resolved(&self, resolved: &Path) -> io::Result<()> {
        validate_workspace_boundary_multi(resolved, &self.roots)
    }

    /// 校验未解析的路径：先 normalize/canonicalize，再调用 `validate_resolved`。
    /// `allow_missing` 为 true 时，对不存在的路径回退到父目录 canonicalize。
    pub fn validate_path(&self, path: &str, allow_missing: bool) -> io::Result<PathBuf> {
        let trimmed = path.trim_matches(|ch: char| {
            matches!(
                ch,
                '"' | '\'' | '`' | ',' | ';' | ')' | '(' | '[' | ']' | '{' | '}'
            )
        });
        let candidate = PathBuf::from(trimmed);
        let absolute = if candidate.is_absolute() {
            candidate
        } else {
            std::env::current_dir().unwrap_or_default().join(candidate)
        };
        let resolved = if allow_missing {
            absolute
                .parent()
                .and_then(|parent| parent.canonicalize().ok())
                .map(|parent| parent.join(absolute.file_name().unwrap_or_default()))
                .unwrap_or(absolute)
        } else {
            absolute.canonicalize().unwrap_or(absolute)
        };
        self.validate_resolved(&resolved)?;
        Ok(resolved)
    }
}

/// Text payload returned by file-reading operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextFilePayload {
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub content: String,
    #[serde(rename = "numLines")]
    pub num_lines: usize,
    #[serde(rename = "startLine")]
    pub start_line: usize,
    #[serde(rename = "totalLines")]
    pub total_lines: usize,
}

/// Output envelope for the `read_file` tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadFileOutput {
    #[serde(rename = "type")]
    pub kind: String,
    pub file: TextFilePayload,
}

/// Structured patch hunk emitted by write and edit operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuredPatchHunk {
    #[serde(rename = "oldStart")]
    pub old_start: usize,
    #[serde(rename = "oldLines")]
    pub old_lines: usize,
    #[serde(rename = "newStart")]
    pub new_start: usize,
    #[serde(rename = "newLines")]
    pub new_lines: usize,
    pub lines: Vec<String>,
}

/// Output envelope for full-file write operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WriteFileOutput {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub content: String,
    #[serde(rename = "structuredPatch")]
    pub structured_patch: Vec<StructuredPatchHunk>,
    #[serde(rename = "originalFile")]
    pub original_file: Option<String>,
    #[serde(rename = "gitDiff")]
    pub git_diff: Option<serde_json::Value>,
}

/// Output envelope for targeted string-replacement edits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditFileOutput {
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(rename = "oldString")]
    pub old_string: String,
    #[serde(rename = "newString")]
    pub new_string: String,
    #[serde(rename = "originalFile")]
    pub original_file: String,
    #[serde(rename = "structuredPatch")]
    pub structured_patch: Vec<StructuredPatchHunk>,
    #[serde(rename = "userModified")]
    pub user_modified: bool,
    #[serde(rename = "replaceAll")]
    pub replace_all: bool,
    #[serde(rename = "gitDiff")]
    pub git_diff: Option<serde_json::Value>,
}

/// Result of a glob-based filename search.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GlobSearchOutput {
    #[serde(rename = "durationMs")]
    pub duration_ms: u128,
    #[serde(rename = "numFiles")]
    pub num_files: usize,
    pub filenames: Vec<String>,
    pub truncated: bool,
}

/// Parameters accepted by the grep-style search tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepSearchInput {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    #[serde(rename = "output_mode")]
    pub output_mode: Option<String>,
    #[serde(rename = "-B")]
    pub before: Option<usize>,
    #[serde(rename = "-A")]
    pub after: Option<usize>,
    #[serde(rename = "-C")]
    pub context_short: Option<usize>,
    pub context: Option<usize>,
    #[serde(rename = "-n")]
    pub line_numbers: Option<bool>,
    #[serde(rename = "-i")]
    pub case_insensitive: Option<bool>,
    #[serde(rename = "type")]
    pub file_type: Option<String>,
    pub head_limit: Option<usize>,
    pub offset: Option<usize>,
    pub multiline: Option<bool>,
}

/// Result payload returned by the grep-style search tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepSearchOutput {
    pub mode: Option<String>,
    #[serde(rename = "numFiles")]
    pub num_files: usize,
    pub filenames: Vec<String>,
    pub content: Option<String>,
    #[serde(rename = "numLines")]
    pub num_lines: Option<usize>,
    #[serde(rename = "numMatches")]
    pub num_matches: Option<usize>,
    #[serde(rename = "appliedLimit")]
    pub applied_limit: Option<usize>,
    #[serde(rename = "appliedOffset")]
    pub applied_offset: Option<usize>,
    /// 截断前的真实匹配文件总数(用于诊断"是否被 head_limit 截断")。
    /// 与 `num_files` 不同:`num_files` 是返回的文件数,本字段是
    /// 应用 limit/offset 前的全部匹配数。None 表示未跟踪(向后兼容)。
    #[serde(rename = "totalFilesBeforeLimit", default, skip_serializing_if = "Option::is_none")]
    pub total_files_before_limit: Option<usize>,
    /// 读取失败/被跳过的文件数(用于诊断"静默跳过"问题)。
    /// 当文件过大、读取错误、或被识别为二进制时计入。
    #[serde(rename = "skippedFiles", default, skip_serializing_if = "Option::is_none")]
    pub skipped_files: Option<usize>,
}

/// Reads a text file and returns a line-windowed payload.
pub fn read_file(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> io::Result<ReadFileOutput> {
    let absolute_path = normalize_path(path)?;

    // Check file size before reading
    let metadata = fs::metadata(&absolute_path)?;
    if metadata.len() > MAX_READ_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "file is too large ({} bytes, max {} bytes)",
                metadata.len(),
                MAX_READ_SIZE
            ),
        ));
    }

    // Detect binary files
    if is_binary_file(&absolute_path)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file appears to be binary",
        ));
    }

    let content = fs::read_to_string(&absolute_path)?;
    let lines: Vec<&str> = content.lines().collect();
    let start_index = offset.unwrap_or(0).min(lines.len());
    let end_index = limit.map_or(lines.len(), |limit| {
        start_index.saturating_add(limit).min(lines.len())
    });
    let selected = lines[start_index..end_index].join("\n");

    Ok(ReadFileOutput {
        kind: String::from("text"),
        file: TextFilePayload {
            file_path: absolute_path.to_string_lossy().into_owned(),
            content: selected,
            num_lines: end_index.saturating_sub(start_index),
            start_line: start_index.saturating_add(1),
            total_lines: lines.len(),
        },
    })
}

/// Replaces a file's contents and returns patch metadata.
pub fn write_file(path: &str, content: &str) -> io::Result<WriteFileOutput> {
    if content.len() > MAX_WRITE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "content is too large ({} bytes, max {} bytes)",
                content.len(),
                MAX_WRITE_SIZE
            ),
        ));
    }

    let absolute_path = normalize_path_allow_missing(path)?;
    let original_file = fs::read_to_string(&absolute_path).ok();
    if let Some(parent) = absolute_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&absolute_path, content)?;

    Ok(WriteFileOutput {
        kind: if original_file.is_some() {
            String::from("update")
        } else {
            String::from("create")
        },
        file_path: absolute_path.to_string_lossy().into_owned(),
        content: content.to_owned(),
        structured_patch: make_patch(original_file.as_deref().unwrap_or(""), content),
        original_file,
        git_diff: None,
    })
}

/// Performs an in-file string replacement and returns patch metadata.
pub fn edit_file(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> io::Result<EditFileOutput> {
    let absolute_path = normalize_path(path)?;
    let original_file = fs::read_to_string(&absolute_path)?;
    if old_string == new_string {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "old_string and new_string must differ",
        ));
    }
    if !original_file.contains(old_string) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "old_string not found in file",
        ));
    }

    let updated = if replace_all {
        original_file.replace(old_string, new_string)
    } else {
        original_file.replacen(old_string, new_string, 1)
    };
    fs::write(&absolute_path, &updated)?;

    Ok(EditFileOutput {
        file_path: absolute_path.to_string_lossy().into_owned(),
        old_string: old_string.to_owned(),
        new_string: new_string.to_owned(),
        original_file: original_file.clone(),
        structured_patch: make_patch(&original_file, &updated),
        user_modified: false,
        replace_all,
        git_diff: None,
    })
}

/// Expands a glob pattern and returns matching filenames.
pub fn glob_search(pattern: &str, path: Option<&str>) -> io::Result<GlobSearchOutput> {
    glob_search_impl(pattern, path, None, &[])
}

/// Refuse to write through a leaf symlink.
///
/// `symlink_metadata` does not follow symlinks, so a symlink at `path`
/// will report `file_type().is_symlink() == true` and we reject it. This
/// closes the TOCTOU window between the workspace-boundary check and the
/// actual write (BUG-P1-5): an attacker who swaps the validated leaf for
/// a symlink pointing outside the workspace cannot trick us into writing
/// through it.
fn reject_leaf_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing to write through symlink at {} (TOCTOU defense)",
                        path.display()
                    ),
                ));
            }
            Ok(())
        }
        // File does not exist yet — nothing to swap, safe to proceed.
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Write `content` to `absolute_path`, refusing to follow a leaf symlink.
///
/// Mirrors [`write_file`]'s logic (size cap, dir creation, patch metadata)
/// but operates on an already-validated canonical path and rejects symlink
/// substitution at the leaf.
fn write_file_at_checked(absolute_path: &Path, content: &str) -> io::Result<WriteFileOutput> {
    if content.len() > MAX_WRITE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "content is too large ({} bytes, max {} bytes)",
                content.len(),
                MAX_WRITE_SIZE
            ),
        ));
    }

    reject_leaf_symlink(absolute_path)?;

    let original_file = fs::read_to_string(absolute_path).ok();
    if let Some(parent) = absolute_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(absolute_path, content)?;

    Ok(WriteFileOutput {
        kind: if original_file.is_some() {
            String::from("update")
        } else {
            String::from("create")
        },
        file_path: absolute_path.to_string_lossy().into_owned(),
        content: content.to_owned(),
        structured_patch: make_patch(original_file.as_deref().unwrap_or(""), content),
        original_file,
        git_diff: None,
    })
}

/// Edit the file at `absolute_path`, refusing to follow a leaf symlink.
///
/// Mirrors [`edit_file`]'s logic but operates on an already-validated
/// canonical path and rejects symlink substitution at the leaf.
fn edit_file_at_checked(
    absolute_path: &Path,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> io::Result<EditFileOutput> {
    reject_leaf_symlink(absolute_path)?;

    let original_file = fs::read_to_string(absolute_path)?;
    if old_string == new_string {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "old_string and new_string must differ",
        ));
    }
    if !original_file.contains(old_string) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "old_string not found in file",
        ));
    }

    let updated = if replace_all {
        original_file.replace(old_string, new_string)
    } else {
        original_file.replacen(old_string, new_string, 1)
    };
    fs::write(absolute_path, &updated)?;

    Ok(EditFileOutput {
        file_path: absolute_path.to_string_lossy().into_owned(),
        old_string: old_string.to_owned(),
        new_string: new_string.to_owned(),
        original_file: original_file.clone(),
        structured_patch: make_patch(&original_file, &updated),
        user_modified: false,
        replace_all,
        git_diff: None,
    })
}

/// Replace lines in a file by line range.
///
/// `start_line` and `end_line` are 1-based inclusive line numbers.
/// The specified range is replaced with `new_content` (including trailing
/// newline if needed).
#[derive(Debug, Serialize)]
pub struct ReplaceLinesOutput {
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(rename = "replacedStartLine")]
    pub replaced_start_line: usize,
    #[serde(rename = "replacedEndLine")]
    pub replaced_end_line: usize,
    #[serde(rename = "newContent")]
    pub new_content: String,
    #[serde(rename = "originalLines")]
    pub original_lines: String,
    #[serde(rename = "gitDiff")]
    pub git_diff: Option<String>,
}

/// Performs a line-range replacement and returns metadata.
///
/// Uses `edit_file_at_checked` pattern: rejects leaf symlinks (TOCTOU defense),
/// enforces `MAX_WRITE_SIZE`, and preserves trailing newline.
pub fn replace_lines(
    path: &str,
    start_line: usize,
    end_line: usize,
    new_content: &str,
) -> io::Result<ReplaceLinesOutput> {
    let absolute_path = normalize_path(path)?;
    reject_leaf_symlink(&absolute_path)?;

    let original_file = fs::read_to_string(&absolute_path)?;
    let original_lines: Vec<&str> = original_file.lines().collect();

    if start_line < 1 || start_line > original_lines.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "start_line {start_line} out of range (file has {} lines)",
                original_lines.len()
            ),
        ));
    }
    if end_line < start_line || end_line > original_lines.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "end_line {end_line} out of range (valid range: {start_line}..{})",
                original_lines.len()
            ),
        ));
    }

    let replaced_slice = original_lines[(start_line - 1)..end_line].join("\n");

    // Build the updated content by splicing lines
    let mut out: Vec<&str> = Vec::with_capacity(
        original_lines.len() - (end_line - start_line + 1) + 1,
    );
    out.extend_from_slice(&original_lines[..start_line - 1]);
    if !new_content.is_empty() {
        for line in new_content.lines() {
            out.push(line);
        }
    }
    out.extend_from_slice(&original_lines[end_line..]);

    let mut updated = out.join("\n");
    // Preserve trailing newline: if original file ended with one, ensure updated does too
    if original_file.ends_with('\n') && !updated.ends_with('\n') {
        updated.push('\n');
    }

    // Enforce size limit (same as write_file)
    if updated.len() > MAX_WRITE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "resulting content is too large ({} bytes, max {} bytes)",
                updated.len(),
                MAX_WRITE_SIZE
            ),
        ));
    }

    fs::write(&absolute_path, &updated)?;

    Ok(ReplaceLinesOutput {
        file_path: absolute_path.to_string_lossy().into_owned(),
        replaced_start_line: start_line,
        replaced_end_line: end_line,
        new_content: new_content.to_owned(),
        original_lines: replaced_slice,
        git_diff: None,
    })
}

/// Workspace-boundary-guarded variant of [`replace_lines`] with extra roots.
pub fn replace_lines_in_workspace_with_roots(
    path: &str,
    start_line: usize,
    end_line: usize,
    new_content: &str,
    workspace_root: &Path,
    extra_roots: &[PathBuf],
) -> io::Result<ReplaceLinesOutput> {
    let absolute_path = normalize_path(path)?;
    let roots = canonicalize_roots(workspace_root, extra_roots);
    validate_workspace_boundary_multi(&absolute_path, &roots)?;
    // BUG-P1-5 (TOCTOU): operate on the already-validated `absolute_path`
    // and refuse to follow symlinks at the leaf, same as edit_file_at_checked.
    // We pass the original `path` to replace_lines which re-normalizes it,
    // but the boundary check above already validated the canonical path.
    replace_lines(path, start_line, end_line, new_content)
}

/// If the modified file is a `.rs` file in a Rust project (has a parent
/// Cargo.toml), run `cargo check` in that project's root and return the
/// full output. Returns `None` if not a `.rs` file, not a Rust project, or
/// cargo is unavailable.
///
/// Uses `--message-format=short` to reduce output volume, and enforces a
/// 60-second timeout to prevent blocking the TUI on large projects.
pub fn run_cargo_check_for_file(file_path: &Path) -> Option<String> {
    // Only trigger for Rust source files
    if file_path.extension() != Some(std::ffi::OsStr::new("rs")) {
        return None;
    }

    // Walk up to find Cargo.toml
    let mut dir = file_path.parent()?;
    let cargo_toml = loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.exists() {
            break Some(candidate);
        }
        dir = dir.parent()?;
    }?;

    let project_root = cargo_toml.parent()?;

    let mut child = std::process::Command::new("cargo")
        .arg("check")
        .arg("--message-format=short")
        .current_dir(project_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;

    let timeout = std::time::Duration::from_secs(60);
    let start = std::time::Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                let stdout = child
                    .stdout
                    .take()
                    .map(|mut s| {
                        use std::io::Read;
                        let mut buf = String::new();
                        let _ = s.read_to_string(&mut buf);
                        buf
                    })
                    .unwrap_or_default();
                let stderr = child
                    .stderr
                    .take()
                    .map(|mut s| {
                        use std::io::Read;
                        let mut buf = String::new();
                        let _ = s.read_to_string(&mut buf);
                        buf
                    })
                    .unwrap_or_default();

                let combined = format!("{stdout}{stderr}");
                if combined.trim().is_empty() {
                    return Some("cargo check: OK (no output)".to_string());
                }
                // Truncate to prevent massive responses
                let max_len = 5000;
                if combined.len() > max_len {
                    return Some(format!(
                        "cargo check:\n{}\n...(truncated, {} total bytes)",
                        &combined[..max_len],
                        combined.len()
                    ));
                }
                return Some(format!("cargo check:\n{combined}"));
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    return Some("cargo check timed out (60s)".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => return Some(format!("cargo check error: {e}")),
        }
    }
}

fn glob_search_impl(
    pattern: &str,
    path: Option<&str>,
    workspace_root: Option<&Path>,
    extra_roots: &[PathBuf],
) -> io::Result<GlobSearchOutput> {
    let started = Instant::now();
    let base_dir = path
        .map(normalize_path)
        .transpose()?
        .unwrap_or(std::env::current_dir()?);
    let canonical_roots: Vec<PathBuf> = workspace_root
        .map(|root| canonicalize_roots(root, extra_roots))
        .unwrap_or_default();
    if !canonical_roots.is_empty() {
        validate_workspace_boundary_multi(&base_dir, &canonical_roots)?;
    }
    let search_pattern = if Path::new(pattern).is_absolute() {
        pattern.to_owned()
    } else {
        base_dir.join(pattern).to_string_lossy().into_owned()
    };
    // Windows 兼容(BUG-W1 修复):
    // 1. canonicalize() 返回 \\?\ 前缀的扩展路径语法,替换 \ 后会变成 //?/ 导致路径失效,
    //    需要先去掉 \\?\ 前缀。
    // 2. glob::Pattern 把 `\` 当作转义字符而非路径分隔符,需要规范化为 `/` 才能正确匹配。
    //    Rust 的 `Path` 在 Windows 上同时支持两种分隔符,后续 derive_glob_walk_root 和
    //    WalkDir 都能正确处理 `/` 分隔符。
    let search_pattern = search_pattern
        .strip_prefix(r"\\?\")
        .unwrap_or(&search_pattern)
        .replace('\\', "/");

    // The `glob` crate does not support brace expansion ({a,b,c}).
    // Expand braces into multiple patterns so patterns like
    // `Assets/**/*.{cs,uxml,uss}` work correctly.
    let expanded = expand_braces(&search_pattern);

    let mut seen = HashSet::new();
    let mut matches = Vec::new();
    for pat in &expanded {
        let compiled = Pattern::new(pat)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        let walk_root = derive_glob_walk_root(pat);
        if !canonical_roots.is_empty() {
            let canonical_walk_root = walk_root
                .canonicalize()
                .unwrap_or_else(|_| walk_root.clone());
            validate_workspace_boundary_multi(&canonical_walk_root, &canonical_roots)?;
        }
        let entries = WalkDir::new(&walk_root)
            .into_iter()
            .filter_entry(|entry| !should_skip_glob_dir(entry));
        for entry in entries.flatten() {
            let candidate = entry.path();
            if entry.file_type().is_file()
                && compiled.matches_path(candidate)
                && seen.insert(candidate.to_path_buf())
            {
                if !canonical_roots.is_empty() {
                    let canonical_candidate = candidate.canonicalize()?;
                    validate_workspace_boundary_multi(&canonical_candidate, &canonical_roots)?;
                }
                matches.push(candidate.to_path_buf());
            }
        }
    }

    matches.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .map(Reverse)
    });

    let truncated = matches.len() > 100;
    let filenames = matches
        .into_iter()
        .take(100)
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    Ok(GlobSearchOutput {
        duration_ms: started.elapsed().as_millis(),
        num_files: filenames.len(),
        filenames,
        truncated,
    })
}

/// Runs a regex search over workspace files with optional context lines.
pub fn grep_search(input: &GrepSearchInput) -> io::Result<GrepSearchOutput> {
    grep_search_impl(input, None, &[])
}

fn grep_search_impl(
    input: &GrepSearchInput,
    workspace_root: Option<&Path>,
    extra_roots: &[PathBuf],
) -> io::Result<GrepSearchOutput> {
    let base_path = input
        .path
        .as_deref()
        .map(normalize_path)
        .transpose()?
        .unwrap_or(std::env::current_dir()?);
    let canonical_roots: Vec<PathBuf> = workspace_root
        .map(|root| canonicalize_roots(root, extra_roots))
        .unwrap_or_default();
    if !canonical_roots.is_empty() {
        validate_workspace_boundary_multi(&base_path, &canonical_roots)?;
    }

    let regex = RegexBuilder::new(&input.pattern)
        .case_insensitive(input.case_insensitive.unwrap_or(false))
        .dot_matches_new_line(input.multiline.unwrap_or(false))
        .build()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;

    let glob_filter = input
        .glob
        .as_deref()
        .map(Pattern::new)
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let file_type = input.file_type.as_deref();
    let output_mode = input
        .output_mode
        .clone()
        .unwrap_or_else(|| String::from("files_with_matches"));
    let context = input.context.or(input.context_short).unwrap_or(0);

    let mut filenames = Vec::new();
    let mut content_lines = Vec::new();
    let mut total_matches = 0usize;
    let mut skipped_files = 0usize;

    for file_path in collect_search_files(&base_path)? {
        if !canonical_roots.is_empty() {
            let canonical_file = file_path.canonicalize()?;
            validate_workspace_boundary_multi(&canonical_file, &canonical_roots)?;
        }
        if !matches_optional_filters(&file_path, glob_filter.as_ref(), file_type) {
            continue;
        }

        // 读取失败根因修复(痛点 1):
        // 原实现用 read_to_string,遇到 BOM/非 UTF-8/二进制字节会整文件
        // 静默跳过,导致"明明有匹配却返回 0"的错误结论。
        // 改为字节读取 + from_utf8_lossy,使含 BOM/部分非 UTF-8 的源码
        // 文件也能被正常搜索;真正的二进制文件(含 NUL 字节)跳过但计数。
        let file_contents = match read_file_for_search(&file_path) {
            Ok(c) => c,
            Err(_) => {
                skipped_files += 1;
                continue;
            }
        };

        if output_mode == "count" {
            let count = regex.find_iter(&file_contents).count();
            if count > 0 {
                filenames.push(file_path.to_string_lossy().into_owned());
                total_matches += count;
            }
            continue;
        }

        let lines: Vec<&str> = file_contents.lines().collect();
        let mut matched_lines = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if regex.is_match(line) {
                total_matches += 1;
                matched_lines.push(index);
            }
        }

        if matched_lines.is_empty() {
            continue;
        }

        filenames.push(file_path.to_string_lossy().into_owned());
        if output_mode == "content" {
            for index in matched_lines {
                let start = index.saturating_sub(input.before.unwrap_or(context));
                let end = (index + input.after.unwrap_or(context) + 1).min(lines.len());
                for (current, line) in lines.iter().enumerate().take(end).skip(start) {
                    let prefix = if input.line_numbers.unwrap_or(true) {
                        format!("{}:{}:", file_path.to_string_lossy(), current + 1)
                    } else {
                        format!("{}:", file_path.to_string_lossy())
                    };
                    content_lines.push(format!("{prefix}{line}"));
                }
            }
        }
    }

    let total_files_before_limit = filenames.len();
    let (filenames, applied_limit, applied_offset) =
        apply_limit(filenames, input.head_limit, input.offset);
    if output_mode == "content" {
        return Ok(build_grep_content_output(
            output_mode,
            filenames,
            content_lines,
            input.head_limit,
            input.offset,
            total_files_before_limit,
            skipped_files,
        ));
    }

    Ok(GrepSearchOutput {
        mode: Some(output_mode.clone()),
        num_files: filenames.len(),
        filenames,
        content: None,
        num_lines: None,
        num_matches: (output_mode == "count").then_some(total_matches),
        applied_limit,
        applied_offset,
        // 截断前的真实匹配总数(诊断"假阴性"用)
        total_files_before_limit: Some(total_files_before_limit),
        // 跳过文件数 > 0 时才上报,避免污染常规输出
        skipped_files: (skipped_files > 0).then_some(skipped_files),
    })
}

fn build_grep_content_output(
    output_mode: String,
    filenames: Vec<String>,
    content_lines: Vec<String>,
    head_limit: Option<usize>,
    offset: Option<usize>,
    total_files_before_limit: usize,
    skipped_files: usize,
) -> GrepSearchOutput {
    let (lines, limit, offset) = apply_limit(content_lines, head_limit, offset);
    GrepSearchOutput {
        mode: Some(output_mode),
        num_files: filenames.len(),
        filenames,
        num_lines: Some(lines.len()),
        content: Some(lines.join("\n")),
        num_matches: None,
        applied_limit: limit,
        applied_offset: offset,
        total_files_before_limit: Some(total_files_before_limit),
        skipped_files: (skipped_files > 0).then_some(skipped_files),
    }
}

fn canonicalize_workspace_root(workspace_root: &Path) -> PathBuf {
    workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf())
}

fn should_skip_glob_dir(entry: &DirEntry) -> bool {
    entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .is_some_and(|name| GLOB_SEARCH_IGNORED_DIRS.contains(&name))
}

fn derive_glob_walk_root(pattern: &str) -> PathBuf {
    let path = Path::new(pattern);
    let mut prefix = PathBuf::new();
    let mut saw_component = false;

    for component in path.components() {
        let text = component.as_os_str().to_string_lossy();
        if component_contains_glob(&text) {
            break;
        }
        prefix.push(component.as_os_str());
        saw_component = true;
    }

    if saw_component {
        prefix
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }
}

fn component_contains_glob(component: &str) -> bool {
    component.contains('*') || component.contains('?') || component.contains('[')
}

fn collect_search_files(base_path: &Path) -> io::Result<Vec<PathBuf>> {
    if base_path.is_file() {
        return Ok(vec![base_path.to_path_buf()]);
    }

    let mut files = Vec::new();
    // 复用 glob_search 的目录过滤逻辑,避免爬 .git/target/node_modules 等重目录,
    // 这些目录既拖慢搜索又易触发 250 文件上限导致真实匹配被截断丢失。
    for entry in WalkDir::new(base_path)
        .into_iter()
        .filter_entry(|e| !should_skip_glob_dir(e))
    {
        let entry = entry.map_err(|error| io::Error::other(error.to_string()))?;
        if entry.file_type().is_file() {
            files.push(entry.path().to_path_buf());
        }
    }
    Ok(files)
}

fn matches_optional_filters(
    path: &Path,
    glob_filter: Option<&Pattern>,
    file_type: Option<&str>,
) -> bool {
    if let Some(glob_filter) = glob_filter {
        let path_string = path.to_string_lossy();
        if !glob_filter.matches(&path_string) && !glob_filter.matches_path(path) {
            return false;
        }
    }

    if let Some(file_type) = file_type {
        let extension = path.extension().and_then(|extension| extension.to_str());
        if extension != Some(file_type) {
            return false;
        }
    }

    true
}

fn apply_limit<T>(
    items: Vec<T>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> (Vec<T>, Option<usize>, Option<usize>) {
    let offset_value = offset.unwrap_or(0);
    let mut items = items.into_iter().skip(offset_value).collect::<Vec<_>>();
    let explicit_limit = limit.unwrap_or(250);
    if explicit_limit == 0 {
        return (items, None, (offset_value > 0).then_some(offset_value));
    }

    let truncated = items.len() > explicit_limit;
    items.truncate(explicit_limit);
    (
        items,
        truncated.then_some(explicit_limit),
        (offset_value > 0).then_some(offset_value),
    )
}

fn make_patch(original: &str, updated: &str) -> Vec<StructuredPatchHunk> {
    let mut lines = Vec::new();
    for line in original.lines() {
        lines.push(format!("-{line}"));
    }
    for line in updated.lines() {
        lines.push(format!("+{line}"));
    }

    vec![StructuredPatchHunk {
        old_start: 1,
        old_lines: original.lines().count(),
        new_start: 1,
        new_lines: updated.lines().count(),
        lines,
    }]
}

fn normalize_path(path: &str) -> io::Result<PathBuf> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir()?.join(path)
    };
    candidate.canonicalize()
}

fn normalize_path_allow_missing(path: &str) -> io::Result<PathBuf> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir()?.join(path)
    };

    if let Ok(canonical) = candidate.canonicalize() {
        return Ok(canonical);
    }

    if let Some(parent) = candidate.parent() {
        let canonical_parent = parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf());
        if let Some(name) = candidate.file_name() {
            return Ok(canonical_parent.join(name));
        }
    }

    Ok(candidate)
}

/// Read a file with workspace boundary enforcement.
#[allow(dead_code)]
pub fn read_file_in_workspace(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    workspace_root: &Path,
) -> io::Result<ReadFileOutput> {
    read_file_in_workspace_with_roots(path, offset, limit, workspace_root, &[])
}

/// Read a file with multi-root workspace boundary enforcement.
/// `extra_roots` 为 `--add-dir` 提供的额外允许根；路径落在主根或任一额外根内即放行。
#[allow(dead_code)]
pub fn read_file_in_workspace_with_roots(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    workspace_root: &Path,
    extra_roots: &[PathBuf],
) -> io::Result<ReadFileOutput> {
    let absolute_path = normalize_path(path)?;
    let roots = canonicalize_roots(workspace_root, extra_roots);
    validate_workspace_boundary_multi(&absolute_path, &roots)?;
    read_file(path, offset, limit)
}

/// Write a file with workspace boundary enforcement.
#[allow(dead_code)]
pub fn write_file_in_workspace(
    path: &str,
    content: &str,
    workspace_root: &Path,
) -> io::Result<WriteFileOutput> {
    write_file_in_workspace_with_roots(path, content, workspace_root, &[])
}

/// Write a file with multi-root workspace boundary enforcement.
#[allow(dead_code)]
pub fn write_file_in_workspace_with_roots(
    path: &str,
    content: &str,
    workspace_root: &Path,
    extra_roots: &[PathBuf],
) -> io::Result<WriteFileOutput> {
    let absolute_path = normalize_path_allow_missing(path)?;
    let roots = canonicalize_roots(workspace_root, extra_roots);
    validate_workspace_boundary_multi(&absolute_path, &roots)?;
    // BUG-P1-5 (TOCTOU): previously this called `write_file(path)`, which
    // re-normalized `path` and then `fs::write`-d. Between the boundary
    // check above and the write below, an attacker could replace `path`
    // with a symlink pointing outside the workspace, causing the write to
    // land on the symlink target. We now write to the already-validated
    // `absolute_path` directly and refuse to follow symlinks at the leaf.
    write_file_at_checked(&absolute_path, content)
}

/// Edit a file with workspace boundary enforcement.
#[allow(dead_code)]
pub fn edit_file_in_workspace(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    workspace_root: &Path,
) -> io::Result<EditFileOutput> {
    edit_file_in_workspace_with_roots(
        path,
        old_string,
        new_string,
        replace_all,
        workspace_root,
        &[],
    )
}

/// Edit a file with multi-root workspace boundary enforcement.
#[allow(dead_code)]
pub fn edit_file_in_workspace_with_roots(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    workspace_root: &Path,
    extra_roots: &[PathBuf],
) -> io::Result<EditFileOutput> {
    let absolute_path = normalize_path(path)?;
    let roots = canonicalize_roots(workspace_root, extra_roots);
    validate_workspace_boundary_multi(&absolute_path, &roots)?;
    // BUG-P1-5 (TOCTOU): write to the already-validated `absolute_path`
    // and refuse to follow symlinks at the leaf. See
    // `write_file_in_workspace_with_roots` for the full rationale.
    edit_file_at_checked(&absolute_path, old_string, new_string, replace_all)
}

/// Expand a glob pattern with workspace boundary enforcement.
#[allow(dead_code)]
pub fn glob_search_in_workspace(
    pattern: &str,
    path: Option<&str>,
    workspace_root: &Path,
) -> io::Result<GlobSearchOutput> {
    glob_search_impl(pattern, path, Some(workspace_root), &[])
}

/// Expand a glob pattern with multi-root workspace boundary enforcement.
#[allow(dead_code)]
pub fn glob_search_in_workspace_with_roots(
    pattern: &str,
    path: Option<&str>,
    workspace_root: &Path,
    extra_roots: &[PathBuf],
) -> io::Result<GlobSearchOutput> {
    glob_search_impl(pattern, path, Some(workspace_root), extra_roots)
}

/// Search file contents with workspace boundary enforcement.
#[allow(dead_code)]
pub fn grep_search_in_workspace(
    input: &GrepSearchInput,
    workspace_root: &Path,
) -> io::Result<GrepSearchOutput> {
    grep_search_impl(input, Some(workspace_root), &[])
}

/// Search file contents with multi-root workspace boundary enforcement.
#[allow(dead_code)]
pub fn grep_search_in_workspace_with_roots(
    input: &GrepSearchInput,
    workspace_root: &Path,
    extra_roots: &[PathBuf],
) -> io::Result<GrepSearchOutput> {
    grep_search_impl(input, Some(workspace_root), extra_roots)
}

/// Check whether a path is a symlink that resolves outside the workspace.
#[allow(dead_code)]
pub fn is_symlink_escape(path: &Path, workspace_root: &Path) -> io::Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_symlink() {
        return Ok(false);
    }
    let resolved = path.canonicalize()?;
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    Ok(!resolved.starts_with(&canonical_root))
}

/// Expand shell-style brace groups in a glob pattern.
///
/// Handles one level of braces: `foo.{a,b,c}` → `["foo.a", "foo.b", "foo.c"]`.
/// Nested braces are not expanded (uncommon in practice).
/// Patterns without braces pass through unchanged.
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_owned()];
    };
    let Some(close) = pattern[open..].find('}').map(|i| open + i) else {
        // Unmatched brace — treat as literal.
        return vec![pattern.to_owned()];
    };
    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    let alternatives = &pattern[open + 1..close];
    alternatives
        .split(',')
        .flat_map(|alt| expand_braces(&format!("{prefix}{alt}{suffix}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        component_contains_glob, derive_glob_walk_root, edit_file, expand_braces, glob_search,
        grep_search, is_symlink_escape, read_file, read_file_in_workspace, write_file,
        GrepSearchInput, MAX_WRITE_SIZE,
    };

    fn temp_path(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        std::env::temp_dir().join(format!("clawd-native-{name}-{unique}"))
    }

    #[test]
    fn reads_and_writes_files() {
        let path = temp_path("read-write.txt");
        let write_output = write_file(path.to_string_lossy().as_ref(), "one\ntwo\nthree")
            .expect("write should succeed");
        assert_eq!(write_output.kind, "create");

        let read_output = read_file(path.to_string_lossy().as_ref(), Some(1), Some(1))
            .expect("read should succeed");
        assert_eq!(read_output.file.content, "two");
    }

    #[test]
    fn edits_file_contents() {
        let path = temp_path("edit.txt");
        write_file(path.to_string_lossy().as_ref(), "alpha beta alpha")
            .expect("initial write should succeed");
        let output = edit_file(path.to_string_lossy().as_ref(), "alpha", "omega", true)
            .expect("edit should succeed");
        assert!(output.replace_all);
    }

    #[test]
    fn rejects_binary_files() {
        let path = temp_path("binary-test.bin");
        std::fs::write(&path, b"\x00\x01\x02\x03binary content").expect("write should succeed");
        let result = read_file(path.to_string_lossy().as_ref(), None, None);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("binary"));
    }

    #[test]
    fn rejects_oversized_writes() {
        let path = temp_path("oversize-write.txt");
        let huge = "x".repeat(MAX_WRITE_SIZE + 1);
        let result = write_file(path.to_string_lossy().as_ref(), &huge);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn enforces_workspace_boundary() {
        let workspace = temp_path("workspace-boundary");
        std::fs::create_dir_all(&workspace).expect("workspace dir should be created");
        let inside = workspace.join("inside.txt");
        write_file(inside.to_string_lossy().as_ref(), "safe content")
            .expect("write inside workspace should succeed");

        // Reading inside workspace should succeed
        let result =
            read_file_in_workspace(inside.to_string_lossy().as_ref(), None, None, &workspace);
        assert!(result.is_ok());

        // Reading outside workspace should fail
        let outside = temp_path("outside-boundary.txt");
        write_file(outside.to_string_lossy().as_ref(), "unsafe content")
            .expect("write outside should succeed");
        let result =
            read_file_in_workspace(outside.to_string_lossy().as_ref(), None, None, &workspace);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("escapes workspace"));
    }

    #[test]
    fn detects_symlink_escape() {
        let workspace = temp_path("symlink-workspace");
        std::fs::create_dir_all(&workspace).expect("workspace dir should be created");
        let outside = temp_path("symlink-target.txt");
        std::fs::write(&outside, "target content").expect("target should write");

        #[cfg(unix)]
        {
            let link_path = workspace.join("escape-link.txt");
            std::os::unix::fs::symlink(&outside, &link_path).expect("symlink should create");
            assert!(is_symlink_escape(&link_path, &workspace).expect("check should succeed"));
        }

        // Non-symlink file should not be an escape
        let normal = workspace.join("normal.txt");
        std::fs::write(&normal, "normal content").expect("normal file should write");
        assert!(!is_symlink_escape(&normal, &workspace).expect("check should succeed"));
    }

    #[test]
    #[cfg(unix)]
    fn workspace_read_rejects_symlink_escape_regression_3007_class() {
        let workspace = temp_path("workspace-read-symlink-escape");
        let outside = temp_path("workspace-read-symlink-target");
        std::fs::create_dir_all(&workspace).expect("workspace dir should be created");
        std::fs::create_dir_all(&outside).expect("outside dir should be created");
        let outside_file = outside.join("secret.txt");
        std::fs::write(&outside_file, "outside secret").expect("outside file should write");

        let link_path = workspace.join("linked-secret.txt");
        std::os::unix::fs::symlink(&outside_file, &link_path).expect("symlink should create");

        let result =
            read_file_in_workspace(link_path.to_string_lossy().as_ref(), None, None, &workspace);

        assert!(result.is_err(), "symlink escape must be rejected");
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains("escapes workspace"),
            "error should explain workspace escape: {error}"
        );

        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    #[cfg(unix)]
    fn workspace_write_rejects_parent_symlink_escape_regression_3007_class() {
        let workspace = temp_path("workspace-write-symlink-escape");
        let outside = temp_path("workspace-write-symlink-target");
        std::fs::create_dir_all(&workspace).expect("workspace dir should be created");
        std::fs::create_dir_all(&outside).expect("outside dir should be created");

        let link_dir = workspace.join("linked-outside");
        std::os::unix::fs::symlink(&outside, &link_dir).expect("symlink dir should create");
        let escaped_child = link_dir.join("created.txt");

        let result = write_file_in_workspace(
            escaped_child.to_string_lossy().as_ref(),
            "must not escape",
            &workspace,
        );

        assert!(result.is_err(), "parent symlink escape must be rejected");
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains("escapes workspace"),
            "error should explain workspace escape: {error}"
        );
        assert!(
            !outside.join("created.txt").exists(),
            "write should not create through an escaping symlink"
        );

        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn globs_and_greps_directory() {
        let dir = temp_path("search-dir");
        std::fs::create_dir_all(&dir).expect("directory should be created");
        let file = dir.join("demo.rs");
        write_file(
            file.to_string_lossy().as_ref(),
            "fn main() {\n println!(\"hello\");\n}\n",
        )
        .expect("file write should succeed");

        let globbed = glob_search("**/*.rs", Some(dir.to_string_lossy().as_ref()))
            .expect("glob should succeed");
        assert_eq!(globbed.num_files, 1);

        let grep_output = grep_search(&GrepSearchInput {
            pattern: String::from("hello"),
            path: Some(dir.to_string_lossy().into_owned()),
            glob: Some(String::from("**/*.rs")),
            output_mode: Some(String::from("content")),
            before: None,
            after: None,
            context_short: None,
            context: None,
            line_numbers: Some(true),
            case_insensitive: Some(false),
            file_type: None,
            head_limit: Some(10),
            offset: Some(0),
            multiline: Some(false),
        })
        .expect("grep should succeed");
        assert!(grep_output.content.unwrap_or_default().contains("hello"));
    }

    #[test]
    fn expand_braces_no_braces() {
        assert_eq!(expand_braces("*.rs"), vec!["*.rs"]);
    }

    #[test]
    fn expand_braces_single_group() {
        let mut result = expand_braces("Assets/**/*.{cs,uxml,uss}");
        result.sort();
        assert_eq!(
            result,
            vec!["Assets/**/*.cs", "Assets/**/*.uss", "Assets/**/*.uxml",]
        );
    }

    #[test]
    fn expand_braces_nested() {
        let mut result = expand_braces("src/{a,b}.{rs,toml}");
        result.sort();
        assert_eq!(
            result,
            vec!["src/a.rs", "src/a.toml", "src/b.rs", "src/b.toml"]
        );
    }

    #[test]
    fn expand_braces_unmatched() {
        assert_eq!(expand_braces("foo.{bar"), vec!["foo.{bar"]);
    }

    #[test]
    fn glob_search_with_braces_finds_files() {
        let dir = temp_path("glob-braces");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("b.toml"), "[package]").unwrap();
        std::fs::write(dir.join("c.txt"), "hello").unwrap();

        let result =
            glob_search("*.{rs,toml}", Some(dir.to_str().unwrap())).expect("glob should succeed");
        assert_eq!(
            result.num_files, 2,
            "should match .rs and .toml but not .txt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_search_skips_common_heavy_directories() {
        let dir = temp_path("glob-ignored-dirs");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(dir.join(".build/checkouts/pkg")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug/deps")).unwrap();

        std::fs::write(dir.join("src/AGENTS.md"), "src").unwrap();
        std::fs::write(dir.join("docs/AGENTS.md"), "docs").unwrap();
        std::fs::write(dir.join("node_modules/pkg/AGENTS.md"), "node_modules").unwrap();
        std::fs::write(dir.join(".build/checkouts/pkg/AGENTS.md"), ".build").unwrap();
        std::fs::write(dir.join("target/debug/deps/AGENTS.md"), "target").unwrap();

        let result =
            glob_search("**/AGENTS.md", Some(dir.to_str().unwrap())).expect("glob should succeed");

        assert_eq!(result.num_files, 2, "ignored dirs should be pruned");
        // 用 Path::ends_with 而非 str::ends_with,以兼容 Windows 的 \ 分隔符。
        assert!(result
            .filenames
            .iter()
            .any(|path| std::path::Path::new(path).ends_with("src/AGENTS.md")));
        assert!(result
            .filenames
            .iter()
            .any(|path| std::path::Path::new(path).ends_with("docs/AGENTS.md")));
        assert!(!result
            .filenames
            .iter()
            .any(|path| path.contains("node_modules")
                || path.contains(".build")
                || path.contains("\\target\\")
                || path.contains("/target/")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derive_glob_walk_root_stops_at_first_glob_component() {
        let root = derive_glob_walk_root("/tmp/demo/**/AGENTS.md");
        assert_eq!(root, PathBuf::from("/tmp/demo"));
        assert!(component_contains_glob("**"));
        assert!(component_contains_glob("*.rs"));
        assert!(!component_contains_glob("src"));
    }

    // ---- 痛点 1 回归测试:grep 静默跳过 bug 修复 ----

    /// 辅助:构造 GrepSearchInput(files_with_matches 模式)
    fn grep_files_with_matches(pattern: &str, path: &str, glob: Option<&str>) -> GrepSearchInput {
        GrepSearchInput {
            pattern: pattern.to_string(),
            path: Some(path.to_string()),
            glob: glob.map(str::to_string),
            output_mode: Some("files_with_matches".to_string()),
            before: None,
            after: None,
            context_short: None,
            context: None,
            line_numbers: None,
            case_insensitive: None,
            file_type: None,
            head_limit: None,
            offset: None,
            multiline: None,
        }
    }

    /// 回归:含 BOM 的 UTF-8 文件必须能被搜索到(原 read_to_string 会跳过)。
    #[test]
    fn grep_finds_matches_in_bom_prefixed_file() {
        let dir = temp_path("grep-bom");
        std::fs::create_dir_all(&dir).expect("dir");
        // UTF-8 BOM (EF BB BF) + "fn hello()"
        let bom_content: &[u8] = b"\xEF\xBB\xBFfn hello() {\n    println!(\"hello\");\n}\n";
        let file = dir.join("bom.rs");
        std::fs::write(&file, bom_content).expect("write");

        let out = grep_search(&grep_files_with_matches("hello", dir.to_str().unwrap(), Some("*.rs")))
            .expect("grep");
        assert_eq!(
            out.num_files, 1,
            "BOM 文件必须被搜索到(痛点 1 根因);skipped={:?}",
            out.skipped_files
        );
        assert!(out.filenames.iter().any(|f| f.contains("bom.rs")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归:含少量非 UTF-8 字节的文件也应被搜索(lossy 解码)。
    #[test]
    fn grep_finds_matches_in_partially_invalid_utf8_file() {
        let dir = temp_path("grep-invalid-utf8");
        std::fs::create_dir_all(&dir).expect("dir");
        // "fn broken() {" + 无效 UTF-8 字节 FF FE + " // hello marker"
        let mut content: Vec<u8> = b"fn broken() {\n".to_vec();
        content.extend_from_slice(b"\xFF\xFE"); // 无效 UTF-8
        content.extend_from_slice(b"    // hello marker\n}\n");
        let file = dir.join("broken.rs");
        std::fs::write(&file, &content).expect("write");

        let out = grep_search(&grep_files_with_matches("hello", dir.to_str().unwrap(), Some("*.rs")))
            .expect("grep");
        assert_eq!(
            out.num_files, 1,
            "部分非 UTF-8 文件应通过 lossy 解码被搜索;skipped={:?}",
            out.skipped_files
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归:二进制文件应被跳过并计入 skipped_files(而非静默丢失)。
    #[test]
    fn grep_skips_binary_file_and_reports_count() {
        let dir = temp_path("grep-binary");
        std::fs::create_dir_all(&dir).expect("dir");
        // 一个二进制文件(含 NUL)+ 一个正常文本文件,两者都含 "hello"
        std::fs::write(dir.join("bin.dat"), b"\x00\x01hello\x00\x02").expect("write bin");
        std::fs::write(dir.join("text.rs"), "fn hello() {}").expect("write text");

        let out = grep_search(&grep_files_with_matches("hello", dir.to_str().unwrap(), None))
            .expect("grep");
        // 文本文件应被找到
        assert_eq!(out.num_files, 1, "应只匹配文本文件");
        assert!(
            out.filenames.iter().any(|f| f.contains("text.rs")),
            "应找到 text.rs"
        );
        // 二进制跳过应在 skipped_files 中上报
        assert_eq!(
            out.skipped_files,
            Some(1),
            "二进制文件跳过必须上报(痛点 1:不再静默)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归:.git/target/node_modules 等重目录应被 grep 过滤(原 collect_search_files 不过滤)。
    #[test]
    fn grep_prunes_heavy_directories() {
        let dir = temp_path("grep-prune");
        std::fs::create_dir_all(dir.join("src")).expect("src");
        std::fs::create_dir_all(dir.join("target/debug")).expect("target");
        std::fs::create_dir_all(dir.join("node_modules/pkg")).expect("node_modules");

        std::fs::write(dir.join("src/real.rs"), "fn marker() {}").expect("write src");
        std::fs::write(dir.join("target/debug/junk.rs"), "fn marker() {}").expect("write target");
        std::fs::write(dir.join("node_modules/pkg/deps.rs"), "fn marker() {}").expect("write nm");

        let out = grep_search(&grep_files_with_matches("marker", dir.to_str().unwrap(), Some("*.rs")))
            .expect("grep");
        assert_eq!(
            out.num_files, 1,
            "应只匹配 src/real.rs,target/node_modules 应被过滤"
        );
        assert!(
            out.filenames.iter().any(|f| f.contains("src") && f.contains("real.rs")),
            "应找到 src/real.rs;实际: {:?}",
            out.filenames
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归:totalFilesBeforeLimit 在截断时上报真实总数(诊断假阴性)。
    #[test]
    fn grep_reports_total_before_limit_when_truncated() {
        let dir = temp_path("grep-truncate");
        std::fs::create_dir_all(&dir).expect("dir");
        // 创建 3 个匹配文件,但 head_limit=1
        for i in 0..3 {
            std::fs::write(dir.join(format!("f{i}.rs")), "fn marker() {}").expect("write");
        }

        let mut input = grep_files_with_matches("marker", dir.to_str().unwrap(), Some("*.rs"));
        input.head_limit = Some(1);
        let out = grep_search(&input).expect("grep");
        assert_eq!(out.num_files, 1, "head_limit=1 应只返回 1 个");
        assert_eq!(
            out.total_files_before_limit,
            Some(3),
            "应上报截断前真实总数 3(诊断假阴性)"
        );
        assert_eq!(out.applied_limit, Some(1), "应上报 applied_limit=1");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
