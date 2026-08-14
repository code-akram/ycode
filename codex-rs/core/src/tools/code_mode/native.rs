use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use codex_code_mode::NativeCodeModeDelegate;
use codex_code_mode::NativeRunIdentity;
use codex_code_mode::NativeToolFuture;
use codex_code_mode::NativeToolInvocation;
use codex_code_mode::host::NativeToolOutcome;
use codex_code_mode::host::NativeToolRequest;
use codex_features::Feature;
use codex_protocol::AgentPath;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use codex_tools::ToolName;
use serde_json::json;
use tokio::sync::Semaphore;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::agent::control::SpawnAgentOptions;
use crate::native_run_tree::NativeRunCancelScope;
use crate::native_run_tree::NativeRunNodeKind;
use crate::native_run_tree::NativeRunNodeStatus;
use crate::native_run_tree::NativeRunTreeOwner;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::multi_agents_common::apply_explicit_spawn_agent_model_overrides;
use crate::tools::handlers::multi_agents_common::apply_spawn_agent_service_tier;
use crate::tools::handlers::multi_agents_common::build_agent_spawn_config;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolCallSource;

const NATIVE_TOTAL_CALLS: usize = 32;
const NATIVE_CONCURRENT_CALLS: usize = 4;
const NATIVE_CALL_BYTES: usize = 64 * 1024;
const NATIVE_TOTAL_ARTIFACT_BYTES: usize = 1024 * 1024;
const NATIVE_WORKFLOW_TIMEOUT: Duration = Duration::from_secs(30);
const NATIVE_WORKER_AGENTS: usize = 3;
const NATIVE_AGENT_TASK_BYTES: usize = 16 * 1024;
const NATIVE_AGENT_MODEL_BYTES: usize = 128;
const NATIVE_AGENT_EFFORT_BYTES: usize = 32;
// Stay within the process-owned client's 500 ms cooperative delegate-cancellation grace.
const NATIVE_AGENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(400);
// After the client has already provided its cooperative grace, leave enough of the 950 ms
// driver deadline for exact keyed emergency cleanup and owner-task joining.
const NATIVE_AGENT_OWNER_EMERGENCY_AFTER: Duration = Duration::from_millis(100);
const NATIVE_AGENT_DEVELOPER_CONTRACT: &str = "This is a scoped native Code Mode worker. Complete only the supplied task. Inherit the root model and reasoning by default; use an explicit override only when the human requested it or a clear task-specific reason requires it. Do not invoke Code Mode or spawn, message, or otherwise create sub-agents. Return one concise final result; raw reasoning and tool traffic remain in this worker rollout.";

/// One run-scoped bridge from native delegate callbacks into the canonical tool router.
pub(crate) struct NativeCodeModeDispatchWorker {
    identity: NativeRunIdentity,
    attempt: u8,
    session: Arc<Session>,
    step_context: Arc<StepContext>,
    tool_runtime: ToolCallRuntime,
    cancellation: CancellationToken,
    deadline: Instant,
    permits: Arc<Semaphore>,
    agent_permits: Arc<Semaphore>,
    total_calls: AtomicUsize,
    active_calls: Arc<AtomicUsize>,
    active_agents: Arc<AtomicUsize>,
    artifact_bytes: Arc<AtomicUsize>,
    seen_runtime_calls: Mutex<HashSet<String>>,
    agent_owners: Arc<NativeAgentOwners>,
    #[cfg(test)]
    emergency_agent_settlements: AtomicUsize,
    run_tree: NativeRunTreeOwner,
}

impl NativeCodeModeDispatchWorker {
    pub(crate) fn new(
        identity: NativeRunIdentity,
        attempt: u8,
        session: Arc<Session>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
        cancellation: CancellationToken,
        run_tree: NativeRunTreeOwner,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity,
            attempt,
            session: Arc::clone(&session),
            step_context: Arc::clone(&step_context),
            tool_runtime: ToolCallRuntime::new(session, step_context, tracker),
            cancellation,
            deadline: Instant::now() + NATIVE_WORKFLOW_TIMEOUT,
            permits: Arc::new(Semaphore::new(NATIVE_CONCURRENT_CALLS)),
            agent_permits: Arc::new(Semaphore::new(NATIVE_WORKER_AGENTS)),
            total_calls: AtomicUsize::new(0),
            active_calls: Arc::new(AtomicUsize::new(0)),
            active_agents: Arc::new(AtomicUsize::new(0)),
            artifact_bytes: Arc::new(AtomicUsize::new(0)),
            seen_runtime_calls: Mutex::new(HashSet::new()),
            agent_owners: Arc::new(NativeAgentOwners::default()),
            #[cfg(test)]
            emergency_agent_settlements: AtomicUsize::new(0),
            run_tree,
        })
    }

    #[cfg(test)]
    pub(crate) fn owned_counts(&self) -> (usize, usize) {
        (
            self.active_calls.load(Ordering::Acquire),
            NATIVE_CONCURRENT_CALLS.saturating_sub(self.permits.available_permits()),
        )
    }

    #[cfg(test)]
    pub(crate) fn owned_agent_counts(&self) -> (usize, usize) {
        (
            self.active_agents.load(Ordering::Acquire),
            NATIVE_WORKER_AGENTS.saturating_sub(self.agent_permits.available_permits()),
        )
    }

    #[cfg(test)]
    fn owned_agent_task_count(&self) -> usize {
        self.agent_owners
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    #[cfg(test)]
    fn emergency_agent_settlement_count(&self) -> usize {
        self.emergency_agent_settlements.load(Ordering::Acquire)
    }

    async fn invoke_inner(
        &self,
        invocation: NativeToolInvocation,
        delegate_cancellation: CancellationToken,
    ) -> Result<NativeToolOutcome, String> {
        self.invoke_inner_with_encoder(invocation, delegate_cancellation, encode_tool_result)
            .await
    }

    async fn invoke_inner_with_encoder(
        &self,
        invocation: NativeToolInvocation,
        delegate_cancellation: CancellationToken,
        encode_result: impl Fn(&serde_json::Value) -> Result<Vec<u8>, String>,
    ) -> Result<NativeToolOutcome, String> {
        let call_ordinal = self.validate_invocation(&invocation)?;
        if self.cancellation.is_cancelled() || delegate_cancellation.is_cancelled() {
            return Err("native tool call cancelled before admission".to_string());
        }
        let count = self.total_calls.fetch_add(1, Ordering::AcqRel) + 1;
        if count > NATIVE_TOTAL_CALLS {
            return Ok(NativeToolOutcome::Failure {
                message: "native run exceeded 32 total calls".to_string(),
            });
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("native workflow deadline elapsed".to_string());
        }
        let tree_summary = native_request_summary(&invocation.request);
        let runtime_call_id = invocation.runtime_call_id;
        let (tool_name, payload) = match invocation.request {
            NativeToolRequest::Shell {
                command,
                workdir,
                timeout_ms,
            } => {
                let timeout_ms = u64::from(timeout_ms)
                    .min(u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX))
                    .max(1);
                let arguments = shell_handler_arguments(&command, workdir.as_deref(), timeout_ms)?;
                (
                    ToolName::plain("shell_command"),
                    ToolPayload::Function { arguments },
                )
            }
            NativeToolRequest::ApplyPatch { patch } => (
                ToolName::plain("apply_patch"),
                ToolPayload::Custom { input: patch },
            ),
            NativeToolRequest::Agent {
                task,
                model,
                reasoning_effort,
            } => {
                return self
                    .invoke_agent(
                        call_ordinal,
                        runtime_call_id,
                        task,
                        model,
                        reasoning_effort,
                        tree_summary,
                        delegate_cancellation,
                    )
                    .await;
            }
        };
        let request_bytes = exact_handler_payload_bytes(&payload);
        if request_bytes > NATIVE_CALL_BYTES || !self.reserve_artifact_bytes(request_bytes) {
            return Ok(NativeToolOutcome::Failure {
                message: "native call request exceeded its bounded artifact budget".to_string(),
            });
        }
        let permit = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => {
                return Err("native run cancelled before tool admission".to_string());
            }
            _ = delegate_cancellation.cancelled() => {
                return Err("native tool call cancelled before admission".to_string());
            }
            permit = Arc::clone(&self.permits).acquire_owned() => {
                permit.map_err(|_| "native tool dispatcher closed".to_string())?
            }
        };
        self.active_calls.fetch_add(1, Ordering::AcqRel);
        let _active = ActiveCounter::new(Arc::clone(&self.active_calls));
        let tree_call_id = format!("call-{runtime_call_id}");
        let launch_ordinal = u64::try_from(call_ordinal)
            .map_err(|_| "native tool launch ordinal overflowed".to_string())?;
        self.run_tree.start_ordered(
            tree_call_id.clone(),
            format!("workflow-a{}", self.attempt),
            NativeRunNodeKind::ToolCall,
            &tree_summary,
            Some((NativeRunCancelScope::Call, delegate_cancellation.clone())),
            launch_ordinal,
        );
        let mut tree_call = TreeCallSettlement::new(
            self.run_tree.clone(),
            tree_call_id,
            delegate_cancellation.clone(),
            self.cancellation.clone(),
        );
        let is_shell = tool_name.name == "shell_command";
        let call = ToolCall {
            tool_name,
            call_id: format!("native-{}-{runtime_call_id}", self.identity.run_id),
            payload,
            encrypted_function_args: None,
        };
        let cancellation = self.cancellation.child_token();
        let mut dispatch = Box::pin(self.tool_runtime.clone().handle_tool_call_with_source(
            call,
            ToolCallSource::NativeCodeMode {
                run_id: self.identity.run_id.clone(),
                runtime_call_id,
            },
            cancellation.clone(),
        ));
        let result = tokio::select! {
            biased;
            _ = delegate_cancellation.cancelled() => {
                cancellation.cancel();
                dispatch.await
            }
            _ = self.cancellation.cancelled() => {
                cancellation.cancel();
                dispatch.await
            }
            result = &mut dispatch => result,
        };
        drop(permit);
        let selectively_cancelled =
            delegate_cancellation.is_cancelled() && !self.cancellation.is_cancelled();
        let outcome: Result<NativeToolOutcome, String> = if selectively_cancelled {
            Ok(NativeToolOutcome::Failure {
                message: "native tool call cancelled by user".to_string(),
            })
        } else {
            match result {
                Ok(result) => {
                    let value = result.code_mode_result();
                    match encode_result(&value) {
                        Err(error) => Err(error),
                        Ok(output)
                            if output.len() > NATIVE_CALL_BYTES
                                || !self.reserve_artifact_bytes(output.len()) =>
                        {
                            Ok(NativeToolOutcome::Failure {
                                message: "native tool result exceeded its bounded artifact budget"
                                    .to_string(),
                            })
                        }
                        Ok(output)
                            if is_shell
                                && !value
                                    .as_str()
                                    .is_some_and(|text| text.starts_with("Exit code: 0\n")) =>
                        {
                            // The canonical complete-result shell handler exposes its Code Mode
                            // result as a bounded human-readable string beginning with the exit
                            // status. Keep the generated SDK outcome typed without bypassing or
                            // duplicating that handler.
                            Ok(NativeToolOutcome::Failure {
                                message: bounded_error(
                                    String::from_utf8_lossy(&output).into_owned(),
                                ),
                            })
                        }
                        Ok(output) => Ok(NativeToolOutcome::Success { output }),
                    }
                }
                Err(error) => Ok(NativeToolOutcome::Failure {
                    message: bounded_error(error.to_string()),
                }),
            }
        };
        tree_call.settle(&outcome);
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    async fn invoke_agent(
        &self,
        call_ordinal: usize,
        runtime_call_id: String,
        task: String,
        model: Option<String>,
        reasoning_effort: Option<String>,
        tree_summary: String,
        delegate_cancellation: CancellationToken,
    ) -> Result<NativeToolOutcome, String> {
        let launch_ordinal = u64::try_from(call_ordinal)
            .map_err(|_| "native agent launch ordinal overflowed".to_string())?;
        validate_agent_fields(&task, model.as_deref(), reasoning_effort.as_deref())?;
        let request_bytes =
            exact_agent_request_bytes(&task, model.as_deref(), reasoning_effort.as_deref())?;
        if request_bytes > NATIVE_CALL_BYTES || !self.reserve_artifact_bytes(request_bytes) {
            return Ok(NativeToolOutcome::Failure {
                message: "native agent request exceeded its bounded artifact budget".to_string(),
            });
        }
        let call_permit = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => {
                return Err("native run cancelled before agent admission".to_string());
            }
            _ = delegate_cancellation.cancelled() => {
                return Err("native agent cancelled before admission".to_string());
            }
            permit = Arc::clone(&self.permits).acquire_owned() => {
                permit.map_err(|_| "native dispatcher closed".to_string())?
            }
        };
        let agent_permit = match Arc::clone(&self.agent_permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                return Ok(NativeToolOutcome::Failure {
                    message: "native run allows at most three concurrent worker agents".to_string(),
                });
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                return Err("native agent dispatcher closed".to_string());
            }
        };
        let mut config = build_agent_spawn_config(
            &self.session.get_base_instructions().await,
            self.step_context.turn.as_ref(),
            self.step_context.environments.primary(),
        )
        .map_err(|error| bounded_error(error.to_string()))?;
        let requested_effort = reasoning_effort
            .as_deref()
            .map(str::parse::<ReasoningEffort>)
            .transpose()
            .map_err(bounded_error)?;
        apply_explicit_spawn_agent_model_overrides(
            self.session.as_ref(),
            self.step_context.turn.as_ref(),
            &mut config,
            model.as_deref(),
            requested_effort,
        )
        .await
        .map_err(|error| bounded_error(error.to_string()))?;
        apply_spawn_agent_service_tier(
            self.session.as_ref(),
            &mut config,
            self.step_context.turn.config.service_tier.as_deref(),
            None,
        )
        .await
        .map_err(|error| bounded_error(error.to_string()))?;

        let inherited_limit = self
            .step_context
            .turn
            .config
            .effective_agent_max_threads(self.step_context.turn.multi_agent_version)
            .unwrap_or(NATIVE_WORKER_AGENTS);
        // AgentRegistry counts spawned workers and shares the ordinary agent admission pool.
        // Intersect its configured limit with this run's three-worker ceiling; the native root
        // remains the fourth active agent conceptually but is not a spawned registry entry.
        config.agent_max_threads = Some(inherited_limit.min(NATIVE_WORKER_AGENTS));
        config.agents_enabled = false;
        let _ = config.features.disable(Feature::Collab);
        let _ = config.features.disable(Feature::MultiAgentV2);
        let scoped_instructions = config.developer_instructions.take().unwrap_or_default();
        config.developer_instructions = Some(if scoped_instructions.is_empty() {
            NATIVE_AGENT_DEVELOPER_CONTRACT.to_string()
        } else {
            format!("{scoped_instructions}\n\n{NATIVE_AGENT_DEVELOPER_CONTRACT}")
        });

        let task_name = format!(
            "native_{}_{call_ordinal}",
            self.identity.run_id.get(..8).unwrap_or("run")
        );
        let agent_path = AgentPath::root()
            .join(&task_name)
            .map_err(|error| bounded_error(format!("invalid native agent lineage: {error}")))?;
        let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: self.session.thread_id,
            depth: 1,
            agent_path: Some(agent_path),
            agent_nickname: None,
            agent_role: None,
        });
        self.active_calls.fetch_add(1, Ordering::AcqRel);
        self.active_agents.fetch_add(1, Ordering::AcqRel);
        let active_call = ActiveCounter::new(Arc::clone(&self.active_calls));
        let active_agent = ActiveCounter::new(Arc::clone(&self.active_agents));
        let agent_control = self.session.services.agent_control.clone();
        let owner = Arc::new(NativeAgentOwner::new(agent_control.clone()));
        self.agent_owners
            .insert(runtime_call_id.clone(), Arc::clone(&owner))?;
        let (result_tx, result_rx) = oneshot::channel();
        let owner_id = runtime_call_id.clone();
        let owners = Arc::clone(&self.agent_owners);
        let parent_thread_id = self.session.thread_id;
        let parent_turn_id = self.step_context.turn.sub_id.clone();
        let environments = self.step_context.environments.to_selections();
        let run_tree = self.run_tree.clone();
        let run_cancellation = self.cancellation.clone();
        let artifact_bytes = Arc::clone(&self.artifact_bytes);
        let attempt = self.attempt;
        let owner_for_task = Arc::clone(&owner);
        let task = tokio::spawn(async move {
            let _completion = NativeAgentTaskCompletion::new(
                Arc::clone(&owners.entries),
                owner_id.clone(),
                Arc::clone(&owner_for_task),
            );
            let outcome = run_owned_native_agent(NativeAgentRun {
                config,
                task,
                session_source,
                parent_thread_id,
                parent_turn_id,
                environments,
                agent_control,
                ownership_handoff: owner_for_task.ownership_handoff.clone(),
                run_tree,
                run_cancellation,
                delegate_cancellation,
                owner: Arc::clone(&owner_for_task),
                tree_summary,
                runtime_call_id: owner_id.clone(),
                attempt,
                launch_ordinal,
                artifact_bytes,
                call_permit,
                agent_permit,
                active_call,
                active_agent,
            })
            .await;
            let _ = result_tx.send(outcome);
        });
        self.agent_owners.track(runtime_call_id.clone(), task);
        let mut drop_guard = CancelOnDrop::new(owner.cancellation.clone());
        let outcome = result_rx
            .await
            .map_err(|_| "native agent owner ended without a result".to_string())?;
        self.agent_owners.join(&runtime_call_id).await?;
        drop_guard.disarm();
        outcome
    }

    async fn settle_agent_invocation(&self, runtime_call_id: &str) -> Result<(), String> {
        let mut cleanup_error = None;
        if let Some(owner) = self.agent_owners.get(runtime_call_id) {
            owner.cancellation.cancel();
            if tokio::time::timeout(NATIVE_AGENT_OWNER_EMERGENCY_AFTER, owner.done.cancelled())
                .await
                .is_err()
            {
                match owner.wait_for_worker_or_done().await {
                    NativeAgentOwnerReadiness::Done => {}
                    NativeAgentOwnerReadiness::Worker(thread_id) => {
                        #[cfg(test)]
                        self.emergency_agent_settlements
                            .fetch_add(1, Ordering::AcqRel);
                        if let Err(error) = owner.force_settle_worker(thread_id).await {
                            cleanup_error = Some(error);
                        }
                        if let Err(error) = self.agent_owners.abort_and_join(runtime_call_id).await
                        {
                            cleanup_error.get_or_insert(error);
                        }
                    }
                }
            }
        }
        if let Err(error) = self.agent_owners.join(runtime_call_id).await {
            cleanup_error.get_or_insert(error);
        }
        cleanup_error.map_or(Ok(()), Err)
    }

    fn validate_invocation(&self, invocation: &NativeToolInvocation) -> Result<usize, String> {
        if invocation.identity != self.identity {
            self.cancellation.cancel();
            return Err("native delegate identity does not match its owner".to_string());
        }
        let call_id = invocation.runtime_call_id.as_str();
        let prefix = format!("native-{}-a{}-", self.identity.run_id, self.attempt);
        let Some(ordinal_text) = call_id.strip_prefix(&prefix) else {
            self.cancellation.cancel();
            return Err("native runtime call ID does not match its run owner".to_string());
        };
        let ordinal = ordinal_text.parse::<usize>().ok().filter(|ordinal| {
            (1..=NATIVE_TOTAL_CALLS).contains(ordinal) && ordinal.to_string() == ordinal_text
        });
        let Some(ordinal) = ordinal else {
            self.cancellation.cancel();
            return Err("native runtime call ID has an invalid launch ordinal".to_string());
        };
        let mut seen = self
            .seen_runtime_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !seen.insert(call_id.to_string()) {
            self.cancellation.cancel();
            return Err("duplicate native runtime call ID".to_string());
        }
        Ok(ordinal)
    }

    fn reserve_artifact_bytes(&self, bytes: usize) -> bool {
        reserve_artifact_bytes(self.artifact_bytes.as_ref(), bytes)
    }
}

#[derive(Default)]
struct NativeAgentOwners {
    entries: Arc<Mutex<HashMap<String, Arc<NativeAgentOwner>>>>,
    tasks: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl NativeAgentOwners {
    fn insert(&self, id: String, owner: Arc<NativeAgentOwner>) -> Result<(), String> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries.contains_key(&id) {
            return Err("native agent invocation already has an owner".to_string());
        }
        entries.insert(id, owner);
        Ok(())
    }

    fn get(&self, id: &str) -> Option<Arc<NativeAgentOwner>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    fn track(&self, id: String, task: tokio::task::JoinHandle<()>) {
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            tasks.insert(id, task).is_none(),
            "validated native runtime call ID must own exactly one task"
        );
    }

    async fn join(&self, id: &str) -> Result<(), String> {
        let task = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        if let Some(task) = task {
            task.await.map_err(|error| {
                bounded_error(format!("native agent owner task failed: {error}"))
            })?;
        }
        Ok(())
    }

    async fn abort_and_join(&self, id: &str) -> Result<(), String> {
        let task = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        if let Some(task) = task {
            task.abort();
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                return Err(bounded_error(format!(
                    "native agent emergency owner task failed: {error}"
                )));
            }
        }
        Ok(())
    }
}

struct NativeAgentOwner {
    agent_control: crate::agent::AgentControl,
    ownership_handoff: crate::agent::control::NativeAgentOwnershipHandoff,
    cancellation: CancellationToken,
    done: CancellationToken,
}

impl NativeAgentOwner {
    fn new(agent_control: crate::agent::AgentControl) -> Self {
        Self {
            agent_control,
            ownership_handoff: Default::default(),
            cancellation: CancellationToken::new(),
            done: CancellationToken::new(),
        }
    }

    async fn wait_for_worker_or_done(&self) -> NativeAgentOwnerReadiness {
        if let Some(thread_id) = self.ownership_handoff.thread_id() {
            return NativeAgentOwnerReadiness::Worker(thread_id);
        }
        tokio::select! {
            biased;
            _ = self.done.cancelled() => NativeAgentOwnerReadiness::Done,
            thread_id = self.ownership_handoff.wait_for_thread_id() => {
                NativeAgentOwnerReadiness::Worker(thread_id)
            }
        }
    }

    async fn force_settle_worker(&self, thread_id: codex_protocol::ThreadId) -> Result<(), String> {
        let settlement = self
            .agent_control
            .settle_native_agent(thread_id, Duration::ZERO)
            .await;
        if self.agent_control.get_status(thread_id).await != AgentStatus::NotFound {
            self.agent_control
                .force_settle_native_agent_tree(thread_id)
                .await
                .map_err(|error| {
                    bounded_error(format!("native agent keyed force cleanup failed: {error}"))
                })?;
        }
        if self.agent_control.get_status(thread_id).await != AgentStatus::NotFound {
            return Err(
                "native agent emergency cleanup did not remove its exact worker".to_string(),
            );
        }
        settlement.map_err(|error| {
            bounded_error(format!("native agent emergency cleanup failed: {error}"))
        })
    }
}

enum NativeAgentOwnerReadiness {
    Done,
    Worker(codex_protocol::ThreadId),
}

struct NativeAgentTaskCompletion {
    entries: Arc<Mutex<HashMap<String, Arc<NativeAgentOwner>>>>,
    id: String,
    owner: Arc<NativeAgentOwner>,
}

impl NativeAgentTaskCompletion {
    fn new(
        entries: Arc<Mutex<HashMap<String, Arc<NativeAgentOwner>>>>,
        id: String,
        owner: Arc<NativeAgentOwner>,
    ) -> Self {
        Self { entries, id, owner }
    }
}

impl Drop for NativeAgentTaskCompletion {
    fn drop(&mut self) {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
        self.owner.done.cancel();
    }
}

struct CancelOnDrop {
    cancellation: CancellationToken,
    armed: bool,
}

impl CancelOnDrop {
    fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
        }
    }
}

struct NativeAgentRun {
    config: crate::config::Config,
    task: String,
    session_source: SessionSource,
    parent_thread_id: codex_protocol::ThreadId,
    parent_turn_id: String,
    environments: Vec<codex_protocol::protocol::TurnEnvironmentSelection>,
    agent_control: crate::agent::AgentControl,
    ownership_handoff: crate::agent::control::NativeAgentOwnershipHandoff,
    run_tree: NativeRunTreeOwner,
    run_cancellation: CancellationToken,
    delegate_cancellation: CancellationToken,
    owner: Arc<NativeAgentOwner>,
    tree_summary: String,
    runtime_call_id: String,
    attempt: u8,
    launch_ordinal: u64,
    artifact_bytes: Arc<AtomicUsize>,
    call_permit: tokio::sync::OwnedSemaphorePermit,
    agent_permit: tokio::sync::OwnedSemaphorePermit,
    active_call: ActiveCounter,
    active_agent: ActiveCounter,
}

async fn run_owned_native_agent(run: NativeAgentRun) -> Result<NativeToolOutcome, String> {
    let NativeAgentRun {
        config,
        task,
        session_source,
        parent_thread_id,
        parent_turn_id,
        environments,
        agent_control,
        ownership_handoff,
        run_tree,
        run_cancellation,
        delegate_cancellation,
        owner,
        tree_summary,
        runtime_call_id,
        attempt,
        launch_ordinal,
        artifact_bytes,
        call_permit: _call_permit,
        agent_permit: _agent_permit,
        active_call: _active_call,
        active_agent: _active_agent,
    } = run;
    let spawned = agent_control
        .spawn_native_agent_with_metadata(
            config,
            vec![UserInput::Text {
                text: task,
                text_elements: Vec::new(),
            }],
            session_source,
            SpawnAgentOptions {
                parent_thread_id: Some(parent_thread_id),
                parent_turn_id: Some(parent_turn_id),
                environments: Some(environments),
                ..Default::default()
            },
            ownership_handoff,
        )
        .await
        .map_err(|error| bounded_error(format!("native agent admission failed: {error}")))?;
    let tree_call_id = format!("agent-{runtime_call_id}");
    run_tree.start_ordered(
        tree_call_id.clone(),
        format!("workflow-a{attempt}"),
        NativeRunNodeKind::Agent,
        &tree_summary,
        Some((NativeRunCancelScope::Agent, delegate_cancellation.clone())),
        launch_ordinal,
    );
    let mut tree_call = TreeCallSettlement::new(
        run_tree,
        tree_call_id,
        delegate_cancellation.clone(),
        run_cancellation.clone(),
    );
    let mut status = match agent_control
        .subscribe_native_agent_status(spawned.thread_id)
        .await
    {
        Ok(status) => status,
        Err(error) => {
            let cleanup = agent_control
                .settle_native_agent(spawned.thread_id, NATIVE_AGENT_SHUTDOWN_TIMEOUT)
                .await;
            let outcome = Err(bounded_error(match cleanup {
                Ok(()) => format!("native agent status unavailable: {error}"),
                Err(cleanup) => {
                    format!("native agent status unavailable: {error}; cleanup failed: {cleanup}")
                }
            }));
            tree_call.settle(&outcome);
            return outcome;
        }
    };

    let terminal = loop {
        let current = status.borrow().clone();
        if matches!(
            current,
            AgentStatus::Completed(_)
                | AgentStatus::Errored(_)
                | AgentStatus::Shutdown
                | AgentStatus::NotFound
        ) {
            break current;
        }
        tokio::select! {
            biased;
            _ = owner.cancellation.cancelled() => break AgentStatus::Interrupted,
            _ = delegate_cancellation.cancelled() => break AgentStatus::Interrupted,
            _ = run_cancellation.cancelled() => break AgentStatus::Interrupted,
            changed = status.changed() => {
                if changed.is_err() {
                    break agent_control.get_status(spawned.thread_id).await;
                }
            }
        }
    };
    let cancelled = owner.cancellation.is_cancelled()
        || delegate_cancellation.is_cancelled()
        || run_cancellation.is_cancelled();
    if owner.cancellation.is_cancelled()
        && !delegate_cancellation.is_cancelled()
        && !run_cancellation.is_cancelled()
    {
        // Caller drop is an ownership cancellation even when the transport token was not
        // explicitly cancelled. Mark the same node/control token so tree settlement is truthful.
        delegate_cancellation.cancel();
    }
    if cancelled {
        let _ = agent_control.interrupt_agent(spawned.thread_id).await;
    }
    if let Err(error) = agent_control
        .settle_native_agent(spawned.thread_id, NATIVE_AGENT_SHUTDOWN_TIMEOUT)
        .await
    {
        let outcome = Err(bounded_error(format!(
            "native agent cleanup failed after forced settlement: {error}"
        )));
        tree_call.settle(&outcome);
        return outcome;
    }

    let outcome = if cancelled {
        Ok(NativeToolOutcome::Failure {
            message: if delegate_cancellation.is_cancelled() && !run_cancellation.is_cancelled() {
                "native agent cancelled by user".to_string()
            } else {
                "native agent cancelled with its run".to_string()
            },
        })
    } else {
        match terminal {
            AgentStatus::Completed(Some(output)) => {
                let output = output.into_bytes();
                if output.len() > NATIVE_CALL_BYTES
                    || !reserve_artifact_bytes(artifact_bytes.as_ref(), output.len())
                {
                    Ok(NativeToolOutcome::Failure {
                        message: "native agent result exceeded its bounded artifact budget"
                            .to_string(),
                    })
                } else {
                    Ok(NativeToolOutcome::Success { output })
                }
            }
            AgentStatus::Completed(None) => Ok(NativeToolOutcome::Failure {
                message: "native agent completed without a final result".to_string(),
            }),
            AgentStatus::Errored(message) => Ok(NativeToolOutcome::Failure {
                message: bounded_error(message),
            }),
            AgentStatus::Interrupted => Ok(NativeToolOutcome::Failure {
                message: "native agent was interrupted".to_string(),
            }),
            AgentStatus::Shutdown
            | AgentStatus::NotFound
            | AgentStatus::PendingInit
            | AgentStatus::Running => Ok(NativeToolOutcome::Failure {
                message: "native agent ended without a terminal result".to_string(),
            }),
        }
    };
    tree_call.settle_with_recent(&outcome, agent_recent(&outcome));
    outcome
}

fn reserve_artifact_bytes(counter: &AtomicUsize, bytes: usize) -> bool {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current
                .checked_add(bytes)
                .filter(|next| *next <= NATIVE_TOTAL_ARTIFACT_BYTES)
        })
        .is_ok()
}

fn encode_tool_result(value: &serde_json::Value) -> Result<Vec<u8>, String> {
    serde_json::to_vec(value)
        .map_err(|error| bounded_error(format!("failed to encode tool result: {error}")))
}

struct TreeCallSettlement {
    run_tree: NativeRunTreeOwner,
    stable_id: String,
    delegate_cancellation: CancellationToken,
    run_cancellation: CancellationToken,
    settled: bool,
}

impl TreeCallSettlement {
    fn new(
        run_tree: NativeRunTreeOwner,
        stable_id: String,
        delegate_cancellation: CancellationToken,
        run_cancellation: CancellationToken,
    ) -> Self {
        Self {
            run_tree,
            stable_id,
            delegate_cancellation,
            run_cancellation,
            settled: false,
        }
    }

    fn settle(&mut self, outcome: &Result<NativeToolOutcome, String>) {
        self.settle_with_recent(outcome, default_outcome_recent(outcome));
    }

    fn settle_with_recent(&mut self, outcome: &Result<NativeToolOutcome, String>, recent: &str) {
        let cancelled =
            self.delegate_cancellation.is_cancelled() || self.run_cancellation.is_cancelled();
        let status = match outcome {
            Ok(NativeToolOutcome::Success { .. }) => NativeRunNodeStatus::Succeeded,
            Ok(NativeToolOutcome::Retry { .. }) => NativeRunNodeStatus::Failed,
            Ok(NativeToolOutcome::Failure { .. }) => {
                if cancelled {
                    NativeRunNodeStatus::Cancelled
                } else {
                    NativeRunNodeStatus::Failed
                }
            }
            Err(_) => {
                if cancelled {
                    NativeRunNodeStatus::Cancelled
                } else {
                    NativeRunNodeStatus::Failed
                }
            }
        };
        self.run_tree.settle(&self.stable_id, status, recent);
        self.settled = true;
    }
}

fn default_outcome_recent(outcome: &Result<NativeToolOutcome, String>) -> &str {
    match outcome {
        Ok(NativeToolOutcome::Success { .. }) => "completed",
        Ok(NativeToolOutcome::Retry { reason }) => reason,
        Ok(NativeToolOutcome::Failure { message }) => message,
        Err(error) => error,
    }
}

impl Drop for TreeCallSettlement {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let cancelled =
            self.delegate_cancellation.is_cancelled() || self.run_cancellation.is_cancelled();
        self.run_tree.settle(
            &self.stable_id,
            if cancelled {
                NativeRunNodeStatus::Cancelled
            } else {
                NativeRunNodeStatus::Failed
            },
            if cancelled {
                "tool call cancelled before settlement"
            } else {
                "tool call ended before settlement"
            },
        );
    }
}

impl NativeCodeModeDelegate for NativeCodeModeDispatchWorker {
    fn invoke<'a>(
        &'a self,
        invocation: NativeToolInvocation,
        cancellation: CancellationToken,
    ) -> NativeToolFuture<'a> {
        Box::pin(self.invoke_inner(invocation, cancellation))
    }

    fn settle_invocation<'a>(
        &'a self,
        runtime_call_id: &'a str,
    ) -> codex_code_mode::NativeSettleFuture<'a> {
        Box::pin(self.settle_agent_invocation(runtime_call_id))
    }
}

struct ActiveCounter {
    counter: Arc<AtomicUsize>,
}

impl ActiveCounter {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        Self { counter }
    }
}

impl Drop for ActiveCounter {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

fn exact_handler_payload_bytes(payload: &ToolPayload) -> usize {
    match payload {
        ToolPayload::Function { arguments } => arguments.len(),
        ToolPayload::Custom { input } => input.len(),
        ToolPayload::ToolSearch { .. } => unreachable!("native Code Mode exposes no tool search"),
    }
}

fn native_request_summary(request: &NativeToolRequest) -> String {
    match request {
        NativeToolRequest::Shell {
            command,
            workdir,
            timeout_ms,
        } => format!(
            "shell request · {} command bytes · workdir {} · timeout {}ms",
            command.len(),
            if workdir.is_some() { "set" } else { "default" },
            timeout_ms
        ),
        NativeToolRequest::ApplyPatch { patch } => format!("apply patch · {} bytes", patch.len()),
        NativeToolRequest::Agent {
            task,
            model,
            reasoning_effort,
        } => format!(
            "agent task · {} bytes · model {} · reasoning {}",
            task.len(),
            if model.is_some() {
                "explicit"
            } else {
                "inherited"
            },
            if reasoning_effort.is_some() {
                "explicit"
            } else {
                "inherited"
            }
        ),
    }
}

fn validate_agent_fields(
    task: &str,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> Result<(), String> {
    bounded_nonempty("native agent task", task, NATIVE_AGENT_TASK_BYTES)?;
    if let Some(model) = model {
        bounded_nonempty("native agent model", model, NATIVE_AGENT_MODEL_BYTES)?;
    }
    if let Some(reasoning_effort) = reasoning_effort {
        bounded_nonempty(
            "native agent reasoning effort",
            reasoning_effort,
            NATIVE_AGENT_EFFORT_BYTES,
        )?;
    }
    Ok(())
}

fn bounded_nonempty(label: &str, value: &str, limit: usize) -> Result<(), String> {
    if value.is_empty() || value.len() > limit {
        return Err(format!("{label} must contain 1..={limit} bytes"));
    }
    Ok(())
}

fn exact_agent_request_bytes(
    task: &str,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> Result<usize, String> {
    serde_json::to_vec(&json!({
        "tool": "agent",
        "task": task,
        "model": model,
        "reasoningEffort": reasoning_effort,
    }))
    .map(|bytes| bytes.len())
    .map_err(|error| bounded_error(format!("failed to encode native agent request: {error}")))
}

fn agent_recent(outcome: &Result<NativeToolOutcome, String>) -> &str {
    match outcome {
        Ok(NativeToolOutcome::Success { output }) => {
            std::str::from_utf8(output).unwrap_or("agent completed with non-UTF-8 bounded output")
        }
        _ => default_outcome_recent(outcome),
    }
}

fn shell_handler_arguments(
    command: &str,
    workdir: Option<&str>,
    timeout_ms: u64,
) -> Result<String, String> {
    serde_json::to_string(&json!({
        "command": command,
        "workdir": workdir,
        "login": false,
        "timeout_ms": timeout_ms,
    }))
    .map_err(|error| format!("failed to encode native shell call: {error}"))
}

fn bounded_error(mut message: String) -> String {
    const MARKER: &str = "...[truncated]";
    if message.len() <= NATIVE_CALL_BYTES {
        return message;
    }
    let mut end = NATIVE_CALL_BYTES.saturating_sub(MARKER.len());
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push_str(MARKER);
    message
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::path::PathBuf;

    use codex_code_mode::NativeExecute;
    use codex_code_mode::NativeExecution;
    use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
    use codex_login::CodexAuth;
    use codex_model_provider_info::built_in_model_providers;
    use codex_tools::ShellCommandBackendConfig;
    use core_test_support::responses::ev_assistant_message;
    use core_test_support::responses::ev_completed_with_tokens;
    use core_test_support::responses::ev_response_created;
    use core_test_support::responses::mount_sse_once_match;
    use core_test_support::responses::sse;
    use pretty_assertions::assert_eq;
    use wiremock::Mock;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::body_string_contains;
    use wiremock::matchers::method;
    use wiremock::matchers::path_regex;

    use super::*;
    use crate::StartThreadOptions;
    use crate::ThreadManager;
    use crate::session::step_context::StepContext;
    use crate::tools::handlers::ShellCommandHandler;
    use crate::tools::handlers::apply_patch::ApplyPatchHandler;
    use crate::tools::registry::ToolRegistry;
    use crate::tools::router::ToolRouter;
    use crate::turn_diff_tracker::TurnDiffTracker;

    struct NativeLifecycleStartCounter(Arc<AtomicUsize>);

    impl codex_extension_api::ToolLifecycleContributor for NativeLifecycleStartCounter {
        fn on_tool_start<'a>(
            &'a self,
            _input: codex_extension_api::ToolStartInput<'a>,
        ) -> codex_extension_api::ToolLifecycleFuture<'a> {
            self.0.fetch_add(1, Ordering::AcqRel);
            Box::pin(async {})
        }
    }

    #[test]
    fn native_agents_use_shared_agent_control_with_scoped_loopback_rollouts() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_stack_size(16 * 1024 * 1024)
            .enable_all()
            .build()
            .expect("production-sized Tokio runtime")
            .block_on(native_agents_use_shared_agent_control_with_scoped_loopback_rollouts_inner());
    }

    async fn native_agents_use_shared_agent_control_with_scoped_loopback_rollouts_inner() {
        let server = core_test_support::responses::start_mock_server().await;
        let producer = mount_native_agent_response(
            &server,
            "producer task",
            "producer-result",
            "native-producer-response",
            11,
        )
        .await;
        let critic = mount_native_agent_response(
            &server,
            "critic task",
            "critic-result",
            "native-critic-response",
            13,
        )
        .await;
        let verifier = mount_native_agent_response(
            &server,
            "verifier task",
            "verifier-result",
            "native-verifier-response",
            17,
        )
        .await;
        let _reuse = mount_native_agent_response(
            &server,
            "reuse task",
            "reuse-result",
            "native-reuse-response",
            5,
        )
        .await;
        let codex_home = tempfile::tempdir().expect("temp ycode home");
        let provider = {
            let mut provider = built_in_model_providers()["openai"].clone();
            provider.base_url = Some(server.uri());
            provider
        };
        let provider_for_config = provider.clone();
        let (mut session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                CodexAuth::from_api_key("native-agent-loopback"),
                Vec::new(),
                codex_home.path(),
                move |config| {
                    config.model_provider = provider_for_config;
                    config.agent_max_threads = Some(NATIVE_WORKER_AGENTS);
                },
            )
            .await;
        let manager = ThreadManager::with_models_provider_for_tests(
            CodexAuth::from_api_key("native-agent-loopback"),
            provider,
        );
        let agent_control = manager.agent_control();
        agent_control
            .rollout_budget()
            .configure(crate::config::RolloutBudgetConfig {
                limit_tokens: 1_000,
                reminder_at_remaining_tokens: Vec::new(),
                sampling_token_weight: 1.0,
                prefill_token_weight: 1.0,
            });
        let root = manager
            .start_thread(StartThreadOptions::new((*turn.config).clone()))
            .await
            .expect("native root thread starts");
        let session_mut = Arc::get_mut(&mut session).expect("test session is uniquely owned");
        session_mut.services.agent_control = agent_control.clone();
        session_mut.thread_id = root.thread_id;

        let history_before = session.clone_history().await.into_raw_items();
        let identity = NativeRunIdentity {
            session_id: "native-agent-session".to_string(),
            thread_id: "00000000-0000-4000-8000-000000000031".to_string(),
            run_id: "00000000-0000-4000-8000-000000000032".to_string(),
        };
        let (worker, tree_registry) = test_worker_and_registry(
            identity.clone(),
            Arc::clone(&session),
            Arc::clone(&turn),
            CancellationToken::new(),
        );
        let producer_call = worker.invoke_inner(
            native_agent_invocation(&identity, 1, "producer task"),
            CancellationToken::new(),
        );
        let critic_call = worker.invoke_inner(
            native_agent_invocation(&identity, 2, "critic task"),
            CancellationToken::new(),
        );
        let verifier_call = worker.invoke_inner(
            NativeToolInvocation {
                identity: identity.clone(),
                runtime_call_id: format!("native-{}-a1-3", identity.run_id),
                request: NativeToolRequest::Agent {
                    task: "verifier task".to_string(),
                    model: Some(turn.model_info.slug.clone()),
                    reasoning_effort: Some("high".to_string()),
                },
            },
            CancellationToken::new(),
        );
        let (producer_outcome, critic_outcome, verifier_outcome) =
            tokio::join!(producer_call, critic_call, verifier_call);

        assert_agent_success(producer_outcome, "producer-result");
        assert_agent_success(critic_outcome, "critic-result");
        assert_agent_success(verifier_outcome, "verifier-result");
        assert_eq!(worker.owned_agent_counts(), (0, 0));
        assert_eq!(agent_control.spawned_agent_count_for_test(), 0);
        assert_eq!(
            agent_control
                .rollout_budget()
                .weighted_tokens_used_for_test(),
            Some(41.0),
            "11 + 13 + 17 canonical worker usage is charged exactly once"
        );
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );
        let request_bodies = producer
            .requests()
            .into_iter()
            .chain(critic.requests())
            .chain(verifier.requests())
            .map(|request| request.body_json().to_string())
            .collect::<HashSet<_>>();
        assert_eq!(request_bodies.len(), 3);
        assert!(request_bodies.iter().any(|body| {
            body.contains("verifier task")
                && body.contains(&turn.model_info.slug)
                && body.contains("high")
        }));
        for body in request_bodies {
            let body: serde_json::Value =
                serde_json::from_str(&body).expect("captured request JSON");
            assert_eq!(body["model"].as_str(), Some(turn.model_info.slug.as_str()));
            let tools = body["tools"].as_array().expect("worker tools array");
            assert!(tools.iter().all(|tool| {
                !matches!(
                    tool.get("name").and_then(serde_json::Value::as_str),
                    Some(
                        "spawn_agent"
                            | "send_message"
                            | "followup_task"
                            | "wait_agent"
                            | "interrupt_agent"
                            | "list_agents"
                    )
                )
            }));
            let body = body.to_string();
            assert!(!body.contains("parent-history-secret"));
            assert!(body.contains(NATIVE_AGENT_DEVELOPER_CONTRACT));
            assert!(body.contains(
                "use an explicit override only when the human requested it or a clear task-specific reason requires it"
            ));
        }
        let snapshot = tree_registry
            .subscribe(&identity.thread_id, &identity.run_id)
            .expect("active native tree")
            .borrow()
            .clone()
            .expect("tree snapshot");
        let agent_nodes = snapshot
            .nodes
            .iter()
            .filter(|node| node.kind == NativeRunNodeKind::Agent)
            .collect::<Vec<_>>();
        assert_eq!(agent_nodes.len(), 3);
        assert!(
            agent_nodes
                .windows(2)
                .all(|pair| pair[0].launch_ordinal < pair[1].launch_ordinal)
        );
        assert!(
            agent_nodes
                .iter()
                .all(|node| node.status == NativeRunNodeStatus::Succeeded
                    && node.parent_id.as_deref() == Some("workflow-a1")
                    && !node.summary.contains("producer task")
                    && !node.summary.contains("critic task")
                    && !node.summary.contains("verifier task"))
        );
        assert_agent_success(
            worker
                .invoke_inner(
                    native_agent_invocation(&identity, 4, "reuse task"),
                    CancellationToken::new(),
                )
                .await,
            "reuse-result",
        );
        assert_eq!(agent_control.spawned_agent_count_for_test(), 0);
        assert_eq!(
            agent_control
                .rollout_budget()
                .weighted_tokens_used_for_test(),
            Some(46.0)
        );
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );
    }

    #[test]
    fn native_agent_usage_exhausts_the_shared_canonical_rollout_budget_once() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_stack_size(16 * 1024 * 1024)
            .enable_all()
            .build()
            .expect("production-sized Tokio runtime")
            .block_on(native_agent_shared_budget_inner());
    }

    async fn native_agent_shared_budget_inner() {
        let server = core_test_support::responses::start_mock_server().await;
        let _first = mount_native_agent_response(
            &server,
            "budget worker one",
            "first-result",
            "native-budget-one",
            11,
        )
        .await;
        let _second = mount_native_agent_response(
            &server,
            "budget worker two",
            "second-result",
            "native-budget-two",
            13,
        )
        .await;
        let (_home, session, turn, _manager, agent_control, _root_id) =
            native_agent_test_session(&server, NATIVE_WORKER_AGENTS).await;
        agent_control
            .rollout_budget()
            .configure(crate::config::RolloutBudgetConfig {
                limit_tokens: 20,
                reminder_at_remaining_tokens: Vec::new(),
                sampling_token_weight: 1.0,
                prefill_token_weight: 1.0,
            });
        let history_before = session.clone_history().await.into_raw_items();
        let identity = native_agent_identity(0x45, 0x46);
        let worker = test_worker(
            identity.clone(),
            Arc::clone(&session),
            Arc::clone(&turn),
            CancellationToken::new(),
        );
        assert_agent_success(
            worker
                .invoke_inner(
                    native_agent_invocation(&identity, 1, "budget worker one"),
                    CancellationToken::new(),
                )
                .await,
            "first-result",
        );
        let second = worker
            .invoke_inner(
                native_agent_invocation(&identity, 2, "budget worker two"),
                CancellationToken::new(),
            )
            .await
            .expect("budget exhaustion remains a typed worker outcome");
        assert!(matches!(
            second,
            NativeToolOutcome::Failure { message }
                if message == "shared rollout token budget exhausted"
        ));
        assert_eq!(
            agent_control
                .rollout_budget()
                .weighted_tokens_used_for_test(),
            Some(24.0)
        );
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );
        assert_eq!(worker.owned_agent_counts(), (0, 0));
        assert_eq!(agent_control.spawned_agent_count_for_test(), 0);
    }

    #[test]
    fn selected_native_agent_cancel_interrupts_agent_control_and_releases_ownership() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_stack_size(16 * 1024 * 1024)
            .enable_all()
            .build()
            .expect("production-sized Tokio runtime")
            .block_on(selected_native_agent_cancel_inner());
    }

    async fn selected_native_agent_cancel_inner() {
        let server = core_test_support::responses::start_mock_server().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/responses$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse(vec![
                        ev_response_created("pending-native-agent"),
                        ev_assistant_message("pending-native-message", "too late"),
                        ev_completed_with_tokens("pending-native-agent", 19),
                    ]))
                    .set_delay(Duration::from_secs(5)),
            )
            .mount(&server)
            .await;
        let codex_home = tempfile::tempdir().expect("temp ycode home");
        let provider = {
            let mut provider = built_in_model_providers()["openai"].clone();
            provider.base_url = Some(server.uri());
            provider
        };
        let provider_for_config = provider.clone();
        let (mut session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                CodexAuth::from_api_key("native-agent-cancel-loopback"),
                Vec::new(),
                codex_home.path(),
                move |config| {
                    config.model_provider = provider_for_config;
                    config.agent_max_threads = Some(NATIVE_WORKER_AGENTS);
                },
            )
            .await;
        let manager = ThreadManager::with_models_provider_for_tests(
            CodexAuth::from_api_key("native-agent-cancel-loopback"),
            provider,
        );
        let root = manager
            .start_thread(StartThreadOptions::new((*turn.config).clone()))
            .await
            .expect("native root thread starts");
        let agent_control = manager.agent_control();
        let session_mut = Arc::get_mut(&mut session).expect("test session is uniquely owned");
        session_mut.services.agent_control = agent_control.clone();
        session_mut.thread_id = root.thread_id;
        let history_before = session.clone_history().await.into_raw_items();
        let identity = NativeRunIdentity {
            session_id: "native-agent-cancel".to_string(),
            thread_id: "00000000-0000-4000-8000-000000000035".to_string(),
            run_id: "00000000-0000-4000-8000-000000000036".to_string(),
        };
        let (worker, tree_registry) = test_worker_and_registry(
            identity.clone(),
            Arc::clone(&session),
            Arc::clone(&turn),
            CancellationToken::new(),
        );
        let call_cancellation = CancellationToken::new();
        let invoke_worker = Arc::clone(&worker);
        let invocation = native_agent_invocation(&identity, 1, "pending verifier task");
        let call_cancel_for_task = call_cancellation.clone();
        let invoke = tokio::spawn(async move {
            invoke_worker
                .invoke_inner(invocation, call_cancel_for_task)
                .await
        });
        let mut observation = tree_registry
            .subscribe(&identity.thread_id, &identity.run_id)
            .expect("active tree");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if observation.borrow().as_ref().is_some_and(|snapshot| {
                    snapshot.nodes.iter().any(|node| {
                        node.stable_id == "agent-native-00000000-0000-4000-8000-000000000036-a1-1"
                            && node.status == NativeRunNodeStatus::Running
                    })
                }) {
                    break;
                }
                observation.changed().await.expect("tree remains active");
            }
        })
        .await
        .expect("agent node becomes observable after AgentControl admission");
        let started = Instant::now();
        assert_eq!(
            tree_registry
                .cancel(
                    &identity.thread_id,
                    &identity.run_id,
                    "agent-native-00000000-0000-4000-8000-000000000036-a1-1",
                )
                .expect("selected agent cancel routes"),
            crate::native_run_tree::NativeRunCancelResult::Requested
        );
        let outcome = tokio::time::timeout(Duration::from_secs(1), invoke)
            .await
            .expect("selected agent cancellation settles within one second")
            .expect("invoke task joins")
            .expect("selected cancellation returns a typed outcome");
        assert!(matches!(
            outcome,
            NativeToolOutcome::Failure { message }
                if message == "native agent cancelled by user"
        ));
        let selected_elapsed = started.elapsed();
        eprintln!(
            "native selected-agent cancellation settled in {:.3} ms",
            selected_elapsed.as_secs_f64() * 1_000.0
        );
        assert!(selected_elapsed < Duration::from_secs(1));
        assert_eq!(worker.owned_counts(), (0, 0));
        assert_eq!(worker.owned_agent_counts(), (0, 0));
        assert_terminal_and_not_cancellable(
            &tree_registry,
            &identity,
            "agent-native-00000000-0000-4000-8000-000000000036-a1-1",
            NativeRunNodeStatus::Cancelled,
        );
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );

        let run_identity = NativeRunIdentity {
            session_id: "native-agent-cancel".to_string(),
            thread_id: "00000000-0000-4000-8000-000000000037".to_string(),
            run_id: "00000000-0000-4000-8000-000000000038".to_string(),
        };
        let (run_worker, run_tree_registry) = test_worker_and_registry(
            run_identity.clone(),
            Arc::clone(&session),
            Arc::clone(&turn),
            CancellationToken::new(),
        );
        let invoke_worker = Arc::clone(&run_worker);
        let invocation = native_agent_invocation(&run_identity, 1, "pending critic task");
        let invoke = tokio::spawn(async move {
            invoke_worker
                .invoke_inner(invocation, CancellationToken::new())
                .await
        });
        let mut observation = run_tree_registry
            .subscribe(&run_identity.thread_id, &run_identity.run_id)
            .expect("active run-cancel tree");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if observation.borrow().as_ref().is_some_and(|snapshot| {
                    snapshot.nodes.iter().any(|node| {
                        node.kind == NativeRunNodeKind::Agent
                            && node.status == NativeRunNodeStatus::Running
                    })
                }) {
                    break;
                }
                observation.changed().await.expect("tree remains active");
            }
        })
        .await
        .expect("run-owned agent becomes observable");
        let started = Instant::now();
        assert_eq!(
            run_tree_registry
                .cancel(&run_identity.thread_id, &run_identity.run_id, "run")
                .expect("root cancel routes"),
            crate::native_run_tree::NativeRunCancelResult::Requested
        );
        let outcome = tokio::time::timeout(Duration::from_secs(1), invoke)
            .await
            .expect("root cancellation settles native agent within one second")
            .expect("root cancellation task joins")
            .expect("root cancellation returns typed outcome");
        assert!(matches!(
            outcome,
            NativeToolOutcome::Failure { message }
                if message == "native agent cancelled with its run"
        ));
        let run_elapsed = started.elapsed();
        eprintln!(
            "native run cancellation with pending agent settled in {:.3} ms",
            run_elapsed.as_secs_f64() * 1_000.0
        );
        assert!(run_elapsed < Duration::from_secs(1));
        assert_eq!(run_worker.owned_counts(), (0, 0));
        assert_eq!(run_worker.owned_agent_counts(), (0, 0));
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );
        assert_eq!(agent_control.spawned_agent_count_for_test(), 0);
    }

    #[test]
    fn shared_agent_control_admits_three_real_native_workers_and_rejects_fourth() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_stack_size(16 * 1024 * 1024)
            .enable_all()
            .build()
            .expect("production-sized Tokio runtime")
            .block_on(shared_agent_control_hard_cap_inner());
    }

    async fn shared_agent_control_hard_cap_inner() {
        let server = pending_native_agent_server().await;
        let (_home, session, turn, manager, _agent_control, root_id) =
            native_agent_test_session(&server, NATIVE_WORKER_AGENTS).await;
        let identity = native_agent_identity(0x33, 0x34);
        let (worker, registry) = test_worker_and_registry(
            identity.clone(),
            Arc::clone(&session),
            Arc::clone(&turn),
            CancellationToken::new(),
        );
        let mut calls = Vec::new();
        let mut cancellations = Vec::new();
        for ordinal in 1..=NATIVE_WORKER_AGENTS {
            let cancellation = CancellationToken::new();
            cancellations.push(cancellation.clone());
            let invoke_worker = Arc::clone(&worker);
            let invocation = native_agent_invocation(&identity, ordinal, "pending cap worker");
            calls.push(tokio::spawn(async move {
                invoke_worker.invoke_inner(invocation, cancellation).await
            }));
        }
        for ordinal in 1..=NATIVE_WORKER_AGENTS {
            wait_for_agent_node(
                &registry,
                &identity,
                &format!("native-{}-a1-{ordinal}", identity.run_id),
            )
            .await;
        }
        assert_eq!(
            manager.list_thread_ids().await.len(),
            4,
            "root + three workers"
        );
        let outcome = worker
            .invoke_inner(
                native_agent_invocation(&identity, 4, "actual fourth worker"),
                CancellationToken::new(),
            )
            .await
            .expect("hard-cap rejection is typed");
        assert_eq!(
            outcome,
            NativeToolOutcome::Failure {
                message: "native run allows at most three concurrent worker agents".to_string(),
            }
        );
        assert_eq!(manager.list_thread_ids().await.len(), 4);
        let snapshot = registry
            .subscribe(&identity.thread_id, &identity.run_id)
            .expect("active tree")
            .borrow()
            .clone()
            .expect("tree snapshot");
        let ordinals = snapshot
            .nodes
            .iter()
            .filter(|node| node.kind == NativeRunNodeKind::Agent)
            .map(|node| node.launch_ordinal)
            .collect::<Vec<_>>();
        assert_eq!(ordinals.len(), 3);
        assert!(ordinals.windows(2).all(|pair| pair[0] < pair[1]));
        for cancellation in cancellations {
            cancellation.cancel();
        }
        for call in calls {
            let _ = call.await.expect("native cap call joins");
        }
        wait_for_native_agent_zero(&worker).await;
        assert_eq!(manager.list_thread_ids().await, vec![root_id]);

        let reuse_cancel = CancellationToken::new();
        let reuse_worker = Arc::clone(&worker);
        let reuse_invocation = native_agent_invocation(&identity, 5, "reused native slot");
        let reuse_cancel_for_task = reuse_cancel.clone();
        let reuse = tokio::spawn(async move {
            reuse_worker
                .invoke_inner(reuse_invocation, reuse_cancel_for_task)
                .await
        });
        wait_for_agent_node(
            &registry,
            &identity,
            &format!("native-{}-a1-5", identity.run_id),
        )
        .await;
        reuse_cancel.cancel();
        let _ = reuse.await.expect("reused slot joins");
        wait_for_native_agent_zero(&worker).await;
        assert_eq!(manager.list_thread_ids().await, vec![root_id]);
    }

    #[test]
    fn ordinary_agent_and_lower_shared_limit_reduce_native_admission() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(3)
            .thread_stack_size(16 * 1024 * 1024)
            .enable_all()
            .build()
            .expect("production-sized Tokio runtime")
            .block_on(ordinary_agent_and_lower_limit_inner());
    }

    async fn ordinary_agent_and_lower_limit_inner() {
        let server = pending_native_agent_server().await;
        let (_home, session, turn, manager, agent_control, root_id) =
            native_agent_test_session(&server, 2).await;
        let ordinary = agent_control
            .spawn_agent_with_metadata(
                (*turn.config).clone(),
                vec![UserInput::Text {
                    text: "ordinary pending child".to_string(),
                    text_elements: Vec::new(),
                }],
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: root_id,
                    depth: 1,
                    agent_path: Some(
                        AgentPath::root()
                            .join("ordinary_pending")
                            .expect("ordinary path"),
                    ),
                    agent_nickname: None,
                    agent_role: None,
                })),
                SpawnAgentOptions {
                    parent_thread_id: Some(root_id),
                    parent_turn_id: Some(turn.sub_id.clone()),
                    ..Default::default()
                },
            )
            .await
            .expect("ordinary AgentControl child admitted");
        let identity = native_agent_identity(0x35, 0x36);
        let (worker, registry) = test_worker_and_registry(
            identity.clone(),
            Arc::clone(&session),
            Arc::clone(&turn),
            CancellationToken::new(),
        );
        let first_cancel = CancellationToken::new();
        let invoke_worker = Arc::clone(&worker);
        let first_invocation = native_agent_invocation(&identity, 1, "one shared native slot");
        let first_cancel_for_task = first_cancel.clone();
        let first = tokio::spawn(async move {
            invoke_worker
                .invoke_inner(first_invocation, first_cancel_for_task)
                .await
        });
        wait_for_agent_node(
            &registry,
            &identity,
            &format!("native-{}-a1-1", identity.run_id),
        )
        .await;
        let rejected = worker
            .invoke_inner(
                native_agent_invocation(&identity, 2, "lower limit must win"),
                CancellationToken::new(),
            )
            .await
            .expect_err("shared AgentRegistry rejects over-admission");
        assert_eq!(
            rejected,
            "native agent admission failed: agent thread limit reached"
        );
        assert_eq!(
            manager.list_thread_ids().await.len(),
            3,
            "root + ordinary + native"
        );
        first_cancel.cancel();
        let _ = first.await.expect("native call joins");
        agent_control
            .shutdown_agent_tree(ordinary.thread_id)
            .await
            .expect("ordinary child shutdown");
        wait_for_native_agent_zero(&worker).await;
        assert_eq!(manager.list_thread_ids().await, vec![root_id]);
    }

    #[test]
    fn native_agent_ownership_survives_caller_drop_and_forced_cleanup() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_stack_size(16 * 1024 * 1024)
            .enable_all()
            .build()
            .expect("production-sized Tokio runtime")
            .block_on(native_agent_ownership_survives_caller_drop_inner());
    }

    async fn native_agent_ownership_survives_caller_drop_inner() {
        let server = pending_native_agent_server().await;
        let (_home, session, turn, manager, agent_control, root_id) =
            native_agent_test_session(&server, NATIVE_WORKER_AGENTS).await;
        let identity = native_agent_identity(0x41, 0x42);
        let history_before = session.clone_history().await.into_raw_items();
        let (worker, tree_registry) = test_worker_and_registry(
            identity.clone(),
            Arc::clone(&session),
            Arc::clone(&turn),
            CancellationToken::new(),
        );
        let runtime_call_id = format!("native-{}-a1-1", identity.run_id);
        let invoke_worker = Arc::clone(&worker);
        let invocation = native_agent_invocation(&identity, 1, "pending owned task");
        let invoke = tokio::spawn(async move {
            invoke_worker
                .invoke_inner(invocation, CancellationToken::new())
                .await
        });
        wait_for_agent_node(&tree_registry, &identity, &runtime_call_id).await;
        let worker_thread = manager
            .list_thread_ids()
            .await
            .into_iter()
            .find(|thread_id| *thread_id != root_id)
            .expect("native AgentControl worker admitted");
        agent_control.delay_next_native_graceful_shutdown_for_test(Duration::from_millis(600));
        let started = Instant::now();
        invoke.abort();
        let _ = invoke.await;
        worker
            .settle_agent_invocation(&runtime_call_id)
            .await
            .expect("caller-drop cleanup is explicitly forced and joined");
        wait_for_native_agent_zero(&worker).await;
        assert_eq!(worker.owned_agent_task_count(), 0);
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(1), "elapsed={elapsed:?}");
        assert!(!manager.list_thread_ids().await.contains(&worker_thread));
        assert_eq!(manager.list_thread_ids().await, vec![root_id]);
        assert_eq!(agent_control.spawned_agent_count_for_test(), 0);
        assert_eq!(worker.owned_counts(), (0, 0));
        assert_eq!(worker.owned_agent_counts(), (0, 0));
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );

        let reuse_cancel = CancellationToken::new();
        let reuse_worker = Arc::clone(&worker);
        let reuse_cancel_for_task = reuse_cancel.clone();
        let reuse_invocation = native_agent_invocation(&identity, 2, "reuse after caller drop");
        let reuse = tokio::spawn(async move {
            reuse_worker
                .invoke_inner(reuse_invocation, reuse_cancel_for_task)
                .await
        });
        wait_for_agent_node(
            &tree_registry,
            &identity,
            &format!("native-{}-a1-2", identity.run_id),
        )
        .await;
        reuse_cancel.cancel();
        let _ = reuse.await.expect("caller-drop reuse call joins");
        wait_for_native_agent_zero(&worker).await;
        assert_eq!(manager.list_thread_ids().await, vec![root_id]);
        assert_eq!(agent_control.spawned_agent_count_for_test(), 0);
        assert_terminal_and_not_cancellable(
            &tree_registry,
            &identity,
            &format!("agent-{runtime_call_id}"),
            NativeRunNodeStatus::Cancelled,
        );
    }

    #[test]
    fn native_agent_registration_window_drop_is_keyed_and_transactional() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_stack_size(16 * 1024 * 1024)
            .enable_all()
            .build()
            .expect("production-sized Tokio runtime")
            .block_on(native_agent_registration_window_drop_inner());
    }

    async fn native_agent_registration_window_drop_inner() {
        let server = pending_native_agent_server().await;
        let (_home, session, turn, manager, agent_control, root_id) =
            native_agent_test_session(&server, NATIVE_WORKER_AGENTS).await;
        let identity = native_agent_identity(0x49, 0x4a);
        let history_before = session.clone_history().await.into_raw_items();
        let (worker, tree_registry) = test_worker_and_registry(
            identity.clone(),
            Arc::clone(&session),
            Arc::clone(&turn),
            CancellationToken::new(),
        );
        let mut thread_created = manager.subscribe_thread_created();
        let registration_pause =
            agent_control.pause_next_native_spawn_after_registration_for_test();
        let runtime_call_id = format!("native-{}-a1-1", identity.run_id);
        let invoke_worker = Arc::clone(&worker);
        let invocation = native_agent_invocation(&identity, 1, "cancel during registration");
        let mut invoke = tokio::spawn(async move {
            invoke_worker
                .invoke_inner(invocation, CancellationToken::new())
                .await
        });

        tokio::select! {
            _ = registration_pause.wait_until_reached() => {}
            result = &mut invoke => panic!("native invoke ended before registration pause: {result:?}"),
        }
        let worker_thread = manager
            .list_thread_ids()
            .await
            .into_iter()
            .find(|thread_id| *thread_id != root_id)
            .expect("native worker committed before initial-input admission");
        assert_eq!(agent_control.spawned_agent_count_for_test(), 1);
        assert!(thread_created.try_recv().is_err());
        assert!(
            agent_control
                .persisted_native_spawn_children_for_test(root_id)
                .await
                .expect("native spawn edges load")
                .is_empty(),
            "pre-input native ownership must not publish a durable edge"
        );
        assert!(
            !tree_registry
                .subscribe(&identity.thread_id, &identity.run_id)
                .expect("native tree remains registered")
                .borrow()
                .clone()
                .expect("native tree snapshot")
                .nodes
                .iter()
                .any(|node| node.kind == NativeRunNodeKind::Agent),
            "the Agent node is not visible before input admission"
        );

        let started = Instant::now();
        invoke.abort();
        let _ = invoke.await;
        worker
            .settle_agent_invocation(&runtime_call_id)
            .await
            .expect("registration-window owner is keyed, settled, and joined");
        assert!(started.elapsed() < Duration::from_secs(1));
        wait_for_native_agent_zero(&worker).await;
        assert_eq!(worker.owned_agent_task_count(), 0);
        assert_eq!(worker.owned_counts(), (0, 0));
        assert_eq!(worker.owned_agent_counts(), (0, 0));
        assert_eq!(manager.list_thread_ids().await, vec![root_id]);
        assert!(!manager.list_thread_ids().await.contains(&worker_thread));
        assert_eq!(agent_control.spawned_agent_count_for_test(), 0);
        assert_eq!(agent_control.native_residency_counts_for_test(), (0, 0));
        assert!(thread_created.try_recv().is_err());
        assert!(
            agent_control
                .persisted_native_spawn_children_for_test(root_id)
                .await
                .expect("settled native spawn edges load")
                .is_empty()
        );
        assert!(session.list_background_terminals().await.is_empty());
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );

        let reuse_cancel = CancellationToken::new();
        let reuse_worker = Arc::clone(&worker);
        let reuse_cancel_for_task = reuse_cancel.clone();
        let reuse_invocation = native_agent_invocation(&identity, 2, "reuse after registration");
        let reuse = tokio::spawn(async move {
            reuse_worker
                .invoke_inner(reuse_invocation, reuse_cancel_for_task)
                .await
        });
        wait_for_agent_node(
            &tree_registry,
            &identity,
            &format!("native-{}-a1-2", identity.run_id),
        )
        .await;
        reuse_cancel.cancel();
        let _ = reuse.await.expect("registration-window reuse joins");
        wait_for_native_agent_zero(&worker).await;
        assert_eq!(manager.list_thread_ids().await, vec![root_id]);
        assert_eq!(agent_control.spawned_agent_count_for_test(), 0);
        assert_eq!(agent_control.native_residency_counts_for_test(), (0, 0));
    }

    #[test]
    fn native_agent_partial_spawn_and_subscription_failures_are_transactional() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_stack_size(16 * 1024 * 1024)
            .enable_all()
            .build()
            .expect("production-sized Tokio runtime")
            .block_on(native_agent_partial_failures_inner());
    }

    async fn native_agent_partial_failures_inner() {
        let server = pending_native_agent_server().await;
        let (_home, session, turn, manager, agent_control, root_id) =
            native_agent_test_session(&server, NATIVE_WORKER_AGENTS).await;
        let identity = native_agent_identity(0x43, 0x44);
        let history_before = session.clone_history().await.into_raw_items();
        let (worker, registry) = test_worker_and_registry(
            identity.clone(),
            Arc::clone(&session),
            Arc::clone(&turn),
            CancellationToken::new(),
        );
        let mut thread_created = manager.subscribe_thread_created();

        agent_control.fail_next_native_spawn_after_create_for_test();
        let error = worker
            .invoke_inner(
                native_agent_invocation(&identity, 1, "post-create failure"),
                CancellationToken::new(),
            )
            .await
            .expect_err("injected post-create failure is bounded");
        assert!(error.contains("post-create failure"));
        assert_eq!(manager.list_thread_ids().await, vec![root_id]);
        assert_eq!(worker.owned_agent_counts(), (0, 0));
        assert_eq!(agent_control.spawned_agent_count_for_test(), 0);
        assert!(thread_created.try_recv().is_err());

        agent_control.fail_next_native_status_subscription_for_test();
        let error = worker
            .invoke_inner(
                native_agent_invocation(&identity, 2, "subscription failure"),
                CancellationToken::new(),
            )
            .await
            .expect_err("injected subscription failure is bounded");
        assert!(error.contains("status unavailable"));
        assert_eq!(manager.list_thread_ids().await, vec![root_id]);
        let _created_then_rolled_back = thread_created
            .try_recv()
            .expect("successful native input admission emits one creation notification");
        assert!(thread_created.try_recv().is_err());
        assert_eq!(agent_control.spawned_agent_count_for_test(), 0);
        assert_eq!(worker.owned_counts(), (0, 0));
        assert_eq!(worker.owned_agent_counts(), (0, 0));
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );

        let reuse_cancel = CancellationToken::new();
        let reuse_worker = Arc::clone(&worker);
        let reuse_cancel_for_task = reuse_cancel.clone();
        let reuse_invocation = native_agent_invocation(&identity, 3, "reuse after failures");
        let reuse = tokio::spawn(async move {
            reuse_worker
                .invoke_inner(reuse_invocation, reuse_cancel_for_task)
                .await
        });
        wait_for_agent_node(
            &registry,
            &identity,
            &format!("native-{}-a1-3", identity.run_id),
        )
        .await;
        reuse_cancel.cancel();
        let _ = reuse.await.expect("failure-reuse call joins");
        wait_for_native_agent_zero(&worker).await;
        assert_eq!(manager.list_thread_ids().await, vec![root_id]);
        assert_eq!(agent_control.spawned_agent_count_for_test(), 0);
    }

    #[tokio::test]
    async fn escaped_shell_payload_is_bounded_before_dispatch_and_counted_exactly() {
        let (mut session, turn) = crate::session::tests::make_session_and_context().await;
        let lifecycle_starts = Arc::new(AtomicUsize::new(0));
        let mut extensions =
            codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
        extensions.tool_lifecycle_contributor(Arc::new(NativeLifecycleStartCounter(Arc::clone(
            &lifecycle_starts,
        ))));
        session.services.extensions = Arc::new(extensions.build());
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let history_before = session.clone_history().await.into_raw_items();
        let identity = NativeRunIdentity {
            session_id: "native-bounds".to_string(),
            thread_id: "00000000-0000-4000-8000-000000000011".to_string(),
            run_id: "00000000-0000-4000-8000-000000000012".to_string(),
        };
        let (worker, tree_registry) = test_worker_and_registry(
            identity.clone(),
            Arc::clone(&session),
            Arc::clone(&turn),
            CancellationToken::new(),
        );
        let escaped = "\u{0001}".repeat(NATIVE_CALL_BYTES / 2);
        assert!(escaped.len() <= NATIVE_CALL_BYTES);
        let encoded = shell_handler_arguments(&escaped, None, 1_000).expect("encode shell payload");
        assert!(encoded.len() > NATIVE_CALL_BYTES);

        let outcome = worker
            .invoke_inner(
                NativeToolInvocation {
                    identity: identity.clone(),
                    runtime_call_id: "native-00000000-0000-4000-8000-000000000012-a1-1".to_string(),
                    request: NativeToolRequest::Shell {
                        command: escaped,
                        workdir: None,
                        timeout_ms: 1_000,
                    },
                },
                CancellationToken::new(),
            )
            .await
            .expect("oversized encoded request should be a typed failure");
        assert!(matches!(
            outcome,
            NativeToolOutcome::Failure { message }
                if message == "native call request exceeded its bounded artifact budget"
        ));
        assert_eq!(worker.owned_counts(), (0, 0));
        assert_eq!(worker.artifact_bytes.load(Ordering::Acquire), 0);
        assert_eq!(lifecycle_starts.load(Ordering::Acquire), 0);
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );
        assert!(session.list_background_terminals().await.is_empty());

        let exact = shell_handler_arguments("printf ok", Some("/tmp"), 1_000)
            .expect("encode exact shell payload")
            .len();
        worker
            .artifact_bytes
            .store(NATIVE_TOTAL_ARTIFACT_BYTES - exact, Ordering::Release);
        let accepted = worker
            .invoke_inner(
                NativeToolInvocation {
                    identity: identity.clone(),
                    runtime_call_id: "native-00000000-0000-4000-8000-000000000012-a1-2".to_string(),
                    request: NativeToolRequest::Shell {
                        command: "printf ok".to_string(),
                        workdir: Some("/tmp".to_string()),
                        timeout_ms: 1_000,
                    },
                },
                CancellationToken::new(),
            )
            .await
            .expect("exact aggregate boundary should admit one request");
        assert!(matches!(
            accepted,
            NativeToolOutcome::Failure { message }
                if message == "native tool result exceeded its bounded artifact budget"
        ));
        assert_eq!(
            worker.artifact_bytes.load(Ordering::Acquire),
            NATIVE_TOTAL_ARTIFACT_BYTES
        );
        assert_eq!(lifecycle_starts.load(Ordering::Acquire), 1);
        assert_terminal_and_not_cancellable(
            &tree_registry,
            &identity,
            "call-native-00000000-0000-4000-8000-000000000012-a1-2",
            NativeRunNodeStatus::Failed,
        );

        let rejected = worker
            .invoke_inner(
                NativeToolInvocation {
                    identity: identity.clone(),
                    runtime_call_id: "native-00000000-0000-4000-8000-000000000012-a1-3".to_string(),
                    request: NativeToolRequest::ApplyPatch {
                        patch: "x".to_string(),
                    },
                },
                CancellationToken::new(),
            )
            .await
            .expect("aggregate overflow should be a typed failure");
        assert!(matches!(
            rejected,
            NativeToolOutcome::Failure { message }
                if message == "native call request exceeded its bounded artifact budget"
        ));
        assert_eq!(worker.owned_counts(), (0, 0));
        assert_eq!(lifecycle_starts.load(Ordering::Acquire), 1);

        worker.artifact_bytes.store(0, Ordering::Release);
        let encoding_failure = worker
            .invoke_inner_with_encoder(
                NativeToolInvocation {
                    identity: identity.clone(),
                    runtime_call_id: "native-00000000-0000-4000-8000-000000000012-a1-4".to_string(),
                    request: NativeToolRequest::Shell {
                        command: "printf ok".to_string(),
                        workdir: Some("/tmp".to_string()),
                        timeout_ms: 1_000,
                    },
                },
                CancellationToken::new(),
                |_| Err("failed to encode tool result: injected serializer failure".to_string()),
            )
            .await;
        assert!(
            encoding_failure
                .unwrap_err()
                .contains("injected serializer failure")
        );
        assert_terminal_and_not_cancellable(
            &tree_registry,
            &identity,
            "call-native-00000000-0000-4000-8000-000000000012-a1-4",
            NativeRunNodeStatus::Failed,
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_client_executes_real_shell_and_apply_patch_without_history_insertion() {
        let Some(host_program) = std::env::var_os("YCODE_TEST_CODE_MODE_HOST") else {
            eprintln!("skipping native client integration without YCODE_TEST_CODE_MODE_HOST");
            return;
        };
        let workspace = tempfile::tempdir().expect("temp workspace");
        let codex_home = tempfile::tempdir().expect("temp ycode home");
        let _home = EnvGuard::set("CODEX_HOME", codex_home.path().as_os_str());
        let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
            workspace.path().to_path_buf(),
        )
        .expect("absolute temp workspace");
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_in_cwd_for_tests(
                codex_home.path(),
                cwd,
            )
            .await;
        let history_before = session.clone_history().await.into_raw_items();
        let identity = NativeRunIdentity {
            session_id: "native-session".to_string(),
            thread_id: "00000000-0000-4000-8000-000000000001".to_string(),
            run_id: "00000000-0000-4000-8000-000000000002".to_string(),
        };
        let cancellation = CancellationToken::new();
        let (worker, tree_registry) = test_worker_and_registry(
            identity.clone(),
            Arc::clone(&session),
            Arc::clone(&turn),
            cancellation.clone(),
        );
        let provider =
            ProcessOwnedCodeModeSessionProvider::with_host_program(PathBuf::from(host_program));
        let client = provider.native_client();
        std::fs::write(workspace.path().join("native-proof.txt"), "seed\n")
            .expect("seed concurrent fixture");
        let source = fixture_source(workspace.path());
        let execute_client = client.clone();
        let execute_identity = identity.clone();
        let execute_worker = Arc::clone(&worker);
        let execute_cancellation = cancellation.clone();
        let execute = tokio::spawn(async move {
            execute_client
                .execute(
                    NativeExecute {
                        identity: execute_identity,
                        attempt: 1,
                        task: "concurrently inspect and patch one deterministic file".to_string(),
                        source,
                    },
                    execute_worker,
                    execute_cancellation,
                )
                .await
        });
        let mut observation = tree_registry
            .subscribe(&identity.thread_id, &identity.run_id)
            .expect("active tree");
        let call_nodes = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = observation.borrow().clone().expect("active snapshot");
                let calls = snapshot
                    .nodes
                    .iter()
                    .filter(|node| node.kind == NativeRunNodeKind::ToolCall)
                    .cloned()
                    .collect::<Vec<_>>();
                if calls.len() == 2 {
                    break calls;
                }
                observation.changed().await.expect("tree remains active");
            }
        })
        .await
        .expect("both concurrent calls become observable");
        assert!(call_nodes[0].launch_ordinal < call_nodes[1].launch_ordinal);
        assert!(
            call_nodes
                .iter()
                .all(|node| node.parent_id.as_deref() == Some("workflow-a1"))
        );
        assert!(call_nodes[0].summary.starts_with("shell request ·"));
        assert!(call_nodes[1].summary.starts_with("apply patch · "));
        assert!(call_nodes[1].summary.ends_with(" bytes"));
        assert!(
            call_nodes
                .iter()
                .all(|node| !node.summary.contains("native-proof"))
        );
        let result = execute
            .await
            .expect("native task joins")
            .expect("native execution should complete");
        let NativeExecution::Completed { evidence, .. } = result else {
            panic!("native execution should return Evidence: {result:?}")
        };
        assert_eq!(evidence.summary, "real shell and apply_patch completed");
        assert_eq!(
            evidence.provenance_ids,
            [
                "native-00000000-0000-4000-8000-000000000002-a1-1",
                "native-00000000-0000-4000-8000-000000000002-a1-2",
            ]
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("native-proof.txt"))
                .expect("read native proof"),
            "seed\npatched\n"
        );
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );
        assert!(session.list_background_terminals().await.is_empty());
        assert_eq!(worker.owned_counts(), (0, 0));

        let shell_failure = worker
            .invoke_inner(
                NativeToolInvocation {
                    identity: identity.clone(),
                    runtime_call_id: "native-00000000-0000-4000-8000-000000000002-a1-3".to_string(),
                    request: NativeToolRequest::Shell {
                        command: "exit 7".to_string(),
                        workdir: Some(workspace.path().display().to_string()),
                        timeout_ms: 1_000,
                    },
                },
                CancellationToken::new(),
            )
            .await
            .expect("shell failure should remain a typed outcome");
        assert!(matches!(
            shell_failure,
            NativeToolOutcome::Failure { message }
                if message.as_bytes().len() <= NATIVE_CALL_BYTES
                    && message.contains("Exit code: 7")
        ));
        assert_terminal_and_not_cancellable(
            &tree_registry,
            &identity,
            "call-native-00000000-0000-4000-8000-000000000002-a1-3",
            NativeRunNodeStatus::Failed,
        );

        let patch_failure = worker
            .invoke_inner(
                NativeToolInvocation {
                    identity: identity.clone(),
                    runtime_call_id: "native-00000000-0000-4000-8000-000000000002-a1-4".to_string(),
                    request: NativeToolRequest::ApplyPatch {
                        patch: "not a patch".to_string(),
                    },
                },
                CancellationToken::new(),
            )
            .await
            .expect("apply_patch failure should remain a typed outcome");
        assert!(matches!(
            patch_failure,
            NativeToolOutcome::Failure { message }
                if message.as_bytes().len() <= NATIVE_CALL_BYTES
        ));

        worker
            .total_calls
            .store(NATIVE_TOTAL_CALLS, Ordering::Release);
        let over_call_limit = worker
            .invoke_inner(
                NativeToolInvocation {
                    identity: identity.clone(),
                    runtime_call_id: "native-00000000-0000-4000-8000-000000000002-a1-5".to_string(),
                    request: NativeToolRequest::ApplyPatch {
                        patch: "*** Begin Patch\n*** End Patch".to_string(),
                    },
                },
                CancellationToken::new(),
            )
            .await
            .expect("call-limit result should be typed");
        assert!(matches!(
            over_call_limit,
            NativeToolOutcome::Failure { message }
                if message == "native run exceeded 32 total calls"
        ));
        worker
            .artifact_bytes
            .store(NATIVE_TOTAL_ARTIFACT_BYTES, Ordering::Release);
        assert!(!worker.reserve_artifact_bytes(1));
        client
            .finalize(identity.clone())
            .await
            .expect("completed native run should finalize idempotently");

        let rejected_identity = NativeRunIdentity {
            run_id: "00000000-0000-4000-8000-000000000003".to_string(),
            ..identity
        };
        let rejected_worker = NativeCodeModeDispatchWorker::new(
            rejected_identity.clone(),
            1,
            Arc::clone(&session),
            StepContext::for_test(Arc::clone(&turn)).with_tool_router_for_test(Arc::new(
                ToolRouter::from_parts(
                    ToolRegistry::from_tools([
                        Arc::new(ShellCommandHandler::from(
                            ShellCommandBackendConfig::Classic,
                        ))
                            as Arc<dyn crate::tools::registry::CoreToolRuntime>,
                        Arc::new(ApplyPatchHandler::new(/*multi_environment*/ false))
                            as Arc<dyn crate::tools::registry::CoreToolRuntime>,
                    ]),
                    Vec::new(),
                ),
            )),
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            CancellationToken::new(),
            test_run_tree(&rejected_identity, CancellationToken::new()),
        );
        let rejected = client
            .execute(
                NativeExecute {
                    identity: rejected_identity.clone(),
                    attempt: 1,
                    task: "compile rejection remains repair pending".to_string(),
                    source: "use ycode_native_sdk as sdk; fn main( {".to_string(),
                },
                rejected_worker,
                CancellationToken::new(),
            )
            .await
            .expect("compile failure is a typed native response");
        assert!(
            matches!(
                &rejected,
                NativeExecution::Failed { failure, .. } if failure.kind == "Compile"
            ),
            "unexpected repair-boundary result: {rejected:?}"
        );
        client
            .finalize(rejected_identity)
            .await
            .expect("explicit finalize should abandon repair-pending run");
        drop(provider);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_real_shell_cancellation_reaps_process_and_clears_owners() {
        let Some(host_program) = std::env::var_os("YCODE_TEST_CODE_MODE_HOST") else {
            eprintln!("skipping native cancellation integration without YCODE_TEST_CODE_MODE_HOST");
            return;
        };
        let workspace = tempfile::tempdir().expect("temp workspace");
        let codex_home = tempfile::tempdir().expect("temp ycode home");
        let _home = EnvGuard::set("CODEX_HOME", codex_home.path().as_os_str());
        let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
            workspace.path().to_path_buf(),
        )
        .expect("absolute temp workspace");
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_in_cwd_for_tests(
                codex_home.path(),
                cwd,
            )
            .await;
        let history_before = session.clone_history().await.into_raw_items();
        let provider =
            ProcessOwnedCodeModeSessionProvider::with_host_program(PathBuf::from(host_program));
        let client = provider.native_client();
        let source = cancellation_fixture_source(workspace.path());
        let mut samples_ms = Vec::new();

        for sample in 0..=20 {
            let pid_path = workspace.path().join("native-cancel.pid");
            let _ = std::fs::remove_file(&pid_path);
            let identity = NativeRunIdentity {
                session_id: "native-cancel-session".to_string(),
                thread_id: "00000000-0000-4000-8000-000000000004".to_string(),
                run_id: format!("00000000-0000-4000-8000-{sample:012x}"),
            };
            let cancellation = CancellationToken::new();
            let worker = test_worker(
                identity.clone(),
                Arc::clone(&session),
                Arc::clone(&turn),
                cancellation.clone(),
            );
            let execute_client = client.clone();
            let execute_cancellation = cancellation.clone();
            let run_source = source.clone();
            let execute = tokio::spawn(async move {
                execute_client
                    .execute(
                        NativeExecute {
                            identity,
                            attempt: 1,
                            task: "start a cancellable real shell".to_string(),
                            source: run_source,
                        },
                        worker.clone(),
                        execute_cancellation,
                    )
                    .await
                    .map(|result| (result, worker))
            });
            let pid = wait_for_pid(&pid_path).await;
            assert!(
                process_exists(pid),
                "shell PID {pid} must exist before cancel"
            );
            let started = Instant::now();
            cancellation.cancel();
            let settled = tokio::time::timeout(Duration::from_secs(1), execute)
                .await
                .expect("native cancellation must settle within one second")
                .expect("native cancellation task must join");
            let elapsed = started.elapsed();
            let worker = match settled {
                Ok((_result, worker)) => worker,
                Err(_error) => {
                    // The connection returns cancellation as a request error; worker ownership is
                    // independently proven by the next run and exact descendant PID check.
                    assert!(!process_exists(pid));
                    if sample > 0 {
                        samples_ms.push(elapsed.as_secs_f64() * 1_000.0);
                    }
                    continue;
                }
            };
            assert!(
                !process_exists(pid),
                "shell PID {pid} survived cancellation"
            );
            assert_eq!(worker.owned_counts(), (0, 0));
            if sample > 0 {
                samples_ms.push(elapsed.as_secs_f64() * 1_000.0);
            }
        }

        assert_eq!(samples_ms.len(), 20);
        let mut ranked = samples_ms.clone();
        ranked.sort_by(f64::total_cmp);
        let p50 = ranked[9];
        let p95 = ranked[18];
        let max = ranked[19];
        eprintln!(
            "native real-shell cancellation ms raw={samples_ms:?} p50={p50:.3} p95={p95:.3} max={max:.3}"
        );
        assert!(p95 <= 250.0, "cancellation p95 {p95:.3} ms exceeded 250 ms");
        assert!(max < 1_000.0, "cancellation max {max:.3} ms exceeded 1 s");

        // Dropping the caller is a distinct ownership path: the client facade's drop guard must
        // cancel the outer request, which cascades through this real ToolCallRuntime shell call.
        let pid_path = workspace.path().join("native-cancel.pid");
        let _ = std::fs::remove_file(&pid_path);
        let dropped_identity = NativeRunIdentity {
            session_id: "native-cancel-session".to_string(),
            thread_id: "00000000-0000-4000-8000-000000000004".to_string(),
            run_id: "00000000-0000-4000-8000-0000000000ff".to_string(),
        };
        let dropped_cancellation = CancellationToken::new();
        let dropped_worker = test_worker(
            dropped_identity.clone(),
            Arc::clone(&session),
            Arc::clone(&turn),
            dropped_cancellation.clone(),
        );
        let dropped_client = client.clone();
        let dropped_source = source.clone();
        let task_worker = Arc::clone(&dropped_worker);
        let dropped_execute = tokio::spawn(async move {
            dropped_client
                .execute(
                    NativeExecute {
                        identity: dropped_identity.clone(),
                        attempt: 1,
                        task: "drop a caller with a live real shell".to_string(),
                        source: dropped_source,
                    },
                    task_worker,
                    dropped_cancellation,
                )
                .await
        });
        let dropped_pid = wait_for_pid(&pid_path).await;
        assert!(process_exists(dropped_pid));
        dropped_execute.abort();
        assert!(
            dropped_execute
                .await
                .expect_err("execute task must abort")
                .is_cancelled()
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !process_exists(dropped_pid) && dropped_worker.owned_counts() == (0, 0) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("dropped native caller must reap its real shell and release core ownership");

        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );
        assert!(session.list_background_terminals().await.is_empty());
        drop(provider);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_selected_shell_cancellation_is_typed_and_leaves_run_owned() {
        let Some(host_program) = std::env::var_os("YCODE_TEST_CODE_MODE_HOST") else {
            eprintln!("skipping selected native cancellation without YCODE_TEST_CODE_MODE_HOST");
            return;
        };
        let workspace = tempfile::tempdir().expect("temp workspace");
        let codex_home = tempfile::tempdir().expect("temp ycode home");
        let _home = EnvGuard::set("CODEX_HOME", codex_home.path().as_os_str());
        let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
            workspace.path().to_path_buf(),
        )
        .expect("cwd");
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_in_cwd_for_tests(
                codex_home.path(),
                cwd,
            )
            .await;
        let history_before = session.clone_history().await.into_raw_items();
        let provider =
            ProcessOwnedCodeModeSessionProvider::with_host_program(PathBuf::from(host_program));
        let client = provider.native_client();
        let source = selective_cancellation_fixture_source(workspace.path());
        let mut samples_ms = Vec::new();
        for sample in 0..20 {
            let pid_path = workspace.path().join("native-selected-cancel.pid");
            let _ = std::fs::remove_file(&pid_path);
            let identity = NativeRunIdentity {
                session_id: "native-selected-cancel-session".into(),
                thread_id: "00000000-0000-4000-8000-000000000014".into(),
                run_id: format!("00000000-0000-4000-9000-{sample:012x}"),
            };
            let cancellation = CancellationToken::new();
            let (worker, registry) = test_worker_and_registry(
                identity.clone(),
                Arc::clone(&session),
                Arc::clone(&turn),
                cancellation.clone(),
            );
            let execute_client = client.clone();
            let execute_identity = identity.clone();
            let run_source = source.clone();
            let execute = tokio::spawn(async move {
                execute_client
                    .execute(
                        NativeExecute {
                            identity: execute_identity,
                            attempt: 1,
                            task: "handle one selectively cancelled shell".into(),
                            source: run_source,
                        },
                        worker.clone(),
                        cancellation,
                    )
                    .await
                    .map(|result| (result, worker))
            });
            let pid = wait_for_pid(&pid_path).await;
            assert!(process_exists(pid));
            let observed = registry
                .subscribe(&identity.thread_id, &identity.run_id)
                .expect("active run tree");
            let snapshot = observed.borrow().clone().expect("active snapshot");
            let call_summary = snapshot
                .nodes
                .iter()
                .find(|node| node.kind == NativeRunNodeKind::ToolCall)
                .expect("live tool node")
                .summary
                .as_str();
            assert!(call_summary.starts_with("shell request ·"));
            assert!(!call_summary.contains("sleep"));
            assert!(!call_summary.contains(&workspace.path().display().to_string()));
            let started = Instant::now();
            assert_eq!(
                registry.cancel(
                    &identity.thread_id,
                    &identity.run_id,
                    &format!("call-native-{}-a1-1", identity.run_id),
                ),
                Ok(crate::native_run_tree::NativeRunCancelResult::Requested)
            );
            let (execution, worker) = tokio::time::timeout(Duration::from_secs(1), execute)
                .await
                .expect("selected cancellation settles within one second")
                .expect("join")
                .expect("native response");
            let elapsed = started.elapsed();
            assert!(!process_exists(pid));
            assert_eq!(worker.owned_counts(), (0, 0));
            assert!(
                matches!(execution, NativeExecution::Completed { evidence, .. } if evidence.summary == "selected cancellation handled")
            );
            samples_ms.push(elapsed.as_secs_f64() * 1_000.0);
        }
        let mut ranked = samples_ms.clone();
        ranked.sort_by(f64::total_cmp);
        let p50 = ranked[9];
        let p95 = ranked[18];
        let max = ranked[19];
        eprintln!(
            "native selected-shell cancellation ms raw={samples_ms:?} p50={p50:.3} p95={p95:.3} max={max:.3}"
        );
        assert!(p95 <= 250.0);
        assert!(max < 1_000.0);
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_host_disconnect_cancels_real_shell_and_clears_owners() {
        let Some(host_program) = std::env::var_os("YCODE_TEST_CODE_MODE_HOST") else {
            eprintln!("skipping native disconnect integration without YCODE_TEST_CODE_MODE_HOST");
            return;
        };
        let workspace = tempfile::tempdir().expect("temp workspace");
        let codex_home = tempfile::tempdir().expect("temp ycode home");
        let _home = EnvGuard::set("CODEX_HOME", codex_home.path().as_os_str());
        let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
            workspace.path().to_path_buf(),
        )
        .expect("absolute temp workspace");
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_in_cwd_for_tests(
                codex_home.path(),
                cwd,
            )
            .await;
        let history_before = session.clone_history().await.into_raw_items();
        let existing_children = direct_child_pids();
        let provider =
            ProcessOwnedCodeModeSessionProvider::with_host_program(PathBuf::from(host_program));
        let client = provider.native_client();
        let identity = NativeRunIdentity {
            session_id: "native-disconnect-session".to_string(),
            thread_id: "00000000-0000-4000-8000-000000000005".to_string(),
            run_id: "00000000-0000-4000-8000-000000000006".to_string(),
        };
        let cancellation = CancellationToken::new();
        let worker = test_worker(
            identity.clone(),
            Arc::clone(&session),
            Arc::clone(&turn),
            cancellation.clone(),
        );
        let task_worker = Arc::clone(&worker);
        let source = cancellation_fixture_source(workspace.path());
        let execute_client = client.clone();
        let execute = tokio::spawn(async move {
            execute_client
                .execute(
                    NativeExecute {
                        identity,
                        attempt: 1,
                        task: "disconnect the adjacent host during a real shell".to_string(),
                        source,
                    },
                    task_worker,
                    cancellation,
                )
                .await
        });
        let shell_pid = wait_for_pid(&workspace.path().join("native-cancel.pid")).await;
        let host_pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let new_children = direct_child_pids()
                    .difference(&existing_children)
                    .copied()
                    .filter(|pid| process_command(*pid).contains("codex-code-mode-host"))
                    .collect::<Vec<_>>();
                if let [host_pid] = new_children.as_slice() {
                    break *host_pid;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the process-owned client must have exactly one adjacent host child");
        assert!(process_exists(host_pid));
        assert!(process_exists(shell_pid));

        let started = Instant::now();
        let status = std::process::Command::new("/bin/kill")
            .args(["-TERM", &host_pid.to_string()])
            .status()
            .expect("terminate exact test-owned adjacent host");
        assert!(status.success());
        let result = tokio::time::timeout(Duration::from_secs(1), execute)
            .await
            .expect("host disconnect must settle within one second")
            .expect("native execute task must join");
        assert!(
            result.is_err(),
            "host disconnect must fail the native request"
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !process_exists(host_pid)
                    && !process_exists(shell_pid)
                    && worker.owned_counts() == (0, 0)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("host disconnect must reap exact children and release native/core ownership");
        let elapsed = started.elapsed();
        eprintln!(
            "native real-shell host-disconnect settled in {:.3} ms",
            elapsed.as_secs_f64() * 1_000.0
        );
        assert!(elapsed < Duration::from_secs(1));
        assert!(session.list_background_terminals().await.is_empty());
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );
        drop(provider);
    }

    #[test]
    #[serial_test::serial]
    fn native_host_disconnect_settles_an_admitted_agent_owner() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_stack_size(16 * 1024 * 1024)
            .enable_all()
            .build()
            .expect("production-sized Tokio runtime")
            .block_on(native_host_disconnect_settles_an_admitted_agent_owner_inner());
    }

    async fn native_host_disconnect_settles_an_admitted_agent_owner_inner() {
        let Some(host_program) = std::env::var_os("YCODE_TEST_CODE_MODE_HOST") else {
            eprintln!("skipping native agent disconnect without YCODE_TEST_CODE_MODE_HOST");
            return;
        };
        let server = pending_native_agent_server().await;
        let (codex_home, session, turn, manager, agent_control, root_id) =
            native_agent_test_session(&server, NATIVE_WORKER_AGENTS).await;
        let _home = EnvGuard::set("CODEX_HOME", codex_home.path().as_os_str());
        let history_before = session.clone_history().await.into_raw_items();
        let existing_children = direct_child_pids();
        let provider =
            ProcessOwnedCodeModeSessionProvider::with_host_program(PathBuf::from(host_program));
        let client = provider.native_client();
        let identity = native_agent_identity(0x47, 0x48);
        let cancellation = CancellationToken::new();
        let (worker, registry) = test_worker_and_registry(
            identity.clone(),
            Arc::clone(&session),
            Arc::clone(&turn),
            cancellation.clone(),
        );
        let task_worker = Arc::clone(&worker);
        let execute_client = client.clone();
        let execute_identity = identity.clone();
        let mut execute = tokio::spawn(async move {
            execute_client
                .execute(
                    NativeExecute {
                        identity: execute_identity,
                        attempt: 1,
                        task: "disconnect with an admitted native agent".to_string(),
                        source: pending_agent_fixture_source(),
                    },
                    task_worker,
                    cancellation,
                )
                .await
        });
        let runtime_call_id = format!("native-{}-a1-1", identity.run_id);
        tokio::select! {
            _ = wait_for_agent_node(&registry, &identity, &runtime_call_id) => {}
            result = &mut execute => {
                panic!("native execute ended before AgentControl admission: {result:?}");
            }
        }
        let worker_thread = manager
            .list_thread_ids()
            .await
            .into_iter()
            .find(|thread_id| *thread_id != root_id)
            .expect("native worker admitted before disconnect");
        agent_control.delay_next_native_graceful_shutdown_for_test(Duration::from_millis(600));
        agent_control.delay_next_native_force_shutdown_for_test(Duration::from_millis(800));
        let host_pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let new_children = direct_child_pids()
                    .difference(&existing_children)
                    .copied()
                    .filter(|pid| process_command(*pid).contains("codex-code-mode-host"))
                    .collect::<Vec<_>>();
                if let [host_pid] = new_children.as_slice() {
                    return *host_pid;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("one process-owned adjacent host");
        let started = Instant::now();
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-TERM", &host_pid.to_string()])
                .status()
                .expect("terminate exact test-owned host")
                .success()
        );
        let result = tokio::time::timeout(Duration::from_secs(1), execute)
            .await
            .expect("host disconnect settles within one second")
            .expect("execute task joins");
        assert!(result.is_err());
        wait_for_native_agent_zero(&worker).await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(worker.emergency_agent_settlement_count(), 1);
        assert_eq!(worker.owned_agent_task_count(), 0);
        assert!(!manager.list_thread_ids().await.contains(&worker_thread));
        assert_eq!(manager.list_thread_ids().await, vec![root_id]);
        assert_eq!(agent_control.spawned_agent_count_for_test(), 0);
        assert_eq!(agent_control.native_residency_counts_for_test(), (0, 0));
        assert!(session.list_background_terminals().await.is_empty());
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );
        assert_terminal_and_not_cancellable(
            &registry,
            &identity,
            &format!("agent-{runtime_call_id}"),
            NativeRunNodeStatus::Cancelled,
        );

        let reuse_cancellation = CancellationToken::new();
        let reuse_worker = Arc::clone(&worker);
        let reuse_token = reuse_cancellation.clone();
        let reuse_invocation = native_agent_invocation(&identity, 2, "reuse after disconnect");
        let reuse = tokio::spawn(async move {
            reuse_worker
                .invoke_inner(reuse_invocation, reuse_token)
                .await
        });
        wait_for_agent_node(
            &registry,
            &identity,
            &format!("native-{}-a1-2", identity.run_id),
        )
        .await;
        reuse_cancellation.cancel();
        let _ = reuse.await.expect("post-disconnect reuse joins");
        wait_for_native_agent_zero(&worker).await;
        assert_eq!(manager.list_thread_ids().await, vec![root_id]);
        assert_eq!(agent_control.spawned_agent_count_for_test(), 0);
        assert_eq!(agent_control.native_residency_counts_for_test(), (0, 0));
        drop(provider);
    }

    fn test_worker(
        identity: NativeRunIdentity,
        session: Arc<crate::session::session::Session>,
        turn: Arc<crate::session::turn_context::TurnContext>,
        cancellation: CancellationToken,
    ) -> Arc<NativeCodeModeDispatchWorker> {
        test_worker_and_registry(identity, session, turn, cancellation).0
    }

    async fn mount_native_agent_response(
        server: &wiremock::MockServer,
        task: &str,
        result: &str,
        response_id: &str,
        tokens: i64,
    ) -> core_test_support::responses::ResponseMock {
        mount_sse_once_match(
            server,
            body_string_contains(task),
            sse(vec![
                ev_response_created(response_id),
                ev_assistant_message(&format!("{response_id}-message"), result),
                ev_completed_with_tokens(response_id, tokens),
            ]),
        )
        .await
    }

    async fn pending_native_agent_server() -> wiremock::MockServer {
        let server = core_test_support::responses::start_mock_server().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/responses$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse(vec![
                        ev_response_created("pending-native-owner"),
                        ev_assistant_message("pending-native-owner-message", "too late"),
                        ev_completed_with_tokens("pending-native-owner", 5),
                    ]))
                    .set_delay(Duration::from_secs(5)),
            )
            .mount(&server)
            .await;
        server
    }

    async fn native_agent_test_session(
        server: &wiremock::MockServer,
        max_threads: usize,
    ) -> (
        tempfile::TempDir,
        Arc<crate::session::session::Session>,
        Arc<crate::session::turn_context::TurnContext>,
        ThreadManager,
        crate::agent::AgentControl,
        codex_protocol::ThreadId,
    ) {
        let codex_home = tempfile::tempdir().expect("temp ycode home");
        let provider = {
            let mut provider = built_in_model_providers()["openai"].clone();
            provider.base_url = Some(server.uri());
            provider
        };
        let provider_for_config = provider.clone();
        let (mut session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                CodexAuth::from_api_key("native-agent-ownership-loopback"),
                Vec::new(),
                codex_home.path(),
                move |config| {
                    config.model_provider = provider_for_config;
                    config.agent_max_threads = Some(max_threads);
                },
            )
            .await;
        let state_db = crate::init_state_db(turn.config.as_ref()).await;
        let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
            CodexAuth::from_api_key("native-agent-ownership-loopback"),
            provider,
            codex_home.path().to_path_buf(),
            Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
            state_db,
        );
        let root = manager
            .start_thread(StartThreadOptions::new((*turn.config).clone()))
            .await
            .expect("native root thread starts");
        let agent_control = manager.agent_control();
        let session_mut = Arc::get_mut(&mut session).expect("test session uniquely owned");
        session_mut.services.agent_control = agent_control.clone();
        session_mut.thread_id = root.thread_id;
        (
            codex_home,
            session,
            turn,
            manager,
            agent_control,
            root.thread_id,
        )
    }

    fn native_agent_identity(thread_suffix: u8, run_suffix: u8) -> NativeRunIdentity {
        NativeRunIdentity {
            session_id: "native-agent-ownership".to_string(),
            thread_id: format!("00000000-0000-4000-8000-0000000000{thread_suffix:02x}"),
            run_id: format!("00000000-0000-4000-8000-0000000000{run_suffix:02x}"),
        }
    }

    fn pending_agent_fixture_source() -> String {
        r#"#![forbid(unsafe_code)]
use ycode_native_sdk::{run, Evidence, Outcome, Request};
fn main() {
    run(|context| {
        let outcome = context.call(Request::Agent {
            task: "pending host-disconnect agent".into(),
            model: None,
            reasoning_effort: None,
        })?;
        let call_id = match outcome {
            Outcome::Success { call_id, .. }
            | Outcome::Retry { call_id, .. }
            | Outcome::Failure { call_id, .. } => call_id,
        };
        context.finish(Evidence {
            version: 1,
            summary: "agent settled".into(),
            verified: vec![], disputed: vec![], unresolved: vec![], artifact_refs: vec![],
            partial_failures: vec![], provenance_ids: vec![call_id],
        })
    }).unwrap();
}
"#
        .to_string()
    }

    async fn wait_for_agent_node(
        registry: &crate::native_run_tree::NativeRunTreeRegistry,
        identity: &NativeRunIdentity,
        runtime_call_id: &str,
    ) {
        let mut observation = registry
            .subscribe(&identity.thread_id, &identity.run_id)
            .expect("active native tree");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if observation.borrow().as_ref().is_some_and(|snapshot| {
                    snapshot.nodes.iter().any(|node| {
                        node.stable_id == format!("agent-{runtime_call_id}")
                            && node.status == NativeRunNodeStatus::Running
                    })
                }) {
                    return;
                }
                observation.changed().await.expect("tree remains active");
            }
        })
        .await
        .expect("native AgentControl admission becomes visible");
    }

    async fn wait_for_native_agent_zero(worker: &NativeCodeModeDispatchWorker) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while worker.owned_agent_counts() != (0, 0)
                || worker.owned_counts() != (0, 0)
                || worker.owned_agent_task_count() != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("native ownership reaches zero");
    }

    fn native_agent_invocation(
        identity: &NativeRunIdentity,
        ordinal: usize,
        task: &str,
    ) -> NativeToolInvocation {
        NativeToolInvocation {
            identity: identity.clone(),
            runtime_call_id: format!("native-{}-a1-{ordinal}", identity.run_id),
            request: NativeToolRequest::Agent {
                task: task.to_string(),
                model: None,
                reasoning_effort: None,
            },
        }
    }

    fn assert_agent_success(outcome: Result<NativeToolOutcome, String>, expected: &str) {
        assert_eq!(
            outcome.expect("native agent invocation should settle"),
            NativeToolOutcome::Success {
                output: expected.as_bytes().to_vec(),
            }
        );
    }

    fn assert_terminal_and_not_cancellable(
        registry: &crate::native_run_tree::NativeRunTreeRegistry,
        identity: &NativeRunIdentity,
        stable_id: &str,
        expected_status: NativeRunNodeStatus,
    ) {
        let snapshot = registry
            .subscribe(&identity.thread_id, &identity.run_id)
            .expect("active tree")
            .borrow()
            .clone()
            .expect("active snapshot");
        let node = snapshot
            .nodes
            .iter()
            .find(|node| node.stable_id == stable_id)
            .expect("call node");
        assert_eq!(node.status, expected_status);
        assert!(node.finished_at.is_some());
        assert_eq!(
            registry
                .cancel(&identity.thread_id, &identity.run_id, stable_id)
                .expect("settled call cancellation is bounded"),
            crate::native_run_tree::NativeRunCancelResult::NotCancellable
        );
    }

    fn test_worker_and_registry(
        identity: NativeRunIdentity,
        session: Arc<crate::session::session::Session>,
        turn: Arc<crate::session::turn_context::TurnContext>,
        cancellation: CancellationToken,
    ) -> (
        Arc<NativeCodeModeDispatchWorker>,
        Arc<crate::native_run_tree::NativeRunTreeRegistry>,
    ) {
        let registry = ToolRegistry::from_tools([
            Arc::new(ShellCommandHandler::from(
                ShellCommandBackendConfig::Classic,
            )) as Arc<dyn crate::tools::registry::CoreToolRuntime>,
            Arc::new(ApplyPatchHandler::new(/*multi_environment*/ false))
                as Arc<dyn crate::tools::registry::CoreToolRuntime>,
        ]);
        let step = StepContext::for_test(turn)
            .with_tool_router_for_test(Arc::new(ToolRouter::from_parts(registry, Vec::new())));
        let tree_registry = Arc::new(crate::native_run_tree::NativeRunTreeRegistry::default());
        let run_tree = tree_registry
            .begin(identity.clone(), "test", cancellation.clone())
            .expect("tree");
        run_tree.start(
            "workflow-a1",
            "run",
            crate::native_run_tree::NativeRunNodeKind::Workflow {
                attempt: 1,
                pid: Some(1),
            },
            "test workflow",
            Some((
                crate::native_run_tree::NativeRunCancelScope::Run,
                cancellation.clone(),
            )),
        );
        let worker = NativeCodeModeDispatchWorker::new(
            identity,
            1,
            session,
            step,
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            cancellation,
            run_tree,
        );
        (worker, tree_registry)
    }

    fn test_run_tree(
        identity: &NativeRunIdentity,
        cancellation: CancellationToken,
    ) -> crate::native_run_tree::NativeRunTreeOwner {
        let registry = Arc::new(crate::native_run_tree::NativeRunTreeRegistry::default());
        let owner = registry
            .begin(identity.clone(), "test", cancellation.clone())
            .expect("tree");
        owner.start(
            "workflow-a1",
            "run",
            crate::native_run_tree::NativeRunNodeKind::Workflow {
                attempt: 1,
                pid: Some(1),
            },
            "test workflow",
            Some((
                crate::native_run_tree::NativeRunCancelScope::Run,
                cancellation,
            )),
        );
        owner
    }

    async fn wait_for_pid(path: &std::path::Path) -> u32 {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(path)
                    && let Ok(pid) = text.trim().parse::<u32>()
                {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("real shell must publish its PID")
    }

    fn process_exists(pid: u32) -> bool {
        std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn direct_child_pids() -> HashSet<u32> {
        let output = std::process::Command::new("/usr/bin/pgrep")
            .args(["-P", &std::process::id().to_string()])
            .output()
            .expect("list direct test-process children");
        if !output.status.success() {
            return HashSet::new();
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect()
    }

    fn process_command(pid: u32) -> String {
        let output = std::process::Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .expect("inspect exact direct child command");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn cancellation_fixture_source(workspace: &std::path::Path) -> String {
        let workdir = workspace.to_string_lossy();
        format!(
            r#"use ycode_native_sdk as sdk;
use sdk::{{Context, Evidence, Outcome, Request, Result}};

fn main() {{
    if let Err(error) = sdk::run(workflow) {{
        eprintln!("{{error}}");
        std::process::exit(1);
    }}
}}

fn workflow(context: &mut Context) -> Result<()> {{
    let outcome = context.call(Request::Shell {{
        command: "echo $$ > native-cancel.pid; exec sleep 30".to_string(),
        workdir: Some({workdir:?}.to_string()),
        timeout_ms: 30_000,
    }})?;
    let call_id = match outcome {{
        Outcome::Success {{ call_id, .. }} => call_id,
        Outcome::Retry {{ reason, .. }} => return Err(sdk::Error::Host(reason)),
        Outcome::Failure {{ message, .. }} => return Err(sdk::Error::Host(message)),
    }};
    context.finish(Evidence {{
        version: 1,
        summary: "unexpected uncancelled completion".to_string(),
        verified: vec![], disputed: vec![], unresolved: vec![],
        artifact_refs: vec![], partial_failures: vec![],
        provenance_ids: vec![call_id],
    }})
}}
"#
        )
    }

    fn selective_cancellation_fixture_source(workspace: &std::path::Path) -> String {
        let workdir = workspace.to_string_lossy();
        format!(
            r#"use ycode_native_sdk as sdk;
use sdk::{{Context, Evidence, Outcome, Request, Result}};
fn main() {{ if let Err(error) = sdk::run(workflow) {{ eprintln!("{{error}}"); std::process::exit(1); }} }}
fn workflow(context: &mut Context) -> Result<()> {{
    let outcome = context.call(Request::Shell {{
        command: "echo $$ > native-selected-cancel.pid; exec sleep 30".to_string(),
        workdir: Some({workdir:?}.to_string()), timeout_ms: 30_000,
    }})?;
    let call_id = match outcome {{
        Outcome::Failure {{ call_id, message }} if message == "native tool call cancelled by user" => call_id,
        other => return Err(sdk::Error::Host(format!("unexpected outcome: {{other:?}}"))),
    }};
    context.finish(Evidence {{ version: 1, summary: "selected cancellation handled".to_string(),
        verified: vec!["cancelled call returned a typed outcome".to_string()], disputed: vec![], unresolved: vec![],
        artifact_refs: vec![], partial_failures: vec![], provenance_ids: vec![call_id] }})
}}
"#
        )
    }

    fn fixture_source(workspace: &std::path::Path) -> String {
        let workdir = workspace.to_string_lossy();
        format!(
            r#"use ycode_native_sdk as sdk;
use sdk::{{Context, Evidence, Outcome, Request, Result}};

fn main() {{
    if let Err(error) = sdk::run(workflow) {{
        eprintln!("{{error}}");
        std::process::exit(1);
    }}
}}

fn workflow(context: &mut Context) -> Result<()> {{
    let shell = context.spawn(Request::Shell {{
        command: "sleep 0.2; printf checked".to_string(),
        workdir: Some({workdir:?}.to_string()),
        timeout_ms: 5_000,
    }})?;
    let patch = context.spawn(Request::ApplyPatch {{
        patch: "*** Begin Patch\n*** Update File: native-proof.txt\n@@\n seed\n+patched\n*** End Patch".to_string(),
    }})?;
    let shell_id = match context.join(shell)? {{
        Outcome::Success {{ call_id, .. }} => call_id,
        Outcome::Retry {{ reason, .. }} => return Err(sdk::Error::Host(reason)),
        Outcome::Failure {{ message, .. }} => return Err(sdk::Error::Host(message)),
    }};
    let patch_id = match context.join(patch)? {{
        Outcome::Success {{ call_id, .. }} => call_id,
        Outcome::Retry {{ reason, .. }} => return Err(sdk::Error::Host(reason)),
        Outcome::Failure {{ message, .. }} => return Err(sdk::Error::Host(message)),
    }};
    context.finish(Evidence {{
        version: 1,
        summary: "real shell and apply_patch completed".to_string(),
        verified: vec!["native-proof.txt contains seed and patched".to_string()],
        disputed: vec![],
        unresolved: vec![],
        artifact_refs: vec![],
        partial_failures: vec![],
        provenance_ids: vec![shell_id, patch_id],
    }})
}}
"#
        )
    }

    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
            let previous = std::env::var_os(key);
            // SAFETY: this test is serialized and restores the process environment on drop.
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: this test is serialized and no spawned host survives the test owner.
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }
}
