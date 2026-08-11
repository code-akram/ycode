mod file_system_handler;
mod handler;
mod process_handler;
mod processor;
mod registry;
mod request_dispatcher;
mod session_registry;
mod transport;

pub(crate) use handler::ExecServerHandler;
pub(crate) use processor::ConnectionProcessor;
pub use request_dispatcher::ConcurrentRequestLimit;
pub use request_dispatcher::RequestDispatchMode;
pub use transport::DEFAULT_LISTEN_URL;
pub use transport::ExecServerListenUrlParseError;

use crate::ExecServerRuntimePaths;
use crate::ExecServerTelemetry;
use codex_http_client::HttpClientFactory;

pub async fn run_main(
    listen_url: &str,
    runtime_paths: ExecServerRuntimePaths,
    http_client_factory: HttpClientFactory,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_main_with_telemetry(
        listen_url,
        runtime_paths,
        ExecServerTelemetry::default(),
        http_client_factory,
        RequestDispatchMode::Inline,
    )
    .await
}

#[tracing::instrument(
    name = "codex.exec_server",
    skip_all,
    fields(otel.kind = "internal")
)]
pub async fn run_main_with_telemetry(
    listen_url: &str,
    runtime_paths: ExecServerRuntimePaths,
    telemetry: ExecServerTelemetry,
    http_client_factory: HttpClientFactory,
    request_dispatch_mode: RequestDispatchMode,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    transport::run_transport(
        listen_url,
        runtime_paths,
        telemetry,
        http_client_factory,
        request_dispatch_mode,
    )
    .await
}
