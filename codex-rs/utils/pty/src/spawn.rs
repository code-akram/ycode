use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;

use crate::SpawnedProcess;
use crate::TerminalSize;

/// Process launch request shared by local and exec-server execution.
pub struct SpawnRequest<'a> {
    pub command: &'a [String],
    pub cwd: &'a Path,
    pub env: &'a HashMap<String, String>,
    pub arg0: &'a Option<String>,
    pub tty: bool,
    pub stdin_open: bool,
    pub inherited_fds: &'a [i32],
}

pub async fn spawn_process(request: SpawnRequest<'_>) -> Result<SpawnedProcess> {
    let (program, args) = request
        .command
        .split_first()
        .context("missing program for process spawn")?;
    if request.tty {
        crate::pty::spawn_process(
            program,
            args,
            request.cwd,
            request.env,
            request.arg0,
            TerminalSize::default(),
            request.inherited_fds,
        )
        .await
    } else if request.stdin_open {
        crate::pipe::spawn_process(
            program,
            args,
            request.cwd,
            request.env,
            request.arg0,
            request.inherited_fds,
        )
        .await
    } else {
        crate::pipe::spawn_process_no_stdin(
            program,
            args,
            request.cwd,
            request.env,
            request.arg0,
            request.inherited_fds,
        )
        .await
    }
}
