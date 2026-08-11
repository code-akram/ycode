//! Runtime settings state and model/collaboration coordination for `ChatWidget`.

use super::*;
use crate::chatwidget::rate_limits::RATE_LIMIT_SWITCH_PROMPT_VIEW_ID;

impl ChatWidget {
    pub(crate) fn set_feature_enabled(&mut self, feature: Feature, enabled: bool) -> bool {
        if let Err(err) = self.config.features.set_enabled(feature, enabled) {
            tracing::warn!(
                error = %err,
                feature = feature.key(),
                "failed to update constrained chat widget feature state"
            );
        }
        let enabled = self.config.features.enabled(feature);
        if feature == Feature::FastMode {
            self.refresh_effective_service_tier();
            self.sync_service_tier_commands();
        }
        if feature == Feature::Personality {
            self.sync_personality_command_enabled();
        }
        if feature == Feature::Plugins {
            self.sync_plugins_command_enabled();
            self.refresh_plugin_mentions();
        }
        if feature == Feature::Goals {
            self.sync_goal_command_enabled();
            if !enabled {
                self.current_goal_status_indicator = None;
                self.current_goal_status = None;
                self.turn_lifecycle.goal_status_active_turn_started_at = None;
                self.turn_lifecycle.budget_limited_turn_ids.clear();
                self.update_goal_status_indicator();
            }
        }
        if feature == Feature::MentionsV2 {
            self.sync_mentions_v2_enabled();
        }
        if feature == Feature::PreventIdleSleep {
            self.turn_lifecycle.set_prevent_idle_sleep(enabled);
        }
        enabled
    }

    /// Set the reasoning effort used by subsequent turns.
    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<ReasoningEffortConfig>) {
        self.current_agent_settings = self.current_agent_settings.with_updates(
            /*model*/ None,
            Some(effort.clone()),
            /*developer_instructions*/ None,
        );
        self.refresh_model_dependent_surfaces();
    }

    /// Set the personality in the widget's config copy.
    pub(crate) fn set_personality(&mut self, personality: Personality) {
        self.config.personality = Some(personality);
    }

    pub(crate) fn status_account_display(&self) -> Option<&StatusAccountDisplay> {
        self.status_account_display.as_ref()
    }

    pub(crate) fn runtime_model_provider_base_url(&self) -> Option<&str> {
        self.runtime_model_provider_base_url.as_deref()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn model_catalog(&self) -> Arc<ModelCatalog> {
        self.model_catalog.clone()
    }

    pub(crate) fn current_plan_type(&self) -> Option<PlanType> {
        self.plan_type
    }

    pub(crate) fn has_chatgpt_account(&self) -> bool {
        self.has_chatgpt_account
    }

    pub(crate) fn has_codex_backend_auth(&self) -> bool {
        self.has_codex_backend_auth
    }

    pub(crate) fn update_account_state(
        &mut self,
        status_account_display: Option<StatusAccountDisplay>,
        plan_type: Option<PlanType>,
        has_chatgpt_account: bool,
        has_codex_backend_auth: bool,
    ) {
        // Account-update notifications are the identity boundary. The visible account fields can
        // be identical across two accounts, so always invalidate account-scoped requests and data.
        self.clear_pending_token_activity_refreshes();
        self.clear_pending_rate_limit_reset_requests();
        self.codex_rate_limit_reached_type = None;
        self.codex_spend_control_reached = None;
        self.rate_limit_warnings = RateLimitWarningState::default();
        self.rate_limit_switch_prompt = RateLimitSwitchPromptState::Idle;
        self.bottom_pane
            .dismiss_view_by_id(RATE_LIMIT_SWITCH_PROMPT_VIEW_ID);
        let had_refreshing_status_outputs = !self.refreshing_status_outputs.is_empty();
        let now = Local::now();
        for (_, handle) in self.refreshing_status_outputs.drain(..) {
            handle.finish_rate_limit_refresh(&[], now);
        }
        if had_refreshing_status_outputs {
            self.request_redraw();
        }
        self.status_line_workspace_headline = None;
        self.status_line_workspace_headline_pending_request_id = None;
        self.status_line_workspace_headline_last_requested_at = None;
        self.status_line_workspace_messages_disabled = false;
        self.status_account_display = status_account_display;
        self.plan_type = plan_type;
        self.has_chatgpt_account = has_chatgpt_account;
        self.has_codex_backend_auth = has_codex_backend_auth;
        self.bottom_pane
            .set_token_activity_command_enabled(has_codex_backend_auth);
        self.refresh_status_surfaces();
    }

    /// Set the syntax theme override in the widget's config copy.
    pub(crate) fn set_tui_theme(&mut self, theme: Option<String>) {
        self.config.tui_theme = theme;
    }

    /// Set the model in the widget's config copy and stored agent settings.
    pub(crate) fn set_model(&mut self, model: &str) {
        self.current_agent_settings = self.current_agent_settings.with_updates(
            Some(model.to_string()),
            /*effort*/ None,
            /*developer_instructions*/ None,
        );
        self.refresh_effective_service_tier();
        self.refresh_model_dependent_surfaces();
    }

    pub(crate) fn current_model(&self) -> &str {
        self.current_agent_settings.model()
    }

    pub(super) fn sync_personality_command_enabled(&mut self) {
        self.bottom_pane
            .set_personality_command_enabled(self.config.features.enabled(Feature::Personality));
    }

    pub(super) fn sync_plugins_command_enabled(&mut self) {
        self.bottom_pane
            .set_plugins_command_enabled(self.config.features.enabled(Feature::Plugins));
    }

    pub(super) fn sync_goal_command_enabled(&mut self) {
        self.bottom_pane
            .set_goal_command_enabled(self.config.features.enabled(Feature::Goals));
    }

    pub(super) fn sync_mentions_v2_enabled(&mut self) {
        self.bottom_pane
            .set_mentions_v2_enabled(self.config.features.enabled(Feature::MentionsV2));
    }

    pub(super) fn current_model_supports_personality(&self) -> bool {
        let model = self.current_model();
        self.model_catalog
            .try_list_models()
            .ok()
            .and_then(|models| {
                models
                    .into_iter()
                    .find(|preset| preset.model == model)
                    .map(|preset| preset.supports_personality)
            })
            .unwrap_or(false)
    }

    /// Return whether the effective model currently advertises image-input support.
    ///
    /// We intentionally default to `true` when model metadata cannot be read so transient catalog
    /// failures do not hard-block user input in the UI.
    pub(super) fn current_model_supports_images(&self) -> bool {
        let model = self.current_model();
        self.model_catalog
            .try_list_models()
            .ok()
            .and_then(|models| {
                models
                    .into_iter()
                    .find(|preset| preset.model == model)
                    .map(|preset| preset.input_modalities.contains(&InputModality::Image))
            })
            .unwrap_or(true)
    }

    pub(super) fn sync_image_paste_enabled(&mut self) {
        let enabled = self.current_model_supports_images();
        self.bottom_pane.set_image_paste_enabled(enabled);
    }

    pub(super) fn image_inputs_not_supported_message(&self) -> String {
        format!(
            "Model {} does not support image inputs. Remove images or switch models.",
            self.current_model()
        )
    }

    pub(crate) fn current_agent_settings(&self) -> &AgentSettings {
        &self.current_agent_settings
    }

    pub(crate) fn current_reasoning_effort(&self) -> Option<ReasoningEffortConfig> {
        self.effective_reasoning_effort()
    }

    pub(crate) fn on_thread_settings_updated(
        &mut self,
        notification: ThreadSettingsUpdatedNotification,
    ) {
        let Ok(thread_id) = ThreadId::from_string(&notification.thread_id) else {
            tracing::warn!(
                thread_id = notification.thread_id,
                "ignoring app-server ThreadSettingsUpdated with invalid thread_id"
            );
            return;
        };
        if self.thread_id != Some(thread_id) {
            return;
        }

        self.apply_thread_settings(notification.thread_settings);
    }

    pub(super) fn is_session_configured(&self) -> bool {
        self.thread_id.is_some()
    }

    pub(super) fn effective_reasoning_effort(&self) -> Option<ReasoningEffortConfig> {
        self.current_agent_settings.reasoning_effort()
    }

    pub(crate) fn effective_agent_settings(&self) -> AgentSettings {
        self.current_agent_settings.clone()
    }

    pub(super) fn refresh_model_display(&mut self) {
        let effective = self.effective_agent_settings();
        self.session_header.set_model(effective.model());
        // Keep composer paste affordances aligned with the currently effective model.
        self.sync_image_paste_enabled();
        self.sync_service_tier_commands();
        self.refresh_terminal_title();
        let effort = self.effective_reasoning_effort();
        self.bottom_pane
            .set_active_reasoning_effort(effort.as_ref());
    }

    /// Refresh every UI surface that depends on the effective model or reasoning effort.
    pub(super) fn refresh_model_dependent_surfaces(&mut self) {
        self.refresh_model_display();
        self.refresh_status_line();
    }

    fn apply_thread_settings(&mut self, mut settings: ThreadSettings) {
        let cwd_changed = self.config.cwd != settings.cwd;
        self.apply_thread_settings_cwd(settings.cwd.clone());
        self.config.model_provider_id = settings.model_provider.clone();
        self.set_service_tier(settings.service_tier.clone());
        self.config.personality = settings.personality;

        settings.agent_settings.settings.model = settings.model;
        settings.agent_settings.settings.reasoning_effort = settings.effort;
        self.set_effective_agent_settings(settings.agent_settings);
        self.refresh_effective_service_tier();
        self.refresh_status_surfaces();
        self.sync_service_tier_commands();
        self.sync_personality_command_enabled();
        if cwd_changed {
            self.refresh_skills_for_current_cwd(/*force_reload*/ true);
        }
        self.refresh_plugin_mentions();
        self.request_redraw();
    }

    fn apply_thread_settings_cwd(&mut self, cwd: AbsolutePathBuf) {
        let previous_cwd = std::mem::replace(&mut self.config.cwd, cwd.clone());
        self.current_cwd = Some(cwd.to_path_buf());
        self.status_line_project_root_name_cache = None;

        if !self.config.workspace_roots.contains(&previous_cwd) {
            return;
        }

        let previous_roots = std::mem::take(&mut self.config.workspace_roots);
        self.config.workspace_roots.push(cwd);
        for root in previous_roots {
            if root != previous_cwd && !self.config.workspace_roots.contains(&root) {
                self.config.workspace_roots.push(root);
            }
        }
        self.config
            .permissions
            .set_workspace_roots(self.config.workspace_roots.clone());
    }

    pub(super) fn set_effective_agent_settings(&mut self, settings: AgentSettings) {
        self.current_agent_settings = settings;
        self.refresh_model_dependent_surfaces();
    }

    pub(super) fn model_display_name(&self) -> &str {
        let model = self.current_model();
        if model.is_empty() {
            DEFAULT_MODEL_DISPLAY_NAME
        } else {
            model
        }
    }

    pub(super) fn update_goal_status_indicator(&mut self) {
        let goal_indicator = self.goal_status_indicator(Instant::now());
        self.current_goal_status_indicator = goal_indicator.clone();
        self.bottom_pane.set_goal_status_indicator(goal_indicator);
    }

    pub(super) fn refresh_goal_status_indicator_for_time_tick(&mut self) {
        let goal_indicator = self.goal_status_indicator(Instant::now());
        if goal_indicator != self.current_goal_status_indicator {
            self.current_goal_status_indicator = goal_indicator.clone();
            self.bottom_pane.set_goal_status_indicator(goal_indicator);
        }
    }

    fn goal_status_indicator(&self, now: Instant) -> Option<GoalStatusIndicator> {
        if !self.config.features.enabled(Feature::Goals) {
            return None;
        }
        self.current_goal_status.as_ref().and_then(|state| {
            state.indicator(now, self.turn_lifecycle.goal_status_active_turn_started_at)
        })
    }

    pub(super) fn on_thread_goal_updated(&mut self, goal: AppThreadGoal, turn_id: Option<String>) {
        if let Some(active_thread_id) = self.thread_id
            && active_thread_id.to_string() != goal.thread_id
        {
            return;
        }
        if !self.config.features.enabled(Feature::Goals) {
            self.current_goal_status_indicator = None;
            self.current_goal_status = None;
            self.update_goal_status_indicator();
            return;
        }
        if goal.status == AppThreadGoalStatus::BudgetLimited
            && let Some(turn_id) = turn_id
        {
            self.turn_lifecycle.mark_budget_limited(turn_id);
        }
        self.current_goal_status = Some(GoalStatusState::new(goal, Instant::now()));
        self.update_goal_status_indicator();
    }
}
