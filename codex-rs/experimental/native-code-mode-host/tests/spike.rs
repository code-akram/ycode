#![cfg(target_os = "macos")]

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use codex_native_code_mode_host::{
    CONCURRENT_CALLS, DIAGNOSTIC_BYTES, FINAL_EVIDENCE_BYTES, FailureKind, HostEventKind, Limits,
    NativeHost, RunFailure, SOURCE_BYTES, WORKFLOW_STDERR_BYTES, process_exists,
};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const SUCCESS_SOURCE: &str = include_str!("fixtures/workflow.rs");
const WAIT_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::{run, Evidence, Request};
fn main() { run(|context| { let _ = context.call(Request::Fetch { query: "wait".into(), attempt: 0 })?; context.finish(Evidence(b"unexpected".to_vec())) }).unwrap(); }
"#;
const CPU_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::Context;
fn main() { let _ = std::mem::size_of::<Context>(); loop { std::hint::spin_loop(); } }
"#;
const DESCENDANT_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::{run, Evidence, Request};
fn main() { run(|context| { let mut child = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap(); let query = format!("descendant:{}", child.id()); let _ = context.call(Request::Fetch { query, attempt: 0 })?; let _ = child.wait(); context.finish(Evidence(b"unexpected".to_vec())) }).unwrap(); }
"#;
const CRASH_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::Context;
fn main() { let _ = std::mem::size_of::<Context>(); panic!("intentional child crash"); }
"#;
const OVERSIZED_STDERR_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::{run, Evidence};
fn main() { eprintln!("{}", "x".repeat(70 * 1024)); run(|context| context.finish(Evidence(b"done".to_vec()))).unwrap(); }
"#;
const OVERSIZED_STDOUT_SOURCE: &str = r#"#![forbid(unsafe_code)]
use std::io::Write;
use ycode_native_sdk::Context;
fn main() { let _ = std::mem::size_of::<Context>(); let mut out = std::io::stdout().lock(); out.write_all(&0x59434e52_u32.to_le_bytes()).unwrap(); out.write_all(&1_u16.to_le_bytes()).unwrap(); out.write_all(&1_u16.to_le_bytes()).unwrap(); out.write_all(&(65537_u32).to_le_bytes()).unwrap(); out.flush().unwrap(); std::thread::sleep(std::time::Duration::from_secs(30)); }
"#;
const BAD_VERSION_SOURCE: &str = r#"#![forbid(unsafe_code)]
use std::io::Write;
use ycode_native_sdk::Context;
fn main() { let _ = std::mem::size_of::<Context>(); let mut out = std::io::stdout().lock(); out.write_all(&0x59434e52_u32.to_le_bytes()).unwrap(); out.write_all(&2_u16.to_le_bytes()).unwrap(); out.write_all(&1_u16.to_le_bytes()).unwrap(); out.write_all(&0_u32.to_le_bytes()).unwrap(); out.flush().unwrap(); std::thread::sleep(std::time::Duration::from_secs(30)); }
"#;
const BAD_MAGIC_SOURCE: &str = r#"#![forbid(unsafe_code)]
use std::io::Write;
use ycode_native_sdk::Context;
fn main() { let _ = std::mem::size_of::<Context>(); let mut out = std::io::stdout().lock(); out.write_all(&0_u32.to_le_bytes()).unwrap(); out.write_all(&1_u16.to_le_bytes()).unwrap(); out.write_all(&1_u16.to_le_bytes()).unwrap(); out.write_all(&0_u32.to_le_bytes()).unwrap(); out.flush().unwrap(); std::thread::sleep(std::time::Duration::from_secs(30)); }
"#;

struct Tools {
    _root: TempDir,
    rustc: PathBuf,
    sdk: PathBuf,
    binary: PathBuf,
}

static TOOLS: OnceLock<Tools> = OnceLock::new();

fn tools() -> &'static Tools {
    TOOLS.get_or_init(|| {
        let root = tempfile::tempdir().unwrap();
        let rustc_output = std::process::Command::new("rustup")
            .args(["which", "--toolchain", "1.95.0", "rustc"])
            .output()
            .unwrap();
        assert!(rustc_output.status.success());
        let rustc = PathBuf::from(String::from_utf8(rustc_output.stdout).unwrap().trim());
        let sdk = root.path().join("libycode_native_sdk.rlib");
        let sdk_source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../native-code-mode-sdk/src/lib.rs");
        let status = std::process::Command::new(&rustc)
            .args([
                "--crate-name",
                "ycode_native_sdk",
                "--crate-type",
                "rlib",
                "--edition=2024",
            ])
            .arg("-Copt-level=0")
            .arg("-Cdebuginfo=0")
            .arg("-o")
            .arg(&sdk)
            .arg(&sdk_source)
            .status()
            .unwrap();
        assert!(status.success());
        Tools {
            _root: root,
            rustc,
            sdk,
            binary: PathBuf::from(env!("CARGO_BIN_EXE_codex-native-code-mode-spike")),
        }
    })
}

async fn host(root: &Path, limits: Limits) -> NativeHost {
    NativeHost::new(
        tools().rustc.clone(),
        tools().sdk.clone(),
        root.to_path_buf(),
        limits,
    )
    .await
    .unwrap()
}

async fn run_failure(source: &str, limits: Limits) -> RunFailure {
    let root = tempfile::tempdir().unwrap();
    let host = host(root.path(), limits).await;
    let artifact = host.prepare(source.as_bytes()).unwrap();
    let failure = host
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(!artifact.binary_exists());
    assert!(artifact.source_path().is_file());
    assert_eq!(std::fs::read_dir(artifact.run_dir()).unwrap().count(), 1);
    assert_eq!(failure.owned_tasks_after, 0);
    assert!(failure.diagnostic.len() <= DIAGNOSTIC_BYTES);
    artifact.cleanup().unwrap();
    failure
}

#[tokio::test]
async fn retained_source_identity_and_cleanup_ownership_are_not_bypassable() {
    let root = tempfile::tempdir().unwrap();
    let host = host(root.path(), Limits::default()).await;
    let artifact = host.prepare(SUCCESS_SOURCE.as_bytes()).unwrap();
    std::fs::write(
        artifact.source_path(),
        b"#![feature(test)]\nuse ycode_native_sdk::Context;",
    )
    .unwrap();
    let forbidden = host
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(forbidden.kind, FailureKind::Admission);
    assert_eq!(forbidden.process_reaped, None);
    assert!(forbidden.diagnostic.contains("revalidation"));

    std::fs::write(artifact.source_path(), vec![b'x'; SOURCE_BYTES + 1]).unwrap();
    let oversized = host
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(oversized.kind, FailureKind::Admission);
    assert_eq!(oversized.process_reaped, None);
    assert!(oversized.diagnostic.contains("source exceeds"));
    artifact.cleanup().unwrap();

    // This integration crate can only obtain read-only path references. The private fields make
    // path substitution through the public Rust API unrepresentable.
    let host_source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs")).unwrap();
    assert!(!host_source.contains("pub run_dir:"));
    assert!(!host_source.contains("pub source_path:"));

    let artifact = host.prepare(SUCCESS_SOURCE.as_bytes()).unwrap();
    let outside = root.path().join("outside-sentinel");
    std::fs::write(&outside, b"preserve").unwrap();
    std::fs::remove_file(artifact.source_path()).unwrap();
    symlink(&outside, artifact.source_path()).unwrap();
    artifact.cleanup().unwrap();
    assert_eq!(std::fs::read(&outside).unwrap(), b"preserve");

    let artifact = host.prepare(SUCCESS_SOURCE.as_bytes()).unwrap();
    let run_dir = artifact.run_dir().to_path_buf();
    std::fs::remove_file(artifact.source_path()).unwrap();
    std::fs::remove_dir(&run_dir).unwrap();
    let outside_dir = root.path().join("outside-dir");
    std::fs::create_dir(&outside_dir).unwrap();
    let sentinel = outside_dir.join("sentinel");
    std::fs::write(&sentinel, b"preserve").unwrap();
    symlink(&outside_dir, &run_dir).unwrap();
    let error = artifact.cleanup().unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"preserve");
    std::fs::remove_file(&run_dir).unwrap();
}

#[tokio::test]
async fn success_is_typed_concurrent_deterministic_and_cleanup_is_explicit() {
    let root = tempfile::tempdir().unwrap();
    let host = host(root.path(), Limits::default()).await;
    let artifact = host.prepare(SUCCESS_SOURCE.as_bytes()).unwrap();
    assert_eq!(
        std::fs::read_to_string(artifact.source_path()).unwrap(),
        SUCCESS_SOURCE
    );
    let report = host
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap();
    assert!(report.evidence.starts_with(b"native-evidence:v1:"));
    assert_eq!(report.total_calls, 11);
    assert!(
        std::str::from_utf8(&report.evidence)
            .unwrap()
            .contains("remaining=21")
    );
    assert_eq!(report.peak_concurrent_calls, CONCURRENT_CALLS);
    assert_eq!(report.owned_tasks_after, 0);
    assert!(!artifact.binary_exists());
    assert!(artifact.source_path().exists());
    let run_dir = artifact.run_dir().to_path_buf();
    artifact.cleanup().unwrap();
    assert!(!run_dir.exists());
}

#[tokio::test]
async fn admission_diagnostics_streams_and_evidence_are_bounded() {
    let root = tempfile::tempdir().unwrap();
    let host = host(root.path(), Limits::default()).await;
    let oversized = vec![b'x'; SOURCE_BYTES + 1];
    assert_eq!(
        host.prepare(&oversized).unwrap_err().kind,
        FailureKind::Admission
    );

    let mut invalid = String::from(
        "#![forbid(unsafe_code)]\nuse ycode_native_sdk::Context;\nfn main() { let _ = std::mem::size_of::<Context>();\n",
    );
    for index in 0..1_200 {
        invalid.push_str(&format!("let value_{index}: MissingType{index} = 0;\n"));
    }
    invalid.push_str("}\n");
    assert!(invalid.len() < SOURCE_BYTES);
    let diagnostic = run_failure(&invalid, Limits::default()).await;
    assert_eq!(diagnostic.kind, FailureKind::Compile);
    assert!(diagnostic.diagnostic.len() <= DIAGNOSTIC_BYTES);
    assert_eq!(diagnostic.source_hash.len(), 64);

    let stderr = run_failure(OVERSIZED_STDERR_SOURCE, Limits::default()).await;
    assert_eq!(stderr.kind, FailureKind::StderrLimit);
    assert!(stderr.diagnostic.contains("stderr"));

    let stdout = run_failure(OVERSIZED_STDOUT_SOURCE, Limits::default()).await;
    assert_eq!(stdout.kind, FailureKind::Protocol);
    assert!(stdout.diagnostic.contains("cap"));

    let cumulative_stdout = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::{run, Evidence};
fn main() { run(|context| { for _ in 0..25_000 { let _ = context.budget()?; } context.finish(Evidence(b"unexpected".to_vec())) }).unwrap(); }
"#;
    let stdout_total = run_failure(cumulative_stdout, Limits::default()).await;
    assert_eq!(stdout_total.kind, FailureKind::Protocol);
    assert!(stdout_total.diagnostic.contains("stdout byte limit"));

    let evidence_source = format!(
        "#![forbid(unsafe_code)]\nuse ycode_native_sdk::{{run, Evidence}};\nfn main() {{ run(|context| context.finish(Evidence(vec![b'x'; {}]))).unwrap(); }}\n",
        FINAL_EVIDENCE_BYTES + 1
    );
    let evidence = run_failure(&evidence_source, Limits::default()).await;
    assert_eq!(evidence.kind, FailureKind::EvidenceLimit);
    assert!(evidence.diagnostic.contains("before history boundary"));
}

#[tokio::test]
async fn malformed_oversized_and_version_mismatched_frames_are_terminal() {
    let malformed = run_failure(BAD_MAGIC_SOURCE, Limits::default()).await;
    assert_eq!(malformed.kind, FailureKind::Protocol);
    assert!(malformed.diagnostic.contains("magic"));
    let version = run_failure(BAD_VERSION_SOURCE, Limits::default()).await;
    assert_eq!(version.kind, FailureKind::Protocol);
    assert!(version.diagnostic.contains("version"));
    let oversized = run_failure(OVERSIZED_STDOUT_SOURCE, Limits::default()).await;
    assert_eq!(oversized.kind, FailureKind::Protocol);
    assert!(oversized.diagnostic.contains("cap"));
}

#[tokio::test]
async fn compiler_unavailable_wrong_version_and_timeout_fail_quickly() {
    let root = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let unavailable = NativeHost::new(
        root.path().join("missing-rustc"),
        tools().sdk.clone(),
        root.path().join("a"),
        Limits::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(unavailable.kind, FailureKind::CompilerUnavailable);
    assert_eq!(unavailable.process_reaped, None);
    assert!(unavailable.diagnostic.len() <= DIAGNOSTIC_BYTES);
    assert!(started.elapsed() < Duration::from_secs(2));

    let wrong = root.path().join("fake-rustc-wrong");
    symlink(&tools().binary, &wrong).unwrap();
    let wrong_error = NativeHost::new(
        wrong,
        tools().sdk.clone(),
        root.path().join("b"),
        Limits::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(wrong_error.kind, FailureKind::CompilerVersion);
    assert_eq!(wrong_error.process_reaped, Some(true));
    assert!(wrong_error.diagnostic.len() <= DIAGNOSTIC_BYTES);
    assert!(wrong_error.diagnostic.contains("expected 1.95.0"));

    let slow = root.path().join("fake-rustc-sleep");
    symlink(&tools().binary, &slow).unwrap();
    let limits = Limits {
        compile_timeout: Duration::from_millis(50),
        ..Limits::default()
    };
    let host = NativeHost::new(slow, tools().sdk.clone(), root.path().join("c"), limits)
        .await
        .unwrap();
    let artifact = host.prepare(SUCCESS_SOURCE.as_bytes()).unwrap();
    let error = host
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, FailureKind::CompileTimeout);
    assert_eq!(error.process_reaped, Some(true));
    assert!(!artifact.binary_exists());
    artifact.cleanup().unwrap();
}

#[tokio::test]
async fn compiler_removed_after_verification_owns_no_execute_child() {
    let root = tempfile::tempdir().unwrap();
    let rustc = root.path().join("fake-rustc-sleep");
    symlink(&tools().binary, &rustc).unwrap();
    let (events, mut event_rx) = mpsc::unbounded_channel();
    let host = NativeHost::new(
        rustc.clone(),
        tools().sdk.clone(),
        root.path().join("runs"),
        Limits::default(),
    )
    .await
    .unwrap()
    .with_events(events);
    std::fs::remove_file(rustc).unwrap();
    let artifact = host.prepare(SUCCESS_SOURCE.as_bytes()).unwrap();
    let error = host
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, FailureKind::CompilerUnavailable);
    assert_eq!(error.process_reaped, None);
    assert!(error.diagnostic.contains("failed to launch pinned rustc"));
    assert!(error.diagnostic.len() <= DIAGNOSTIC_BYTES);
    assert!(matches!(
        event_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(!artifact.binary_exists());
    assert!(artifact.source_path().is_file());
    artifact.cleanup().unwrap();
}

#[tokio::test]
async fn diagnostics_are_final_byte_bounded() {
    let root = tempfile::tempdir().unwrap();
    let invalid = root.path().join("fake-rustc-invalid");
    symlink(&tools().binary, &invalid).unwrap();
    let host = NativeHost::new(
        invalid,
        tools().sdk.clone(),
        root.path().join("invalid-runs"),
        Limits::default(),
    )
    .await
    .unwrap();
    let artifact = host.prepare(SUCCESS_SOURCE.as_bytes()).unwrap();
    let error = host
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, FailureKind::Compile);
    assert_eq!(error.process_reaped, Some(true));
    assert!(error.diagnostic.len() <= DIAGNOSTIC_BYTES);
    assert!(error.diagnostic.starts_with("error: invalid UTF-8"));
    assert!(error.diagnostic.contains("...[diagnostic truncated]"));
    assert!(
        error
            .diagnostic
            .ends_with(&format!("[source_hash={}]", error.source_hash))
    );
    artifact.cleanup().unwrap();
}

#[tokio::test]
async fn execution_lease_is_exclusive_before_a_second_compiler_spawn() {
    let root = tempfile::tempdir().unwrap();
    let slow = root.path().join("fake-rustc-sleep");
    symlink(&tools().binary, &slow).unwrap();
    let (events, mut event_rx) = mpsc::unbounded_channel();
    let host = NativeHost::new(
        slow,
        tools().sdk.clone(),
        root.path().join("exclusive-runs"),
        Limits {
            compile_timeout: Duration::from_secs(30),
            ..Limits::default()
        },
    )
    .await
    .unwrap()
    .with_events(events);
    let artifact = host.prepare(SUCCESS_SOURCE.as_bytes()).unwrap();
    let cancellation = CancellationToken::new();
    let mut execution = Box::pin(host.execute(&artifact, cancellation.clone()));
    let compiler_pid = loop {
        let event = tokio::select! {
            event = event_rx.recv() => event.unwrap(),
            result = &mut execution => panic!("execution settled before drop: {result:?}"),
        };
        if let HostEventKind::CompilerStarted(pid) = event.kind {
            break pid;
        }
    };
    assert!(process_exists(compiler_pid));
    assert_eq!(
        artifact.cleanup().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let second = host
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(second.kind, FailureKind::Admission);
    assert_eq!(second.process_reaped, None);
    assert!(second.diagnostic.contains("active execution owner"));
    assert!(process_exists(compiler_pid));
    assert!(matches!(
        event_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    cancellation.cancel();
    let first = tokio::time::timeout(Duration::from_secs(1), &mut execution)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(first.kind, FailureKind::Cancelled);
    assert_eq!(first.process_reaped, Some(true));
    assert_eq!(first.owned_tasks_after, 0);
    assert!(!process_exists(compiler_pid));
    assert!(!artifact.binary_exists());
    assert!(artifact.source_path().is_file());
    drop(execution);
    artifact.cleanup().unwrap();
}

#[tokio::test]
async fn dropped_compilation_reaps_reader_and_removes_partial_binary() {
    let root = tempfile::tempdir().unwrap();
    let partial = root.path().join("fake-rustc-partial");
    symlink(&tools().binary, &partial).unwrap();
    let (events, mut event_rx) = mpsc::unbounded_channel();
    let host = NativeHost::new(
        partial,
        tools().sdk.clone(),
        root.path().join("partial-runs"),
        Limits {
            compile_timeout: Duration::from_secs(30),
            ..Limits::default()
        },
    )
    .await
    .unwrap()
    .with_events(events);
    let artifact = host.prepare(SUCCESS_SOURCE.as_bytes()).unwrap();
    let mut execution = Box::pin(host.execute(&artifact, CancellationToken::new()));
    let compiler_pid = loop {
        let event = tokio::select! {
            event = event_rx.recv() => event.unwrap(),
            result = &mut execution => panic!("execution settled before drop: {result:?}"),
        };
        if let HostEventKind::CompilerStarted(pid) = event.kind {
            break pid;
        }
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        while !artifact.binary_exists() {
            assert!(process_exists(compiler_pid));
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("fake compiler did not emit its partial output");
    drop(execution);
    assert!(!process_exists(compiler_pid));
    assert!(!artifact.binary_exists());
    assert!(artifact.source_path().is_file());
    artifact.cleanup().unwrap();
}

#[tokio::test]
async fn dropped_workflow_execution_kills_group_and_removes_binary() {
    let root = tempfile::tempdir().unwrap();
    let (events, mut event_rx) = mpsc::unbounded_channel();
    let host = host(
        root.path(),
        Limits {
            capability_delay: Duration::from_secs(30),
            ..Limits::default()
        },
    )
    .await
    .with_events(events);
    let artifact = host.prepare(DESCENDANT_SOURCE.as_bytes()).unwrap();
    let mut execution = Box::pin(host.execute(&artifact, CancellationToken::new()));
    let mut workflow_pid = None;
    let descendant_pid = loop {
        let event = tokio::select! {
            event = event_rx.recv() => event.unwrap(),
            result = &mut execution => panic!("execution settled before drop: {result:?}"),
        };
        match event.kind {
            HostEventKind::WorkflowStarted(pid) => workflow_pid = Some(pid),
            HostEventKind::DescendantPid(pid) => break pid,
            _ => {}
        }
    };
    drop(execution);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = event_rx.recv().await.unwrap();
            if matches!(event.kind, HostEventKind::OwnedTasksDrained) {
                break;
            }
        }
    })
    .await
    .expect("dropped execution did not drain its owned capability tasks");
    assert!(!process_exists(workflow_pid.unwrap()));
    assert!(!process_exists(descendant_pid));
    assert!(!artifact.binary_exists());
    artifact.cleanup().unwrap();
}

#[tokio::test]
async fn workflow_timeout_crash_total_call_limit_and_backpressure_are_bounded() {
    let timeout = run_failure(
        CPU_SOURCE,
        Limits {
            workflow_timeout: Duration::from_millis(50),
            ..Limits::default()
        },
    )
    .await;
    assert_eq!(timeout.kind, FailureKind::WorkflowTimeout);
    assert_eq!(timeout.process_reaped, Some(true));

    let crash = run_failure(CRASH_SOURCE, Limits::default()).await;
    assert!(matches!(
        crash.kind,
        FailureKind::Protocol | FailureKind::ChildCrash
    ));
    assert_eq!(crash.process_reaped, Some(true));

    let calls = format!(
        "#![forbid(unsafe_code)]\nuse ycode_native_sdk::{{run, Evidence, Request}};\nfn main() {{ run(|context| {{ let mut tasks=Vec::new(); for i in 0..{} {{ tasks.push(context.spawn(Request::Fetch {{ query: i.to_string(), attempt: 0 }})?); }} for task in tasks {{ let _=context.join(task)?; }} context.finish(Evidence(b\"done\".to_vec())) }}).unwrap(); }}\n",
        codex_native_code_mode_host::TOTAL_CALLS + 1
    );
    let call_limit = run_failure(&calls, Limits::default()).await;
    assert_eq!(call_limit.kind, FailureKind::CallLimit);

    let root = tempfile::tempdir().unwrap();
    let host = host(
        root.path(),
        Limits {
            capability_delay: Duration::from_millis(20),
            ..Limits::default()
        },
    )
    .await;
    let artifact = host.prepare(SUCCESS_SOURCE.as_bytes()).unwrap();
    let report = host
        .execute(&artifact, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.peak_concurrent_calls, CONCURRENT_CALLS);
    assert_eq!(report.owned_tasks_after, 0);
    assert!(report.workflow_peak_bytes > 0);
    assert!(report.host_peak_bytes > 0);
    artifact.cleanup().unwrap();
}

async fn cancellation_samples(
    class: &str,
    source: &str,
    event_kind: fn(&HostEventKind) -> bool,
    require_cpu_ready: bool,
) -> Vec<u128> {
    let mut samples = Vec::new();
    for _ in 0..20 {
        let root = tempfile::tempdir().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let host = host(
            root.path(),
            Limits {
                capability_delay: Duration::from_secs(30),
                workflow_timeout: Duration::from_secs(30),
                ..Limits::default()
            },
        )
        .await
        .with_events(event_tx);
        let artifact = host.prepare(source.as_bytes()).unwrap();
        let cancellation = CancellationToken::new();
        let (failure, elapsed, exact_pid) = {
            let run = host.execute(&artifact, cancellation.clone());
            tokio::pin!(run);
            let exact_pid = loop {
                let event = tokio::select! { event = event_rx.recv() => event.unwrap(), result = &mut run => panic!("run settled before cancellation: {result:?}") };
                if event_kind(&event.kind) {
                    let pid = match event.kind {
                        HostEventKind::WorkflowStarted(pid) if require_cpu_ready => {
                            wait_for_cpu_usage(pid).await;
                            assert!(process_exists(pid));
                            Some(pid)
                        }
                        _ => None,
                    };
                    break pid;
                }
            };
            let started = Instant::now();
            cancellation.cancel();
            let failure = tokio::time::timeout(Duration::from_secs(1), &mut run)
                .await
                .unwrap()
                .unwrap_err();
            (failure, started.elapsed(), exact_pid)
        };
        samples.push(elapsed.as_micros());
        assert_eq!(failure.kind, FailureKind::Cancelled);
        assert_eq!(failure.owned_tasks_after, 0);
        assert_eq!(failure.process_reaped, Some(true));
        for pid in &failure.observed_descendant_pids {
            assert!(!process_exists(*pid), "surviving descendant pid {pid}");
        }
        if let Some(pid) = exact_pid {
            assert!(!process_exists(pid), "surviving CPU workflow pid {pid}");
        }
        assert!(!artifact.binary_exists());
        artifact.cleanup().unwrap();
    }
    print_stats(&format!("cancel-{class}"), &samples);
    samples
}

async fn wait_for_cpu_usage(pid: u32) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if process_user_time_ns(pid).is_some_and(|time| time >= 100_000) {
                return;
            }
            assert!(process_exists(pid), "CPU workflow exited before readiness");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("CPU workflow did not demonstrate CPU-loop readiness");
}

fn process_user_time_ns(pid: u32) -> Option<u64> {
    // SAFETY: proc_pid_rusage writes one initialized v4 structure for a concrete test child.
    let mut usage = unsafe { std::mem::zeroed::<libc::rusage_info_v4>() };
    // SAFETY: the pointer is valid for this v4 structure and lives through the call.
    let result = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V4,
            (&mut usage as *mut libc::rusage_info_v4).cast(),
        )
    };
    (result == 0).then_some(usage.ri_user_time)
}

#[tokio::test]
async fn cancellation_reaps_normal_cpu_and_descendant_runs() {
    let first = |event: &HostEventKind| matches!(event, HostEventKind::FirstCapability);
    let workflow = |event: &HostEventKind| matches!(event, HostEventKind::WorkflowStarted(_));
    let descendant = |event: &HostEventKind| matches!(event, HostEventKind::DescendantPid(_));
    for (class, samples) in [
        (
            "wait",
            cancellation_samples("wait", WAIT_SOURCE, first, false).await,
        ),
        (
            "cpu",
            cancellation_samples("cpu", CPU_SOURCE, workflow, true).await,
        ),
        (
            "descendant",
            cancellation_samples("descendant", DESCENDANT_SOURCE, descendant, false).await,
        ),
    ] {
        let (_, p95, max) = stats(&samples);
        assert!(p95 <= 250_000, "{class} p95 {p95}us exceeds 250ms");
        assert!(max <= 1_000_000, "{class} max {max}us exceeds 1s");
    }
}

#[tokio::test]
async fn controlled_host_disconnect_reaps_descendant() {
    let root = tempfile::tempdir().unwrap();
    let source_path = root.path().join("descendant.rs");
    std::fs::write(&source_path, DESCENDANT_SOURCE).unwrap();
    let mut child = Command::new(&tools().binary)
        .arg("run-controlled")
        .arg(&tools().rustc)
        .arg(&tools().sdk)
        .arg(root.path().join("runs"))
        .arg(&source_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let descendant_pid = loop {
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Some(pid) = line.strip_prefix("EVENT descendant ") {
            break pid.parse::<u32>().unwrap();
        }
    };
    drop(child.stdin.take());
    let status = tokio::time::timeout(Duration::from_secs(1), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success());
    let mut remaining_lines = Vec::new();
    while let Some(line) = lines.next_line().await.unwrap() {
        remaining_lines.push(line);
    }
    let remaining = remaining_lines.join("\n");
    assert!(remaining.contains("owned_tasks_after=0"), "{remaining}");
    assert!(
        remaining.contains("process_reaped=Some(true)"),
        "{remaining}"
    );
    assert!(!process_exists(descendant_pid));
}

#[tokio::test]
async fn real_host_process_meets_latency_and_determinism_gates() {
    let root = tempfile::tempdir().unwrap();
    let source_path = root.path().join("workflow.rs");
    std::fs::write(&source_path, SUCCESS_SOURCE).unwrap();
    let mut first = Vec::new();
    let mut final_times = Vec::new();
    let mut evidence = Vec::new();
    for sample in 0..21 {
        let output = Command::new(&tools().binary)
            .arg("run")
            .arg(&tools().rustc)
            .arg(&tools().sdk)
            .arg(root.path().join("runs"))
            .arg(&source_path)
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let line = String::from_utf8(output.stdout).unwrap();
        let fields = parse_fields(&line);
        let first_us = fields["first_us"].parse::<u128>().unwrap();
        let final_us = fields["final_us"].parse::<u128>().unwrap();
        if sample == 0 {
            println!("COLD_LIKE first_us={first_us} final_us={final_us}");
            assert!(final_us <= 10_000_000);
        } else {
            first.push(first_us);
            final_times.push(final_us);
        }
        evidence.push(fields["evidence"].clone());
    }
    print_stats("e2e-first", &first);
    print_stats("e2e-final", &final_times);
    assert!(stats(&first).1 <= 5_000_000);
    assert!(stats(&final_times).1 <= 5_000_000);
    assert!(evidence.windows(2).all(|pair| pair[0] == pair[1]));

    let host = host(root.path(), Limits::default()).await;
    let mut deterministic = Vec::new();
    for _ in 0..79 {
        let artifact = host.prepare(SUCCESS_SOURCE.as_bytes()).unwrap();
        let report = host
            .execute(&artifact, CancellationToken::new())
            .await
            .unwrap();
        deterministic.push(report.evidence);
        artifact.cleanup().unwrap();
    }
    evidence.extend(deterministic.into_iter().map(|bytes| hex(&bytes)));
    assert_eq!(evidence.len(), 100);
    assert!(evidence.windows(2).all(|pair| pair[0] == pair[1]));
    println!("DETERMINISM identical=100/100 evidence={}", evidence[0]);
}

fn parse_fields(line: &str) -> std::collections::HashMap<String, String> {
    line.split_whitespace()
        .skip(1)
        .filter_map(|field| field.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn stats(samples: &[u128]) -> (u128, u128, u128) {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let p50 = sorted[(sorted.len() * 50).div_ceil(100) - 1];
    let p95 = sorted[(sorted.len() * 95).div_ceil(100) - 1];
    (p50, p95, *sorted.last().unwrap())
}

fn print_stats(label: &str, samples: &[u128]) {
    let (p50, p95, max) = stats(samples);
    println!(
        "LEDGER {label} raw_us={} p50_us={p50} p95_us={p95} max_us={max}",
        samples
            .iter()
            .map(u128::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn constants_are_intentionally_bounded() {
    assert!(WORKFLOW_STDERR_BYTES <= 64 * 1024);
    assert!(FINAL_EVIDENCE_BYTES < codex_native_code_mode_host::FRAME_BYTES);
}

#[test]
fn production_default_members_build_and_install_surfaces_are_unwired() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = std::fs::read_to_string(manifest_dir.join("../../Cargo.toml")).unwrap();
    assert!(workspace.contains("default-members = [\"cli\", \"code-mode-host\"]"));
    let build =
        std::fs::read_to_string(manifest_dir.join("../../../scripts/build/build-product.sh"))
            .unwrap();
    let install =
        std::fs::read_to_string(manifest_dir.join("../../../scripts/install/install.sh")).unwrap();
    assert!(!build.contains("codex-native-code-mode-spike"));
    assert!(!install.contains("codex-native-code-mode-spike"));
    let sdk_manifest =
        std::fs::read_to_string(manifest_dir.join("../native-code-mode-sdk/Cargo.toml")).unwrap();
    assert!(!sdk_manifest.contains("[dependencies]"));
}
