//! Chat command parser and handler for IM bridge.
//!
//! Commands are intercepted before reaching the agent.
//! Supported commands:
//! - `/help` — show available commands
//! - `/new` or `/reset` — start a new session (clears conversation history)
//! - `/status` — show current session info
//! - `/history` — show recent messages (placeholder for future)

use agent_client_protocol as acp;

/// Recognized chat commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatCommand {
    /// Show available commands and usage.
    Help,
    /// Start a new session (discard current conversation).
    NewSession,
    /// Show current session status.
    Status,
    /// Show recent message history.
    History,
    /// Session Bus 命令：`/bus list|send <target> <text>|watch <target>|unwatch <target>`。
    /// 真实执行在 `SessionManager::handle_command`（需访问 bus + IM 路由表）。
    Bus {
        /// `/bus` 后的完整子命令文本（如 "send im:feishu:oc_xxx 你好"）。
        args: String,
    },
}

/// Result of parsing a chat command from user text.
#[derive(Debug, Clone)]
pub enum CommandParseResult {
    /// The text is a recognized command.
    Command(ChatCommand),
    /// The text is a regular message (not a command).
    Message(String),
}

/// Parse a message to check if it's a command.
pub fn parse_command(text: &str) -> CommandParseResult {
    let trimmed = text.trim();

    match trimmed {
        "/help" | "/h" => CommandParseResult::Command(ChatCommand::Help),
        "/new" | "/reset" | "/clear" => CommandParseResult::Command(ChatCommand::NewSession),
        "/status" | "/info" => CommandParseResult::Command(ChatCommand::Status),
        "/history" | "/hist" => CommandParseResult::Command(ChatCommand::History),
        // /bus 前缀 → 子命令交给 SessionManager 执行（需 bus + 路由表）
        other if other == "/bus" || other.starts_with("/bus ") => {
            let args = other.trim_start_matches("/bus").trim().to_string();
            CommandParseResult::Command(ChatCommand::Bus { args })
        }
        other => CommandParseResult::Message(other.to_string()),
    }
}

/// Handle a chat command and return the response text.
///
/// `session_count` is the total number of active sessions (for /status).
pub async fn handle_command(
    cmd: &ChatCommand,
    session_id: &acp::SessionId,
    session_count: usize,
) -> String {
    match cmd {
        ChatCommand::Help => r#"**IM Bridge Commands**

`/help` — Show this help
`/new` — Start a new session (clear conversation history)
`/status` — Show current session info
`/history` — Show recent messages
`/bus` — Session Bus: list/send/watch/unwatch (see below)

Just type a message to chat with claw-code AI."#
            .to_string(),
        ChatCommand::Bus { .. } => {
            // 真实执行在 SessionManager::handle_command（需要访问 bus + IM 路由表）；
            // 此处兜底返回用法说明。
            r#"**Session Bus Commands**

`/bus list` — List all visible sessions (peers) and their status
`/bus send <target> <text>` — Send a message to another session (e.g. `/bus send im:feishu:oc_xxx 你好`)
`/bus watch <target>` — Subscribe to another session's message stream
`/bus unwatch <target>` — Unsubscribe

Targets: `im:{platform}:{chat_id}` for IM channels, or `*` for broadcast."#
                .to_string()
        }
        ChatCommand::NewSession => {
            "Starting a new session. Your next message will begin a fresh conversation.".to_string()
        }
        ChatCommand::Status => {
            format!(
                "**Session Info**\n\
                 Session ID: `{session_id}`\n\
                 Active sessions: {session_count}\n\
                 Type `/new` to start a fresh session."
            )
        }
        ChatCommand::History => {
            "History is not yet implemented. Type a message to start a conversation.".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_help_command() {
        for variant in &["/help", "/h", "  /help  "] {
            let result = parse_command(variant);
            assert!(matches!(
                result,
                CommandParseResult::Command(ChatCommand::Help)
            ));
        }
    }

    #[test]
    fn test_parse_new_command() {
        for variant in &["/new", "/reset", "/clear"] {
            let result = parse_command(variant);
            assert!(
                matches!(result, CommandParseResult::Command(ChatCommand::NewSession)),
                "failed for {variant}: {result:?}"
            );
        }
    }

    #[test]
    fn test_parse_status_command() {
        for variant in &["/status", "/info"] {
            let result = parse_command(variant);
            assert!(matches!(
                result,
                CommandParseResult::Command(ChatCommand::Status)
            ));
        }
    }

    #[test]
    fn test_parse_regular_message() {
        let result = parse_command("hello world");
        assert!(matches!(result, CommandParseResult::Message(m) if m == "hello world"));
    }

    #[test]
    fn test_parse_bus_list() {
        for variant in &["/bus", "/bus list", "  /bus list  "] {
            let result = parse_command(variant);
            match result {
                CommandParseResult::Command(ChatCommand::Bus { args }) => {
                    if *variant == "/bus" {
                        assert!(args.is_empty(), "bare /bus has empty args");
                    } else {
                        assert_eq!(args, "list");
                    }
                }
                other => panic!("expected Bus command for {variant}: {other:?}"),
            }
        }
    }

    #[test]
    fn test_parse_bus_send() {
        let result = parse_command("/bus send im:feishu:oc_123 你好");
        assert!(matches!(
            result,
            CommandParseResult::Command(ChatCommand::Bus { ref args })
                if args == "send im:feishu:oc_123 你好"
        ));
    }

    #[test]
    fn test_parse_bus_watch_unwatch() {
        let result = parse_command("/bus watch im:wecom:chat-456");
        assert!(matches!(
            result,
            CommandParseResult::Command(ChatCommand::Bus { ref args })
                if args == "watch im:wecom:chat-456"
        ));
        let result = parse_command("/bus unwatch im:wecom:chat-456");
        assert!(matches!(
            result,
            CommandParseResult::Command(ChatCommand::Bus { ref args })
                if args == "unwatch im:wecom:chat-456"
        ));
    }

    #[test]
    fn test_parse_non_bus_slash_still_message() {
        // /busy 不是 /bus 前缀（避免误匹配）
        let result = parse_command("/busy doing things");
        assert!(matches!(result, CommandParseResult::Message(_)));
    }

    #[test]
    fn test_parse_non_command_slash() {
        // Text starting with slashes but not matching commands
        let result = parse_command("/something_else");
        assert!(matches!(result, CommandParseResult::Message(_)));
    }
}
