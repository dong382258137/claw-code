//! 语音输入(本地离线语音转文字)。
//!
//! `/voice [seconds]` 与 `/listen [seconds]` 的实现:
//! 1. 用系统 ffmpeg 录制麦克风 → 16kHz 单声道 WAV
//! 2. 调用本地 `local-asr` 技能(Qwen3-ASR via OpenVINO,可跑 NPU/GPU/CPU)
//! 3. 识别文本通过 draft 槽投递给 TUI 主循环,自动填入输入框;
//!    无 draft 槽(普通 REPL)时直接打印识别结果
//!
//! 全部推理在本机完成,不依赖云端。首次使用会经 `install-env.ps1` 创建
//! Python 虚拟环境并下载模型(约 2GB),耗时较长,已给出超时与提示。

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::app::LiveCli;

/// 默认录音秒数。
const DEFAULT_RECORD_SECONDS: u64 = 5;
/// 录音秒数允许范围。
const MIN_RECORD_SECONDS: u64 = 1;
const MAX_RECORD_SECONDS: u64 = 60;
/// 首次识别可能触发模型/环境准备,给足 10 分钟。
const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(600);
/// 环境安装(创建 venv + 安装依赖)给 15 分钟。
const INSTALL_ENV_TIMEOUT: Duration = Duration::from_secs(900);
/// 子进程轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// draft 槽:TUI 主循环注册一个 Sender,worker 线程把识别文本送回输入框。
static DRAFT_SINK: OnceLock<Mutex<Option<mpsc::Sender<String>>>> = OnceLock::new();

/// 注册(或注销)识别文本投递通道。TUI 启动时调用,退出时以 `None` 清理。
pub(crate) fn set_draft_sink(sender: Option<mpsc::Sender<String>>) {
    let pool = DRAFT_SINK.get_or_init(|| Mutex::new(None));
    *pool.lock().unwrap_or_else(|e| e.into_inner()) = sender;
}

/// 把识别文本投递给 TUI 输入框;无接收者时静默丢弃(普通 REPL 场景不依赖它)。
pub(crate) fn emit_draft(text: String) {
    let Some(pool) = DRAFT_SINK.get() else {
        return;
    };
    let Ok(guard) = pool.lock() else {
        return;
    };
    if let Some(tx) = guard.as_ref() {
        let _ = tx.send(text);
    }
}

/// 解析 `/voice [seconds]` / `/listen [seconds]` 的秒数参数。
///
/// 兼容旧语义的 `on`/`off`(原“语音输入模式开关”),返回默认秒数。
pub(crate) fn parse_seconds(hint: Option<&str>) -> Result<u64, String> {
    let Some(raw) = hint.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(DEFAULT_RECORD_SECONDS);
    };
    if matches!(raw, "on" | "off") {
        return Ok(DEFAULT_RECORD_SECONDS);
    }
    let seconds: u64 = raw
        .parse()
        .map_err(|_| format!("无效的录音秒数 {raw:?},示例:`/listen 8`"))?;
    if !(MIN_RECORD_SECONDS..=MAX_RECORD_SECONDS).contains(&seconds) {
        return Err(format!(
            "录音秒数需在 {MIN_RECORD_SECONDS}..={MAX_RECORD_SECONDS} 之间,收到 {seconds}"
        ));
    }
    Ok(seconds)
}

/// 定位 ffmpeg:优先 `CLAW_FFMPEG` 环境变量,其次 PATH。
fn find_ffmpeg() -> Option<PathBuf> {
    if let Ok(custom) = std::env::var("CLAW_FFMPEG") {
        let p = PathBuf::from(custom);
        if p.is_file() {
            return Some(p);
        }
    }
    let exe = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        let candidate = dir.join(exe);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 列出 DirectShow 输入音频设备(麦克风)。
fn list_input_devices(ffmpeg: &Path) -> Result<Vec<String>, String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args([
        "-hide_banner",
        "-list_devices",
        "true",
        "-f",
        "dshow",
        "-i",
        "dummy",
    ]);
    let output = run_with_timeout(&mut cmd, Duration::from_secs(30), "枚举音频设备")?;
    let stderr = String::from_utf8_lossy(&output.1).into_owned();
    let devices = parse_dshow_devices(&stderr);
    if devices.is_empty() {
        return Err("未检测到录音设备(ffmpeg 未列出任何 DirectShow 音频输入)".to_string());
    }
    Ok(devices)
}

/// 纯函数:从 `ffmpeg -list_devices` 的 stderr 文本提取输入音频设备名。
///
/// 兼容两种 ffmpeg 输出风格:
/// - 旧版(ffmpeg ≤7)有 "DirectShow audio devices" 标题,设备名行无后缀:
///   ```text
///   [dshow @ ...] DirectShow audio devices
///   [dshow @ ...]  "Microphone Array (Realtek(R) Audio)"
///   ```
/// - 新版(ffmpeg 8+)无标题,设备行内联 `(audio)`/`(video)` 标记:
///   ```text
///   [in#0 @ ...] "Integrated Camera" (video)
///   [in#0 @ ...] "麦克风阵列 (适用于数字麦克风的英特尔® 智音技术)" (audio)
///   ```
/// Alternative name 行(以 `@` 开头)与视频设备一律排除。
pub(crate) fn parse_dshow_devices(stderr: &str) -> Vec<String> {
    let mut in_audio_section = false;
    let mut out: Vec<String> = Vec::new();
    for line in stderr.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("directshow audio devices") {
            in_audio_section = true;
            continue;
        }
        if lower.contains("directshow video devices") {
            in_audio_section = false;
            continue;
        }
        let Some(name) = extract_quoted_device_name(line) else {
            continue;
        };
        // 跳过 Alternative name 行(形如 "@device_pnp_...")和非设备行。
        if name.starts_with('@') {
            continue;
        }
        let suffix = rest_after_closing_quote(line)
            .unwrap_or_default()
            .trim_start()
            .to_ascii_lowercase();
        if suffix.starts_with("(video") {
            continue;
        }
        let is_audio = suffix.starts_with("(audio") || in_audio_section;
        if is_audio && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

fn extract_quoted_device_name(line: &str) -> Option<String> {
    let start = line.find('"')?;
    let rest = &line[start + 1..];
    let end = rest.find('"')?;
    let name = rest[..end].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// 返回闭合引号之后的内容(用于判断 `(audio)`/`(video)` 标记)。
fn rest_after_closing_quote(line: &str) -> Option<&str> {
    let open = line.find('"')?;
    let close = line[open + 1..].find('"')? + open + 1;
    Some(&line[close + 1..])
}

/// 从设备列表里挑一个最像麦克风的;无匹配则取第一个。
fn pick_input_device(devices: &[String]) -> Result<String, String> {
    let first = devices.first().ok_or_else(|| "设备列表为空".to_string());
    for device in devices {
        let lower = device.to_ascii_lowercase();
        if (lower.contains("mic") || lower.contains("麦克风"))
            && !lower.contains("stereo mix")
            && !lower.contains("aux")
        {
            return Ok(device.clone());
        }
    }
    first.cloned()
}

/// 用 ffmpeg 录制 `seconds` 秒 → 16kHz 单声道 PCM WAV。
fn record_audio(ffmpeg: &Path, device_name: &str, seconds: u64, out: &Path) -> Result<(), String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args([
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "dshow",
        "-i",
    ]);
    cmd.arg(format!("audio={device_name}"));
    cmd.arg("-t").arg(seconds.to_string());
    cmd.args(["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"]);
    cmd.arg(out);
    let output = run_with_timeout(&mut cmd, Duration::from_secs(seconds + 10), "录音")?;
    if output.0.success() && out.is_file() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.1);
    let last = detail
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or("未知错误");
    Err(format!("录音失败(设备可能被占用): {last}"))
}

/// 定位 local-asr 技能根目录:优先 `CLAW_ASR_SKILL_DIR`,其次常见安装位置。
fn find_asr_skill_root() -> Option<PathBuf> {
    if let Ok(custom) = std::env::var("CLAW_ASR_SKILL_DIR") {
        let p = PathBuf::from(custom);
        if is_asr_skill(&p) {
            return Some(p);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("skills").join("local-asr"));
            candidates.push(dir.join("local-asr"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("skills").join("local-asr"));
        candidates.push(cwd.join(".claw").join("skills").join("local-asr"));
    }
    if let Ok(home) = home_dir() {
        candidates.push(home.join(".claw").join("skills").join("local-asr"));
    }
    candidates.into_iter().find(|p| is_asr_skill(p))
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var("USERPROFILE")
        .ok()
        .or_else(|| std::env::var("HOME").ok())
        .map(PathBuf::from)
        .ok_or_else(|| "无法定位用户主目录".to_string())
}

fn is_asr_skill(root: &Path) -> bool {
    root.join("scripts").join("client.py").is_file() && root.join("info.json").is_file()
}

/// 解析 local-asr 的 info.json,得到其 venv 内 python.exe 绝对路径。
fn asr_venv_python(root: &Path) -> Result<PathBuf, String> {
    let info_text = std::fs::read_to_string(root.join("info.json"))
        .map_err(|e| format!("读取 info.json 失败: {e}"))?;
    let info: serde_json::Value =
        serde_json::from_str(&info_text).map_err(|e| format!("解析 info.json 失败: {e}"))?;
    let venv_name = info
        .get("venv_name")
        .and_then(|v| v.as_str())
        .unwrap_or("asr-cu");
    let home = home_dir()?;
    Ok(home
        .join(".openvino")
        .join("venv")
        .join(venv_name)
        .join("Scripts")
        .join("python.exe"))
}

/// 确保 local-asr 的 venv 存在;缺失时运行 install-env.ps1(首次使用)。
fn ensure_venv(root: &Path, venv_py: &Path) -> Result<(), String> {
    if venv_py.is_file() {
        return Ok(());
    }
    let install_script = root.join("scripts").join("install-env.ps1");
    if !install_script.is_file() {
        return Err("local-asr 技能缺少 install-env.ps1,无法初始化 Python 环境".to_string());
    }
    let powershell = if cfg!(windows) {
        "powershell.exe"
    } else {
        "powershell"
    };
    let mut cmd = Command::new(powershell);
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ]);
    cmd.arg(&install_script);
    cmd.args(["-SkillRoot"]).arg(root);
    let output = run_with_timeout(&mut cmd, INSTALL_ENV_TIMEOUT, "初始化本地识别环境")?;
    if venv_py.is_file() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.1);
    Err(format!(
        "本地识别环境初始化失败:{}",
        detail
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("未知错误")
    ))
}

/// 调用 local-asr 的 client.py 转写音频文件,返回识别文本。
fn transcribe(root: &Path, wav: &Path, venv_py: &Path) -> Result<String, String> {
    let client = root.join("scripts").join("client.py");
    let mut cmd = Command::new(venv_py);
    cmd.arg(&client);
    cmd.arg("--audio").arg(wav);
    cmd.arg("--language").arg("auto");
    let output = run_with_timeout(&mut cmd, TRANSCRIBE_TIMEOUT, "语音识别")?;
    if !output.0.success() {
        // client.py 友好错误(如"模型正在下载,请用命令 scripts\run.ps1 --continue 继续")
        // 打到 stdout;stderr 才是内部错误。优先展示 stdout 最后一行。
        let msg = String::from_utf8_lossy(&output.1);
        let last = msg
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("识别失败");
        return Err(last.to_string());
    }
    extract_result_text(&String::from_utf8_lossy(&output.1))
}

/// 纯函数:从 client.py 的 stdout 提取第一个 `=== RESULT … ===` 后的 JSON `text` 字段。
fn extract_result_text(stdout: &str) -> Result<String, String> {
    // 兼容 `=== RESULT ===`(单文件)与 `=== RESULT 1/2 ===`(批量)。
    const MARK: &str = "=== RESULT";
    let marker_pos = stdout
        .find(MARK)
        .ok_or_else(|| "识别结果解析失败:未找到 RESULT 标记".to_string())?;
    let after = &stdout[marker_pos + MARK.len()..];
    let json_start = after
        .find('{')
        .ok_or_else(|| "识别结果解析失败:RESULT 后缺少 JSON".to_string())?;
    let json_span = brace_balanced(&after[json_start..])
        .ok_or_else(|| "识别结果解析失败:JSON 不完整".to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(json_span).map_err(|e| format!("识别结果 JSON 解析失败: {e}"))?;
    let text = value
        .get("text")
        .and_then(|t| t.as_str())
        .ok_or_else(|| "识别结果缺少 text 字段".to_string())?;
    let text = text.trim();
    if text.is_empty() {
        return Err("未识别到语音内容,请靠近麦克风后重试".to_string());
    }
    Ok(text.to_string())
}

/// 从 `{` 起截取到配平的 `}`(字符串内容不参与计数)。
fn brace_balanced(s: &str) -> Option<&str> {
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (idx, byte) in s.bytes().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[..=idx]);
                }
            }
            _ => {}
        }
    }
    None
}

/// 带超时的子进程运行器:stdout/stderr 统一落盘,返回 (退出码, 输出字节)。
///
/// 用文件代替管道,避免大输出(模型下载进度等)填满管道缓冲导致死锁。
fn run_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
    what: &str,
) -> Result<(ExitStatus, Vec<u8>), String> {
    let dir = temp_voice_dir()?;
    let out_file = dir.join(format!("proc-{}.out", unique_suffix()));
    let file = std::fs::File::create(&out_file)
        .map_err(|e| format!("创建输出文件失败({out_file:?}): {e}"))?;
    let mut child = cmd
        .stdout(Stdio::from(
            file.try_clone().map_err(|e| format!("克隆句柄失败: {e}"))?,
        ))
        .stderr(Stdio::from(file))
        .spawn()
        .map_err(|e| format!("无法启动 {what}: {e}"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_file(&out_file);
                    return Err(format!("{what}超时(>{timeout:?}),已终止"));
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&out_file);
                return Err(format!("等待{what}失败: {e}"));
            }
        }
    };
    let bytes = std::fs::read(&out_file).unwrap_or_default();
    let _ = std::fs::remove_file(&out_file);
    Ok((status, bytes))
}

fn temp_voice_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("claw-voice");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    Ok(dir)
}

/// 文件名的唯一后缀:进程号 + 时间戳毫秒。
fn unique_suffix() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{}-{millis}", std::process::id())
}

fn display_text(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// 从 info.json 读取首个模型的目录名 `dir_name`。
fn asr_model_dir_name(root: &Path) -> Result<String, String> {
    let info_text = std::fs::read_to_string(root.join("info.json"))
        .map_err(|e| format!("读取 info.json 失败: {e}"))?;
    let info: serde_json::Value =
        serde_json::from_str(&info_text).map_err(|e| format!("解析 info.json 失败: {e}"))?;
    let model = info
        .get("models")
        .and_then(|m| m.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| "info.json 缺少 models 配置".to_string())?;
    model
        .get("dir_name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "info.json models 缺少 dir_name".to_string())
}

/// 模型正式目录:`~/.openvino/models/<dir_name>`(下载完成后的目录)。
fn asr_model_dir(root: &Path) -> Result<PathBuf, String> {
    let dir_name = asr_model_dir_name(root)?;
    let home = home_dir()?;
    Ok(home.join(".openvino").join("models").join(dir_name))
}

/// 从 info.json 读取首个模型要求的文件列表(相对路径)。
fn asr_required_files(root: &Path) -> Result<Vec<String>, String> {
    let info_text = std::fs::read_to_string(root.join("info.json"))
        .map_err(|e| format!("读取 info.json 失败: {e}"))?;
    let info: serde_json::Value =
        serde_json::from_str(&info_text).map_err(|e| format!("解析 info.json 失败: {e}"))?;
    let model = info
        .get("models")
        .and_then(|m| m.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| "info.json 缺少 models 配置".to_string())?;
    Ok(model
        .get("required_files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| f.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// 识别模型是否已下载完整(required_files 全在正式目录)。
pub(crate) fn asr_model_ready(root: &Path) -> Result<bool, String> {
    let model_dir = asr_model_dir(root)?;
    let required = asr_required_files(root)?;
    Ok(model_ready_in(&model_dir, &required))
}

fn model_ready_in(model_dir: &Path, required: &[String]) -> bool {
    required.iter().all(|f| model_dir.join(f).is_file())
}

/// 已下载字节数(partial 目录递归求和),用于提示下载进度。
fn asr_partial_mb(root: &Path) -> u64 {
    let Ok(dir) = asr_model_dir_name(root) else {
        return 0;
    };
    let Ok(home) = home_dir() else {
        return 0;
    };
    let partial = home
        .join(".openvino")
        .join("models")
        .join(format!("{dir}.partial"));
    dir_size_mb(&partial)
}

/// 目录递归大小(MB),用于下载进度提示。
fn dir_size_mb(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(current) = stack.pop() {
        if current.is_file() {
            total += current.metadata().map(|m| m.len()).unwrap_or(0);
        } else if let Ok(entries) = std::fs::read_dir(&current) {
            stack.extend(entries.flatten().map(|e| e.path()));
        }
    }
    total / (1024 * 1024)
}

/// `/voice [seconds]` / `/listen [seconds]` 的执行入口。
///
/// 流程:校验识别模型就绪 → 录音 → 本地识别 → 文本填入输入框(TUI)draft 槽;
/// 无槽时直接打印。模型未就绪(首次下载中)时给出明确提示,不阻塞、不白录音。
pub(crate) fn run_voice_input(cli: &mut LiveCli, hint: Option<&str>) -> Result<(), String> {
    let seconds = parse_seconds(hint)?;
    let progress = |msg: &str| {
        if !cli.tui_println(msg) {
            println!("{msg}");
        }
    };

    // 先确认技能、虚拟环境与模型就绪,避免"录完才发现不能用"。
    let skill_root = find_asr_skill_root().ok_or_else(|| {
        "找不到 local-asr 技能(请确认已安装 claw 附带技能,或用 CLAW_ASR_SKILL_DIR 指定)".to_string()
    })?;
    let venv_py = asr_venv_python(&skill_root)?;
    ensure_venv(&skill_root, &venv_py)?;
    if !asr_model_ready(&skill_root)? {
        let downloaded_mb = asr_partial_mb(&skill_root);
        return Err(format!(
            "识别模型尚未下载完成(首次使用约需 2GB,当前已下载约 {downloaded_mb} MB,后台持续下载中)。\
             下载完成后再次按 F4(或输入 /voice)即可直接使用。"
        ));
    }

    progress(&format!("🎤 开始录音 {seconds} 秒,请开始说话…"));
    let ffmpeg = find_ffmpeg().ok_or_else(|| {
        "未找到 ffmpeg,请安装并加入 PATH,或用环境变量 CLAW_FFMPEG 指定路径".to_string()
    })?;
    let devices = list_input_devices(&ffmpeg)?;
    let device = pick_input_device(&devices)?;
    let wav_dir = temp_voice_dir()?;
    let wav = wav_dir.join(format!("voice-{}.wav", unique_suffix()));

    record_audio(&ffmpeg, &device, seconds, &wav)?;

    progress("录音完成,本地识别中…");
    let text = transcribe(&skill_root, &wav, &venv_py)?;
    let _ = std::fs::remove_file(&wav);

    let display = display_text(&text);
    emit_draft(display.clone());
    progress(&format!("✅ 识别完成:{display}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_seconds_defaults_when_empty() {
        assert_eq!(parse_seconds(None).unwrap(), 5);
        assert_eq!(parse_seconds(Some("")).unwrap(), 5);
        assert_eq!(parse_seconds(Some("  ")).unwrap(), 5);
    }

    #[test]
    fn parse_seconds_accepts_legacy_on_off() {
        assert_eq!(parse_seconds(Some("on")).unwrap(), 5);
        assert_eq!(parse_seconds(Some("off")).unwrap(), 5);
    }

    #[test]
    fn parse_seconds_valid_range() {
        assert_eq!(parse_seconds(Some("1")).unwrap(), 1);
        assert_eq!(parse_seconds(Some("8")).unwrap(), 8);
        assert_eq!(parse_seconds(Some("60")).unwrap(), 60);
    }

    #[test]
    fn parse_seconds_rejects_bad_input() {
        assert!(parse_seconds(Some("abc")).is_err());
        assert!(parse_seconds(Some("0")).is_err());
        assert!(parse_seconds(Some("61")).is_err());
    }

    #[test]
    fn parse_dshow_devices_collects_only_audio_devices() {
        // 旧版 ffmpeg(≤7)带 "DirectShow audio devices" 标题
        let sample = r#"[dshow @ 000] DirectShow video devices (some may be both video and audio devices)
[dshow @ 000]  "Integrated Camera"
[dshow @ 000] DirectShow audio devices
[dshow @ 000]  "Microphone Array (Realtek(R) Audio)"
[dshow @ 000]  "Stereo Mix (Realtek(R) Audio)"
"#;
        let devices = parse_dshow_devices(sample);
        assert_eq!(
            devices,
            vec![
                "Microphone Array (Realtek(R) Audio)".to_string(),
                "Stereo Mix (Realtek(R) Audio)".to_string()
            ]
        );
    }

    #[test]
    fn parse_dshow_devices_handles_ffmpeg8_inline_markers() {
        // ffmpeg 8.x:无标题,设备行带 (video)/(audio),且含 Alternative name 行
        let sample = r#"[in#0 @ 0002] "Integrated Camera" (video)
[in#0 @ 0002]   Alternative name "@device_pnp_\\?\usb#vid_30c9&pid_00ec&mi_00#global"
[in#0 @ 0002] "麦克风阵列 (适用于数字麦克风的英特尔® 智音技术)" (audio)
[in#0 @ 0002]   Alternative name "@device_cm_{33D9A762}"
[in#0 @ 0002] "Stereo Mix" (audio)
Error opening input file dummy.
"#;
        let devices = parse_dshow_devices(sample);
        assert_eq!(
            devices,
            vec![
                "麦克风阵列 (适用于数字麦克风的英特尔® 智音技术)".to_string(),
                "Stereo Mix".to_string()
            ]
        );
    }

    #[test]
    fn parse_dshow_devices_ignores_quotes_outside_audio_section() {
        let sample = "some preamble \"Not A Device\"\nDirectShow audio devices\n  \"Real Mic\"\n";
        let devices = parse_dshow_devices(sample);
        assert_eq!(devices, vec!["Real Mic".to_string()]);
    }

    #[test]
    fn pick_input_device_prefers_microphone() {
        let devices = vec!["Stereo Mix".to_string(), "Microphone Array".to_string()];
        assert_eq!(pick_input_device(&devices).unwrap(), "Microphone Array");
        // 无麦克风时退回第一个
        let devices = vec!["Stereo Mix".to_string()];
        assert_eq!(pick_input_device(&devices).unwrap(), "Stereo Mix");
    }

    #[test]
    fn extract_result_text_parses_single_result_json() {
        let stdout = "some log line\n=== RESULT ===\n{\n  \"text\": \"今天的会议内容\",\n  \"language\": \"Chinese\",\n  \"device\": \"GPU.0\"\n}\n\nTotal time: 3.8s\n";
        assert_eq!(extract_result_text(stdout).unwrap(), "今天的会议内容");
    }

    #[test]
    fn extract_result_text_handles_multiple_results_block() {
        let stdout = "=== RESULT 1/2 ===\n{\"text\": \"first\"}\n=== RESULT 2/2 ===\n{\"text\": \"second\"}\n";
        // 只取第一个 RESULT 块的 text
        assert_eq!(extract_result_text(stdout).unwrap(), "first");
    }

    #[test]
    fn extract_result_text_rejects_missing_marker_or_text() {
        assert!(extract_result_text("nothing here").is_err());
        assert!(extract_result_text("=== RESULT ===\n{\"language\": \"Chinese\"}\n").is_err());
        assert!(extract_result_text("=== RESULT ===\n{\"text\": \"   \"}\n").is_err());
    }

    #[test]
    fn brace_balanced_ignores_string_contents() {
        let input = r#"{"text": "} not closing", "nested": {"a": 1}}"#;
        assert_eq!(brace_balanced(input), Some(input));
        assert_eq!(brace_balanced("{not closed"), None);
    }

    #[test]
    fn display_text_collapses_newlines() {
        assert_eq!(display_text("hello\nworld\n"), "hello world");
        assert_eq!(display_text("  spaced  \n\nnext  "), "spaced next");
    }

    #[test]
    fn model_ready_requires_all_required_files() {
        let dir = temp_test_dir("voice-model-ready");
        let model = dir.join("Model");
        std::fs::create_dir_all(&model).expect("create model dir");
        std::fs::write(model.join("a.bin"), b"x").expect("write a");
        let required = vec!["a.bin".to_string(), "b.bin".to_string()];
        // 缺 b.bin → 未就绪
        assert!(!model_ready_in(&model, &required));
        std::fs::write(model.join("b.bin"), b"y").expect("write b");
        assert!(model_ready_in(&model, &required));
        // required 为空 → 视为就绪
        assert!(model_ready_in(&model, &[]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dir_size_mb_counts_recursively() {
        let dir = temp_test_dir("voice-dir-size");
        std::fs::create_dir_all(dir.join("sub")).expect("sub dir");
        std::fs::write(dir.join("f1"), vec![0u8; 1024 * 1024 + 1]).expect("f1");
        std::fs::write(dir.join("sub/f2"), vec![0u8; 1024 * 1024]).expect("f2");
        // (1MB + 1B + 1MB) / 1MB = 2
        assert_eq!(dir_size_mb(&dir), 2);
        assert_eq!(dir_size_mb(&dir.join("missing")), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn asr_info_parsing_reads_first_model() {
        let dir = temp_test_dir("voice-info");
        std::fs::write(
            dir.join("info.json"),
            r#"{
  "venv_name": "asr-cu",
  "models": [
    {
      "model_id": "snake7gun/Qwen3-ASR-0.6B-fp16-ov",
      "dir_name": "Qwen3-ASR-0.6B-fp16-ov",
      "required_files": ["config.json", "thinker/a.bin", "thinker/b.bin"]
    }
  ]
}"#,
        )
        .expect("write info.json");
        assert_eq!(asr_model_dir_name(&dir).unwrap(), "Qwen3-ASR-0.6B-fp16-ov");
        let required = asr_required_files(&dir).unwrap();
        assert_eq!(
            required,
            vec!["config.json", "thinker/a.bin", "thinker/b.bin"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("claw-voice-test-{label}-{}", unique_suffix()));
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }
}
