use super::*;
use app_test_support::create_fake_rollout;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn fork_current_session_preserves_conversation_ultra() -> Result<()> {
    let mut app = make_test_app().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = codex_state::SqliteConfig::new_for_testing(codex_home.path().abs());
    let source_thread_id = ThreadId::from_string(
        &create_fake_rollout(
            codex_home.path(),
            "2025-01-05T12-00-00",
            "2025-01-05T12:00:00Z",
            "Saved user message",
            Some(app.config.model_provider_id.as_str()),
            /*git_info*/ None,
        )
        .expect("create source rollout"),
    )?;
    app.chat_widget.handle_thread_session(ThreadSessionState {
        model: "gpt-5.4".to_string(),
        reasoning_effort: Some(ReasoningEffortConfig::Ultra),
        ..test_thread_session(source_thread_id, test_path_buf("/tmp/project"))
    });
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut cli_runtime = crate::start_embedded_cli_runtime_for_picker(&app.config).await?;

    let control = Box::pin(app.handle_event(
        &mut tui,
        &mut cli_runtime,
        AppEvent::ForkCurrentSession { name: None },
    ))
    .await?;

    assert!(matches!(control, AppRunControl::Continue));
    assert_ne!(app.chat_widget.thread_id(), Some(source_thread_id));
    assert_eq!(app.chat_widget.current_model(), "gpt-5.4");
    assert_eq!(
        app.chat_widget.current_reasoning_effort(),
        Some(ReasoningEffortConfig::Ultra)
    );
    cli_runtime.shutdown().await?;
    Ok(())
}
