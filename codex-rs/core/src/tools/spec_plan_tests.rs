use std::collections::BTreeMap;
use std::sync::Arc;

use codex_features::Feature;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_5_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_PROVIDER_ID;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::openai_models::ApplyPatchToolType;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::openai_models::WebSearchToolType;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolCall as ExtensionToolCall;
use codex_tools::ToolExecutor;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolSpec;
use pretty_assertions::assert_eq;
use serde_json::json;

use crate::WaitForEnvironmentToolConfig;
use crate::config::CurrentTimeReminderConfig;
use crate::environment_selection::TurnEnvironmentState;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::tools::handlers::WaitForEnvironmentHandler;
use crate::tools::handlers::multi_agents_spec::MULTI_AGENT_V1_NAMESPACE;
use crate::tools::router::ToolRouter;
use crate::tools::spec_plan::append_source_tools;
use crate::tools::spec_plan::build_core_tool_registry;

const MULTI_AGENT_V2_NAMESPACE: &str = "collaboration";

#[derive(Default)]
struct ToolPlanInputs {
    extension_tool_executors: Vec<Arc<dyn ToolExecutor<ExtensionToolCall>>>,
    wait_for_environment_tool_config: Option<Arc<WaitForEnvironmentToolConfig>>,
    dynamic_tools: Vec<DynamicToolSpec>,
}

struct ToolPlanProbe {
    visible_specs: Vec<ToolSpec>,
    visible_names: Vec<String>,
    namespace_functions: BTreeMap<String, Vec<String>>,
    registered_names: Vec<String>,
    exposures: BTreeMap<String, ToolExposure>,
}

impl ToolPlanProbe {
    fn from_router(router: ToolRouter) -> Self {
        let visible_specs = router.model_visible_specs();
        let visible_names = visible_specs
            .iter()
            .map(|spec| spec.name().to_string())
            .collect::<Vec<_>>();
        let namespace_functions = visible_specs
            .iter()
            .filter_map(|spec| match spec {
                ToolSpec::Namespace(namespace) => Some((
                    namespace.name.clone(),
                    namespace
                        .tools
                        .iter()
                        .map(|tool| match tool {
                            ResponsesApiNamespaceTool::Function(tool) => tool.name.clone(),
                            ResponsesApiNamespaceTool::Custom(tool) => tool.name.clone(),
                        })
                        .collect::<Vec<_>>(),
                )),
                ToolSpec::Function(_)
                | ToolSpec::ToolSearch { .. }
                | ToolSpec::WebSearch { .. }
                | ToolSpec::Freeform(_) => None,
            })
            .collect::<BTreeMap<_, _>>();
        let registered_tool_names = router.registered_tool_names_for_test();
        let registered_names = registered_tool_names
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let exposures = registered_tool_names
            .iter()
            .filter_map(|name| {
                router
                    .tool_exposure_for_test(name)
                    .map(|exposure| (name.to_string(), exposure))
            })
            .collect::<BTreeMap<_, _>>();

        Self {
            visible_specs,
            visible_names,
            namespace_functions,
            registered_names,
            exposures,
        }
    }

    fn assert_visible_contains(&self, expected: &[&str]) {
        for name in expected {
            assert!(
                self.visible_names.iter().any(|visible| visible == name),
                "expected visible tool `{name}` in {:?}",
                self.visible_names
            );
        }
    }

    fn assert_visible_lacks(&self, expected_absent: &[&str]) {
        for name in expected_absent {
            assert!(
                !self.visible_names.iter().any(|visible| visible == name),
                "expected visible tool `{name}` to be absent from {:?}",
                self.visible_names
            );
        }
    }

    fn assert_registered_contains(&self, expected: &[&str]) {
        for name in expected {
            assert!(
                self.registered_names
                    .iter()
                    .any(|registered| registered == name),
                "expected registered tool `{name}` in {:?}",
                self.registered_names
            );
        }
    }

    fn assert_registered_lacks(&self, expected_absent: &[&str]) {
        for name in expected_absent {
            assert!(
                !self
                    .registered_names
                    .iter()
                    .any(|registered| registered == name),
                "expected registered tool `{name}` to be absent from {:?}",
                self.registered_names
            );
        }
    }

    fn namespace_function_names(&self, namespace: &str) -> &[String] {
        self.namespace_functions
            .get(namespace)
            .map_or(&[], Vec::as_slice)
    }

    fn visible_spec(&self, name: &str) -> &ToolSpec {
        self.visible_specs
            .iter()
            .find(|spec| spec.name() == name)
            .unwrap_or_else(|| panic!("expected visible spec `{name}` in {:?}", self.visible_names))
    }

    fn exposure(&self, name: &str) -> ToolExposure {
        *self
            .exposures
            .get(name)
            .unwrap_or_else(|| panic!("expected registered tool `{name}`"))
    }
}

async fn probe_with(
    configure_turn: impl FnOnce(&mut TurnContext),
    inputs: ToolPlanInputs,
) -> ToolPlanProbe {
    let (_session, mut turn) = make_session_and_context().await;
    configure_turn(&mut turn);
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let mut registry = build_core_tool_registry(
        step_context.turn.as_ref(),
        &step_context.environments,
        inputs.wait_for_environment_tool_config.as_ref(),
    );
    let hosted_specs = append_source_tools(
        step_context.turn.as_ref(),
        &mut registry,
        inputs.extension_tool_executors,
        &inputs.dynamic_tools,
    );
    let router = ToolRouter::from_registry(
        step_context.turn.as_ref(),
        registry,
        hosted_specs,
        &Default::default(),
    );
    ToolPlanProbe::from_router(router)
}

async fn probe(configure_turn: impl FnOnce(&mut TurnContext)) -> ToolPlanProbe {
    probe_with(configure_turn, ToolPlanInputs::default()).await
}

fn set_feature(turn: &mut TurnContext, feature: Feature, enabled: bool) {
    let mut config = (*turn.config).clone();
    if enabled {
        config
            .features
            .enable(feature)
            .expect("test feature should be enableable in config");
    } else {
        config
            .features
            .disable(feature)
            .expect("test feature should be disableable in config");
    }
    turn.multi_agent_version = config.multi_agent_version_from_features();
    turn.config = Arc::new(config);
}

fn set_features(turn: &mut TurnContext, features: &[Feature]) {
    for feature in features {
        set_feature(turn, *feature, /*enabled*/ true);
    }
}

fn zsh_fork_config_for_spec_plan_tests() -> codex_tools::ZshForkConfig {
    let placeholder_exe = codex_utils_absolute_path::AbsolutePathBuf::try_from(
        std::env::current_exe().expect("current exe path"),
    )
    .expect("current exe should be absolute");

    // Spec planning only checks whether the shell mode is ZshFork. These paths
    // are never executed, so use a stable absolute placeholder instead of
    // depending on packaged zsh-fork artifacts in schema tests.
    codex_tools::ZshForkConfig {
        shell_zsh_path: placeholder_exe.clone(),
        main_execve_wrapper_exe: placeholder_exe,
    }
}

fn update_config(turn: &mut TurnContext, update: impl FnOnce(&mut crate::config::Config)) {
    let mut config = (*turn.config).clone();
    update(&mut config);
    turn.config = Arc::new(config);
}

fn set_web_search_mode(turn: &mut TurnContext, mode: WebSearchMode) {
    update_config(turn, |config| {
        config
            .web_search_mode
            .set(mode)
            .expect("test web search mode should be accepted");
    });
}

fn use_chatgpt_auth(turn: &mut TurnContext) {
    turn.auth_manager = Some(AuthManager::from_auth_for_testing(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
    ));
    turn.provider = create_model_provider(
        turn.config.model_provider.clone(),
        turn.auth_manager.clone(),
    );
}

fn use_bedrock_provider(turn: &mut TurnContext) {
    let provider_info = ModelProviderInfo::create_amazon_bedrock_provider(/*aws*/ None);
    update_config(turn, |config| {
        config.model_provider_id = AMAZON_BEDROCK_PROVIDER_ID.to_string();
        config.model_provider = provider_info.clone();
    });
    turn.provider = create_model_provider(provider_info, turn.auth_manager.clone());
}

struct TestNamespaceExtensionTool {
    namespace: &'static str,
    tool_name: &'static str,
}

impl ToolExecutor<ExtensionToolCall> for TestNamespaceExtensionTool {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(self.namespace, self.tool_name)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(codex_tools::ResponsesApiNamespace {
            name: self.namespace.to_string(),
            description: "Test namespace.".to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: self.tool_name.to_string(),
                description: "Test namespace tool.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            })],
        })
    }

    fn handle(&self, _call: ExtensionToolCall) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async {
            Ok(Box::new(codex_tools::JsonToolOutput::new(json!({}))) as Box<dyn ToolOutput>)
        })
    }
}

struct DeferredExtensionTool;

impl ToolExecutor<ExtensionToolCall> for DeferredExtensionTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("extension_echo")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "extension_echo".to_string(),
            description: "Echoes arguments through an extension tool.".to_string(),
            strict: true,
            defer_loading: None,
            parameters: codex_tools::JsonSchema::object(
                BTreeMap::from([(
                    "message".to_string(),
                    codex_tools::JsonSchema::string(/*description*/ None),
                )]),
                Some(vec!["message".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Deferred
    }

    fn handle(&self, _call: ExtensionToolCall) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async { panic!("spec planning should not execute extension tools") })
    }
}

fn duplicate_primary_environment(turn: &mut TurnContext) {
    let mut second_environment = turn
        .environments
        .primary()
        .expect("primary environment")
        .clone();
    second_environment.environment_id = "secondary".to_string();
    turn.environments
        .environments
        .push(TurnEnvironmentState::Ready(second_environment));
}

fn dynamic_tool(namespace: Option<&str>, name: &str, defer_loading: bool) -> DynamicToolSpec {
    let function = codex_protocol::dynamic_tools::DynamicToolFunctionSpec {
        name: name.to_string(),
        description: format!("{name} dynamic tool"),
        input_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }),
        defer_loading,
    };
    match namespace {
        Some(namespace) => {
            DynamicToolSpec::Namespace(codex_protocol::dynamic_tools::DynamicToolNamespaceSpec {
                name: namespace.to_string(),
                description: format!("{namespace} dynamic tools"),
                tools: vec![
                    codex_protocol::dynamic_tools::DynamicToolNamespaceTool::Function(function),
                ],
            })
        }
        None => DynamicToolSpec::Function(function),
    }
}

fn has_parameter(spec: &ToolSpec, parameter_name: &str) -> bool {
    serde_json::to_value(spec)
        .expect("tool spec should serialize")
        .pointer(&format!("/parameters/properties/{parameter_name}"))
        .is_some()
}

fn apply_patch_accepts_environment_id(spec: &ToolSpec) -> bool {
    match spec {
        ToolSpec::Freeform(tool) if tool.name == "apply_patch" => {
            tool.format.definition.contains("Environment ID")
        }
        _ => false,
    }
}

#[tokio::test]
async fn wait_for_environment_requires_feature_and_uses_host_config_when_present() {
    const TOOL_DESCRIPTION: &str = "Host-provided wait tool description";
    const ENVIRONMENT_ID_DESCRIPTION: &str = "Host-provided environment ID description";

    for deferred_executor_enabled in [false, true] {
        for config_present in [false, true] {
            let wait_for_environment_tool_config = config_present.then(|| {
                Arc::new(WaitForEnvironmentToolConfig {
                    tool_description: TOOL_DESCRIPTION.to_string(),
                    environment_id_description: ENVIRONMENT_ID_DESCRIPTION.to_string(),
                })
            });
            let plan = probe_with(
                |turn| {
                    set_feature(turn, Feature::DeferredExecutor, deferred_executor_enabled);
                },
                ToolPlanInputs {
                    wait_for_environment_tool_config,
                    ..ToolPlanInputs::default()
                },
            )
            .await;

            if deferred_executor_enabled {
                plan.assert_visible_contains(&["wait_for_environment"]);
                plan.assert_registered_contains(&["wait_for_environment"]);
                if !config_present {
                    assert_eq!(
                        plan.visible_spec("wait_for_environment"),
                        &WaitForEnvironmentHandler::default().spec()
                    );
                    continue;
                }
                let ToolSpec::Function(ResponsesApiTool {
                    description,
                    parameters,
                    ..
                }) = plan.visible_spec("wait_for_environment")
                else {
                    panic!("expected wait_for_environment function spec");
                };
                assert_eq!(description, TOOL_DESCRIPTION);
                assert_eq!(
                    parameters
                        .properties
                        .as_ref()
                        .and_then(|properties| properties.get("environment_id"))
                        .and_then(|schema| schema.description.as_deref()),
                    Some(ENVIRONMENT_ID_DESCRIPTION)
                );
            } else {
                plan.assert_visible_lacks(&["wait_for_environment"]);
                plan.assert_registered_lacks(&["wait_for_environment"]);
            }
        }
    }
}

#[tokio::test]
async fn wait_for_environment_falls_back_for_oversized_host_configuration() {
    const MAX_COMBINED_DESCRIPTION_BYTES: usize = 1_024;

    for (tool_description, environment_id_description) in [
        (
            "x".repeat(MAX_COMBINED_DESCRIPTION_BYTES + 1),
            String::new(),
        ),
        (
            String::new(),
            "x".repeat(MAX_COMBINED_DESCRIPTION_BYTES + 1),
        ),
        ("x".repeat(512), "x".repeat(513)),
        // The descriptions fit the aggregate input cap, but the complete serialized schema does
        // not fit its model-context cap once the surrounding tool definition is included.
        ("x".repeat(500), "x".repeat(500)),
    ] {
        let configured_tool_description = tool_description.clone();
        let configured_environment_id_description = environment_id_description.clone();
        let plan = probe_with(
            |turn| {
                set_feature(turn, Feature::DeferredExecutor, /*enabled*/ true);
            },
            ToolPlanInputs {
                wait_for_environment_tool_config: Some(Arc::new(WaitForEnvironmentToolConfig {
                    tool_description,
                    environment_id_description,
                })),
                ..ToolPlanInputs::default()
            },
        )
        .await;

        plan.assert_visible_contains(&["wait_for_environment"]);
        plan.assert_registered_contains(&["wait_for_environment"]);
        let ToolSpec::Function(ResponsesApiTool {
            description,
            parameters,
            ..
        }) = plan.visible_spec("wait_for_environment")
        else {
            panic!("expected wait_for_environment function spec");
        };
        let environment_id_description = parameters
            .properties
            .as_ref()
            .and_then(|properties| properties.get("environment_id"))
            .and_then(|schema| schema.description.as_deref())
            .expect("environment_id description should be present");
        assert_ne!(description, &configured_tool_description);
        assert_ne!(
            environment_id_description,
            configured_environment_id_description
        );
        assert!(
            serde_json::to_vec(plan.visible_spec("wait_for_environment"))
                .expect("tool spec should serialize")
                .len()
                <= 1_000
        );
    }
}

#[tokio::test]
async fn request_user_input_tool_respects_experimental_config_gate() {
    let enabled = probe(|_| {}).await;
    enabled.assert_visible_contains(&["request_user_input"]);
    enabled.assert_registered_contains(&["request_user_input"]);
    assert_eq!(
        enabled.exposure("request_user_input"),
        ToolExposure::DirectModelOnly
    );

    let disabled = probe(|turn| {
        update_config(turn, |config| {
            config.experimental_request_user_input_enabled = false;
        });
    })
    .await;
    disabled.assert_visible_lacks(&["request_user_input"]);
    disabled.assert_registered_lacks(&["request_user_input"]);
}

#[tokio::test]
async fn update_plan_tool_respects_config_gate() {
    let enabled = probe(|_| {}).await;
    enabled.assert_visible_contains(&["update_plan"]);
    enabled.assert_registered_contains(&["update_plan"]);

    let disabled = probe(|turn| {
        update_config(turn, |config| {
            config.update_plan_enabled = false;
        });
    })
    .await;
    disabled.assert_visible_lacks(&["update_plan"]);
    disabled.assert_registered_lacks(&["update_plan"]);
}

#[tokio::test]
async fn request_user_input_stays_direct_in_code_mode_only() {
    let plan = probe(|turn| {
        set_features(turn, &[Feature::CodeMode, Feature::CodeModeOnly]);
    })
    .await;

    plan.assert_visible_contains(&[
        "request_user_input",
        codex_code_mode::PUBLIC_TOOL_NAME,
        codex_code_mode::WAIT_TOOL_NAME,
    ]);
    plan.assert_registered_contains(&["request_user_input"]);
    assert_eq!(
        plan.exposure("request_user_input"),
        ToolExposure::DirectModelOnly
    );

    let ToolSpec::Freeform(exec) = plan.visible_spec(codex_code_mode::PUBLIC_TOOL_NAME) else {
        panic!("expected code mode exec tool");
    };
    assert!(!exec.description.contains("request_user_input"));
}

#[tokio::test]
async fn shell_family_registers_visible_unified_exec_and_hidden_legacy_shell() {
    let plan = probe(|turn| {
        set_features(turn, &[Feature::ShellTool, Feature::UnifiedExec]);
        set_feature(turn, Feature::ShellZshFork, /*enabled*/ false);
        turn.model_info.shell_type = ConfigShellToolType::ShellCommand;
    })
    .await;

    plan.assert_visible_contains(&["exec_command", "write_stdin"]);
    plan.assert_visible_lacks(&["shell_command"]);
    plan.assert_registered_contains(&["exec_command", "write_stdin", "shell_command"]);
    assert_eq!(plan.exposure("shell_command"), ToolExposure::Hidden);
    assert!(has_parameter(plan.visible_spec("exec_command"), "shell"));
}

#[tokio::test]
async fn login_shell_parameter_follows_selected_environment() {
    for tool_name in ["shell_command", "exec_command"] {
        for allow_login_shell in [false, true] {
            let plan = probe(|turn| {
                set_feature(turn, Feature::ShellTool, /*enabled*/ true);
                set_feature(turn, Feature::UnifiedExec, tool_name == "exec_command");
                set_feature(turn, Feature::ShellZshFork, /*enabled*/ false);
                turn.model_info.shell_type = ConfigShellToolType::ShellCommand;
                update_config(turn, |config| {
                    config.permissions.allow_login_shell = !allow_login_shell;
                });
                let TurnEnvironmentState::Ready(environment) = turn
                    .environments
                    .environments
                    .first_mut()
                    .expect("primary environment")
                else {
                    panic!("primary environment should be ready");
                };
                environment.config.allow_login_shell = allow_login_shell;
            })
            .await;

            assert_eq!(
                has_parameter(plan.visible_spec(tool_name), "login"),
                allow_login_shell
            );
        }
    }
}

#[tokio::test]
async fn login_shell_parameter_is_available_when_any_environment_allows_it() {
    let plan = probe(|turn| {
        set_features(turn, &[Feature::ShellTool, Feature::UnifiedExec]);
        set_feature(turn, Feature::ShellZshFork, /*enabled*/ false);
        update_config(turn, |config| {
            config.permissions.allow_login_shell = false;
        });
        duplicate_primary_environment(turn);
        for (index, environment) in turn.environments.environments.iter_mut().enumerate() {
            let TurnEnvironmentState::Ready(environment) = environment else {
                panic!("environment should be ready");
            };
            environment.config.allow_login_shell = index == 1;
        }
    })
    .await;

    assert!(has_parameter(plan.visible_spec("exec_command"), "login"));
}

#[tokio::test]
async fn shell_command_is_not_registered_without_a_single_local_environment() {
    let remote_environment = probe(|turn| {
        set_feature(turn, Feature::ShellTool, /*enabled*/ true);
        set_feature(turn, Feature::UnifiedExec, /*enabled*/ false);
        turn.model_info.shell_type = ConfigShellToolType::ShellCommand;

        let TurnEnvironmentState::Ready(environment) = turn
            .environments
            .environments
            .first_mut()
            .expect("primary environment")
        else {
            panic!("primary environment should be ready");
        };
        environment.environment_id = "remote".to_string();
        environment.environment = Arc::new(
            codex_exec_server::Environment::create_for_tests(Some(
                "ws://127.0.0.1:1/remote-exec-server".to_string(),
            ))
            .expect("remote test environment"),
        );
    })
    .await;
    remote_environment.assert_visible_lacks(&["shell_command", "exec_command", "write_stdin"]);
    remote_environment.assert_registered_lacks(&["shell_command", "exec_command", "write_stdin"]);

    let multiple_local_environments = probe(|turn| {
        set_feature(turn, Feature::ShellTool, /*enabled*/ true);
        set_feature(turn, Feature::UnifiedExec, /*enabled*/ false);
        turn.model_info.shell_type = ConfigShellToolType::ShellCommand;
        duplicate_primary_environment(turn);
    })
    .await;
    multiple_local_environments.assert_visible_lacks(&["shell_command"]);
    multiple_local_environments.assert_registered_lacks(&["shell_command"]);
}

#[tokio::test]
async fn dynamic_tools_cannot_reclaim_the_reserved_shell_command_name() {
    let plan = probe_with(
        duplicate_primary_environment,
        ToolPlanInputs {
            dynamic_tools: vec![
                dynamic_tool(
                    /*namespace*/ None,
                    "shell_command",
                    /*defer_loading*/ false,
                ),
                dynamic_tool(
                    Some("client"),
                    "shell_command",
                    /*defer_loading*/ false,
                ),
            ],
            ..ToolPlanInputs::default()
        },
    )
    .await;

    plan.assert_visible_lacks(&["shell_command"]);
    plan.assert_registered_lacks(&["shell_command"]);
    plan.assert_visible_contains(&["client"]);
    plan.assert_registered_contains(
        &[&ToolName::namespaced("client", "shell_command").to_string()],
    );
    assert_eq!(
        plan.namespace_function_names("client"),
        &["shell_command".to_string()]
    );
}

#[tokio::test]
async fn shell_zsh_fork_stays_standalone_until_unified_exec_composition_is_enabled() {
    let standalone = probe(|turn| {
        set_features(turn, &[Feature::ShellTool, Feature::UnifiedExec]);
        set_feature(turn, Feature::ShellZshFork, /*enabled*/ true);
        set_feature(turn, Feature::UnifiedExecZshFork, /*enabled*/ false);
        turn.model_info.shell_type = ConfigShellToolType::ShellCommand;
    })
    .await;

    standalone.assert_visible_contains(&["shell_command"]);
    standalone.assert_visible_lacks(&["exec_command", "write_stdin"]);
    standalone.assert_registered_contains(&["shell_command"]);
    standalone.assert_registered_lacks(&["exec_command", "write_stdin"]);

    let composed = probe(|turn| {
        set_features(
            turn,
            &[
                Feature::ShellTool,
                Feature::UnifiedExec,
                Feature::ShellZshFork,
                Feature::UnifiedExecZshFork,
            ],
        );
        turn.model_info.shell_type = ConfigShellToolType::ShellCommand;
    })
    .await;

    composed.assert_visible_contains(&["exec_command", "write_stdin"]);
    composed.assert_visible_lacks(&["shell_command"]);
    composed.assert_registered_contains(&["exec_command", "write_stdin", "shell_command"]);
    assert_eq!(composed.exposure("shell_command"), ToolExposure::Hidden);
}

#[tokio::test]
async fn zsh_fork_unified_exec_hides_shell_parameter() {
    let plan = probe(|turn| {
        set_features(
            turn,
            &[
                Feature::ShellTool,
                Feature::UnifiedExec,
                Feature::ShellZshFork,
                Feature::UnifiedExecZshFork,
            ],
        );
        turn.unified_exec_shell_mode =
            codex_tools::UnifiedExecShellMode::ZshFork(zsh_fork_config_for_spec_plan_tests());
    })
    .await;

    plan.assert_visible_contains(&["exec_command", "write_stdin"]);
    assert!(!has_parameter(plan.visible_spec("exec_command"), "shell"));
}

#[tokio::test]
async fn zsh_fork_unified_exec_keeps_shell_parameter_when_remote_environment_available() {
    let plan = probe(|turn| {
        set_features(
            turn,
            &[
                Feature::ShellTool,
                Feature::UnifiedExec,
                Feature::ShellZshFork,
                Feature::UnifiedExecZshFork,
            ],
        );
        turn.unified_exec_shell_mode =
            codex_tools::UnifiedExecShellMode::ZshFork(zsh_fork_config_for_spec_plan_tests());
        let remote_cwd = turn
            .environments
            .primary()
            .expect("primary environment")
            .cwd()
            .clone();
        turn.environments
            .environments
            .push(TurnEnvironmentState::Ready(
                crate::session::turn_context::TurnEnvironment::new(
                    "remote".to_string(),
                    Arc::new(
                        codex_exec_server::Environment::create_for_tests(Some(
                            "ws://127.0.0.1:1/remote-exec-server".to_string(),
                        ))
                        .expect("remote test environment"),
                    ),
                    remote_cwd,
                    Vec::new(),
                    /*shell*/ None,
                    crate::session::turn_context::EnvironmentConfig {
                        allow_login_shell: true,
                        permission_profile: turn
                            .config
                            .permissions
                            .permission_profile_state()
                            .snapshot(),
                    },
                ),
            ));
    })
    .await;

    plan.assert_visible_contains(&["exec_command", "write_stdin"]);
    plan.assert_visible_lacks(&["shell_command"]);
    plan.assert_registered_lacks(&["shell_command"]);
    assert!(has_parameter(plan.visible_spec("exec_command"), "shell"));
    assert!(has_parameter(
        plan.visible_spec("exec_command"),
        "environment_id"
    ));
}

#[tokio::test]
async fn environment_count_controls_environment_backed_tools() {
    let no_environment = probe(|turn| {
        turn.environments.environments.clear();
        set_feature(turn, Feature::ShellTool, /*enabled*/ true);
        set_feature(turn, Feature::RequestPermissionsTool, /*enabled*/ true);
        turn.model_info.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
    })
    .await;
    no_environment.assert_visible_lacks(&[
        "shell_command",
        "exec_command",
        "apply_patch",
        "view_image",
        "request_permissions",
    ]);
    no_environment.assert_registered_lacks(&[
        "shell_command",
        "exec_command",
        "apply_patch",
        "view_image",
        "request_permissions",
    ]);

    let multiple_environments = probe(|turn| {
        duplicate_primary_environment(turn);
        set_feature(turn, Feature::ShellTool, /*enabled*/ true);
        set_feature(turn, Feature::UnifiedExec, /*enabled*/ true);
        set_feature(turn, Feature::RequestPermissionsTool, /*enabled*/ true);
        turn.model_info.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
    })
    .await;
    multiple_environments.assert_visible_contains(&[
        "exec_command",
        "apply_patch",
        "view_image",
        "request_permissions",
    ]);
    multiple_environments.assert_visible_lacks(&["shell_command"]);
    multiple_environments.assert_registered_lacks(&["shell_command"]);
    assert!(has_parameter(
        multiple_environments.visible_spec("exec_command"),
        "environment_id"
    ));
    assert!(apply_patch_accepts_environment_id(
        multiple_environments.visible_spec("apply_patch")
    ));
    assert!(has_parameter(
        multiple_environments.visible_spec("view_image"),
        "environment_id"
    ));
}

#[tokio::test]
async fn environment_tools_follow_the_step_context() {
    let (_session, mut turn) = make_session_and_context().await;
    set_feature(&mut turn, Feature::UnifiedExec, /*enabled*/ true);
    turn.model_info.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);

    let environments = turn.environments.clone();
    turn.environments.environments.clear();
    let turn = Arc::new(turn);
    let plan = ToolPlanProbe::from_router(ToolRouter::from_registry(
        turn.as_ref(),
        build_core_tool_registry(
            turn.as_ref(),
            &environments,
            /*wait_for_environment_tool_config*/ None,
        ),
        super::hosted_model_tool_specs(turn.as_ref(), &[]),
        &Default::default(),
    ));

    plan.assert_visible_contains(&["exec_command", "apply_patch", "view_image"]);
}

#[tokio::test]
async fn sleep_tool_follows_current_time_config() {
    let disabled = probe(|turn| {
        set_feature(turn, Feature::CurrentTimeReminder, /*enabled*/ true);
    })
    .await;
    assert_eq!(disabled.namespace_function_names("clock"), ["curr_time"]);

    let enabled = probe(|turn| {
        set_feature(turn, Feature::CurrentTimeReminder, /*enabled*/ true);
        let mut config = (*turn.config).clone();
        config.current_time_reminder = Some(CurrentTimeReminderConfig {
            sleep_tool: true,
            ..CurrentTimeReminderConfig::default()
        });
        turn.config = Arc::new(config);
    })
    .await;
    assert_eq!(
        enabled.namespace_function_names("clock"),
        ["curr_time", "sleep"]
    );
}

#[tokio::test]
async fn sleep_tool_stays_direct_and_outside_code_mode() {
    for code_mode_only in [false, true] {
        let plan = probe(|turn| {
            set_features(
                turn,
                &[
                    Feature::CodeMode,
                    Feature::CurrentTimeReminder,
                    Feature::MultiAgentV2,
                ],
            );
            if code_mode_only {
                set_feature(turn, Feature::CodeModeOnly, /*enabled*/ true);
            }
            update_config(turn, |config| {
                config.current_time_reminder = Some(CurrentTimeReminderConfig {
                    sleep_tool: true,
                    ..CurrentTimeReminderConfig::default()
                });
                config.multi_agent_v2.wait_agent_enabled = false;
            });
        })
        .await;

        assert!(
            plan.namespace_function_names("clock")
                .iter()
                .any(|name| name == "sleep")
        );
        let sleep_tool_name = ToolName::namespaced("clock", "sleep").to_string();
        let wait_agent_tool_name =
            ToolName::namespaced(MULTI_AGENT_V2_NAMESPACE, "wait_agent").to_string();
        assert_eq!(
            plan.exposure(&sleep_tool_name),
            ToolExposure::DirectModelOnly
        );
        plan.assert_registered_lacks(&[wait_agent_tool_name.as_str()]);

        let ToolSpec::Freeform(exec) = plan.visible_spec(codex_code_mode::PUBLIC_TOOL_NAME) else {
            panic!("expected code mode exec tool");
        };
        if code_mode_only {
            assert!(exec.description.contains("clock__curr_time"));
        }
        assert!(!exec.description.contains("clock__sleep"));
    }
}

#[tokio::test]
async fn deferred_extension_tools_are_discoverable_with_tool_search() {
    let plan = probe_with(
        |turn| {
            turn.model_info.supports_search_tool = true;
        },
        ToolPlanInputs {
            extension_tool_executors: vec![Arc::new(DeferredExtensionTool)],
            ..ToolPlanInputs::default()
        },
    )
    .await;

    plan.assert_visible_contains(&["tool_search"]);
    plan.assert_visible_lacks(&["extension_echo"]);
    plan.assert_registered_contains(&["extension_echo"]);
    assert_eq!(plan.exposure("extension_echo"), ToolExposure::Deferred);
}

#[tokio::test]
async fn code_mode_only_exposes_code_executor_and_hides_nested_tools() {
    let input = ToolPlanInputs {
        dynamic_tools: vec![dynamic_tool(
            Some("codex_app"),
            "lookup",
            /*defer_loading*/ false,
        )],
        ..ToolPlanInputs::default()
    };
    let plain = probe_with(|_| {}, input).await;
    assert_eq!(
        plain.namespace_function_names("codex_app"),
        &["lookup".to_string()]
    );
    plain.assert_visible_lacks(&[
        codex_code_mode::PUBLIC_TOOL_NAME,
        codex_code_mode::WAIT_TOOL_NAME,
    ]);

    let code_mode_only = probe_with(
        |turn| {
            set_features(turn, &[Feature::CodeMode, Feature::CodeModeOnly]);
        },
        ToolPlanInputs {
            dynamic_tools: vec![dynamic_tool(
                Some("codex_app"),
                "lookup",
                /*defer_loading*/ false,
            )],
            ..ToolPlanInputs::default()
        },
    )
    .await;
    code_mode_only.assert_visible_contains(&[
        codex_code_mode::PUBLIC_TOOL_NAME,
        codex_code_mode::WAIT_TOOL_NAME,
    ]);
    assert_eq!(
        code_mode_only.namespace_function_names("codex_app"),
        Vec::<String>::new().as_slice()
    );
}

#[tokio::test]
async fn code_mode_buffered_exec_updates_exec_description() {
    let plan = probe(|turn| {
        set_features(turn, &[Feature::CodeMode, Feature::CodeModeBufferedExec]);
    })
    .await;

    let ToolSpec::Freeform(exec) = plan.visible_spec(codex_code_mode::PUBLIC_TOOL_NAME) else {
        panic!("expected code mode exec tool");
    };
    assert!(exec.description.contains("Defaults to 30000 ms."));
    assert!(!exec.description.contains("Defaults to 10000 ms."));
}

#[tokio::test]
async fn code_mode_only_exposes_configured_dynamic_namespace_directly() {
    let plan = probe_with(
        |turn| {
            set_features(turn, &[Feature::CodeMode, Feature::CodeModeOnly]);
            turn.model_info.supports_search_tool = true;
            update_config(turn, |config| {
                config.code_mode.direct_only_tool_namespaces = vec!["direct_only".to_string()];
            });
        },
        ToolPlanInputs {
            dynamic_tools: vec![dynamic_tool(
                Some("direct_only"),
                "lookup",
                /*defer_loading*/ true,
            )],
            ..ToolPlanInputs::default()
        },
    )
    .await;

    plan.assert_visible_contains(&[
        codex_code_mode::PUBLIC_TOOL_NAME,
        codex_code_mode::WAIT_TOOL_NAME,
        "direct_only",
    ]);
    plan.assert_visible_lacks(&["tool_search"]);
    assert_eq!(
        plan.exposure(&ToolName::namespaced("direct_only", "lookup").to_string()),
        ToolExposure::DirectModelOnly
    );
    let ToolSpec::Namespace(namespace) = plan.visible_spec("direct_only") else {
        panic!("expected direct-only namespace spec");
    };
    let ResponsesApiNamespaceTool::Function(tool) = &namespace.tools[0] else {
        panic!("expected direct-only namespace function tool");
    };
    assert_eq!(tool.defer_loading, None);
    let ToolSpec::Freeform(exec) = plan.visible_spec(codex_code_mode::PUBLIC_TOOL_NAME) else {
        panic!("expected code mode exec tool");
    };
    assert!(!exec.description.contains("direct_only_lookup(args:"));
}

#[tokio::test]
async fn code_mode_only_exposes_default_namespace_tools_directly() {
    let plan = probe(|turn| {
        set_features(turn, &[Feature::CodeMode, Feature::CodeModeOnly]);
        update_config(turn, |config| {
            config.code_mode.direct_only_tool_namespaces = vec!["functions".to_string()];
        });
    })
    .await;

    plan.assert_visible_contains(&["update_plan"]);
    assert_eq!(plan.exposure("update_plan"), ToolExposure::DirectModelOnly);

    let ToolSpec::Freeform(exec) = plan.visible_spec(codex_code_mode::PUBLIC_TOOL_NAME) else {
        panic!("expected code mode exec tool");
    };
    assert!(!exec.description.contains("update_plan(args:"));
}

#[tokio::test]
async fn excluded_deferred_namespaces_do_not_enable_nested_tool_guidance() {
    let plan = probe_with(
        |turn| {
            set_features(turn, &[Feature::CodeMode, Feature::CodeModeOnly]);
            set_feature(turn, Feature::Collab, /*enabled*/ false);
            turn.model_info.supports_search_tool = true;
            update_config(turn, |config| {
                config.code_mode.excluded_tool_namespaces = vec!["excluded".to_string()];
            });
        },
        ToolPlanInputs {
            dynamic_tools: vec![dynamic_tool(
                Some("excluded"),
                "lookup",
                /*defer_loading*/ true,
            )],
            ..ToolPlanInputs::default()
        },
    )
    .await;

    let ToolSpec::Freeform(exec) = plan.visible_spec(codex_code_mode::PUBLIC_TOOL_NAME) else {
        panic!("expected code mode exec tool");
    };
    assert!(
        !exec
            .description
            .contains("Some deferred nested tools may be omitted")
    );
    plan.assert_registered_contains(&[
        &ToolName::namespaced("excluded", "lookup").to_string(),
        "tool_search",
    ]);
}

#[tokio::test]
async fn code_mode_excludes_default_namespace_tools() {
    let plan = probe(|turn| {
        set_feature(turn, Feature::CodeMode, /*enabled*/ true);
        update_config(turn, |config| {
            config.code_mode.excluded_tool_namespaces = vec!["functions".to_string()];
        });
    })
    .await;

    plan.assert_visible_contains(&["update_plan"]);
    plan.assert_registered_contains(&["update_plan"]);
    assert_eq!(plan.exposure("update_plan"), ToolExposure::Direct);

    let ToolSpec::Freeform(exec) = plan.visible_spec(codex_code_mode::PUBLIC_TOOL_NAME) else {
        panic!("expected code mode exec tool");
    };
    assert!(!exec.description.contains("update_plan(args:"));
}

#[tokio::test]
async fn multi_agent_feature_selects_one_agent_tool_family() {
    let v1 = probe(|turn| {
        set_feature(turn, Feature::Collab, /*enabled*/ true);
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ false);
    })
    .await;
    v1.assert_visible_contains(&[MULTI_AGENT_V1_NAMESPACE]);
    v1.assert_visible_lacks(&[
        "spawn_agent",
        "send_input",
        "resume_agent",
        "wait_agent",
        "close_agent",
        "interrupt_agent",
        "send_message",
        "followup_task",
        "assign_task",
        "list_agents",
    ]);
    assert_eq!(
        v1.namespace_function_names(MULTI_AGENT_V1_NAMESPACE),
        &[
            "close_agent".to_string(),
            "resume_agent".to_string(),
            "send_input".to_string(),
            "spawn_agent".to_string(),
            "wait_agent".to_string(),
        ]
    );
    let ToolSpec::Namespace(namespace) = v1.visible_spec(MULTI_AGENT_V1_NAMESPACE) else {
        panic!("expected v1 multi-agent namespace");
    };
    let Some(ResponsesApiNamespaceTool::Function(spawn_agent)) =
        namespace.tools.iter().find(|tool| {
            matches!(
                tool,
                ResponsesApiNamespaceTool::Function(tool) if tool.name == "spawn_agent"
            )
        })
    else {
        panic!("expected v1 spawn_agent function");
    };
    let properties = spawn_agent
        .parameters
        .properties
        .as_ref()
        .expect("spawn_agent should use object params");
    for property in ["model", "reasoning_effort", "service_tier"] {
        assert!(
            properties.contains_key(property),
            "expected v1 spawn_agent to expose `{property}`"
        );
    }
    assert!(!properties.contains_key("agent_type"));

    let v2 = probe(|turn| {
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ true);
        update_config(turn, |config| {
            config.multi_agent_v2.max_concurrent_threads_per_session = 17;
        });
    })
    .await;
    v2.assert_visible_contains(&[MULTI_AGENT_V2_NAMESPACE]);
    v2.assert_visible_lacks(&[
        "spawn_agent",
        "send_message",
        "followup_task",
        "wait_agent",
        "interrupt_agent",
        "list_agents",
        "send_input",
        "resume_agent",
        "assign_task",
        "close_agent",
    ]);
    for tool_name in [
        "spawn_agent",
        "send_message",
        "followup_task",
        "wait_agent",
        "interrupt_agent",
        "list_agents",
    ] {
        assert!(
            v2.namespace_function_names(MULTI_AGENT_V2_NAMESPACE)
                .iter()
                .any(|name| name == tool_name),
            "expected {tool_name} in {MULTI_AGENT_V2_NAMESPACE} namespace"
        );
    }
    let ToolSpec::Namespace(namespace) = v2.visible_spec(MULTI_AGENT_V2_NAMESPACE) else {
        panic!("expected {MULTI_AGENT_V2_NAMESPACE} namespace");
    };
    let Some(ResponsesApiNamespaceTool::Function(spawn_agent)) =
        namespace.tools.iter().find(|tool| {
            matches!(
                tool,
                ResponsesApiNamespaceTool::Function(tool) if tool.name == "spawn_agent"
            )
        })
    else {
        panic!("expected spawn_agent in {MULTI_AGENT_V2_NAMESPACE} namespace");
    };
    let spawn_agent_properties = spawn_agent
        .parameters
        .properties
        .as_ref()
        .expect("spawn_agent should use object params");
    for property in ["model", "reasoning_effort"] {
        assert!(spawn_agent_properties.contains_key(property));
    }
    for property in ["agent_type", "service_tier"] {
        assert!(!spawn_agent_properties.contains_key(property));
    }
    let spawn_agent_description = spawn_agent.description.as_str();
    assert!(!spawn_agent_description.contains("max_concurrent_threads_per_session"));
    assert!(spawn_agent_description.contains(
        "Note that passing `fork_turns=\"none\"` will not pass any surrounding context to the spawned subagent"
    ));

    let direct_model_only = probe(|turn| {
        set_features(
            turn,
            &[
                Feature::CodeMode,
                Feature::CodeModeOnly,
                Feature::MultiAgentV2,
            ],
        );
        update_config(turn, |config| {
            config.multi_agent_v2.non_code_mode_only = true;
        });
    })
    .await;
    direct_model_only.assert_visible_contains(&[MULTI_AGENT_V2_NAMESPACE]);
    direct_model_only.assert_visible_lacks(&["spawn_agent", "send_message", "wait_agent"]);
    assert_eq!(
        direct_model_only
            .exposure(&ToolName::namespaced(MULTI_AGENT_V2_NAMESPACE, "spawn_agent").to_string()),
        ToolExposure::DirectModelOnly
    );
}

#[tokio::test]
async fn multi_agent_v2_message_schemas_are_encrypted() {
    let plan = probe(|turn| {
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ true);
    })
    .await;
    let ToolSpec::Namespace(namespace) = plan.visible_spec(MULTI_AGENT_V2_NAMESPACE) else {
        panic!("expected {MULTI_AGENT_V2_NAMESPACE} namespace");
    };
    for tool_name in ["spawn_agent", "send_message", "followup_task"] {
        let Some(ResponsesApiNamespaceTool::Function(tool)) = namespace.tools.iter().find(|tool| {
            matches!(
                tool,
                ResponsesApiNamespaceTool::Function(tool) if tool.name == tool_name
            )
        }) else {
            panic!("expected {tool_name} in {MULTI_AGENT_V2_NAMESPACE} namespace");
        };
        let properties = tool
            .parameters
            .properties
            .as_ref()
            .expect("tool should use object params");
        assert_eq!(
            properties
                .get("message")
                .and_then(|schema| schema.encrypted),
            Some(true)
        );
    }
}

#[tokio::test]
async fn multi_agent_v2_can_disable_wait_agent() {
    let plan = probe(|turn| {
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ true);
        update_config(turn, |config| {
            config.multi_agent_v2.wait_agent_enabled = false;
        });
    })
    .await;

    assert_eq!(
        plan.namespace_function_names(MULTI_AGENT_V2_NAMESPACE),
        &[
            "followup_task".to_string(),
            "interrupt_agent".to_string(),
            "list_agents".to_string(),
            "send_message".to_string(),
            "spawn_agent".to_string(),
        ]
    );
    plan.assert_visible_lacks(&["clock"]);
    plan.assert_registered_lacks(&["collaboration.wait_agent", "clock.sleep"]);
}

#[tokio::test]
async fn tool_mode_selector_overrides_feature_flags() {
    let direct = probe(|turn| {
        set_features(turn, &[Feature::CodeMode, Feature::CodeModeOnly]);
        turn.model_info.tool_mode = Some(ToolMode::Direct);
    })
    .await;
    direct.assert_visible_lacks(&[
        codex_code_mode::PUBLIC_TOOL_NAME,
        codex_code_mode::WAIT_TOOL_NAME,
    ]);
}

#[tokio::test]
async fn v1_multi_agent_tools_defer_when_tool_search_available() {
    let plan = probe(|turn| {
        turn.model_info.supports_search_tool = true;
        set_feature(turn, Feature::Collab, /*enabled*/ true);
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ false);
    })
    .await;

    plan.assert_visible_contains(&["tool_search"]);
    plan.assert_visible_lacks(&[
        "spawn_agent",
        "send_input",
        "resume_agent",
        "wait_agent",
        "close_agent",
        "interrupt_agent",
    ]);
    for tool_name in [
        "spawn_agent",
        "send_input",
        "resume_agent",
        "wait_agent",
        "close_agent",
    ] {
        let namespaced_tool_name = ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, tool_name);
        let namespaced_tool_name = namespaced_tool_name.to_string();
        assert!(
            plan.registered_names.contains(&namespaced_tool_name),
            "expected namespaced runtime for {tool_name}"
        );
        assert!(
            !plan
                .registered_names
                .contains(&ToolName::plain(tool_name).to_string()),
            "expected no plain runtime for deferred {tool_name}"
        );
        assert_eq!(plan.exposure(&namespaced_tool_name), ToolExposure::Deferred);
    }
    let ToolSpec::ToolSearch { description, .. } = plan.visible_spec("tool_search") else {
        panic!("expected visible tool_search spec");
    };
    assert!(description.contains("- Multi-agent tools: Spawn and manage sub-agents."));
}

#[tokio::test]
async fn multi_agent_v2_can_use_configured_tool_namespace() {
    let namespaced = probe(|turn| {
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ true);
        update_config(turn, |config| {
            config.multi_agent_v2.tool_namespace = Some("agents".to_string());
        });
    })
    .await;

    namespaced.assert_visible_contains(&["agents"]);
    namespaced.assert_visible_lacks(&["assign_task"]);
    assert!(
        !namespaced
            .registered_names
            .contains(&ToolName::namespaced("agents", "assign_task").to_string()),
        "expected no namespaced runtime for assign_task"
    );
    assert!(
        !namespaced
            .namespace_function_names("agents")
            .iter()
            .any(|name| name == "assign_task"),
        "expected assign_task to be absent from agents namespace"
    );
    for tool_name in [
        "spawn_agent",
        "send_message",
        "followup_task",
        "wait_agent",
        "interrupt_agent",
        "list_agents",
    ] {
        namespaced.assert_visible_lacks(&[tool_name]);
        assert!(
            namespaced
                .registered_names
                .contains(&ToolName::namespaced("agents", tool_name).to_string()),
            "expected namespaced runtime for {tool_name}"
        );
        assert!(
            !namespaced
                .registered_names
                .contains(&ToolName::plain(tool_name).to_string()),
            "expected no plain runtime for {tool_name}"
        );
        assert!(
            namespaced
                .namespace_function_names("agents")
                .iter()
                .any(|name| name == tool_name),
            "expected {tool_name} in agents namespace"
        );
    }
}

#[tokio::test]
async fn multi_agent_v2_namespace_is_supported_by_bedrock_provider() {
    let plan = probe(|turn| {
        set_feature(turn, Feature::MultiAgentV2, /*enabled*/ true);
        update_config(turn, |config| {
            config.multi_agent_v2.tool_namespace = Some("agents".to_string());
        });
        use_bedrock_provider(turn);
    })
    .await;

    plan.assert_visible_contains(&["agents"]);
    plan.assert_visible_lacks(&["spawn_agent", "send_message", "list_agents"]);
    assert!(
        !plan
            .registered_names
            .contains(&ToolName::plain("spawn_agent").to_string())
    );
    assert!(
        plan.registered_names
            .contains(&ToolName::namespaced("agents", "spawn_agent").to_string())
    );
}

#[tokio::test]
async fn multi_agent_v2_bedrock_workers_only_delegate_when_model_supports_v2() {
    for (model, model_multi_agent_version, supports_delegation) in [
        (
            AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID,
            Some(MultiAgentVersion::V2),
            true,
        ),
        (
            AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID,
            Some(MultiAgentVersion::V1),
            false,
        ),
        (AMAZON_BEDROCK_GPT_5_5_MODEL_ID, None, false),
    ] {
        let plan = probe(|turn| {
            set_feature(turn, Feature::MultiAgentV2, /*enabled*/ true);
            update_config(turn, |config| {
                config.multi_agent_v2.tool_namespace = Some("agents".to_string());
            });
            use_bedrock_provider(turn);
            turn.model_info.slug = model.to_string();
            turn.model_info.multi_agent_version = model_multi_agent_version;
            turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: ThreadId::new(),
                depth: 1,
                agent_path: Some(AgentPath::try_from("/root/worker").expect("valid agent path")),
                agent_nickname: None,
                agent_role: None,
            });
        })
        .await;

        let spawn_agent_name = ToolName::namespaced("agents", "spawn_agent").to_string();
        let followup_task_name = ToolName::namespaced("agents", "followup_task").to_string();
        if supports_delegation {
            plan.assert_visible_contains(&["agents"]);
            plan.assert_registered_contains(&[&spawn_agent_name, &followup_task_name]);
        } else {
            plan.assert_visible_lacks(&["agents"]);
            plan.assert_registered_lacks(&[&spawn_agent_name, &followup_task_name]);
        }
    }
}

#[tokio::test]
async fn code_mode_only_can_expose_namespaced_multi_agent_v2_as_normal_tools() {
    let plan = probe(|turn| {
        set_features(
            turn,
            &[
                Feature::CodeMode,
                Feature::CodeModeOnly,
                Feature::MultiAgentV2,
            ],
        );
        update_config(turn, |config| {
            config.multi_agent_v2.non_code_mode_only = true;
            config.multi_agent_v2.tool_namespace = Some("agents".to_string());
        });
    })
    .await;

    assert_eq!(
        plan.visible_names,
        vec![
            "exec",
            "wait",
            "request_user_input",
            "agents",
            // Hosted Responses tool.
            "web_search",
        ]
    );
    assert!(
        !plan
            .namespace_function_names("agents")
            .iter()
            .any(|name| name == "assign_task"),
        "expected assign_task to be absent from agents namespace"
    );
    for tool_name in [
        "spawn_agent",
        "send_message",
        "followup_task",
        "wait_agent",
        "interrupt_agent",
        "list_agents",
    ] {
        assert!(
            plan.namespace_function_names("agents")
                .iter()
                .any(|name| name == tool_name),
            "expected {tool_name} in agents namespace"
        );
    }
}

#[tokio::test]
async fn hosted_web_search_and_standalone_image_generation_follow_runtime_gates() {
    let image_generation_tool = Arc::new(TestNamespaceExtensionTool {
        namespace: "image_gen",
        tool_name: "imagegen",
    });
    let image_generation = probe_with(
        |turn| {
            use_chatgpt_auth(turn);
            turn.model_info.input_modalities = vec![InputModality::Image];
        },
        ToolPlanInputs {
            extension_tool_executors: vec![image_generation_tool.clone()],
            ..Default::default()
        },
    )
    .await;
    image_generation.assert_visible_contains(&["image_gen"]);

    let extension_disabled = probe_with(
        |turn| {
            use_chatgpt_auth(turn);
            set_feature(turn, Feature::ImageGeneration, /*enabled*/ false);
            turn.model_info.input_modalities = vec![InputModality::Image];
        },
        ToolPlanInputs {
            extension_tool_executors: vec![image_generation_tool.clone()],
            ..Default::default()
        },
    )
    .await;
    extension_disabled.assert_visible_lacks(&["image_gen"]);

    let text_only_model = probe_with(
        |turn| {
            use_chatgpt_auth(turn);
            turn.model_info.input_modalities = vec![];
        },
        ToolPlanInputs {
            extension_tool_executors: vec![image_generation_tool.clone()],
            ..Default::default()
        },
    )
    .await;
    text_only_model.assert_visible_lacks(&["image_gen"]);

    let unsupported_provider = probe_with(
        |turn| {
            use_bedrock_provider(turn);
            turn.model_info.input_modalities = vec![InputModality::Image];
        },
        ToolPlanInputs {
            extension_tool_executors: vec![image_generation_tool],
            ..Default::default()
        },
    )
    .await;
    unsupported_provider.assert_visible_lacks(&["image_gen"]);

    let live_web_search = probe(|turn| {
        set_web_search_mode(turn, WebSearchMode::Live);
        turn.model_info.web_search_tool_type = WebSearchToolType::TextAndImage;
    })
    .await;
    assert_eq!(
        live_web_search.visible_spec("web_search"),
        &ToolSpec::WebSearch {
            external_web_access: Some(true),
            indexed_web_access: None,
            filters: None,
            user_location: None,
            search_context_size: None,
            search_content_types: Some(vec!["text".to_string(), "image".to_string()]),
        }
    );

    let code_mode_only = probe(|turn| {
        use_chatgpt_auth(turn);
        set_features(turn, &[Feature::CodeModeOnly, Feature::MultiAgentV2]);
        set_web_search_mode(turn, WebSearchMode::Live);
        turn.model_info.input_modalities = vec![InputModality::Image];
    })
    .await;
    assert_eq!(
        code_mode_only.visible_names,
        vec![
            // Code-mode entrypoints.
            codex_code_mode::PUBLIC_TOOL_NAME,
            codex_code_mode::WAIT_TOOL_NAME,
            "request_user_input",
            // Multi-agent v2 tools.
            MULTI_AGENT_V2_NAMESPACE,
            // Hosted Responses tools.
            "web_search",
        ]
    );

    let standalone_web_search_without_web_run = probe(|turn| {
        set_feature(turn, Feature::StandaloneWebSearch, /*enabled*/ true);
        set_web_search_mode(turn, WebSearchMode::Live);
    })
    .await;
    standalone_web_search_without_web_run.assert_visible_contains(&["web_search"]);

    let standalone_web_search_with_dynamic_web_run = probe_with(
        |turn| {
            set_feature(turn, Feature::StandaloneWebSearch, /*enabled*/ true);
            set_web_search_mode(turn, WebSearchMode::Live);
        },
        ToolPlanInputs {
            dynamic_tools: vec![dynamic_tool(
                Some("web"),
                "run",
                /*defer_loading*/ false,
            )],
            ..Default::default()
        },
    )
    .await;
    standalone_web_search_with_dynamic_web_run.assert_visible_contains(&["web", "web_search"]);

    let standalone_web_search = probe_with(
        |turn| {
            set_feature(turn, Feature::StandaloneWebSearch, /*enabled*/ true);
            set_web_search_mode(turn, WebSearchMode::Live);
        },
        ToolPlanInputs {
            extension_tool_executors: vec![Arc::new(TestNamespaceExtensionTool {
                namespace: "web",
                tool_name: "run",
            })],
            ..Default::default()
        },
    )
    .await;
    standalone_web_search.assert_visible_lacks(&["web_search"]);

    let bedrock_cached_web_search = probe(|turn| {
        use_bedrock_provider(turn);
        turn.model_info.web_search_tool_type = WebSearchToolType::Text;
    })
    .await;
    assert_eq!(
        bedrock_cached_web_search.visible_spec("web_search"),
        &ToolSpec::WebSearch {
            external_web_access: Some(false),
            indexed_web_access: None,
            filters: None,
            user_location: None,
            search_context_size: None,
            search_content_types: None,
        }
    );

    let bedrock_with_standalone_web_search = probe(|turn| {
        set_feature(turn, Feature::StandaloneWebSearch, /*enabled*/ true);
        set_web_search_mode(turn, WebSearchMode::Cached);
        use_bedrock_provider(turn);
        turn.model_info.web_search_tool_type = WebSearchToolType::Text;
    })
    .await;
    bedrock_with_standalone_web_search.assert_visible_contains(&["web_search"]);
    bedrock_with_standalone_web_search.assert_visible_lacks(&["web"]);
}
