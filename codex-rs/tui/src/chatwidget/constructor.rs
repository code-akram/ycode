//! Construction and initial wiring for `ChatWidget`.

use super::*;

impl ChatWidget {
    pub(crate) fn new_with_app_event(common: ChatWidgetInit) -> Self {
        Self::new_with_op_target(common, CodexOpTarget::AppEvent)
    }

    pub(super) fn new_with_op_target(
        common: ChatWidgetInit,
        codex_op_target: CodexOpTarget,
    ) -> Self {
        let ChatWidgetInit {
            config,
            frame_requester,
            app_event_tx,
            workspace_command_runner,
            initial_user_message,
            enhanced_keys_supported,
            has_chatgpt_account,
            has_codex_backend_auth,
            model_catalog,
            is_first_run,
            status_account_display,
            runtime_model_provider_base_url,
            initial_plan_type,
            model,
        } = common;
        let model = model.filter(|m| !m.trim().is_empty());
        let mut config = config;
        config.model = model.clone();
        let prevent_idle_sleep = config.features.enabled(Feature::PreventIdleSleep);
        let mut rng = rand::rng();
        let placeholder = PLACEHOLDERS[rng.random_range(0..PLACEHOLDERS.len())].to_string();
        let side_placeholder =
            SIDE_PLACEHOLDERS[rng.random_range(0..SIDE_PLACEHOLDERS.len())].to_string();

        let model_for_header = model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL_DISPLAY_NAME.to_string());
        let header_model = model_for_header.clone();
        let fallback_default = Settings {
            model: header_model.clone(),
            reasoning_effort: None,
            developer_instructions: None,
        };
        let current_agent_settings = AgentSettings {
            settings: fallback_default,
        };

        let active_cell = Some(Self::placeholder_session_header_cell(&config));

        let current_cwd = Some(config.cwd.to_path_buf());
        let effective_service_tier = crate::service_tier_resolution::effective_service_tier(
            &config,
            &header_model,
            &model_catalog.try_list_models().unwrap_or_default(),
        );
        let current_terminal_info = terminal_info();
        let runtime_keymap = RuntimeKeymap::from_config(&config.tui_keymap).ok();
        let default_keymap = RuntimeKeymap::defaults();
        let copy_last_response_binding = runtime_keymap
            .as_ref()
            .map(|keymap| keymap.app.copy.clone())
            .unwrap_or_else(|| default_keymap.app.copy.clone());
        let chat_keymap = runtime_keymap
            .as_ref()
            .map(|keymap| keymap.chat.clone())
            .unwrap_or_else(|| default_keymap.chat.clone());
        let queued_message_edit_hint_binding = queued_message_edit_hint_binding(
            runtime_keymap.as_ref().unwrap_or(&default_keymap),
            current_terminal_info,
        );
        let mut widget = Self {
            app_event_tx: app_event_tx.clone(),
            frame_requester: frame_requester.clone(),
            codex_op_target,
            bottom_pane: BottomPane::new(BottomPaneParams {
                frame_requester,
                app_event_tx,
                has_input_focus: true,
                enhanced_keys_supported,
                placeholder_text: placeholder.clone(),
                disable_paste_burst: config.disable_paste_burst,
                animations_enabled: false,
                skills: None,
            }),
            transcript: TranscriptState::new(active_cell),
            raw_output_mode: config.tui_raw_output_mode,
            config,
            effective_service_tier,
            skills_all: Vec::new(),
            skills_initial_state: None,
            current_agent_settings,
            has_chatgpt_account,
            has_codex_backend_auth,
            model_catalog,
            session_header: SessionHeader::new(header_model),
            initial_user_message,
            status_account_display,
            runtime_model_provider_base_url,
            remote_connection: None,
            token_info: None,
            token_usage_pending: false,
            rate_limit_snapshots_by_limit_id: BTreeMap::new(),
            refreshing_status_outputs: Vec::new(),
            next_status_refresh_request_id: 0,
            refreshing_token_activity_output: None,
            completed_token_activity_output: None,
            next_token_activity_request_id: 0,
            pending_rate_limit_reset_request_id: None,
            pending_rate_limit_reset_idempotency_key: None,
            rate_limit_reset_picker_request_id: None,
            pending_rate_limit_reset_hint_request_id: None,
            pending_usage_menu_rate_limit_request_id: None,
            pending_rate_limit_reset_hint: None,
            available_rate_limit_reset_credits: None,
            next_rate_limit_reset_request_id: 0,
            plan_type: initial_plan_type,
            codex_rate_limit_reached_type: None,
            codex_spend_control_reached: None,
            rate_limit_warnings: RateLimitWarningState::default(),
            warning_display_state: WarningDisplayState::default(),
            rate_limit_switch_prompt: RateLimitSwitchPromptState::default(),
            add_credits_nudge_email_in_flight: None,
            adaptive_chunking: AdaptiveChunkingPolicy::default(),
            stream_controller: None,
            pending_stream_consolidations: 0,
            clipboard_lease: None,
            copy_last_response_binding,
            running_commands: HashMap::new(),
            collab_agent_metadata: HashMap::new(),
            pending_collab_spawn_requests: HashMap::new(),
            suppressed_exec_calls: HashSet::new(),
            last_unified_wait: None,
            unified_exec_wait_streak: None,
            turn_lifecycle: TurnLifecycleState::new(prevent_idle_sleep),
            safety_buffering: SafetyBufferingState::default(),
            task_complete_pending: false,
            unified_exec_processes: Vec::new(),
            ide_context: IdeContextState::default(),
            interrupts: InterruptManager::new(),
            reasoning_buffer: String::new(),
            reasoning_header: None,
            reasoning_summary_parts: Vec::new(),
            status_state: StatusState::default(),
            thread_id: None,
            thread_name: None,
            thread_rename_block_message: None,
            active_side_conversation: false,
            blocks_direct_input: false,
            normal_placeholder_text: placeholder,
            side_placeholder_text: side_placeholder,
            forked_from: None,
            interrupted_turn_notice_mode: InterruptedTurnNoticeMode::Default,
            input_queue: InputQueueState::default(),
            safety_buffering_prompt: None,
            chat_keymap,
            queued_message_edit_hint_binding,
            show_welcome_banner: is_first_run,
            suppress_session_configured_redraw: false,
            suppress_initial_user_message_submit: false,
            pending_notification: None,
            quit_shortcut_expires_at: None,
            quit_shortcut_key: None,
            last_rendered_width: std::cell::Cell::new(None),
            current_rollout_path: None,
            current_cwd,
            workspace_command_runner,
            instruction_source_paths: Vec::new(),
            last_terminal_title: None,
            last_terminal_title_requires_action: false,
            status_line_project_root_name_cache: None,
            current_goal_status_indicator: None,
            current_goal_status: None,
            external_editor_state: ExternalEditorState::Closed,
            last_rendered_user_message_display: None,
            last_non_retry_error: None,
        };

        widget.prefetch_rate_limits();
        if let Some(keymap) = runtime_keymap {
            widget.bottom_pane.set_keymap_bindings(&keymap);
        }
        widget
            .bottom_pane
            .set_vim_enabled(widget.config.tui_vim_mode_default);
        widget.bottom_pane.set_status_line_enabled(true);
        widget.sync_service_tier_commands();
        widget.sync_personality_command_enabled();
        widget.sync_goal_command_enabled();
        widget.sync_mentions_v2_enabled();
        widget
            .bottom_pane
            .set_queued_message_edit_binding(widget.queued_message_edit_hint_binding);
        widget.update_goal_status_indicator();

        widget
            .bottom_pane
            .set_token_activity_command_enabled(widget.has_codex_backend_auth);
        widget.refresh_status_surfaces();

        widget
    }
}
