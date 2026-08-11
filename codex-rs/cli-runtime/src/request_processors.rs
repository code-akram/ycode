use crate::bespoke_event_handling::apply_bespoke_event_handling;
use crate::command_exec::CommandExecManager;
use crate::command_exec::StartCommandExecParams;
use crate::config_manager::ConfigManager;
use crate::error_code::INPUT_TOO_LARGE_ERROR_CODE;
use crate::error_code::invalid_params;
use crate::models::supported_models;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::ConnectionRequestId;
use crate::outgoing_message::OutgoingMessageSender;
use crate::outgoing_message::RequestContext;
use crate::outgoing_message::ThreadScopedOutgoingMessageSender;
use crate::skills_watcher::SkillsWatcher;
use crate::thread_status::ThreadWatchManager;
use crate::thread_status::resolve_thread_status;
use chrono::Duration as ChronoDuration;
use chrono::SecondsFormat;
use codex_analytics::AnalyticsEventsClient;
use codex_analytics::AnalyticsJsonRpcError;
use codex_analytics::InputError;
use codex_analytics::TurnSteerRequestError;
use codex_arg0::Arg0DispatchPaths;
use codex_backend_client::AddCreditsNudgeCreditType as BackendAddCreditsNudgeCreditType;
use codex_backend_client::Client as BackendClient;
use codex_backend_client::CodexWorkspaceMessage as BackendWorkspaceMessage;
use codex_backend_client::CodexWorkspaceMessageType as BackendWorkspaceMessageType;
use codex_backend_client::CodexWorkspaceMessagesResponse as BackendWorkspaceMessagesResponse;
use codex_backend_client::ConsumeRateLimitResetCreditCode as BackendConsumeRateLimitResetCreditCode;
use codex_backend_client::RateLimitResetCreditDetails as BackendRateLimitResetCreditDetails;
use codex_backend_client::RateLimitResetCreditsDetails as BackendRateLimitResetCreditsDetails;
use codex_backend_client::RequestError as BackendRequestError;
use codex_backend_client::TokenUsageProfile;
use codex_cli_protocol::Account;
use codex_cli_protocol::AccountLoginCompletedNotification;
use codex_cli_protocol::AccountTokenUsageDailyBucket;
use codex_cli_protocol::AccountTokenUsageSummary;
use codex_cli_protocol::AccountUpdatedNotification;
use codex_cli_protocol::AddCreditsNudgeCreditType;
use codex_cli_protocol::AddCreditsNudgeEmailStatus;
use codex_cli_protocol::AdditionalContextEntry;
use codex_cli_protocol::AdditionalContextKind;
use codex_cli_protocol::AskForApproval;
use codex_cli_protocol::AuthMode;
use codex_cli_protocol::CancelLoginAccountParams;
use codex_cli_protocol::CancelLoginAccountResponse;
use codex_cli_protocol::CancelLoginAccountStatus;
use codex_cli_protocol::ClientInfo;
use codex_cli_protocol::ClientRequest;
use codex_cli_protocol::ClientResponsePayload;
use codex_cli_protocol::CodexErrorInfo;
use codex_cli_protocol::CommandExecParams;
use codex_cli_protocol::CommandExecResizeParams;
use codex_cli_protocol::CommandExecTerminateParams;
use codex_cli_protocol::CommandExecWriteParams;
use codex_cli_protocol::ConfigWarningNotification;
use codex_cli_protocol::ConsumeAccountRateLimitResetCreditOutcome;
use codex_cli_protocol::ConsumeAccountRateLimitResetCreditParams;
use codex_cli_protocol::ConsumeAccountRateLimitResetCreditResponse;
use codex_cli_protocol::ConversationGitInfo;
use codex_cli_protocol::ConversationSummary;
use codex_cli_protocol::DeprecationNoticeNotification;
use codex_cli_protocol::DynamicToolFunctionSpec;
use codex_cli_protocol::DynamicToolNamespaceTool;
use codex_cli_protocol::DynamicToolSpec;
use codex_cli_protocol::EnvironmentAddParams;
use codex_cli_protocol::EnvironmentAddResponse;
use codex_cli_protocol::EnvironmentInfoParams;
use codex_cli_protocol::EnvironmentInfoResponse;
use codex_cli_protocol::EnvironmentShellInfo;
use codex_cli_protocol::EnvironmentStatusKind;
use codex_cli_protocol::EnvironmentStatusParams;
use codex_cli_protocol::EnvironmentStatusResponse;
use codex_cli_protocol::ExperimentalFeature as ApiExperimentalFeature;
use codex_cli_protocol::ExperimentalFeatureListParams;
use codex_cli_protocol::ExperimentalFeatureListResponse;
use codex_cli_protocol::ExperimentalFeatureStage as ApiExperimentalFeatureStage;
use codex_cli_protocol::FeedbackUploadParams;
use codex_cli_protocol::FeedbackUploadResponse;
use codex_cli_protocol::GetAccountParams;
use codex_cli_protocol::GetAccountRateLimitsResponse;
use codex_cli_protocol::GetAccountResponse;
use codex_cli_protocol::GetAccountTokenUsageResponse;
use codex_cli_protocol::GetAuthStatusParams;
use codex_cli_protocol::GetAuthStatusResponse;
use codex_cli_protocol::GetConversationSummaryParams;
use codex_cli_protocol::GetConversationSummaryResponse;
use codex_cli_protocol::GetWorkspaceMessagesResponse;
use codex_cli_protocol::GitDiffToRemoteParams;
use codex_cli_protocol::GitDiffToRemoteResponse;
use codex_cli_protocol::GitInfo as ApiGitInfo;
use codex_cli_protocol::InitializeParams;
use codex_cli_protocol::InitializeResponse;
use codex_cli_protocol::JSONRPCErrorError;
use codex_cli_protocol::LoginAccountParams;
use codex_cli_protocol::LoginAccountResponse;
use codex_cli_protocol::LoginApiKeyParams;
use codex_cli_protocol::LoginAppBrand;
use codex_cli_protocol::LogoutAccountResponse;
use codex_cli_protocol::MemoryResetResponse;
use codex_cli_protocol::MockExperimentalMethodParams;
use codex_cli_protocol::MockExperimentalMethodResponse;
use codex_cli_protocol::ModelListParams;
use codex_cli_protocol::ModelListResponse;
use codex_cli_protocol::PermissionProfileListParams;
use codex_cli_protocol::PermissionProfileListResponse;
use codex_cli_protocol::PermissionProfileSummary;
use codex_cli_protocol::RateLimitResetCredit;
use codex_cli_protocol::RateLimitResetCreditStatus;
use codex_cli_protocol::RateLimitResetCreditsSummary;
use codex_cli_protocol::RateLimitResetType;
use codex_cli_protocol::RequestId;
use codex_cli_protocol::SandboxMode;
use codex_cli_protocol::SendAddCreditsNudgeEmailParams;
use codex_cli_protocol::SendAddCreditsNudgeEmailResponse;
use codex_cli_protocol::ServerNotification;
use codex_cli_protocol::ServerRequestResolvedNotification;
use codex_cli_protocol::SkillsConfigWriteParams;
use codex_cli_protocol::SkillsConfigWriteResponse;
use codex_cli_protocol::SkillsExtraRootsSetParams;
use codex_cli_protocol::SkillsExtraRootsSetResponse;
use codex_cli_protocol::SkillsListParams;
use codex_cli_protocol::SkillsListResponse;
use codex_cli_protocol::SortDirection;
use codex_cli_protocol::Thread;
use codex_cli_protocol::ThreadArchiveParams;
use codex_cli_protocol::ThreadArchiveResponse;
use codex_cli_protocol::ThreadArchivedNotification;
use codex_cli_protocol::ThreadBackgroundTerminal;
use codex_cli_protocol::ThreadBackgroundTerminalsCleanParams;
use codex_cli_protocol::ThreadBackgroundTerminalsCleanResponse;
use codex_cli_protocol::ThreadBackgroundTerminalsListParams;
use codex_cli_protocol::ThreadBackgroundTerminalsListResponse;
use codex_cli_protocol::ThreadBackgroundTerminalsTerminateParams;
use codex_cli_protocol::ThreadBackgroundTerminalsTerminateResponse;
use codex_cli_protocol::ThreadClosedNotification;
use codex_cli_protocol::ThreadCompactStartParams;
use codex_cli_protocol::ThreadCompactStartResponse;
use codex_cli_protocol::ThreadDecrementElicitationParams;
use codex_cli_protocol::ThreadDecrementElicitationResponse;
use codex_cli_protocol::ThreadDeleteParams;
use codex_cli_protocol::ThreadDeleteResponse;
use codex_cli_protocol::ThreadDeletedNotification;
use codex_cli_protocol::ThreadForkParams;
use codex_cli_protocol::ThreadForkResponse;
use codex_cli_protocol::ThreadGoal;
use codex_cli_protocol::ThreadGoalClearParams;
use codex_cli_protocol::ThreadGoalClearResponse;
use codex_cli_protocol::ThreadGoalClearedNotification;
use codex_cli_protocol::ThreadGoalGetParams;
use codex_cli_protocol::ThreadGoalGetResponse;
use codex_cli_protocol::ThreadGoalSetParams;
use codex_cli_protocol::ThreadGoalSetResponse;
use codex_cli_protocol::ThreadGoalStatus;
use codex_cli_protocol::ThreadGoalUpdatedNotification;
use codex_cli_protocol::ThreadHistoryBuilder;
#[cfg(test)]
use codex_cli_protocol::ThreadHistoryMode;
use codex_cli_protocol::ThreadIncrementElicitationParams;
use codex_cli_protocol::ThreadIncrementElicitationResponse;
use codex_cli_protocol::ThreadInjectItemsParams;
use codex_cli_protocol::ThreadInjectItemsResponse;
use codex_cli_protocol::ThreadItem;
use codex_cli_protocol::ThreadItemEntry;
use codex_cli_protocol::ThreadItemsListParams;
use codex_cli_protocol::ThreadItemsListResponse;
use codex_cli_protocol::ThreadListCwdFilter;
use codex_cli_protocol::ThreadListParams;
use codex_cli_protocol::ThreadListResponse;
use codex_cli_protocol::ThreadLoadedListParams;
use codex_cli_protocol::ThreadLoadedListResponse;
use codex_cli_protocol::ThreadMemoryModeSetParams;
use codex_cli_protocol::ThreadMemoryModeSetResponse;
use codex_cli_protocol::ThreadMetadataGitInfoUpdateParams;
use codex_cli_protocol::ThreadMetadataUpdateParams;
use codex_cli_protocol::ThreadMetadataUpdateResponse;
use codex_cli_protocol::ThreadNameUpdatedNotification;
use codex_cli_protocol::ThreadReadParams;
use codex_cli_protocol::ThreadReadResponse;
use codex_cli_protocol::ThreadRealtimeAppendAudioParams;
use codex_cli_protocol::ThreadRealtimeAppendAudioResponse;
use codex_cli_protocol::ThreadRealtimeAppendSpeechParams;
use codex_cli_protocol::ThreadRealtimeAppendSpeechResponse;
use codex_cli_protocol::ThreadRealtimeAppendTextParams;
use codex_cli_protocol::ThreadRealtimeAppendTextResponse;
use codex_cli_protocol::ThreadRealtimeListVoicesResponse;
use codex_cli_protocol::ThreadRealtimeStartParams;
use codex_cli_protocol::ThreadRealtimeStartResponse;
use codex_cli_protocol::ThreadRealtimeStartTransport;
use codex_cli_protocol::ThreadRealtimeStopParams;
use codex_cli_protocol::ThreadRealtimeStopResponse;
use codex_cli_protocol::ThreadResumeInitialTurnsPageParams;
use codex_cli_protocol::ThreadResumeParams;
use codex_cli_protocol::ThreadResumeResponse;
use codex_cli_protocol::ThreadRollbackParams;
use codex_cli_protocol::ThreadSearchOccurrence;
use codex_cli_protocol::ThreadSearchOccurrencesParams;
use codex_cli_protocol::ThreadSearchOccurrencesResponse;
use codex_cli_protocol::ThreadSearchParams;
use codex_cli_protocol::ThreadSearchResponse;
use codex_cli_protocol::ThreadSearchResult;
use codex_cli_protocol::ThreadSearchSortKey;
use codex_cli_protocol::ThreadSearchTextRange;
use codex_cli_protocol::ThreadSetNameParams;
use codex_cli_protocol::ThreadSetNameResponse;
use codex_cli_protocol::ThreadSettings;
use codex_cli_protocol::ThreadSettingsUpdateParams;
use codex_cli_protocol::ThreadSettingsUpdateResponse;
use codex_cli_protocol::ThreadShellCommandParams;
use codex_cli_protocol::ThreadShellCommandResponse;
use codex_cli_protocol::ThreadSortKey;
use codex_cli_protocol::ThreadSourceKind;
use codex_cli_protocol::ThreadStartParams;
use codex_cli_protocol::ThreadStartResponse;
use codex_cli_protocol::ThreadStartedNotification;
use codex_cli_protocol::ThreadStatus;
use codex_cli_protocol::ThreadTurnsListParams;
use codex_cli_protocol::ThreadTurnsListResponse;
use codex_cli_protocol::ThreadUnarchiveParams;
use codex_cli_protocol::ThreadUnarchiveResponse;
use codex_cli_protocol::ThreadUnarchivedNotification;
use codex_cli_protocol::ThreadUnsubscribeParams;
use codex_cli_protocol::ThreadUnsubscribeResponse;
use codex_cli_protocol::ThreadUnsubscribeStatus;
use codex_cli_protocol::Turn;
use codex_cli_protocol::TurnEnvironmentParams;
use codex_cli_protocol::TurnError;
use codex_cli_protocol::TurnInterruptParams;
use codex_cli_protocol::TurnInterruptResponse;
use codex_cli_protocol::TurnItemsView;
use codex_cli_protocol::TurnStartParams;
use codex_cli_protocol::TurnStartResponse;
use codex_cli_protocol::TurnStatus;
use codex_cli_protocol::TurnSteerParams;
use codex_cli_protocol::TurnSteerResponse;
use codex_cli_protocol::UserInput as V2UserInput;
use codex_cli_protocol::WorkspaceMessage;
use codex_cli_protocol::WorkspaceMessageType;
use codex_config::CloudConfigBundleLoadError;
use codex_config::CloudConfigBundleLoadErrorCode;
use codex_config::ConfigLayerStack;
use codex_config::loader::project_trust_key;
use codex_core::CodexThread;
use codex_core::CodexThreadSettingsOverrides;
use codex_core::ForkSnapshot;
use codex_core::NewThread;
#[cfg(test)]
use codex_core::SessionMeta;
use codex_core::StartThreadOptions;
use codex_core::SteerInputError;
use codex_core::ThreadConfigSnapshot;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::config::NetworkProxyAuditMetadata;
use codex_core::config::edit::ConfigEdit;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_core::exec::ExecCapturePolicy;
use codex_core::exec::ExecExpiration;
use codex_core::exec::ExecParams;
use codex_core::exec_env::create_env;
use codex_core::path_utils;
#[cfg(test)]
use codex_core::read_head_for_summary;
use codex_core::truncate_rollout_after_turn_id;
use codex_core::truncate_rollout_before_turn_id;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::EnvironmentObservedStatus;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_exec_server::LOCAL_FS;
use codex_features::FEATURES;
use codex_features::Feature;
use codex_features::Stage;
use codex_feedback::CodexFeedback;
use codex_feedback::FeedbackAttachmentPath;
use codex_feedback::FeedbackUploadOptions;
use codex_git_utils::git_diff_to_remote;
use codex_git_utils::resolve_root_git_project_for_trust;
use codex_login::AuthManager;
use codex_login::CODEX_OPEN_APP_URL;
use codex_login::CodexAuth;
use codex_login::LoginSuccessPage;
use codex_login::LoginSuccessPageBrand;
use codex_login::ServerOptions as LoginServerOptions;
use codex_login::ShutdownHandle;
use codex_login::complete_device_code_login;
use codex_login::login_with_api_key;
use codex_login::login_with_bedrock_api_key;
use codex_login::oauth_client_id;
use codex_login::request_device_code;
use codex_login::run_login_server;
use codex_memories_write::clear_memory_roots_contents;
use codex_model_provider::create_model_provider;
use codex_protocol::ThreadId;
use codex_protocol::config_types::AgentSettings;
use codex_protocol::config_types::ForcedLoginMethod;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::TrustLevel;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
#[cfg(test)]
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
#[cfg(test)]
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::ConversationAudioParams;
use codex_protocol::protocol::ConversationSpeechParams;
use codex_protocol::protocol::ConversationStartParams;
use codex_protocol::protocol::ConversationStartTransport;
use codex_protocol::protocol::ConversationTextParams;
use codex_protocol::protocol::EventMsg;
#[cfg(test)]
use codex_protocol::protocol::GitInfo as CoreGitInfo;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RealtimeVoicesList;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionConfiguredEvent;
#[cfg(test)]
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::TurnEnvironmentSelections;
use codex_protocol::protocol::W3cTraceContext;
use codex_protocol::protocol::strip_user_message_prefix;
use codex_protocol::user_input::MAX_USER_INPUT_TEXT_CHARS;
use codex_protocol::user_input::UserInput as CoreInputItem;
use codex_rollout::is_persisted_rollout_item;
use codex_rollout::state_db::StateDbHandle;
use codex_rollout::state_db::reconcile_rollout;
use codex_state::ThreadMetadata;
use codex_state::log_db::LogDbLayer;
use codex_thread_store::ArchiveThreadParams as StoreArchiveThreadParams;
use codex_thread_store::ArchiveThreadsParams as StoreArchiveThreadsParams;
use codex_thread_store::DeleteThreadsParams as StoreDeleteThreadsParams;
use codex_thread_store::GitInfoPatch as StoreGitInfoPatch;
use codex_thread_store::ItemSortKey as StoreItemSortKey;
use codex_thread_store::ListItemsParams as StoreListItemsParams;
use codex_thread_store::ListThreadsParams as StoreListThreadsParams;
use codex_thread_store::ListTurnsParams as StoreListTurnsParams;
use codex_thread_store::LoadThreadHistoryParams as StoreLoadThreadHistoryParams;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::ReadThreadByRolloutPathParams as StoreReadThreadByRolloutPathParams;
use codex_thread_store::ReadThreadParams as StoreReadThreadParams;
use codex_thread_store::SearchThreadOccurrencesParams as StoreSearchThreadOccurrencesParams;
use codex_thread_store::SearchThreadsParams as StoreSearchThreadsParams;
use codex_thread_store::SortDirection as StoreSortDirection;
use codex_thread_store::StoredThread;
use codex_thread_store::StoredTurn;
use codex_thread_store::StoredTurnItemsView;
use codex_thread_store::StoredTurnStatus;
use codex_thread_store::ThreadMetadataPatch as StoreThreadMetadataPatch;
use codex_thread_store::ThreadRelationFilter as StoreThreadRelationFilter;
use codex_thread_store::ThreadSortKey as StoreThreadSortKey;
use codex_thread_store::ThreadStore;
use codex_thread_store::ThreadStoreError;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_pty::DEFAULT_OUTPUT_BYTES_CAP;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Error as IoError;
use std::path::Path;
use std::path::PathBuf;
use std::result::Result;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;
use tokio::sync::broadcast;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use toml::Value as TomlValue;
use tracing::Instrument;
use tracing::error;
use tracing::info;
use tracing::warn;
use uuid::Uuid;

#[cfg(test)]
use codex_cli_protocol::ServerRequest;

mod account_processor;
mod bedrock_auth;
mod catalog_processor;
mod command_exec_processor;
mod config_processor;
mod environment_processor;
mod feedback_doctor_report;
mod feedback_processor;
mod fs_processor;
mod git_processor;
mod initialize_processor;
mod process_exec_processor;
mod search;
mod thread_enrichment;
mod thread_fork_goal;
mod thread_processor;
mod thread_sections;
mod token_usage_replay;
mod turn_processor;

pub(crate) use account_processor::AccountRequestProcessor;
pub(crate) use catalog_processor::CatalogRequestProcessor;
pub(crate) use command_exec_processor::CommandExecRequestProcessor;
pub(crate) use config_processor::ConfigRequestProcessor;
pub(crate) use environment_processor::EnvironmentRequestProcessor;
pub(crate) use feedback_processor::FeedbackRequestProcessor;
pub(crate) use fs_processor::FsRequestProcessor;
pub(crate) use git_processor::GitRequestProcessor;
pub(crate) use initialize_processor::InitializeRequestProcessor;
pub(crate) use process_exec_processor::ProcessExecRequestProcessor;
pub(crate) use search::SearchRequestProcessor;
pub(crate) use thread_goal_processor::ThreadGoalRequestProcessor;
pub(crate) use thread_processor::ThreadRequestProcessor;
pub(crate) use turn_processor::TurnRequestProcessor;

use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::filters::compute_source_filters;
use crate::filters::source_kind_matches;
use crate::thread_state::ConnectionCapabilities;
use crate::thread_state::ThreadListenerCommand;
use crate::thread_state::ThreadState;
use crate::thread_state::ThreadStateManager;
use token_usage_replay::restored_token_usage_turn_id;
use token_usage_replay::send_thread_token_usage_update_to_connection;

fn resolve_request_cwd(cwd: Option<PathBuf>) -> Result<Option<AbsolutePathBuf>, JSONRPCErrorError> {
    cwd.map(|cwd| {
        AbsolutePathBuf::relative_to_current_dir(path_utils::normalize_for_native_workdir(cwd))
            .map_err(|err| invalid_request(format!("invalid cwd: {err}")))
    })
    .transpose()
}

fn resolve_turn_environment_selections(
    thread_manager: &ThreadManager,
    environments: Option<Vec<TurnEnvironmentParams>>,
) -> Result<Option<Vec<TurnEnvironmentSelection>>, JSONRPCErrorError> {
    let Some(environments) = environments else {
        return Ok(None);
    };
    let mut selections = Vec::with_capacity(environments.len());
    for environment in environments {
        let environment_id = environment.environment_id;
        let cwd = environment
            .cwd
            .to_inferred_path_uri()
            .ok_or_else(|| {
                invalid_request(format!(
                    "invalid cwd for environment `{environment_id}`: path `{}` does not use absolute POSIX or Windows path syntax",
                    environment.cwd
                ))
            })?;
        let workspace_roots = environment
            .runtime_workspace_roots
            .map(|roots| {
                let mut resolved_roots = Vec::new();
                for root in roots {
                    let root = root.to_inferred_path_uri().ok_or_else(|| {
                        invalid_request(format!(
                            "invalid runtime workspace root for environment `{environment_id}`: path `{root}` does not use absolute POSIX or Windows path syntax"
                        ))
                    })?;
                    if !resolved_roots.contains(&root) {
                        resolved_roots.push(root);
                    }
                }
                Ok::<_, JSONRPCErrorError>(resolved_roots)
            })
            .transpose()?
            .unwrap_or_else(|| vec![cwd.clone()]);
        selections.push(TurnEnvironmentSelection {
            environment_id,
            cwd,
            workspace_roots,
        });
    }
    thread_manager
        .validate_environment_selections(&selections)
        .map_err(environment_selection_error)?;
    Ok(Some(selections))
}

fn resolve_runtime_workspace_roots(workspace_roots: Vec<AbsolutePathBuf>) -> Vec<AbsolutePathBuf> {
    let mut resolved_roots = Vec::new();
    for root in workspace_roots {
        if !resolved_roots.iter().any(|existing| existing == &root) {
            resolved_roots.push(root);
        }
    }
    resolved_roots
}

mod config_errors;
mod request_errors;
mod thread_delete;
mod thread_goal_processor;
mod thread_lifecycle;
mod thread_summary;

use self::config_errors::*;
use self::request_errors::*;
use self::thread_goal_processor::api_thread_goal_from_state;
use self::thread_lifecycle::*;
use self::thread_summary::*;

pub(crate) use self::thread_lifecycle::populate_thread_turns_from_history;
pub(crate) use self::thread_processor::thread_from_stored_thread;
#[cfg(test)]
pub(crate) use self::thread_summary::read_summary_from_rollout;
#[cfg(test)]
pub(crate) use self::thread_summary::summary_to_thread;
pub(crate) use self::thread_summary::thread_settings_from_config_snapshot;
pub(crate) use self::thread_summary::thread_settings_from_core_snapshot;

pub(crate) fn build_legacy_api_turns_from_rollout_items(items: &[RolloutItem]) -> Vec<Turn> {
    let mut builder = ThreadHistoryBuilder::new();
    for item in items {
        if is_persisted_rollout_item(item, codex_protocol::protocol::ThreadHistoryMode::Legacy) {
            builder.handle_rollout_item(item);
        }
    }
    builder.finish()
}
