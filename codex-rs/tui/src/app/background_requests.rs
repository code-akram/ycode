//! Background cli-runtime requests launched by the TUI app.
//!
//! This module owns fire-and-forget fetch/write helpers for skills, rate
//! limits, add-credit nudges, and feedback uploads. Results are routed back through `AppEvent` so
//! the main event loop remains single-threaded.

use super::*;
use codex_cli_protocol::ConsumeAccountRateLimitResetCreditParams;
use codex_cli_protocol::ConsumeAccountRateLimitResetCreditResponse;

use codex_cli_protocol::RequestId;

use codex_utils_absolute_path::AbsolutePathBuf;

const TOKEN_ACTIVITY_FETCH_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(/*secs*/ 15);
const RATE_LIMIT_RESET_REQUEST_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(/*secs*/ 15);
const WORKSPACE_HEADLINE_FETCH_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(/*millis*/ 2000);

impl App {
    /// Spawns a background task to fetch account rate limits and deliver the
    /// result as a `RateLimitsLoaded` event.
    ///
    /// The `origin` is forwarded to the completion handler so it can distinguish
    /// a startup prefetch (which updates cached snapshots and may surface a
    /// reset-credit notice) from a `/status`-triggered refresh (which must
    /// finalize the corresponding status card).
    pub(super) fn refresh_rate_limits(
        &mut self,
        cli_runtime: &CliRuntimeSession,
        origin: RateLimitRefreshOrigin,
    ) {
        let request_handle = cli_runtime.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        let hard_stop_generation = self.rate_limit_hard_stop_generation;
        tokio::spawn(async move {
            let request = fetch_account_rate_limits(request_handle);
            let result = match origin {
                RateLimitRefreshOrigin::ResetConsume { .. }
                | RateLimitRefreshOrigin::ResetPicker { .. } => {
                    tokio::time::timeout(RATE_LIMIT_RESET_REQUEST_TIMEOUT, request)
                        .await
                        .map_err(|_| "account/rateLimits/read timed out in TUI".to_string())
                        .and_then(|result| result.map_err(|err| err.to_string()))
                }
                RateLimitRefreshOrigin::StartupPrefetch { .. }
                | RateLimitRefreshOrigin::StatusCommand { .. }
                | RateLimitRefreshOrigin::UsageMenu { .. } => {
                    request.await.map_err(|err| err.to_string())
                }
            };
            app_event_tx.send(AppEvent::RateLimitsLoaded {
                origin,
                hard_stop_generation,
                result,
            });
        });
    }

    pub(super) fn refresh_token_activity(
        &mut self,
        cli_runtime: &CliRuntimeSession,
        request_id: u64,
    ) {
        let request_handle = cli_runtime.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                TOKEN_ACTIVITY_FETCH_TIMEOUT,
                fetch_account_token_activity(request_handle),
            )
            .await
            .map_err(|_| "account/usage/read timed out in TUI".to_string())
            .and_then(|result| result.map_err(|err| err.to_string()));
            app_event_tx.send(AppEvent::TokenActivityLoaded { request_id, result });
        });
    }

    pub(super) fn consume_rate_limit_reset_credit(
        &mut self,
        cli_runtime: &CliRuntimeSession,
        request_id: u64,
        idempotency_key: String,
        credit_id: Option<String>,
    ) {
        let request_handle = cli_runtime.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                RATE_LIMIT_RESET_REQUEST_TIMEOUT,
                consume_rate_limit_reset_credit_request(
                    request_handle,
                    idempotency_key.clone(),
                    credit_id.clone(),
                ),
            )
            .await
            .map_err(|_| "account/rateLimitResetCredit/consume timed out in TUI".to_string())
            .and_then(|result| result.map_err(|err| err.to_string()));
            app_event_tx.send(AppEvent::RateLimitResetCreditConsumed {
                request_id,
                idempotency_key,
                credit_id,
                result,
            });
        });
    }

    pub(super) fn refresh_status_line_workspace_headline(
        &mut self,
        cli_runtime: &CliRuntimeSession,
        request_id: u64,
    ) {
        let request_handle = cli_runtime.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                WORKSPACE_HEADLINE_FETCH_TIMEOUT,
                fetch_workspace_messages(request_handle),
            )
            .await
            .map_err(|_| "account/workspaceMessages/read timed out in TUI".to_string())
            .and_then(|result| {
                result
                    .map(crate::workspace_messages::workspace_headline_from_response)
                    .map_err(|err| err.to_string())
            });
            app_event_tx.send(AppEvent::StatusLineWorkspaceHeadlineUpdated { request_id, result });
        });
    }

    pub(super) fn send_add_credits_nudge_email(
        &mut self,
        cli_runtime: &CliRuntimeSession,
        credit_type: AddCreditsNudgeCreditType,
    ) {
        let request_handle = cli_runtime.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = send_add_credits_nudge_email(request_handle, credit_type)
                .await
                .map_err(|err| err.to_string());
            app_event_tx.send(AppEvent::AddCreditsNudgeEmailFinished { result });
        });
    }

    /// Starts the initial skills refresh without delaying the first interactive frame.
    ///
    /// Startup only needs skill metadata to populate skill mentions and the skills UI; the prompt can be
    /// rendered before that metadata arrives. The result is routed through the normal app event queue so
    /// the same response handler updates the chat widget and emits invalid `SKILL.md` warnings once the
    /// cli-runtime RPC finishes. User-initiated skills refreshes still use the blocking app command path so
    /// callers that explicitly asked for fresh skill state do not race ahead of their own refresh.
    pub(super) fn refresh_startup_skills(&mut self, cli_runtime: &CliRuntimeSession) {
        let request_handle = cli_runtime.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        let cwd = self.config.cwd.to_path_buf();
        tokio::spawn(async move {
            let result = fetch_skills_list(request_handle, cwd)
                .await
                .map_err(|err| format!("{err:#}"));
            app_event_tx.send(AppEvent::SkillsListLoaded { result });
        });
    }

    pub(super) fn submit_feedback(
        &mut self,
        cli_runtime: &CliRuntimeSession,
        category: FeedbackCategory,
        reason: Option<String>,
        turn_id: Option<String>,
        include_logs: bool,
    ) {
        let request_handle = cli_runtime.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        let origin_thread_id = self.chat_widget.thread_id();
        let rollout_path = if include_logs {
            self.chat_widget.rollout_path()
        } else {
            None
        };
        let params = build_feedback_upload_params(
            origin_thread_id,
            rollout_path,
            category,
            reason,
            turn_id,
            include_logs,
        );
        tokio::spawn(async move {
            let result = fetch_feedback_upload(request_handle, params)
                .await
                .map(|response| response.thread_id)
                .map_err(|err| err.to_string());
            app_event_tx.send(AppEvent::FeedbackSubmitted {
                origin_thread_id,
                category,
                include_logs,
                result,
            });
        });
    }

    pub(super) fn handle_feedback_thread_event(&mut self, event: FeedbackThreadEvent) {
        match event.result {
            Ok(thread_id) => {
                self.chat_widget
                    .add_to_history(crate::bottom_pane::feedback_success_cell(
                        event.category,
                        event.include_logs,
                        &thread_id,
                        event.feedback_audience,
                    ))
            }
            Err(err) => self
                .chat_widget
                .add_to_history(history_cell::new_error_event(format!(
                    "Failed to upload feedback: {err}"
                ))),
        }
    }

    pub(super) async fn enqueue_thread_feedback_event(
        &mut self,
        thread_id: ThreadId,
        event: FeedbackThreadEvent,
    ) {
        let (sender, store) = {
            let channel = self.ensure_thread_channel(thread_id);
            (channel.sender.clone(), Arc::clone(&channel.store))
        };

        let should_send = {
            let mut guard = store.lock().await;
            guard
                .buffer
                .push_back(ThreadBufferedEvent::FeedbackSubmission(event.clone()));
            if guard.buffer.len() > guard.capacity
                && let Some(removed) = guard.buffer.pop_front()
                && let ThreadBufferedEvent::Request(request) = &removed
            {
                guard
                    .pending_interactive_replay
                    .note_evicted_server_request(request.as_ref());
            }
            guard.active
        };

        if should_send {
            match sender.try_send(ThreadBufferedEvent::FeedbackSubmission(event)) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    tokio::spawn(async move {
                        if let Err(err) = sender.send(event).await {
                            tracing::warn!("thread {thread_id} event channel closed: {err}");
                        }
                    });
                }
                Err(TrySendError::Closed(_)) => {
                    tracing::warn!("thread {thread_id} event channel closed");
                }
            }
        }
    }

    pub(super) async fn handle_feedback_submitted(
        &mut self,
        origin_thread_id: Option<ThreadId>,
        category: FeedbackCategory,
        include_logs: bool,
        result: Result<String, String>,
    ) {
        let event = FeedbackThreadEvent {
            category,
            include_logs,
            feedback_audience: self.feedback_audience,
            result,
        };
        if let Some(thread_id) = origin_thread_id {
            self.enqueue_thread_feedback_event(thread_id, event).await;
        } else {
            self.handle_feedback_thread_event(event);
        }
    }
}

pub(super) async fn fetch_account_rate_limits(
    request_handle: CliRuntimeRequestHandle,
) -> Result<GetAccountRateLimitsResponse> {
    let request_id = RequestId::String(format!("account-rate-limits-{}", Uuid::new_v4()));
    request_handle
        .request_typed(ClientRequest::GetAccountRateLimits {
            request_id,
            params: None,
        })
        .await
        .wrap_err("account/rateLimits/read failed in TUI")
}

pub(super) async fn fetch_account_token_activity(
    request_handle: CliRuntimeRequestHandle,
) -> Result<codex_cli_protocol::GetAccountTokenUsageResponse> {
    let request_id = RequestId::String(format!("account-token-usage-{}", Uuid::new_v4()));
    request_handle
        .request_typed(ClientRequest::GetAccountTokenUsage {
            request_id,
            params: None,
        })
        .await
        .wrap_err("account/usage/read failed in TUI")
}

pub(super) async fn consume_rate_limit_reset_credit_request(
    request_handle: CliRuntimeRequestHandle,
    idempotency_key: String,
    credit_id: Option<String>,
) -> Result<ConsumeAccountRateLimitResetCreditResponse> {
    let request_id = RequestId::String(format!("consume-rate-limit-reset-{}", Uuid::new_v4()));
    request_handle
        .request_typed(ClientRequest::ConsumeAccountRateLimitResetCredit {
            request_id,
            params: ConsumeAccountRateLimitResetCreditParams {
                idempotency_key,
                credit_id,
            },
        })
        .await
        .wrap_err("account/rateLimitResetCredit/consume failed in TUI")
}

pub(super) async fn fetch_workspace_messages(
    request_handle: CliRuntimeRequestHandle,
) -> Result<codex_cli_protocol::GetWorkspaceMessagesResponse> {
    let request_id = RequestId::String(format!("workspace-messages-{}", Uuid::new_v4()));
    request_handle
        .request_typed(ClientRequest::GetWorkspaceMessages {
            request_id,
            params: None,
        })
        .await
        .wrap_err("account/workspaceMessages/read failed in TUI")
}

pub(super) async fn send_add_credits_nudge_email(
    request_handle: CliRuntimeRequestHandle,
    credit_type: AddCreditsNudgeCreditType,
) -> Result<codex_cli_protocol::AddCreditsNudgeEmailStatus> {
    let request_id = RequestId::String(format!("add-credits-nudge-{}", Uuid::new_v4()));
    let response: codex_cli_protocol::SendAddCreditsNudgeEmailResponse = request_handle
        .request_typed(ClientRequest::SendAddCreditsNudgeEmail {
            request_id,
            params: SendAddCreditsNudgeEmailParams { credit_type },
        })
        .await
        .wrap_err("account/sendAddCreditsNudgeEmail failed in TUI")?;

    Ok(response.status)
}

pub(super) async fn fetch_skills_list(
    request_handle: CliRuntimeRequestHandle,
    cwd: PathBuf,
) -> Result<SkillsListResponse> {
    let request_id = RequestId::String(format!("startup-skills-list-{}", Uuid::new_v4()));
    // Use the cloneable request handle so startup can issue this RPC from a background task without
    // extending a borrow of `CliRuntimeSession` across the first frame render.
    request_handle
        .request_typed(ClientRequest::SkillsList {
            request_id,
            params: SkillsListParams {
                cwds: vec![cwd],
                force_reload: true,
            },
        })
        .await
        .wrap_err("skills/list failed in TUI")
}

pub(super) fn build_feedback_upload_params(
    origin_thread_id: Option<ThreadId>,
    rollout_path: Option<PathBuf>,
    category: FeedbackCategory,
    reason: Option<String>,
    turn_id: Option<String>,
    include_logs: bool,
) -> FeedbackUploadParams {
    let extra_log_files = if include_logs {
        rollout_path.map(|rollout_path| vec![rollout_path])
    } else {
        None
    };
    let tags = turn_id.map(|turn_id| BTreeMap::from([(String::from("turn_id"), turn_id)]));
    FeedbackUploadParams {
        classification: crate::bottom_pane::feedback_classification(category).to_string(),
        reason,
        thread_id: origin_thread_id.map(|thread_id| thread_id.to_string()),
        include_logs,
        extra_log_files,
        tags,
    }
}

pub(super) async fn fetch_feedback_upload(
    request_handle: CliRuntimeRequestHandle,
    params: FeedbackUploadParams,
) -> Result<FeedbackUploadResponse> {
    let request_id = RequestId::String(format!("feedback-upload-{}", Uuid::new_v4()));
    request_handle
        .request_typed(ClientRequest::FeedbackUpload { request_id, params })
        .await
        .wrap_err("feedback/upload failed in TUI")
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    #[test]
    fn build_feedback_upload_params_includes_thread_id_and_rollout_path() {
        let thread_id = ThreadId::new();
        let rollout_path = PathBuf::from("/tmp/rollout.jsonl");

        let params = build_feedback_upload_params(
            Some(thread_id),
            Some(rollout_path.clone()),
            FeedbackCategory::SafetyCheck,
            Some("needs follow-up".to_string()),
            Some("turn-123".to_string()),
            /*include_logs*/ true,
        );

        assert_eq!(params.classification, "safety_check");
        assert_eq!(params.reason, Some("needs follow-up".to_string()));
        assert_eq!(params.thread_id, Some(thread_id.to_string()));
        assert_eq!(
            params
                .tags
                .as_ref()
                .and_then(|tags| tags.get("turn_id"))
                .map(String::as_str),
            Some("turn-123")
        );
        assert_eq!(params.include_logs, true);
        assert_eq!(params.extra_log_files, Some(vec![rollout_path]));
    }

    #[test]
    fn build_feedback_upload_params_omits_rollout_path_without_logs() {
        let params = build_feedback_upload_params(
            /*origin_thread_id*/ None,
            Some(PathBuf::from("/tmp/rollout.jsonl")),
            FeedbackCategory::GoodResult,
            /*reason*/ None,
            /*turn_id*/ None,
            /*include_logs*/ false,
        );

        assert_eq!(params.classification, "good_result");
        assert_eq!(params.reason, None);
        assert_eq!(params.thread_id, None);
        assert_eq!(params.tags, None);
        assert_eq!(params.include_logs, false);
        assert_eq!(params.extra_log_files, None);
    }
}
