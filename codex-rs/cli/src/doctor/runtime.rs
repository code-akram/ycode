//! Captures how this Codex process was launched.
//!
//! Runtime diagnostics answer which binary is running, which platform it
//! targets, and whether search comes from bundled package files or from PATH.

use std::env;
use std::process::Command;

use codex_install_context::InstallContext;

use super::CheckStatus;
use super::DoctorCheck;
use super::push_path_detail;

/// Builds the process provenance row for the current Codex executable.
///
/// This check is informational and should not fail on its own.
pub(super) fn runtime_check() -> DoctorCheck {
    let current_exe = env::current_exe().ok();
    let os = env::consts::OS;
    let arch = env::consts::ARCH;
    let platform = format!("{os}-{arch}");
    let mut details = vec![
        format!("version: {}", env!("CARGO_PKG_VERSION")),
        format!("platform: {platform}"),
        format!("commit: {}", build_commit()),
    ];
    push_path_detail(&mut details, "current executable", current_exe.as_deref());

    DoctorCheck::new(
        "runtime.provenance",
        "runtime",
        CheckStatus::Ok,
        format!("running on {platform}"),
    )
    .details(details)
}

/// Verifies that the search command selected by the install context is usable.
///
/// Package-layout installs should point at a bundled ripgrep binary, while local
/// installs without that layout usually resolve rg from PATH. A warning here
/// means features that depend on file search may degrade even when the CLI
/// launches.
pub(super) fn search_check() -> DoctorCheck {
    let install_context = InstallContext::current();
    let rg_command = install_context.rg_command();
    let provider = search_provider(&install_context);
    let mut details = vec![
        format!("search command: {}", rg_command.display()),
        format!("search provider: {provider}"),
    ];

    let status = if rg_command.components().count() > 1 {
        match std::fs::metadata(&rg_command) {
            Ok(metadata) if metadata.is_file() => {
                details.push("search command readiness: file exists".to_string());
                CheckStatus::Ok
            }
            Ok(_) => {
                details.push("search command readiness: path is not a file".to_string());
                CheckStatus::Warning
            }
            Err(err) => {
                details.push(format!("search command readiness: {err}"));
                CheckStatus::Warning
            }
        }
    } else {
        match Command::new(&rg_command).arg("--version").output() {
            Ok(output) if output.status.success() => {
                let version = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("rg version unknown")
                    .to_string();
                details.push(format!("search command readiness: {version}"));
                CheckStatus::Ok
            }
            Ok(output) => {
                details.push(format!(
                    "search command readiness: exited with status {}",
                    output.status
                ));
                CheckStatus::Warning
            }
            Err(err) => {
                details.push(format!("search command readiness: {err}"));
                CheckStatus::Warning
            }
        }
    };

    let summary = match status {
        CheckStatus::Ok => format!("search is OK ({provider})"),
        CheckStatus::Warning => "search command could not be verified".to_string(),
        CheckStatus::Fail => unreachable!(),
    };
    let mut check = DoctorCheck::new("runtime.search", "search", status, summary).details(details);
    if status != CheckStatus::Ok {
        check = check.remediation("Install ripgrep or repair the bundled Codex package.");
    }
    check
}

fn search_provider(context: &InstallContext) -> &'static str {
    let rg_command = context.rg_command();
    let from_package_layout = context
        .package_layout
        .as_ref()
        .and_then(|package_layout| package_layout.path_dir.as_ref())
        .is_some_and(|path_dir| rg_command.starts_with(path_dir));
    if from_package_layout {
        "bundled"
    } else {
        "system"
    }
}

fn build_commit() -> &'static str {
    option_env!("CODEX_BUILD_COMMIT")
        .or(option_env!("GIT_COMMIT"))
        .unwrap_or("unknown")
}
