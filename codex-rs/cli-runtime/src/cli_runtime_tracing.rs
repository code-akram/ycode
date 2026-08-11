//! Tracing helpers for the embedded CLI runtime.

use crate::message_processor::ConnectionSessionState;
use crate::outgoing_message::ConnectionId;
use codex_cli_protocol::ClientRequest;
use codex_otel::set_parent_from_context;
use codex_otel::set_parent_from_w3c_trace_context;
use codex_otel::traceparent_context_from_env;
use codex_protocol::protocol::W3cTraceContext;
use tracing::Span;
use tracing::field;
use tracing::info_span;

/// Builds tracing span metadata for typed in-process requests.
///
/// This mirrors `request_span` semantics while stamping transport as
/// `in-process` and deriving client info either from initialize params or
/// from existing connection session state.
pub(crate) fn typed_request_span(
    request: &ClientRequest,
    connection_id: ConnectionId,
    session: &ConnectionSessionState,
) -> Span {
    let method = request.method_name();
    let span = cli_runtime_request_span_template(method, "in-process", request.id(), connection_id);

    let client_info = initialize_client_info_from_typed_request(request);
    record_client_info(
        &span,
        client_info
            .map(|(client_name, _)| client_name)
            .or(session.cli_runtime_client_name()),
        client_info
            .map(|(_, client_version)| client_version)
            .or(session.client_version()),
    );

    attach_parent_context(&span, method, request.id(), /*parent_trace*/ None);
    span
}

fn cli_runtime_request_span_template(
    method: &str,
    transport: &'static str,
    request_id: &impl std::fmt::Display,
    connection_id: ConnectionId,
) -> Span {
    info_span!(
        "cli_runtime.request",
        otel.kind = "server",
        otel.name = method,
        rpc.system = "jsonrpc",
        rpc.method = method,
        rpc.transport = transport,
        rpc.request_id = %request_id,
        cli_runtime.connection_id = %connection_id,
        cli_runtime.api_version = "v2",
        cli_runtime.client_name = field::Empty,
        cli_runtime.client_version = field::Empty,
        turn.id = field::Empty,
    )
}

fn record_client_info(span: &Span, client_name: Option<&str>, client_version: Option<&str>) {
    if let Some(client_name) = client_name {
        span.record("cli_runtime.client_name", client_name);
    }
    if let Some(client_version) = client_version {
        span.record("cli_runtime.client_version", client_version);
    }
}

fn attach_parent_context(
    span: &Span,
    method: &str,
    request_id: &impl std::fmt::Display,
    parent_trace: Option<&W3cTraceContext>,
) {
    if let Some(trace) = parent_trace {
        if !set_parent_from_w3c_trace_context(span, trace) {
            tracing::warn!(
                rpc_method = method,
                rpc_request_id = %request_id,
                "ignoring invalid inbound request trace carrier"
            );
        }
    } else if let Some(context) = traceparent_context_from_env() {
        set_parent_from_context(span, context);
    }
}

fn initialize_client_info_from_typed_request(request: &ClientRequest) -> Option<(&str, &str)> {
    match request {
        ClientRequest::Initialize { params, .. } => Some((
            params.client_info.name.as_str(),
            params.client_info.version.as_str(),
        )),
        _ => None,
    }
}
