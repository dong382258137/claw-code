//! Configuration for the IM bridge.
//!
//! Reads from `~/.claw/im-bridge.toml`.

use serde::Deserialize;
use std::path::PathBuf;

/// Top-level IM bridge configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ImBridgeConfig {
    /// Server listen address (default: "127.0.0.1:3456").
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,

    /// Feishu (Lark) configuration.
    #[serde(default)]
    pub feishu: Option<FeishuConfig>,

    /// WeCom (企业微信) configuration.
    #[serde(default)]
    pub wecom: Option<WeComConfig>,

    /// Session idle timeout in seconds (default: 1800 = 30 min).
    #[serde(default = "default_session_timeout_secs")]
    pub session_timeout_secs: u64,

    /// Agent 工作区配置（默认：启动目录，即沙盒边界）。
    #[serde(default)]
    pub agent: AgentConfig,
}

/// Agent 工作区配置。
///
/// 默认情况下 agent 的沙盒边界 = im-bridge 进程启动目录：bash 在启动目录下
/// 执行，文件工具（read/write/edit/grep/glob）拒绝访问边界之外的路径。
/// 本配置用于扩展工作区根；**未配置时程序自动枚举本机所有盘符根作为白名单**，
/// 即默认获得跨盘"最大权限"，无需手动配置。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentConfig {
    /// 主工作区根目录。设置后 im-bridge 进程以它为当前目录：bash 默认工作
    /// 目录、文件工具主边界都跟随它。设为盘符根（如 `"C:\\"`）即可访问该盘
    /// 任意路径。未设置时保持启动目录为 bash 默认目录。
    #[serde(default)]
    pub workspace_root: Option<PathBuf>,

    /// 额外允许的工作区根（可包含多个盘符/目录）。文件工具边界 = 主根 +
    /// 这些额外根。**为空时自动探测本机所有盘符根**（最大权限）；显式设置
    /// 时可借此收窄/指定访问范围。
    #[serde(default)]
    pub workspace_roots: Vec<PathBuf>,
}

fn default_listen_addr() -> String {
    "127.0.0.1:3456".to_string()
}

fn default_session_timeout_secs() -> u64 {
    1800
}

/// Feishu (Lark) bot configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct FeishuConfig {
    /// Feishu App ID.
    pub app_id: String,
    /// Feishu App Secret.
    pub app_secret: String,
    /// Verification token for event subscription (optional; for signature verification).
    pub verification_token: Option<String>,
    /// Encrypt key for event decryption (optional; if events are encrypted).
    pub encrypt_key: Option<String>,
    /// Event subscription mode: `"http"` (webhook, default) or `"ws"` (long connection).
    #[serde(default = "default_feishu_mode")]
    pub mode: String,
}

fn default_feishu_mode() -> String {
    // 长连接（WebSocket）模式无需公网地址，是推荐模式；旧配置缺失 mode 时落在该模式。
    "ws".to_string()
}

/// WeCom (企业微信) smart bot configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct WeComConfig {
    /// WeCom Corp ID.
    pub corp_id: String,
    /// WeCom Corp Secret (for access token).
    pub secret: String,
    /// Token for URL verification and message signature.
    pub token: String,
    /// Encoding AES key (43 characters) for message encryption.
    pub encoding_aes_key: String,
    /// Webhook URL for active push (optional; if set, prefer over API).
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// Agent ID for API-based push (optional; fallback when webhook_url is absent).
    #[serde(default)]
    pub agent_id: Option<i64>,
}

impl ImBridgeConfig {
    /// Load configuration from the default path: `~/.claw/im-bridge.toml`.
    pub fn load() -> Result<Self, String> {
        let path = Self::default_path();
        if !path.exists() {
            return Err(format!(
                "Configuration file not found: {}\n\
                 Create it with at least one platform section (feishu or wecom).\n\
                 See https://open.feishu.cn/ or https://developer.work.weixin.qq.com/",
                path.display()
            ));
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        let config: Self = toml::from_str(&content)
            .map_err(|e| format!("Failed to parse {}: {e}", path.display()))?;
        config.validate()?;
        warn_legacy_mode(&path, &content);
        Ok(config)
    }

    /// Validate that at least one platform is configured.
    fn validate(&self) -> Result<(), String> {
        if self.feishu.is_none() && self.wecom.is_none() {
            return Err(
                "At least one of [feishu] or [wecom] must be configured in im-bridge.toml"
                    .to_string(),
            );
        }
        Ok(())
    }

    fn default_path() -> PathBuf {
        let home = dirs_next().unwrap_or_else(|| PathBuf::from("."));
        home.join(".claw").join("im-bridge.toml")
    }
}

fn dirs_next() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Detect legacy configs that predate the `mode` field on `[feishu]`.
///
/// Such configs silently fall back to `default_feishu_mode()` ("ws"). This is a
/// config-compatibility symptom: upgrading the binary does not migrate the data
/// file, so we surface the upgrade hint explicitly instead of silently reusing
/// the old config.
fn warn_legacy_mode(path: &std::path::Path, content: &str) {
    // Find the `[feishu]` section and check whether it declares `mode`.
    let mut in_feishu = false;
    let mut has_mode = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            in_feishu = t == "[feishu]";
            continue;
        }
        if in_feishu && t.starts_with("mode") {
            has_mode = true;
        }
    }
    if in_feishu && !has_mode {
        eprintln!(
            "[im-bridge] warning: {} has a [feishu] section without a `mode` field (legacy config). \
             Falling back to default mode \"ws\" (long connection, no public URL needed). \
             To pin it explicitly, add `mode = \"ws\"` under [feishu].",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [agent] 段可解析：workspace_root + workspace_roots。
    #[test]
    fn parses_agent_workspace_config() {
        let toml_str = r#"
            listen_addr = "127.0.0.1:3456"
            session_timeout_secs = 1800

            [feishu]
            mode = "ws"
            app_id = "cli_test"
            app_secret = "secret"

            [agent]
            workspace_root = "C:\\"
            workspace_roots = ["D:\\", "E:\\"]
        "#;
        let config: ImBridgeConfig = toml::from_str(toml_str).expect("should parse");
        let agent = config.agent;
        assert_eq!(
            agent.workspace_root.as_deref(),
            Some(std::path::Path::new("C:\\"))
        );
        let roots: Vec<String> = agent
            .workspace_roots
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert_eq!(roots, vec!["D:\\", "E:\\"]);
    }

    /// 无 [agent] 段时默认：不限制主根、无额外根。
    #[test]
    fn agent_defaults_when_section_missing() {
        let toml_str = r#"
            [feishu]
            mode = "ws"
            app_id = "cli_test"
            app_secret = "secret"
        "#;
        let config: ImBridgeConfig = toml::from_str(toml_str).expect("should parse");
        assert!(config.agent.workspace_root.is_none());
        assert!(config.agent.workspace_roots.is_empty());
    }
}
