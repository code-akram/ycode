use codex_protocol::models::ShellCommandToolCallParams;
use codex_tools::ShellCommandBackendConfig;
use codex_tools::ToolName;
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::exec::ExecCapturePolicy;
use crate::exec::ExecParams;
use crate::exec_env::create_env;
use crate::exec_env::inject_permission_profile_env;
use crate::function_tool::FunctionCallError;
use crate::maybe_emit_implicit_skill_invocation;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::Shell;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments_with_base_path;
use crate::tools::handlers::resolve_workdir_base_path;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolSpec;

use super::super::shell_spec::CommandToolOptions;
use super::super::shell_spec::create_shell_command_tool;
use super::RunExecLikeArgs;
use super::run_exec_like;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShellCommandHandler {
    options: ShellCommandHandlerOptions,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShellCommandHandlerOptions {
    pub(crate) backend_config: ShellCommandBackendConfig,
    pub(crate) allow_login_shell: bool,
}

impl ShellCommandHandler {
    pub(crate) fn new(options: ShellCommandHandlerOptions) -> Self {
        Self { options }
    }

    pub(super) fn resolve_use_login_shell(
        login: Option<bool>,
        allow_login_shell: bool,
    ) -> Result<bool, FunctionCallError> {
        if !allow_login_shell && login == Some(true) {
            return Err(FunctionCallError::RespondToModel(
                "login shell is disabled by config; omit `login` or set it to false.".to_string(),
            ));
        }

        Ok(login.unwrap_or(allow_login_shell))
    }

    pub(super) fn base_command(shell: &Shell, command: &str, use_login_shell: bool) -> Vec<String> {
        shell.derive_exec_args(command, use_login_shell)
    }

    pub(super) fn to_exec_params(
        params: &ShellCommandToolCallParams,
        session: &crate::session::session::Session,
        turn_context: &TurnContext,
        turn_environment: &TurnEnvironment,
        cwd: AbsolutePathBuf,
    ) -> Result<ExecParams, FunctionCallError> {
        let session_shell = session.user_shell();
        let shell = turn_environment
            .shell
            .as_ref()
            .unwrap_or(session_shell.as_ref());
        let use_login_shell =
            Self::resolve_use_login_shell(params.login, turn_environment.config.allow_login_shell)?;
        let command = Self::base_command(shell, &params.command, use_login_shell);

        let mut env = create_env(
            &turn_context.config.permissions.shell_environment_policy,
            Some(session.thread_id),
        );
        let active_permission_profile = turn_environment.active_permission_profile();
        inject_permission_profile_env(&mut env, active_permission_profile.as_ref());
        Ok(ExecParams {
            command,
            cwd,
            expiration: params.timeout_ms.into(),
            capture_policy: ExecCapturePolicy::ShellTool,
            env,
            arg0: None,
        })
    }
}

impl From<ShellCommandBackendConfig> for ShellCommandHandler {
    fn from(backend_config: ShellCommandBackendConfig) -> Self {
        Self::new(ShellCommandHandlerOptions {
            backend_config,
            allow_login_shell: false,
        })
    }
}

impl ToolExecutor<ToolInvocation> for ShellCommandHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("shell_command")
    }

    fn spec(&self) -> ToolSpec {
        create_shell_command_tool(CommandToolOptions {
            allow_login_shell: self.options.allow_login_shell,
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl ShellCommandHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            step_context,
            cancellation_token,
            tracker,
            call_id,
            payload,
            ..
        } = invocation;

        let tool_name = self.tool_name();
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(format!(
                "unsupported payload for shell_command handler: {tool_name}"
            )));
        };

        let Some(turn_environment) = step_context.environments.primary().cloned() else {
            return Err(FunctionCallError::RespondToModel(
                "shell is unavailable in this session".to_string(),
            ));
        };

        let environment_cwd = turn_environment.cwd().to_abs_path().map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "shell_command cwd `{}` is not native to the Codex host: {err}",
                turn_environment.cwd()
            ))
        })?;
        let cwd = resolve_workdir_base_path(&arguments, &environment_cwd)?;
        let params: ShellCommandToolCallParams = parse_arguments_with_base_path(&arguments, &cwd)?;
        maybe_emit_implicit_skill_invocation(
            session.as_ref(),
            turn.as_ref(),
            &params.command,
            &cwd,
        )
        .await;
        let exec_params = Self::to_exec_params(
            &params,
            session.as_ref(),
            turn.as_ref(),
            &turn_environment,
            cwd,
        )?;
        run_exec_like(RunExecLikeArgs {
            tool_name,
            exec_params,
            cancellation_token,
            hook_command: params.command,
            session,
            turn,
            turn_environment,
            tracker,
            call_id,
        })
        .await
        .map(boxed_tool_output)
    }
}

impl CoreToolRuntime for ShellCommandHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }
}
