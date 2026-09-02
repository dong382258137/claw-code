//! 临时诊断:生产精确命令下 execute_bash 是否 30s 内返回(三次事故复现验证)。
use std::io::Write;

fn main() {
    let cmd = "cd /d/chanlunV2 && C:/Users/38225/AppData/Local/Programs/Python/Python311/python.exe -B -m chanlun_py.web.server > /d/chanlunV2/chanlun_py/.sandbox-tmp/ws_server_restart.log 2>&1 &\necho \"started, waiting for port...\"; sleep 8; netstat -ano 2>/dev/null | grep -E \":8765|5001\\s.*LISTENING\" | head -5";
    println!("[bgtest] running execute_bash with timeout=30000");
    let _ = std::io::stdout().flush();
    let start = std::time::Instant::now();
    let out = runtime::bash::execute_bash(runtime::bash::BashCommandInput {
        command: cmd.to_string(),
        timeout: Some(30000),
        description: Some("bg test".to_string()),
        run_in_background: Some(false),
        dangerously_disable_sandbox: Some(false),
        namespace_restrictions: Some(false),
        isolate_network: Some(false),
        filesystem_mode: Some(runtime::sandbox::FilesystemIsolationMode::WorkspaceOnly),
        allowed_mounts: None,
    });
    let elapsed = start.elapsed();
    println!("[bgtest] elapsed: {elapsed:?}");
    match out {
        Ok(o) => {
            println!("[bgtest] OK stdout={:?} stderr={:?}", o.stdout, o.stderr);
        }
        Err(e) => {
            println!("[bgtest] ERR {e}");
        }
    }
}
