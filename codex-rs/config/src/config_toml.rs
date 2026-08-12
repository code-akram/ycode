//! Schema-heavy configuration TOML types used by Codex.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;

use crate::permissions_toml::PermissionsToml;
use crate::profile_toml::ConfigProfile;
use crate::types::ApprovalsReviewer;
use crate::types::AppsConfigToml;
use crate::types::History;
use crate::types::MemoriesToml;
use crate::types::Notice;
use crate::types::SandboxWorkspaceWrite;
use crate::types::ShellEnvironmentPolicyToml;
use crate::types::SkillsConfig;
use crate::types::ToolSuggestConfig;
use crate::types::Tui;
use crate::types::UriBasedFileOpener;
use crate::types::WindowsToml;
use codex_features::FeaturesToml;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::config_types::ForcedLoginMethod;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::SandboxMode;
use codex_protocol::config_types::TrustLevel;
use codex_protocol::config_types::Verbosity;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::config_types::WebSearchToolConfig;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AskForApproval;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path::normalize_for_path_comparison;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::Error as SerdeError;
use serde_json::Value as JsonValue;

pub const DEFAULT_PROJECT_DOC_MAX_BYTES: usize = 32 * 1024;

fn default_history() -> Option<History> {
    Some(History::default())
}

const fn default_project_doc_max_bytes() -> Option<usize> {
    Some(DEFAULT_PROJECT_DOC_MAX_BYTES)
}

fn default_project_doc_fallback_filenames() -> Option<Vec<String>> {
    Some(Vec::new())
}

const fn default_hide_agent_reasoning() -> Option<bool> {
    Some(false)
}

const fn default_true() -> bool {
    true
}

/// Backward-compatible shape for ChatGPT workspace login restrictions in config.toml.
#[derive(Serialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(untagged)]
pub enum ForcedChatgptWorkspaceIds {
    Single(String),
    Multiple(Vec<String>),
}

impl ForcedChatgptWorkspaceIds {
    pub fn into_vec(self) -> Vec<String> {
        match self {
            Self::Single(value) => vec![value],
            Self::Multiple(values) => values,
        }
    }
}

impl<'de> Deserialize<'de> for ForcedChatgptWorkspaceIds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Single(String),
            Multiple(Vec<String>),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Single(value) if value.contains(',') => Err(D::Error::custom(
                "forced_chatgpt_workspace_id must be a single workspace ID string or a TOML list \
of strings; comma-separated strings are not supported. Use \
`forced_chatgpt_workspace_id = [\"123e4567-e89b-42d3-a456-426614174000\", \
\"123e4567-e89b-42d3-a456-426614174001\"]` instead.",
            )),
            Repr::Single(value) => Ok(Self::Single(value)),
            Repr::Multiple(values) => Ok(Self::Multiple(values)),
        }
    }
}

/// Orchestrator-owned feature settings.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OrchestratorToml {
    pub skills: Option<OrchestratorFeatureToml>,
}

/// Settings for a feature owned by the orchestrator.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OrchestratorFeatureToml {
    pub enabled: Option<bool>,
}

/// Base config deserialized from ~/.codex/config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ConfigToml {
    /// Optional override of model selection.
    pub model: Option<String>,
    /// Review model override used by the `/review` feature.
    pub review_model: Option<String>,

    /// Size of the context window for the model, in tokens.
    pub model_context_window: Option<i64>,

    /// Token usage threshold triggering auto-compaction of conversation history.
    pub model_auto_compact_token_limit: Option<i64>,

    /// Controls whether the auto-compaction limit applies to the full context or
    /// only to tokens after the carried prefix in the current compaction window.
    pub model_auto_compact_token_limit_scope: Option<AutoCompactTokenLimitScope>,

    #[serde(skip)]
    #[schemars(skip)]
    pub approval_policy: Option<AskForApproval>,

    #[serde(skip)]
    #[schemars(skip)]
    pub approvals_reviewer: Option<ApprovalsReviewer>,

    /// Optional policy instructions for the guardian auto-reviewer.
    #[serde(skip)]
    #[schemars(skip)]
    pub auto_review: Option<AutoReviewToml>,

    #[serde(default)]
    pub shell_environment_policy: ShellEnvironmentPolicyToml,

    /// Whether the model may request a login shell for shell-based tools.
    /// Default to `true`
    ///
    /// If `true`, the model may request a login shell (`login = true`), and
    /// omitting `login` defaults to using a login shell.
    /// If `false`, the model can never use a login shell: `login = true`
    /// requests are rejected, and omitting `login` defaults to a non-login
    /// shell.
    pub allow_login_shell: Option<bool>,

    #[serde(skip)]
    #[schemars(skip)]
    pub sandbox_mode: Option<SandboxMode>,

    #[serde(skip)]
    #[schemars(skip)]
    pub sandbox_workspace_write: Option<SandboxWorkspaceWrite>,

    #[serde(skip)]
    #[schemars(skip)]
    pub default_permissions: Option<String>,

    #[serde(skip)]
    #[schemars(skip)]
    pub permissions: Option<PermissionsToml>,

    /// Optional external command to spawn for end-user notifications.
    #[serde(default)]
    pub notify: Option<Vec<String>>,

    /// System instructions.
    pub instructions: Option<String>,

    /// Developer instructions inserted as a `developer` role message.
    #[serde(default)]
    pub developer_instructions: Option<String>,

    #[serde(skip)]
    #[schemars(skip)]
    pub include_permissions_instructions: Option<bool>,

    /// Whether to inject the `<apps_instructions>` developer block.
    pub include_apps_instructions: Option<bool>,

    /// Whether to inject the `<environment_context>` user block.
    pub include_environment_context: Option<bool>,

    /// Optional path to a file containing model instructions that will override
    /// the built-in instructions for the selected model. Users are STRONGLY
    /// DISCOURAGED from using this field, as deviating from the instructions
    /// sanctioned by Codex will likely degrade model performance.
    pub model_instructions_file: Option<AbsolutePathBuf>,

    /// Compact prompt used for history compaction.
    pub compact_prompt: Option<String>,

    /// When set, restricts ChatGPT login to one or more workspace identifiers.
    #[serde(default)]
    pub forced_chatgpt_workspace_id: Option<ForcedChatgptWorkspaceIds>,

    /// When set, restricts the login mechanism users may use.
    #[serde(default)]
    pub forced_login_method: Option<ForcedLoginMethod>,

    /// Maximum number of bytes to include from an AGENTS.md project doc file.
    #[serde(default = "default_project_doc_max_bytes")]
    pub project_doc_max_bytes: Option<usize>,

    /// Ordered list of fallback filenames to look for when AGENTS.md is missing.
    #[serde(default = "default_project_doc_fallback_filenames")]
    pub project_doc_fallback_filenames: Option<Vec<String>>,

    /// Token budget applied when storing tool/function outputs in the context manager.
    pub tool_output_token_limit: Option<usize>,

    /// Maximum poll window for background terminal output (`write_stdin`), in milliseconds.
    /// Default: `300000` (5 minutes).
    pub background_terminal_max_timeout: Option<u64>,

    /// Deprecated: ignored.
    #[schemars(skip)]
    pub js_repl_node_path: Option<AbsolutePathBuf>,

    /// Deprecated: ignored.
    #[schemars(skip)]
    pub js_repl_node_module_dirs: Option<Vec<AbsolutePathBuf>>,

    /// Profile to use from the `profiles` map.
    pub profile: Option<String>,

    /// Named profiles to facilitate switching between different configurations.
    #[serde(default)]
    pub profiles: HashMap<String, ConfigProfile>,

    /// Settings that govern if and what will be written to `~/.codex/history.jsonl`.
    #[serde(default = "default_history")]
    pub history: Option<History>,

    /// Directory where Codex stores the SQLite state DB.
    /// Defaults to `$CODEX_SQLITE_HOME` when set. Otherwise uses `$CODEX_HOME`.
    pub sqlite_home: Option<AbsolutePathBuf>,

    /// Directory where Codex writes log files. Setting this value explicitly
    /// also enables the TUI text log in this directory.
    /// Defaults to `$CODEX_HOME/log`.
    pub log_dir: Option<AbsolutePathBuf>,

    /// Debugging and reproducibility settings.
    pub debug: Option<DebugToml>,

    /// Optional URI-based file opener. If set, citations to files in the model
    /// output will be hyperlinked using the specified URI scheme.
    pub file_opener: Option<UriBasedFileOpener>,

    /// Collection of settings that are specific to the TUI.
    pub tui: Option<Tui>,

    /// When set to `true`, `AgentReasoning` events will be hidden from the
    /// UI/output. Defaults to `false`.
    #[serde(default = "default_hide_agent_reasoning")]
    pub hide_agent_reasoning: Option<bool>,

    /// When set to `true`, `AgentReasoningRawContentEvent` events will be shown in the UI/output.
    /// Defaults to `false`.
    pub show_raw_agent_reasoning: Option<bool>,

    pub model_reasoning_effort: Option<ReasoningEffort>,
    pub model_reasoning_summary: Option<ReasoningSummary>,
    /// Optional verbosity control for GPT-5 models (Responses API `text.verbosity`).
    pub model_verbosity: Option<Verbosity>,

    /// Optional path to a JSON model catalog (applied on startup only).
    /// Per-thread `config` overrides are accepted but do not reapply this (no-ops).
    pub model_catalog_json: Option<AbsolutePathBuf>,

    /// Optionally specify a personality for the model
    pub personality: Option<Personality>,

    /// Optional explicit service tier request id for new turns (for example
    /// `default`, `priority`, or `flex`; legacy `fast` also works).
    pub service_tier: Option<String>,

    /// Orchestrator-owned feature settings.
    pub orchestrator: Option<OrchestratorToml>,

    /// Machine-local realtime audio device preferences used by realtime voice.
    #[serde(default)]
    pub audio: Option<RealtimeAudioToml>,

    /// Experimental / do not use. Selects the realtime websocket model/snapshot
    /// used for the `Op::RealtimeConversation` connection.
    pub experimental_realtime_ws_model: Option<String>,
    /// Experimental / do not use. Realtime websocket session selection.
    /// `version` controls v1/v2 and `type` controls conversational/transcription.
    #[serde(default)]
    pub realtime: Option<RealtimeToml>,
    /// Experimental / do not use. Overrides only the realtime conversation
    /// websocket transport instructions (the `Op::RealtimeConversation`
    /// `/ws` session.update instructions) without changing normal prompts.
    pub experimental_realtime_ws_backend_prompt: Option<String>,
    /// Experimental / do not use. Replaces the synthesized realtime startup
    /// context appended to websocket session instructions. An empty string
    /// disables startup context injection entirely.
    pub experimental_realtime_ws_startup_context: Option<String>,
    /// Experimental / do not use. Replaces the built-in realtime start
    /// instructions inserted into developer messages when realtime becomes
    /// active.
    pub experimental_realtime_start_instructions: Option<String>,

    /// Removed. Former remote thread-store endpoint setting kept only so we can
    /// fail fast instead of silently falling back to local persistence.
    #[schemars(skip)]
    pub experimental_thread_store_endpoint: Option<String>,

    /// Experimental / do not use. Selects the thread store implementation.
    pub experimental_thread_store: Option<ThreadStoreToml>,
    pub projects: Option<HashMap<String, ProjectConfig>>,

    /// Controls the web search tool mode: disabled, cached, indexed, or live.
    pub web_search: Option<WebSearchMode>,

    /// Nested tools section for feature toggles
    pub tools: Option<ToolsToml>,

    /// Additional discoverable tools that can be suggested for installation.
    pub tool_suggest: Option<ToolSuggestConfig>,

    /// Agent-related settings (thread limits, etc.).
    pub agents: Option<AgentsToml>,

    /// Memories subsystem settings.
    pub memories: Option<MemoriesToml>,

    /// User-level skill config entries keyed by SKILL.md path.
    pub skills: Option<SkillsConfig>,

    /// Centralized feature flags (new). Prefer this over individual toggles.
    #[serde(default)]
    // Injects known feature keys into the schema and forbids unknown keys.
    #[schemars(schema_with = "crate::schema::features_schema")]
    pub features: Option<FeaturesToml>,

    /// Suppress warnings about unstable (under development) features.
    pub suppress_unstable_features_warning: Option<bool>,

    /// Compatibility-only settings retained so legacy `ghost_snapshot`
    /// config still loads.
    #[serde(default)]
    pub ghost_snapshot: Option<GhostSnapshotToml>,

    /// Markers used to detect the project root when searching parent
    /// directories for `.codex` folders. Defaults to [".git"] when unset.
    #[serde(default)]
    pub project_root_markers: Option<Vec<String>>,

    /// When `true`, checks for Codex updates on startup and surfaces update prompts.
    /// Set to `false` only if your Codex updates are centrally managed.
    /// Defaults to `true`.
    pub check_for_update_on_startup: Option<bool>,

    /// When true, disables burst-paste detection for typed input entirely.
    /// All characters are inserted as they are received, and no buffering
    /// or placeholder replacement will occur for fast keypress bursts.
    pub disable_paste_burst: Option<bool>,

    /// Settings for app-specific controls.
    #[serde(default)]
    pub apps: Option<AppsConfigToml>,

    /// Opaque desktop settings stored alongside the rest of config.toml.
    #[serde(default)]
    pub desktop: Option<HashMap<String, JsonValue>>,

    /// Windows-specific configuration.
    #[serde(default)]
    pub windows: Option<WindowsToml>,

    /// Collection of in-product notices (different from notifications)
    /// See [`crate::types::Notice`] for more details
    pub notice: Option<Notice>,

    pub experimental_compact_prompt_file: Option<AbsolutePathBuf>,
    pub experimental_use_unified_exec_tool: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ConfigLockfileToml {
    pub version: u32,
    pub codex_version: String,

    /// Replayable effective config captured in the lockfile.
    pub config: ConfigToml,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct DebugToml {
    pub config_lockfile: Option<DebugConfigLockToml>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct DebugConfigLockToml {
    /// Directory where Codex writes effective session config lock files.
    pub export_dir: Option<AbsolutePathBuf>,

    /// Lockfile to replay as the authoritative effective config.
    pub load_path: Option<AbsolutePathBuf>,

    /// Allow replaying a lock generated by a different Codex version.
    pub allow_codex_version_mismatch: Option<bool>,

    /// Save fields resolved from the model catalog/session configuration.
    pub save_fields_resolved_from_model_catalog: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadStoreToml {
    Local {},
    #[schemars(skip)]
    InMemory {
        id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
pub struct AutoReviewToml {
    /// Additional policy instructions inserted into the guardian prompt.
    pub policy: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ProjectConfig {
    pub trust_level: Option<TrustLevel>,
}

impl ProjectConfig {
    pub fn is_trusted(&self) -> bool {
        matches!(self.trust_level, Some(TrustLevel::Trusted))
    }

    pub fn is_untrusted(&self) -> bool {
        matches!(self.trust_level, Some(TrustLevel::Untrusted))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RealtimeAudioConfig {
    pub microphone: Option<String>,
    pub speaker: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeWsMode {
    #[default]
    Conversational,
    Transcription,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeTransport {
    #[default]
    #[serde(rename = "webrtc")]
    WebRtc,
    Websocket,
}

pub use codex_protocol::protocol::RealtimeConversationVersion as RealtimeWsVersion;
pub use codex_protocol::protocol::RealtimeVoice;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RealtimeConfig {
    pub version: RealtimeWsVersion,
    #[serde(rename = "type")]
    pub session_type: RealtimeWsMode,
    pub transport: RealtimeTransport,
    pub voice: Option<RealtimeVoice>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RealtimeToml {
    pub version: Option<RealtimeWsVersion>,
    #[serde(rename = "type")]
    pub session_type: Option<RealtimeWsMode>,
    pub transport: Option<RealtimeTransport>,
    pub voice: Option<RealtimeVoice>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RealtimeAudioToml {
    pub microphone: Option<String>,
    pub speaker: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ToolsToml {
    #[serde(
        default,
        deserialize_with = "deserialize_optional_web_search_tool_config"
    )]
    pub web_search: Option<WebSearchToolConfig>,
    pub experimental_request_user_input: Option<ExperimentalRequestUserInput>,
    pub update_plan: Option<UpdatePlanToolConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ExperimentalRequestUserInput {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct UpdatePlanToolConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WebSearchToolConfigInput {
    Enabled(bool),
    Config(WebSearchToolConfig),
}

fn deserialize_optional_web_search_tool_config<'de, D>(
    deserializer: D,
) -> Result<Option<WebSearchToolConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<WebSearchToolConfigInput>::deserialize(deserializer)?;

    Ok(match value {
        None => None,
        Some(WebSearchToolConfigInput::Enabled(enabled)) => {
            let _ = enabled;
            None
        }
        Some(WebSearchToolConfigInput::Config(config)) => Some(config),
    })
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AgentsToml {
    /// Whether multi-agent tools are enabled. Defaults to true.
    /// An enabled `features.multi_agent_v2` setting takes precedence.
    pub enabled: Option<bool>,
    /// Maximum number of spawned agent threads that can be open concurrently per session.
    /// When unset, the selected multi-agent backend uses its default.
    #[serde(alias = "max_threads")]
    #[schemars(range(min = 1))]
    pub max_concurrent_threads_per_session: Option<usize>,
    /// Maximum nesting depth for V1 agent threads. Ignored by V2.
    pub max_depth: Option<i32>,
    /// Default model for spawned subagents when the spawn call does not select one.
    pub default_subagent_model: Option<String>,
    /// Default reasoning effort for spawned subagents when the spawn call does not select one.
    pub default_subagent_reasoning_effort: Option<ReasoningEffort>,
    /// Removed agent-job setting retained as a no-op for compatibility.
    #[schemars(skip)]
    pub job_max_runtime_seconds: Option<u64>,
    /// Whether to record a model-visible message when an agent turn is interrupted.
    /// Defaults to true.
    pub interrupt_message: Option<bool>,

    /// User-defined role declarations keyed by role name.
    ///
    /// Example:
    /// ```toml
    /// [agents.researcher]
    /// description = "Research-focused role."
    /// config_file = "./agents/researcher.toml"
    /// nickname_candidates = ["Herodotus", "Ibn Battuta"]
    /// ```
    #[serde(default, flatten)]
    pub roles: BTreeMap<String, AgentRoleToml>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AgentRoleToml {
    /// Human-facing role documentation used in spawn tool guidance.
    /// Required unless supplied by the referenced agent role file.
    pub description: Option<String>,

    /// Path to a role-specific config layer.
    /// Relative paths are resolved relative to the `config.toml` that defines them.
    pub config_file: Option<AbsolutePathBuf>,

    /// Candidate nicknames for agents spawned with this role.
    pub nickname_candidates: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct GhostSnapshotToml {
    /// Legacy no-op setting retained for compatibility.
    #[serde(alias = "ignore_untracked_files_over_bytes")]
    pub ignore_large_untracked_files: Option<i64>,
    /// Legacy no-op setting retained for compatibility.
    #[serde(alias = "large_untracked_dir_warning_threshold")]
    pub ignore_large_untracked_dirs: Option<i64>,
    /// Legacy no-op setting retained for compatibility.
    pub disable_warnings: Option<bool>,
}

impl ConfigToml {
    /// Derive the effective permission profile from legacy sandbox config.
    ///
    /// Call this only after ruling out `default_permissions`: named
    /// `[permissions]` profiles must be compiled through the permissions
    /// profile pipeline, not reconstructed from `sandbox_mode`.
    pub async fn derive_permission_profile(
        &self,
        sandbox_mode_override: Option<SandboxMode>,
        windows_sandbox_level: WindowsSandboxLevel,
        active_project: Option<&ProjectConfig>,
        permission_profile_constraint: Option<&crate::Constrained<PermissionProfile>>,
    ) -> PermissionProfile {
        let configured_sandbox_mode = sandbox_mode_override.or(self.sandbox_mode);
        let resolved_sandbox_mode = configured_sandbox_mode
            .or_else(|| {
                // If no sandbox_mode is set but this directory has a trust decision,
                // default to workspace-write except on unsandboxed Windows where we
                // default to read-only.
                active_project
                    .filter(|project| project.is_trusted() || project.is_untrusted())
                    .map(|_| {
                        if cfg!(target_os = "windows")
                            && windows_sandbox_level == WindowsSandboxLevel::Disabled
                        {
                            SandboxMode::ReadOnly
                        } else {
                            SandboxMode::WorkspaceWrite
                        }
                    })
            })
            .unwrap_or_default();
        let effective_sandbox_mode = if cfg!(target_os = "windows")
            // If the experimental Windows sandbox is enabled, do not force a downgrade.
            && windows_sandbox_level == WindowsSandboxLevel::Disabled
            && matches!(resolved_sandbox_mode, SandboxMode::WorkspaceWrite)
        {
            SandboxMode::ReadOnly
        } else {
            resolved_sandbox_mode
        };

        let permission_profile = match effective_sandbox_mode {
            SandboxMode::ReadOnly => PermissionProfile::read_only(),
            SandboxMode::WorkspaceWrite => match self.sandbox_workspace_write.as_ref() {
                Some(SandboxWorkspaceWrite {
                    writable_roots,
                    network_access,
                    exclude_tmpdir_env_var,
                    exclude_slash_tmp,
                }) => {
                    let network_policy = if *network_access {
                        NetworkSandboxPolicy::Enabled
                    } else {
                        NetworkSandboxPolicy::Restricted
                    };
                    PermissionProfile::workspace_write_with(
                        writable_roots,
                        network_policy,
                        *exclude_tmpdir_env_var,
                        *exclude_slash_tmp,
                    )
                }
                None => PermissionProfile::workspace_write(),
            },
            SandboxMode::DangerFullAccess => PermissionProfile::Disabled,
        };
        if configured_sandbox_mode.is_none()
            && let Some(constraint) = permission_profile_constraint
            && let Err(err) = constraint.can_set(&permission_profile)
        {
            tracing::warn!(
                error = %err,
                "default sandbox policy is disallowed by requirements; falling back to required default"
            );
            PermissionProfile::read_only()
        } else {
            permission_profile
        }
    }

    /// Resolves the cwd to an existing project, or returns None if ConfigToml
    /// does not contain a project corresponding to cwd or the resolved git repo
    /// root for cwd.
    pub fn get_active_project(
        &self,
        resolved_cwd: &Path,
        repo_root: Option<&Path>,
    ) -> Option<ProjectConfig> {
        let projects = self.projects.as_ref()?;

        for normalized_cwd in normalized_project_lookup_keys(resolved_cwd) {
            if let Some(project_config) = project_config_for_lookup_key(projects, &normalized_cwd) {
                return Some(project_config);
            }
        }

        if let Some(repo_root) = repo_root {
            for normalized_repo_root in normalized_project_lookup_keys(repo_root) {
                if let Some(project_config_for_root) =
                    project_config_for_lookup_key(projects, &normalized_repo_root)
                {
                    return Some(project_config_for_root);
                }
            }
        }

        None
    }
}

/// Canonicalize the path and convert it to a string to be used as a key in the
/// projects trust map. On Windows, strips UNC, when possible, to try to ensure
/// that different paths that point to the same location have the same key.
fn normalized_project_lookup_keys(path: &Path) -> Vec<String> {
    let normalized_path = normalize_project_lookup_key(path.to_string_lossy().to_string());
    let normalized_canonical_path = normalize_project_lookup_key(
        normalize_for_path_comparison(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .to_string(),
    );
    if normalized_path == normalized_canonical_path {
        vec![normalized_canonical_path]
    } else {
        vec![normalized_canonical_path, normalized_path]
    }
}

fn normalize_project_lookup_key(key: String) -> String {
    if cfg!(windows) {
        key.to_ascii_lowercase()
    } else {
        key
    }
}

fn project_config_for_lookup_key(
    projects: &HashMap<String, ProjectConfig>,
    lookup_key: &str,
) -> Option<ProjectConfig> {
    if let Some(project_config) = projects.get(lookup_key) {
        return Some(project_config.clone());
    }

    let mut normalized_matches: Vec<_> = projects
        .iter()
        .filter(|(key, _)| normalize_project_lookup_key((*key).clone()) == lookup_key)
        .collect();
    normalized_matches.sort_by_key(|(key, _)| *key);
    normalized_matches
        .first()
        .map(|(_, project_config)| (**project_config).clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const WORKSPACE_ID_A: &str = "123e4567-e89b-42d3-a456-426614174000";
    const WORKSPACE_ID_B: &str = "123e4567-e89b-42d3-a456-426614174001";

    #[test]
    fn forced_chatgpt_workspace_id_accepts_single_string() {
        let config: ConfigToml = toml::from_str(&format!(
            r#"forced_chatgpt_workspace_id = "{WORKSPACE_ID_A}""#
        ))
        .expect("single workspace id should deserialize");

        assert_eq!(
            config
                .forced_chatgpt_workspace_id
                .expect("workspace id should be set")
                .into_vec(),
            vec![WORKSPACE_ID_A.to_string()]
        );
    }

    #[test]
    fn forced_chatgpt_workspace_id_accepts_string_list() {
        let config: ConfigToml = toml::from_str(&format!(
            r#"forced_chatgpt_workspace_id = ["{WORKSPACE_ID_A}", "{WORKSPACE_ID_B}"]"#
        ))
        .expect("workspace id list should deserialize");

        assert_eq!(
            config
                .forced_chatgpt_workspace_id
                .expect("workspace ids should be set")
                .into_vec(),
            vec![WORKSPACE_ID_A.to_string(), WORKSPACE_ID_B.to_string()]
        );
    }

    #[test]
    fn forced_chatgpt_workspace_id_rejects_comma_separated_string() {
        let err = toml::from_str::<ConfigToml>(&format!(
            r#"forced_chatgpt_workspace_id = "{WORKSPACE_ID_A},{WORKSPACE_ID_B}""#
        ))
        .expect_err("comma-separated string should be rejected");

        let message = err.to_string();
        assert!(message.contains("TOML list of strings"));
        assert!(message.contains("comma-separated strings are not supported"));
    }
}
