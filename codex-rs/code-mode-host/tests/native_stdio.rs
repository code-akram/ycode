#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use codex_code_mode_protocol::host::Capability;
use codex_code_mode_protocol::host::CapabilitySet;
use codex_code_mode_protocol::host::ClientHello;
use codex_code_mode_protocol::host::ClientToHost;
use codex_code_mode_protocol::host::DelegateRequest;
use codex_code_mode_protocol::host::DelegateResponse;
use codex_code_mode_protocol::host::FramedReader;
use codex_code_mode_protocol::host::FramedWriter;
use codex_code_mode_protocol::host::HostRequest;
use codex_code_mode_protocol::host::HostResponse;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::NATIVE_RUST_V1_CAPABILITY;
use codex_code_mode_protocol::host::NativeExecuteRequest;
use codex_code_mode_protocol::host::NativeProgressPhase;
use codex_code_mode_protocol::host::NativeToolOutcome;
use codex_code_mode_protocol::host::NativeToolRequest;
use codex_code_mode_protocol::host::ProtocolVersion;
use codex_code_mode_protocol::host::RequestId;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::SupportedProtocolVersions;
use codex_code_mode_protocol::host::WireResult;
use tokio::process::ChildStdout;
use tokio::process::Command;

const SOURCE: &str = include_str!("../../native-code-mode-runtime/tests/fixtures/workflow.rs");
const THREAD_ID: &str = "50000000-0000-4000-8000-000000000005";
const ESCAPING_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::{run, Evidence};
fn main() { run(|context| context.finish(Evidence { version: 1, summary: "\0".repeat(3_000), verified: vec![], disputed: vec![], unresolved: vec![], artifact_refs: vec![], partial_failures: vec![], provenance_ids: vec![] })).unwrap(); }
"#;
const BROKEN_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::Context;
fn main() { let _: MissingType = std::mem::size_of::<Context>(); }
"#;
const NO_TOOL_DELAY_SOURCE: &str = r#"#![forbid(unsafe_code)]
use std::time::Duration;
use ycode_native_sdk::{run, Evidence};
fn main() {
    run(|context| {
        std::thread::sleep(Duration::from_millis(200));
        context.finish(Evidence {
            version: 1,
            summary: "no-tool workflow completed".to_string(),
            verified: vec!["workflow child ran before finishing".to_string()],
            disputed: vec![],
            unresolved: vec![],
            artifact_refs: vec![],
            partial_failures: vec![],
            provenance_ids: vec![],
        })
    }).unwrap();
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
        let error = context.finish(Evidence {
            version: 1,
            summary: "retained invalid evidence shape".to_string(),
            verified: vec![],
            disputed: vec![],
            unresolved: vec![],
            artifact_refs: vec!["shell:git-status".to_string()],
            partial_failures: vec![],
            provenance_ids: vec!["git-status".to_string()],
        }).expect_err("invented provenance must fail");
        std::fs::write("finish-error.txt", format!("{error}"))?;
        Ok(())
    }).unwrap();
}
"#;
const CORRECTED_INVENTORY_PROBE_SOURCE: &str = r#"#![forbid(unsafe_code)]
use ycode_native_sdk::{run, Evidence, Outcome, Request};
fn main() {
    run(|context| {
        let mut provenance_ids = Vec::new();
        for command in [
            "find . -maxdepth 1 -mindepth 1 -print | LC_ALL=C sort",
            "command -v cargo",
        ] {
            let outcome = context.call(Request::Shell {
                command: command.to_string(),
                workdir: None,
                timeout_ms: 5_000,
            })?;
            provenance_ids.push(match outcome {
                Outcome::Success { call_id, .. }
                | Outcome::Retry { call_id, .. }
                | Outcome::Failure { call_id, .. } => call_id,
            });
        }
        context.finish(Evidence {
            version: 1,
            summary: "inventory and tool probe completed".to_string(),
            verified: vec!["top-level inventory inspected before ecosystem checks".to_string()],
            disputed: vec![],
            unresolved: vec![],
            artifact_refs: vec![],
            partial_failures: vec![],
            provenance_ids,
        })
    }).unwrap();
}
"#;

#[tokio::test]
async fn adjacent_stdio_native_lane_is_capability_gated_typed_and_fast() {
    let home = tempfile::tempdir().expect("temporary ycode home");
    let mut child = Command::new(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    )
    .env("CODEX_HOME", home.path())
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn adjacent host");
    let mut writer = FramedWriter::new(child.stdin.take().expect("host stdin"));
    let mut reader = FramedReader::new(child.stdout.take().expect("host stdout"));
    let capability = Capability::new(NATIVE_RUST_V1_CAPABILITY).unwrap();
    writer
        .write(&ClientToHost::ClientHello(
            ClientHello::new(
                SupportedProtocolVersions::try_new([ProtocolVersion::V1]).unwrap(),
                CapabilitySet::try_new([capability.clone()]).unwrap(),
                CapabilitySet::empty(),
            )
            .unwrap(),
        ))
        .await
        .unwrap();
    let HostToClient::HostHello(hello) = reader.read().await.unwrap().unwrap() else {
        panic!("native stdio handshake was not accepted");
    };
    assert!(hello.capabilities().contains(&capability));

    let mut first_delegate_ms = Vec::new();
    let mut final_ms = Vec::new();
    for sample in 0_i64..21 {
        let run_id = format!("60000000-0000-4000-8000-{sample:012x}");
        let started = Instant::now();
        writer
            .write(&ClientToHost::Request {
                id: RequestId::new(sample + 1),
                request: HostRequest::NativeExecute {
                    request: NativeExecuteRequest {
                        session_id: SessionId::new("native-measurement").unwrap(),
                        thread_id: THREAD_ID.to_string(),
                        run_id,
                        attempt: 1,
                        task: "inspect and update deterministic workspace files".to_string(),
                        source: SOURCE.to_string(),
                    },
                },
            })
            .await
            .unwrap();
        let mut observed_first = false;
        let mut observed_workflow_start = false;
        loop {
            let message = tokio::time::timeout(Duration::from_secs(10), reader.read())
                .await
                .expect("native lane response timeout")
                .unwrap()
                .expect("native host closed unexpectedly");
            match message {
                HostToClient::NativeProgress {
                    id,
                    session_id,
                    thread_id,
                    phase: NativeProgressPhase::WorkflowStarted,
                    ..
                } => {
                    assert_eq!(id, RequestId::new(sample + 1));
                    assert_eq!(session_id.as_str(), "native-measurement");
                    assert_eq!(thread_id, THREAD_ID);
                    assert!(!observed_workflow_start);
                    assert!(!observed_first);
                    observed_workflow_start = true;
                }
                HostToClient::DelegateRequest {
                    id,
                    request: DelegateRequest::NativeInvokeTool { request, .. },
                    ..
                } => {
                    assert!(observed_workflow_start);
                    if !observed_first {
                        first_delegate_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
                        observed_first = true;
                    }
                    let outcome = fake_outcome(request);
                    writer
                        .write(&ClientToHost::DelegateResponse {
                            id,
                            result: WireResult::Ok {
                                value: DelegateResponse::NativeToolResult { outcome },
                            },
                        })
                        .await
                        .unwrap();
                }
                HostToClient::Response {
                    id,
                    result:
                        WireResult::Ok {
                            value: HostResponse::NativeCompleted { evidence, .. },
                        },
                } => {
                    assert!(observed_workflow_start);
                    assert_eq!(id, RequestId::new(sample + 1));
                    assert_eq!(evidence.version, 1);
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
                    assert_eq!(evidence.partial_failures.len(), 1);
                    final_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
                    break;
                }
                other => panic!("unexpected native host message: {other:?}"),
            }
        }
    }
    assert!(
        first_delegate_ms[0] <= 10_000.0,
        "cold-like sample exceeded gate"
    );
    assert!(nearest_rank(&first_delegate_ms[1..], 95) <= 5_000.0);
    assert!(nearest_rank(&final_ms[1..], 95) <= 5_000.0);
    println!("native-first-delegate-ms={first_delegate_ms:?}");
    println!("native-final-evidence-ms={final_ms:?}");

    drop(writer);
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("host shutdown timeout")
        .expect("wait for host");
    assert!(status.success());
}

#[tokio::test]
async fn native_wire_evidence_cap_and_repair_finalize_are_truthful() {
    let home = tempfile::tempdir().expect("temporary ycode home");
    let mut child = Command::new(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    )
    .env("CODEX_HOME", home.path())
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn adjacent host");
    let mut writer = FramedWriter::new(child.stdin.take().expect("host stdin"));
    let mut reader = FramedReader::new(child.stdout.take().expect("host stdout"));
    let capability = Capability::new(NATIVE_RUST_V1_CAPABILITY).unwrap();
    writer
        .write(&ClientToHost::ClientHello(
            ClientHello::new(
                SupportedProtocolVersions::try_new([ProtocolVersion::V1]).unwrap(),
                CapabilitySet::try_new([capability]).unwrap(),
                CapabilitySet::empty(),
            )
            .unwrap(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        reader.read().await.unwrap().unwrap(),
        HostToClient::HostHello(_)
    ));

    let escaping_run = "70000000-0000-4000-8000-000000000007";
    writer
        .write(&ClientToHost::Request {
            id: RequestId::new(100),
            request: HostRequest::NativeExecute {
                request: NativeExecuteRequest {
                    session_id: SessionId::new("native-boundary").unwrap(),
                    thread_id: THREAD_ID.into(),
                    run_id: escaping_run.into(),
                    attempt: 1,
                    task: "wire boundary".into(),
                    source: ESCAPING_SOURCE.into(),
                },
            },
        })
        .await
        .unwrap();
    let HostToClient::Response {
        result:
            WireResult::Ok {
                value: HostResponse::NativeFailed { failure, .. },
            },
        ..
    } = read_non_progress(&mut reader).await
    else {
        panic!("oversized wire evidence was not rejected");
    };
    assert_eq!(failure.kind, "EvidenceLimit");
    assert!(failure.diagnostic.contains("evidence.json"));
    let evidence_path = home
        .path()
        .join("native-code-mode/v1/sessions")
        .join(THREAD_ID)
        .join("runs")
        .join(escaping_run)
        .join("evidence.json");
    let evidence_len = std::fs::metadata(&evidence_path).unwrap().len();
    assert!(evidence_len > 16 * 1024 && evidence_len <= 128 * 1024);

    let repair_run = "80000000-0000-4000-8000-000000000008";
    writer
        .write(&ClientToHost::Request {
            id: RequestId::new(101),
            request: HostRequest::NativeExecute {
                request: NativeExecuteRequest {
                    session_id: SessionId::new("native-boundary").unwrap(),
                    thread_id: THREAD_ID.into(),
                    run_id: repair_run.into(),
                    attempt: 1,
                    task: "repair boundary".into(),
                    source: BROKEN_SOURCE.into(),
                },
            },
        })
        .await
        .unwrap();
    let HostToClient::Response {
        result:
            WireResult::Ok {
                value: HostResponse::NativeFailed { failure, .. },
            },
        ..
    } = read_non_progress(&mut reader).await
    else {
        panic!("compile rejection was not returned");
    };
    assert_eq!(failure.kind, "Compile");
    let manifest_path = home
        .path()
        .join("native-code-mode/v1/sessions")
        .join(THREAD_ID)
        .join("runs")
        .join(repair_run)
        .join("manifest.json");
    let pending: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    assert!(pending["completed_at_unix_ms"].is_null());

    writer
        .write(&ClientToHost::Request {
            id: RequestId::new(102),
            request: HostRequest::NativeFinalize {
                session_id: SessionId::new("native-boundary").unwrap(),
                thread_id: THREAD_ID.into(),
                run_id: repair_run.into(),
            },
        })
        .await
        .unwrap();
    assert!(matches!(
        reader.read().await.unwrap().unwrap(),
        HostToClient::Response {
            result: WireResult::Ok {
                value: HostResponse::NativeFinalized { .. }
            },
            ..
        }
    ));
    let finalized: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    assert!(finalized["completed_at_unix_ms"].is_number());

    drop(writer);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

fn fake_outcome(request: NativeToolRequest) -> NativeToolOutcome {
    match request {
        NativeToolRequest::Shell { command, .. } if command == "summarize:workspace/item-2.txt" => {
            NativeToolOutcome::Retry {
                reason: "transient-item-2".to_string(),
            }
        }
        NativeToolRequest::Shell {
            command,
            workdir,
            timeout_ms,
        } => NativeToolOutcome::Success {
            output: format!("shell:{command}:{workdir:?}:{timeout_ms}").into_bytes(),
        },
        NativeToolRequest::ApplyPatch { patch } => NativeToolOutcome::Success {
            output: format!("patch:{}", patch.len()).into_bytes(),
        },
    }
}

async fn read_non_progress(reader: &mut FramedReader<ChildStdout>) -> HostToClient {
    loop {
        let message = reader
            .read()
            .await
            .expect("read native host frame")
            .expect("native host closed unexpectedly");
        if !matches!(message, HostToClient::NativeProgress { .. }) {
            return message;
        }
    }
}

#[tokio::test]
async fn workflow_start_progress_precedes_no_tool_rust_work_and_finish() {
    let home = tempfile::tempdir().expect("temporary ycode home");
    let mut child = Command::new(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    )
    .env("CODEX_HOME", home.path())
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn adjacent host");
    let mut writer = FramedWriter::new(child.stdin.take().expect("host stdin"));
    let mut reader = FramedReader::new(child.stdout.take().expect("host stdout"));
    writer
        .write(&ClientToHost::ClientHello(
            ClientHello::new(
                SupportedProtocolVersions::try_new([ProtocolVersion::V1]).unwrap(),
                CapabilitySet::try_new([Capability::new(NATIVE_RUST_V1_CAPABILITY).unwrap()])
                    .unwrap(),
                CapabilitySet::empty(),
            )
            .unwrap(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        reader.read().await.unwrap().unwrap(),
        HostToClient::HostHello(_)
    ));

    let request_id = RequestId::new(900);
    let run_id = "90000000-0000-4000-8000-000000000009";
    writer
        .write(&ClientToHost::Request {
            id: request_id,
            request: HostRequest::NativeExecute {
                request: NativeExecuteRequest {
                    session_id: SessionId::new("native-progress").unwrap(),
                    thread_id: THREAD_ID.to_string(),
                    run_id: run_id.to_string(),
                    attempt: 1,
                    task: "prove actual workflow start".to_string(),
                    source: NO_TOOL_DELAY_SOURCE.to_string(),
                },
            },
        })
        .await
        .unwrap();
    let started = Instant::now();
    let HostToClient::NativeProgress {
        id,
        session_id,
        thread_id,
        run_id: observed_run_id,
        phase: NativeProgressPhase::WorkflowStarted,
    } = tokio::time::timeout(Duration::from_secs(10), reader.read())
        .await
        .expect("workflow start timeout")
        .unwrap()
        .unwrap()
    else {
        panic!("workflow start was not the first native execution message");
    };
    assert_eq!(id, request_id);
    assert_eq!(session_id.as_str(), "native-progress");
    assert_eq!(thread_id, THREAD_ID);
    assert_eq!(observed_run_id, run_id);
    let progress_at = started.elapsed();

    let HostToClient::Response {
        id,
        result:
            WireResult::Ok {
                value: HostResponse::NativeCompleted { evidence, .. },
            },
    } = tokio::time::timeout(Duration::from_secs(2), reader.read())
        .await
        .expect("no-tool workflow timeout")
        .unwrap()
        .unwrap()
    else {
        panic!("no-tool workflow did not complete after its start progress");
    };
    assert_eq!(id, request_id);
    assert_eq!(evidence.summary, "no-tool workflow completed");
    assert!(
        started.elapsed().saturating_sub(progress_at) >= Duration::from_millis(100),
        "workflow-start progress must precede the child's deliberate Rust wait"
    );

    drop(writer);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn retained_invalid_then_corrected_stdio_shape_returns_concrete_protocol_failure() {
    let home = tempfile::tempdir().expect("temporary ycode home");
    let mut child = Command::new(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    )
    .env("CODEX_HOME", home.path())
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn adjacent host");
    let mut writer = FramedWriter::new(child.stdin.take().expect("host stdin"));
    let mut reader = FramedReader::new(child.stdout.take().expect("host stdout"));
    writer
        .write(&ClientToHost::ClientHello(
            ClientHello::new(
                SupportedProtocolVersions::try_new([ProtocolVersion::V1]).unwrap(),
                CapabilitySet::try_new([Capability::new(NATIVE_RUST_V1_CAPABILITY).unwrap()])
                    .unwrap(),
                CapabilitySet::empty(),
            )
            .unwrap(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        reader.read().await.unwrap().unwrap(),
        HostToClient::HostHello(_)
    ));

    let session_id = SessionId::new("retained-protocol-shape").unwrap();
    let invalid_run = "91000000-0000-4000-8000-000000000009";
    writer
        .write(&ClientToHost::Request {
            id: RequestId::new(910),
            request: HostRequest::NativeExecute {
                request: NativeExecuteRequest {
                    session_id: session_id.clone(),
                    thread_id: THREAD_ID.to_string(),
                    run_id: invalid_run.to_string(),
                    attempt: 1,
                    task: "retained invalid provenance regression".to_string(),
                    source: RETAINED_INVALID_EVIDENCE_SOURCE.to_string(),
                },
            },
        })
        .await
        .unwrap();
    let mut invalid_calls = 0;
    let mut invalid_started = false;
    let invalid_failure = loop {
        match tokio::time::timeout(Duration::from_secs(10), reader.read())
            .await
            .expect("invalid workflow timeout")
            .unwrap()
            .expect("host must return a bounded failure, not EOF")
        {
            HostToClient::NativeProgress {
                id,
                phase: NativeProgressPhase::WorkflowStarted,
                ..
            } => {
                assert_eq!(id, RequestId::new(910));
                assert!(!invalid_started);
                invalid_started = true;
            }
            HostToClient::DelegateRequest {
                id,
                request: DelegateRequest::NativeInvokeTool { request, .. },
                ..
            } => {
                assert!(invalid_started);
                invalid_calls += 1;
                writer
                    .write(&ClientToHost::DelegateResponse {
                        id,
                        result: WireResult::Ok {
                            value: DelegateResponse::NativeToolResult {
                                outcome: fake_outcome(request),
                            },
                        },
                    })
                    .await
                    .unwrap();
            }
            HostToClient::Response {
                id,
                result:
                    WireResult::Ok {
                        value: HostResponse::NativeFailed { failure, .. },
                    },
            } => {
                assert_eq!(id, RequestId::new(910));
                break failure;
            }
            other => panic!("unexpected invalid-workflow message: {other:?}"),
        }
    };
    assert_eq!(invalid_calls, 5);
    assert_eq!(invalid_failure.kind, "Protocol");
    assert!(
        invalid_failure
            .diagnostic
            .starts_with("evidence provenance id does not identify a joined native call")
    );
    assert!(invalid_failure.diagnostic.contains("[source_hash="));
    let invalid_dir = home
        .path()
        .join("native-code-mode/v1/sessions")
        .join(THREAD_ID)
        .join("runs")
        .join(invalid_run);
    assert_eq!(
        std::fs::read_to_string(invalid_dir.join("finish-error.txt")).unwrap(),
        "host error: evidence provenance id does not identify a joined native call"
    );
    assert_eq!(
        std::fs::read_dir(invalid_dir.join("calls"))
            .unwrap()
            .count(),
        10
    );
    let invalid_manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(invalid_dir.join("manifest.json")).unwrap()).unwrap();
    assert!(invalid_manifest["completed_at_unix_ms"].is_number());

    let corrected_run = "92000000-0000-4000-8000-000000000009";
    writer
        .write(&ClientToHost::Request {
            id: RequestId::new(920),
            request: HostRequest::NativeExecute {
                request: NativeExecuteRequest {
                    session_id,
                    thread_id: THREAD_ID.to_string(),
                    run_id: corrected_run.to_string(),
                    attempt: 1,
                    task: "corrected inventory and tool probe".to_string(),
                    source: CORRECTED_INVENTORY_PROBE_SOURCE.to_string(),
                },
            },
        })
        .await
        .unwrap();
    let mut corrected_calls = 0;
    let corrected_evidence = loop {
        match tokio::time::timeout(Duration::from_secs(10), reader.read())
            .await
            .expect("corrected workflow timeout")
            .unwrap()
            .expect("corrected workflow response")
        {
            HostToClient::NativeProgress {
                phase: NativeProgressPhase::WorkflowStarted,
                ..
            } => {}
            HostToClient::DelegateRequest {
                id,
                request: DelegateRequest::NativeInvokeTool { request, .. },
                ..
            } => {
                corrected_calls += 1;
                writer
                    .write(&ClientToHost::DelegateResponse {
                        id,
                        result: WireResult::Ok {
                            value: DelegateResponse::NativeToolResult {
                                outcome: fake_outcome(request),
                            },
                        },
                    })
                    .await
                    .unwrap();
            }
            HostToClient::Response {
                id,
                result:
                    WireResult::Ok {
                        value: HostResponse::NativeCompleted { evidence, .. },
                    },
            } => {
                assert_eq!(id, RequestId::new(920));
                break evidence;
            }
            other => panic!("unexpected corrected-workflow message: {other:?}"),
        }
    };
    assert_eq!(corrected_calls, 2);
    assert_eq!(corrected_evidence.provenance_ids.len(), 2);
    assert_eq!(corrected_evidence.artifact_refs.len(), 4);
    for provenance in &corrected_evidence.provenance_ids {
        assert!(
            corrected_evidence
                .artifact_refs
                .contains(&format!("calls/{provenance}.request.bin"))
        );
        assert!(
            corrected_evidence
                .artifact_refs
                .contains(&format!("calls/{provenance}.result.bin"))
        );
    }

    drop(writer);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

fn nearest_rank(samples: &[f64], percentile: usize) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (percentile * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}
