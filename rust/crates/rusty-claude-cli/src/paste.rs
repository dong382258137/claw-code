//! Paste handling: clipboard reading, paste folding, placeholder expansion.
//!
//! 参考 claude-code-best-source 的 pasteStore + usePasteHandler 机制：
//! - 粘贴超阈值（500 字符 / 3 行）时，把原始内容存到 paste-cache 目录
//! - 显示用占位符 `[Pasted text #1 +N lines]` 替代原始多行内容
//! - 提交给 LLM 时展开占位符为原始内容
//!
//! 与 claude-code-best-source 的差异：
//! - claude-code-best-source 在 paste 事件发生时即时拦截（React/Ink 层）
//! - claw 在 Submit 时后处理（rustyline 已显示原始内容，但提交后清除并显示占位符卡片）
//! - 视觉效果：用户看到原始多行 → 回车 → 原始内容被清除 → 显示折叠占位符卡片

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// TUI 模式静默标志：true 时禁用 eprintln! 调试日志（避免污染 alternate screen）。
/// 由 TUI 入口在启动时设置为 true。
static TUI_SILENT: AtomicBool = AtomicBool::new(false);

/// 设置 TUI 静默模式：TUI 启动时调用 `set_tui_silent(true)`，退出时 `false`。
pub(crate) fn set_tui_silent(silent: bool) {
    TUI_SILENT.store(silent, Ordering::Relaxed);
}

/// 诊断日志文件路径：`%USERPROFILE%\.claw\paste-debug.log`。
/// 用于排查 conhost 右键粘贴多行被切断的 BUG。无论 TUI 模式与否都写入文件，
/// 避免污染 alternate screen。
fn paste_diag_log_path() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(home).join(".claw").join("paste-debug.log"))
}

/// 诊断日志：追加写入 `~/.claw/paste-debug.log`，带时间戳。
/// 用于排查 TUI 多行粘贴 BUG（conhost 右键粘贴场景）。
/// 调用方应在关键路径加日志：Submit 入口、try_auto_expand_clipboard 触发前后、
/// skip_submit 判断、Event::Paste 接收等。
///
/// BUG-2 修复：用 `CLAW_PASTE_DEBUG=1` 环境变量门控，避免生产环境每次键盘
/// 事件都触发磁盘 I/O 与日志无限增长。环境变量只在首次调用时读取一次并缓存。
pub(crate) fn paste_diag_log(msg: &str) {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    let enabled = *ENABLED.get_or_init(|| {
        std::env::var_os("CLAW_PASTE_DEBUG")
            .map(|v| v == "1" || v == "true" || v == "TRUE")
            .unwrap_or(false)
    });
    if !enabled {
        return;
    }
    let Some(path) = paste_diag_log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(file, "[{timestamp}] {msg}");
    }
}

/// 内部调试日志：TUI 模式下静默，CLI 模式下输出到 stderr。
/// 同时追加到 `~/.claw/paste-debug.log` 用于诊断。
macro_rules! paste_log {
    ($($arg:tt)*) => {{
        let msg = format!($($arg)*);
        paste_diag_log(&msg);
        if !TUI_SILENT.load(Ordering::Relaxed) {
            eprintln!("{msg}");
        }
    }};
}

/// 粘贴折叠阈值：超过此字符数或行数则折叠为占位符。
/// 方案 A（激进，小粘贴也折叠）：500 字符 / 3 行。
pub(crate) const PASTE_FOLD_CHAR_THRESHOLD: usize = 500;
pub(crate) const PASTE_FOLD_LINE_THRESHOLD: usize = 3;

/// paste-cache 根目录：`%USERPROFILE%\.claw\paste-cache\`。
/// 与 sessions 目录平级，存放超阈值粘贴的原始内容。
pub(crate) fn paste_cache_root() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(home).join(".claw").join("paste-cache"))
}

/// 确保某个 paste-cache 文件名可用：返回完整路径。
/// 文件名格式：`<session_id>_<paste_id>.txt`，paste_id 在本会话内自增。
pub(crate) fn paste_cache_path(session_id: &str, paste_id: u32) -> Option<PathBuf> {
    let root = paste_cache_root()?;
    Some(root.join(format!("{session_id}_{paste_id}.txt")))
}

/// 计算字符串的"额外行数"（与 claude-code-best-source 的 getPastedTextRefNumLines 对齐）：
/// 换行符数量（即 `line1\nline2\nline3` 视为 +2 lines，不是 3 lines）。
pub(crate) fn pasted_text_ref_num_lines(text: &str) -> usize {
    text.chars().filter(|c| matches!(c, '\n' | '\r')).count()
}

/// 生成 `[Pasted text #<id> +<num_lines> lines]` 占位符。
/// num_lines 为 0 时省略 `+N lines` 部分。
pub(crate) fn format_pasted_text_ref(id: u32, num_lines: usize) -> String {
    if num_lines == 0 {
        format!("[Pasted text #{id}]")
    } else {
        format!("[Pasted text #{id} +{num_lines} lines]")
    }
}

/// 判断 input 是否应被折叠为 paste 占位符。
/// 触发条件：字符数 > 阈值 **或** 行数 > 阈值。
pub(crate) fn should_fold_paste(input: &str) -> bool {
    let char_count = input.chars().count();
    let line_count = input.lines().count();
    char_count > PASTE_FOLD_CHAR_THRESHOLD || line_count > PASTE_FOLD_LINE_THRESHOLD
}

/// 把超阈值的粘贴内容存到 paste-cache，返回占位符字符串。
/// 存储失败时（如磁盘满），退化为不折叠（返回原始内容）。
pub(crate) fn store_paste_and_make_placeholder(
    input: &str,
    session_id: &str,
    paste_id: u32,
) -> String {
    let Some(path) = paste_cache_path(session_id, paste_id) else {
        // 无 USERPROFILE/HOME 环境变量，无法存储，退化为不折叠。
        return input.to_string();
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if let Err(error) = std::fs::create_dir_all(parent) {
        paste_log!("[paste-store] create_dir_all failed: {error}; fallback to inline");
        return input.to_string();
    }
    if let Err(error) = std::fs::write(&path, input) {
        paste_log!("[paste-store] write failed: {error}; fallback to inline");
        return input.to_string();
    }
    let num_lines = pasted_text_ref_num_lines(input);
    format_pasted_text_ref(paste_id, num_lines)
}

/// P3 主入口：处理用户输入，返回 (display_text, expanded_text)。
/// - display_text: 用于 user 卡片显示（可能含占位符）
/// - expanded_text: 实际发送给 LLM 的内容（始终是原始展开内容）
///
/// 对于 slash 命令（以 `/` 开头），不进行折叠（命令本身不会很长）。
/// 对于 bare skill 触发，也不折叠（skill 名通常很短）。
///
/// `paste_id_gen` 是本会话的自增 paste id 生成器（&mut u32）。
pub(crate) fn fold_pasted_input(
    input: &str,
    session_id: &str,
    paste_id_gen: &mut u32,
) -> (String, String) {
    // slash 命令不折叠
    if input.trim_start().starts_with('/') {
        return (input.to_string(), input.to_string());
    }
    if !should_fold_paste(input) {
        return (input.to_string(), input.to_string());
    }
    *paste_id_gen += 1;
    let paste_id = *paste_id_gen;
    let display = store_paste_and_make_placeholder(input, session_id, paste_id);
    // expanded_text 始终是原始内容（LLM 需要看到完整粘贴）
    (display, input.to_string())
}

/// 展开输入中的所有 `[Pasted text #N +M lines]` 占位符为原始内容。
/// 从 paste-cache 读取对应文件。如果文件不存在（已被清理），占位符保留原样。
///
/// 用简单字符串解析替代正则（避免引入 regex 依赖）。
/// 占位符格式：`[Pasted text #<id>]` 或 `[Pasted text #<id> +<n> lines]`。
pub(crate) fn expand_paste_placeholders(input: &str, session_id: &str) -> String {
    const PREFIX: &str = "[Pasted text #";
    let mut result = String::new();
    let mut remaining = input;
    loop {
        let Some(start) = remaining.find(PREFIX) else {
            result.push_str(remaining);
            break;
        };
        // 把 prefix 之前的内容原样追加
        result.push_str(&remaining[..start]);
        let after_prefix = &remaining[start + PREFIX.len()..];
        // 找到占位符的闭合 `]`
        let Some(end) = after_prefix.find(']') else {
            // 没有闭合，原样追加剩余
            result.push_str(&remaining[start..]);
            break;
        };
        let inner = &after_prefix[..end];
        // inner 格式：`<id>` 或 `<id> +<n> lines`
        let id_str = inner.split_whitespace().next().unwrap_or("");
        let paste_id: u32 = id_str.parse().unwrap_or(0);
        if paste_id == 0 {
            // 解析失败，保留原占位符
            result.push_str(&remaining[start..=start + PREFIX.len() + end]);
        } else {
            let replacement = paste_cache_path(session_id, paste_id)
                .and_then(|path| std::fs::read_to_string(&path).ok())
                .unwrap_or_else(|| {
                    // 文件不存在，保留原占位符
                    remaining[start..=start + PREFIX.len() + end].to_string()
                });
            result.push_str(&replacement);
        }
        remaining = &after_prefix[end + 1..];
    }
    result
}

/// 读取 Windows 剪贴板中的图片，保存为 PNG 到 paste-cache，返回 `@<路径>` 字符串。
///
/// 使用 PowerShell + System.Windows.Forms 检测剪贴板是否包含图片。
/// 需要 STA 线程模式（PowerShell 默认），否则 OLE 剪贴板操作可能失败。
///
/// **返回值**：
/// - `Ok(Some("@path"))`: 检测到图片并成功保存
/// - `Ok(None)`: 剪贴板中无图片
/// - `Err(...)`: 检测过程出错（如 PowerShell 不可用）
pub(crate) fn read_clipboard_image(
    session_id: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let root = match paste_cache_root() {
        Some(r) => r,
        None => return Ok(None),
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let filename = format!("clipboard_img_{session_id}_{timestamp}.png");
    let path = root.join(&filename);

    if let Err(e) = std::fs::create_dir_all(&root) {
        paste_log!("[paste-img] create_dir_all 失败: {e}");
        return Ok(None);
    }

    let path_str = path.to_string_lossy().to_string();
    let ps_script = format!(
        "\
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
if ([System.Windows.Forms.Clipboard]::ContainsImage()) {{
    $img = [System.Windows.Forms.Clipboard]::GetImage()
    $img.Save('{}')
    Write-Output 'IMAGE_OK'
}} else {{
    Write-Output 'NO_IMAGE'
}}",
        path_str.replace('\'', "''")
    );

    let output = std::process::Command::new("powershell")
        .args([
            "-STA",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &ps_script,
        ])
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() {
        paste_log!(
            "[paste-img] PowerShell 失败: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(None);
    }

    if stdout == "IMAGE_OK" {
        paste_log!("[paste-img] 图片已保存到 {:?}", path);
        Ok(Some(format!("@{path_str}")))
    } else {
        Ok(None)
    }
}

/// 读取 Windows 剪贴板文本内容。
/// 用 PowerShell `Get-Clipboard` 命令获取，绕过终端粘贴机制。
/// 适用于 cmd.exe/conhost 等不支持 bracketed paste 的终端。
///
/// 返回剪贴板的原始文本（可能含多行）。如果剪贴板里是图片或其他非文本格式，
/// 返回空字符串。
pub(crate) fn read_clipboard_text() -> Result<String, Box<dyn std::error::Error>> {
    // 关键修复：PowerShell 默认输出编码是系统 ANSI 代码页（中文系统是 GBK/CP936），
    // 中文字符会被编码为多字节 GBK。Rust 的 `String::from_utf8_lossy` 会把无效
    // UTF-8 字节替换为 U+FFFD，导致后续字符串匹配失败（例如 `try_auto_expand_clipboard`
    // 里 `first_line != user_input` 永远成立，自动剪贴板检测不触发）。
    //
    // 修复：在 PowerShell 命令前显式设置 `[Console]::OutputEncoding = UTF8`，
    // 让 PowerShell 以 UTF-8 输出到 stdout，并 strip 掉可能的 UTF-8 BOM。
    let ps_script = "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; Get-Clipboard -Raw";
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", ps_script])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "Get-Clipboard failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let mut bytes = output.stdout;
    // PowerShell UTF-8 输出可能带 BOM（EF BB BF），strip 掉以免干扰字符串匹配。
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        bytes.drain(..3);
    }
    let text = String::from_utf8_lossy(&bytes).to_string();
    // Get-Clipboard -Raw 保留原始换行，但可能尾部有额外换行，trim 一下尾部。
    Ok(text.trim_end_matches(['\r', '\n']).to_string())
}

/// P3 自动剪贴板检测：检查剪贴板是否有多行内容，且第一行等于用户输入。
/// 如果匹配，用剪贴板完整内容替换用户输入，并填充 pending_paste_lines
/// 以便主循环丢弃后续被 conhost 逐行发送的行。
///
/// 返回 `Some((display, expanded, raw_clipboard))` 如果触发了剪贴板替换；
/// `None` 表示未触发。第三个元素是原始剪贴板内容，供调用方复用，避免
/// 重复调用 `read_clipboard_text`（P0-1 优化：原 TUI 路径触发后还会再读一次
/// 剪贴板用于写临时文件，两次 PowerShell 调用 = 200-1000ms 主线程冻结）。
///
/// 触发条件（全部满足）：
/// 1. 剪贴板内容是多行（行数 > 1）
/// 2. 剪贴板第一行（trim 后）等于用户输入（trim 后）
///
/// 无论是否超折叠阈值，都替换为完整内容（fold_pasted_input 内部决定是否折叠）。
///
/// 性能考虑：此函数会调用 PowerShell Get-Clipboard（~100ms 开销）。
/// 只在用户输入是单行、不以 / 开头、且 pending_paste_lines 为空时调用。
pub(crate) fn try_auto_expand_clipboard(
    user_input: &str,
    session_id: &str,
    paste_id_gen: &mut u32,
    pending_paste_lines: &mut Vec<String>,
) -> Option<(String, String, String)> {
    paste_log!("[paste-dbg] user_input={:?}", user_input);
    let clipboard = match read_clipboard_text() {
        Ok(c) => c,
        Err(e) => {
            paste_log!("[paste-dbg] read_clipboard_text failed: {e}");
            return None;
        }
    };
    paste_log!(
        "[paste-dbg] clipboard len={} lines={}",
        clipboard.chars().count(),
        clipboard.lines().count()
    );
    if clipboard.is_empty() {
        paste_log!("[paste-dbg] clipboard empty, skip");
        return None;
    }
    let clipboard_lines: Vec<&str> = clipboard.lines().collect();
    // 必须是多行内容（>1 行）才触发
    if clipboard_lines.len() <= 1 {
        paste_log!("[paste-dbg] only {} lines, skip", clipboard_lines.len());
        return None;
    }
    // 剪贴板首行必须匹配用户输入的**尾部**（trim 后比较）。
    // 支持两种场景：
    // 1. 空输入粘贴：user_input == first_line（精确匹配）
    // 2. 有前缀文字粘贴：user_input = "前缀文字" + first_line（结尾匹配）
    //    例如用户先输入"这段代码有什么问题："再粘贴代码
    let first_line = clipboard_lines[0].trim();
    let user_trimmed = user_input.trim();
    paste_log!(
        "[paste-dbg] first_line={:?} user_input={:?}",
        first_line,
        user_trimmed
    );
    if first_line.is_empty() || !user_trimmed.ends_with(first_line) {
        paste_log!("[paste-dbg] first line mismatch (not suffix), skip");
        return None;
    }
    // 触发剪贴板替换：用完整内容走折叠流程（fold_pasted_input 内部决定是否折叠）
    let (display, expanded) = fold_pasted_input(&clipboard, session_id, paste_id_gen);
    // 把剩余行填入 pending_paste_lines，主循环会丢弃匹配的后续 Submit。
    //
    // 过滤掉 trim 后为空的行：conhost 粘贴空行时发送空 Submit，
    // 主循环 `if trimmed.is_empty() { continue; }` 会直接跳过，
    // 如果 pending_paste_lines 包含空行，会导致下一个非空 Submit 不匹配。
    // 所以这里主动过滤空行，让 pending_paste_lines 只包含非空行。
    *pending_paste_lines = clipboard_lines[1..]
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    paste_log!(
        "[paste-dbg] triggered! pending={} lines",
        pending_paste_lines.len()
    );
    Some((display, expanded, clipboard))
}

/// conhost TUI 路径专用：把剪贴板完整内容写到临时文件，返回 `@<文件路径>` 字符串。
///
/// **设计动机**：conhost 不支持 bracketed paste，Ctrl+V 粘贴多行文本时，crossterm
/// 把剪贴板内容作为普通字符序列处理，第一行的 `\n` 必然触发 Submit。即使
/// `try_auto_expand_clipboard` 能兜底发送完整内容，但用户无法"先编辑再发送"。
///
/// **新行为**：在 Submit 入口检测到 conhost 多行粘贴时，把完整剪贴板内容写到
/// `%USERPROFILE%\.claw\paste-cache\clipboard_<timestamp>.txt`，返回 `@<文件路径>`。
/// 主循环把 `@<路径>` 填充到 InputLine buffer，不发送给 AI。用户看到 `@<路径>`
/// 后可以继续编辑或直接按 Enter 发送，AI 会看到 `@<路径>` 然后读取文件内容。
///
/// **参数**：
/// - `clipboard`: 完整剪贴板内容
/// - `session_id`: 会话 ID（用于日志）
///
/// **返回**：
/// - `Ok(Some(path_str))`: 文件写入成功，返回 `@<路径>` 字符串
/// - `Ok(None)`: 剪贴板为空或写入失败，主循环应回退到原行为
pub(crate) fn write_clipboard_to_temp_file(clipboard: &str, session_id: &str) -> Option<String> {
    if clipboard.trim().is_empty() {
        return None;
    }

    let root = paste_cache_root()?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let filename = format!("clipboard_{session_id}_{timestamp}.txt");
    let path = root.join(&filename);

    // 确保目录存在
    if let Err(e) = std::fs::create_dir_all(&root) {
        paste_log!("[paste-dbg] write_clipboard_to_temp_file: create_dir_all 失败: {e}");
        return None;
    }

    // 写入文件
    if let Err(e) = std::fs::write(&path, clipboard) {
        paste_log!("[paste-dbg] write_clipboard_to_temp_file: write 失败: {e}");
        return None;
    }

    paste_log!(
        "[paste-dbg] write_clipboard_to_temp_file: 已写入 {} 字节到 {:?}",
        clipboard.len(),
        path
    );

    // 返回 @<路径>
    let path_str = path.to_string_lossy().to_string();
    Some(format!("@{path_str}"))
}
