//! DeepSeek 网页会话凭证存储：auth.json（cookie + bearer + userAgent）。
//!
//! 凭证来源参考 openclaw-zero-token（MIT）的 deepseek-web-auth：
//! - cookie：浏览器登录态（d_id= / ds_session_id= / HWSID= 等）
//! - bearer：GET /api/v0/users/current 返回的 data.data.biz_data.token
//! - userAgent：发起请求时伪造的浏览器 UA

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 默认浏览器 UA（与 DeepSeek 前端一致，避免被风控识别为脚本）。
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// 深网页会话凭证。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebAuth {
    pub cookie: String,
    #[serde(default)]
    pub bearer: String,
    #[serde(default = "default_ua")]
    pub user_agent: String,
    /// 写入时间，便于排查过期。
    #[serde(default)]
    pub captured_at: Option<String>,
}

fn default_ua() -> String {
    DEFAULT_USER_AGENT.to_string()
}

/// auth.json 的默认位置：`~/.claw/web-bridge/deepseek-auth.json`（可用
/// `CLAW_HOME` / `DEEPSEEK_WEB_AUTH` 覆盖）。
pub fn default_auth_path() -> PathBuf {
    if let Ok(p) = std::env::var("DEEPSEEK_WEB_AUTH") {
        return PathBuf::from(p);
    }
    let home = std::env::var("CLAW_HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".claw")
        .join("web-bridge")
        .join("deepseek-auth.json")
}

impl WebAuth {
    pub fn load() -> Result<Self, String> {
        let path = default_auth_path();
        let text = fs::read_to_string(&path).map_err(|e| {
            format!(
                "读取凭证失败 {}（先运行 `claw-web-bridge login` 或 `import`）: {e}",
                path.display()
            )
        })?;
        let auth: WebAuth = serde_json::from_str(&text)
            .map_err(|e| format!("凭证格式解析失败 {}: {e}", path.display()))?;
        if auth.cookie.trim().is_empty() {
            return Err("凭证 cookie 为空，请重新登录".to_string());
        }
        Ok(auth)
    }

    pub fn save(&self) -> Result<PathBuf, String> {
        let path = default_auth_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .map_err(|e| format!("创建凭证目录失败 {}: {e}", dir.display()))?;
        }
        fs::write(
            &path,
            serde_json::to_string_pretty(self).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("写入凭证失败 {}: {e}", path.display()))?;
        Ok(path)
    }

    /// 构造请求头（参考 openclaw zero-token 的 fetchHeaders）。
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h: Vec<(&'static str, String)> = vec![
            ("Cookie", self.cookie.clone()),
            ("User-Agent", self.user_agent.clone()),
            ("Content-Type", "application/json".to_string()),
            ("Accept", "*/*".to_string()),
            ("Referer", "https://chat.deepseek.com/".to_string()),
            ("Origin", "https://chat.deepseek.com".to_string()),
            ("x-client-platform", "web".to_string()),
            ("x-client-version", "1.7.0".to_string()),
            ("x-app-version", "20241129.1".to_string()),
            ("x-client-locale", "zh_CN".to_string()),
            ("x-client-timezone-offset", "28800".to_string()),
        ];
        if !self.bearer.trim().is_empty() {
            h.push(("Authorization", format!("Bearer {}", self.bearer.trim())));
        }
        h
    }
}

/// 运行时状态：当前使用的会话 id（跨 round_trip 复用同一个网页会话）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeState {
    pub session_id: Option<String>,
}
