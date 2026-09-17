//! `claw-web-bridge` CLI：DeepSeek 网页版(登录态、零 Token)作为 claw 的模型后端。
//!
//! 子命令：
//! - `login`   — 打开本机 Edge/Chrome（专用 profile）登录 chat.deepseek.com 一次，
//!   自动抓取 cookie + userToken 保存到 auth.json（之后不再需要浏览器）
//! - `import`  — 手动粘贴浏览器 cookie（DevTools→Network→任一请求→Cookie 头），
//!   自动调 users/current 取 token 并保存
//! - `run "<任务>"` — 用网页版会话跑完整 claw agent 回合（本地工具执行），打印最终文本
//!
//! 旧链路（Electron 壳 / 浏览器扩展 / WS 网关）已退役。
//!
//! 环境变量：
//! - `DEEPSEEK_WEB_BASE`：覆盖 API 基址（测试/代理用）
//! - `DEEPSEEK_WEB_AUTH` / `CLAW_HOME`：auth.json 位置
//! - `CHROME_PATH`：登录使用的浏览器可执行文件

use std::process::ExitCode;

use web_bridge::deepseek_web::{fetch_user_token, DeepseekWebClient};
use web_bridge::web_session::WebAuth;
use web_bridge::{driver, WebApiClient};

fn system_prompt() -> Vec<String> {
    vec![
        format!(
            "You are claw-code, an AI coding assistant. Today's date is {}.",
            chrono::Local::now().format("%Y-%m-%d")
        ),
        "You are running through the DeepSeek web page session (login once, zero API token). \
         Use the full claw toolset (bash, file read/write/edit, search, web search/fetch, todo, \
         skills, sub-agents, MCP, ...) to complete the user's request, then reply with the result \
         concisely in the user's language."
            .to_string(),
    ]
}

fn usage() -> ! {
    eprintln!(
        "用法: claw-web-bridge <login|import|run>\n\
         \x20 login                       登录一次并保存凭证\n\
         \x20 import --cookie \"<cookie>\"   手动粘贴浏览器 Cookie(自动取 token)\n\
         \x20 run \"<任务文本>\"             跑完整 claw 回合(本地工具执行)"
    );
    std::process::exit(2);
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "claw_web_bridge=info,web_bridge=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let result = match cmd {
        "login" => rt.block_on(login(&args)),
        "import" => rt.block_on(import(&args)),
        "run" => rt.block_on(run_task(&args)),
        _ => {
            if cmd.is_empty() {
                usage();
            } else {
                eprintln!("未知子命令: {cmd}");
                usage();
            }
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

// ========== import：粘贴 cookie 自动取 token ==========

async fn import(args: &[String]) -> Result<(), String> {
    let cookie = args
        .windows(2)
        .find(|w| w[0] == "--cookie")
        .map(|w| w[1].clone())
        .ok_or_else(|| {
            "缺少 cookie，用法: claw-web-bridge import --cookie \"<cookie>\"".to_string()
        })?;
    let base = std::env::var("DEEPSEEK_WEB_BASE")
        .unwrap_or_else(|_| "https://chat.deepseek.com".to_string());
    let bearer = fetch_user_token(&base, &cookie)?;
    let auth = WebAuth {
        cookie,
        bearer,
        user_agent: web_bridge::web_session::DEFAULT_USER_AGENT.to_string(),
        captured_at: Some(chrono::Local::now().to_rfc3339()),
    };
    let path = auth.save()?;
    println!("凭证已保存: {}", path.display());
    println!(
        "token 获取成功(bearer 长度 {})，现在可以 `claw-web-bridge run \"任务\"`",
        auth.bearer.len()
    );
    Ok(())
}

// ========== run：完整 agent 回合 ==========

async fn run_task(args: &[String]) -> Result<(), String> {
    let task = args
        .get(1)
        .ok_or_else(|| "缺少任务文本，用法: claw-web-bridge run \"<任务>\"".to_string())?;
    let auth = WebAuth::load()?;
    let client = WebApiClient::new(
        Box::new(DeepseekWebClient::new(auth)),
        ::tools::GlobalToolRegistry::builtin(),
    );
    let text = driver::run_prompt(client, system_prompt(), task).await?;
    println!("{text}");
    Ok(())
}

// ========== login：CDP 一次登录 ==========

async fn login(_args: &[String]) -> Result<(), String> {
    web_bridge::login::run().await
}
