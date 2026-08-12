use std::collections::HashMap;
use std::collections::VecDeque;

use super::App;
use crate::app_command::AppCommand;
use crate::runtime_session::CliRuntimeSession;
use codex_cli_protocol::JSONRPCErrorError;
use codex_cli_protocol::RequestId as CliRuntimeRequestId;
use codex_cli_protocol::ServerRequest;

impl App {
    pub(super) async fn reject_cli_runtime_request(
        &self,
        cli_runtime_client: &CliRuntimeSession,
        request_id: CliRuntimeRequestId,
        reason: String,
    ) -> std::result::Result<(), String> {
        cli_runtime_client
            .reject_server_request(
                request_id,
                JSONRPCErrorError {
                    code: -32000,
                    message: reason,
                    data: None,
                },
            )
            .await
            .map_err(|err| format!("failed to reject cli-runtime request: {err}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CliRuntimeRequestResolution {
    pub(super) request_id: CliRuntimeRequestId,
    pub(super) result: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UnsupportedCliRuntimeRequest {
    pub(super) request_id: CliRuntimeRequestId,
    pub(super) message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedCliRuntimeRequest {
    UserInput { call_id: String },
}

#[derive(Debug, Default)]
pub(super) struct PendingCliRuntimeRequests {
    user_inputs: HashMap<String, VecDeque<PendingUserInputRequest>>,
}

impl PendingCliRuntimeRequests {
    pub(super) fn clear(&mut self) {
        self.user_inputs.clear();
    }

    pub(super) fn note_server_request(
        &mut self,
        request: &ServerRequest,
    ) -> Option<UnsupportedCliRuntimeRequest> {
        match request {
            ServerRequest::ToolRequestUserInput { request_id, params } => {
                self.user_inputs
                    .entry(params.turn_id.clone())
                    .or_default()
                    .push_back(PendingUserInputRequest {
                        item_id: params.item_id.clone(),
                        request_id: request_id.clone(),
                    });
                None
            }
            ServerRequest::DynamicToolCall { request_id, .. } => {
                Some(UnsupportedCliRuntimeRequest {
                    request_id: request_id.clone(),
                    message: "Dynamic tool calls are not available in TUI yet.".to_string(),
                })
            }
            ServerRequest::ChatgptAuthTokensRefresh { .. } => None,
            ServerRequest::AttestationGenerate { request_id, .. } => {
                Some(UnsupportedCliRuntimeRequest {
                    request_id: request_id.clone(),
                    message: "Attestation generation is not available in TUI.".to_string(),
                })
            }
            ServerRequest::CurrentTimeRead { request_id, .. } => {
                Some(UnsupportedCliRuntimeRequest {
                    request_id: request_id.clone(),
                    message: "External current time is not available in TUI.".to_string(),
                })
            }
        }
    }

    pub(super) fn take_resolution<T>(
        &mut self,
        op: T,
    ) -> Result<Option<CliRuntimeRequestResolution>, String>
    where
        T: Into<AppCommand>,
    {
        let op: AppCommand = op.into();
        let resolution = match &op {
            AppCommand::UserInputAnswer { id, response } => self
                .pop_user_input_request_for_turn(id)
                .map(|pending| {
                    Ok::<CliRuntimeRequestResolution, String>(CliRuntimeRequestResolution {
                        request_id: pending.request_id,
                        result: serde_json::to_value(response).map_err(|err| {
                            format!("failed to serialize request_user_input response: {err}")
                        })?,
                    })
                })
                .transpose()?,
            _ => None,
        };
        Ok(resolution)
    }

    pub(super) fn resolve_notification(
        &mut self,
        request_id: &CliRuntimeRequestId,
    ) -> Option<ResolvedCliRuntimeRequest> {
        if let Some(pending) = self.remove_user_input_request(request_id) {
            return Some(ResolvedCliRuntimeRequest::UserInput {
                call_id: pending.item_id,
            });
        }

        None
    }

    pub(super) fn contains_server_request(&self, request: &ServerRequest) -> bool {
        match request {
            ServerRequest::ToolRequestUserInput { request_id, .. } => {
                self.user_inputs.values().any(|queue| {
                    queue
                        .iter()
                        .any(|pending| &pending.request_id == request_id)
                })
            }
            ServerRequest::DynamicToolCall { .. }
            | ServerRequest::ChatgptAuthTokensRefresh { .. }
            | ServerRequest::AttestationGenerate { .. }
            | ServerRequest::CurrentTimeRead { .. } => true,
        }
    }

    fn pop_user_input_request_for_turn(
        &mut self,
        turn_id: &str,
    ) -> Option<PendingUserInputRequest> {
        let pending = self
            .user_inputs
            .get_mut(turn_id)
            .and_then(VecDeque::pop_front);
        if self
            .user_inputs
            .get(turn_id)
            .is_some_and(VecDeque::is_empty)
        {
            self.user_inputs.remove(turn_id);
        }
        pending
    }

    fn remove_user_input_request(
        &mut self,
        request_id: &CliRuntimeRequestId,
    ) -> Option<PendingUserInputRequest> {
        let (turn_id, index) = self.user_inputs.iter().find_map(|(turn_id, queue)| {
            queue
                .iter()
                .position(|pending| &pending.request_id == request_id)
                .map(|index| (turn_id.clone(), index))
        })?;
        let queue = self.user_inputs.get_mut(&turn_id)?;
        let removed = queue.remove(index);
        if queue.is_empty() {
            self.user_inputs.remove(&turn_id);
        }
        removed
    }
}

#[derive(Debug)]
struct PendingUserInputRequest {
    item_id: String,
    request_id: CliRuntimeRequestId,
}

#[cfg(test)]
mod tests {
    use super::PendingCliRuntimeRequests;
    use super::ResolvedCliRuntimeRequest;
    use super::UnsupportedCliRuntimeRequest;
    use crate::app_command::AppCommand as Op;
    use codex_cli_protocol::RequestId as CliRuntimeRequestId;
    use codex_cli_protocol::ServerRequest;
    use codex_cli_protocol::ToolRequestUserInputParams;
    use codex_cli_protocol::ToolRequestUserInputResponse;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn rejects_dynamic_tool_calls_as_unsupported() {
        let mut pending = PendingCliRuntimeRequests::default();
        let unsupported = pending
            .note_server_request(&ServerRequest::DynamicToolCall {
                request_id: CliRuntimeRequestId::Integer(99),
                params: codex_cli_protocol::DynamicToolCallParams {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    call_id: "tool-1".to_string(),
                    namespace: None,
                    tool: "tool".to_string(),
                    arguments: json!({}),
                },
            })
            .expect("dynamic tool calls should be rejected");

        assert_eq!(unsupported.request_id, CliRuntimeRequestId::Integer(99));
        assert_eq!(
            unsupported.message,
            "Dynamic tool calls are not available in TUI yet."
        );
    }

    #[test]
    fn does_not_mark_chatgpt_auth_refresh_as_unsupported() {
        let mut pending = PendingCliRuntimeRequests::default();

        assert_eq!(
            pending.note_server_request(&ServerRequest::ChatgptAuthTokensRefresh {
                request_id: CliRuntimeRequestId::Integer(100),
                params: codex_cli_protocol::ChatgptAuthTokensRefreshParams {
                    reason: codex_cli_protocol::ChatgptAuthTokensRefreshReason::Unauthorized,
                    previous_account_id: Some("workspace-1".to_string()),
                },
            }),
            None
        );
    }

    #[test]
    fn resolve_notification_returns_resolved_user_input_item_id() {
        let mut pending = PendingCliRuntimeRequests::default();
        pending.note_server_request(&ServerRequest::ToolRequestUserInput {
            request_id: CliRuntimeRequestId::Integer(8),
            params: ToolRequestUserInputParams {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "tool-1".to_string(),
                questions: Vec::new(),
                is_blocking: true,
                auto_resolution_ms: None,
            },
        });

        assert_eq!(
            pending.resolve_notification(&CliRuntimeRequestId::Integer(8)),
            Some(ResolvedCliRuntimeRequest::UserInput {
                call_id: "tool-1".to_string(),
            })
        );
    }

    #[test]
    fn same_turn_user_input_answers_resolve_runtime_requests_fifo() {
        let mut pending = PendingCliRuntimeRequests::default();
        for (request_id, item_id) in [(8, "tool-1"), (9, "tool-2")] {
            pending.note_server_request(&ServerRequest::ToolRequestUserInput {
                request_id: CliRuntimeRequestId::Integer(request_id),
                params: ToolRequestUserInputParams {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: item_id.to_string(),
                    questions: Vec::new(),
                    is_blocking: true,
                    auto_resolution_ms: None,
                },
            });
        }

        let response = ToolRequestUserInputResponse {
            answers: HashMap::new(),
        };
        let first_response = pending
            .take_resolution(&Op::UserInputAnswer {
                id: "turn-1".to_string(),
                response: response.clone(),
            })
            .expect("user input response should serialize")
            .expect("first user input request should be pending");
        let second_response = pending
            .take_resolution(&Op::UserInputAnswer {
                id: "turn-1".to_string(),
                response,
            })
            .expect("user input response should serialize")
            .expect("second user input request should be pending");

        assert_eq!(first_response.request_id, CliRuntimeRequestId::Integer(8));
        assert_eq!(second_response.request_id, CliRuntimeRequestId::Integer(9));
    }
}
