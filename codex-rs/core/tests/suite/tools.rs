#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used)]

use std::fs;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use codex_core::StartThreadOptions;
use codex_features::Feature;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::assert_regex_match;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_custom_tool_call_with_namespace;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::strip_response_item_ids_from_json;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::local;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use regex_lite::Regex;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;
use wiremock::ResponseTemplate;

fn tool_names(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.get("name")
                        .or_else(|| tool.get("type"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test_case(false, false; "normal sampling")]
#[test_case(true, false; "pre sampling compaction")]
#[test_case(false, true; "namespace collision")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_tool_collisions_fail_the_turn_before_sampling(
    pre_compact: bool,
    namespace_collision: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(move |config| {
        config.tool_registry.error_on_tool_collisions = true;
        if pre_compact {
            config.model_auto_compact_token_limit = Some(0);
        }
    });
    let test = builder.build_with_auto_env(&server).await?;
    let dynamic_tools = if namespace_collision {
        [
            ("first", "First namespace description."),
            ("second", "Second namespace description."),
        ]
        .into_iter()
        .map(|(name, description)| {
            DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
                name: "shared".to_string(),
                description: description.to_string(),
                tools: vec![DynamicToolNamespaceTool::Function(
                    DynamicToolFunctionSpec {
                        name: name.to_string(),
                        description: format!("The {name} tool."),
                        input_schema: json!({
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false,
                        }),
                        defer_loading: false,
                    },
                )],
            })
        })
        .collect()
    } else {
        vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
            name: "update_plan".to_string(),
            description: "Collides with the built-in planning tool.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
            defer_loading: false,
        })]
    };
    let thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            dynamic_tools,
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?
        .thread;

    thread
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "use the planning tool".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let EventMsg::Error(error) =
        wait_for_event(&thread, |event| matches!(event, EventMsg::Error(_))).await
    else {
        unreachable!("event predicate guarantees an error");
    };
    let expected_collision = if namespace_collision {
        "duplicate tool: shared"
    } else {
        "duplicate tool: functions.update_plan"
    };
    assert_eq!(error.message, expected_collision);

    let EventMsg::TurnComplete(completed) =
        wait_for_event(&thread, |event| matches!(event, EventMsg::TurnComplete(_))).await
    else {
        unreachable!("event predicate guarantees turn completion");
    };
    assert_eq!(completed.error, Some(error));
    assert!(
        server
            .received_requests()
            .await
            .context("mock server should expose received requests")?
            .iter()
            .all(|request| request.url.path() != "/v1/responses"),
        "a colliding turn should fail before making a model request"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_tool_collisions_do_not_duplicate_unrelated_compaction_errors() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let error = json!({
        "error": {
            "message": "compaction request is invalid",
            "code": "invalid_request",
        },
    });
    let compact_mock =
        mount_response_once(&server, ResponseTemplate::new(400).set_body_json(&error)).await;
    let mut builder = test_codex().with_config(|config| {
        config.tool_registry.error_on_tool_collisions = true;
        config.model_auto_compact_token_limit = Some(0);
    });
    let test = builder.build_with_auto_env(&server).await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "trigger compaction".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let mut errors = Vec::new();
    wait_for_event(&test.codex, |event| match event {
        EventMsg::Error(error) => {
            errors.push(error.message.clone());
            false
        }
        EventMsg::TurnComplete(_) => true,
        _ => false,
    })
    .await;

    assert_eq!(
        errors,
        vec![format!("Error running remote compact task: {error}")]
    );
    assert_eq!(compact_mock.requests().len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_turn_environments_omits_environment_backed_tools() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("unified exec should enable for test");
    });
    let test = builder.build(&server).await?;

    test.submit_turn_with_environments("which tools are available?", Some(vec![]))
        .await?;

    let tools = tool_names(&response_mock.single_request().body_json());
    assert!(
        tools.contains(&"update_plan".to_string()),
        "non-environment tool should remain available; got {tools:?}"
    );
    for environment_tool in ["exec_command", "write_stdin", "apply_patch", "view_image"] {
        assert!(
            !tools.contains(&environment_tool.to_string()),
            "{environment_tool} should be omitted for explicit empty turn environments; got {tools:?}"
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_environment_selection_keeps_environment_backed_tools() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("unified exec should enable for test");
    });
    let test = builder.build(&server).await?;

    test.submit_turn_with_environments(
        "which tools are available?",
        Some(vec![local(test.config.cwd.clone())]),
    )
    .await?;

    let tools = tool_names(&response_mock.single_request().body_json());
    assert!(
        tools.contains(&"exec_command".to_string()),
        "environment tool should remain available with selected local environment; got {tools:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_tool_unknown_returns_custom_output_error() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build(&server).await?;

    let call_id = "custom-unsupported";
    let tool_name = "unsupported_tool";

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_custom_tool_call(call_id, tool_name, "\"payload\""),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_text_turn("invoke custom tool").await?;

    let item = mock.single_request().custom_tool_call_output(call_id);
    let output = item
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let expected = format!("unsupported custom tool call: {tool_name}");
    assert_eq!(output, expected);
    assert!(
        item.pointer("/internal_chat_message_metadata_passthrough/executed_tool_calls")
            .is_none(),
        "attempted-tool metadata must be disabled by default",
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespaced_custom_tool_call_preserves_namespace_through_dispatch_and_replay() -> Result<()>
{
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    builder = builder.with_config(|config| {
        let _ = config.features.enable(Feature::ExecutedToolCallMetadata);
    });
    let test = builder.build(&server).await?;

    let call_id = "custom-namespaced";
    let namespace = "test_namespace::";
    let tool_name = "unsupported_tool";
    let input = "\"payload\"";

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_custom_tool_call_with_namespace(call_id, namespace, tool_name, input),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_text_turn("invoke namespaced custom tool")
        .await?;

    let request = mock.single_request();
    let custom_tool_calls = request.inputs_of_type("custom_tool_call");
    let turn_id = custom_tool_calls
        .first()
        .and_then(|item| item.pointer("/internal_chat_message_metadata_passthrough/turn_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .expect("custom tool call should include turn metadata");
    assert_eq!(
        (
            strip_response_item_ids_from_json(Value::Array(custom_tool_calls)),
            strip_response_item_ids_from_json(request.custom_tool_call_output(call_id)),
        ),
        (
            Value::Array(vec![json!({
                "type": "custom_tool_call",
                "call_id": call_id,
                "namespace": namespace,
                "name": tool_name,
                "input": input,
                "internal_chat_message_metadata_passthrough": {
                    "turn_id": turn_id,
                },
            })]),
            json!({
                "type": "custom_tool_call_output",
                "call_id": call_id,
                "output": format!("unsupported custom tool call: {namespace}{tool_name}"),
                "internal_chat_message_metadata_passthrough": {
                    "turn_id": turn_id,
                    "executed_tool_calls": [{
                        "name": format!("{namespace}__{tool_name}"),
                        "arguments": input,
                    }],
                },
            }),
        )
    );
    let escaped_call_id = "custom-namespaced-escaped";
    let escaped_input = "\\".repeat(4_096);
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-3"),
            ev_custom_tool_call_with_namespace(
                escaped_call_id,
                namespace,
                tool_name,
                &escaped_input,
            ),
            ev_completed("resp-3"),
        ]),
    )
    .await;
    let escaped_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-2", "done"),
            ev_completed("resp-4"),
        ]),
    )
    .await;
    test.submit_text_turn("invoke namespaced custom tool with escaped arguments")
        .await?;
    let escaped_request = escaped_mock.single_request();
    assert_eq!(
        escaped_request.custom_tool_call_output(call_id)["internal_chat_message_metadata_passthrough"]
            ["executed_tool_calls"],
        json!([{
            "name": format!("{namespace}__{tool_name}"),
            "arguments": input,
        }]),
    );
    let expected_escaped_calls = json!([{
        "name": format!("{namespace}__{tool_name}"),
        "arguments": {
            "_codex_executed_tool_call_truncated": {
                "original_bytes": serde_json::to_vec(&escaped_input)?.len(),
                "max_bytes": 8 * 1024,
            },
        },
    }]);
    assert_eq!(
        escaped_request.custom_tool_call_output(escaped_call_id)["internal_chat_message_metadata_passthrough"]
            ["executed_tool_calls"],
        expected_escaped_calls,
    );

    let direct_exec_call_id = "custom-direct-exec";
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-5"),
            ev_custom_tool_call(
                direct_exec_call_id,
                codex_code_mode::PUBLIC_TOOL_NAME,
                input,
            ),
            ev_completed("resp-5"),
        ]),
    )
    .await;
    let direct_exec_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-3", "done"),
            ev_completed("resp-6"),
        ]),
    )
    .await;

    test.submit_text_turn("invoke direct custom exec outside code mode")
        .await?;

    let direct_exec_request = direct_exec_mock.single_request();
    assert_eq!(
        direct_exec_request.custom_tool_call_output(call_id)["internal_chat_message_metadata_passthrough"]
            ["executed_tool_calls"],
        json!([{
            "name": format!("{namespace}__{tool_name}"),
            "arguments": input,
        }]),
    );
    assert_eq!(
        direct_exec_request.custom_tool_call_output(escaped_call_id)["internal_chat_message_metadata_passthrough"]
            ["executed_tool_calls"],
        expected_escaped_calls,
    );
    let direct_exec_output = direct_exec_request.custom_tool_call_output(direct_exec_call_id);
    assert_eq!(
        direct_exec_output["output"],
        json!("unsupported custom tool call: exec"),
    );
    assert_eq!(
        direct_exec_output["internal_chat_message_metadata_passthrough"]["executed_tool_calls"],
        json!([{
            "name": codex_code_mode::PUBLIC_TOOL_NAME,
            "arguments": input,
        }]),
    );

    Ok(())
}

async fn collect_tools(use_unified_exec: bool) -> Result<Vec<String>> {
    let server = start_mock_server().await;

    let responses = vec![sse(vec![
        ev_response_created("resp-1"),
        ev_assistant_message("msg-1", "done"),
        ev_completed("resp-1"),
    ])];
    let mock = mount_sse_sequence(&server, responses).await;

    let mut builder = test_codex().with_config(move |config| {
        if use_unified_exec {
            config
                .features
                .enable(Feature::UnifiedExec)
                .expect("test config should allow feature update");
        } else {
            config
                .features
                .disable(Feature::UnifiedExec)
                .expect("test config should allow feature update");
        }
    });
    let test = builder.build(&server).await?;

    test.submit_text_turn("list tools").await?;

    let first_body = mock.single_request().body_json();
    Ok(tool_names(&first_body))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_spec_toggle_end_to_end() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let tools_disabled = collect_tools(/*use_unified_exec*/ false).await?;
    assert!(
        !tools_disabled.iter().any(|name| name == "exec_command"),
        "tools list should not include exec_command when disabled: {tools_disabled:?}"
    );
    assert!(
        !tools_disabled.iter().any(|name| name == "write_stdin"),
        "tools list should not include write_stdin when disabled: {tools_disabled:?}"
    );

    let tools_enabled = collect_tools(/*use_unified_exec*/ true).await?;
    assert!(
        tools_enabled.iter().any(|name| name == "exec_command"),
        "tools list should include exec_command when enabled: {tools_enabled:?}"
    );
    assert!(
        tools_enabled.iter().any(|name| name == "write_stdin"),
        "tools list should include write_stdin when enabled: {tools_enabled:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_command_timeout_includes_timeout_prefix_and_metadata() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("test-gpt-5-codex");
    let test = builder.build(&server).await?;

    let call_id = "shell-command-timeout";
    let timeout_ms = 50u64;
    let args = json!({
        "command": "yes line | head -n 400; sleep 1",
        "login": false,
        "timeout_ms": timeout_ms,
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
    let second_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_text_turn("run a long command").await?;

    let timeout_item = second_mock.single_request().function_call_output(call_id);

    let output_str = timeout_item
        .get("output")
        .and_then(Value::as_str)
        .expect("timeout output string");

    // The exec path can report a timeout in two ways depending on timing:
    // 1) Structured JSON with exit_code 124 and a timeout prefix (preferred), or
    // 2) A plain error string if the child is observed as killed by a signal first.
    if let Ok(output_json) = serde_json::from_str::<Value>(output_str) {
        assert_eq!(
            output_json["metadata"]["exit_code"].as_i64(),
            Some(124),
            "expected timeout exit code 124",
        );

        let stdout = output_json["output"].as_str().unwrap_or_default();
        assert!(
            stdout.contains("command timed out"),
            "timeout output missing `command timed out`: {stdout}"
        );
    } else {
        let normalized_output = output_str
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .trim_end_matches('\n')
            .to_string();

        let shell_output_pattern = r"(?s)^Exit code: 124\nWall time: [0-9]+(?:\.[0-9]+)? seconds\nOutput:\ncommand timed out after [0-9]+ milliseconds\n(?:.*)?$";
        if Regex::new(shell_output_pattern)
            .expect("shell timeout output regex should compile")
            .is_match(&normalized_output)
        {
            return Ok(());
        }

        // Fallback: accept the signal classification path to deflake the test.
        let signal_pattern = r"(?is)^execution error:.*signal.*$";
        assert_regex_match(signal_pattern, output_str);
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_command_timeout_handles_background_grandchild_stdout() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.4");
    let test = builder.build(&server).await?;

    let call_id = "shell-command-grandchild-timeout";
    let pid_path = test.cwd.path().join("grandchild_pid.txt");
    let script_path = test.cwd.path().join("spawn_detached.py");
    let script = format!(
        r#"import subprocess
import time
from pathlib import Path

# Spawn a detached grandchild that inherits stdout/stderr so the pipe stays open.
proc = subprocess.Popen(["/bin/sh", "-c", "sleep 60"], start_new_session=True)
Path({pid_path:?}).write_text(str(proc.pid))
time.sleep(60)
"#
    );
    fs::write(&script_path, script)?;

    let args = json!({
        "command": format!("python3 {:?}", script_path.to_string_lossy()),
        "login": false,
        "timeout_ms": 200,
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
    let second_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    let start = Instant::now();
    let output_str = tokio::time::timeout(Duration::from_secs(10), async {
        test.submit_text_turn("run a command with a detached grandchild")
            .await?;
        let timeout_item = second_mock.single_request().function_call_output(call_id);
        timeout_item
            .get("output")
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("timeout output string")
    })
    .await
    .context("exec call should not hang waiting for grandchild pipes to close")??;
    let elapsed = start.elapsed();

    if let Ok(output_json) = serde_json::from_str::<Value>(&output_str) {
        assert_eq!(
            output_json["metadata"]["exit_code"].as_i64(),
            Some(124),
            "expected timeout exit code 124",
        );
    } else {
        let timeout_pattern = r"(?is)command timed out|timeout";
        assert_regex_match(timeout_pattern, &output_str);
    }

    assert!(
        elapsed < Duration::from_secs(9),
        "command should return shortly after timeout even with live grandchildren: {elapsed:?}"
    );

    if let Ok(pid_str) = fs::read_to_string(&pid_path)
        && let Ok(pid) = pid_str.trim().parse::<libc::pid_t>()
    {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    Ok(())
}
