#![recursion_limit = "256"]
#![deny(clippy::print_stdout, clippy::print_stderr)]

use codex_cli_protocol::ConfigWarningNotification;
use codex_cli_protocol::TextPosition as RuntimeTextPosition;
use codex_cli_protocol::TextRange as RuntimeTextRange;

mod attestation;
mod auth_mode;
mod bespoke_event_handling;
mod cli_runtime_tracing;
mod command_exec;
mod config_layer;
mod config_manager;
mod config_manager_service;
mod connection_rpc_gate;
mod current_time;
mod dynamic_tools;
mod error_code;
mod extensions;
mod external_auth;
mod filters;
mod fs_watch;
mod fuzzy_file_search;
mod image_url;
pub mod in_process;
mod message_processor;
mod models;
mod models_refresh_worker;
mod outgoing_message;
mod request_processors;
mod request_serialization;
mod server_request_error;
mod skills_watcher;
mod thread_state;
mod thread_status;
mod transport;

pub use crate::error_code::INPUT_TOO_LARGE_ERROR_CODE;
pub use crate::error_code::INVALID_PARAMS_ERROR_CODE;

#[cfg(any())]
fn exec_policy_warning_location(
    err: &ExecPolicyError,
) -> (Option<String>, Option<RuntimeTextRange>) {
    match err {
        ExecPolicyError::ParsePolicy { path, source } => {
            if let Some(location) = source.location() {
                let range = RuntimeTextRange {
                    start: RuntimeTextPosition {
                        line: location.range.start.line,
                        column: location.range.start.column,
                    },
                    end: RuntimeTextPosition {
                        line: location.range.end.line,
                        column: location.range.end.column,
                    },
                };
                return (Some(location.path), Some(range));
            }
            (Some(path.clone()), None)
        }
        _ => (None, None),
    }
}

#[cfg(any())]
fn exec_policy_config_warning(err: &ExecPolicyError) -> ConfigWarningNotification {
    let (path, range) = exec_policy_warning_location(err);
    ConfigWarningNotification {
        summary: "Error parsing rules; custom rules not applied.".to_string(),
        details: Some(err.to_string()),
        path,
        range,
    }
}
