use super::session::Session;
use super::step_context::StepContext;
use crate::context::world_state::AgentsMdState;
use crate::context::world_state::ContextWindowGuidanceState;
use crate::context::world_state::EnvironmentsInstructionsState;
use crate::context::world_state::EnvironmentsState;
use crate::context::world_state::ModelInstructionsState;
use crate::context::world_state::MultiAgentModeState;
use crate::context::world_state::MultiAgentUsageHintState;
use crate::context::world_state::PersonalityState;
use crate::context::world_state::RealtimeState;
use crate::context::world_state::ToolsState;
use crate::context::world_state::WorldState;
use codex_extension_api::WorldStateContributionInput;
use codex_features::Feature;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;

impl Session {
    #[tracing::instrument(name = "world_state.build", level = "info", skip_all)]
    pub(crate) async fn build_world_state_for_step(
        &self,
        step_context: &StepContext,
    ) -> CodexResult<WorldState> {
        let turn_context = step_context.turn.as_ref();
        tracing::trace!(
            selected_capability_root_count = step_context.selected_capability_roots.len(),
            "building step world state"
        );
        let (previous_model, previous_context, base_instructions) = {
            let state = self.state.lock().await;
            (
                state
                    .previous_turn_settings()
                    .map(|previous| previous.model),
                state.reference_context_item(),
                state.session_configuration.base_instructions.clone(),
            )
        };
        let model_instructions = turn_context
            .model_info
            .get_model_instructions(turn_context.personality);
        let personality_is_baked = turn_context.model_info.supports_personality()
            && base_instructions == model_instructions;
        let environment_subagents = if turn_context.config.include_environment_context {
            self.services
                .agent_control
                .format_environment_context_subagents(self.thread_id)
                .await
        } else {
            String::new()
        };
        let mut world_state = WorldState::default();
        world_state.add_section(ModelInstructionsState::new(
            &turn_context.model_info.slug,
            previous_model.as_deref(),
            model_instructions,
        ));
        if self.features.enabled(Feature::Personality) {
            let personality_instructions = turn_context.personality.and_then(|personality| {
                turn_context
                    .model_info
                    .model_messages
                    .as_ref()
                    .and_then(|messages| messages.get_personality_message(Some(personality)))
                    .filter(|message| !message.is_empty())
            });
            world_state.add_section(PersonalityState::new(
                &turn_context.model_info.slug,
                turn_context.personality,
                previous_context
                    .as_ref()
                    .map(|previous| previous.model.as_str()),
                previous_context
                    .as_ref()
                    .and_then(|previous| previous.personality),
                personality_instructions,
                personality_is_baked,
            ));
        }
        if turn_context.config.features.enabled(Feature::TokenBudget)
            && turn_context.model_context_window().is_some()
            && let Some(guidance) = turn_context
                .config
                .token_budget
                .as_ref()
                .and_then(|config| config.guidance_message.as_deref())
                .filter(|message| !message.trim().is_empty())
        {
            world_state.add_section(ContextWindowGuidanceState::new(guidance));
        }
        let realtime_mode_instructions = self.conversation.mode_instructions().await;
        world_state.add_section(RealtimeState::new(
            turn_context.realtime_active,
            realtime_mode_instructions
                .as_ref()
                .and_then(|instructions| instructions.start.as_deref())
                .or(turn_context
                    .config
                    .experimental_realtime_start_instructions
                    .as_deref()),
            realtime_mode_instructions
                .as_ref()
                .and_then(|instructions| instructions.end.as_deref()),
        ));
        world_state.add_section(AgentsMdState::new(step_context.loaded_agents_md.as_deref()));
        if turn_context.config.include_environment_context {
            let current_date = self
                .services
                .time_provider
                .current_time(self.thread_id())
                .await
                .map_err(|err| CodexErr::Fatal(format!("failed to read current time: {err:#}")))?
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string();
            world_state.add_section(
                EnvironmentsState::from_turn_context_with_environments(
                    turn_context,
                    &step_context.environments,
                    Some(current_date),
                )
                .with_subagents(environment_subagents),
            );
        }
        world_state.add_section(EnvironmentsInstructionsState::new(
            turn_context.config.include_environment_context
                && turn_context
                    .config
                    .features
                    .enabled(Feature::DeferredExecutor),
        ));
        if turn_context
            .config
            .features
            .enabled(Feature::DeferredToolWorldState)
        {
            world_state.add_section(ToolsState::new(
                step_context.tool_router.deferred_tool_namespaces(),
            ));
        }
        let environments = step_context.environments.to_selections();
        let ready_selected_capability_roots = step_context
            .selected_capability_roots
            .iter()
            .map(|root| root.selected_root().clone())
            .collect::<Vec<_>>();
        for contributor in self.services.extensions.context_contributors() {
            for section in contributor
                .contribute_world_state(WorldStateContributionInput {
                    thread_id: self.thread_id(),
                    turn_id: turn_context.sub_id.as_str(),
                    environments: &environments,
                    ready_selected_capability_roots: &ready_selected_capability_roots,
                    executor_capability_discovery: step_context
                        .executor_capability_discovery
                        .as_deref(),
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store: turn_context.extension_data.as_ref(),
                })
                .await
            {
                world_state.add_extension_section(section);
            }
        }
        let mut multi_agent_mode = MultiAgentModeState::new(
            super::multi_agents::effective_multi_agent_mode(turn_context),
        );
        if let Some(usage_hint_text) =
            super::multi_agents::usage_hint_text(turn_context, &turn_context.session_source)
        {
            let usage_hint = MultiAgentUsageHintState::new(usage_hint_text);
            multi_agent_mode = multi_agent_mode.with_usage_hint(&usage_hint);
            world_state.add_section(usage_hint);
        }
        world_state.add_section(multi_agent_mode);
        Ok(world_state)
    }
}
