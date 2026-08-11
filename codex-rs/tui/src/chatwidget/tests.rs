//! Exercises `ChatWidget` event handling and rendering invariants.
//!
//! These tests cover both cli-runtime-native inputs and focused widget helpers. Many assertions are
//! snapshot-based so that layout regressions and status/header changes show up as stable,
//! reviewable diffs.

pub(super) use super::*;
pub(super) use crate::app_command::AppCommand as Op;
pub(super) use crate::app_event::AppEvent;
pub(super) use crate::app_event::ExitMode;
pub(super) use crate::app_event_sender::AppEventSender;
pub(super) use crate::approval_events::ApplyPatchApprovalRequestEvent;
pub(super) use crate::approval_events::ExecApprovalRequestEvent;
pub(super) use crate::bottom_pane::LocalImageAttachment;
pub(super) use crate::bottom_pane::MentionBinding;
pub(super) use crate::bottom_pane::QueuedInputAction;
pub(super) use crate::diff_model::FileChange;
pub(super) use crate::history_cell::UserHistoryCell;
pub(super) use crate::legacy_core::config::Config;
pub(super) use crate::legacy_core::config::ConfigBuilder;
pub(super) use crate::model_catalog::ModelCatalog;
pub(super) use crate::test_backend::VT100Backend;
pub(super) use crate::test_support::PathBufExt;
pub(super) use crate::test_support::test_path_buf;
pub(super) use crate::test_support::test_path_display;
pub(super) use crate::token_usage::TokenUsage;
pub(super) use crate::token_usage::TokenUsageInfo;
pub(super) use crate::tui::FrameRequester;
pub(super) use assert_matches::assert_matches;
pub(super) use codex_cli_protocol::AddCreditsNudgeCreditType;
pub(super) use codex_cli_protocol::AddCreditsNudgeEmailStatus;
pub(super) use codex_cli_protocol::AdditionalFileSystemPermissions as CliRuntimeAdditionalFileSystemPermissions;
pub(super) use codex_cli_protocol::AdditionalNetworkPermissions as CliRuntimeAdditionalNetworkPermissions;
pub(super) use codex_cli_protocol::AdditionalPermissionProfile as CliRuntimeAdditionalPermissionProfile;
pub(super) use codex_cli_protocol::AutoReviewDecisionSource as CliRuntimeGuardianApprovalReviewDecisionSource;
pub(super) use codex_cli_protocol::CodexErrorInfo;
pub(super) use codex_cli_protocol::CollabAgentState as CliRuntimeCollabAgentState;
pub(super) use codex_cli_protocol::CollabAgentStatus as CliRuntimeCollabAgentStatus;
pub(super) use codex_cli_protocol::CollabAgentTool as CliRuntimeCollabAgentTool;
pub(super) use codex_cli_protocol::CollabAgentToolCallStatus as CliRuntimeCollabAgentToolCallStatus;
pub(super) use codex_cli_protocol::CommandAction as CliRuntimeCommandAction;
pub(super) use codex_cli_protocol::CommandExecutionRequestApprovalParams as CliRuntimeCommandExecutionRequestApprovalParams;
pub(super) use codex_cli_protocol::CommandExecutionSource as ExecCommandSource;
pub(super) use codex_cli_protocol::CommandExecutionSource as CliRuntimeCommandExecutionSource;
pub(super) use codex_cli_protocol::CommandExecutionStatus as CliRuntimeCommandExecutionStatus;
pub(super) use codex_cli_protocol::ConfigWarningNotification;
pub(super) use codex_cli_protocol::CreditsSnapshot;
pub(super) use codex_cli_protocol::ErrorNotification;
pub(super) use codex_cli_protocol::ExecPolicyAmendment;
pub(super) use codex_cli_protocol::FileUpdateChange;
pub(super) use codex_cli_protocol::GuardianApprovalReview;
pub(super) use codex_cli_protocol::GuardianApprovalReviewAction as CliRuntimeGuardianApprovalReviewAction;
pub(super) use codex_cli_protocol::GuardianApprovalReviewStatus;
pub(super) use codex_cli_protocol::GuardianCommandSource as CliRuntimeGuardianCommandSource;
pub(super) use codex_cli_protocol::GuardianRiskLevel as CliRuntimeGuardianRiskLevel;
pub(super) use codex_cli_protocol::GuardianUserAuthorization as CliRuntimeGuardianUserAuthorization;
pub(super) use codex_cli_protocol::GuardianWarningNotification;
pub(super) use codex_cli_protocol::HookCompletedNotification as CliRuntimeHookCompletedNotification;
pub(super) use codex_cli_protocol::HookEventName as CliRuntimeHookEventName;
pub(super) use codex_cli_protocol::HookExecutionMode as CliRuntimeHookExecutionMode;
pub(super) use codex_cli_protocol::HookHandlerType as CliRuntimeHookHandlerType;
pub(super) use codex_cli_protocol::HookOutputEntry as CliRuntimeHookOutputEntry;
pub(super) use codex_cli_protocol::HookOutputEntryKind as CliRuntimeHookOutputEntryKind;
pub(super) use codex_cli_protocol::HookRunStatus as CliRuntimeHookRunStatus;
pub(super) use codex_cli_protocol::HookRunSummary as CliRuntimeHookRunSummary;
pub(super) use codex_cli_protocol::HookScope as CliRuntimeHookScope;
pub(super) use codex_cli_protocol::HookStartedNotification as CliRuntimeHookStartedNotification;
pub(super) use codex_cli_protocol::ItemCompletedNotification;
pub(super) use codex_cli_protocol::ItemGuardianApprovalReviewCompletedNotification;
pub(super) use codex_cli_protocol::ItemGuardianApprovalReviewStartedNotification;
pub(super) use codex_cli_protocol::ItemStartedNotification;
pub(super) use codex_cli_protocol::MarketplaceAddResponse;
pub(super) use codex_cli_protocol::MarketplaceInterface;
pub(super) use codex_cli_protocol::MarketplaceUpgradeErrorInfo;
pub(super) use codex_cli_protocol::MarketplaceUpgradeResponse;
pub(super) use codex_cli_protocol::ModelSafetyBufferingUpdatedNotification;
pub(super) use codex_cli_protocol::ModelVerification as CliRuntimeModelVerification;
pub(super) use codex_cli_protocol::ModelVerificationNotification;
pub(super) use codex_cli_protocol::NonSteerableTurnKind;
pub(super) use codex_cli_protocol::PatchApplyStatus as CliRuntimePatchApplyStatus;
pub(super) use codex_cli_protocol::PatchChangeKind;
pub(super) use codex_cli_protocol::PermissionsRequestApprovalParams as CliRuntimePermissionsRequestApprovalParams;
pub(super) use codex_cli_protocol::PluginAuthPolicy;
pub(super) use codex_cli_protocol::PluginDetail;
pub(super) use codex_cli_protocol::PluginInstallPolicy;
pub(super) use codex_cli_protocol::PluginInterface;
pub(super) use codex_cli_protocol::PluginListResponse;
pub(super) use codex_cli_protocol::PluginMarketplaceEntry;
pub(super) use codex_cli_protocol::PluginReadResponse;
pub(super) use codex_cli_protocol::PluginSource;
pub(super) use codex_cli_protocol::PluginSummary;
pub(super) use codex_cli_protocol::RateLimitReachedType;
pub(super) use codex_cli_protocol::RateLimitSnapshot;
pub(super) use codex_cli_protocol::RateLimitWindow;
pub(super) use codex_cli_protocol::ReasoningSummaryTextDeltaNotification;
pub(super) use codex_cli_protocol::ReviewTarget;
pub(super) use codex_cli_protocol::ServerNotification;
pub(super) use codex_cli_protocol::SkillMetadata;
pub(super) use codex_cli_protocol::SkillSummary;
pub(super) use codex_cli_protocol::ThreadClosedNotification;
pub(super) use codex_cli_protocol::ThreadItem as CliRuntimeThreadItem;
pub(super) use codex_cli_protocol::ToolRequestUserInputOption;
pub(super) use codex_cli_protocol::ToolRequestUserInputParams;
pub(super) use codex_cli_protocol::ToolRequestUserInputQuestion;
pub(super) use codex_cli_protocol::Turn as CliRuntimeTurn;
pub(super) use codex_cli_protocol::TurnCompletedNotification;
pub(super) use codex_cli_protocol::TurnError as CliRuntimeTurnError;
pub(super) use codex_cli_protocol::TurnStartedNotification;
pub(super) use codex_cli_protocol::TurnStatus as CliRuntimeTurnStatus;
pub(super) use codex_cli_protocol::UserInput;
pub(super) use codex_cli_protocol::UserInput as CliRuntimeUserInput;
pub(super) use codex_cli_protocol::WarningNotification;
pub(super) use codex_config::ConfigLayerStack;
pub(super) use codex_config::Constrained;
pub(super) use codex_config::ConstraintError;
pub(super) use codex_config::RequirementSource;
pub(super) use codex_config::types::ApprovalsReviewer;
pub(super) use codex_config::types::Notifications;
pub(super) use codex_core_plugins::OPENAI_CURATED_MARKETPLACE_NAME;
pub(super) use codex_features::FEATURES;
pub(super) use codex_features::Feature;
pub(super) use codex_git_utils::CommitLogEntry;
pub(super) use codex_models_manager::test_support::construct_model_info_offline_for_tests;
pub(super) use codex_models_manager::test_support::get_model_offline_for_tests;
pub(super) use codex_otel::RuntimeMetricsSummary;
pub(super) use codex_otel::SessionTelemetry;
pub(super) use codex_protocol::ThreadId;
pub(super) use codex_protocol::account::PlanType;
pub(super) use codex_protocol::approvals::GuardianAssessmentAction;
pub(super) use codex_protocol::approvals::GuardianAssessmentDecisionSource;
pub(super) use codex_protocol::approvals::GuardianAssessmentEvent;
pub(super) use codex_protocol::approvals::GuardianAssessmentStatus;
pub(super) use codex_protocol::approvals::GuardianCommandSource;
pub(super) use codex_protocol::approvals::GuardianRiskLevel;
pub(super) use codex_protocol::approvals::GuardianUserAuthorization;
pub(super) use codex_protocol::config_types::CollaborationMode;
pub(super) use codex_protocol::config_types::ModeKind;
pub(super) use codex_protocol::config_types::Personality;
pub(super) use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
pub(super) use codex_protocol::config_types::ServiceTier;
pub(super) use codex_protocol::models::ActivePermissionProfile;
pub(super) use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
pub(super) use codex_protocol::models::FileSystemPermissions;
pub(super) use codex_protocol::models::MessagePhase;
pub(super) use codex_protocol::models::NetworkPermissions;
pub(super) use codex_protocol::models::PermissionProfile;
pub(super) use codex_protocol::openai_models::ModelInfo;
pub(super) use codex_protocol::openai_models::ModelPreset;
pub(super) use codex_protocol::openai_models::ModelsResponse;
pub(super) use codex_protocol::openai_models::ReasoningEffortPreset;
pub(super) use codex_protocol::openai_models::default_input_modalities;
pub(super) use codex_protocol::parse_command::ParsedCommand;
pub(super) use codex_protocol::plan_tool::PlanItemArg;
pub(super) use codex_protocol::plan_tool::StepStatus;
pub(super) use codex_protocol::plan_tool::UpdatePlanArgs;
pub(super) use codex_protocol::request_permissions::RequestPermissionProfile;
pub(super) use codex_protocol::user_input::TextElement;
pub(super) use codex_terminal_detection::Multiplexer;
pub(super) use codex_terminal_detection::TerminalInfo;
pub(super) use codex_terminal_detection::TerminalName;
pub(super) use codex_utils_absolute_path::AbsolutePathBuf;
pub(super) use codex_utils_approval_presets::builtin_approval_presets;
pub(super) use codex_utils_path_uri::LegacyAppPathString;
pub(super) use crossterm::event::KeyCode;
pub(super) use crossterm::event::KeyEvent;
pub(super) use crossterm::event::KeyModifiers;
pub(super) use insta::assert_snapshot;
pub(super) use serde_json::json;
pub(super) use std::collections::HashMap;
pub(super) use std::path::PathBuf;
pub(super) use tempfile::NamedTempFile;
pub(super) use tempfile::tempdir;
pub(super) use tokio::sync::mpsc::error::TryRecvError;
pub(super) use tokio::sync::mpsc::unbounded_channel;
pub(super) use toml::Value as TomlValue;

pub(super) fn chatwidget_snapshot_dir() -> PathBuf {
    let snapshot_file = codex_utils_cargo_bin::find_resource!(
        "src/chatwidget/snapshots/codex_tui__chatwidget__tests__chatwidget_tall.snap"
    )
    .expect("snapshot file");
    snapshot_file
        .parent()
        .unwrap_or_else(|| panic!("snapshot file has no parent: {}", snapshot_file.display()))
        .to_path_buf()
}

macro_rules! assert_chatwidget_snapshot {
    ($name:expr, $value:expr $(,)?) => {{
        let mut settings = insta::Settings::clone_current();
        settings.set_prepend_module_to_snapshot(false);
        settings.set_snapshot_path(crate::chatwidget::tests::chatwidget_snapshot_dir());
        settings.bind(|| {
            insta::assert_snapshot!(format!("codex_tui__chatwidget__tests__{}", $name), $value);
        });
    }};
    ($name:expr, $value:expr, @$snapshot:literal $(,)?) => {{
        let mut settings = insta::Settings::clone_current();
        settings.set_prepend_module_to_snapshot(false);
        settings.set_snapshot_path(crate::chatwidget::tests::chatwidget_snapshot_dir());
        settings.bind(|| {
            insta::assert_snapshot!(
                format!("codex_tui__chatwidget__tests__{}", $name),
                &($value),
                @$snapshot
            );
        });
    }};
}

fn next_goal_draft(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    expected_thread_id: ThreadId,
) -> crate::goal_files::GoalDraft {
    loop {
        let event = rx.try_recv().expect("expected goal draft event");
        if let AppEvent::SetThreadGoalDraft {
            thread_id, draft, ..
        } = event
        {
            assert_eq!(thread_id, expected_thread_id);
            return draft;
        }
    }
}

mod approval_requests;
mod composer_submission;
#[path = "tests/config_errors_tests.rs"]
mod config_errors;
mod exec_flow;
mod goal_menu;
mod goal_validation;
mod guardian;
pub(crate) mod helpers;
mod history_replay;
mod permissions;
mod plan_mode;
#[path = "tests/plugin_catalog_tests.rs"]
mod plugin_catalog;
mod popups_and_settings;
mod review_mode;
mod runtime;
mod side;
mod slash_commands;
mod status_and_layout;
mod status_command_tests;
mod status_surface_previews;
mod terminal_title;
mod usage;

pub(crate) use helpers::make_chatwidget_manual_with_sender;
pub(crate) use helpers::set_chatgpt_auth;
pub(crate) use helpers::set_fast_mode_test_catalog;
pub(super) use helpers::*;
