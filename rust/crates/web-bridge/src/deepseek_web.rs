//! DeepSeek 网页版模型后端：登录态直连 `chat.deepseek.com` 内部 API。
//!
//! 链路（端点/请求体/头参考 openclaw-zero-token・MIT）：
//! `create_pow_challenge` → 求解 PoW → `chat/completion`（SSE）→ 完整回复文本。
//!
//! SSE 提取器移植自 `references/cuckoo-code/src/providers/deepseek.js` 的
//! `observeBody/createExtractor`（已在 Electron 壳中线上验证），以服务端
//! `FINISHED` 状态作为完成信号——不再依赖 DOM 稳定判定。

use std::time::Duration;

use serde_json::{json, Value};

use crate::pow::{self, PowChallenge};
use crate::web_session::{WebAuth, DEFAULT_USER_AGENT};
use crate::WebExchange;

const BASE_URL: &str = "https://chat.deepseek.com";

/// 可选文件日志(`CLAW_WEB_LOG_FILE`):TUI 下 stderr 被 StderrGuard 重定向,
/// tracing::info 不可见;排查网络卡点时把关键阶段落盘。
pub(crate) fn debug_log(msg: &str) {
    if let Ok(path) = std::env::var("CLAW_WEB_LOG_FILE") {
        use std::io::Write;
        let t = chrono::Local::now().format("%H:%M:%S%.3f");
        let th = std::thread::current().name().unwrap_or("?").to_string();
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(f, "{t} [{th}] {msg}");
        }
    }
}

// ========== SSE 帧解码（与 deepseek.js createFrameDecoder 等价） ==========

struct FrameDecoder {
    buffer: String,
    scan_from: usize,
}

impl FrameDecoder {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            scan_from: 0,
        }
    }

    /// JS 的 `/\r?\n\r?\n/g` 等价处理：先把 `\r\n` 归一为 `\n` 再按 `\n\n` 切帧；
    /// 最后一个 `\n\n` 之后的不完整帧留在 buffer，等 finish() 时整体输出。
    fn push(&mut self, text: &str) -> Vec<String> {
        self.buffer.push_str(&text.replace("\r\n", "\n"));
        let mut frames = Vec::new();
        let mut offset = 0usize;
        let hay = self.buffer.as_str();
        let mut start = offset;
        while let Some(rel) = hay[start..].find("\n\n") {
            let end = start + rel;
            frames.push(hay[offset..end].to_string());
            offset = end + 2;
            start = offset;
        }
        self.buffer = hay[offset..].to_string();
        self.scan_from = 0;
        frames
    }

    fn finish(mut self) -> Vec<String> {
        let rest = std::mem::take(&mut self.buffer);
        if rest.trim().is_empty() {
            Vec::new()
        } else {
            vec![rest]
        }
    }
}

/// 解析单个 SSE 块（收集所有 `data:` 行后 JSON 解析，等价 parseBlock）。
fn parse_block(block: &str) -> Option<Value> {
    let mut data: Option<String> = None;
    for line in block.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(v) = line.strip_prefix("data:") {
            let d = v.trim();
            data = Some(match data {
                None => d.to_string(),
                Some(prev) => format!("{prev}\n{d}"),
            });
        }
    }
    let text = data?;
    serde_json::from_str(&text).ok()
}

// ========== 回复提取器（移植 deepseek.js createExtractor） ==========

struct Extractor {
    fragment_types: Vec<String>,
    current_index: i32,
    observed: bool,
    text: String,
    finished: bool,
}

fn last_seg(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or("")
}

fn is_think(t: &str) -> bool {
    t.eq_ignore_ascii_case("think")
}

impl Extractor {
    fn new() -> Self {
        Self {
            fragment_types: Vec::new(),
            current_index: -1,
            observed: false,
            text: String::new(),
            finished: false,
        }
    }

    fn type_at(&self, slot: i32) -> Option<&str> {
        let i = if slot == -1 { self.current_index } else { slot };
        if i < 0 {
            return None;
        }
        self.fragment_types.get(i as usize).map(|s| s.as_str())
    }

    fn is_fragments_append(v: &Value) -> bool {
        let p = v.get("p").and_then(|x| x.as_str()).unwrap_or("");
        p.ends_with("/fragments")
            && v.get("o").and_then(|x| x.as_str()) == Some("APPEND")
            && v.get("v").map(|x| x.is_array()).unwrap_or(false)
    }

    fn snapshot_fragments(v: &Value) -> Option<Vec<Value>> {
        let r = v.get("v")?.get("response")?;
        let f = r.get("fragments")?;
        if !f.is_array() {
            return None;
        }
        let arr = f.as_array()?;
        if arr.is_empty() {
            None
        } else {
            Some(arr.clone())
        }
    }

    fn frag_text(f: &Value) -> String {
        f.get("content")
            .and_then(|x| x.as_str())
            .or_else(|| f.get("text").and_then(|x| x.as_str()))
            .unwrap_or("")
            .to_string()
    }

    fn is_response_text_patch(v: &Value) -> bool {
        let p = v.get("p").and_then(|x| x.as_str()).unwrap_or("");
        let s = last_seg(p);
        (s == "content" || s == "text" || s == "markdown" || s == "delta")
            && (p == "response" || p.starts_with("response/"))
    }

    fn is_thinking_patch(v: &Value) -> bool {
        let s = last_seg(v.get("p").and_then(|x| x.as_str()).unwrap_or(""));
        s == "reasoning_content" || s == "thinking_content"
    }

    fn consume(&mut self, parsed: &Value) {
        if !parsed.is_object() {
            return;
        }
        if parsed.get("o").and_then(|x| x.as_str()) == Some("BATCH") {
            if let Some(arr) = parsed.get("v").and_then(|x| x.as_array()) {
                for item in arr {
                    self.consume(item);
                }
            }
            return;
        }
        // 片段类型登记
        if Self::is_fragments_append(parsed) {
            let arr = parsed["v"].as_array().unwrap_or(&Vec::new()).to_vec();
            let types: Vec<String> = arr
                .iter()
                .map(|f| String::from(f.get("type").and_then(|x| x.as_str()).unwrap_or("RESPONSE")))
                .collect();
            self.current_index = (self.fragment_types.len() + types.len()) as i32 - 1;
            self.fragment_types.extend(types);
            self.observed = true;
            let n = self.fragment_types.len();
            for (i, f) in arr.iter().enumerate() {
                let idx = n - arr.len() + i;
                if !is_think(
                    self.fragment_types
                        .get(idx)
                        .map(|s| s.as_str())
                        .unwrap_or(""),
                ) {
                    self.text.push_str(&Self::frag_text(f));
                }
            }
            return;
        }
        if let Some(snap) = Self::snapshot_fragments(parsed) {
            let first = !self.observed;
            let types: Vec<String> = snap
                .iter()
                .map(|f| String::from(f.get("type").and_then(|x| x.as_str()).unwrap_or("RESPONSE")))
                .collect();
            self.current_index = types.len() as i32 - 1;
            self.fragment_types = types;
            self.observed = true;
            if first {
                for f in &snap {
                    self.text.push_str(&Self::frag_text(f));
                }
            }
            return;
        }
        if Self::is_thinking_patch(parsed) {
            return;
        }
        if Self::is_response_text_patch(parsed) {
            if let Some(s) = parsed.get("v").and_then(|x| x.as_str()) {
                let p = parsed["p"].as_str().unwrap_or("");
                let idx = regex_slot(p);
                let t = self.type_at(idx as i32);
                if !t.map(is_think).unwrap_or(false) {
                    self.text.push_str(s);
                }
            }
            return;
        }
        if parsed.get("p").is_none() {
            if let Some(s) = parsed.get("v").and_then(|x| x.as_str()) {
                let t = self.type_at(-1);
                if !t.map(is_think).unwrap_or(false) {
                    self.text.push_str(s);
                }
            }
            return;
        }
        if matches!(
            parsed.get("p").and_then(|x| x.as_str()),
            Some("response/status") | Some("quasi_status")
        ) && parsed.get("v").and_then(|x| x.as_str()) == Some("FINISHED")
        {
            self.finished = true;
        }
    }
}

/// 从 `response/fragments/<idx>/...` 提取片段槽位。
fn regex_slot(p: &str) -> i64 {
    let rest = p.strip_prefix("response/fragments/").unwrap_or("");
    let num: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    num.parse().unwrap_or(-1)
}

// ========== HTTP 客户端 ==========

// ========== 请求原语（在专用线程内运行，无 tokio 上下文） ==========

fn build_headers(auth: &WebAuth, extra: Vec<(&'static str, String)>) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (k, v) in auth.headers() {
        let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
            .unwrap_or_else(|e| panic!("invalid header name {k:?}: {e}"));
        if let Ok(val) = reqwest::header::HeaderValue::from_str(&v) {
            map.insert(name, val);
        }
    }
    for (k, v) in extra {
        let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
            .unwrap_or_else(|e| panic!("invalid extra header name {k:?}: {e}"));
        if let Ok(val) = reqwest::header::HeaderValue::from_str(&v) {
            map.insert(name, val);
        }
    }
    map
}

fn post_json(
    http: &reqwest::blocking::Client,
    auth: &WebAuth,
    base: &str,
    path: &str,
    body: Value,
    extra: Vec<(&'static str, String)>,
) -> Result<reqwest::blocking::Response, String> {
    http.post(format!("{base}{path}"))
        .headers(build_headers(auth, extra))
        .json(&body)
        .send()
        .map_err(|e| format!("请求失败 {path}: {e}"))
}

fn create_pow_challenge(
    http: &reqwest::blocking::Client,
    auth: &WebAuth,
    base: &str,
    target_path: &str,
) -> Result<PowChallenge, String> {
    let resp = post_json(
        http,
        auth,
        base,
        "/api/v0/chat/create_pow_challenge",
        json!({ "target_path": target_path }),
        vec![],
    )?;
    let status = resp.status();
    let text = resp.text().map_err(|e| format!("读取 PoW 响应失败: {e}"))?;
    if !status.is_success() {
        return Err(format!("create_pow_challenge 返回 {status}: {text}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("PoW JSON 解析失败: {e}"))?;
    let ch: PowChallenge = v
        .pointer("/data/biz_data/challenge")
        .or_else(|| v.pointer("/data/challenge"))
        .or_else(|| v.get("challenge"))
        .cloned()
        .and_then(|c| serde_json::from_value(c).ok())
        .ok_or_else(|| format!("PoW 响应缺少 challenge: {text}"))?;
    Ok(ch)
}

fn create_chat_session(
    http: &reqwest::blocking::Client,
    auth: &WebAuth,
    base: &str,
) -> Result<String, String> {
    let resp = post_json(
        http,
        auth,
        base,
        "/api/v0/chat_session/create",
        json!({}),
        vec![],
    )?;
    let status = resp.status();
    let text = resp.text().map_err(|e| format!("读取会话响应失败: {e}"))?;
    if !status.is_success() {
        return Err(format!("chat_session/create 返回 {status}: {text}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("会话 JSON 解析失败: {e}"))?;
    let biz = v
        .pointer("/data/biz_data")
        .ok_or_else(|| format!("缺少 biz_data: {text}"))?;
    let id = biz
        .get("id")
        .or_else(|| biz.get("chat_session_id"))
        .and_then(|x| x.as_str())
        .ok_or_else(|| format!("缺少会话 id: {text}"))?;
    Ok(id.to_string())
}

/// 单轮：确保会话 → PoW → chat/completion（SSE FINISHED）→ 完整回复文本。
fn completion_round(
    http: &reqwest::blocking::Client,
    auth: &WebAuth,
    base: &str,
    session_slot: &mut Option<String>,
    prompt: &str,
) -> Result<String, String> {
    debug_log(&format!("round_trip 开始 (prompt {} chars)", prompt.len()));
    let session_id = match session_slot {
        Some(s) => s.clone(),
        None => {
            let id = create_chat_session(http, auth, base)?;
            tracing::info!("deepseek web: created chat session {id}");
            debug_log(&format!("会话创建成功 {id}"));
            *session_slot = Some(id.clone());
            id
        }
    };

    let target_path = "/api/v0/chat/completion";
    let challenge = create_pow_challenge(http, auth, base, target_path)?;
    tracing::info!("PoW challenge: {challenge:?}");
    debug_log(&format!("PoW challenge 拿到: {challenge:?}"));
    let pow_header = pow::solve_to_header(&challenge, target_path)?;
    tracing::info!("PoW header: {pow_header}");
    debug_log("PoW 求解完成");

    let body = json!({
        "chat_session_id": session_id,
        "parent_message_id": serde_json::Value::Null,
        "prompt": prompt,
        "ref_file_ids": [],
        "thinking_enabled": true,
        "search_enabled": true,
        "preempt": false,
    });
    debug_log("发送 chat/completion 请求...");
    let resp = post_json(
        http,
        auth,
        base,
        target_path,
        body,
        vec![("x-ds-pow-response", pow_header)],
    )?;
    let status = resp.status();
    debug_log(&format!("chat/completion 响应状态: {status}"));
    if !status.is_success() {
        let text = resp.text().unwrap_or_default();
        return Err(format!("chat/completion 返回 {status}: {text}"));
    }
    let text = resp.text().map_err(|e| format!("读取流失败: {e}"))?;
    debug_log(&format!("SSE 流读取完成 ({} chars)", text.len()));
    // 可选 SSE 原始帧落盘(`CLAW_WEB_DUMP_SSE`):排查 extractor 丢帧 vs
    // 模型真实输出截断(两者在 debug_log 中表现相同,需原始帧区分)。
    if let Ok(path) = std::env::var("CLAW_WEB_DUMP_SSE") {
        let stamp = chrono::Local::now().format("%H%M%S%.3f");
        let dump = format!("{path}.{stamp}.sse");
        if let Err(e) = std::fs::write(&dump, &text) {
            debug_log(&format!("SSE dump 写入失败: {e}"));
        } else {
            debug_log(&format!("SSE 原始帧已落盘: {dump}"));
        }
    }
    // 诊断：记录原始响应开头（区分 SSE 流 / 错误 JSON / 空响应）
    tracing::info!("chat/completion 原始响应头: {}", truncate(&text, 600));
    if !text.starts_with("event:") && !text.contains("\ndata:") && !text.starts_with("data:") {
        // 非 SSE 形态，多半是 JSON 错误体或新协议，直接报出来
        return Err(format!(
            "chat/completion 响应非 SSE 流，前600字: {}",
            truncate(&text, 600)
        ));
    }
    let reply = extract_reply(text.as_bytes());
    debug_log(&format!("extract 完成 ({} chars)", reply.len()));
    Ok(reply)
}

/// 把 SSE 字节流喂给提取器，返回服务端 FINISHED 时的完整回复文本。
fn extract_reply(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut decoder = FrameDecoder::new();
    let mut extractor = Extractor::new();
    for frame in decoder.push(&text) {
        if let Some(v) = parse_block(&frame) {
            extractor.consume(&v);
        }
    }
    for frame in decoder.finish() {
        if let Some(v) = parse_block(&frame) {
            extractor.consume(&v);
        }
    }
    extractor.text
}

/// 一次性隔离线程里取 token（避免阻塞 Client 在异步上下文构造/析构 panic）。
pub fn fetch_user_token(base: &str, cookie: &str) -> Result<String, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let base = base.to_string();
    let cookie = cookie.to_string();
    std::thread::spawn(move || {
        let client = reqwest::blocking::Client::new();
        let result = client
            .get(format!("{base}/api/v0/users/current"))
            .headers(build_headers(
                &WebAuth {
                    cookie,
                    bearer: String::new(),
                    user_agent: DEFAULT_USER_AGENT.to_string(),
                    captured_at: None,
                },
                vec![],
            ))
            .send()
            .map_err(|e| format!("users/current 请求失败: {e}"))
            .and_then(|resp| {
                let status = resp.status();
                let text = resp.text().unwrap_or_default();
                if !status.is_success() {
                    return Err(format!(
                        "users/current 返回 {status}，响应体前300字: {}",
                        truncate(&text, 300)
                    ));
                }
                let v: Value = serde_json::from_str(&text).map_err(|e| {
                    format!(
                        "users/current JSON 解析失败({e})，响应体前300字: {}",
                        truncate(&text, 300)
                    )
                })?;
                v.pointer("/data/biz_data/token")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
                    .ok_or_else(|| {
                        format!(
                            "users/current 响应缺少 token，响应体前300字: {}",
                            truncate(&text, 300)
                        )
                    })
            });
        let _ = tx.send(result);
    });
    rx.recv_timeout(Duration::from_secs(30))
        .map_err(|e| format!("等待 users/current 响应失败: {e}"))?
}

fn truncate(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}

// ========== 会话客户端（worker 线程模型） ==========

enum WorkerJob {
    Chat {
        prompt: String,
        reply: std::sync::mpsc::Sender<Result<String, String>>,
    },
}

/// DeepSeek 网页会话客户端。
///
/// `reqwest::blocking` 的运行时**不能在 tokio 异步上下文**中构造或析构
/// （会 panic），因此所有 HTTP 调用被隔离到本结构自带的专用 std 线程里
/// 执行；对外 `round_trip` 仍是同步接口，经 mpsc 通道收发。
pub struct DeepseekWebClient {
    tx: std::sync::mpsc::Sender<WorkerJob>,
}

impl DeepseekWebClient {
    pub fn new(auth: WebAuth) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<WorkerJob>();
        std::thread::Builder::new()
            .name("deepseek-web-worker".to_string())
            .spawn(move || {
                let base =
                    std::env::var("DEEPSEEK_WEB_BASE").unwrap_or_else(|_| BASE_URL.to_string());
                // 专用线程内构造阻塞客户端：该线程无 tokio 上下文，安全。
                let http = reqwest::blocking::Client::builder()
                    .user_agent("Mozilla/5.0")
                    .build()
                    .expect("http client build");
                let mut session_slot: Option<String> = None;
                while let Ok(job) = rx.recv() {
                    match job {
                        WorkerJob::Chat { prompt, reply } => {
                            let result =
                                completion_round(&http, &auth, &base, &mut session_slot, &prompt);
                            let _ = reply.send(result);
                        }
                    }
                }
            })
            .expect("spawn worker thread");
        Self { tx }
    }
}

impl WebExchange for DeepseekWebClient {
    fn round_trip(&mut self, prompt: &str) -> Result<String, String> {
        tracing::info!("deepseek web: round_trip prompt ({} chars)", prompt.len());
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(WorkerJob::Chat {
                prompt: prompt.to_string(),
                reply: reply_tx,
            })
            .map_err(|e| format!("deepseek worker 已退出: {e}"))?;
        let reply = match reply_rx.recv_timeout(Duration::from_secs(300)) {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                debug_log(&format!("worker 返回错误: {e}"));
                return Err(e);
            }
            Err(mpsc_recv_timeout) => {
                debug_log(&format!("300s 等待网页回复超时: {mpsc_recv_timeout}"));
                return Err(format!("等待网页回复超时: {mpsc_recv_timeout}"));
            }
        };
        let preview: String = reply.chars().take(120).collect();
        tracing::info!(
            "deepseek web: reply ({} chars) preview: {preview}",
            reply.len()
        );
        Ok(reply)
    }
}

// ========== 测试 ==========

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extractor_parses_finished_batch() {
        // 仿真实响应：THINK 片段不进入正文，status FINISHED 置完成信号。
        let sse = concat!(
            "event: status\ndata: {\"p\":\"response/status\",\"v\":\"RUNNING\"}\n\n",
            "event: data\ndata: {\"o\":\"BATCH\",\"v\":[{\"p\":\"response/fragments/0/content\",\"v\":\"hello\"},",
            "{\"p\":\"response/fragments/1/reasoning_content\",\"v\":\"hidden\"}]}\n\n",
            "event: data\ndata: {\"o\":\"BATCH\",\"v\":[{\"p\":\"response/fragments/0/content\",\"v\":\" world\"}]}\n\n",
            "event: data\ndata: {\"p\":\"response/status\",\"v\":\"FINISHED\"}\n\n",
        );
        let text = extract_reply(sse.as_bytes());
        assert_eq!(text, "hello world");
    }
}
