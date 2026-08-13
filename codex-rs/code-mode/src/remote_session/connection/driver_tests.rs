use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeNestedToolCall;
use codex_code_mode_protocol::CodeModeSessionCellExecutionLimits;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::NotificationFuture;
use codex_code_mode_protocol::ToolInvocationFuture;
use codex_code_mode_protocol::WaitRequest;
use codex_code_mode_protocol::host::CapabilitySet;
use codex_code_mode_protocol::host::ClientToHost;
use codex_code_mode_protocol::host::DelegateRequest;
use codex_code_mode_protocol::host::DelegateRequestId;
use codex_code_mode_protocol::host::DelegateResponse;
use codex_code_mode_protocol::host::EncodedFrame;
use codex_code_mode_protocol::host::HostRequest;
use codex_code_mode_protocol::host::HostResponse;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::MAX_PENDING_DELEGATE_CALLS;
use codex_code_mode_protocol::host::NativeEvidence;
use codex_code_mode_protocol::host::NativeToolOutcome;
use codex_code_mode_protocol::host::NativeToolRequest;
use codex_code_mode_protocol::host::RequestId;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WireNestedToolCall;
use codex_code_mode_protocol::host::WireResult;
use codex_code_mode_protocol::host::WireRuntimeResponse;
use codex_code_mode_protocol::host::WireSessionCellExecutionLimits;
use codex_code_mode_protocol::host::WireWaitOutcome;
use codex_protocol::ToolName;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::super::Connection;
use super::super::DEFAULT_HOST_WAIT_TRANSPORT_TIMEOUT;
use super::ConnectionDriver;
use super::DriverCommand;
use super::DriverEvent;
use super::DriverLifecycle;
use super::RemoteSession;
use super::SessionCleanup;
use crate::NativeCodeModeDelegate;
use crate::NativeExecute;
use crate::NativeRunIdentity;
use crate::NativeToolFuture;
use crate::NativeToolInvocation;

struct NativeDelegate;

impl NativeCodeModeDelegate for NativeDelegate {
    fn invoke<'a>(
        &'a self,
        invocation: NativeToolInvocation,
        _cancellation: CancellationToken,
    ) -> NativeToolFuture<'a> {
        Box::pin(async move {
            Ok(NativeToolOutcome::Success {
                output: invocation.runtime_call_id.into_bytes(),
            })
        })
    }
}

struct DisconnectNativeDelegate {
    started: Arc<Notify>,
    cancelled: Arc<Notify>,
}

struct NonCooperativeNativeDelegate {
    started: Arc<Notify>,
    dropped: Arc<Notify>,
}

struct DropSignal(Arc<Notify>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

impl NativeCodeModeDelegate for NonCooperativeNativeDelegate {
    fn invoke<'a>(
        &'a self,
        _invocation: NativeToolInvocation,
        _cancellation: CancellationToken,
    ) -> NativeToolFuture<'a> {
        Box::pin(async move {
            let _dropped = DropSignal(Arc::clone(&self.dropped));
            self.started.notify_one();
            std::future::pending().await
        })
    }
}

impl NativeCodeModeDelegate for DisconnectNativeDelegate {
    fn invoke<'a>(
        &'a self,
        _invocation: NativeToolInvocation,
        cancellation: CancellationToken,
    ) -> NativeToolFuture<'a> {
        Box::pin(async move {
            self.started.notify_one();
            cancellation.cancelled().await;
            self.cancelled.notify_one();
            Err("client disconnected".to_string())
        })
    }
}

struct DriverHarness {
    command_tx: mpsc::Sender<DriverCommand>,
    event_tx: mpsc::Sender<DriverEvent>,
    execute_claim_tx: mpsc::UnboundedSender<RequestId>,
    outgoing_rx: mpsc::Receiver<codex_code_mode_protocol::host::EncodedFrame>,
    cancellation: CancellationToken,
    alive: Arc<AtomicBool>,
    failure: Arc<StdMutex<Option<String>>>,
    native_tasks: Arc<AtomicUsize>,
    driver_task: tokio::task::JoinHandle<()>,
}

impl DriverHarness {
    fn start() -> Self {
        let (command_tx, command_rx) = mpsc::channel(/*max_capacity*/ 16);
        let (event_tx, event_rx) = mpsc::channel(/*max_capacity*/ 16);
        let (outgoing_tx, outgoing_rx) = mpsc::channel(/*max_capacity*/ 16);
        let cancellation = CancellationToken::new();
        let alive = Arc::new(AtomicBool::new(true));
        let failure = Arc::new(StdMutex::new(None));
        let native_tasks = Arc::new(AtomicUsize::new(0));
        let (driver, execute_claim_tx) = ConnectionDriver::new(
            command_rx,
            event_rx,
            event_tx.clone(),
            outgoing_tx,
            DriverLifecycle {
                alive: Arc::clone(&alive),
                failure: Arc::clone(&failure),
                cancellation: cancellation.clone(),
                native_tasks: Arc::clone(&native_tasks),
            },
        );
        let driver_task = tokio::spawn(driver.run());
        Self {
            command_tx,
            event_tx,
            execute_claim_tx,
            outgoing_rx,
            cancellation,
            alive,
            failure,
            native_tasks,
            driver_task,
        }
    }

    async fn open(
        &mut self,
        session: RemoteSession,
        delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> SessionCleanup {
        let cleanup = SessionCleanup::new();
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(DriverCommand::OpenSession {
                session: session.clone(),
                delegate,
                limits: Default::default(),
                cleanup: cleanup.clone(),
                caller_cancellation: CancellationToken::new(),
                response_tx,
            })
            .await
            .expect("open command");
        self.outgoing_rx.recv().await.expect("open frame");
        self.event_tx
            .send(DriverEvent::HostMessage(HostToClient::Response {
                id: RequestId::new(/*value*/ 1),
                result: WireResult::Ok {
                    value: HostResponse::SessionReady {
                        session_id: session.id,
                    },
                },
            }))
            .await
            .expect("open response");
        response_rx
            .await
            .expect("open reply")
            .expect("open session");
        cleanup
    }

    async fn start_cell(
        &mut self,
        session: RemoteSession,
        request_id: i64,
        cell_id: &str,
    ) -> codex_code_mode_protocol::StartedCell {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(DriverCommand::Execute {
                session,
                request: ExecuteRequest {
                    tool_call_id: format!("call-{request_id}"),
                    enabled_tools: Vec::new(),
                    source: "await new Promise(() => {})".to_string(),
                    yield_time_ms: Some(1),
                    max_output_tokens: None,
                },
                caller_cancellation: CancellationToken::new(),
                response_tx,
            })
            .await
            .expect("execute command");
        self.outgoing_rx.recv().await.expect("execute frame");
        self.event_tx
            .send(DriverEvent::HostMessage(HostToClient::Response {
                id: RequestId::new(request_id),
                result: WireResult::Ok {
                    value: HostResponse::ExecutionStarted {
                        cell_id: CellId::new(cell_id.to_string()).into(),
                    },
                },
            }))
            .await
            .expect("execute response");
        let delivered = response_rx
            .await
            .expect("execute reply")
            .expect("execute session");
        self.execute_claim_tx
            .send(delivered.request_id)
            .expect("claim execute");
        delivered.started
    }

    async fn start_tool_delegate(&self, session: &RemoteSession, id: DelegateRequestId) {
        self.event_tx
            .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
                id,
                session_id: session.id.clone(),
                request: DelegateRequest::InvokeTool {
                    invocation: WireNestedToolCall {
                        cell_id: CellId::new("1".to_string()).into(),
                        runtime_tool_call_id: "tool-1".to_string(),
                        tool_name: ToolName::plain("slow").into(),
                        tool_kind: codex_code_mode_protocol::CodeModeToolKind::Function.into(),
                        input: None,
                    },
                },
            }))
            .await
            .expect("delegate request");
    }
}

impl Drop for DriverHarness {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[tokio::test]
async fn native_delegate_routes_only_to_exact_short_lived_owner() {
    let mut harness = DriverHarness::start();
    let identity = NativeRunIdentity {
        session_id: "native-session".to_string(),
        thread_id: "00000000-0000-4000-8000-000000000001".to_string(),
        run_id: "00000000-0000-4000-8000-000000000002".to_string(),
    };
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let (response_tx, response_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::NativeExecute {
            request: NativeExecute {
                identity: identity.clone(),
                attempt: 1,
                task: "task".to_string(),
                source: "fn main() {}".to_string(),
            },
            delegate: Arc::new(NativeDelegate),
            progress_tx: Some(progress_tx),
            caller_cancellation: CancellationToken::new(),
            response_tx,
        })
        .await
        .expect("native execute command");
    let execute = harness
        .outgoing_rx
        .recv()
        .await
        .expect("native execute frame");
    assert!(matches!(
        EncodedFrame::decode_framed::<ClientToHost>(&execute.into_framed_bytes())
            .expect("decode native execute"),
        ClientToHost::Request {
            request: HostRequest::NativeExecute { .. },
            ..
        }
    ));

    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::NativeProgress {
            id: RequestId::new(1),
            session_id: SessionId::new(identity.session_id.clone()).expect("session"),
            thread_id: identity.thread_id.clone(),
            run_id: identity.run_id.clone(),
            phase: codex_code_mode_protocol::host::NativeProgressPhase::WorkflowStarted,
        }))
        .await
        .expect("native workflow progress");
    assert_eq!(
        progress_rx.recv().await,
        Some(crate::native::NativeProgress::WorkflowStarted)
    );

    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
            id: DelegateRequestId::new(7),
            session_id: SessionId::new("wrong-session").expect("session"),
            request: DelegateRequest::NativeInvokeTool {
                run_id: identity.run_id.clone(),
                call_id: "call-1".to_string(),
                request: NativeToolRequest::ApplyPatch {
                    patch: "*** Begin Patch\n*** End Patch".to_string(),
                },
            },
        }))
        .await
        .expect("mismatched native delegate");
    assert_delegate_error(
        harness.outgoing_rx.recv().await.expect("mismatch response"),
        "unknown or mismatched native delegate target",
    );

    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
            id: DelegateRequestId::new(8),
            session_id: SessionId::new(identity.session_id.clone()).expect("session"),
            request: DelegateRequest::NativeInvokeTool {
                run_id: identity.run_id.clone(),
                call_id: "call-1".to_string(),
                request: NativeToolRequest::Shell {
                    command: "true".to_string(),
                    workdir: None,
                    timeout_ms: 1_000,
                },
            },
        }))
        .await
        .expect("native delegate");
    let result = harness.outgoing_rx.recv().await.expect("native result");
    let ClientToHost::DelegateResponse { id, result } =
        EncodedFrame::decode_framed::<ClientToHost>(&result.into_framed_bytes())
            .expect("decode native result")
    else {
        panic!("expected native delegate response")
    };
    assert_eq!(id, DelegateRequestId::new(8));
    assert_eq!(
        result.into_result().expect("native result"),
        DelegateResponse::NativeToolResult {
            outcome: NativeToolOutcome::Success {
                output: b"call-1".to_vec(),
            },
        }
    );

    for (id, expected) in [
        (8, "duplicate native delegate request ID"),
        (10, "duplicate native runtime call ID"),
    ] {
        harness
            .event_tx
            .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
                id: DelegateRequestId::new(id),
                session_id: SessionId::new(identity.session_id.clone()).expect("session"),
                request: DelegateRequest::NativeInvokeTool {
                    run_id: identity.run_id.clone(),
                    call_id: "call-1".to_string(),
                    request: NativeToolRequest::Shell {
                        command: "true".to_string(),
                        workdir: None,
                        timeout_ms: 1_000,
                    },
                },
            }))
            .await
            .expect("duplicate native delegate");
        assert_delegate_error(
            harness
                .outgoing_rx
                .recv()
                .await
                .expect("duplicate response"),
            expected,
        );
    }

    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(1),
            result: WireResult::Ok {
                value: HostResponse::NativeCompleted {
                    session_id: SessionId::new(identity.session_id.clone()).expect("session"),
                    thread_id: identity.thread_id.clone(),
                    run_id: identity.run_id.clone(),
                    source_hash: "a".repeat(64),
                    evidence: Box::new(NativeEvidence {
                        version: 1,
                        summary: "done".to_string(),
                        verified: Vec::new(),
                        disputed: Vec::new(),
                        unresolved: Vec::new(),
                        artifact_refs: Vec::new(),
                        partial_failures: Vec::new(),
                        provenance_ids: vec!["call-1".to_string()],
                    }),
                },
            },
        }))
        .await
        .expect("native completed response");
    response_rx
        .await
        .expect("native response channel")
        .expect("native completed result");

    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
            id: DelegateRequestId::new(9),
            session_id: SessionId::new(identity.session_id).expect("session"),
            request: DelegateRequest::NativeInvokeTool {
                run_id: identity.run_id,
                call_id: "call-2".to_string(),
                request: NativeToolRequest::ApplyPatch {
                    patch: "*** Begin Patch\n*** End Patch".to_string(),
                },
            },
        }))
        .await
        .expect("late native delegate");
    assert_delegate_error(
        harness.outgoing_rx.recv().await.expect("late response"),
        "unknown or mismatched native delegate target",
    );
}

fn assert_delegate_error(frame: EncodedFrame, expected: &str) {
    let ClientToHost::DelegateResponse { result, .. } =
        EncodedFrame::decode_framed::<ClientToHost>(&frame.into_framed_bytes())
            .expect("decode delegate error")
    else {
        panic!("expected delegate error")
    };
    assert!(result.into_result().unwrap_err().contains(expected));
}

#[tokio::test]
async fn native_client_disconnect_cancels_delegate_and_pending_request() {
    let mut harness = DriverHarness::start();
    let identity = NativeRunIdentity {
        session_id: "native-session".to_string(),
        thread_id: "00000000-0000-4000-8000-000000000001".to_string(),
        run_id: "00000000-0000-4000-8000-000000000003".to_string(),
    };
    let started = Arc::new(Notify::new());
    let cancelled = Arc::new(Notify::new());
    let (response_tx, response_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::NativeExecute {
            request: NativeExecute {
                identity: identity.clone(),
                attempt: 1,
                task: "disconnect".to_string(),
                source: "fn main() {}".to_string(),
            },
            delegate: Arc::new(DisconnectNativeDelegate {
                started: Arc::clone(&started),
                cancelled: Arc::clone(&cancelled),
            }),
            progress_tx: None,
            caller_cancellation: CancellationToken::new(),
            response_tx,
        })
        .await
        .expect("native execute command");
    harness.outgoing_rx.recv().await.expect("execute frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
            id: DelegateRequestId::new(7),
            session_id: SessionId::new(identity.session_id).expect("session"),
            request: DelegateRequest::NativeInvokeTool {
                run_id: identity.run_id,
                call_id: "call-1".to_string(),
                request: NativeToolRequest::Shell {
                    command: "sleep 30".to_string(),
                    workdir: None,
                    timeout_ms: 30_000,
                },
            },
        }))
        .await
        .expect("native delegate request");
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("delegate should start");
    harness
        .event_tx
        .send(DriverEvent::Failed("injected host disconnect".to_string()))
        .await
        .expect("disconnect event");
    tokio::time::timeout(Duration::from_secs(1), cancelled.notified())
        .await
        .expect("disconnect should cancel delegate");
    assert_eq!(
        response_rx
            .await
            .expect("native response channel")
            .unwrap_err(),
        "injected host disconnect"
    );
    tokio::time::timeout(Duration::from_secs(1), &mut harness.driver_task)
        .await
        .expect("driver should stop")
        .expect("driver join");
    assert!(!harness.alive.load(Ordering::Acquire));
    tokio::time::timeout(Duration::from_secs(1), async {
        while harness.native_tasks.load(Ordering::Acquire) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnect must leave zero native client tasks");
}

#[tokio::test]
async fn non_cooperative_native_delegate_is_forcibly_settled_on_cancel_and_unregister() {
    for settle_by_completion in [false, true] {
        let mut harness = DriverHarness::start();
        let identity = NativeRunIdentity {
            session_id: "native-session".to_string(),
            thread_id: "00000000-0000-4000-8000-000000000021".to_string(),
            run_id: if settle_by_completion {
                "00000000-0000-4000-8000-000000000022".to_string()
            } else {
                "00000000-0000-4000-8000-000000000023".to_string()
            },
        };
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        let (response_tx, response_rx) = oneshot::channel();
        harness
            .command_tx
            .send(DriverCommand::NativeExecute {
                request: NativeExecute {
                    identity: identity.clone(),
                    attempt: 1,
                    task: "non-cooperative delegate".to_string(),
                    source: "fn main() {}".to_string(),
                },
                delegate: Arc::new(NonCooperativeNativeDelegate {
                    started: Arc::clone(&started),
                    dropped: Arc::clone(&dropped),
                }),
                progress_tx: None,
                caller_cancellation: CancellationToken::new(),
                response_tx,
            })
            .await
            .expect("native execute command");
        harness.outgoing_rx.recv().await.expect("execute frame");
        let delegate_id = DelegateRequestId::new(71);
        harness
            .event_tx
            .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
                id: delegate_id,
                session_id: SessionId::new(identity.session_id.clone()).expect("session"),
                request: DelegateRequest::NativeInvokeTool {
                    run_id: identity.run_id.clone(),
                    call_id: "call-1".to_string(),
                    request: NativeToolRequest::ApplyPatch {
                        patch: "*** Begin Patch\n*** End Patch".to_string(),
                    },
                },
            }))
            .await
            .expect("native delegate request");
        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("non-cooperative delegate should start");

        if settle_by_completion {
            harness
                .event_tx
                .send(DriverEvent::HostMessage(HostToClient::Response {
                    id: RequestId::new(1),
                    result: WireResult::Ok {
                        value: HostResponse::NativeCompleted {
                            session_id: SessionId::new(identity.session_id.clone())
                                .expect("session"),
                            thread_id: identity.thread_id.clone(),
                            run_id: identity.run_id.clone(),
                            source_hash: "a".repeat(64),
                            evidence: Box::new(NativeEvidence {
                                version: 1,
                                summary: "completed".to_string(),
                                verified: Vec::new(),
                                disputed: Vec::new(),
                                unresolved: Vec::new(),
                                artifact_refs: Vec::new(),
                                partial_failures: Vec::new(),
                                provenance_ids: Vec::new(),
                            }),
                        },
                    },
                }))
                .await
                .expect("native completed response");
            response_rx
                .await
                .expect("native response channel")
                .expect("native completion");
        } else {
            harness
                .event_tx
                .send(DriverEvent::HostMessage(
                    HostToClient::CancelDelegateRequest { id: delegate_id },
                ))
                .await
                .expect("cancel delegate request");
        }

        tokio::time::timeout(Duration::from_secs(1), dropped.notified())
            .await
            .expect("non-cooperative delegate future must be forcibly dropped");
        wait_for_native_tasks(&harness.native_tasks).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(150), harness.outgoing_rx.recv())
                .await
                .is_err(),
            "revoked native delegate emitted a late response"
        );
    }
}

#[tokio::test]
async fn non_cooperative_native_delegate_is_forcibly_settled_on_disconnect() {
    let mut harness = DriverHarness::start();
    let identity = NativeRunIdentity {
        session_id: "native-session".to_string(),
        thread_id: "00000000-0000-4000-8000-000000000024".to_string(),
        run_id: "00000000-0000-4000-8000-000000000025".to_string(),
    };
    let started = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    let (response_tx, response_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::NativeExecute {
            request: NativeExecute {
                identity: identity.clone(),
                attempt: 1,
                task: "disconnect non-cooperative delegate".to_string(),
                source: "fn main() {}".to_string(),
            },
            delegate: Arc::new(NonCooperativeNativeDelegate {
                started: Arc::clone(&started),
                dropped: Arc::clone(&dropped),
            }),
            progress_tx: None,
            caller_cancellation: CancellationToken::new(),
            response_tx,
        })
        .await
        .expect("native execute command");
    harness.outgoing_rx.recv().await.expect("execute frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
            id: DelegateRequestId::new(72),
            session_id: SessionId::new(identity.session_id).expect("session"),
            request: DelegateRequest::NativeInvokeTool {
                run_id: identity.run_id,
                call_id: "call-1".to_string(),
                request: NativeToolRequest::Shell {
                    command: "true".to_string(),
                    workdir: None,
                    timeout_ms: 1_000,
                },
            },
        }))
        .await
        .expect("native delegate request");
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("non-cooperative delegate should start");
    harness
        .event_tx
        .send(DriverEvent::Failed(
            "injected non-cooperative disconnect".to_string(),
        ))
        .await
        .expect("disconnect event");
    assert_eq!(
        response_rx
            .await
            .expect("native response channel")
            .unwrap_err(),
        "injected non-cooperative disconnect"
    );
    tokio::time::timeout(Duration::from_secs(1), dropped.notified())
        .await
        .expect("disconnect must forcibly drop non-cooperative delegate");
    tokio::time::timeout(Duration::from_secs(1), &mut harness.driver_task)
        .await
        .expect("driver should settle non-cooperative delegate")
        .expect("driver join");
    assert_eq!(harness.native_tasks.load(Ordering::Acquire), 0);
    assert!(harness.outgoing_rx.try_recv().is_err());
}

async fn wait_for_native_tasks(tasks: &AtomicUsize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tasks.load(Ordering::Acquire) == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("native client task ownership must settle to zero");
}

#[tokio::test]
async fn open_session_includes_nondefault_cell_execution_limits() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let limits = CodeModeSessionCellExecutionLimits {
        max_yield_time_ms: Some(250),
        max_heap_size_bytes: Some(16 * 1024 * 1024),
    };
    let (response_tx, _response_rx) = oneshot::channel();

    harness
        .command_tx
        .send(DriverCommand::OpenSession {
            session: session.clone(),
            delegate: Arc::new(RecordingDelegate::default()),
            limits,
            cleanup: SessionCleanup::new(),
            caller_cancellation: CancellationToken::new(),
            response_tx,
        })
        .await
        .expect("limited session open command");
    let frame = harness
        .outgoing_rx
        .recv()
        .await
        .expect("limited session open frame");

    assert_eq!(
        EncodedFrame::decode_framed::<ClientToHost>(&frame.into_framed_bytes())
            .expect("decode limited session open request"),
        ClientToHost::Request {
            id: RequestId::new(/*value*/ 1),
            request: HostRequest::OpenSession {
                session_id: session.id,
                cell_execution_limits: Some(WireSessionCellExecutionLimits {
                    max_yield_time_ms: Some(250),
                    max_heap_size_bytes: Some(16 * 1024 * 1024),
                }),
            },
        }
    );
}

#[derive(Default)]
struct RecordingDelegate {
    closed_cells: StdMutex<Vec<CellId>>,
    invocations: AtomicUsize,
    notifications: AtomicUsize,
}

struct PanickingDelegate;

struct LargeResultBurstDelegate {
    started: AtomicUsize,
    release: CancellationToken,
}

#[derive(Debug, Eq, PartialEq)]
enum HeldDelegateEvent {
    Started,
    Cancelled,
    Finished,
    CellClosed(CellId),
}

struct HeldDelegate {
    events_tx: mpsc::UnboundedSender<HeldDelegateEvent>,
    release: CancellationToken,
}

impl HeldDelegate {
    fn new() -> (
        Arc<Self>,
        mpsc::UnboundedReceiver<HeldDelegateEvent>,
        CancellationToken,
    ) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let release = CancellationToken::new();
        (
            Arc::new(Self {
                events_tx,
                release: release.clone(),
            }),
            events_rx,
            release,
        )
    }
}

impl CodeModeSessionDelegate for HeldDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        let events_tx = self.events_tx.clone();
        let release = self.release.clone();
        Box::pin(async move {
            let _ = events_tx.send(HeldDelegateEvent::Started);
            cancellation_token.cancelled().await;
            let _ = events_tx.send(HeldDelegateEvent::Cancelled);
            release.cancelled().await;
            let _ = events_tx.send(HeldDelegateEvent::Finished);
            Err("cancelled".to_string())
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        let _ = self
            .events_tx
            .send(HeldDelegateEvent::CellClosed(cell_id.clone()));
    }
}

impl CodeModeSessionDelegate for PanickingDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async { panic!("delegate panic probe") })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

impl CodeModeSessionDelegate for LargeResultBurstDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        self.started.fetch_add(1, Ordering::Release);
        let release = self.release.clone();
        Box::pin(async move {
            tokio::select! {
                _ = cancellation_token.cancelled() => Err("cancelled".to_string()),
                _ = release.cancelled() => Ok("x".repeat(256 * 1024).into()),
            }
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

impl CodeModeSessionDelegate for RecordingDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        self.invocations.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            cancellation_token.cancelled().await;
            Err("cancelled".to_string())
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        self.notifications.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.closed_cells
            .lock()
            .expect("closed cells lock")
            .push(cell_id.clone());
    }
}

fn remote_session() -> RemoteSession {
    RemoteSession {
        id: SessionId::new("session-1").expect("session ID"),
        generation: 1,
    }
}

async fn next_held_delegate_event(
    events_rx: &mut mpsc::UnboundedReceiver<HeldDelegateEvent>,
) -> HeldDelegateEvent {
    tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
        .await
        .expect("delegate event timeout")
        .expect("delegate event stream")
}

#[tokio::test]
async fn deferred_delegates_follow_cell_readiness_and_cancellation() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let delegate = Arc::new(RecordingDelegate::default());
    harness.open(session.clone(), delegate.clone()).await;

    let (first_response_tx, first_response_rx) = oneshot::channel();
    let (second_response_tx, second_response_rx) = oneshot::channel();
    for (tool_call_id, response_tx) in [
        ("first-cell", first_response_tx),
        ("second-cell", second_response_tx),
    ] {
        harness
            .command_tx
            .send(DriverCommand::Execute {
                session: session.clone(),
                request: ExecuteRequest {
                    tool_call_id: tool_call_id.to_string(),
                    enabled_tools: Vec::new(),
                    source: "text('done')".to_string(),
                    yield_time_ms: Some(/*yield_time_ms*/ 1),
                    max_output_tokens: None,
                },
                caller_cancellation: CancellationToken::new(),
                response_tx,
            })
            .await
            .expect("execute command");
        harness.outgoing_rx.recv().await.expect("execute frame");
    }

    let first_cell_id = CellId::new("first-cell".to_string());
    let second_cell_id = CellId::new("second-cell".to_string());
    let first_delegate_id = DelegateRequestId::new(/*value*/ 7);
    let second_delegate_id = DelegateRequestId::new(/*value*/ 8);
    let cancelled_delegate_id = DelegateRequestId::new(/*value*/ 9);
    for (delegate_id, cell_id) in [
        (first_delegate_id, first_cell_id.clone()),
        (second_delegate_id, second_cell_id.clone()),
        (cancelled_delegate_id, first_cell_id.clone()),
    ] {
        harness
            .event_tx
            .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
                id: delegate_id,
                session_id: session.id.clone(),
                request: DelegateRequest::Notify {
                    call_id: format!("notify-{}", cell_id.as_str()),
                    cell_id: (&cell_id).into(),
                    text: "hello".to_string(),
                },
            }))
            .await
            .expect("early delegate request");
    }

    harness
        .event_tx
        .send(DriverEvent::HostMessage(
            HostToClient::CancelDelegateRequest {
                id: cancelled_delegate_id,
            },
        ))
        .await
        .expect("deferred delegate cancellation");

    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 3),
            result: WireResult::Ok {
                value: HostResponse::ExecutionStarted {
                    cell_id: (&second_cell_id).into(),
                },
            },
        }))
        .await
        .expect("second execution-started response");
    let _second_started = second_response_rx
        .await
        .expect("second execute response")
        .expect("second started cell");
    let second_response = tokio::time::timeout(Duration::from_secs(1), harness.outgoing_rx.recv())
        .await
        .expect("second delegate response timeout")
        .expect("second delegate response frame");
    assert_eq!(
        EncodedFrame::decode_framed::<ClientToHost>(&second_response.into_framed_bytes())
            .expect("decode second delegate response"),
        ClientToHost::DelegateResponse {
            id: second_delegate_id,
            result: WireResult::Ok {
                value: codex_code_mode_protocol::host::DelegateResponse::NotificationDelivered,
            },
        }
    );
    assert_eq!(delegate.notifications.load(Ordering::Relaxed), 1);

    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 2),
            result: WireResult::Ok {
                value: HostResponse::ExecutionStarted {
                    cell_id: (&first_cell_id).into(),
                },
            },
        }))
        .await
        .expect("first execution-started response");
    let _first_started = first_response_rx
        .await
        .expect("first execute response")
        .expect("first started cell");
    let first_response = tokio::time::timeout(Duration::from_secs(1), harness.outgoing_rx.recv())
        .await
        .expect("first delegate response timeout")
        .expect("first delegate response frame");
    assert_eq!(
        EncodedFrame::decode_framed::<ClientToHost>(&first_response.into_framed_bytes())
            .expect("decode first delegate response"),
        ClientToHost::DelegateResponse {
            id: first_delegate_id,
            result: WireResult::Ok {
                value: codex_code_mode_protocol::host::DelegateResponse::NotificationDelivered,
            },
        }
    );
    assert_eq!(delegate.notifications.load(Ordering::Relaxed), 2);
    assert!(matches!(
        harness.outgoing_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(harness.alive.load(Ordering::Relaxed));
}

#[tokio::test]
async fn dropped_open_waiter_shuts_down_committed_session() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let (open_tx, open_rx) = oneshot::channel();
    let cleanup = SessionCleanup::new();
    harness
        .command_tx
        .send(DriverCommand::OpenSession {
            session: session.clone(),
            delegate: Arc::new(RecordingDelegate::default()),
            limits: Default::default(),
            cleanup,
            caller_cancellation: CancellationToken::new(),
            response_tx: open_tx,
        })
        .await
        .expect("open command");
    drop(open_rx);
    harness.outgoing_rx.recv().await.expect("open frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 1),
            result: WireResult::Ok {
                value: HostResponse::SessionReady {
                    session_id: session.id.clone(),
                },
            },
        }))
        .await
        .expect("open response");
    harness
        .outgoing_rx
        .recv()
        .await
        .expect("abandoned session shutdown frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 2),
            result: WireResult::Ok {
                value: HostResponse::SessionClosed {
                    session_id: session.id.clone(),
                },
            },
        }))
        .await
        .expect("shutdown response");

    let (execute_tx, execute_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::Execute {
            session: session.clone(),
            request: ExecuteRequest {
                tool_call_id: "call-1".to_string(),
                enabled_tools: Vec::new(),
                source: "text('ok')".to_string(),
                yield_time_ms: None,
                max_output_tokens: None,
            },
            caller_cancellation: CancellationToken::new(),
            response_tx: execute_tx,
        })
        .await
        .expect("execute command");
    assert_eq!(
        execute_rx
            .await
            .expect("execute reply")
            .err()
            .expect("closed session should reject execute"),
        "unknown code-mode session session-1"
    );
}

#[tokio::test]
async fn delegate_cancel_is_best_effort_and_sends_no_late_response() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let delegate = Arc::new(RecordingDelegate::default());
    harness.open(session.clone(), delegate.clone()).await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;
    let request_id = DelegateRequestId::new(/*value*/ 7);
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
            id: request_id,
            session_id: session.id.clone(),
            request: DelegateRequest::InvokeTool {
                invocation: WireNestedToolCall {
                    cell_id: CellId::new("1".to_string()).into(),
                    runtime_tool_call_id: "tool-1".to_string(),
                    tool_name: ToolName::plain("slow").into(),
                    tool_kind: codex_code_mode_protocol::CodeModeToolKind::Function.into(),
                    input: None,
                },
            },
        }))
        .await
        .expect("delegate request");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(
            HostToClient::CancelDelegateRequest { id: request_id },
        ))
        .await
        .expect("delegate cancel");
    tokio::task::yield_now().await;
    assert!(matches!(
        harness.outgoing_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
            id: request_id,
            session_id: session.id,
            request: DelegateRequest::Notify {
                call_id: "notify-reused".to_string(),
                cell_id: CellId::new("1".to_string()).into(),
                text: "duplicate".to_string(),
            },
        }))
        .await
        .expect("reused delegate request");
    tokio::task::yield_now().await;

    assert!(!harness.alive.load(Ordering::Acquire));
    assert_eq!(delegate.invocations.load(Ordering::Relaxed), 1);
    assert_eq!(delegate.notifications.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn concurrent_large_delegate_results_do_not_disconnect_a_backpressured_bulk_lane() {
    const CONCURRENT_RESULTS: usize = 129;

    let (command_tx, command_rx) = mpsc::channel(/*max_capacity*/ 16);
    let (event_tx, event_rx) = mpsc::channel(/*max_capacity*/ 16);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(/*max_capacity*/ 16);
    let (bulk_tx, mut bulk_rx) = mpsc::channel(MAX_PENDING_DELEGATE_CALLS);
    let cancellation = CancellationToken::new();
    let alive = Arc::new(AtomicBool::new(true));
    let failure = Arc::new(StdMutex::new(None));
    let native_tasks = Arc::new(AtomicUsize::new(0));
    let (driver, execute_claim_tx) = ConnectionDriver::new(
        command_rx,
        event_rx,
        event_tx.clone(),
        outgoing_tx,
        DriverLifecycle {
            alive: Arc::clone(&alive),
            failure: Arc::clone(&failure),
            cancellation: cancellation.clone(),
            native_tasks: Arc::clone(&native_tasks),
        },
    );
    let driver_task = tokio::spawn(driver.with_bulk_sender(bulk_tx).run());
    let mut harness = DriverHarness {
        command_tx,
        event_tx,
        execute_claim_tx,
        outgoing_rx,
        cancellation,
        alive,
        failure,
        native_tasks,
        driver_task,
    };
    let session = remote_session();
    let delegate = Arc::new(LargeResultBurstDelegate {
        started: AtomicUsize::new(0),
        release: CancellationToken::new(),
    });
    harness.open(session.clone(), delegate.clone()).await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;

    for value in 1..=CONCURRENT_RESULTS {
        harness
            .start_tool_delegate(&session, DelegateRequestId::new(value as i64))
            .await;
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while delegate.started.load(Ordering::Acquire) < CONCURRENT_RESULTS {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("concurrent delegate calls should all start");

    delegate.release.cancel();
    tokio::time::timeout(Duration::from_secs(10), async {
        while bulk_rx.len() < CONCURRENT_RESULTS {
            assert!(
                harness.alive.load(Ordering::Acquire),
                "bulk queue disconnected before accepting all concurrent tool results"
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("concurrent large results should queue behind the blocked bulk writer");

    let _unrelated = harness.start_cell(session, /*request_id*/ 3, "2").await;
    assert!(harness.alive.load(Ordering::Acquire));

    for _ in 0..CONCURRENT_RESULTS {
        let frame = bulk_rx.recv().await.expect("queued bulk delegate result");
        let message = EncodedFrame::decode_framed::<ClientToHost>(&frame.into_framed_bytes())
            .expect("decode queued delegate result");
        let ClientToHost::DelegateResponse {
            result:
                WireResult::Ok {
                    value: DelegateResponse::ToolResult { result },
                },
            ..
        } = message
        else {
            panic!("expected a successful large delegate result");
        };
        assert_eq!(result.as_str().map(str::len), Some(256 * 1024));
    }
    assert!(harness.alive.load(Ordering::Acquire));
}

#[tokio::test]
async fn delegate_limit_returns_an_error_without_disconnecting() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let delegate = Arc::new(RecordingDelegate::default());
    harness.open(session.clone(), delegate.clone()).await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;

    for value in 1..=MAX_PENDING_DELEGATE_CALLS {
        harness
            .start_tool_delegate(&session, DelegateRequestId::new(value as i64))
            .await;
    }

    let overflow_id = DelegateRequestId::new(MAX_PENDING_DELEGATE_CALLS as i64 + 1);
    harness.start_tool_delegate(&session, overflow_id).await;
    let response = tokio::time::timeout(Duration::from_secs(5), harness.outgoing_rx.recv())
        .await
        .expect("delegate overflow response timeout")
        .expect("delegate overflow response frame");

    assert_eq!(
        EncodedFrame::decode_framed::<ClientToHost>(&response.into_framed_bytes())
            .expect("decode delegate overflow response"),
        ClientToHost::DelegateResponse {
            id: overflow_id,
            result: WireResult::Err {
                message: format!(
                    "code-mode host exceeded the limit of {MAX_PENDING_DELEGATE_CALLS} pending delegate calls"
                ),
            },
        }
    );
    assert!(harness.alive.load(Ordering::Acquire));

    harness
        .event_tx
        .send(DriverEvent::HostMessage(
            HostToClient::CancelDelegateRequest {
                id: DelegateRequestId::new(/*value*/ 1),
            },
        ))
        .await
        .expect("cancel pending delegate");
    harness
        .start_tool_delegate(
            &session,
            DelegateRequestId::new(MAX_PENDING_DELEGATE_CALLS as i64 + 2),
        )
        .await;

    tokio::time::timeout(Duration::from_secs(5), async {
        while delegate.invocations.load(Ordering::Relaxed) <= MAX_PENDING_DELEGATE_CALLS {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("delegate capacity should be available after cancellation");
    assert_eq!(
        delegate.invocations.load(Ordering::Relaxed),
        MAX_PENDING_DELEGATE_CALLS + 1
    );
    assert!(harness.alive.load(Ordering::Acquire));
}

#[tokio::test]
async fn terminate_closes_cell_without_waiting_for_delegate_cleanup() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let (delegate, mut events_rx, release) = HeldDelegate::new();
    harness.open(session.clone(), delegate).await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;
    let delegate_id = DelegateRequestId::new(/*value*/ 7);
    harness.start_tool_delegate(&session, delegate_id).await;
    assert_eq!(
        next_held_delegate_event(&mut events_rx).await,
        HeldDelegateEvent::Started
    );

    let (response_tx, response_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::Terminate {
            session: session.clone(),
            cell_id: CellId::new("1".to_string()),
            response_tx,
        })
        .await
        .expect("terminate command");
    harness.outgoing_rx.recv().await.expect("terminate frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(
            HostToClient::CancelDelegateRequest { id: delegate_id },
        ))
        .await
        .expect("delegate cancel");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::CellClosed {
            session_id: session.id,
            cell_id: CellId::new("1".to_string()).into(),
        }))
        .await
        .expect("cell close");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 3),
            result: WireResult::Ok {
                value: HostResponse::WaitCompleted {
                    outcome: WireWaitOutcome::LiveCell(WireRuntimeResponse::Terminated {
                        cell_id: CellId::new("1".to_string()).into(),
                        content_items: Vec::new(),
                    }),
                },
            },
        }))
        .await
        .expect("terminate response");

    let closure_events = [
        next_held_delegate_event(&mut events_rx).await,
        next_held_delegate_event(&mut events_rx).await,
    ];
    assert!(closure_events.contains(&HeldDelegateEvent::Cancelled));
    assert!(closure_events.contains(&HeldDelegateEvent::CellClosed(CellId::new("1".to_string()))));
    assert_eq!(
        response_rx.await.expect("terminate reply"),
        Ok(codex_code_mode_protocol::WaitOutcome::LiveCell(
            codex_code_mode_protocol::RuntimeResponse::Terminated {
                cell_id: CellId::new("1".to_string()),
                content_items: Vec::new(),
            }
        ))
    );
    assert!(matches!(
        events_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected)
    ));

    release.cancel();
    assert_eq!(
        next_held_delegate_event(&mut events_rx).await,
        HeldDelegateEvent::Finished
    );
    assert!(matches!(
        events_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected)
    ));
    assert!(matches!(
        harness.outgoing_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(harness.alive.load(Ordering::Acquire));
}

#[tokio::test]
async fn shutdown_closes_cell_without_waiting_for_delegate_cleanup() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let (delegate, mut events_rx, release) = HeldDelegate::new();
    harness.open(session.clone(), delegate).await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;
    let delegate_id = DelegateRequestId::new(/*value*/ 7);
    harness.start_tool_delegate(&session, delegate_id).await;
    assert_eq!(
        next_held_delegate_event(&mut events_rx).await,
        HeldDelegateEvent::Started
    );

    let (response_tx, response_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::ShutdownSession {
            session: session.clone(),
            response_tx,
        })
        .await
        .expect("shutdown command");
    harness.outgoing_rx.recv().await.expect("shutdown frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(
            HostToClient::CancelDelegateRequest { id: delegate_id },
        ))
        .await
        .expect("delegate cancel");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::CellClosed {
            session_id: session.id.clone(),
            cell_id: CellId::new("1".to_string()).into(),
        }))
        .await
        .expect("cell close");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 3),
            result: WireResult::Ok {
                value: HostResponse::SessionClosed {
                    session_id: session.id,
                },
            },
        }))
        .await
        .expect("shutdown response");

    let closure_events = [
        next_held_delegate_event(&mut events_rx).await,
        next_held_delegate_event(&mut events_rx).await,
    ];
    assert!(closure_events.contains(&HeldDelegateEvent::Cancelled));
    assert!(closure_events.contains(&HeldDelegateEvent::CellClosed(CellId::new("1".to_string()))));
    assert_eq!(response_rx.await.expect("shutdown reply"), Ok(()));
    assert!(matches!(
        events_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    release.cancel();
    assert_eq!(
        next_held_delegate_event(&mut events_rx).await,
        HeldDelegateEvent::Finished
    );
    assert!(matches!(
        events_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected)
    ));
    assert!(matches!(
        harness.outgoing_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(harness.alive.load(Ordering::Acquire));
}

#[tokio::test]
async fn completed_delegate_request_id_cannot_be_reused() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let delegate = Arc::new(RecordingDelegate::default());
    harness.open(session.clone(), delegate.clone()).await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;
    let request_id = DelegateRequestId::new(/*value*/ 7);
    let request = || DelegateRequest::Notify {
        call_id: "notify-1".to_string(),
        cell_id: CellId::new("1".to_string()).into(),
        text: "once".to_string(),
    };
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
            id: request_id,
            session_id: session.id.clone(),
            request: request(),
        }))
        .await
        .expect("delegate request");
    harness
        .outgoing_rx
        .recv()
        .await
        .expect("delegate response frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
            id: request_id,
            session_id: session.id,
            request: request(),
        }))
        .await
        .expect("reused delegate request");
    tokio::task::yield_now().await;

    assert!(!harness.alive.load(Ordering::Acquire));
    assert_eq!(delegate.notifications.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn delegate_task_panic_becomes_tool_error_without_killing_connection() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    harness
        .open(session.clone(), Arc::new(PanickingDelegate))
        .await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
            id: DelegateRequestId::new(/*value*/ 7),
            session_id: session.id.clone(),
            request: DelegateRequest::InvokeTool {
                invocation: WireNestedToolCall {
                    cell_id: CellId::new("1".to_string()).into(),
                    runtime_tool_call_id: "tool-1".to_string(),
                    tool_name: ToolName::plain("panic").into(),
                    tool_kind: codex_code_mode_protocol::CodeModeToolKind::Function.into(),
                    input: None,
                },
            },
        }))
        .await
        .expect("delegate request");
    tokio::time::timeout(Duration::from_secs(1), harness.outgoing_rx.recv())
        .await
        .expect("delegate response timeout")
        .expect("delegate response frame");

    assert!(harness.alive.load(Ordering::Acquire));
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::CellClosed {
            session_id: session.id,
            cell_id: CellId::new("1".to_string()).into(),
        }))
        .await
        .expect("cell close");
}

#[tokio::test]
async fn delegate_for_unknown_cell_returns_error_without_invocation() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let delegate = Arc::new(RecordingDelegate::default());
    harness.open(session.clone(), delegate.clone()).await;

    let id = DelegateRequestId::new(/*value*/ 7);
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
            id,
            session_id: session.id,
            request: DelegateRequest::InvokeTool {
                invocation: WireNestedToolCall {
                    cell_id: CellId::new("missing".to_string()).into(),
                    runtime_tool_call_id: "tool-1".to_string(),
                    tool_name: ToolName::plain("slow").into(),
                    tool_kind: codex_code_mode_protocol::CodeModeToolKind::Function.into(),
                    input: None,
                },
            },
        }))
        .await
        .expect("delegate request");
    let response = tokio::time::timeout(Duration::from_secs(1), harness.outgoing_rx.recv())
        .await
        .expect("delegate response timeout")
        .expect("delegate response frame");

    assert_eq!(
        EncodedFrame::decode_framed::<ClientToHost>(&response.into_framed_bytes())
            .expect("decode delegate response"),
        ClientToHost::DelegateResponse {
            id,
            result: WireResult::Err {
                message: "code-mode host delegated for unknown cell missing in session session-1"
                    .to_string(),
            },
        }
    );
    assert!(harness.alive.load(Ordering::Acquire));
    assert_eq!(delegate.invocations.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn delegate_after_cell_close_returns_error_without_invocation() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let delegate = Arc::new(RecordingDelegate::default());
    harness.open(session.clone(), delegate.clone()).await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::CellClosed {
            session_id: session.id.clone(),
            cell_id: CellId::new("1".to_string()).into(),
        }))
        .await
        .expect("cell close");
    let id = DelegateRequestId::new(/*value*/ 7);
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::DelegateRequest {
            id,
            session_id: session.id,
            request: DelegateRequest::Notify {
                call_id: "notify-1".to_string(),
                cell_id: CellId::new("1".to_string()).into(),
                text: "late".to_string(),
            },
        }))
        .await
        .expect("delegate request");
    let response = tokio::time::timeout(Duration::from_secs(1), harness.outgoing_rx.recv())
        .await
        .expect("delegate response timeout")
        .expect("delegate response frame");

    assert_eq!(
        EncodedFrame::decode_framed::<ClientToHost>(&response.into_framed_bytes())
            .expect("decode delegate response"),
        ClientToHost::DelegateResponse {
            id,
            result: WireResult::Err {
                message: "code-mode host delegated for unknown cell 1 in session session-1"
                    .to_string(),
            },
        }
    );
    assert!(harness.alive.load(Ordering::Acquire));
    assert_eq!(delegate.notifications.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn mismatched_initial_response_fails_connection_and_closes_cell_once() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let delegate = Arc::new(RecordingDelegate::default());
    harness.open(session.clone(), delegate.clone()).await;
    let started = harness.start_cell(session, /*request_id*/ 2, "1").await;
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::InitialResponse {
            id: RequestId::new(/*value*/ 2),
            result: WireResult::Ok {
                value: WireRuntimeResponse::Yielded {
                    cell_id: CellId::new("2".to_string()).into(),
                    content_items: Vec::new(),
                },
            },
        }))
        .await
        .expect("initial response");

    assert!(started.initial_response().await.is_err());
    assert!(!harness.alive.load(Ordering::Acquire));
    assert_eq!(
        *delegate.closed_cells.lock().expect("closed cells lock"),
        vec![CellId::new("1".to_string())]
    );
}

#[tokio::test]
async fn mismatched_wait_response_fails_connection() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let delegate = Arc::new(RecordingDelegate::default());
    harness.open(session.clone(), delegate.clone()).await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;
    let (response_tx, response_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::Wait {
            session,
            request: WaitRequest {
                cell_id: CellId::new("1".to_string()),
                yield_time_ms: 1,
            },
            caller_cancellation: CancellationToken::new(),
            response_tx,
        })
        .await
        .expect("wait command");
    harness.outgoing_rx.recv().await.expect("wait frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 3),
            result: WireResult::Ok {
                value: HostResponse::WaitCompleted {
                    outcome: WireWaitOutcome::LiveCell(WireRuntimeResponse::Yielded {
                        cell_id: CellId::new("2".to_string()).into(),
                        content_items: Vec::new(),
                    }),
                },
            },
        }))
        .await
        .expect("wait response");

    assert!(response_rx.await.expect("wait reply").is_err());
    assert!(!harness.alive.load(Ordering::Acquire));
    assert_eq!(
        *delegate.closed_cells.lock().expect("closed cells lock"),
        vec![CellId::new("1".to_string())]
    );
}

#[tokio::test]
async fn mismatched_terminate_response_fails_connection() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let delegate = Arc::new(RecordingDelegate::default());
    harness.open(session.clone(), delegate.clone()).await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;
    let (response_tx, response_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::Terminate {
            session,
            cell_id: CellId::new("1".to_string()),
            response_tx,
        })
        .await
        .expect("terminate command");
    harness.outgoing_rx.recv().await.expect("terminate frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 3),
            result: WireResult::Ok {
                value: HostResponse::WaitCompleted {
                    outcome: WireWaitOutcome::MissingCell(WireRuntimeResponse::Terminated {
                        cell_id: CellId::new("2".to_string()).into(),
                        content_items: Vec::new(),
                    }),
                },
            },
        }))
        .await
        .expect("terminate response");

    assert!(response_rx.await.expect("terminate reply").is_err());
    assert!(!harness.alive.load(Ordering::Acquire));
    assert_eq!(
        *delegate.closed_cells.lock().expect("closed cells lock"),
        vec![CellId::new("1".to_string())]
    );
}

#[tokio::test]
async fn remote_wait_accepts_durations_longer_than_five_minutes() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    harness
        .open(session.clone(), Arc::new(RecordingDelegate::default()))
        .await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;
    let (response_tx, response_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::Wait {
            session,
            request: WaitRequest {
                cell_id: CellId::new("1".to_string()),
                yield_time_ms: 300_001,
            },
            caller_cancellation: CancellationToken::new(),
            response_tx,
        })
        .await
        .expect("wait command");
    tokio::time::timeout(Duration::from_secs(1), harness.outgoing_rx.recv())
        .await
        .expect("wait frame timeout")
        .expect("wait frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 3),
            result: WireResult::Ok {
                value: HostResponse::WaitCompleted {
                    outcome: WireWaitOutcome::LiveCell(WireRuntimeResponse::Yielded {
                        cell_id: CellId::new("1".to_string()).into(),
                        content_items: Vec::new(),
                    }),
                },
            },
        }))
        .await
        .expect("wait response");

    assert_eq!(
        response_rx.await.expect("wait reply"),
        Ok(codex_code_mode_protocol::WaitOutcome::LiveCell(
            codex_code_mode_protocol::RuntimeResponse::Yielded {
                cell_id: CellId::new("1".to_string()),
                content_items: Vec::new(),
            }
        ))
    );
}

/// A stalled remote wait must expire even when its request never reaches the host.
#[tokio::test(start_paused = true)]
async fn queued_remote_wait_times_out_and_invalidates_the_connection() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    harness
        .open(session.clone(), Arc::new(RecordingDelegate::default()))
        .await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;
    let connection = Connection {
        command_tx: harness.command_tx.clone(),
        execute_claim_tx: harness.execute_claim_tx.clone(),
        alive: Arc::clone(&harness.alive),
        failure: Arc::clone(&harness.failure),
        cancellation: harness.cancellation.clone(),
        capabilities: CapabilitySet::empty(),
    };
    let response = tokio::spawn(async move {
        let result = connection
            .wait(
                session,
                WaitRequest {
                    cell_id: CellId::new("1".to_string()),
                    yield_time_ms: 1,
                },
            )
            .await;
        (connection, result)
    });
    while harness.outgoing_rx.is_empty() {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(DEFAULT_HOST_WAIT_TRANSPORT_TIMEOUT + Duration::from_secs(2)).await;

    let (connection, result) = response.await.expect("wait task");
    assert_eq!(
        result,
        Err("code-mode host timed out waiting for wait response".to_string())
    );
    assert!(!harness.alive.load(Ordering::Acquire));
    assert!(harness.cancellation.is_cancelled());
    drop(connection);
}

/// A stalled termination must expire and invalidate the same connection as a stalled wait.
#[tokio::test(start_paused = true)]
async fn queued_remote_termination_times_out_and_invalidates_the_connection() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    harness
        .open(session.clone(), Arc::new(RecordingDelegate::default()))
        .await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;
    let connection = Connection {
        command_tx: harness.command_tx.clone(),
        execute_claim_tx: harness.execute_claim_tx.clone(),
        alive: Arc::clone(&harness.alive),
        failure: Arc::clone(&harness.failure),
        cancellation: harness.cancellation.clone(),
        capabilities: CapabilitySet::empty(),
    };
    let response = tokio::spawn(async move {
        let result = connection
            .terminate(session, CellId::new("1".to_string()))
            .await;
        (connection, result)
    });
    while harness.outgoing_rx.is_empty() {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(DEFAULT_HOST_WAIT_TRANSPORT_TIMEOUT + Duration::from_secs(1)).await;

    let (connection, result) = response.await.expect("termination task");
    assert_eq!(
        result,
        Err("code-mode host timed out waiting for terminate response".to_string())
    );
    assert!(!harness.alive.load(Ordering::Acquire));
    assert!(harness.cancellation.is_cancelled());
    drop(connection);
}

#[tokio::test]
async fn cancelled_wait_is_retired_before_next_wait_is_sent() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    harness
        .open(session.clone(), Arc::new(RecordingDelegate::default()))
        .await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;
    let first_cancellation = CancellationToken::new();
    let (first_tx, first_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::Wait {
            session: session.clone(),
            request: WaitRequest {
                cell_id: CellId::new("1".to_string()),
                yield_time_ms: 60_000,
            },
            caller_cancellation: first_cancellation.clone(),
            response_tx: first_tx,
        })
        .await
        .expect("first wait command");
    harness.outgoing_rx.recv().await.expect("first wait frame");
    first_cancellation.cancel();
    drop(first_rx);

    let (second_tx, second_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::Wait {
            session,
            request: WaitRequest {
                cell_id: CellId::new("1".to_string()),
                yield_time_ms: 1,
            },
            caller_cancellation: CancellationToken::new(),
            response_tx: second_tx,
        })
        .await
        .expect("second wait command");
    harness
        .outgoing_rx
        .recv()
        .await
        .expect("cancel request frame");
    assert!(matches!(
        harness.outgoing_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 3),
            result: WireResult::Err {
                message: "code-mode request cancelled".to_string(),
            },
        }))
        .await
        .expect("cancelled wait response");
    harness.outgoing_rx.recv().await.expect("second wait frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 4),
            result: WireResult::Ok {
                value: HostResponse::WaitCompleted {
                    outcome: WireWaitOutcome::LiveCell(WireRuntimeResponse::Yielded {
                        cell_id: CellId::new("1".to_string()).into(),
                        content_items: Vec::new(),
                    }),
                },
            },
        }))
        .await
        .expect("second wait response");

    assert_eq!(
        second_rx.await.expect("second wait reply"),
        Ok(codex_code_mode_protocol::WaitOutcome::LiveCell(
            codex_code_mode_protocol::RuntimeResponse::Yielded {
                cell_id: CellId::new("1".to_string()),
                content_items: Vec::new(),
            }
        ))
    );
}

#[tokio::test]
async fn abandoned_execute_is_tracked_and_terminated_after_admission() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let delegate = Arc::new(RecordingDelegate::default());
    harness.open(session.clone(), delegate.clone()).await;
    let cancellation = CancellationToken::new();
    let (execute_tx, execute_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::Execute {
            session: session.clone(),
            request: ExecuteRequest {
                tool_call_id: "call-1".to_string(),
                enabled_tools: Vec::new(),
                source: "await new Promise(() => {})".to_string(),
                yield_time_ms: Some(1),
                max_output_tokens: None,
            },
            caller_cancellation: cancellation.clone(),
            response_tx: execute_tx,
        })
        .await
        .expect("execute command");
    harness.outgoing_rx.recv().await.expect("execute frame");
    cancellation.cancel();
    drop(execute_rx);
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 2),
            result: WireResult::Ok {
                value: HostResponse::ExecutionStarted {
                    cell_id: CellId::new("1".to_string()).into(),
                },
            },
        }))
        .await
        .expect("execute response");

    harness
        .outgoing_rx
        .recv()
        .await
        .expect("execute cancellation frame");
    harness
        .outgoing_rx
        .recv()
        .await
        .expect("abandoned cell termination frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::InitialResponse {
            id: RequestId::new(/*value*/ 2),
            result: WireResult::Ok {
                value: WireRuntimeResponse::Terminated {
                    cell_id: CellId::new("1".to_string()).into(),
                    content_items: Vec::new(),
                },
            },
        }))
        .await
        .expect("initial response");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 3),
            result: WireResult::Ok {
                value: HostResponse::WaitCompleted {
                    outcome: WireWaitOutcome::LiveCell(WireRuntimeResponse::Terminated {
                        cell_id: CellId::new("1".to_string()).into(),
                        content_items: Vec::new(),
                    }),
                },
            },
        }))
        .await
        .expect("terminate response");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::CellClosed {
            session_id: session.id,
            cell_id: CellId::new("1".to_string()).into(),
        }))
        .await
        .expect("cell close");
    tokio::task::yield_now().await;

    assert!(harness.alive.load(Ordering::Acquire));
    assert_eq!(
        *delegate.closed_cells.lock().expect("closed cells lock"),
        vec![CellId::new("1".to_string())]
    );
}

#[tokio::test]
async fn delivered_but_unclaimed_execute_is_terminated_when_the_caller_is_cancelled() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let delegate = Arc::new(RecordingDelegate::default());
    harness.open(session.clone(), delegate.clone()).await;
    let cancellation = CancellationToken::new();
    let (execute_tx, execute_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::Execute {
            session: session.clone(),
            request: ExecuteRequest {
                tool_call_id: "call-1".to_string(),
                enabled_tools: Vec::new(),
                source: "await new Promise(() => {})".to_string(),
                yield_time_ms: Some(1),
                max_output_tokens: None,
            },
            caller_cancellation: cancellation.clone(),
            response_tx: execute_tx,
        })
        .await
        .expect("execute command");
    harness.outgoing_rx.recv().await.expect("execute frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 2),
            result: WireResult::Ok {
                value: HostResponse::ExecutionStarted {
                    cell_id: CellId::new("1".to_string()).into(),
                },
            },
        }))
        .await
        .expect("execute response");
    let delivered = execute_rx
        .await
        .expect("execute reply")
        .expect("delivered execute");
    assert_eq!(delivered.request_id, RequestId::new(/*value*/ 2));
    cancellation.cancel();

    harness
        .outgoing_rx
        .recv()
        .await
        .expect("execute cancellation frame");
    harness
        .outgoing_rx
        .recv()
        .await
        .expect("unclaimed cell termination frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::InitialResponse {
            id: RequestId::new(/*value*/ 2),
            result: WireResult::Ok {
                value: WireRuntimeResponse::Terminated {
                    cell_id: CellId::new("1".to_string()).into(),
                    content_items: Vec::new(),
                },
            },
        }))
        .await
        .expect("initial response");
    assert!(delivered.started.initial_response().await.is_ok());
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 3),
            result: WireResult::Ok {
                value: HostResponse::WaitCompleted {
                    outcome: WireWaitOutcome::LiveCell(WireRuntimeResponse::Terminated {
                        cell_id: CellId::new("1".to_string()).into(),
                        content_items: Vec::new(),
                    }),
                },
            },
        }))
        .await
        .expect("terminate response");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::CellClosed {
            session_id: session.id,
            cell_id: CellId::new("1".to_string()).into(),
        }))
        .await
        .expect("cell close");
    tokio::task::yield_now().await;

    assert!(harness.alive.load(Ordering::Acquire));
    assert_eq!(
        *delegate.closed_cells.lock().expect("closed cells lock"),
        vec![CellId::new("1".to_string())]
    );
}

#[tokio::test]
async fn session_accepts_more_than_4096_cells_without_growing_a_tombstone_set() {
    const CELL_COUNT: usize = 4097;

    let mut harness = DriverHarness::start();
    let session = remote_session();
    let delegate = Arc::new(RecordingDelegate::default());
    harness.open(session.clone(), delegate.clone()).await;

    for sequence in 1..=CELL_COUNT {
        let request_id = i64::try_from(sequence).expect("cell sequence fits in i64") + 1;
        let cell_id = sequence.to_string();
        let started = harness
            .start_cell(session.clone(), request_id, &cell_id)
            .await;
        harness
            .event_tx
            .send(DriverEvent::HostMessage(HostToClient::InitialResponse {
                id: RequestId::new(request_id),
                result: WireResult::Ok {
                    value: WireRuntimeResponse::Yielded {
                        cell_id: CellId::new(cell_id.clone()).into(),
                        content_items: Vec::new(),
                    },
                },
            }))
            .await
            .expect("initial response");
        assert!(started.initial_response().await.is_ok());
        harness
            .event_tx
            .send(DriverEvent::HostMessage(HostToClient::CellClosed {
                session_id: session.id.clone(),
                cell_id: CellId::new(cell_id).into(),
            }))
            .await
            .expect("cell close");
    }

    tokio::time::timeout(Duration::from_secs(1), async {
        while delegate
            .closed_cells
            .lock()
            .expect("closed cells lock")
            .len()
            != CELL_COUNT
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cell close callbacks timeout");
    assert!(harness.alive.load(Ordering::Acquire));
}

#[tokio::test]
async fn connection_failure_closes_every_live_cell_once() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let delegate = Arc::new(RecordingDelegate::default());
    let cleanup = harness.open(session.clone(), delegate.clone()).await;
    let (execute_tx, execute_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::Execute {
            session,
            request: ExecuteRequest {
                tool_call_id: "call-1".to_string(),
                enabled_tools: Vec::new(),
                source: "await new Promise(() => {})".to_string(),
                yield_time_ms: Some(1),
                max_output_tokens: None,
            },
            caller_cancellation: CancellationToken::new(),
            response_tx: execute_tx,
        })
        .await
        .expect("execute command");
    harness.outgoing_rx.recv().await.expect("execute frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 2),
            result: WireResult::Ok {
                value: HostResponse::ExecutionStarted {
                    cell_id: CellId::new("1".to_string()).into(),
                },
            },
        }))
        .await
        .expect("execute response");
    let _started = execute_rx
        .await
        .expect("execute reply")
        .expect("execute session");
    harness
        .event_tx
        .send(DriverEvent::Failed("host crashed".to_string()))
        .await
        .expect("failure event");
    tokio::time::timeout(Duration::from_secs(1), cleanup.wait())
        .await
        .expect("session cleanup timeout");
    assert_eq!(
        *delegate.closed_cells.lock().expect("closed cells lock"),
        vec![CellId::new("1".to_string())]
    );
}

#[tokio::test]
async fn session_cleanup_does_not_wait_for_delegate_completion() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let (delegate, mut events_rx, release) = HeldDelegate::new();
    let cleanup = harness.open(session.clone(), delegate).await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;
    harness
        .start_tool_delegate(&session, DelegateRequestId::new(/*value*/ 7))
        .await;
    assert_eq!(
        next_held_delegate_event(&mut events_rx).await,
        HeldDelegateEvent::Started
    );

    harness
        .event_tx
        .send(DriverEvent::Failed("host crashed".to_string()))
        .await
        .expect("failure event");
    let closure_events = [
        next_held_delegate_event(&mut events_rx).await,
        next_held_delegate_event(&mut events_rx).await,
    ];
    assert!(closure_events.contains(&HeldDelegateEvent::Cancelled));
    assert!(closure_events.contains(&HeldDelegateEvent::CellClosed(CellId::new("1".to_string()))));
    tokio::time::timeout(Duration::from_secs(1), cleanup.wait())
        .await
        .expect("session cleanup timeout");
    assert!(matches!(
        events_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    release.cancel();
    assert_eq!(
        next_held_delegate_event(&mut events_rx).await,
        HeldDelegateEvent::Finished
    );
    assert!(matches!(
        events_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected)
    ));
}

#[tokio::test]
async fn aborting_driver_marks_connection_dead_and_closes_cells() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    let delegate = Arc::new(RecordingDelegate::default());
    harness.open(session.clone(), delegate.clone()).await;
    let _started = harness
        .start_cell(session.clone(), /*request_id*/ 2, "1")
        .await;
    let (wait_tx, wait_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::Wait {
            session,
            request: WaitRequest {
                cell_id: CellId::new("1".to_string()),
                yield_time_ms: 60_000,
            },
            caller_cancellation: CancellationToken::new(),
            response_tx: wait_tx,
        })
        .await
        .expect("wait command");
    harness.outgoing_rx.recv().await.expect("wait frame");

    harness.driver_task.abort();
    for _ in 0..10 {
        if !harness.alive.load(Ordering::Acquire) {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert!(!harness.alive.load(Ordering::Acquire));
    assert!(harness.cancellation.is_cancelled());
    assert!(wait_rx.await.expect("wait failure").is_err());
    assert_eq!(
        *delegate.closed_cells.lock().expect("closed cells lock"),
        vec![CellId::new("1".to_string())]
    );
}

#[tokio::test]
async fn dropped_shutdown_waiter_does_not_abort_remote_cleanup() {
    let mut harness = DriverHarness::start();
    let session = remote_session();
    harness
        .open(session.clone(), Arc::new(RecordingDelegate::default()))
        .await;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::ShutdownSession {
            session: session.clone(),
            response_tx: shutdown_tx,
        })
        .await
        .expect("shutdown command");
    drop(shutdown_rx);
    harness.outgoing_rx.recv().await.expect("shutdown frame");
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 2),
            result: WireResult::Ok {
                value: HostResponse::SessionClosed {
                    session_id: session.id.clone(),
                },
            },
        }))
        .await
        .expect("shutdown response");

    let (execute_tx, execute_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::Execute {
            session,
            request: ExecuteRequest {
                tool_call_id: "call-2".to_string(),
                enabled_tools: Vec::new(),
                source: "text('unreachable')".to_string(),
                yield_time_ms: None,
                max_output_tokens: None,
            },
            caller_cancellation: CancellationToken::new(),
            response_tx: execute_tx,
        })
        .await
        .expect("execute command");
    assert_eq!(
        execute_rx
            .await
            .expect("execute reply")
            .err()
            .expect("closed session should reject execute"),
        "unknown code-mode session session-1"
    );
    assert!(matches!(
        harness.outgoing_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}
