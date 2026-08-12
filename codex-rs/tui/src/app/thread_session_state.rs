use super::App;
use crate::session_resume::read_session_model;
use crate::session_state::ThreadSessionState;
use codex_cli_protocol::Thread;
use codex_protocol::ThreadId;

impl App {
    pub(super) async fn sync_active_thread_service_tier_to_cached_session(&mut self) {
        let Some(active_thread_id) = self.active_thread_id else {
            return;
        };

        let service_tier = self.chat_widget.current_service_tier().map(str::to_string);
        let update_session = |session: &mut ThreadSessionState| {
            session.service_tier = service_tier.clone();
        };

        if self.primary_thread_id == Some(active_thread_id)
            && let Some(session) = self.primary_session_configured.as_mut()
        {
            update_session(session);
        }

        if let Some(channel) = self.thread_event_channels.get(&active_thread_id) {
            let mut store = channel.store.lock().await;
            if let Some(session) = store.session.as_mut() {
                update_session(session);
            }
        }
    }

    pub(super) async fn session_state_for_thread_read(
        &self,
        thread_id: ThreadId,
        thread: &Thread,
    ) -> ThreadSessionState {
        let mut session = if let Some(mut session) = self.primary_session_configured.clone() {
            if session.thread_id != thread_id {
                // `thread/read` does not include thread settings, so do not carry
                // thread-scoped state from the currently active session.
                session.agent_settings = None;
                session.personality = None;
            }
            session
        } else {
            ThreadSessionState {
                thread_id,
                forked_from_id: None,
                fork_parent_title: None,
                thread_name: None,
                model: self.chat_widget.current_model().to_string(),
                model_provider_id: self.config.model_provider_id.clone(),
                service_tier: self.chat_widget.current_service_tier().map(str::to_string),
                cwd: thread.cwd.clone(),
                runtime_workspace_roots: self.config.workspace_roots.clone(),
                instruction_source_paths: Vec::new(),
                reasoning_effort: self.chat_widget.current_reasoning_effort(),
                agent_settings: None,
                personality: None,
                message_history: None,
                rollout_path: thread.path.clone(),
            }
        };
        session.thread_id = thread_id;
        session.thread_name = thread.name.clone();
        session.model_provider_id = thread.model_provider.clone();
        session.set_cwd_retargeting_implicit_runtime_workspace_root(thread.cwd.clone());
        session.instruction_source_paths = Vec::new();
        session.rollout_path = thread.path.clone();
        if let Some(model) =
            read_session_model(self.state_db.as_deref(), thread_id, thread.path.as_deref()).await
        {
            session.model = model;
        } else if thread.path.is_some() {
            session.model.clear();
        }
        session.message_history = None;
        session
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::side::SideThreadState;
    use crate::app::test_support::make_test_app;
    use crate::app::thread_events::ThreadEventChannel;
    use crate::legacy_core::config::PermissionProfileSnapshot;
    use crate::test_support::PathBufExt;
    use crate::test_support::test_path_buf;
    use codex_cli_protocol::AskForApproval;
    use codex_config::types::ApprovalsReviewer;
    use codex_protocol::config_types::ServiceTier;
    use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
    use codex_protocol::models::ManagedFileSystemPermissions;
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::permissions::FileSystemAccessMode;
    use codex_protocol::permissions::FileSystemPath;
    use codex_protocol::permissions::FileSystemSandboxEntry;
    use codex_protocol::permissions::FileSystemSpecialPath;
    use codex_protocol::permissions::NetworkSandboxPolicy;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn test_thread_session(thread_id: ThreadId, cwd: PathBuf) -> ThreadSessionState {
        ThreadSessionState {
            thread_id,
            forked_from_id: None,
            fork_parent_title: None,
            thread_name: None,
            model: "gpt-test".to_string(),
            model_provider_id: "test-provider".to_string(),
            service_tier: None,
            cwd: cwd.abs(),
            runtime_workspace_roots: vec![cwd.abs()],
            instruction_source_paths: Vec::new(),
            reasoning_effort: None,
            agent_settings: None,
            personality: None,
            message_history: None,
            rollout_path: Some(PathBuf::new()),
        }
    }

    #[tokio::test]
    async fn service_tier_sync_updates_active_cached_session() {
        let mut app = make_test_app().await;
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000406").expect("valid thread");
        let session = ThreadSessionState {
            service_tier: Some(ServiceTier::Fast.request_value().to_string()),
            ..test_thread_session(thread_id, test_path_buf("/tmp/main"))
        };

        app.primary_thread_id = Some(thread_id);
        app.active_thread_id = Some(thread_id);
        app.primary_session_configured = Some(session.clone());
        app.thread_event_channels.insert(
            thread_id,
            ThreadEventChannel::new_with_session(/*capacity*/ 4, session.clone(), Vec::new()),
        );
        app.chat_widget.handle_thread_session(session);
        app.chat_widget.set_service_tier(/*service_tier*/ None);

        app.sync_active_thread_service_tier_to_cached_session()
            .await;

        let expected_session = ThreadSessionState {
            service_tier: None,
            ..test_thread_session(thread_id, test_path_buf("/tmp/main"))
        };
        assert_eq!(
            app.primary_session_configured,
            Some(expected_session.clone())
        );

        let store_session = app
            .thread_event_channels
            .get(&thread_id)
            .expect("thread channel")
            .store
            .lock()
            .await
            .session
            .clone();
        assert_eq!(store_session, Some(expected_session));
    }
}
