use std::path::PathBuf;
use std::sync::Arc;

use codex_code_mode_protocol::host::DelegateRequest;
use codex_code_mode_protocol::host::DelegateResponse;
use codex_code_mode_protocol::host::NativeEvidence;
use codex_code_mode_protocol::host::NativeExecuteRequest;
use codex_code_mode_protocol::host::NativeFailure;
use codex_code_mode_protocol::host::NativeToolOutcome;
use codex_code_mode_protocol::host::NativeToolRequest;
use codex_code_mode_protocol::host::SessionId;
use codex_native_code_mode_runtime::FINAL_EVIDENCE_BYTES;
use codex_native_code_mode_runtime::Limits;
use codex_native_code_mode_runtime::NativeCall;
use codex_native_code_mode_runtime::NativeCapabilityDelegate;
use codex_native_code_mode_runtime::NativeDelegateFuture;
use codex_native_code_mode_runtime::NativeHost;
use codex_native_code_mode_runtime::NativeOutcome;
use codex_native_code_mode_runtime::NativeRequest;
use codex_native_code_mode_runtime::RunFailure;
use codex_native_code_mode_runtime::decode_evidence;
use codex_native_code_mode_runtime::finalize_run;
use codex_native_code_mode_runtime::materialize_sdk;
use tokio_util::sync::CancellationToken;

use crate::peer::HostPeer;

const SDK_BYTES: &[u8] = include_bytes!(env!("YCODE_NATIVE_SDK_RLIB"));
const SDK_HASH: &str = env!("YCODE_NATIVE_SDK_HASH");

pub(super) enum NativeExecutionResult {
    Completed {
        session_id: SessionId,
        thread_id: String,
        run_id: String,
        source_hash: String,
        evidence: NativeEvidence,
    },
    Failed {
        session_id: SessionId,
        thread_id: String,
        run_id: String,
        failure: NativeFailure,
    },
}

pub(super) async fn execute(
    peer: Arc<HostPeer>,
    request: NativeExecuteRequest,
    cancellation: CancellationToken,
) -> NativeExecutionResult {
    let session_id = request.session_id.clone();
    let thread_id = request.thread_id.clone();
    let run_id = request.run_id.clone();
    let result = execute_inner(peer, &request, cancellation).await;
    match result {
        Ok((source_hash, evidence)) => NativeExecutionResult::Completed {
            session_id,
            thread_id,
            run_id,
            source_hash,
            evidence,
        },
        Err(failure) => NativeExecutionResult::Failed {
            session_id,
            thread_id,
            run_id,
            failure: wire_failure(failure),
        },
    }
}

async fn execute_inner(
    peer: Arc<HostPeer>,
    request: &NativeExecuteRequest,
    cancellation: CancellationToken,
) -> Result<(String, NativeEvidence), RunFailure> {
    let root = native_store_root().map_err(admission_failure)?;
    let sdk = materialize_sdk(&root, SDK_BYTES, SDK_HASH).map_err(|error| {
        admission_failure(format!("failed to materialize embedded SDK: {error}"))
    })?;
    let delegate = Arc::new(RemoteNativeDelegate {
        peer,
        session_id: request.session_id.clone(),
        run_id: request.run_id.clone(),
    });
    let host = NativeHost::discover(sdk, root, Limits::default(), delegate).await?;
    let artifact = host.prepare_run(
        &request.thread_id,
        &request.run_id,
        &request.task,
        request.attempt,
        request.source.as_bytes(),
    )?;
    let report = host.execute(&artifact, cancellation).await?;
    let evidence = decode_evidence(&report.evidence)
        .map_err(|error| admission_failure(format!("validated evidence decode failed: {error}")))?;
    let evidence = NativeEvidence {
        version: evidence.version,
        summary: evidence.summary,
        verified: evidence.verified,
        disputed: evidence.disputed,
        unresolved: evidence.unresolved,
        artifact_refs: evidence.artifact_refs,
        partial_failures: evidence.partial_failures,
        provenance_ids: evidence.provenance_ids,
    };
    let wire_bytes = evidence.exact_json_wire_len().map_err(|error| {
        RunFailure::evidence_limit(
            report.source_hash.clone(),
            format!(
                "failed to measure final Evidence wire representation: {error}; local evidence retained at evidence.json"
            ),
        )
    })?;
    if wire_bytes > FINAL_EVIDENCE_BYTES {
        return Err(RunFailure::evidence_limit(
            report.source_hash,
            format!(
                "final Evidence JSON wire representation is {wire_bytes} bytes, exceeding {FINAL_EVIDENCE_BYTES}; local evidence retained at evidence.json"
            ),
        ));
    }
    Ok((report.source_hash, evidence))
}

pub(super) fn finalize(thread_id: &str, run_id: &str) -> Result<(), String> {
    let root = native_store_root()?;
    finalize_run(&root, thread_id, run_id)
        .map_err(|error| format!("failed to finalize native run: {error}"))
}

fn native_store_root() -> Result<PathBuf, String> {
    let home = match std::env::var_os("CODEX_HOME") {
        Some(path) => {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err("CODEX_HOME must be an absolute path".to_string());
            }
            path
        }
        None => {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| "HOME is unavailable and CODEX_HOME was not set".to_string())?;
            PathBuf::from(home).join(".ycode")
        }
    };
    Ok(home.join("native-code-mode/v1"))
}

fn admission_failure(message: String) -> RunFailure {
    RunFailure::admission(message)
}

fn wire_failure(failure: RunFailure) -> NativeFailure {
    NativeFailure {
        kind: format!("{:?}", failure.kind),
        source_hash: failure.source_hash,
        diagnostic: failure.diagnostic,
        process_reaped: failure.process_reaped,
    }
}

struct RemoteNativeDelegate {
    peer: Arc<HostPeer>,
    session_id: SessionId,
    run_id: String,
}

impl NativeCapabilityDelegate for RemoteNativeDelegate {
    fn invoke<'a>(
        &'a self,
        call: NativeCall,
        cancellation: CancellationToken,
    ) -> NativeDelegateFuture<'a> {
        Box::pin(async move {
            let request = match call.request {
                NativeRequest::Shell {
                    command,
                    workdir,
                    timeout_ms,
                } => NativeToolRequest::Shell {
                    command,
                    workdir,
                    timeout_ms,
                },
                NativeRequest::ApplyPatch { patch } => NativeToolRequest::ApplyPatch { patch },
            };
            match self
                .peer
                .call_native(
                    self.session_id.clone(),
                    DelegateRequest::NativeInvokeTool {
                        run_id: self.run_id.clone(),
                        call_id: call.call_id,
                        request,
                    },
                    cancellation,
                )
                .await?
            {
                DelegateResponse::NativeToolResult { outcome } => Ok(match outcome {
                    NativeToolOutcome::Success { output } => NativeOutcome::Success(output),
                    NativeToolOutcome::Retry { reason } => NativeOutcome::Retry(reason),
                    NativeToolOutcome::Failure { message } => NativeOutcome::Failure(message),
                }),
                _ => Err("code-mode client returned an invalid native tool result".to_string()),
            }
        })
    }
}
