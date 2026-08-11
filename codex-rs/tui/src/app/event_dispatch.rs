//! AppEvent dispatch for the TUI app.
//!
//! This module contains the exhaustive `AppEvent` dispatcher and exit-mode handling. Large domain
//! actions are delegated to focused app submodules so the central match remains the routing layer.

use super::resize_reflow::trailing_run_start;
use super::session_lifecycle::ThreadAttachPresentation;
use super::*;
use crate::config_update::format_config_error;
use crate::pager_overlay::TranscriptHistoryState;
use crate::runtime_session::ForkGoalContinuation;

const SHUTDOWN_FIRST_EXIT_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 2);

impl App {
    pub(super) async fn handle_event(
        &mut self,
        tui: &mut tui::Tui,
        cli_runtime: &mut CliRuntimeSession,
        event: AppEvent,
    ) -> Result<AppRunControl> {
        match event {
            AppEvent::NewSession { name } => {
                self.start_fresh_session_with_summary_hint(
                    tui,
                    cli_runtime,
                    /*session_start_source*/ None,
                    /*initial_user_message*/ None,
                    name,
                )
                .await;
            }
            AppEvent::StartupThreadStarted { result } => {
                self.handle_startup_thread_started(cli_runtime, result)
                    .await?;
            }
            AppEvent::RequestOlderScrollbackHistory { thread_id } => {
                if self.chat_widget.thread_id() == Some(thread_id)
                    && self.overlay.is_none()
                    && self.scrollback_has_older_history
                {
                    self.request_older_history_page(cli_runtime, thread_id);
                }
            }
            AppEvent::OlderThreadHistoryLoaded {
                thread_id,
                cursor,
                result,
            } => {
                if let Err(err) = self
                    .handle_older_history_page(tui, cli_runtime, thread_id, &cursor, result)
                    .await
                {
                    cli_runtime.cancel_older_history_page(thread_id);
                    if self.chat_widget.thread_id() == Some(thread_id)
                        && let Some(Overlay::Transcript(overlay)) = self.overlay.as_mut()
                    {
                        overlay.set_history_state(TranscriptHistoryState::Failed);
                        tui.frame_requester().schedule_frame();
                    }
                    tracing::warn!(%thread_id, error = %err, "failed to load older transcript history");
                }
            }
            AppEvent::ClearUi { name } => {
                self.clear_terminal_ui(tui, /*redraw_header*/ false)?;
                self.reset_app_ui_state_after_clear();

                self.start_fresh_session_with_summary_hint(
                    tui,
                    cli_runtime,
                    Some(ThreadStartSource::Clear),
                    /*initial_user_message*/ None,
                    name,
                )
                .await;
            }
            AppEvent::RawOutputModeChanged { enabled } => {
                self.apply_raw_output_mode(tui, enabled, /*notify*/ false);
            }
            AppEvent::ClearUiAndSubmitUserMessage { text } => {
                self.clear_terminal_ui(tui, /*redraw_header*/ false)?;
                self.reset_app_ui_state_after_clear();

                self.start_fresh_session_with_summary_hint(
                    tui,
                    cli_runtime,
                    Some(ThreadStartSource::Clear),
                    crate::chatwidget::create_initial_user_message(
                        Some(text),
                        Vec::new(),
                        Vec::new(),
                    ),
                    /*new_thread_name*/ None,
                )
                .await;
            }
            AppEvent::OpenResumePicker => {
                let picker_cli_runtime = match crate::start_cli_runtime_for_picker(
                    &self.config,
                    &self.cli_runtime_target,
                    self.state_db.clone(),
                    self.environment_manager.clone(),
                )
                .await
                {
                    Ok(cli_runtime) => cli_runtime,
                    Err(err) => {
                        self.chat_widget.add_error_message(format!(
                            "Failed to start TUI session picker: {err}"
                        ));
                        self.chat_widget.maybe_send_next_queued_input();
                        return Ok(AppRunControl::Continue);
                    }
                };
                match crate::resume_picker::run_resume_picker_from_existing_session_with_cli_runtime(
                    tui,
                    &self.config,
                    /*show_all*/ false,
                    /*include_non_interactive*/ false,
                    picker_cli_runtime,
                )
                .await?
                {
                    SessionSelection::Resume(target_session) => {
                        match self
                            .resume_target_session(tui, cli_runtime, target_session)
                            .await?
                        {
                            AppRunControl::Continue => {}
                            AppRunControl::Exit(reason) => {
                                return Ok(AppRunControl::Exit(reason));
                            }
                        }
                    }
                    SessionSelection::Exit | SessionSelection::StartFresh => {
                        self.refresh_in_memory_config_from_disk_best_effort(
                            "closing the session picker",
                        )
                        .await;
                    }
                    SessionSelection::Fork(_) => {}
                }

                self.chat_widget.maybe_send_next_queued_input();
                // Leaving alt-screen may blank the inline viewport; force a redraw either way.
                tui.frame_requester().schedule_frame();
            }
            AppEvent::ResumeSessionByIdOrName(id_or_name) => {
                match crate::lookup_session_target_with_cli_runtime(
                    cli_runtime,
                    &self.config,
                    &id_or_name,
                )
                .await?
                {
                    Some(target_session) => {
                        return self
                            .resume_target_session(tui, cli_runtime, target_session)
                            .await;
                    }
                    None => {
                        self.chat_widget.add_error_message(format!(
                            "No saved chat found matching '{id_or_name}'."
                        ));
                    }
                }
            }
            AppEvent::ArchiveCurrentThread => {
                return Ok(self.archive_current_thread(cli_runtime).await);
            }
            AppEvent::DeleteCurrentThread => {
                return Ok(self.delete_current_thread(cli_runtime).await);
            }
            AppEvent::ForkCurrentSession { name } => {
                let summary = session_summary(
                    self.chat_widget.token_usage(),
                    self.chat_widget.thread_id(),
                    self.chat_widget.thread_name(),
                    self.chat_widget.rollout_path().as_deref(),
                );
                self.chat_widget
                    .add_plain_history_lines(vec!["/fork".magenta().into()]);
                if let Some(thread_id) = self.chat_widget.thread_id() {
                    self.refresh_in_memory_config_from_disk_best_effort("forking the thread")
                        .await;
                    let mut fork_config = self.config.clone();
                    fork_config.model = Some(self.chat_widget.current_model().to_string());
                    fork_config.model_reasoning_effort =
                        self.chat_widget.current_reasoning_effort();
                    match cli_runtime.fork_thread(fork_config, thread_id).await {
                        Ok(mut forked) => {
                            let name_error = if let Some(name) = name {
                                match cli_runtime
                                    .thread_set_name(forked.session.thread_id, name.clone())
                                    .await
                                {
                                    Ok(()) => {
                                        forked.session.thread_name = Some(name);
                                        None
                                    }
                                    Err(err) => {
                                        Some(format!("Failed to name the forked session: {err}"))
                                    }
                                }
                            } else {
                                None
                            };
                            self.shutdown_current_thread(cli_runtime).await;
                            match self
                                .replace_chat_widget_with_cli_runtime_thread(
                                    tui,
                                    forked,
                                    ThreadAttachPresentation::SessionLineage,
                                    /*initial_user_message*/ None,
                                )
                                .await
                            {
                                Ok(()) => {
                                    if let Some(err) = name_error {
                                        self.chat_widget.add_error_message(err);
                                    }
                                    if let Some(summary) = summary {
                                        let mut lines: Vec<Line<'static>> = Vec::new();
                                        if let Some(usage_line) = summary.usage_line {
                                            lines.push(usage_line.into());
                                        }
                                        if let Some(command) = summary.resume_hint {
                                            let spans = vec![
                                                "To continue this session, run ".into(),
                                                command.cyan(),
                                            ];
                                            lines.push(spans.into());
                                        }
                                        self.chat_widget.add_plain_history_lines(lines);
                                    }
                                }
                                Err(err) => {
                                    self.chat_widget.add_error_message(format!(
                                        "Failed to attach to forked cli-runtime thread: {err}"
                                    ));
                                }
                            }
                        }
                        Err(err) => {
                            self.chat_widget.add_error_message(format!(
                                "Failed to fork current session through the app server: {err}"
                            ));
                        }
                    }
                } else {
                    self.chat_widget.add_error_message(
                        "A thread must contain at least one turn before it can be forked."
                            .to_string(),
                    );
                }

                self.chat_widget.maybe_send_next_queued_input();
                tui.frame_requester().schedule_frame();
            }
            AppEvent::ForkSessionForPromptEdit {
                thread_id,
                nth_user_message,
                mut prompt,
            } => {
                if self.chat_widget.thread_id() != Some(thread_id) {
                    return Ok(AppRunControl::Continue);
                }
                self.refresh_in_memory_config_from_disk_best_effort("forking the thread")
                    .await;
                let config = self.fresh_session_config();
                let turns = match self.thread_event_channels.get(&thread_id) {
                    Some(channel) => Some(channel.store.lock().await.turns.clone()),
                    None => None,
                };
                let started = match turns {
                    Some(turns) => match crate::app_backtrack::backtrack_fork_before_turn_id(
                        &turns,
                        nth_user_message,
                        &mut prompt,
                    ) {
                        Ok(before_turn_id)
                            if before_turn_id.is_some()
                                || cli_runtime.has_older_history(thread_id) =>
                        {
                            let before_turn_id = before_turn_id
                                .or_else(|| turns.first().map(|turn| turn.id.clone()));
                            cli_runtime
                                .fork_thread_at(
                                    config.clone(),
                                    thread_id,
                                    /*last_turn_id*/ None,
                                    before_turn_id,
                                    ForkGoalContinuation::StartIfIdle,
                                )
                                .await
                        }
                        Ok(_) => {
                            cli_runtime
                                .start_thread_with_session_start_source(
                                    &config, /*session_start_source*/ None,
                                )
                                .await
                        }
                        Err(err) => Err(err),
                    },
                    None => Err(color_eyre::eyre::eyre!(
                        "the selected thread is no longer available for prompt editing"
                    )),
                };
                match started {
                    Ok(forked) => {
                        self.shutdown_current_thread(cli_runtime).await;
                        match self
                            .replace_chat_widget_with_cli_runtime_thread(
                                tui,
                                forked,
                                ThreadAttachPresentation::PromptEdit,
                                /*initial_user_message*/ None,
                            )
                            .await
                        {
                            Ok(()) => self.chat_widget.restore_user_message_to_composer(prompt),
                            Err(err) => {
                                self.restore_backtrack_prompt_after_branch_error(prompt, err);
                            }
                        }
                    }
                    Err(err) => {
                        self.restore_backtrack_prompt_after_branch_error(prompt, err);
                    }
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::BeginInitialHistoryReplayBuffer => {
                self.begin_initial_history_replay_buffer();
            }
            AppEvent::BeginThreadSwitchHistoryReplayBuffer => {
                self.begin_thread_switch_history_replay_buffer();
            }
            AppEvent::InsertHistoryCell(cell) => {
                self.insert_history_cell(tui, cell);
            }
            AppEvent::EndInitialHistoryReplayBuffer => {
                self.scrollback_has_older_history = self
                    .chat_widget
                    .thread_id()
                    .is_some_and(|thread_id| cli_runtime.has_older_history(thread_id));
                self.finish_initial_history_replay_buffer(tui);
            }
            AppEvent::ConsolidateAgentMessage {
                source,
                cwd,
                inline_visualization_context,
                scrollback_reflow,
                deferred_history_cell,
            } => {
                self.handle_consolidate_agent_message(
                    tui,
                    source,
                    cwd,
                    inline_visualization_context,
                    scrollback_reflow,
                    deferred_history_cell,
                )?;
                self.chat_widget.note_stream_consolidation_completed();
                self.insert_pending_usage_output_after_stream_shutdown(tui);
            }
            AppEvent::StartCommitAnimation => {
                if self
                    .commit_anim_running
                    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    let tx = self.app_event_tx.clone();
                    let running = self.commit_anim_running.clone();
                    thread::spawn(move || {
                        while running.load(Ordering::Relaxed) {
                            thread::sleep(COMMIT_ANIMATION_TICK);
                            tx.send(AppEvent::CommitTick);
                        }
                    });
                }
            }
            AppEvent::StopCommitAnimation => {
                self.commit_anim_running.store(false, Ordering::Release);
            }
            AppEvent::CommitTick => {
                self.chat_widget.on_commit_tick();
            }
            AppEvent::Exit(mode) => {
                if mode == ExitMode::ShutdownFirst {
                    self.show_shutdown_state(tui)?;
                }
                return Ok(self.handle_exit_mode(cli_runtime, mode).await);
            }
            AppEvent::Logout => match cli_runtime.logout_account().await {
                Ok(()) => {
                    self.show_shutdown_state(tui)?;
                    return Ok(self
                        .handle_exit_mode(cli_runtime, ExitMode::ShutdownFirst)
                        .await);
                }
                Err(err) => {
                    tracing::error!("failed to logout: {err}");
                    self.chat_widget
                        .add_error_message(format!("Logout failed: {err}"));
                }
            },
            AppEvent::FatalExitRequest(message) => {
                return Ok(AppRunControl::Exit(ExitReason::Fatal(message)));
            }
            AppEvent::CodexOp(op) => {
                let is_user_turn = matches!(&op, AppCommand::UserTurn { .. });
                if is_user_turn {
                    let screen_size = tui.terminal.last_known_screen_size;
                    self.handle_draw_pre_render(tui, screen_size)?;
                    if self.transcript_reflow.has_pending_reflow() {
                        self.transcript_reflow.schedule_immediate();
                        self.maybe_run_resize_reflow(tui, screen_size)?;
                    }
                    self.chat_widget.pre_draw_tick();
                    self.render_chat_widget_frame(tui, screen_size)?;
                }
                self.chat_widget.prepare_local_op_submission(&op);
                if let Err(err) = self.submit_active_thread_op(cli_runtime, op).await {
                    let handled = is_user_turn
                        && matches!(
                            err.downcast_ref::<TypedRequestError>(),
                            Some(TypedRequestError::Server { method, .. })
                                if method == "turn/start"
                        )
                        && self
                            .chat_widget
                            .handle_turn_start_rejection(format!("Failed to start turn: {err:#}"));
                    if !handled {
                        return Err(err);
                    }
                    tracing::error!(error = ?err, "failed to start turn through app server");
                }
            }
            AppEvent::RetrySafetyBufferedTurn {
                thread_id,
                turn_id,
                model,
                turn,
                prompt,
            } => {
                self.retry_safety_buffered_turn(
                    tui,
                    cli_runtime,
                    super::safety_buffering::SafetyBufferedRetry {
                        thread_id,
                        turn_id,
                        model,
                        turn,
                        prompt,
                    },
                )
                .await;
            }
            AppEvent::AppendMessageHistoryEntry { thread_id, text } => {
                self.append_message_history_entry(thread_id, text);
            }
            AppEvent::SyncThreadGitBranch { thread_id, branch } => {
                if let Err(err) = cli_runtime
                    .thread_metadata_update_branch(thread_id, branch)
                    .await
                {
                    tracing::warn!("failed to sync thread git branch from directive: {err}");
                }
            }
            AppEvent::LookupMessageHistoryEntry {
                thread_id,
                offset,
                log_id,
            } => {
                self.lookup_message_history_entry(thread_id, offset, log_id)
                    .await?;
            }
            AppEvent::LookupMessageHistoryBatch {
                thread_id,
                cursor,
                log_id,
            } => {
                self.lookup_message_history_batch(thread_id, cursor, log_id)
                    .await?;
            }
            AppEvent::SubmitThreadOp { thread_id, op } => {
                self.submit_thread_op(cli_runtime, thread_id, op).await?;
            }
            AppEvent::ThreadHistoryEntryResponse { thread_id, event } => {
                self.enqueue_thread_history_entry_response(thread_id, event)
                    .await?;
            }
            AppEvent::DiffResult(text) => {
                // Clear the in-progress state in the bottom pane
                self.chat_widget.on_diff_complete();
                // Enter alternate screen using TUI helper and build pager lines
                let _ = tui.enter_alt_screen();
                let pager_lines: Vec<ratatui::text::Line<'static>> = if text.trim().is_empty() {
                    vec!["No changes detected.".italic().into()]
                } else {
                    text.lines().map(ansi_escape_line).collect()
                };
                self.overlay = Some(Overlay::new_static_with_lines(
                    pager_lines,
                    "D I F F".to_string(),
                    self.keymap.pager.clone(),
                ));
                tui.frame_requester().schedule_frame();
            }
            AppEvent::OpenUrlInBrowser { url } => {
                self.open_url_in_browser(url);
            }
            AppEvent::OpenDesktopThread { thread_id } => {
                self.open_desktop_thread(thread_id);
            }
            AppEvent::PetSelected { pet_id } => {
                self.handle_pet_selected(tui, pet_id);
            }
            AppEvent::PetDisabled => {
                self.handle_pet_disabled(tui).await;
            }
            AppEvent::PetPreviewRequested { pet_id } => {
                self.chat_widget.start_pet_picker_preview(pet_id);
            }
            AppEvent::PetPreviewLoaded { request_id, result } => {
                self.handle_pet_preview_loaded(tui, request_id, result);
            }
            AppEvent::PetSelectionLoaded {
                request_id,
                pet_id,
                result,
            } => {
                return self
                    .handle_pet_selection_loaded(tui, request_id, pet_id, result)
                    .await;
            }
            AppEvent::ConfiguredPetLoaded { pet_id, result } => {
                self.handle_configured_pet_loaded(tui, pet_id, result);
            }
            AppEvent::SkillsListLoaded { result } => {
                self.handle_skills_list_result(
                    result.map_err(|err| color_eyre::eyre::eyre!(err)),
                    "failed to load skills on startup",
                );
            }
            AppEvent::StartFileSearch(query) => {
                self.file_search.on_user_query(query);
            }
            AppEvent::FileSearchResult { query, matches } => {
                self.chat_widget.apply_file_search_result(query, matches);
            }
            AppEvent::RefreshRateLimits { origin } => {
                self.refresh_rate_limits(cli_runtime, origin);
            }
            AppEvent::RefreshTokenActivity { request_id } => {
                self.refresh_token_activity(cli_runtime, request_id);
            }
            AppEvent::RefreshStatusLineWorkspaceHeadline { request_id } => {
                self.refresh_status_line_workspace_headline(cli_runtime, request_id);
            }
            AppEvent::OpenThreadGoalMenu { thread_id } => {
                self.open_thread_goal_menu(cli_runtime, thread_id).await;
            }
            AppEvent::OpenThreadGoalEditor { thread_id } => {
                self.open_thread_goal_editor(cli_runtime, thread_id).await;
            }
            AppEvent::SetThreadGoalDraft {
                thread_id,
                draft,
                mode,
            } => {
                self.set_thread_goal_draft(cli_runtime, thread_id, draft, mode)
                    .await;
            }
            AppEvent::SetThreadGoalStatus { thread_id, status } => {
                self.set_thread_goal_status(cli_runtime, thread_id, status)
                    .await;
            }
            AppEvent::ClearThreadGoal { thread_id } => {
                self.clear_thread_goal(cli_runtime, thread_id).await;
            }
            AppEvent::SendAddCreditsNudgeEmail { credit_type } => {
                if self
                    .chat_widget
                    .start_add_credits_nudge_email_request(credit_type)
                {
                    self.send_add_credits_nudge_email(cli_runtime, credit_type);
                }
            }
            AppEvent::AddCreditsNudgeEmailFinished { result } => {
                self.chat_widget
                    .finish_add_credits_nudge_email_request(result);
            }
            AppEvent::RateLimitsLoaded {
                origin,
                hard_stop_generation,
                result,
            } => match result {
                Ok(response) => {
                    let rate_limit_reset_credits = response.rate_limit_reset_credits.clone();
                    let snapshots = if hard_stop_generation == self.rate_limit_hard_stop_generation
                    {
                        cli_runtime_rate_limit_snapshots(response)
                    } else {
                        Vec::new()
                    };
                    match origin {
                        RateLimitRefreshOrigin::StartupPrefetch {
                            reset_hint_request_id,
                        } => {
                            if self.chat_widget.finish_rate_limit_reset_hint_refresh(
                                reset_hint_request_id,
                                snapshots,
                                rate_limit_reset_credits.ok_or_else(|| {
                                    "account/rateLimits/read response did not include rateLimitResetCredits"
                                        .to_string()
                                }),
                            ) {
                                self.insert_pending_usage_output_if_ready(tui);
                            }
                            tui.frame_requester().schedule_frame();
                        }
                        RateLimitRefreshOrigin::ResetConsume { request_id } => {
                            self.chat_widget.finish_post_consume_reset_credits_refresh(
                                request_id,
                                snapshots,
                                rate_limit_reset_credits.ok_or_else(|| {
                                    "account/rateLimits/read response did not include rateLimitResetCredits"
                                        .to_string()
                                }),
                            );
                            tui.frame_requester().schedule_frame();
                        }
                        RateLimitRefreshOrigin::StatusCommand { request_id } => {
                            self.chat_widget
                                .finish_status_rate_limit_refresh(request_id, snapshots);
                        }
                        RateLimitRefreshOrigin::UsageMenu { request_id } => {
                            self.chat_widget.finish_usage_menu_rate_limit_refresh(
                                request_id,
                                snapshots,
                                rate_limit_reset_credits.ok_or_else(|| {
                                    "account/rateLimits/read response did not include rateLimitResetCredits"
                                    .to_string()
                                }),
                            );
                        }
                        RateLimitRefreshOrigin::ResetPicker { request_id } => {
                            self.chat_widget.finish_rate_limit_reset_credits_refresh(
                                request_id,
                                snapshots,
                                rate_limit_reset_credits.ok_or_else(|| {
                                    "account/rateLimits/read response did not include rateLimitResetCredits"
                                        .to_string()
                                }),
                            );
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!("account/rateLimits/read failed during TUI refresh: {err}");
                    match origin {
                        RateLimitRefreshOrigin::StartupPrefetch {
                            reset_hint_request_id,
                        } => {
                            self.chat_widget.finish_rate_limit_reset_hint_refresh(
                                reset_hint_request_id,
                                Vec::new(),
                                Err(err),
                            );
                        }
                        RateLimitRefreshOrigin::ResetConsume { request_id } => {
                            self.chat_widget.finish_post_consume_reset_credits_refresh(
                                request_id,
                                Vec::new(),
                                Err(err),
                            );
                        }
                        RateLimitRefreshOrigin::StatusCommand { request_id } => {
                            self.chat_widget
                                .finish_status_rate_limit_refresh(request_id, Vec::new());
                        }
                        RateLimitRefreshOrigin::UsageMenu { request_id } => {
                            self.chat_widget.finish_usage_menu_rate_limit_refresh(
                                request_id,
                                Vec::new(),
                                Err(err),
                            );
                        }
                        RateLimitRefreshOrigin::ResetPicker { request_id } => {
                            self.chat_widget.finish_rate_limit_reset_credits_refresh(
                                request_id,
                                Vec::new(),
                                Err(err),
                            );
                        }
                    }
                }
            },
            AppEvent::OpenTokenActivity => {
                self.chat_widget
                    .add_token_activity_output(crate::chatwidget::TokenActivityView::Daily);
            }
            AppEvent::OpenRateLimitResetCredits => {
                let request_id = self.chat_widget.show_rate_limit_reset_loading_popup();
                self.refresh_rate_limits(
                    cli_runtime,
                    RateLimitRefreshOrigin::ResetPicker { request_id },
                );
            }
            AppEvent::OpenRateLimitResetConfirmation {
                picker_request_id,
                confirmation_gate,
                credit_id,
                reset_title,
                reset_detail,
                reset_description,
            } => {
                self.chat_widget.show_rate_limit_reset_confirmation(
                    picker_request_id,
                    confirmation_gate,
                    credit_id,
                    reset_title,
                    reset_detail,
                    reset_description,
                );
            }
            AppEvent::ConsumeRateLimitResetCredit {
                idempotency_key,
                credit_id,
            } => {
                if let Some(request_id) = self
                    .chat_widget
                    .start_rate_limit_reset_consumption(&idempotency_key)
                {
                    self.consume_rate_limit_reset_credit(
                        cli_runtime,
                        request_id,
                        idempotency_key,
                        credit_id,
                    );
                }
            }
            AppEvent::RateLimitResetCreditConsumed {
                request_id,
                idempotency_key,
                credit_id,
                result,
            } => {
                if let Err(err) = &result {
                    tracing::warn!(
                        "account/rateLimitResetCredit/consume failed during TUI request: {err}"
                    );
                }
                if self.chat_widget.finish_rate_limit_reset_consume(
                    request_id,
                    idempotency_key,
                    credit_id,
                    result,
                ) {
                    self.refresh_rate_limits(
                        cli_runtime,
                        RateLimitRefreshOrigin::ResetConsume { request_id },
                    );
                }
            }
            AppEvent::TokenActivityLoaded { request_id, result } => {
                if let Err(err) = &result {
                    tracing::warn!("account/usage/read failed during TUI refresh: {err}");
                }
                if self
                    .chat_widget
                    .finish_token_activity_refresh(request_id, result)
                {
                    // Commit synchronously so an already queued /clear cannot overtake this card.
                    // Do not route through ChatWidget::add_to_history: /usage may complete during
                    // active work, and flushing an in-progress tool cell would corrupt its lifecycle.
                    // If an answer stream is active, keep the settled card transient until its
                    // provisional transcript cells have been consolidated.
                    self.insert_pending_usage_output_if_ready(tui);
                }
            }
            AppEvent::CommitPendingUsageOutput => {
                self.insert_pending_usage_output_if_ready(tui);
            }
            AppEvent::CommitPendingUsageOutputAfterStreamShutdown => {
                self.insert_pending_usage_output_after_stream_shutdown(tui);
            }
            AppEvent::UpdateReasoningEffort(effort) => {
                self.on_update_reasoning_effort(effort.clone());
                self.sync_active_thread_reasoning_setting(cli_runtime, effort)
                    .await;
            }
            AppEvent::UpdateModel(model) => {
                let model_changed = self.chat_widget.current_model() != model
                    || self.chat_widget.current_agent_settings().model() != model;
                if model_changed {
                    self.chat_widget.set_model(&model);
                    self.sync_active_thread_model_setting(cli_runtime, model, /*effort*/ None)
                        .await;
                    self.sync_active_thread_service_tier_to_cached_session()
                        .await;
                }
            }
            AppEvent::UpdatePersonality(personality) => {
                self.on_update_personality(personality);
                self.sync_active_thread_personality_setting(cli_runtime, personality)
                    .await;
            }
            AppEvent::SettingsSelectionClosed => {
                self.app_event_tx.send(AppEvent::SettingsSelectionSettled);
            }
            AppEvent::SettingsSelectionSettled => {
                if self.chat_widget.no_modal_or_popup_active() {
                    self.chat_widget
                        .set_queue_autosend_suppressed(/*suppressed*/ false);
                    self.chat_widget.maybe_send_next_queued_input();
                }
            }
            AppEvent::OpenReasoningPopup { model } => {
                self.chat_widget.open_reasoning_popup(model);
            }
            AppEvent::OpenAdvancedReasoningPopup { model } => {
                self.chat_widget.open_advanced_reasoning_popup(model);
            }
            AppEvent::ApplyAdvancedReasoning { model, effort } => {
                let model_changed = self.chat_widget.current_model() != model
                    || self.chat_widget.current_agent_settings().model() != model;
                let default_effort =
                    self.on_apply_advanced_reasoning(model.as_str(), effort.clone());
                if model_changed {
                    self.sync_active_thread_model_setting(
                        cli_runtime,
                        model.clone(),
                        Some(effort.clone()),
                    )
                    .await;
                } else if let Some(mut params) =
                    self.active_thread_reasoning_setting_update_params(Some(effort.clone()))
                {
                    params.agent_settings = Some(self.chat_widget.effective_agent_settings());
                    self.send_thread_settings_update(cli_runtime, params).await;
                }
                self.sync_active_thread_service_tier_to_cached_session()
                    .await;

                if let Some(default_effort) = default_effort.as_ref()
                    && let Err(err) = crate::config_update::write_config_batch(
                        cli_runtime.request_handle(),
                        crate::config_update::build_model_selection_edits(
                            model.as_str(),
                            Some(default_effort),
                        ),
                    )
                    .await
                {
                    let error = format_config_error(&err);
                    tracing::error!(error = %error, "failed to persist conversation model");
                    self.chat_widget
                        .add_error_message(format!("Failed to save default model: {error}"));
                } else {
                    self.chat_widget.add_info_message(
                        format!("Model changed to {model} {effort} for this conversation"),
                        /*hint*/ None,
                    );
                }
            }
            AppEvent::OpenAllModelsPopup { models } => {
                self.chat_widget.open_all_models_popup(models);
            }
            AppEvent::LaunchExternalEditor => {
                if self.chat_widget.external_editor_state() == ExternalEditorState::Active {
                    self.launch_external_editor(tui).await;
                }
            }
            AppEvent::PersistModelSelection { model, effort } => {
                match crate::config_update::write_config_batch(
                    cli_runtime.request_handle(),
                    crate::config_update::build_model_selection_edits(
                        model.as_str(),
                        effort.as_ref(),
                    ),
                )
                .await
                {
                    Ok(_) => {
                        let effort_label = effort
                            .as_ref()
                            .map(std::string::ToString::to_string)
                            .unwrap_or_else(|| "default".to_string());
                        tracing::info!("Selected model: {model}, Selected effort: {effort_label}");
                        let mut message = format!("Model changed to {model}");
                        if let Some(label) = Self::reasoning_label_for(&model, effort.as_ref()) {
                            message.push(' ');
                            message.push_str(&label);
                        }
                        self.chat_widget.add_info_message(message, /*hint*/ None);
                    }
                    Err(err) => {
                        let error = format_config_error(&err);
                        tracing::error!(
                            error = %error,
                            "failed to persist model selection"
                        );
                        self.chat_widget
                            .add_error_message(format!("Failed to save default model: {error}"));
                    }
                }
            }
            AppEvent::PersistPersonalitySelection { personality } => {
                match crate::config_update::write_config_batch(
                    cli_runtime.request_handle(),
                    vec![crate::config_update::replace_config_value(
                        "personality",
                        serde_json::json!(personality.to_string()),
                    )],
                )
                .await
                {
                    Ok(_) => {
                        let label = Self::personality_label(personality);
                        let message = format!("Personality set to {label}");
                        self.chat_widget.add_info_message(message, /*hint*/ None);
                    }
                    Err(err) => {
                        tracing::error!(
                            error = %err,
                            "failed to persist personality selection"
                        );
                        self.chat_widget.add_error_message(format!(
                            "Failed to save default personality: {err}"
                        ));
                    }
                }
            }
            AppEvent::PersistServiceTierSelection { service_tier } => {
                self.refresh_status_line();
                self.config.service_tier = service_tier.clone();
                self.sync_active_thread_service_tier_to_cached_session()
                    .await;
                let edits = crate::config_update::build_service_tier_selection_edits(
                    service_tier.as_deref(),
                );
                match crate::config_update::write_config_batch(cli_runtime.request_handle(), edits)
                    .await
                {
                    Ok(_) => {
                        let message = if let Some(service_tier) = service_tier {
                            format!("Service tier set to {service_tier}")
                        } else {
                            "Service tier cleared".to_string()
                        };
                        self.chat_widget.add_info_message(message, /*hint*/ None);
                    }
                    Err(err) => {
                        tracing::error!(error = %err, "failed to persist service tier selection");
                        self.chat_widget.add_error_message(format!(
                            "Failed to save default service tier: {err}"
                        ));
                    }
                }
            }
            AppEvent::UpdateFeatureFlags { updates } => {
                self.update_feature_flags(cli_runtime, updates).await;
            }
            AppEvent::UpdateMemorySettings {
                use_memories,
                generate_memories,
            } => {
                self.update_memory_settings_with_cli_runtime(
                    cli_runtime,
                    use_memories,
                    generate_memories,
                )
                .await;
            }
            AppEvent::ResetMemories => {
                self.reset_memories_with_cli_runtime(cli_runtime).await;
            }
            AppEvent::UpdateRateLimitSwitchPromptHidden(hidden) => {
                self.chat_widget.set_rate_limit_switch_prompt_hidden(hidden);
            }
            AppEvent::PersistRateLimitSwitchPromptHidden => {
                if let Err(err) = ConfigEditsBuilder::for_config(&self.config)
                    .set_hide_rate_limit_model_nudge(/*acknowledged*/ true)
                    .apply()
                    .await
                {
                    tracing::error!(
                        error = %err,
                        "failed to persist rate limit switch prompt preference"
                    );
                    self.chat_widget.add_error_message(format!(
                        "Failed to save rate limit reminder preference: {err}"
                    ));
                }
            }
            AppEvent::PersistModelMigrationPromptAcknowledged {
                from_model,
                to_model,
            } => {
                if let Err(err) = ConfigEditsBuilder::for_config(&self.config)
                    .record_model_migration_seen(from_model.as_str(), to_model.as_str())
                    .apply()
                    .await
                {
                    tracing::error!(
                        error = %err,
                        "failed to persist model migration prompt acknowledgement"
                    );
                    self.chat_widget.add_error_message(format!(
                        "Failed to save model migration prompt preference: {err}"
                    ));
                }
            }
            AppEvent::OpenAgentPicker => {
                self.open_agent_picker(cli_runtime).await;
            }
            AppEvent::AgentPickerThreadsLoaded {
                primary_thread_id,
                request_id,
                result,
            } => {
                self.apply_agent_picker_thread_refresh(primary_thread_id, request_id, result);
            }
            AppEvent::SelectAgentThread(thread_id) => {
                self.select_agent_thread_and_discard_side(tui, cli_runtime, thread_id)
                    .await?;
            }
            AppEvent::StartSide {
                parent_thread_id,
                user_message,
            } => {
                return self
                    .handle_start_side(tui, cli_runtime, parent_thread_id, user_message)
                    .await;
            }
            AppEvent::OpenSkillsList => {
                self.chat_widget.open_skills_list();
            }
            AppEvent::OpenManageSkillsPopup => {
                self.chat_widget.open_manage_skills_popup();
            }
            AppEvent::SetSkillEnabled { path, enabled } => {
                match crate::config_update::write_skill_enabled(
                    cli_runtime.request_handle(),
                    path.clone(),
                    enabled,
                )
                .await
                {
                    Ok(()) => {
                        self.chat_widget.update_skill_enabled(path, enabled);
                    }
                    Err(err) => {
                        let path_display = path.display();
                        self.chat_widget.add_error_message(format!(
                            "Failed to update skill config for {path_display}: {err}"
                        ));
                    }
                }
            }
            AppEvent::ManageSkillsClosed => {
                self.chat_widget.handle_manage_skills_closed();
            }
            AppEvent::StatusLineSetup {
                items,
                use_theme_colors,
            } => {
                let ids = items.iter().map(ToString::to_string).collect::<Vec<_>>();
                let items_edit = crate::legacy_core::config::edit::status_line_items_edit(&ids);
                let colors_edit =
                    crate::legacy_core::config::edit::status_line_use_colors_edit(use_theme_colors);
                let apply_result = ConfigEditsBuilder::for_config(&self.config)
                    .with_edits([items_edit, colors_edit])
                    .apply()
                    .await;
                match apply_result {
                    Ok(()) => {
                        self.config.tui_status_line = Some(ids.clone());
                        self.config.tui_status_line_use_colors = use_theme_colors;
                        self.chat_widget.setup_status_line(items, use_theme_colors);
                    }
                    Err(err) => {
                        let error = format_config_error(&err);
                        tracing::error!(error = %error, "failed to persist status line settings; keeping previous selection");
                        self.chat_widget.add_error_message(format!(
                            "Failed to save status line settings: {error}"
                        ));
                    }
                }
            }
            AppEvent::StatusLineBranchUpdated { cwd, branch } => {
                self.chat_widget.set_status_line_branch(cwd, branch);
                self.refresh_status_line();
            }
            AppEvent::StatusLineGitSummaryUpdated { cwd, summary } => {
                self.chat_widget.set_status_line_git_summary(cwd, summary);
                self.refresh_status_line();
            }
            AppEvent::StatusLineWorkspaceHeadlineUpdated { request_id, result } => {
                if self
                    .chat_widget
                    .set_status_line_workspace_headline(request_id, result)
                {
                    tui.frame_requester().schedule_frame();
                }
            }
            AppEvent::StatusLineSetupCancelled => {
                self.chat_widget.cancel_status_line_setup();
            }
            AppEvent::TerminalTitleSetup { items } => {
                let ids = items.iter().map(ToString::to_string).collect::<Vec<_>>();
                let edit = crate::legacy_core::config::edit::terminal_title_items_edit(&ids);
                let apply_result = ConfigEditsBuilder::for_config(&self.config)
                    .with_edits([edit])
                    .apply()
                    .await;
                match apply_result {
                    Ok(()) => {
                        self.config.tui_terminal_title = Some(ids.clone());
                        self.chat_widget.setup_terminal_title(items);
                    }
                    Err(err) => {
                        tracing::error!(error = %err, "failed to persist terminal title items; keeping previous selection");
                        self.chat_widget.revert_terminal_title_setup_preview();
                        self.chat_widget.add_error_message(format!(
                            "Failed to save terminal title items: {err}"
                        ));
                    }
                }
            }
            AppEvent::TerminalTitleSetupPreview { items } => {
                self.chat_widget.preview_terminal_title(items);
            }
            AppEvent::TerminalTitleSetupCancelled => {
                self.chat_widget.cancel_terminal_title_setup();
            }
            AppEvent::SyntaxThemeSelected { name } => {
                let edit = crate::legacy_core::config::edit::syntax_theme_edit(&name);
                let apply_result = ConfigEditsBuilder::for_config(&self.config)
                    .with_edits([edit])
                    .apply()
                    .await;
                match apply_result {
                    Ok(()) => {
                        // Ensure the selected theme is active in the current
                        // session.  The preview callback covers arrow-key
                        // navigation, but if the user presses Enter without
                        // navigating, the runtime theme must still be applied.
                        if let Some(theme) = crate::render::highlight::resolve_theme_by_name(
                            &name,
                            Some(&self.config.codex_home),
                        ) {
                            crate::render::highlight::set_syntax_theme(theme);
                        }
                        self.sync_tui_theme_selection(name);
                        self.refresh_status_line();
                        tui.frame_requester().schedule_frame();
                    }
                    Err(err) => {
                        self.restore_runtime_theme_from_config();
                        self.refresh_status_line();
                        tracing::error!(error = %err, "failed to persist theme selection");
                        self.chat_widget
                            .add_error_message(format!("Failed to save theme: {err}"));
                    }
                }
            }
            AppEvent::SyntaxThemePreviewed => {
                self.refresh_status_line();
                tui.frame_requester().schedule_frame();
            }
            AppEvent::OpenKeymapActionMenu { context, action } => {
                self.chat_widget
                    .open_keymap_action_menu(context, action, &self.keymap);
            }
            AppEvent::OpenKeymapReplaceBindingMenu { context, action } => {
                self.chat_widget
                    .open_keymap_replace_binding_menu(context, action, &self.keymap);
            }
            AppEvent::OpenKeymapCapture {
                context,
                action,
                intent,
                capture_mode,
            } => {
                self.chat_widget.open_keymap_capture(
                    context,
                    action,
                    intent,
                    capture_mode,
                    &self.keymap,
                );
            }
            AppEvent::OpenKeymapDebug => {
                self.chat_widget.open_keymap_debug(&self.keymap);
            }
            AppEvent::KeymapCaptured {
                context,
                action,
                key,
                intent,
            } => {
                self.apply_keymap_capture(context, action, key, intent)
                    .await;
            }
            AppEvent::KeymapCleared { context, action } => {
                self.apply_keymap_clear(context, action).await;
            }
        }
        Ok(AppRunControl::Continue)
    }

    async fn apply_keymap_capture(
        &mut self,
        context: String,
        action: String,
        key: String,
        intent: crate::app_event::KeymapEditIntent,
    ) {
        let outcome = match crate::keymap_setup::keymap_with_edit(
            &self.config.tui_keymap,
            &self.keymap,
            &context,
            &action,
            &key,
            &intent,
        ) {
            Ok(outcome) => outcome,
            Err(err) => {
                self.chat_widget.add_error_message(err);
                return;
            }
        };
        let (keymap_config, bindings, message) = match outcome {
            crate::keymap_setup::KeymapEditOutcome::Updated {
                keymap_config,
                bindings,
                message,
            } => (*keymap_config, bindings, message),
            crate::keymap_setup::KeymapEditOutcome::Unchanged { message } => {
                self.chat_widget.add_info_message(message, /*hint*/ None);
                return;
            }
        };

        let runtime_keymap = match RuntimeKeymap::from_config(&keymap_config) {
            Ok(runtime_keymap) => runtime_keymap,
            Err(err) => {
                let params = crate::keymap_setup::build_keymap_conflict_params(
                    context, action, key, intent, err,
                );
                self.chat_widget.show_selection_view(params);
                return;
            }
        };

        let edit =
            crate::legacy_core::config::edit::keymap_bindings_edit(&context, &action, &bindings);
        match ConfigEditsBuilder::for_config(&self.config)
            .with_edits([edit])
            .apply()
            .await
        {
            Ok(()) => {
                self.cancel_pending_key_chord();
                self.config.tui_keymap = keymap_config.clone();
                self.keymap = runtime_keymap.clone();
                self.chat_widget
                    .apply_keymap_update(keymap_config, &runtime_keymap);
                self.sync_side_thread_ui();
                self.chat_widget
                    .return_to_keymap_picker(&context, &action, &runtime_keymap);
                self.chat_widget.add_info_message(message, /*hint*/ None);
            }
            Err(err) => {
                tracing::error!(error = %err, "failed to persist keymap binding");
                self.chat_widget
                    .add_error_message(format!("Failed to save shortcut: {err}"));
            }
        }
    }

    async fn apply_keymap_clear(&mut self, context: String, action: String) {
        let keymap_config = match crate::keymap_setup::keymap_without_custom_binding(
            &self.config.tui_keymap,
            &context,
            &action,
        ) {
            Ok(keymap_config) => keymap_config,
            Err(err) => {
                self.chat_widget.add_error_message(err);
                return;
            }
        };

        let runtime_keymap = match RuntimeKeymap::from_config(&keymap_config) {
            Ok(runtime_keymap) => runtime_keymap,
            Err(err) => {
                self.chat_widget
                    .add_error_message(format!("Failed to refresh shortcuts: {err}"));
                return;
            }
        };

        let edit = crate::legacy_core::config::edit::keymap_binding_clear_edit(&context, &action);
        match ConfigEditsBuilder::for_config(&self.config)
            .with_edits([edit])
            .apply()
            .await
        {
            Ok(()) => {
                self.cancel_pending_key_chord();
                self.config.tui_keymap = keymap_config.clone();
                self.keymap = runtime_keymap.clone();
                self.chat_widget
                    .apply_keymap_update(keymap_config, &runtime_keymap);
                self.sync_side_thread_ui();
                self.chat_widget
                    .return_to_keymap_picker(&context, &action, &runtime_keymap);
                self.chat_widget.add_info_message(
                    format!("Removed custom shortcut for `{context}.{action}`."),
                    /*hint*/ None,
                );
            }
            Err(err) => {
                tracing::error!(error = %err, "failed to clear keymap binding");
                self.chat_widget
                    .add_error_message(format!("Failed to remove shortcut: {err}"));
            }
        }
    }

    pub(super) async fn handle_exit_mode(
        &mut self,
        cli_runtime: &mut CliRuntimeSession,
        mode: ExitMode,
    ) -> AppRunControl {
        match mode {
            ExitMode::ShutdownFirst => {
                // Mark the thread we are explicitly shutting down for exit so
                // its shutdown completion does not trigger agent failover.
                self.pending_shutdown_exit_thread_id =
                    self.active_thread_id.or(self.chat_widget.thread_id());
                if self.pending_shutdown_exit_thread_id.is_some() {
                    // This is a UI escape-hatch budget, not a protocol
                    // deadline. A healthy local thread/unsubscribe round trip
                    // should finish comfortably inside two seconds, while a
                    // longer wait makes Ctrl+C feel broken when the cli-runtime
                    // is already wedged.
                    if tokio::time::timeout(
                        SHUTDOWN_FIRST_EXIT_TIMEOUT,
                        self.shutdown_current_thread(cli_runtime),
                    )
                    .await
                    .is_err()
                    {
                        tracing::warn!("timed out waiting for cli-runtime thread shutdown");
                    }
                }
                self.pending_shutdown_exit_thread_id = None;
                AppRunControl::Exit(ExitReason::UserRequested)
            }
            ExitMode::Immediate => {
                self.pending_shutdown_exit_thread_id = None;
                AppRunControl::Exit(ExitReason::UserRequested)
            }
        }
    }

    pub(super) async fn archive_current_thread(
        &mut self,
        cli_runtime: &mut CliRuntimeSession,
    ) -> AppRunControl {
        let Some(thread_id) = self.active_thread_id.or(self.chat_widget.thread_id()) else {
            self.chat_widget
                .add_error_message("A thread must start before it can be archived.".to_string());
            return AppRunControl::Continue;
        };
        if self.side_threads.contains_key(&thread_id) {
            self.chat_widget.add_error_message(
                "'/archive' is unavailable in side conversations. Press Ctrl+C to return to the main thread first."
                    .to_string(),
            );
            return AppRunControl::Continue;
        }

        match cli_runtime.thread_archive(thread_id).await {
            Ok(()) => AppRunControl::Exit(ExitReason::UserRequested),
            Err(err) => {
                self.chat_widget
                    .add_error_message(format!("Failed to archive current thread: {err}"));
                AppRunControl::Continue
            }
        }
    }

    pub(super) async fn delete_current_thread(
        &mut self,
        cli_runtime: &mut CliRuntimeSession,
    ) -> AppRunControl {
        let Some(thread_id) = self.active_thread_id.or(self.chat_widget.thread_id()) else {
            self.chat_widget
                .add_error_message("A thread must start before it can be deleted.".to_string());
            return AppRunControl::Continue;
        };
        if self.side_threads.contains_key(&thread_id) {
            self.chat_widget.add_error_message(
                "'/delete' is unavailable in side conversations. Press Ctrl+C to return to the main thread first."
                    .to_string(),
            );
            return AppRunControl::Continue;
        }

        match cli_runtime.thread_delete(thread_id).await {
            Ok(()) => AppRunControl::Exit(ExitReason::UserRequested),
            Err(err) => {
                self.chat_widget
                    .add_error_message(format!("Failed to delete current thread: {err}"));
                AppRunControl::Continue
            }
        }
    }
}
