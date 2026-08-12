use super::Config;
use super::ConfigTomlLoadResult;
use super::resolve_bootstrap_auth_route_config;
use codex_login::AuthConfig;
use std::path::Path;

impl Config {
    pub fn auth_config(&self) -> AuthConfig {
        AuthConfig {
            codex_home: self.codex_home.to_path_buf(),
            forced_login_method: self.forced_login_method,
            chatgpt_base_url: Some(self.chatgpt_base_url.clone()),
            forced_chatgpt_workspace_id: self.forced_chatgpt_workspace_id.clone(),
            managed_auth_policy: self.config_layer_stack.requirements().managed_auth_policy(),
            auth_route_config: self.auth_route_config(),
        }
    }
}

/// Builds authentication settings from the locally resolved bootstrap config.
///
/// Use this before fetching cloud requirements, when a full [`Config`] is not
/// yet available. Preserves the official ChatGPT endpoint, auth routing, and
/// managed login/workspace restrictions.
pub fn bootstrap_auth_config(
    codex_home: &Path,
    bootstrap_config: &ConfigTomlLoadResult,
) -> std::io::Result<AuthConfig> {
    let config = &bootstrap_config.config_toml;
    // Empty legacy workspace settings mean unrestricted, not an empty allowlist.
    let forced_chatgpt_workspace_id = config
        .forced_chatgpt_workspace_id
        .clone()
        .map(|workspaces| {
            workspaces
                .into_vec()
                .into_iter()
                .map(|workspace| workspace.trim().to_string())
                .filter(|workspace| !workspace.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|workspaces| !workspaces.is_empty());
    let auth_config = AuthConfig {
        codex_home: codex_home.to_path_buf(),
        forced_login_method: config.forced_login_method,
        chatgpt_base_url: Some("https://chatgpt.com/backend-api/".to_string()),
        forced_chatgpt_workspace_id,
        managed_auth_policy: bootstrap_config
            .config_layer_stack
            .requirements()
            .managed_auth_policy(),
        auth_route_config: resolve_bootstrap_auth_route_config(
            config,
            bootstrap_config
                .config_layer_stack
                .requirements()
                .feature_requirements
                .as_ref(),
        )?,
    };
    auth_config.validate()?;
    Ok(auth_config)
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
