use super::*;
use pretty_assertions::assert_eq;

fn parse_toml(value: &str) -> TomlValue {
    toml::from_str(value).expect("TOML should parse")
}

/// Feature tables added above boolean toggles retain the lower layer's enabled state.
#[test]
fn merge_multi_agent_v2_table_preserves_boolean_toggle() {
    for feature_path in ["features"] {
        let mut base = parse_toml(&format!("[{feature_path}]\nmulti_agent_v2 = true\n"));
        let overlay = parse_toml(&format!(
            "[{feature_path}.multi_agent_v2]\nsubagent_usage_hint_text = \"Delegate carefully.\"\n",
        ));

        merge_toml_values(&mut base, &overlay);

        assert_eq!(
            base,
            parse_toml(&format!(
                "[{feature_path}.multi_agent_v2]\nenabled = true\nsubagent_usage_hint_text = \"Delegate carefully.\"\n",
            ))
        );
    }
}

/// Boolean feature toggles update enabled state without discarding nested configuration.
#[test]
fn merge_multi_agent_v2_boolean_preserves_existing_feature_table() {
    for feature_path in ["features"] {
        let mut base = parse_toml(&format!(
            "[{feature_path}.multi_agent_v2]\nenabled = true\nsubagent_usage_hint_text = \"Delegate carefully.\"\n",
        ));
        let overlay = parse_toml(&format!("[{feature_path}]\nmulti_agent_v2 = false\n"));

        merge_toml_values(&mut base, &overlay);

        assert_eq!(
            base,
            parse_toml(&format!(
                "[{feature_path}.multi_agent_v2]\nenabled = false\nsubagent_usage_hint_text = \"Delegate carefully.\"\n",
            ))
        );
    }
}

/// Opaque desktop settings retain ordinary scalar/table replacement semantics.
#[test]
fn merge_multi_agent_v2_compatibility_excludes_opaque_desktop_paths() {
    let cases = [
        (
            "[desktop.features.multi_agent_v2]\nenabled = true\n",
            "[desktop.features]\nmulti_agent_v2 = false\n",
            "[desktop.features]\nmulti_agent_v2 = false\n",
        ),
        (
            "[desktop.features]\nmulti_agent_v2 = true\n",
            "[desktop.features.multi_agent_v2]\ncustom = true\n",
            "[desktop.features.multi_agent_v2]\ncustom = true\n",
        ),
    ];

    for (base, overlay, expected) in cases {
        let mut base = parse_toml(base);
        merge_toml_values(&mut base, &parse_toml(overlay));
        assert_eq!(base, parse_toml(expected));
    }
}

/// CLI overrides preserve the multi-agent toggle and nested options in either ordering.
#[test]
fn multi_agent_v2_cli_overrides_preserve_boolean_and_nested_configuration() {
    for feature_path in ["features"] {
        let instructions = (
            format!("{feature_path}.multi_agent_v2.subagent_usage_hint_text"),
            TomlValue::String("Delegate carefully.".to_string()),
        );
        let enabled = (
            format!("{feature_path}.multi_agent_v2"),
            TomlValue::Boolean(true),
        );
        let feature_table = (
            format!("{feature_path}.multi_agent_v2"),
            parse_toml("subagent_usage_hint_text = \"Delegate carefully.\"\n"),
        );
        let expected = parse_toml(&format!(
            "[{feature_path}.multi_agent_v2]\nenabled = true\nsubagent_usage_hint_text = \"Delegate carefully.\"\n",
        ));

        for overrides in [
            vec![enabled.clone(), instructions.clone()],
            vec![instructions, enabled.clone()],
            vec![enabled.clone(), feature_table.clone()],
            vec![feature_table, enabled],
        ] {
            assert_eq!(crate::build_cli_overrides_layer(&overrides), expected);
        }
    }
}

/// Repeated opaque desktop overrides continue to replace their previous value.
#[test]
fn multi_agent_v2_cli_compatibility_excludes_opaque_desktop_paths() {
    let path = "desktop.features.multi_agent_v2".to_string();
    let enabled = (path.clone(), TomlValue::Boolean(true));
    let feature_table = (path, parse_toml("custom = true\n"));

    assert_eq!(
        crate::build_cli_overrides_layer(&[enabled.clone(), feature_table.clone()]),
        parse_toml("[desktop.features.multi_agent_v2]\ncustom = true\n")
    );
    assert_eq!(
        crate::build_cli_overrides_layer(&[feature_table, enabled]),
        parse_toml("[desktop.features]\nmulti_agent_v2 = true\n")
    );
}

#[test]
fn merge_toml_values_normalizes_permission_network_domains_before_overlaying() {
    let mut base = parse_toml(
        r#"
[permissions.dev.network.domains]
"example.com" = "deny"
"#,
    );
    let overlay = parse_toml(
        r#"
[permissions.dev.network.domains]
"EXAMPLE.COM" = "allow"
"#,
    );

    merge_toml_values(&mut base, &overlay);

    let expected = parse_toml(
        r#"
[permissions.dev.network.domains]
"example.com" = "allow"
"#,
    );
    assert_eq!(base, expected);
}

#[test]
fn shell_environment_policy_filters_overlay_merges_by_key_case_insensitively() {
    let mut base = parse_toml(
        r#"
[shell_environment_policy.filters]
"FLIP_*" = "exclude"
"KEEP_*" = "include"
"#,
    );
    let overlay = parse_toml(
        r#"
[shell_environment_policy.filters]
"ADD_*" = "exclude"
"flip_*" = "include"
"#,
    );

    merge_toml_values(&mut base, &overlay);

    assert_eq!(
        base,
        parse_toml(
            r#"
[shell_environment_policy.filters]
"add_*" = "exclude"
"flip_*" = "include"
"keep_*" = "include"
"#,
        )
    );
}

#[test]
fn shell_environment_policy_filters_overlay_merges_unicode_keys_case_insensitively() {
    let mut base = parse_toml(
        r#"
[shell_environment_policy.filters]
"СЕКРЕТ_*" = "exclude"
"#,
    );
    let overlay = parse_toml(
        r#"
[shell_environment_policy.filters]
"секрет_*" = "include"
"#,
    );

    merge_toml_values(&mut base, &overlay);

    assert_eq!(base, overlay);
}
