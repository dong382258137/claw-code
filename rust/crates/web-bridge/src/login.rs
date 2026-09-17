//! 一次性浏览器登录：用本机 Edge/Chrome（独立用户目录 + CDP 调试端口）打开
//! chat.deepseek.com，用户登录后自动抓取 cookie 与 bearer 保存到 auth.json。
//!
//! 之后运行时不再需要浏览器——凭证直接调用网页内部 API。
//! 参考 openclaw-zero-token・MIT 的做法（Network.getAllCookies + users/current）。

use std::collections::HashMap;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as WsMsg;

use crate::deepseek_web::fetch_user_token;
use crate::web_session::{default_auth_path, WebAuth, DEFAULT_USER_AGENT};

fn find_chrome() -> Option<String> {
    if let Ok(p) = std::env::var("CHROME_PATH") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    const CANDIDATES: &[&str] = &[
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
    ];
    CANDIDATES
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|s| s.to_string())
}

fn pick_free_port() -> Result<u16, String> {
    use std::net::TcpListener;
    let l = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("bind 失败: {e}"))?;
    let port = l.local_addr().map_err(|e| e.to_string())?.port();
    drop(l);
    Ok(port)
}

fn spawn_browser(chrome: &str, port: u16, profile: &std::path::Path) -> Result<Child, String> {
    let mut cmd = Command::new(chrome);
    cmd.arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-session-crashed-bubble")
        .arg("https://chat.deepseek.com/");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd.spawn().map_err(|e| format!("启动浏览器失败: {e}"))
}

/// 等待 CDP 调试端口就绪并返回页面的 webSocketDebuggerUrl。
async fn wait_cdp_page_ws(port: u16) -> Result<String, String> {
    let list_url = format!("http://127.0.0.1:{port}/json");
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let Ok(resp) = reqwest::get(&list_url).await else {
            continue;
        };
        let Ok(list) = resp.json::<Vec<Value>>().await else {
            continue;
        };
        let target = list
            .iter()
            .find(|t| {
                t.get("type").and_then(|x| x.as_str()) == Some("page")
                    && t.get("url")
                        .and_then(|x| x.as_str())
                        .map(|u| u.contains("deepseek.com"))
                        .unwrap_or(false)
            })
            .or_else(|| {
                list.iter()
                    .find(|t| t.get("type").and_then(|x| x.as_str()) == Some("page"))
            });
        if let Some(t) = target {
            if let Some(ws) = t.get("webSocketDebuggerUrl").and_then(|x| x.as_str()) {
                return Ok(ws.to_string());
            }
        }
    }
    Err("CDP 调试端口未就绪（/json 无 page 目标）".to_string())
}

pub async fn run() -> Result<(), String> {
    let chrome = find_chrome().ok_or_else(|| {
        "未找到 Edge/Chrome。请设置 CHROME_PATH 环境变量，或用 `import` 手动粘贴 cookie。"
            .to_string()
    })?;
    let port = pick_free_port()?;
    let profile_dir = default_auth_path()
        .parent()
        .map(|p| p.join("edge-profile"))
        .unwrap_or_else(|| std::path::PathBuf::from("edge-profile"));
    let _ = std::fs::create_dir_all(&profile_dir);

    let mut browser = spawn_browser(&chrome, port, &profile_dir)?;
    println!("浏览器已打开，请在窗口里登录 chat.deepseek.com（扫码/密码均可）…");
    println!("登录完成后会自动抓取凭证并关闭浏览器。");

    let result = capture(port).await;
    kill_browser(&mut browser);
    result
}

type Pending = Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>>;

/// CDP 会话：发送命令（响应由后台 reader 投递到 pending 表）。
struct Cdp {
    tx: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        WsMsg,
    >,
    next_id: u64,
    pending: Pending,
}

impl Cdp {
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({ "id": id, "method": method, "params": params });
        self.tx
            .send(WsMsg::Text(msg.to_string()))
            .await
            .map_err(|e| format!("CDP 发送失败: {e}"))?;
        tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .map_err(|_| format!("CDP 等待超时: {method}"))?
            .map_err(|_| format!("CDP 响应通道关闭: {method}"))
    }
}

/// 后台 reader：把 ws 上按序到达的响应按 id 路由给等待方。
fn spawn_cdp_reader(
    rx: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    pending: Pending,
) {
    tokio::spawn(async move {
        let mut rx = rx;
        while let Some(Ok(msg)) = rx.next().await {
            let Ok(text) = msg.into_text() else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Some(id) = v.get("id").and_then(|x| x.as_u64()) else {
                continue;
            };
            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                let _ = tx.send(v);
            }
        }
    });
}

async fn capture(port: u16) -> Result<(), String> {
    let ws_url = wait_cdp_page_ws(port).await?;
    let (ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .map_err(|e| format!("连接 CDP 失败: {e}"))?;
    let (tx, rx) = ws.split();
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    spawn_cdp_reader(rx, pending.clone());
    let mut cdp = Cdp {
        tx,
        next_id: 0,
        pending,
    };

    let _ = cdp.call("Network.enable", json!({})).await;
    let _ = cdp.call("Runtime.enable", json!({})).await;

    println!("等待登录…（已登录则立即完成）");
    let mut waited = 0u32;
    loop {
        if waited > 150 {
            return Err("等待登录超时（5 分钟），请重试".to_string());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        waited += 1;

        // 1) 抓取 deepseek 域 cookie（会话/登录取证）
        let resp = cdp.call("Network.getAllCookies", json!({})).await?;
        let Some(cookies) = resp
            .pointer("/result/cookies")
            .and_then(|c| c.as_array())
            .cloned()
        else {
            continue;
        };
        let cookie = cookies
            .iter()
            .filter(|c| {
                c.get("domain")
                    .and_then(|d| d.as_str())
                    .map(|d| d.contains("deepseek.com"))
                    .unwrap_or(false)
            })
            .filter_map(|c| {
                Some(format!(
                    "{}={}",
                    c.get("name").and_then(|n| n.as_str())?,
                    c.get("value").and_then(|v| v.as_str())?
                ))
            })
            .collect::<Vec<_>>()
            .join("; ");

        // 2) 读页面 localStorage，找 token（官网升级后会话凭证挂在这里：
        //    key 含 token/auth，值可能是 JSON {token} 或纯字符串）
        let mut bearer = String::new();
        let ev = cdp
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": "(function(){var d={};for(var i=0;i<localStorage.length;i++){var k=localStorage.key(i);if(k)d[k]=localStorage.getItem(k);}return d;})()",
                    "returnByValue": true,
                }),
            )
            .await
            .ok();
        if let Some(ev) = ev {
            let store = ev.pointer("/result/result/value");
            if let Some(obj) = store.and_then(|o| o.as_object()) {
                // 诊断：列出疑似 token 的 key 概览（脱敏：仅 key 名/长度/前缀）
                let mut seen_short = String::new();
                for (k, v) in obj {
                    let lk = k.to_lowercase();
                    if lk.contains("token") || lk.contains("auth") {
                        let s = v.as_str().unwrap_or("").trim();
                        let p: String = s.chars().take(8).collect();
                        seen_short.push_str(&format!("[{}]len={} prefix={p:?} ", k, s.len()));
                    }
                }
                if !seen_short.is_empty() {
                    println!("  - localStorage token 类 key 概览: {seen_short}");
                }
                for (k, v) in obj {
                    let lk = k.to_lowercase();
                    if !(lk.contains("token") || lk.contains("auth")) {
                        continue;
                    }
                    let Some(s) = v.as_str() else { continue };
                    let s = s.trim();
                    if s.is_empty() || s == "undefined" || s == "null" {
                        continue;
                    }
                    if lk == "usertoken" {
                        // 精确命中官网最常用 key：值形如 {"value":"..."}
                        if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                            if let Some(t) = parsed
                                .get("value")
                                .or_else(|| parsed.get("token"))
                                .and_then(|x| x.as_str())
                            {
                                let t = t.trim();
                                if !t.is_empty() && t != "null" && t != "undefined" {
                                    bearer = t.to_string();
                                    break;
                                }
                            }
                        }
                        bearer = s.to_string();
                        break;
                    }
                    // 值可能是 {"token"/"value": "..."} 之类 JSON
                    if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                        let t = parsed
                            .get("token")
                            .or_else(|| parsed.get("value"))
                            .and_then(|x| x.as_str());
                        if let Some(t) = t {
                            let t = t.trim();
                            if !t.is_empty() {
                                bearer = t.to_string();
                                break;
                            }
                        }
                    }
                    if bearer.is_empty() {
                        bearer = s.to_string();
                    }
                }
            }
        }

        if cookie.is_empty() && bearer.is_empty() {
            println!("  - 尚未获取到登录态（无 cookie / 无 token），请完成登录…");
            continue;
        }

        // 3) token 兜底：users/current 换 token。
        if bearer.is_empty() && !cookie.is_empty() {
            let base = std::env::var("DEEPSEEK_WEB_BASE")
                .unwrap_or_else(|_| "https://chat.deepseek.com".to_string());
            match fetch_user_token(&base, &cookie) {
                Ok(b) => bearer = b,
                Err(e) => {
                    println!("  - 已抓到 cookie 但换 token 失败({e})，等待登录…");
                    continue;
                }
            }
        }

        if bearer.is_empty() {
            println!("  - 尚未拿到 userToken，请确认页面已登录…");
            continue;
        }

        let auth = WebAuth {
            cookie,
            bearer,
            user_agent: DEFAULT_USER_AGENT.to_string(),
            captured_at: Some(chrono::Local::now().to_rfc3339()),
        };
        let path = auth.save()?;
        println!("凭证已保存: {}", path.display());
        println!("现在可用 `claw-web-bridge run \"任务\"`（本窗口可关闭）。");
        return Ok(());
    }
}

fn kill_browser(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
