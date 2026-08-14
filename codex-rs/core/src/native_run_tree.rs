use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

pub use codex_code_mode::NativeRunIdentity;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub const NATIVE_RUN_TREE_MAX_NODES: usize = 64;
pub const NATIVE_RUN_TREE_SUMMARY_BYTES: usize = 256;
pub const NATIVE_RUN_TREE_RECENT_BYTES: usize = 1024;
pub const NATIVE_RUN_TREE_MAX_REFS: usize = 16;
pub const NATIVE_RUN_TREE_REF_BYTES: usize = 512;
const LAUNCH_ORDINAL_STRIDE: u64 = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeRunNodeKind {
    Run,
    Generation,
    Compile { attempt: u8, pid: Option<u32> },
    Repair,
    Workflow { attempt: u8, pid: Option<u32> },
    ToolCall,
    Agent,
    Process { pid: u32 },
    Finalization,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeRunNodeStatus {
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeRunCancelScope {
    None,
    Run,
    Call,
    Agent,
}

#[derive(Clone, Debug)]
pub struct NativeRunNode {
    pub stable_id: String,
    pub parent_id: Option<String>,
    pub launch_ordinal: u64,
    pub kind: NativeRunNodeKind,
    pub status: NativeRunNodeStatus,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    pub summary: String,
    pub recent: String,
    pub artifact_refs: Vec<String>,
    pub cancel_scope: NativeRunCancelScope,
}

#[derive(Clone, Debug)]
pub struct NativeRunTreeSnapshot {
    pub identity: NativeRunIdentity,
    pub nodes: Vec<NativeRunNode>,
    pub local_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeRunCancelResult {
    Requested,
    NotCancellable,
}

#[derive(Default)]
pub(crate) struct NativeRunTreeRegistry {
    state: Mutex<RegistryState>,
}

#[derive(Default)]
struct RegistryState {
    active: Option<ActiveRun>,
}

struct ActiveRun {
    snapshot: NativeRunTreeSnapshot,
    next_ordinal: u64,
    targets: HashMap<String, CancellationToken>,
    tx: watch::Sender<Option<NativeRunTreeSnapshot>>,
}

#[derive(Clone)]
pub(crate) struct NativeRunTreeOwner {
    registry: Arc<NativeRunTreeRegistry>,
    identity: NativeRunIdentity,
    lifetime: Arc<()>,
}

impl NativeRunTreeRegistry {
    pub(crate) fn begin(
        self: &Arc<Self>,
        identity: NativeRunIdentity,
        task: &str,
        cancellation: CancellationToken,
    ) -> Result<NativeRunTreeOwner, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.active.is_some() {
            return Err("a native run tree is already active".to_string());
        }
        let root = NativeRunNode {
            stable_id: "run".to_string(),
            parent_id: None,
            launch_ordinal: 0,
            kind: NativeRunNodeKind::Run,
            status: NativeRunNodeStatus::Running,
            started_at: Instant::now(),
            finished_at: None,
            summary: bounded(task, NATIVE_RUN_TREE_SUMMARY_BYTES),
            recent: String::new(),
            artifact_refs: Vec::new(),
            cancel_scope: NativeRunCancelScope::Run,
        };
        let snapshot = NativeRunTreeSnapshot {
            identity: identity.clone(),
            nodes: vec![root],
            local_error: None,
        };
        let (tx, _rx) = watch::channel(Some(snapshot.clone()));
        state.active = Some(ActiveRun {
            snapshot,
            next_ordinal: LAUNCH_ORDINAL_STRIDE,
            targets: HashMap::from([("run".to_string(), cancellation)]),
            tx,
        });
        Ok(NativeRunTreeOwner {
            registry: Arc::clone(self),
            identity,
            lifetime: Arc::new(()),
        })
    }

    pub fn subscribe(
        &self,
        thread_id: &str,
        run_id: &str,
    ) -> Result<watch::Receiver<Option<NativeRunTreeSnapshot>>, String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active = state
            .active
            .as_ref()
            .ok_or_else(|| "no native run is active".to_string())?;
        if active.snapshot.identity.thread_id != thread_id
            || active.snapshot.identity.run_id != run_id
        {
            return Err("native run tree identity does not match the active run".to_string());
        }
        Ok(active.tx.subscribe())
    }

    pub fn cancel(
        &self,
        thread_id: &str,
        run_id: &str,
        node_id: &str,
    ) -> Result<NativeRunCancelResult, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active = state
            .active
            .as_mut()
            .ok_or_else(|| "no native run is active".to_string())?;
        if active.snapshot.identity.thread_id != thread_id
            || active.snapshot.identity.run_id != run_id
        {
            return Err("native run cancellation identity does not match its owner".to_string());
        }
        let Some(node) = active
            .snapshot
            .nodes
            .iter_mut()
            .find(|node| node.stable_id == node_id)
        else {
            return Err("unknown native run tree node".to_string());
        };
        if node.status != NativeRunNodeStatus::Running {
            return Ok(NativeRunCancelResult::NotCancellable);
        }
        let Some(target) = active.targets.get(node_id).cloned() else {
            return Ok(NativeRunCancelResult::NotCancellable);
        };
        node.status = NativeRunNodeStatus::Cancelling;
        target.cancel();
        publish(active);
        Ok(NativeRunCancelResult::Requested)
    }
}

impl NativeRunTreeOwner {
    pub(crate) fn start(
        &self,
        stable_id: impl Into<String>,
        parent_id: impl Into<String>,
        kind: NativeRunNodeKind,
        summary: &str,
        cancellation: Option<(NativeRunCancelScope, CancellationToken)>,
    ) {
        self.start_inner(
            stable_id.into(),
            parent_id.into(),
            kind,
            summary,
            cancellation,
            None,
        );
    }

    pub(crate) fn start_ordered(
        &self,
        stable_id: impl Into<String>,
        parent_id: impl Into<String>,
        kind: NativeRunNodeKind,
        summary: &str,
        cancellation: Option<(NativeRunCancelScope, CancellationToken)>,
        parent_launch_offset: u64,
    ) {
        self.start_inner(
            stable_id.into(),
            parent_id.into(),
            kind,
            summary,
            cancellation,
            Some(parent_launch_offset),
        );
    }

    fn start_inner(
        &self,
        stable_id: String,
        parent_id: String,
        kind: NativeRunNodeKind,
        summary: &str,
        cancellation: Option<(NativeRunCancelScope, CancellationToken)>,
        parent_launch_offset: Option<u64>,
    ) {
        self.with_active(|active| {
            if active.snapshot.nodes.len() >= NATIVE_RUN_TREE_MAX_NODES {
                set_error(active, "native run tree node limit reached");
                return;
            }
            let parent_ordinal = active
                .snapshot
                .nodes
                .iter()
                .find(|node| node.stable_id == parent_id)
                .map(|node| node.launch_ordinal);
            if active
                .snapshot
                .nodes
                .iter()
                .any(|node| node.stable_id == stable_id)
                || parent_ordinal.is_none()
            {
                set_error(
                    active,
                    "native run tree rejected duplicate or orphaned node",
                );
                return;
            }
            let launch_ordinal = match parent_launch_offset {
                Some(offset) if (1..LAUNCH_ORDINAL_STRIDE).contains(&offset) => {
                    parent_ordinal.and_then(|parent| parent.checked_add(offset))
                }
                Some(_) => None,
                None => {
                    let ordinal = active.next_ordinal;
                    active.next_ordinal = active.next_ordinal.saturating_add(LAUNCH_ORDINAL_STRIDE);
                    Some(ordinal)
                }
            };
            let Some(launch_ordinal) = launch_ordinal else {
                set_error(active, "native run tree rejected invalid launch order");
                return;
            };
            if active
                .snapshot
                .nodes
                .iter()
                .any(|node| node.launch_ordinal == launch_ordinal)
            {
                set_error(active, "native run tree rejected duplicate launch order");
                return;
            }
            let (cancel_scope, token) = cancellation
                .map_or((NativeRunCancelScope::None, None), |(scope, token)| {
                    (scope, Some(token))
                });
            if let Some(token) = token {
                active.targets.insert(stable_id.clone(), token);
            }
            active.snapshot.nodes.push(NativeRunNode {
                stable_id,
                parent_id: Some(parent_id),
                launch_ordinal,
                kind,
                status: NativeRunNodeStatus::Running,
                started_at: Instant::now(),
                finished_at: None,
                summary: bounded(summary, NATIVE_RUN_TREE_SUMMARY_BYTES),
                recent: String::new(),
                artifact_refs: Vec::new(),
                cancel_scope,
            });
            active
                .snapshot
                .nodes
                .sort_by_key(|node| node.launch_ordinal);
            publish(active);
        });
    }

    pub(crate) fn settle(&self, stable_id: &str, status: NativeRunNodeStatus, recent: &str) {
        self.with_active(|active| {
            let Some(node) = active
                .snapshot
                .nodes
                .iter_mut()
                .find(|node| node.stable_id == stable_id)
            else {
                set_error(
                    active,
                    "native run tree rejected settlement for an unknown node",
                );
                return;
            };
            if !matches!(
                node.status,
                NativeRunNodeStatus::Running | NativeRunNodeStatus::Cancelling
            ) {
                set_error(active, "native run tree rejected duplicate node settlement");
                return;
            }
            node.status = status;
            node.finished_at = Some(Instant::now());
            node.recent = bounded(recent, NATIVE_RUN_TREE_RECENT_BYTES);
            active.targets.remove(stable_id);
            publish(active);
        });
    }

    pub(crate) fn update_kind(&self, stable_id: &str, kind: NativeRunNodeKind) {
        self.with_active(|active| {
            let Some(node) = active
                .snapshot
                .nodes
                .iter_mut()
                .find(|node| node.stable_id == stable_id)
            else {
                set_error(
                    active,
                    "native run tree rejected progress for an unknown node",
                );
                return;
            };
            if node.status != NativeRunNodeStatus::Running {
                set_error(
                    active,
                    "native run tree rejected progress for a settled node",
                );
                return;
            }
            node.kind = kind;
            publish(active);
        });
    }

    pub(crate) fn add_ref(&self, stable_id: &str, reference: &str) {
        self.with_active(|active| {
            let Some(node) = active
                .snapshot
                .nodes
                .iter_mut()
                .find(|node| node.stable_id == stable_id)
            else {
                set_error(
                    active,
                    "native run tree rejected reference for an unknown node",
                );
                return;
            };
            let reference = bounded(reference, NATIVE_RUN_TREE_REF_BYTES);
            if node.artifact_refs.iter().any(|known| known == &reference) {
                return;
            }
            if node.artifact_refs.len() >= NATIVE_RUN_TREE_MAX_REFS {
                set_error(active, "native run tree reference limit reached");
                return;
            }
            node.artifact_refs.push(reference);
            publish(active);
        });
    }

    pub(crate) fn settle_unfinished(&self, status: NativeRunNodeStatus) {
        self.with_active(|active| {
            let now = Instant::now();
            for node in &mut active.snapshot.nodes {
                if node.stable_id != "run"
                    && matches!(
                        node.status,
                        NativeRunNodeStatus::Running | NativeRunNodeStatus::Cancelling
                    )
                {
                    node.status = status;
                    node.finished_at = Some(now);
                    active.targets.remove(&node.stable_id);
                }
            }
            publish(active);
        });
    }

    pub(crate) fn finish(&self, status: NativeRunNodeStatus) {
        let mut state = self
            .registry
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(active) = state.active.as_mut() else {
            return;
        };
        if active.snapshot.identity != self.identity {
            return;
        }
        if let Some(root) = active
            .snapshot
            .nodes
            .iter_mut()
            .find(|node| node.stable_id == "run")
        {
            root.status = status;
            root.finished_at = Some(Instant::now());
        }
        active.tx.send_replace(None);
        state.active = None;
    }

    fn with_active(&self, update: impl FnOnce(&mut ActiveRun)) {
        let mut state = self
            .registry
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(active) = state.active.as_mut() else {
            return;
        };
        if active.snapshot.identity == self.identity {
            update(active);
        }
    }
}

impl Drop for NativeRunTreeOwner {
    fn drop(&mut self) {
        if Arc::strong_count(&self.lifetime) != 1 {
            return;
        }
        let mut state = self
            .registry
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .active
            .as_ref()
            .is_some_and(|active| active.snapshot.identity == self.identity)
        {
            if let Some(active) = state.active.as_mut() {
                active.tx.send_replace(None);
            }
            state.active = None;
        }
    }
}

fn publish(active: &mut ActiveRun) {
    active.tx.send_replace(Some(active.snapshot.clone()));
}
fn set_error(active: &mut ActiveRun, message: &str) {
    active.snapshot.local_error = Some(bounded(message, NATIVE_RUN_TREE_SUMMARY_BYTES));
    publish(active);
}
fn bounded(value: &str, cap: usize) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    if sanitized.len() <= cap {
        return sanitized;
    }
    let marker = "…";
    let mut end = cap.saturating_sub(marker.len()).min(sanitized.len());
    while !sanitized.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{}", &sanitized[..end], marker)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> NativeRunIdentity {
        NativeRunIdentity {
            session_id: "session".into(),
            thread_id: "thread".into(),
            run_id: "run-id".into(),
        }
    }

    #[test]
    fn stable_launch_order_and_selective_cancellation_are_bounded() {
        let registry = Arc::new(NativeRunTreeRegistry::default());
        let root = CancellationToken::new();
        let owner = registry
            .begin(identity(), "inspect", root.clone())
            .expect("begin");
        let call = CancellationToken::new();
        owner.start(
            "generation",
            "run",
            NativeRunNodeKind::Generation,
            "generate",
            None,
        );
        owner.start(
            "call-1",
            "run",
            NativeRunNodeKind::ToolCall,
            "shell_command",
            Some((NativeRunCancelScope::Call, call.clone())),
        );
        let receiver = registry.subscribe("thread", "run-id").expect("subscribe");
        let snapshot = receiver.borrow().clone().expect("active");
        assert_eq!(
            snapshot
                .nodes
                .iter()
                .map(|node| node.launch_ordinal)
                .collect::<Vec<_>>(),
            vec![0, LAUNCH_ORDINAL_STRIDE, LAUNCH_ORDINAL_STRIDE * 2]
        );
        assert_eq!(
            registry.cancel("thread", "run-id", "call-1"),
            Ok(NativeRunCancelResult::Requested)
        );
        assert!(call.is_cancelled());
        assert!(!root.is_cancelled());
        owner.settle("call-1", NativeRunNodeStatus::Cancelled, "cancelled");
        owner.finish(NativeRunNodeStatus::Succeeded);
        assert!(receiver.borrow().is_none());
    }

    #[test]
    fn malformed_updates_fail_closed_without_corrupting_nodes() {
        let registry = Arc::new(NativeRunTreeRegistry::default());
        let owner = registry
            .begin(identity(), "inspect", CancellationToken::new())
            .expect("begin");
        owner.start(
            "orphan",
            "missing",
            NativeRunNodeKind::ToolCall,
            "bad",
            None,
        );
        owner.start(
            "generation",
            "run",
            NativeRunNodeKind::Generation,
            "good",
            None,
        );
        owner.start(
            "generation",
            "run",
            NativeRunNodeKind::Generation,
            "duplicate",
            None,
        );
        let receiver = registry.subscribe("thread", "run-id").expect("subscribe");
        let snapshot = receiver.borrow().clone().expect("active");
        assert_eq!(snapshot.nodes.len(), 2);
        assert!(
            snapshot
                .local_error
                .as_deref()
                .is_some_and(|error| error.contains("duplicate"))
        );
        assert!(registry.subscribe("other", "run-id").is_err());
        assert!(registry.cancel("thread", "run-id", "unknown").is_err());
    }

    #[test]
    fn overflow_becomes_a_compact_local_error() {
        let registry = Arc::new(NativeRunTreeRegistry::default());
        let owner = registry
            .begin(
                identity(),
                "x".repeat(1000).as_str(),
                CancellationToken::new(),
            )
            .expect("begin");
        for index in 0..NATIVE_RUN_TREE_MAX_NODES + 2 {
            owner.start(
                format!("node-{index}"),
                "run",
                NativeRunNodeKind::ToolCall,
                &"y".repeat(1000),
                None,
            );
        }
        let receiver = registry.subscribe("thread", "run-id").expect("subscribe");
        let snapshot = receiver.borrow().clone().expect("active");
        assert_eq!(snapshot.nodes.len(), NATIVE_RUN_TREE_MAX_NODES);
        assert!(
            snapshot
                .nodes
                .iter()
                .all(|node| node.summary.len() <= NATIVE_RUN_TREE_SUMMARY_BYTES)
        );
        assert!(
            snapshot
                .local_error
                .as_ref()
                .is_some_and(|error| error.len() <= NATIVE_RUN_TREE_SUMMARY_BYTES)
        );
    }

    #[test]
    fn stored_detail_fields_are_utf8_bounded_single_line_and_refs_are_deduplicated() {
        let registry = Arc::new(NativeRunTreeRegistry::default());
        let owner = registry
            .begin(
                identity(),
                &format!("task\n\u{001b}[31m{}", "界".repeat(200)),
                CancellationToken::new(),
            )
            .expect("begin");
        owner.start(
            "call-1",
            "run",
            NativeRunNodeKind::ToolCall,
            "shell\rrequest",
            None,
        );
        owner.add_ref("call-1", "native-code-mode://thread/run/a\nref");
        owner.add_ref("call-1", "native-code-mode://thread/run/a\nref");
        owner.settle(
            "call-1",
            NativeRunNodeStatus::Succeeded,
            &format!("result\t{}", "界".repeat(500)),
        );

        let receiver = registry.subscribe("thread", "run-id").expect("subscribe");
        let snapshot = receiver.borrow().clone().expect("active");
        let root = &snapshot.nodes[0];
        let call = &snapshot.nodes[1];
        for (value, cap) in [
            (&root.summary, NATIVE_RUN_TREE_SUMMARY_BYTES),
            (&call.summary, NATIVE_RUN_TREE_SUMMARY_BYTES),
            (&call.recent, NATIVE_RUN_TREE_RECENT_BYTES),
            (&call.artifact_refs[0], NATIVE_RUN_TREE_REF_BYTES),
        ] {
            assert!(value.len() <= cap);
            assert!(!value.chars().any(char::is_control), "{value:?}");
        }
        assert_eq!(call.artifact_refs.len(), 1);
        assert!(root.summary.ends_with('…'));
        assert!(call.recent.ends_with('…'));
    }

    #[test]
    fn dropping_the_last_owner_removes_the_ephemeral_tree() {
        let registry = Arc::new(NativeRunTreeRegistry::default());
        let owner = registry
            .begin(identity(), "inspect", CancellationToken::new())
            .expect("begin");
        let receiver = registry.subscribe("thread", "run-id").expect("subscribe");
        let observer_owner = owner.clone();
        drop(owner);
        assert!(receiver.borrow().is_some());
        drop(observer_owner);
        assert!(receiver.borrow().is_none());
        assert!(registry.subscribe("thread", "run-id").is_err());
    }

    #[test]
    fn compiler_workflow_and_descendant_cancellation_truthfully_cancel_the_run() {
        let kinds = [
            NativeRunNodeKind::Compile {
                attempt: 1,
                pid: Some(11),
            },
            NativeRunNodeKind::Workflow {
                attempt: 1,
                pid: Some(12),
            },
            NativeRunNodeKind::Process { pid: 13 },
        ];
        for (index, kind) in kinds.into_iter().enumerate() {
            let registry = Arc::new(NativeRunTreeRegistry::default());
            let root = CancellationToken::new();
            let owner = registry
                .begin(identity(), "inspect", root.clone())
                .expect("begin");
            let node_id = format!("run-owned-{index}");
            owner.start(
                node_id.clone(),
                "run",
                kind,
                "run-owned work",
                Some((NativeRunCancelScope::Run, root.clone())),
            );
            assert_eq!(
                registry.cancel("thread", "run-id", &node_id),
                Ok(NativeRunCancelResult::Requested)
            );
            assert!(root.is_cancelled());
        }
    }

    #[test]
    fn authoritative_child_ordinals_reorder_concurrent_callbacks_stably() {
        let registry = Arc::new(NativeRunTreeRegistry::default());
        let owner = registry
            .begin(identity(), "inspect", CancellationToken::new())
            .expect("begin");
        owner.start(
            "workflow-a1",
            "run",
            NativeRunNodeKind::Workflow {
                attempt: 1,
                pid: Some(10),
            },
            "workflow",
            None,
        );
        owner.start_ordered(
            "call-2",
            "workflow-a1",
            NativeRunNodeKind::ToolCall,
            "second callback arrived first",
            None,
            2,
        );
        owner.start_ordered(
            "call-1",
            "workflow-a1",
            NativeRunNodeKind::ToolCall,
            "first callback arrived second",
            None,
            1,
        );
        let receiver = registry.subscribe("thread", "run-id").expect("subscribe");
        let snapshot = receiver.borrow().clone().expect("active");
        assert_eq!(
            snapshot
                .nodes
                .iter()
                .map(|node| node.stable_id.as_str())
                .collect::<Vec<_>>(),
            ["run", "workflow-a1", "call-1", "call-2"]
        );
        assert_eq!(snapshot.nodes[2].launch_ordinal, LAUNCH_ORDINAL_STRIDE + 1);
        assert_eq!(snapshot.nodes[3].launch_ordinal, LAUNCH_ORDINAL_STRIDE + 2);
    }
}
