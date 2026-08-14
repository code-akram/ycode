use super::*;
use async_channel::bounded;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RawResponseItemEvent;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::request_user_input::RequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputEvent;
use codex_protocol::request_user_input::RequestUserInputQuestion;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::time::timeout;

#[tokio::test]
async fn forward_events_filters_private_events_before_blocked_send_is_cancelled() {
    let (tx_events, rx_events) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (tx_sub, rx_sub) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (_agent_status_tx, agent_status) = watch::channel(AgentStatus::PendingInit);
    let (session, ctx, _rx_evt) = crate::session::tests::make_session_and_context_with_rx().await;
    let io = Arc::new(SessionIo {
        tx_sub,
        rx_event: rx_events,
        agent_status,
        session_loop_termination: completed_session_loop_termination(),
        session_loop_abort: None,
    });

    let (tx_out, rx_out) = bounded(1);
    tx_out
        .send(Event {
            id: "full".to_string(),
            msg: EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some("turn-1".to_string()),
                started_at: None,
                reason: TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
            }),
        })
        .await
        .unwrap();

    let cancel = CancellationToken::new();
    let forward = tokio::spawn(forward_events(
        Arc::clone(&io),
        tx_out.clone(),
        session,
        ctx,
        cancel.clone(),
    ));

    let visible_msg = EventMsg::RawResponseItem(RawResponseItemEvent {
        item: ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "call-1".to_string(),
            name: "tool".to_string(),
            namespace: None,
            input: "{}".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    });
    for id in ["visible-1", "visible-2", "blocked"] {
        tx_events
            .send(Event {
                id: id.to_string(),
                msg: visible_msg.clone(),
            })
            .await
            .unwrap();
    }

    drop(tx_events);
    let received = rx_out.recv().await.expect("prefilled event missing");
    assert_eq!(received.id, "full");
    let received = rx_out.recv().await.expect("visible event missing");
    assert_eq!(received.id, "visible-1");
    cancel.cancel();
    timeout(std::time::Duration::from_millis(1000), forward)
        .await
        .expect("forward_events hung")
        .expect("forward_events join error");

    let mut ops = Vec::new();
    while let Ok(sub) = rx_sub.try_recv() {
        ops.push(sub.op);
    }
    assert!(
        ops.iter().any(|op| matches!(op, Op::Interrupt)),
        "expected Interrupt op after cancellation"
    );
    assert!(
        ops.iter().any(|op| matches!(op, Op::Shutdown)),
        "expected Shutdown op after cancellation"
    );
}

#[tokio::test]
async fn forward_ops_preserves_submission_trace_context() {
    let (tx_sub, rx_sub) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (_tx_events, rx_events) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (_agent_status_tx, agent_status) = watch::channel(AgentStatus::PendingInit);
    let io = Arc::new(SessionIo {
        tx_sub,
        rx_event: rx_events,
        agent_status,
        session_loop_termination: completed_session_loop_termination(),
        session_loop_abort: None,
    });
    let (tx_ops, rx_ops) = bounded(1);
    let cancel = CancellationToken::new();
    let forward = tokio::spawn(forward_ops(Arc::clone(&io), rx_ops, cancel));

    let submission = Submission {
        id: "sub-1".to_string(),
        op: Op::Interrupt,
        client_user_message_id: None,
        trace: Some(codex_protocol::protocol::W3cTraceContext {
            traceparent: Some(
                "00-1234567890abcdef1234567890abcdef-1234567890abcdef-01".to_string(),
            ),
            tracestate: Some("vendor=state".to_string()),
        }),
        parent_turn_id: Some("parent-turn".to_string()),
    };
    tx_ops.send(submission.clone()).await.unwrap();
    drop(tx_ops);

    let forwarded = timeout(Duration::from_secs(1), rx_sub.recv())
        .await
        .expect("forward_ops hung")
        .expect("forwarded submission missing");
    assert_eq!(submission.id, forwarded.id);
    assert_eq!(submission.op, forwarded.op);
    assert_eq!(submission.trace, forwarded.trace);
    assert_eq!(submission.parent_turn_id, forwarded.parent_turn_id);

    timeout(Duration::from_secs(1), forward)
        .await
        .expect("forward_ops did not exit")
        .expect("forward_ops join error");
}

#[tokio::test]
async fn run_codex_thread_interactive_respects_pre_cancelled_spawn() {
    let (parent_session, parent_ctx, _rx_events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();

    let result = timeout(
        Duration::from_secs(/*secs*/ 1),
        run_codex_thread_interactive(
            parent_ctx.config.as_ref().clone(),
            Arc::clone(&parent_session.services.auth_manager),
            Arc::clone(&parent_session.services.models_manager),
            parent_session,
            parent_ctx,
            cancel_token,
            SubAgentSource::Review,
            /*initial_history*/ None,
            crate::session::GitEnrichmentPolicy::Fresh,
        ),
    )
    .await
    .expect("cancelled delegate spawn should not hang");

    assert!(matches!(
        result,
        Err(err) if matches!(err.details(), CodexErrorDetails::TurnAborted)
    ));
}

#[tokio::test]
async fn handle_request_user_input_preserves_non_blocking_flag_for_round_trip() {
    let (parent_session, parent_ctx, rx_events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    *parent_session.active_turn.lock().await = Some(crate::state::ActiveTurn::default());

    let (tx_sub, rx_sub) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (_tx_events, rx_events_child) = bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (_agent_status_tx, agent_status) = watch::channel(AgentStatus::PendingInit);
    let io = Arc::new(SessionIo {
        tx_sub,
        rx_event: rx_events_child,
        agent_status,
        session_loop_termination: completed_session_loop_termination(),
        session_loop_abort: None,
    });
    let cancel_token = CancellationToken::new();
    let child_event_id = "child-request-1".to_string();
    let parent_user_input_id = parent_ctx.sub_id.clone();
    let expected_response = RequestUserInputResponse {
        answers: HashMap::from([(
            "q1".to_string(),
            RequestUserInputAnswer {
                answers: vec!["yes".to_string()],
            },
        )]),
    };

    let handle = tokio::spawn({
        let io = Arc::clone(&io);
        let parent_session = Arc::clone(&parent_session);
        let parent_ctx = Arc::clone(&parent_ctx);
        let cancel_token = cancel_token.clone();
        let child_event_id = child_event_id.clone();
        async move {
            handle_request_user_input(
                io.as_ref(),
                child_event_id,
                &parent_session,
                &parent_ctx,
                RequestUserInputEvent {
                    call_id: "child-call-1".to_string(),
                    turn_id: "child-turn-1".to_string(),
                    questions: vec![RequestUserInputQuestion {
                        id: "q1".to_string(),
                        header: "Confirm".to_string(),
                        question: "Continue?".to_string(),
                        is_other: false,
                        is_secret: false,
                        options: None,
                    }],
                    is_blocking: false,
                    auto_resolution_ms: None,
                },
                &cancel_token,
            )
            .await;
        }
    });

    let request_event = timeout(Duration::from_secs(1), rx_events.recv())
        .await
        .expect("request_user_input event timed out")
        .expect("request_user_input event missing");
    let EventMsg::RequestUserInput(request) = request_event.msg else {
        panic!("expected RequestUserInput event");
    };
    assert_eq!(request.call_id, parent_user_input_id);
    assert_eq!(request.is_blocking, false);

    parent_session
        .notify_user_input_response(&parent_user_input_id, expected_response.clone())
        .await;

    timeout(Duration::from_secs(1), handle)
        .await
        .expect("handle_request_user_input hung")
        .expect("handle_request_user_input join error");

    let submission = timeout(Duration::from_secs(1), rx_sub.recv())
        .await
        .expect("request_user_input response timed out")
        .expect("request_user_input response missing");
    assert_eq!(
        submission.op,
        Op::UserInputAnswer {
            id: child_event_id,
            response: expected_response,
        }
    );
}
