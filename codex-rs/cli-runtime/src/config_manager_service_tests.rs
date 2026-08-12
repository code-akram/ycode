use super::*;
use anyhow::Result;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_config::test_support::CloudConfigBundleFixture;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[test]
fn toml_value_to_item_handles_nested_config_tables() {
    let config = r#"
[services.docs]
command = "docs-service"

[services.docs.http_headers]
X-Doc = "42"
"#;

    let value: TomlValue = toml::from_str(config).expect("parse config example");
    let item = toml_value_to_item(&value).expect("convert to toml_edit item");

    let root = item.as_table().expect("root table");
    assert!(!root.is_implicit(), "root table should be explicit");

    let services = root
        .get("services")
        .and_then(TomlItem::as_table)
        .expect("services table");
    assert!(!services.is_implicit(), "services table should be explicit");

    let docs = services
        .get("docs")
        .and_then(TomlItem::as_table)
        .expect("docs table");
    assert_eq!(
        docs.get("command")
            .and_then(TomlItem::as_value)
            .and_then(toml_edit::Value::as_str),
        Some("docs-service")
    );

    let http_headers = docs
        .get("http_headers")
        .and_then(TomlItem::as_table)
        .expect("http_headers table");
    assert_eq!(
        http_headers
            .get("X-Doc")
            .and_then(TomlItem::as_value)
            .and_then(toml_edit::Value::as_str),
        Some("42")
    );
}

#[tokio::test]
async fn write_value_preserves_comments_and_order() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    let original = r#"# Codex user configuration
model = "gpt-5.2"
service_tier = "priority"

[notice]
# Preserve this comment
hide_full_access_warning = true

[features]
unified_exec = true
"#;
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), original)?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    service
        .write_value(ConfigValueWriteParams {
            file_path: Some(tmp.path().join(CONFIG_TOML_FILE).display().to_string()),
            key_path: "features.personality".to_string(),
            value: serde_json::json!(true),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("write succeeds");

    let updated = std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"# Codex user configuration
model = "gpt-5.2"
service_tier = "priority"

[notice]
# Preserve this comment
hide_full_access_warning = true

[features]
unified_exec = true
personality = true
"#;
    assert_eq!(updated, expected);
    Ok(())
}

#[tokio::test]
async fn process_routing_does_not_enter_config_layers() -> Result<()> {
    let tmp = tempdir()?;
    let mut service = ConfigManager::new_for_tests(
        tmp.path().to_path_buf(),
        Vec::new(),
        LoaderOverrides::without_managed_config_for_tests(),
        CloudConfigBundleLoader::default(),
    );
    service.psp = true;

    let config = service
        .load_with_overrides(
            Some(
                [(
                    "features".to_string(),
                    serde_json::json!({ "multi_agent_v2": true }),
                )]
                .into_iter()
                .collect(),
            ),
            Default::default(),
        )
        .await?;

    assert!(config.psp);
    assert!(config.http_client_factory().has_chatgpt_cookies());
    assert!(
        config
            .config_layer_stack
            .effective_config()
            .get("features")
            .and_then(|features| features.get("psp"))
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn clear_missing_nested_config_is_noop() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&path, "")?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    let response = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(path.display().to_string()),
            key_path: "features.personality".to_string(),
            value: serde_json::Value::Null,
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("clear missing config succeeds");

    assert_eq!(response.status, WriteStatus::Ok);
    assert_eq!(response.overridden_metadata, None);
    assert_eq!(std::fs::read_to_string(&path)?, "");
    Ok(())
}

#[tokio::test]
async fn clear_user_value_if_matches_clears_matching_value() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&path, "model = \"gpt-5.2\"\nservice_tier = \"priority\"\n")?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    service
        .clear_user_value_if_matches("model", serde_json::json!("gpt-5.2"))
        .await?;

    assert_eq!(
        std::fs::read_to_string(&path)?,
        "service_tier = \"priority\"\n"
    );
    Ok(())
}

#[tokio::test]
async fn clear_user_value_if_matches_preserves_non_matching_value() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join(CONFIG_TOML_FILE);
    let original = "model = \"gpt-5.2\"\nservice_tier = \"priority\"\n";
    std::fs::write(&path, original)?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    service
        .clear_user_value_if_matches("model", serde_json::json!("gpt-5.3"))
        .await?;

    assert_eq!(std::fs::read_to_string(&path)?, original);
    Ok(())
}

#[tokio::test]
async fn version_conflict_rejected() {
    let tmp = tempdir().expect("tempdir");
    let user_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&user_path, "model = \"user\"").unwrap();

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    let error = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(tmp.path().join(CONFIG_TOML_FILE).display().to_string()),
            key_path: "model".to_string(),
            value: serde_json::json!("gpt-5.2"),
            merge_strategy: MergeStrategy::Replace,
            expected_version: Some("sha256:bogus".to_string()),
        })
        .await
        .expect_err("should fail");

    assert_eq!(
        error.write_error_code(),
        Some(ConfigWriteErrorCode::ConfigVersionConflict)
    );
}

#[tokio::test]
async fn write_value_defaults_to_user_config_path() {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), "").unwrap();

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    service
        .write_value(ConfigValueWriteParams {
            file_path: None,
            key_path: "model".to_string(),
            value: serde_json::json!("gpt-new"),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("write succeeds");

    let contents = std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).expect("read config");
    assert!(
        contents.contains("model = \"gpt-new\""),
        "config.toml should be updated even when file_path is omitted"
    );
}

#[tokio::test]
async fn write_value_defaults_to_selected_user_config_path() {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), "model = \"gpt-main\"").unwrap();
    let selected_path = tmp.path().join("work.config.toml");
    std::fs::write(&selected_path, "").unwrap();

    let mut loader_overrides = LoaderOverrides::without_managed_config_for_tests();
    loader_overrides.user_config_path =
        Some(AbsolutePathBuf::from_absolute_path(&selected_path).expect("selected config path"));
    loader_overrides.user_config_profile = Some("work".parse().expect("profile-v2 name"));
    let service = ConfigManager::new_for_tests(
        tmp.path().to_path_buf(),
        vec![],
        loader_overrides,
        CloudConfigBundleLoader::default(),
    );
    service
        .write_value(ConfigValueWriteParams {
            file_path: None,
            key_path: "model".to_string(),
            value: serde_json::json!("gpt-work"),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("write succeeds");

    assert_eq!(
        std::fs::read_to_string(&selected_path).expect("read selected config"),
        "model = \"gpt-work\"\n"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).expect("read main config"),
        "model = \"gpt-main\""
    );
}

#[tokio::test]
async fn load_default_config_preserves_requirements_and_selected_user_config_path() {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), "model = \"gpt-main\"").unwrap();
    std::fs::write(
        tmp.path().join("requirements.toml"),
        "allowed_login_methods = [\"api\"]\nallowed_chatgpt_workspaces = [\"managed-workspace\"]\n",
    )
    .unwrap();
    let selected_path = tmp.path().join("work.config.toml");
    std::fs::write(&selected_path, "not valid toml").unwrap();
    let selected_file =
        AbsolutePathBuf::from_absolute_path(&selected_path).expect("selected config path");

    let mut loader_overrides =
        LoaderOverrides::with_system_config_path_for_tests(tmp.path().join("system_config.toml"));
    loader_overrides.user_config_path = Some(selected_file.clone());
    loader_overrides.user_config_profile = Some("work".parse().expect("profile-v2 name"));
    let service = ConfigManager::new_for_tests(
        tmp.path().to_path_buf(),
        vec![],
        loader_overrides,
        CloudConfigBundleLoader::default(),
    );

    service
        .load_latest_config(/*fallback_cwd*/ None)
        .await
        .expect_err("selected config should fail to load");
    let config = service
        .load_default_config()
        .await
        .expect("default config loads after selected config error");

    assert_eq!(
        config.config_layer_stack.get_user_config_file(),
        Some(&selected_file)
    );
    assert_eq!(
        config
            .config_layer_stack
            .requirements()
            .managed_auth_policy(),
        codex_config::ManagedAuthPolicy {
            allowed_login_methods: Some(vec![codex_protocol::config_types::ForcedLoginMethod::Api]),
            allowed_chatgpt_workspaces: Some(vec!["managed-workspace".to_string()]),
        }
    );
}

#[tokio::test]
async fn auth_policy_survives_unusable_requirements_file_changes() -> Result<()> {
    let tmp = tempdir()?;
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), "")?;
    let requirements_path = tmp.path().join("requirements.toml");
    std::fs::write(
        &requirements_path,
        "allowed_login_methods = [\"api\"]\nallowed_chatgpt_workspaces = [\"startup\"]\n",
    )?;
    let service = ConfigManager::new_for_tests(
        tmp.path().to_path_buf(),
        Vec::new(),
        LoaderOverrides::with_system_config_path_for_tests(tmp.path().join("system_config.toml")),
        CloudConfigBundleLoader::default(),
    );
    let startup = service.load_latest_config(/*fallback_cwd*/ None).await?;
    let auth_manager = codex_login::AuthManager::shared_from_config(
        &startup, /*enable_codex_api_key_env*/ false,
    )
    .await;
    std::fs::write(
        &requirements_path,
        "allowed_login_methods = [\"chatgpt\"]\nallowed_chatgpt_workspaces = []\n",
    )?;
    for refreshed in [
        service.load_latest_config(/*fallback_cwd*/ None).await?,
        service.load_latest_config_for_thread(&startup).await?,
    ] {
        assert_eq!(refreshed.forced_login_method, None);
        assert_eq!(refreshed.forced_chatgpt_workspace_id, None);
    }
    assert!(
        auth_manager.is_login_method_allowed(codex_protocol::config_types::ForcedLoginMethod::Api)
    );
    assert!(
        !auth_manager
            .is_login_method_allowed(codex_protocol::config_types::ForcedLoginMethod::Chatgpt)
    );
    assert_eq!(
        auth_manager.effective_chatgpt_workspaces(),
        Some(vec!["startup".to_string()])
    );
    Ok(())
}

#[tokio::test]
async fn write_value_rejects_feature_requirement_conflict() {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), "").unwrap();

    let service = ConfigManager::new_for_tests(
        tmp.path().to_path_buf(),
        vec![],
        LoaderOverrides::without_managed_config_for_tests(),
        CloudConfigBundleFixture::loader_with_enterprise_requirement(
            r#"
[features]
personality = true
"#,
        ),
    );

    let error = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(tmp.path().join(CONFIG_TOML_FILE).display().to_string()),
            key_path: "features.personality".to_string(),
            value: serde_json::json!(false),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect_err("conflicting feature write should fail");

    assert_eq!(
        error.write_error_code(),
        Some(ConfigWriteErrorCode::ConfigValidationError)
    );
    assert!(
        error
            .to_string()
            .contains("invalid value for `features`: `features.personality=false`"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).unwrap(),
        ""
    );
}

#[tokio::test]
async fn write_value_rejects_exact_managed_requirement() {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&path, "allow_login_shell = true\n").unwrap();

    let service = ConfigManager::new_for_tests(
        tmp.path().to_path_buf(),
        vec![],
        LoaderOverrides::without_managed_config_for_tests(),
        CloudConfigBundleFixture::loader_with_enterprise_requirement("allow_login_shell = false"),
    );

    let error = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(path.display().to_string()),
            key_path: "allow_login_shell".to_string(),
            value: serde_json::json!(true),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect_err("managed exact field should be read-only");

    assert_eq!(
        error.write_error_code(),
        Some(ConfigWriteErrorCode::ConfigRequirementReadonly)
    );
    assert!(error.to_string().contains("`allow_login_shell`"));
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "allow_login_shell = true\n"
    );
}

fn toml_path(tmp: &Path, name: &str) -> String {
    tmp.join(name).to_string_lossy().replace('\\', "\\\\")
}

#[tokio::test]
async fn read_omits_origins_for_exact_managed_values() {
    for has_user_values in [true, false] {
        let tmp = tempdir().expect("tempdir");
        let user_config = if has_user_values {
            format!(
                r#"model = "user-model"
sqlite_home = "{}"
allow_login_shell = true
"#,
                toml_path(tmp.path(), "user-sqlite"),
            )
        } else {
            "model = \"user-model\"\n".to_string()
        };
        std::fs::write(tmp.path().join(CONFIG_TOML_FILE), user_config).unwrap();

        let requirements = format!(
            r#"sqlite_home = "{}"
allow_login_shell = false
"#,
            toml_path(tmp.path(), "managed-sqlite"),
        );
        let service = ConfigManager::new_for_tests(
            tmp.path().to_path_buf(),
            vec![],
            LoaderOverrides::without_managed_config_for_tests(),
            CloudConfigBundleFixture::loader_with_enterprise_requirement(requirements),
        );

        let response = service
            .read(ConfigReadParams {
                include_layers: false,
                cwd: None,
            })
            .await
            .expect("config read should succeed");

        assert_eq!(
            response.config.additional.get("sqlite_home"),
            Some(&serde_json::json!(tmp.path().join("managed-sqlite")))
        );
        assert_eq!(
            response.config.additional.get("allow_login_shell"),
            Some(&serde_json::json!(false))
        );
        for path in ["sqlite_home", "allow_login_shell"] {
            assert!(!response.origins.contains_key(path), "origin for {path}");
        }
        assert!(response.origins.contains_key("model"));
    }
}

#[tokio::test]
async fn read_materializes_default_allow_login_shell() {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), "").unwrap();

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    let response = service
        .read(ConfigReadParams {
            include_layers: false,
            cwd: None,
        })
        .await
        .expect("config read should succeed");

    assert_eq!(
        response.config.additional.get("allow_login_shell"),
        Some(&serde_json::json!(true))
    );
}

#[tokio::test]
async fn upsert_merges_tables_replace_overwrites() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join(CONFIG_TOML_FILE);
    let base = r#"[services.linear]
bearer_token_env_var = "TOKEN"
name = "linear"
url = "https://linear.example"

[services.linear.env_http_headers]
existing = "keep"

[services.linear.http_headers]
alpha = "a"
"#;

    let overlay = serde_json::json!({
        "bearer_token_env_var": "NEW_TOKEN",
        "http_headers": {
            "alpha": "updated",
            "beta": "b"
        },
        "name": "linear",
        "url": "https://linear.example"
    });

    std::fs::write(&path, base)?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    service
        .write_value(ConfigValueWriteParams {
            file_path: Some(path.display().to_string()),
            key_path: "services.linear".to_string(),
            value: overlay.clone(),
            merge_strategy: MergeStrategy::Upsert,
            expected_version: None,
        })
        .await
        .expect("upsert succeeds");

    let upserted: TomlValue = toml::from_str(&std::fs::read_to_string(&path)?)?;
    let expected_upsert: TomlValue = toml::from_str(
        r#"[services.linear]
bearer_token_env_var = "NEW_TOKEN"
name = "linear"
url = "https://linear.example"

[services.linear.env_http_headers]
existing = "keep"

[services.linear.http_headers]
alpha = "updated"
beta = "b"
"#,
    )?;
    assert_eq!(upserted, expected_upsert);

    std::fs::write(&path, base)?;

    service
        .write_value(ConfigValueWriteParams {
            file_path: Some(path.display().to_string()),
            key_path: "services.linear".to_string(),
            value: overlay,
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("replace succeeds");

    let replaced: TomlValue = toml::from_str(&std::fs::read_to_string(&path)?)?;
    let expected_replace: TomlValue = toml::from_str(
        r#"[services.linear]
bearer_token_env_var = "NEW_TOKEN"
name = "linear"
url = "https://linear.example"

[services.linear.http_headers]
alpha = "updated"
beta = "b"
"#,
    )?;
    assert_eq!(replaced, expected_replace);

    Ok(())
}

#[tokio::test]
async fn config_writes_apply_path_sensitive_merge_rules() -> Result<()> {
    let cases = [
        (
            r#"[shell_environment_policy.filters]
"aws_*" = "exclude"
"#,
            "shell_environment_policy.filters",
            serde_json::json!({"AWS_*": "include"}),
            r#"[shell_environment_policy.filters]
"aws_*" = "include"
"#,
        ),
        (
            r#"[shell_environment_policy.filters]
"aws_*" = "exclude"
"#,
            "shell_environment_policy.filters.AWS_*",
            serde_json::json!("include"),
            r#"[shell_environment_policy.filters]
"aws_*" = "include"
"#,
        ),
        (
            r#"[shell_environment_policy.filters]
"секрет_*" = "exclude"
"#,
            "shell_environment_policy.filters.СЕКРЕТ_*",
            serde_json::json!("include"),
            r#"[shell_environment_policy.filters]
"секрет_*" = "include"
"#,
        ),
        (
            r#"[features]
multi_agent_v2 = true
"#,
            "features.multi_agent_v2.subagent_usage_hint_text",
            serde_json::json!("Delegate carefully."),
            r#"[features.multi_agent_v2]
enabled = true
subagent_usage_hint_text = "Delegate carefully."
"#,
        ),
        (
            r#"[features]
multi_agent_v2 = true
"#,
            "features.multi_agent_v2",
            serde_json::json!({"subagent_usage_hint_text": "Delegate carefully."}),
            r#"[features.multi_agent_v2]
enabled = true
subagent_usage_hint_text = "Delegate carefully."
"#,
        ),
        (
            r#"[features.multi_agent_v2]
enabled = true
subagent_usage_hint_text = "Delegate carefully."
"#,
            "features.multi_agent_v2",
            serde_json::json!(false),
            r#"[features.multi_agent_v2]
enabled = false
subagent_usage_hint_text = "Delegate carefully."
"#,
        ),
        (
            r#"[features.multi_agent_v2]
enabled = true
subagent_usage_hint_text = "Delegate carefully."
"#,
            "features.multi_agent_v2",
            serde_json::Value::Null,
            "",
        ),
        (
            r#"[desktop.features.multi_agent_v2]
custom = true
"#,
            "desktop.features.multi_agent_v2",
            serde_json::json!(false),
            r#"[desktop.features]
multi_agent_v2 = false
"#,
        ),
        (
            r#"[desktop.features]
multi_agent_v2 = true
"#,
            "desktop.features.multi_agent_v2",
            serde_json::json!({"custom": true}),
            r#"[desktop.features.multi_agent_v2]
custom = true
"#,
        ),
    ];

    for (base, key_path, value, expected) in cases {
        let tmp = tempdir()?;
        let path = tmp.path().join(CONFIG_TOML_FILE);
        std::fs::write(&path, base)?;

        let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
        service
            .write_value(ConfigValueWriteParams {
                file_path: Some(path.display().to_string()),
                key_path: key_path.to_string(),
                value,
                merge_strategy: MergeStrategy::Upsert,
                expected_version: None,
            })
            .await?;

        let updated: TomlValue = toml::from_str(&std::fs::read_to_string(&path)?)?;
        let expected: TomlValue = toml::from_str(expected)?;
        assert_eq!(updated, expected);

        service
            .read(ConfigReadParams {
                include_layers: false,
                cwd: None,
            })
            .await?;
    }

    Ok(())
}

#[tokio::test]
async fn clear_shell_environment_filter_ignores_ascii_case() -> Result<()> {
    let tmp = tempdir()?;
    let path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(
        &path,
        r#"[shell_environment_policy.filters]
"aws_*" = "exclude"
"keep_*" = "include"
"#,
    )?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    let response = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(path.display().to_string()),
            key_path: "shell_environment_policy.filters.AWS_*".to_string(),
            value: serde_json::Value::Null,
            merge_strategy: MergeStrategy::Upsert,
            expected_version: None,
        })
        .await?;

    assert_eq!(response.status, WriteStatus::Ok);
    assert_eq!(response.overridden_metadata, None);
    assert_eq!(
        std::fs::read_to_string(&path)?,
        r#"[shell_environment_policy.filters]
"keep_*" = "include"
"#
    );
    service
        .read(ConfigReadParams {
            include_layers: false,
            cwd: None,
        })
        .await?;

    Ok(())
}

#[tokio::test]
async fn upsert_shell_environment_scalar_preserves_unrelated_formatting() -> Result<()> {
    let tmp = tempdir()?;
    let path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(
        &path,
        r#"[shell_environment_policy]
inherit = "all"
set = { KEEP = "1", OTHER = "2" } # keep this inline table

[shell_environment_policy.filters]
"AWS_*" = "exclude" # keep this filter
"#,
    )?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    service
        .write_value(ConfigValueWriteParams {
            file_path: Some(path.display().to_string()),
            key_path: "shell_environment_policy.inherit".to_string(),
            value: serde_json::json!("core"),
            merge_strategy: MergeStrategy::Upsert,
            expected_version: None,
        })
        .await?;

    assert_eq!(
        std::fs::read_to_string(&path)?,
        r#"[shell_environment_policy]
inherit = "core"
set = { KEEP = "1", OTHER = "2" } # keep this inline table

[shell_environment_policy.filters]
"AWS_*" = "exclude" # keep this filter
"#
    );
    service
        .read(ConfigReadParams {
            include_layers: false,
            cwd: None,
        })
        .await?;

    Ok(())
}

#[tokio::test]
async fn upsert_shell_environment_filter_scalar_preserves_formatting_and_version() -> Result<()> {
    let tmp = tempdir()?;
    let path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(
        &path,
        r#"[shell_environment_policy]
set = { KEEP = "1", OTHER = "2" } # keep this inline table

[shell_environment_policy.filters]
"AWS_*" = "exclude" # keep this edited comment
"KEEP_*" = "include" # keep this untouched comment
"#,
    )?;
    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());

    let response = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(path.display().to_string()),
            key_path: "shell_environment_policy.filters.aws_*".to_string(),
            value: serde_json::json!("include"),
            merge_strategy: MergeStrategy::Upsert,
            expected_version: None,
        })
        .await?;

    assert_eq!(
        std::fs::read_to_string(&path)?,
        r#"[shell_environment_policy]
set = { KEEP = "1", OTHER = "2" } # keep this inline table

[shell_environment_policy.filters]
"AWS_*" = "include" # keep this edited comment
"KEEP_*" = "include" # keep this untouched comment
"#
    );
    service
        .write_value(ConfigValueWriteParams {
            file_path: Some(path.display().to_string()),
            key_path: "shell_environment_policy.filters.AWS_*".to_string(),
            value: serde_json::json!("exclude"),
            merge_strategy: MergeStrategy::Upsert,
            expected_version: Some(response.version),
        })
        .await?;
    service
        .read(ConfigReadParams {
            include_layers: false,
            cwd: None,
        })
        .await?;

    Ok(())
}

#[tokio::test]
async fn shell_environment_upsert_rejects_case_variant_filters_in_one_edit() -> Result<()> {
    let tmp = tempdir()?;
    let path = tmp.path().join(CONFIG_TOML_FILE);
    let initial = r#"[shell_environment_policy.filters]
"KEEP_*" = "include"
"#;
    std::fs::write(&path, initial)?;
    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());

    let error = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(path.display().to_string()),
            key_path: "shell_environment_policy.filters".to_string(),
            value: serde_json::json!({"AWS_*": "include", "aws_*": "exclude"}),
            merge_strategy: MergeStrategy::Upsert,
            expected_version: None,
        })
        .await
        .expect_err("one filter-map edit must not contain case-variant keys");

    assert_eq!(
        error.write_error_code(),
        Some(ConfigWriteErrorCode::ConfigValidationError)
    );
    assert!(
        error
            .to_string()
            .contains("duplicate shell environment filter")
    );
    assert_eq!(std::fs::read_to_string(&path)?, initial);
    Ok(())
}
