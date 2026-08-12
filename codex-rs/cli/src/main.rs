use clap::Args;
use clap::CommandFactory;
use clap::Parser;
use clap_complete::Shell;
use clap_complete::generate;
use codex_arg0::Arg0DispatchPaths;
use codex_arg0::arg0_dispatch_or_else;
use codex_chatgpt::apply_command::ApplyCommand;
use codex_chatgpt::apply_command::run_apply_command;
use codex_cli::run_login_status;
use codex_cli::run_login_with_chatgpt;
use codex_cli::run_login_with_device_code;
use codex_cli::run_logout;
use codex_cloud_tasks::Cli as CloudTasksCli;
use codex_exec::Cli as ExecCli;
use codex_rollout_trace::REDUCED_STATE_FILE_NAME;
use codex_rollout_trace::replay_bundle;
use codex_state::StateRuntime;
use codex_tui::AppExitInfo;
use codex_tui::Cli as TuiCli;
use codex_tui::ExitReason;
use codex_utils_cli::CliConfigOverrides;
use codex_utils_cli::ProfileV2Name;
use codex_utils_cli::SharedCliOptions;
use owo_colors::OwoColorize;
use std::collections::HashSet;
use std::io::IsTerminal;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use supports_color::Stream;

mod doctor;
mod state_db_recovery;

use doctor::DoctorCommand;
use state_db_recovery as local_state_db;

use codex_config::LoaderOverrides;
use codex_core::build_models_manager;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_core::config::find_codex_home;
use codex_core::config::resolve_profile_v2_config_path;
use codex_features::FEATURES;
use codex_features::Stage;
use codex_features::is_known_feature_key;
use codex_home::CodexHomeUserInstructionsProvider;
use codex_login::AuthManager;
use codex_memories_write::clear_memory_roots_contents;
use codex_models_manager::bundled_models_response;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::user_input::UserInput;
use codex_terminal_detection::TerminalName;

/// Codex CLI
///
/// If no subcommand is specified, options will be forwarded to the interactive CLI.
#[derive(Debug, Parser)]
#[clap(
    author,
    version,
    // If a sub‑command is given, ignore requirements of the default args.
    subcommand_negates_reqs = true,
    // Keep help output on the generic `codex` command name that users run.
    bin_name = "codex",
    override_usage = "codex [OPTIONS] [PROMPT]\n       codex [OPTIONS] <COMMAND> [ARGS]"
)]
struct MultitoolCli {
    /// Enable process-only PSP routing for first-party ChatGPT requests.
    #[arg(long, global = true, hide = true)]
    psp: bool,

    #[clap(flatten)]
    pub config_overrides: CliConfigOverrides,

    #[clap(flatten)]
    pub feature_toggles: FeatureToggles,

    #[clap(flatten)]
    interactive: TuiCli,

    #[clap(subcommand)]
    subcommand: Option<Subcommand>,
}

#[derive(Debug, clap::Subcommand)]
enum Subcommand {
    /// Run Codex non-interactively.
    Exec(ExecCli),

    /// Manage login.
    Login(LoginCommand),

    /// Remove stored authentication credentials.
    Logout(LogoutCommand),

    /// Generate shell completion scripts.
    Completion(CompletionCommand),

    /// Diagnose local Codex installation, config, auth, and runtime health.
    Doctor(DoctorCommand),

    /// Debugging tools.
    Debug(DebugCommand),

    /// Apply the latest diff produced by Codex agent as a `git apply` to your local working tree.
    Apply(ApplyCommand),

    /// Resume a previous interactive session (picker by default; use --last to continue the most recent).
    Resume(ResumeCommand),

    /// Archive a saved session by id or session name.
    Archive(SessionArchiveCommand),

    /// Permanently delete a saved session by id or session name.
    Delete(DeleteCommand),

    /// Unarchive a saved session by id or session name.
    Unarchive(SessionArchiveCommand),

    /// Fork a previous interactive session (picker by default; use --last to fork the most recent).
    Fork(ForkCommand),

    /// [EXPERIMENTAL] Browse tasks from Codex Cloud and apply changes locally.
    #[clap(name = "cloud")]
    Cloud(CloudTasksCli),

    /// Inspect feature flags.
    Features(FeaturesCli),
}

#[derive(Debug, Parser)]
struct CompletionCommand {
    /// Shell to generate completions for
    #[clap(value_enum, default_value_t = Shell::Bash)]
    shell: Shell,
}

#[derive(Debug, Parser)]
struct DebugCommand {
    #[command(subcommand)]
    subcommand: DebugSubcommand,
}

#[derive(Debug, clap::Subcommand)]
enum DebugSubcommand {
    /// Render the raw model catalog as JSON.
    Models(DebugModelsCommand),

    /// Render the model-visible prompt input list as JSON.
    PromptInput(DebugPromptInputCommand),

    /// Replay a rollout trace bundle and write reduced state JSON.
    #[clap(hide = true)]
    TraceReduce(DebugTraceReduceCommand),

    /// Internal: reset local memory state for a fresh start.
    #[clap(hide = true)]
    ClearMemories,
}

#[derive(Debug, Parser)]
struct DebugPromptInputCommand {
    /// Optional user prompt to append after session context.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// Optional image(s) to attach to the user prompt.
    #[arg(long = "image", short = 'i', value_name = "FILE", value_delimiter = ',', num_args = 1..)]
    images: Vec<PathBuf>,
}

#[derive(Debug, Parser)]
struct DebugModelsCommand {
    /// Skip refresh and dump only the bundled catalog shipped with this binary.
    #[arg(long = "bundled", default_value_t = false)]
    bundled: bool,
}

#[derive(Debug, Parser)]
struct DebugTraceReduceCommand {
    /// Trace bundle directory containing manifest.json and trace.jsonl.
    #[arg(value_name = "TRACE_BUNDLE")]
    trace_bundle: PathBuf,

    /// Output path for reduced RolloutTrace JSON. Defaults to TRACE_BUNDLE/state.json.
    #[arg(long = "output", short = 'o', value_name = "FILE")]
    output: Option<PathBuf>,
}

#[derive(Debug, Parser)]
struct ResumeCommand {
    /// Session id (UUID) or session name. UUIDs take precedence if it parses.
    /// If omitted, use --last to pick the most recent recorded session.
    #[arg(value_name = "SESSION_ID")]
    session_id: Option<String>,

    /// Continue the most recent session without showing the picker.
    #[arg(long = "last", default_value_t = false)]
    last: bool,

    /// Show all sessions (disables cwd filtering and shows CWD column).
    #[arg(long = "all", default_value_t = false)]
    all: bool,

    /// Include non-interactive sessions in the resume picker and --last selection.
    #[arg(long = "include-non-interactive", default_value_t = false)]
    include_non_interactive: bool,

    #[clap(flatten)]
    config_overrides: SessionTuiCli,
}

#[derive(Debug, Parser)]
struct SessionArchiveCommand {
    /// Session id (UUID) or session name. UUIDs take precedence if it parses.
    #[arg(value_name = "SESSION")]
    target: String,

    #[clap(flatten)]
    config_overrides: SessionArchiveConfigOverrides,
}

#[derive(Debug, Args, Clone, Default)]
struct SessionArchiveConfigOverrides {
    #[clap(flatten)]
    shared: SharedCliOptions,

    /// Error out when config.toml contains fields that are not recognized by this version of Codex.
    #[arg(long = "strict-config", default_value_t = false)]
    strict_config: bool,

    #[clap(flatten)]
    config_overrides: CliConfigOverrides,
}

#[derive(Debug, Args)]
struct DeleteCommand {
    #[clap(flatten)]
    session: SessionArchiveCommand,

    /// Delete without prompting. SESSION must be a UUID.
    #[arg(long, default_value_t = false)]
    force: bool,
}

#[derive(Debug, Parser)]
struct ForkCommand {
    /// Conversation/session id (UUID). When provided, forks this session.
    /// If omitted, use --last to pick the most recent recorded session.
    #[arg(value_name = "SESSION_ID")]
    session_id: Option<String>,

    /// Fork the most recent session without showing the picker.
    #[arg(long = "last", default_value_t = false)]
    last: bool,

    /// Show all sessions (disables cwd filtering and shows CWD column).
    #[arg(long = "all", default_value_t = false)]
    all: bool,

    #[clap(flatten)]
    config_overrides: SessionTuiCli,
}

/// TUI arguments for session commands where a parsed prompt implies an explicit session id.
///
/// This keeps `--last PROMPT` valid while rejecting `--last SESSION_ID PROMPT`.
#[derive(Debug)]
struct SessionTuiCli(TuiCli);

impl Args for SessionTuiCli {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        TuiCli::augment_args(cmd).mut_arg("prompt", |arg| arg.conflicts_with("last"))
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        TuiCli::augment_args_for_update(cmd).mut_arg("prompt", |arg| arg.conflicts_with("last"))
    }
}

impl clap::FromArgMatches for SessionTuiCli {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        TuiCli::from_arg_matches(matches).map(Self)
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        self.0.update_from_arg_matches(matches)
    }
}

#[derive(Debug, Parser)]
struct LoginCommand {
    #[clap(skip)]
    config_overrides: CliConfigOverrides,

    #[arg(long = "device-auth")]
    use_device_code: bool,

    #[command(subcommand)]
    action: Option<LoginSubcommand>,
}

#[derive(Debug, clap::Subcommand)]
enum LoginSubcommand {
    /// Show login status.
    Status,
}

#[derive(Debug, Parser)]
struct LogoutCommand {
    #[clap(skip)]
    config_overrides: CliConfigOverrides,
}

fn format_exit_messages(exit_info: AppExitInfo, color_enabled: bool) -> Vec<String> {
    let is_fatal = matches!(&exit_info.exit_reason, ExitReason::Fatal(_));
    let AppExitInfo {
        token_usage,
        thread_id: conversation_id,
        session_title,
        resume_hint,
        ..
    } = exit_info;

    let mut lines = Vec::new();
    if let Some(resume_cmd) = resume_hint {
        let command = if color_enabled {
            resume_cmd.cyan().to_string()
        } else {
            resume_cmd
        };
        let identity = if color_enabled {
            "ycode".bold().to_string()
        } else {
            "ycode".to_string()
        };
        lines.push(String::new());
        lines.push(identity);
        if let Some(title) = session_title.filter(|title| !title.trim().is_empty()) {
            lines.push(format!(
                "Session   {}",
                title.trim().replace(['\r', '\n'], " ")
            ));
        }
        lines.push(format!("Continue  {command}"));
        if !token_usage.is_zero() {
            lines.push(format!("Usage     {token_usage}"));
        }
        lines.push(String::new());
    } else if is_fatal && let Some(conversation_id) = conversation_id {
        lines.push(format!("Session ID: {conversation_id}"));
    } else if !token_usage.is_zero() {
        lines.push(token_usage.to_string());
    }

    lines
}

/// Handle the app exit and print the results.
fn handle_app_exit(exit_info: AppExitInfo) -> anyhow::Result<()> {
    let is_fatal = match &exit_info.exit_reason {
        ExitReason::Fatal(message) => {
            eprintln!("ERROR: {message}");
            true
        }
        ExitReason::UserRequested => false,
    };

    let color_enabled = supports_color::on(Stream::Stdout).is_some();
    for line in format_exit_messages(exit_info, color_enabled) {
        println!("{line}");
    }
    if is_fatal {
        std::io::stdout().flush()?;
        std::process::exit(1);
    }
    Ok(())
}

async fn run_session_archive_cli_command(
    action: codex_tui::SessionArchiveAction,
    cmd: SessionArchiveCommand,
    mut interactive: TuiCli,
    root_config_overrides: CliConfigOverrides,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<String> {
    let SessionArchiveCommand {
        target,
        config_overrides,
    } = cmd;
    interactive =
        finalize_session_archive_interactive(interactive, root_config_overrides, config_overrides);
    codex_tui::run_session_archive_command(
        action,
        target,
        codex_tui::SessionArchiveCommandOptions {
            cli: interactive,
            arg0_paths,
        },
    )
    .await
    .map_err(|err| anyhow::anyhow!("{err}"))
}

fn delete_action(target: &str, force: bool) -> anyhow::Result<codex_tui::SessionArchiveAction> {
    if force && codex_protocol::ThreadId::from_string(target).is_err() {
        anyhow::bail!("--force requires a session UUID; names must be confirmed interactively");
    }
    let confirmation = match force {
        true => codex_tui::DeleteConfirmation::Skip,
        false => codex_tui::DeleteConfirmation::Prompt,
    };
    Ok(codex_tui::SessionArchiveAction::Delete(confirmation))
}

#[derive(Debug, Default, Parser, Clone)]
struct FeatureToggles {
    /// Enable a feature (repeatable). Equivalent to `-c features.<name>=true`.
    #[arg(long = "enable", value_name = "FEATURE", action = clap::ArgAction::Append, global = true)]
    enable: Vec<String>,

    /// Disable a feature (repeatable). Equivalent to `-c features.<name>=false`.
    #[arg(long = "disable", value_name = "FEATURE", action = clap::ArgAction::Append, global = true)]
    disable: Vec<String>,
}

impl FeatureToggles {
    fn to_overrides(&self) -> anyhow::Result<Vec<String>> {
        let mut v = Vec::new();
        for feature in &self.enable {
            Self::validate_feature(feature)?;
            v.push(format!("features.{feature}=true"));
        }
        for feature in &self.disable {
            Self::validate_feature(feature)?;
            v.push(format!("features.{feature}=false"));
        }
        Ok(v)
    }

    fn validate_feature(feature: &str) -> anyhow::Result<()> {
        if is_known_feature_key(feature) {
            Ok(())
        } else {
            anyhow::bail!("Unknown feature flag: {feature}")
        }
    }
}

#[derive(Debug, Parser)]
struct FeaturesCli {
    #[command(subcommand)]
    sub: FeaturesSubcommand,
}

#[derive(Debug, Parser)]
enum FeaturesSubcommand {
    /// List known features with their stage and effective state.
    List,
    /// Enable a feature in config.toml.
    Enable(FeatureSetArgs),
    /// Disable a feature in config.toml.
    Disable(FeatureSetArgs),
}

#[derive(Debug, Parser)]
struct FeatureSetArgs {
    /// Feature key to update (for example: unified_exec).
    feature: String,
}

fn stage_str(stage: Stage) -> &'static str {
    match stage {
        Stage::UnderDevelopment => "under development",
        Stage::Experimental { .. } => "experimental",
        Stage::Stable => "stable",
    }
}

fn main() -> anyhow::Result<()> {
    arg0_dispatch_or_else(move |arg0_paths: Arg0DispatchPaths| async move {
        cli_main(arg0_paths).await?;
        Ok(())
    })
}

async fn cli_main(arg0_paths: Arg0DispatchPaths) -> anyhow::Result<()> {
    let MultitoolCli {
        psp,
        config_overrides: mut root_config_overrides,
        feature_toggles,
        mut interactive,
        subcommand,
    } = MultitoolCli::parse();
    interactive.psp = psp;

    // Fold --enable/--disable into config overrides so they flow to all subcommands.
    let toggle_overrides = feature_toggles.to_overrides()?;
    root_config_overrides.raw_overrides.extend(toggle_overrides);
    let root_strict_config = interactive.strict_config;
    reject_root_strict_config_for_subcommand(root_strict_config, &subcommand)?;
    if let Some(subcommand) = subcommand.as_ref() {
        profile_v2_for_subcommand(&interactive, subcommand)?;
    }

    match subcommand {
        None => {
            prepend_config_flags(
                &mut interactive.config_overrides,
                root_config_overrides.clone(),
            );
            let exit_info = run_interactive_tui(interactive, arg0_paths.clone()).await?;
            handle_app_exit(exit_info)?;
        }
        Some(Subcommand::Exec(mut exec_cli)) => {
            exec_cli
                .shared
                .inherit_exec_root_options(&interactive.shared);
            exec_cli.psp = psp;
            exec_cli.strict_config |= root_strict_config;
            prepend_config_flags(
                &mut exec_cli.config_overrides,
                root_config_overrides.clone(),
            );
            codex_exec::run_main(exec_cli, arg0_paths.clone()).await?;
        }
        Some(Subcommand::Resume(ResumeCommand {
            session_id,
            last,
            all,
            include_non_interactive,
            config_overrides,
        })) => {
            let SessionTuiCli(config_overrides) = config_overrides;
            interactive = finalize_resume_interactive(
                interactive,
                root_config_overrides.clone(),
                session_id,
                last,
                all,
                include_non_interactive,
                config_overrides,
            );
            let exit_info = run_interactive_tui(interactive, arg0_paths.clone()).await?;
            handle_app_exit(exit_info)?;
        }
        Some(Subcommand::Archive(cmd)) => {
            let output = run_session_archive_cli_command(
                codex_tui::SessionArchiveAction::Archive,
                cmd,
                interactive,
                root_config_overrides.clone(),
                arg0_paths.clone(),
            )
            .await?;
            println!("{output}");
        }
        Some(Subcommand::Delete(DeleteCommand { session, force })) => {
            let action = delete_action(&session.target, force)?;
            let output = run_session_archive_cli_command(
                action,
                session,
                interactive,
                root_config_overrides.clone(),
                arg0_paths.clone(),
            )
            .await?;
            println!("{output}");
        }
        Some(Subcommand::Unarchive(cmd)) => {
            let output = run_session_archive_cli_command(
                codex_tui::SessionArchiveAction::Unarchive,
                cmd,
                interactive,
                root_config_overrides.clone(),
                arg0_paths.clone(),
            )
            .await?;
            println!("{output}");
        }
        Some(Subcommand::Fork(ForkCommand {
            session_id,
            last,
            all,
            config_overrides,
        })) => {
            let SessionTuiCli(config_overrides) = config_overrides;
            interactive = finalize_fork_interactive(
                interactive,
                root_config_overrides.clone(),
                session_id,
                last,
                all,
                config_overrides,
            );
            let exit_info = run_interactive_tui(interactive, arg0_paths.clone()).await?;
            handle_app_exit(exit_info)?;
        }
        Some(Subcommand::Login(mut login_cli)) => {
            prepend_config_flags(
                &mut login_cli.config_overrides,
                root_config_overrides.clone(),
            );
            match login_cli.action {
                Some(LoginSubcommand::Status) => {
                    run_login_status(login_cli.config_overrides).await;
                }
                None => {
                    if login_cli.use_device_code {
                        run_login_with_device_code(login_cli.config_overrides).await;
                    } else {
                        run_login_with_chatgpt(login_cli.config_overrides).await;
                    }
                }
            }
        }
        Some(Subcommand::Logout(mut logout_cli)) => {
            prepend_config_flags(
                &mut logout_cli.config_overrides,
                root_config_overrides.clone(),
            );
            run_logout(logout_cli.config_overrides).await;
        }
        Some(Subcommand::Completion(completion_cli)) => {
            print_completion(completion_cli);
        }
        Some(Subcommand::Doctor(doctor_cli)) => {
            doctor::run_doctor(
                doctor_cli,
                root_config_overrides.clone(),
                &interactive,
                &arg0_paths,
            )
            .await?;
        }
        Some(Subcommand::Cloud(mut cloud_cli)) => {
            prepend_config_flags(
                &mut cloud_cli.config_overrides,
                root_config_overrides.clone(),
            );
            codex_cloud_tasks::run_main(cloud_cli).await?;
        }
        Some(Subcommand::Debug(DebugCommand { subcommand })) => match subcommand {
            DebugSubcommand::Models(cmd) => {
                run_debug_models_command(cmd, root_config_overrides).await?;
            }
            DebugSubcommand::PromptInput(cmd) => {
                run_debug_prompt_input_command(
                    cmd,
                    root_config_overrides,
                    interactive,
                    arg0_paths.clone(),
                )
                .await?;
            }
            DebugSubcommand::TraceReduce(cmd) => {
                run_debug_trace_reduce_command(cmd).await?;
            }
            DebugSubcommand::ClearMemories => {
                run_debug_clear_memories_command(&root_config_overrides).await?;
            }
        },
        Some(Subcommand::Apply(mut apply_cli)) => {
            prepend_config_flags(
                &mut apply_cli.config_overrides,
                root_config_overrides.clone(),
            );
            run_apply_command(apply_cli, /*cwd*/ None).await?;
        }
        Some(Subcommand::Features(FeaturesCli { sub })) => match sub {
            FeaturesSubcommand::List => {
                let mut cli_kv_overrides = root_config_overrides
                    .parse_overrides()
                    .map_err(anyhow::Error::msg)?;

                // Honor `--search` via the canonical web_search mode.
                if interactive.web_search {
                    cli_kv_overrides.push((
                        "web_search".to_string(),
                        toml::Value::String("live".to_string()),
                    ));
                }

                let config = ConfigBuilder::default()
                    .cli_overrides(cli_kv_overrides)
                    .build()
                    .await?;
                let mut rows = Vec::with_capacity(FEATURES.len());
                let mut name_width = 0;
                let mut stage_width = 0;
                for def in FEATURES {
                    let name = def.key;
                    let stage = stage_str(def.stage);
                    let enabled = config.features.enabled(def.id);
                    name_width = name_width.max(name.len());
                    stage_width = stage_width.max(stage.len());
                    rows.push((name, stage, enabled));
                }
                rows.sort_unstable_by_key(|(name, _, _)| *name);

                for (name, stage, enabled) in rows {
                    println!("{name:<name_width$}  {stage:<stage_width$}  {enabled}");
                }
            }
            FeaturesSubcommand::Enable(FeatureSetArgs { feature }) => {
                enable_feature_in_config(&feature).await?;
            }
            FeaturesSubcommand::Disable(FeatureSetArgs { feature }) => {
                disable_feature_in_config(&feature).await?;
            }
        },
    }

    Ok(())
}

fn profile_v2_for_subcommand<'a>(
    interactive: &'a TuiCli,
    subcommand: &Subcommand,
) -> anyhow::Result<Option<&'a ProfileV2Name>> {
    let Some(profile_v2) = interactive.config_profile_v2.as_ref() else {
        return Ok(None);
    };

    match subcommand {
        Subcommand::Exec(_)
        | Subcommand::Resume(_)
        | Subcommand::Archive(_)
        | Subcommand::Delete(_)
        | Subcommand::Unarchive(_)
        | Subcommand::Fork(_)
        | Subcommand::Debug(DebugCommand {
            subcommand: DebugSubcommand::PromptInput(_),
        }) => Ok(Some(profile_v2)),
        _ => anyhow::bail!(
            "--profile only applies to runtime commands: `codex`, `codex exec`, `codex resume`, `codex archive`, `codex delete`, `codex unarchive`, `codex fork`, and `codex debug prompt-input`."
        ),
    }
}

async fn enable_feature_in_config(feature: &str) -> anyhow::Result<()> {
    FeatureToggles::validate_feature(feature)?;
    let codex_home = find_codex_home()?;
    ConfigEditsBuilder::new(&codex_home)
        .set_feature_enabled(feature, /*enabled*/ true)
        .apply()
        .await?;
    println!("Enabled feature `{feature}` in config.toml.");
    maybe_print_under_development_feature_warning(&codex_home, feature);
    Ok(())
}

async fn disable_feature_in_config(feature: &str) -> anyhow::Result<()> {
    FeatureToggles::validate_feature(feature)?;
    let codex_home = find_codex_home()?;
    ConfigEditsBuilder::new(&codex_home)
        .set_feature_enabled(feature, /*enabled*/ false)
        .apply()
        .await?;
    println!("Disabled feature `{feature}` in config.toml.");
    Ok(())
}

fn loader_overrides_for_profile(
    profile_v2: Option<&ProfileV2Name>,
) -> anyhow::Result<LoaderOverrides> {
    match profile_v2 {
        Some(profile_v2) => {
            let codex_home = find_codex_home()?;
            Ok(loader_overrides_for_profile_at_codex_home(
                Some(profile_v2),
                &codex_home,
            ))
        }
        None => Ok(LoaderOverrides::default()),
    }
}

fn loader_overrides_for_profile_at_codex_home(
    profile_v2: Option<&ProfileV2Name>,
    codex_home: &std::path::Path,
) -> LoaderOverrides {
    match profile_v2 {
        Some(profile_v2) => LoaderOverrides {
            user_config_path: Some(resolve_profile_v2_config_path(codex_home, profile_v2)),
            user_config_profile: Some(profile_v2.clone()),
            ..Default::default()
        },
        None => LoaderOverrides::default(),
    }
}

fn maybe_print_under_development_feature_warning(codex_home: &std::path::Path, feature: &str) {
    let Some(spec) = FEATURES.iter().find(|spec| spec.key == feature) else {
        return;
    };
    if !matches!(spec.stage, Stage::UnderDevelopment) {
        return;
    }

    let config_path = codex_home.join(codex_config::CONFIG_TOML_FILE);
    eprintln!(
        "Under-development features enabled: {feature}. Under-development features are incomplete and may behave unpredictably. To suppress this warning, set `suppress_unstable_features_warning = true` in {}.",
        config_path.display()
    );
}

async fn run_debug_trace_reduce_command(cmd: DebugTraceReduceCommand) -> anyhow::Result<()> {
    let output = cmd
        .output
        .unwrap_or_else(|| cmd.trace_bundle.join(REDUCED_STATE_FILE_NAME));

    let trace = replay_bundle(&cmd.trace_bundle)?;
    let reduced_json = serde_json::to_vec_pretty(&trace)?;
    tokio::fs::write(&output, reduced_json).await?;
    println!("{}", output.display());

    Ok(())
}

async fn run_debug_prompt_input_command(
    cmd: DebugPromptInputCommand,
    root_config_overrides: CliConfigOverrides,
    interactive: TuiCli,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<()> {
    let loader_overrides = loader_overrides_for_profile(interactive.config_profile_v2.as_ref())?;
    let shared = interactive.shared.into_inner();
    let mut cli_kv_overrides = root_config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;
    if interactive.web_search {
        cli_kv_overrides.push((
            "web_search".to_string(),
            toml::Value::String("live".to_string()),
        ));
    }

    let overrides = ConfigOverrides {
        model: shared.model,
        cwd: shared.cwd,
        codex_self_exe: arg0_paths.codex_self_exe,
        main_execve_wrapper_exe: arg0_paths.main_execve_wrapper_exe,
        ephemeral: Some(true),
        ..Default::default()
    };
    let config = ConfigBuilder::default()
        .cli_overrides(cli_kv_overrides)
        .harness_overrides(overrides)
        .loader_overrides(loader_overrides)
        .build()
        .await?;

    let mut input = shared
        .images
        .into_iter()
        .chain(cmd.images)
        .map(|path| UserInput::LocalImage { path, detail: None })
        .collect::<Vec<_>>();
    if let Some(prompt) = cmd.prompt.or(interactive.prompt) {
        input.push(UserInput::Text {
            text: prompt.replace("\r\n", "\n").replace('\r', "\n"),
            text_elements: Vec::new(),
        });
    }

    let user_instructions_provider = Arc::new(CodexHomeUserInstructionsProvider::new(
        config.codex_home.clone(),
    ));
    let auth_manager =
        AuthManager::shared_from_config(&config, /*enable_codex_api_key_env*/ false).await;
    let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
    codex_git_attribution::install(
        &mut extensions,
        auth_manager,
        config.chatgpt_base_url.clone(),
        config.http_client_factory(),
    );
    codex_skills_extension::install(&mut extensions, |config: &Config| {
        codex_skills_extension::SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            bundled_skills_enabled: config.bundled_skills_enabled(),
            orchestrator_skills_enabled: config.orchestrator_skills_enabled,
        }
    });
    let prompt_input = codex_core::build_prompt_input(
        config,
        input,
        /*state_db*/ None,
        Arc::new(extensions.build()),
        user_instructions_provider,
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&prompt_input)?);

    Ok(())
}

async fn run_debug_models_command(
    cmd: DebugModelsCommand,
    root_config_overrides: CliConfigOverrides,
) -> anyhow::Result<()> {
    let catalog = if cmd.bundled {
        bundled_models_response()?
    } else {
        let cli_overrides = root_config_overrides
            .parse_overrides()
            .map_err(anyhow::Error::msg)?;
        let config = ConfigBuilder::default()
            .cli_overrides(cli_overrides)
            .build()
            .await?;
        let auth_manager =
            AuthManager::shared_from_config(&config, /*enable_codex_api_key_env*/ true).await;
        let models_manager = build_models_manager(&config, auth_manager);
        models_manager
            .raw_model_catalog(
                RefreshStrategy::OnlineIfUncached,
                config.http_client_factory(),
            )
            .await
    };

    serde_json::to_writer(std::io::stdout(), &catalog)?;
    println!();
    Ok(())
}

async fn run_debug_clear_memories_command(
    root_config_overrides: &CliConfigOverrides,
) -> anyhow::Result<()> {
    let cli_kv_overrides = root_config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;
    let config = ConfigBuilder::default()
        .cli_overrides(cli_kv_overrides)
        .build()
        .await?;

    let memories_path = config.sqlite_config().memories_db_path();
    let cleared_memories_db =
        StateRuntime::clear_memory_data_in_sqlite_home(config.sqlite_config()).await?;

    clear_memory_roots_contents(&config.codex_home).await?;

    let mut message = if cleared_memories_db {
        format!("Cleared memory state from {}.", memories_path.display())
    } else {
        format!("No memories db found at {}.", memories_path.display())
    };
    message.push_str(&format!(
        " Cleared memory directories under {}.",
        config.codex_home.display()
    ));

    println!("{message}");

    Ok(())
}

/// Prepend root-level overrides so they have lower precedence than
/// CLI-specific ones specified after the subcommand (if any).
fn prepend_config_flags(
    subcommand_config_overrides: &mut CliConfigOverrides,
    cli_config_overrides: CliConfigOverrides,
) {
    subcommand_config_overrides.prepend_root_overrides(cli_config_overrides);
}

fn reject_root_strict_config_for_subcommand(
    strict_config: bool,
    subcommand: &Option<Subcommand>,
) -> anyhow::Result<()> {
    if !strict_config {
        return Ok(());
    }

    match unsupported_subcommand_name_for_strict_config(subcommand) {
        Some(subcommand_name) => {
            reject_strict_config_for_unsupported_subcommand(strict_config, subcommand_name)
        }
        None => Ok(()),
    }
}

/// Return the selected subcommand name when a root-level `--strict-config`
/// flag should be rejected after parsing.
///
/// `--strict-config` is parsed on the root interactive CLI so commands like
/// `codex --strict-config` continue to work for the TUI and for wrappers that
/// forward root options into another command shape. Clap will still accept that
/// root flag before the dispatcher knows which subcommand the user selected, so
/// unsupported subcommands need an explicit post-parse reject path.
///
/// `Some(...)` returns the user-facing command name fragment to embed in the
/// rejection error, such as `cloud`. `None` means the
/// selected command is allowed to inherit root `--strict-config`.
fn unsupported_subcommand_name_for_strict_config(
    subcommand: &Option<Subcommand>,
) -> Option<&'static str> {
    match subcommand {
        None
        | Some(Subcommand::Exec(_))
        | Some(Subcommand::Resume(_))
        | Some(Subcommand::Archive(_))
        | Some(Subcommand::Delete(_))
        | Some(Subcommand::Unarchive(_))
        | Some(Subcommand::Fork(_))
        | Some(Subcommand::Doctor(_)) => None,
        Some(Subcommand::Login(_)) => Some("login"),
        Some(Subcommand::Logout(_)) => Some("logout"),
        Some(Subcommand::Completion(_)) => Some("completion"),
        Some(Subcommand::Cloud(_)) => Some("cloud"),
        Some(Subcommand::Debug(_)) => Some("debug"),
        Some(Subcommand::Apply(_)) => Some("apply"),
        Some(Subcommand::Features(_)) => Some("features"),
    }
}

fn reject_strict_config_for_unsupported_subcommand(
    strict_config: bool,
    subcommand: &str,
) -> anyhow::Result<()> {
    if strict_config {
        anyhow::bail!("`--strict-config` is not supported for `codex {subcommand}`");
    }
    Ok(())
}

async fn run_interactive_tui(
    mut interactive: TuiCli,
    arg0_paths: Arg0DispatchPaths,
) -> std::io::Result<AppExitInfo> {
    if let Some(prompt) = interactive.prompt.take() {
        // Normalize CRLF/CR to LF so CLI-provided text can't leak `\r` into TUI state.
        interactive.prompt = Some(prompt.replace("\r\n", "\n").replace('\r', "\n"));
    }

    let terminal_info = codex_terminal_detection::terminal_info();
    if terminal_info.name == TerminalName::Dumb {
        if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
            return Ok(AppExitInfo::fatal(
                "TERM is set to \"dumb\". Refusing to start the interactive TUI because no terminal is available for a confirmation prompt (stdin/stderr is not a TTY). Run in a supported terminal or unset TERM.",
            ));
        }

        eprintln!(
            "WARNING: TERM is set to \"dumb\". Codex's interactive TUI may not work in this terminal."
        );
        if !confirm("Continue anyway? [y/N]: ")? {
            return Ok(AppExitInfo::fatal(
                "Refusing to start the interactive TUI because TERM is set to \"dumb\". Run in a supported terminal or unset TERM.",
            ));
        }
    }

    let start_tui = || {
        codex_tui::run_main(
            interactive.clone(),
            arg0_paths.clone(),
            codex_config::LoaderOverrides::default(),
        )
    };
    let mut attempted_backups = HashSet::new();
    loop {
        let err = match start_tui().await {
            Ok(exit_info) => return Ok(exit_info),
            Err(err) => err,
        };
        let Some(startup_error) = local_state_db::startup_error(&err) else {
            return Err(err);
        };
        if local_state_db::is_locked(startup_error.detail()) {
            local_state_db::print_locked_guidance(startup_error);
            return Ok(AppExitInfo::fatal(startup_error.to_string()));
        }
        if !local_state_db::is_auto_backup_recoverable(startup_error) {
            local_state_db::print_diagnostic_guidance(startup_error);
            return Ok(AppExitInfo::fatal(startup_error.to_string()));
        }
        if !attempted_backups.insert(startup_error.database_path().to_path_buf()) {
            local_state_db::print_diagnostic_guidance(startup_error);
            return Ok(AppExitInfo::fatal(startup_error.to_string()));
        }

        local_state_db::print_auto_backup_start(startup_error);
        match local_state_db::backup_files_for_fresh_start(startup_error).await {
            Ok(backups) => local_state_db::confirm_fresh_start_rebuild(startup_error, &backups)?,
            Err(backup_err) => {
                local_state_db::print_diagnostic_guidance(startup_error);
                return Ok(AppExitInfo::fatal(format!(
                    "failed to move damaged Codex local database files into a backup folder automatically: {backup_err}"
                )));
            }
        }
    }
}

fn confirm(prompt: &str) -> std::io::Result<bool> {
    eprintln!("{prompt}");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let answer = input.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

/// Build the final `TuiCli` for a `codex resume` invocation.
fn finalize_resume_interactive(
    mut interactive: TuiCli,
    root_config_overrides: CliConfigOverrides,
    session_id: Option<String>,
    last: bool,
    show_all: bool,
    include_non_interactive: bool,
    mut resume_cli: TuiCli,
) -> TuiCli {
    // Start with the parsed interactive CLI so resume shares the same
    // configuration surface area as `codex` without additional flags.
    // Clap assigns the first positional to `session_id`. With `--last`, reinterpret it as the
    // prompt when no second positional prompt was provided.
    let resume_session_id = if last && resume_cli.prompt.is_none() {
        resume_cli.prompt = session_id;
        None
    } else {
        session_id
    };
    interactive.resume_picker = resume_session_id.is_none() && !last;
    interactive.resume_last = last;
    interactive.resume_session_id = resume_session_id;
    interactive.resume_show_all = show_all;
    interactive.resume_include_non_interactive = include_non_interactive;

    // Merge resume-scoped flags and overrides with highest precedence.
    merge_interactive_cli_flags(&mut interactive, resume_cli);

    // Propagate any root-level config overrides (e.g. `-c key=value`).
    prepend_config_flags(&mut interactive.config_overrides, root_config_overrides);

    interactive
}

/// Build the final `TuiCli` for a `codex fork` invocation.
fn finalize_fork_interactive(
    mut interactive: TuiCli,
    root_config_overrides: CliConfigOverrides,
    session_id: Option<String>,
    last: bool,
    show_all: bool,
    mut fork_cli: TuiCli,
) -> TuiCli {
    // Start with the parsed interactive CLI so fork shares the same
    // configuration surface area as `codex` without additional flags.
    // Clap assigns the first positional to `session_id`. With `--last`, reinterpret it as the
    // prompt when no second positional prompt was provided.
    let fork_session_id = if last && fork_cli.prompt.is_none() {
        fork_cli.prompt = session_id;
        None
    } else {
        session_id
    };
    interactive.fork_picker = fork_session_id.is_none() && !last;
    interactive.fork_last = last;
    interactive.fork_session_id = fork_session_id;
    interactive.fork_show_all = show_all;

    // Merge fork-scoped flags and overrides with highest precedence.
    merge_interactive_cli_flags(&mut interactive, fork_cli);

    // Propagate any root-level config overrides (e.g. `-c key=value`).
    prepend_config_flags(&mut interactive.config_overrides, root_config_overrides);

    interactive
}

fn finalize_session_archive_interactive(
    mut interactive: TuiCli,
    root_config_overrides: CliConfigOverrides,
    archive_cli: SessionArchiveConfigOverrides,
) -> TuiCli {
    let SessionArchiveConfigOverrides {
        shared,
        strict_config,
        config_overrides,
    } = archive_cli;
    interactive.shared.apply_subcommand_overrides(shared);
    if strict_config {
        interactive.strict_config = true;
    }
    interactive
        .config_overrides
        .raw_overrides
        .extend(config_overrides.raw_overrides);
    prepend_config_flags(&mut interactive.config_overrides, root_config_overrides);
    interactive
}

/// Merge flags provided to runtime wrapper commands so they take precedence over any root-level
/// flags. Only overrides fields explicitly set on the subcommand-scoped CLI. Also appends
/// `-c key=value` overrides with highest precedence.
fn merge_interactive_cli_flags(interactive: &mut TuiCli, subcommand_cli: TuiCli) {
    let TuiCli {
        shared,
        strict_config,
        web_search,
        prompt,
        config_overrides,
        ..
    } = subcommand_cli;
    interactive
        .shared
        .apply_subcommand_overrides(shared.into_inner());
    if web_search {
        interactive.web_search = true;
    }
    if strict_config {
        interactive.strict_config = true;
    }
    if let Some(prompt) = prompt {
        // Normalize CRLF/CR to LF so CLI-provided text can't leak `\r` into TUI state.
        interactive.prompt = Some(prompt.replace("\r\n", "\n").replace('\r', "\n"));
    }

    interactive
        .config_overrides
        .raw_overrides
        .extend(config_overrides.raw_overrides);
}

fn print_completion(cmd: CompletionCommand) {
    let mut app = MultitoolCli::command();
    let name = "codex";
    generate(cmd.shell, &mut app, name, &mut std::io::stdout());
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::ThreadId;
    use codex_tui::TokenUsage;
    use pretty_assertions::assert_eq;

    fn finalize_resume_from_args(args: &[&str]) -> TuiCli {
        let cli = MultitoolCli::try_parse_from(args).expect("parse");
        let MultitoolCli {
            psp: _,
            interactive,
            config_overrides: root_overrides,
            subcommand,
            feature_toggles: _,
        } = cli;
        let Subcommand::Resume(ResumeCommand {
            session_id,
            last,
            all,
            include_non_interactive,
            config_overrides: resume_cli,
        }) = subcommand.expect("resume present")
        else {
            unreachable!()
        };
        let SessionTuiCli(resume_cli) = resume_cli;

        finalize_resume_interactive(
            interactive,
            root_overrides,
            session_id,
            last,
            all,
            include_non_interactive,
            resume_cli,
        )
    }

    fn finalize_fork_from_args(args: &[&str]) -> TuiCli {
        let cli = MultitoolCli::try_parse_from(args).expect("parse");
        let MultitoolCli {
            psp: _,
            interactive,
            config_overrides: root_overrides,
            subcommand,
            feature_toggles: _,
        } = cli;
        let Subcommand::Fork(ForkCommand {
            session_id,
            last,
            all,
            config_overrides: fork_cli,
        }) = subcommand.expect("fork present")
        else {
            unreachable!()
        };
        let SessionTuiCli(fork_cli) = fork_cli;

        finalize_fork_interactive(interactive, root_overrides, session_id, last, all, fork_cli)
    }

    fn finalize_archive_from_args(args: &[&str]) -> (String, TuiCli) {
        let cli = MultitoolCli::try_parse_from(args).expect("parse");
        let MultitoolCli {
            psp: _,
            interactive,
            config_overrides: root_overrides,
            subcommand,
            feature_toggles: _,
        } = cli;

        let Subcommand::Archive(SessionArchiveCommand {
            target,
            config_overrides: archive_cli,
        }) = subcommand.expect("archive present")
        else {
            unreachable!()
        };

        (
            target,
            finalize_session_archive_interactive(interactive, root_overrides, archive_cli),
        )
    }

    fn profile_v2_for_args(args: &[&str]) -> anyhow::Result<Option<String>> {
        let cli = MultitoolCli::try_parse_from(args).expect("parse");
        let Some(subcommand) = cli.subcommand.as_ref() else {
            return Ok(cli
                .interactive
                .config_profile_v2
                .as_ref()
                .map(std::string::ToString::to_string));
        };
        Ok(profile_v2_for_subcommand(&cli.interactive, subcommand)?.map(ToString::to_string))
    }

    #[test]
    fn profile_loader_overrides_use_explicit_codex_home() -> anyhow::Result<()> {
        let codex_home = tempfile::tempdir()?;
        let profile: ProfileV2Name = "work".parse()?;

        let overrides =
            loader_overrides_for_profile_at_codex_home(Some(&profile), codex_home.path());

        assert_eq!(
            overrides.user_config_path,
            Some(resolve_profile_v2_config_path(codex_home.path(), &profile))
        );
        assert_eq!(overrides.user_config_profile, Some(profile));
        Ok(())
    }

    #[test]
    fn profile_v2_is_rejected_for_config_management_subcommands() {
        assert!(profile_v2_for_args(&["codex", "--profile", "work", "features", "list"]).is_err());
    }

    #[test]
    fn profile_v2_is_allowed_for_runtime_subcommands() {
        assert_eq!(
            profile_v2_for_args(&["codex", "--profile", "work", "resume"])
                .expect("resume supports profile-v2")
                .as_deref(),
            Some("work")
        );
        assert_eq!(
            profile_v2_for_args(&["codex", "--profile", "work", "debug", "prompt-input"])
                .expect("debug prompt-input supports profile-v2")
                .as_deref(),
            Some("work")
        );
    }

    #[test]
    fn import_remains_an_interactive_prompt() {
        let cli = MultitoolCli::try_parse_from(["codex", "import"]).expect("parse");

        assert!(cli.subcommand.is_none());
        assert_eq!(cli.interactive.prompt.as_deref(), Some("import"));
    }

    #[test]
    fn profile_v2_rejects_non_plain_names_at_parse_time() {
        assert!(
            MultitoolCli::try_parse_from(["codex", "--profile", "nested/work", "resume"]).is_err()
        );
    }

    #[test]
    fn exec_resume_last_accepts_prompt_positional() {
        let cli =
            MultitoolCli::try_parse_from(["codex", "exec", "--json", "resume", "--last", "2+2"])
                .expect("parse should succeed");

        let Some(Subcommand::Exec(exec)) = cli.subcommand else {
            panic!("expected exec subcommand");
        };
        let Some(codex_exec::Command::Resume(args)) = exec.command else {
            panic!("expected exec resume");
        };

        assert!(args.last);
        assert_eq!(args.session_id, None);
        assert_eq!(args.prompt.as_deref(), Some("2+2"));
    }

    #[test]
    fn exec_resume_accepts_output_flags_after_subcommand() {
        let cli = MultitoolCli::try_parse_from([
            "codex",
            "exec",
            "resume",
            "session-123",
            "-o",
            "/tmp/resume-output.md",
            "--output-schema",
            "/tmp/schema.json",
            "re-review",
        ])
        .expect("parse should succeed");

        let Some(Subcommand::Exec(exec)) = cli.subcommand else {
            panic!("expected exec subcommand");
        };
        let Some(codex_exec::Command::Resume(args)) = exec.command else {
            panic!("expected exec resume");
        };

        assert_eq!(
            exec.last_message_file,
            Some(std::path::PathBuf::from("/tmp/resume-output.md"))
        );
        assert_eq!(
            exec.output_schema,
            Some(std::path::PathBuf::from("/tmp/schema.json"))
        );
        assert_eq!(args.session_id.as_deref(), Some("session-123"));
        assert_eq!(args.prompt.as_deref(), Some("re-review"));
    }

    #[test]
    fn debug_prompt_input_parses_prompt_and_images() {
        let cli = MultitoolCli::try_parse_from([
            "codex",
            "debug",
            "prompt-input",
            "hello",
            "--image",
            "/tmp/a.png,/tmp/b.png",
        ])
        .expect("parse");

        let Some(Subcommand::Debug(DebugCommand {
            subcommand: DebugSubcommand::PromptInput(cmd),
        })) = cli.subcommand
        else {
            panic!("expected debug prompt-input subcommand");
        };

        assert_eq!(cmd.prompt.as_deref(), Some("hello"));
        assert_eq!(
            cmd.images,
            vec![PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b.png")]
        );
    }

    #[test]
    fn debug_models_parses_bundled_flag() {
        let cli =
            MultitoolCli::try_parse_from(["codex", "debug", "models", "--bundled"]).expect("parse");

        let Some(Subcommand::Debug(DebugCommand {
            subcommand: DebugSubcommand::Models(cmd),
        })) = cli.subcommand
        else {
            panic!("expected debug models subcommand");
        };

        assert!(cmd.bundled);
    }

    #[test]
    fn responses_subcommand_is_not_registered() {
        let command = MultitoolCli::command();
        assert!(
            command
                .get_subcommands()
                .all(|subcommand| subcommand.get_name() != "responses")
        );
    }

    #[test]
    fn current_subcommands_have_no_removed_commands_or_aliases() {
        let command = MultitoolCli::command();
        for current in ["exec", "apply", "cloud"] {
            assert!(
                command.find_subcommand(current).is_some(),
                "missing {current}"
            );
        }
        for removed in ["app", "e", "a", "cloud-tasks"] {
            assert!(
                command.find_subcommand(removed).is_none(),
                "removed command or alias {removed} is still registered"
            );
        }
    }

    #[test]
    fn archive_merges_scoped_tui_flags() {
        let (target, interactive) = finalize_archive_from_args(
            [
                "codex",
                "-C",
                "/root",
                "archive",
                "--strict-config",
                "-m",
                "gpt-5.1-test",
                "-p",
                "work",
                "-C",
                "/archive",
                "my-thread",
            ]
            .as_ref(),
        );

        assert_eq!(target, "my-thread");
        assert_eq!(interactive.model.as_deref(), Some("gpt-5.1-test"));
        assert_eq!(interactive.config_profile_v2.as_deref(), Some("work"));
        assert_eq!(
            interactive.cwd.as_deref(),
            Some(std::path::Path::new("/archive"))
        );
        assert!(interactive.strict_config);
    }

    #[test]
    fn delete_force_requires_uuid() {
        assert!(delete_action("123e4567-e89b-12d3-a456-426614174000", /*force*/ true).is_ok());

        let err =
            delete_action("my-thread", /*force*/ true).expect_err("name should require prompt");
        assert_eq!(
            err.to_string(),
            "--force requires a session UUID; names must be confirmed interactively"
        );
    }

    fn sample_exit_info(conversation_id: Option<&str>, thread_name: Option<&str>) -> AppExitInfo {
        let token_usage = TokenUsage {
            output_tokens: 2,
            total_tokens: 2,
            ..Default::default()
        };
        let thread_id = conversation_id
            .map(ThreadId::from_string)
            .map(Result::unwrap);
        AppExitInfo {
            token_usage,
            thread_id,
            session_title: thread_name.map(str::to_string),
            resume_hint: codex_utils_cli::resume_hint(thread_name, thread_id),
            exit_reason: ExitReason::UserRequested,
        }
    }

    #[test]
    fn format_exit_messages_skips_zero_usage() {
        let exit_info = AppExitInfo {
            token_usage: TokenUsage::default(),
            thread_id: None,
            session_title: None,
            resume_hint: None,
            exit_reason: ExitReason::UserRequested,
        };
        let lines = format_exit_messages(exit_info, /*color_enabled*/ false);
        assert!(lines.is_empty());
    }

    #[test]
    fn format_exit_messages_includes_session_id_for_fatal_exit_without_resume_hint() {
        let exit_info = AppExitInfo {
            token_usage: TokenUsage::default(),
            thread_id: Some(ThreadId::from_string("123e4567-e89b-12d3-a456-426614174000").unwrap()),
            session_title: None,
            resume_hint: None,
            exit_reason: ExitReason::Fatal("boom".to_string()),
        };
        let lines = format_exit_messages(exit_info, /*color_enabled*/ false);
        assert_eq!(
            lines,
            vec!["Session ID: 123e4567-e89b-12d3-a456-426614174000".to_string()]
        );
    }

    #[test]
    fn format_exit_messages_includes_resume_hint_for_fatal_exit() {
        let mut exit_info = sample_exit_info(
            Some("123e4567-e89b-12d3-a456-426614174000"),
            /*thread_name*/ None,
        );
        exit_info.exit_reason = ExitReason::Fatal("boom".to_string());
        let lines = format_exit_messages(exit_info, /*color_enabled*/ false);
        assert_eq!(
            lines,
            vec![
                "".to_string(),
                "ycode".to_string(),
                "Continue  codex resume 123e4567-e89b-12d3-a456-426614174000".to_string(),
                "Usage     Token usage: total=2 input=0 output=2".to_string(),
                "".to_string(),
            ]
        );
    }

    #[test]
    fn format_exit_messages_includes_resume_hint_without_color() {
        let exit_info = sample_exit_info(
            Some("123e4567-e89b-12d3-a456-426614174000"),
            /*thread_name*/ None,
        );
        let lines = format_exit_messages(exit_info, /*color_enabled*/ false);
        assert_eq!(
            lines,
            vec![
                "".to_string(),
                "ycode".to_string(),
                "Continue  codex resume 123e4567-e89b-12d3-a456-426614174000".to_string(),
                "Usage     Token usage: total=2 input=0 output=2".to_string(),
                "".to_string(),
            ]
        );
    }

    #[test]
    fn format_exit_messages_applies_color_when_enabled() {
        let exit_info = sample_exit_info(
            Some("123e4567-e89b-12d3-a456-426614174000"),
            /*thread_name*/ None,
        );
        let lines = format_exit_messages(exit_info, /*color_enabled*/ true);
        assert_eq!(lines.len(), 5);
        assert!(lines[1].contains("\u{1b}[1m"));
        assert!(lines[2].contains("\u{1b}[36m"));
    }

    #[test]
    fn format_exit_messages_names_picker_item_when_thread_has_name() {
        let exit_info = sample_exit_info(
            Some("123e4567-e89b-12d3-a456-426614174000"),
            Some("my-thread"),
        );
        let lines = format_exit_messages(exit_info, /*color_enabled*/ false);
        assert_eq!(
            lines,
            vec![
                "".to_string(),
                "ycode".to_string(),
                "Session   my-thread".to_string(),
                "Continue  codex resume, then select my-thread (123e4567-e89b-12d3-a456-426614174000)".to_string(),
                "Usage     Token usage: total=2 input=0 output=2".to_string(),
                "".to_string(),
            ]
        );
    }

    #[test]
    fn resume_model_flag_applies_when_no_root_flags() {
        let interactive =
            finalize_resume_from_args(["codex", "resume", "-m", "gpt-5.1-test"].as_ref());

        assert_eq!(interactive.model.as_deref(), Some("gpt-5.1-test"));
        assert!(interactive.resume_picker);
        assert!(!interactive.resume_last);
        assert_eq!(interactive.resume_session_id, None);
    }

    #[test]
    fn resume_picker_logic_none_and_not_last() {
        let interactive = finalize_resume_from_args(["codex", "resume"].as_ref());
        assert!(interactive.resume_picker);
        assert!(!interactive.resume_last);
        assert_eq!(interactive.resume_session_id, None);
        assert!(!interactive.resume_show_all);
    }

    #[test]
    fn resume_picker_logic_last() {
        let interactive = finalize_resume_from_args(["codex", "resume", "--last"].as_ref());
        assert!(!interactive.resume_picker);
        assert!(interactive.resume_last);
        assert_eq!(interactive.resume_session_id, None);
        assert!(!interactive.resume_show_all);
    }

    #[test]
    fn resume_last_accepts_prompt_positional() {
        let interactive = finalize_resume_from_args(
            ["codex", "resume", "--last", "/compact focus on auth"].as_ref(),
        );

        assert!(!interactive.resume_picker);
        assert!(interactive.resume_last);
        assert_eq!(interactive.resume_session_id, None);
        assert_eq!(
            interactive.prompt.as_deref(),
            Some("/compact focus on auth")
        );
    }

    #[test]
    fn resume_last_rejects_explicit_session_and_prompt() {
        let err =
            MultitoolCli::try_parse_from(["codex", "resume", "--last", "1234", "continue here"])
                .expect_err("--last with an explicit session and prompt should be rejected");

        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn resume_picker_logic_with_session_id() {
        let interactive = finalize_resume_from_args(["codex", "resume", "1234"].as_ref());
        assert!(!interactive.resume_picker);
        assert!(!interactive.resume_last);
        assert_eq!(interactive.resume_session_id.as_deref(), Some("1234"));
        assert!(!interactive.resume_show_all);
    }

    #[test]
    fn resume_with_session_id_accepts_prompt_positional() {
        let interactive =
            finalize_resume_from_args(["codex", "resume", "1234", "continue here"].as_ref());

        assert!(!interactive.resume_picker);
        assert!(!interactive.resume_last);
        assert_eq!(interactive.resume_session_id.as_deref(), Some("1234"));
        assert_eq!(interactive.prompt.as_deref(), Some("continue here"));
    }

    #[test]
    fn resume_all_flag_sets_show_all() {
        let interactive = finalize_resume_from_args(["codex", "resume", "--all"].as_ref());
        assert!(interactive.resume_picker);
        assert!(interactive.resume_show_all);
    }

    #[test]
    fn resume_include_non_interactive_flag_sets_source_filter_override() {
        let interactive =
            finalize_resume_from_args(["codex", "resume", "--include-non-interactive"].as_ref());

        assert!(interactive.resume_picker);
        assert!(interactive.resume_include_non_interactive);
    }

    #[test]
    fn resume_merges_option_flags() {
        let interactive = finalize_resume_from_args(
            [
                "codex",
                "resume",
                "sid",
                "--search",
                "-m",
                "gpt-5.1-test",
                "-p",
                "my-config",
                "-C",
                "/tmp",
                "--strict-config",
                "-i",
                "/tmp/a.png,/tmp/b.png",
            ]
            .as_ref(),
        );

        assert_eq!(interactive.model.as_deref(), Some("gpt-5.1-test"));
        assert_eq!(interactive.config_profile_v2.as_deref(), Some("my-config"));
        assert_eq!(
            interactive.cwd.as_deref(),
            Some(std::path::Path::new("/tmp"))
        );
        assert!(interactive.web_search);
        assert!(interactive.strict_config);
        let has_a = interactive
            .images
            .iter()
            .any(|p| p == std::path::Path::new("/tmp/a.png"));
        let has_b = interactive
            .images
            .iter()
            .any(|p| p == std::path::Path::new("/tmp/b.png"));
        assert!(has_a && has_b);
        assert!(!interactive.resume_picker);
        assert!(!interactive.resume_last);
        assert_eq!(interactive.resume_session_id.as_deref(), Some("sid"));
    }

    #[test]
    fn fork_picker_logic_none_and_not_last() {
        let interactive = finalize_fork_from_args(["codex", "fork"].as_ref());
        assert!(interactive.fork_picker);
        assert!(!interactive.fork_last);
        assert_eq!(interactive.fork_session_id, None);
        assert!(!interactive.fork_show_all);
    }

    #[test]
    fn fork_picker_logic_last() {
        let interactive = finalize_fork_from_args(["codex", "fork", "--last"].as_ref());
        assert!(!interactive.fork_picker);
        assert!(interactive.fork_last);
        assert_eq!(interactive.fork_session_id, None);
        assert!(!interactive.fork_show_all);
    }

    #[test]
    fn fork_last_accepts_prompt_positional() {
        let interactive =
            finalize_fork_from_args(["codex", "fork", "--last", "/compact focus on auth"].as_ref());

        assert!(!interactive.fork_picker);
        assert!(interactive.fork_last);
        assert_eq!(interactive.fork_session_id, None);
        assert_eq!(
            interactive.prompt.as_deref(),
            Some("/compact focus on auth")
        );
    }

    #[test]
    fn fork_last_rejects_explicit_session_and_prompt() {
        let err =
            MultitoolCli::try_parse_from(["codex", "fork", "--last", "1234", "continue here"])
                .expect_err("--last with an explicit session and prompt should be rejected");

        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn fork_picker_logic_with_session_id() {
        let interactive = finalize_fork_from_args(["codex", "fork", "1234"].as_ref());
        assert!(!interactive.fork_picker);
        assert!(!interactive.fork_last);
        assert_eq!(interactive.fork_session_id.as_deref(), Some("1234"));
        assert!(!interactive.fork_show_all);
    }

    #[test]
    fn fork_with_session_id_accepts_prompt_positional() {
        let interactive =
            finalize_fork_from_args(["codex", "fork", "1234", "continue here"].as_ref());

        assert!(!interactive.fork_picker);
        assert!(!interactive.fork_last);
        assert_eq!(interactive.fork_session_id.as_deref(), Some("1234"));
        assert_eq!(interactive.prompt.as_deref(), Some("continue here"));
    }

    #[test]
    fn fork_all_flag_sets_show_all() {
        let interactive = finalize_fork_from_args(["codex", "fork", "--all"].as_ref());
        assert!(interactive.fork_picker);
        assert!(interactive.fork_show_all);
    }

    #[test]
    fn strict_config_parses_for_interactive_command() {
        let cli = MultitoolCli::try_parse_from(["codex", "--strict-config"]).expect("parse");
        assert!(cli.interactive.strict_config);
    }

    #[test]
    fn psp_is_a_global_runtime_argument() {
        for args in [
            ["codex", "--psp"].as_slice(),
            ["codex", "cli-runtime", "--psp"].as_slice(),
            ["codex", "remote-control", "--psp"].as_slice(),
        ] {
            let cli = MultitoolCli::try_parse_from(args).expect("parse runtime PSP flag");
            assert!(cli.psp);
            assert!(cli.config_overrides.raw_overrides.is_empty());
        }
    }

    #[test]
    fn features_enable_parses_feature_name() {
        let cli = MultitoolCli::try_parse_from(["codex", "features", "enable", "unified_exec"])
            .expect("parse should succeed");
        let Some(Subcommand::Features(FeaturesCli { sub })) = cli.subcommand else {
            panic!("expected features subcommand");
        };
        let FeaturesSubcommand::Enable(FeatureSetArgs { feature }) = sub else {
            panic!("expected features enable");
        };
        assert_eq!(feature, "unified_exec");
    }

    #[test]
    fn features_disable_parses_feature_name() {
        let cli = MultitoolCli::try_parse_from(["codex", "features", "disable", "shell_tool"])
            .expect("parse should succeed");
        let Some(Subcommand::Features(FeaturesCli { sub })) = cli.subcommand else {
            panic!("expected features subcommand");
        };
        let FeaturesSubcommand::Disable(FeatureSetArgs { feature }) = sub else {
            panic!("expected features disable");
        };
        assert_eq!(feature, "shell_tool");
    }

    #[test]
    fn feature_toggles_known_features_generate_overrides() {
        let toggles = FeatureToggles {
            enable: vec!["standalone_web_search".to_string()],
            disable: vec!["unified_exec".to_string()],
        };
        let overrides = toggles.to_overrides().expect("valid features");
        assert_eq!(
            overrides,
            vec![
                "features.standalone_web_search=true".to_string(),
                "features.unified_exec=false".to_string(),
            ]
        );
    }

    #[test]
    fn feature_toggles_unknown_feature_errors() {
        let toggles = FeatureToggles {
            enable: vec!["does_not_exist".to_string()],
            disable: Vec::new(),
        };
        let err = toggles
            .to_overrides()
            .expect_err("feature should be rejected");
        assert_eq!(err.to_string(), "Unknown feature flag: does_not_exist");
    }

    #[test]
    fn strict_config_with_unknown_enable_errors() {
        let err = strict_config_feature_toggle_error(["--enable", "does_not_exist"].as_ref());
        assert_eq!(err.to_string(), "Unknown feature flag: does_not_exist");
    }

    #[test]
    fn strict_config_with_unknown_disable_errors() {
        let err = strict_config_feature_toggle_error(["--disable", "does_not_exist"].as_ref());
        assert_eq!(err.to_string(), "Unknown feature flag: does_not_exist");
    }

    #[test]
    fn strict_config_with_compound_enable_errors() {
        let err = strict_config_feature_toggle_error(
            ["--enable", "multi_agent_v2.subagent_usage_hint_text"].as_ref(),
        );
        assert_eq!(
            err.to_string(),
            "Unknown feature flag: multi_agent_v2.subagent_usage_hint_text"
        );
    }

    fn strict_config_feature_toggle_error(args: &[&str]) -> anyhow::Error {
        let cli_args = std::iter::once("codex")
            .chain(std::iter::once("--strict-config"))
            .chain(args.iter().copied());
        let cli = MultitoolCli::try_parse_from(cli_args).expect("parse should succeed");
        assert!(cli.interactive.strict_config);
        cli.feature_toggles
            .to_overrides()
            .expect_err("feature should be rejected")
    }
}
