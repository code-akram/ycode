//! Core-owned process execution request types.

use crate::exec::ExecCapturePolicy;
use crate::exec::ExecExpiration;
use crate::exec::StdoutStream;
use crate::exec::execute_exec_request;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;

#[derive(Debug)]
#[allow(dead_code)] // Retained compatibility, test, or architectural seam for non-default consumers.
pub(crate) struct ExecOptions {
    pub(crate) expiration: ExecExpiration,
    pub(crate) capture_policy: ExecCapturePolicy,
}

#[derive(Clone, Debug)]
pub(crate) struct ExecServerEnvConfig {
    pub(crate) policy: codex_exec_server::ExecEnvPolicy,
    pub(crate) local_policy_env: HashMap<String, String>,
}

#[derive(Debug)]
pub struct ExecRequest {
    pub command: Vec<String>,
    pub cwd: PathUri,
    pub env: HashMap<String, String>,
    pub(crate) exec_server_env_config: Option<ExecServerEnvConfig>,
    pub expiration: ExecExpiration,
    pub capture_policy: ExecCapturePolicy,
    pub arg0: Option<String>,
}

impl ExecRequest {
    pub fn new(
        command: Vec<String>,
        cwd: PathUri,
        env: HashMap<String, String>,
        expiration: ExecExpiration,
        capture_policy: ExecCapturePolicy,
        arg0: Option<String>,
    ) -> Self {
        Self {
            command,
            cwd,
            env,
            exec_server_env_config: None,
            expiration,
            capture_policy,
            arg0,
        }
    }
}

pub async fn execute_env(
    exec_request: ExecRequest,
    stdout_stream: Option<StdoutStream>,
) -> codex_protocol::error::Result<ExecToolCallOutput> {
    execute_exec_request(exec_request, stdout_stream, /*after_spawn*/ None).await
}

pub async fn execute_exec_request_with_after_spawn(
    exec_request: ExecRequest,
    stdout_stream: Option<StdoutStream>,
    after_spawn: Option<Box<dyn FnOnce() + Send>>,
) -> codex_protocol::error::Result<ExecToolCallOutput> {
    execute_exec_request(exec_request, stdout_stream, after_spawn).await
}
