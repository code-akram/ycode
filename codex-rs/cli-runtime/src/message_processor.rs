use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;

use crate::attestation::cli_runtime_attestation_provider;
use crate::config_manager::ConfigManager;
use crate::connection_rpc_gate::ConnectionRpcGate;
use crate::current_time::cli_runtime_time_provider;
use crate::error_code::invalid_request;
use crate::extensions::ThreadExtensionDependencies;
use crate::extensions::cli_runtime_extension_event_sink;
use crate::extensions::thread_extensions;
use crate::fs_watch::FsWatchManager;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::ConnectionRequestId;
use crate::outgoing_message::OutgoingMessageSender;
use crate::outgoing_message::RequestContext;
use crate::request_processors::AccountRequestProcessor;
use crate::request_processors::CatalogRequestProcessor;
use crate::request_processors::CommandExecRequestProcessor;
use crate::request_processors::ConfigRequestProcessor;
use crate::request_processors::EnvironmentRequestProcessor;
use crate::request_processors::FsRequestProcessor;
use crate::request_processors::GitRequestProcessor;
use crate::request_processors::InitializeRequestProcessor;
use crate::request_processors::ProcessExecRequestProcessor;
use crate::request_processors::SearchRequestProcessor;
use crate::request_processors::ThreadGoalRequestProcessor;
use crate::request_processors::ThreadRequestProcessor;
use crate::request_processors::TurnRequestProcessor;
use crate::request_serialization::QueuedInitializedRequest;
use crate::request_serialization::RequestSerializationQueueKey;
use crate::request_serialization::RequestSerializationQueues;
use crate::skills_watcher::SkillsWatcher;
use crate::thread_state::ConnectionCapabilities;
use crate::thread_state::ThreadStateManager;
use codex_arg0::Arg0DispatchPaths;
use codex_cli_protocol::ClientNotification;
use codex_cli_protocol::ClientRequest;
use codex_cli_protocol::ClientResponsePayload;
use codex_cli_protocol::ConfigWarningNotification;
use codex_cli_protocol::ExperimentalApi;
use codex_cli_protocol::JSONRPCErrorError;
use codex_cli_protocol::experimental_required_message;
use codex_code_mode::CodeModeSessionProvider;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_exec_server::EnvironmentManager;
use codex_goal_extension::GoalService;
use codex_home::CodexHomeUserInstructionsProvider;
use codex_login::AuthManager;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_rollout::StateDbHandle;
use codex_state::log_db::LogDbLayer;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::broadcast;
use tokio::time::Duration;
use tokio::time::timeout;
use tracing::Instrument;

use crate::models_refresh_worker::ModelsRefreshWorker;

const CONNECTION_RPC_DRAIN_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 30);

pub(crate) struct MessageProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    models_refresh_worker: ModelsRefreshWorker,
    skills_watcher: Arc<SkillsWatcher>,
    account_processor: AccountRequestProcessor,
    catalog_processor: CatalogRequestProcessor,
    command_exec_processor: CommandExecRequestProcessor,
    process_exec_processor: ProcessExecRequestProcessor,
    config_processor: ConfigRequestProcessor,
    environment_processor: EnvironmentRequestProcessor,
    fs_processor: FsRequestProcessor,
    git_processor: GitRequestProcessor,
    initialize_processor: InitializeRequestProcessor,
    search_processor: SearchRequestProcessor,
    thread_goal_processor: ThreadGoalRequestProcessor,
    thread_processor: ThreadRequestProcessor,
    turn_processor: TurnRequestProcessor,
    thread_manager: Arc<ThreadManager>,
    request_serialization_queues: RequestSerializationQueues,
}

#[derive(Debug)]
pub(crate) struct ConnectionSessionState {
    pub(crate) rpc_gate: Arc<ConnectionRpcGate>,
    initialized: OnceLock<InitializedConnectionSessionState>,
}

#[derive(Debug)]
pub(crate) struct InitializedConnectionSessionState {
    pub(crate) experimental_api_enabled: bool,
    pub(crate) opted_out_notification_methods: HashSet<String>,
    pub(crate) cli_runtime_client_name: String,
    pub(crate) client_version: String,
    pub(crate) request_attestation: bool,
}

impl Default for ConnectionSessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionSessionState {
    pub(crate) fn new() -> Self {
        Self {
            rpc_gate: Arc::new(ConnectionRpcGate::new()),
            initialized: OnceLock::new(),
        }
    }

    pub(crate) fn initialized(&self) -> bool {
        self.initialized.get().is_some()
    }

    pub(crate) fn experimental_api_enabled(&self) -> bool {
        self.initialized
            .get()
            .is_some_and(|session| session.experimental_api_enabled)
    }

    pub(crate) fn opted_out_notification_methods(&self) -> HashSet<String> {
        self.initialized
            .get()
            .map(|session| session.opted_out_notification_methods.clone())
            .unwrap_or_default()
    }

    pub(crate) fn cli_runtime_client_name(&self) -> Option<&str> {
        self.initialized
            .get()
            .map(|session| session.cli_runtime_client_name.as_str())
    }

    pub(crate) fn client_version(&self) -> Option<&str> {
        self.initialized
            .get()
            .map(|session| session.client_version.as_str())
    }

    pub(crate) fn request_attestation(&self) -> bool {
        self.initialized
            .get()
            .is_some_and(|session| session.request_attestation)
    }
    pub(crate) fn initialize(&self, session: InitializedConnectionSessionState) -> Result<(), ()> {
        self.initialized.set(session).map_err(|_| ())
    }
}

pub(crate) struct MessageProcessorArgs {
    pub(crate) outgoing: Arc<OutgoingMessageSender>,
    pub(crate) arg0_paths: Arg0DispatchPaths,
    pub(crate) config: Arc<Config>,
    pub(crate) config_manager: ConfigManager,
    pub(crate) environment_manager: Arc<EnvironmentManager>,
    pub(crate) log_db: Option<LogDbLayer>,
    pub(crate) state_db: Option<StateDbHandle>,
    pub(crate) config_warnings: Vec<ConfigWarningNotification>,
    pub(crate) session_source: SessionSource,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) installation_id: String,
    pub(crate) code_mode_session_provider: Option<Arc<dyn CodeModeSessionProvider>>,
}

impl MessageProcessor {
    /// Create a new `MessageProcessor`, retaining a handle to the outgoing
    /// `Sender` so handlers can enqueue messages to be written to stdout.
    pub(crate) fn new(args: MessageProcessorArgs) -> Self {
        let MessageProcessorArgs {
            outgoing,
            arg0_paths,
            config,
            config_manager,
            environment_manager,
            log_db,
            state_db,
            config_warnings,
            session_source,
            auth_manager,
            installation_id,
            code_mode_session_provider,
        } = args;
        let thread_state_manager = ThreadStateManager::new();
        // The thread store is intentionally process-scoped. Config reloads can
        // affect per-thread behavior, but they must not move newly started,
        // resumed, or forked threads to a different persistence backend/root.
        let thread_store = codex_core::thread_store_from_config(config.as_ref(), state_db.clone());
        let environment_manager_for_requests = Arc::clone(&environment_manager);
        let environment_manager_for_extensions = Arc::clone(&environment_manager);
        let restriction_product = session_source.restriction_product();
        let executor_skill_provider: Arc<dyn codex_skills_extension::SkillProvider> = Arc::new(
            codex_skills_extension::ExecutorSkillProvider::new_with_restriction_product(
                Arc::clone(&environment_manager_for_extensions),
                restriction_product,
            ),
        );
        let goal_service = Arc::new(GoalService::new());
        let thread_manager = Arc::new_cyclic(|thread_manager| {
            let manager = ThreadManager::new(
                config.as_ref(),
                auth_manager.clone(),
                codex_core::build_models_manager(config.as_ref(), auth_manager.clone()),
                session_source,
                environment_manager,
                thread_extensions(ThreadExtensionDependencies {
                    event_sink: cli_runtime_extension_event_sink(
                        outgoing.clone(),
                        thread_state_manager.clone(),
                    ),
                    auth_manager: auth_manager.clone(),
                    state_db: state_db.clone(),
                    thread_manager: thread_manager.clone(),
                    goal_service: Arc::clone(&goal_service),
                    environment_manager: Arc::clone(&environment_manager_for_extensions),
                    executor_skill_provider: Arc::clone(&executor_skill_provider),
                    git_attribution_base_url: config.chatgpt_base_url.clone(),
                    http_client_factory: config.http_client_factory(),
                    thread_store: Arc::clone(&thread_store),
                }),
                Arc::new(CodexHomeUserInstructionsProvider::new(
                    config.codex_home.clone(),
                )),
                Arc::clone(&thread_store),
                codex_core::local_agent_graph_store_from_state_db(state_db.as_ref()),
                installation_id,
                Some(cli_runtime_attestation_provider(
                    outgoing.clone(),
                    thread_state_manager.clone(),
                )),
                Some(cli_runtime_time_provider(
                    outgoing.clone(),
                    thread_state_manager.clone(),
                )),
            );
            match code_mode_session_provider {
                Some(provider) => manager.with_code_mode_session_provider(provider),
                None => manager,
            }
        });
        let models_manager = thread_manager.get_models_manager();
        let models_refresh_worker =
            crate::models_refresh_worker::spawn(&models_manager, config.http_client_factory());
        let skills_watcher = SkillsWatcher::new(
            thread_manager.skills_service(),
            &config.codex_home,
            outgoing.clone(),
        );

        let pending_thread_unloads = Arc::new(Mutex::new(HashSet::new()));
        let thread_watch_manager =
            crate::thread_status::ThreadWatchManager::new_with_outgoing(outgoing.clone());
        let thread_list_state_permit = Arc::new(Semaphore::new(/*permits*/ 1));
        let request_serialization_queues = RequestSerializationQueues::default();
        let config_processor = ConfigRequestProcessor::new(
            outgoing.clone(),
            config_manager.clone(),
            thread_manager.clone(),
        );
        let account_processor = AccountRequestProcessor::new(
            auth_manager.clone(),
            Arc::clone(&thread_manager),
            outgoing.clone(),
            Arc::clone(&config),
            config_manager.clone(),
        );
        let catalog_processor = CatalogRequestProcessor::new(
            outgoing.clone(),
            Arc::clone(&skills_watcher),
            Arc::clone(&thread_manager),
            Arc::clone(&config),
            config_manager.clone(),
        );
        let command_exec_processor = CommandExecRequestProcessor::new(
            arg0_paths.clone(),
            Arc::clone(&config),
            outgoing.clone(),
            config_manager.clone(),
            Arc::clone(&environment_manager_for_requests),
        );
        let process_exec_processor = ProcessExecRequestProcessor::new(
            outgoing.clone(),
            Arc::clone(&environment_manager_for_requests),
        );
        let git_processor = GitRequestProcessor::new();
        let initialize_processor = InitializeRequestProcessor::new(
            outgoing.clone(),
            Arc::clone(&config),
            config_warnings.clone(),
        );
        let search_processor = SearchRequestProcessor::new(outgoing.clone());
        let thread_goal_processor = ThreadGoalRequestProcessor::new(
            Arc::clone(&thread_manager),
            outgoing.clone(),
            Arc::clone(&config),
            thread_state_manager.clone(),
            state_db.clone(),
            Arc::clone(&goal_service),
        );
        let thread_processor = ThreadRequestProcessor::new(
            auth_manager.clone(),
            Arc::clone(&thread_manager),
            outgoing.clone(),
            arg0_paths.clone(),
            Arc::clone(&config),
            config_manager.clone(),
            Arc::clone(&thread_store),
            Arc::clone(&pending_thread_unloads),
            thread_state_manager.clone(),
            thread_watch_manager.clone(),
            Arc::clone(&thread_list_state_permit),
            thread_goal_processor.clone(),
            state_db.clone(),
            log_db,
            Arc::clone(&skills_watcher),
            config_warnings,
        );
        let turn_processor = TurnRequestProcessor::new(
            auth_manager.clone(),
            Arc::clone(&thread_manager),
            outgoing.clone(),
            arg0_paths.clone(),
            Arc::clone(&config),
            config_manager.clone(),
            pending_thread_unloads,
            thread_state_manager,
            thread_watch_manager,
            thread_list_state_permit,
            Arc::clone(&skills_watcher),
        );
        let environment_processor =
            EnvironmentRequestProcessor::new(thread_manager.environment_manager());
        let fs_processor = FsRequestProcessor::new(
            Arc::clone(&environment_manager_for_requests),
            FsWatchManager::new(outgoing.clone()),
        );
        Self {
            outgoing,
            models_refresh_worker,
            skills_watcher,
            account_processor,
            catalog_processor,
            command_exec_processor,
            process_exec_processor,
            config_processor,
            environment_processor,
            fs_processor,
            git_processor,
            initialize_processor,
            search_processor,
            thread_goal_processor,
            thread_processor,
            turn_processor,
            thread_manager,
            request_serialization_queues,
        }
    }

    pub(crate) async fn start_native_code_mode_from_interactive_tui(
        &self,
        thread_id: codex_protocol::ThreadId,
        task: String,
    ) -> codex_protocol::error::Result<String> {
        self.thread_manager
            .get_thread(thread_id)
            .await?
            .start_native_code_mode_from_interactive_tui(task)
            .await
    }

    pub(crate) async fn observe_native_code_mode_from_interactive_tui(
        &self,
        thread_id: codex_protocol::ThreadId,
        run_id: String,
    ) -> codex_protocol::error::Result<
        tokio::sync::watch::Receiver<Option<codex_core::native_run_tree::NativeRunTreeSnapshot>>,
    > {
        self.thread_manager
            .get_thread(thread_id)
            .await?
            .observe_native_code_mode_from_interactive_tui(&run_id)
    }

    pub(crate) async fn cancel_native_code_mode_node_from_interactive_tui(
        &self,
        thread_id: codex_protocol::ThreadId,
        run_id: String,
        node_id: String,
    ) -> codex_protocol::error::Result<codex_core::native_run_tree::NativeRunCancelResult> {
        self.thread_manager
            .get_thread(thread_id)
            .await?
            .cancel_native_code_mode_node_from_interactive_tui(&run_id, &node_id)
    }

    pub(crate) fn clear_runtime_references(&self) {
        self.account_processor.clear_external_auth();
        self.models_refresh_worker.shutdown();
        self.skills_watcher.shutdown();
    }

    /// Handles a typed request path used by in-process embedders.
    ///
    /// This bypasses JSON request deserialization but keeps identical request
    /// semantics by delegating to `handle_client_request`.
    pub(crate) async fn process_client_request(
        self: &Arc<Self>,
        connection_id: ConnectionId,
        request: ClientRequest,
        session: Arc<ConnectionSessionState>,
        outbound_initialized: &AtomicBool,
    ) {
        let request_id = ConnectionRequestId {
            connection_id,
            request_id: request.id().clone(),
        };
        let request_span =
            crate::cli_runtime_tracing::typed_request_span(&request, connection_id, &session);
        let request_context =
            RequestContext::new(request_id.clone(), request_span, /*parent_trace*/ None);
        tracing::trace!(
            ?connection_id,
            request_id = ?request_id.request_id,
            "cli-runtime typed request"
        );
        Self::run_request_with_context(
            Arc::clone(&self.outgoing),
            request_context.clone(),
            async {
                // In-process clients do not have the websocket transport loop that performs
                // post-initialize bookkeeping, so they still finalize outbound readiness in
                // the shared request handler.
                let result = self
                    .handle_client_request(
                        request_id.clone(),
                        request,
                        Arc::clone(&session),
                        Some(outbound_initialized),
                        request_context.clone(),
                    )
                    .await;
                if let Err(error) = result {
                    self.outgoing.send_error(request_id.clone(), error).await;
                }
            },
        )
        .await;
    }

    /// Handles typed notifications from in-process clients.
    pub(crate) async fn process_client_notification(&self, notification: ClientNotification) {
        // Currently, we do not expect to receive any typed notifications from
        // in-process clients, so we just log them.
        tracing::info!("<- typed notification: {:?}", notification);
    }

    async fn run_request_with_context<F>(
        outgoing: Arc<OutgoingMessageSender>,
        request_context: RequestContext,
        request_fut: F,
    ) where
        F: Future<Output = ()>,
    {
        outgoing
            .register_request_context(request_context.clone())
            .await;
        request_fut.instrument(request_context.span()).await;
    }

    pub(crate) fn thread_created_receiver(&self) -> broadcast::Receiver<ThreadId> {
        self.thread_processor.thread_created_receiver()
    }

    pub(crate) async fn send_initialize_notifications(&self) {
        self.initialize_processor
            .send_initialize_notifications()
            .await;
    }

    pub(crate) async fn try_attach_thread_listener(
        &self,
        thread_id: ThreadId,
        connection_ids: Vec<ConnectionId>,
    ) {
        self.thread_processor
            .try_attach_thread_listener(thread_id, connection_ids)
            .await;
    }

    pub(crate) async fn drain_background_tasks(&self) {
        self.models_refresh_worker.shutdown();
        self.thread_processor.drain_background_tasks().await;
    }

    pub(crate) async fn cancel_active_login(&self) {
        self.account_processor.cancel_active_login().await;
    }

    pub(crate) async fn clear_all_thread_listeners(&self) {
        self.thread_processor.clear_all_thread_listeners().await;
    }

    pub(crate) async fn shutdown_threads(&self) {
        self.thread_processor.shutdown_threads().await;
    }

    pub(crate) async fn connection_closed(
        &self,
        connection_id: ConnectionId,
        session_state: &ConnectionSessionState,
    ) {
        if timeout(
            CONNECTION_RPC_DRAIN_TIMEOUT,
            session_state.rpc_gate.shutdown(),
        )
        .await
        .is_err()
        {
            tracing::warn!(
                ?connection_id,
                timeout_seconds = CONNECTION_RPC_DRAIN_TIMEOUT.as_secs(),
                "timed out waiting for connection RPCs to drain"
            );
        }
        self.outgoing.connection_closed(connection_id).await;
        self.fs_processor.connection_closed(connection_id).await;
        self.command_exec_processor
            .connection_closed(connection_id)
            .await;
        self.process_exec_processor
            .connection_closed(connection_id)
            .await;
        self.thread_processor.connection_closed(connection_id).await;
    }

    async fn handle_client_request(
        self: &Arc<Self>,
        connection_request_id: ConnectionRequestId,
        codex_request: ClientRequest,
        session: Arc<ConnectionSessionState>,
        // `Some(...)` means the caller wants initialize to immediately mark the
        // connection outbound-ready. Websocket JSON-RPC calls pass `None` so
        // lib.rs can deliver connection-scoped initialize notifications first.
        outbound_initialized: Option<&AtomicBool>,
        request_context: RequestContext,
    ) -> Result<(), JSONRPCErrorError> {
        let connection_id = connection_request_id.connection_id;
        if let ClientRequest::Initialize { request_id, params } = codex_request {
            let connection_initialized = self
                .initialize_processor
                .initialize(
                    connection_id,
                    request_id,
                    params,
                    &session,
                    outbound_initialized,
                )
                .await?;
            if connection_initialized {
                self.thread_processor
                    .connection_initialized(
                        connection_id,
                        ConnectionCapabilities {
                            request_attestation: session.request_attestation(),
                        },
                    )
                    .await;
            }
            return Ok(());
        }

        self.dispatch_initialized_client_request(
            connection_request_id,
            codex_request,
            session,
            request_context,
        )
        .await
    }

    async fn dispatch_initialized_client_request(
        self: &Arc<Self>,
        connection_request_id: ConnectionRequestId,
        codex_request: ClientRequest,
        session: Arc<ConnectionSessionState>,
        request_context: RequestContext,
    ) -> Result<(), JSONRPCErrorError> {
        if !session.initialized() {
            return Err(invalid_request("Not initialized"));
        }

        if let Some(reason) = codex_request.experimental_reason()
            && !session.experimental_api_enabled()
        {
            return Err(invalid_request(experimental_required_message(reason)));
        }
        let connection_id = connection_request_id.connection_id;
        let serialization_scope = codex_request.serialization_scope();
        let cli_runtime_client_name = session.cli_runtime_client_name().map(str::to_string);
        let client_version = session.client_version().map(str::to_string);
        let error_request_id = connection_request_id.clone();
        let rpc_gate = Arc::clone(&session.rpc_gate);
        let processor = Arc::clone(self);
        let span = request_context.span();
        let request = QueuedInitializedRequest::new(
            rpc_gate,
            async move {
                let processor_for_request = Arc::clone(&processor);
                let result = processor_for_request
                    .handle_initialized_client_request(
                        connection_request_id,
                        codex_request,
                        request_context,
                        cli_runtime_client_name,
                        client_version,
                    )
                    .await;
                if let Err(error) = result {
                    processor.outgoing.send_error(error_request_id, error).await;
                }
            }
            .instrument(span),
        );

        if let Some(scope) = serialization_scope {
            let (key, access) = RequestSerializationQueueKey::from_scope(connection_id, scope);
            self.request_serialization_queues
                .enqueue(key, access, request)
                .await;
        } else {
            tokio::spawn(async move {
                request.run().await;
            });
        }
        Ok(())
    }

    async fn handle_initialized_client_request(
        self: Arc<Self>,
        connection_request_id: ConnectionRequestId,
        codex_request: ClientRequest,
        request_context: RequestContext,
        cli_runtime_client_name: Option<String>,
        client_version: Option<String>,
    ) -> Result<(), JSONRPCErrorError> {
        let connection_id = connection_request_id.connection_id;
        let request_id = ConnectionRequestId {
            connection_id,
            request_id: codex_request.id().clone(),
        };

        let result: Result<Option<ClientResponsePayload>, JSONRPCErrorError> = match codex_request {
            ClientRequest::Initialize { .. } => {
                panic!("Initialize should be handled before initialized request dispatch");
            }
            ClientRequest::ConfigRead { params, .. } => self
                .config_processor
                .read(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::ConfigValueWrite { params, .. } => {
                self.config_processor.value_write(params).await.map(Some)
            }
            ClientRequest::ConfigBatchWrite { params, .. } => {
                self.config_processor.batch_write(params).await.map(Some)
            }
            ClientRequest::EnvironmentAdd { params, .. } => {
                self.environment_processor.environment_add(params).await
            }
            ClientRequest::EnvironmentInfo { params, .. } => {
                self.environment_processor.environment_info(params).await
            }
            ClientRequest::EnvironmentStatus { params, .. } => {
                self.environment_processor.environment_status(params).await
            }
            ClientRequest::FsReadFile { params, .. } => self
                .fs_processor
                .read_file(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsWriteFile { params, .. } => self
                .fs_processor
                .write_file(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsCreateDirectory { params, .. } => self
                .fs_processor
                .create_directory(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsGetMetadata { params, .. } => self
                .fs_processor
                .get_metadata(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsReadDirectory { params, .. } => self
                .fs_processor
                .read_directory(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsRemove { params, .. } => self
                .fs_processor
                .remove(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsCopy { params, .. } => self
                .fs_processor
                .copy(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsWatch { params, .. } => self
                .fs_processor
                .watch(connection_id, params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FsUnwatch { params, .. } => self
                .fs_processor
                .unwatch(connection_id, params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::ThreadStart { params, .. } => {
                self.thread_processor
                    .thread_start(
                        request_id.clone(),
                        params,
                        cli_runtime_client_name.clone(),
                        client_version.clone(),
                        request_context,
                    )
                    .await
            }
            ClientRequest::ThreadUnsubscribe { params, .. } => {
                self.thread_processor
                    .thread_unsubscribe(&request_id, params)
                    .await
            }
            ClientRequest::ThreadResume { params, .. } => {
                self.thread_processor
                    .thread_resume(
                        request_id.clone(),
                        params,
                        cli_runtime_client_name.clone(),
                        client_version.clone(),
                    )
                    .await
            }
            ClientRequest::ThreadFork { params, .. } => {
                self.thread_processor
                    .thread_fork(
                        request_id.clone(),
                        params,
                        cli_runtime_client_name.clone(),
                        client_version.clone(),
                    )
                    .await
            }
            ClientRequest::ThreadArchive { params, .. } => {
                self.thread_processor
                    .thread_archive(request_id.clone(), params)
                    .await
            }
            ClientRequest::ThreadDelete { params, .. } => {
                self.thread_processor
                    .thread_delete(request_id.clone(), params)
                    .await
            }
            ClientRequest::ThreadIncrementElicitation { params, .. } => {
                self.thread_processor
                    .thread_increment_elicitation(params)
                    .await
            }
            ClientRequest::ThreadDecrementElicitation { params, .. } => {
                self.thread_processor
                    .thread_decrement_elicitation(params)
                    .await
            }
            ClientRequest::ThreadSetName { params, .. } => {
                self.thread_processor
                    .thread_set_name(request_id.clone(), params)
                    .await
            }
            ClientRequest::ThreadGoalSet { params, .. } => {
                self.thread_goal_processor
                    .thread_goal_set(request_id.clone(), params)
                    .await
            }
            ClientRequest::ThreadGoalGet { params, .. } => {
                self.thread_goal_processor.thread_goal_get(params).await
            }
            ClientRequest::ThreadGoalClear { params, .. } => {
                self.thread_goal_processor
                    .thread_goal_clear(request_id.clone(), params)
                    .await
            }
            ClientRequest::ThreadMetadataUpdate { params, .. } => {
                self.thread_processor.thread_metadata_update(params).await
            }
            ClientRequest::ThreadSectionMove { params, .. } => {
                self.thread_processor.thread_section_move(params).await
            }
            ClientRequest::ThreadSectionList { params, .. } => {
                self.thread_processor.thread_section_list(params).await
            }
            ClientRequest::ThreadSectionCreate { params, .. } => {
                self.thread_processor.thread_section_create(params).await
            }
            ClientRequest::ThreadSectionUpdate { params, .. } => {
                self.thread_processor.thread_section_update(params).await
            }
            ClientRequest::ThreadSectionDelete { params, .. } => {
                self.thread_processor.thread_section_delete(params).await
            }
            ClientRequest::ThreadSettingsUpdate { params, .. } => {
                self.turn_processor
                    .thread_settings_update(&request_id, params)
                    .await
            }
            ClientRequest::ThreadMemoryModeSet { params, .. } => {
                self.thread_processor.thread_memory_mode_set(params).await
            }
            ClientRequest::MemoryReset { .. } => self.thread_processor.memory_reset().await,
            ClientRequest::ThreadUnarchive { params, .. } => {
                self.thread_processor
                    .thread_unarchive(request_id.clone(), params)
                    .await
            }
            ClientRequest::ThreadCompactStart { params, .. } => {
                self.thread_processor
                    .thread_compact_start(&request_id, params)
                    .await
            }
            ClientRequest::ThreadBackgroundTerminalsClean { params, .. } => {
                self.thread_processor
                    .thread_background_terminals_clean(&request_id, params)
                    .await
            }
            ClientRequest::ThreadBackgroundTerminalsList { params, .. } => {
                self.thread_processor
                    .thread_background_terminals_list(params)
                    .await
            }
            ClientRequest::ThreadBackgroundTerminalsTerminate { params, .. } => {
                self.thread_processor
                    .thread_background_terminals_terminate(params)
                    .await
            }
            ClientRequest::ThreadRollback { params, .. } => {
                self.thread_processor
                    .thread_rollback(&request_id, params, cli_runtime_client_name.as_deref())
                    .await
            }
            ClientRequest::ThreadList { params, .. } => {
                self.thread_processor.thread_list(params).await
            }
            ClientRequest::ThreadSearch { params, .. } => {
                self.thread_processor.thread_search(params).await
            }
            ClientRequest::ThreadSearchOccurrences { params, .. } => {
                self.thread_processor
                    .thread_search_occurrences(params)
                    .await
            }
            ClientRequest::ThreadLoadedList { params, .. } => {
                self.thread_processor.thread_loaded_list(params).await
            }
            ClientRequest::ThreadRead { params, .. } => {
                self.thread_processor.thread_read(params).await
            }
            ClientRequest::ThreadTurnsList { params, .. } => {
                self.thread_processor.thread_turns_list(params).await
            }
            ClientRequest::ThreadItemsList { params, .. } => {
                self.thread_processor.thread_items_list(params).await
            }
            ClientRequest::ThreadShellCommand { params, .. } => {
                self.thread_processor
                    .thread_shell_command(&request_id, params)
                    .await
            }
            ClientRequest::GetConversationSummary { params, .. } => {
                self.thread_processor.conversation_summary(params).await
            }
            ClientRequest::SkillsList { params, .. } => {
                self.catalog_processor.skills_list(params).await
            }
            ClientRequest::SkillsExtraRootsSet { params, .. } => {
                self.catalog_processor.skills_extra_roots_set(params).await
            }
            ClientRequest::SkillsConfigWrite { params, .. } => {
                self.catalog_processor.skills_config_write(params).await
            }
            ClientRequest::ModelList { params, .. } => {
                self.catalog_processor.model_list(params).await
            }
            ClientRequest::ExperimentalFeatureList { params, .. } => {
                self.catalog_processor
                    .experimental_feature_list(params)
                    .await
            }
            ClientRequest::PermissionProfileList { params, .. } => {
                self.catalog_processor.permission_profile_list(params).await
            }
            ClientRequest::MockExperimentalMethod { params, .. } => {
                self.catalog_processor
                    .mock_experimental_method(params)
                    .await
            }
            ClientRequest::TurnStart { params, .. } => {
                self.turn_processor
                    .turn_start(
                        request_id.clone(),
                        params,
                        cli_runtime_client_name.clone(),
                        client_version.clone(),
                    )
                    .await
            }
            ClientRequest::ThreadInjectItems { params, .. } => {
                self.turn_processor.thread_inject_items(params).await
            }
            ClientRequest::TurnSteer { params, .. } => {
                self.turn_processor.turn_steer(&request_id, params).await
            }
            ClientRequest::TurnInterrupt { params, .. } => {
                self.turn_processor
                    .turn_interrupt(&request_id, params)
                    .await
            }
            ClientRequest::ThreadRealtimeStart { params, .. } => {
                self.turn_processor
                    .thread_realtime_start(&request_id, params)
                    .await
            }
            ClientRequest::ThreadRealtimeAppendAudio { params, .. } => {
                self.turn_processor
                    .thread_realtime_append_audio(&request_id, params)
                    .await
            }
            ClientRequest::ThreadRealtimeAppendText { params, .. } => {
                self.turn_processor
                    .thread_realtime_append_text(&request_id, params)
                    .await
            }
            ClientRequest::ThreadRealtimeAppendSpeech { params, .. } => {
                self.turn_processor
                    .thread_realtime_append_speech(&request_id, params)
                    .await
            }
            ClientRequest::ThreadRealtimeStop { params, .. } => {
                self.turn_processor
                    .thread_realtime_stop(&request_id, params)
                    .await
            }
            ClientRequest::ThreadRealtimeListVoices { params: _, .. } => {
                self.turn_processor.thread_realtime_list_voices().await
            }
            ClientRequest::LoginAccount { params, .. } => {
                self.account_processor
                    .login_account(request_id.clone(), params)
                    .await
            }
            ClientRequest::LogoutAccount { .. } => {
                self.account_processor
                    .logout_account(request_id.clone())
                    .await
            }
            ClientRequest::CancelLoginAccount { params, .. } => {
                self.account_processor.cancel_login_account(params).await
            }
            ClientRequest::GetAccount { params, .. } => {
                self.account_processor.get_account(params).await
            }
            ClientRequest::GetAuthStatus { params, .. } => {
                self.account_processor.get_auth_status(params).await
            }
            ClientRequest::GetAccountRateLimits { .. } => {
                self.account_processor.get_account_rate_limits().await
            }
            ClientRequest::ConsumeAccountRateLimitResetCredit { params, .. } => {
                self.account_processor
                    .consume_account_rate_limit_reset_credit(params)
                    .await
            }
            ClientRequest::GetAccountTokenUsage { .. } => {
                self.account_processor.get_account_token_usage().await
            }
            ClientRequest::GetWorkspaceMessages { .. } => {
                self.account_processor.get_workspace_messages().await
            }
            ClientRequest::SendAddCreditsNudgeEmail { params, .. } => {
                self.account_processor
                    .send_add_credits_nudge_email(params)
                    .await
            }
            ClientRequest::GitDiffToRemote { params, .. } => {
                self.git_processor.git_diff_to_remote(params).await
            }
            ClientRequest::FuzzyFileSearch { params, .. } => self
                .search_processor
                .fuzzy_file_search(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FuzzyFileSearchSessionStart { params, .. } => self
                .search_processor
                .fuzzy_file_search_session_start_response(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FuzzyFileSearchSessionUpdate { params, .. } => self
                .search_processor
                .fuzzy_file_search_session_update_response(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::FuzzyFileSearchSessionStop { params, .. } => self
                .search_processor
                .fuzzy_file_search_session_stop(params)
                .await
                .map(|response| Some(response.into())),
            ClientRequest::OneOffCommandExec { params, .. } => {
                self.command_exec_processor
                    .one_off_command_exec(&request_id, params)
                    .await
            }
            ClientRequest::CommandExecWrite { params, .. } => {
                self.command_exec_processor
                    .command_exec_write(request_id.clone(), params)
                    .await
            }
            ClientRequest::CommandExecResize { params, .. } => {
                self.command_exec_processor
                    .command_exec_resize(request_id.clone(), params)
                    .await
            }
            ClientRequest::CommandExecTerminate { params, .. } => {
                self.command_exec_processor
                    .command_exec_terminate(request_id.clone(), params)
                    .await
            }
            ClientRequest::ProcessSpawn { params, .. } => self
                .process_exec_processor
                .process_spawn(request_id.clone(), params)
                .await
                .map(|()| None),
            ClientRequest::ProcessWriteStdin { params, .. } => {
                self.process_exec_processor
                    .process_write_stdin(request_id.clone(), params)
                    .await
            }
            ClientRequest::ProcessKill { params, .. } => {
                self.process_exec_processor
                    .process_kill(request_id.clone(), params)
                    .await
            }
            ClientRequest::ProcessResizePty { params, .. } => {
                self.process_exec_processor
                    .process_resize_pty(request_id.clone(), params)
                    .await
            }
        };

        match result {
            Ok(Some(response)) => {
                self.outgoing
                    .send_response_as(request_id.clone(), response)
                    .await;
            }
            Ok(None) => {}
            Err(error) => {
                self.outgoing.send_error(request_id.clone(), error).await;
            }
        }
        Ok(())
    }
}
