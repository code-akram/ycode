use std::time::Duration;

use anyhow::Result;
use anyhow::bail;
use app_test_support::ChatGptAuthFixture;
use app_test_support::DEFAULT_CLIENT_NAME;
use app_test_support::TestAppServer;
use app_test_support::start_analytics_events_server;
use app_test_support::write_chatgpt_auth;
use codex_app_server_protocol::PluginAuthPolicy;
use codex_app_server_protocol::PluginAvailability;
use codex_app_server_protocol::PluginInstallParams;
use codex_app_server_protocol::PluginInstallResponse;
use codex_app_server_protocol::RequestId;
use codex_config::types::AuthCredentialsStoreMode;
use codex_utils_absolute_path::AbsolutePathBuf;
use flate2::Compression;
use flate2::write::GzEncoder;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Match;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

// Plugin install tests wait on connector discovery after the install response path
// starts, which is noticeably slower on Windows CI.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const REMOTE_PLUGIN_ID: &str = "plugins~Plugin_00000000000000000000000000000000";
const TEST_ALLOW_HTTP_REMOTE_PLUGIN_BUNDLE_DOWNLOADS: &str =
    "CODEX_TEST_ALLOW_HTTP_REMOTE_PLUGIN_BUNDLE_DOWNLOADS";

#[tokio::test]
async fn plugin_install_rejects_relative_marketplace_paths() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_raw_request(
            "plugin/install",
            Some(serde_json::json!({
                "marketplacePath": "relative-marketplace.json",
                "pluginName": "missing-plugin",
            })),
        )
        .await?;

    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(err.error.message.contains("Invalid request"));
    Ok(())
}

#[tokio::test]
async fn plugin_install_rejects_missing_install_source() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: None,
            remote_marketplace_name: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;

    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(
        err.error
            .message
            .contains("requires exactly one of marketplacePath or remoteMarketplaceName")
    );
    Ok(())
}

#[tokio::test]
async fn plugin_install_rejects_multiple_install_sources() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(AbsolutePathBuf::try_from(
                codex_home.path().join("marketplace.json"),
            )?),
            remote_marketplace_name: Some("openai-curated-remote".to_string()),
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;

    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(
        err.error
            .message
            .contains("requires exactly one of marketplacePath or remoteMarketplaceName")
    );
    Ok(())
}

#[tokio::test]
async fn plugin_install_rejects_remote_marketplace_when_plugins_are_disabled() -> Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"[features]
plugins = false
"#,
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: None,
            remote_marketplace_name: Some("openai-curated-remote".to_string()),
            plugin_name: "plugins~Plugin_22222222222222222222222222222222".to_string(),
        })
        .await?;

    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(
        err.error
            .message
            .contains("remote plugin install is not enabled")
    );
    Ok(())
}

#[tokio::test]
async fn plugin_install_writes_remote_plugin_to_cloud_and_cache() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = MockServer::start().await;
    let installed_path = codex_home
        .path()
        .join("plugins/cache/openai-curated-remote/linear/1.2.3");
    let bundle_url = mount_remote_plugin_bundle(
        &server,
        /*status_code*/ 200,
        remote_plugin_bundle_tar_gz_bytes_with_contents(r#"{"name":"linear","version":"0.0.1"}"#)?,
    )
    .await;
    configure_remote_plugin_test(codex_home.path(), &server)?;
    mount_remote_plugin_detail(&server, REMOTE_PLUGIN_ID, "1.2.3", Some(&bundle_url)).await;
    mount_empty_remote_installed_plugins(&server).await;
    mount_remote_plugin_install_after_cache_write(
        &server,
        REMOTE_PLUGIN_ID,
        installed_path.join(".codex-plugin/plugin.json"),
    )
    .await;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[(TEST_ALLOW_HTTP_REMOTE_PLUGIN_BUNDLE_DOWNLOADS, Some("1"))])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = send_remote_plugin_install_request(&mut mcp, REMOTE_PLUGIN_ID).await?;
    let response: PluginInstallResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;

    assert_eq!(
        response,
        PluginInstallResponse {
            auth_policy: PluginAuthPolicy::OnUse,
        }
    );
    wait_for_remote_plugin_request_count(
        &server,
        "POST",
        &format!("/ps/plugins/{REMOTE_PLUGIN_ID}/install"),
        /*expected_count*/ 1,
    )
    .await?;
    wait_for_remote_plugin_request_count(
        &server,
        "GET",
        "/bundles/linear.tar.gz",
        /*expected_count*/ 1,
    )
    .await?;
    assert!(installed_path.join(".codex-plugin/plugin.json").is_file());
    let installed_plugin_manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(installed_path.join(".codex-plugin/plugin.json"))?,
    )?;
    assert_eq!(installed_plugin_manifest["name"], json!("linear"));
    assert_eq!(installed_plugin_manifest["version"], json!("1.2.3"));
    assert!(installed_path.join("skills/plan-work/SKILL.md").is_file());
    assert!(
        !codex_home
            .path()
            .join(format!(
                "plugins/cache/openai-curated-remote/{REMOTE_PLUGIN_ID}/1.2.3"
            ))
            .exists()
    );
    Ok(())
}
#[tokio::test]
async fn plugin_install_rejects_missing_remote_bundle_url() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = MockServer::start().await;
    configure_remote_plugin_test(codex_home.path(), &server)?;
    mount_remote_plugin_detail(
        &server,
        REMOTE_PLUGIN_ID,
        "1.2.3",
        /*bundle_download_url*/ None,
    )
    .await;
    mount_empty_remote_installed_plugins(&server).await;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = send_remote_plugin_install_request(&mut mcp, REMOTE_PLUGIN_ID).await?;
    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32603);
    assert!(
        err.error
            .message
            .contains("backend did not return a download URL")
    );
    wait_for_remote_plugin_request_count(
        &server,
        "POST",
        &format!("/ps/plugins/{REMOTE_PLUGIN_ID}/install"),
        /*expected_count*/ 0,
    )
    .await?;
    assert!(
        !codex_home
            .path()
            .join("plugins/cache/openai-curated-remote/linear")
            .exists()
    );
    Ok(())
}

#[tokio::test]
async fn plugin_install_rejects_plain_http_remote_bundle_url() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = MockServer::start().await;
    let bundle_url = format!("{}/bundles/linear.tar.gz", server.uri());
    configure_remote_plugin_test(codex_home.path(), &server)?;
    mount_remote_plugin_detail(&server, REMOTE_PLUGIN_ID, "1.2.3", Some(&bundle_url)).await;
    mount_empty_remote_installed_plugins(&server).await;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = send_remote_plugin_install_request(&mut mcp, REMOTE_PLUGIN_ID).await?;
    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32603);
    assert!(
        err.error
            .message
            .contains("unsupported download URL scheme")
    );
    wait_for_remote_plugin_request_count(
        &server,
        "POST",
        &format!("/ps/plugins/{REMOTE_PLUGIN_ID}/install"),
        /*expected_count*/ 0,
    )
    .await?;
    assert!(
        !codex_home
            .path()
            .join("plugins/cache/openai-curated-remote/linear")
            .exists()
    );
    Ok(())
}

#[tokio::test]
async fn plugin_install_rejects_invalid_remote_release_version() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = MockServer::start().await;
    configure_remote_plugin_test(codex_home.path(), &server)?;
    mount_remote_plugin_detail(
        &server,
        REMOTE_PLUGIN_ID,
        "../1.2.3",
        Some("https://127.0.0.1:1/bundles/linear.tar.gz"),
    )
    .await;
    mount_empty_remote_installed_plugins(&server).await;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = send_remote_plugin_install_request(&mut mcp, REMOTE_PLUGIN_ID).await?;
    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32603);
    assert!(err.error.message.contains("invalid release version"));
    wait_for_remote_plugin_request_count(
        &server,
        "POST",
        &format!("/ps/plugins/{REMOTE_PLUGIN_ID}/install"),
        /*expected_count*/ 0,
    )
    .await?;
    assert!(
        !codex_home
            .path()
            .join("plugins/cache/openai-curated-remote/linear")
            .exists()
    );
    Ok(())
}

#[tokio::test]
async fn plugin_install_rejects_invalid_remote_plugin_name() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_remote_plugin_catalog_config(codex_home.path(), "https://example.invalid/backend-api/")?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: None,
            remote_marketplace_name: Some("openai-curated-remote".to_string()),
            plugin_name: "linear/../../oops".to_string(),
        })
        .await?;

    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(err.error.message.contains("invalid remote plugin id"));
    Ok(())
}

#[tokio::test]
async fn plugin_install_tracks_analytics_when_remote_detail_fetch_fails() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = MockServer::start().await;
    configure_remote_plugin_test(codex_home.path(), &server)?;
    mount_empty_remote_installed_plugins(&server).await;
    mount_backend_analytics_events(&server).await;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = send_remote_plugin_install_request(&mut mcp, REMOTE_PLUGIN_ID).await?;
    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(err.error.message.contains("failed with status 404"));

    let payload = wait_for_plugin_analytics_payload(&server).await?;
    let event_params = &payload["events"][0]["event_params"];
    assert_eq!(
        payload["events"][0]["event_type"],
        "codex_plugin_install_failed"
    );
    assert_eq!(event_params["plugin_id"], json!(null));
    assert_eq!(event_params["remote_plugin_id"], REMOTE_PLUGIN_ID);
    assert_eq!(event_params["plugin_name"], json!(null));
    assert_eq!(event_params["marketplace_name"], json!(null));
    assert_eq!(event_params["source"], "manual");
    assert_eq!(
        event_params["error_type"],
        "remote_catalog_unexpected_status"
    );
    Ok(())
}

#[tokio::test]
async fn plugin_install_rejects_remote_plugin_disabled_by_admin_before_download() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = MockServer::start().await;
    let bundle_url = mount_remote_plugin_bundle(
        &server,
        /*status_code*/ 200,
        remote_plugin_bundle_tar_gz_bytes("linear")?,
    )
    .await;
    configure_remote_plugin_test(codex_home.path(), &server)?;
    mount_remote_plugin_detail_with_status(
        &server,
        REMOTE_PLUGIN_ID,
        "1.2.3",
        Some(&bundle_url),
        PluginAvailability::DisabledByAdmin,
    )
    .await;
    mount_empty_remote_installed_plugins(&server).await;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[(TEST_ALLOW_HTTP_REMOTE_PLUGIN_BUNDLE_DOWNLOADS, Some("1"))])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = send_remote_plugin_install_request(&mut mcp, REMOTE_PLUGIN_ID).await?;
    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(err.error.message.contains("disabled by admin"));
    wait_for_remote_plugin_request_count(
        &server,
        "GET",
        "/bundles/linear.tar.gz",
        /*expected_count*/ 0,
    )
    .await?;
    wait_for_remote_plugin_request_count(
        &server,
        "POST",
        &format!("/ps/plugins/{REMOTE_PLUGIN_ID}/install"),
        /*expected_count*/ 0,
    )
    .await?;
    assert!(
        !codex_home
            .path()
            .join("plugins/cache/openai-curated-remote/linear")
            .exists()
    );
    Ok(())
}

#[tokio::test]
async fn plugin_install_rejects_remote_plugin_not_available() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = MockServer::start().await;
    configure_remote_plugin_test(codex_home.path(), &server)?;
    mount_remote_plugin_detail_with_install_policy(
        &server,
        REMOTE_PLUGIN_ID,
        "1.2.3",
        /*install_policy*/ "NOT_AVAILABLE",
    )
    .await;
    mount_empty_remote_installed_plugins(&server).await;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = send_remote_plugin_install_request(&mut mcp, REMOTE_PLUGIN_ID).await?;
    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(err.error.message.contains("not available for install"));
    wait_for_remote_plugin_request_count(
        &server,
        "POST",
        &format!("/ps/plugins/{REMOTE_PLUGIN_ID}/install"),
        /*expected_count*/ 0,
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn plugin_install_rejects_when_workspace_codex_plugins_disabled() -> Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    let server = MockServer::start().await;
    write_plugins_enabled_config_with_base_url(
        codex_home.path(),
        &format!("{}/backend-api/", server.uri()),
    )?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("chatgpt-token")
            .account_id("account-123")
            .chatgpt_user_id("user-123")
            .chatgpt_account_id("account-123")
            .plan_type("team"),
        AuthCredentialsStoreMode::File,
    )?;
    write_plugin_marketplace(
        repo_root.path(),
        "debug",
        "sample-plugin",
        "./sample-plugin",
        /*install_policy*/ None,
        /*auth_policy*/ None,
    )?;
    write_plugin_source(repo_root.path(), "sample-plugin")?;
    let marketplace_path =
        AbsolutePathBuf::try_from(repo_root.path().join(".agents/plugins/marketplace.json"))?;

    Mock::given(method("GET"))
        .and(path("/backend-api/accounts/account-123/settings"))
        .and(header("authorization", "Bearer chatgpt-token"))
        .and(header("chatgpt-account-id", "account-123"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"beta_settings":{"enable_plugins":false}}"#),
        )
        .mount(&server)
        .await;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(marketplace_path),
            remote_marketplace_name: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;

    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(
        err.error
            .message
            .contains("Codex plugins are disabled for this workspace")
    );
    Ok(())
}

#[tokio::test]
async fn plugin_install_returns_invalid_request_for_missing_marketplace_file() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(AbsolutePathBuf::try_from(
                codex_home.path().join("missing-marketplace.json"),
            )?),
            remote_marketplace_name: None,
            plugin_name: "missing-plugin".to_string(),
        })
        .await?;

    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(err.error.message.contains("marketplace file"));
    assert!(err.error.message.contains("does not exist"));
    Ok(())
}

#[tokio::test]
async fn plugin_install_returns_invalid_request_for_not_available_plugin() -> Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    write_plugin_marketplace(
        repo_root.path(),
        "debug",
        "sample-plugin",
        "./sample-plugin",
        Some("NOT_AVAILABLE"),
        /*auth_policy*/ None,
    )?;
    write_plugin_source(repo_root.path(), "sample-plugin")?;
    let marketplace_path =
        AbsolutePathBuf::try_from(repo_root.path().join(".agents/plugins/marketplace.json"))?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(marketplace_path),
            remote_marketplace_name: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;

    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(err.error.message.contains("not available for install"));
    Ok(())
}

#[tokio::test]
async fn plugin_install_returns_invalid_request_for_disallowed_product_plugin() -> Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    std::fs::create_dir_all(repo_root.path().join(".agents/plugins"))?;
    std::fs::write(
        repo_root.path().join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample-plugin",
      "source": {
        "source": "local",
        "path": "./sample-plugin"
      },
      "policy": {
        "products": ["CHATGPT"]
      }
    }
  ]
}"#,
    )?;
    write_plugin_source(repo_root.path(), "sample-plugin")?;
    let marketplace_path =
        AbsolutePathBuf::try_from(repo_root.path().join(".agents/plugins/marketplace.json"))?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_args(&["--session-source", "atlas"])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(marketplace_path),
            remote_marketplace_name: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;

    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32600);
    assert!(err.error.message.contains("not available for install"));
    Ok(())
}

#[tokio::test]
async fn plugin_install_tracks_analytics_event() -> Result<()> {
    let analytics_server = start_analytics_events_server().await?;
    let codex_home = TempDir::new()?;
    write_analytics_config(codex_home.path(), &analytics_server.uri())?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("chatgpt-token")
            .account_id("account-123")
            .chatgpt_user_id("user-123")
            .chatgpt_account_id("account-123"),
        AuthCredentialsStoreMode::File,
    )?;

    let repo_root = TempDir::new()?;
    write_plugin_marketplace(
        repo_root.path(),
        "debug",
        "sample-plugin",
        "./sample-plugin",
        /*install_policy*/ None,
        /*auth_policy*/ None,
    )?;
    write_plugin_source(repo_root.path(), "sample-plugin")?;
    let marketplace_path =
        AbsolutePathBuf::try_from(repo_root.path().join(".agents/plugins/marketplace.json"))?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(marketplace_path),
            remote_marketplace_name: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;
    let _response: PluginInstallResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;

    let payload = wait_for_plugin_analytics_payload(&analytics_server).await?;
    assert_eq!(
        payload,
        json!({
            "events": [{
                "event_type": "codex_plugin_installed",
                "event_params": {
                    "plugin_id": "sample-plugin@debug",
                    "remote_plugin_id": null,
                    "plugin_name": "sample-plugin",
                    "marketplace_name": "debug",
                    "has_skills": false,
                    "mcp_server_count": 0,
                    "connector_ids": [],
                    "product_client_id": DEFAULT_CLIENT_NAME,
                }
            }]
        })
    );
    Ok(())
}

#[tokio::test]
async fn plugin_install_failure_tracks_analytics_event() -> Result<()> {
    let analytics_server = start_analytics_events_server().await?;
    let codex_home = TempDir::new()?;
    write_analytics_config(codex_home.path(), &analytics_server.uri())?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("chatgpt-token")
            .account_id("account-123")
            .chatgpt_user_id("user-123")
            .chatgpt_account_id("account-123"),
        AuthCredentialsStoreMode::File,
    )?;

    let repo_root = TempDir::new()?;
    write_plugin_marketplace(
        repo_root.path(),
        "debug",
        "sample-plugin",
        "./missing-plugin",
        /*install_policy*/ None,
        /*auth_policy*/ None,
    )?;
    let marketplace_path =
        AbsolutePathBuf::try_from(repo_root.path().join(".agents/plugins/marketplace.json"))?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = mcp
        .send_plugin_install_request(PluginInstallParams {
            marketplace_path: Some(marketplace_path),
            remote_marketplace_name: None,
            plugin_name: "sample-plugin".to_string(),
        })
        .await?;
    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(err.error.code, -32600);

    let payload = wait_for_plugin_analytics_payload(&analytics_server).await?;
    let event_params = &payload["events"][0]["event_params"];
    assert_eq!(
        payload["events"][0]["event_type"],
        "codex_plugin_install_failed"
    );
    assert_eq!(event_params["plugin_id"], "sample-plugin@debug");
    assert_eq!(event_params["remote_plugin_id"], json!(null));
    assert_eq!(event_params["plugin_name"], "sample-plugin");
    assert_eq!(event_params["marketplace_name"], "debug");
    assert_eq!(event_params["has_skills"], json!(null));
    assert_eq!(event_params["mcp_server_count"], json!(null));
    assert_eq!(event_params["connector_ids"], json!(null));
    assert_eq!(event_params["product_client_id"], DEFAULT_CLIENT_NAME);
    assert_eq!(event_params["source"], "manual");
    assert_eq!(event_params["error_type"], "store_invalid");
    Ok(())
}

#[tokio::test]
async fn plugin_install_tracks_remote_plugin_analytics_event() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = MockServer::start().await;
    let bundle_url = mount_remote_plugin_bundle(
        &server,
        /*status_code*/ 200,
        remote_plugin_bundle_tar_gz_bytes("linear")?,
    )
    .await;
    configure_remote_plugin_test(codex_home.path(), &server)?;
    mount_remote_plugin_detail(&server, REMOTE_PLUGIN_ID, "1.2.3", Some(&bundle_url)).await;
    mount_empty_remote_installed_plugins(&server).await;
    mount_remote_plugin_install(&server, REMOTE_PLUGIN_ID).await;
    mount_backend_analytics_events(&server).await;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[(TEST_ALLOW_HTTP_REMOTE_PLUGIN_BUNDLE_DOWNLOADS, Some("1"))])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = send_remote_plugin_install_request(&mut mcp, REMOTE_PLUGIN_ID).await?;
    let _response: PluginInstallResponse =
        timeout(DEFAULT_TIMEOUT, mcp.read_response(request_id)).await??;

    let payload = wait_for_plugin_analytics_payload(&server).await?;
    assert_eq!(
        payload,
        json!({
            "events": [{
                "event_type": "codex_plugin_installed",
                "event_params": {
                    "plugin_id": "linear@openai-curated-remote",
                    "remote_plugin_id": REMOTE_PLUGIN_ID,
                    "plugin_name": "linear",
                    "marketplace_name": "openai-curated-remote",
                    "has_skills": true,
                    "mcp_server_count": 0,
                    "connector_ids": [],
                    "product_client_id": DEFAULT_CLIENT_NAME,
                }
            }]
        })
    );
    Ok(())
}

#[tokio::test]
async fn plugin_install_preserves_status_when_remote_bundle_error_body_is_too_large() -> Result<()>
{
    let codex_home = TempDir::new()?;
    let server = MockServer::start().await;
    let bundle_url =
        mount_remote_plugin_bundle(&server, /*status_code*/ 503, vec![b'x'; 8 * 1024 + 1]).await;
    configure_remote_plugin_test(codex_home.path(), &server)?;
    mount_remote_plugin_detail(&server, REMOTE_PLUGIN_ID, "1.2.3", Some(&bundle_url)).await;
    mount_empty_remote_installed_plugins(&server).await;
    mount_remote_plugin_install(&server, REMOTE_PLUGIN_ID).await;
    mount_backend_analytics_events(&server).await;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[(TEST_ALLOW_HTTP_REMOTE_PLUGIN_BUNDLE_DOWNLOADS, Some("1"))])
        .build_initialized_with_timeout(DEFAULT_TIMEOUT)
        .await?;

    let request_id = send_remote_plugin_install_request(&mut mcp, REMOTE_PLUGIN_ID).await?;
    let err = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(err.error.code, -32603);
    assert!(err.error.message.contains("failed with status 503"));
    assert!(
        err.error
            .message
            .contains("[response body truncated after 8192 bytes]")
    );
    assert_eq!(
        err.error
            .message
            .bytes()
            .filter(|byte| *byte == b'x')
            .count(),
        8192
    );
    assert!(!err.error.message.contains("exceeded maximum size"));
    wait_for_remote_plugin_request_count(
        &server,
        "GET",
        "/bundles/linear.tar.gz",
        /*expected_count*/ 1,
    )
    .await?;
    wait_for_remote_plugin_request_count(
        &server,
        "POST",
        &format!("/ps/plugins/{REMOTE_PLUGIN_ID}/install"),
        /*expected_count*/ 0,
    )
    .await?;
    let payload = wait_for_plugin_analytics_payload(&server).await?;
    let event_params = &payload["events"][0]["event_params"];
    assert_eq!(
        payload["events"][0]["event_type"],
        "codex_plugin_install_failed"
    );
    assert_eq!(event_params["plugin_id"], "linear@openai-curated-remote");
    assert_eq!(event_params["remote_plugin_id"], REMOTE_PLUGIN_ID);
    assert_eq!(event_params["marketplace_name"], "openai-curated-remote");
    assert_eq!(event_params["source"], "manual");
    assert_eq!(event_params["error_type"], "remote_bundle_download_status");
    assert!(
        !codex_home
            .path()
            .join("plugins/cache/openai-curated-remote/linear")
            .exists()
    );
    Ok(())
}
fn write_plugins_enabled_config_with_base_url(
    codex_home: &std::path::Path,
    base_url: &str,
) -> std::io::Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"chatgpt_base_url = "{base_url}"

[features]
plugins = true
"#,
        ),
    )
}

fn write_analytics_config(codex_home: &std::path::Path, base_url: &str) -> std::io::Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        format!("chatgpt_base_url = \"{base_url}\"\n"),
    )
}

async fn mount_backend_analytics_events(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/analytics-events/events"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
        .mount(server)
        .await;
}

async fn wait_for_plugin_analytics_payload(server: &MockServer) -> Result<serde_json::Value> {
    timeout(DEFAULT_TIMEOUT, async {
        loop {
            let Some(requests) = server.received_requests().await else {
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            };
            if let Some(request) = requests.iter().find(|request| {
                request.method == "POST"
                    && request
                        .url
                        .path()
                        .ends_with("/codex/analytics-events/events")
            }) {
                return serde_json::from_slice(&request.body)
                    .map_err(|err| anyhow::anyhow!("invalid analytics payload: {err}"));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?
}

fn write_remote_plugin_catalog_config(
    codex_home: &std::path::Path,
    base_url: &str,
) -> std::io::Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"
chatgpt_base_url = "{base_url}"

[features]
plugins = true
"#
        ),
    )
}

fn configure_remote_plugin_test(codex_home: &std::path::Path, server: &MockServer) -> Result<()> {
    write_remote_plugin_catalog_config(codex_home, &format!("{}/backend-api/", server.uri()))?;
    write_chatgpt_auth(
        codex_home,
        ChatGptAuthFixture::new("chatgpt-token")
            .account_id("account-123")
            .chatgpt_user_id("user-123")
            .chatgpt_account_id("account-123"),
        AuthCredentialsStoreMode::File,
    )
}

async fn mount_remote_plugin_bundle(
    server: &MockServer,
    status_code: u16,
    body: Vec<u8>,
) -> String {
    Mock::given(method("GET"))
        .and(path("/bundles/linear.tar.gz"))
        .respond_with(
            ResponseTemplate::new(status_code)
                .insert_header("content-type", "application/gzip")
                .set_body_bytes(body),
        )
        .mount(server)
        .await;
    format!("{}/bundles/linear.tar.gz", server.uri())
}

async fn mount_remote_plugin_detail(
    server: &MockServer,
    remote_plugin_id: &str,
    release_version: &str,
    bundle_download_url: Option<&str>,
) {
    mount_remote_plugin_detail_with_status(
        server,
        remote_plugin_id,
        release_version,
        bundle_download_url,
        PluginAvailability::Available,
    )
    .await;
}

async fn mount_remote_plugin_detail_with_status(
    server: &MockServer,
    remote_plugin_id: &str,
    release_version: &str,
    bundle_download_url: Option<&str>,
    status: PluginAvailability,
) {
    mount_remote_plugin_detail_with_options(
        server,
        remote_plugin_id,
        release_version,
        bundle_download_url,
        status,
        "AVAILABLE",
    )
    .await;
}

async fn mount_remote_plugin_detail_with_install_policy(
    server: &MockServer,
    remote_plugin_id: &str,
    release_version: &str,
    install_policy: &str,
) {
    mount_remote_plugin_detail_with_options(
        server,
        remote_plugin_id,
        release_version,
        /*bundle_download_url*/ None,
        PluginAvailability::Available,
        install_policy,
    )
    .await;
}

async fn mount_remote_plugin_detail_with_options(
    server: &MockServer,
    remote_plugin_id: &str,
    release_version: &str,
    bundle_download_url: Option<&str>,
    status: PluginAvailability,
    install_policy: &str,
) {
    let status = match status {
        PluginAvailability::Available => "ENABLED",
        PluginAvailability::DisabledByAdmin => "DISABLED_BY_ADMIN",
    };
    let bundle_download_url_field = bundle_download_url
        .map(|url| format!(r#"    "bundle_download_url": "{url}","#))
        .unwrap_or_default();
    let detail_body = format!(
        r#"{{
  "id": "{remote_plugin_id}",
  "name": "linear",
  "scope": "GLOBAL",
  "installation_policy": "{install_policy}",
  "authentication_policy": "ON_USE",
  "status": "{status}",
  "release": {{
    "version": "{release_version}",
{bundle_download_url_field}
    "display_name": "Linear",
    "description": "Track work in Linear",
    "interface": {{
      "short_description": "Plan and track work"
    }},
    "skills": []
  }}
}}"#
    );

    Mock::given(method("GET"))
        .and(path(format!("/backend-api/ps/plugins/{remote_plugin_id}")))
        .and(query_param("includeDownloadUrls", "true"))
        .and(header("authorization", "Bearer chatgpt-token"))
        .and(header("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(200).set_body_string(detail_body))
        .mount(server)
        .await;
}

async fn mount_empty_remote_installed_plugins(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/backend-api/ps/plugins/installed"))
        .and(query_param("scope", "GLOBAL"))
        .and(header("authorization", "Bearer chatgpt-token"))
        .and(header("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{
  "plugins": [],
  "pagination": {
    "limit": 50,
    "next_page_token": null
  }
}"#,
        ))
        .mount(server)
        .await;
}

async fn mount_remote_plugin_install(server: &MockServer, remote_plugin_id: &str) {
    Mock::given(method("POST"))
        .and(path(format!(
            "/backend-api/ps/plugins/{remote_plugin_id}/install"
        )))
        .and(header("authorization", "Bearer chatgpt-token"))
        .and(header("chatgpt-account-id", "account-123"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"id":"{remote_plugin_id}","enabled":true}}"#)),
        )
        .mount(server)
        .await;
}

#[derive(Debug, Clone)]
struct CacheManifestExists {
    manifest_path: std::path::PathBuf,
}

impl Match for CacheManifestExists {
    fn matches(&self, _request: &Request) -> bool {
        self.manifest_path.is_file()
    }
}

async fn mount_remote_plugin_install_after_cache_write(
    server: &MockServer,
    remote_plugin_id: &str,
    manifest_path: std::path::PathBuf,
) {
    Mock::given(method("POST"))
        .and(path(format!(
            "/backend-api/ps/plugins/{remote_plugin_id}/install"
        )))
        .and(header("authorization", "Bearer chatgpt-token"))
        .and(header("chatgpt-account-id", "account-123"))
        .and(CacheManifestExists { manifest_path })
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"id":"{remote_plugin_id}","enabled":true}}"#)),
        )
        .mount(server)
        .await;
}

async fn send_remote_plugin_install_request(
    mcp: &mut TestAppServer,
    remote_plugin_id: &str,
) -> Result<i64> {
    mcp.send_plugin_install_request(PluginInstallParams {
        marketplace_path: None,
        remote_marketplace_name: Some("caller-marketplace-is-ignored".to_string()),
        plugin_name: remote_plugin_id.to_string(),
    })
    .await
}

async fn wait_for_remote_plugin_request_count(
    server: &MockServer,
    method_name: &str,
    path_suffix: &str,
    expected_count: usize,
) -> Result<()> {
    timeout(DEFAULT_TIMEOUT, async {
        loop {
            let Some(requests) = server.received_requests().await else {
                bail!("wiremock did not record requests");
            };
            let request_count = requests
                .iter()
                .filter(|request| {
                    request.method == method_name && request.url.path().ends_with(path_suffix)
                })
                .count();
            if request_count == expected_count {
                return Ok::<(), anyhow::Error>(());
            }
            if request_count > expected_count {
                bail!(
                    "expected exactly {expected_count} {method_name} {path_suffix} requests, got {request_count}"
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(())
}

fn write_plugin_marketplace(
    repo_root: &std::path::Path,
    marketplace_name: &str,
    plugin_name: &str,
    source_path: &str,
    install_policy: Option<&str>,
    auth_policy: Option<&str>,
) -> std::io::Result<()> {
    let policy = if install_policy.is_some() || auth_policy.is_some() {
        let installation = install_policy
            .map(|installation| format!("\n        \"installation\": \"{installation}\""))
            .unwrap_or_default();
        let separator = if install_policy.is_some() && auth_policy.is_some() {
            ","
        } else {
            ""
        };
        let authentication = auth_policy
            .map(|authentication| {
                format!("{separator}\n        \"authentication\": \"{authentication}\"")
            })
            .unwrap_or_default();
        format!(",\n      \"policy\": {{{installation}{authentication}\n      }}")
    } else {
        String::new()
    };
    std::fs::create_dir_all(repo_root.join(".git"))?;
    std::fs::create_dir_all(repo_root.join(".agents/plugins"))?;
    std::fs::write(
        repo_root.join(".agents/plugins/marketplace.json"),
        format!(
            r#"{{
  "name": "{marketplace_name}",
  "plugins": [
    {{
      "name": "{plugin_name}",
      "source": {{
        "source": "local",
        "path": "{source_path}"
      }}{policy}
    }}
  ]
}}"#
        ),
    )
}

fn write_plugin_source(repo_root: &std::path::Path, plugin_name: &str) -> Result<()> {
    let plugin_root = repo_root.join(plugin_name);
    std::fs::create_dir_all(plugin_root.join(".codex-plugin"))?;
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        format!(r#"{{"name":"{plugin_name}"}}"#),
    )?;

    Ok(())
}

fn remote_plugin_bundle_tar_gz_bytes(plugin_name: &str) -> Result<Vec<u8>> {
    let manifest = format!(r#"{{"name":"{plugin_name}"}}"#);
    remote_plugin_bundle_tar_gz_bytes_with_contents(&manifest)
}

fn remote_plugin_bundle_tar_gz_bytes_with_contents(plugin_manifest: &str) -> Result<Vec<u8>> {
    let skill = "---\nname: plan-work\ndescription: Track work in Linear.\n---\n\n# Plan Work\n";
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut tar = tar::Builder::new(encoder);
    let entries = vec![
        (
            ".codex-plugin/plugin.json",
            plugin_manifest.as_bytes(),
            /*mode*/ 0o644,
        ),
        (
            "skills/plan-work/SKILL.md",
            skill.as_bytes(),
            /*mode*/ 0o644,
        ),
    ];
    for (path, contents, mode) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        tar.append_data(&mut header, path, contents)?;
    }
    Ok(tar.into_inner()?.finish()?)
}
