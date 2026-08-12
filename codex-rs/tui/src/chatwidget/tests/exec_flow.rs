use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn preamble_keeps_working_status_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());

    // Regression sequence: a preamble line is committed to history before any exec/tool event.
    // After commentary completes, the status row should be restored before subsequent work.
    chat.on_task_started();
    chat.on_agent_message_delta("Preamble line\n".to_string());
    chat.on_commit_tick();
    drain_insert_history(&mut rx);
    complete_assistant_message(
        &mut chat,
        "msg-commentary-snapshot",
        "Preamble line\n",
        Some(MessagePhase::Commentary),
    );

    let height = chat.desired_height(/*width*/ 80);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, height))
        .expect("create terminal");
    terminal
        .draw(|f| chat.render(f.area(), f.buffer_mut()))
        .expect("draw preamble + status widget");
    assert_chatwidget_snapshot!(
        "preamble_keeps_working_status",
        normalized_backend_snapshot(terminal.backend())
    );
}

#[tokio::test]
async fn unified_exec_begin_restores_status_indicator_after_preamble() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.on_task_started();
    assert_eq!(chat.bottom_pane.status_indicator_visible(), true);

    // Simulate a hidden status row during an active turn.
    chat.bottom_pane.hide_status_indicator();
    assert_eq!(chat.bottom_pane.status_indicator_visible(), false);
    assert_eq!(chat.bottom_pane.is_task_running(), true);

    begin_unified_exec_startup(&mut chat, "call-1", "proc-1", "sleep 2");

    assert_eq!(chat.bottom_pane.status_indicator_visible(), true);
}

#[tokio::test]
async fn unified_exec_begin_restores_working_status_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.on_task_started();
    chat.on_agent_message_delta("Preamble line\n".to_string());
    chat.on_commit_tick();
    drain_insert_history(&mut rx);

    begin_unified_exec_startup(&mut chat, "call-1", "proc-1", "sleep 2");

    let width: u16 = 80;
    let height = chat.desired_height(width);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("create terminal");
    terminal
        .draw(|f| chat.render(f.area(), f.buffer_mut()))
        .expect("draw chatwidget");
    assert_chatwidget_snapshot!(
        "unified_exec_begin_restores_working_status",
        normalized_backend_snapshot(terminal.backend())
    );
}

#[tokio::test]
async fn exec_history_cell_shows_working_then_completed() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    // Begin command
    let begin = begin_exec(&mut chat, "call-1", "echo done");

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 0, "no exec cell should have been flushed yet");

    // End command successfully
    end_exec(&mut chat, begin, "done", "", /*exit_code*/ 0);

    let cells = drain_insert_history(&mut rx);
    // Exec end now finalizes and flushes the exec cell immediately.
    assert_eq!(cells.len(), 1, "expected finalized exec cell to flush");
    // Inspect the flushed exec cell rendering.
    let lines = &cells[0];
    let blob = lines_to_single_string(lines);
    // New behavior: no glyph markers; ensure command is shown and no panic.
    assert!(
        blob.contains("• Ran"),
        "expected summary header present: {blob:?}"
    );
    assert!(
        blob.contains("echo done"),
        "expected command text to be present: {blob:?}"
    );
}

#[tokio::test]
async fn exec_history_cell_shows_working_then_failed() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    // Begin command
    let begin = begin_exec(&mut chat, "call-2", "false");
    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 0, "no exec cell should have been flushed yet");

    // End command with failure
    end_exec(&mut chat, begin, "", "Bloop", /*exit_code*/ 2);

    let cells = drain_insert_history(&mut rx);
    // Exec end with failure should also flush immediately.
    assert_eq!(cells.len(), 1, "expected finalized exec cell to flush");
    let lines = &cells[0];
    let blob = lines_to_single_string(lines);
    assert!(
        blob.contains("• Ran false"),
        "expected command and header text present: {blob:?}"
    );
    assert!(blob.to_lowercase().contains("bloop"), "expected error text");
}

#[tokio::test]
async fn exec_end_without_begin_uses_event_command() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let command = vec![
        "bash".to_string(),
        "-lc".to_string(),
        "echo orphaned".to_string(),
    ];
    let command_actions = codex_shell_command::parse_command::parse_command(&command)
        .into_iter()
        .map(|parsed| CliRuntimeCommandAction::from_core_with_cwd(parsed, &chat.config.cwd))
        .collect();
    let cwd = chat.config.cwd.clone();
    handle_exec_end(
        &mut chat,
        CliRuntimeThreadItem::CommandExecution {
            id: "call-orphan".to_string(),
            command: codex_shell_command::parse_command::shlex_join(&command),
            cwd: cwd.into(),
            process_id: None,
            source: ExecCommandSource::Agent,
            status: CliRuntimeCommandExecutionStatus::Completed,
            command_actions,
            aggregated_output: Some("done".to_string()),
            exit_code: Some(0),
            duration_ms: Some(5),
        },
    );

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1, "expected finalized exec cell to flush");
    let blob = lines_to_single_string(&cells[0]);
    assert!(
        blob.contains("• Ran echo orphaned"),
        "expected command text to come from event: {blob:?}"
    );
    assert!(
        !blob.contains("call-orphan"),
        "call id should not be rendered when event has the command: {blob:?}"
    );
}

#[tokio::test]
async fn exec_end_without_begin_does_not_flush_unrelated_running_exploring_cell() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    begin_exec(&mut chat, "call-exploring", "cat /dev/null");
    assert!(drain_insert_history(&mut rx).is_empty());
    assert!(active_blob(&chat).contains("Read null"));

    let orphan =
        begin_unified_exec_startup(&mut chat, "call-orphan", "proc-1", "echo repro-marker");
    assert!(drain_insert_history(&mut rx).is_empty());

    end_exec(
        &mut chat,
        orphan,
        "repro-marker\n",
        "",
        /*exit_code*/ 0,
    );

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1, "only the orphan end should be inserted");
    let orphan_blob = lines_to_single_string(&cells[0]);
    assert!(
        orphan_blob.contains("• Ran echo repro-marker"),
        "expected orphan end to render a standalone entry: {orphan_blob:?}"
    );
    let active = active_blob(&chat);
    assert!(
        active.contains("• Exploring"),
        "expected unrelated exploring call to remain active: {active:?}"
    );
    assert!(
        active.contains("Read null"),
        "expected active exploring command to remain visible: {active:?}"
    );
    assert!(
        !active.contains("echo repro-marker"),
        "orphaned end should not replace the active exploring cell: {active:?}"
    );
}

#[tokio::test]
async fn exec_end_without_begin_flushes_completed_unrelated_exploring_cell() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let begin_ls = begin_exec(&mut chat, "call-ls", "ls -la");
    end_exec(&mut chat, begin_ls, "", "", /*exit_code*/ 0);
    assert!(drain_insert_history(&mut rx).is_empty());
    assert!(active_blob(&chat).contains("ls -la"));

    let orphan = begin_unified_exec_startup(&mut chat, "call-after", "proc-1", "echo after");
    end_exec(&mut chat, orphan, "after\n", "", /*exit_code*/ 0);

    let cells = drain_insert_history(&mut rx);
    assert_eq!(
        cells.len(),
        2,
        "completed exploring cell should flush before the orphan entry"
    );
    let first = lines_to_single_string(&cells[0]);
    let second = lines_to_single_string(&cells[1]);
    assert!(
        first.contains("• Explored"),
        "expected flushed exploring cell: {first:?}"
    );
    assert!(
        first.contains("List ls -la"),
        "expected flushed exploring cell: {first:?}"
    );
    assert!(
        second.contains("• Ran echo after"),
        "expected orphan end entry after flush: {second:?}"
    );
    assert!(
        chat.transcript.active_cell.is_none(),
        "both entries should be finalized"
    );
}

#[tokio::test]
async fn overlapping_exploring_exec_end_is_not_misclassified_as_orphan() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    let begin_ls = begin_exec(&mut chat, "call-ls", "ls -la");
    let begin_cat = begin_exec(&mut chat, "call-cat", "cat foo.txt");
    assert!(drain_insert_history(&mut rx).is_empty());

    end_exec(&mut chat, begin_ls, "foo.txt\n", "", /*exit_code*/ 0);

    let cells = drain_insert_history(&mut rx);
    assert!(
        cells.is_empty(),
        "tracked end inside an exploring cell should not render as an orphan"
    );
    let active = active_blob(&chat);
    assert!(
        active.contains("List ls -la"),
        "expected first command still grouped: {active:?}"
    );
    assert!(
        active.contains("Read foo.txt"),
        "expected second running command to stay in the same active cell: {active:?}"
    );
    assert!(
        active.contains("• Exploring"),
        "expected grouped exploring header to remain active: {active:?}"
    );

    end_exec(&mut chat, begin_cat, "hello\n", "", /*exit_code*/ 0);
}

#[tokio::test]
async fn exec_history_shows_unified_exec_startup_commands() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let begin = begin_exec_with_source(
        &mut chat,
        "call-startup",
        "echo unified exec startup",
        ExecCommandSource::UnifiedExecStartup,
    );
    assert!(
        drain_insert_history(&mut rx).is_empty(),
        "exec begin should not flush until completion"
    );

    end_exec(
        &mut chat,
        begin,
        "echo unified exec startup\n",
        "",
        /*exit_code*/ 0,
    );

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1, "expected finalized exec cell to flush");
    let blob = lines_to_single_string(&cells[0]);
    assert!(
        blob.contains("• Ran echo unified exec startup"),
        "expected startup command to render: {blob:?}"
    );
}

#[tokio::test]
async fn exec_history_shows_unified_exec_tool_calls() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let begin = begin_exec_with_source(
        &mut chat,
        "call-startup",
        "ls",
        ExecCommandSource::UnifiedExecStartup,
    );
    end_exec(&mut chat, begin, "", "", /*exit_code*/ 0);

    let blob = active_blob(&chat);
    assert_eq!(blob, "• Explored\n  └ List ls\n");
}

#[tokio::test]
async fn unified_exec_unknown_end_with_active_exploring_cell_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    begin_exec(&mut chat, "call-exploring", "cat /dev/null");
    let orphan =
        begin_unified_exec_startup(&mut chat, "call-orphan", "proc-1", "echo repro-marker");
    end_exec(
        &mut chat,
        orphan,
        "repro-marker\n",
        "",
        /*exit_code*/ 0,
    );

    let cells = drain_insert_history(&mut rx);
    let history = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    let active = active_blob(&chat);
    let snapshot = format!("History:\n{history}\nActive:\n{active}");
    assert_chatwidget_snapshot!(
        "unified_exec_unknown_end_with_active_exploring_cell",
        snapshot
    );
}

#[tokio::test]
async fn unified_exec_end_after_task_complete_is_suppressed() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let begin = begin_exec_with_source(
        &mut chat,
        "call-startup",
        "echo unified exec startup",
        ExecCommandSource::UnifiedExecStartup,
    );
    drain_insert_history(&mut rx);

    chat.on_task_complete(
        /*last_agent_message*/ None, /*duration_ms*/ None, /*from_replay*/ false,
    );
    let completed_turn = drain_insert_history(&mut rx);
    assert_eq!(completed_turn.len(), 1);
    assert!(lines_to_single_string(&completed_turn[0]).contains("▣ Build · gpt-5.6-sol · 0s"));
    end_exec(&mut chat, begin, "", "", /*exit_code*/ 0);

    let cells = drain_insert_history(&mut rx);
    assert!(
        cells.is_empty(),
        "expected unified exec end after task complete to be suppressed"
    );
}

#[tokio::test]
async fn unified_exec_interaction_after_task_complete_is_suppressed() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    chat.on_task_complete(
        /*last_agent_message*/ None, /*duration_ms*/ None, /*from_replay*/ false,
    );
    let completed_turn = drain_insert_history(&mut rx);
    assert_eq!(completed_turn.len(), 1);
    assert!(lines_to_single_string(&completed_turn[0]).contains("▣ Build · gpt-5.6-sol · 0s"));

    terminal_interaction(&mut chat, "call-1", "proc-1", "ls\n");

    let cells = drain_insert_history(&mut rx);
    assert!(
        cells.is_empty(),
        "expected unified exec interaction after task complete to be suppressed"
    );
}

#[tokio::test]
async fn unified_exec_wait_after_final_agent_message_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");

    begin_unified_exec_startup(&mut chat, "call-wait", "proc-1", "cargo test -p codex-core");
    terminal_interaction(&mut chat, "call-wait-stdin", "proc-1", "");

    complete_assistant_message(&mut chat, "msg-1", "Final response.", /*phase*/ None);
    handle_turn_completed(&mut chat, "turn-1", /*duration_ms*/ None);

    let cells = drain_insert_history(&mut rx);
    let combined = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    assert_chatwidget_snapshot!("unified_exec_wait_after_final_agent_message", combined);
}

#[tokio::test]
async fn unified_exec_wait_before_streamed_agent_message_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");

    begin_unified_exec_startup(
        &mut chat,
        "call-wait-stream",
        "proc-1",
        "cargo test -p codex-core",
    );
    terminal_interaction(&mut chat, "call-wait-stream-stdin", "proc-1", "");

    handle_agent_message_delta(&mut chat, "Streaming response.");
    handle_turn_completed(&mut chat, "turn-wait-1", /*duration_ms*/ None);

    let cells = drain_insert_history(&mut rx);
    let combined = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    assert_chatwidget_snapshot!("unified_exec_wait_before_streamed_agent_message", combined);
}

#[tokio::test]
async fn final_worked_for_uses_cumulative_turn_duration_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");

    let exec = begin_exec_with_source(
        &mut chat,
        "call-1",
        "echo preparing",
        ExecCommandSource::Agent,
    );
    end_exec(&mut chat, exec, "preparing\n", "", /*exit_code*/ 0);

    complete_assistant_message(
        &mut chat,
        "msg-final",
        "Final response.",
        Some(MessagePhase::FinalAnswer),
    );
    handle_turn_completed(&mut chat, "turn-1", Some(125_000));

    let cells = drain_insert_history(&mut rx);
    let combined = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    assert!(
        combined.contains("▣ Build · gpt-5.6-sol · 2m 05s"),
        "expected Mini turn summary to use cumulative turn duration, got:\n{combined}"
    );
    assert_chatwidget_snapshot!("final_worked_for_uses_cumulative_turn_duration", combined);
}

#[tokio::test]
async fn unified_exec_wait_status_header_updates_on_late_command_display() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    chat.unified_exec_processes.push(UnifiedExecProcessSummary {
        key: "proc-1".to_string(),
        call_id: "call-1".to_string(),
        command_display: "sleep 5".to_string(),
        recent_chunks: Vec::new(),
    });

    terminal_interaction(&mut chat, "call-1", "proc-1", "");

    assert!(chat.transcript.active_cell.is_none());
    assert_eq!(
        chat.status_state.current_status.header,
        "Waiting for background terminal"
    );
    let status = chat
        .bottom_pane
        .status_widget()
        .expect("status indicator should be visible");
    assert_eq!(status.header(), "Waiting for background terminal");
    assert_eq!(status.details(), Some("sleep 5"));
}

#[tokio::test]
async fn unified_exec_empty_poll_for_finished_process_does_not_show_waiting_status() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    terminal_interaction(&mut chat, "call-finished", "proc-finished", "");

    assert_eq!(chat.status_state.current_status.header, "Working");
    let status = chat
        .bottom_pane
        .status_widget()
        .expect("task status indicator should remain visible");
    assert_eq!(status.header(), "Working");
    assert!(chat.unified_exec_wait_streak.is_none());
}

#[tokio::test]
async fn unified_exec_waiting_multiple_empty_snapshots() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    begin_unified_exec_startup(&mut chat, "call-wait-1", "proc-1", "just fix");

    terminal_interaction(&mut chat, "call-wait-1a", "proc-1", "");
    terminal_interaction(&mut chat, "call-wait-1b", "proc-1", "");
    assert_eq!(
        chat.status_state.current_status.header,
        "Waiting for background terminal"
    );
    let status = chat
        .bottom_pane
        .status_widget()
        .expect("status indicator should be visible");
    assert_eq!(status.header(), "Waiting for background terminal");
    assert_eq!(status.details(), Some("just fix"));

    handle_turn_completed(&mut chat, "turn-wait-3", /*duration_ms*/ None);

    let cells = drain_insert_history(&mut rx);
    let combined = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    assert_chatwidget_snapshot!("unified_exec_waiting_multiple_empty_after", combined);
}

#[tokio::test]
async fn unified_exec_wait_status_renders_command_in_single_details_row_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    begin_unified_exec_startup(
        &mut chat,
        "call-wait-ui",
        "proc-ui",
        "cargo test -p codex-core -- --exact some::very::long::test::name",
    );

    terminal_interaction(&mut chat, "call-wait-ui-stdin", "proc-ui", "");

    let rendered = render_bottom_popup(&chat, /*width*/ 48);
    assert_chatwidget_snapshot!(
        "unified_exec_wait_status_renders_command_in_single_details_row",
        normalize_snapshot_paths(rendered)
    );
}

#[tokio::test]
async fn unified_exec_empty_then_non_empty_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    begin_unified_exec_startup(&mut chat, "call-wait-2", "proc-2", "just fix");

    terminal_interaction(&mut chat, "call-wait-2a", "proc-2", "");
    terminal_interaction(&mut chat, "call-wait-2b", "proc-2", "ls\n");

    let cells = drain_insert_history(&mut rx);
    let combined = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    assert_chatwidget_snapshot!("unified_exec_empty_then_non_empty_after", combined);
}

#[tokio::test]
async fn unified_exec_non_empty_then_empty_snapshots() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    begin_unified_exec_startup(&mut chat, "call-wait-3", "proc-3", "just fix");

    terminal_interaction(&mut chat, "call-wait-3a", "proc-3", "pwd\n");
    terminal_interaction(&mut chat, "call-wait-3b", "proc-3", "");
    assert_eq!(
        chat.status_state.current_status.header,
        "Waiting for background terminal"
    );
    let status = chat
        .bottom_pane
        .status_widget()
        .expect("status indicator should be visible");
    assert_eq!(status.header(), "Waiting for background terminal");
    assert_eq!(status.details(), Some("just fix"));
    let pre_cells = drain_insert_history(&mut rx);
    let active_combined = pre_cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    assert_chatwidget_snapshot!("unified_exec_non_empty_then_empty_active", active_combined);

    handle_turn_completed(&mut chat, "turn-1", /*duration_ms*/ None);

    let post_cells = drain_insert_history(&mut rx);
    let mut combined = pre_cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    let post = post_cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    if !combined.is_empty() && !post.is_empty() {
        combined.push('\n');
    }
    combined.push_str(&post);
    assert_chatwidget_snapshot!("unified_exec_non_empty_then_empty_after", combined);
}

#[tokio::test]
async fn view_image_tool_call_adds_history_cell() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let image_path = chat.config.cwd.join("example.png");

    handle_view_image_tool_call(&mut chat, "call-image", image_path);

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1, "expected a single history cell");
    let combined = lines_to_single_string(&cells[0]);
    assert_chatwidget_snapshot!("local_image_attachment_history_snapshot", combined);
}

#[tokio::test]
async fn view_image_tool_call_preserves_foreign_path() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let image_path: LegacyAppPathString =
        serde_json::from_value(json!(r"C:\workspace\assets\example.png"))
            .expect("valid legacy app path string");

    handle_view_image_tool_call(&mut chat, "call-image", image_path);

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1, "expected a single history cell");
    let combined = lines_to_single_string(&cells[0]);
    assert_chatwidget_snapshot!("foreign_image_attachment_history_snapshot", combined);
}

#[tokio::test]
async fn image_generation_begin_restores_working_status_after_single_line_preamble() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.on_task_started();
    chat.on_agent_message_delta("Generating an image.".to_string());
    chat.on_image_generation_begin();

    assert!(chat.bottom_pane.is_task_running());
    assert!(chat.bottom_pane.status_indicator_visible());

    let width: u16 = 80;
    let height = chat.desired_height(width);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("create terminal");
    terminal
        .draw(|frame| chat.render(frame.area(), frame.buffer_mut()))
        .expect("draw image generation status");
    assert_chatwidget_snapshot!(
        "image_generation_begin_restores_working_status",
        normalized_backend_snapshot(terminal.backend())
    );
}

#[tokio::test]
async fn image_generation_call_adds_history_cell() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    handle_image_generation_end(
        &mut chat,
        "call-image-generation",
        "completed",
        Some("A tiny blue square".into()),
        Some(test_path_buf("/tmp/ig-1.png").abs()),
    );

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1, "expected a single history cell");
    let platform_file_url = url::Url::from_file_path(test_path_buf("/tmp/ig-1.png"))
        .expect("test path should convert to file URL")
        .to_string();
    let combined =
        lines_to_single_string(&cells[0]).replace(&platform_file_url, "file:///tmp/ig-1.png");
    assert_chatwidget_snapshot!("image_generation_call_history_snapshot", combined);

    handle_image_generation_end(
        &mut chat,
        "call-image-generation-failed",
        "failed",
        Some("A tiny blue square".into()),
        /*saved_path*/ None,
    );

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1, "expected a single failure history cell");
    assert_chatwidget_snapshot!(
        "failed_image_generation_call_history_snapshot",
        lines_to_single_string(&cells[0])
    );
}

#[tokio::test]
async fn exec_history_extends_previous_when_consecutive() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    // 1) Start "ls -la" (List)
    let begin_ls = begin_exec(&mut chat, "call-ls", "ls -la");
    assert_chatwidget_snapshot!("exploring_step1_start_ls", active_blob(&chat));

    // 2) Finish "ls -la"
    end_exec(&mut chat, begin_ls, "", "", /*exit_code*/ 0);
    assert_chatwidget_snapshot!("exploring_step2_finish_ls", active_blob(&chat));

    // 3) Start "cat foo.txt" (Read)
    let begin_cat_foo = begin_exec(&mut chat, "call-cat-foo", "cat foo.txt");
    assert_chatwidget_snapshot!("exploring_step3_start_cat_foo", active_blob(&chat));

    // 4) Complete "cat foo.txt"
    end_exec(
        &mut chat,
        begin_cat_foo,
        "hello from foo",
        "",
        /*exit_code*/ 0,
    );
    assert_chatwidget_snapshot!("exploring_step4_finish_cat_foo", active_blob(&chat));

    // 5) Start & complete "sed -n 100,200p foo.txt" (treated as Read of foo.txt)
    let begin_sed_range = begin_exec(&mut chat, "call-sed-range", "sed -n 100,200p foo.txt");
    end_exec(
        &mut chat,
        begin_sed_range,
        "chunk",
        "",
        /*exit_code*/ 0,
    );
    assert_chatwidget_snapshot!("exploring_step5_finish_sed_range", active_blob(&chat));

    // 6) Start & complete "cat bar.txt"
    let begin_cat_bar = begin_exec(&mut chat, "call-cat-bar", "cat bar.txt");
    end_exec(
        &mut chat,
        begin_cat_bar,
        "hello from bar",
        "",
        /*exit_code*/ 0,
    );
    assert_chatwidget_snapshot!("exploring_step6_finish_cat_bar", active_blob(&chat));
}

#[tokio::test]
async fn user_shell_command_renders_output_not_exploring() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    let begin_ls = begin_exec_with_source(
        &mut chat,
        "user-shell-ls",
        "ls",
        ExecCommandSource::UserShell,
    );
    end_exec(
        &mut chat,
        begin_ls,
        "file1\nfile2\n",
        "",
        /*exit_code*/ 0,
    );

    let cells = drain_insert_history(&mut rx);
    assert_eq!(
        cells.len(),
        1,
        "expected a single history cell for the user command"
    );
    let blob = lines_to_single_string(cells.first().unwrap());
    assert_chatwidget_snapshot!("user_shell_ls_output", blob);
}

#[tokio::test]
async fn bang_shell_enter_while_task_running_submits_run_user_shell_command() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    let rollout_file = NamedTempFile::new().unwrap();
    let configured = crate::session_state::ThreadSessionState {
        thread_id,
        forked_from_id: None,
        fork_parent_title: None,
        thread_name: None,
        model: "test-model".to_string(),
        model_provider_id: "test-provider".to_string(),
        service_tier: None,
        cwd: test_path_buf("/home/user/project").abs(),
        runtime_workspace_roots: Vec::new(),
        instruction_source_paths: Vec::new(),
        reasoning_effort: Some(ReasoningEffortConfig::default()),
        agent_settings: None,
        personality: None,
        message_history: None,
        rollout_path: Some(rollout_file.path().to_path_buf()),
    };
    chat.handle_thread_session(configured);
    drain_insert_history(&mut rx);
    while op_rx.try_recv().is_ok() {}
    handle_turn_started(&mut chat, "turn-1");

    chat.bottom_pane
        .set_composer_text("!echo hi".to_string(), Vec::new(), Vec::new());
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    match op_rx.try_recv() {
        Ok(Op::RunUserShellCommand { command }) => assert_eq!(command, "echo hi"),
        other => panic!("expected RunUserShellCommand op, got {other:?}"),
    }
    assert_matches!(
        rx.try_recv(),
        Ok(AppEvent::AppendMessageHistoryEntry { text, .. }) if text == "!echo hi"
    );
    assert_matches!(rx.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn user_message_during_user_shell_command_is_queued_not_steered() {
    let (mut chat, _rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    handle_turn_started(&mut chat, "turn-1");
    let begin = begin_exec_with_source(
        &mut chat,
        "user-shell-sleep",
        "sleep 10",
        ExecCommandSource::UserShell,
    );

    assert!(chat.only_user_shell_commands_running());
    chat.bottom_pane
        .set_composer_text("hi".to_string(), Vec::new(), Vec::new());
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_matches!(op_rx.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(chat.queued_user_message_texts(), vec!["hi".to_string()]);

    end_exec(&mut chat, begin, "", "", /*exit_code*/ 0);
    complete_assistant_message(
        &mut chat,
        "msg-done",
        "done",
        Some(MessagePhase::FinalAnswer),
    );
    handle_turn_completed(&mut chat, "turn-1", /*duration_ms*/ None);

    match next_submit_op(&mut op_rx) {
        Op::UserTurn { items, .. } => assert_eq!(
            items,
            vec![UserInput::Text {
                text: "hi".to_string(),
                text_elements: Vec::new(),
            }]
        ),
        other => panic!("expected queued user message after shell completion, got {other:?}"),
    }
    assert!(chat.input_queue.queued_user_messages.is_empty());
}

#[tokio::test]
async fn disabled_slash_command_while_task_running_snapshot() {
    // Build a chat widget and simulate an active task
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    handle_turn_started(&mut chat, "turn-1");

    // Resume remains available while idle, but not while an agent turn is active.
    chat.dispatch_command(SlashCommand::Resume);

    // Drain history and snapshot the rendered error line(s)
    let cells = drain_insert_history(&mut rx);
    assert!(
        !cells.is_empty(),
        "expected an error message history cell to be emitted",
    );
    let blob = lines_to_single_string(cells.last().unwrap());
    assert_chatwidget_snapshot!("disabled_slash_command_while_task_running_snapshot", blob);
}

#[tokio::test]
async fn interrupt_preserves_unified_exec_processes() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    begin_unified_exec_startup(&mut chat, "call-1", "process-1", "sleep 5");
    begin_unified_exec_startup(&mut chat, "call-2", "process-2", "sleep 6");
    assert_eq!(chat.unified_exec_processes.len(), 2);

    handle_turn_interrupted(&mut chat, "turn-1");

    assert_eq!(chat.unified_exec_processes.len(), 2);

    chat.add_ps_output();
    let cells = drain_insert_history(&mut rx);
    let combined = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        combined.contains("Background terminals"),
        "expected /ps to remain available after interrupt; got {combined:?}"
    );
    assert!(
        combined.contains("sleep 5") && combined.contains("sleep 6"),
        "expected /ps to list running unified exec processes; got {combined:?}"
    );

    let _ = drain_insert_history(&mut rx);
}

#[tokio::test]
async fn interrupt_preserves_unified_exec_wait_streak_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    handle_turn_started(&mut chat, "turn-1");

    let begin = begin_unified_exec_startup(&mut chat, "call-1", "process-1", "just fix");
    terminal_interaction(&mut chat, "call-1a", "process-1", "");

    handle_turn_interrupted(&mut chat, "turn-1");

    end_exec(&mut chat, begin, "", "", /*exit_code*/ 0);
    let cells = drain_insert_history(&mut rx);
    let combined = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<Vec<_>>()
        .join("\n");
    let snapshot = format!("cells={}\n{combined}", cells.len());
    assert_chatwidget_snapshot!("interrupt_preserves_unified_exec_wait_streak", snapshot);
}

#[tokio::test]
async fn turn_complete_keeps_unified_exec_processes() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    begin_unified_exec_startup(&mut chat, "call-1", "process-1", "sleep 5");
    begin_unified_exec_startup(&mut chat, "call-2", "process-2", "sleep 6");
    assert_eq!(chat.unified_exec_processes.len(), 2);

    handle_turn_completed(&mut chat, "turn-1", /*duration_ms*/ None);

    assert_eq!(chat.unified_exec_processes.len(), 2);

    chat.add_ps_output();
    let cells = drain_insert_history(&mut rx);
    let combined = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        combined.contains("Background terminals"),
        "expected /ps to remain available after turn complete; got {combined:?}"
    );
    assert!(
        combined.contains("sleep 5") && combined.contains("sleep 6"),
        "expected /ps to list running unified exec processes; got {combined:?}"
    );

    let _ = drain_insert_history(&mut rx);
}

#[tokio::test]
async fn apply_patch_events_emit_history_cells() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    // Begin apply -> per-file apply block cell (no global header)
    let mut changes2 = HashMap::new();
    changes2.insert(
        PathBuf::from("foo.txt"),
        FileChange::Add {
            content: "hello\n".to_string(),
        },
    );
    handle_patch_apply_begin(&mut chat, "c1", "turn-c1", changes2);
    let cells = drain_insert_history(&mut rx);
    assert!(!cells.is_empty(), "expected apply block cell to be sent");
    let blob = lines_to_single_string(cells.last().unwrap());
    assert!(
        blob.contains("Added foo.txt") || blob.contains("Edited foo.txt"),
        "expected single-file header with filename (Added/Edited): {blob:?}"
    );

    // End apply success -> success cell
    let mut end_changes = HashMap::new();
    end_changes.insert(
        PathBuf::from("foo.txt"),
        FileChange::Add {
            content: "hello\n".to_string(),
        },
    );
    handle_patch_apply_end(
        &mut chat,
        "c1",
        "turn-c1",
        end_changes,
        CliRuntimePatchApplyStatus::Completed,
    );
    let cells = drain_insert_history(&mut rx);
    assert!(
        cells.is_empty(),
        "no success cell should be emitted anymore"
    );
}
