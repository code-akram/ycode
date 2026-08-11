use std::collections::HashMap;

use codex_exec_server_protocol::JSONRPCErrorError;
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::ExecServerRuntimePaths;
use crate::protocol::ExecParams;
use crate::rpc::invalid_params;

pub(crate) struct PreparedExecRequest {
    pub(crate) command: Vec<String>,
    pub(crate) cwd: AbsolutePathBuf,
    pub(crate) env: HashMap<String, String>,
    pub(crate) arg0: Option<String>,
}

pub(crate) async fn prepare_exec_request(
    params: &ExecParams,
    env: HashMap<String, String>,
    _runtime_paths: Option<&ExecServerRuntimePaths>,
) -> Result<PreparedExecRequest, JSONRPCErrorError> {
    Ok(PreparedExecRequest {
        command: params.argv.clone(),
        cwd: params
            .cwd
            .to_abs_path()
            .map_err(|err| invalid_params(format!("cwd is not native to this executor: {err}")))?,
        env,
        arg0: params.arg0.clone(),
    })
}
