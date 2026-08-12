//! Types used to define loaded and effective Codex configuration values.

// Note this file should generally be restricted to simple struct/enum
// definitions that do not contain business logic.

pub use crate::shell_environment_policy::ShellEnvironmentPolicyToml;
pub use codex_protocol::config_types::ApprovalsReviewer;
pub use codex_protocol::config_types::Personality;
pub use codex_protocol::config_types::ServiceTier;
pub use codex_protocol::config_types::WebSearchMode;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::BTreeMap;
use std::collections::HashMap;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

pub use crate::tui_keymap::KeybindingSpec;
pub use crate::tui_keymap::KeybindingsSpec;
pub use crate::tui_keymap::MAX_FUNCTION_KEY;
pub use crate::tui_keymap::TuiChatKeymap;
pub use crate::tui_keymap::TuiComposerKeymap;
pub use crate::tui_keymap::TuiEditorKeymap;
pub use crate::tui_keymap::TuiGlobalKeymap;
pub use crate::tui_keymap::TuiKeymap;
pub use crate::tui_keymap::TuiListKeymap;
pub use crate::tui_keymap::TuiPagerKeymap;
pub use crate::tui_keymap::TuiVimNormalKeymap;
pub use crate::tui_keymap::TuiVimOperatorKeymap;

pub const DEFAULT_MEMORIES_MAX_ROLLOUTS_PER_STARTUP: usize = 2;
pub const DEFAULT_MEMORIES_MAX_ROLLOUT_AGE_DAYS: i64 = 10;
pub const DEFAULT_MEMORIES_MIN_ROLLOUT_IDLE_HOURS: i64 = 6;
pub const DEFAULT_MEMORIES_MIN_RATE_LIMIT_REMAINING_PERCENT: i64 = 25;
pub const DEFAULT_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION: usize = 256;
pub const DEFAULT_MEMORIES_MAX_UNUSED_DAYS: i64 = 30;
const MIN_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION: usize = 1;
const MAX_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION: usize = 4096;
const MIN_MEMORIES_MAX_ROLLOUTS_PER_STARTUP: usize = 1;
const MAX_MEMORIES_MAX_ROLLOUTS_PER_STARTUP: usize = 128;

const fn default_enabled() -> bool {
    true
}

/// Approval behavior for app tools retained until the app surface is removed.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AppToolApproval {
    #[default]
    Auto,
    Prompt,
    Writes,
    Approve,
}

/// Working directory to use when resuming or forking a session.
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ResumeCwdMode {
    /// Use the directory where Codex was launched.
    Current,
    /// Use the latest working directory recorded in the selected session.
    Session,
}

impl ResumeCwdMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Session => "session",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WindowsSandboxModeToml {
    Elevated,
    Unelevated,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct WindowsToml {
    pub sandbox: Option<WindowsSandboxModeToml>,
    /// Defaults to `true`. Set to `false` to launch the final sandboxed child
    /// process on `Winsta0\\Default` instead of a private desktop.
    pub sandbox_private_desktop: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, JsonSchema)]
pub enum UriBasedFileOpener {
    #[serde(rename = "vscode")]
    VsCode,

    #[serde(rename = "vscode-insiders")]
    VsCodeInsiders,

    #[serde(rename = "windsurf")]
    Windsurf,

    #[serde(rename = "cursor")]
    Cursor,

    /// Option to disable the URI-based file opener.
    #[serde(rename = "none")]
    None,
}

/// Settings that govern if and what will be written to `~/.ycode/history.jsonl`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[serde(default)]
#[schemars(deny_unknown_fields)]
pub struct History {
    /// If true, history entries will not be written to disk.
    pub persistence: HistoryPersistence,

    /// If set, the maximum size of the history file in bytes. The oldest entries
    /// are dropped once the file exceeds this limit.
    pub max_bytes: Option<usize>,
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryPersistence {
    /// Save all history entries to disk.
    #[default]
    SaveAll,
    /// Do not write history to disk.
    None,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolSuggestDiscoverableType {
    Connector,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ToolSuggestDiscoverable {
    #[serde(rename = "type")]
    pub kind: ToolSuggestDiscoverableType,
    pub id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ToolSuggestDisabledTool {
    #[serde(rename = "type")]
    pub kind: ToolSuggestDiscoverableType,
    pub id: String,
}

impl ToolSuggestDisabledTool {
    pub fn connector(id: impl Into<String>) -> Self {
        Self {
            kind: ToolSuggestDiscoverableType::Connector,
            id: id.into(),
        }
    }

    pub fn normalized(&self) -> Option<Self> {
        let id = self.id.trim();
        (!id.is_empty()).then(|| Self {
            kind: self.kind,
            id: id.to_string(),
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ToolSuggestConfig {
    #[serde(default)]
    pub discoverables: Vec<ToolSuggestDiscoverable>,
    #[serde(default)]
    pub disabled_tools: Vec<ToolSuggestDisabledTool>,
}

/// Memories settings loaded from config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct MemoriesToml {
    /// When `true`, external context sources mark the thread `memory_mode` as `"polluted"`.
    pub disable_on_external_context: Option<bool>,
    /// When `false`, newly created threads are stored with `memory_mode = "disabled"` in the state DB.
    pub generate_memories: Option<bool>,
    /// When `false`, skip injecting memory usage instructions into developer prompts.
    pub use_memories: Option<bool>,
    /// When `true`, expose dedicated memory tools through the extension tool surface.
    pub dedicated_tools: Option<bool>,
    /// Maximum number of recent raw memories retained for global consolidation.
    #[schemars(range(min = 1, max = 4096))]
    pub max_raw_memories_for_consolidation: Option<usize>,
    /// Maximum number of days since a memory was last used before it becomes ineligible for phase 2 selection.
    pub max_unused_days: Option<i64>,
    /// Maximum age of the threads used for memories.
    pub max_rollout_age_days: Option<i64>,
    /// Maximum number of rollout candidates processed per pass.
    #[schemars(range(min = 1, max = 128))]
    pub max_rollouts_per_startup: Option<usize>,
    /// Minimum idle time between last thread activity and memory creation (hours). > 12h recommended.
    pub min_rollout_idle_hours: Option<i64>,
    /// Minimum remaining percentage required in Codex rate-limit windows before memory startup runs.
    #[schemars(range(min = 0, max = 100))]
    pub min_rate_limit_remaining_percent: Option<i64>,
    /// Model used for thread summarisation.
    pub extract_model: Option<String>,
    /// Model used for memory consolidation.
    pub consolidation_model: Option<String>,
}

/// Effective memories settings after defaults are applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoriesConfig {
    pub disable_on_external_context: bool,
    pub generate_memories: bool,
    pub use_memories: bool,
    pub dedicated_tools: bool,
    pub max_raw_memories_for_consolidation: usize,
    pub max_unused_days: i64,
    pub max_rollout_age_days: i64,
    pub max_rollouts_per_startup: usize,
    pub min_rollout_idle_hours: i64,
    pub min_rate_limit_remaining_percent: i64,
    pub extract_model: Option<String>,
    pub consolidation_model: Option<String>,
}

impl Default for MemoriesConfig {
    fn default() -> Self {
        Self {
            disable_on_external_context: false,
            generate_memories: true,
            use_memories: true,
            dedicated_tools: false,
            max_raw_memories_for_consolidation: DEFAULT_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION,
            max_unused_days: DEFAULT_MEMORIES_MAX_UNUSED_DAYS,
            max_rollout_age_days: DEFAULT_MEMORIES_MAX_ROLLOUT_AGE_DAYS,
            max_rollouts_per_startup: DEFAULT_MEMORIES_MAX_ROLLOUTS_PER_STARTUP,
            min_rollout_idle_hours: DEFAULT_MEMORIES_MIN_ROLLOUT_IDLE_HOURS,
            min_rate_limit_remaining_percent: DEFAULT_MEMORIES_MIN_RATE_LIMIT_REMAINING_PERCENT,
            extract_model: None,
            consolidation_model: None,
        }
    }
}

impl From<MemoriesToml> for MemoriesConfig {
    fn from(toml: MemoriesToml) -> Self {
        let defaults = Self::default();
        Self {
            disable_on_external_context: toml
                .disable_on_external_context
                .unwrap_or(defaults.disable_on_external_context),
            generate_memories: toml.generate_memories.unwrap_or(defaults.generate_memories),
            use_memories: toml.use_memories.unwrap_or(defaults.use_memories),
            dedicated_tools: toml.dedicated_tools.unwrap_or(defaults.dedicated_tools),
            max_raw_memories_for_consolidation: toml
                .max_raw_memories_for_consolidation
                .unwrap_or(defaults.max_raw_memories_for_consolidation)
                .clamp(
                    MIN_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION,
                    MAX_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION,
                ),
            max_unused_days: toml
                .max_unused_days
                .unwrap_or(defaults.max_unused_days)
                .clamp(0, 365),
            max_rollout_age_days: toml
                .max_rollout_age_days
                .unwrap_or(defaults.max_rollout_age_days)
                .clamp(0, 90),
            max_rollouts_per_startup: toml
                .max_rollouts_per_startup
                .unwrap_or(defaults.max_rollouts_per_startup)
                .clamp(
                    MIN_MEMORIES_MAX_ROLLOUTS_PER_STARTUP,
                    MAX_MEMORIES_MAX_ROLLOUTS_PER_STARTUP,
                ),
            min_rollout_idle_hours: toml
                .min_rollout_idle_hours
                .unwrap_or(defaults.min_rollout_idle_hours)
                .clamp(1, 48),
            min_rate_limit_remaining_percent: toml
                .min_rate_limit_remaining_percent
                .unwrap_or(defaults.min_rate_limit_remaining_percent)
                .clamp(0, 100),
            extract_model: toml.extract_model,
            consolidation_model: toml.consolidation_model,
        }
    }
}

/// Default settings that apply to all apps.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AppsDefaultConfig {
    /// When `false`, apps are disabled unless overridden by per-app settings.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Reviewer for approval prompts unless overridden by per-app settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<ApprovalsReviewer>,

    /// Whether tools with `destructive_hint = true` are allowed by default.
    #[serde(
        default = "default_enabled",
        skip_serializing_if = "std::clone::Clone::clone"
    )]
    pub destructive_enabled: bool,

    /// Whether tools with `open_world_hint = true` are allowed by default.
    #[serde(
        default = "default_enabled",
        skip_serializing_if = "std::clone::Clone::clone"
    )]
    pub open_world_enabled: bool,

    /// Approval mode for tools unless overridden by per-app or per-tool settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools_approval_mode: Option<AppToolApproval>,
}

/// Per-tool settings for a single app tool.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AppToolConfig {
    /// Whether this tool is enabled. `Some(true)` explicitly allows this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,

    /// Approval mode for this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<AppToolApproval>,
}

/// Tool settings for a single app.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AppToolsConfig {
    /// Per-tool overrides keyed by tool name (for example `repos/list`).
    #[serde(default, flatten)]
    pub tools: HashMap<String, AppToolConfig>,
}

/// Config values for a single app/connector.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AppConfig {
    /// When `false`, Codex does not surface this app.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Reviewer for approval prompts from this app, overriding the thread default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<ApprovalsReviewer>,

    /// Whether tools with `destructive_hint = true` are allowed for this app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_enabled: Option<bool>,

    /// Whether tools with `open_world_hint = true` are allowed for this app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_enabled: Option<bool>,

    /// Approval mode for tools in this app unless a tool override exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools_approval_mode: Option<AppToolApproval>,

    /// Whether tools are enabled by default for this app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools_enabled: Option<bool>,

    /// Per-tool settings for this app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<AppToolsConfig>,
}

/// App/connector settings loaded from `config.toml`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AppsConfigToml {
    /// Default settings for all apps.
    #[serde(default, rename = "_default", skip_serializing_if = "Option::is_none")]
    pub default: Option<AppsDefaultConfig>,

    /// Per-app settings keyed by app ID (for example `[apps.google_drive]`).
    #[serde(default, flatten)]
    pub apps: HashMap<String, AppConfig>,
}

/// Fallback resize-reflow row cap when Codex cannot identify a terminal-specific scrollback size.
pub const DEFAULT_TERMINAL_RESIZE_REFLOW_FALLBACK_MAX_ROWS: usize = 1_000;

/// Collection of settings that are specific to the TUI.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(deny_unknown_fields)]
pub struct Tui {
    /// Start the composer in Vim mode (`Normal`) by default.
    /// Defaults to `false`.
    #[serde(default)]
    pub vim_mode_default: bool,

    /// Start the TUI in raw scrollback mode for copy-friendly transcript output.
    /// Defaults to `false`.
    #[serde(default)]
    pub raw_output_mode: bool,

    /// Working directory to use when resuming or forking a session.
    /// When unset, prompt if the current and session directories differ.
    #[serde(default)]
    pub resume_cwd: Option<ResumeCwdMode>,

    /// Keybinding overrides for the TUI.
    ///
    /// This supports rebinding selected actions globally and by context.
    /// Context bindings take precedence over `global` bindings.
    #[serde(default)]
    pub keymap: TuiKeymap,

    /// Trim terminal resize-reflow replay to the most recent rendered terminal rows when the
    /// transcript exceeds this cap. Omit to use Codex's terminal-specific default. Set to `0` to
    /// keep all rendered rows.
    #[serde(default)]
    #[schemars(range(min = 0))]
    pub terminal_resize_reflow_max_rows: Option<usize>,
}

/// Settings for notices we display to users via the tui and app-server clients
/// (primarily the Codex IDE extension). NOTE: these are different from
/// notifications - notices are warnings, NUX screens, acknowledgements, etc.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Notice {
    /// Tracks whether the user has acknowledged the full access warning prompt.
    pub hide_full_access_warning: Option<bool>,
    /// Tracks whether the user has acknowledged the Windows world-writable directories warning.
    pub hide_world_writable_warning: Option<bool>,
    /// Tracks whether the user opted out of Codex-managed fast defaults.
    pub fast_default_opt_out: Option<bool>,
    /// Tracks whether the user opted out of the rate limit model switch reminder.
    pub hide_rate_limit_model_nudge: Option<bool>,
    /// Tracks whether the user has seen the model migration prompt
    pub hide_gpt5_1_migration_prompt: Option<bool>,
    /// Tracks whether the user has seen the gpt-5.1-codex-max migration prompt
    #[serde(rename = "hide_gpt-5.1-codex-max_migration_prompt")]
    pub hide_gpt_5_1_codex_max_migration_prompt: Option<bool>,
    /// Tracks acknowledged model migrations as old->new model slug mappings.
    #[serde(default)]
    pub model_migrations: BTreeMap<String, String>,
}

pub use crate::skills_config::BundledSkillsConfig;
pub use crate::skills_config::SkillConfig;
pub use crate::skills_config::SkillsConfig;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct SandboxWorkspaceWrite {
    #[serde(default)]
    pub writable_roots: Vec<AbsolutePathBuf>,
    #[serde(default)]
    pub network_access: bool,
    #[serde(default)]
    pub exclude_tmpdir_env_var: bool,
    #[serde(default)]
    pub exclude_slash_tmp: bool,
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;
