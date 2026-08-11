use std::time::Duration;
use std::time::Instant;

use tracing::debug;

#[derive(Clone, Copy, Debug)]
pub(crate) enum ConnectionTransport {
    Relay,
    Stdio,
    WebSocket,
}

impl ConnectionTransport {
    fn as_str(self) -> &'static str {
        match self {
            Self::Relay => "relay",
            Self::Stdio => "stdio",
            Self::WebSocket => "websocket",
        }
    }
}

/// Local-only exec-server diagnostics. These events use the normal tracing subscriber and are
/// never exported or batched for remote reporting.
#[derive(Clone, Default)]
pub struct ExecServerTelemetry;

pub(crate) struct ConnectionMetricGuard {
    transport: ConnectionTransport,
}

pub(crate) struct ProcessMetricGuard {
    started_at: Instant,
    result: &'static str,
}

impl ExecServerTelemetry {
    pub(crate) fn connection_started(
        &self,
        transport: ConnectionTransport,
    ) -> ConnectionMetricGuard {
        debug!(
            transport = transport.as_str(),
            "exec-server connection started"
        );
        ConnectionMetricGuard { transport }
    }

    pub(crate) fn request_completed(&self, method: &str, result: &'static str, duration: Duration) {
        debug!(
            method,
            result,
            duration_ms = duration.as_millis(),
            "exec-server request completed"
        );
    }

    pub(crate) fn remote_registration_completed(&self, result: &'static str, duration: Duration) {
        debug!(
            result,
            duration_ms = duration.as_millis(),
            "exec-server registration completed"
        );
    }

    pub(crate) fn remote_rendezvous_completed(&self, result: &'static str, duration: Duration) {
        debug!(
            result,
            duration_ms = duration.as_millis(),
            "exec-server rendezvous completed"
        );
    }

    pub(crate) fn remote_reconnect(&self, reason: &'static str) {
        debug!(reason, "exec-server reconnecting");
    }

    pub(crate) fn process_started(&self) -> ProcessMetricGuard {
        debug!("exec-server process started");
        ProcessMetricGuard {
            started_at: Instant::now(),
            result: "unknown",
        }
    }
}

impl Drop for ConnectionMetricGuard {
    fn drop(&mut self) {
        debug!(
            transport = self.transport.as_str(),
            "exec-server connection ended"
        );
    }
}

impl ProcessMetricGuard {
    pub(crate) fn finish(mut self, result: &'static str) {
        self.result = result;
    }
}

impl Drop for ProcessMetricGuard {
    fn drop(&mut self) {
        debug!(
            result = self.result,
            duration_ms = self.started_at.elapsed().as_millis(),
            "exec-server process ended"
        );
    }
}
