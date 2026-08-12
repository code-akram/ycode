use std::sync::Arc;

use crate::config_manager::ConfigManager;
use crate::config_manager_service::ConfigManagerError;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::ConnectionRequestId;
use crate::outgoing_message::OutgoingMessageSender;
use codex_cli_protocol::BrowserUseRequirements;
use codex_cli_protocol::ClientResponsePayload;
use codex_cli_protocol::ComputerUseRequirements;
use codex_cli_protocol::ConfigBatchWriteParams;
use codex_cli_protocol::ConfigReadParams;
use codex_cli_protocol::ConfigReadResponse;
use codex_cli_protocol::ConfigRequirements;
use codex_cli_protocol::ConfigRequirementsReadResponse;
use codex_cli_protocol::ConfigValueWriteParams;
use codex_cli_protocol::ConfigWriteErrorCode;
use codex_cli_protocol::ConfigWriteResponse;
use codex_cli_protocol::JSONRPCErrorError;
use codex_cli_protocol::ModelsRequirements;
use codex_cli_protocol::NetworkDomainPermission;
use codex_cli_protocol::NetworkRequirements;
use codex_cli_protocol::NetworkUnixSocketPermission;
use codex_cli_protocol::NewThreadModelDefaults;
use codex_cli_protocol::SandboxMode;
use codex_config::ConfigRequirementsToml;
use codex_config::ResidencyRequirement as CoreResidencyRequirement;
use codex_config::SandboxModeRequirement as CoreSandboxModeRequirement;
use codex_core::ThreadManager;
use codex_features::feature_for_key;
use codex_protocol::config_types::WebSearchMode;
use serde_json::json;
use std::path::PathBuf;

const SUPPORTED_EXPERIMENTAL_FEATURE_ENABLEMENT: &[&str] = &[
    "auth_elicitation",
    "memories",
    "mentions_v2",
    "remote_control",
    "tool_suggest",
];

#[derive(Clone)]
pub(crate) struct ConfigRequestProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    config_manager: ConfigManager,
    thread_manager: Arc<ThreadManager>,
}

impl ConfigRequestProcessor {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        config_manager: ConfigManager,
        thread_manager: Arc<ThreadManager>,
    ) -> Self {
        Self {
            outgoing,
            config_manager,
            thread_manager,
        }
    }

    pub(crate) async fn read(
        &self,
        params: ConfigReadParams,
    ) -> Result<ConfigReadResponse, JSONRPCErrorError> {
        let fallback_cwd = params.cwd.as_ref().map(PathBuf::from);
        let mut response = self.config_manager.read(params).await.map_err(map_error)?;
        let config = self.load_latest_config(fallback_cwd).await?;
        for feature_key in SUPPORTED_EXPERIMENTAL_FEATURE_ENABLEMENT {
            let Some(feature) = feature_for_key(feature_key) else {
                continue;
            };
            let features = response
                .config
                .additional
                .entry("features".to_string())
                .or_insert_with(|| json!({}));
            if !features.is_object() {
                *features = json!({});
            }
            if let Some(features) = features.as_object_mut() {
                features.insert(
                    (*feature_key).to_string(),
                    json!(config.features.enabled(feature)),
                );
            }
        }
        Ok(response)
    }

    pub(crate) async fn value_write(
        &self,
        params: ConfigValueWriteParams,
    ) -> Result<ClientResponsePayload, JSONRPCErrorError> {
        self.handle_config_mutation_result(self.write_value(params).await)
            .await
            .map(ClientResponsePayload::ConfigValueWrite)
    }

    pub(crate) async fn batch_write(
        &self,
        params: ConfigBatchWriteParams,
    ) -> Result<ClientResponsePayload, JSONRPCErrorError> {
        let session_defaults_only = !params.edits.is_empty()
            && params.edits.iter().all(|edit| {
                matches!(
                    edit.key_path.as_str(),
                    "model" | "model_reasoning_effort" | "service_tier" | "personality"
                )
            });
        let reload_user_config = params.reload_user_config;
        let response = self.batch_write_inner(params).await?;
        if !session_defaults_only {
            self.handle_config_mutation().await;
            if reload_user_config {
                self.reload_user_config().await;
            }
        }
        Ok(ClientResponsePayload::ConfigBatchWrite(response))
    }

    pub(crate) async fn handle_config_mutation(&self) {
        self.thread_manager.skills_service().clear_cache();
    }

    async fn handle_config_mutation_result<T>(
        &self,
        result: std::result::Result<T, JSONRPCErrorError>,
    ) -> Result<T, JSONRPCErrorError> {
        let response = result?;
        self.handle_config_mutation().await;
        Ok(response)
    }

    async fn load_latest_config(
        &self,
        fallback_cwd: Option<PathBuf>,
    ) -> Result<codex_core::config::Config, JSONRPCErrorError> {
        self.config_manager
            .load_latest_config(fallback_cwd)
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to resolve feature override precedence: {err}"
                ))
            })
    }

    async fn write_value(
        &self,
        params: ConfigValueWriteParams,
    ) -> Result<ConfigWriteResponse, JSONRPCErrorError> {
        self.config_manager
            .write_value(params)
            .await
            .map_err(map_error)
    }

    async fn batch_write_inner(
        &self,
        params: ConfigBatchWriteParams,
    ) -> Result<ConfigWriteResponse, JSONRPCErrorError> {
        self.config_manager
            .batch_write(params)
            .await
            .map_err(map_error)
    }

    async fn reload_user_config(&self) {
        match self.load_latest_config(/*fallback_cwd*/ None).await {
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(
                    "failed to rebuild user config for runtime refresh: {}",
                    err.message
                );
                return;
            }
        };
        let thread_ids = self.thread_manager.list_thread_ids().await;
        for thread_id in thread_ids {
            let Ok(thread) = self.thread_manager.get_thread(thread_id).await else {
                continue;
            };
            let current_config = thread.config().await;
            let next_config = match self
                .config_manager
                .load_latest_config_for_thread(current_config.as_ref())
                .await
            {
                Ok(config) => config,
                Err(err) => {
                    tracing::warn!(%thread_id, %err, "failed to reload thread configuration");
                    continue;
                }
            };
            thread.refresh_runtime_config(next_config).await;
        }
    }
}

fn map_requirements_toml_to_api(requirements: ConfigRequirementsToml) -> ConfigRequirements {
    ConfigRequirements {
        allowed_approval_policies: requirements.allowed_approval_policies.map(|policies| {
            policies
                .into_iter()
                .map(codex_cli_protocol::AskForApproval::from)
                .collect()
        }),
        allowed_approvals_reviewers: requirements.allowed_approvals_reviewers.map(|reviewers| {
            reviewers
                .into_iter()
                .map(codex_cli_protocol::ApprovalsReviewer::from)
                .collect()
        }),
        allowed_sandbox_modes: requirements.allowed_sandbox_modes.map(|modes| {
            modes
                .into_iter()
                .filter_map(map_sandbox_mode_requirement_to_api)
                .collect()
        }),
        allowed_permission_profiles: requirements.allowed_permission_profiles,
        default_permissions: requirements.default_permissions,
        allowed_web_search_modes: requirements.allowed_web_search_modes.map(|modes| {
            let mut normalized = modes
                .into_iter()
                .map(Into::into)
                .collect::<Vec<WebSearchMode>>();
            if !normalized.contains(&WebSearchMode::Disabled) {
                normalized.push(WebSearchMode::Disabled);
            }
            normalized
        }),
        allow_appshots: requirements.allow_appshots,
        allow_remote_control: requirements.allow_remote_control,
        computer_use: requirements
            .computer_use
            .map(map_computer_use_requirements_to_api),
        browser_use: requirements
            .browser_use
            .map(map_browser_use_requirements_to_api),
        feature_requirements: requirements
            .feature_requirements
            .map(|requirements| requirements.entries),
        enforce_residency: requirements
            .enforce_residency
            .map(map_residency_requirement_to_api),
        network: requirements.network.map(map_network_requirements_to_api),
        models: requirements.models.map(|models| ModelsRequirements {
            new_thread: models.new_thread.map(|new_thread| NewThreadModelDefaults {
                model: new_thread.model,
                model_reasoning_effort: new_thread.model_reasoning_effort,
                service_tier: new_thread.service_tier,
            }),
        }),
        sqlite_home: requirements.sqlite_home.map(Into::into),
        log_dir: requirements.log_dir.map(Into::into),
        model_catalog_json: requirements.model_catalog_json.map(Into::into),
        check_for_update_on_startup: requirements.check_for_update_on_startup,
        allow_login_shell: requirements.allow_login_shell,
    }
}

fn map_computer_use_requirements_to_api(
    computer_use: codex_config::ComputerUseRequirementsToml,
) -> ComputerUseRequirements {
    ComputerUseRequirements {
        allow_locked_computer_use: computer_use.allow_locked_computer_use,
    }
}

fn map_browser_use_requirements_to_api(
    browser_use: codex_config::BrowserUseRequirementsToml,
) -> BrowserUseRequirements {
    BrowserUseRequirements {
        disable_auto_review: browser_use.disable_auto_review,
    }
}

fn map_sandbox_mode_requirement_to_api(mode: CoreSandboxModeRequirement) -> Option<SandboxMode> {
    match mode {
        CoreSandboxModeRequirement::ReadOnly => Some(SandboxMode::ReadOnly),
        CoreSandboxModeRequirement::WorkspaceWrite => Some(SandboxMode::WorkspaceWrite),
        CoreSandboxModeRequirement::DangerFullAccess => Some(SandboxMode::DangerFullAccess),
        CoreSandboxModeRequirement::ExternalSandbox => None,
    }
}

fn map_residency_requirement_to_api(
    residency: CoreResidencyRequirement,
) -> codex_cli_protocol::ResidencyRequirement {
    match residency {
        CoreResidencyRequirement::Us => codex_cli_protocol::ResidencyRequirement::Us,
    }
}

fn map_network_requirements_to_api(
    network: codex_config::NetworkRequirementsToml,
) -> NetworkRequirements {
    let allowed_domains = network
        .domains
        .as_ref()
        .and_then(codex_config::NetworkDomainPermissionsToml::allowed_domains);
    let denied_domains = network
        .domains
        .as_ref()
        .and_then(codex_config::NetworkDomainPermissionsToml::denied_domains);
    let allow_unix_sockets = network
        .unix_sockets
        .as_ref()
        .map(codex_config::NetworkUnixSocketPermissionsToml::allow_unix_sockets)
        .filter(|entries| !entries.is_empty());

    NetworkRequirements {
        enabled: network.enabled,
        http_port: network.http_port,
        socks_port: network.socks_port,
        allow_upstream_proxy: network.allow_upstream_proxy,
        dangerously_allow_non_loopback_proxy: network.dangerously_allow_non_loopback_proxy,
        dangerously_allow_all_unix_sockets: network.dangerously_allow_all_unix_sockets,
        domains: network.domains.map(|domains| {
            domains
                .entries
                .into_iter()
                .map(|(pattern, permission)| {
                    (pattern, map_network_domain_permission_to_api(permission))
                })
                .collect()
        }),
        managed_allowed_domains_only: network.managed_allowed_domains_only,
        allowed_domains,
        denied_domains,
        unix_sockets: network.unix_sockets.map(|unix_sockets| {
            unix_sockets
                .entries
                .into_iter()
                .map(|(path, permission)| {
                    (path, map_network_unix_socket_permission_to_api(permission))
                })
                .collect()
        }),
        allow_unix_sockets,
        allow_local_binding: network.allow_local_binding,
    }
}

fn map_network_domain_permission_to_api(
    permission: codex_config::NetworkDomainPermissionToml,
) -> NetworkDomainPermission {
    match permission {
        codex_config::NetworkDomainPermissionToml::Allow => NetworkDomainPermission::Allow,
        codex_config::NetworkDomainPermissionToml::Deny => NetworkDomainPermission::Deny,
    }
}

fn map_network_unix_socket_permission_to_api(
    permission: codex_config::NetworkUnixSocketPermissionToml,
) -> NetworkUnixSocketPermission {
    match permission {
        codex_config::NetworkUnixSocketPermissionToml::Allow => NetworkUnixSocketPermission::Allow,
        codex_config::NetworkUnixSocketPermissionToml::Deny => NetworkUnixSocketPermission::Deny,
    }
}

pub(super) fn map_error(err: ConfigManagerError) -> JSONRPCErrorError {
    if let Some(code) = err.write_error_code() {
        return config_write_error(code, err.to_string());
    }

    internal_error(err.to_string())
}

fn config_write_error(code: ConfigWriteErrorCode, message: impl Into<String>) -> JSONRPCErrorError {
    let mut error = invalid_request(message);
    error.data = Some(json!({
        "config_write_error_code": code,
    }));
    error
}

#[cfg(test)]
mod tests {
    use super::map_requirements_toml_to_api;
    use codex_config::ComputerUseRequirementsToml;
    use codex_config::ConfigRequirementsToml;
    use codex_config::ModelsRequirementsToml;
    use codex_config::NewThreadModelDefaultsToml;
    use codex_protocol::openai_models::ReasoningEffort;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use codex_utils_path_uri::PathUri;
    use pretty_assertions::assert_eq;

    #[test]
    fn requirements_api_includes_new_thread_model_defaults() {
        let mapped = map_requirements_toml_to_api(ConfigRequirementsToml {
            models: Some(ModelsRequirementsToml {
                new_thread: Some(NewThreadModelDefaultsToml {
                    model: Some("gpt-managed".to_string()),
                    model_reasoning_effort: Some(ReasoningEffort::Medium),
                    service_tier: Some("fast".to_string()),
                }),
            }),
            ..ConfigRequirementsToml::default()
        });

        let defaults = mapped
            .models
            .and_then(|models| models.new_thread)
            .expect("new-thread defaults");
        assert_eq!(defaults.model.as_deref(), Some("gpt-managed"));
        assert_eq!(
            defaults.model_reasoning_effort,
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(defaults.service_tier.as_deref(), Some("fast"));
    }

    #[test]
    fn requirements_api_includes_computer_use_requirements() {
        let mapped = map_requirements_toml_to_api(ConfigRequirementsToml {
            computer_use: Some(ComputerUseRequirementsToml {
                allow_locked_computer_use: Some(false),
            }),
            ..ConfigRequirementsToml::default()
        });

        assert_eq!(
            mapped
                .computer_use
                .and_then(|requirements| requirements.allow_locked_computer_use),
            Some(false)
        );
    }

    #[test]
    fn requirements_api_includes_exact_managed_values() {
        let sqlite_home = AbsolutePathBuf::try_from(std::env::temp_dir().join("managed-state"))
            .expect("managed sqlite home should be absolute");
        let log_dir = AbsolutePathBuf::try_from(std::env::temp_dir().join("managed-logs"))
            .expect("managed log dir should be absolute");
        let model_catalog_json =
            AbsolutePathBuf::try_from(std::env::temp_dir().join("managed-models.json"))
                .expect("managed model catalog path should be absolute");
        let mapped = map_requirements_toml_to_api(ConfigRequirementsToml {
            sqlite_home: Some(sqlite_home.clone()),
            log_dir: Some(log_dir.clone()),
            model_catalog_json: Some(model_catalog_json.clone()),
            check_for_update_on_startup: Some(false),
            allow_login_shell: Some(false),
            ..ConfigRequirementsToml::default()
        });

        assert_eq!(mapped.sqlite_home, Some(PathUri::from(sqlite_home)));
        assert_eq!(mapped.log_dir, Some(PathUri::from(log_dir)));
        assert_eq!(
            mapped.model_catalog_json,
            Some(PathUri::from(model_catalog_json))
        );
        assert_eq!(mapped.check_for_update_on_startup, Some(false));
        assert_eq!(mapped.allow_login_shell, Some(false));
    }
}
