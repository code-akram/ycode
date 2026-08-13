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
use codex_tools::ToolName;
use serde_json::json;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolPayload;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolCallSource;

const NATIVE_TOTAL_CALLS: usize = 32;
const NATIVE_CONCURRENT_CALLS: usize = 4;
const NATIVE_CALL_BYTES: usize = 64 * 1024;
const NATIVE_TOTAL_ARTIFACT_BYTES: usize = 1024 * 1024;
const NATIVE_WORKFLOW_TIMEOUT: Duration = Duration::from_secs(30);

/// One run-scoped bridge from native delegate callbacks into the canonical tool router.
pub(crate) struct NativeCodeModeDispatchWorker {
    identity: NativeRunIdentity,
    attempt: u8,
    tool_runtime: ToolCallRuntime,
    cancellation: CancellationToken,
    deadline: Instant,
    permits: Arc<Semaphore>,
    total_calls: AtomicUsize,
    next_runtime_call: AtomicUsize,
    active_calls: AtomicUsize,
    artifact_bytes: AtomicUsize,
    seen_runtime_calls: Mutex<HashSet<String>>,
}

impl NativeCodeModeDispatchWorker {
    pub(crate) fn new(
        identity: NativeRunIdentity,
        attempt: u8,
        session: Arc<Session>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
        cancellation: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity,
            attempt,
            tool_runtime: ToolCallRuntime::new(session, step_context, tracker),
            cancellation,
            deadline: Instant::now() + NATIVE_WORKFLOW_TIMEOUT,
            permits: Arc::new(Semaphore::new(NATIVE_CONCURRENT_CALLS)),
            total_calls: AtomicUsize::new(0),
            next_runtime_call: AtomicUsize::new(1),
            active_calls: AtomicUsize::new(0),
            artifact_bytes: AtomicUsize::new(0),
            seen_runtime_calls: Mutex::new(HashSet::new()),
        })
    }

    #[cfg(test)]
    pub(crate) fn owned_counts(&self) -> (usize, usize) {
        (
            self.active_calls.load(Ordering::Acquire),
            NATIVE_CONCURRENT_CALLS.saturating_sub(self.permits.available_permits()),
        )
    }

    async fn invoke_inner(
        &self,
        invocation: NativeToolInvocation,
        delegate_cancellation: CancellationToken,
    ) -> Result<NativeToolOutcome, String> {
        self.validate_invocation(&invocation)?;
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
        let _active = ActiveCall { worker: self };
        let runtime_call_id = invocation.runtime_call_id;
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
        match result {
            Ok(result) => {
                let value = result.code_mode_result();
                let output = serde_json::to_vec(&value).map_err(|error| {
                    bounded_error(format!("failed to encode tool result: {error}"))
                })?;
                if output.len() > NATIVE_CALL_BYTES || !self.reserve_artifact_bytes(output.len()) {
                    return Ok(NativeToolOutcome::Failure {
                        message: "native tool result exceeded its bounded artifact budget"
                            .to_string(),
                    });
                }
                // The canonical complete-result shell handler exposes its Code Mode result as
                // a bounded human-readable string beginning with the exit status. Keep the
                // generated SDK outcome typed without bypassing or duplicating that handler.
                if is_shell
                    && !value
                        .as_str()
                        .is_some_and(|text| text.starts_with("Exit code: 0\n"))
                {
                    return Ok(NativeToolOutcome::Failure {
                        message: bounded_error(String::from_utf8_lossy(&output).into_owned()),
                    });
                }
                Ok(NativeToolOutcome::Success { output })
            }
            Err(error) => Ok(NativeToolOutcome::Failure {
                message: bounded_error(error.to_string()),
            }),
        }
    }

    fn validate_invocation(&self, invocation: &NativeToolInvocation) -> Result<(), String> {
        if invocation.identity != self.identity {
            self.cancellation.cancel();
            return Err("native delegate identity does not match its owner".to_string());
        }
        let call_id = invocation.runtime_call_id.as_str();
        let expected = self.next_runtime_call.fetch_add(1, Ordering::AcqRel);
        let expected = format!(
            "native-{}-a{}-{expected}",
            self.identity.run_id, self.attempt
        );
        if call_id != expected {
            self.cancellation.cancel();
            return Err(format!(
                "native runtime call ID is out of sequence: expected {expected}"
            ));
        }
        let mut seen = self
            .seen_runtime_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !seen.insert(call_id.to_string()) {
            self.cancellation.cancel();
            return Err("duplicate native runtime call ID".to_string());
        }
        Ok(())
    }

    fn reserve_artifact_bytes(&self, bytes: usize) -> bool {
        self.artifact_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= NATIVE_TOTAL_ARTIFACT_BYTES)
            })
            .is_ok()
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
}

struct ActiveCall<'a> {
    worker: &'a NativeCodeModeDispatchWorker,
}

impl Drop for ActiveCall<'_> {
    fn drop(&mut self) {
        self.worker.active_calls.fetch_sub(1, Ordering::AcqRel);
    }
}

fn exact_handler_payload_bytes(payload: &ToolPayload) -> usize {
    match payload {
        ToolPayload::Function { arguments } => arguments.len(),
        ToolPayload::Custom { input } => input.len(),
        ToolPayload::ToolSearch { .. } => unreachable!("native Code Mode exposes no tool search"),
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
    use codex_tools::ShellCommandBackendConfig;
    use pretty_assertions::assert_eq;

    use super::*;
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
        let worker = test_worker(
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

        let rejected = worker
            .invoke_inner(
                NativeToolInvocation {
                    identity,
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
        let registry = ToolRegistry::from_tools([
            Arc::new(ShellCommandHandler::from(
                ShellCommandBackendConfig::Classic,
            )) as Arc<dyn crate::tools::registry::CoreToolRuntime>,
            Arc::new(ApplyPatchHandler::new(/*multi_environment*/ false))
                as Arc<dyn crate::tools::registry::CoreToolRuntime>,
        ]);
        let step = StepContext::for_test(Arc::clone(&turn))
            .with_tool_router_for_test(Arc::new(ToolRouter::from_parts(registry, Vec::new())));
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let identity = NativeRunIdentity {
            session_id: "native-session".to_string(),
            thread_id: "00000000-0000-4000-8000-000000000001".to_string(),
            run_id: "00000000-0000-4000-8000-000000000002".to_string(),
        };
        let cancellation = CancellationToken::new();
        let worker = NativeCodeModeDispatchWorker::new(
            identity.clone(),
            1,
            Arc::clone(&session),
            step,
            tracker,
            cancellation.clone(),
        );
        let provider =
            ProcessOwnedCodeModeSessionProvider::with_host_program(PathBuf::from(host_program));
        let client = provider.native_client();
        let source = fixture_source(workspace.path());
        let result = client
            .execute(
                NativeExecute {
                    identity: identity.clone(),
                    attempt: 1,
                    task: "create and patch one deterministic file".to_string(),
                    source,
                },
                worker.clone(),
                cancellation,
            )
            .await
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

    fn test_worker(
        identity: NativeRunIdentity,
        session: Arc<crate::session::session::Session>,
        turn: Arc<crate::session::turn_context::TurnContext>,
        cancellation: CancellationToken,
    ) -> Arc<NativeCodeModeDispatchWorker> {
        let registry = ToolRegistry::from_tools([
            Arc::new(ShellCommandHandler::from(
                ShellCommandBackendConfig::Classic,
            )) as Arc<dyn crate::tools::registry::CoreToolRuntime>,
            Arc::new(ApplyPatchHandler::new(/*multi_environment*/ false))
                as Arc<dyn crate::tools::registry::CoreToolRuntime>,
        ]);
        let step = StepContext::for_test(turn)
            .with_tool_router_for_test(Arc::new(ToolRouter::from_parts(registry, Vec::new())));
        NativeCodeModeDispatchWorker::new(
            identity,
            1,
            session,
            step,
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            cancellation,
        )
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
    let shell = context.call(Request::Shell {{
        command: "printf 'seed\n' > native-proof.txt".to_string(),
        workdir: Some({workdir:?}.to_string()),
        timeout_ms: 5_000,
    }})?;
    let shell_id = match shell {{
        Outcome::Success {{ call_id, .. }} => call_id,
        Outcome::Retry {{ reason, .. }} => return Err(sdk::Error::Host(reason)),
        Outcome::Failure {{ message, .. }} => return Err(sdk::Error::Host(message)),
    }};
    let patch = context.call(Request::ApplyPatch {{
        patch: "*** Begin Patch\n*** Update File: native-proof.txt\n@@\n seed\n+patched\n*** End Patch".to_string(),
    }})?;
    let patch_id = match patch {{
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
        artifact_refs: vec!["native-proof.txt".to_string()],
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
