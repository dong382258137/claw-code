//! Windows desktop control via cua-driver's `call` CLI.
//!
//! Architecture — mirrors `browser_control`'s perception → action → verify
//! loop, backed by cua-driver (Windows UIA accessibility tree + background
//! input injection, no focus stealing):
//!
//! 1. **Perception layer**
//!    - `snapshot` calls `get_window_state(pid, window_id)`, returning the UIA
//!      element tree (`elements[]`, each with a `element_token` like
//!      `s00000001:0`) plus a window screenshot. The model addresses elements
//!      by the `ref` field (= element_token).
//!    - `desktop` returns a full-screen screenshot for coordinate-based
//!      desktop-scope work.
//!    - `list_windows` / `list_apps` discover the target pid + window_id.
//! 2. **Execution layer** — actions address elements by `ref` (element_token,
//!    UIA Invoke path, background, stable) or window-local `x`/`y` pixels.
//!    `click` / `double_click` / `right_click` / `type_text` / `press_key` /
//!    `hotkey` / `scroll` / `drag` / `set_value` / `launch_app` / `verify`.
//! 3. **Verification loop** — every action echoes the cua-driver result; the
//!    model confirms with `snapshot` again or `verify` (bounded postconditions).
//!    Screenshots are saved to `<cwd>/.claw/windows_shots/<ts>.png` and the
//!    JSON base64 blob is replaced with the file path (keeps context small).
//!
//! Transport: one-shot `cua-driver call <tool> --json` per action (~1.2s,
//! process spawn + daemon handshake). The daemon (autostart scheduled task
//! `cua-driver-serve`) owns all UI session state (window snapshots, element
//! tokens), so calls are stateless. If the daemon is not running, the driver
//! is started in the background and the call retried once.
//!
//! INVARIANT: call `snapshot(pid, window_id)` once per turn before any element
//! action against that window — a new snapshot invalidates previous element
//! tokens.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Input schema for the `windows_control` tool.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WindowsControlInput {
    /// One of: list_windows | list_apps | launch_app | snapshot | desktop |
    ///         click | double_click | right_click | type_text | press_key |
    ///         hotkey | scroll | drag | set_value | verify | cdp_open
    pub action: String,
    /// Process id of the target window's app (required by snapshot/click/…).
    pub pid: Option<u64>,
    /// Window handle id returned by `list_windows` (required by snapshot).
    pub window_id: Option<u64>,
    /// Element reference (cua-driver `element_token`, e.g. "s00000001:0") from
    /// the last `snapshot`. Preferred over `x`/`y`.
    #[serde(rename = "ref", alias = "element_token")]
    pub ref_: Option<String>,
    /// Window-local screenshot pixels (fallback addressing when UIA cannot
    /// cover the surface, e.g. canvas/WebGL/self-drawn controls).
    pub x: Option<u64>,
    pub y: Option<u64>,
    /// Text typed by `type_text`.
    pub text: Option<String>,
    /// Single key for `press_key` (return/tab/escape/…/f1-f12/alnum).
    pub key: Option<String>,
    /// Combination keys for `hotkey` (e.g. ["ctrl", "c"]).
    pub keys: Option<Vec<String>>,
    /// Modifier keys for `press_key`.
    pub modifiers: Option<Vec<String>>,
    /// Mouse button for `click`: left | right | middle.
    pub button: Option<String>,
    /// Click count for `click`: 1 | 2 | 3.
    pub count: Option<u64>,
    /// Scroll direction: up | down | left | right (required by `scroll`).
    pub direction: Option<String>,
    /// Scroll amount (`amount`) and unit (`by`: line | page) for `scroll`.
    pub amount: Option<u64>,
    pub by: Option<String>,
    /// Value for `set_value`.
    pub value: Option<String>,
    /// Drag endpoints (required by `drag`), window-local pixels.
    pub from_x: Option<u64>,
    pub from_y: Option<u64>,
    pub to_x: Option<u64>,
    pub to_y: Option<u64>,
    /// Keyed expectations for `verify` (array of objects).
    pub expect: Option<Vec<Value>>,
    /// `verify`: number of stable samples; `snapshot`: passed through.
    pub stable_samples: Option<u64>,
    /// App name / path for `launch_app` (name or path required).
    pub name: Option<String>,
    pub path: Option<String>,
    /// `snapshot`: case-insensitive substring projection of the element tree.
    pub query: Option<String>,
    /// `snapshot`: bound the UIA walk (large Electron/web trees).
    pub max_elements: Option<u64>,
    pub max_depth: Option<u64>,
    /// `verify` timeout in ms; default cua-driver side.
    pub timeout_ms: Option<u64>,
    /// `type_text` keystroke delay in ms.
    pub delay_ms: Option<u64>,
    /// `click` delivery: background (default, preferred) | foreground.
    pub delivery_mode: Option<String>,
    /// Output file path for screenshots (snapshot/desktop). Default:
    /// <cwd>/.claw/windows_shots/<timestamp>.png.
    pub save_path: Option<String>,
    /// CDP debug port for `cdp_open`. Default: first free port in 9000-9999.
    pub port: Option<u16>,
}

/// Main entry point used by the tool executor.
pub fn run_windows_control(input: WindowsControlInput) -> Result<String, String> {
    // `cdp_open` is a native action (process + CDP orchestration), not a
    // cua-driver call, so it bypasses the build_call/invoke_tool path.
    if input.action == "cdp_open" {
        return cdp_open(&input);
    }
    let (tool, args) = build_call(&input)?;
    let raw = invoke_tool(&tool, &args)?;
    postprocess(&tool, &input, &raw)
}

/// Map the native action onto a cua-driver tool name + JSON arguments.
/// Only `Some` fields are emitted; missing required fields error out with an
/// actionable message (mirrors browser_control).
fn build_call(input: &WindowsControlInput) -> Result<(String, Value), String> {
    let mut a = Map::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            a.insert(k.to_string(), v);
        }
    };

    let (tool, required): (&str, &[&str]) = match input.action.as_str() {
        "list_windows" => ("list_windows", &[]),
        "list_apps" => ("list_apps", &[]),
        "launch_app" => ("launch_app", &["name-or-path"]),
        "snapshot" => ("get_window_state", &["pid", "window_id"]),
        "desktop" => ("get_desktop_state", &[]),
        "click" => ("click", &["pid"]),
        "double_click" => ("double_click", &["pid"]),
        "right_click" => ("right_click", &["pid"]),
        "type_text" => ("type_text", &["text"]),
        "press_key" => ("press_key", &["key"]),
        "hotkey" => ("hotkey", &["keys"]),
        "scroll" => ("scroll", &["direction"]),
        "drag" => ("drag", &["from_x", "from_y", "to_x", "to_y"]),
        "set_value" => ("set_value", &["pid", "value"]),
        "verify" => ("verify_state", &["pid", "window_id", "expect"]),
        other => {
            return Err(format!(
                "unknown action '{other}' — expected one of: list_windows, list_apps, \
                 launch_app, snapshot, desktop, click, double_click, right_click, \
                 type_text, press_key, hotkey, scroll, drag, set_value, verify"
            ))
        }
    };

    // Required-field validation.
    for key in required {
        let present = match *key {
            "pid" => input.pid.is_some(),
            "window_id" => input.window_id.is_some(),
            "ref_" => input.ref_.is_some(),
            "x" => input.x.is_some(),
            "y" => input.y.is_some(),
            "text" => input.text.is_some(),
            "key" => input.key.is_some(),
            "keys" => input.keys.is_some(),
            "direction" => input.direction.is_some(),
            "value" => input.value.is_some(),
            "from_x" => input.from_x.is_some(),
            "from_y" => input.from_y.is_some(),
            "to_x" => input.to_x.is_some(),
            "to_y" => input.to_y.is_some(),
            "expect" => input.expect.is_some(),
            "name-or-path" => input.name.is_some() || input.path.is_some(),
            _ => true,
        };
        if !present {
            let field = if *key == "name-or-path" {
                "'name' or 'path'"
            } else {
                key
            };
            return Err(format!("action '{}' requires '{}'", input.action, field));
        }
    }

    put("pid", input.pid.map(Value::from));
    put("window_id", input.window_id.map(Value::from));
    put("element_token", input.ref_.as_deref().map(Value::from));
    put("x", input.x.map(Value::from));
    put("y", input.y.map(Value::from));
    put("text", input.text.as_deref().map(Value::from));
    put("key", input.key.as_deref().map(Value::from));
    put("keys", input.keys.as_ref().map(|k| json!(k)));
    put("modifiers", input.modifiers.as_ref().map(|k| json!(k)));
    put("button", input.button.as_deref().map(Value::from));
    put("count", input.count.map(Value::from));
    put("direction", input.direction.as_deref().map(Value::from));
    put("amount", input.amount.map(Value::from));
    put("by", input.by.as_deref().map(Value::from));
    put("value", input.value.as_deref().map(Value::from));
    put("from_x", input.from_x.map(Value::from));
    put("from_y", input.from_y.map(Value::from));
    put("to_x", input.to_x.map(Value::from));
    put("to_y", input.to_y.map(Value::from));
    put("expect", input.expect.as_ref().map(|e| json!(e)));
    put("stable_samples", input.stable_samples.map(Value::from));
    put("name", input.name.as_deref().map(Value::from));
    put("path", input.path.as_deref().map(Value::from));
    put("query", input.query.as_deref().map(Value::from));
    put("max_elements", input.max_elements.map(Value::from));
    put("max_depth", input.max_depth.map(Value::from));
    put("timeout_ms", input.timeout_ms.map(Value::from));
    put("delay_ms", input.delay_ms.map(Value::from));
    put(
        "delivery_mode",
        input.delivery_mode.as_deref().map(Value::from),
    );

    Ok((tool.to_string(), Value::Object(a)))
}

/// Run `cua-driver call <tool> --json` with args piped via stdin, return
/// stdout trimmed. Every call is stamped with a process-lifetime named session
/// so the daemon keeps snapshots/tokens alive across calls (each `cua-driver
/// call` is a fresh transport lease; without a shared session label the daemon
/// forgets `get_window_state` captures as soon as the calling process exits,
/// and `element_token`/`capture_id` come back `stale_element_token` /
/// `capture_not_found`). Auto-starts the daemon and retries once when it is
/// not running.
fn invoke_tool(tool: &str, args: &Value) -> Result<String, String> {
    let exe = resolve_cua_driver()?;
    let session = ensure_session(&exe)?;
    let mut args = args.clone();
    attach_session(&mut args, &session);
    match invoke_once(&exe, tool, &args) {
        Ok(out) => Ok(out),
        Err(err) => {
            if err.contains("daemon is not running") {
                start_daemon(&exe)?;
                std::thread::sleep(Duration::from_millis(1500));
                invoke_once(&exe, tool, &args)
            } else {
                Err(err)
            }
        }
    }
}

/// Process-lifetime named session shared by every cua-driver call.
fn session_name() -> &'static str {
    static SESSION: OnceLock<String> = OnceLock::new();
    SESSION.get_or_init(|| format!("claw-wc-{}", std::process::id()))
}

/// Ensure the named session exists on the daemon (idempotent). Needed once per
/// process; handles a cold daemon by starting it and retrying.
fn ensure_session(exe: &Path) -> Result<String, String> {
    static READY: OnceLock<Mutex<bool>> = OnceLock::new();
    let ready = READY.get_or_init(|| Mutex::new(false));
    {
        let r = ready
            .lock()
            .map_err(|_| "session lock poisoned".to_string())?;
        if *r {
            return Ok(session_name().to_string());
        }
    }
    let name = session_name().to_string();
    let mut last_err = String::new();
    for attempt in 0..2 {
        match invoke_once(exe, "start_session", &json!({ "session": name })) {
            Ok(_) => {
                let mut r = ready
                    .lock()
                    .map_err(|_| "session lock poisoned".to_string())?;
                *r = true;
                return Ok(name);
            }
            Err(e) if attempt == 0 && e.contains("daemon is not running") => {
                last_err = e;
                start_daemon(exe)?;
                std::thread::sleep(Duration::from_millis(1500));
            }
            Err(e) => last_err = e,
        }
    }
    Err(format!(
        "failed to create cua-driver session '{name}': {last_err}"
    ))
}

/// Stamp a session label onto call args (no-op for `start_session`).
fn attach_session(args: &mut Value, session: &str) {
    if let Value::Object(map) = args {
        if !map.contains_key("session") {
            map.insert("session".to_string(), Value::String(session.to_string()));
        }
    }
}

fn invoke_once(exe: &Path, tool: &str, args: &Value) -> Result<String, String> {
    let mut cmd = Command::new(exe);
    cmd.arg("call").arg(tool).arg("--json");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn cua-driver '{}': {e}", exe.display()))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "cua-driver stdin unavailable".to_string())?;
        let payload =
            serde_json::to_string(args).map_err(|e| format!("invalid call args JSON: {e}"))?;
        stdin
            .write_all(payload.as_bytes())
            .map_err(|e| format!("failed to write args to cua-driver: {e}"))?;
        // Drop `stdin` so the child sees EOF and can finish parsing.
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("failed waiting on cua-driver: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        // cua-driver prints errors to stdout as JSON or to stderr as text;
        // surface the most informative part.
        let msg = if !stdout.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        return Err(format!(
            "cua-driver '{}' failed ({}): {msg}",
            tool, out.status
        ));
    }
    Ok(stdout.trim().to_string())
}

/// Start the cua-driver daemon in the background (`cua-driver serve`).
fn start_daemon(exe: &Path) -> Result<(), String> {
    let mut cmd = Command::new(exe);
    cmd.arg("serve");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn()
        .map_err(|e| format!("failed to start cua-driver daemon: {e}"))?;
    Ok(())
}

/// Post-process the raw cua-driver JSON result:
/// - strip the `screenshot_png_b64` blob, save it to
///   `<cwd>/.claw/windows_shots/<ts>.png`, and report `screenshot_path`
///   instead (keeps the model context small, mirroring browser_control).
fn postprocess(tool: &str, input: &WindowsControlInput, raw: &str) -> Result<String, String> {
    if !(tool == "get_window_state" || tool == "get_desktop_state") {
        return Ok(raw.to_string());
    }
    let mut v: Value = serde_json::from_str(raw)
        .map_err(|e| format!("cua-driver returned non-JSON output: {e}"))?;
    if let Value::Object(map) = &mut v {
        if let Some(Value::String(b64)) = map.get("screenshot_png_b64") {
            if !b64.is_empty() {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| format!("failed to decode screenshot base64: {e}"))?;
                let path = resolve_screenshot_path(input.save_path.as_deref())?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
                }
                std::fs::write(&path, &bytes)
                    .map_err(|e| format!("failed to write screenshot {}: {e}", path.display()))?;
                map.remove("screenshot_png_b64");
                map.insert(
                    "screenshot_path".to_string(),
                    Value::String(path.display().to_string()),
                );
            }
        }
    }
    serde_json::to_string(&v).map_err(|e| format!("failed to serialize result: {e}"))
}

fn resolve_screenshot_path(save_path: Option<&str>) -> Result<PathBuf, String> {
    if let Some(p) = save_path {
        let p = p.trim();
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system clock error: {e}"))?
        .as_millis();
    let cwd = std::env::current_dir().map_err(|e| format!("failed to get cwd: {e}"))?;
    Ok(cwd
        .join(".claw")
        .join("windows_shots")
        .join(format!("{ts}.png")))
}

// ---------------------------------------------------------------------------
// CDP penetration (`cdp_open`)
//
// Electron / WebView2 / Chromium apps are "fake desktops": the real UI is a web
// page that UIA cannot see (Obsidian is the reference case — UIA sees no
// window). This action detects such apps and restarts them with a remote
// debugging port so the model can drive the DOM through
// `browser_control connect port=<port>`. Chromium only reads
// `--remote-debugging-port` at process start, so an already-running app must be
// killed and relaunched (WebView2 instead reads the flag from the
// `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` env var at WebView2-environment
// creation). Works on the main-process pid or any renderer/gpu child pid (walks
// up to the root).
// ---------------------------------------------------------------------------

/// CDP target kind, chosen by `detect_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CdpMode {
    Electron,
    Chromium,
    WebView2,
}

impl CdpMode {
    fn as_str(self) -> &'static str {
        match self {
            CdpMode::Electron => "electron",
            CdpMode::Chromium => "chromium",
            CdpMode::WebView2 => "webview2",
        }
    }
}

/// `cdp_open` entry: detect a Chromium-based app and open a CDP debug port.
/// Locate the target with `pid` or `name`; `port` is optional (default: first
/// free port in 9000-9999). Kills and relaunches the app with the debug flag,
/// polls `http://127.0.0.1:<port>/json/version` until ready, then returns the
/// endpoint for `browser_control connect`.
fn cdp_open(input: &WindowsControlInput) -> Result<String, String> {
    if !cfg!(windows) {
        return Err("action 'cdp_open' is only supported on Windows".to_string());
    }
    if input.pid.is_none() && input.name.is_none() {
        return Err(
            "action 'cdp_open' requires 'pid' or 'name' to locate the target app".to_string(),
        );
    }

    // 1. Locate the target process.
    let mut pid = match input.pid {
        Some(p) => p,
        None => {
            let name = input.name.as_deref().unwrap_or_default();
            match find_pid_by_name(&normalize_exe_name(name))? {
                Some(p) => p,
                None => {
                    return Err(format!(
                        "no running process named '{name}' — start it first \
                         (launch_app) or pass its pid"
                    ))
                }
            }
        }
    };
    // A renderer/gpu child belongs to the app too; restart must target the root.
    pid = walk_to_root(pid)?;

    // 2. Detect the CDP backend.
    let exe = ps_exe_path(pid)?;
    if exe.trim().is_empty() {
        return Err(format!(
            "process {pid} not found or has no readable executable path \
             (elevated apps are invisible to a non-elevated session)"
        ));
    }
    let mode = detect_mode(Path::new(&exe), pid)?;

    // 3. Pick the debug port.
    let port = match input.port {
        Some(p) => p,
        None => find_free_port(9000..=9999)
            .ok_or_else(|| "no free port found in 9000-9999".to_string())?,
    };

    // 4. Kill the tree, wait for it to die, relaunch with the debug flag.
    kill_tree(pid)?;
    wait_process_gone(pid, Duration::from_secs(3));
    let orig_cmdline = ps_cmdline(pid).unwrap_or_default();
    relaunch_with_debug(Path::new(&exe), &orig_cmdline, port, mode)?;

    // 5. Poll the CDP version endpoint until the debug server answers.
    let version = wait_for_cdp(port, Duration::from_secs(12))?;
    let browser = version
        .get("Browser")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let ws_url = version
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let app = Path::new(&exe)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("")
        .to_string();

    Ok(json!({
        "status": "cdp_open",
        "mode": mode.as_str(),
        "app": app,
        "pid": pid,
        "exe": exe,
        "port": port,
        "endpoint": format!("http://127.0.0.1:{port}/json/version"),
        "ws_url": ws_url,
        "browser": browser,
        "next": format!("drive the app DOM with browser_control action 'connect' port={port}")
    })
    .to_string())
}

/// Run a PowerShell one-liner and return trimmed stdout ("" when the command
/// produced no output).
fn ps_capture(script: &str) -> Result<String, String> {
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to run powershell: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Find the process id of an app by image name (e.g. "Obsidian.exe").
/// Multi-process Chromium apps have many processes sharing the name; prefer the
/// main process (no `--type=` in its command line), fall back to the first.
/// Returns Ok(None) when the app is not running.
fn find_pid_by_name(exe_name: &str) -> Result<Option<u64>, String> {
    let script = format!(
        "$ps = @(Get-CimInstance Win32_Process -Filter \"Name = '{exe_name}'\"); \
         $main = $ps | Where-Object {{ $_.CommandLine -notlike '*--type=*' }} | \
         Select-Object -First 1; \
         if ($main) {{ $main.ProcessId }} \
         elseif ($ps) {{ $ps[0].ProcessId }}"
    );
    let out = ps_capture(&script)?;
    if out.is_empty() {
        return Ok(None);
    }
    out.parse::<u64>()
        .map(Some)
        .map_err(|e| format!("unexpected powershell output '{out}': {e}"))
}

/// Executable path of a process, or "" when the process is gone.
fn ps_exe_path(pid: u64) -> Result<String, String> {
    let script = format!(
        "$p = Get-CimInstance Win32_Process -Filter \"ProcessId = {pid}\"; \
         if ($p) {{ $p.ExecutablePath }}"
    );
    ps_capture(&script)
}

/// Command line of a process, or "" when the process is gone.
fn ps_cmdline(pid: u64) -> Result<String, String> {
    let script = format!(
        "$p = Get-CimInstance Win32_Process -Filter \"ProcessId = {pid}\"; \
         if ($p) {{ $p.CommandLine }}"
    );
    ps_capture(&script)
}

/// Parent process id of a process (0 when gone).
fn ps_parent(pid: u64) -> Result<u64, String> {
    let script = format!(
        "$p = Get-CimInstance Win32_Process -Filter \"ProcessId = {pid}\"; \
         if ($p) {{ $p.ParentProcessId }}"
    );
    let out = ps_capture(&script)?;
    out.parse::<u64>()
        .map_err(|e| format!("unexpected powershell output '{out}': {e}"))
}

/// Walk up from a Chromium child process (`--type=renderer/gpu/…`) to the app's
/// main process, so kill + restart targets the real root.
fn walk_to_root(mut pid: u64) -> Result<u64, String> {
    for _ in 0..8 {
        let cmdline = ps_cmdline(pid).unwrap_or_default();
        if !cmdline.contains("--type=") {
            break;
        }
        let parent = ps_parent(pid).unwrap_or(0);
        if parent == 0 || parent == pid {
            break;
        }
        pid = parent;
    }
    Ok(pid)
}

/// Classify the target as Electron / plain Chromium / WebView2.
fn detect_mode(exe: &Path, pid: u64) -> Result<CdpMode, String> {
    // Electron: an `app.asar` next to the executable is the definitive marker.
    let app_asar = exe.parent().map(|d| d.join("resources").join("app.asar"));
    if app_asar.as_deref().map(|p| p.exists()).unwrap_or(false) {
        return Ok(CdpMode::Electron);
    }
    // WebView2: the host app spawns msedgewebview2.exe children.
    let script = format!(
        "Get-CimInstance Win32_Process -Filter \"ParentProcessId = {pid}\" | \
         Where-Object {{ $_.Name -like 'msedgewebview2*' }} | Select-Object -First 1 | \
         ForEach-Object {{ $_.Name }}"
    );
    if !ps_capture(&script)?.is_empty() {
        return Ok(CdpMode::WebView2);
    }
    // Plain Chromium main process (Chrome/Edge/Brave/…).
    let name = exe
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if [
        "chrome.exe",
        "msedge.exe",
        "chromium.exe",
        "brave.exe",
        "vivaldi.exe",
        "opera.exe",
    ]
    .contains(&name.as_str())
    {
        return Ok(CdpMode::Chromium);
    }
    Err(format!(
        "{} does not look like an Electron/Chromium/WebView2 app (no resources\\app.asar, \
         no msedgewebview2.exe children, exe not a Chromium browser). CDP penetration only \
         applies to Chromium-based 'fake desktop' apps; native Win32/WPF apps should keep \
         using snapshot/click instead.",
        exe.display()
    ))
}

/// Kill a process and its whole child tree (`taskkill /PID <pid> /T /F`).
fn kill_tree(pid: u64) -> Result<(), String> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("failed to run taskkill: {e}"))?;
    // taskkill exit 128 = process already gone; treat as success either way —
    // the relaunch below is what matters.
    let _ = status;
    Ok(())
}

/// Wait until a pid no longer has an executable path (i.e. is gone).
fn wait_process_gone(pid: u64, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ps_exe_path(pid)
            .map(|p| p.trim().is_empty())
            .unwrap_or(true)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Relaunch the app with `--remote-debugging-port=<port>` (WebView2 via the
/// `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` env var, read at environment
/// creation). Preserves the original command-line arguments.
fn relaunch_with_debug(
    exe: &Path,
    orig_cmdline: &str,
    port: u16,
    mode: CdpMode,
) -> Result<(), String> {
    let mut cmd = Command::new(exe);
    cmd.args(launch_args(exe, orig_cmdline, port));
    if mode == CdpMode::WebView2 {
        cmd.env(
            "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
            format!("--remote-debugging-port={port}"),
        );
    }
    cmd.spawn().map_err(|e| {
        format!(
            "failed to relaunch {} with --remote-debugging-port={port}: {e} \
             (the app was stopped; relaunch it manually, e.g. `{} --remote-debugging-port={port}`)",
            exe.display(),
            exe.display(),
        )
    })?;
    Ok(())
}

/// Build the relaunch argument list: the original command line minus the exe
/// token and any stale debug-port flag, plus our `--remote-debugging-port`.
fn launch_args(exe: &Path, orig_cmdline: &str, port: u16) -> Vec<String> {
    let mut args = split_cmdline(orig_cmdline);
    if let Some(first) = args.first() {
        let first = first.trim_matches('"');
        let same_exe = first.eq_ignore_ascii_case(&exe.display().to_string())
            || Path::new(first)
                .file_name()
                .and_then(|f| f.to_str())
                .map(|f| f.to_ascii_lowercase())
                == exe
                    .file_name()
                    .and_then(|f| f.to_str())
                    .map(|f| f.to_ascii_lowercase());
        if same_exe {
            args.remove(0);
        }
    }
    args.retain(|a| !a.starts_with("--remote-debugging-port"));
    args.push(format!("--remote-debugging-port={port}"));
    args
}

/// Split a Windows command line on whitespace, respecting double quotes.
fn split_cmdline(cmdline: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for c in cmdline.chars() {
        match c {
            '"' => in_quote = !in_quote,
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    args.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        args.push(cur);
    }
    args
}

/// Normalize an app name to `<name>.exe` (case-insensitive suffix check).
fn normalize_exe_name(name: &str) -> String {
    let n = name.trim();
    if n.to_ascii_lowercase().ends_with(".exe") {
        n.to_string()
    } else {
        format!("{n}.exe")
    }
}

/// First free port in the range (bind probe). Small TOCTOU window between the
/// probe and the app binding it is acceptable.
fn find_free_port(mut range: std::ops::RangeInclusive<u16>) -> Option<u16> {
    range.find(|&port| std::net::TcpListener::bind(("127.0.0.1", port)).is_ok())
}

/// Poll `http://127.0.0.1:<port>/json/version` until the CDP server answers.
fn wait_for_cdp(port: u16, timeout: Duration) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match cdp_version(port) {
            Ok(v) => return Ok(v),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(300));
            }
            Err(_) => break,
        }
    }
    let url = format!("http://127.0.0.1:{port}/json/version");
    cdp_version(port).map_err(|e| format!("CDP endpoint {url} not ready within {timeout:?}: {e}"))
}

/// Fetch the CDP `/json/version` payload for a port.
fn cdp_version(port: u16) -> Result<Value, String> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let resp = reqwest::blocking::get(&url).map_err(|e| format!("{url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url}: HTTP {}", resp.status()));
    }
    resp.json::<Value>()
        .map_err(|e| format!("{url}: bad JSON: {e}"))
}

/// Resolve the cua-driver executable, cached for the process lifetime.
/// Order: `CLAW_CUA_DRIVER` env → default install path → PATH lookup.
fn resolve_cua_driver() -> Result<PathBuf, String> {
    static EXE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    EXE.get_or_init(|| {
        if let Ok(p) = std::env::var("CLAW_CUA_DRIVER") {
            let p = p.trim().to_string();
            if !p.is_empty() {
                return Ok(PathBuf::from(p));
            }
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let candidate = PathBuf::from(local)
                .join("Programs")
                .join("Cua")
                .join("cua-driver")
                .join("bin")
                .join("cua-driver.exe");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            let candidate = dir.join("cua-driver.exe");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        Err("cua-driver executable not found. Install it with: \
             irm https://cua.ai/driver/install.ps1 | iex \
             (or set CLAW_CUA_DRIVER to the cua-driver.exe path)"
            .to_string())
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(action: &str) -> WindowsControlInput {
        // Helper: every field None except action.
        let mut v = WindowsControlInput {
            action: action.to_string(),
            pid: None,
            window_id: None,
            ref_: None,
            x: None,
            y: None,
            text: None,
            key: None,
            keys: None,
            modifiers: None,
            button: None,
            count: None,
            direction: None,
            amount: None,
            by: None,
            value: None,
            from_x: None,
            from_y: None,
            to_x: None,
            to_y: None,
            expect: None,
            stable_samples: None,
            name: None,
            path: None,
            query: None,
            max_elements: None,
            max_depth: None,
            timeout_ms: None,
            delay_ms: None,
            delivery_mode: None,
            save_path: None,
            port: None,
        };
        v.action = action.to_string();
        v
    }

    #[test]
    fn unknown_action_errors() {
        let e = build_call(&input("nope")).unwrap_err();
        assert!(e.contains("unknown action 'nope'"));
    }

    #[test]
    fn snapshot_requires_pid_and_window_id() {
        let e = build_call(&input("snapshot")).unwrap_err();
        assert!(e.contains("requires 'pid'"));
        let mut i = input("snapshot");
        i.pid = Some(1);
        let e = build_call(&i).unwrap_err();
        assert!(e.contains("requires 'window_id'"));
    }

    #[test]
    fn click_maps_ref_to_element_token_and_requires_pid() {
        let e = build_call(&input("click")).unwrap_err();
        assert!(e.contains("requires 'pid'"));
        let mut i = input("click");
        i.pid = Some(42);
        i.ref_ = Some("s00000001:3".to_string());
        i.button = Some("right".to_string());
        let (tool, args) = build_call(&i).unwrap();
        assert_eq!(tool, "click");
        assert_eq!(args["pid"], 42);
        assert_eq!(args["element_token"], "s00000001:3");
        assert_eq!(args["button"], "right");
        // ref_ must not leak as its own key
        assert!(args.get("ref").is_none());
    }

    #[test]
    fn scroll_requires_direction() {
        let e = build_call(&input("scroll")).unwrap_err();
        assert!(e.contains("requires 'direction'"));
    }

    #[test]
    fn hotkey_requires_keys() {
        let e = build_call(&input("hotkey")).unwrap_err();
        assert!(e.contains("requires 'keys'"));
    }

    #[test]
    fn launch_app_requires_name_or_path() {
        let e = build_call(&input("launch_app")).unwrap_err();
        assert!(e.contains("'name' or 'path'"));
    }

    #[test]
    fn postprocess_strips_screenshot_b64() {
        let raw = json!({
            "app_name": "notepad.exe",
            "screenshot_png_b64": "aGVsbG8="  // "hello"
        })
        .to_string();
        let out = postprocess("get_window_state", &input("snapshot"), &raw).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("screenshot_png_b64").is_none());
        assert!(v["screenshot_path"].as_str().unwrap().ends_with(".png"));
        assert_eq!(v["app_name"], "notepad.exe");
        // The written file contains the decoded bytes.
        let path = v["screenshot_path"].as_str().unwrap();
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(bytes, b"hello");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn postprocess_passthrough_for_actions() {
        let raw = json!({"status": "clicked"}).to_string();
        let out = postprocess("click", &input("click"), &raw).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn attach_session_stamps_label_without_overriding() {
        let mut v = json!({"pid": 42});
        attach_session(&mut v, "claw-wc-123");
        assert_eq!(v["session"], "claw-wc-123");
        assert_eq!(v["pid"], 42);

        // An existing session key (e.g. a caller-provided label) wins.
        let mut v = json!({"session": "mine"});
        attach_session(&mut v, "claw-wc-123");
        assert_eq!(v["session"], "mine");
    }

    // ------------------------------------------------------------------
    // cdp_open pure helpers
    // ------------------------------------------------------------------

    #[test]
    fn split_cmdline_respects_quotes() {
        let args = split_cmdline(
            r#""C:\Program Files\Obsidian\Obsidian.exe" --disable-gpu --flag="a b c""#,
        );
        assert_eq!(
            args,
            vec![
                "C:\\Program Files\\Obsidian\\Obsidian.exe",
                "--disable-gpu",
                "--flag=a b c",
            ]
        );
        // Bare exe (no quotes), empty input.
        assert_eq!(split_cmdline("notepad.exe"), vec!["notepad.exe"]);
        assert!(split_cmdline("  ").is_empty());
    }

    #[test]
    fn launch_args_drops_exe_and_stale_port_then_appends_debug_flag() {
        let exe = Path::new(r"C:\Users\me\AppData\Local\Programs\Obsidian\Obsidian.exe");
        let cmdline = format!(
            r#""{}" --remote-debugging-port=9222 --disable-gpu"#,
            exe.display()
        );
        let args = launch_args(exe, &cmdline, 9001);
        // exe token and stale port removed; our flag appended; other args kept.
        assert_eq!(args, vec!["--disable-gpu", "--remote-debugging-port=9001"]);
        // Case-insensitive exe match (cmdline may quote differently).
        let args = launch_args(exe, "obsidian.EXE --foo", 9002);
        assert_eq!(args, vec!["--foo", "--remote-debugging-port=9002"]);
        // Empty cmdline → just the flag.
        let args = launch_args(exe, "", 9003);
        assert_eq!(args, vec!["--remote-debugging-port=9003"]);
    }

    #[test]
    fn normalize_exe_name_appends_extension() {
        assert_eq!(normalize_exe_name("Obsidian"), "Obsidian.exe");
        assert_eq!(normalize_exe_name("Obsidian.exe"), "Obsidian.exe");
        assert_eq!(normalize_exe_name("  chrome.EXE  "), "chrome.EXE");
    }

    #[test]
    fn cdp_open_requires_pid_or_name() {
        // No pid/name → fails fast before any process interaction (safe on all
        // platforms, including Windows where pid=… would touch real processes).
        let err = cdp_open(&input("cdp_open")).unwrap_err();
        assert!(err.contains("requires 'pid' or 'name'"));
    }

    #[test]
    fn find_free_port_returns_bindable_port() {
        let port = find_free_port(9000..=9999).expect("a free port must exist");
        // The port must be bindable (it was probed as free).
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        drop(listener);
    }

    #[test]
    fn cdp_mode_labels() {
        assert_eq!(CdpMode::Electron.as_str(), "electron");
        assert_eq!(CdpMode::Chromium.as_str(), "chromium");
        assert_eq!(CdpMode::WebView2.as_str(), "webview2");
    }
}
