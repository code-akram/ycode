// - In the default output mode, it is paramount that the only thing written to
//   stdout is the final message (if any).
// - In --json mode, stdout must be valid JSONL, one event per line.
// For both modes, any other output must be written to stderr.
#![deny(clippy::print_stdout)]

mod cli;
mod event_processor;
mod event_processor_with_human_output;
pub(crate) mod event_processor_with_jsonl_output;
pub(crate) mod exec_events;

pub use cli::Cli;
pub use cli::Command;
use codex_arg0::Arg0DispatchPaths;
use codex_cli_protocol::ClientRequest;
use codex_cli_protocol::ConfigWarningNotification;
use codex_cli_protocol::JSONRPCErrorError;
use codex_cli_protocol::RequestId;
use codex_cli_protocol::ServerNotification;
use codex_cli_protocol::ServerRequest;
use codex_cli_protocol::Thread as CliRuntimeThread;
use codex_cli_protocol::ThreadItem as CliRuntimeThreadItem;
use codex_cli_protocol::ThreadListParams;
use codex_cli_protocol::ThreadListResponse;
use codex_cli_protocol::ThreadReadParams;
use codex_cli_protocol::ThreadReadResponse;
use codex_cli_protocol::ThreadResumeParams;
use codex_cli_protocol::ThreadResumeResponse;
use codex_cli_protocol::ThreadSortKey;
use codex_cli_protocol::ThreadSource;
use codex_cli_protocol::ThreadSourceKind;
use codex_cli_protocol::ThreadStartParams;
use codex_cli_protocol::ThreadStartResponse;
use codex_cli_protocol::ThreadUnsubscribeParams;
use codex_cli_protocol::ThreadUnsubscribeResponse;
use codex_cli_protocol::TurnInterruptParams;
use codex_cli_protocol::TurnInterruptResponse;
use codex_cli_protocol::TurnStartParams;
use codex_cli_protocol::TurnStartResponse;
use codex_cli_runtime_client::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY;
use codex_cli_runtime_client::EnvironmentManager;
use codex_cli_runtime_client::ExecServerRuntimePaths;
use codex_cli_runtime_client::InProcessCliRuntimeClient;
use codex_cli_runtime_client::InProcessClientStartArgs;
use codex_cli_runtime_client::InProcessServerEvent;
use codex_config::CloudConfigBundleLoader;
use codex_config::ConfigLoadError;
use codex_config::ConfigLoadOptions;
use codex_config::LoaderOverrides;
use codex_config::format_config_error_with_source;
use codex_core::StateDbHandle;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::config::ConfigTomlLoadResult;
use codex_core::config::find_codex_home;
use codex_core::config::load_config_toml_with_layer_stack;
use codex_core::config::resolve_profile_v2_config_path;
use codex_core::find_thread_meta_by_name_str;
use codex_core::path_utils;
use codex_core::read_session_meta_line;
use codex_git_utils::get_git_repo_root;
use codex_login::default_client::set_default_client_residency_requirement;
use codex_login::default_client::set_default_originator;
use codex_login::enforce_login_restrictions;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::canonicalize_existing_preserving_symlinks;
use codex_utils_cli::SharedCliOptions;
use event_processor_with_human_output::EventProcessorWithHumanOutput;
pub use event_processor_with_jsonl_output::CodexStatus;
pub use event_processor_with_jsonl_output::CollectedThreadEvents;
pub use event_processor_with_jsonl_output::EventProcessorWithJsonOutput;
pub use exec_events::AgentMessageItem;
pub use exec_events::CollabAgentState;
pub use exec_events::CollabAgentStatus;
pub use exec_events::CollabTool;
pub use exec_events::CollabToolCallItem;
pub use exec_events::CollabToolCallStatus;
pub use exec_events::CommandExecutionItem;
pub use exec_events::CommandExecutionStatus;
pub use exec_events::ErrorItem;
pub use exec_events::FileChangeItem;
pub use exec_events::FileUpdateChange;
pub use exec_events::ItemCompletedEvent;
pub use exec_events::ItemStartedEvent;
pub use exec_events::ItemUpdatedEvent;
pub use exec_events::PatchApplyStatus;
pub use exec_events::PatchChangeKind;
pub use exec_events::ReasoningItem;
pub use exec_events::ThreadErrorEvent;
pub use exec_events::ThreadEvent;
pub use exec_events::ThreadItem as ExecThreadItem;
pub use exec_events::ThreadItemDetails;
pub use exec_events::ThreadStartedEvent;
pub use exec_events::TodoItem;
pub use exec_events::TodoListItem;
pub use exec_events::TurnCompletedEvent;
pub use exec_events::TurnFailedEvent;
pub use exec_events::TurnStartedEvent;
pub use exec_events::Usage;
pub use exec_events::WebSearchItem;
use serde_json::Value;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use supports_color::Stream;
use tokio::sync::mpsc;
use tracing::Instrument;
use tracing::error;
use tracing::field;
use tracing::info;
use tracing::info_span;
use tracing::warn;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;
use uuid::Uuid;

use crate::cli::Command as ExecCommand;
use crate::event_processor::EventProcessor;

const EXEC_DEFAULT_LOG_FILTER: &str = "error";

enum InitialOperation {
    UserTurn {
        items: Vec<UserInput>,
        output_schema: Option<Value>,
    },
}

enum StdinPromptBehavior {
    /// Read stdin only when there is no positional prompt, which is the legacy
    /// `codex exec` behavior for `codex exec` with piped input.
    RequiredIfPiped,
    /// Always treat stdin as the prompt, used for the explicit `codex exec -`
    /// sentinel and similar forced-stdin call sites.
    Forced,
    /// If stdin is piped alongside a positional prompt, treat stdin as
    /// additional context to append rather than as the primary prompt.
    OptionalAppend,
}

struct RequestIdSequencer {
    next: i64,
}

impl RequestIdSequencer {
    fn new() -> Self {
        Self { next: 1 }
    }

    fn next(&mut self) -> RequestId {
        let id = self.next;
        self.next += 1;
        RequestId::Integer(id)
    }
}

struct ExecRunArgs {
    in_process_start_args: InProcessClientStartArgs,
    state_db: Option<StateDbHandle>,
    command: Option<ExecCommand>,
    config: Config,
    exec_span: tracing::Span,
    images: Vec<PathBuf>,
    json_mode: bool,
    last_message_file: Option<PathBuf>,
    output_schema_path: Option<PathBuf>,
    prompt: Option<String>,
    skip_git_repo_check: bool,
    stderr_with_ansi: bool,
}

fn exec_root_span() -> tracing::Span {
    info_span!(
        "codex.exec",
        thread.id = field::Empty,
        turn.id = field::Empty,
    )
}

fn exec_stderr_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(EXEC_DEFAULT_LOG_FILTER))
        .unwrap_or_else(|_| EnvFilter::new("error"))
}

pub async fn run_main(cli: Cli, arg0_paths: Arg0DispatchPaths) -> anyhow::Result<()> {
    if let Err(err) = set_default_originator("codex_exec".to_string()) {
        tracing::warn!(?err, "Failed to set codex exec originator override {err:?}");
    }

    let Cli {
        psp,
        command,
        strict_config,
        shared,
        skip_git_repo_check,
        ephemeral,
        ignore_user_config,
        color,
        last_message_file,
        json: json_mode,
        prompt,
        output_schema: output_schema_path,
        mut config_overrides,
    } = cli;
    let shared = shared.into_inner();
    let SharedCliOptions {
        images,
        model: model_cli_arg,
        config_profile_v2,
        cwd,
    } = shared;

    let (_stdout_with_ansi, stderr_with_ansi) = match color {
        cli::Color::Always => (true, true),
        cli::Color::Never => (false, false),
        cli::Color::Auto => (
            supports_color::on_cached(Stream::Stdout).is_some(),
            supports_color::on_cached(Stream::Stderr).is_some(),
        ),
    };
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(stderr_with_ansi)
        .with_writer(std::io::stderr)
        .with_filter(exec_stderr_env_filter());

    // Parse `-c` overrides from the CLI.
    let cli_kv_overrides = match config_overrides.parse_overrides() {
        Ok(v) => v,
        #[allow(clippy::print_stderr)]
        Err(e) => {
            eprintln!("Error parsing -c overrides: {e}");
            std::process::exit(1);
        }
    };

    let resolved_cwd = cwd.clone();
    let config_cwd = match resolved_cwd.as_deref() {
        Some(path) => {
            AbsolutePathBuf::from_absolute_path(canonicalize_existing_preserving_symlinks(path)?)?
        }
        None => AbsolutePathBuf::current_dir()?,
    };

    // we load config.toml here to determine project state.
    #[allow(clippy::print_stderr)]
    let codex_home = match find_codex_home() {
        Ok(codex_home) => codex_home,
        Err(err) => {
            eprintln!("Error finding codex home: {err}");
            std::process::exit(1);
        }
    };
    let user_config_path = config_profile_v2
        .as_ref()
        .map(|profile_v2| resolve_profile_v2_config_path(&codex_home, profile_v2));
    let loader_overrides = LoaderOverrides {
        user_config_path,
        user_config_profile: config_profile_v2,
        ignore_user_config,
        ..Default::default()
    };

    let bootstrap_config = load_bootstrap_config_or_exit(
        &codex_home,
        Some(&config_cwd),
        cli_kv_overrides.clone(),
        loader_overrides.clone(),
        strict_config,
        CloudConfigBundleLoader::default(),
    )
    .await;
    let _bootstrap_config_toml = &bootstrap_config.config_toml;
    let cloud_config_bundle = CloudConfigBundleLoader::default();
    let run_cli_overrides = cli_kv_overrides.clone();
    let run_loader_overrides = loader_overrides.clone();
    let run_cloud_config_bundle = cloud_config_bundle.clone();

    let model = model_cli_arg;

    let overrides = ConfigOverrides {
        model,
        review_model: None,
        cwd: resolved_cwd,
        workspace_roots: None,
        service_tier: None,
        codex_self_exe: arg0_paths.codex_self_exe.clone(),
        main_execve_wrapper_exe: arg0_paths.main_execve_wrapper_exe.clone(),
        default_zsh_path: None,
        base_instructions: None,
        developer_instructions: None,
        personality: None,
        compact_prompt: None,
        ephemeral: ephemeral.then_some(true),
        psp: Some(psp),
        ..Default::default()
    };

    let build_config = |overrides| {
        ConfigBuilder::default()
            .codex_home(codex_home.to_path_buf())
            .cli_overrides(cli_kv_overrides.clone())
            .harness_overrides(overrides)
            .loader_overrides(loader_overrides.clone())
            .strict_config(strict_config)
            .cloud_config_bundle(cloud_config_bundle.clone())
            .build()
    };
    let config = build_config(overrides).await?;
    set_default_client_residency_requirement(config.enforce_residency.value());

    if let Err(err) = enforce_login_restrictions(&config.auth_config()).await {
        eprintln!("{err}");
        std::process::exit(1);
    }

    let _ = tracing_subscriber::registry().with(fmt_layer).try_init();

    let exec_span = exec_root_span();
    let config_warnings: Vec<ConfigWarningNotification> = config
        .startup_warnings
        .iter()
        .map(|warning| ConfigWarningNotification {
            summary: warning.clone(),
            details: None,
            path: None,
            range: None,
        })
        .collect();
    let local_runtime_paths =
        ExecServerRuntimePaths::from_optional_path(arg0_paths.codex_self_exe.clone())?;
    let state_db = codex_core::init_state_db(&config).await;
    let environment_manager = if run_loader_overrides.ignore_user_config {
        EnvironmentManager::from_env(Some(local_runtime_paths), config.http_client_factory())
            .await?
    } else {
        EnvironmentManager::from_codex_home(
            config.codex_home.clone(),
            Some(local_runtime_paths),
            config.http_client_factory(),
        )
        .await?
    };
    let in_process_start_args = InProcessClientStartArgs {
        arg0_paths,
        config: std::sync::Arc::new(config.clone()),
        cli_overrides: run_cli_overrides,
        loader_overrides: run_loader_overrides,
        strict_config,
        cloud_config_bundle: run_cloud_config_bundle,
        log_db: None,
        state_db: state_db.clone(),
        environment_manager: std::sync::Arc::new(environment_manager),
        config_warnings,
        session_source: SessionSource::Exec,
        enable_codex_api_key_env: true,
        client_name: "codex_exec".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        experimental_api: true,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
    };
    run_exec_session(ExecRunArgs {
        in_process_start_args,
        state_db,
        command,
        config,
        exec_span: exec_span.clone(),
        images,
        json_mode,
        last_message_file,
        output_schema_path,
        prompt,
        skip_git_repo_check,
        stderr_with_ansi,
    })
    .instrument(exec_span)
    .await
}

#[allow(clippy::print_stderr)]
async fn load_bootstrap_config_or_exit(
    codex_home: &Path,
    cwd: Option<&AbsolutePathBuf>,
    cli_kv_overrides: Vec<(String, codex_config::TomlValue)>,
    loader_overrides: LoaderOverrides,
    strict_config: bool,
    cloud_config_bundle: CloudConfigBundleLoader,
) -> ConfigTomlLoadResult {
    match load_config_toml_with_layer_stack(
        codex_home,
        cwd,
        cli_kv_overrides,
        ConfigLoadOptions {
            loader_overrides,
            strict_config,
            cloud_config_bundle,
        },
    )
    .await
    {
        Ok(config_toml) => config_toml,
        Err(err) => {
            let config_error = err
                .get_ref()
                .and_then(|err| err.downcast_ref::<ConfigLoadError>())
                .map(ConfigLoadError::config_error);
            if let Some(config_error) = config_error {
                eprintln!(
                    "Error loading config.toml:\n{}",
                    format_config_error_with_source(config_error)
                );
            } else {
                eprintln!("Error loading config.toml: {err}");
            }
            std::process::exit(1);
        }
    }
}

async fn run_exec_session(args: ExecRunArgs) -> anyhow::Result<()> {
    let ExecRunArgs {
        in_process_start_args,
        state_db,
        command,
        config,
        exec_span,
        images,
        json_mode,
        last_message_file,
        output_schema_path,
        prompt,
        skip_git_repo_check,
        stderr_with_ansi,
    } = args;

    let mut event_processor: Box<dyn EventProcessor> = match json_mode {
        true => Box::new(EventProcessorWithJsonOutput::new(last_message_file.clone())),
        _ => Box::new(EventProcessorWithHumanOutput::create_with_ansi(
            stderr_with_ansi,
            &config,
            last_message_file.clone(),
        )),
    };
    let default_cwd = config.cwd.to_path_buf();
    let default_effort = config.model_reasoning_effort.clone();

    let (initial_operation, prompt_summary) = match (command.as_ref(), prompt, images) {
        (Some(ExecCommand::Resume(args)), root_prompt, imgs) => {
            let prompt_arg = args
                .prompt
                .clone()
                .or_else(|| {
                    if args.last {
                        args.session_id.clone()
                    } else {
                        None
                    }
                })
                .or(root_prompt);
            let prompt_text = resolve_prompt(prompt_arg);
            let mut items: Vec<UserInput> = imgs
                .into_iter()
                .chain(args.images.iter().cloned())
                .map(|path| UserInput::LocalImage { path, detail: None })
                .collect();
            items.push(UserInput::Text {
                text: prompt_text.clone(),
                // CLI input doesn't track UI element ranges, so none are available here.
                text_elements: Vec::new(),
            });
            let output_schema = load_output_schema(output_schema_path.clone());
            (
                InitialOperation::UserTurn {
                    items,
                    output_schema,
                },
                prompt_text,
            )
        }
        (None, root_prompt, imgs) => {
            let prompt_text = resolve_root_prompt(root_prompt);
            let mut items: Vec<UserInput> = imgs
                .into_iter()
                .map(|path| UserInput::LocalImage { path, detail: None })
                .collect();
            items.push(UserInput::Text {
                text: prompt_text.clone(),
                // CLI input doesn't track UI element ranges, so none are available here.
                text_elements: Vec::new(),
            });
            let output_schema = load_output_schema(output_schema_path);
            (
                InitialOperation::UserTurn {
                    items,
                    output_schema,
                },
                prompt_text,
            )
        }
    };

    if !skip_git_repo_check && get_git_repo_root(&default_cwd).is_none() {
        eprintln!("Not inside a trusted directory and --skip-git-repo-check was not specified.");
        std::process::exit(1);
    }

    let mut request_ids = RequestIdSequencer::new();
    let mut client = InProcessCliRuntimeClient::start(in_process_start_args)
        .await
        .map_err(|err| {
            anyhow::anyhow!("failed to initialize in-process cli-runtime client: {err}")
        })?;

    // Handle resume subcommand through existing `thread/list` + `thread/resume`
    // APIs so exec no longer reaches into rollout storage directly.
    let (primary_thread_id, fallback_session_configured) = if let Some(ExecCommand::Resume(args)) =
        command.as_ref()
    {
        if let Some(thread_id) =
            resolve_resume_thread_id(&client, &config, state_db.as_ref(), args).await?
        {
            let response: ThreadResumeResponse = send_request_with_response(
                &client,
                ClientRequest::ThreadResume {
                    request_id: request_ids.next(),
                    params: thread_resume_params_from_config(&config, thread_id),
                },
                "thread/resume",
            )
            .await
            .map_err(anyhow::Error::msg)?;
            let session_configured =
                session_configured_from_thread_resume_response(&response, &config)
                    .map_err(anyhow::Error::msg)?;
            (session_configured.thread_id, session_configured)
        } else {
            let response: ThreadStartResponse = send_request_with_response(
                &client,
                ClientRequest::ThreadStart {
                    request_id: request_ids.next(),
                    params: thread_start_params_from_config(&config),
                },
                "thread/start",
            )
            .await
            .map_err(anyhow::Error::msg)?;
            let session_configured =
                session_configured_from_thread_start_response(&response, &config)
                    .map_err(anyhow::Error::msg)?;
            (session_configured.thread_id, session_configured)
        }
    } else {
        let response: ThreadStartResponse = send_request_with_response(
            &client,
            ClientRequest::ThreadStart {
                request_id: request_ids.next(),
                params: thread_start_params_from_config(&config),
            },
            "thread/start",
        )
        .await
        .map_err(anyhow::Error::msg)?;
        let session_configured = session_configured_from_thread_start_response(&response, &config)
            .map_err(anyhow::Error::msg)?;
        (session_configured.thread_id, session_configured)
    };

    let primary_thread_id_for_span = primary_thread_id.to_string();
    // Use the start/resume response as the authoritative bootstrap payload.
    // Waiting for a later streamed `SessionConfigured` event adds up to 10s of
    // avoidable startup latency on the in-process path.
    let session_configured = fallback_session_configured;

    exec_span.record("thread.id", primary_thread_id_for_span.as_str());

    // Print the effective configuration and initial request so users can see what Codex
    // is using.
    event_processor.print_config_summary(&config, &prompt_summary, &session_configured);
    info!("Codex initialized with event: {session_configured:?}");

    let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::debug!("Keyboard interrupt");
            let _ = interrupt_tx.send(());
        }
    });

    let task_id = match initial_operation {
        InitialOperation::UserTurn {
            items,
            output_schema,
        } => {
            let response: TurnStartResponse = send_request_with_response(
                &client,
                ClientRequest::TurnStart {
                    request_id: request_ids.next(),
                    params: TurnStartParams {
                        thread_id: primary_thread_id_for_span.clone(),
                        client_user_message_id: None,
                        input: items.into_iter().map(Into::into).collect(),
                        responsesapi_client_metadata: None,
                        additional_context: None,
                        environments: None,
                        cwd: Some(default_cwd),
                        runtime_workspace_roots: None,
                        model: None,
                        service_tier: None,
                        effort: default_effort,
                        summary: None,
                        personality: None,
                        output_schema,
                        agent_settings: None,
                        multi_agent_mode: None,
                    },
                },
                "turn/start",
            )
            .await
            .map_err(anyhow::Error::msg)?;
            let task_id = response.turn.id;
            info!("Sent prompt with event ID: {task_id}");
            task_id
        }
    };
    exec_span.record("turn.id", task_id.as_str());

    // Run the loop until the task is complete.
    // Track whether a fatal error was reported by the server so we can
    // exit with a non-zero status for automation-friendly signaling.
    let mut error_seen = false;
    let mut interrupt_channel_open = true;
    let primary_thread_id_for_requests = primary_thread_id.to_string();
    loop {
        let server_event = tokio::select! {
            maybe_interrupt = interrupt_rx.recv(), if interrupt_channel_open => {
                if maybe_interrupt.is_none() {
                    interrupt_channel_open = false;
                    continue;
                }
                if let Err(err) = send_request_with_response::<TurnInterruptResponse>(
                    &client,
                    ClientRequest::TurnInterrupt {
                        request_id: request_ids.next(),
                        params: TurnInterruptParams {
                            thread_id: primary_thread_id_for_requests.clone(),
                            turn_id: task_id.clone(),
                        },
                    },
                    "turn/interrupt",
                )
                .await
                {
                    warn!("turn/interrupt failed: {err}");
                }
                continue;
            }
            maybe_event = client.next_event() => maybe_event,
        };

        let Some(server_event) = server_event else {
            break;
        };

        match server_event {
            InProcessServerEvent::ServerRequest(request) => {
                handle_server_request(&client, *request, &mut error_seen).await;
            }
            InProcessServerEvent::ServerNotification(notification) => {
                let mut notification = *notification;
                if let ServerNotification::Error(payload) = &notification {
                    if payload.thread_id == primary_thread_id_for_requests
                        && payload.turn_id == task_id
                        && !payload.will_retry
                    {
                        error_seen = true;
                    }
                } else if let ServerNotification::TurnCompleted(payload) = &notification
                    && payload.thread_id == primary_thread_id_for_requests
                    && payload.turn.id == task_id
                    && matches!(
                        payload.turn.status,
                        codex_cli_protocol::TurnStatus::Failed
                            | codex_cli_protocol::TurnStatus::Interrupted
                    )
                {
                    error_seen = true;
                }

                if should_process_notification(
                    &notification,
                    &primary_thread_id_for_requests,
                    &task_id,
                ) {
                    maybe_backfill_turn_completed_items(
                        config.ephemeral,
                        &client,
                        &mut request_ids,
                        &mut notification,
                    )
                    .await;

                    match event_processor.process_server_notification(notification) {
                        CodexStatus::Running => {}
                        CodexStatus::InitiateShutdown => {
                            if let Err(err) = request_shutdown(
                                &client,
                                &mut request_ids,
                                &primary_thread_id_for_requests,
                            )
                            .await
                            {
                                warn!("thread/unsubscribe failed during shutdown: {err}");
                            }
                            break;
                        }
                    }
                }
            }
            InProcessServerEvent::Lagged { skipped } => {
                let message = lagged_event_warning_message(skipped);
                warn!("{message}");
                event_processor.process_warning(message);
            }
        }
    }

    if let Err(err) = client.shutdown().await {
        warn!("in-process cli-runtime shutdown failed: {err}");
    }
    event_processor.print_final_output();
    if error_seen {
        std::process::exit(1);
    }

    Ok(())
}

fn thread_start_params_from_config(config: &Config) -> ThreadStartParams {
    ThreadStartParams {
        model: config.model.clone(),
        cwd: Some(config.cwd.to_string_lossy().to_string()),
        runtime_workspace_roots: Some(config.workspace_roots.clone()),
        config: thread_config_overrides_from_config(config),
        ephemeral: Some(config.ephemeral),
        thread_source: Some(ThreadSource::User),
        ..ThreadStartParams::default()
    }
}

fn thread_resume_params_from_config(config: &Config, thread_id: String) -> ThreadResumeParams {
    ThreadResumeParams {
        thread_id,
        model: config.model.clone(),
        cwd: Some(config.cwd.to_string_lossy().to_string()),
        runtime_workspace_roots: Some(config.workspace_roots.clone()),
        config: thread_config_overrides_from_config(config),
        exclude_turns: true,
        ..ThreadResumeParams::default()
    }
}

fn thread_config_overrides_from_config(_config: &Config) -> Option<HashMap<String, Value>> {
    None
}

async fn send_request_with_response<T>(
    client: &InProcessCliRuntimeClient,
    request: ClientRequest,
    method: &str,
) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
{
    client.request_typed(request).await.map_err(|err| {
        if method.is_empty() {
            err.to_string()
        } else {
            format!("{method}: {err}")
        }
    })
}

fn session_configured_from_thread_start_response(
    response: &ThreadStartResponse,
    config: &Config,
) -> Result<SessionConfiguredEvent, String> {
    session_configured_from_thread_response(
        &response.thread.session_id,
        &response.thread.id,
        response.thread.parent_thread_id.as_deref(),
        response.thread.thread_source.clone().map(Into::into),
        response.thread.name.clone(),
        response.thread.path.clone(),
        response.model.clone(),
        response.model_provider.clone(),
        response.service_tier.clone(),
        response.cwd.clone(),
        response.reasoning_effort.clone(),
    )
}

fn session_configured_from_thread_resume_response(
    response: &ThreadResumeResponse,
    config: &Config,
) -> Result<SessionConfiguredEvent, String> {
    session_configured_from_thread_response(
        &response.thread.session_id,
        &response.thread.id,
        response.thread.parent_thread_id.as_deref(),
        response.thread.thread_source.clone().map(Into::into),
        response.thread.name.clone(),
        response.thread.path.clone(),
        response.model.clone(),
        response.model_provider.clone(),
        response.service_tier.clone(),
        response.cwd.clone(),
        response.reasoning_effort.clone(),
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "session mapping keeps explicit fields"
)]
fn session_configured_from_thread_response(
    session_id: &str,
    thread_id: &str,
    parent_thread_id: Option<&str>,
    thread_source: Option<codex_protocol::protocol::ThreadSource>,
    thread_name: Option<String>,
    rollout_path: Option<PathBuf>,
    model: String,
    model_provider_id: String,
    service_tier: Option<String>,
    cwd: AbsolutePathBuf,
    reasoning_effort: Option<codex_protocol::openai_models::ReasoningEffort>,
) -> Result<SessionConfiguredEvent, String> {
    let session_id = SessionId::from_string(session_id)
        .map_err(|err| format!("session id `{session_id}` is invalid: {err}"))?;
    let thread_id = ThreadId::from_string(thread_id)
        .map_err(|err| format!("thread id `{thread_id}` is invalid: {err}"))?;
    let parent_thread_id = parent_thread_id
        .map(ThreadId::from_string)
        .transpose()
        .map_err(|err| format!("parent thread id is invalid: {err}"))?;

    Ok(SessionConfiguredEvent {
        session_id,
        thread_id,
        forked_from_id: None,
        parent_thread_id,
        thread_source,
        thread_name,
        model,
        model_provider_id,
        service_tier,
        cwd,
        reasoning_effort,
        initial_messages: None,
        rollout_path,
    })
}

fn lagged_event_warning_message(skipped: usize) -> String {
    format!("in-process cli-runtime event stream lagged; dropped {skipped} events")
}

fn should_process_notification(
    notification: &ServerNotification,
    thread_id: &str,
    turn_id: &str,
) -> bool {
    match notification {
        ServerNotification::ConfigWarning(_) | ServerNotification::DeprecationNotice(_) => true,
        // TODO(anp) resolve duplicate startup warnings
        ServerNotification::Warning(notification) => notification
            .thread_id
            .as_deref()
            .is_none_or(|candidate| candidate == thread_id),
        ServerNotification::Error(notification) => {
            notification.thread_id == thread_id && notification.turn_id == turn_id
        }
        ServerNotification::ItemCompleted(notification) => {
            notification.thread_id == thread_id && notification.turn_id == turn_id
        }
        ServerNotification::ItemStarted(notification) => {
            notification.thread_id == thread_id && notification.turn_id == turn_id
        }
        ServerNotification::ModelRerouted(notification) => {
            notification.thread_id == thread_id && notification.turn_id == turn_id
        }
        ServerNotification::ModelVerification(notification) => {
            notification.thread_id == thread_id && notification.turn_id == turn_id
        }
        ServerNotification::ThreadTokenUsageUpdated(notification) => {
            notification.thread_id == thread_id && notification.turn_id == turn_id
        }
        ServerNotification::TurnCompleted(notification) => {
            notification.thread_id == thread_id && notification.turn.id == turn_id
        }
        ServerNotification::TurnDiffUpdated(notification) => {
            notification.thread_id == thread_id && notification.turn_id == turn_id
        }
        ServerNotification::TurnPlanUpdated(notification) => {
            notification.thread_id == thread_id && notification.turn_id == turn_id
        }
        ServerNotification::TurnStarted(notification) => {
            notification.thread_id == thread_id && notification.turn.id == turn_id
        }
        _ => false,
    }
}

async fn maybe_backfill_turn_completed_items(
    thread_ephemeral: bool,
    client: &InProcessCliRuntimeClient,
    request_ids: &mut RequestIdSequencer,
    notification: &mut ServerNotification,
) {
    // In-process delivery may drop non-terminal item notifications under backpressure while still
    // guaranteeing `turn/completed`. Because cli-runtime currently emits that completion with an
    // empty `turn.items`, exec does one last `thread/read` here so human/json output can recover
    // the final message and reconcile any still-running items before shutdown.
    if !should_backfill_turn_completed_items(thread_ephemeral, notification) {
        return;
    }

    let ServerNotification::TurnCompleted(payload) = notification else {
        return;
    };

    let response = send_request_with_response::<ThreadReadResponse>(
        client,
        ClientRequest::ThreadRead {
            request_id: request_ids.next(),
            params: ThreadReadParams {
                thread_id: payload.thread_id.clone(),
                include_turns: true,
            },
        },
        "thread/read",
    )
    .await;

    match response {
        Ok(response) => {
            if let Some(items) = turn_items_for_thread(&response.thread, &payload.turn.id) {
                payload.turn.items = items;
            }
        }
        Err(err) => {
            warn!("thread/read failed while backfilling turn items for turn completion: {err}");
        }
    }
}

/// Returns true only when `exec` can safely recover missing turn items from
/// rollout-backed thread history.
fn should_backfill_turn_completed_items(
    thread_ephemeral: bool,
    notification: &ServerNotification,
) -> bool {
    let ServerNotification::TurnCompleted(payload) = notification else {
        return false;
    };

    !thread_ephemeral && payload.turn.items_view != codex_cli_protocol::TurnItemsView::Full
}

fn turn_items_for_thread(
    thread: &CliRuntimeThread,
    turn_id: &str,
) -> Option<Vec<CliRuntimeThreadItem>> {
    thread
        .turns
        .iter()
        .find(|turn| turn.id == turn_id)
        .map(|turn| turn.items.clone())
}

fn all_thread_source_kinds() -> Vec<ThreadSourceKind> {
    vec![
        ThreadSourceKind::Cli,
        ThreadSourceKind::VsCode,
        ThreadSourceKind::Exec,
        ThreadSourceKind::CliRuntime,
        ThreadSourceKind::SubAgent,
        ThreadSourceKind::SubAgentReview,
        ThreadSourceKind::SubAgentCompact,
        ThreadSourceKind::SubAgentThreadSpawn,
        ThreadSourceKind::SubAgentOther,
        ThreadSourceKind::Unknown,
    ]
}

async fn latest_thread_cwd(thread: &CliRuntimeThread) -> PathBuf {
    if let Some(path) = thread.path.as_deref()
        && let Some(cwd) = parse_latest_turn_context_cwd(path).await
    {
        return cwd;
    }
    thread.cwd.to_path_buf()
}

async fn parse_latest_turn_context_cwd(path: &Path) -> Option<PathBuf> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    for line in text.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(rollout_line) = serde_json::from_str::<RolloutLine>(trimmed) else {
            continue;
        };
        if let RolloutItem::TurnContext(item) = rollout_line.item {
            return Some(item.cwd.into_path_buf());
        }
    }
    None
}

fn cwds_match(current_cwd: &Path, session_cwd: &Path) -> bool {
    path_utils::paths_match_after_normalization(current_cwd, session_cwd)
}

async fn resolve_resume_thread_id(
    client: &InProcessCliRuntimeClient,
    config: &Config,
    state_db: Option<&StateDbHandle>,
    args: &crate::cli::ResumeArgs,
) -> anyhow::Result<Option<String>> {
    let model_providers = resume_lookup_model_providers(config, args);

    if args.last {
        let mut use_state_db_only = state_db.is_some();
        let mut cursor = None;
        loop {
            let response: ThreadListResponse = send_request_with_response(
                client,
                ClientRequest::ThreadList {
                    request_id: RequestId::Integer(0),
                    params: ThreadListParams {
                        cursor,
                        limit: Some(100),
                        sort_key: Some(ThreadSortKey::UpdatedAt),
                        sort_direction: None,
                        model_providers: model_providers.clone(),
                        source_kinds: Some(all_thread_source_kinds()),
                        archived: Some(false),
                        section_id: None,
                        parent_thread_id: None,
                        ancestor_thread_id: None,
                        cwd: None,
                        use_state_db_only,
                        search_term: None,
                    },
                },
                "thread/list",
            )
            .await
            .map_err(anyhow::Error::msg)?;
            for thread in response.data {
                if use_state_db_only && let Some(path) = thread.path.as_deref() {
                    let Ok(session_meta) = read_session_meta_line(path).await else {
                        continue;
                    };
                    if session_meta.meta.id.to_string() != thread.id {
                        continue;
                    }
                }
                let latest_cwd = latest_thread_cwd(&thread).await;
                if args.all || cwds_match(config.cwd.as_path(), latest_cwd.as_path()) {
                    // A usable SQLite candidate is authoritative. Scanning is reserved for a
                    // complete miss so successful `--last` lookups avoid auditing every rollout.
                    return Ok(Some(thread.id));
                }
            }
            let Some(next_cursor) = response.next_cursor else {
                if use_state_db_only {
                    // Repair from rollouts before giving up on a missing SQLite match.
                    use_state_db_only = false;
                    cursor = None;
                    continue;
                }
                return Ok(None);
            };
            cursor = Some(next_cursor);
        }
    }

    let Some(session_id) = args.session_id.as_deref() else {
        return Ok(None);
    };
    if Uuid::parse_str(session_id).is_ok() {
        return Ok(Some(session_id.to_string()));
    }
    if let Some(state_db) = state_db {
        let cwd = (!args.all).then_some(config.cwd.as_path());
        let resolved = state_db
            .find_thread_by_exact_title(
                session_id,
                &[],
                /*model_providers*/ None,
                /*archived_only*/ false,
                cwd,
            )
            .await?;
        if let Some(thread) = resolved {
            return Ok(Some(thread.id.to_string()));
        }
        if let Some((_, session_meta)) =
            find_thread_meta_by_name_str(&config.codex_home, session_id, Some(state_db.as_ref()))
                .await?
            && (args.all || cwds_match(config.cwd.as_path(), &session_meta.meta.cwd))
        {
            return Ok(Some(session_meta.meta.id.to_string()));
        }
    }

    let mut cursor = None;
    loop {
        let response: ThreadListResponse = send_request_with_response(
            client,
            ClientRequest::ThreadList {
                request_id: RequestId::Integer(0),
                params: ThreadListParams {
                    cursor,
                    limit: Some(100),
                    sort_key: Some(ThreadSortKey::UpdatedAt),
                    sort_direction: None,
                    model_providers: model_providers.clone(),
                    source_kinds: Some(all_thread_source_kinds()),
                    archived: Some(false),
                    section_id: None,
                    parent_thread_id: None,
                    ancestor_thread_id: None,
                    cwd: None,
                    use_state_db_only: false,
                    search_term: Some(session_id.to_string()),
                },
            },
            "thread/list",
        )
        .await
        .map_err(anyhow::Error::msg)?;
        for thread in response.data {
            if thread.name.as_deref() != Some(session_id) {
                continue;
            }
            let latest_cwd = latest_thread_cwd(&thread).await;
            if args.all || cwds_match(config.cwd.as_path(), latest_cwd.as_path()) {
                return Ok(Some(thread.id));
            }
        }
        let Some(next_cursor) = response.next_cursor else {
            return Ok(None);
        };
        cursor = Some(next_cursor);
    }
}

fn resume_lookup_model_providers(
    config: &Config,
    args: &crate::cli::ResumeArgs,
) -> Option<Vec<String>> {
    if args.last {
        Some(vec![config.model_provider_id.clone()])
    } else {
        None
    }
}

async fn request_shutdown(
    client: &InProcessCliRuntimeClient,
    request_ids: &mut RequestIdSequencer,
    thread_id: &str,
) -> Result<(), String> {
    let request = ClientRequest::ThreadUnsubscribe {
        request_id: request_ids.next(),
        params: ThreadUnsubscribeParams {
            thread_id: thread_id.to_string(),
        },
    };
    send_request_with_response::<ThreadUnsubscribeResponse>(client, request, "thread/unsubscribe")
        .await
        .map(|_| ())
}

async fn resolve_server_request(
    client: &InProcessCliRuntimeClient,
    request_id: RequestId,
    value: serde_json::Value,
    method: &str,
) -> Result<(), String> {
    client
        .resolve_server_request(request_id, value)
        .await
        .map_err(|err| format!("failed to resolve `{method}` server request: {err}"))
}

async fn reject_server_request(
    client: &InProcessCliRuntimeClient,
    request_id: RequestId,
    method: &str,
    reason: String,
) -> Result<(), String> {
    client
        .reject_server_request(
            request_id,
            JSONRPCErrorError {
                code: -32000,
                message: reason,
                data: None,
            },
        )
        .await
        .map_err(|err| format!("failed to reject `{method}` server request: {err}"))
}

fn server_request_method_name(request: &ServerRequest) -> String {
    serde_json::to_value(request)
        .ok()
        .and_then(|value| {
            value
                .get("method")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_string())
}

async fn handle_server_request(
    client: &InProcessCliRuntimeClient,
    request: ServerRequest,
    error_seen: &mut bool,
) {
    let method = server_request_method_name(&request);
    let handle_result = match request {
        ServerRequest::ToolRequestUserInput { request_id, params } => {
            reject_server_request(
                client,
                request_id,
                &method,
                format!(
                    "request_user_input is not supported in exec mode for thread `{}`",
                    params.thread_id
                ),
            )
            .await
        }
        ServerRequest::DynamicToolCall { request_id, params } => {
            reject_server_request(
                client,
                request_id,
                &method,
                format!(
                    "dynamic tool calls are not supported in exec mode for thread `{}`",
                    params.thread_id
                ),
            )
            .await
        }
        ServerRequest::ChatgptAuthTokensRefresh { request_id, .. } => {
            reject_server_request(
                client,
                request_id,
                &method,
                "chatgpt auth token refresh is not supported in exec mode".to_string(),
            )
            .await
        }
        ServerRequest::AttestationGenerate { request_id, .. } => {
            reject_server_request(
                client,
                request_id,
                &method,
                "attestation generation is not supported in exec mode".to_string(),
            )
            .await
        }
        ServerRequest::CurrentTimeRead { request_id, .. } => {
            reject_server_request(
                client,
                request_id,
                &method,
                "external current time is not supported in exec mode".to_string(),
            )
            .await
        }
    };

    if let Err(err) = handle_result {
        *error_seen = true;
        warn!("{err}");
    }
}

fn load_output_schema(path: Option<PathBuf>) -> Option<Value> {
    let path = path?;

    let schema_str = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) => {
            eprintln!(
                "Failed to read output schema file {}: {err}",
                path.display()
            );
            std::process::exit(1);
        }
    };

    match serde_json::from_str::<Value>(&schema_str) {
        Ok(value) => Some(value),
        Err(err) => {
            eprintln!(
                "Output schema file {} is not valid JSON: {err}",
                path.display()
            );
            std::process::exit(1);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PromptDecodeError {
    InvalidUtf8 { valid_up_to: usize },
    InvalidUtf16 { encoding: &'static str },
    UnsupportedBom { encoding: &'static str },
}

impl std::fmt::Display for PromptDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromptDecodeError::InvalidUtf8 { valid_up_to } => write!(
                f,
                "input is not valid UTF-8 (invalid byte at offset {valid_up_to}). Convert it to UTF-8 and retry (e.g., `iconv -f <ENC> -t UTF-8 prompt.txt`)."
            ),
            PromptDecodeError::InvalidUtf16 { encoding } => write!(
                f,
                "input looked like {encoding} but could not be decoded. Convert it to UTF-8 and retry."
            ),
            PromptDecodeError::UnsupportedBom { encoding } => write!(
                f,
                "input appears to be {encoding}. Convert it to UTF-8 and retry."
            ),
        }
    }
}

fn decode_prompt_bytes(input: &[u8]) -> Result<String, PromptDecodeError> {
    let input = input.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(input);

    if input.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) {
        return Err(PromptDecodeError::UnsupportedBom {
            encoding: "UTF-32LE",
        });
    }

    if input.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) {
        return Err(PromptDecodeError::UnsupportedBom {
            encoding: "UTF-32BE",
        });
    }

    if let Some(rest) = input.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, "UTF-16LE", u16::from_le_bytes);
    }

    if let Some(rest) = input.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, "UTF-16BE", u16::from_be_bytes);
    }

    std::str::from_utf8(input)
        .map(str::to_string)
        .map_err(|e| PromptDecodeError::InvalidUtf8 {
            valid_up_to: e.valid_up_to(),
        })
}

fn decode_utf16(
    input: &[u8],
    encoding: &'static str,
    decode_unit: fn([u8; 2]) -> u16,
) -> Result<String, PromptDecodeError> {
    if !input.len().is_multiple_of(2) {
        return Err(PromptDecodeError::InvalidUtf16 { encoding });
    }

    let units: Vec<u16> = input
        .chunks_exact(2)
        .map(|chunk| decode_unit([chunk[0], chunk[1]]))
        .collect();

    String::from_utf16(&units).map_err(|_| PromptDecodeError::InvalidUtf16 { encoding })
}

fn read_prompt_from_stdin(behavior: StdinPromptBehavior) -> Option<String> {
    let stdin_is_terminal = std::io::stdin().is_terminal();

    match behavior {
        StdinPromptBehavior::RequiredIfPiped if stdin_is_terminal => {
            eprintln!(
                "No prompt provided. Either specify one as an argument or pipe the prompt into stdin."
            );
            std::process::exit(1);
        }
        StdinPromptBehavior::RequiredIfPiped => {
            eprintln!("Reading prompt from stdin...");
        }
        StdinPromptBehavior::Forced => {}
        StdinPromptBehavior::OptionalAppend if stdin_is_terminal => return None,
        StdinPromptBehavior::OptionalAppend => {
            eprintln!("Reading additional input from stdin...");
        }
    }

    let mut bytes = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut bytes) {
        eprintln!("Failed to read prompt from stdin: {e}");
        std::process::exit(1);
    }

    let buffer = match decode_prompt_bytes(&bytes) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to read prompt from stdin: {e}");
            std::process::exit(1);
        }
    };

    if buffer.trim().is_empty() {
        match behavior {
            StdinPromptBehavior::OptionalAppend => None,
            StdinPromptBehavior::RequiredIfPiped | StdinPromptBehavior::Forced => {
                eprintln!("No prompt provided via stdin.");
                std::process::exit(1);
            }
        }
    } else {
        Some(buffer)
    }
}

fn prompt_with_stdin_context(prompt: &str, stdin_text: &str) -> String {
    let mut combined = format!("{prompt}\n\n<stdin>\n{stdin_text}");
    if !stdin_text.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str("</stdin>");
    combined
}

fn resolve_prompt(prompt_arg: Option<String>) -> String {
    match prompt_arg {
        Some(p) if p != "-" => p,
        maybe_dash => {
            let behavior = if matches!(maybe_dash.as_deref(), Some("-")) {
                StdinPromptBehavior::Forced
            } else {
                StdinPromptBehavior::RequiredIfPiped
            };
            let Some(prompt) = read_prompt_from_stdin(behavior) else {
                unreachable!("required stdin prompt should produce content");
            };
            prompt
        }
    }
}

fn resolve_root_prompt(prompt_arg: Option<String>) -> String {
    match prompt_arg {
        Some(prompt) if prompt != "-" => {
            if let Some(stdin_text) = read_prompt_from_stdin(StdinPromptBehavior::OptionalAppend) {
                prompt_with_stdin_context(&prompt, &stdin_text)
            } else {
                prompt
            }
        }
        maybe_dash => resolve_prompt(maybe_dash),
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
