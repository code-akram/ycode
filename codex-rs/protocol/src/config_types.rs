use codex_utils_absolute_path::AbsolutePathBuf;
use schemars::JsonSchema;
use schemars::r#gen::SchemaGenerator;
use schemars::schema::InstanceType;
use schemars::schema::Metadata;
use schemars::schema::Schema;
use schemars::schema::SchemaObject;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU64;
use std::ops::Deref;
use std::str::FromStr;
use std::time::Duration;
use strum_macros::Display;
use strum_macros::EnumIter;
use ts_rs::TS;
use wildmatch::WildMatchPattern;

use crate::openai_models::ReasoningEffort;

/// Selects which part of the active context is charged against
/// `model_auto_compact_token_limit`.
#[derive(
    Debug, Serialize, Deserialize, Default, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AutoCompactTokenLimitScope {
    /// Count the full active context against the limit.
    #[default]
    Total,
    /// Count sampled output and later growth after the carried window prefix.
    BodyAfterPrefix,
}

/// A summary of the reasoning performed by the model. This can be useful for
/// debugging and understanding the model's reasoning process.
/// See https://platform.openai.com/docs/guides/reasoning?api-mode=responses#reasoning-summaries
#[derive(
    Debug, Serialize, Deserialize, Default, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ReasoningSummary {
    #[default]
    Auto,
    Concise,
    Detailed,
    /// Option to disable reasoning summaries.
    None,
}

/// Controls output length/detail on GPT-5 models via the Responses API.
/// Serialized with lowercase values to match the OpenAI API.
#[derive(
    Hash,
    Debug,
    Serialize,
    Deserialize,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Display,
    JsonSchema,
    TS,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Verbosity {
    Low,
    #[default]
    Medium,
    High,
}

#[derive(
    Deserialize, Debug, Clone, Copy, PartialEq, Default, Serialize, Display, JsonSchema, TS,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum SandboxMode {
    #[serde(rename = "read-only")]
    #[default]
    ReadOnly,

    #[serde(rename = "workspace-write")]
    WorkspaceWrite,

    #[serde(rename = "danger-full-access")]
    DangerFullAccess,
}

/// Validated plain profile-v2 name used to select `$CODEX_HOME/<name>.config.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileV2Name(String);

impl ProfileV2Name {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ProfileV2NameParseError {
    value: String,
}

impl fmt::Display for ProfileV2NameParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid --profile value `{}`; pass a plain name such as `work`",
            self.value
        )
    }
}

impl std::error::Error for ProfileV2NameParseError {}

impl FromStr for ProfileV2Name {
    type Err = ProfileV2NameParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ProfileV2NameParseError {
                value: value.to_string(),
            });
        }

        Ok(Self(value.to_string()))
    }
}

impl Deref for ProfileV2Name {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for ProfileV2Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Display, TS)]
#[strum(serialize_all = "snake_case")]
#[ts(type = r#""user" | "auto_review" | "guardian_subagent""#)]
/// Configures who approval requests are routed to for review. Examples
/// include sandbox escapes, blocked network access, tool approval prompts, and
/// ARC escalations. Defaults to `user`. `auto_review` uses a carefully
/// prompted subagent to gather relevant context and apply a risk-based
/// decision framework before approving or denying the request.
pub enum ApprovalsReviewer {
    #[default]
    #[serde(rename = "user")]
    User,
    #[serde(rename = "auto_review", alias = "guardian_subagent")]
    #[strum(serialize = "auto_review")]
    AutoReview,
}

impl JsonSchema for ApprovalsReviewer {
    fn schema_name() -> String {
        "ApprovalsReviewer".to_string()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        string_enum_schema_with_description(
            &["user", "auto_review", "guardian_subagent"],
            "Configures who approval requests are routed to for review. Examples include sandbox escapes, blocked network access, tool approval prompts, and ARC escalations. Defaults to `user`. `auto_review` uses a carefully prompted subagent to gather relevant context and apply a risk-based decision framework before approving or denying the request. The legacy value `guardian_subagent` is accepted for compatibility.",
        )
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ShellEnvironmentPolicyInherit {
    /// "Core" environment variables for the platform. On UNIX, this would
    /// include HOME, LOGNAME, PATH, SHELL, and USER, among others.
    Core,

    /// Inherits the full environment from the parent process.
    #[default]
    All,

    /// Do not inherit any environment variables from the parent process.
    None,
}

/// Assigns a shell environment variable pattern to the include-only or exclude
/// set. Includes do not re-add variables removed by another exclude pattern.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export_to = "v2/")]
pub enum ShellEnvironmentPolicyFilter {
    Include,
    Exclude,
}

pub type EnvironmentVariablePattern = WildMatchPattern<'*', '?'>;

/// Deriving the `env` based on this policy works as follows:
/// 1. Create an initial map based on the `inherit` policy.
/// 2. If `ignore_default_excludes` is false, filter the map using the default
///    exclude pattern(s), which are: `"*KEY*"`, `"*SECRET*"`, and `"*TOKEN*"`.
/// 3. If `exclude` is not empty, filter the map using the provided patterns.
/// 4. Insert any entries from `r#set` into the map.
/// 5. If non-empty, filter the map using the `include_only` patterns.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellEnvironmentPolicy {
    /// Starting point when building the environment.
    pub inherit: ShellEnvironmentPolicyInherit,

    /// True to skip the check to exclude default environment variables that
    /// contain "KEY", "SECRET", or "TOKEN" in their name. Defaults to true.
    pub ignore_default_excludes: bool,

    /// Environment variable names to exclude from the environment.
    pub exclude: Vec<EnvironmentVariablePattern>,

    /// (key, value) pairs to insert in the environment.
    pub r#set: HashMap<String, String>,

    /// Environment variable names to retain in the environment.
    pub include_only: Vec<EnvironmentVariablePattern>,

    /// If true, the shell profile will be used to run the command.
    pub use_profile: bool,
}

impl Default for ShellEnvironmentPolicy {
    fn default() -> Self {
        Self {
            inherit: ShellEnvironmentPolicyInherit::All,
            ignore_default_excludes: true,
            exclude: Vec::new(),
            r#set: HashMap::new(),
            include_only: Vec::new(),
            use_profile: false,
        }
    }
}

fn string_enum_schema_with_description(values: &[&str], description: &str) -> Schema {
    let mut schema = SchemaObject {
        instance_type: Some(InstanceType::String.into()),
        metadata: Some(Box::new(Metadata {
            description: Some(description.to_string()),
            ..Default::default()
        })),
        ..Default::default()
    };
    schema.enum_values = Some(
        values
            .iter()
            .map(|value| Value::String((*value).to_string()))
            .collect(),
    );
    Schema::Object(schema)
}

#[derive(
    Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Display, JsonSchema, TS,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum WindowsSandboxLevel {
    #[default]
    Disabled,
    RestrictedToken,
    Elevated,
}

/// Controls whether a Windows sandbox launch reconciles persistent proxy settings or preserves
/// the settings established by another launch.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WindowsSandboxProxySettingsMode {
    #[default]
    Reconcile,
    Preserve,
}

#[derive(
    Debug,
    Serialize,
    Deserialize,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Display,
    JsonSchema,
    TS,
    PartialOrd,
    Ord,
    EnumIter,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Personality {
    None,
    Friendly,
    Pragmatic,
}

/// Controls the effective multi-agent delegation instructions for a turn. `custom` means the
/// configured mode hint defines the policy instead of a built-in policy.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Display, JsonSchema, TS, Default)]
#[serde(rename_all = "camelCase", from = "MultiAgentModeWire")]
#[ts(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum MultiAgentMode {
    Custom(String),
    #[default]
    ExplicitRequestOnly,
    Proactive,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum MultiAgentModeWire {
    None,
    Custom(String),
    ExplicitRequestOnly,
    Proactive,
}

impl From<MultiAgentModeWire> for MultiAgentMode {
    fn from(value: MultiAgentModeWire) -> Self {
        match value {
            MultiAgentModeWire::None => Self::Custom(String::new()),
            MultiAgentModeWire::Custom(hint_text) => Self::Custom(hint_text),
            MultiAgentModeWire::ExplicitRequestOnly => Self::ExplicitRequestOnly,
            MultiAgentModeWire::Proactive => Self::Proactive,
        }
    }
}

#[derive(
    Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS, Default,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum WebSearchMode {
    Disabled,
    #[default]
    Cached,
    Indexed,
    Live,
}

/// A model-facing surface on which a tool can be exposed.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ToolExposureSurface {
    /// Nested tools available to Code Mode scripts.
    CodeMode,
    /// Tools discovered later through tool search.
    Deferred,
    /// Tools present in the model's initial tool list.
    Direct,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum WebSearchContextSize {
    Low,
    Medium,
    High,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchLocation {
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub timezone: Option<String>,
}

impl WebSearchLocation {
    pub fn merge(&self, other: &Self) -> Self {
        Self {
            country: other.country.clone().or_else(|| self.country.clone()),
            region: other.region.clone().or_else(|| self.region.clone()),
            city: other.city.clone().or_else(|| self.city.clone()),
            timezone: other.timezone.clone().or_else(|| self.timezone.clone()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchToolConfig {
    pub context_size: Option<WebSearchContextSize>,
    pub allowed_domains: Option<Vec<String>>,
    pub location: Option<WebSearchLocation>,
}

impl WebSearchToolConfig {
    pub fn merge(&self, other: &Self) -> Self {
        Self {
            context_size: other.context_size.or(self.context_size),
            allowed_domains: other
                .allowed_domains
                .clone()
                .or_else(|| self.allowed_domains.clone()),
            location: match (&self.location, &other.location) {
                (Some(location), Some(other_location)) => Some(location.merge(other_location)),
                (Some(location), None) => Some(location.clone()),
                (None, Some(other_location)) => Some(other_location.clone()),
                (None, None) => None,
            },
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchFilters {
    pub allowed_domains: Option<Vec<String>>,
}

#[derive(
    Debug, Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq, Display, JsonSchema, TS,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum WebSearchUserLocationType {
    #[default]
    Approximate,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchUserLocation {
    #[serde(default)]
    pub r#type: WebSearchUserLocationType,
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub timezone: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchConfig {
    pub filters: Option<WebSearchFilters>,
    pub user_location: Option<WebSearchUserLocation>,
    pub search_context_size: Option<WebSearchContextSize>,
}

impl From<WebSearchLocation> for WebSearchUserLocation {
    fn from(location: WebSearchLocation) -> Self {
        Self {
            r#type: WebSearchUserLocationType::Approximate,
            country: location.country,
            region: location.region,
            city: location.city,
            timezone: location.timezone,
        }
    }
}

impl From<WebSearchToolConfig> for WebSearchConfig {
    fn from(config: WebSearchToolConfig) -> Self {
        Self {
            filters: config
                .allowed_domains
                .map(|allowed_domains| WebSearchFilters {
                    allowed_domains: Some(allowed_domains),
                }),
            user_location: config.location.map(Into::into),
            search_context_size: config.context_size,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ServiceTier {
    Fast,
    Flex,
}

/// Request/config sentinel for explicit standard routing.
///
/// This is not a catalog service tier id. It means the user intentionally
/// selected no service tier, so model catalog defaults should not apply.
pub const SERVICE_TIER_DEFAULT_REQUEST_VALUE: &str = "default";

impl ServiceTier {
    pub const fn request_value(self) -> &'static str {
        match self {
            Self::Fast => "priority",
            Self::Flex => "flex",
        }
    }

    pub fn from_request_value(value: &str) -> Option<Self> {
        match value {
            "fast" | "priority" => Some(Self::Fast),
            "flex" => Some(Self::Flex),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ForcedLoginMethod {
    Chatgpt,
    Api,
}

const DEFAULT_PROVIDER_AUTH_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_PROVIDER_AUTH_REFRESH_INTERVAL_MS: u64 = 300_000;

/// Configuration for obtaining a provider bearer token from a command.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelProviderAuthInfo {
    /// Command to execute. Bare names are resolved via `PATH`; paths are resolved against `cwd`.
    pub command: String,

    /// Command arguments.
    #[serde(default)]
    pub args: Vec<String>,

    /// Maximum time to wait for the token command to exit successfully.
    #[serde(default = "default_provider_auth_timeout_ms")]
    pub timeout_ms: NonZeroU64,

    /// Maximum age for the cached token before rerunning the command.
    /// Set to `0` to disable proactive refresh and only rerun after a 401 retry path.
    #[serde(default = "default_provider_auth_refresh_interval_ms")]
    pub refresh_interval_ms: u64,

    /// Working directory used when running the token command.
    #[serde(default = "default_provider_auth_cwd")]
    #[schemars(skip_serializing_if = "is_default_provider_auth_cwd")]
    pub cwd: AbsolutePathBuf,
}

impl ModelProviderAuthInfo {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.get())
    }

    pub fn refresh_interval(&self) -> Option<Duration> {
        NonZeroU64::new(self.refresh_interval_ms).map(|value| Duration::from_millis(value.get()))
    }
}

fn default_provider_auth_timeout_ms() -> NonZeroU64 {
    non_zero_u64(
        DEFAULT_PROVIDER_AUTH_TIMEOUT_MS,
        "model_providers.<id>.auth.timeout_ms",
    )
}

fn default_provider_auth_refresh_interval_ms() -> u64 {
    DEFAULT_PROVIDER_AUTH_REFRESH_INTERVAL_MS
}

fn non_zero_u64(value: u64, field_name: &str) -> NonZeroU64 {
    match NonZeroU64::new(value) {
        Some(value) => value,
        None => panic!("{field_name} must be non-zero"),
    }
}

fn default_provider_auth_cwd() -> AbsolutePathBuf {
    let deserializer = serde::de::value::StrDeserializer::<serde::de::value::Error>::new(".");
    if let Ok(cwd) = AbsolutePathBuf::deserialize(deserializer) {
        return cwd;
    }

    match AbsolutePathBuf::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => panic!("provider auth cwd must resolve: {err}"),
    }
}

fn is_default_provider_auth_cwd(path: &AbsolutePathBuf) -> bool {
    path == &default_provider_auth_cwd()
}

/// Represents the trust level for a project directory.
/// This determines the approval policy and sandbox mode applied.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum TrustLevel {
    Trusted,
    Untrusted,
}

/// Controls whether the TUI uses the terminal's alternate screen buffer.
///
/// - `auto` (default): Use alternate screen mode.
/// - `always`: Always use alternate screen mode.
/// - `never`: Never use alternate screen mode. Runs in inline mode, preserving scrollback.
///
/// The CLI flag `--no-alt-screen` can override this setting at runtime.
#[derive(
    Debug, Serialize, Deserialize, Default, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum AltScreenMode {
    /// Use alternate screen mode.
    #[default]
    Auto,
    /// Always use alternate screen mode.
    Always,
    /// Never use alternate screen (inline mode only).
    Never,
}

/// Model settings for a Codex session.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema, TS)]
pub struct AgentSettings {
    pub settings: Settings,
}

impl AgentSettings {
    /// Returns a reference to the settings.
    fn settings_ref(&self) -> &Settings {
        &self.settings
    }

    pub fn model(&self) -> &str {
        self.settings_ref().model.as_str()
    }

    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.settings_ref().reasoning_effort.clone()
    }

    /// Updates the agent settings with new model and/or effort values.
    ///
    /// - `model`: `Some(s)` to update the model, `None` to keep the current model
    /// - `effort`: `Some(Some(e))` to set effort to `e`, `Some(None)` to clear effort, `None` to keep current effort
    /// - `developer_instructions`: `Some(Some(s))` to set instructions, `Some(None)` to clear them, `None` to keep current
    ///
    /// Returns a new `AgentSettings` with updated values.
    pub fn with_updates(
        &self,
        model: Option<String>,
        effort: Option<Option<ReasoningEffort>>,
        developer_instructions: Option<Option<String>>,
    ) -> Self {
        let settings = self.settings_ref();
        let updated_settings = Settings {
            model: model.unwrap_or_else(|| settings.model.clone()),
            reasoning_effort: effort.unwrap_or_else(|| settings.reasoning_effort.clone()),
            developer_instructions: developer_instructions
                .unwrap_or_else(|| settings.developer_instructions.clone()),
        };

        AgentSettings {
            settings: updated_settings,
        }
    }
}

/// Settings for an agent turn.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema, TS)]
pub struct Settings {
    pub model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub developer_instructions: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn approvals_reviewer_serializes_auto_review_and_accepts_legacy_guardian_subagent() {
        assert_eq!(ApprovalsReviewer::User.to_string(), "user");
        assert_eq!(
            serde_json::to_string(&ApprovalsReviewer::User).expect("serialize reviewer"),
            "\"user\""
        );
        assert_eq!(
            serde_json::to_string(&ApprovalsReviewer::AutoReview).expect("serialize reviewer"),
            "\"auto_review\""
        );

        for value in ["user", "auto_review", "guardian_subagent"] {
            let json = format!("\"{value}\"");
            let reviewer: ApprovalsReviewer =
                serde_json::from_str(&json).expect("deserialize reviewer");
            let expected = if value == "user" {
                ApprovalsReviewer::User
            } else {
                ApprovalsReviewer::AutoReview
            };
            assert_eq!(expected, reviewer);
        }
    }

    #[test]
    fn profile_v2_name_rejects_paths_and_empty_names() {
        assert_eq!(
            ProfileV2Name::from_str("../foo"),
            Err(ProfileV2NameParseError {
                value: "../foo".to_string(),
            }),
            "dots and slashes are disallowed to prevent reading arbitrary files"
        );
        assert_eq!(
            ProfileV2Name::from_str(""),
            Err(ProfileV2NameParseError {
                value: String::new(),
            }),
            "profile name cannot be empty"
        );
    }

    #[test]
    fn web_search_location_merge_prefers_overlay_values() {
        let base = WebSearchLocation {
            country: Some("US".to_string()),
            region: Some("CA".to_string()),
            city: None,
            timezone: Some("America/Los_Angeles".to_string()),
        };
        let overlay = WebSearchLocation {
            country: None,
            region: Some("WA".to_string()),
            city: Some("Seattle".to_string()),
            timezone: None,
        };

        let expected = WebSearchLocation {
            country: Some("US".to_string()),
            region: Some("WA".to_string()),
            city: Some("Seattle".to_string()),
            timezone: Some("America/Los_Angeles".to_string()),
        };

        assert_eq!(expected, base.merge(&overlay));
    }

    #[test]
    fn web_search_tool_config_merge_prefers_overlay_values() {
        let base = WebSearchToolConfig {
            context_size: Some(WebSearchContextSize::Low),
            allowed_domains: Some(vec!["openai.com".to_string()]),
            location: Some(WebSearchLocation {
                country: Some("US".to_string()),
                region: Some("CA".to_string()),
                city: None,
                timezone: Some("America/Los_Angeles".to_string()),
            }),
        };
        let overlay = WebSearchToolConfig {
            context_size: Some(WebSearchContextSize::High),
            allowed_domains: None,
            location: Some(WebSearchLocation {
                country: None,
                region: Some("WA".to_string()),
                city: Some("Seattle".to_string()),
                timezone: None,
            }),
        };

        let expected = WebSearchToolConfig {
            context_size: Some(WebSearchContextSize::High),
            allowed_domains: Some(vec!["openai.com".to_string()]),
            location: Some(WebSearchLocation {
                country: Some("US".to_string()),
                region: Some("WA".to_string()),
                city: Some("Seattle".to_string()),
                timezone: Some("America/Los_Angeles".to_string()),
            }),
        };

        assert_eq!(expected, base.merge(&overlay));
    }
}
