use std::fmt;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

use super::Capability;
use super::CapabilitySet;
use super::DelegateRequestId;
use super::HandshakeRejectReason;
use super::ProtocolVersion;
use super::RequestId;
use super::SessionId;
use super::SupportedProtocolVersions;
use super::TransportLane;
use super::WireCellId;
use super::WireExecuteRequest;
use super::WireNestedToolCall;
use super::WireRuntimeResponse;
use super::WireSessionCellExecutionLimits;
use super::WireWaitOutcome;
use super::WireWaitRequest;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientHello {
    supported_versions: SupportedProtocolVersions,
    required_capabilities: CapabilitySet,
    optional_capabilities: CapabilitySet,
}

impl ClientHello {
    pub fn new(
        supported_versions: SupportedProtocolVersions,
        required_capabilities: CapabilitySet,
        optional_capabilities: CapabilitySet,
    ) -> Result<Self, ClientHelloError> {
        if let Some(capability) = required_capabilities
            .iter()
            .find(|capability| optional_capabilities.contains(capability))
        {
            return Err(ClientHelloError::OverlappingCapability(capability.clone()));
        }
        Ok(Self {
            supported_versions,
            required_capabilities,
            optional_capabilities,
        })
    }

    pub fn supported_versions(&self) -> &SupportedProtocolVersions {
        &self.supported_versions
    }

    pub fn required_capabilities(&self) -> &CapabilitySet {
        &self.required_capabilities
    }

    pub fn optional_capabilities(&self) -> &CapabilitySet {
        &self.optional_capabilities
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ClientHelloWire {
    supported_versions: SupportedProtocolVersions,
    required_capabilities: CapabilitySet,
    optional_capabilities: CapabilitySet,
}

impl<'de> Deserialize<'de> for ClientHello {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ClientHelloWire::deserialize(deserializer)?;
        Self::new(
            wire.supported_versions,
            wire.required_capabilities,
            wire.optional_capabilities,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientHelloError {
    OverlappingCapability(Capability),
}

impl fmt::Display for ClientHelloError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OverlappingCapability(capability) => write!(
                formatter,
                "capability `{capability}` cannot be both required and optional"
            ),
        }
    }
}

impl std::error::Error for ClientHelloError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HostHello {
    selected_version: ProtocolVersion,
    capabilities: CapabilitySet,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bulk_connection_token: Option<String>,
}

impl HostHello {
    pub fn new(selected_version: ProtocolVersion, capabilities: CapabilitySet) -> Self {
        Self {
            selected_version,
            capabilities,
            bulk_connection_token: None,
        }
    }

    pub fn with_bulk_connection_token(mut self, token: String) -> Self {
        self.bulk_connection_token = Some(token);
        self
    }

    pub fn selected_version(&self) -> ProtocolVersion {
        self.selected_version
    }

    pub fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    pub fn bulk_connection_token(&self) -> Option<&str> {
        self.bulk_connection_token.as_deref()
    }
}

/// Messages sent from a client to the code-mode host.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all_fields = "camelCase")]
pub enum ClientToHost {
    #[serde(rename = "connection/hello")]
    ClientHello(ClientHello),
    #[serde(rename = "operation/request")]
    Request { id: RequestId, request: HostRequest },
    #[serde(rename = "operation/cancel")]
    CancelRequest { id: RequestId },
    #[serde(rename = "delegate/response")]
    DelegateResponse {
        id: DelegateRequestId,
        result: WireResult<DelegateResponse>,
    },
}

impl ClientToHost {
    /// Keeps notification acknowledgments with control traffic and tool results on the bulk lane.
    pub fn transport_lane(&self) -> TransportLane {
        match self {
            Self::DelegateResponse {
                result:
                    WireResult::Ok {
                        value: DelegateResponse::NotificationDelivered,
                    },
                ..
            }
            | Self::ClientHello(_)
            | Self::Request { .. }
            | Self::CancelRequest { .. } => TransportLane::Control,
            Self::DelegateResponse { .. } => TransportLane::Bulk,
        }
    }

    /// Validates the message families accepted by each paired socket.
    pub fn allows_transport_lane(&self, lane: TransportLane) -> bool {
        self.transport_lane() == lane
    }
}

/// Messages sent from the code-mode host to a client.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all_fields = "camelCase")]
pub enum HostToClient {
    #[serde(rename = "connection/ready")]
    HostHello(HostHello),
    #[serde(rename = "connection/rejected")]
    HandshakeRejected { reason: HandshakeRejectReason },
    #[serde(rename = "operation/response")]
    Response {
        id: RequestId,
        result: WireResult<HostResponse>,
    },
    #[serde(rename = "execute/initialResponse")]
    InitialResponse {
        id: RequestId,
        result: WireResult<WireRuntimeResponse>,
    },
    #[serde(rename = "delegate/request")]
    DelegateRequest {
        id: DelegateRequestId,
        session_id: SessionId,
        request: DelegateRequest,
    },
    #[serde(rename = "delegate/cancel")]
    CancelDelegateRequest { id: DelegateRequestId },
    #[serde(rename = "cell/closed")]
    CellClosed {
        session_id: SessionId,
        cell_id: WireCellId,
    },
    #[serde(rename = "native/progress")]
    NativeProgress {
        id: RequestId,
        session_id: SessionId,
        thread_id: String,
        run_id: String,
        phase: NativeProgressPhase,
    },
}

impl HostToClient {
    /// Keeps notifications with control traffic and nested-tool callbacks on the bulk lane.
    pub fn transport_lane(&self) -> TransportLane {
        match self {
            Self::DelegateRequest {
                request:
                    DelegateRequest::InvokeTool { .. } | DelegateRequest::NativeInvokeTool { .. },
                ..
            }
            | Self::CancelDelegateRequest { .. } => TransportLane::Bulk,
            Self::DelegateRequest {
                request: DelegateRequest::Notify { .. },
                ..
            }
            | Self::HostHello(_)
            | Self::HandshakeRejected { .. }
            | Self::Response { .. }
            | Self::InitialResponse { .. }
            | Self::CellClosed { .. }
            | Self::NativeProgress { .. } => TransportLane::Control,
        }
    }

    /// Rejects messages received on the wrong paired socket.
    pub fn allows_transport_lane(&self, lane: TransportLane) -> bool {
        self.transport_lane() == lane
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "method", rename_all_fields = "camelCase")]
pub enum HostRequest {
    #[serde(rename = "session/open")]
    OpenSession {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell_execution_limits: Option<WireSessionCellExecutionLimits>,
    },
    #[serde(rename = "session/execute")]
    Execute {
        session_id: SessionId,
        request: WireExecuteRequest,
    },
    #[serde(rename = "session/wait")]
    Wait {
        session_id: SessionId,
        request: WireWaitRequest,
    },
    #[serde(rename = "session/terminate")]
    Terminate {
        session_id: SessionId,
        cell_id: WireCellId,
    },
    #[serde(rename = "session/shutdown")]
    ShutdownSession { session_id: SessionId },
    #[serde(rename = "native/execute")]
    NativeExecute { request: NativeExecuteRequest },
    #[serde(rename = "native/finalize")]
    NativeFinalize {
        session_id: SessionId,
        thread_id: String,
        run_id: String,
    },
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all_fields = "camelCase")]
pub enum HostResponse {
    #[serde(rename = "session/ready")]
    SessionReady { session_id: SessionId },
    #[serde(rename = "execution/started")]
    ExecutionStarted { cell_id: WireCellId },
    #[serde(rename = "wait/completed")]
    WaitCompleted { outcome: WireWaitOutcome },
    #[serde(rename = "session/closed")]
    SessionClosed { session_id: SessionId },
    #[serde(rename = "native/completed")]
    NativeCompleted {
        session_id: SessionId,
        thread_id: String,
        run_id: String,
        source_hash: String,
        evidence: Box<NativeEvidence>,
    },
    #[serde(rename = "native/failed")]
    NativeFailed {
        session_id: SessionId,
        thread_id: String,
        run_id: String,
        failure: NativeFailure,
    },
    #[serde(rename = "native/finalized")]
    NativeFinalized {
        session_id: SessionId,
        thread_id: String,
        run_id: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NativeProgressPhase {
    WorkflowStarted,
    CompilerStarted { pid: u32 },
    Compiled,
    WorkflowProcessStarted { pid: u32 },
    DescendantStarted { pid: u32 },
    Finished,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NativeExecuteRequest {
    pub session_id: SessionId,
    pub thread_id: String,
    pub run_id: String,
    pub attempt: u8,
    pub task: String,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NativeEvidence {
    pub version: u16,
    pub summary: String,
    pub verified: Vec<String>,
    pub disputed: Vec<String>,
    pub unresolved: Vec<String>,
    pub artifact_refs: Vec<String>,
    pub partial_failures: Vec<String>,
    pub provenance_ids: Vec<String>,
}

impl NativeEvidence {
    pub fn exact_json_wire_len(&self) -> Result<usize, serde_json::Error> {
        serde_json::to_vec(self).map(|bytes| bytes.len())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NativeFailure {
    pub kind: String,
    pub source_hash: String,
    pub diagnostic: String,
    pub process_reaped: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "tool", rename_all_fields = "camelCase")]
pub enum NativeToolRequest {
    #[serde(rename = "shell")]
    Shell {
        command: String,
        workdir: Option<String>,
        timeout_ms: u32,
    },
    #[serde(rename = "applyPatch")]
    ApplyPatch { patch: String },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "status", rename_all_fields = "camelCase")]
pub enum NativeToolOutcome {
    #[serde(rename = "success")]
    Success { output: Vec<u8> },
    #[serde(rename = "retry")]
    Retry { reason: String },
    #[serde(rename = "failure")]
    Failure { message: String },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all_fields = "camelCase")]
pub enum DelegateRequest {
    #[serde(rename = "tool/invoke")]
    InvokeTool { invocation: WireNestedToolCall },
    #[serde(rename = "notification/send")]
    Notify {
        call_id: String,
        cell_id: WireCellId,
        text: String,
    },
    #[serde(rename = "native/tool/invoke")]
    NativeInvokeTool {
        run_id: String,
        call_id: String,
        request: NativeToolRequest,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all_fields = "camelCase")]
pub enum DelegateResponse {
    #[serde(rename = "tool/result")]
    ToolResult { result: JsonValue },
    #[serde(rename = "notification/delivered")]
    NotificationDelivered,
    #[serde(rename = "native/tool/result")]
    NativeToolResult { outcome: NativeToolOutcome },
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "status", rename_all_fields = "camelCase")]
pub enum WireResult<T> {
    #[serde(rename = "ok")]
    Ok { value: T },
    #[serde(rename = "error")]
    Err { message: String },
}

impl<T> WireResult<T> {
    pub fn from_result(result: Result<T, String>) -> Self {
        match result {
            Ok(value) => Self::Ok { value },
            Err(message) => Self::Err { message },
        }
    }

    pub fn into_result(self) -> Result<T, String> {
        match self {
            Self::Ok { value } => Ok(value),
            Self::Err { message } => Err(message),
        }
    }
}
