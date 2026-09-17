//! M0 试点:无浏览器的端到端验证。
//!
//! 链路:mock 网页(mock_web)→ WebApiClient(实现 runtime::ApiClient)
//! → claw agent 循环 → 本地 read_file 工具执行 → 最终回复。
//!
//! 通过判据:最终回复包含 `MOCK_FINAL: <fixture_text>` —— 只有本地工具真实
//! 执行、结果回到上下文后,mock 才会给出该答复;否则触发 `[mock-stall]` 保护。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use tools::GlobalToolRegistry;
use web_bridge::driver::run_prompt;
use web_bridge::mock_web::{MockWebExchange, MOCK_STALL};
use web_bridge::WebApiClient;

fn unique_text() -> String {
    format!(
        "m0fixture-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos()
    )
}

#[tokio::test]
async fn mock_web_drives_local_tool_and_returns_final_reply() {
    // fixture 放在本 crate 的 target/ 下(进程 cwd 内,文件工具边界友好)。
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target_dir = crate_dir.join("target");
    std::fs::create_dir_all(&target_dir).expect("create target dir");

    // 内容 token 只在工具「结果」里出现(工具入参是路径,文件名用固定值),
    // mock 据此区分"工具已真正执行"与"参数里碰巧有该字符串"。
    let fixture_text = unique_text();
    let fixture_path = target_dir.join("m0_fixture.txt");
    std::fs::write(&fixture_path, &fixture_text).expect("write fixture");

    let client = WebApiClient::new(
        Box::new(MockWebExchange::new(
            fixture_path.to_string_lossy().to_string(),
            fixture_text.clone(),
            6,
        )),
        GlobalToolRegistry::builtin(),
    );

    let system_prompt = vec![
        "You are claw-code, an AI coding assistant backed by the DeepSeek web page. \
         Respond concisely in Chinese."
            .to_string(),
    ];

    let final_text = run_prompt(
        client,
        system_prompt,
        &format!("请读取本地文件 {:?} 并告诉我它的内容。", fixture_path),
    )
    .await
    .expect("m0 pilot should complete without ACP errors");

    let expected = format!("MOCK_FINAL: {fixture_text}");
    assert!(
        final_text.contains(&expected),
        "最终回复应包含 mock 的最终答复(证明本地工具已执行);实际回复:\n{final_text:#?}"
    );
    assert!(
        !final_text.contains(MOCK_STALL),
        "不应触发 mock 死循环保护;实际回复:\n{final_text:#?}"
    );

    let _ = std::fs::remove_file(&fixture_path);
}
