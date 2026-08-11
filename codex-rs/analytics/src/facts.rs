use crate::events::CliRuntimeRpcTransport;
use crate::events::CodexRuntimeMetadata;
use crate::events::GuardianReviewEventParams;
use codex_cli_protocol::ClientRequest;
use codex_cli_protocol::ClientResponsePayload;
use codex_cli_protocol::InitializeParams;
use codex_cli_protocol::JSONRPCErrorError;
use codex_cli_protocol::RequestId;
use codex_cli_protocol::ServerNotification;
use codex_cli_protocol::ServerRequest;
use codex_cli_protocol::ServerResponse;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::error::CodexErr;
pub use codex_protocol::error::CodexErrKind;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SkillScope;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AcceptedLineFingerprint {
    pub path_hash: String,
    pub line_hash: String,
}

#[derive(Clone)]
pub struct TrackEventsContext {
    pub model_slug: String,
    pub thread_id: String,
    pub turn_id: String,
    pub product_client_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeModeToolCallFact {
    CellStarted {
        thread_id: String,
        turn_id: String,
        call_id: String,
        cell_id: String,
    },
    ChildStarted {
        thread_id: String,
        turn_id: String,
        call_id: String,
        cell_id: String,
    },
    CellClosed {
        thread_id: String,
        turn_id: String,
        cell_id: String,
    },
    SamplingResponseCompleted {
        thread_id: String,
        turn_id: String,
        response_id: String,
        tool_call_ids: Vec<String>,
    },
    Completed {
        thread_id: String,
        turn_id: String,
        call_id: String,
        cell_id: Option<String>,
        tool_name: String,
        started_at_ms: u64,
        completed_at_ms: u64,
        status: CodeModeToolCallStatus,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeModeToolCallStatus {
    Completed,
    Failed,
    Interrupted,
}

pub fn build_track_events_context(
    model_slug: String,
    thread_id: String,
    turn_id: String,
    product_client_id: String,
) -> TrackEventsContext {
    TrackEventsContext {
        model_slug,
        thread_id,
        turn_id,
        product_client_id,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageDetailSetting {
    High,
    Original,
}

/// Measurements for one successfully decoded image at the point where Codex prepares it for
/// durable conversation history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ImagePreparationMetadata {
    /// Set for images embedded in message content.
    pub message_role: Option<String>,
    /// Set to the originating call ID for tool-output images. This joins to the `item_id` on
    /// existing tool events for tool type and provenance.
    pub item_id: Option<String>,
    pub effective_detail: ImageDetailSetting,
    pub source_width: u32,
    pub source_height: u32,
    pub prepared_width: u32,
    pub prepared_height: u32,
}

#[derive(Clone)]
pub struct ImagePreparationFact {
    pub turn_id: String,
    pub metadata: ImagePreparationMetadata,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnSubmissionType {
    Default,
    Queued,
}

#[derive(Clone)]
pub struct TurnResolvedConfigFact {
    pub turn_id: String,
    pub thread_id: String,
    pub num_input_images: usize,
    pub submission_type: Option<TurnSubmissionType>,
    pub ephemeral: bool,
    pub session_source: SessionSource,
    pub model: String,
    pub model_provider: String,
    pub permission_profile: PermissionProfile,
    pub permission_profile_cwd: PathBuf,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub reasoning_summary: Option<ReasoningSummary>,
    pub service_tier: Option<ServiceTier>,
    pub approval_policy: AskForApproval,
    pub approvals_reviewer: ApprovalsReviewer,
    pub sandbox_network_access: bool,
    pub personality: Option<Personality>,
    pub workspace_kind: Option<String>,
    pub is_first_turn: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadInitializationMode {
    New,
    Forked,
    Resumed,
}

#[derive(Clone)]
pub struct TurnTokenUsageFact {
    pub turn_id: String,
    pub thread_id: String,
    pub token_usage: TokenUsage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TurnProfile {
    pub before_first_sampling_ms: u64,
    pub sampling_ms: u64,
    pub compaction_ms: u64,
    pub between_sampling_overhead_ms: u64,
    pub tool_blocking_ms: u64,
    pub after_last_sampling_ms: u64,
    pub sampling_request_count: u32,
    pub sampling_retry_count: u32,
}

#[derive(Clone)]
pub struct TurnProfileFact {
    pub turn_id: String,
    pub profile: TurnProfile,
}

#[derive(Clone)]
pub struct TurnCodexErrorFact {
    pub(crate) turn_id: String,
    pub(crate) thread_id: String,
    pub(crate) error: TurnCodexError,
}

impl TurnCodexErrorFact {
    pub fn from_codex_err(thread_id: String, turn_id: String, error: &CodexErr) -> Self {
        Self {
            turn_id,
            thread_id,
            error: TurnCodexError::from_codex_err(error),
        }
    }
}

#[derive(Clone)]
pub(crate) struct TurnCodexError {
    pub(crate) kind: CodexErrKind,
    pub(crate) http_status_code: Option<u16>,
}

impl TurnCodexError {
    fn from_codex_err(error: &CodexErr) -> Self {
        Self {
            kind: error.into(),
            http_status_code: error.http_status_code_value(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Failed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnSteerResult {
    Accepted,
    Rejected,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnSteerRejectionReason {
    NoActiveTurn,
    ExpectedTurnMismatch,
    NonSteerableCompact,
    EmptyInput,
    InputTooLarge,
}

#[derive(Clone)]
pub struct CodexTurnSteerEvent {
    pub expected_turn_id: Option<String>,
    pub accepted_turn_id: Option<String>,
    pub num_input_images: usize,
    pub result: TurnSteerResult,
    pub rejection_reason: Option<TurnSteerRejectionReason>,
    pub created_at: u64,
}

#[derive(Clone, Copy, Debug)]
pub enum AnalyticsJsonRpcError {
    TurnSteer(TurnSteerRequestError),
    Input(InputError),
}

#[derive(Clone, Copy, Debug)]
pub enum TurnSteerRequestError {
    NoActiveTurn,
    ExpectedTurnMismatch,
    NonSteerableCompact,
}

#[derive(Clone, Copy, Debug)]
pub enum InputError {
    Empty,
    TooLarge,
}

impl From<TurnSteerRequestError> for TurnSteerRejectionReason {
    fn from(error: TurnSteerRequestError) -> Self {
        match error {
            TurnSteerRequestError::NoActiveTurn => Self::NoActiveTurn,
            TurnSteerRequestError::ExpectedTurnMismatch => Self::ExpectedTurnMismatch,
            TurnSteerRequestError::NonSteerableCompact => Self::NonSteerableCompact,
        }
    }
}

impl From<InputError> for TurnSteerRejectionReason {
    fn from(error: InputError) -> Self {
        match error {
            InputError::Empty => Self::EmptyInput,
            InputError::TooLarge => Self::InputTooLarge,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SkillInvocation {
    pub skill_name: String,
    pub skill_scope: SkillScope,
    pub skill_path: PathBuf,
    pub invocation_type: InvocationType,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InvocationType {
    Explicit,
    Implicit,
}

pub struct AppInvocation {
    pub connector_id: Option<String>,
    pub app_name: Option<String>,
    pub invocation_type: Option<InvocationType>,
}

#[derive(Clone)]
pub struct SubAgentThreadStartedInput {
    pub session_id: String,
    pub thread_id: String,
    pub parent_thread_id: Option<String>,
    pub forked_from_thread_id: Option<String>,
    pub product_client_id: String,
    pub client_name: String,
    pub client_version: String,
    pub model: String,
    pub ephemeral: bool,
    pub subagent_source: SubAgentSource,
    pub created_at: u64,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Manual,
    Auto,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    UserRequested,
    ContextLimit,
    ModelDownshift,
    CompHashChanged,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionImplementation {
    Responses,
    ResponsesCompactionV2,
    ResponsesCompact,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionPhase {
    StandaloneTurn,
    PreTurn,
    MidTurn,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStrategy {
    Memento,
    PrefixCompaction,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStatus {
    Completed,
    Failed,
    Interrupted,
}

#[derive(Clone)]
pub struct CodexCompactionEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub trigger: CompactionTrigger,
    pub reason: CompactionReason,
    pub implementation: CompactionImplementation,
    pub phase: CompactionPhase,
    pub strategy: CompactionStrategy,
    pub status: CompactionStatus,
    pub codex_error_kind: Option<CodexErrKind>,
    pub codex_error_http_status_code: Option<u16>,
    pub active_context_tokens_before: i64,
    pub active_context_tokens_after: i64,
    pub retained_image_count: Option<usize>,
    pub compaction_summary_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub cache_write_input_tokens: Option<i64>,
    pub started_at: u64,
    pub completed_at: u64,
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalEventKind {
    Created,
    UsageAccounted,
    StatusChanged,
    Cleared,
}

#[derive(Clone)]
pub struct CodexGoalEvent {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub goal_id: String,
    pub event_kind: GoalEventKind,
    pub goal_status: codex_state::ThreadGoalStatus,
    pub has_token_budget: bool,
    pub cumulative_tokens_accounted: Option<i64>,
    pub cumulative_time_accounted_seconds: Option<i64>,
}

#[allow(dead_code)]
pub(crate) enum AnalyticsFact {
    Initialize {
        connection_id: u64,
        params: InitializeParams,
        product_client_id: String,
        runtime: CodexRuntimeMetadata,
        rpc_transport: CliRuntimeRpcTransport,
    },
    ClientRequest {
        connection_id: u64,
        request_id: RequestId,
        request: Box<ClientRequest>,
    },
    ExplicitClientInterruptRequest {
        connection_id: u64,
        request_id: RequestId,
        turn_id: String,
        requested_at_ms: u64,
    },
    ClientResponse {
        connection_id: u64,
        request_id: RequestId,
        response: Box<ClientResponsePayload>,
        thread_originator: Option<String>,
    },
    ErrorResponse {
        connection_id: u64,
        request_id: RequestId,
        error: JSONRPCErrorError,
        error_type: Option<AnalyticsJsonRpcError>,
    },
    ServerRequest {
        connection_id: u64,
        request: Box<ServerRequest>,
    },
    ServerResponse {
        completed_at_ms: u64,
        response: Box<ServerResponse>,
    },
    EffectivePermissionsApprovalResponse {
        completed_at_ms: u64,
        request_id: RequestId,
        response: Box<RequestPermissionsResponse>,
    },
    ServerRequestAborted {
        completed_at_ms: u64,
        request_id: RequestId,
    },
    Notification(Box<ServerNotification>),
    // Facts that do not naturally exist on the cli-runtime protocol surface, or
    // would require non-trivial protocol reshaping on this branch.
    Custom(CustomAnalyticsFact),
}

pub(crate) enum CustomAnalyticsFact {
    CodeModeToolCall(CodeModeToolCallFact),
    SubAgentThreadStarted(SubAgentThreadStartedInput),
    Compaction(Box<CodexCompactionEvent>),
    Goal(Box<CodexGoalEvent>),
    GuardianReview(Box<GuardianReviewEventParams>),
    TurnResolvedConfig(Box<TurnResolvedConfigFact>),
    TurnTokenUsage(Box<TurnTokenUsageFact>),
    TurnProfile(Box<TurnProfileFact>),
    TurnCodexError(Box<TurnCodexErrorFact>),
    ImagePreparation(Box<ImagePreparationFact>),
    SkillInvoked(SkillInvokedInput),
    AppMentioned(AppMentionedInput),
    AppUsed(AppUsedInput),
}

pub(crate) struct SkillInvokedInput {
    pub tracking: TrackEventsContext,
    pub invocations: Vec<SkillInvocation>,
}

pub(crate) struct AppMentionedInput {
    pub tracking: TrackEventsContext,
    pub mentions: Vec<AppInvocation>,
}

pub(crate) struct AppUsedInput {
    pub tracking: TrackEventsContext,
    pub app: AppInvocation,
}
