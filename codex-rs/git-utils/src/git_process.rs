use std::process::Output;
use std::process::Stdio;
use std::time::Duration;

use codex_utils_pty::process_group::kill_process_group;
use tokio::process::Child;
use tokio::process::Command;
use tokio::time::timeout;

struct KillGitProcessTreeOnDrop {
    process_id: u32,
    armed: bool,
}

impl Drop for KillGitProcessTreeOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = kill_process_group(self.process_id);
        }
    }
}

fn spawn_git_command(command: &mut Command) -> Option<(Child, KillGitProcessTreeOnDrop)> {
    command.process_group(0);
    command.kill_on_drop(true);

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = command.spawn().ok()?;

    let process_tree = KillGitProcessTreeOnDrop {
        process_id: child.id()?,
        armed: true,
    };

    Some((child, process_tree))
}

async fn wait_for_git_command_with_timeout_output(
    child: Child,
    process_tree: KillGitProcessTreeOnDrop,
    timeout_duration: Duration,
) -> Option<Output> {
    let mut process_tree = process_tree;

    let result = timeout(timeout_duration, child.wait_with_output()).await;

    match result {
        Ok(Ok(output)) => {
            process_tree.armed = false;
            Some(output)
        }
        _ => None,
    }
}

pub(crate) async fn run_git_command_with_timeout_output(
    command: &mut Command,
    timeout_duration: Duration,
) -> Option<Output> {
    let (child, process_tree) = spawn_git_command(command)?;
    wait_for_git_command_with_timeout_output(child, process_tree, timeout_duration).await
}

#[cfg(test)]
#[path = "git_process_tests.rs"]
mod tests;
