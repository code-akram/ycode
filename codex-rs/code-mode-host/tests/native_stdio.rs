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
use codex_code_mode_protocol::host::NativeToolOutcome;
use codex_code_mode_protocol::host::NativeToolRequest;
use codex_code_mode_protocol::host::ProtocolVersion;
use codex_code_mode_protocol::host::RequestId;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::SupportedProtocolVersions;
use codex_code_mode_protocol::host::WireResult;
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
        loop {
            let message = tokio::time::timeout(Duration::from_secs(10), reader.read())
                .await
                .expect("native lane response timeout")
                .unwrap()
                .expect("native host closed unexpectedly");
            match message {
                HostToClient::DelegateRequest {
                    id,
                    request: DelegateRequest::NativeInvokeTool { request, .. },
                    ..
                } => {
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
    } = reader.read().await.unwrap().unwrap()
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
    } = reader.read().await.unwrap().unwrap()
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

fn nearest_rank(samples: &[f64], percentile: usize) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (percentile * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}
