use crate::CliRuntimeTarget;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteConnectionStatus {
    pub(crate) address: String,
    pub(crate) version: String,
}

pub(crate) fn remote_connection_status_value(
    _cli_runtime_target: &CliRuntimeTarget,
    _server_version: Option<&str>,
) -> Option<RemoteConnectionStatus> {
    None
}
