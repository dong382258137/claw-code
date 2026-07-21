//! 修复 #1 测试：SystemContent 在 OpenAI 兼容路径下的拆分与序列化。
//!
//! 修复两个问题：
//! 1. 之前 `SystemContent::Blocks` 被直接序列化成 JSON 数组塞进 system message
//!    的 `content` 字段，但 OpenAI/DeepSeek 要求 content 是 string。
//! 2. 之前静态段和动态段全混在一起，破坏 DeepSeek 隐式前缀缓存。修复后按
//!    `cache_control` 标记位置拆成「静态 system message + 动态 system message」
//!    两个 message，静态段在前稳定可缓存，动态段在后不破坏前缀。

use api::build_chat_completion_request;
use api::{
    CacheControl, InputContentBlock, InputMessage, MessageRequest, OpenAiCompatConfig, SystemBlock,
    SystemContent,
};
use serde_json::json;

fn sample_request(system: Option<SystemContent>) -> MessageRequest {
    MessageRequest {
        model: "deepseek-chat".to_string(),
        max_tokens: 64,
        messages: vec![InputMessage {
            role: "user".to_string(),
            content: vec![InputContentBlock::Text {
                text: "hello".to_string(),
            }],
        }],
        system,
        tools: None,
        tool_choice: None,
        stream: false,
        ..Default::default()
    }
}

#[test]
fn system_content_text_emits_single_string_system_message() {
    // SystemContent::Text 应该作为单个 system message，content 是 string。
    let payload = build_chat_completion_request(
        &sample_request(Some(SystemContent::from_text("be helpful"))),
        OpenAiCompatConfig::openai(),
    );
    assert_eq!(payload["messages"][0]["role"], json!("system"));
    assert_eq!(payload["messages"][0]["content"], json!("be helpful"));
    assert!(payload["messages"][0]["content"].is_string());
    assert_eq!(payload["messages"][1]["role"], json!("user"));
}

#[test]
fn system_content_blocks_without_cache_control_emits_single_joined_message() {
    // 没有 cache_control 标记时，所有 block 文本用 \n\n 拼接成单个 system message。
    // 这是修复前的兼容路径 —— 不能再序列化成数组。
    let payload = build_chat_completion_request(
        &sample_request(Some(SystemContent::Blocks(vec![
            SystemBlock::new("section A"),
            SystemBlock::new("section B"),
            SystemBlock::new("section C"),
        ]))),
        OpenAiCompatConfig::openai(),
    );
    assert_eq!(payload["messages"][0]["role"], json!("system"));
    assert_eq!(
        payload["messages"][0]["content"],
        json!("section A\n\nsection B\n\nsection C")
    );
    assert!(payload["messages"][0]["content"].is_string());
    assert_eq!(payload["messages"][1]["role"], json!("user"));
}

#[test]
fn system_content_blocks_with_cache_control_splits_into_static_and_dynamic() {
    // 带 cache_control 标记时，按标记位置拆成两个 system message：
    // - 第一个：标记及其之前的 blocks（静态段，命中 DeepSeek 前缀缓存）
    // - 第二个：标记之后的 blocks（动态段，内容变化但不破坏前缀）
    let payload = build_chat_completion_request(
        &sample_request(Some(SystemContent::Blocks(vec![
            SystemBlock::new("static intro"),
            SystemBlock::new("static body").with_cache_control(CacheControl::ephemeral()),
            SystemBlock::new("dynamic: env_info"),
            SystemBlock::new("dynamic: mcp_instructions"),
        ]))),
        OpenAiCompatConfig::openai(),
    );
    // 第一个 system message = 静态段
    assert_eq!(payload["messages"][0]["role"], json!("system"));
    assert_eq!(
        payload["messages"][0]["content"],
        json!("static intro\n\nstatic body")
    );
    assert!(payload["messages"][0]["content"].is_string());
    // 第二个 system message = 动态段
    assert_eq!(payload["messages"][1]["role"], json!("system"));
    assert_eq!(
        payload["messages"][1]["content"],
        json!("dynamic: env_info\n\ndynamic: mcp_instructions")
    );
    assert!(payload["messages"][1]["content"].is_string());
    // 用户消息在两个 system message 之后
    assert_eq!(payload["messages"][2]["role"], json!("user"));
}

#[test]
fn system_content_blocks_with_cache_control_on_last_block_emits_only_static() {
    // 边界情况：cache_control 标记在最后一个 block 上 → 没有动态段，
    // 只输出一个 system message。
    let payload = build_chat_completion_request(
        &sample_request(Some(SystemContent::Blocks(vec![
            SystemBlock::new("section A"),
            SystemBlock::new("section B").with_cache_control(CacheControl::ephemeral()),
        ]))),
        OpenAiCompatConfig::openai(),
    );
    assert_eq!(payload["messages"][0]["role"], json!("system"));
    assert_eq!(
        payload["messages"][0]["content"],
        json!("section A\n\nsection B")
    );
    assert_eq!(payload["messages"][1]["role"], json!("user"));
}

#[test]
fn system_content_blocks_with_multiple_cache_control_uses_last_as_boundary() {
    // 多个 cache_control 标记时，用最后一个作为边界（与 Anthropic 路径
    // build_system_blocks 的语义一致 —— 它只在最后一个 static block 上标标记）。
    let payload = build_chat_completion_request(
        &sample_request(Some(SystemContent::Blocks(vec![
            SystemBlock::new("A").with_cache_control(CacheControl::ephemeral()),
            SystemBlock::new("B").with_cache_control(CacheControl::ephemeral()),
            SystemBlock::new("C"),
        ]))),
        OpenAiCompatConfig::openai(),
    );
    // 静态段 = A + B（最后一个 cache_control 在 B 上）
    assert_eq!(payload["messages"][0]["content"], json!("A\n\nB"));
    // 动态段 = C
    assert_eq!(payload["messages"][1]["content"], json!("C"));
}

#[test]
fn empty_system_content_emits_no_system_message() {
    // 空 SystemContent 不应该产生 system message。
    let payload = build_chat_completion_request(
        &sample_request(Some(SystemContent::Text(String::new()))),
        OpenAiCompatConfig::openai(),
    );
    assert_eq!(payload["messages"][0]["role"], json!("user"));

    let payload = build_chat_completion_request(
        &sample_request(Some(SystemContent::Blocks(vec![]))),
        OpenAiCompatConfig::openai(),
    );
    assert_eq!(payload["messages"][0]["role"], json!("user"));

    let payload =
        build_chat_completion_request(&sample_request(None), OpenAiCompatConfig::openai());
    assert_eq!(payload["messages"][0]["role"], json!("user"));
}

#[test]
fn cache_control_marker_does_not_leak_into_openai_payload() {
    // cache_control 是 Anthropic 专有字段，不能出现在 OpenAI 路径的输出里
    // （OpenAI 不识别，会导致 400）。
    let payload = build_chat_completion_request(
        &sample_request(Some(SystemContent::Blocks(vec![
            SystemBlock::new("static").with_cache_control(CacheControl::ephemeral()),
            SystemBlock::new("dynamic"),
        ]))),
        OpenAiCompatConfig::openai(),
    );
    let system_msg = &payload["messages"][0];
    // content 必须是纯字符串，不能是带 cache_control 字段的对象数组
    assert!(system_msg["content"].is_string());
    assert!(system_msg.get("cache_control").is_none());
    assert!(system_msg["content"]
        .as_str()
        .is_some_and(|s| !s.contains("cache_control")));
}
