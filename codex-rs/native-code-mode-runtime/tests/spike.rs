#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::BufRead as _;
use std::io::Read as _;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use codex_native_code_mode_runtime::CONCURRENT_CALLS;
use codex_native_code_mode_runtime::DIAGNOSTIC_BYTES;
use codex_native_code_mode_runtime::FailureKind;
use codex_native_code_mode_runtime::HOST_EVENT_CHANNEL_CAPACITY;
use codex_native_code_mode_runtime::HostEventKind;
use codex_native_code_mode_runtime::Limits;
use codex_native_code_mode_runtime::NativeCall;
use codex_native_code_mode_runtime::NativeCapabilityDelegate;
use codex_native_code_mode_runtime::NativeDelegateFuture;
use codex_native_code_mode_runtime::NativeHost;
use codex_native_code_mode_runtime::NativeOutcome;
use codex_native_code_mode_runtime::NativeRequest;
use codex_native_code_mode_runtime::RUNS_PER_THREAD;
use codex_native_code_mode_runtime::SOURCE_BYTES;
use codex_native_code_mode_runtime::decode_evidence;
use codex_native_code_mode_runtime::materialize_sdk;
use codex_native_code_mode_runtime::process_exists;
use sha2::Digest;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const THREAD_ID: &str = "10000000-0000-4000-8000-000000000001";
const RUN_ID: &str = "20000000-0000-4000-8000-000000000002";
const SUCCESS_SOURCE: &str = include_str!("fixtures/workflow.rs");
const CPU_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::Context;
fn main() { let _ = std::mem::size_of::<Context>(); loop { std::hint::spin_loop(); } }
"#;
const DESCENDANT_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::{Error, run, Request};
fn main() { run(|context| { let mut child = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap(); let command = format!("descendant:{}", child.id()); let _ = context.call(Request::Shell { command, workdir: None, timeout_ms: 30_000 })?; let _ = child.wait(); Err::<(), _>(Error::Host("unexpected descendant completion".into())) }).unwrap(); }
"#;
const STDERR_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::Context;
fn main() { let _ = std::mem::size_of::<Context>(); eprintln!("{}", "x".repeat(70 * 1024)); }
"#;
const CRASH_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::Context;
fn main() { let _ = std::mem::size_of::<Context>(); panic!("intentional workflow crash"); }
"#;
const STDOUT_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::run;
fn main() { run(|context| { for _ in 0..25_000 { let _ = context.budget()?; } Ok(()) }).unwrap(); }
"#;
const TOTAL_CALL_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::{run, Request};
fn main() { run(|context| { for index in 0..33 { let _ = context.call(Request::Shell { command: index.to_string(), workdir: None, timeout_ms: 100 })?; } Ok(()) }).unwrap(); }
"#;
const RAW_CALL_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::{run, Request};
fn main() { run(|context| { for index in 0..32 { let _ = context.call(Request::Shell { command: index.to_string(), workdir: None, timeout_ms: 100 })?; } Ok(()) }).unwrap(); }
"#;
const LARGE_OUTCOME_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::{run, Evidence, Outcome, Request};
fn main() { run(|context| { let outcome = context.call(Request::Shell { command: "large".into(), workdir: None, timeout_ms: 100 })?; let failure = match outcome { Outcome::Failure { message, .. } => message, _ => "missing bound".into() }; context.finish(Evidence { version: 1, summary: "bounded".into(), verified: vec![], disputed: vec![], unresolved: vec![], artifact_refs: vec![], partial_failures: vec![failure], provenance_ids: vec![] }) }).unwrap(); }
"#;
const RETAINED_AUDIT_SHAPE_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::{run, Evidence, Outcome, Request};

fn main() {
    run(|context| {
        let mut verified = Vec::new();
        let mut unresolved = Vec::new();
        let mut provenance_ids = Vec::new();

        let inventory = context.call(Request::Shell {
            command: "find . -maxdepth 1 -mindepth 1 -print | LC_ALL=C sort".to_string(),
            workdir: None,
            timeout_ms: 5_000,
        })?;
        let inventory_id = match inventory {
            Outcome::Success { call_id, .. }
            | Outcome::Retry { call_id, .. }
            | Outcome::Failure { call_id, .. } => call_id,
        };
        provenance_ids.push(inventory_id);

        let cargo_probe = context.call(Request::Shell {
            command: "command -v cargo".to_string(),
            workdir: None,
            timeout_ms: 5_000,
        })?;
        let (cargo_probe_id, cargo_available) = match cargo_probe {
            Outcome::Success { call_id, output } => (call_id, !output.is_empty()),
            Outcome::Retry { call_id, .. } | Outcome::Failure { call_id, .. } => {
                (call_id, false)
            }
        };
        provenance_ids.push(cargo_probe_id);

        let mut commands = vec!["git status --short"];
        if cargo_available {
            commands.push("cargo test --workspace --all-targets --quiet");
        }
        commands.extend([
            "git diff --check",
            "git grep -n -I -E 'TODO|FIXME|XXX|HACK' -- . ':!target'",
            "git grep -n -I -E 'shell=True|verify=False|subprocess' -- '*.py'",
        ]);
        for command in commands {
            let outcome = context.call(Request::Shell {
                command: command.to_string(),
                workdir: None,
                timeout_ms: 120_000,
            })?;
            match outcome {
                Outcome::Success { call_id, .. } => {
                    provenance_ids.push(call_id);
                    verified.push(format!("completed: {command}"));
                }
                Outcome::Retry { call_id, reason } => {
                    provenance_ids.push(call_id);
                    unresolved.push(format!("retry: {reason}"));
                }
                Outcome::Failure { call_id, message } => {
                    provenance_ids.push(call_id);
                    unresolved.push(format!("failed: {message}"));
                }
            }
        }
        context.finish(Evidence {
            version: 1,
            summary: "Repository audit completed with five bounded calls".to_string(),
            verified,
            disputed: Vec::new(),
            unresolved,
            artifact_refs: Vec::new(),
            partial_failures: Vec::new(),
            provenance_ids,
        })
    })
    .expect("retained audit shape failed");
}
"#;
const RETAINED_INVALID_EVIDENCE_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::{run, Evidence, Request};

fn main() {
    run(|context| {
        for command in [
            "git status --short",
            "cargo test --workspace --all-targets --quiet",
            "git diff --check",
            "git grep TODO",
            "git grep subprocess",
        ] {
            let _ = context.call(Request::Shell {
                command: command.to_string(),
                workdir: None,
                timeout_ms: 5_000,
            })?;
        }
        let result = context.finish(Evidence {
            version: 1,
            summary: "retained invalid evidence shape".to_string(),
            verified: Vec::new(),
            disputed: Vec::new(),
            unresolved: Vec::new(),
            artifact_refs: vec!["shell:git-status".to_string()],
            partial_failures: Vec::new(),
            provenance_ids: vec!["git-status".to_string()],
        });
        let error = result.expect_err("invented provenance must fail");
        std::fs::write("finish-error.txt", format!("{error}"))?;
        Ok(())
    })
    .expect("invalid-evidence workflow should receive the host failure");
}
"#;

struct Tools {
    _root: TempDir,
    rustc: PathBuf,
    sdk: PathBuf,
    sdk_bytes: Vec<u8>,
    sdk_hash: String,
}

static TOOLS: OnceLock<Tools> = OnceLock::new();

fn tools() -> &'static Tools {
    TOOLS.get_or_init(|| {
        let root = tempfile::tempdir().expect("temp tools root");
        let output = std::process::Command::new("rustup")
            .args([
                "which",
                "--toolchain",
                "1.95.0-aarch64-apple-darwin",
                "rustc",
            ])
            .output()
            .expect("resolve pinned rustc");
        assert!(output.status.success());
        let rustc = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        let sdk = root.path().join("libycode_native_sdk.rlib");
        let source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../native-code-mode-sdk/src/lib.rs");
        let status = std::process::Command::new(&rustc)
            .args([
                "--crate-name=ycode_native_sdk",
                "--crate-type=rlib",
                "--edition=2024",
                "--target=aarch64-apple-darwin",
                "-Copt-level=0",
                "-Cdebuginfo=0",
                "-Cmetadata=ycode-native-sdk-v1",
                "-o",
            ])
            .arg(&sdk)
            .arg(source)
            .status()
            .expect("compile SDK");
        assert!(status.success());
        let sdk_bytes = std::fs::read(&sdk).unwrap();
        let sdk_hash = format!("{:x}", sha2::Sha256::digest(&sdk_bytes));
        Tools {
            _root: root,
            rustc,
            sdk,
            sdk_bytes,
            sdk_hash,
        }
    })
}

#[derive(Default)]
struct FakeDelegate {
    active: AtomicUsize,
    peak: AtomicUsize,
}

impl NativeCapabilityDelegate for FakeDelegate {
    fn invoke<'a>(
        &'a self,
        call: NativeCall,
        cancellation: CancellationToken,
    ) -> NativeDelegateFuture<'a> {
        Box::pin(async move {
            let now = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.peak.fetch_max(now, Ordering::AcqRel);
            let _guard = ActiveGuard(&self.active);
            tokio::select! {
                _ = cancellation.cancelled() => return Err("cancelled".into()),
                _ = tokio::time::sleep(Duration::from_millis(15)) => {}
            }
            let value = match call.request {
                NativeRequest::Shell {
                    command,
                    workdir,
                    timeout_ms,
                } => {
                    if command == "summarize:workspace/item-2.txt" {
                        return Ok(NativeOutcome::Retry("transient-item-2".into()));
                    }
                    format!("shell:{command}:{workdir:?}:{timeout_ms}").into_bytes()
                }
                NativeRequest::ApplyPatch { patch } => {
                    format!("patch:{}", patch.len()).into_bytes()
                }
            };
            Ok(NativeOutcome::Success(value))
        })
    }
}

struct ActiveGuard<'a>(&'a AtomicUsize);
impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct LargeDelegate;
struct MediumDelegate;
struct RetainedAuditDelegate;

impl NativeCapabilityDelegate for LargeDelegate {
    fn invoke<'a>(
        &'a self,
        _call: NativeCall,
        _cancellation: CancellationToken,
    ) -> NativeDelegateFuture<'a> {
        Box::pin(async { Ok(NativeOutcome::Success(vec![b'x'; 64 * 1024 + 1])) })
    }
}

impl NativeCapabilityDelegate for MediumDelegate {
    fn invoke<'a>(
        &'a self,
        _call: NativeCall,
        _cancellation: CancellationToken,
    ) -> NativeDelegateFuture<'a> {
        Box::pin(async { Ok(NativeOutcome::Success(vec![b'x'; 60 * 1024])) })
    }
}

impl NativeCapabilityDelegate for RetainedAuditDelegate {
    fn invoke<'a>(
        &'a self,
        call: NativeCall,
        _cancellation: CancellationToken,
    ) -> NativeDelegateFuture<'a> {
        Box::pin(async move {
            let NativeRequest::Shell { command, .. } = call.request else {
                return Err("retained audit fixture only permits Shell".to_string());
            };
            if command == "command -v cargo" {
                Ok(NativeOutcome::Success(b"/toolchain/bin/cargo".to_vec()))
            } else if command.starts_with("cargo test") {
                Ok(NativeOutcome::Failure(
                    "cargo was unavailable after an explicit tool probe".to_string(),
                ))
            } else if command.contains("shell=True") {
                Ok(NativeOutcome::Failure(
                    "no matching Python files".to_string(),
                ))
            } else {
                Ok(NativeOutcome::Success(
                    format!("completed:{command}").into_bytes(),
                ))
            }
        })
    }
}

async fn host(root: &Path, delegate: Arc<FakeDelegate>, limits: Limits) -> NativeHost {
    NativeHost::new(
        tools().rustc.clone(),
        tools().sdk.clone(),
        root.to_path_buf(),
        limits,
        delegate,
    )
    .await
    .unwrap()
}

fn prepare(host: &NativeHost, source: &str) -> codex_native_code_mode_runtime::RunArtifact {
    host.prepare_run(
        THREAD_ID,
        RUN_ID,
        "inspect and update deterministic workspace files",
        1,
        source.as_bytes(),
    )
    .unwrap()
}

#[tokio::test]
async fn typed_success_cache_artifacts_and_explicit_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let delegate = Arc::new(FakeDelegate::default());
    let runtime = host(root.path(), Arc::clone(&delegate), Limits::default()).await;
    let artifact = prepare(&runtime, SUCCESS_SOURCE);
    let report = runtime
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap();
    let evidence = decode_evidence(&report.evidence).unwrap();
    assert!(evidence.summary.contains("items=10"));
    assert_eq!(evidence.provenance_ids.len(), 10);
    assert_eq!(evidence.artifact_refs.len(), 20);
    for provenance in &evidence.provenance_ids {
        assert!(
            evidence
                .artifact_refs
                .contains(&format!("calls/{provenance}.request.bin"))
        );
        assert!(
            evidence
                .artifact_refs
                .contains(&format!("calls/{provenance}.result.bin"))
        );
    }
    assert_eq!(report.total_calls, 11);
    assert_eq!(report.peak_concurrent_calls, CONCURRENT_CALLS);
    assert_eq!(delegate.peak.load(Ordering::Acquire), CONCURRENT_CALLS);
    assert_eq!(report.owned_tasks_after, 0);
    println!(
        "native-resources workflow_peak_bytes={} workflow_user_ns={} workflow_system_ns={} host_peak_bytes={}",
        report.workflow_peak_bytes,
        report.workflow_user_time_ns,
        report.workflow_system_time_ns,
        report.host_peak_bytes
    );
    assert!(artifact.run_dir().join("manifest.json").is_file());
    assert!(artifact.run_dir().join("task.txt").is_file());
    assert!(artifact.run_dir().join("attempt-1/rustc.stderr").is_file());
    assert!(artifact.run_dir().join("evidence.json").is_file());
    assert_eq!(
        std::fs::read_dir(artifact.run_dir().join("calls"))
            .unwrap()
            .count(),
        22
    );
    assert_private_tree(artifact.run_dir());

    let second_root = tempfile::tempdir().unwrap();
    let host2 = host(
        second_root.path(),
        Arc::new(FakeDelegate::default()),
        Limits::default(),
    )
    .await;
    let second = prepare(&host2, SUCCESS_SOURCE);
    let second_report = host2
        .execute(&second, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.evidence, second_report.evidence);
    let run_dir = artifact.run_dir().to_path_buf();
    artifact.cleanup().unwrap();
    assert!(!run_dir.exists());
}

#[tokio::test]
async fn retained_invalid_then_corrected_audit_shape_is_truthful() {
    let root = tempfile::tempdir().unwrap();
    let runtime = NativeHost::new(
        tools().rustc.clone(),
        tools().sdk.clone(),
        root.path().to_path_buf(),
        Limits::default(),
        Arc::new(RetainedAuditDelegate),
    )
    .await
    .unwrap();
    let invalid = prepare(&runtime, RETAINED_INVALID_EVIDENCE_SOURCE);
    let failure = runtime
        .execute(&invalid, CancellationToken::new())
        .await
        .expect_err("invented provenance must be rejected");
    assert_eq!(failure.kind, FailureKind::Protocol);
    assert!(failure.diagnostic.len() <= DIAGNOSTIC_BYTES);
    assert_eq!(
        failure.diagnostic,
        format!(
            "evidence provenance id does not identify a joined native call\n[source_hash={}]",
            failure.source_hash
        )
    );
    assert_eq!(failure.owned_tasks_after, 0);
    assert_eq!(failure.process_reaped, Some(true));
    assert_eq!(
        std::fs::read_to_string(invalid.run_dir().join("finish-error.txt")).unwrap(),
        "host error: evidence provenance id does not identify a joined native call"
    );
    assert_eq!(
        std::fs::read_dir(invalid.run_dir().join("calls"))
            .unwrap()
            .count(),
        10
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(invalid.run_dir().join("manifest.json")).unwrap())
            .unwrap();
    assert!(manifest["completed_at_unix_ms"].is_number());
    invalid.cleanup().unwrap();

    let artifact = prepare(&runtime, RETAINED_AUDIT_SHAPE_SOURCE);
    let report = runtime
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap();
    let evidence = decode_evidence(&report.evidence).unwrap();
    assert_eq!(report.total_calls, 7);
    assert_eq!(evidence.provenance_ids.len(), 7);
    assert_eq!(evidence.artifact_refs.len(), 14);
    assert!(artifact.run_dir().join("evidence.json").is_file());
    for provenance in &evidence.provenance_ids {
        assert!(
            evidence
                .artifact_refs
                .contains(&format!("calls/{provenance}.request.bin"))
        );
        assert!(
            evidence
                .artifact_refs
                .contains(&format!("calls/{provenance}.result.bin"))
        );
    }
    artifact.cleanup().unwrap();
}

#[tokio::test]
async fn admitted_source_and_cleanup_containment_are_immutable() {
    let root = tempfile::tempdir().unwrap();
    let runtime = host(
        root.path(),
        Arc::new(FakeDelegate::default()),
        Limits::default(),
    )
    .await;
    let artifact = prepare(&runtime, SUCCESS_SOURCE);
    std::fs::write(
        artifact.source_path(),
        b"#![feature(test)]\nuse ycode_native_sdk::Context;",
    )
    .unwrap();
    let failure = runtime
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(failure.kind, FailureKind::Admission);
    assert_eq!(failure.process_reaped, None);
    assert!(failure.diagnostic.contains("revalidation"));
    artifact.cleanup().unwrap();

    let oversized = vec![b'x'; SOURCE_BYTES + 1];
    let failure = runtime
        .prepare_run(THREAD_ID, RUN_ID, "task", 1, &oversized)
        .unwrap_err();
    assert_eq!(failure.kind, FailureKind::Admission);
    assert_eq!(failure.process_reaped, None);

    let artifact = prepare(&runtime, SUCCESS_SOURCE);
    let outside = root.path().join("outside");
    std::fs::write(&outside, b"preserve").unwrap();
    std::fs::remove_file(artifact.source_path()).unwrap();
    std::os::unix::fs::symlink(&outside, artifact.source_path()).unwrap();
    artifact.cleanup().unwrap();
    assert_eq!(std::fs::read(&outside).unwrap(), b"preserve");

    let artifact = prepare(&runtime, SUCCESS_SOURCE);
    let run_dir = artifact.run_dir().to_path_buf();
    let displaced = root.path().join("displaced-run");
    std::fs::rename(&run_dir, &displaced).unwrap();
    let outside_dir = root.path().join("outside-dir");
    std::fs::create_dir(&outside_dir).unwrap();
    std::fs::write(outside_dir.join("sentinel"), b"preserve").unwrap();
    std::os::unix::fs::symlink(&outside_dir, &run_dir).unwrap();
    assert_eq!(
        artifact.cleanup().unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        std::fs::read(outside_dir.join("sentinel")).unwrap(),
        b"preserve"
    );
    std::fs::remove_file(&run_dir).unwrap();
    std::fs::rename(&displaced, &run_dir).unwrap();
    artifact.cleanup().unwrap();
}

#[tokio::test]
async fn two_attempts_and_completed_run_retention_preserve_inspectable_sources() {
    let root = tempfile::tempdir().unwrap();
    let host = host(
        root.path(),
        Arc::new(FakeDelegate::default()),
        Limits::default(),
    )
    .await;
    let broken = "#![forbid(unsafe_code)]\nuse ycode_native_sdk::Context;\nfn main() { let _: Missing = 1; }\n";
    let first = host
        .prepare_run(THREAD_ID, RUN_ID, "repair task", 1, broken.as_bytes())
        .unwrap();
    let failure = host
        .execute(&first, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(failure.kind, FailureKind::Compile);
    let second = host
        .prepare_run(
            THREAD_ID,
            RUN_ID,
            "repair task",
            2,
            SUCCESS_SOURCE.as_bytes(),
        )
        .unwrap();
    host.execute(&second, CancellationToken::new())
        .await
        .unwrap();
    assert!(second.run_dir().join("attempt-1/source.rs").is_file());
    assert!(second.run_dir().join("attempt-1/rustc.stderr").is_file());
    assert!(second.run_dir().join("attempt-2/source.rs").is_file());
    assert!(second.run_dir().join("attempt-2/rustc.stderr").is_file());
    second.cleanup().unwrap();

    let mut first_run = None;
    for sample in 0_u64..=RUNS_PER_THREAD as u64 {
        let run_id = format!("90000000-0000-4000-8000-{sample:012x}");
        let artifact = host
            .prepare_run(
                THREAD_ID,
                &run_id,
                "retention",
                1,
                SUCCESS_SOURCE.as_bytes(),
            )
            .unwrap();
        host.execute(&artifact, CancellationToken::new())
            .await
            .unwrap();
        if sample == 0 {
            first_run = Some(artifact.run_dir().to_path_buf());
        }
    }
    assert!(!first_run.unwrap().exists());
    assert_eq!(
        std::fs::read_dir(root.path().join("sessions").join(THREAD_ID).join("runs"))
            .unwrap()
            .count(),
        RUNS_PER_THREAD
    );
}

#[tokio::test]
async fn total_raw_and_child_visible_call_outputs_are_bounded() {
    let total = run_failure(TOTAL_CALL_SOURCE).await;
    assert_eq!(total.kind, FailureKind::CallLimit);

    let raw_root = tempfile::tempdir().unwrap();
    let raw_host = NativeHost::new(
        tools().rustc.clone(),
        tools().sdk.clone(),
        raw_root.path().to_path_buf(),
        Limits::default(),
        Arc::new(MediumDelegate),
    )
    .await
    .unwrap();
    let raw_artifact = prepare(&raw_host, RAW_CALL_SOURCE);
    let raw = raw_host
        .execute(&raw_artifact, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(raw.kind, FailureKind::CallLimit);
    assert!(raw.diagnostic.contains("raw call artifacts"));
    raw_artifact.cleanup().unwrap();

    let root = tempfile::tempdir().unwrap();
    let host = NativeHost::new(
        tools().rustc.clone(),
        tools().sdk.clone(),
        root.path().to_path_buf(),
        Limits::default(),
        Arc::new(LargeDelegate),
    )
    .await
    .unwrap();
    let artifact = prepare(&host, LARGE_OUTCOME_SOURCE);
    let report = host
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap();
    let evidence = decode_evidence(&report.evidence).unwrap();
    assert_eq!(
        evidence.partial_failures,
        ["native delegate output exceeded limit"]
    );
    artifact.cleanup().unwrap();
}

#[tokio::test]
async fn cache_hit_stampede_corruption_and_sdk_materialization_are_bounded() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let sdk = materialize_sdk(&store, &tools().sdk_bytes, &tools().sdk_hash).unwrap();
    assert_eq!(std::fs::read(&sdk).unwrap(), tools().sdk_bytes);
    assert_eq!(
        std::fs::metadata(&sdk).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(materialize_sdk(&store, b"corrupt", &tools().sdk_hash).is_err());

    let runtime = host(&store, Arc::new(FakeDelegate::default()), Limits::default()).await;
    let competing_host = host(&store, Arc::new(FakeDelegate::default()), Limits::default()).await;
    let first = prepare(&runtime, SUCCESS_SOURCE);
    let second = runtime
        .prepare_run(
            THREAD_ID,
            "70000000-0000-4000-8000-000000000007",
            "inspect and update deterministic workspace files",
            1,
            SUCCESS_SOURCE.as_bytes(),
        )
        .unwrap();
    let (first_result, second_result) = tokio::join!(
        runtime.execute(&first, CancellationToken::new()),
        competing_host.execute(&second, CancellationToken::new())
    );
    assert!(first_result.is_ok());
    assert!(second_result.is_ok());
    let object = std::fs::read_dir(store.join("cache/objects"))
        .unwrap()
        .find_map(|entry| {
            let entry = entry.unwrap();
            entry.file_type().unwrap().is_dir().then_some(entry.path())
        })
        .unwrap();
    let binary = object.join("workflow");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    first.cleanup().unwrap();
    second.cleanup().unwrap();

    let repaired = prepare(&runtime, SUCCESS_SOURCE);
    runtime
        .execute(&repaired, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        std::fs::metadata(&binary).unwrap().permissions().mode() & 0o777,
        0o500
    );
    repaired.cleanup().unwrap();
}

#[tokio::test]
async fn malicious_compiler_cache_manifest_and_sdk_files_are_bounded_before_read() {
    let root = tempfile::tempdir().unwrap();
    let outside = root.path().join("outside-sentinel");
    std::fs::write(&outside, b"outside unchanged").unwrap();
    let store = root.path().join("store");
    let sdk = materialize_sdk(&store, &tools().sdk_bytes, &tools().sdk_hash).unwrap();
    std::fs::remove_file(&sdk).unwrap();
    std::os::unix::fs::symlink(&outside, &sdk).unwrap();
    assert!(materialize_sdk(&store, &tools().sdk_bytes, &tools().sdk_hash).is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside unchanged");
    std::fs::remove_file(&sdk).unwrap();
    let oversized_sdk = std::fs::File::create(&sdk).unwrap();
    oversized_sdk.set_len(16 * 1024 * 1024 + 1).unwrap();
    drop(oversized_sdk);
    assert!(materialize_sdk(&store, &tools().sdk_bytes, &tools().sdk_hash).is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside unchanged");

    let compiler = root.path().join("oversized-rustc");
    write_oversized_compiler(&compiler);
    let oversized_store = root.path().join("oversized-store");
    let oversized_host = NativeHost::new(
        compiler,
        tools().sdk.clone(),
        oversized_store.clone(),
        Limits::default(),
        Arc::new(FakeDelegate::default()),
    )
    .await
    .unwrap();
    let artifact = prepare(&oversized_host, SUCCESS_SOURCE);
    let failure = oversized_host
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(failure.kind, FailureKind::Compile);
    assert!(failure.diagnostic.contains("oversized"));
    assert!(
        !oversized_store
            .join("cache/objects")
            .read_dir()
            .unwrap()
            .any(|entry| { entry.unwrap().file_name().to_string_lossy().len() == 64 })
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside unchanged");
    artifact.cleanup().unwrap();

    let cache_store = root.path().join("cache-store");
    let cache_host = host(
        &cache_store,
        Arc::new(FakeDelegate::default()),
        Limits::default(),
    )
    .await;
    let first = prepare(&cache_host, SUCCESS_SOURCE);
    cache_host
        .execute(&first, CancellationToken::new())
        .await
        .unwrap();
    first.cleanup().unwrap();
    let object = std::fs::read_dir(cache_store.join("cache/objects"))
        .unwrap()
        .find_map(|entry| {
            let entry = entry.unwrap();
            entry.file_type().unwrap().is_dir().then_some(entry.path())
        })
        .unwrap();
    let binary = object.join("workflow");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&binary)
        .unwrap()
        .set_len(64 * 1024 * 1024 + 1)
        .unwrap();
    let second = cache_host
        .prepare_run(
            THREAD_ID,
            "71000000-0000-4000-8000-000000000071",
            "cache repair",
            1,
            SUCCESS_SOURCE.as_bytes(),
        )
        .unwrap();
    cache_host
        .execute(&second, CancellationToken::new())
        .await
        .unwrap();
    assert!(std::fs::metadata(&binary).unwrap().len() < 64 * 1024 * 1024);
    second.cleanup().unwrap();

    std::fs::OpenOptions::new()
        .write(true)
        .open(object.join("manifest.json"))
        .unwrap()
        .set_len(64 * 1024 + 1)
        .unwrap();
    let third = cache_host
        .prepare_run(
            THREAD_ID,
            "72000000-0000-4000-8000-000000000072",
            "manifest repair",
            1,
            SUCCESS_SOURCE.as_bytes(),
        )
        .unwrap();
    cache_host
        .execute(&third, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        std::fs::metadata(object.join("manifest.json"))
            .unwrap()
            .len()
            < 64 * 1024
    );
    third.cleanup().unwrap();

    let broken = "#![forbid(unsafe_code)]\nuse ycode_native_sdk::Context;\nfn main() { let _: Missing = 1; }\n";
    let run_manifest = cache_host
        .prepare_run(
            THREAD_ID,
            "73000000-0000-4000-8000-000000000073",
            "run manifest bound",
            1,
            broken.as_bytes(),
        )
        .unwrap();
    assert_eq!(
        cache_host
            .execute(&run_manifest, CancellationToken::new())
            .await
            .unwrap_err()
            .kind,
        FailureKind::Compile
    );
    std::fs::OpenOptions::new()
        .write(true)
        .open(run_manifest.run_dir().join("manifest.json"))
        .unwrap()
        .set_len(16 * 1024 + 1)
        .unwrap();
    assert_eq!(
        cache_host
            .prepare_run(
                THREAD_ID,
                "73000000-0000-4000-8000-000000000073",
                "run manifest bound",
                2,
                SUCCESS_SOURCE.as_bytes(),
            )
            .unwrap_err()
            .kind,
        FailureKind::Admission
    );
    run_manifest.cleanup().unwrap();
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside unchanged");
}

#[tokio::test]
async fn substantive_fixture_is_byte_identical_for_100_runs() {
    let root = tempfile::tempdir().unwrap();
    let runtime = host(
        root.path(),
        Arc::new(FakeDelegate::default()),
        Limits::default(),
    )
    .await;
    let mut expected = None;
    for _ in 0..100 {
        let artifact = runtime
            .prepare_run(
                THREAD_ID,
                "80000000-0000-4000-8000-000000000008",
                "inspect and update deterministic workspace files",
                1,
                SUCCESS_SOURCE.as_bytes(),
            )
            .unwrap();
        let evidence = runtime
            .execute(&artifact, CancellationToken::new())
            .await
            .unwrap()
            .evidence;
        match &expected {
            Some(expected) => assert_eq!(&evidence, expected),
            None => expected = Some(evidence),
        }
        artifact.cleanup().unwrap();
    }
}

#[tokio::test]
async fn cancellation_reaps_cpu_and_descendant_process_groups() {
    for (class, source) in [
        ("normal", SUCCESS_SOURCE),
        ("cpu", CPU_SOURCE),
        ("descendant", DESCENDANT_SOURCE),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (events_tx, mut events_rx) = mpsc::channel(HOST_EVENT_CHANNEL_CAPACITY);
        let host = host(
            root.path(),
            Arc::new(FakeDelegate::default()),
            Limits::default(),
        )
        .await
        .with_events(events_tx);
        let mut samples_ms = Vec::new();
        for sample in 0_u64..20 {
            let run_id = match class {
                "normal" => format!("30000000-0000-4000-8000-{sample:012x}"),
                "cpu" => format!("40000000-0000-4000-8000-{sample:012x}"),
                _ => format!("50000000-0000-4000-8000-{sample:012x}"),
            };
            let artifact = host
                .prepare_run(THREAD_ID, &run_id, "cancel lifecycle", 1, source.as_bytes())
                .unwrap();
            let cancellation = CancellationToken::new();
            let execute = host.execute(&artifact, cancellation.clone());
            tokio::pin!(execute);
            let mut workflow_pid = 0;
            let mut descendant_pid = None;
            loop {
                tokio::select! {
                    event = events_rx.recv() => match event.unwrap().kind {
                        HostEventKind::WorkflowStarted(pid) => {
                            workflow_pid = pid;
                            if class == "cpu" { break; }
                        }
                        HostEventKind::FirstCapability if class == "normal" => break,
                        HostEventKind::DescendantPid(pid) if class == "descendant" => {
                            descendant_pid = Some(pid);
                            break;
                        }
                        _ => {}
                    },
                    result = &mut execute => panic!("execution settled before cancellation boundary: {result:?}"),
                }
            }
            assert!(workflow_pid != 0 && process_exists(workflow_pid));
            if let Some(pid) = descendant_pid {
                assert!(process_exists(pid));
            }
            let cancelled_at = std::time::Instant::now();
            cancellation.cancel();
            let failure = execute.await.unwrap_err();
            samples_ms.push(cancelled_at.elapsed().as_secs_f64() * 1_000.0);
            assert_eq!(failure.kind, FailureKind::Cancelled);
            assert_eq!(failure.process_reaped, Some(true));
            assert_eq!(failure.owned_tasks_after, 0);
            assert!(!process_exists(workflow_pid));
            for pid in failure.observed_descendant_pids {
                assert!(!process_exists(pid));
            }
            artifact.cleanup().unwrap();
        }
        assert!(samples_ms.iter().copied().fold(0.0, f64::max) <= 1_000.0);
        assert!(nearest_rank(&samples_ms, 95) <= 250.0);
        println!("native-cancel-{class}-ms={samples_ms:?}");
    }
}

#[tokio::test]
async fn bounded_progress_overflow_fails_workflow_start_and_reaps_instead_of_deadlocking() {
    let root = tempfile::tempdir().unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(1);
    let host = host(
        root.path(),
        Arc::new(FakeDelegate::default()),
        Limits::default(),
    )
    .await
    .with_events(events_tx.clone());
    let run_id = "51000000-0000-4000-8000-000000000001";
    let artifact = host
        .prepare_run(
            THREAD_ID,
            run_id,
            "bounded progress overflow",
            1,
            SUCCESS_SOURCE.as_bytes(),
        )
        .unwrap();
    let execution = host.execute(&artifact, CancellationToken::new());
    tokio::pin!(execution);

    let compiler = tokio::select! {
        event = events_rx.recv() => event.unwrap(),
        result = &mut execution => panic!("execution settled before compiler event: {result:?}"),
    };
    assert!(matches!(compiler.kind, HostEventKind::CompilerStarted(_)));
    let compiled = tokio::select! {
        event = events_rx.recv() => event.unwrap(),
        result = &mut execution => panic!("execution settled before compiled event: {result:?}"),
    };
    assert!(matches!(compiled.kind, HostEventKind::Compiled));
    events_tx
        .try_send(codex_native_code_mode_runtime::HostEvent {
            run_id: run_id.to_string(),
            kind: HostEventKind::FirstCapability,
        })
        .unwrap();

    let failure = tokio::time::timeout(Duration::from_secs(2), &mut execution)
        .await
        .expect("progress overflow must settle without delegate-gate deadlock")
        .unwrap_err();
    assert_eq!(failure.kind, FailureKind::Cleanup);
    assert!(failure.diagnostic.contains("progress delivery failed"));
    assert_eq!(failure.process_reaped, Some(true));
    assert_eq!(failure.owned_tasks_after, 0);
    artifact.cleanup().unwrap();
}

#[tokio::test]
async fn generated_fake_and_unrelated_descendant_hints_never_emit_process_events() {
    let root = tempfile::tempdir().unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(HOST_EVENT_CHANNEL_CAPACITY);
    let host = host(
        root.path(),
        Arc::new(FakeDelegate::default()),
        Limits::default(),
    )
    .await
    .with_events(events_tx);
    let source = format!(
        r#"#![forbid(unsafe_code)]
use ycode_native_sdk as sdk;
use sdk::{{Context, Evidence, Outcome, Request, Result}};
fn main() {{ sdk::run(workflow).unwrap(); }}
fn workflow(context: &mut Context) -> Result<()> {{
    let mut provenance = Vec::new();
    for pid in [{}, 4_294_967_295_u32] {{
        let result = context.call(Request::Shell {{
            command: format!("descendant:{{pid}}"), workdir: None, timeout_ms: 1_000,
        }})?;
        match result {{
            Outcome::Success {{ call_id, .. }} => provenance.push(call_id),
            Outcome::Retry {{ reason, .. }} | Outcome::Failure {{ message: reason, .. }} =>
                return Err(sdk::Error::Host(reason)),
        }}
    }}
    context.finish(Evidence {{ version: 1, summary: "fake descendants ignored".into(),
        verified: vec![], disputed: vec![], unresolved: vec![], artifact_refs: vec![],
        partial_failures: vec![], provenance_ids: provenance }})
}}
"#,
        std::process::id()
    );
    let run_id = "52000000-0000-4000-8000-000000000001";
    let artifact = host
        .prepare_run(
            THREAD_ID,
            run_id,
            "reject invented descendants",
            1,
            source.as_bytes(),
        )
        .unwrap();
    let execution = host.execute(&artifact, CancellationToken::new());
    tokio::pin!(execution);
    let mut events = Vec::new();
    let report = loop {
        tokio::select! {
            event = events_rx.recv() => events.push(event.expect("progress remains open").kind),
            result = &mut execution => break result.unwrap(),
        }
    };
    assert_eq!(report.total_calls, 2);
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, HostEventKind::DescendantPid(_)))
    );
    assert!(report.observed_descendant_pids.is_empty());
    artifact.cleanup().unwrap();
}

fn nearest_rank(samples: &[f64], percentile: usize) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (percentile * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

#[test]
fn cross_process_cache_contender_helper() {
    let Some(store) = std::env::var_os("YCODE_NATIVE_CONTENDER_STORE") else {
        return;
    };
    let rustc = PathBuf::from(std::env::var_os("YCODE_NATIVE_CONTENDER_RUSTC").unwrap());
    let sdk = PathBuf::from(std::env::var_os("YCODE_NATIVE_CONTENDER_SDK").unwrap());
    let run_id = std::env::var("YCODE_NATIVE_CONTENDER_RUN").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (host, artifact) = runtime.block_on(async move {
        let host = NativeHost::new(
            rustc,
            sdk,
            PathBuf::from(store),
            Limits::default(),
            Arc::new(FakeDelegate::default()),
        )
        .await
        .unwrap();
        let artifact = host
            .prepare_run(
                THREAD_ID,
                &run_id,
                "cross process cache contender",
                1,
                SUCCESS_SOURCE.as_bytes(),
            )
            .unwrap();
        (host, artifact)
    });
    println!("YCODE_NATIVE_CONTENDER_READY");
    std::io::stdout().flush().unwrap();
    let mut release = [0_u8; 1];
    std::io::stdin().read_exact(&mut release).unwrap();
    runtime.block_on(async move {
        let result = host.execute(&artifact, CancellationToken::new()).await;
        match result {
            Ok(_) => println!("YCODE_NATIVE_CONTENDER_RESULT=ok"),
            Err(error) => println!("YCODE_NATIVE_CONTENDER_RESULT=transient:{:?}", error.kind),
        }
        artifact.cleanup().unwrap();
    });
}

#[test]
fn simultaneous_processes_publish_one_valid_immutable_cache_object() {
    struct Contender {
        child: std::process::Child,
        reader: std::io::BufReader<std::process::ChildStdout>,
    }

    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("shared-store");
    let mut contenders = Vec::new();
    for run_id in [
        "91000000-0000-4000-8000-000000000091",
        "92000000-0000-4000-8000-000000000092",
    ] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cross_process_cache_contender_helper",
                "--nocapture",
            ])
            .env("YCODE_NATIVE_CONTENDER_STORE", &store)
            .env("YCODE_NATIVE_CONTENDER_RUSTC", &tools().rustc)
            .env("YCODE_NATIVE_CONTENDER_SDK", &tools().sdk)
            .env("YCODE_NATIVE_CONTENDER_RUN", run_id)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if line.contains("YCODE_NATIVE_CONTENDER_READY") {
                break;
            }
        }
        contenders.push(Contender { child, reader });
    }
    for contender in &mut contenders {
        contender
            .child
            .stdin
            .take()
            .unwrap()
            .write_all(b"x")
            .unwrap();
    }
    let mut results = Vec::new();
    for contender in &mut contenders {
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(contender.reader.read_line(&mut line).unwrap(), 0);
            if let Some(result) = line.split("YCODE_NATIVE_CONTENDER_RESULT=").nth(1) {
                results.push(result.trim().to_string());
                break;
            }
        }
        assert!(contender.child.wait().unwrap().success());
    }
    assert!(results.iter().any(|result| result == "ok"));
    assert!(results.iter().all(|result| {
        result == "ok" || result == "transient:Admission" || result == "transient:Cleanup"
    }));
    let objects: Vec<_> = std::fs::read_dir(store.join("cache/objects"))
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            entry.file_type().unwrap().is_dir().then_some(entry.path())
        })
        .collect();
    assert_eq!(objects.len(), 1);
    let object = &objects[0];
    let binary = object.join("workflow");
    assert!(std::fs::symlink_metadata(&binary).unwrap().is_file());
    assert!(std::fs::metadata(&binary).unwrap().len() <= 64 * 1024 * 1024);
    assert_eq!(
        std::fs::metadata(&binary).unwrap().permissions().mode() & 0o777,
        0o500
    );
    assert!(
        std::fs::metadata(object.join("manifest.json"))
            .unwrap()
            .len()
            <= 64 * 1024
    );
}

#[tokio::test]
async fn compile_timeout_drop_and_spawn_failure_own_processes_truthfully() {
    let root = tempfile::tempdir().unwrap();
    let compiler = root.path().join("fake-rustc");
    write_fake_compiler(&compiler);
    let store = root.path().join("store");
    let limits = Limits {
        compile_timeout: Duration::from_millis(100),
        ..Limits::default()
    };
    let host = NativeHost::new(
        compiler.clone(),
        tools().sdk.clone(),
        store,
        limits,
        Arc::new(FakeDelegate::default()),
    )
    .await
    .unwrap();
    let artifact = prepare(&host, SUCCESS_SOURCE);
    let failure = host
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(failure.kind, FailureKind::CompileTimeout);
    assert_eq!(failure.process_reaped, Some(true));
    assert!(!artifact.binary_exists());
    assert!(
        !std::fs::read_dir(root.path().join("store/cache/objects"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tmp-"))
    );
    artifact.cleanup().unwrap();

    let store = root.path().join("removed-store");
    write_fake_compiler(&compiler);
    let host = NativeHost::new(
        compiler.clone(),
        tools().sdk.clone(),
        store,
        Limits::default(),
        Arc::new(FakeDelegate::default()),
    )
    .await
    .unwrap();
    std::fs::remove_file(&compiler).unwrap();
    let artifact = prepare(&host, SUCCESS_SOURCE);
    let failure = host
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(failure.kind, FailureKind::CompilerUnavailable);
    assert_eq!(failure.process_reaped, None);
    assert!(!artifact.binary_exists());
    assert!(artifact.source_path().is_file());
    artifact.cleanup().unwrap();
}

#[tokio::test]
async fn exclusive_lease_compile_cancel_drop_timeout_and_crash_settle_ownership() {
    let root = tempfile::tempdir().unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(HOST_EVENT_CHANNEL_CAPACITY);
    let runtime = host(
        root.path(),
        Arc::new(FakeDelegate::default()),
        Limits::default(),
    )
    .await
    .with_events(events_tx);
    let artifact = prepare(&runtime, SUCCESS_SOURCE);
    let cancellation = CancellationToken::new();
    let mut first = Box::pin(runtime.execute(&artifact, cancellation.clone()));
    let compiler_pid = loop {
        tokio::select! {
            event = events_rx.recv() => if let HostEventKind::CompilerStarted(pid) = event.unwrap().kind { break pid; },
            result = &mut first => panic!("first execution settled before compiler boundary: {result:?}"),
        }
    };
    assert!(process_exists(compiler_pid));
    let second = runtime
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(second.kind, FailureKind::Admission);
    assert_eq!(second.process_reaped, None);
    assert!(second.diagnostic.contains("execution owner"));
    cancellation.cancel();
    let first = first.await.unwrap_err();
    assert_eq!(first.kind, FailureKind::Cancelled);
    assert_eq!(first.process_reaped, Some(true));
    assert!(!process_exists(compiler_pid));
    artifact.cleanup().unwrap();

    let compiler = root.path().join("drop-rustc");
    write_fake_compiler(&compiler);
    let (events_tx, mut events_rx) = mpsc::channel(HOST_EVENT_CHANNEL_CAPACITY);
    let drop_host = NativeHost::new(
        compiler,
        tools().sdk.clone(),
        root.path().join("drop-store"),
        Limits::default(),
        Arc::new(FakeDelegate::default()),
    )
    .await
    .unwrap()
    .with_events(events_tx);
    let artifact = prepare(&drop_host, SUCCESS_SOURCE);
    let mut execution = Box::pin(drop_host.execute(&artifact, CancellationToken::new()));
    let compiler_pid = loop {
        tokio::select! {
            event = events_rx.recv() => if let HostEventKind::CompilerStarted(pid) = event.unwrap().kind { break pid; },
            result = &mut execution => panic!("drop probe settled before compiler boundary: {result:?}"),
        }
    };
    drop(execution);
    assert!(!process_exists(compiler_pid));
    assert!(!artifact.binary_exists());
    assert!(
        !std::fs::read_dir(root.path().join("drop-store/cache/objects"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tmp-"))
    );
    artifact.cleanup().unwrap();

    let timeout_root = tempfile::tempdir().unwrap();
    let timeout_host = host(
        timeout_root.path(),
        Arc::new(FakeDelegate::default()),
        Limits {
            workflow_timeout: Duration::from_millis(100),
            ..Limits::default()
        },
    )
    .await;
    let timeout_artifact = prepare(&timeout_host, CPU_SOURCE);
    let timeout = timeout_host
        .execute(&timeout_artifact, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(timeout.kind, FailureKind::WorkflowTimeout);
    assert_eq!(timeout.process_reaped, Some(true));
    timeout_artifact.cleanup().unwrap();

    let crash = run_failure(CRASH_SOURCE).await;
    assert!(matches!(
        crash.kind,
        FailureKind::ChildCrash | FailureKind::Protocol
    ));
    assert_eq!(crash.process_reaped, Some(true));
    let stdout = run_failure(STDOUT_SOURCE).await;
    assert_eq!(stdout.kind, FailureKind::Protocol);
    assert!(stdout.diagnostic.contains("stdout byte limit"));
}

#[tokio::test]
async fn wrong_or_missing_compiler_and_bounded_diagnostics_fail_fast() {
    let root = tempfile::tempdir().unwrap();
    let missing = match NativeHost::new(
        root.path().join("missing"),
        tools().sdk.clone(),
        root.path().join("a"),
        Limits::default(),
        Arc::new(FakeDelegate::default()),
    )
    .await
    {
        Ok(_) => panic!("missing compiler accepted"),
        Err(error) => error,
    };
    assert_eq!(missing.kind, FailureKind::CompilerUnavailable);
    assert_eq!(missing.process_reaped, None);

    let wrong = root.path().join("wrong");
    std::fs::write(&wrong, "#!/bin/sh\necho 'release: 1.94.0'\n").unwrap();
    std::fs::set_permissions(&wrong, std::fs::Permissions::from_mode(0o700)).unwrap();
    let wrong = match NativeHost::new(
        wrong,
        tools().sdk.clone(),
        root.path().join("b"),
        Limits::default(),
        Arc::new(FakeDelegate::default()),
    )
    .await
    {
        Ok(_) => panic!("wrong compiler accepted"),
        Err(error) => error,
    };
    assert_eq!(wrong.kind, FailureKind::CompilerVersion);
    assert!(wrong.diagnostic.len() <= DIAGNOSTIC_BYTES);

    let failure = run_failure(STDERR_SOURCE).await;
    assert_eq!(failure.kind, FailureKind::StderrLimit);
    assert!(failure.diagnostic.len() <= DIAGNOSTIC_BYTES);
}

#[test]
fn sdk_contract_is_std_only_seven_concepts_and_six_context_operations() {
    let manifest = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../native-code-mode-sdk/Cargo.toml"),
    )
    .unwrap();
    assert!(!manifest.contains("[dependencies]"));
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../native-code-mode-sdk/src/lib.rs"),
    )
    .unwrap();
    for concept in [
        "enum Request",
        "struct Task",
        "enum Outcome",
        "struct Evidence",
        "enum Error",
        "type Result",
        "struct Context",
    ] {
        assert!(source.contains(concept));
    }
    for operation in [
        "fn call",
        "fn spawn",
        "fn join",
        "fn budget",
        "fn cancelled",
        "fn finish",
    ] {
        assert!(source.contains(operation));
    }
    assert!(source.contains("#![forbid(unsafe_code)]"));
}

async fn run_failure(source: &str) -> codex_native_code_mode_runtime::RunFailure {
    let root = tempfile::tempdir().unwrap();
    let host = host(
        root.path(),
        Arc::new(FakeDelegate::default()),
        Limits::default(),
    )
    .await;
    let artifact = prepare(&host, source);
    let failure = host
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(failure.owned_tasks_after, 0);
    artifact.cleanup().unwrap();
    failure
}

fn assert_private_tree(root: &Path) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            let mode = metadata.permissions().mode() & 0o777;
            if metadata.is_dir() {
                assert_eq!(mode, 0o700);
                pending.push(entry.path());
            } else {
                assert_eq!(mode, 0o600);
            }
        }
    }
}

fn write_fake_compiler(path: &Path) {
    std::fs::write(path, r#"#!/bin/sh
if [ "$1" = "-vV" ]; then
  printf '%s\n' 'rustc 1.95.0' 'commit-hash: 59807616e1fa2540724bfbac14d7976d7e4a3860' 'host: aarch64-apple-darwin' 'release: 1.95.0'
  exit 0
fi
output=
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-o" ]; then shift; output=$1; fi
  shift
done
printf partial > "$output"
chmod 700 "$output"
sleep 30
"#).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn write_oversized_compiler(path: &Path) {
    std::fs::write(
        path,
        r#"#!/bin/sh
if [ "$1" = "-vV" ]; then
  printf '%s\n' 'rustc 1.95.0' 'commit-hash: 59807616e1fa2540724bfbac14d7976d7e4a3860' 'host: aarch64-apple-darwin' 'release: 1.95.0'
  exit 0
fi
output=
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-o" ]; then shift; output=$1; fi
  shift
done
/bin/dd if=/dev/zero of="$output" bs=1 count=1 seek=67108864 2>/dev/null
chmod 700 "$output"
"#,
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}
