mod experimental_api;
mod protocol;
pub mod rpc;

pub use experimental_api::*;
pub use protocol::common::*;
pub use protocol::event_mapping::*;
pub use protocol::item_builders::*;
pub use protocol::thread_history::*;
pub use protocol::thread_history_projection::*;
pub use protocol::v1::ApplyPatchApprovalParams;
pub use protocol::v1::ApplyPatchApprovalResponse;
pub use protocol::v1::ClientInfo;
pub use protocol::v1::ConversationGitInfo;
pub use protocol::v1::ConversationSummary;
pub use protocol::v1::ExecCommandApprovalParams;
pub use protocol::v1::ExecCommandApprovalResponse;
pub use protocol::v1::GetAuthStatusParams;
pub use protocol::v1::GetAuthStatusResponse;
pub use protocol::v1::GetConversationSummaryParams;
pub use protocol::v1::GetConversationSummaryResponse;
pub use protocol::v1::GitDiffToRemoteParams;
pub use protocol::v1::GitDiffToRemoteResponse;
pub use protocol::v1::GitSha;
pub use protocol::v1::InitializeCapabilities;
pub use protocol::v1::InitializeParams;
pub use protocol::v1::InitializeResponse;
pub use protocol::v1::InterruptConversationResponse;
pub use protocol::v1::SandboxSettings;
pub use protocol::v1::Tools;
pub use protocol::v1::UserSavedConfig;
pub use protocol::v2::*;
pub use rpc::*;

#[cfg(not(test))]
pub(crate) use codex_cli_protocol_noop_macros::JsonSchema;
#[cfg(not(test))]
pub(crate) use codex_cli_protocol_noop_macros::TS;
#[cfg(test)]
pub(crate) use schemars::JsonSchema;
#[cfg(test)]
pub(crate) use ts_rs::TS;
