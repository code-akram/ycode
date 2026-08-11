use super::*;
use codex_protocol::protocol::FileChange;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::path::PathBuf;
#[test]
fn diff_consumer_streams_apply_patch_changes() {
    let mut consumer = ApplyPatchArgumentDiffConsumer::default();
    assert!(
        consumer
            .push_delta("call-1".to_string(), "*** Begin Patch\n")
            .is_none()
    );

    let event = consumer
        .push_delta("call-1".to_string(), "*** Add File: hello.txt\n+hello")
        .expect("progress event");
    assert_eq!(
        (event.call_id, event.changes),
        (
            "call-1".to_string(),
            HashMap::from([(
                PathBuf::from("hello.txt"),
                FileChange::Add {
                    content: String::new(),
                },
            )]),
        )
    );

    assert!(
        consumer
            .push_delta("call-1".to_string(), "\n+world")
            .is_none()
    );
    assert!(
        consumer
            .push_delta("call-1".to_string(), "\n*** End Patch")
            .is_none()
    );

    let event = consumer
        .finish_update_on_complete()
        .expect("finish parser")
        .expect("progress event");
    assert_eq!(
        (event.call_id, event.changes),
        (
            "call-1".to_string(),
            HashMap::from([(
                PathBuf::from("hello.txt"),
                FileChange::Add {
                    content: "hello\nworld\n".to_string(),
                },
            )]),
        )
    );
}

#[test]
fn diff_consumer_streams_apply_patch_changes_with_environment_header() {
    let mut consumer = ApplyPatchArgumentDiffConsumer::default();
    assert!(
        consumer
            .push_delta(
                "call-1".to_string(),
                "*** Begin Patch\n*** Environment ID: remote\n",
            )
            .is_none()
    );

    let event = consumer
        .push_delta("call-1".to_string(), "*** Add File: hello.txt\n+hello")
        .expect("progress event");
    assert_eq!(
        event.changes,
        HashMap::from([(
            PathBuf::from("hello.txt"),
            FileChange::Add {
                content: String::new(),
            },
        )])
    );
}

#[test]
fn diff_consumer_sends_next_update_after_buffer_interval() {
    let mut consumer = ApplyPatchArgumentDiffConsumer::default();
    consumer.push_delta("call-1".to_string(), "*** Begin Patch\n");
    let first = consumer
        .push_delta("call-1".to_string(), "*** Add File: hello.txt\n+hello")
        .expect("first progress event");
    assert_eq!(
        first.changes,
        HashMap::from([(
            PathBuf::from("hello.txt"),
            FileChange::Add {
                content: String::new(),
            },
        )])
    );

    consumer.last_sent_at =
        Some(std::time::Instant::now() - APPLY_PATCH_ARGUMENT_DIFF_BUFFER_INTERVAL);
    let second = consumer
        .push_delta("call-1".to_string(), "\n+world")
        .expect("second progress event");
    assert_eq!(
        second.changes,
        HashMap::from([(
            PathBuf::from("hello.txt"),
            FileChange::Add {
                content: "hello\n".to_string(),
            },
        )])
    );
}

#[test]
fn reconcile_environment_id_requires_selection_when_enabled() {
    assert_eq!(
        require_environment_id(Some("remote"), /*allow_environment_id*/ false),
        Err(FunctionCallError::RespondToModel(
            "apply_patch environment selection is unavailable for this turn".to_string(),
        ))
    );
    assert_eq!(
        require_environment_id(
            /*parsed_environment_id*/ None, /*allow_environment_id*/ true
        ),
        Ok(None)
    );
}
