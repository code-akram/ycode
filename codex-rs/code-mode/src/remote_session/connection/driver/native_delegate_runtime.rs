use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode_protocol::host::DelegateRequestId;
use codex_code_mode_protocol::host::MAX_PENDING_DELEGATE_CALLS;
use codex_code_mode_protocol::host::NativeToolRequest;
use codex_code_mode_protocol::host::SessionId;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::types::DriverEvent;
use super::types::NativeRunKey;
use crate::native::NativeCodeModeDelegate;
use crate::native::NativeRunIdentity;
use crate::native::NativeToolInvocation;
use crate::native::validate_tool_outcome;
use crate::native::validate_tool_request;

const MAX_RECENT_NATIVE_DELEGATE_IDS: usize = 4_096;
const COOPERATIVE_CANCEL_GRACE: Duration = Duration::from_millis(500);
const DRIVER_SETTLE_GRACE: Duration = Duration::from_millis(950);
const MAX_CLEANUP_DIAGNOSTIC_BYTES: usize = 1_024;

struct NativeTarget {
    identity: NativeRunIdentity,
    delegate: Arc<dyn NativeCodeModeDelegate>,
    seen_call_ids: HashSet<String>,
}

struct NativeCall {
    key: NativeRunKey,
    cancellation: CancellationToken,
    completion_stop: CancellationToken,
    task: NativeTaskOwner,
}

impl NativeCall {
    fn revoke(self) -> NativeTaskOwner {
        self.cancellation.cancel();
        self.completion_stop.cancel();
        self.task
    }
}

struct NativeTaskOwner {
    handle: JoinHandle<()>,
}

impl NativeTaskOwner {
    fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    async fn settle(mut self, deadline: Instant) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if tokio::time::timeout(remaining, &mut self.handle)
            .await
            .is_err()
        {
            self.handle.abort();
            let _ = (&mut self.handle).await;
        }
    }
}

impl Drop for NativeTaskOwner {
    fn drop(&mut self) {
        // An unexpected driver drop must never detach the delegate future.
        self.handle.abort();
    }
}

pub(super) struct NativeDelegateRuntime {
    targets: HashMap<NativeRunKey, NativeTarget>,
    calls: HashMap<DelegateRequestId, NativeCall>,
    seen_request_ids: HashSet<DelegateRequestId>,
    request_order: VecDeque<DelegateRequestId>,
    event_tx: mpsc::Sender<DriverEvent>,
    owned_tasks: Arc<AtomicUsize>,
    retired_tasks: Vec<NativeTaskOwner>,
}

impl NativeDelegateRuntime {
    pub(super) fn new(event_tx: mpsc::Sender<DriverEvent>, owned_tasks: Arc<AtomicUsize>) -> Self {
        Self {
            targets: HashMap::new(),
            calls: HashMap::new(),
            seen_request_ids: HashSet::new(),
            request_order: VecDeque::new(),
            event_tx,
            owned_tasks,
            retired_tasks: Vec::new(),
        }
    }

    pub(super) fn register(
        &mut self,
        key: NativeRunKey,
        identity: NativeRunIdentity,
        delegate: Arc<dyn NativeCodeModeDelegate>,
    ) -> Result<(), String> {
        if self.targets.contains_key(&key) {
            return Err("native run already has a delegate owner".to_string());
        }
        self.targets.insert(
            key,
            NativeTarget {
                identity,
                delegate,
                seen_call_ids: HashSet::new(),
            },
        );
        Ok(())
    }

    pub(super) fn start(
        &mut self,
        id: DelegateRequestId,
        session_id: SessionId,
        run_id: String,
        runtime_call_id: String,
        request: NativeToolRequest,
    ) -> Result<(), String> {
        if !self.seen_request_ids.insert(id) {
            return Err(format!("duplicate native delegate request ID {id:?}"));
        }
        self.request_order.push_back(id);
        while self.request_order.len() > MAX_RECENT_NATIVE_DELEGATE_IDS {
            if let Some(expired) = self.request_order.pop_front() {
                self.seen_request_ids.remove(&expired);
            }
        }
        if self.calls.len() >= MAX_PENDING_DELEGATE_CALLS {
            return Err(format!(
                "native delegate limit of {MAX_PENDING_DELEGATE_CALLS} calls exceeded"
            ));
        }
        validate_tool_request(&request)?;
        if runtime_call_id.is_empty() || runtime_call_id.len() > 256 {
            return Err("native runtime call ID must contain 1..=256 bytes".to_string());
        }
        let key = NativeRunKey { session_id, run_id };
        let target = self
            .targets
            .get_mut(&key)
            .ok_or_else(|| "unknown or mismatched native delegate target".to_string())?;
        if !target.seen_call_ids.insert(runtime_call_id.clone()) {
            return Err("duplicate native runtime call ID".to_string());
        }
        let cancellation = CancellationToken::new();
        let completion_stop = CancellationToken::new();
        let invocation = NativeToolInvocation {
            identity: target.identity.clone(),
            runtime_call_id: runtime_call_id.clone(),
            request,
        };
        let delegate = Arc::clone(&target.delegate);
        let task_cancellation = cancellation.clone();
        let event_tx = self.event_tx.clone();
        let owned_tasks = Arc::clone(&self.owned_tasks);
        let task_completion_stop = completion_stop.clone();
        owned_tasks.fetch_add(1, Ordering::AcqRel);
        let handle = tokio::spawn(async move {
            let _task = OwnedTask(owned_tasks);
            let mut invocation = Box::pin(delegate.invoke(invocation, task_cancellation.clone()));
            let (result, stopped) = tokio::select! {
                biased;
                _ = task_completion_stop.cancelled() => {
                    task_cancellation.cancel();
                    let result = tokio::time::timeout(
                        COOPERATIVE_CANCEL_GRACE,
                        &mut invocation,
                    )
                    .await
                    .unwrap_or_else(|_| Err("native delegate ignored cancellation".to_string()));
                    (result, true)
                },
                result = &mut invocation => (result, false),
            };
            drop(invocation);
            if stopped && let Err(error) = delegate.settle_invocation(&runtime_call_id).await {
                warn!(
                    runtime_call_id,
                    error = %bounded_cleanup_diagnostic(&error),
                    "native delegate cleanup failed after response suppression"
                );
            }
            let result = result.and_then(|outcome| {
                validate_tool_outcome(&outcome)?;
                Ok(outcome)
            });
            if stopped {
                return;
            }
            tokio::select! {
                biased;
                _ = task_completion_stop.cancelled() => {}
                _ = event_tx.send(DriverEvent::NativeDelegateCompleted { id, result }) => {}
            }
        });
        self.calls.insert(
            id,
            NativeCall {
                key,
                cancellation,
                completion_stop,
                task: NativeTaskOwner { handle },
            },
        );
        Ok(())
    }

    pub(super) fn complete(&mut self, id: DelegateRequestId) -> Option<NativeRunKey> {
        let call = self.calls.remove(&id)?;
        let key = call.key.clone();
        self.retired_tasks.push(call.task);
        self.reap_finished();
        Some(key)
    }

    pub(super) fn cancel(&mut self, id: DelegateRequestId) {
        if let Some(call) = self.calls.remove(&id) {
            self.retired_tasks.push(call.revoke());
        }
        self.reap_finished();
    }

    pub(super) fn unregister(&mut self, key: &NativeRunKey) {
        self.targets.remove(key);
        let ids = self
            .calls
            .iter()
            .filter_map(|(id, call)| (&call.key == key).then_some(*id))
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(call) = self.calls.remove(&id) {
                self.retired_tasks.push(call.revoke());
            }
        }
        self.reap_finished();
    }

    pub(super) fn fail_all(&mut self) {
        self.targets.clear();
        for (_, call) in self.calls.drain() {
            self.retired_tasks.push(call.revoke());
        }
        self.reap_finished();
    }

    pub(super) async fn settle_all(&mut self) {
        self.fail_all();
        let deadline = Instant::now() + DRIVER_SETTLE_GRACE;
        for task in self.retired_tasks.drain(..) {
            task.settle(deadline).await;
        }
    }

    fn reap_finished(&mut self) {
        self.retired_tasks.retain(|task| !task.is_finished());
    }
}

fn bounded_cleanup_diagnostic(error: &str) -> &str {
    if error.len() <= MAX_CLEANUP_DIAGNOSTIC_BYTES {
        return error;
    }
    let mut end = MAX_CLEANUP_DIAGNOSTIC_BYTES;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    &error[..end]
}

struct OwnedTask(Arc<AtomicUsize>);

impl Drop for OwnedTask {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
