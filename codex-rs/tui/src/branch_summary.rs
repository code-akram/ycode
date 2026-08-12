//! Best-effort branch lookup used to keep the active thread's branch metadata current.

use std::path::Path;

use crate::workspace_command::WorkspaceCommand;
use crate::workspace_command::WorkspaceCommandExecutor;

/// Returns the checked-out branch name for one working directory.
///
/// Detached HEADs, non-git directories, and command failures return `None` so branch metadata
/// synchronization never interrupts the interactive session.
pub(crate) async fn current_branch_name(
    runner: &dyn WorkspaceCommandExecutor,
    cwd: &Path,
) -> Option<String> {
    let output = runner
        .run(
            WorkspaceCommand::new(vec![
                "git".to_string(),
                "-c".to_string(),
                codex_git_utils::SAFE_BARE_REPOSITORY_CONFIG.to_string(),
                "branch".to_string(),
                "--show-current".to_string(),
            ])
            .cwd(cwd.to_path_buf())
            .env("GIT_OPTIONAL_LOCKS", "0"),
        )
        .await
        .ok()?;
    if !output.success() {
        return None;
    }

    Some(output.stdout.trim().to_string()).filter(|name| !name.is_empty())
}
