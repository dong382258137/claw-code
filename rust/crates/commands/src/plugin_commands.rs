//! Claude 插件 `commands/*.md` 的加载与渲染。
//!
//! 插件命令是纯提示词模板：frontmatter 描述元信息，正文里可含
//! `$ARGUMENTS` / `$1..$9` 占位符。这里只做「发现 + 解析 + 渲染」，
//! 具体执行仍由调用方把渲染结果当作一次普通用户回合提交，避免另起
//! 一套执行路径。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{plugin_manager_for, ConfigLoader};
/// 一条来自已启用插件的 slash 命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommand {
    /// 插件元数据里的名称（用于 `/<plugin>:<command>` 形式调用）。
    pub plugin_name: String,
    /// 命令名；子目录命令用 `:` 连接（如 `frontend:component`）。
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub allowed_tools: Vec<String>,
    pub model: Option<String>,
    pub body: String,
    pub plugin_root: Option<PathBuf>,
    pub source: PathBuf,
}

impl PluginCommand {
    /// 可被用户输入命中的名称集合：裸名与 `插件名:命令名`。
    #[must_use]
    pub fn invocation_names(&self) -> Vec<String> {
        let mut names = vec![self.name.clone()];
        let qualified = format!("{}:{}", self.plugin_name, self.name);
        if qualified != self.name {
            names.push(qualified);
        }
        names
    }
}

/// 发现所有启用插件贡献的命令。
///
/// 与 skills/agents/MCP 共用 `plugin_manager_for` + `is_enabled()` 判定，
/// 避免出现「启用了但没注入」「禁用后还挂着」。
#[must_use]
pub fn discover_plugin_commands(cwd: &Path) -> Vec<PluginCommand> {
    let loader = ConfigLoader::default_for(cwd);
    let Ok(runtime_config) = loader.load() else {
        return Vec::new();
    };
    let manager = plugin_manager_for(cwd, loader.config_home(), &runtime_config);
    let Ok(registry) = manager.plugin_registry() else {
        return Vec::new();
    };

    let mut commands = Vec::new();
    for plugin in registry
        .plugins()
        .iter()
        .filter(|plugin| plugin.is_enabled())
    {
        let metadata = plugin.metadata();
        let root = metadata.root.clone();
        for directory in &plugin.components().command_paths {
            collect_commands_from(
                directory,
                directory,
                &metadata.name,
                root.as_deref(),
                &mut commands,
            );
        }
    }
    commands.sort_by(|left, right| left.name.cmp(&right.name));
    commands
}

/// 按名称查找命令；未命中返回 `None`。
#[must_use]
pub fn find_plugin_command(cwd: &Path, name: &str) -> Option<PluginCommand> {
    let name = name.trim();
    discover_plugin_commands(cwd).into_iter().find(|command| {
        command
            .invocation_names()
            .iter()
            .any(|candidate| candidate == name)
    })
}

/// 把命令模板渲染成可直接提交的用户消息。
#[must_use]
pub fn render_plugin_command(
    command: &PluginCommand,
    arguments: Option<&str>,
    cwd: &Path,
) -> String {
    let arguments = arguments.unwrap_or("").trim();
    let mut body = command.body.clone();
    body = body.replace("${CLAUDE_PROJECT_DIR}", &cwd.to_string_lossy());
    if let Some(root) = &command.plugin_root {
        body = body.replace("${CLAUDE_PLUGIN_ROOT}", &root.to_string_lossy());
    }
    body = body.replace("$ARGUMENTS", arguments);
    body = body.replace("${ARGUMENTS}", arguments);

    let positional = arguments.split_whitespace().collect::<Vec<_>>();
    for index in 1..=9 {
        let value = positional.get(index - 1).copied().unwrap_or("");
        body = body.replace(&format!("${index}"), value);
    }
    body.trim().to_string()
}

fn collect_commands_from(
    directory: &Path,
    base: &Path,
    plugin_name: &str,
    plugin_root: Option<&Path>,
    commands: &mut Vec<PluginCommand>,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_commands_from(&path, base, plugin_name, plugin_root, commands);
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let (frontmatter, body) = parse_command_markdown(&content);
        if body.trim().is_empty() {
            continue;
        }
        let relative = path.strip_prefix(base).unwrap_or(&path);
        let name = relative
            .with_extension("")
            .components()
            .filter_map(|component| component.as_os_str().to_str())
            .collect::<Vec<_>>()
            .join(":");
        if name.is_empty() {
            continue;
        }
        commands.push(PluginCommand {
            plugin_name: plugin_name.to_string(),
            name,
            description: frontmatter.get("description").cloned(),
            argument_hint: frontmatter.get("argument-hint").cloned(),
            allowed_tools: frontmatter
                .get("allowed-tools")
                .map(|value| split_tool_list(value))
                .unwrap_or_default(),
            model: frontmatter.get("model").cloned(),
            body,
            plugin_root: plugin_root.map(Path::to_path_buf),
            source: path,
        });
    }
}

fn parse_command_markdown(content: &str) -> (BTreeMap<String, String>, String) {
    let content = content.trim_start_matches('\u{feff}');
    let Some(rest) = content.strip_prefix("---") else {
        return (BTreeMap::new(), content.to_string());
    };
    let rest = rest.strip_prefix('\r').unwrap_or(rest);
    let Some(rest) = rest.strip_prefix('\n') else {
        return (BTreeMap::new(), content.to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (BTreeMap::new(), content.to_string());
    };
    let frontmatter = &rest[..end];
    let body = rest[end + 4..]
        .strip_prefix('\r')
        .unwrap_or(&rest[end + 4..])
        .strip_prefix('\n')
        .unwrap_or(&rest[end + 4..])
        .to_string();

    let mut map = BTreeMap::new();
    let mut last_key: Option<String> = None;
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(item) = trimmed.strip_prefix("- ") {
            if let Some(key) = &last_key {
                let entry = map.entry(key.clone()).or_insert_with(String::new);
                if !entry.is_empty() {
                    entry.push(',');
                }
                entry.push_str(item.trim());
            }
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().to_string();
        let value = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        last_key = Some(key.clone());
        map.insert(key, value);
    }
    (map, body)
}

fn split_tool_list(value: &str) -> Vec<String> {
    let mut tools = Vec::new();
    let mut current = String::new();
    let mut depth = 0_u32;
    for character in value.chars() {
        match character {
            '(' => {
                depth += 1;
                current.push(character);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            ',' if depth == 0 => {
                if !current.trim().is_empty() {
                    tools.push(current.trim().to_string());
                }
                current.clear();
            }
            _ => current.push(character),
        }
    }
    if !current.trim().is_empty() {
        tools.push(current.trim().to_string());
    }
    tools
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{parse_command_markdown, render_plugin_command, split_tool_list, PluginCommand};

    #[test]
    fn parses_frontmatter_and_body() {
        let content = "---\ndescription: \"Review a PR\"\nargument-hint: [pr-number]\nallowed-tools: Bash(gh pr view:*), Read\nmodel: sonnet\n---\nReview PR $ARGUMENTS.\n";
        let (frontmatter, body) = parse_command_markdown(content);
        assert_eq!(
            frontmatter.get("description").map(String::as_str),
            Some("Review a PR")
        );
        assert_eq!(
            frontmatter.get("argument-hint").map(String::as_str),
            Some("[pr-number]")
        );
        assert_eq!(frontmatter.get("model").map(String::as_str), Some("sonnet"));
        assert_eq!(body.trim(), "Review PR $ARGUMENTS.");
    }

    #[test]
    fn parses_block_style_frontmatter_lists() {
        let content = "---\ndescription: demo\nallowed-tools:\n  - Read\n  - Bash(git add:*, git status:*)\n---\nbody\n";
        let (frontmatter, _) = parse_command_markdown(content);
        let tools = split_tool_list(frontmatter.get("allowed-tools").expect("tools"));
        assert_eq!(tools, vec!["Read", "Bash(git add:*, git status:*)"]);
    }

    #[test]
    fn renders_arguments_and_plugin_root() {
        let command = PluginCommand {
            plugin_name: "demo".to_string(),
            name: "review".to_string(),
            description: None,
            argument_hint: None,
            allowed_tools: Vec::new(),
            model: None,
            body: "Run ${CLAUDE_PLUGIN_ROOT}/scripts/check.sh for $ARGUMENTS and $1".to_string(),
            plugin_root: Some(std::path::PathBuf::from("/plugins/demo")),
            source: std::path::PathBuf::from("/plugins/demo/commands/review.md"),
        };
        let rendered = render_plugin_command(&command, Some("42 extra"), Path::new("/workspace"));
        assert_eq!(
            rendered,
            "Run /plugins/demo/scripts/check.sh for 42 extra and 42"
        );
    }

    #[test]
    fn discovers_commands_from_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let commands_dir = dir.path().join("commands");
        fs::create_dir_all(commands_dir.join("frontend")).expect("mkdir");
        fs::write(
            commands_dir.join("review.md"),
            "---\ndescription: review\n---\nReview $ARGUMENTS",
        )
        .expect("write");
        fs::write(
            commands_dir.join("frontend").join("component.md"),
            "---\ndescription: component\n---\nBuild $1",
        )
        .expect("write");

        let mut commands = Vec::new();
        super::collect_commands_from(
            &commands_dir,
            &commands_dir,
            "demo",
            Some(dir.path()),
            &mut commands,
        );
        let mut names = commands
            .into_iter()
            .map(|command| command.name)
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec!["frontend:component", "review"]);
    }
}
