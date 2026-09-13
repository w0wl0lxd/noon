use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use n00n_config_macro::ConfigSection;
use n00n_storage::paths;
use n00n_storage::sessions::{
    DEFAULT_MAX_RETAINED_SUBAGENT_HISTORIES, DEFAULT_MAX_RETAINED_TOOL_OUTPUTS, RetentionBudget,
    StoredThinking, ThinkingParseError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use thiserror::Error;
use tracing::warn;

const PROJECT_DIR: &str = ".n00n";
const PERMISSIONS_FILE: &str = "permissions.toml";

pub mod providers;

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1024;
pub const DEFAULT_MAX_OUTPUT_LINES: usize = 500;
pub const DEFAULT_FLASH_DURATION_MS: u64 = 1500;
pub const DEFAULT_TYPEWRITER_MS_PER_CHAR: u64 = 4;
pub const DEFAULT_MOUSE_SCROLL_LINES: u32 = 3;
pub const DEFAULT_MAX_INPUT_LINES: u32 = 20;
pub const MIN_MAX_INPUT_LINES: u32 = 1;
pub const DEFAULT_MCP_TOOL_DESC_MAX_CHARS: usize = 200;

pub const DEFAULT_MAX_CONTINUATION_TURNS: u32 = 3;
pub const DEFAULT_MAX_DEPTH: usize = 4;
pub const DEFAULT_MAX_TOTAL_DESCENDANTS: usize = 16;
pub const DEFAULT_MAX_ACTIVE_DESCENDANTS: usize = 8;
pub const DEFAULT_COMPACTION_BUFFER: CompactionBuffer = CompactionBuffer::Percent(20);

pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_LOW_SPEED_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_STREAM_TIMEOUT_SECS: u64 = 300;
pub const DEFAULT_OPENAI_CODING_PLAN_SLOTS: u64 = 8;
pub const MAX_OPENAI_CODING_PLAN_SLOTS: u64 = 8;
pub const DEFAULT_OPENAI_CODEX_ACCEPTS_PROMPT_CACHE_OPTIONS_IMPLICIT: bool = false;
pub const DEFAULT_OPENAI_CODEX_ACCEPTS_PROMPT_CACHE_OPTIONS_EXPLICIT: bool = false;
pub const DEFAULT_OPENAI_CODEX_ACCEPTS_PROMPT_CACHE_BREAKPOINTS: bool = false;
pub const DEFAULT_FUSION_LEAD_MODEL: &str = "codex/gpt-5.6-sol";
pub const DEFAULT_FUSION_SIDEKICK_MODEL: &str = "codex/gpt-5.6-luna";
pub const DEFAULT_FUSION_SIDEKICK_THINKING: &str = "max";

pub const DEFAULT_SESSION_ROUNDTRIP_TIMEOUT_SECS: u64 = 5;
pub const DEFAULT_SNAPSHOT_TIMEOUT_SECS: u64 = 2;

pub const DEFAULT_MAX_LOG_BYTES_MB: u64 = 200;
pub const DEFAULT_MAX_LOG_FILES: u32 = 10;
pub const DEFAULT_INPUT_HISTORY_SIZE: usize = 100;

pub const MIN_OUTPUT_BYTES: usize = 1024;
pub const MIN_OUTPUT_LINES: usize = 10;
pub const MIN_MAX_CONTINUATION_TURNS: u32 = 1;
pub const MIN_MAX_DEPTH: usize = 1;
pub const MIN_MAX_TOTAL_DESCENDANTS: usize = 1;
pub const MIN_MAX_ACTIVE_DESCENDANTS: usize = 1;
pub const MIN_COMPACTION_BUFFER: u32 = 1_000;
const MAX_COMPACTION_PERCENT: u8 = 99;
const COMPACTION_BUFFER_EXPECTED: &str =
    r#"a token count (e.g. 12000) or a percent of the context window (e.g. "20%")"#;
pub const MIN_MOUSE_SCROLL_LINES: u32 = 1;
pub const MIN_TOOL_OUTPUT_LINES: usize = 1;
pub const MIN_MAX_LOG_BYTES_MB: u64 = 1;
pub const MIN_MAX_LOG_FILES: u32 = 1;
pub const MIN_INPUT_HISTORY_SIZE: usize = 10;
/// A session still has to render the turn it is in, so the floor keeps the
/// outputs and histories of the most recent turns resident.
pub const MIN_MAX_RETAINED_TOOL_OUTPUTS: usize = 16;
pub const MIN_MAX_RETAINED_SUBAGENT_HISTORIES: usize = 4;
pub const MIN_SESSION_ROUNDTRIP_TIMEOUT_SECS: u64 = 1;
pub const MIN_SNAPSHOT_TIMEOUT_SECS: u64 = 1;
pub const MIN_CONNECT_TIMEOUT_SECS: u64 = 1;
pub const MIN_LOW_SPEED_TIMEOUT_SECS: u64 = 1;
pub const MIN_STREAM_TIMEOUT_SECS: u64 = 10;

pub const DEFAULT_BUILTINS: &[&str] = &[
    "agent_control",
    "bash",
    "batch",
    "blackboard",
    "code_execution",
    "codegraph",
    "edit",
    "explore",
    "fusion",
    "git",
    "github",
    "glob",
    "grep",
    "index",
    "memory",
    "question",
    "read",
    "semblem",
    "sessions",
    "skill",
    "smell",
    "task",
    "team",
    "tmux",
    "todo_write",
    "view_image",
    "webfetch",
    "websearch",
    "workflow",
    "write",
];

/// These used to be their own `tools.<name>` tables and are now edit plugin
/// options; the config layer uses this list to reject the old form with a
/// pointer to the new one.
pub const EDIT_SUB_TOOLS: &[&str] = &["edit_lines", "insert_lines", "multiedit"];

pub const FILE_WRITE_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "edit_file_bulk",
    "edit_file_lines",
    "insert_file_lines",
];

#[derive(Debug, Clone, Copy)]
pub enum ConfigValue {
    Bool(bool),
    U64(u64),
    Str(&'static str),
}

impl ConfigValue {
    #[must_use]
    pub fn format_default(&self) -> String {
        match self {
            Self::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Self::U64(v) => v.to_string(),
            Self::Str(s) => (*s).to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ConfigField {
    pub name: &'static str,
    pub ty: &'static str,
    pub default: ConfigValue,
    pub min: Option<u64>,
    pub description: &'static str,
}

pub const TOP_LEVEL_FIELDS: &[ConfigField] = &[
    ConfigField {
        name: "always_yolo",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        description: "Start every session with YOLO mode (skip permission prompts, deny rules still apply)",
    },
    ConfigField {
        name: "always_fast",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        description: "Start every session with Anthropic fast mode (Opus only; ignored otherwise)",
    },
    ConfigField {
        name: "always_workflow",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        description: "Start every session with workflow mode (task callable inside code_execution)",
    },
    ConfigField {
        name: "always_fusion",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        description: "Start every session with Fusion dual-lane routing (lead + sidekick)",
    },
    ConfigField {
        name: "always_thinking",
        ty: "bool | string",
        default: ConfigValue::Bool(false),
        min: None,
        description: "Start every session with extended thinking (true/\"adaptive\", \"off\", an effort level (\"minimal\" to \"max\"), or a token budget)",
    },
];
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid config: {section}.{field} = {value} is below minimum ({min})")]
    BelowMinimum {
        section: &'static str,
        field: &'static str,
        value: u64,
        min: u64,
    },
    #[error("invalid config: {section}.{field} = {value} exceeds maximum ({max})")]
    AboveMaximum {
        section: &'static str,
        field: &'static str,
        value: u64,
        max: u64,
    },
    #[error("invalid config: always_thinking: {0}")]
    Thinking(#[from] ThinkingParseError),
    #[error("invalid config: agent.fusion.sidekick_thinking: {0}")]
    InvalidFusionSidekickThinking(ThinkingParseError),
    #[error(
        "invalid config: plugins.{tool} was removed; {tool} is provided by the edit plugin, \
         set plugins.edit = {{ {tool} = true|false }} instead"
    )]
    RemovedEditSubTool { tool: &'static str },
    #[error(
        "invalid config: plugins.{plugin}: no bundled plugin is named \"{plugin}\" \
         (bundled plugins: {valid})"
    )]
    UnknownPlugin { plugin: String, valid: String },
    #[error(
        "invalid config: agent.fusion.sidekick_tier must be weak, medium, or strong, got {tier:?}"
    )]
    InvalidFusionSidekickTier { tier: crate::providers::Tier },
    #[error(
        "invalid config: agent lineage limits require max_depth <= max_total_descendants and \
         max_active_descendants <= max_total_descendants (got max_depth={max_depth}, \
         max_total_descendants={max_total_descendants}, max_active_descendants={max_active_descendants})"
    )]
    InvalidLineageLimits {
        max_depth: usize,
        max_total_descendants: usize,
        max_active_descendants: usize,
    },
}

fn check(
    section: &'static str,
    field: &'static str,
    value: u64,
    min: u64,
) -> Result<(), ConfigError> {
    if value < min {
        return Err(ConfigError::BelowMinimum {
            section,
            field,
            value,
            min,
        });
    }
    Ok(())
}

macro_rules! merge_option {
    ($self:ident, $overlay:ident, $($field:ident),+) => {
        $(if $overlay.$field.is_some() { $self.$field = $overlay.$field; })+
    };
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum AlwaysThinking {
    Toggle(bool),
    Budget(u32),
    Mode(String),
}

impl AlwaysThinking {
    fn resolve(self) -> Result<StoredThinking, ThinkingParseError> {
        match self {
            Self::Toggle(true) => Ok(StoredThinking::Adaptive),
            Self::Toggle(false) => Ok(StoredThinking::Off),
            Self::Budget(n) => StoredThinking::parse_setting(&n.to_string()),
            Self::Mode(s) => StoredThinking::parse_setting(&s),
        }
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct RawConfig {
    pub always_yolo: Option<bool>,
    pub always_fast: Option<bool>,
    pub always_workflow: Option<bool>,
    pub always_fusion: Option<bool>,
    pub always_thinking: Option<AlwaysThinking>,
    #[serde(default)]
    pub ui: UiFileConfig,
    pub agent: AgentFileConfig,
    pub provider: ProviderFileConfig,
    pub search: SearchFileConfig,
    pub storage: StorageFileConfig,
    pub plugins: HashMap<String, PluginFileConfig>,
}

impl RawConfig {
    pub fn merge(&mut self, overlay: RawConfig) {
        merge_option!(
            self,
            overlay,
            always_yolo,
            always_fast,
            always_workflow,
            always_fusion,
            always_thinking
        );
        self.ui.merge(overlay.ui);
        self.agent.merge(&overlay.agent);
        self.provider.merge(overlay.provider);
        self.search.merge(&overlay.search);
        self.storage.merge(&overlay.storage);
        for (name, plugin) in overlay.plugins {
            let entry = self.plugins.entry(name).or_default();
            if plugin.enabled.is_some() {
                entry.enabled = plugin.enabled;
            }
            entry.opts.extend(plugin.opts);
        }
    }

    /// Convert the parsed raw configuration into a validated `Config`.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError` if plugin tables are invalid or any thinking
    /// setting cannot be parsed.
    pub fn into_config(self, no_rtk: bool) -> Result<Config, ConfigError> {
        validate_plugin_tables(&self.plugins)?;

        let disabled_tools: Vec<String> = self
            .plugins
            .iter()
            .filter(|(_, cfg)| cfg.enabled == Some(false))
            .map(|(name, _)| name.clone())
            .collect();
        Ok(Config {
            always_yolo: self.always_yolo.is_some_and(|v| v),
            always_fast: self.always_fast.is_some_and(|v| v),
            always_workflow: self.always_workflow.is_some_and(|v| v),
            always_fusion: self.always_fusion.is_some_and(|v| v),
            always_thinking: self
                .always_thinking
                .map(AlwaysThinking::resolve)
                .transpose()?,
            ui: UiConfig::from_file(self.ui),
            agent: AgentConfig::from_file(self.agent.clone(), no_rtk, disabled_tools),
            provider: ProviderConfig::from_file(self.provider),
            search: SearchConfig::from_file(&self.search),
            storage: StorageConfig::from_file(&self.storage),
            permissions: PermissionsConfig::default(),
            project_trusted: false,
            plugins: PluginsConfig::from_plugins(&self.plugins),
        })
    }
}

fn validate_plugin_tables(plugins: &HashMap<String, PluginFileConfig>) -> Result<(), ConfigError> {
    for &name in EDIT_SUB_TOOLS {
        if plugins.contains_key(name) {
            return Err(ConfigError::RemovedEditSubTool { tool: name });
        }
    }
    let mut unknown: Vec<&String> = plugins
        .keys()
        .filter(|name| !DEFAULT_BUILTINS.contains(&name.as_str()))
        .collect();
    unknown.sort();
    if let Some(&plugin) = unknown.first() {
        return Err(ConfigError::UnknownPlugin {
            plugin: plugin.clone(),
            valid: DEFAULT_BUILTINS.join(", "),
        });
    }
    Ok(())
}

#[derive(Deserialize, Default, Debug)]
#[serde(default)]
pub struct PluginFileConfig {
    pub enabled: Option<bool>,
    /// Plugin-specific options passed through opaquely; each plugin declares
    /// and validates its own via `n00n.api.register_options`.
    #[serde(flatten)]
    pub opts: JsonMap<String, JsonValue>,
}
#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct UiFileConfig {
    pub splash_animation: Option<bool>,
    pub reduced_motion: Option<bool>,
    pub mascot: Option<bool>,
    pub scrollbar: Option<bool>,
    pub flash_duration_ms: Option<u64>,
    pub typewriter_ms_per_char: Option<u64>,
    pub mouse_scroll_lines: Option<u32>,
    pub show_thinking: Option<bool>,
    pub theme: Option<String>,
    pub tool_output_lines: Option<ToolOutputLinesFile>,
    pub max_input_lines: Option<u32>,
    pub notifications: Option<UiNotifications>,
    pub terminal_title: Option<bool>,
}

impl UiFileConfig {
    fn merge(&mut self, overlay: UiFileConfig) {
        merge_option!(
            self,
            overlay,
            splash_animation,
            reduced_motion,
            mascot,
            scrollbar,
            flash_duration_ms,
            typewriter_ms_per_char,
            mouse_scroll_lines,
            show_thinking,
            theme,
            max_input_lines,
            notifications,
            terminal_title
        );
        match (self.tool_output_lines.as_mut(), overlay.tool_output_lines) {
            (Some(base), Some(over)) => base.merge(&over),
            (None, Some(over)) => self.tool_output_lines = Some(over),
            _ => {}
        }
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct ToolOutputLinesFile {
    pub bash: Option<usize>,
    pub code_execution: Option<usize>,
    pub task: Option<usize>,
    pub workflow: Option<usize>,
    pub index: Option<usize>,
    pub grep: Option<usize>,
    pub explore: Option<usize>,
    pub read: Option<usize>,
    pub write: Option<usize>,
    pub web: Option<usize>,
    pub other: Option<usize>,
}

impl ToolOutputLinesFile {
    fn merge(&mut self, overlay: &ToolOutputLinesFile) {
        merge_option!(
            self,
            overlay,
            bash,
            code_execution,
            task,
            workflow,
            index,
            grep,
            explore,
            read,
            write,
            web,
            other
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionBuffer {
    Tokens(u32),
    Percent(u8),
}

impl CompactionBuffer {
    #[must_use]
    pub fn resolve(self, context_window: u32) -> u32 {
        match self {
            Self::Tokens(n) => n,
            Self::Percent(p) => u32::try_from(u64::from(context_window) * u64::from(p) / 100)
                .unwrap_or_else(|_| u32::MAX),
        }
    }
}

impl Serialize for CompactionBuffer {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Tokens(n) => s.serialize_u32(*n),
            Self::Percent(p) => s.collect_str(&format_args!("{p}%")),
        }
    }
}

impl<'de> Deserialize<'de> for CompactionBuffer {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct BufferVisitor;

        impl serde::de::Visitor<'_> for BufferVisitor {
            type Value = CompactionBuffer;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(COMPACTION_BUFFER_EXPECTED)
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                if let Ok(n) = u32::try_from(v)
                    && n >= MIN_COMPACTION_BUFFER
                {
                    Ok(CompactionBuffer::Tokens(n))
                } else {
                    Err(E::custom(format!(
                        "compaction_buffer must be at least {MIN_COMPACTION_BUFFER} tokens"
                    )))
                }
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                let u = u64::try_from(v)
                    .map_err(|_| E::custom(format!("compaction_buffer value {v} is negative")))?;
                self.visit_u64(u)
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Self::Value, E> {
                if let Some(n) = s.strip_suffix('%')
                    && let Ok(p) = n.trim().parse::<u8>()
                    && (1..=MAX_COMPACTION_PERCENT).contains(&p)
                {
                    Ok(CompactionBuffer::Percent(p))
                } else {
                    Err(E::custom(format!(
                        "invalid compaction_buffer {s:?}: expected {COMPACTION_BUFFER_EXPECTED}"
                    )))
                }
            }
        }

        d.deserialize_any(BufferVisitor)
    }
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct AgentFileConfig {
    pub max_output_bytes: Option<usize>,
    pub max_output_lines: Option<usize>,
    pub max_continuation_turns: Option<u32>,
    pub max_depth: Option<usize>,
    pub max_total_descendants: Option<usize>,
    pub max_active_descendants: Option<usize>,
    pub compaction_buffer: Option<CompactionBuffer>,
    pub mcp_tool_desc_max_chars: Option<usize>,
    pub session_roundtrip_timeout_secs: Option<u64>,
    pub dynamic_tools: Option<DynamicToolFileConfig>,
    pub fusion: Option<FusionFileConfig>,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct FusionFileConfig {
    pub enabled: Option<bool>,
    pub lead_model: Option<String>,
    pub sidekick_model: Option<String>,
    pub sidekick_thinking: Option<String>,
    pub sidekick_tier: Option<crate::providers::Tier>,
}
#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct DynamicToolFileConfig {
    pub enabled: Option<bool>,
    pub default_mode: Option<String>,
}
impl AgentFileConfig {
    fn merge(&mut self, overlay: &AgentFileConfig) {
        merge_option!(
            self,
            overlay,
            max_output_bytes,
            max_output_lines,
            max_continuation_turns,
            max_depth,
            max_total_descendants,
            max_active_descendants,
            compaction_buffer,
            mcp_tool_desc_max_chars,
            session_roundtrip_timeout_secs
        );
        match (self.dynamic_tools.as_mut(), overlay.dynamic_tools.clone()) {
            (Some(base), Some(over)) => {
                if over.enabled.is_some() {
                    base.enabled = over.enabled;
                }
                if over.default_mode.is_some() {
                    base.default_mode = over.default_mode;
                }
            }
            (None, Some(over)) => self.dynamic_tools = Some(over),
            _ => {}
        }
        match (self.fusion.as_mut(), overlay.fusion.clone()) {
            (Some(base), Some(over)) => {
                if over.enabled.is_some() {
                    base.enabled = over.enabled;
                }
                if over.lead_model.is_some() {
                    base.lead_model.clone_from(&over.lead_model);
                }
                if over.sidekick_model.is_some() {
                    base.sidekick_model.clone_from(&over.sidekick_model);
                }
                if over.sidekick_thinking.is_some() {
                    base.sidekick_thinking.clone_from(&over.sidekick_thinking);
                }
                if over.sidekick_tier.is_some() {
                    base.sidekick_tier = over.sidekick_tier;
                }
            }
            (None, Some(over)) => self.fusion = Some(over),
            _ => {}
        }
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderFileConfig {
    pub default_model: Option<String>,
    pub connect_timeout_secs: Option<u64>,
    pub low_speed_timeout_secs: Option<u64>,
    pub stream_timeout_secs: Option<u64>,
    pub openai_coding_plan_slots: Option<u64>,
    pub openai_codex_accepts_prompt_cache_options_implicit: Option<bool>,
    pub openai_codex_accepts_prompt_cache_options_explicit: Option<bool>,
    pub openai_codex_accepts_prompt_cache_breakpoints: Option<bool>,
}

impl ProviderFileConfig {
    fn merge(&mut self, overlay: ProviderFileConfig) {
        merge_option!(
            self,
            overlay,
            default_model,
            connect_timeout_secs,
            low_speed_timeout_secs,
            stream_timeout_secs,
            openai_coding_plan_slots,
            openai_codex_accepts_prompt_cache_options_implicit,
            openai_codex_accepts_prompt_cache_options_explicit,
            openai_codex_accepts_prompt_cache_breakpoints
        );
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct SearchFileConfig {
    pub enabled: Option<bool>,
}

impl SearchFileConfig {
    fn merge(&mut self, overlay: &Self) {
        merge_option!(self, overlay, enabled);
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct StorageFileConfig {
    pub max_log_bytes_mb: Option<u64>,
    pub max_log_files: Option<u32>,
    pub input_history_size: Option<usize>,
    pub max_retained_tool_outputs: Option<usize>,
    pub max_retained_subagent_histories: Option<usize>,
    pub snapshot_timeout_secs: Option<u64>,
}

impl StorageFileConfig {
    fn merge(&mut self, overlay: &StorageFileConfig) {
        merge_option!(
            self,
            overlay,
            max_log_bytes_mb,
            max_log_files,
            input_history_size,
            max_retained_tool_outputs,
            max_retained_subagent_histories,
            snapshot_timeout_secs
        );
    }
}

#[derive(Default)]
struct PermissionsFileConfig {
    default: Option<DefaultEffect>,
    tools: HashMap<String, ToolPermissions>,
    mcp_rules: Vec<PermissionRule>,
    mcp_defaults: HashMap<ToolKey, DefaultEffect>,
}

impl<'de> Deserialize<'de> for PermissionsFileConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let table = toml::Table::deserialize(deserializer)?;
        let default = table
            .get("default")
            .and_then(|v| DefaultEffect::deserialize(v.clone()).ok())
            .or_else(|| {
                table
                    .get("allow_all")?
                    .as_bool()?
                    .then_some(DefaultEffect::Allow)
            });

        let mut tools = HashMap::new();
        let mut mcp_rules = Vec::new();
        let mut mcp_defaults = HashMap::new();

        for (k, v) in &table {
            if k.is_empty() || k == "allow_all" || k == "default" {
                continue;
            }
            if k == "mcp" {
                // TOML [mcp.server] creates nested table: mcp → {server → {...}}
                if let Some(mcp_table) = v.as_table() {
                    for (server_name, server_value) in mcp_table {
                        if let Some(server_table) = server_value.as_table() {
                            parse_mcp_server_table(
                                server_name,
                                server_table,
                                &mut mcp_rules,
                                &mut mcp_defaults,
                            );
                        } else {
                            tracing::warn!(
                                server = server_name.as_str(),
                                "[mcp.{server_name}] is not a table — skipping"
                            );
                        }
                    }
                } else {
                    tracing::warn!("[mcp] is not a table (got {}) — skipping", v.type_str());
                }
            } else if let Ok(tp) = v.clone().try_into::<ToolPermissions>() {
                if k.contains('.') {
                    tracing::warn!(
                        key = k.as_str(),
                        "tool section [{k}] contains a dot — did you mean [mcp.{k}]? Skipping."
                    );
                } else {
                    tools.insert(k.clone(), tp);
                }
            }
        }

        Ok(Self {
            default,
            tools,
            mcp_rules,
            mcp_defaults,
        })
    }
}

#[derive(Deserialize)]
struct ToolPermissions {
    allow: Option<ScopeSet>,
    deny: Option<ScopeSet>,
    default: Option<DefaultEffect>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScopeSet {
    All(bool),
    Scopes(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultEffect {
    Allow,
    Deny,
    #[default]
    Prompt,
}

impl From<Effect> for DefaultEffect {
    fn from(e: Effect) -> Self {
        match e {
            Effect::Allow => DefaultEffect::Allow,
            Effect::Deny => DefaultEffect::Deny,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PermissionTarget {
    Global,
    Project(PathBuf),
}

use std::sync::Arc;

pub const TOOL_ALIASES: &[(&str, &str)] = &[
    ("agent_control", "control_agent"),
    ("agent_list", "list_agents"),
    ("agent_status", "get_agent"),
    ("batch", "run_batch"),
    ("bash", "run_shell"),
    ("blackboard", "use_blackboard"),
    ("code_execution", "run_python"),
    ("codegraph", "map_codegraph"),
    ("edit", "edit_file"),
    ("edit_lines", "edit_file_lines"),
    ("explore", "explore_code"),
    ("fusion_delegate", "delegate_fusion"),
    ("glob", "search_files"),
    ("grep", "search_code"),
    ("index", "index_file"),
    ("insert_lines", "insert_file_lines"),
    ("load_namespace", "load_toolset"),
    ("memory", "use_memory"),
    ("multi_edit", "edit_file_bulk"),
    ("multiedit", "edit_file_bulk"),
    ("question", "ask_user"),
    ("read", "read_file"),
    ("semblem", "search_text"),
    ("skill", "load_skill"),
    ("task", "run_task"),
    ("team", "run_team"),
    ("todo_write", "update_todo"),
    ("tool_search", "search_tools"),
    ("webfetch", "fetch_url"),
    ("websearch", "search_web"),
    ("workflow", "run_workflow"),
    ("write", "write_file"),
];

#[must_use]
pub fn canonical_tool_name(name: &str) -> &str {
    TOOL_ALIASES
        .iter()
        .find_map(|(alias, canonical)| (*alias == name).then_some(*canonical))
        .map_or(name, |canonical| canonical)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ToolKey {
    Wildcard,
    Native(Arc<str>),
    McpServer { server: Arc<str> },
    McpTool { server: Arc<str>, tool: Arc<str> },
}

/// NOTE: `ToolKey` deliberately does not implement `serde::Deserialize`.
/// Use `ToolKey::parse(&str)` at deserialization boundaries — it performs
/// validation (wire format, server name, length) that a blanket Deserialize
/// would skip. All current deserialization paths go through `parse`.
impl serde::Serialize for ToolKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Check if a name matches the LLM wire format: `^[a-zA-Z0-9_-]{1,64}$`.
/// Tool names with dots, over 64 chars, or special characters are rejected.
#[must_use]
pub fn is_valid_wire_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl ToolKey {
    /// Parse a qualified tool name into a `ToolKey`.
    ///
    /// Use this at config/dispatch boundaries where input is untrusted.
    ///
    /// # Errors
    ///
    /// Returns `ToolKeyParseError` for malformed input (empty names, empty
    /// server/tool parts, or tool names that don't match the wire format).
    pub fn parse(name: &str) -> Result<Self, ToolKeyParseError> {
        if name.is_empty() {
            return Err(ToolKeyParseError::EmptyName);
        }
        if name == "*" {
            return Ok(Self::Wildcard);
        }
        match name.split_once('.') {
            Some(("", _) | (_, "")) => Err(ToolKeyParseError::MalformedParts(name.to_string())),
            Some((server, "*")) => {
                if !is_valid_server_name(server) {
                    return Err(ToolKeyParseError::InvalidServerName(server.to_string()));
                }
                Ok(Self::McpServer {
                    server: server.into(),
                })
            }
            Some((server, tool)) => {
                if !is_valid_server_name(server) {
                    return Err(ToolKeyParseError::InvalidServerName(server.to_string()));
                }
                if !is_valid_wire_name(tool) {
                    return Err(ToolKeyParseError::InvalidToolName(tool.to_string()));
                }
                // Wire format is server__tool — check total length fits LLM API limits
                let wire_len = server.len() + 2 + tool.len();
                if wire_len > 64 {
                    return Err(ToolKeyParseError::WireNameTooLong {
                        server: server.to_string(),
                        tool: tool.to_string(),
                        len: wire_len,
                    });
                }
                Ok(Self::McpTool {
                    server: server.into(),
                    tool: tool.into(),
                })
            }
            None => {
                if !is_valid_wire_name(name) {
                    return Err(ToolKeyParseError::InvalidToolName(name.to_string()));
                }
                Ok(Self::Native(canonical_tool_name(name).into()))
            }
        }
    }

    /// Create a `ToolKey` from a known-valid native tool name.
    ///
    /// # Panics
    ///
    /// Panics if `name` is empty or contains dots. Use `ToolKey::parse` for
    /// untrusted input or MCP tool names.
    #[must_use]
    pub fn native(name: &str) -> Self {
        if name == "*" {
            Self::Wildcard
        } else {
            assert!(!name.is_empty(), "native tool name must not be empty");
            assert!(
                !name.contains('.'),
                "native tool name must not contain dots: {name:?} - use ToolKey::parse for MCP tools"
            );
            Self::Native(canonical_tool_name(name).into())
        }
    }

    #[must_use]
    pub fn is_mcp(&self) -> bool {
        matches!(self, Self::McpServer { .. } | Self::McpTool { .. })
    }
}

impl std::fmt::Display for ToolKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wildcard => write!(f, "*"),
            Self::Native(name) => write!(f, "{name}"),
            Self::McpServer { server } => write!(f, "{server}.*"),
            Self::McpTool { server, tool } => write!(f, "{server}.{tool}"),
        }
    }
}

/// Error returned when a tool key string fails validation.
#[derive(Debug, thiserror::Error)]
pub enum ToolKeyParseError {
    #[error("tool name is empty")]
    EmptyName,
    #[error("malformed tool key: empty server or tool part in {0:?}")]
    MalformedParts(String),
    #[error("invalid server name {0:?}: must match [a-zA-Z0-9-]{{1,64}}")]
    InvalidServerName(String),
    #[error("invalid tool name {0:?}: must match [a-zA-Z0-9_-]{{1,64}}")]
    InvalidToolName(String),
    #[error("wire name {server}__{tool} is {len} chars, max 64")]
    WireNameTooLong {
        server: String,
        tool: String,
        len: usize,
    },
}

#[derive(Debug, Clone)]
pub struct PermissionRule {
    pub tool: ToolKey,
    pub scope: Option<String>,
    pub effect: Effect,
}
#[derive(Debug, Clone, Default)]
pub struct PermissionsConfig {
    pub default: DefaultEffect,
    pub tool_defaults: HashMap<ToolKey, DefaultEffect>,
    pub rules: Vec<PermissionRule>,
    pub yolo: bool,
}

#[derive(Clone)]
pub struct Config {
    pub always_yolo: bool,
    pub always_fast: bool,
    pub always_workflow: bool,
    pub always_fusion: bool,
    pub always_thinking: Option<StoredThinking>,
    pub ui: UiConfig,
    pub agent: AgentConfig,
    pub provider: ProviderConfig,
    pub search: SearchConfig,
    pub storage: StorageConfig,
    pub permissions: PermissionsConfig,
    pub project_trusted: bool,
    pub plugins: PluginsConfig,
}

/// How n00n asks for the user's attention when a turn ends or a prompt
/// needs input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiNotifications {
    /// Emit nothing.
    Off,
    /// Ring the terminal bell (BEL). tmux surfaces it as a window bell, so
    /// it also works across muxes.
    #[default]
    Bell,
    /// Emit an OSC 9 desktop notification (kitty, wezterm, iTerm2); silent
    /// where unsupported.
    Osc9,
    /// Ring the bell and emit OSC 9.
    All,
}

/// Rejected `ui.notifications` / `N00N_NOTIFICATIONS` value.
#[derive(Debug, Error)]
#[error("invalid notification mode {0:?}; expected \"off\", \"bell\", \"osc9\", or \"all\"")]
pub struct InvalidNotificationMode(String);

impl std::str::FromStr for UiNotifications {
    type Err = InvalidNotificationMode;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "bell" => Ok(Self::Bell),
            "osc9" => Ok(Self::Osc9),
            "all" => Ok(Self::All),
            other => Err(InvalidNotificationMode(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, ConfigSection)]
#[config(section = "ui")]
pub struct UiConfig {
    #[config(default = true, desc = "Show splash animation on startup")]
    pub splash_animation: bool,

    #[config(
        default = false,
        desc = "Replace animated spinners and the typewriter reveal with their finished state. Set N00N_REDUCED_MOTION to override the file value; N00N_REDUCED_MOTION=0 forces motion back on"
    )]
    pub reduced_motion: bool,

    #[config(
        default = true,
        desc = "Show the n00n mascot on the idle splash screen"
    )]
    pub mascot: bool,

    #[config(default = true, desc = "Show vertical scrollbar in scrollable areas")]
    pub scrollbar: bool,

    #[config(default = DEFAULT_FLASH_DURATION_MS, desc = "Duration of flash messages (ms)")]
    pub flash_duration_ms: u64,

    #[config(default = DEFAULT_TYPEWRITER_MS_PER_CHAR, desc = "Typewriter effect speed (ms/char)")]
    pub typewriter_ms_per_char: u64,

    #[config(default = DEFAULT_MOUSE_SCROLL_LINES, min = MIN_MOUSE_SCROLL_LINES, desc = "Lines per mouse wheel scroll")]
    pub mouse_scroll_lines: u32,

    #[config(default = DEFAULT_MAX_INPUT_LINES, min = MIN_MAX_INPUT_LINES, desc = "Maximum visible input lines")]
    pub max_input_lines: u32,

    #[config(
        default = true,
        desc = "When true (default), show full model reasoning live and persisted. When false, hide reasoning behind an indicator (thinking> ...) with a click-to-expand hint, both while thinking and after it completes"
    )]
    pub show_thinking: bool,

    #[config(
        default = "UiNotifications::default()",
        ty = "off | bell | osc9 | all",
        default_doc = "\"bell\"",
        desc = "Attention signal when a turn ends or input is required while the terminal is unfocused. \"bell\" rings the terminal bell, \"osc9\" emits a desktop-notification escape, \"all\" emits both. Set N00N_NOTIFICATIONS to override the file value"
    )]
    pub notifications: UiNotifications,

    #[config(
        default = true,
        desc = "Set the terminal window title to the focused session title and state (OSC 2); the previous title is restored on exit where the terminal supports the title stack"
    )]
    pub terminal_title: bool,

    #[config(skip, default = "None")]
    pub theme: Option<String>,

    #[config(skip, default = "ToolOutputLines::default()")]
    pub tool_output_lines: ToolOutputLines,
}

/// Name of the environment variable that overrides `ui.reduced_motion`.
pub const REDUCED_MOTION_ENV: &str = "N00N_REDUCED_MOTION";

/// Name of the environment variable that overrides `ui.notifications`.
pub const NOTIFICATIONS_ENV: &str = "N00N_NOTIFICATIONS";

/// `Some` when the environment gives a definite mode, `None` to fall
/// through to the file config. Invalid values are rejected loudly and fall
/// through rather than silently disabling notifications.
fn notifications_from_env(
    get: impl Fn(&str) -> Result<String, std::env::VarError>,
) -> Option<UiNotifications> {
    match get(NOTIFICATIONS_ENV) {
        Ok(value) => match value.parse() {
            Ok(mode) => Some(mode),
            Err(error) => {
                warn!(variable = NOTIFICATIONS_ENV, %error, "ignoring environment override");
                None
            }
        },
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            warn!(
                variable = NOTIFICATIONS_ENV,
                "ignoring environment override that is not valid UTF-8"
            );
            None
        }
    }
}

/// `Some` when the environment gives a definite answer, `None` to fall through
/// to the file config.
///
/// Any value other than `0` turns reduced motion on, matching how
/// `N00N_TRUECOLOR` is read in `n00n-ui`.
fn reduced_motion_from_env(
    get: impl Fn(&str) -> Result<String, std::env::VarError>,
) -> Option<bool> {
    match get(REDUCED_MOTION_ENV) {
        Ok(value) => Some(value != "0"),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            warn!(
                variable = REDUCED_MOTION_ENV,
                "ignoring environment override that is not valid UTF-8"
            );
            None
        }
    }
}

impl UiConfig {
    #[must_use]
    pub fn flash_duration(&self) -> Duration {
        Duration::from_millis(self.flash_duration_ms)
    }

    fn from_file(f: UiFileConfig) -> Self {
        Self::from_file_with_env(f, |var| std::env::var(var))
    }

    fn from_file_with_env(
        f: UiFileConfig,
        get_env: impl Fn(&str) -> Result<String, std::env::VarError>,
    ) -> Self {
        Self {
            splash_animation: f.splash_animation.is_none_or(|v| v),
            reduced_motion: reduced_motion_from_env(&get_env)
                .unwrap_or_else(|| f.reduced_motion.is_some_and(|v| v)),
            mascot: f.mascot.is_none_or(|v| v),
            scrollbar: f.scrollbar.is_none_or(|v| v),
            flash_duration_ms: f
                .flash_duration_ms
                .unwrap_or_else(|| DEFAULT_FLASH_DURATION_MS),
            typewriter_ms_per_char: f
                .typewriter_ms_per_char
                .unwrap_or_else(|| DEFAULT_TYPEWRITER_MS_PER_CHAR),
            mouse_scroll_lines: f
                .mouse_scroll_lines
                .unwrap_or_else(|| DEFAULT_MOUSE_SCROLL_LINES),
            max_input_lines: f.max_input_lines.unwrap_or_else(|| DEFAULT_MAX_INPUT_LINES),
            show_thinking: f.show_thinking.unwrap_or_else(|| true),
            notifications: notifications_from_env(&get_env)
                .unwrap_or_else(|| f.notifications.unwrap_or_else(UiNotifications::default)),
            terminal_title: f.terminal_title.is_none_or(|v| v),
            theme: f.theme,
            tool_output_lines: ToolOutputLines::from_file(f.tool_output_lines),
        }
    }

    /// Validate this UI config and its nested `ToolOutputLines`.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError::BelowMinimum` if any configured numeric value is
    /// below its allowed minimum.
    pub fn validate_all(&self) -> Result<(), ConfigError> {
        self.validate()?;
        self.tool_output_lines.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolOutputLines {
    pub bash: usize,
    pub code_execution: usize,
    pub task: usize,
    pub workflow: usize,
    pub index: usize,
    pub grep: usize,
    pub explore: usize,
    pub read: usize,
    pub write: usize,
    pub web: usize,
    pub other: usize,
}

impl ToolOutputLines {
    pub const DEFAULT: Self = Self {
        bash: 4,
        code_execution: 4,
        task: 4,
        workflow: 5,
        index: 2,
        grep: 2,
        explore: 3,
        read: 2,
        write: 5,
        web: 2,
        other: 2,
    };

    pub const FIELD_DEFAULTS: &[(&'static str, usize)] = &[
        ("bash", Self::DEFAULT.bash),
        ("code_execution", Self::DEFAULT.code_execution),
        ("task", Self::DEFAULT.task),
        ("workflow", Self::DEFAULT.workflow),
        ("index", Self::DEFAULT.index),
        ("grep", Self::DEFAULT.grep),
        ("explore", Self::DEFAULT.explore),
        ("read", Self::DEFAULT.read),
        ("write", Self::DEFAULT.write),
        ("web", Self::DEFAULT.web),
        ("other", Self::DEFAULT.other),
    ];

    fn from_file(f: Option<ToolOutputLinesFile>) -> Self {
        let d = Self::DEFAULT;
        let f = f.unwrap_or_else(ToolOutputLinesFile::default);
        Self {
            bash: f.bash.unwrap_or_else(|| d.bash),
            code_execution: f.code_execution.unwrap_or_else(|| d.code_execution),
            task: f.task.unwrap_or_else(|| d.task),
            workflow: f.workflow.unwrap_or_else(|| d.workflow),
            index: f.index.unwrap_or_else(|| d.index),
            grep: f.grep.unwrap_or_else(|| d.grep),
            explore: f.explore.unwrap_or_else(|| d.explore),
            read: f.read.unwrap_or_else(|| d.read),
            write: f.write.unwrap_or_else(|| d.write),
            web: f.web.unwrap_or_else(|| d.web),
            other: f.other.unwrap_or_else(|| d.other),
        }
    }

    fn fields(&self) -> [(&'static str, usize); 11] {
        [
            ("bash", self.bash),
            ("code_execution", self.code_execution),
            ("task", self.task),
            ("workflow", self.workflow),
            ("index", self.index),
            ("grep", self.grep),
            ("explore", self.explore),
            ("read", self.read),
            ("write", self.write),
            ("web", self.web),
            ("other", self.other),
        ]
    }
    /// Validate all tool output line counts are above their minimum.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError::BelowMinimum` if any configured line count is too
    /// small.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (name, value) in self.fields() {
            check(
                "ui.tool_output_lines",
                name,
                value as u64,
                MIN_TOOL_OUTPUT_LINES as u64,
            )?;
        }
        Ok(())
    }

    #[must_use]
    pub fn get(&self, name: &str) -> usize {
        // Tool names arrive in canonical form from the runtime, so map them
        // back to the legacy buckets this structure keys on.
        let name = match canonical_tool_name(name) {
            "run_shell" => "bash",
            "run_python" => "code_execution",
            "run_task" => "task",
            "run_workflow" => "workflow",
            "index_file" => "index",
            "search_code" | "search_files" | "search_text" => "grep",
            "map_codegraph" | "explore_code" => "explore",
            "read_file" => "read",
            "use_memory" => "memory",
            "fetch_url" | "search_web" => "web",
            other => other,
        };
        match name {
            "bash" => self.bash,
            "code_execution" => self.code_execution,
            "task" => self.task,
            "workflow" => self.workflow,
            "index" => self.index,
            "grep" | "glob" => self.grep,
            "codegraph" | "explore" => self.explore,
            "read" => self.read,
            "memory" => self.write,
            name if FILE_WRITE_TOOLS.contains(&name) => self.write,
            "webfetch" | "websearch" => self.web,
            _ => self.other,
        }
    }
}

impl Default for ToolOutputLines {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Debug, Clone, ConfigSection, Serialize)]
#[config(section = "agent")]
pub struct AgentConfig {
    #[config(default = DEFAULT_MAX_OUTPUT_BYTES, min = MIN_OUTPUT_BYTES, desc = "Max tool output size (bytes)")]
    pub max_output_bytes: usize,

    #[config(default = DEFAULT_MAX_OUTPUT_LINES, min = MIN_OUTPUT_LINES, desc = "Max tool output lines")]
    pub max_output_lines: usize,

    #[config(default = DEFAULT_MAX_CONTINUATION_TURNS, min = MIN_MAX_CONTINUATION_TURNS, desc = "Max automatic continuation turns")]
    pub max_continuation_turns: u32,

    #[config(default = DEFAULT_MAX_DEPTH, min = MIN_MAX_DEPTH, desc = "Maximum session lineage depth")]
    pub max_depth: usize,

    #[config(default = DEFAULT_MAX_TOTAL_DESCENDANTS, min = MIN_MAX_TOTAL_DESCENDANTS, desc = "Maximum total descendants per session lineage root")]
    pub max_total_descendants: usize,

    #[config(default = DEFAULT_MAX_ACTIVE_DESCENDANTS, min = MIN_MAX_ACTIVE_DESCENDANTS, desc = "Maximum active descendants per session lineage root")]
    pub max_active_descendants: usize,

    #[config(default = DEFAULT_COMPACTION_BUFFER, ty = "u32 | string", default_doc = "20%", desc = "Context reserved for compaction: token count or percent of the context window (e.g. \"20%\")")]
    pub compaction_buffer: CompactionBuffer,
    #[config(default = DEFAULT_MCP_TOOL_DESC_MAX_CHARS, min = 10, desc = "Max MCP tool description length (characters)")]
    pub mcp_tool_desc_max_chars: usize,

    #[config(default = DEFAULT_SESSION_ROUNDTRIP_TIMEOUT_SECS, min = MIN_SESSION_ROUNDTRIP_TIMEOUT_SECS, desc = "TUI session roundtrip timeout (seconds)")]
    pub session_roundtrip_timeout_secs: u64,

    #[config(skip, default = false)]
    pub no_rtk: bool,

    #[config(skip, default = "None")]
    pub max_turns: Option<u32>,

    #[config(skip, default = "Vec::new()")]
    pub allowed_tools: Vec<String>,

    #[config(skip, default = "Vec::new()")]
    pub disabled_tools: Vec<String>,

    #[config(skip, default = "DynamicToolConfig::default()")]
    pub dynamic_tools: DynamicToolConfig,

    #[config(skip, default = "FusionConfig::default()")]
    pub fusion: FusionConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FusionConfig {
    pub enabled: bool,
    pub lead_model: String,
    pub sidekick_model: String,
    pub sidekick_thinking: String,
    pub sidekick_tier: crate::providers::Tier,
}

impl Default for FusionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            lead_model: DEFAULT_FUSION_LEAD_MODEL.to_owned(),
            sidekick_model: DEFAULT_FUSION_SIDEKICK_MODEL.to_owned(),
            sidekick_thinking: DEFAULT_FUSION_SIDEKICK_THINKING.to_owned(),
            sidekick_tier: crate::providers::Tier::Weak,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ConfigSection)]
#[config(section = "dynamic_tools", fields_only)]
pub struct DynamicToolConfig {
    #[config(
        ty = "bool",
        default = "false",
        desc = "Enable mode-based dynamic tool loading"
    )]
    pub enabled: bool,

    #[config(
        ty = "String",
        default = "\"default\"",
        desc = "Default mode for tool filtering (e.g. \"default\", \"research\", \"build\")"
    )]
    pub default_mode: String,
}

impl Default for DynamicToolConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_mode: "default".to_string(),
        }
    }
}

impl AgentConfig {
    fn from_file(file: AgentFileConfig, no_rtk: bool, disabled_tools: Vec<String>) -> Self {
        let dynamic_tools = if let Some(dt) = file.dynamic_tools {
            DynamicToolConfig {
                enabled: dt.enabled.unwrap_or_else(|| false),
                default_mode: dt.default_mode.unwrap_or_else(|| "default".to_string()),
            }
        } else {
            DynamicToolConfig::default()
        };

        let fusion = if let Some(ff) = file.fusion {
            FusionConfig {
                enabled: ff.enabled.unwrap_or_else(|| false),
                lead_model: ff
                    .lead_model
                    .unwrap_or_else(|| DEFAULT_FUSION_LEAD_MODEL.to_owned()),
                sidekick_model: ff
                    .sidekick_model
                    .unwrap_or_else(|| DEFAULT_FUSION_SIDEKICK_MODEL.to_owned()),
                sidekick_thinking: ff
                    .sidekick_thinking
                    .unwrap_or_else(|| DEFAULT_FUSION_SIDEKICK_THINKING.to_owned()),
                sidekick_tier: ff
                    .sidekick_tier
                    .unwrap_or_else(|| crate::providers::Tier::Weak),
            }
        } else {
            FusionConfig::default()
        };

        Self {
            no_rtk,
            max_output_bytes: file
                .max_output_bytes
                .unwrap_or_else(|| DEFAULT_MAX_OUTPUT_BYTES),
            max_output_lines: file
                .max_output_lines
                .unwrap_or_else(|| DEFAULT_MAX_OUTPUT_LINES),
            max_continuation_turns: file
                .max_continuation_turns
                .unwrap_or_else(|| DEFAULT_MAX_CONTINUATION_TURNS),
            max_depth: file.max_depth.unwrap_or_else(|| DEFAULT_MAX_DEPTH),
            max_total_descendants: file
                .max_total_descendants
                .unwrap_or_else(|| DEFAULT_MAX_TOTAL_DESCENDANTS),
            max_active_descendants: file
                .max_active_descendants
                .unwrap_or_else(|| DEFAULT_MAX_ACTIVE_DESCENDANTS),
            compaction_buffer: file
                .compaction_buffer
                .unwrap_or_else(|| DEFAULT_COMPACTION_BUFFER),
            mcp_tool_desc_max_chars: file
                .mcp_tool_desc_max_chars
                .unwrap_or_else(|| DEFAULT_MCP_TOOL_DESC_MAX_CHARS),
            session_roundtrip_timeout_secs: file
                .session_roundtrip_timeout_secs
                .unwrap_or_else(|| DEFAULT_SESSION_ROUNDTRIP_TIMEOUT_SECS),
            max_turns: None,
            allowed_tools: Vec::new(),
            disabled_tools,
            dynamic_tools,
            fusion,
        }
    }

    fn validate_lineage_limits(&self) -> Result<(), ConfigError> {
        if self.max_depth > self.max_total_descendants
            || self.max_active_descendants > self.max_total_descendants
        {
            return Err(ConfigError::InvalidLineageLimits {
                max_depth: self.max_depth,
                max_total_descendants: self.max_total_descendants,
                max_active_descendants: self.max_active_descendants,
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn session_roundtrip_timeout(&self) -> Duration {
        Duration::from_secs(self.session_roundtrip_timeout_secs)
    }
}

#[derive(Debug, Clone, ConfigSection)]
#[config(section = "provider", fields_only)]
pub struct ProviderConfig {
    #[config(
        ty = "String",
        desc = "Default model identifier (e.g. `anthropic/claude-sonnet-4-6`)"
    )]
    pub default_model: Option<String>,

    #[config(key = "connect_timeout_secs", ty = "u64", default = DEFAULT_CONNECT_TIMEOUT_SECS,
             min = MIN_CONNECT_TIMEOUT_SECS, val = "self.connect_timeout.as_secs()",
             desc = "HTTP connect timeout (seconds)")]
    pub connect_timeout: Duration,

    #[config(key = "low_speed_timeout_secs", ty = "u64", default = DEFAULT_LOW_SPEED_TIMEOUT_SECS,
             min = MIN_LOW_SPEED_TIMEOUT_SECS, val = "self.low_speed_timeout.as_secs()",
             desc = "Low speed timeout (seconds with less than 1 byte received)")]
    pub low_speed_timeout: Duration,

    #[config(key = "stream_timeout_secs", ty = "u64", default = DEFAULT_STREAM_TIMEOUT_SECS,
             min = MIN_STREAM_TIMEOUT_SECS, val = "self.stream_timeout.as_secs()",
             desc = "Streaming response timeout (seconds)")]
    pub stream_timeout: Duration,

    #[config(key = "openai_coding_plan_slots", ty = "u64", default = DEFAULT_OPENAI_CODING_PLAN_SLOTS,
             min = 1, desc = "Maximum concurrent OpenAI Coding Plan streams per account (1-8)")]
    pub openai_coding_plan_slots: u64,

    #[config(key = "openai_codex_accepts_prompt_cache_options_implicit", ty = "bool", default = DEFAULT_OPENAI_CODEX_ACCEPTS_PROMPT_CACHE_OPTIONS_IMPLICIT,
             desc = "Experimental: allow Codex implicit prompt_cache_options only after independently verifying endpoint support")]
    pub openai_codex_accepts_prompt_cache_options_implicit: bool,
    #[config(key = "openai_codex_accepts_prompt_cache_options_explicit", ty = "bool", default = DEFAULT_OPENAI_CODEX_ACCEPTS_PROMPT_CACHE_OPTIONS_EXPLICIT,
             desc = "Experimental: allow Codex explicit prompt_cache_options only after independently verifying endpoint support")]
    pub openai_codex_accepts_prompt_cache_options_explicit: bool,

    #[config(key = "openai_codex_accepts_prompt_cache_breakpoints", ty = "bool", default = DEFAULT_OPENAI_CODEX_ACCEPTS_PROMPT_CACHE_BREAKPOINTS,
             desc = "Experimental: allow Codex cache breakpoints only after independently verifying endpoint support")]
    pub openai_codex_accepts_prompt_cache_breakpoints: bool,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            default_model: None,
            connect_timeout: Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
            low_speed_timeout: Duration::from_secs(DEFAULT_LOW_SPEED_TIMEOUT_SECS),
            stream_timeout: Duration::from_secs(DEFAULT_STREAM_TIMEOUT_SECS),
            openai_coding_plan_slots: DEFAULT_OPENAI_CODING_PLAN_SLOTS,
            openai_codex_accepts_prompt_cache_options_implicit:
                DEFAULT_OPENAI_CODEX_ACCEPTS_PROMPT_CACHE_OPTIONS_IMPLICIT,
            openai_codex_accepts_prompt_cache_options_explicit:
                DEFAULT_OPENAI_CODEX_ACCEPTS_PROMPT_CACHE_OPTIONS_EXPLICIT,
            openai_codex_accepts_prompt_cache_breakpoints:
                DEFAULT_OPENAI_CODEX_ACCEPTS_PROMPT_CACHE_BREAKPOINTS,
        }
    }
}

impl ProviderConfig {
    fn from_file(f: ProviderFileConfig) -> Self {
        Self {
            default_model: f.default_model,
            connect_timeout: Duration::from_secs(
                f.connect_timeout_secs
                    .unwrap_or_else(|| DEFAULT_CONNECT_TIMEOUT_SECS),
            ),
            low_speed_timeout: Duration::from_secs(
                f.low_speed_timeout_secs
                    .unwrap_or_else(|| DEFAULT_LOW_SPEED_TIMEOUT_SECS),
            ),
            stream_timeout: Duration::from_secs(
                f.stream_timeout_secs
                    .unwrap_or_else(|| DEFAULT_STREAM_TIMEOUT_SECS),
            ),
            openai_coding_plan_slots: f
                .openai_coding_plan_slots
                .unwrap_or_else(|| DEFAULT_OPENAI_CODING_PLAN_SLOTS),
            openai_codex_accepts_prompt_cache_options_implicit: f
                .openai_codex_accepts_prompt_cache_options_implicit
                .unwrap_or_else(|| DEFAULT_OPENAI_CODEX_ACCEPTS_PROMPT_CACHE_OPTIONS_IMPLICIT),
            openai_codex_accepts_prompt_cache_options_explicit: f
                .openai_codex_accepts_prompt_cache_options_explicit
                .unwrap_or_else(|| DEFAULT_OPENAI_CODEX_ACCEPTS_PROMPT_CACHE_OPTIONS_EXPLICIT),
            openai_codex_accepts_prompt_cache_breakpoints: f
                .openai_codex_accepts_prompt_cache_breakpoints
                .unwrap_or_else(|| DEFAULT_OPENAI_CODEX_ACCEPTS_PROMPT_CACHE_BREAKPOINTS),
        }
    }

    fn validate_openai_coding_plan_slots(&self) -> Result<(), ConfigError> {
        if self.openai_coding_plan_slots > MAX_OPENAI_CODING_PLAN_SLOTS {
            return Err(ConfigError::AboveMaximum {
                section: "provider",
                field: "openai_coding_plan_slots",
                value: self.openai_coding_plan_slots,
                max: MAX_OPENAI_CODING_PLAN_SLOTS,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchConfig {
    enabled: bool,
}

impl SearchConfig {
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    const fn from_file(file: &SearchFileConfig) -> Self {
        Self {
            enabled: matches!(file.enabled, Some(true)),
        }
    }
}

#[derive(Debug, Clone, Copy, ConfigSection)]
#[config(section = "storage", fields_only)]
pub struct StorageConfig {
    #[config(key = "max_log_bytes_mb", ty = "u64", default = DEFAULT_MAX_LOG_BYTES_MB,
             min = MIN_MAX_LOG_BYTES_MB, val = "self.max_log_bytes / (1024 * 1024)",
             desc = "Max total log size (MB)")]
    pub max_log_bytes: u64,

    #[config(default = DEFAULT_MAX_LOG_FILES, min = MIN_MAX_LOG_FILES,
             desc = "Max number of log files to keep")]
    pub max_log_files: u32,

    #[config(default = DEFAULT_INPUT_HISTORY_SIZE, min = MIN_INPUT_HISTORY_SIZE,
             desc = "Number of input history entries to retain")]
    pub input_history_size: usize,

    #[config(default = DEFAULT_MAX_RETAINED_TOOL_OUTPUTS, min = MIN_MAX_RETAINED_TOOL_OUTPUTS,
             desc = "Tool outputs a live session keeps in memory; older ones are read back from the session log on demand")]
    pub max_retained_tool_outputs: usize,

    #[config(default = DEFAULT_MAX_RETAINED_SUBAGENT_HISTORIES, min = MIN_MAX_RETAINED_SUBAGENT_HISTORIES,
             desc = "Subagent histories a live session keeps in memory; older ones are read back from the session log on demand")]
    pub max_retained_subagent_histories: usize,

    #[config(key = "snapshot_timeout_secs", ty = "u64", default = DEFAULT_SNAPSHOT_TIMEOUT_SECS,
             min = MIN_SNAPSHOT_TIMEOUT_SECS, val = "self.snapshot_timeout.as_secs()",
             desc = "Storage snapshot timeout (seconds)")]
    pub snapshot_timeout: Duration,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            max_log_bytes: DEFAULT_MAX_LOG_BYTES_MB * 1024 * 1024,
            max_log_files: DEFAULT_MAX_LOG_FILES,
            input_history_size: DEFAULT_INPUT_HISTORY_SIZE,
            max_retained_tool_outputs: DEFAULT_MAX_RETAINED_TOOL_OUTPUTS,
            max_retained_subagent_histories: DEFAULT_MAX_RETAINED_SUBAGENT_HISTORIES,
            snapshot_timeout: Duration::from_secs(DEFAULT_SNAPSHOT_TIMEOUT_SECS),
        }
    }
}

impl StorageConfig {
    fn from_file(f: &StorageFileConfig) -> Self {
        Self {
            max_log_bytes: f
                .max_log_bytes_mb
                .unwrap_or_else(|| DEFAULT_MAX_LOG_BYTES_MB)
                * 1024
                * 1024,
            max_log_files: f.max_log_files.unwrap_or_else(|| DEFAULT_MAX_LOG_FILES),
            input_history_size: f
                .input_history_size
                .unwrap_or_else(|| DEFAULT_INPUT_HISTORY_SIZE),
            max_retained_tool_outputs: f
                .max_retained_tool_outputs
                .unwrap_or_else(|| DEFAULT_MAX_RETAINED_TOOL_OUTPUTS),
            max_retained_subagent_histories: f
                .max_retained_subagent_histories
                .unwrap_or_else(|| DEFAULT_MAX_RETAINED_SUBAGENT_HISTORIES),
            snapshot_timeout: Duration::from_secs(
                f.snapshot_timeout_secs
                    .unwrap_or_else(|| DEFAULT_SNAPSHOT_TIMEOUT_SECS),
            ),
        }
    }

    /// The in-memory ceiling a live session enforces after each durable write.
    #[must_use]
    pub fn retention_budget(&self) -> RetentionBudget {
        RetentionBudget {
            tool_outputs: self.max_retained_tool_outputs,
            subagent_histories: self.max_retained_subagent_histories,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PluginsConfig {
    pub enabled: bool,
    pub names: Vec<String>,
    /// Per-plugin option tables, without `enabled`. Each plugin validates its
    /// own via `n00n.api.register_options` at load time.
    pub opts: HashMap<String, JsonMap<String, JsonValue>>,
}

impl PluginsConfig {
    #[must_use]
    pub fn from_plugins(plugins: &HashMap<String, PluginFileConfig>) -> Self {
        let mut all: Vec<String> = DEFAULT_BUILTINS
            .iter()
            .filter(|name| {
                plugins
                    .get(**name)
                    .and_then(|t| t.enabled)
                    .is_none_or(|v| v)
            })
            .map(std::string::ToString::to_string)
            .collect();

        let mut extra: Vec<&String> = plugins
            .iter()
            .filter(|(name, cfg)| {
                !DEFAULT_BUILTINS.contains(&name.as_str()) && cfg.enabled.is_some_and(|v| v)
            })
            .map(|(name, _)| name)
            .collect();
        extra.sort();
        all.extend(extra.into_iter().cloned());

        let opts = plugins
            .iter()
            .filter(|(_, cfg)| !cfg.opts.is_empty())
            .map(|(name, cfg)| (name.clone(), cfg.opts.clone()))
            .collect();

        Self {
            enabled: true,
            names: all,
            opts,
        }
    }
}

impl Config {
    /// Validate the full configuration, including all nested subconfigs.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError` if any subconfig value is below its allowed
    /// minimum.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.ui.validate_all()?;
        self.agent.validate()?;
        self.agent.validate_lineage_limits()?;
        self.provider.validate()?;
        self.provider.validate_openai_coding_plan_slots()?;
        if self.agent.fusion.sidekick_tier == crate::providers::Tier::Compaction {
            return Err(ConfigError::InvalidFusionSidekickTier {
                tier: self.agent.fusion.sidekick_tier,
            });
        }
        StoredThinking::parse_setting(&self.agent.fusion.sidekick_thinking)
            .map_err(ConfigError::InvalidFusionSidekickThinking)?;
        self.storage.validate()?;
        Ok(())
    }
}

fn push_rules(
    rules: &mut Vec<PermissionRule>,
    tools: &HashMap<String, ToolPermissions>,
    effect: Effect,
) {
    for (tool, perms) in tools {
        let scope_set = match effect {
            Effect::Deny => &perms.deny,
            Effect::Allow => &perms.allow,
        };
        let Some(scope_set) = scope_set else {
            continue;
        };
        match scope_set {
            ScopeSet::All(true) => rules.push(PermissionRule {
                tool: ToolKey::native(tool),
                scope: None,
                effect,
            }),
            ScopeSet::Scopes(scopes) => {
                for s in scopes {
                    rules.push(PermissionRule {
                        tool: ToolKey::native(tool),
                        scope: Some(s.clone()),
                        effect,
                    });
                }
            }
            ScopeSet::All(false) => {}
        }
    }
}

#[must_use]
pub fn is_valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Validates the *tool* portion of an MCP qualified name.
/// Currently identical to `is_valid_wire_name`, but kept distinct
/// in case MCP tools need different constraints from native wire names.
fn is_valid_tool_name(name: &str) -> bool {
    is_valid_wire_name(name)
}

fn push_mcp_tool_rule(
    rules: &mut Vec<PermissionRule>,
    server_name: &str,
    tool_name: &str,
    effect: Effect,
) {
    let qualified = format!("{server_name}.{tool_name}");
    match ToolKey::parse(&qualified) {
        Ok(key) => {
            rules.push(PermissionRule {
                tool: key,
                scope: None,
                effect,
            });
        }
        Err(e) => {
            tracing::warn!(
                server = server_name,
                tool = tool_name,
                error = %e,
                "skipping invalid MCP tool name"
            );
        }
    }
}

fn apply_mcp_effect(
    server_name: &str,
    key: &str,
    value: &toml::Value,
    rules: &mut Vec<PermissionRule>,
) {
    let effect = if key == "allow" {
        Effect::Allow
    } else {
        Effect::Deny
    };
    match value {
        toml::Value::Array(arr) => {
            for item in arr {
                if let Some(tool_name) = item.as_str() {
                    if tool_name == "*" {
                        // `allow = ["*"]` / `deny = ["*"]` means server-wide.
                        // Create an McpServer rule so deny-wins logic applies:
                        // McpServer deny blocks all tools on the server.
                        // No allow can override a deny — any deny wins.
                        rules.push(PermissionRule {
                            tool: ToolKey::McpServer {
                                server: server_name.into(),
                            },
                            scope: None,
                            effect,
                        });
                        continue;
                    }
                    push_mcp_tool_rule(rules, server_name, tool_name, effect);
                }
            }
        }
        toml::Value::Boolean(true) => {
            tracing::warn!(
                server = server_name,
                key,
                "{key} = true is deprecated — use default = \"{key}\" instead; ignoring"
            );
        }
        toml::Value::Boolean(false) => {
            // No-op: explicitly disabled.
        }
        toml::Value::String(s) => {
            let tool_name = s.as_str();
            if tool_name == "*" {
                // Treat `allow = "*"` the same as `allow = ["*"]` —
                // create a hard McpServer rule, not a default.
                rules.push(PermissionRule {
                    tool: ToolKey::McpServer {
                        server: server_name.into(),
                    },
                    scope: None,
                    effect,
                });
            } else {
                tracing::info!(
                    server = server_name,
                    tool = tool_name,
                    "{key} = \"{tool_name}\" coerced to {key} = [\"{tool_name}\"] — \
                     consider using array syntax"
                );
                push_mcp_tool_rule(rules, server_name, tool_name, effect);
            }
        }
        other => {
            tracing::warn!(
                server = server_name,
                key,
                value = ?other,
                "unexpected value for [mcp.{server_name}].{key} — \
                 expected array of tool names or default = \"allow\"/\"deny\""
            );
        }
    }
}

fn child_table<'a>(
    table: &'a mut toml_edit::Table,
    key: &str,
) -> Result<&'a mut toml_edit::Table, String> {
    table
        .entry(key)
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| format!("[{key}] is not a table"))
}

fn push_unique(table: &mut toml_edit::Table, key: &str, value: &str) -> Result<(), String> {
    let arr = table
        .entry(key)
        .or_insert_with(|| toml_edit::Item::Value(toml_edit::Value::Array(toml_edit::Array::new())))
        .as_array_mut()
        .ok_or_else(|| format!("{key} is not an array"))?;
    if !arr.iter().any(|v| v.as_str() == Some(value)) {
        arr.push(value);
        arr.set_trailing("\n");
        arr.set_trailing_comma(true);
        for item in arr.iter_mut() {
            item.decor_mut().set_prefix("\n    ");
        }
    }
    Ok(())
}
fn parse_mcp_server_table(
    server_name: &str,
    table: &toml::Table,
    rules: &mut Vec<PermissionRule>,
    mcp_defaults: &mut HashMap<ToolKey, DefaultEffect>,
) {
    if !is_valid_server_name(server_name) {
        tracing::warn!(
            server = server_name,
            "skipping [mcp.{server_name}] — invalid server name; \
             must contain only alphanumeric characters and hyphens"
        );
        return;
    }

    for (key, value) in table {
        match key.as_str() {
            "allow" | "deny" => {
                apply_mcp_effect(server_name, key.as_str(), value, rules);
            }
            "default" => {
                if let Ok(d) = value.clone().try_into::<DefaultEffect>() {
                    mcp_defaults.insert(
                        ToolKey::McpServer {
                            server: server_name.into(),
                        },
                        d,
                    );
                } else {
                    tracing::warn!(
                        server = server_name,
                        value = ?value,
                        "invalid [mcp.{server_name}].default value — expected \"allow\", \"deny\", or \"prompt\""
                    );
                }
            }
            other => {
                if value.is_table() {
                    tracing::warn!(
                        server = server_name,
                        key = other,
                        "unknown key [mcp.{server_name}.{other}] — server names cannot \
                         contain dots; use [mcp.{other}] instead if this is a server name"
                    );
                } else {
                    tracing::warn!(
                        server = server_name,
                        key = other,
                        "unknown key in [mcp.{server_name}] — ignored"
                    );
                }
            }
        }
    }
}

fn build_permissions(
    global: &PermissionsFileConfig,
    project: &PermissionsFileConfig,
) -> PermissionsConfig {
    let global_default = global.default.unwrap_or_else(|| DefaultEffect::Prompt);
    let default = if let Some(d) = project.default
        && d != DefaultEffect::Allow
    {
        d
    } else {
        global_default
    };

    let mut tool_defaults = HashMap::new();
    for (tool, perms) in &global.tools {
        if let Some(d) = perms.default {
            let key = ToolKey::native(tool);
            if matches!(key, ToolKey::Wildcard) {
                tracing::warn!(
                    tool = tool,
                    "ignoring [\"*\"].default — use the top-level `default` field instead \
                     for global fallback behavior"
                );
            } else {
                tool_defaults.insert(key, d);
            }
        }
    }
    for (key, d) in &global.mcp_defaults {
        tool_defaults.insert(key.clone(), *d);
    }
    for (tool, perms) in &project.tools {
        if let Some(d) = perms.default
            && d != DefaultEffect::Allow
        {
            let key = ToolKey::native(tool);
            if matches!(key, ToolKey::Wildcard) {
                tracing::warn!(
                    tool = tool,
                    "ignoring project [\"*\"].default — use the top-level `default` field instead"
                );
            } else {
                tool_defaults.insert(key, d);
            }
        }
    }
    for (key, d) in &project.mcp_defaults {
        if *d != DefaultEffect::Allow {
            tool_defaults.insert(key.clone(), *d);
        }
    }

    let mut rules = Vec::new();
    for rule in &global.mcp_rules {
        if rule.effect == Effect::Deny {
            rules.push(rule.clone());
        }
    }
    for rule in &global.mcp_rules {
        if rule.effect == Effect::Allow {
            rules.push(rule.clone());
        }
    }
    for tools in [&global.tools, &project.tools] {
        push_rules(&mut rules, tools, Effect::Deny);
        push_rules(&mut rules, tools, Effect::Allow);
    }
    for rule in &project.mcp_rules {
        if rule.effect == Effect::Deny {
            rules.push(rule.clone());
        }
    }
    for rule in &project.mcp_rules {
        if rule.effect == Effect::Allow {
            rules.push(rule.clone());
        }
    }
    PermissionsConfig {
        default,
        tool_defaults,
        rules,
        yolo: false,
    }
}
fn global_dir() -> Option<PathBuf> {
    paths::config_dir().ok()
}

fn config_search_dirs(global: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = global {
        dirs.push(d.to_path_buf());
    }
    if let Ok(xdg) = paths::xdg_config_dir()
        && dirs.first() != Some(&xdg)
    {
        dirs.push(xdg);
    }
    dirs
}

#[allow(unsafe_code)]
fn load_env_files_with_global(cwd: &Path, global: Option<&Path>) {
    let mut vars = HashMap::new();
    if let Some(path) = global {
        collect_env_vars(&path.join(".env"), &mut vars);
    }
    collect_env_vars(&cwd.join(PROJECT_DIR).join(".env"), &mut vars);

    for (key, value) in vars {
        if std::env::var_os(&key).is_none() {
            // SAFETY: single-threaded at startup, before any async runtime
            unsafe { std::env::set_var(&key, &value) };
        }
    }
}

fn collect_env_vars(path: &Path, vars: &mut HashMap<String, String>) {
    let Ok(iter) = dotenvy::from_path_iter(path) else {
        return;
    };
    for item in iter.flatten() {
        vars.insert(item.0, item.1);
    }
}

pub fn load_env_files(cwd: &Path) {
    load_env_files_with_global(cwd, global_dir().as_deref());
}

/// Error message shown when no Bash-compatible runtime is found on Windows.
#[cfg(windows)]
const BASH_NOT_FOUND_ERROR: &str = "bash not found on Windows. Install Git for Windows:\n  \
     winget install --id Git.Git -e --source winget\n  \
     or download from https://git-scm.com/download/win\n\n  \
     Alternatively, enable WSL: \
     https://learn.microsoft.com/en-us/windows/wsl/install";

/// Search PATH and common install paths for a Bash executable.
#[cfg(windows)]
fn find_bash_on_path() -> Option<PathBuf> {
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths).find_map(|dir| {
                let bash = dir.join("bash.exe");
                if bash.is_file() { Some(bash) } else { None }
            })
        })
        .or_else(|| {
            let candidates = [
                // Git for Windows
                r"C:\Program Files\Git\bin\bash.exe",
                r"C:\Program Files\Git\usr\bin\bash.exe",
                r"C:\Program Files (x86)\Git\bin\bash.exe",
                // Cygwin
                r"C:\cygwin64\bin\bash.exe",
                r"C:\cygwin\bin\bash.exe",
                // MSYS2
                r"C:\msys64\usr\bin\bash.exe",
                r"C:\msys32\usr\bin\bash.exe",
            ];
            candidates.iter().find_map(|p| {
                let path = PathBuf::from(p);
                path.is_file().then_some(path)
            })
        })
}

/// Search PATH and System32 for `wsl.exe`.
#[cfg(windows)]
fn find_wsl() -> Option<PathBuf> {
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths).find_map(|dir| {
                let wsl = dir.join("wsl.exe");
                if wsl.is_file() { Some(wsl) } else { None }
            })
        })
        .or_else(|| {
            let path = PathBuf::from(r"C:\Windows\System32\wsl.exe");
            path.is_file().then_some(path)
        })
}

/// Build a `bash -c` command for the given shell string.
///
/// Mirrors Neovim's list-form `jobstart(['bash', '-c', ...])`: the command
/// string is passed as a single argv element, so quoting is preserved by the
/// C runtime / libuv argument parser instead of being reinterpreted by
/// cmd.exe. On Windows, searches PATH and known install locations for Git
/// Bash, Cygwin, MSYS2 and falls back to WSL's `wsl.exe -e bash -c`.
///
/// # Errors
///
/// Returns `Err` on Windows if no bash-compatible shell (Git Bash, Cygwin,
/// MSYS2, or WSL) can be located.
pub fn bash_command(cmd: &str) -> Result<Command, String> {
    #[cfg(unix)]
    {
        let mut c = Command::new("bash");
        c.arg("-c").arg(cmd);
        Ok(c)
    }
    #[cfg(windows)]
    {
        if let Some(bash) = find_bash_on_path() {
            let mut c = Command::new(bash);
            c.arg("-c").arg(cmd);
            return Ok(c);
        }
        if let Some(wsl) = find_wsl() {
            let mut c = Command::new(wsl);
            c.arg("-e").arg("bash").arg("-c").arg(cmd);
            return Ok(c);
        }
        Err(BASH_NOT_FOUND_ERROR.to_string())
    }
}

#[must_use]
pub fn load_permissions(cwd: &Path, project_trusted: bool) -> PermissionsConfig {
    let global_dirs = config_search_dirs(global_dir().as_deref());
    load_permissions_inner(cwd, &global_dirs, project_trusted)
}

fn load_permissions_inner(
    cwd: &Path,
    global_dirs: &[PathBuf],
    project_trusted: bool,
) -> PermissionsConfig {
    let mut global_perms = PermissionsFileConfig::default();
    for dir in global_dirs {
        if let Some(p) = read_permissions_file(&dir.join(PERMISSIONS_FILE)) {
            global_perms = p;
        }
    }

    let project_perms = if project_trusted {
        read_permissions_file(&cwd.join(PROJECT_DIR).join(PERMISSIONS_FILE))
            .unwrap_or_else(PermissionsFileConfig::default)
    } else {
        PermissionsFileConfig::default()
    };

    build_permissions(&global_perms, &project_perms)
}

fn migrate_mcp_entry(
    doc: &mut toml_edit::DocumentMut,
    server_name: &str,
    tool_name: &str,
    item: &toml_edit::Item,
) {
    // Old format: ["mcp:server__tool"] with booleans or scope-string arrays.
    // New format: [mcp.server] allow = ["tool_name"]. Old scope strings were
    // dead code (MCP scopes are always wildcarded), so only the effect survives.
    let mut push = |effect_key: &str| {
        let res = child_table(doc.as_table_mut(), "mcp")
            .and_then(|mcp| child_table(mcp, server_name))
            .and_then(|server| push_unique(server, effect_key, tool_name));
        if let Err(e) = res {
            warn!(
                server = server_name,
                tool = tool_name,
                error = %e,
                "skipping MCP entry migration"
            );
        }
    };

    // Bare boolean: old format like [mcp]\ndeepwiki__search = true
    // means "allow this tool".
    if let Some(b) = item.as_bool() {
        if b {
            push("allow");
        }
        return;
    }

    if let Some(old_table) = item.as_table() {
        for (key, value) in old_table {
            match key {
                "allow" | "deny" => {
                    if value.as_bool() == Some(true) || value.as_array().is_some() {
                        push(key);
                    }
                }
                _ => {
                    warn!(
                        key,
                        server = server_name,
                        tool = tool_name,
                        "dropping unknown key in old MCP entry during migration"
                    );
                }
            }
        }
    }
}

/// Migrates old permission formats and returns the (possibly rewritten)
/// file content. The rewrite to disk is best-effort: loading uses the
/// migrated content even when the write fails.
fn migrate_permissions_file(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let Ok(mut doc) = content.parse::<toml_edit::DocumentMut>() else {
        return Some(content);
    };
    let mut migrated = false;
    if let Some(item) = doc.remove("allow_all") {
        migrated = true;
        if item.as_bool() == Some(true) {
            doc.insert("default", toml_edit::value("allow"));
        }
    }

    // Migrate flat MCP keys: "mcp:server__tool" → [mcp.server]
    // Two TOML representations to handle:
    // 1. Quoted keys: ["mcp:server__tool"] → flat top-level key
    // 2. Bare keys: [mcp:server__tool] → nested "mcp" → {"server__tool": ...}

    // Path 1: Flat quoted keys starting with "mcp:" containing "__"
    let flat_old_keys: Vec<String> = doc
        .iter()
        .filter_map(|(k, _)| {
            k.strip_prefix("mcp:")
                .and_then(|rest| rest.contains("__").then(|| k.to_string()))
        })
        .collect();

    for old_key in flat_old_keys {
        if let Some(item) = doc.remove(&old_key) {
            let rest = &old_key[4..]; // strip "mcp:"
            if let Some((server, tool)) = rest.split_once("__") {
                if !is_valid_server_name(server) || !is_valid_tool_name(tool) {
                    tracing::error!(
                        key = old_key.as_str(),
                        server = server,
                        tool = tool,
                        "SECURITY: skipping migration of malformed MCP key — \
                         rules for this tool will not be restored"
                    );
                    continue;
                }
                migrate_mcp_entry(&mut doc, server, tool, &item);
                migrated = true;
            }
        }
    }

    // Path 2: Nested "mcp" sub-table (bare key mcp: created nesting)
    let nested_old_entries: Vec<(String, String, toml_edit::Item)> = {
        let mut entries = Vec::new();
        if let Some(toml_edit::Item::Table(mcp_table)) = doc.get("mcp") {
            for (key, _) in mcp_table {
                if key.contains("__")
                    && let Some((server, tool)) = key.split_once("__")
                {
                    let item = mcp_table.get(key).cloned();
                    if let Some(item) = item {
                        entries.push((server.to_string(), tool.to_string(), item));
                    }
                }
            }
        }
        entries
    };

    for (server_name, tool_name, item) in nested_old_entries {
        if !is_valid_server_name(&server_name) || !is_valid_tool_name(&tool_name) {
            tracing::error!(
                server = server_name.as_str(),
                tool = &*tool_name,
                "SECURITY: skipping migration of malformed nested MCP key — \
                 rules for this tool will not be restored"
            );
            continue;
        }
        if let Some(toml_edit::Item::Table(mcp_table)) = doc.get_mut("mcp") {
            mcp_table.remove(&format!("{server_name}__{tool_name}"));
        }
        migrate_mcp_entry(&mut doc, &server_name, &tool_name, &item);
        migrated = true;
    }

    // Clean up the now-empty "mcp" parent table if it has no children
    if let Some(toml_edit::Item::Table(mcp_table)) = doc.get("mcp")
        && mcp_table.is_empty()
    {
        doc.remove("mcp");
    }

    if !migrated {
        return Some(content);
    }
    let new_content = doc.to_string();
    if let Err(e) = n00n_storage::atomic_write(path, new_content.as_bytes()) {
        warn!(path = %path.display(), error = %e, "failed to persist migrated permissions file");
    }
    Some(new_content)
}

fn read_permissions_file(path: &Path) -> Option<PermissionsFileConfig> {
    let content = migrate_permissions_file(path)?;
    match toml::from_str(&content) {
        Ok(p) => Some(p),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to parse permissions");
            None
        }
    }
}

#[must_use]
pub fn global_config_dir() -> Option<PathBuf> {
    global_dir()
}

#[must_use]
pub fn global_config_dirs() -> Vec<PathBuf> {
    config_search_dirs(global_dir().as_deref())
}

/// Append a permission rule to the global or project permissions file.
///
/// # Errors
///
/// Returns a `String` error if the home directory cannot be determined or if
/// the permissions file cannot be read, parsed, or written.
pub fn append_permission_rule(
    tool: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
    target: &PermissionTarget,
) -> Result<(), String> {
    let dir = config_search_dirs(global_dir().as_deref())
        .into_iter()
        .last();
    append_permission_rule_with_global(tool, scope, effect, target, dir)
}

fn append_permission_rule_with_global(
    tool: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
    target: &PermissionTarget,
    global: Option<PathBuf>,
) -> Result<(), String> {
    match target {
        PermissionTarget::Global => append_global_permission(tool, scope, effect, global),
        PermissionTarget::Project(cwd) => append_project_permission(tool, scope, effect, cwd),
    }
}

fn append_global_permission(
    tool: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
    global: Option<PathBuf>,
) -> Result<(), String> {
    let path = global
        .ok_or_else(|| "cannot determine home directory".to_string())?
        .join(PERMISSIONS_FILE);
    let content = std::fs::read_to_string(&path).unwrap_or_else(|_| String::new());
    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|e| format!("failed to parse permissions: {e}"))?;

    insert_permission_entry(&mut doc, tool, scope, effect)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create config dir: {e}"))?;
    }
    n00n_storage::atomic_write(&path, doc.to_string().as_bytes())
        .map_err(|e| format!("cannot write permissions: {e}"))?;
    Ok(())
}

fn append_project_permission(
    tool: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
    cwd: &Path,
) -> Result<(), String> {
    let path = cwd.join(PROJECT_DIR).join(PERMISSIONS_FILE);
    let content = std::fs::read_to_string(&path).unwrap_or_else(|_| String::new());
    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|e| format!("failed to parse .n00n/{PERMISSIONS_FILE}: {e}"))?;
    insert_permission_entry(&mut doc, tool, scope, effect)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create .n00n dir: {e}"))?;
    }
    n00n_storage::atomic_write(&path, doc.to_string().as_bytes())
        .map_err(|e| format!("cannot write .n00n/{PERMISSIONS_FILE}: {e}"))?;
    Ok(())
}

fn insert_permission_entry(
    doc: &mut toml_edit::DocumentMut,
    tool_key: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
) -> Result<(), String> {
    let key = match effect {
        Effect::Allow => "allow",
        Effect::Deny => "deny",
    };

    match tool_key {
        // MCP scopes are always wildcarded, so `scope` is ignored for MCP keys.
        ToolKey::McpTool { server, tool } => {
            let server_table = child_table(child_table(doc.as_table_mut(), "mcp")?, server)?;
            push_unique(server_table, key, tool)?;
        }
        ToolKey::McpServer { server } => {
            let server_table = child_table(child_table(doc.as_table_mut(), "mcp")?, server)?;
            server_table.insert("default", toml_edit::value(key));
        }
        ToolKey::Wildcard => {
            // Wildcard rules are config-only; runtime never writes them.
            return Err("cannot write wildcard permission rule to config".to_string());
        }
        ToolKey::Native(name) => {
            let tool_table = child_table(doc.as_table_mut(), name)?;
            match scope {
                Some(s) => push_unique(tool_table, key, s)?,
                None => {
                    tool_table.insert(key, toml_edit::value(true));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Tier;
    use n00n_storage::sessions::Effort;
    use std::fs;
    use tempfile::TempDir;
    use test_case::test_case;

    fn plugin_enabled(enabled: bool) -> PluginFileConfig {
        PluginFileConfig {
            enabled: Some(enabled),
            opts: JsonMap::new(),
        }
    }

    fn write_global_permissions(dir: &Path, content: &str) {
        let perms_dir = dir.join(".config/n00n");
        fs::create_dir_all(&perms_dir).unwrap();
        fs::write(perms_dir.join("permissions.toml"), content).unwrap();
    }

    fn global_config_dir(dir: &Path) -> PathBuf {
        dir.join(".config/n00n")
    }

    #[test]
    fn enabled_search_is_keyless_and_needs_no_endpoint() {
        let raw: RawConfig = toml::from_str("[search]\nenabled = true").unwrap();
        assert!(raw.into_config(false).unwrap().search.enabled());
    }

    #[test]
    fn search_rejects_remote_endpoint_configuration() {
        assert!(
            toml::from_str::<RawConfig>(
                "[search]\nenabled = true\nendpoint = \"https://search.example.com\""
            )
            .is_err()
        );
    }

    #[test]
    fn openai_codex_cache_capabilities_default_off_and_parse_from_file() {
        let default = RawConfig::default().into_config(false).unwrap();
        assert!(
            !default
                .provider
                .openai_codex_accepts_prompt_cache_options_implicit
        );
        assert!(
            !default
                .provider
                .openai_codex_accepts_prompt_cache_options_explicit
        );
        assert!(
            !default
                .provider
                .openai_codex_accepts_prompt_cache_breakpoints
        );
        let config = RawConfig {
            provider: ProviderFileConfig {
                openai_codex_accepts_prompt_cache_options_implicit: Some(true),
                openai_codex_accepts_prompt_cache_options_explicit: Some(true),
                openai_codex_accepts_prompt_cache_breakpoints: Some(true),
                ..Default::default()
            },
            ..Default::default()
        }
        .into_config(false)
        .unwrap();

        assert!(
            config
                .provider
                .openai_codex_accepts_prompt_cache_options_implicit
        );
        assert!(
            config
                .provider
                .openai_codex_accepts_prompt_cache_options_explicit
        );
        assert!(
            config
                .provider
                .openai_codex_accepts_prompt_cache_breakpoints
        );
    }
    #[test_case("12000", CompactionBuffer::Tokens(12_000) ; "tokens_number")]
    #[test_case("\"20%\"", CompactionBuffer::Percent(20) ; "percent_string")]
    #[test_case("\" 5 %\"", CompactionBuffer::Percent(5) ; "percent_with_spaces")]
    fn compaction_buffer_deserializes(json: &str, expected: CompactionBuffer) {
        let parsed: CompactionBuffer = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, expected);
    }

    #[test_case("500" ; "tokens_below_min")]
    #[test_case("-1" ; "negative_tokens")]
    #[test_case("\"0%\"" ; "zero_percent")]
    #[test_case("\"100%\"" ; "percent_too_high")]
    #[test_case("\"abc%\"" ; "non_numeric_percent")]
    fn compaction_buffer_rejects(json: &str) {
        assert!(serde_json::from_str::<CompactionBuffer>(json).is_err());
    }

    #[test_case(CompactionBuffer::Tokens(10_000), 64_000, 10_000 ; "tokens_ignore_window")]
    #[test_case(CompactionBuffer::Percent(20), 64_000, 12_800 ; "percent_of_window")]
    fn compaction_buffer_resolves(buffer: CompactionBuffer, window: u32, expected: u32) {
        assert_eq!(buffer.resolve(window), expected);
    }

    #[test]
    fn compaction_buffer_serializes_percent_as_string() {
        assert_eq!(
            serde_json::to_value(CompactionBuffer::Percent(20)).unwrap(),
            serde_json::json!("20%")
        );
        assert_eq!(
            serde_json::to_value(CompactionBuffer::Tokens(9_000)).unwrap(),
            serde_json::json!(9_000)
        );
    }

    #[test]
    fn openai_coding_plan_slots_default_and_reject_above_eight() {
        let default = RawConfig::default().into_config(false).unwrap();
        assert_eq!(
            default.provider.openai_coding_plan_slots,
            DEFAULT_OPENAI_CODING_PLAN_SLOTS
        );
        let invalid = RawConfig {
            provider: ProviderFileConfig {
                openai_coding_plan_slots: Some(MAX_OPENAI_CODING_PLAN_SLOTS + 1),
                ..Default::default()
            },
            ..Default::default()
        }
        .into_config(false)
        .unwrap();
        assert!(matches!(
            invalid.validate(),
            Err(ConfigError::AboveMaximum { .. })
        ));
    }

    /// A fake environment holding exactly one variable, so these tests never
    /// depend on the ambient environment of whoever runs them. Every other
    /// variable reports `NotPresent`.
    fn env_with<'v>(
        name: &'static str,
        value: Option<&'v str>,
    ) -> impl Fn(&str) -> Result<String, std::env::VarError> + 'v {
        move |var| {
            if var == name {
                value
                    .map(ToOwned::to_owned)
                    .ok_or(std::env::VarError::NotPresent)
            } else {
                Err(std::env::VarError::NotPresent)
            }
        }
    }

    #[test]
    fn reduced_motion_defaults_off_and_reads_the_file_value() {
        let default = UiConfig::from_file_with_env(
            UiFileConfig::default(),
            env_with(REDUCED_MOTION_ENV, None),
        );
        assert!(!default.reduced_motion, "default is full motion");

        let opted_in = UiConfig::from_file_with_env(
            UiFileConfig {
                reduced_motion: Some(true),
                ..Default::default()
            },
            env_with(REDUCED_MOTION_ENV, None),
        );
        assert!(opted_in.reduced_motion, "file value is honoured");
    }

    #[test]
    fn reduced_motion_env_override_beats_the_file() {
        for (raw, want) in [("1", true), ("true", true), ("", true), ("0", false)] {
            for file in [None, Some(true), Some(false)] {
                let ui = UiConfig::from_file_with_env(
                    UiFileConfig {
                        reduced_motion: file,
                        ..Default::default()
                    },
                    env_with(REDUCED_MOTION_ENV, Some(raw)),
                );
                assert_eq!(
                    ui.reduced_motion, want,
                    "{REDUCED_MOTION_ENV}={raw:?} must beat file {file:?}"
                );
            }
        }
    }

    #[test]
    fn reduced_motion_unset_env_falls_through_to_the_file() {
        assert_eq!(
            reduced_motion_from_env(env_with(REDUCED_MOTION_ENV, None)),
            None
        );
    }

    #[test]
    fn reduced_motion_merges_like_the_other_ui_flags() {
        let mut base = UiFileConfig {
            reduced_motion: Some(true),
            ..Default::default()
        };
        base.merge(UiFileConfig::default());
        assert_eq!(base.reduced_motion, Some(true), "base preserved");

        base.merge(UiFileConfig {
            reduced_motion: Some(false),
            ..Default::default()
        });
        assert_eq!(base.reduced_motion, Some(false), "overlay wins");
    }

    #[test]
    fn notifications_default_is_bell_and_reads_the_file_value() {
        let default = UiConfig::from_file_with_env(
            UiFileConfig::default(),
            env_with(NOTIFICATIONS_ENV, None),
        );
        assert_eq!(default.notifications, UiNotifications::Bell);

        let opted_in = UiConfig::from_file_with_env(
            UiFileConfig {
                notifications: Some(UiNotifications::Osc9),
                ..Default::default()
            },
            env_with(NOTIFICATIONS_ENV, None),
        );
        assert_eq!(opted_in.notifications, UiNotifications::Osc9);
    }

    #[test]
    fn notifications_env_override_beats_the_file() {
        for (raw, want) in [
            ("off", UiNotifications::Off),
            ("bell", UiNotifications::Bell),
            ("osc9", UiNotifications::Osc9),
            ("all", UiNotifications::All),
        ] {
            for file in [None, Some(UiNotifications::All)] {
                let ui = UiConfig::from_file_with_env(
                    UiFileConfig {
                        notifications: file,
                        ..Default::default()
                    },
                    env_with(NOTIFICATIONS_ENV, Some(raw)),
                );
                assert_eq!(
                    ui.notifications, want,
                    "{NOTIFICATIONS_ENV}={raw:?} must beat file {file:?}"
                );
            }
        }
    }

    #[test]
    fn notifications_env_invalid_falls_through_to_the_file() {
        let ui = UiConfig::from_file_with_env(
            UiFileConfig {
                notifications: Some(UiNotifications::Off),
                ..Default::default()
            },
            env_with(NOTIFICATIONS_ENV, Some("loud")),
        );
        assert_eq!(
            ui.notifications,
            UiNotifications::Off,
            "invalid env value is ignored, not silently applied"
        );
        assert_eq!(
            notifications_from_env(env_with(NOTIFICATIONS_ENV, None)),
            None
        );
    }

    #[test_case("off", UiNotifications::Off ; "off")]
    #[test_case("bell", UiNotifications::Bell ; "bell")]
    #[test_case("osc9", UiNotifications::Osc9 ; "osc9")]
    #[test_case("all", UiNotifications::All ; "all")]
    fn notifications_deserialize(value: &str, expected: UiNotifications) {
        let raw: RawConfig =
            toml::from_str(&format!("[ui]\nnotifications = \"{value}\"\n")).unwrap();
        assert_eq!(raw.ui.notifications, Some(expected));
    }

    #[test]
    fn notifications_reject_unknown_mode() {
        let result: Result<RawConfig, _> = toml::from_str("[ui]\nnotifications = \"loud\"\n");
        assert!(result.is_err(), "unknown mode should be rejected");
    }

    #[test]
    fn terminal_title_defaults_on_and_deserializes() {
        let config = RawConfig::default().into_config(false).unwrap();
        assert!(config.ui.terminal_title);

        let raw: RawConfig = toml::from_str("[ui]\nterminal_title = false\n").unwrap();
        assert_eq!(raw.ui.terminal_title, Some(false));
    }

    #[test]
    fn notification_fields_merge_like_the_other_ui_flags() {
        let mut base = UiFileConfig {
            notifications: Some(UiNotifications::Off),
            terminal_title: Some(false),
            ..Default::default()
        };
        base.merge(UiFileConfig::default());
        assert_eq!(base.notifications, Some(UiNotifications::Off));
        assert_eq!(base.terminal_title, Some(false));

        base.merge(UiFileConfig {
            notifications: Some(UiNotifications::All),
            terminal_title: Some(true),
            ..Default::default()
        });
        assert_eq!(base.notifications, Some(UiNotifications::All));
        assert_eq!(base.terminal_title, Some(true));
    }

    #[test]
    fn empty_config_returns_defaults() {
        let config = RawConfig::default().into_config(false).unwrap();
        assert!(config.ui.splash_animation);
        assert_eq!(config.agent.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
        assert_eq!(
            config.provider.connect_timeout,
            Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS)
        );
        assert_eq!(
            config.storage.max_log_bytes,
            DEFAULT_MAX_LOG_BYTES_MB * 1024 * 1024
        );
        assert!(!config.always_fusion, "Fusion must be opt-in");
        assert!(
            !config.agent.fusion.enabled,
            "agent Fusion must default off independently"
        );
        assert_eq!(config.agent.fusion.lead_model, DEFAULT_FUSION_LEAD_MODEL);
        assert_eq!(
            config.agent.fusion.sidekick_model,
            DEFAULT_FUSION_SIDEKICK_MODEL
        );
        assert_eq!(
            config.agent.fusion.sidekick_thinking,
            DEFAULT_FUSION_SIDEKICK_THINKING
        );
    }

    #[test]
    fn fusion_opt_ins_parse_independently() {
        let agent_only: RawConfig = toml::from_str("[agent.fusion]\nenabled = true\n").unwrap();
        let agent_only = agent_only.into_config(false).unwrap();
        assert!(agent_only.agent.fusion.enabled);
        assert!(!agent_only.always_fusion);

        let always_only: RawConfig = toml::from_str("always_fusion = true\n").unwrap();
        let always_only = always_only.into_config(false).unwrap();
        assert!(always_only.always_fusion);
        assert!(!always_only.agent.fusion.enabled);
    }

    #[test]
    fn fusion_overlay_merges_fields_without_enabling_by_presence() {
        let mut base: RawConfig =
            toml::from_str("[agent.fusion]\nenabled = false\nsidekick_tier = \"medium\"\n")
                .unwrap();
        let overlay: RawConfig = toml::from_str("[agent.fusion]\nenabled = true\n").unwrap();
        base.merge(overlay);
        let merged = base.into_config(false).unwrap();

        assert!(merged.agent.fusion.enabled);
        assert_eq!(
            merged.agent.fusion.sidekick_tier,
            crate::providers::Tier::Medium
        );
        assert!(!merged.always_fusion);

        let explicit: RawConfig = toml::from_str(
            "[agent.fusion]\nlead_model = \"anthropic/lead\"\nsidekick_model = \"openai/sidekick\"\nsidekick_thinking = \"high\"\n",
        )
        .unwrap();
        let explicit = explicit.into_config(false).unwrap();
        assert_eq!(explicit.agent.fusion.lead_model, "anthropic/lead");
        assert_eq!(explicit.agent.fusion.sidekick_model, "openai/sidekick");
        assert_eq!(explicit.agent.fusion.sidekick_thinking, "high");
    }

    #[test]
    fn fusion_unknown_fields_are_rejected() {
        let error = toml::from_str::<RawConfig>(
            "[agent.fusion]\nenabled = true\nimplicit_model_switch = true\n",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown field `implicit_model_switch`"),
            "unexpected parse error: {error}"
        );
    }

    #[test]
    fn fusion_invalid_sidekick_thinking_is_rejected() {
        let raw: RawConfig =
            toml::from_str("[agent.fusion]\nsidekick_thinking = \"impossible\"\n").unwrap();
        let config = raw.into_config(false).unwrap();

        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidFusionSidekickThinking(_))
        ));
    }

    #[test]
    fn fusion_compaction_sidekick_tier_is_rejected() {
        let raw: RawConfig =
            toml::from_str("[agent.fusion]\nenabled = true\nsidekick_tier = \"compaction\"\n")
                .unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidFusionSidekickTier {
                tier: Tier::Compaction
            })
        ));
    }

    #[test]
    fn partial_agent_config_preserves_unset_fields() {
        let raw = RawConfig {
            agent: AgentFileConfig {
                max_output_lines: Some(5000),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.agent.max_output_lines, 5000);
        assert_eq!(config.agent.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
    }

    #[test]
    fn merge_overlay_wins_field_by_field() {
        let mut base = RawConfig {
            always_yolo: Some(false),
            ui: UiFileConfig {
                splash_animation: Some(false),
                flash_duration_ms: Some(2000),
                ..Default::default()
            },
            agent: AgentFileConfig {
                max_output_lines: Some(3000),
                max_output_bytes: Some(80_000),
                ..Default::default()
            },
            ..Default::default()
        };
        let overlay = RawConfig {
            always_yolo: Some(true),
            agent: AgentFileConfig {
                max_output_lines: Some(5000),
                ..Default::default()
            },
            ..Default::default()
        };
        base.merge(overlay);

        assert_eq!(base.always_yolo, Some(true), "overlay wins");
        assert_eq!(base.agent.max_output_lines, Some(5000), "overlay wins");
        assert_eq!(base.agent.max_output_bytes, Some(80_000), "base preserved");
        assert_eq!(base.ui.splash_animation, Some(false), "base preserved");
        assert_eq!(base.ui.flash_duration_ms, Some(2000), "base preserved");
    }

    #[test]
    fn merge_always_flags_overlay_wins() {
        let mut base = RawConfig {
            always_fast: Some(false),
            always_workflow: Some(false),
            always_thinking: Some(AlwaysThinking::Mode("off".into())),
            ..Default::default()
        };
        let overlay = RawConfig {
            always_fast: Some(true),
            always_workflow: Some(true),
            always_thinking: Some(AlwaysThinking::Toggle(true)),
            ..Default::default()
        };
        base.merge(overlay);

        assert_eq!(base.always_fast, Some(true), "overlay wins");
        assert_eq!(base.always_workflow, Some(true), "overlay wins");
        assert_eq!(
            base.always_thinking,
            Some(AlwaysThinking::Toggle(true)),
            "overlay wins"
        );
    }

    #[test]
    fn always_workflow_resolves_default_and_set() {
        let defaults = RawConfig::default().into_config(false).unwrap();
        assert!(!defaults.always_workflow, "absent resolves to false");

        let raw = RawConfig {
            always_workflow: Some(true),
            ..Default::default()
        };
        assert!(raw.into_config(false).unwrap().always_workflow);
    }

    #[test_case(AlwaysThinking::Toggle(true), StoredThinking::Adaptive ; "toggle_true")]
    #[test_case(AlwaysThinking::Toggle(false), StoredThinking::Off ; "toggle_false")]
    #[test_case(AlwaysThinking::Budget(8192), StoredThinking::Budget { tokens: 8192 } ; "budget_number")]
    #[test_case(AlwaysThinking::Mode("xhigh".into()), StoredThinking::Effort { level: Effort::XHigh } ; "effort_xhigh")]
    #[test_case(AlwaysThinking::Mode("minimal".into()), StoredThinking::Effort { level: Effort::Minimal } ; "effort_minimal")]
    fn always_thinking_toggle_resolve(input: AlwaysThinking, expected: StoredThinking) {
        assert_eq!(input.resolve(), Ok(expected));
    }

    #[test]
    fn into_config_resolves_always_thinking() {
        let defaults = RawConfig::default().into_config(false).unwrap();
        assert!(defaults.always_thinking.is_none());

        let raw = RawConfig {
            always_thinking: Some(AlwaysThinking::Mode("8192".into())),
            ..Default::default()
        };
        let config = raw.into_config(false).unwrap();
        assert_eq!(
            config.always_thinking,
            Some(StoredThinking::Budget { tokens: 8192 })
        );

        let raw = RawConfig {
            always_thinking: Some(AlwaysThinking::Mode("fast".into())),
            ..Default::default()
        };
        let err = raw.into_config(false).err().expect("expected config error");
        assert!(matches!(err, ConfigError::Thinking(_)));
    }

    #[test_case("max_output_bytes",  0 ; "zero_output_bytes")]
    #[test_case("max_output_lines",  0 ; "zero_output_lines")]
    #[test_case("max_output_bytes",  500 ; "below_min_output_bytes")]
    fn validate_rejects_invalid_agent(field: &str, value: usize) {
        let mut config = AgentConfig::default();
        match field {
            "max_output_bytes" => config.max_output_bytes = value,
            "max_output_lines" => config.max_output_lines = value,
            _ => unreachable!(),
        }
        let err = config.validate().unwrap_err();
        assert!(matches!(err, ConfigError::BelowMinimum { field: f, .. } if f == field));
    }
    #[test]
    fn validate_rejects_compaction_as_fusion_sidekick_tier() {
        let mut config = RawConfig::default().into_config(false).unwrap();
        config.agent.fusion.sidekick_tier = crate::providers::Tier::Compaction;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidFusionSidekickTier { .. })
        ));
    }

    const OVERRIDE_RETAINED_TOOL_OUTPUTS: usize = 64;
    const OVERRIDE_RETAINED_SUBAGENT_HISTORIES: usize = 8;

    #[test]
    fn retention_budget_defaults_match_storage() {
        let budget = StorageConfig::default().retention_budget();
        assert_eq!(budget, RetentionBudget::default());
    }

    #[test]
    fn retention_budget_reads_the_storage_table() {
        let raw = RawConfig {
            storage: StorageFileConfig {
                max_retained_tool_outputs: Some(OVERRIDE_RETAINED_TOOL_OUTPUTS),
                max_retained_subagent_histories: Some(OVERRIDE_RETAINED_SUBAGENT_HISTORIES),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = raw.into_config(false).unwrap();
        assert_eq!(
            config.storage.retention_budget(),
            RetentionBudget {
                tool_outputs: OVERRIDE_RETAINED_TOOL_OUTPUTS,
                subagent_histories: OVERRIDE_RETAINED_SUBAGENT_HISTORIES,
            }
        );
    }

    #[test_case("max_retained_tool_outputs", 0 ; "zero_retained_tool_outputs")]
    #[test_case("max_retained_subagent_histories", 0 ; "zero_retained_subagent_histories")]
    #[test_case("max_retained_tool_outputs", MIN_MAX_RETAINED_TOOL_OUTPUTS - 1 ; "below_min_retained_tool_outputs")]
    fn validate_rejects_invalid_retention(field: &str, value: usize) {
        let mut config = StorageConfig::default();
        match field {
            "max_retained_tool_outputs" => config.max_retained_tool_outputs = value,
            "max_retained_subagent_histories" => config.max_retained_subagent_histories = value,
            _ => unreachable!(),
        }
        let err = config.validate().unwrap_err();
        assert!(matches!(err, ConfigError::BelowMinimum { field: f, .. } if f == field));
    }

    #[test]
    fn tool_output_lines_per_tool_override() {
        let raw = RawConfig {
            ui: UiFileConfig {
                tool_output_lines: Some(ToolOutputLinesFile {
                    bash: Some(20),
                    read: Some(20),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.ui.tool_output_lines.bash, 20);
        assert_eq!(config.ui.tool_output_lines.read, 20);
        assert_eq!(
            config.ui.tool_output_lines.index,
            ToolOutputLines::DEFAULT.index
        );
    }

    #[test]
    fn tool_output_lines_get_routes_semblem_search_text_to_grep_bucket() {
        let mut tol = ToolOutputLines::DEFAULT;
        tol.grep = 99;
        tol.other = 1;

        assert_eq!(tol.get("semblem"), 99);
        assert_eq!(tol.get("search_text"), 99);
    }

    #[test_case("provider", "connect_timeout_secs", 0 ; "provider_zero_connect_timeout")]
    #[test_case("storage",  "max_log_files",        0 ; "storage_zero_log_files")]
    #[test_case("ui",       "mouse_scroll_lines",   0 ; "ui_zero_scroll_lines")]
    #[test_case("agent",    "max_output_lines",     1 ; "agent_output_lines_too_low")]
    fn validate_rejects_invalid_sections(section: &str, field: &str, value: u64) {
        let mut config = Config {
            always_yolo: false,
            always_fast: false,
            always_workflow: false,
            always_fusion: false,
            always_thinking: None,
            ui: UiConfig::default(),
            agent: AgentConfig::default(),
            provider: ProviderConfig::default(),
            search: SearchConfig::default(),
            storage: StorageConfig::default(),
            permissions: PermissionsConfig::default(),
            project_trusted: false,
            plugins: PluginsConfig::default(),
        };
        match (section, field) {
            ("provider", "connect_timeout_secs") => {
                config.provider.connect_timeout = Duration::from_secs(value);
            }
            ("storage", "max_log_files") => {
                config.storage.max_log_files = u32::try_from(value).unwrap();
            }
            ("ui", "mouse_scroll_lines") => {
                config.ui.mouse_scroll_lines = u32::try_from(value).unwrap();
            }
            ("agent", "max_output_lines") => {
                config.agent.max_output_lines = usize::try_from(value).unwrap();
            }
            _ => unreachable!(),
        }
        let err = config.validate().unwrap_err();
        assert!(matches!(
            err,
            ConfigError::BelowMinimum { section: s, field: f, .. } if s == section && f == field
        ));
    }

    #[test_case(false, DefaultEffect::Prompt ; "untrusted_project_ignored")]
    #[test_case(true, DefaultEffect::Deny ; "trusted_project_loaded")]
    fn project_permissions_respect_trust(project_trusted: bool, expected_default: DefaultEffect) {
        let dir = TempDir::new().unwrap();
        let project_dir = dir.path().join(PROJECT_DIR);
        fs::create_dir(&project_dir).unwrap();
        fs::write(project_dir.join(PERMISSIONS_FILE), "default = \"deny\"\n").unwrap();

        let permissions = load_permissions_inner(dir.path(), &[], project_trusted);

        assert_eq!(permissions.default, expected_default);
    }

    #[test]
    fn permissions_loaded_from_permissions_file() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"allow\"\n\n\
             [bash]\nallow = [\n    \"cargo *\",\n]\ndeny = [\n    \"rm -rf *\",\n]\n",
        );

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(perms.default, DefaultEffect::Allow);
        assert_eq!(perms.rules.len(), 2);
        assert_eq!(perms.rules[0].effect, Effect::Deny);
        assert_eq!(perms.rules[0].tool, ToolKey::native("bash"));
        assert_eq!(perms.rules[0].scope.as_deref(), Some("rm -rf *"));
        assert_eq!(perms.rules[1].effect, Effect::Allow);
        assert_eq!(perms.rules[1].tool, ToolKey::native("bash"));
        assert_eq!(perms.rules[1].scope.as_deref(), Some("cargo *"));
    }

    #[test]
    fn permissions_merge_global_and_project() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[bash]\nallow = [\"git *\"]\ndeny = [\"rm -rf *\"]\n",
        );
        let n00n_dir = dir.path().join(".n00n");
        fs::create_dir_all(&n00n_dir).unwrap();
        fs::write(
            n00n_dir.join("permissions.toml"),
            "[read]\nallow = true\n\
             [write]\ndeny = [\"/etc/*\"]\n",
        )
        .unwrap();

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(perms.default, DefaultEffect::Prompt);
        assert_eq!(perms.rules.len(), 4);

        let deny_rules: Vec<_> = perms
            .rules
            .iter()
            .filter(|r| r.effect == Effect::Deny)
            .collect();
        let allow_rules: Vec<_> = perms
            .rules
            .iter()
            .filter(|r| r.effect == Effect::Allow)
            .collect();

        assert_eq!(deny_rules.len(), 2);
        assert_eq!(deny_rules[0].tool, ToolKey::native("bash"));
        assert_eq!(deny_rules[1].tool, ToolKey::native("write"));

        assert_eq!(allow_rules.len(), 2);
        assert_eq!(allow_rules[0].tool, ToolKey::native("bash"));
        assert_eq!(allow_rules[1].tool, ToolKey::native("read"));
    }

    #[test]
    fn project_default_allow_ignored() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        let n00n_dir = dir.path().join(".n00n");
        fs::create_dir_all(&n00n_dir).unwrap();
        fs::write(n00n_dir.join("permissions.toml"), "default = \"allow\"\n").unwrap();

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(perms.default, DefaultEffect::Prompt);
    }

    #[test]
    fn append_permission_rule_writes_canonical_tool_name() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();

        append_permission_rule_with_global(
            &ToolKey::native("bash"),
            Some("cargo *"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();
        append_permission_rule_with_global(
            &ToolKey::native("bash"),
            Some("rm -rf *"),
            Effect::Deny,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(content.contains("[run_shell]"));
        assert!(!content.contains("[bash]"));
        assert!(content.contains("cargo *"));
        assert!(content.contains("rm -rf *"));
        assert!(!content.contains("[permissions]"));
    }

    #[test]
    fn append_permission_rule_writes_mcp_nested_form() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();

        append_permission_rule_with_global(
            &ToolKey::parse("deepwiki.search").unwrap(),
            Some("*"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(content.contains("[mcp.deepwiki]"), "nested table present");
        assert!(content.contains("\"search\""), "tool name in array");
        assert!(!content.contains("deepwiki.search"), "no flat key");
        assert!(!content.contains("__"), "no __ separator");
    }

    #[test]
    fn no_permissions_file_returns_defaults() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(perms.default, DefaultEffect::Prompt);
        assert!(perms.rules.is_empty());
    }

    #[test]
    fn deny_rules_before_allow_rules() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[bash]\nallow = [\"git *\"]\ndeny = [\"rm *\"]\n",
        );

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(perms.rules[0].effect, Effect::Deny);
        assert_eq!(perms.rules[1].effect, Effect::Allow);
    }

    #[test]
    fn permissions_default_deny_global() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "default = \"deny\"\n");

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(perms.default, DefaultEffect::Deny);
    }

    #[test]
    fn permissions_default_per_tool() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"deny\"\n\n[bash]\ndefault = \"allow\"\nallow = [\"cargo *\"]\n",
        );

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(perms.default, DefaultEffect::Deny);
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::native("bash")).copied(),
            Some(DefaultEffect::Allow)
        );
    }

    #[test]
    fn permissions_default_merge_project_overrides_global_per_tool() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[run_shell]\ndefault = \"allow\"\n");
        let n00n_dir = dir.path().join(".n00n");
        fs::create_dir_all(&n00n_dir).unwrap();
        fs::write(
            n00n_dir.join("permissions.toml"),
            "[bash]\ndefault = \"deny\"\n",
        )
        .unwrap();

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(
            perms
                .tool_defaults
                .get(&ToolKey::native("run_shell"))
                .copied(),
            Some(DefaultEffect::Deny)
        );
        assert_eq!(ToolKey::native("bash"), ToolKey::native("run_shell"));
    }

    #[test]
    fn permissions_allow_all_migrated() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "allow_all = true\n\n[bash]\nallow = [\"cargo *\"]\n",
        );

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(perms.default, DefaultEffect::Allow);

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(!content.contains("allow_all"));
        assert!(content.contains("default = \"allow\""));
    }

    #[test]
    fn permissions_allow_all_false_migrated_removed() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "allow_all = false\n");

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(perms.default, DefaultEffect::Prompt);

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(!content.contains("allow_all"));
        assert!(!content.contains("default"));
    }

    #[test]
    fn project_default_deny_allowed() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        let n00n_dir = dir.path().join(".n00n");
        fs::create_dir_all(&n00n_dir).unwrap();
        fs::write(n00n_dir.join("permissions.toml"), "default = \"deny\"\n").unwrap();

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(perms.default, DefaultEffect::Deny);
    }

    #[test]
    fn append_permission_rule_deduplicates() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();

        append_permission_rule_with_global(
            &ToolKey::native("bash"),
            Some("cargo *"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();
        append_permission_rule_with_global(
            &ToolKey::native("bash"),
            Some("cargo *"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();
        append_permission_rule_with_global(
            &ToolKey::native("bash"),
            Some("cargo *"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert_eq!(content.matches("cargo *").count(), 1);
    }

    #[test]
    #[allow(unsafe_code)]
    fn env_file_precedence() {
        const GLOBAL_ONLY: &str = "TEST_N00N_GLOBAL_ONLY";
        const PROJECT_SHADOWS: &str = "TEST_N00N_PROJECT_SHADOWS";
        const PROCESS_WINS: &str = "TEST_N00N_PROCESS_WINS";

        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        fs::write(
            global.join(".env"),
            format!("{GLOBAL_ONLY}=global\n{PROJECT_SHADOWS}=global\n{PROCESS_WINS}=global"),
        )
        .unwrap();

        let n00n_dir = dir.path().join(".n00n");
        fs::create_dir_all(&n00n_dir).unwrap();
        fs::write(
            n00n_dir.join(".env"),
            format!("{PROJECT_SHADOWS}=project\n{PROCESS_WINS}=project"),
        )
        .unwrap();

        // SAFETY: tests run single-threaded; temporarily mutating process
        // environment is required to validate .env file precedence.
        unsafe {
            std::env::remove_var(GLOBAL_ONLY);
            std::env::remove_var(PROJECT_SHADOWS);
            std::env::set_var(PROCESS_WINS, "process");
        }

        load_env_files_with_global(dir.path(), Some(&global));

        assert_eq!(std::env::var(GLOBAL_ONLY).unwrap(), "global");
        assert_eq!(std::env::var(PROJECT_SHADOWS).unwrap(), "project");
        assert_eq!(std::env::var(PROCESS_WINS).unwrap(), "process");

        // SAFETY: tests run single-threaded; cleanup of environment variables
        // set earlier in this test.
        unsafe {
            std::env::remove_var(GLOBAL_ONLY);
            std::env::remove_var(PROJECT_SHADOWS);
            std::env::remove_var(PROCESS_WINS);
        }
    }

    #[test]
    fn merge_plugins_overlay_wins_per_key() {
        let mut base: RawConfig = toml::from_str(
            "[plugins.index]\nenabled = true\n\
             [plugins.websearch]\nenabled = true\n\
             [plugins.grep]\nenabled = true\nsearch_result_limit = 200\nmax_line_bytes = 900\n",
        )
        .unwrap();
        let overlay: RawConfig = toml::from_str(
            "[plugins.websearch]\nenabled = false\n\
             [plugins.alpha_tool]\nenabled = true\n\
             [plugins.grep]\nsearch_result_limit = 50\n",
        )
        .unwrap();

        base.merge(overlay);
        assert_eq!(
            base.plugins["index"].enabled,
            Some(true),
            "base-only key preserved"
        );
        assert_eq!(
            base.plugins["websearch"].enabled,
            Some(false),
            "overlay replaces"
        );
        assert_eq!(
            base.plugins["alpha_tool"].enabled,
            Some(true),
            "overlay-only key added"
        );
        let grep = &base.plugins["grep"];
        assert_eq!(
            grep.enabled,
            Some(true),
            "enabled preserved when overlay omits it"
        );
        assert_eq!(
            grep.opts["search_result_limit"],
            serde_json::json!(50),
            "overlay opt wins"
        );
        assert_eq!(
            grep.opts["max_line_bytes"],
            serde_json::json!(900),
            "base opt preserved"
        );
    }

    #[test]
    fn show_thinking_deserializes_true() {
        let raw: RawConfig = toml::from_str("[ui]\nshow_thinking = true\n").unwrap();
        assert!(raw.ui.show_thinking.unwrap());
    }

    #[test]
    fn show_thinking_deserializes_false() {
        let raw: RawConfig = toml::from_str("[ui]\nshow_thinking = false\n").unwrap();
        assert!(!raw.ui.show_thinking.unwrap());
    }

    #[test]
    fn show_thinking_missing_defaults_true() {
        let raw: RawConfig = toml::from_str("").unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(config.ui.show_thinking);
    }

    #[test]
    fn max_input_lines_defaults_and_deserializes() {
        let raw: RawConfig = toml::from_str("").unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.ui.max_input_lines, DEFAULT_MAX_INPUT_LINES);

        let raw: RawConfig = toml::from_str("[ui]\nmax_input_lines = 5\n").unwrap();
        assert_eq!(raw.ui.max_input_lines.unwrap(), 5);
    }

    #[test_case("[ui]\nsplash_animaton = true\n" ; "top_level_typo")]
    #[test_case("agent = { unknown_field = 1 }\n" ; "agent_unknown_field")]
    #[test_case("agent = { bash_timeout_secs = 60 }\n" ; "removed_agent_field")]
    #[test_case("[index]\nmax_file_size_mb = 4\n" ; "removed_index_section")]
    #[test_case("[tools.bash]\nenabled = true\n" ; "renamed_tools_section")]
    fn deny_unknown_fields_rejects(toml_str: &str) {
        let result: Result<RawConfig, _> = toml::from_str(toml_str);
        assert!(
            result.is_err(),
            "unknown field should be rejected: {toml_str}"
        );
    }

    #[test]
    fn deny_unknown_fields_accepts_valid_plugins() {
        const VALID: &str =
            "[plugins.bash]\nenabled = true\n[plugins.websearch]\nenabled = false\n";
        let result: Result<RawConfig, _> = toml::from_str(VALID);
        assert!(
            result.is_ok(),
            "valid plugins section should parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn plugin_extra_keys_parse_into_opts() {
        let raw: RawConfig =
            toml::from_str("[plugins.bash]\nenabled = true\ntimeout_secs = 180\n").unwrap();
        let bash = &raw.plugins["bash"];
        assert_eq!(bash.enabled, Some(true));
        assert_eq!(bash.opts["timeout_secs"], serde_json::json!(180));
    }

    #[test]
    fn firecrawl_backend_options_flow_into_config() {
        let raw: RawConfig = toml::from_str(
            "[plugins.websearch]\nbackend = \"firecrawl\"\n\
             [plugins.webfetch]\nbackend = \"direct\"\n",
        )
        .unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(
            config.plugins.opts["websearch"]["backend"],
            serde_json::json!("firecrawl")
        );
        assert_eq!(
            config.plugins.opts["webfetch"]["backend"],
            serde_json::json!("direct")
        );
    }
    #[test]
    fn into_config_wires_plugin_names_and_opts() {
        let raw: RawConfig = toml::from_str(
            "[plugins.bash]\ntimeout_secs = 180\n[plugins.websearch]\nenabled = false\n",
        )
        .unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(config.plugins.names.contains(&"bash".to_string()));
        assert!(!config.plugins.names.contains(&"websearch".to_string()));
        assert!(
            config.plugins.names.contains(&"index".to_string()),
            "untouched builtin stays"
        );
        assert_eq!(
            config.plugins.opts["bash"]["timeout_secs"],
            serde_json::json!(180)
        );
        assert!(
            !config.plugins.opts.contains_key("websearch"),
            "enabled-only tables produce no opts"
        );
    }

    #[test]
    fn from_plugins_default() {
        let plugins = PluginsConfig::from_plugins(&HashMap::new());
        let expected: Vec<String> = DEFAULT_BUILTINS
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert_eq!(plugins.names, expected);
        assert!(plugins.enabled);
    }

    #[test]
    fn from_plugins_enable_disable_and_sort() {
        let mut entries = HashMap::new();
        entries.insert("websearch".to_string(), plugin_enabled(false));
        entries.insert("zeta".to_string(), plugin_enabled(true));
        entries.insert("alpha".to_string(), plugin_enabled(true));
        entries.insert("custom_tool".to_string(), PluginFileConfig::default());

        let plugins = PluginsConfig::from_plugins(&entries);
        assert!(
            !plugins.names.contains(&"websearch".to_string()),
            "disabled builtin removed"
        );
        assert!(
            plugins.names.contains(&"index".to_string()),
            "untouched builtin stays"
        );
        assert!(
            plugins.names.contains(&"bash".to_string()),
            "bash is a default builtin"
        );
        assert!(
            !plugins.names.contains(&"custom_tool".to_string()),
            "enabled=None non-default ignored"
        );

        let extras: Vec<_> = plugins
            .names
            .iter()
            .filter(|t| !DEFAULT_BUILTINS.contains(&t.as_str()))
            .cloned()
            .collect();
        assert_eq!(
            extras,
            vec!["alpha", "zeta"],
            "extras sorted alphabetically"
        );
    }

    #[test]
    fn merge_tool_output_lines_field_level_overlay() {
        let mut base = RawConfig {
            ui: UiFileConfig {
                tool_output_lines: Some(ToolOutputLinesFile {
                    bash: Some(50),
                    read: Some(30),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let overlay = RawConfig {
            ui: UiFileConfig {
                tool_output_lines: Some(ToolOutputLinesFile {
                    bash: Some(100),
                    grep: Some(15),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        base.merge(overlay);
        let tol = base.ui.tool_output_lines.as_ref().unwrap();
        assert_eq!(tol.bash, Some(100), "overlay wins");
        assert_eq!(tol.read, Some(30), "base preserved");
        assert_eq!(tol.grep, Some(15), "overlay added");
    }

    #[test]
    fn default_builtins_sorted() {
        for pair in DEFAULT_BUILTINS.windows(2) {
            assert!(
                pair[0] < pair[1],
                "DEFAULT_BUILTINS not sorted: {:?} >= {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn removed_sub_tool_tables_error() {
        for &tool in EDIT_SUB_TOOLS {
            let raw: RawConfig = toml::from_str(&format!("[plugins.{tool}]\n")).unwrap();
            let Err(err) = raw.into_config(false) else {
                panic!("plugins.{tool} should be rejected");
            };
            let msg = err.to_string();
            assert!(
                msg.contains(&format!("plugins.{tool} was removed"))
                    && msg.contains("plugins.edit = {"),
                "error should point at plugins.edit, got: {msg}"
            );
        }
    }

    #[test_case("enabled = false" ; "enabled_false")]
    #[test_case("search_result_limit = 50" ; "opts_only")]
    fn unknown_plugin_name_errors(body: &str) {
        let raw: RawConfig = toml::from_str(&format!("[plugins.gerp]\n{body}\n")).unwrap();
        let Err(err) = raw.into_config(false) else {
            panic!("plugins.gerp should be rejected");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("no bundled plugin is named \"gerp\"") && msg.contains("grep"),
            "error should name the typo and list bundled plugins, got: {msg}"
        );
    }

    #[test]
    fn disabled_plugin_keeps_opts_but_not_load_entry() {
        let raw: RawConfig =
            toml::from_str("[plugins.bash]\nenabled = false\ntimeout_secs = 180\n").unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(!config.plugins.names.contains(&"bash".to_string()));
        assert_eq!(
            config.plugins.opts["bash"]["timeout_secs"],
            serde_json::json!(180),
            "opts survive for when the plugin is re-enabled"
        );
    }

    #[test]
    fn edit_sub_tool_toggles_flow_as_edit_opts() {
        let raw: RawConfig =
            toml::from_str("[plugins.edit]\nmultiedit = false\nedit_lines = true\n").unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(
            config.plugins.opts["edit"]["multiedit"],
            serde_json::json!(false)
        );
        assert_eq!(
            config.plugins.opts["edit"]["edit_lines"],
            serde_json::json!(true)
        );
        assert!(config.agent.disabled_tools.is_empty());
    }

    #[test]
    fn permissions_mcp_per_tool_allow() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[mcp.deepwiki]\nallow = [\"search\", \"fetch\"]\n",
        );
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(perms.rules.len(), 2);
        assert!(perms.rules.iter().any(|r| r.tool
            == ToolKey::McpTool {
                server: "deepwiki".into(),
                tool: "search".into()
            }
            && r.effect == Effect::Allow));
        assert!(perms.rules.iter().any(|r| r.tool
            == ToolKey::McpTool {
                server: "deepwiki".into(),
                tool: "fetch".into()
            }
            && r.effect == Effect::Allow));
    }

    #[test]
    fn permissions_mcp_server_wide_allow_true_ignored() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.deepwiki]\nallow = true\n");
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(perms.rules.len(), 0, "no rules generated");
        assert!(
            !perms.tool_defaults.contains_key(&ToolKey::McpServer {
                server: "deepwiki".into()
            }),
            "allow = true is deprecated and ignored — no default injected"
        );
    }

    #[test]
    fn permissions_mcp_deny_true_ignored() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.server]\ndeny = true\n");
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert!(
            !perms.tool_defaults.contains_key(&ToolKey::McpServer {
                server: "server".into()
            }),
            "deny = true is deprecated and ignored — no default injected"
        );
    }

    #[test]
    fn explicit_default_preserved_with_deprecated_deny_true() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[mcp.server]\ndefault = \"allow\"\ndeny = true\n",
        );
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::McpServer {
                server: "server".into()
            }),
            Some(&DefaultEffect::Allow),
            "explicit default still works; deprecated deny = true is ignored"
        );
    }

    #[test]
    fn permissions_mcp_deny_rules() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.github]\ndeny = [\"admin_delete\"]\n");
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(perms.rules.len(), 1);
        assert_eq!(
            perms.rules[0].tool,
            ToolKey::McpTool {
                server: "github".into(),
                tool: "admin_delete".into()
            }
        );
        assert_eq!(perms.rules[0].effect, Effect::Deny);
    }

    #[test]
    fn permissions_mcp_dotted_tool_name_rejected() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.myserver]\nallow = [\"web.search\"]\n");
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(perms.rules.len(), 0, "dotted tool name should be rejected");
    }

    #[test]
    fn permissions_mcp_default_allow() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"deny\"\n\n[mcp.exa]\ndefault = \"allow\"\n",
        );
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::McpServer {
                server: "exa".into()
            }),
            Some(&DefaultEffect::Allow),
            "MCP server default should be extracted"
        );
    }

    #[test]
    fn permissions_mcp_default_prompt() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[mcp.exa]\ndefault = \"prompt\"\nallow = [\"search\"]\n",
        );
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::McpServer {
                server: "exa".into()
            }),
            Some(&DefaultEffect::Prompt),
            "MCP server default = prompt should be extracted"
        );
        assert_eq!(perms.rules.len(), 1);
        assert_eq!(
            perms.rules[0].tool,
            ToolKey::McpTool {
                server: "exa".into(),
                tool: "search".into()
            }
        );
    }

    #[test]
    fn migrate_mcp_old_flat_keys() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        // Old n00n format used quoted TOML keys for mcp:server__tool
        fs::write(
            global.join("permissions.toml"),
            "[\"mcp:deepwiki__search\"]\nallow = true\n\
             [\"mcp:github__issue\"]\nallow = [\"read\"]\n",
        )
        .unwrap();

        let _perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(content.contains("[mcp.deepwiki]"), "server table present");
        assert!(content.contains("[mcp.github]"), "server table present");
        assert!(content.contains("\"search\""), "tool name migrated");
        assert!(content.contains("\"issue\""), "tool name migrated");
        assert!(
            !content.contains("mcp:deepwiki__search"),
            "old flat key gone"
        );
        assert!(!content.contains("mcp:github__issue"), "old flat key gone");
        assert!(!content.contains("__"), "no old __ separator remains");
    }

    #[test]
    fn migrate_mcp_nested_bare_keys() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        // Bare TOML key [mcp.deepwiki__search] creates nested mcp → deepwiki__search
        fs::write(
            global.join("permissions.toml"),
            "[mcp]\n\
             deepwiki__search = true\n\
             github__issue = true\n",
        )
        .unwrap();

        let _perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(content.contains("[mcp.deepwiki]"), "server table present");
        assert!(content.contains("[mcp.github]"), "server table present");
        assert!(content.contains("\"search\""), "tool name migrated");
        assert!(content.contains("\"issue\""), "tool name migrated");
        assert!(!content.contains("__"), "no old __ separator remains");
    }

    #[test]
    fn empty_tool_key_sections_ignored() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[\"\"]\ndefault = \"allow\"\nallow = [\"x\"]\n");
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        assert!(perms.rules.is_empty());
        assert!(perms.tool_defaults.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn migration_applies_in_memory_when_write_fails() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        fs::write(
            global.join("permissions.toml"),
            "[\"mcp:github__delete\"]\ndeny = true\n",
        )
        .unwrap();
        fs::set_permissions(&global, fs::Permissions::from_mode(0o555)).unwrap();
        if fs::write(global.join("probe"), b"x").is_ok() {
            return; // running as root, cannot simulate a read-only dir
        }

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global), true);
        fs::set_permissions(&global, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(perms.rules.len(), 1);
        assert_eq!(perms.rules[0].effect, Effect::Deny);
        assert_eq!(
            perms.rules[0].tool,
            ToolKey::parse("github.delete").unwrap()
        );
    }
}
