use std::time::Duration;

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_fake_parented_rollout_with_source;
use app_test_support::create_final_assistant_message_sse_response;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use core_test_support::responses;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Guardian reviewer turns respect a disabled viewer while retaining execution tools.
#[tokio::test]
async fn guardian_reviewer_inherits_disabled_view_image() -> Result<()> {
    let responses_server = responses::start_mock_server().await;
    let guardian_request = responses::mount_sse_once(
        &responses_server,
        create_final_assistant_message_sse_response("review complete")?,
    )
    .await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&responses_server.uri())
        .with_provider_config("supports_websockets = false")
        .write(codex_home.path())?;

    let guardian_thread_id = create_fake_parented_rollout_with_source(
        codex_home.path(),
        "2025-01-05T12-00-00",
        "2025-01-05T12:00:00Z",
        "review a requested action",
        Some("mock_provider"),
        /*git_info*/ None,
        SessionSource::SubAgent(SubAgentSource::Other("guardian".to_string())),
        ThreadId::new().into(),
        ThreadId::new(),
    )?;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: guardian_thread_id,
            config: Some(
                [("features.view_image".to_string(), json!(false))]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        })
        .await?;
    let ThreadResumeResponse { thread, .. } =
        timeout(DEFAULT_READ_TIMEOUT, app_server.read_response(resume_id)).await??;
    let environment = app_server.auto_env_params()?;

    let _: TurnStartResponse = app_server
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id,
                input: vec![UserInput::Text {
                    text: "review the requested action".to_string(),
                    text_elements: Vec::new(),
                }],
                environments: Some(vec![environment]),
                ..Default::default()
            },
        })
        .await?;
    let _: TurnCompletedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_notification("turn/completed"),
    )
    .await??;

    let request = guardian_request.single_request().body_json();
    let tools = request["tools"].as_array().expect("model-visible tools");
    for tool in ["exec_command", "write_stdin"] {
        assert!(
            tools.iter().any(|spec| spec["name"] == tool),
            "guardian reviewer must retain {tool}"
        );
    }
    assert!(
        tools.iter().all(|spec| spec["name"] != "view_image"),
        "guardian reviewer must not receive the disabled image viewer"
    );

    Ok(())
}
