use std::sync::Arc;
use std::sync::Weak;

use codex_cli_protocol::AttestationGenerateParams;
use codex_cli_protocol::AttestationGenerateResponse;
use codex_cli_protocol::ServerRequestPayload;
use codex_core::AttestationContext;
use codex_core::AttestationProvider;
use codex_core::GenerateAttestationFuture;
use http::HeaderValue;
use serde::Serialize;
use tokio::time::Duration;
use tokio::time::timeout;
use tracing::warn;

use crate::outgoing_message::OutgoingMessageSender;
use crate::thread_state::ThreadStateManager;

const ATTESTATION_GENERATE_TIMEOUT: Duration = Duration::from_millis(100);

pub(crate) fn cli_runtime_attestation_provider(
    outgoing: Arc<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
) -> Arc<dyn AttestationProvider> {
    Arc::new(CliRuntimeAttestationProvider {
        outgoing: Arc::downgrade(&outgoing),
        thread_state_manager,
    })
}

struct CliRuntimeAttestationProvider {
    outgoing: Weak<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
}

impl std::fmt::Debug for CliRuntimeAttestationProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CliRuntimeAttestationProvider")
            .finish()
    }
}

impl AttestationProvider for CliRuntimeAttestationProvider {
    fn header_for_request(&self, context: AttestationContext) -> GenerateAttestationFuture<'_> {
        let Some(outgoing) = self.outgoing.upgrade() else {
            return Box::pin(async { None });
        };
        let thread_state_manager = self.thread_state_manager.clone();
        Box::pin(async move {
            request_attestation_header_value_with_timeout(
                outgoing,
                thread_state_manager,
                context.thread_id,
                ATTESTATION_GENERATE_TIMEOUT,
            )
            .await
            .and_then(|value| HeaderValue::from_bytes(value.as_bytes()).ok())
        })
    }
}

async fn request_attestation_header_value_with_timeout(
    outgoing: Arc<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
    thread_id: codex_protocol::ThreadId,
    timeout_duration: Duration,
) -> Option<String> {
    let connection_id = thread_state_manager
        .first_attestation_capable_connection_for_thread(thread_id)
        .await?;

    let connection_ids = [connection_id];
    let (request_id, rx) = outgoing
        .send_request_to_connections(
            Some(&connection_ids),
            ServerRequestPayload::AttestationGenerate(AttestationGenerateParams {}),
            /*thread_id*/ None,
        )
        .await;

    let result = match timeout(timeout_duration, rx).await {
        Ok(Ok(Ok(result))) => result,
        Ok(Ok(Err(err))) => {
            warn!(
                code = err.code,
                message = %err.message,
                "attestation generation request failed"
            );
            return cli_runtime_attestation_header_value(
                CliRuntimeAttestationStatus::RequestFailed,
                /*token*/ None,
            );
        }
        Ok(Err(err)) => {
            warn!("attestation generation request canceled: {err}");
            return cli_runtime_attestation_header_value(
                CliRuntimeAttestationStatus::RequestCanceled,
                /*token*/ None,
            );
        }
        Err(_) => {
            let _canceled = outgoing.cancel_request(&request_id).await;
            warn!(
                timeout_seconds = timeout_duration.as_secs(),
                "attestation generation request timed out"
            );
            return cli_runtime_attestation_header_value(
                CliRuntimeAttestationStatus::Timeout,
                /*token*/ None,
            );
        }
    };

    match serde_json::from_value::<AttestationGenerateResponse>(result) {
        Ok(response) => cli_runtime_attestation_header_value(
            CliRuntimeAttestationStatus::Ok,
            Some(&response.token),
        ),
        Err(err) => {
            warn!("failed to deserialize attestation generation response: {err}");
            cli_runtime_attestation_header_value(
                CliRuntimeAttestationStatus::MalformedResponse,
                /*token*/ None,
            )
        }
    }
}

#[derive(Clone, Copy)]
enum CliRuntimeAttestationStatus {
    Ok,
    Timeout,
    RequestFailed,
    RequestCanceled,
    MalformedResponse,
}

impl CliRuntimeAttestationStatus {
    const fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Timeout => 1,
            Self::RequestFailed => 2,
            Self::RequestCanceled => 3,
            Self::MalformedResponse => 4,
        }
    }
}

#[derive(Serialize)]
struct CliRuntimeAttestationEnvelope<'a> {
    v: u8,
    s: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    t: Option<&'a str>,
}

fn cli_runtime_attestation_header_value(
    status: CliRuntimeAttestationStatus,
    token: Option<&str>,
) -> Option<String> {
    serde_json::to_string(&CliRuntimeAttestationEnvelope {
        v: 1,
        s: status.code(),
        t: token,
    })
    .map_err(|err| warn!("failed to serialize cli-runtime attestation envelope: {err}"))
    .ok()
}

#[cfg(test)]
mod tests {
    use super::CliRuntimeAttestationStatus;
    use super::cli_runtime_attestation_header_value;
    use pretty_assertions::assert_eq;

    #[test]
    fn cli_runtime_attestation_header_value_wraps_opaque_client_payloads() {
        assert_eq!(
            cli_runtime_attestation_header_value(
                CliRuntimeAttestationStatus::Ok,
                Some("v1.opaque-client-payload"),
            ),
            Some(r#"{"v":1,"s":0,"t":"v1.opaque-client-payload"}"#.to_string())
        );
    }

    #[test]
    fn cli_runtime_attestation_header_value_reports_cli_runtime_failures() {
        assert_eq!(
            cli_runtime_attestation_header_value(
                CliRuntimeAttestationStatus::Timeout,
                /*token*/ None,
            ),
            Some(r#"{"v":1,"s":1}"#.to_string())
        );
        assert_eq!(
            cli_runtime_attestation_header_value(
                CliRuntimeAttestationStatus::RequestFailed,
                /*token*/ None,
            ),
            Some(r#"{"v":1,"s":2}"#.to_string())
        );
        assert_eq!(
            cli_runtime_attestation_header_value(
                CliRuntimeAttestationStatus::RequestCanceled,
                /*token*/ None,
            ),
            Some(r#"{"v":1,"s":3}"#.to_string())
        );
        assert_eq!(
            cli_runtime_attestation_header_value(
                CliRuntimeAttestationStatus::MalformedResponse,
                /*token*/ None
            ),
            Some(r#"{"v":1,"s":4}"#.to_string())
        );
    }
}
