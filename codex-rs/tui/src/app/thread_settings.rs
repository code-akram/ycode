//! Thread settings sync between TUI-local state and cli-runtime thread state.

use super::App;
use crate::app_command::AppCommand;
use crate::runtime_session::CliRuntimeSession;
use crate::session_state::ThreadSessionState;
use codex_cli_protocol::ThreadSettings;
use codex_cli_protocol::ThreadSettingsUpdateParams;
use codex_protocol::ThreadId;

impl App {
    pub(super) async fn sync_active_thread_model_setting(
        &mut self,
        cli_runtime: &mut CliRuntimeSession,
        model: String,
        effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    ) {
        let Some(mut params) = self.active_thread_model_setting_update_params(model) else {
            return;
        };
        params.effort = effort;
        self.send_thread_settings_update(cli_runtime, params).await;
    }

    pub(super) fn active_thread_model_setting_update_params(
        &self,
        model: String,
    ) -> Option<ThreadSettingsUpdateParams> {
        let thread_id = self.active_thread_id?;
        let params = ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            model: Some(model),
            agent_settings: Some(self.chat_widget.effective_agent_settings()),
            ..ThreadSettingsUpdateParams::default()
        };

        Some(params)
    }

    pub(super) async fn sync_active_thread_reasoning_setting(
        &mut self,
        cli_runtime: &mut CliRuntimeSession,
        effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    ) {
        let Some(params) = self.active_thread_reasoning_setting_update_params(effort) else {
            return;
        };
        self.send_thread_settings_update(cli_runtime, params).await;
    }

    pub(super) fn active_thread_reasoning_setting_update_params(
        &self,
        effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    ) -> Option<ThreadSettingsUpdateParams> {
        let thread_id = self.active_thread_id?;
        Some(ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            effort,
            agent_settings: Some(self.chat_widget.current_agent_settings().clone()),
            ..ThreadSettingsUpdateParams::default()
        })
    }

    pub(super) async fn sync_active_thread_personality_setting(
        &mut self,
        cli_runtime: &mut CliRuntimeSession,
        personality: codex_protocol::config_types::Personality,
    ) {
        let Some(thread_id) = self.active_thread_id else {
            return;
        };
        let params = ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            personality: Some(personality),
            ..ThreadSettingsUpdateParams::default()
        };
        self.send_thread_settings_update(cli_runtime, params).await;
    }

    pub(super) async fn sync_override_turn_context_settings(
        &mut self,
        cli_runtime: &mut CliRuntimeSession,
        thread_id: ThreadId,
        op: &AppCommand,
    ) {
        let AppCommand::OverrideTurnContext {
            cwd,
            model,
            effort,
            summary,
            service_tier,
            agent_settings,
            personality,
            ..
        } = op
        else {
            return;
        };

        let params = ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            cwd: cwd.clone(),
            model: model.clone(),
            effort: effort.clone().unwrap_or_default(),
            summary: *summary,
            service_tier: service_tier.clone(),
            agent_settings: agent_settings.clone(),
            personality: *personality,
            ..ThreadSettingsUpdateParams::default()
        };
        self.send_thread_settings_update(cli_runtime, params).await;
    }

    pub(super) async fn apply_thread_settings_to_cached_session(
        &mut self,
        thread_id: ThreadId,
        settings: &ThreadSettings,
    ) {
        if self.primary_thread_id == Some(thread_id)
            && let Some(session) = self.primary_session_configured.as_mut()
        {
            apply_thread_settings_to_session(session, settings);
        }

        if let Some(channel) = self.thread_event_channels.get(&thread_id) {
            let mut store = channel.store.lock().await;
            if let Some(session) = store.session.as_mut() {
                apply_thread_settings_to_session(session, settings);
            }
        }
    }

    pub(super) async fn send_thread_settings_update(
        &mut self,
        cli_runtime: &mut CliRuntimeSession,
        params: ThreadSettingsUpdateParams,
    ) -> bool {
        if !thread_settings_update_has_changes(&params) {
            return false;
        }
        match cli_runtime.thread_settings_update(params).await {
            Ok(settings_updated) => settings_updated,
            Err(err) => {
                tracing::warn!("failed to update cli-runtime thread settings from TUI: {err}");
                self.chat_widget
                    .add_error_message(format!("Failed to update thread settings: {err}"));
                false
            }
        }
    }
}

fn apply_thread_settings_to_session(session: &mut ThreadSessionState, settings: &ThreadSettings) {
    session.model = settings.model.clone();
    session.reasoning_effort = settings.effort.clone();
    session.model_provider_id = settings.model_provider.clone();
    session.service_tier = settings.service_tier.clone();
    session.set_cwd_retargeting_implicit_runtime_workspace_root(settings.cwd.clone());
    session.personality = settings.personality;
    let mut agent_settings = settings.agent_settings.clone();
    agent_settings.settings.model.clone_from(&settings.model);
    agent_settings.settings.reasoning_effort = settings.effort.clone();
    session.agent_settings = Some(Box::new(agent_settings));
}

fn thread_settings_update_has_changes(params: &ThreadSettingsUpdateParams) -> bool {
    params.cwd.is_some()
        || params.model.is_some()
        || params.service_tier.is_some()
        || params.effort.is_some()
        || params.summary.is_some()
        || params.agent_settings.is_some()
        || params.personality.is_some()
}
