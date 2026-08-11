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
    use codex_cli_protocol::AdditionalFileSystemPermissions;
    use codex_cli_protocol::AdditionalNetworkPermissions;
    use codex_cli_protocol::CommandExecutionApprovalDecision;
    use codex_cli_protocol::CommandExecutionRequestApprovalParams;
    use codex_cli_protocol::FileChangeApprovalDecision;
    use codex_cli_protocol::FileChangeRequestApprovalParams;
    use codex_cli_protocol::PermissionGrantScope;
    use codex_cli_protocol::PermissionsRequestApprovalParams;
    use codex_cli_protocol::PermissionsRequestApprovalResponse;
    use codex_cli_protocol::RequestId as CliRuntimeRequestId;
    use codex_cli_protocol::ServerRequest;
    use codex_cli_protocol::ToolRequestUserInputAnswer;
    use codex_cli_protocol::ToolRequestUserInputParams;
    use codex_cli_protocol::ToolRequestUserInputResponse;
    use codex_protocol::models::FileSystemPermissions;
    use codex_protocol::models::NetworkPermissions;
    use codex_protocol::request_permissions::RequestPermissionProfile;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn resolves_exec_approval_through_cli_runtime_request_id() {
        let mut pending = PendingCliRuntimeRequests::default();
        let request = ServerRequest::CommandExecutionRequestApproval {
            request_id: CliRuntimeRequestId::Integer(41),
            params: CommandExecutionRequestApprovalParams {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "call-1".to_string(),
                started_at_ms: 0,
                approval_id: Some("approval-1".to_string()),
                environment_id: None,
                reason: None,
                network_approval_context: None,
                command: Some("ls".to_string()),
                cwd: None,
                command_actions: None,
                additional_permissions: None,
                proposed_execpolicy_amendment: None,
                proposed_network_policy_amendments: None,
                available_decisions: None,
            },
        };

        assert_eq!(pending.note_server_request(&request), None);

        let resolution = pending
            .take_resolution(&Op::ExecApproval {
                id: "approval-1".to_string(),
                turn_id: None,
                decision: CommandExecutionApprovalDecision::Accept,
            })
            .expect("resolution should serialize")
            .expect("request should be pending");

        assert_eq!(resolution.request_id, CliRuntimeRequestId::Integer(41));
        assert_eq!(resolution.result, json!({ "decision": "accept" }));
    }

    #[test]
    fn rejects_permissions_with_paths_that_cannot_be_localized() {
        let mut pending = PendingCliRuntimeRequests::default();
        let request_id = CliRuntimeRequestId::Integer(7);
        let permissions = codex_cli_protocol::RequestPermissionProfile {
            network: None,
            file_system: Some(AdditionalFileSystemPermissions {
                read: Some(vec![
                    serde_json::from_value(json!("relative/path"))
                        .expect("relative API path should deserialize"),
                ]),
                write: None,
                glob_scan_max_depth: None,
                entries: None,
            }),
        };
        let localization_error =
            RequestPermissionProfile::try_from(permissions.clone()).expect_err("relative path");
        let cwd = AbsolutePathBuf::try_from(PathBuf::from(if cfg!(windows) {
            r"C:\tmp"
        } else {
            "/tmp"
        }))
        .expect("path must be absolute");

        assert_eq!(
            pending.note_server_request(&ServerRequest::PermissionsRequestApproval {
                request_id: request_id.clone(),
                params: PermissionsRequestApprovalParams {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "perm-1".to_string(),
                    environment_id: None,
                    started_at_ms: 0,
                    cwd,
                    reason: None,
                    permissions,
                },
            }),
            Some(UnsupportedCliRuntimeRequest {
                request_id,
                message: format!(
                    "failed to localize requested filesystem paths: {localization_error}"
                ),
            })
        );
    }

    #[test]
    fn resolves_permissions_and_user_input_through_cli_runtime_request_id() {
        let mut pending = PendingCliRuntimeRequests::default();
        let read_path = if cfg!(windows) {
            r"C:\tmp\read-only"
        } else {
            "/tmp/read-only"
        };
        let write_path = if cfg!(windows) {
            r"C:\tmp\write"
        } else {
            "/tmp/write"
        };
        let absolute_path = |path: &str| {
            AbsolutePathBuf::try_from(PathBuf::from(path)).expect("path must be absolute")
        };

        assert_eq!(
            pending.note_server_request(&ServerRequest::PermissionsRequestApproval {
                request_id: CliRuntimeRequestId::Integer(7),
                params: PermissionsRequestApprovalParams {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "perm-1".to_string(),
                    environment_id: None,
                    started_at_ms: 0,
                    cwd: absolute_path(if cfg!(windows) { r"C:\tmp" } else { "/tmp" }),
                    reason: None,
                    permissions: serde_json::from_value(json!({
                        "network": { "enabled": null }
                    }))
                    .expect("valid permissions"),
                },
            }),
            None
        );
        assert_eq!(
            pending.note_server_request(&ServerRequest::ToolRequestUserInput {
                request_id: CliRuntimeRequestId::Integer(8),
                params: ToolRequestUserInputParams {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-2".to_string(),
                    item_id: "tool-1".to_string(),
                    questions: Vec::new(),
                    is_blocking: true,
                    auto_resolution_ms: None,
                },
            }),
            None
        );

        let permissions = pending
            .take_resolution(&Op::RequestPermissionsResponse {
                id: "perm-1".to_string(),
                response: codex_protocol::request_permissions::RequestPermissionsResponse {
                    permissions: RequestPermissionProfile {
                        network: Some(NetworkPermissions {
                            enabled: Some(true),
                        }),
                        file_system: Some(FileSystemPermissions::from_read_write_roots(
                            Some(vec![absolute_path(read_path)]),
                            Some(vec![absolute_path(write_path)]),
                        )),
                    },
                    scope: codex_protocol::request_permissions::PermissionGrantScope::Session,
                    strict_auto_review: false,
                },
            })
            .expect("permissions response should serialize")
            .expect("permissions request should be pending");
        assert_eq!(permissions.request_id, CliRuntimeRequestId::Integer(7));
        assert_eq!(
            serde_json::from_value::<PermissionsRequestApprovalResponse>(permissions.result)
                .expect("permissions response should decode"),
            PermissionsRequestApprovalResponse {
                permissions: codex_cli_protocol::GrantedPermissionProfile {
                    network: Some(AdditionalNetworkPermissions {
                        enabled: Some(true),
                    }),
                    file_system: Some(AdditionalFileSystemPermissions {
                        read: Some(vec![absolute_path(read_path).into()]),
                        write: Some(vec![absolute_path(write_path).into()]),
                        glob_scan_max_depth: None,
                        entries: Some(vec![
                            codex_cli_protocol::FileSystemSandboxEntry {
                                path: codex_cli_protocol::FileSystemPath::Path {
                                    path: absolute_path(read_path).into(),
                                },
                                access: codex_cli_protocol::FileSystemAccessMode::Read,
                            },
                            codex_cli_protocol::FileSystemSandboxEntry {
                                path: codex_cli_protocol::FileSystemPath::Path {
                                    path: absolute_path(write_path).into(),
                                },
                                access: codex_cli_protocol::FileSystemAccessMode::Write,
                            },
                        ]),
                    }),
                },
                scope: PermissionGrantScope::Session,
                strict_auto_review: None,
            }
        );

        let user_input = pending
            .take_resolution(&Op::UserInputAnswer {
                id: "turn-2".to_string(),
                response: ToolRequestUserInputResponse {
                    answers: std::iter::once((
                        "question".to_string(),
                        ToolRequestUserInputAnswer {
                            answers: vec!["yes".to_string()],
                        },
                    ))
                    .collect(),
                },
            })
            .expect("user input response should serialize")
            .expect("user input request should be pending");
        assert_eq!(user_input.request_id, CliRuntimeRequestId::Integer(8));
        assert_eq!(
            serde_json::from_value::<ToolRequestUserInputResponse>(user_input.result)
                .expect("user input response should decode"),
            ToolRequestUserInputResponse {
                answers: std::iter::once((
                    "question".to_string(),
                    ToolRequestUserInputAnswer {
                        answers: vec!["yes".to_string()],
                    },
                ))
                .collect(),
            }
        );
    }

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
    fn resolves_patch_approval_through_cli_runtime_request_id() {
        let mut pending = PendingCliRuntimeRequests::default();
        assert_eq!(
            pending.note_server_request(&ServerRequest::FileChangeRequestApproval {
                request_id: CliRuntimeRequestId::Integer(13),
                params: FileChangeRequestApprovalParams {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "patch-1".to_string(),
                    started_at_ms: 0,
                    reason: None,
                    grant_root: None,
                },
            }),
            None
        );

        let resolution = pending
            .take_resolution(&Op::PatchApproval {
                id: "patch-1".to_string(),
                decision: FileChangeApprovalDecision::Cancel,
            })
            .expect("resolution should serialize")
            .expect("request should be pending");

        assert_eq!(resolution.request_id, CliRuntimeRequestId::Integer(13));
        assert_eq!(resolution.result, json!({ "decision": "cancel" }));
    }

    #[test]
    fn resolve_notification_returns_resolved_exec_request() {
        let mut pending = PendingCliRuntimeRequests::default();
        assert_eq!(
            pending.note_server_request(&ServerRequest::CommandExecutionRequestApproval {
                request_id: CliRuntimeRequestId::Integer(41),
                params: CommandExecutionRequestApprovalParams {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "call-1".to_string(),
                    started_at_ms: 0,
                    approval_id: Some("approval-1".to_string()),
                    environment_id: None,
                    reason: None,
                    network_approval_context: None,
                    command: Some("ls".to_string()),
                    cwd: None,
                    command_actions: None,
                    additional_permissions: None,
                    proposed_execpolicy_amendment: None,
                    proposed_network_policy_amendments: None,
                    available_decisions: None,
                },
            }),
            None
        );

        assert_eq!(
            pending.resolve_notification(&CliRuntimeRequestId::Integer(41)),
            Some(ResolvedCliRuntimeRequest::ExecApproval {
                id: "approval-1".to_string(),
            })
        );
        assert_eq!(
            pending.resolve_notification(&CliRuntimeRequestId::Integer(41)),
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
