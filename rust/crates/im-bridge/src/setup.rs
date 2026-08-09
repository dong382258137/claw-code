//! 交互式配置向导：`claw-im-bridge --setup`
//!
//! 引导用户一步步填写飞书 / 企业微信凭据，自动生成 `~/.claw/im-bridge.toml`，
//! 免去手写 TOML 的麻烦。已存在的配置会被备份为 `im-bridge.toml.bak`。

use std::io::{self, Write};
use std::path::PathBuf;

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn config_path() -> PathBuf {
    home_dir().join(".claw").join("im-bridge.toml")
}

/// 读取一行用户输入，空输入时回退到默认值。
fn prompt(label: &str, hint: &str, default: Option<&str>, required: bool) -> Option<String> {
    loop {
        let suffix = match default {
            Some(d) if !d.is_empty() => format!(" [{d}]"),
            _ => String::new(),
        };
        print!("  \u{251c} {label}{suffix} {hint}: ");
        io::stdout().flush().ok();

        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            return None;
        }
        let value = line.trim().to_string();

        if !value.is_empty() {
            return Some(value);
        }
        if let Some(d) = default {
            if !d.is_empty() {
                return Some(d.to_string());
            }
        }
        if required {
            println!("  \u{2514} \x1b[31m必填\x1b[0m，不能为空，请重新输入。");
            continue;
        }
        return None;
    }
}

/// 生成 TOML 配置文件内容。
#[allow(clippy::too_many_arguments)]
fn render_feishu_toml(
    listen_addr: &str,
    session_timeout: u64,
    mode: &str,
    app_id: &str,
    app_secret: &str,
    verification_token: Option<&str>,
    encrypt_key: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("listen_addr = \"{listen_addr}\"\n"));
    out.push_str(&format!("session_timeout_secs = {session_timeout}\n"));
    out.push_str("\n[feishu]\n");
    out.push_str(&format!("mode = \"{mode}\"\n"));
    out.push_str(&format!("app_id = \"{app_id}\"\n"));
    out.push_str(&format!("app_secret = \"{app_secret}\"\n"));
    if let Some(t) = verification_token.filter(|s| !s.is_empty()) {
        out.push_str(&format!("verification_token = \"{t}\"\n"));
    }
    if let Some(k) = encrypt_key.filter(|s| !s.is_empty()) {
        out.push_str(&format!("encrypt_key = \"{k}\"\n"));
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn render_wecom_toml(
    listen_addr: &str,
    session_timeout: u64,
    corp_id: &str,
    secret: &str,
    token: &str,
    encoding_aes_key: &str,
    webhook_url: Option<&str>,
    agent_id: Option<i64>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("listen_addr = \"{listen_addr}\"\n"));
    out.push_str(&format!("session_timeout_secs = {session_timeout}\n"));
    out.push_str("\n[wecom]\n");
    out.push_str(&format!("corp_id = \"{corp_id}\"\n"));
    out.push_str(&format!("secret = \"{secret}\"\n"));
    out.push_str(&format!("token = \"{token}\"\n"));
    out.push_str(&format!("encoding_aes_key = \"{encoding_aes_key}\"\n"));
    if let Some(u) = webhook_url.filter(|s| !s.is_empty()) {
        out.push_str(&format!("webhook_url = \"{u}\"\n"));
    }
    if let Some(a) = agent_id {
        out.push_str(&format!("agent_id = {a}\n"));
    }
    out
}

/// 运行交互式配置向导。
pub fn run_setup() -> Result<(), String> {
    let path = config_path();
    println!("\u{1f916} claw-im-bridge \u{2014} 交互式配置向导");
    println!("  将生成配置文件: {}", path.display());
    println!("  (Ctrl+C 随时取消)\n");

    println!("  \u{251c} 选择要接入的 IM 平台:");
    println!("  \u{2502}   1) 飞书 Feishu/Lark");
    println!("  \u{2502}   2) 企业微信 WeCom");
    let platform = prompt("平台", "[1]", Some("1"), true)
        .ok_or_else(|| "输入被取消".to_string())?;

    let listen_addr = prompt(
        "HTTP 监听地址",
        "(飞书/企微 webhook 回调地址对应端口)",
        Some("127.0.0.1:3456"),
        true,
    )
    .ok_or_else(|| "输入被取消".to_string())?;

    let session_timeout = prompt(
        "会话空闲超时(秒)",
        "(30 分钟 = 1800)",
        Some("1800"),
        true,
    )
    .ok_or_else(|| "输入被取消".to_string())?
    .parse::<u64>()
    .map_err(|_| "会话超时必须为数字".to_string())?;

    // 生成配置，已存在则先备份
    if path.exists() {
        let backup = path.with_extension("toml.bak");
        std::fs::copy(&path, &backup)
            .map_err(|e| format!("备份旧配置失败: {e}"))?;
        println!("\n  \u{1f4c1} 旧配置已备份为 {}", backup.display());
    }

    let content = if platform == "2" {
        let corp_id = prompt("企业 ID (corp_id)", "", None, true)
            .ok_or_else(|| "输入被取消".to_string())?;
        let secret = prompt("企业 Secret", "", None, true)
            .ok_or_else(|| "输入被取消".to_string())?;
        let token = prompt("Token", "", None, true)
            .ok_or_else(|| "输入被取消".to_string())?;
        let aes = prompt("EncodingAESKey (43 字符)", "", None, true)
            .ok_or_else(|| "输入被取消".to_string())?;
        let webhook = prompt("Webhook URL (可选)", "", None, false);
        let agent_id = prompt("Agent ID (可选)", "", None, false)
            .and_then(|s| s.parse::<i64>().ok());
        render_wecom_toml(
            &listen_addr,
            session_timeout,
            &corp_id,
            &secret,
            &token,
            &aes,
            webhook.as_deref(),
            agent_id,
        )
    } else {
        println!("  \u{251c} 飞书事件订阅模式:");
        println!("  \u{2502}   1) ws —— 长连接 (推荐，无需公网地址/回调 URL)");
        println!("  \u{2502}   2) http —— webhook 回调 (需要公网地址)");
        let mode = prompt("模式", "[1]", Some("1"), true)
            .ok_or_else(|| "输入被取消".to_string())?;
        let mode = if mode == "2" { "http" } else { "ws" };

        let app_id = prompt("App ID (cli_xxx)", "(飞书开放平台-凭证与基础信息)", None, true)
            .ok_or_else(|| "输入被取消".to_string())?;
        let app_secret = prompt("App Secret", "", None, true)
            .ok_or_else(|| "输入被取消".to_string())?;
        let verification = prompt("Verification Token (可选)", "", None, false);
        let encrypt_key = prompt("Encrypt Key (可选)", "", None, false);
        render_feishu_toml(
            &listen_addr,
            session_timeout,
            mode,
            &app_id,
            &app_secret,
            verification.as_deref(),
            encrypt_key.as_deref(),
        )
    };

    // 写入文件
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    std::fs::write(&path, content).map_err(|e| format!("写入配置失败: {e}"))?;

    println!("\n  \u{2705} 配置文件已生成: {}", path.display());
    println!("  \x1b[2m下一步：运行 \x1b[0m\x1b[1mclaw-im-bridge\x1b[0m\x1b[2m 启动桥接服务。\x1b[0m");
    println!(
        "  \x1b[2m提示：还需在飞书/企微管理后台配置 webhook 回调地址指向本服务的 /feishu/webhook 或 /wecom/webhook。\x1b[0m"
    );
    Ok(())
}