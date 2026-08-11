use super::*;
use crate::unified_exec::clamp_yield_time;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;
use tokio::time::Duration;
use tokio::time::Instant;

#[test]
fn unified_exec_env_injects_defaults() {
    let env = apply_unified_exec_env(HashMap::new());
    let expected = HashMap::from([
        ("NO_COLOR".to_string(), "1".to_string()),
        ("TERM".to_string(), "dumb".to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
        ("LC_CTYPE".to_string(), "C.UTF-8".to_string()),
        ("LC_ALL".to_string(), "C.UTF-8".to_string()),
        ("COLORTERM".to_string(), String::new()),
        ("PAGER".to_string(), "cat".to_string()),
        ("GIT_PAGER".to_string(), "cat".to_string()),
        ("GH_PAGER".to_string(), "cat".to_string()),
        ("CODEX_CI".to_string(), "1".to_string()),
    ]);

    assert_eq!(env, expected);
}

#[test]
fn unified_exec_env_overrides_existing_values() {
    let mut base = HashMap::new();
    base.insert("NO_COLOR".to_string(), "0".to_string());
    base.insert("PATH".to_string(), "/usr/bin".to_string());

    let env = apply_unified_exec_env(base);

    assert_eq!(env.get("NO_COLOR"), Some(&"1".to_string()));
    assert_eq!(env.get("PATH"), Some(&"/usr/bin".to_string()));
}

#[test]
fn initial_exec_yield_time_has_no_platform_floor() {
    assert_eq!(clamp_yield_time(/*yield_time_ms*/ 1_000), 1_000);
    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ 1),
        crate::unified_exec::MIN_YIELD_TIME_MS
    );
}

#[tokio::test]
async fn output_collection_stays_bounded_across_repeated_drains() {
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let output = OutputHandles {
        output_buffer: Arc::clone(&output_buffer),
        output_notify: Arc::clone(&output_notify),
        output_closed: Arc::clone(&output_closed),
        output_closed_notify: Arc::clone(&output_closed_notify),
        cancellation_token: cancellation_token.clone(),
    };

    let collect = UnifiedExecProcessManager::collect_output_until_deadline(
        &output,
        /*pause_state*/ None,
        Instant::now() + Duration::from_secs(5),
    );
    let produce = async {
        for byte in *b"abc" {
            output_buffer.lock().await.push_chunk(
                vec![byte; crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES],
            );
            output_notify.notify_one();
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if output_buffer.lock().await.retained_bytes() == 0 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("collector should drain each chunk");
        }

        output_closed.store(true, Ordering::Release);
        cancellation_token.cancel();
        output_closed_notify.notify_waiters();
        output_notify.notify_waiters();
    };

    let (collected, ()) = tokio::join!(collect, produce);
    let mut expected = HeadTailBuffer::default();
    for byte in *b"abc" {
        expected.push_chunk(vec![
            byte;
            crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES
        ]);
    }
    assert_eq!(collected, expected);
}

#[tokio::test]
async fn output_collection_preserves_omissions_from_drained_buffer() {
    let mut buffered_output = HeadTailBuffer::default();
    buffered_output.push_chunk(vec![
        b'a';
        crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES
    ]);
    buffered_output.push_chunk(b"overflow".to_vec());
    let mut expected = HeadTailBuffer::default();
    expected.push_chunk(vec![
        b'a';
        crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES
    ]);
    expected.push_chunk(b"overflow".to_vec());
    let output_buffer = Arc::new(tokio::sync::Mutex::new(buffered_output));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(true));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    cancellation_token.cancel();
    let output = OutputHandles {
        output_buffer,
        output_notify,
        output_closed,
        output_closed_notify,
        cancellation_token,
    };

    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output,
        /*pause_state*/ None,
        Instant::now() + Duration::from_secs(1),
    )
    .await;

    assert_eq!(collected, expected);
}

#[test]
fn pruning_prefers_exited_processes_outside_recently_used() {
    let now = Instant::now();
    let meta = vec![
        (1, now - Duration::from_secs(40), false),
        (2, now - Duration::from_secs(30), true),
        (3, now - Duration::from_secs(20), false),
        (4, now - Duration::from_secs(19), false),
        (5, now - Duration::from_secs(18), false),
        (6, now - Duration::from_secs(17), false),
        (7, now - Duration::from_secs(16), false),
        (8, now - Duration::from_secs(15), false),
        (9, now - Duration::from_secs(14), false),
        (10, now - Duration::from_secs(13), false),
    ];

    let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

    assert_eq!(candidate, Some(2));
}

#[test]
fn pruning_falls_back_to_lru_when_no_exited() {
    let now = Instant::now();
    let meta = vec![
        (1, now - Duration::from_secs(40), false),
        (2, now - Duration::from_secs(30), false),
        (3, now - Duration::from_secs(20), false),
        (4, now - Duration::from_secs(19), false),
        (5, now - Duration::from_secs(18), false),
        (6, now - Duration::from_secs(17), false),
        (7, now - Duration::from_secs(16), false),
        (8, now - Duration::from_secs(15), false),
        (9, now - Duration::from_secs(14), false),
        (10, now - Duration::from_secs(13), false),
    ];

    let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

    assert_eq!(candidate, Some(1));
}

#[test]
fn pruning_protects_recent_processes_even_if_exited() {
    let now = Instant::now();
    let meta = vec![
        (1, now - Duration::from_secs(40), false),
        (2, now - Duration::from_secs(30), false),
        (3, now - Duration::from_secs(20), true),
        (4, now - Duration::from_secs(19), false),
        (5, now - Duration::from_secs(18), false),
        (6, now - Duration::from_secs(17), false),
        (7, now - Duration::from_secs(16), false),
        (8, now - Duration::from_secs(15), false),
        (9, now - Duration::from_secs(14), false),
        (10, now - Duration::from_secs(13), true),
    ];

    let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

    // (10) is exited but among the last 8; we should drop the LRU outside that set.
    assert_eq!(candidate, Some(1));
}

#[cfg(unix)]
#[tokio::test]
async fn pruning_does_not_evict_live_process_while_exited_process_is_finalizing() {
    let exited_process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
        )
        .await,
    );
    exited_process
        .terminate_confirmed()
        .await
        .expect("exited process should terminate");
    let live_process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
        )
        .await,
    );
    let _interaction_guard = exited_process.interaction_lock().lock_owned().await;
    let now = Instant::now();
    let cwd = PathUri::parse("file:///tmp").expect("test cwd should be valid");
    let mut store = ProcessStore::default();
    let max_process_id =
        i32::try_from(MAX_UNIFIED_EXEC_PROCESSES).expect("process cap should fit in i32");

    for process_id in 1..=max_process_id {
        let is_exited = process_id == 1;
        store.processes.insert(
            process_id,
            ProcessEntry {
                process: if is_exited {
                    Arc::clone(&exited_process)
                } else {
                    Arc::clone(&live_process)
                },
                call_id: format!("call-{process_id}"),
                process_id,
                cwd: cwd.clone(),
                initial_exec_command_active: Arc::new(AtomicBool::new(false)),
                hook_command: format!("command-{process_id}"),
                tty: false,
                session: std::sync::Weak::new(),
                last_used: if is_exited {
                    now - Duration::from_secs(1)
                } else {
                    now
                },
            },
        );
    }

    let pruned = UnifiedExecProcessManager::prune_processes_if_needed(&mut store);

    assert_eq!(
        (pruned.map(|entry| entry.process_id), store.processes.len()),
        (None, MAX_UNIFIED_EXEC_PROCESSES)
    );
}
