//! Long-running shell execution with PTY and stdin support.

use crate::exec::ExecCapturePolicy;
use crate::exec::ExecExpiration;
use crate::sandboxing::ExecRequest;
use crate::sandboxing::ExecServerEnvConfig;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::runtimes::RuntimePathPrepends;
use crate::tools::runtimes::maybe_wrap_shell_lc_with_snapshot;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::unified_exec::NoopSpawnLifecycle;
use crate::unified_exec::UnifiedExecProcess;
use crate::unified_exec::UnifiedExecProcessManager;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct UnifiedExecRequest {
    pub command: Vec<String>,
    pub process_id: i32,
    pub cwd: PathUri,
    pub turn_environment: TurnEnvironment,
    pub env: HashMap<String, String>,
    pub exec_server_env_config: Option<ExecServerEnvConfig>,
    pub explicit_env_overrides: HashMap<String, String>,
    pub tty: bool,
}

pub struct UnifiedExecRuntime<'a> {
    manager: &'a UnifiedExecProcessManager,
}

impl<'a> UnifiedExecRuntime<'a> {
    pub fn new(manager: &'a UnifiedExecProcessManager) -> Self {
        Self { manager }
    }
}

impl ToolRuntime<UnifiedExecRequest, UnifiedExecProcess> for UnifiedExecRuntime<'_> {
    async fn run(
        &mut self,
        req: &UnifiedExecRequest,
        ctx: &ToolCtx,
    ) -> Result<UnifiedExecProcess, ToolError> {
        if req.command.is_empty() {
            return Err(ToolError::Rejected(
                "missing command line for PTY".to_string(),
            ));
        }

        let session_shell = ctx.session.user_shell();
        let shell = req
            .turn_environment
            .shell
            .as_ref()
            .unwrap_or(session_shell.as_ref());
        let environment_is_remote = req.turn_environment.environment.is_remote();
        let shell_snapshot_location = if environment_is_remote {
            None
        } else {
            let native_cwd = req
                .cwd
                .to_abs_path()
                .map_err(|err| ToolError::Rejected(err.to_string()))?;
            req.turn_environment.shell_snapshot(&native_cwd)
        };
        let mut env = req.env.clone();
        let runtime_path_prepends = if environment_is_remote {
            RuntimePathPrepends::default()
        } else {
            let mut prepends = RuntimePathPrepends::default();
            crate::tools::runtimes::apply_package_path_prepend(&mut env, &mut prepends);
            prepends
        };
        let command = if environment_is_remote {
            req.command.clone()
        } else {
            maybe_wrap_shell_lc_with_snapshot(
                &req.command,
                shell,
                shell_snapshot_location.as_ref(),
                &req.explicit_env_overrides,
                &env,
                &runtime_path_prepends,
            )
        };
        let mut request = ExecRequest::new(
            command,
            req.cwd.clone(),
            env,
            ExecExpiration::DefaultTimeout,
            ExecCapturePolicy::ShellTool,
            None,
        );
        request.exec_server_env_config = req.exec_server_env_config.clone();
        self.manager
            .open_session_with_prepared_exec_env(
                req.process_id,
                &request,
                req.tty,
                Box::new(NoopSpawnLifecycle),
                req.turn_environment.environment.as_ref(),
            )
            .await
            .map_err(|err| ToolError::Rejected(err.to_string()))
    }
}
