//! App-server event stream handling for the TUI app.

use super::App;
use super::runtime_event_targets::ServerNotificationThreadTarget;
use super::runtime_event_targets::server_notification_thread_target;
use super::runtime_event_targets::server_request_thread_id;
use crate::app_event::AppEvent;
use crate::runtime_session::CliRuntimeSession;
use crate::runtime_session::status_account_display_from_auth_mode;
use codex_cli_protocol::AuthMode;
use codex_cli_protocol::RateLimitReachedType;
use codex_cli_protocol::ServerNotification;
use codex_cli_protocol::ServerRequest;
use codex_cli_runtime_client::CliRuntimeEvent;

impl App {
    pub(super) async fn handle_cli_runtime_event(
        &mut self,
        cli_runtime_client: &CliRuntimeSession,
        event: CliRuntimeEvent,
    ) {
        match event {
            CliRuntimeEvent::Lagged { skipped } => {
                tracing::warn!(
                    skipped,
                    "cli-runtime event consumer lagged; dropping ignored events"
                );
            }
            CliRuntimeEvent::ServerNotification(notification) => {
                self.handle_server_notification_event(cli_runtime_client, *notification)
                    .await;
            }
            CliRuntimeEvent::ServerRequest(request) => {
                self.handle_server_request_event(cli_runtime_client, *request)
                    .await;
            }
            CliRuntimeEvent::Disconnected { message } => {
                tracing::warn!("cli-runtime event stream disconnected: {message}");
                self.chat_widget.add_error_message(message.clone());
                self.app_event_tx.send(AppEvent::FatalExitRequest(message));
            }
        }
    }

    async fn handle_server_notification_event(
        &mut self,
        _cli_runtime_client: &CliRuntimeSession,
        notification: ServerNotification,
    ) {
        match &notification {
            ServerNotification::ServerRequestResolved(notification) => {
                if let Some(request) = self
                    .pending_runtime_requests
                    .resolve_notification(&notification.request_id)
                {
                    self.chat_widget.dismiss_cli_runtime_request(&request);
                }
            }
            ServerNotification::AccountRateLimitsUpdated(notification) => {
                if matches!(
                    notification.rate_limits.rate_limit_reached_type,
                    Some(
                        RateLimitReachedType::WorkspaceOwnerCreditsDepleted
                            | RateLimitReachedType::WorkspaceMemberCreditsDepleted
                            | RateLimitReachedType::WorkspaceOwnerUsageLimitReached
                            | RateLimitReachedType::WorkspaceMemberUsageLimitReached
                    )
                ) || notification.rate_limits.spend_control_reached == Some(true)
                {
                    self.rate_limit_hard_stop_generation =
                        self.rate_limit_hard_stop_generation.wrapping_add(1);
                }
                self.chat_widget
                    .on_rolling_rate_limit_snapshot(notification.rate_limits.clone());
                return;
            }
            ServerNotification::AccountUpdated(notification) => {
                let has_codex_backend_auth = matches!(
                    notification.auth_mode,
                    Some(
                        AuthMode::Chatgpt
                            | AuthMode::ChatgptAuthTokens
                            | AuthMode::AgentIdentity
                            | AuthMode::PersonalAccessToken
                    )
                );
                self.chat_widget.update_account_state(
                    status_account_display_from_auth_mode(
                        notification.auth_mode,
                        notification.plan_type,
                    ),
                    notification.plan_type,
                    notification
                        .auth_mode
                        .is_some_and(AuthMode::has_chatgpt_account),
                    has_codex_backend_auth,
                );
                return;
            }
            _ => {}
        }

        match server_notification_thread_target(&notification) {
            ServerNotificationThreadTarget::Thread(thread_id) => {
                let result = if self.primary_thread_id == Some(thread_id)
                    || self.primary_thread_id.is_none()
                {
                    self.enqueue_primary_thread_notification(notification).await
                } else {
                    self.enqueue_thread_notification(thread_id, notification)
                        .await
                };

                if let Err(err) = result {
                    tracing::warn!("failed to enqueue cli-runtime notification: {err}");
                }
                return;
            }
            ServerNotificationThreadTarget::InvalidThreadId(thread_id) => {
                tracing::warn!(
                    thread_id,
                    "ignoring cli-runtime notification with invalid thread_id"
                );
                return;
            }
            ServerNotificationThreadTarget::Global => {}
        }

        self.chat_widget
            .handle_server_notification(notification, /*replay_kind*/ None);
    }

    async fn handle_server_request_event(
        &mut self,
        cli_runtime_client: &CliRuntimeSession,
        request: ServerRequest,
    ) {
        let thread_id = server_request_thread_id(&request);
        if thread_id.is_some_and(|thread_id| self.abandoned_side_threads.contains(&thread_id)) {
            if let Err(err) = self
                .reject_cli_runtime_request(
                    cli_runtime_client,
                    request.id().clone(),
                    "side conversation was closed".to_string(),
                )
                .await
            {
                tracing::warn!("{err}");
            }
            return;
        }

        if let Some(unsupported) = self.pending_runtime_requests.note_server_request(&request) {
            tracing::warn!(
                request_id = ?unsupported.request_id,
                message = unsupported.message,
                "rejecting unsupported cli-runtime request"
            );
            self.chat_widget
                .add_error_message(unsupported.message.clone());
            if let Err(err) = self
                .reject_cli_runtime_request(
                    cli_runtime_client,
                    unsupported.request_id,
                    unsupported.message,
                )
                .await
            {
                tracing::warn!("{err}");
            }
            return;
        }

        let Some(thread_id) = thread_id else {
            tracing::warn!("ignoring threadless cli-runtime request");
            return;
        };

        let result =
            if self.primary_thread_id == Some(thread_id) || self.primary_thread_id.is_none() {
                self.enqueue_primary_thread_request(request).await
            } else {
                self.enqueue_thread_request(thread_id, request).await
            };
        if let Err(err) = result {
            tracing::warn!("failed to enqueue cli-runtime request: {err}");
        }
    }
}
