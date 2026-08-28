//! Browser control via Chrome DevTools Protocol (chromiumoxide).
//!
//! Provides the `browser_control` tool: launch a persistent Chrome session,
//! navigate, snapshot the page text, take screenshots and interact with
//! elements (click / type). The session lives for the whole process so
//! consecutive calls share the same tab.

use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::{Page, ScreenshotParams};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;

/// Input schema for the `browser_control` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct BrowserControlInput {
    /// One of: launch | goto | snapshot | screenshot | click | type | close
    pub action: String,
    /// URL used by `launch` / `goto`.
    pub url: Option<String>,
    /// CSS selector used by `click` / `type`.
    pub selector: Option<String>,
    /// Text typed by `type`.
    pub text: Option<String>,
    /// Output file path for `screenshot`; defaults to `<cwd>/.claw/browser_shots/<timestamp>.png`.
    pub save_path: Option<String>,
    /// Launch headless (no visible window). Defaults to false (visible window).
    pub headless: Option<bool>,
}

/// One persistent browser session per process.
struct BrowserSession {
    runtime: tokio::runtime::Runtime,
    browser: Browser,
    page: Option<Page>,
}

static SESSION: OnceLock<Mutex<Option<BrowserSession>>> = OnceLock::new();

/// Main entry point used by the tool executor.
pub fn run_browser_control(input: BrowserControlInput) -> Result<String, String> {
    let mutex = SESSION.get_or_init(|| Mutex::new(None));
    let mut guard = mutex
        .lock()
        .map_err(|_| "browser session lock poisoned".to_string())?;

    if input.action == "close" {
        guard.take(); // Browser's Drop impl kills the child process.
        return Ok(json!({ "status": "closed" }).to_string());
    }

    if guard.is_none() {
        let headless = input.headless.unwrap_or(false);
        *guard = Some(create_session(headless)?);
    }
    let session = guard.as_mut().expect("session just ensured");
    // Disjoint field borrows keep the runtime and browser borrows apart so the
    // async dispatch can reborrow the page slot mutably.
    let runtime = &session.runtime;
    let browser = &session.browser;
    let page = &mut session.page;
    runtime.block_on(dispatch(browser, page, &input))
}

fn create_session(headless: bool) -> Result<BrowserSession, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to start tokio runtime: {e}"))?;

    let (browser, handler) = runtime.block_on(async move {
        let builder = BrowserConfig::builder().window_size(1280, 900);
        let config = if headless {
            builder.new_headless_mode().build()
        } else {
            builder.with_head().build()
        };
        let config = config.map_err(|e| format!("invalid browser config: {e}"))?;
        Browser::launch(config).await.map_err(|e| e.to_string())
    })?;

    // Drive the CDP handler in the background so the connection stays alive
    // between tool calls.
    runtime.spawn(async move {
        let mut handler = handler;
        while let Some(event) = handler.next().await {
            let _ = event;
        }
        // Keep the task alive until the runtime shuts down.
        std::future::pending::<()>().await;
    });

    Ok(BrowserSession {
        runtime,
        browser,
        page: None,
    })
}

async fn dispatch(
    browser: &Browser,
    page_slot: &mut Option<Page>,
    input: &BrowserControlInput,
) -> Result<String, String> {
    match input.action.as_str() {
        "launch" => {
            let page = ensure_page(browser, page_slot).await?;
            let url = page.url().await.ok().flatten().unwrap_or_default();
            Ok(json!({ "status": "ready", "url": url }).to_string())
        }
        "goto" => {
            let url = input
                .url
                .clone()
                .ok_or_else(|| "action 'goto' requires 'url'".to_string())?;
            let page = ensure_page(browser, page_slot).await?;
            page.goto(url.clone())
                .await
                .map_err(|e| format!("navigate failed: {e}"))?;
            let _ = page.wait_for_navigation().await;
            // Give client-side rendering a moment before snapshotting.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let current = page.url().await.ok().flatten().unwrap_or(url);
            Ok(json!({ "status": "navigated", "url": current }).to_string())
        }
        "snapshot" => {
            let page = ensure_page(browser, page_slot).await?;
            let url = page.url().await.ok().flatten().unwrap_or_default();
            let title = page.get_title().await.ok().flatten().unwrap_or_default();
            let text = eval_string(page, "document.body ? document.body.innerText : ''").await;
            let mut text = text.unwrap_or_default();
            const MAX_TEXT: usize = 8000;
            let truncated = text.chars().count() > MAX_TEXT;
            if truncated {
                text = text.chars().take(MAX_TEXT).collect();
            }
            Ok(json!({
                "url": url,
                "title": title,
                "text": text,
                "textTruncated": truncated
            })
            .to_string())
        }
        "screenshot" => {
            let page = ensure_page(browser, page_slot).await?;
            let bytes = page
                .screenshot(ScreenshotParams::builder().build())
                .await
                .map_err(|e| format!("screenshot failed: {e}"))?;
            let path = resolve_screenshot_path(input.save_path.as_deref())?;
            fs::write(&path, &bytes)
                .map_err(|e| format!("failed to write screenshot {}: {e}", path.display()))?;
            Ok(json!({ "path": path.display().to_string(), "bytes": bytes.len() }).to_string())
        }
        "click" => {
            let page = ensure_page(browser, page_slot).await?;
            let selector = input
                .selector
                .clone()
                .ok_or_else(|| "action 'click' requires 'selector'".to_string())?;
            let element = page
                .find_element(&selector)
                .await
                .map_err(|e| format!("element not found ({selector}): {e}"))?;
            element
                .click()
                .await
                .map_err(|e| format!("click failed: {e}"))?;
            Ok(json!({ "status": "clicked", "selector": selector }).to_string())
        }
        "type" => {
            let page = ensure_page(browser, page_slot).await?;
            let selector = input
                .selector
                .clone()
                .ok_or_else(|| "action 'type' requires 'selector'".to_string())?;
            let text = input
                .text
                .clone()
                .ok_or_else(|| "action 'type' requires 'text'".to_string())?;
            let element = page
                .find_element(&selector)
                .await
                .map_err(|e| format!("element not found ({selector}): {e}"))?;
            element
                .click()
                .await
                .map_err(|e| format!("focus failed: {e}"))?;
            element
                .type_str(&text)
                .await
                .map_err(|e| format!("type failed: {e}"))?;
            Ok(json!({ "status": "typed", "selector": selector, "chars": text.chars().count() })
                .to_string())
        }
        other => Err(format!("unsupported browser_control action: {other}")),
    }
}

async fn ensure_page<'a>(
    browser: &Browser,
    page_slot: &'a mut Option<Page>,
) -> Result<&'a Page, String> {
    if page_slot.is_none() {
        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|e| format!("failed to open a tab: {e}"))?;
        *page_slot = Some(page);
    }
    Ok(page_slot.as_ref().expect("page just ensured"))
}

async fn eval_string(page: &Page, expression: &str) -> Result<String, String> {
    let result = page
        .evaluate_expression(expression)
        .await
        .map_err(|e| format!("evaluate failed ({expression}): {e}"))?;
    result
        .into_value::<String>()
        .map_err(|e| format!("evaluation of {expression} did not return a string: {e}"))
}

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
    use serde_json::json;

    #[test]
    fn parses_full_input() {
        let input: BrowserControlInput = serde_json::from_value(json!({
            "action": "goto",
            "url": "https://example.com",
            "selector": "#btn",
            "text": "hello",
            "save_path": "C:/tmp/s.png",
            "headless": true
        }))
        .unwrap();
        assert_eq!(input.action, "goto");
        assert_eq!(input.url.as_deref(), Some("https://example.com"));
        assert_eq!(input.headless, Some(true));
    }

    #[test]
    fn parses_minimal_input() {
        let input: BrowserControlInput =
            serde_json::from_value(json!({ "action": "snapshot" })).unwrap();
        assert_eq!(input.action, "snapshot");
        assert!(input.url.is_none() && input.headless.is_none());
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
    fn register_schema_in_mvp_specs_has_browser_control() {
        let specs = crate::mvp_tool_specs();
        let spec = specs
            .iter()
            .find(|s| s.name == "browser_control")
            .expect("browser_control spec must be registered");
        assert_eq!(
            spec.required_permission,
            runtime::PermissionMode::DangerFullAccess
        );
        assert!(spec.description.contains("snapshot"));
    }

    /// End-to-end smoke test against a real Chrome/Chromium install.
    /// Opt-in: `cargo test -p tools browser_control_smoke -- --ignored`.
    #[test]
    #[ignore]
    fn browser_control_smoke_full_flow() {
        let headless = match std::env::var("BROWSER_SMOKE_HEADFUL") {
            Ok(v) if v == "1" => false,
            _ => true,
        };
        let launch = run_browser_control(BrowserControlInput {
            action: "launch".into(),
            url: None,
            selector: None,
            text: None,
            save_path: None,
            headless: Some(headless),
        });
        let launch = match launch {
            Ok(ok) => ok,
            Err(e) => {
                // Browser may be absent on CI machines; skip rather than fail.
                eprintln!("browser_control smoke skip (launch failed): {e}");
                return;
            }
        };
        assert!(launch.contains("\"status\":\"ready\""), "launch: {launch}");

        let gone = run_browser_control(BrowserControlInput {
            action: "goto".into(),
            url: Some("https://example.com".into()),
            selector: None,
            text: None,
            save_path: None,
            headless: None,
        })
        .unwrap_or_else(|e| panic!("goto failed: {e}"));
        assert!(gone.contains("example.com"), "goto: {gone}");

        let snap = run_browser_control(BrowserControlInput {
            action: "snapshot".into(),
            url: None,
            selector: None,
            text: None,
            save_path: None,
            headless: None,
        })
        .map_err(|e| panic!("snapshot failed: {e}"))
        .unwrap();
        assert!(
            snap.contains("Example Domain"),
            "snapshot should contain page text: {snap}"
        );

        let close = run_browser_control(BrowserControlInput {
            action: "close".into(),
            url: None,
            selector: None,
            text: None,
            save_path: None,
            headless: None,
        })
        .unwrap();
        assert!(close.contains("closed"), "close: {close}");
    }
}