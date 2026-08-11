use super::*;
use codex_network_proxy::BlockedRequest;
use codex_network_proxy::BlockedRequestArgs;
use codex_network_proxy::NetworkMode;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use pretty_assertions::assert_eq;
use std::time::Duration;

fn make_exec_output(
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    aggregated: &str,
) -> ExecToolCallOutput {
    ExecToolCallOutput {
        exit_code,
        stdout: StreamOutput::new(stdout.to_string()),
        stderr: StreamOutput::new(stderr.to_string()),
        aggregated_output: StreamOutput::new(aggregated.to_string()),
        duration: Duration::from_millis(1),
        timed_out: false,
    }
}

#[test]
fn classifies_filesystem_violation_with_path() {
    let output = make_exec_output(
        /*exit_code*/ 1,
        "",
        "bash: /private/tmp/denied: Operation not permitted",
        "",
    );

    assert_eq!(
        classify_filesystem_sandbox_violation(SandboxType::MacosSeatbelt, &output),
        Some(FileSystemSandboxViolation {
            backend: SandboxViolationBackend::Seatbelt,
            reason: FileSystemSandboxViolationReason::OperationNotPermitted,
            path: Some("/private/tmp/denied".to_string()),
            output_snippet: "bash: /private/tmp/denied: Operation not permitted".to_string(),
        })
    );
}

#[test]
fn classifies_filesystem_violation_with_unicode_before_marker() {
    let output = make_exec_output(
        /*exit_code*/ 1,
        "",
        "bash: /private/tmp/\u{130}-denied: Operation not permitted",
        "",
    );

    assert_eq!(
        classify_filesystem_sandbox_violation(SandboxType::MacosSeatbelt, &output),
        Some(FileSystemSandboxViolation {
            backend: SandboxViolationBackend::Seatbelt,
            reason: FileSystemSandboxViolationReason::OperationNotPermitted,
            path: Some("/private/tmp/\u{130}-denied".to_string()),
            output_snippet: "bash: /private/tmp/\u{130}-denied: Operation not permitted"
                .to_string(),
        })
    );
}

#[test]
fn classifies_filesystem_violation_from_aggregated_output() {
    let output = make_exec_output(
        /*exit_code*/ 101,
        "",
        "",
        "cargo failed: Read-only file system when writing target",
    );

    assert_eq!(
        classify_filesystem_sandbox_violation(SandboxType::MacosSeatbelt, &output),
        Some(FileSystemSandboxViolation {
            backend: SandboxViolationBackend::Seatbelt,
            reason: FileSystemSandboxViolationReason::ReadOnlyFileSystem,
            path: None,
            output_snippet: "cargo failed: Read-only file system when writing target".to_string(),
        })
    );
}

#[test]
fn keeps_output_snippet_on_the_stream_that_matched() {
    let output = make_exec_output(
        /*exit_code*/ 1,
        "bash: /private/tmp/denied: Permission denied",
        "unrelated warning",
        "unrelated warning\nbash: /private/tmp/denied: Permission denied",
    );

    assert_eq!(
        classify_filesystem_sandbox_violation(SandboxType::MacosSeatbelt, &output),
        Some(FileSystemSandboxViolation {
            backend: SandboxViolationBackend::Seatbelt,
            reason: FileSystemSandboxViolationReason::PermissionDenied,
            path: Some("/private/tmp/denied".to_string()),
            output_snippet: "bash: /private/tmp/denied: Permission denied".to_string(),
        })
    );
}

#[test]
fn does_not_classify_non_sandbox_mode() {
    let output = make_exec_output(/*exit_code*/ 1, "", "Operation not permitted", "");

    assert!(classify_filesystem_sandbox_violation(SandboxType::None, &output).is_none());
}

#[test]
fn converts_blocked_request_to_network_violation() {
    let blocked = BlockedRequest::new(BlockedRequestArgs {
        host: "example.com".to_string(),
        reason: "not_allowed".to_string(),
        client: Some("curl".to_string()),
        method: Some("CONNECT".to_string()),
        mode: Some(NetworkMode::Limited),
        protocol: "https".to_string(),
        decision: Some("block".to_string()),
        source: Some("policy".to_string()),
        port: Some(443),
    });

    assert_eq!(
        NetworkSandboxViolation::from_blocked_request(&blocked),
        NetworkSandboxViolation {
            backend: SandboxViolationBackend::ManagedNetworkProxy,
            host: "example.com".to_string(),
            reason: "not_allowed".to_string(),
            client: Some("curl".to_string()),
            method: Some("CONNECT".to_string()),
            mode: Some(NetworkMode::Limited),
            protocol: "https".to_string(),
            decision: Some("block".to_string()),
            source: Some("policy".to_string()),
            port: Some(443),
            timestamp: blocked.timestamp,
        }
    );
}
