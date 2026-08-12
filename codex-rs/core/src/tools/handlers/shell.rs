use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::exec::ExecParams;
use crate::function_tool::FunctionCallError;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::FunctionToolOutput;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::apply_patch::intercept_apply_patch;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::runtimes::shell::ShellRequest;
use crate::tools::runtimes::shell::ShellRuntime;
use crate::tools::sandboxing::ToolCtx;
use codex_protocol::protocol::ExecCommandSource;
use codex_tools::ToolName;
use codex_utils_path_uri::PathUri;

mod shell_command;

pub use shell_command::ShellCommandHandler;
pub(crate) use shell_command::ShellCommandHandlerOptions;

struct RunExecLikeArgs {
    tool_name: ToolName,
    exec_params: ExecParams,
    cancellation_token: CancellationToken,
    hook_command: String,
    session: Arc<crate::session::session::Session>,
    turn: Arc<TurnContext>,
    turn_environment: TurnEnvironment,
    tracker: crate::tools::context::SharedTurnDiffTracker,
    call_id: String,
}

async fn run_exec_like(args: RunExecLikeArgs) -> Result<FunctionToolOutput, FunctionCallError> {
    let RunExecLikeArgs {
        tool_name,
        exec_params,
        cancellation_token,
        hook_command,
        session,
        turn,
        turn_environment,
        tracker,
        call_id,
    } = args;

    let fs = turn_environment.environment.get_filesystem();

    let explicit_env_overrides = turn
        .config
        .permissions
        .shell_environment_policy
        .r#set
        .clone();
    // Intercept apply_patch if present.
    let apply_patch_cwd = PathUri::from_abs_path(&exec_params.cwd);
    if let Some(output) = intercept_apply_patch(
        &exec_params.command,
        &apply_patch_cwd,
        fs.as_ref(),
        turn_environment.clone(),
        session.clone(),
        turn.clone(),
        Some(&tracker),
        &call_id,
        tool_name.name.as_str(),
    )
    .await?
    {
        return Ok(output);
    }

    let source = ExecCommandSource::Agent;
    let emitter = ToolEmitter::shell(exec_params.command.clone(), exec_params.cwd.clone(), source);
    let event_ctx = ToolEventCtx::new(
        session.as_ref(),
        turn.as_ref(),
        &call_id,
        /*turn_diff_tracker*/ None,
    );
    emitter.begin(event_ctx).await;

    let req = ShellRequest {
        command: exec_params.command.clone(),
        turn_environment: turn_environment.clone(),
        hook_command,
        cwd: exec_params.cwd.clone(),
        timeout_ms: exec_params.expiration.timeout_ms(),
        cancellation_token,
        env: exec_params.env.clone(),
        explicit_env_overrides,
    };
    let mut orchestrator = ToolOrchestrator::new();
    let mut runtime = ShellRuntime::for_shell_command();
    let tool_ctx = ToolCtx {
        session: session.clone(),
        turn: turn.clone(),
        call_id: call_id.clone(),
        tool_name,
    };
    let out = orchestrator.run(&mut runtime, &req, &tool_ctx).await;
    let event_ctx = ToolEventCtx::new(
        session.as_ref(),
        turn.as_ref(),
        &call_id,
        /*turn_diff_tracker*/ None,
    );
    let content = emitter
        .finish(event_ctx, out, /*applied_patch_delta*/ None)
        .await?;
    Ok(FunctionToolOutput {
        body: vec![
            codex_protocol::models::FunctionCallOutputContentItem::InputText { text: content },
        ],
        success: Some(true),
    })
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
