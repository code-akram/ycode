//! Centralized feature flags and metadata.
//!
//! This crate defines the feature registry plus the logic used to resolve an
//! effective feature set from config-like inputs.

use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use toml::Table;

mod feature_configs;
pub use feature_configs::CodeModeConfigToml;
pub use feature_configs::CodeModeHostConfigToml;
pub use feature_configs::CurrentTimeReminderConfigToml;
pub use feature_configs::CurrentTimeReminderDeliveryMode;
pub use feature_configs::CurrentTimeSource;
pub use feature_configs::MultiAgentV2ConfigToml;
pub use feature_configs::NetworkProxyConfigToml;
pub use feature_configs::NetworkProxyDomainPermissionToml;
pub use feature_configs::NetworkProxyModeToml;
pub use feature_configs::NetworkProxyUnixSocketPermissionToml;
pub use feature_configs::RolloutBudgetConfigToml;
pub use feature_configs::TokenBudgetConfigToml;
pub use feature_configs::TokenBudgetMode;
pub use feature_configs::ToolRegistryConfigToml;

/// High-level lifecycle stage for a feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Features that are still under development, not ready for external use
    UnderDevelopment,
    /// Experimental features made available to users through the `/experimental` menu
    Experimental {
        name: &'static str,
        menu_description: &'static str,
        announcement: &'static str,
    },
    /// Stable features. The feature flag is kept for ad-hoc enabling/disabling
    Stable,
}

impl Stage {
    pub fn experimental_menu_name(self) -> Option<&'static str> {
        match self {
            Stage::Experimental { name, .. } => Some(name),
            Stage::UnderDevelopment | Stage::Stable => None,
        }
    }

    pub fn experimental_menu_description(self) -> Option<&'static str> {
        match self {
            Stage::Experimental {
                menu_description, ..
            } => Some(menu_description),
            Stage::UnderDevelopment | Stage::Stable => None,
        }
    }

    pub fn experimental_announcement(self) -> Option<&'static str> {
        match self {
            Stage::Experimental {
                announcement: "", ..
            } => None,
            Stage::Experimental { announcement, .. } => Some(announcement),
            Stage::UnderDevelopment | Stage::Stable => None,
        }
    }
}

/// Unique features toggled via configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Feature {
    // Stable.
    /// Enable the default shell tool.
    ShellTool,
    /// Enable the built-in local image viewer.
    ViewImage,
    // Experimental
    /// Record model-attempted tool calls in internal Responses metadata.
    ExecutedToolCallMetadata,
    /// Enable JavaScript code mode backed by the standalone host process.
    CodeMode,
    /// Use a 30-second default yield timeout for code mode exec calls.
    CodeModeBufferedExec,
    /// Run JavaScript code mode in the standalone host process.
    CodeModeHost,
    /// Restrict model-visible tools to code mode entrypoints (`exec`, `wait`).
    CodeModeOnly,
    /// Use the single unified PTY-backed exec tool.
    UnifiedExec,
    /// Route shell tool execution through the zsh exec bridge.
    ShellZshFork,
    /// Allow unified exec to compose with the zsh exec bridge.
    ///
    /// This flag is only a composition gate. Enabling it by itself must not turn
    /// on either `unified_exec` or `shell_zsh_fork` because those features have
    /// separate rollout and enterprise controls.
    UnifiedExecZshFork,
    /// Add terminal-specific visualization guidance to TUI developer instructions.
    TerminalVisualizationInstructions,
    /// Stream structured progress while apply_patch input is being generated.
    ApplyPatchStreamingEvents,
    /// Allow exec tools to request additional permissions while staying sandboxed.
    ExecPermissionApprovals,
    /// Expose the built-in request_permissions tool.
    RequestPermissionsTool,
    /// Expose the extension-backed standalone web search tool.
    StandaloneWebSearch,
    /// Experimental shell snapshotting.
    ShellSnapshot,
    /// Allow turns to start while selected executors are still starting.
    DeferredExecutor,
    /// Enable startup memory extraction and file-backed memory consolidation.
    MemoryTool,
    /// Compress cold local thread-store rollout files.
    LocalThreadStoreCompression,
    /// Enable the Chronicle sidecar for passive screen-context memories.
    Chronicle,
    /// Compress request bodies (zstd) when sending streaming requests to codex-backend.
    EnableRequestCompression,
    /// Start the managed network proxy for sandboxed sessions.
    NetworkProxy,
    /// Respect host system proxy settings for Codex-owned network clients.
    RespectSystemProxy,
    /// Enable collab tools.
    Collab,
    /// Enable task-path-based multi-agent routing.
    MultiAgentV2,
    /// Describe deferred tool namespaces in the model-visible world state.
    DeferredToolWorldState,
    /// Enable discoverable tool suggestions for apps.
    ToolSuggest,
    /// Discover selected-root skill manifests through one high-level exec-server RPC.
    ExecutorCapabilityDiscovery,
    /// Allow the in-app browser pane in desktop apps.
    ///
    /// Requirements-only gate: this should be set from requirements, not user config.
    InAppBrowser,
    /// Allow Browser Use agent integration in desktop apps.
    ///
    /// Requirements-only gate: this should be set from requirements, not user config.
    BrowserUse,
    /// Allow Browser Use integration to access the full Chrome DevTools Protocol surface.
    ///
    /// Requirements-only gate: this should be set from requirements, not user config.
    BrowserUseFullCdpAccess,
    /// Allow Browser Use integration with external browsers.
    ///
    /// Requirements-only gate: this should be set from requirements, not user config.
    BrowserUseExternal,
    /// Allow Codex Computer Use.
    ///
    /// Requirements-only gate: this should be set from requirements, not user config.
    ComputerUse,
    /// Enable extension-backed image generation.
    ImageGeneration,
    /// Tell the model when a prompt image was resized and include its dimensions.
    ImageResizeNotice,
    /// Request sequential cutoff reasoning summary delivery.
    ConcurrentReasoningSummaries,
    /// Enable search across retained filesystem-backed skill providers.
    SkillSearch,
    /// Enable the unified mention popup used by default in the TUI.
    MentionsV2,
    /// Enable automatic review for approval prompts.
    GuardianApproval,
    /// Enable Guardian V2 automatic approval reviews.
    GuardianV2,
    /// Enable persisted thread goals and automatic goal continuation.
    Goals,
    /// Add current context-window metadata to model-visible context.
    TokenBudget,
    /// Track and report a shared token budget across a session's agent threads.
    RolloutBudget,
    /// Add current-time reminders to model-visible context.
    CurrentTimeReminder,
    /// Enable personality selection in the TUI.
    Personality,
    /// Enable native artifact tools.
    Artifact,
    /// Enable Fast mode selection in the TUI and request layer.
    FastMode,
    /// Enable experimental realtime voice conversation mode in the TUI.
    RealtimeConversation,
    /// Prevent idle system sleep while a turn is actively running.
    PreventIdleSleep,
    /// Enable remote compaction v2 over the normal Responses API.
    RemoteCompactionV2,
    /// Use Agent Identity for ChatGPT-authenticated sessions.
    UseAgentIdentity,
    /// Enable workspace dependency support.
    WorkspaceDependencies,
}

impl Feature {
    pub fn key(self) -> &'static str {
        self.info().key
    }

    pub fn stage(self) -> Stage {
        self.info().stage
    }

    pub fn default_enabled(self) -> bool {
        self.info().default_enabled
    }

    fn info(self) -> &'static FeatureSpec {
        FEATURES
            .iter()
            .find(|spec| spec.id == self)
            .unwrap_or_else(|| unreachable!("missing FeatureSpec for {self:?}"))
    }
}

/// Holds the effective set of enabled features.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Features {
    enabled: BTreeSet<Feature>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FeatureConfigSource<'a> {
    pub features: Option<&'a FeaturesToml>,
}

impl Features {
    /// Starts with built-in defaults.
    pub fn with_defaults() -> Self {
        let mut set = BTreeSet::new();
        for spec in FEATURES {
            if spec.default_enabled {
                set.insert(spec.id);
            }
        }
        Self { enabled: set }
    }

    pub fn enabled(&self, f: Feature) -> bool {
        self.enabled.contains(&f)
    }

    pub fn enable(&mut self, f: Feature) -> &mut Self {
        self.enabled.insert(f);
        self
    }

    pub fn disable(&mut self, f: Feature) -> &mut Self {
        self.enabled.remove(&f);
        self
    }

    pub fn set_enabled(&mut self, f: Feature, enabled: bool) -> &mut Self {
        if enabled {
            self.enable(f)
        } else {
            self.disable(f)
        }
    }

    /// Apply a table of key -> bool toggles (e.g. from TOML).
    pub fn apply_map(&mut self, m: &BTreeMap<String, bool>) {
        for (k, v) in m {
            match feature_for_key(k) {
                Some(feat) => {
                    if *v {
                        self.enable(feat);
                    } else {
                        self.disable(feat);
                    }
                }
                None => {
                    tracing::warn!("unknown feature key in config: {k}");
                }
            }
        }
    }

    pub fn from_sources(base: FeatureConfigSource<'_>, profile: FeatureConfigSource<'_>) -> Self {
        let mut features = Features::with_defaults();

        for source in [base, profile] {
            if let Some(feature_entries) = source.features {
                features.apply_toml(feature_entries);
            }
        }

        features.normalize_dependencies();

        features
    }

    pub fn enabled_features(&self) -> Vec<Feature> {
        self.enabled.iter().copied().collect()
    }

    pub fn normalize_dependencies(&mut self) {
        if self.enabled(Feature::CodeModeOnly) && !self.enabled(Feature::CodeMode) {
            self.enable(Feature::CodeMode);
        }
    }
}

/// Keys accepted in `[features]` tables.
pub fn feature_for_key(key: &str) -> Option<Feature> {
    canonical_feature_for_key(key)
}

pub fn canonical_feature_for_key(key: &str) -> Option<Feature> {
    FEATURES
        .iter()
        .find(|spec| spec.key == key)
        .map(|spec| spec.id)
}

/// Returns `true` if the provided string matches a known `[features]` key.
pub fn is_known_feature_key(key: &str) -> bool {
    key == "tool_registry" || feature_for_key(key).is_some()
}

/// Deserializable features table for TOML.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
pub struct FeaturesToml {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_registry: Option<ToolRegistryConfigToml>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_mode: Option<FeatureToml<CodeModeConfigToml>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_mode_host: Option<FeatureToml<CodeModeHostConfigToml>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_agent_v2: Option<FeatureToml<MultiAgentV2ConfigToml>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<FeatureToml<TokenBudgetConfigToml>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_budget: Option<FeatureToml<RolloutBudgetConfigToml>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_time_reminder: Option<FeatureToml<CurrentTimeReminderConfigToml>>,
    pub network_proxy: Option<FeatureToml<NetworkProxyConfigToml>>,
    /// Boolean feature toggles keyed by canonical feature name.
    #[serde(flatten)]
    entries: BTreeMap<String, bool>,
}

impl Features {
    fn apply_toml(&mut self, features: &FeaturesToml) {
        let entries = features.entries();
        self.apply_map(&entries);
    }
}

impl FeaturesToml {
    pub fn entries(&self) -> BTreeMap<String, bool> {
        let mut entries = self.entries.clone();
        if let Some(enabled) = self.code_mode.as_ref().and_then(FeatureToml::enabled) {
            entries.insert(Feature::CodeMode.key().to_string(), enabled);
        }
        if let Some(enabled) = self.code_mode_host.as_ref().and_then(FeatureToml::enabled) {
            entries.insert(Feature::CodeModeHost.key().to_string(), enabled);
        }
        if let Some(enabled) = self.multi_agent_v2.as_ref().and_then(FeatureToml::enabled) {
            entries.insert(Feature::MultiAgentV2.key().to_string(), enabled);
        }
        if let Some(enabled) = self.token_budget.as_ref().and_then(FeatureToml::enabled) {
            entries.insert(Feature::TokenBudget.key().to_string(), enabled);
        }
        if let Some(enabled) = self.rollout_budget.as_ref().and_then(FeatureToml::enabled) {
            entries.insert(Feature::RolloutBudget.key().to_string(), enabled);
        }
        if let Some(enabled) = self
            .current_time_reminder
            .as_ref()
            .and_then(FeatureToml::enabled)
        {
            entries.insert(Feature::CurrentTimeReminder.key().to_string(), enabled);
        }
        if let Some(enabled) = self.network_proxy.as_ref().and_then(FeatureToml::enabled) {
            entries.insert(Feature::NetworkProxy.key().to_string(), enabled);
        }
        entries
    }

    pub fn materialize_resolved_enabled(&mut self, features: &Features) {
        let Self {
            tool_registry: _,
            code_mode,
            code_mode_host,
            multi_agent_v2,
            token_budget,
            rollout_budget,
            current_time_reminder,
            network_proxy,
            entries,
        } = self;
        for spec in FEATURES {
            let enabled = features.enabled(spec.id);
            if spec.id == Feature::CodeMode {
                materialize_resolved_feature_enabled(code_mode, enabled);
            } else if spec.id == Feature::CodeModeHost {
                materialize_resolved_feature_enabled(code_mode_host, enabled);
            } else if spec.id == Feature::MultiAgentV2 {
                materialize_resolved_feature_enabled(multi_agent_v2, enabled);
            } else if spec.id == Feature::TokenBudget {
                materialize_resolved_feature_enabled(token_budget, enabled);
            } else if spec.id == Feature::RolloutBudget {
                materialize_resolved_feature_enabled(rollout_budget, enabled);
            } else if spec.id == Feature::CurrentTimeReminder {
                materialize_resolved_feature_enabled(current_time_reminder, enabled);
            } else if spec.id == Feature::NetworkProxy {
                materialize_resolved_feature_enabled(network_proxy, enabled);
            } else {
                entries.insert(spec.key.to_string(), enabled);
            }
        }
    }
}

fn materialize_resolved_feature_enabled<T: FeatureConfig>(
    feature: &mut Option<FeatureToml<T>>,
    enabled: bool,
) {
    match feature {
        Some(feature) => feature.set_enabled(enabled),
        None => *feature = Some(FeatureToml::Enabled(enabled)),
    }
}

impl From<BTreeMap<String, bool>> for FeaturesToml {
    fn from(entries: BTreeMap<String, bool>) -> Self {
        Self {
            entries,
            ..Default::default()
        }
    }
}

// To be used for features that need more configuration than just enabled/disabled and
// require a custom config struct under `[features]`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(untagged)]
pub enum FeatureToml<T> {
    Enabled(bool),
    Config(T),
}

impl<T: FeatureConfig> FeatureToml<T> {
    pub fn enabled(&self) -> Option<bool> {
        match self {
            Self::Enabled(enabled) => Some(*enabled),
            Self::Config(config) => config.enabled(),
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        match self {
            Self::Enabled(value) => *value = enabled,
            Self::Config(config) => config.set_enabled(enabled),
        }
    }
}

// A trait to be implemented by custom feature config structs when defining a feature that needs more configuration than
// just enabled/disabled.
pub trait FeatureConfig {
    fn enabled(&self) -> Option<bool>;
    fn set_enabled(&mut self, enabled: bool);
}

/// Single, easy-to-read registry of all feature definitions.
#[derive(Debug, Clone, Copy)]
pub struct FeatureSpec {
    pub id: Feature,
    pub key: &'static str,
    pub stage: Stage,
    pub default_enabled: bool,
}

pub const FEATURES: &[FeatureSpec] = &[
    // Stable features.
    FeatureSpec {
        id: Feature::ShellTool,
        key: "shell_tool",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ViewImage,
        key: "view_image",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::UnifiedExec,
        key: "unified_exec",
        stage: Stage::Stable,
        default_enabled: !cfg!(windows),
    },
    FeatureSpec {
        id: Feature::ShellZshFork,
        key: "shell_zsh_fork",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::UnifiedExecZshFork,
        key: "unified_exec_zsh_fork",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ShellSnapshot,
        key: "shell_snapshot",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::DeferredExecutor,
        key: "deferred_executor",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ExecutedToolCallMetadata,
        key: "executed_tool_call_metadata",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::CodeMode,
        key: "code_mode",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::CodeModeBufferedExec,
        key: "code_mode_buffered_exec",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::CodeModeHost,
        key: "code_mode_host",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::CodeModeOnly,
        key: "code_mode_only",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::StandaloneWebSearch,
        key: "standalone_web_search",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::MemoryTool,
        key: "memories",
        stage: Stage::Stable,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::LocalThreadStoreCompression,
        key: "local_thread_store_compression",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::Chronicle,
        key: "chronicle",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ApplyPatchStreamingEvents,
        key: "apply_patch_streaming_events",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ExecPermissionApprovals,
        key: "exec_permission_approvals",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::RequestPermissionsTool,
        key: "request_permissions_tool",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::EnableRequestCompression,
        key: "enable_request_compression",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::NetworkProxy,
        key: "network_proxy",
        stage: Stage::Experimental {
            name: "Network proxy",
            menu_description: "Apply network proxy restrictions to sandboxed sessions that already have network access.",
            announcement: "NEW: Network proxy can now be enabled from /experimental. Restart Codex after enabling it.",
        },
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::RespectSystemProxy,
        key: "respect_system_proxy",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::Collab,
        key: "multi_agent",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::MultiAgentV2,
        key: "multi_agent_v2",
        stage: Stage::Stable,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::DeferredToolWorldState,
        key: "deferred_tool_world_state",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ToolSuggest,
        key: "tool_suggest",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ExecutorCapabilityDiscovery,
        key: "executor_capability_discovery",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::InAppBrowser,
        key: "in_app_browser",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::BrowserUse,
        key: "browser_use",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::BrowserUseFullCdpAccess,
        key: "browser_use_full_cdp_access",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::BrowserUseExternal,
        key: "browser_use_external",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ComputerUse,
        key: "computer_use",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ImageGeneration,
        key: "image_generation",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ImageResizeNotice,
        key: "image_resize_notice",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ConcurrentReasoningSummaries,
        key: "concurrent_reasoning_summaries",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::SkillSearch,
        key: "skill_search",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::MentionsV2,
        key: "mentions_v2",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::TerminalVisualizationInstructions,
        key: "terminal_visualization_instructions",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::GuardianApproval,
        key: "guardian_approval",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::GuardianV2,
        key: "guardianv2",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::Goals,
        key: "goals",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::TokenBudget,
        key: "token_budget",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::RolloutBudget,
        key: "rollout_budget",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::CurrentTimeReminder,
        key: "current_time_reminder",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::Personality,
        key: "personality",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::Artifact,
        key: "artifact",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::FastMode,
        key: "fast_mode",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::RealtimeConversation,
        key: "realtime_conversation",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::PreventIdleSleep,
        key: "prevent_idle_sleep",
        stage: Stage::Experimental {
            name: "Prevent sleep while running",
            menu_description: "Keep your computer awake while Codex is running a thread.",
            announcement: "NEW: Prevent sleep while running is now available in /experimental.",
        },
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::RemoteCompactionV2,
        key: "remote_compaction_v2",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::UseAgentIdentity,
        key: "use_agent_identity",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::WorkspaceDependencies,
        key: "workspace_dependencies",
        stage: Stage::Stable,
        default_enabled: true,
    },
];

pub fn unstable_features_warning_event(
    effective_features: Option<&Table>,
    suppress_unstable_features_warning: bool,
    features: &Features,
    config_path: &str,
) -> Option<Event> {
    if suppress_unstable_features_warning {
        return None;
    }

    let mut under_development_feature_keys = Vec::new();
    if let Some(table) = effective_features {
        for (key, value) in table {
            let is_enabled = value.as_bool() == Some(true)
                || value
                    .as_table()
                    .and_then(|table| table.get("enabled"))
                    .and_then(toml::Value::as_bool)
                    == Some(true);
            if !is_enabled {
                continue;
            }
            let Some(spec) = FEATURES.iter().find(|spec| spec.key == key.as_str()) else {
                continue;
            };
            if !features.enabled(spec.id) {
                continue;
            }
            if matches!(spec.stage, Stage::UnderDevelopment) {
                under_development_feature_keys.push(spec.key.to_string());
            }
        }
    }

    if under_development_feature_keys.is_empty() {
        return None;
    }

    under_development_feature_keys.sort();
    let under_development_feature_keys = under_development_feature_keys.join(", ");
    let message = format!(
        "Under-development features enabled: {under_development_feature_keys}. Under-development features are incomplete and may behave unpredictably. To suppress this warning, set `suppress_unstable_features_warning = true` in {config_path}."
    );
    Some(Event {
        id: String::new(),
        msg: EventMsg::Warning(WarningEvent { message }),
    })
}

#[cfg(test)]
mod tests;
