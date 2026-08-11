use super::*;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use codex_protocol::DEFAULT_FUNCTION_NAMESPACE;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

struct TestHandler {
    tool_name: codex_tools::ToolName,
}

impl ToolExecutor<ToolInvocation> for TestHandler {
    fn tool_name(&self) -> codex_tools::ToolName {
        self.tool_name.clone()
    }

    fn spec(&self) -> codex_tools::ToolSpec {
        test_spec(&self.tool_name)
    }

    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async {
            Ok(
                Box::new(crate::tools::context::FunctionToolOutput::from_text(
                    "ok".to_string(),
                    Some(true),
                )) as Box<dyn crate::tools::context::ToolOutput>,
            )
        })
    }
}

impl CoreToolRuntime for TestHandler {}

struct ReadinessTestHandler {
    handler: TestHandler,
    readiness_waits: Arc<AtomicUsize>,
}

impl ToolExecutor<ToolInvocation> for ReadinessTestHandler {
    fn tool_name(&self) -> codex_tools::ToolName {
        self.handler.tool_name()
    }

    fn spec(&self) -> codex_tools::ToolSpec {
        self.handler.spec()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        self.handler.handle(invocation)
    }
}

impl CoreToolRuntime for ReadinessTestHandler {
    fn wait_until_ready<'a>(&'a self, _session: &'a Arc<Session>) -> Option<BoxFuture<'a, ()>> {
        Some(Box::pin(async {
            self.readiness_waits.fetch_add(1, Ordering::Relaxed);
        }))
    }
}

#[derive(Clone)]
enum LifecycleTestResult {
    Ok { success: bool },
    Err,
}

struct LifecycleTestHandler {
    tool_name: codex_tools::ToolName,
    result: LifecycleTestResult,
}

impl ToolExecutor<ToolInvocation> for LifecycleTestHandler {
    fn tool_name(&self) -> codex_tools::ToolName {
        self.tool_name.clone()
    }

    fn spec(&self) -> codex_tools::ToolSpec {
        test_spec(&self.tool_name)
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        assert_eq!(
            invocation.tool_name,
            self.tool_name.clone().with_default_namespace()
        );
        Box::pin(self.handle_call())
    }
}

impl LifecycleTestHandler {
    async fn handle_call(
        &self,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        match self.result.clone() {
            LifecycleTestResult::Ok { success } => Ok(Box::new(
                crate::tools::context::FunctionToolOutput::from_text(
                    "ok".to_string(),
                    Some(success),
                ),
            )
                as Box<dyn crate::tools::context::ToolOutput>),
            LifecycleTestResult::Err => Err(FunctionCallError::RespondToModel(
                "handler failed".to_string(),
            )),
        }
    }
}

impl CoreToolRuntime for LifecycleTestHandler {}

fn test_spec(tool_name: &codex_tools::ToolName) -> codex_tools::ToolSpec {
    codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
        name: tool_name.name.clone(),
        description: "Test tool.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: codex_tools::JsonSchema::default(),
        output_schema: None,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum RecordedToolLifecycle {
    Start {
        call_id: String,
        tool_name: codex_tools::ToolName,
    },
    Finish {
        call_id: String,
        tool_name: codex_tools::ToolName,
        outcome: codex_extension_api::ToolCallOutcome,
    },
}

struct ToolLifecycleRecorder {
    records: Arc<std::sync::Mutex<Vec<RecordedToolLifecycle>>>,
}

impl codex_extension_api::ToolLifecycleContributor for ToolLifecycleRecorder {
    fn on_tool_start<'a>(
        &'a self,
        input: codex_extension_api::ToolStartInput<'a>,
    ) -> codex_extension_api::ToolLifecycleFuture<'a> {
        let records = Arc::clone(&self.records);
        let record = RecordedToolLifecycle::Start {
            call_id: input.call_id.to_string(),
            tool_name: input.tool_name.clone(),
        };
        Box::pin(async move {
            records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(record);
        })
    }

    fn on_tool_finish<'a>(
        &'a self,
        input: codex_extension_api::ToolFinishInput<'a>,
    ) -> codex_extension_api::ToolLifecycleFuture<'a> {
        let records = Arc::clone(&self.records);
        let record = RecordedToolLifecycle::Finish {
            call_id: input.call_id.to_string(),
            tool_name: input.tool_name.clone(),
            outcome: input.outcome,
        };
        Box::pin(async move {
            records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(record);
        })
    }
}

#[test]
fn handler_normalizes_only_the_default_namespace() {
    let namespace = "extension__mail";
    let tool_name = "gmail_get_recent_emails";
    let plain_name = codex_tools::ToolName::plain(tool_name);
    let namespaced_name = codex_tools::ToolName::namespaced(namespace, tool_name);
    let plain_handler = Arc::new(TestHandler {
        tool_name: plain_name.clone(),
    }) as Arc<dyn CoreToolRuntime>;
    let namespaced_handler = Arc::new(TestHandler {
        tool_name: namespaced_name.clone(),
    }) as Arc<dyn CoreToolRuntime>;
    let registry =
        ToolRegistry::from_tools([Arc::clone(&plain_handler), Arc::clone(&namespaced_handler)]);

    let plain = registry.tool(&plain_name);
    let default_namespaced = registry.tool(&codex_tools::ToolName::namespaced(
        DEFAULT_FUNCTION_NAMESPACE,
        tool_name,
    ));
    let empty_namespaced = registry.tool(&codex_tools::ToolName::namespaced("", tool_name));
    let namespaced = registry.tool(&namespaced_name);
    let missing_namespaced = registry.tool(&codex_tools::ToolName::namespaced(
        "extension__calendar",
        tool_name,
    ));

    assert_eq!(plain.is_some(), true);
    assert_eq!(namespaced.is_some(), true);
    assert_eq!(missing_namespaced.is_none(), true);
    assert!(
        plain
            .as_ref()
            .is_some_and(|handler| Arc::ptr_eq(handler, &plain_handler))
    );
    assert!(
        default_namespaced
            .as_ref()
            .is_some_and(|handler| Arc::ptr_eq(handler, &plain_handler))
    );
    assert!(
        empty_namespaced
            .as_ref()
            .is_some_and(|handler| Arc::ptr_eq(handler, &plain_handler))
    );
    assert!(
        namespaced
            .as_ref()
            .is_some_and(|handler| Arc::ptr_eq(handler, &namespaced_handler))
    );
}

#[test]
fn registry_rejects_default_namespace_alias_collisions() {
    let plain_name = codex_tools::ToolName::plain("lookup");
    let namespaced_name = codex_tools::ToolName::namespaced(DEFAULT_FUNCTION_NAMESPACE, "lookup");

    for [first_name, duplicate_name] in [
        [plain_name.clone(), namespaced_name.clone()],
        [namespaced_name, plain_name],
    ] {
        let winner = Arc::new(TestHandler {
            tool_name: first_name.clone(),
        }) as Arc<dyn CoreToolRuntime>;
        let mut registry = ToolRegistry::from_tools([Arc::clone(&winner)]);

        assert!(!registry.register_external(Arc::new(TestHandler {
            tool_name: duplicate_name.clone(),
        })));
        assert!(
            registry
                .tool(&duplicate_name)
                .is_some_and(|handler| Arc::ptr_eq(&handler, &winner))
        );
        assert_eq!(
            registry.tool_exposure(&duplicate_name),
            Some(ToolExposure::Direct)
        );
        assert_eq!(
            registry.supports_parallel_tool_calls(&duplicate_name),
            Some(false)
        );
        assert!(
            registry
                .remove(&duplicate_name)
                .is_some_and(|handler| Arc::ptr_eq(&handler, &winner))
        );
        assert!(registry.tool(&first_name).is_none());
    }
}

#[test]
fn registry_preserves_external_winners_and_trusted_synthetic_order() {
    let handler = |tool_name| Arc::new(TestHandler { tool_name }) as Arc<dyn CoreToolRuntime>;
    let [first_name, second_name, synthetic_name] =
        ["first", "second", "synthetic"].map(codex_tools::ToolName::plain);
    let first_handler = handler(first_name.clone());

    let mut registry = ToolRegistry::from_tools([Arc::clone(&first_handler)]);
    assert!(!registry.register_external(handler(first_name.clone())));
    let canonical_first_name = first_name.clone().with_default_namespace();
    assert_eq!(registry.first_collision(), Some(&canonical_first_name));
    assert!(registry.register_external(handler(second_name.clone())));
    registry.prepend_trusted(handler(synthetic_name.clone()));

    assert_eq!(
        registry
            .entries()
            .map(|tool| tool.runtime.tool_name())
            .collect::<Vec<_>>(),
        vec![synthetic_name, first_name.clone(), second_name],
    );
    assert!(
        registry
            .remove(&first_name)
            .is_some_and(|handler| Arc::ptr_eq(&handler, &first_handler))
    );
}

#[test]
fn reserved_shell_command_rejects_external_runtimes_without_a_builtin() {
    let handler = |tool_name| Arc::new(TestHandler { tool_name }) as Arc<dyn CoreToolRuntime>;
    let shell_command_name = codex_tools::ToolName::plain("shell_command");
    let namespaced_shell_command_name =
        codex_tools::ToolName::namespaced("client", "shell_command");
    let mut registry = ToolRegistry::default();

    assert!(!registry.register_external(handler(shell_command_name.clone())));
    assert!(!registry.register_external_with_exposure(
        handler(shell_command_name.clone()),
        ToolExposure::Direct,
    ));
    assert!(
        !registry.register_external(handler(codex_tools::ToolName::namespaced(
            DEFAULT_FUNCTION_NAMESPACE,
            "shell_command",
        )))
    );
    assert!(registry.tool(&shell_command_name).is_none());
    assert_eq!(registry.first_collision(), None);

    let namespaced_handler = handler(namespaced_shell_command_name.clone());
    assert!(registry.register_external(Arc::clone(&namespaced_handler)));
    assert!(
        registry
            .tool(&namespaced_shell_command_name)
            .is_some_and(|runtime| Arc::ptr_eq(&runtime, &namespaced_handler))
    );
}

#[test]
fn registry_records_reserved_shell_command_when_a_matching_tool_exists() {
    let tool_name = codex_tools::ToolName::plain("shell_command");
    let trusted = Arc::new(TestHandler {
        tool_name: tool_name.clone(),
    }) as Arc<dyn CoreToolRuntime>;
    let external = Arc::new(TestHandler {
        tool_name: tool_name.clone(),
    });
    let mut registry = ToolRegistry::from_tools([trusted]);

    assert!(!registry.register_external(external));
    let canonical_tool_name = tool_name.with_default_namespace();
    assert_eq!(registry.first_collision(), Some(&canonical_tool_name));
}

#[test]
fn registry_allows_identical_names_in_different_namespaces() {
    let handler = |tool_name| Arc::new(TestHandler { tool_name }) as Arc<dyn CoreToolRuntime>;
    let mut registry = ToolRegistry::from_tools([handler(codex_tools::ToolName::namespaced(
        "first", "lookup",
    ))]);

    assert!(
        registry.register_external(handler(codex_tools::ToolName::namespaced(
            "second", "lookup",
        )))
    );
    assert_eq!(registry.first_collision(), None);
}

#[tokio::test]
async fn readiness_selects_exact_tool_with_registry_owned_exposure() {
    let (session, _turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let plain_name = codex_tools::ToolName::plain("echo");
    let namespaced_name = codex_tools::ToolName::namespaced("extension__server", "echo");
    assert!(
        TestHandler {
            tool_name: plain_name.clone(),
        }
        .wait_until_ready(&session)
        .is_none()
    );
    let plain_readiness_waits = Arc::new(AtomicUsize::new(0));
    let namespaced_readiness_waits = Arc::new(AtomicUsize::new(0));
    let plain_handler = Arc::new(ReadinessTestHandler {
        handler: TestHandler {
            tool_name: plain_name.clone(),
        },
        readiness_waits: Arc::clone(&plain_readiness_waits),
    }) as Arc<dyn CoreToolRuntime>;
    let namespaced_handler = Arc::new(ReadinessTestHandler {
        handler: TestHandler {
            tool_name: namespaced_name.clone(),
        },
        readiness_waits: Arc::clone(&namespaced_readiness_waits),
    });
    let mut registry = ToolRegistry::from_tools([plain_handler]);
    registry.register_trusted_with_exposure(namespaced_handler, ToolExposure::DirectModelOnly);

    registry
        .tool(&plain_name)
        .expect("plain runtime should be registered")
        .wait_until_ready(&session)
        .expect("plain runtime should provide a readiness wait")
        .await;
    assert_eq!(
        [
            plain_readiness_waits.load(Ordering::Relaxed),
            namespaced_readiness_waits.load(Ordering::Relaxed),
        ],
        [1, 0]
    );

    registry
        .tool(&namespaced_name)
        .expect("namespaced runtime should be registered")
        .wait_until_ready(&session)
        .expect("namespaced runtime should forward its readiness wait")
        .await;
    assert_eq!(
        [
            plain_readiness_waits.load(Ordering::Relaxed),
            namespaced_readiness_waits.load(Ordering::Relaxed),
        ],
        [1, 1]
    );

    assert!(
        registry
            .tool(&codex_tools::ToolName::namespaced(
                "extension__missing",
                "echo"
            ))
            .is_none()
    );
    assert_eq!(
        [
            plain_readiness_waits.load(Ordering::Relaxed),
            namespaced_readiness_waits.load(Ordering::Relaxed),
        ],
        [1, 1]
    );
}

#[tokio::test]
async fn dispatch_uses_canonical_tool_names_for_lifecycle_contributors() -> anyhow::Result<()> {
    let (mut session, turn) = crate::session::tests::make_session_and_context().await;
    let records = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    builder.tool_lifecycle_contributor(Arc::new(ToolLifecycleRecorder {
        records: Arc::clone(&records),
    }));
    session.services.extensions = Arc::new(builder.build());

    let ok_tool = codex_tools::ToolName::plain("ok_tool");
    let failing_tool = codex_tools::ToolName::namespaced("extensions", "failing_tool");
    let ok_handler = Arc::new(LifecycleTestHandler {
        tool_name: ok_tool.clone(),
        result: LifecycleTestResult::Ok { success: false },
    }) as Arc<dyn CoreToolRuntime>;
    let failing_handler = Arc::new(LifecycleTestHandler {
        tool_name: failing_tool.clone(),
        result: LifecycleTestResult::Err,
    }) as Arc<dyn CoreToolRuntime>;
    let registry = ToolRegistry::from_tools([ok_handler, failing_handler]);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    registry
        .dispatch_any_with_terminal_outcome(
            test_invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                "ok-call",
                codex_tools::ToolName::namespaced(DEFAULT_FUNCTION_NAMESPACE, "ok_tool"),
            ),
            /*terminal_outcome_reached*/ None,
        )
        .await?;
    let err = match registry
        .dispatch_any_with_terminal_outcome(
            test_invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                "failing-call",
                failing_tool.clone(),
            ),
            /*terminal_outcome_reached*/ None,
        )
        .await
    {
        Ok(_) => panic!("failing handler should return an error"),
        Err(err) => err,
    };
    assert_eq!(err.to_string(), "handler failed");

    let expected = vec![
        RecordedToolLifecycle::Start {
            call_id: "ok-call".to_string(),
            tool_name: ok_tool.clone().with_default_namespace(),
        },
        RecordedToolLifecycle::Finish {
            call_id: "ok-call".to_string(),
            tool_name: ok_tool.with_default_namespace(),
            outcome: codex_extension_api::ToolCallOutcome::Completed { success: false },
        },
        RecordedToolLifecycle::Start {
            call_id: "failing-call".to_string(),
            tool_name: failing_tool.clone(),
        },
        RecordedToolLifecycle::Finish {
            call_id: "failing-call".to_string(),
            tool_name: failing_tool,
            outcome: codex_extension_api::ToolCallOutcome::Failed {
                handler_executed: true,
            },
        },
    ];
    let actual = records
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain(..)
        .collect::<Vec<_>>();
    assert_eq!(expected, actual);

    Ok(())
}

fn test_invocation(
    session: Arc<crate::session::session::Session>,
    turn: Arc<crate::session::turn_context::TurnContext>,
    call_id: &str,
    tool_name: codex_tools::ToolName,
) -> ToolInvocation {
    let step_context = StepContext::for_test(Arc::clone(&turn));
    ToolInvocation {
        session,
        step_context,
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: call_id.to_string(),
        tool_name,
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    }
}
