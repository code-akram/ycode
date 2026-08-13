//! App-server session facade used by the TUI event loop.
//!
//! This module owns the typed JSON-RPC calls needed by the TUI and keeps
//! request/response plumbing out of `App` and `ChatWidget`.

mod fs;
mod history;

pub(crate) use history::HISTORY_ITEM_PAGE_LIMIT;
pub(crate) use history::HISTORY_ITEM_SCAN_LIMIT;
pub(crate) use history::HistoryHydrationScope;
pub(crate) use history::thread_items_page_params;

use crate::legacy_core::config::Config;
use crate::service_tier_resolution;
use crate::session_state::MessageHistoryMetadata;
use crate::session_state::ThreadSessionState;
use crate::status::StatusAccountDisplay;
use crate::status::plan_type_display_name;
use crate::terminal_visualization_instructions::with_terminal_visualization_instructions;
use codex_cli_protocol::Account;
use codex_cli_protocol::AuthMode;
use codex_cli_protocol::ClientRequest;
use codex_cli_protocol::ConfigBatchWriteParams;
use codex_cli_protocol::ConfigWriteResponse;
use codex_cli_protocol::GetAccountParams;
use codex_cli_protocol::GetAccountRateLimitsResponse;
use codex_cli_protocol::GetAccountResponse;
use codex_cli_protocol::JSONRPCErrorError;
use codex_cli_protocol::LogoutAccountResponse;
use codex_cli_protocol::MemoryResetResponse;
use codex_cli_protocol::Model as ApiModel;
use codex_cli_protocol::ModelListParams;
use codex_cli_protocol::ModelListResponse;
use codex_cli_protocol::RateLimitSnapshot;
use codex_cli_protocol::RequestId;
use codex_cli_protocol::SessionSource;
use codex_cli_protocol::SkillsListParams;
use codex_cli_protocol::SkillsListResponse;
use codex_cli_protocol::Thread;
use codex_cli_protocol::ThreadArchiveParams;
use codex_cli_protocol::ThreadArchiveResponse;
use codex_cli_protocol::ThreadBackgroundTerminalsCleanParams;
use codex_cli_protocol::ThreadBackgroundTerminalsCleanResponse;
use codex_cli_protocol::ThreadCompactStartParams;
use codex_cli_protocol::ThreadCompactStartResponse;
use codex_cli_protocol::ThreadDeleteParams;
use codex_cli_protocol::ThreadDeleteResponse;
use codex_cli_protocol::ThreadForkParams;
use codex_cli_protocol::ThreadForkResponse;
use codex_cli_protocol::ThreadGoalClearParams;
use codex_cli_protocol::ThreadGoalClearResponse;
use codex_cli_protocol::ThreadGoalGetParams;
use codex_cli_protocol::ThreadGoalGetResponse;
use codex_cli_protocol::ThreadGoalSetParams;
use codex_cli_protocol::ThreadGoalSetResponse;
use codex_cli_protocol::ThreadGoalStatus;
use codex_cli_protocol::ThreadHistoryMode;
use codex_cli_protocol::ThreadInjectItemsParams;
use codex_cli_protocol::ThreadInjectItemsResponse;
use codex_cli_protocol::ThreadListParams;
use codex_cli_protocol::ThreadListResponse;
use codex_cli_protocol::ThreadLoadedListParams;
use codex_cli_protocol::ThreadLoadedListResponse;
use codex_cli_protocol::ThreadMemoryMode;
use codex_cli_protocol::ThreadMemoryModeSetParams;
use codex_cli_protocol::ThreadMemoryModeSetResponse;
use codex_cli_protocol::ThreadMetadataGitInfoUpdateParams;
use codex_cli_protocol::ThreadMetadataUpdateParams;
use codex_cli_protocol::ThreadMetadataUpdateResponse;
use codex_cli_protocol::ThreadReadParams;
use codex_cli_protocol::ThreadReadResponse;
use codex_cli_protocol::ThreadResumeParams;
use codex_cli_protocol::ThreadResumeResponse;
use codex_cli_protocol::ThreadSetNameParams;
use codex_cli_protocol::ThreadSetNameResponse;
use codex_cli_protocol::ThreadSettingsUpdateParams;
use codex_cli_protocol::ThreadSettingsUpdateResponse;
use codex_cli_protocol::ThreadShellCommandParams;
use codex_cli_protocol::ThreadShellCommandResponse;
use codex_cli_protocol::ThreadSource;
use codex_cli_protocol::ThreadStartParams;
use codex_cli_protocol::ThreadStartResponse;
use codex_cli_protocol::ThreadStartSource;
use codex_cli_protocol::ThreadUnarchiveParams;
use codex_cli_protocol::ThreadUnarchiveResponse;
use codex_cli_protocol::ThreadUnsubscribeParams;
use codex_cli_protocol::ThreadUnsubscribeResponse;
use codex_cli_protocol::Turn;
use codex_cli_protocol::TurnInterruptParams;
use codex_cli_protocol::TurnInterruptResponse;
use codex_cli_protocol::TurnStartParams;
use codex_cli_protocol::TurnStartResponse;
use codex_cli_protocol::TurnSteerParams;
use codex_cli_protocol::TurnSteerResponse;
use codex_cli_protocol::UserInput;
use codex_cli_runtime_client::CliRuntimeClient;
use codex_cli_runtime_client::CliRuntimeEvent;
use codex_cli_runtime_client::CliRuntimePath;
use codex_cli_runtime_client::CliRuntimeRequestHandle;
use codex_cli_runtime_client::InteractiveTuiNativeCodeModeHandle;
use codex_cli_runtime_client::TypedRequestError;
use codex_protocol::ThreadId;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelAvailabilityNux;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ModelServiceTier;
use codex_protocol::openai_models::ModelUpgrade;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::protocol::SubAgentSource;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use color_eyre::eyre::ContextCompat;
use color_eyre::eyre::Result;
use color_eyre::eyre::WrapErr;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use uuid::Uuid;

const JSONRPC_INVALID_REQUEST: i64 = -32600;
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
const JSONRPC_INVALID_PARAMS: i64 = -32602;
const THREAD_SETTINGS_UPDATE_METHOD: &str = "thread/settings/update";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ForkGoalContinuation {
    StartIfIdle,
    DeferUntilNextTurn,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForkPresentation {
    Regular,
    SideConversation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ThreadHistorySupport {
    Paginated,
    LegacyOnly,
}

fn bootstrap_request_error(context: &'static str, err: TypedRequestError) -> color_eyre::Report {
    color_eyre::eyre::eyre!("{context}: {err}")
}

fn is_history_pagination_unsupported(source: &JSONRPCErrorError) -> bool {
    if source.code == JSONRPC_METHOD_NOT_FOUND {
        return true;
    }

    if !matches!(
        source.code,
        JSONRPC_INVALID_REQUEST | JSONRPC_INVALID_PARAMS
    ) {
        return false;
    }

    let message = source.message.to_ascii_lowercase();
    [
        "historymode",
        "history mode",
        "excludeturns",
        "exclude turns",
        "thread/turns/list",
        "thread/items/list",
    ]
    .into_iter()
    .any(|field| message.contains(field))
        || (message.contains("paginated")
            && ["unknown variant", "unsupported variant", "invalid enum"]
                .into_iter()
                .any(|error| message.contains(error)))
}

async fn request_thread_start_with_history_fallback(
    request_handle: &CliRuntimeRequestHandle,
    request_id: RequestId,
    mut params: ThreadStartParams,
) -> std::result::Result<(ThreadStartResponse, ThreadHistorySupport), TypedRequestError> {
    match request_handle
        .request_typed(ClientRequest::ThreadStart {
            request_id,
            params: params.clone(),
        })
        .await
    {
        Ok(response) => Ok((response, ThreadHistorySupport::Paginated)),
        Err(TypedRequestError::Server { source, .. })
            if params.history_mode.is_some() && is_history_pagination_unsupported(&source) =>
        {
            params.history_mode = None;
            let response = request_handle
                .request_typed(ClientRequest::ThreadStart {
                    request_id: RequestId::String(format!(
                        "legacy-thread-start-{}",
                        Uuid::new_v4()
                    )),
                    params,
                })
                .await?;
            Ok((response, ThreadHistorySupport::LegacyOnly))
        }
        Err(err) => Err(err),
    }
}

fn is_thread_settings_update_unsupported(source: &JSONRPCErrorError) -> bool {
    source.code == JSONRPC_METHOD_NOT_FOUND
        || (source.code == JSONRPC_INVALID_REQUEST
            && source.message.contains(THREAD_SETTINGS_UPDATE_METHOD))
}

/// Data collected during the TUI bootstrap phase that the main event loop
/// needs to configure the UI and initial rate-limit prefetch.
///
/// Rate-limit snapshots are intentionally **not** included here; they are
/// fetched asynchronously after bootstrap returns so that the TUI can render
/// its first frame without waiting for the rate-limit round-trip.
pub(crate) struct CliRuntimeBootstrap {
    pub(crate) duration: Duration,
    pub(crate) auth_mode: Option<AuthMode>,
    pub(crate) status_account_display: Option<StatusAccountDisplay>,
    pub(crate) plan_type: Option<codex_protocol::account::PlanType>,
    /// Whether the configured model provider needs OpenAI-style auth. Combined
    /// with `has_chatgpt_account` to decide if a startup rate-limit prefetch
    /// should be fired.
    pub(crate) requires_openai_auth: bool,
    pub(crate) default_model: String,
    pub(crate) has_chatgpt_account: bool,
    pub(crate) available_models: Vec<ModelPreset>,
}

pub(crate) struct CliRuntimeSession {
    client: CliRuntimeClient,
    native_code_mode: Option<InteractiveTuiNativeCodeModeHandle>,
    next_request_id: i64,
    history_pagination: HashMap<ThreadId, history::ThreadHistoryPagination>,
    remote_cwd_override: Option<PathBuf>,
    thread_params_mode: ThreadParamsMode,
    history_support: ThreadHistorySupport,
    thread_settings_update_supported: bool,
    default_model: Option<String>,
    available_models: Vec<ModelPreset>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThreadParamsMode {
    Embedded,
}

/// Determines where model settings come from when resuming a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResumeModelSettings {
    /// Sends the current config's model and reasoning effort as explicit overrides.
    OverrideFromCurrentConfig,
    /// Omits those overrides so cli-runtime restores the settings saved with the thread.
    RestoreFromThread,
}

#[derive(Debug)]
pub(crate) struct CliRuntimeStartedThread {
    pub(crate) session: ThreadSessionState,
    pub(crate) turns: Vec<Turn>,
    pub(crate) blocks_direct_input: bool,
}

pub(crate) fn source_agent_path(source: &SessionSource) -> Option<String> {
    match source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_path, .. }) => {
            agent_path.clone().map(String::from)
        }
        _ => None,
    }
}

/// Uses the server capability when available and preserves compatibility with older servers.
pub(crate) fn thread_blocks_direct_input(thread: &Thread) -> bool {
    thread
        .can_accept_direct_input
        .map(|can_accept| !can_accept)
        .unwrap_or_else(|| source_agent_path(&thread.source).is_some())
}

impl CliRuntimeSession {
    pub(crate) fn new(client: CliRuntimeClient, thread_params_mode: ThreadParamsMode) -> Self {
        Self {
            client,
            native_code_mode: None,
            next_request_id: 1,
            history_pagination: HashMap::new(),
            remote_cwd_override: None,
            thread_params_mode,
            history_support: ThreadHistorySupport::Paginated,
            thread_settings_update_supported: true,
            default_model: None,
            available_models: Vec::new(),
        }
    }

    pub(crate) fn new_interactive_tui(
        client: CliRuntimeClient,
        thread_params_mode: ThreadParamsMode,
        native_code_mode: InteractiveTuiNativeCodeModeHandle,
    ) -> Self {
        Self {
            native_code_mode: Some(native_code_mode),
            ..Self::new(client, thread_params_mode)
        }
    }

    pub(crate) fn with_remote_cwd_override(mut self, remote_cwd_override: Option<PathBuf>) -> Self {
        self.remote_cwd_override = remote_cwd_override;
        self
    }

    pub(crate) fn remote_cwd_override(&self) -> Option<&std::path::Path> {
        self.remote_cwd_override.as_deref()
    }

    pub(crate) fn uses_remote_workspace(&self) -> bool {
        false
    }

    pub(crate) fn uses_embedded_cli_runtime(&self) -> bool {
        matches!(&self.client, CliRuntimeClient::InProcess(_))
    }

    /// Starts a one-shot native task through the private embedded-only lane.
    pub(crate) async fn start_native_code_mode_from_interactive_composer(
        &self,
        thread_id: ThreadId,
        task: String,
    ) -> Result<String> {
        let native_code_mode = self.native_code_mode.as_ref().ok_or_else(|| {
            color_eyre::eyre::eyre!(
                "native Code Mode is unavailable outside the live embedded TUI composer"
            )
        })?;
        native_code_mode
            .start(thread_id, task)
            .await
            .map_err(Into::into)
    }

    pub(crate) fn codex_home_path(
        &self,
        local_codex_home: &AbsolutePathBuf,
    ) -> Option<CliRuntimePath> {
        self.client.codex_home(local_codex_home)
    }

    pub(crate) fn server_version(&self) -> Option<&str> {
        None
    }

    pub(crate) async fn bootstrap(&mut self, config: &Config) -> Result<CliRuntimeBootstrap> {
        let started_at = Instant::now();
        let account = self.read_account().await?;
        let model_request_id = self.next_request_id();
        let models = self
            .client
            .request_typed::<ModelListResponse>(ClientRequest::ModelList {
                request_id: model_request_id,
                params: ModelListParams {
                    cursor: None,
                    limit: None,
                    include_hidden: Some(true),
                },
            })
            .await
            .map_err(|err| {
                bootstrap_request_error("model/list failed during TUI bootstrap", err)
            })?;
        let available_models = models
            .data
            .into_iter()
            .map(model_preset_from_api_model)
            .collect::<Vec<_>>();
        let default_model = config
            .model
            .clone()
            .or_else(|| {
                available_models
                    .iter()
                    .find(|model| model.is_default)
                    .map(|model| model.model.clone())
            })
            .or_else(|| available_models.first().map(|model| model.model.clone()))
            .wrap_err("model/list returned no models for TUI bootstrap")?;
        self.default_model = Some(default_model.clone());
        self.available_models = available_models.clone();

        let (auth_mode, status_account_display, plan_type, has_chatgpt_account) =
            match account.account {
                Some(Account::ApiKey {}) => (
                    Some(AuthMode::ApiKey),
                    Some(StatusAccountDisplay::ApiKey),
                    None,
                    false,
                ),
                Some(Account::Chatgpt { email, plan_type }) => (
                    Some(AuthMode::Chatgpt),
                    Some(StatusAccountDisplay::ChatGpt {
                        email,
                        plan: Some(plan_type_display_name(plan_type)),
                    }),
                    Some(plan_type),
                    true,
                ),
                None => (None, None, None, false),
            };
        Ok(CliRuntimeBootstrap {
            duration: started_at.elapsed(),
            auth_mode,
            status_account_display,
            plan_type,
            requires_openai_auth: account.requires_openai_auth,
            default_model,
            has_chatgpt_account,
            available_models,
        })
    }

    /// Fetches the current account info without refreshing the auth token.
    ///
    /// Used by both `bootstrap` (to populate the initial UI) and `get_login_status`
    /// (to check auth mode without the overhead of a full bootstrap).
    pub(crate) async fn read_account(&mut self) -> Result<GetAccountResponse> {
        let account_request_id = self.next_request_id();
        self.client
            .request_typed(ClientRequest::GetAccount {
                request_id: account_request_id,
                params: GetAccountParams {
                    refresh_token: false,
                },
            })
            .await
            .map_err(|err| bootstrap_request_error("account/read failed during TUI bootstrap", err))
    }

    pub(crate) async fn next_event(&mut self) -> Option<CliRuntimeEvent> {
        self.client.next_event().await
    }

    #[cfg(test)]
    pub(crate) async fn start_thread(
        &mut self,
        config: &Config,
    ) -> Result<CliRuntimeStartedThread> {
        self.start_thread_with_session_start_source(config, /*session_start_source*/ None)
            .await
    }

    pub(crate) async fn start_thread_with_session_start_source(
        &mut self,
        config: &Config,
        session_start_source: Option<ThreadStartSource>,
    ) -> Result<CliRuntimeStartedThread> {
        let request_id = self.next_request_id();
        let session_config = self.session_config_with_effective_service_tier(config);
        let mut params = thread_start_params_from_config(
            &session_config,
            self.thread_params_mode(),
            self.remote_cwd_override.as_deref(),
            session_start_source,
        );
        if self.history_support == ThreadHistorySupport::LegacyOnly {
            params.history_mode = None;
        }
        let request_handle = self.request_handle();
        let (response, history_support) =
            request_thread_start_with_history_fallback(&request_handle, request_id, params)
                .await
                .map_err(|err| {
                    bootstrap_request_error("thread/start failed during TUI bootstrap", err)
                })?;
        if history_support == ThreadHistorySupport::LegacyOnly {
            self.history_support = ThreadHistorySupport::LegacyOnly;
        }
        started_thread_from_start_response(response, config, self.thread_params_mode()).await
    }

    pub(crate) async fn resume_thread(
        &mut self,
        config: Config,
        thread_id: ThreadId,
        model_settings: ResumeModelSettings,
    ) -> Result<CliRuntimeStartedThread> {
        let request_id = self.next_request_id();
        let session_config = if model_settings == ResumeModelSettings::RestoreFromThread {
            config.clone()
        } else {
            self.session_config_with_effective_service_tier(&config)
        };
        let mut params = thread_resume_params_from_config(
            session_config,
            thread_id,
            self.thread_params_mode(),
            self.remote_cwd_override.as_deref(),
            model_settings,
        );
        params.exclude_turns = self.history_support == ThreadHistorySupport::Paginated
            && self
                .history_pagination
                .get(&thread_id)
                .is_none_or(|state| state.history_mode == ThreadHistoryMode::Paginated);
        let mut response: ThreadResumeResponse = match self
            .client
            .request_typed(ClientRequest::ThreadResume {
                request_id,
                params: params.clone(),
            })
            .await
        {
            Ok(response) => response,
            Err(TypedRequestError::Server { source, .. })
                if params.exclude_turns && is_history_pagination_unsupported(&source) =>
            {
                self.history_support = ThreadHistorySupport::LegacyOnly;
                params.exclude_turns = false;
                let request_id = self.next_request_id();
                self.client
                    .request_typed(ClientRequest::ThreadResume { request_id, params })
                    .await
                    .map_err(|err| {
                        bootstrap_request_error("thread/resume failed during TUI bootstrap", err)
                    })?
            }
            Err(err) => {
                return Err(bootstrap_request_error(
                    "thread/resume failed during TUI bootstrap",
                    err,
                ));
            }
        };
        self.hydrate_initial_thread_history(
            &mut response.thread,
            response.turns_backwards_cursor.clone(),
            response.items_backwards_cursor.clone(),
            Some(&config),
            HistoryHydrationScope::Initial,
        )
        .await?;
        let fork_parent_title = self
            .fork_parent_title_from_cli_runtime(response.thread.forked_from_id.as_deref())
            .await;
        let mut started =
            started_thread_from_resume_response(response, &config, self.thread_params_mode())
                .await?;
        started.session.fork_parent_title = fork_parent_title;
        Ok(started)
    }

    pub(crate) async fn fork_thread(
        &mut self,
        config: Config,
        thread_id: ThreadId,
    ) -> Result<CliRuntimeStartedThread> {
        self.fork_thread_at(
            config,
            thread_id,
            /*last_turn_id*/ None,
            /*before_turn_id*/ None,
            ForkGoalContinuation::StartIfIdle,
        )
        .await
    }

    pub(crate) async fn fork_thread_at(
        &mut self,
        config: Config,
        thread_id: ThreadId,
        last_turn_id: Option<String>,
        before_turn_id: Option<String>,
        goal_continuation: ForkGoalContinuation,
    ) -> Result<CliRuntimeStartedThread> {
        self.fork_thread_at_with_presentation(
            config,
            thread_id,
            last_turn_id,
            before_turn_id,
            goal_continuation,
            ForkPresentation::Regular,
        )
        .await
    }

    pub(crate) async fn fork_side_thread(
        &mut self,
        config: Config,
        thread_id: ThreadId,
    ) -> Result<CliRuntimeStartedThread> {
        self.fork_thread_at_with_presentation(
            config,
            thread_id,
            /*last_turn_id*/ None,
            /*before_turn_id*/ None,
            ForkGoalContinuation::StartIfIdle,
            ForkPresentation::SideConversation,
        )
        .await
    }

    async fn fork_thread_at_with_presentation(
        &mut self,
        config: Config,
        thread_id: ThreadId,
        last_turn_id: Option<String>,
        before_turn_id: Option<String>,
        goal_continuation: ForkGoalContinuation,
        presentation: ForkPresentation,
    ) -> Result<CliRuntimeStartedThread> {
        let fork_parent = match presentation {
            ForkPresentation::Regular => self
                .thread_read(thread_id, /*include_turns*/ false)
                .await
                .ok(),
            ForkPresentation::SideConversation => None,
        };
        let exclude_turns = self.history_support == ThreadHistorySupport::Paginated
            && (fork_parent
                .as_ref()
                .is_some_and(|thread| thread.history_mode == ThreadHistoryMode::Paginated)
                || presentation == ForkPresentation::SideConversation);
        let request_id = self.next_request_id();
        let session_config = self.session_config_with_effective_service_tier(&config);
        let mut params = ThreadForkParams {
            last_turn_id,
            before_turn_id,
            defer_goal_continuation: goal_continuation == ForkGoalContinuation::DeferUntilNextTurn,
            exclude_turns,
            ..thread_fork_params_from_config(
                session_config,
                thread_id,
                self.thread_params_mode(),
                self.remote_cwd_override.as_deref(),
            )
        };
        let response: ThreadForkResponse = match self
            .client
            .request_typed(ClientRequest::ThreadFork {
                request_id,
                params: params.clone(),
            })
            .await
        {
            Ok(response) => response,
            Err(TypedRequestError::Server { source, .. })
                if params.exclude_turns && is_history_pagination_unsupported(&source) =>
            {
                self.history_support = ThreadHistorySupport::LegacyOnly;
                params.exclude_turns = false;
                let request_id = self.next_request_id();
                self.client
                    .request_typed(ClientRequest::ThreadFork { request_id, params })
                    .await
                    .map_err(|err| {
                        bootstrap_request_error("thread/fork failed during TUI bootstrap", err)
                    })?
            }
            Err(err) => {
                return Err(bootstrap_request_error(
                    "thread/fork failed during TUI bootstrap",
                    err,
                ));
            }
        };
        let mut response = response;
        if presentation == ForkPresentation::Regular
            && !response.thread.ephemeral
            && let Err(error) = self
                .hydrate_initial_thread_history(
                    &mut response.thread,
                    /*turn_cursor*/ None,
                    /*item_cursor*/ None,
                    Some(&config),
                    HistoryHydrationScope::Initial,
                )
                .await
        {
            tracing::warn!(
                thread_id = %response.thread.id,
                error = %error,
                "preserving the created fork after bounded history hydration failed"
            );
        }
        let mut started =
            started_thread_from_fork_response(response, &config, self.thread_params_mode()).await?;
        started.session.fork_parent_title = fork_parent.and_then(|thread| thread.name);
        Ok(started)
    }

    pub(crate) fn thread_params_mode(&self) -> ThreadParamsMode {
        self.thread_params_mode
    }

    fn session_config_with_effective_service_tier(&self, config: &Config) -> Config {
        let Some(model) = config.model.as_deref().or(self.default_model.as_deref()) else {
            return config.clone();
        };
        let mut session_config = config.clone();
        match service_tier_resolution::service_tier_update_for_core(
            config,
            model,
            &self.available_models,
        ) {
            Some(Some(service_tier)) => {
                session_config.service_tier = Some(service_tier);
                session_config.notices.fast_default_opt_out = None;
            }
            Some(None) => {
                session_config.service_tier = Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string());
                session_config.notices.fast_default_opt_out = None;
            }
            None => {
                session_config.service_tier = None;
                session_config.notices.fast_default_opt_out = None;
            }
        }
        session_config
    }

    async fn fork_parent_title_from_cli_runtime(
        &mut self,
        forked_from_id: Option<&str>,
    ) -> Option<String> {
        let forked_from_id = forked_from_id?;
        let forked_from_id = match ThreadId::from_string(forked_from_id) {
            Ok(thread_id) => thread_id,
            Err(err) => {
                tracing::warn!("Failed to parse fork parent thread id from app server: {err}");
                return None;
            }
        };

        match self
            .thread_read(forked_from_id, /*include_turns*/ false)
            .await
        {
            Ok(thread) => thread.name,
            Err(err) => {
                tracing::warn!("Failed to read fork parent metadata from app server: {err}");
                None
            }
        }
    }

    pub(crate) async fn thread_list(
        &mut self,
        params: ThreadListParams,
    ) -> Result<ThreadListResponse> {
        let request_id = self.next_request_id();
        self.client
            .request_typed(ClientRequest::ThreadList { request_id, params })
            .await
            .wrap_err("thread/list failed during TUI session lookup")
    }

    /// Lists thread ids that the app server currently holds in memory.
    ///
    /// Used by `App::backfill_loaded_subagent_threads` to discover subagent threads that were
    /// spawned before the TUI connected. The caller then fetches full metadata per thread via
    /// `thread_read` and walks the spawn tree.
    pub(crate) async fn thread_loaded_list(
        &mut self,
        params: ThreadLoadedListParams,
    ) -> Result<ThreadLoadedListResponse> {
        let request_id = self.next_request_id();
        self.client
            .request_typed(ClientRequest::ThreadLoadedList { request_id, params })
            .await
            .wrap_err("failed to list loaded threads from app server")
    }

    pub(crate) async fn thread_read(
        &mut self,
        thread_id: ThreadId,
        include_turns: bool,
    ) -> Result<Thread> {
        let request_id = self.next_request_id();
        let response = self
            .client
            .request_typed::<ThreadReadResponse>(ClientRequest::ThreadRead {
                request_id,
                params: ThreadReadParams {
                    thread_id: thread_id.to_string(),
                    include_turns,
                },
            })
            .await;
        let mut response: ThreadReadResponse = match response {
            Ok(response) => return Ok(response.thread),
            Err(TypedRequestError::Server { source, .. })
                if include_turns
                    && source.message
                        == "paginated threads do not support thread/read(includeTurns=true)" =>
            {
                let request_id = self.next_request_id();
                self.client
                    .request_typed(ClientRequest::ThreadRead {
                        request_id,
                        params: ThreadReadParams {
                            thread_id: thread_id.to_string(),
                            include_turns: false,
                        },
                    })
                    .await
                    .wrap_err("thread/read failed during TUI session lookup")?
            }
            Err(err) => return Err(err).wrap_err("thread/read failed during TUI session lookup"),
        };
        self.hydrate_initial_thread_history(
            &mut response.thread,
            /*turn_cursor*/ None,
            /*item_cursor*/ None,
            /*config*/ None,
            HistoryHydrationScope::Initial,
        )
        .await?;
        Ok(response.thread)
    }

    pub(crate) async fn thread_archive(&mut self, thread_id: ThreadId) -> Result<()> {
        let request_id = self.next_request_id();
        let _: ThreadArchiveResponse = self
            .client
            .request_typed(ClientRequest::ThreadArchive {
                request_id,
                params: ThreadArchiveParams {
                    thread_id: thread_id.to_string(),
                },
            })
            .await
            .wrap_err("failed to archive session")?;
        Ok(())
    }

    pub(crate) async fn thread_delete(&mut self, thread_id: ThreadId) -> Result<()> {
        let request_id = self.next_request_id();
        let _: ThreadDeleteResponse = self
            .client
            .request_typed(ClientRequest::ThreadDelete {
                request_id,
                params: ThreadDeleteParams {
                    thread_id: thread_id.to_string(),
                },
            })
            .await
            .wrap_err("failed to delete session")?;
        Ok(())
    }

    pub(crate) async fn thread_unarchive(&mut self, thread_id: ThreadId) -> Result<Thread> {
        let request_id = self.next_request_id();
        let response: ThreadUnarchiveResponse = self
            .client
            .request_typed(ClientRequest::ThreadUnarchive {
                request_id,
                params: ThreadUnarchiveParams {
                    thread_id: thread_id.to_string(),
                },
            })
            .await
            .wrap_err("failed to unarchive session")?;
        Ok(response.thread)
    }

    pub(crate) async fn thread_metadata_update_branch(
        &mut self,
        thread_id: ThreadId,
        branch: String,
    ) -> Result<ThreadMetadataUpdateResponse> {
        let request_id = self.next_request_id();
        self.client
            .request_typed(ClientRequest::ThreadMetadataUpdate {
                request_id,
                params: ThreadMetadataUpdateParams {
                    thread_id: thread_id.to_string(),
                    git_info: Some(ThreadMetadataGitInfoUpdateParams {
                        sha: None,
                        branch: Some(Some(branch)),
                        origin_url: None,
                    }),
                },
            })
            .await
            .wrap_err("thread/metadata/update failed while syncing git branch")
    }

    pub(crate) async fn thread_settings_update(
        &mut self,
        params: ThreadSettingsUpdateParams,
    ) -> Result<bool> {
        if !self.thread_settings_update_supported {
            return Ok(false);
        }
        let request_id = self.next_request_id();
        match self
            .client
            .request_typed::<ThreadSettingsUpdateResponse>(ClientRequest::ThreadSettingsUpdate {
                request_id,
                params,
            })
            .await
        {
            Ok(_) => Ok(true),
            Err(TypedRequestError::Server { source, .. })
                if is_thread_settings_update_unsupported(&source) =>
            {
                // Older remote app servers can reject this experimental method as
                // method-not-found, experimental-capability-gated, or an unknown
                // request variant. Treat those as a session-level capability
                // downgrade so local TUI setting changes stay best-effort instead
                // of showing an error every time the user changes model, effort,
                // personality, or mode.
                self.thread_settings_update_supported = false;
                Ok(false)
            }
            Err(err) => Err(err).wrap_err("thread/settings/update failed in TUI"),
        }
    }

    pub(crate) async fn thread_inject_items(
        &mut self,
        thread_id: ThreadId,
        items: Vec<ResponseItem>,
    ) -> Result<ThreadInjectItemsResponse> {
        let items = items
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()
            .wrap_err("failed to encode thread/inject_items payload")?;
        let request_id = self.next_request_id();
        self.client
            .request_typed(ClientRequest::ThreadInjectItems {
                request_id,
                params: ThreadInjectItemsParams {
                    thread_id: thread_id.to_string(),
                    items,
                },
            })
            .await
            .wrap_err("thread/inject_items failed during TUI side conversation setup")
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn turn_start(
        &mut self,
        thread_id: ThreadId,
        items: Vec<UserInput>,
        cwd: PathBuf,
        workspace_roots: &[AbsolutePathBuf],
        model: String,
        effort: Option<codex_protocol::openai_models::ReasoningEffort>,
        summary: Option<codex_protocol::config_types::ReasoningSummary>,
        service_tier: Option<Option<String>>,
        agent_settings: Option<codex_protocol::config_types::AgentSettings>,
        personality: Option<codex_protocol::config_types::Personality>,
        output_schema: Option<serde_json::Value>,
    ) -> Result<TurnStartResponse> {
        let request_id = self.next_request_id();
        self.client
            .request_typed(ClientRequest::TurnStart {
                request_id,
                params: TurnStartParams {
                    thread_id: thread_id.to_string(),
                    client_user_message_id: None,
                    input: items,
                    responsesapi_client_metadata: None,
                    additional_context: None,
                    environments: None,
                    cwd: Some(cwd),
                    runtime_workspace_roots: Some(workspace_roots.to_vec()),
                    model: Some(model),
                    service_tier,
                    effort,
                    summary,
                    personality,
                    output_schema,
                    agent_settings,
                    multi_agent_mode: None,
                },
            })
            .await
            .wrap_err("turn/start failed in TUI")
    }

    pub(crate) async fn turn_interrupt(
        &mut self,
        thread_id: ThreadId,
        turn_id: String,
    ) -> std::result::Result<(), TypedRequestError> {
        let request_id = self.next_request_id();
        let _: TurnInterruptResponse = self
            .client
            .request_typed(ClientRequest::TurnInterrupt {
                request_id,
                params: TurnInterruptParams {
                    thread_id: thread_id.to_string(),
                    turn_id,
                },
            })
            .await?;
        Ok(())
    }

    pub(crate) async fn startup_interrupt(
        &mut self,
        thread_id: ThreadId,
    ) -> std::result::Result<(), TypedRequestError> {
        self.turn_interrupt(thread_id, String::new()).await
    }

    pub(crate) async fn turn_steer(
        &mut self,
        thread_id: ThreadId,
        turn_id: String,
        items: Vec<UserInput>,
    ) -> std::result::Result<TurnSteerResponse, TypedRequestError> {
        let request_id = self.next_request_id();
        self.client
            .request_typed(ClientRequest::TurnSteer {
                request_id,
                params: TurnSteerParams {
                    thread_id: thread_id.to_string(),
                    client_user_message_id: None,
                    input: items,
                    responsesapi_client_metadata: None,
                    additional_context: None,
                    expected_turn_id: turn_id,
                },
            })
            .await
    }

    pub(crate) async fn thread_set_name(
        &mut self,
        thread_id: ThreadId,
        name: String,
    ) -> Result<()> {
        let request_id = self.next_request_id();
        let _: ThreadSetNameResponse = self
            .client
            .request_typed(ClientRequest::ThreadSetName {
                request_id,
                params: ThreadSetNameParams {
                    thread_id: thread_id.to_string(),
                    name,
                },
            })
            .await
            .wrap_err("thread/name/set failed in TUI")?;
        Ok(())
    }

    pub(crate) async fn thread_memory_mode_set(
        &mut self,
        thread_id: ThreadId,
        mode: ThreadMemoryMode,
    ) -> Result<()> {
        let request_id = self.next_request_id();
        let _: ThreadMemoryModeSetResponse = self
            .client
            .request_typed(ClientRequest::ThreadMemoryModeSet {
                request_id,
                params: ThreadMemoryModeSetParams {
                    thread_id: thread_id.to_string(),
                    mode,
                },
            })
            .await
            .wrap_err("thread/memoryMode/set failed in TUI")?;
        Ok(())
    }

    pub(crate) async fn memory_reset(&mut self) -> Result<()> {
        let request_id = self.next_request_id();
        let _: MemoryResetResponse = self
            .client
            .request_typed(ClientRequest::MemoryReset {
                request_id,
                params: None,
            })
            .await
            .wrap_err("memory/reset failed in TUI")?;
        Ok(())
    }

    pub(crate) async fn thread_goal_get(
        &mut self,
        thread_id: ThreadId,
    ) -> Result<ThreadGoalGetResponse> {
        let request_id = self.next_request_id();
        self.client
            .request_typed(ClientRequest::ThreadGoalGet {
                request_id,
                params: ThreadGoalGetParams {
                    thread_id: thread_id.to_string(),
                },
            })
            .await
            .wrap_err("thread/goal/get failed in TUI")
    }

    pub(crate) async fn thread_goal_set(
        &mut self,
        thread_id: ThreadId,
        objective: Option<String>,
        status: Option<ThreadGoalStatus>,
        token_budget: Option<Option<i64>>,
    ) -> Result<ThreadGoalSetResponse> {
        let request_id = self.next_request_id();
        self.client
            .request_typed(ClientRequest::ThreadGoalSet {
                request_id,
                params: ThreadGoalSetParams {
                    thread_id: thread_id.to_string(),
                    objective,
                    status,
                    token_budget,
                },
            })
            .await
            .wrap_err("thread/goal/set failed in TUI")
    }

    pub(crate) async fn thread_goal_clear(
        &mut self,
        thread_id: ThreadId,
    ) -> Result<ThreadGoalClearResponse> {
        let request_id = self.next_request_id();
        self.client
            .request_typed(ClientRequest::ThreadGoalClear {
                request_id,
                params: ThreadGoalClearParams {
                    thread_id: thread_id.to_string(),
                },
            })
            .await
            .wrap_err("thread/goal/clear failed in TUI")
    }

    pub(crate) async fn logout_account(&mut self) -> Result<()> {
        let request_id = self.next_request_id();
        let _: LogoutAccountResponse = self
            .client
            .request_typed(ClientRequest::LogoutAccount {
                request_id,
                params: None,
            })
            .await
            .wrap_err("account/logout failed in TUI")?;
        Ok(())
    }

    pub(crate) async fn thread_unsubscribe(&mut self, thread_id: ThreadId) -> Result<()> {
        let request_id = self.next_request_id();
        let _: ThreadUnsubscribeResponse = self
            .client
            .request_typed(ClientRequest::ThreadUnsubscribe {
                request_id,
                params: ThreadUnsubscribeParams {
                    thread_id: thread_id.to_string(),
                },
            })
            .await
            .wrap_err("thread/unsubscribe failed in TUI")?;
        Ok(())
    }

    pub(crate) async fn thread_compact_start(&mut self, thread_id: ThreadId) -> Result<()> {
        let request_id = self.next_request_id();
        let _: ThreadCompactStartResponse = self
            .client
            .request_typed(ClientRequest::ThreadCompactStart {
                request_id,
                params: ThreadCompactStartParams {
                    thread_id: thread_id.to_string(),
                },
            })
            .await
            .wrap_err("thread/compact/start failed in TUI")?;
        Ok(())
    }

    pub(crate) async fn thread_shell_command(
        &mut self,
        thread_id: ThreadId,
        command: String,
    ) -> Result<()> {
        let request_id = self.next_request_id();
        let _: ThreadShellCommandResponse = self
            .client
            .request_typed(ClientRequest::ThreadShellCommand {
                request_id,
                params: ThreadShellCommandParams {
                    thread_id: thread_id.to_string(),
                    command,
                },
            })
            .await
            .wrap_err("thread/shellCommand failed in TUI")?;
        Ok(())
    }

    pub(crate) async fn thread_background_terminals_clean(
        &mut self,
        thread_id: ThreadId,
    ) -> Result<()> {
        let request_id = self.next_request_id();
        let _: ThreadBackgroundTerminalsCleanResponse = self
            .client
            .request_typed(ClientRequest::ThreadBackgroundTerminalsClean {
                request_id,
                params: ThreadBackgroundTerminalsCleanParams {
                    thread_id: thread_id.to_string(),
                },
            })
            .await
            .wrap_err("thread/backgroundTerminals/clean failed in TUI")?;
        Ok(())
    }

    pub(crate) async fn skills_list(
        &mut self,
        params: SkillsListParams,
    ) -> Result<SkillsListResponse> {
        let request_id = self.next_request_id();
        self.client
            .request_typed(ClientRequest::SkillsList { request_id, params })
            .await
            .wrap_err("skills/list failed in TUI")
    }

    pub(crate) async fn reload_user_config(&mut self) -> Result<()> {
        let request_id = self.next_request_id();
        let _: ConfigWriteResponse = self
            .client
            .request_typed(ClientRequest::ConfigBatchWrite {
                request_id,
                params: ConfigBatchWriteParams {
                    edits: Vec::new(),
                    file_path: None,
                    expected_version: None,
                    reload_user_config: true,
                },
            })
            .await
            .wrap_err("config/batchWrite failed while reloading user config in TUI")?;
        Ok(())
    }

    pub(crate) async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> std::io::Result<()> {
        self.client.reject_server_request(request_id, error).await
    }

    pub(crate) async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: serde_json::Value,
    ) -> std::io::Result<()> {
        self.client.resolve_server_request(request_id, result).await
    }

    pub(crate) async fn shutdown(self) -> std::io::Result<()> {
        self.client.shutdown().await
    }

    pub(crate) fn request_handle(&self) -> CliRuntimeRequestHandle {
        self.client.request_handle()
    }

    pub(crate) fn next_request_id(&mut self) -> RequestId {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        RequestId::Integer(request_id)
    }
}

pub(crate) async fn start_thread_with_request_handle(
    request_handle: CliRuntimeRequestHandle,
    config: Config,
    thread_params_mode: ThreadParamsMode,
    remote_cwd_override: Option<PathBuf>,
) -> Result<CliRuntimeStartedThread> {
    let request_id = RequestId::String(format!("startup-thread-start-{}", Uuid::new_v4()));
    let params = thread_start_params_from_config(
        &config,
        thread_params_mode,
        remote_cwd_override.as_deref(),
        /*session_start_source*/ None,
    );
    let (response, _history_support) =
        request_thread_start_with_history_fallback(&request_handle, request_id, params)
            .await
            .map_err(|err| {
                bootstrap_request_error("thread/start failed during TUI bootstrap", err)
            })?;
    started_thread_from_start_response(response, &config, thread_params_mode).await
}

pub(crate) fn status_account_display_from_auth_mode(
    auth_mode: Option<AuthMode>,
    plan_type: Option<codex_protocol::account::PlanType>,
) -> Option<StatusAccountDisplay> {
    match auth_mode {
        Some(AuthMode::ApiKey) => Some(StatusAccountDisplay::ApiKey),
        Some(AuthMode::Chatgpt)
        | Some(AuthMode::ChatgptAuthTokens)
        | Some(AuthMode::AgentIdentity)
        | Some(AuthMode::PersonalAccessToken) => Some(StatusAccountDisplay::ChatGpt {
            email: None,
            plan: plan_type.map(plan_type_display_name),
        }),
        Some(AuthMode::Headers) => None,
        None => None,
    }
}

fn model_preset_from_api_model(model: ApiModel) -> ModelPreset {
    let upgrade = model.upgrade.map(|upgrade_id| {
        let upgrade_info = model.upgrade_info.clone();
        ModelUpgrade {
            id: upgrade_id,
            migration_config_key: model.model.clone(),
            model_link: upgrade_info
                .as_ref()
                .and_then(|info| info.model_link.clone()),
            upgrade_copy: upgrade_info
                .as_ref()
                .and_then(|info| info.upgrade_copy.clone()),
            migration_markdown: upgrade_info.and_then(|info| info.migration_markdown),
        }
    });

    ModelPreset {
        id: model.id,
        model: model.model,
        display_name: model.display_name,
        description: model.description,
        model_specialty: model.model_specialty,
        default_reasoning_effort: model.default_reasoning_effort,
        supported_reasoning_efforts: model
            .supported_reasoning_efforts
            .into_iter()
            .map(|effort| ReasoningEffortPreset {
                effort: effort.reasoning_effort,
                description: effort.description,
            })
            .collect(),
        supports_personality: model.supports_personality,
        additional_speed_tiers: model.additional_speed_tiers,
        service_tiers: model
            .service_tiers
            .into_iter()
            .map(|service_tier| ModelServiceTier {
                id: service_tier.id,
                name: service_tier.name,
                description: service_tier.description,
            })
            .collect(),
        default_service_tier: model.default_service_tier,
        is_default: model.is_default,
        upgrade,
        show_in_picker: !model.hidden,
        multi_agent_version: None,
        availability_nux: model.availability_nux.map(|nux| ModelAvailabilityNux {
            message: nux.message,
        }),
        // `model/list` already returns models filtered for the active client/auth context.
        supported_in_api: true,
        input_modalities: model.input_modalities,
    }
}

fn config_request_overrides_from_config(
    config: &Config,
) -> Option<HashMap<String, serde_json::Value>> {
    let mut overrides = HashMap::new();
    let mut insert = |key: &str, value: Option<String>| {
        if let Some(value) = value {
            overrides.insert(key.to_string(), serde_json::Value::String(value));
        }
    };
    insert(
        "model_reasoning_effort",
        config
            .model_reasoning_effort
            .as_ref()
            .map(std::string::ToString::to_string),
    );
    insert(
        "model_reasoning_summary",
        config
            .model_reasoning_summary
            .map(|summary| summary.to_string()),
    );
    insert(
        "model_verbosity",
        config
            .model_verbosity
            .map(|verbosity| verbosity.to_string()),
    );
    insert(
        "personality",
        config
            .personality
            .map(|personality| personality.to_string()),
    );
    insert(
        "web_search",
        Some(config.web_search_mode.value().to_string()),
    );
    Some(overrides)
}

fn service_tier_override_from_config(config: &Config) -> Option<Option<String>> {
    config.service_tier.clone().map(Some).or_else(|| {
        (config.notices.fast_default_opt_out == Some(true))
            .then(|| Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string()))
    })
}

fn thread_start_params_from_config(
    config: &Config,
    thread_params_mode: ThreadParamsMode,
    remote_cwd_override: Option<&std::path::Path>,
    session_start_source: Option<ThreadStartSource>,
) -> ThreadStartParams {
    ThreadStartParams {
        model: config.model.clone(),
        service_tier: service_tier_override_from_config(config),
        cwd: thread_cwd_from_config(config, thread_params_mode, remote_cwd_override),
        runtime_workspace_roots: Some(config.workspace_roots.clone()),
        config: config_request_overrides_from_config(config),
        ephemeral: Some(config.ephemeral),
        history_mode: (!config.ephemeral).then_some(ThreadHistoryMode::Paginated),
        session_start_source,
        thread_source: Some(ThreadSource::User),
        developer_instructions: with_terminal_visualization_instructions(
            config, /*control_instructions*/ None,
        ),
        ..ThreadStartParams::default()
    }
}

fn thread_resume_params_from_config(
    config: Config,
    thread_id: ThreadId,
    thread_params_mode: ThreadParamsMode,
    remote_cwd_override: Option<&std::path::Path>,
    model_settings: ResumeModelSettings,
) -> ThreadResumeParams {
    let mut config_overrides = config_request_overrides_from_config(&config);
    if model_settings == ResumeModelSettings::RestoreFromThread
        && let Some(overrides) = config_overrides.as_mut()
    {
        overrides.remove("model_reasoning_effort");
        if overrides.is_empty() {
            config_overrides = None;
        }
    }
    let model = match model_settings {
        ResumeModelSettings::OverrideFromCurrentConfig => config.model.clone(),
        ResumeModelSettings::RestoreFromThread => None,
    };
    ThreadResumeParams {
        thread_id: thread_id.to_string(),
        model,
        service_tier: service_tier_override_from_config(&config),
        cwd: thread_cwd_from_config(&config, thread_params_mode, remote_cwd_override),
        runtime_workspace_roots: Some(config.workspace_roots.clone()),
        config: config_overrides,
        developer_instructions: with_terminal_visualization_instructions(
            &config, /*control_instructions*/ None,
        ),
        ..ThreadResumeParams::default()
    }
}

fn thread_fork_params_from_config(
    config: Config,
    thread_id: ThreadId,
    thread_params_mode: ThreadParamsMode,
    remote_cwd_override: Option<&std::path::Path>,
) -> ThreadForkParams {
    ThreadForkParams {
        thread_id: thread_id.to_string(),
        model: config.model.clone(),
        service_tier: service_tier_override_from_config(&config),
        cwd: thread_cwd_from_config(&config, thread_params_mode, remote_cwd_override),
        runtime_workspace_roots: Some(config.workspace_roots.clone()),
        config: config_request_overrides_from_config(&config),
        base_instructions: config.base_instructions.clone(),
        developer_instructions: with_terminal_visualization_instructions(
            &config,
            config.developer_instructions.clone(),
        ),
        ephemeral: config.ephemeral,
        thread_source: Some(ThreadSource::User),
        ..ThreadForkParams::default()
    }
}

fn thread_cwd_from_config(
    config: &Config,
    _thread_params_mode: ThreadParamsMode,
    _remote_cwd_override: Option<&std::path::Path>,
) -> Option<String> {
    Some(config.cwd.to_string_lossy().to_string())
}

async fn started_thread_from_start_response(
    response: ThreadStartResponse,
    config: &Config,
    thread_params_mode: ThreadParamsMode,
) -> Result<CliRuntimeStartedThread> {
    let blocks_direct_input = thread_blocks_direct_input(&response.thread);
    let session =
        thread_session_state_from_thread_start_response(&response, config, thread_params_mode)
            .await
            .map_err(color_eyre::eyre::Report::msg)?;
    Ok(CliRuntimeStartedThread {
        session,
        turns: response.thread.turns,
        blocks_direct_input,
    })
}

async fn started_thread_from_resume_response(
    response: ThreadResumeResponse,
    config: &Config,
    thread_params_mode: ThreadParamsMode,
) -> Result<CliRuntimeStartedThread> {
    let blocks_direct_input = thread_blocks_direct_input(&response.thread);
    let session =
        thread_session_state_from_thread_resume_response(&response, config, thread_params_mode)
            .await
            .map_err(color_eyre::eyre::Report::msg)?;
    Ok(CliRuntimeStartedThread {
        session,
        turns: response.thread.turns,
        blocks_direct_input,
    })
}

async fn started_thread_from_fork_response(
    response: ThreadForkResponse,
    config: &Config,
    thread_params_mode: ThreadParamsMode,
) -> Result<CliRuntimeStartedThread> {
    let blocks_direct_input = thread_blocks_direct_input(&response.thread);
    let session =
        thread_session_state_from_thread_fork_response(&response, config, thread_params_mode)
            .await
            .map_err(color_eyre::eyre::Report::msg)?;
    Ok(CliRuntimeStartedThread {
        session,
        turns: response.thread.turns,
        blocks_direct_input,
    })
}

async fn thread_session_state_from_thread_start_response(
    response: &ThreadStartResponse,
    config: &Config,
    _thread_params_mode: ThreadParamsMode,
) -> Result<ThreadSessionState, String> {
    thread_session_state_from_thread_response(
        &response.thread.id,
        response.thread.forked_from_id.clone(),
        response.thread.name.clone(),
        response.thread.path.clone(),
        response.model.clone(),
        response.model_provider.clone(),
        response.service_tier.clone(),
        response.cwd.clone(),
        response.runtime_workspace_roots.clone(),
        response.instruction_source_path_uris(),
        response.reasoning_effort.clone(),
        config,
    )
    .await
}

async fn thread_session_state_from_thread_resume_response(
    response: &ThreadResumeResponse,
    config: &Config,
    _thread_params_mode: ThreadParamsMode,
) -> Result<ThreadSessionState, String> {
    thread_session_state_from_thread_response(
        &response.thread.id,
        response.thread.forked_from_id.clone(),
        response.thread.name.clone(),
        response.thread.path.clone(),
        response.model.clone(),
        response.model_provider.clone(),
        response.service_tier.clone(),
        response.cwd.clone(),
        response.runtime_workspace_roots.clone(),
        response.instruction_source_path_uris(),
        response.reasoning_effort.clone(),
        config,
    )
    .await
}

async fn thread_session_state_from_thread_fork_response(
    response: &ThreadForkResponse,
    config: &Config,
    _thread_params_mode: ThreadParamsMode,
) -> Result<ThreadSessionState, String> {
    thread_session_state_from_thread_response(
        &response.thread.id,
        response.thread.forked_from_id.clone(),
        response.thread.name.clone(),
        response.thread.path.clone(),
        response.model.clone(),
        response.model_provider.clone(),
        response.service_tier.clone(),
        response.cwd.clone(),
        response.runtime_workspace_roots.clone(),
        response.instruction_source_path_uris(),
        response.reasoning_effort.clone(),
        config,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "session mapping keeps explicit fields"
)]
async fn thread_session_state_from_thread_response(
    thread_id: &str,
    forked_from_id: Option<String>,
    thread_name: Option<String>,
    rollout_path: Option<PathBuf>,
    model: String,
    model_provider_id: String,
    service_tier: Option<String>,
    cwd: AbsolutePathBuf,
    runtime_workspace_roots: Vec<AbsolutePathBuf>,
    instruction_source_paths: Vec<PathUri>,
    reasoning_effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    config: &Config,
) -> Result<ThreadSessionState, String> {
    let thread_id = ThreadId::from_string(thread_id)
        .map_err(|err| format!("thread id `{thread_id}` is invalid: {err}"))?;
    let forked_from_id = forked_from_id
        .as_deref()
        .map(ThreadId::from_string)
        .transpose()
        .map_err(|err| format!("forked_from_id is invalid: {err}"))?;
    let history_config =
        codex_message_history::HistoryConfig::new(config.codex_home.clone(), &config.history);
    let (log_id, entry_count) = codex_message_history::history_metadata(&history_config).await;
    Ok(ThreadSessionState {
        thread_id,
        forked_from_id,
        fork_parent_title: None,
        thread_name,
        model,
        model_provider_id,
        service_tier,
        cwd,
        runtime_workspace_roots,
        instruction_source_paths,
        reasoning_effort,
        agent_settings: None,
        personality: config.personality,
        message_history: Some(MessageHistoryMetadata {
            log_id,
            entry_count,
        }),
        rollout_path,
    })
}

pub(crate) fn cli_runtime_rate_limit_snapshots(
    response: GetAccountRateLimitsResponse,
) -> Vec<RateLimitSnapshot> {
    let primary_limit_id = response.rate_limits.limit_id.clone();
    let mut snapshots = vec![response.rate_limits];
    if let Some(by_limit_id) = response.rate_limits_by_limit_id {
        snapshots.extend(by_limit_id.into_iter().filter_map(|(limit_id, snapshot)| {
            if primary_limit_id.as_deref().is_some_and(|primary_limit_id| {
                primary_limit_id == limit_id
                    || Some(primary_limit_id) == snapshot.limit_id.as_deref()
            }) {
                None
            } else {
                Some(snapshot)
            }
        }));
    }
    snapshots
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::legacy_core::config::ConfigBuilder;
    use crate::legacy_core::config::ConfigOverrides;
    use app_test_support::create_fake_paginated_rollout;
    use app_test_support::create_fake_rollout;
    use codex_cli_protocol::ThreadStatus;
    use codex_cli_protocol::Turn;
    use codex_cli_protocol::TurnStatus;
    use codex_features::Feature;
    use codex_protocol::config_types::Personality;
    use codex_protocol::config_types::ReasoningSummary;
    use codex_protocol::config_types::ServiceTier;
    use codex_protocol::config_types::Verbosity;
    use codex_protocol::config_types::WebSearchMode;
    use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_READ_ONLY;
    use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
    use codex_protocol::openai_models::ModelServiceTier;
    use codex_protocol::openai_models::ReasoningEffort;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use codex_utils_path_uri::LegacyAppPathString;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    async fn build_config(temp_dir: &TempDir) -> Config {
        ConfigBuilder::default()
            .codex_home(temp_dir.path().to_path_buf())
            .build()
            .await
            .expect("config should build")
    }

    fn rate_limit_snapshot(limit_id: &str) -> RateLimitSnapshot {
        RateLimitSnapshot {
            limit_id: Some(limit_id.to_string()),
            limit_name: None,
            primary: Some(codex_cli_protocol::RateLimitWindow {
                used_percent: 0,
                window_duration_mins: Some(10_080),
                resets_at: None,
            }),
            secondary: None,
            credits: None,
            individual_limit: None,
            spend_control_reached: None,
            plan_type: None,
            rate_limit_reached_type: None,
        }
    }

    #[test]
    fn cli_runtime_rate_limit_snapshots_deduplicates_top_level_limit_from_map() {
        let response = GetAccountRateLimitsResponse {
            rate_limits: rate_limit_snapshot("codex"),
            rate_limits_by_limit_id: Some(HashMap::from([
                ("codex".to_string(), rate_limit_snapshot("codex")),
                ("other".to_string(), rate_limit_snapshot("other")),
            ])),
            rate_limit_reset_credits: None,
        };

        let snapshots = cli_runtime_rate_limit_snapshots(response);

        assert_eq!(
            snapshots
                .iter()
                .map(|snapshot| snapshot.limit_id.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("codex"), Some("other")]
        );
    }

    #[test]
    fn thread_settings_update_compat_detects_unsupported_errors() {
        let cases = [
            (JSONRPC_METHOD_NOT_FOUND, "method not found", true),
            (
                JSONRPC_INVALID_REQUEST,
                "thread/settings/update requires experimentalApi capability",
                true,
            ),
            (
                JSONRPC_INVALID_REQUEST,
                "Invalid request: unknown variant `thread/settings/update`",
                true,
            ),
            (JSONRPC_INVALID_REQUEST, "invalid thread id", false),
        ];

        for (code, message, expected) in cases {
            let source = JSONRPCErrorError {
                code,
                data: None,
                message: message.to_string(),
            };
            assert_eq!(
                is_thread_settings_update_unsupported(&source),
                expected,
                "{message}"
            );
        }
    }

    #[test]
    fn history_pagination_compat_detects_unsupported_server_fields() {
        let cases = [
            (JSONRPC_INVALID_PARAMS, "unknown field `historyMode`", true),
            (
                JSONRPC_INVALID_REQUEST,
                "thread/resume.excludeTurns requires experimentalApi capability",
                true,
            ),
            (
                JSONRPC_INVALID_REQUEST,
                "thread/fork.excludeTurns requires experimentalApi capability",
                true,
            ),
            (
                JSONRPC_METHOD_NOT_FOUND,
                "unknown method thread/turns/list",
                true,
            ),
            (JSONRPC_METHOD_NOT_FOUND, "method not found", true),
            (
                JSONRPC_INVALID_PARAMS,
                "unknown variant \"paginated\", expected \"legacy\"",
                true,
            ),
            (
                JSONRPC_INVALID_PARAMS,
                "invalid enum value `paginated`",
                true,
            ),
            (
                JSONRPC_INVALID_PARAMS,
                "paginated thread was not found",
                false,
            ),
            (JSONRPC_INVALID_PARAMS, "invalid thread id", false),
        ];

        for (code, message, expected) in cases {
            let source = JSONRPCErrorError {
                code,
                data: None,
                message: message.to_string(),
            };
            assert_eq!(
                is_history_pagination_unsupported(&source),
                expected,
                "{message}"
            );
        }
    }

    #[tokio::test]
    async fn ephemeral_thread_start_does_not_request_paginated_history() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let mut config = build_config(&temp_dir).await;
        config.ephemeral = true;

        let params = thread_start_params_from_config(
            &config,
            ThreadParamsMode::Embedded,
            /*remote_cwd_override*/ None,
            /*session_start_source*/ None,
        );

        assert_eq!(params.ephemeral, Some(true));
        assert_eq!(params.history_mode, None);
    }

    #[tokio::test]
    async fn thread_start_params_include_cwd_for_embedded_sessions() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let config = ConfigBuilder::default()
            .codex_home(temp_dir.path().to_path_buf())
            .harness_overrides(ConfigOverrides {
                default_permissions: Some(BUILT_IN_PERMISSION_PROFILE_WORKSPACE.to_string()),
                ..ConfigOverrides::default()
            })
            .build()
            .await
            .expect("config should build");

        let params = thread_start_params_from_config(
            &config,
            ThreadParamsMode::Embedded,
            /*remote_cwd_override*/ None,
            /*session_start_source*/ None,
        );

        assert_eq!(params.cwd, Some(config.cwd.to_string_lossy().to_string()));
        assert_eq!(
            params.runtime_workspace_roots,
            Some(config.workspace_roots.clone())
        );
        assert_eq!(params.thread_source, Some(ThreadSource::User));
    }

    #[tokio::test]
    async fn thread_start_params_can_mark_clear_source() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let config = build_config(&temp_dir).await;

        let params = thread_start_params_from_config(
            &config,
            ThreadParamsMode::Embedded,
            /*remote_cwd_override*/ None,
            Some(ThreadStartSource::Clear),
        );

        assert_eq!(params.session_start_source, Some(ThreadStartSource::Clear));
    }

    #[tokio::test]
    async fn thread_lifecycle_params_forward_config_overrides_and_service_tier() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let mut config = build_config(&temp_dir).await;
        config.model_reasoning_effort = Some(ReasoningEffort::High);
        config.model_reasoning_summary = Some(ReasoningSummary::Detailed);
        config.model_verbosity = Some(Verbosity::Low);
        config.personality = Some(Personality::Pragmatic);
        config
            .web_search_mode
            .set(WebSearchMode::Disabled)
            .expect("test web search mode should be allowed");
        config.service_tier = Some(ServiceTier::Fast.request_value().to_string());
        let thread_id = ThreadId::new();

        let start = thread_start_params_from_config(
            &config,
            ThreadParamsMode::Embedded,
            /*remote_cwd_override*/ None,
            /*session_start_source*/ None,
        );
        let resume = thread_resume_params_from_config(
            config.clone(),
            thread_id,
            ThreadParamsMode::Embedded,
            /*remote_cwd_override*/ None,
            ResumeModelSettings::OverrideFromCurrentConfig,
        );
        let fork = thread_fork_params_from_config(
            config,
            thread_id,
            ThreadParamsMode::Embedded,
            /*remote_cwd_override*/ None,
        );

        let expected_service_tier = Some(Some(ServiceTier::Fast.request_value().to_string()));
        assert_eq!(start.service_tier, expected_service_tier);
        assert_eq!(resume.service_tier, expected_service_tier);
        assert_eq!(fork.service_tier, expected_service_tier);
        let string = |value: &str| serde_json::Value::String(value.to_string());
        let expected_config = HashMap::from([
            ("model_reasoning_effort".to_string(), string("high")),
            ("model_reasoning_summary".to_string(), string("detailed")),
            ("model_verbosity".to_string(), string("low")),
            ("personality".to_string(), string("pragmatic")),
            ("web_search".to_string(), string("disabled")),
        ]);
        assert_eq!(start.config, Some(expected_config.clone()));
        assert_eq!(resume.config, Some(expected_config.clone()));
        assert_eq!(fork.config, Some(expected_config));
    }

    #[tokio::test]
    async fn thread_resume_params_can_restore_persisted_model_settings() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let mut config = build_config(&temp_dir).await;
        config.model = Some("configured-model".to_string());
        config.model_reasoning_effort = Some(ReasoningEffort::Ultra);
        config.model_reasoning_summary = Some(ReasoningSummary::Detailed);

        let params = thread_resume_params_from_config(
            config,
            ThreadId::new(),
            ThreadParamsMode::Embedded,
            /*remote_cwd_override*/ None,
            ResumeModelSettings::RestoreFromThread,
        );

        assert_eq!(params.model, None);
        assert_eq!(
            params.config,
            Some(HashMap::from([
                (
                    "model_reasoning_summary".to_string(),
                    serde_json::Value::String("detailed".to_string()),
                ),
                (
                    "personality".to_string(),
                    serde_json::Value::String("pragmatic".to_string()),
                ),
                (
                    "web_search".to_string(),
                    serde_json::Value::String("cached".to_string()),
                ),
            ]))
        );
    }

    #[tokio::test]
    async fn persisted_resume_does_not_forward_implicit_service_tier() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let mut config = build_config(&codex_home).await;
        config.model = Some("gpt-5.4".to_string());
        config.service_tier = None;
        config
            .features
            .enable(Feature::FastMode)
            .expect("enable fast mode");
        let thread_id = ThreadId::from_string(
            &create_fake_rollout(
                codex_home.path(),
                "2025-01-05T12-00-00",
                "2025-01-05T12:00:00Z",
                "Saved user message",
                Some(config.model_provider_id.as_str()),
                /*git_info*/ None,
            )
            .expect("create source rollout"),
        )?;
        let mut cli_runtime = crate::start_embedded_cli_runtime_for_picker(&config).await?;
        let mut preset = crate::test_support::TEST_MODEL_PRESETS
            .iter()
            .find(|preset| preset.model == "gpt-5.4")
            .expect("gpt-5.4 test preset")
            .clone();
        preset.service_tiers = vec![ModelServiceTier {
            id: ServiceTier::Fast.request_value().to_string(),
            name: "fast".to_string(),
            description: "Fast tier".to_string(),
        }];
        preset.default_service_tier = Some(ServiceTier::Fast.request_value().to_string());
        cli_runtime.available_models = vec![preset];

        let resumed = cli_runtime
            .resume_thread(config, thread_id, ResumeModelSettings::RestoreFromThread)
            .await?;

        assert_eq!(resumed.session.service_tier, None);
        cli_runtime.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn side_fork_skips_parent_title_lookup_but_normal_ephemeral_fork_keeps_it() -> Result<()>
    {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let config = build_config(&codex_home).await;
        let source_thread_id = ThreadId::from_string(
            &create_fake_rollout(
                codex_home.path(),
                "2025-01-05T12-00-00",
                "2025-01-05T12:00:00Z",
                "Saved user message",
                Some(config.model_provider_id.as_str()),
                /*git_info*/ None,
            )
            .expect("create source rollout"),
        )?;
        let mut cli_runtime = crate::start_embedded_cli_runtime_for_picker(&config).await?;
        cli_runtime
            .resume_thread(
                config.clone(),
                source_thread_id,
                ResumeModelSettings::RestoreFromThread,
            )
            .await?;
        cli_runtime
            .thread_set_name(source_thread_id, "Source thread".to_string())
            .await?;

        let mut ephemeral_config = config;
        ephemeral_config.ephemeral = true;
        let normal_ephemeral_fork = cli_runtime
            .fork_thread(ephemeral_config.clone(), source_thread_id)
            .await?;
        let side_fork = cli_runtime
            .fork_side_thread(ephemeral_config, source_thread_id)
            .await?;

        assert_eq!(
            normal_ephemeral_fork.session.fork_parent_title.as_deref(),
            Some("Source thread")
        );
        assert_eq!(side_fork.session.fork_parent_title, None);
        cli_runtime.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn ephemeral_paginated_fork_skips_unsupported_history_hydration() -> Result<()> {
        let codex_home = tempfile::tempdir()?;
        let config = build_config(&codex_home).await;
        let source_thread_id = ThreadId::from_string(
            &create_fake_paginated_rollout(
                codex_home.path(),
                "2025-01-05T12-00-00",
                "2025-01-05T12:00:00Z",
                "Saved user message",
                Some(config.model_provider_id.as_str()),
                /*git_info*/ None,
            )
            .map_err(|error| {
                color_eyre::eyre::eyre!("failed to create paginated rollout: {error}")
            })?,
        )?;
        let mut cli_runtime = crate::start_embedded_cli_runtime_for_picker(&config).await?;
        let mut ephemeral_config = config;
        ephemeral_config.ephemeral = true;

        let fork = cli_runtime
            .fork_thread(ephemeral_config, source_thread_id)
            .await?;

        assert_eq!(fork.session.forked_from_id, Some(source_thread_id));
        assert!(fork.turns.is_empty());
        cli_runtime.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn side_fork_uses_one_request_for_long_paginated_history() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let mut config = build_config(&codex_home).await;
        config.terminal_resize_reflow.max_rows =
            crate::legacy_core::config::TerminalResizeReflowMaxRows::Limit(100);
        let filename_ts = "2025-01-05T12-00-00";
        let source_id = create_fake_paginated_rollout(
            codex_home.path(),
            filename_ts,
            "2025-01-05T12:00:00Z",
            "Saved user message",
            Some(config.model_provider_id.as_str()),
            /*git_info*/ None,
        )
        .expect("create long paginated source rollout");
        let source_path =
            app_test_support::rollout_path(codex_home.path(), filename_ts, source_id.as_str());
        let mut contents = std::fs::read_to_string(&source_path)?;
        let rollout_line = |ordinal: usize, payload: serde_json::Value| {
            serde_json::json!({
                "timestamp": "2025-01-05T12:00:00Z",
                "type": "event_msg",
                "payload": payload,
                "ordinal": ordinal,
            })
        };
        let started = rollout_line(
            /*ordinal*/ 3,
            serde_json::json!({
                "type": "task_started",
                "turn_id": "long-history-turn",
                "model_context_window": null,
            }),
        );
        contents.push_str(&format!("{started}\n"));
        for index in 0..256 {
            let item = rollout_line(
                index + 4,
                serde_json::json!({
                    "type": "item_completed",
                    "thread_id": source_id,
                    "turn_id": "long-history-turn",
                    "item": {
                        "type": "UserMessage",
                        "id": format!("long-history-user-{index}"),
                        "content": [{
                            "type": "text",
                            "text": format!("long history message {index}"),
                        }],
                    },
                }),
            );
            contents.push_str(&format!("{item}\n"));
        }
        std::fs::write(source_path, contents)?;

        let source_thread_id = ThreadId::from_string(source_id.as_str())?;
        let mut cli_runtime = crate::start_embedded_cli_runtime_for_picker(&config).await?;
        let resumed = cli_runtime
            .resume_thread(
                config.clone(),
                source_thread_id,
                ResumeModelSettings::RestoreFromThread,
            )
            .await?;
        let loaded_items: usize = resumed.turns.iter().map(|turn| turn.items.len()).sum();
        assert!(loaded_items <= HISTORY_ITEM_PAGE_LIMIT as usize);
        assert!(cli_runtime.has_older_history(source_thread_id));

        let mut side_config = config;
        side_config.ephemeral = true;
        let next_request_id = cli_runtime.next_request_id;
        let side = cli_runtime
            .fork_side_thread(side_config, source_thread_id)
            .await?;

        assert_eq!(cli_runtime.next_request_id, next_request_id + 1);
        assert_eq!(side.session.forked_from_id, Some(source_thread_id));
        assert_eq!(side.turns, Vec::<Turn>::new());
        assert!(cli_runtime.has_older_history(source_thread_id));
        assert!(
            !cli_runtime
                .history_pagination
                .contains_key(&side.session.thread_id)
        );

        cli_runtime.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn config_request_overrides_preserve_implicit_personality_default() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let mut config = build_config(&temp_dir).await;
        config.personality = None;

        let implicit_overrides =
            config_request_overrides_from_config(&config).expect("config overrides");

        assert!(!implicit_overrides.contains_key("personality"));

        config.personality = Some(Personality::None);
        let explicit_overrides =
            config_request_overrides_from_config(&config).expect("config overrides");

        assert_eq!(
            explicit_overrides.get("personality"),
            Some(&serde_json::Value::String("none".to_string()))
        );
    }

    #[tokio::test]
    async fn thread_fork_params_forward_instruction_overrides() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let mut config = build_config(&temp_dir).await;
        config.base_instructions = Some("Base override.".to_string());
        config.developer_instructions = Some("Developer override.".to_string());
        let thread_id = ThreadId::new();

        let params = thread_fork_params_from_config(
            config,
            thread_id,
            ThreadParamsMode::Embedded,
            /*remote_cwd_override*/ None,
        );

        assert_eq!(params.base_instructions.as_deref(), Some("Base override."));
        assert_eq!(
            params.developer_instructions.as_deref(),
            Some("Developer override.")
        );
    }

    #[tokio::test]
    async fn side_fork_excludes_turns_without_clearing_regular_ephemeral_fork() -> Result<()> {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let mut config = build_config(&codex_home).await;
        config.ephemeral = true;
        let thread_id = ThreadId::from_string(
            &create_fake_rollout(
                codex_home.path(),
                "2025-01-05T12-00-00",
                "2025-01-05T12:00:00Z",
                "Saved user message",
                Some(config.model_provider_id.as_str()),
                /*git_info*/ None,
            )
            .expect("create rollout"),
        )?;
        let mut cli_runtime = crate::start_embedded_cli_runtime_for_picker(&config).await?;

        let regular = cli_runtime.fork_thread(config.clone(), thread_id).await?;
        let side = cli_runtime.fork_side_thread(config, thread_id).await?;

        assert_eq!(regular.turns.len(), 1);
        assert!(matches!(
            regular.turns[0].items.as_slice(),
            [codex_cli_protocol::ThreadItem::UserMessage { content, .. }]
                if content == &[UserInput::Text {
                    text: "Saved user message".to_string(),
                    text_elements: Vec::new(),
                }]
        ));
        assert_eq!(side.turns, Vec::<Turn>::new());
        cli_runtime.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn terminal_visualization_instructions_are_gated_for_all_tui_thread_flows() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let mut config = build_config(&temp_dir).await;
        config.developer_instructions = Some("Developer override.".to_string());
        let thread_id = ThreadId::new();

        let control_start = thread_start_params_from_config(
            &config,
            ThreadParamsMode::Embedded,
            /*remote_cwd_override*/ None,
            /*session_start_source*/ None,
        );
        let control_resume = thread_resume_params_from_config(
            config.clone(),
            thread_id,
            ThreadParamsMode::Embedded,
            /*remote_cwd_override*/ None,
            ResumeModelSettings::OverrideFromCurrentConfig,
        );
        let control_fork = thread_fork_params_from_config(
            config.clone(),
            thread_id,
            ThreadParamsMode::Embedded,
            /*remote_cwd_override*/ None,
        );

        assert_eq!(control_start.developer_instructions, None);
        assert_eq!(control_resume.developer_instructions, None);
        assert_eq!(
            control_fork.developer_instructions.as_deref(),
            Some("Developer override.")
        );

        let _ = config
            .features
            .enable(Feature::TerminalVisualizationInstructions);
        let treatment_start = thread_start_params_from_config(
            &config,
            ThreadParamsMode::Embedded,
            /*remote_cwd_override*/ None,
            /*session_start_source*/ None,
        );
        let treatment_resume = thread_resume_params_from_config(
            config.clone(),
            thread_id,
            ThreadParamsMode::Embedded,
            /*remote_cwd_override*/ None,
            ResumeModelSettings::OverrideFromCurrentConfig,
        );
        let treatment_fork = thread_fork_params_from_config(
            config,
            thread_id,
            ThreadParamsMode::Embedded,
            /*remote_cwd_override*/ None,
        );
        let expected = format!(
            "Developer override.\n\n{}",
            crate::terminal_visualization_instructions::TERMINAL_VISUALIZATION_INSTRUCTIONS
        );

        assert_eq!(
            treatment_start.developer_instructions.as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(
            treatment_resume.developer_instructions.as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(
            treatment_fork.developer_instructions.as_deref(),
            Some(expected.as_str())
        );
    }

    #[tokio::test]
    async fn resume_response_restores_turns_from_thread_items() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let config = build_config(&temp_dir).await;
        let thread_id = ThreadId::new();
        let forked_from_id = ThreadId::new();
        let response = ThreadResumeResponse {
            thread: codex_cli_protocol::Thread {
                id: thread_id.to_string(),
                extra: None,
                session_id: ThreadId::new().to_string(),
                forked_from_id: Some(forked_from_id.to_string()),
                parent_thread_id: None,
                preview: "hello".to_string(),
                ephemeral: false,
                section: None,
                section_entered_at: None,
                history_mode: Default::default(),
                model_provider: "openai".to_string(),
                created_at: 1,
                updated_at: 2,
                recency_at: Some(2),
                status: ThreadStatus::Idle,
                path: None,
                cwd: test_path_buf("/tmp/project").abs(),
                cli_version: "0.0.0".to_string(),
                source: codex_cli_protocol::SessionSource::Cli,
                can_accept_direct_input: None,
                thread_source: None,
                agent_nickname: None,
                agent_role: None,
                git_info: None,
                name: None,
                turns: vec![Turn {
                    id: "turn-1".to_string(),
                    items_view: codex_cli_protocol::TurnItemsView::Full,
                    items: vec![
                        codex_cli_protocol::ThreadItem::UserMessage {
                            id: "user-1".to_string(),
                            client_id: None,
                            content: vec![codex_cli_protocol::UserInput::Text {
                                text: "hello from history".to_string(),
                                text_elements: Vec::new(),
                            }],
                        },
                        codex_cli_protocol::ThreadItem::AgentMessage {
                            id: "assistant-1".to_string(),
                            text: "assistant reply".to_string(),
                            phase: None,
                            memory_citation: None,
                        },
                    ],
                    status: TurnStatus::Completed,
                    error: None,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                }],
            },
            model: "gpt-5.4".to_string(),
            model_provider: "openai".to_string(),
            service_tier: None,
            cwd: test_path_buf("/tmp/project").abs(),
            runtime_workspace_roots: vec![
                test_path_buf("/tmp/project").abs(),
                test_path_buf("/tmp/project/extra").abs(),
            ],
            instruction_sources: vec![LegacyAppPathString::from_abs_path(
                &test_path_buf("/tmp/project/AGENTS.md").abs(),
            )],
            reasoning_effort: None,
            multi_agent_mode: Default::default(),
            initial_turns_page: None,
            turns_backwards_cursor: None,
            items_backwards_cursor: None,
        };

        let started = started_thread_from_resume_response(
            response.clone(),
            &config,
            ThreadParamsMode::Embedded,
        )
        .await
        .expect("resume response should map");
        assert_eq!(started.session.forked_from_id, Some(forked_from_id));
        assert_eq!(
            started.session.runtime_workspace_roots,
            response.runtime_workspace_roots
        );
        assert_eq!(
            started.session.instruction_source_paths,
            response.instruction_source_path_uris()
        );
        assert_eq!(started.turns.len(), 1);
        assert_eq!(started.turns[0], response.thread.turns[0]);
        assert!(!started.blocks_direct_input);

        let mut empty_roots_response = response;
        empty_roots_response.runtime_workspace_roots = Vec::new();
        let started = started_thread_from_resume_response(
            empty_roots_response,
            &config,
            ThreadParamsMode::Embedded,
        )
        .await
        .expect("resume response should map");
        assert_eq!(started.session.runtime_workspace_roots, Vec::new());
    }

    #[tokio::test]
    async fn session_configured_populates_history_metadata() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let config = build_config(&temp_dir).await;
        let thread_id = ThreadId::new();

        let history_config =
            codex_message_history::HistoryConfig::new(config.codex_home.clone(), &config.history);

        codex_message_history::append_entry("older", &thread_id, &history_config)
            .await
            .expect("history append should succeed");
        codex_message_history::append_entry("newer", &thread_id, &history_config)
            .await
            .expect("history append should succeed");

        let session = thread_session_state_from_thread_response(
            &thread_id.to_string(),
            /*forked_from_id*/ None,
            Some("restore".to_string()),
            /*rollout_path*/ None,
            "gpt-5.4".to_string(),
            "openai".to_string(),
            /*service_tier*/ None,
            test_path_buf("/tmp/project").abs(),
            Vec::new(),
            Vec::new(),
            /*reasoning_effort*/ None,
            &config,
        )
        .await
        .expect("session should map");

        let metadata = session
            .message_history
            .expect("session should include message-history metadata");
        assert_ne!(metadata.log_id, 0);
        assert_eq!(metadata.entry_count, 2);
    }

    #[tokio::test]
    async fn session_configured_preserves_fork_source_thread_id() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let config = build_config(&temp_dir).await;
        let thread_id = ThreadId::new();
        let forked_from_id = ThreadId::new();

        let session = thread_session_state_from_thread_response(
            &thread_id.to_string(),
            Some(forked_from_id.to_string()),
            Some("restore".to_string()),
            /*rollout_path*/ None,
            "gpt-5.4".to_string(),
            "openai".to_string(),
            /*service_tier*/ None,
            test_path_buf("/tmp/project").abs(),
            Vec::new(),
            Vec::new(),
            /*reasoning_effort*/ None,
            &config,
        )
        .await
        .expect("session should map");

        assert_eq!(session.forked_from_id, Some(forked_from_id));
    }

    #[test]
    fn status_account_display_from_auth_mode_uses_remapped_plan_labels() {
        let business = status_account_display_from_auth_mode(
            Some(AuthMode::Chatgpt),
            Some(codex_protocol::account::PlanType::EnterpriseCbpUsageBased),
        );
        assert!(matches!(
            business,
            Some(StatusAccountDisplay::ChatGpt {
                email: None,
                plan: Some(ref plan),
            }) if plan == "Enterprise"
        ));

        let team = status_account_display_from_auth_mode(
            Some(AuthMode::Chatgpt),
            Some(codex_protocol::account::PlanType::SelfServeBusinessUsageBased),
        );
        assert!(matches!(
            team,
            Some(StatusAccountDisplay::ChatGpt {
                email: None,
                plan: Some(ref plan),
            }) if plan == "Business"
        ));

        let business_prolite = status_account_display_from_auth_mode(
            Some(AuthMode::Chatgpt),
            Some(codex_protocol::account::PlanType::SelfServeBusinessProLite),
        );
        assert!(matches!(
            business_prolite,
            Some(StatusAccountDisplay::ChatGpt {
                email: None,
                plan: Some(ref plan),
            }) if plan == "Business"
        ));
    }
}
