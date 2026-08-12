use super::*;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::protocol::AdditionalContextEntry as CoreAdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind as CoreAdditionalContextKind;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

use crate::image_url::REMOTE_IMAGE_URL_ERROR;
use crate::image_url::is_remote_image_url;

const DIRECT_INPUT_TO_MULTI_AGENT_V2_SUBAGENT_ERROR: &str =
    "direct cli-runtime input is not allowed for multi-agent v2 sub-agents";

/// Mirrors the direct-input policy in both request validation and thread capability responses.
pub(super) fn can_accept_direct_input(
    multi_agent_version: Option<MultiAgentVersion>,
    session_source: &SessionSource,
) -> bool {
    multi_agent_version != Some(MultiAgentVersion::V2)
        || !matches!(
            session_source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        )
}

fn validate_user_input_image_urls(input: &[V2UserInput]) -> Result<(), JSONRPCErrorError> {
    if input.iter().any(|item| {
        matches!(
            item,
            V2UserInput::Image { url, .. } if is_remote_image_url(url)
        )
    }) {
        return Err(invalid_request(REMOTE_IMAGE_URL_ERROR));
    }
    Ok(())
}

fn validate_response_item_image_urls(items: &[ResponseItem]) -> Result<(), JSONRPCErrorError> {
    if items.iter().any(|item| match item {
        ResponseItem::Message { content, .. } => content.iter().any(|item| {
            matches!(
                item,
                ContentItem::InputImage { image_url, .. } if is_remote_image_url(image_url)
            )
        }),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            output.content_items().is_some_and(|content| {
                content.iter().any(|item| {
                    matches!(
                        item,
                        FunctionCallOutputContentItem::InputImage { image_url, .. }
                            if is_remote_image_url(image_url)
                    )
                })
            })
        }
        ResponseItem::Reasoning { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::AdditionalTools { .. }
        | ResponseItem::Other => false,
    }) {
        return Err(invalid_request(REMOTE_IMAGE_URL_ERROR));
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct TurnRequestProcessor {
    auth_manager: Arc<AuthManager>,
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    #[allow(dead_code)] // Retained turn processor dependency for compatibility requests.
    arg0_paths: Arg0DispatchPaths,
    #[allow(dead_code)]
    // Retained compatibility, test, or architectural seam for non-default consumers.
    config: Arc<Config>,
    #[allow(dead_code)] // Retained turn processor dependency for compatibility requests.
    config_manager: ConfigManager,
    pending_thread_unloads: Arc<Mutex<HashSet<ThreadId>>>,
    thread_state_manager: ThreadStateManager,
    thread_watch_manager: ThreadWatchManager,
    thread_list_state_permit: Arc<Semaphore>,
    skills_watcher: Arc<SkillsWatcher>,
}

fn map_additional_context(
    additional_context: Option<HashMap<String, AdditionalContextEntry>>,
) -> BTreeMap<String, CoreAdditionalContextEntry> {
    additional_context
        .unwrap_or_default()
        .into_iter()
        .map(|(key, entry)| {
            (
                key,
                CoreAdditionalContextEntry {
                    value: entry.value,
                    kind: match entry.kind {
                        AdditionalContextKind::Untrusted => CoreAdditionalContextKind::Untrusted,
                        AdditionalContextKind::Application => {
                            CoreAdditionalContextKind::Application
                        }
                    },
                },
            )
        })
        .collect()
}

struct ThreadSettingsBuildParams {
    environments: Option<TurnEnvironmentSelections>,
    model: Option<String>,
    service_tier: Option<Option<String>>,
    effort: Option<ReasoningEffort>,
    summary: Option<ReasoningSummary>,
    agent_settings: Option<AgentSettings>,
    personality: Option<Personality>,
}

impl TurnRequestProcessor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        auth_manager: Arc<AuthManager>,
        thread_manager: Arc<ThreadManager>,
        outgoing: Arc<OutgoingMessageSender>,
        arg0_paths: Arg0DispatchPaths,
        config: Arc<Config>,
        config_manager: ConfigManager,
        pending_thread_unloads: Arc<Mutex<HashSet<ThreadId>>>,
        thread_state_manager: ThreadStateManager,
        thread_watch_manager: ThreadWatchManager,
        thread_list_state_permit: Arc<Semaphore>,
        skills_watcher: Arc<SkillsWatcher>,
    ) -> Self {
        Self {
            auth_manager,
            thread_manager,
            outgoing,
            arg0_paths,
            config,
            config_manager,
            pending_thread_unloads,
            thread_state_manager,
            thread_watch_manager,
            thread_list_state_permit,
            skills_watcher,
        }
    }

    pub(crate) async fn turn_start(
        &self,
        request_id: ConnectionRequestId,
        params: TurnStartParams,
        cli_runtime_client_name: Option<String>,
        cli_runtime_client_version: Option<String>,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        validate_user_input_image_urls(&params.input)?;
        self.turn_start_inner(
            request_id,
            params,
            cli_runtime_client_name,
            cli_runtime_client_version,
        )
        .await
        .map(|response| Some(response.into()))
    }

    pub(crate) async fn thread_inject_items(
        &self,
        params: ThreadInjectItemsParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_inject_items_response_inner(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn thread_settings_update(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadSettingsUpdateParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_settings_update_inner(request_id, params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn turn_steer(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnSteerParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        validate_user_input_image_urls(&params.input)?;
        self.turn_steer_inner(request_id, params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn turn_interrupt(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnInterruptParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let result = self.turn_interrupt_inner(request_id, params).await;
        if let Err(error) = &result {
            self.log_error_response(request_id, error);
        }
        result.map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_start(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStartParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_start_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_append_audio(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendAudioParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_append_audio_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_append_text(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendTextParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_append_text_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_append_speech(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendSpeechParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_append_speech_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_stop(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStopParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_stop_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_list_voices(
        &self,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        Ok(Some(
            ThreadRealtimeListVoicesResponse {
                voices: RealtimeVoicesList::builtin(),
            }
            .into(),
        ))
    }

    fn log_error_response(&self, request_id: &ConnectionRequestId, error: &JSONRPCErrorError) {
        tracing::debug!(
            connection_id = request_id.connection_id.0,
            request_id = %request_id.request_id,
            ?error,
            "cli-runtime request failed"
        );
    }

    async fn load_thread(
        &self,
        thread_id: &str,
    ) -> Result<(ThreadId, Arc<CodexThread>), JSONRPCErrorError> {
        // Resolve the core conversation handle from a v2 thread id string.
        let thread_id = ThreadId::from_string(thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;

        let thread = self
            .thread_manager
            .get_thread(thread_id)
            .await
            .map_err(|_| invalid_request(format!("thread not found: {thread_id}")))?;

        Ok((thread_id, thread))
    }

    async fn ensure_direct_input_allowed(
        &self,
        request_id: &ConnectionRequestId,
        thread: &CodexThread,
    ) -> Result<(), JSONRPCErrorError> {
        let config_snapshot = thread.config_snapshot().await;
        if !can_accept_direct_input(
            thread.multi_agent_version(),
            &config_snapshot.session_source,
        ) {
            let error = invalid_request(DIRECT_INPUT_TO_MULTI_AGENT_V2_SUBAGENT_ERROR);
            self.log_error_response(request_id, &error);
            return Err(error);
        }

        Ok(())
    }

    async fn request_trace_context(
        &self,
        request_id: &ConnectionRequestId,
    ) -> Option<codex_protocol::protocol::W3cTraceContext> {
        self.outgoing.request_trace_context(request_id).await
    }

    async fn submit_core_op(
        &self,
        request_id: &ConnectionRequestId,
        thread: &CodexThread,
        op: Op,
    ) -> CodexResult<String> {
        thread
            .submit_with_trace(op, self.request_trace_context(request_id).await)
            .await
    }

    fn input_too_large_error(actual_chars: usize) -> JSONRPCErrorError {
        let mut error = invalid_params(format!(
            "Input exceeds the maximum length of {MAX_USER_INPUT_TEXT_CHARS} characters."
        ));
        error.data = Some(serde_json::json!({
            "input_error_code": INPUT_TOO_LARGE_ERROR_CODE,
            "max_chars": MAX_USER_INPUT_TEXT_CHARS,
            "actual_chars": actual_chars,
        }));
        error
    }

    fn validate_v2_input_limit(items: &[V2UserInput]) -> Result<(), JSONRPCErrorError> {
        let actual_chars: usize = items.iter().map(V2UserInput::text_char_count).sum();
        if actual_chars > MAX_USER_INPUT_TEXT_CHARS {
            return Err(Self::input_too_large_error(actual_chars));
        }
        Ok(())
    }

    async fn turn_start_inner(
        &self,
        request_id: ConnectionRequestId,
        params: TurnStartParams,
        cli_runtime_client_name: Option<String>,
        cli_runtime_client_version: Option<String>,
    ) -> Result<TurnStartResponse, JSONRPCErrorError> {
        let (thread_id, thread) =
            self.load_thread(&params.thread_id)
                .await
                .inspect_err(|error| {
                    self.log_error_response(&request_id, error);
                })?;
        self.ensure_direct_input_allowed(&request_id, thread.as_ref())
            .await?;
        if let Err(error) = Self::validate_v2_input_limit(&params.input) {
            self.log_error_response(&request_id, &error);
            return Err(error);
        }
        Self::set_cli_runtime_client_info(
            thread.as_ref(),
            cli_runtime_client_name,
            cli_runtime_client_version,
        )
        .await
        .inspect_err(|error| {
            self.log_error_response(&request_id, error);
        })?;
        let runtime_workspace_roots = params
            .runtime_workspace_roots
            .map(resolve_runtime_workspace_roots);
        let environment_selections =
            resolve_turn_environment_selections(self.thread_manager.as_ref(), params.environments)?;

        // Map v2 input items to core input items.
        let mapped_items: Vec<CoreInputItem> = params
            .input
            .into_iter()
            .map(V2UserInput::into_core)
            .collect();
        let client_user_message_id = params.client_user_message_id;
        let additional_context = map_additional_context(params.additional_context);
        let turn_has_input = !mapped_items.is_empty();
        let cwd = resolve_request_cwd(params.cwd)?;
        let environments = self
            .build_environment_override(
                thread.as_ref(),
                cwd,
                runtime_workspace_roots,
                environment_selections,
            )
            .await;
        let thread_settings = self
            .build_thread_settings_overrides(
                thread.as_ref(),
                ThreadSettingsBuildParams {
                    environments,
                    model: params.model,
                    service_tier: params.service_tier,
                    effort: params.effort,
                    summary: params.summary,
                    agent_settings: params.agent_settings,
                    personality: params.personality,
                },
            )
            .await?;
        // Start the turn by submitting the user input. Return its submission id as turn_id.
        let turn_op = Op::UserInput {
            items: mapped_items,
            final_output_json_schema: params.output_schema,
            responsesapi_client_metadata: params.responsesapi_client_metadata,
            additional_context,
            thread_settings,
        };
        let turn_id = thread
            .submit_user_input_with_client_user_message_id(
                turn_op,
                self.request_trace_context(&request_id).await,
                client_user_message_id,
            )
            .await
            .map_err(|err| {
                let error = internal_error(format!("failed to start turn: {err}"));
                self.log_error_response(&request_id, &error);
                error
            })?;

        if turn_has_input {
            let config_snapshot = thread.config_snapshot().await;
            codex_memories_write::start_memories_startup_task(
                Arc::clone(&self.thread_manager),
                Arc::clone(&self.auth_manager),
                thread_id,
                Arc::clone(&thread),
                thread.config().await,
                config_snapshot.permission_profile,
                &config_snapshot.session_source,
            );
        }

        self.outgoing
            .record_request_turn_id(&request_id, &turn_id)
            .await;
        let turn = Turn {
            id: turn_id,
            items: vec![],
            items_view: TurnItemsView::NotLoaded,
            error: None,
            status: TurnStatus::InProgress,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        };

        Ok(TurnStartResponse { turn })
    }

    async fn build_environment_override(
        &self,
        thread: &CodexThread,
        cwd: Option<AbsolutePathBuf>,
        workspace_roots: Option<Vec<AbsolutePathBuf>>,
        environment_selections: Option<Vec<TurnEnvironmentSelection>>,
    ) -> Option<TurnEnvironmentSelections> {
        if cwd.is_none() && workspace_roots.is_none() && environment_selections.is_none() {
            return None;
        }

        // Explicit environment selections own their roots and pass through unchanged. Top-level
        // `runtimeWorkspaceRoots` is only a compatibility input for default environments.
        if let Some(environment_selections) = environment_selections {
            let legacy_fallback_cwd = match cwd {
                Some(cwd) => cwd,
                None => match environment_selections
                    .iter()
                    .find(|selection| selection.environment_id == LOCAL_ENVIRONMENT_ID)
                    .and_then(|selection| selection.cwd.to_abs_path().ok())
                {
                    Some(cwd) => cwd,
                    None => thread.config_snapshot().await.cwd().clone(),
                },
            };
            return Some(TurnEnvironmentSelections::new(
                legacy_fallback_cwd,
                environment_selections,
            ));
        }

        let snapshot = thread.config_snapshot().await;
        let current_cwd = snapshot.cwd().clone();
        let legacy_fallback_cwd = cwd.unwrap_or_else(|| current_cwd.clone());
        let workspace_roots = match workspace_roots {
            Some(workspace_roots) => workspace_roots,
            None => {
                // Match the pre-environment partial-update behavior: a cwd-only update retargets
                // the old cwd root while preserving any additional roots. Deduplicate because the
                // new cwd may already be present as an additional root.
                let mut retargeted_workspace_roots = Vec::new();
                for root in snapshot.workspace_roots {
                    let root = if root == current_cwd {
                        legacy_fallback_cwd.clone()
                    } else {
                        root
                    };
                    if !retargeted_workspace_roots.contains(&root) {
                        retargeted_workspace_roots.push(root);
                    }
                }
                retargeted_workspace_roots
            }
        };
        let environment_selections = self
            .thread_manager
            .default_environment_selections(&legacy_fallback_cwd, &workspace_roots);
        Some(TurnEnvironmentSelections::new(
            legacy_fallback_cwd,
            environment_selections,
        ))
    }

    async fn build_thread_settings_overrides(
        &self,
        thread: &CodexThread,
        params: ThreadSettingsBuildParams,
    ) -> Result<codex_protocol::protocol::ThreadSettingsOverrides, JSONRPCErrorError> {
        let ThreadSettingsBuildParams {
            environments,
            model,
            service_tier,
            effort,
            summary,
            agent_settings,
            personality,
        } = params;

        let has_environment_override = environments.is_some();
        let has_any_overrides = has_environment_override
            || model.is_some()
            || service_tier.is_some()
            || effort.is_some()
            || summary.is_some()
            || agent_settings.is_some()
            || personality.is_some();

        let effort = effort.map(Some);

        if has_any_overrides {
            thread
                .preview_thread_settings_overrides(CodexThreadSettingsOverrides {
                    environments: environments.clone(),
                    profile_workspace_roots: None,
                    approval_policy: None,
                    approvals_reviewer: None,
                    sandbox_policy: None,
                    permission_profile: None,
                    active_permission_profile: None,
                    windows_sandbox_level: None,
                    model: model.clone(),
                    effort: effort.clone(),
                    summary,
                    service_tier: service_tier.clone(),
                    agent_settings: agent_settings.clone(),
                    personality,
                })
                .await
                .map_err(|err| {
                    invalid_request(format!("invalid thread settings override: {err}"))
                })?;
        }

        Ok(codex_protocol::protocol::ThreadSettingsOverrides {
            environments,
            profile_workspace_roots: None,
            approval_policy: None,
            approvals_reviewer: None,
            sandbox_policy: None,
            permission_profile: None,
            active_permission_profile: None,
            windows_sandbox_level: None,
            model,
            effort,
            summary,
            service_tier,
            agent_settings,
            personality,
        })
    }

    async fn thread_settings_update_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadSettingsUpdateParams,
    ) -> Result<ThreadSettingsUpdateResponse, JSONRPCErrorError> {
        let (_, thread) = self.load_thread(&params.thread_id).await?;
        let cwd = resolve_request_cwd(params.cwd)?;
        let environments = self
            .build_environment_override(
                thread.as_ref(),
                cwd,
                /*workspace_roots*/ None,
                /*environment_selections*/ None,
            )
            .await;
        let thread_settings = self
            .build_thread_settings_overrides(
                thread.as_ref(),
                ThreadSettingsBuildParams {
                    environments,
                    model: params.model,
                    service_tier: params.service_tier,
                    effort: params.effort,
                    summary: params.summary,
                    agent_settings: params.agent_settings,
                    personality: params.personality,
                },
            )
            .await?;

        if thread_settings != codex_protocol::protocol::ThreadSettingsOverrides::default() {
            self.submit_core_op(
                request_id,
                thread.as_ref(),
                Op::ThreadSettings { thread_settings },
            )
            .await
            .map_err(|err| internal_error(format!("failed to update thread settings: {err}")))?;
        }

        Ok(ThreadSettingsUpdateResponse {})
    }

    async fn thread_inject_items_response_inner(
        &self,
        params: ThreadInjectItemsParams,
    ) -> Result<ThreadInjectItemsResponse, JSONRPCErrorError> {
        let (_, thread) = self.load_thread(&params.thread_id).await?;

        let items = params
            .items
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                serde_json::from_value::<ResponseItem>(value)
                    .map_err(|err| format!("items[{index}] is not a valid response item: {err}"))
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(invalid_request)?;
        validate_response_item_image_urls(&items)?;

        thread
            .inject_response_items(items)
            .await
            .map_err(|err| match err.details() {
                CodexErrorDetails::InvalidRequest(message) => invalid_request(message.clone()),
                _ => internal_error(format!("failed to inject response items: {err}")),
            })?;
        Ok(ThreadInjectItemsResponse {})
    }

    async fn set_cli_runtime_client_info(
        thread: &CodexThread,
        cli_runtime_client_name: Option<String>,
        cli_runtime_client_version: Option<String>,
    ) -> Result<(), JSONRPCErrorError> {
        thread
            .set_cli_runtime_client_info(cli_runtime_client_name, cli_runtime_client_version)
            .await
            .map_err(|err| internal_error(format!("failed to set app server client info: {err}")))
    }

    async fn turn_steer_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnSteerParams,
    ) -> Result<TurnSteerResponse, JSONRPCErrorError> {
        let (_, thread) = self
            .load_thread(&params.thread_id)
            .await
            .inspect_err(|error| {
                self.log_error_response(request_id, error);
            })?;
        self.ensure_direct_input_allowed(request_id, thread.as_ref())
            .await?;

        if params.expected_turn_id.is_empty() {
            return Err(invalid_request("expectedTurnId must not be empty"));
        }
        self.outgoing
            .record_request_turn_id(request_id, &params.expected_turn_id)
            .await;
        if let Err(error) = Self::validate_v2_input_limit(&params.input) {
            self.log_error_response(request_id, &error);
            return Err(error);
        }

        let mapped_items: Vec<CoreInputItem> = params
            .input
            .into_iter()
            .map(V2UserInput::into_core)
            .collect();
        let additional_context = map_additional_context(params.additional_context);

        let turn_id = thread
            .steer_input(
                mapped_items,
                additional_context,
                Some(&params.expected_turn_id),
                params.client_user_message_id,
                params.responsesapi_client_metadata,
            )
            .await
            .map_err(|err| {
                let (message, data) = match err {
                    SteerInputError::NoActiveTurn(_) => {
                        ("no active turn to steer".to_string(), None)
                    }
                    SteerInputError::ExpectedTurnMismatch { expected, actual } => (
                        format!("expected active turn id `{expected}` but found `{actual}`"),
                        None,
                    ),
                    SteerInputError::ActiveTurnNotSteerable { turn_kind } => {
                        let message = match turn_kind {
                            codex_protocol::protocol::NonSteerableTurnKind::Compact => {
                                "cannot steer a compact turn".to_string()
                            }
                        };
                        let error = TurnError {
                            message: message.clone(),
                            codex_error_info: Some(CodexErrorInfo::ActiveTurnNotSteerable {
                                turn_kind: turn_kind.into(),
                            }),
                            additional_details: None,
                        };
                        let data = match serde_json::to_value(error) {
                            Ok(data) => Some(data),
                            Err(error) => {
                                tracing::error!(
                                    ?error,
                                    "failed to serialize active-turn-not-steerable turn error"
                                );
                                None
                            }
                        };
                        (message, data)
                    }
                    SteerInputError::EmptyInput => ("input must not be empty".to_string(), None),
                };
                let mut error = invalid_request(message);
                error.data = data;
                self.log_error_response(request_id, &error);
                error
            })?;
        Ok(TurnSteerResponse { turn_id })
    }

    async fn prepare_realtime_conversation_thread(
        &self,
        request_id: &ConnectionRequestId,
        thread_id: &str,
    ) -> Result<Option<(ThreadId, Arc<CodexThread>)>, JSONRPCErrorError> {
        let (thread_id, thread) = self.load_thread(thread_id).await?;

        match self
            .ensure_conversation_listener(
                thread_id,
                request_id.connection_id,
                /*raw_events_enabled*/ false,
            )
            .await
        {
            Ok(EnsureConversationListenerResult::Attached) => {}
            Ok(EnsureConversationListenerResult::ConnectionClosed) => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }

        if !thread.enabled(Feature::RealtimeConversation) {
            return Err(invalid_request(format!(
                "thread {thread_id} does not support realtime conversation"
            )));
        }

        Ok(Some((thread_id, thread)))
    }

    async fn thread_realtime_start_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStartParams,
    ) -> Result<Option<ThreadRealtimeStartResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RealtimeConversationStart(ConversationStartParams {
                client_managed_handoffs: params.client_managed_handoffs.unwrap_or(false),
                delegation_ack_filler: params.delegation_ack_filler,
                flush_transcript_tail_on_session_end: params
                    .flush_transcript_tail_on_session_end
                    .unwrap_or(false),
                codex_responses_as_items: params.codex_responses_as_items.unwrap_or(false),
                codex_response_item_prefix: params.codex_response_item_prefix,
                codex_response_handoff_mode: params.codex_response_handoff_mode.unwrap_or_default(),
                codex_response_handoff_channel_prefixes: params
                    .codex_response_handoff_channel_prefixes,
                model: params.model,
                output_modality: params.output_modality,
                include_startup_context: params.include_startup_context.unwrap_or(true),
                initial_items: params
                    .initial_items
                    .unwrap_or_default()
                    .into_iter()
                    .map(|item| ConversationTextParams {
                        text: item.text,
                        role: item.role,
                    })
                    .collect(),
                realtime_start_instructions: params.realtime_start_instructions,
                realtime_end_instructions: params.realtime_end_instructions,
                prompt: params.prompt,
                realtime_session_id: params.realtime_session_id,
                transport: params.transport.map(|transport| match transport {
                    ThreadRealtimeStartTransport::Websocket => {
                        ConversationStartTransport::Websocket
                    }
                    ThreadRealtimeStartTransport::Webrtc { sdp } => {
                        ConversationStartTransport::Webrtc { sdp }
                    }
                }),
                version: params.version,
                voice: params.voice,
            }),
        )
        .await
        .map_err(|err| internal_error(format!("failed to start realtime conversation: {err}")))?;
        Ok(Some(ThreadRealtimeStartResponse::default()))
    }

    async fn thread_realtime_append_audio_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendAudioParams,
    ) -> Result<Option<ThreadRealtimeAppendAudioResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RealtimeConversationAudio(ConversationAudioParams {
                frame: params.audio.into(),
            }),
        )
        .await
        .map_err(|err| {
            internal_error(format!(
                "failed to append realtime conversation audio: {err}"
            ))
        })?;
        Ok(Some(ThreadRealtimeAppendAudioResponse::default()))
    }

    async fn thread_realtime_append_text_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendTextParams,
    ) -> Result<Option<ThreadRealtimeAppendTextResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RealtimeConversationText(ConversationTextParams {
                text: params.text,
                role: params.role,
            }),
        )
        .await
        .map_err(|err| {
            internal_error(format!(
                "failed to append realtime conversation text: {err}"
            ))
        })?;
        Ok(Some(ThreadRealtimeAppendTextResponse::default()))
    }

    async fn thread_realtime_append_speech_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendSpeechParams,
    ) -> Result<Option<ThreadRealtimeAppendSpeechResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RealtimeConversationSpeech(ConversationSpeechParams { text: params.text }),
        )
        .await
        .map_err(|err| {
            internal_error(format!(
                "failed to append realtime conversation speech: {err}"
            ))
        })?;
        Ok(Some(ThreadRealtimeAppendSpeechResponse::default()))
    }

    async fn thread_realtime_stop_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStopParams,
    ) -> Result<Option<ThreadRealtimeStopResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(request_id, thread.as_ref(), Op::RealtimeConversationClose)
            .await
            .map_err(|err| {
                internal_error(format!("failed to stop realtime conversation: {err}"))
            })?;
        Ok(Some(ThreadRealtimeStopResponse::default()))
    }

    async fn turn_interrupt_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnInterruptParams,
    ) -> Result<Option<TurnInterruptResponse>, JSONRPCErrorError> {
        let TurnInterruptParams { thread_id, turn_id } = params;
        let is_startup_interrupt = turn_id.is_empty();

        let (thread_uuid, thread) = self.load_thread(&thread_id).await?;

        // Record turn interrupts so we can reply when TurnAborted arrives. Startup
        // interrupts do not have a turn and are acknowledged after submission.
        if !is_startup_interrupt {
            let thread_state = self.thread_state_manager.thread_state(thread_uuid).await;
            let is_running = matches!(thread.agent_status().await, AgentStatus::Running);
            {
                let mut thread_state = thread_state.lock().await;
                if let Some(active_turn) = thread_state.active_turn_snapshot() {
                    if active_turn.id != turn_id {
                        return Err(invalid_request(format!(
                            "expected active turn id {turn_id} but found {}",
                            active_turn.id
                        )));
                    }
                } else if thread_state.last_terminal_turn_id.as_deref() == Some(turn_id.as_str())
                    || !is_running
                {
                    return Err(invalid_request("no active turn to interrupt"));
                }
                thread_state.pending_interrupts.push(request_id.clone());
            }

            self.outgoing
                .record_request_turn_id(request_id, &turn_id)
                .await;
        }

        // Submit the interrupt. Turn interrupts respond upon TurnAborted; startup
        // interrupts respond here because startup cancellation has no turn event.
        match self
            .submit_core_op(request_id, thread.as_ref(), Op::Interrupt)
            .await
        {
            Ok(_) if is_startup_interrupt => Ok(Some(TurnInterruptResponse {})),
            Ok(_) => Ok(None),
            Err(err) => {
                if !is_startup_interrupt {
                    let thread_state = self.thread_state_manager.thread_state(thread_uuid).await;
                    let mut thread_state = thread_state.lock().await;
                    thread_state
                        .pending_interrupts
                        .retain(|pending_request_id| pending_request_id != request_id);
                }
                let interrupt_target = if is_startup_interrupt {
                    "startup"
                } else {
                    "turn"
                };
                Err(internal_error(format!(
                    "failed to interrupt {interrupt_target}: {err}"
                )))
            }
        }
    }

    fn listener_task_context(&self) -> ListenerTaskContext {
        ListenerTaskContext {
            thread_manager: Arc::clone(&self.thread_manager),
            thread_state_manager: self.thread_state_manager.clone(),
            outgoing: Arc::clone(&self.outgoing),
            pending_thread_unloads: Arc::clone(&self.pending_thread_unloads),
            thread_watch_manager: self.thread_watch_manager.clone(),
            thread_list_state_permit: self.thread_list_state_permit.clone(),
            fallback_model_provider: self.config.model_provider_id.clone(),
            codex_home: self.config.codex_home.to_path_buf(),
            skills_watcher: Arc::clone(&self.skills_watcher),
        }
    }

    async fn ensure_conversation_listener(
        &self,
        conversation_id: ThreadId,
        connection_id: ConnectionId,
        raw_events_enabled: bool,
    ) -> Result<EnsureConversationListenerResult, JSONRPCErrorError> {
        super::thread_lifecycle::ensure_conversation_listener(
            self.listener_task_context(),
            conversation_id,
            connection_id,
            raw_events_enabled,
        )
        .await
    }
}
