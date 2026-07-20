#![cfg(feature = "full-tui")]

//! Slash command popup menu with fuzzy filtering.
//!
//! When the user types a `/`-prefixed query, `SlashMenu` filters the
//! available `SlashCommandSpec` list and tracks the currently selected
//! item. Up/Down arrow keys move the selection; Enter submits the
//! selected command; Esc closes the menu.

use std::borrow::Cow;

use commands::{slash_command_specs, SlashCommandSpec};

use crate::commands_handler::STUB_COMMANDS;

/// Maximum items shown at once in the popup.
const MAX_VISIBLE_ITEMS: usize = 10;

/// A slash command menu with fuzzy-filtered items.
#[derive(Debug, Clone)]
pub(crate) struct SlashMenu {
    /// All candidate commands (loaded once from `slash_command_specs()`).
    all_items: Vec<&'static SlashCommandSpec>,
    /// Current filter query (text after the `/`).
    query: String,
    /// Currently selected index into `filtered()`, or None if no selection.
    selected: Option<usize>,
    /// Scroll offset for the visible window.
    scroll: usize,
    /// Cached filtered list (invalidated on query change).
    filtered_cache: Vec<&'static SlashCommandSpec>,
}

impl SlashMenu {
    /// Build a menu from the static `slash_command_specs()` list.
    ///
    /// Filters out STUB_COMMANDS so the popup only surfaces actually
    /// implemented commands (mirrors rustyline completion behavior in
    /// `slash_command_completion_candidates_with_sessions`).
    #[must_use]
    pub(crate) fn new() -> Self {
        let all_items = slash_command_specs()
            .iter()
            .filter(|spec| !STUB_COMMANDS.contains(&spec.name))
            .collect::<Vec<_>>();
        let selected = if all_items.is_empty() { None } else { Some(0) };
        let filtered_cache = all_items.clone();
        Self {
            all_items,
            query: String::new(),
            selected,
            scroll: 0,
            filtered_cache,
        }
    }

    /// Update the filter query (text typed after `/`). Resets selection
    /// to the first item. Empty query shows all commands.
    pub(crate) fn set_query(&mut self, query: &str) {
        if self.query == query {
            return;
        }
        self.query = query.to_string();
        self.filtered_cache = self.compute_filtered();
        self.selected = if self.filtered_cache.is_empty() { None } else { Some(0) };
        self.scroll = 0;
    }

    /// Move selection up by one (wraps to bottom).
    pub(crate) fn move_up(&mut self) {
        if let Some(idx) = self.selected {
            let len = self.filtered().len();
            if len == 0 {
                return;
            }
            let new_idx = if idx == 0 { len - 1 } else { idx - 1 };
            self.selected = Some(new_idx);
            self.adjust_scroll();
        }
    }

    /// Move selection down by one (wraps to top).
    pub(crate) fn move_down(&mut self) {
        if let Some(idx) = self.selected {
            let len = self.filtered().len();
            if len == 0 {
                return;
            }
            let new_idx = if idx + 1 >= len { 0 } else { idx + 1 };
            self.selected = Some(new_idx);
            self.adjust_scroll();
        }
    }

    /// Currently selected command spec, or None.
    pub(crate) fn selected_spec(&self) -> Option<&'static SlashCommandSpec> {
        let idx = self.selected?;
        self.filtered().get(idx).copied()
    }

    /// Reset to initial state (clear query, select first).
    pub(crate) fn reset(&mut self) {
        self.query.clear();
        self.filtered_cache = self.all_items.clone();
        self.selected = if self.all_items.is_empty() { None } else { Some(0) };
        self.scroll = 0;
    }

    /// Filtered command list based on current query (cached).
    /// Empty query → all commands. Non-empty query → commands whose name
    /// OR aliases OR summary contains the query (case-insensitive).
    pub(crate) fn filtered(&self) -> &[&'static SlashCommandSpec] {
        &self.filtered_cache
    }

    /// Compute the filtered list from scratch (called on query change).
    fn compute_filtered(&self) -> Vec<&'static SlashCommandSpec> {
        if self.query.is_empty() {
            return self.all_items.clone();
        }
        let q = self.query.to_ascii_lowercase();
        self.all_items
            .iter()
            .filter(|spec| {
                let name = spec.name.to_ascii_lowercase();
                let summary = spec.summary.to_ascii_lowercase();
                let aliases_match = spec.aliases.iter().any(|a| a.to_ascii_lowercase().contains(&q));
                name.contains(&q) || summary.contains(&q) || aliases_match
            })
            .copied()
            .collect()
    }

    /// Visible window of items (paginated by `MAX_VISIBLE_ITEMS`).
    pub(crate) fn visible_window(&self) -> Vec<&'static SlashCommandSpec> {
        let filtered = self.filtered();
        let start = self.scroll.min(filtered.len().saturating_sub(1));
        let end = (start + MAX_VISIBLE_ITEMS).min(filtered.len());
        filtered[start..end].to_vec()
    }

    /// Current scroll offset (for rendering scroll indicators).
    pub(crate) fn scroll_offset(&self) -> usize {
        self.scroll
    }

    /// Total filtered count (for rendering "N of M").
    pub(crate) fn total_count(&self) -> usize {
        self.filtered().len()
    }

    /// Currently selected index (None if nothing selected).
    pub(crate) fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    /// Index within the visible window (None if out of view).
    pub(crate) fn visible_index(&self) -> Option<usize> {
        let idx = self.selected?;
        let visible = idx.saturating_sub(self.scroll);
        if visible < MAX_VISIBLE_ITEMS {
            Some(visible)
        } else {
            None
        }
    }

    /// Total number of all candidate commands (ignoring filter).
    pub(crate) fn all_items_count(&self) -> usize {
        self.all_items.len()
    }

    fn adjust_scroll(&mut self) {
        if let Some(idx) = self.selected {
            if idx < self.scroll {
                self.scroll = idx;
            } else if idx >= self.scroll + MAX_VISIBLE_ITEMS {
                self.scroll = idx + 1 - MAX_VISIBLE_ITEMS;
            }
        }
    }
}

impl Default for SlashMenu {
    fn default() -> Self {
        Self::new()
    }
}

/// Render a single slash command spec as a display string for the popup.
/// Format: `/name [aliases]  summary — 中文注释`
///
/// 中文注释来自 `chinese_summary` 映射表；未覆盖的命令只显示英文 summary。
pub(crate) fn format_menu_item(spec: &SlashCommandSpec) -> Cow<'static, str> {
    let mut s = String::new();
    s.push('/');
    s.push_str(spec.name);
    if !spec.aliases.is_empty() {
        s.push_str(", /");
        s.push_str(&spec.aliases.join(", /"));
    }
    s.push_str("  ");
    s.push_str(spec.summary);
    if let Some(zh) = chinese_summary(spec.name) {
        s.push_str(" — ");
        s.push_str(zh);
    }
    Cow::Owned(s)
}

/// 中文命令注释映射表（按命令名查找）。
///
/// 仅用于 TUI 菜单显示，不影响 CLI 帮助文本。未列出的命令将不附加中文注释。
/// 翻译遵循"简短描述"原则，保留命令名英文，仅翻译语义说明。
#[allow(unreachable_patterns)]
fn chinese_summary(name: &str) -> Option<&'static str> {
    Some(match name {
        // 会话类
        "help" => "显示所有斜杠命令",
        "status" => "显示当前会话状态",
        "sandbox" => "显示沙箱隔离状态",
        "compact" => "压缩本地会话历史",
        "clear" => "开启新的本地会话",
        "cost" => "显示本会话累计 token 用量",
        "usage" => "显示用量统计",
        "stats" => "显示会话统计",
        "resume" => "加载已保存的会话到 REPL",
        "session" => "列出/切换/分叉/删除受管会话",
        "rename" => "重命名当前会话",
        "export" => "导出当前对话到文件",
        "search" => "按关键词搜索对话历史",
        "history" => "查看历史",
        "summary" => "生成会话摘要",
        "tag" => "给当前会话打标签",
        "copy" => "复制最近一条回复到剪贴板",
        "share" => "分享当前对话",
        "feedback" => "提交反馈",
        "rewind" => "回退到之前的对话状态",
        "context" => "查看上下文使用情况",
        "tokens" => "显示 token 详情",
        "cache" => "缓存管理",
        "exit" => "退出 CLI",
        "undo" => "撤销最近一次文件编辑",
        "retry" => "重试上一轮",
        "stop" => "停止当前轮次",
        "version" => "显示 CLI 版本与构建信息",
        "bookmarks" => "查看书签",
        "pin" => "固定消息",
        "unpin" => "取消固定",
        "files" => "查看会话相关文件",
        "focus" => "进入聚焦模式",
        "unfocus" => "退出聚焦模式",

        // 配置类
        "model" => "显示或切换当前模型",
        "permissions" => "显示或切换当前权限模式",
        "config" => "查看 Claude 配置文件或合并节",
        "memory" => "查看已加载的 Claude 指令记忆文件",
        "mcp" => "查看已配置的 MCP 服务器",
        "theme" => "切换或查看主题",
        "vim" => "切换 vim 编辑模式",
        "voice" => "切换语音输入",
        "color" => "切换颜色主题",
        "effort" => "设置模型推理努力等级",
        "fast" => "切换快速模式",
        "brief" => "切换简短输出模式",
        "output-style" => "设置输出样式",
        "keybindings" => "查看或自定义快捷键",
        "privacy-settings" => "隐私设置",
        "stickers" => "查看 stickers",
        "language" => "设置语言",
        "profile" => "切换 profile",
        "max-tokens" => "设置最大 token 数",
        "temperature" => "设置温度",
        "system-prompt" => "查看或设置系统提示词",
        "api-key" => "设置 API key",
        "terminal-setup" => "终端设置",
        "notifications" => "通知设置",
        "telemetry" => "遥测设置",
        "providers" => "查看可用 providers",
        "env" => "查看环境变量",
        "project" => "项目管理",
        "reasoning" => "推理设置",
        "budget" => "预算设置",
        "rate-limit" => "速率限制",
        "workspace" => "工作区管理",
        "reset" => "重置配置",
        "ide" => "IDE 集成",
        "desktop" => "桌面集成",
        "upgrade" => "升级 CLI 到最新版本",
        "add-dir" => "添加目录到工作区",
        "poor" => "切换穷人模式（省 token）",
        "goal" => "设置或查看目标",
        "bg" => "后台任务管理",
        "allowed-tools" => "查看/管理允许的工具",
        "hooks" => "查看 hooks 配置",
        "format" => "设置输出格式",
        "tool-details" => "工具调用详情",
        "insights" => "查看洞察",
        "thinkback" => "显示历史推理",
        "release-notes" => "查看发布说明",
        "advisor" => "启用 advisor",

        // 调试类
        "debug-tool-call" => "重放最近一次工具调用并显示调试信息",
        "doctor" => "诊断 CLI 配置问题",
        "diagnostics" => "诊断信息",
        "changelog" => "查看变更日志",
        "metrics" => "指标",

        // 工具类
        "init" => "为本仓库创建 CLAUDE.md 模板",
        "diff" => "显示当前工作区的 git diff",
        "bughunter" => "检查代码库中潜在的 bug",
        "commit" => "生成 commit message 并创建 git commit",
        "pr" => "从对话中起草或创建 PR",
        "issue" => "从对话中起草或创建 GitHub issue",
        "ultraplan" => "运行深度规划提示词（多步推理）",
        "teleport" => "通过搜索工作区跳转到文件或符号",
        "plan" => "进入规划模式",
        "review" => "代码审查模式",
        "tasks" => "任务管理",
        "security-review" => "安全审查模式",
        "approve" => "批准工具调用",
        "deny" => "拒绝工具调用",
        "paste" => "粘贴内容",
        "screenshot" => "截屏",
        "image" => "上传图片",
        "listen" => "监听语音",
        "speak" => "朗读文本",
        "branch" => "切换或创建 git 分支",
        "test" => "运行测试",
        "lint" => "运行 lint",
        "build" => "构建项目",
        "run" => "运行命令",
        "git" => "git 操作",
        "stash" => "git stash 管理",
        "blame" => "git blame",
        "log" => "git log",
        "cron" => "定时任务管理",
        "team" => "团队管理",
        "benchmark" => "性能基准测试",
        "migrate" => "迁移",
        "templates" => "管理模板",
        "explain" => "解释代码",
        "refactor" => "重构代码",
        "docs" => "生成文档",
        "fix" => "修复问题",
        "perf" => "性能优化",
        "chat" => "聊天模式",
        "web" => "网络搜索",
        "map" => "代码地图",
        "symbols" => "查找符号",
        "references" => "查找引用",
        "definition" => "跳转到定义",
        "hover" => "悬停信息",
        "autofix" => "自动修复",
        "multi" => "多模型对话",
        "macro" => "宏录制",
        "alias" => "别名管理",
        "parallel" => "并行执行",
        "agent" => "启动 agent",
        "subagent" => "启动 subagent",
        "demo" => "演示",
        "sample" => "示例",
        "plugin" => "查看已配置的插件",
        "agents" => "查看已配置的 agents",
        "skills" => "查看已配置的 skills",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_menu_has_all_specs_and_first_selected() {
        let menu = SlashMenu::new();
        assert!(!menu.all_items.is_empty(), "slash_command_specs should return commands");
        assert_eq!(menu.selected, Some(0));
        assert_eq!(menu.scroll, 0);
    }

    #[test]
    fn empty_query_shows_all() {
        let menu = SlashMenu::new();
        let all = menu.all_items.len();
        assert_eq!(menu.filtered().len(), all);
    }

    #[test]
    fn query_filters_by_name_substring() {
        let mut menu = SlashMenu::new();
        menu.set_query("hel");
        let filtered = menu.filtered();
        assert!(filtered.iter().any(|s| s.name == "help"), "should find 'help'");
        for spec in filtered {
            let name_lower = spec.name.to_ascii_lowercase();
            let summary_lower = spec.summary.to_ascii_lowercase();
            let alias_match = spec.aliases.iter().any(|a| a.to_ascii_lowercase().contains("hel"));
            assert!(
                name_lower.contains("hel") || summary_lower.contains("hel") || alias_match,
                "filtered item '{}' should match query 'hel'",
                spec.name
            );
        }
    }

    #[test]
    fn query_matches_aliases() {
        let mut menu = SlashMenu::new();
        menu.set_query("mcp");
        let filtered = menu.filtered();
        assert!(filtered.iter().any(|s| s.name == "mcp"));
    }

    #[test]
    fn query_matches_summary_substring() {
        let mut menu = SlashMenu::new();
        menu.set_query("session");
        let filtered = menu.filtered();
        assert!(filtered.iter().any(|s| s.name == "status"));
    }

    #[test]
    fn move_down_wraps_to_top() {
        let mut menu = SlashMenu::new();
        let last_idx = menu.filtered().len() - 1;
        menu.selected = Some(last_idx);
        menu.move_down();
        assert_eq!(menu.selected, Some(0), "should wrap to top");
    }

    #[test]
    fn move_up_wraps_to_bottom() {
        let mut menu = SlashMenu::new();
        menu.selected = Some(0);
        let last_idx = menu.filtered().len() - 1;
        menu.move_up();
        assert_eq!(menu.selected, Some(last_idx), "should wrap to bottom");
    }

    #[test]
    fn selected_spec_returns_current() {
        let mut menu = SlashMenu::new();
        menu.selected = Some(0);
        let spec = menu.selected_spec();
        assert!(spec.is_some());
        assert_eq!(spec.unwrap().name, menu.all_items[0].name);
    }

    #[test]
    fn set_query_resets_selection_to_first() {
        let mut menu = SlashMenu::new();
        menu.selected = Some(5);
        menu.set_query("hel");
        assert_eq!(menu.selected, Some(0));
        assert_eq!(menu.scroll, 0);
    }

    #[test]
    fn set_query_with_no_matches_clears_selection() {
        let mut menu = SlashMenu::new();
        menu.set_query("zzz_nomatch_zzz");
        assert_eq!(menu.filtered().len(), 0);
        assert_eq!(menu.selected, None);
    }

    #[test]
    fn scroll_adjusts_when_moving_past_bottom_of_window() {
        let mut menu = SlashMenu::new();
        let big_idx = 15.min(menu.all_items.len().saturating_sub(1));
        if big_idx >= MAX_VISIBLE_ITEMS {
            menu.selected = Some(big_idx);
            menu.adjust_scroll();
            assert!(
                menu.scroll + MAX_VISIBLE_ITEMS > big_idx,
                "scroll should make selected visible"
            );
            assert!(menu.visible_index().is_some(), "selected should be in visible window");
        }
    }

    #[test]
    fn visible_window_returns_at_most_max_items() {
        let menu = SlashMenu::new();
        let visible = menu.visible_window();
        assert!(visible.len() <= MAX_VISIBLE_ITEMS);
    }

    #[test]
    fn reset_clears_query_and_selects_first() {
        let mut menu = SlashMenu::new();
        menu.set_query("hel");
        menu.reset();
        assert!(menu.query.is_empty());
        assert_eq!(menu.filtered().len(), menu.all_items.len());
        assert_eq!(menu.selected, Some(0));
    }

    #[test]
    fn format_menu_item_includes_name_and_summary() {
        let menu = SlashMenu::new();
        let first = menu.all_items[0];
        let s = format_menu_item(first);
        assert!(s.starts_with('/'));
        assert!(s.contains(first.name));
        assert!(s.contains(first.summary));
    }

    #[test]
    fn all_items_count_matches_static_specs() {
        // P2-2 修复：SlashMenu::new() 现在过滤 STUB_COMMANDS，
        // 因此 all_items_count 应等于 specs 总数减去 STUB 数量。
        let menu = SlashMenu::new();
        let non_stub_count = slash_command_specs()
            .iter()
            .filter(|spec| !STUB_COMMANDS.contains(&spec.name))
            .count();
        assert_eq!(menu.all_items_count(), non_stub_count);
        // 过滤后所有 item 都不应是 STUB
        for item in &menu.all_items {
            assert!(!STUB_COMMANDS.contains(&item.name), "stub leaked: {}", item.name);
        }
    }
}
