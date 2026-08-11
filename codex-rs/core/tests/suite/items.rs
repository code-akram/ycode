#![cfg(not(target_os = "windows"))]

use anyhow::Ok;
use codex_features::Feature;
use codex_protocol::config_types::AgentSettings;
use codex_protocol::config_types::Settings;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::WebSearchAction;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::ByteRange;
use codex_protocol::user_input::TextElement;
use codex_protocol::user_input::UserInput;
use core_test_support::PathBufExt;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_message_item_added;
use core_test_support::responses::ev_output_text_delta;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_reasoning_item_added;
use core_test_support::responses::ev_reasoning_summary_text_delta;
use core_test_support::responses::ev_reasoning_text_delta;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::ev_web_search_call_added_partial;
use core_test_support::responses::ev_web_search_call_done;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_message_item_is_emitted() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let TestCodex { codex, .. } = test_codex().build(&server).await?;

    let first_response = sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]);
    mount_sse_once(&server, first_response).await;

    let text_elements = vec![TextElement::new(
        ByteRange { start: 0, end: 6 },
        Some("<file>".into()),
    )];
    let expected_input = UserInput::Text {
        text: "please inspect sample.txt".into(),
        text_elements: text_elements.clone(),
    };

    codex
        .submit(Op::UserInput {
            items: vec![expected_input.clone()],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let started_item = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ItemStarted(ItemStartedEvent {
            item: TurnItem::UserMessage(item),
            ..
        }) => Some(item.clone()),
        _ => None,
    })
    .await;
    let completed_item = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ItemCompleted(ItemCompletedEvent {
            item: TurnItem::UserMessage(item),
            ..
        }) => Some(item.clone()),
        _ => None,
    })
    .await;

    assert_eq!(started_item.id, completed_item.id);
    assert_eq!(started_item.content, vec![expected_input.clone()]);
    assert_eq!(completed_item.content, vec![expected_input]);

    let legacy_message = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::UserMessage(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    assert_eq!(legacy_message.message, "please inspect sample.txt");
    assert_eq!(legacy_message.text_elements, text_elements);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assistant_message_item_is_emitted() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let TestCodex { codex, .. } = test_codex().build(&server).await?;

    let first_response = sse(vec![
        ev_response_created("resp-1"),
        ev_assistant_message("msg-1", "all done"),
        ev_completed("resp-1"),
    ]);
    mount_sse_once(&server, first_response).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "please summarize results".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let started = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ItemStarted(ItemStartedEvent {
            item: TurnItem::AgentMessage(item),
            ..
        }) => Some(item.clone()),
        _ => None,
    })
    .await;
    let completed = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ItemCompleted(ItemCompletedEvent {
            item: TurnItem::AgentMessage(item),
            ..
        }) => Some(item.clone()),
        _ => None,
    })
    .await;

    assert_eq!(started.id, completed.id);
    let Some(codex_protocol::items::AgentMessageContent::Text { text }) = completed.content.first()
    else {
        panic!("expected agent message text content");
    };
    assert_eq!(text, "all done");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_item_is_emitted() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let TestCodex { codex, .. } = test_codex().build(&server).await?;

    let reasoning_item = ev_reasoning_item(
        "reasoning-1",
        &["Consider inputs", "Compute output"],
        &["Detailed reasoning trace"],
    );

    let first_response = sse(vec![
        ev_response_created("resp-1"),
        reasoning_item,
        ev_completed("resp-1"),
    ]);
    mount_sse_once(&server, first_response).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "explain your reasoning".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let started = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ItemStarted(ItemStartedEvent {
            item: TurnItem::Reasoning(item),
            ..
        }) => Some(item.clone()),
        _ => None,
    })
    .await;
    let completed = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ItemCompleted(ItemCompletedEvent {
            item: TurnItem::Reasoning(item),
            ..
        }) => Some(item.clone()),
        _ => None,
    })
    .await;

    assert_eq!(started.id, completed.id);
    assert_eq!(
        completed.summary_text,
        vec!["Consider inputs".to_string(), "Compute output".to_string()]
    );
    assert_eq!(
        completed.raw_content,
        vec!["Detailed reasoning trace".to_string()]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_streamed_reasoning_id_is_reused_for_completion() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let TestCodex { codex, .. } = test_codex().build_with_auto_env(&server).await?;

    let mut reasoning_added = ev_reasoning_item_added("unused", &[]);
    reasoning_added["item"]
        .as_object_mut()
        .expect("reasoning item")
        .remove("id");
    let mut reasoning_done = ev_reasoning_item("unused", &["Consider inputs"], &[]);
    reasoning_done["item"]
        .as_object_mut()
        .expect("reasoning item")
        .remove("id");
    let response_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            reasoning_added,
            reasoning_done,
            ev_completed("resp-1"),
        ]),
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "explain your reasoning".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let started_id = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ItemStarted(ItemStartedEvent {
            item: TurnItem::Reasoning(item),
            ..
        }) => Some(item.id.clone()),
        _ => None,
    })
    .await;
    let completed_id = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ItemCompleted(ItemCompletedEvent {
            item: TurnItem::Reasoning(item),
            ..
        }) => Some(item.id.clone()),
        _ => None,
    })
    .await;

    assert!(started_id.starts_with("rs_"));
    assert_eq!(started_id, completed_id);
    response_mock.single_request();

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_search_item_is_emitted() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let TestCodex { codex, .. } = test_codex()
        .with_history_mode(ThreadHistoryMode::Paginated)
        .build(&server)
        .await?;

    let web_search_added = ev_web_search_call_added_partial("web-search-1", "in_progress");
    let web_search_done = ev_web_search_call_done("web-search-1", "completed", "weather seattle");

    let first_response = sse(vec![
        ev_response_created("resp-1"),
        web_search_added,
        web_search_done,
        ev_completed("resp-1"),
    ]);
    mount_sse_once(&server, first_response).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "find the weather".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let started = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ItemStarted(ItemStartedEvent {
            item: TurnItem::WebSearch(item),
            started_at_ms,
            ..
        }) => Some((item.clone(), *started_at_ms)),
        _ => None,
    })
    .await;
    let begin = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::WebSearchBegin(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    let completed = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ItemCompleted(ItemCompletedEvent {
            item: TurnItem::WebSearch(item),
            started_at_ms,
            completed_at_ms,
            ..
        }) => Some((item.clone(), *started_at_ms, *completed_at_ms)),
        _ => None,
    })
    .await;

    assert_eq!(begin.call_id, "web-search-1");
    assert_eq!(started.0.id, begin.call_id);
    assert!(started.1 > 0);
    assert_eq!(completed.0.id, begin.call_id);
    assert_eq!(completed.1, Some(started.1));
    assert!(completed.2 > 0);
    assert_eq!(
        completed.0.action,
        WebSearchAction::Search {
            query: Some("weather seattle".to_string()),
            queries: None,
        }
    );

    codex.flush_rollout().await?;
    let rollout_path = codex.rollout_path().expect("paginated rollout path");
    let rollout = std::fs::read_to_string(rollout_path)?;
    let persisted_completion = rollout
        .lines()
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find_map(|line| match line.item {
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                if matches!(&event.item, TurnItem::WebSearch(item) if item.id == begin.call_id) =>
            {
                Some(event)
            }
            _ => None,
        })
        .expect("persisted web search completion");
    assert_eq!(persisted_completion.started_at_ms, Some(started.1));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_message_content_delta_has_item_metadata() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let TestCodex {
        codex,
        session_configured,
        ..
    } = test_codex().build(&server).await?;

    let stream = sse(vec![
        ev_response_created("resp-1"),
        ev_message_item_added("msg-1", ""),
        ev_output_text_delta("streamed response"),
        ev_assistant_message("msg-1", "streamed response"),
        ev_completed("resp-1"),
    ]);
    mount_sse_once(&server, stream).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "please stream text".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let (started_turn_id, started_item) = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ItemStarted(ItemStartedEvent {
            turn_id,
            item: TurnItem::AgentMessage(item),
            ..
        }) => Some((turn_id.clone(), item.clone())),
        _ => None,
    })
    .await;

    let delta_event = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::AgentMessageContentDelta(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    let completed_item = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ItemCompleted(ItemCompletedEvent {
            item: TurnItem::AgentMessage(item),
            ..
        }) => Some(item.clone()),
        _ => None,
    })
    .await;

    let thread_id = session_configured.thread_id.to_string();
    assert_eq!(delta_event.thread_id, thread_id);
    assert_eq!(delta_event.turn_id, started_turn_id);
    assert_eq!(delta_event.item_id, started_item.id);
    assert_eq!(delta_event.delta, "streamed response");
    assert_eq!(completed_item.id, started_item.id);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_content_delta_has_item_metadata() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let TestCodex { codex, .. } = test_codex().build(&server).await?;

    let stream = sse(vec![
        ev_response_created("resp-1"),
        ev_reasoning_item_added("reasoning-1", &[""]),
        ev_reasoning_summary_text_delta("step one"),
        ev_reasoning_item("reasoning-1", &["step one"], &[]),
        ev_completed("resp-1"),
    ]);
    mount_sse_once(&server, stream).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "reason through it".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let reasoning_item = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ItemStarted(ItemStartedEvent {
            item: TurnItem::Reasoning(item),
            ..
        }) => Some(item.clone()),
        _ => None,
    })
    .await;

    let delta_event = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ReasoningContentDelta(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    assert_eq!(delta_event.item_id, reasoning_item.id);
    assert_eq!(delta_event.delta, "step one");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequential_cutoff_renders_done_summaries_for_active_reasoning_item() -> anyhow::Result<()>
{
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let TestCodex { codex, .. } = test_codex()
        .with_config(|config| {
            config
                .features
                .enable(Feature::ConcurrentReasoningSummaries)
                .expect("test config should allow feature update");
        })
        .build(&server)
        .await?;
    let summary_done = |item_id: &str, summary_index: i64, text: &str| {
        serde_json::json!({
            "type": "response.reasoning_summary_text.done",
            "item_id": item_id,
            "summary_index": summary_index,
            "text": text,
        })
    };
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_reasoning_item_added("reasoning-1", &[""]),
            // Sequential cutoff renders atomic done events, not partial legacy events.
            ev_reasoning_summary_text_delta("partial"),
            summary_done("reasoning-1", 0, "step one"),
            summary_done("reasoning-1", 1, "step two"),
            ev_reasoning_item("reasoning-1", &["step one", "step two"], &[]),
            ev_message_item_added("message-1", ""),
            // Drop a stale completion rather than attaching it to the active message.
            summary_done("reasoning-1", 2, "late step"),
            ev_output_text_delta("Done"),
            ev_assistant_message("message-1", "Done"),
            ev_completed("resp-1"),
        ]),
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "reason through it".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let reasoning_item = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ItemStarted(ItemStartedEvent {
            item: TurnItem::Reasoning(item),
            ..
        }) => Some(item.clone()),
        _ => None,
    })
    .await;

    let mut summary_deltas = Vec::new();
    let mut summary_sections = Vec::new();
    loop {
        match wait_for_event(&codex, |_| true).await {
            EventMsg::ReasoningContentDelta(event) => {
                summary_deltas.push((event.item_id, event.delta, event.summary_index));
            }
            EventMsg::AgentReasoningSectionBreak(event) => {
                summary_sections.push((event.item_id, event.summary_index));
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(
        summary_deltas,
        vec![
            (reasoning_item.id.clone(), "step one".to_string(), 0),
            (reasoning_item.id, "step two".to_string(), 1),
        ]
    );
    assert_eq!(summary_sections, vec![("reasoning-1".to_string(), 1)]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_raw_content_delta_respects_flag() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let TestCodex { codex, .. } = test_codex()
        .with_config(|config| {
            config.show_raw_agent_reasoning = true;
        })
        .build(&server)
        .await?;

    let stream = sse(vec![
        ev_response_created("resp-1"),
        ev_reasoning_item_added("reasoning-raw", &[""]),
        ev_reasoning_text_delta("raw detail"),
        ev_reasoning_item("reasoning-raw", &["complete"], &["raw detail"]),
        ev_completed("resp-1"),
    ]);
    mount_sse_once(&server, stream).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "show raw reasoning".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let reasoning_item = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ItemStarted(ItemStartedEvent {
            item: TurnItem::Reasoning(item),
            ..
        }) => Some(item.clone()),
        _ => None,
    })
    .await;

    let delta_event = wait_for_event_match(&codex, |ev| match ev {
        EventMsg::ReasoningRawContentDelta(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    assert_eq!(delta_event.item_id, reasoning_item.id);
    assert_eq!(delta_event.delta, "raw detail");

    Ok(())
}
