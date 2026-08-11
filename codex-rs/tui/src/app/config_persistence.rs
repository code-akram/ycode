//! Runtime configuration persistence helpers for the TUI app.
//!
//! This module owns the app-level glue between config.toml edits, in-memory `Config` refreshes,
//! and the ChatWidget copy of session settings, keeping persistence-heavy code out of the main app
//! loop.

use super::*;
use codex_config::ConfigLayerSource;

async fn build_config_on_runtime_worker(
    builder: ConfigBuilder,
    error_context: String,
) -> Result<Config> {
    match tokio::spawn(async move { builder.build().await }).await {
        Ok(build_result) => build_result.wrap_err(error_context),
        Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
        Err(err) => Err(err).wrap_err_with(|| format!("{error_context} task failed")),
    }
}

pub(super) fn resume_model_settings_for_overrides(
    config: &Config,
    harness_overrides: &ConfigOverrides,
) -> crate::runtime_session::ResumeModelSettings {
    let has_layer_override = config.config_layer_stack.layers_high_to_low().any(|layer| {
        matches!(
            &layer.name,
            ConfigLayerSource::SessionFlags
                | ConfigLayerSource::User {
                    profile: Some(_),
                    ..
                }
        ) && ["model", "model_provider", "model_reasoning_effort"]
            .iter()
            .any(|key| layer.config.get(*key).is_some())
    });
    if harness_overrides.model.is_some()
        || harness_overrides.model_provider.is_some()
        || has_layer_override
    {
        crate::runtime_session::ResumeModelSettings::OverrideFromCurrentConfig
    } else {
        crate::runtime_session::ResumeModelSettings::RestoreFromThread
    }
}

impl App {
    pub(super) async fn rebuild_config_for_cwd(&self, cwd: PathBuf) -> Result<Config> {
        let mut overrides = self.harness_overrides.clone();
        overrides.cwd = Some(cwd.clone());
        let cwd_display = cwd.display().to_string();
        let builder = ConfigBuilder::default()
            .codex_home(self.config.codex_home.to_path_buf())
            .cli_overrides(self.cli_kv_overrides.clone())
            .harness_overrides(overrides)
            .loader_overrides(self.loader_overrides.clone())
            .cloud_config_bundle(self.cloud_config_bundle.clone());
        build_config_on_runtime_worker(
            builder,
            format!("Failed to rebuild config for cwd {cwd_display}"),
        )
        .await
    }

    pub(super) async fn refresh_in_memory_config_from_disk(&mut self) -> Result<()> {
        let mut config = self
            .rebuild_config_for_cwd(self.chat_widget.config_ref().cwd.to_path_buf())
            .await?;
        self.config = config;
        self.chat_widget.sync_plugin_mentions_config(&self.config);
        Ok(())
    }

    pub(super) async fn refresh_in_memory_config_from_disk_best_effort(&mut self, action: &str) {
        if let Err(err) = self.refresh_in_memory_config_from_disk().await {
            tracing::warn!(
                error = %err,
                action,
                "failed to refresh config before thread transition; continuing with current in-memory config"
            );
        }
    }

    pub(super) async fn read_effective_config_after_overridden_write(
        &mut self,
        cli_runtime: &mut CliRuntimeSession,
        setting: &str,
    ) -> Option<ConfigReadResponse> {
        let cwd = self.chat_widget.config_ref().cwd.display().to_string();
        match crate::config_update::read_effective_config(cli_runtime.request_handle(), cwd).await {
            Ok(response) => Some(response),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    setting,
                    "failed to refresh effective config after an overridden write"
                );
                self.chat_widget.add_error_message(format!(
                    "{setting} were saved, but Codex could not refresh the effective config: {err}"
                ));
                None
            }
        }
    }

    pub(super) async fn rebuild_config_for_resume_or_fallback(
        &mut self,
        current_cwd: &Path,
        resume_cwd: PathBuf,
    ) -> Result<Config> {
        match self.rebuild_config_for_cwd(resume_cwd.clone()).await {
            Ok(config) => Ok(config),
            Err(err) => {
                if crate::session_resume::cwds_differ(current_cwd, &resume_cwd) {
                    Err(err)
                } else {
                    let resume_cwd_display = resume_cwd.display().to_string();
                    tracing::warn!(
                        error = %err,
                        cwd = %resume_cwd_display,
                        "failed to rebuild config for same-cwd resume; using current in-memory config"
                    );
                    Ok(self.config.clone())
                }
            }
        }
    }

    pub(super) async fn update_feature_flags(
        &mut self,
        cli_runtime: &mut CliRuntimeSession,
        updates: Vec<(Feature, bool)>,
    ) {
        if updates.is_empty() {
            return;
        }

        let mut next_config = self.config.clone();
        let mut feature_updates_to_apply = Vec::with_capacity(updates.len());
        let mut config_edits = Vec::new();

        for (feature, enabled) in updates {
            let feature_key = feature.key();
            let mut feature_edits = Vec::new();
            let mut feature_config = next_config.clone();
            if let Err(err) = feature_config.features.set_enabled(feature, enabled) {
                tracing::error!(
                    error = %err,
                    feature = feature_key,
                    "failed to update constrained feature flags"
                );
                self.chat_widget.add_error_message(format!(
                    "Failed to update experimental feature `{feature_key}`: {err}"
                ));
                continue;
            }
            let effective_enabled = feature_config.features.enabled(feature);
            next_config = feature_config;
            feature_updates_to_apply.push((feature, effective_enabled));
            config_edits.extend(feature_edits);
            config_edits.push(crate::config_update::build_feature_enabled_edit(
                feature_key,
                effective_enabled,
            ));
        }

        // Persist first so the live session does not diverge from disk if the
        // config edit fails. Runtime/UI state is patched below only after the
        // durable config update succeeds.
        let write_response = match crate::config_update::write_config_batch(
            cli_runtime.request_handle(),
            config_edits,
        )
        .await
        {
            Ok(response) => response,
            Err(err) => {
                let error = crate::config_update::format_config_error(&err);
                tracing::error!(error = %error, "failed to persist feature flags");
                self.chat_widget
                    .add_error_message(format!("Failed to update experimental features: {error}"));
                return;
            }
        };
        if write_response.status == WriteStatus::OkOverridden {
            let message = overridden_write_message(&write_response);
            tracing::warn!(
                message,
                "feature flag config write was overridden by effective config"
            );
            self.chat_widget.add_error_message(format!(
                "Experimental feature changes were saved but not applied: {message}"
            ));
            if let Some(effective_config) = self
                .read_effective_config_after_overridden_write(
                    cli_runtime,
                    "Experimental feature changes",
                )
                .await
            {
                self.sync_feature_state_from_effective_config(
                    &effective_config,
                    &feature_updates_to_apply,
                );
            }
            return;
        }

        let memory_tool_was_enabled = self.config.features.enabled(Feature::MemoryTool);
        self.config = next_config;
        let show_memory_enable_notice =
            feature_updates_to_apply.iter().any(|(feature, enabled)| {
                *feature == Feature::MemoryTool && *enabled && !memory_tool_was_enabled
            });
        for (feature, effective_enabled) in feature_updates_to_apply {
            self.chat_widget
                .set_feature_enabled(feature, effective_enabled);
        }
        if show_memory_enable_notice {
            self.chat_widget.add_memories_enable_notice();
        }
    }

    pub(super) async fn update_memory_settings(
        &mut self,
        cli_runtime: &mut CliRuntimeSession,
        use_memories: bool,
        generate_memories: bool,
    ) -> bool {
        let edits =
            crate::config_update::build_memory_settings_edits(use_memories, generate_memories);

        let write_response =
            match crate::config_update::write_config_batch(cli_runtime.request_handle(), edits)
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    tracing::error!(error = %err, "failed to persist memory settings");
                    self.chat_widget
                        .add_error_message(format!("Failed to save memory settings: {err}"));
                    return false;
                }
            };
        if write_response.status == WriteStatus::OkOverridden {
            let message = overridden_write_message(&write_response);
            tracing::warn!(
                message,
                "memory settings config write was overridden by effective config"
            );
            self.chat_widget.add_error_message(format!(
                "Memory setting changes were saved but not applied: {message}"
            ));
            let Some(effective_config) = self
                .read_effective_config_after_overridden_write(cli_runtime, "Memory setting changes")
                .await
            else {
                return false;
            };
            return self.sync_memory_state_from_effective_config(&effective_config);
        }

        self.config.memories.use_memories = use_memories;
        self.config.memories.generate_memories = generate_memories;
        self.chat_widget
            .set_memory_settings(use_memories, generate_memories);
        true
    }

    pub(super) async fn update_memory_settings_with_cli_runtime(
        &mut self,
        cli_runtime: &mut CliRuntimeSession,
        use_memories: bool,
        generate_memories: bool,
    ) {
        let previous_generate_memories = self.config.memories.generate_memories;
        if !self
            .update_memory_settings(cli_runtime, use_memories, generate_memories)
            .await
        {
            return;
        }

        let generate_memories = self.config.memories.generate_memories;
        if previous_generate_memories == generate_memories {
            return;
        }

        let Some(thread_id) = self.current_displayed_thread_id() else {
            return;
        };

        let mode = if generate_memories {
            ThreadMemoryMode::Enabled
        } else {
            ThreadMemoryMode::Disabled
        };

        if let Err(err) = cli_runtime.thread_memory_mode_set(thread_id, mode).await {
            tracing::error!(error = %err, %thread_id, "failed to update thread memory mode");
            self.chat_widget.add_error_message(format!(
                "Saved memory settings, but failed to update the current thread: {err}"
            ));
        }
    }

    pub(super) async fn reset_memories_with_cli_runtime(
        &mut self,
        cli_runtime: &mut CliRuntimeSession,
    ) {
        if let Err(err) = cli_runtime.memory_reset().await {
            tracing::error!(error = %err, "failed to reset memories");
            self.chat_widget
                .add_error_message(format!("Failed to reset memories: {err}"));
            return;
        }

        self.chat_widget
            .add_info_message("Reset local memories.".to_string(), /*hint*/ None);
    }

    pub(super) fn reasoning_label(reasoning_effort: Option<&ReasoningEffortConfig>) -> String {
        match reasoning_effort {
            None | Some(ReasoningEffortConfig::None) => "default".to_string(),
            Some(reasoning_effort) => reasoning_effort.as_str().to_string(),
        }
    }

    pub(super) fn reasoning_label_for(
        model: &str,
        reasoning_effort: Option<&ReasoningEffortConfig>,
    ) -> Option<String> {
        (!model.starts_with("codex-auto-")).then(|| Self::reasoning_label(reasoning_effort))
    }

    pub(crate) fn token_usage(&self) -> crate::token_usage::TokenUsage {
        self.chat_widget.token_usage()
    }

    pub(super) fn on_update_reasoning_effort(&mut self, effort: Option<ReasoningEffortConfig>) {
        // TODO(aibrahim): Remove this and don't use config as a state object.
        // Instead, explicitly pass the stored agent settings's effort into new sessions.
        self.config.model_reasoning_effort = effort.clone();
        self.chat_widget.set_reasoning_effort(effort.clone());
    }

    pub(super) fn on_apply_advanced_reasoning(
        &mut self,
        model: &str,
        effort: ReasoningEffortConfig,
    ) -> Option<ReasoningEffortConfig> {
        let default_effort = self.default_reasoning_effort_for_conversation_model(model);
        if let Some(default_effort) = default_effort.as_ref() {
            self.config.model = Some(model.to_string());
            self.config.model_reasoning_effort = Some(default_effort.clone());
        }
        self.chat_widget.set_model(model);
        self.chat_widget.set_reasoning_effort(Some(effort.clone()));
        default_effort
    }

    fn default_reasoning_effort_for_conversation_model(
        &self,
        model: &str,
    ) -> Option<ReasoningEffortConfig> {
        let configured_effort = self
            .config
            .model_reasoning_effort
            .as_ref()
            .filter(|effort| **effort != ReasoningEffortConfig::Ultra);
        let preset = self
            .model_catalog
            .try_list_models()
            .ok()?
            .into_iter()
            .find(|preset| preset.model == model)?;
        let supported = &preset.supported_reasoning_efforts;

        configured_effort
            .filter(|effort| supported.iter().any(|option| option.effort == **effort))
            .cloned()
            .or_else(|| {
                (preset.default_reasoning_effort != ReasoningEffortConfig::Ultra)
                    .then_some(preset.default_reasoning_effort)
            })
            .or_else(|| {
                supported
                    .iter()
                    .find(|option| option.effort != ReasoningEffortConfig::Ultra)
                    .map(|option| option.effort.clone())
            })
    }

    pub(super) fn resume_model_settings(&self) -> crate::runtime_session::ResumeModelSettings {
        resume_model_settings_for_overrides(&self.config, &self.harness_overrides)
    }

    pub(super) fn on_update_personality(&mut self, personality: Personality) {
        self.config.personality = Some(personality);
        self.chat_widget.set_personality(personality);
    }

    pub(super) fn sync_tui_theme_selection(&mut self, name: String) {
        self.config.tui_theme = Some(name.clone());
        self.chat_widget.set_tui_theme(Some(name));
    }

    #[cfg(test)]
    pub(super) fn sync_tui_pet_selection(&mut self, pet: String) {
        self.config.tui_pet = Some(pet.clone());
        self.chat_widget.set_tui_pet(Some(pet));
    }

    pub(super) fn sync_tui_pet_disabled(&mut self) {
        let pet = crate::pets::DISABLED_PET_ID.to_string();
        self.config.tui_pet = Some(pet.clone());
        self.chat_widget.set_tui_pet(Some(pet));
    }

    pub(super) fn restore_runtime_theme_from_config(&self) {
        if let Some(name) = self.config.tui_theme.as_deref()
            && let Some(theme) =
                crate::render::highlight::resolve_theme_by_name(name, Some(&self.config.codex_home))
        {
            crate::render::highlight::set_syntax_theme(theme);
            return;
        }

        let auto_theme_name = crate::render::highlight::adaptive_default_theme_name();
        if let Some(theme) = crate::render::highlight::resolve_theme_by_name(
            auto_theme_name,
            Some(&self.config.codex_home),
        ) {
            crate::render::highlight::set_syntax_theme(theme);
        }
    }

    pub(super) fn personality_label(personality: Personality) -> &'static str {
        match personality {
            Personality::None => "None",
            Personality::Friendly => "Friendly",
            Personality::Pragmatic => "Pragmatic",
        }
    }

    fn sync_feature_state_from_effective_config(
        &mut self,
        effective_config: &ConfigReadResponse,
        feature_updates: &[(Feature, bool)],
    ) {
        for (feature, _) in feature_updates {
            let enabled = feature_enabled_from_effective_config(effective_config, *feature);
            if let Err(err) = self.config.features.set_enabled(*feature, enabled) {
                tracing::warn!(
                    error = %err,
                    feature = feature.key(),
                    "failed to sync effective feature state after an overridden write"
                );
                continue;
            }
            self.chat_widget.set_feature_enabled(*feature, enabled);
        }
    }

    fn sync_memory_state_from_effective_config(
        &mut self,
        effective_config: &ConfigReadResponse,
    ) -> bool {
        let Some(memories) = memories_from_effective_config(effective_config) else {
            tracing::warn!(
                "config/read omitted memories after an overridden memory settings write"
            );
            return false;
        };
        let use_memories = memories
            .use_memories
            .unwrap_or(self.config.memories.use_memories);
        let generate_memories = memories
            .generate_memories
            .unwrap_or(self.config.memories.generate_memories);
        self.config.memories.use_memories = use_memories;
        self.config.memories.generate_memories = generate_memories;
        self.chat_widget
            .set_memory_settings(use_memories, generate_memories);
        true
    }
}

fn overridden_write_message(write_response: &ConfigWriteResponse) -> &str {
    write_response
        .overridden_metadata
        .as_ref()
        .map(|metadata| metadata.message.as_str())
        .unwrap_or("the effective config is overridden by a higher-priority layer")
}

fn feature_enabled_from_effective_config(
    effective_config: &ConfigReadResponse,
    feature: Feature,
) -> bool {
    let root_features = effective_config
        .config
        .additional
        .get("features")
        .and_then(features_toml_from_json);
    root_features
        .as_ref()
        .and_then(|features| features.entries().get(feature.key()).copied())
        .unwrap_or_else(|| feature.default_enabled())
}

fn memories_from_effective_config(effective_config: &ConfigReadResponse) -> Option<MemoriesToml> {
    effective_config
        .config
        .additional
        .get("memories")
        .and_then(|memories| serde_json::from_value(memories.clone()).ok())
}

fn features_toml_from_json(value: &serde_json::Value) -> Option<FeaturesToml> {
    serde_json::from_value(value.clone()).ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::app::test_support::app_enabled_in_effective_config;
    use crate::app::test_support::make_test_app;
    use crate::legacy_core::config::edit::ConfigEdit;
    use crate::test_support::PathBufExt;
    use codex_config::ConfigLayerEntry;
    use codex_config::ConfigLayerStack;
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::openai_models::ReasoningEffortPreset;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[tokio::test]
    async fn update_reasoning_effort_updates_agent_settings() {
        let mut app = make_test_app().await;
        app.chat_widget
            .set_reasoning_effort(Some(ReasoningEffortConfig::Medium));

        app.on_update_reasoning_effort(Some(ReasoningEffortConfig::High));

        assert_eq!(
            app.chat_widget.current_reasoning_effort(),
            Some(ReasoningEffortConfig::High)
        );
        assert_eq!(
            app.config.model_reasoning_effort,
            Some(ReasoningEffortConfig::High)
        );
    }

    #[tokio::test]
    async fn conversation_reasoning_uses_compatible_default_for_new_threads() {
        for (configured_effort, expected_default_effort) in [
            (ReasoningEffortConfig::Low, ReasoningEffortConfig::Low),
            (
                ReasoningEffortConfig::Custom("unsupported".to_string()),
                ReasoningEffortConfig::Medium,
            ),
            (ReasoningEffortConfig::Ultra, ReasoningEffortConfig::Medium),
        ] {
            let mut app = make_test_app().await;
            app.config.model = Some("gpt-5.4".to_string());
            app.config.model_reasoning_effort = Some(configured_effort.clone());
            app.chat_widget
                .set_reasoning_effort(Some(configured_effort));

            let default_effort =
                app.on_apply_advanced_reasoning("gpt-5.4", ReasoningEffortConfig::Ultra);
            let new_thread_config = app.fresh_session_config();

            assert_eq!(default_effort, Some(expected_default_effort.clone()));
            assert_eq!(app.chat_widget.current_model(), "gpt-5.4");
            assert_eq!(
                app.chat_widget.current_reasoning_effort(),
                Some(ReasoningEffortConfig::Ultra)
            );
            assert_eq!(
                (
                    new_thread_config.model.as_deref(),
                    new_thread_config.model_reasoning_effort,
                ),
                (Some("gpt-5.4"), Some(expected_default_effort))
            );
        }
    }

    #[tokio::test]
    async fn conversation_reasoning_keeps_previous_default_for_ultra_only_model() {
        let mut app = make_test_app().await;
        app.config.model = Some("gpt-5.4".to_string());
        app.config.model_reasoning_effort = Some(ReasoningEffortConfig::Low);
        let mut preset = app
            .model_catalog
            .try_list_models()
            .expect("model catalog is infallible")
            .into_iter()
            .find(|preset| preset.model == "gpt-5.4")
            .expect("gpt-5.4 preset");
        preset.model = "ultra-only".to_string();
        preset.default_reasoning_effort = ReasoningEffortConfig::Ultra;
        preset.supported_reasoning_efforts = vec![ReasoningEffortPreset {
            effort: ReasoningEffortConfig::Ultra,
            description: "Ultra reasoning".to_string(),
        }];
        app.model_catalog = Arc::new(ModelCatalog::new(vec![preset]));

        let default_effort =
            app.on_apply_advanced_reasoning("ultra-only", ReasoningEffortConfig::Ultra);
        let new_thread_config = app.fresh_session_config();

        assert_eq!(default_effort, None);
        assert_eq!(app.chat_widget.current_model(), "ultra-only");
        assert_eq!(
            app.chat_widget.current_reasoning_effort(),
            Some(ReasoningEffortConfig::Ultra)
        );
        assert_eq!(
            (
                new_thread_config.model.as_deref(),
                new_thread_config.model_reasoning_effort,
            ),
            (Some("gpt-5.4"), Some(ReasoningEffortConfig::Low))
        );
    }

    #[tokio::test]
    async fn resume_model_settings_preserves_only_explicit_model_overrides() {
        let mut app = make_test_app().await;

        assert_eq!(
            app.resume_model_settings(),
            crate::runtime_session::ResumeModelSettings::RestoreFromThread
        );
        let profile_path = test_path_buf("/tmp/work.config.toml").abs();
        let profile = "work"
            .parse::<codex_config::ProfileV2Name>()
            .expect("valid profile name");
        for (key, expected) in [
            (
                "model",
                crate::runtime_session::ResumeModelSettings::OverrideFromCurrentConfig,
            ),
            (
                "model_provider",
                crate::runtime_session::ResumeModelSettings::OverrideFromCurrentConfig,
            ),
            (
                "model_reasoning_effort",
                crate::runtime_session::ResumeModelSettings::OverrideFromCurrentConfig,
            ),
            (
                "sandbox_mode",
                crate::runtime_session::ResumeModelSettings::RestoreFromThread,
            ),
        ] {
            let config = TomlValue::Table(toml::map::Map::from_iter([(
                key.to_string(),
                TomlValue::String("value".to_string()),
            )]));
            app.config.config_layer_stack = ConfigLayerStack::new(
                vec![ConfigLayerEntry::new(
                    ConfigLayerSource::SessionFlags,
                    config.clone(),
                )],
                Default::default(),
                Default::default(),
            )
            .expect("session flags layer stack");
            assert_eq!(app.resume_model_settings(), expected);

            app.config.config_layer_stack = ConfigLayerStack::default()
                .with_user_config_profile(&profile_path, Some(&profile), config)
                .expect("user config profile layer stack");
            assert_eq!(app.resume_model_settings(), expected);
        }

        app.config.config_layer_stack = ConfigLayerStack::default()
            .with_user_config(
                &profile_path,
                TomlValue::Table(toml::map::Map::from_iter([(
                    "model_reasoning_effort".to_string(),
                    TomlValue::String("high".to_string()),
                )])),
            )
            .expect("user config layer stack");
        assert_eq!(
            app.resume_model_settings(),
            crate::runtime_session::ResumeModelSettings::RestoreFromThread
        );

        app.harness_overrides.model_provider = Some("custom-provider".to_string());
        assert_eq!(
            app.resume_model_settings(),
            crate::runtime_session::ResumeModelSettings::OverrideFromCurrentConfig
        );
    }

    #[tokio::test]
    async fn refresh_in_memory_config_from_disk_loads_latest_apps_state() -> Result<()> {
        let mut app = make_test_app().await;
        let codex_home = tempdir()?;
        app.config.codex_home = codex_home.path().to_path_buf().abs();
        let app_id = "unit_test_refresh_in_memory_config_connector".to_string();

        assert_eq!(app_enabled_in_effective_config(&app.config, &app_id), None);

        ConfigEditsBuilder::for_config(&app.config)
            .with_edits([
                ConfigEdit::SetPath {
                    segments: vec!["apps".to_string(), app_id.clone(), "enabled".to_string()],
                    value: false.into(),
                },
                ConfigEdit::SetPath {
                    segments: vec![
                        "apps".to_string(),
                        app_id.clone(),
                        "disabled_reason".to_string(),
                    ],
                    value: "user".into(),
                },
            ])
            .apply()
            .await
            .expect("persist app toggle");

        assert_eq!(app_enabled_in_effective_config(&app.config, &app_id), None);

        app.refresh_in_memory_config_from_disk().await?;

        assert_eq!(
            app_enabled_in_effective_config(&app.config, &app_id),
            Some(false)
        );
        Ok(())
    }

    // Regression coverage for `/new` and `/clear`: cloud requirements
    // must survive the config refresh that runs before thread transitions.
    #[tokio::test]
    async fn refresh_in_memory_config_from_disk_keeps_cloud_requirements_for_thread_transitions()
    -> Result<()> {
        let mut app = make_test_app().await;
        let codex_home = tempdir()?;
        let required_policy = codex_protocol::protocol::AskForApproval::Never;
        let cloud_config_bundle =
            codex_config::test_support::CloudConfigBundleFixture::loader_with_enterprise_requirement(
                r#"allowed_approval_policies = ["never"]"#,
            );

        let config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .cloud_config_bundle(cloud_config_bundle.clone())
            .build()
            .await?;
        app.config = config;
        app.cloud_config_bundle = cloud_config_bundle;
        let app_id = "unit_test_cloud_requirements_reload_marker";
        std::fs::write(
            codex_home.path().join("config.toml"),
            format!(
                r#"
[apps.{app_id}]
enabled = false
"#
            ),
        )?;

        let assert_cloud_requirements = |app: &App| {
            let config = app.fresh_session_config();
            assert_eq!(
                config
                    .config_layer_stack
                    .requirements_toml()
                    .allowed_approval_policies
                    .clone(),
                Some(vec![required_policy])
            );
            assert_eq!(config.permissions.approval_policy.value(), required_policy);
        };

        assert_cloud_requirements(&app);
        assert_eq!(app_enabled_in_effective_config(&app.config, app_id), None);

        // This is the fallible reload that the best-effort `/new`, `/clear`,
        // `/fork`, side-conversation, and session-picker paths wrap.
        app.refresh_in_memory_config_from_disk().await?;

        assert_eq!(
            app_enabled_in_effective_config(&app.config, app_id),
            Some(false)
        );
        assert_cloud_requirements(&app);
        Ok(())
    }

    #[tokio::test]
    async fn refresh_in_memory_config_from_disk_best_effort_keeps_current_config_on_error()
    -> Result<()> {
        let mut app = make_test_app().await;
        let codex_home = tempdir()?;
        app.config.codex_home = codex_home.path().to_path_buf().abs();
        std::fs::write(codex_home.path().join("config.toml"), "[broken")?;
        let original_config = app.config.clone();

        app.refresh_in_memory_config_from_disk_best_effort("starting a new thread")
            .await;

        assert_eq!(app.config, original_config);
        Ok(())
    }

    #[tokio::test]
    async fn refresh_in_memory_config_from_disk_uses_active_chat_widget_cwd() -> Result<()> {
        let mut app = make_test_app().await;
        let original_cwd = app.config.cwd.clone();
        let next_cwd_tmp = tempdir()?;
        let next_cwd = next_cwd_tmp.path().to_path_buf();

        app.chat_widget
            .handle_thread_session(crate::session_state::ThreadSessionState {
                thread_id: ThreadId::new(),
                forked_from_id: None,
                fork_parent_title: None,
                thread_name: None,
                model: "gpt-test".to_string(),
                model_provider_id: "test-provider".to_string(),
                service_tier: None,
                approval_policy: AskForApproval::Never,
                approvals_reviewer: ApprovalsReviewer::User,
                permission_profile: PermissionProfile::read_only(),
                active_permission_profile: None,
                cwd: next_cwd.clone().abs(),
                runtime_workspace_roots: Vec::new(),
                instruction_source_paths: Vec::new(),
                reasoning_effort: None,
                agent_settings: None,
                personality: None,
                message_history: None,
                network_proxy: None,
                rollout_path: Some(PathBuf::new()),
            });

        assert_eq!(app.chat_widget.config_ref().cwd.to_path_buf(), next_cwd);
        assert_eq!(app.config.cwd, original_cwd);

        app.refresh_in_memory_config_from_disk().await?;

        assert_eq!(app.config.cwd, app.chat_widget.config_ref().cwd);
        Ok(())
    }

    #[tokio::test]
    async fn refresh_in_memory_config_from_disk_updates_resize_reflow_config() -> Result<()> {
        let mut app = make_test_app().await;
        let codex_home = tempdir()?;
        app.config.codex_home = codex_home.path().to_path_buf().abs();
        std::fs::write(
            codex_home.path().join("config.toml"),
            r#"
[tui]
terminal_resize_reflow_max_rows = 9000
"#,
        )?;

        app.refresh_in_memory_config_from_disk().await?;

        assert_eq!(
            app.config.terminal_resize_reflow.max_rows,
            crate::legacy_core::config::TerminalResizeReflowMaxRows::Limit(9000)
        );
        Ok(())
    }

    #[tokio::test]
    async fn overridden_disabled_guardian_does_not_apply_auto_review_companions() -> Result<()> {
        let mut app = make_test_app().await;
        let original_policy = app.config.permissions.approval_policy.value();
        let effective_config: ConfigReadResponse = serde_json::from_value(serde_json::json!({
            "config": {
                "approval_policy": AskForApproval::OnRequest,
                "approvals_reviewer": codex_cli_protocol::ApprovalsReviewer::AutoReview,
                "sandbox_mode": CliRuntimeSandboxMode::WorkspaceWrite,
                "features": {
                    "guardian_approval": false,
                },
            },
            "origins": {},
        }))?;

        app.sync_feature_state_from_effective_config(
            &effective_config,
            &[(Feature::GuardianApproval, /*enabled*/ true)],
        );

        assert!(!app.config.features.enabled(Feature::GuardianApproval));
        assert!(
            !app.chat_widget
                .config_ref()
                .features
                .enabled(Feature::GuardianApproval)
        );
        assert_eq!(app.config.approvals_reviewer, ApprovalsReviewer::User);
        assert_eq!(
            app.chat_widget.config_ref().approvals_reviewer,
            ApprovalsReviewer::User
        );
        assert_eq!(
            app.config.permissions.approval_policy.value(),
            original_policy
        );
        Ok(())
    }

    #[tokio::test]
    async fn rebuild_config_for_resume_or_fallback_uses_current_config_on_same_cwd_error()
    -> Result<()> {
        let mut app = make_test_app().await;
        let codex_home = tempdir()?;
        app.config.codex_home = codex_home.path().to_path_buf().abs();
        std::fs::write(codex_home.path().join("config.toml"), "[broken")?;
        let current_config = app.config.clone();
        let current_cwd = current_config.cwd.clone();

        let resume_config = app
            .rebuild_config_for_resume_or_fallback(&current_cwd, current_cwd.to_path_buf())
            .await?;

        assert_eq!(resume_config, current_config);
        Ok(())
    }

    #[tokio::test]
    async fn rebuild_config_for_resume_or_fallback_errors_when_cwd_changes() -> Result<()> {
        let mut app = make_test_app().await;
        let codex_home = tempdir()?;
        app.config.codex_home = codex_home.path().to_path_buf().abs();
        std::fs::write(codex_home.path().join("config.toml"), "[broken")?;
        let current_cwd = app.config.cwd.clone();
        let next_cwd_tmp = tempdir()?;
        let next_cwd = next_cwd_tmp.path().to_path_buf();

        let result = app
            .rebuild_config_for_resume_or_fallback(&current_cwd, next_cwd)
            .await;

        assert!(result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn sync_tui_theme_selection_updates_chat_widget_config_copy() {
        let mut app = make_test_app().await;

        app.sync_tui_theme_selection("dracula".to_string());

        assert_eq!(app.config.tui_theme.as_deref(), Some("dracula"));
        assert_eq!(
            app.chat_widget.config_ref().tui_theme.as_deref(),
            Some("dracula")
        );
    }

    #[tokio::test]
    async fn sync_tui_pet_selection_updates_chat_widget_config_copy() {
        let mut app = make_test_app().await;

        app.sync_tui_pet_selection("chefito".to_string());

        assert_eq!(app.config.tui_pet.as_deref(), Some("chefito"));
        assert_eq!(
            app.chat_widget.config_ref().tui_pet.as_deref(),
            Some("chefito")
        );
    }

    #[tokio::test]
    async fn sync_tui_pet_disabled_updates_chat_widget_config_copy() {
        let mut app = make_test_app().await;

        app.sync_tui_pet_disabled();

        assert_eq!(
            app.config.tui_pet.as_deref(),
            Some(crate::pets::DISABLED_PET_ID)
        );
        assert_eq!(
            app.chat_widget.config_ref().tui_pet.as_deref(),
            Some(crate::pets::DISABLED_PET_ID)
        );
    }
}
