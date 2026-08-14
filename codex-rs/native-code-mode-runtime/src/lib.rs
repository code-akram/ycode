#![cfg(target_os = "macos")]

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::io;
use std::io::Read as _;
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub const SOURCE_BYTES: usize = 64 * 1024;
pub const TASK_BYTES: usize = 16 * 1024;
pub const SOURCE_LINES: usize = 1_500;
pub const DIAGNOSTIC_BYTES: usize = 64 * 1024;
pub const FRAME_BYTES: usize = 64 * 1024;
pub const WORKFLOW_STDOUT_BYTES: usize = 256 * 1024;
pub const WORKFLOW_STDERR_BYTES: usize = 64 * 1024;
pub const FINAL_EVIDENCE_BYTES: usize = 16 * 1024;
pub const CALL_OUTPUT_BYTES: usize = 64 * 1024;
pub const TOTAL_CALLS: u32 = 32;
pub const CONCURRENT_CALLS: usize = 4;
pub const COMPILE_TIMEOUT: Duration = Duration::from_secs(10);
pub const WORKFLOW_TIMEOUT: Duration = Duration::from_secs(30);
pub const COMPILER_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
pub const TERMINATE_GRACE: Duration = Duration::from_millis(100);
pub const RUNS_PER_THREAD: usize = 20;
pub const RUNS_GLOBAL: usize = 256;
pub const RUN_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
pub const CACHE_BYTES: u64 = 1024 * 1024 * 1024;
pub const CACHE_ENTRIES: usize = 256;
pub const RAW_CALL_ARTIFACT_BYTES: usize = 1024 * 1024;

const WORKFLOW_EXECUTABLE_BYTES: u64 = 64 * 1024 * 1024;
const SDK_RLIB_BYTES: u64 = 16 * 1024 * 1024;
const RUN_MANIFEST_BYTES: u64 = 16 * 1024;
const CACHE_MANIFEST_BYTES: u64 = 64 * 1024;
const CACHE_STAMP_BYTES: u64 = 64;
const EVIDENCE_ARTIFACT_BYTES: usize = 128 * 1024;
const MAX_EVIDENCE_ITEMS: usize = 64;
const MAX_EVIDENCE_STRING_BYTES: usize = 4 * 1024;
const RUN_RESERVATION_BYTES: u64 = (TASK_BYTES
    + 2 * SOURCE_BYTES
    + 2 * DIAGNOSTIC_BYTES
    + WORKFLOW_STDOUT_BYTES
    + WORKFLOW_STDERR_BYTES
    + EVIDENCE_ARTIFACT_BYTES
    + RAW_CALL_ARTIFACT_BYTES) as u64;

const MAGIC: u32 = 0x5943_4e52;
const VERSION: u16 = 1;
const HEADER_BYTES: usize = 12;
const RUSTC_RELEASE: &str = "release: 1.95.0";
const RUSTC_COMMIT: &str = "commit-hash: 59807616e1fa2540724bfbac14d7976d7e4a3860";
const RUSTC_HOST: &str = "host: aarch64-apple-darwin";
const WORKFLOW_FLAGS: &[&str] = &[
    "--crate-name=native_workflow",
    "--edition=2024",
    "--target=aarch64-apple-darwin",
    "-Copt-level=0",
    "-Cdebuginfo=0",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeRequest {
    Shell {
        command: String,
        workdir: Option<String>,
        timeout_ms: u32,
    },
    ApplyPatch {
        patch: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeOutcome {
    Success(Vec<u8>),
    Retry(String),
    Failure(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeCall {
    pub call_id: String,
    pub request: NativeRequest,
}

pub type NativeDelegateFuture<'a> =
    Pin<Box<dyn Future<Output = Result<NativeOutcome, String>> + Send + 'a>>;

pub trait NativeCapabilityDelegate: Send + Sync {
    fn invoke<'a>(
        &'a self,
        call: NativeCall,
        cancellation: CancellationToken,
    ) -> NativeDelegateFuture<'a>;
}

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
static CACHE_LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Weak<Semaphore>>>> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct Limits {
    pub compile_timeout: Duration,
    pub workflow_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            compile_timeout: COMPILE_TIMEOUT,
            workflow_timeout: WORKFLOW_TIMEOUT,
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
    Finished,
}

/// The runtime emits at most five fixed lifecycle events plus one verified descendant event for
/// each admitted capability call. Keeping the channel larger than that complete execution set
/// makes `try_send` non-blocking without permitting unbounded observer memory.
pub const HOST_EVENT_MAX_PER_EXECUTION: usize = 5 + TOTAL_CALLS as usize;
pub const HOST_EVENT_CHANNEL_CAPACITY: usize = 40;

#[derive(Debug)]
pub struct RunArtifact {
    identity: Arc<ArtifactIdentity>,
}

#[derive(Debug)]
struct ArtifactIdentity {
    thread_id: String,
    run_id: String,
    attempt: u8,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RunManifest {
    version: u16,
    thread_id: String,
    run_id: String,
    task_hash: String,
    initial_source_hash: String,
    created_at_unix_ms: u64,
    completed_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CacheManifest {
    version: u16,
    key: String,
    binary_hash: String,
    source_hash: String,
    sdk_hash: String,
    rustc_vv: String,
    flags: Vec<String>,
    created_at_unix_ms: u64,
}

struct CompiledWorkflow {
    binary: PathBuf,
    compiler_spawned: bool,
    _use_guard: AdvisoryLock,
}

impl RunManifest {
    fn new(thread_id: &str, run_id: &str, task: &str, initial_source_hash: String) -> Self {
        Self {
            version: 1,
            thread_id: thread_id.to_string(),
            run_id: run_id.to_string(),
            task_hash: source_hash(task.as_bytes()),
            initial_source_hash,
            created_at_unix_ms: unix_millis(),
            completed_at_unix_ms: None,
        }
    }
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
        let run_lock = match try_advisory_lock(
            &self.identity.run_dir.join(".active.lock"),
            LockMode::Exclusive,
        ) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                self.identity
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .cleaning = false;
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "run is owned by another process",
                ));
            }
            Err(error) => {
                self.identity
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .cleaning = false;
                return Err(error);
            }
        };
        let result = cleanup_owned_artifact(&self.identity);
        drop(run_lock);
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
    process_lock: Option<AdvisoryLock>,
    complete_on_drop: bool,
    settled: bool,
}

impl ExecutionLease {
    fn preserve_for_repair(&mut self) {
        self.complete_on_drop = false;
    }

    fn settle(&mut self) -> io::Result<()> {
        if self.complete_on_drop {
            mark_run_completed(&self.identity.run_dir)?;
        }
        self.release();
        self.settled = true;
        Ok(())
    }

    fn release(&mut self) {
        let mut state = self
            .identity
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active_executions = state.active_executions.saturating_sub(1);
        drop(state);
        self.process_lock.take();
    }
}

impl Drop for ExecutionLease {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        if self.complete_on_drop {
            let _ = mark_run_completed(&self.identity.run_dir);
        }
        self.release();
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
    repair_pending: bool,
}

impl RunFailure {
    pub fn admission(diagnostic: impl Into<String>) -> Self {
        failure(FailureKind::Admission, &diagnostic.into(), String::new())
    }

    pub fn evidence_limit(source_hash: String, diagnostic: impl Into<String>) -> Self {
        process_failure(
            FailureKind::EvidenceLimit,
            &diagnostic.into(),
            source_hash,
            true,
        )
    }
}

impl fmt::Display for RunFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.diagnostic)
    }
}

impl std::error::Error for RunFailure {}

pub struct NativeHost {
    rustc: PathBuf,
    rustc_vv: String,
    sdk_rlib: PathBuf,
    sdk_hash: String,
    root: PathBuf,
    limits: Limits,
    events: Option<mpsc::Sender<HostEvent>>,
    delegate: Arc<dyn NativeCapabilityDelegate>,
}

impl NativeHost {
    pub async fn discover(
        sdk_rlib: PathBuf,
        root: PathBuf,
        limits: Limits,
        delegate: Arc<dyn NativeCapabilityDelegate>,
    ) -> Result<Self, RunFailure> {
        let rustc = discover_pinned_compiler().await?;
        Self::new(rustc, sdk_rlib, root, limits, delegate).await
    }

    pub async fn new(
        rustc: PathBuf,
        sdk_rlib: PathBuf,
        root: PathBuf,
        limits: Limits,
        delegate: Arc<dyn NativeCapabilityDelegate>,
    ) -> Result<Self, RunFailure> {
        let rustc_vv = verify_compiler(&rustc).await?;
        let (sdk_hash, _) =
            hash_bounded_regular_file(&sdk_rlib, SDK_RLIB_BYTES).map_err(|error| {
                failure(
                    FailureKind::Admission,
                    &format!("SDK rlib is unavailable or invalid: {error}"),
                    String::new(),
                )
            })?;
        create_private_dir_all(&root).map_err(|error| {
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
        for relative in [
            "sessions",
            "cache",
            "cache/sdk",
            "cache/objects",
            "cache/locks",
        ] {
            create_private_dir_all(&root.join(relative)).map_err(|error| {
                failure(
                    FailureKind::Admission,
                    &format!("failed to create native store {relative}: {error}"),
                    String::new(),
                )
            })?;
        }
        Ok(Self {
            rustc,
            rustc_vv,
            sdk_rlib,
            sdk_hash,
            root,
            limits,
            events: None,
            delegate,
        })
    }

    pub fn with_events(mut self, events: mpsc::Sender<HostEvent>) -> Self {
        self.events = Some(events);
        self
    }

    pub fn prepare_run(
        &self,
        thread_id: &str,
        run_id: &str,
        task: &str,
        attempt: u8,
        source: &[u8],
    ) -> Result<RunArtifact, RunFailure> {
        let source_hash = source_hash(source);
        validate_identity("thread", thread_id)
            .and_then(|()| validate_identity("run", run_id))
            .map_err(|message| failure(FailureKind::Admission, &message, source_hash.clone()))?;
        if task.len() > TASK_BYTES || std::str::from_utf8(task.as_bytes()).is_err() {
            return Err(failure(
                FailureKind::Admission,
                "task exceeds 16384 UTF-8 bytes",
                source_hash,
            ));
        }
        if !(1..=2).contains(&attempt) {
            return Err(failure(
                FailureKind::Admission,
                "attempt must be 1 or 2",
                source_hash,
            ));
        }
        validate_source(source)
            .map_err(|message| failure(FailureKind::Admission, &message, source_hash.clone()))?;
        if attempt == 1 {
            enforce_run_retention(&self.root, thread_id).map_err(|error| {
                failure(
                    FailureKind::Admission,
                    &format!("run retention failed: {error}"),
                    source_hash.clone(),
                )
            })?;
        }
        let thread_dir = self.root.join("sessions").join(thread_id);
        let runs_dir = thread_dir.join("runs");
        create_private_dir_all(&runs_dir).map_err(|error| {
            failure(
                FailureKind::Admission,
                &format!("failed to create session directories: {error}"),
                source_hash.clone(),
            )
        })?;
        let run_dir = runs_dir.join(run_id);
        if attempt == 1 {
            create_private_dir(&run_dir).map_err(|error| {
                failure(
                    FailureKind::Admission,
                    &format!("failed to create run directory: {error}"),
                    source_hash.clone(),
                )
            })?;
            write_private_file(&run_dir.join("task.txt"), task.as_bytes()).map_err(|error| {
                let _ = cleanup_exact_run(&self.root, thread_id, run_id);
                failure(
                    FailureKind::Admission,
                    &format!("failed to retain task: {error}"),
                    source_hash.clone(),
                )
            })?;
            let manifest = RunManifest::new(thread_id, run_id, task, source_hash.clone());
            write_json_private(&run_dir.join("manifest.json"), &manifest).map_err(|error| {
                let _ = cleanup_exact_run(&self.root, thread_id, run_id);
                failure(
                    FailureKind::Admission,
                    &format!("failed to retain run manifest: {error}"),
                    source_hash.clone(),
                )
            })?;
            create_private_dir(&run_dir.join("calls")).map_err(|error| {
                let _ = cleanup_exact_run(&self.root, thread_id, run_id);
                failure(
                    FailureKind::Admission,
                    &format!("failed to create bounded call artifact directory: {error}"),
                    source_hash.clone(),
                )
            })?;
        } else {
            validate_existing_run(&self.root, thread_id, run_id, task).map_err(|error| {
                failure(
                    FailureKind::Admission,
                    &format!("invalid repair run identity: {error}"),
                    source_hash.clone(),
                )
            })?;
        }
        let attempt_dir = run_dir.join(format!("attempt-{attempt}"));
        create_private_dir(&attempt_dir).map_err(|error| {
            failure(
                FailureKind::Admission,
                &format!("failed to create attempt directory: {error}"),
                source_hash.clone(),
            )
        })?;
        let source_path = attempt_dir.join("source.rs");
        if let Err(error) = write_private_file(&source_path, source) {
            let cleanup = if attempt == 1 {
                cleanup_exact_run(&self.root, thread_id, run_id)
            } else {
                std::fs::remove_dir(&attempt_dir)
            };
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
        if let Err(error) = write_private_file(&attempt_dir.join("rustc.stderr"), &[]) {
            if attempt == 1 {
                let _ = cleanup_exact_run(&self.root, thread_id, run_id);
            } else {
                let _ = std::fs::remove_dir_all(&attempt_dir);
            }
            return Err(failure(
                FailureKind::Admission,
                &format!("failed to initialize bounded compiler diagnostic: {error}"),
                source_hash,
            ));
        }
        let binary_path = attempt_dir.join("workflow");
        Ok(RunArtifact {
            identity: Arc::new(ArtifactIdentity {
                thread_id: thread_id.to_string(),
                run_id: run_id.to_string(),
                attempt,
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
        let mut lease = acquire_execution(artifact)?;
        self.validate_artifact(&lease.identity)?;
        let hash = lease.identity.source_hash.clone();
        let started = Instant::now();
        let result = match self
            .compile(&lease.identity, &hash, cancellation.clone())
            .await
        {
            Ok(compiled) => match self.event(&lease.identity, HostEventKind::Compiled) {
                Ok(()) => {
                    self.run_child(
                        &lease.identity,
                        &compiled.binary,
                        &hash,
                        started,
                        cancellation,
                        compiled.compiler_spawned,
                    )
                    .await
                }
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        if artifact.identity.attempt == 1
            && result.as_ref().is_err_and(|failure| failure.repair_pending)
        {
            lease.preserve_for_repair();
        }
        if let Err(error) = lease.settle() {
            let mut settlement = failure(
                FailureKind::Cleanup,
                &format!("failed to settle run ownership: {error}"),
                hash,
            );
            settlement.process_reaped = result
                .as_ref()
                .err()
                .and_then(|failure| failure.process_reaped);
            return Err(settlement);
        }
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
        let retained = read_bounded_regular_file(&artifact.source_path, SOURCE_BYTES as u64)
            .map_err(|error| {
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

    fn event(&self, artifact: &ArtifactIdentity, kind: HostEventKind) -> Result<(), RunFailure> {
        if let Some(events) = &self.events {
            events
                .try_send(HostEvent {
                    run_id: artifact.run_id.clone(),
                    kind,
                })
                .map_err(|error| {
                    failure(
                        FailureKind::Cleanup,
                        &format!("native runtime progress delivery failed: {error}"),
                        artifact.source_hash.clone(),
                    )
                })?;
        }
        Ok(())
    }

    async fn compile(
        &self,
        artifact: &ArtifactIdentity,
        hash: &str,
        cancellation: CancellationToken,
    ) -> Result<CompiledWorkflow, RunFailure> {
        let cache_key = self.cache_key(&artifact.source_hash);
        let keyed_lock = self.cache_lock(&cache_key);
        let _cache_lease = keyed_lock
            .acquire_owned()
            .await
            .map_err(|_| failure(FailureKind::Cleanup, "cache lock closed", hash.to_string()))?;
        let cache_lock_path = self
            .root
            .join("cache/locks")
            .join(format!("{cache_key}.lock"));
        let cache_use = try_advisory_lock(&cache_lock_path, LockMode::Shared).map_err(|error| {
            failure(
                FailureKind::Admission,
                &format!("failed to inspect binary cache ownership: {error}"),
                hash.to_string(),
            )
        })?;
        if let Some(use_guard) = cache_use
            && let Ok(binary) = self.validate_cached_binary(&cache_key, hash)
        {
            let _ = write_cache_use_stamp(&self.root, &cache_key);
            retain_diagnostic(artifact, &[]).map_err(|error| {
                failure(
                    FailureKind::Cleanup,
                    &format!("failed to retain cache-hit diagnostic: {error}"),
                    hash.to_string(),
                )
            })?;
            return Ok(CompiledWorkflow {
                binary,
                compiler_spawned: false,
                _use_guard: use_guard,
            });
        }
        let build_guard = try_advisory_lock(&cache_lock_path, LockMode::Exclusive)
            .map_err(|error| {
                failure(
                    FailureKind::Admission,
                    &format!("failed to acquire binary cache build lock: {error}"),
                    hash.to_string(),
                )
            })?
            .ok_or_else(|| {
                failure(
                    FailureKind::Admission,
                    "binary cache key is owned by another host process; retry is bounded and safe",
                    hash.to_string(),
                )
            })?;
        if let Ok(binary) = self.validate_cached_binary(&cache_key, hash) {
            let use_guard = build_guard.downgrade().map_err(|error| {
                failure(
                    FailureKind::Admission,
                    &format!("failed to retain cache winner use lock: {error}"),
                    hash.to_string(),
                )
            })?;
            retain_diagnostic(artifact, &[]).map_err(|error| {
                failure(
                    FailureKind::Cleanup,
                    &format!("failed to retain cache-winner diagnostic: {error}"),
                    hash.to_string(),
                )
            })?;
            return Ok(CompiledWorkflow {
                binary,
                compiler_spawned: false,
                _use_guard: use_guard,
            });
        }
        evict_corrupt_cache_object(&self.root, &cache_key).map_err(|error| {
            failure(
                FailureKind::Admission,
                &format!("failed to evict corrupt cached object: {error}"),
                hash.to_string(),
            )
        })?;
        let temporary = self.root.join("cache/objects").join(format!(
            ".tmp-{cache_key}-{}",
            NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
        ));
        create_private_dir(&temporary).map_err(|error| {
            failure(
                FailureKind::Admission,
                &format!("failed to create cache staging directory: {error}"),
                hash.to_string(),
            )
        })?;
        let mut temporary_guard = OwnedDirGuard::new(temporary.clone());
        let binary_path = temporary.join("workflow");
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
            .arg(WORKFLOW_FLAGS[0])
            .arg(WORKFLOW_FLAGS[1])
            .arg(WORKFLOW_FLAGS[2])
            .arg("--extern")
            .arg(format!("ycode_native_sdk={}", self.sdk_rlib.display()))
            .arg("-L")
            .arg(format!("dependency={}", dependency_dir.display()))
            .arg(WORKFLOW_FLAGS[3])
            .arg(WORKFLOW_FLAGS[4])
            .arg("-o")
            .arg(&binary_path)
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
        if let Err(error) = self.event(artifact, HostEventKind::CompilerStarted(pid)) {
            let cleanup = terminate_group(&mut child, &mut group_guard).await;
            return Err(failure_after_cleanup(
                FailureKind::Cleanup,
                &error.diagnostic,
                hash.to_string(),
                cleanup,
            ));
        }
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
        retain_diagnostic(artifact, &diagnostic).map_err(|error| {
            process_failure(
                FailureKind::Cleanup,
                &format!("failed to retain bounded compiler diagnostic: {error}"),
                hash.to_string(),
                true,
            )
        })?;
        if !status.success() {
            let diagnostic = bounded_text(&diagnostic, DIAGNOSTIC_BYTES);
            let mut failure =
                process_failure(FailureKind::Compile, &diagnostic, hash.to_string(), true);
            failure.repair_pending = true;
            return Err(failure);
        }
        let (binary_hash, binary_bytes) = hash_bounded_regular_file_with_mode(
            &binary_path,
            WORKFLOW_EXECUTABLE_BYTES,
            Some(0o500),
        )
        .map_err(|error| {
            process_failure(
                FailureKind::Compile,
                &format!("compiled workflow executable is invalid or oversized: {error}"),
                hash.to_string(),
                true,
            )
        })?;
        let manifest = CacheManifest {
            version: 1,
            key: cache_key.clone(),
            binary_hash,
            source_hash: artifact.source_hash.clone(),
            sdk_hash: self.sdk_hash.clone(),
            rustc_vv: self.rustc_vv.clone(),
            flags: WORKFLOW_FLAGS
                .iter()
                .map(|flag| (*flag).to_string())
                .collect(),
            created_at_unix_ms: unix_millis(),
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| {
            process_failure(
                FailureKind::Cleanup,
                &format!("failed to encode cache manifest: {error}"),
                hash.to_string(),
                true,
            )
        })?;
        if manifest_bytes.len() as u64 > CACHE_MANIFEST_BYTES {
            return Err(process_failure(
                FailureKind::Cleanup,
                "encoded cache manifest exceeds fixed cap",
                hash.to_string(),
                true,
            ));
        }
        write_private_file(&temporary.join("manifest.json"), &manifest_bytes).map_err(|error| {
            process_failure(
                FailureKind::Cleanup,
                &format!("failed to write cache manifest: {error}"),
                hash.to_string(),
                true,
            )
        })?;
        let candidate_bytes = binary_bytes.saturating_add(manifest_bytes.len() as u64);
        let publication_lease = self
            .cache_lock("publication")
            .acquire_owned()
            .await
            .map_err(|_| {
                process_failure(
                    FailureKind::Cleanup,
                    "cache publication lock closed",
                    hash.to_string(),
                    true,
                )
            })?;
        let publication_path = self.root.join("cache/locks/publication.lock");
        let publication_guard = try_advisory_lock(&publication_path, LockMode::Exclusive)
            .map_err(|error| {
                process_failure(
                    FailureKind::Cleanup,
                    &format!("failed to acquire cache publication lock: {error}"),
                    hash.to_string(),
                    true,
                )
            })?
            .ok_or_else(|| {
                process_failure(
                    FailureKind::Cleanup,
                    "another host is publishing a cache object; bounded retry required",
                    hash.to_string(),
                    true,
                )
            })?;
        enforce_cache_retention(&self.root, 1, candidate_bytes).map_err(|error| {
            process_failure(
                FailureKind::Cleanup,
                &format!("binary cache cannot reserve candidate bytes: {error}"),
                hash.to_string(),
                true,
            )
        })?;
        std::fs::File::open(&temporary)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                process_failure(
                    FailureKind::Cleanup,
                    &format!("failed to sync cache staging directory: {error}"),
                    hash.to_string(),
                    true,
                )
            })?;
        let object = self.root.join("cache/objects").join(&cache_key);
        let mut published_guard = None;
        match std::fs::rename(&temporary, &object) {
            Ok(()) => {
                temporary_guard.disarm();
                published_guard = Some(OwnedDirGuard::new(object));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(process_failure(
                    FailureKind::Cleanup,
                    &format!("failed to publish cached executable atomically: {error}"),
                    hash.to_string(),
                    true,
                ));
            }
        }
        std::fs::File::open(self.root.join("cache/objects"))
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                process_failure(
                    FailureKind::Cleanup,
                    &format!("failed to sync cache object directory: {error}"),
                    hash.to_string(),
                    true,
                )
            })?;
        let binary = self.validate_cached_binary(&cache_key, hash)?;
        write_cache_use_stamp(&self.root, &cache_key).map_err(|error| {
            process_failure(
                FailureKind::Cleanup,
                &format!("failed to record cache use: {error}"),
                hash.to_string(),
                true,
            )
        })?;
        enforce_cache_retention(&self.root, 0, 0).map_err(|error| {
            process_failure(
                FailureKind::Cleanup,
                &format!("binary cache exceeds fixed cap: {error}"),
                hash.to_string(),
                true,
            )
        })?;
        assert_cache_postcondition(&self.root).map_err(|error| {
            process_failure(
                FailureKind::Cleanup,
                &format!("binary cache postcondition failed: {error}"),
                hash.to_string(),
                true,
            )
        })?;
        if let Some(guard) = published_guard.as_mut() {
            guard.disarm();
        }
        drop(publication_guard);
        drop(publication_lease);
        let use_guard = build_guard.downgrade().map_err(|error| {
            process_failure(
                FailureKind::Cleanup,
                &format!("failed to retain published executable use lock: {error}"),
                hash.to_string(),
                true,
            )
        })?;
        Ok(CompiledWorkflow {
            binary,
            compiler_spawned: true,
            _use_guard: use_guard,
        })
    }

    fn cache_key(&self, source_hash: &str) -> String {
        let mut digest = Sha256::new();
        digest.update(b"ycode-native-code-mode-object-v1\0");
        digest.update(source_hash.as_bytes());
        digest.update(b"\0");
        digest.update(self.sdk_hash.as_bytes());
        digest.update(b"\0sdk-v1\0child-protocol-v1\0");
        digest.update(self.rustc_vv.as_bytes());
        for flag in WORKFLOW_FLAGS {
            digest.update(b"\0");
            digest.update(flag.as_bytes());
        }
        format!("{:x}", digest.finalize())
    }

    fn cache_lock(&self, key: &str) -> Arc<Semaphore> {
        let lock_id = format!("{}:{key}", self.root.display());
        let mut locks = CACHE_LOCKS
            .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = locks.get(&lock_id).and_then(Weak::upgrade) {
            return existing;
        }
        let lock = Arc::new(Semaphore::new(1));
        locks.insert(lock_id, Arc::downgrade(&lock));
        lock
    }

    fn validate_cached_binary(&self, key: &str, hash: &str) -> Result<PathBuf, RunFailure> {
        let object = self.root.join("cache/objects").join(key);
        let canonical = std::fs::canonicalize(&object).map_err(|error| {
            failure(
                FailureKind::Admission,
                &format!("cache miss: {error}"),
                hash.to_string(),
            )
        })?;
        if canonical != object {
            return Err(failure(
                FailureKind::Admission,
                "cached object path changed identity",
                hash.to_string(),
            ));
        }
        let manifest = read_cache_manifest(&object.join("manifest.json")).map_err(|error| {
            failure(
                FailureKind::Admission,
                &format!("cached object manifest invalid: {error}"),
                hash.to_string(),
            )
        })?;
        let binary = object.join("workflow");
        let metadata = std::fs::symlink_metadata(&binary).map_err(|error| {
            failure(
                FailureKind::Admission,
                &format!("cached executable unavailable: {error}"),
                hash.to_string(),
            )
        })?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > WORKFLOW_EXECUTABLE_BYTES
            || metadata.permissions().mode() & 0o777 != 0o500
            || manifest.version != 1
            || manifest.key != key
            || manifest.source_hash != hash
            || manifest.sdk_hash != self.sdk_hash
            || manifest.rustc_vv != self.rustc_vv
            || manifest.flags != WORKFLOW_FLAGS
        {
            return Err(failure(
                FailureKind::Admission,
                "cached executable metadata does not match its content identity",
                hash.to_string(),
            ));
        }
        let (binary_hash, _) = hash_bounded_regular_file(&binary, WORKFLOW_EXECUTABLE_BYTES)
            .map_err(|error| {
                failure(
                    FailureKind::Admission,
                    &format!("cached executable unreadable: {error}"),
                    hash.to_string(),
                )
            })?;
        if binary_hash != manifest.binary_hash {
            return Err(failure(
                FailureKind::Admission,
                "cached executable hash mismatch",
                hash.to_string(),
            ));
        }
        Ok(binary)
    }

    async fn run_child(
        &self,
        artifact: &ArtifactIdentity,
        binary: &Path,
        hash: &str,
        started: Instant,
        cancellation: CancellationToken,
        compiler_spawned: bool,
    ) -> Result<RunReport, RunFailure> {
        let mut command = Command::new(binary);
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
            let diagnostic = format!("failed to launch workflow: {error}");
            if compiler_spawned {
                process_failure(FailureKind::ChildCrash, &diagnostic, hash.to_string(), true)
            } else {
                failure(FailureKind::ChildCrash, &diagnostic, hash.to_string())
            }
        })?;
        let pid = child.id().unwrap_or(0);
        let mut group_guard = ProcessGroupGuard::new(pid);
        if let Err(error) = self.event(artifact, HostEventKind::WorkflowStarted(pid)) {
            let cleanup = terminate_group(&mut child, &mut group_guard).await;
            return Err(failure_after_cleanup(
                FailureKind::Cleanup,
                &error.diagnostic,
                hash.to_string(),
                cleanup,
            ));
        }
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
                    raw_artifact_bytes: Arc::new(AtomicUsize::new(0)),
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
        self.event(artifact, HostEventKind::Finished)
            .map_err(|error| {
                process_failure(
                    FailureKind::Cleanup,
                    &error.diagnostic,
                    hash.to_string(),
                    process_reaped,
                )
            })?;
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
        let mut joined_calls = HashSet::<String>::new();
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
                    let (task_id, request) = parse_spawn(&payload).map_err(protocol_failure)?;
                    if pending.contains_key(&task_id) {
                        return Err((FailureKind::Protocol, "duplicate task id".into()));
                    }
                    let call_id =
                        format!("native-{}-a{}-{task_id}", artifact.run_id, artifact.attempt);
                    write_call_artifact(
                        artifact,
                        &call_id,
                        "request.bin",
                        &payload,
                        &state.raw_artifact_bytes,
                    )
                    .map_err(|error| (FailureKind::CallLimit, error.to_string()))?;
                    let first = state.first_capability.set(started.elapsed()).is_ok();
                    if first {
                        update_process_usage(state.workflow_pid, &state.workflow_usage);
                        self.event(artifact, HostEventKind::FirstCapability)
                            .map_err(|error| (FailureKind::Cleanup, error.diagnostic))?;
                    }
                    if let Some(pid) = verified_descendant_pid(&request, state.workflow_pid) {
                        let admitted = {
                            let mut descendants = state
                                .descendants
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if descendants.len() >= TOTAL_CALLS as usize
                                || descendants.contains(&pid)
                            {
                                false
                            } else {
                                descendants.push(pid);
                                true
                            }
                        };
                        if admitted {
                            self.event(artifact, HostEventKind::DescendantPid(pid))
                                .map_err(|error| (FailureKind::Cleanup, error.diagnostic))?;
                        }
                    }
                    let (result_tx, result_rx) = oneshot::channel();
                    let task = spawn_capability(CapabilityJob {
                        task_id,
                        run_id: artifact.run_id.clone(),
                        attempt: artifact.attempt,
                        request,
                        delegate: Arc::clone(&self.delegate),
                        semaphore: Arc::clone(&state.semaphore),
                        active: Arc::clone(&state.active),
                        peak: Arc::clone(&state.peak),
                        owned: Arc::clone(&state.owned),
                        cancellation: cancellation.clone(),
                        result_tx,
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
                    let mut artifact_result = Vec::new();
                    artifact_result.push(result.status);
                    put_bytes(&mut artifact_result, result.call_id.as_bytes())
                        .map_err(protocol_failure)?;
                    put_bytes(&mut artifact_result, &result.value).map_err(protocol_failure)?;
                    write_call_artifact(
                        artifact,
                        &result.call_id,
                        "result.bin",
                        &artifact_result,
                        &state.raw_artifact_bytes,
                    )
                    .map_err(|error| (FailureKind::CallLimit, error.to_string()))?;
                    let mut response =
                        Vec::with_capacity(9 + result.call_id.len() + result.value.len());
                    response.push(result.status);
                    put_bytes(&mut response, result.call_id.as_bytes())
                        .map_err(protocol_failure)?;
                    put_bytes(&mut response, &result.value).map_err(protocol_failure)?;
                    write_frame(writer, OUTCOME, &response)
                        .await
                        .map_err(protocol_failure)?;
                    joined_calls.insert(result.call_id);
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
                        return Err(finish_protocol_failure(
                            reader,
                            writer,
                            "finish with unjoined capability tasks",
                        )
                        .await);
                    }
                    if payload.is_empty() {
                        return Err(finish_protocol_failure(
                            reader,
                            writer,
                            "final evidence is empty",
                        )
                        .await);
                    }
                    let mut evidence = match decode_evidence(&payload) {
                        Ok(evidence) => evidence,
                        Err(_) => {
                            return Err(finish_protocol_failure(
                                reader,
                                writer,
                                "final evidence schema is invalid",
                            )
                            .await);
                        }
                    };
                    if let Err(error) = validate_and_normalize_evidence_artifacts(
                        &artifact.run_dir,
                        &joined_calls,
                        &mut evidence,
                    ) {
                        return Err(
                            finish_protocol_failure(reader, writer, &error.to_string()).await
                        );
                    }
                    let payload = match encode_validated_evidence(&evidence) {
                        Ok(payload) => payload,
                        Err(error) => {
                            return Err(finish_protocol_failure(
                                reader,
                                writer,
                                &error.to_string(),
                            )
                            .await);
                        }
                    };
                    if payload.len() > FINAL_EVIDENCE_BYTES {
                        return Err((
                            FailureKind::EvidenceLimit,
                            "host-verified final evidence exceeds limit".into(),
                        ));
                    }
                    write_evidence_artifact(artifact, &evidence)
                        .map_err(|error| (FailureKind::Cleanup, error.to_string()))?;
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
    raw_artifact_bytes: Arc<AtomicUsize>,
}

struct IpcIo<'a, R, W> {
    reader: &'a mut R,
    writer: &'a mut W,
}

struct CapabilityResult {
    status: u8,
    call_id: String,
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
    run_id: String,
    attempt: u8,
    request: NativeRequest,
    delegate: Arc<dyn NativeCapabilityDelegate>,
    semaphore: Arc<Semaphore>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    owned: Arc<AtomicUsize>,
    cancellation: CancellationToken,
    result_tx: oneshot::Sender<CapabilityResult>,
}

fn spawn_capability(job: CapabilityJob) -> JoinHandle<()> {
    let CapabilityJob {
        task_id,
        run_id,
        attempt,
        request,
        delegate,
        semaphore,
        active,
        peak,
        owned,
        cancellation,
        result_tx,
    } = job;
    owned.fetch_add(1, Ordering::AcqRel);
    let owned_guard = OwnedTaskGuard {
        owned: Arc::clone(&owned),
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
        let call_id = format!("native-{run_id}-a{attempt}-{task_id}");
        let outcome = tokio::select! {
            _ = cancellation.cancelled() => return,
            outcome = delegate.invoke(
                NativeCall { call_id: call_id.clone(), request },
                cancellation.clone(),
            ) => outcome,
        };
        drop(permit);
        let (status, mut value) = match outcome {
            Ok(NativeOutcome::Success(value)) => (0, value),
            Ok(NativeOutcome::Retry(reason)) => (1, reason.into_bytes()),
            Ok(NativeOutcome::Failure(message)) | Err(message) => (2, message.into_bytes()),
        };
        if value.len() > CALL_OUTPUT_BYTES {
            value = b"native delegate output exceeded limit".to_vec();
            let _ = result_tx.send(CapabilityResult {
                status: 2,
                call_id,
                value,
            });
            return;
        }
        let _ = result_tx.send(CapabilityResult {
            status,
            call_id,
            value,
        });
    })
}

struct OwnedTaskGuard {
    owned: Arc<AtomicUsize>,
}
impl Drop for OwnedTaskGuard {
    fn drop(&mut self) {
        self.owned.fetch_sub(1, Ordering::AcqRel);
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

async fn discover_pinned_compiler() -> Result<PathBuf, RunFailure> {
    let mut command = Command::new("rustup");
    command
        .process_group(0)
        .kill_on_drop(true)
        .args([
            "which",
            "--toolchain",
            "1.95.0-aarch64-apple-darwin",
            "rustc",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        failure(
            FailureKind::CompilerUnavailable,
            &format!("rustup is required to resolve pinned rustc 1.95.0: {error}"),
            String::new(),
        )
    })?;
    let pid = child.id().unwrap_or(0);
    let mut group_guard = ProcessGroupGuard::new(pid);
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let cleanup = terminate_group(&mut child, &mut group_guard).await;
        return Err(failure_after_cleanup(
            FailureKind::CompilerUnavailable,
            "rustup compiler discovery pipes were unavailable",
            String::new(),
            cleanup,
        ));
    };
    let supervised = {
        let stdout_read = read_bounded_owned(stdout, 4096);
        let stderr_read = read_bounded_owned(stderr, 4096);
        let child_and_output = async { tokio::join!(child.wait(), stdout_read, stderr_read) };
        tokio::pin!(child_and_output);
        tokio::time::timeout(COMPILER_PROBE_TIMEOUT, &mut child_and_output).await
    };
    let (status, stdout, stderr) = match supervised {
        Ok((Ok(status), Ok(stdout), Ok(stderr))) => (status, stdout, stderr),
        Ok((wait, stdout, stderr)) => {
            let cleanup = terminate_group(&mut child, &mut group_guard).await;
            let detail = format!(
                "rustup compiler discovery failed: wait={wait:?}; stdout={stdout:?}; stderr={stderr:?}"
            );
            return Err(failure_after_cleanup(
                FailureKind::CompilerUnavailable,
                &detail,
                String::new(),
                cleanup,
            ));
        }
        Err(_) => {
            let cleanup = terminate_group(&mut child, &mut group_guard).await;
            return Err(failure_after_cleanup(
                FailureKind::CompilerUnavailable,
                "rustup compiler discovery timed out",
                String::new(),
                cleanup,
            ));
        }
    };
    settle_reaped_group(&mut group_guard);
    if !status.success() {
        return Err(process_failure(
            FailureKind::CompilerUnavailable,
            &format!(
                "pinned rustc 1.95.0-aarch64-apple-darwin is unavailable: {}",
                bounded_text(&stderr, 4096)
            ),
            String::new(),
            true,
        ));
    }
    let raw = std::str::from_utf8(&stdout).map_err(|_| {
        process_failure(
            FailureKind::CompilerUnavailable,
            "rustup returned a non-UTF-8 compiler path",
            String::new(),
            true,
        )
    })?;
    let path = PathBuf::from(raw.trim());
    let canonical = std::fs::canonicalize(&path).map_err(|error| {
        process_failure(
            FailureKind::CompilerUnavailable,
            &format!("failed to canonicalize rustup compiler path: {error}"),
            String::new(),
            true,
        )
    })?;
    if !canonical.is_file() {
        return Err(process_failure(
            FailureKind::CompilerUnavailable,
            "rustup compiler path is not a file",
            String::new(),
            true,
        ));
    }
    Ok(canonical)
}

async fn verify_compiler(rustc: &Path) -> Result<String, RunFailure> {
    let mut command = Command::new(rustc);
    command
        .process_group(0)
        .kill_on_drop(true)
        .arg("-vV")
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
    Ok(stdout)
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
    let process_lock = match try_advisory_lock(
        &artifact.identity.run_dir.join(".active.lock"),
        LockMode::Exclusive,
    ) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            state.active_executions = 0;
            return Err(failure(
                FailureKind::Admission,
                "run artifact already has an execution owner in another process",
                artifact.identity.source_hash.clone(),
            ));
        }
        Err(error) => {
            state.active_executions = 0;
            return Err(failure(
                FailureKind::Admission,
                &format!("failed to acquire run execution lock: {error}"),
                artifact.identity.source_hash.clone(),
            ));
        }
    };
    drop(state);
    Ok(ExecutionLease {
        identity: Arc::clone(&artifact.identity),
        process_lock: Some(process_lock),
        complete_on_drop: true,
        settled: false,
    })
}

fn validate_artifact_paths(artifact: &ArtifactIdentity, host_root: &Path) -> Result<(), String> {
    if artifact.owned_root != host_root {
        return Err("run artifact belongs to a different host root".into());
    }
    let expected_runs_dir = host_root
        .join("sessions")
        .join(&artifact.thread_id)
        .join("runs");
    let expected_run_dir = expected_runs_dir.join(&artifact.run_id);
    let expected_attempt_dir = expected_run_dir.join(format!("attempt-{}", artifact.attempt));
    if artifact.run_dir != expected_run_dir
        || artifact.source_path != expected_attempt_dir.join("source.rs")
        || artifact.binary_path != expected_attempt_dir.join("workflow")
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
    let canonical_runs = std::fs::canonicalize(&expected_runs_dir)
        .map_err(|error| format!("runs directory is unavailable: {error}"))?;
    if canonical_run != artifact.run_dir || canonical_run.parent() != Some(canonical_runs.as_path())
    {
        return Err("run directory escaped its host-owned canonical root".into());
    }
    let canonical_source = std::fs::canonicalize(&artifact.source_path)
        .map_err(|error| format!("retained source is unavailable: {error}"))?;
    if canonical_source != artifact.source_path || !canonical_source.is_file() {
        return Err("retained source path is not the exact host-owned file".into());
    }
    Ok(())
}

fn cleanup_owned_artifact(artifact: &ArtifactIdentity) -> io::Result<()> {
    validate_artifact_paths_for_cleanup(artifact)?;
    std::fs::remove_dir_all(&artifact.run_dir)
}

fn validate_artifact_paths_for_cleanup(artifact: &ArtifactIdentity) -> io::Result<()> {
    let expected_runs_dir = artifact
        .owned_root
        .join("sessions")
        .join(&artifact.thread_id)
        .join("runs");
    let expected_run_dir = expected_runs_dir.join(&artifact.run_id);
    let expected_attempt_dir = expected_run_dir.join(format!("attempt-{}", artifact.attempt));
    if artifact.run_dir != expected_run_dir
        || artifact.source_path != expected_attempt_dir.join("source.rs")
        || artifact.binary_path != expected_attempt_dir.join("workflow")
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cleanup rejected paths outside the immutable run identity",
        ));
    }
    let root = std::fs::canonicalize(&artifact.owned_root)?;
    let runs = std::fs::canonicalize(expected_runs_dir)?;
    let run = std::fs::canonicalize(&artifact.run_dir)?;
    if root != artifact.owned_root || run != artifact.run_dir || run.parent() != Some(&runs) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cleanup rejected a run directory outside the canonical owned root",
        ));
    }
    Ok(())
}

fn validate_identity(label: &str, value: &str) -> Result<(), String> {
    let parsed = uuid::Uuid::parse_str(value)
        .map_err(|_| format!("{label} identity must be a canonical UUID"))?;
    if parsed.hyphenated().to_string() != value {
        return Err(format!(
            "{label} identity must be a lowercase canonical UUID"
        ));
    }
    Ok(())
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

fn create_private_dir_all(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private directory path is not a real directory",
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

fn replace_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no parent"))?;
    let name = format!(
        ".native-tmp-{}",
        NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
    );
    let temporary = parent.join(name);
    write_private_file(&temporary, bytes)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })?;
    std::fs::File::open(parent)?.sync_all()
}

fn write_json_private(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    write_private_file(path, &bytes)
}

fn read_manifest(path: &Path) -> io::Result<RunManifest> {
    let bytes = read_bounded_regular_file(path, RUN_MANIFEST_BYTES)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn read_cache_manifest(path: &Path) -> io::Result<CacheManifest> {
    let bytes = read_bounded_regular_file(path, CACHE_MANIFEST_BYTES)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn open_bounded_regular_file(path: &Path, max_bytes: u64) -> io::Result<(std::fs::File, u64)> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "path is not a regular file",
        ));
    }
    if metadata.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file exceeds fixed {max_bytes}-byte cap"),
        ));
    }
    Ok((file, metadata.len()))
}

fn read_bounded_regular_file(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let (file, initial_len) = open_bounded_regular_file(path, max_bytes)?;
    let mut bytes = Vec::with_capacity(initial_len as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file grew beyond fixed {max_bytes}-byte cap"),
        ));
    }
    Ok(bytes)
}

fn hash_bounded_regular_file(path: &Path, max_bytes: u64) -> io::Result<(String, u64)> {
    hash_bounded_regular_file_with_mode(path, max_bytes, None)
}

fn hash_bounded_regular_file_with_mode(
    path: &Path,
    max_bytes: u64,
    set_mode: Option<u32>,
) -> io::Result<(String, u64)> {
    let (mut file, _) = open_bounded_regular_file(path, max_bytes)?;
    if let Some(mode) = set_mode {
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("file grew beyond fixed {max_bytes}-byte cap"),
            ));
        }
        digest.update(&buffer[..read]);
    }
    Ok((format!("{:x}", digest.finalize()), total))
}

fn validate_existing_run(root: &Path, thread_id: &str, run_id: &str, task: &str) -> io::Result<()> {
    let run_dir = root
        .join("sessions")
        .join(thread_id)
        .join("runs")
        .join(run_id);
    let canonical = std::fs::canonicalize(&run_dir)?;
    if canonical != run_dir {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "run path changed identity",
        ));
    }
    let manifest = read_manifest(&run_dir.join("manifest.json"))?;
    if manifest.version != 1
        || manifest.thread_id != thread_id
        || manifest.run_id != run_id
        || manifest.task_hash != source_hash(task.as_bytes())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "run manifest does not match repair identity",
        ));
    }
    Ok(())
}

fn mark_run_completed(run_dir: &Path) -> io::Result<()> {
    let manifest_path = run_dir.join("manifest.json");
    let mut manifest = read_manifest(&manifest_path)?;
    manifest.completed_at_unix_ms = Some(unix_millis());
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
    replace_private_file(&manifest_path, &bytes)
}

fn cleanup_exact_run(root: &Path, thread_id: &str, run_id: &str) -> io::Result<()> {
    validate_identity("thread", thread_id).map_err(io::Error::other)?;
    validate_identity("run", run_id).map_err(io::Error::other)?;
    let runs = root.join("sessions").join(thread_id).join("runs");
    let run = runs.join(run_id);
    let canonical_root = std::fs::canonicalize(root)?;
    let canonical_runs = std::fs::canonicalize(&runs)?;
    let canonical_run = std::fs::canonicalize(&run)?;
    if canonical_root != root
        || canonical_run != run
        || canonical_run.parent() != Some(&canonical_runs)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cleanup containment check failed",
        ));
    }
    std::fs::remove_dir_all(run)
}

#[derive(Debug)]
struct RetainedRun {
    completed: u64,
    thread_id: String,
    run_id: String,
    bytes: u64,
    retention_lock: Option<AdvisoryLock>,
}

fn enforce_run_retention(root: &Path, incoming_thread: &str) -> io::Result<()> {
    let mut runs = collect_retained_runs(root)?;
    let mut total_bytes = runs.iter().map(|run| run.bytes).sum::<u64>();
    loop {
        let thread_count = runs
            .iter()
            .filter(|run| run.thread_id == incoming_thread)
            .count();
        if thread_count < RUNS_PER_THREAD
            && runs.len() < RUNS_GLOBAL
            && total_bytes.saturating_add(RUN_RESERVATION_BYTES) <= RUN_ARTIFACT_BYTES
        {
            return Ok(());
        }
        runs.sort_by(|left, right| {
            (left.completed, &left.thread_id, &left.run_id).cmp(&(
                right.completed,
                &right.thread_id,
                &right.run_id,
            ))
        });
        let index = runs.iter().position(|run| {
            run.retention_lock.is_some()
                && run.completed != u64::MAX
                && (thread_count >= RUNS_PER_THREAD && run.thread_id == incoming_thread
                    || runs.len() >= RUNS_GLOBAL
                    || total_bytes.saturating_add(RUN_RESERVATION_BYTES) > RUN_ARTIFACT_BYTES)
        });
        let Some(index) = index else {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "retention caps cannot be met without evicting an active run",
            ));
        };
        let evicted = runs.remove(index);
        cleanup_exact_run(root, &evicted.thread_id, &evicted.run_id)?;
        total_bytes = total_bytes.saturating_sub(evicted.bytes);
    }
}

fn collect_retained_runs(root: &Path) -> io::Result<Vec<RetainedRun>> {
    let sessions = root.join("sessions");
    let mut output = Vec::new();
    for thread_entry in std::fs::read_dir(sessions)? {
        let thread_entry = thread_entry?;
        if !thread_entry.file_type()?.is_dir() {
            continue;
        }
        let thread_id = thread_entry.file_name().to_string_lossy().into_owned();
        if validate_identity("thread", &thread_id).is_err() {
            continue;
        }
        let runs = thread_entry.path().join("runs");
        let Ok(entries) = std::fs::read_dir(runs) else {
            continue;
        };
        for run_entry in entries {
            let run_entry = run_entry?;
            if !run_entry.file_type()?.is_dir() {
                continue;
            }
            let run_id = run_entry.file_name().to_string_lossy().into_owned();
            if validate_identity("run", &run_id).is_err() {
                continue;
            }
            let path = run_entry.path();
            let manifest = read_manifest(&path.join("manifest.json"))?;
            let retention_lock =
                try_advisory_lock(&path.join(".active.lock"), LockMode::Exclusive)?;
            output.push(RetainedRun {
                completed: manifest.completed_at_unix_ms.unwrap_or(u64::MAX),
                thread_id: thread_id.clone(),
                run_id,
                bytes: directory_bytes(&path)?,
                retention_lock,
            });
        }
    }
    Ok(output)
}

pub fn finalize_run(root: &Path, thread_id: &str, run_id: &str) -> io::Result<()> {
    validate_identity("thread", thread_id).map_err(io::Error::other)?;
    validate_identity("run", run_id).map_err(io::Error::other)?;
    let root = std::fs::canonicalize(root)?;
    let run_dir = root
        .join("sessions")
        .join(thread_id)
        .join("runs")
        .join(run_id);
    let canonical_run = std::fs::canonicalize(&run_dir)?;
    let canonical_runs = std::fs::canonicalize(root.join("sessions").join(thread_id).join("runs"))?;
    if canonical_run != run_dir || canonical_run.parent() != Some(&canonical_runs) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "finalize rejected a run outside the canonical owned root",
        ));
    }
    let _lock = try_advisory_lock(&run_dir.join(".active.lock"), LockMode::Exclusive)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "run is still active"))?;
    mark_run_completed(&run_dir)
}

fn retain_diagnostic(artifact: &ArtifactIdentity, bytes: &[u8]) -> io::Result<()> {
    let bounded = bounded_text(bytes, DIAGNOSTIC_BYTES);
    let path = artifact
        .run_dir
        .join(format!("attempt-{}/rustc.stderr", artifact.attempt));
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "diagnostic path is not an owned file",
        ));
    }
    replace_private_file(&path, bounded.as_bytes())
}

pub fn materialize_sdk(root: &Path, bytes: &[u8], expected_hash: &str) -> io::Result<PathBuf> {
    if bytes.len() as u64 > SDK_RLIB_BYTES
        || source_hash(bytes) != expected_hash
        || !is_sha256(expected_hash)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "embedded SDK hash mismatch",
        ));
    }
    create_private_dir_all(root)?;
    let root = std::fs::canonicalize(root)?;
    let sdk_dir = root.join("cache/sdk");
    create_private_dir_all(&sdk_dir)?;
    let path = sdk_dir.join(format!("libycode_native_sdk-{expected_hash}.rlib"));
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "materialized SDK path is not an owned regular file",
                ));
            }
            if metadata.len() > SDK_RLIB_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "materialized SDK exceeds fixed size cap",
                ));
            }
            let (existing_hash, _) = hash_bounded_regular_file(&path, SDK_RLIB_BYTES)?;
            if metadata.permissions().mode() & 0o777 == 0o600 && existing_hash == expected_hash {
                return Ok(path);
            }
            std::fs::remove_file(&path)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let temporary = sdk_dir.join(format!(
        ".tmp-{expected_hash}-{}",
        NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
    ));
    write_private_file(&temporary, bytes)?;
    std::fs::rename(&temporary, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })?;
    std::fs::File::open(&sdk_dir)?.sync_all()?;
    let (materialized_hash, materialized_len) = hash_bounded_regular_file(&path, SDK_RLIB_BYTES)?;
    if materialized_len != bytes.len() as u64 || materialized_hash != expected_hash {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "materialized SDK hash mismatch",
        ));
    }
    Ok(path)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn write_cache_use_stamp(root: &Path, key: &str) -> io::Result<()> {
    if !is_sha256(key) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid cache key",
        ));
    }
    let path = root.join("cache/locks").join(format!("{key}.used"));
    replace_private_file(&path, format!("{}\n", unix_millis()).as_bytes())
}

#[derive(Debug)]
struct CacheEntry {
    key: String,
    path: PathBuf,
    used: u64,
    bytes: u64,
}

fn enforce_cache_retention(
    root: &Path,
    incoming_entries: usize,
    incoming_bytes: u64,
) -> io::Result<()> {
    let objects = root.join("cache/objects");
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&objects)? {
        let entry = entry?;
        let key = entry.file_name().to_string_lossy().into_owned();
        if !is_sha256(&key) || !entry.file_type()?.is_dir() {
            continue;
        }
        let used_path = root.join("cache/locks").join(format!("{key}.used"));
        let used = read_bounded_regular_file(&used_path, CACHE_STAMP_BYTES)
            .ok()
            .and_then(|value| std::str::from_utf8(&value).ok()?.trim().parse().ok())
            .unwrap_or(0);
        entries.push(CacheEntry {
            key,
            bytes: directory_bytes(&entry.path())?,
            path: entry.path(),
            used,
        });
    }
    let mut total = entries.iter().map(|entry| entry.bytes).sum::<u64>();
    while entries.len().saturating_add(incoming_entries) > CACHE_ENTRIES
        || total.saturating_add(incoming_bytes) > CACHE_BYTES
    {
        entries.sort_by(|left, right| (left.used, &left.key).cmp(&(right.used, &right.key)));
        let mut selected = None;
        for (index, entry) in entries.iter().enumerate() {
            let lock_path = root.join("cache/locks").join(format!("{}.lock", entry.key));
            if let Some(lock) = try_advisory_lock(&lock_path, LockMode::Exclusive)? {
                selected = Some((index, lock));
                break;
            }
        }
        let Some((index, _eviction_lock)) = selected else {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "cache cap cannot be met without evicting a locked entry",
            ));
        };
        let evicted = entries.remove(index);
        let canonical_objects = std::fs::canonicalize(&objects)?;
        let canonical_entry = std::fs::canonicalize(&evicted.path)?;
        if canonical_entry.parent() != Some(&canonical_objects) || canonical_entry != evicted.path {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cache eviction containment check failed",
            ));
        }
        std::fs::remove_dir_all(evicted.path)?;
        let _ = std::fs::remove_file(
            root.join("cache/locks")
                .join(format!("{}.used", evicted.key)),
        );
        total = total.saturating_sub(evicted.bytes);
    }
    Ok(())
}

fn assert_cache_postcondition(root: &Path) -> io::Result<()> {
    let objects = root.join("cache/objects");
    let mut entries = 0_usize;
    let mut bytes = 0_u64;
    for entry in std::fs::read_dir(objects)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || !is_sha256(&entry.file_name().to_string_lossy()) {
            continue;
        }
        entries = entries.saturating_add(1);
        bytes = bytes.saturating_add(directory_bytes(&entry.path())?);
    }
    if entries > CACHE_ENTRIES || bytes > CACHE_BYTES {
        return Err(io::Error::other(format!(
            "cache contains {entries} entries/{bytes} bytes after publication"
        )));
    }
    Ok(())
}

fn evict_corrupt_cache_object(root: &Path, key: &str) -> io::Result<()> {
    if !is_sha256(key) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid cache key",
        ));
    }
    let objects = root.join("cache/objects");
    let object = objects.join(key);
    let metadata = match std::fs::symlink_metadata(&object) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "corrupt cache object is not an owned directory",
        ));
    }
    let canonical_objects = std::fs::canonicalize(objects)?;
    let canonical_object = std::fs::canonicalize(&object)?;
    if canonical_object != object || canonical_object.parent() != Some(&canonical_objects) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "corrupt cache object escaped cache root",
        ));
    }
    std::fs::remove_dir_all(object)
}

fn directory_bytes(path: &Path) -> io::Result<u64> {
    let mut total = 0_u64;
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "symlink in native artifact store",
                ));
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

fn scrub_environment(command: &mut Command) {
    command.env_clear();
    command.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    command.env("TMPDIR", "/tmp");
    command.env("LANG", "C");
}

#[derive(Clone, Copy)]
enum LockMode {
    Shared,
    Exclusive,
}

impl LockMode {
    fn operation(self) -> libc::c_int {
        match self {
            Self::Shared => libc::LOCK_SH,
            Self::Exclusive => libc::LOCK_EX,
        }
    }
}

#[derive(Debug)]
struct AdvisoryLock {
    file: std::fs::File,
}

impl AdvisoryLock {
    fn downgrade(self) -> io::Result<Self> {
        // SAFETY: flock receives a valid owned file descriptor and does not outlive it.
        if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_SH) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(self)
    }
}

fn try_advisory_lock(path: &Path, mode: LockMode) -> io::Result<Option<AdvisoryLock>> {
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "advisory lock path is not a regular file",
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    // SAFETY: flock receives a valid owned file descriptor and does not outlive it.
    let result = unsafe { libc::flock(file.as_raw_fd(), mode.operation() | libc::LOCK_NB) };
    if result == 0 {
        return Ok(Some(AdvisoryLock { file }));
    }
    let error = io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
    {
        return Ok(None);
    }
    Err(error)
}

struct OwnedDirGuard {
    path: PathBuf,
    armed: bool,
}

impl OwnedDirGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OwnedDirGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.path);
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

async fn finish_protocol_failure(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    message: &str,
) -> (FailureKind, String) {
    match write_failure(writer, message).await {
        Ok(()) => {
            // Give the child a bounded opportunity to observe the terminal FAILURE frame and
            // close its protocol stream before process-group cleanup begins. No acknowledgement
            // frame exists; EOF is the only expected post-FINISH signal.
            let mut unexpected = [0_u8; 1];
            let _ = tokio::time::timeout(TERMINATE_GRACE, reader.read(&mut unexpected)).await;
            (FailureKind::Protocol, message.to_string())
        }
        Err(error) => protocol_failure(error),
    }
}

fn parse_spawn(payload: &[u8]) -> io::Result<(u32, NativeRequest)> {
    let mut cursor = PayloadCursor::new(payload);
    let task = cursor.u32()?;
    let request = match cursor.byte()? {
        1 => {
            let command = cursor.string()?;
            let workdir = match cursor.byte()? {
                0 => None,
                1 => Some(cursor.string()?),
                _ => return Err(invalid_data("invalid workdir tag")),
            };
            let timeout_ms = cursor.u32()?;
            NativeRequest::Shell {
                command,
                workdir,
                timeout_ms,
            }
        }
        2 => NativeRequest::ApplyPatch {
            patch: cursor.string()?,
        },
        _ => return Err(invalid_data("unknown native request tag")),
    };
    cursor.finish()?;
    Ok((task, request))
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

fn verified_descendant_pid(request: &NativeRequest, workflow_pid: u32) -> Option<u32> {
    let NativeRequest::Shell { command, .. } = request else {
        return None;
    };
    let pid = command.strip_prefix("descendant:")?.parse().ok()?;
    if pid == 0 || pid == workflow_pid || !process_exists(pid) {
        return None;
    }
    // SAFETY: `getpgid` only inspects kernel process metadata for the supplied integer PID.
    let process_group = unsafe { libc::getpgid(pid as libc::pid_t) };
    (process_group == workflow_pid as libc::pid_t).then_some(pid)
}

struct PayloadCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> PayloadCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn byte(&mut self) -> io::Result<u8> {
        let value = *self
            .bytes
            .get(self.offset)
            .ok_or_else(|| invalid_data("truncated payload"))?;
        self.offset += 1;
        Ok(value)
    }

    fn u16(&mut self) -> io::Result<u16> {
        let value = self.take(2)?;
        Ok(u16::from_le_bytes([value[0], value[1]]))
    }

    fn u32(&mut self) -> io::Result<u32> {
        let value = self.take(4)?;
        Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
    }

    fn take(&mut self, length: usize) -> io::Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| invalid_data("payload length overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| invalid_data("truncated payload"))?;
        self.offset = end;
        Ok(value)
    }

    fn bytes(&mut self) -> io::Result<&'a [u8]> {
        let length = self.u32()? as usize;
        self.take(length)
    }

    fn string(&mut self) -> io::Result<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| invalid_data("invalid UTF-8"))
    }

    fn finish(&self) -> io::Result<()> {
        (self.offset == self.bytes.len())
            .then_some(())
            .ok_or_else(|| invalid_data("trailing payload bytes"))
    }
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[derive(Debug, Serialize)]
pub struct ValidatedEvidence {
    pub version: u16,
    pub summary: String,
    pub verified: Vec<String>,
    pub disputed: Vec<String>,
    pub unresolved: Vec<String>,
    pub artifact_refs: Vec<String>,
    pub partial_failures: Vec<String>,
    pub provenance_ids: Vec<String>,
}

pub fn decode_evidence(payload: &[u8]) -> io::Result<ValidatedEvidence> {
    let mut cursor = PayloadCursor::new(payload);
    let version = cursor.u16()?;
    if version != VERSION {
        return Err(invalid_data("unsupported evidence version"));
    }
    let summary = decode_evidence_string(&mut cursor)?;
    let mut collections = Vec::with_capacity(6);
    for _ in 0..6 {
        let count = cursor.u32()? as usize;
        if count > MAX_EVIDENCE_ITEMS {
            return Err(invalid_data("too many evidence items"));
        }
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(decode_evidence_string(&mut cursor)?);
        }
        collections.push(values);
    }
    cursor.finish()?;
    let [
        verified,
        disputed,
        unresolved,
        artifact_refs,
        partial_failures,
        provenance_ids,
    ]: [Vec<String>; 6] = collections
        .try_into()
        .map_err(|_| invalid_data("invalid evidence collection count"))?;
    Ok(ValidatedEvidence {
        version,
        summary,
        verified,
        disputed,
        unresolved,
        artifact_refs,
        partial_failures,
        provenance_ids,
    })
}

fn encode_validated_evidence(evidence: &ValidatedEvidence) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    output.extend_from_slice(&evidence.version.to_le_bytes());
    put_bytes(&mut output, evidence.summary.as_bytes())?;
    for values in [
        &evidence.verified,
        &evidence.disputed,
        &evidence.unresolved,
        &evidence.artifact_refs,
        &evidence.partial_failures,
        &evidence.provenance_ids,
    ] {
        output.extend_from_slice(
            &u32::try_from(values.len())
                .map_err(|_| invalid_data("too many evidence items"))?
                .to_le_bytes(),
        );
        for value in values {
            put_bytes(&mut output, value.as_bytes())?;
        }
    }
    Ok(output)
}

fn validate_and_normalize_evidence_artifacts(
    run_dir: &Path,
    joined_calls: &HashSet<String>,
    evidence: &mut ValidatedEvidence,
) -> io::Result<()> {
    let mut provenance = HashSet::new();
    let mut verified_refs = BTreeSet::new();
    for call_id in &evidence.provenance_ids {
        if !provenance.insert(call_id.as_str()) {
            return Err(invalid_data("duplicate evidence provenance id"));
        }
        if !joined_calls.contains(call_id) {
            return Err(invalid_data(
                "evidence provenance id does not identify a joined native call",
            ));
        }
        for suffix in ["request.bin", "result.bin"] {
            let logical = format!("calls/{call_id}.{suffix}");
            validate_retained_artifact(run_dir, &logical)?;
            verified_refs.insert(logical);
        }
    }
    for requested in &evidence.artifact_refs {
        if !verified_refs.contains(requested) {
            return Err(invalid_data(
                "evidence artifact reference is not a verified native call artifact",
            ));
        }
    }
    evidence.artifact_refs = verified_refs.into_iter().collect();
    Ok(())
}

fn validate_retained_artifact(run_dir: &Path, logical: &str) -> io::Result<()> {
    let path = run_dir.join(logical);
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "evidence artifact is not a retained regular file",
        ));
    }
    let canonical_run = std::fs::canonicalize(run_dir)?;
    let canonical_path = std::fs::canonicalize(&path)?;
    if canonical_run != run_dir || canonical_path != path || !canonical_path.starts_with(run_dir) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "evidence artifact escaped the canonical run directory",
        ));
    }
    Ok(())
}

fn decode_evidence_string(cursor: &mut PayloadCursor<'_>) -> io::Result<String> {
    let bytes = cursor.bytes()?;
    if bytes.len() > MAX_EVIDENCE_STRING_BYTES {
        return Err(invalid_data("invalid evidence string"));
    }
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| invalid_data("invalid evidence string"))
}

fn write_evidence_artifact(
    artifact: &ArtifactIdentity,
    evidence: &ValidatedEvidence,
) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(evidence).map_err(io::Error::other)?;
    if bytes.len() > EVIDENCE_ARTIFACT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "serialized local evidence artifact exceeds cap",
        ));
    }
    write_private_file(&artifact.run_dir.join("evidence.json"), &bytes)
}

fn write_call_artifact(
    artifact: &ArtifactIdentity,
    call_id: &str,
    suffix: &str,
    bytes: &[u8],
    total: &AtomicUsize,
) -> io::Result<()> {
    if bytes.len() > FRAME_BYTES + 1024
        || !call_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(invalid_data(
            "call artifact exceeds per-call bound or has invalid identity",
        ));
    }
    let mut observed = total.load(Ordering::Acquire);
    loop {
        let next = observed
            .checked_add(bytes.len())
            .ok_or_else(|| invalid_data("raw call artifact byte count overflow"))?;
        if next > RAW_CALL_ARTIFACT_BYTES {
            return Err(invalid_data("raw call artifacts exceed per-run cap"));
        }
        match total.compare_exchange_weak(observed, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(actual) => observed = actual,
        }
    }
    write_private_file(
        &artifact
            .run_dir
            .join("calls")
            .join(format!("{call_id}.{suffix}")),
        bytes,
    )
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
        repair_pending: false,
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
        repair_pending: false,
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
        repair_pending: false,
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
    use std::os::unix::process::CommandExt as _;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn bounded_host_event_capacity_covers_the_complete_execution_set() {
        assert_eq!(HOST_EVENT_MAX_PER_EXECUTION, 37);
        assert!(HOST_EVENT_MAX_PER_EXECUTION <= HOST_EVENT_CHANNEL_CAPACITY);
    }

    #[test]
    fn descendant_hint_requires_a_live_nonleader_in_the_owned_process_group() {
        // SAFETY: getpgrp reads the process group of the current test process.
        let owned_group = unsafe { libc::getpgrp() } as u32;
        let mut owned_descendant = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let owned_request = NativeRequest::Shell {
            command: format!("descendant:{}", owned_descendant.id()),
            workdir: None,
            timeout_ms: 1,
        };
        assert_eq!(
            verified_descendant_pid(&owned_request, owned_group),
            Some(owned_descendant.id())
        );

        let mut unrelated = std::process::Command::new("/bin/sleep");
        unrelated.arg("30").process_group(0);
        let mut unrelated = unrelated.spawn().unwrap();
        let unrelated_request = NativeRequest::Shell {
            command: format!("descendant:{}", unrelated.id()),
            workdir: None,
            timeout_ms: 1,
        };
        assert_eq!(
            verified_descendant_pid(&unrelated_request, owned_group),
            None
        );
        assert_eq!(
            verified_descendant_pid(
                &NativeRequest::Shell {
                    command: "descendant:4294967295".to_string(),
                    workdir: None,
                    timeout_ms: 1,
                },
                owned_group,
            ),
            None
        );
        assert_eq!(
            verified_descendant_pid(
                &NativeRequest::Shell {
                    command: format!("descendant:{owned_group}"),
                    workdir: None,
                    timeout_ms: 1,
                },
                owned_group,
            ),
            None
        );

        owned_descendant.kill().unwrap();
        owned_descendant.wait().unwrap();
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
    }

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

    #[tokio::test]
    async fn finish_protocol_failure_replies_before_settling() {
        let (host, mut child) = tokio::io::duplex(256);
        let (mut host_reader, mut host_writer) = tokio::io::split(host);
        let failure = finish_protocol_failure(
            &mut host_reader,
            &mut host_writer,
            "evidence provenance id does not identify a joined native call",
        );
        tokio::pin!(failure);
        let (kind, payload, _) = tokio::select! {
            frame = read_frame(&mut child, FRAME_BYTES + HEADER_BYTES) => frame.unwrap(),
            _ = &mut failure => panic!("protocol failure settled before the child read it"),
        };
        drop(child);
        let failure = failure.await;
        assert_eq!(failure.0, FailureKind::Protocol);
        assert_eq!(
            failure.1,
            "evidence provenance id does not identify a joined native call"
        );
        assert_eq!(kind, FAILURE);
        assert_eq!(
            payload,
            b"evidence provenance id does not identify a joined native call"
        );
    }

    #[test]
    fn evidence_requires_unique_joined_provenance_and_owned_regular_call_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = std::fs::canonicalize(temp.path()).unwrap();
        create_private_dir(&run_dir.join("calls")).unwrap();
        let call_id = "native-00000000-0000-4000-8000-000000000002-a1-1";
        let request = format!("calls/{call_id}.request.bin");
        let result = format!("calls/{call_id}.result.bin");
        write_private_file(&run_dir.join(&request), b"request").unwrap();
        write_private_file(&run_dir.join(&result), b"result").unwrap();
        let joined = HashSet::from([call_id.to_string()]);
        let evidence = || ValidatedEvidence {
            version: VERSION,
            summary: "verified".to_string(),
            verified: Vec::new(),
            disputed: Vec::new(),
            unresolved: Vec::new(),
            artifact_refs: Vec::new(),
            partial_failures: Vec::new(),
            provenance_ids: vec![call_id.to_string()],
        };

        let mut valid = evidence();
        validate_and_normalize_evidence_artifacts(&run_dir, &joined, &mut valid).unwrap();
        assert_eq!(valid.artifact_refs, [request, result.clone()]);

        let mut duplicate = evidence();
        duplicate.provenance_ids.push(call_id.to_string());
        assert!(
            validate_and_normalize_evidence_artifacts(&run_dir, &joined, &mut duplicate)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );

        let mut unknown = evidence();
        unknown.provenance_ids = vec!["native-unknown-a1-7".to_string()];
        assert!(
            validate_and_normalize_evidence_artifacts(&run_dir, &joined, &mut unknown)
                .unwrap_err()
                .to_string()
                .contains("joined")
        );

        let mut traversal = evidence();
        traversal.artifact_refs = vec!["../outside".to_string()];
        assert!(
            validate_and_normalize_evidence_artifacts(&run_dir, &joined, &mut traversal)
                .unwrap_err()
                .to_string()
                .contains("not a verified")
        );

        std::fs::remove_file(run_dir.join(&result)).unwrap();
        let mut nonexistent = evidence();
        assert_eq!(
            validate_and_normalize_evidence_artifacts(&run_dir, &joined, &mut nonexistent)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );

        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("outside");
        std::fs::write(&outside, b"outside must remain unchanged").unwrap();
        std::os::unix::fs::symlink(&outside, run_dir.join(&result)).unwrap();
        let mut symlink = evidence();
        assert_eq!(
            validate_and_normalize_evidence_artifacts(&run_dir, &joined, &mut symlink)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"outside must remain unchanged"
        );
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
    fn cache_eviction_is_strict_deterministic_and_skips_locked_oldest() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        create_private_dir_all(&root.join("cache/objects")).unwrap();
        create_private_dir_all(&root.join("cache/locks")).unwrap();
        let mut keys = Vec::new();
        for index in 0..=CACHE_ENTRIES {
            let key = source_hash(format!("entry-{index}").as_bytes());
            let object = root.join("cache/objects").join(&key);
            create_private_dir(&object).unwrap();
            write_private_file(&object.join("workflow"), b"binary").unwrap();
            replace_private_file(
                &root.join("cache/locks").join(format!("{key}.used")),
                format!("{index}\n").as_bytes(),
            )
            .unwrap();
            keys.push(key);
        }
        use std::io::BufRead as _;
        let locked_path = root.join("cache/locks").join(format!("{}.lock", keys[0]));
        let mut owner = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::advisory_lock_process_helper",
                "--nocapture",
            ])
            .env("YCODE_NATIVE_LOCK_HELPER_PATH", &locked_path)
            .env("YCODE_NATIVE_LOCK_HELPER_MODE", "shared")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = std::io::BufReader::new(owner.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(output.read_line(&mut line).unwrap(), 0);
            if line.contains("YCODE_NATIVE_LOCKED") {
                break;
            }
        }
        enforce_cache_retention(&root, 0, 0).unwrap();
        assert!(root.join("cache/objects").join(&keys[0]).exists());
        assert!(!root.join("cache/objects").join(&keys[1]).exists());
        assert_eq!(
            std::fs::read_dir(root.join("cache/objects"))
                .unwrap()
                .count(),
            CACHE_ENTRIES
        );
        owner.kill().unwrap();
        owner.wait().unwrap();
        enforce_cache_retention(&root, 1, 0).unwrap();
        assert!(!root.join("cache/objects").join(&keys[0]).exists());
    }

    #[test]
    fn cache_reservation_uses_exact_candidate_bytes_before_publication() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        create_private_dir_all(&root.join("cache/objects")).unwrap();
        create_private_dir_all(&root.join("cache/locks")).unwrap();
        let key = source_hash(b"large-existing-object");
        let object = root.join("cache/objects").join(&key);
        create_private_dir(&object).unwrap();
        let existing = std::fs::File::create(object.join("workflow")).unwrap();
        existing.set_len(CACHE_BYTES - 1).unwrap();
        drop(existing);
        let lock_path = root.join("cache/locks").join(format!("{key}.lock"));
        let use_lock = try_advisory_lock(&lock_path, LockMode::Shared)
            .unwrap()
            .unwrap();
        assert_eq!(
            enforce_cache_retention(&root, 1, WORKFLOW_EXECUTABLE_BYTES)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(object.exists(), "locked object is never evicted");
        drop(use_lock);
        enforce_cache_retention(&root, 1, WORKFLOW_EXECUTABLE_BYTES).unwrap();
        assert!(!object.exists());
        assert_cache_postcondition(&root).unwrap();
    }

    #[test]
    fn advisory_lock_process_helper() {
        let Some(path) = std::env::var_os("YCODE_NATIVE_LOCK_HELPER_PATH") else {
            return;
        };
        let mode = match std::env::var("YCODE_NATIVE_LOCK_HELPER_MODE").as_deref() {
            Ok("shared") => LockMode::Shared,
            _ => LockMode::Exclusive,
        };
        let _lock = try_advisory_lock(Path::new(&path), mode)
            .unwrap()
            .expect("helper acquires lock");
        println!("YCODE_NATIVE_LOCKED");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    fn kernel_locks_release_on_process_death_and_ignore_stale_files() {
        use std::io::BufRead as _;

        let root = tempfile::tempdir().unwrap();
        let same_process = root.path().join("same-process.lock");
        let held = try_advisory_lock(&same_process, LockMode::Exclusive)
            .unwrap()
            .unwrap();
        assert!(
            try_advisory_lock(&same_process, LockMode::Exclusive)
                .unwrap()
                .is_none(),
            "separate file descriptions in one host cannot bypass ownership"
        );
        drop(held);
        for mode in ["exclusive", "shared"] {
            let path = root.path().join(format!("{mode}.lock"));
            write_private_file(&path, b"stale marker content\n").unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::advisory_lock_process_helper",
                    "--nocapture",
                ])
                .env("YCODE_NATIVE_LOCK_HELPER_PATH", &path)
                .env("YCODE_NATIVE_LOCK_HELPER_MODE", mode)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                assert_ne!(output.read_line(&mut line).unwrap(), 0);
                if line.contains("YCODE_NATIVE_LOCKED") {
                    break;
                }
            }
            assert!(
                try_advisory_lock(&path, LockMode::Exclusive)
                    .unwrap()
                    .is_none(),
                "genuinely held {mode} lock prevents eviction/execution"
            );
            child.kill().unwrap();
            child.wait().unwrap();
            assert!(path.exists(), "lock pathname may remain as a stale inode");
            assert!(
                try_advisory_lock(&path, LockMode::Exclusive)
                    .unwrap()
                    .is_some(),
                "kernel releases {mode} ownership on process death"
            );
        }
    }

    #[test]
    fn repair_pending_run_is_not_evicted_until_explicit_finalize() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        create_private_dir_all(&root.join("sessions")).unwrap();
        let thread_id = "10000000-0000-4000-8000-000000000001";
        let runs = root.join("sessions").join(thread_id).join("runs");
        create_private_dir_all(&runs).unwrap();
        let pending_id = "20000000-0000-4000-8000-000000000002";
        for index in 0..RUNS_PER_THREAD {
            let run_id = if index == 0 {
                pending_id.to_string()
            } else {
                format!("30000000-0000-4000-8000-{index:012x}")
            };
            let run = runs.join(&run_id);
            create_private_dir(&run).unwrap();
            let mut manifest = RunManifest::new(thread_id, &run_id, "task", source_hash(b"src"));
            if index != 0 {
                manifest.completed_at_unix_ms = Some(index as u64);
            }
            write_json_private(&run.join("manifest.json"), &manifest).unwrap();
        }
        enforce_run_retention(&root, thread_id).unwrap();
        assert!(runs.join(pending_id).exists());
        assert!(!runs.join("30000000-0000-4000-8000-000000000001").exists());

        finalize_run(&root, thread_id, pending_id).unwrap();
        let mut pending = read_manifest(&runs.join(pending_id).join("manifest.json")).unwrap();
        pending.completed_at_unix_ms = Some(0);
        replace_private_file(
            &runs.join(pending_id).join("manifest.json"),
            &serde_json::to_vec_pretty(&pending).unwrap(),
        )
        .unwrap();
        let newest_id = "40000000-0000-4000-8000-000000000099";
        let newest = runs.join(newest_id);
        create_private_dir(&newest).unwrap();
        let mut newest_manifest =
            RunManifest::new(thread_id, newest_id, "task", source_hash(b"src"));
        newest_manifest.completed_at_unix_ms = Some(u64::MAX - 1);
        write_json_private(&newest.join("manifest.json"), &newest_manifest).unwrap();
        enforce_run_retention(&root, thread_id).unwrap();
        assert!(!runs.join(pending_id).exists());
    }
}
