use std::future::Future;
use std::pin::Pin;

use codex_code_mode_protocol::host::NativeEvidence;
use codex_code_mode_protocol::host::NativeFailure;
use codex_code_mode_protocol::host::NativeToolOutcome;
use codex_code_mode_protocol::host::NativeToolRequest;
use tokio_util::sync::CancellationToken;

pub const NATIVE_TASK_BYTES: usize = 16 * 1024;
pub const NATIVE_SOURCE_BYTES: usize = 64 * 1024;
pub const NATIVE_DIAGNOSTIC_BYTES: usize = 64 * 1024;
pub const NATIVE_EVIDENCE_BYTES: usize = 16 * 1024;
pub const NATIVE_CALL_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NativeRunIdentity {
    pub session_id: String,
    pub thread_id: String,
    pub run_id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NativeExecute {
    pub identity: NativeRunIdentity,
    pub attempt: u8,
    pub task: String,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NativeExecution {
    Completed {
        identity: NativeRunIdentity,
        source_hash: String,
        evidence: NativeEvidence,
    },
    Failed {
        identity: NativeRunIdentity,
        failure: NativeFailure,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct NativeToolInvocation {
    pub identity: NativeRunIdentity,
    pub runtime_call_id: String,
    pub request: NativeToolRequest,
}

pub type NativeToolFuture<'a> =
    Pin<Box<dyn Future<Output = Result<NativeToolOutcome, String>> + Send + 'a>>;

pub trait NativeCodeModeDelegate: Send + Sync {
    fn invoke<'a>(
        &'a self,
        invocation: NativeToolInvocation,
        cancellation: CancellationToken,
    ) -> NativeToolFuture<'a>;
}

pub(crate) fn validate_execute(request: &NativeExecute) -> Result<(), String> {
    validate_identity(&request.identity)?;
    if request.attempt != 1 && request.attempt != 2 {
        return Err("native attempt must be 1 or 2".to_string());
    }
    bounded("native task", &request.task, NATIVE_TASK_BYTES)?;
    bounded("native source", &request.source, NATIVE_SOURCE_BYTES)?;
    Ok(())
}

pub(crate) fn validate_identity(identity: &NativeRunIdentity) -> Result<(), String> {
    bounded_identifier("native session ID", &identity.session_id)?;
    canonical_uuid("native thread ID", &identity.thread_id)?;
    canonical_uuid("native run ID", &identity.run_id)
}

pub(crate) fn validate_tool_request(request: &NativeToolRequest) -> Result<(), String> {
    match request {
        NativeToolRequest::Shell {
            command, workdir, ..
        } => {
            bounded("native shell command", command, NATIVE_CALL_OUTPUT_BYTES)?;
            if let Some(workdir) = workdir {
                bounded("native shell workdir", workdir, NATIVE_CALL_OUTPUT_BYTES)?;
            }
        }
        NativeToolRequest::ApplyPatch { patch } => {
            bounded("native apply_patch input", patch, NATIVE_CALL_OUTPUT_BYTES)?;
        }
    }
    Ok(())
}

pub(crate) fn validate_tool_outcome(outcome: &NativeToolOutcome) -> Result<(), String> {
    match outcome {
        NativeToolOutcome::Success { output } => {
            if output.len() > NATIVE_CALL_OUTPUT_BYTES {
                return Err(format!(
                    "native tool output exceeds {NATIVE_CALL_OUTPUT_BYTES} bytes"
                ));
            }
        }
        NativeToolOutcome::Retry { reason } => {
            bounded("native tool retry", reason, NATIVE_CALL_OUTPUT_BYTES)?;
        }
        NativeToolOutcome::Failure { message } => {
            bounded("native tool failure", message, NATIVE_CALL_OUTPUT_BYTES)?;
        }
    }
    Ok(())
}

pub(crate) fn validate_execution(result: &NativeExecution) -> Result<(), String> {
    match result {
        NativeExecution::Completed {
            identity,
            source_hash,
            evidence,
        } => {
            validate_identity(identity)?;
            validate_source_hash(source_hash)?;
            let bytes = evidence
                .exact_json_wire_len()
                .map_err(|error| format!("failed to measure native Evidence: {error}"))?;
            if bytes > NATIVE_EVIDENCE_BYTES {
                return Err(format!(
                    "native Evidence exceeds {NATIVE_EVIDENCE_BYTES} wire bytes"
                ));
            }
        }
        NativeExecution::Failed { identity, failure } => {
            validate_identity(identity)?;
            bounded("native failure kind", &failure.kind, 256)?;
            validate_source_hash(&failure.source_hash)?;
            bounded(
                "native failure diagnostic",
                &failure.diagnostic,
                NATIVE_DIAGNOSTIC_BYTES,
            )?;
        }
    }
    Ok(())
}

fn bounded(label: &str, value: &str, limit: usize) -> Result<(), String> {
    if value.len() > limit {
        return Err(format!("{label} exceeds {limit} bytes"));
    }
    Ok(())
}

fn bounded_identifier(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 256 {
        return Err(format!("{label} must contain 1..=256 bytes"));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!(
            "{label} may contain only ASCII letters, digits, '-' and '_'"
        ));
    }
    Ok(())
}

fn validate_source_hash(value: &str) -> Result<(), String> {
    if value.is_empty() || (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Ok(());
    }
    Err("native source hash must be empty or 64 hexadecimal bytes".to_string())
}

fn canonical_uuid(label: &str, value: &str) -> Result<(), String> {
    if value.len() != 36 {
        return Err(format!("{label} must be a lowercase canonical UUID"));
    }
    for (index, byte) in value.bytes().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) {
            if byte != b'-' {
                return Err(format!("{label} must be a lowercase canonical UUID"));
            }
        } else if !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase() {
            return Err(format!("{label} must be a lowercase canonical UUID"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_bounds_native_requests_outcomes_and_escaped_evidence() {
        assert!(
            validate_tool_request(&NativeToolRequest::ApplyPatch {
                patch: "x".repeat(NATIVE_CALL_OUTPUT_BYTES + 1),
            })
            .is_err()
        );
        assert!(
            validate_tool_outcome(&NativeToolOutcome::Success {
                output: vec![0; NATIVE_CALL_OUTPUT_BYTES + 1],
            })
            .is_err()
        );
        let result = NativeExecution::Completed {
            identity: NativeRunIdentity {
                session_id: "native-session".to_string(),
                thread_id: "00000000-0000-4000-8000-000000000001".to_string(),
                run_id: "00000000-0000-4000-8000-000000000002".to_string(),
            },
            source_hash: "a".repeat(64),
            evidence: NativeEvidence {
                version: 1,
                summary: "\u{0001}".repeat(NATIVE_EVIDENCE_BYTES / 2),
                verified: Vec::new(),
                disputed: Vec::new(),
                unresolved: Vec::new(),
                artifact_refs: Vec::new(),
                partial_failures: Vec::new(),
                provenance_ids: Vec::new(),
            },
        };
        assert!(validate_execution(&result).is_err());
    }

    #[test]
    fn client_rejects_noncanonical_run_identity_before_transport() {
        let request = NativeExecute {
            identity: NativeRunIdentity {
                session_id: "native-session".to_string(),
                thread_id: "not-a-uuid".to_string(),
                run_id: "00000000-0000-4000-8000-000000000002".to_string(),
            },
            attempt: 1,
            task: "task".to_string(),
            source: "use ycode_native_sdk as sdk; fn main() {}".to_string(),
        };
        assert!(validate_execute(&request).is_err());
    }
}
