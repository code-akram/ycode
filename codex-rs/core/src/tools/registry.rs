use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::lifecycle::notify_tool_finish;
use crate::tools::lifecycle::notify_tool_start;
use crate::tools::tool_dispatch_trace::ToolDispatchTrace;
use crate::util::error_or_panic;
use codex_extension_api::ToolCallOutcome;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::EventMsg;
use codex_rollout::state_db;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use indexmap::IndexMap;
use indexmap::map::Entry;

pub use codex_tools::ToolExecutor;
pub use codex_tools::ToolExposure;

/// Typed runtime contract for locally executed tools.
///
/// Implementers provide the shared `ToolExecutor` behavior plus optional
/// core-owned metadata for telemetry, tool search, and argument diffs.
pub(crate) trait CoreToolRuntime: ToolExecutor<ToolInvocation> {
    /// Returns a readiness wait for this exact tool before taking the execution gate.
    fn wait_until_ready<'a>(&'a self, _session: &'a Arc<Session>) -> Option<BoxFuture<'a, ()>> {
        None
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(
            payload,
            ToolPayload::Function { .. } | ToolPayload::ToolSearch { .. }
        )
    }

    /// Whether cancellation should let the handler finish teardown before the
    /// host returns an aborted tool response.
    fn waits_for_runtime_cancellation(&self) -> bool {
        false
    }

    /// Creates an optional consumer for streamed tool argument diffs.
    fn create_diff_consumer(&self) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        None
    }
}

/// Consumes streamed argument diffs for a tool call and emits protocol events
/// derived from partial tool input.
pub(crate) trait ToolArgumentDiffConsumer: Send {
    /// Consume the next argument diff for a tool call.
    fn consume_diff(&mut self, turn: &TurnContext, call_id: String, diff: &str)
    -> Option<EventMsg>;

    /// Finish consuming argument diffs before the tool call completes.
    fn finish(&mut self) -> Result<Option<EventMsg>, FunctionCallError> {
        Ok(None)
    }
}

pub(crate) struct AnyToolResult {
    pub(crate) call_id: String,
    pub(crate) payload: ToolPayload,
    pub(crate) result: Box<dyn ToolOutput>,
}

impl AnyToolResult {
    pub(crate) fn into_response(self) -> ResponseInputItem {
        let Self {
            call_id,
            payload,
            result,
            ..
        } = self;
        result.to_response_item(&call_id, &payload)
    }

    pub(crate) fn code_mode_result(self) -> serde_json::Value {
        let Self {
            payload, result, ..
        } = self;
        result.code_mode_result(&payload)
    }
}

/// A tool runtime together with its effective exposure for the current step.
pub(crate) struct RegisteredTool {
    pub(crate) runtime: Arc<dyn CoreToolRuntime>,
    pub(crate) exposure: ToolExposure,
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: IndexMap<ToolName, RegisteredTool>,
    first_collision: Option<ToolName>,
}

impl ToolRegistry {
    #[cfg(test)]
    pub(crate) fn from_tools(tools: impl IntoIterator<Item = Arc<dyn CoreToolRuntime>>) -> Self {
        let mut registry = Self::default();

        for runtime in tools {
            registry.register_trusted(runtime);
        }

        registry
    }

    pub(crate) fn add<T>(&mut self, handler: T)
    where
        T: CoreToolRuntime + 'static,
    {
        self.register_trusted(Arc::new(handler));
    }

    pub(crate) fn add_with_exposure<T>(&mut self, handler: T, exposure: ToolExposure)
    where
        T: CoreToolRuntime + 'static,
    {
        self.register_trusted_with_exposure(Arc::new(handler), exposure);
    }

    pub(crate) fn register_trusted(&mut self, runtime: Arc<dyn CoreToolRuntime>) {
        let exposure = runtime.exposure();
        self.register_trusted_with_exposure(runtime, exposure);
    }

    pub(crate) fn register_trusted_with_exposure(
        &mut self,
        runtime: Arc<dyn CoreToolRuntime>,
        exposure: ToolExposure,
    ) {
        let tool_name = runtime.tool_name().with_default_namespace();
        match self.tools.entry(tool_name) {
            Entry::Vacant(entry) => {
                entry.insert(RegisteredTool { runtime, exposure });
            }
            Entry::Occupied(entry) => {
                let tool_name = entry.key();
                error_or_panic(format!("tool {tool_name} already registered"));
            }
        }
    }

    pub(crate) fn prepend_trusted(&mut self, runtime: Arc<dyn CoreToolRuntime>) {
        let tool_name = runtime.tool_name().with_default_namespace();
        if self.tools.contains_key(&tool_name) {
            error_or_panic(format!("tool {tool_name} already registered"));
            return;
        }

        let exposure = runtime.exposure();
        self.tools
            .shift_insert(0, tool_name, RegisteredTool { runtime, exposure });
    }

    pub(crate) fn register_external(&mut self, runtime: Arc<dyn CoreToolRuntime>) -> bool {
        let exposure = runtime.exposure();
        self.register_external_with_exposure(runtime, exposure)
    }

    pub(crate) fn register_external_with_exposure(
        &mut self,
        runtime: Arc<dyn CoreToolRuntime>,
        exposure: ToolExposure,
    ) -> bool {
        let tool_name = runtime.tool_name().with_default_namespace();
        if tool_name.is_default_namespace() && tool_name.name == "shell_command" {
            tracing::warn!(tool_name = %tool_name, "skipping external tool with reserved name");
            if self.tools.contains_key(&tool_name) {
                self.record_collision(tool_name);
            }
            return false;
        }

        match self.tools.entry(tool_name) {
            Entry::Vacant(entry) => {
                entry.insert(RegisteredTool { runtime, exposure });
                true
            }
            Entry::Occupied(entry) => {
                tracing::warn!(
                    tool_name = %entry.key(),
                    "skipping duplicate external tool that is already registered"
                );
                self.first_collision
                    .get_or_insert_with(|| entry.key().clone());
                false
            }
        }
    }

    pub(crate) fn record_collision(&mut self, tool_name: ToolName) {
        self.first_collision.get_or_insert(tool_name);
    }

    pub(crate) fn first_collision(&self) -> Option<&ToolName> {
        self.first_collision.as_ref()
    }

    pub(crate) fn remove(&mut self, tool_name: &ToolName) -> Option<Arc<dyn CoreToolRuntime>> {
        self.tools
            .shift_remove(&tool_name.clone().with_default_namespace())
            .map(|tool| tool.runtime)
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &RegisteredTool> {
        self.tools.values()
    }

    pub(crate) fn entries_mut(&mut self) -> impl Iterator<Item = &mut RegisteredTool> {
        self.tools.values_mut()
    }

    pub(crate) fn deferred_tool_namespaces(&self) -> BTreeMap<String, String> {
        let mut namespaces = BTreeMap::<String, String>::new();
        for (name, tool) in &self.tools {
            if !tool.exposure.is_deferred() || name.is_default_namespace() {
                continue;
            }
            let Some(namespace) = &name.namespace else {
                continue;
            };
            let existing_description = namespaces.entry(namespace.clone()).or_default();
            if !existing_description.trim().is_empty() {
                continue;
            }
            let description = match tool.runtime.spec() {
                ToolSpec::Namespace(namespace) => namespace.description,
                ToolSpec::Function(_)
                | ToolSpec::Freeform(_)
                | ToolSpec::ToolSearch { .. }
                | ToolSpec::WebSearch { .. } => String::new(),
            };
            if !description.trim().is_empty() {
                *existing_description = description;
            }
        }
        namespaces
    }

    #[cfg(test)]
    pub(crate) fn empty_for_test() -> Self {
        Self::from_tools(std::iter::empty())
    }

    #[cfg(test)]
    pub(crate) fn with_handler_for_test<T>(handler: Arc<T>) -> Self
    where
        T: CoreToolRuntime + 'static,
    {
        Self::from_tools([handler as Arc<dyn CoreToolRuntime>])
    }

    pub(crate) fn tool(&self, name: &ToolName) -> Option<Arc<dyn CoreToolRuntime>> {
        self.tools
            .get(&name.clone().with_default_namespace())
            .map(|tool| Arc::clone(&tool.runtime))
    }

    #[cfg(test)]
    pub(crate) fn tool_names_for_test(&self) -> Vec<ToolName> {
        let mut names = self.tools.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
    }

    #[cfg(test)]
    pub(crate) fn tool_exposure(&self, name: &ToolName) -> Option<ToolExposure> {
        self.tools
            .get(&name.clone().with_default_namespace())
            .map(|tool| tool.exposure)
    }

    pub(crate) fn create_diff_consumer(
        &self,
        name: &ToolName,
    ) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        self.tool(name)?.create_diff_consumer()
    }

    pub(crate) fn supports_parallel_tool_calls(&self, name: &ToolName) -> Option<bool> {
        let tool = self.tools.get(&name.clone().with_default_namespace())?;
        Some(tool.exposure != ToolExposure::Hidden && tool.runtime.supports_parallel_tool_calls())
    }

    pub(crate) fn waits_for_runtime_cancellation(&self, name: &ToolName) -> Option<bool> {
        let tool = self.tool(name)?;
        Some(tool.waits_for_runtime_cancellation())
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "tool dispatch must keep active-turn accounting atomic"
    )]
    pub(crate) async fn dispatch_any_with_terminal_outcome(
        &self,
        invocation: ToolInvocation,
        terminal_outcome_reached: Option<Arc<AtomicBool>>,
    ) -> Result<AnyToolResult, FunctionCallError> {
        let tool_name = invocation.tool_name.clone();
        {
            let mut active = invocation.session.active_turn.lock().await;
            if let Some(active_turn) = active.as_mut() {
                let mut turn_state = active_turn.turn_state.lock().await;
                turn_state.tool_calls = turn_state.tool_calls.saturating_add(1);
            }
        }

        let dispatch_trace = ToolDispatchTrace::start(&invocation);
        let tool = match self.tool(&tool_name) {
            Some(tool) => tool,
            None => {
                let message = unsupported_tool_call_message(&invocation.payload, &tool_name);
                let err = FunctionCallError::RespondToModel(message);
                dispatch_trace.record_failed(&err);
                return Err(err);
            }
        };
        if !tool.matches_kind(&invocation.payload) {
            let message = format!("tool {tool_name} invoked with incompatible payload");
            let err = FunctionCallError::Fatal(message);
            dispatch_trace.record_failed(&err);
            return Err(err);
        }

        notify_tool_start(&invocation).await;

        let invocation_for_tool = invocation.clone();
        let result = handle_any_tool(tool.as_ref(), invocation_for_tool).await;
        let lifecycle_outcome = match &result {
            Ok(result) => ToolCallOutcome::Completed {
                success: result.result.success_for_logging(),
            },
            Err(_) => ToolCallOutcome::Failed {
                handler_executed: true,
            },
        };
        notify_tool_finish_if_unclaimed(
            &invocation,
            terminal_outcome_reached.as_deref(),
            lifecycle_outcome,
        )
        .await;

        match result {
            Ok(result) => {
                dispatch_trace.record_completed(
                    &invocation,
                    &result.call_id,
                    &result.payload,
                    result.result.as_ref(),
                );
                Ok(result)
            }
            Err(err) => {
                dispatch_trace.record_failed(&err);
                Err(err)
            }
        }
    }
}

async fn notify_tool_finish_if_unclaimed(
    invocation: &ToolInvocation,
    terminal_outcome_reached: Option<&AtomicBool>,
    outcome: ToolCallOutcome,
) -> bool {
    if terminal_outcome_reached.is_some_and(|reached| reached.swap(true, Ordering::AcqRel)) {
        return false;
    }

    notify_tool_finish(invocation, outcome).await;
    true
}

async fn handle_any_tool(
    tool: &dyn CoreToolRuntime,
    invocation: ToolInvocation,
) -> Result<AnyToolResult, FunctionCallError> {
    let call_id = invocation.call_id.clone();
    let payload = invocation.payload.clone();
    let output = tool.handle(invocation.clone()).await?;
    if output.contains_external_context()
        && invocation.turn.config.memories.disable_on_external_context
    {
        state_db::mark_thread_memory_mode_polluted(
            invocation.session.services.state_db.as_deref(),
            invocation.session.thread_id,
            "tool_output",
        )
        .await;
    }
    Ok(AnyToolResult {
        call_id,
        payload,
        result: output,
    })
}

fn unsupported_tool_call_message(payload: &ToolPayload, tool_name: &ToolName) -> String {
    match payload {
        ToolPayload::Custom { .. } => format!("unsupported custom tool call: {tool_name}"),
        _ => format!("unsupported call: {tool_name}"),
    }
}
#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
