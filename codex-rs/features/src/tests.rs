use crate::Feature;
use crate::FeatureConfigSource;
use crate::FeatureToml;
use crate::Features;
use crate::FeaturesToml;
use crate::Stage;
use crate::feature_for_key;
use crate::unstable_features_warning_event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use toml::Table;
use toml::Value as TomlValue;

#[test]
fn under_development_features_are_disabled_by_default() {
    for spec in crate::FEATURES {
        if matches!(spec.stage, Stage::UnderDevelopment) {
            assert_eq!(
                spec.default_enabled, false,
                "feature `{}` is under development and must be disabled by default",
                spec.key
            );
        }
    }
}

#[test]
fn tool_registry_config_is_not_a_feature_toggle() {
    let features: FeaturesToml =
        toml::from_str("[tool_registry]\nerror_on_tool_collisions = true\n")
            .expect("tool registry settings should deserialize");

    assert_eq!(
        features.tool_registry,
        Some(crate::ToolRegistryConfigToml {
            error_on_tool_collisions: Some(true),
        })
    );
    assert!(features.entries().is_empty());
    assert!(crate::is_known_feature_key("tool_registry"));
    assert_eq!(feature_for_key("tool_registry"), None);
}

#[test]
fn executor_capability_discovery_is_an_opt_in_map_feature() {
    let mut features = Features::with_defaults();
    assert!(!features.enabled(Feature::ExecutorCapabilityDiscovery));

    features.apply_map(&BTreeMap::from([(
        "executor_capability_discovery".to_string(),
        true,
    )]));

    assert!(features.enabled(Feature::ExecutorCapabilityDiscovery));
}

#[test]
fn default_enabled_features_are_stable() {
    for spec in crate::FEATURES {
        if spec.default_enabled {
            assert!(
                matches!(spec.stage, Stage::Stable),
                "feature `{}` is enabled by default but is not stable/removed ({:?})",
                spec.key,
                spec.stage
            );
        }
    }
}

#[test]
fn code_mode_only_requires_code_mode() {
    let mut features = Features::with_defaults();
    features.enable(Feature::CodeModeOnly);
    features.normalize_dependencies();

    assert_eq!(features.enabled(Feature::CodeModeOnly), true);
    assert_eq!(features.enabled(Feature::CodeMode), true);
}

#[test]
fn code_mode_host_feature_config_preserves_boolean_toggle() {
    let features: FeaturesToml =
        toml::from_str("code_mode_host = false").expect("features table should deserialize");

    assert_eq!(features.code_mode_host, Some(FeatureToml::Enabled(false)));
    assert_eq!(
        features.entries(),
        BTreeMap::from([("code_mode_host".to_string(), false)])
    );
}

#[test]
fn code_mode_host_feature_config_deserializes_fallback_setting() {
    let features: FeaturesToml = toml::from_str(
        r#"
[code_mode_host]
enabled = true
disable_in_process_fallback = true
"#,
    )
    .expect("features table should deserialize");

    assert_eq!(
        features.code_mode_host,
        Some(FeatureToml::Config(crate::CodeModeHostConfigToml {
            enabled: Some(true),
            disable_in_process_fallback: Some(true),
        }))
    );
    assert_eq!(
        features.entries(),
        BTreeMap::from([("code_mode_host".to_string(), true)])
    );
}

#[test]
fn image_generation_toggle_controls_extension_backed_generation() {
    let mut entries = BTreeMap::new();
    entries.insert("image_generation".to_string(), false);
    let mut features = Features::with_defaults();
    features.apply_map(&entries);
    assert!(!features.enabled(Feature::ImageGeneration));

    entries.insert("image_generation".to_string(), true);
    features.disable(Feature::ImageGeneration);
    features.apply_map(&entries);
    assert!(features.enabled(Feature::ImageGeneration));
}

#[test]
fn from_sources_applies_base_profile_and_overrides() {
    let mut profile_entries = BTreeMap::new();
    profile_entries.insert("code_mode_only".to_string(), true);
    let profile_features = FeaturesToml {
        entries: profile_entries,
        ..Default::default()
    };

    let features = Features::from_sources(
        FeatureConfigSource {
            ..Default::default()
        },
        FeatureConfigSource {
            features: Some(&profile_features),
            ..Default::default()
        },
    );

    assert_eq!(features.enabled(Feature::CodeModeOnly), true);
    assert_eq!(features.enabled(Feature::CodeMode), true);
}

#[test]
fn multi_agent_v2_feature_config_deserializes_boolean_toggle() {
    let features: FeaturesToml = toml::from_str(
        r#"
multi_agent_v2 = true
"#,
    )
    .expect("features table should deserialize");

    assert_eq!(
        features.entries(),
        BTreeMap::from([("multi_agent_v2".to_string(), true)])
    );
    assert_eq!(features.multi_agent_v2, Some(FeatureToml::Enabled(true)));
}

#[test]
fn multi_agent_v2_feature_config_deserializes_table() {
    let features: FeaturesToml = toml::from_str(
        r#"
[multi_agent_v2]
enabled = true
max_concurrent_threads_per_session = 4
min_wait_timeout_ms = 2500
max_wait_timeout_ms = 120000
default_wait_timeout_ms = 30000
usage_hint_text = "Custom delegation guidance."
root_agent_usage_hint_text = "Root guidance."
subagent_usage_hint_text = "Subagent guidance."
subagent_developer_instructions = "Delegate carefully."
multi_agent_mode_hint_text = "Custom mode guidance."
tool_namespace = "agents"
hide_spawn_agent_metadata = true
expose_spawn_agent_model_overrides = true
wait_agent_enabled = false
non_code_mode_only = true
"#,
    )
    .expect("features table should deserialize");

    assert_eq!(
        features.entries(),
        BTreeMap::from([("multi_agent_v2".to_string(), true)])
    );
    assert_eq!(
        features.multi_agent_v2,
        Some(crate::FeatureToml::Config(crate::MultiAgentV2ConfigToml {
            enabled: Some(true),
            max_concurrent_threads_per_session: Some(4),
            min_wait_timeout_ms: Some(2500),
            max_wait_timeout_ms: Some(120000),
            default_wait_timeout_ms: Some(30000),
            usage_hint_text: Some("Custom delegation guidance.".to_string()),
            root_agent_usage_hint_text: Some("Root guidance.".to_string()),
            subagent_usage_hint_text: Some("Subagent guidance.".to_string()),
            subagent_developer_instructions: Some("Delegate carefully.".to_string()),
            multi_agent_mode_hint_text: Some("Custom mode guidance.".to_string()),
            tool_namespace: Some("agents".to_string()),
            hide_spawn_agent_metadata: Some(true),
            expose_spawn_agent_model_overrides: Some(true),
            wait_agent_enabled: Some(false),
            non_code_mode_only: Some(true),
        }))
    );
}

#[test]
fn materialize_resolved_enabled_writes_all_features_and_preserves_custom_config() {
    let mut features = Features::with_defaults();
    features.enable(Feature::CodeMode);
    features.enable(Feature::MultiAgentV2);
    features.enable(Feature::NetworkProxy);
    features.enable(Feature::RespectSystemProxy);

    let mut features_toml = FeaturesToml {
        tool_registry: Some(crate::ToolRegistryConfigToml {
            error_on_tool_collisions: Some(true),
        }),
        code_mode_host: Some(FeatureToml::Config(crate::CodeModeHostConfigToml {
            enabled: Some(false),
            disable_in_process_fallback: Some(true),
        })),
        multi_agent_v2: Some(FeatureToml::Config(crate::MultiAgentV2ConfigToml {
            enabled: Some(false),
            min_wait_timeout_ms: Some(2500),
            subagent_developer_instructions: Some("Delegate carefully.".to_string()),
            ..Default::default()
        })),
        network_proxy: Some(FeatureToml::Config(crate::NetworkProxyConfigToml {
            enabled: Some(false),
            proxy_url: Some("http://127.0.0.1:43128".to_string()),
            ..Default::default()
        })),
        entries: BTreeMap::new(),
        ..Default::default()
    };

    features_toml.materialize_resolved_enabled(&features);

    assert_eq!(
        features_toml.tool_registry,
        Some(crate::ToolRegistryConfigToml {
            error_on_tool_collisions: Some(true),
        })
    );
    let entries = features_toml.entries();
    assert!(!entries.contains_key("tool_registry"));
    for spec in crate::FEATURES {
        assert_eq!(
            entries.get(spec.key),
            Some(&features.enabled(spec.id)),
            "{}",
            spec.key
        );
    }
    assert_eq!(
        features_toml.code_mode_host,
        Some(FeatureToml::Config(crate::CodeModeHostConfigToml {
            enabled: Some(true),
            disable_in_process_fallback: Some(true),
        }))
    );
    assert_eq!(
        features_toml.multi_agent_v2,
        Some(FeatureToml::Config(crate::MultiAgentV2ConfigToml {
            enabled: Some(true),
            min_wait_timeout_ms: Some(2500),
            subagent_developer_instructions: Some("Delegate carefully.".to_string()),
            ..Default::default()
        }))
    );
    assert_eq!(
        features_toml.network_proxy,
        Some(FeatureToml::Config(crate::NetworkProxyConfigToml {
            enabled: Some(true),
            proxy_url: Some("http://127.0.0.1:43128".to_string()),
            ..Default::default()
        }))
    );
    let replayed = Features::from_sources(
        FeatureConfigSource {
            features: Some(&features_toml),
            ..Default::default()
        },
        FeatureConfigSource::default(),
    );
}

#[test]
fn unstable_warning_event_only_mentions_enabled_under_development_features() {
    let mut configured_features = Table::new();
    configured_features.insert(
        "apply_patch_streaming_events".to_string(),
        TomlValue::Boolean(true),
    );
    configured_features.insert("personality".to_string(), TomlValue::Boolean(true));
    configured_features.insert("unknown".to_string(), TomlValue::Boolean(true));

    let mut features = Features::with_defaults();
    features.enable(Feature::ApplyPatchStreamingEvents);

    let warning = unstable_features_warning_event(
        Some(&configured_features),
        /*suppress_unstable_features_warning*/ false,
        &features,
        "/tmp/config.toml",
    )
    .expect("warning event");

    let EventMsg::Warning(WarningEvent { message }) = warning.msg else {
        panic!("expected warning event");
    };
    assert!(message.contains("apply_patch_streaming_events"));
    assert!(!message.contains("personality"));
    assert!(message.contains("/tmp/config.toml"));
}

#[test]
fn unstable_warning_event_ignores_enabled_structured_stable_feature() {
    let configured_features: Table = toml::from_str(
        r#"
multi_agent_v2 = { enabled = true, tool_namespace = "agents" }
code_mode = true
"#,
    )
    .expect("features table should deserialize");

    let mut features = Features::with_defaults();
    features.enable(Feature::MultiAgentV2);
    features.enable(Feature::CodeMode);

    let warning = unstable_features_warning_event(
        Some(&configured_features),
        /*suppress_unstable_features_warning*/ false,
        &features,
        "/tmp/config.toml",
    )
    .expect("warning event");

    let EventMsg::Warning(WarningEvent { message }) = warning.msg else {
        panic!("expected warning event");
    };
    assert_eq!(
        "Under-development features enabled: code_mode. Under-development features are incomplete and may behave unpredictably. To suppress this warning, set `suppress_unstable_features_warning = true` in /tmp/config.toml.".to_string(),
        message
    );
}
