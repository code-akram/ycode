use crate::outgoing_message::OutgoingEnvelope;
use codex_cli_protocol::ClientResponsePayload;
use codex_cli_protocol::ExperimentalApi;
use codex_cli_protocol::JSONRPCErrorError;
use codex_cli_protocol::RequestId;
use codex_cli_protocol::ServerNotificationEnvelope;
use codex_cli_protocol::ServerRequest;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::warn;

pub(crate) const CHANNEL_CAPACITY: usize = 128;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConnectionId(pub(crate) u64);

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum OutgoingMessage {
    Request(ServerRequest),
    CliRuntimeNotification(ServerNotificationEnvelope),
    Response(OutgoingResponse),
    Error(OutgoingError),
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct OutgoingResponse {
    pub(crate) id: RequestId,
    pub(crate) result: Box<ClientResponsePayload>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OutgoingError {
    pub(crate) error: JSONRPCErrorError,
    pub(crate) id: RequestId,
}

#[derive(Debug)]
pub(crate) struct QueuedOutgoingMessage {
    pub(crate) message: OutgoingMessage,
    pub(crate) write_complete_tx: Option<oneshot::Sender<()>>,
}

pub(crate) struct OutboundConnectionState {
    pub(crate) initialized: Arc<AtomicBool>,
    pub(crate) experimental_api_enabled: Arc<AtomicBool>,
    pub(crate) opted_out_notification_methods: Arc<RwLock<HashSet<String>>>,
    pub(crate) writer: mpsc::Sender<QueuedOutgoingMessage>,
    disconnect_sender: Option<CancellationToken>,
}

impl OutboundConnectionState {
    pub(crate) fn new(
        writer: mpsc::Sender<QueuedOutgoingMessage>,
        initialized: Arc<AtomicBool>,
        experimental_api_enabled: Arc<AtomicBool>,
        opted_out_notification_methods: Arc<RwLock<HashSet<String>>>,
        disconnect_sender: Option<CancellationToken>,
    ) -> Self {
        Self {
            initialized,
            experimental_api_enabled,
            opted_out_notification_methods,
            writer,
            disconnect_sender,
        }
    }

    fn can_disconnect(&self) -> bool {
        self.disconnect_sender.is_some()
    }

    pub(crate) fn request_disconnect(&self) {
        if let Some(disconnect_sender) = &self.disconnect_sender {
            disconnect_sender.cancel();
        }
    }
}

fn should_skip_notification_for_connection(
    connection_state: &OutboundConnectionState,
    message: &OutgoingMessage,
) -> bool {
    let Ok(opted_out_notification_methods) = connection_state.opted_out_notification_methods.read()
    else {
        warn!("failed to read outbound opted-out notifications");
        return false;
    };
    match message {
        OutgoingMessage::CliRuntimeNotification(envelope) => {
            if envelope.notification.experimental_reason().is_some()
                && !connection_state
                    .experimental_api_enabled
                    .load(Ordering::Acquire)
            {
                return true;
            }
            let method = envelope.notification.to_string();
            opted_out_notification_methods.contains(method.as_str())
        }
        _ => false,
    }
}

fn disconnect_connection(
    connections: &mut HashMap<ConnectionId, OutboundConnectionState>,
    connection_id: ConnectionId,
) -> bool {
    if let Some(connection_state) = connections.remove(&connection_id) {
        connection_state.request_disconnect();
        return true;
    }
    false
}

async fn send_message_to_connection(
    connections: &mut HashMap<ConnectionId, OutboundConnectionState>,
    connection_id: ConnectionId,
    message: OutgoingMessage,
    write_complete_tx: Option<tokio::sync::oneshot::Sender<()>>,
) -> bool {
    let Some(connection_state) = connections.get(&connection_id) else {
        warn!("dropping message for disconnected connection: {connection_id:?}");
        return false;
    };
    let message = filter_outgoing_message_for_connection(connection_state, message);
    if should_skip_notification_for_connection(connection_state, &message) {
        return false;
    }

    let writer = connection_state.writer.clone();
    let queued_message = QueuedOutgoingMessage {
        message,
        write_complete_tx,
    };
    if connection_state.can_disconnect() {
        match writer.try_send(queued_message) {
            Ok(()) => false,
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(
                    "disconnecting slow connection after outbound queue filled: {connection_id:?}"
                );
                disconnect_connection(connections, connection_id)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                disconnect_connection(connections, connection_id)
            }
        }
    } else if writer.send(queued_message).await.is_err() {
        disconnect_connection(connections, connection_id)
    } else {
        false
    }
}

fn filter_outgoing_message_for_connection(
    connection_state: &OutboundConnectionState,
    message: OutgoingMessage,
) -> OutgoingMessage {
    let experimental_api_enabled = connection_state
        .experimental_api_enabled
        .load(Ordering::Acquire);
    match message {
        OutgoingMessage::Request(ServerRequest::CommandExecutionRequestApproval {
            request_id,
            mut params,
        }) => {
            if !experimental_api_enabled {
                params.strip_experimental_fields();
            }
            OutgoingMessage::Request(ServerRequest::CommandExecutionRequestApproval {
                request_id,
                params,
            })
        }
        _ => message,
    }
}

pub(crate) async fn route_outgoing_envelope(
    connections: &mut HashMap<ConnectionId, OutboundConnectionState>,
    envelope: OutgoingEnvelope,
) {
    match envelope {
        OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            write_complete_tx,
        } => {
            let _ =
                send_message_to_connection(connections, connection_id, message, write_complete_tx)
                    .await;
        }
        OutgoingEnvelope::Broadcast { message } => {
            let target_connections: Vec<ConnectionId> = connections
                .iter()
                .filter_map(|(connection_id, connection_state)| {
                    if connection_state.initialized.load(Ordering::Acquire)
                        && !should_skip_notification_for_connection(connection_state, &message)
                    {
                        Some(*connection_id)
                    } else {
                        None
                    }
                })
                .collect();

            for connection_id in target_connections {
                let _ = send_message_to_connection(
                    connections,
                    connection_id,
                    message.clone(),
                    /*write_complete_tx*/ None,
                )
                .await;
            }
        }
    }
}
