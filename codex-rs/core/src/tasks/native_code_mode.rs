use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode::NativeCodeModeDelegate;
use codex_code_mode::NativeExecute;
use codex_code_mode::NativeExecution;
use codex_code_mode::NativeProgress;
use codex_code_mode::NativeRunIdentity;
use codex_code_mode::NativeToolFuture;
use codex_code_mode::NativeToolInvocation;
use codex_code_mode::ProcessOwnedNativeCodeModeClient;
use codex_code_mode::host::NativeEvidence;
use codex_protocol::items::NativeCodeModeItem;
use codex_protocol::items::NativeCodeModePhase;
use codex_protocol::items::TurnItem;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout_trace::InferenceTraceContext;
use futures::StreamExt;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::SessionTask;
use super::SessionTaskResult;
use crate::client_common::Prompt;
use crate::native_run_tree::NativeRunCancelScope;
use crate::native_run_tree::NativeRunNodeKind;
use crate::native_run_tree::NativeRunNodeStatus;
use crate::native_run_tree::NativeRunTreeOwner;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::tools::context::SharedTurnDiffTracker;
use crate::turn_diff_tracker::TurnDiffTracker;

const SOURCE_BYTES: usize = 64 * 1024;
const SOURCE_LINES: usize = 1_500;
const TASK_BYTES: usize = 16 * 1024;
const DIAGNOSTIC_BYTES: usize = 64 * 1024;
const FINAL_EVIDENCE_BYTES: usize = 16 * 1024;
const MODEL_OUTPUT_BYTES: usize = 256 * 1024;
const EVIDENCE_ITEMS: usize = 64;
const EVIDENCE_STRING_BYTES: usize = 4 * 1024;
const NATIVE_PROTOCOL_VERSION: u16 = 1;
const INITIAL_RESPONSES_TIMEOUT: Duration = Duration::from_secs(90);
const REPAIR_RESPONSES_TIMEOUT: Duration = Duration::from_secs(60);

const SDK_CONTRACT: &str = r#"SDK v1 (`ycode_native_sdk`, std only):
- `run(|context: &mut Context| -> Result<()>) -> Result<()>`
- Request::Shell { command: String, workdir: Option<String>, timeout_ms: u32 }
- Request::ApplyPatch { patch: String }
- Outcome::Success { call_id: String, output: Vec<u8> }
- Outcome::Retry { call_id: String, reason: String }
- Outcome::Failure { call_id: String, message: String }
- Evidence { version: 1, summary, verified, disputed, unresolved, artifact_refs, partial_failures, provenance_ids }
- Every provenance ID must be the exact `call_id` from a completed, joined Outcome used by the Evidence. Collect those IDs from Outcome variants; never invent labels.
- Set `artifact_refs` empty. The host derives retained request/result artifact refs only from verified provenance IDs.
- Context operations: call(Request) -> Result<Outcome>, spawn(Request) -> Result<Task>, join(Task) -> Result<Outcome>, budget() -> Result<u32>, cancelled() -> Result<bool>, finish(Evidence) -> Result<()>
- Public concepts: Request, Task, Outcome, Evidence, Error, Result, Context"#;

#[derive(Clone)]
pub(crate) struct NativeCodeModeTask {
    task: String,
}

impl NativeCodeModeTask {
    pub(crate) fn new(task: String) -> Self {
        Self { task }
    }
}

trait NativeLifecycle: Send + Sync {
    fn transition<'a>(&'a self, phase: NativeCodeModePhase) -> BoxFuture<'a, ()>;
}

struct SessionNativeLifecycle {
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    identity: NativeRunIdentity,
    run_tree: NativeRunTreeOwner,
    compile_attempt: AtomicU8,
    cancellation: CancellationToken,
}

impl NativeLifecycle for SessionNativeLifecycle {
    fn transition<'a>(&'a self, phase: NativeCodeModePhase) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            match phase {
                NativeCodeModePhase::Generating => self.run_tree.start(
                    "generation",
                    "run",
                    NativeRunNodeKind::Generation,
                    "source generation",
                    None,
                ),
                NativeCodeModePhase::Compiling => {
                    let attempt = self.compile_attempt.fetch_add(1, Ordering::AcqRel) + 1;
                    if attempt == 1 {
                        self.run_tree.settle(
                            "generation",
                            NativeRunNodeStatus::Succeeded,
                            "source admitted",
                        );
                    }
                    self.run_tree.start(
                        format!("compile-{attempt}"),
                        "run",
                        NativeRunNodeKind::Compile { attempt, pid: None },
                        &format!("compile attempt {attempt}"),
                        Some((NativeRunCancelScope::Run, self.cancellation.clone())),
                    );
                }
                NativeCodeModePhase::Repairing => self.run_tree.start(
                    "repair",
                    "run",
                    NativeRunNodeKind::Repair,
                    "compiler repair",
                    None,
                ),
                NativeCodeModePhase::Repair => self.run_tree.settle(
                    "repair",
                    NativeRunNodeStatus::Succeeded,
                    "repaired source admitted",
                ),
                _ => {}
            }
            emit_native_lifecycle(
                self.session.as_ref(),
                self.turn.as_ref(),
                &self.identity,
                phase,
                String::new(),
            )
            .await;
        })
    }
}

impl SessionTask for NativeCodeModeTask {
    fn kind(&self) -> TaskKind {
        TaskKind::NativeCodeMode
    }

    fn span_name(&self) -> &'static str {
        "session_task.native_code_mode"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation: CancellationToken,
    ) -> SessionTaskResult {
        debug_assert!(
            input.is_empty(),
            "native Code Mode has no ordinary turn input"
        );
        session
            .send_event(
                ctx.as_ref(),
                EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: ctx.sub_id.clone(),
                    trace_id: ctx.trace_id.clone(),
                    started_at: ctx.turn_timing_state.started_at_unix_secs().await,
                    model_context_window: ctx.model_context_window(),
                }),
            )
            .await;

        let history_before = session.clone_history().await.into_raw_items();
        let identity = NativeRunIdentity {
            session_id: session.thread_id.to_string(),
            thread_id: session.thread_id.to_string(),
            run_id: uuid::Uuid::new_v4().to_string(),
        };
        let run_tree = session
            .services
            .code_mode_service
            .native_run_trees()
            .begin(identity.clone(), &self.task, cancellation.clone())
            .map_err(codex_protocol::error::CodexErr::InvalidRequest)?;
        emit_native_lifecycle(
            session.as_ref(),
            ctx.as_ref(),
            &identity,
            NativeCodeModePhase::Invocation,
            self.task.clone(),
        )
        .await;

        let lifecycle = SessionNativeLifecycle {
            session: Arc::clone(&session),
            turn: Arc::clone(&ctx),
            identity: identity.clone(),
            run_tree: run_tree.clone(),
            compile_attempt: AtomicU8::new(0),
            cancellation: cancellation.clone(),
        };
        lifecycle.transition(NativeCodeModePhase::Generating).await;
        let terminal = match prepare_live_backends(
            Arc::clone(&session),
            Arc::clone(&ctx),
            identity.clone(),
            cancellation.clone(),
            run_tree.clone(),
        )
        .await
        {
            Ok((generator, executor)) => {
                orchestrate_inner(
                    &generator,
                    &executor,
                    &self.task,
                    &identity,
                    cancellation,
                    Some(&lifecycle),
                )
                .await
            }
            Err(message) => terminal_failure(
                &identity,
                "Native Rust Code Mode could not start",
                &message,
                None,
                /*interrupted*/ false,
            ),
        };

        if let Some(reference) = terminal.evidence.artifact_refs.first().cloned() {
            run_tree.add_ref("run", &reference);
            emit_native_lifecycle(
                session.as_ref(),
                ctx.as_ref(),
                &identity,
                NativeCodeModePhase::Artifact,
                reference,
            )
            .await;
        }

        // No generation, repair, source, diagnostic, or native tool payload is recorded through
        // the normal turn/history builders. This equality is checked immediately before the one
        // terminal item crosses the canonical history boundary.
        if session.clone_history().await.into_raw_items() != history_before {
            run_tree.settle_unfinished(NativeRunNodeStatus::Failed);
            run_tree.finish(NativeRunNodeStatus::Failed);
            return Err(codex_protocol::error::CodexErr::Fatal(
                "native Code Mode history changed before terminal Evidence".to_string(),
            ));
        }

        let (evidence_text, item, outcome) = terminal_evidence_item(terminal, &identity);
        let terminal_status = match &outcome {
            NativeCodeModePhase::Succeeded => NativeRunNodeStatus::Succeeded,
            NativeCodeModePhase::Interrupted => NativeRunNodeStatus::Cancelled,
            _ => NativeRunNodeStatus::Failed,
        };
        run_tree.settle_unfinished(terminal_status);
        run_tree.start(
            "finalization",
            "run",
            NativeRunNodeKind::Finalization,
            "terminal evidence",
            None,
        );
        emit_native_lifecycle(
            session.as_ref(),
            ctx.as_ref(),
            &identity,
            outcome,
            String::new(),
        )
        .await;
        session
            .record_response_item_and_emit_turn_item(ctx.as_ref(), item)
            .await;
        run_tree.settle("finalization", NativeRunNodeStatus::Succeeded, "settled");
        run_tree.finish(terminal_status);
        Ok(Some(evidence_text))
    }
}

async fn emit_native_lifecycle(
    session: &Session,
    ctx: &TurnContext,
    identity: &NativeRunIdentity,
    phase: NativeCodeModePhase,
    text: String,
) {
    let item = TurnItem::NativeCodeMode(NativeCodeModeItem {
        id: uuid::Uuid::now_v7().to_string(),
        run_id: identity.run_id.clone(),
        phase,
        text,
    });
    session.emit_turn_item_started(ctx, &item).await;
    session.emit_turn_item_completed(ctx, item).await;
}

impl Session {
    /// Reserves an idle turn and starts one native task without replacing existing work.
    pub(crate) async fn start_native_code_mode_task(
        self: &Arc<Self>,
        task: String,
    ) -> codex_protocol::error::Result<String> {
        if task.trim().is_empty() {
            return Err(codex_protocol::error::CodexErr::InvalidRequest(
                "native Code Mode requires a task".to_string(),
            ));
        }
        if task.len() > TASK_BYTES {
            return Err(codex_protocol::error::CodexErr::InvalidRequest(format!(
                "native Code Mode task exceeds {TASK_BYTES} bytes"
            )));
        }
        {
            let mut active_turn = self.active_turn.lock().await;
            if active_turn.is_some() {
                return Err(codex_protocol::error::CodexErr::InvalidRequest(
                    "native Code Mode is available only while the thread is idle".to_string(),
                ));
            }
            *active_turn = Some(super::ActiveTurn::default());
        }
        let turn = self.new_default_turn().await;
        let turn_id = turn.sub_id.clone();
        self.start_task(
            turn,
            Vec::new(),
            NativeCodeModeTask::new(task),
            super::MailboxParentProvenance::Ignore,
        )
        .await;
        Ok(turn_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GenerationKind {
    Initial,
    Repair,
}

struct GenerationRequest<'a> {
    kind: GenerationKind,
    task: &'a str,
    original_source: Option<&'a str>,
    source_hash: Option<&'a str>,
    diagnostic: Option<&'a str>,
}

trait SourceGenerator: Send + Sync {
    fn generate<'a>(
        &'a self,
        request: GenerationRequest<'a>,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<String, String>>;
}

trait NativeExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        request: NativeExecute,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<NativeExecution, String>>;

    fn finalize<'a>(&'a self, identity: NativeRunIdentity) -> BoxFuture<'a, Result<(), String>>;
}

struct ResponsesSourceGenerator {
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    applicable_instructions: String,
}

impl SourceGenerator for ResponsesSourceGenerator {
    fn generate<'a>(
        &'a self,
        request: GenerationRequest<'a>,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move { self.generate_inner(request, cancellation).await })
    }
}

impl ResponsesSourceGenerator {
    async fn generate_inner(
        &self,
        request: GenerationRequest<'_>,
        cancellation: CancellationToken,
    ) -> Result<String, String> {
        let cwd = self
            .turn
            .environments
            .primary()
            .ok_or_else(|| "native task has no ready primary environment".to_string())?
            .cwd()
            .inferred_native_path_string();
        let prompt = generation_prompt(&request, &cwd, &self.applicable_instructions)?;
        let request_kind = match request.kind {
            GenerationKind::Initial => CodexResponsesRequestKind::NativeCodeModeGeneration,
            GenerationKind::Repair => CodexResponsesRequestKind::NativeCodeModeRepair,
        };
        let metadata = self.turn.turn_metadata_state.to_responses_metadata(
            self.session.installation_id.clone(),
            self.session.current_window_id().await,
            request_kind,
        );
        // A fresh request-scoped session prevents either native request from inheriting normal
        // turn history or prior native response state.
        let mut client = self.session.services.model_client.new_session();
        let inference_trace = InferenceTraceContext::disabled();
        let timeout = responses_timeout(request.kind);
        let label = match request.kind {
            GenerationKind::Initial => "native source generation",
            GenerationKind::Repair => "native compiler repair",
        };
        let mut token_usage = None;
        let response_cancellation = cancellation.child_token();
        let response = async {
            let stream = client
                .stream(
                    &prompt,
                    &self.turn.model_info,
                    self.turn.reasoning_effort.clone(),
                    self.turn.reasoning_summary,
                    self.turn.config.service_tier.clone(),
                    &metadata,
                    &inference_trace,
                )
                .await
                .map_err(|error| {
                    bounded_error(format!("native Responses request failed: {error}"))
                })?;
            collect_source(stream, response_cancellation, &mut token_usage).await
        };
        let source = await_bounded_generation(response, cancellation, timeout, label).await;
        self.session
            .record_token_usage_info(&self.turn, token_usage.as_ref())
            .await
            .map_err(|error| {
                bounded_error(format!("native Responses usage accounting failed: {error}"))
            })?;
        source
    }
}

fn responses_timeout(kind: GenerationKind) -> Duration {
    match kind {
        GenerationKind::Initial => INITIAL_RESPONSES_TIMEOUT,
        GenerationKind::Repair => REPAIR_RESPONSES_TIMEOUT,
    }
}

async fn await_bounded_generation<F>(
    response: F,
    cancellation: CancellationToken,
    timeout: Duration,
    label: &str,
) -> Result<String, String>
where
    F: Future<Output = Result<String, String>>,
{
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(format!("{label} cancelled")),
        result = tokio::time::timeout(timeout, response) => match result {
            Ok(result) => result,
            Err(_) => Err(format!("{label} timed out after {} seconds", timeout.as_secs())),
        },
    }
}

struct ProcessNativeExecutor {
    client: ProcessOwnedNativeCodeModeClient,
    session: Arc<Session>,
    step: Arc<StepContext>,
    tracker: SharedTurnDiffTracker,
    identity: NativeRunIdentity,
    run_tree: NativeRunTreeOwner,
}

struct WorkflowStartedDelegate {
    worker: Arc<dyn NativeCodeModeDelegate>,
    workflow_started: tokio::sync::watch::Receiver<bool>,
}

impl NativeCodeModeDelegate for WorkflowStartedDelegate {
    fn invoke<'a>(
        &'a self,
        invocation: NativeToolInvocation,
        cancellation: CancellationToken,
    ) -> NativeToolFuture<'a> {
        Box::pin(async move {
            let mut workflow_started = self.workflow_started.clone();
            while !*workflow_started.borrow() {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        return Err("native workflow cancelled before tool dispatch".to_string());
                    }
                    changed = workflow_started.changed() => {
                        if changed.is_err() {
                            return Err("native workflow start ownership closed".to_string());
                        }
                    }
                }
            }
            self.worker.invoke(invocation, cancellation).await
        })
    }
}

impl NativeExecutor for ProcessNativeExecutor {
    fn execute<'a>(
        &'a self,
        request: NativeExecute,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<NativeExecution, String>> {
        Box::pin(async move {
            if request.identity != self.identity {
                return Err("native execution identity changed during orchestration".to_string());
            }
            let worker = self.session.services.code_mode_service.start_native_worker(
                request.identity.clone(),
                request.attempt,
                Arc::clone(&self.session),
                Arc::clone(&self.step),
                Arc::clone(&self.tracker),
                crate::tools::code_mode::NativeWorkerOwnership {
                    cancellation: cancellation.clone(),
                    run_tree: self.run_tree.clone(),
                },
            );
            let worker: Arc<dyn NativeCodeModeDelegate> = worker;
            let (workflow_started_tx, workflow_started_rx) = tokio::sync::watch::channel(false);
            let delegate: Arc<dyn NativeCodeModeDelegate> = Arc::new(WorkflowStartedDelegate {
                worker,
                workflow_started: workflow_started_rx,
            });
            let (progress_tx, mut progress_rx) =
                tokio::sync::mpsc::channel(crate::native_run_tree::NATIVE_RUN_TREE_MAX_NODES);
            let attempt = request.attempt;
            let execution = self.client.execute_with_progress(
                request,
                delegate,
                progress_tx,
                cancellation.clone(),
            );
            tokio::pin!(execution);
            let compile_id = format!("compile-{attempt}");
            let workflow_id = format!("workflow-a{attempt}");
            let mut workflow_started = false;
            let mut progress_open = true;
            loop {
                tokio::select! {
                    biased;
                    progress = progress_rx.recv(), if progress_open => {
                        match progress {
                            Some(NativeProgress::CompilerStarted { pid }) => {
                                self.run_tree.update_kind(
                                    &compile_id,
                                    NativeRunNodeKind::Compile { attempt, pid: Some(pid) },
                                );
                                add_compile_source_ref(
                                    &self.run_tree,
                                    &self.identity,
                                    attempt,
                                    &compile_id,
                                );
                            }
                            Some(NativeProgress::Compiled) => {
                                // A cache hit has no compiler process event. Compiled is still
                                // authoritative proof that prepare_run retained this attempt.
                                add_compile_source_ref(
                                    &self.run_tree,
                                    &self.identity,
                                    attempt,
                                    &compile_id,
                                );
                                self.run_tree.settle(
                                    &compile_id, NativeRunNodeStatus::Succeeded, "compiled",
                                );
                            }
                            Some(NativeProgress::WorkflowStarted) => {
                                workflow_started = true;
                                self.run_tree.start(
                                    workflow_id.clone(), "run",
                                    NativeRunNodeKind::Workflow { attempt, pid: None },
                                    "workflow process",
                                    Some((NativeRunCancelScope::Run, cancellation.clone())),
                                );
                                emit_native_lifecycle(
                                    self.session.as_ref(), self.step.turn.as_ref(), &self.identity,
                                    NativeCodeModePhase::Running, String::new(),
                                ).await;
                                let _ = workflow_started_tx.send(true);
                            }
                            Some(NativeProgress::WorkflowProcessStarted { pid }) => {
                                workflow_started = true;
                                self.run_tree.start(
                                    workflow_id.clone(), "run",
                                    NativeRunNodeKind::Workflow { attempt, pid: Some(pid) },
                                    &format!("workflow pid {pid}"),
                                    Some((NativeRunCancelScope::Run, cancellation.clone())),
                                );
                                emit_native_lifecycle(
                                    self.session.as_ref(), self.step.turn.as_ref(), &self.identity,
                                    NativeCodeModePhase::Running, String::new(),
                                ).await;
                                let _ = workflow_started_tx.send(true);
                            }
                            Some(NativeProgress::DescendantStarted { pid }) => self.run_tree.start(
                                format!("process-{pid}"), workflow_id.clone(),
                                NativeRunNodeKind::Process { pid }, &format!("descendant pid {pid}"),
                                Some((NativeRunCancelScope::Run, cancellation.clone())),
                            ),
                            Some(NativeProgress::Finished) => self.run_tree.settle(
                                &workflow_id, NativeRunNodeStatus::Succeeded, "finished",
                            ),
                            // A settled host response removes the pending request and drops this
                            // sender before the response receiver necessarily wins this select.
                            // The execution result remains the authoritative terminal signal.
                            None => progress_open = false,
                        }
                    }
                    result = &mut execution => {
                        let status = if matches!(&result, Ok(NativeExecution::Completed { .. })) {
                            NativeRunNodeStatus::Succeeded
                        } else if cancellation.is_cancelled()
                            || matches!(&result, Ok(NativeExecution::Failed { failure, .. }) if failure.kind == "Cancelled")
                        {
                            NativeRunNodeStatus::Cancelled
                        } else {
                            NativeRunNodeStatus::Failed
                        };
                        if workflow_started {
                            self.run_tree.settle_unfinished(status);
                        } else {
                            self.run_tree.settle(&compile_id, status, "execution did not start");
                        }
                        break result;
                    }
                }
            }
        })
    }

    fn finalize<'a>(&'a self, identity: NativeRunIdentity) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.run_tree.start(
                "host-finalize",
                "run",
                NativeRunNodeKind::Finalization,
                "abandon repair-pending run",
                None,
            );
            let result = self.client.finalize(identity).await;
            self.run_tree.settle(
                "host-finalize",
                if result.is_ok() {
                    NativeRunNodeStatus::Succeeded
                } else {
                    NativeRunNodeStatus::Failed
                },
                if result.is_ok() {
                    "settled"
                } else {
                    "finalization failed"
                },
            );
            result
        })
    }
}

fn add_compile_source_ref(
    run_tree: &NativeRunTreeOwner,
    identity: &NativeRunIdentity,
    attempt: u8,
    compile_id: &str,
) {
    if let Ok(reference) = artifact_uri(identity, &format!("attempt-{attempt}/source.rs")) {
        run_tree.add_ref(compile_id, &reference);
    }
}

async fn prepare_live_backends(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    identity: NativeRunIdentity,
    cancellation: CancellationToken,
    run_tree: NativeRunTreeOwner,
) -> Result<(ResponsesSourceGenerator, ProcessNativeExecutor), String> {
    let client = session
        .services
        .code_mode_service
        .native_client()
        .ok_or_else(|| "the process-owned native-rust-v1 client is unavailable".to_string())?;
    let step = session
        .capture_step_context(Arc::clone(&turn), &cancellation)
        .await
        .map_err(|error| {
            bounded_error(format!("failed to capture native task context: {error}"))
        })?;
    let applicable_instructions = step
        .loaded_agents_md
        .as_deref()
        .map_or_else(String::new, crate::agents_md::LoadedAgentsMd::text);
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    Ok((
        ResponsesSourceGenerator {
            session: Arc::clone(&session),
            turn,
            applicable_instructions,
        },
        ProcessNativeExecutor {
            client,
            session,
            step,
            tracker,
            identity,
            run_tree,
        },
    ))
}

#[cfg(test)]
async fn orchestrate(
    generator: &dyn SourceGenerator,
    executor: &dyn NativeExecutor,
    task: &str,
    identity: &NativeRunIdentity,
    cancellation: CancellationToken,
) -> NativeEvidence {
    orchestrate_inner(generator, executor, task, identity, cancellation, None)
        .await
        .evidence
}

struct TerminalEvidence {
    evidence: NativeEvidence,
    outcome: NativeCodeModePhase,
}

async fn orchestrate_inner(
    generator: &dyn SourceGenerator,
    executor: &dyn NativeExecutor,
    task: &str,
    identity: &NativeRunIdentity,
    cancellation: CancellationToken,
    lifecycle: Option<&dyn NativeLifecycle>,
) -> TerminalEvidence {
    if task.len() > TASK_BYTES {
        return terminal_failure(
            identity,
            "Native Rust Code Mode rejected the task",
            "task exceeded the 16 KiB admission limit",
            None,
            /*interrupted*/ false,
        );
    }
    let source = match generator
        .generate(
            GenerationRequest {
                kind: GenerationKind::Initial,
                task,
                original_source: None,
                source_hash: None,
                diagnostic: None,
            },
            cancellation.child_token(),
        )
        .await
    {
        Ok(source) => source,
        Err(error) => {
            let interrupted = cancellation.is_cancelled();
            return terminal_failure(
                identity,
                if interrupted {
                    "Native Rust Code Mode interrupted"
                } else {
                    "Native Rust source generation failed"
                },
                if interrupted {
                    "native source generation cancelled"
                } else {
                    &error
                },
                None,
                interrupted,
            );
        }
    };

    if let Some(lifecycle) = lifecycle {
        lifecycle.transition(NativeCodeModePhase::Compiling).await;
    }
    let first = executor
        .execute(
            NativeExecute {
                identity: identity.clone(),
                attempt: 1,
                task: task.to_string(),
                source: source.clone(),
            },
            cancellation.child_token(),
        )
        .await;
    let failure = match first {
        Ok(NativeExecution::Completed { evidence, .. }) => {
            return complete_evidence(evidence, identity, 1);
        }
        Ok(NativeExecution::Failed { failure, .. }) if failure.kind == "Compile" => failure,
        Ok(NativeExecution::Failed { failure, .. }) => {
            let attempt = retained_attempt_for_failure(&failure, 1);
            return terminal_failure(
                identity,
                "Native Rust workflow failed",
                &native_failure_detail(&failure),
                attempt,
                failure.kind == "Cancelled",
            );
        }
        Err(error) => {
            let detail = finalize_after_ambiguous_delivery(executor, identity, error).await;
            let interrupted = cancellation.is_cancelled();
            return terminal_failure(
                identity,
                if interrupted {
                    "Native Rust Code Mode interrupted"
                } else {
                    "Native Rust workflow could not execute"
                },
                &detail,
                None,
                interrupted,
            );
        }
    };

    let repair = if cancellation.is_cancelled() {
        Err("native repair cancelled".to_string())
    } else {
        if let Some(lifecycle) = lifecycle {
            lifecycle.transition(NativeCodeModePhase::Repairing).await;
        }
        generator
            .generate(
                GenerationRequest {
                    kind: GenerationKind::Repair,
                    task,
                    original_source: Some(&source),
                    source_hash: Some(&failure.source_hash),
                    diagnostic: Some(&failure.diagnostic),
                },
                cancellation.child_token(),
            )
            .await
    };
    let repaired_source = match repair {
        Ok(source) => {
            if let Some(lifecycle) = lifecycle {
                lifecycle.transition(NativeCodeModePhase::Repair).await;
            }
            source
        }
        Err(error) => {
            let finalize = executor.finalize(identity.clone()).await;
            let interrupted = cancellation.is_cancelled();
            let error = if interrupted {
                "native compiler repair cancelled".to_string()
            } else {
                error
            };
            let detail = match finalize {
                Ok(()) => error,
                Err(finalize_error) => {
                    format!("{error}; run finalization failed: {finalize_error}")
                }
            };
            return terminal_failure(
                identity,
                if interrupted {
                    "Native Rust Code Mode interrupted"
                } else {
                    "Native Rust compiler repair failed"
                },
                &detail,
                Some(1),
                interrupted,
            );
        }
    };

    if let Some(lifecycle) = lifecycle {
        lifecycle.transition(NativeCodeModePhase::Compiling).await;
    }
    match executor
        .execute(
            NativeExecute {
                identity: identity.clone(),
                attempt: 2,
                task: task.to_string(),
                source: repaired_source,
            },
            cancellation.clone(),
        )
        .await
    {
        Ok(NativeExecution::Completed { evidence, .. }) => complete_evidence(evidence, identity, 2),
        Ok(NativeExecution::Failed { failure, .. }) => {
            let attempt = retained_attempt_for_failure(&failure, 2);
            terminal_failure(
                identity,
                "Native Rust workflow failed after the single repair attempt",
                &native_failure_detail(&failure),
                attempt,
                failure.kind == "Cancelled",
            )
        }
        Err(error) => {
            let detail = finalize_after_ambiguous_delivery(executor, identity, error).await;
            let interrupted = cancellation.is_cancelled();
            terminal_failure(
                identity,
                if interrupted {
                    "Native Rust Code Mode interrupted"
                } else {
                    "Native Rust repaired workflow could not execute"
                },
                &detail,
                Some(1),
                interrupted,
            )
        }
    }
}

fn native_failure_detail(failure: &codex_code_mode::host::NativeFailure) -> String {
    let diagnostic = failure.diagnostic.trim();
    if diagnostic.is_empty() {
        format!("{} failure", failure.kind)
    } else {
        format!("{} · {diagnostic}", failure.kind)
    }
}

fn retained_attempt_for_failure(
    failure: &codex_code_mode::host::NativeFailure,
    attempt: u8,
) -> Option<u8> {
    match failure.kind.as_str() {
        "Compile" | "CompileTimeout" | "Protocol" | "StderrLimit" | "WorkflowTimeout"
        | "Cancelled" | "ChildCrash" | "CallLimit" | "EvidenceLimit" | "Cleanup" => Some(attempt),
        _ => None,
    }
}

async fn finalize_after_ambiguous_delivery(
    executor: &dyn NativeExecutor,
    identity: &NativeRunIdentity,
    error: String,
) -> String {
    match executor.finalize(identity.clone()).await {
        Ok(()) => error,
        Err(finalize_error) => bounded_error(format!(
            "{error}; best-effort run finalization failed: {finalize_error}"
        )),
    }
}

fn generation_prompt(
    request: &GenerationRequest<'_>,
    cwd: &str,
    applicable_instructions: &str,
) -> Result<Prompt, String> {
    let task = bounded_owned("native task", request.task, TASK_BYTES)?;
    let user_text = match request.kind {
        GenerationKind::Initial => task,
        GenerationKind::Repair => {
            let source = bounded_owned(
                "original native source",
                request.original_source.unwrap_or_default(),
                SOURCE_BYTES,
            )?;
            let diagnostic = bounded_owned(
                "native compiler diagnostic",
                request.diagnostic.unwrap_or_default(),
                DIAGNOSTIC_BYTES,
            )?;
            format!(
                "Task:\n{task}\n\nOriginal complete source:\n{source}\n\nSource SHA-256:\n{}\n\nPinned rustc diagnostic:\n{diagnostic}\n\nReturn one corrected complete source object.",
                request.source_hash.unwrap_or_default()
            )
        }
    };
    let contract = format!(
        "You generate exactly one complete Rust source file for ycode Native Code Mode. \
No JavaScript, Cargo, dependencies, build scripts, proc macros, unsafe, Tokio, shell orchestration, \
or external crates. Use only std plus the SDK below. Runtime loops, branches, retry, spawn/join, \
aggregation, typed outcomes, and final Evidence must remain in the generated Rust. The program must \
call sdk::run and finish exactly once. For repository audits, first use bounded Shell calls to inspect \
the deterministic top-level inventory and probe required tools with `command -v`; only then branch to \
ecosystem-specific checks. Never assume Cargo or another build system from the task or cwd alone. \
Source is limited to {SOURCE_BYTES} UTF-8 bytes and \
{SOURCE_LINES} lines. Current cwd: {cwd}\n\n{SDK_CONTRACT}\n\nApplicable AGENTS instructions:\n{applicable_instructions}"
    );
    Ok(Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText { text: user_text }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        tools: Vec::new(),
        parallel_tool_calls: false,
        base_instructions: BaseInstructions { text: contract },
        output_schema: Some(json!({
            "type": "object",
            "properties": { "source": { "type": "string" } },
            "required": ["source"],
            "additionalProperties": false
        })),
        output_schema_strict: true,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceObject {
    source: String,
}

async fn collect_source(
    mut stream: crate::ResponseStream,
    cancellation: CancellationToken,
    token_usage: &mut Option<TokenUsage>,
) -> Result<String, String> {
    let mut assistant_text = None;
    let mut completed = false;
    let mut terminal_error = None;
    loop {
        let event = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err("native generation cancelled".to_string()),
            event = stream.next() => event,
        };
        let Some(event) = event else {
            break;
        };
        match event
            .map_err(|error| bounded_error(format!("native Responses stream failed: {error}")))?
        {
            codex_api::ResponseEvent::OutputItemDone(ResponseItem::Reasoning { .. })
            | codex_api::ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. }) => {}
            codex_api::ResponseEvent::OutputItemDone(ResponseItem::Message {
                role,
                content,
                ..
            }) if role == "assistant" => {
                if assistant_text.is_some() || content.len() != 1 {
                    terminal_error.get_or_insert_with(|| {
                        "native generation returned multiple assistant outputs".to_string()
                    });
                    continue;
                }
                let Some(ContentItem::OutputText { text }) = content.into_iter().next() else {
                    terminal_error.get_or_insert_with(|| {
                        "native generation returned a non-text assistant output".to_string()
                    });
                    continue;
                };
                if text.len() > MODEL_OUTPUT_BYTES {
                    terminal_error.get_or_insert_with(|| {
                        "native generation output exceeded its bounded envelope".to_string()
                    });
                    continue;
                }
                assistant_text = Some(text);
            }
            codex_api::ResponseEvent::OutputItemAdded(ResponseItem::Message { role, .. })
                if role == "assistant" => {}
            codex_api::ResponseEvent::OutputItemDone(_)
            | codex_api::ResponseEvent::OutputItemAdded(_) => {
                terminal_error.get_or_insert_with(|| {
                    "native generation returned forbidden extra output".to_string()
                });
            }
            codex_api::ResponseEvent::Completed {
                token_usage: completed_usage,
                ..
            } => {
                completed = true;
                *token_usage = completed_usage;
                break;
            }
            _ => {}
        }
    }
    if !completed {
        return Err("native generation ended before response.completed".to_string());
    }
    if let Some(error) = terminal_error {
        return Err(error);
    }
    let text = assistant_text
        .ok_or_else(|| "native generation returned no terminal assistant source".to_string())?;
    let object: SourceObject = serde_json::from_str(&text).map_err(|error| {
        bounded_error(format!(
            "native generation returned invalid source JSON: {error}"
        ))
    })?;
    validate_source(&object.source)?;
    Ok(object.source)
}

fn validate_source(source: &str) -> Result<(), String> {
    if source.len() > SOURCE_BYTES {
        return Err(format!("native source exceeds {SOURCE_BYTES} bytes"));
    }
    if source.lines().count() > SOURCE_LINES {
        return Err(format!("native source exceeds {SOURCE_LINES} lines"));
    }
    if source.trim().is_empty() {
        return Err("native source is empty".to_string());
    }
    Ok(())
}

fn complete_evidence(
    mut evidence: NativeEvidence,
    identity: &NativeRunIdentity,
    attempt: u8,
) -> TerminalEvidence {
    evidence.artifact_refs.push("evidence.json".to_string());
    match normalize_evidence(evidence, identity, Some(attempt)) {
        Ok(evidence) => TerminalEvidence {
            evidence,
            outcome: NativeCodeModePhase::Succeeded,
        },
        Err(error) => terminal_failure(
            identity,
            "Native Rust workflow returned invalid Evidence",
            &error,
            Some(attempt),
            /*interrupted*/ false,
        ),
    }
}

fn normalize_evidence(
    mut evidence: NativeEvidence,
    identity: &NativeRunIdentity,
    attempt: Option<u8>,
) -> Result<NativeEvidence, String> {
    if evidence.version != NATIVE_PROTOCOL_VERSION {
        return Err("unsupported Evidence version".to_string());
    }
    validate_host_artifact_contract(&evidence.artifact_refs, &evidence.provenance_ids)?;
    let mut refs = BTreeSet::new();
    for logical in evidence.artifact_refs {
        validate_host_artifact_ref(&logical, &evidence.provenance_ids)?;
        refs.insert(artifact_uri(identity, &logical)?);
    }
    if let Some(attempt) = attempt {
        refs.insert(artifact_uri(
            identity,
            &format!("attempt-{attempt}/source.rs"),
        )?);
    }
    evidence.artifact_refs = refs.into_iter().collect();
    validate_evidence_fields(&evidence)?;
    Ok(evidence)
}

fn validate_host_artifact_contract(
    artifact_refs: &[String],
    provenance_ids: &[String],
) -> Result<(), String> {
    let mut unique = BTreeSet::new();
    for provenance in provenance_ids {
        if !unique.insert(provenance.as_str()) {
            return Err("native host returned duplicate Evidence provenance".to_string());
        }
        for suffix in ["request.bin", "result.bin"] {
            let expected = format!("calls/{provenance}.{suffix}");
            if !artifact_refs.iter().any(|reference| reference == &expected) {
                return Err(
                    "native host omitted a retained artifact for Evidence provenance".to_string(),
                );
            }
        }
    }
    Ok(())
}

fn validate_host_artifact_ref(logical: &str, provenance_ids: &[String]) -> Result<(), String> {
    if logical == "evidence.json" {
        return Ok(());
    }
    let Some(rest) = logical.strip_prefix("calls/") else {
        return Err("native host returned an unverified artifact reference".to_string());
    };
    let call_id = rest
        .strip_suffix(".request.bin")
        .or_else(|| rest.strip_suffix(".result.bin"))
        .ok_or_else(|| "native host returned an invalid call artifact reference".to_string())?;
    if !provenance_ids
        .iter()
        .any(|provenance| provenance == call_id)
    {
        return Err("native host returned an unverified call artifact reference".to_string());
    }
    Ok(())
}

fn failure_evidence(
    identity: &NativeRunIdentity,
    summary: &str,
    detail: &str,
    attempt: Option<u8>,
) -> NativeEvidence {
    let mut refs = Vec::new();
    if let Some(attempt) = attempt {
        refs.extend(artifact_uri(
            identity,
            &format!("attempt-{attempt}/source.rs"),
        ));
    }
    NativeEvidence {
        version: NATIVE_PROTOCOL_VERSION,
        summary: truncate_utf8(summary, EVIDENCE_STRING_BYTES),
        verified: Vec::new(),
        disputed: Vec::new(),
        unresolved: vec!["The requested native workflow did not complete.".to_string()],
        artifact_refs: refs,
        partial_failures: vec![truncate_utf8(detail, EVIDENCE_STRING_BYTES)],
        provenance_ids: Vec::new(),
    }
}

fn terminal_failure(
    identity: &NativeRunIdentity,
    summary: &str,
    detail: &str,
    attempt: Option<u8>,
    interrupted: bool,
) -> TerminalEvidence {
    TerminalEvidence {
        evidence: failure_evidence(identity, summary, detail, attempt),
        outcome: if interrupted {
            NativeCodeModePhase::Interrupted
        } else {
            NativeCodeModePhase::Failed
        },
    }
}

fn terminal_evidence_item(
    terminal: TerminalEvidence,
    identity: &NativeRunIdentity,
) -> (String, ResponseItem, NativeCodeModePhase) {
    let (evidence, mut outcome) = match validate_terminal_evidence(terminal.evidence, identity) {
        Ok(evidence) => (evidence, terminal.outcome),
        Err(_) => (
            fixed_fallback_evidence("terminal Evidence validation failed"),
            NativeCodeModePhase::Failed,
        ),
    };
    let mut text = serde_json::to_string(&evidence).unwrap_or_else(|_| {
        serde_json::to_string(&fixed_fallback_evidence(
            "terminal Evidence encoding failed",
        ))
        .unwrap_or_else(|_| fixed_fallback_json())
    });
    let mut item = assistant_item(text.clone());
    if serde_json::to_vec(&item).map_or(true, |encoded| encoded.len() > FINAL_EVIDENCE_BYTES) {
        outcome = NativeCodeModePhase::Failed;
        text = serde_json::to_string(&fixed_fallback_evidence(
            "terminal Evidence exceeded the 16 KiB boundary",
        ))
        .unwrap_or_else(|_| fixed_fallback_json());
        item = assistant_item(text.clone());
    }
    assert!(
        serde_json::to_vec(&item).is_ok_and(|encoded| encoded.len() <= FINAL_EVIDENCE_BYTES),
        "fixed native Evidence must fit its history boundary"
    );
    (text, item, outcome)
}

fn validate_terminal_evidence(
    evidence: NativeEvidence,
    identity: &NativeRunIdentity,
) -> Result<NativeEvidence, String> {
    if evidence.version != NATIVE_PROTOCOL_VERSION {
        return Err("unsupported Evidence version".to_string());
    }
    let mut unique_provenance = BTreeSet::new();
    if evidence
        .provenance_ids
        .iter()
        .any(|provenance| !unique_provenance.insert(provenance.as_str()))
    {
        return Err("Evidence contains duplicate provenance".to_string());
    }
    let prefix = format!(
        "native-code-mode://{}/{}/",
        identity.thread_id, identity.run_id
    );
    let mut logical_refs = Vec::new();
    for reference in &evidence.artifact_refs {
        let logical = reference
            .strip_prefix(&prefix)
            .ok_or_else(|| "Evidence artifact reference changed run identity".to_string())?;
        validate_logical_artifact(logical)?;
        let known_core_ref = logical == "evidence.json"
            || matches!(logical, "attempt-1/source.rs" | "attempt-2/source.rs");
        if !known_core_ref {
            validate_host_artifact_ref(logical, &evidence.provenance_ids)?;
        }
        logical_refs.push(logical.to_string());
    }
    validate_host_artifact_contract(&logical_refs, &evidence.provenance_ids)?;
    validate_evidence_fields(&evidence)?;
    Ok(evidence)
}

fn fixed_fallback_evidence(reason: &str) -> NativeEvidence {
    NativeEvidence {
        version: NATIVE_PROTOCOL_VERSION,
        summary: "Native Rust Code Mode failed to produce bounded Evidence.".to_string(),
        verified: Vec::new(),
        disputed: Vec::new(),
        unresolved: vec![reason.to_string()],
        artifact_refs: Vec::new(),
        partial_failures: Vec::new(),
        provenance_ids: Vec::new(),
    }
}

fn fixed_fallback_json() -> String {
    r#"{"version":1,"summary":"Native Rust Code Mode failed to produce bounded Evidence.","verified":[],"disputed":[],"unresolved":["terminal Evidence encoding failed"],"artifact_refs":[],"partial_failures":[],"provenance_ids":[]}"#.to_string()
}

fn assistant_item(text: String) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText { text }],
        phase: Some(codex_protocol::models::MessagePhase::FinalAnswer),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn validate_evidence_fields(evidence: &NativeEvidence) -> Result<(), String> {
    if evidence.summary.len() > EVIDENCE_STRING_BYTES {
        return Err("Evidence summary exceeds its string boundary".to_string());
    }
    for values in [
        &evidence.verified,
        &evidence.disputed,
        &evidence.unresolved,
        &evidence.artifact_refs,
        &evidence.partial_failures,
        &evidence.provenance_ids,
    ] {
        if values.len() > EVIDENCE_ITEMS
            || values
                .iter()
                .any(|value| value.len() > EVIDENCE_STRING_BYTES)
        {
            return Err("Evidence collection exceeds its bounded shape".to_string());
        }
    }
    if evidence
        .exact_json_wire_len()
        .map_err(|error| format!("failed to measure Evidence: {error}"))?
        > FINAL_EVIDENCE_BYTES
    {
        return Err("Evidence exceeds its 16 KiB wire boundary".to_string());
    }
    Ok(())
}

fn artifact_uri(identity: &NativeRunIdentity, logical: &str) -> Result<String, String> {
    let prefix = format!(
        "native-code-mode://{}/{}/",
        identity.thread_id, identity.run_id
    );
    if let Some(rest) = logical.strip_prefix(&prefix) {
        validate_logical_artifact(rest)?;
        return Ok(format!("{prefix}{rest}"));
    }
    validate_logical_artifact(logical)?;
    Ok(format!("{prefix}{logical}"))
}

fn validate_logical_artifact(logical: &str) -> Result<(), String> {
    if logical.is_empty() || logical.len() > 512 || logical.starts_with('/') {
        return Err("invalid native artifact reference".to_string());
    }
    for component in logical.split('/') {
        if component.is_empty()
            || matches!(component, "." | "..")
            || !component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err("invalid native artifact reference".to_string());
        }
    }
    Ok(())
}

fn bounded_owned(label: &str, value: &str, limit: usize) -> Result<String, String> {
    if value.len() > limit {
        Err(format!("{label} exceeds {limit} bytes"))
    } else {
        Ok(value.to_string())
    }
}

fn bounded_error(message: String) -> String {
    truncate_utf8(&message, EVIDENCE_STRING_BYTES)
}

fn truncate_utf8(value: &str, limit: usize) -> String {
    const MARKER: &str = "...[truncated]";
    if value.len() <= limit {
        return value.to_string();
    }
    let mut end = limit.saturating_sub(MARKER.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARKER}", &value[..end])
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use codex_code_mode::host::NativeFailure;
    use codex_login::CodexAuth;
    use codex_protocol::models::ReasoningItemContent;
    use codex_protocol::user_input::UserInput;
    use core_test_support::responses;
    use futures::FutureExt;
    use pretty_assertions::assert_eq;
    use wiremock::Mock;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path_regex;

    use super::*;

    struct FakeGenerator {
        results: Mutex<VecDeque<Result<String, String>>>,
        calls: AtomicUsize,
        observed: Mutex<Vec<GenerationKind>>,
    }

    impl FakeGenerator {
        fn new(results: impl IntoIterator<Item = Result<String, String>>) -> Self {
            Self {
                results: Mutex::new(results.into_iter().collect()),
                calls: AtomicUsize::new(0),
                observed: Mutex::new(Vec::new()),
            }
        }
    }

    impl SourceGenerator for FakeGenerator {
        fn generate<'a>(
            &'a self,
            request: GenerationRequest<'a>,
            _cancellation: CancellationToken,
        ) -> BoxFuture<'a, Result<String, String>> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.observed
                .lock()
                .expect("observed lock")
                .push(request.kind);
            let result = self.results.lock().expect("result lock").pop_front();
            async move { result.expect("one fake generation result per call") }.boxed()
        }
    }

    struct FakeExecutor {
        results: Mutex<VecDeque<Result<NativeExecution, String>>>,
        attempts: Mutex<Vec<u8>>,
        finalized: AtomicUsize,
    }

    struct CancelOnRepairGenerator {
        repair_started: Arc<tokio::sync::Notify>,
        calls: Arc<AtomicUsize>,
    }

    impl SourceGenerator for CancelOnRepairGenerator {
        fn generate<'a>(
            &'a self,
            request: GenerationRequest<'a>,
            cancellation: CancellationToken,
        ) -> BoxFuture<'a, Result<String, String>> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            match request.kind {
                GenerationKind::Initial => {
                    async { Ok("use ycode_native_sdk as sdk; fn main( {".to_string()) }.boxed()
                }
                GenerationKind::Repair => {
                    let repair_started = Arc::clone(&self.repair_started);
                    async move {
                        repair_started.notify_one();
                        cancellation.cancelled().await;
                        Err("native repair cancelled".to_string())
                    }
                    .boxed()
                }
            }
        }
    }

    impl FakeExecutor {
        fn new(results: impl IntoIterator<Item = Result<NativeExecution, String>>) -> Self {
            Self {
                results: Mutex::new(results.into_iter().collect()),
                attempts: Mutex::new(Vec::new()),
                finalized: AtomicUsize::new(0),
            }
        }
    }

    impl NativeExecutor for FakeExecutor {
        fn execute<'a>(
            &'a self,
            request: NativeExecute,
            _cancellation: CancellationToken,
        ) -> BoxFuture<'a, Result<NativeExecution, String>> {
            self.attempts
                .lock()
                .expect("attempt lock")
                .push(request.attempt);
            let result = self.results.lock().expect("result lock").pop_front();
            async move { result.expect("one fake execution result per call") }.boxed()
        }

        fn finalize<'a>(
            &'a self,
            _identity: NativeRunIdentity,
        ) -> BoxFuture<'a, Result<(), String>> {
            self.finalized.fetch_add(1, Ordering::AcqRel);
            async { Ok(()) }.boxed()
        }
    }

    fn identity() -> NativeRunIdentity {
        NativeRunIdentity {
            session_id: "native-session".to_string(),
            thread_id: "00000000-0000-4000-8000-000000000001".to_string(),
            run_id: "00000000-0000-4000-8000-000000000002".to_string(),
        }
    }

    fn compiled(identity: &NativeRunIdentity, attempt: u8) -> NativeExecution {
        NativeExecution::Completed {
            identity: identity.clone(),
            source_hash: "a".repeat(64),
            evidence: NativeEvidence {
                version: 1,
                summary: format!("completed attempt {attempt}"),
                verified: vec!["fixture completed".to_string()],
                disputed: Vec::new(),
                unresolved: Vec::new(),
                artifact_refs: Vec::new(),
                partial_failures: Vec::new(),
                provenance_ids: Vec::new(),
            },
        }
    }

    fn compile_failure(identity: &NativeRunIdentity, attempt: u8) -> NativeExecution {
        NativeExecution::Failed {
            identity: identity.clone(),
            failure: NativeFailure {
                kind: "Compile".to_string(),
                source_hash: format!("{:064x}", attempt),
                diagnostic: "error: expected expression".to_string(),
                process_reaped: Some(true),
            },
        }
    }

    #[tokio::test]
    async fn compile_reject_repairs_once_then_succeeds_with_exact_counts() {
        let identity = identity();
        let generator = FakeGenerator::new([
            Ok("fn attempt_one() {}".to_string()),
            Ok("fn attempt_two() {}".to_string()),
        ]);
        let executor = FakeExecutor::new([
            Ok(compile_failure(&identity, 1)),
            Ok(compiled(&identity, 2)),
        ]);
        let evidence = orchestrate(
            &generator,
            &executor,
            "complete the fixture",
            &identity,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(generator.calls.load(Ordering::Acquire), 2);
        assert_eq!(*executor.attempts.lock().expect("attempt lock"), [1, 2]);
        assert_eq!(executor.finalized.load(Ordering::Acquire), 0);
        assert_eq!(evidence.summary, "completed attempt 2");
    }

    #[tokio::test]
    async fn second_compile_reject_stops_without_third_call_or_compile() {
        let identity = identity();
        let generator = FakeGenerator::new([
            Ok("fn attempt_one() {}".to_string()),
            Ok("fn attempt_two() {}".to_string()),
        ]);
        let executor = FakeExecutor::new([
            Ok(compile_failure(&identity, 1)),
            Ok(compile_failure(&identity, 2)),
        ]);
        let evidence = orchestrate(
            &generator,
            &executor,
            "complete the fixture",
            &identity,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(generator.calls.load(Ordering::Acquire), 2);
        assert_eq!(*executor.attempts.lock().expect("attempt lock"), [1, 2]);
        assert!(evidence.summary.contains("single repair attempt"));
    }

    #[tokio::test]
    async fn repaired_attempt_transport_error_finalizes_once_without_third_work() {
        let identity = identity();
        let generator = FakeGenerator::new([
            Ok("fn attempt_one() {}".to_string()),
            Ok("fn attempt_two() {}".to_string()),
        ]);
        let executor = FakeExecutor::new([
            Ok(compile_failure(&identity, 1)),
            Err("attempt two transport disconnected".to_string()),
        ]);
        let evidence = orchestrate(
            &generator,
            &executor,
            "complete the fixture",
            &identity,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(generator.calls.load(Ordering::Acquire), 2);
        assert_eq!(*executor.attempts.lock().expect("attempt lock"), [1, 2]);
        assert_eq!(executor.finalized.load(Ordering::Acquire), 1);
        assert!(evidence.summary.contains("could not execute"));
        assert!(
            evidence
                .artifact_refs
                .iter()
                .all(|reference| !reference.contains("attempt-2"))
        );
    }

    #[tokio::test]
    async fn first_attempt_transport_ambiguity_finalizes_without_claiming_artifacts() {
        let identity = identity();
        let generator = FakeGenerator::new([Ok("fn main() {}".to_string())]);
        let executor = FakeExecutor::new([Err("delivery uncertain".to_string())]);
        let evidence = orchestrate(
            &generator,
            &executor,
            "complete the fixture",
            &identity,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(generator.calls.load(Ordering::Acquire), 1);
        assert_eq!(*executor.attempts.lock().expect("attempt lock"), [1]);
        assert_eq!(executor.finalized.load(Ordering::Acquire), 1);
        assert!(evidence.artifact_refs.is_empty());
    }

    #[tokio::test]
    async fn repair_failure_finalizes_repair_pending_run() {
        let identity = identity();
        let generator = FakeGenerator::new([
            Ok("fn attempt_one() {}".to_string()),
            Err("repair transport failed".to_string()),
        ]);
        let executor = FakeExecutor::new([Ok(compile_failure(&identity, 1))]);
        let evidence = orchestrate(
            &generator,
            &executor,
            "complete the fixture",
            &identity,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(generator.calls.load(Ordering::Acquire), 2);
        assert_eq!(*executor.attempts.lock().expect("attempt lock"), [1]);
        assert_eq!(executor.finalized.load(Ordering::Acquire), 1);
        assert!(evidence.summary.contains("repair failed"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn repair_cancellation_finalizes_the_actual_host_manifest() {
        let Some(host_program) = std::env::var_os("YCODE_TEST_CODE_MODE_HOST") else {
            eprintln!("skipping real repair finalization proof without host binary");
            return;
        };
        let codex_home = tempfile::tempdir().expect("temp ycode home");
        let _home = EnvGuard::set("CODEX_HOME", codex_home.path().as_os_str());
        let (mut session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                CodexAuth::from_api_key("test-api-key"),
                Vec::new(),
                codex_home.path(),
                |_| {},
            )
            .await;
        let provider = Arc::new(
            codex_code_mode::ProcessOwnedCodeModeSessionProvider::with_host_program(PathBuf::from(
                host_program,
            )),
        );
        let features = session.features().clone();
        Arc::get_mut(&mut session)
            .expect("test session is uniquely owned")
            .services
            .code_mode_service = crate::tools::code_mode::CodeModeService::new_with_native_client(
            Arc::clone(&provider) as Arc<dyn codex_code_mode::CodeModeSessionProvider>,
            Some(provider.native_client()),
            &features,
        );
        let identity = NativeRunIdentity {
            session_id: session.thread_id.to_string(),
            thread_id: session.thread_id.to_string(),
            run_id: "50000000-0000-4000-8000-000000000005".to_string(),
        };
        let cancellation = CancellationToken::new();
        let run_tree = session
            .services
            .code_mode_service
            .native_run_trees()
            .begin(
                identity.clone(),
                "repair cancellation test",
                cancellation.clone(),
            )
            .expect("run tree");
        let (_responses, executor) = prepare_live_backends(
            Arc::clone(&session),
            turn,
            identity.clone(),
            cancellation.clone(),
            run_tree,
        )
        .await
        .expect("real native executor");
        let repair_started = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let generator = CancelOnRepairGenerator {
            repair_started: Arc::clone(&repair_started),
            calls: Arc::clone(&calls),
        };
        let run_dir = codex_home
            .path()
            .join("native-code-mode/v1/sessions")
            .join(&identity.thread_id)
            .join("runs")
            .join(&identity.run_id);
        let execute_identity = identity.clone();
        let execute_cancellation = cancellation.clone();
        let orchestration = tokio::spawn(async move {
            orchestrate(
                &generator,
                &executor,
                "compile then cancel the repair",
                &execute_identity,
                execute_cancellation,
            )
            .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            repair_started.notified(),
        )
        .await
        .expect("attempt-1 Compile reaches the repair boundary");
        let pending: serde_json::Value = serde_json::from_slice(
            &std::fs::read(run_dir.join("manifest.json")).expect("pending run manifest"),
        )
        .expect("pending manifest JSON");
        assert!(pending["completed_at_unix_ms"].is_null());

        cancellation.cancel();
        let evidence = tokio::time::timeout(std::time::Duration::from_secs(2), orchestration)
            .await
            .expect("cancelled repair orchestration settles")
            .expect("orchestration task joins");
        assert_eq!(evidence.summary, "Native Rust Code Mode interrupted");
        assert_eq!(
            evidence.partial_failures,
            ["native compiler repair cancelled"]
        );
        assert_eq!(calls.load(Ordering::Acquire), 2);
        let completed: serde_json::Value = serde_json::from_slice(
            &std::fs::read(run_dir.join("manifest.json")).expect("completed run manifest"),
        )
        .expect("completed manifest JSON");
        assert!(completed["completed_at_unix_ms"].is_u64());
        drop(provider);
    }

    #[tokio::test]
    async fn generation_terminal_errors_never_compile_or_repair() {
        for message in [
            "refusal: request was declined",
            "incomplete response",
            "authentication failed",
            "entitlement unavailable",
            "transport disconnected",
            "native generation cancelled",
        ] {
            let identity = identity();
            let generator = FakeGenerator::new([Err(message.to_string())]);
            let executor = FakeExecutor::new([]);
            let evidence = orchestrate(
                &generator,
                &executor,
                "complete the fixture",
                &identity,
                CancellationToken::new(),
            )
            .await;
            assert_eq!(generator.calls.load(Ordering::Acquire), 1, "{message}");
            assert!(executor.attempts.lock().expect("attempt lock").is_empty());
            assert_eq!(executor.finalized.load(Ordering::Acquire), 0);
            assert!(evidence.summary.contains("generation failed"));
            assert!(evidence.artifact_refs.is_empty());
        }
    }

    #[tokio::test]
    async fn non_compile_attempt_one_failures_never_trigger_repair() {
        for kind in [
            "Admission",
            "CompilerUnavailable",
            "CompilerVersion",
            "Runtime",
            "Tool",
            "EvidenceLimit",
            "Cancelled",
            "Protocol",
        ] {
            let identity = identity();
            let generator = FakeGenerator::new([Ok("fn main() {}".to_string())]);
            let executor = FakeExecutor::new([Ok(NativeExecution::Failed {
                identity: identity.clone(),
                failure: NativeFailure {
                    kind: kind.to_string(),
                    source_hash: "b".repeat(64),
                    diagnostic: "local diagnostic".to_string(),
                    process_reaped: None,
                },
            })]);
            let evidence = orchestrate(
                &generator,
                &executor,
                "complete the fixture",
                &identity,
                CancellationToken::new(),
            )
            .await;
            assert_eq!(generator.calls.load(Ordering::Acquire), 1, "{kind}");
            assert_eq!(*executor.attempts.lock().expect("attempt lock"), [1]);
            assert_eq!(executor.finalized.load(Ordering::Acquire), 0);
            if matches!(
                kind,
                "Admission" | "CompilerUnavailable" | "CompilerVersion" | "Runtime" | "Tool"
            ) {
                assert!(evidence.artifact_refs.is_empty(), "{kind}");
            }
        }
    }

    #[tokio::test]
    async fn retained_provenance_protocol_failure_surfaces_concrete_host_diagnostic() {
        let identity = identity();
        let source_hash = "c".repeat(64);
        let diagnostic = format!(
            "evidence provenance id does not identify a joined native call\n[source_hash={source_hash}]"
        );
        let generator = FakeGenerator::new([Ok("fn main() {}".to_string())]);
        let executor = FakeExecutor::new([Ok(NativeExecution::Failed {
            identity: identity.clone(),
            failure: NativeFailure {
                kind: "Protocol".to_string(),
                source_hash,
                diagnostic: diagnostic.clone(),
                process_reaped: Some(true),
            },
        })]);

        let evidence = orchestrate(
            &generator,
            &executor,
            "inspect the repository",
            &identity,
            CancellationToken::new(),
        )
        .await;

        assert_eq!(generator.calls.load(Ordering::Acquire), 1);
        assert_eq!(*executor.attempts.lock().expect("attempt lock"), [1]);
        assert_eq!(executor.finalized.load(Ordering::Acquire), 0);
        assert_eq!(
            evidence.partial_failures,
            [format!("Protocol · {diagnostic}")]
        );
        assert!(evidence.summary.contains("workflow failed"));
        assert!(
            evidence
                .artifact_refs
                .iter()
                .any(|reference| reference.ends_with("/attempt-1/source.rs"))
        );
    }

    #[test]
    fn prompt_is_history_free_tools_empty_and_strict_with_complete_repair_input() {
        let initial = generation_prompt(
            &GenerationRequest {
                kind: GenerationKind::Initial,
                task: "human task only",
                original_source: None,
                source_hash: None,
                diagnostic: None,
            },
            "/tmp/workspace",
            "applicable instruction",
        )
        .expect("initial prompt");
        assert!(initial.tools.is_empty());
        assert!(!initial.parallel_tool_calls);
        assert!(initial.output_schema_strict);
        assert_eq!(initial.input.len(), 1);
        let ResponseItem::Message { role, content, .. } = &initial.input[0] else {
            panic!("expected one user message")
        };
        assert_eq!(role, "user");
        assert_eq!(
            content,
            &[ContentItem::InputText {
                text: "human task only".to_string()
            }]
        );
        assert_eq!(
            initial.output_schema,
            Some(json!({
                "type": "object",
                "properties": { "source": { "type": "string" } },
                "required": ["source"],
                "additionalProperties": false
            }))
        );
        assert!(initial.base_instructions.text.contains(SDK_CONTRACT));
        assert!(initial.base_instructions.text.contains("output: Vec<u8>"));
        assert!(
            initial
                .base_instructions
                .text
                .contains("inspect the deterministic top-level inventory")
        );
        assert!(
            initial
                .base_instructions
                .text
                .contains("probe required tools with `command -v`")
        );
        assert!(
            initial
                .base_instructions
                .text
                .contains("Never assume Cargo")
        );
        assert!(
            initial
                .base_instructions
                .text
                .contains("exact `call_id` from a completed, joined Outcome")
        );
        assert!(
            initial
                .base_instructions
                .text
                .contains("Set `artifact_refs` empty")
        );
        assert!(
            initial
                .base_instructions
                .text
                .contains("cancelled() -> Result<bool>")
        );
        assert!(
            initial
                .base_instructions
                .text
                .contains("applicable instruction")
        );

        let source_hash = "a".repeat(64);
        let repair = generation_prompt(
            &GenerationRequest {
                kind: GenerationKind::Repair,
                task: "human task only",
                original_source: Some("fn main() {}"),
                source_hash: Some(&source_hash),
                diagnostic: Some("error: expected item"),
            },
            "/tmp/workspace",
            "applicable instruction",
        )
        .expect("repair prompt");
        let serialized = serde_json::to_string(&repair.input).expect("serialize repair input");
        assert!(serialized.contains("human task only"));
        assert!(serialized.contains("fn main() {}"));
        assert!(serialized.contains("error: expected item"));
    }

    #[test]
    fn terminal_evidence_is_one_bounded_assistant_item_and_falls_back_on_escape_growth() {
        let identity = identity();
        let evidence = NativeEvidence {
            version: 1,
            summary: "\u{0001}".repeat(2_000),
            verified: vec!["\u{0002}".repeat(2_000)],
            disputed: Vec::new(),
            unresolved: Vec::new(),
            artifact_refs: vec!["result.txt".to_string()],
            partial_failures: Vec::new(),
            provenance_ids: Vec::new(),
        };
        let (text, item, outcome) = terminal_evidence_item(
            TerminalEvidence {
                evidence,
                outcome: NativeCodeModePhase::Succeeded,
            },
            &identity,
        );
        assert!(serde_json::to_vec(&item).expect("encode item").len() <= FINAL_EVIDENCE_BYTES);
        assert!(text.contains("failed to produce bounded Evidence"));
        assert_eq!(outcome, NativeCodeModePhase::Failed);
        let ResponseItem::Message { role, content, .. } = item else {
            panic!("expected assistant Evidence item")
        };
        assert_eq!(role, "assistant");
        assert_eq!(content.len(), 1);
    }

    #[test]
    fn artifact_refs_are_canonical_and_contained() {
        let identity = identity();
        assert_eq!(
            artifact_uri(&identity, "attempt-1/source.rs").expect("valid artifact"),
            "native-code-mode://00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/attempt-1/source.rs"
        );
        assert!(artifact_uri(&identity, "../outside").is_err());
        assert!(artifact_uri(&identity, "/absolute").is_err());

        let provenance = "native-run-a1-1".to_string();
        let valid_refs = vec![
            format!("calls/{provenance}.request.bin"),
            format!("calls/{provenance}.result.bin"),
        ];
        assert!(validate_host_artifact_contract(&valid_refs, &[provenance.clone()]).is_ok());
        assert!(
            validate_host_artifact_contract(&valid_refs, &[provenance.clone(), provenance])
                .is_err()
        );
        assert!(
            validate_host_artifact_contract(
                &["calls/unknown.request.bin".to_string()],
                &["native-run-a1-1".to_string()],
            )
            .is_err()
        );
        assert!(
            validate_host_artifact_ref("../outside", &["native-run-a1-1".to_string()]).is_err()
        );

        let rejected = complete_evidence(
            NativeEvidence {
                version: 1,
                summary: "untrusted child reference".to_string(),
                verified: Vec::new(),
                disputed: Vec::new(),
                unresolved: Vec::new(),
                artifact_refs: vec!["workspace/output.txt".to_string()],
                partial_failures: Vec::new(),
                provenance_ids: Vec::new(),
            },
            &identity,
            1,
        );
        assert!(rejected.evidence.summary.contains("invalid Evidence"));
        assert!(
            rejected
                .evidence
                .artifact_refs
                .iter()
                .all(|reference| reference.ends_with("/attempt-1/source.rs"))
        );
    }

    #[test]
    fn cold_and_cache_hit_compile_progress_expose_only_the_retained_source_ref() {
        for cache_hit in [false, true] {
            let identity = identity();
            let registry = Arc::new(crate::native_run_tree::NativeRunTreeRegistry::default());
            let owner = registry
                .begin(identity.clone(), "inspect", CancellationToken::new())
                .expect("begin");
            owner.start(
                "compile-1",
                "run",
                NativeRunNodeKind::Compile {
                    attempt: 1,
                    pid: None,
                },
                "compile attempt 1",
                None,
            );
            if !cache_hit {
                add_compile_source_ref(&owner, &identity, 1, "compile-1");
            }
            // Both cold compiles and cache hits receive Compiled. Repeating the
            // authoritative reference must not duplicate it.
            add_compile_source_ref(&owner, &identity, 1, "compile-1");
            owner.settle("compile-1", NativeRunNodeStatus::Succeeded, "compiled");

            let receiver = registry
                .subscribe(&identity.thread_id, &identity.run_id)
                .expect("subscribe");
            let snapshot = receiver.borrow().clone().expect("active");
            let compile = snapshot
                .nodes
                .iter()
                .find(|node| node.stable_id == "compile-1")
                .expect("compile node");
            assert_eq!(
                compile.artifact_refs,
                [artifact_uri(&identity, "attempt-1/source.rs").expect("source uri")],
                "cache_hit={cache_hit}"
            );
            assert!(snapshot.nodes[0].artifact_refs.is_empty());
        }
    }

    #[tokio::test]
    async fn mocked_official_responses_request_is_history_free_tools_empty_and_strict() {
        for auth in [
            CodexAuth::from_api_key("test-api-key"),
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        ] {
            let server = responses::start_mock_server().await;
            let mock = responses::mount_sse_once(
                &server,
                responses::sse(vec![
                    responses::ev_assistant_message(
                        "native-source",
                        r#"{"source":"fn main() {}"}"#,
                    ),
                    responses::ev_completed_with_tokens("native-response", 7),
                ]),
            )
            .await;
            let base_url = format!("{}/v1", server.uri());
            let (session, turn, _events) =
                crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
                    auth,
                    Vec::new(),
                    |config| {
                        config.model_provider.base_url = Some(base_url);
                        config.model_provider.supports_websockets = false;
                    },
                )
                .await;
            session
                .record_conversation_items(
                    turn.as_ref(),
                    &[ResponseItem::Message {
                        id: None,
                        role: "user".to_string(),
                        content: vec![ContentItem::InputText {
                            text: "ordinary history secret".to_string(),
                        }],
                        phase: None,
                        internal_chat_message_metadata_passthrough: None,
                    }],
                )
                .await;
            let history_before = session.clone_history().await.into_raw_items();
            let generator = ResponsesSourceGenerator {
                session: Arc::clone(&session),
                turn,
                applicable_instructions: "follow the local AGENTS instruction".to_string(),
            };
            let source = generator
                .generate_inner(
                    GenerationRequest {
                        kind: GenerationKind::Initial,
                        task: "human native task",
                        original_source: None,
                        source_hash: None,
                        diagnostic: None,
                    },
                    CancellationToken::new(),
                )
                .await
                .expect("mocked generation succeeds");
            assert_eq!(source, "fn main() {}");
            let usage = session
                .token_usage_info()
                .await
                .expect("nonzero native generation usage is recorded");
            assert_eq!(usage.total_token_usage.total_tokens, 7);
            assert_eq!(usage.last_token_usage.total_tokens, 7);
            assert_eq!(
                session.clone_history().await.into_raw_items(),
                history_before
            );

            let request = mock.single_request();
            let body = request.body_json();
            assert_eq!(body["tools"], json!([]));
            assert_eq!(body["parallel_tool_calls"], json!(false));
            assert_eq!(request.message_input_texts("user"), ["human native task"]);
            assert!(!request.body_contains_text("ordinary history secret"));
            assert!(request.instructions_text().contains(SDK_CONTRACT));
            assert!(
                request
                    .instructions_text()
                    .contains("follow the local AGENTS instruction")
            );
            assert_eq!(body["text"]["format"]["type"], json!("json_schema"));
            assert_eq!(body["text"]["format"]["strict"], json!(true));
            assert_eq!(
                body["text"]["format"]["schema"]["additionalProperties"],
                json!(false)
            );
            let metadata: serde_json::Value = serde_json::from_str(
                body["client_metadata"]["x-codex-turn-metadata"]
                    .as_str()
                    .expect("canonical metadata"),
            )
            .expect("metadata JSON");
            assert_eq!(
                metadata["request_kind"],
                json!("native_code_mode_generation")
            );
        }
    }

    #[tokio::test]
    async fn mocked_official_responses_performs_exactly_one_attributed_repair() {
        let server = responses::start_mock_server().await;
        let mock = responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse(vec![
                    responses::ev_assistant_message(
                        "native-source-1",
                        r#"{"source":"fn attempt_one() {}"}"#,
                    ),
                    responses::ev_completed_with_tokens("native-response-1", 11),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message(
                        "native-source-2",
                        r#"{"source":"fn attempt_two() {}"}"#,
                    ),
                    responses::ev_completed_with_tokens("native-response-2", 17),
                ]),
            ],
        )
        .await;
        let base_url = format!("{}/v1", server.uri());
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
                CodexAuth::from_api_key("test-api-key"),
                Vec::new(),
                |config| {
                    config.model_provider.base_url = Some(base_url);
                    config.model_provider.supports_websockets = false;
                },
            )
            .await;
        let history_before = session.clone_history().await.into_raw_items();
        let generator = ResponsesSourceGenerator {
            session: Arc::clone(&session),
            turn,
            applicable_instructions: "repair test instruction".to_string(),
        };
        let identity = identity();
        let executor = FakeExecutor::new([
            Ok(compile_failure(&identity, 1)),
            Ok(compiled(&identity, 2)),
        ]);
        let evidence = orchestrate(
            &generator,
            &executor,
            "repair the real Rust source",
            &identity,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(evidence.summary, "completed attempt 2");
        assert_eq!(*executor.attempts.lock().expect("attempt lock"), [1, 2]);
        assert_eq!(mock.requests().len(), 2);
        let requests = mock.requests();
        for request in &requests {
            let body = request.body_json();
            assert_eq!(body["tools"], json!([]));
            assert_eq!(body["parallel_tool_calls"], json!(false));
            assert_eq!(body["text"]["format"]["strict"], json!(true));
        }
        let request_kind = |request: &core_test_support::responses::ResponsesRequest| {
            let body = request.body_json();
            let metadata: serde_json::Value = serde_json::from_str(
                body["client_metadata"]["x-codex-turn-metadata"]
                    .as_str()
                    .expect("canonical metadata"),
            )
            .expect("metadata JSON");
            metadata["request_kind"].as_str().unwrap().to_string()
        };
        assert_eq!(request_kind(&requests[0]), "native_code_mode_generation");
        assert_eq!(request_kind(&requests[1]), "native_code_mode_repair");
        let repair_input = requests[1].body_json()["input"].to_string();
        assert!(repair_input.contains("repair the real Rust source"));
        assert!(repair_input.contains("fn attempt_one() {}"));
        assert!(repair_input.contains("error: expected expression"));
        assert!(repair_input.contains(&format!("{:064x}", 1)));
        let usage = session
            .token_usage_info()
            .await
            .expect("generation and repair usage accumulate");
        assert_eq!(usage.total_token_usage.total_tokens, 28);
        assert_eq!(usage.last_token_usage.total_tokens, 17);
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );
    }

    #[tokio::test]
    async fn responses_usage_budget_error_is_bounded_and_never_records_raw_items() {
        let server = responses::start_mock_server().await;
        let _mock = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                responses::ev_assistant_message("native-source", r#"{"source":"fn main() {}"}"#),
                responses::ev_completed_with_tokens("native-response", 7),
            ]),
        )
        .await;
        let base_url = format!("{}/v1", server.uri());
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
                CodexAuth::from_api_key("test-api-key"),
                Vec::new(),
                |config| {
                    config.model_provider.base_url = Some(base_url);
                    config.model_provider.supports_websockets = false;
                },
            )
            .await;
        session.services.agent_control.rollout_budget().configure(
            crate::config::RolloutBudgetConfig {
                limit_tokens: 1,
                reminder_at_remaining_tokens: Vec::new(),
                sampling_token_weight: 1.0,
                prefill_token_weight: 1.0,
            },
        );
        let history_before = session.clone_history().await.into_raw_items();
        let generator = ResponsesSourceGenerator {
            session: Arc::clone(&session),
            turn,
            applicable_instructions: String::new(),
        };
        let error = generator
            .generate_inner(
                GenerationRequest {
                    kind: GenerationKind::Initial,
                    task: "bounded budget test",
                    original_source: None,
                    source_hash: None,
                    diagnostic: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect_err("exhausted rollout budget terminates generation");
        assert!(error.contains("usage accounting failed"));
        assert!(error.len() <= EVIDENCE_STRING_BYTES);
        assert_eq!(
            session
                .token_usage_info()
                .await
                .expect("usage is still accounted")
                .total_token_usage
                .total_tokens,
            7
        );
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );
    }

    #[tokio::test]
    async fn completed_usage_is_charged_even_when_structured_output_is_invalid() {
        let server = responses::start_mock_server().await;
        let _mock = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                responses::ev_assistant_message(
                    "native-source",
                    r#"{"source":"fn main() {}","extra":true}"#,
                ),
                responses::ev_completed_with_tokens("native-response", 9),
            ]),
        )
        .await;
        let base_url = format!("{}/v1", server.uri());
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
                CodexAuth::from_api_key("test-api-key"),
                Vec::new(),
                |config| {
                    config.model_provider.base_url = Some(base_url);
                    config.model_provider.supports_websockets = false;
                },
            )
            .await;
        let history_before = session.clone_history().await.into_raw_items();
        let generator = ResponsesSourceGenerator {
            session: Arc::clone(&session),
            turn,
            applicable_instructions: String::new(),
        };
        assert!(
            generator
                .generate_inner(
                    GenerationRequest {
                        kind: GenerationKind::Initial,
                        task: "invalid structured output accounting",
                        original_source: None,
                        source_hash: None,
                        diagnostic: None,
                    },
                    CancellationToken::new(),
                )
                .await
                .expect_err("extra structured field remains terminal")
                .contains("invalid source JSON")
        );
        assert_eq!(
            session
                .token_usage_info()
                .await
                .expect("completed usage is charged")
                .total_token_usage
                .total_tokens,
            9
        );
        assert_eq!(
            session.clone_history().await.into_raw_items(),
            history_before
        );
    }

    #[tokio::test]
    async fn source_collector_accepts_hidden_reasoning_and_rejects_extra_or_invalid_output() {
        let valid = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: r#"{"source":"fn main() {}"}"#.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let reasoning = ResponseItem::Reasoning {
            id: None,
            summary: Vec::new(),
            content: Some(vec![ReasoningItemContent::ReasoningText {
                text: "hidden".to_string(),
            }]),
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let mut usage = None;
        let accepted = collect_source(
            response_stream(vec![
                Ok(codex_api::ResponseEvent::OutputItemDone(reasoning)),
                Ok(codex_api::ResponseEvent::OutputItemDone(valid.clone())),
                Ok(completed_event()),
            ]),
            CancellationToken::new(),
            &mut usage,
        )
        .await
        .expect("hidden reasoning plus one source is accepted");
        assert_eq!(accepted, "fn main() {}");
        assert!(usage.is_none());

        let mut usage = None;
        let multiple = collect_source(
            response_stream(vec![
                Ok(codex_api::ResponseEvent::OutputItemDone(valid.clone())),
                Ok(codex_api::ResponseEvent::OutputItemDone(valid.clone())),
                Ok(completed_event()),
            ]),
            CancellationToken::new(),
            &mut usage,
        )
        .await
        .expect_err("multiple assistant messages are rejected");
        assert!(multiple.contains("multiple assistant"));

        let tool = ResponseItem::FunctionCall {
            id: None,
            name: "shell_command".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            encrypted_function_args: None,
            call_id: "forbidden".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };
        let mut usage = None;
        let tool_error = collect_source(
            response_stream(vec![
                Ok(codex_api::ResponseEvent::OutputItemDone(tool)),
                Ok(completed_event()),
            ]),
            CancellationToken::new(),
            &mut usage,
        )
        .await
        .expect_err("tool output is rejected");
        assert!(tool_error.contains("forbidden extra output"));

        let malformed = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: r#"{"source":"ok","extra":true}"#.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let mut usage = None;
        assert!(
            collect_source(
                response_stream(vec![
                    Ok(codex_api::ResponseEvent::OutputItemDone(malformed)),
                    Ok(completed_event()),
                ]),
                CancellationToken::new(),
                &mut usage,
            )
            .await
            .expect_err("extra structured field is rejected")
            .contains("invalid source JSON")
        );

        let oversized = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: serde_json::to_string(&json!({ "source": "x".repeat(SOURCE_BYTES + 1) }))
                    .expect("oversize source JSON"),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let mut usage = None;
        assert!(
            collect_source(
                response_stream(vec![
                    Ok(codex_api::ResponseEvent::OutputItemDone(oversized)),
                    Ok(completed_event()),
                ]),
                CancellationToken::new(),
                &mut usage,
            )
            .await
            .expect_err("oversize source is rejected")
            .contains("exceeds")
        );

        let mut usage = None;
        assert!(
            collect_source(
                response_stream(vec![Err(codex_protocol::error::CodexErr::Stream(
                    "Incomplete response returned".to_string(),
                ))]),
                CancellationToken::new(),
                &mut usage,
            )
            .await
            .expect_err("incomplete response is terminal")
            .contains("stream failed")
        );
    }

    fn completed_event() -> codex_api::ResponseEvent {
        codex_api::ResponseEvent::Completed {
            response_id: "response".to_string(),
            token_usage: None,
            end_turn: Some(true),
        }
    }

    fn response_stream(
        events: Vec<codex_protocol::error::Result<codex_api::ResponseEvent>>,
    ) -> crate::ResponseStream {
        let (tx, rx) = tokio::sync::mpsc::channel(events.len().max(1));
        for event in events {
            tx.try_send(event).expect("test response channel capacity");
        }
        drop(tx);
        crate::ResponseStream {
            rx_event: rx,
            consumer_dropped: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn responses_wait_is_bounded_and_cancellable_without_polling() {
        let timed_out = await_bounded_generation(
            std::future::pending(),
            CancellationToken::new(),
            Duration::from_millis(10),
            "native compiler repair",
        )
        .await
        .expect_err("pending repair response must time out");
        assert_eq!(
            timed_out,
            "native compiler repair timed out after 0 seconds"
        );

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled = await_bounded_generation(
            std::future::pending(),
            cancellation,
            Duration::from_secs(30),
            "native source generation",
        )
        .await
        .expect_err("cancelled generation must settle");
        assert_eq!(cancelled, "native source generation cancelled");
        assert_eq!(
            responses_timeout(GenerationKind::Initial),
            Duration::from_secs(90)
        );
        assert_eq!(
            responses_timeout(GenerationKind::Repair),
            Duration::from_secs(60)
        );
    }

    #[tokio::test]
    async fn native_session_task_cancels_generation_records_one_evidence_and_resets() {
        let server = responses::start_mock_server().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/responses$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_delay(std::time::Duration::from_secs(30)),
            )
            .mount(&server)
            .await;
        let base_url = format!("{}/v1", server.uri());
        let (mut session, turn, events) =
            crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
                CodexAuth::from_api_key("test-api-key"),
                Vec::new(),
                |config| {
                    config.model_provider.base_url = Some(base_url);
                    config.model_provider.supports_websockets = false;
                },
            )
            .await;
        let seed = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "existing history".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        session
            .record_conversation_items(turn.as_ref(), std::slice::from_ref(&seed))
            .await;
        let history_before = session.clone_history().await.into_raw_items();
        let provider = Arc::new(
            codex_code_mode::ProcessOwnedCodeModeSessionProvider::with_host_program(PathBuf::from(
                "/definitely/not/a/code-mode-host",
            )),
        );
        let features = session.features().clone();
        Arc::get_mut(&mut session)
            .expect("test session is uniquely owned")
            .services
            .code_mode_service = crate::tools::code_mode::CodeModeService::new_with_native_client(
            Arc::clone(&provider) as Arc<dyn codex_code_mode::CodeModeSessionProvider>,
            Some(provider.native_client()),
            &features,
        );

        session
            .start_native_code_mode_task("task text must stay out of history".to_string())
            .await
            .expect("native task starts");
        let duplicate = session
            .start_native_code_mode_task("must not replace active native work".to_string())
            .await;
        let duplicate = duplicate.expect_err("active native task cannot be replaced");
        assert!(
            duplicate
                .to_string()
                .contains("only while the thread is idle")
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if server
                    .received_requests()
                    .await
                    .is_some_and(|requests| !requests.is_empty())
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("mocked generation request must begin");
        let steer = session
            .steer_input(
                vec![UserInput::Text {
                    text: "cannot steer native task".to_string(),
                    text_elements: Vec::new(),
                }],
                BTreeMap::new(),
                None,
                None,
                None,
            )
            .await;
        assert!(matches!(
            steer,
            Err(crate::session::SteerInputError::NativeCodeModeNotSteerable)
        ));
        let started = std::time::Instant::now();
        session
            .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
            .await;
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(session.active_turn.lock().await.is_none());
        assert!(session.list_background_terminals().await.is_empty());

        let history = session.clone_history().await.into_raw_items();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0], history_before[0]);
        let encoded = serde_json::to_string(&history[1]).expect("terminal item serializes");
        assert!(encoded.contains("Native Rust Code Mode interrupted"));
        assert!(encoded.contains("native source generation cancelled"));
        assert!(!encoded.contains("task text must stay out of history"));
        let lifecycle = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event.msg {
                EventMsg::ItemCompleted(completed) => match completed.item {
                    TurnItem::NativeCodeMode(item) => Some(item.phase),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(lifecycle.contains(&NativeCodeModePhase::Generating));
        assert!(!lifecycle.contains(&NativeCodeModePhase::Compiling));

        // Starting another task is possible immediately: no mode bit or retained task owner needs
        // resetting. Abort it at the same mocked generation boundary to keep the test bounded.
        session
            .start_native_code_mode_task("second isolated task".to_string())
            .await
            .expect("second native task starts after reset");
        assert!(session.active_turn.lock().await.is_some());
        session
            .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
            .await;
        assert!(session.active_turn.lock().await.is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mocked_generation_runs_real_adjacent_host_shell_patch_and_inserts_only_evidence() {
        let Some(host_program) = std::env::var_os("YCODE_TEST_CODE_MODE_HOST") else {
            eprintln!("skipping Phase III adjacent-host proof without YCODE_TEST_CODE_MODE_HOST");
            return;
        };
        let workspace = tempfile::tempdir().expect("temp workspace");
        let codex_home = tempfile::tempdir().expect("temp ycode home");
        let _home = EnvGuard::set("CODEX_HOME", codex_home.path().as_os_str());
        let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
            workspace.path().to_path_buf(),
        )
        .expect("absolute workspace");
        let source = real_tool_fixture(workspace.path());
        let server = responses::start_mock_server().await;
        let response = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                responses::ev_assistant_message(
                    "native-source",
                    &serde_json::to_string(&json!({ "source": source }))
                        .expect("fixture source JSON"),
                ),
                responses::ev_completed("native-response"),
            ]),
        )
        .await;
        let base_url = format!("{}/v1", server.uri());
        let (mut session, _turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                CodexAuth::from_api_key("test-api-key"),
                Vec::new(),
                codex_home.path(),
                |config| {
                    config.cwd = cwd;
                    config.model_provider.base_url = Some(base_url);
                    config.model_provider.supports_websockets = false;
                },
            )
            .await;
        let provider = Arc::new(
            codex_code_mode::ProcessOwnedCodeModeSessionProvider::with_host_program(PathBuf::from(
                host_program,
            )),
        );
        let features = session.features().clone();
        Arc::get_mut(&mut session)
            .expect("test session is uniquely owned")
            .services
            .code_mode_service = crate::tools::code_mode::CodeModeService::new_with_native_client(
            Arc::clone(&provider) as Arc<dyn codex_code_mode::CodeModeSessionProvider>,
            Some(provider.native_client()),
            &features,
        );
        let history_before = session.clone_history().await.into_raw_items();
        session
            .start_native_code_mode_task(
                "create and patch the deterministic proof file".to_string(),
            )
            .await
            .expect("native task starts");
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if session.active_turn.lock().await.is_none() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("native SessionTask must settle");
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("phase-three-proof.txt"))
                .expect("proof file"),
            "seed\npatched\n"
        );
        let history = session.clone_history().await.into_raw_items();
        assert_eq!(history.len(), history_before.len() + 1);
        assert_eq!(&history[..history_before.len()], history_before.as_slice());
        let text = serde_json::to_string(history.last().expect("one Evidence item"))
            .expect("Evidence history serializes");
        assert!(text.contains("Phase III real tools completed"));
        let ResponseItem::Message { content, .. } = history.last().expect("one Evidence item")
        else {
            panic!("expected one assistant Evidence message")
        };
        let [
            ContentItem::OutputText {
                text: evidence_json,
            },
        ] = content.as_slice()
        else {
            panic!("expected one Evidence output text")
        };
        let evidence: NativeEvidence =
            serde_json::from_str(evidence_json).expect("bounded Evidence JSON");
        assert_eq!(evidence.provenance_ids.len(), 2);
        assert_eq!(evidence.artifact_refs.len(), 6);
        assert!(
            evidence
                .verified
                .iter()
                .any(|finding| finding.contains("phase-three-proof.txt"))
        );
        assert!(
            evidence
                .artifact_refs
                .iter()
                .all(|reference| !reference.ends_with("/phase-three-proof.txt"))
        );
        for provenance in &evidence.provenance_ids {
            assert!(
                evidence
                    .artifact_refs
                    .iter()
                    .any(|reference| reference
                        .ends_with(&format!("/calls/{provenance}.request.bin")))
            );
            assert!(
                evidence.artifact_refs.iter().any(
                    |reference| reference.ends_with(&format!("/calls/{provenance}.result.bin"))
                )
            );
        }
        assert!(!text.contains("printf"));
        assert!(!text.contains("*** Begin Patch"));
        assert!(!text.contains("create and patch the deterministic proof file"));
        assert_eq!(response.requests().len(), 1);
        assert!(session.list_background_terminals().await.is_empty());
        drop(provider);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn session_task_charges_generation_and_repair_then_inserts_one_evidence() {
        let Some(host_program) = std::env::var_os("YCODE_TEST_CODE_MODE_HOST") else {
            eprintln!("skipping Phase III repair/accounting proof without host binary");
            return;
        };
        let workspace = tempfile::tempdir().expect("temp workspace");
        let codex_home = tempfile::tempdir().expect("temp ycode home");
        let _home = EnvGuard::set("CODEX_HOME", codex_home.path().as_os_str());
        let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
            workspace.path().to_path_buf(),
        )
        .expect("absolute workspace");
        let repaired_source = real_tool_fixture(workspace.path());
        let server = responses::start_mock_server().await;
        let response = responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse(vec![
                    responses::ev_assistant_message(
                        "native-source-1",
                        r#"{"source":"use ycode_native_sdk as sdk; fn main( {"}"#,
                    ),
                    responses::ev_completed_with_tokens("native-response-1", 11),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message(
                        "native-source-2",
                        &serde_json::to_string(&json!({ "source": repaired_source }))
                            .expect("repaired source JSON"),
                    ),
                    responses::ev_completed_with_tokens("native-response-2", 17),
                ]),
            ],
        )
        .await;
        let base_url = format!("{}/v1", server.uri());
        let (mut session, _turn, events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                CodexAuth::from_api_key("test-api-key"),
                Vec::new(),
                codex_home.path(),
                |config| {
                    config.cwd = cwd;
                    config.model_provider.base_url = Some(base_url);
                    config.model_provider.supports_websockets = false;
                },
            )
            .await;
        let provider = Arc::new(
            codex_code_mode::ProcessOwnedCodeModeSessionProvider::with_host_program(PathBuf::from(
                host_program,
            )),
        );
        let features = session.features().clone();
        Arc::get_mut(&mut session)
            .expect("test session is uniquely owned")
            .services
            .code_mode_service = crate::tools::code_mode::CodeModeService::new_with_native_client(
            Arc::clone(&provider) as Arc<dyn codex_code_mode::CodeModeSessionProvider>,
            Some(provider.native_client()),
            &features,
        );
        let history_before = session.clone_history().await.into_raw_items();
        session
            .start_native_code_mode_task("repair and run the native fixture".to_string())
            .await
            .expect("native repair task starts");
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if session.active_turn.lock().await.is_none() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("repaired native SessionTask settles");
        let observed_history = session.clone_history().await.into_raw_items();
        assert_eq!(
            response.requests().len(),
            2,
            "terminal history: {:?}",
            observed_history.last()
        );
        let usage = session
            .token_usage_info()
            .await
            .expect("both Responses calls are accounted");
        assert_eq!(usage.total_token_usage.total_tokens, 28);
        assert_eq!(usage.last_token_usage.total_tokens, 17);
        let history = observed_history;
        assert_eq!(history.len(), history_before.len() + 1);
        assert_eq!(&history[..history_before.len()], history_before.as_slice());
        let terminal = serde_json::to_string(history.last().expect("one terminal Evidence"))
            .expect("terminal Evidence serializes");
        assert!(terminal.contains("Phase III real tools completed"));
        assert!(!terminal.contains("repair and run the native fixture"));
        assert!(!terminal.contains("fn main"));
        assert!(!terminal.contains("expected"));
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("phase-three-proof.txt"))
                .expect("repaired fixture output"),
            "seed\npatched\n"
        );
        let mut recorded_events = Vec::new();
        while let Ok(event) = events.try_recv() {
            recorded_events.push(event);
        }
        let events = recorded_events;
        let phase_positions = |phase| {
            events
                .iter()
                .enumerate()
                .filter_map(|(index, event)| match &event.msg {
                    EventMsg::ItemCompleted(completed)
                        if matches!(
                            &completed.item,
                            TurnItem::NativeCodeMode(NativeCodeModeItem {
                                phase: observed,
                                ..
                            }) if *observed == phase
                        ) =>
                    {
                        Some(index)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let generating_positions = phase_positions(NativeCodeModePhase::Generating);
        let compiling_positions = phase_positions(NativeCodeModePhase::Compiling);
        let repairing_positions = phase_positions(NativeCodeModePhase::Repairing);
        let running_positions = phase_positions(NativeCodeModePhase::Running);
        let succeeded_positions = phase_positions(NativeCodeModePhase::Succeeded);
        assert_eq!(generating_positions.len(), 1);
        assert_eq!(compiling_positions.len(), 2);
        assert_eq!(repairing_positions.len(), 1);
        assert_eq!(running_positions.len(), 1);
        assert_eq!(succeeded_positions.len(), 1);
        let repair_positions = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| match &event.msg {
                EventMsg::ItemCompleted(completed)
                    if matches!(
                        &completed.item,
                        TurnItem::NativeCodeMode(NativeCodeModeItem {
                            phase: NativeCodeModePhase::Repair,
                            ..
                        })
                    ) =>
                {
                    Some(index)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            repair_positions.len(),
            1,
            "repair lifecycle emits exactly once"
        );
        let repair_index = repair_positions[0];
        let attempt_two_tool_index = events
            .iter()
            .position(|event| {
                matches!(
                    &event.msg,
                    EventMsg::ItemStarted(started)
                        if matches!(
                            &started.item,
                            TurnItem::CommandExecution(_) | TurnItem::FileChange(_)
                        )
                )
            })
            .expect("attempt-two real tool lifecycle starts");
        let artifact_index = events
            .iter()
            .position(|event| {
                matches!(
                    &event.msg,
                    EventMsg::ItemCompleted(completed)
                        if matches!(
                            &completed.item,
                            TurnItem::NativeCodeMode(NativeCodeModeItem {
                                phase: NativeCodeModePhase::Artifact,
                                ..
                            })
                        )
                )
            })
            .expect("verified artifact lifecycle settles");
        let evidence_index = events
            .iter()
            .rposition(|event| {
                matches!(
                    &event.msg,
                    EventMsg::ItemCompleted(completed)
                        if matches!(&completed.item, TurnItem::AgentMessage(_))
                )
            })
            .expect("terminal Evidence lifecycle settles");
        assert!(generating_positions[0] < compiling_positions[0]);
        assert!(compiling_positions[0] < repairing_positions[0]);
        assert!(repairing_positions[0] < repair_index);
        assert!(repair_index < compiling_positions[1]);
        assert!(compiling_positions[1] < running_positions[0]);
        assert!(running_positions[0] < attempt_two_tool_index);
        assert!(repair_index < attempt_two_tool_index);
        assert!(attempt_two_tool_index < artifact_index);
        assert!(artifact_index < succeeded_positions[0]);
        assert!(succeeded_positions[0] < evidence_index);
        drop(provider);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_session_task_cancellation_reaps_real_shell_and_resets() {
        let Some(host_program) = std::env::var_os("YCODE_TEST_CODE_MODE_HOST") else {
            eprintln!("skipping Phase III cancellation proof without YCODE_TEST_CODE_MODE_HOST");
            return;
        };
        let workspace = tempfile::tempdir().expect("temp workspace");
        let codex_home = tempfile::tempdir().expect("temp ycode home");
        let _home = EnvGuard::set("CODEX_HOME", codex_home.path().as_os_str());
        let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
            workspace.path().to_path_buf(),
        )
        .expect("absolute workspace");
        let source = real_cancellation_fixture(workspace.path());
        let server = responses::start_mock_server().await;
        let _response = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                responses::ev_assistant_message(
                    "native-source",
                    &serde_json::to_string(&json!({ "source": source }))
                        .expect("fixture source JSON"),
                ),
                responses::ev_completed("native-response"),
            ]),
        )
        .await;
        let base_url = format!("{}/v1", server.uri());
        let (mut session, _turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                CodexAuth::from_api_key("test-api-key"),
                Vec::new(),
                codex_home.path(),
                |config| {
                    config.cwd = cwd;
                    config.model_provider.base_url = Some(base_url);
                    config.model_provider.supports_websockets = false;
                },
            )
            .await;
        let provider = Arc::new(
            codex_code_mode::ProcessOwnedCodeModeSessionProvider::with_host_program(PathBuf::from(
                host_program,
            )),
        );
        let features = session.features().clone();
        Arc::get_mut(&mut session)
            .expect("test session is uniquely owned")
            .services
            .code_mode_service = crate::tools::code_mode::CodeModeService::new_with_native_client(
            Arc::clone(&provider) as Arc<dyn codex_code_mode::CodeModeSessionProvider>,
            Some(provider.native_client()),
            &features,
        );
        let history_before = session.clone_history().await.into_raw_items();
        session
            .start_native_code_mode_task("start the cancellable native workflow".to_string())
            .await
            .expect("cancellable native task starts");
        let pid_path = workspace.path().join("phase-three-cancel.pid");
        let pid = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pid_path)
                    && let Ok(pid) = text.trim().parse::<u32>()
                    && process_exists(pid)
                {
                    break pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("real shell child must become observable before cancellation");

        let started = std::time::Instant::now();
        session
            .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
            .await;
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(1));
        assert!(!process_exists(pid), "real shell PID {pid} survived abort");
        assert!(session.active_turn.lock().await.is_none());
        assert!(session.list_background_terminals().await.is_empty());
        let history = session.clone_history().await.into_raw_items();
        assert_eq!(history.len(), history_before.len() + 1);
        assert_eq!(&history[..history_before.len()], history_before.as_slice());
        let terminal = serde_json::to_string(history.last().expect("terminal Evidence item"))
            .expect("terminal Evidence serializes");
        assert!(terminal.contains("Native Rust workflow"), "{terminal}");
        assert!(!terminal.contains("start the cancellable native workflow"));
        eprintln!(
            "Phase III SessionTask real-shell request-to-reaped-zero latency ms={:.3}",
            elapsed.as_secs_f64() * 1_000.0
        );
        drop(provider);
    }

    fn process_exists(pid: u32) -> bool {
        std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn real_tool_fixture(workspace: &std::path::Path) -> String {
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
    let shell = context.call(Request::Shell {{
        command: "printf 'seed\n' > phase-three-proof.txt".to_string(),
        workdir: Some({workdir:?}.to_string()),
        timeout_ms: 5_000,
    }})?;
    let shell_id = match shell {{
        Outcome::Success {{ call_id, .. }} => call_id,
        Outcome::Retry {{ reason, .. }} => return Err(sdk::Error::Host(reason)),
        Outcome::Failure {{ message, .. }} => return Err(sdk::Error::Host(message)),
    }};
    let patch = context.call(Request::ApplyPatch {{
        patch: "*** Begin Patch\n*** Update File: phase-three-proof.txt\n@@\n seed\n+patched\n*** End Patch".to_string(),
    }})?;
    let patch_id = match patch {{
        Outcome::Success {{ call_id, .. }} => call_id,
        Outcome::Retry {{ reason, .. }} => return Err(sdk::Error::Host(reason)),
        Outcome::Failure {{ message, .. }} => return Err(sdk::Error::Host(message)),
    }};
    context.finish(Evidence {{
        version: 1,
        summary: "Phase III real tools completed".to_string(),
        verified: vec!["phase-three-proof.txt contains seed and patched".to_string()],
        disputed: vec![], unresolved: vec![],
        artifact_refs: vec![],
        partial_failures: vec![], provenance_ids: vec![shell_id, patch_id],
    }})
}}
"#
        )
    }

    fn real_cancellation_fixture(workspace: &std::path::Path) -> String {
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
        command: "echo $$ > phase-three-cancel.pid; exec sleep 30".to_string(),
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
