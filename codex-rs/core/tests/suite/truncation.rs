#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used)]

use anyhow::Context;
use anyhow::Result;
use codex_protocol::models::PermissionProfile;
use core_test_support::assert_regex_match;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use serde_json::Value;
use serde_json::json;

fn assert_wall_time_header(output: &str) {
    let (wall_time, marker) = output
        .split_once('\n')
        .expect("wall-time header should contain an Output marker");
    assert_regex_match(r"^Wall time: [0-9]+(?:\.[0-9]+)? seconds$", wall_time);
    assert_eq!(marker, "Output:");
}

// Verifies that a standard tool call (shell_command) exceeding the model formatting
// limits is truncated before being sent back to the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_call_output_configured_limit_chars_type() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    // Use a model that exposes the shell_command tool.
    let mut builder = test_codex().with_model("gpt-5.2").with_config(|config| {
        config.tool_output_token_limit = Some(100_000);
    });

    let fixture = builder.build(&server).await?;

    let call_id = "shell-too-large";
    let command = if cfg!(windows) {
        "for ($i=1; $i -le 100000; $i++) { Write-Output $i }"
    } else {
        "seq 1 100000"
    };
    let args = serde_json::json!({
        "command": command,
        "timeout_ms": 5_000,
    });

    // First response: model tells us to run the tool; second: complete the turn.
    mount_sse_once(
        &server,
        sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let mock2 = mount_sse_once(
        &server,
        sse(vec![
            responses::ev_assistant_message("msg-1", "done"),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    fixture
        .submit_turn_with_permission_profile(
            "trigger big shell output",
            PermissionProfile::Disabled,
        )
        .await?;

    // Inspect what we sent back to the model; it should contain a truncated
    // function_call_output for the shell call.
    let output = mock2
        .single_request()
        .function_call_output_text(call_id)
        .context("function_call_output present for shell call")?;
    let output = output.replace("\r\n", "\n");

    // Expect plain text (not JSON) containing the entire shell output.
    assert!(
        serde_json::from_str::<Value>(&output).is_err(),
        "expected truncated shell output to be plain text"
    );

    assert!(
        (400000..=401000).contains(&output.len()),
        "we should be almost 100k tokens"
    );

    assert!(
        !output.contains("tokens truncated"),
        "shell output should not contain tokens truncated marker: {output}"
    );

    Ok(())
}

// Verifies that a standard tool call (shell_command) exceeding the model formatting
// limits is truncated before being sent back to the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_call_output_exceeds_limit_truncated_chars_limit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    // Use a model that exposes the shell_command tool.
    let mut builder = test_codex().with_model("gpt-5.2");

    let fixture = builder.build(&server).await?;

    let call_id = "shell-too-large";
    let command = if cfg!(windows) {
        "for ($i=1; $i -le 100000; $i++) { Write-Output $i }"
    } else {
        "seq 1 100000"
    };
    let args = serde_json::json!({
        "command": command,
        "timeout_ms": 5_000,
    });

    // First response: model tells us to run the tool; second: complete the turn.
    mount_sse_once(
        &server,
        sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let mock2 = mount_sse_once(
        &server,
        sse(vec![
            responses::ev_assistant_message("msg-1", "done"),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    fixture
        .submit_turn_with_permission_profile(
            "trigger big shell output",
            PermissionProfile::Disabled,
        )
        .await?;

    // Inspect what we sent back to the model; it should contain a truncated
    // function_call_output for the shell call.
    let output = mock2
        .single_request()
        .function_call_output_text(call_id)
        .context("function_call_output present for shell call")?;
    let output = output.replace("\r\n", "\n");

    // Expect plain text (not JSON) containing the entire shell output.
    assert!(
        serde_json::from_str::<Value>(&output).is_err(),
        "expected truncated shell output to be plain text"
    );

    let truncated_pattern = r#"(?s)^Exit code: 0\nWall time: [0-9]+(?:\.[0-9]+)? seconds\nTotal output lines: 100000\nOutput:\n.*?…\d+ chars truncated….*$"#;

    assert_regex_match(truncated_pattern, &output);

    let len = output.len();
    assert!(
        (9_900..=10_100).contains(&len),
        "expected ~10k chars after truncation, got {len}"
    );

    Ok(())
}

// Verifies that a standard tool call (shell_command) exceeding the model formatting
// limits is truncated before being sent back to the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_call_output_exceeds_limit_truncated_for_model() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    // Use a model that exposes the shell_command tool.
    let mut builder = test_codex().with_model("gpt-5.4");
    let fixture = builder.build(&server).await?;

    let call_id = "shell-too-large";
    let command = if cfg!(windows) {
        "for ($i=1; $i -le 100000; $i++) { Write-Output $i }"
    } else {
        "seq 1 100000"
    };
    let args = serde_json::json!({
        "command": command,
        "timeout_ms": 5_000,
    });

    // First response: model tells us to run the tool; second: complete the turn.
    mount_sse_once(
        &server,
        sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let mock2 = mount_sse_once(
        &server,
        sse(vec![
            responses::ev_assistant_message("msg-1", "done"),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    fixture
        .submit_turn_with_permission_profile(
            "trigger big shell output",
            PermissionProfile::Disabled,
        )
        .await?;

    // Inspect what we sent back to the model; it should contain a truncated
    // function_call_output for the shell call.
    let output = mock2
        .single_request()
        .function_call_output_text(call_id)
        .context("function_call_output present for shell call")?;
    let output = output.replace("\r\n", "\n");

    // Expect plain text (not JSON) containing the entire shell output.
    assert!(
        serde_json::from_str::<Value>(&output).is_err(),
        "expected truncated shell output to be plain text"
    );
    let truncated_pattern = r#"(?s)^Exit code: 0
Wall time: [0-9]+(?:\.[0-9]+)? seconds
Total output lines: 100000
Output:
1
2
3
4
5
6
.*…137224 tokens truncated.*
99999
100000
$"#;
    assert_regex_match(truncated_pattern, &output);

    Ok(())
}

// Ensures shell_command outputs that exceed the line limit are truncated only once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_call_output_truncated_only_once() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let mut builder = test_codex().with_model("gpt-5.4");
    let fixture = builder.build(&server).await?;
    let call_id = "shell-single-truncation";
    let command = if cfg!(windows) {
        "for ($i=1; $i -le 10000; $i++) { Write-Output $i }"
    } else {
        "seq 1 10000"
    };
    let args = serde_json::json!({
        "command": command,
        "timeout_ms": 5_000,
    });

    mount_sse_once(
        &server,
        sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let mock2 = mount_sse_once(
        &server,
        sse(vec![
            responses::ev_assistant_message("msg-1", "done"),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    fixture
        .submit_turn_with_permission_profile(
            "trigger big shell output",
            PermissionProfile::Disabled,
        )
        .await?;

    let output = mock2
        .single_request()
        .function_call_output_text(call_id)
        .context("function_call_output present for shell call")?;

    let truncation_markers = output.matches("tokens truncated").count();

    assert_eq!(
        truncation_markers, 1,
        "shell output should carry only one truncation marker: {output}"
    );

    Ok(())
}

// Token-based policy should report token counts even when truncation is byte-estimated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_policy_marker_reports_tokens() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.4").with_config(|config| {
        config.tool_output_token_limit = Some(50); // small budget to force truncation
    });
    let fixture = builder.build(&server).await?;

    let call_id = "shell-token-marker";
    let args = json!({
        "command": "seq 1 150",
        "timeout_ms": 5_000,
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let done_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    fixture
        .submit_turn_with_permission_profile("run the shell tool", PermissionProfile::Disabled)
        .await?;

    let output = done_mock
        .single_request()
        .function_call_output_text(call_id)
        .context("shell output present")?;

    let pattern = r"(?s)^Exit code: 0\nWall time: [0-9]+(?:\.[0-9]+)? seconds\nTotal output lines: 150\nOutput:\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19.*tokens truncated.*129\n130\n131\n132\n133\n134\n135\n136\n137\n138\n139\n140\n141\n142\n143\n144\n145\n146\n147\n148\n149\n150\n$";

    assert_regex_match(pattern, &output);

    Ok(())
}

// Byte-based policy should report bytes removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn byte_policy_marker_reports_bytes() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.2").with_config(|config| {
        config.tool_output_token_limit = Some(50); // ~200 byte cap
    });
    let fixture = builder.build(&server).await?;

    let call_id = "shell-byte-marker";
    let args = json!({
        "command": "seq 1 150",
        "timeout_ms": 5_000,
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let done_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    fixture
        .submit_turn_with_permission_profile("run the shell tool", PermissionProfile::Disabled)
        .await?;

    let output = done_mock
        .single_request()
        .function_call_output_text(call_id)
        .context("shell output present")?;

    let pattern = r"(?s)^Exit code: 0\nWall time: [0-9]+(?:\.[0-9]+)? seconds\nTotal output lines: 150\nOutput:\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19.*chars truncated.*129\n130\n131\n132\n133\n134\n135\n136\n137\n138\n139\n140\n141\n142\n143\n144\n145\n146\n147\n148\n149\n150\n$";

    assert_regex_match(pattern, &output);

    Ok(())
}

// shell_command output should remain intact when the config opts into a large token budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_command_output_not_truncated_with_custom_limit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.4").with_config(|config| {
        config.tool_output_token_limit = Some(50_000); // ample budget
    });
    let fixture = builder.build(&server).await?;

    let call_id = "shell-no-trunc";
    let args = json!({
        "command": "seq 1 1000",
        "timeout_ms": 5_000,
    });
    let expected_body: String = (1..=1000).map(|i| format!("{i}\n")).collect();

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let done_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    fixture
        .submit_turn_with_permission_profile(
            "run big output without truncation",
            PermissionProfile::Disabled,
        )
        .await?;

    let output = done_mock
        .single_request()
        .function_call_output_text(call_id)
        .context("shell output present")?;

    assert!(
        output.ends_with(&expected_body),
        "expected entire shell output when budget increased: {output}"
    );
    assert!(
        !output.contains("truncated"),
        "output should remain untruncated with ample budget"
    );

    Ok(())
}
