//! Thread event buffering and replay state for the TUI app.
//!
//! This module owns the per-thread event store used when the TUI switches between the main
//! conversation, subagents, and side conversations. It keeps buffered cli-runtime notifications,
//! pending interactive request replay state, active-turn tracking, and saved composer state close
//! together with the replay behavior that consumes them.

use super::*;
use std::borrow::Cow;

#[derive(Debug, Clone)]
pub(super) struct ThreadEventSnapshot {
    pub(super) session: Option<ThreadSessionState>,
    pub(super) turns: Vec<Turn>,
    pub(super) events: Vec<ThreadBufferedEvent>,
    pub(super) input_state: Option<ThreadInputState>,
}

#[derive(Debug, Clone)]
pub(super) enum ThreadBufferedEvent {
    Notification(Box<ServerNotification>),
    Request(Box<ServerRequest>),
    HistoryEntryResponse(HistoryLookupResponse),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ThreadEventAttachment {
    Live,
    ReplayOnly,
}

#[derive(Debug)]
pub(super) struct ThreadEventStore {
    pub(super) session: Option<ThreadSessionState>,
    pub(super) turns: Vec<Turn>,
    pub(super) buffer: VecDeque<ThreadBufferedEvent>,
    pub(super) pending_interactive_replay: PendingInteractiveReplayState,
    pub(super) active_turn_id: Option<String>,
    pub(super) pending_interrupt_turn_id: Option<String>,
    pub(super) input_state: Option<ThreadInputState>,
    pub(super) capacity: usize,
    pub(super) active: bool,
}

impl ThreadEventStore {
    pub(super) fn event_survives_session_refresh(event: &ThreadBufferedEvent) -> bool {
        match event {
            ThreadBufferedEvent::Request(_) => true,
            ThreadBufferedEvent::Notification(_) => false,
            ThreadBufferedEvent::HistoryEntryResponse(_) => false,
        }
    }

    pub(super) fn new(capacity: usize) -> Self {
        Self {
            session: None,
            turns: Vec::new(),
            buffer: VecDeque::new(),
            pending_interactive_replay: PendingInteractiveReplayState::default(),
            active_turn_id: None,
            pending_interrupt_turn_id: None,
            input_state: None,
            capacity,
            active: false,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn new_with_session(
        capacity: usize,
        session: ThreadSessionState,
        turns: Vec<Turn>,
    ) -> Self {
        let mut store = Self::new(capacity);
        store.session = Some(session);
        store.set_turns(turns);
        store
    }

    pub(super) fn set_session(&mut self, session: ThreadSessionState, turns: Vec<Turn>) {
        self.session = Some(session);
        self.set_turns(turns);
    }

    pub(super) fn rebase_buffer_after_session_refresh(&mut self) {
        self.buffer.retain(Self::event_survives_session_refresh);
    }

    pub(super) fn set_turns(&mut self, turns: Vec<Turn>) {
        self.active_turn_id = turns
            .iter()
            .rev()
            .find(|turn| matches!(turn.status, TurnStatus::InProgress))
            .map(|turn| turn.id.clone());
        self.turns = turns;
    }

    pub(super) fn push_notification(&mut self, notification: ServerNotification) {
        self.push_notification_inner(Cow::Owned(notification));
    }

    pub(super) fn push_notification_ref(&mut self, notification: &ServerNotification) {
        self.push_notification_inner(Cow::Borrowed(notification));
    }

    fn push_notification_inner(&mut self, notification: Cow<'_, ServerNotification>) {
        self.pending_interactive_replay
            .note_server_notification(notification.as_ref());
        match notification.as_ref() {
            ServerNotification::TurnStarted(turn) => {
                self.active_turn_id = Some(turn.turn.id.clone());
            }
            ServerNotification::TurnCompleted(turn) => {
                if self.active_turn_id.as_deref() == Some(turn.turn.id.as_str()) {
                    self.active_turn_id = None;
                }
                if self.pending_interrupt_turn_id.as_deref() == Some(turn.turn.id.as_str()) {
                    self.pending_interrupt_turn_id = None;
                }
            }
            ServerNotification::ThreadClosed(_) => {
                self.active_turn_id = None;
                self.pending_interrupt_turn_id = None;
            }
            _ => {}
        }

        // These notifications are either handled before routing or ignored by ChatWidget on
        // replay. In particular, raw response items and realtime audio can carry large payloads,
        // so cloning them into every thread's replay buffer only retains data the TUI cannot use.
        if matches!(
            notification.as_ref(),
            ServerNotification::RawResponseItemCompleted(_)
                | ServerNotification::FileChangePatchUpdated(_)
                | ServerNotification::ServerRequestResolved(_)
                | ServerNotification::ThreadRealtimeItemAdded(_)
                | ServerNotification::ThreadRealtimeOutputAudioDelta(_)
                | ServerNotification::ThreadRealtimeSdp(_)
                | ServerNotification::ThreadRealtimeTranscriptDelta(_)
                | ServerNotification::ThreadRealtimeTranscriptDone(_)
                | ServerNotification::CommandExecOutputDelta(_)
                | ServerNotification::ProcessOutputDelta(_)
                | ServerNotification::ProcessExited(_)
        ) {
            return;
        }

        self.buffer
            .push_back(ThreadBufferedEvent::Notification(Box::new(
                notification.into_owned(),
            )));
        if self.buffer.len() > self.capacity
            && let Some(removed) = self.buffer.pop_front()
            && let ThreadBufferedEvent::Request(request) = &removed
        {
            self.pending_interactive_replay
                .note_evicted_server_request(request.as_ref());
        }
    }

    pub(super) fn push_request(&mut self, request: ServerRequest) {
        self.pending_interactive_replay
            .note_server_request(&request);
        self.buffer
            .push_back(ThreadBufferedEvent::Request(Box::new(request)));
        if self.buffer.len() > self.capacity
            && let Some(removed) = self.buffer.pop_front()
            && let ThreadBufferedEvent::Request(request) = &removed
        {
            self.pending_interactive_replay
                .note_evicted_server_request(request.as_ref());
        }
    }

    pub(super) fn pending_replay_requests(&self) -> Vec<ServerRequest> {
        self.buffer
            .iter()
            .filter_map(|event| match event {
                ThreadBufferedEvent::Request(request)
                    if self
                        .pending_interactive_replay
                        .should_replay_snapshot_request(request.as_ref()) =>
                {
                    Some(request.as_ref().clone())
                }
                ThreadBufferedEvent::Request(_)
                | ThreadBufferedEvent::Notification(_)
                | ThreadBufferedEvent::HistoryEntryResponse(_) => None,
            })
            .collect()
    }

    pub(super) fn file_change_changes(
        &self,
        turn_id: &str,
        item_id: &str,
    ) -> Option<Vec<codex_cli_protocol::FileUpdateChange>> {
        self.buffer
            .iter()
            .rev()
            .find_map(|event| match event {
                ThreadBufferedEvent::Notification(notification) => match notification.as_ref() {
                    ServerNotification::ItemStarted(notification)
                        if turn_id_matches(turn_id, &notification.turn_id) =>
                    {
                        file_change_item_changes(&notification.item, item_id)
                    }
                    ServerNotification::ItemCompleted(notification)
                        if turn_id_matches(turn_id, &notification.turn_id) =>
                    {
                        file_change_item_changes(&notification.item, item_id)
                    }
                    _ => None,
                },
                ThreadBufferedEvent::Request(_) | ThreadBufferedEvent::HistoryEntryResponse(_) => {
                    None
                }
            })
            .or_else(|| {
                self.turns
                    .iter()
                    .rev()
                    .filter(|turn| turn_id_matches(turn_id, &turn.id))
                    .flat_map(|turn| turn.items.iter().rev())
                    .find_map(|item| file_change_item_changes(item, item_id))
            })
    }

    pub(super) fn snapshot(&self) -> ThreadEventSnapshot {
        ThreadEventSnapshot {
            session: self.session.clone(),
            turns: self.turns.clone(),
            // Thread switches replay buffered events into a rebuilt ChatWidget. Only replay
            // interactive prompts that are still pending, or answered approvals/input will reappear.
            events: self
                .buffer
                .iter()
                .filter(|event| match event {
                    ThreadBufferedEvent::Request(request) => self
                        .pending_interactive_replay
                        .should_replay_snapshot_request(request.as_ref()),
                    ThreadBufferedEvent::Notification(_)
                    | ThreadBufferedEvent::HistoryEntryResponse(_) => true,
                })
                .cloned()
                .collect(),
            input_state: self.input_state.clone(),
        }
    }

    pub(super) fn note_outbound_op<T>(&mut self, op: T)
    where
        T: Into<AppCommand>,
    {
        self.pending_interactive_replay.note_outbound_op(op);
    }

    pub(super) fn op_can_change_pending_replay_state<T>(op: T) -> bool
    where
        T: Into<AppCommand>,
    {
        PendingInteractiveReplayState::op_can_change_state(op)
    }

    pub(super) fn side_parent_pending_status(&self) -> Option<SideParentStatus> {
        if self
            .pending_interactive_replay
            .has_pending_thread_user_input()
        {
            Some(SideParentStatus::NeedsInput)
        } else {
            None
        }
    }

    pub(super) fn active_turn_id(&self) -> Option<&str> {
        self.active_turn_id.as_deref()
    }

    pub(super) fn clear_active_turn_id(&mut self) {
        self.active_turn_id = None;
    }
}

fn turn_id_matches(request_turn_id: &str, candidate_turn_id: &str) -> bool {
    request_turn_id.is_empty() || request_turn_id == candidate_turn_id
}

fn file_change_item_changes(
    item: &ThreadItem,
    item_id: &str,
) -> Option<Vec<codex_cli_protocol::FileUpdateChange>> {
    match item {
        ThreadItem::FileChange { id, changes, .. } if id == item_id => Some(changes.clone()),
        _ => None,
    }
}

#[derive(Debug)]
pub(super) struct ThreadEventChannel {
    pub(super) sender: mpsc::Sender<ThreadBufferedEvent>,
    pub(super) receiver: Option<mpsc::Receiver<ThreadBufferedEvent>>,
    pub(super) store: Arc<Mutex<ThreadEventStore>>,
    attachment: ThreadEventAttachment,
}

impl ThreadEventChannel {
    pub(super) fn new(capacity: usize) -> Self {
        let (sender, receiver) = mpsc::channel(capacity);
        Self {
            sender,
            receiver: Some(receiver),
            store: Arc::new(Mutex::new(ThreadEventStore::new(capacity))),
            attachment: ThreadEventAttachment::Live,
        }
    }

    pub(super) fn mark_replay_only(&mut self) {
        self.attachment = ThreadEventAttachment::ReplayOnly;
    }

    pub(super) fn attachment(&self) -> ThreadEventAttachment {
        self.attachment
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn new_with_session(
        capacity: usize,
        session: ThreadSessionState,
        turns: Vec<Turn>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(capacity);
        Self {
            sender,
            receiver: Some(receiver),
            store: Arc::new(Mutex::new(ThreadEventStore::new_with_session(
                capacity, session, turns,
            ))),
            attachment: ThreadEventAttachment::Live,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::PathBufExt;
    use crate::test_support::test_path_buf;
    use codex_cli_protocol::TurnCompletedNotification;
    use codex_cli_protocol::TurnStartedNotification;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn test_thread_session(thread_id: ThreadId, cwd: PathBuf) -> ThreadSessionState {
        ThreadSessionState {
            thread_id,
            forked_from_id: None,
            fork_parent_title: None,
            thread_name: None,
            model: "gpt-test".to_string(),
            model_provider_id: "test-provider".to_string(),
            service_tier: None,
            cwd: cwd.abs(),
            runtime_workspace_roots: Vec::new(),
            instruction_source_paths: Vec::new(),
            reasoning_effort: None,
            agent_settings: None,
            personality: None,
            message_history: None,
            rollout_path: Some(PathBuf::new()),
        }
    }

    fn test_turn(turn_id: &str, status: TurnStatus, items: Vec<ThreadItem>) -> Turn {
        Turn {
            id: turn_id.to_string(),
            items_view: codex_cli_protocol::TurnItemsView::Full,
            items,
            status,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }
    }

    fn turn_started_notification(thread_id: ThreadId, turn_id: &str) -> ServerNotification {
        ServerNotification::TurnStarted(TurnStartedNotification {
            thread_id: thread_id.to_string(),
            turn: Turn {
                started_at: Some(0),
                ..test_turn(turn_id, TurnStatus::InProgress, Vec::new())
            },
        })
    }

    fn turn_completed_notification(
        thread_id: ThreadId,
        turn_id: &str,
        status: TurnStatus,
    ) -> ServerNotification {
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: thread_id.to_string(),
            turn: Turn {
                completed_at: Some(0),
                duration_ms: Some(1),
                ..test_turn(turn_id, status, Vec::new())
            },
        })
    }

    #[test]
    fn thread_event_store_tracks_active_turn_lifecycle() {
        let mut store = ThreadEventStore::new(/*capacity*/ 8);
        assert_eq!(store.active_turn_id(), None);

        let thread_id = ThreadId::new();
        store.push_notification(turn_started_notification(thread_id, "turn-1"));
        assert_eq!(store.active_turn_id(), Some("turn-1"));

        store.push_notification(turn_completed_notification(
            thread_id,
            "turn-2",
            TurnStatus::Completed,
        ));
        assert_eq!(store.active_turn_id(), Some("turn-1"));

        store.push_notification(turn_completed_notification(
            thread_id,
            "turn-1",
            TurnStatus::Interrupted,
        ));
        assert_eq!(store.active_turn_id(), None);
    }

    #[test]
    fn thread_event_store_restores_active_turn_from_snapshot_turns() {
        let thread_id = ThreadId::new();
        let session = test_thread_session(thread_id, test_path_buf("/tmp/project"));
        let turns = vec![
            test_turn("turn-1", TurnStatus::Completed, Vec::new()),
            test_turn("turn-2", TurnStatus::InProgress, Vec::new()),
        ];

        let store =
            ThreadEventStore::new_with_session(/*capacity*/ 8, session.clone(), turns.clone());
        assert_eq!(store.active_turn_id(), Some("turn-2"));

        let mut refreshed_store = ThreadEventStore::new(/*capacity*/ 8);
        refreshed_store.set_session(session, turns);
        assert_eq!(refreshed_store.active_turn_id(), Some("turn-2"));
    }

    #[test]
    fn thread_event_store_clear_active_turn_id_resets_cached_turn() {
        let mut store = ThreadEventStore::new(/*capacity*/ 8);
        let thread_id = ThreadId::new();
        store.push_notification(turn_started_notification(thread_id, "turn-1"));

        store.clear_active_turn_id();

        assert_eq!(store.active_turn_id(), None);
    }
}
