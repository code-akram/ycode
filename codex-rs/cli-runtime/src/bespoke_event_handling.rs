use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::ClientRequestResult;
use crate::outgoing_message::ThreadScopedOutgoingMessageSender;
use crate::request_processors::populate_thread_turns_from_history;
use crate::request_processors::thread_from_stored_thread;
use crate::request_processors::thread_settings_from_core_snapshot;
use crate::server_request_error::is_turn_transition_server_request_error;
use crate::thread_state::ThreadState;
use crate::thread_state::TurnSummary;
use crate::thread_state::resolve_server_request_on_thread_listener;
use crate::thread_status::ThreadWatchActiveGuard;
use crate::thread_status::ThreadWatchManager;
use codex_cli_protocol::AccountRateLimitsUpdatedNotification;
use codex_cli_protocol::CodexErrorInfo as V2CodexErrorInfo;
use codex_cli_protocol::CommandAction as V2ParsedCommand;
use codex_cli_protocol::CommandExecutionSource;
use codex_cli_protocol::CommandExecutionStatus;
use codex_cli_protocol::DeprecationNoticeNotification;
use codex_cli_protocol::DynamicToolCallParams;
use codex_cli_protocol::EnvironmentConnectionNotification;
use codex_cli_protocol::ErrorNotification;
use codex_cli_protocol::ItemCompletedNotification;
use codex_cli_protocol::ItemStartedNotification;
use codex_cli_protocol::ModelReroutedNotification;
use codex_cli_protocol::ModelSafetyBufferingUpdatedNotification;
use codex_cli_protocol::ModelVerificationNotification;
use codex_cli_protocol::NetworkApprovalContext as V2NetworkApprovalContext;
use codex_cli_protocol::RawResponseCompletedNotification;
use codex_cli_protocol::RawResponseItemCompletedNotification;
use codex_cli_protocol::RequestId;
use codex_cli_protocol::ServerNotification;
use codex_cli_protocol::ServerRequestPayload;
use codex_cli_protocol::ThreadGoalUpdatedNotification;
use codex_cli_protocol::ThreadItem;
use codex_cli_protocol::ThreadRealtimeClosedNotification;
use codex_cli_protocol::ThreadRealtimeErrorNotification;
use codex_cli_protocol::ThreadRealtimeItemAddedNotification;
use codex_cli_protocol::ThreadRealtimeOutputAudioDeltaNotification;
use codex_cli_protocol::ThreadRealtimeSdpNotification;
use codex_cli_protocol::ThreadRealtimeStartedNotification;
use codex_cli_protocol::ThreadRealtimeTranscriptDeltaNotification;
use codex_cli_protocol::ThreadRealtimeTranscriptDoneNotification;
use codex_cli_protocol::ThreadRollbackResponse;
use codex_cli_protocol::ThreadSettingsUpdatedNotification;
use codex_cli_protocol::ThreadStatus;
use codex_cli_protocol::ThreadTokenUsage;
use codex_cli_protocol::ThreadTokenUsageUpdatedNotification;
use codex_cli_protocol::ToolRequestUserInputOption;
use codex_cli_protocol::ToolRequestUserInputParams;
use codex_cli_protocol::ToolRequestUserInputQuestion;
use codex_cli_protocol::ToolRequestUserInputResponse;
use codex_cli_protocol::Turn;
use codex_cli_protocol::TurnCompletedNotification;
use codex_cli_protocol::TurnDiffUpdatedNotification;
use codex_cli_protocol::TurnError;
use codex_cli_protocol::TurnInterruptResponse;
use codex_cli_protocol::TurnItemsView;
use codex_cli_protocol::TurnModerationMetadataNotification;
use codex_cli_protocol::TurnPlanStep;
use codex_cli_protocol::TurnPlanUpdatedNotification;
use codex_cli_protocol::TurnStartedNotification;
use codex_cli_protocol::TurnStatus;
use codex_cli_protocol::WarningNotification;
use codex_cli_protocol::item_event_to_server_notification;
use codex_core::CodexThread;
use codex_core::ThreadManager;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentTool as CoreCollabAgentTool;
use codex_protocol::items::TurnItem as CoreTurnItem;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::CodexErrorInfo as CoreCodexErrorInfo;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RealtimeEvent;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnDiffEvent;
use codex_protocol::request_user_input::RequestUserInputAnswer as CoreRequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputResponse as CoreRequestUserInputResponse;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::LegacyAppPathString;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tracing::error;

#[allow(dead_code)] // Retained compatibility projection for disabled approval events.
enum CommandExecutionApprovalPresentation {
    Network(V2NetworkApprovalContext),
    Command(CommandExecutionCompletionItem),
}

#[derive(Debug, PartialEq)]
#[allow(dead_code)] // Retained compatibility projection for disabled approval events.
struct CommandExecutionCompletionItem {
    command: String,
    cwd: LegacyAppPathString,
    command_actions: Vec<V2ParsedCommand>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_bespoke_event_handling(
    event: Event,
    conversation_id: ThreadId,
    conversation: Arc<CodexThread>,
    thread_manager: Arc<ThreadManager>,
    outgoing: ThreadScopedOutgoingMessageSender,
    thread_state: Arc<tokio::sync::Mutex<ThreadState>>,
    thread_watch_manager: ThreadWatchManager,
    thread_list_state_permit: Arc<tokio::sync::Semaphore>,
    #[allow(dead_code)]
    // Retained compatibility, test, or architectural seam for non-default consumers.
    fallback_model_provider: String,
) {
    let Event {
        id: event_turn_id,
        msg,
    } = event;
    #[allow(dead_code)]
    // Retained compatibility, test, or architectural seam for non-default consumers.
    match msg {
        EventMsg::TurnStarted(payload) => {
            // While not technically necessary as it was already done on TurnComplete, be extra cautios and abort any pending server requests.
            outgoing.abort_pending_server_requests().await;
            thread_watch_manager
                .note_turn_started(&conversation_id.to_string())
                .await;
            let turn = {
                let state = thread_state.lock().await;
                let mut turn = state.active_turn_snapshot().unwrap_or_else(|| Turn {
                    id: payload.turn_id.clone(),
                    items: Vec::new(),
                    items_view: TurnItemsView::NotLoaded,
                    error: None,
                    status: TurnStatus::InProgress,
                    started_at: payload.started_at,
                    completed_at: None,
                    duration_ms: None,
                });
                turn.items.clear();
                turn.items_view = TurnItemsView::NotLoaded;
                turn
            };
            let notification = TurnStartedNotification {
                thread_id: conversation_id.to_string(),
                turn,
            };
            outgoing
                .send_server_notification(ServerNotification::TurnStarted(notification))
                .await;
        }
        EventMsg::TurnComplete(turn_complete_event) => {
            // All per-thread requests are bound to a turn, so abort them.
            outgoing.abort_pending_server_requests().await;
            respond_to_pending_interrupts(&thread_state, &outgoing).await;
            let turn_failed = thread_state.lock().await.turn_summary.last_error.is_some();
            thread_watch_manager
                .note_turn_completed(&conversation_id.to_string(), turn_failed)
                .await;
            handle_turn_complete(
                conversation_id,
                event_turn_id,
                turn_complete_event,
                &outgoing,
                &thread_state,
            )
            .await;
        }
        EventMsg::EnvironmentConnected(event) => {
            outgoing
                .send_server_notification(ServerNotification::EnvironmentConnected(
                    EnvironmentConnectionNotification {
                        thread_id: conversation_id.to_string(),
                        environment_id: event.environment_id,
                    },
                ))
                .await;
        }
        EventMsg::EnvironmentDisconnected(event) => {
            outgoing
                .send_server_notification(ServerNotification::EnvironmentDisconnected(
                    EnvironmentConnectionNotification {
                        thread_id: conversation_id.to_string(),
                        environment_id: event.environment_id,
                    },
                ))
                .await;
        }
        EventMsg::Warning(warning_event) => {
            let notification = WarningNotification {
                thread_id: Some(conversation_id.to_string()),
                message: warning_event.message,
            };
            outgoing
                .send_server_notification(ServerNotification::Warning(notification))
                .await;
        }
        EventMsg::GuardianWarning(_) | EventMsg::GuardianAssessment(_) => {}
        EventMsg::ModelReroute(event) => {
            let notification = ModelReroutedNotification {
                thread_id: conversation_id.to_string(),
                turn_id: event_turn_id.clone(),
                from_model: event.from_model,
                to_model: event.to_model,
                reason: event.reason.into(),
            };
            outgoing
                .send_server_notification(ServerNotification::ModelRerouted(notification))
                .await;
        }
        EventMsg::ModelVerification(event) => {
            let notification = ModelVerificationNotification {
                thread_id: conversation_id.to_string(),
                turn_id: event_turn_id.clone(),
                verifications: event.verifications.into_iter().map(Into::into).collect(),
            };
            outgoing
                .send_server_notification(ServerNotification::ModelVerification(notification))
                .await;
        }
        EventMsg::TurnModerationMetadata(event) => {
            let notification = TurnModerationMetadataNotification {
                thread_id: conversation_id.to_string(),
                turn_id: event_turn_id.clone(),
                metadata: event.metadata,
            };
            outgoing
                .send_server_notification(ServerNotification::TurnModerationMetadata(notification))
                .await;
        }
        EventMsg::SafetyBuffering(event) => {
            let notification = ModelSafetyBufferingUpdatedNotification {
                thread_id: conversation_id.to_string(),
                turn_id: event_turn_id.clone(),
                model: event.model,
                use_cases: event.use_cases,
                reasons: event.reasons,
                show_buffering_ui: event.show_buffering_ui,
                faster_model: event.faster_model,
            };
            outgoing
                .send_server_notification(ServerNotification::ModelSafetyBufferingUpdated(
                    notification,
                ))
                .await;
        }
        EventMsg::RealtimeConversationStarted(event) => {
            let notification = ThreadRealtimeStartedNotification {
                thread_id: conversation_id.to_string(),
                realtime_session_id: event.realtime_session_id,
                version: event.version,
            };
            outgoing
                .send_server_notification(ServerNotification::ThreadRealtimeStarted(notification))
                .await;
        }
        EventMsg::RealtimeConversationSdp(event) => {
            let notification = ThreadRealtimeSdpNotification {
                thread_id: conversation_id.to_string(),
                sdp: event.sdp,
            };
            outgoing
                .send_server_notification(ServerNotification::ThreadRealtimeSdp(notification))
                .await;
        }
        EventMsg::RealtimeConversationRealtime(event) => match event.payload {
            RealtimeEvent::SessionUpdated { .. } => {}
            RealtimeEvent::InputAudioSpeechStarted(event) => {
                let notification = ThreadRealtimeItemAddedNotification {
                    thread_id: conversation_id.to_string(),
                    item: serde_json::json!({
                        "type": "input_audio_buffer.speech_started",
                        "item_id": event.item_id,
                    }),
                };
                outgoing
                    .send_server_notification(ServerNotification::ThreadRealtimeItemAdded(
                        notification,
                    ))
                    .await;
            }
            RealtimeEvent::InputTranscriptDelta(event) => {
                let notification = ThreadRealtimeTranscriptDeltaNotification {
                    thread_id: conversation_id.to_string(),
                    role: "user".to_string(),
                    delta: event.delta,
                };
                outgoing
                    .send_server_notification(ServerNotification::ThreadRealtimeTranscriptDelta(
                        notification,
                    ))
                    .await;
            }
            RealtimeEvent::InputTranscriptDone(event) => {
                let notification = ThreadRealtimeTranscriptDoneNotification {
                    thread_id: conversation_id.to_string(),
                    role: "user".to_string(),
                    text: event.text,
                };
                outgoing
                    .send_server_notification(ServerNotification::ThreadRealtimeTranscriptDone(
                        notification,
                    ))
                    .await;
            }
            RealtimeEvent::OutputTranscriptDelta(event) => {
                let notification = ThreadRealtimeTranscriptDeltaNotification {
                    thread_id: conversation_id.to_string(),
                    role: "assistant".to_string(),
                    delta: event.delta,
                };
                outgoing
                    .send_server_notification(ServerNotification::ThreadRealtimeTranscriptDelta(
                        notification,
                    ))
                    .await;
            }
            RealtimeEvent::OutputTranscriptDone(event) => {
                let notification = ThreadRealtimeTranscriptDoneNotification {
                    thread_id: conversation_id.to_string(),
                    role: "assistant".to_string(),
                    text: event.text,
                };
                outgoing
                    .send_server_notification(ServerNotification::ThreadRealtimeTranscriptDone(
                        notification,
                    ))
                    .await;
            }
            RealtimeEvent::AudioOut(audio) => {
                let notification = ThreadRealtimeOutputAudioDeltaNotification {
                    thread_id: conversation_id.to_string(),
                    audio: audio.into(),
                };
                outgoing
                    .send_server_notification(ServerNotification::ThreadRealtimeOutputAudioDelta(
                        notification,
                    ))
                    .await;
            }
            RealtimeEvent::ResponseCreated(_) => {}
            RealtimeEvent::ResponseCancelled(event) => {
                let notification = ThreadRealtimeItemAddedNotification {
                    thread_id: conversation_id.to_string(),
                    item: serde_json::json!({
                        "type": "response.cancelled",
                        "response_id": event.response_id,
                    }),
                };
                outgoing
                    .send_server_notification(ServerNotification::ThreadRealtimeItemAdded(
                        notification,
                    ))
                    .await;
            }
            RealtimeEvent::ResponseDone(_) => {}
            RealtimeEvent::ConversationItemAdded(item) => {
                let notification = ThreadRealtimeItemAddedNotification {
                    thread_id: conversation_id.to_string(),
                    item,
                };
                outgoing
                    .send_server_notification(ServerNotification::ThreadRealtimeItemAdded(
                        notification,
                    ))
                    .await;
            }
            RealtimeEvent::ConversationItemDone { .. } | RealtimeEvent::NoopRequested(_) => {}
            RealtimeEvent::HandoffRequested(handoff) => {
                let notification = ThreadRealtimeItemAddedNotification {
                    thread_id: conversation_id.to_string(),
                    item: serde_json::json!({
                        "type": "handoff_request",
                        "handoff_id": handoff.handoff_id,
                        "item_id": handoff.item_id,
                        "input_transcript": handoff.input_transcript,
                        "active_transcript": handoff.active_transcript,
                    }),
                };
                outgoing
                    .send_server_notification(ServerNotification::ThreadRealtimeItemAdded(
                        notification,
                    ))
                    .await;
            }
            RealtimeEvent::Error(message) => {
                let notification = ThreadRealtimeErrorNotification {
                    thread_id: conversation_id.to_string(),
                    message,
                };
                outgoing
                    .send_server_notification(ServerNotification::ThreadRealtimeError(notification))
                    .await;
            }
        },
        EventMsg::RealtimeConversationClosed(event) => {
            let notification = ThreadRealtimeClosedNotification {
                thread_id: conversation_id.to_string(),
                reason: event.reason,
            };
            outgoing
                .send_server_notification(ServerNotification::ThreadRealtimeClosed(notification))
                .await;
        }
        EventMsg::RequestUserInput(request) => {
            let user_input_guard = thread_watch_manager
                .note_user_input_requested(&conversation_id.to_string())
                .await;
            let questions = request
                .questions
                .into_iter()
                .map(|question| ToolRequestUserInputQuestion {
                    id: question.id,
                    header: question.header,
                    question: question.question,
                    is_other: question.is_other,
                    is_secret: question.is_secret,
                    options: question.options.map(|options| {
                        options
                            .into_iter()
                            .map(|option| ToolRequestUserInputOption {
                                label: option.label,
                                description: option.description,
                            })
                            .collect()
                    }),
                })
                .collect();
            let params = ToolRequestUserInputParams {
                thread_id: conversation_id.to_string(),
                turn_id: request.turn_id,
                item_id: request.call_id,
                questions,
                is_blocking: request.is_blocking,
                auto_resolution_ms: request.auto_resolution_ms,
            };
            let (pending_request_id, rx) = outgoing
                .send_request(ServerRequestPayload::ToolRequestUserInput(params))
                .await;
            tokio::spawn(async move {
                on_request_user_input_response(
                    event_turn_id,
                    pending_request_id,
                    rx,
                    conversation,
                    thread_state,
                    user_input_guard,
                )
                .await;
            });
        }
        EventMsg::ApplyPatchApprovalRequest(_)
        | EventMsg::ExecApprovalRequest(_)
        | EventMsg::RequestPermissions(_) => {}
        EventMsg::DynamicToolCallRequest(_)
        | EventMsg::DynamicToolCallResponse(_)
        | EventMsg::CollabAgentSpawnBegin(_)
        | EventMsg::CollabAgentSpawnEnd(_)
        | EventMsg::CollabAgentInteractionBegin(_)
        | EventMsg::CollabAgentInteractionEnd(_)
        | EventMsg::CollabWaitingBegin(_)
        | EventMsg::CollabWaitingEnd(_)
        | EventMsg::CollabCloseBegin(_)
        | EventMsg::CollabCloseEnd(_)
        | EventMsg::CollabResumeBegin(_)
        | EventMsg::CollabResumeEnd(_)
        | EventMsg::SubAgentActivity(_)
        | EventMsg::ExecCommandBegin(_)
        | EventMsg::ExecCommandEnd(_)
        | EventMsg::EnteredReviewMode(_)
        | EventMsg::ExitedReviewMode(_) => {
            // Deprecated item lifecycle events are still fanned out for raw-event and rollout
            // compatibility consumers.
            // App-server v2 receives TurnItem lifecycle instead, and dispatches dynamic tool
            // requests from DynamicToolCall starts.
        }
        msg @ (EventMsg::AgentMessageContentDelta(_)
        | EventMsg::ReasoningContentDelta(_)
        | EventMsg::ReasoningRawContentDelta(_)
        | EventMsg::AgentReasoningSectionBreak(_)) => {
            let notification = item_event_to_server_notification(
                msg,
                &conversation_id.to_string(),
                &event_turn_id,
            );
            outgoing.send_server_notification(notification).await;
        }
        EventMsg::ContextCompacted(..) => {
            // Core still fans out this deprecated event for raw-event and rollout compatibility
            // consumers;
            // v2 clients receive the canonical ContextCompaction item instead.
        }
        EventMsg::DeprecationNotice(event) => {
            let notification = DeprecationNoticeNotification {
                summary: event.summary,
                details: event.details,
            };
            outgoing
                .send_server_notification(ServerNotification::DeprecationNotice(notification))
                .await;
        }
        EventMsg::TokenCount(token_count_event) => {
            handle_token_count_event(conversation_id, event_turn_id, token_count_event, &outgoing)
                .await;
        }
        EventMsg::Error(ev) => {
            thread_watch_manager
                .note_system_error(&conversation_id.to_string())
                .await;

            let message = ev.message.clone();
            let codex_error_info = ev.codex_error_info.clone();
            // If this error belongs to an in-flight `thread/rollback` request, fail that request
            // (and clear pending state) so subsequent rollbacks are unblocked.
            //
            // Don't send a notification for this error.
            if matches!(
                codex_error_info,
                Some(CoreCodexErrorInfo::ThreadRollbackFailed)
            ) {
                return handle_thread_rollback_failed(
                    conversation_id,
                    message,
                    &thread_state,
                    &outgoing,
                )
                .await;
            };

            if !ev.affects_turn_status() {
                return;
            }

            let turn_error = TurnError {
                message: ev.message,
                codex_error_info: ev.codex_error_info.map(V2CodexErrorInfo::from),
                additional_details: None,
            };
            handle_error_notification(
                conversation_id,
                &event_turn_id,
                turn_error,
                &outgoing,
                &thread_state,
            )
            .await;
        }
        EventMsg::StreamError(ev) => {
            // We don't need to update the turn summary store for stream errors as they are intermediate error states for retries,
            // but we notify the client.
            let turn_error = TurnError {
                message: ev.message,
                codex_error_info: ev.codex_error_info.map(V2CodexErrorInfo::from),
                additional_details: ev.additional_details,
            };
            outgoing
                .send_server_notification(ServerNotification::Error(ErrorNotification {
                    error: turn_error,
                    will_retry: true,
                    thread_id: conversation_id.to_string(),
                    turn_id: event_turn_id.clone(),
                }))
                .await;
        }
        EventMsg::ViewImageToolCall(_) => {}
        EventMsg::ItemStarted(event) => {
            let should_emit = match &event.item {
                // Approval and guardian flows can emit the command start notification before core
                // emits the canonical item. Reuse the same set to suppress that duplicate.
                CoreTurnItem::CommandExecution(item) => thread_state
                    .lock()
                    .await
                    .turn_summary
                    .command_execution_started
                    .insert(item.id.clone()),
                _ => true,
            };
            let dynamic_tool_call_params = match &event.item {
                CoreTurnItem::DynamicToolCall(item) => Some(DynamicToolCallParams {
                    thread_id: conversation_id.to_string(),
                    turn_id: event.turn_id.clone(),
                    call_id: item.id.clone(),
                    namespace: item.namespace.clone(),
                    tool: item.tool.clone(),
                    arguments: item.arguments.clone(),
                }),
                _ => None,
            };
            if should_emit {
                let notification = item_event_to_server_notification(
                    EventMsg::ItemStarted(event),
                    &conversation_id.to_string(),
                    &event_turn_id,
                );
                outgoing.send_server_notification(notification).await;
            }
            if let Some(params) = dynamic_tool_call_params {
                let call_id = params.call_id.clone();
                let (_pending_request_id, rx) = outgoing
                    .send_request(ServerRequestPayload::DynamicToolCall(params))
                    .await;
                tokio::spawn(async move {
                    crate::dynamic_tools::on_call_response(call_id, rx, conversation).await;
                });
            }
        }
        EventMsg::ItemCompleted(event) => {
            apply_canonical_item_completed_side_effects(
                &thread_manager,
                &thread_watch_manager,
                &thread_state,
                &event.item,
            )
            .await;
            let notification = item_event_to_server_notification(
                EventMsg::ItemCompleted(event),
                &conversation_id.to_string(),
                &event_turn_id,
            );
            outgoing.send_server_notification(notification).await;
        }
        msg @ (EventMsg::PatchApplyUpdated(_) | EventMsg::TerminalInteraction(_)) => {
            let notification = item_event_to_server_notification(
                msg,
                &conversation_id.to_string(),
                &event_turn_id,
            );
            outgoing.send_server_notification(notification).await;
        }
        EventMsg::RawResponseItem(raw_response_item_event) => {
            maybe_emit_raw_response_item_completed(
                conversation_id,
                &event_turn_id,
                raw_response_item_event.item,
                &outgoing,
            )
            .await;
        }
        EventMsg::RawResponseCompleted(raw_response_completed_event) => {
            let notification = RawResponseCompletedNotification {
                thread_id: conversation_id.to_string(),
                turn_id: event_turn_id,
                response_id: raw_response_completed_event.response_id,
                usage: raw_response_completed_event.token_usage.map(Into::into),
            };
            outgoing
                .send_server_notification(ServerNotification::RawResponseCompleted(notification))
                .await;
        }
        EventMsg::PatchApplyBegin(_) | EventMsg::PatchApplyEnd(_) => {
            // Core still fans out these deprecated events for raw-event and rollout compatibility
            // consumers;
            // v2 clients receive the canonical FileChange item instead.
        }
        EventMsg::ExecCommandOutputDelta(exec_command_output_delta_event) => {
            let notification = item_event_to_server_notification(
                EventMsg::ExecCommandOutputDelta(exec_command_output_delta_event),
                &conversation_id.to_string(),
                &event_turn_id,
            );
            outgoing.send_server_notification(notification).await;
        }
        // If this is a TurnAborted, reply to any pending interrupt requests.
        EventMsg::TurnAborted(turn_aborted_event) => {
            // All per-thread requests are bound to a turn, so abort them.
            outgoing.abort_pending_server_requests().await;
            respond_to_pending_interrupts(&thread_state, &outgoing).await;

            thread_watch_manager
                .note_turn_interrupted(&conversation_id.to_string())
                .await;
            handle_turn_interrupted(
                conversation_id,
                event_turn_id,
                turn_aborted_event,
                &outgoing,
                &thread_state,
            )
            .await;
        }
        EventMsg::ThreadRolledBack(_rollback_event) => {
            let pending = {
                let mut state = thread_state.lock().await;
                state.pending_rollbacks.take()
            };

            if let Some(request_id) = pending {
                let _thread_list_state_permit = match thread_list_state_permit.acquire().await {
                    Ok(permit) => permit,
                    Err(err) => {
                        outgoing
                            .send_error(
                                request_id,
                                internal_error(format!(
                                    "failed to acquire thread list state permit: {err}"
                                )),
                            )
                            .await;
                        return;
                    }
                };
                let fallback_cwd = conversation.config_snapshot().await.cwd().clone();
                let stored_thread = match conversation
                    .read_thread(
                        /*include_archived*/ true, /*include_history*/ true,
                    )
                    .await
                {
                    Ok(stored_thread) => stored_thread,
                    Err(err) => {
                        outgoing
                            .send_error(
                                request_id.clone(),
                                internal_error(format!(
                                    "failed to read thread {conversation_id} after rollback: {err}"
                                )),
                            )
                            .await;
                        return;
                    }
                };
                let loaded_status = thread_watch_manager
                    .loaded_status_for_thread(&conversation_id.to_string())
                    .await;
                let response = match thread_rollback_response_from_stored_thread(
                    stored_thread,
                    conversation.session_configured().session_id.to_string(),
                    fallback_model_provider.as_str(),
                    &fallback_cwd,
                    loaded_status,
                ) {
                    Ok(response) => response,
                    Err(err) => {
                        outgoing
                            .send_error(request_id.clone(), internal_error(err))
                            .await;
                        return;
                    }
                };

                outgoing.send_response(request_id, response).await;
            }
        }
        EventMsg::ThreadGoalUpdated(thread_goal_event) => {
            let notification = ThreadGoalUpdatedNotification {
                thread_id: thread_goal_event.thread_id.to_string(),
                turn_id: thread_goal_event.turn_id,
                goal: thread_goal_event.goal.clone().into(),
            };
            outgoing
                .send_global_server_notification(ServerNotification::ThreadGoalUpdated(
                    notification,
                ))
                .await;
        }
        EventMsg::ThreadSettingsApplied(thread_settings_event) => {
            let thread_settings =
                thread_settings_from_core_snapshot(thread_settings_event.thread_settings);
            let changed = {
                let mut state = thread_state.lock().await;
                state.note_thread_settings(thread_settings.clone())
            };
            if changed {
                outgoing
                    .send_server_notification(ServerNotification::ThreadSettingsUpdated(
                        ThreadSettingsUpdatedNotification {
                            thread_id: conversation_id.to_string(),
                            thread_settings,
                        },
                    ))
                    .await;
            }
        }
        EventMsg::TurnDiff(turn_diff_event) => {
            handle_turn_diff(conversation_id, &event_turn_id, turn_diff_event, &outgoing).await;
        }
        EventMsg::PlanUpdate(plan_update_event) => {
            handle_turn_plan_update(
                conversation_id,
                &event_turn_id,
                plan_update_event,
                &outgoing,
            )
            .await;
        }
        EventMsg::ShutdownComplete => {
            thread_watch_manager
                .note_thread_shutdown(&conversation_id.to_string())
                .await;
        }

        _ => {}
    }
}

async fn handle_turn_diff(
    conversation_id: ThreadId,
    event_turn_id: &str,
    turn_diff_event: TurnDiffEvent,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    let notification = TurnDiffUpdatedNotification {
        thread_id: conversation_id.to_string(),
        turn_id: event_turn_id.to_string(),
        diff: turn_diff_event.unified_diff,
    };
    outgoing
        .send_server_notification(ServerNotification::TurnDiffUpdated(notification))
        .await;
}

async fn handle_turn_plan_update(
    conversation_id: ThreadId,
    event_turn_id: &str,
    plan_update_event: UpdatePlanArgs,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    // `update_plan` is a todo/checklist tool; it is not related to plan-mode updates
    let notification = TurnPlanUpdatedNotification {
        thread_id: conversation_id.to_string(),
        turn_id: event_turn_id.to_string(),
        explanation: plan_update_event.explanation,
        plan: plan_update_event
            .plan
            .into_iter()
            .map(TurnPlanStep::from)
            .collect(),
    };
    outgoing
        .send_server_notification(ServerNotification::TurnPlanUpdated(notification))
        .await;
}

struct TurnCompletionMetadata {
    status: TurnStatus,
    error: Option<TurnError>,
    last_agent_message: Option<ThreadItem>,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
}

async fn emit_turn_completed_with_status(
    conversation_id: ThreadId,
    event_turn_id: String,
    turn_completion_metadata: TurnCompletionMetadata,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    let (items, items_view) = match turn_completion_metadata.last_agent_message {
        Some(item) => (vec![item], TurnItemsView::Summary),
        None => (Vec::new(), TurnItemsView::NotLoaded),
    };
    let notification = TurnCompletedNotification {
        thread_id: conversation_id.to_string(),
        turn: Turn {
            id: event_turn_id,
            items,
            items_view,
            error: turn_completion_metadata.error,
            status: turn_completion_metadata.status,
            started_at: turn_completion_metadata.started_at,
            completed_at: turn_completion_metadata.completed_at,
            duration_ms: turn_completion_metadata.duration_ms,
        },
    };
    outgoing
        .send_server_notification(ServerNotification::TurnCompleted(notification))
        .await;
}

async fn apply_canonical_item_completed_side_effects(
    thread_manager: &Arc<ThreadManager>,
    thread_watch_manager: &ThreadWatchManager,
    thread_state: &Arc<Mutex<ThreadState>>,
    item: &CoreTurnItem,
) {
    match item {
        CoreTurnItem::CommandExecution(item) => {
            thread_state
                .lock()
                .await
                .turn_summary
                .command_execution_started
                .remove(&item.id);
        }
        CoreTurnItem::SubAgentActivity(activity)
            if activity.kind == SubAgentActivityKind::Interrupted =>
        {
            remove_missing_thread_watch(
                thread_manager,
                thread_watch_manager,
                activity.agent_thread_id,
            )
            .await;
        }
        CoreTurnItem::CollabAgentToolCall(item) if item.tool == CoreCollabAgentTool::CloseAgent => {
            for thread_id in &item.receiver_thread_ids {
                remove_missing_thread_watch(thread_manager, thread_watch_manager, *thread_id).await;
            }
        }
        _ => {}
    }
}

async fn remove_missing_thread_watch(
    thread_manager: &Arc<ThreadManager>,
    thread_watch_manager: &ThreadWatchManager,
    thread_id: ThreadId,
) {
    if thread_manager.get_thread(thread_id).await.is_err() {
        thread_watch_manager
            .remove_thread(&thread_id.to_string())
            .await;
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(dead_code)] // Retained compatibility projection for disabled approval events.
async fn start_command_execution_item(
    conversation_id: &ThreadId,
    turn_id: String,
    item_id: String,
    command: String,
    cwd: LegacyAppPathString,
    command_actions: Vec<V2ParsedCommand>,
    source: CommandExecutionSource,
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_state: &Arc<Mutex<ThreadState>>,
) -> bool {
    let first_start = {
        let mut state = thread_state.lock().await;
        state
            .turn_summary
            .command_execution_started
            .insert(item_id.clone())
    };
    if first_start {
        let notification = ItemStartedNotification {
            thread_id: conversation_id.to_string(),
            turn_id,
            started_at_ms: now_unix_timestamp_ms(),
            item: ThreadItem::CommandExecution {
                id: item_id,
                command,
                cwd,
                process_id: None,
                source,
                status: CommandExecutionStatus::InProgress,
                command_actions,
                aggregated_output: None,
                exit_code: None,
                duration_ms: None,
            },
        };
        outgoing
            .send_server_notification(ServerNotification::ItemStarted(notification))
            .await;
    }
    first_start
}

#[allow(clippy::too_many_arguments)]
#[allow(dead_code)] // Retained compatibility projection for disabled approval events.
async fn complete_command_execution_item(
    conversation_id: &ThreadId,
    turn_id: String,
    item_id: String,
    completion_item: CommandExecutionCompletionItem,
    process_id: Option<String>,
    source: CommandExecutionSource,
    status: CommandExecutionStatus,
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_state: &Arc<Mutex<ThreadState>>,
) {
    let should_emit = thread_state
        .lock()
        .await
        .turn_summary
        .command_execution_started
        .remove(&item_id);
    if !should_emit {
        return;
    }

    let item = ThreadItem::CommandExecution {
#[allow(dead_code)] // Retained compatibility, test, or architectural seam for non-default consumers.
        id: item_id,
        command: completion_item.command,
        cwd: completion_item.cwd,
        process_id,
        source,
        status,
        command_actions: completion_item.command_actions,
        aggregated_output: None,
        exit_code: None,
        duration_ms: None,
    };
    let notification = ItemCompletedNotification {
        thread_id: conversation_id.to_string(),
        turn_id,
        completed_at_ms: now_unix_timestamp_ms(),
        item,
    };
    outgoing
        .send_server_notification(ServerNotification::ItemCompleted(notification))
        .await;
}

async fn maybe_emit_raw_response_item_completed(
    conversation_id: ThreadId,
    turn_id: &str,
    item: codex_protocol::models::ResponseItem,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    let notification = RawResponseItemCompletedNotification {
        thread_id: conversation_id.to_string(),
        turn_id: turn_id.to_string(),
        item,
    };
    outgoing
        .send_server_notification(ServerNotification::RawResponseItemCompleted(notification))
        .await;
}

async fn find_and_remove_turn_summary(
    _conversation_id: ThreadId,
    thread_state: &Arc<Mutex<ThreadState>>,
) -> TurnSummary {
    let mut state = thread_state.lock().await;
    std::mem::take(&mut state.turn_summary)
}

async fn handle_turn_complete(
    conversation_id: ThreadId,
    event_turn_id: String,
    turn_complete_event: TurnCompleteEvent,
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_state: &Arc<Mutex<ThreadState>>,
) {
    let turn_summary = find_and_remove_turn_summary(conversation_id, thread_state).await;

    let (status, error, last_agent_message) = match turn_summary.last_error {
        Some(error) => (TurnStatus::Failed, Some(error), None),
        None => (TurnStatus::Completed, None, turn_summary.last_agent_message),
    };

    emit_turn_completed_with_status(
        conversation_id,
        event_turn_id,
        TurnCompletionMetadata {
            status,
            error,
            last_agent_message,
            started_at: turn_summary.started_at,
            completed_at: turn_complete_event.completed_at,
            duration_ms: turn_complete_event.duration_ms,
        },
        outgoing,
    )
    .await;
}

async fn handle_turn_interrupted(
    conversation_id: ThreadId,
    event_turn_id: String,
    turn_aborted_event: TurnAbortedEvent,
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_state: &Arc<Mutex<ThreadState>>,
) {
    let turn_summary = find_and_remove_turn_summary(conversation_id, thread_state).await;

    emit_turn_completed_with_status(
        conversation_id,
        event_turn_id,
        TurnCompletionMetadata {
            status: TurnStatus::Interrupted,
            error: None,
            last_agent_message: None,
            started_at: turn_summary.started_at,
            completed_at: turn_aborted_event.completed_at,
            duration_ms: turn_aborted_event.duration_ms,
        },
        outgoing,
    )
    .await;
}

async fn handle_thread_rollback_failed(
    _conversation_id: ThreadId,
    message: String,
    thread_state: &Arc<Mutex<ThreadState>>,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    let pending_rollback = thread_state.lock().await.pending_rollbacks.take();

    if let Some(request_id) = pending_rollback {
        outgoing
            .send_error(request_id, invalid_request(message))
            .await;
    }
}

fn thread_rollback_response_from_stored_thread(
    stored_thread: codex_thread_store::StoredThread,
    session_id: String,
    fallback_model_provider: &str,
    fallback_cwd: &AbsolutePathBuf,
    loaded_status: ThreadStatus,
) -> std::result::Result<ThreadRollbackResponse, String> {
    let thread_id = stored_thread.thread_id;
    let (mut thread, history) =
        thread_from_stored_thread(stored_thread, fallback_model_provider, fallback_cwd);
    thread.session_id = session_id;
    let Some(history) = history else {
        return Err(format!(
            "thread {thread_id} did not include persisted history after rollback"
        ));
    };
    populate_thread_turns_from_history(&mut thread, &history.items, /*active_turn*/ None);
    thread.status = loaded_status;
    Ok(ThreadRollbackResponse { thread })
}

async fn respond_to_pending_interrupts(
    thread_state: &Arc<Mutex<ThreadState>>,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    let pending = {
        let mut state = thread_state.lock().await;
        std::mem::take(&mut state.pending_interrupts)
    };

    for request_id in pending {
        outgoing
            .send_response(request_id, TurnInterruptResponse {})
            .await;
    }
}

async fn handle_token_count_event(
    conversation_id: ThreadId,
    turn_id: String,
    token_count_event: TokenCountEvent,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    let TokenCountEvent { info, rate_limits } = token_count_event;
    if let Some(token_usage) = info.map(ThreadTokenUsage::from) {
        let notification = ThreadTokenUsageUpdatedNotification {
            thread_id: conversation_id.to_string(),
            turn_id,
            token_usage,
        };
        outgoing
            .send_server_notification(ServerNotification::ThreadTokenUsageUpdated(notification))
            .await;
    }
    if let Some(rate_limits) = rate_limits {
        outgoing
            .send_server_notification(ServerNotification::AccountRateLimitsUpdated(
                AccountRateLimitsUpdatedNotification {
                    rate_limits: rate_limits.into(),
                },
            ))
            .await;
    }
}

async fn handle_error(
    _conversation_id: ThreadId,
    error: TurnError,
    thread_state: &Arc<Mutex<ThreadState>>,
) {
    let mut state = thread_state.lock().await;
    state.turn_summary.last_error = Some(error);
}

async fn handle_error_notification(
    conversation_id: ThreadId,
    event_turn_id: &str,
    error: TurnError,
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_state: &Arc<Mutex<ThreadState>>,
) {
    handle_error(conversation_id, error.clone(), thread_state).await;
    outgoing
        .send_server_notification(ServerNotification::Error(ErrorNotification {
            error,
            will_retry: false,
            thread_id: conversation_id.to_string(),
            turn_id: event_turn_id.to_string(),
        }))
        .await;
}

async fn on_request_user_input_response(
    event_turn_id: String,
    pending_request_id: RequestId,
    receiver: oneshot::Receiver<ClientRequestResult>,
    conversation: Arc<CodexThread>,
    thread_state: Arc<Mutex<ThreadState>>,
    user_input_guard: ThreadWatchActiveGuard,
) {
    let response = receiver.await;
    resolve_server_request_on_thread_listener(&thread_state, pending_request_id).await;
    drop(user_input_guard);
    let value = match response {
        Ok(Ok(value)) => value,
        Ok(Err(err)) if is_turn_transition_server_request_error(&err) => return,
        Ok(Err(err)) => {
            error!("request failed with client error: {err:?}");
            let empty = CoreRequestUserInputResponse {
                answers: HashMap::new(),
            };
            if let Err(err) = conversation
                .submit(Op::UserInputAnswer {
                    id: event_turn_id,
                    response: empty,
                })
                .await
            {
                error!("failed to submit UserInputAnswer: {err}");
            }
            return;
        }
        Err(err) => {
            error!("request failed: {err:?}");
            let empty = CoreRequestUserInputResponse {
                answers: HashMap::new(),
            };
            if let Err(err) = conversation
                .submit(Op::UserInputAnswer {
                    id: event_turn_id,
                    response: empty,
                })
                .await
            {
                error!("failed to submit UserInputAnswer: {err}");
            }
            return;
        }
    };

    let response =
        serde_json::from_value::<ToolRequestUserInputResponse>(value).unwrap_or_else(|err| {
            error!("failed to deserialize ToolRequestUserInputResponse: {err}");
            ToolRequestUserInputResponse {
                answers: HashMap::new(),
            }
        });
    let response = CoreRequestUserInputResponse {
        answers: response
            .answers
            .into_iter()
            .map(|(id, answer)| {
                (
                    id,
                    CoreRequestUserInputAnswer {
                        answers: answer.answers,
                    },
                )
            })
            .collect(),
    };

    if let Err(err) = conversation
        .submit(Op::UserInputAnswer {
            id: event_turn_id,
            response,
        })
        .await
    {
        error!("failed to submit UserInputAnswer: {err}");
    }
}

#[allow(dead_code)] // Retained timestamp helper for compatibility event projection.
fn now_unix_timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outgoing_message::ConnectionId;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessage;
    use crate::outgoing_message::OutgoingMessageSender;
    use crate::transport::CHANNEL_CAPACITY;
    use anyhow::Result;
    use anyhow::anyhow;
    use anyhow::bail;
    use chrono::Utc;
    use codex_cli_protocol::ServerRequest;
    use codex_cli_protocol::TurnPlanStepStatus;
    use codex_login::CodexAuth;
    #[allow(dead_code)]
    // Retained compatibility, test, or architectural seam for non-default consumers.
    use codex_protocol::AgentPath;
    use codex_protocol::items::AgentMessageContent as CoreAgentMessageContent;
    use codex_protocol::items::AgentMessageItem as CoreAgentMessageItem;
    use codex_protocol::items::DynamicToolCallItem;
    use codex_protocol::items::DynamicToolCallStatus as CoreDynamicToolCallStatus;
    use codex_protocol::items::SubAgentActivityItem;
    use codex_protocol::items::TurnItem as CoreTurnItem;
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::plan_tool::PlanItemArg;
    use codex_protocol::plan_tool::StepStatus;
    use codex_protocol::protocol::AgentMessageEvent;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::CreditsSnapshot;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ItemCompletedEvent;
    use codex_protocol::protocol::ItemStartedEvent;
    use codex_protocol::protocol::RateLimitSnapshot;
    use codex_protocol::protocol::RateLimitWindow;
    use codex_protocol::protocol::RolloutItem;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::TokenUsage;
    use codex_protocol::protocol::TokenUsageInfo;
    use codex_protocol::protocol::UserMessageEvent;
    use codex_thread_store::StoredThread;
    use codex_thread_store::StoredThreadHistory;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use core_test_support::load_default_config_for_test;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::sync::Mutex;
    use tokio::sync::mpsc;

    fn new_thread_state() -> Arc<Mutex<ThreadState>> {
        Arc::new(Mutex::new(ThreadState::default()))
    }

    const TEST_TURN_COMPLETED_AT: i64 = 1_716_000_456;
    const TEST_TURN_DURATION_MS: i64 = 1_234;

    async fn recv_broadcast_message(
        rx: &mut mpsc::Receiver<OutgoingEnvelope>,
    ) -> Result<OutgoingMessage> {
        let envelope = rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("should send one message"))?;
        match envelope {
            OutgoingEnvelope::Broadcast { message } => Ok(message),
            OutgoingEnvelope::ToConnection { message, .. } => Ok(message),
        }
    }

    async fn recv_broadcast_notification(
        rx: &mut mpsc::Receiver<OutgoingEnvelope>,
    ) -> Result<ServerNotification> {
        let message = recv_broadcast_message(rx).await?;
        let OutgoingMessage::CliRuntimeNotification(envelope) = message else {
            bail!("unexpected message: {message:?}");
        };
        Ok(envelope.notification)
    }

    #[test]
    fn rollback_response_rebuilds_pathless_thread_from_stored_history() -> Result<()> {
        let thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000789")?;
        let created_at = Utc::now();
        let history_items = vec![
            RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                client_id: None,
                message: "before rollback".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            })),
            RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
                message: "after rollback".to_string(),
                phase: None,
                memory_citation: None,
            })),
        ];
        let stored_thread = StoredThread {
            thread_id,
            extra_config: None,
            rollout_path: None,
            forked_from_id: None,
            parent_thread_id: None,
            preview: "fallback preview".to_string(),
            name: Some("Rollback thread".to_string()),
            model_provider: "openai".to_string(),
            model: None,
            reasoning_effort: None,
            created_at,
            updated_at: created_at,
            recency_at: created_at,
            archived_at: None,
            section: None,
            section_position: None,
            section_entered_at: None,
            cwd: test_path_buf("/tmp").abs().into(),
            cli_version: "0.0.0".to_string(),
            source: SessionSource::Cli,
            history_mode: Default::default(),
            thread_source: None,
            agent_nickname: None,
            agent_role: None,
            agent_path: None,
            git_info: None,
            approval_mode: AskForApproval::OnRequest,
            permission_profile: PermissionProfile::read_only(),
            token_usage: None,
            first_user_message: Some("before rollback".to_string()),
            history: Some(StoredThreadHistory {
                thread_id,
                items: history_items,
            }),
        };
        let fallback_cwd = test_path_buf("/tmp").abs();

        let response = thread_rollback_response_from_stored_thread(
            stored_thread,
            thread_id.to_string(),
            "fallback-provider",
            &fallback_cwd,
            ThreadStatus::NotLoaded,
        )
        .expect("rollback response should rebuild from stored history");

        assert_eq!(response.thread.id, thread_id.to_string());
        assert_eq!(response.thread.path, None);
        assert_eq!(response.thread.preview, "fallback preview");
        assert_eq!(response.thread.name.as_deref(), Some("Rollback thread"));
        assert_eq!(response.thread.status, ThreadStatus::NotLoaded);
        assert_eq!(response.thread.turns.len(), 1);
        assert_eq!(response.thread.turns[0].items.len(), 2);
        Ok(())
    }

    fn turn_complete_event(turn_id: &str) -> TurnCompleteEvent {
        TurnCompleteEvent {
            turn_id: turn_id.to_string(),
            started_at: None,
            last_agent_message: None,
            error: None,
            completed_at: Some(TEST_TURN_COMPLETED_AT),
            duration_ms: Some(TEST_TURN_DURATION_MS),
            time_to_first_token_ms: None,
        }
    }

    fn turn_aborted_event(turn_id: &str) -> TurnAbortedEvent {
        TurnAbortedEvent {
            turn_id: Some(turn_id.to_string()),
            started_at: None,
            reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
            completed_at: Some(TEST_TURN_COMPLETED_AT),
            duration_ms: Some(TEST_TURN_DURATION_MS),
        }
    }

    fn command_execution_completion_item(command: &str) -> CommandExecutionCompletionItem {
        CommandExecutionCompletionItem {
            command: command.to_string(),
            cwd: test_path_buf("/tmp").abs().into(),
            command_actions: vec![V2ParsedCommand::Unknown {
                command: command.to_string(),
            }],
        }
    }

    #[tokio::test]
    async fn command_execution_started_helper_emits_once() -> Result<()> {
        let conversation_id = ThreadId::new();
        let thread_state = new_thread_state();
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing,
            vec![ConnectionId(1)],
            ThreadId::new(),
        );
        let completion_item = command_execution_completion_item("printf hi");

        let first_start = start_command_execution_item(
            &conversation_id,
            "turn-1".to_string(),
            "cmd-1".to_string(),
            completion_item.command.clone(),
            completion_item.cwd.clone(),
            completion_item.command_actions.clone(),
            CommandExecutionSource::Agent,
            &outgoing,
            &thread_state,
        )
        .await;
        assert!(first_start);

        let msg = recv_broadcast_notification(&mut rx).await?;
        match msg {
            ServerNotification::ItemStarted(payload) => {
                assert_eq!(payload.thread_id, conversation_id.to_string());
                assert_eq!(payload.turn_id, "turn-1");
                assert_eq!(
                    payload.item,
                    ThreadItem::CommandExecution {
                        id: "cmd-1".to_string(),
                        command: completion_item.command.clone(),
                        cwd: completion_item.cwd.clone(),
                        process_id: None,
                        source: CommandExecutionSource::Agent,
                        status: CommandExecutionStatus::InProgress,
                        command_actions: completion_item.command_actions.clone(),
                        aggregated_output: None,
                        exit_code: None,
                        duration_ms: None,
                    }
                );
            }
            other => bail!("unexpected message: {other:?}"),
        }

        let second_start = start_command_execution_item(
            &conversation_id,
            "turn-1".to_string(),
            "cmd-1".to_string(),
            completion_item.command.clone(),
            completion_item.cwd.clone(),
            completion_item.command_actions.clone(),
            CommandExecutionSource::Agent,
            &outgoing,
            &thread_state,
        )
        .await;
        assert!(!second_start);
        assert!(rx.try_recv().is_err(), "duplicate start should not emit");
        Ok(())
    }

    #[tokio::test]
    async fn complete_command_execution_item_emits_declined_once_for_pending_command() -> Result<()>
    {
        let conversation_id = ThreadId::new();
        let thread_state = new_thread_state();
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing,
            vec![ConnectionId(1)],
            ThreadId::new(),
        );
        let completion_item = command_execution_completion_item("printf hi");

        start_command_execution_item(
            &conversation_id,
            "turn-1".to_string(),
            "cmd-1".to_string(),
            completion_item.command.clone(),
            completion_item.cwd.clone(),
            completion_item.command_actions.clone(),
            CommandExecutionSource::Agent,
            &outgoing,
            &thread_state,
        )
        .await;
        let _started = recv_broadcast_notification(&mut rx).await?;

        complete_command_execution_item(
            &conversation_id,
            "turn-1".to_string(),
            "cmd-1".to_string(),
            completion_item,
            /*process_id*/ None,
            CommandExecutionSource::Agent,
            CommandExecutionStatus::Declined,
            &outgoing,
            &thread_state,
        )
        .await;

        let completed = recv_broadcast_notification(&mut rx).await?;
        match completed {
            ServerNotification::ItemCompleted(payload) => {
                let ThreadItem::CommandExecution { id, status, .. } = payload.item else {
                    bail!("expected command execution completion");
                };
                assert_eq!(id, "cmd-1");
                assert_eq!(status, CommandExecutionStatus::Declined);
            }
            other => bail!("unexpected message: {other:?}"),
        }

        complete_command_execution_item(
            &conversation_id,
            "turn-1".to_string(),
            "cmd-1".to_string(),
            command_execution_completion_item("printf hi"),
            /*process_id*/ None,
            CommandExecutionSource::Agent,
            CommandExecutionStatus::Declined,
            &outgoing,
            &thread_state,
        )
        .await;
        assert!(
            rx.try_recv().is_err(),
            "completion should not emit after the pending item is cleared"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_handle_error_records_message() -> Result<()> {
        let conversation_id = ThreadId::new();
        let thread_state = new_thread_state();

        handle_error(
            conversation_id,
            TurnError {
                message: "boom".to_string(),
                codex_error_info: Some(V2CodexErrorInfo::InternalServerError),
                additional_details: None,
            },
            &thread_state,
        )
        .await;

        let turn_summary = find_and_remove_turn_summary(conversation_id, &thread_state).await;
        assert_eq!(
            turn_summary.last_error,
            Some(TurnError {
                message: "boom".to_string(),
                codex_error_info: Some(V2CodexErrorInfo::InternalServerError),
                additional_details: None,
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn turn_started_omits_active_snapshot_items() -> Result<()> {
        let codex_home = TempDir::new()?;
        let config = load_default_config_for_test(&codex_home).await;
        let thread_manager = Arc::new(
            codex_core::test_support::thread_manager_with_models_provider_and_home(
                CodexAuth::create_dummy_chatgpt_auth_for_testing(),
                config.model_provider.clone(),
                config.codex_home.to_path_buf(),
                Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
            ),
        );
        let codex_core::NewThread {
            thread_id: conversation_id,
            thread: conversation,
            ..
        } = thread_manager
            .start_thread(codex_core::StartThreadOptions::new(config.clone()))
            .await?;
        let thread_state = new_thread_state();
        {
            let mut state = thread_state.lock().await;
            state.track_current_turn_event(
                "turn-1",
                &EventMsg::TurnStarted(codex_protocol::protocol::TurnStartedEvent {
                    turn_id: "turn-1".to_string(),
                    trace_id: None,
                    started_at: Some(42),
                    model_context_window: None,
                }),
            );
            state.track_current_turn_event(
                "turn-1",
                &EventMsg::UserMessage(codex_protocol::protocol::UserMessageEvent {
                    client_id: None,
                    message: "already tracked".to_string(),
                    images: None,
                    local_images: Vec::new(),
                    text_elements: Vec::new(),
                    ..Default::default()
                }),
            );
        }
        let thread_watch_manager = ThreadWatchManager::new();
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing,
            vec![ConnectionId(1)],
            conversation_id,
        );

        apply_bespoke_event_handling(
            Event {
                id: "turn-1".to_string(),
                msg: EventMsg::TurnStarted(codex_protocol::protocol::TurnStartedEvent {
                    turn_id: "turn-1".to_string(),
                    trace_id: None,
                    started_at: Some(42),
                    model_context_window: None,
                }),
            },
            conversation_id,
            conversation,
            thread_manager,
            outgoing,
            thread_state,
            thread_watch_manager,
            Arc::new(tokio::sync::Semaphore::new(/*permits*/ 1)),
            "test-provider".to_string(),
        )
        .await;

        let msg = recv_broadcast_notification(&mut rx).await?;
        match msg {
            ServerNotification::TurnStarted(n) => {
                assert_eq!(n.turn.id, "turn-1");
                assert_eq!(n.turn.items_view, TurnItemsView::NotLoaded);
                assert!(n.turn.items.is_empty());
            }
            other => bail!("unexpected message: {other:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn interrupted_subagent_activity_removes_missing_thread_watch() -> Result<()> {
        let codex_home = TempDir::new()?;
        let config = load_default_config_for_test(&codex_home).await;
        let thread_manager = Arc::new(
            codex_core::test_support::thread_manager_with_models_provider_and_home(
                CodexAuth::create_dummy_chatgpt_auth_for_testing(),
                config.model_provider.clone(),
                config.codex_home.to_path_buf(),
                Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
            ),
        );
        let codex_core::NewThread {
            thread_id: conversation_id,
            thread: conversation,
            ..
        } = thread_manager
            .start_thread(codex_core::StartThreadOptions::new(config))
            .await?;
        let child_thread_id = ThreadId::new();
        let child_thread_id_string = child_thread_id.to_string();
        let thread_watch_manager = ThreadWatchManager::new();
        thread_watch_manager
            .note_turn_started(&child_thread_id_string)
            .await;
        assert_eq!(thread_watch_manager.running_turn_count().await, 1);
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing,
            vec![ConnectionId(1)],
            conversation_id,
        );

        apply_bespoke_event_handling(
            Event {
                id: "turn-1".to_string(),
                msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                    thread_id: conversation_id,
                    turn_id: "turn-1".to_string(),
                    item: CoreTurnItem::SubAgentActivity(SubAgentActivityItem {
                        id: "activity-1".to_string(),
                        kind: SubAgentActivityKind::Interrupted,
                        agent_thread_id: child_thread_id,
                        agent_path: AgentPath::try_from("/root/worker")
                            .expect("agent path should parse"),
                    }),
                    started_at_ms: Some(42),
                    completed_at_ms: 42,
                }),
            },
            conversation_id,
            conversation,
            thread_manager,
            outgoing,
            new_thread_state(),
            thread_watch_manager.clone(),
            Arc::new(tokio::sync::Semaphore::new(/*permits*/ 1)),
            "test-provider".to_string(),
        )
        .await;

        assert_eq!(
            thread_watch_manager
                .loaded_status_for_thread(&child_thread_id_string)
                .await,
            ThreadStatus::NotLoaded
        );
        assert_eq!(thread_watch_manager.running_turn_count().await, 0);
        let message = recv_broadcast_notification(&mut rx).await?;
        let ServerNotification::ItemCompleted(payload) = message else {
            bail!("unexpected message: {message:?}");
        };
        assert_eq!(
            payload,
            ItemCompletedNotification {
                item: ThreadItem::SubAgentActivity {
                    id: "activity-1".to_string(),
                    kind: codex_cli_protocol::SubAgentActivityKind::Interrupted,
                    agent_thread_id: child_thread_id_string,
                    agent_path: "/root/worker".to_string(),
                },
                thread_id: conversation_id.to_string(),
                turn_id: "turn-1".to_string(),
                completed_at_ms: 42,
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn canonical_dynamic_tool_start_emits_item_and_requests_client() -> Result<()> {
        let codex_home = TempDir::new()?;
        let config = load_default_config_for_test(&codex_home).await;
        let thread_manager = Arc::new(
            codex_core::test_support::thread_manager_with_models_provider_and_home(
                CodexAuth::create_dummy_chatgpt_auth_for_testing(),
                config.model_provider.clone(),
                config.codex_home.to_path_buf(),
                Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
            ),
        );
        let codex_core::NewThread {
            thread_id: conversation_id,
            thread: conversation,
            ..
        } = thread_manager
            .start_thread(codex_core::StartThreadOptions::new(config))
            .await?;
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing,
            vec![ConnectionId(1)],
            conversation_id,
        );

        apply_bespoke_event_handling(
            Event {
                id: "turn-1".to_string(),
                msg: EventMsg::ItemStarted(ItemStartedEvent {
                    thread_id: conversation_id,
                    turn_id: "turn-1".to_string(),
                    item: CoreTurnItem::DynamicToolCall(DynamicToolCallItem {
                        id: "dynamic-1".to_string(),
                        namespace: Some("apps".to_string()),
                        tool: "lookup".to_string(),
                        arguments: json!({"id": "123"}),
                        status: CoreDynamicToolCallStatus::InProgress,
                        content_items: None,
                        success: None,
                        error: None,
                        duration: None,
                    }),
                    started_at_ms: 42,
                }),
            },
            conversation_id,
            conversation,
            thread_manager,
            outgoing,
            new_thread_state(),
            ThreadWatchManager::new(),
            Arc::new(tokio::sync::Semaphore::new(/*permits*/ 1)),
            "test-provider".to_string(),
        )
        .await;

        let item_started = recv_broadcast_notification(&mut rx).await?;
        let ServerNotification::ItemStarted(payload) = item_started else {
            bail!("unexpected message: {item_started:?}");
        };
        assert_eq!(payload.item.id(), "dynamic-1");

        let request = recv_broadcast_message(&mut rx).await?;
        let OutgoingMessage::Request(ServerRequest::DynamicToolCall { params, .. }) = request
        else {
            bail!("unexpected message: {request:?}");
        };
        assert_eq!(
            params,
            DynamicToolCallParams {
                thread_id: conversation_id.to_string(),
                turn_id: "turn-1".to_string(),
                call_id: "dynamic-1".to_string(),
                namespace: Some("apps".to_string()),
                tool: "lookup".to_string(),
                arguments: json!({"id": "123"}),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_handle_turn_complete_emits_completed_without_error() -> Result<()> {
        let conversation_id = ThreadId::new();
        let event_turn_id = "complete1".to_string();
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing,
            vec![ConnectionId(1)],
            ThreadId::new(),
        );
        let thread_state = new_thread_state();
        let event = turn_complete_event(&event_turn_id);
        {
            let mut state = thread_state.lock().await;
            state.track_current_turn_event(
                &event_turn_id,
                &EventMsg::TurnStarted(codex_protocol::protocol::TurnStartedEvent {
                    turn_id: event_turn_id.clone(),
                    trace_id: None,
                    started_at: Some(42),
                    model_context_window: None,
                }),
            );
            state.track_current_turn_event(
                &event_turn_id,
                &EventMsg::ItemCompleted(ItemCompletedEvent {
                    thread_id: conversation_id,
                    turn_id: event_turn_id.clone(),
                    item: CoreTurnItem::AgentMessage(CoreAgentMessageItem {
                        id: "msg-1".to_string(),
                        content: vec![
                            CoreAgentMessageContent::Text {
                                text: "complete ".to_string(),
                            },
                            CoreAgentMessageContent::Text {
                                text: "response".to_string(),
                            },
                        ],
                        phase: None,
                        memory_citation: None,
                    }),
                    started_at_ms: Some(0),
                    completed_at_ms: 0,
                }),
            );
            state.track_current_turn_event(
                &event_turn_id,
                &EventMsg::ItemCompleted(ItemCompletedEvent {
                    thread_id: conversation_id,
                    turn_id: event_turn_id.clone(),
                    item: CoreTurnItem::AgentMessage(CoreAgentMessageItem {
                        id: "msg-2".to_string(),
                        content: vec![CoreAgentMessageContent::Text {
                            text: "  ".to_string(),
                        }],
                        phase: None,
                        memory_citation: None,
                    }),
                    started_at_ms: Some(0),
                    completed_at_ms: 0,
                }),
            );
            state.track_current_turn_event(&event_turn_id, &EventMsg::TurnComplete(event.clone()));
        }

        handle_turn_complete(
            conversation_id,
            event_turn_id.clone(),
            event,
            &outgoing,
            &thread_state,
        )
        .await;

        let msg = recv_broadcast_notification(&mut rx).await?;
        match msg {
            ServerNotification::TurnCompleted(n) => {
                assert_eq!(n.turn.id, event_turn_id);
                assert_eq!(n.turn.status, TurnStatus::Completed);
                assert_eq!(n.turn.items_view, TurnItemsView::Summary);
                assert!(matches!(
                    &n.turn.items[..],
                    [ThreadItem::AgentMessage { id, text, .. }]
                        if id == "msg-1" && text == "complete response"
                ));
                assert_eq!(n.turn.error, None);
                assert_eq!(n.turn.started_at, Some(42));
                assert_eq!(n.turn.completed_at, Some(TEST_TURN_COMPLETED_AT));
                assert_eq!(n.turn.duration_ms, Some(TEST_TURN_DURATION_MS));
            }
            other => bail!("unexpected message: {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "no extra messages expected");
        Ok(())
    }

    #[tokio::test]
    async fn test_handle_turn_interrupted_emits_interrupted_without_error() -> Result<()> {
        let conversation_id = ThreadId::new();
        let event_turn_id = "interrupt1".to_string();
        let thread_state = new_thread_state();
        handle_error(
            conversation_id,
            TurnError {
                message: "oops".to_string(),
                codex_error_info: None,
                additional_details: None,
            },
            &thread_state,
        )
        .await;
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing,
            vec![ConnectionId(1)],
            ThreadId::new(),
        );

        handle_turn_interrupted(
            conversation_id,
            event_turn_id.clone(),
            turn_aborted_event(&event_turn_id),
            &outgoing,
            &thread_state,
        )
        .await;

        let msg = recv_broadcast_notification(&mut rx).await?;
        match msg {
            ServerNotification::TurnCompleted(n) => {
                assert_eq!(n.turn.id, event_turn_id);
                assert_eq!(n.turn.status, TurnStatus::Interrupted);
                assert_eq!(n.turn.error, None);
                assert_eq!(n.turn.completed_at, Some(TEST_TURN_COMPLETED_AT));
                assert_eq!(n.turn.duration_ms, Some(TEST_TURN_DURATION_MS));
            }
            other => bail!("unexpected message: {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "no extra messages expected");
        Ok(())
    }

    #[tokio::test]
    async fn test_handle_turn_complete_emits_failed_with_error() -> Result<()> {
        let conversation_id = ThreadId::new();
        let event_turn_id = "complete_err1".to_string();
        let thread_state = new_thread_state();
        handle_error(
            conversation_id,
            TurnError {
                message: "bad".to_string(),
                codex_error_info: Some(V2CodexErrorInfo::Other),
                additional_details: None,
            },
            &thread_state,
        )
        .await;
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing,
            vec![ConnectionId(1)],
            ThreadId::new(),
        );

        handle_turn_complete(
            conversation_id,
            event_turn_id.clone(),
            turn_complete_event(&event_turn_id),
            &outgoing,
            &thread_state,
        )
        .await;

        let msg = recv_broadcast_notification(&mut rx).await?;
        match msg {
            ServerNotification::TurnCompleted(n) => {
                assert_eq!(n.turn.id, event_turn_id);
                assert_eq!(n.turn.status, TurnStatus::Failed);
                assert_eq!(
                    n.turn.error,
                    Some(TurnError {
                        message: "bad".to_string(),
                        codex_error_info: Some(V2CodexErrorInfo::Other),
                        additional_details: None,
                    })
                );
                assert_eq!(n.turn.completed_at, Some(TEST_TURN_COMPLETED_AT));
                assert_eq!(n.turn.duration_ms, Some(TEST_TURN_DURATION_MS));
            }
            other => bail!("unexpected message: {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "no extra messages expected");
        Ok(())
    }

    #[tokio::test]
    async fn test_handle_turn_plan_update_emits_notification_for_v2() -> Result<()> {
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing,
            vec![ConnectionId(1)],
            ThreadId::new(),
        );
        let update = UpdatePlanArgs {
            explanation: Some("need plan".to_string()),
            plan: vec![
                PlanItemArg {
                    step: "first".to_string(),
                    status: StepStatus::Pending,
                },
                PlanItemArg {
                    step: "second".to_string(),
                    status: StepStatus::Completed,
                },
            ],
        };

        let conversation_id = ThreadId::new();

        handle_turn_plan_update(conversation_id, "turn-123", update, &outgoing).await;

        let msg = recv_broadcast_notification(&mut rx).await?;
        match msg {
            ServerNotification::TurnPlanUpdated(n) => {
                assert_eq!(n.thread_id, conversation_id.to_string());
                assert_eq!(n.turn_id, "turn-123");
                assert_eq!(n.explanation.as_deref(), Some("need plan"));
                assert_eq!(n.plan.len(), 2);
                assert_eq!(n.plan[0].step, "first");
                assert_eq!(n.plan[0].status, TurnPlanStepStatus::Pending);
                assert_eq!(n.plan[1].step, "second");
                assert_eq!(n.plan[1].status, TurnPlanStepStatus::Completed);
            }
            other => bail!("unexpected message: {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "no extra messages expected");
        Ok(())
    }

    #[tokio::test]
    async fn test_handle_token_count_event_emits_usage_and_rate_limits() -> Result<()> {
        let conversation_id = ThreadId::new();
        let turn_id = "turn-123".to_string();
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing,
            vec![ConnectionId(1)],
            ThreadId::new(),
        );

        let info = TokenUsageInfo {
            total_token_usage: TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 25,
                cache_write_input_tokens: 0,
                output_tokens: 50,
                reasoning_output_tokens: 9,
                total_tokens: 200,
                codex_rollout_budget_units: None,
            },
            last_token_usage: TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 5,
                cache_write_input_tokens: 0,
                output_tokens: 7,
                reasoning_output_tokens: 1,
                total_tokens: 23,
                codex_rollout_budget_units: None,
            },
            model_context_window: Some(4096),
        };
        let rate_limits = RateLimitSnapshot {
            limit_id: Some("codex".to_string()),
            limit_name: None,
            primary: Some(RateLimitWindow {
                used_percent: 42.5,
                window_minutes: Some(15),
                resets_at: Some(1700000000),
            }),
            secondary: None,
            credits: Some(CreditsSnapshot {
                has_credits: true,
                unlimited: false,
                balance: Some("5".to_string()),
            }),
            individual_limit: None,
            spend_control_reached: None,
            plan_type: None,
            rate_limit_reached_type: None,
        };

        handle_token_count_event(
            conversation_id,
            turn_id.clone(),
            TokenCountEvent {
                info: Some(info),
                rate_limits: Some(rate_limits),
            },
            &outgoing,
        )
        .await;

        let first = recv_broadcast_notification(&mut rx).await?;
        match first {
            ServerNotification::ThreadTokenUsageUpdated(payload) => {
                assert_eq!(payload.thread_id, conversation_id.to_string());
                assert_eq!(payload.turn_id, turn_id);
                let usage = payload.token_usage;
                assert_eq!(usage.total.total_tokens, 200);
                assert_eq!(usage.total.cached_input_tokens, 25);
                assert_eq!(usage.last.output_tokens, 7);
                assert_eq!(usage.model_context_window, Some(4096));
            }
            other => bail!("unexpected notification: {other:?}"),
        }

        let second = recv_broadcast_notification(&mut rx).await?;
        match second {
            ServerNotification::AccountRateLimitsUpdated(payload) => {
                assert_eq!(payload.rate_limits.limit_id.as_deref(), Some("codex"));
                assert_eq!(payload.rate_limits.limit_name, None);
                assert!(payload.rate_limits.primary.is_some());
                assert!(payload.rate_limits.credits.is_some());
            }
            other => bail!("unexpected notification: {other:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_handle_token_count_event_without_usage_info() -> Result<()> {
        let conversation_id = ThreadId::new();
        let turn_id = "turn-456".to_string();
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing,
            vec![ConnectionId(1)],
            ThreadId::new(),
        );

        handle_token_count_event(
            conversation_id,
            turn_id.clone(),
            TokenCountEvent {
                info: None,
                rate_limits: None,
            },
            &outgoing,
        )
        .await;

        assert!(
            rx.try_recv().is_err(),
            "no notifications should be emitted when token usage info is absent"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_handle_turn_complete_emits_error_multiple_turns() -> Result<()> {
        // Conversation A will have two turns; Conversation B will have one turn.
        let conversation_a = ThreadId::new();
        let conversation_b = ThreadId::new();
        let thread_state = new_thread_state();

        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing,
            vec![ConnectionId(1)],
            ThreadId::new(),
        );

        // Turn 1 on conversation A
        let a_turn1 = "a_turn1".to_string();
        handle_error(
            conversation_a,
            TurnError {
                message: "a1".to_string(),
                codex_error_info: Some(V2CodexErrorInfo::BadRequest),
                additional_details: None,
            },
            &thread_state,
        )
        .await;
        handle_turn_complete(
            conversation_a,
            a_turn1.clone(),
            turn_complete_event(&a_turn1),
            &outgoing,
            &thread_state,
        )
        .await;

        // Turn 1 on conversation B
        let b_turn1 = "b_turn1".to_string();
        handle_error(
            conversation_b,
            TurnError {
                message: "b1".to_string(),
                codex_error_info: None,
                additional_details: None,
            },
            &thread_state,
        )
        .await;
        handle_turn_complete(
            conversation_b,
            b_turn1.clone(),
            turn_complete_event(&b_turn1),
            &outgoing,
            &thread_state,
        )
        .await;

        // Turn 2 on conversation A
        let a_turn2 = "a_turn2".to_string();
        handle_turn_complete(
            conversation_a,
            a_turn2.clone(),
            turn_complete_event(&a_turn2),
            &outgoing,
            &thread_state,
        )
        .await;

        // Verify: A turn 1
        let msg = recv_broadcast_notification(&mut rx).await?;
        match msg {
            ServerNotification::TurnCompleted(n) => {
                assert_eq!(n.turn.id, a_turn1);
                assert_eq!(n.turn.status, TurnStatus::Failed);
                assert_eq!(
                    n.turn.error,
                    Some(TurnError {
                        message: "a1".to_string(),
                        codex_error_info: Some(V2CodexErrorInfo::BadRequest),
                        additional_details: None,
                    })
                );
            }
            other => bail!("unexpected message: {other:?}"),
        }

        // Verify: B turn 1
        let msg = recv_broadcast_notification(&mut rx).await?;
        match msg {
            ServerNotification::TurnCompleted(n) => {
                assert_eq!(n.turn.id, b_turn1);
                assert_eq!(n.turn.status, TurnStatus::Failed);
                assert_eq!(
                    n.turn.error,
                    Some(TurnError {
                        message: "b1".to_string(),
                        codex_error_info: None,
                        additional_details: None,
                    })
                );
            }
            other => bail!("unexpected message: {other:?}"),
        }

        // Verify: A turn 2
        let msg = recv_broadcast_notification(&mut rx).await?;
        match msg {
            ServerNotification::TurnCompleted(n) => {
                assert_eq!(n.turn.id, a_turn2);
                assert_eq!(n.turn.status, TurnStatus::Completed);
                assert_eq!(n.turn.error, None);
            }
            other => bail!("unexpected message: {other:?}"),
        }

        assert!(rx.try_recv().is_err(), "no extra messages expected");
        Ok(())
    }

    #[tokio::test]
    async fn test_handle_turn_diff_emits_v2_notification() -> Result<()> {
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing,
            vec![ConnectionId(1)],
            ThreadId::new(),
        );
        let unified_diff = "--- a\n+++ b\n".to_string();
        let conversation_id = ThreadId::new();

        handle_turn_diff(
            conversation_id,
            "turn-1",
            TurnDiffEvent {
                unified_diff: unified_diff.clone(),
            },
            &outgoing,
        )
        .await;

        let msg = recv_broadcast_notification(&mut rx).await?;
        match msg {
            ServerNotification::TurnDiffUpdated(notification) => {
                assert_eq!(notification.thread_id, conversation_id.to_string());
                assert_eq!(notification.turn_id, "turn-1");
                assert_eq!(notification.diff, unified_diff);
            }
            other => bail!("unexpected message: {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "no extra messages expected");
        Ok(())
    }
}
