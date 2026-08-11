use std::collections::BTreeMap;
use std::sync::Arc;

use crate::config::Config;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::context::ToolPayload;
use crate::tools::spec_plan::append_source_tools;
use crate::tools::spec_plan::build_core_tool_registry;
use crate::tools::spec_plan::extension_tool_executors;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall as ExtensionToolCall;
use codex_extension_api::ToolExecutor;
use codex_protocol::DEFAULT_FUNCTION_NAMESPACE;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_tools::default_namespace_description;
use core_test_support::responses::strip_response_item_ids_from_json;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::ToolCall;
use super::ToolCallSource;
use super::ToolRouter;
use super::tool_log_payload;

struct ExtensionEchoContributor;

#[test]
fn tool_log_payload_redacts_plaintext_multi_agent_messages() {
    let payload = ToolPayload::Function {
        arguments: json!({"target": "/root/worker", "message": "secret message"}).to_string(),
    };
    assert_eq!(
        tool_log_payload(&payload, &ToolCallSource::DirectPlaintextMessage),
        "[plaintext arguments]"
    );
    assert_eq!(
        tool_log_payload(&payload, &ToolCallSource::Direct),
        payload.log_payload()
    );
}

impl codex_extension_api::ToolContributor for ExtensionEchoContributor {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn ToolExecutor<ExtensionToolCall>>> {
        vec![Arc::new(ExtensionEchoExecutor)]
    }
}

struct ExtensionEchoExecutor;

impl ToolExecutor<ExtensionToolCall> for ExtensionEchoExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced("extension/", "echo")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "extension/".to_string(),
            description: default_namespace_description("extension/"),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: "echo".to_string(),
                description: "Echoes arguments through an extension tool.".to_string(),
                strict: true,
                parameters: codex_extension_api::parse_tool_input_schema(&json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" },
                    },
                    "required": ["message"],
                    "additionalProperties": false,
                }))
                .expect("extension schema should parse"),
                output_schema: None,
                defer_loading: None,
            })],
        })
    }

    fn handle(&self, call: ExtensionToolCall) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(call))
    }
}

impl ExtensionEchoExecutor {
    async fn handle_call(
        &self,
        call: ExtensionToolCall,
    ) -> Result<Box<dyn codex_tools::ToolOutput>, codex_tools::FunctionCallError> {
        let arguments: serde_json::Value =
            serde_json::from_str(call.function_arguments()?).expect("test arguments should parse");
        Ok(Box::new(codex_tools::JsonToolOutput::new(json!({
            "arguments": arguments,
            "callId": call.call_id,
            "conversationHistory": call.conversation_history.items(),
            "ok": true,
        }))) as Box<dyn codex_tools::ToolOutput>)
    }
}

fn extension_tool_test_registry() -> Arc<ExtensionRegistry<Config>> {
    let mut builder = ExtensionRegistryBuilder::new();
    builder.tool_contributor(Arc::new(ExtensionEchoContributor));
    Arc::new(builder.build())
}

fn test_tool_router(
    step_context: &StepContext,
    extension_tool_executors: impl IntoIterator<Item = Arc<dyn ToolExecutor<ExtensionToolCall>>>,
    dynamic_tools: &[DynamicToolSpec],
) -> ToolRouter {
    let mut registry = build_core_tool_registry(
        step_context.turn.as_ref(),
        &step_context.environments,
        /*wait_for_environment_tool_config*/ None,
    );
    let hosted_specs = append_source_tools(
        step_context.turn.as_ref(),
        &mut registry,
        extension_tool_executors,
        dynamic_tools,
    );
    ToolRouter::from_registry(
        step_context.turn.as_ref(),
        registry,
        hosted_specs,
        &Default::default(),
    )
}

#[tokio::test]
async fn parallel_support_does_not_match_namespaced_local_tool_names() -> anyhow::Result<()> {
    let (_, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let router = test_tool_router(step_context.as_ref(), Vec::new(), &turn.dynamic_tools);

    let parallel_tool_name = ["exec_command", "shell_command"]
        .into_iter()
        .find(|name| {
            router.tool_supports_parallel(&ToolCall {
                tool_name: ToolName::plain(*name),
                call_id: "call-parallel-tool".to_string(),
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
                encrypted_function_args: None,
            })
        })
        .expect("test session should expose a parallel shell-like tool");

    assert_eq!(
        router
            .tool_runtime(&ToolCall {
                tool_name: ToolName::plain(parallel_tool_name),
                call_id: "call-local-tool".to_string(),
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
                encrypted_function_args: None,
            })
            .map(|runtime| runtime.tool_name()),
        Some(ToolName::plain(parallel_tool_name))
    );

    assert!(!router.tool_supports_parallel(&ToolCall {
        tool_name: ToolName::namespaced("extension", parallel_tool_name),
        call_id: "call-namespaced-tool".to_string(),
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
        encrypted_function_args: None,
    }));

    Ok(())
}

#[tokio::test]
async fn build_tool_call_uses_namespace_for_registry_name() -> anyhow::Result<()> {
    let tool_name = "create_event".to_string();

    let call = ToolRouter::build_tool_call(ResponseItem::FunctionCall {
        id: None,
        name: tool_name.clone(),
        namespace: Some("calendar".to_string()),
        arguments: "{}".to_string(),
        encrypted_function_args: Some(Vec::new()),
        call_id: "call-namespace".to_string(),
        internal_chat_message_metadata_passthrough: None,
    })?
    .expect("function_call should produce a tool call");

    assert_eq!(call.tool_name, ToolName::namespaced("calendar", tool_name));
    assert_eq!(call.call_id, "call-namespace");
    assert_eq!(call.encrypted_function_args, Some(Vec::new()));
    assert_eq!(call.direct_source(), ToolCallSource::Direct);
    match call.payload {
        ToolPayload::Function { arguments } => {
            assert_eq!(arguments, "{}");
        }
        other => panic!("expected function payload, got {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn build_custom_tool_call_uses_namespace_for_registry_name() -> anyhow::Result<()> {
    let tool_name = "exec".to_string();

    let call = ToolRouter::build_tool_call(ResponseItem::CustomToolCall {
        id: None,
        status: None,
        call_id: "call-namespace".to_string(),
        name: tool_name.clone(),
        namespace: Some("python".to_string()),
        input: "print('hello')".to_string(),
        internal_chat_message_metadata_passthrough: None,
    })?
    .expect("custom_tool_call should produce a tool call");

    assert_eq!(
        call,
        ToolCall {
            tool_name: ToolName::namespaced("python", tool_name),
            call_id: "call-namespace".to_string(),
            payload: ToolPayload::Custom {
                input: "print('hello')".to_string(),
            },
            encrypted_function_args: None,
        }
    );

    Ok(())
}

#[test]
fn build_tool_call_normalizes_default_function_and_custom_namespaces() -> anyhow::Result<()> {
    for namespace in [None, Some(""), Some(DEFAULT_FUNCTION_NAMESPACE)] {
        let function_call = ToolRouter::build_tool_call(ResponseItem::FunctionCall {
            id: None,
            name: "lookup".to_string(),
            namespace: namespace.map(str::to_string),
            arguments: "{}".to_string(),
            encrypted_function_args: None,
            call_id: "call-function".to_string(),
            internal_chat_message_metadata_passthrough: None,
        })?
        .expect("function_call should produce a tool call");
        let custom_call = ToolRouter::build_tool_call(ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "call-custom".to_string(),
            name: "apply_patch".to_string(),
            namespace: namespace.map(str::to_string),
            input: "patch".to_string(),
            internal_chat_message_metadata_passthrough: None,
        })?
        .expect("custom_tool_call should produce a tool call");

        assert_eq!(
            [function_call.tool_name, custom_call.tool_name],
            [
                ToolName::namespaced(DEFAULT_FUNCTION_NAMESPACE, "lookup"),
                ToolName::namespaced(DEFAULT_FUNCTION_NAMESPACE, "apply_patch"),
            ]
        );
    }

    Ok(())
}

#[tokio::test]
async fn tools_without_handlers_do_not_support_parallel() -> anyhow::Result<()> {
    let (_, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let router = test_tool_router(step_context.as_ref(), Vec::new(), &turn.dynamic_tools);

    assert!(!router.tool_supports_parallel(&ToolCall {
        tool_name: ToolName::plain("web_search"),
        call_id: "call-web-search".to_string(),
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
        encrypted_function_args: None,
    }));

    Ok(())
}

#[tokio::test]
async fn specs_filter_deferred_dynamic_tools() -> anyhow::Result<()> {
    let (_, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let hidden_tool = "hidden_dynamic_tool";
    let visible_tool = "visible_dynamic_tool";
    let dynamic_tools = vec![DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
        name: "codex_app".to_string(),
        description: "Codex app tools.".to_string(),
        tools: vec![
            DynamicToolNamespaceTool::Function(DynamicToolFunctionSpec {
                name: hidden_tool.to_string(),
                description: "Hidden until discovered.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
                defer_loading: true,
            }),
            DynamicToolNamespaceTool::Function(DynamicToolFunctionSpec {
                name: visible_tool.to_string(),
                description: "Visible immediately.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
                defer_loading: false,
            }),
        ],
    })];

    let router = test_tool_router(step_context.as_ref(), Vec::new(), &dynamic_tools);

    assert_eq!(
        namespace_function_names(&router.model_visible_specs(), "codex_app"),
        vec![visible_tool.to_string()]
    );
    assert_eq!(
        router.deferred_tool_namespaces(),
        BTreeMap::from([("codex_app".to_string(), "Codex app tools.".to_string())])
    );

    Ok(())
}

#[tokio::test]
async fn extension_tool_executors_are_model_visible_and_dispatchable() -> anyhow::Result<()> {
    let (mut session, turn) = make_session_and_context().await;
    session.services.extensions = extension_tool_test_registry();
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let history_item = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "extension history".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    session
        .record_conversation_items(&turn, std::slice::from_ref(&history_item))
        .await;
    let mut expected_history_item = history_item.clone();
    expected_history_item.set_turn_id_if_missing(&turn.sub_id);

    let router = test_tool_router(
        step_context.as_ref(),
        extension_tool_executors(
            &session,
            &codex_extension_api::ExtensionData::new(turn.sub_id.clone()),
        ),
        &turn.dynamic_tools,
    );

    assert!(
        router.model_visible_specs().iter().any(
            |spec| matches!(spec, ToolSpec::Namespace(namespace)
            if namespace.name == "extension/"
                && namespace.tools.iter().any(|tool| matches!(
                    tool,
                    ResponsesApiNamespaceTool::Function(tool) if tool.name == "echo"
                )))
        ),
        "expected extension-provided tool to be visible to the model"
    );

    let call = ToolRouter::build_tool_call(ResponseItem::FunctionCall {
        id: None,
        name: "echo".to_string(),
        namespace: Some("extension/".to_string()),
        arguments: json!({ "message": "hello" }).to_string(),
        call_id: "call-extension".to_string(),
        encrypted_function_args: None,
        internal_chat_message_metadata_passthrough: None,
    })?
    .expect("function_call should produce a tool call");
    let result = router
        .dispatch_tool_call_with_code_mode_result(
            Arc::new(session),
            step_context,
            CancellationToken::new(),
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call,
            ToolCallSource::Direct,
        )
        .await?;

    let response = result.into_response();
    match response {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "call-extension");
            let FunctionCallOutputBody::Text(text) = output.body else {
                panic!("expected text function call output")
            };
            let value: serde_json::Value =
                serde_json::from_str(&text).expect("extension tool output should be json");
            assert_eq!(
                strip_response_item_ids_from_json(value),
                strip_response_item_ids_from_json(json!({
                    "arguments": { "message": "hello" },
                    "callId": "call-extension",
                    "conversationHistory": [expected_history_item],
                    "ok": true,
                }))
            );
        }
        other => panic!("expected function call output, got {other:?}"),
    }

    Ok(())
}

fn namespace_function_names(specs: &[ToolSpec], namespace_name: &str) -> Vec<String> {
    specs
        .iter()
        .find_map(|spec| match spec {
            ToolSpec::Namespace(namespace) if namespace.name == namespace_name => Some(
                namespace
                    .tools
                    .iter()
                    .map(|tool| match tool {
                        ResponsesApiNamespaceTool::Function(tool) => tool.name.clone(),
                        ResponsesApiNamespaceTool::Custom(tool) => tool.name.clone(),
                    })
                    .collect(),
            ),
            ToolSpec::Function(_)
            | ToolSpec::Freeform(_)
            | ToolSpec::ToolSearch { .. }
            | ToolSpec::WebSearch { .. }
            | ToolSpec::Namespace(_) => None,
        })
        .unwrap_or_default()
}
