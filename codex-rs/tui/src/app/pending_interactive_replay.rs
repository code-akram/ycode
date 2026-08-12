use crate::app_command::AppCommand;
use codex_cli_protocol::RequestId as CliRuntimeRequestId;
use codex_cli_protocol::ServerNotification;
use codex_cli_protocol::ServerRequest;
use std::collections::HashMap;
use std::collections::HashSet;

#[derive(Debug, Default)]
// Tracks which interactive prompts are still unresolved in the thread-event buffer.
//
// Thread snapshots are replayed when switching threads/agents. Most events should replay
// verbatim, but request_user_input prompts must
// only replay if they are still pending. This state is updated from:
// - inbound events (`note_event`)
// - outbound ops that resolve a prompt (`note_outbound_op`)
// - buffer eviction (`note_evicted_event`)
//
// We keep both fast lookup sets (for snapshot filtering by call_id/request key) and
// turn-indexed queues/vectors so turn completion or interruption can clear
// stale prompts tied to a turn. `request_user_input` removal is FIFO because
// the overlay answers queued prompts in FIFO order for a shared `turn_id`.
pub(super) struct PendingInteractiveReplayState {
    request_user_input_call_ids: HashSet<String>,
    request_user_input_call_ids_by_turn_id: HashMap<String, Vec<String>>,
    pending_requests_by_request_id: HashMap<CliRuntimeRequestId, PendingInteractiveRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingInteractiveRequest {
    RequestUserInput { turn_id: String, item_id: String },
}

impl PendingInteractiveReplayState {
    pub(super) fn op_can_change_state<T>(op: T) -> bool
    where
        T: Into<AppCommand>,
    {
        let op: AppCommand = op.into();
        matches!(&op, AppCommand::UserInputAnswer { .. })
    }

    pub(super) fn note_outbound_op<T>(&mut self, op: T)
    where
        T: Into<AppCommand>,
    {
        let op: AppCommand = op.into();
        match &op {
            // `Op::UserInputAnswer` identifies the turn, not the prompt call_id. The UI
            // answers queued prompts for the same turn in FIFO order, so remove the oldest
            // queued call_id for that turn.
            AppCommand::UserInputAnswer { id, .. } => {
                let mut remove_turn_entry = false;
                if let Some(call_ids) = self.request_user_input_call_ids_by_turn_id.get_mut(id) {
                    if !call_ids.is_empty() {
                        let call_id = call_ids.remove(0);
                        self.request_user_input_call_ids.remove(&call_id);
                        self.pending_requests_by_request_id.retain(
                            |_, pending| {
                                !matches!(pending, PendingInteractiveRequest::RequestUserInput { item_id, .. } if *item_id == call_id)
                            },
                        );
                    }
                    if call_ids.is_empty() {
                        remove_turn_entry = true;
                    }
                }
                if remove_turn_entry {
                    self.request_user_input_call_ids_by_turn_id.remove(id);
                }
            }
            _ => {}
        }
    }

    pub(super) fn note_server_request(&mut self, request: &ServerRequest) {
        match request {
            ServerRequest::ToolRequestUserInput { request_id, params } => {
                self.request_user_input_call_ids
                    .insert(params.item_id.clone());
                self.request_user_input_call_ids_by_turn_id
                    .entry(params.turn_id.clone())
                    .or_default()
                    .push(params.item_id.clone());
                self.pending_requests_by_request_id.insert(
                    request_id.clone(),
                    PendingInteractiveRequest::RequestUserInput {
                        turn_id: params.turn_id.clone(),
                        item_id: params.item_id.clone(),
                    },
                );
            }
            _ => {}
        }
    }

    pub(super) fn note_server_notification(&mut self, notification: &ServerNotification) {
        match notification {
            ServerNotification::TurnCompleted(notification) => {
                self.clear_request_user_input_turn(&notification.turn.id);
            }
            ServerNotification::ServerRequestResolved(notification) => {
                self.remove_request(&notification.request_id);
            }
            ServerNotification::ThreadClosed(_) => self.clear(),
            _ => {}
        }
    }

    pub(super) fn note_evicted_server_request(&mut self, request: &ServerRequest) {
        match request {
            ServerRequest::ToolRequestUserInput { params, .. } => {
                self.request_user_input_call_ids.remove(&params.item_id);
                let mut remove_turn_entry = false;
                if let Some(call_ids) = self
                    .request_user_input_call_ids_by_turn_id
                    .get_mut(&params.turn_id)
                {
                    call_ids.retain(|call_id| call_id != &params.item_id);
                    if call_ids.is_empty() {
                        remove_turn_entry = true;
                    }
                }
                if remove_turn_entry {
                    self.request_user_input_call_ids_by_turn_id
                        .remove(&params.turn_id);
                }
            }
            _ => {}
        }
        self.pending_requests_by_request_id
            .retain(|_, pending| !Self::request_matches_server_request(pending, request));
    }

    pub(super) fn should_replay_snapshot_request(&self, request: &ServerRequest) -> bool {
        match request {
            ServerRequest::ToolRequestUserInput { params, .. } => {
                self.request_user_input_call_ids.contains(&params.item_id)
            }
            _ => true,
        }
    }

    pub(super) fn has_pending_thread_user_input(&self) -> bool {
        !self.request_user_input_call_ids.is_empty()
    }

    fn clear_request_user_input_turn(&mut self, turn_id: &str) {
        if let Some(call_ids) = self.request_user_input_call_ids_by_turn_id.remove(turn_id) {
            for call_id in call_ids {
                self.request_user_input_call_ids.remove(&call_id);
            }
        }
        self.pending_requests_by_request_id.retain(
            |_, pending| {
                !matches!(pending, PendingInteractiveRequest::RequestUserInput { turn_id: pending_turn_id, .. } if pending_turn_id == turn_id)
            },
        );
    }

    #[allow(dead_code)] // Retained compatibility, test, or architectural seam for non-default consumers.
    fn remove_call_id_from_turn_map(
        call_ids_by_turn_id: &mut HashMap<String, Vec<String>>,
        call_id: &str,
    ) {
        call_ids_by_turn_id.retain(|_, call_ids| {
            call_ids.retain(|queued_call_id| queued_call_id != call_id);
            !call_ids.is_empty()
        });
    }

    fn remove_call_id_from_turn_map_entry(
        call_ids_by_turn_id: &mut HashMap<String, Vec<String>>,
        turn_id: &str,
        call_id: &str,
    ) {
        let mut remove_turn_entry = false;
        if let Some(call_ids) = call_ids_by_turn_id.get_mut(turn_id) {
            call_ids.retain(|queued_call_id| queued_call_id != call_id);
            if call_ids.is_empty() {
                remove_turn_entry = true;
            }
        }
        if remove_turn_entry {
            call_ids_by_turn_id.remove(turn_id);
        }
    }

    fn clear(&mut self) {
        self.request_user_input_call_ids.clear();
        self.request_user_input_call_ids_by_turn_id.clear();
        self.pending_requests_by_request_id.clear();
    }

    fn remove_request(&mut self, request_id: &CliRuntimeRequestId) {
        let Some(pending) = self.pending_requests_by_request_id.remove(request_id) else {
            return;
        };
        match pending {
            PendingInteractiveRequest::RequestUserInput { turn_id, item_id } => {
                self.request_user_input_call_ids.remove(&item_id);
                Self::remove_call_id_from_turn_map_entry(
                    &mut self.request_user_input_call_ids_by_turn_id,
                    &turn_id,
                    &item_id,
                );
            }
        }
    }

    fn request_matches_server_request(
        pending: &PendingInteractiveRequest,
        request: &ServerRequest,
    ) -> bool {
        match (pending, request) {
            (
                PendingInteractiveRequest::RequestUserInput { turn_id, item_id },
                ServerRequest::ToolRequestUserInput { params, .. },
            ) => turn_id == &params.turn_id && item_id == &params.item_id,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ThreadBufferedEvent;
    use super::super::ThreadEventStore;
    use crate::app_command::AppCommand as Op;
    use codex_cli_protocol::RequestId as CliRuntimeRequestId;
    use codex_cli_protocol::ServerNotification;
    use codex_cli_protocol::ServerRequest;
    use codex_cli_protocol::ServerRequestResolvedNotification;
    use codex_cli_protocol::ThreadClosedNotification;
    use codex_cli_protocol::ToolRequestUserInputParams;
    use codex_cli_protocol::ToolRequestUserInputResponse;
    use codex_cli_protocol::Turn;
    use codex_cli_protocol::TurnCompletedNotification;
    use codex_cli_protocol::TurnStatus;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;

    fn request_user_input_request(call_id: &str, turn_id: &str) -> ServerRequest {
        ServerRequest::ToolRequestUserInput {
            request_id: CliRuntimeRequestId::Integer(1),
            params: ToolRequestUserInputParams {
                thread_id: "thread-1".to_string(),
                turn_id: turn_id.to_string(),
                item_id: call_id.to_string(),
                questions: Vec::new(),
                is_blocking: true,
                auto_resolution_ms: None,
            },
        }
    }

    fn turn_completed(turn_id: &str) -> ServerNotification {
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: Turn {
                id: turn_id.to_string(),
                items_view: codex_cli_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: Some(1),
            },
        })
    }

    fn thread_closed() -> ServerNotification {
        ServerNotification::ThreadClosed(ThreadClosedNotification {
            thread_id: "thread-1".to_string(),
        })
    }

    fn request_resolved(request_id: CliRuntimeRequestId) -> ServerNotification {
        ServerNotification::ServerRequestResolved(ServerRequestResolvedNotification {
            thread_id: "thread-1".to_string(),
            request_id,
        })
    }

    #[test]
    fn thread_event_snapshot_keeps_pending_request_user_input() {
        let mut store = ThreadEventStore::new(/*capacity*/ 8);
        let request = request_user_input_request("call-1", "turn-1");

        store.push_request(request);

        let snapshot = store.snapshot();
        assert_eq!(snapshot.events.len(), 1);
        assert!(matches!(
            snapshot.events.first(),
            Some(ThreadBufferedEvent::Request(request))
                if matches!(
                    request.as_ref(),
                    ServerRequest::ToolRequestUserInput { params, .. }
                        if params.item_id == "call-1"
                )
        ));
    }

    #[test]
    fn thread_event_snapshot_drops_resolved_request_user_input_after_user_answer() {
        let mut store = ThreadEventStore::new(/*capacity*/ 8);
        store.push_request(request_user_input_request("call-1", "turn-1"));

        store.note_outbound_op(&Op::UserInputAnswer {
            id: "turn-1".to_string(),
            response: ToolRequestUserInputResponse {
                answers: HashMap::new(),
            },
        });

        let snapshot = store.snapshot();
        assert!(
            snapshot.events.is_empty(),
            "resolved request_user_input prompt should not replay on thread switch"
        );
    }

    #[test]
    fn thread_event_snapshot_drops_resolved_request_user_input_after_server_resolution() {
        let mut store = ThreadEventStore::new(/*capacity*/ 8);
        store.push_request(request_user_input_request("call-1", "turn-1"));

        store.push_notification(request_resolved(CliRuntimeRequestId::Integer(1)));

        let snapshot = store.snapshot();
        assert!(
            snapshot.events.iter().all(|event| {
                !matches!(
                    event,
                    ThreadBufferedEvent::Request(request)
                        if matches!(
                            request.as_ref(),
                            ServerRequest::ToolRequestUserInput { .. }
                        )
                )
            }),
            "server-resolved request_user_input prompt should not replay on thread switch"
        );
    }

    #[test]
    fn thread_event_snapshot_drops_answered_request_user_input_for_multi_prompt_turn() {
        let mut store = ThreadEventStore::new(/*capacity*/ 8);
        store.push_request(request_user_input_request("call-1", "turn-1"));

        store.note_outbound_op(&Op::UserInputAnswer {
            id: "turn-1".to_string(),
            response: ToolRequestUserInputResponse {
                answers: HashMap::new(),
            },
        });

        store.push_request(request_user_input_request("call-2", "turn-1"));

        let snapshot = store.snapshot();
        assert_eq!(snapshot.events.len(), 1);
        assert!(matches!(
            snapshot.events.first(),
            Some(ThreadBufferedEvent::Request(request))
                if matches!(
                    request.as_ref(),
                    ServerRequest::ToolRequestUserInput { params, .. }
                        if params.item_id == "call-2"
                )
        ));
    }

    #[test]
    fn thread_event_snapshot_keeps_newer_request_user_input_pending_when_same_turn_has_queue() {
        let mut store = ThreadEventStore::new(/*capacity*/ 8);
        store.push_request(request_user_input_request("call-1", "turn-1"));
        store.push_request(request_user_input_request("call-2", "turn-1"));

        store.note_outbound_op(&Op::UserInputAnswer {
            id: "turn-1".to_string(),
            response: ToolRequestUserInputResponse {
                answers: HashMap::new(),
            },
        });

        let snapshot = store.snapshot();
        assert_eq!(snapshot.events.len(), 1);
        assert!(matches!(
            snapshot.events.first(),
            Some(ThreadBufferedEvent::Request(request))
                if matches!(
                    request.as_ref(),
                    ServerRequest::ToolRequestUserInput { params, .. }
                        if params.item_id == "call-2"
                )
        ));
    }

    #[test]
    fn thread_event_snapshot_drops_pending_requests_when_thread_closes() {
        let mut store = ThreadEventStore::new(/*capacity*/ 8);
        store.push_request(request_user_input_request("call-1", "turn-1"));
        store.push_notification(thread_closed());

        assert!(store.snapshot().events.iter().all(|event| {
            !matches!(
                event,
                ThreadBufferedEvent::Request(request)
                    if matches!(
                        request.as_ref(),
                        ServerRequest::ToolRequestUserInput { .. }
                    )
            )
        }));
    }
}
