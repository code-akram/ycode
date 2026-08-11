#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used)]

use anyhow::Result;
use codex_core::StartThreadOptions;
use codex_core::config::Config;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolContributor;
use codex_models_manager::bundled_models_response;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::FunctionCallError;
use codex_tools::JsonToolOutput;
use codex_tools::ToolCall;
use codex_tools::ToolExecutor;
use codex_tools::ToolExecutorFuture;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolPayload;
use codex_tools::ToolSpec;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::ev_tool_search_call;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::namespace_child_tool;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";

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

fn tool_search_output_item(request: &ResponsesRequest, call_id: &str) -> Value {
    request.tool_search_output(call_id)
}

fn tool_search_output_tools(request: &ResponsesRequest, call_id: &str) -> Vec<Value> {
    tool_search_output_item(request, call_id)
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn tool_search_output_has_namespace_child(
    request: &ResponsesRequest,
    call_id: &str,
    namespace: &str,
    tool_name: &str,
) -> bool {
    let output = json!({
        "tools": tool_search_output_tools(request, call_id),
    });
    namespace_child_tool(&output, namespace, tool_name).is_some()
}

fn configure_search_capable_model(config: &mut codex_core::config::Config) {
    let mut model_catalog = bundled_models_response().expect("bundled models.json should parse");
    let model = model_catalog
        .models
        .iter_mut()
        .find(|model| model.slug == "gpt-5.4")
        .expect("gpt-5.4 exists in bundled models.json");
    config.model = Some("gpt-5.4".to_string());
    model.supports_search_tool = true;
    config.model_catalog = Some(model_catalog);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_search_returns_deferred_v1_multi_agent_tools() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "tool-search-spawn-agent";
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_tool_search_call(
                    call_id,
                    &json!({
                        "query": "spawn agent",
                        "limit": 1,
                    }),
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    let mut builder = test_codex().with_config(configure_search_capable_model);
    let test = builder.build(&server).await?;
    test.submit_turn_with_approval_and_permission_profile(
        "Find the spawn agent tool",
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);

    let first_request_body = requests[0].body_json();
    let first_request_tools = tool_names(&first_request_body);
    assert!(
        first_request_tools
            .iter()
            .any(|name| name == TOOL_SEARCH_TOOL_NAME),
        "first request should advertise tool_search: {first_request_tools:?}"
    );
    for tool_name in [
        "spawn_agent",
        "send_input",
        "resume_agent",
        "wait_agent",
        "close_agent",
    ] {
        assert!(
            !first_request_tools.iter().any(|name| name == tool_name),
            "v1 multi-agent tools should be hidden before search: {first_request_tools:?}"
        );
    }
    assert!(
        !first_request_body
            .to_string()
            .contains("### When to delegate vs. do the subtask yourself"),
        "deferred v1 multi-agent guidance should stay out of initial developer context"
    );

    let tools = tool_search_output_tools(&requests[1], call_id);
    assert!(
        !tools.iter().any(|tool| {
            tool.get("type").and_then(Value::as_str) == Some("function")
                && tool.get("name").and_then(Value::as_str) == Some("spawn_agent")
        }),
        "spawn_agent should be returned as a namespace child, not a flat function: {tools:?}"
    );
    assert!(
        tools.iter().any(|tool| {
            tool.get("type").and_then(Value::as_str) == Some("namespace")
                && tool.get("name").and_then(Value::as_str) == Some("multi_agent_v1")
        }),
        "expected tool_search to return multi_agent_v1 namespace: {tools:?}"
    );
    let output = tool_search_output_item(&requests[1], call_id);
    let spawn_agent = namespace_child_tool(&output, "multi_agent_v1", "spawn_agent")
        .expect("tool_search should return multi_agent_v1.spawn_agent");
    assert_eq!(
        spawn_agent.get("defer_loading").and_then(Value::as_bool),
        Some(true)
    );
    let description = spawn_agent
        .get("description")
        .and_then(Value::as_str)
        .expect("spawn_agent description should be present");
    assert!(description.contains(
        "Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work."
    ));
    assert!(description.contains("### Designing delegated subtasks"));
    assert!(description.contains("### When to delegate vs. do the subtask yourself"));

    Ok(())
}

struct DeferredCustomTool;

impl ToolContributor for DeferredCustomTool {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn ToolExecutor<ToolCall>>> {
        vec![Arc::new(Self)]
    }
}

impl ToolExecutor<ToolCall> for DeferredCustomTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("custom_echo")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Freeform(FreeformTool {
            name: "custom_echo".to_string(),
            description: "Echo a custom payload.".to_string(),
            defer_loading: None,
            format: FreeformToolFormat {
                r#type: "grammar".to_string(),
                syntax: "lark".to_string(),
                definition: "start: /.+/".to_string(),
            },
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Deferred
    }

    fn handle(&self, call: ToolCall) -> ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Custom { input } = call.payload else {
                return Err(FunctionCallError::Fatal(
                    "expected custom tool payload".to_string(),
                ));
            };
            Ok(Box::new(JsonToolOutput::new(json!({
                "echo": input,
                "namespace": call.tool_name.namespace,
            }))) as Box<dyn ToolOutput>)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_search_returns_deferred_custom_tool_and_routes_follow_up_call() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_tool_search_call("search-1", &json!({ "query": "custom payload" })),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_custom_tool_call_with_namespace("custom-1", "functions", "custom_echo", "hello"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;

    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_contributor(Arc::new(DeferredCustomTool));
    let mut builder = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(configure_search_capable_model);
    let test = builder.build_with_auto_env(&server).await?;
    test.submit_turn("Find and run the custom echo tool")
        .await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);

    let initial_tools = tool_names(&requests[0].body_json());
    assert!(
        initial_tools
            .iter()
            .any(|name| name == TOOL_SEARCH_TOOL_NAME)
    );
    assert!(initial_tools.iter().all(|name| name != "custom_echo"));
    assert_eq!(
        tool_search_output_tools(&requests[1], "search-1"),
        vec![json!({
            "type": "namespace",
            "name": "functions",
            "description": "",
            "tools": [{
                "type": "custom",
                "name": "custom_echo",
                "description": "Echo a custom payload.",
                "defer_loading": true,
                "format": {
                    "type": "grammar",
                    "syntax": "lark",
                    "definition": "start: /.+/",
                },
            }],
        })]
    );
    let output = requests[2].custom_tool_call_output("custom-1");
    let output: Value = serde_json::from_str(
        output["output"]
            .as_str()
            .expect("custom tool output should contain serialized JSON"),
    )?;
    assert_eq!(output, json!({ "echo": "hello", "namespace": "functions" }));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_search_returns_deferred_dynamic_tool_and_routes_follow_up_call() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let search_call_id = "tool-search-1";
    let dynamic_call_id = "dyn-search-call-1";
    let tool_name = "automation_update";
    let tool_description = "Create, update, view, or delete recurring automations.";
    let tool_args = json!({ "mode": "create" });
    let tool_call_arguments = serde_json::to_string(&tool_args)?;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_tool_search_call(
                    search_call_id,
                    &json!({
                        "query": "recurring automations",
                        "limit": 8,
                    }),
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "function_call",
                        "call_id": dynamic_call_id,
                        "namespace": "codex_app",
                        "name": tool_name,
                        "arguments": tool_call_arguments,
                    }
                }),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;

    let input_schema = json!({
        "type": "object",
        "properties": {
            "mode": { "type": "string" },
        },
        "required": ["mode"],
        "additionalProperties": false,
    });
    let dynamic_tool = DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
        name: "codex_app".to_string(),
        description: "Automation tools.".to_string(),
        tools: vec![DynamicToolNamespaceTool::Function(
            DynamicToolFunctionSpec {
                name: tool_name.to_string(),
                description: tool_description.to_string(),
                input_schema: input_schema.clone(),
                defer_loading: true,
            },
        )],
    });
    let shadow_tool = DynamicToolSpec::Function(DynamicToolFunctionSpec {
        name: TOOL_SEARCH_TOOL_NAME.to_string(),
        description: "Client-provided tool that must not replace tool search.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }),
        defer_loading: false,
    });

    let mut builder = test_codex().with_config(configure_search_capable_model);
    let base_test = builder.build_with_auto_env(&server).await?;
    let new_thread = base_test
        .thread_manager
        .start_thread(StartThreadOptions {
            dynamic_tools: vec![dynamic_tool, shadow_tool],
            ..StartThreadOptions::new(base_test.config.clone())
        })
        .await?;
    let mut test = base_test;
    test.codex = new_thread.thread;
    test.session_configured = new_thread.session_configured;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Use the automation tool".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let EventMsg::DynamicToolCallRequest(request) = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::DynamicToolCallRequest(_))
    })
    .await
    else {
        unreachable!("event guard guarantees DynamicToolCallRequest");
    };
    assert_eq!(request.call_id, dynamic_call_id);
    assert_eq!(request.namespace.as_deref(), Some("codex_app"));
    assert_eq!(request.tool, tool_name);
    assert_eq!(request.arguments, tool_args);

    test.codex
        .submit(Op::DynamicToolResponse {
            id: request.call_id,
            response: DynamicToolResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputText {
                    text: "dynamic-search-ok".to_string(),
                }],
                success: true,
            },
        })
        .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);

    let first_request_body = requests[0].body_json();
    let first_request_tools = tool_names(&first_request_body);
    let advertised_search_tool_types = first_request_body
        .get("tools")
        .and_then(Value::as_array)
        .expect("first request should contain model tools")
        .iter()
        .filter(|tool| {
            tool.get("name")
                .or_else(|| tool.get("type"))
                .and_then(Value::as_str)
                == Some(TOOL_SEARCH_TOOL_NAME)
        })
        .map(|tool| tool.get("type").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        advertised_search_tool_types,
        vec![Some(TOOL_SEARCH_TOOL_NAME)],
        "first request should advertise exactly one host tool_search: {first_request_tools:?}"
    );
    assert!(
        !first_request_tools.iter().any(|name| name == tool_name),
        "deferred dynamic tool should be hidden before search: {first_request_tools:?}"
    );

    let tools = tool_search_output_tools(&requests[1], search_call_id);
    assert_eq!(
        tools,
        vec![json!({
            "type": "namespace",
            "name": "codex_app",
            "description": "Automation tools.",
            "tools": [{
                "type": "function",
                "name": tool_name,
                "description": tool_description,
                "strict": false,
                "defer_loading": true,
                "parameters": input_schema,
            }],
        })]
    );

    let second_request_body = requests[1].body_json();
    let second_request_tools = tool_names(&second_request_body);
    assert!(
        !second_request_tools.iter().any(|name| name == tool_name),
        "follow-up request should rely on tool_search_output history, not tool injection: {second_request_tools:?}"
    );

    let output = requests[2]
        .function_call_output(dynamic_call_id)
        .get("output")
        .cloned()
        .expect("dynamic tool output should be present");
    let payload: FunctionCallOutputPayload = serde_json::from_value(output)?;
    assert_eq!(
        payload,
        FunctionCallOutputPayload::from_text("dynamic-search-ok".to_string())
    );

    let third_request_body = requests[2].body_json();
    let third_request_tools = tool_names(&third_request_body);
    assert!(
        !third_request_tools.iter().any(|name| name == tool_name),
        "post-tool follow-up should rely on tool_search_output history, not tool injection: {third_request_tools:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_search_matches_dynamic_tools_by_name_description_namespace_and_schema_terms()
-> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let query_cases = [
        ("tool-search-dynamic-name", "quasar_ping_beacon"),
        ("tool-search-dynamic-spaces", "quasar ping beacon"),
        ("tool-search-dynamic-description", "saffron metronome"),
        ("tool-search-dynamic-namespace", "orbit_ops"),
        ("tool-search-dynamic-schema", "chrono_spec"),
    ];
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(std::iter::once(ev_response_created("resp-1"))
                .chain(query_cases.into_iter().map(|(call_id, query)| {
                    ev_tool_search_call(
                        call_id,
                        &json!({
                            "query": query,
                            "limit": 8,
                        }),
                    )
                }))
                .chain(std::iter::once(ev_completed("resp-1")))
                .collect()),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    let dynamic_tool = DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
        name: "orbit_ops".to_string(),
        description: "Orbital reminder operations.".to_string(),
        tools: vec![DynamicToolNamespaceTool::Function(
            DynamicToolFunctionSpec {
                name: "quasar_ping_beacon".to_string(),
                description: "Trigger the saffron metronome workflow for reminder follow-ups."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "chrono_spec": { "type": "string" },
                        "targetThreadId": { "type": "string" },
                    },
                    "required": ["chrono_spec"],
                    "additionalProperties": false,
                }),
                defer_loading: true,
            },
        )],
    });

    let mut builder = test_codex().with_config(configure_search_capable_model);
    let base_test = builder.build(&server).await?;
    let new_thread = base_test
        .thread_manager
        .start_thread(StartThreadOptions {
            dynamic_tools: vec![dynamic_tool],
            ..StartThreadOptions::new(base_test.config.clone())
        })
        .await?;
    let mut test = base_test;
    test.codex = new_thread.thread;
    test.session_configured = new_thread.session_configured;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Search for the dynamic tool".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);

    for call_id in [
        "tool-search-dynamic-name",
        "tool-search-dynamic-spaces",
        "tool-search-dynamic-description",
        "tool-search-dynamic-namespace",
        "tool-search-dynamic-schema",
    ] {
        assert!(
            tool_search_output_has_namespace_child(
                &requests[1],
                call_id,
                "orbit_ops",
                "quasar_ping_beacon"
            ),
            "expected query {call_id} to surface the quasar_ping_beacon tool: {:?}",
            tool_search_output_tools(&requests[1], call_id)
        );
    }

    Ok(())
}
