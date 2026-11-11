pub mod test_isolation;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const EXTERNAL_MARKETPLACE: &str = "external";
const BUILTIN_MARKETPLACE: &str = "builtin";
const BUNDLED_MARKETPLACE: &str = "bundled";
const SETTINGS_FILE_NAME: &str = "settings.json";
const REGISTRY_FILE_NAME: &str = "installed.json";
const MANIFEST_FILE_NAME: &str = "plugin.json";
const MANIFEST_RELATIVE_PATH: &str = ".claude-plugin/plugin.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    Builtin,
    Bundled,
    External,
}

impl Display for PluginKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Builtin => write!(f, "builtin"),
            Self::Bundled => write!(f, "bundled"),
            Self::External => write!(f, "external"),
        }
    }
}

impl PluginKind {
    #[must_use]
    fn marketplace(self) -> &'static str {
        match self {
            Self::Builtin => BUILTIN_MARKETPLACE,
            Self::Bundled => BUNDLED_MARKETPLACE,
            Self::External => EXTERNAL_MARKETPLACE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginMetadata {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub kind: PluginKind,
    pub source: String,
    pub default_enabled: bool,
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginHooks {
    #[serde(rename = "PreToolUse", default)]
    pub pre_tool_use: Vec<String>,
    #[serde(rename = "PostToolUse", default)]
    pub post_tool_use: Vec<String>,
    #[serde(rename = "PostToolUseFailure", default)]
    pub post_tool_use_failure: Vec<String>,
}

impl PluginHooks {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.post_tool_use_failure.is_empty()
    }

    #[must_use]
    pub fn merged_with(&self, other: &Self) -> Self {
        let mut merged = self.clone();
        merged
            .pre_tool_use
            .extend(other.pre_tool_use.iter().cloned());
        merged
            .post_tool_use
            .extend(other.post_tool_use.iter().cloned());
        merged
            .post_tool_use_failure
            .extend(other.post_tool_use_failure.iter().cloned());
        merged
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginLifecycle {
    #[serde(rename = "Init", default)]
    pub init: Vec<String>,
    #[serde(rename = "Shutdown", default)]
    pub shutdown: Vec<String>,
}

impl PluginLifecycle {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.init.is_empty() && self.shutdown.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub permissions: Vec<PluginPermission>,
    #[serde(rename = "defaultEnabled", default)]
    pub default_enabled: bool,
    #[serde(default)]
    pub hooks: PluginHooks,
    #[serde(default)]
    pub lifecycle: PluginLifecycle,
    #[serde(default)]
    pub tools: Vec<PluginToolManifest>,
    #[serde(default)]
    pub commands: Vec<PluginCommandManifest>,
    /// Claude Code 插件契约组件（skills/agents/commands/hooks/mcpServers/userConfig）。
    #[serde(default)]
    pub components: ClaudeComponents,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginPermission {
    Read,
    Write,
    Execute,
}

impl PluginPermission {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "execute" => Some(Self::Execute),
            _ => None,
        }
    }
}

impl AsRef<str> for PluginPermission {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginToolManifest {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub required_permission: PluginToolPermission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginToolPermission {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl PluginToolPermission {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "danger-full-access" => Some(Self::DangerFullAccess),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCommandManifest {
    pub name: String,
    pub description: String,
    pub command: String,
}

/// Claude Code 插件里的一类 hook 事件（已从 hooks.json / 内联对象归一化）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeHookSpec {
    /// Claude 事件名，如 `PreToolUse` / `SessionStart`。
    pub event: String,
    /// 工具名过滤表达式（Claude 的 `matcher`），为空表示不限制。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// Claude Code 插件声明的 MCP 服务器（内联或 `.mcp.json` 读取后归一化）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginMcpServerSpec {
    pub name: String,
    /// `stdio` / `http` / `sse` / `ws`。
    pub transport: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// 远程服务器的显式代理（`http://user:pass@host:port`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
}

/// Claude 插件 `userConfig` 的一项；安装确认页据此向用户收集值。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginUserConfigField {
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// Claude Code 插件契约里 claw 能执行/需要展示的组件集合。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeComponents {
    /// 已解析为绝对路径的技能目录根（可直接加入技能发现根）。
    #[serde(default)]
    pub skill_paths: Vec<PathBuf>,
    /// 已解析为绝对路径的子代理目录根（可直接加入子代理发现根）。
    #[serde(default)]
    pub agent_paths: Vec<PathBuf>,
    /// 命令 markdown 文件或目录（动态命令表读取）。
    #[serde(default)]
    pub command_paths: Vec<PathBuf>,
    #[serde(default)]
    pub hooks: Vec<ClaudeHookSpec>,
    #[serde(default)]
    pub mcp_servers: Vec<PluginMcpServerSpec>,
    #[serde(default)]
    pub user_config: Vec<PluginUserConfigField>,
    /// 无法映射的 Claude 字段/事件，展示给用户，不静默丢弃。
    #[serde(default)]
    pub unsupported: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct RawPluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(rename = "defaultEnabled", default)]
    pub default_enabled: bool,
    /// claw 原生 hooks（字符串数组）或 Claude hooks（文件路径/对象），两种都接受。
    #[serde(default)]
    pub hooks: Value,
    #[serde(default)]
    pub lifecycle: PluginLifecycle,
    #[serde(default)]
    pub tools: Vec<RawPluginToolManifest>,
    /// claw 原生命令对象数组，或 Claude 的命令目录/文件列表。
    #[serde(default)]
    pub commands: Option<Value>,
    #[serde(default)]
    pub skills: Option<Value>,
    #[serde(default)]
    pub agents: Option<Value>,
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: Option<Value>,
    #[serde(rename = "userConfig", default)]
    pub user_config: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RawPluginToolManifest {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(
        rename = "requiredPermission",
        default = "default_tool_permission_label"
    )]
    pub required_permission: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginTool {
    plugin_id: String,
    plugin_name: String,
    definition: PluginToolDefinition,
    command: String,
    args: Vec<String>,
    required_permission: PluginToolPermission,
    root: Option<PathBuf>,
}

impl PluginTool {
    #[must_use]
    pub fn new(
        plugin_id: impl Into<String>,
        plugin_name: impl Into<String>,
        definition: PluginToolDefinition,
        command: impl Into<String>,
        args: Vec<String>,
        required_permission: PluginToolPermission,
        root: Option<PathBuf>,
    ) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            plugin_name: plugin_name.into(),
            definition,
            command: command.into(),
            args,
            required_permission,
            root,
        }
    }

    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    #[must_use]
    pub fn definition(&self) -> &PluginToolDefinition {
        &self.definition
    }

    #[must_use]
    pub fn required_permission(&self) -> &str {
        self.required_permission.as_str()
    }

    pub fn execute(&self, input: &Value) -> Result<String, PluginError> {
        let input_json = input.to_string();
        let mut process = Command::new(&self.command);
        process
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("CLAWD_PLUGIN_ID", &self.plugin_id)
            .env("CLAWD_PLUGIN_NAME", &self.plugin_name)
            .env("CLAWD_TOOL_NAME", &self.definition.name)
            .env("CLAWD_TOOL_INPUT", &input_json);
        if let Some(root) = &self.root {
            process
                .current_dir(root)
                .env("CLAWD_PLUGIN_ROOT", root.display().to_string());
        }

        let mut child = process.spawn()?;
        if let Some(stdin) = child.stdin.as_mut() {
            use std::io::Write as _;
            stdin.write_all(input_json.as_bytes())?;
        }

        let output = child.wait_with_output()?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(PluginError::CommandFailed(format!(
                "plugin tool `{}` from `{}` failed for `{}`: {}",
                self.definition.name,
                self.plugin_id,
                self.command,
                if stderr.is_empty() {
                    format!("exit status {}", output.status)
                } else {
                    stderr
                }
            )))
        }
    }
}

fn default_tool_permission_label() -> String {
    "danger-full-access".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginInstallSource {
    LocalPath { path: PathBuf },
    GitUrl { url: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledPluginRecord {
    #[serde(default = "default_plugin_kind")]
    pub kind: PluginKind,
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub install_path: PathBuf,
    pub source: PluginInstallSource,
    pub installed_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    /// 安装时校验过的插件目录树 sha256；用于更新时复核。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// 固定的 git commit（完整性锚点之一）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// 仓库内子目录（Claude marketplace 的 `git-subdir`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<PathBuf>,
}

/// 市场安装请求：来源 + 完整性锚点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInstallRequest {
    pub source: PluginInstallSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// 插件目录树 sha256；与固定 git commit 至少要提供一个。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledPluginRegistry {
    #[serde(default)]
    pub plugins: BTreeMap<String, InstalledPluginRecord>,
}

fn default_plugin_kind() -> PluginKind {
    PluginKind::External
}

#[derive(Debug, Clone, PartialEq)]
pub struct BuiltinPlugin {
    metadata: PluginMetadata,
    hooks: PluginHooks,
    lifecycle: PluginLifecycle,
    tools: Vec<PluginTool>,
    components: ClaudeComponents,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BundledPlugin {
    metadata: PluginMetadata,
    hooks: PluginHooks,
    lifecycle: PluginLifecycle,
    tools: Vec<PluginTool>,
    components: ClaudeComponents,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExternalPlugin {
    metadata: PluginMetadata,
    hooks: PluginHooks,
    lifecycle: PluginLifecycle,
    tools: Vec<PluginTool>,
    components: ClaudeComponents,
}

pub trait Plugin {
    fn metadata(&self) -> &PluginMetadata;
    fn hooks(&self) -> &PluginHooks;
    fn lifecycle(&self) -> &PluginLifecycle;
    fn tools(&self) -> &[PluginTool];
    fn components(&self) -> &ClaudeComponents;
    fn validate(&self) -> Result<(), PluginError>;
    fn initialize(&self) -> Result<(), PluginError>;
    fn shutdown(&self) -> Result<(), PluginError>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum PluginDefinition {
    Builtin(BuiltinPlugin),
    Bundled(BundledPlugin),
    External(ExternalPlugin),
}

impl Plugin for BuiltinPlugin {
    fn metadata(&self) -> &PluginMetadata {
        &self.metadata
    }

    fn hooks(&self) -> &PluginHooks {
        &self.hooks
    }

    fn lifecycle(&self) -> &PluginLifecycle {
        &self.lifecycle
    }

    fn tools(&self) -> &[PluginTool] {
        &self.tools
    }

    fn components(&self) -> &ClaudeComponents {
        &self.components
    }

    fn validate(&self) -> Result<(), PluginError> {
        Ok(())
    }

    fn initialize(&self) -> Result<(), PluginError> {
        Ok(())
    }

    fn shutdown(&self) -> Result<(), PluginError> {
        Ok(())
    }
}

impl Plugin for BundledPlugin {
    fn metadata(&self) -> &PluginMetadata {
        &self.metadata
    }

    fn hooks(&self) -> &PluginHooks {
        &self.hooks
    }

    fn lifecycle(&self) -> &PluginLifecycle {
        &self.lifecycle
    }

    fn tools(&self) -> &[PluginTool] {
        &self.tools
    }

    fn components(&self) -> &ClaudeComponents {
        &self.components
    }

    fn validate(&self) -> Result<(), PluginError> {
        validate_hook_paths(self.metadata.root.as_deref(), &self.hooks)?;
        validate_lifecycle_paths(self.metadata.root.as_deref(), &self.lifecycle)?;
        validate_tool_paths(self.metadata.root.as_deref(), &self.tools)
    }

    fn initialize(&self) -> Result<(), PluginError> {
        run_lifecycle_commands(
            self.metadata(),
            self.lifecycle(),
            "init",
            &self.lifecycle.init,
        )
    }

    fn shutdown(&self) -> Result<(), PluginError> {
        run_lifecycle_commands(
            self.metadata(),
            self.lifecycle(),
            "shutdown",
            &self.lifecycle.shutdown,
        )
    }
}

impl Plugin for ExternalPlugin {
    fn metadata(&self) -> &PluginMetadata {
        &self.metadata
    }

    fn hooks(&self) -> &PluginHooks {
        &self.hooks
    }

    fn lifecycle(&self) -> &PluginLifecycle {
        &self.lifecycle
    }

    fn tools(&self) -> &[PluginTool] {
        &self.tools
    }

    fn components(&self) -> &ClaudeComponents {
        &self.components
    }

    fn validate(&self) -> Result<(), PluginError> {
        validate_hook_paths(self.metadata.root.as_deref(), &self.hooks)?;
        validate_lifecycle_paths(self.metadata.root.as_deref(), &self.lifecycle)?;
        validate_tool_paths(self.metadata.root.as_deref(), &self.tools)
    }

    fn initialize(&self) -> Result<(), PluginError> {
        run_lifecycle_commands(
            self.metadata(),
            self.lifecycle(),
            "init",
            &self.lifecycle.init,
        )
    }

    fn shutdown(&self) -> Result<(), PluginError> {
        run_lifecycle_commands(
            self.metadata(),
            self.lifecycle(),
            "shutdown",
            &self.lifecycle.shutdown,
        )
    }
}

impl Plugin for PluginDefinition {
    fn metadata(&self) -> &PluginMetadata {
        match self {
            Self::Builtin(plugin) => plugin.metadata(),
            Self::Bundled(plugin) => plugin.metadata(),
            Self::External(plugin) => plugin.metadata(),
        }
    }

    fn hooks(&self) -> &PluginHooks {
        match self {
            Self::Builtin(plugin) => plugin.hooks(),
            Self::Bundled(plugin) => plugin.hooks(),
            Self::External(plugin) => plugin.hooks(),
        }
    }

    fn lifecycle(&self) -> &PluginLifecycle {
        match self {
            Self::Builtin(plugin) => plugin.lifecycle(),
            Self::Bundled(plugin) => plugin.lifecycle(),
            Self::External(plugin) => plugin.lifecycle(),
        }
    }

    fn tools(&self) -> &[PluginTool] {
        match self {
            Self::Builtin(plugin) => plugin.tools(),
            Self::Bundled(plugin) => plugin.tools(),
            Self::External(plugin) => plugin.tools(),
        }
    }

    fn components(&self) -> &ClaudeComponents {
        match self {
            Self::Builtin(plugin) => plugin.components(),
            Self::Bundled(plugin) => plugin.components(),
            Self::External(plugin) => plugin.components(),
        }
    }

    fn validate(&self) -> Result<(), PluginError> {
        match self {
            Self::Builtin(plugin) => plugin.validate(),
            Self::Bundled(plugin) => plugin.validate(),
            Self::External(plugin) => plugin.validate(),
        }
    }

    fn initialize(&self) -> Result<(), PluginError> {
        match self {
            Self::Builtin(plugin) => plugin.initialize(),
            Self::Bundled(plugin) => plugin.initialize(),
            Self::External(plugin) => plugin.initialize(),
        }
    }

    fn shutdown(&self) -> Result<(), PluginError> {
        match self {
            Self::Builtin(plugin) => plugin.shutdown(),
            Self::Bundled(plugin) => plugin.shutdown(),
            Self::External(plugin) => plugin.shutdown(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredPlugin {
    definition: PluginDefinition,
    enabled: bool,
}

impl RegisteredPlugin {
    #[must_use]
    pub fn new(definition: PluginDefinition, enabled: bool) -> Self {
        Self {
            definition,
            enabled,
        }
    }

    #[must_use]
    pub fn metadata(&self) -> &PluginMetadata {
        self.definition.metadata()
    }

    #[must_use]
    pub fn hooks(&self) -> &PluginHooks {
        self.definition.hooks()
    }

    #[must_use]
    pub fn tools(&self) -> &[PluginTool] {
        self.definition.tools()
    }

    /// Claude 组件（skills/agents/commands/hooks/mcpServers），用于运行时注入。
    #[must_use]
    pub fn components(&self) -> &ClaudeComponents {
        self.definition.components()
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn validate(&self) -> Result<(), PluginError> {
        self.definition.validate()
    }

    pub fn initialize(&self) -> Result<(), PluginError> {
        self.definition.initialize()
    }

    pub fn shutdown(&self) -> Result<(), PluginError> {
        self.definition.shutdown()
    }

    #[must_use]
    pub fn summary(&self) -> PluginSummary {
        PluginSummary {
            metadata: self.metadata().clone(),
            enabled: self.enabled,
            lifecycle: self.definition.lifecycle().clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSummary {
    pub metadata: PluginMetadata,
    pub enabled: bool,
    pub lifecycle: PluginLifecycle,
}

impl PluginSummary {
    #[must_use]
    pub fn lifecycle_state(&self) -> &'static str {
        if self.enabled {
            "ready"
        } else {
            "disabled"
        }
    }
}

#[derive(Debug)]
pub struct PluginLoadFailure {
    pub plugin_root: PathBuf,
    pub kind: PluginKind,
    pub source: String,
    error: Box<PluginError>,
}

impl PluginLoadFailure {
    #[must_use]
    pub fn new(plugin_root: PathBuf, kind: PluginKind, source: String, error: PluginError) -> Self {
        Self {
            plugin_root,
            kind,
            source,
            error: Box::new(error),
        }
    }

    #[must_use]
    pub fn error(&self) -> &PluginError {
        self.error.as_ref()
    }
}

impl Display for PluginLoadFailure {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "failed to load {} plugin from `{}` (source: {}): {}",
            self.kind,
            self.plugin_root.display(),
            self.source,
            self.error()
        )
    }
}

#[derive(Debug)]
pub struct PluginRegistryReport {
    registry: PluginRegistry,
    failures: Vec<PluginLoadFailure>,
}

impl PluginRegistryReport {
    #[must_use]
    pub fn new(registry: PluginRegistry, failures: Vec<PluginLoadFailure>) -> Self {
        Self { registry, failures }
    }

    #[must_use]
    pub fn registry(&self) -> &PluginRegistry {
        &self.registry
    }

    #[must_use]
    pub fn failures(&self) -> &[PluginLoadFailure] {
        &self.failures
    }

    #[must_use]
    pub fn has_failures(&self) -> bool {
        !self.failures.is_empty()
    }

    #[must_use]
    pub fn summaries(&self) -> Vec<PluginSummary> {
        self.registry.summaries()
    }

    pub fn into_registry(self) -> Result<PluginRegistry, PluginError> {
        if self.failures.is_empty() {
            Ok(self.registry)
        } else {
            Err(PluginError::LoadFailures(self.failures))
        }
    }
}

#[derive(Debug, Default)]
struct PluginDiscovery {
    plugins: Vec<PluginDefinition>,
    failures: Vec<PluginLoadFailure>,
}

impl PluginDiscovery {
    fn push_plugin(&mut self, plugin: PluginDefinition) {
        self.plugins.push(plugin);
    }

    fn push_failure(&mut self, failure: PluginLoadFailure) {
        self.failures.push(failure);
    }

    fn extend(&mut self, other: Self) {
        self.plugins.extend(other.plugins);
        self.failures.extend(other.failures);
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginRegistry {
    plugins: Vec<RegisteredPlugin>,
}

impl PluginRegistry {
    #[must_use]
    pub fn new(mut plugins: Vec<RegisteredPlugin>) -> Self {
        plugins.sort_by(|left, right| left.metadata().id.cmp(&right.metadata().id));
        Self { plugins }
    }

    #[must_use]
    pub fn plugins(&self) -> &[RegisteredPlugin] {
        &self.plugins
    }

    #[must_use]
    pub fn get(&self, plugin_id: &str) -> Option<&RegisteredPlugin> {
        self.plugins
            .iter()
            .find(|plugin| plugin.metadata().id == plugin_id)
    }

    #[must_use]
    pub fn contains(&self, plugin_id: &str) -> bool {
        self.get(plugin_id).is_some()
    }

    #[must_use]
    pub fn summaries(&self) -> Vec<PluginSummary> {
        self.plugins.iter().map(RegisteredPlugin::summary).collect()
    }

    pub fn aggregated_hooks(&self) -> Result<PluginHooks, PluginError> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.is_enabled())
            .try_fold(PluginHooks::default(), |acc, plugin| {
                plugin.validate()?;
                Ok(acc.merged_with(plugin.hooks()))
            })
    }

    pub fn aggregated_tools(&self) -> Result<Vec<PluginTool>, PluginError> {
        let mut tools = Vec::new();
        let mut seen_names = BTreeMap::new();
        for plugin in self.plugins.iter().filter(|plugin| plugin.is_enabled()) {
            plugin.validate()?;
            for tool in plugin.tools() {
                if let Some(existing_plugin) =
                    seen_names.insert(tool.definition().name.clone(), tool.plugin_id().to_string())
                {
                    return Err(PluginError::InvalidManifest(format!(
                        "plugin tool `{}` is defined by both `{existing_plugin}` and `{}`",
                        tool.definition().name,
                        tool.plugin_id()
                    )));
                }
                tools.push(tool.clone());
            }
        }
        Ok(tools)
    }

    pub fn initialize(&self) -> Result<(), PluginError> {
        for plugin in self.plugins.iter().filter(|plugin| plugin.is_enabled()) {
            plugin.validate()?;
            plugin.initialize()?;
        }
        Ok(())
    }

    pub fn shutdown(&self) -> Result<(), PluginError> {
        for plugin in self
            .plugins
            .iter()
            .rev()
            .filter(|plugin| plugin.is_enabled())
        {
            plugin.shutdown()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManagerConfig {
    pub config_home: PathBuf,
    pub enabled_plugins: BTreeMap<String, bool>,
    pub external_dirs: Vec<PathBuf>,
    pub install_root: Option<PathBuf>,
    pub registry_path: Option<PathBuf>,
    pub bundled_root: Option<PathBuf>,
}

impl PluginManagerConfig {
    #[must_use]
    pub fn new(config_home: impl Into<PathBuf>) -> Self {
        Self {
            config_home: config_home.into(),
            enabled_plugins: BTreeMap::new(),
            external_dirs: Vec::new(),
            install_root: None,
            registry_path: None,
            bundled_root: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManager {
    config: PluginManagerConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    pub plugin_id: String,
    pub version: String,
    pub install_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub plugin_id: String,
    pub old_version: String,
    pub new_version: String,
    pub install_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginManifestValidationError {
    EmptyField {
        field: &'static str,
    },
    EmptyEntryField {
        kind: &'static str,
        field: &'static str,
        name: Option<String>,
    },
    InvalidPermission {
        permission: String,
    },
    DuplicatePermission {
        permission: String,
    },
    DuplicateEntry {
        kind: &'static str,
        name: String,
    },
    MissingPath {
        kind: &'static str,
        path: PathBuf,
    },
    PathIsDirectory {
        kind: &'static str,
        path: PathBuf,
    },
    InvalidToolInputSchema {
        tool_name: String,
    },
    InvalidToolRequiredPermission {
        tool_name: String,
        permission: String,
    },
    UnsupportedManifestContract {
        detail: String,
    },
}

impl Display for PluginManifestValidationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyField { field } => {
                write!(f, "plugin manifest {field} cannot be empty")
            }
            Self::EmptyEntryField { kind, field, name } => match name {
                Some(name) if !name.is_empty() => {
                    write!(f, "plugin {kind} `{name}` {field} cannot be empty")
                }
                _ => write!(f, "plugin {kind} {field} cannot be empty"),
            },
            Self::InvalidPermission { permission } => {
                write!(
                    f,
                    "plugin manifest permission `{permission}` must be one of read, write, or execute"
                )
            }
            Self::DuplicatePermission { permission } => {
                write!(f, "plugin manifest permission `{permission}` is duplicated")
            }
            Self::DuplicateEntry { kind, name } => {
                write!(f, "plugin {kind} `{name}` is duplicated")
            }
            Self::MissingPath { kind, path } => {
                write!(f, "{kind} path `{}` does not exist", path.display())
            }
            Self::PathIsDirectory { kind, path } => {
                write!(f, "{kind} path `{}` must point to a file", path.display())
            }
            Self::InvalidToolInputSchema { tool_name } => {
                write!(
                    f,
                    "plugin tool `{tool_name}` inputSchema must be a JSON object"
                )
            }
            Self::InvalidToolRequiredPermission {
                tool_name,
                permission,
            } => write!(
                f,
                "plugin tool `{tool_name}` requiredPermission `{permission}` must be read-only, workspace-write, or danger-full-access"
            ),
            Self::UnsupportedManifestContract { detail } => f.write_str(detail),
        }
    }
}

#[derive(Debug)]
pub enum PluginError {
    Io(std::io::Error),
    Json(serde_json::Error),
    ManifestValidation(Vec<PluginManifestValidationError>),
    LoadFailures(Vec<PluginLoadFailure>),
    InvalidManifest(String),
    NotFound(String),
    CommandFailed(String),
}

impl Display for PluginError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "{error}"),
            Self::ManifestValidation(errors) => {
                for (index, error) in errors.iter().enumerate() {
                    if index > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{error}")?;
                }
                Ok(())
            }
            Self::LoadFailures(failures) => {
                for (index, failure) in failures.iter().enumerate() {
                    if index > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{failure}")?;
                }
                Ok(())
            }
            Self::InvalidManifest(message)
            | Self::NotFound(message)
            | Self::CommandFailed(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for PluginError {}

impl From<std::io::Error> for PluginError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for PluginError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl PluginManager {
    #[must_use]
    pub fn new(config: PluginManagerConfig) -> Self {
        Self { config }
    }

    #[must_use]
    pub fn bundled_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bundled")
    }

    #[must_use]
    pub fn install_root(&self) -> PathBuf {
        self.config
            .install_root
            .clone()
            .unwrap_or_else(|| self.config.config_home.join("plugins").join("installed"))
    }

    #[must_use]
    pub fn registry_path(&self) -> PathBuf {
        self.config.registry_path.clone().unwrap_or_else(|| {
            self.config
                .config_home
                .join("plugins")
                .join(REGISTRY_FILE_NAME)
        })
    }

    #[must_use]
    pub fn settings_path(&self) -> PathBuf {
        self.config.config_home.join(SETTINGS_FILE_NAME)
    }

    pub fn plugin_registry(&self) -> Result<PluginRegistry, PluginError> {
        self.plugin_registry_report()?.into_registry()
    }

    pub fn plugin_registry_report(&self) -> Result<PluginRegistryReport, PluginError> {
        self.sync_bundled_plugins()?;

        let mut discovery = PluginDiscovery::default();
        discovery.plugins.extend(builtin_plugins());

        let installed = self.discover_installed_plugins_with_failures()?;
        discovery.extend(installed);

        let external =
            self.discover_external_directory_plugins_with_failures(&discovery.plugins)?;
        discovery.extend(external);

        Ok(self.build_registry_report(discovery))
    }

    pub fn list_plugins(&self) -> Result<Vec<PluginSummary>, PluginError> {
        Ok(self.plugin_registry()?.summaries())
    }

    pub fn list_installed_plugins(&self) -> Result<Vec<PluginSummary>, PluginError> {
        Ok(self.installed_plugin_registry()?.summaries())
    }

    pub fn discover_plugins(&self) -> Result<Vec<PluginDefinition>, PluginError> {
        Ok(self
            .plugin_registry()?
            .plugins
            .into_iter()
            .map(|plugin| plugin.definition)
            .collect())
    }

    pub fn aggregated_hooks(&self) -> Result<PluginHooks, PluginError> {
        self.plugin_registry()?.aggregated_hooks()
    }

    pub fn aggregated_tools(&self) -> Result<Vec<PluginTool>, PluginError> {
        self.plugin_registry()?.aggregated_tools()
    }

    pub fn validate_plugin_source(&self, source: &str) -> Result<PluginManifest, PluginError> {
        let path = resolve_local_source(source)?;
        load_plugin_from_directory(&path)
    }

    pub fn install(&mut self, source: &str) -> Result<InstallOutcome, PluginError> {
        let install_source = parse_install_source(source)?;
        let temp_root = self.install_root().join(".tmp");
        let staged_source = materialize_source(&install_source, None, &temp_root)?;
        let cleanup_source = matches!(install_source, PluginInstallSource::GitUrl { .. });
        let manifest = load_plugin_from_directory(&staged_source)?;

        let plugin_id = plugin_id(&manifest.name, EXTERNAL_MARKETPLACE);
        let install_path = self.install_root().join(sanitize_plugin_id(&plugin_id));
        if install_path.exists() {
            fs::remove_dir_all(&install_path)?;
        }
        copy_dir_all(&staged_source, &install_path)?;
        if cleanup_source {
            let _ = fs::remove_dir_all(&staged_source);
        }

        let now = unix_time_ms();
        let record = InstalledPluginRecord {
            kind: PluginKind::External,
            id: plugin_id.clone(),
            name: manifest.name,
            version: manifest.version.clone(),
            description: manifest.description,
            install_path: install_path.clone(),
            source: install_source,
            installed_at_unix_ms: now,
            updated_at_unix_ms: now,
            checksum: None,
            git_ref: None,
            subdir: None,
        };

        let mut registry = self.load_registry()?;
        registry.plugins.insert(plugin_id.clone(), record);
        self.store_registry(&registry)?;
        self.write_enabled_state(&plugin_id, Some(true))?;
        self.config.enabled_plugins.insert(plugin_id.clone(), true);

        Ok(InstallOutcome {
            plugin_id,
            version: manifest.version,
            install_path,
        })
    }

    /// 市场安装：带完整性锚点、原子切换与失败回滚。
    ///
    /// 与 `install` 的区别：
    /// - 支持 `git-subdir` 与固定 git commit；
    /// - 必须提供 `expected_sha256` 或固定 commit，二者皆无则拒绝安装；
    /// - 新版本先落到同盘临时目录，校验通过后再原子替换旧目录，任何一步失败都恢复旧版本。
    pub fn install_request(
        &mut self,
        request: PluginInstallRequest,
    ) -> Result<InstallOutcome, PluginError> {
        self.reject_install_without_integrity_anchor(&request)?;

        let temp_root = self.install_root().join(".tmp");
        let staged_source = materialize_request(&request, &temp_root)?;
        let cleanup_source = matches!(request.source, PluginInstallSource::GitUrl { .. });
        let result = self.install_staged(&request, &staged_source);
        if cleanup_source {
            let _ = fs::remove_dir_all(&staged_source);
        }
        result
    }

    fn reject_install_without_integrity_anchor(
        &self,
        request: &PluginInstallRequest,
    ) -> Result<(), PluginError> {
        if request.expected_sha256.is_some() {
            return Ok(());
        }
        let pinned_commit = matches!(request.source, PluginInstallSource::GitUrl { .. })
            && request.git_ref.as_deref().is_some_and(is_pinned_git_commit);
        if pinned_commit {
            return Ok(());
        }
        Err(PluginError::CommandFailed(
            "拒绝安装：缺少完整性校验。请提供插件目录 sha256，或固定到具体 git commit（40 位 sha）。"
                .to_string(),
        ))
    }

    fn install_staged(
        &mut self,
        request: &PluginInstallRequest,
        staged_source: &Path,
    ) -> Result<InstallOutcome, PluginError> {
        let manifest = load_plugin_from_directory(staged_source)?;
        let checksum = compute_plugin_tree_sha256(staged_source)?;
        if let Some(expected) = &request.expected_sha256 {
            if !expected.eq_ignore_ascii_case(&checksum) {
                return Err(PluginError::CommandFailed(format!(
                    "插件校验失败：期望 sha256 `{expected}`，实际 `{checksum}`"
                )));
            }
        }

        let plugin_id = plugin_id(&manifest.name, EXTERNAL_MARKETPLACE);
        let install_path = self.install_root().join(sanitize_plugin_id(&plugin_id));
        atomic_replace_dir(staged_source, &install_path)?;

        let now = unix_time_ms();
        let record = InstalledPluginRecord {
            kind: PluginKind::External,
            id: plugin_id.clone(),
            name: manifest.name,
            version: manifest.version.clone(),
            description: manifest.description,
            install_path: install_path.clone(),
            source: request.source.clone(),
            installed_at_unix_ms: now,
            updated_at_unix_ms: now,
            checksum: Some(checksum),
            git_ref: request.git_ref.clone(),
            subdir: request.subdir.clone(),
        };

        let previous_registry = self.load_registry()?;
        let mut registry = previous_registry.clone();
        registry.plugins.insert(plugin_id.clone(), record);
        if let Err(error) = self.store_registry(&registry) {
            let _ = fs::remove_dir_all(&install_path);
            return Err(error);
        }
        if let Err(error) = self.write_enabled_state(&plugin_id, Some(true)) {
            let _ = self.store_registry(&previous_registry);
            let _ = fs::remove_dir_all(&install_path);
            return Err(error);
        }
        self.config.enabled_plugins.insert(plugin_id.clone(), true);

        Ok(InstallOutcome {
            plugin_id,
            version: manifest.version,
            install_path,
        })
    }

    pub fn enable(&mut self, plugin_id: &str) -> Result<(), PluginError> {
        self.ensure_known_plugin(plugin_id)?;
        self.write_enabled_state(plugin_id, Some(true))?;
        self.config
            .enabled_plugins
            .insert(plugin_id.to_string(), true);
        Ok(())
    }

    pub fn disable(&mut self, plugin_id: &str) -> Result<(), PluginError> {
        self.ensure_known_plugin(plugin_id)?;
        self.write_enabled_state(plugin_id, Some(false))?;
        self.config
            .enabled_plugins
            .insert(plugin_id.to_string(), false);
        Ok(())
    }

    pub fn uninstall(&mut self, plugin_id: &str) -> Result<(), PluginError> {
        let mut registry = self.load_registry()?;
        let record = registry.plugins.remove(plugin_id).ok_or_else(|| {
            PluginError::NotFound(format!("plugin `{plugin_id}` is not installed"))
        })?;
        if record.kind == PluginKind::Bundled {
            registry.plugins.insert(plugin_id.to_string(), record);
            return Err(PluginError::CommandFailed(format!(
                "plugin `{plugin_id}` is bundled and managed automatically; disable it instead"
            )));
        }
        if record.install_path.exists() {
            fs::remove_dir_all(&record.install_path)?;
        }
        self.store_registry(&registry)?;
        self.write_enabled_state(plugin_id, None)?;
        self.config.enabled_plugins.remove(plugin_id);
        Ok(())
    }

    pub fn update(&mut self, plugin_id: &str) -> Result<UpdateOutcome, PluginError> {
        let mut registry = self.load_registry()?;
        let record = registry.plugins.get(plugin_id).cloned().ok_or_else(|| {
            PluginError::NotFound(format!("plugin `{plugin_id}` is not installed"))
        })?;

        let temp_root = self.install_root().join(".tmp");
        let staged_source =
            materialize_source(&record.source, record.git_ref.as_deref(), &temp_root)?;
        let staged_source = resolve_staged_subdir(&staged_source, record.subdir.as_deref())?;
        let cleanup_source = matches!(record.source, PluginInstallSource::GitUrl { .. });
        let manifest = load_plugin_from_directory(&staged_source)?;
        let checksum = compute_plugin_tree_sha256(&staged_source)?;
        if let Some(expected) = &record.checksum {
            if !expected.eq_ignore_ascii_case(&checksum) {
                if cleanup_source {
                    let _ = fs::remove_dir_all(&staged_source);
                }
                return Err(PluginError::CommandFailed(format!(
                    "插件更新校验失败：期望 sha256 `{expected}`，实际 `{checksum}`"
                )));
            }
        }

        atomic_replace_dir(&staged_source, &record.install_path)?;
        if cleanup_source {
            let _ = fs::remove_dir_all(&staged_source);
        }

        let updated_record = InstalledPluginRecord {
            version: manifest.version.clone(),
            description: manifest.description,
            updated_at_unix_ms: unix_time_ms(),
            checksum: Some(checksum),
            ..record.clone()
        };
        registry
            .plugins
            .insert(plugin_id.to_string(), updated_record);
        self.store_registry(&registry)?;

        Ok(UpdateOutcome {
            plugin_id: plugin_id.to_string(),
            old_version: record.version,
            new_version: manifest.version,
            install_path: record.install_path,
        })
    }

    fn discover_installed_plugins_with_failures(&self) -> Result<PluginDiscovery, PluginError> {
        let mut registry = self.load_registry()?;
        let mut discovery = PluginDiscovery::default();
        let mut seen_ids = BTreeSet::<String>::new();
        let mut seen_paths = BTreeSet::<PathBuf>::new();
        let mut stale_registry_ids = Vec::new();

        for install_path in discover_plugin_dirs(&self.install_root())? {
            let matched_record = registry
                .plugins
                .values()
                .find(|record| record.install_path == install_path);
            let kind = matched_record.map_or(PluginKind::External, |record| record.kind);
            let source = matched_record.map_or_else(
                || install_path.display().to_string(),
                |record| describe_install_source(&record.source),
            );
            match load_plugin_definition(&install_path, kind, source.clone(), kind.marketplace()) {
                Ok(plugin) => {
                    if seen_ids.insert(plugin.metadata().id.clone()) {
                        seen_paths.insert(install_path);
                        discovery.push_plugin(plugin);
                    }
                }
                Err(error) => {
                    discovery.push_failure(PluginLoadFailure::new(
                        install_path,
                        kind,
                        source,
                        error,
                    ));
                }
            }
        }

        for record in registry.plugins.values() {
            if seen_paths.contains(&record.install_path) {
                continue;
            }
            if !record.install_path.exists() || plugin_manifest_path(&record.install_path).is_err()
            {
                stale_registry_ids.push(record.id.clone());
                continue;
            }
            let source = describe_install_source(&record.source);
            match load_plugin_definition(
                &record.install_path,
                record.kind,
                source.clone(),
                record.kind.marketplace(),
            ) {
                Ok(plugin) => {
                    if seen_ids.insert(plugin.metadata().id.clone()) {
                        seen_paths.insert(record.install_path.clone());
                        discovery.push_plugin(plugin);
                    }
                }
                Err(error) => {
                    discovery.push_failure(PluginLoadFailure::new(
                        record.install_path.clone(),
                        record.kind,
                        source,
                        error,
                    ));
                }
            }
        }

        if !stale_registry_ids.is_empty() {
            for plugin_id in stale_registry_ids {
                registry.plugins.remove(&plugin_id);
            }
            self.store_registry(&registry)?;
        }

        Ok(discovery)
    }

    fn discover_external_directory_plugins_with_failures(
        &self,
        existing_plugins: &[PluginDefinition],
    ) -> Result<PluginDiscovery, PluginError> {
        let mut discovery = PluginDiscovery::default();

        for directory in &self.config.external_dirs {
            for root in discover_plugin_dirs(directory)? {
                let source = root.display().to_string();
                match load_plugin_definition(
                    &root,
                    PluginKind::External,
                    source.clone(),
                    EXTERNAL_MARKETPLACE,
                ) {
                    Ok(plugin) => {
                        if existing_plugins
                            .iter()
                            .chain(discovery.plugins.iter())
                            .all(|existing| existing.metadata().id != plugin.metadata().id)
                        {
                            discovery.push_plugin(plugin);
                        }
                    }
                    Err(error) => {
                        discovery.push_failure(PluginLoadFailure::new(
                            root,
                            PluginKind::External,
                            source,
                            error,
                        ));
                    }
                }
            }
        }

        Ok(discovery)
    }

    pub fn installed_plugin_registry_report(&self) -> Result<PluginRegistryReport, PluginError> {
        self.sync_bundled_plugins()?;
        Ok(self.build_registry_report(self.discover_installed_plugins_with_failures()?))
    }

    fn sync_bundled_plugins(&self) -> Result<(), PluginError> {
        let bundled_root = self
            .config
            .bundled_root
            .clone()
            .unwrap_or_else(Self::bundled_root);
        let bundled_plugins = discover_plugin_dirs(&bundled_root)?;
        let mut registry = self.load_registry()?;
        let mut changed = false;
        let install_root = self.install_root();
        let mut active_bundled_ids = BTreeSet::new();

        for source_root in bundled_plugins {
            let manifest = load_plugin_from_directory(&source_root)?;
            let plugin_id = plugin_id(&manifest.name, BUNDLED_MARKETPLACE);
            active_bundled_ids.insert(plugin_id.clone());
            let install_path = install_root.join(sanitize_plugin_id(&plugin_id));
            let now = unix_time_ms();
            let existing_record = registry.plugins.get(&plugin_id);
            let installed_copy_is_valid =
                install_path.exists() && load_plugin_from_directory(&install_path).is_ok();
            let needs_sync = existing_record.is_none_or(|record| {
                record.kind != PluginKind::Bundled
                    || record.version != manifest.version
                    || record.name != manifest.name
                    || record.description != manifest.description
                    || record.install_path != install_path
                    || !record.install_path.exists()
                    || !installed_copy_is_valid
            });

            if !needs_sync {
                continue;
            }

            if install_path.exists() {
                fs::remove_dir_all(&install_path)?;
            }
            copy_dir_all(&source_root, &install_path)?;

            let installed_at_unix_ms =
                existing_record.map_or(now, |record| record.installed_at_unix_ms);
            registry.plugins.insert(
                plugin_id.clone(),
                InstalledPluginRecord {
                    kind: PluginKind::Bundled,
                    id: plugin_id,
                    name: manifest.name,
                    version: manifest.version,
                    description: manifest.description,
                    install_path,
                    source: PluginInstallSource::LocalPath { path: source_root },
                    installed_at_unix_ms,
                    updated_at_unix_ms: now,
                    checksum: None,
                    git_ref: None,
                    subdir: None,
                },
            );
            changed = true;
        }

        let stale_bundled_ids = registry
            .plugins
            .iter()
            .filter_map(|(plugin_id, record)| {
                (record.kind == PluginKind::Bundled && !active_bundled_ids.contains(plugin_id))
                    .then_some(plugin_id.clone())
            })
            .collect::<Vec<_>>();

        for plugin_id in stale_bundled_ids {
            if let Some(record) = registry.plugins.remove(&plugin_id) {
                if record.install_path.exists() {
                    fs::remove_dir_all(&record.install_path)?;
                }
                changed = true;
            }
        }

        if changed {
            self.store_registry(&registry)?;
        }

        Ok(())
    }

    fn is_enabled(&self, metadata: &PluginMetadata) -> bool {
        self.config
            .enabled_plugins
            .get(&metadata.id)
            .copied()
            .unwrap_or(match metadata.kind {
                PluginKind::External => false,
                PluginKind::Builtin | PluginKind::Bundled => metadata.default_enabled,
            })
    }

    fn ensure_known_plugin(&self, plugin_id: &str) -> Result<(), PluginError> {
        if self.plugin_registry()?.contains(plugin_id) {
            Ok(())
        } else {
            Err(PluginError::NotFound(format!(
                "plugin `{plugin_id}` is not installed or discoverable"
            )))
        }
    }

    fn load_registry(&self) -> Result<InstalledPluginRegistry, PluginError> {
        let path = self.registry_path();
        match fs::read_to_string(&path) {
            Ok(contents) if contents.trim().is_empty() => Ok(InstalledPluginRegistry::default()),
            Ok(contents) => Ok(serde_json::from_str(&contents)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(InstalledPluginRegistry::default())
            }
            Err(error) => Err(PluginError::Io(error)),
        }
    }

    fn store_registry(&self, registry: &InstalledPluginRegistry) -> Result<(), PluginError> {
        let path = self.registry_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string_pretty(registry)?)?;
        Ok(())
    }

    fn write_enabled_state(
        &self,
        plugin_id: &str,
        enabled: Option<bool>,
    ) -> Result<(), PluginError> {
        update_settings_json(&self.settings_path(), |root| {
            let enabled_plugins = ensure_object(root, "enabledPlugins");
            match enabled {
                Some(value) => {
                    enabled_plugins.insert(plugin_id.to_string(), Value::Bool(value));
                }
                None => {
                    enabled_plugins.remove(plugin_id);
                }
            }
        })
    }

    fn installed_plugin_registry(&self) -> Result<PluginRegistry, PluginError> {
        self.installed_plugin_registry_report()?.into_registry()
    }

    fn build_registry_report(&self, discovery: PluginDiscovery) -> PluginRegistryReport {
        PluginRegistryReport::new(
            PluginRegistry::new(
                discovery
                    .plugins
                    .into_iter()
                    .map(|plugin| {
                        let enabled = self.is_enabled(plugin.metadata());
                        RegisteredPlugin::new(plugin, enabled)
                    })
                    .collect(),
            ),
            discovery.failures,
        )
    }
}

#[must_use]
pub fn builtin_plugins() -> Vec<PluginDefinition> {
    vec![PluginDefinition::Builtin(BuiltinPlugin {
        metadata: PluginMetadata {
            id: plugin_id("example-builtin", BUILTIN_MARKETPLACE),
            name: "example-builtin".to_string(),
            version: "0.1.0".to_string(),
            description: "Example built-in plugin scaffold for the Rust plugin system".to_string(),
            kind: PluginKind::Builtin,
            source: BUILTIN_MARKETPLACE.to_string(),
            default_enabled: false,
            root: None,
        },
        hooks: PluginHooks::default(),
        lifecycle: PluginLifecycle::default(),
        tools: Vec::new(),
        components: ClaudeComponents::default(),
    })]
}

fn load_plugin_definition(
    root: &Path,
    kind: PluginKind,
    source: String,
    marketplace: &str,
) -> Result<PluginDefinition, PluginError> {
    let manifest = load_plugin_from_directory(root)?;
    let metadata = PluginMetadata {
        id: plugin_id(&manifest.name, marketplace),
        name: manifest.name,
        version: manifest.version,
        description: manifest.description,
        kind,
        source,
        default_enabled: manifest.default_enabled,
        root: Some(root.to_path_buf()),
    };
    let components = manifest.components.clone();
    let hooks = resolve_hooks(root, &manifest.hooks);
    let lifecycle = resolve_lifecycle(root, &manifest.lifecycle);
    let tools = resolve_tools(root, &metadata.id, &metadata.name, &manifest.tools);
    Ok(match kind {
        PluginKind::Builtin => PluginDefinition::Builtin(BuiltinPlugin {
            metadata,
            hooks,
            lifecycle,
            tools,
            components,
        }),
        PluginKind::Bundled => PluginDefinition::Bundled(BundledPlugin {
            metadata,
            hooks,
            lifecycle,
            tools,
            components,
        }),
        PluginKind::External => PluginDefinition::External(ExternalPlugin {
            metadata,
            hooks,
            lifecycle,
            tools,
            components,
        }),
    })
}

pub fn load_plugin_from_directory(root: &Path) -> Result<PluginManifest, PluginError> {
    load_manifest_from_directory(root)
}

fn load_manifest_from_directory(root: &Path) -> Result<PluginManifest, PluginError> {
    let manifest_path = plugin_manifest_path(root)?;
    load_manifest_from_path(root, &manifest_path)
}

fn load_manifest_from_path(
    root: &Path,
    manifest_path: &Path,
) -> Result<PluginManifest, PluginError> {
    let contents = fs::read_to_string(manifest_path).map_err(|error| {
        PluginError::NotFound(format!(
            "plugin manifest not found at {}: {error}",
            manifest_path.display()
        ))
    })?;
    let raw_json: Value = serde_json::from_str(&contents)?;
    let raw_manifest: RawPluginManifest = serde_json::from_value(raw_json)?;
    build_plugin_manifest(root, raw_manifest)
}

fn plugin_manifest_path(root: &Path) -> Result<PathBuf, PluginError> {
    let direct_path = root.join(MANIFEST_FILE_NAME);
    if direct_path.exists() {
        return Ok(direct_path);
    }

    let packaged_path = root.join(MANIFEST_RELATIVE_PATH);
    if packaged_path.exists() {
        return Ok(packaged_path);
    }

    Err(PluginError::NotFound(format!(
        "plugin manifest not found at {} or {}",
        direct_path.display(),
        packaged_path.display()
    )))
}

fn build_plugin_manifest(
    root: &Path,
    raw: RawPluginManifest,
) -> Result<PluginManifest, PluginError> {
    let mut errors = Vec::new();

    validate_required_manifest_field("name", &raw.name, &mut errors);
    let version = if raw.version.trim().is_empty() {
        "0.0.0".to_string()
    } else {
        raw.version
    };
    let description = raw.description;

    let permissions = build_manifest_permissions(&raw.permissions, &mut errors);
    let (hooks, mut claude_hooks, mut lifecycle, hook_unsupported) =
        parse_hooks_value(root, &raw.hooks);
    if !raw.lifecycle.init.is_empty() {
        lifecycle.init.extend(raw.lifecycle.init.iter().cloned());
    }
    if !raw.lifecycle.shutdown.is_empty() {
        lifecycle
            .shutdown
            .extend(raw.lifecycle.shutdown.iter().cloned());
    }
    validate_command_entries(root, hooks.pre_tool_use.iter(), "hook", &mut errors);
    validate_command_entries(root, hooks.post_tool_use.iter(), "hook", &mut errors);
    validate_command_entries(
        root,
        hooks.post_tool_use_failure.iter(),
        "hook",
        &mut errors,
    );
    validate_command_entries(
        root,
        lifecycle.init.iter(),
        "lifecycle command",
        &mut errors,
    );
    validate_command_entries(
        root,
        lifecycle.shutdown.iter(),
        "lifecycle command",
        &mut errors,
    );

    let tools = build_manifest_tools(root, raw.tools, &mut errors);
    let has_raw_commands = raw.commands.is_some();
    let (commands, mut command_paths) = parse_commands_value(root, raw.commands, &mut errors);
    let mut skill_paths = parse_component_roots(root, raw.skills.as_ref(), true);
    let mut agent_paths = parse_component_roots(root, raw.agents.as_ref(), false);
    if raw.skills.is_none() && root.join("skills").is_dir() {
        skill_paths.push(root.join("skills"));
    }
    if raw.agents.is_none() && root.join("agents").is_dir() {
        agent_paths.push(root.join("agents"));
    }
    if !has_raw_commands && root.join("commands").is_dir() {
        command_paths.push(root.join("commands"));
    }
    if raw.hooks.is_null() {
        let file = root.join("hooks").join("hooks.json");
        if file.is_file() {
            let parsed = parse_hooks_file(root, &file);
            claude_hooks.extend(parsed.claude);
            lifecycle.init.extend(parsed.lifecycle.init);
            lifecycle.shutdown.extend(parsed.lifecycle.shutdown);
        }
    }
    let mcp_servers = parse_mcp_servers(root, raw.mcp_servers.as_ref());
    let user_config = parse_user_config(raw.user_config.as_ref());
    let mut unsupported: Vec<String> = hook_unsupported
        .into_iter()
        .map(|event| format!("hook 事件 `{event}` 在 claw 中没有对应机制，已跳过"))
        .collect();
    if raw.mcp_servers.is_some()
        && mcp_servers
            .iter()
            .any(|server| !matches!(server.transport.as_str(), "stdio" | "sse" | "http" | "ws"))
    {
        unsupported.push("mcpServers 里存在未识别的传输类型".to_string());
    }

    if !errors.is_empty() {
        return Err(PluginError::ManifestValidation(errors));
    }

    Ok(PluginManifest {
        name: raw.name,
        version,
        description,
        permissions,
        default_enabled: raw.default_enabled,
        hooks,
        lifecycle,
        tools,
        commands,
        components: ClaudeComponents {
            skill_paths,
            agent_paths,
            command_paths,
            hooks: claude_hooks,
            mcp_servers,
            user_config,
            unsupported,
        },
    })
}

#[derive(Debug, Default)]
struct ParsedHooks {
    native: PluginHooks,
    claude: Vec<ClaudeHookSpec>,
    lifecycle: PluginLifecycle,
    unsupported: Vec<String>,
}

fn parse_hooks_file(root: &Path, path: &Path) -> ParsedHooks {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut parsed = ParsedHooks::default();
    if let Ok(json) = serde_json::from_str::<Value>(&text) {
        // 兼容 `{ "hooks": { ... } }` 包装和直接事件对象两种写法。
        let object = json.get("hooks").cloned().unwrap_or(json);
        collect_hook_object(root, &object, &mut parsed);
    }
    parsed
}

/// 同时接受 claw 原生字符串数组和 Claude hooks（文件路径 / 内联对象）。
fn parse_hooks_value(
    root: &Path,
    value: &Value,
) -> (
    PluginHooks,
    Vec<ClaudeHookSpec>,
    PluginLifecycle,
    Vec<String>,
) {
    let parsed = parse_hooks_value_inner(root, value);
    (
        parsed.native,
        parsed.claude,
        parsed.lifecycle,
        parsed.unsupported,
    )
}

fn parse_hooks_value_inner(root: &Path, value: &Value) -> ParsedHooks {
    let mut parsed = ParsedHooks::default();
    match value {
        Value::Null => {}
        Value::String(path) => {
            let file = resolve_path(root, path);
            if file.is_file() {
                let content = fs::read_to_string(&file).unwrap_or_default();
                if let Ok(json) = serde_json::from_str::<Value>(&content) {
                    let object = json.get("hooks").cloned().unwrap_or(json);
                    collect_hook_object(root, &object, &mut parsed);
                }
            }
        }
        Value::Object(_) => {
            let object = value.get("hooks").cloned().unwrap_or_else(|| value.clone());
            collect_hook_object(root, &object, &mut parsed);
        }
        _ => {}
    }
    parsed
}

fn collect_hook_object(root: &Path, value: &Value, parsed: &mut ParsedHooks) {
    let Some(object) = value.as_object() else {
        return;
    };
    for (event, entries) in object {
        match event.as_str() {
            "PreToolUse" => collect_native_or_claude_hooks(
                root,
                event,
                entries,
                &mut parsed.native.pre_tool_use,
                &mut parsed.claude,
            ),
            "PostToolUse" => collect_native_or_claude_hooks(
                root,
                event,
                entries,
                &mut parsed.native.post_tool_use,
                &mut parsed.claude,
            ),
            "PostToolUseFailure" => collect_native_or_claude_hooks(
                root,
                event,
                entries,
                &mut parsed.native.post_tool_use_failure,
                &mut parsed.claude,
            ),
            "SessionStart" => collect_lifecycle_commands(root, entries, &mut parsed.lifecycle.init),
            "Stop" | "SessionEnd" => {
                collect_lifecycle_commands(root, entries, &mut parsed.lifecycle.shutdown)
            }
            other => collect_claude_hooks(root, other, entries, &mut parsed.claude),
        }
    }
}

/// Claude 事件对象 → 归一化 hook 规格；字符串形式只对 claw 原生事件有意义。
fn collect_claude_hooks(
    root: &Path,
    event: &str,
    entries: &Value,
    claude: &mut Vec<ClaudeHookSpec>,
) {
    let Some(items) = entries.as_array() else {
        return;
    };
    for item in items {
        let matcher = item
            .get("matcher")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(commands) = item.get("hooks").and_then(Value::as_array) {
            for hook in commands {
                if let Some(command) = hook.get("command").and_then(Value::as_str) {
                    let type_name = hook
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("command");
                    if type_name != "command" {
                        continue;
                    }
                    claude.push(ClaudeHookSpec {
                        event: event.to_string(),
                        matcher: matcher.clone(),
                        command: expand_placeholders(root, command),
                        timeout_ms: hook
                            .get("timeout")
                            .and_then(Value::as_u64)
                            .map(|seconds| seconds.saturating_mul(1000)),
                    });
                }
            }
        } else if let Some(command) = item.get("command").and_then(Value::as_str) {
            claude.push(ClaudeHookSpec {
                event: event.to_string(),
                matcher,
                command: expand_placeholders(root, command),
                timeout_ms: item
                    .get("timeout")
                    .and_then(Value::as_u64)
                    .map(|seconds| seconds.saturating_mul(1000)),
            });
        }
    }
}

fn collect_native_or_claude_hooks(
    root: &Path,
    event: &str,
    entries: &Value,
    native: &mut Vec<String>,
    claude: &mut Vec<ClaudeHookSpec>,
) {
    if let Some(items) = entries.as_array() {
        for item in items {
            if let Some(command) = item.as_str() {
                native.push(expand_placeholders(root, command));
            }
        }
    }
    collect_claude_hooks(root, event, entries, claude);
}

fn collect_lifecycle_commands(root: &Path, entries: &Value, target: &mut Vec<String>) {
    let Some(items) = entries.as_array() else {
        return;
    };
    for item in items {
        if let Some(command) = item.as_str() {
            target.push(expand_placeholders(root, command));
        } else if let Some(commands) = item.get("hooks").and_then(Value::as_array) {
            for hook in commands {
                if let Some(command) = hook.get("command").and_then(Value::as_str) {
                    target.push(expand_placeholders(root, command));
                }
            }
        } else if let Some(command) = item.get("command").and_then(Value::as_str) {
            target.push(expand_placeholders(root, command));
        }
    }
}

/// 命令字段既可能是 claw 原生对象数组，也可能是 Claude 的目录/文件列表。
fn parse_commands_value(
    root: &Path,
    value: Option<Value>,
    errors: &mut Vec<PluginManifestValidationError>,
) -> (Vec<PluginCommandManifest>, Vec<PathBuf>) {
    let Some(value) = value else {
        return (Vec::new(), Vec::new());
    };
    let mut native_values = Vec::new();
    let mut path_values = Vec::new();
    match value {
        Value::String(path) => path_values.push(path),
        Value::Array(items) => {
            for item in items {
                if item.is_string() {
                    path_values.push(item.as_str().unwrap_or_default().to_string());
                } else if item.get("command").is_some() {
                    native_values.push(item);
                } else if let Some(path) = item.get("path").and_then(Value::as_str) {
                    path_values.push(path.to_string());
                }
            }
        }
        Value::Object(map) if map.contains_key("command") => native_values.push(Value::Object(map)),
        _ => {}
    }
    let native = if native_values.is_empty() {
        Vec::new()
    } else {
        match serde_json::from_value::<Vec<PluginCommandManifest>>(Value::Array(native_values)) {
            Ok(commands) => build_manifest_commands(root, commands, errors),
            Err(_) => Vec::new(),
        }
    };
    let mut paths = Vec::new();
    for path in path_values {
        paths.extend(expand_component_path(&resolve_path(root, &path)));
    }
    (native, paths)
}

/// 把 skills/agents 的声明归一化成“发现根”。技能根约定为包含 `<skill>/SKILL.md` 的目录。
fn parse_component_roots(root: &Path, value: Option<&Value>, skill: bool) -> Vec<PathBuf> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut raw_paths = Vec::new();
    match value {
        Value::String(path) => raw_paths.push(path.clone()),
        Value::Array(items) => {
            for item in items {
                if let Some(path) = item.as_str() {
                    raw_paths.push(path.to_string());
                } else if let Some(path) = item.get("path").and_then(Value::as_str) {
                    raw_paths.push(path.to_string());
                }
            }
        }
        _ => {}
    }
    let mut roots = Vec::new();
    for raw in raw_paths {
        for path in expand_component_path(&resolve_path(root, &raw)) {
            if skill {
                let is_skill_md =
                    path.is_file() && path.file_name().is_some_and(|name| name == "SKILL.md");
                let is_skill_dir = path.is_dir() && path.join("SKILL.md").is_file();
                if is_skill_md || is_skill_dir {
                    if let Some(parent) = path.parent() {
                        roots.push(parent.to_path_buf());
                    }
                } else {
                    roots.push(path);
                }
            } else if path.is_file() {
                if let Some(parent) = path.parent() {
                    roots.push(parent.to_path_buf());
                }
            } else {
                roots.push(path);
            }
        }
    }
    dedupe_paths(roots)
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn resolve_path(root: &Path, raw: &str) -> PathBuf {
    let root_text = root.to_string_lossy();
    let expanded = raw
        .replace("${CLAUDE_PLUGIN_ROOT}", &root_text)
        .replace("$CLAUDE_PLUGIN_ROOT", &root_text)
        .replace("${CLAUDE_PROJECT_DIR}", ".")
        .replace("$CLAUDE_PROJECT_DIR", ".");
    let path = Path::new(expanded.trim());
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn expand_component_path(path: &Path) -> Vec<PathBuf> {
    let text = path.to_string_lossy();
    if !text.contains('*') {
        return vec![path.to_path_buf()];
    }
    let components: Vec<_> = path.components().collect();
    let first_wildcard = components
        .iter()
        .position(|component| component.as_os_str().to_string_lossy().contains('*'));
    let Some(index) = first_wildcard else {
        return vec![path.to_path_buf()];
    };
    let mut base = PathBuf::new();
    for component in &components[..index] {
        base.push(component.as_os_str());
    }
    let parts: Vec<String> = components[index..]
        .iter()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();
    expand_glob_parts(&base, &parts)
}

fn expand_glob_parts(base: &Path, parts: &[String]) -> Vec<PathBuf> {
    let Some(part) = parts.first() else {
        return vec![base.to_path_buf()];
    };
    if parts.len() == 1 && !part.contains('*') {
        return vec![base.join(part)];
    }
    if part == "**" {
        let mut out = expand_glob_parts(base, &parts[1..]);
        if let Ok(entries) = fs::read_dir(base) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    out.extend(expand_glob_parts(&entry.path(), parts));
                }
            }
        }
        return out;
    }
    if part.contains('*') {
        let mut out = Vec::new();
        if let Ok(entries) = fs::read_dir(base) {
            for entry in entries.flatten() {
                if wildcard_match(&entry.file_name().to_string_lossy(), part) {
                    out.extend(expand_glob_parts(&entry.path(), &parts[1..]));
                }
            }
        }
        out
    } else {
        expand_glob_parts(&base.join(part), &parts[1..])
    }
}

fn wildcard_match(text: &str, pattern: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let mut memo = BTreeMap::new();
    fn go(
        text: &[char],
        pattern: &[char],
        ti: usize,
        pi: usize,
        memo: &mut BTreeMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(value) = memo.get(&(ti, pi)) {
            return *value;
        }
        let result = if pi == pattern.len() {
            ti == text.len()
        } else if pattern[pi] == '*' {
            go(text, pattern, ti, pi + 1, memo)
                || (ti < text.len() && go(text, pattern, ti + 1, pi, memo))
        } else {
            ti < text.len()
                && (pattern[pi] == '?' || pattern[pi] == text[ti])
                && go(text, pattern, ti + 1, pi + 1, memo)
        };
        memo.insert((ti, pi), result);
        result
    }
    go(&text, &pattern, 0, 0, &mut memo)
}

fn parse_mcp_servers(root: &Path, value: Option<&Value>) -> Vec<PluginMcpServerSpec> {
    let Some(value) = value else {
        return Vec::new();
    };
    let source = match value {
        Value::String(path) => {
            let file = resolve_path(root, path);
            fs::read_to_string(file)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .unwrap_or(Value::Null)
        }
        other => other.clone(),
    };
    let object = source.get("mcpServers").cloned().unwrap_or(source);
    let Some(map) = object.as_object() else {
        return Vec::new();
    };
    let mut servers = Vec::new();
    for (name, config) in map {
        let Some(config) = config.as_object() else {
            continue;
        };
        let transport = config
            .get("type")
            .or_else(|| config.get("transport"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                if config.get("url").is_some() {
                    // 未声明 type 的远程服务器按当前标准走 streamable HTTP；
                    // 仅支持旧版 SSE 的服务需要显式写 `"type": "sse"`。
                    "http".to_string()
                } else {
                    "stdio".to_string()
                }
            });
        let command = config
            .get("command")
            .and_then(Value::as_str)
            .map(|command| expand_placeholders(root, command));
        let args = config
            .get("args")
            .and_then(Value::as_array)
            .map(|args| {
                args.iter()
                    .filter_map(Value::as_str)
                    .map(|arg| expand_placeholders(root, arg))
                    .collect()
            })
            .unwrap_or_default();
        let env = string_map(config.get("env"), root);
        let headers = string_map(config.get("headers"), root);
        let url = config
            .get("url")
            .and_then(Value::as_str)
            .map(|url| expand_placeholders(root, url));
        let timeout_ms = config
            .get("timeout")
            .and_then(Value::as_u64)
            .map(|seconds| seconds.saturating_mul(1000));
        let proxy = config
            .get("proxy")
            .and_then(Value::as_str)
            .map(|proxy| expand_placeholders(root, proxy));
        servers.push(PluginMcpServerSpec {
            name: name.clone(),
            transport,
            command,
            args,
            env,
            url,
            headers,
            timeout_ms,
            proxy,
        });
    }
    servers
}

fn string_map(value: Option<&Value>, root: &Path) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Some(object) = value.and_then(Value::as_object) {
        for (key, value) in object {
            let text = match value {
                Value::String(text) => text.clone(),
                Value::Number(number) => number.to_string(),
                Value::Bool(boolean) => boolean.to_string(),
                _ => continue,
            };
            map.insert(key.clone(), expand_placeholders(root, &text));
        }
    }
    map
}

fn expand_placeholders(root: &Path, text: &str) -> String {
    let mut result = text
        .replace("${CLAUDE_PLUGIN_ROOT}", &root.to_string_lossy())
        .replace("$CLAUDE_PLUGIN_ROOT", &root.to_string_lossy())
        .replace("${CLAUDE_PROJECT_DIR}", ".")
        .replace("$CLAUDE_PROJECT_DIR", ".");
    // 剩余 `${VAR}` 从环境变量展开；缺失时原样保留，便于安装确认页提示。
    for (key, value) in std::env::vars() {
        result = result.replace(&format!("${{{key}}}"), &value);
    }
    result
}

fn parse_user_config(value: Option<&Value>) -> Vec<PluginUserConfigField> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    match value {
        Value::Object(object) => {
            // Claude 的 userConfig 是 name -> schema 的对象。
            for (name, schema) in object {
                if let Some(field) = parse_user_config_field(name, schema) {
                    fields.push(field);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                let Some(name) = item.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(field) = parse_user_config_field(name, item) {
                    fields.push(field);
                }
            }
        }
        _ => {}
    }
    fields
}

fn parse_user_config_field(name: &str, schema: &Value) -> Option<PluginUserConfigField> {
    let object = schema.as_object()?;
    Some(PluginUserConfigField {
        name: name.to_string(),
        title: object
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(name)
            .to_string(),
        description: object
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        kind: object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("string")
            .to_string(),
        required: object
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        sensitive: object
            .get("sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        default: object
            .get("default")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn validate_required_manifest_field(
    field: &'static str,
    value: &str,
    errors: &mut Vec<PluginManifestValidationError>,
) {
    if value.trim().is_empty() {
        errors.push(PluginManifestValidationError::EmptyField { field });
    }
}

fn build_manifest_permissions(
    permissions: &[String],
    errors: &mut Vec<PluginManifestValidationError>,
) -> Vec<PluginPermission> {
    let mut seen = BTreeSet::new();
    let mut validated = Vec::new();

    for permission in permissions {
        let permission = permission.trim();
        if permission.is_empty() {
            errors.push(PluginManifestValidationError::EmptyEntryField {
                kind: "permission",
                field: "value",
                name: None,
            });
            continue;
        }
        if !seen.insert(permission.to_string()) {
            errors.push(PluginManifestValidationError::DuplicatePermission {
                permission: permission.to_string(),
            });
            continue;
        }
        match PluginPermission::parse(permission) {
            Some(permission) => validated.push(permission),
            None => errors.push(PluginManifestValidationError::InvalidPermission {
                permission: permission.to_string(),
            }),
        }
    }

    validated
}

fn build_manifest_tools(
    root: &Path,
    tools: Vec<RawPluginToolManifest>,
    errors: &mut Vec<PluginManifestValidationError>,
) -> Vec<PluginToolManifest> {
    let mut seen = BTreeSet::new();
    let mut validated = Vec::new();

    for tool in tools {
        let name = tool.name.trim().to_string();
        if name.is_empty() {
            errors.push(PluginManifestValidationError::EmptyEntryField {
                kind: "tool",
                field: "name",
                name: None,
            });
            continue;
        }
        if !seen.insert(name.clone()) {
            errors.push(PluginManifestValidationError::DuplicateEntry { kind: "tool", name });
            continue;
        }
        if tool.description.trim().is_empty() {
            errors.push(PluginManifestValidationError::EmptyEntryField {
                kind: "tool",
                field: "description",
                name: Some(name.clone()),
            });
        }
        if tool.command.trim().is_empty() {
            errors.push(PluginManifestValidationError::EmptyEntryField {
                kind: "tool",
                field: "command",
                name: Some(name.clone()),
            });
        } else {
            validate_command_entry(root, &tool.command, "tool", errors);
        }
        if !tool.input_schema.is_object() {
            errors.push(PluginManifestValidationError::InvalidToolInputSchema {
                tool_name: name.clone(),
            });
        }
        let Some(required_permission) =
            PluginToolPermission::parse(tool.required_permission.trim())
        else {
            errors.push(
                PluginManifestValidationError::InvalidToolRequiredPermission {
                    tool_name: name.clone(),
                    permission: tool.required_permission.trim().to_string(),
                },
            );
            continue;
        };

        validated.push(PluginToolManifest {
            name,
            description: tool.description,
            input_schema: tool.input_schema,
            command: tool.command,
            args: tool.args,
            required_permission,
        });
    }

    validated
}

fn build_manifest_commands(
    root: &Path,
    commands: Vec<PluginCommandManifest>,
    errors: &mut Vec<PluginManifestValidationError>,
) -> Vec<PluginCommandManifest> {
    let mut seen = BTreeSet::new();
    let mut validated = Vec::new();

    for command in commands {
        let name = command.name.trim().to_string();
        if name.is_empty() {
            errors.push(PluginManifestValidationError::EmptyEntryField {
                kind: "command",
                field: "name",
                name: None,
            });
            continue;
        }
        if !seen.insert(name.clone()) {
            errors.push(PluginManifestValidationError::DuplicateEntry {
                kind: "command",
                name,
            });
            continue;
        }
        if command.description.trim().is_empty() {
            errors.push(PluginManifestValidationError::EmptyEntryField {
                kind: "command",
                field: "description",
                name: Some(name.clone()),
            });
        }
        if command.command.trim().is_empty() {
            errors.push(PluginManifestValidationError::EmptyEntryField {
                kind: "command",
                field: "command",
                name: Some(name.clone()),
            });
        } else {
            validate_command_entry(root, &command.command, "command", errors);
        }
        validated.push(command);
    }

    validated
}

fn validate_command_entries<'a>(
    root: &Path,
    entries: impl Iterator<Item = &'a String>,
    kind: &'static str,
    errors: &mut Vec<PluginManifestValidationError>,
) {
    for entry in entries {
        validate_command_entry(root, entry, kind, errors);
    }
}

fn validate_command_entry(
    root: &Path,
    entry: &str,
    kind: &'static str,
    errors: &mut Vec<PluginManifestValidationError>,
) {
    if entry.trim().is_empty() {
        errors.push(PluginManifestValidationError::EmptyEntryField {
            kind,
            field: "command",
            name: None,
        });
        return;
    }
    if is_literal_command(entry) {
        return;
    }

    let path = if Path::new(entry).is_absolute() {
        PathBuf::from(entry)
    } else {
        root.join(entry)
    };
    if !path.exists() {
        errors.push(PluginManifestValidationError::MissingPath { kind, path });
    } else if !path.is_file() {
        errors.push(PluginManifestValidationError::PathIsDirectory { kind, path });
    }
}

fn resolve_hooks(root: &Path, hooks: &PluginHooks) -> PluginHooks {
    PluginHooks {
        pre_tool_use: hooks
            .pre_tool_use
            .iter()
            .map(|entry| resolve_hook_entry(root, entry))
            .collect(),
        post_tool_use: hooks
            .post_tool_use
            .iter()
            .map(|entry| resolve_hook_entry(root, entry))
            .collect(),
        post_tool_use_failure: hooks
            .post_tool_use_failure
            .iter()
            .map(|entry| resolve_hook_entry(root, entry))
            .collect(),
    }
}

fn resolve_lifecycle(root: &Path, lifecycle: &PluginLifecycle) -> PluginLifecycle {
    PluginLifecycle {
        init: lifecycle
            .init
            .iter()
            .map(|entry| resolve_hook_entry(root, entry))
            .collect(),
        shutdown: lifecycle
            .shutdown
            .iter()
            .map(|entry| resolve_hook_entry(root, entry))
            .collect(),
    }
}

fn resolve_tools(
    root: &Path,
    plugin_id: &str,
    plugin_name: &str,
    tools: &[PluginToolManifest],
) -> Vec<PluginTool> {
    tools
        .iter()
        .map(|tool| {
            PluginTool::new(
                plugin_id,
                plugin_name,
                PluginToolDefinition {
                    name: tool.name.clone(),
                    description: Some(tool.description.clone()),
                    input_schema: tool.input_schema.clone(),
                },
                resolve_hook_entry(root, &tool.command),
                tool.args.clone(),
                tool.required_permission,
                Some(root.to_path_buf()),
            )
        })
        .collect()
}

fn validate_hook_paths(root: Option<&Path>, hooks: &PluginHooks) -> Result<(), PluginError> {
    let Some(root) = root else {
        return Ok(());
    };
    for entry in hooks
        .pre_tool_use
        .iter()
        .chain(hooks.post_tool_use.iter())
        .chain(hooks.post_tool_use_failure.iter())
    {
        validate_command_path(root, entry, "hook")?;
    }
    Ok(())
}

fn validate_lifecycle_paths(
    root: Option<&Path>,
    lifecycle: &PluginLifecycle,
) -> Result<(), PluginError> {
    let Some(root) = root else {
        return Ok(());
    };
    for entry in lifecycle.init.iter().chain(lifecycle.shutdown.iter()) {
        validate_command_path(root, entry, "lifecycle command")?;
    }
    Ok(())
}

fn validate_tool_paths(root: Option<&Path>, tools: &[PluginTool]) -> Result<(), PluginError> {
    let Some(root) = root else {
        return Ok(());
    };
    for tool in tools {
        validate_command_path(root, &tool.command, "tool")?;
    }
    Ok(())
}

fn validate_command_path(root: &Path, entry: &str, kind: &str) -> Result<(), PluginError> {
    if is_literal_command(entry) {
        return Ok(());
    }
    let path = if Path::new(entry).is_absolute() {
        PathBuf::from(entry)
    } else {
        root.join(entry)
    };
    if !path.exists() {
        return Err(PluginError::InvalidManifest(format!(
            "{kind} path `{}` does not exist",
            path.display()
        )));
    }
    if !path.is_file() {
        return Err(PluginError::InvalidManifest(format!(
            "{kind} path `{}` must point to a file",
            path.display()
        )));
    }
    Ok(())
}

fn resolve_hook_entry(root: &Path, entry: &str) -> String {
    if is_literal_command(entry) {
        entry.to_string()
    } else {
        root.join(entry).display().to_string()
    }
}

fn is_literal_command(entry: &str) -> bool {
    !entry.starts_with("./") && !entry.starts_with("../") && !Path::new(entry).is_absolute()
}

fn run_lifecycle_commands(
    metadata: &PluginMetadata,
    lifecycle: &PluginLifecycle,
    phase: &str,
    commands: &[String],
) -> Result<(), PluginError> {
    if lifecycle.is_empty() || commands.is_empty() {
        return Ok(());
    }

    for command in commands {
        let mut process = if Path::new(command).exists() {
            if cfg!(windows) {
                let mut process = Command::new("cmd");
                process.arg("/C").arg(command);
                process
            } else {
                let mut process = Command::new("sh");
                process.arg(command);
                process
            }
        } else if cfg!(windows) {
            let mut process = Command::new("cmd");
            process.arg("/C").arg(command);
            process
        } else {
            let mut process = Command::new("sh");
            process.arg("-lc").arg(command);
            process
        };
        if let Some(root) = &metadata.root {
            process.current_dir(root);
        }
        let output = process.output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(PluginError::CommandFailed(format!(
                "plugin `{}` {} failed for `{}`: {}",
                metadata.id,
                phase,
                command,
                if stderr.is_empty() {
                    format!("exit status {}", output.status)
                } else {
                    stderr
                }
            )));
        }
    }

    Ok(())
}

fn resolve_local_source(source: &str) -> Result<PathBuf, PluginError> {
    let path = PathBuf::from(source);
    if path.exists() {
        Ok(path)
    } else {
        Err(PluginError::NotFound(format!(
            "plugin source `{source}` was not found"
        )))
    }
}

fn parse_install_source(source: &str) -> Result<PluginInstallSource, PluginError> {
    if source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("git@")
        || Path::new(source)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("git"))
    {
        Ok(PluginInstallSource::GitUrl {
            url: source.to_string(),
        })
    } else {
        Ok(PluginInstallSource::LocalPath {
            path: resolve_local_source(source)?,
        })
    }
}

fn materialize_source(
    source: &PluginInstallSource,
    git_ref: Option<&str>,
    temp_root: &Path,
) -> Result<PathBuf, PluginError> {
    fs::create_dir_all(temp_root)?;
    match source {
        PluginInstallSource::LocalPath { path } => Ok(path.clone()),
        PluginInstallSource::GitUrl { url } => clone_git_repo(url, git_ref, temp_root),
    }
}

fn clone_git_repo(
    url: &str,
    git_ref: Option<&str>,
    temp_root: &Path,
) -> Result<PathBuf, PluginError> {
    static MATERIALIZE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = MATERIALIZE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let destination = temp_root.join(format!("plugin-{nanos}-{unique}"));
    // 固定 commit 时必须拿到完整历史才能 checkout 任意 sha；未固定时用浅克隆。
    let mut clone = Command::new("git");
    clone.arg("clone");
    if git_ref.is_none() {
        clone.arg("--depth").arg("1");
    }
    let output = clone.arg(url).arg(&destination).output()?;
    if !output.status.success() {
        return Err(PluginError::CommandFailed(format!(
            "git clone failed for `{url}`: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    if let Some(reference) = git_ref {
        let output = Command::new("git")
            .arg("-C")
            .arg(&destination)
            .arg("checkout")
            .arg("--detach")
            .arg(reference)
            .output()?;
        if !output.status.success() {
            let _ = fs::remove_dir_all(&destination);
            return Err(PluginError::CommandFailed(format!(
                "git checkout `{reference}` failed for `{url}`: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }
    Ok(destination)
}

fn materialize_request(
    request: &PluginInstallRequest,
    temp_root: &Path,
) -> Result<PathBuf, PluginError> {
    let root = materialize_source(&request.source, request.git_ref.as_deref(), temp_root)?;
    resolve_staged_subdir(&root, request.subdir.as_deref())
}

fn resolve_staged_subdir(root: &Path, subdir: Option<&Path>) -> Result<PathBuf, PluginError> {
    let Some(subdir) = subdir.filter(|subdir| !subdir.as_os_str().is_empty()) else {
        return Ok(root.to_path_buf());
    };
    let candidate = root.join(subdir);
    if candidate.is_dir() {
        Ok(candidate)
    } else {
        Err(PluginError::NotFound(format!(
            "插件子目录不存在：{}",
            candidate.display()
        )))
    }
}

/// 只要出现任意一步失败，就恢复旧目录；成功时清理备份。
fn atomic_replace_dir(source: &Path, destination: &Path) -> Result<(), PluginError> {
    static REPLACE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = destination.parent().ok_or_else(|| {
        PluginError::CommandFailed(format!("插件安装目录没有父目录：{}", destination.display()))
    })?;
    fs::create_dir_all(parent)?;
    let staging_root = parent.join(".tmp");
    fs::create_dir_all(&staging_root)?;
    let unique = REPLACE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let staging = staging_root.join(format!(".staging-{nanos}-{unique}"));
    let backup = staging_root.join(format!(".backup-{nanos}-{unique}"));

    copy_dir_all(source, &staging)?;
    let had_previous = destination.exists();
    if had_previous {
        fs::rename(destination, &backup)?;
    }
    if let Err(error) = fs::rename(&staging, destination) {
        if had_previous {
            let _ = fs::rename(&backup, destination);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(PluginError::Io(error));
    }
    if had_previous {
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(())
}

fn is_pinned_git_commit(reference: &str) -> bool {
    reference.len() == 40 && reference.chars().all(|ch| ch.is_ascii_hexdigit())
}

/// 对插件目录做确定性 sha256：按相对路径排序，逐个写入路径、长度与内容。
fn compute_plugin_tree_sha256(root: &Path) -> Result<String, PluginError> {
    use sha2::{Digest, Sha256};

    let mut files = Vec::new();
    collect_tree_files(root, root, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut hasher = Sha256::new();
    for (relative, path) in files {
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0u8]);
        let bytes = fs::read(&path)?;
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_tree_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), PluginError> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" || name == ".tmp" {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_tree_files(root, &path, files)?;
        } else {
            let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            files.push((relative, path));
        }
    }
    Ok(())
}

fn discover_plugin_dirs(root: &Path) -> Result<Vec<PathBuf>, PluginError> {
    match fs::read_dir(root) {
        Ok(entries) => {
            let mut paths = Vec::new();
            for entry in entries {
                let path = entry?.path();
                if path.is_dir() && plugin_manifest_path(&path).is_ok() {
                    paths.push(path);
                }
            }
            paths.sort();
            Ok(paths)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(PluginError::Io(error)),
    }
}

fn plugin_id(name: &str, marketplace: &str) -> String {
    format!("{name}@{marketplace}")
}

fn sanitize_plugin_id(plugin_id: &str) -> String {
    plugin_id
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | '@' | ':' => '-',
            other => other,
        })
        .collect()
}

fn describe_install_source(source: &PluginInstallSource) -> String {
    match source {
        PluginInstallSource::LocalPath { path } => path.display().to_string(),
        PluginInstallSource::GitUrl { url } => url.clone(),
    }
}

fn unix_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be after epoch")
        .as_millis()
}

fn copy_dir_all(source: &Path, destination: &Path) -> Result<(), PluginError> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn update_settings_json(
    path: &Path,
    mut update: impl FnMut(&mut Map<String, Value>),
) -> Result<(), PluginError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut root = match fs::read_to_string(path) {
        Ok(contents) if !contents.trim().is_empty() => serde_json::from_str::<Value>(&contents)?,
        Ok(_) => Value::Object(Map::new()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(error) => return Err(PluginError::Io(error)),
    };

    let object = root.as_object_mut().ok_or_else(|| {
        PluginError::InvalidManifest(format!(
            "settings file {} must contain a JSON object",
            path.display()
        ))
    })?;
    update(object);
    fs::write(path, serde_json::to_string_pretty(&root)?)?;
    Ok(())
}

fn ensure_object<'a>(root: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    if !root.get(key).is_some_and(Value::is_object) {
        root.insert(key.to_string(), Value::Object(Map::new()));
    }
    root.get_mut(key)
        .and_then(Value::as_object_mut)
        .expect("object should exist")
}

/// Environment variable lock for test isolation.
/// Guards against concurrent modification of `CLAW_CONFIG_HOME`.
#[cfg(test)]
fn env_lock() -> &'static std::sync::Mutex<()> {
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &ENV_LOCK
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn temp_dir(label: &str) -> TempDirGuard {
        TempDirGuard::new(label)
    }

    /// RAII guard:测试 panic 时自动清理临时目录,防止残留文件被
    /// TRAE CN 文件监视器检测到并自动打开,从而避免 TRAE CN 打开文件
    /// 时的控制序列泄漏到 claw TUI 共享终端污染 InputLine。
    ///
    /// 使用 CARGO_TARGET_TMPDIR(target/tmp/)而非 std::env::temp_dir()
    /// (%TEMP%),因为 TRAE CN 文件监视器会扫描 %TEMP% 中的 .sh 文件并
    /// 自动打开,导致控制序列泄漏。target/ 目录在 .gitignore 中,TRAE CN
    /// 不会监视。
    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(label: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time should be after epoch")
                .as_nanos();
            // 使用 target/tmp/ 而非 %TEMP%:CARGO_TARGET_TMPDIR 仅对 [[test]]
            // 目标可用,对 lib 内 #[cfg(test)] 不可用,因此用
            // env!("CARGO_MANIFEST_DIR")(编译时注入)回溯到 workspace target/。
            // target/ 在 .gitignore 中,TRAE CN 不会监视,避免 .sh 文件
            // 被自动打开后控制序列泄漏到 TUI 终端。
            let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            let target_tmp = manifest_dir
                .parent() // crates/plugins → crates
                .and_then(|p| p.parent()) // crates → workspace root
                .map(|root| root.join("target").join("tmp"))
                .unwrap_or_else(std::env::temp_dir);
            let path = target_tmp.join(format!("plugins-{label}-{nanos}"));
            std::fs::create_dir_all(&path).expect("create target/tmp");
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            // Windows 上文件句柄释放有延迟(子进程退出、AV 扫描、
            // TRAE CN 文件读取等),remove_dir_all 可能因 "file in use"
            // 失败。重试几次,每次间隔递增。
            let mut delay_ms = 10;
            for attempt in 0..5u32 {
                match fs::remove_dir_all(&self.path) {
                    Ok(()) => return,
                    Err(_) if attempt < 4 => {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                        delay_ms *= 2;
                    }
                    Err(_) => return,
                }
            }
        }
    }

    impl std::ops::Deref for TempDirGuard {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.path
        }
    }

    impl AsRef<Path> for TempDirGuard {
        fn as_ref(&self) -> &Path {
            &self.path
        }
    }

    impl AsRef<std::ffi::OsStr> for TempDirGuard {
        fn as_ref(&self) -> &std::ffi::OsStr {
            self.path.as_os_str()
        }
    }

    #[test]
    fn env_guard_recovers_after_poisoning() {
        let poisoned = std::thread::spawn(|| {
            let _guard = env_guard();
            panic!("poison env lock");
        })
        .join();
        assert!(poisoned.is_err(), "poisoning thread should panic");

        let _guard = env_guard();
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent dir");
        }
        fs::write(path, contents).expect("write file");
    }

    fn write_loader_plugin(root: &Path) {
        write_file(
            root.join("hooks").join("pre.sh").as_path(),
            "#!/bin/sh\nprintf 'pre'\n",
        );
        write_file(
            root.join("tools").join("echo-tool.sh").as_path(),
            "#!/bin/sh\ncat\n",
        );
        write_file(
            root.join("commands").join("sync.sh").as_path(),
            "#!/bin/sh\nprintf 'sync'\n",
        );
        write_file(
            root.join(MANIFEST_FILE_NAME).as_path(),
            r#"{
  "name": "loader-demo",
  "version": "1.2.3",
  "description": "Manifest loader test plugin",
  "permissions": ["read", "write"],
  "hooks": {
    "PreToolUse": ["./hooks/pre.sh"]
  },
  "tools": [
    {
      "name": "echo_tool",
      "description": "Echoes JSON input",
      "inputSchema": {
        "type": "object"
      },
      "command": "./tools/echo-tool.sh",
      "requiredPermission": "workspace-write"
    }
  ],
  "commands": [
    {
      "name": "sync",
      "description": "Sync command",
      "command": "./commands/sync.sh"
    }
  ]
}"#,
        );
    }

    fn write_external_plugin(root: &Path, name: &str, version: &str) {
        write_file(
            root.join("hooks").join("pre.sh").as_path(),
            "#!/bin/sh\nprintf 'pre'\n",
        );
        write_file(
            root.join("hooks").join("post.sh").as_path(),
            "#!/bin/sh\nprintf 'post'\n",
        );
        write_file(
            root.join(MANIFEST_RELATIVE_PATH).as_path(),
            format!(
                "{{\n  \"name\": \"{name}\",\n  \"version\": \"{version}\",\n  \"description\": \"test plugin\",\n  \"hooks\": {{\n    \"PreToolUse\": [\"./hooks/pre.sh\"],\n    \"PostToolUse\": [\"./hooks/post.sh\"]\n  }}\n}}"
            )
            .as_str(),
        );
    }

    fn write_broken_plugin(root: &Path, name: &str) {
        write_file(
            root.join(MANIFEST_RELATIVE_PATH).as_path(),
            format!(
                "{{\n  \"name\": \"{name}\",\n  \"version\": \"1.0.0\",\n  \"description\": \"broken plugin\",\n  \"hooks\": {{\n    \"PreToolUse\": [\"./hooks/missing.sh\"]\n  }}\n}}"
            )
            .as_str(),
        );
    }

    fn write_directory_path_plugin(root: &Path, name: &str) {
        fs::create_dir_all(root.join("hooks").join("pre-dir")).expect("hook dir");
        fs::create_dir_all(root.join("tools").join("tool-dir")).expect("tool dir");
        fs::create_dir_all(root.join("commands").join("sync-dir")).expect("command dir");
        fs::create_dir_all(root.join("lifecycle").join("init-dir")).expect("lifecycle dir");
        write_file(
            root.join(MANIFEST_FILE_NAME).as_path(),
            format!(
                "{{\n  \"name\": \"{name}\",\n  \"version\": \"1.0.0\",\n  \"description\": \"directory path plugin\",\n  \"hooks\": {{\n    \"PreToolUse\": [\"./hooks/pre-dir\"]\n  }},\n  \"lifecycle\": {{\n    \"Init\": [\"./lifecycle/init-dir\"]\n  }},\n  \"tools\": [\n    {{\n      \"name\": \"dir_tool\",\n      \"description\": \"Directory tool\",\n      \"inputSchema\": {{\"type\": \"object\"}},\n      \"command\": \"./tools/tool-dir\"\n    }}\n  ],\n  \"commands\": [\n    {{\n      \"name\": \"sync\",\n      \"description\": \"Directory command\",\n      \"command\": \"./commands/sync-dir\"\n    }}\n  ]\n}}"
            )
            .as_str(),
        );
    }

    fn write_broken_failure_hook_plugin(root: &Path, name: &str) {
        write_file(
            root.join(MANIFEST_RELATIVE_PATH).as_path(),
            format!(
                "{{\n  \"name\": \"{name}\",\n  \"version\": \"1.0.0\",\n  \"description\": \"broken plugin\",\n  \"hooks\": {{\n    \"PostToolUseFailure\": [\"./hooks/missing-failure.sh\"]\n  }}\n}}"
            )
            .as_str(),
        );
    }

    fn write_lifecycle_plugin(root: &Path, name: &str, version: &str) -> PathBuf {
        let log_path = root.join("lifecycle.log");
        // 跨平台 inline 命令:不创建 .sh 文件,避免文件被 TRAE CN 文件
        // 监视器检测并自动打开后控制序列泄漏到 TUI 终端。
        // is_literal_command() 会将非路径命令识别为字面命令,跳过路径验证。
        let (init_cmd, shutdown_cmd) = if cfg!(windows) {
            ("echo init>>lifecycle.log", "echo shutdown>>lifecycle.log")
        } else {
            (
                "printf 'init\\n' >> lifecycle.log",
                "printf 'shutdown\\n' >> lifecycle.log",
            )
        };
        let manifest = serde_json::json!({
            "name": name,
            "version": version,
            "description": "lifecycle plugin",
            "lifecycle": {
                "Init": [init_cmd],
                "Shutdown": [shutdown_cmd]
            }
        });
        write_file(
            root.join(MANIFEST_RELATIVE_PATH).as_path(),
            &manifest.to_string(),
        );
        log_path
    }

    fn write_tool_plugin(root: &Path, name: &str, version: &str) {
        write_tool_plugin_with_name(root, name, version, "plugin_echo");
    }

    fn write_tool_plugin_with_name(root: &Path, name: &str, version: &str, tool_name: &str) {
        // 跨平台 inline 命令:不创建 .sh 文件,用 command+args 方式。
        // Unix: sh -c "INPUT=$(cat); printf ..."
        // Windows: powershell -NoProfile -Command "$i=[Console]::In.ReadToEnd(); ..."
        let (command, args) = if cfg!(windows) {
            // PowerShell:用单引号字符串拼接,避免 \" 转义问题
            // (PowerShell 中 \" 不是有效转义,需用 `" 或单引号拼接)
            let script = r#"$i=[Console]::In.ReadToEnd(); '{"plugin":"' + $env:CLAWD_PLUGIN_ID + '","tool":"' + $env:CLAWD_TOOL_NAME + '","input":' + $i + '}'"#;
            (
                "powershell".to_string(),
                vec![
                    "-NoProfile".to_string(),
                    "-Command".to_string(),
                    script.to_string(),
                ],
            )
        } else {
            let script = r#"INPUT=$(cat); printf '{"plugin":"%s","tool":"%s","input":%s}\n' "$CLAWD_PLUGIN_ID" "$CLAWD_TOOL_NAME" "$INPUT""#;
            ("sh".to_string(), vec!["-c".to_string(), script.to_string()])
        };
        let manifest = serde_json::json!({
            "name": name,
            "version": version,
            "description": "tool plugin",
            "tools": [{
                "name": tool_name,
                "description": "Echo JSON input",
                "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}, "required": ["message"], "additionalProperties": false},
                "command": command,
                "args": args,
                "requiredPermission": "workspace-write"
            }]
        });
        write_file(
            root.join(MANIFEST_RELATIVE_PATH).as_path(),
            &manifest.to_string(),
        );
    }

    fn write_bundled_plugin(root: &Path, name: &str, version: &str, default_enabled: bool) {
        write_file(
            root.join(MANIFEST_RELATIVE_PATH).as_path(),
            format!(
                "{{\n  \"name\": \"{name}\",\n  \"version\": \"{version}\",\n  \"description\": \"bundled plugin\",\n  \"defaultEnabled\": {}\n}}",
                if default_enabled { "true" } else { "false" }
            )
            .as_str(),
        );
    }

    fn load_enabled_plugins(path: &Path) -> BTreeMap<String, bool> {
        let contents = fs::read_to_string(path).expect("settings should exist");
        let root: Value = serde_json::from_str(&contents).expect("settings json");
        root.get("enabledPlugins")
            .and_then(Value::as_object)
            .map(|enabled_plugins| {
                enabled_plugins
                    .iter()
                    .map(|(plugin_id, value)| {
                        (
                            plugin_id.clone(),
                            value.as_bool().expect("plugin state should be a bool"),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn load_plugin_from_directory_validates_required_fields() {
        let _guard = env_guard();
        let root = temp_dir("manifest-required");
        write_file(
            root.join(MANIFEST_FILE_NAME).as_path(),
            r#"{"name":"","version":"1.0.0","description":"desc"}"#,
        );

        let error = load_plugin_from_directory(&root).expect_err("empty name should fail");
        assert!(error.to_string().contains("name cannot be empty"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_plugin_from_directory_reads_root_manifest_and_validates_entries() {
        let _guard = env_guard();
        let root = temp_dir("manifest-root");
        write_loader_plugin(&root);

        let manifest = load_plugin_from_directory(&root).expect("manifest should load");
        assert_eq!(manifest.name, "loader-demo");
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(
            manifest
                .permissions
                .iter()
                .map(|permission| permission.as_str())
                .collect::<Vec<_>>(),
            vec!["read", "write"]
        );
        assert_eq!(manifest.hooks.pre_tool_use, vec!["./hooks/pre.sh"]);
        assert_eq!(manifest.tools.len(), 1);
        assert_eq!(manifest.tools[0].name, "echo_tool");
        assert_eq!(
            manifest.tools[0].required_permission,
            PluginToolPermission::WorkspaceWrite
        );
        assert_eq!(manifest.commands.len(), 1);
        assert_eq!(manifest.commands[0].name, "sync");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_plugin_from_directory_supports_packaged_manifest_path() {
        let _guard = env_guard();
        let root = temp_dir("manifest-packaged");
        write_external_plugin(&root, "packaged-demo", "1.0.0");

        let manifest = load_plugin_from_directory(&root).expect("packaged manifest should load");
        assert_eq!(manifest.name, "packaged-demo");
        assert!(manifest.tools.is_empty());
        assert!(manifest.commands.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_plugin_from_directory_defaults_optional_fields() {
        let _guard = env_guard();
        let root = temp_dir("manifest-defaults");
        write_file(
            root.join(MANIFEST_FILE_NAME).as_path(),
            r#"{
  "name": "minimal",
  "version": "0.1.0",
  "description": "Minimal manifest"
}"#,
        );

        let manifest = load_plugin_from_directory(&root).expect("minimal manifest should load");
        assert!(manifest.permissions.is_empty());
        assert!(manifest.hooks.is_empty());
        assert!(manifest.tools.is_empty());
        assert!(manifest.commands.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_plugin_from_directory_rejects_duplicate_permissions_and_commands() {
        let _guard = env_guard();
        let root = temp_dir("manifest-duplicates");
        write_file(
            root.join("commands").join("sync.sh").as_path(),
            "#!/bin/sh\nprintf 'sync'\n",
        );
        write_file(
            root.join(MANIFEST_FILE_NAME).as_path(),
            r#"{
  "name": "duplicate-manifest",
  "version": "1.0.0",
  "description": "Duplicate validation",
  "permissions": ["read", "read"],
  "commands": [
    {"name": "sync", "description": "Sync one", "command": "./commands/sync.sh"},
    {"name": "sync", "description": "Sync two", "command": "./commands/sync.sh"}
  ]
}"#,
        );

        let error = load_plugin_from_directory(&root).expect_err("duplicates should fail");
        match error {
            PluginError::ManifestValidation(errors) => {
                assert!(errors.iter().any(|error| matches!(
                    error,
                    PluginManifestValidationError::DuplicatePermission { permission }
                    if permission == "read"
                )));
                assert!(errors.iter().any(|error| matches!(
                    error,
                    PluginManifestValidationError::DuplicateEntry { kind, name }
                    if *kind == "command" && name == "sync"
                )));
            }
            other => panic!("expected manifest validation errors, got {other}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_plugin_from_directory_normalizes_claude_code_components() {
        let root = temp_dir("manifest-claude-code-contract");
        write_file(
            root.join(MANIFEST_FILE_NAME).as_path(),
            r#"{
  "name": "oh-my-claudecode",
  "version": "4.10.2",
  "description": "Claude Code plugin manifest",
  "hooks": {
    "SessionStart": ["hooks/session-start.sh"],
    "PreToolUse": [
      {"matcher": "Bash", "hooks": [{"type": "command", "command": "hooks/pre.sh", "timeout": 30}]}
    ]
  },
  "agents": ["agents/*.md"],
  "commands": ["commands/**/*.md"],
  "skills": "./skills/",
  "mcpServers": "./.mcp.json",
  "userConfig": {
    "apiKey": {"title": "API Key", "type": "string", "required": true, "sensitive": true}
  }
}"#,
        );
        write_file(
            &root.join("skills").join("demo").join("SKILL.md"),
            "# demo\n",
        );
        write_file(
            &root.join("agents").join("reviewer.md"),
            "---\nname: reviewer\n---\n",
        );
        write_file(&root.join("commands").join("hello.md"), "# hello\n");
        write_file(
            &root.join(".mcp.json"),
            r#"{
  "mcpServers": {
    "local": {"command": "node", "args": ["${CLAUDE_PLUGIN_ROOT}/server.js"]},
    "remote": {"type": "http", "url": "https://example.com/mcp", "headers": {"Authorization": "Bearer token"}, "timeout": 12},
    "legacy": {"type": "sse", "url": "https://example.com/sse"},
    "socket": {"transport": "ws", "url": "wss://example.com/ws", "headers": {"X-Token": "${CLAW_TEST_MCP_TOKEN}"}}
  }
}"#,
        );
        std::env::set_var("CLAW_TEST_MCP_TOKEN", "from-env");

        let manifest =
            load_plugin_from_directory(&root).expect("Claude Code manifest should normalize");

        assert_eq!(
            manifest.components.skill_paths,
            vec![root.join("skills")],
            "技能根应直接指向包含 <skill>/SKILL.md 的目录"
        );
        assert_eq!(manifest.components.agent_paths, vec![root.join("agents")]);
        assert_eq!(
            manifest.components.command_paths,
            vec![root.join("commands").join("hello.md")]
        );

        let pretool = manifest
            .components
            .hooks
            .iter()
            .find(|hook| hook.event == "PreToolUse")
            .expect("PreToolUse 应被归一化");
        assert_eq!(pretool.matcher.as_deref(), Some("Bash"));
        assert_eq!(pretool.command, "hooks/pre.sh");
        assert_eq!(pretool.timeout_ms, Some(30_000));
        assert_eq!(
            manifest.lifecycle.init,
            vec!["hooks/session-start.sh".to_string()]
        );

        assert_eq!(manifest.components.mcp_servers.len(), 4);
        let local = manifest
            .components
            .mcp_servers
            .iter()
            .find(|server| server.name == "local")
            .expect("stdio server");
        assert_eq!(local.transport, "stdio");
        assert_eq!(local.command.as_deref(), Some("node"));
        let remote = manifest
            .components
            .mcp_servers
            .iter()
            .find(|server| server.name == "remote")
            .expect("http server");
        assert_eq!(remote.transport, "http");
        assert_eq!(remote.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(
            remote.headers.get("Authorization").map(String::as_str),
            Some("Bearer token")
        );
        assert_eq!(remote.timeout_ms, Some(12_000));

        let legacy = manifest
            .components
            .mcp_servers
            .iter()
            .find(|server| server.name == "legacy")
            .expect("sse server");
        assert_eq!(legacy.transport, "sse");
        assert_eq!(legacy.url.as_deref(), Some("https://example.com/sse"));

        let socket = manifest
            .components
            .mcp_servers
            .iter()
            .find(|server| server.name == "socket")
            .expect("ws server");
        assert_eq!(socket.transport, "ws");
        assert_eq!(socket.url.as_deref(), Some("wss://example.com/ws"));
        assert_eq!(
            socket.headers.get("X-Token").map(String::as_str),
            Some("from-env"),
            "headers 中的 ${{VAR}} 应从环境变量展开"
        );
        std::env::remove_var("CLAW_TEST_MCP_TOKEN");

        let config = manifest
            .components
            .user_config
            .iter()
            .find(|field| field.name == "apiKey")
            .expect("userConfig 应被归一化");
        assert!(config.required);
        assert!(config.sensitive);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_plugin_from_directory_rejects_missing_tool_or_command_paths() {
        let root = temp_dir("manifest-paths");
        write_file(
            root.join(MANIFEST_FILE_NAME).as_path(),
            r#"{
  "name": "missing-paths",
  "version": "1.0.0",
  "description": "Missing path validation",
  "tools": [
    {
      "name": "tool_one",
      "description": "Missing tool script",
      "inputSchema": {"type": "object"},
      "command": "./tools/missing.sh"
    }
  ]
}"#,
        );

        let error = load_plugin_from_directory(&root).expect_err("missing paths should fail");
        assert!(error.to_string().contains("does not exist"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_plugin_from_directory_rejects_missing_lifecycle_paths() {
        // given
        let root = temp_dir("manifest-lifecycle-paths");
        write_file(
            root.join(MANIFEST_FILE_NAME).as_path(),
            r#"{
  "name": "missing-lifecycle-paths",
  "version": "1.0.0",
  "description": "Missing lifecycle path validation",
  "lifecycle": {
    "Init": ["./lifecycle/init.sh"],
    "Shutdown": ["./lifecycle/shutdown.sh"]
  }
}"#,
        );

        // when
        let error =
            load_plugin_from_directory(&root).expect_err("missing lifecycle paths should fail");

        // then
        match error {
            PluginError::ManifestValidation(errors) => {
                assert!(errors.iter().any(|error| matches!(
                    error,
                    PluginManifestValidationError::MissingPath { kind, path }
                    if *kind == "lifecycle command"
                        && path.ends_with(Path::new("lifecycle/init.sh"))
                )));
                assert!(errors.iter().any(|error| matches!(
                    error,
                    PluginManifestValidationError::MissingPath { kind, path }
                    if *kind == "lifecycle command"
                        && path.ends_with(Path::new("lifecycle/shutdown.sh"))
                )));
            }
            other => panic!("expected manifest validation errors, got {other}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_plugin_from_directory_rejects_directory_command_paths() {
        // given
        let root = temp_dir("manifest-directory-paths");
        write_directory_path_plugin(&root, "directory-paths");

        // when
        let error =
            load_plugin_from_directory(&root).expect_err("directory command paths should fail");

        // then
        match error {
            PluginError::ManifestValidation(errors) => {
                assert!(errors.iter().any(|error| matches!(
                    error,
                    PluginManifestValidationError::PathIsDirectory { kind, path }
                    if *kind == "hook" && path.ends_with(Path::new("hooks/pre-dir"))
                )));
                assert!(errors.iter().any(|error| matches!(
                    error,
                    PluginManifestValidationError::PathIsDirectory { kind, path }
                    if *kind == "lifecycle command"
                        && path.ends_with(Path::new("lifecycle/init-dir"))
                )));
                assert!(errors.iter().any(|error| matches!(
                    error,
                    PluginManifestValidationError::PathIsDirectory { kind, path }
                    if *kind == "tool" && path.ends_with(Path::new("tools/tool-dir"))
                )));
                assert!(errors.iter().any(|error| matches!(
                    error,
                    PluginManifestValidationError::PathIsDirectory { kind, path }
                    if *kind == "command" && path.ends_with(Path::new("commands/sync-dir"))
                )));
            }
            other => panic!("expected manifest validation errors, got {other}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_plugin_from_directory_rejects_invalid_permissions() {
        let root = temp_dir("manifest-invalid-permissions");
        write_file(
            root.join(MANIFEST_FILE_NAME).as_path(),
            r#"{
  "name": "invalid-permissions",
  "version": "1.0.0",
  "description": "Invalid permission validation",
  "permissions": ["admin"]
}"#,
        );

        let error = load_plugin_from_directory(&root).expect_err("invalid permissions should fail");
        match error {
            PluginError::ManifestValidation(errors) => {
                assert!(errors.iter().any(|error| matches!(
                    error,
                    PluginManifestValidationError::InvalidPermission { permission }
                    if permission == "admin"
                )));
            }
            other => panic!("expected manifest validation errors, got {other}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_plugin_from_directory_rejects_invalid_tool_required_permission() {
        let root = temp_dir("manifest-invalid-tool-permission");
        write_file(
            root.join("tools").join("echo.sh").as_path(),
            "#!/bin/sh\ncat\n",
        );
        write_file(
            root.join(MANIFEST_FILE_NAME).as_path(),
            r#"{
  "name": "invalid-tool-permission",
  "version": "1.0.0",
  "description": "Invalid tool permission validation",
  "tools": [
    {
      "name": "echo_tool",
      "description": "Echo tool",
      "inputSchema": {"type": "object"},
      "command": "./tools/echo.sh",
      "requiredPermission": "admin"
    }
  ]
}"#,
        );

        let error =
            load_plugin_from_directory(&root).expect_err("invalid tool permission should fail");
        match error {
            PluginError::ManifestValidation(errors) => {
                assert!(errors.iter().any(|error| matches!(
                    error,
                    PluginManifestValidationError::InvalidToolRequiredPermission {
                        tool_name,
                        permission
                    } if tool_name == "echo_tool" && permission == "admin"
                )));
            }
            other => panic!("expected manifest validation errors, got {other}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_plugin_from_directory_accumulates_multiple_validation_errors() {
        let root = temp_dir("manifest-multi-error");
        write_file(
            root.join(MANIFEST_FILE_NAME).as_path(),
            r#"{
  "name": "",
  "version": "1.0.0",
  "description": "",
  "permissions": ["admin"],
  "commands": [
    {"name": "", "description": "", "command": "./commands/missing.sh"}
  ]
}"#,
        );

        let error =
            load_plugin_from_directory(&root).expect_err("multiple manifest errors should fail");
        match error {
            PluginError::ManifestValidation(errors) => {
                assert!(errors.len() >= 3);
                assert!(errors.iter().any(|error| matches!(
                    error,
                    PluginManifestValidationError::EmptyField { field } if *field == "name"
                )));
                assert!(errors.iter().any(|error| matches!(
                    error,
                    PluginManifestValidationError::InvalidPermission { permission }
                    if permission == "admin"
                )));
            }
            other => panic!("expected manifest validation errors, got {other}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discovers_builtin_and_bundled_plugins() {
        let _guard = env_guard();
        let home = temp_dir("discover");
        let manager = PluginManager::new(PluginManagerConfig::new(&home));
        let plugins = manager.list_plugins().expect("plugins should list");
        assert!(plugins
            .iter()
            .any(|plugin| plugin.metadata.kind == PluginKind::Builtin));
        assert!(plugins
            .iter()
            .any(|plugin| plugin.metadata.kind == PluginKind::Bundled));
    }

    #[test]
    fn installs_enables_updates_and_uninstalls_external_plugins() {
        let _guard = env_guard();
        let config_home = temp_dir("home");
        let source_root = temp_dir("source");
        write_external_plugin(&source_root, "demo", "1.0.0");

        let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        let install = manager
            .install(source_root.to_str().expect("utf8 path"))
            .expect("install should succeed");
        assert_eq!(install.plugin_id, "demo@external");
        assert!(manager
            .list_plugins()
            .expect("list plugins")
            .iter()
            .any(|plugin| plugin.metadata.id == "demo@external" && plugin.enabled));

        let hooks = manager.aggregated_hooks().expect("hooks should aggregate");
        assert_eq!(hooks.pre_tool_use.len(), 1);
        assert!(hooks.pre_tool_use[0].contains("pre.sh"));

        manager
            .disable("demo@external")
            .expect("disable should work");
        assert!(manager
            .aggregated_hooks()
            .expect("hooks after disable")
            .is_empty());
        manager.enable("demo@external").expect("enable should work");

        write_external_plugin(&source_root, "demo", "2.0.0");
        let update = manager.update("demo@external").expect("update should work");
        assert_eq!(update.old_version, "1.0.0");
        assert_eq!(update.new_version, "2.0.0");

        manager
            .uninstall("demo@external")
            .expect("uninstall should work");
        assert!(!manager
            .list_plugins()
            .expect("list plugins")
            .iter()
            .any(|plugin| plugin.metadata.id == "demo@external"));

        let _ = fs::remove_dir_all(&config_home);
        let _ = fs::remove_dir_all(&source_root);
    }

    #[test]
    fn auto_installs_bundled_plugins_into_the_registry() {
        let _guard = env_guard();
        let config_home = temp_dir("bundled-home");
        let bundled_root = temp_dir("bundled-root");
        write_bundled_plugin(&bundled_root.join("starter"), "starter", "0.1.0", false);

        let mut config = PluginManagerConfig::new(&config_home);
        config.bundled_root = Some(bundled_root.to_path_buf());
        let manager = PluginManager::new(config);

        let installed = manager
            .list_installed_plugins()
            .expect("bundled plugins should auto-install");
        assert!(installed.iter().any(|plugin| {
            plugin.metadata.id == "starter@bundled"
                && plugin.metadata.kind == PluginKind::Bundled
                && !plugin.enabled
        }));

        let registry = manager.load_registry().expect("registry should exist");
        let record = registry
            .plugins
            .get("starter@bundled")
            .expect("bundled plugin should be recorded");
        assert_eq!(record.kind, PluginKind::Bundled);
        assert!(record.install_path.exists());

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(bundled_root);
    }

    #[test]
    fn default_bundled_root_loads_repo_bundles_as_installed_plugins() {
        let _guard = env_guard();
        let config_home = temp_dir("default-bundled-home");
        let manager = PluginManager::new(PluginManagerConfig::new(&config_home));

        let installed = manager
            .list_installed_plugins()
            .expect("default bundled plugins should auto-install");
        assert!(installed
            .iter()
            .any(|plugin| plugin.metadata.id == "example-bundled@bundled"));
        assert!(installed
            .iter()
            .any(|plugin| plugin.metadata.id == "sample-hooks@bundled"));

        let _ = fs::remove_dir_all(config_home);
    }

    #[test]
    fn bundled_sync_prunes_removed_bundled_registry_entries() {
        let _guard = env_guard();
        let config_home = temp_dir("bundled-prune-home");
        let bundled_root = temp_dir("bundled-prune-root");
        let stale_install_path = config_home
            .join("plugins")
            .join("installed")
            .join("stale-bundled-external");
        write_bundled_plugin(&bundled_root.join("active"), "active", "0.1.0", false);
        write_file(
            stale_install_path.join(MANIFEST_RELATIVE_PATH).as_path(),
            r#"{
  "name": "stale",
  "version": "0.1.0",
  "description": "stale bundled plugin"
}"#,
        );

        let mut config = PluginManagerConfig::new(&config_home);
        config.bundled_root = Some(bundled_root.to_path_buf());
        config.install_root = Some(config_home.join("plugins").join("installed"));
        let manager = PluginManager::new(config);

        let mut registry = InstalledPluginRegistry::default();
        registry.plugins.insert(
            "stale@bundled".to_string(),
            InstalledPluginRecord {
                kind: PluginKind::Bundled,
                id: "stale@bundled".to_string(),
                name: "stale".to_string(),
                version: "0.1.0".to_string(),
                description: "stale bundled plugin".to_string(),
                install_path: stale_install_path.clone(),
                source: PluginInstallSource::LocalPath {
                    path: bundled_root.join("stale"),
                },
                installed_at_unix_ms: 1,
                updated_at_unix_ms: 1,
                checksum: None,
                git_ref: None,
                subdir: None,
            },
        );
        manager.store_registry(&registry).expect("store registry");
        manager
            .write_enabled_state("stale@bundled", Some(true))
            .expect("seed bundled enabled state");

        let installed = manager
            .list_installed_plugins()
            .expect("bundled sync should succeed");
        assert!(installed
            .iter()
            .any(|plugin| plugin.metadata.id == "active@bundled"));
        assert!(!installed
            .iter()
            .any(|plugin| plugin.metadata.id == "stale@bundled"));

        let registry = manager.load_registry().expect("load registry");
        assert!(!registry.plugins.contains_key("stale@bundled"));
        assert!(!stale_install_path.exists());

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(bundled_root);
    }

    #[test]
    fn installed_plugin_discovery_keeps_registry_entries_outside_install_root() {
        let _guard = env_guard();
        let config_home = temp_dir("registry-fallback-home");
        let bundled_root = temp_dir("registry-fallback-bundled");
        let install_root = config_home.join("plugins").join("installed");
        let external_install_path = temp_dir("registry-fallback-external");
        write_file(
            external_install_path.join(MANIFEST_FILE_NAME).as_path(),
            r#"{
  "name": "registry-fallback",
  "version": "1.0.0",
  "description": "Registry fallback plugin"
}"#,
        );

        let mut config = PluginManagerConfig::new(&config_home);
        config.bundled_root = Some(bundled_root.to_path_buf());
        config.install_root = Some(install_root.clone());
        let manager = PluginManager::new(config);

        let mut registry = InstalledPluginRegistry::default();
        registry.plugins.insert(
            "registry-fallback@external".to_string(),
            InstalledPluginRecord {
                kind: PluginKind::External,
                id: "registry-fallback@external".to_string(),
                name: "registry-fallback".to_string(),
                version: "1.0.0".to_string(),
                description: "Registry fallback plugin".to_string(),
                install_path: external_install_path.to_path_buf(),
                source: PluginInstallSource::LocalPath {
                    path: external_install_path.to_path_buf(),
                },
                installed_at_unix_ms: 1,
                updated_at_unix_ms: 1,
                checksum: None,
                git_ref: None,
                subdir: None,
            },
        );
        manager.store_registry(&registry).expect("store registry");
        manager
            .write_enabled_state("stale-external@external", Some(true))
            .expect("seed stale external enabled state");

        let installed = manager
            .list_installed_plugins()
            .expect("registry fallback plugin should load");
        assert!(installed
            .iter()
            .any(|plugin| plugin.metadata.id == "registry-fallback@external"));

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(bundled_root);
        let _ = fs::remove_dir_all(external_install_path);
    }

    #[test]
    fn installed_plugin_discovery_prunes_stale_registry_entries() {
        let _guard = env_guard();
        let config_home = temp_dir("registry-prune-home");
        let bundled_root = temp_dir("registry-prune-bundled");
        let install_root = config_home.join("plugins").join("installed");
        let missing_install_path = temp_dir("registry-prune-missing");

        let mut config = PluginManagerConfig::new(&config_home);
        config.bundled_root = Some(bundled_root.to_path_buf());
        config.install_root = Some(install_root);
        let manager = PluginManager::new(config);

        let mut registry = InstalledPluginRegistry::default();
        registry.plugins.insert(
            "stale-external@external".to_string(),
            InstalledPluginRecord {
                kind: PluginKind::External,
                id: "stale-external@external".to_string(),
                name: "stale-external".to_string(),
                version: "1.0.0".to_string(),
                description: "stale external plugin".to_string(),
                install_path: missing_install_path.to_path_buf(),
                source: PluginInstallSource::LocalPath {
                    path: missing_install_path.to_path_buf(),
                },
                installed_at_unix_ms: 1,
                updated_at_unix_ms: 1,
                checksum: None,
                git_ref: None,
                subdir: None,
            },
        );
        manager.store_registry(&registry).expect("store registry");

        let installed = manager
            .list_installed_plugins()
            .expect("stale registry entries should be pruned");
        assert!(!installed
            .iter()
            .any(|plugin| plugin.metadata.id == "stale-external@external"));

        let registry = manager.load_registry().expect("load registry");
        assert!(!registry.plugins.contains_key("stale-external@external"));

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(bundled_root);
    }

    #[test]
    fn persists_bundled_plugin_enable_state_across_reloads() {
        let _guard = env_guard();
        let config_home = temp_dir("bundled-state-home");
        let bundled_root = temp_dir("bundled-state-root");
        write_bundled_plugin(&bundled_root.join("starter"), "starter", "0.1.0", false);

        let mut config = PluginManagerConfig::new(&config_home);
        config.bundled_root = Some(bundled_root.to_path_buf());
        let mut manager = PluginManager::new(config.clone());

        manager
            .enable("starter@bundled")
            .expect("enable bundled plugin should succeed");
        assert_eq!(
            load_enabled_plugins(&manager.settings_path()).get("starter@bundled"),
            Some(&true)
        );

        let mut reloaded_config = PluginManagerConfig::new(&config_home);
        reloaded_config.bundled_root = Some(bundled_root.to_path_buf());
        reloaded_config.enabled_plugins = load_enabled_plugins(&manager.settings_path());
        let reloaded_manager = PluginManager::new(reloaded_config);
        let reloaded = reloaded_manager
            .list_installed_plugins()
            .expect("bundled plugins should still be listed");
        assert!(reloaded
            .iter()
            .any(|plugin| { plugin.metadata.id == "starter@bundled" && plugin.enabled }));

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(bundled_root);
    }

    #[test]
    fn persists_bundled_plugin_disable_state_across_reloads() {
        let _guard = env_guard();
        let config_home = temp_dir("bundled-disabled-home");
        let bundled_root = temp_dir("bundled-disabled-root");
        write_bundled_plugin(&bundled_root.join("starter"), "starter", "0.1.0", true);

        let mut config = PluginManagerConfig::new(&config_home);
        config.bundled_root = Some(bundled_root.to_path_buf());
        let mut manager = PluginManager::new(config);

        manager
            .disable("starter@bundled")
            .expect("disable bundled plugin should succeed");
        assert_eq!(
            load_enabled_plugins(&manager.settings_path()).get("starter@bundled"),
            Some(&false)
        );

        let mut reloaded_config = PluginManagerConfig::new(&config_home);
        reloaded_config.bundled_root = Some(bundled_root.to_path_buf());
        reloaded_config.enabled_plugins = load_enabled_plugins(&manager.settings_path());
        let reloaded_manager = PluginManager::new(reloaded_config);
        let reloaded = reloaded_manager
            .list_installed_plugins()
            .expect("bundled plugins should still be listed");
        assert!(reloaded
            .iter()
            .any(|plugin| { plugin.metadata.id == "starter@bundled" && !plugin.enabled }));

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(bundled_root);
    }

    #[test]
    fn validates_plugin_source_before_install() {
        let _guard = env_guard();
        let config_home = temp_dir("validate-home");
        let source_root = temp_dir("validate-source");
        write_external_plugin(&source_root, "validator", "1.0.0");
        let manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        let manifest = manager
            .validate_plugin_source(source_root.to_str().expect("utf8 path"))
            .expect("manifest should validate");
        assert_eq!(manifest.name, "validator");
        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(source_root);
    }

    #[test]
    fn plugin_registry_tracks_enabled_state_and_lookup() {
        let _guard = env_guard();
        let config_home = temp_dir("registry-home");
        let source_root = temp_dir("registry-source");
        write_external_plugin(&source_root, "registry-demo", "1.0.0");

        let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        manager
            .install(source_root.to_str().expect("utf8 path"))
            .expect("install should succeed");
        manager
            .disable("registry-demo@external")
            .expect("disable should succeed");

        let registry = manager.plugin_registry().expect("registry should build");
        let plugin = registry
            .get("registry-demo@external")
            .expect("installed plugin should be discoverable");
        assert_eq!(plugin.metadata().name, "registry-demo");
        assert!(!plugin.is_enabled());
        assert!(registry.contains("registry-demo@external"));
        assert!(!registry.contains("missing@external"));

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(source_root);
    }

    #[test]
    fn plugin_registry_report_collects_load_failures_without_dropping_valid_plugins() {
        let _guard = env_guard();
        // given
        let config_home = temp_dir("report-home");
        let external_root = temp_dir("report-external");
        write_external_plugin(&external_root.join("valid"), "valid-report", "1.0.0");
        write_broken_plugin(&external_root.join("broken"), "broken-report");

        let mut config = PluginManagerConfig::new(&config_home);
        config.external_dirs = vec![external_root.to_path_buf()];
        let manager = PluginManager::new(config);

        // when
        let report = manager
            .plugin_registry_report()
            .expect("report should tolerate invalid external plugins");

        // then
        assert!(report.registry().contains("valid-report@external"));
        assert_eq!(report.failures().len(), 1);
        assert_eq!(report.failures()[0].kind, PluginKind::External);
        assert!(report.failures()[0]
            .plugin_root
            .ends_with(Path::new("broken")));
        assert!(report.failures()[0]
            .error()
            .to_string()
            .contains("does not exist"));

        let error = manager
            .plugin_registry()
            .expect_err("strict registry should surface load failures");
        match error {
            PluginError::LoadFailures(failures) => {
                assert_eq!(failures.len(), 1);
                assert!(failures[0].plugin_root.ends_with(Path::new("broken")));
            }
            other => panic!("expected load failures, got {other}"),
        }

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(external_root);
    }

    #[test]
    fn installed_plugin_registry_report_collects_load_failures_from_install_root() {
        let _guard = env_guard();
        // given
        let config_home = temp_dir("installed-report-home");
        let bundled_root = temp_dir("installed-report-bundled");
        let install_root = config_home.join("plugins").join("installed");
        write_lifecycle_plugin(&install_root.join("valid"), "installed-valid", "1.0.0");
        write_broken_plugin(&install_root.join("broken"), "installed-broken");

        let mut config = PluginManagerConfig::new(&config_home);
        config.bundled_root = Some(bundled_root.to_path_buf());
        config.install_root = Some(install_root);
        let manager = PluginManager::new(config);

        // when
        let report = manager
            .installed_plugin_registry_report()
            .expect("installed report should tolerate invalid installed plugins");

        // then
        assert!(report.registry().contains("installed-valid@external"));
        let summaries = report.summaries();
        let valid = summaries
            .iter()
            .find(|summary| summary.metadata.id == "installed-valid@external")
            .expect("valid plugin summary should be present");
        assert_eq!(valid.lifecycle_state(), "disabled");
        assert_eq!(valid.lifecycle.init.len(), 1);
        assert_eq!(valid.lifecycle.shutdown.len(), 1);
        assert_eq!(report.failures().len(), 1);
        assert!(report.failures()[0]
            .plugin_root
            .ends_with(Path::new("broken")));

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(bundled_root);
    }

    #[test]
    fn rejects_plugin_sources_with_missing_hook_paths() {
        let _guard = env_guard();
        // given
        let config_home = temp_dir("broken-home");
        let source_root = temp_dir("broken-source");
        write_broken_plugin(&source_root, "broken");

        let manager = PluginManager::new(PluginManagerConfig::new(&config_home));

        // when
        let error = manager
            .validate_plugin_source(source_root.to_str().expect("utf8 path"))
            .expect_err("missing hook file should fail validation");

        // then
        assert!(error.to_string().contains("does not exist"));

        let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        let install_error = manager
            .install(source_root.to_str().expect("utf8 path"))
            .expect_err("install should reject invalid hook paths");
        assert!(install_error.to_string().contains("does not exist"));

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(source_root);
    }

    #[test]
    fn rejects_plugin_sources_with_missing_failure_hook_paths() {
        let _guard = env_guard();
        // given
        let config_home = temp_dir("broken-failure-home");
        let source_root = temp_dir("broken-failure-source");
        write_broken_failure_hook_plugin(&source_root, "broken-failure");

        let manager = PluginManager::new(PluginManagerConfig::new(&config_home));

        // when
        let error = manager
            .validate_plugin_source(source_root.to_str().expect("utf8 path"))
            .expect_err("missing failure hook file should fail validation");

        // then
        assert!(error.to_string().contains("does not exist"));

        let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        let install_error = manager
            .install(source_root.to_str().expect("utf8 path"))
            .expect_err("install should reject invalid failure hook paths");
        assert!(install_error.to_string().contains("does not exist"));

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(source_root);
    }

    #[test]
    fn plugin_registry_runs_initialize_and_shutdown_for_enabled_plugins() {
        let _guard = env_guard();
        let config_home = temp_dir("lifecycle-home");
        let source_root = temp_dir("lifecycle-source");
        let _ = write_lifecycle_plugin(&source_root, "lifecycle-demo", "1.0.0");

        let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        let install = manager
            .install(source_root.to_str().expect("utf8 path"))
            .expect("install should succeed");
        let log_path = install.install_path.join("lifecycle.log");

        let registry = manager.plugin_registry().expect("registry should build");
        registry.initialize().expect("init should succeed");
        registry.shutdown().expect("shutdown should succeed");

        let log = fs::read_to_string(&log_path).expect("lifecycle log should exist");
        // Windows cmd echo 输出 \r\n,Unix printf 输出 \n,统一比较
        assert_eq!(log.replace("\r\n", "\n"), "init\nshutdown\n");

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(source_root);
    }

    #[test]
    fn aggregates_and_executes_plugin_tools() {
        let _guard = env_guard();
        let config_home = temp_dir("tool-home");
        let source_root = temp_dir("tool-source");
        write_tool_plugin(&source_root, "tool-demo", "1.0.0");

        let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        manager
            .install(source_root.to_str().expect("utf8 path"))
            .expect("install should succeed");

        let tools = manager.aggregated_tools().expect("tools should aggregate");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].definition().name, "plugin_echo");
        assert_eq!(tools[0].required_permission(), "workspace-write");

        let output = tools[0]
            .execute(&serde_json::json!({ "message": "hello" }))
            .expect("plugin tool should execute");
        let payload: Value = serde_json::from_str(&output).expect("valid json");
        assert_eq!(payload["plugin"], "tool-demo@external");
        assert_eq!(payload["tool"], "plugin_echo");
        assert_eq!(payload["input"]["message"], "hello");

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(source_root);
    }

    #[test]
    fn list_installed_plugins_scans_install_root_without_registry_entries() {
        let _guard = env_guard();
        let config_home = temp_dir("installed-scan-home");
        let bundled_root = temp_dir("installed-scan-bundled");
        let install_root = config_home.join("plugins").join("installed");
        let installed_plugin_root = install_root.join("scan-demo");
        write_file(
            installed_plugin_root.join(MANIFEST_FILE_NAME).as_path(),
            r#"{
  "name": "scan-demo",
  "version": "1.0.0",
  "description": "Scanned from install root"
}"#,
        );

        let mut config = PluginManagerConfig::new(&config_home);
        config.bundled_root = Some(bundled_root.to_path_buf());
        config.install_root = Some(install_root);
        let manager = PluginManager::new(config);

        let installed = manager
            .list_installed_plugins()
            .expect("installed plugins should scan directories");
        assert!(installed
            .iter()
            .any(|plugin| plugin.metadata.id == "scan-demo@external"));

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(bundled_root);
    }

    #[test]
    fn list_installed_plugins_scans_packaged_manifests_in_install_root() {
        let _guard = env_guard();
        let config_home = temp_dir("installed-packaged-scan-home");
        let bundled_root = temp_dir("installed-packaged-scan-bundled");
        let install_root = config_home.join("plugins").join("installed");
        let installed_plugin_root = install_root.join("scan-packaged");
        write_file(
            installed_plugin_root.join(MANIFEST_RELATIVE_PATH).as_path(),
            r#"{
  "name": "scan-packaged",
  "version": "1.0.0",
  "description": "Packaged manifest in install root"
}"#,
        );

        let mut config = PluginManagerConfig::new(&config_home);
        config.bundled_root = Some(bundled_root.to_path_buf());
        config.install_root = Some(install_root);
        let manager = PluginManager::new(config);

        let installed = manager
            .list_installed_plugins()
            .expect("installed plugins should scan packaged manifests");
        assert!(installed
            .iter()
            .any(|plugin| plugin.metadata.id == "scan-packaged@external"));

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(bundled_root);
    }

    /// Regression test for ROADMAP #41: verify that `CLAW_CONFIG_HOME` isolation prevents
    /// host `~/.claw/plugins/` from bleeding into test runs.
    #[test]
    fn claw_config_home_isolation_prevents_host_plugin_leakage() {
        let _guard = env_guard();

        // Create a temp directory to act as our isolated CLAW_CONFIG_HOME
        let config_home = temp_dir("isolated-home");
        let bundled_root = temp_dir("isolated-bundled");

        // Set CLAW_CONFIG_HOME to our temp directory
        std::env::set_var("CLAW_CONFIG_HOME", &config_home);

        // Create a test fixture plugin in the isolated config home
        let install_root = config_home.join("plugins").join("installed");
        let fixture_plugin_root = install_root.join("isolated-test-plugin");
        write_file(
            fixture_plugin_root.join(MANIFEST_RELATIVE_PATH).as_path(),
            r#"{
  "name": "isolated-test-plugin",
  "version": "1.0.0",
  "description": "Test fixture plugin in isolated config home"
}"#,
        );

        // Create PluginManager with isolated bundled_root - it should use the temp config_home, not host ~/.claw/
        let mut config = PluginManagerConfig::new(&config_home);
        config.bundled_root = Some(bundled_root.to_path_buf());
        let manager = PluginManager::new(config);

        // List installed plugins - should only see the test fixture, not host plugins
        let installed = manager
            .list_installed_plugins()
            .expect("installed plugins should list");

        // Verify we only see the test fixture plugin
        assert_eq!(
            installed.len(),
            1,
            "should only see the test fixture plugin, not host ~/.claw/plugins/"
        );
        assert_eq!(
            installed[0].metadata.id, "isolated-test-plugin@external",
            "should see the test fixture plugin"
        );

        // Cleanup
        std::env::remove_var("CLAW_CONFIG_HOME");
        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(bundled_root);
    }

    #[test]
    fn plugin_lifecycle_handles_parallel_execution() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use std::sync::Arc;
        use std::thread;

        let _guard = env_guard();

        // Shared base directory for all threads
        let base_dir = temp_dir("parallel-base");

        // Track successful installations and any errors
        let success_count = Arc::new(AtomicUsize::new(0));
        let error_count = Arc::new(AtomicUsize::new(0));

        // Spawn multiple threads to install plugins simultaneously
        let mut handles = Vec::new();
        for thread_id in 0..5 {
            let base_dir = base_dir.to_path_buf();
            let success_count = Arc::clone(&success_count);
            let error_count = Arc::clone(&error_count);

            let handle = thread::spawn(move || {
                // Create unique directories for this thread
                let config_home = base_dir.join(format!("config-{thread_id}"));
                let source_root = base_dir.join(format!("source-{thread_id}"));

                // Write lifecycle plugin for this thread
                let _log_path =
                    write_lifecycle_plugin(&source_root, &format!("parallel-{thread_id}"), "1.0.0");

                // Create PluginManager and install
                let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
                let install_result = manager.install(source_root.to_str().expect("utf8 path"));

                match install_result {
                    Ok(install) => {
                        let log_path = install.install_path.join("lifecycle.log");

                        // Initialize and shutdown the registry to trigger lifecycle hooks
                        let registry = manager.plugin_registry();
                        match registry {
                            Ok(registry) => {
                                if registry.initialize().is_ok() && registry.shutdown().is_ok() {
                                    // Verify lifecycle.log exists and has expected content
                                    if let Ok(log) = fs::read_to_string(&log_path) {
                                        if log.replace("\r\n", "\n") == "init\nshutdown\n" {
                                            success_count.fetch_add(1, AtomicOrdering::Relaxed);
                                        }
                                    }
                                }
                            }
                            Err(_) => {
                                error_count.fetch_add(1, AtomicOrdering::Relaxed);
                            }
                        }
                    }
                    Err(_) => {
                        error_count.fetch_add(1, AtomicOrdering::Relaxed);
                    }
                }
            });
            handles.push(handle);
        }

        // Wait for all threads to complete
        for handle in handles {
            handle.join().expect("thread should complete");
        }

        // Verify all threads succeeded without collisions
        let successes = success_count.load(AtomicOrdering::Relaxed);
        let errors = error_count.load(AtomicOrdering::Relaxed);

        assert_eq!(
            successes, 5,
            "all 5 parallel plugin installations should succeed"
        );
        assert_eq!(
            errors, 0,
            "no errors should occur during parallel execution"
        );

        // Cleanup
        let _ = fs::remove_dir_all(base_dir);
    }
}
