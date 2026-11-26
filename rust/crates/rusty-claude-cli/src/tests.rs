#![allow(dead_code, unused_imports, unused_variables)]

//! Phase 3 后的测试面：只覆盖 Web 服务仍使用的模块
//! （会话内核、运行时构建、API 客户端、插件状态、蓝图/技能装配）。
//! CLI 参数解析、终端渲染与斜杠命令的测试已随界面层一并删除。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use api::{InputContentBlock, MessageResponse, OutputContentBlock, Usage};
use plugins::{PluginTool, PluginToolDefinition, PluginToolPermission};
use runtime::PermissionMode;
use serde_json::json;
use tools::GlobalToolRegistry;

fn registry_with_plugin_tool() -> GlobalToolRegistry {
    GlobalToolRegistry::with_plugin_tools(vec![PluginTool::new(
        "plugin-demo@external",
        "plugin-demo",
        PluginToolDefinition {
            name: "plugin_echo".to_string(),
            description: Some("Echo plugin payload".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"],
                "additionalProperties": false
            }),
        },
        "echo".to_string(),
        Vec::new(),
        PluginToolPermission::WorkspaceWrite,
        None,
    )])
    .expect("plugin tool registry should build")
}

fn temp_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be after epoch")
        .as_nanos();
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rusty-claude-cli-{nanos}-{unique}"))
}

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn write_skill_fixture(root: &Path, name: &str, description: &str) {
    let skill_dir = root.join(name);
    std::fs::create_dir_all(&skill_dir).expect("skill dir should exist");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n"),
    )
    .expect("skill file should write");
}

#[test]
fn short_tool_id_truncates_long_identifiers_with_ellipsis() {
    // given
    let long = "toolu_01ABCDEFGHIJKLMN";
    let short = "tool_1";

    // when
    let trimmed_long = crate::short_tool_id(long);
    let trimmed_short = crate::short_tool_id(short);

    // then
    assert_eq!(trimmed_long, "toolu_01ABCD…");
    assert_eq!(trimmed_short, "tool_1");
}

#[test]
fn truncate_for_summary_appends_ellipsis_only_when_needed() {
    assert_eq!(crate::truncate_for_summary("abc", 5), "abc");
    assert_eq!(crate::truncate_for_summary("abcdef", 3), "abc…");
}

#[test]
fn filtered_tool_specs_respect_allowlist() {
    let allowed = ["read_file", "grep_search"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let filtered = crate::filter_tool_specs(&GlobalToolRegistry::builtin(), Some(&allowed));
    let names = filtered
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["read_file", "grep_search"]);
}

#[test]
fn filtered_tool_specs_include_plugin_tools() {
    let filtered = crate::filter_tool_specs(&registry_with_plugin_tool(), None);
    let names = filtered
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"bash".to_string()));
    assert!(names.contains(&"plugin_echo".to_string()));
}

#[test]
fn permission_policy_uses_plugin_tool_permissions() {
    let feature_config = runtime::RuntimeFeatureConfig::default();
    let policy = crate::api_client::permission_policy(
        PermissionMode::ReadOnly,
        &feature_config,
        &registry_with_plugin_tool(),
    )
    .expect("permission policy should build");
    let required = policy.required_mode_for("plugin_echo");
    assert_eq!(required, PermissionMode::WorkspaceWrite);
}

#[test]
fn compact_tool_output_unwraps_web_search_json() {
    let web_search_json = serde_json::json!({
        "query": "rust async",
        "results": [
            "Search results for \"rust async\". Include a Sources section.\n- [Tokio](https://tokio.rs)\n- [async-std](https://async.rs)",
            {
                "tool_use_id": "web_search_1",
                "content": [
                    {"title": "Tokio", "url": "https://tokio.rs"},
                    {"title": "async-std", "url": "https://async.rs"}
                ]
            }
        ],
        "durationSeconds": 0.5
    })
    .to_string();

    let compacted =
        crate::api_client::compact_tool_output_for_model("WebSearch", &web_search_json, false);
    assert!(
        compacted.contains("Tokio"),
        "should extract commentary containing search results, got: {compacted}"
    );
    assert!(
        !compacted.contains("durationSeconds"),
        "should strip JSON wrapper, got: {compacted}"
    );
    assert!(
        !compacted.contains("tool_use_id"),
        "should strip structured hit objects, got: {compacted}"
    );
}

#[test]
fn compact_tool_output_unwraps_web_fetch_json() {
    let web_fetch_json = serde_json::json!({
        "bytes": 1234,
        "code": 200,
        "code_text": "OK",
        "result": "This page is about Rust programming language features.",
        "duration_ms": 500,
        "url": "https://example.com/rust"
    })
    .to_string();

    let compacted =
        crate::api_client::compact_tool_output_for_model("WebFetch", &web_fetch_json, false);
    assert_eq!(
        compacted,
        "This page is about Rust programming language features."
    );
}

#[test]
fn compact_tool_output_passes_through_unknown_tools() {
    let output = "some raw tool output";
    let compacted = crate::api_client::compact_tool_output_for_model("Bash", output, false);
    assert_eq!(compacted, output);
}

#[test]
fn compact_tool_output_falls_back_on_invalid_json() {
    let output = "not valid json at all";
    let compacted = crate::api_client::compact_tool_output_for_model("WebSearch", output, false);
    assert_eq!(compacted, output);
}

#[test]
fn compact_tool_output_preserves_edit_file_error() {
    let error_output = serde_json::json!({
        "error": "old_string not found in file",
        "filePath": "D:\\claw-code-src\\test.txt"
    })
    .to_string();
    let compacted =
        crate::api_client::compact_tool_output_for_model("edit_file", &error_output, true);
    assert!(
        compacted.contains("old_string not found in file"),
        "error message should be preserved, got: {compacted}"
    );
    assert!(
        !compacted.contains("has been updated successfully"),
        "should not claim success on error, got: {compacted}"
    );
    assert!(
        !compacted.contains("unknown"),
        "should not rewrite path to 'unknown', got: {compacted}"
    );
}

#[test]
fn response_to_events_keeps_text_and_thinking_blocks() {
    let response = MessageResponse {
        id: "msg_1".to_string(),
        kind: "message".to_string(),
        role: "assistant".to_string(),
        model: "deepseek-flash".to_string(),
        content: vec![
            OutputContentBlock::Thinking {
                thinking: "let me think".to_string(),
                signature: Some("sig".to_string()),
            },
            OutputContentBlock::Text {
                text: "hello".to_string(),
            },
        ],
        stop_reason: Some("end_turn".to_string()),
        stop_sequence: None,
        usage: Usage::default(),
        request_id: Some("req_1".to_string()),
    };

    let events = crate::api_client::response_to_events(response).expect("events should convert");
    assert!(matches!(
        &events[0],
        runtime::AssistantEvent::Thinking { thinking, .. } if thinking == "let me think"
    ));
    assert!(matches!(
        &events[1],
        runtime::AssistantEvent::TextDelta(text) if text == "hello"
    ));
}

mod blueprint_tests {
    use super::temp_dir;
    use std::fs;

    /// 根目录 `BLUEPRINT.md` 是入库位置，必须优先于 `.claw/` 本机覆盖。
    #[test]
    fn load_blueprint_prefers_tracked_root_file() {
        let root = temp_dir();
        fs::create_dir_all(root.join(".claw")).expect("create .claw");
        fs::write(
            root.join("BLUEPRINT.md"),
            "# Project Blueprint\n\n使命:不跑偏。",
        )
        .expect("write root blueprint");
        fs::write(root.join(".claw").join("BLUEPRINT.md"), "本机临时覆盖").expect("write override");

        let got = crate::app::load_blueprint(&root).expect("应读到根目录蓝图");
        assert!(
            got.contains("不跑偏"),
            "根目录 BLUEPRINT.md 必须优先于 .claw 覆盖, got: {got}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_blueprint_falls_back_to_local_override() {
        let root = temp_dir();
        fs::create_dir_all(root.join(".claw")).expect("create .claw");
        fs::write(root.join(".claw").join("BLUEPRINT.md"), "本机覆盖生效").expect("write override");

        let got = crate::app::load_blueprint(&root).expect("应回退到 .claw 覆盖");
        assert!(got.contains("本机覆盖生效"), "got: {got}");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_blueprint_returns_none_when_absent_or_blank() {
        let root = temp_dir();
        fs::create_dir_all(&root).expect("create root");
        assert!(
            crate::app::load_blueprint(&root).is_none(),
            "文件缺失时应返回 None"
        );

        fs::write(root.join("BLUEPRINT.md"), "   \n\t\n  ").expect("write blank");
        assert!(
            crate::app::load_blueprint(&root).is_none(),
            "纯空白时应返回 None, 否则会注入空壳段"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_blueprint_creates_skeleton_in_real_project() {
        let root = temp_dir();
        fs::create_dir_all(&root).expect("create root");

        let got = crate::app::ensure_blueprint(&root, false).expect("应创建模板");
        assert!(got.contains("## 1. 使命"), "模板需含六节骨架");
        assert!(got.contains("TODO"), "模板应保留 TODO 提示待 AI 填写");
        assert!(
            root.join("BLUEPRINT.md").exists(),
            "模板必须真的落盘, 否则下个会话仍缺装配图"
        );
        fs::write(root.join("BLUEPRINT.md"), "# 已填写").expect("write filled");
        let again = crate::app::ensure_blueprint(&root, false).expect("应读到已填内容");
        assert_eq!(again, "# 已填写", "已存在的蓝图不得被模板覆盖");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_blueprint_skips_broad_directory() {
        let root = temp_dir();
        fs::create_dir_all(&root).expect("create root");

        assert!(
            crate::app::ensure_blueprint(&root, true).is_none(),
            "宽泛目录应跳过创建"
        );
        assert!(
            !root.join("BLUEPRINT.md").exists(),
            "宽泛目录不得落盘 BLUEPRINT.md"
        );

        let _ = fs::remove_dir_all(&root);
    }
}

#[test]
fn init_template_mentions_detected_rust_workspace() {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let rendered = crate::init::render_init_claude_md(&workspace_root);
    assert!(rendered.contains("# CLAUDE.md"));
    assert!(rendered.contains("cargo clippy --workspace --all-targets -- -D warnings"));
}

#[test]
fn load_skill_catalog_reflects_project_skills() {
    let _guard = env_lock();
    let workspace = temp_dir();
    std::fs::create_dir_all(&workspace).expect("workspace should exist");
    write_skill_fixture(
        &workspace.join(".codex").join("skills"),
        "caveman",
        "Project skill fixture",
    );

    let catalog = crate::app::load_skill_catalog(&workspace).expect("catalog should build");
    assert!(catalog.contains("caveman"), "got: {catalog}");

    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn render_session_markdown_contains_messages_and_tool_summary() {
    let mut session = runtime::Session::new();
    session
        .messages
        .push(runtime::ConversationMessage::user_text("导出验证"));
    session
        .messages
        .push(runtime::ConversationMessage::assistant(vec![
            runtime::ContentBlock::Text {
                text: "好的".to_string(),
            },
            runtime::ContentBlock::ToolUse {
                id: "toolu_01ABCDEFGHIJKLMN".to_string(),
                name: "read_file".to_string(),
                input: "{\"path\":\"README.md\"}".to_string(),
            },
        ]));

    let markdown = crate::session_mgr::render_session_markdown(
        &session,
        "session-1",
        Path::new(".claw/sessions/x/session-1.jsonl"),
    );
    assert!(markdown.contains("# Conversation Export"));
    assert!(markdown.contains("导出验证"));
    assert!(markdown.contains("**Tool call** `read_file`"));
    assert!(markdown.contains("toolu_01ABCD…"));
}

#[test]
fn version_label_is_never_empty() {
    assert!(!crate::version_label().is_empty());
}
