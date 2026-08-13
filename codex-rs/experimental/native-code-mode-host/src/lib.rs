#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub const SOURCE_BYTES: usize = 64 * 1024;
pub const SOURCE_LINES: usize = 1_500;
pub const DIAGNOSTIC_BYTES: usize = 64 * 1024;
pub const FRAME_BYTES: usize = 64 * 1024;
pub const WORKFLOW_STDOUT_BYTES: usize = 256 * 1024;
pub const WORKFLOW_STDERR_BYTES: usize = 64 * 1024;
pub const FINAL_EVIDENCE_BYTES: usize = 16 * 1024;
pub const TOTAL_CALLS: u32 = 32;
pub const CONCURRENT_CALLS: usize = 4;
pub const COMPILE_TIMEOUT: Duration = Duration::from_secs(10);
pub const WORKFLOW_TIMEOUT: Duration = Duration::from_secs(30);
pub const COMPILER_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
pub const TERMINATE_GRACE: Duration = Duration::from_millis(100);

const MAGIC: u32 = 0x5943_4e52;
const VERSION: u16 = 1;
const HEADER_BYTES: usize = 12;
const TARGET: &str = "aarch64-apple-darwin";
const RUSTC_RELEASE: &str = "release: 1.95.0";
const RUSTC_COMMIT: &str = "commit-hash: 59807616e1fa2540724bfbac14d7976d7e4a3860";
const RUSTC_HOST: &str = "host: aarch64-apple-darwin";

const SPAWN: u16 = 1;
const JOIN: u16 = 2;
const BUDGET: u16 = 3;
const CANCELLED: u16 = 4;
const FINISH: u16 = 5;
const ACK: u16 = 101;
const OUTCOME: u16 = 102;
const BUDGET_RESULT: u16 = 103;
const CANCELLED_RESULT: u16 = 104;
const FINISHED: u16 = 105;
const FAILURE: u16 = 199;

static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct Limits {
    pub compile_timeout: Duration,
    pub workflow_timeout: Duration,
    pub capability_delay: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            compile_timeout: COMPILE_TIMEOUT,
            workflow_timeout: WORKFLOW_TIMEOUT,
            capability_delay: Duration::from_millis(5),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HostEvent {
    pub run_id: String,
    pub kind: HostEventKind,
}

#[derive(Clone, Debug)]
pub enum HostEventKind {
    CompilerStarted(u32),
    Compiled,
    WorkflowStarted(u32),
    FirstCapability,
    DescendantPid(u32),
    OwnedTasksDrained,
    Finished,
}

#[derive(Debug)]
pub struct RunArtifact {
    identity: Arc<ArtifactIdentity>,
}

#[derive(Debug)]
struct ArtifactIdentity {
    run_id: String,
    owned_root: PathBuf,
    run_dir: PathBuf,
    source_path: PathBuf,
    binary_path: PathBuf,
    source_hash: String,
    admitted_source: Box<[u8]>,
    state: std::sync::Mutex<ArtifactState>,
}

#[derive(Debug, Default)]
struct ArtifactState {
    active_executions: usize,
    cleaning: bool,
}

impl RunArtifact {
    pub fn run_id(&self) -> &str {
        &self.identity.run_id
    }

    pub fn run_dir(&self) -> &Path {
        &self.identity.run_dir
    }

    pub fn source_path(&self) -> &Path {
        &self.identity.source_path
    }

    pub fn binary_exists(&self) -> bool {
        self.identity.binary_path.exists()
    }

    pub fn cleanup(&self) -> io::Result<()> {
        {
            let mut state = self
                .identity
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.active_executions != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "run is still owned by an execution",
                ));
            }
            if state.cleaning {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "run was already cleaned",
                ));
            }
            state.cleaning = true;
        }
        let result = cleanup_owned_artifact(&self.identity);
        if result.is_err() {
            self.identity
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .cleaning = false;
        }
        result
    }
}

struct ExecutionLease {
    identity: Arc<ArtifactIdentity>,
}

impl Drop for ExecutionLease {
    fn drop(&mut self) {
        let mut state = self
            .identity
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active_executions = state.active_executions.saturating_sub(1);
    }
}

#[derive(Debug)]
pub struct RunReport {
    pub source_hash: String,
    pub evidence: Vec<u8>,
    pub compile_to_first_capability: Duration,
    pub compile_to_final_evidence: Duration,
    pub peak_concurrent_calls: usize,
    pub total_calls: u32,
    pub workflow_stderr: Vec<u8>,
    pub owned_tasks_after: usize,
    pub observed_descendant_pids: Vec<u32>,
    pub workflow_peak_bytes: u64,
    pub workflow_user_time_ns: u64,
    pub workflow_system_time_ns: u64,
    pub host_peak_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureKind {
    Admission,
    CompilerUnavailable,
    CompilerVersion,
    Compile,
    CompileTimeout,
    Protocol,
    StderrLimit,
    WorkflowTimeout,
    Cancelled,
    ChildCrash,
    CallLimit,
    EvidenceLimit,
    Cleanup,
}

#[derive(Debug)]
pub struct RunFailure {
    /// The only future model-repair boundary: bounded diagnostic plus exact source hash.
    /// This spike never invokes a model or retries compilation automatically.
    pub kind: FailureKind,
    pub source_hash: String,
    pub diagnostic: String,
    pub owned_tasks_after: usize,
    pub observed_descendant_pids: Vec<u32>,
    /// Execution-scoped aggregate ownership: None means `execute` spawned no child;
    /// Some(true) means every child it spawned was reaped; Some(false) means otherwise.
    pub process_reaped: Option<bool>,
}

impl fmt::Display for RunFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.diagnostic)
    }
}

impl std::error::Error for RunFailure {}

#[derive(Debug)]
pub struct NativeHost {
    rustc: PathBuf,
    sdk_rlib: PathBuf,
    root: PathBuf,
    limits: Limits,
    events: Option<mpsc::UnboundedSender<HostEvent>>,
}

impl NativeHost {
    pub async fn new(
        rustc: PathBuf,
        sdk_rlib: PathBuf,
        root: PathBuf,
        limits: Limits,
    ) -> Result<Self, RunFailure> {
        verify_compiler(&rustc).await?;
        if !sdk_rlib.is_file() {
            return Err(failure(
                FailureKind::Admission,
                "SDK rlib is unavailable",
                String::new(),
            ));
        }
        std::fs::create_dir_all(&root).map_err(|error| {
            failure(
                FailureKind::Admission,
                &format!("failed to create run root: {error}"),
                String::new(),
            )
        })?;
        let root = std::fs::canonicalize(&root).map_err(|error| {
            failure(
                FailureKind::Admission,
                &format!("failed to canonicalize run root: {error}"),
                String::new(),
            )
        })?;
        Ok(Self {
            rustc,
            sdk_rlib,
            root,
            limits,
            events: None,
        })
    }

    pub fn with_events(mut self, events: mpsc::UnboundedSender<HostEvent>) -> Self {
        self.events = Some(events);
        self
    }

    pub fn prepare(&self, source: &[u8]) -> Result<RunArtifact, RunFailure> {
        let source_hash = source_hash(source);
        validate_source(source)
            .map_err(|message| failure(FailureKind::Admission, &message, source_hash.clone()))?;
        let sequence = NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed);
        let run_id = format!(
            "run-{}-{sequence}-{}",
            std::process::id(),
            &source_hash[..12]
        );
        let run_dir = self.root.join(&run_id);
        std::fs::create_dir(&run_dir).map_err(|error| {
            failure(
                FailureKind::Admission,
                &format!("failed to create run directory: {error}"),
                source_hash.clone(),
            )
        })?;
        let source_path = run_dir.join("workflow.rs");
        if let Err(error) = std::fs::write(&source_path, source) {
            let cleanup = cleanup_new_run_dir(&run_dir, &source_path);
            return Err(failure(
                FailureKind::Admission,
                &format!(
                    "failed to retain source: {error}; just-created run cleanup: {}",
                    cleanup
                        .as_ref()
                        .map(|()| "complete".to_string())
                        .unwrap_or_else(std::string::ToString::to_string)
                ),
                source_hash,
            ));
        }
        let binary_path = run_dir.join("workflow");
        Ok(RunArtifact {
            identity: Arc::new(ArtifactIdentity {
                run_id,
                owned_root: self.root.clone(),
                run_dir,
                source_path,
                binary_path,
                source_hash,
                admitted_source: source.into(),
                state: std::sync::Mutex::new(ArtifactState::default()),
            }),
        })
    }

    pub async fn execute(
        &self,
        artifact: &RunArtifact,
        cancellation: CancellationToken,
    ) -> Result<RunReport, RunFailure> {
        let lease = acquire_execution(artifact)?;
        self.validate_artifact(&lease.identity)?;
        let hash = lease.identity.source_hash.clone();
        let mut binary_guard = BinaryCleanupGuard::new(&lease.identity.binary_path);
        let started = Instant::now();
        let result = match self
            .compile(&lease.identity, &hash, cancellation.clone())
            .await
        {
            Ok(()) => {
                self.event(&lease.identity, HostEventKind::Compiled);
                self.run_child(&lease.identity, &hash, started, cancellation)
                    .await
            }
            Err(error) => Err(error),
        };
        let process_reaped = match &result {
            Ok(_) => Some(true),
            Err(error) => error.process_reaped,
        };
        remove_binary(&lease.identity, &hash, process_reaped)?;
        binary_guard.disarm();
        result
    }

    fn validate_artifact(&self, artifact: &ArtifactIdentity) -> Result<(), RunFailure> {
        validate_artifact_paths(artifact, &self.root).map_err(|message| {
            failure(
                FailureKind::Admission,
                &message,
                artifact.source_hash.clone(),
            )
        })?;
        let retained = std::fs::read(&artifact.source_path).map_err(|error| {
            failure(
                FailureKind::Admission,
                &format!("retained source unavailable: {error}"),
                artifact.source_hash.clone(),
            )
        })?;
        validate_source(&retained).map_err(|message| {
            failure(
                FailureKind::Admission,
                &format!("retained source failed revalidation: {message}"),
                artifact.source_hash.clone(),
            )
        })?;
        if source_hash(&retained) != artifact.source_hash
            || retained.as_slice() != artifact.admitted_source.as_ref()
        {
            return Err(failure(
                FailureKind::Admission,
                "retained source no longer matches the admitted source identity",
                artifact.source_hash.clone(),
            ));
        }
        if std::fs::symlink_metadata(&artifact.binary_path).is_ok() {
            return Err(failure(
                FailureKind::Admission,
                "disposable binary path unexpectedly exists before compilation",
                artifact.source_hash.clone(),
            ));
        }
        Ok(())
    }

    fn event(&self, artifact: &ArtifactIdentity, kind: HostEventKind) {
        if let Some(events) = &self.events {
            let _ = events.send(HostEvent {
                run_id: artifact.run_id.clone(),
                kind,
            });
        }
    }

    async fn compile(
        &self,
        artifact: &ArtifactIdentity,
        hash: &str,
        cancellation: CancellationToken,
    ) -> Result<(), RunFailure> {
        let dependency_dir = self.sdk_rlib.parent().ok_or_else(|| {
            failure(
                FailureKind::Admission,
                "SDK rlib has no parent",
                hash.to_string(),
            )
        })?;
        let mut command = Command::new(&self.rustc);
        command
            .process_group(0)
            .kill_on_drop(true)
            .current_dir(&artifact.run_dir)
            .arg(&artifact.source_path)
            .arg("--crate-name=native_workflow")
            .arg("--edition=2024")
            .arg(format!("--target={TARGET}"))
            .arg("--extern")
            .arg(format!("ycode_native_sdk={}", self.sdk_rlib.display()))
            .arg("-L")
            .arg(format!("dependency={}", dependency_dir.display()))
            .arg("-C")
            .arg("opt-level=0")
            .arg("-C")
            .arg("debuginfo=0")
            .arg("-o")
            .arg(&artifact.binary_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        scrub_environment(&mut command);
        command.env("TMPDIR", &artifact.run_dir);
        let mut child = command.spawn().map_err(|error| {
            failure(
                FailureKind::CompilerUnavailable,
                &format!("failed to launch pinned rustc: {error}"),
                hash.to_string(),
            )
        })?;
        let pid = child.id().unwrap_or(0);
        let mut group_guard = ProcessGroupGuard::new(pid);
        self.event(artifact, HostEventKind::CompilerStarted(pid));
        let Some(stderr) = child.stderr.take() else {
            let cleanup = terminate_group(&mut child, &mut group_guard).await;
            return Err(failure_after_cleanup(
                FailureKind::Compile,
                "pinned rustc stderr pipe was unavailable",
                hash.to_string(),
                cleanup,
            ));
        };
        let supervised = {
            let diagnostic = read_bounded_owned(stderr, DIAGNOSTIC_BYTES);
            let child_and_diagnostic = async { tokio::join!(child.wait(), diagnostic) };
            tokio::pin!(child_and_diagnostic);
            tokio::select! {
                _ = cancellation.cancelled() => Err((FailureKind::Cancelled, "compilation cancelled")),
                result = tokio::time::timeout(self.limits.compile_timeout, &mut child_and_diagnostic) => match result {
                    Ok(result) => Ok(result),
                    Err(_) => Err((FailureKind::CompileTimeout, "pinned rustc exceeded compile timeout")),
                }
            }
        };
        let (status, diagnostic) = match supervised {
            Ok((Ok(status), diagnostic)) => (status, diagnostic),
            Ok((Err(error), _)) => {
                let cleanup = terminate_group(&mut child, &mut group_guard).await;
                return Err(failure_after_cleanup(
                    FailureKind::Compile,
                    &format!("failed waiting for rustc: {error}"),
                    hash.to_string(),
                    cleanup,
                ));
            }
            Err((kind, message)) => {
                let cleanup = terminate_group(&mut child, &mut group_guard).await;
                return Err(failure_after_cleanup(
                    kind,
                    message,
                    hash.to_string(),
                    cleanup,
                ));
            }
        };
        settle_reaped_group(&mut group_guard);
        let diagnostic =
            diagnostic.unwrap_or_else(|_| b"compiler diagnostic exceeded limit".to_vec());
        if !status.success() {
            let diagnostic = bounded_text(&diagnostic, DIAGNOSTIC_BYTES);
            return Err(process_failure(
                FailureKind::Compile,
                &diagnostic,
                hash.to_string(),
                true,
            ));
        }
        Ok(())
    }

    async fn run_child(
        &self,
        artifact: &ArtifactIdentity,
        hash: &str,
        started: Instant,
        cancellation: CancellationToken,
    ) -> Result<RunReport, RunFailure> {
        let mut command = Command::new(&artifact.binary_path);
        command
            .process_group(0)
            .kill_on_drop(true)
            .current_dir(&artifact.run_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        scrub_environment(&mut command);
        command.env("TMPDIR", &artifact.run_dir);
        let mut child = command.spawn().map_err(|error| {
            process_failure(
                FailureKind::ChildCrash,
                &format!("failed to launch workflow: {error}"),
                hash.to_string(),
                true,
            )
        })?;
        let pid = child.id().unwrap_or(0);
        let mut group_guard = ProcessGroupGuard::new(pid);
        self.event(artifact, HostEventKind::WorkflowStarted(pid));
        let (Some(mut child_input), Some(mut child_output), Some(child_stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let cleanup = terminate_group(&mut child, &mut group_guard).await;
            return Err(failure_after_cleanup(
                FailureKind::ChildCrash,
                "workflow IPC pipes were unavailable",
                hash.to_string(),
                cleanup,
            ));
        };
        let stderr_read = read_bounded_owned(child_stderr, WORKFLOW_STDERR_BYTES);
        tokio::pin!(stderr_read);
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let owned = Arc::new(AtomicUsize::new(0));
        let total = Arc::new(AtomicU32::new(0));
        let descendants = Arc::new(std::sync::Mutex::new(Vec::new()));
        let semaphore = Arc::new(Semaphore::new(CONCURRENT_CALLS));
        let first_capability = Arc::new(std::sync::OnceLock::new());
        let workflow_usage = Arc::new(std::sync::Mutex::new(ProcessUsage::default()));
        let tasks = Arc::new(std::sync::Mutex::new(Vec::<JoinHandle<()>>::new()));
        let mut drop_cancellation = CancelOnDrop::new(cancellation.clone());
        let mut stderr_result = None;
        let ipc_result = {
            let workflow_deadline = tokio::time::Instant::now() + self.limits.workflow_timeout;
            let ipc = self.drive_ipc(
                artifact,
                IpcIo {
                    reader: &mut child_output,
                    writer: &mut child_input,
                },
                started,
                cancellation.clone(),
                IpcState {
                    active: Arc::clone(&active),
                    peak: Arc::clone(&peak),
                    owned: Arc::clone(&owned),
                    total: Arc::clone(&total),
                    descendants: Arc::clone(&descendants),
                    semaphore,
                    first_capability: Arc::clone(&first_capability),
                    workflow_usage: Arc::clone(&workflow_usage),
                    workflow_pid: pid,
                    tasks: Arc::clone(&tasks),
                },
            );
            tokio::pin!(ipc);
            tokio::select! {
                _ = cancellation.cancelled() => Err((FailureKind::Cancelled, "workflow cancelled".to_string())),
                result = tokio::time::timeout_at(workflow_deadline, &mut ipc) => match result {
                    Ok(result) => result,
                    Err(_) => Err((FailureKind::WorkflowTimeout, "workflow exceeded timeout".to_string())),
                },
                stderr = &mut stderr_read => match stderr {
                    Ok(stderr) => {
                        stderr_result = Some(stderr);
                        tokio::select! {
                            _ = cancellation.cancelled() => Err((FailureKind::Cancelled, "workflow cancelled".to_string())),
                            result = tokio::time::timeout_at(workflow_deadline, &mut ipc) => match result {
                                Ok(result) => result,
                                Err(_) => Err((FailureKind::WorkflowTimeout, "workflow exceeded timeout".to_string())),
                            },
                        }
                    }
                    Err(_) => {
                        stderr_result = Some(Vec::new());
                        Err((FailureKind::StderrLimit, "workflow stderr exceeded limit".to_string()))
                    },
                },
            }
        };

        if ipc_result.is_err() {
            cancellation.cancel();
        }
        abort_tasks(&tasks).await;
        drop(child_input);
        let process_reaped = if ipc_result.is_err() {
            terminate_group(&mut child, &mut group_guard)
                .await
                .map_err(|error| {
                    run_failure(
                        FailureKind::Cleanup,
                        format!("failed to terminate and reap workflow: {error}"),
                        hash,
                        &owned,
                        &descendants,
                        false,
                    )
                })?;
            true
        } else {
            match tokio::time::timeout(TERMINATE_GRACE, child.wait()).await {
                Ok(Ok(status)) if status.success() => {
                    settle_reaped_group(&mut group_guard);
                    true
                }
                Ok(Ok(status)) => {
                    let message = format!("workflow exited with {status}");
                    settle_reaped_group(&mut group_guard);
                    return Err(run_failure(
                        FailureKind::ChildCrash,
                        message,
                        hash,
                        &owned,
                        &descendants,
                        true,
                    ));
                }
                Ok(Err(error)) => {
                    let cleanup = terminate_group(&mut child, &mut group_guard).await;
                    return Err(match cleanup {
                        Ok(()) => run_failure(
                            FailureKind::ChildCrash,
                            format!("failed initial workflow wait, but cleanup reaped it: {error}"),
                            hash,
                            &owned,
                            &descendants,
                            true,
                        ),
                        Err(cleanup) => run_failure(
                            FailureKind::Cleanup,
                            format!("workflow wait failed: {error}; cleanup failed: {cleanup}"),
                            hash,
                            &owned,
                            &descendants,
                            false,
                        ),
                    });
                }
                Err(_) => {
                    terminate_group(&mut child, &mut group_guard)
                        .await
                        .map_err(|error| {
                            run_failure(
                                FailureKind::Cleanup,
                                format!("failed to terminate and reap workflow: {error}"),
                                hash,
                                &owned,
                                &descendants,
                                false,
                            )
                        })?;
                    true
                }
            }
        };
        let stderr = match stderr_result {
            Some(stderr) => stderr,
            None => match stderr_read.await {
                Ok(stderr) => stderr,
                Err(_) => {
                    return Err(run_failure(
                        FailureKind::StderrLimit,
                        "workflow stderr exceeded limit".into(),
                        hash,
                        &owned,
                        &descendants,
                        process_reaped,
                    ));
                }
            },
        };
        let evidence = match ipc_result {
            Ok(evidence) => evidence,
            Err((kind, message)) => {
                return Err(run_failure(
                    kind,
                    message,
                    hash,
                    &owned,
                    &descendants,
                    process_reaped,
                ));
            }
        };
        self.event(artifact, HostEventKind::Finished);
        let first = first_capability
            .get()
            .copied()
            .unwrap_or_else(|| started.elapsed());
        let workflow_usage = *workflow_usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let host_usage = process_usage(std::process::id()).unwrap_or_default();
        drop_cancellation.disarm();
        Ok(RunReport {
            source_hash: hash.to_string(),
            evidence,
            compile_to_first_capability: first,
            compile_to_final_evidence: started.elapsed(),
            peak_concurrent_calls: peak.load(Ordering::Acquire),
            total_calls: total.load(Ordering::Acquire),
            workflow_stderr: stderr,
            owned_tasks_after: owned.load(Ordering::Acquire),
            observed_descendant_pids: descendants
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            workflow_peak_bytes: workflow_usage.lifetime_peak_bytes,
            workflow_user_time_ns: workflow_usage.user_time_ns,
            workflow_system_time_ns: workflow_usage.system_time_ns,
            host_peak_bytes: host_usage.lifetime_peak_bytes,
        })
    }

    async fn drive_ipc<R, W>(
        &self,
        artifact: &ArtifactIdentity,
        io: IpcIo<'_, R, W>,
        started: Instant,
        cancellation: CancellationToken,
        state: IpcState,
    ) -> Result<Vec<u8>, (FailureKind, String)>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let IpcIo { reader, writer } = io;
        let mut pending = HashMap::<u32, oneshot::Receiver<CapabilityResult>>::new();
        let mut stdout_remaining = WORKFLOW_STDOUT_BYTES;
        loop {
            let (kind, payload, consumed) = read_frame(reader, stdout_remaining)
                .await
                .map_err(protocol_failure)?;
            stdout_remaining -= consumed;
            match kind {
                SPAWN => {
                    let call_number = state.total.fetch_add(1, Ordering::AcqRel) + 1;
                    if call_number > TOTAL_CALLS {
                        write_failure(writer, "total capability call limit exceeded")
                            .await
                            .map_err(protocol_failure)?;
                        return Err((
                            FailureKind::CallLimit,
                            "total capability call limit exceeded".into(),
                        ));
                    }
                    let (task_id, capability, attempt, input) =
                        parse_spawn(&payload).map_err(protocol_failure)?;
                    if !(1..=4).contains(&capability) {
                        return Err((FailureKind::Protocol, "unknown capability id".into()));
                    }
                    if pending.contains_key(&task_id) {
                        return Err((FailureKind::Protocol, "duplicate task id".into()));
                    }
                    let first = state.first_capability.set(started.elapsed()).is_ok();
                    if first {
                        update_process_usage(state.workflow_pid, &state.workflow_usage);
                        self.event(artifact, HostEventKind::FirstCapability);
                    }
                    if let Some(pid) = parse_descendant_pid(&input) {
                        state
                            .descendants
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(pid);
                        self.event(artifact, HostEventKind::DescendantPid(pid));
                    }
                    let (result_tx, result_rx) = oneshot::channel();
                    let task = spawn_capability(CapabilityJob {
                        task_id,
                        capability,
                        attempt,
                        input,
                        semaphore: Arc::clone(&state.semaphore),
                        active: Arc::clone(&state.active),
                        peak: Arc::clone(&state.peak),
                        owned: Arc::clone(&state.owned),
                        cancellation: cancellation.clone(),
                        delay: self.limits.capability_delay,
                        result_tx,
                        drained_event: self
                            .events
                            .as_ref()
                            .map(|events| (events.clone(), artifact.run_id.clone())),
                    });
                    state
                        .tasks
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(task);
                    pending.insert(task_id, result_rx);
                    write_frame(writer, ACK, &[])
                        .await
                        .map_err(protocol_failure)?;
                }
                JOIN => {
                    let task_id = parse_task_id(&payload).map_err(protocol_failure)?;
                    let receiver = pending.remove(&task_id).ok_or_else(|| {
                        (FailureKind::Protocol, "join referenced unknown task".into())
                    })?;
                    let result = tokio::select! {
                        _ = cancellation.cancelled() => return Err((FailureKind::Cancelled, "capability join cancelled".into())),
                        result = receiver => result.map_err(|_| (FailureKind::Cancelled, "capability task ended without result".into()))?,
                    };
                    let mut response = Vec::with_capacity(6 + result.value.len());
                    response.push(result.status);
                    if result.status == 1 {
                        response.push(result.next_attempt);
                    }
                    put_bytes(&mut response, &result.value).map_err(protocol_failure)?;
                    write_frame(writer, OUTCOME, &response)
                        .await
                        .map_err(protocol_failure)?;
                }
                BUDGET => {
                    require_empty(&payload).map_err(protocol_failure)?;
                    let remaining = TOTAL_CALLS.saturating_sub(state.total.load(Ordering::Acquire));
                    write_frame(writer, BUDGET_RESULT, &remaining.to_le_bytes())
                        .await
                        .map_err(protocol_failure)?;
                }
                CANCELLED => {
                    require_empty(&payload).map_err(protocol_failure)?;
                    write_frame(
                        writer,
                        CANCELLED_RESULT,
                        &[u8::from(cancellation.is_cancelled())],
                    )
                    .await
                    .map_err(protocol_failure)?;
                }
                FINISH => {
                    update_process_usage(state.workflow_pid, &state.workflow_usage);
                    if payload.len() > FINAL_EVIDENCE_BYTES {
                        write_failure(writer, "final evidence exceeds limit")
                            .await
                            .map_err(protocol_failure)?;
                        return Err((
                            FailureKind::EvidenceLimit,
                            "final evidence exceeds limit before history boundary".into(),
                        ));
                    }
                    if !pending.is_empty() {
                        return Err((
                            FailureKind::Protocol,
                            "finish with unjoined capability tasks".into(),
                        ));
                    }
                    if payload.is_empty() {
                        return Err((FailureKind::Protocol, "final evidence is empty".into()));
                    }
                    if !payload.starts_with(b"native-evidence:v1:") {
                        return Err((
                            FailureKind::Protocol,
                            "final evidence schema is invalid".into(),
                        ));
                    }
                    write_frame(writer, FINISHED, &[])
                        .await
                        .map_err(protocol_failure)?;
                    return Ok(payload);
                }
                _ => return Err((FailureKind::Protocol, format!("unknown frame kind {kind}"))),
            }
        }
    }
}

struct IpcState {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    owned: Arc<AtomicUsize>,
    total: Arc<AtomicU32>,
    descendants: Arc<std::sync::Mutex<Vec<u32>>>,
    semaphore: Arc<Semaphore>,
    first_capability: Arc<std::sync::OnceLock<Duration>>,
    workflow_usage: Arc<std::sync::Mutex<ProcessUsage>>,
    workflow_pid: u32,
    tasks: Arc<std::sync::Mutex<Vec<JoinHandle<()>>>>,
}

struct IpcIo<'a, R, W> {
    reader: &'a mut R,
    writer: &'a mut W,
}

struct CapabilityResult {
    status: u8,
    next_attempt: u8,
    value: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ProcessUsage {
    lifetime_peak_bytes: u64,
    user_time_ns: u64,
    system_time_ns: u64,
}

fn process_usage(pid: u32) -> Option<ProcessUsage> {
    if pid == 0 {
        return None;
    }
    // SAFETY: proc_pid_rusage writes one initialized v4 structure for a concrete pid.
    let mut usage = unsafe { std::mem::zeroed::<libc::rusage_info_v4>() };
    // SAFETY: the pointer is valid for the v4 structure and lives through the call.
    let result = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V4,
            (&mut usage as *mut libc::rusage_info_v4).cast(),
        )
    };
    (result == 0).then_some(ProcessUsage {
        lifetime_peak_bytes: usage.ri_lifetime_max_phys_footprint,
        user_time_ns: usage.ri_user_time,
        system_time_ns: usage.ri_system_time,
    })
}

fn update_process_usage(pid: u32, destination: &std::sync::Mutex<ProcessUsage>) {
    if let Some(usage) = process_usage(pid) {
        let mut destination = destination
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        destination.lifetime_peak_bytes = destination
            .lifetime_peak_bytes
            .max(usage.lifetime_peak_bytes);
        destination.user_time_ns = destination.user_time_ns.max(usage.user_time_ns);
        destination.system_time_ns = destination.system_time_ns.max(usage.system_time_ns);
    }
}

struct CapabilityJob {
    task_id: u32,
    capability: u8,
    attempt: u8,
    input: Vec<u8>,
    semaphore: Arc<Semaphore>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    owned: Arc<AtomicUsize>,
    cancellation: CancellationToken,
    delay: Duration,
    result_tx: oneshot::Sender<CapabilityResult>,
    drained_event: Option<(mpsc::UnboundedSender<HostEvent>, String)>,
}

fn spawn_capability(job: CapabilityJob) -> JoinHandle<()> {
    let CapabilityJob {
        task_id,
        capability,
        attempt,
        input,
        semaphore,
        active,
        peak,
        owned,
        cancellation,
        delay,
        result_tx,
        drained_event,
    } = job;
    owned.fetch_add(1, Ordering::AcqRel);
    let owned_guard = OwnedTaskGuard {
        owned: Arc::clone(&owned),
        drained_event,
    };
    tokio::spawn(async move {
        let _owned_guard = owned_guard;
        let permit = tokio::select! {
            _ = cancellation.cancelled() => return,
            permit = semaphore.acquire_owned() => match permit { Ok(permit) => permit, Err(_) => return },
        };
        let now = active.fetch_add(1, Ordering::AcqRel) + 1;
        peak.fetch_max(now, Ordering::AcqRel);
        let _active_guard = ActiveGuard(active);
        tokio::select! {
            _ = cancellation.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
        drop(permit);
        let digest = stable_digest(&input)
            ^ u64::from(task_id)
            ^ (u64::from(capability) << 40)
            ^ (u64::from(attempt) << 48);
        let value =
            format!("task={task_id};cap={capability};attempt={attempt};digest={digest:016x}")
                .into_bytes();
        let (status, next_attempt) = if capability == 1 && attempt == 1 {
            (1, attempt.saturating_add(1))
        } else {
            (0, 0)
        };
        let _ = result_tx.send(CapabilityResult {
            status,
            next_attempt,
            value,
        });
    })
}

struct OwnedTaskGuard {
    owned: Arc<AtomicUsize>,
    drained_event: Option<(mpsc::UnboundedSender<HostEvent>, String)>,
}
impl Drop for OwnedTaskGuard {
    fn drop(&mut self) {
        if self.owned.fetch_sub(1, Ordering::AcqRel) == 1
            && let Some((events, run_id)) = &self.drained_event
        {
            let _ = events.send(HostEvent {
                run_id: run_id.clone(),
                kind: HostEventKind::OwnedTasksDrained,
            });
        }
    }
}
struct ActiveGuard(Arc<AtomicUsize>);
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
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

async fn abort_tasks(tasks: &std::sync::Mutex<Vec<JoinHandle<()>>>) {
    let tasks = {
        let mut locked = tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *locked)
    };
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        let _ = task.await;
    }
}

async fn verify_compiler(rustc: &Path) -> Result<(), RunFailure> {
    let mut command = Command::new(rustc);
    command
        .process_group(0)
        .kill_on_drop(true)
        .arg("--version")
        .arg("--verbose")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    scrub_environment(&mut command);
    let mut child = command.spawn().map_err(|error| {
        failure(
            FailureKind::CompilerUnavailable,
            &format!("pinned rustc unavailable: {error}"),
            String::new(),
        )
    })?;
    let pid = child.id().unwrap_or(0);
    let mut group_guard = ProcessGroupGuard::new(pid);
    let Some(output) = child.stdout.take() else {
        let cleanup = terminate_group(&mut child, &mut group_guard).await;
        return Err(failure_after_cleanup(
            FailureKind::CompilerUnavailable,
            "compiler version stdout pipe was unavailable",
            String::new(),
            cleanup,
        ));
    };
    let supervised = {
        let read = read_bounded_owned(output, 4096);
        let child_and_output = async { tokio::join!(child.wait(), read) };
        tokio::pin!(child_and_output);
        tokio::time::timeout(COMPILER_PROBE_TIMEOUT, &mut child_and_output).await
    };
    let (status, read) = match supervised {
        Ok((Ok(status), read)) => (status, read),
        Ok((Err(error), _)) => {
            let cleanup = terminate_group(&mut child, &mut group_guard).await;
            return Err(failure_after_cleanup(
                FailureKind::CompilerUnavailable,
                &format!("compiler version probe failed: {error}"),
                String::new(),
                cleanup,
            ));
        }
        Err(_) => {
            let cleanup = terminate_group(&mut child, &mut group_guard).await;
            return Err(failure_after_cleanup(
                FailureKind::CompilerUnavailable,
                "compiler version probe timed out",
                String::new(),
                cleanup,
            ));
        }
    };
    settle_reaped_group(&mut group_guard);
    let stdout = match read {
        Ok(bytes) => bounded_text(&bytes, 4096),
        Err(_) => {
            return Err(process_failure(
                FailureKind::CompilerVersion,
                "compiler version output exceeded 4096 bytes",
                String::new(),
                true,
            ));
        }
    };
    if !status.success()
        || !stdout.contains(RUSTC_RELEASE)
        || !stdout.contains(RUSTC_COMMIT)
        || !stdout.contains(RUSTC_HOST)
    {
        return Err(process_failure(
            FailureKind::CompilerVersion,
            &format!(
                "wrong rustc version; expected 1.95.0/59807616e/aarch64-apple-darwin, observed: {stdout}"
            ),
            String::new(),
            true,
        ));
    }
    Ok(())
}

fn validate_source(source: &[u8]) -> Result<(), String> {
    if source.len() > SOURCE_BYTES {
        return Err(format!("source exceeds {SOURCE_BYTES} bytes"));
    }
    let source = std::str::from_utf8(source).map_err(|_| "source must be UTF-8".to_string())?;
    if source.lines().count() > SOURCE_LINES {
        return Err(format!("source exceeds {SOURCE_LINES} lines"));
    }
    const FORBIDDEN: &[&str] = &[
        "#![feature",
        "#![link",
        "#[link",
        "link_args",
        "global_asm!",
        "include!",
        "include_bytes!",
        "include_str!",
        "env!",
        "option_env!",
        "extern crate",
        "mod ",
        "#[path",
        "--extern",
    ];
    for token in FORBIDDEN {
        if source.contains(token) {
            return Err(format!(
                "source contains forbidden admission token `{token}`"
            ));
        }
    }
    if !source.contains("ycode_native_sdk") {
        return Err("source must use the native SDK".into());
    }
    Ok(())
}

fn acquire_execution(artifact: &RunArtifact) -> Result<ExecutionLease, RunFailure> {
    let mut state = artifact
        .identity
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.cleaning {
        return Err(failure(
            FailureKind::Admission,
            "run artifact is being cleaned or was already cleaned",
            artifact.identity.source_hash.clone(),
        ));
    }
    if state.active_executions != 0 {
        return Err(failure(
            FailureKind::Admission,
            "run artifact already has an active execution owner",
            artifact.identity.source_hash.clone(),
        ));
    }
    state.active_executions = 1;
    drop(state);
    Ok(ExecutionLease {
        identity: Arc::clone(&artifact.identity),
    })
}

fn validate_artifact_paths(artifact: &ArtifactIdentity, host_root: &Path) -> Result<(), String> {
    if artifact.owned_root != host_root {
        return Err("run artifact belongs to a different host root".into());
    }
    let expected_run_dir = host_root.join(&artifact.run_id);
    if artifact.run_dir != expected_run_dir
        || artifact.source_path != expected_run_dir.join("workflow.rs")
        || artifact.binary_path != expected_run_dir.join("workflow")
    {
        return Err("run artifact paths do not match its immutable run identity".into());
    }
    let canonical_root = std::fs::canonicalize(host_root)
        .map_err(|error| format!("run root is unavailable: {error}"))?;
    if canonical_root != host_root {
        return Err("run root canonical identity changed".into());
    }
    let canonical_run = std::fs::canonicalize(&artifact.run_dir)
        .map_err(|error| format!("run directory is unavailable: {error}"))?;
    if canonical_run != artifact.run_dir || canonical_run.parent() != Some(host_root) {
        return Err("run directory escaped its host-owned canonical root".into());
    }
    let canonical_source = std::fs::canonicalize(&artifact.source_path)
        .map_err(|error| format!("retained source is unavailable: {error}"))?;
    if canonical_source != artifact.source_path || !canonical_source.is_file() {
        return Err("retained source path is not the exact host-owned file".into());
    }
    Ok(())
}

fn cleanup_new_run_dir(run_dir: &Path, source_path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(source_path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir(source_path)?,
        Ok(_) => std::fs::remove_file(source_path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::remove_dir(run_dir)
}

fn cleanup_owned_artifact(artifact: &ArtifactIdentity) -> io::Result<()> {
    validate_artifact_paths_for_cleanup(artifact)?;
    remove_owned_file(&artifact.binary_path)?;
    remove_owned_file(&artifact.source_path)?;
    std::fs::remove_dir(&artifact.run_dir)
}

fn validate_artifact_paths_for_cleanup(artifact: &ArtifactIdentity) -> io::Result<()> {
    let expected_run_dir = artifact.owned_root.join(&artifact.run_id);
    if artifact.run_dir != expected_run_dir
        || artifact.source_path != expected_run_dir.join("workflow.rs")
        || artifact.binary_path != expected_run_dir.join("workflow")
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cleanup rejected paths outside the immutable run identity",
        ));
    }
    let root = std::fs::canonicalize(&artifact.owned_root)?;
    let run = std::fs::canonicalize(&artifact.run_dir)?;
    if root != artifact.owned_root || run != artifact.run_dir || run.parent() != Some(&root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cleanup rejected a run directory outside the canonical owned root",
        ));
    }
    Ok(())
}

fn remove_owned_file(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn scrub_environment(command: &mut Command) {
    command.env_clear();
    command.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    command.env("TMPDIR", "/tmp");
    command.env("LANG", "C");
}

fn remove_binary(
    artifact: &ArtifactIdentity,
    hash: &str,
    process_reaped: Option<bool>,
) -> Result<(), RunFailure> {
    match std::fs::remove_file(&artifact.binary_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            let diagnostic = format!("failed to remove disposable workflow binary: {error}");
            Err(match process_reaped {
                Some(reaped) => {
                    process_failure(FailureKind::Cleanup, &diagnostic, hash.to_string(), reaped)
                }
                None => failure(FailureKind::Cleanup, &diagnostic, hash.to_string()),
            })
        }
    }
}

struct BinaryCleanupGuard<'a> {
    path: &'a Path,
    armed: bool,
}

impl<'a> BinaryCleanupGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BinaryCleanupGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

struct ProcessGroupGuard {
    pid: u32,
    armed: bool,
}

impl ProcessGroupGuard {
    fn new(pid: u32) -> Self {
        Self {
            pid,
            armed: pid != 0,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        signal_group(self.pid, libc::SIGKILL);
        blocking_reap(self.pid);
        self.armed = false;
    }
}

fn settle_reaped_group(guard: &mut ProcessGroupGuard) {
    if guard.armed {
        signal_group(guard.pid, libc::SIGKILL);
        guard.disarm();
    }
}

async fn terminate_group(child: &mut Child, guard: &mut ProcessGroupGuard) -> io::Result<()> {
    if guard.armed {
        signal_group(guard.pid, libc::SIGTERM);
    }
    let first_wait = tokio::time::timeout(TERMINATE_GRACE, child.wait()).await;
    let first_wait_reaped = wait_result_proves_reaped(&first_wait);
    if guard.armed {
        signal_group(guard.pid, libc::SIGKILL);
    }
    let result = match first_wait {
        Ok(Ok(_)) if first_wait_reaped => Ok(()),
        Ok(Ok(_)) => unreachable!("successful wait must prove reaping"),
        Ok(Err(first_error)) => child.wait().await.map(|_| ()).map_err(|second_error| {
            io::Error::other(format!(
                "initial wait failed: {first_error}; retry failed: {second_error}"
            ))
        }),
        Err(_) => child.wait().await.map(|_| ()),
    };
    if result.is_ok() {
        guard.disarm();
    }
    result
}

fn wait_result_proves_reaped<T>(
    result: &Result<io::Result<T>, tokio::time::error::Elapsed>,
) -> bool {
    matches!(result, Ok(Ok(_)))
}

fn blocking_reap(pid: u32) {
    if pid == 0 {
        return;
    }
    loop {
        let mut status = 0;
        // SAFETY: pid is the concrete owned child leader; this is the drop-path reap fallback.
        let result = unsafe { libc::waitpid(pid as i32, &mut status, 0) };
        if result == pid as i32 {
            return;
        }
        if result == -1 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return;
    }
}

fn signal_group(pid: u32, signal: i32) {
    // SAFETY: kill is called with a negative, already-owned child process-group id.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

async fn read_bounded(reader: &mut (impl AsyncRead + Unpin), cap: usize) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > cap {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stream limit exceeded",
            ));
        }
        output.extend_from_slice(&chunk[..read]);
    }
}

async fn read_bounded_owned(mut reader: impl AsyncRead + Unpin, cap: usize) -> io::Result<Vec<u8>> {
    read_bounded(&mut reader, cap).await
}

async fn read_frame(
    reader: &mut (impl AsyncRead + Unpin),
    remaining: usize,
) -> io::Result<(u16, Vec<u8>, usize)> {
    if remaining < HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workflow stdout byte limit exceeded",
        ));
    }
    let mut header = [0_u8; HEADER_BYTES];
    reader.read_exact(&mut header).await?;
    if u32::from_le_bytes([header[0], header[1], header[2], header[3]]) != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad frame magic",
        ));
    }
    if u16::from_le_bytes([header[4], header[5]]) != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported protocol version",
        ));
    }
    let kind = u16::from_le_bytes([header[6], header[7]]);
    let length = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
    if length > FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "declared frame exceeds cap",
        ));
    }
    if HEADER_BYTES + length > remaining {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workflow stdout byte limit exceeded",
        ));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await?;
    Ok((kind, payload, HEADER_BYTES + length))
}

async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    kind: u16,
    payload: &[u8],
) -> io::Result<()> {
    if payload.len() > FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "outgoing frame exceeds cap",
        ));
    }
    writer.write_all(&MAGIC.to_le_bytes()).await?;
    writer.write_all(&VERSION.to_le_bytes()).await?;
    writer.write_all(&kind.to_le_bytes()).await?;
    writer
        .write_all(&(payload.len() as u32).to_le_bytes())
        .await?;
    writer.write_all(payload).await?;
    writer.flush().await
}

async fn write_failure(writer: &mut (impl AsyncWrite + Unpin), message: &str) -> io::Result<()> {
    write_frame(writer, FAILURE, message.as_bytes()).await
}

fn parse_spawn(payload: &[u8]) -> io::Result<(u32, u8, u8, Vec<u8>)> {
    if payload.len() < 10 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated spawn payload",
        ));
    }
    let task = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let capability = payload[4];
    let attempt = payload[5];
    let length = u32::from_le_bytes([payload[6], payload[7], payload[8], payload[9]]) as usize;
    if length > FRAME_BYTES || payload.len() != 10 + length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid spawn input length",
        ));
    }
    Ok((task, capability, attempt, payload[10..].to_vec()))
}

fn parse_task_id(payload: &[u8]) -> io::Result<u32> {
    if payload.len() != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid task id payload",
        ));
    }
    Ok(u32::from_le_bytes([
        payload[0], payload[1], payload[2], payload[3],
    ]))
}

fn require_empty(payload: &[u8]) -> io::Result<()> {
    if payload.is_empty() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected payload",
        ))
    }
}

fn put_bytes(output: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    let length = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "value too large"))?;
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(bytes);
    Ok(())
}

fn parse_descendant_pid(input: &[u8]) -> Option<u32> {
    std::str::from_utf8(input)
        .ok()?
        .strip_prefix("descendant:")?
        .parse()
        .ok()
}

fn stable_digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn source_hash(source: &[u8]) -> String {
    format!("{:x}", Sha256::digest(source))
}

fn bounded_text(bytes: &[u8], cap: usize) -> String {
    const MARKER: &str = "...[truncated]";
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(cap)]);
    if text.len() <= cap {
        return text.into_owned();
    }
    let body_cap = cap.saturating_sub(MARKER.len());
    let mut end = body_cap.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = String::with_capacity(cap);
    bounded.push_str(&text[..end]);
    bounded.push_str(MARKER);
    bounded
}

fn protocol_failure(error: io::Error) -> (FailureKind, String) {
    (FailureKind::Protocol, error.to_string())
}

fn failure(kind: FailureKind, diagnostic: &str, source_hash: String) -> RunFailure {
    RunFailure {
        kind,
        diagnostic: bounded_diagnostic(diagnostic.as_bytes(), &source_hash),
        source_hash,
        owned_tasks_after: 0,
        observed_descendant_pids: Vec::new(),
        process_reaped: None,
    }
}

fn process_failure(
    kind: FailureKind,
    diagnostic: &str,
    source_hash: String,
    process_reaped: bool,
) -> RunFailure {
    RunFailure {
        kind,
        diagnostic: bounded_diagnostic(diagnostic.as_bytes(), &source_hash),
        source_hash,
        owned_tasks_after: 0,
        observed_descendant_pids: Vec::new(),
        process_reaped: Some(process_reaped),
    }
}

fn failure_after_cleanup(
    kind: FailureKind,
    diagnostic: &str,
    source_hash: String,
    cleanup: io::Result<()>,
) -> RunFailure {
    match cleanup {
        Ok(()) => process_failure(kind, diagnostic, source_hash, true),
        Err(error) => process_failure(
            FailureKind::Cleanup,
            &format!("{diagnostic}; process cleanup failed: {error}"),
            source_hash,
            false,
        ),
    }
}

fn bounded_diagnostic(bytes: &[u8], source_hash: &str) -> String {
    const MARKER: &str = "\n...[diagnostic truncated]";
    let suffix = if source_hash.is_empty() {
        String::new()
    } else {
        format!("\n[source_hash={source_hash}]")
    };
    let text = String::from_utf8_lossy(bytes);
    if text.len().saturating_add(suffix.len()) <= DIAGNOSTIC_BYTES {
        let mut output = text.into_owned();
        output.push_str(&suffix);
        return output;
    }
    let body_cap = DIAGNOSTIC_BYTES
        .saturating_sub(MARKER.len())
        .saturating_sub(suffix.len());
    let mut end = body_cap.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut output = String::with_capacity(DIAGNOSTIC_BYTES);
    output.push_str(&text[..end]);
    output.push_str(MARKER);
    output.push_str(&suffix);
    output
}

fn run_failure(
    kind: FailureKind,
    diagnostic: String,
    hash: &str,
    owned: &AtomicUsize,
    descendants: &std::sync::Mutex<Vec<u32>>,
    process_reaped: bool,
) -> RunFailure {
    RunFailure {
        kind,
        diagnostic: bounded_diagnostic(diagnostic.as_bytes(), hash),
        source_hash: hash.to_string(),
        owned_tasks_after: owned.load(Ordering::Acquire),
        observed_descendant_pids: descendants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
        process_reaped: Some(process_reaped),
    }
}

pub fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: signal zero performs a read-only existence probe for a concrete pid.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn frame_rejects_oversize_before_reading_payload() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&MAGIC.to_le_bytes()).await.unwrap();
        writer.write_all(&VERSION.to_le_bytes()).await.unwrap();
        writer.write_all(&SPAWN.to_le_bytes()).await.unwrap();
        writer
            .write_all(&((FRAME_BYTES + 1) as u32).to_le_bytes())
            .await
            .unwrap();
        let error = read_frame(&mut reader, FRAME_BYTES + HEADER_BYTES)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("declared frame exceeds cap"));
    }

    #[tokio::test]
    async fn frame_rejects_bad_magic_and_version() {
        for (magic, version, expected) in
            [(0_u32, VERSION, "magic"), (MAGIC, VERSION + 1, "version")]
        {
            let (mut writer, mut reader) = tokio::io::duplex(64);
            writer.write_all(&magic.to_le_bytes()).await.unwrap();
            writer.write_all(&version.to_le_bytes()).await.unwrap();
            writer.write_all(&SPAWN.to_le_bytes()).await.unwrap();
            writer.write_all(&0_u32.to_le_bytes()).await.unwrap();
            assert!(
                read_frame(&mut reader, FRAME_BYTES + HEADER_BYTES)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
        }
    }

    #[test]
    fn admission_rejects_oversize_unstable_and_link_injection() {
        assert!(
            validate_source(&vec![b'x'; SOURCE_BYTES + 1])
                .unwrap_err()
                .contains("bytes")
        );
        assert!(
            validate_source(b"#![feature(test)]\nuse ycode_native_sdk::Context;")
                .unwrap_err()
                .contains("feature")
        );
        assert!(
            validate_source(b"#[link(name=\"x\")]\nuse ycode_native_sdk::Context;")
                .unwrap_err()
                .contains("link")
        );
    }

    #[test]
    fn wait_paths_are_structurally_async() {
        let source = include_str!("lib.rs");
        assert!(!source.contains(&["try_", "wait()"].concat()));
        assert!(!source.contains(&["thread::yield", "_now"].concat()));
        assert!(source.contains("tokio::select!"));
        assert!(source.contains("Semaphore"));
    }

    #[test]
    fn pipe_readers_are_borrowed_futures_not_detached_tasks() {
        let source = include_str!("lib.rs");
        let spawn = ["tokio::", "spawn"].concat();
        let detached_reader = ["stderr_", "task"].concat();
        let reader_join = ["JoinHandle<", "io::Result"].concat();
        assert_eq!(
            source.matches(&spawn).count(),
            1,
            "only capability jobs spawn"
        );
        assert!(!source.contains(&detached_reader));
        assert!(!source.contains(&reader_join));
        assert!(source.matches("read_bounded_owned(").count() >= 4);
    }

    #[test]
    fn inner_wait_error_never_claims_reaping() {
        let result: Result<io::Result<()>, tokio::time::error::Elapsed> =
            Ok(Err(io::Error::other("injected wait failure")));
        assert!(!wait_result_proves_reaped(&result));
    }

    #[test]
    fn failed_source_retention_cleans_only_the_just_created_run() {
        let root = tempfile::tempdir().unwrap();
        let run = root.path().join("run-owned");
        let source = run.join("workflow.rs");
        std::fs::create_dir(&run).unwrap();
        std::fs::create_dir(&source).unwrap();
        assert!(std::fs::write(&source, b"source").is_err());
        cleanup_new_run_dir(&run, &source).unwrap();
        assert!(!run.exists());
        assert!(root.path().exists());
    }
}
