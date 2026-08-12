use super::*;
use codex_config::ConfigLayerStack;
use codex_config::ConfigRequirements;
use codex_config::ConfigRequirementsToml;
use codex_config::RequirementSource;
use codex_config::Sourced;
use codex_config::config_toml::ConfigToml;
use codex_config::config_toml::ForcedChatgptWorkspaceIds;
use codex_protocol::config_types::ForcedLoginMethod;
use pretty_assertions::assert_eq;

#[test]
fn managed_auth_restrictions_intersect_workspaces_and_fail_closed() {
    let config = ConfigToml {
        forced_login_method: None,
        forced_chatgpt_workspace_id: Some(ForcedChatgptWorkspaceIds::Multiple(vec![
            " denied ".to_string(),
            " allowed ".to_string(),
        ])),
        ..Default::default()
    };
    let mut requirements = ConfigRequirements {
        allowed_login_methods: Some(Sourced::new(
            vec![ForcedLoginMethod::Chatgpt],
            RequirementSource::Unknown,
        )),
        allowed_chatgpt_workspaces: Some(Sourced::new(
            vec!["allowed".to_string()],
            RequirementSource::Unknown,
        )),
        ..Default::default()
    };

    let bootstrap_config = ConfigTomlLoadResult {
        config_toml: config.clone(),
        config_layer_stack: ConfigLayerStack::new(
            Vec::new(),
            requirements.clone(),
            ConfigRequirementsToml::default(),
        )
        .expect("requirements should stack"),
    };
    let auth_config = bootstrap_auth_config(Path::new("codex-home"), &bootstrap_config)
        .expect("policy should resolve");
    assert_eq!(auth_config.forced_login_method, None);
    assert!(auth_config.is_login_method_allowed(ForcedLoginMethod::Chatgpt));
    assert!(!auth_config.is_login_method_allowed(ForcedLoginMethod::Api));
    assert_eq!(
        auth_config.forced_chatgpt_workspace_id,
        Some(vec!["denied".to_string(), "allowed".to_string()])
    );
    assert_eq!(
        auth_config.effective_chatgpt_workspaces(),
        Some(vec!["allowed".to_string()])
    );

    requirements.allowed_chatgpt_workspaces =
        Some(Sourced::new(Vec::new(), RequirementSource::Unknown));
    let bootstrap_config = ConfigTomlLoadResult {
        config_toml: config,
        config_layer_stack: ConfigLayerStack::new(
            Vec::new(),
            requirements,
            ConfigRequirementsToml::default(),
        )
        .expect("requirements should stack"),
    };
    assert_eq!(
        bootstrap_auth_config(Path::new("codex-home"), &bootstrap_config)
            .expect_err("ChatGPT-only policy without an allowed workspace must fail")
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
}
