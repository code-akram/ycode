/*
Runtime: shell

Executes shell requests directly with full host access.
*/

use crate::exec::ExecCapturePolicy;
use crate::sandboxing::ExecRequest;
use crate::sandboxing::execute_env;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::runtimes::RuntimePathPrepends;
use crate::tools::runtimes::maybe_wrap_shell_lc_with_snapshot;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct ShellRequest {
    pub command: Vec<String>,
    pub turn_environment: TurnEnvironment,
    #[allow(dead_code)]
    // Retained compatibility, test, or architectural seam for non-default consumers.
    pub hook_command: String,
    pub cwd: AbsolutePathBuf,
    pub timeout_ms: Option<u64>,
    pub cancellation_token: CancellationToken,
    pub env: HashMap<String, String>,
    pub explicit_env_overrides: HashMap<String, String>,
}

pub struct ShellRuntime;

impl ShellRuntime {
    pub(crate) fn for_shell_command() -> Self {
        Self
    }

    fn stdout_stream(ctx: &ToolCtx) -> Option<crate::exec::StdoutStream> {
        Some(crate::exec::StdoutStream {
            sub_id: ctx.turn.sub_id.clone(),
            call_id: ctx.call_id.clone(),
            tx_event: ctx.session.get_tx_event(),
        })
    }
}

impl ToolRuntime<ShellRequest, ExecToolCallOutput> for ShellRuntime {
    async fn run(
        &mut self,
        req: &ShellRequest,
        ctx: &ToolCtx,
    ) -> Result<ExecToolCallOutput, ToolError> {
        let session_shell = ctx.session.user_shell();
        let shell = req
            .turn_environment
            .shell
            .as_ref()
            .unwrap_or(session_shell.as_ref());
        let shell_snapshot_location = req.turn_environment.shell_snapshot(&req.cwd);
        let env = req.env.clone();
        let explicit_env_overrides = req.explicit_env_overrides.clone();
        #[cfg(unix)]
        let (env, runtime_path_prepends) = {
            let mut env = env;
            let mut runtime_path_prepends = RuntimePathPrepends::default();
            crate::tools::runtimes::apply_package_path_prepend(
                &mut env,
                &mut runtime_path_prepends,
            );
            (env, runtime_path_prepends)
        };
        #[cfg(not(unix))]
        let runtime_path_prepends = RuntimePathPrepends::default();
        let command = maybe_wrap_shell_lc_with_snapshot(
            &req.command,
            shell,
            shell_snapshot_location.as_ref(),
            &explicit_env_overrides,
            &env,
            &runtime_path_prepends,
        );
        let mut expiration: crate::exec::ExecExpiration = req.timeout_ms.into();
        expiration = expiration.with_cancellation(req.cancellation_token.clone());
        let request = ExecRequest::new(
            command,
            req.cwd.clone().into(),
            env,
            expiration,
            ExecCapturePolicy::ShellTool,
            None,
        );
        let out = execute_env(request, Self::stdout_stream(ctx))
            .await
            .map_err(ToolError::Codex)?;
        Ok(out)
    }
}
