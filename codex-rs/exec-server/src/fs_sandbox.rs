use std::collections::HashMap;

use codex_exec_server_protocol::JSONRPCErrorError;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::ExecServerRuntimePaths;
use crate::FileSystemSandboxContext;
use crate::fs_helper::CODEX_FS_HELPER_ARG1;
use crate::fs_helper::FsHelperPayload;
use crate::fs_helper::FsHelperRequest;
use crate::fs_helper::FsHelperResponse;
use crate::rpc::internal_error;

const FS_HELPER_ENV_ALLOWLIST: &[&str] = &["PATH", "TMPDIR", "TMP", "TEMP"];

#[derive(Clone, Debug)]
pub(crate) struct FileSystemSandboxRunner {
    runtime_paths: ExecServerRuntimePaths,
    helper_env: HashMap<String, String>,
}

impl FileSystemSandboxRunner {
    pub(crate) fn new(runtime_paths: ExecServerRuntimePaths) -> Self {
        Self {
            runtime_paths,
            helper_env: helper_env(),
        }
    }

    pub(crate) async fn run(
        &self,
        context: &FileSystemSandboxContext,
        request: FsHelperRequest,
    ) -> Result<FsHelperPayload, JSONRPCErrorError> {
        let cwd = match context.cwd.as_ref() {
            Some(cwd) => cwd.to_abs_path().map_err(io_error)?,
            None => std::env::current_dir()
                .map_err(io_error)?
                .try_into()
                .map_err(io_error)?,
        };
        let mut command = Command::new(self.runtime_paths.codex_self_exe.as_path());
        command.arg(CODEX_FS_HELPER_ARG1);
        command.current_dir(cwd.as_path());
        command.env_clear();
        command.envs(&self.helper_env);
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        command.kill_on_drop(true);

        let mut child = command.spawn().map_err(io_error)?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| internal_error("failed to open filesystem helper stdin".to_string()))?;
        let request_json = serde_json::to_vec(&request).map_err(json_error)?;
        stdin.write_all(&request_json).await.map_err(io_error)?;
        stdin.shutdown().await.map_err(io_error)?;
        drop(stdin);

        let output = child.wait_with_output().await.map_err(io_error)?;
        if !output.status.success() {
            return Err(internal_error(format!(
                "filesystem helper failed with status {status}: {stderr}",
                status = output.status,
                stderr = String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        match serde_json::from_slice(&output.stdout).map_err(json_error)? {
            FsHelperResponse::Ok(payload) => Ok(payload),
            FsHelperResponse::Error(error) => Err(error),
        }
    }
}

fn helper_env() -> HashMap<String, String> {
    std::env::vars()
        .filter(|(key, _)| FS_HELPER_ENV_ALLOWLIST.contains(&key.as_str()))
        .collect()
}

fn io_error(err: impl std::fmt::Display) -> JSONRPCErrorError {
    internal_error(err.to_string())
}

fn json_error(err: serde_json::Error) -> JSONRPCErrorError {
    internal_error(format!(
        "failed to encode or decode filesystem helper message: {err}"
    ))
}
