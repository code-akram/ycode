use std::path::PathBuf;

use codex_cli_protocol::ToolRequestUserInputResponse;
use codex_cli_protocol::UserInput;
use codex_protocol::config_types::AgentSettings;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use serde::Serialize;
use serde_json::Value;

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) enum AppCommand {
    Interrupt,
    CleanBackgroundTerminals,
    RunUserShellCommand {
        command: String,
    },
    UserTurn {
        items: Vec<UserInput>,
        cwd: PathBuf,
        model: String,
        effort: Option<ReasoningEffortConfig>,
        summary: Option<ReasoningSummaryConfig>,
        service_tier: Option<Option<String>>,
        final_output_json_schema: Option<Value>,
        agent_settings: Option<AgentSettings>,
        personality: Option<Personality>,
    },
    OverrideTurnContext {
        cwd: Option<PathBuf>,
        model: Option<String>,
        effort: Option<Option<ReasoningEffortConfig>>,
        summary: Option<ReasoningSummaryConfig>,
        service_tier: Option<Option<String>>,
        agent_settings: Option<AgentSettings>,
        personality: Option<Personality>,
    },
    UserInputAnswer {
        id: String,
        response: ToolRequestUserInputResponse,
    },
    ReloadUserConfig,
    ListSkills {
        cwds: Vec<PathBuf>,
        force_reload: bool,
    },
    Compact,
    SetThreadName {
        name: String,
    },
}

impl AppCommand {
    pub(crate) fn interrupt() -> Self {
        Self::Interrupt
    }

    pub(crate) fn clean_background_terminals() -> Self {
        Self::CleanBackgroundTerminals
    }

    pub(crate) fn run_user_shell_command(command: String) -> Self {
        Self::RunUserShellCommand { command }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn user_turn(
        items: Vec<UserInput>,
        cwd: PathBuf,
        model: String,
        effort: Option<ReasoningEffortConfig>,
        summary: Option<ReasoningSummaryConfig>,
        service_tier: Option<Option<String>>,
        final_output_json_schema: Option<Value>,
        agent_settings: Option<AgentSettings>,
        personality: Option<Personality>,
    ) -> Self {
        Self::UserTurn {
            items,
            cwd,
            model,
            effort,
            summary,
            service_tier,
            final_output_json_schema,
            agent_settings,
            personality,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn override_turn_context(
        cwd: Option<PathBuf>,
        model: Option<String>,
        effort: Option<Option<ReasoningEffortConfig>>,
        summary: Option<ReasoningSummaryConfig>,
        service_tier: Option<Option<String>>,
        agent_settings: Option<AgentSettings>,
        personality: Option<Personality>,
    ) -> Self {
        Self::OverrideTurnContext {
            cwd,
            model,
            effort,
            summary,
            service_tier,
            agent_settings,
            personality,
        }
    }

    pub(crate) fn user_input_answer(id: String, response: ToolRequestUserInputResponse) -> Self {
        Self::UserInputAnswer { id, response }
    }

    pub(crate) fn reload_user_config() -> Self {
        Self::ReloadUserConfig
    }

    pub(crate) fn list_skills(cwds: Vec<PathBuf>, force_reload: bool) -> Self {
        Self::ListSkills { cwds, force_reload }
    }

    pub(crate) fn compact() -> Self {
        Self::Compact
    }

    pub(crate) fn set_thread_name(name: String) -> Self {
        Self::SetThreadName { name }
    }
}

impl From<&AppCommand> for AppCommand {
    fn from(value: &AppCommand) -> Self {
        value.clone()
    }
}
