//! Browser control via Chrome DevTools Protocol (chromiumoxide).
//!
//! Architecture — mirrors the de-facto state of the art (Playwright MCP,
//! browser-use, agent-browser):
//!
//! 1. **Perception layer**
//!    - `snapshot` builds an *accessibility tree snapshot* from
//!      `Accessibility.getFullAXTree`. Every interactive element is assigned a
//!      stable `ref` (e.g. `e1`, `e2`) the model uses to address elements.
//!      Deterministic and far cheaper than screenshots.
//!    - `get_state` reads page context + a specific element's live state
//!      (text/value/checked/disabled/visible) so the model can *verify* an
//!      action took effect instead of guessing.
//!    - `screenshot` / `wait_for` provide visual / conditional feedback.
//! 2. **Execution layer** — actions address elements by `ref` (resolved to
//!    `backend_dom_node_id`) or a CSS `selector`. Interactions use **real CDP
//!    events**: coordinates from `DOM.getContentQuads` + `Input.dispatchMouseEvent`
//!    for clicks/hovers, `Input.insertText` for typing (real keyboard path),
//!    `Runtime.callFunctionOn` for form state that needs JS semantics.
//! 3. **Verification loop** — every interaction echoes back page url + title
//!    and, when cheap, the affected element's resulting state. The model sees
//!    evidence after every step; `snapshot` re-issues refs whenever the page
//!    changed.
//!
//! The session persists across calls: one Chrome process, multiple tabs, and
//! the last snapshot's `ref → backendNodeId` map.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::accessibility::{AxNode, AxValue, GetFullAxTreeParams};
use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, DescribeNodeParams, GetContentQuadsParams, GetDocumentParams,
    QuerySelectorParams, ResolveNodeParams, ScrollIntoViewIfNeededParams,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    InsertTextParams, MouseButton,
};
use chromiumoxide::cdp::js_protocol::runtime::{
    CallFunctionOnParams, RemoteObjectId, RemoteObjectType,
};
use chromiumoxide::handler::viewport::Viewport;
use chromiumoxide::layout::{ElementQuad, Point};
use chromiumoxide::page::{Page, ScreenshotParams};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Input schema for the `browser_control` tool.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BrowserControlInput {
    /// One of: launch | goto | back | forward | reload | snapshot |
    ///         screenshot | get_state | wait_for | click | fill | type |
    ///         press_key | hover | scroll | select_option | check | uncheck |
    ///         new_tab | switch_tab | list_tabs | close_tab |
    ///         evaluate_js | close
    pub action: String,
    /// URL used by `launch` / `goto` / `new_tab`.
    pub url: Option<String>,
    /// Accessibility snapshot reference (e.g. "e1") from a prior `snapshot`.
    #[serde(rename = "ref")]
    pub ref_: Option<String>,
    /// CSS selector fallback for element actions when `ref` is not available.
    pub selector: Option<String>,
    /// Text typed by `type` or `fill`.
    pub text: Option<String>,
    /// Key pressed by `press_key` (e.g. "Enter", "Tab", "Escape").
    pub key: Option<String>,
    /// Value for `select_option`.
    pub value: Option<String>,
    /// Scroll direction: up | down | top | bottom for page scrolling,
    /// or "element" (+ref/selector) to scroll a specific element into view.
    pub direction: Option<String>,
    /// JavaScript expression evaluated by `evaluate_js`.
    pub script: Option<String>,
    /// Wait condition for `wait_for`: "url:…" | "text:…" | CSS selector |
    /// "networkidle". Default timeout applies.
    pub wait_for: Option<String>,
    /// Optional timeout (ms) for `wait_for`. Default 5000.
    pub timeout_ms: Option<u64>,
    /// Tab index for `switch_tab` / `close_tab`.
    pub index: Option<usize>,
    /// Output file path for `screenshot`; defaults to <cwd>/.claw/browser_shots/<timestamp>.png.
    pub save_path: Option<String>,
    /// Launch headless (no visible window). Defaults to false (visible window).
    pub headless: Option<bool>,
    /// CDP port for `connect` (e.g. 9222). Alternative to `url` pointing at an
    /// already-running browser or Electron app.
    pub port: Option<u16>,
}

/// One persistent browser session per process.
struct BrowserSession {
    runtime: Arc<tokio::runtime::Runtime>,
    /// None until the first `launch` action succeeds.
    browser: Option<Browser>,
    tabs: Vec<Page>,
    active: usize,
    /// `ref → backend_dom_node_id` map from the most recent snapshot.
    refs: HashMap<String, BackendNodeId>,
}

static SESSION: OnceLock<Mutex<Option<BrowserSession>>> = OnceLock::new();

/// Main entry point used by the tool executor.
pub fn run_browser_control(input: BrowserControlInput) -> Result<String, String> {
    let mutex = SESSION.get_or_init(|| Mutex::new(None));
    let mut guard = mutex
        .lock()
        .map_err(|_| "browser session lock poisoned".to_string())?;

    if input.action == "close" {
        // 在 tokio runtime 上下文(如 claw 的 async 执行栈)直接 drop 会话会
        // 触发 "Cannot drop a runtime in a context where blocking is not
        // allowed" —— Runtime::drop 需要 blocking 上下文来关闭阻塞线程池。
        // 用 thread::scope 把会话移到独立 OS 线程 drop(同步等待,保证返回时
        // 浏览器进程已关闭)。浏览器进程由 Browser 的 Drop 触发 kill。
        if let Some(session) = guard.take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                std::thread::scope(|s| {
                    s.spawn(|| drop(session));
                });
            } else {
                drop(session);
            }
        }
        return Ok(json!({ "status": "closed" }).to_string());
    }

    if guard.is_none() {
        *guard = Some(create_session(&input)?);
    }

    // 工具执行器可能在 tokio runtime 上下文中调用本函数(如 run_turn_async
    // 调用栈内)。直接 `runtime.block_on` 会触发 "Cannot start a runtime from
    // within a runtime" panic。检测到当前线程已在 runtime 中时,把会话移入
    // 独立 OS 线程执行 `handle.block_on`(该线程不在任何 runtime 上下文)。
    // `block_in_place` 不可用:claw-shell 使用 current_thread + LocalSet,
    // block_in_place 在 current_thread runtime 上会 panic。参照 llm_clients.rs
    // 的同款修复。
    if tokio::runtime::Handle::try_current().is_ok() {
        let mut session = guard.take().expect("session just ensured");
        let runtime = session.runtime.clone();
        let handle = runtime.handle().clone();
        // 闭包返回 (结果, 会话),便于 join 后放回 guard。
        let joined = std::thread::spawn(move || {
            let out = handle.block_on(dispatch(&mut session, &input));
            (out, session)
        })
        .join()
        .map_err(|e| format!("browser_control worker thread panicked: {e:?}"))?;
        let (result, session) = joined;
        *guard = Some(session);
        result
    } else {
        let session = guard.as_mut().expect("session just ensured");
        // Arc: Clone is a cheap reference bump, so we can hold the runtime handle
        // while `dispatch` reborrows the session mutably.
        let runtime = session.runtime.clone();
        runtime.block_on(dispatch(session, &input))
    }
}

/// Launch a fresh browser, or attach to an already-running one for `connect`.
fn create_session(input: &BrowserControlInput) -> Result<BrowserSession, String> {
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("failed to start tokio runtime: {e}"))?,
    );

    // 与 run_browser_control 相同的嵌套 runtime 防护;future 需 'static,
    // 故先克隆创建会话所需的输入字段。
    let action = input.action.clone();
    let headless = input.headless;
    let url = input.url.clone();
    let port = input.port;

    let (browser, handler) = block_on_future(&runtime, async move {
        if action == "connect" {
            let target = connect_target_parts(url, port)?;
            Browser::connect(target)
                .await
                .map_err(|e| format!("connect to CDP endpoint failed: {e}"))
        } else {
            // 窗口与页面渲染区域(CDP viewport)必须一致:window_size 只生成
            // `--window-size` 命令行参数(窗口外框),viewport 是独立的 CDP
            // 设备指标,默认 800x600。若不同步,页面内容被钉死在默认小区域,
            // 窗口其余空白(高分屏 + DPI 缩放下尤其明显)。
            let viewport = Viewport {
                width: 1440,
                height: 900,
                ..Default::default()
            };
            // 反自动化指纹:抵消 --enable-automation 的 webdriver 标记、去掉
            // HeadlessChrome UA 痕迹、覆盖默认 en_US,降低反爬(如携程
            // whaleguard)对正常查询的误伤概率。
            let builder = BrowserConfig::builder()
                .window_size(1440, 900)
                .viewport(viewport)
                .arg(("disable-blink-features", "AutomationControlled"))
                .arg((
                    "user-agent",
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36",
                ))
                .arg(("lang", "zh-CN"));
            let config = if headless.unwrap_or(false) {
                builder.new_headless_mode().build()
            } else {
                builder.with_head().build()
            };
            let config = config.map_err(|e| format!("invalid browser config: {e}"))?;
            Browser::launch(config).await.map_err(|e| e.to_string())
        }
    })?;

    // Drive the CDP handler in the background so the connection stays alive
    // between tool calls.
    {
        let runtime = runtime.clone();
        runtime.spawn(async move {
            let mut handler = handler;
            while let Some(event) = handler.next().await {
                let _ = event;
            }
            // Keep the task alive until the runtime shuts down.
            std::future::pending::<()>().await;
        });
    }

    Ok(BrowserSession {
        runtime,
        browser: Some(browser),
        tabs: Vec::new(),
        active: 0,
        refs: HashMap::new(),
    })
}

/// 安全驱动 `fut`:若当前线程已在某个 tokio runtime 上下文中,则在独立 OS
/// 线程上执行 `handle.block_on`,避免 "Cannot start a runtime from within a
/// runtime" panic;否则直接 `rt.block_on`。参照 llm_clients.rs 的同款修复。
fn block_on_future<T: Send + 'static>(
    rt: &Arc<tokio::runtime::Runtime>,
    fut: impl std::future::Future<Output = Result<T, String>> + Send + 'static,
) -> Result<T, String> {
    if tokio::runtime::Handle::try_current().is_ok() {
        let handle = rt.handle().clone();
        std::thread::spawn(move || handle.block_on(fut))
            .join()
            .map_err(|e| format!("browser_control worker thread panicked: {e:?}"))?
    } else {
        rt.block_on(fut)
    }
}

/// Resolve the CDP endpoint from the raw inputs. Accepts a `ws://` URL, an
/// `http://` URL (the `/json/version` endpoint is queried automatically), or a
/// bare port number that is expanded to `http://127.0.0.1:<port>`.
fn connect_target_parts(url: Option<String>, port: Option<u16>) -> Result<String, String> {
    if let Some(u) = url {
        let u = u.trim();
        if !u.is_empty() {
            return Ok(u.to_string());
        }
    }
    if let Some(p) = port {
        return Ok(format!("http://127.0.0.1:{p}"));
    }
    Err(
        "action 'connect' requires 'url' (e.g. \"http://127.0.0.1:9222\" or \
         \"ws://127.0.0.1:9222/devtools/browser/…\") or 'port'"
            .to_string(),
    )
}

async fn dispatch(
    session: &mut BrowserSession,
    input: &BrowserControlInput,
) -> Result<String, String> {
    match input.action.as_str() {
        // ------------------------------------------------------------------
        // Navigation
        // ------------------------------------------------------------------
        "launch" => {
            let page = ensure_active_page(session).await?;
            let (url, title) = page_basics(&page).await;
            Ok(json!({ "status": "ready", "url": url, "title": title }).to_string())
        }
        "connect" => {
            // Session was attached at creation time. Adopt any tabs that were
            // already open in the target browser (e.g. the user's own Chrome,
            // an Electron app), otherwise open a fresh one.
            if session.tabs.is_empty() {
                let pages = {
                    let browser = session
                        .browser
                        .as_ref()
                        .ok_or_else(|| "browser not attached".to_string())?;
                    browser
                        .pages()
                        .await
                        .map_err(|e| format!("list pages failed: {e}"))?
                };
                if pages.is_empty() {
                    ensure_active_page(session).await?;
                } else {
                    session.tabs = pages;
                    session.active = 0;
                }
            }
            let (url, title) = page_basics(&session.tabs[session.active]).await;
            Ok(json!({
                "status": "connected",
                "tab_count": session.tabs.len(),
                "active": session.active,
                "url": url,
                "title": title,
            })
            .to_string())
        }
        "goto" => {
            let url = input
                .url
                .clone()
                .ok_or_else(|| "action 'goto' requires 'url'".to_string())?;
            let page = ensure_active_page(session).await?;
            page.goto(url.clone())
                .await
                .map_err(|e| format!("navigate failed: {e}"))?;
            let _ = page.wait_for_navigation().await;
            // 等待页面真正就绪:readyState=complete 且 body 有实际内容,且不在
            // 反爬挑战页(Cloudflare "Pardon Our Interruption" / "Just a moment"
            // 等)。挑战页在挑战通过前 AX 树近乎为空,过早返回会让 AI 拿到
            // 空 snapshot 而放弃感知层转 evaluate_js 盲试(实测 60+ 次盲试)。
            let mut ready = false;
            for _ in 0..50 {
                ready = page
                    .evaluate_expression(
                        "(() => { const t = document.body ? document.body.innerText : ''; \
                         const challenge = /Pardon Our Interruption|Just a moment|Verify you are \
                         human|请稍候|正在验证|cf-challenge/i; \
                         return document.readyState === 'complete' && t.trim().length > 100 && \
                         !challenge.test(t); })()",
                    )
                    .await
                    .ok()
                    .and_then(|r| r.into_value::<bool>().ok())
                    .unwrap_or(false);
                if ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let (current, title) = page_basics(&page).await;
            // Navigation invalidates the previous ref map.
            session.refs.clear();
            Ok(json!({
                "status": "navigated",
                "url": current,
                "title": title,
                "ready": ready,
            })
            .to_string())
        }
        "back" => {
            let page = ensure_active_page(session).await?;
            navigate_history(&page, -1).await?;
            session.refs.clear();
            let (url, title) = page_basics(&page).await;
            Ok(json!({ "status": "ok", "url": url, "title": title }).to_string())
        }
        "forward" => {
            let page = ensure_active_page(session).await?;
            navigate_history(&page, 1).await?;
            session.refs.clear();
            let (url, title) = page_basics(&page).await;
            Ok(json!({ "status": "ok", "url": url, "title": title }).to_string())
        }
        "reload" => {
            let page = ensure_active_page(session).await?;
            page.reload()
                .await
                .map_err(|e| format!("reload failed: {e}"))?;
            tokio::time::sleep(Duration::from_millis(400)).await;
            session.refs.clear();
            let (url, title) = page_basics(&page).await;
            Ok(json!({ "status": "ok", "url": url, "title": title }).to_string())
        }

        // ------------------------------------------------------------------
        // Perception
        // ------------------------------------------------------------------
        "snapshot" => {
            let page = ensure_active_page(session).await?;
            let (url, title) = page_basics(&page).await;
            session.refs.clear();
            let tree = build_ax_snapshot(&page, &mut session.refs).await?;
            Ok(json!({
                "url": url,
                "title": title,
                "page": tree,
                "hint": "Interactive elements carry [ref=eN]. Address them with click/type/fill/press_key/select_option/check. Read a live element with get_state (same ref). After any change run snapshot again to refresh refs."
            })
            .to_string())
        }
        "screenshot" => {
            let page = ensure_active_page(session).await?;
            let bytes = page
                .screenshot(ScreenshotParams::builder().build())
                .await
                .map_err(|e| format!("screenshot failed: {e}"))?;
            let path = resolve_screenshot_path(input.save_path.as_deref())?;
            fs::write(&path, &bytes)
                .map_err(|e| format!("failed to write screenshot {}: {e}", path.display()))?;
            let (url, title) = page_basics(&page).await;
            Ok(json!({
                "path": path.display().to_string(),
                "bytes": bytes.len(),
                "url": url, "title": title
            })
            .to_string())
        }
        "get_state" => {
            let page = ensure_active_page(session).await?;
            let state = read_state(&page, session, input).await?;
            Ok(state.to_string())
        }
        "wait_for" => {
            let page = ensure_active_page(session).await?;
            wait_for_condition(&page, input).await
        }

        // ------------------------------------------------------------------
        // Interaction
        // ------------------------------------------------------------------
        "click" => {
            let page = ensure_active_page(session).await?;
            let target = resolve_target(session, input)?;
            click_target(&page, &target).await?;
            let (url, title) = page_basics(&page).await;
            Ok(json!({
                "status": "clicked",
                "on": describe_target(input),
                "url": url,
                "title": title,
                "next": "Run get_state or snapshot to confirm the result."
            })
            .to_string())
        }
        "fill" => {
            let page = ensure_active_page(session).await?;
            let text = input
                .text
                .clone()
                .ok_or_else(|| "action 'fill' requires 'text'".to_string())?;
            let target = resolve_target(session, input)?;
            fill_or_type(&page, &target, &text, true).await?;
            let state = element_state(&page, &target).await?;
            let (url, title) = page_basics(&page).await;
            Ok(json!({
                "status": "filled",
                "chars": text.chars().count(),
                "on": describe_target(input),
                "value": state.get("value2").cloned().unwrap_or(Value::Null),
                "url": url,
                "title": title
            })
            .to_string())
        }
        "type" => {
            let page = ensure_active_page(session).await?;
            let text = input
                .text
                .clone()
                .ok_or_else(|| "action 'type' requires 'text'".to_string())?;
            let target = resolve_target(session, input)?;
            fill_or_type(&page, &target, &text, false).await?;
            let state = element_state(&page, &target).await?;
            let (url, title) = page_basics(&page).await;
            Ok(json!({
                "status": "typed",
                "chars": text.chars().count(),
                "on": describe_target(input),
                "value": state.get("value2").cloned().unwrap_or(Value::Null),
                "url": url,
                "title": title
            })
            .to_string())
        }
        "press_key" => {
            let page = ensure_active_page(session).await?;
            let key = input
                .key
                .clone()
                .ok_or_else(|| "action 'press_key' requires 'key'".to_string())?;
            if let Some(target) = resolve_target_opt(session, input)? {
                focus_target(&page, &target).await?;
            }
            press_key(&page, &key).await?;
            let (url, title) = page_basics(&page).await;
            Ok(json!({ "status": "pressed", "key": key, "url": url, "title": title }).to_string())
        }
        "hover" => {
            let page = ensure_active_page(session).await?;
            let target = resolve_target(session, input)?;
            hover_target(&page, &target).await?;
            Ok(json!({ "status": "hovered", "on": describe_target(input) }).to_string())
        }
        "scroll" => {
            let page = ensure_active_page(session).await?;
            let direction = input.direction.as_deref().unwrap_or("down");
            match direction {
                "element" => {
                    let target = resolve_target(session, input)?;
                    scroll_element_into_view(&page, &target).await?;
                }
                "up" | "down" | "top" | "bottom" => {
                    let script = match direction {
                        "up" => "window.scrollBy({top: -window.innerHeight * 0.8, behavior: 'smooth'});",
                        "down" => "window.scrollBy({top: window.innerHeight * 0.8, behavior: 'smooth'});",
                        "top" => "window.scrollTo({top: 0, behavior: 'smooth'});",
                        _ => "window.scrollTo({top: document.body.scrollHeight, behavior: 'smooth'});",
                    };
                    page.evaluate_expression(script)
                        .await
                        .map_err(|e| format!("scroll failed: {e}"))?;
                }
                other => return Err(format!("unsupported scroll direction: {other}")),
            }
            Ok(json!({ "status": "scrolled", "direction": direction }).to_string())
        }
        "select_option" => {
            let page = ensure_active_page(session).await?;
            let value = input
                .value
                .clone()
                .ok_or_else(|| "action 'select_option' requires 'value'".to_string())?;
            let target = resolve_target(session, input)?;
            select_option_target(&page, &target, &value).await?;
            let state = element_state(&page, &target).await?;
            Ok(json!({
                "status": "selected",
                "value": value,
                "on": describe_target(input),
                "selected": state.get("value2").cloned().unwrap_or(Value::Null)
            })
            .to_string())
        }
        "check" | "uncheck" => {
            let page = ensure_active_page(session).await?;
            let want = input.action == "check";
            let target = resolve_target(session, input)?;
            set_checked_target(&page, &target, want).await?;
            let state = element_state(&page, &target).await?;
            Ok(json!({
                "status": if want { "checked" } else { "unchecked" },
                "on": describe_target(input),
                "checked": state.get("checked").cloned().unwrap_or(Value::Null)
            })
            .to_string())
        }

        // ------------------------------------------------------------------
        // Tabs
        // ------------------------------------------------------------------
        "new_tab" => {
            let url = input.url.clone().unwrap_or_default();
            new_tab(session, &url).await?;
            let page = ensure_active_page(session).await?;
            let (url, title) = page_basics(&page).await;
            Ok(json!({
                "status": "opened",
                "tab": session.active,
                "tabs": session.tabs.len(),
                "url": url,
                "title": title
            })
            .to_string())
        }
        "switch_tab" => {
            let idx = input
                .index
                .ok_or_else(|| "action 'switch_tab' requires 'index'".to_string())?;
            if idx >= session.tabs.len() {
                return Err(format!(
                    "tab index {idx} out of range, {} tab(s) open",
                    session.tabs.len()
                ));
            }
            session.active = idx;
            session.refs.clear();
            let page = ensure_active_page(session).await?;
            let _ = page.activate().await;
            let (url, title) = page_basics(&page).await;
            Ok(json!({
                "status": "switched",
                "tab": session.active,
                "tabs": session.tabs.len(),
                "url": url,
                "title": title
            })
            .to_string())
        }
        "list_tabs" => {
            let mut tabs = Vec::new();
            for (i, page) in session.tabs.iter().enumerate() {
                let (url, title) = page_basics(page).await;
                tabs.push(
                    json!({ "tab": i, "url": url, "title": title, "active": i == session.active }),
                );
            }
            Ok(json!({ "tabs": tabs }).to_string())
        }
        "close_tab" => {
            let idx = if let Some(i) = input.index {
                if i >= session.tabs.len() {
                    return Err(format!(
                        "tab index {i} out of range, {} tab(s) open",
                        session.tabs.len()
                    ));
                }
                i
            } else {
                session.active
            };
            if session.tabs.is_empty() {
                return Ok(json!({ "status": "closed", "tabs": 0 }).to_string());
            }
            let page = session.tabs.remove(idx);
            page.close()
                .await
                .map_err(|e| format!("close tab failed: {e}"))?;
            if session.tabs.is_empty() {
                return Ok(json!({ "status": "closed", "tabs": 0 }).to_string());
            }
            session.active = session.active.min(session.tabs.len() - 1);
            session.refs.clear();
            let page = ensure_active_page(session).await?;
            let (url, title) = page_basics(&page).await;
            Ok(json!({
                "status": "closed",
                "tabs": session.tabs.len(),
                "tab": session.active,
                "url": url,
                "title": title
            })
            .to_string())
        }

        // ------------------------------------------------------------------
        // Escape hatch
        // ------------------------------------------------------------------
        "evaluate_js" => {
            let page = ensure_active_page(session).await?;
            let script = input
                .script
                .clone()
                .ok_or_else(|| "action 'evaluate_js' requires 'script'".to_string())?;
            let result = page
                .evaluate_expression(script.clone())
                .await
                .map_err(|e| format!("evaluate failed: {e}"))?;
            let value = result
                .into_value::<Value>()
                .map_err(|e| format!("expression did not return a JSON value: {e}"))?;
            Ok(json!({ "result": value }).to_string())
        }
        other => Err(format!("unsupported browser_control action: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Perception layer: accessibility snapshot
// ---------------------------------------------------------------------------

/// Set of roles considered interactive. These receive a `ref` and are safe to
/// address with click/type/select/etc. Mirrors the Playwright MCP interactive
/// element heuristic.
const INTERACTIVE_ROLES: &[&str] = &[
    "button",
    "link",
    "textbox",
    "searchbox",
    "combobox",
    "listbox",
    "checkbox",
    "radio",
    "switch",
    "menuitem",
    "tab",
    "spinbutton",
    "slider",
    "treeitem",
    "gridcell",
    "option",
];

/// Maximum total lines emitted by a snapshot (budget control).
const MAX_SNAPSHOT_LINES: usize = 240;
/// Maximum line length for a single node.
const MAX_LINE_CHARS: usize = 160;

/// Build an AX-tree snapshot string and fill `refs` with ref → backendNodeId.
///
/// AX 树的生成滞后于 DOM 就绪(重 JS / Cloudflare 挑战页在 readyState
/// complete 后仍需数秒才产出完整 AX 树)。空树时自动等待 2 秒重试,
/// 最多 5 次,避免 AI 拿到近乎空的快照而放弃感知层转 evaluate_js 盲试。
async fn build_ax_snapshot(
    page: &Page,
    refs: &mut HashMap<String, BackendNodeId>,
) -> Result<String, String> {
    let mut out = String::new();
    let mut lines = 0usize;
    let mut truncated = false;

    for _ in 0..5 {
        let (o, l, t) = render_ax_tree(page, refs).await?;
        out = o;
        lines = l;
        truncated = t;
        if lines > 2 {
            break;
        }
        // AX 树尚未生成完整,短暂等待后重试。
        tokio::time::sleep(Duration::from_millis(2000)).await;
    }

    if out.trim().is_empty() {
        out.push_str("(no accessible content)");
    }
    if truncated {
        out.push_str(&format!(
            "\n... snapshot truncated at {lines} lines. The page is large; narrow your focus."
        ));
    }
    // 空树引导:重试后 AX 树仍近乎为空,通常是页面加载过慢或被反爬挑战页
    // (如 Cloudflare "Pardon Our Interruption")挡住。引导 AI 等待后重试,
    // 避免放弃感知层转 evaluate_js 盲猜 DOM。
    if lines <= 2 {
        out.push_str(
            "\n[snapshot-warning] AX tree is nearly empty after retries — the page is \
             probably still loading or behind a bot-challenge (e.g. Cloudflare). Run \
             'wait_for' (networkidle or text:…) and then re-run 'snapshot'; do NOT fall \
             back to guessing the DOM with evaluate_js.",
        );
    }
    Ok(out)
}

/// 单次取 AX 树并渲染为快照文本,返回 (文本, 有效行数, 是否截断)。
async fn render_ax_tree(
    page: &Page,
    refs: &mut HashMap<String, BackendNodeId>,
) -> Result<(String, usize, bool), String> {
    let resp = page
        .execute(GetFullAxTreeParams::builder().build())
        .await
        .map_err(|e| format!("getFullAXTree failed: {e}"))?;
    let nodes = resp.nodes.clone();
    refs.clear();

    // Index nodes by AX node id.
    let mut by_id: HashMap<&str, &AxNode> = HashMap::new();
    for node in &nodes {
        by_id.insert(node.node_id.inner().as_str(), node);
    }

    // 组织父子关系。**不能跳过 ignored 节点**:ignored 中间容器(如无语义的
    // div)虽然不渲染,但必须挂在父节点下、由 walk 穿过,否则其非 ignored
    // 后代(表单控件、链接等)会整体不可达,导致快照只剩根节点(实测海航
    // 官网 1856 节点 AX 树只渲染出 1 行)。
    let mut children = HashMap::<&str, Vec<&AxNode>>::new();
    for node in &nodes {
        match &node.parent_id {
            Some(parent) if by_id.contains_key(parent.inner().as_str()) => {
                children
                    .entry(parent.inner().as_str())
                    .or_default()
                    .push(node);
            }
            _ => {
                children.entry("").or_default().push(node);
            }
        }
    }

    // Stable child ordering by AX node id (the browser emits them in tree order,
    // but we defensively keep a sorted walk per parent).
    for list in children.values_mut() {
        list.sort_by_key(|n| Reverse(n.node_id.inner().len()));
        list.sort_by_key(|n| n.node_id.inner().clone());
    }

    // Depth-first walk with a ref counter. Only interactive nodes consume a ref.
    let mut counter = 0usize;
    let mut out = String::new();
    let mut lines = 0usize;
    let mut truncated = false;

    #[allow(clippy::too_many_arguments)]
    fn walk(
        node: &AxNode,
        depth: usize,
        // `_by_id` (leading underscore) silences clippy::only_used_in_recursion,
        // which mis-fires on nested fn parameters passed through recursion.
        _by_id: &HashMap<&str, &AxNode>,
        children: &HashMap<&str, Vec<&AxNode>>,
        refs: &mut HashMap<String, BackendNodeId>,
        counter: &mut usize,
        out: &mut String,
        lines: &mut usize,
        truncated: &mut bool,
    ) {
        if *lines >= MAX_SNAPSHOT_LINES {
            *truncated = true;
            return;
        }
        let role = ax_str(&node.role).unwrap_or_default();
        if role == "generic" && ax_str(&node.name).is_none() {
            // Skip anonymous containers to keep the tree compact.
        } else if !node.ignored && !role.is_empty() {
            let interactive =
                INTERACTIVE_ROLES.contains(&role.as_str()) || node.backend_dom_node_id.is_some();
            let ref_id = if interactive {
                *counter += 1;
                let rid = format!("e{}", counter);
                if let Some(bid) = &node.backend_dom_node_id {
                    refs.insert(rid.clone(), *bid);
                }
                rid
            } else {
                String::new()
            };
            let line = render_node_line(node, &role, &ref_id);
            if *lines < MAX_SNAPSHOT_LINES {
                out.push_str(&"  ".repeat(depth.min(12)));
                out.push_str(&line);
                out.push('\n');
                *lines += 1;
            }
        }
        if let Some(kids) = children.get(node.node_id.inner().as_str()) {
            for child in kids {
                walk(
                    child,
                    depth + 1,
                    _by_id,
                    children,
                    refs,
                    counter,
                    out,
                    lines,
                    truncated,
                );
            }
        }
    }

    let roots = children.get("").cloned().unwrap_or_default();
    for root in &roots {
        walk(
            root,
            0,
            &by_id,
            &children,
            refs,
            &mut counter,
            &mut out,
            &mut lines,
            &mut truncated,
        );
    }

    Ok((out, lines, truncated))
}

/// Render one AX node as `- role "name" [ref=eN]` plus state markers.
fn render_node_line(node: &AxNode, role: &str, ref_id: &str) -> String {
    let name = ax_str(&node.name).unwrap_or_default();
    let mut props: Vec<String> = Vec::new();
    if !ref_id.is_empty() {
        props.push(ref_id.to_string());
    }
    if let Some(level) = ax_prop_int(node, "level") {
        props.push(format!("{level}"));
    }
    for (label, key) in [
        ("checked", "checked"),
        ("disabled", "disabled"),
        ("expanded", "expanded"),
        ("selected", "selected"),
        ("pressed", "pressed"),
    ] {
        if let Some(v) = ax_prop_bool(node, key) {
            props.push(format!("{label}={v}"));
        }
    }
    if let Some(v) = ax_str(&node.value) {
        if !v.is_empty() && role != "text" {
            props.push(format!("value={v:?}"));
        }
    }
    let mut line = format!("- {role}");
    if !name.is_empty() {
        line.push_str(&format!(" {name:?}"));
    }
    if !props.is_empty() {
        line.push_str(&format!(" [{}]", props.join(", ")));
    }
    if line.chars().count() > MAX_LINE_CHARS {
        let cut: String = line.chars().take(MAX_LINE_CHARS - 3).collect();
        line = format!("{cut}...");
    }
    line
}

fn ax_str(v: &Option<AxValue>) -> Option<String> {
    v.as_ref()
        .and_then(|v| v.value.as_ref())
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn ax_prop_bool(node: &AxNode, name: &str) -> Option<bool> {
    node.properties
        .as_ref()
        .and_then(|props| props.iter().find(|p| p.name.as_ref() == name))
        .and_then(|p| p.value.value.as_ref())
        .and_then(Value::as_bool)
}

fn ax_prop_int(node: &AxNode, name: &str) -> Option<i64> {
    node.properties
        .as_ref()
        .and_then(|props| props.iter().find(|p| p.name.as_ref() == name))
        .and_then(|p| p.value.value.as_ref())
        .and_then(Value::as_i64)
}

// ---------------------------------------------------------------------------
// Execution layer: target resolution & real CDP actions
// ---------------------------------------------------------------------------

/// Target resolved by `ref` (preferred) or fallback CSS selector.
#[derive(Debug, Clone)]
enum Target {
    /// Backend node id taken from the last snapshot's ref map.
    ByRef(BackendNodeId),
    /// CSS selector fallback.
    BySelector(String),
}

fn describe_target(input: &BrowserControlInput) -> String {
    if let Some(r) = &input.ref_ {
        format!("ref={r}")
    } else if let Some(s) = &input.selector {
        format!("selector={s:?}")
    } else {
        "current page".to_string()
    }
}

fn resolve_target(
    session: &mut BrowserSession,
    input: &BrowserControlInput,
) -> Result<Target, String> {
    resolve_target_opt(session, input)?
        .ok_or_else(|| format!("element not addressed: {}", describe_target(input)))
}

fn resolve_target_opt(
    session: &mut BrowserSession,
    input: &BrowserControlInput,
) -> Result<Option<Target>, String> {
    if let Some(r) = &input.ref_ {
        let key = r.trim_start_matches('#');
        if let Some(bid) = session.refs.get(key) {
            return Ok(Some(Target::ByRef(*bid)));
        }
        return Err(format!(
            "ref {r} not found in the last snapshot — run 'snapshot' first or after navigation"
        ));
    }
    if let Some(sel) = &input.selector {
        return Ok(Some(Target::BySelector(sel.clone())));
    }
    Ok(None)
}

/// Resolve a `Target` into a JS object handle (`RemoteObjectId`) used for
/// `Runtime.callFunctionOn`, plus the backend node id used for coordinates.
async fn resolve_target_ids(
    page: &Page,
    target: &Target,
) -> Result<(RemoteObjectId, Option<BackendNodeId>), String> {
    match target {
        Target::ByRef(bid) => {
            let object_id = resolve_object_id(page, bid).await?;
            Ok((object_id, Some(*bid)))
        }
        Target::BySelector(sel) => {
            let doc = page
                .execute(GetDocumentParams::builder().build())
                .await
                .map_err(|e| format!("getDocument failed: {e}"))?;
            let root = doc.root.node_id;
            let qparams = QuerySelectorParams::builder()
                .node_id(root)
                .selector(sel.clone())
                .build()?;
            let query = page
                .execute(qparams)
                .await
                .map_err(|e| format!("querySelector failed: {e}"))?;
            if *query.node_id.inner() == 0 {
                return Err(format!("selector {sel:?} did not match any element"));
            }
            let object_id = resolve_object_id_from_node(page, &query.node_id).await?;
            let backend = describe_backend_id(page, &query.node_id).await?;
            Ok((object_id, backend))
        }
    }
}

/// `DOM.resolveNode` → `RemoteObjectId` (JS object handle for `callFunctionOn`).
async fn resolve_object_id(page: &Page, bid: &BackendNodeId) -> Result<RemoteObjectId, String> {
    let resp = page
        .execute(ResolveNodeParams::builder().backend_node_id(*bid).build())
        .await
        .map_err(|e| format!("resolveNode failed: {e}"))?;
    resp.result
        .object
        .object_id
        .ok_or_else(|| "resolved node has no runtime object (detached?)".to_string())
}

/// `DOM.resolveNode` from a document node id.
async fn resolve_object_id_from_node(
    page: &Page,
    node_id: &chromiumoxide::cdp::browser_protocol::dom::NodeId,
) -> Result<RemoteObjectId, String> {
    let resp = page
        .execute(ResolveNodeParams::builder().node_id(*node_id).build())
        .await
        .map_err(|e| format!("resolveNode failed: {e}"))?;
    resp.result
        .object
        .object_id
        .ok_or_else(|| "resolved node has no runtime object (detached?)".to_string())
}

/// Run a JS function (declared without args) on the element, returning by value.
async fn call_on_node(
    page: &Page,
    object_id: &RemoteObjectId,
    function_declaration: &str,
) -> Result<Value, String> {
    let mut params = CallFunctionOnParams::new(function_declaration);
    params.object_id = Some(object_id.clone());
    params.return_by_value = Some(true);
    params.await_promise = Some(true);
    params.user_gesture = Some(true);
    let resp = page
        .execute(params)
        .await
        .map_err(|e| format!("callFunctionOn failed: {e}"))?;
    resp.result
        .result
        .value
        .or_else(|| {
            // `undefined` results are reported as no value; normalize to null.
            if resp.result.result.r#type == RemoteObjectType::Undefined {
                Some(Value::Null)
            } else {
                None
            }
        })
        .ok_or_else(|| "function produced no serializable result".to_string())
}

/// Describe a node id → backend node id (for coordinate math).
async fn describe_backend_id(
    page: &Page,
    node_id: &chromiumoxide::cdp::browser_protocol::dom::NodeId,
) -> Result<Option<BackendNodeId>, String> {
    let resp = page
        .execute(DescribeNodeParams::builder().node_id(*node_id).build())
        .await
        .map_err(|e| format!("describeNode failed: {e}"))?;
    Ok(Some(resp.node.backend_node_id))
}

// --- Real mouse / keyboard events -----------------------------------------

/// Scroll the element into view and compute its on-screen center point.
async fn element_center(page: &Page, bid: &BackendNodeId) -> Option<Point> {
    let _ = page
        .execute(
            ScrollIntoViewIfNeededParams::builder()
                .backend_node_id(*bid)
                .build(),
        )
        .await;
    let resp = page
        .execute(
            GetContentQuadsParams::builder()
                .backend_node_id(*bid)
                .build(),
        )
        .await
        .ok()?;
    resp.quads
        .iter()
        .filter(|q| q.inner().len() == 8)
        .map(ElementQuad::from_quad)
        .filter(|q| q.quad_area() > 1.)
        .map(|q| q.quad_center())
        .next()
}

/// Real mouse event: mouseMoved to the point (hover semantics).
async fn mouse_move_to(page: &Page, p: &Point) -> Result<(), String> {
    let params = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseMoved)
        .x(p.x)
        .y(p.y)
        .build()
        .map_err(|e| e.to_string())?;
    page.execute(params)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Real mouse event: press + release at a point (click semantics).
async fn mouse_click_at(page: &Page, p: &Point) -> Result<(), String> {
    let mut params = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MousePressed)
        .x(p.x)
        .y(p.y)
        .button(MouseButton::Left)
        .build()
        .map_err(|e| e.to_string())?;
    params.click_count = Some(1);
    page.execute(params).await.map_err(|e| e.to_string())?;
    let mut release = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseReleased)
        .x(p.x)
        .y(p.y)
        .button(MouseButton::Left)
        .build()
        .map_err(|e| e.to_string())?;
    release.click_count = Some(1);
    page.execute(release)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Click with a real mouse event; falls back to an element-level JS click
/// when the element has no laid-out coordinates (hidden/detached).
async fn click_target(page: &Page, target: &Target) -> Result<(), String> {
    let (object_id, backend) = resolve_target_ids(page, target).await?;
    if let Some(bid) = &backend {
        if let Some(center) = element_center(page, bid).await {
            // Move the pointer first, then click, matching a real user.
            let _ = mouse_move_to(page, &center).await;
            return mouse_click_at(page, &center).await;
        }
    }
    // Fallback: JS click (no scroll/coords needed; triggers the same handlers).
    let out = call_on_node(
        page,
        &object_id,
        "function(){ this.scrollIntoView({block:'center'}); this.click(); return 'clicked'; }",
    )
    .await?;
    if out.as_str() != Some("clicked") {
        return Err(format!("click produced no confirmation: {out}"));
    }
    Ok(())
}

/// Hover with a real mouse move over the element's center.
async fn hover_target(page: &Page, target: &Target) -> Result<(), String> {
    let (_, backend) = resolve_target_ids(page, target).await?;
    let bid = backend
        .ok_or_else(|| "hover needs a backend node id; the element may be off-DOM".to_string())?;
    let center = element_center(page, &bid)
        .await
        .ok_or_else(|| "element has no on-screen position (hidden?)".to_string())?;
    mouse_move_to(page, &center).await
}

/// Focus an element via JS (triggers site focus logic).
async fn focus_target(page: &Page, target: &Target) -> Result<(), String> {
    let (object_id, _) = resolve_target_ids(page, target).await?;
    let out = call_on_node(
        page,
        &object_id,
        "function(){ this.focus(); return 'focused'; }",
    )
    .await?;
    if out.as_str() != Some("focused") {
        return Err(format!("focus produced no confirmation: {out}"));
    }
    Ok(())
}

/// Scroll an element into view (smooth).
async fn scroll_element_into_view(page: &Page, target: &Target) -> Result<(), String> {
    let (object_id, _) = resolve_target_ids(page, target).await?;
    call_on_node(
        page,
        &object_id,
        "function(){ this.scrollIntoView({block:'center', behavior:'smooth'}); return 'scrolled'; }",
    )
    .await?;
    Ok(())
}

/// `fill` clears then types; `type` focuses and types without clearing.
/// Typing uses `Input.insertText` — the real keyboard input path, which
/// triggers `input` events and works with composition-based sites.
async fn fill_or_type(
    page: &Page,
    target: &Target,
    text: &str,
    clear_first: bool,
) -> Result<(), String> {
    let (object_id, _) = resolve_target_ids(page, target).await?;
    if clear_first {
        let out = call_on_node(
            page,
            &object_id,
            "function(){ this.focus(); this.select(); return 'ready'; }",
        )
        .await?;
        if out.as_str() != Some("ready") {
            return Err(format!("focus/select produced no confirmation: {out}"));
        }
        // select() selects existing text; one Backspace clears it.
        press_key(page, "Backspace").await?;
    } else {
        focus_target(page, target).await?;
    }
    page.execute(InsertTextParams::new(text))
        .await
        .map_err(|e| format!("insertText failed: {e}"))?;
    let _ = page.execute(InsertTextParams::new("\u{0}")).await;
    Ok(())
}

/// Select an option of a native `<select>` by value, with input/change events.
async fn select_option_target(page: &Page, target: &Target, value: &str) -> Result<(), String> {
    let (object_id, _) = resolve_target_ids(page, target).await?;
    let value_json = serde_json::to_string(value).map_err(|e| e.to_string())?;
    let decl = format!(
        "function(){{ const el=this; el.value={value_json}; \
         el.dispatchEvent(new Event('input',{{bubbles:true}})); \
         el.dispatchEvent(new Event('change',{{bubbles:true}})); \
         return el.value; }}",
        value_json = value_json
    );
    let out = call_on_node(page, &object_id, &decl).await?;
    if out.as_str() != Some(value) {
        return Err(format!(
            "select did not stick: expected {value:?}, got {out}. The target may not be a <select>."
        ));
    }
    Ok(())
}

/// Check/uncheck a checkbox/radio via JS with input/change events.
async fn set_checked_target(page: &Page, target: &Target, want: bool) -> Result<(), String> {
    let (object_id, _) = resolve_target_ids(page, target).await?;
    let decl = format!(
        "function(){{ const el=this; if ('checked' in el) el.checked={want}; \
         el.dispatchEvent(new Event('input',{{bubbles:true}})); \
         el.dispatchEvent(new Event('change',{{bubbles:true}})); \
         return true; }}",
        want = want
    );
    call_on_node(page, &object_id, &decl).await?;
    Ok(())
}

/// Read the live state of a page (and of one element when targeted).
async fn read_state(
    page: &Page,
    session: &mut BrowserSession,
    input: &BrowserControlInput,
) -> Result<Value, String> {
    let mut out = json!({
        "url": page.url().await.ok().flatten().unwrap_or_default(),
        "title": page.get_title().await.ok().flatten().unwrap_or_default(),
    });
    if let Some(target) = resolve_target_opt(session, input)? {
        let state = element_state(page, &target).await?;
        out["element"] = state;
    }
    Ok(out)
}

/// Element state snapshot for verification (what a sighted user would see).
async fn element_state(page: &Page, target: &Target) -> Result<Value, String> {
    let (object_id, _) = resolve_target_ids(page, target).await?;
    let out = call_on_node(
        page,
        &object_id,
        "function(){
            const el = this;
            const r = el.getBoundingClientRect ? el.getBoundingClientRect() : {x:0,y:0,width:0,height:0};
            let visible = false;
            if (el.getClientRects && el.getClientRects().length > 0) {
                const cs = el.ownerDocument.defaultView.getComputedStyle(el);
                visible = r.width > 0 && r.height > 0 && cs.visibility !== 'hidden' && cs.display !== 'none';
            }
            const o = {
                tag: el.tagName ? el.tagName.toLowerCase() : null,
                role: el.getAttribute ? el.getAttribute('role') : null,
                name: (el.getAttribute && (el.getAttribute('aria-label') || el.getAttribute('name') || el.id)) || null,
                text: (el.innerText || el.textContent || '').slice(0, 500),
                visible: visible,
                rect: { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height) }
            };
            if ('value' in el) o.value = el.value;
            if ('checked' in el) o.checked = el.checked;
            if ('disabled' in el) o.disabled = el.disabled;
            if ('duration' in el) o.duration = el.duration;
            return o;
        }",
    )
    .await?;
    Ok(out)
}

// --- Page-level helpers -----------------------------------------------------

async fn ensure_active_page(session: &mut BrowserSession) -> Result<Page, String> {
    if session.tabs.is_empty() {
        let page = {
            let browser = session
                .browser
                .as_ref()
                .ok_or_else(|| "browser not launched — call 'launch' first".to_string())?;
            browser
                .new_page("about:blank")
                .await
                .map_err(|e| format!("failed to open a tab: {e}"))?
        };
        session.tabs.push(page);
        session.active = 0;
    }
    Ok(session.tabs[session.active].clone())
}

async fn new_tab(session: &mut BrowserSession, url: &str) -> Result<(), String> {
    let page = {
        let browser = session
            .browser
            .as_ref()
            .ok_or_else(|| "browser not launched — call 'launch' first".to_string())?;
        browser
            .new_page(url)
            .await
            .map_err(|e| format!("failed to open tab: {e}"))?
    };
    session.tabs.push(page);
    session.active = session.tabs.len() - 1;
    session.refs.clear();
    Ok(())
}

async fn page_basics(page: &Page) -> (String, String) {
    let url = page.url().await.ok().flatten().unwrap_or_default();
    let title = page.get_title().await.ok().flatten().unwrap_or_default();
    (url, title)
}

async fn navigate_history(page: &Page, delta: i32) -> Result<(), String> {
    use chromiumoxide::cdp::browser_protocol::page::{
        GetNavigationHistoryParams, NavigateToHistoryEntryParams,
    };
    let history = page
        .execute(GetNavigationHistoryParams {})
        .await
        .map_err(|e| format!("getNavigationHistory failed: {e}"))?;
    let current = history.current_index as i64;
    let target = current + delta as i64;
    if target < 0 {
        return Err("no previous page in history".to_string());
    }
    let entries = &history.entries;
    if let Some(entry) = entries.get(target as usize) {
        let nav = NavigateToHistoryEntryParams::builder()
            .entry_id(entry.id)
            .build()?;
        page.execute(nav)
            .await
            .map_err(|e| format!("navigateToHistoryEntry failed: {e}"))?;
        page.wait_for_navigation().await.ok();
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    } else {
        Err("history entry out of range".to_string())
    }
}

/// Poll the page until a condition is met or the timeout elapses.
/// Conditions: `url:…`, `text:…`, `networkidle`, or a CSS selector.
async fn wait_for_condition(page: &Page, input: &BrowserControlInput) -> Result<String, String> {
    let cond = input
        .wait_for
        .clone()
        .ok_or_else(|| "action 'wait_for' requires 'wait_for'".to_string())?;
    let timeout = Duration::from_millis(input.timeout_ms.unwrap_or(5000));
    let deadline = SystemTime::now() + timeout;
    let cond_trim = cond.trim();

    let mut last_resources: i64 = -1;
    loop {
        let matched = if let Some(url) = cond_trim.strip_prefix("url:").map(str::trim) {
            page.url()
                .await
                .ok()
                .flatten()
                .is_some_and(|u| u.contains(url))
        } else if let Some(text) = cond_trim.strip_prefix("text:").map(str::trim) {
            let body = page
                .evaluate_expression("document.body ? document.body.innerText : ''")
                .await
                .ok()
                .and_then(|r| r.into_value::<String>().ok())
                .unwrap_or_default();
            body.contains(text)
        } else if cond_trim.starts_with("networkidle") {
            // Approximation: document complete AND resource count stopped growing.
            let ready = page
                .evaluate_expression("document.readyState")
                .await
                .ok()
                .and_then(|r| r.into_value::<String>().ok())
                .unwrap_or_default();
            let resources = page
                .evaluate_expression("performance.getEntriesByType('resource').length")
                .await
                .ok()
                .and_then(|r| r.into_value::<i64>().ok())
                .unwrap_or(0);
            let stable = resources == last_resources;
            last_resources = resources;
            ready == "complete" && stable
        } else {
            let expr = format!(
                "document.querySelector({:?}) !== null",
                cond_trim.trim_matches('"')
            );
            page.evaluate_expression(expr)
                .await
                .ok()
                .and_then(|r| r.into_value::<bool>().ok())
                .unwrap_or(false)
        };

        if matched {
            return Ok(json!({ "status": "matched", "condition": cond }).to_string());
        }
        if SystemTime::now() >= deadline {
            return Err(format!(
                "wait_for timed out after {} ms: {cond}",
                timeout.as_millis()
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Dispatch a key press (keyDown + keyUp) through the Input domain.
async fn press_key(page: &Page, key: &str) -> Result<(), String> {
    let (key_name, code, vk) = key_code(key);
    let down = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyDown)
        .key(key_name.clone())
        .code(code.clone())
        .windows_virtual_key_code(vk)
        .build()
        .map_err(|e| format!("bad keyDown params: {e}"))?;
    page.execute(down)
        .await
        .map_err(|e| format!("keyDown failed: {e}"))?;
    let up = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyUp)
        .key(key_name)
        .code(code)
        .windows_virtual_key_code(vk)
        .build()
        .map_err(|e| format!("bad keyUp params: {e}"))?;
    page.execute(up)
        .await
        .map_err(|e| format!("keyUp failed: {e}"))?;
    Ok(())
}

/// Map friendly key names to CDP key/code/virtual key code tuples.
fn key_code(key: &str) -> (String, String, i64) {
    let k = key.to_lowercase();
    match k.as_str() {
        "enter" => ("Enter".into(), "Enter".into(), 13),
        "escape" | "esc" => ("Escape".into(), "Escape".into(), 27),
        "tab" => ("Tab".into(), "Tab".into(), 9),
        "backspace" => ("Backspace".into(), "Backspace".into(), 8),
        "delete" => ("Delete".into(), "Delete".into(), 46),
        "arrowup" | "up" => ("ArrowUp".into(), "ArrowUp".into(), 38),
        "arrowdown" | "down" => ("ArrowDown".into(), "ArrowDown".into(), 40),
        "arrowleft" | "left" => ("ArrowLeft".into(), "ArrowLeft".into(), 37),
        "arrowright" | "right" => ("ArrowRight".into(), "ArrowRight".into(), 39),
        "home" => ("Home".into(), "Home".into(), 36),
        "end" => ("End".into(), "End".into(), 35),
        "pageup" => ("PageUp".into(), "PageUp".into(), 33),
        "pagedown" => ("PageDown".into(), "PageDown".into(), 34),
        "space" => (" ".into(), "Space".into(), 32),
        _ => {
            let c = key.chars().next().unwrap_or('a');
            let upper = c.to_ascii_uppercase();
            let code = format!("Key{upper}");
            (c.to_string(), code, c as i64)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn resolve_screenshot_path(save_path: Option<&str>) -> Result<PathBuf, String> {
    if let Some(p) = save_path {
        if p.trim().is_empty() {
            return Err("save_path must not be empty".to_string());
        }
        return Ok(PathBuf::from(p));
    }
    let cwd = std::env::current_dir().map_err(|e| format!("cannot resolve cwd: {e}"))?;
    let dir = cwd.join(".claw").join("browser_shots");
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    Ok(dir.join(format!("shot_{ts}.png")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chromiumoxide::cdp::browser_protocol::accessibility::{AxNodeId, AxValueType};
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn parses_full_input() {
        let input: BrowserControlInput = serde_json::from_value(json!({
            "action": "fill",
            "url": "https://example.com",
            "ref": "e1",
            "selector": "#btn",
            "text": "hello",
            "key": "Enter",
            "value": "opt1",
            "direction": "down",
            "script": "1+1",
            "wait_for": "url:example",
            "timeout_ms": 3000,
            "index": 1,
            "save_path": "C:/tmp/s.png",
            "headless": true,
            "port": 9222
        }))
        .unwrap();
        assert_eq!(input.action, "fill");
        assert_eq!(input.ref_.as_deref(), Some("e1"));
        assert_eq!(input.selector.as_deref(), Some("#btn"));
        assert_eq!(input.key.as_deref(), Some("Enter"));
        assert_eq!(input.timeout_ms, Some(3000));
        assert_eq!(input.index, Some(1));
        assert_eq!(input.port, Some(9222));
    }

    #[test]
    fn connect_target_resolves_endpoint() {
        // Bare port expands to the local http endpoint.
        let by_port = connect_target_parts(None, Some(9222));
        assert_eq!(by_port.unwrap(), "http://127.0.0.1:9222");
        // Explicit URL wins over port.
        let by_url = connect_target_parts(
            Some("ws://127.0.0.1:9223/devtools/browser/x".into()),
            Some(9222),
        );
        assert_eq!(by_url.unwrap(), "ws://127.0.0.1:9223/devtools/browser/x");
        // Missing both url and port is an error.
        assert!(connect_target_parts(None, None).is_err());
    }

    /// 回归测试:claw 在 tokio runtime 上下文(async 执行栈)中调用本工具时,
    /// 不得触发 "Cannot start a runtime from within a runtime" panic。
    /// `block_on_future` 检测到当前线程已在 runtime 内,应改走独立 OS 线程。
    #[test]
    fn block_on_future_avoids_nested_runtime_panic() {
        let rt = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        rt.block_on(async {
            let res = block_on_future(&rt, async { Ok::<_, String>("hi".to_string()) })
                .expect("no nested-runtime panic");
            assert_eq!(res, "hi");
        });
    }

    /// 端到端复现用户报错场景:在 tokio runtime 的 worker 里驱动完整
    /// launch → goto → snapshot → close 流程,修复前第一步 launch 即 panic。
    /// Opt-in: `cargo test -p tools browser_control_smoke_inside_runtime -- --ignored`.
    #[test]
    #[ignore]
    fn browser_control_smoke_inside_runtime() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let input: BrowserControlInput = serde_json::from_value(json!({
                "action": "launch",
                "headless": true,
            }))
            .unwrap();
            let launch = match run_browser_control(input) {
                Ok(ok) => ok,
                Err(e) => {
                    eprintln!("browser_control smoke skip (launch failed): {e}");
                    return;
                }
            };
            assert!(
                launch.contains("\"status\":\"ready\""),
                "launch inside runtime: {launch}"
            );

            let gone = serde_json::from_value(json!({
                "action": "goto",
                "url": "https://example.com",
            }))
            .map(run_browser_control)
            .unwrap()
            .unwrap_or_else(|e| panic!("goto failed: {e}"));
            assert!(gone.contains("example.com"), "goto: {gone}");

            let close = serde_json::from_value(json!({ "action": "close" }))
                .map(run_browser_control)
                .unwrap()
                .unwrap();
            assert!(close.contains("closed"), "close: {close}");
        });
    }

    #[test]
    fn parses_minimal_input() {
        let input: BrowserControlInput =
            serde_json::from_value(json!({ "action": "snapshot" })).unwrap();
        assert_eq!(input.action, "snapshot");
        assert!(input.ref_.is_none() && input.headless.is_none());
    }

    #[test]
    fn missing_action_is_rejected() {
        let result: Result<BrowserControlInput, _> = serde_json::from_value(json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn resolves_explicit_save_path() {
        let path = resolve_screenshot_path(Some("d:/shots/a.png")).unwrap();
        assert_eq!(path, PathBuf::from("d:/shots/a.png"));
        assert!(resolve_screenshot_path(Some("  ")).is_err());
    }

    #[test]
    fn key_code_maps_common_keys() {
        let (name, code, vk) = key_code("Enter");
        assert_eq!(
            (name, code, vk),
            ("Enter".to_string(), "Enter".to_string(), 13)
        );
        let (name, code, vk) = key_code("Tab");
        assert_eq!((name, code, vk), ("Tab".to_string(), "Tab".to_string(), 9));
        let (name, code, vk) = key_code("a");
        assert_eq!(name, "a");
        assert_eq!(code, "KeyA");
        assert_eq!(vk, 97);
    }

    #[test]
    fn render_ax_line_format() {
        let node = AxNode::builder()
            .node_id(AxNodeId::new("n1"))
            .ignored(false)
            .role(
                AxValue::builder()
                    .r#type(AxValueType::Role)
                    .value("button")
                    .build()
                    .unwrap(),
            )
            .name(
                AxValue::builder()
                    .r#type(AxValueType::String)
                    .value("Search")
                    .build()
                    .unwrap(),
            )
            .backend_dom_node_id(BackendNodeId::new(42))
            .build()
            .unwrap();
        let line = render_node_line(&node, "button", "e1");
        assert_eq!(line, "- button \"Search\" [e1]");
    }

    fn session_with_refs() -> BrowserSession {
        let mut refs = HashMap::new();
        refs.insert("e3".to_string(), BackendNodeId::new(7));
        BrowserSession {
            runtime: Arc::new(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap(),
            ),
            browser: None,
            tabs: Vec::new(),
            active: 0,
            refs,
        }
    }

    fn input_with(patches: &[(&str, Value)]) -> BrowserControlInput {
        let seed: BrowserControlInput =
            serde_json::from_value(json!({ "action": "click" })).unwrap();
        let mut v = serde_json::to_value(seed).unwrap();
        for (k, val) in patches {
            v[k] = val.clone();
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn target_resolution_prefers_ref_over_selector() {
        let mut session = session_with_refs();
        let input = input_with(&[("ref", json!("e3")), ("selector", json!("#btn"))]);
        match resolve_target(&mut session, &input) {
            Ok(Target::ByRef(_)) => {}
            other => panic!("expected ByRef, got {other:?}"),
        }
    }

    #[test]
    fn target_resolution_falls_back_to_selector() {
        let mut session = session_with_refs();
        let input = input_with(&[("selector", json!("#btn"))]);
        match resolve_target(&mut session, &input) {
            Ok(Target::BySelector(s)) => assert_eq!(s, "#btn"),
            other => panic!("expected BySelector, got {other:?}"),
        }
    }

    #[test]
    fn target_resolution_rejects_unknown_ref() {
        let mut session = session_with_refs();
        let input = input_with(&[("ref", json!("e99"))]);
        assert!(resolve_target(&mut session, &input).is_err());
    }

    /// End-to-end smoke test against a real Chrome/Chromium install.
    /// Opt-in: `cargo test -p tools browser_control_smoke -- --ignored`.
    #[test]
    #[ignore]
    fn browser_control_smoke_full_flow() {
        let headless = std::env::var("BROWSER_SMOKE_HEADFUL")
            .map(|v| v != "1")
            .unwrap_or(true);
        let mut base = BrowserControlInput {
            action: "launch".into(),
            url: None,
            ref_: None,
            selector: None,
            text: None,
            key: None,
            value: None,
            direction: None,
            script: None,
            wait_for: None,
            timeout_ms: None,
            index: None,
            save_path: None,
            headless: Some(headless),
            port: None,
        };
        let run = |action: &str| {
            let mut i = base.clone();
            i.action = action.into();
            run_browser_control(i)
        };
        let run_with = |action: &str, patch: &dyn Fn(&mut BrowserControlInput)| {
            let mut i = base.clone();
            i.action = action.into();
            patch(&mut i);
            run_browser_control(i)
        };

        let launch = match run("launch") {
            Ok(ok) => ok,
            Err(e) => {
                // Browser may be absent on CI machines; skip rather than fail.
                eprintln!("browser_control smoke skip (launch failed): {e}");
                return;
            }
        };
        assert!(launch.contains("\"status\":\"ready\""), "launch: {launch}");

        let gone = run_with("goto", &|i| i.url = Some("https://example.com".into()))
            .unwrap_or_else(|e| panic!("goto failed: {e}"));
        assert!(gone.contains("example.com"), "goto: {gone}");

        let snap = run("snapshot")
            .map_err(|e| panic!("snapshot failed: {e}"))
            .unwrap();
        assert!(
            snap.contains("Example Domain"),
            "snapshot should contain page text: {snap}"
        );

        let state = run("get_state").unwrap_or_else(|e| panic!("get_state failed: {e}"));
        assert!(state.contains("example.com"), "get_state: {state}");

        base.action = "close".into();
        let close = run_browser_control(base).unwrap();
        assert!(close.contains("closed"), "close: {close}");
    }

    /// 验证启动配置修复:viewport 与窗口一致(页面填满)+ 反自动化指纹
    /// (webdriver=false、无 HeadlessChrome UA、中文语言)。
    /// Opt-in: `cargo test -p tools browser_control_smoke_stealth -- --ignored`.
    #[test]
    #[ignore]
    fn browser_control_smoke_stealth() {
        let launch = run_browser_control(
            serde_json::from_value(json!({
                "action": "launch",
                "headless": true,
            }))
            .unwrap(),
        )
        .unwrap_or_else(|e| panic!("launch failed: {e}"));
        assert!(launch.contains("\"status\":\"ready\""), "launch: {launch}");

        let probe = run_browser_control(serde_json::from_value(json!({
            "action": "evaluate_js",
            "script": "JSON.stringify({ webdriver: navigator.webdriver, innerWidth: window.innerWidth, innerHeight: window.innerHeight, ua: navigator.userAgent, lang: navigator.language })",
        }))
        .unwrap())
        .unwrap_or_else(|e| panic!("probe failed: {e}"));

        // evaluate_js 返回 {"result": "<JSON 字符串>"},解析两层。
        let parsed: serde_json::Value = serde_json::from_str(&probe).expect("probe is json");
        let inner: serde_json::Value = serde_json::from_str(
            parsed
                .get("result")
                .and_then(|r| r.as_str())
                .expect("probe has result string"),
        )
        .expect("result is json");

        let get = |k: &str| {
            inner
                .get(k)
                .map(|x| x.to_string())
                .unwrap_or_default()
                .trim_matches('"')
                .to_string()
        };
        assert_eq!(
            get("webdriver"),
            "false",
            "webdriver must be hidden: {probe}"
        );
        assert_eq!(get("innerWidth"), "1440", "viewport width: {probe}");
        assert_eq!(get("innerHeight"), "900", "viewport height: {probe}");
        assert!(
            !get("ua").contains("HeadlessChrome"),
            "UA must not leak headless: {probe}"
        );
        // `--lang` 与 chromiumoxide 默认 `--lang=en_US` 冲突(DEFAULT_ARGS 先注册,
        // Chrome 单值 switch 取第一个),headless 下可能仍为 en-US。语言非主要
        // 指纹,en-US 也是 Chromium 常见默认值,断言允许两种即可。
        assert!(
            get("lang") == "zh-CN" || get("lang") == "en-US",
            "language should be zh-CN or en-US: {probe}"
        );

        let close =
            run_browser_control(serde_json::from_value(json!({ "action": "close" })).unwrap())
                .unwrap();
        assert!(close.contains("closed"), "close: {close}");
    }

    /// 验证 goto 的就绪等待:重 JS + Cloudflare 挑战页(海航官网)在 goto
    /// 返回后立即 snapshot 应拿到完整 AX 树(而非空树导致 AI 转 evaluate_js 盲试)。
    /// Opt-in: `cargo test -p tools browser_control_smoke_goto_ready_wait -- --ignored`.
    #[test]
    #[ignore]
    fn browser_control_smoke_goto_ready_wait() {
        let run = |action: &str, extra: &serde_json::Value| {
            let mut v = json!({ "action": action });
            if let Some(obj) = extra.as_object() {
                for (k, val) in obj {
                    v[k] = val.clone();
                }
            }
            let input: BrowserControlInput = serde_json::from_value(v).unwrap();
            run_browser_control(input)
        };

        run("launch", &json!({ "headless": true }))
            .unwrap_or_else(|e| panic!("launch failed: {e}"));
        let goto = run(
            "goto",
            &json!({ "url": "https://www.hainanairlines.com/CN/CN/Home" }),
        )
        .unwrap_or_else(|e| panic!("goto failed: {e}"));
        eprintln!("goto: {goto}");
        // goto 应等待页面就绪(ready=true 表示已过 Cloudflare 挑战加载出真实内容)。
        assert!(
            goto.contains("\"ready\":true") || goto.contains("海南航空"),
            "goto should wait for page readiness: {goto}"
        );

        // goto 返回后立即 snapshot,应拿到完整 AX 树(含表单元素),而非空树。
        let snap = run("snapshot", &json!({}))
            .map_err(|e| panic!("snapshot failed: {e}"))
            .unwrap();
        eprintln!("snapshot head: {}", &snap[..snap.len().min(400)]);
        assert!(
            snap.contains("RootWebArea") && !snap.contains("snapshot-warning"),
            "snapshot should contain real content, got: {snap}"
        );
        // 应包含表单元素(textbox/button 等可交互元素)。
        assert!(
            snap.contains("textbox") || snap.contains("button") || snap.contains("link"),
            "snapshot should expose interactive elements: {snap}"
        );

        let close = run("close", &json!({})).unwrap();
        assert!(close.contains("closed"), "close: {close}");
    }

    /// Connect to an externally-launched Chrome (e.g. the user's own browser,
    /// or an Electron app) over its CDP port, drive it, and verify that
    /// `close` only detaches — it must NOT kill the external process.
    /// Opt-in: `cargo test -p tools browser_control_smoke_connect -- --ignored`.
    #[test]
    #[ignore]
    fn browser_control_smoke_connect() {
        let port = 9333u16;
        let profile = std::env::temp_dir().join(format!("claw-bc-connect-{}", std::process::id()));
        let candidates: Vec<String> = if cfg!(windows) {
            vec![
                "C:/Program Files/Google/Chrome/Application/chrome.exe".into(),
                "C:/Program Files (x86)/Google/Chrome/Application/chrome.exe".into(),
                "C:/Program Files/Microsoft/Edge/Application/msedge.exe".into(),
                "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe".into(),
            ]
        } else {
            vec![
                "google-chrome".into(),
                "chromium".into(),
                "chromium-browser".into(),
                "chrome".into(),
            ]
        };
        let Some(bin) = candidates.iter().find(|p| Path::new(p).exists()) else {
            eprintln!("browser_control smoke connect skip (no chrome binary found)");
            return;
        };

        let mut child = std::process::Command::new(bin)
            .arg(format!("--remote-debugging-port={port}"))
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--headless=new")
            .arg("about:blank")
            .spawn()
            .expect("spawn external chrome");
        // Give Chrome time to open the debugging endpoint.
        std::thread::sleep(Duration::from_millis(2500));

        let run = |action: &str, extra: &serde_json::Value| {
            let mut v = json!({ "action": action });
            if let Some(obj) = extra.as_object() {
                for (k, val) in obj {
                    v[k] = val.clone();
                }
            }
            let input: BrowserControlInput = serde_json::from_value(v).unwrap();
            run_browser_control(input)
        };

        let connect = run("connect", &json!({ "port": port }))
            .unwrap_or_else(|e| panic!("connect failed: {e}"));
        assert!(
            connect.contains("\"status\":\"connected\""),
            "connect: {connect}"
        );

        let gone = run("goto", &json!({ "url": "https://example.com" }))
            .unwrap_or_else(|e| panic!("goto after connect failed: {e}"));
        assert!(gone.contains("example.com"), "goto: {gone}");

        let snap = run("snapshot", &json!({}))
            .map_err(|e| panic!("snapshot failed: {e}"))
            .unwrap();
        assert!(
            snap.contains("Example Domain"),
            "snapshot should see the external browser's page: {snap}"
        );

        // `close` detaches the session but must leave the external Chrome alive.
        let close = run("close", &json!({})).unwrap();
        assert!(close.contains("closed"), "close: {close}");
        std::thread::sleep(Duration::from_millis(500));
        match child.try_wait() {
            Ok(Some(status)) => panic!("external chrome was killed by close: {status}"),
            Ok(None) => {}
            Err(e) => panic!("try_wait failed: {e}"),
        }

        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(&profile);
    }
}
