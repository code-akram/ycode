//! Background cli-runtime requests launched by the TUI app.
//!
//! This module owns fire-and-forget fetch/write helpers for skills, rate
//! limits, and add-credit nudges. Results are routed back through `AppEvent` so
//! the main event loop remains single-threaded.

use super::*;
use codex_cli_protocol::ConsumeAccountRateLimitResetCreditParams;
use codex_cli_protocol::ConsumeAccountRateLimitResetCreditResponse;

use codex_cli_protocol::RequestId;

const TOKEN_ACTIVITY_FETCH_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(/*secs*/ 15);
const RATE_LIMIT_RESET_REQUEST_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(/*secs*/ 15);

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
